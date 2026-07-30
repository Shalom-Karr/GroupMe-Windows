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

/// Derives the recipient of a DM from its thread key.
///
/// A DM's conversation id is `"<a>+<b>"` with the two user ids in ascending
/// order, so the recipient is whichever half is not the signed-in account.
///
/// Every failure here is an error, never a guess. Sending a direct message to
/// the wrong person is not something the user can take back, so an unknown
/// signed-in id has to stop the send rather than pick a half.
fn dm_recipient(conversation_id: &str, signed_in_user_id: Option<&str>) -> Result<String, String> {
    let me = signed_in_user_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "cannot tell which account is signed in, so the recipient of this direct message \
             cannot be determined — sign in and let one sync finish first"
                .to_string()
        })?;

    let (a, b) = conversation_id
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
#[tauri::command]
pub async fn client_react(
    client: State<'_, SharedClient>,
    conversation_id: String,
    message_id: String,
    code: Option<String>,
) -> CmdResult<Vec<Reaction>> {
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.like_message(&conversation_id, &message_id, code.as_deref())
        .await
        .map_err(|e| map_api("adding the reaction", e))
}

#[tauri::command]
pub async fn client_unreact(
    client: State<'_, SharedClient>,
    conversation_id: String,
    message_id: String,
) -> CmdResult<Vec<Reaction>> {
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.unlike_message(&conversation_id, &message_id)
        .await
        .map_err(|e| map_api("removing the reaction", e))
}

#[tauri::command]
pub async fn client_mark_read(
    client: State<'_, SharedClient>,
    conversation_id: String,
    last_read_message_id: String,
) -> CmdResult<()> {
    let guard = client.read().await;
    let Some(api) = guard.as_ref() else {
        return Err(NOT_SIGNED_IN.into());
    };
    api.mark_read(&conversation_id, &last_read_message_id)
        .await
        .map_err(|e| map_api("marking the conversation read", e))
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
            found, 11,
            "expected exactly the eleven client commands \
             (7 mutations + 2 realtime bridges + 2 ui-preference)"
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

    #[test]
    fn dm_recipient_rejects_a_group_id_mistaken_for_a_thread_key() {
        assert!(dm_recipient("99000001", Some(ME)).is_err());
        assert!(dm_recipient("+20000001", Some(ME)).is_err());
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
}
