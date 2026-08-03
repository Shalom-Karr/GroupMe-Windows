//! The IPC surface exposed to the custom client window.
//!
//! Everything here **writes**, which is precisely why it is not in
//! `commands.rs`. That module is the offline reader's surface: it contains no
//! mutating command at all, and a meta-test fails the build if one is added.
//! This module is the other half of that split — the mutations, plus a
//! mirror-image test asserting every command here is named `client_*`. Neither
//! surface can drift into the other without one of those two tests going red.
//!
//! The boundary that actually stops a page from sending is *which page runs in
//! which window*. This project ships no app-level ACL manifest, so app-defined
//! commands registered through `generate_handler!` are not permission-gated the
//! way plugin commands are — any bundled local page that can call `invoke` can
//! reach any registered command. What is gated is the remote origin:
//! `capabilities/remote.json` grants `web.groupme.com` only `core:event:*`, so
//! third-party code we do not control has no command access whatsoever. Within
//! our own pages the separation is by construction — `offline.html` never
//! learns these names — and `capabilities/client.json` documents the intent.

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::State;
use tokio::sync::RwLock;

use crate::api::{ApiError, GroupMeClient};
use crate::commands::SharedStore;
use crate::model::{ConversationKind, Message, Reaction, SystemEvent};
use crate::store::Store;

/// An **async** lock, unlike [`SharedStore`]: the guard is held across the HTTP
/// round trip, which a `std::sync::Mutex` guard cannot survive without making
/// the future non-`Send`. `None` until a token has been captured *and* the
/// account verified — see `lib.rs::adopt_token`.
pub type SharedClient = Arc<RwLock<Option<GroupMeClient>>>;

type CmdResult<T> = Result<T, String>;

/// GroupMe truncates or rejects past roughly this; the exact ceiling is
/// undocumented and has moved before, so this is deliberately a round number
/// under it rather than a value scraped from an error body.
pub const MAX_TEXT_CHARS: usize = 1000;

/// GroupMe's image service caps uploads well below this. The point of the check
/// is to refuse a huge file before it is read into a `Vec<u8>` and pushed
/// through the IPC bridge, not to predict their limit.
///
/// 16 MiB rather than something round and generous, because this number sets a
/// memory spike, not just a policy: the bytes exist simultaneously as a JS
/// array, as the IPC serialisation of it, and as a `Vec<u8>` here. A limit that
/// no real photo reaches costs nothing to enforce and bounds the worst case.
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

const NOT_SIGNED_IN: &str =
    "not signed in yet — open GroupMe and wait for the account to be verified";
const SESSION_EXPIRED: &str = "session expired — sign in again";

fn fail(context: &str, e: impl std::fmt::Display) -> String {
    log::error!("{context}: {e}");
    format!("{context} failed")
}

/// Logged in full, returned in brief — with one exception.
///
/// `Unauthorized` is the only failure the user can actually do something about,
/// and "sending failed" would send them looking for a network problem that is
/// not there. It gets its own message; everything else stays generic.
fn map_api(context: &str, e: ApiError) -> String {
    log::error!("{context}: {e}");
    match e {
        ApiError::Unauthorized => SESSION_EXPIRED.to_string(),
        ApiError::RateLimited { .. } => {
            format!("{context} failed: GroupMe is rate limiting this account — try again shortly")
        }
        ApiError::NotFound => format!("{context} failed: it no longer exists"),
        _ => format!("{context} failed"),
    }
}

// ---------------------------------------------------------------- archive I/O

/// Mirrors a completed mutation into the archive.
///
/// Infallible from the caller's point of view on purpose: the server has
/// already accepted the write, so surfacing a SQLite error as a command failure
/// would invite the frontend to retry a send that actually succeeded. A missing
/// local row is repaired by the next sync; a duplicate message is not.
///
/// The guard lives entirely inside the closure. A `std::sync::MutexGuard` held
/// across an `.await` makes the future non-`Send`, which `spawn` rejects.
async fn mirror_to_archive<F>(store: &SharedStore, context: &'static str, f: F)
where
    F: FnOnce(&mut Store) -> anyhow::Result<()> + Send + 'static,
{
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // A writer that panicked mid-statement poisons the lock. Adopt the
        // guard rather than propagating: one bad write must not brick the
        // archive for the rest of the session.
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    })
    .await;

    match joined {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("{context}: {e:#}"),
        Err(e) => log::error!("{context}: {e}"),
    }
}

async fn read_meta(store: &SharedStore, key: &'static str) -> CmdResult<Option<String>> {
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_meta(key)
    })
    .await;

    match joined {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(fail("reading the signed-in account", format!("{e:#}"))),
        Err(e) => Err(fail("reading the signed-in account", e)),
    }
}

/// A local store write whose failure the caller *does* want to see.
///
/// Unlike [`mirror_to_archive`], which swallows errors because the server has
/// already accepted a mutation, these commands are the whole operation — a pin
/// or mute that failed to persist should surface so the UI does not show a state
/// the archive never recorded. The guard lives entirely inside the closure so it
/// is never held across an `.await`.
async fn write_store<F>(store: &SharedStore, context: &'static str, f: F) -> CmdResult<()>
where
    F: FnOnce(&mut Store) -> anyhow::Result<()> + Send + 'static,
{
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    })
    .await;

    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(fail(context, format!("{e:#}"))),
        Err(e) => Err(fail(context, e)),
    }
}

/// Reads a conversation's stored kind on the blocking pool.
async fn read_conversation_kind(
    store: &SharedStore,
    conversation_id: String,
) -> CmdResult<Option<ConversationKind>> {
    let store = store.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.conversation_kind(&conversation_id)
    })
    .await;

    match joined {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(fail("reading the conversation", format!("{e:#}"))),
        Err(e) => Err(fail("reading the conversation", e)),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- pure helpers

/// Where a send is actually addressed. A group is addressed by the
/// conversation id itself; a DM is addressed by the *other participant*, which
/// the conversation id only implies.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Group(String),
    Dm(String),
}

/// GroupMe echoes `source_guid` back on the created message. That echo is how
/// an optimistic local row is matched to the server's copy instead of rendering
/// twice — the server id is not known until the response arrives, and the
/// realtime frame for the same message may land first.
fn new_source_guid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rejects rather than truncates.
///
/// The command returns the created `Message`, so there is no channel for "sent,
/// but I dropped 200 characters" — silently shipping a clipped body would be
/// unrecoverable once the message is out. Refusing up front, naming the limit
/// and the actual length, is the only outcome the user can act on.
///
/// Counted in `chars`, not bytes: a body of emoji is a few hundred characters
/// and several thousand bytes, and a byte limit would reject it wrongly.
fn validate_text(text: &str, has_attachments: bool) -> Result<(), String> {
    if text.trim().is_empty() && !has_attachments {
        return Err("nothing to send — type a message or attach something".into());
    }
    let len = text.chars().count();
    if len > MAX_TEXT_CHARS {
        return Err(format!(
            "message is too long: {len} characters, and GroupMe accepts about {MAX_TEXT_CHARS}"
        ));
    }
    Ok(())
}

/// A reply is an attachment, not a field on the message.
///
/// `reply_id` and `base_reply_id` both point at the target. GroupMe uses
/// `base_reply_id` as the root of a reply chain, but this client threads one
/// level deep, so the two coincide.
///
/// `user_id` is omitted when the author is unknown rather than sent empty:
/// GroupMe renders the quoted header from it, and `""` produces a blank
/// attribution where a missing key produces a lookup by `reply_id`.
fn reply_attachment(reply_to: &str, reply_to_user_id: Option<&str>) -> Result<Value, String> {
    let target = reply_to.trim();
    if target.is_empty() {
        return Err("cannot reply to an unidentified message".into());
    }
    let mut att = json!({
        "type": "reply",
        "reply_id": target,
        "base_reply_id": target,
    });
    if let Some(uid) = reply_to_user_id.map(str::trim).filter(|s| !s.is_empty()) {
        att["user_id"] = Value::String(uid.to_string());
    }
    Ok(att)
}

/// Derives the recipient of a DM.
///
/// Two forms reach this. **The archive stores a DM under the other participant's
/// user id** — `upsert_chat` keys on `other_user.id` — and that bare id *is*
/// already the recipient. The composite `"<a>+<b>"` thread key, ascending, is
/// what arrives on realtime frames and read receipts; there the recipient is
/// whichever half is not the signed-in account.
///
/// Handling only the composite form is what broke sending in every DM: the
/// stored id has no `+`, so this returned "not a direct-message thread key" and
/// the send failed before a request was ever made.
///
/// Every failure is an error, never a guess. Sending a direct message to the
/// wrong person is not something the user can take back, so an unknown signed-in
/// id stops a composite-form send rather than picking a half.
fn dm_recipient(conversation_id: &str, signed_in_user_id: Option<&str>) -> Result<String, String> {
    let id = conversation_id.trim();
    if id.is_empty() {
        return Err("cannot send a direct message without a recipient".into());
    }

    // Stored form: already the other participant. No account id needed, which
    // also removes a failure mode — a send no longer depends on a sync having
    // recorded who we are.
    if !id.contains('+') {
        return Ok(id.to_string());
    }

    let me = signed_in_user_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "cannot tell which account is signed in, so the recipient of this direct message \
             cannot be determined — sign in and let one sync finish first"
                .to_string()
        })?;

    let (a, b) = id
        .split_once('+')
        .ok_or_else(|| format!("{conversation_id:?} is not a direct-message thread key"))?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return Err(format!("{conversation_id:?} is not a usable thread key"));
    }

    match (a == me, b == me) {
        // A note-to-self thread is "<me>+<me>"; the recipient really is us.
        (true, true) => Ok(me.to_string()),
        (true, false) => Ok(b.to_string()),
        (false, true) => Ok(a.to_string()),
        (false, false) => Err(format!(
            "the signed-in account is not a participant in {conversation_id:?}"
        )),
    }
}

fn route(kind: &str, conversation_id: &str) -> Result<&'static str, String> {
    match kind {
        "group" => Ok("group"),
        "dm" => Ok("dm"),
        other => Err(format!(
            "unknown conversation kind {other:?} for {conversation_id:?} — expected \"group\" or \"dm\""
        )),
    }
}

/// The composite DM thread key `"{lo}+{hi}"`, sorted **numerically** ascending.
///
/// This is the id every DM HTTP path wants — including
/// `POST /v3/messages/{conversation_id}/{message_id}/like` — while the archive
/// keys a DM by the *other participant's* bare user id. Sorting the two ids as
/// strings produces the wrong key whenever they differ in length, so the compare
/// is on the parsed integer, mirroring `realtime::dm_conversation_id`.
fn dm_thread_key(a: &str, b: &str) -> String {
    use crate::model::id_sort_key;
    let (lo, hi) = if id_sort_key(a) <= id_sort_key(b) {
        (a, b)
    } else {
        (b, a)
    };
    format!("{lo}+{hi}")
}

/// The signed-in account's membership id within a group-detail response.
///
/// Leaving a group is addressed by the *membership* id, not the user id
/// (docs §7.1/§7.3), and that id only appears alongside the members. `user_id`
/// is a string in this payload; `id` (the membership) can arrive as a string or,
/// defensively, a number — both normalise to a string.
fn membership_id_of(group: &Value, my_user_id: &str) -> Option<String> {
    group.get("members")?.as_array()?.iter().find_map(|m| {
        let uid = m.get("user_id").and_then(Value::as_str)?;
        if uid != my_user_id {
            return None;
        }
        match m.get("id")? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    })
}

/// Resolves a conversation id to the form the reaction endpoint expects.
///
/// A group id is already correct. A DM, though, is stored under the other
/// participant's bare user id, while `POST /v3/messages/{conversation_id}/…/like`
/// wants the composite `"{lo}+{hi}"` thread key — passing the bare id makes
/// GroupMe answer `404 not found`, the same DM-key mismatch that once broke send
/// and mark-read. The kind is read from the store rather than taken from the
/// caller, so the `client_react`/`client_unreact` contract is unchanged.
async fn resolve_reaction_conversation(
    store: &SharedStore,
    conversation_id: &str,
) -> CmdResult<String> {
    // Already the composite thread key (an arriving realtime/read-receipt id):
    // nothing to resolve.
    if conversation_id.contains('+') {
        return Ok(conversation_id.to_string());
    }
    match read_conversation_kind(store, conversation_id.to_string()).await? {
        Some(ConversationKind::Dm) => {
            let me = read_meta(store, "account_user_id").await?.ok_or_else(|| {
                "cannot react in a direct message until the signed-in account is known — \
                 let one sync finish first"
                    .to_string()
            })?;
            Ok(dm_thread_key(&me, conversation_id))
        }
        // A group (the id is already the right key) or a conversation we do not
        // hold (nothing better to do than pass it through unchanged).
        _ => Ok(conversation_id.to_string()),
    }
}

/// Normalises a content type and refuses anything that is not an image.
///
/// The parameter (`; charset=…`) is stripped before comparison because a
/// browser `File.type` can carry one, and `"image/png; charset=binary"` is
/// still a PNG.
fn validate_image_mime(mime: &str) -> Result<String, String> {
    let base = mime
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !base.starts_with("image/") || base.len() == "image/".len() {
        return Err(format!("{mime:?} is not an image content type"));
    }
    Ok(base)
}

fn validate_upload_size(len: usize) -> Result<(), String> {
    if len == 0 {
        return Err("nothing to upload — the file is empty".into());
    }
    if len > MAX_UPLOAD_BYTES {
        return Err(format!(
            "image is too large: {:.1} MB, and the limit is {} MB",
            len as f64 / (1024.0 * 1024.0),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- commands

#[tauri::command]
pub async fn client_send_message(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    conversation_id: String,
    kind: String,
    text: String,
    reply_to: Option<String>,
    reply_to_user_id: Option<String>,
    attachments: Option<Vec<Value>>,
) -> CmdResult<Message> {
    let mut attachments = attachments.unwrap_or_default();
    validate_text(&text, !attachments.is_empty())?;
    if let Some(target) = reply_to.as_deref() {
        attachments.push(reply_attachment(target, reply_to_user_id.as_deref())?);
    }

    // Resolved before the client lock is taken: the DM case reads the archive
    // on the blocking pool, and holding the client across that would serialise
    // every other send behind a SQLite round trip for no reason.
    let target = match route(&kind, &conversation_id)? {
        "dm" => {
            let me = read_meta(store.inner(), "account_user_id").await?;
            Target::Dm(dm_recipient(&conversation_id, me.as_deref())?)
        }
        _ => Target::Group(conversation_id.clone()),
    };

    let source_guid = new_source_guid();
    let msg = {
        let guard = client.read().await;
        let Some(api) = guard.as_ref() else {
            return Err(NOT_SIGNED_IN.into());
        };
        let sent = match &target {
            Target::Group(group_id) => {
                api.send_group_message(group_id, &text, attachments, &source_guid)
                    .await
            }
            Target::Dm(recipient_id) => {
                api.send_direct_message(recipient_id, &text, attachments, &source_guid)
                    .await
            }
        };
        sent.map_err(|e| map_api("sending the message", e))?
    };

    // Written now rather than waiting for the realtime echo: the frontend
    // reconciles its optimistic row against the returned message immediately,
    // and a reload before the echo arrives must not lose it.
    let archived = msg.clone();
    let key = conversation_id.clone();
    mirror_to_archive(store.inner(), "recording the sent message", move |s| {
        s.insert_messages(&key, &[archived])?;
        Ok(())
    })
    .await;

    Ok(msg)
}

/// Groups only. GroupMe exposes no edit endpoint for direct messages.
///
/// Attachments are not carried: this command has no attachment parameter, so
/// the edit replaces the body and leaves the server's own handling of the
/// existing attachments alone.
#[tauri::command]
pub async fn client_edit_message(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    group_id: String,
    message_id: String,
    text: String,
) -> CmdResult<Message> {
    validate_text(&text, false)?;

    let msg = {
        let guard = client.read().await;
        let Some(api) = guard.as_ref() else {
            return Err(NOT_SIGNED_IN.into());
        };
        api.edit_message(&group_id, &message_id, &text, Vec::new())
            .await
            .map_err(|e| map_api("editing the message", e))?
    };

    let archived = msg.clone();
    let key = group_id.clone();
    mirror_to_archive(store.inner(), "recording the edited message", move |s| {
        s.insert_messages(&key, &[archived])?;
        Ok(())
    })
    .await;

    Ok(msg)
}

#[tauri::command]
pub async fn client_delete_message(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    conversation_id: String,
    message_id: String,
) -> CmdResult<()> {
    {
        let guard = client.read().await;
        let Some(api) = guard.as_ref() else {
            return Err(NOT_SIGNED_IN.into());
        };
        api.delete_message(&conversation_id, &message_id)
            .await
            .map_err(|e| map_api("deleting the message", e))?;
    }

    // The same tombstone path the realtime `message.deleted` frame takes, so
    // the local row ends up in exactly the state the echo would have produced —
    // and the UI updates without waiting for it. The row survives; that a
    // message existed and was removed is itself archival information.
    let event = SystemEvent {
        kind: Some("message.deleted".into()),
        data: json!({ "message_id": message_id, "deleted_at": now_secs() }),
    };
    mirror_to_archive(store.inner(), "tombstoning the deleted message", move |s| {
        s.apply_event(&event)?;
        Ok(())
    })
    .await;

    Ok(())
}

/// `code` is the Unicode character for a reaction, or `None` for a plain like.
///
/// The conversation id is resolved to the reaction endpoint's expected form
/// first (see [`resolve_reaction_conversation`]): a DM is stored under the other
/// participant's bare user id, but `POST /v3/messages/{conversation_id}/…/like`
/// wants the composite `"{lo}+{hi}"` thread key, and the bare id 404s.
#[tauri::command]
pub async fn client_react(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    conversation_id: String,
    message_id: String,
    code: Option<String>,
) -> CmdResult<Vec<Reaction>> {
    let api_conversation_id =
        resolve_reaction_conversation(store.inner(), &conversation_id).await?;
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.like_message(&api_conversation_id, &message_id, code.as_deref())
        .await
        .map_err(|e| map_api("adding the reaction", e))
}

#[tauri::command]
pub async fn client_unreact(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    conversation_id: String,
    message_id: String,
) -> CmdResult<Vec<Reaction>> {
    let api_conversation_id =
        resolve_reaction_conversation(store.inner(), &conversation_id).await?;
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.unlike_message(&api_conversation_id, &message_id)
        .await
        .map_err(|e| map_api("removing the reaction", e))
}

/// Best-effort, and never surfaced. Unlike a send or a reaction, a read receipt
/// is fired automatically when a thread is opened, not asked for — so its
/// failure must not become a user-facing error. It legitimately fails for
/// reasons that are not problems: GroupMe answers `403 "not a member"` for a
/// group the user has left but still has archived, and the whole write is
/// blocked on a filtered network. Log it and return `Ok` either way.
#[tauri::command]
pub async fn client_mark_read(
    client: State<'_, SharedClient>,
    conversation_id: String,
    last_read_message_id: String,
) -> CmdResult<()> {
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        // Not even an error worth returning: nothing to mark read before sign-in.
        return Ok(());
    };
    if let Err(e) = api.mark_read(&conversation_id, &last_read_message_id).await {
        log::debug!("marking {conversation_id} read (ignored): {e}");
    }
    Ok(())
}

/// Returns the `i.groupme.com` URL to put in an `image` attachment.
#[tauri::command]
pub async fn client_upload_image(
    client: State<'_, SharedClient>,
    bytes: Vec<u8>,
    mime: String,
) -> CmdResult<String> {
    validate_upload_size(bytes.len())?;
    let mime = validate_image_mime(&mime)?;

    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.upload_image(bytes, &mime)
        .await
        .map_err(|e| map_api("uploading the image", e))
}

/// Subscribes the realtime socket to a conversation's own channel.
///
/// Messages arrive on the account's `/user/{id}` channel regardless, so this is
/// not what makes the thread live — it is what delivers that thread's typing
/// notices, which are published per-conversation and are invisible without it.
/// Idempotent and safe while the socket is down: the subscription set is
/// replayed on reconnect.
///
/// `kind` is required, not inferred. The archive keys a DM by the *other
/// participant's* user id, so a DM id is shape-identical to a group id;
/// guessing subscribed DMs to `/group/{user_id}`, a channel the account does
/// not own, and GroupMe answered by failing authentication and tearing down the
/// whole session on the first DM opened.
#[tauri::command]
pub async fn client_watch_conversation(
    realtime: State<'_, crate::realtime::RealtimeSlot>,
    conversation_id: String,
    kind: String,
    previous_id: Option<String>,
) -> CmdResult<bool> {
    let kind = parse_kind(&kind)?;
    let guard = realtime.lock().await;
    let Some(rt) = guard.as_ref() else {
        // No socket yet. Not an error: polling still delivers messages, so the
        // thread works — it just has no typing notices until realtime is up.
        return Ok(false);
    };
    // Dropping the old subscription as the user leaves keeps the set bounded;
    // a long session would otherwise accumulate every thread ever opened.
    if let Some(prev) = previous_id.as_deref().filter(|p| *p != conversation_id) {
        rt.unwatch_conversation(prev);
    }
    rt.watch_conversation(&conversation_id, kind);
    Ok(rt.is_connected())
}

/// Publishes a typing notice. Fire-and-forget by design: a dropped notice is
/// invisible, and a failure here must never interrupt composing.
#[tauri::command]
pub async fn client_typing(
    realtime: State<'_, crate::realtime::RealtimeSlot>,
    conversation_id: String,
    kind: String,
) -> CmdResult<()> {
    let kind = parse_kind(&kind)?;
    if let Some(rt) = realtime.lock().await.as_ref() {
        rt.send_typing(&conversation_id, kind);
    }
    Ok(())
}

/// Rejects rather than defaulting. Defaulting to `Group` is what produced the
/// wrong channel in the first place, and a silently wrong subscription costs the
/// entire realtime session.
fn parse_kind(kind: &str) -> Result<ConversationKind, String> {
    ConversationKind::parse(kind)
        .ok_or_else(|| format!("unknown conversation kind {kind:?} — expected \"group\" or \"dm\""))
}

pub const UI_WEB: &str = "web";
pub const UI_CLIENT: &str = "client";
const META_PREFERRED_UI: &str = "preferred_ui";

/// Which surface the window opens on: GroupMe's web client or this one.
/// Defaults to the web client — a fresh install has no token until the user
/// signs in there, so it is the only surface that can bootstrap a session.
#[tauri::command]
pub async fn client_ui_preference(store: State<'_, SharedStore>) -> CmdResult<String> {
    Ok(read_meta(&store, META_PREFERRED_UI)
        .await?
        .filter(|v| v.as_str() == UI_CLIENT)
        .unwrap_or_else(|| UI_WEB.to_string()))
}

#[tauri::command]
pub async fn client_set_ui_preference(store: State<'_, SharedStore>, ui: String) -> CmdResult<()> {
    if ui != UI_WEB && ui != UI_CLIENT {
        return Err(format!(
            "unknown ui {ui:?} — expected {UI_WEB:?} or {UI_CLIENT:?}"
        ));
    }
    mirror_to_archive(&store, "saving the ui preference", move |s| {
        s.set_meta(META_PREFERRED_UI, &ui)
    })
    .await;
    Ok(())
}

// ----------------------------------------------- local pins, ordering, mute

/// Set (`Some`) or clear (`None`) a conversation's local pin rank. Store-backed
/// and works offline — this is deliberately *not* GroupMe's own pinned list.
#[tauri::command]
pub async fn client_set_pin(
    store: State<'_, SharedStore>,
    conversation_id: String,
    rank: Option<i64>,
) -> CmdResult<()> {
    write_store(store.inner(), "pinning the conversation", move |s| {
        s.set_pin(&conversation_id, rank)
    })
    .await
}

/// Assign pin ranks `0..n` to `ordered_ids` in order. Conversations not listed
/// keep their existing pin state.
#[tauri::command]
pub async fn client_reorder_pins(
    store: State<'_, SharedStore>,
    ordered_ids: Vec<String>,
) -> CmdResult<()> {
    write_store(store.inner(), "reordering the pins", move |s| {
        s.reorder_pins(&ordered_ids)
    })
    .await
}

/// Set the local mute flag. A muted conversation raises no tray notification
/// (see `tray::notify_message`); it does not touch GroupMe's own mute.
#[tauri::command]
pub async fn client_set_mute(
    store: State<'_, SharedStore>,
    conversation_id: String,
    muted: bool,
) -> CmdResult<()> {
    write_store(store.inner(), "muting the conversation", move |s| {
        s.set_mute(&conversation_id, muted)
    })
    .await
}

// ----------------------------------------------------- group settings (API)

/// `GET /v3/groups/{id}?include=members` → the raw group object with members, for
/// a settings panel. Routed through the client's fallback-aware GET.
#[tauri::command]
pub async fn client_group_detail(
    client: State<'_, SharedClient>,
    group_id: String,
) -> CmdResult<Value> {
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.group_detail(&group_id)
        .await
        .map_err(|e| map_api("loading the group", e))
}

/// Leave a group. Resolves the signed-in account's *membership* id from the
/// group detail first, because that — not the user id — is what the captured
/// leave endpoint (`.../memberships/{membership_id}/destroy`, docs §7.3) wants.
#[tauri::command]
pub async fn client_leave_group(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    group_id: String,
) -> CmdResult<()> {
    let me = read_meta(store.inner(), "account_user_id")
        .await?
        .ok_or_else(|| {
            "cannot leave a group until the signed-in account is known — \
             let one sync finish first"
                .to_string()
        })?;

    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    let detail = api
        .group_detail(&group_id)
        .await
        .map_err(|e| map_api("leaving the group", e))?;
    let membership_id = membership_id_of(&detail, &me)
        .ok_or_else(|| "you do not appear to be a member of this group".to_string())?;
    api.leave_group(&group_id, &membership_id)
        .await
        .map_err(|e| map_api("leaving the group", e))
}

/// Enqueues a priority sync of one conversation, waking the sync loop immediately
/// rather than waiting for its next scheduled cycle.
///
/// The kind must be exactly `"group"` or `"dm"` — refused rather than defaulted,
/// because a wrong kind produces a wrong API path (DM subscribed to `/group/…`
/// once caused authentication failures that killed realtime for the session).
#[tauri::command]
pub async fn client_sync_now(
    sync_now: State<'_, std::sync::Arc<crate::SyncNow>>,
    conversation_id: String,
    kind: String,
) -> CmdResult<()> {
    let kind = parse_kind(&kind)?;
    sync_now.enqueue(conversation_id, kind);
    Ok(())
}

/// Block (`blocked = true`) or unblock a user via the captured `/v3/blocks`
/// endpoints (docs §7.5). `user` is the signed-in account; `otherUser` is the
/// target.
#[tauri::command]
pub async fn client_set_block(
    store: State<'_, SharedStore>,
    client: State<'_, SharedClient>,
    user_id: String,
    blocked: bool,
) -> CmdResult<()> {
    let me = read_meta(store.inner(), "account_user_id")
        .await?
        .ok_or_else(|| {
            "cannot change block state until the signed-in account is known — \
             let one sync finish first"
                .to_string()
        })?;

    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.set_block(&me, &user_id, blocked).await.map_err(|e| {
        map_api(
            if blocked {
                "blocking the user"
            } else {
                "unblocking the user"
            },
            e,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic ids throughout — this repository is public.
    const ME: &str = "20000001";
    const THEM: &str = "10000001";
    /// GroupMe orders the halves ascending, so the signed-in account is not
    /// reliably on either side. Both orderings are exercised below.
    const DM_KEY: &str = "10000001+20000001";

    /// Only the production half of this file. The test module names the very
    /// patterns it checks for, so scanning the whole file would match itself.
    fn production_source() -> &'static str {
        let full = include_str!("client_commands.rs");
        full.split("#[cfg(test)]").next().unwrap_or(full)
    }

    /// The mirror of `commands.rs`'s read-only assertion.
    ///
    /// That file proves nothing here can be invoked from the offline reader by
    /// proving it holds no mutation; this one proves nothing archival leaks in
    /// the other direction. Together the two names — `archive_*` and `client_*`
    /// — are the boundary, and neither surface can quietly absorb the other.
    #[test]
    fn every_exposed_command_is_named_client_something() {
        let source = production_source();
        let marker = "#[tauri::command]";
        let mut found = 0;
        for (idx, _) in source.match_indices(marker) {
            let sig: String = source[idx + marker.len()..]
                .lines()
                .find(|l| l.contains("fn "))
                .unwrap_or_default()
                .to_string();
            assert!(
                sig.contains("fn client_"),
                "a #[tauri::command] on the client surface is not a client_* mutation: {sig}"
            );
            found += 1;
        }
        assert_eq!(
            found, 18,
            "expected exactly the eighteen client commands \
             (7 message mutations + 2 realtime bridges + 2 ui-preference \
              + 3 local pin/mute + 3 group-settings/leave/block + 1 priority sync)"
        );
    }

    #[test]
    fn no_archive_reader_leaks_onto_the_client_surface() {
        // Assembled at runtime so the literal never appears in the scanned half.
        let needle = format!("fn {}", "archive_");
        assert!(
            !production_source().contains(&needle),
            "archive readers belong on the offline surface, not here"
        );
    }

    // ------------------------------------------------------------- replies

    #[test]
    fn a_reply_becomes_an_attachment_carrying_both_ids_and_the_author() {
        let att = reply_attachment("170000000000000005", Some(THEM)).unwrap();
        assert_eq!(att["type"], "reply");
        assert_eq!(att["reply_id"], "170000000000000005");
        // One level deep, so the chain root and the target coincide.
        assert_eq!(att["base_reply_id"], "170000000000000005");
        assert_eq!(att["user_id"], THEM);
    }

    #[test]
    fn a_reply_omits_user_id_entirely_when_the_author_is_unknown() {
        let att = reply_attachment("170000000000000005", None).unwrap();
        assert!(
            att.get("user_id").is_none(),
            "an empty user_id blanks the quoted header; the key must be absent instead"
        );
        // Whitespace is not an author either.
        let att = reply_attachment("170000000000000005", Some("   ")).unwrap();
        assert!(att.get("user_id").is_none());
    }

    #[test]
    fn a_reply_to_nothing_is_rejected() {
        assert!(reply_attachment("   ", Some(THEM)).is_err());
    }

    // ------------------------------------------------- DM recipient routing

    #[test]
    fn dm_recipient_is_the_other_half_when_we_are_the_first() {
        // "<me>+<them>" — we are on the left.
        let key = format!("{ME}+{THEM}");
        assert_eq!(dm_recipient(&key, Some(ME)).unwrap(), THEM);
    }

    #[test]
    fn dm_recipient_is_the_other_half_when_we_are_the_second() {
        // The ascending-order key puts the smaller id first, so this is the
        // shape actually seen for this pair.
        assert_eq!(DM_KEY, format!("{THEM}+{ME}"));
        assert_eq!(dm_recipient(DM_KEY, Some(ME)).unwrap(), THEM);
    }

    /// The case that must never guess: misdelivery is unrecoverable.
    #[test]
    fn dm_recipient_refuses_to_pick_a_half_without_a_known_account() {
        let err = dm_recipient(DM_KEY, None).unwrap_err();
        assert!(
            err.contains("signed in"),
            "the error must say why the send stopped: {err}"
        );
        assert!(dm_recipient(DM_KEY, Some("")).is_err());
        assert!(dm_recipient(DM_KEY, Some("   ")).is_err());
    }

    #[test]
    fn dm_recipient_rejects_a_thread_we_are_not_part_of() {
        assert!(dm_recipient("10000001+10000002", Some(ME)).is_err());
    }

    /// This test used to assert that a bare id was *rejected*, on the assumption
    /// that a DM is always addressed by its `"{a}+{b}"` thread key. That
    /// assumption was wrong and it broke sending in every DM: `upsert_chat` keys
    /// a DM by `other_user.id`, so the id the UI holds for a DM is a bare number
    /// and this returned "not a direct-message thread key" before any request was
    /// made.
    ///
    /// A bare id is therefore accepted — it *is* the recipient. What prevents a
    /// group id being misread as a person is not the id's shape, which never
    /// distinguished them, but that this is only reached once the caller has said
    /// `kind == "dm"`; `route` rejects anything else.
    #[test]
    fn a_bare_id_is_the_recipient_because_that_is_how_a_dm_is_stored() {
        assert_eq!(dm_recipient("99000001", Some(ME)).unwrap(), "99000001");
        // Works without knowing the signed-in account, which removes a failure
        // mode: a send no longer depends on a sync having recorded who we are.
        assert_eq!(dm_recipient("99000001", None).unwrap(), "99000001");
    }

    #[test]
    fn dm_recipient_still_rejects_what_is_not_addressable() {
        // A malformed thread key must not silently become a recipient.
        assert!(dm_recipient("+20000001", Some(ME)).is_err());
        assert!(dm_recipient("20000001+", Some(ME)).is_err());
        assert!(dm_recipient("", Some(ME)).is_err());
        assert!(dm_recipient("   ", Some(ME)).is_err());
    }

    #[test]
    fn a_note_to_self_thread_addresses_us() {
        let key = format!("{ME}+{ME}");
        assert_eq!(dm_recipient(&key, Some(ME)).unwrap(), ME);
    }

    #[test]
    fn routing_rejects_a_kind_that_is_neither_group_nor_dm() {
        assert_eq!(route("group", "99000001").unwrap(), "group");
        assert_eq!(route("dm", DM_KEY).unwrap(), "dm");
        assert!(route("Group", "99000001").is_err());
        assert!(route("channel", "99000001").is_err());
    }

    /// client_sync_now must reject any kind string that is not exactly "group" or
    /// "dm". The function it delegates to is parse_kind, which wraps
    /// ConversationKind::parse; this test confirms the mapping is wired correctly.
    /// Defaulting to Group on an unknown kind is what produced wrong API paths and
    /// broken realtime sessions in earlier versions (see CLAUDE.md).
    #[test]
    fn client_sync_now_kind_parsing_rejects_invalid_kinds() {
        assert!(parse_kind("group").is_ok());
        assert!(parse_kind("dm").is_ok());
        assert!(parse_kind("").is_err(), "empty kind must be rejected");
        assert!(parse_kind("Group").is_err(), "kind is case-sensitive");
        assert!(
            parse_kind("channel").is_err(),
            "unknown kind must be rejected"
        );
    }

    // ------------------------------------------------------- text guard rail

    #[test]
    fn an_empty_body_with_no_attachment_is_rejected() {
        assert!(validate_text("", false).is_err());
        assert!(validate_text("   \n\t ", false).is_err());
    }

    #[test]
    fn an_empty_body_is_fine_when_something_is_attached() {
        // An image with no caption is a normal message.
        assert!(validate_text("", true).is_ok());
    }

    #[test]
    fn a_body_over_the_limit_is_refused_here_rather_than_by_the_api() {
        assert!(validate_text(&"a".repeat(MAX_TEXT_CHARS), false).is_ok());
        let err = validate_text(&"a".repeat(MAX_TEXT_CHARS + 1), false).unwrap_err();
        assert!(
            err.contains(&(MAX_TEXT_CHARS + 1).to_string()) && err.contains("1000"),
            "the error must name both the actual length and the limit: {err}"
        );
    }

    /// A byte-length cap would reject a body of emoji that is well inside the
    /// character limit.
    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let emoji = "🎉".repeat(MAX_TEXT_CHARS);
        assert!(emoji.len() > MAX_TEXT_CHARS, "precondition: multi-byte");
        assert!(validate_text(&emoji, false).is_ok());
    }

    // ------------------------------------------------------------- uploads

    #[test]
    fn only_image_content_types_are_accepted() {
        assert_eq!(validate_image_mime("image/png").unwrap(), "image/png");
        assert_eq!(validate_image_mime("IMAGE/JPEG").unwrap(), "image/jpeg");
        assert!(validate_image_mime("application/pdf").is_err());
        assert!(validate_image_mime("text/html").is_err());
        // Not an image type, just a prefix that looks like one.
        assert!(validate_image_mime("imagexyz").is_err());
        assert!(validate_image_mime("image/").is_err());
        assert!(validate_image_mime("").is_err());
    }

    #[test]
    fn a_charset_parameter_does_not_disqualify_an_image() {
        assert_eq!(
            validate_image_mime("image/webp; charset=binary").unwrap(),
            "image/webp"
        );
    }

    #[test]
    fn an_oversized_upload_is_refused_before_it_reaches_the_network() {
        assert!(validate_upload_size(1).is_ok());
        assert!(validate_upload_size(MAX_UPLOAD_BYTES).is_ok());
        let err = validate_upload_size(MAX_UPLOAD_BYTES + 1).unwrap_err();
        // Derived from the constant, not written out: a hardcoded "50 MB" here
        // is what broke when the limit was lowered to bound the IPC spike.
        let limit_mb = format!("{} MB", MAX_UPLOAD_BYTES / (1024 * 1024));
        assert!(
            err.contains(&limit_mb),
            "the error must name the limit ({limit_mb}): {err}"
        );
        assert!(validate_upload_size(0).is_err());
    }

    // ---------------------------------------------------------- source_guid

    #[test]
    fn each_send_gets_its_own_source_guid() {
        let a = new_source_guid();
        let b = new_source_guid();
        assert_ne!(a, b, "a reused guid would collapse two messages into one");
        // v4 hyphenated: 8-4-4-4-12.
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }

    // ------------------------------------------------- reaction DM-key routing

    #[test]
    fn a_dm_reaction_key_is_the_composite_thread_key_regardless_of_order() {
        // Numeric ascending: THEM (10000001) sorts below ME (20000001), so the
        // signed-in account lands on either side depending on the argument order.
        assert_eq!(dm_thread_key(ME, THEM), DM_KEY);
        assert_eq!(dm_thread_key(THEM, ME), DM_KEY);
        // Differing lengths must sort numerically, not lexically.
        assert_eq!(dm_thread_key("20000001", "9999999"), "9999999+20000001");
        assert_eq!(dm_thread_key("9999999", "20000001"), "9999999+20000001");
    }

    #[test]
    fn membership_id_of_finds_the_signed_in_accounts_membership() {
        let group = json!({
            "id": THEM,
            "members": [
                {"id": "1000000002", "user_id": "40000000", "nickname": "Other"},
                {"id": "1000000001", "user_id": ME, "nickname": "Me"}
            ]
        });
        assert_eq!(membership_id_of(&group, ME).as_deref(), Some("1000000001"));
        // Not a member -> None, so leaving reports it rather than acting on a
        // stranger's membership.
        assert!(membership_id_of(&group, "50000000").is_none());
        // A numeric membership id normalises to a string.
        let numeric = json!({"members": [{"id": 1000000003i64, "user_id": ME}]});
        assert_eq!(
            membership_id_of(&numeric, ME).as_deref(),
            Some("1000000003")
        );
    }

    #[tokio::test]
    async fn a_dm_reaction_resolves_to_the_composite_key_while_a_group_passes_through() {
        let store: SharedStore =
            std::sync::Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        {
            let s = store.lock().unwrap();
            s.set_meta("account_user_id", ME).unwrap();
            s.upsert_group(
                &crate::model::Group {
                    id: "10000002".into(),
                    updated_at: 1,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
            // A DM is keyed by the other participant's bare user id.
            s.upsert_chat(
                &crate::model::Chat {
                    updated_at: 1,
                    other_user: crate::model::OtherUser {
                        id: THEM.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        }

        // A group id is already the reaction endpoint's key.
        assert_eq!(
            resolve_reaction_conversation(&store, "10000002")
                .await
                .unwrap(),
            "10000002"
        );
        // A DM stored under the other participant resolves to the composite key.
        assert_eq!(
            resolve_reaction_conversation(&store, THEM).await.unwrap(),
            DM_KEY
        );
        // An already-composite id is left untouched.
        assert_eq!(
            resolve_reaction_conversation(&store, DM_KEY).await.unwrap(),
            DM_KEY
        );
    }
}
