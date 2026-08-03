//! GroupMe realtime over Faye (Bayeux) on `wss://push.groupme.com/faye`.
//!
//! Protocol reference: `docs/groupme-api.md` §9. Channel naming, the
//! `ext.access_token` auth object and the batched-array framing come from
//! there; anything this file had to assume is marked `UNVERIFIED`.
//!
//! # Events emitted to the `main` window
//!
//! Always `emit_to("main", …)`, never a broadcast `emit`: the online webview
//! holds `core:event:allow-listen`, so a broadcast would hand archive contents
//! to `web.groupme.com`.
//!
//! ```jsonc
//! // "realtime://message"
//! {
//!   "conversation_id": "10000001",   // group id, or "<lo>+<hi>" for a DM
//!   "kind": "group" | "dm",
//!   "frame_type": "line.create",     // GroupMe's data.type, verbatim
//!   "system_event": "message.deleted" | null,  // message.event.type, if any
//!   "stored": true,                  // false means the archive write failed
//!   "message": { /* model::Message, exactly as archive_messages returns */ }
//! }
//!
//! // "realtime://reaction"
//! {
//!   "conversation_id": "10000001",
//!   "kind": "group" | "dm",
//!   "frame_type": "like.create" | "like.destroy",
//!   "message_id": "170000000000000001",
//!   "reactor_id": "20000002" | null, // who reacted, not the message's author
//!   "reaction_count": 2,             // deduplicated, see Message::reaction_count
//!   "favorited_by": ["20000001"],
//!   "stored": true,
//!   "message": { /* model::Message */ }
//! }
//!
//! // "realtime://typing"
//! { "conversation_id": "10000001", "kind": "group" | "dm", "user_id": "20000001" | null }
//!
//! // "realtime://state"
//! {
//!   "connected": false,
//!   "status": "connecting" | "connected" | "disconnected" | "unauthorized",
//!   "detail": "handshake rejected" | null,
//!   "retry_in_ms": 4137 | null
//! }
//! ```
//!
//! Every message frame is written to the archive before it is emitted, so the
//! UI and the archive never disagree and a frame that lands while the polling
//! worker is idle is not lost.
//!
//! `watch_conversation` takes the conversation's kind alongside its id, because
//! the archive stores a DM under the *other participant's* user id — so a DM id
//! is shape-identical to a group id and cannot be told apart. It accepts either
//! that stored form or the composite `"{lo}+{hi}"` thread key, and subscribes to
//! `/direct_message/{lo}_{hi}`. Messages themselves arrive on the account's own
//! `/user/{id}` channel regardless, so a conversation subscription only adds
//! that thread's typing notices and read receipts.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::commands::SharedStore;
use crate::model::{id_sort_key, ConversationKind, Message};
use crate::store::Store;
use crate::tray;

const FAYE_URL: &str = "wss://push.groupme.com/faye";

/// Bayeux protocol version echoed by the server in the §9 handshake capture.
const BAYEUX_VERSION: &str = "1.0";

/// The real client lists `websocket` first and upgrades immediately (§9).
const CONNECTION_TYPE: &str = "websocket";

/// Fallback for `advice.timeout` when the handshake omits it. The capture shows
/// 600 000 ms.
const DEFAULT_ADVICE_TIMEOUT_MS: u64 = 600_000;

/// Slack added to `advice.timeout` before declaring the socket dead. A Faye
/// `/meta/connect` legitimately holds for the whole timeout with no traffic.
const WATCHDOG_SLACK: Duration = Duration::from_secs(60);

/// Websocket-level keepalive. Purely to stop a NAT or proxy idle-timer from
/// dropping a connection that is mid-hold; liveness is judged by the watchdog,
/// not by whether a pong comes back.
const PING_EVERY: Duration = Duration::from_secs(45);

const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_CEILING: Duration = Duration::from_secs(300);

/// Minimum gap between toasts for the same conversation. A burst of N messages
/// fires at most one toast per conversation per this window so the tray is not
/// flooded by a busy group.
const NOTIFY_FLOOR: Duration = Duration::from_secs(30);

/// Each variant carries the conversation's kind because the channel name cannot
/// be derived from a stored id alone — see `conversation_channel`.
#[derive(Debug)]
enum Command {
    Watch(String, ConversationKind),
    Unwatch(String),
    Typing(String, ConversationKind),
}

/// The live worker, held in Tauri managed state. `None` until a token has been
/// verified. Replacing the handle drops the previous one, which closes its
/// command channel — the old worker's shutdown signal — so a token rotation
/// swaps the socket rather than adding a second one.
pub type RealtimeSlot = Arc<tokio::sync::Mutex<Option<RealtimeHandle>>>;

pub struct RealtimeHandle {
    inner: Arc<HandleInner>,
}

struct HandleInner {
    tx: mpsc::UnboundedSender<Command>,
    // Shared with the worker as its own Arc, NOT by handing the worker an
    // Arc<HandleInner>: the worker holding the struct that owns `tx` would keep
    // its own command channel open forever, so dropping every handle would
    // never close the channel and the shutdown path could never fire.
    connected: Arc<AtomicBool>,
}

impl Clone for RealtimeHandle {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl RealtimeHandle {
    /// Subscribe to a conversation's channel. Idempotent, and safe while
    /// offline: the set is replayed on every reconnect.
    ///
    /// `kind` is required rather than inferred. A DM is stored under the other
    /// participant's user id, so its id looks exactly like a group id, and
    /// guessing wrong subscribes to a channel the account does not own — which
    /// GroupMe answers by failing authentication and dropping the session.
    pub fn watch_conversation(&self, conversation_id: &str, kind: ConversationKind) {
        self.send(Command::Watch(conversation_id.to_string(), kind));
    }

    pub fn unwatch_conversation(&self, conversation_id: &str) {
        self.send(Command::Unwatch(conversation_id.to_string()));
    }

    pub fn send_typing(&self, conversation_id: &str, kind: ConversationKind) {
        self.send(Command::Typing(conversation_id.to_string(), kind));
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Relaxed)
    }

    fn send(&self, cmd: Command) {
        // A closed channel means the worker stopped (rejected token, app
        // shutting down). Realtime is an accelerator over polling, so dropping
        // the command is the correct degradation.
        if self.inner.tx.send(cmd).is_err() {
            log::debug!("realtime worker is gone; command dropped");
        }
    }
}

/// Start the realtime worker. Returns immediately; the socket comes up in the
/// background and republishes its state over `realtime://state`.
pub fn spawn(
    app: tauri::AppHandle,
    store: SharedStore,
    token: String,
    user_id: String,
) -> RealtimeHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let connected = Arc::new(AtomicBool::new(false));
    let inner = Arc::new(HandleInner {
        tx,
        connected: Arc::clone(&connected),
    });

    let worker = Worker {
        app,
        store,
        user_id,
        connected,
        frames: Frames::new(token),
        notify_times: Mutex::new(HashMap::new()),
    };
    tauri::async_runtime::spawn(worker.run(rx));

    RealtimeHandle { inner }
}

struct Worker {
    app: tauri::AppHandle,
    store: SharedStore,
    user_id: String,
    connected: Arc<AtomicBool>,
    frames: Frames,
    /// Per-conversation last-notified times for the 30-second burst floor.
    notify_times: Mutex<HashMap<String, Instant>>,
}

#[derive(Debug)]
enum SessionError {
    /// The token was refused. Retrying with the same credential cannot succeed,
    /// so the worker stops rather than spinning.
    Unauthorized(String),
    Transport(String),
    /// Every `RealtimeHandle` was dropped.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Connecting,
    Connected,
    Disconnected,
    Unauthorized,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Connecting => "connecting",
            Status::Connected => "connected",
            Status::Disconnected => "disconnected",
            Status::Unauthorized => "unauthorized",
        }
    }
}

impl Worker {
    async fn run(self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let mut watched: BTreeMap<String, ConversationKind> = BTreeMap::new();
        let mut attempt: u32 = 0;

        loop {
            self.publish_state(Status::Connecting, None, None);
            let started = tokio::time::Instant::now();
            let outcome = self.session(&mut rx, &mut watched).await;
            self.connected.store(false, Ordering::Relaxed);

            let detail = match outcome {
                Err(SessionError::Shutdown) => {
                    log::info!("realtime worker shutting down");
                    return;
                }
                Err(SessionError::Unauthorized(detail)) => {
                    log::error!("realtime rejected the access token ({detail}); stopping. A new handle must be spawned when the token rotates.");
                    self.publish_state(Status::Unauthorized, Some(&detail), None);
                    return;
                }
                Err(SessionError::Transport(detail)) => {
                    log::warn!("realtime session ended: {detail}");
                    detail
                }
                Ok(()) => {
                    log::info!("realtime socket closed by the server");
                    "closed by server".to_string()
                }
            };

            // A session that stayed up is evidence the endpoint is healthy, so
            // the next blip starts from zero rather than inheriting an hour-old
            // backoff. One that died immediately keeps escalating.
            attempt = if started.elapsed() >= Duration::from_secs(60) {
                0
            } else {
                attempt.saturating_add(1)
            };

            let delay = backoff_delay(attempt);
            self.publish_state(Status::Disconnected, Some(&detail), Some(delay));
            if !self.wait_out_backoff(&mut rx, &mut watched, delay).await {
                return;
            }
        }
    }

    /// Sleeps for `delay` while still accepting subscription changes, so a user
    /// switching conversations offline is not queued behind a five-minute
    /// backoff. Returns `false` once every handle has been dropped.
    async fn wait_out_backoff(
        &self,
        rx: &mut mpsc::UnboundedReceiver<Command>,
        watched: &mut BTreeMap<String, ConversationKind>,
        delay: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return true,
                cmd = rx.recv() => match cmd {
                    None => return false,
                    // Typing is discarded rather than queued: by the time the
                    // socket is back the user has long stopped typing.
                    Some(Command::Typing(..)) => {}
                    Some(Command::Watch(id, kind)) => { watched.insert(id, kind); }
                    Some(Command::Unwatch(id)) => { watched.remove(&id); }
                },
            }
        }
    }

    /// One socket, from connect to close.
    async fn session(
        &self,
        rx: &mut mpsc::UnboundedReceiver<Command>,
        watched: &mut BTreeMap<String, ConversationKind>,
    ) -> Result<(), SessionError> {
        let (ws, _response) = tokio_tungstenite::connect_async(FAYE_URL)
            .await
            .map_err(|e| {
                // The whole chain, not just the outermost error: DNS failure,
                // connection refused and a rejected certificate all render
                // identically at the top level, and telling them apart is the
                // difference between "no network" and "TLS inspection broke us".
                SessionError::Transport(format!("connecting to {FAYE_URL}: {}", error_chain(&e)))
            })?;
        let (mut sink, mut stream) = ws.split();

        self.send_frames(&mut sink, vec![self.frames.handshake()])
            .await?;
        let hs = read_handshake(&mut stream).await?;
        log::info!(
            "realtime handshake accepted, hold {} ms",
            hs.advice_timeout_ms
        );

        let mut batch = Vec::with_capacity(watched.len() + 1);
        batch.push(
            self.frames
                .subscribe(&hs.client_id, &user_channel(&self.user_id)),
        );
        for channel in watched
            .iter()
            .filter_map(|(id, kind)| conversation_channel(id, *kind, &self.user_id))
        {
            batch.push(self.frames.subscribe(&hs.client_id, &channel));
        }
        self.send_frames(&mut sink, batch).await?;
        self.send_frames(&mut sink, vec![self.frames.connect(&hs.client_id)])
            .await?;

        self.connected.store(true, Ordering::Relaxed);
        self.publish_state(Status::Connected, None, None);

        let hold = Duration::from_millis(hs.advice_timeout_ms) + WATCHDOG_SLACK;
        let mut next_connect_at = far_future();
        let mut next_ping_at = tokio::time::Instant::now() + PING_EVERY;
        let mut watchdog_at = tokio::time::Instant::now() + hold;

        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(frame) = frame else { return Ok(()) };
                    let frame = frame.map_err(|e| {
                        SessionError::Transport(format!("reading a frame: {}", error_chain(&e)))
                    })?;
                    watchdog_at = tokio::time::Instant::now() + hold;

                    let text = match frame {
                        WsMessage::Text(t) => t,
                        // Faye is text-only, but a proxy that repacks frames as
                        // binary would otherwise silently drop every message.
                        WsMessage::Binary(b) => match String::from_utf8(b) {
                            Ok(t) => t,
                            Err(_) => continue,
                        },
                        WsMessage::Close(_) => return Ok(()),
                        _ => continue,
                    };

                    match self.handle_text(&text, watched).await? {
                        Flow::Idle => {}
                        Flow::ConnectAfter(d) => next_connect_at = tokio::time::Instant::now() + d,
                        Flow::Closed => return Ok(()),
                    }
                }

                cmd = rx.recv() => match cmd {
                    None => return Err(SessionError::Shutdown),
                    Some(Command::Watch(id, kind)) => {
                        if let Some(channel) = conversation_channel(&id, kind, &self.user_id) {
                            if watched.insert(id, kind).is_none() {
                                let f = self.frames.subscribe(&hs.client_id, &channel);
                                self.send_frames(&mut sink, vec![f]).await?;
                            }
                        }
                    }
                    Some(Command::Unwatch(id)) => {
                        if let Some(kind) = watched.remove(&id) {
                            if let Some(channel) =
                                conversation_channel(&id, kind, &self.user_id)
                            {
                                let f = self.frames.unsubscribe(&hs.client_id, &channel);
                                self.send_frames(&mut sink, vec![f]).await?;
                            }
                        }
                    }
                    Some(Command::Typing(conversation_id, kind)) => {
                        if let Some(channel) =
                            conversation_channel(&conversation_id, kind, &self.user_id)
                        {
                            let f = self.frames.publish(
                                &hs.client_id,
                                &channel,
                                self.typing_payload(),
                            );
                            self.send_frames(&mut sink, vec![f]).await?;
                        }
                    }
                },

                _ = tokio::time::sleep_until(next_connect_at) => {
                    next_connect_at = far_future();
                    let f = self.frames.connect(&hs.client_id);
                    self.send_frames(&mut sink, vec![f]).await?;
                }

                _ = tokio::time::sleep_until(next_ping_at) => {
                    next_ping_at = tokio::time::Instant::now() + PING_EVERY;
                    self.send_ws(&mut sink, WsMessage::Ping(Vec::new())).await?;
                }

                _ = tokio::time::sleep_until(watchdog_at) => {
                    return Err(SessionError::Transport(format!(
                        "no traffic for {}s while the server's hold was {} ms",
                        hold.as_secs(), hs.advice_timeout_ms
                    )));
                }
            }
        }
    }
}

// ---------------------------------------------------------------- frames

/// Outgoing Bayeux frames. Separate from [`Worker`] so the wire format is
/// testable without a Tauri app handle.
struct Frames {
    token: String,
    next: AtomicU64,
}

impl Frames {
    fn new(token: String) -> Self {
        Self {
            token,
            next: AtomicU64::new(1),
        }
    }

    /// Bayeux correlation id. Base 36 to match the `"d"`, `"e"`, `"f"`, `"g"`
    /// sequence in the §9 capture.
    fn next_id(&self) -> String {
        to_base36(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn handshake(&self) -> Value {
        json!({
            "channel": "/meta/handshake",
            "version": BAYEUX_VERSION,
            "supportedConnectionTypes": [CONNECTION_TYPE],
            "id": self.next_id(),
        })
    }

    fn connect(&self, client_id: &str) -> Value {
        json!({
            "channel": "/meta/connect",
            "clientId": client_id,
            "connectionType": CONNECTION_TYPE,
            "id": self.next_id(),
        })
    }

    /// The only frame the capture shows carrying credentials. The `/meta/
    /// unsubscribe` sent in the *same* batch has no `ext` at all, which is the
    /// evidence that GroupMe's Faye extension stamps the token onto subscribes
    /// (and publishes) rather than onto everything outgoing — so handshake and
    /// connect are deliberately sent without it.
    fn subscribe(&self, client_id: &str, channel: &str) -> Value {
        json!({
            "channel": "/meta/subscribe",
            "clientId": client_id,
            "subscription": channel,
            "id": self.next_id(),
            "ext": { "access_token": self.token },
        })
    }

    fn unsubscribe(&self, client_id: &str, channel: &str) -> Value {
        json!({
            "channel": "/meta/unsubscribe",
            "clientId": client_id,
            "subscription": channel,
            "id": self.next_id(),
        })
    }

    /// UNVERIFIED: no publish frame was captured. The envelope follows the
    /// documented subscribe form; only `data` is a guess.
    fn publish(&self, client_id: &str, channel: &str, data: Value) -> Value {
        json!({
            "channel": channel,
            "clientId": client_id,
            "data": data,
            "id": self.next_id(),
            "ext": { "access_token": self.token },
        })
    }
}

impl Worker {
    async fn send_frames<S>(&self, sink: &mut S, frames: Vec<Value>) -> Result<(), SessionError>
    where
        S: futures_util::Sink<WsMessage> + Unpin,
        S::Error: std::error::Error,
    {
        if frames.is_empty() {
            return Ok(());
        }
        // Batched as an array: that is the wire form in §9, where an
        // unsubscribe and a subscribe travelled together in one message.
        let payload = Value::Array(frames).to_string();
        self.send_ws(sink, WsMessage::Text(payload)).await
    }

    async fn send_ws<S>(&self, sink: &mut S, msg: WsMessage) -> Result<(), SessionError>
    where
        S: futures_util::Sink<WsMessage> + Unpin,
        S::Error: std::error::Error,
    {
        sink.send(msg)
            .await
            .map_err(|e| SessionError::Transport(format!("sending: {}", error_chain(&e))))
    }
}

// ------------------------------------------------------------- dispatch

enum Flow {
    Idle,
    ConnectAfter(Duration),
    Closed,
}

impl Worker {
    async fn handle_text(
        &self,
        text: &str,
        watched: &mut BTreeMap<String, ConversationKind>,
    ) -> Result<Flow, SessionError> {
        let mut flow = Flow::Idle;

        for msg in parse_batch(text) {
            match channel_of(&msg) {
                "/meta/connect" => {
                    if !is_successful(&msg) {
                        return Err(classify_failure("/meta/connect", failure_of(&msg)));
                    }
                    // Faye holds the connect open and answers when it has
                    // something or the hold expires; the client immediately
                    // opens the next one.
                    if !matches!(flow, Flow::Closed) {
                        flow = Flow::ConnectAfter(advice_interval(&msg));
                    }
                }

                "/meta/subscribe" => {
                    if is_successful(&msg) {
                        continue;
                    }
                    let err = failure_of(&msg);
                    if looks_like_auth_failure(&err) {
                        return Err(SessionError::Unauthorized(format!(
                            "/meta/subscribe: {err}"
                        )));
                    }
                    // A group that can no longer be subscribed to (left,
                    // deleted, no longer visible) leaves the watch set, or every
                    // reconnect retries it forever.
                    if let Some((id, _)) = msg
                        .get("subscription")
                        .and_then(Value::as_str)
                        .and_then(conversation_of_channel)
                    {
                        watched.remove(&id);
                    }
                    log::warn!("realtime subscribe rejected: {err}");
                }

                "/meta/disconnect" => flow = Flow::Closed,
                "/meta/handshake" | "/meta/unsubscribe" | "/meta/ping" => {}

                channel => {
                    if let Some(data) = msg.get("data") {
                        self.handle_data(channel, data).await;
                    }
                }
            }
        }

        Ok(flow)
    }

    async fn handle_data(&self, channel: &str, data: &Value) {
        let Some(inbound) = interpret(channel, data) else {
            log::debug!(
                "realtime frame ignored on {channel}: type {:?}",
                data.get("type")
            );
            return;
        };

        match inbound {
            Inbound::Typing {
                conversation_id,
                kind,
                user_id,
            } => self.emit(
                "realtime://typing",
                json!({
                    "conversation_id": conversation_id,
                    "kind": kind.as_str(),
                    "user_id": user_id,
                }),
            ),

            Inbound::Message(inc) => {
                let stored = self.persist(&inc).await;
                self.maybe_notify(&inc);
                self.emit(
                    "realtime://message",
                    json!({
                        "conversation_id": inc.conversation_id,
                        "kind": inc.kind.as_str(),
                        "frame_type": inc.frame_type,
                        "system_event": inc.message.event.as_ref().and_then(|e| e.kind.clone()),
                        "stored": stored,
                        "message": inc.message,
                    }),
                );
            }

            Inbound::Reaction {
                incoming: inc,
                reactor_id,
            } => {
                // The frame carries the whole message, so the archive's
                // favorited_by / reactions are refreshed by the same write.
                let stored = self.persist(&inc).await;
                self.emit(
                    "realtime://reaction",
                    json!({
                        "conversation_id": inc.conversation_id,
                        "kind": inc.kind.as_str(),
                        "frame_type": inc.frame_type,
                        "message_id": inc.message.id,
                        "reactor_id": reactor_id,
                        "reaction_count": inc.message.reaction_count(),
                        "favorited_by": inc.message.favorited_by,
                        "stored": stored,
                        "message": inc.message,
                    }),
                );
            }
        }
    }

    /// Archive first, emit second, so the UI never shows a message the archive
    /// does not have.
    async fn persist(&self, inc: &Incoming) -> bool {
        let store = Arc::clone(&self.store);
        let conversation_id = inc.conversation_id.clone();
        let message = inc.message.clone();

        // The guard is taken inside the closure and dropped when it returns —
        // never held across an await, which would make this future non-`Send`.
        let joined = tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
            persist_message(&mut guard, &conversation_id, &message)
        })
        .await;

        match joined {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                log::warn!("archiving a realtime frame: {e:#}");
                false
            }
            Err(e) => {
                log::warn!("archiving a realtime frame: {e}");
                false
            }
        }
    }

    /// `emit_to("main")`, never `emit`. A broadcast also reaches the online
    /// `web.groupme.com` webview, which holds `core:event:allow-listen` — that
    /// would hand archive contents to a third-party origin.
    fn emit(&self, event: &str, payload: Value) {
        if let Err(e) = self.app.emit_to("main", event, payload) {
            log::warn!("emitting {event}: {e}");
        }
    }

    /// Shaped like the frame the web client publishes: the type, who is
    /// typing, and when they started, in milliseconds.
    fn typing_payload(&self) -> Value {
        json!({
            "type": "typing",
            "user_id": self.user_id,
            "started": now_millis(),
        })
    }

    fn publish_state(&self, status: Status, detail: Option<&str>, retry_in: Option<Duration>) {
        self.emit(
            "realtime://state",
            json!({
                "connected": status == Status::Connected,
                "status": status.as_str(),
                "detail": detail,
                "retry_in_ms": retry_in.map(|d| d.as_millis() as u64),
            }),
        );
    }

    /// Fire a desktop notification for an incoming message, subject to several
    /// suppression gates. Best-effort: any error is logged and never propagates.
    ///
    /// Suppressed when:
    /// - The message is a system event (joins, leaves, deletions).
    /// - The sender is the signed-in account (no echo toasts).
    /// - The conversation fired a toast within the last 30 seconds (burst floor).
    /// - The main window is focused (tray::notify_message handles this internally).
    /// - Notifications are globally off or the conversation is muted (also handled
    ///   inside tray::notify_message).
    ///
    /// # Click-to-open
    /// tauri-plugin-notification v2 does not expose a WinRT Toast click callback
    /// through its Rust API on Windows desktop. Windows may bring a running app to
    /// the foreground when the user clicks a toast (COM activation), but that
    /// requires registering the process as a COM server — which is out of scope and
    /// not provided by this plugin. At minimum, the notification fires and is
    /// visible in the Action Centre; click behaviour is OS-default.
    fn maybe_notify(&self, inc: &Incoming) {
        // System messages (joins, leaves, deletions) carry no user content.
        if inc.message.system || inc.message.sender_type.as_deref() == Some("system") {
            return;
        }
        // Compare both fields: GroupMe sends user_id on group messages and
        // sender_id on DMs; a message from self must never produce a toast.
        if inc.message.user_id.as_deref() == Some(self.user_id.as_str())
            || inc.message.sender_id.as_deref() == Some(self.user_id.as_str())
        {
            return;
        }
        // Rate-limit: one toast per conversation per 30 s to absorb bursts.
        {
            let mut times = self.notify_times.lock().unwrap_or_else(|e| e.into_inner());
            if !burst_floor_pass(&mut times, &inc.conversation_id, Instant::now()) {
                return;
            }
        }

        let app = self.app.clone();
        let store = Arc::clone(&self.store);
        let conversation_id = inc.conversation_id.clone();
        let kind = inc.kind;
        let sender = inc.message.name.clone().unwrap_or_default();
        let body = message_body(&inc.message);

        // Look up the conversation name and fire the toast on the blocking pool.
        // The store lock is blocking (never held across an await), so it cannot
        // run on the async executor directly.
        drop(tokio::task::spawn_blocking(move || {
            // For a DM the conversation IS the other person, so passing sender as
            // both arguments lets notification_title(s, s) collapse to just the
            // sender name without stutter. For a group, look up the stored name.
            let conversation = if kind == ConversationKind::Group {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .list_conversations()
                    .ok()
                    .and_then(|convs| convs.into_iter().find(|c| c.id == conversation_id))
                    .and_then(|c| c.name)
                    .unwrap_or_else(|| sender.clone())
            } else {
                sender.clone()
            };
            tray::notify_message(&app, &conversation_id, &sender, &body, &conversation);
        }));
    }
}

/// Build a notification body from a message.
///
/// Prefers the text field; falls back to "Sent a picture" when the message
/// carries only attachments (text is absent or blank). The empty-string case is
/// forwarded to `tray::notification_body`, which emits "New message".
fn message_body(m: &Message) -> String {
    let text = m.text.as_deref().unwrap_or("").trim();
    if text.is_empty() && !m.attachments.is_empty() {
        return "Sent a picture".to_string();
    }
    text.to_string()
}

/// Returns `true` and records `now` when `conv_id` has not been notified within
/// `NOTIFY_FLOOR`; returns `false` (suppress) when it has. Updates `times` on
/// the first call so the next call within the window is suppressed.
///
/// Extracted as a pure function so it can be unit-tested without a Tauri app.
fn burst_floor_pass(times: &mut HashMap<String, Instant>, conv_id: &str, now: Instant) -> bool {
    if let Some(last) = times.get(conv_id) {
        if now.duration_since(*last) < NOTIFY_FLOOR {
            return false;
        }
    }
    times.insert(conv_id.to_string(), now);
    true
}

/// Writes one realtime message into the archive.
///
/// Deliberately does **not** advance the conversation's sync cursors. The
/// polling worker owns those, and a frame that arrives mid-page would move
/// `newest_id` past messages the poller has not fetched yet. `insert_messages`
/// is idempotent, so the poller re-fetching this same message later is a no-op.
fn persist_message(store: &mut Store, conversation_id: &str, m: &Message) -> anyhow::Result<()> {
    store.insert_messages(conversation_id, std::slice::from_ref(m))?;

    // Gated on `event`, not on `system`: DM messages omit `system` entirely, so
    // gating on it drops every DM edit and delete — which is exactly what
    // arrives here as a `direct_message.create` from sender `system`.
    if let Some(ev) = &m.event {
        if matches!(
            ev.kind.as_deref(),
            Some("message.update") | Some("message.deleted")
        ) {
            store.apply_event(ev)?;
        }
    }
    Ok(())
}

// -------------------------------------------------------- frame parsing

struct Handshake {
    client_id: String,
    advice_timeout_ms: u64,
}

struct Incoming {
    conversation_id: String,
    kind: ConversationKind,
    frame_type: String,
    message: Message,
}

enum Inbound {
    Message(Incoming),
    Reaction {
        incoming: Incoming,
        reactor_id: Option<String>,
    },
    Typing {
        conversation_id: String,
        kind: ConversationKind,
        user_id: Option<String>,
    },
}

async fn read_handshake<S>(stream: &mut S) -> Result<Handshake, SessionError>
where
    S: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let read = async {
        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|e| {
                SessionError::Transport(format!("reading the handshake: {}", error_chain(&e)))
            })?;
            let text = match frame {
                WsMessage::Text(t) => t,
                WsMessage::Binary(b) => String::from_utf8(b).unwrap_or_default(),
                WsMessage::Close(_) => {
                    return Err(SessionError::Transport(
                        "closed before the handshake completed".into(),
                    ))
                }
                _ => continue,
            };
            for msg in parse_batch(&text) {
                if channel_of(&msg) == "/meta/handshake" {
                    return parse_handshake(&msg);
                }
            }
        }
        Err(SessionError::Transport(
            "stream ended before the handshake completed".into(),
        ))
    };

    tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .map_err(|_| SessionError::Transport("handshake timed out".into()))?
}

fn parse_handshake(v: &Value) -> Result<Handshake, SessionError> {
    if !is_successful(v) {
        return Err(classify_failure("/meta/handshake", failure_of(v)));
    }
    let Some(client_id) = v
        .get("clientId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(SessionError::Transport(
            "handshake carried no clientId".into(),
        ));
    };
    Ok(Handshake {
        client_id: client_id.to_string(),
        advice_timeout_ms: v
            .get("advice")
            .and_then(|a| a.get("timeout"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_ADVICE_TIMEOUT_MS),
    })
}

/// Bayeux messages arrive batched several per array (§9), but a lone object is
/// legal too.
fn parse_batch(text: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(v)) => v,
        Ok(v @ Value::Object(_)) => vec![v],
        Ok(_) => Vec::new(),
        Err(e) => {
            log::warn!("unparseable realtime frame: {e}");
            Vec::new()
        }
    }
}

fn channel_of(v: &Value) -> &str {
    v.get("channel").and_then(Value::as_str).unwrap_or_default()
}

fn is_successful(v: &Value) -> bool {
    v.get("successful")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn failure_of(v: &Value) -> String {
    v.get("error")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("unspecified")
        .to_string()
}

fn advice_interval(v: &Value) -> Duration {
    Duration::from_millis(
        v.get("advice")
            .and_then(|a| a.get("interval"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

fn classify_failure(context: &str, error: String) -> SessionError {
    if looks_like_auth_failure(&error) {
        SessionError::Unauthorized(format!("{context}: {error}"))
    } else {
        SessionError::Transport(format!("{context} failed: {error}"))
    }
}

/// A refused token must not be retried on a backoff — it will be refused
/// identically forever, and the retries look like a network problem in the logs.
fn looks_like_auth_failure(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "401",
        "403",
        "unauthorized",
        "unauthenticated",
        "forbidden",
        "access_token",
        "access token",
        "invalid token",
        "authentication",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

/// Turn one `data` payload into something the UI and the archive can use.
///
/// Unknown `type` values are dropped rather than guessed at: a frame whose
/// `subject` is not a message (a membership object, say) would otherwise be
/// written into the archive as one.
fn interpret(channel: &str, data: &Value) -> Option<Inbound> {
    let frame_type = data.get("type").and_then(Value::as_str).unwrap_or_default();

    if frame_type.contains("typing") {
        let (conversation_id, kind) = thread_of(data, channel)
            .or_else(|| thread_of(data.get("subject").unwrap_or(&Value::Null), channel))?;
        return Some(Inbound::Typing {
            conversation_id,
            kind,
            user_id: string_id(data.get("user_id"))
                .or_else(|| string_id(data.get("subject")?.get("user_id"))),
        });
    }

    let subject = data.get("subject")?;

    match frame_type {
        // `direct_message.create` also carries DM edits and deletes, sent from
        // `sender_id: "system"` with the mutation in `subject.event`.
        "line.create" | "direct_message.create" => {
            let message = parse_message(subject, frame_type)?;
            let (conversation_id, kind) = conversation_of(subject, channel, &message)?;
            Some(Inbound::Message(Incoming {
                conversation_id,
                kind,
                frame_type: frame_type.to_string(),
                message,
            }))
        }

        // A like frame is shaped differently from every other one: the message
        // is nested at `subject.line`, and the reaction list is a *sibling* of
        // it rather than a field on it. Parsing `subject` as a message here
        // finds no id and drops every reaction.
        "like.create" | "like.destroy" => {
            let mut line = subject.get("line")?.clone();
            if let (Some(obj), Some(reactions)) = (line.as_object_mut(), subject.get("reactions")) {
                obj.insert("reactions".to_string(), reactions.clone());
            }
            let message = parse_message(&line, frame_type)?;
            let (conversation_id, kind) = conversation_of(&line, channel, &message)?;
            Some(Inbound::Reaction {
                // `subject.user_id` is the reactor, not the message's author.
                reactor_id: string_id(subject.get("user_id")),
                incoming: Incoming {
                    conversation_id,
                    kind,
                    frame_type: frame_type.to_string(),
                    message,
                },
            })
        }

        _ => None,
    }
}

fn parse_message(v: &Value, frame_type: &str) -> Option<Message> {
    match serde_json::from_value::<Message>(v.clone()) {
        Ok(m) if !m.id.is_empty() => Some(m),
        Ok(_) => None,
        Err(e) => {
            log::debug!("realtime {frame_type} subject is not a message: {e}");
            None
        }
    }
}

/// The thread a frame belongs to, from the keys GroupMe puts on it.
fn thread_of(v: &Value, channel: &str) -> Option<(String, ConversationKind)> {
    if let Some(id) = string_id(v.get("group_id")) {
        return Some((id, ConversationKind::Group));
    }
    // A DM frame carries its thread key directly as `chat_id`; it does not have
    // to be rebuilt from the two user ids.
    if let Some(id) = string_id(v.get("chat_id")) {
        return Some((id, ConversationKind::Dm));
    }
    if let Some(id) = string_id(v.get("conversation_id")) {
        let kind = if id.contains('+') {
            ConversationKind::Dm
        } else {
            ConversationKind::Group
        };
        return Some((id, kind));
    }
    conversation_of_channel(channel)
}

fn conversation_of(
    subject: &Value,
    channel: &str,
    message: &Message,
) -> Option<(String, ConversationKind)> {
    if let Some(found) = thread_of(subject, channel) {
        return Some(found);
    }
    // Last resort: rebuild the DM key from the two participants.
    let a = message
        .sender_id
        .as_deref()
        .or(message.user_id.as_deref())?;
    let b = message.recipient_id.as_deref()?;
    Some((dm_conversation_id(a, b), ConversationKind::Dm))
}

/// GroupMe writes ids as strings almost everywhere and as numbers inside
/// `event.data`. Accept either, always yield a non-empty string.
fn string_id(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The channel a conversation is subscribed on. Groups are `/group/{id}` (§9);
/// a DM thread is `/direct_message/{lo}_{hi}` — the same two ids as the
/// `chat_id`, joined with `_` instead of `+`.
///
/// The kind has to be passed in; it cannot be inferred from the id. **The
/// archive stores a DM under the *other participant's* user id**, not under the
/// composite `"{a}+{b}"` thread key — `upsert_chat` uses `other_user.id` as the
/// primary key. So a DM id is a bare number, indistinguishable in shape from a
/// group id, and guessing by looking for a `+` silently classified every DM as a
/// group. That produced a subscribe to `/group/{some_user_id}` — a channel the
/// account does not own — which GroupMe answers with
/// `Access token authentication failed`, killing the whole realtime session on
/// the first DM opened.
///
/// Both DM forms are accepted, since the composite key is what arrives on frames.
fn conversation_channel(
    conversation_id: &str,
    kind: ConversationKind,
    my_user_id: &str,
) -> Option<String> {
    if conversation_id.is_empty() {
        return None;
    }
    Some(match kind {
        ConversationKind::Group => format!("/group/{conversation_id}"),
        ConversationKind::Dm => {
            let thread = if conversation_id.contains('+') {
                conversation_id.to_string()
            } else {
                // Stored form: the other participant. Rebuild the thread key.
                dm_conversation_id(my_user_id, conversation_id)
            };
            format!("/direct_message/{}", thread.replace('+', "_"))
        }
    })
}

/// The inverse, for attributing a frame that named no thread of its own.
fn conversation_of_channel(channel: &str) -> Option<(String, ConversationKind)> {
    if let Some(id) = channel.strip_prefix("/group/").filter(|id| !id.is_empty()) {
        return Some((id.to_string(), ConversationKind::Group));
    }
    let dm = channel
        .strip_prefix("/direct_message/")
        .filter(|id| id.contains('_'))?;
    Some((dm.replace('_', "+"), ConversationKind::Dm))
}

/// The account's own channel. Every `line.create`, `direct_message.create` and
/// `like.create` observed in the capture arrived here rather than on the
/// per-group channel — this subscription, not the group ones, is what makes the
/// UI live. The group and DM channels carry typing and read receipts.
fn user_channel(user_id: &str) -> String {
    format!("/user/{user_id}")
}

/// `"{lower}+{higher}"`, sorted **numerically**. Sorting the two ids as strings
/// yields the wrong key whenever they differ in length.
fn dm_conversation_id(a: &str, b: &str) -> String {
    let (lo, hi) = if id_sort_key(a) <= id_sort_key(b) {
        (a, b)
    } else {
        (b, a)
    };
    format!("{lo}+{hi}")
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Exponential with jitter over the top quarter of the window. Without the
/// jitter every client that lost the same server reconnects in the same
/// millisecond.
fn backoff_delay(attempt: u32) -> Duration {
    let base = BACKOFF_BASE.as_millis() as u64;
    let capped = base
        .saturating_mul(1u64 << attempt.min(8))
        .min(BACKOFF_CEILING.as_millis() as u64);
    let window = capped / 4;
    let jitter = if window == 0 {
        0
    } else {
        coarse_entropy() % window
    };
    Duration::from_millis(capped - window + jitter)
}

/// Jitter only. A dedicated RNG dependency for a few bits of scheduling noise
/// is not worth the supply chain.
fn coarse_entropy() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn far_future() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(3600)
}

/// `tungstenite`'s outermost error renders identically for DNS failure, a
/// refused connection and a rejected certificate. The chain is what tells them
/// apart, and it is the reason the TLS failure in v0.1.0 stayed invisible.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        out.push_str(" <- ");
        out.push_str(&inner.to_string());
        source = inner.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every id below is synthetic and of the right shape: user 2000000x,
    // group 1000000x, message 17000000000000000x. Nothing from a real capture.

    fn handshake_ok() -> Value {
        serde_json::json!([{
            "id": "1",
            "channel": "/meta/handshake",
            "successful": true,
            "version": "1.0",
            "supportedConnectionTypes": [
                "long-polling", "cross-origin-long-polling", "callback-polling",
                "websocket", "eventsource", "in-process"
            ],
            "clientId": "0123456789abcdef0123456789abcdef",
            "advice": { "reconnect": "retry", "interval": 0, "timeout": 600000 }
        }])
    }

    #[test]
    fn handshake_yields_the_client_id_and_the_hold() {
        let batch = parse_batch(&handshake_ok().to_string());
        assert_eq!(batch.len(), 1);
        let hs = parse_handshake(&batch[0]).expect("handshake accepted");
        assert_eq!(hs.client_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(hs.advice_timeout_ms, 600_000);
    }

    #[test]
    fn a_handshake_without_advice_falls_back_to_the_documented_hold() {
        let v = serde_json::json!({
            "channel": "/meta/handshake", "successful": true, "clientId": "abc"
        });
        assert_eq!(
            parse_handshake(&v).expect("accepted").advice_timeout_ms,
            DEFAULT_ADVICE_TIMEOUT_MS
        );
    }

    #[test]
    fn a_rejected_token_is_not_a_network_error() {
        // The distinction is what stops a dead credential from spinning.
        let v = serde_json::json!({
            "channel": "/meta/handshake", "successful": false,
            "error": "401:invalid access_token"
        });
        assert!(matches!(
            parse_handshake(&v),
            Err(SessionError::Unauthorized(_))
        ));

        let v = serde_json::json!({
            "channel": "/meta/handshake", "successful": false,
            "error": "503:server unavailable"
        });
        assert!(matches!(
            parse_handshake(&v),
            Err(SessionError::Transport(_))
        ));
    }

    #[test]
    fn a_handshake_that_succeeded_without_a_client_id_is_rejected() {
        let v = serde_json::json!({ "channel": "/meta/handshake", "successful": true });
        assert!(matches!(
            parse_handshake(&v),
            Err(SessionError::Transport(_))
        ));
    }

    #[test]
    fn a_batch_and_a_lone_object_both_parse() {
        assert_eq!(
            parse_batch(r#"[{"channel":"/a"},{"channel":"/b"}]"#).len(),
            2
        );
        assert_eq!(parse_batch(r#"{"channel":"/a"}"#).len(), 1);
        // Junk must not panic or take the socket down.
        assert!(parse_batch("not json").is_empty());
        assert!(parse_batch("42").is_empty());
    }

    #[test]
    fn a_group_message_frame_is_attributed_to_its_group() {
        let data = serde_json::json!({
            "type": "line.create",
            "subject": {
                "id": "170000000000000001",
                "group_id": "10000001",
                "user_id": "20000001",
                "sender_id": "20000001",
                "sender_type": "user",
                "name": "Example Sender",
                "text": "Example message body",
                "created_at": 1785301508,
                "system": false,
                "favorited_by": [],
                "attachments": []
            }
        });
        let Some(Inbound::Message(inc)) = interpret("/group/10000001", &data) else {
            panic!("expected a message");
        };
        assert_eq!(inc.conversation_id, "10000001");
        assert_eq!(inc.kind, ConversationKind::Group);
        assert_eq!(inc.frame_type, "line.create");
        assert_eq!(inc.message.text.as_deref(), Some("Example message body"));
    }

    #[test]
    fn a_dm_frame_uses_the_chat_id_on_the_frame_rather_than_rebuilding_it() {
        // The thread key arrives directly; deriving it from the two user ids is
        // only ever a fallback.
        let data = serde_json::json!({
            "type": "direct_message.create",
            "subject": {
                "id": "170000000000000002",
                "chat_id": "20000001+20000002",
                "user_id": "20000002",
                "sender_id": "20000002",
                "recipient_id": "20000001",
                "text": "Example direct message",
                "created_at": 1785301509,
                "favorited_by": [],
                "attachments": []
            }
        });
        let Some(Inbound::Message(inc)) = interpret("/user/20000001", &data) else {
            panic!("expected a message");
        };
        assert_eq!(inc.conversation_id, "20000001+20000002");
        assert_eq!(inc.kind, ConversationKind::Dm);
    }

    #[test]
    fn a_dm_edit_arrives_as_a_create_from_system_and_keeps_its_event() {
        // The socket is not create-only for DMs: edits and deletes come through
        // as direct_message.create from sender "system", carrying the mutation.
        let data = serde_json::json!({
            "type": "direct_message.create",
            "subject": {
                "id": "170000000000000003",
                "chat_id": "20000001+20000002",
                "sender_id": "system",
                "sender_type": "system",
                "text": "This message was deleted",
                "created_at": 1785301510,
                "event": {
                    "type": "message.deleted",
                    "data": { "message_id": "170000000000000002", "deleted_at": 1785301510 }
                }
            }
        });
        let Some(Inbound::Message(inc)) = interpret("/user/20000001", &data) else {
            panic!("expected a message");
        };
        assert_eq!(inc.kind, ConversationKind::Dm);
        let event = inc.message.event.as_ref().expect("the mutation survives");
        assert_eq!(event.kind.as_deref(), Some("message.deleted"));
        assert_eq!(
            event.target_message_id().as_deref(),
            Some("170000000000000002")
        );
    }

    #[test]
    fn a_like_frame_reads_the_message_out_of_subject_line() {
        // Unlike every other frame, a like nests the message under `line` and
        // puts the reaction list beside it. Parsing `subject` as a message here
        // finds no id and silently drops every reaction.
        let data = serde_json::json!({
            "type": "like.create",
            "subject": {
                "line": {
                    "id": "170000000000000004",
                    "group_id": "10000001",
                    "user_id": "20000001",
                    "name": "Example Sender",
                    "text": "Example message body",
                    "created_at": 1785301511,
                    "favorited_at": 1785301600,
                    "favorited_by": ["20000002", "20000002"],
                    "attachments": []
                },
                "reactions": [
                    { "type": "unicode", "code": "\u{1f923}", "user_ids": ["20000002"] }
                ],
                "user_id": "20000002",
                "user_reaction": {
                    "type": "unicode", "code": "\u{1f923}", "user_ids": ["20000002"]
                }
            }
        });
        let Some(Inbound::Reaction {
            incoming,
            reactor_id,
        }) = interpret("/user/20000001", &data)
        else {
            panic!("expected a reaction");
        };
        assert_eq!(incoming.message.id, "170000000000000004");
        assert_eq!(incoming.conversation_id, "10000001");
        assert_eq!(incoming.kind, ConversationKind::Group);
        // The sibling reaction list has to reach the message, or the archive
        // keeps only the favorited_by half of the story.
        assert_eq!(incoming.message.reactions.len(), 1);
        assert_eq!(
            incoming.message.reaction_count(),
            1,
            "one reactor, listed twice"
        );
        // The reactor, not the author.
        assert_eq!(reactor_id.as_deref(), Some("20000002"));
    }

    #[test]
    fn a_typing_frame_resolves_its_conversation() {
        let data = serde_json::json!({
            "type": "typing", "user_id": "20000001", "group_id": "10000001"
        });
        let Some(Inbound::Typing {
            conversation_id,
            kind,
            user_id,
        }) = interpret("/group/10000001", &data)
        else {
            panic!("expected typing");
        };
        assert_eq!(conversation_id, "10000001");
        assert_eq!(kind, ConversationKind::Group);
        assert_eq!(user_id.as_deref(), Some("20000001"));
    }

    #[test]
    fn typing_falls_back_to_the_channel_when_the_frame_names_no_thread() {
        let data = serde_json::json!({ "type": "typing" });
        let Some(Inbound::Typing {
            conversation_id, ..
        }) = interpret("/group/10000002", &data)
        else {
            panic!("expected typing");
        };
        assert_eq!(conversation_id, "10000002");
    }

    #[test]
    fn an_unknown_frame_type_is_dropped_rather_than_archived_as_a_message() {
        // A membership object is not a message; writing one into the archive
        // because it happened to carry an id would be worse than ignoring it.
        let data = serde_json::json!({
            "type": "membership.create",
            "subject": { "id": "1000000001", "user_id": "20000001", "group_id": "10000001" }
        });
        assert!(interpret("/group/10000001", &data).is_none());
    }

    #[test]
    fn a_frame_with_no_data_type_at_all_is_dropped() {
        assert!(interpret("/group/10000001", &serde_json::json!({})).is_none());
        assert!(interpret("/group/10000001", &serde_json::json!({ "subject": 7 })).is_none());
    }

    #[test]
    fn a_message_frame_with_no_resolvable_thread_is_dropped() {
        let data = serde_json::json!({
            "type": "line.create",
            "subject": { "id": "170000000000000005", "text": "orphan", "created_at": 1 }
        });
        assert!(interpret("/unknown/channel", &data).is_none());
    }

    #[test]
    fn a_dm_thread_key_sorts_numerically_not_lexically() {
        // "9999999" > "20000001" as strings, but not as numbers.
        assert_eq!(
            dm_conversation_id("20000001", "9999999"),
            "9999999+20000001"
        );
        assert_eq!(
            dm_conversation_id("9999999", "20000001"),
            "9999999+20000001"
        );
        assert_eq!(
            dm_conversation_id("20000001", "20000002"),
            "20000001+20000002"
        );
    }

    #[test]
    fn channel_names_round_trip_for_both_conversation_kinds() {
        const ME: &str = "20000001";
        assert_eq!(
            conversation_channel("10000001", ConversationKind::Group, ME).as_deref(),
            Some("/group/10000001")
        );
        // The DM channel joins the two ids with `_`, where the chat_id uses `+`.
        assert_eq!(
            conversation_channel("20000001+20000002", ConversationKind::Dm, ME).as_deref(),
            Some("/direct_message/20000001_20000002")
        );
        // The form the archive actually stores: a DM keyed by the *other*
        // participant. Treating this as a group subscribed to
        // `/group/20000002`, which GroupMe rejects as an auth failure and which
        // tore down the session on the first DM opened.
        assert_eq!(
            conversation_channel("20000002", ConversationKind::Dm, ME).as_deref(),
            Some("/direct_message/20000001_20000002"),
            "a DM stored under the other participant's id must still resolve to its thread channel"
        );
        // Ordering is numeric, and our id is not reliably the smaller one.
        assert_eq!(
            conversation_channel("999", ConversationKind::Dm, "20000001").as_deref(),
            Some("/direct_message/999_20000001")
        );
        assert_eq!(conversation_channel("", ConversationKind::Group, ME), None);
        assert_eq!(user_channel("20000001"), "/user/20000001");

        assert_eq!(
            conversation_of_channel("/group/10000001"),
            Some(("10000001".to_string(), ConversationKind::Group))
        );
        assert_eq!(
            conversation_of_channel("/direct_message/20000001_20000002"),
            Some(("20000001+20000002".to_string(), ConversationKind::Dm))
        );
        assert_eq!(conversation_of_channel("/user/20000001"), None);
        assert_eq!(conversation_of_channel("/group/"), None);
    }

    #[test]
    fn a_read_receipt_on_a_dm_channel_is_attributed_to_its_thread() {
        // Not archived — but it must not be mistaken for a message either.
        let data = serde_json::json!({
            "type": "read_receipt.create",
            "subject": {
                "chat_id": "20000001+20000002",
                "id": "170000000000000006",
                "message_id": "170000000000000006",
                "read_at": 1785301512,
                "user_id": "20000001"
            }
        });
        assert!(interpret("/direct_message/20000001_20000002", &data).is_none());
    }

    #[test]
    fn only_the_subscribe_frame_carries_the_token() {
        // The capture shows an unsubscribe and a subscribe in one batch, with
        // ext.access_token on the subscribe only.
        let f = Frames::new("test-token".to_string());
        let sub = f.subscribe("cid", "/group/10000001");
        assert_eq!(sub["ext"]["access_token"], "test-token");
        assert_eq!(sub["subscription"], "/group/10000001");
        assert_eq!(sub["channel"], "/meta/subscribe");

        let unsub = f.unsubscribe("cid", "/group/10000001");
        assert!(unsub.get("ext").is_none(), "unsubscribe carries no token");

        let hs = f.handshake();
        assert!(hs.get("ext").is_none());
        assert_eq!(hs["supportedConnectionTypes"][0], "websocket");
        assert_eq!(hs["version"], "1.0");

        let connect = f.connect("cid");
        assert!(connect.get("ext").is_none());
        assert_eq!(connect["connectionType"], "websocket");
        assert_eq!(connect["clientId"], "cid");

        // A publish is authenticated like a subscribe.
        let publish = f.publish("cid", "/group/10000001", json!({ "type": "typing" }));
        assert_eq!(publish["ext"]["access_token"], "test-token");
        assert_eq!(publish["channel"], "/group/10000001");
        assert_eq!(publish["data"]["type"], "typing");
    }

    #[test]
    fn correlation_ids_increment_in_base_36() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(13), "d");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");

        let f = Frames::new("test-token".to_string());
        let first = f.handshake()["id"].as_str().unwrap().to_string();
        let second = f.handshake()["id"].as_str().unwrap().to_string();
        assert_ne!(first, second);
    }

    #[test]
    fn a_typing_frame_from_a_dm_channel_resolves_without_a_thread_key() {
        // The observed DM typing frame names no chat_id; the channel is the
        // only thing that identifies the thread.
        let data = serde_json::json!({
            "type": "typing", "user_id": "20000002", "started": 1785301512997u64
        });
        let Some(Inbound::Typing {
            conversation_id,
            kind,
            user_id,
        }) = interpret("/direct_message/20000001_20000002", &data)
        else {
            panic!("expected typing");
        };
        assert_eq!(conversation_id, "20000001+20000002");
        assert_eq!(kind, ConversationKind::Dm);
        assert_eq!(user_id.as_deref(), Some("20000002"));
    }

    #[test]
    fn burst_floor_suppresses_repeated_notifications_within_30s() {
        let mut times = HashMap::new();
        let t0 = Instant::now();
        let conv = "10000001";

        // First notification for a conversation always passes.
        assert!(burst_floor_pass(&mut times, conv, t0));
        // Immediate repeat is suppressed.
        assert!(!burst_floor_pass(&mut times, conv, t0));
        // Still suppressed at 29 seconds.
        assert!(!burst_floor_pass(
            &mut times,
            conv,
            t0 + Duration::from_secs(29)
        ));
        // Exactly at the floor the gate re-opens.
        let t1 = t0 + NOTIFY_FLOOR;
        assert!(burst_floor_pass(&mut times, conv, t1));
        // Immediately suppressed again.
        assert!(!burst_floor_pass(&mut times, conv, t1));

        // A different conversation has its own independent window.
        let other = "10000002";
        assert!(burst_floor_pass(&mut times, other, t0));
        assert!(!burst_floor_pass(&mut times, other, t0));
    }

    #[test]
    fn backoff_grows_and_stays_inside_the_ceiling() {
        assert!(backoff_delay(0) <= BACKOFF_BASE);
        assert!(backoff_delay(0) >= Duration::from_millis(1500));
        assert!(backoff_delay(4) > backoff_delay(0));
        for attempt in 0..64 {
            assert!(
                backoff_delay(attempt) <= BACKOFF_CEILING,
                "attempt {attempt} exceeded the ceiling"
            );
            // Never zero: a tight reconnect loop is the failure mode this
            // whole function exists to prevent.
            assert!(backoff_delay(attempt) >= Duration::from_millis(1000));
        }
    }

    #[test]
    fn advice_interval_defaults_to_immediate() {
        let v = serde_json::json!({ "advice": { "interval": 250 } });
        assert_eq!(advice_interval(&v), Duration::from_millis(250));
        assert_eq!(advice_interval(&serde_json::json!({})), Duration::ZERO);
    }

    #[test]
    fn auth_failures_are_recognised_but_ordinary_ones_are_not() {
        assert!(looks_like_auth_failure("401:unauthorized"));
        assert!(looks_like_auth_failure("invalid access_token"));
        assert!(!looks_like_auth_failure("503:service unavailable"));
        assert!(!looks_like_auth_failure("unspecified"));
    }

    #[test]
    fn the_error_chain_survives_into_the_log_line() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "certificate rejected")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "io error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        // "io error" alone is what made the v0.1.0 TLS failure unreadable.
        assert_eq!(
            error_chain(&Outer(Inner)),
            "io error <- certificate rejected"
        );
    }

    #[test]
    fn a_realtime_message_lands_in_the_archive_and_reads_back() {
        let mut store = Store::open_in_memory().unwrap();
        let m: Message = serde_json::from_value(serde_json::json!({
            "id": "170000000000000001",
            "group_id": "10000001",
            "user_id": "20000001",
            "sender_id": "20000001",
            "name": "Example Sender",
            "text": "Example message body",
            "created_at": 1785301508
        }))
        .unwrap();

        persist_message(&mut store, "10000001", &m).unwrap();
        let page = store.messages_page("10000001", 10, None).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "170000000000000001");

        // Idempotent: the polling worker re-fetching the same message later
        // must not duplicate the row.
        persist_message(&mut store, "10000001", &m).unwrap();
        assert_eq!(store.message_count("10000001").unwrap(), 1);
    }

    #[test]
    fn a_realtime_delete_event_is_applied_to_the_message_it_targets() {
        let mut store = Store::open_in_memory().unwrap();
        let original: Message = serde_json::from_value(serde_json::json!({
            "id": "170000000000000002",
            "chat_id": "20000001+20000002",
            "sender_id": "20000002",
            "text": "Example direct message",
            "created_at": 1785301509
        }))
        .unwrap();
        persist_message(&mut store, "20000001+20000002", &original).unwrap();

        let tombstone: Message = serde_json::from_value(serde_json::json!({
            "id": "170000000000000003",
            "sender_id": "system",
            "sender_type": "system",
            "text": "This message was deleted",
            "created_at": 1785301510,
            "event": {
                "type": "message.deleted",
                "data": { "message_id": "170000000000000002", "deleted_at": 1785301510 }
            }
        }))
        .unwrap();
        persist_message(&mut store, "20000001+20000002", &tombstone).unwrap();

        let page = store.messages_page("20000001+20000002", 10, None).unwrap();
        let target = page
            .iter()
            .find(|m| m.id == "170000000000000002")
            .expect("the original is still held");
        assert!(target.is_deleted(), "the deletion reached the stored row");
    }

    /// Tauri managed state is `Send + Sync + 'static`, and the handle is held
    /// across awaits by whatever spawns it.
    #[test]
    fn the_handle_can_live_in_managed_state() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<RealtimeHandle>();
    }

    /// The archive guard must never be alive across an `.await`: the future
    /// stops being `Send` and `tauri::async_runtime::spawn` refuses it. The
    /// production path proves this by construction — `spawn` below would not
    /// compile otherwise — so this only pins the store handle itself.
    #[test]
    fn the_shared_store_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SharedStore>();
    }

    #[test]
    fn a_dropped_handle_does_not_panic_the_caller() {
        // Commands issued after the worker stopped are dropped, not unwrapped.
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let handle = RealtimeHandle {
            inner: Arc::new(HandleInner {
                tx,
                connected: Arc::new(AtomicBool::new(false)),
            }),
        };
        handle.watch_conversation("10000001", ConversationKind::Group);
        handle.unwatch_conversation("10000001");
        handle.send_typing("10000001", ConversationKind::Group);
        assert!(!handle.is_connected());
    }
}
