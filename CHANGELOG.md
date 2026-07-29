# Changelog

Follows the [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) format.

## [Unreleased]

Initial development. No release has been cut.

### Added

- **Tauri 2 scaffold.** Native Windows `.exe` wrapping `https://web.groupme.com` in a WebView2 window. Single-instance enforcement via `tauri-plugin-single-instance`; background-timer throttling disabled via `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` so the sync worker is not stalled when the window is minimised.

- **SQLite archive with FTS5.** `store.rs` implements the full schema (schema version 1): `conversations`, `users`, `messages`, `attachments`, `media_cache`, `sync_state`, and an external-content `messages_fts` FTS5 index kept in step by INSERT/UPDATE/DELETE triggers. SQLite is compiled from source (`rusqlite` bundled feature) to guarantee FTS5 availability and remove the system-SQLite dependency. WAL journal mode; `PRAGMA user_version` for migration versioning.

- **GroupMe API client types.** `model.rs` defines wire types for the GroupMe v3 API: `Message`, `Group`, `Chat`, `Member`, `Attachment` (open enum with passthrough `Other` variant), `Reaction`, `SystemEvent`, `Me`, `Conversation`, and the `ConversationKind` discriminant. All fields are optional or defaulted; the deserialiser tolerates missing keys rather than failing on absent optional fields. `id_sort_key` parses GroupMe IDs to i64 for SQL ordering.

- **Token capture.** `inject.js` initialization script patches `window.fetch` and `XMLHttpRequest` to intercept `x-access-token` from outgoing `api.groupme.com` requests. Token is forwarded to Rust via `groupme://token` Tauri event. `token.rs` validates, stores in Windows Credential Manager (`keyring` crate, `windows-native` feature), and computes a SHA-256 fingerprint for account-change detection. The archive stores the fingerprint, never the raw token.

- **Connectivity detection and routing.** `frontend/index.html` serves as the startup page. Probes `api.groupme.com` with a `no-cors` fetch (6-second timeout, cache-busted); routes to `web.groupme.com` on success or to `offline.html` on failure. Listens for `window.online` events to recover eagerly. Manual "Read offline" button bypasses the probe.

- **Offline reader surface.** `frontend/offline.html` (in progress): bundled local reader rendering conversations and messages from SQLite via Tauri commands. No network assets; entirely self-contained. No send command registered — read-only is structural.

- **Background sync worker** (in progress): tokio-based worker calling `api.groupme.com/v3` with reqwest. Tail-before-backfill strategy: `newest_id` (`after_id`) updated before `oldest_id` (`before_id`) so a user offline has recent messages rather than oldest-first. Backfill capped per cycle to prevent one large group from starving the others. Terminates on empty page, not short page.

- **API documentation from live capture.** `docs/groupme-api.md` documents all endpoints, payload shapes, pagination rules, and gotchas from a proxied capture of `web.groupme.com` taken 2026-07-29. Covers the SAS-expiry problem with attachment URLs (§7), the IEEE-754 ID corruption issue (§8), and the Faye realtime surface (§9). Capture tooling: `tools/capture_api.py` (selenium-wire MITM proxy, includes monkey-patch for pyOpenSSL 23.3 incompatibility) and `tools/digest_capture.py`.

- **Project documentation.** `README.md`, `docs/architecture.md`, `docs/schema.md`, `docs/offline-behaviour.md`.
