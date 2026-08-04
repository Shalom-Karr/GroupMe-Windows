//! The sync engine: turns the live API into the local archive.
//!
//! Each conversation is synced in two phases, in this order:
//!
//! 1. **Tail** — everything newer than `sync_state.newest_id`, via
//!    `Cursor::After`. This runs first on purpose. A user who opens the app on a
//!    train wants today's messages, and a cycle that spent its whole budget
//!    walking back through 2019 would hand them an archive that is complete at
//!    the wrong end.
//! 2. **Backfill** — older than `sync_state.oldest_id`, via `Cursor::Before`,
//!    capped at a page budget so one 142,000-message group cannot starve every
//!    other conversation. The cap costs nothing but wall clock: the walk resumes
//!    from the persisted cursor on the next cycle.
//!
//! The same ordering is applied a level up, across conversations. A cycle that
//! finished each conversation before starting the next would leave the hundredth
//! chat empty until the ninety-ninth had spent its entire backfill budget, so
//! **every conversation nobody has tailed yet gets its newest page before any
//! conversation walks backwards**. A cold archive is therefore useful in
//! seconds rather than tens of minutes, and a group joined later jumps the
//! queue for the same reason.
//!
//! Cadence has two modes. While any conversation is still incomplete the engine
//! is *catching up*: it takes the larger per-cycle budgets and reports
//! `more_work`, which the caller answers by coming straight back round instead
//! of sleeping. Once everything is backfilled it drops to the gentle budgets and
//! the caller's normal minute-long sleep — steady state is tailing, and tailing
//! is cheap. The speedup comes from not sleeping between cycles and not capping
//! pages while catching up, never from raising the request rate:
//! `request_spacing` stays at ~120 ms (~8 req/s), requests stay serialised, and
//! a 429 that outlives the client's own retries stands the whole cycle down.
//!
//! The backfill terminates on an **empty** page, never a short one. Short pages
//! occur mid-history; treating one as the end silently truncates the archive,
//! and the opposite mistake — never terminating — loops forever.
//!
//! Two properties are load-bearing and easy to lose in a refactor:
//!
//! * `Store` is blocking rusqlite, and so is every filesystem write here. Both
//!   run inside `tokio::task::spawn_blocking`, so a 50 ms page insert or a 5 MB
//!   blob write occupies a blocking-pool thread instead of a Tokio worker. The
//!   `std::sync::Mutex` guard is created inside that closure and dropped when it
//!   returns — it is never alive across an `await`, which would both stall every
//!   UI read for an HTTP round trip and make the cycle future non-`Send`.
//! * Attachment bytes are downloaded, not referenced. `m.groupme.com` redirects
//!   to a `cdn2.groupme.com` URL signed with an expiring Azure SAS token, so a
//!   stored URL is worthless the moment the signature lapses — online as well as
//!   off. Only the bytes on disk are an archive.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::api::{ApiError, Cursor, GroupMeClient};
use crate::model::{id_sort_key, Conversation, ConversationKind, Message};
use crate::store::{Store, SyncState};

#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Page size the client asks for. Used only to recognise a short tail page
    /// as "caught up"; the backfill deliberately ignores it.
    pub page_limit: u32,
    /// Ceiling on media downloads per cycle once the archive is caught up, so a
    /// group that just posted 400 photos does not monopolise the cycle.
    pub media_per_cycle: usize,
    /// Media downloads per cycle while still catching up.
    pub catchup_media_per_cycle: usize,
    /// Ceiling on backfill pages per conversation per cycle once the archive is
    /// caught up. Also the ceiling for a manual single-conversation refresh.
    pub backfill_pages_per_cycle: usize,
    /// Backfill pages per conversation per cycle while still catching up.
    ///
    /// Deliberately an order of magnitude larger: the archive is the product,
    /// and at five pages a minute a 142,000-message group takes five hours.
    /// Raising it costs nothing in request rate — the cycle is still serialised
    /// and still spaced by `request_spacing` — it only stops the engine from
    /// idling between conversations.
    pub catchup_backfill_pages_per_cycle: usize,
    /// Delay between API calls. The rate limit is undocumented and sits near
    /// 10 req/s per token, so requests are serialised and spaced.
    pub request_spacing: Duration,
    /// How long the whole cycle stands down after a 429 that outlived the
    /// client's own retry/backoff. Getting the user's real account flagged is a
    /// far worse outcome than a slow backfill.
    pub rate_limit_backoff: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            page_limit: 100,
            media_per_cycle: 40,
            catchup_media_per_cycle: 200,
            backfill_pages_per_cycle: 5,
            catchup_backfill_pages_per_cycle: 50,
            request_spacing: Duration::from_millis(120),
            rate_limit_backoff: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncReport {
    pub conversations_seen: usize,
    pub messages_inserted: usize,
    pub media_cached: usize,
    pub backfills_completed: usize,
    /// Conversations whose history is still incomplete after this cycle.
    pub conversations_backfilling: usize,
    /// Messages the archive holds now, across every conversation.
    pub total_archived: i64,
    /// True while history remains to walk or media remains to fetch *and* this
    /// cycle made progress. The caller loops immediately instead of sleeping
    /// when it is set.
    ///
    /// Gated on progress on purpose. "Some conversation is incomplete" alone is
    /// not enough: a group that 404s, or one whose cursor the server refuses to
    /// advance, stays incomplete for the life of the process, and the caller
    /// would then spin at the full request rate forever achieving nothing.
    pub more_work: bool,
    pub errors: Vec<String>,
}

pub struct SyncEngine {
    client: GroupMeClient,
    store: Arc<Mutex<Store>>,
    config: SyncConfig,
    media_dir: PathBuf,
    /// URLs that failed permanently (403/404) this run.
    ///
    /// `uncached_media_urls` is a `LIMIT` over a stable scan, so a permanently
    /// dead asset at the front of that list would be retried — and re-fail —
    /// every cycle, blocking everything behind it forever. Skipping it for the
    /// rest of the process lets the queue drain; a restart retries, which is the
    /// right cadence for something that might have been a server-side blip.
    ///
    /// Async, unlike the store: this one guards an in-memory set, never a
    /// blocking call.
    blocked_media: tokio::sync::Mutex<HashSet<String>>,
}

/// Adopts a poisoned lock instead of propagating the panic. A single task that
/// died mid-query must not permanently brick the archive for everything else.
fn lock_store(store: &Arc<Mutex<Store>>) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// A blocking archive task can only fail to join if it panicked or the runtime
/// is shutting down. Neither is an API failure, but `ApiError` is the only
/// channel out of the sync path — see `store_error`.
fn join_error(context: &str, err: tokio::task::JoinError) -> ApiError {
    log::error!("archive task failed while {context}: {err}");
    ApiError::Decode(format!("archive: {context}: {err}"))
}

impl SyncEngine {
    pub fn new(client: GroupMeClient, store: Arc<Mutex<Store>>, media_dir: PathBuf) -> Self {
        Self::with_config(client, store, media_dir, SyncConfig::default())
    }

    pub fn with_config(
        client: GroupMeClient,
        store: Arc<Mutex<Store>>,
        media_dir: PathBuf,
        config: SyncConfig,
    ) -> Self {
        Self {
            client,
            store,
            config,
            media_dir,
            blocked_media: tokio::sync::Mutex::new(HashSet::new()),
        }
    }

    /// One full cycle: refresh the conversation list, tail everything nobody has
    /// tailed yet, walk history, then spend the media budget.
    ///
    /// Deliberately infallible. A cycle that aborted on the first bad
    /// conversation would leave the rest of the archive permanently stale, so
    /// failures are collected and reported. There are two exceptions, both of
    /// which are about the account rather than the archive: a dead token, where
    /// every subsequent call fails identically, and a 429 that outlived the
    /// client's own retries. Hammering either is how an account gets flagged.
    ///
    /// `SyncReport::more_work` tells the caller whether to come straight back
    /// round or fall back to its idle sleep.
    pub async fn sync_once(&self) -> SyncReport {
        let mut report = SyncReport::default();
        let mut ids: Vec<String> = Vec::new();
        let stood_down = self.run_cycle(&mut report, &mut ids).await;
        self.finish(report, &ids, stood_down).await
    }

    /// Pulls the whole read-state map and applies it to the archive.
    ///
    /// One request covers every conversation, so this runs each cycle rather than
    /// per conversation. It is deliberately non-fatal: read state is a nicety
    /// next to the messages themselves, and losing it must never stand the cycle
    /// down or abort a backfill.
    ///
    /// Needs the account's own user id to resolve `+`-joined DM keys onto the
    /// rows this archive stores under the other participant's id; without it the
    /// DM half of the map is unusable, so the call is skipped rather than applied
    /// half-right.
    ///
    /// Failures are logged, never added to `SyncReport::errors`. That field is
    /// what the status panel reports as "sync had errors", and read state is a
    /// nicety beside the messages themselves — a stale unread dot must not make a
    /// healthy archive look broken.
    async fn refresh_read_state(&self) {
        let receipts = match self.client.read_receipts().await {
            Ok(r) => r,
            Err(e) => {
                log::debug!("read receipts unavailable: {e}");
                return;
            }
        };

        let store = self.store.clone();
        let me = {
            let store = store.clone();
            match tokio::task::spawn_blocking(move || {
                lock_store(&store).get_meta("account_user_id")
            })
            .await
            {
                Ok(Ok(Some(id))) => id,
                _ => {
                    log::debug!("read receipts skipped: signed-in account not yet known");
                    return;
                }
            }
        };

        let pairs: Vec<(String, Option<String>)> = receipts
            .into_iter()
            .map(|r| (r.conversation_id, r.last_read_message_id))
            .collect();
        let total = pairs.len();

        let applied = tokio::task::spawn_blocking(move || {
            lock_store(&store).apply_read_receipts(&pairs, &me)
        })
        .await;
        match applied {
            Ok(Ok(n)) => {
                log::debug!("read receipts: {n} of {total} matched an archived conversation")
            }
            Ok(Err(e)) => log::warn!("applying read receipts: {e:#}"),
            Err(e) => log::warn!("applying read receipts: {e}"),
        }
    }

    /// The body of a cycle. Returns true when it stood down — a rejected token,
    /// or a 429 the client's own retries could not ride out — which is the one
    /// outcome the caller must not answer by coming straight back round.
    ///
    /// `ids` comes back filled with every conversation the cycle knew about, so
    /// the progress fields can be computed once at the end rather than by
    /// re-listing.
    async fn run_cycle(&self, report: &mut SyncReport, ids: &mut Vec<String>) -> bool {
        let now = now_unix();

        match self.client.groups().await {
            Ok(groups) => {
                // Collect the live ids before moving `groups` into the closure.
                let present_ids: Vec<String> = groups.iter().map(|g| g.id.clone()).collect();
                let store = self.store.clone();
                let written = tokio::task::spawn_blocking(move || {
                    let mut guard = lock_store(&store);
                    let mut errors = Vec::new();
                    for g in &groups {
                        // Metadata only — the lean list carries no members. Rosters
                        // are pulled per group below, once each.
                        if let Err(e) = guard.upsert_group_meta(g, now) {
                            errors.push(format!("store group {}: {e}", g.id));
                        }
                    }
                    // Mark groups absent from the live list as former. Intentionally
                    // called only here, on the success arm — an offline or filtered
                    // cycle must never mark every group former just because the list
                    // call failed or returned nothing.
                    if let Err(e) = guard.mark_former_groups(&present_ids) {
                        errors.push(format!("marking former groups: {e}"));
                    }
                    errors
                })
                .await;
                match written {
                    Ok(errors) => report.errors.extend(errors),
                    Err(e) => report.errors.push(format!("storing groups: {e}")),
                }
            }
            Err(e) => {
                let stop = self.stand_down(&e).await;
                report.errors.push(format!("listing groups: {e}"));
                if stop {
                    return true;
                }
            }
        }

        self.pace().await;

        match self.client.chats().await {
            Ok(chats) => {
                let store = self.store.clone();
                let written = tokio::task::spawn_blocking(move || {
                    let guard = lock_store(&store);
                    let mut errors = Vec::new();
                    for c in &chats {
                        if let Err(e) = guard.upsert_chat(c, now) {
                            errors.push(format!("store chat {}: {e}", c.other_user.id));
                        }
                    }
                    errors
                })
                .await;
                match written {
                    Ok(errors) => report.errors.extend(errors),
                    Err(e) => report.errors.push(format!("storing chats: {e}")),
                }
            }
            Err(e) => {
                let stop = self.stand_down(&e).await;
                report.errors.push(format!("listing chats: {e}"));
                if stop {
                    return true;
                }
            }
        }

        self.pace().await;
        self.refresh_read_state().await;

        // Member rosters, one group at a time. The lean list above carries no
        // members; fill them in per group, each a small response that clears the
        // size-based filter the full list trips. Bounded per cycle, and only
        // groups whose roster is not stored yet are fetched — so after the first
        // few cycles this loop finds nothing and the sync stays cheap.
        let need = {
            let store = self.store.clone();
            match tokio::task::spawn_blocking(move || lock_store(&store).groups_needing_members(40))
                .await
            {
                Ok(Ok(ids)) => ids,
                Ok(Err(e)) => {
                    report
                        .errors
                        .push(format!("listing groups needing members: {e}"));
                    Vec::new()
                }
                Err(e) => {
                    report
                        .errors
                        .push(format!("listing groups needing members: {e}"));
                    Vec::new()
                }
            }
        };
        if !need.is_empty() {
            log::info!("fetching member rosters for {} group(s)", need.len());
        }
        for id in need {
            match self.client.group_detail(&id).await {
                Ok(value) => match serde_json::from_value::<crate::model::Group>(value) {
                    Ok(g) => {
                        let store = self.store.clone();
                        let gid = id.clone();
                        match tokio::task::spawn_blocking(move || {
                            lock_store(&store).upsert_group(&g, now)
                        })
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => report.errors.push(format!("store members {gid}: {e}")),
                            Err(e) => report.errors.push(format!("store members {gid}: {e}")),
                        }
                    }
                    Err(e) => log::debug!("decode members for group {id}: {e}"),
                },
                // Best-effort: a roster that fails (filter, network) stays NULL and
                // is retried next cycle. It must not turn a good cycle into a bad
                // one — the group's name and metadata are already stored.
                Err(e) => log::debug!("group {id} roster deferred: {e}"),
            }
            self.pace().await;
        }

        // Read the list back from the archive rather than from the two API
        // responses: it is already ordered most-recent-first, so the cycle
        // spends its early requests where the user is most likely to look, and
        // it still yields work when the list calls themselves failed offline.
        let conversations = {
            let store = self.store.clone();
            let listed =
                tokio::task::spawn_blocking(move || lock_store(&store).list_conversations()).await;
            match listed {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    report.errors.push(format!("listing conversations: {e}"));
                    return true;
                }
                Err(e) => {
                    report.errors.push(format!("listing conversations: {e}"));
                    return true;
                }
            }
        };
        ids.extend(conversations.iter().map(|c| c.id.clone()));

        // Every cursor in one hop: primary-key lookups under a single lock.
        // This decides the shape of the whole cycle — which conversations have
        // never been tailed, and whether the engine is still catching up — so it
        // is read up front rather than discovered conversation by conversation.
        let states = self.sync_states(ids).await;

        // A conversation with no cursor and no completed backfill has never been
        // tailed: either the archive is cold, or the user joined it since the
        // last cycle. Either way it holds nothing the reader can show.
        let never_tailed = |c: &Conversation| match states.get(&c.id) {
            Some(s) => s.newest_id.is_none() && !s.backfill_complete,
            None => true,
        };
        // A missing state is an unread row, not a complete one — assume the
        // pessimistic answer rather than dropping to the gentle cadence.
        let catching_up = conversations
            .iter()
            .any(|c| states.get(&c.id).map_or(true, |s| !s.backfill_complete));
        let backfill_pages = if catching_up {
            self.config.catchup_backfill_pages_per_cycle
        } else {
            self.config.backfill_pages_per_cycle
        };

        // --- Pass 1: recent everywhere ---------------------------------------
        //
        // Newest page only, for every conversation that has never been tailed,
        // before any conversation walks backwards. Costs one request each and is
        // the difference between a reader that fills in half a minute and one
        // that fills over half an hour.
        //
        // Interruption-safe by construction: a conversation this pass never
        // reached still has no cursor, so the next cycle sweeps it again.
        let untouched: Vec<(String, ConversationKind)> = conversations
            .iter()
            .filter(|c| never_tailed(c))
            .map(|c| (c.id.clone(), c.kind))
            .collect();
        let mut swept: HashMap<String, bool> = HashMap::with_capacity(untouched.len());
        if !untouched.is_empty() {
            log::info!(
                "tailing {} conversation(s) before any backfill",
                untouched.len()
            );
            for (id, kind) in &untouched {
                match self
                    .sync_conversation_detail(id, *kind, Pass::tail_only())
                    .await
                {
                    Ok(outcome) => {
                        report.messages_inserted += outcome.inserted;
                        if outcome.backfill_completed {
                            report.backfills_completed += 1;
                        }
                        swept.insert(id.clone(), true);
                    }
                    Err(e) => {
                        swept.insert(id.clone(), false);
                        let stop = self.stand_down(&e).await;
                        report.errors.push(format!("conversation {id}: {e}"));
                        if stop {
                            return true;
                        }
                    }
                }
            }
        }

        // --- Pass 2: deep history --------------------------------------------
        for c in &conversations {
            report.conversations_seen += 1;
            let pass = match swept.get(&c.id) {
                // Failed moments ago in pass 1. Asking again inside the same
                // cycle would only put the identical error in the report twice.
                Some(false) => continue,
                Some(true) => Pass::backfill_only(backfill_pages),
                None => Pass::full(backfill_pages),
            };
            match self.sync_conversation_detail(&c.id, c.kind, pass).await {
                Ok(outcome) => {
                    report.messages_inserted += outcome.inserted;
                    if outcome.backfill_completed {
                        report.backfills_completed += 1;
                        log::info!("backfill complete for conversation {}", c.id);
                    }
                }
                Err(e) => {
                    let stop = self.stand_down(&e).await;
                    report.errors.push(format!("conversation {}: {e}", c.id));
                    if stop {
                        return true;
                    }
                }
            }
        }

        let media_cap = if catching_up {
            self.config.catchup_media_per_cycle
        } else {
            self.config.media_per_cycle
        };
        report.media_cached = self.cache_media_up_to(media_cap).await;
        false
    }

    /// Cursors for every conversation, read under one lock.
    ///
    /// A row that fails to read is simply absent: callers treat "unknown" as
    /// "never tailed", which costs one extra tail request and no correctness —
    /// every conversation re-reads its own state before it is synced.
    async fn sync_states(&self, ids: &[String]) -> HashMap<String, SyncState> {
        let store = self.store.clone();
        let wanted = ids.to_vec();
        let read = tokio::task::spawn_blocking(move || {
            let guard = lock_store(&store);
            let mut out: HashMap<String, SyncState> = HashMap::with_capacity(wanted.len());
            for id in wanted {
                match guard.get_sync_state(&id) {
                    Ok(s) => {
                        out.insert(id, s);
                    }
                    Err(e) => log::warn!("reading sync state for {id}: {e}"),
                }
            }
            out
        })
        .await;
        match read {
            Ok(states) => states,
            Err(e) => {
                log::warn!("reading sync state: {e}");
                HashMap::new()
            }
        }
    }

    /// Fills in the progress fields and decides whether the caller should come
    /// straight back round.
    async fn finish(&self, mut report: SyncReport, ids: &[String], stood_down: bool) -> SyncReport {
        let store = self.store.clone();
        let wanted = ids.to_vec();
        // One hop for both reads. `total_message_count` is a COUNT over an
        // index, and a cycle can never be shorter than the paced requests it
        // just made, so it is noise next to the work it summarises.
        let read = tokio::task::spawn_blocking(move || {
            let guard = lock_store(&store);
            let backfilling = wanted
                .iter()
                .filter(|id| {
                    guard
                        .get_sync_state(id)
                        .map(|s| !s.backfill_complete)
                        .unwrap_or(false)
                })
                .count();
            (backfilling, guard.total_message_count().unwrap_or(0))
        })
        .await;
        match read {
            Ok((backfilling, total)) => {
                report.conversations_backfilling = backfilling;
                report.total_archived = total;
            }
            Err(e) => log::warn!("summarising archive progress: {e}"),
        }

        let progressed = report.messages_inserted > 0
            || report.backfills_completed > 0
            || report.media_cached > 0;
        report.more_work = !stood_down
            && progressed
            && (report.conversations_backfilling > 0 || report.media_cached > 0);
        report
    }

    /// Decides whether an API failure ends the cycle, and pays the 429 penalty
    /// when there is one.
    ///
    /// A rejected token fails identically for everything that follows. A 429
    /// that outlived the client's own retry/backoff means the account is already
    /// over the line, and pressing on — even at 8 req/s — is how it gets
    /// flagged. The pause lives here rather than in the caller so it applies
    /// wherever the error surfaced.
    async fn stand_down(&self, err: &ApiError) -> bool {
        match err {
            ApiError::Unauthorized => {
                log::warn!("access token rejected; abandoning sync cycle");
                true
            }
            ApiError::RateLimited { attempts } => {
                log::warn!(
                    "rate limited by GroupMe after {attempts} attempts; \
                     pausing the sync cycle for {:?}",
                    self.config.rate_limit_backoff
                );
                if !self.config.rate_limit_backoff.is_zero() {
                    tokio::time::sleep(self.config.rate_limit_backoff).await;
                }
                true
            }
            _ => false,
        }
    }

    /// Tail then backfill a single conversation. Returns rows written.
    ///
    /// The manual entry point, so it takes the gentle page budget: the catch-up
    /// budget belongs to `sync_once`, which is the only caller that knows
    /// whether the archive as a whole is still filling.
    pub async fn sync_conversation(
        &self,
        id: &str,
        kind: ConversationKind,
    ) -> Result<usize, ApiError> {
        Ok(self
            .sync_conversation_detail(id, kind, Pass::full(self.config.backfill_pages_per_cycle))
            .await?
            .inserted)
    }

    async fn sync_conversation_detail(
        &self,
        id: &str,
        kind: ConversationKind,
        pass: Pass,
    ) -> Result<ConversationOutcome, ApiError> {
        let mut state = {
            let store = self.store.clone();
            let id = id.to_string();
            tokio::task::spawn_blocking(move || lock_store(&store).get_sync_state(&id))
                .await
                .map_err(|e| join_error("reading sync state", e))?
                .map_err(|e| store_error("reading sync state", e))?
        };
        let mut outcome = ConversationOutcome::default();

        // --- Phase 1: tail -------------------------------------------------
        if pass.tail {
            loop {
                // No cursor yet means nothing of this conversation is held at
                // all; `Latest` seeds both ends in one request.
                let seeding = state.newest_id.is_none();
                let cursor = match &state.newest_id {
                    Some(newest) => Cursor::After(newest.clone()),
                    None => Cursor::Latest,
                };

                // `Arc` so the page can be handed to the blocking writer without
                // deep-copying 100 messages; `page.len()` is still needed after.
                let page = Arc::new(self.fetch_page(id, kind, &cursor).await?);
                if page.is_empty() {
                    if seeding {
                        // `Latest` came back empty: the conversation holds
                        // nothing at all, so there is no history to walk either.
                        // Recording that now is what keeps an empty group out of
                        // the never-tailed set — otherwise it would have no
                        // cursor forever, re-triggering the recent-first pass
                        // every cycle and never letting the engine go idle.
                        state.backfill_complete = true;
                        state.last_sync_at = Some(now_unix());
                        outcome.backfill_completed = true;
                        self.persist_state(&state).await?;
                    }
                    break;
                }

                let previous_newest = state.newest_id.clone();
                outcome.inserted += self.write_page(id, Arc::clone(&page), &mut state).await?;

                if seeding {
                    // `Latest` is by definition the newest page; nothing follows.
                    break;
                }
                if self.config.page_limit > 0 && page.len() < self.config.page_limit as usize {
                    // A short page while tailing means we have caught up. This
                    // is safe here and *not* safe in the backfill below: worst
                    // case we re-check next cycle, whereas a truncated backfill
                    // is silent.
                    break;
                }
                if state.newest_id == previous_newest {
                    // The server returned messages but the cursor did not
                    // advance, so `after_id` is not being honoured. Looping
                    // would never end.
                    log::warn!("tail cursor for {id} did not advance; stopping");
                    break;
                }
            }
        }

        // --- Phase 2: backfill ---------------------------------------------
        if state.backfill_complete {
            return Ok(outcome);
        }

        for _ in 0..pass.backfill_pages {
            let cursor = match &state.oldest_id {
                Some(oldest) => Cursor::Before(oldest.clone()),
                None => Cursor::Latest,
            };

            let page = Arc::new(self.fetch_page(id, kind, &cursor).await?);
            if page.is_empty() {
                // The terminator. Only an empty page proves we have reached the
                // start of history.
                state.backfill_complete = true;
                state.last_sync_at = Some(now_unix());
                outcome.backfill_completed = true;
                self.persist_state(&state).await?;
                break;
            }

            let previous_oldest = state.oldest_id.clone();
            outcome.inserted += self.write_page(id, page, &mut state).await?;
            if state.oldest_id == previous_oldest {
                log::warn!("backfill cursor for {id} did not advance; stopping");
                break;
            }
        }

        Ok(outcome)
    }

    /// Download every queued asset the steady-state budget allows.
    ///
    /// Never fails the cycle: avatars return 403 routinely and attachment hosts
    /// time out, and neither is a reason to stop archiving text.
    pub async fn cache_pending_media(&self) -> usize {
        self.cache_media_up_to(self.config.media_per_cycle).await
    }

    async fn cache_media_up_to(&self, cap: usize) -> usize {
        if cap == 0 {
            return 0;
        }
        // Filesystem calls block too, so they go to the same pool as the store.
        // `tokio::fs` is not an option here: the tokio dependency does not carry
        // the `fs` feature.
        let dir = self.media_dir.clone();
        let created = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir)).await;
        let failure = match created {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = failure {
            log::error!("creating media directory {}: {e}", self.media_dir.display());
            return 0;
        }

        // Avatars get a reserved share of the budget. They are small, stable and
        // few, and they are the difference between an offline reader that looks
        // like the app and one that looks like a wall of broken images — a busy
        // archive would otherwise spend every cycle on attachments alone.
        let avatar_cap = (cap / 2).max(1) as i64;
        let pending = {
            let store = self.store.clone();
            let queued = tokio::task::spawn_blocking(move || {
                let guard = lock_store(&store);
                let mut urls = match guard.uncached_avatar_urls(avatar_cap) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("querying uncached avatars: {e}");
                        Vec::new()
                    }
                };
                let remaining = cap.saturating_sub(urls.len()) as i64;
                if remaining > 0 {
                    match guard.uncached_media_urls(remaining) {
                        Ok(v) => urls.extend(v),
                        Err(e) => log::warn!("querying uncached media: {e}"),
                    }
                }
                urls
            })
            .await;
            match queued {
                Ok(urls) => urls,
                Err(e) => {
                    log::warn!("querying the media queue: {e}");
                    return 0;
                }
            }
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut cached = 0usize;

        for url in pending {
            if url.is_empty() || !seen.insert(url.clone()) {
                continue;
            }
            if self.blocked_media.lock().await.contains(&url) {
                continue;
            }

            self.pace().await;
            let (bytes, content_type) = match self.client.fetch_bytes(&url).await {
                Ok(v) => v,
                Err(e) => {
                    if is_rate_limited(&e) {
                        // The CDN throttling us is the same signal as the API
                        // throttling us. `fetch_bytes` has no retry of its own,
                        // so the whole media pass stands down for this cycle
                        // rather than working through another 199 downloads.
                        log::warn!("rate limited fetching media; ending the media pass");
                        if !self.config.rate_limit_backoff.is_zero() {
                            tokio::time::sleep(self.config.rate_limit_backoff).await;
                        }
                        break;
                    }
                    if is_permanent(&e) {
                        self.blocked_media.lock().await.insert(url.clone());
                        // Persist permanently unavailable state so the URL stays
                        // out of the uncached_media_urls queue across restarts.
                        // Best-effort: a store error here must not break the pass.
                        let store_c = self.store.clone();
                        let url_c = url.clone();
                        let r = tokio::task::spawn_blocking(move || {
                            lock_store(&store_c).mark_media_unavailable(&url_c)
                        })
                        .await;
                        if let Ok(Err(ref e2)) = r {
                            log::debug!("persisting permanent media failure for {url}: {e2}");
                        }
                        // Expected, and there is one per attachment: GroupMe's
                        // attachment URLs redirect to expiring signed CDN links,
                        // so an old image answers 403 forever (§10). Logging each
                        // one floods the file and rotates away the lines that
                        // matter — it has already buried two real diagnoses.
                        // The blocked set means each URL is only tried once, so
                        // `trace` still shows the full list when asked for.
                        log::trace!("media permanently unavailable: {url}: {e}");
                    } else {
                        log::debug!("media fetch failed for {url}: {e}");
                    }
                    continue;
                }
            };

            let path = self
                .media_dir
                .join(blob_name(&url, content_type.as_deref()));

            // Blob then row, in one hop off the runtime: a 5 MB write is 50-100
            // ms of blocking work, and the cache row must never point at a file
            // that was not written.
            let store = self.store.clone();
            let recorded = tokio::task::spawn_blocking(move || -> Result<(), String> {
                std::fs::write(&path, &bytes)
                    .map_err(|e| format!("writing media blob {}: {e}", path.display()))?;
                lock_store(&store)
                    .put_media(
                        &url,
                        &path.to_string_lossy(),
                        content_type.as_deref(),
                        bytes.len() as i64,
                        now_unix(),
                    )
                    .map_err(|e| format!("recording media {url}: {e}"))
            })
            .await;

            match recorded {
                Ok(Ok(())) => cached += 1,
                Ok(Err(e)) => log::warn!("{e}"),
                Err(e) => log::warn!("caching media: {e}"),
            }
        }

        cached
    }

    // ---------------------------------------------------------------- internals

    /// Persist cursors that changed without a page write behind them.
    async fn persist_state(&self, state: &SyncState) -> Result<(), ApiError> {
        let store = self.store.clone();
        let snapshot = state.clone();
        tokio::task::spawn_blocking(move || lock_store(&store).put_sync_state(&snapshot))
            .await
            .map_err(|e| join_error("writing sync state", e))?
            .map_err(|e| store_error("writing sync state", e))
    }

    async fn fetch_page(
        &self,
        id: &str,
        kind: ConversationKind,
        cursor: &Cursor,
    ) -> Result<Vec<Message>, ApiError> {
        self.pace().await;
        match kind {
            ConversationKind::Group => self.client.group_messages(id, cursor).await,
            ConversationKind::Dm => self.client.direct_messages(id, cursor).await,
        }
    }

    /// Write one page and advance the persisted cursors.
    ///
    /// One `spawn_blocking` for the whole page, not one per message: the insert
    /// is already a single transaction, and a hop per message would cost more
    /// than it saves. The lock is taken inside that closure and released when it
    /// returns — never around the request that produced `page`.
    async fn write_page(
        &self,
        conversation_id: &str,
        page: Arc<Vec<Message>>,
        state: &mut SyncState,
    ) -> Result<usize, ApiError> {
        let store = self.store.clone();
        let conversation_id = conversation_id.to_string();
        // The cursors advance on the blocking side and come back with the row
        // count, so a failed write leaves the caller's `state` untouched.
        let mut working = state.clone();

        let (inserted, working) = tokio::task::spawn_blocking(move || {
            let mut guard = lock_store(&store);

            let inserted = guard
                .insert_messages(&conversation_id, &page)
                .map_err(|e| store_error("inserting messages", e))?;

            // Edits and deletions land twice: the original row is rewritten in
            // place AND a separate system message is appended. We apply the
            // event anyway, because a backfill can hand us the event long after
            // it has handed us the stale original. Applied after the insert so
            // an event can target a message that arrived in the same page.
            //
            // Gated on `event` being present, NOT on `system == true`: group
            // messages carry `system`, but DM messages omit the field entirely,
            // so it deserializes to false and every DM edit and delete would be
            // silently skipped. The `event` object is what actually describes
            // the mutation; `system` is only a display flag.
            for m in page.iter() {
                let Some(event) = &m.event else { continue };
                if !matches!(
                    event.kind.as_deref(),
                    Some("message.update") | Some("message.deleted")
                ) {
                    continue;
                }
                match guard.apply_event(event) {
                    Ok(true) => log::debug!("applied {:?} from message {}", event.kind, m.id),
                    Ok(false) => {}
                    Err(e) => log::warn!("applying {:?} from message {}: {e}", event.kind, m.id),
                }
            }

            advance_cursors(&mut working, &page);
            working.last_sync_at = Some(now_unix());
            guard
                .put_sync_state(&working)
                .map_err(|e| store_error("writing sync state", e))?;

            Ok::<_, ApiError>((inserted, working))
        })
        .await
        .map_err(|e| join_error("writing a message page", e))??;

        *state = working;
        Ok(inserted)
    }

    async fn pace(&self) {
        if !self.config.request_spacing.is_zero() {
            tokio::time::sleep(self.config.request_spacing).await;
        }
    }
}

#[derive(Debug, Default)]
struct ConversationOutcome {
    inserted: usize,
    backfill_completed: bool,
}

/// How much of one conversation a single visit is allowed to do.
///
/// The cycle visits a cold conversation twice: once to tail it alongside every
/// other cold conversation, and again to walk its history. Splitting the two
/// phases is what lets "recent everywhere" run before "complete anywhere"
/// without paying for a second tail request.
#[derive(Debug, Clone, Copy)]
struct Pass {
    tail: bool,
    backfill_pages: usize,
}

impl Pass {
    fn tail_only() -> Self {
        Self {
            tail: true,
            backfill_pages: 0,
        }
    }

    fn backfill_only(pages: usize) -> Self {
        Self {
            tail: false,
            backfill_pages: pages,
        }
    }

    fn full(pages: usize) -> Self {
        Self {
            tail: true,
            backfill_pages: pages,
        }
    }
}

/// Recompute both cursors from a page.
///
/// Every id is compared as an integer via `id_sort_key`. Lexicographic
/// comparison is wrong the moment ids differ in length ("99" > "100" as text),
/// and page order is not trusted because the two ends of the archive depend on
/// these values being right.
fn advance_cursors(state: &mut SyncState, page: &[Message]) {
    for m in page {
        let key = id_sort_key(&m.id);
        // A non-numeric id yields 0, which would look older than everything and
        // then be handed back as a `before_id` the server cannot use.
        if key <= 0 {
            continue;
        }
        let extends_back = match state.oldest_id.as_deref() {
            Some(current) => key < id_sort_key(current),
            None => true,
        };
        if extends_back {
            state.oldest_id = Some(m.id.clone());
        }

        let extends_forward = match state.newest_id.as_deref() {
            Some(current) => key > id_sort_key(current),
            None => true,
        };
        if extends_forward {
            state.newest_id = Some(m.id.clone());
        }
    }
}

/// A 4xx on an asset is the asset's final answer — GroupMe serves 403 for some
/// avatars and 404 for objects that have aged out. Anything else may be
/// transient and stays queued.
///
/// 429 is the exception inside that range: it is a statement about the *rate*,
/// not the asset, and blocking the URL for the life of the process would
/// permanently lose a perfectly good attachment because we asked too fast.
pub(crate) fn is_permanent(err: &ApiError) -> bool {
    match err {
        ApiError::NotFound => true,
        ApiError::Status { status, .. } => (400..500).contains(status) && *status != 429,
        _ => false,
    }
}

/// `fetch_bytes` bypasses the retrying request path, so a throttled CDN arrives
/// as a plain 429 status rather than `ApiError::RateLimited`.
fn is_rate_limited(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::RateLimited { .. } | ApiError::Status { status: 429, .. }
    )
}

/// Content-addressed blob name. Hashing the URL keeps the filename stable
/// across runs, safe on Windows, and independent of the query string that a
/// signed CDN URL drags along.
fn blob_name(url: &str, content_type: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let mut name = hex(&hasher.finalize());
    name.push_str(extension_for(content_type));
    name
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: [u8; 16] = *b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// The URL path is useless for this: the `m.groupme.com` form carries a
/// rendition suffix and the signed CDN form carries a query string, so the
/// response's own content type is the only honest source.
fn extension_for(content_type: Option<&str>) -> &'static str {
    let raw = content_type.unwrap_or("");
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "image/heic" => ".heic",
        "image/svg+xml" => ".svg",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/mp4" | "audio/x-m4a" => ".m4a",
        "audio/ogg" => ".ogg",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        _ => ".bin",
    }
}

/// `ApiError` has no storage variant and cannot grow one from here, so archive
/// failures ride out on `Decode`. The log line is the real diagnostic; the
/// string exists so the failure reaches `SyncReport.errors` instead of vanishing.
fn store_error(context: &str, err: anyhow::Error) -> ApiError {
    log::error!("archive failure while {context}: {err:#}");
    ApiError::Decode(format!("archive: {context}: {err}"))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const G1_MESSAGES: &str = r"^.*/groups/g1/messages$";
    const G2_MESSAGES: &str = r"^.*/groups/g2/messages$";
    const GROUPS_LIST: &str = r"^.*/groups$";
    const CHATS_LIST: &str = r"^.*/chats$";

    fn memory_store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    fn fast() -> SyncConfig {
        SyncConfig {
            request_spacing: Duration::ZERO,
            rate_limit_backoff: Duration::ZERO,
            ..Default::default()
        }
    }

    fn engine(
        server: &MockServer,
        store: Arc<Mutex<Store>>,
        dir: &tempfile::TempDir,
        config: SyncConfig,
    ) -> SyncEngine {
        let client = GroupMeClient::with_base_url("test-token", server.uri()).unwrap();
        SyncEngine::with_config(client, store, dir.path().to_path_buf(), config)
    }

    fn msg(id: &str, text: &str) -> serde_json::Value {
        json!({
            "id": id, "user_id": "u1", "name": "Test Sender",
            "text": text, "created_at": 1, "system": false, "attachments": []
        })
    }

    fn group_page(messages: Vec<serde_json::Value>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "meta": { "code": 200 },
            "response": { "count": messages.len(), "messages": messages }
        }))
    }

    fn envelope(response: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({ "meta": { "code": 200 }, "response": response }))
    }

    /// Specific cursor mocks outrank the catch-all that serves `Cursor::Latest`.
    /// Priority 1 is the highest; wiremock's default is 5.
    async fn mount_cursor(
        server: &MockServer,
        path_pattern: &str,
        param: &str,
        value: &str,
        page: Vec<serde_json::Value>,
    ) {
        Mock::given(method("GET"))
            .and(path_regex(path_pattern))
            .and(query_param(param, value))
            .respond_with(group_page(page))
            .with_priority(1)
            .mount(server)
            .await;
    }

    async fn mount_latest(server: &MockServer, path_pattern: &str, page: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path_regex(path_pattern))
            .respond_with(group_page(page))
            .mount(server)
            .await;
    }

    async fn mount_conversation_lists(server: &MockServer, groups: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path_regex(GROUPS_LIST))
            .respond_with(envelope(groups))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(CHATS_LIST))
            .respond_with(envelope(json!([])))
            .mount(server)
            .await;
    }

    async fn queries_containing(server: &MockServer, needle: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.query().unwrap_or("").contains(needle))
            .count()
    }

    async fn paths_ending_with(server: &MockServer, suffix: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with(suffix))
            .count()
    }

    fn state_of(store: &Arc<Mutex<Store>>, id: &str) -> SyncState {
        lock_store(store).get_sync_state(id).unwrap()
    }

    fn count_in(store: &Arc<Mutex<Store>>, id: &str) -> i64 {
        lock_store(store).message_count(id).unwrap()
    }

    /// Latest page + one older page + the empty terminator.
    async fn mount_three_page_history(server: &MockServer) {
        mount_cursor(
            server,
            G1_MESSAGES,
            "before_id",
            "201",
            vec![msg("101", "old one"), msg("102", "old two")],
        )
        .await;
        mount_cursor(server, G1_MESSAGES, "before_id", "101", vec![]).await;
        mount_latest(
            server,
            G1_MESSAGES,
            vec![msg("201", "recent one"), msg("202", "recent two")],
        )
        .await;
    }

    #[tokio::test]
    async fn fresh_conversation_backfills_until_the_empty_page() {
        let server = MockServer::start().await;
        mount_three_page_history(&server).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        let written = engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(written, 4);
        assert_eq!(count_in(&store, "g1"), 4);

        let state = state_of(&store, "g1");
        assert!(
            state.backfill_complete,
            "an empty page must close the backfill"
        );
        assert_eq!(state.oldest_id.as_deref(), Some("101"));
        assert_eq!(state.newest_id.as_deref(), Some("202"));
    }

    #[tokio::test]
    async fn a_short_page_does_not_end_the_backfill() {
        // Two messages against a page_limit of 100 is a short page, and it sits
        // mid-history. Only the empty page that follows may terminate.
        let server = MockServer::start().await;
        mount_three_page_history(&server).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(
            count_in(&store, "g1"),
            4,
            "the short page truncated the archive"
        );
    }

    #[tokio::test]
    async fn a_completed_backfill_is_never_walked_again() {
        let server = MockServer::start().await;
        mount_three_page_history(&server).await;
        mount_cursor(&server, G1_MESSAGES, "after_id", "202", vec![]).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        let after_first = queries_containing(&server, "before_id").await;
        assert_eq!(after_first, 2);

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(
            queries_containing(&server, "before_id").await,
            after_first,
            "a completed conversation issued a backfill request"
        );
    }

    #[tokio::test]
    async fn overlapping_pages_do_not_duplicate_rows() {
        let server = MockServer::start().await;
        mount_three_page_history(&server).await;
        // An inclusive `after_id` hands back a message we already hold. Sync
        // re-reads overlapping ranges constantly; it must be a no-op on count.
        mount_cursor(
            &server,
            G1_MESSAGES,
            "after_id",
            "202",
            vec![msg("202", "recent two")],
        )
        .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();

        assert_eq!(count_in(&store, "g1"), 4);
        let state = state_of(&store, "g1");
        assert_eq!(state.newest_id.as_deref(), Some("202"));
        assert_eq!(state.oldest_id.as_deref(), Some("101"));
    }

    #[tokio::test]
    async fn cursors_compare_ids_as_integers_not_strings() {
        // "99" sorts above "100" as text. Getting this wrong points the next
        // `before_id` at the wrong end of history.
        let server = MockServer::start().await;
        mount_cursor(&server, G1_MESSAGES, "before_id", "99", vec![]).await;
        // Deliberately not in ascending order: page order is not trusted.
        mount_latest(
            &server,
            G1_MESSAGES,
            vec![msg("100", "newer"), msg("99", "older")],
        )
        .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();

        let state = state_of(&store, "g1");
        assert_eq!(
            state.newest_id.as_deref(),
            Some("100"),
            "string comparison picked \"99\" as newest"
        );
        assert_eq!(state.oldest_id.as_deref(), Some("99"));
    }

    #[tokio::test]
    async fn backfill_is_capped_per_cycle() {
        let server = MockServer::start().await;
        mount_cursor(
            &server,
            G1_MESSAGES,
            "before_id",
            "301",
            vec![msg("201", "b"), msg("202", "b")],
        )
        .await;
        mount_cursor(
            &server,
            G1_MESSAGES,
            "before_id",
            "201",
            vec![msg("101", "c"), msg("102", "c")],
        )
        .await;
        mount_cursor(&server, G1_MESSAGES, "before_id", "101", vec![]).await;
        mount_latest(&server, G1_MESSAGES, vec![msg("301", "a"), msg("302", "a")]).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(
            &server,
            store.clone(),
            &dir,
            SyncConfig {
                backfill_pages_per_cycle: 1,
                ..fast()
            },
        );

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(queries_containing(&server, "before_id").await, 1);
        let state = state_of(&store, "g1");
        assert!(!state.backfill_complete);
        assert_eq!(state.oldest_id.as_deref(), Some("201"));

        // The cursor is persisted, so the next cycle resumes rather than restarts.
        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(state_of(&store, "g1").oldest_id.as_deref(), Some("101"));
    }

    #[tokio::test]
    async fn delete_event_marks_the_target_deleted() {
        let server = MockServer::start().await;
        let deletion = json!({
            "id": "101", "user_id": "system", "sender_id": "system",
            "text": "A message was deleted", "created_at": 2, "system": true,
            "attachments": [],
            "event": { "type": "message.deleted",
                       "data": { "message_id": "100", "deleted_at": 1784663704 } }
        });
        mount_cursor(&server, G1_MESSAGES, "before_id", "100", vec![]).await;
        mount_latest(&server, G1_MESSAGES, vec![msg("100", "secret"), deletion]).await;

        let dir = tempfile::tempdir().unwrap();
        // File-backed so the assertion can read the column through a second
        // connection; `deleted_at` is not part of the message JSON.
        let db_path = dir.path().join("archive.db");
        let store = Arc::new(Mutex::new(Store::open(&db_path).unwrap()));
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();

        let probe = rusqlite::Connection::open(&db_path).unwrap();
        let deleted_at: Option<i64> = probe
            .query_row(
                "SELECT deleted_at FROM messages WHERE id = '100'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted_at, Some(1784663704));
        assert_eq!(count_in(&store, "g1"), 2, "the tombstone row must survive");
    }

    #[tokio::test]
    async fn media_is_downloaded_to_disk_and_recorded() {
        let server = MockServer::start().await;
        let media_url = format!("{}/uploads/photo", server.uri());
        let bytes: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

        let attachment_msg = json!({
            "id": "100", "user_id": "u1", "name": "Test Sender", "text": null,
            "created_at": 1, "system": false,
            "attachments": [{ "type": "image", "url": media_url.clone() }]
        });
        mount_cursor(&server, G1_MESSAGES, "before_id", "100", vec![]).await;
        mount_latest(&server, G1_MESSAGES, vec![attachment_msg]).await;
        Mock::given(method("GET"))
            .and(path("/uploads/photo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes.clone())
                    .insert_header("content-type", "image/png"),
            )
            .mount(&server)
            .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();
        assert_eq!(engine.cache_pending_media().await, 1);

        let recorded = lock_store(&store).get_media(&media_url).unwrap();
        let recorded = recorded.expect("media was not recorded");
        assert!(
            recorded.ends_with(".png"),
            "extension must come from the content type: {recorded}"
        );
        // Storing the URL is not archiving the asset; only the bytes are.
        assert_eq!(std::fs::read(&recorded).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_media_404_is_skipped_without_failing_the_cycle() {
        let server = MockServer::start().await;
        let gone = format!("{}/uploads/gone", server.uri());
        let good = format!("{}/uploads/good", server.uri());

        let msgs = vec![
            json!({ "id": "100", "user_id": "u1", "name": "S", "created_at": 1,
                    "system": false,
                    "attachments": [{ "type": "image", "url": gone.clone() }] }),
            json!({ "id": "101", "user_id": "u1", "name": "S", "created_at": 2,
                    "system": false,
                    "attachments": [{ "type": "image", "url": good.clone() }] }),
        ];
        mount_cursor(&server, G1_MESSAGES, "before_id", "100", vec![]).await;
        mount_latest(&server, G1_MESSAGES, msgs).await;
        Mock::given(method("GET"))
            .and(path("/uploads/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/uploads/good"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(vec![1u8, 2, 3])
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        engine
            .sync_conversation("g1", ConversationKind::Group)
            .await
            .unwrap();

        assert_eq!(
            engine.cache_pending_media().await,
            1,
            "the healthy asset must still be cached"
        );
        assert!(lock_store(&store).get_media(&gone).unwrap().is_none());
        assert!(lock_store(&store).get_media(&good).unwrap().is_some());

        // A permanently dead asset must not be retried forever: it sits at the
        // front of a LIMITed queue and would block everything behind it.
        engine.cache_pending_media().await;
        assert_eq!(paths_ending_with(&server, "/uploads/gone").await, 1);
    }

    #[tokio::test]
    async fn unauthorized_aborts_the_whole_cycle() {
        let server = MockServer::start().await;
        mount_conversation_lists(
            &server,
            json!([
                { "id": "g1", "name": "First", "updated_at": 200 },
                { "id": "g2", "name": "Second", "updated_at": 100 },
            ]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path_regex(G1_MESSAGES))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(json!({ "meta": { "code": 401, "errors": ["unauthorized"] } })),
            )
            .mount(&server)
            .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store, &dir, fast());

        let report = engine.sync_once().await;
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(
            paths_ending_with(&server, "/groups/g2/messages").await,
            0,
            "a dead token must not be hammered against the remaining conversations"
        );
    }

    #[tokio::test]
    async fn one_failing_conversation_does_not_stop_the_others() {
        let server = MockServer::start().await;
        mount_conversation_lists(
            &server,
            json!([
                { "id": "g1", "name": "Broken", "updated_at": 200 },
                { "id": "g2", "name": "Healthy", "updated_at": 100 },
            ]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path_regex(G1_MESSAGES))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        mount_cursor(&server, G2_MESSAGES, "before_id", "201", vec![]).await;
        mount_latest(
            &server,
            G2_MESSAGES,
            vec![msg("201", "one"), msg("202", "two")],
        )
        .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        let report = engine.sync_once().await;
        assert_eq!(report.conversations_seen, 2);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert_eq!(report.messages_inserted, 2);
        assert_eq!(report.backfills_completed, 1);
        assert_eq!(count_in(&store, "g2"), 2);
    }

    // --- Catch-up mode ---------------------------------------------------

    #[tokio::test]
    async fn more_work_is_set_until_every_backfill_is_complete() {
        // The caller loops immediately on `more_work` instead of sleeping a
        // minute, so a cycle that leaves history unwalked has to say so.
        let server = MockServer::start().await;
        mount_conversation_lists(&server, json!([{ "id": "g1", "name": "One" }])).await;
        mount_cursor(
            &server,
            G1_MESSAGES,
            "before_id",
            "201",
            vec![msg("101", "older one"), msg("102", "older two")],
        )
        .await;
        mount_cursor(&server, G1_MESSAGES, "before_id", "101", vec![]).await;
        mount_cursor(&server, G1_MESSAGES, "after_id", "202", vec![]).await;
        mount_latest(
            &server,
            G1_MESSAGES,
            vec![msg("201", "recent one"), msg("202", "recent two")],
        )
        .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(
            &server,
            store.clone(),
            &dir,
            SyncConfig {
                catchup_backfill_pages_per_cycle: 1,
                ..fast()
            },
        );

        let first = engine.sync_once().await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(
            first.more_work,
            "history remains but the caller was told to go to sleep"
        );
        assert_eq!(first.conversations_backfilling, 1);
        assert_eq!(first.total_archived, 4);

        let second = engine.sync_once().await;
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert_eq!(second.backfills_completed, 1);
        assert_eq!(second.conversations_backfilling, 0);
        assert!(
            !second.more_work,
            "a fully archived account must fall back to the gentle cadence"
        );
        assert_eq!(second.total_archived, 4);
    }

    #[tokio::test]
    async fn a_cold_archive_tails_every_conversation_before_any_backfill() {
        // The failure this prevents: conversation-at-a-time processing, where
        // the last chat in the list shows nothing until every earlier one has
        // spent its whole backfill budget.
        let server = MockServer::start().await;
        mount_conversation_lists(
            &server,
            json!([
                { "id": "g1", "name": "First", "updated_at": 200 },
                { "id": "g2", "name": "Second", "updated_at": 100 },
            ]),
        )
        .await;
        mount_cursor(
            &server,
            G1_MESSAGES,
            "before_id",
            "301",
            vec![msg("201", "g1 old"), msg("202", "g1 old")],
        )
        .await;
        mount_cursor(&server, G1_MESSAGES, "before_id", "201", vec![]).await;
        mount_latest(
            &server,
            G1_MESSAGES,
            vec![msg("301", "g1 new"), msg("302", "g1 new")],
        )
        .await;
        mount_cursor(&server, G2_MESSAGES, "before_id", "401", vec![]).await;
        mount_latest(
            &server,
            G2_MESSAGES,
            vec![msg("401", "g2 new"), msg("402", "g2 new")],
        )
        .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        let report = engine.sync_once().await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let requests = server.received_requests().await.unwrap_or_default();
        let messages: Vec<String> = requests
            .iter()
            .filter(|r| r.url.path().contains("/groups/") && r.url.path().ends_with("/messages"))
            .map(|r| format!("{}?{}", r.url.path(), r.url.query().unwrap_or("")))
            .collect();
        let first_backfill = messages
            .iter()
            .position(|q| q.contains("before_id"))
            .expect("no backfill ran at all");
        let tails = &messages[..first_backfill];
        for group in ["/groups/g1/messages", "/groups/g2/messages"] {
            assert!(
                tails.iter().any(|q| q.starts_with(group)),
                "{group} had not been tailed before the first backfill request: {messages:?}"
            );
        }
        assert_eq!(count_in(&store, "g1"), 4);
        assert_eq!(count_in(&store, "g2"), 2);
    }

    #[tokio::test]
    async fn the_catch_up_cap_applies_while_catching_up_and_not_once_done() {
        let server = MockServer::start().await;
        mount_conversation_lists(&server, json!([{ "id": "g1", "name": "Deep" }])).await;
        for (from, page) in [
            ("501", vec![msg("401", "d"), msg("402", "d")]),
            ("401", vec![msg("301", "c"), msg("302", "c")]),
            ("301", vec![msg("201", "b"), msg("202", "b")]),
            ("201", vec![]),
        ] {
            mount_cursor(&server, G1_MESSAGES, "before_id", from, page).await;
        }
        mount_cursor(&server, G1_MESSAGES, "after_id", "502", vec![]).await;
        mount_latest(&server, G1_MESSAGES, vec![msg("501", "a"), msg("502", "a")]).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(
            &server,
            store.clone(),
            &dir,
            SyncConfig {
                backfill_pages_per_cycle: 1,
                catchup_backfill_pages_per_cycle: 3,
                ..fast()
            },
        );

        engine.sync_once().await;
        assert_eq!(
            queries_containing(&server, "before_id").await,
            3,
            "an incomplete archive was held to the gentle steady-state cap"
        );

        let second = engine.sync_once().await;
        assert_eq!(queries_containing(&server, "before_id").await, 4);
        assert!(!second.more_work);
        assert_eq!(count_in(&store, "g1"), 8);

        // Caught up: the cadence drops to tail-only, so no page budget of
        // either size is spent. The gentle cap still governs the manual
        // single-conversation path — see `backfill_is_capped_per_cycle`.
        engine.sync_once().await;
        assert_eq!(
            queries_containing(&server, "before_id").await,
            4,
            "a completed archive kept walking backwards"
        );
        assert_eq!(
            queries_containing(&server, "after_id").await,
            2,
            "steady state must still tail"
        );
    }

    #[tokio::test]
    async fn an_empty_conversation_is_recorded_as_complete() {
        // A group with no messages never gets a cursor, so without this it would
        // look "never tailed" forever: swept again every cycle, and counted as
        // outstanding work that keeps the caller looping at full speed.
        let server = MockServer::start().await;
        mount_conversation_lists(&server, json!([{ "id": "g1", "name": "Silent" }])).await;
        mount_latest(&server, G1_MESSAGES, vec![]).await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        let report = engine.sync_once().await;
        assert!(state_of(&store, "g1").backfill_complete);
        assert_eq!(report.conversations_backfilling, 0);
        assert!(!report.more_work);
        assert_eq!(
            paths_ending_with(&server, "/groups/g1/messages").await,
            1,
            "an empty conversation cost more than the one request that proved it empty"
        );
    }

    #[tokio::test]
    async fn dm_conversations_use_the_direct_message_endpoint() {
        let server = MockServer::start().await;
        let dm_path = r"^.*/direct_messages$";
        Mock::given(method("GET"))
            .and(path_regex(dm_path))
            .and(query_param("before_id", "201"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": { "code": 200 },
                "response": { "count": 0, "direct_messages": [] }
            })))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(dm_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": { "code": 200 },
                "response": { "count": 2,
                              "direct_messages": [msg("201", "hi"), msg("202", "there")] }
            })))
            .mount(&server)
            .await;

        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, store.clone(), &dir, fast());

        let written = engine
            .sync_conversation("u9", ConversationKind::Dm)
            .await
            .unwrap();
        assert_eq!(written, 2);
        assert!(state_of(&store, "u9").backfill_complete);
    }

    #[tokio::test]
    async fn a_sync_cycle_is_spawnable() {
        // The engine is driven from a background task, so the cycle future has
        // to be `Send`. A store guard accidentally left alive across an await
        // would break this at compile time — which is the point.
        fn assert_send<T: Send>(_: T) {}
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&server, memory_store(), &dir, fast());
        assert_send(engine.sync_once());
        assert_send(engine.cache_pending_media());
        assert_send(engine.sync_conversation("g1", ConversationKind::Group));
    }

    #[test]
    fn cursors_do_not_trust_page_order() {
        // Same assertion one level down, where the client's own sort cannot mask
        // a comparison bug: the extremes must come from the ids, not the slice.
        let page: Vec<Message> = ["100", "99", "170000000000000007", "170000000000000006"]
            .iter()
            .map(|id| Message {
                id: (*id).to_string(),
                ..Default::default()
            })
            .collect();
        let mut state = SyncState::default();
        advance_cursors(&mut state, &page);
        assert_eq!(state.oldest_id.as_deref(), Some("99"));
        assert_eq!(state.newest_id.as_deref(), Some("170000000000000007"));
    }

    #[test]
    fn an_unparseable_id_never_becomes_a_cursor() {
        // `id_sort_key` yields 0 for garbage, which would look older than all of
        // history and then be handed back to the server as a `before_id`.
        let page: Vec<Message> = ["200", "not-an-id"]
            .iter()
            .map(|id| Message {
                id: (*id).to_string(),
                ..Default::default()
            })
            .collect();
        let mut state = SyncState::default();
        advance_cursors(&mut state, &page);
        assert_eq!(state.oldest_id.as_deref(), Some("200"));
        assert_eq!(state.newest_id.as_deref(), Some("200"));
    }

    #[test]
    fn blob_names_are_stable_and_typed_by_content_type() {
        let url = "https://m.groupme.com/uploads/abc/1792x2400.original.jpeg";
        assert_eq!(
            blob_name(url, Some("image/jpeg")),
            blob_name(url, Some("image/jpeg"))
        );
        assert!(blob_name(url, Some("image/jpeg; charset=binary")).ends_with(".jpg"));
        assert!(blob_name(url, None).ends_with(".bin"));
        assert!(blob_name(url, Some("application/octet-stream")).ends_with(".bin"));
        assert_ne!(
            blob_name(url, Some("image/jpeg")),
            blob_name("https://other", Some("image/jpeg"))
        );
        // 64 hex characters plus the extension.
        assert_eq!(blob_name(url, Some("image/png")).len(), 68);
    }
}
