# Offline behaviour

When the connectivity router (`frontend/index.html`) determines the network is unreachable, the webview navigates to the bundled `frontend/offline.html`, which reads entirely from the local SQLite archive. No network requests are made; all assets are bundled or cached locally.

---

## What works offline

- **Browse all archived conversations.** The conversation list renders from the `conversations` table, sorted by recency (`COALESCE(last_message_created_at, updated_at) DESC`). Both groups and DMs appear in a unified list.

- **Read full message history.** Every message archived before connectivity was lost is readable. The reader pages backward using `id_sort` cursor pagination — the same `before_id` logic used by the API, implemented locally.

- **Full-text search.** The `messages_fts` FTS5 table is local and indexes every message body that was archived. Search results include conversation name and sender name via JOIN against the `conversations` and `messages` tables. The `unicode61` tokenizer handles emoji and non-ASCII text.

- **View cached images and avatars.** Any image or avatar whose bytes were downloaded to the `blobs/` directory before going offline renders from the local path stored in `media_cache`. The `local_path` column maps the original URL to the cached file.

- **BlurHash previews for uncached images.** For images where the `blur_hash` field was present in the API response, the offline reader can decode a blurred placeholder from approximately 30 bytes of stored data. This shows something recognisable instead of a broken-image icon for images that were never fully downloaded.

---

## What does not work offline

- **Sending messages.** There is no send composer in the offline reader. See the rationale below.

- **Reactions, edits, and deletes.** Any write operation against the GroupMe API requires connectivity. The offline surface has no commands registered for these.

- **Uploads.** File and image upload goes through GroupMe's own infrastructure and requires connectivity.

- **Messages not yet synced.** Only messages that were archived before connectivity was lost are readable. If the last sync cycle ran three hours ago, the three-hour gap is dark. This is not recoverable offline.

- **Uncached media.** Attachment images and avatars that were not downloaded to `blobs/` before going offline show either a BlurHash placeholder (if the field was present in the API response) or a blank placeholder. Video thumbnails follow the same rule.

- **Real-time state.** Read receipts, typing indicators, reaction updates, and any other live state require connectivity. The archive is a snapshot, not a live view.

---

## Why there is no outbox

A queued-send outbox — where messages typed offline are held locally and fired when connectivity returns — is deliberately not implemented, and will not be.

The problem is trust. If a message is composed at 2 pm and the connection drops, the user has no way of knowing when it will actually send. It might fire at 4 pm, mid-conversation, in a thread that has moved on, with no context that it was queued. Recipients have no indication the message is delayed. GroupMe has no way to distinguish a queued send from an intentional late message, and there is no API for timestamped delivery.

A disabled composer is the honest behaviour. It tells the user: you cannot send right now. A message sent is a message sent, with all the social expectations attached to that timing. A message that fires silently hours later is a reliability problem dressed up as a feature.

This is a constraint of the current architecture, not a bug to fix: the archiver is read-only against the GroupMe API by design, and the offline surface inherits that property structurally.

---

## Connectivity detection and the state machine

The router in `frontend/index.html` runs a probe before routing the session:

1. If `navigator.onLine === false`, skip the probe and go offline immediately. The browser's link-layer state is reliable for the negative case.

2. Otherwise, fire a `no-cors` fetch to `https://api.groupme.com/v3/users/me` with a cache-busting parameter and a 6-second timeout. `no-cors` resolves on any HTTP response (including 4xx from GroupMe) and rejects only on a genuine network failure. A captive portal or a GroupMe 500 does not trigger offline mode.

3. If the probe fails, show the offline UI with "Try again" and "Read offline" buttons.

4. If the probe succeeds, navigate to `https://web.groupme.com`.

The probe timeout is 6 seconds. This is deliberately long. A 2-second blip — a momentary packet loss, a Wi-Fi handoff — should not yank the user out of a conversation. The intent is to declare offline only when there is a clear sustained failure, not at the first sign of latency.

Recovery is eager. `window.addEventListener('online', route)` re-runs the probe as soon as the browser reports a link-layer change. `inject.js` also emits `groupme://online` to the Rust process so the sync worker can restart without waiting for its own timer.

The "Read offline" button on the router page navigates directly to `offline.html` without waiting for the probe to time out. This is the manual offline toggle.

---

## Verifying offline mode

To verify the offline reader works without an actual network outage:

1. Build and run the app.
2. Wait for at least one sync cycle to complete (the archive must have content).
3. Open `frontend/index.html` directly in a browser, or navigate the webview to it.
4. Click "Read offline" — this bypasses the probe and navigates to `offline.html`.
5. Alternatively, disable the network adapter in Windows or use Windows Firewall to block `api.groupme.com`. The probe will time out (6 seconds) and the offline UI will appear.

The `offline.html` surface renders entirely from the local SQLite file. If conversations and messages are visible, the archive is working. If the list is empty, either no sync cycle has run or the database path is wrong.
