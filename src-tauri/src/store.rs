//! The local archive: schema, migrations, and every read/write path.
//!
//! Design notes that are load-bearing:
//!
//! * All GroupMe IDs are stored as `TEXT`. They exceed IEEE-754 integer
//!   precision, so anything that round-trips them through a float corrupts
//!   them. A parallel `id_sort INTEGER` column carries the same value parsed to
//!   i64 (they top out near 1.8e17, well under the 9.2e18 i64 ceiling) purely so
//!   ordering and cursor comparison happen in SQL instead of in string space.
//!
//! * Writes are idempotent. Sync re-fetches overlapping ranges routinely, and
//!   an archive that double-counts on a retry is worse than no archive.
//!
//! * `messages_fts` is an external-content FTS5 table kept in step by triggers.
//!   External-content means the text is not duplicated on disk.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::model::{
    id_sort_key, Chat, Conversation, ConversationKind, Group, Message,
};

/// Bump only alongside a matching arm in `migrate`.
pub const SCHEMA_VERSION: i32 = 1;

pub struct Store {
    conn: Connection,
}

/// How much of a conversation's history we have, and where the cursors sit.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncState {
    pub conversation_id: String,
    /// Oldest message ID reached walking backwards with `before_id`.
    pub oldest_id: Option<String>,
    /// Newest message ID seen; the `since_id` cursor for tailing.
    pub newest_id: Option<String>,
    /// Set once a `before_id` walk returns an empty page — the point at which
    /// we know we hold the entire history and never need to walk back again.
    pub backfill_complete: bool,
    pub last_sync_at: Option<i64>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating archive directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening archive at {}", path.display()))?;
        Self::from_conn(conn)
    }

    /// In-memory archive. Used by the test suite so the whole store layer is
    /// exercised without touching disk or the network.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // WAL so the sync worker writing does not block the UI reading.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i32 =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap_or(0);

        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        if current < 1 {
            tx.execute_batch(SCHEMA_V1)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---------------------------------------------------------------- meta

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    // ------------------------------------------------------- conversations

    pub fn upsert_group(&self, g: &Group, now: i64) -> Result<()> {
        let preview = g.messages.as_ref();
        self.conn.execute(
            "INSERT INTO conversations (
                id, kind, name, description, image_url, creator_user_id,
                created_at, updated_at, messages_count,
                last_message_id, last_message_text, last_message_created_at,
                members_json, raw_json, synced_at
             ) VALUES (?1,'group',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                image_url = excluded.image_url,
                updated_at = excluded.updated_at,
                messages_count = excluded.messages_count,
                last_message_id = excluded.last_message_id,
                last_message_text = excluded.last_message_text,
                last_message_created_at = excluded.last_message_created_at,
                members_json = excluded.members_json,
                raw_json = excluded.raw_json,
                synced_at = excluded.synced_at",
            params![
                g.id,
                g.name,
                g.description,
                g.image_url,
                g.creator_user_id,
                g.created_at,
                g.updated_at,
                g.messages_count.or_else(|| preview.and_then(|p| p.count)),
                preview.and_then(|p| p.last_message_id.clone()),
                preview
                    .and_then(|p| p.preview.as_ref())
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                preview.and_then(|p| p.last_message_created_at),
                serde_json::to_string(&g.members)?,
                serde_json::to_string(g)?,
                now,
            ],
        )?;

        for m in &g.members {
            if let Some(uid) = &m.user_id {
                self.upsert_user(
                    uid,
                    m.nickname.as_deref().or(m.name.as_deref()),
                    m.image_url.as_deref(),
                )?;
            }
        }
        Ok(())
    }

    pub fn upsert_chat(&self, c: &Chat, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (
                id, kind, name, image_url, created_at, updated_at, messages_count,
                last_message_id, last_message_text, last_message_created_at,
                raw_json, synced_at
             ) VALUES (?1,'dm',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                image_url = excluded.image_url,
                updated_at = excluded.updated_at,
                messages_count = excluded.messages_count,
                last_message_id = excluded.last_message_id,
                last_message_text = excluded.last_message_text,
                last_message_created_at = excluded.last_message_created_at,
                raw_json = excluded.raw_json,
                synced_at = excluded.synced_at",
            params![
                c.other_user.id,
                c.other_user.name,
                c.other_user.avatar_url,
                c.created_at,
                c.updated_at,
                c.messages_count,
                c.last_message.as_ref().map(|m| m.id.clone()),
                c.last_message.as_ref().and_then(|m| m.text.clone()),
                c.last_message.as_ref().map(|m| m.created_at),
                serde_json::to_string(c)?,
                now,
            ],
        )?;
        self.upsert_user(
            &c.other_user.id,
            c.other_user.name.as_deref(),
            c.other_user.avatar_url.as_deref(),
        )?;
        Ok(())
    }

    pub fn upsert_user(
        &self,
        id: &str,
        name: Option<&str>,
        avatar_url: Option<&str>,
    ) -> Result<()> {
        // COALESCE so a later sighting that lacks an avatar does not erase one
        // we already have.
        self.conn.execute(
            "INSERT INTO users (id, name, avatar_url) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                name = COALESCE(excluded.name, users.name),
                avatar_url = COALESCE(excluded.avatar_url, users.avatar_url)",
            params![id, name, avatar_url],
        )?;
        Ok(())
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, image_url, updated_at, messages_count,
                    last_message_text, last_message_created_at
             FROM conversations
             ORDER BY COALESCE(last_message_created_at, updated_at) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Conversation {
                id: r.get(0)?,
                kind: ConversationKind::parse(&r.get::<_, String>(1)?)
                    .unwrap_or(ConversationKind::Group),
                name: r.get(2)?,
                image_url: r.get(3)?,
                updated_at: r.get(4)?,
                messages_count: r.get(5)?,
                last_message_text: r.get(6)?,
                last_message_created_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn conversation_count(&self) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM conversations",
            [],
            |r| r.get(0),
        )?)
    }

    // ------------------------------------------------------------ messages

    /// Idempotent bulk insert. Re-inserting a page already held is a no-op on
    /// row count, which is what makes overlapping re-fetches safe.
    pub fn insert_messages(&mut self, conversation_id: &str, msgs: &[Message]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages (
                    id, id_sort, conversation_id, source_guid, user_id, sender_id,
                    sender_type, name, avatar_url, text, created_at, system,
                    platform, favorited_by_json,
                    reactions_json, event_json, deleted_at, updated_at, raw_json
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,
                           ?15,?16,?17,?18,?19)
                 ON CONFLICT(id) DO UPDATE SET
                    text = excluded.text,
                    favorited_by_json = excluded.favorited_by_json,
                    reactions_json = excluded.reactions_json,
                    event_json = excluded.event_json,
                    -- COALESCE: a re-fetch of an older page will not carry the
                    -- deletion, and must not resurrect a message we know is gone.
                    deleted_at = COALESCE(excluded.deleted_at, messages.deleted_at),
                    updated_at = COALESCE(excluded.updated_at, messages.updated_at),
                    raw_json = excluded.raw_json
                    -- Skip the update entirely when nothing changed. This
                    -- suppresses the FTS update trigger and the attachment
                    -- clear+reinsert on every overlapping re-fetch page.
                    WHERE messages.raw_json IS NOT excluded.raw_json",
            )?;
            // att_stmt sets `cached` from media_cache at insert time so that
            // URLs already downloaded (e.g. from a prior sync of the same
            // attachment) do not re-appear in uncached_media_urls after an
            // attachment row is replaced due to a genuine message edit.
            let mut att_stmt = tx.prepare(
                "INSERT INTO attachments (message_id, kind, url, cached)
                 VALUES (?1, ?2, ?3,
                     (SELECT COUNT(*) FROM media_cache WHERE url = ?3))",
            )?;
            let mut clear_att =
                tx.prepare("DELETE FROM attachments WHERE message_id = ?1")?;
            // Prepare once rather than recompiling on every message.
            let mut user_stmt = tx.prepare(
                "INSERT INTO users (id, name, avatar_url) VALUES (?1,?2,?3)
                 ON CONFLICT(id) DO UPDATE SET
                    name = COALESCE(excluded.name, users.name),
                    avatar_url = COALESCE(excluded.avatar_url, users.avatar_url)",
            )?;

            for m in msgs {
                let changed = stmt.execute(params![
                    m.id,
                    id_sort_key(&m.id),
                    conversation_id,
                    m.source_guid,
                    m.user_id,
                    m.sender_id.as_ref().or(m.user_id.as_ref()),
                    m.sender_type,
                    m.name,
                    m.avatar_url,
                    m.text,
                    m.created_at,
                    m.system as i32,
                    m.platform,
                    serde_json::to_string(&m.favorited_by)?,
                    serde_json::to_string(&m.reactions)?,
                    m.event.as_ref().map(serde_json::to_string).transpose()?,
                    m.deleted_at,
                    m.updated_at,
                    serde_json::to_string(m)?,
                ])?;
                inserted += changed;

                // Only churn attachments when the message row actually changed.
                // On a re-fetch of an unchanged page `changed` is 0 and the
                // existing attachment rows (with their cached flags) are preserved.
                if changed > 0 {
                    clear_att.execute(params![m.id])?;
                    for a in &m.attachments {
                        att_stmt.execute(params![
                            m.id,
                            a.kind(),
                            a.media_url(),
                        ])?;
                    }
                }

                if let (Some(uid), Some(name)) = (&m.user_id, &m.name) {
                    user_stmt.execute(params![uid, name, m.avatar_url])?;
                }
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Apply an edit or delete event to the message it targets.
    ///
    /// GroupMe delivers edits and deletions as *new system messages* rather
    /// than by mutating the original. A purely append-only archive would
    /// therefore keep serving stale text forever, and would keep showing
    /// content the sender deleted — which is the version of "wrong" that
    /// actually matters. The tombstone is retained: that a message existed and
    /// was removed is itself archival information.
    ///
    /// Returns whether a stored row was affected. `false` is normal and means
    /// the target predates what we hold.
    pub fn apply_event(&self, ev: &crate::model::SystemEvent) -> Result<bool> {
        let Some(target) = ev.target_message_id() else {
            return Ok(false);
        };
        let affected = match ev.kind.as_deref() {
            Some("message.update") => {
                let Some(text) = ev.updated_text() else {
                    return Ok(false);
                };
                self.conn.execute(
                    "UPDATE messages SET text = ?2, updated_at = ?3 WHERE id = ?1",
                    params![
                        target,
                        text,
                        ev.data.get("updated_at").and_then(|v| v.as_i64())
                    ],
                )?
            }
            Some("message.deleted") => self.conn.execute(
                "UPDATE messages SET deleted_at = ?2 WHERE id = ?1",
                params![target, ev.deleted_at()],
            )?,
            _ => 0,
        };
        Ok(affected > 0)
    }

    /// Newest-first page of a conversation. `before_sort` pages backwards.
    pub fn messages_page(
        &self,
        conversation_id: &str,
        limit: i64,
        before_sort: Option<i64>,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT raw_json FROM messages
             WHERE conversation_id = ?1 AND (?2 IS NULL OR id_sort < ?2)
             ORDER BY id_sort DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![conversation_id, before_sort, limit], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for raw in rows {
            // A single unparseable row must not sink the whole page.
            if let Ok(m) = serde_json::from_str::<Message>(&raw?) {
                out.push(m);
            }
        }
        Ok(out)
    }

    pub fn message_count(&self, conversation_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
            params![conversation_id],
            |r| r.get(0),
        )?)
    }

    pub fn total_message_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }

    /// Full-text search across the archive. This is the feature that makes it
    /// an archive rather than a cache.
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.conversation_id, c.name, m.name, m.text, m.created_at
             FROM messages_fts f
             JOIN messages m ON m.rowid = f.rowid
             LEFT JOIN conversations c ON c.id = m.conversation_id
             WHERE messages_fts MATCH ?1
             ORDER BY m.id_sort DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |r| {
            Ok(SearchHit {
                message_id: r.get(0)?,
                conversation_id: r.get(1)?,
                conversation_name: r.get(2)?,
                sender_name: r.get(3)?,
                text: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---------------------------------------------------------- sync state

    pub fn get_sync_state(&self, conversation_id: &str) -> Result<SyncState> {
        let found = self
            .conn
            .query_row(
                "SELECT oldest_id, newest_id, backfill_complete, last_sync_at
                 FROM sync_state WHERE conversation_id = ?1",
                params![conversation_id],
                |r| {
                    Ok(SyncState {
                        conversation_id: conversation_id.to_string(),
                        oldest_id: r.get(0)?,
                        newest_id: r.get(1)?,
                        backfill_complete: r.get::<_, i32>(2)? != 0,
                        last_sync_at: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(found.unwrap_or_else(|| SyncState {
            conversation_id: conversation_id.to_string(),
            ..Default::default()
        }))
    }

    pub fn put_sync_state(&self, s: &SyncState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state
                (conversation_id, oldest_id, newest_id, backfill_complete, last_sync_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(conversation_id) DO UPDATE SET
                oldest_id = excluded.oldest_id,
                newest_id = excluded.newest_id,
                backfill_complete = excluded.backfill_complete,
                last_sync_at = excluded.last_sync_at",
            params![
                s.conversation_id,
                s.oldest_id,
                s.newest_id,
                s.backfill_complete as i32,
                s.last_sync_at,
            ],
        )?;
        Ok(())
    }

    // --------------------------------------------------------- media cache

    pub fn put_media(
        &mut self,
        url: &str,
        local_path: &str,
        content_type: Option<&str>,
        bytes: i64,
        now: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO media_cache (url, local_path, content_type, bytes, fetched_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(url) DO UPDATE SET
                local_path = excluded.local_path,
                content_type = excluded.content_type,
                bytes = excluded.bytes,
                fetched_at = excluded.fetched_at",
            params![url, local_path, content_type, bytes, now],
        )?;
        // Keep the cached flag in attachments in sync with media_cache so that
        // uncached_media_urls can seek on (cached, url) rather than scanning
        // the whole table and joining. Both writes are in the same transaction
        // so they cannot diverge across a crash.
        tx.execute(
            "UPDATE attachments SET cached = 1 WHERE url = ?1",
            params![url],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_media(&self, url: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT local_path FROM media_cache WHERE url = ?1",
                params![url],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Remote asset URLs referenced by stored messages that are not yet cached.
    ///
    /// Uses an index seek on `(cached, url)` rather than a full table scan
    /// with LEFT JOIN. On a mature archive where most attachments are cached
    /// this is O(uncached) rather than O(all attachments).
    pub fn uncached_media_urls(&self, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT url FROM attachments
             WHERE cached = 0 AND url IS NOT NULL
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Avatar URLs (conversations + users) not yet cached. Offline without
    /// avatars is a wall of broken images, so these are fetched too.
    ///
    /// Avatar URLs come from `conversations.image_url` and `users.avatar_url`,
    /// not the `attachments` table, so adding a `cached` flag to those rows
    /// would require touching two additional upsert paths for limited gain.
    /// Both source tables are small (100+ conversations, bounded users), making
    /// the LEFT JOIN cost negligible compared with the ~750k-row attachments case.
    pub fn uncached_avatar_urls(&self, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            // `t.url` qualified: media_cache also has a `url` column, so a bare
            // `url` here is ambiguous and SQLite rejects the statement.
            "SELECT DISTINCT t.url FROM (
                SELECT image_url AS url FROM conversations WHERE image_url IS NOT NULL
                UNION
                SELECT avatar_url AS url FROM users WHERE avatar_url IS NOT NULL
             ) t
             LEFT JOIN media_cache mc ON mc.url = t.url
             WHERE mc.url IS NULL
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub message_id: String,
    pub conversation_id: String,
    pub conversation_name: Option<String>,
    pub sender_name: Option<String>,
    pub text: Option<String>,
    pub created_at: i64,
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

-- Groups and DMs share one table: the reader shows a single unified list, and
-- keeping them apart would mean a UNION on every read for no benefit.
CREATE TABLE IF NOT EXISTS conversations (
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

CREATE TABLE IF NOT EXISTS users (
    id         TEXT PRIMARY KEY,
    name       TEXT,
    avatar_url TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id                TEXT PRIMARY KEY,
    -- Same value as `id`, parsed to i64, so ordering and cursors are integer
    -- comparisons rather than lexicographic guesses.
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
    -- Undocumented fields from live traffic. Broken out of raw_json because the
    -- reader filters and renders on them.
    reactions_json    TEXT,
    event_json        TEXT,
    -- A deleted message keeps its row: that it existed and was removed is
    -- itself archival information. `text` becomes GroupMe's tombstone string.
    deleted_at        INTEGER,
    updated_at        INTEGER,
    raw_json          TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_conv_sort
    ON messages (conversation_id, id_sort DESC);
CREATE INDEX IF NOT EXISTS idx_messages_created
    ON messages (created_at DESC);

CREATE TABLE IF NOT EXISTS attachments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL,
    kind       TEXT NOT NULL,
    url        TEXT,
    -- 0 = URL not yet in media_cache; 1 = cached locally.
    -- Maintained by put_media (sets 1) and insert_messages (initialises from
    -- media_cache at insert time). Enables an index seek in uncached_media_urls
    -- instead of a full-table scan with LEFT JOIN on every sync cycle.
    cached     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_attachments_message ON attachments (message_id);
-- Covering index for the uncached_media_urls seek: WHERE cached=0 AND url IS NOT NULL
CREATE INDEX IF NOT EXISTS idx_attachments_cached ON attachments (cached, url);

CREATE TABLE IF NOT EXISTS media_cache (
    url          TEXT PRIMARY KEY,
    local_path   TEXT NOT NULL,
    content_type TEXT,
    bytes        INTEGER,
    fetched_at   INTEGER
);

CREATE TABLE IF NOT EXISTS sync_state (
    conversation_id   TEXT PRIMARY KEY,
    oldest_id         TEXT,
    newest_id         TEXT,
    backfill_complete INTEGER NOT NULL DEFAULT 0,
    last_sync_at      INTEGER
);

-- External-content FTS5: the index points at `messages` rather than storing a
-- second copy of every message body.
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    text,
    content='messages',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
    INSERT INTO messages_fts (rowid, text) VALUES (new.rowid, new.text);
END;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attachment, OtherUser};

    fn msg(id: &str, text: &str, created_at: i64) -> Message {
        Message {
            id: id.to_string(),
            user_id: Some("20000005".into()),
            name: Some("Test Sender".into()),
            text: Some(text.to_string()),
            created_at,
            ..Default::default()
        }
    }

    #[test]
    fn migrate_sets_schema_version() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migrate_is_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");
        {
            let s = Store::open(&path).unwrap();
            s.set_meta("hello", "world").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(s.get_meta("hello").unwrap().as_deref(), Some("world"));
    }

    #[test]
    fn meta_upserts_rather_than_duplicating() {
        let s = Store::open_in_memory().unwrap();
        s.set_meta("k", "v1").unwrap();
        s.set_meta("k", "v2").unwrap();
        assert_eq!(s.get_meta("k").unwrap().as_deref(), Some("v2"));
        assert!(s.get_meta("absent").unwrap().is_none());
    }

    #[test]
    fn reinserting_the_same_page_does_not_duplicate() {
        // The whole point of idempotent writes: sync re-fetches overlapping
        // ranges constantly and must not inflate the archive.
        let mut s = Store::open_in_memory().unwrap();
        let page = vec![msg("100", "one", 1), msg("101", "two", 2)];
        s.insert_messages("g1", &page).unwrap();
        s.insert_messages("g1", &page).unwrap();
        s.insert_messages("g1", &page).unwrap();
        assert_eq!(s.message_count("g1").unwrap(), 2);
    }

    #[test]
    fn messages_come_back_newest_first_by_id_not_string_order() {
        let mut s = Store::open_in_memory().unwrap();
        // Deliberately mixed digit lengths: lexicographic sort would put "99"
        // above "100", integer sort must not.
        s.insert_messages(
            "g1",
            &[msg("99", "older", 1), msg("100", "newer", 2)],
        )
        .unwrap();
        let page = s.messages_page("g1", 10, None).unwrap();
        assert_eq!(page[0].id, "100", "newest must sort first");
        assert_eq!(page[1].id, "99");
    }

    #[test]
    fn real_length_groupme_ids_order_correctly() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages(
            "g1",
            &[
                msg("170000000000000006", "older", 1),
                msg("170000000000000007", "newer", 2),
            ],
        )
        .unwrap();
        let page = s.messages_page("g1", 10, None).unwrap();
        assert_eq!(page[0].id, "170000000000000007");
    }

    #[test]
    fn messages_page_paginates_backwards() {
        let mut s = Store::open_in_memory().unwrap();
        let all: Vec<Message> = (1..=10).map(|i| msg(&i.to_string(), "x", i)).collect();
        s.insert_messages("g1", &all).unwrap();

        let first = s.messages_page("g1", 4, None).unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(first[0].id, "10");

        let cursor = id_sort_key(&first.last().unwrap().id);
        let second = s.messages_page("g1", 4, Some(cursor)).unwrap();
        assert_eq!(second[0].id, "6");
        // No overlap between pages.
        assert!(second.iter().all(|m| !first.iter().any(|f| f.id == m.id)));
    }

    #[test]
    fn messages_are_scoped_to_their_conversation() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages("g1", &[msg("1", "in g1", 1)]).unwrap();
        s.insert_messages("g2", &[msg("2", "in g2", 2)]).unwrap();
        assert_eq!(s.message_count("g1").unwrap(), 1);
        assert_eq!(s.message_count("g2").unwrap(), 1);
        assert_eq!(s.total_message_count().unwrap(), 2);
    }

    #[test]
    fn fts_finds_a_message_by_word() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages(
            "g1",
            &[
                msg("1", "the quick brown fox", 1),
                msg("2", "completely unrelated", 2),
            ],
        )
        .unwrap();
        let hits = s.search("brown", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "1");
    }

    #[test]
    fn fts_index_follows_an_edit() {
        // Upserting a message must not leave the old text searchable.
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages("g1", &[msg("1", "original wording", 1)])
            .unwrap();
        assert_eq!(s.search("original", 10).unwrap().len(), 1);

        s.insert_messages("g1", &[msg("1", "replacement wording", 1)])
            .unwrap();
        assert_eq!(
            s.search("original", 10).unwrap().len(),
            0,
            "stale FTS row survived an update"
        );
        assert_eq!(s.search("replacement", 10).unwrap().len(), 1);
    }

    #[test]
    fn fts_joins_conversation_name_into_hits() {
        let mut s = Store::open_in_memory().unwrap();
        let g = Group {
            id: "g1".into(),
            name: Some("Family Chat".into()),
            ..Default::default()
        };
        s.upsert_group(&g, 0).unwrap();
        s.insert_messages("g1", &[msg("1", "findme", 1)]).unwrap();
        let hits = s.search("findme", 10).unwrap();
        assert_eq!(hits[0].conversation_name.as_deref(), Some("Family Chat"));
    }

    #[test]
    fn message_with_no_text_does_not_break_insert_or_search() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "", 1);
        m.text = None;
        s.insert_messages("g1", &[m]).unwrap();
        assert_eq!(s.message_count("g1").unwrap(), 1);
        assert_eq!(s.search("anything", 10).unwrap().len(), 0);
    }

    #[test]
    fn attachments_are_indexed_and_not_duplicated_on_reinsert() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "pic", 1);
        m.attachments = vec![Attachment::Image {
            url: Some("https://i.groupme.com/a.png".into()),
            source_url: None,
            blur_hash: None,
        }];
        s.insert_messages("g1", &[m.clone()]).unwrap();
        s.insert_messages("g1", &[m]).unwrap();

        let urls = s.uncached_media_urls(10).unwrap();
        assert_eq!(urls, vec!["https://i.groupme.com/a.png".to_string()]);
    }

    #[test]
    fn cached_media_drops_out_of_the_uncached_list() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "pic", 1);
        m.attachments = vec![Attachment::Image {
            url: Some("https://i.groupme.com/a.png".into()),
            source_url: None,
            blur_hash: None,
        }];
        s.insert_messages("g1", &[m]).unwrap();
        assert_eq!(s.uncached_media_urls(10).unwrap().len(), 1);

        s.put_media(
            "https://i.groupme.com/a.png",
            "blobs/a.png",
            Some("image/png"),
            123,
            0,
        )
        .unwrap();
        assert!(s.uncached_media_urls(10).unwrap().is_empty());
        assert_eq!(
            s.get_media("https://i.groupme.com/a.png").unwrap().as_deref(),
            Some("blobs/a.png")
        );
    }

    #[test]
    fn non_media_attachments_are_not_queued_for_download() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "reply", 1);
        m.attachments = vec![Attachment::Reply {
            user_id: Some("9".into()),
            reply_id: Some("8".into()),
            base_reply_id: Some("8".into()),
        }];
        s.insert_messages("g1", &[m]).unwrap();
        assert!(s.uncached_media_urls(10).unwrap().is_empty());
    }

    #[test]
    fn avatars_are_queued_for_offline_caching() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_user("1", Some("A"), Some("https://i.groupme.com/av.png"))
            .unwrap();
        let urls = s.uncached_avatar_urls(10).unwrap();
        assert!(urls.contains(&"https://i.groupme.com/av.png".to_string()));
    }

    #[test]
    fn user_upsert_does_not_erase_a_known_avatar() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_user("1", Some("A"), Some("https://i.groupme.com/av.png"))
            .unwrap();
        // Later sighting with no avatar — must not blank the stored one.
        s.upsert_user("1", Some("A Renamed"), None).unwrap();
        let urls = s.uncached_avatar_urls(10).unwrap();
        assert!(urls.contains(&"https://i.groupme.com/av.png".to_string()));
    }

    #[test]
    fn conversations_list_sorts_by_recency() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "old".into(),
                name: Some("Old".into()),
                updated_at: 100,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.upsert_chat(
            &Chat {
                updated_at: 900,
                other_user: OtherUser {
                    id: "dm1".into(),
                    name: Some("Recent Person".into()),
                    avatar_url: None,
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let list = s.list_conversations().unwrap();
        assert_eq!(list[0].id, "dm1");
        assert_eq!(list[0].kind, ConversationKind::Dm);
        assert_eq!(list[1].kind, ConversationKind::Group);
    }

    #[test]
    fn group_upsert_updates_rather_than_duplicates() {
        let s = Store::open_in_memory().unwrap();
        let mut g = Group {
            id: "g1".into(),
            name: Some("First Name".into()),
            ..Default::default()
        };
        s.upsert_group(&g, 0).unwrap();
        g.name = Some("Renamed".into());
        s.upsert_group(&g, 1).unwrap();

        let list = s.list_conversations().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name.as_deref(), Some("Renamed"));
    }

    // --- Edit / delete events -------------------------------------------

    #[test]
    fn edit_event_rewrites_the_stored_body_and_the_search_index() {
        // The scenario that matters: backfill archived the original, and the
        // edit only shows up later on a tailing pass.
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages("g1", &[msg("170000000000000003", "original wording", 1)])
            .unwrap();
        assert_eq!(s.search("original", 10).unwrap().len(), 1);

        let ev: crate::model::SystemEvent = serde_json::from_str(
            r#"{"type":"message.update","data":{
                "message_id":"170000000000000003","updated_at":1784499085,
                "message":{"text":"corrected wording","attachments":[]}}}"#,
        )
        .unwrap();
        assert!(s.apply_event(&ev).unwrap());

        let page = s.messages_page("g1", 10, None).unwrap();
        assert_eq!(page.len(), 1, "edit must not create a second row");
        assert_eq!(
            s.search("original", 10).unwrap().len(),
            0,
            "superseded text is still searchable"
        );
        assert_eq!(s.search("corrected", 10).unwrap().len(), 1);
    }

    #[test]
    fn delete_event_tombstones_rather_than_dropping_the_row() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages("g1", &[msg("170000000000000005", "since deleted", 1)])
            .unwrap();

        let ev: crate::model::SystemEvent = serde_json::from_str(
            r#"{"type":"message.deleted","data":{
                "message_id":"170000000000000005","deleted_at":1784663704,
                "deletion_actor":"sender"}}"#,
        )
        .unwrap();
        assert!(s.apply_event(&ev).unwrap());

        let deleted_at: Option<i64> = s
            .conn
            .query_row(
                "SELECT deleted_at FROM messages WHERE id = ?1",
                params!["170000000000000005"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted_at, Some(1784663704));
        assert_eq!(
            s.message_count("g1").unwrap(),
            1,
            "row must survive; the deletion is itself archival"
        );
    }

    #[test]
    fn event_for_an_unheld_message_is_a_clean_no_op() {
        let s = Store::open_in_memory().unwrap();
        let ev: crate::model::SystemEvent = serde_json::from_str(
            r#"{"type":"message.deleted","data":{"message_id":"999","deleted_at":1}}"#,
        )
        .unwrap();
        assert!(!s.apply_event(&ev).unwrap());
    }

    #[test]
    fn membership_events_are_ignored_by_apply_event() {
        let s = Store::open_in_memory().unwrap();
        let ev: crate::model::SystemEvent = serde_json::from_str(
            r#"{"type":"membership.announce.joined","data":{"user":{"id":1,"nickname":"X"}}}"#,
        )
        .unwrap();
        assert!(!s.apply_event(&ev).unwrap());
    }

    #[test]
    fn refetching_an_old_page_does_not_resurrect_a_deleted_message() {
        // Backfill re-reads ranges constantly. A page fetched before the
        // deletion still shows the message as live; that must not win.
        let mut s = Store::open_in_memory().unwrap();
        let original = msg("170000000000000005", "since deleted", 1);
        s.insert_messages("g1", &[original.clone()]).unwrap();

        let ev: crate::model::SystemEvent = serde_json::from_str(
            r#"{"type":"message.deleted","data":{
                "message_id":"170000000000000005","deleted_at":1784663704}}"#,
        )
        .unwrap();
        s.apply_event(&ev).unwrap();

        s.insert_messages("g1", &[original]).unwrap();

        let deleted_at: Option<i64> = s
            .conn
            .query_row(
                "SELECT deleted_at FROM messages WHERE id = ?1",
                params!["170000000000000005"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            deleted_at,
            Some(1784663704),
            "a stale re-fetch un-deleted the message"
        );
    }

    #[test]
    fn reactions_and_events_survive_the_round_trip() {
        let mut s = Store::open_in_memory().unwrap();
        let m: Message = serde_json::from_str(
            r#"{"id":"1","text":"reacted to","favorited_by":["7","8"],
                "reactions":[{"type":"unicode","code":"🤣","user_ids":["7","8"]}]}"#,
        )
        .unwrap();
        s.insert_messages("g1", &[m]).unwrap();

        let back = s.messages_page("g1", 10, None).unwrap();
        assert_eq!(back[0].reactions.len(), 1);
        assert_eq!(back[0].reactions[0].display_char(), Some("🤣"));
        assert_eq!(back[0].reaction_count(), 2);
    }

    #[test]
    fn system_message_with_numeric_event_ids_stores_cleanly() {
        let mut s = Store::open_in_memory().unwrap();
        let m: Message = serde_json::from_str(
            r#"{"id":"1","system":true,"user_id":"system","sender_id":"system",
                "text":"X has left the group.",
                "event":{"type":"membership.notifications.exited",
                         "data":{"removed_user":{"id":20000001,"nickname":"X"}}}}"#,
        )
        .unwrap();
        s.insert_messages("g1", &[m]).unwrap();
        let back = s.messages_page("g1", 10, None).unwrap();
        assert_eq!(
            back[0].event.as_ref().unwrap().subject_user_id().as_deref(),
            Some("20000001")
        );
    }

    #[test]
    fn sync_state_defaults_are_safe_for_a_fresh_conversation() {
        let s = Store::open_in_memory().unwrap();
        let st = s.get_sync_state("never-seen").unwrap();
        assert!(st.oldest_id.is_none());
        assert!(st.newest_id.is_none());
        assert!(!st.backfill_complete, "must not claim a complete backfill");
    }

    #[test]
    fn sync_state_round_trips() {
        let s = Store::open_in_memory().unwrap();
        let st = SyncState {
            conversation_id: "g1".into(),
            oldest_id: Some("100".into()),
            newest_id: Some("200".into()),
            backfill_complete: true,
            last_sync_at: Some(1700000000),
        };
        s.put_sync_state(&st).unwrap();
        let back = s.get_sync_state("g1").unwrap();
        assert_eq!(back.oldest_id.as_deref(), Some("100"));
        assert_eq!(back.newest_id.as_deref(), Some("200"));
        assert!(back.backfill_complete);
    }

    #[test]
    fn put_media_flips_cached_flag_so_url_leaves_queue() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "pic", 1);
        m.attachments = vec![Attachment::Image {
            url: Some("https://i.groupme.com/b.png".into()),
            source_url: None,
            blur_hash: None,
        }];
        s.insert_messages("g1", &[m]).unwrap();
        assert_eq!(s.uncached_media_urls(10).unwrap().len(), 1);

        s.put_media("https://i.groupme.com/b.png", "blobs/b.png", None, 0, 0)
            .unwrap();

        assert!(s.uncached_media_urls(10).unwrap().is_empty());
        let cached: i32 = s
            .conn
            .query_row(
                "SELECT cached FROM attachments WHERE url = ?1",
                params!["https://i.groupme.com/b.png"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cached, 1, "cached flag must be 1 after put_media");
    }

    #[test]
    fn unchanged_reinsert_does_not_churn_attachments() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = msg("1", "pic", 1);
        m.attachments = vec![Attachment::Image {
            url: Some("https://i.groupme.com/c.png".into()),
            source_url: None,
            blur_hash: None,
        }];
        s.insert_messages("g1", &[m.clone()]).unwrap();

        let att_id: i64 = s
            .conn
            .query_row(
                "SELECT id FROM attachments WHERE message_id = '1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Re-insert identical message — must not delete+reinsert the attachment row.
        s.insert_messages("g1", &[m]).unwrap();

        let att_id2: i64 = s
            .conn
            .query_row(
                "SELECT id FROM attachments WHERE message_id = '1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            att_id, att_id2,
            "attachment row was deleted and reinserted on an unchanged message"
        );
    }
}
