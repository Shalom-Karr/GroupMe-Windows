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
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::model::{id_sort_key, Chat, Conversation, ConversationKind, Group, Member, Message};

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bump only alongside a matching arm in `migrate`.
pub const SCHEMA_VERSION: i32 = 4;

/// v2 adds server read state to `conversations`.
///
/// Written as `ALTER TABLE` rather than folded into `SCHEMA_V1` because existing
/// archives are multiple gigabytes: they must gain the columns in place, not be
/// rebuilt. `SCHEMA_V1` also declares them, so a fresh database arrives at the
/// same shape without running this — hence the errors below are ignored
/// individually, since "duplicate column name" is the expected outcome there.
const SCHEMA_V2: &[&str] = &[
    "ALTER TABLE conversations ADD COLUMN unread_count INTEGER",
    "ALTER TABLE conversations ADD COLUMN last_read_message_id TEXT",
    "ALTER TABLE conversations ADD COLUMN last_read_at INTEGER",
];

/// v3 adds local pin ordering and a local mute flag to `conversations`. Both are
/// this app's own state — neither is synced from GroupMe — so they live only
/// here. Same in-place `ALTER TABLE` reasoning as [`SCHEMA_V2`]: an existing
/// archive gains the columns without a rebuild, while `SCHEMA_V1` also declares
/// them so a fresh database is correct without running these, and the
/// "duplicate column name" error on that path is ignored individually.
const SCHEMA_V3: &[&str] = &[
    "ALTER TABLE conversations ADD COLUMN pin_rank INTEGER",
    "ALTER TABLE conversations ADD COLUMN muted INTEGER NOT NULL DEFAULT 0",
];

/// v4 adds a `former` flag to `conversations` that marks groups the account has
/// left. The flag is written by [`Store::mark_former_groups`] after a successful
/// live-list fetch, so the history stays archived while the sidebar can grey out
/// the entry. Same in-place `ALTER TABLE` reasoning as `SCHEMA_V2` / `SCHEMA_V3`:
/// `SCHEMA_V1` already declares the column so a fresh database arrives at the
/// correct shape without running this, and "duplicate column name" on that path
/// is ignored.
const SCHEMA_V4: &[&str] =
    &["ALTER TABLE conversations ADD COLUMN former INTEGER NOT NULL DEFAULT 0"];

pub struct Store {
    conn: Connection,
    /// Database file path. `None` for in-memory stores; `open_readonly` errors
    /// in that case rather than returning a useless shared in-memory connection.
    db_path: Option<std::path::PathBuf>,
}

/// Conversation and message totals split by conversation kind.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct KindCounts {
    pub groups: usize,
    pub dms: usize,
    pub group_messages: i64,
    pub dm_messages: i64,
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
        Self::from_conn(conn, Some(path.to_path_buf()))
    }

    /// In-memory archive. Used by the test suite so the whole store layer is
    /// exercised without touching disk or the network.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?, None)
    }

    fn from_conn(conn: Connection, db_path: Option<std::path::PathBuf>) -> Result<Self> {
        // WAL so the sync worker writing does not block the UI reading.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Memory is bounded explicitly rather than left to defaults, because
        // this archive reaches multiple gigabytes and every one of these knobs
        // scales with the file rather than with the working set.
        //
        // A negative cache_size is KiB rather than pages, so this is 8 MiB
        // regardless of page size. The access pattern is a 60-row page at the
        // tip of one conversation plus FTS lookups; a larger cache buys almost
        // nothing and a smaller one starts thrashing the FTS index.
        conn.pragma_update(None, "cache_size", -8_000)?;

        // Cap the WAL instead of letting it grow to match a long sync burst.
        // The default autocheckpoint is 1000 pages, but nothing truncates the
        // file afterwards, so a first-run backfill of 142k messages leaves a
        // permanently large -wal alongside the archive.
        conn.pragma_update(None, "journal_size_limit", 16 * 1024 * 1024)?;

        // Scratch space for FTS merges and ORDER BY spills goes to memory up to
        // a hard ceiling, then to a temp file — never unbounded RSS.
        conn.pragma_update(None, "temp_store", "FILE")?;
        conn.pragma_update(None, "soft_heap_limit", 64 * 1024 * 1024)?;

        // Deliberately NOT setting mmap_size. Memory-mapping a multi-gigabyte
        // archive charges every page touched to the process working set, which
        // is exactly the number a user watching Task Manager reacts to, and it
        // buys throughput this app does not need — it reads 60 rows at a time.
        let mut store = Self { conn, db_path };
        store.migrate()?;
        Ok(store)
    }

    /// Opens a new read-only connection to the same database file so heavy
    /// analytic queries can run without holding the shared mutex.
    ///
    /// Returns `Err` for in-memory stores (no path to open) or any OS error.
    /// Callers must fall back to the locked path on error.
    ///
    /// WAL note: a read-only connection in WAL mode sees a consistent snapshot
    /// at the time it begins reading and never blocks the writer or other
    /// readers — that is precisely why heavy queries use this instead of the
    /// main connection.
    pub fn open_readonly(&self) -> Result<Connection> {
        let path = self
            .db_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no side connection for in-memory store"))?;
        // SQLITE_OPEN_READ_ONLY so SQLite enforces the intent at the OS level.
        // SQLITE_OPEN_NO_MUTEX because this connection is used from a single
        // thread inside spawn_blocking — the default serialized mode adds lock
        // overhead with no benefit here.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // WAL: must be set so the connection participates in the WAL protocol.
        // Other pragmas mirror the main connection where relevant for reads;
        // write-only ones (synchronous, foreign_keys, journal_size_limit) are
        // omitted.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "cache_size", -4_000i32)?;
        conn.pragma_update(None, "temp_store", "FILE")?;
        conn.pragma_update(None, "busy_timeout", 5_000i32)?;
        Ok(conn)
    }

    /// Raw connection accessor for the analytics fallback path in `commands.rs`.
    /// Not part of the public API; callers must prefer `open_readonly`.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);

        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        if current < 1 {
            tx.execute_batch(SCHEMA_V1)?;
        }
        if current >= 1 {
            // Only an existing v1 archive needs these added; a fresh database
            // already has them from SCHEMA_V1. Each is attempted alone and its
            // error ignored, so a half-applied migration from an interrupted
            // run completes instead of failing on the column it already added.
            for stmt in SCHEMA_V2 {
                let _ = tx.execute(stmt, []);
            }
        }
        if current >= 1 {
            // v1/v2 -> v3: pin ordering and the mute flag. Same
            // ignore-per-statement handling as SCHEMA_V2 — a fresh database
            // already carries these from SCHEMA_V1, and an existing archive that
            // somehow already has one column must still gain the other.
            for stmt in SCHEMA_V3 {
                let _ = tx.execute(stmt, []);
            }
        }
        if current >= 1 {
            // v1/v2/v3 -> v4: former flag for groups the account has left.
            // Same in-place upgrade pattern: a fresh database already has this
            // column from SCHEMA_V1, and "duplicate column name" is ignored.
            for stmt in SCHEMA_V4 {
                let _ = tx.execute(stmt, []);
            }
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
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
                members_json, raw_json, synced_at,
                unread_count, last_read_message_id, last_read_at
             ) VALUES (?1,'group',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
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
                synced_at = excluded.synced_at,
                -- A successful group-detail fetch proves the account is still a
                -- member; reset the former flag so a rejoin is reflected immediately
                -- rather than waiting for the next live-list mark_former_groups call.
                former = 0,
                -- COALESCE, not a plain overwrite: `GET /v3/groups` omits read
                -- state on most groups, so a list sync would otherwise erase the
                -- values a single-group fetch had filled in.
                unread_count = COALESCE(excluded.unread_count, conversations.unread_count),
                last_read_message_id =
                    COALESCE(excluded.last_read_message_id, conversations.last_read_message_id),
                last_read_at = COALESCE(excluded.last_read_at, conversations.last_read_at)",
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
                g.unread_count,
                g.last_read_message_id,
                g.last_read_at,
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

    /// Upserts a group's METADATA only, from the lean `omit=memberships` list.
    ///
    /// It never writes `members_json` — NULL on insert, untouched on conflict —
    /// so [`Self::groups_needing_members`] can find groups whose roster has not
    /// been fetched, and so a lean list sync never wipes a roster a per-group
    /// [`Self::upsert_group`] already stored. `raw_json` is likewise preserved on
    /// conflict: the lean object has no members, and the full one from
    /// `group_detail` is the copy worth keeping.
    pub fn upsert_group_meta(&self, g: &Group, now: i64) -> Result<()> {
        let preview = g.messages.as_ref();
        self.conn.execute(
            "INSERT INTO conversations (
                id, kind, name, description, image_url, creator_user_id,
                created_at, updated_at, messages_count,
                last_message_id, last_message_text, last_message_created_at,
                members_json, raw_json, synced_at,
                unread_count, last_read_message_id, last_read_at
             ) VALUES (?1,'group',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,?12,?13,?14,?15,?16)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                image_url = excluded.image_url,
                updated_at = excluded.updated_at,
                messages_count = excluded.messages_count,
                last_message_id = excluded.last_message_id,
                last_message_text = excluded.last_message_text,
                last_message_created_at = excluded.last_message_created_at,
                synced_at = excluded.synced_at,
                -- Rows from the lean list endpoint come only from the live member
                -- list, so appearance here proves current membership. Reset former
                -- so a rejoin is visible as soon as the list is refreshed — well
                -- before mark_former_groups runs for the current cycle.
                former = 0,
                -- members_json and raw_json are deliberately not touched here.
                unread_count = COALESCE(excluded.unread_count, conversations.unread_count),
                last_read_message_id =
                    COALESCE(excluded.last_read_message_id, conversations.last_read_message_id),
                last_read_at = COALESCE(excluded.last_read_at, conversations.last_read_at)",
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
                serde_json::to_string(g)?,
                now,
                g.unread_count,
                g.last_read_message_id,
                g.last_read_at,
            ],
        )?;
        Ok(())
    }

    /// Group ids whose member roster is not stored yet (the lean list leaves
    /// `members_json` NULL). Each is filled by one `group_detail` call. Ordered
    /// most-recently-active first so the groups likely to be opened fill in
    /// soonest; `limit` bounds the per-cycle burst.
    pub fn groups_needing_members(&self, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM conversations
             WHERE kind = 'group' AND members_json IS NULL
             ORDER BY COALESCE(last_message_created_at, updated_at) DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn upsert_chat(&self, c: &Chat, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (
                id, kind, name, image_url, created_at, updated_at, messages_count,
                last_message_id, last_message_text, last_message_created_at,
                raw_json, synced_at,
                unread_count, last_read_message_id, last_read_at
             ) VALUES (?1,'dm',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                image_url = excluded.image_url,
                updated_at = excluded.updated_at,
                messages_count = excluded.messages_count,
                last_message_id = excluded.last_message_id,
                last_message_text = excluded.last_message_text,
                last_message_created_at = excluded.last_message_created_at,
                raw_json = excluded.raw_json,
                synced_at = excluded.synced_at,
                -- See upsert_group: absent read state must not erase known state.
                unread_count = COALESCE(excluded.unread_count, conversations.unread_count),
                last_read_message_id =
                    COALESCE(excluded.last_read_message_id, conversations.last_read_message_id),
                last_read_at = COALESCE(excluded.last_read_at, conversations.last_read_at)",
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
                c.unread_count,
                c.last_read_message_id,
                c.last_read_at,
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
                    last_message_text, last_message_created_at,
                    unread_count, last_read_message_id,
                    pin_rank, muted, former
             FROM conversations
             -- Pinned first (NULL pin_rank sorts last), then by rank; within each
             -- tier former groups sink below current ones, then recency descending.
             ORDER BY (pin_rank IS NULL), pin_rank,
                      former,
                      COALESCE(last_message_created_at, updated_at) DESC",
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
                unread_count: r.get(8)?,
                last_read_message_id: r.get(9)?,
                pin_rank: r.get(10)?,
                muted: r.get::<_, i64>(11)? != 0,
                former: r.get::<_, i64>(12)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Applies read state from `GET /v4/read_receipts`.
    ///
    /// Receipts key DMs by the `+`-joined thread key while this table keys them
    /// by the other participant's user id, so `my_user_id` is needed to resolve
    /// the row. A receipt for a conversation we have not archived is ignored
    /// rather than inserted: a bare id carries no kind, name or timestamps, and a
    /// half-built row would show up in the sidebar as a blank conversation.
    ///
    /// Returns how many rows were actually updated.
    pub fn apply_read_receipts(
        &self,
        receipts: &[(String, Option<String>)],
        my_user_id: &str,
    ) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "UPDATE conversations
                SET last_read_message_id = ?2,
                    unread_count = CASE
                        -- Derived, not guessed: if the last read message is the
                        -- newest we hold, there is nothing unread. Otherwise
                        -- leave the count unknown (NULL) rather than inventing a
                        -- number, and let the UI compare ids instead.
                        WHEN ?2 IS NOT NULL AND last_message_id IS NOT NULL
                             AND ?2 = last_message_id THEN 0
                        ELSE NULL
                    END
              WHERE id = ?1",
        )?;

        let mut updated = 0;
        for (conversation_id, last_read) in receipts {
            let key = match conversation_id.split_once('+') {
                Some((a, b)) => {
                    // A note-to-self thread is "<me>+<me>", so both halves
                    // matching is not an error.
                    if a == my_user_id {
                        b.to_string()
                    } else if b == my_user_id {
                        a.to_string()
                    } else {
                        // Not our thread; nothing sane to map it to.
                        continue;
                    }
                }
                None => conversation_id.clone(),
            };
            updated += stmt.execute(params![key, last_read])?;
        }
        Ok(updated)
    }

    pub fn conversation_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?)
    }

    /// Conversation and message totals split by kind, so the sync panel can show
    /// groups and DMs separately — which matters when one is fully archived and
    /// the other is blocked by a network filter.
    pub fn counts_by_kind(&self) -> Result<KindCounts> {
        let conv = |kind: &str| -> Result<usize> {
            Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM conversations WHERE kind = ?1",
                params![kind],
                |r| r.get(0),
            )?)
        };
        let msgs = |kind: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM messages m
                 JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.kind = ?1",
                params![kind],
                |r| r.get(0),
            )?)
        };
        Ok(KindCounts {
            groups: conv("group")?,
            dms: conv("dm")?,
            group_messages: msgs("group")?,
            dm_messages: msgs("dm")?,
        })
    }

    // ----------------------------------------------------- pins and mute

    /// Set (or clear, with `None`) a conversation's local pin rank.
    pub fn set_pin(&self, conversation_id: &str, rank: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE conversations SET pin_rank = ?2 WHERE id = ?1",
            params![conversation_id, rank],
        )?;
        Ok(())
    }

    /// Assigns `pin_rank` 0..n to `ordered_ids` in order, in one transaction.
    /// Conversations not named here keep whatever pin state they already have —
    /// clearing them is deliberately not this method's job.
    pub fn reorder_pins(&mut self, ordered_ids: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE conversations SET pin_rank = ?2 WHERE id = ?1")?;
            for (i, id) in ordered_ids.iter().enumerate() {
                stmt.execute(params![id, i as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Set the local mute flag on a conversation.
    pub fn set_mute(&self, conversation_id: &str, muted: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE conversations SET muted = ?2 WHERE id = ?1",
            params![conversation_id, muted as i64],
        )?;
        Ok(())
    }

    /// Whether a conversation is locally muted. A conversation we do not hold is
    /// not muted (rather than an error), so a notification for one still shows.
    pub fn is_muted(&self, conversation_id: &str) -> Result<bool> {
        let muted: Option<i64> = self
            .conn
            .query_row(
                "SELECT muted FROM conversations WHERE id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(muted.unwrap_or(0) != 0)
    }

    /// Marks groups absent from `present_ids` as former, and resets those present
    /// to not-former. Must only be called when the live-list fetch succeeded — a
    /// filtered or offline cycle must never mark everything former.
    ///
    /// Uses a two-step UPDATE (mark all, then unmark present ones) chunked into
    /// batches of 500 to stay well under SQLite's 999-parameter limit, then wraps
    /// both steps in one transaction so the archive is never half-updated.
    pub fn mark_former_groups(&mut self, present_ids: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;

        // Mark every group as former first; re-active ones are cleared below.
        tx.execute(
            "UPDATE conversations SET former = 1 WHERE kind = 'group'",
            [],
        )?;

        // Unmark groups that are still in the live list, in parameter-safe chunks.
        for chunk in present_ids.chunks(500) {
            let placeholders = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "UPDATE conversations SET former = 0 \
                 WHERE kind = 'group' AND id IN ({placeholders})"
            );
            tx.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?;
        }

        tx.commit()?;
        Ok(())
    }

    /// The stored kind of a conversation, or `None` if we do not hold it. Lets a
    /// command tell a group from a DM without the caller stating which — needed
    /// because a DM is keyed by the other participant's bare user id, so its id
    /// is shape-identical to a group id.
    pub fn conversation_kind(&self, conversation_id: &str) -> Result<Option<ConversationKind>> {
        let kind: Option<String> = self
            .conn
            .query_row(
                "SELECT kind FROM conversations WHERE id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(kind.as_deref().and_then(ConversationKind::parse))
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
            let mut clear_att = tx.prepare("DELETE FROM attachments WHERE message_id = ?1")?;
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
                        att_stmt.execute(params![m.id, a.kind(), a.media_url(),])?;
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
        // `raw_json` is patched alongside the columns because every reader —
        // pages, search hits, the client UI — rebuilds messages from it. A
        // column-only update leaves the stored JSON serving the pre-event
        // content, which for a deletion means continuing to show text the
        // sender removed.
        let affected = match ev.kind.as_deref() {
            Some("message.update") => {
                let Some(text) = ev.updated_text() else {
                    return Ok(false);
                };
                let updated_at = ev
                    .data
                    .get("updated_at")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_else(now_unix);
                self.conn.execute(
                    "UPDATE messages SET text = ?2, updated_at = ?3,
                        raw_json = json_set(raw_json, '$.text', ?2, '$.updated_at', ?3)
                     WHERE id = ?1",
                    params![target, text, updated_at],
                )?
            }
            Some("message.deleted") => {
                // A concrete timestamp even when the frame omits one: a JSON
                // null here would read back as "not deleted".
                let deleted_at = ev.deleted_at().unwrap_or_else(now_unix);
                self.conn.execute(
                    "UPDATE messages SET deleted_at = ?2,
                        raw_json = json_set(raw_json, '$.deleted_at', ?2)
                     WHERE id = ?1",
                    params![target, deleted_at],
                )?
            }
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

    /// The id of the newest message in a conversation at or before `before_unix`
    /// (compared on the stored `created_at`), for a "jump to date" picker.
    ///
    /// Returns the id as a **string**: GroupMe ids exceed 2^53 and must never be
    /// parsed to a JS number. `None` when nothing in the conversation is that old.
    pub fn message_near_date(
        &self,
        conversation_id: &str,
        before_unix: i64,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM messages
                 WHERE conversation_id = ?1 AND created_at <= ?2
                 ORDER BY created_at DESC, id_sort DESC
                 LIMIT 1",
                params![conversation_id, before_unix],
                |r| r.get(0),
            )
            .optional()?)
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
        self.search_scoped(query, None, limit)
    }

    /// [`Store::search`], optionally scoped to a single conversation. `None`
    /// searches the whole archive; `Some(id)` restricts to that conversation.
    pub fn search_scoped(
        &self,
        query: &str,
        conversation_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.conversation_id, c.name, m.name, m.text, m.created_at
             FROM messages_fts f
             JOIN messages m ON m.rowid = f.rowid
             LEFT JOIN conversations c ON c.id = m.conversation_id
             WHERE messages_fts MATCH ?1
               AND (?2 IS NULL OR m.conversation_id = ?2)
             ORDER BY m.id_sort DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![query, conversation_id, limit], |r| {
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

    pub(crate) fn user_profile_on(conn: &Connection, user_id: &str) -> Result<UserProfile> {
        // Identity: the users table is the canonical name/avatar, but a person
        // we have messages from may predate ever being upserted there, so fall
        // back to their most recent message.
        let (mut name, mut avatar_url): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT name, avatar_url FROM users WHERE id = ?1",
                [user_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        if name.is_none() || avatar_url.is_none() {
            if let Some((n, a)) = conn
                .query_row(
                    "SELECT name, avatar_url FROM messages
                     WHERE user_id = ?1 AND name IS NOT NULL
                     ORDER BY id_sort DESC LIMIT 1",
                    [user_id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()?
            {
                name = name.or(n);
                avatar_url = avatar_url.or(a);
            }
        }

        // Shared groups: scan each group's stored member list for this user.
        let mut shared_groups = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, name, image_url, members_json FROM conversations
                 WHERE kind = 'group' AND members_json IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (id, group_name, image_url, members_json) = row?;
                let Some(mj) = members_json else { continue };
                // A single group with malformed member JSON must not fail the
                // whole lookup — treat it as no members.
                let members: Vec<Member> = serde_json::from_str(&mj).unwrap_or_default();
                if members
                    .iter()
                    .any(|m| m.user_id.as_deref() == Some(user_id))
                {
                    shared_groups.push(GroupRef {
                        id,
                        name: group_name,
                        avatar_url: image_url,
                    });
                }
            }
        }
        shared_groups.sort_by(|a, b| {
            a.name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b.name.as_deref().unwrap_or("").to_lowercase())
        });

        // A DM is stored under the other participant's bare user id, so the DM's
        // conversation id is exactly `user_id` when one exists.
        let dm_conversation_id: Option<String> = conn
            .query_row(
                "SELECT id FROM conversations WHERE kind = 'dm' AND id = ?1",
                [user_id],
                |r| r.get(0),
            )
            .optional()?;

        let (message_count, first_seen, last_seen) = conn.query_row(
            "SELECT COUNT(*), MIN(created_at), MAX(created_at)
             FROM messages WHERE user_id = ?1",
            [user_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;

        Ok(UserProfile {
            user_id: user_id.to_string(),
            name,
            avatar_url,
            shared_groups,
            dm_conversation_id,
            message_count,
            first_seen,
            last_seen,
        })
    }

    /// Everything the archive knows about one person: their name and avatar, the
    /// groups you both belong to, whether a DM with them exists, and how much of
    /// their history is stored. Read-only; drives the profile card.
    ///
    /// Membership is not indexed — it lives only in each group's `members_json`
    /// — so shared groups are found by scanning the (few hundred) groups and
    /// parsing their member lists. That is fine for an on-demand, one-user
    /// lookup and avoids a second membership table the sync would have to keep
    /// in step.
    pub fn user_profile(&self, user_id: &str) -> Result<UserProfile> {
        Self::user_profile_on(&self.conn, user_id)
    }

    pub(crate) fn past_members_on(
        conn: &Connection,
        conversation_id: &str,
    ) -> Result<Vec<PastMember>> {
        let current: std::collections::HashSet<String> = {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT members_json FROM conversations WHERE id = ?1",
                    params![conversation_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match raw {
                Some(json) => serde_json::from_str::<Vec<Member>>(&json)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| m.user_id)
                    .collect(),
                None => std::collections::HashSet::new(),
            }
        };

        let mut stmt = conn.prepare(
            "SELECT
                m.user_id,
                (SELECT mm.name FROM messages mm
                 WHERE mm.user_id = m.user_id
                   AND mm.conversation_id = ?1
                   AND mm.name IS NOT NULL
                 ORDER BY mm.id_sort DESC LIMIT 1) AS name,
                (SELECT mm.avatar_url FROM messages mm
                 WHERE mm.user_id = m.user_id
                   AND mm.conversation_id = ?1
                 ORDER BY mm.id_sort DESC LIMIT 1) AS avatar_url,
                COUNT(*) AS message_count,
                MAX(m.created_at) AS last_seen
             FROM messages m
             WHERE m.conversation_id = ?1
               AND m.user_id IS NOT NULL
               AND (m.sender_type IS NULL OR m.sender_type != 'system')
               AND m.system = 0
             GROUP BY m.user_id
             ORDER BY message_count DESC",
        )?;

        let rows = stmt.query_map(params![conversation_id], |r| {
            Ok(PastMember {
                user_id: r.get(0)?,
                name: r.get(1)?,
                avatar_url: r.get(2)?,
                message_count: r.get(3)?,
                last_seen: r.get(4)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            let pm = row?;
            if !current.contains(&pm.user_id) {
                out.push(pm);
            }
        }
        Ok(out)
    }

    /// Past participants of a conversation: message senders who are no longer in
    /// the current member roster.
    ///
    /// Name and avatar come from the sender's most recent message in this
    /// conversation, so they reflect what they looked like last time they posted
    /// rather than what the archive holds for them globally. System messages are
    /// excluded; NULL user_ids are excluded. Current roster members are excluded
    /// by parsing `members_json` from the stored conversation row.
    ///
    /// Intended for the "former members" panel of a conversation info sheet — it
    /// answers "who used to post here?" without requiring a live API call.
    pub fn past_members(&self, conversation_id: &str) -> Result<Vec<PastMember>> {
        Self::past_members_on(&self.conn, conversation_id)
    }

    // ------------------------------------------------- analytics / stats

    /// The id of the chronologically oldest message in a conversation (by
    /// `id_sort`, which carries the same value as `id` parsed to i64). Returns
    /// `None` when the conversation has no messages archived.
    pub fn first_message_id(&self, conversation_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM messages WHERE conversation_id = ?1 ORDER BY id_sort ASC LIMIT 1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub(crate) fn group_stats_on(
        conn: &Connection,
        conversation_id: &str,
    ) -> Result<GroupStatsData> {
        let created_at: Option<i64> = conn
            .query_row(
                "SELECT CASE WHEN created_at = 0 THEN NULL ELSE created_at END
                 FROM conversations WHERE id = ?1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let message_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
            params![conversation_id],
            |r| r.get(0),
        )?;

        let first = conn
            .query_row(
                "SELECT id, created_at, name FROM messages
                 WHERE conversation_id = ?1 ORDER BY id_sort ASC LIMIT 1",
                params![conversation_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;

        let last = conn
            .query_row(
                "SELECT id, created_at FROM messages
                 WHERE conversation_id = ?1 ORDER BY id_sort DESC LIMIT 1",
                params![conversation_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;

        let distinct_senders: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT user_id) FROM messages
             WHERE conversation_id = ?1 AND system = 0 AND user_id IS NOT NULL",
            params![conversation_id],
            |r| r.get(0),
        )?;

        let cutoff = now_unix() - 30 * 86400;

        let active_last_30d: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT user_id) FROM messages
             WHERE conversation_id = ?1 AND system = 0 AND user_id IS NOT NULL
               AND created_at >= ?2",
            params![conversation_id, cutoff],
            |r| r.get(0),
        )?;

        let messages_last_30d: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND created_at >= ?2",
            params![conversation_id, cutoff],
            |r| r.get(0),
        )?;

        // Sum COUNT(DISTINCT user_id) per UTC calendar day over the 30d window,
        // then divide by 30. Days with no messages are absent from the inner
        // result and contribute 0 to the sum — exact denominator.
        let daily_distinct_sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(cnt), 0) FROM (
                     SELECT COUNT(DISTINCT user_id) AS cnt
                     FROM messages
                     WHERE conversation_id = ?1 AND system = 0 AND user_id IS NOT NULL
                       AND created_at >= ?2
                     GROUP BY created_at / 86400
                 ) sub",
            params![conversation_id, cutoff],
            |r| r.get(0),
        )?;
        let avg_active_per_day_30d = daily_distinct_sum as f64 / 30.0;

        // `created_at / 86400 * 86400` is UTC midnight bucketing: integer
        // division truncates to the day boundary, multiply restores the unix
        // timestamp of that midnight.
        let busiest = conn
            .query_row(
                "SELECT created_at / 86400 * 86400, COUNT(*) AS cnt
                 FROM messages WHERE conversation_id = ?1
                 GROUP BY created_at / 86400
                 ORDER BY cnt DESC LIMIT 1",
                params![conversation_id],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;

        let top = conn
            .query_row(
                "SELECT user_id, name, COUNT(*) AS cnt
                 FROM messages
                 WHERE conversation_id = ?1 AND system = 0 AND user_id IS NOT NULL
                 GROUP BY user_id ORDER BY cnt DESC LIMIT 1",
                params![conversation_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;

        Ok(GroupStatsData {
            conversation_id: conversation_id.to_string(),
            created_at,
            message_count,
            first_message_id: first.as_ref().map(|(id, _, _)| id.clone()),
            first_message_at: first.as_ref().and_then(|(_, t, _)| *t),
            first_message_name: first.as_ref().and_then(|(_, _, n)| n.clone()),
            last_message_id: last.as_ref().map(|(id, _)| id.clone()),
            last_message_at: last.as_ref().and_then(|(_, t)| *t),
            distinct_senders,
            active_last_30d,
            messages_last_30d,
            avg_active_per_day_30d,
            busiest_day_unix: busiest.as_ref().and_then(|(d, _)| *d),
            busiest_day_count: busiest.as_ref().map(|(_, c)| *c).unwrap_or(0),
            top_sender_user_id: top.as_ref().map(|(id, _, _)| id.clone()),
            top_sender_name: top.as_ref().and_then(|(_, n, _)| n.clone()),
            top_sender_count: top.as_ref().map(|(_, _, c)| *c).unwrap_or(0),
        })
    }

    /// Conversation-level stats for the group info panel.
    ///
    /// All sender-based counts exclude `system=1` rows. The 30-day window is
    /// always relative to the moment of the call via `now_unix()`.
    /// `avg_active_per_day_30d` sums `COUNT(DISTINCT user_id)` per calendar
    /// day (UTC midnight bucketing via `created_at / 86400`) across the last 30
    /// days and divides by 30.0 — days with zero activity count as zero, so the
    /// denominator is always exactly 30.
    pub fn group_stats(&self, conversation_id: &str) -> Result<GroupStatsData> {
        Self::group_stats_on(&self.conn, conversation_id)
    }

    pub(crate) fn leaderboard_on(
        conn: &Connection,
        conversation_id: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<LeaderboardRow>> {
        const SQL: &str = "
WITH
  base_msgs AS (
    SELECT m.id, m.id_sort, m.user_id, m.name, m.avatar_url,
           m.created_at, m.favorited_by_json, m.conversation_id
    FROM messages m
    JOIN conversations c ON c.id = m.conversation_id
    WHERE m.system = 0
      AND m.user_id IS NOT NULL
      AND (m.sender_type IS NULL OR m.sender_type NOT IN ('system', 'bot'))
      AND ((?1 IS NULL AND c.kind = 'group') OR m.conversation_id = ?1)
      AND (?2 IS NULL OR m.created_at >= ?2)
  ),
  user_msgs AS (
    SELECT user_id,
           COUNT(*) AS messages,
           COALESCE(SUM(
             CASE WHEN favorited_by_json IS NULL
                       OR favorited_by_json = ''
                       OR favorited_by_json = '[]'
                  THEN 0
                  ELSE json_array_length(favorited_by_json) END
           ), 0) AS likes_received,
           MIN(created_at) AS first_at,
           MAX(created_at) AS last_at
    FROM base_msgs
    GROUP BY user_id
  ),
  user_latest AS (
    SELECT user_id, name, avatar_url,
           ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY id_sort DESC) AS rn
    FROM base_msgs
  ),
  likes_given AS (
    SELECT CAST(je.value AS TEXT) AS user_id, COUNT(*) AS cnt
    FROM (
      SELECT favorited_by_json
      FROM base_msgs
      WHERE favorited_by_json IS NOT NULL
        AND favorited_by_json != '[]'
        AND favorited_by_json != ''
    ) AS fm, json_each(fm.favorited_by_json) je
    GROUP BY CAST(je.value AS TEXT)
  ),
  sys_msgs AS (
    SELECT m.raw_json, m.conversation_id
    FROM messages m
    JOIN conversations c ON c.id = m.conversation_id
    WHERE m.system = 1
      AND ((?1 IS NULL AND c.kind = 'group') OR m.conversation_id = ?1)
      AND (?2 IS NULL OR m.created_at >= ?2)
  ),
  kicks AS (
    SELECT CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT) AS user_id,
           COUNT(*) AS cnt
    FROM sys_msgs
    WHERE json_extract(raw_json, '$.event.type') LIKE 'membership%'
      AND json_extract(raw_json, '$.event.data.removed_user.id') IS NOT NULL
      AND json_extract(raw_json, '$.event.data.remover_user.id') IS NOT NULL
      AND CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT) !=
          CAST(json_extract(raw_json, '$.event.data.remover_user.id') AS TEXT)
    GROUP BY CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT)
  ),
  leaves AS (
    SELECT CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT) AS user_id,
           COUNT(*) AS cnt
    FROM sys_msgs
    WHERE json_extract(raw_json, '$.event.type') LIKE 'membership%'
      AND json_extract(raw_json, '$.event.data.removed_user.id') IS NOT NULL
      AND (
        json_extract(raw_json, '$.event.data.remover_user.id') IS NULL
        OR CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT) =
           CAST(json_extract(raw_json, '$.event.data.remover_user.id') AS TEXT)
      )
    GROUP BY CAST(json_extract(raw_json, '$.event.data.removed_user.id') AS TEXT)
  ),
  deleted AS (
    SELECT m.user_id, COUNT(*) AS cnt
    FROM sys_msgs sm
    JOIN messages m
      ON m.id = CAST(json_extract(sm.raw_json, '$.event.data.message_id') AS TEXT)
     AND m.conversation_id = sm.conversation_id
    WHERE json_extract(sm.raw_json, '$.event.type') = 'message.deleted'
      AND m.user_id IS NOT NULL
    GROUP BY m.user_id
  ),
  all_users AS (
    SELECT user_id FROM user_msgs
    UNION SELECT user_id FROM likes_given
    UNION SELECT user_id FROM kicks
    UNION SELECT user_id FROM leaves
    UNION SELECT user_id FROM deleted
  )
SELECT
  au.user_id,
  COALESCE(ul.name, u.name)             AS name,
  COALESCE(ul.avatar_url, u.avatar_url) AS avatar_url,
  COALESCE(um.messages, 0)              AS messages,
  COALESCE(um.likes_received, 0)        AS likes_received,
  COALESCE(lg.cnt, 0)                   AS likes_given,
  COALESCE(lv.cnt, 0)                   AS leaves,
  COALESCE(k.cnt, 0)                    AS kicks,
  COALESCE(d.cnt, 0)                    AS deleted,
  um.first_at                           AS first_at,
  um.last_at                            AS last_at,
  COALESCE(um.messages, 0) * 1
    + COALESCE(um.likes_received, 0) * 20
    + COALESCE(lg.cnt, 0) * 10
    - COALESCE(lv.cnt, 0) * 25
    - COALESCE(k.cnt, 0) * 500
    - COALESCE(d.cnt, 0) * 5             AS points
FROM all_users au
LEFT JOIN user_msgs um ON um.user_id = au.user_id
LEFT JOIN (SELECT user_id, name, avatar_url FROM user_latest WHERE rn = 1) ul
       ON ul.user_id = au.user_id
LEFT JOIN users u       ON u.id = au.user_id
LEFT JOIN likes_given lg ON lg.user_id = au.user_id
LEFT JOIN leaves lv     ON lv.user_id = au.user_id
LEFT JOIN kicks k       ON k.user_id = au.user_id
LEFT JOIN deleted d     ON d.user_id = au.user_id
ORDER BY points DESC
LIMIT 100";

        let mut stmt = conn.prepare(SQL)?;
        let rows = stmt.query_map(params![conversation_id, since_unix], |r| {
            Ok(LeaderboardRow {
                user_id: r.get(0)?,
                name: r.get(1)?,
                avatar_url: r.get(2)?,
                messages: r.get(3)?,
                likes_received: r.get(4)?,
                likes_given: r.get(5)?,
                leaves: r.get(6)?,
                kicks: r.get(7)?,
                deleted: r.get(8)?,
                first_at: r.get(9)?,
                last_at: r.get(10)?,
                points: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Per-user gamification leaderboard.
    ///
    /// `conversation_id` `None` = across all conversations of `kind='group'`
    /// (DMs are excluded regardless). `since_unix` `None` = all time.
    ///
    /// Points: `messages×1 + likes_received×20 + likes_given×10 − leaves×25
    /// − kicks×500 − deleted×5`. The source economy also penalises inactive
    /// months on all-time boards; that penalty is omitted for v1.
    ///
    /// Events are resolved from system messages' `raw_json`:
    /// * **kick**: `membership.*` event, `removed_user` present, `remover_user`
    ///   present and different from `removed_user`.
    /// * **leave**: `membership.*` event, `removed_user` present, `remover_user`
    ///   absent or equal to `removed_user` (a self-removal).
    /// * **deleted**: `message.deleted` event; `message_id` is resolved to its
    ///   author via JOIN within the same conversation; unresolvable rows skipped.
    ///
    /// GroupMe membership event ids are JSON numbers, not strings. `CAST(…AS TEXT)`
    /// normalises both forms so they compare correctly against `user_id` TEXT values.
    pub fn leaderboard(
        &self,
        conversation_id: Option<&str>,
        since_unix: Option<i64>,
    ) -> Result<Vec<LeaderboardRow>> {
        Self::leaderboard_on(&self.conn, conversation_id, since_unix)
    }
}

/// A user who posted in a conversation but is no longer in its current member
/// roster. See [`Store::past_members`].
#[derive(Debug, Clone, Serialize)]
pub struct PastMember {
    pub user_id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub message_count: i64,
    pub last_seen: Option<i64>,
}

/// A group referenced from a profile card: enough to render and open it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRef {
    pub id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// What the archive knows about one user — see [`Store::user_profile`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub user_id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub shared_groups: Vec<GroupRef>,
    /// The DM's conversation id (equal to `user_id`) when a DM with them is
    /// archived, else `None`.
    pub dm_conversation_id: Option<String>,
    pub message_count: i64,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
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

/// Statistics for a single group conversation. Drives the group info / stats
/// panel. See [`Store::group_stats`].
#[derive(Debug, Clone, Serialize)]
pub struct GroupStatsData {
    pub conversation_id: String,
    /// Unix timestamp of the conversation's creation from the API. `None` when
    /// the stored value is 0 (the schema default) or SQL NULL.
    pub created_at: Option<i64>,
    pub message_count: i64,
    pub first_message_id: Option<String>,
    pub first_message_at: Option<i64>,
    /// Sender name on the oldest archived message.
    pub first_message_name: Option<String>,
    pub last_message_id: Option<String>,
    pub last_message_at: Option<i64>,
    /// All-time count of distinct non-system senders.
    pub distinct_senders: i64,
    /// Distinct non-system senders with at least one message in the last 30 days.
    pub active_last_30d: i64,
    pub messages_last_30d: i64,
    /// Mean over the last 30 calendar days (UTC) of the per-day distinct-sender
    /// count. Zero-activity days count as zero, so the denominator is always 30.
    pub avg_active_per_day_30d: f64,
    /// Midnight UTC (unix) of the day with the most messages, all time. The day
    /// boundary uses `created_at / 86400 * 86400` — UTC midnight, not local time.
    pub busiest_day_unix: Option<i64>,
    pub busiest_day_count: i64,
    pub top_sender_user_id: Option<String>,
    pub top_sender_name: Option<String>,
    pub top_sender_count: i64,
}

/// One row of the gamification leaderboard. See [`Store::leaderboard`].
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub user_id: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub messages: i64,
    pub likes_received: i64,
    pub likes_given: i64,
    pub leaves: i64,
    pub kicks: i64,
    pub deleted: i64,
    pub points: i64,
    pub first_at: Option<i64>,
    pub last_at: Option<i64>,
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
    synced_at               INTEGER,
    -- Server read state. Nullable on purpose: NULL means GroupMe did not say,
    -- which is a different claim from "zero unread" and must not be read as
    -- "already read".
    unread_count            INTEGER,
    last_read_message_id    TEXT,
    last_read_at            INTEGER,
    -- Local-only (v3): pin ordering and mute. Declared here so a fresh database
    -- is correct without the SCHEMA_V3 ALTER; NULL pin_rank is unpinned.
    pin_rank                INTEGER,
    muted                   INTEGER NOT NULL DEFAULT 0,
    -- Local-only (v4): marks a group the account has left. Declared here so a
    -- fresh database is correct without the SCHEMA_V4 ALTER. 0 = current member,
    -- 1 = former member (history kept, group greyed out in sidebar).
    former                  INTEGER NOT NULL DEFAULT 0
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
        s.insert_messages("g1", &[msg("99", "older", 1), msg("100", "newer", 2)])
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
            s.get_media("https://i.groupme.com/a.png")
                .unwrap()
                .as_deref(),
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

    /// The bug this exists to prevent: a conversation already read on another
    /// device showing as unread forever, because the archive only knew what this
    /// window had opened.
    #[test]
    fn server_read_state_survives_the_round_trip() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                name: Some("Example Group".into()),
                updated_at: 100,
                unread_count: Some(3),
                last_read_message_id: Some("170000000000000001".into()),
                last_read_at: Some(1_785_300_000),
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let c = &s.list_conversations().unwrap()[0];
        assert_eq!(c.unread_count, Some(3));
        assert_eq!(
            c.last_read_message_id.as_deref(),
            Some("170000000000000001")
        );
    }

    /// `GET /v3/groups` omits read state on most groups — 200 of 211 in the
    /// capture — so a list sync arriving after a single-group fetch must not
    /// wipe what the detailed fetch established.
    #[test]
    fn a_sync_without_read_state_does_not_erase_the_read_state_we_have() {
        let s = Store::open_in_memory().unwrap();
        let detailed = Group {
            id: "10000001".into(),
            updated_at: 100,
            unread_count: Some(5),
            last_read_message_id: Some("170000000000000009".into()),
            last_read_at: Some(1_785_300_000),
            ..Default::default()
        };
        s.upsert_group(&detailed, 0).unwrap();

        // The same group as the list endpoint returns it: no read state at all.
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 200,
                ..Default::default()
            },
            1,
        )
        .unwrap();

        let c = &s.list_conversations().unwrap()[0];
        assert_eq!(
            c.unread_count,
            Some(5),
            "unread_count was erased by a list sync"
        );
        assert_eq!(
            c.last_read_message_id.as_deref(),
            Some("170000000000000009"),
            "last_read_message_id was erased by a list sync"
        );
        assert_eq!(c.updated_at, 200, "the rest of the row should still update");
    }

    /// The profile card's data in one lookup: identity, the groups you share,
    /// whether a DM exists, and how much of their history is stored.
    #[test]
    fn user_profile_gathers_identity_shared_groups_dm_and_counts() {
        let mut s = Store::open_in_memory().unwrap();

        // A group we are both in — the member row carries their name and avatar.
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                name: Some("Book Club".into()),
                image_url: Some("https://img/group.png".into()),
                updated_at: 100,
                members: vec![Member {
                    user_id: Some("20000002".into()),
                    nickname: Some("Sam".into()),
                    image_url: Some("https://img/sam.png".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            0,
        )
        .unwrap();

        // A group they are in but we are not both in — must not count as shared.
        s.upsert_group(
            &Group {
                id: "10000002".into(),
                name: Some("Strangers".into()),
                updated_at: 100,
                members: vec![Member {
                    user_id: Some("20000009".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            0,
        )
        .unwrap();

        // A DM with them: stored under their bare user id.
        s.upsert_chat(
            &Chat {
                updated_at: 200,
                other_user: OtherUser {
                    id: "20000002".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();

        s.insert_messages(
            "10000001",
            &[
                Message {
                    id: "170000000000000001".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Sam".into()),
                    created_at: 1000,
                    ..Default::default()
                },
                Message {
                    id: "170000000000000002".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Sam".into()),
                    created_at: 2000,
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let p = s.user_profile("20000002").unwrap();
        assert_eq!(p.user_id, "20000002");
        assert_eq!(p.name.as_deref(), Some("Sam"));
        assert_eq!(p.avatar_url.as_deref(), Some("https://img/sam.png"));
        assert_eq!(
            p.shared_groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["10000001"],
            "only the group we are both in is shared"
        );
        assert_eq!(p.shared_groups[0].name.as_deref(), Some("Book Club"));
        assert_eq!(p.dm_conversation_id.as_deref(), Some("20000002"));
        assert_eq!(p.message_count, 2);
        assert_eq!(p.first_seen, Some(1000));
        assert_eq!(p.last_seen, Some(2000));
    }

    /// Someone we hold nothing on still resolves — empty, not an error, and no
    /// phantom DM (a bare user id must not be mistaken for a DM that isn't there).
    #[test]
    fn user_profile_of_a_stranger_is_empty_not_an_error() {
        let s = Store::open_in_memory().unwrap();
        let p = s.user_profile("29999999").unwrap();
        assert!(p.shared_groups.is_empty());
        assert_eq!(p.dm_conversation_id, None);
        assert_eq!(p.message_count, 0);
        assert_eq!(p.name, None);
    }

    /// Receipts key DMs by the `+`-joined thread key; this table keys them by the
    /// other participant. Getting that mapping wrong silently applies every DM's
    /// read state to nothing at all.
    #[test]
    fn read_receipts_map_onto_groups_and_onto_dms_by_the_other_participant() {
        const ME: &str = "20000001";
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 100,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.upsert_chat(
            &Chat {
                updated_at: 200,
                other_user: OtherUser {
                    id: "20000002".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let receipts = vec![
            (
                "10000001".to_string(),
                Some("170000000000000001".to_string()),
            ),
            // Our id on the left...
            (
                format!("{ME}+20000002"),
                Some("170000000000000002".to_string()),
            ),
            // ...and a thread we are not part of, which must be ignored.
            (
                "20000008+20000009".to_string(),
                Some("170000000000000003".to_string()),
            ),
        ];
        assert_eq!(s.apply_read_receipts(&receipts, ME).unwrap(), 2);

        let by_id: std::collections::HashMap<_, _> = s
            .list_conversations()
            .unwrap()
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        assert_eq!(
            by_id["10000001"].last_read_message_id.as_deref(),
            Some("170000000000000001")
        );
        assert_eq!(
            by_id["20000002"].last_read_message_id.as_deref(),
            Some("170000000000000002"),
            "a DM receipt must land on the row keyed by the other participant"
        );
    }

    /// The ascending-order thread key puts the smaller id first, so the account's
    /// own id is not reliably on either side.
    #[test]
    fn a_dm_receipt_maps_with_our_id_on_either_side() {
        const ME: &str = "20000009";
        let s = Store::open_in_memory().unwrap();
        s.upsert_chat(
            &Chat {
                updated_at: 1,
                other_user: OtherUser {
                    id: "20000002".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();
        // "20000002+20000009" — we are on the right this time.
        let receipts = vec![(
            format!("20000002+{ME}"),
            Some("170000000000000004".to_string()),
        )];
        assert_eq!(s.apply_read_receipts(&receipts, ME).unwrap(), 1);
        assert_eq!(
            s.list_conversations().unwrap()[0]
                .last_read_message_id
                .as_deref(),
            Some("170000000000000004")
        );
    }

    /// A receipt naming the newest message we hold means nothing is unread, and
    /// the UI should not have to infer that. Anything else stays unknown rather
    /// than inventing a count.
    #[test]
    fn a_receipt_for_the_newest_message_resolves_to_zero_unread() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 100,
                messages: Some(crate::model::GroupPreview {
                    last_message_id: Some("170000000000000007".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            0,
        )
        .unwrap();

        s.apply_read_receipts(
            &[(
                "10000001".to_string(),
                Some("170000000000000007".to_string()),
            )],
            "20000001",
        )
        .unwrap();
        assert_eq!(s.list_conversations().unwrap()[0].unread_count, Some(0));

        // An older receipt leaves the count unknown, not zero.
        s.apply_read_receipts(
            &[(
                "10000001".to_string(),
                Some("170000000000000003".to_string()),
            )],
            "20000001",
        )
        .unwrap();
        assert_eq!(s.list_conversations().unwrap()[0].unread_count, None);
    }

    /// A multi-gigabyte v1 archive has to gain the columns in place rather than
    /// be rebuilt, so the upgrade path is tested separately from a fresh create.
    #[test]
    fn a_v1_archive_upgrades_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");

        // Build a v1-shaped archive: original schema, no read-state columns.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT,
                    description TEXT, image_url TEXT, creator_user_id TEXT,
                    created_at INTEGER, updated_at INTEGER, messages_count INTEGER,
                    last_message_id TEXT, last_message_text TEXT,
                    last_message_created_at INTEGER, members_json TEXT,
                    raw_json TEXT, synced_at INTEGER
                 );
                 INSERT INTO conversations (id, kind, name, updated_at)
                 VALUES ('10000001','group','Kept',42);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);

        let convos = s.list_conversations().unwrap();
        assert_eq!(
            convos.len(),
            1,
            "the existing row must survive the migration"
        );
        assert_eq!(convos[0].name.as_deref(), Some("Kept"));
        // Absent, not zero: we have never been told, so the UI must fall back.
        assert_eq!(convos[0].unread_count, None);

        // Re-opening an already-migrated archive must be a no-op, not an error.
        drop(s);
        let again = Store::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(again.list_conversations().unwrap().len(), 1);
    }

    /// The v2 archive (read-state columns, but no pin/mute) must gain the v3
    /// columns in place and keep its rows — the same in-place upgrade guarantee
    /// as the v1 test above, one schema version on.
    #[test]
    fn a_v2_archive_upgrades_in_place_and_gains_pin_and_mute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");

        // Build a v2-shaped archive: read-state columns present, pin/mute absent.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT,
                    description TEXT, image_url TEXT, creator_user_id TEXT,
                    created_at INTEGER, updated_at INTEGER, messages_count INTEGER,
                    last_message_id TEXT, last_message_text TEXT,
                    last_message_created_at INTEGER, members_json TEXT,
                    raw_json TEXT, synced_at INTEGER,
                    unread_count INTEGER, last_read_message_id TEXT, last_read_at INTEGER
                 );
                 INSERT INTO conversations (id, kind, name, updated_at, unread_count)
                 VALUES ('10000001','group','Kept',42,3);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }

        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);

        let convos = s.list_conversations().unwrap();
        assert_eq!(
            convos.len(),
            1,
            "the existing row must survive the migration"
        );
        assert_eq!(convos[0].name.as_deref(), Some("Kept"));
        // Pre-existing read state is untouched by the v3 migration.
        assert_eq!(convos[0].unread_count, Some(3));
        // The new columns arrive with their defaults: unpinned and unmuted.
        assert_eq!(convos[0].pin_rank, None);
        assert!(!convos[0].muted);

        // The pin/mute writes work against the migrated row.
        s.set_pin("10000001", Some(0)).unwrap();
        s.set_mute("10000001", true).unwrap();
        let c = &s.list_conversations().unwrap()[0];
        assert_eq!(c.pin_rank, Some(0));
        assert!(c.muted);
    }

    /// A pin overrides recency: a pinned conversation sorts above a more-recently
    /// active unpinned one.
    #[test]
    fn a_pinned_conversation_sorts_above_a_more_recent_unpinned_one() {
        let s = Store::open_in_memory().unwrap();
        // Most-recently active, but not pinned.
        s.upsert_chat(
            &Chat {
                updated_at: 9000,
                other_user: OtherUser {
                    id: "20000002".into(),
                    name: Some("Recent Person".into()),
                    avatar_url: None,
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();
        // Older, but pinned.
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                name: Some("Pinned".into()),
                updated_at: 100,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.set_pin("10000001", Some(0)).unwrap();

        let list = s.list_conversations().unwrap();
        assert_eq!(
            list[0].id, "10000001",
            "the pinned conversation must sort first despite being older"
        );
        assert_eq!(list[0].pin_rank, Some(0));
        assert_eq!(list[1].id, "20000002");
        assert_eq!(list[1].pin_rank, None);
    }

    #[test]
    fn reorder_pins_assigns_zero_through_n_in_the_given_order() {
        let mut s = Store::open_in_memory().unwrap();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            s.upsert_group(
                &Group {
                    id: (*id).into(),
                    updated_at: 100 + i as i64,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        }
        // Deliberately not the natural order.
        s.reorder_pins(&["c".into(), "a".into(), "b".into()])
            .unwrap();

        let by_id: std::collections::HashMap<_, _> = s
            .list_conversations()
            .unwrap()
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        assert_eq!(by_id["c"].pin_rank, Some(0));
        assert_eq!(by_id["a"].pin_rank, Some(1));
        assert_eq!(by_id["b"].pin_rank, Some(2));

        // And the list order follows the assigned ranks.
        let order: Vec<String> = s
            .list_conversations()
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    #[test]
    fn mute_flag_round_trips_and_defaults_off() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 1,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        assert!(!s.is_muted("10000001").unwrap());
        assert!(!s.list_conversations().unwrap()[0].muted);

        s.set_mute("10000001", true).unwrap();
        assert!(s.is_muted("10000001").unwrap());
        assert!(s.list_conversations().unwrap()[0].muted);

        // A conversation we do not hold is not muted, rather than an error.
        assert!(!s.is_muted("nonexistent").unwrap());
    }

    #[test]
    fn set_pin_clears_with_none() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 1,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.set_pin("10000001", Some(5)).unwrap();
        assert_eq!(s.list_conversations().unwrap()[0].pin_rank, Some(5));
        s.set_pin("10000001", None).unwrap();
        assert_eq!(s.list_conversations().unwrap()[0].pin_rank, None);
    }

    #[test]
    fn conversation_kind_distinguishes_group_from_dm() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                updated_at: 1,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.upsert_chat(
            &Chat {
                updated_at: 1,
                other_user: OtherUser {
                    id: "20000002".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();
        assert_eq!(
            s.conversation_kind("10000001").unwrap(),
            Some(ConversationKind::Group)
        );
        assert_eq!(
            s.conversation_kind("20000002").unwrap(),
            Some(ConversationKind::Dm)
        );
        assert_eq!(s.conversation_kind("unknown").unwrap(), None);
    }

    #[test]
    fn message_near_date_returns_the_newest_at_or_before_the_boundary() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages(
            "10000001",
            &[
                msg("170000000000000001", "a", 1000),
                msg("170000000000000002", "b", 2000),
                msg("170000000000000003", "c", 3000),
            ],
        )
        .unwrap();

        // Exactly on a message's timestamp returns that message.
        assert_eq!(
            s.message_near_date("10000001", 2000).unwrap().as_deref(),
            Some("170000000000000002")
        );
        // Between two returns the newer one that is still at or before.
        assert_eq!(
            s.message_near_date("10000001", 2500).unwrap().as_deref(),
            Some("170000000000000002")
        );
        // After everything returns the newest.
        assert_eq!(
            s.message_near_date("10000001", 9999).unwrap().as_deref(),
            Some("170000000000000003")
        );
        // Before everything returns nothing.
        assert!(s.message_near_date("10000001", 500).unwrap().is_none());
        // Scoped to the conversation.
        assert!(s.message_near_date("other", 9999).unwrap().is_none());
    }

    #[test]
    fn scoped_search_restricts_to_one_conversation() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert_messages("10000001", &[msg("1", "shared word here", 1)])
            .unwrap();
        s.insert_messages("10000002", &[msg("2", "shared word there", 2)])
            .unwrap();

        // Unscoped finds both.
        assert_eq!(s.search("shared", 10).unwrap().len(), 2);
        // Scoped finds only the one conversation's hit.
        let hits = s.search_scoped("shared", Some("10000001"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, "10000001");
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
        s.insert_messages("g1", std::slice::from_ref(&original))
            .unwrap();

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

    // ------------------------------------------------- former groups (v4) ---

    /// Groups absent from the present-ids list become former; groups in the list
    /// are (re-)marked as current. The entry remains visible in list_conversations.
    #[test]
    fn mark_former_groups_flags_absent_and_unflags_present() {
        let mut s = Store::open_in_memory().unwrap();
        // Two groups; we "leave" 10000002 by not including it in present_ids.
        for (id, updated_at) in [("10000001", 100i64), ("10000002", 200)] {
            s.upsert_group(
                &Group {
                    id: id.into(),
                    name: Some(format!("Group {id}")),
                    updated_at,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        }
        assert!(!s.list_conversations().unwrap().iter().any(|c| c.former));

        s.mark_former_groups(&["10000001".to_string()]).unwrap();

        let by_id: std::collections::HashMap<_, _> = s
            .list_conversations()
            .unwrap()
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();

        assert!(!by_id["10000001"].former, "10000001 is still present");
        assert!(by_id["10000002"].former, "10000002 was not in present list");

        // Calling again with both ids must reset 10000002 to not-former.
        s.mark_former_groups(&["10000001".to_string(), "10000002".to_string()])
            .unwrap();
        let by_id: std::collections::HashMap<_, _> = s
            .list_conversations()
            .unwrap()
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();
        assert!(
            !by_id["10000002"].former,
            "rejoin must clear the former flag"
        );
    }

    /// A former group still appears in list_conversations but sorts below current ones.
    #[test]
    fn former_group_sorts_after_current_groups_of_the_same_recency_tier() {
        let mut s = Store::open_in_memory().unwrap();
        // Both groups have the same updated_at so recency ordering would be a tie;
        // former must break the tie by sinking the left-group below the current one.
        for id in ["10000001", "10000002"] {
            s.upsert_group(
                &Group {
                    id: id.into(),
                    name: Some(format!("Group {id}")),
                    updated_at: 100,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        }
        // Leave 10000001 by not including it in the present list.
        s.mark_former_groups(&["10000002".to_string()]).unwrap();

        let list = s.list_conversations().unwrap();
        assert_eq!(list.len(), 2, "former group must still appear");
        assert_eq!(list[0].id, "10000002", "current group must sort first");
        assert!(list[1].former, "left group must be flagged former");

        // DMs are never marked former so they must not be affected.
        s.upsert_chat(
            &Chat {
                updated_at: 50,
                other_user: crate::model::OtherUser {
                    id: "20000003".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();
        let list = s.list_conversations().unwrap();
        assert_eq!(list.len(), 3);
        assert!(!list[0].former);
        assert!(!list[1].former || list[1].kind == ConversationKind::Dm);
    }

    // --------------------------------------------------- past members (F3) ---

    /// A sender who has messages but is not in members_json appears as a past
    /// member; a current roster member does NOT; system messages are ignored.
    #[test]
    fn past_members_returns_non_roster_senders_and_excludes_current_members() {
        let mut s = Store::open_in_memory().unwrap();

        // Group with one current member (20000002) and one past sender (20000003).
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                name: Some("Test Group".into()),
                updated_at: 100,
                members: vec![Member {
                    user_id: Some("20000002".into()),
                    nickname: Some("Current Member".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            0,
        )
        .unwrap();
        // Insert three messages: two from the past sender, one from the current
        // member, and one system message (should be excluded).
        s.insert_messages(
            "10000001",
            &[
                Message {
                    id: "170000000000000001".into(),
                    user_id: Some("20000003".into()),
                    name: Some("Past Sender".into()),
                    avatar_url: Some("https://img/past.png".into()),
                    created_at: 1000,
                    ..Default::default()
                },
                Message {
                    id: "170000000000000002".into(),
                    user_id: Some("20000003".into()),
                    name: Some("Past Sender".into()),
                    avatar_url: Some("https://img/past.png".into()),
                    created_at: 2000,
                    ..Default::default()
                },
                Message {
                    id: "170000000000000003".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Current Member".into()),
                    created_at: 3000,
                    ..Default::default()
                },
                // System message — must be excluded from past_members.
                Message {
                    id: "170000000000000004".into(),
                    user_id: Some("20000009".into()),
                    sender_type: Some("system".into()),
                    system: true,
                    created_at: 4000,
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let past = s.past_members("10000001").unwrap();
        assert_eq!(past.len(), 1, "only the past sender must appear");
        assert_eq!(past[0].user_id, "20000003");
        assert_eq!(past[0].name.as_deref(), Some("Past Sender"));
        assert_eq!(past[0].message_count, 2);
        assert_eq!(past[0].last_seen, Some(2000));
    }

    // ------------------------------------- schema v4 in-place migration ------

    /// A v3 archive (pin/mute columns present, former absent) must gain `former`
    /// in place, keep its rows, and have all former flags default to 0.
    #[test]
    fn a_v3_archive_upgrades_to_v4_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");

        // Build a v3-shaped archive: pin/mute columns present, former absent.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT,
                    description TEXT, image_url TEXT, creator_user_id TEXT,
                    created_at INTEGER DEFAULT 0, updated_at INTEGER DEFAULT 0,
                    messages_count INTEGER, last_message_id TEXT,
                    last_message_text TEXT, last_message_created_at INTEGER,
                    members_json TEXT, raw_json TEXT, synced_at INTEGER,
                    unread_count INTEGER, last_read_message_id TEXT,
                    last_read_at INTEGER,
                    pin_rank INTEGER, muted INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO conversations (id, kind, name, updated_at)
                 VALUES ('10000001','group','Kept',42);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }

        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);

        let convos = s.list_conversations().unwrap();
        assert_eq!(
            convos.len(),
            1,
            "the existing row must survive the migration"
        );
        assert_eq!(convos[0].name.as_deref(), Some("Kept"));
        // The new column arrives with its default: not former.
        assert!(
            !convos[0].former,
            "former must default to false after migration"
        );
        // Existing state is preserved.
        assert_eq!(convos[0].pin_rank, None);
        assert!(!convos[0].muted);

        // Re-open must be a no-op.
        drop(s);
        let again = Store::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(again.list_conversations().unwrap().len(), 1);
    }

    // ------------------------------------------ analytics (new commands) ---

    /// Guards against a build-feature regression: if the bundled SQLite is
    /// compiled without JSON1, the leaderboard query silently returns nothing
    /// instead of erroring, and everything looks fine until someone notices the
    /// scoreboard is empty. This test catches that class of failure.
    #[test]
    fn json1_functions_available_in_bundled_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // json_each must iterate an array and yield one row per element.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM json_each('[\"a\",\"b\",\"c\"]')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "json_each must be available in the bundled SQLite");
        // json_extract must navigate a path and return the value.
        let v: i64 = conn
            .query_row("SELECT json_extract('{\"k\":42}', '$.k')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            v, 42,
            "json_extract must be available in the bundled SQLite"
        );
    }

    /// `first_message_id` returns the message with the smallest `id_sort`
    /// (oldest by GroupMe's monotonically increasing id space), and `None`
    /// when the conversation has no archived messages.
    #[test]
    fn first_message_id_returns_oldest_by_sort_and_none_for_unknown() {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                ..Default::default()
            },
            0,
        )
        .unwrap();
        s.insert_messages(
            "10000001",
            &[
                Message {
                    id: "170000000000000003".into(),
                    created_at: 300,
                    ..Default::default()
                },
                Message {
                    id: "170000000000000001".into(),
                    created_at: 100,
                    ..Default::default()
                },
                Message {
                    id: "170000000000000002".into(),
                    created_at: 200,
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        assert_eq!(
            s.first_message_id("10000001").unwrap().as_deref(),
            Some("170000000000000001"),
            "must return the message with the lowest id_sort, not lowest created_at"
        );
        assert!(
            s.first_message_id("10000002").unwrap().is_none(),
            "unknown conversation must return None"
        );
    }

    /// group_stats must correctly compute message counts, first/last message
    /// pointers, sender counts, the 30-day window metrics, busiest day, and
    /// top sender. All sender-based counts exclude system messages.
    #[test]
    fn group_stats_computes_all_fields() {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "10000001".into(),
                created_at: 1_785_300_000,
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let now = now_unix();
        let cutoff = now - 30 * 86400;

        // Three messages in the ancient past (outside 30d window, all on day 0
        // of the unix epoch so they share a busiest-day bucket with count 3).
        // Three messages from user 20000001, two from 20000002, two from 20000003.
        s.insert_messages(
            "10000001",
            &[
                Message {
                    id: "170000000000000001".into(),
                    user_id: Some("20000001".into()),
                    name: Some("Alice".into()),
                    created_at: 1000,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                Message {
                    id: "170000000000000002".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Bob".into()),
                    created_at: 2000,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                Message {
                    id: "170000000000000003".into(),
                    user_id: Some("20000001".into()),
                    name: Some("Alice".into()),
                    created_at: 3000,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                // In 30d window, each on a distinct day.
                Message {
                    id: "170000000000000004".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Bob".into()),
                    created_at: cutoff + 86400,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                Message {
                    id: "170000000000000005".into(),
                    user_id: Some("20000001".into()),
                    name: Some("Alice".into()),
                    created_at: cutoff + 2 * 86400,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                // These two are on the same day bucket (now-10d), one user.
                Message {
                    id: "170000000000000006".into(),
                    user_id: Some("20000003".into()),
                    name: Some("Charlie".into()),
                    created_at: cutoff + 20 * 86400,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                Message {
                    id: "170000000000000007".into(),
                    user_id: Some("20000003".into()),
                    name: Some("Charlie".into()),
                    created_at: cutoff + 20 * 86400 + 1000,
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let st = s.group_stats("10000001").unwrap();

        assert_eq!(st.conversation_id, "10000001");
        assert_eq!(st.created_at, Some(1_785_300_000));
        assert_eq!(st.message_count, 7);

        assert_eq!(
            st.first_message_id.as_deref(),
            Some("170000000000000001"),
            "oldest by id_sort"
        );
        assert_eq!(st.first_message_at, Some(1000));
        assert_eq!(st.first_message_name.as_deref(), Some("Alice"));

        assert_eq!(
            st.last_message_id.as_deref(),
            Some("170000000000000007"),
            "newest by id_sort"
        );

        assert_eq!(st.distinct_senders, 3);
        assert_eq!(st.active_last_30d, 3, "all three sent within 30d");
        assert_eq!(st.messages_last_30d, 4, "msgs 004-007 are in window");

        // Daily distinct sum: day(cutoff+86400)→1, day(cutoff+2d)→1,
        // day(cutoff+20d)→1 (both msg6 and msg7 are the same user on same day).
        // Sum = 3, avg = 3/30 = 0.1.
        assert!(
            (st.avg_active_per_day_30d - 0.1).abs() < 1e-9,
            "avg active per day should be 3/30 = 0.1, got {}",
            st.avg_active_per_day_30d
        );

        // Busiest day (all time): msgs 001, 002, 003 are all on unix day 0
        // (created_at 1000, 2000, 3000 < 86400), giving count=3.
        assert_eq!(st.busiest_day_unix, Some(0), "day 0 of the unix epoch");
        assert_eq!(st.busiest_day_count, 3);

        // Top sender: user 20000001 has msgs 001, 003, 005 = 3 messages.
        assert_eq!(st.top_sender_user_id.as_deref(), Some("20000001"));
        assert_eq!(st.top_sender_name.as_deref(), Some("Alice"));
        assert_eq!(st.top_sender_count, 3);

        // group_stats on an unknown conversation must not panic.
        let empty = s.group_stats("10000099").unwrap();
        assert_eq!(empty.message_count, 0);
        assert!(empty.first_message_id.is_none());
        assert!(empty.created_at.is_none());
    }

    /// Full leaderboard scoring scenario: verifies likes, kicks vs leaves
    /// distinguished by remover presence, deleted-message attribution, exact
    /// point formula, DM exclusion on the all-groups query, and since_unix
    /// filtering.
    #[test]
    fn leaderboard_scores_events_and_excludes_dms() {
        use crate::model::SystemEvent;

        let mut s = Store::open_in_memory().unwrap();

        // Two groups.
        for id in ["10000001", "10000002"] {
            s.upsert_group(
                &Group {
                    id: id.into(),
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        }
        // One DM — its messages must not appear in the all-groups leaderboard.
        s.upsert_chat(
            &Chat {
                updated_at: 1,
                other_user: crate::model::OtherUser {
                    id: "20000006".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let now = now_unix();

        // Group A messages: user 20000001 writes msg11 (liked by 002 and 003);
        // user 20000002 writes msg12 (liked by 001).
        s.insert_messages(
            "10000001",
            &[
                Message {
                    id: "170000000000000011".into(),
                    user_id: Some("20000001".into()),
                    name: Some("Alice".into()),
                    created_at: now,
                    favorited_by: vec!["20000002".into(), "20000003".into()],
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                Message {
                    id: "170000000000000012".into(),
                    user_id: Some("20000002".into()),
                    name: Some("Bob".into()),
                    created_at: now,
                    favorited_by: vec!["20000001".into()],
                    sender_type: Some("user".into()),
                    ..Default::default()
                },
                // Kick: 20000001 removes 20000003 (remover != removed → kick).
                Message {
                    id: "170000000000000013".into(),
                    system: true,
                    created_at: now,
                    event: Some(SystemEvent {
                        kind: Some("membership.announce.removed".into()),
                        data: serde_json::json!({
                            "removed_user": {"id": "20000003"},
                            "remover_user": {"id": "20000001"}
                        }),
                    }),
                    ..Default::default()
                },
                // Leave: 20000004 exits on their own (no remover_user → leave).
                Message {
                    id: "170000000000000014".into(),
                    system: true,
                    created_at: now,
                    event: Some(SystemEvent {
                        kind: Some("membership.notifications.exited".into()),
                        data: serde_json::json!({
                            "removed_user": {"id": "20000004"}
                        }),
                    }),
                    ..Default::default()
                },
                // Delete: attributes the deletion to the author of msg12 (20000002).
                Message {
                    id: "170000000000000015".into(),
                    system: true,
                    created_at: now,
                    event: Some(SystemEvent {
                        kind: Some("message.deleted".into()),
                        data: serde_json::json!({
                            "message_id": "170000000000000012",
                            "deleted_at": 1_785_400_000
                        }),
                    }),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        // Group B: user 20000005 writes msg16 (liked by 20000001).
        s.insert_messages(
            "10000002",
            &[Message {
                id: "170000000000000016".into(),
                user_id: Some("20000005".into()),
                name: Some("Eve".into()),
                created_at: now,
                favorited_by: vec!["20000001".into()],
                sender_type: Some("user".into()),
                ..Default::default()
            }],
        )
        .unwrap();

        // DM: must be excluded from all-groups leaderboard.
        s.insert_messages(
            "20000006",
            &[Message {
                id: "170000000000000017".into(),
                user_id: Some("20000006".into()),
                created_at: now,
                ..Default::default()
            }],
        )
        .unwrap();

        let board = s.leaderboard(None, None).unwrap();

        // Build a lookup by user_id for easy assertion.
        let by_uid: std::collections::HashMap<&str, &LeaderboardRow> =
            board.iter().map(|r| (r.user_id.as_str(), r)).collect();

        // DM user must be absent.
        assert!(
            !by_uid.contains_key("20000006"),
            "DM user must not appear in the all-groups leaderboard"
        );

        // ------ user 20000001: 1 msg, 2 likes_received, 2 likes_given ------
        let u1 = by_uid["20000001"];
        assert_eq!(u1.messages, 1);
        assert_eq!(u1.likes_received, 2, "msg11 liked by 002 and 003");
        assert_eq!(u1.likes_given, 2, "liked msg12 and msg16");
        assert_eq!(u1.leaves, 0);
        assert_eq!(u1.kicks, 0);
        assert_eq!(u1.deleted, 0);
        // points = 1*1 + 2*20 + 2*10 = 61
        assert_eq!(u1.points, 61);

        // ------ user 20000002: 1 msg, 1 like_received, 1 like_given, 1 deleted ---
        let u2 = by_uid["20000002"];
        assert_eq!(u2.messages, 1);
        assert_eq!(u2.likes_received, 1);
        assert_eq!(u2.likes_given, 1, "liked msg11");
        assert_eq!(u2.deleted, 1, "msg12 was deleted → attributed to 20000002");
        // points = 1 + 20 + 10 - 5 = 26
        assert_eq!(u2.points, 26);

        // ------ user 20000003: 0 msgs, 1 like_given, 1 kick ----------------
        let u3 = by_uid["20000003"];
        assert_eq!(u3.messages, 0);
        assert_eq!(u3.likes_given, 1, "liked msg11");
        assert_eq!(u3.kicks, 1, "was kicked by 20000001 in msg13");
        assert_eq!(u3.leaves, 0, "remover != removed → kick not leave");
        // points = 0 + 0 + 10 - 500 = -490
        assert_eq!(u3.points, -490);

        // ------ user 20000004: 0 msgs, 1 leave (no remover) ----------------
        let u4 = by_uid["20000004"];
        assert_eq!(u4.messages, 0);
        assert_eq!(u4.leaves, 1);
        assert_eq!(u4.kicks, 0, "absent remover_user means leave not kick");
        // points = -25
        assert_eq!(u4.points, -25);

        // ------ user 20000005: 1 msg in group B, 1 like_received ----------
        let u5 = by_uid["20000005"];
        assert_eq!(u5.messages, 1);
        assert_eq!(u5.likes_received, 1);
        // points = 1 + 20 = 21
        assert_eq!(u5.points, 21);

        // Ordering: 61, 26, 21, -25, -490
        assert_eq!(board[0].user_id, "20000001");
        assert_eq!(board[1].user_id, "20000002");
        assert_eq!(board[2].user_id, "20000005");
        assert_eq!(board[3].user_id, "20000004");
        assert_eq!(board[4].user_id, "20000003");

        // ---- scoped to group A only ----------------------------------------
        let board_a = s.leaderboard(Some("10000001"), None).unwrap();
        let by_uid_a: std::collections::HashMap<&str, &LeaderboardRow> =
            board_a.iter().map(|r| (r.user_id.as_str(), r)).collect();
        // 20000005 is only in group B, so must be absent.
        assert!(
            !by_uid_a.contains_key("20000005"),
            "20000005 only posted in group B"
        );
        // 20000001's likes_given in group A only: liked msg12 (yes); did NOT
        // like msg16 (group B). So likes_given=1.
        assert_eq!(by_uid_a["20000001"].likes_given, 1);

        // ---- since_unix filtering: old messages excluded -------------------
        // An ancient message for 20000001 that predates the since_unix cutoff.
        s.insert_messages(
            "10000001",
            &[Message {
                id: "170000000000000018".into(),
                user_id: Some("20000001".into()),
                created_at: 1000,
                sender_type: Some("user".into()),
                ..Default::default()
            }],
        )
        .unwrap();

        // With since_unix = now - 86400 (yesterday), the ancient message is excluded.
        let board_recent = s.leaderboard(None, Some(now - 86400)).unwrap();
        let by_uid_r: std::collections::HashMap<&str, &LeaderboardRow> = board_recent
            .iter()
            .map(|r| (r.user_id.as_str(), r))
            .collect();
        // 20000001 still has 1 message from the recent batch (msg11, created_at=now).
        assert_eq!(
            by_uid_r["20000001"].messages, 1,
            "the recent message must still count"
        );
        // If since_unix > now (far future), 20000001 disappears entirely.
        let board_future = s.leaderboard(None, Some(now + 86400)).unwrap();
        assert!(
            board_future.is_empty() || !board_future.iter().any(|r| r.user_id == "20000001"),
            "future since_unix must exclude all messages"
        );
    }

    #[test]
    fn open_readonly_on_in_memory_store_errors() {
        let s = Store::open_in_memory().unwrap();
        let result = s.open_readonly();
        assert!(
            result.is_err(),
            "open_readonly must fail for in-memory stores (no db path)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("in-memory"),
            "error message must mention in-memory: {msg}"
        );
    }

    /// Runs `leaderboard_on` against a read-only side connection opened with
    /// `open_readonly` and asserts identical results to the locked main path.
    /// This is the correctness proof that the analytics bypass is safe.
    #[test]
    fn leaderboard_side_connection_matches_locked_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");
        let mut s = Store::open(&path).unwrap();

        s.upsert_group(
            &Group {
                id: "10000001".into(),
                name: Some("Test Group".into()),
                updated_at: 100,
                ..Default::default()
            },
            0,
        )
        .unwrap();

        s.insert_messages(
            "10000001",
            &[
                msg("170000000000000001", "hello", 1_000_000),
                msg("170000000000000002", "world", 1_000_001),
            ],
        )
        .unwrap();

        let expected = s.leaderboard(None, None).unwrap();

        // Open a side connection; the locked path must not be held.
        let side = s.open_readonly().unwrap();
        let actual = Store::leaderboard_on(&side, None, None).unwrap();

        assert_eq!(
            expected.len(),
            actual.len(),
            "side connection must return the same row count as the locked path"
        );
        if let (Some(e), Some(a)) = (expected.first(), actual.first()) {
            assert_eq!(e.user_id, a.user_id, "top user_id must match");
            assert_eq!(e.messages, a.messages, "message count must match");
            assert_eq!(e.points, a.points, "points must match");
        }
    }
}
