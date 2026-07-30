//! Fetches API responses through a real `web.groupme.com` browser context, for
//! networks whose filter allowlists the web app but blocks the bare API host.
//!
//! This is the fallback path. The sync worker and command surface call
//! `api.groupme.com` directly with `reqwest` as normal; only when a response is
//! detected as intercepted (`api::intercepting_host`) does the client hand the
//! URL here. So a user who is not behind such a filter never triggers it, and
//! the hidden webview it needs is created lazily on the first interception —
//! never for the unfiltered majority. See [[techloq-proxy-fallback]].
//!
//! The webview shares the app's WebView2 profile, so it is already signed in;
//! the request carries the same origin, cookies and TLS fingerprint the browser
//! session does. It is the user's own account fetching the same data the web app
//! already shows them, over the path the filter already permits — not a spoofed
//! fingerprint.
//!
//! Bridge: `netproxy://fetch {id,url,token}` out, `netproxy://result {id,…}`
//! back, correlated by `id`. See `frontend/netproxy.js`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Listener, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::oneshot;

const PROXY_WINDOW: &str = "netproxy";
const PROXY_ORIGIN: &str = "https://web.groupme.com";
const PROXY_JS: &str = include_str!("../frontend/netproxy.js");

/// How long to wait for the hidden page to load and attach its listener. The SPA
/// can be slow on a cold cache; generous because it happens at most once.
const READY_TIMEOUT: Duration = Duration::from_secs(45);
/// Per-request ceiling, so a lost `netproxy://result` cannot hang a sync cycle.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ApiProxy {
    inner: Arc<Inner>,
}

struct Inner {
    app: AppHandle,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<ProxyResponse, String>>>>,
    next_id: AtomicU64,
    /// Serialises window creation and the readiness wait; `true` once the page
    /// has reported ready, so later callers skip straight to fetching.
    lifecycle: tokio::sync::Mutex<Lifecycle>,
}

#[derive(Default)]
struct Lifecycle {
    window_built: bool,
    ready: bool,
}

pub struct ProxyResponse {
    pub status: u16,
    pub body: String,
}

/// Installs the single result listener and returns a handle. The webview is not
/// created here — that waits for the first [`ApiProxy::fetch`].
pub fn new(app: AppHandle) -> ApiProxy {
    let inner = Arc::new(Inner {
        app,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        lifecycle: tokio::sync::Mutex::new(Lifecycle::default()),
    });

    // Self-reported page state, so a fetch failure inside the hidden window is
    // diagnosable from the Rust log without a devtools session on it.
    inner.app.listen("netproxy://diag", |event| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) {
            if let Some(m) = v.get("msg").and_then(|m| m.as_str()) {
                log::info!("netproxy page: {m}");
            }
        }
    });

    let listener_inner = Arc::clone(&inner);
    inner.app.listen("netproxy://result", move |event| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) else {
            return;
        };
        let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
            return;
        };
        let reply = listener_inner.pending.lock().unwrap().remove(&id);
        let Some(tx) = reply else { return };
        let result = if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
            Ok(ProxyResponse {
                status: v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16,
                body: v
                    .get("body")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        } else {
            Err(v
                .get("error")
                .and_then(|s| s.as_str())
                .unwrap_or("proxy fetch failed")
                .to_string())
        };
        let _ = tx.send(result);
    });

    ApiProxy { inner }
}

impl ApiProxy {
    /// Fetches `url` through the hidden browser context, creating it on first
    /// use. `token` is passed to the page rather than read there so the request
    /// uses the same verified credential the direct path does.
    pub async fn fetch(&self, url: &str, token: &str) -> Result<ProxyResponse, String> {
        self.ensure_ready().await?;

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);

        if let Err(e) = self.inner.app.emit_to(
            PROXY_WINDOW,
            "netproxy://fetch",
            json!({ "id": id, "url": url, "token": token }),
        ) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(format!("emitting to the proxy window: {e}"));
        }

        match tokio::time::timeout(FETCH_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("proxy reply channel dropped".into()),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err("proxy fetch timed out".into())
            }
        }
    }

    async fn ensure_ready(&self) -> Result<(), String> {
        let mut life = self.inner.lifecycle.lock().await;
        if life.ready {
            return Ok(());
        }

        // Register the readiness listener BEFORE building the window, so a page
        // that loads quickly cannot emit `ready` before we are listening.
        let (ready_tx, ready_rx) = oneshot::channel();
        let ready_tx = Mutex::new(Some(ready_tx));
        let handler = self.inner.app.listen("netproxy://ready", move |_| {
            if let Some(tx) = ready_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        });

        if !life.window_built {
            let url = PROXY_ORIGIN
                .parse()
                .map_err(|_| "invalid proxy origin".to_string())?;
            WebviewWindowBuilder::new(&self.inner.app, PROXY_WINDOW, WebviewUrl::External(url))
                .title("GroupMe sync")
                .visible(false)
                .initialization_script(PROXY_JS)
                .build()
                .map_err(|e| format!("creating the proxy window: {e}"))?;
            life.window_built = true;
        }

        let outcome = tokio::time::timeout(READY_TIMEOUT, ready_rx).await;
        self.inner.app.unlisten(handler);
        match outcome {
            Ok(Ok(())) => {
                life.ready = true;
                log::info!(
                    "api proxy ready — routing intercepted requests through the web session"
                );
                Ok(())
            }
            _ => Err("the proxy page did not become ready in time".into()),
        }
    }
}
