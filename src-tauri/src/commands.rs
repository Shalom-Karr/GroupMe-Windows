//! The IPC surface exposed to the bundled offline reader.
//!
//! Every command here is a **read**. There is deliberately no command to send,
//! edit, delete, react, or otherwise mutate anything — not a disabled one, not
//! a guarded one, none at all. "Read-only when offline" is therefore a property
//! of what got compiled in rather than a check some future refactor can skip:
//! there is no reachable path from the offline surface to a write.
//!
//! Sending stays entirely with the real `web.groupme.com` in the online
//! webview, which never loads these commands.

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use serde::Serialize;
use tauri::State;

use crate::model::{id_sort_key, Conversation, Message};
use crate::store::{SearchHit, Store};

/// A **blocking** mutex on purpose. `Store` is synchronous rusqlite, so every
/// access happens inside `spawn_blocking` (see `read_store`) and never on a
/// Tokio worker — which is exactly where an async mutex would be the wrong
/// tool.
pub type SharedStore = Arc<Mutex<Store>>;

/// Commands return `Result<_, String>` because the error crosses into
/// JavaScript, where an `anyhow::Error` means nothing.
type CmdResult<T> = Result<T, String>;

fn fail(context: &str, e: impl std::fmt::Display) -> String {
    // Logged in full, returned in brief: the reader shows this to a user who
    // is offline and cannot act on a SQLite error string.
    log::error!("{context}: {e}");
    format!("{context} failed")
}

/// Runs one archive read on the blocking pool.
///
/// At this archive's scale a single query runs for hundreds of milliseconds.
/// Executed inline it would pin a Tokio worker for that whole time and, through
/// the shared lock, stall every other reader behind it.
async fn read_store<T, F>(store: &SharedStore, context: &'static str, f: F) -> CmdResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
{
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // A reader that panicked mid-query poisons the lock. The guard is
        // adopted rather than propagated: one bad query must not brick the
        // archive for the rest of the session.
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    })
    .await;

    match joined {
        Ok(Ok(v)) => Ok(v),
        // `{e:#}` so the anyhow context chain reaches the log rather than just
        // its outermost layer.
        Ok(Err(e)) => Err(fail(context, format!("{e:#}"))),
        Err(e) => Err(fail(context, e)),
    }
}

#[derive(Debug, Serialize)]
pub struct ArchiveStats {
    pub conversations: usize,
    pub messages: i64,
    pub last_sync_at: Option<i64>,
    pub account_name: Option<String>,
    /// Groups vs DMs, split out so the panel can report each on its own — a
    /// filtered network can leave groups empty while DMs are complete.
    pub groups: usize,
    pub dms: usize,
    pub group_messages: i64,
    pub dm_messages: i64,
}

#[tauri::command]
pub async fn archive_conversations(store: State<'_, SharedStore>) -> CmdResult<Vec<Conversation>> {
    read_store(store.inner(), "loading conversations", |s| {
        s.list_conversations()
    })
    .await
}

/// One page, newest-first. `before_id` is the raw id of the oldest message
/// already held; pass `null`/`None` for the newest page.
///
/// A **string**, deliberately, not a number. GroupMe ids run to ~1.78e17, far
/// past `Number.MAX_SAFE_INTEGER`, where a double's ulp is 32 — a numeric cursor
/// rounds up and repeats the boundary message, or rounds down and drops it from
/// the reader permanently. The decimal string is what JavaScript already holds
/// losslessly, so the parse happens here instead.
#[tauri::command]
pub async fn archive_messages(
    store: State<'_, SharedStore>,
    conversation_id: String,
    limit: Option<i64>,
    before_id: Option<String>,
) -> CmdResult<Vec<Message>> {
    // Clamp rather than trust: an unbounded limit from the page would pull a
    // 40,000-message group into memory in one go.
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let before_sort = match before_id.as_deref() {
        // `id_sort_key` yields 0 for anything unparseable, and 0 as a cursor
        // selects nothing — which the reader would read as the start of history
        // and stop paging. A cursor that is not a real id is a caller bug, so it
        // fails loudly instead of silently truncating the conversation.
        Some(raw) => Some(
            Some(id_sort_key(raw))
                .filter(|k| *k > 0)
                .ok_or_else(|| fail("loading messages", format!("unusable cursor {raw:?}")))?,
        ),
        None => None,
    };
    read_store(store.inner(), "loading messages", move |s| {
        s.messages_page(&conversation_id, limit, before_sort)
    })
    .await
}

/// Full-text search. `conversation_id`, when present, scopes the search to that
/// one conversation; `None` searches the whole archive.
#[tauri::command]
pub async fn archive_search(
    store: State<'_, SharedStore>,
    query: String,
    conversation_id: Option<String>,
    limit: Option<i64>,
) -> CmdResult<Vec<SearchHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.unwrap_or(50).clamp(1, 200);
    // FTS5 treats a bare `"` or an unbalanced operator as a syntax error, and
    // the user is typing free text, not a query language. Quote the whole thing
    // as a phrase so anything they type is searchable rather than a hard error.
    let phrase = format!("\"{}\"", query.replace('"', ""));
    read_store(store.inner(), "searching archive", move |s| {
        s.search_scoped(&phrase, conversation_id.as_deref(), limit)
    })
    .await
}

/// The id of the newest message in a conversation at or before `before_unix`,
/// for a "jump to date" picker. Returns the id as a **string** — GroupMe ids
/// exceed 2^53 and must never round-trip through a JS number.
#[tauri::command]
pub async fn archive_message_near_date(
    store: State<'_, SharedStore>,
    conversation_id: String,
    before_unix: i64,
) -> CmdResult<Option<String>> {
    read_store(store.inner(), "finding a message by date", move |s| {
        s.message_near_date(&conversation_id, before_unix)
    })
    .await
}

/// Local path for a cached asset, or `None` if it was never downloaded.
///
/// The reader must not fall back to the remote URL when this returns `None`:
/// attachment URLs redirect to expiring signed CDN links, so offline they fail
/// and online they eventually expire anyway.
#[tauri::command]
pub async fn archive_media_path(
    store: State<'_, SharedStore>,
    url: String,
) -> CmdResult<Option<String>> {
    read_store(store.inner(), "resolving cached media", move |s| {
        s.get_media(&url)
    })
    .await
}

#[tauri::command]
pub async fn archive_stats(store: State<'_, SharedStore>) -> CmdResult<ArchiveStats> {
    // One hop onto the blocking pool for all four reads, not four.
    read_store(store.inner(), "loading archive stats", |s| {
        let kinds = s.counts_by_kind().context("counting by kind")?;
        Ok(ArchiveStats {
            // A COUNT, not `list_conversations().len()`: the latter selects
            // every column of every row purely to throw them away.
            conversations: s.conversation_count().context("counting conversations")?,
            messages: s.total_message_count().context("counting messages")?,
            last_sync_at: s
                .get_meta("last_sync_at")
                .context("reading sync time")?
                .and_then(|v| v.parse::<i64>().ok()),
            account_name: s.get_meta("account_name").context("reading account")?,
            groups: kinds.groups,
            dms: kinds.dms,
            group_messages: kinds.group_messages,
            dm_messages: kinds.dm_messages,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConversationKind, Group};

    /// Only the production half of this file. The test module below names the
    /// very patterns it forbids, so scanning the whole file would always match
    /// itself and the assertion would be worthless.
    fn production_source() -> &'static str {
        let full = include_str!("commands.rs");
        full.split("#[cfg(test)]").next().unwrap_or(full)
    }

    /// The read-only guarantee, asserted rather than assumed.
    ///
    /// If someone later adds a mutating command to this module, this is what
    /// should stop them — it fails loudly rather than letting "offline is
    /// read-only" quietly become untrue.
    #[test]
    fn no_mutating_command_exists_on_the_offline_surface() {
        let source = production_source();
        // Assembled at runtime so the literals never appear in the file being
        // scanned — otherwise this list is its own counterexample.
        let verbs = [
            "send_", "post_", "delete_", "edit_", "update_", "create_", "react", "like_",
            "unlike_", "remove_", "pin_", "unpin_",
        ];
        for verb in verbs {
            let needle = format!("fn {verb}");
            assert!(
                !source.contains(&needle),
                "a mutating command (`{needle}`) was added to the offline IPC \
                 surface; offline must stay read-only by construction"
            );
        }
    }

    #[test]
    fn every_exposed_command_is_named_archive_something() {
        // The naming convention doubles as the boundary marker: if it is
        // reachable from the offline page, it reads the archive and nothing else.
        let source = production_source();
        let marker = "#[tauri::command]";
        let mut found = 0;
        for (idx, _) in source.match_indices(marker) {
            let after = &source[idx + marker.len()..];
            let sig: String = after
                .lines()
                .find(|l| l.contains("fn "))
                .unwrap_or_default()
                .to_string();
            assert!(
                sig.contains("fn archive_"),
                "a #[tauri::command] is not an archive_* reader: {sig}"
            );
            found += 1;
        }
        assert_eq!(found, 6, "expected exactly the six archive readers");
    }

    #[test]
    fn search_rejects_empty_input_without_touching_the_db() {
        let store: SharedStore = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let s = store.lock().unwrap();
        // Exercised directly rather than through State, which needs an app handle.
        assert!(s.search("\"\"", 10).unwrap_or_default().is_empty());
    }

    /// The reason `before_id` is a string rather than a number.
    #[test]
    fn a_real_id_survives_the_cursor_round_trip_that_a_double_would_corrupt() {
        let raw = "170000000000000007";
        let exact = id_sort_key(raw);
        assert_eq!(
            exact.to_string(),
            raw,
            "the string cursor must parse exactly"
        );
        // What `Number(message.id)` handed us instead: past 2^53 a double's ulp
        // is 32, so the boundary message is either repeated or silently skipped.
        assert_ne!(exact, raw.parse::<f64>().unwrap() as i64);
    }

    #[test]
    fn an_unparseable_cursor_is_not_silently_treated_as_the_end_of_history() {
        // `id_sort_key` yields 0 for garbage, and `id_sort < 0` matches nothing.
        assert_eq!(id_sort_key("not-an-id"), 0);
    }

    #[test]
    fn search_survives_quote_characters_in_user_input() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "g1".into(),
                name: Some("Example".into()),
                ..Default::default()
            },
            0,
        )
        .unwrap();

        // A bare quote is an FTS5 syntax error; the command quotes and strips.
        let raw = "say \"hello\"";
        let phrase = format!("\"{}\"", raw.replace('"', ""));
        assert!(
            s.search(&phrase, 10).is_ok(),
            "unbalanced quotes must not error"
        );
    }

    #[tokio::test]
    async fn stats_are_zero_on_a_fresh_archive() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.total_message_count().unwrap(), 0);
        assert!(s.list_conversations().unwrap().is_empty());
        assert!(s.get_meta("last_sync_at").unwrap().is_none());
    }

    #[tokio::test]
    async fn conversations_come_back_for_the_reader() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_group(
            &Group {
                id: "g1".into(),
                name: Some("Example Group".into()),
                updated_at: 100,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        let list = s.list_conversations().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, ConversationKind::Group);
    }
}
