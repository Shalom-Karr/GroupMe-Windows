# Architecture

GroupMe for Windows is a Tauri 2 desktop application. The Rust process hosts a WebView2 window; a background tokio runtime runs the archive worker. Everything shares one SQLite file.

---

## The three parts

```
┌─────────────────────────────────────────────────────────────────┐
│  WebView2 window                                                 │
│                                                                  │
│  ┌─────────────────────────┐   ┌───────────────────────────┐    │
│  │  ONLINE                 │   │  OFFLINE                  │    │
│  │  web.groupme.com        │   │  frontend/offline.html    │    │
│  │  (GroupMe's servers)    │   │  (bundled, no network)    │    │
│  │                         │   │                           │    │
│  │  sending / uploads /    │   │  read-only over SQLite    │    │
│  │  emoji / reactions      │   │  browse / search / media  │    │
│  └────────────┬────────────┘   └───────────────┬───────────┘    │
│               │ inject.js                       │                │
│               │ emits groupme://token           │                │
└───────────────┼─────────────────────────────────┼───────────────┘
                │ Tauri event channel              │ Tauri commands
                ▼                                 ▼
┌───────────────────────────────────────────────────────────────┐
│  Rust / Tauri process                                         │
│                                                               │
│  ┌──────────────┐   ┌─────────────────┐   ┌───────────────┐  │
│  │  token.rs    │   │  sync worker    │   │  store.rs     │  │
│  │  Credential  │   │  (planned)      │   │  SQLite       │  │
│  │  Manager     │   │  reqwest calls  │   │  archive.db   │  │
│  │  SHA-256 fp  │   │  api.groupme.com│   │               │  │
│  └──────────────┘   └─────────────────┘   └───────────────┘  │
└───────────────────────────────────────────────────────────────┘
```

**Part 1 — Online.** The webview navigates to `https://web.groupme.com`. GroupMe's own client handles all write operations. Nothing about sending, uploading, or reacting is implemented in this codebase; it falls through to the real web client. This also means the app stays correct when GroupMe reskins or restructures their frontend — we're not scraping markup.

**Part 2 — Archive.** A background tokio worker calls `api.groupme.com/v3` directly using reqwest. It reads the API (not the DOM) because the API is a versioned contract and the markup is deployment artefact. The worker is purely read-only; no write verb is ever issued. See [docs/groupme-api.md](groupme-api.md) for the full endpoint reference and the rationale behind each call.

**Part 3 — Offline.** When connectivity is lost, the Tauri process instructs the webview to navigate to the bundled `offline.html`, which reads from SQLite via Tauri commands. There is no send command registered on this surface, so read-only behaviour is structural rather than cosmetic — there is no path to a write.

---

## Process and threading model

The Tauri process is single-process, multi-thread:

- **Main thread** — Win32 message loop and WebView2 host. Owned by Tauri's runtime.
- **Tokio runtime** — `rt-multi-thread`, spawned once at startup. Hosts all async work: the sync worker, reqwest connection pools, and any IPC handlers that do I/O.
- **rusqlite** — synchronous, accessed from the tokio runtime via `spawn_blocking` or a dedicated thread. WAL journal mode means the sync worker writing does not block the offline reader reading.

WebView2 has a known behaviour on Windows: background timers are throttled when the window is minimised or occluded. This would stall any heartbeat that runs inside the webview. At startup `lib.rs` sets three Chromium flags via `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` to disable background timer throttling, renderer backgrounding, and occluded-window backgrounding.

Single-instance enforcement is handled by `tauri-plugin-single-instance`: if a second process starts, the running instance's window is shown and focused and the second process exits.

---

## Token capture flow

GroupMe's `x-access-token` is a ~40-character bearer credential. The app needs it to authenticate the archive worker's API calls. It is obtained from the webview without reimplementing the sign-in flow.

```
1. User signs in at web.groupme.com (inside the webview, GroupMe's own flow)

2. GroupMe's JS client makes its first API call to api.groupme.com with the
   x-access-token header

3. inject.js (a Tauri initialization_script, runs before page scripts) has
   patched window.fetch and XMLHttpRequest.prototype.setRequestHeader

4. The patch sees the api.groupme.com request, reads x-access-token from the
   request headers, and emits a groupme://token Tauri event

5. Rust receives the event, validates the token (20–128 alphanumeric chars),
   stores it in Windows Credential Manager under service=dev.shalomkarr.groupme,
   and writes a SHA-256 fingerprint to archive.db meta table

6. If the fingerprint differs from what is stored, a different account has
   signed in — the archive worker rejects mixing two accounts' data
```

**Why headers, not localStorage.** The `x-access-token` request header is the wire contract between GroupMe's web client and their API — it is visible to any browser with DevTools open. localStorage key names are minified build output; GroupMe can rename or restructure them between deploys without notice. Intercepting the header never silently breaks; reading a renamed localStorage key silently returns `undefined` forever.

**Why Credential Manager, not a config file.** A plaintext token in a config file is readable by any process running as the same user. Windows Credential Manager stores it encrypted and tied to the Windows account. The SQLite archive holds only a SHA-256 fingerprint, which is safe to log and safe to persist — it identifies which token without being usable as one.

The Tauri capability grant for the `web.groupme.com` remote origin is limited to `core:event:allow-emit` and `core:event:allow-listen`. That origin is third-party code we do not control, so it receives no filesystem, shell, or archive-read commands. The event channel is the minimum surface necessary.

---

## Connectivity state machine

The connectivity router (`frontend/index.html`) is the first thing the window loads. Its job is to route the session to either the live client or the local archive without exposing WebView2's "can't reach this page" error.

```
           ┌──────────────────────────────────────────────────────┐
           │  Startup / navigation                                 │
           └───────────────────────┬──────────────────────────────┘
                                   ▼
                        navigator.onLine === false?
                          or probe fetch fails?
                         /                    \
                       YES                    NO
                        │                     │
                        ▼                     ▼
                    ┌───────┐          ┌──────────────┐
                    │OFFLINE│          │    ONLINE    │
                    │       │◄─────────│ web.groupme  │
                    │archive│  network │    .com      │
                    │reader │  lost    │              │
                    └───┬───┘          └──────────────┘
                        │
                    "Try again" button
                        │
                        ▼
                  re-run probe
```

The probe is a `no-cors` fetch to `https://api.groupme.com/v3/users/me` with a cache-busting query parameter and a 6-second timeout. `no-cors` resolves on any HTTP response and rejects only on a genuine network failure — a `4xx` from GroupMe still means the internet is reachable.

`navigator.onLine` is used only to short-circuit to offline immediately when it reports `false`. It is not used to declare online — a captive portal or DNS blackhole can report `true` while all traffic fails. The probe is the authoritative check.

The offline state is deliberately reluctant to assert: a 2-second connectivity blip should not yank the user out of a conversation mid-read. Recovery is eager: the `window.addEventListener('online', route)` handler re-runs the probe as soon as the browser reports a link-layer change.

`inject.js` also emits `groupme://online` and `groupme://offline` events to Rust so the archive worker can react immediately without waiting for its own probe cycle.

---

## Sync strategy

The archive worker maintains two cursors per conversation: `newest_id` (the `after_id` tailing cursor) and `oldest_id` (the `before_id` backfill cursor). A `backfill_complete` flag is set once and never cleared; it is the signal that the entire history is held and backward walking can stop.

**Tail before backfill.** Each sync cycle updates `newest_id` first — fetching everything since the last seen message — before extending the backfill. A user who opens the app offline gets today's messages rather than 2019's, even if backfill has not reached that far. This is the most valuable ordering property of the strategy.

**Terminate on empty, not short.** The backfill loop stops when a page returns zero messages. Short pages occur legitimately mid-history (deleted messages, gaps) and are not a terminator. This is specified in the GroupMe API documentation and verified from live capture.

**Backfill cap per cycle.** Each sync cycle processes a bounded number of backfill pages per conversation. Without a cap, a single large group (GroupMe allows up to 5,000 members and years of history) can hold the tokio thread busy indefinitely and starve every other conversation's tail updates.

**Idempotent writes.** Sync re-fetches overlapping page ranges routinely, both because the tailing cursor may overlap the most recent backfill page and because retries after a network error replay the same request. `INSERT ... ON CONFLICT DO UPDATE` throughout `store.rs` means re-inserting a page already held is a no-op on every column that must not regress (see [docs/schema.md](schema.md) for the COALESCE rules on `deleted_at` and `updated_at`).

**Edit and delete events.** GroupMe delivers edits and deletions as new system messages rather than by mutating the original. A sync that is purely append-only would keep serving deleted content forever. The `store.rs::apply_event` function applies `message.update` and `message.deleted` events to the stored row. The original row is never dropped — the tombstone (`deleted_at` timestamp) is itself archival information.

See [docs/groupme-api.md](groupme-api.md) for the full endpoint catalogue and pagination details.
