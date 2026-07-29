# The GroupMe API, as observed

Reference for the endpoints this app depends on.

**Every endpoint here was observed on the wire.** Nothing is copied from
GroupMe's published docs, and nothing is inferred. The source is a proxied
capture of `web.groupme.com` (client version `GroupMeWeb/1.2.3`) taken
2026-07-29, covering sign-in, chat list load, group and DM history paging,
sending, replying, reacting, and deleting/editing/pinning/unpinning a message —
plus one request supplied directly.

Where this contradicts [dev.groupme.com](https://dev.groupme.com/docs/v3),
believe this document. Several things below appear nowhere in the official
docs: the entire `/v4` surface, `/v3/pinned/*`, `/v3/poll/*`, subgroups,
relationships, the `omit=` and `include=` parameters, and the fact that
attachment URLs now redirect to expiring signed CDN links.

Endpoints marked **unverified** were seen returning an empty result, so the
path and status are confirmed but the populated payload shape is not.

All identifiers, names, and message text in the examples are invented.

---

## 1. Hosts

| Host | Role |
|---|---|
| `api.groupme.com` | Main API. `/v1`, `/v3`, `/v4` all live here. |
| `v2.groupme.com` | Sign-in and user lookup. Only these two. |
| `push.groupme.com` | Faye realtime (Bayeux). |
| `image.groupme.com` | Image upload and QR rendering. |
| `i.groupme.com` | Avatars. |
| `m.groupme.com` | Message attachments — **redirects** (see §7). |
| `cdn2.groupme.com` | Where attachments actually live, behind signed URLs. |

---

## 2. Authentication

### Request headers

The web client sends this on every authenticated call:

```http
GET /v3/groups?per_page=100&omit=memberships&page=1 HTTP/1.1
Host: api.groupme.com
x-access-token: <40-char token>
x-requested-with: GroupMeWeb/1.2.3
accept: application/json
Origin: https://web.groupme.com
Referer: https://web.groupme.com/chats
```

`x-access-token` is the whole auth story — a 40-character alphanumeric bearer
token. There is no `Authorization` header and no signature.

`x-requested-with: GroupMeWeb/<version>` is undocumented and not required, but
sending it makes an unofficial client's traffic indistinguishable from the web
client's. This app sends it.

A `token=` query parameter also works, but keeps the credential in proxy logs
and `Referer` chains. Use the header.

### Token lifetime

Tokens do not expire on their own and there is no refresh flow. A leaked token
is valid until revoked at
[dev.groupme.com/applications](https://dev.groupme.com/applications), and it
can read every DM and post as the user. Treat it as equivalent to the password.

This app keeps the token in **Windows Credential Manager**, never in the SQLite
archive or a config file. The archive stores only a SHA-256 fingerprint, used
to notice that a different account signed in.

### Sign-in flow

Sign-in is a three-step challenge on `v2.groupme.com`, not OAuth.

**Step 1 — credentials.** Returns `202` and a verification challenge, *not* a
token:

```http
POST https://v2.groupme.com/access_tokens
content-type: application/json

{
  "username": "5555550100",
  "password": "<password>",
  "grant_type": "password",
  "app_id": "groupme-web",
  "device_id": "<uuid v4, client-generated>"
}
```
```json
{
  "meta": { "code": 20200 },
  "response": {
    "verification": {
      "code": "<64-char hex challenge id>",
      "methods": { "sms": "00" },
      "status": "unverified",
      "code_format": { "min_length": 4, "max_length": 4, "charset": "numeric" },
      "type": "force",
      "long_pin": "<12-char hex>",
      "system_number": "+1 5555550199"
    }
  }
}
```

`type: "force"` means the second factor is mandatory, not optional. The
`methods` values are the last two digits of the destination, for display.

**Step 2 — request the code.** `{challenge}` is `verification.code` above:

```http
POST https://api.groupme.com/v3/verifications/{challenge}/initiate
{ "verification": { "method": "sms" } }
```
```json
{ "meta": {"code": 200},
  "response": { "hint": "00",
                "code_format": {"min_length":4,"max_length":4,"charset":"numeric"} } }
```

**Step 3 — confirm.**

```http
POST https://api.groupme.com/v3/verifications/{challenge}/confirm
{ "verification": { "pin": "0000" } }
```
```json
{ "meta": {"code": 200}, "response": { "status": 20000 } }
```

Steps 1–3 carry **no** `x-access-token`. How the token is finally delivered was
not captured — it is not in the confirm response body, so it arrives by cookie
or a follow-up call outside the recorded window. **Unverified.**

> This app does not implement sign-in. The user signs in to the real
> `web.groupme.com` inside the webview, and the app lifts the resulting
> `x-access-token` off an outgoing request header. Reimplementing a password
> and 2FA flow to obtain a credential the webview already holds would add risk
> and no capability.

---

## 3. Response envelope

Everything is wrapped:

```json
{ "meta": { "code": 200 }, "response": { } }
```

> **`meta.code` is not always the HTTP status.** Observed: `/v3/poll/{id}`
> returned HTTP `200` with `"code": 20000`, and `POST /access_tokens` returned
> HTTP `202` with `"code": 20200`. These look like an internal 5-digit scheme
> layered over the HTTP one. **Branch on the HTTP status; treat `meta.code` as
> advisory.** Asserting `meta.code == 200` will reject perfectly good responses.

`response` is sometimes `null`, and sometimes `[]` — both mean "nothing here."
Handle both without erroring; see §5.

One endpoint escapes the envelope entirely: `DELETE …/messages/{id}` returns
`204` with a zero-length body (§4). `/v1/urls/preview` is the other exception,
for a different reason (§6).

---

## 4. Endpoint catalogue

Every row below was observed. Status is what was actually returned.

### Identity and contacts

| Method | Path | Query | Status | Notes |
|---|---|---|---|---|
| `GET` | `/v3/users/me` | — | 200 | Full profile: id, name, email, phone, bio, `mfa`, `consents`, `tags` |
| `GET` | `v2.groupme.com/users/{user_id}` | `include_shared_groups` | 200 | Another user's public profile + `relationship` |
| `GET` | `/v4/relationships` | `include_blocked`, `since` | 200 | Contact list. `since` is an ISO-8601 timestamp for delta sync |
| `GET` | `/v3/directories` | — | 200 | Returned `[]`. **Unverified.** |

`/v4/relationships` entry:

```json
{
  "id": "10000001", "user_id": "10000001", "name": "Example Contact",
  "avatar_url": "https://i.groupme.com/…",
  "created_at": 1740000000, "created_at_iso8601": "2025-02-20T05:09:02.226176Z",
  "updated_at": 1740000000, "updated_at_iso8601": "2025-02-20T05:09:02.226176Z",
  "reason": 1, "hidden": false, "app_installed": true, "is_blocked": false
}
```

### Conversations

| Method | Path | Query | Status | Notes |
|---|---|---|---|---|
| `GET` | `/v3/groups` | `per_page`, `page`, `omit` | 200 | `omit=memberships` drops the member arrays |
| `GET` | `/v3/groups/{group_id}` | `include` | 200 | `include=members` to get memberships back |
| `GET` | `/v3/chats` | `per_page`, `page` | 200 | DM threads |
| `GET` | `/v3/groups/{group_id}/members` | `filter` | 200 | `filter=inactive` → members with `state` |
| `GET` | `/v3/groups/{group_id}/pending_memberships` | — | 200 | Returned `[]`. **Unverified.** |
| `GET` | `/v3/groups/{group_id}/subgroups` | — | 200 | Returned `[]`. **Unverified.** |
| `GET` | `/v4/pinned_conversations` | — | 200 | `{ "pinned_conversation_ids": ["…"] }` |

> `/v3/groups` and `/v3/chats` take **`per_page`**, while message endpoints take
> **`limit`**. This inconsistency is real; don't normalize it away.

> **`omit=memberships` is the single biggest performance lever here.** A member
> array on a 5,000-member group dwarfs the rest of the payload. The web client
> lists groups with `omit=memberships`, then fetches
> `/v3/groups/{id}?include=members` only for the group actually opened. This app
> does the same.

### Messages

| Method | Path | Query | Status | Response key |
|---|---|---|---|---|
| `GET` | `/v3/groups/{group_id}/messages` | `limit`, `before_id`, `acceptFiles` | 200 | `messages` |
| `POST` | `/v3/groups/{group_id}/messages` | — | **201** | `message` |
| `GET` | `/v3/direct_messages` | `other_user_id`, `limit`, `acceptFiles` | 200 | `direct_messages` |
| `GET` | `/v3/pinned/groups/{group_id}/messages` | — | 200 | `messages` |
| `GET` | `/v3/pinned/direct_messages` | `other_user_id` | 200 | `direct_messages` |

`acceptFiles=1` is sent by the web client on every history fetch. Undocumented;
presumably opts into file attachments. Harmless, and this app mirrors it.

### Message management

Delete, edit, pin and unpin. All four take the message id in the path; none
takes a query parameter.

| Method | Path | Status | Response |
|---|---|---|---|
| `DELETE` | `/v3/conversations/{conversation_id}/messages/{message_id}` | **204** | Empty — no envelope at all |
| `PUT` | `/v4/groups/{group_id}/messages/{message_id}` | 200 | `message` |
| `POST` | `/v3/conversations/{conversation_id}/messages/{message_id}/pin` | 200 | `null` |
| `POST` | `/v3/conversations/{conversation_id}/messages/{message_id}/unpin` | 200 | `null` |

> **These four do not share a path shape.** Delete and pin/unpin sit under
> `/v3/conversations/{id}/messages/…`; edit sits under
> **`/v4/groups/{id}/messages/…`** — a different version *and* a different
> collection noun for the same object. Reactions are a third shape,
> `/v3/messages/{conversation_id}/{message_id}/…`, and the read side of pinning
> is a fourth, `/v3/pinned/groups/{group_id}/messages`. There is no single
> message resource path; route each verb individually.

Only groups were exercised. The DM forms are **unverified** — the
`conversations` prefix suggests passing `"{lower_user_id}+{higher_user_id}"` as
`conversation_id` for delete/pin/unpin, but that was not observed, and no
`/v4/chats/…` or `/v4/direct_messages/…` edit route appeared at all.

**Delete.**

```http
DELETE /v3/conversations/{conversation_id}/messages/{message_id} HTTP/1.1
Host: api.groupme.com
x-access-token: <40-char token>
x-requested-with: GroupMeWeb/1.2.3
Accept: */*
```

No request body and no `content-type`. Returns **`204 No Content`** with a
zero-length body — the only endpoint observed that does not return the
`meta`/`response` envelope. Code that decodes JSON unconditionally on the
message routes throws here.

The target need not be the caller's own message: the observed call deleted
another member's message, as a group admin. Compare
`message_deletion_mode: ["admin", "sender"]` on the group payload (§6), which is
what governs who may call this.

**Edit.**

```http
PUT /v4/groups/{group_id}/messages/{message_id}
content-type: application/json

{ "text": "Example edited body", "attachments": [] }
```
```json
{ "meta": { "code": 200 },
  "response": { "message": {
    "id": "170000000000000001",
    "source_guid": "3f1c…",
    "group_id": "10000001",
    "user_id": "20000001", "sender_id": "20000001", "sender_type": "user",
    "name": "Example Sender", "avatar_url": null,
    "text": "Example edited body",
    "created_at": 1785303377,
    "updated_at": 1785303382,
    "system": false, "platform": "gm",
    "favorited_by": [], "attachments": [],
    "pinned_at": null, "pinned_by": ""
  } } }
```

The request body is a bare `{text, attachments}` — **not** wrapped in
`"message"` the way `POST /v3/groups/{id}/messages` wraps its payload, and it
carries no `source_guid`. `attachments: []` was sent explicitly; whether
omitting the key preserves existing attachments or clears them is **unverified**.

The response message carries `updated_at`, which the create response does not.
`message_edit_period` on the group (§6) presumably bounds how long an edit is
accepted; no expired edit was attempted, so the rejection status is
**unverified**.

The `/v4` route answers with
`access-control-allow-methods: POST, GET, PUT, PATCH, DELETE, OPTIONS`, while
the `/v3` routes omit `PATCH`. A partial `PATCH` on the same path is therefore
plausible and **unverified**.

**Pin and unpin.**

```http
POST /v3/conversations/{conversation_id}/messages/{message_id}/pin
Content-Length: 0
Accept: */*
```
```json
{ "meta": { "code": 200 }, "response": null }
```

`unpin` is identical in shape. Both send **no body and no `content-type`**, and
both return `response: null`, so the new state has to be read back from
`/v3/pinned/groups/{group_id}/messages` — which the web client does immediately
after each call. That read returns the message with `pinned_at`/`pinned_by`
filled in:

```json
{ "id": "170000000000000001", "group_id": "10000001",
  "user_id": "20000001", "sender_id": "20000001", "sender_type": "user",
  "name": "Example Sender", "avatar_url": null,
  "source_guid": "3f1c…", "text": "Example edited body",
  "created_at": 1785303377, "updated_at": 1785303382,
  "system": false, "platform": "gm",
  "attachments": [], "favorited_by": [],
  "pinned_at": 1785303387, "pinned_by": "20000001" }
```

A conversation holds more than one pin — `count: 2` was observed on that list.
`pinned_by` is a user id as a **string**, unlike the empty-string default on an
unpinned message.

**How they propagate.** Delete, edit and pin each land *twice*: the original
message row is rewritten in place, **and** a new system message is appended to
the conversation. Both are visible on the next `GET …/messages`. Unpin is the
exception — see below.

A deleted message keeps its id, sender, `created_at` and `source_guid`, and
gains a tombstone:

```json
{ "id": "170000000000000002", "group_id": "10000001",
  "user_id": "20000002", "sender_id": "20000002", "sender_type": "user",
  "name": "Example Member", "avatar_url": null,
  "source_guid": "…", "system": false,
  "text": "An admin deleted this message",
  "created_at": 1785303339,
  "deleted_at": 1785303362, "deletion_actor": "admin",
  "attachments": [], "favorited_by": [],
  "platform": "gm", "pinned_at": null, "pinned_by": "" }
```

`deletion_actor` was observed as `"admin"` and `"sender"`, and the substituted
`text` tracks it — `"An admin deleted this message"` versus
`"This message was deleted"`. The row is **never removed** from the history
listing; `deleted_at`/`deletion_actor` appear only on deleted messages, so their
presence is the test.

The companion system message:

```json
{ "id": "170000000000000005", "group_id": "10000001",
  "name": "GroupMe",
  "user_id": "system", "sender_id": "system", "sender_type": "system",
  "system": true, "source_guid": "<32-char hex>",
  "text": "A message was deleted.",
  "created_at": 1785303362,
  "event": { "type": "message.deleted",
             "data": { "message_id": "170000000000000002",
                       "deleted_at": 1785303362,
                       "deletion_actor": "admin" } },
  "attachments": [], "favorited_by": [],
  "platform": "gm", "pinned_at": null, "pinned_by": "" }
```

An edit rewrites `text` on the original and adds `updated_at`; its system
message carries the whole new content, so a client can apply the edit without
refetching:

```json
{ "text": "Example Sender edited to: “Example edited body”",
  "event": { "type": "message.update",
             "data": { "message_id": "170000000000000001",
                       "sender_id": "20000001",
                       "updated_at": 1785303382,
                       "message": { "text": "Example edited body",
                                    "attachments": [] } } } }
```

A pin appends `message.pinned`:

```json
{ "text": "Example Sender pinned a message.",
  "event": { "type": "message.pinned",
             "data": { "message_id": "170000000000000001",
                       "pinned": true,
                       "pinned_by": "20000001",
                       "pinned_at": 1785303387 } } }
```

> **Unpin emits nothing.** No `message.unpinned` event and no `message.pinned`
> with `pinned: false` followed the unpin. The only signal is `pinned_at` and
> `pinned_by` reverting to `null` and `""` on the message, and the row dropping
> out of `/v3/pinned/…`. A client that tracks pins from the event stream alone
> displays a stale pin forever; the pinned list is the source of truth.

Full `event.type` census over this capture: `message.deleted`, `message.update`,
`message.pinned`, `membership.announce.added`, `membership.announce.joined`,
`membership.announce.rejoined`, `membership.notifications.removed`,
`membership.notifications.exited`, `group.role_change_admin`, `bot.add`.

### Reactions

| Method | Path | Status |
|---|---|---|
| `POST` | `/v3/messages/{conversation_id}/{message_id}/like` | 200 |
| `POST` | `/v3/messages/{conversation_id}/{message_id}/unlike` | 200 |

Note the path takes **both** the conversation id and the message id, and lives
under `/v3/messages/`, not under the group or chat.

```http
POST /v3/messages/{conversation_id}/{message_id}/like
content-type: application/json

{ "like_icon": { "type": "unicode", "code": "🤣" } }
```
```json
{ "meta": { "code": 200 },
  "response": { "reactions": [
    { "type": "unicode", "pack_id": 0, "pack_index": 0,
      "code": "🤣", "user_ids": ["20000001"] } ] } }
```

The response returns the message's **full** reaction list, not a delta — so it
can be written straight over the stored value rather than merged.

> `pack_id`/`pack_index` come back as **integers** (`0`) here, while the same
> fields on a message read arrive as **strings**. Parse both permissively.
> `unlike` takes the same body shape and returns the remaining reactions.

`like` is the endpoint name, `reactions` is the response field, and
`favorited_by` is the flattened list on a message read. Three names, one
concept — an artefact of the feature growing from a like button into arbitrary
emoji reactions.

### Read state

| Method | Path | Status | Body |
|---|---|---|---|
| `GET` | `/v4/read_receipts` | 200 | `{ "receipts": [{ "conversation_id", "last_read_message_id" }] }` |
| `POST` | `/v4/read_receipts/{conversation_id}` | **202** | `{ "last_read_message_id": "…" }` → `{ "status": "accepted" }` |

The whole read-state map arrives in one call, keyed by conversation. `202` is
correct here — the write is accepted asynchronously.

### Extras

| Method | Path | Query | Status | Notes |
|---|---|---|---|---|
| `GET` | `/v1/urls/preview` | `url` | 200 | Open Graph unfurl. Note **`/v1`** |
| `GET` | `/v3/conversations/{conversation_id}/events/list` | `end_at`, `limit` | 200 | Calendar events. `end_at` is ISO-8601 with offset |
| `GET` | `/v3/poll/{conversation_id}` | — | 200 | Polls; `meta.code` was `20000` |
| `POST` | `/v3/web_pings` | — | 201 | `{ "ttl": 1200 }`, empty `text/html` reply. Presence keepalive |

---

## 5. Pagination

### Messages

| Param | Returns | Order | Status |
|---|---|---|---|
| `before_id` | Strictly older than the given id | **newest-first** | confirmed |
| `after_id` | Newer than the given id | oldest-first | unverified |
| `since_id` | Newer than the given id | newest-first | unverified |
| `limit` | Page size, max **100** | — | confirmed |

Only `before_id` appeared in this capture: the web client pages backwards on
scroll and receives new messages over the Faye socket rather than polling
forward. `after_id`/`since_id` are carried from prior observation and are **not**
confirmed here — treat their ordering as unknown.

### Going further back — the confirmed chain

Observed while scrolling back through a group of 4,551 messages:

```
GET /v3/groups/{id}/messages?acceptFiles=1&limit=100
    -> messages[0]  = 178530181614165510   (newest)
       messages[99] = 178465652287931200   (oldest)

GET /v3/groups/{id}/messages?acceptFiles=1&limit=100&before_id=178465652287931200
    -> messages[0]  = 178465412837361938
       messages[99] = 178444842257092684

GET /v3/groups/{id}/messages?acceptFiles=1&limit=100&before_id=178444842257092684
    -> …
```

**The cursor is the last element of the page you just received**, because pages
arrive **descending** — `messages[0]` is the newest and `messages[len-1]` the
oldest. Taking `messages[0]` as the cursor walks forwards into nothing and the
backfill stalls on page two.

**`before_id` is exclusive.** Page two's newest id is strictly below the cursor,
so pages do not overlap and no message is fetched twice.

Do not rely on array position anyway. This app takes the **minimum `id_sort`**
of the page as the next `before_id` and sorts every page by that integer key on
arrival, which is correct whichever order the server chooses today.

**Terminate on an empty page, not a short one** — short pages occur mid-history,
and treating one as the end silently truncates the archive.

`response.count` is the conversation's **lifetime total**, not the page size,
and stays constant across pages (`4551` on every page above). That makes it a
free completeness check: when local rows reach `count`, the backfill is done —
useful because it catches a stalled backfill that an empty-page check alone
would call finished.

Some conversations legitimately return fewer than `limit` on the very first
call (observed: `count: 1` and `count: 3` returning 1 and 3 messages). A short
first page is not an error and not a truncation.

### Groups and chats

`per_page=100&page=N`, incrementing until a short page.

---

## 6. Payload shapes

### Message

```json
{
  "id": "170000000000000001",
  "source_guid": "3f1c…",
  "group_id": "10000001",
  "user_id": "20000001",
  "sender_id": "20000001",
  "sender_type": "user",
  "name": "Example Sender",
  "avatar_url": "https://i.groupme.com/…",
  "text": "Example message body",
  "created_at": 1785301508,
  "system": false,
  "platform": "gm",
  "favorited_by": [],
  "attachments": [],
  "pinned_at": null,
  "pinned_by": ""
}
```

| Field | Notes |
|---|---|
| `id` | Decimal string. **Never parse as a JS number** — see §8 |
| `text` | **Nullable.** Attachment-only messages carry `"text": null` |
| `avatar_url` | **Nullable** |
| `sender_type` | `user` \| `bot` \| `system` |
| `pinned_by` | Empty **string** when unpinned, not `null` |
| `pinned_at` | Unix seconds when pinned |
| `platform` | `gm` on everything observed |
| `favorited_by` | User ids who reacted |
| `source_guid` | Client-generated idempotency key; echoed back on send |
| `updated_at` | **Absent unless the message was edited.** Unix seconds |
| `deleted_at` | **Absent unless deleted.** Unix seconds |
| `deletion_actor` | **Absent unless deleted.** `admin` \| `sender` |
| `event` | Present only on system messages; see §4, Message management |

DM messages add `conversation_id` and `recipient_id`, and omit `group_id`,
`system` and `platform` — all three are present on every group message and on
none of the DM messages observed.

**DM conversation ids are `"{lower_user_id}+{higher_user_id}"`**, the two user
ids sorted **numerically** ascending and joined with `+`. Sorting them as
strings produces the wrong key for ids of differing length.

`GET /v3/direct_messages` also returns a sibling `read_receipt` object next to
`direct_messages`:

```json
{ "id": "", "chat_id": "20000001+20000002",
  "message_id": "170000000000000001", "user_id": "20000002",
  "read_at": 1784752304 }
```

### Attachments

An **open union** — GroupMe has shipped new `type` values without notice. An
unknown type must never fail the containing message; losing a message to an
unrecognised sticker is the worst outcome for an archive. This app parses
unknown types into a passthrough variant.

Observed in this capture:

```json
{ "type": "image", "url": "https://m.groupme.com/uploads/{hash}/1792x2400.original.jpeg" }

{ "type": "reply", "user_id": "20000001",
  "reply_id": "170000000000000001", "base_reply_id": "170000000000000001" }
```

From the supplied request, also confirmed:

```json
{ "type": "image", "url": "…", "source_url": "…", "blur_hash": "]47^xx~D4URPjF…" }

{ "type": "mentions", "user_ids": ["20000001", "-1"], "loci": [[0, 12], [20, 8]] }

{ "type": "emoji", "placeholder": "�", "charmap": [[1, 5]] }
```

- `loci` is `[[start_char, length], …]`, parallel to `user_ids`.
- A `user_id` of **`"-1"`** is `@everyone`, not a real account. Joining it
  against a users table finds nothing; special-case it.
- `reply_id` is the message replied to; `base_reply_id` is the thread root.
  They differ when replying to a reply.
- `blur_hash` decodes to a blurred placeholder from ~30 bytes — useful for
  showing *something* offline when an image was never cached. Older messages
  lack it.

### Sending

```http
POST /v3/groups/{group_id}/messages
content-type: application/json

{
  "message": {
    "source_guid": "<client-generated uuid v4>",
    "text": "Example reply",
    "attachments": [
      { "type": "reply", "reply_id": "170000000000000001",
        "base_reply_id": "170000000000000001", "user_id": "20000001" }
    ]
  }
}
```

Returns **`201`** with the created message under `response.message`, echoing
`source_guid` — that is the idempotency handle for matching an optimistic local
row to the server's.

> This app never calls this endpoint. Sending is left entirely to the real
> `web.groupme.com` in the webview. The archiver is read-only against the API,
> which is what makes offline read-only structural rather than a UI check.

### Group

```json
{
  "id": "10000001", "group_id": "10000001",
  "name": "Example Group", "type": "closed",
  "description": "…", "image_url": "https://i.groupme.com/…",
  "creator_user_id": "20000001",
  "created_at": 1730821439, "updated_at": 1766619877,
  "phone_number": "+1 5555550100",
  "muted_until": 253402300800,
  "messages": {
    "count": 46070,
    "last_message_id": "170000000000000001",
    "last_message_created_at": 1785278295,
    "last_message_updated_at": 1785278295,
    "preview": { "nickname": "Example Sender", "text": "…",
                 "image_url": "", "attachments": [] }
  },
  "max_members": 5000,
  "theme_name": "music",
  "like_icon": { "type": "emoji", "pack_id": 1, "pack_index": 57 },
  "requires_approval": true,
  "show_join_question": true,
  "join_question": { "type": "join_reason/questions/text", "text": "…" },
  "message_deletion_period": 2147483647,
  "message_deletion_mode": ["admin", "sender"],
  "message_edit_period": 15,
  "children_count": 0,
  "share_url": "https://groupme.com/join_group/10000001/XXXXXXXX",
  "share_qr_code_url": "https://image.groupme.com/qr/join_group/…",
  "system_message_settings": {
    "all_notifications": true,
    "categories": { "albums": ["member","creator","admin"],
                    "events": ["member","host","admin"],
                    "join_leave": ["member","admin"] }
  },
  "members": null, "members_count": 0,
  "unread_count": null, "last_read_message_id": null, "last_read_at": null
}
```

- With `omit=memberships`, `members` is `null` and `members_count` is `0` —
  **`0` here means "not loaded", not "empty group."** Don't persist it as truth.
- `muted_until: 253402300800` is year-9999, i.e. muted forever.
- `like_icon.pack_id`/`pack_index` are **numbers** here, but the same fields
  arrive as **strings** inside a message's `reactions`. See §8.
- `unread_count`/`last_read_*` were `null` throughout; read state comes from
  `/v4/read_receipts` instead.

### Membership

`GET /v3/groups/{id}/members?filter=inactive` returns `{ "memberships": [...] }`:

```json
{ "id": "1000000001", "user_id": "20000001",
  "name": "Example Person", "nickname": "Example",
  "state": "removed", "roles": ["user"] }
```

`state` observed: `removed`, `exited`. In the full group payload, members
instead look like:

```json
{ "id": "1000000001", "user_id": "20000001",
  "nickname": "Example", "name": "Example Person",
  "image_url": "", "muted": true, "autokicked": false,
  "roles": ["admin", "owner"] }
```

> **`id` is the membership id, not the user id.** They are different numbers,
> and member-management endpoints want the membership id.

### Chat (DM thread)

```json
{
  "created_at": 1781230932, "updated_at": 1784752303,
  "messages_count": 108,
  "other_user": { "id": "20000001", "name": "Example Contact",
                  "avatar_url": "https://i.groupme.com/…" },
  "last_message": { },
  "message_deletion_period": 2147483647,
  "message_deletion_mode": ["sender"],
  "message_edit_period": 15,
  "requires_approval": false,
  "unread_count": null, "last_read_message_id": null, "last_read_at": null
}
```

A DM thread has no id of its own — it is keyed by `other_user.id`. This app
uses that as the conversation id.

### Events

`GET /v3/conversations/{conversation_id}/events/list?end_at=<iso8601>&limit=100`
→ `{ "events": [...] }`:

```json
{
  "event_id": "<32-char hex>", "conversation_id": "10000001",
  "name": "Example Event", "creator_id": "20000001",
  "start_at": "2025-11-30T20:47:00-05:00",
  "end_at": "2025-11-30T21:30:00-05:00",
  "is_all_day": false, "timezone": "America/New_York",
  "scheduled_call": false, "call_started": false,
  "reminders": [0],
  "aesthetics": { "font": "NONE", "theme": "NONE", "effect": "NONE" },
  "going": ["20000001"], "not_going": [], "maybe_going": [], "waitlisted": [],
  "going_count": 0,
  "rsvp_list": { "20000001": "2025-12-01T01:45:34Z" },
  "created_at": "2025-12-01T01:45:34Z", "updated_at": "2025-12-01T02:02:53Z",
  "share_url": "https://groupme.com/join_event/…",
  "deep_link_ios": "groupme://join_event/…",
  "deep_link_android": "groupme://groupme.com/join_event/…",
  "is_top_level": false
}
```

> Event timestamps are **ISO-8601 strings**, while message timestamps are Unix
> integers. Both appear in the same API. `going_count: 0` alongside a non-empty
> `going` array was observed — derive the count from the array.

### Polls

`GET /v3/poll/{conversation_id}` → `{ "polls": [{"data": {…}}], "continuation_token": null }`:

```json
{
  "id": "1766515361214035", "conversation_id": "10000001",
  "subject": "Example poll question?", "owner_id": "20000001",
  "created_at": 1766515361, "expiration": 1767675561,
  "last_modified": 1767675575,
  "status": "past", "type": "single", "visibility": "anonymous",
  "options": [
    { "id": "1", "title": "Option A", "votes": 1 },
    { "id": "2", "title": "Option B", "votes": 3 }
  ]
}
```

Each poll is nested under a `data` key. `visibility` observed as `anonymous`
and `public`; only `public` polls carry `voter_ids` on options, and an option
with zero votes may omit `votes` entirely rather than sending `0`.
`continuation_token` implies pagination — its use is **unverified**.

### URL preview

`GET /v1/urls/preview?url=<encoded>`:

```json
{
  "meta": { "title": "…", "description": "…", "site": "…",
            "canonical": "https://…", "medium": "link", "theme-color": "#1e2327" },
  "links": {
    "thumbnail": [{ "href": "https://…", "type": "image",
                    "rel": ["twitter","thumbnail"],
                    "media": { "width": 1200, "height": 600 } }],
    "icon": [{ "href": "https://…", "type": "image/png", "rel": ["icon"] }]
  },
  "rel": []
}
```

Note this response is **not** wrapped in the usual `meta`/`response` envelope —
its top-level `meta` is Open Graph metadata, not a status block. Special-case it.

---

## 7. Media, and why URLs are not archivable

Attachment URLs in message payloads point at `m.groupme.com`:

```
https://m.groupme.com/uploads/{hash}/1792x2400.original.jpeg
```

That returns **`301`** to `cdn2.groupme.com`, where the real object sits behind
an **Azure Blob Storage SAS signature**:

```
https://cdn2.groupme.com/uploads/{hash}/original.jpeg
    ?sv=…&se=…&sr=…&sp=…&sig=…
    &skoid=…&sktid=…&ske=…&sks=…&skt=…&skv=…&rsct=…
```

`se` is the expiry and `sig` the signature.

> **This is the single most important consequence for an archive.** Storing the
> attachment URL is not archiving the attachment — the `m.groupme.com` URL needs
> a live redirect and a fresh signature to resolve, so a "cached" message with a
> stored URL shows a broken image offline, and eventually breaks online too.
>
> The bytes must be downloaded and stored locally. This app follows the redirect
> at sync time, saves the object to a blob directory, and records the mapping in
> `media_cache`. `blur_hash`, where present, is the fallback for anything not yet
> fetched.

Avatars on `i.groupme.com` are unsigned and stable, e.g.
`https://i.groupme.com/{w}x{h}.{ext}.{hash}`. A `.avatar` suffix variant serves
a cropped rendition, and a `.large` variant exists. Several avatar requests
returned **`403`** during capture while others succeeded — treat avatar fetch
failure as routine and non-fatal.

Upload (not used by this app, from prior observation): `POST` raw bytes to
`https://image.groupme.com/pictures` with a matching `Content-Type`, returning
`{"payload": {"url": "…"}}`.

---

## 8. Gotchas

**IDs must be strings.** `170000000000000001` exceeds IEEE-754 integer
precision (2^53). Round-trip it through a JS `Number` or a JSON parser that
defaults to float and it silently corrupts into a different, valid-looking id.
Store as `TEXT`. They *do* fit in a signed 64-bit integer, so this app keeps a
parallel `id_sort INTEGER` column for ordering and cursors, and never uses it
as identity.

**`meta.code` is not the HTTP status.** Observed `20000` and `20200` on HTTP
`200`/`202` responses. Branch on the HTTP status.

**Types are not stable across endpoints.** `like_icon.pack_id` is a JSON number
on a group but a string in a message's `reactions`. IDs inside system-message
`event.data` split by event family: the `membership.*` events carry numbers
(`{"id": 20000001, "nickname": "Example"}`), while `message.deleted`,
`message.update` and `message.pinned` carry strings (`"message_id":
"170000000000000001"`). Same object, same envelope, both types. Parse
permissively and normalize on the way in.

**Timestamps come in two formats.** Unix seconds on messages, groups, chats and
polls; ISO-8601 strings on events and `/v4/relationships`.

**Response keys differ for identical shapes.** `messages` for groups,
`direct_messages` for DMs.

**`per_page` vs `limit`.** Conversation lists take `per_page`; message
endpoints take `limit`.

**`members_count: 0` with `omit=memberships` means "not loaded."** Persisting
it as a real count silently zeroes every group.

**Status codes vary by verb.** `POST` a message → `201`. `POST` a read receipt
→ `202`. `DELETE` a message → `204`. `PUT` an edit and `POST` a pin → `200`. Do
not assert `200`.

**Empty is spelled several ways.** `"response": []`, `"response": null`, and
`{"count": 0, "messages": []}` all occur. All three must be handled as "no
results" rather than as errors — this is the backfill terminator, so getting it
wrong means either an infinite loop or a truncated archive.

**Rate limits are undocumented.** No `429` was triggered during capture, so no
limit is confirmed here. Prior observation puts it near 10 req/s per token.
This app serialises sync requests and backs off exponentially (1s/2s/4s with
jitter) on `429` and `5xx`.

**Deletions and edits land twice — as a system message *and* as an in-place
rewrite of the original.** Both were observed end to end (§4, Message
management). The system message is the push signal; the rewritten original is
what a refetch returns. An archive that applies only the events, or only the
refetch, is correct either way, but one that applies neither keeps showing
deleted content forever.

**A deleted message is not deleted.** The row stays in `…/messages` with its id,
sender and `created_at` intact; only `text` is replaced with a tombstone and
`deleted_at`/`deletion_actor` are added. Detect it by key presence, not by
absence from the page — and never by matching the tombstone string, which
differs by actor (`"An admin deleted this message"` vs
`"This message was deleted"`).

**`updated_at` exists only after an edit.** Never-edited messages omit the key
entirely rather than mirroring `created_at` (9 of 1,416 group messages carried
it in this capture). A schema that reads `updated_at` as "last touched" gets
`null` for almost everything.

**`DELETE` returns `204` with no envelope.** Every other endpoint returns
`{"meta":…,"response":…}`; this one returns zero bytes. Decoding the body
unconditionally throws on the one call whose success is hardest to retry safely.

**Message mutation has four different path shapes.** Delete and pin/unpin are
`/v3/conversations/{id}/messages/…`, edit is `/v4/groups/{id}/messages/…`,
reactions are `/v3/messages/{conversation_id}/{message_id}/…`, and the pinned
list is `/v3/pinned/groups/{group_id}/messages`. Nothing generalises; hardcode
each.

**Unpin is silent.** `pin` appends a `message.pinned` system message; `unpin`
appends nothing at all. Pin state must be reconciled against
`/v3/pinned/…`, not accumulated from events.

**DM messages have no `system` and no `platform` key.** Group messages carry
both on every row (1,416 of 1,416); DM rows carry neither (0 of 100), including
DM *system* messages, which are identifiable only by
`sender_type: "system"`. Branching on `message.system == true` silently misses
every edit and delete notice in a DM.

---

## 9. Realtime (Faye)

`push.groupme.com` runs a [Faye](https://faye.jcoglan.com/) (Bayeux) server.
The web client loads `faye.min.js` and handshakes over JSONP before upgrading:

```
GET https://push.groupme.com/faye
    ?message=[{"channel":"/meta/handshake","version":"1.0",…}]
    &jsonp=__jsonp1__
```
```js
/**/__jsonp1__([{
  "id": "1",
  "channel": "/meta/handshake",
  "successful": true,
  "version": "1.0",
  "supportedConnectionTypes": [
    "long-polling","cross-origin-long-polling","callback-polling",
    "websocket","eventsource","in-process"
  ],
  "clientId": "<32-char client id>",
  "advice": { "reconnect": "retry", "interval": 0, "timeout": 600000 }
}]);
```

A request to the same path returned **`101 Switching Protocols`** — the
websocket upgrade. The JSONP exchange above is only the bootstrap, and the
client switches to the socket immediately: `supportedConnectionTypes` in the
client's handshake lists `websocket` **first**.

`advice.timeout: 600000` (10 minutes) with `reconnect: "retry"` and
`interval: 0` is a standard Bayeux long-poll hold.

### Subscribing

Frames sent after the websocket upgrade are invisible to an HTTP proxy. To
observe them, `tools/capture_api.py --force-longpoll` deletes `window.WebSocket`
and `window.EventSource` before any page script runs; Faye then negotiates
`cross-origin-long-polling`, which is ordinary HTTP and fully visible.

Under long-polling the transport becomes a **`POST` to `/faye` with a
form-encoded body**, `message=<url-encoded JSON array>` — not the `GET` +
`jsonp` used for the bootstrap.

Bayeux messages are **batched**, several per array. Observed on a conversation
switch (unsubscribe the old channel, subscribe the new one, in one request):

```json
[
  { "channel": "/meta/unsubscribe",
    "clientId": "<32-char client id>",
    "subscription": "/group/10000001",
    "id": "d" },
  { "channel": "/meta/subscribe",
    "clientId": "<32-char client id>",
    "subscription": "/group/10000002",
    "id": "e",
    "ext": { "access_token": "<40-char token>" } }
]
```

| Element | Value |
|---|---|
| Channel for a group | `/group/{group_id}` |
| Auth | `ext.access_token` on the **subscribe** message |
| `clientId` | from the `/meta/handshake` response |
| `id` | per-message correlation id, incrementing (`"d"`, `"e"`, `"f"`, `"g"`) |

> **The token travels in the Bayeux message body, not a header.** There is no
> `x-access-token` on these requests — the whole exchange is authenticated by
> `ext.access_token` inside the subscribe frame. Anything reading only headers
> will conclude these calls are unauthenticated.

The DM channel name was **not** captured (only group channels were subscribed
during the session). `/user/{user_id}` is the obvious guess given the group
pattern, but it is **unverified** — confirm before relying on it.

Long-poll requests returned `504 Gateway Time-out` through the capture proxy.
That is an artefact of the proxy's timeout being shorter than the
`advice.timeout` of 600 s, not a GroupMe error.

### Still missing before this can be implemented

- The **inbound message frame** shape — what actually arrives on a subscribed
  channel when someone sends a message. The 504s meant no long-poll ever
  completed with a payload.
- The DM channel name.
- Whether `/meta/connect` needs `ext.access_token` too, or only `/meta/subscribe`.

**This app still polls** until those are known. But the hard part — channel
naming and how the subscription authenticates — is now documented.

---

## 10. What this app calls

Deliberately small, and **read-only**: no `POST`, `PUT`, or `DELETE` is issued
against the API at all.

| Call | When |
|---|---|
| `GET /v3/users/me` | Once after token capture, to fingerprint the account |
| `GET /v3/groups?per_page=100&omit=memberships&page=N` | Each sync cycle |
| `GET /v3/chats?per_page=100&page=N` | Each sync cycle |
| `GET /v3/groups/{id}?include=members` | When a group's members are stale |
| `GET /v3/groups/{id}/messages?limit=100&before_id=…` | Backfill until empty |
| `GET /v3/groups/{id}/messages?limit=100&after_id=…` | Tailing |
| `GET /v3/direct_messages?other_user_id=…&limit=100` | Same two patterns for DMs |
| `GET /v4/read_receipts` | Each sync cycle, for unread state |
| `GET` on `m.groupme.com` / `cdn2` / `i.groupme.com` | Downloading media bytes |

---

## 11. Reproducing this capture

`tools/capture_api.py` drives Chrome behind a selenium-wire MITM proxy and logs
full request/response detail to `tools/capture-out/traffic.jsonl`;
`tools/digest_capture.py` reduces that to a reviewable digest.

selenium-wire 5.1.0 has been unmaintained since 2023 and its vendored mitmproxy
is built against a pyOpenSSL X509 API that was removed in 23.3. Four call sites
break, and the symptom is that no page loads at all rather than an obvious
error. `capture_api.py` monkey-patches all four (`Cert.altnames`, `create_ca`,
`dummy_cert`, `CertStore.create_store`) onto `cryptography` at import time.

> **The capture output contains live credentials** — access token, session
> cookies, and, if sign-in happens during recording, the password field and the
> 2FA PIN, plus the plaintext of every message fetched. `tools/capture-out/`
> and `tools/.chrome-profile/` are gitignored. Revoke the token afterwards.

---

## 12. Not covered

Confirmed to exist but not documented above, because the capture returned empty
results or the flow was not exercised:

- `/v3/directories`, `/v3/groups/{id}/pending_memberships`,
  `/v3/groups/{id}/subgroups` — all returned empty
- Faye `/meta/subscribe` and channel authentication (§9)
- How the access token is finally issued at the end of sign-in (§2)
- Member add/remove, group create/update
- Message delete/edit/pin/unpin **in a DM** — the group forms are documented
  (§4), the DM paths were never exercised
- File attachments: `POST file.groupme.com/v1/{conversation_id}/fileData` was
  observed returning file metadata, but the upload that produces a `file_id`
  was not
- Image upload
- Copilot
- `continuation_token` pagination on polls

### Not missing: message search

No search endpoint appeared anywhere in the capture, and that is not a gap in
the capture — **GroupMe has no message search.** There is no endpoint to
document because the feature does not exist.

This is the single strongest argument for the local archive. Full-text search
over the user's entire history is not a degraded offline substitute for
something the web client does better; it is a capability the product does not
offer at all. `messages_fts` (FTS5, see `docs/schema.md`) answers in
milliseconds across hundreds of thousands of messages.

**Design decision: search stays offline-only.** It is reachable through the
bundled reader, which the window switches to when the network drops. Surfacing
it while online was considered — a separate archive window, a global hotkey, an
overlay injected into the live page — and deliberately not built. The overlay
option in particular would mean injecting UI into third-party markup that
GroupMe can change without notice, which is the fragile coupling this app's
whole architecture avoids.

Revisit if the archive turns out to be something people reach for rather than
something they fall back on.
