//! Signed auto-update: check a minisign-signed manifest, download in the
//! background, and install only on an explicit restart or as the app quits.
//!
//! ## Why the install is staged rather than immediate
//!
//! This is a messenger. An updater that swaps the binary the moment a new
//! version lands would close the window on somebody mid-conversation, and would
//! do it at a random moment they cannot predict or prevent — the single most
//! hostile thing a background task can do to a chat app. The archive sync worker
//! is also usually mid-flight, and killing it between an API page and its SQLite
//! write is how a half-written archive happens.
//!
//! So a found update is downloaded and *staged*: the installer bytes are held in
//! memory (`STAGED`) and nothing on disk changes. They are handed to the
//! installer only when
//!
//!   * the user presses "Restart & Install" ([`updater_restart`]), or
//!   * the app is quitting anyway ([`install_staged_on_exit`]),
//!
//! so the new version is simply what launches next time. Holding ~10 MB resident
//! for the rest of the session is the price; it buys an update that never
//! interrupts anyone.
//!
//! ## Events
//!
//! Phases go out with `emit_to(WINDOW_LABEL, …)` rather than a broadcast, so in
//! the ordinary case only the dialog is woken and `web.groupme.com` — third
//! party code sitting in the `main` window — is not handed our update state.
//!
//! That is delivery scoping, not a security boundary, and it is worth being
//! precise about which: Tauri delivers to any listener registered with the
//! default `Any` target *regardless* of the emit filter
//! (`event::listener::match_any_or_filter`), and the remote window holds
//! `core:event:allow-emit`, so it can equally broadcast a forged
//! `updater://ready` into this dialog. Nothing downstream trusts an event to
//! mean anything: every install and download decision is re-checked against
//! `STAGED` here in Rust, the installer bytes are minisign-verified by the
//! plugin before they are ever run, and the dialog writes every dynamic value
//! with `textContent`. A forged phase can mislead the user about a version
//! number, and that is the whole of it.
//!
//! | event                         | payload                                                  |
//! |-------------------------------|----------------------------------------------------------|
//! | `updater://checking`          | `{ userInitiated, currentVersion }`                       |
//! | `updater://up-to-date`        | `{ userInitiated, currentVersion }`                       |
//! | `updater://update-available`  | `{ userInitiated, currentVersion, version, notes, releaseUrl }` |
//! | `updater://downloading`       | `{ userInitiated, currentVersion, version, notes }`             |
//! | `updater://download-progress` | `{ downloaded, total, percent }` (`total`/`percent` null)       |
//! | `updater://ready`             | `{ userInitiated, currentVersion, version, notes }`             |
//! | `updater://error`             | `{ userInitiated, message, downloadBlocked?, releaseUrl? }`     |
//!
//! Phases (not progress ticks) are cached in `LAST_STATUS` and served by
//! [`updater_last_status`], so a dialog that opens *after* a check resolved
//! paints the verdict instead of a spinner nothing will ever answer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const EVENT_CHECKING: &str = "updater://checking";
pub const EVENT_UP_TO_DATE: &str = "updater://up-to-date";
pub const EVENT_AVAILABLE: &str = "updater://update-available";
pub const EVENT_DOWNLOADING: &str = "updater://downloading";
pub const EVENT_PROGRESS: &str = "updater://download-progress";
pub const EVENT_READY: &str = "updater://ready";
pub const EVENT_ERROR: &str = "updater://error";

/// GitHub releases page — the manual download fallback shown when a content
/// filter blocks `objects.githubusercontent.com`.
pub const RELEASE_URL: &str = "https://github.com/Shalom-Karr/groupme-windows/releases/latest";

/// Label and page of the small frameless status dialog.
pub const WINDOW_LABEL: &str = "updater";

/// Returns `true` when the error is plausibly caused by a content filter
/// blocking the download — i.e. it is NOT a signature-verification failure.
///
/// Signature failures must never be classified as "blocked": the tamper case
/// (a forged or corrupted update) would then suggest the user fetch from GitHub,
/// which hands them the same tampered binary through a different route.
/// Everything else — network, timeout, HTTP, JSON, config — is infrastructure
/// the user's browser or a manual download can work around.
fn is_download_blocked(e: &tauri_plugin_updater::Error) -> bool {
    !matches!(
        e,
        tauri_plugin_updater::Error::Minisign(_)
            | tauri_plugin_updater::Error::SignatureUtf8(_)
            | tauri_plugin_updater::Error::Base64(_)
    )
}

const DIALOG_PAGE: &str = "update.html";

/// Let the app settle before the first check. Startup is already contending for
/// the network: the connectivity probe, the webview loading `web.groupme.com`,
/// and then the first (heaviest) archive sync pass.
const STARTUP_CHECK_DELAY_SECS: u64 = 20;

/// The app is a long-lived window people leave open for weeks; a startup-only
/// check would never fire again for exactly those users.
const RECHECK_INTERVAL_SECS: u64 = 60 * 60 * 24;

/// Caps the manifest fetch and the download. Without it a stalled connection on
/// a captive-portal network leaves `CHECK_IN_PROGRESS` stuck true and silently
/// no-ops every later check for the rest of the session.
const UPDATE_TIMEOUT_SECS: u64 = 30;

/// Serialises checks — the 24h loop and a user click can otherwise collide and
/// download the same installer twice.
static CHECK_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// Clears the guard on every exit from a check, including an early `?`.
struct CheckGuard;

impl Drop for CheckGuard {
    fn drop(&mut self) {
        CHECK_IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

struct StagedUpdate {
    update: Update,
    bytes: Vec<u8>,
}

fn staged() -> &'static Mutex<Option<StagedUpdate>> {
    static STAGED: OnceLock<Mutex<Option<StagedUpdate>>> = OnceLock::new();
    STAGED.get_or_init(|| Mutex::new(None))
}

fn last_status() -> &'static Mutex<Option<serde_json::Value>> {
    static LAST_STATUS: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();
    LAST_STATUS.get_or_init(|| Mutex::new(None))
}

/// A poisoned lock here means a previous holder panicked while swapping a
/// status value. There is no invariant to repair — take the data and carry on
/// rather than propagating a panic into the updater.
fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What started this run, which decides both how loud it is and whether it
/// downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// The 24h loop. Finds, downloads and stages without a word — nobody asked,
    /// so nothing is shown; the update simply lands on the next launch.
    Background,
    /// "Check for updates". Reports the verdict and stops at "available": the
    /// user is watching, and an installer download is theirs to authorise (this
    /// machine may be on a phone hotspot).
    Check,
    /// "Download" on the dialog. Re-resolves the manifest — one small JSON GET,
    /// cheaper than keeping a found `Update` alive between two commands — then
    /// downloads and stages it.
    Download,
}

impl Trigger {
    fn user_initiated(self) -> bool {
        !matches!(self, Trigger::Background)
    }

    fn stages(self) -> bool {
        !matches!(self, Trigger::Check)
    }
}

/// Start the background check loop: once shortly after launch, then daily.
pub fn spawn_periodic_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STARTUP_CHECK_DELAY_SECS)).await;
        loop {
            run(&app, Trigger::Background).await;
            tokio::time::sleep(Duration::from_secs(RECHECK_INTERVAL_SECS)).await;
        }
    });
}

/// Open, or refocus, the update dialog.
pub fn open_dialog(app: &AppHandle) -> tauri::Result<()> {
    if let Some(win) = app.get_webview_window(WINDOW_LABEL) {
        // Reused rather than rebuilt: the close button hides this window, so a
        // second open must bring the same one back.
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        tauri::WebviewUrl::App(DIALOG_PAGE.into()),
    )
    .title("GroupMe — Updates")
    .inner_size(380.0, 240.0)
    .decorations(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .center()
    .build()?;

    Ok(())
}

/// Best-effort install of a staged update while the app is quitting. No-op when
/// nothing is staged, including after [`updater_restart`] already consumed it,
/// so there is no double install.
pub fn install_staged_on_exit() {
    let Some(s) = take_staged() else {
        return;
    };
    if let Err(e) = s.update.install(&s.bytes) {
        // Nowhere left to show this — the app is on its way out. The staged
        // bytes are lost, and the next launch just checks again.
        log::error!("installing staged update on exit: {e}");
    }
}

fn take_staged() -> Option<StagedUpdate> {
    lock(staged()).take()
}

/// Broadcast a phase to the dialog and remember it for [`updater_last_status`].
fn emit_status(app: &AppHandle, event: &str, payload: serde_json::Value) {
    *lock(last_status()) = Some(json!({ "event": event, "payload": payload }));
    let _ = app.emit_to(WINDOW_LABEL, event, payload);
}

/// Runs a check to completion, converting any failure into a terminal
/// `updater://error` phase.
async fn run(app: &AppHandle, trigger: Trigger) {
    let user_initiated = trigger.user_initiated();

    if CHECK_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        // Something is already running. Re-announce "checking" instead of
        // returning silently, or a click landing during the startup check gives
        // the user a dialog that never resolves. The in-flight run's own
        // terminal phase still reaches the dialog.
        if user_initiated {
            emit_status(
                app,
                EVENT_CHECKING,
                json!({ "userInitiated": true, "currentVersion": version_of(app) }),
            );
        }
        return;
    }
    let _guard = CheckGuard;

    if let Err(e) = try_run(app, trigger).await {
        let message = e.to_string();
        if user_initiated {
            log::error!("update check failed: {message}");
        } else {
            // This app is offline-aware by design; a background check that
            // failed because the laptop is on a train is not an error anyone
            // needs to read in a log.
            log::debug!("background update check failed: {message}");
        }
        // Emitted even for a silent run: a failure must still leave a terminal
        // phase behind, or a dialog opened afterwards restores a stale
        // "Checking…". And it is deliberately its own phase — reporting a
        // network failure as "up to date" would tell the user they are current
        // when nobody actually knows.
        let mut payload = json!({ "userInitiated": user_initiated, "message": message });
        if is_download_blocked(&e) {
            payload["downloadBlocked"] = json!(true);
            payload["releaseUrl"] = json!(RELEASE_URL);
        }
        emit_status(app, EVENT_ERROR, payload);
    }
}

async fn try_run(app: &AppHandle, trigger: Trigger) -> Result<(), tauri_plugin_updater::Error> {
    let current = version_of(app);
    let user_initiated = trigger.user_initiated();

    emit_status(
        app,
        EVENT_CHECKING,
        json!({ "userInitiated": user_initiated, "currentVersion": current }),
    );

    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(UPDATE_TIMEOUT_SECS))
        .build()?;

    let Some(update) = updater.check().await? else {
        // Always emitted, background or not. A user-initiated check that says
        // nothing looks broken, and the phase costs nothing when no dialog is
        // listening.
        emit_status(
            app,
            EVENT_UP_TO_DATE,
            json!({ "userInitiated": user_initiated, "currentVersion": current }),
        );
        return Ok(());
    };

    let version = update.version.clone();
    let found = json!({
        "userInitiated": user_initiated,
        "currentVersion": current,
        "version": version,
        "notes": update.body.clone(),
        "releaseUrl": RELEASE_URL,
    });

    // Already staged this exact version? Skip the identical re-download the
    // daily loop would otherwise repeat forever, and re-announce readiness so
    // the dialog can still offer the restart.
    let already_staged = lock(staged())
        .as_ref()
        .is_some_and(|s| s.update.version == version);
    if already_staged {
        emit_status(app, EVENT_READY, found);
        return Ok(());
    }

    if !trigger.stages() {
        emit_status(app, EVENT_AVAILABLE, found);
        return Ok(());
    }

    emit_status(app, EVENT_DOWNLOADING, found.clone());

    let progress_app = app.clone();
    let mut downloaded: u64 = 0;
    let mut last_point: i64 = -1;
    let bytes = update
        .download(
            move |chunk_len, total| {
                // The plugin reports each chunk's length, not a running total —
                // the accounting is ours to do.
                downloaded += chunk_len as u64;
                let percent =
                    total.and_then(|t| (t > 0).then(|| (downloaded as f64 / t as f64) * 100.0));

                // One IPC message per whole percent. The chunk callback fires
                // every few KiB, and several thousand events would cost more
                // than the download itself.
                if let Some(p) = percent {
                    let point = p as i64;
                    if point == last_point {
                        return;
                    }
                    last_point = point;
                }

                let _ = progress_app.emit_to(
                    WINDOW_LABEL,
                    EVENT_PROGRESS,
                    json!({ "downloaded": downloaded, "total": total, "percent": percent }),
                );
            },
            || {},
        )
        .await?;

    *lock(staged()) = Some(StagedUpdate { update, bytes });
    emit_status(app, EVENT_READY, found);
    Ok(())
}

fn version_of(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

/// "Check for updates" / "Retry".
#[tauri::command]
pub async fn updater_check(app: AppHandle) {
    run(&app, Trigger::Check).await;
}

/// "Download" — fetch and stage the update without installing anything.
#[tauri::command]
pub async fn updater_download(app: AppHandle) {
    run(&app, Trigger::Download).await;
}

/// "Restart & Install" — the only in-session install path, and only ever from a
/// deliberate click.
#[tauri::command]
pub async fn updater_restart(app: AppHandle) {
    let Some(s) = take_staged() else {
        // Nothing staged (a stale dialog, or a previous install consumed it).
        // The user has already said they want this installed, so re-acquire it
        // rather than making them hunt for the Download button; they press
        // restart again once it is ready.
        run(&app, Trigger::Download).await;
        return;
    };

    // Tear the webviews down BEFORE installing. On Windows `install()` runs the
    // NSIS installer and hard-exits the process with WebView2 still alive,
    // which leaves the user-data directory locked and hangs the relaunch the
    // installer immediately attempts.
    for (_, win) in app.webview_windows() {
        let _ = win.destroy();
    }

    match s.update.install(&s.bytes) {
        Ok(()) => app.restart(),
        Err(e) => {
            log::error!("installing staged update: {e}");
            // The windows are gone, so this emit reaches nobody — but it caches
            // the phase, and the dialog reopened on the next line reads that
            // cache on load and shows the failure with a Retry button. Without
            // reopening, a failed install would leave the app running with no
            // visible window at all.
            emit_status(
                &app,
                EVENT_ERROR,
                json!({ "userInitiated": true, "message": format!("Install failed: {e}") }),
            );
            if let Err(e) = open_dialog(&app) {
                log::error!("reopening the update dialog after a failed install: {e}");
            }
        }
    }
}

/// The installed version, so the dialog can paint its header before any phase
/// arrives.
#[tauri::command]
pub fn updater_version(app: AppHandle) -> String {
    version_of(&app)
}

/// The last phase as `{ event, payload }`, or `null` if nothing has run yet.
///
/// The dialog calls this on load. `open_dialog` returns as soon as the window is
/// *created*, so a check that resolves before `update.html` has attached its
/// listeners would otherwise land on nobody and leave the panel blank forever.
#[tauri::command]
pub fn updater_last_status() -> Option<serde_json::Value> {
    lock(last_status()).clone()
}

/// Opens the GitHub releases page in the system browser.
///
/// Invoked from the update dialog when a download fails due to a content filter
/// blocking `objects.githubusercontent.com`. The opener plugin is called
/// Rust-side so the updater window needs no `opener:default` capability.
#[tauri::command]
pub fn updater_open_release_page(app: AppHandle) {
    use tauri_plugin_opener::OpenerExt;
    if let Err(e) = app.opener().open_url(RELEASE_URL, None::<&str>) {
        log::warn!("could not open release page in browser: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALOG_HTML: &str = include_str!("../frontend/update.html");

    #[test]
    fn the_dialog_listens_for_every_phase_we_emit() {
        // The Rust side and the HTML agree on these strings by convention only;
        // a rename on one side silently produces a dialog that never updates.
        for event in [
            EVENT_CHECKING,
            EVENT_UP_TO_DATE,
            EVENT_AVAILABLE,
            EVENT_DOWNLOADING,
            EVENT_PROGRESS,
            EVENT_READY,
            EVENT_ERROR,
        ] {
            assert!(
                DIALOG_HTML.contains(event),
                "update.html never references {event}"
            );
        }
    }

    #[test]
    fn the_dialog_invokes_only_commands_that_exist() {
        let source = include_str!("updater.rs");
        for command in [
            "updater_check",
            "updater_download",
            "updater_restart",
            "updater_version",
            "updater_last_status",
            "updater_open_release_page",
        ] {
            if DIALOG_HTML.contains(command) {
                assert!(
                    source.contains(&format!("pub async fn {command}"))
                        || source.contains(&format!("pub fn {command}")),
                    "update.html invokes {command}, which is not a command here"
                );
            }
        }
    }

    #[test]
    fn release_notes_never_reach_innerhtml() {
        // Notes and error strings come off a remote manifest. The dialog renders
        // them with textContent; an innerHTML assignment would turn the update
        // feed into script execution inside a window that can invoke commands.
        assert!(
            !DIALOG_HTML.contains("innerHTML"),
            "update.html assigns innerHTML somewhere; dynamic values must use textContent"
        );
    }

    #[test]
    fn concurrent_checks_are_refused_until_the_guard_drops() {
        assert!(
            !CHECK_IN_PROGRESS.swap(true, Ordering::SeqCst),
            "guard started set"
        );
        assert!(
            CHECK_IN_PROGRESS.swap(true, Ordering::SeqCst),
            "a second check must observe the guard"
        );
        drop(CheckGuard);
        assert!(
            !CHECK_IN_PROGRESS.load(Ordering::SeqCst),
            "the guard must clear on drop, or every later check silently no-ops"
        );
    }

    #[test]
    fn background_runs_stage_but_a_user_check_waits_for_a_click() {
        assert!(!Trigger::Background.user_initiated());
        assert!(Trigger::Background.stages());
        assert!(Trigger::Check.user_initiated());
        assert!(!Trigger::Check.stages());
        assert!(Trigger::Download.user_initiated());
        assert!(Trigger::Download.stages());
    }

    #[test]
    fn last_status_round_trips_the_shape_the_dialog_reads() {
        emit_snapshot(EVENT_READY, json!({ "version": "9.9.9" }));
        let s = updater_last_status().expect("a status was just recorded");
        assert_eq!(s["event"], EVENT_READY);
        assert_eq!(s["payload"]["version"], "9.9.9");
    }

    /// `emit_status` without an `AppHandle` — the caching half of it.
    fn emit_snapshot(event: &str, payload: serde_json::Value) {
        *lock(last_status()) = Some(json!({ "event": event, "payload": payload }));
    }
}
