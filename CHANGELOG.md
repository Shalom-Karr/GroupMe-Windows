# Changelog

Follows the [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) format.

## [0.2.0] — 2026-07-29

The app grows its own client. Until now it was a wrapper around
`web.groupme.com` plus a read-only offline reader; this release adds a full
custom UI over the local archive, live updates over GroupMe's realtime socket,
and the write side of the API to back it.

### Added

- **Custom client UI** (`frontend/client.html`). A complete GroupMe client
  reading from the local SQLite archive: conversation list with unread state,
  virtualised message list (142k-message conversations stay smooth), replies,
  reactions, attachments, system events — and full-text search over the FTS5
  index with jump-to-message-in-context, which the web app does not have.
  Entirely self-contained: no CDN, no external resources, vanilla JS.
- **Surface toggle, remembered.** A `Custom UI` button on `web.groupme.com`
  (injected) and a `Web UI` button in the custom client switch between the two;
  the last-used surface is stored in the archive and the router reopens it on
  the next launch. The web client stays the bootstrap surface — it is where the
  token is captured.
- **Realtime over Faye** (`realtime.rs`). Connects to
  `wss://push.groupme.com/faye` (native-tls, same trust store as the browser),
  subscribes to the account's user channel, and applies incoming messages,
  edits, deletes and reactions to the archive before emitting
  `realtime://message|reaction|typing|state` to the UI. Exponential backoff
  with jitter; a rejected token stops the worker rather than spinning.
- **Write API** (`api.rs`). Send (group and DM), edit, delete, react/unreact,
  read receipts and image upload — each routed and status-checked individually,
  because GroupMe spreads them across `/v3`, `/v4` and `m.groupme.com` with
  four different success codes.
- **Write command surface** (`client_commands.rs`). Nine `client_*` commands
  gated behind `/users/me` verification, mirroring every accepted write into
  the archive. A meta-test holds the read/write split: no `archive_*` reader
  can appear here, no mutation can appear on the offline surface.
- **Persisted-token bootstrap.** A token saved by a previous session now starts
  sync (and therefore realtime and the write surface) at launch, still through
  the same verification as a captured one — the archive and custom client work
  without visiting `web.groupme.com` first.

### Fixed

- `apply_event` updated the `deleted_at`/`text` columns but not `raw_json`,
  which every reader rebuilds messages from — so realtime edits and deletions
  were invisible until the poller happened to refetch the row. The JSON is now
  patched in the same statement.
- The realtime worker held the struct containing its own command sender, so
  dropping every handle could never close the channel — its shutdown path was
  unreachable and a token rotation would have leaked a second socket.

## [0.1.1] — 2026-07-29

- **TLS: reqwest and tokio-tungstenite use native-tls (schannel), not
  rustls.** v0.1.0 could not sync at all behind TLS inspection — rustls
  validates against a compiled-in root store and ignores the Windows
  certificate store. See `CLAUDE.md` for the full post-mortem.
- Minimise/restore ordering fixed (`unminimize` before `show`); first-run
  archiving now tails every conversation before deep-backfilling any
  (~5 hours → ~5 minutes on a 142k-message group); tray toggle to simulate
  offline; sync-status panel; `protocol-asset` enabled so cached media renders.

## [0.1.0] — 2026-07-29

First release: web wrapper, SQLite archive with FTS5, token capture,
connectivity routing, read-only offline reader, NSIS installer with
auto-update.

### Initially built



- **Tauri 2 scaffold.** Native Windows `.exe` wrapping `https://web.groupme.com` in a WebView2 window. Single-instance enforcement via `tauri-plugin-single-instance`; background-timer throttling disabled via `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` so the sync worker is not stalled when the window is minimised.

- **SQLite archive with FTS5.** `store.rs` implements the full schema (schema version 1): `conversations`, `users`, `messages`, `attachments`, `media_cache`, `sync_state`, and an external-content `messages_fts` FTS5 index kept in step by INSERT/UPDATE/DELETE triggers. SQLite is compiled from source (`rusqlite` bundled feature) to guarantee FTS5 availability and remove the system-SQLite dependency. WAL journal mode; `PRAGMA user_version` for migration versioning.

- **GroupMe API client types.** `model.rs` defines wire types for the GroupMe v3 API: `Message`, `Group`, `Chat`, `Member`, `Attachment` (open enum with passthrough `Other` variant), `Reaction`, `SystemEvent`, `Me`, `Conversation`, and the `ConversationKind` discriminant. All fields are optional or defaulted; the deserialiser tolerates missing keys rather than failing on absent optional fields. `id_sort_key` parses GroupMe IDs to i64 for SQL ordering.

- **Token capture.** `inject.js` initialization script patches `window.fetch` and `XMLHttpRequest` to intercept `x-access-token` from outgoing `api.groupme.com` requests. Token is forwarded to Rust via `groupme://token` Tauri event. `token.rs` validates, stores in Windows Credential Manager (`keyring` crate, `windows-native` feature), and computes a SHA-256 fingerprint for account-change detection. The archive stores the fingerprint, never the raw token.

- **Connectivity detection and routing.** `frontend/index.html` serves as the startup page. Probes `api.groupme.com` with a `no-cors` fetch (6-second timeout, cache-busted); routes to `web.groupme.com` on success or to `offline.html` on failure. Listens for `window.online` events to recover eagerly. Manual "Read offline" button bypasses the probe.

- **Offline reader surface.** `frontend/offline.html` (in progress): bundled local reader rendering conversations and messages from SQLite via Tauri commands. No network assets; entirely self-contained. No send command registered — read-only is structural.

- **Background sync worker** (in progress): tokio-based worker calling `api.groupme.com/v3` with reqwest. Tail-before-backfill strategy: `newest_id` (`after_id`) updated before `oldest_id` (`before_id`) so a user offline has recent messages rather than oldest-first. Backfill capped per cycle to prevent one large group from starving the others. Terminates on empty page, not short page.

- **API documentation from live capture.** `docs/groupme-api.md` documents all endpoints, payload shapes, pagination rules, and gotchas from a proxied capture of `web.groupme.com` taken 2026-07-29. Covers the SAS-expiry problem with attachment URLs (§7), the IEEE-754 ID corruption issue (§8), and the Faye realtime surface (§9). Capture tooling: `tools/capture_api.py` (selenium-wire MITM proxy, includes monkey-patch for pyOpenSSL 23.3 incompatibility) and `tools/digest_capture.py`.

- **Project documentation.** `README.md`, `docs/architecture.md`, `docs/schema.md`, `docs/offline-behaviour.md`.
