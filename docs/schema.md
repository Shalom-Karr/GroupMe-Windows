# SQLite schema

The archive lives at `%LOCALAPPDATA%\dev.shalomkarr.groupme\archive.db`, with
cached media alongside it in `media\`. All schema is created by the `SCHEMA_V1`
constant in `src-tauri/src/store.rs`.

`%LOCALAPPDATA%`, not `%APPDATA%`, and the distinction matters: `%APPDATA%` is
the *roaming* profile, which a domain-joined Windows machine synchronises to the
server at every logon. A message archive reaches multiple gigabytes, so putting
it there would drag the whole thing across the network on every sign-in.

---

## Migration

Version tracking uses `PRAGMA user_version`. The `SCHEMA_VERSION` constant in `store.rs` is the current version (currently `1`). On open, the store reads `PRAGMA user_version`, applies any missing migration blocks in a transaction, and sets `PRAGMA user_version` to `SCHEMA_VERSION` on commit.

`PRAGMA journal_mode = WAL` is set on every open. WAL mode lets the sync worker write without blocking the offline reader, which holds a read transaction independently.

`PRAGMA foreign_keys = ON` is enabled. `PRAGMA synchronous = NORMAL` is a deliberate choice: with WAL, NORMAL durability survives all OS crashes (the WAL file is fsync'd at checkpoints), while FULL would add a sync per write for no additional protection on a local archive.

---

## Tables

### `meta`

```sql
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
```

Key-value store for app-level metadata. Current uses:

| Key | Value |
|---|---|
| `token_fingerprint` | Hex SHA-256 of the stored access token. Used to detect account changes. |

Writes use `INSERT ... ON CONFLICT DO UPDATE SET value = excluded.value` so the key is a singleton.

---

### `conversations`

```sql
CREATE TABLE conversations (
    id                      TEXT PRIMARY KEY,
    kind                    TEXT NOT NULL CHECK (kind IN ('group','dm')),
    name                    TEXT,
    description             TEXT,
    image_url               TEXT,
    creator_user_id         TEXT,
    created_at              INTEGER DEFAULT 0,
    updated_at              INTEGER DEFAULT 0,
    messages_count          INTEGER,
    last_message_id         TEXT,
    last_message_text       TEXT,
    last_message_created_at INTEGER,
    members_json            TEXT,
    raw_json                TEXT,
    synced_at               INTEGER
);
```

Groups and DMs share one table. The offline reader shows a unified conversation list ordered by recency; a UNION across two tables on every read would add complexity for no benefit.

**`id`** is `TEXT`. GroupMe group IDs are decimal strings (e.g. `"28330145"`) that fit comfortably in i64 but exceed IEEE-754 double precision at the upper end of the ID space. Storing any GroupMe ID as a numeric SQL type risks silent corruption on round-trip. For DMs the ID is derived from the `other_user.id` field on the chat object (see [docs/groupme-api.md §6](groupme-api.md)).

**`kind`** is enforced by a `CHECK` constraint. The two values correspond to `ConversationKind::Group` and `ConversationKind::Dm` in `model.rs`.

**`members_json`** is a JSON-serialised `Vec<Member>`. Members are not broken into a separate table because the archive never queries individual membership records — it reads the whole array at once for a group detail view.

**`raw_json`** preserves the full API response for forward compatibility. Fields GroupMe adds later are present in `raw_json` even if `store.rs` predates them.

**`synced_at`** is a Unix timestamp of the last successful upsert from the API, used to determine whether a conversation's member list is stale.

Upserts use `ON CONFLICT DO UPDATE` and never overwrite `created_at` or `creator_user_id`, which are set once.

---

### `users`

```sql
CREATE TABLE users (
    id         TEXT PRIMARY KEY,
    name       TEXT,
    avatar_url TEXT
);
```

A denormalised user display table. Populated from group member arrays (via `upsert_group`) and from message sender fields (via `insert_messages`). Used by the offline reader to render avatars and sender names without re-parsing `members_json` on every message.

Upsert uses `COALESCE(excluded.name, users.name)` and `COALESCE(excluded.avatar_url, users.avatar_url)` so a later sighting that lacks an avatar does not blank one already held. GroupMe sometimes omits `avatar_url` on users who have never set a photo; the COALESCE ensures the best-known value survives.

---

### `messages`

```sql
CREATE TABLE messages (
    id                TEXT PRIMARY KEY,
    id_sort           INTEGER NOT NULL,
    conversation_id   TEXT NOT NULL,
    source_guid       TEXT,
    user_id           TEXT,
    sender_id         TEXT,
    sender_type       TEXT,
    name              TEXT,
    avatar_url        TEXT,
    text              TEXT,
    created_at        INTEGER NOT NULL DEFAULT 0,
    system            INTEGER NOT NULL DEFAULT 0,
    platform          TEXT,
    favorited_by_json TEXT,
    attachments_json  TEXT,
    reactions_json    TEXT,
    event_json        TEXT,
    deleted_at        INTEGER,
    updated_at        INTEGER,
    raw_json          TEXT
);

CREATE INDEX idx_messages_conv_sort ON messages (conversation_id, id_sort DESC);
CREATE INDEX idx_messages_created   ON messages (created_at DESC);
```

**`id` vs `id_sort`.** GroupMe message IDs are decimal strings that exceed IEEE-754 double precision (2^53 ≈ 9e15; observed IDs reach ~1.78e17). Storing them as SQL `INTEGER` would require SQLite's 64-bit integer type, but the primary key is `TEXT` to preserve the exact string. A parallel `id_sort INTEGER` column holds the same value parsed to i64 (they top out near 1.8e17, well within the 9.2e18 i64 ceiling) solely so `ORDER BY id_sort` and cursor comparisons (`id_sort < ?`) are integer arithmetic rather than lexicographic string comparison. Lexicographic comparison of numeric strings of different lengths produces wrong ordering (`"9" > "100"`). The sort key is computed by `model::id_sort_key` and stored alongside every insert.

**Idempotent writes.** The upsert on `id` conflict updates mutable fields (`text`, `favorited_by_json`, `attachments_json`, `reactions_json`, `event_json`, `raw_json`). Immutable fields (`id_sort`, `conversation_id`, `created_at`) are not in the update list. The sync worker re-fetches overlapping page ranges routinely; an archive that double-counts on retry is worse than no archive.

**`deleted_at` and `updated_at` use COALESCE on upsert:**

```sql
deleted_at = COALESCE(excluded.deleted_at, messages.deleted_at),
updated_at = COALESCE(excluded.updated_at, messages.updated_at),
```

A backfill re-fetch of an old page range will not carry a deletion or edit that occurred after that page was originally served. Without COALESCE, the stale re-fetch would resurrect a deleted message or revert an edit. The COALESCE ensures that once a tombstone or edit timestamp is written, a null from a later re-fetch cannot overwrite it.

**Deleted messages keep their row.** `deleted_at` is a Unix timestamp written when a `message.deleted` event is applied. The row is never deleted. That a message existed and was removed is itself archival information — it lets the reader show "this message was deleted" rather than a silent gap. GroupMe also replaces `text` with a tombstone string (`"This message was deleted"`) on its own side; the `deleted_at` column is the reliable machine-readable signal.

**`system INTEGER`** stores a boolean as 0/1 (SQLite has no boolean type). System messages carry `event_json` describing the membership or moderation event; regular messages have `event_json = NULL`.

**`reactions_json` and `event_json`** are broken out of `raw_json` because the reader filters and renders on them. Parsing `raw_json` on every message to extract reactions would be wasteful at the scale of a full archive.

---

### `attachments`

```sql
CREATE TABLE attachments (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    url          TEXT,
    payload_json TEXT
);

CREATE INDEX idx_attachments_message ON attachments (message_id);
CREATE INDEX idx_attachments_url     ON attachments (url);
```

One row per attachment on a message. `kind` is one of `image`, `video`, `file`, `location`, `mentions`, `reply`, `emoji`, `other`. `url` is non-null only for attachment types that have a downloadable asset (`image`, `video`). `payload_json` is the full serialised `Attachment` enum variant.

Attachments are cleared and re-inserted on every message upsert (`DELETE FROM attachments WHERE message_id = ?` before the insert loop). This is simpler than a diff and correct: the only attachment mutation GroupMe supports is replacing all attachments on an edit, so a clean re-insert is the right semantic.

The `url` index exists to support `uncached_media_urls`, which joins against `media_cache` to find attachment URLs that have not been downloaded yet.

---

### `media_cache`

```sql
CREATE TABLE media_cache (
    url          TEXT PRIMARY KEY,
    local_path   TEXT NOT NULL,
    content_type TEXT,
    bytes        INTEGER,
    fetched_at   INTEGER
);
```

Records every downloaded media byte. `local_path` is relative to the app data directory (e.g. `blobs/abc123.jpeg`).

**Why the bytes must be downloaded.** Attachment URLs in GroupMe API responses point at `m.groupme.com`, which returns a `301` redirect to `cdn2.groupme.com` with an Azure Blob Storage SAS signature in the query string. That signature has an expiry (`se=` parameter). An archive that stores the `m.groupme.com` URL serves broken images offline and eventually broken images online too once the signature expires. The bytes must be fetched at sync time and stored locally. See [docs/groupme-api.md §7](groupme-api.md) for the full redirect analysis.

The `url` primary key makes `put_media` idempotent: re-downloading an asset that is already cached is a no-op on the record.

Avatars (`i.groupme.com`) are also fetched and indexed here via `uncached_avatar_urls`. Offline without avatars is a wall of blank placeholder icons, so they are treated with the same priority as message media.

---

### `sync_state`

```sql
CREATE TABLE sync_state (
    conversation_id   TEXT PRIMARY KEY,
    oldest_id         TEXT,
    newest_id         TEXT,
    backfill_complete INTEGER NOT NULL DEFAULT 0,
    last_sync_at      INTEGER
);
```

One row per conversation. Tracks where each cursor sits:

| Column | Meaning |
|---|---|
| `oldest_id` | Oldest message ID reached walking backwards with `before_id`. The next backfill page requests `before_id=oldest_id`. |
| `newest_id` | Newest message ID seen. The next tail pass requests `after_id=newest_id`. |
| `backfill_complete` | 1 once a `before_id` walk returns an empty page. Never reset to 0 — once the full history is held, it does not un-hold itself. |
| `last_sync_at` | Unix timestamp of the last successful sync cycle for this conversation. |

Cursors are `TEXT` (GroupMe IDs) rather than `INTEGER`. The sync code uses `id_sort_key` for integer comparison where ordering matters, but the cursor values are sent verbatim to the API as query parameters, so they must remain the exact original strings.

---

### `messages_fts`

```sql
CREATE VIRTUAL TABLE messages_fts USING fts5(
    text,
    content='messages',
    content_rowid='rowid',
    tokenize='unicode61'
);
```

An FTS5 external-content table. "External-content" means the index points at the `messages` table rather than storing a second copy of every message body. Message text is not duplicated on disk.

The index is kept in step by three triggers:

```sql
-- Insert
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, text) VALUES (new.rowid, new.text);
END;

-- Delete
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
END;

-- Update (delete old entry, insert new)
CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
    INSERT INTO messages_fts (rowid, text) VALUES (new.rowid, new.text);
END;
```

The update trigger removes the old text from the index before inserting the new. This means a re-fetch that upserts a message with edited text removes the superseded text from search results — the stale wording is no longer findable.

The `unicode61` tokenizer handles Unicode text correctly without requiring ICU. It folds case and handles Unicode word boundaries, which matters for a messaging app where emoji-heavy or non-ASCII messages are common.

SQLite is compiled from source via the `bundled` feature of `rusqlite`. This is deliberate: it removes the system-SQLite dependency on end-user machines (Windows ships an old SQLite that may not have FTS5 enabled) and pins the FTS5 version to a known-good build.

Queries use `WHERE messages_fts MATCH ?` with the standard FTS5 query syntax. Results are joined against `messages` (for metadata) and `conversations` (for the conversation name) — see `store.rs::search` for the full query.
