//! System tray, close-to-tray, and new-message toasts.
//!
//! The tray is the app's persistent presence: a status line naming the signed-in
//! account and whether we are reading live or from the archive, a Show entry, a
//! sync-status window, the two user-facing toggles (notifications,
//! start-with-Windows), an offline simulation for testing, an updater hook, and
//! Quit.
//!
//! ## Close-to-tray
//! The window's X does **not** quit. `CloseRequested` on the main window is
//! intercepted, prevented, and turned into a `hide()`, so the background sync
//! worker keeps filling the archive while the app is "closed" — which is the
//! whole point of having an archive. `Quit` is therefore the only real exit path,
//! and it goes through [`allow_exit`] to disarm the interception before exiting;
//! without that flag the shutdown would simply re-hide the window forever.
//!
//! ## The unread badge
//! Rather than bundle a second asset, the unread icon is composited at runtime:
//! the bundled PNG is decoded, downscaled to tray size, and a red disc bearing
//! the count is drawn into the bottom-right corner (see `make_badged_icon`).
//! Variants are cached per label so a sync tick that does not change the count
//! costs nothing.
//!
//! Every mutator here is a no-op until [`init`] has run, and safe to call from
//! any thread: Tauri's tray/menu types are handles that proxy to the main thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, Wry,
};
use tauri_plugin_notification::NotificationExt;

use crate::commands::SharedStore;
use crate::connectivity;

/// Label of the window declared in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";
const TRAY_ID: &str = "main";
const APP_NAME: &str = "GroupMe";

/// The sync-status window, built on demand by [`open_status_window`]. Its page
/// reads the archive through the existing `archive_stats` command.
const STATUS_WINDOW: &str = "status";
const STATUS_PAGE: &str = "status.html";

/// Emitted when the user picks "Check for updates…"; the updater module owns the
/// rest. Nothing about updating happens in this file.
pub const EVENT_CHECK_UPDATES: &str = "app://check-updates";
/// Emitted with `{"enabled": bool}` when the user flips "Start with Windows".
/// Whoever registers autostart should call [`set_autostart_checked`] with the
/// real outcome so a failed registration does not leave the item lying.
pub const EVENT_TOGGLE_AUTOSTART: &str = "app://toggle-autostart";
/// Emitted with `{"forced": bool}` when the user flips "Simulate offline".
///
/// `connectivity::set_forced_offline` has already been called by the time this
/// lands — the payload is informational. Whoever owns the monitor should answer
/// it with `ConnectivityMonitor::refresh_override()` so the change takes effect
/// now instead of at the next probe tick.
pub const EVENT_CONNECTIVITY_OVERRIDE: &str = "app://connectivity-override";

/// Archive `meta` key backing the notification toggle.
const META_NOTIFY: &str = "notify_on_message";
/// Archive `meta` key written by `lib.rs::verify_account`.
const META_ACCOUNT_NAME: &str = "account_name";

/// Longest notification body we will show. Windows truncates toasts itself, but
/// it does so mid-word and without an ellipsis.
const MAX_BODY_CHARS: usize = 120;

/// Live handles we mutate after the tray is built. Tauri's `TrayIcon`/`MenuItem`
/// are thread-safe handles that proxy mutations to the main thread, so holding
/// them in a process-global is sound and saves plumbing a menu reference through
/// the sync engine.
struct TrayHandles {
    tray: TrayIcon<Wry>,
    /// The whole tray menu — also popped in-window by `show_app_menu`.
    menu: Menu<Wry>,
    status: MenuItem<Wry>,
    notify: CheckMenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    simulate: CheckMenuItem<Wry>,
    /// Base tray icon; `None` only if the bundled PNG and the default window
    /// icon are both unavailable.
    icon_base: Option<Image<'static>>,
    /// Composited badge variants, keyed by the label drawn on them.
    icon_badged: HashMap<String, Image<'static>>,
    /// Badge currently on screen, so an unchanged count skips the OS call.
    last_badge: Option<String>,
    account: Option<String>,
    connectivity: String,
    unread: usize,
}
static HANDLES: OnceLock<Mutex<TrayHandles>> = OnceLock::new();

/// Disarms close-to-tray. Set only by [`allow_exit`].
static EXITING: AtomicBool = AtomicBool::new(false);
/// Mirrors the persisted "Notify on new messages" setting.
static NOTIFY: AtomicBool = AtomicBool::new(true);
/// Last known autostart state. Starts `false` because this module cannot query
/// the OS — the owner of `app://toggle-autostart` reports the truth back.
static AUTOSTART: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Called once from `lib.rs` setup(), after the main window exists and the store
/// has been `manage`d.
pub fn init(app: &tauri::AppHandle) -> tauri::Result<()> {
    NOTIFY.store(
        read_meta(app, META_NOTIFY).map_or(true, |v| v != "0"),
        Ordering::Relaxed,
    );
    let account = read_meta(app, META_ACCOUNT_NAME);
    build_tray(app, account)?;
    hook_close_to_tray(app);
    Ok(())
}

/// Update the tray tooltip + unread badge. The handle is unused — it is taken so
/// every tray setter has one call shape at the call site.
pub fn set_unread(_app: &tauri::AppHandle, count: usize) {
    let Some(lock) = HANDLES.get() else { return };
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    h.unread = count;
    let tip = tooltip_for(count, &h.connectivity);
    let _ = h.tray.set_tooltip(Some(tip.as_str()));
    apply_icon(&mut h);
}

/// Reflect connectivity in the status line and the tooltip, so the user can tell
/// at a glance whether they are reading live or from the archive.
/// `state` is `"online"` | `"degraded"` | `"offline"` — the lowercase serde names
/// of `connectivity::Connectivity`.
pub fn set_connectivity(_app: &tauri::AppHandle, state: &str) {
    let Some(lock) = HANDLES.get() else { return };
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    h.connectivity = state.to_string();
    let label = status_label_for(h.account.as_deref(), state, connectivity::forced_offline());
    let _ = h.status.set_text(label.as_str());
    let tip = tooltip_for(h.unread, state);
    let _ = h.tray.set_tooltip(Some(tip.as_str()));
}

/// Name the status line shows, and the name used to recognise the user's own
/// messages. Call once sync has identified the signed-in account.
pub fn set_account(name: Option<&str>) {
    let Some(lock) = HANDLES.get() else { return };
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    h.account = name.map(str::to_string);
    let label = status_label_for(
        h.account.as_deref(),
        &h.connectivity,
        connectivity::forced_offline(),
    );
    let _ = h.status.set_text(label.as_str());
}

/// Opens (or focuses, if already open) the sync status window.
pub fn open_status_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(STATUS_WINDOW) {
        // Reused rather than rebuilt: building a second window with the same
        // label fails, and the user asked to see it, not to be told why not.
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        app,
        STATUS_WINDOW,
        tauri::WebviewUrl::App(STATUS_PAGE.into()),
    )
    .title("GroupMe — Sync status")
    .inner_size(420.0, 380.0)
    .resizable(false)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .center()
    .build()?;

    Ok(())
}

const UNBLOCK_WINDOW: &str = "filter-unblock";
const UNBLOCK_PAGE: &str = "unblock.html";

/// Opens a small window that fires a request to `api.groupme.com` **from inside
/// the app**, so a content filter sees the request coming from this app rather
/// than a browser — the distinction that matters, since the filter can allow the
/// browser while still blocking the app's own traffic.
///
/// A background HTTP call is invisible to the user and to the filter's
/// interactive allow flow; putting the same request behind a button, in a real
/// window, is what lets the filter surface it and lets the user approve it. The
/// app never touches any PIN or the filter's settings — it only issues the
/// request; the block, the prompt and the allow decision are entirely the
/// filter's, operated by the user.
pub fn open_filter_unblock(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(UNBLOCK_WINDOW) {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        app,
        UNBLOCK_WINDOW,
        tauri::WebviewUrl::App(UNBLOCK_PAGE.into()),
    )
    .title("Allow GroupMe API through your content filter")
    .inner_size(600.0, 480.0)
    .center()
    .build()?;

    Ok(())
}

/// Fire an OS notification for a new message. Silently drops the toast when
/// notifications are off, when the conversation is locally muted, when the
/// message is the user's own, or when the main window is focused.
///
/// `conversation_id` is the archive's conversation key (group id, or a DM's
/// stored key) — used only to consult the local mute flag; `conversation` is the
/// display name shown in the toast.
pub fn notify_message(
    app: &tauri::AppHandle,
    conversation_id: &str,
    sender: &str,
    body: &str,
    conversation: &str,
) {
    if !NOTIFY.load(Ordering::Relaxed) {
        return;
    }
    // A muted conversation raises no toast. Checked here so the suppression holds
    // however the notification path is wired — the flag is local, set by
    // `client_set_mute`.
    if conversation_muted(app, conversation_id) {
        return;
    }
    if is_self_message(sender, account_name().as_deref()) {
        return;
    }
    // Toasting a message the user is watching arrive is pure noise.
    if main_window_focused(app) {
        return;
    }
    let result = app
        .notification()
        .builder()
        .title(notification_title(sender, conversation))
        .body(notification_body(body))
        .show();
    if let Err(e) = result {
        log::warn!("tray: could not show notification: {e}");
    }
}

/// Whether new-message toasts are currently enabled.
pub fn notifications_enabled() -> bool {
    NOTIFY.load(Ordering::Relaxed)
}

/// Report the real autostart state back into the check item, after whoever
/// handles [`EVENT_TOGGLE_AUTOSTART`] has actually registered or unregistered.
pub fn set_autostart_checked(enabled: bool) {
    AUTOSTART.store(enabled, Ordering::Relaxed);
    if let Some(lock) = HANDLES.get() {
        let h = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = h.autostart.set_checked(enabled);
    }
}

/// Let the next close actually close. Without this, `app.exit()` races the
/// close-to-tray hook and the window is merely re-hidden.
pub fn allow_exit() {
    EXITING.store(true, Ordering::SeqCst);
}

/// Pop the tray menu at the cursor — the in-window right-click path.
#[tauri::command]
pub fn show_app_menu(app: tauri::AppHandle) {
    show_app_menu_inner(&app);
}

pub fn show_app_menu_inner(app: &tauri::AppHandle) {
    // Anchor on a VISIBLE window: a hidden owner (main closed to tray) makes
    // Windows dismiss the popup the instant it appears.
    let Some(window) = app
        .get_webview_window(MAIN_WINDOW)
        .filter(|w| w.is_visible().unwrap_or(false))
    else {
        return;
    };
    // Clone the (cheap) menu handle and DROP the guard before popping: popup_menu
    // runs a blocking modal message loop that still pumps main-thread tasks, so a
    // sync tick landing mid-menu would re-enter set_unread and self-deadlock on
    // this same mutex.
    let menu = HANDLES
        .get()
        .and_then(|l| l.lock().ok().map(|h| h.menu.clone()));
    if let Some(menu) = menu {
        let _ = window.popup_menu(&menu);
    }
}

// ---------------------------------------------------------------------------
// Tray construction
// ---------------------------------------------------------------------------

fn build_tray(app: &tauri::AppHandle, account: Option<String>) -> tauri::Result<()> {
    let status_i = MenuItem::with_id(
        app,
        "status",
        status_label(account.as_deref(), ""),
        false,
        None::<&str>,
    )?;
    let show_i = MenuItem::with_id(app, "show", "Show GroupMe", true, None::<&str>)?;
    let status_window_i =
        MenuItem::with_id(app, "sync_status", "Sync status…", true, None::<&str>)?;
    let unblock_i = MenuItem::with_id(
        app,
        "filter_unblock",
        "Allow API through content filter…",
        true,
        None::<&str>,
    )?;
    let updates_i = MenuItem::with_id(
        app,
        "check_updates",
        "Check for updates…",
        true,
        None::<&str>,
    )?;
    let notify_i = CheckMenuItem::with_id(
        app,
        "notify_toggle",
        "Notify on new messages",
        true,
        NOTIFY.load(Ordering::Relaxed),
        None::<&str>,
    )?;
    let autostart_i = CheckMenuItem::with_id(
        app,
        "autostart_toggle",
        "Start with Windows",
        true,
        AUTOSTART.load(Ordering::Relaxed),
        None::<&str>,
    )?;
    // Always starts unchecked: the override is never persisted, so a fresh
    // process is never simulating.
    let simulate_i = CheckMenuItem::with_id(
        app,
        "simulate_offline",
        "Simulate offline (testing)",
        true,
        connectivity::forced_offline(),
        None::<&str>,
    )?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &status_i,
            &sep1,
            &show_i,
            &status_window_i,
            &unblock_i,
            &updates_i,
            &sep2,
            &notify_i,
            &autostart_i,
            &simulate_i,
            &sep3,
            &quit_i,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(APP_NAME)
        .menu(&menu)
        // Left click restores the window (handled below); the menu is right-click
        // only, matching the Windows tray convention.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_and_focus(app),
            "sync_status" => {
                if let Err(e) = open_status_window(app) {
                    log::warn!("tray: could not open the sync status window: {e}");
                }
            }
            "filter_unblock" => {
                if let Err(e) = open_filter_unblock(app) {
                    log::warn!("tray: could not open the filter-unblock window: {e}");
                }
            }
            "check_updates" => {
                if let Err(e) = app.emit(EVENT_CHECK_UPDATES, ()) {
                    log::warn!("tray: could not emit {EVENT_CHECK_UPDATES}: {e}");
                }
            }
            "notify_toggle" => toggle_notify(app),
            "autostart_toggle" => toggle_autostart(app),
            "simulate_offline" => toggle_simulate_offline(app),
            "quit" => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => show_and_focus(tray.app_handle()),
            _ => {}
        });

    let icon_base = build_base_icon(app);
    if let Some(icon) = icon_base.clone() {
        builder = builder.icon(icon);
    }

    let tray = builder.build(app)?;
    let _ = HANDLES.set(Mutex::new(TrayHandles {
        tray,
        menu,
        status: status_i,
        notify: notify_i,
        autostart: autostart_i,
        simulate: simulate_i,
        icon_base,
        icon_badged: HashMap::new(),
        last_badge: None,
        account,
        connectivity: String::new(),
        unread: 0,
    }));
    Ok(())
}

/// Intercept the main window's close: hide instead of destroy, so the sync
/// worker keeps the archive current while the app is "closed".
fn hook_close_to_tray(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        log::warn!("tray: no `{MAIN_WINDOW}` window — close-to-tray not installed");
        return;
    };
    let win = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            if EXITING.load(Ordering::SeqCst) {
                return;
            }
            api.prevent_close();
            let _ = win.hide();
        }
    });
}

fn quit(app: &tauri::AppHandle) {
    allow_exit();
    // Destroy the webviews FIRST so WebView2 releases its profile locks cleanly;
    // a bare process exit leaves them held and a fast relaunch hangs on them.
    for (_, win) in app.webview_windows() {
        let _ = win.destroy();
    }
    app.exit(0);
}

/// Bring the main window back from either hidden (close-to-tray) or minimized.
///
/// Order is load-bearing. `show()` maps to `ShowWindow(SW_SHOW)`, which on a
/// **minimized** window makes it "visible" while leaving it iconic — so calling
/// it first and then `unminimize()` restored nothing, and every route back to
/// the app (tray icon, tray menu, relaunch) silently did nothing. Restore
/// first, then show, then focus.
pub fn show_and_focus(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        log::warn!("tray: no `{MAIN_WINDOW}` window to show");
        return;
    };

    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();

    // Windows refuses SetForegroundWindow to a process that does not own the
    // foreground, so `set_focus` can restore the window *behind* whatever the
    // user is looking at — indistinguishable from nothing happening. Briefly
    // asserting always-on-top is the standard way to raise it without that
    // restriction; it is toggled straight back so the window does not actually
    // stay pinned.
    #[cfg(windows)]
    {
        let pinned = window.is_always_on_top().unwrap_or(false);
        if !pinned {
            let _ = window.set_always_on_top(true);
            let _ = window.set_always_on_top(false);
        }
    }
}

// ---------------------------------------------------------------------------
// Toggles
// ---------------------------------------------------------------------------

/// Flip the persisted setting, then mirror it into the check item. Deriving the
/// new value from the stored one rather than from the item's own (platform-
/// dependent) auto-toggle keeps a single source of truth.
fn toggle_notify(app: &tauri::AppHandle) {
    let desired = !NOTIFY.load(Ordering::Relaxed);
    NOTIFY.store(desired, Ordering::Relaxed);
    write_meta(app, META_NOTIFY, if desired { "1" } else { "0" });
    if let Some(lock) = HANDLES.get() {
        let h = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = h.notify.set_checked(desired);
    }
}

/// Request an autostart change. This module deliberately does not touch the
/// registry or the autostart plugin; it announces the intent and shows the
/// optimistic state until corrected by [`set_autostart_checked`].
fn toggle_autostart(app: &tauri::AppHandle) {
    let desired = !AUTOSTART.load(Ordering::Relaxed);
    AUTOSTART.store(desired, Ordering::Relaxed);
    if let Some(lock) = HANDLES.get() {
        let h = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = h.autostart.set_checked(desired);
    }
    if let Err(e) = app.emit(
        EVENT_TOGGLE_AUTOSTART,
        serde_json::json!({ "enabled": desired }),
    ) {
        log::warn!("tray: could not emit {EVENT_TOGGLE_AUTOSTART}: {e}");
    }
}

/// Pin connectivity to Offline so the archive reader can be exercised without
/// unplugging anything.
///
/// Deliberately NOT written to the archive `meta` table, and deliberately not
/// mirrored in a local static either — `connectivity` owns the flag, and a
/// simulation that survived a restart would be indistinguishable from a real,
/// permanent outage to whoever inherits the machine.
fn toggle_simulate_offline(app: &tauri::AppHandle) {
    let desired = !connectivity::forced_offline();
    connectivity::set_forced_offline(desired);
    if let Some(lock) = HANDLES.get() {
        let h = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = h.simulate.set_checked(desired);
        // Redraw now rather than waiting for the monitor's own transition to
        // come back through `set_connectivity`. Switching the simulation off
        // falls back to the last state the monitor reported, which the re-probe
        // triggered by the event below corrects within a tick.
        let shown = if desired {
            "offline"
        } else {
            h.connectivity.as_str()
        };
        let label = status_label_for(h.account.as_deref(), shown, desired);
        let _ = h.status.set_text(label.as_str());
    }
    if let Err(e) = app.emit(
        EVENT_CONNECTIVITY_OVERRIDE,
        serde_json::json!({ "forced": desired }),
    ) {
        log::warn!("tray: could not emit {EVENT_CONNECTIVITY_OVERRIDE}: {e}");
    }
}

// ---------------------------------------------------------------------------
// Archive `meta` persistence
// ---------------------------------------------------------------------------

/// `try_lock` rather than `lock`: this runs on the main thread inside `setup()`,
/// which is also the UI thread. The store is a blocking mutex, so a contended
/// `lock()` here would freeze the window rather than merely wait. Nothing else
/// holds the store this early, so the `None` fallback is theoretical — and the
/// caller's default (notifications on) is the right answer anyway.
fn read_meta(app: &tauri::AppHandle, key: &str) -> Option<String> {
    let store = app.try_state::<SharedStore>()?;
    let guard = store.try_lock().ok()?;
    match guard.get_meta(key) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("tray: reading meta `{key}`: {e}");
            None
        }
    }
}

/// Fire-and-forget on the blocking pool. Called from menu-event handlers on the
/// UI thread, where a synchronous SQLite write would show up as a stalled menu.
fn write_meta(app: &tauri::AppHandle, key: &'static str, value: &str) {
    let Some(store) = app.try_state::<SharedStore>() else {
        log::warn!("tray: no archive store — `{key}` not persisted");
        return;
    };
    let store = store.inner().clone();
    let value = value.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = guard.set_meta(key, &value) {
            log::warn!("tray: persisting meta `{key}`: {e}");
        }
    });
}

// ---------------------------------------------------------------------------
// Text formatting (pure)
// ---------------------------------------------------------------------------

fn connectivity_text(state: &str) -> &'static str {
    match state {
        "online" => "Online",
        "degraded" => "Unstable connection",
        "offline" => "Offline — reading archive",
        _ => "Connecting…",
    }
}

fn status_label(account: Option<&str>, state: &str) -> String {
    match account.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => format!("{name} — {}", connectivity_text(state)),
        None => connectivity_text(state).to_string(),
    }
}

/// [`status_label`], marked when the outage is simulated rather than real.
/// Without the marker the only way to tell the two apart is to remember flipping
/// a checkbox, which nobody does a week later.
fn status_label_for(account: Option<&str>, state: &str, simulated: bool) -> String {
    let label = status_label(account, state);
    if simulated {
        format!("{label} (simulated)")
    } else {
        label
    }
}

fn tooltip_for(unread: usize, state: &str) -> String {
    let mut tip = String::from(APP_NAME);
    if unread > 0 {
        tip.push_str(&format!(" — {unread} unread"));
    }
    match state {
        "degraded" => tip.push_str(" — unstable connection"),
        "offline" => tip.push_str(" — offline, reading archive"),
        _ => {}
    }
    tip
}

fn notification_title(sender: &str, conversation: &str) -> String {
    let sender = sender.trim();
    let conversation = conversation.trim();
    match (sender.is_empty(), conversation.is_empty()) {
        (true, true) => APP_NAME.to_string(),
        (true, false) => conversation.to_string(),
        (false, true) => sender.to_string(),
        // In a DM the conversation IS the other person; printing both stutters.
        _ if sender == conversation => sender.to_string(),
        _ => format!("{sender} — {conversation}"),
    }
}

fn notification_body(body: &str) -> String {
    // A toast renders as one collapsed block, so flattening first is what makes
    // the character budget below mean anything.
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "New message".to_string();
    }
    if flat.chars().count() <= MAX_BODY_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_BODY_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Backstop only. GroupMe display names are not unique, so the caller should
/// skip messages whose `user_id` equals the signed-in account's before ever
/// reaching here — see `meta.account_user_id`.
fn is_self_message(sender: &str, account: Option<&str>) -> bool {
    let Some(me) = account.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    sender.trim().eq_ignore_ascii_case(me)
}

fn account_name() -> Option<String> {
    let lock = HANDLES.get()?;
    let h = lock.lock().unwrap_or_else(|e| e.into_inner());
    h.account.clone()
}

/// Whether a conversation is locally muted. Fails open: if the store is
/// unavailable or the read errors, the notification is allowed rather than
/// silently swallowed. Not on the UI thread (the notify path runs off it), so a
/// brief blocking lock for one indexed lookup is fine.
fn conversation_muted(app: &tauri::AppHandle, conversation_id: &str) -> bool {
    let Some(store) = app.try_state::<SharedStore>() else {
        return false;
    };
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.is_muted(conversation_id).unwrap_or(false)
}

fn main_window_focused(app: &tauri::AppHandle) -> bool {
    let Some(w) = app.get_webview_window(MAIN_WINDOW) else {
        return false;
    };
    w.is_visible().unwrap_or(false)
        && !w.is_minimized().unwrap_or(false)
        && w.is_focused().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Icon compositing
// ---------------------------------------------------------------------------

/// The bundled 128px icon, decoded and Lanczos-resized to tray size at startup.
/// Handing the tray the full-size window icon leaves the downscale to Windows,
/// which renders it blurry.
const TRAY_SOURCE: &[u8] = include_bytes!("../icons/128x128.png");
const TRAY_SIZE: u32 = 32;

/// Glyph cell grid for the badge count. One blank column separates glyphs.
const GLYPH_W: i32 = 3;
const GLYPH_H: i32 = 5;

fn build_base_icon(app: &tauri::AppHandle) -> Option<Image<'static>> {
    image::load_from_memory(TRAY_SOURCE)
        .ok()
        .map(|img| {
            let small = img
                .resize_exact(TRAY_SIZE, TRAY_SIZE, image::imageops::FilterType::Lanczos3)
                .into_rgba8();
            Image::new_owned(small.into_raw(), TRAY_SIZE, TRAY_SIZE)
        })
        .or_else(|| {
            app.default_window_icon()
                .map(|b| Image::new_owned(b.rgba().to_vec(), b.width(), b.height()))
        })
}

fn badge_label(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=9 => Some(count.to_string()),
        _ => Some("9+".to_string()),
    }
}

/// Swap the tray icon to match the unread count, skipping the OS call when the
/// badge is unchanged.
fn apply_icon(h: &mut TrayHandles) {
    let label = badge_label(h.unread);
    if h.last_badge == label {
        return;
    }
    let Some(base) = h.icon_base.clone() else {
        // No artwork at all: record the state so we stop retrying.
        h.last_badge = label;
        return;
    };
    let icon = match &label {
        None => base,
        Some(text) => h
            .icon_badged
            .entry(text.clone())
            .or_insert_with(|| make_badged_icon(&base, text).unwrap_or_else(|| base.clone()))
            .clone(),
    };
    if h.tray.set_icon(Some(icon)).is_ok() {
        h.last_badge = label;
    } else {
        log::warn!("tray: could not apply the unread badge");
    }
}

/// Composite a red count badge into the bottom-right corner of the app icon.
/// Operates on the raw RGBA the icon already carries — no second asset, and no
/// font: the digits come from the 3x5 bitmap in `glyph`.
fn make_badged_icon(base: &Image, label: &str) -> Option<Image<'static>> {
    let (w, h) = (base.width(), base.height());
    let mut img = image::RgbaImage::from_raw(w, h, base.rgba().to_vec())?;

    let dim = w.min(h) as f32;
    let radius = dim * 0.32;
    let margin = dim * 0.04;
    let cx = w as f32 - radius - margin;
    let cy = h as f32 - radius - margin;

    let halo = [255u8, 255, 255, 235]; // separates the badge from busy artwork
    let disc = [226u8, 47, 41, 255];
    let ink = [255u8, 255, 255, 255];

    let outer = radius + dim * 0.045;
    let x0 = (cx - outer - 1.0).floor().max(0.0) as u32;
    let y0 = (cy - outer - 1.0).floor().max(0.0) as u32;
    let x1 = ((cx + outer + 1.0).ceil().max(0.0) as u32).min(w);
    let y1 = ((cy + outer + 1.0).ceil().max(0.0) as u32).min(h);

    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let px = img.get_pixel_mut(x, y);
            blend(px, halo, (outer - dist + 0.5).clamp(0.0, 1.0));
            blend(px, disc, (radius - dist + 0.5).clamp(0.0, 1.0));
        }
    }

    // Fit the label to the square inscribed in the disc, then rasterise the cell
    // grid by inverse mapping with 2x2 supersampling so edges are not jagged.
    let chars = label.chars().count() as i32;
    if chars > 0 {
        let cols = chars * GLYPH_W + (chars - 1);
        let avail = radius * 1.3;
        let scale = (avail / cols as f32).min(avail / GLYPH_H as f32);
        let ox = cx - cols as f32 * scale / 2.0;
        let oy = cy - GLYPH_H as f32 * scale / 2.0;

        let tx0 = ox.floor().max(0.0) as u32;
        let ty0 = oy.floor().max(0.0) as u32;
        let tx1 = (((ox + cols as f32 * scale).ceil().max(0.0)) as u32 + 1).min(w);
        let ty1 = (((oy + GLYPH_H as f32 * scale).ceil().max(0.0)) as u32 + 1).min(h);

        for y in ty0..ty1 {
            for x in tx0..tx1 {
                let mut hits = 0u8;
                for sy in 0..2 {
                    for sx in 0..2 {
                        let px = x as f32 + 0.25 + sx as f32 * 0.5;
                        let py = y as f32 + 0.25 + sy as f32 * 0.5;
                        let col = ((px - ox) / scale).floor() as i32;
                        let row = ((py - oy) / scale).floor() as i32;
                        if text_cell_on(label, col, row) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    blend(img.get_pixel_mut(x, y), ink, hits as f32 / 4.0);
                }
            }
        }
    }

    Some(Image::new_owned(img.into_raw(), w, h))
}

/// Is cell `(col, row)` of the rendered `label` inked?
fn text_cell_on(label: &str, col: i32, row: i32) -> bool {
    if !(0..GLYPH_H).contains(&row) || col < 0 {
        return false;
    }
    let index = col / (GLYPH_W + 1);
    let within = col % (GLYPH_W + 1);
    if within >= GLYPH_W {
        return false; // gap column between glyphs
    }
    let Some(c) = label.chars().nth(index as usize) else {
        return false;
    };
    let Some(g) = glyph(c) else { return false };
    g[row as usize] & (1 << (GLYPH_W - 1 - within)) != 0
}

/// 3x5 bitmap font, one row per scanline; bit 2 is the leftmost pixel.
fn glyph(c: char) -> Option<[u8; GLYPH_H as usize]> {
    Some(match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        _ => return None,
    })
}

/// Source-over alpha blend of `src` (premultiplied by `cov`) onto `dst`.
fn blend(dst: &mut image::Rgba<u8>, src: [u8; 4], cov: f32) {
    let a = (src[3] as f32 / 255.0) * cov.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    // Colour channels only; alpha is composited separately below.
    for (channel, &s) in dst.0.iter_mut().zip(src.iter()).take(3) {
        *channel = (s as f32 * a + *channel as f32 * (1.0 - a))
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    let da = dst.0[3] as f32 / 255.0;
    dst.0[3] = ((a + da * (1.0 - a)) * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_names_the_app_and_only_counts_when_there_is_something_to_count() {
        assert_eq!(tooltip_for(0, "online"), "GroupMe");
        assert_eq!(tooltip_for(1, "online"), "GroupMe — 1 unread");
        assert_eq!(tooltip_for(42, "online"), "GroupMe — 42 unread");
    }

    #[test]
    fn tooltip_says_when_we_are_not_reading_live() {
        assert_eq!(
            tooltip_for(0, "offline"),
            "GroupMe — offline, reading archive"
        );
        assert_eq!(
            tooltip_for(3, "offline"),
            "GroupMe — 3 unread — offline, reading archive"
        );
        assert_eq!(tooltip_for(0, "degraded"), "GroupMe — unstable connection");
        // An unrecognised state must not silently claim we are online.
        assert_eq!(tooltip_for(0, ""), "GroupMe");
    }

    #[test]
    fn status_line_pairs_the_account_with_connectivity() {
        assert_eq!(
            status_label(Some("Example Sender"), "online"),
            "Example Sender — Online"
        );
        assert_eq!(
            status_label(Some("Example Sender"), "offline"),
            "Example Sender — Offline — reading archive"
        );
        assert_eq!(status_label(None, "online"), "Online");
        assert_eq!(status_label(Some("   "), "online"), "Online");
        // Before the first probe lands.
        assert_eq!(status_label(None, ""), "Connecting…");
    }

    #[test]
    fn a_simulated_outage_is_labelled_as_one() {
        assert_eq!(
            status_label_for(None, "offline", true),
            "Offline — reading archive (simulated)"
        );
        assert_eq!(
            status_label_for(Some("Example Sender"), "offline", true),
            "Example Sender — Offline — reading archive (simulated)"
        );
        // Unsimulated is byte-for-byte what it was before the toggle existed.
        for state in ["online", "degraded", "offline", ""] {
            assert_eq!(
                status_label_for(Some("Example Sender"), state, false),
                status_label(Some("Example Sender"), state)
            );
        }
    }

    /// A simulation that outlived the process would present as a permanent,
    /// inexplicable outage — and the tray checkbox is the last place anyone
    /// would think to look. Enforced here rather than left to review.
    #[test]
    fn the_offline_simulation_is_never_persisted() {
        let production = include_str!("tray.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let body = production
            .split("fn toggle_simulate_offline")
            .nth(1)
            .expect("toggle_simulate_offline must exist");
        // Functions end at a `}` in column 0.
        let body = &body[..body.find("\n}").unwrap_or(body.len())];
        assert!(
            !body.contains("write_meta"),
            "the offline simulation must not reach the archive `meta` table"
        );
    }

    #[test]
    fn badge_label_saturates_rather_than_overflowing_the_disc() {
        assert_eq!(badge_label(0), None);
        assert_eq!(badge_label(1).as_deref(), Some("1"));
        assert_eq!(badge_label(9).as_deref(), Some("9"));
        assert_eq!(badge_label(10).as_deref(), Some("9+"));
        assert_eq!(badge_label(9999).as_deref(), Some("9+"));
    }

    #[test]
    fn every_badge_label_is_actually_renderable() {
        for n in [0usize, 1, 5, 9, 10, 250] {
            let Some(label) = badge_label(n) else {
                continue;
            };
            for c in label.chars() {
                assert!(glyph(c).is_some(), "no glyph for {c:?} in badge {label:?}");
            }
        }
    }

    #[test]
    fn glyph_cells_map_left_to_right_with_a_gap_between_characters() {
        // "1" is `0b010` on its top row: only the middle column is inked.
        assert!(!text_cell_on("1", 0, 0));
        assert!(text_cell_on("1", 1, 0));
        assert!(!text_cell_on("1", 2, 0));
        // Column 3 is the inter-glyph gap; column 4 starts the '+'.
        assert!(!text_cell_on("9+", 3, 2));
        assert!(text_cell_on("9+", 4, 2));
        // Out of bounds in every direction.
        assert!(!text_cell_on("1", -1, 0));
        assert!(!text_cell_on("1", 0, -1));
        assert!(!text_cell_on("1", 0, GLYPH_H));
        assert!(!text_cell_on("1", 99, 0));
    }

    #[test]
    fn notification_body_is_flattened_and_truncated() {
        assert_eq!(notification_body("hello"), "hello");
        assert_eq!(notification_body("two\nlines   here"), "two lines here");
        // Attachment-only messages carry no text.
        assert_eq!(notification_body(""), "New message");
        assert_eq!(notification_body("   \n\t "), "New message");

        let long = "a".repeat(500);
        let out = notification_body(&long);
        assert_eq!(
            out.chars().count(),
            MAX_BODY_CHARS + 1,
            "kept 120 chars plus the ellipsis"
        );
        assert!(out.ends_with('…'));

        let exact = "b".repeat(MAX_BODY_CHARS);
        assert_eq!(
            notification_body(&exact),
            exact,
            "no ellipsis at the boundary"
        );
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // Byte-slicing this would panic; char-counting must not.
        let body = "é".repeat(400);
        let out = notification_body(&body);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), MAX_BODY_CHARS + 1);
    }

    #[test]
    fn a_users_own_message_never_notifies_them() {
        assert!(is_self_message("Example Sender", Some("Example Sender")));
        assert!(is_self_message("  example sender ", Some("Example Sender")));
        assert!(!is_self_message("Someone Else", Some("Example Sender")));
    }

    #[test]
    fn self_filter_stays_inert_when_the_account_is_unknown() {
        // Suppressing every toast because we have not identified the account yet
        // would be worse than the occasional echo.
        assert!(!is_self_message("Example Sender", None));
        assert!(!is_self_message("", None));
        assert!(!is_self_message("", Some("")));
        assert!(!is_self_message("", Some("   ")));
    }

    #[test]
    fn notification_title_identifies_sender_and_conversation() {
        assert_eq!(
            notification_title("Ada", "Study Group"),
            "Ada — Study Group"
        );
        // A DM: the conversation is the sender.
        assert_eq!(notification_title("Ada", "Ada"), "Ada");
        assert_eq!(notification_title("Ada", ""), "Ada");
        assert_eq!(notification_title("", "Study Group"), "Study Group");
        assert_eq!(notification_title("", ""), APP_NAME);
    }

    #[test]
    fn connectivity_states_match_the_serde_names_emitted_by_the_monitor() {
        assert_eq!(connectivity_text("online"), "Online");
        assert_eq!(connectivity_text("degraded"), "Unstable connection");
        assert_eq!(connectivity_text("offline"), "Offline — reading archive");
        assert_eq!(connectivity_text("nonsense"), "Connecting…");
    }
}
