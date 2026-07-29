//! Wire types for the GroupMe v3 API.
//!
//! Every field that is not a primary key is optional or defaulted. GroupMe
//! omits keys rather than nulling them in several places (`text` is absent on
//! image-only messages, `avatar_url` on users who never set one, `attachments`
//! on plain text), and a sync that panics on an absent key is a sync that dies
//! three years into someone's history and never recovers.
//!
//! It also does the opposite — sends the key with an explicit `null` where a
//! collection or scalar is expected — so absence and null are two separate
//! hazards and both have to be handled. See [`null_as_default`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Collapse an explicit JSON `null` to `T::default()`.
///
/// `#[serde(default)]` alone is not enough: it fires only when the key is
/// *absent*. A key present with value `null` is handed straight to the field's
/// own `Deserialize`, and `Vec`/`bool`/`i64`/`String` all reject it — failing
/// the whole enclosing response, not just the field. GroupMe does this
/// routinely (200 of 211 groups in the 2026-07-29 capture carry
/// `"members": null`), so every non-`Option` field pairs this with
/// `#[serde(default)]`.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// GroupMe wraps every v3 response as `{"response": ..., "meta": {...}}`.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    pub response: Option<T>,
    #[serde(default)]
    pub meta: Option<ResponseMeta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseMeta {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub errors: Vec<String>,
}

/// `GET /groups/{id}/messages` -> `{"count": n, "messages": [...]}`
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GroupMessagesPage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub count: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub messages: Vec<Message>,
}

/// `GET /direct_messages` -> `{"count": n, "direct_messages": [...]}`
///
/// Same shape as a group page under a different key. The key difference is the
/// entire reason this is a separate struct.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DirectMessagesPage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub count: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub direct_messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Message {
    pub id: String,
    #[serde(default)]
    pub group_id: Option<String>,
    /// Present on DMs *instead of* `group_id`, as `"<user_a>+<user_b>"` with the
    /// two user IDs in ascending order. It is the canonical DM thread key —
    /// `/v4/read_receipts` is addressed by it — and DM messages carry nothing
    /// else that identifies their thread.
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub source_guid: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub sender_type: Option<String>,
    #[serde(default)]
    pub recipient_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub system: bool,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<serde_json::Value>,
    /// Empty string `""` when unpinned rather than null. Not a typo.
    #[serde(default)]
    pub pinned_by: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub favorited_by: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub attachments: Vec<Attachment>,

    // --- Undocumented; present in live traffic, absent from dev.groupme.com ---
    /// Present only on edited messages.
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Present only on deleted messages. `text` is replaced by a tombstone
    /// string, so this is the only reliable signal that a deletion happened.
    #[serde(default)]
    pub deleted_at: Option<i64>,
    #[serde(default)]
    pub deletion_actor: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub reactions: Vec<Reaction>,
    /// Structured detail on system messages (joins, leaves, edits, deletes).
    #[serde(default)]
    pub event: Option<SystemEvent>,
}

impl Message {
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn is_edited(&self) -> bool {
        self.updated_at.is_some()
    }

    /// Total distinct reactors. `favorited_by` is the flattened union of
    /// `reactions[].user_ids`, so prefer it and fall back to summing.
    pub fn reaction_count(&self) -> usize {
        if !self.favorited_by.is_empty() {
            return self.favorited_by.len();
        }
        self.reactions.iter().map(|r| r.user_ids.len()).sum()
    }
}

/// Undocumented, and distinct from `favorited_by`.
///
/// Two shapes share the field: `unicode` carries a real character in `code`,
/// while `emoji` references GroupMe's proprietary sticker sheet by
/// `pack_id`/`pack_index` and has no Unicode equivalent at all.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Reaction {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Present on `type: "unicode"` only.
    #[serde(default)]
    pub code: Option<String>,
    /// Raw because the read and write paths disagree about the JSON type: a
    /// message list sends `"pack_id":"18"` while `POST .../like` echoes back
    /// `"pack_id":0`. Typing this as `String` stalls a conversation's backfill
    /// permanently the first time a like response is parsed. Read it through
    /// [`Reaction::pack_id`].
    #[serde(default)]
    pub pack_id: Option<serde_json::Value>,
    /// Same string-or-number split as `pack_id`.
    #[serde(default)]
    pub pack_index: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub user_ids: Vec<String>,
}

impl Reaction {
    /// A renderable character, or `None` for pack reactions whose artwork we do
    /// not ship. The reader shows a neutral marker plus a count for those
    /// rather than pulling down GroupMe's emoji sheet.
    pub fn display_char(&self) -> Option<&str> {
        match self.kind.as_deref() {
            Some("unicode") => self.code.as_deref(),
            _ => None,
        }
    }

    /// Sticker sheet ID, normalized to a string across both wire shapes.
    pub fn pack_id(&self) -> Option<String> {
        self.pack_id.as_ref().and_then(json_id)
    }

    /// Index into the sheet named by [`Reaction::pack_id`].
    pub fn pack_index(&self) -> Option<String> {
        self.pack_index.as_ref().and_then(json_id)
    }
}

/// Structured payload on `system: true` messages.
///
/// `data` stays a raw JSON value on purpose: GroupMe encodes user IDs here as
/// JSON **numbers** (`"id": 20000001`) while every other ID in the API is a
/// string. Typing it strictly would fail to deserialize; accessors normalize.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SystemEvent {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl SystemEvent {
    /// The message this event acts upon, for `message.update` / `message.deleted`.
    pub fn target_message_id(&self) -> Option<String> {
        self.data.get("message_id").and_then(json_id)
    }

    /// New body carried by a `message.update` event.
    pub fn updated_text(&self) -> Option<&str> {
        self.data.get("message")?.get("text")?.as_str()
    }

    pub fn deleted_at(&self) -> Option<i64> {
        self.data.get("deleted_at")?.as_i64()
    }

    /// Normalizes the number-or-string ID inconsistency described above.
    pub fn subject_user_id(&self) -> Option<String> {
        for key in ["removed_user", "user", "added_user"] {
            if let Some(v) = self
                .data
                .get(key)
                .and_then(|u| u.get("id"))
                .and_then(json_id)
            {
                return Some(v);
            }
        }
        self.data.get("sender_id").and_then(json_id)
    }
}

/// GroupMe is inconsistent about whether an ID is a JSON string or a JSON
/// number. Accept either and always yield a string.
fn json_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The `user_id` GroupMe uses for an `@everyone` mention. It is not a real
/// account, and joining it against the users table finds nothing.
pub const MENTION_EVERYONE: &str = "-1";

/// Attachments are an open union — GroupMe has shipped new `type` values without
/// notice, and `copilot` is live today with none of its fields modelled here.
///
/// Anything whose `type` is unrecognised (or whose known shape fails to parse)
/// is kept verbatim as [`Attachment::Other`], which holds the original JSON
/// object and re-emits it unchanged on serialize. That matters because
/// `store.rs` derives the archive's `raw_json` column by re-serializing the
/// *parsed* struct: a variant that forgot its payload would silently rewrite
/// history, and the reader would hand that rewritten copy back to the UI.
///
/// The known variants are still lossy in the same way — a field GroupMe adds to
/// `image` will not survive the round-trip until it is added below.
#[derive(Debug, Clone, Deserialize, Serialize)]
// `remote = "Self"` makes the derive emit inherent `Attachment::{serialize,
// deserialize}` instead of the trait impls, leaving the hand-written impls
// below free to wrap them with the unknown-type fallback. `#[serde(other)]`
// cannot do this: it only accepts a *unit* variant, which is exactly the field
// loss being fixed.
#[serde(tag = "type", remote = "Self")]
pub enum Attachment {
    #[serde(rename = "image")]
    Image {
        #[serde(default)]
        url: Option<String>,
        /// Usually identical to `url`; present on newer uploads.
        #[serde(default)]
        source_url: Option<String>,
        /// Undocumented BlurHash. ~30 bytes that decode to a recognizable
        /// blurred preview, so an uncached image still shows something offline
        /// instead of a broken-image icon. Absent on older messages.
        #[serde(default)]
        blur_hash: Option<String>,
    },
    #[serde(rename = "video")]
    Video {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        preview_url: Option<String>,
    },
    #[serde(rename = "file")]
    File {
        #[serde(default)]
        file_id: Option<String>,
    },
    #[serde(rename = "location")]
    Location {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        lat: Option<serde_json::Value>,
        #[serde(default)]
        lng: Option<serde_json::Value>,
    },
    #[serde(rename = "mentions")]
    Mentions {
        #[serde(default, deserialize_with = "null_as_default")]
        user_ids: Vec<String>,
        /// `[[start_char, length], ...]`, parallel to `user_ids`.
        #[serde(default, deserialize_with = "null_as_default")]
        loci: Vec<Vec<i64>>,
    },
    #[serde(rename = "reply")]
    Reply {
        #[serde(default)]
        user_id: Option<String>,
        #[serde(default)]
        reply_id: Option<String>,
        #[serde(default)]
        base_reply_id: Option<String>,
    },
    #[serde(rename = "emoji")]
    Emoji {
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default, deserialize_with = "null_as_default")]
        charmap: Vec<Vec<i64>>,
    },
    /// The untouched wire object for any `type` not listed above.
    #[serde(skip)]
    Other(serde_json::Value),
}

impl<'de> Deserialize<'de> for Attachment {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(d)?;
        // Inherent (remote-derived) `deserialize`, not this trait method —
        // inherent associated functions win name resolution. Bound to a `let`
        // so the borrow of `raw` ends before the fallback arm moves it.
        let parsed = Attachment::deserialize(&raw);
        match parsed {
            Ok(known) => Ok(known),
            Err(_) => Ok(Attachment::Other(raw)),
        }
    }
}

impl Serialize for Attachment {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Attachment::Other(raw) => raw.serialize(s),
            known => Attachment::serialize(known, s),
        }
    }
}

impl Attachment {
    /// The remote asset this attachment points at, if it has one worth caching
    /// for offline viewing.
    pub fn media_url(&self) -> Option<&str> {
        match self {
            Attachment::Image {
                url, source_url, ..
            } => url.as_deref().or(source_url.as_deref()),
            Attachment::Video { preview_url, url } => preview_url.as_deref().or(url.as_deref()),
            _ => None,
        }
    }

    /// BlurHash placeholder, when GroupMe supplied one.
    pub fn blur_hash(&self) -> Option<&str> {
        match self {
            Attachment::Image { blur_hash, .. } => blur_hash.as_deref(),
            _ => None,
        }
    }

    /// Wire `type`, so the archive's attachment index records what GroupMe
    /// actually sent (`copilot`, `poll`, …) rather than lumping every
    /// unrecognised attachment under one meaningless label.
    pub fn kind(&self) -> &str {
        match self {
            Attachment::Image { .. } => "image",
            Attachment::Video { .. } => "video",
            Attachment::File { .. } => "file",
            Attachment::Location { .. } => "location",
            Attachment::Mentions { .. } => "mentions",
            Attachment::Reply { .. } => "reply",
            Attachment::Emoji { .. } => "emoji",
            Attachment::Other(raw) => raw.get("type").and_then(|t| t.as_str()).unwrap_or("other"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Group {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub creator_user_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub updated_at: i64,
    #[serde(default)]
    pub messages_count: Option<i64>,
    /// `GET /v3/groups` nulls this out on most groups and only fills it in on a
    /// single-group fetch — 200 of 211 in the 2026-07-29 capture were `null`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub members: Vec<Member>,
    #[serde(default)]
    pub messages: Option<GroupPreview>,
}

/// The `messages` sub-object on a group carries the last-message preview.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GroupPreview {
    #[serde(default)]
    pub count: Option<i64>,
    #[serde(default)]
    pub last_message_id: Option<String>,
    #[serde(default)]
    pub last_message_created_at: Option<i64>,
    #[serde(default)]
    pub preview: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Member {
    /// Membership ID — distinct from `user_id`, and the one the remove endpoint
    /// wants. Kept only so the archive matches the wire; this app never removes.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub muted: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Chat {
    #[serde(default, deserialize_with = "null_as_default")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub updated_at: i64,
    #[serde(default)]
    pub messages_count: Option<i64>,
    pub other_user: OtherUser,
    #[serde(default)]
    pub last_message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct OtherUser {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Me {
    pub id: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// Which side of the API a conversation came from. Groups and DMs use different
/// endpoints, different pagination keys, and different response keys, so the
/// distinction has to survive into storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationKind {
    Group,
    Dm,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationKind::Group => "group",
            ConversationKind::Dm => "dm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "group" => Some(ConversationKind::Group),
            "dm" => Some(ConversationKind::Dm),
            _ => None,
        }
    }
}

/// A group or DM flattened into the one shape the offline reader renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    pub name: Option<String>,
    pub image_url: Option<String>,
    pub updated_at: i64,
    pub messages_count: Option<i64>,
    pub last_message_text: Option<String>,
    pub last_message_created_at: Option<i64>,
}

/// GroupMe IDs are decimal strings too large for f64 but comfortably inside
/// i64 (~1.8e17 against a 9.2e18 ceiling). Parsing to i64 gives correct ordering
/// and cursor comparison in SQL without string-length games.
pub fn id_sort_key(id: &str) -> i64 {
    id.parse::<i64>().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_sort_key_orders_real_groupme_ids() {
        let older = id_sort_key("170000000000000006");
        let newer = id_sort_key("170000000000000007");
        assert!(newer > older, "newer message must sort above older");
    }

    #[test]
    fn id_sort_key_survives_garbage() {
        assert_eq!(id_sort_key(""), 0);
        assert_eq!(id_sort_key("not-a-number"), 0);
    }

    #[test]
    fn message_parses_with_only_an_id() {
        // The minimum GroupMe has ever handed us. Must not fail.
        let m: Message = serde_json::from_str(r#"{"id":"123"}"#).unwrap();
        assert_eq!(m.id, "123");
        assert!(m.text.is_none());
        assert!(m.attachments.is_empty());
        assert!(!m.system);
    }

    #[test]
    fn message_parses_null_text_and_null_avatar() {
        // Image-only messages really do arrive with text: null.
        let m: Message =
            serde_json::from_str(r#"{"id":"1","text":null,"avatar_url":null,"name":"A"}"#).unwrap();
        assert!(m.text.is_none());
        assert_eq!(m.name.as_deref(), Some("A"));
    }

    #[test]
    fn unknown_attachment_type_does_not_fail_the_message() {
        // If GroupMe ships a new attachment type, the containing message must
        // still archive. Losing a message because of an unrecognised sticker is
        // the worst possible failure for an archive.
        let m: Message = serde_json::from_str(
            r#"{"id":"1","attachments":[{"type":"quantum_sticker","foo":42}]}"#,
        )
        .unwrap();
        assert_eq!(m.attachments.len(), 1);
        // The wire type is reported verbatim; "other" is only the fallback for
        // a payload that carries no usable `type` at all.
        assert_eq!(m.attachments[0].kind(), "quantum_sticker");
    }

    #[test]
    fn reply_and_mentions_attachments_parse() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1","attachments":[
                {"type":"reply","user_id":"9","reply_id":"8","base_reply_id":"8"},
                {"type":"mentions","user_ids":["1","2"],"loci":[[1,7],[60,7]]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(m.attachments[0].kind(), "reply");
        match &m.attachments[1] {
            Attachment::Mentions { user_ids, loci } => {
                assert_eq!(user_ids.len(), 2);
                assert_eq!(loci[1], vec![60, 7]);
            }
            other => panic!("expected mentions, got {other:?}"),
        }
    }

    #[test]
    fn image_attachment_exposes_media_url() {
        let a = Attachment::Image {
            url: Some("https://i.groupme.com/x".into()),
            source_url: None,
            blur_hash: None,
        };
        assert_eq!(a.media_url(), Some("https://i.groupme.com/x"));
        assert_eq!(
            Attachment::Other(serde_json::json!({"type":"copilot"})).media_url(),
            None
        );
    }

    // --- Regressions pinned to the 2026-07-29 live capture -----------------

    #[test]
    fn image_attachment_carries_blur_hash_and_source_url() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1","attachments":[{
                "type":"image",
                "url":"https://m.groupme.com/uploads/abc/540x1130.original.png",
                "source_url":"https://m.groupme.com/uploads/abc/540x1130.original.png",
                "blur_hash":"]47^xx~D4URPjF?a9Z01b]?I"
            }]}"#,
        )
        .unwrap();
        assert_eq!(
            m.attachments[0].blur_hash(),
            Some("]47^xx~D4URPjF?a9Z01b]?I")
        );
        assert!(m.attachments[0].media_url().unwrap().ends_with(".png"));
    }

    #[test]
    fn unicode_reaction_parses_and_renders() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1","favorited_by":["1","2"],
                "reactions":[{"type":"unicode","code":"🤣","user_ids":["1","2"]}]}"#,
        )
        .unwrap();
        assert_eq!(m.reactions[0].display_char(), Some("🤣"));
        assert_eq!(m.reaction_count(), 2);
    }

    #[test]
    fn pack_reaction_has_no_renderable_char() {
        // We do not ship GroupMe's emoji sheet, so these must degrade to a
        // count rather than rendering a wrong glyph.
        let m: Message = serde_json::from_str(
            r#"{"id":"1","reactions":[
                {"type":"emoji","pack_id":"1","pack_index":"76","user_ids":["1","2","3"]}]}"#,
        )
        .unwrap();
        assert_eq!(m.reactions[0].display_char(), None);
        assert_eq!(m.reaction_count(), 3);
    }

    #[test]
    fn dm_system_message_omits_the_system_flag_entirely() {
        // Group messages always carry `system`; DM messages never do. Anything
        // that gates edit/delete handling on `system == true` therefore drops
        // every DM mutation on the floor. The `event` object is the real signal.
        let m: Message = serde_json::from_str(
            r#"{"id":"1","text":"This message was deleted",
                "event":{"type":"message.deleted",
                         "data":{"message_id":"170000000000000001","deleted_at":1}}}"#,
        )
        .unwrap();
        assert!(!m.system, "absent `system` deserialises to false");
        assert!(m.event.is_some(), "the event is what identifies a mutation");
        assert_eq!(
            m.event.as_ref().unwrap().target_message_id().as_deref(),
            Some("170000000000000001")
        );
    }

    #[test]
    fn message_event_ids_are_strings_while_membership_event_ids_are_numbers() {
        // GroupMe is inconsistent between the two event families. Both must
        // normalise to the same string form.
        let msg_ev: SystemEvent = serde_json::from_str(
            r#"{"type":"message.deleted",
                "data":{"message_id":"170000000000000001","deleted_at":1}}"#,
        )
        .unwrap();
        assert_eq!(
            msg_ev.target_message_id().as_deref(),
            Some("170000000000000001")
        );

        let member_ev: SystemEvent = serde_json::from_str(
            r#"{"type":"membership.notifications.exited",
                "data":{"removed_user":{"id":20000001,"nickname":"Example"}}}"#,
        )
        .unwrap();
        assert_eq!(member_ev.subject_user_id().as_deref(), Some("20000001"));
    }

    #[test]
    fn system_event_ids_arrive_as_numbers_not_strings() {
        // The single nastiest inconsistency in the API: every ID is a string
        // except inside event.data, where they are JSON numbers.
        let m: Message = serde_json::from_str(
            r#"{"id":"1","system":true,"user_id":"system","sender_id":"system",
                "text":"Example Person has left the group.",
                "event":{"type":"membership.notifications.exited",
                         "data":{"removed_user":{"id":20000001,"nickname":"Example Person"}}}}"#,
        )
        .unwrap();
        let ev = m.event.as_ref().unwrap();
        assert_eq!(ev.kind.as_deref(), Some("membership.notifications.exited"));
        assert_eq!(ev.subject_user_id().as_deref(), Some("20000001"));
    }

    #[test]
    fn join_event_reads_the_user_key() {
        let ev: SystemEvent = serde_json::from_str(
            r#"{"type":"membership.announce.joined",
                "data":{"user":{"id":20000002,"nickname":"Example Member"}}}"#,
        )
        .unwrap();
        assert_eq!(ev.subject_user_id().as_deref(), Some("20000002"));
    }

    #[test]
    fn edit_event_exposes_target_and_new_text() {
        let ev: SystemEvent = serde_json::from_str(
            r#"{"type":"message.update","data":{
                "message_id":"170000000000000004","sender_id":"20000004",
                "updated_at":1784702902,
                "message":{"text":"corrected body","attachments":[]}}}"#,
        )
        .unwrap();
        assert_eq!(
            ev.target_message_id().as_deref(),
            Some("170000000000000004")
        );
        assert_eq!(ev.updated_text(), Some("corrected body"));
    }

    #[test]
    fn delete_event_exposes_target_and_timestamp() {
        let ev: SystemEvent = serde_json::from_str(
            r#"{"type":"message.deleted","data":{
                "message_id":"170000000000000005","deleted_at":1784663704,
                "deletion_actor":"sender"}}"#,
        )
        .unwrap();
        assert_eq!(
            ev.target_message_id().as_deref(),
            Some("170000000000000005")
        );
        assert_eq!(ev.deleted_at(), Some(1784663704));
    }

    #[test]
    fn deleted_and_edited_messages_are_flagged() {
        let deleted: Message = serde_json::from_str(
            r#"{"id":"1","text":"This message was deleted",
                "deleted_at":1784663704,"deletion_actor":"sender"}"#,
        )
        .unwrap();
        assert!(deleted.is_deleted());
        assert!(!deleted.is_edited());

        let edited: Message =
            serde_json::from_str(r#"{"id":"2","text":"new","updated_at":1784499085}"#).unwrap();
        assert!(edited.is_edited());
        assert!(!edited.is_deleted());
    }

    #[test]
    fn everyone_mention_is_recognisable() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1","attachments":[{"type":"mentions",
                "user_ids":["20000003","-1"],"loci":[[120,9],[167,24]]}]}"#,
        )
        .unwrap();
        match &m.attachments[0] {
            Attachment::Mentions { user_ids, .. } => {
                assert!(user_ids.iter().any(|u| u == MENTION_EVERYONE));
            }
            other => panic!("expected mentions, got {other:?}"),
        }
    }

    #[test]
    fn pinned_by_is_an_empty_string_not_null() {
        let m: Message =
            serde_json::from_str(r#"{"id":"1","pinned_at":null,"pinned_by":""}"#).unwrap();
        assert_eq!(m.pinned_by.as_deref(), Some(""));
    }

    #[test]
    fn full_captured_message_round_trips() {
        // Verbatim shape from the live capture, values altered. If GroupMe
        // changes anything structural, this is the test that notices.
        let raw = r#"{
            "attachments":[{"source_url":"https://m.groupme.com/uploads/x/540x1130.original.png",
                            "type":"image",
                            "url":"https://m.groupme.com/uploads/x/540x1130.original.png",
                            "blur_hash":"]47^xx~D4URPjF"}],
            "avatar_url":"https://i.groupme.com/2048x1536.jpeg.aaa",
            "created_at":1785300979,"favorited_by":[],"group_id":"10000001",
            "id":"170000000000000009","name":"Example Sender","sender_id":"20000006",
            "sender_type":"user","source_guid":"android-305ff00d","system":false,
            "text":"placeholder body","user_id":"20000006","platform":"gm",
            "pinned_at":null,"pinned_by":""
        }"#;
        let m: Message = serde_json::from_str(raw).unwrap();
        assert_eq!(m.id, "170000000000000009");
        assert_eq!(m.sender_type.as_deref(), Some("user"));
        assert_eq!(m.attachments.len(), 1);
        assert!(m.attachments[0].blur_hash().is_some());
        assert!(!m.is_deleted() && !m.is_edited());
    }

    #[test]
    fn dm_and_group_pages_use_different_keys() {
        let g: GroupMessagesPage =
            serde_json::from_str(r#"{"count":1,"messages":[{"id":"1"}]}"#).unwrap();
        assert_eq!(g.messages.len(), 1);

        let d: DirectMessagesPage =
            serde_json::from_str(r#"{"count":1,"direct_messages":[{"id":"1"}]}"#).unwrap();
        assert_eq!(d.direct_messages.len(), 1);
    }

    #[test]
    fn empty_page_parses_as_empty_not_error() {
        // Terminates the backfill loop; must be a clean empty, not a parse error.
        let g: GroupMessagesPage = serde_json::from_str(r#"{"count":0}"#).unwrap();
        assert!(g.messages.is_empty());
    }

    #[test]
    fn group_parses_without_members_or_preview() {
        let g: Group = serde_json::from_str(r#"{"id":"10000002","name":"Test"}"#).unwrap();
        assert!(g.members.is_empty());
        assert!(g.messages.is_none());
    }

    #[test]
    fn conversation_kind_round_trips() {
        for k in [ConversationKind::Group, ConversationKind::Dm] {
            assert_eq!(ConversationKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ConversationKind::parse("nonsense"), None);
    }

    // --- BUG 1: a key present with `null` is not the same as an absent key ---

    #[test]
    fn group_with_explicit_null_members_parses() {
        // Verbatim shape from `GET /v3/groups`, ids synthetic. 200 of the 211
        // captured groups look like this. `#[serde(default)]` alone does not
        // cover it, and one such group used to fail the entire page — which
        // meant zero groups were ever archived.
        let g: Group = serde_json::from_str(
            r#"{"id":"10000001","name":"Example Group","creator_user_id":"20000001",
                "created_at":1780887158,"members":null,"members_count":0,
                "unread_count":null,"last_read_message_id":null,"last_read_at":null}"#,
        )
        .unwrap();
        assert_eq!(g.id, "10000001");
        assert!(g.members.is_empty());
        assert_eq!(g.created_at, 1780887158);
    }

    #[test]
    fn one_null_members_group_does_not_fail_the_whole_page() {
        // The failure mode that mattered: `client.groups()` decodes an array,
        // so a single bad element took every sibling down with it.
        let gs: Vec<Group> = serde_json::from_str(
            r#"[{"id":"10000001","name":"A","members":null},
                {"id":"10000002","name":"B","members":[{"user_id":"20000001","nickname":"Example"}]}]"#,
        )
        .unwrap();
        assert_eq!(gs.len(), 2);
        assert!(gs[0].members.is_empty());
        assert_eq!(gs[1].members.len(), 1);
    }

    #[test]
    fn every_defaulted_scalar_and_collection_tolerates_null() {
        // Same latent bug as `members`, audited across the file. Any of these
        // arriving null used to fail the enclosing response outright.
        let m: Message = serde_json::from_str(
            r#"{"id":"1","created_at":null,"system":null,"favorited_by":null,
                "attachments":null,"reactions":null,"text":null}"#,
        )
        .unwrap();
        assert_eq!(m.created_at, 0);
        assert!(!m.system);
        assert!(m.favorited_by.is_empty() && m.attachments.is_empty() && m.reactions.is_empty());

        let page: GroupMessagesPage =
            serde_json::from_str(r#"{"count":null,"messages":null}"#).unwrap();
        assert_eq!(page.count, 0);
        assert!(page.messages.is_empty());

        let dms: DirectMessagesPage =
            serde_json::from_str(r#"{"count":null,"direct_messages":null}"#).unwrap();
        assert!(dms.direct_messages.is_empty());

        let meta: ResponseMeta = serde_json::from_str(r#"{"code":200,"errors":null}"#).unwrap();
        assert!(meta.errors.is_empty());

        let mem: Member =
            serde_json::from_str(r#"{"user_id":"20000001","muted":null,"roles":null}"#).unwrap();
        assert!(!mem.muted && mem.roles.is_empty());

        let chat: Chat = serde_json::from_str(
            r#"{"created_at":null,"updated_at":null,
                "other_user":{"id":"20000002","name":"Example Person"}}"#,
        )
        .unwrap();
        assert_eq!(chat.created_at, 0);

        let r: Reaction =
            serde_json::from_str(r#"{"type":"unicode","code":"👍","user_ids":null}"#).unwrap();
        assert!(r.user_ids.is_empty());

        let m: Message = serde_json::from_str(
            r#"{"id":"1","attachments":[{"type":"mentions","user_ids":null,"loci":null},
                                        {"type":"emoji","placeholder":"�","charmap":null}]}"#,
        )
        .unwrap();
        assert_eq!(m.attachments[0].kind(), "mentions");
        assert_eq!(m.attachments[1].kind(), "emoji");
    }

    // --- BUG 2: pack ids are strings on read and numbers on write ----------

    #[test]
    fn reaction_pack_ids_parse_as_both_string_and_number() {
        // Read path (`GET .../messages`) sends strings.
        let read: Reaction = serde_json::from_str(
            r#"{"type":"emoji","pack_id":"18","pack_index":"23","user_ids":["20000001"]}"#,
        )
        .unwrap();
        assert_eq!(read.pack_id().as_deref(), Some("18"));
        assert_eq!(read.pack_index().as_deref(), Some("23"));

        // Write path (`POST /v3/messages/{cid}/{mid}/like`) sends integers for
        // the very same fields. Failing here stalled a conversation's backfill
        // forever, because the cursor never advances past a failed page.
        let write: super::Envelope<LikeResponse> = serde_json::from_str(
            r#"{"meta":{"code":200},"response":{"reactions":[
                {"type":"unicode","pack_id":0,"pack_index":0,"code":"👍",
                 "user_ids":["20000001"]}]}}"#,
        )
        .unwrap();
        let reactions = write.response.unwrap().reactions;
        assert_eq!(reactions[0].pack_id().as_deref(), Some("0"));
        assert_eq!(reactions[0].pack_index().as_deref(), Some("0"));
        assert_eq!(reactions[0].display_char(), Some("👍"), "still renders");
    }

    /// Minimal stand-in for the like/unlike response body.
    #[derive(Deserialize)]
    struct LikeResponse {
        #[serde(default, deserialize_with = "null_as_default")]
        reactions: Vec<Reaction>,
    }

    #[test]
    fn numeric_pack_ids_do_not_fail_the_containing_message() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1","text":"hi","reactions":[
                {"type":"unicode","pack_id":0,"pack_index":0,"code":"🤣",
                 "user_ids":["20000001","20000002"]}]}"#,
        )
        .unwrap();
        assert_eq!(m.reaction_count(), 2);
        assert_eq!(m.reactions[0].display_char(), Some("🤣"));
    }

    // --- BUG 3: unknown attachments must survive the archive round-trip ----

    #[test]
    fn unknown_attachment_round_trips_with_every_field_intact() {
        // `copilot` ships today and is not in the enum. `store.rs` writes
        // `raw_json` by re-serializing the parsed struct, so anything the
        // parse drops is destroyed permanently and invisibly.
        let raw = r#"{"type":"copilot","message_id":"MhZK5Pc9iT4Z6mTurCjdr",
            "part_id":"y18WnBkDWUq8B7cPFFsDk","prompt_sender":"20000001",
            "citations":[{"index":1,"title":"Example title",
                          "url":"https://example.com/","publisher":""}]}"#;

        let a: Attachment = serde_json::from_str(raw).unwrap();
        assert_eq!(a.kind(), "copilot");

        let back: serde_json::Value = serde_json::from_str(&serde_json::to_string(&a).unwrap())
            .expect("re-serialized attachment must be valid JSON");
        let want: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(back, want, "unknown attachment must round-trip byte-equal");

        // And through a whole message, which is what store.rs actually persists.
        let m: Message =
            serde_json::from_str(&format!(r#"{{"id":"1","attachments":[{raw}]}}"#)).unwrap();
        let stored = serde_json::to_string(&m).unwrap();
        let reloaded: Message = serde_json::from_str(&stored).unwrap();
        match &reloaded.attachments[0] {
            Attachment::Other(v) => {
                assert_eq!(
                    v.get("part_id").and_then(|x| x.as_str()),
                    Some("y18WnBkDWUq8B7cPFFsDk")
                );
                assert_eq!(
                    v.pointer("/citations/0/url").and_then(|x| x.as_str()),
                    Some("https://example.com/")
                );
                assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("copilot"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn known_attachments_still_serialize_with_their_type_tag() {
        // The hand-written Serialize must not regress the derived variants.
        let a = Attachment::Image {
            url: Some("https://i.groupme.com/x".into()),
            source_url: None,
            blur_hash: Some("]47^xx~D4URPjF".into()),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("image"));
        assert_eq!(
            v.get("blur_hash").and_then(|t| t.as_str()),
            Some("]47^xx~D4URPjF")
        );
    }

    #[test]
    fn attachment_that_is_not_an_object_degrades_instead_of_failing() {
        let m: Message =
            serde_json::from_str(r#"{"id":"1","attachments":["nonsense",7]}"#).unwrap();
        assert_eq!(m.attachments.len(), 2);
        assert_eq!(m.attachments[0].kind(), "other");
    }

    #[test]
    fn dm_messages_carry_conversation_id_instead_of_group_id() {
        // The canonical DM thread key, and what `/v4/read_receipts` is keyed
        // on. Every captured DM has it; none has a `group_id`.
        let m: Message = serde_json::from_str(
            r#"{"id":"170000000000000010","conversation_id":"20000001+20000002",
                "sender_id":"20000001","recipient_id":"20000002","user_id":"20000001",
                "name":"Example Person","text":"placeholder body","created_at":1784752303,
                "favorited_by":[],"attachments":[]}"#,
        )
        .unwrap();
        assert_eq!(m.conversation_id.as_deref(), Some("20000001+20000002"));
        assert!(m.group_id.is_none());

        // And it must survive the archive's parse -> serialize -> reload cycle.
        let reloaded: Message = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(
            reloaded.conversation_id.as_deref(),
            Some("20000001+20000002")
        );
    }
}
