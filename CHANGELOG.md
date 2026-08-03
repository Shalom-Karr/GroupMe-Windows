# Changelog

Follows the [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) format.

## [0.12.0] — 2026-08-03

Hardening for 1.0: the updater routes around the filter, a blank window heals
itself, and the leaderboard stops freezing the app.

### Fixed

- **Updating works on filtered networks.** When the installer download is
  blocked (a content filter that allows browsing but kills the download host),
  the update dialog now offers "Download in browser" — one click opens the
  release page, where the download succeeds. Signature failures deliberately
  never offer this: if verification fails, routing around it is the wrong move.
- **A blank window heals itself.** When the webview fails to attach at launch
  (orphaned WebView2 processes holding the profile), the watchdog now rebuilds
  the window once — the restart-shaped fix, automated — instead of only writing
  advice to the log. Window-bound listeners were made rebuild-proof.
- **The all-groups leaderboard no longer freezes the app.** On a million-message
  archive it takes ~20 seconds to compute; it now runs on its own read-only
  database connection, so reading messages and searching stay instant while it
  works. Group stats and profile cards take the same path.

## [0.11.0] — 2026-08-03

The messenger finally speaks up, and the client gets its finishing pass.

### Added

- **Desktop notifications.** New messages now notify — the code always existed,
  nothing ever called it. Notifications skip your own messages and system
  events, stay silent while the window is focused, respect per-conversation
  mutes, and a burst in one conversation fires at most one toast per 30
  seconds.
- **Ctrl+K quick switcher.** Type a conversation name, Enter opens it; ↑/↓ to
  choose. Alt+↑/↓ steps through the sidebar without the mouse.
- **Image lightbox.** Click any image for a full-screen view — click to zoom,
  wheel up to 6×, drag to pan, Esc to close.
- **A floating "↓ Latest" pill** when scrolled into history.
- **Backfill visibility.** The sync panel now says how many conversations are
  still filling in deep history.

### Fixed

- Profile cards open instantly and fill in, instead of waiting on the lookup.
- Focus rings, hover states, tooltips, ellipsis and scrollbars audited across
  the client (most were already consistent; the stragglers now match).

## [0.10.0] — 2026-08-03

### Added

- **Download attachments on demand.** An attachment the background sync has not
  cached yet now shows a "Download" button on its placeholder — one click
  fetches that file immediately and swaps it in, instead of waiting for the
  budgeted media pass to reach it. Downloads land in the same content-addressed
  cache the sync uses, so a file fetched by hand is indistinguishable from one
  fetched in the background. Failures offer a retry and never fall back to the
  remote URL (GroupMe's CDN links expire).

## [0.9.0] — 2026-08-03

The archive learns to show off: a leaderboard, group metadata, and jumps that
never dead-end.

### Added

- **Leaderboard.** A trophy button on any group opens a ranked sheet — podium
  for the top three, points bars, period tabs (all time / 30 days / 7 days /
  today), and a this-group / all-groups toggle. Points follow the SKChats
  economy: messages ×1, likes received ×20, likes given ×10, leaving −25,
  being removed −500, deleted messages −5 — all computed locally from the
  archive, including reading kick-vs-left and message-deleted events out of the
  stored system messages. Click a row for its full breakdown; negative totals
  collapse under a toggle.
- **About this group.** Group settings now open with everything the archive
  knows: created date, first archived message (click to jump to it), latest
  message, messages archived, how many people ever wrote, active people and
  messages in the last 30 days, average active people per day, busiest day, and
  the most prolific member (click for their profile).
- **Go to first message.** In the date popover and every conversation's menu.
- **Go-to-date never dead-ends.** A date older than the archive now lands on
  the oldest archived message with an honest note, instead of a "no messages"
  toast. (Deep history is also actively backfilling now that the group API is
  reachable — the archive's edge moves earlier every cycle.)

## [0.8.0] — 2026-08-03

The client becomes a live messenger: typing indicators, on-demand sync, filters,
and the archive starts remembering what GroupMe forgets — past groups and past
members.

### Added

- **Typing indicators.** When someone is typing in the open conversation, a
  quiet line above the composer says so — one name, two names, or "3 people are
  typing…" — fed by the realtime socket events the app already received but
  never showed.
- **Sync now.** An empty conversation offers a "Sync now" button, and every
  conversation's actions menu gains one — it jumps that conversation to the
  front of the sync queue instead of waiting for the background cycle to reach
  it.
- **Past groups.** Groups you have left no longer vanish: they move to a
  collapsed "Archives" section at the bottom of the sidebar, readable in full,
  with the composer replaced by a note (you cannot send there anyway). The sync
  marks a group as former only after a successful list fetch, so being offline
  never mis-files anything.
- **Past members.** A group's settings panel now lists people who wrote in the
  group but are no longer in it — with their message count and when they were
  last seen — reconstructed from the archive, which kept their messages all
  along. Clicking one opens their profile card.
- **Sidebar filters.** Toggle chips for Unread, Groups, and DMs. Unread means
  server-confirmed unread, and combines with either kind filter.
- **Sync status panel.** A small status button in the sidebar opens a summary:
  groups/DMs archived, message counts, last sync time, and whether the last
  cycle was healthy.

### Fixed

- **Composer layout.** The message bar's alignment and sizing quirks — button
  drift when the textarea grows, overflow with long text or several image
  drafts, stray gaps — were audited and fixed.

## [0.7.0] — 2026-08-02

Groups sync on networks whose content filter caps response size.

### Fixed

- **Groups never loaded behind a size-limited content filter.** A filter that
  buffers each response to scan it (observed with Techloq, ~1 MB ceiling) blocked
  `GET /v3/groups` wholesale, because the full list — every group with its entire
  member roster — runs to several megabytes. It was never content-specific; no
  group was "blocked," the response was just too big to pass. The list is now
  fetched lean (`omit=memberships`, ~140 KB per 100 groups), and each group's
  members are pulled in a separate small `group_detail` call and cached — only
  groups whose roster is not stored yet are fetched. After the first index, a
  sync is just the lean list plus new messages.

## [0.6.0] — 2026-07-30

Know who you are talking to, and paste a screenshot straight in.

### Added

- **Profile cards.** Clicking anyone's avatar — in the message list or a group's
  member list — opens a card with everything the archive knows about them: their
  name and picture, the groups you both belong to (each opens on click), a link
  to your DM with them when one is archived, how many of their messages are
  stored and when they were first and last seen, and a block/unblock action. A
  new read-only `archive_user_profile` command serves it; shared groups are found
  by scanning each group's stored membership, so it works fully offline.
- **Paste an image into the composer.** Copy a screenshot (or any image) and
  paste it into the message box to attach it — the same path as picking a file or
  dropping one in. Pasting text is unchanged.

## [0.5.0] — 2026-07-30

Conversation management, a denser client, and resilience against a network
filter that blocks the API.

### Added

- **Per-conversation actions.** Each sidebar row and the new header settings
  panel offer pin/unpin, reorder pins, mute/unmute, leave group, and block user.
  Pinned conversations sort to the top in their own section. Pin, order and mute
  are stored locally (schema v3), so they work offline and do not depend on the
  API; leave and block are the captured GroupMe write endpoints.
- **Group settings panel.** Clicking the conversation's name or avatar in the
  header opens a panel; for a group it lists the roster from
  `GET /v3/groups/{id}?include=members` with owner/admin badges and per-member
  block, for a DM a lighter panel with block/mute.
- **The header now shows the conversation's avatar and name** (group image or DM
  avatar, with the same letter-plate fallback the sidebar uses).
- **Date picker and in-conversation search.** The thread header gains a date
  picker that jumps to a day (`archive_message_near_date`) and a search scoped to
  the open conversation (`archive_search` with a conversation filter), with
  jump-to-context. (Semantic search deferred.)
- **Filter resilience.** On a network that blocks the API host, the app now
  detects the interception (a foreign host or HTML where JSON was due) and says
  so plainly instead of silently archiving nothing; offers a browser-context
  proxy that retries through the web session (created lazily, so an unfiltered
  machine never pays for it); and a tray item, "Allow API through content
  filter…", that issues the request from inside the app so the user can approve
  it in their filter.

### Fixed

- **Adding a reaction failed in DMs.** The like endpoint takes the `+`-joined DM
  thread key, but the archive stores DMs under the other participant's bare id,
  so `client_react`/`client_unreact` 404'd — the same key mismatch already fixed
  for send and mark-read. They now resolve the composite key before calling.
- **`mark_read` no longer surfaces an error.** It runs automatically on opening a
  thread and legitimately fails (a group you have left answers `403 not a
  member`; a filtered network blocks the write), so it is logged and ignored
  rather than shown.
- **Density.** The client's vertical rhythm was tightened throughout — header,
  message list, sidebar, composer — so it reads as a chat client rather than a
  roomy web page.

### Changed

- The sync-status panel splits **Groups** and **Direct messages** into their own
  rows with per-kind message counts, and says plainly when groups are empty while
  DMs are not — which on a filtered network means the group API is blocked.

## [0.4.1] — 2026-07-30

Two bugs that only running the app could have found — including one that made
0.4.0's headline fix do nothing.

### Fixed

- **Unread was still wrong in 0.4.0, because nothing ever populated the read
  state.** The storage and the three-tier UI rule were correct, but the fields
  were read off the group and chat objects, where GroupMe leaves them `null` —
  as this project's own API reference says in §4.5. Read state has exactly one
  source, `GET /v4/read_receipts`, which returns the whole map in a single call.
  Now fetched each sync cycle and applied to the archive.

  That endpoint is enveloped like the rest of the API, and decoding its top
  level instead of `response` produced a silent empty list — indistinguishable
  from "you have read nothing", so every conversation still showed unread. With
  both corrected: **375 receipts returned, 223 matched an archived conversation,
  220 of 223 resolved to zero unread** — the sidebar went from 215 unread to 3.

  Receipts key DMs by the `+`-joined thread key while the archive keys them by
  the other participant's user id, so they are mapped through the signed-in
  account; a receipt for a conversation we have not archived is ignored rather
  than inserted, since a bare id carries no kind or name and would appear as a
  blank row.

  Fetching read state cannot fail the cycle: failures are logged, never added to
  `SyncReport::errors`, because that field is what the status panel reports as
  "sync had errors" and a stale unread dot must not make a healthy archive look
  broken.

- **Realtime died on the first DM opened.** The handshake succeeded and then
  `/meta/subscribe` returned `Access token authentication failed`, which stops
  the worker by design — a rejected credential must not be retried in a loop —
  so live updates silently stopped for the rest of the session.

  The archive stores a DM under the **other participant's user id**, not the
  composite `"{a}+{b}"` thread key, so a DM id is shape-identical to a group id.
  The channel was chosen by looking for a `+`, which meant every DM subscribed
  to `/group/{some_user_id}` — a channel the account does not own. The kind is
  now passed explicitly from the UI, which knows it, and is rejected rather than
  defaulted if it is missing: defaulting is what produced the wrong channel.

## [0.4.0] — 2026-07-29

A design pass over every surface, and the unread indicator finally tells the
truth.

### Fixed

- **Conversations you had already read still showed as unread.** The archive held
  no read state at all, so the client could only compare each conversation
  against the last time *that window* had opened it — recorded in localStorage,
  empty on a fresh window, so everything with any message read as unread forever.
  GroupMe sends `unread_count`, `last_read_message_id` and `last_read_at` on both
  list endpoints and nothing parsed them. Now stored (schema v2) and preferred by
  the UI in three tiers: the server count, else the last-read id compared against
  the newest known id as digit strings, else the old local behaviour.

  Two details that are wrong in their obvious form: the fields are `Option`, not
  defaulted to `0`, because "absent" and "zero unread" are different claims and
  reading a missing count as *all read* would hide genuinely unread
  conversations; and the upserts `COALESCE` rather than overwrite, because
  `GET /v3/groups` omits read state on most groups (200 of 211 in the capture)
  and a list sync would otherwise erase what a single-group fetch established.

  Mark-read now also fires when the server said unread but the local timestamp
  had not moved — exactly the case the old rule got wrong.
- Failed page resources log at `debug` rather than `warn`. GroupMe's attachment
  URLs redirect to expiring signed CDN links, so an avatar that 403s is
  documented behaviour, one per avatar on screen — the same log flooding the
  0.3.0 level filtering exists to prevent.

### Changed

- **Every surface redesigned.** The app no longer imitates GroupMe. The direction
  is the archive's own: a research instrument, or a well-set periodical. Warm
  paper in light, deep ink in dark, one vermilion accent used sparingly, hairline
  rules instead of filled boxes, and typography carrying hierarchy rather than
  weight and colour.
  - **The client is a transcript, not a bubble feed** — one continuous column
    with bylines, so replies read as quotations, reactions as marginalia and
    system events as clearly secondary. It also holds up better at 142,000
    messages than alternating alignment. Own messages take an accent rule in the
    margin. Search reads as an index: numbered entries, conversation in display
    serif, sender as a tracked attribution.
  - **The offline reader** loses its disabled composer entirely. A dead input box
    that mimes sending is a worse promise than a stated one, so the thread ends
    in a colophon — which also means no input element exists on that surface at
    all, tightening the read-only boundary rather than decorating it. Being
    offline is styled as the archive working correctly, not as a fault.
  - **Router, sync status and updater** read as printed artifacts rather than
    dialogs. Level is carried by a marginal rule and the wording, never an
    alarmed panel, so colour is never the only signal. The generic spinners are
    gone.
  - Fonts ship with Windows 10/11 — Sitka Heading, Corbel, Bahnschrift, Cascadia
    Mono. Nothing is fetched: the CSP forbids it and `index.html` has to render
    in exactly the case where the network is down.
  - Contrast was computed, not eyeballed, and it changed the design: accent on
    accent-soft is only 4.35:1 in dark, so accent became a rule on those fills;
    the avatar palette is eight muted earth tones, each ≥5.9:1 against its
    initials.

`index.html`'s script block is byte-identical to the previous release, so every
routing decision is unchanged by construction rather than by inspection.

## [0.3.0] — 2026-07-29

### Fixed

- **The window could open completely blank, and nothing said why.**
  `WebviewWindowBuilder::build()` returns `Ok` even when WebView2 fails to attach
  a webview — wry logs the `HRESULT` and carries on — so the app ran with a
  window that painted nothing while the archive synced and the realtime socket
  stayed connected. Every other signal said the app was healthy.

  Cause: WebView2's user-data folder admits one owner at a time. When a previous
  instance's WebView2 processes outlive it — a force-kill, a crash, or an update
  that relaunches before the old children exit — the next launch loses the race
  and gets `E_INVALIDARG` (`0x80070057`). It clears once those processes exit,
  which is why it looked intermittent and unreproducible.

  Confirmed by holding the binary constant and changing only the profile: the
  existing folder failed to create a webview, a fresh one loaded the router and
  reached `web.groupme.com` normally.

### Added

- **Page lifecycle diagnostics.** A release build has no devtools and a remote
  page has no console to read, so a blank window was previously indistinguishable
  from a page that loaded and rendered nothing. `inject.js` now reports
  `script-start` / `dom-ready` / `load` (with body text length), plus JS errors,
  unhandled rejections and failed resources, over the event channel; Rust logs
  them on one timeline beside the backend's own lines. Untrusted like anything
  from that origin — only ever logged, never acted on, every field capped at the
  source.
- **A webview watchdog.** If no page reports in within 15 seconds, the app logs
  the specific cause and remedy and raises a notification, because the tray is
  the only surface still working in that state. It deliberately does not
  relaunch itself: the failure is a race against processes we may not have
  finished losing, and an automatic restart risks a loop worse than a message.
- Two contract tests pin the event names shared between `inject.js` and Rust.
  The beacon and the UI toggle cross a file boundary the compiler cannot check,
  and a drifted name would have the watchdog declare every healthy launch broken.

### Changed

- **Logging is levelled.** `tauri_plugin_log::Builder::new()` logs `TRACE` for
  every crate in the tree; the websocket stack alone emitted a dozen lines per
  keepalive, measured at **92% of a real log file**, rotating away the very
  connectivity transitions and media failures the log existed to record. Now
  `Info` globally, `Debug` for this crate, `Warn` for the transport crates.

## [0.2.1] — 2026-07-29

### Added

- **Typing indicators, connected.** `realtime.rs` had `watch_group` and
  `send_typing`, and the client rendered "X is typing…" with an expiry timer,
  but nothing called either — the feature was dead in both directions. Typing
  is published per-conversation and the socket only subscribed to the account's
  own channel, so no notice could arrive; and none was ever sent.
  `client_watch_conversation` now subscribes on open (dropping the previous
  thread, so a long session cannot accumulate every thread visited) and
  `client_typing` publishes, throttled to one notice per three seconds.

### Fixed

- **Memory is now bounded rather than left to defaults.** SQLite had no limits
  beyond `journal_mode`/`synchronous`, and the remaining defaults scale with
  file size rather than working set on a multi-gigabyte archive: now an 8 MiB
  page cache, a 16 MiB `journal_size_limit` so a first-run backfill does not
  leave a permanently huge `-wal`, and `temp_store=FILE` with a 64 MiB
  `soft_heap_limit` so FTS merges spill to disk instead of RSS. `mmap_size` is
  deliberately left unset.
- **Retained message data was unbounded** even though the DOM was not: the chunk
  observer frees nodes but each chunk kept its message array, so a window left
  open in a busy group grew forever. Capped at 3,000 messages, trimming only the
  oldest end and only while the reader is at the bottom; recovery is the
  existing scroll-up refetch.
- `MAX_UPLOAD_BYTES` reduced from 50 MiB to 16 MiB. That number sets a memory
  spike, not a policy — the bytes exist at once as a JS array, an IPC payload,
  and a `Vec<u8>`.

### Note

Measured on the running app: the Rust host is 12 MB, while WebView2 holding
`web.groupme.com` is ~445 MB across ten processes. The archive layer was never
the memory problem — the wrapped web app is, which is why the custom client is
the real fix.

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
