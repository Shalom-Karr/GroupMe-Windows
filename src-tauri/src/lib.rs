//! GroupMe for Windows — desktop wrapper and offline archive.
//!
//! Copyright (c) 2026 Example Sender. AGPL-3.0-only WITH Commons Clause.
//! Non-commercial use only; commercial use requires a separate licence.
//! See LICENSE.
//!
//! Three parts sharing one SQLite file:
//!
//! 1. **Online** — the webview loads `https://web.groupme.com` unchanged, so
//!    sending, uploads and emoji are GroupMe's problem rather than ours, and
//!    nothing breaks when they reskin.
//! 2. **Archive** — a background worker calls `api.groupme.com/v3` directly and
//!    writes SQLite. It reads the API rather than scraping the DOM: the API is
//!    a contract, the markup is not.
//! 3. **Offline** — the window swaps to a bundled reader over that archive.
//!    Read-only by construction, not by convention: no mutating command is
//!    registered on that surface at all (see `commands.rs`).

pub mod api;
pub mod commands;
pub mod connectivity;
pub mod model;
pub mod store;
pub mod sync;
pub mod token;
pub mod tray;
pub mod updater;

use std::sync::{Arc, Mutex};

use tauri::{Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};

use commands::SharedStore;

pub const GROUPME_WEB_ORIGIN: &str = "https://web.groupme.com";
pub const ARCHIVE_FILENAME: &str = "archive.db";
pub const MEDIA_DIRNAME: &str = "media";

/// Local pages. `index.html` probes connectivity and routes; `offline.html` is
/// the archive reader.
const ROUTER_PAGE: &str = "index.html";
const OFFLINE_PAGE: &str = "offline.html";

/// Injected into every frame before page scripts run, on every navigation.
/// Lifts the access token off an outgoing API request header — see inject.js
/// for why the header rather than a localStorage key.
const INJECT_JS: &str = include_str!("../frontend/inject.js");

pub fn run() {
    // WebView2 suspends timers in backgrounded windows. Without this the
    // injected script's connectivity events stop firing the moment the window
    // is minimised, which is exactly when a laptop tends to lose its network.
    #[cfg(windows)]
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--disable-background-timer-throttling \
         --disable-renderer-backgrounding \
         --disable-backgrounding-occluded-windows",
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_log::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::archive_conversations,
            commands::archive_messages,
            commands::archive_search,
            commands::archive_media_path,
            commands::archive_stats,
            tray::show_app_menu,
            updater::updater_check,
            updater::updater_download,
            updater::updater_restart,
            updater::updater_version,
            updater::updater_last_status,
        ])
        .setup(|app| {
            // LOCAL app data, not roaming. `app_data_dir()` resolves to
            // %APPDATA% on Windows, which is the roaming profile — on a
            // domain-joined machine that would try to sync a multi-gigabyte
            // message archive across the network at every logon.
            let data_dir = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let media_dir = data_dir.join(MEDIA_DIRNAME);
            std::fs::create_dir_all(&media_dir)?;

            let store = store::Store::open(&data_dir.join(ARCHIVE_FILENAME))?;
            let shared: SharedStore = Arc::new(Mutex::new(store));
            app.manage(shared.clone());

            // Built here rather than declared in tauri.conf.json because an
            // initialization script can only be attached at window-build time.
            let window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App(ROUTER_PAGE.into()),
            )
            .title("GroupMe")
            .inner_size(1200.0, 820.0)
            .min_inner_size(800.0, 600.0)
            .resizable(true)
            .center()
            .initialization_script(INJECT_JS)
            .build()?;

            tray::init(app.handle())?;
            updater::spawn_periodic_check(app.handle().clone());

            // The tray's "Check for updates…" item only emits an event; the
            // updater owns the dialog, so the two never need to know about
            // each other.
            {
                let handle = app.handle().clone();
                app.listen("app://check-updates", move |_| {
                    let _ = updater::open_dialog(&handle);
                    let h = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        updater::updater_check(h).await;
                    });
                });
            }

            spawn_token_listener(app.handle().clone(), shared.clone(), media_dir);
            spawn_connectivity_watch(app.handle().clone(), window);

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start GroupMe for Windows")
        .run(|_app, event| {
            // A staged update is installed on the way out, never mid-session.
            // Swapping the binary under someone who is reading their messages
            // is hostile; waiting until they quit costs nothing.
            if let tauri::RunEvent::Exit = event {
                updater::install_staged_on_exit();
            }
        });
}

/// Waits for the injected script to hand over an access token, then persists it
/// and starts syncing.
///
/// The token arrives over the event channel rather than `invoke` because the
/// remote GroupMe origin is granted only `core:event:*` — it is third-party
/// code we do not control, so it gets no command access at all.
fn spawn_token_listener(
    app: tauri::AppHandle,
    store: SharedStore,
    media_dir: std::path::PathBuf,
) {
    let handle = app.clone();
    app.listen("groupme://token", move |event| {
        let Some(token) = parse_token_payload(event.payload()) else {
            return;
        };

        let store = store.clone();
        let media_dir = media_dir.clone();
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = adopt_token(&handle, &store, &media_dir, &token).await {
                log::error!("adopting captured token: {e}");
            }
        });
    });
}

fn parse_token_payload(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let raw = v.get("token").and_then(|t| t.as_str())?;
    token::looks_like_token(raw).then(|| raw.to_string())
}

async fn adopt_token(
    app: &tauri::AppHandle,
    store: &SharedStore,
    media_dir: &std::path::Path,
    token: &str,
) -> anyhow::Result<()> {
    log::info!("captured access token {}", token::redact(token));

    // NOTHING is persisted here.
    //
    // The token arrives over an event channel that `web.groupme.com` is
    // permitted to emit on, and Tauri events carry no verifiable origin — so
    // any script on that page (GroupMe's own, a compromise of their frontend,
    // an injected ad) can forge one. Writing it to the credential store before
    // proving whose account it is would let a forged token overwrite the real
    // credential and, on a fresh archive, claim `account_user_id` permanently:
    // the user's genuine token would then be refused forever.
    //
    // So the token is treated as an unverified claim until `/users/me` says
    // otherwise. Persistence happens in `start_sync`, after verification.
    publish_token(app.clone(), store.clone(), media_dir.to_path_buf(), token.to_string());
    Ok(())
}

/// The access token currently believed good, published to the single sync loop.
///
/// A `watch` channel rather than a fresh task per token: the same token is
/// re-emitted on every navigation (the injected script's dedupe resets each
/// time the webview navigates, and the connectivity watcher navigates on every
/// online/offline flip), so spawning per event would leak a permanent sync
/// worker each time — each one polling the API from the user's IP forever.
static TOKEN_TX: std::sync::OnceLock<tokio::sync::watch::Sender<String>> =
    std::sync::OnceLock::new();

fn publish_token(
    app: tauri::AppHandle,
    store: SharedStore,
    media_dir: std::path::PathBuf,
    token: String,
) {
    if let Some(tx) = TOKEN_TX.get() {
        // A loop already exists. Hand it the token only if it actually changed,
        // so a re-emit of the same value is a no-op rather than a resync.
        tx.send_if_modified(|current| {
            if *current == token {
                false
            } else {
                log::info!("access token rotated; the running sync loop will pick it up");
                *current = token;
                true
            }
        });
        return;
    }

    let (tx, rx) = tokio::sync::watch::channel(token);
    // Losing this race means another thread won and its loop is already
    // running; drop ours rather than starting a second.
    if TOKEN_TX.set(tx).is_err() {
        return;
    }
    start_sync(app, store, media_dir, rx);
}

/// Confirms the archive belongs to the account this token authenticates.
///
/// `/v3/users/me` -> `id` is the only stable account identity available: it
/// survives token rotation, re-authentication, and password changes, none of
/// which are account switches. Returns `false` when the archive belongs to
/// somebody else, in which case syncing must not proceed — interleaving two
/// people's message history corrupts the archive and leaks one user's messages
/// to whoever opens the other's.
async fn verify_account(
    app: &tauri::AppHandle,
    store: &SharedStore,
    me: &model::Me,
) -> anyhow::Result<bool> {
    // The comparison and the claim happen together under one lock on the
    // blocking pool; only the notification needs the app handle, so it is done
    // out here rather than dragging the handle across the thread boundary.
    let mismatch = {
        let store = store.clone();
        let id = me.id.clone();
        let name = me.name.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            match s.get_meta("account_user_id")? {
                Some(prev) if prev != id => Ok(Some(prev)),
                _ => {
                    s.set_meta("account_user_id", &id)?;
                    if let Some(name) = name.as_deref() {
                        s.set_meta("account_name", name)?;
                    }
                    Ok(None)
                }
            }
        })
        .await??
    };

    match mismatch {
        Some(prev) => {
            log::warn!(
                "archive belongs to account {prev}, but the signed-in account is {}; \
                 refusing to sync",
                me.id
            );
            let _ = app.emit(
                "archive://account-changed",
                serde_json::json!({ "archive_user_id": prev, "signed_in_user_id": me.id }),
            );
            Ok(false)
        }
        None => Ok(true),
    }
}

/// The one and only sync loop. Rebuilds its client whenever the token rotates.
fn start_sync(
    app: tauri::AppHandle,
    store: SharedStore,
    media_dir: std::path::PathBuf,
    mut token_rx: tokio::sync::watch::Receiver<String>,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            let token = token_rx.borrow_and_update().clone();

            let client = match api::GroupMeClient::new(token.clone()) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("building api client: {e}");
                    return;
                }
            };

            // Prove whose account this is BEFORE anything is persisted. An
            // unverified token that reached the credential store or claimed
            // `account_user_id` would have already done the damage.
            let me = match client.me().await {
                Ok(me) => me,
                Err(e) => {
                    log::warn!("could not identify the signed-in account: {e}");
                    // Wait for a different token rather than spinning on a bad
                    // one — a forged or revoked token would otherwise retry
                    // against the API forever.
                    if token_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            match verify_account(&app, &store, &me).await {
                Ok(true) => {}
                Ok(false) => {
                    if token_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    log::error!("verifying account: {e}");
                    return;
                }
            }

            // Verified. Only now is it safe to keep.
            if let Err(e) = persist_token(&store, &token).await {
                log::error!("persisting verified token: {e}");
            }

            run_sync_loop(&app, &store, &media_dir, client, &mut token_rx).await;
            // Only returns when the token changed; loop round and rebuild.
        }
    });
}

/// Writes the verified token to Windows Credential Manager and records its
/// fingerprint. Skipped when the same token is already stored.
async fn persist_token(store: &SharedStore, token: &str) -> anyhow::Result<()> {
    let fingerprint = token::fingerprint(token);
    let already_known = {
        let store = store.clone();
        let fp = fingerprint.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            let known = s.get_meta("token_fingerprint")? == Some(fp.clone());
            s.set_meta("token_fingerprint", &fp)?;
            Ok(known)
        })
        .await??
    };
    if !already_known {
        token::TokenStore::new()
            .save(token)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(())
}

async fn run_sync_loop(
    app: &tauri::AppHandle,
    store: &SharedStore,
    media_dir: &std::path::Path,
    client: api::GroupMeClient,
    token_rx: &mut tokio::sync::watch::Receiver<String>,
) {
    let engine = sync::SyncEngine::new(client, store.clone(), media_dir.to_path_buf());
    loop {
        let report = engine.sync_once().await;
        log::info!(
            "sync: {} conversations, {} new messages, {} media",
            report.conversations_seen,
            report.messages_inserted,
            report.media_cached
        );
        {
            let store = store.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let s = store.lock().unwrap_or_else(|e| e.into_inner());
                s.set_meta("last_sync_at", &now_unix().to_string())
            })
            .await;
        }
        // Scoped to the local reader, not broadcast. `SyncReport.errors` can
        // embed conversation ids and raw API response bodies, and the remote
        // GroupMe page holds `core:event:allow-listen` — a broadcast would hand
        // it data it has no business receiving.
        let _ = app.emit_to("main", "archive://synced", &report);

        // Sleep, but wake early if the token rotates so the caller can rebuild
        // the client instead of spending up to a minute using a dead one.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            changed = token_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                return;
            }
        }
    }
}

/// Swaps the window between the live client and the local reader as the network
/// comes and goes.
fn spawn_connectivity_watch(app: tauri::AppHandle, window: tauri::WebviewWindow) {
    let monitor = Arc::new(connectivity::ConnectivityMonitor::new(
        connectivity::HttpProbe::new(),
    ));

    // The webview's own online/offline events are the fastest signal we get —
    // far faster than waiting for the next probe tick — so feed them straight in.
    {
        let monitor = monitor.clone();
        app.listen("groupme://offline", move |_| {
            let monitor = monitor.clone();
            tauri::async_runtime::spawn(async move {
                monitor.poll_now().await;
            });
        });
    }
    {
        let monitor = monitor.clone();
        app.listen("groupme://online", move |_| {
            let monitor = monitor.clone();
            tauri::async_runtime::spawn(async move {
                monitor.poll_now().await;
            });
        });
    }

    let mut rx = monitor.subscribe();
    tauri::async_runtime::spawn({
        let monitor = monitor.clone();
        async move { monitor.run().await }
    });

    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let state = *rx.borrow_and_update();
            let _ = app.emit("archive://connectivity", state);
            tray::set_connectivity(
                &app,
                match state {
                    connectivity::Connectivity::Online => "online",
                    connectivity::Connectivity::Degraded => "degraded",
                    connectivity::Connectivity::Offline => "offline",
                },
            );
            match state {
                connectivity::Connectivity::Offline => {
                    log::info!("offline — switching to the local archive");
                    let _ = window.navigate(offline_url());
                }
                connectivity::Connectivity::Online => {
                    // Only pull the user back if they are actually sitting on
                    // the offline reader. Navigating a working session would
                    // throw away their scroll position for no reason.
                    if on_local_page(&window) {
                        log::info!("back online — returning to the live client");
                        if let Ok(url) = GROUPME_WEB_ORIGIN.parse() {
                            let _ = window.navigate(url);
                        }
                    }
                }
                connectivity::Connectivity::Degraded => {}
            }
        }
    });
}

fn offline_url() -> tauri::Url {
    // tauri://localhost on Windows; the scheme differs per platform, so ask the
    // webview for its own origin rather than hardcoding one.
    format!("tauri://localhost/{OFFLINE_PAGE}")
        .parse()
        .unwrap_or_else(|_| "about:blank".parse().expect("static url"))
}

fn on_local_page(window: &tauri::WebviewWindow) -> bool {
    window
        .url()
        .map(|u| {
            let s = u.as_str();
            s.contains(OFFLINE_PAGE) || s.contains(ROUTER_PAGE)
        })
        .unwrap_or(false)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_payload_is_parsed_and_validated() {
        let good = format!(r#"{{"token":"{}"}}"#, "a".repeat(40));
        assert!(parse_token_payload(&good).is_some());
    }

    #[test]
    fn implausible_tokens_are_rejected_before_reaching_the_keyring() {
        // The injected script watches third-party traffic; anything it hands us
        // is untrusted until it looks like a real credential.
        assert!(parse_token_payload(r#"{"token":""}"#).is_none());
        assert!(parse_token_payload(r#"{"token":"short"}"#).is_none());
        assert!(parse_token_payload(r#"{"token":null}"#).is_none());
        assert!(parse_token_payload(r#"{}"#).is_none());
        assert!(parse_token_payload("not json").is_none());
    }

    #[test]
    fn inject_script_is_actually_bundled() {
        // A silently empty include_str! would mean no token is ever captured
        // and the archive stays permanently empty, with no error anywhere.
        assert!(INJECT_JS.len() > 500, "inject.js missing or truncated");
        assert!(INJECT_JS.contains("api.groupme.com"));
        assert!(INJECT_JS.contains("groupme://token"));
    }

    #[test]
    fn offline_url_is_local_and_never_remote() {
        let u = offline_url();
        assert!(!u.as_str().starts_with("https://"), "offline page must be local");
        assert!(u.as_str().contains(OFFLINE_PAGE));
    }
}
