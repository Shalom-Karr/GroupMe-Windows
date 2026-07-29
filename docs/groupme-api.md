# The GroupMe API, as observed

A protocol reference for building a GroupMe client. Organised by what a client
builder needs — authenticate, read, write, subscribe, administer — not by the
order things were discovered.

## Provenance

**Every endpoint and every frame documented here was observed on the wire.**
Nothing is copied from GroupMe's published docs and nothing is added from
general knowledge of the API. The source is a proxied capture of
`web.groupme.com` (client version `GroupMeWeb/1.2.3`) taken 2026-07-29 —
`tools/capture-out/traffic.jsonl`, 2,378 exchanges across 365 distinct
endpoints, plus `tools/capture-out/websocket.jsonl`, 654 Bayeux frames read
through the Chrome DevTools Protocol. See [§14](#14-reproducing-this-capture).

Anything that is *not* a direct observation is labelled **inferred** at the
point it appears. If a claim carries no such label, a request and its response
are in the capture.

Where this contradicts [dev.groupme.com](https://dev.groupme.com/docs/v3),
believe this document. Several things below appear nowhere in the official
docs: the entire `/v4` surface, `/v3/pinned/*`, `/v3/poll/*`,
`/v3/conversations/{id}/events/*`, subgroups, relationships, blocks, the
`omit=` and `include=` parameters, the Bayeux channel naming, and the fact that
attachment URLs now redirect to expiring signed CDN links.

## Identifiers in this document

The capture contains a live account's data. **Every identifier, name, avatar
hash, token and message body in this document is synthetic**, of the same shape
as the real thing:

| Kind | Placeholder used |
|---|---|
| User id | `20000001`, `20000002`, `20000003` |
| Group id | `10000001`, `10000002` |
| Message id | `170000000000000001` … |
| Membership id | `1000000001` |
| DM conversation id | `20000001+20000002` |
| Access token | `<40-char token>` |

GroupMe user ids are stable and correlatable across every group an account
appears in, so a published `name → id` pair deanonymises that account
everywhere. Do not paste real ones into this file.

---

## 1. Quick reference

### 1.1 Hosts

| Host | Role |
|---|---|
| `api.groupme.com` | Main API. `/v1`, `/v3`, `/v4` all live here. |
| `v2.groupme.com` | Sign-in, user lookup, profile writes, mute/leave. |
| `push.groupme.com` | Faye realtime (Bayeux) — [§6](#6-realtime-faye--bayeux). |
| `image.groupme.com` | Avatar upload and QR rendering. |
| `i.groupme.com` | Avatars, unsigned and stable. |
| `m.groupme.com` | Attachment upload negotiation, and render URLs that **redirect**. |
| `cdn2.groupme.com` | Where attachment bytes live, behind Azure SAS signatures. |
| `file.groupme.com` | File-attachment metadata. |

### 1.2 Every endpoint observed

Status is what was actually returned. Every row links to its detail section.

**Authentication and session** — [§2](#2-authentication)

| Method | Path | Status | Detail |
|---|---|---|---|
| `POST` | `v2.groupme.com/access_tokens` | 202 | [§2.2](#22-sign-in) |
| `POST` | `/v3/verifications/{challenge}/initiate` | 200 | [§2.2](#22-sign-in) |
| `POST` | `/v3/verifications/{challenge}/confirm` | 200 | [§2.2](#22-sign-in) |
| `POST` | `v2.groupme.com/access_tokens` *(with `verification`)* | 201 | [§2.2](#22-sign-in) |
| `POST` | `/v3/web_pings` | 201 | [§2.4](#24-presence-keepalive) |
| `POST` | `/v3/web_pings/destroy` | 200 | [§2.4](#24-presence-keepalive) |

**Reading** — [§4](#4-reading)

| Method | Path | Status | Detail |
|---|---|---|---|
| `GET` | `/v3/users/me` | 200 | [§4.1](#41-identity-and-contacts) |
| `GET` | `v2.groupme.com/users/{user_id}` | 200 | [§4.1](#41-identity-and-contacts) |
| `GET` | `/v4/relationships` | 200 | [§4.1](#41-identity-and-contacts) |
| `GET` | `/v3/directories` | 200 | [§4.1](#41-identity-and-contacts) |
| `GET` | `/v3/groups` | 200 | [§4.2](#42-conversation-lists) |
| `GET` | `/v3/groups/{group_id}` | 200, 404 | [§4.2](#42-conversation-lists) |
| `GET` | `/v3/chats` | 200 | [§4.2](#42-conversation-lists) |
| `GET` | `/v3/chats/{conversation_id}` | 200 | [§4.2](#42-conversation-lists) |
| `GET` | `/v4/pinned_conversations` | 200 | [§4.2](#42-conversation-lists) |
| `GET` | `/v3/groups/{group_id}/messages` | 200, 404 | [§4.3](#43-message-history) |
| `GET` | `/v3/direct_messages` | 200 | [§4.3](#43-message-history) |
| `GET` | `/v3/pinned/groups/{group_id}/messages` | 200, 404 | [§4.3](#43-message-history) |
| `GET` | `/v3/pinned/direct_messages` | 200 | [§4.3](#43-message-history) |
| `GET` | `/v4/read_receipts` | 200 | [§4.5](#45-read-state) |

**Writing** — [§5](#5-writing)

| Method | Path | Status | Detail |
|---|---|---|---|
| `POST` | `/v3/groups/{group_id}/messages` | **201** | [§5.1](#51-send) |
| `POST` | `/v3/direct_messages` | **201** | [§5.1](#51-send) |
| `PUT` | `/v4/groups/{group_id}/messages/{message_id}` | 200 | [§5.2](#52-edit) |
| `DELETE` | `/v3/conversations/{conversation_id}/messages/{message_id}` | **204** | [§5.3](#53-delete) |
| `POST` | `/v3/conversations/{conversation_id}/messages/{message_id}/pin` | 200 | [§5.4](#54-pin-and-unpin) |
| `POST` | `/v3/conversations/{conversation_id}/messages/{message_id}/unpin` | 200 | [§5.4](#54-pin-and-unpin) |
| `POST` | `/v3/messages/{conversation_id}/{message_id}/like` | 200 | [§5.5](#55-reactions) |
| `POST` | `/v3/messages/{conversation_id}/{message_id}/unlike` | 200 | [§5.5](#55-reactions) |
| `POST` | `/v4/read_receipts/{conversation_id}` | **202** | [§5.6](#56-read-receipts) |
| `POST` | `/v3/conversations/{conversation_id}/read_receipt` | 200 | [§5.6](#56-read-receipts) |
| `POST` | `/v4/conversations/mark_all_read` | **202** | [§5.6](#56-read-receipts) |
| `POST` | `m.groupme.com/uploads` | 200 | [§5.7](#57-uploads) |
| `PUT` | `cdn2.groupme.com/uploads/{hash}/original.{ext}` | **201** | [§5.7](#57-uploads) |
| `POST` | `image.groupme.com/pictures` | 200 | [§5.7](#57-uploads) |
| `POST` | `file.groupme.com/v1/{conversation_id}/fileData` | 200 | [§5.7](#57-uploads) |

**Membership and administration** — [§7](#7-membership-and-group-administration)

| Method | Path | Status | Detail |
|---|---|---|---|
| `GET` | `/v3/groups/{group_id}/members` | 200 | [§7.1](#71-reading-membership) |
| `GET` | `/v3/groups/{group_id}/pending_memberships` | 200 | [§7.1](#71-reading-membership) |
| `GET` | `/v3/groups/{group_id}/subgroups` | 200 | [§7.1](#71-reading-membership) |
| `POST` | `/v3/groups/{group_id}/memberships/update` | 200 | [§7.2](#72-changing-a-membership) |
| `POST` | `/v3/groups/{group_id}/members/{membership_id}/update` | 200 | [§7.2](#72-changing-a-membership) |
| `POST` | `v2.groupme.com/groups/{group_id}/memberships/mute` | 200 | [§7.3](#73-mute-unmute-and-leave) |
| `POST` | `v2.groupme.com/groups/{group_id}/memberships/unmute` | 200 | [§7.3](#73-mute-unmute-and-leave) |
| `POST` | `v2.groupme.com/groups/{group_id}/memberships/{membership_id}/destroy` | 200 | [§7.3](#73-mute-unmute-and-leave) |
| `POST` | `/v3/groups/{group_id}/update` | 200 | [§7.4](#74-changing-a-group) |
| `POST` | `/v3/blocks` | **201** | [§7.5](#75-blocking) |
| `DELETE` | `/v3/blocks` | 200 | [§7.5](#75-blocking) |

**Ancillary surfaces** — [§8](#8-ancillary-surfaces)

| Method | Path | Status | Detail |
|---|---|---|---|
| `GET` | `/v3/conversations/{conversation_id}/events/list` | 200 | [§8.1](#81-calendar-events) |
| `GET` | `/v3/conversations/{conversation_id}/events/show` | 200, 404 | [§8.1](#81-calendar-events) |
| `POST` | `/v3/conversations/{conversation_id}/events/create` | **201** | [§8.1](#81-calendar-events) |
| `DELETE` | `/v3/conversations/{conversation_id}/events/delete` | 200 | [§8.1](#81-calendar-events) |
| `POST` | `/v3/conversations/{conversation_id}/events/rsvp` | 200 | [§8.1](#81-calendar-events) |
| `DELETE` | `/v3/conversations/{conversation_id}/events/rsvp/delete` | 200 | [§8.1](#81-calendar-events) |
| `GET` | `/v3/poll/{conversation_id}` | 200, 401 | [§8.2](#82-polls) |
| `POST` | `/v3/poll/{conversation_id}` | **201** | [§8.2](#82-polls) |
| `GET` | `/v3/poll/{conversation_id}/{poll_id}` | 200 | [§8.2](#82-polls) |
| `POST` | `/v3/poll/{conversation_id}/{poll_id}/{option_id}` | 200 | [§8.2](#82-polls) |
| `GET` | `/v1/urls/preview` | 200, 403, 404, 415 | [§8.3](#83-url-preview) |
| `POST` | `v2.groupme.com/users/{user_id}` | 200 | [§8.4](#84-profile-update) |
| `GET` | `image.groupme.com/qr/join_group/{group_id}/{share_token}/preview` | 200 | [§8.5](#85-qr-rendering) |
| `GET` | `image.groupme.com/qr/contact/{user_id}/{share_token}/preview` | 200 | [§8.5](#85-qr-rendering) |

**Media** — [§10](#10-media-and-why-urls-are-not-archivable)

| Method | Path | Status | Detail |
|---|---|---|---|
| `GET` | `m.groupme.com/uploads/{hash}/{w}x{h}.original.{ext}` | **301** | [§10](#10-media-and-why-urls-are-not-archivable) |
| `GET` | `cdn2.groupme.com/uploads/{hash}/original.{ext}` | 200 | [§10](#10-media-and-why-urls-are-not-archivable) |
| `GET` | `i.groupme.com/{w}x{h}.{ext}.{hash}[.avatar]` | 200, 403 | [§10](#10-media-and-why-urls-are-not-archivable) |

**Realtime transport** — [§6](#6-realtime-faye--bayeux)

| Method | Path | Status | Detail |
|---|---|---|---|
| `GET` | `push.groupme.com/faye?message=…&jsonp=…` | 200 | [§6.1](#61-transport-and-handshake) |
| `GET` | `wss://push.groupme.com/faye` *(upgrade)* | **101** | [§6.1](#61-transport-and-handshake) |
| `POST` | `push.groupme.com/faye` *(long-poll fallback)* | **504** | [§6.1](#61-transport-and-handshake) |

### 1.3 Every realtime frame observed

`→` is client-to-server, `←` is server-to-client. Detail in
[§6.4](#64-frame-types).

| Channel | Dir | `data.type` | Carries |
|---|---|---|---|
| `/meta/handshake` | `→ ←` | — | Session bootstrap, yields `clientId` |
| `/meta/connect` | `→ ←` | — | Long-poll hold / keepalive |
| `/meta/subscribe` | `→ ←` | — | Channel join, carries `ext.access_token` |
| `/meta/unsubscribe` | `→ ←` | — | Channel leave |
| `/user/{user_id}` | `←` | `line.create` | A group message, or a group system message |
| `/user/{user_id}` | `←` | `direct_message.create` | A DM, or a DM system message |
| `/user/{user_id}` | `←` | `like.create` | A reaction added to a message |
| `/group/{group_id}` | `→ ←` | `typing` | Typing indicator |
| `/direct_message/{a}_{b}` | `→ ←` | `typing` | Typing indicator |
| `/direct_message/{a}_{b}` | `←` | `read_receipt.create` | The other party read up to a message |

The `event.type` values that appear *inside* `line.create` and
`direct_message.create` payloads are enumerated in
[§6.5](#65-system-event-types).

---

## 2. Authentication

### 2.1 Request headers

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
token. There is no `Authorization` header and no signature. Every
`api.groupme.com` and `v2.groupme.com` request in the capture carried it in
this header.

`x-requested-with: GroupMeWeb/<version>` is undocumented and not required, but
sending it makes an unofficial client's traffic indistinguishable from the web
client's. This app sends it.

Two places take the token *outside* the header, and both are real:

- `POST /v3/web_pings/destroy?token=<40-char token>` — query parameter
  ([§2.4](#24-presence-keepalive)).
- Bayeux `/meta/subscribe` and any client publish — `ext.access_token` inside
  the message body ([§6.3](#63-subscribing)).

Anything that scans only request headers for credentials will miss both.

### 2.2 Sign-in

Sign-in is a **four-call** challenge, not OAuth. The first and last calls hit
the same path on `v2.groupme.com` and are distinguished by whether the body
carries a `verification` object.

**Call 1 — credentials.** Returns `202` and a verification challenge, *not* a
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
`methods` values are the last two digits of the destination, for display only.

**Call 2 — request the code.** `{challenge}` is `verification.code` above.
Note this one is on `api.groupme.com`, not `v2`:

```http
POST https://api.groupme.com/v3/verifications/{challenge}/initiate
{ "verification": { "method": "sms" } }
```
```json
{ "meta": {"code": 200},
  "response": { "hint": "00",
                "code_format": {"min_length":4,"max_length":4,"charset":"numeric"} } }
```

**Call 3 — confirm the PIN.**

```http
POST https://api.groupme.com/v3/verifications/{challenge}/confirm
{ "verification": { "pin": "0000" } }
```
```json
{ "meta": {"code": 200}, "response": { "status": 20000 } }
```

**Call 4 — exchange the challenge for the token.** The *same* body as call 1,
plus the now-confirmed `verification.code`. This is what returns the token, and
it answers `201`, not `202`:

```http
POST https://v2.groupme.com/access_tokens
content-type: application/json

{
  "username": "5555550100",
  "password": "<password>",
  "grant_type": "password",
  "app_id": "groupme-web",
  "device_id": "<uuid v4, same value as call 1>",
  "verification": { "code": "<64-char hex challenge id>" }
}
```
```json
{
  "meta": { "code": 201 },
  "response": {
    "access_token": "<40-char token>",
    "user_id": "20000001",
    "user_name": "Example User",
    "expires_at": 1786511863,
    "user": { "id": "20000001", "name": "Example User",
              "email": "user@example.invalid",
              "avatar_url": null, "admin": false }
  }
}
```

Calls 1–4 carry **no** `x-access-token`. The `device_id` must be the same uuid
across calls 1 and 4.

> `expires_at` is a Unix-seconds absolute expiry, roughly two weeks out in the
> observed response. No refresh call appeared in the capture and none was
> attempted, so what a client is supposed to do at expiry is **inferred**: the
> only observed path back to a token is repeating the whole four-call sign-in.

### 2.3 Handling the token

A token can read every DM and post as the user. Treat it as equivalent to the
password. It is revocable at
[dev.groupme.com/applications](https://dev.groupme.com/applications).

This app keeps the token in **Windows Credential Manager**, never in the SQLite
archive or a config file. The archive stores only a SHA-256 fingerprint, used
to notice that a different account signed in.

> This app does not implement sign-in. The user signs in to the real
> `web.groupme.com` inside the webview, and the app lifts the resulting
> `x-access-token` off an outgoing request header. Reimplementing a password
> and 2FA flow to obtain a credential the webview already holds would add risk
> and no capability.

### 2.4 Presence keepalive

The web client marks itself online with a periodic ping, and clears it on
teardown.

```http
POST /v3/web_pings
content-type: application/json

{ "ttl": 1200 }
```

Returns **`201`** with `Content-Type: text/html` and a zero-length body — no
JSON envelope. `ttl` is seconds; the client re-pings well inside it.

```http
POST /v3/web_pings/destroy?token=<40-char token>
content-type: application/json

{}
```

Returns `200`, again `text/html` and empty. This is the one endpoint observed
that takes the access token as a **query parameter** rather than a header, and
it still sends `x-access-token` as well. Neither call is needed to read or
write; they only drive the presence dot.

---

## 3. Response envelope and errors

Almost everything is wrapped:

```json
{ "meta": { "code": 200 }, "response": { } }
```

`response` is sometimes `null` and sometimes `[]` — both mean "nothing here."
Handle both without erroring; on message routes this is the backfill
terminator, so getting it wrong means an infinite loop or a truncated archive.

### Errors

An error response drops `response` entirely and adds `meta.errors`:

```json
{ "meta": { "code": 404, "errors": ["not found"] } }
{ "meta": { "code": 40100, "errors": ["Not authorized"] } }
```

`meta.errors` is an array of plain strings, not structured codes.

### Endpoints that escape the envelope

| Endpoint | What comes back instead |
|---|---|
| `DELETE /v3/conversations/{id}/messages/{id}` | `204`, zero-length body ([§5.3](#53-delete)) |
| `DELETE /v3/blocks` | `200`, zero-length body ([§7.5](#75-blocking)) |
| `POST /v3/web_pings`, `/v3/web_pings/destroy` | `text/html`, zero-length body ([§2.4](#24-presence-keepalive)) |
| `POST /v4/conversations/mark_all_read` | `202`, zero-length body ([§5.6](#56-read-receipts)) |
| `GET /v1/urls/preview` | Open Graph document whose top-level `meta` is *not* a status block ([§8.3](#83-url-preview)) |
| `POST file.groupme.com/v1/{id}/fileData` | A bare JSON **array** ([§5.7](#57-uploads)) |
| `POST m.groupme.com/uploads` | A bare JSON object, no envelope ([§5.7](#57-uploads)) |
| `GET /v3/groups/{id}/messages` on 404 | Zero-length body |
| `GET /v3/pinned/groups/{id}/messages` on 404 | Zero-length body |
| `DELETE /v3/conversations/undefined/messages/{id}` | `404`, `text/html`, the literal string `Not Found` |

Code that decodes JSON unconditionally throws on all of these.

> The last row is the web client's own bug, captured three times: it issued a
> delete with the string `undefined` where the conversation id belongs before
> retrying with the correct id. It is listed because a client that mirrors the
> web client's request sequence can reproduce it.

See [§11.3](#113-status-codes-and-envelopes) for the full status-code table.

---

## 4. Reading

### 4.1 Identity and contacts

| Method | Path | Query | Status | Notes |
|---|---|---|---|---|
| `GET` | `/v3/users/me` | — | 200 | The signed-in account's full profile |
| `GET` | `v2.groupme.com/users/{user_id}` | `include_shared_groups` | 200 | Another user's public profile |
| `GET` | `/v4/relationships` | `include_blocked`, `since` | 200 | Contact list, delta-syncable |
| `GET` | `/v3/directories` | — | 200 | Returned `[]` on every call |

**`GET /v3/users/me`** returns everything the account knows about itself:

```json
{
  "id": "20000001", "user_id": "20000001",
  "name": "Example User", "email": "user@example.invalid",
  "email_verified": false, "phone_number": "+1 5555550100", "sms": true,
  "bio": "…", "image_url": null, "locale": "en_us", "zip_code": null,
  "created_at": 1727184917, "updated_at": 1763942973,
  "friend_suggestable": true,
  "facebook_connected": false, "microsoft_connected": false,
  "twitter_connected": false,
  "share_url": "https://groupme.com/contact/20000001/XXXXXXXX",
  "share_qr_code_url": "https://image.groupme.com/qr/contact/20000001/XXXXXXXX/preview",
  "mfa": { "enabled": false,
           "channels": [{ "type": "phone_number", "created_at": 1751208166 }] },
  "tags": ["phone-us"],
  "prompt_for_survey": false, "show_age_gate": false, "birth_date_set": true,
  "graduation_year": "", "campus_profile_visibility": "visible",
  "consents": ["copilot_character_group_v1", "copilot_character_dm_v1"]
}
```

The trailing 8 characters of `share_url` are a per-account share token, reused
by the QR endpoint ([§8.5](#85-qr-rendering)).

**`GET v2.groupme.com/users/{user_id}?include_shared_groups=true`** wraps the
subject under `user` and adds a sibling `relationship`:

```json
{ "user": { "id": "20000002", "name": "Example Contact",
            "avatar_url": null, "bio": "…",
            "created_at": 1727184917,
            "app_installed": true, "direct_message_capable": true,
            "directories": [{ "id": 0, "name": "", "short_name": "" }] },
  "relationship": null,
  "graduation_year": "", "campus_profile_visibility": "visible" }
```

`relationship` was `null` throughout; a populated shape was not captured.
Despite the query parameter's name, no shared-groups array appeared in the
response.

**`GET /v4/relationships?include_blocked=true&since=<iso8601>`** is the contact
list, and the only delta-sync endpoint in the API. `since` is a full ISO-8601
timestamp with microseconds (`2025-01-19T19:38:00.084302Z`) — feed back the
largest `updated_at_iso8601` seen. The response is a bare array under
`response`:

```json
{
  "id": "20000002", "user_id": "20000002", "name": "Example Contact",
  "avatar_url": "https://i.groupme.com/…",
  "created_at": 1727815111, "created_at_iso8601": "2024-10-01T20:38:31.690372Z",
  "updated_at": 1727815111, "updated_at_iso8601": "2024-10-01T20:38:31.690372Z",
  "reason": 1, "hidden": false, "app_installed": true, "is_blocked": false
}
```

`id` equals `user_id` on every entry observed. `reason` was `1` throughout; the
enumeration is unknown.

### 4.2 Conversation lists

| Method | Path | Query | Status | Notes |
|---|---|---|---|---|
| `GET` | `/v3/groups` | `per_page`, `page`, `omit` | 200 | Array of groups |
| `GET` | `/v3/groups/{group_id}` | `include` | 200, 404 | One group; `include=members` restores memberships |
| `GET` | `/v3/chats` | `per_page`, `page` | 200 | Array of DM threads |
| `GET` | `/v3/chats/{conversation_id}` | `include` | 200 | One DM thread; observed with `include=read_receipts` |
| `GET` | `/v4/pinned_conversations` | — | 200 | `{ "pinned_conversation_ids": ["10000001", …] }` |

Payload shapes: [group](#group), [chat](#chat-dm-thread).

`/v3/chats/{conversation_id}` takes the `+`-joined DM key, URL-encoded or not —
both `…/chats/20000001+20000002` and the `%2B` form were accepted. The response
is a single chat object, identical in shape to one element of `/v3/chats`.

> `/v3/groups` and `/v3/chats` take **`per_page`**, while message endpoints take
> **`limit`**. This inconsistency is real; don't normalize it away.

> **`omit=memberships` is the single biggest performance lever here.** A member
> array on a 5,000-member group dwarfs the rest of the payload. The web client
> lists groups with `per_page=100&omit=memberships&page=N`, then fetches
> `/v3/groups/{id}?include=members` only for the group actually opened. This app
> does the same.
>
> The cost is that `members` comes back **explicitly `null`** — 900 of 900 group
> objects in the capture — and `members_count` comes back `0`. See
> [§11.1](#111-serialization).

`/v4/pinned_conversations` mixes both id kinds in one array: group ids and
`+`-joined DM keys, undifferentiated.

### 4.3 Message history

| Method | Path | Query | Status | Response key |
|---|---|---|---|---|
| `GET` | `/v3/groups/{group_id}/messages` | `limit`, `before_id`, `acceptFiles` | 200, 404 | `messages` |
| `GET` | `/v3/direct_messages` | `other_user_id`, `limit`, `acceptFiles` | 200 | `direct_messages` |
| `GET` | `/v3/pinned/groups/{group_id}/messages` | — | 200, 404 | `messages` |
| `GET` | `/v3/pinned/direct_messages` | `other_user_id` | 200 | `direct_messages` |

A DM thread is addressed by the **other participant's user id**, not by the
`+`-joined key: `GET /v3/direct_messages?other_user_id=20000002`. The
`+`-joined key is what the *response* carries, as `conversation_id` on each
message and `chat_id` on the sibling `read_receipt`.

```json
{ "count": 4551,
  "messages": [ /* newest first */ ] }
```

`acceptFiles=1` accompanies every history fetch the web client makes. Messages
carrying a `file` attachment were returned with it set; whether they are
suppressed without it was not tested — **inferred** that it opts into file
attachments. Harmless, and this app mirrors it.

`GET /v3/direct_messages` returns a third key alongside `count` and
`direct_messages` — the other party's read position in this thread:

```json
{ "id": "", "chat_id": "20000001+20000002",
  "message_id": "170000000000000001", "user_id": "20000002",
  "read_at": 1784752304 }
```

`id` is the empty string here, not a real id.

**404s are normal.** `GET …/messages` and `GET /v3/pinned/groups/{id}/messages`
both returned `404` with a **zero-length body** for groups the account is no
longer a member of. Treat 404 on a message route as "conversation gone", not as
a transport error, and do not try to parse the body.

**Pinned lists are the source of truth for pin state.** `count` is the number of
pinned messages (`0`, `1` and `2` observed); each entry is a normal message
object with `pinned_at` and `pinned_by` populated. See
[§5.4](#54-pin-and-unpin) for why the event stream cannot substitute.

### 4.4 Pagination and cursors

#### Messages

| Param | Returns | Order | Status |
|---|---|---|---|
| `before_id` | Strictly older than the given id | **newest-first** | observed |
| `limit` | Page size, max **100** | — | observed |
| `after_id` | Newer than the given id | oldest-first | **inferred**, not captured |
| `since_id` | Newer than the given id | newest-first | **inferred**, not captured |

Only `before_id` appeared in this capture: the web client pages backwards on
scroll and receives new messages over the Faye socket rather than polling
forward. `after_id` and `since_id` are carried from prior observation of this
API and were **not** exercised here — treat their ordering as unknown.

The confirmed backfill chain, observed while scrolling back through a group of
4,551 messages:

```
GET /v3/groups/{id}/messages?acceptFiles=1&limit=100
    -> messages[0]  = 170000000000000110   (newest)
       messages[99] = 170000000000000011   (oldest)

GET /v3/groups/{id}/messages?acceptFiles=1&limit=100&before_id=170000000000000011
    -> messages[0]  = 170000000000000010
       messages[99] = 170000000000000001

GET /v3/groups/{id}/messages?acceptFiles=1&limit=100&before_id=170000000000000001
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
and treating one as the end silently truncates the archive. Some conversations
legitimately return fewer than `limit` on the very first call (`count: 1` and
`count: 3` were observed returning 1 and 3 messages).

`response.count` is the conversation's **lifetime total**, not the page size,
and stays constant across pages. That makes it a free completeness check: when
local rows reach `count`, the backfill is done — useful because it catches a
stalled backfill that an empty-page check alone would call finished.

#### Groups and chats

`per_page=100&page=N`, incrementing until a short page. Both endpoints were
observed paging to `page=12`.

#### Polls

`GET /v3/poll/{conversation_id}` returns a `continuation_token` beside `polls`.
It was non-null even on an empty `polls` array, and was never fed back, so its
semantics are **inferred**, not observed.

### 4.5 Read state

| Method | Path | Status | Body |
|---|---|---|---|
| `GET` | `/v4/read_receipts` | 200 | `{ "receipts": [{ "conversation_id", "last_read_message_id" }] }` |

The whole read-state map arrives in one call — 216 entries in the observed
response — keyed by conversation, mixing group ids and `+`-joined DM keys:

```json
{ "receipts": [
    { "conversation_id": "10000001",
      "last_read_message_id": "170000000000000001" },
    { "conversation_id": "20000001+20000002",
      "last_read_message_id": "170000000000000002" } ] }
```

There is no per-conversation read GET. `unread_count`, `last_read_message_id`
and `last_read_at` on the group and chat objects were `null` throughout, so
this endpoint is the only read-state source. Writing read state is
[§5.6](#56-read-receipts).

## 5. Writing

> This app never calls anything in this section. Sending is left entirely to the
> real `web.groupme.com` in the webview, and the archiver is read-only against
> the API — which is what makes "read-only when offline" structural rather than
> a UI check. The section exists because the protocol reference should be
> complete, and because a client built on it will need these.

**There is no single message resource path.** Five different shapes govern one
object:

| Operation | Path shape |
|---|---|
| Send to a group | `/v3/groups/{group_id}/messages` |
| Send a DM | `/v3/direct_messages` |
| Edit | **`/v4`**`/groups/{group_id}/messages/{message_id}` |
| Delete, pin, unpin | `/v3/conversations/{conversation_id}/messages/{message_id}[/pin]` |
| React | `/v3/messages/{conversation_id}/{message_id}/like` |
| Read pins back | `/v3/pinned/groups/{group_id}/messages` |

Nothing generalises. Route each verb individually. See
[§11.4](#114-path-versions-and-shapes).

### 5.1 Send

Groups and DMs use different paths, different request wrappers and different
response keys. Both answer **`201`**.

```http
POST /v3/groups/{group_id}/messages
content-type: application/json

{
  "message": {
    "source_guid": "<client-generated uuid v4>",
    "text": "Example reply",
    "attachments": [
      { "type": "reply", "reply_id": "170000000000000001",
        "base_reply_id": "170000000000000001", "user_id": "20000002" }
    ]
  }
}
```
```json
{ "meta": { "code": 201 },
  "response": { "message": {
    "id": "170000000000000003",
    "source_guid": "<echoed back>",
    "created_at": 1785302330,
    "user_id": "20000001", "group_id": "10000001",
    "name": "Example Sender", "avatar_url": null,
    "text": "Example reply", "system": false,
    "attachments": [ /* echoed */ ],
    "favorited_by": [],
    "sender_type": "user", "sender_id": "20000001"
  } } }
```

```http
POST /v3/direct_messages
content-type: application/json

{
  "direct_message": {
    "recipient_id": "20000002",
    "source_guid": "<client-generated uuid v4>",
    "text": "Example DM",
    "attachments": []
  }
}
```
```json
{ "meta": { "code": 201 },
  "response": { "direct_message": {
    "id": "170000000000000004",
    "source_guid": "<echoed back>",
    "recipient_id": "20000002",
    "created_at": 1785347247,
    "user_id": "20000001", "sender_id": "20000001", "sender_type": "user",
    "name": "Example Sender", "avatar_url": "",
    "text": "Example DM", "attachments": [], "favorited_by": []
  } } }
```

| | Group | DM |
|---|---|---|
| Path | `/v3/groups/{group_id}/messages` | `/v3/direct_messages` |
| Addressed by | group id in the path | `recipient_id` in the body |
| Request wrapper | `message` | `direct_message` |
| Response key | `message` | `direct_message` |

`source_guid` is a client-generated uuid v4, echoed back verbatim. It is the
idempotency handle for matching an optimistic local row to the server's. The
create response carries **no** `updated_at`, no `platform`, and no
`pinned_at`/`pinned_by` — those appear only on a subsequent read.

### 5.2 Edit

```http
PUT /v4/groups/{group_id}/messages/{message_id}
content-type: application/json

{ "text": "Example edited body", "attachments": [] }
```
```json
{ "meta": { "code": 200 },
  "response": { "message": {
    "id": "170000000000000001",
    "source_guid": "<uuid v4>",
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
`"message"` the way send wraps its payload — and it carries no `source_guid`.
`attachments: []` was sent explicitly; whether omitting the key preserves
existing attachments or clears them was not tested.

The response message carries `updated_at`, which the create response does not.
`message_edit_period` on the group ([§9](#group)) bounds how long an edit is
accepted — 15 seconds on most conversations, 43,200 on one; no expired edit was
attempted, so the rejection status is unknown.

**The DM edit route was not captured.** DM *edits* were observed arriving over
the websocket ([§6.4](#64-frame-types)), so the capability exists, but this
account never issued one and no `/v4/chats/…` or `/v4/direct_messages/…` path
appeared. Reusing `/v4/groups/{conversation_id}/…` with a `+`-joined key is a
guess and is **inferred**, not observed.

The `/v4` route answers with
`access-control-allow-methods: POST, GET, PUT, PATCH, DELETE, OPTIONS`, while
the `/v3` routes omit `PATCH`. A partial `PATCH` on the same path is therefore
plausible and **inferred**.

### 5.3 Delete

```http
DELETE /v3/conversations/{conversation_id}/messages/{message_id} HTTP/1.1
Host: api.groupme.com
x-access-token: <40-char token>
x-requested-with: GroupMeWeb/1.2.3
Accept: */*
```

No request body and no `content-type`. Returns **`204 No Content`** with a
zero-length body and no envelope. Code that decodes JSON unconditionally on the
message routes throws here.

`{conversation_id}` is the **group id** for a group and the **`+`-joined DM
key** for a DM — both were observed returning `204`:

```
DELETE /v3/conversations/10000001/messages/170000000000000001            -> 204
DELETE /v3/conversations/20000001+20000002/messages/170000000000000002   -> 204
```

The target need not be the caller's own message: one observed call deleted
another member's message, as a group admin. `message_deletion_mode` on the
group ([§9](#group)) — observed as `["admin", "sender"]` on groups and
`["sender"]` on DMs — governs who may call this.

### 5.4 Pin and unpin

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
  "source_guid": "<uuid v4>", "text": "Example edited body",
  "created_at": 1785303377, "updated_at": 1785303382,
  "system": false, "platform": "gm",
  "attachments": [], "favorited_by": [],
  "pinned_at": 1785303387, "pinned_by": "20000001" }
```

A conversation holds more than one pin — `count: 2` was observed on that list.
`pinned_by` is a user id as a **string**, unlike the empty-string default on an
unpinned message.

> **Unpin emits nothing.** No `message.unpinned` event and no `message.pinned`
> with `pinned: false` followed the unpin, over HTTP or over the socket. The only
> signal is `pinned_at` and `pinned_by` reverting to `null` and `""` on the
> message, and the row dropping out of `/v3/pinned/…`. A client that tracks pins
> from the event stream alone displays a stale pin forever; the pinned list is
> the source of truth.

### 5.5 Reactions

| Method | Path | Status |
|---|---|---|
| `POST` | `/v3/messages/{conversation_id}/{message_id}/like` | 200 |
| `POST` | `/v3/messages/{conversation_id}/{message_id}/unlike` | 200 |

The path takes **both** the conversation id and the message id, and lives under
`/v3/messages/`, not under the group or the chat.

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

`unlike` takes the **same body** — the emoji being removed — and returns the
remaining reactions, `[]` when the last one is gone.

The response returns the message's **full** reaction list, not a delta, so it
can be written straight over the stored value rather than merged.

> `pack_id`/`pack_index` come back as **integers** (`0`) here, and are **absent
> entirely** from the `reactions` array on a message read. Parse permissively.

Three names, one concept: `like` is the endpoint, `reactions` is the response
field and the field on a message read, `favorited_by` is the flattened list of
user ids on a message read. An artefact of the feature growing from a like
button into arbitrary emoji reactions. A user id appears in `favorited_by` once
per reaction they left, so **deduplicate before counting reactors**.

### 5.6 Read receipts

Three separate endpoints write read state, on two API versions.

| Method | Path | Status | Body |
|---|---|---|---|
| `POST` | `/v4/read_receipts/{conversation_id}` | **202** | `{ "last_read_message_id": "…" }` |
| `POST` | `/v3/conversations/{conversation_id}/read_receipt` | 200 | none |
| `POST` | `/v4/conversations/mark_all_read` | **202** | none |

The web client sends the `/v4` write and the `/v3` write **back to back** on the
same conversation. Both are real and both succeed; the pair was observed five
times.

```http
POST /v4/read_receipts/20000001%2B20000002
content-type: application/json

{ "last_read_message_id": "170000000000000001" }
```
```json
{ "meta": { "code": 202 },
  "response": { "conversation_id": "20000001+20000002",
                "last_read_message_id": "170000000000000001",
                "status": "accepted" } }
```

`202` is correct here — the write is accepted asynchronously. The path segment
may be sent percent-encoded (`%2B`) or raw (`+`); both were observed against the
same conversation and both returned `202`.

```http
POST /v3/conversations/20000001+20000002/read_receipt
```
```json
{ "meta": { "code": 200 },
  "response": { "read_receipt": {
    "conversation_id": "20000001+20000002",
    "message_id": "170000000000000001",
    "user_id": "20000001",
    "read_at": 1785347234 } } }
```

No request body — the server marks the conversation read to its own latest
message and echoes the resulting receipt, including `read_at`. This is the form
that produces the `read_receipt.create` frame the other party sees
([§6.4](#64-frame-types)).

```http
POST /v4/conversations/mark_all_read
```

Returns **`202` with a zero-length body**. No envelope, no list of what was
marked.

### 5.7 Uploads

Three different upload surfaces, on three different hosts, none of them on
`api.groupme.com`.

**Message images — two calls, `m.groupme.com` then `cdn2.groupme.com`.** The
first call negotiates a slot and hands back a pre-signed Azure Blob URL; the
second `PUT`s the bytes straight to the CDN.

```http
POST https://m.groupme.com/uploads
content-type: application/json

{ "extension": "jpg", "senderId": "20000001", "groupId": "10000001",
  "fileSize": 150066, "width": 1440, "height": 1687 }
```
```json
{
  "uploadUrl":  "https://cdn2.groupme.com/uploads/{hash}/original.jpg?sv=…&sp=cw&sig=…",
  "renderUrl":  "https://m.groupme.com/uploads/{hash}/1440x1687.original.jpg",
  "thumbnailUrl": null,
  "transcriptUrl": null
}
```

No `meta`/`response` envelope — the object is the whole body. The client then:

```http
PUT <uploadUrl>
<raw image bytes>
```

which returns **`201`** with an empty body. `renderUrl` is what goes into the
message's `image` attachment `url`; it is a `301` to the read-signed CDN object
([§10](#10-media-and-why-urls-are-not-archivable)).

Note the SAS permission in the two URLs differs: `sp=cw` (create/write) on the
upload URL, `sp=r` on every read URL.

**Avatars — `image.groupme.com/pictures`.** A single call, raw bytes as the
request body with a matching `Content-Type`:

```http
POST https://image.groupme.com/pictures
content-type: image/jpeg

<raw image bytes>
```
```json
{ "payload": { "url": "https://i.groupme.com/1672x941.jpeg.{hash}",
               "picture_url": "https://i.groupme.com/1672x941.jpeg.{hash}" } }
```

Again no envelope; `payload` is the whole wrapper. `url` and `picture_url` were
identical. The result is an `i.groupme.com` URL — unsigned and permanent,
unlike message images.

**File attachments — `file.groupme.com`.** Only the *metadata* read was
captured:

```http
POST https://file.groupme.com/v1/{conversation_id}/fileData
content-type: application/json

{ "file_ids": ["<uuid v4>"] }
```
```json
[ { "file_id": "<uuid v4>", "meta": 200,
    "file_data": { "file_name": "README.md", "file_size": 11159,
                   "mime_type": "" } } ]
```

The response is a **bare JSON array**, and `meta` here is a per-element integer
status, not the usual envelope. `mime_type` was the empty string on both
observed files. The conversation id is `+`-joined for a DM.

**The call that produces a `file_id` was not captured.** A message carries
`{"type": "file", "file_id": "<uuid v4>"}`, and this endpoint resolves that id
to a name and size, but the upload itself never appeared. Do not guess it.

### 5.8 How mutations propagate

Delete, edit and pin each land **twice**: the original message row is rewritten
in place, **and** a new system message is appended to the conversation. Both are
visible on the next `GET …/messages`, and the system message is also pushed over
the socket ([§6.4](#64-frame-types)). Unpin is the exception — it emits nothing.

A deleted message keeps its id, sender, `created_at` and `source_guid`, and
gains a tombstone:

```json
{ "id": "170000000000000002", "group_id": "10000001",
  "user_id": "20000002", "sender_id": "20000002", "sender_type": "user",
  "name": "Example Member", "avatar_url": null,
  "source_guid": "<uuid v4>", "system": false,
  "text": "An admin deleted this message",
  "created_at": 1785303339,
  "deleted_at": 1785303362, "deletion_actor": "admin",
  "attachments": [], "favorited_by": [],
  "platform": "gm", "pinned_at": null, "pinned_by": "" }
```

`deletion_actor` was observed as `"admin"` (25 rows) and `"sender"` (81 rows),
and the substituted `text` tracks it — `"An admin deleted this message"` versus
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

The full `event.type` enumeration is in
[§6.5](#65-system-event-types) — the same values reach a client over HTTP
history and over the socket.

---

## 6. Realtime (Faye / Bayeux)

`push.groupme.com` runs a [Faye](https://faye.jcoglan.com/) (Bayeux) server.
The whole realtime protocol — new messages, edits, deletes, reactions, typing
indicators, read receipts — is multiplexed over **one** websocket.

> **These frames are invisible to an HTTP proxy.** Everything after the upgrade
> is inside the socket, so a MITM proxy sees a `101` and then nothing. This
> section was captured with the Chrome DevTools Protocol —
> `goog:loggingPrefs: {performance: ALL}` plus `Network.webSocketFrameSent` and
> `Network.webSocketFrameReceived`. 654 frames.
>
> Forcing Faye's long-poll fallback is **not** a workaround. Deleting
> `window.WebSocket` and `window.EventSource` before page scripts run makes Faye
> negotiate `cross-origin-long-polling`, which is plain HTTP and fully visible —
> but every such `POST /faye` returned **`504 Gateway Time-out`** from GroupMe's
> own edge, not from the proxy. Their client always upgrades, so that path is
> not really supported by their infrastructure.

### 6.1 Transport and handshake

The web client loads `web.groupme.com/js/faye.min.js` and bootstraps over
**JSONP**, then upgrades immediately.

```
GET https://push.groupme.com/faye
    ?message=[{"channel":"/meta/handshake","version":"1.0",
               "supportedConnectionTypes":["websocket","eventsource",
                 "long-polling","cross-origin-long-polling","callback-polling"],
               "id":"1"}]
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
  "clientId": "<31-char client id>",
  "advice": { "reconnect": "retry", "interval": 0, "timeout": 600000 }
}]);
```

The client lists `websocket` **first** in its own
`supportedConnectionTypes`, and a request to the same path then returns
**`101 Switching Protocols`**. The live socket is:

```
wss://push.groupme.com/faye
```

`advice.timeout: 600000` (10 minutes) with `reconnect: "retry"` and
`interval: 0` is a standard Bayeux hold.

`clientId` is a 31-character lowercase alphanumeric string. It changes on every
handshake, and every subsequent client-to-server frame must carry it. Nine
sockets were created across the capture session (page reloads); each got a fresh
`clientId`.

### 6.2 Connect

Immediately after the upgrade:

```json
{ "channel": "/meta/connect",
  "clientId": "<31-char client id>",
  "connectionType": "websocket",
  "id": "2" }
```
```json
{ "id": "2",
  "clientId": "<31-char client id>",
  "channel": "/meta/connect",
  "successful": true,
  "advice": { "reconnect": "retry", "interval": 0, "timeout": 600000 } }
```

`/meta/connect` carries **no** `ext.access_token` — only `/meta/subscribe` and
client publishes do.

### 6.3 Subscribing

```json
{ "channel": "/meta/subscribe",
  "clientId": "<31-char client id>",
  "subscription": "/user/20000001",
  "id": "3",
  "ext": { "access_token": "<40-char token>" } }
```
```json
{ "id": "3",
  "clientId": "<31-char client id>",
  "channel": "/meta/subscribe",
  "successful": true,
  "subscription": "/user/20000001" }
```

`/meta/unsubscribe` is the same shape **without** `ext` and is acknowledged
identically. Bayeux messages may be batched several per frame — a conversation
switch sends the unsubscribe and the subscribe in one array.

| Element | Value |
|---|---|
| Personal firehose | `/user/{user_id}` |
| Group channel | `/group/{group_id}` |
| DM channel | `/direct_message/{lower_user_id}_{higher_user_id}` |
| Auth | `ext.access_token` on `/meta/subscribe` and on every client publish |
| `clientId` | from the `/meta/handshake` response |
| `id` | per-message correlation id, a base-36 counter (`"2"`, `"3"`, … `"b"`, `"1s"`) |

> **The DM channel joins the two user ids with an underscore, while every HTTP
> path and every payload field joins them with a plus.** The same thread is
> `/direct_message/20000001_20000002` on the socket and `20000001+20000002`
> everywhere else. Both are ascending numeric order. Getting this wrong produces
> a subscribe that succeeds and then delivers nothing.

> **The token travels in the Bayeux message body, not a header.** There is no
> `x-access-token` on these frames. Anything reading only headers will conclude
> the socket is unauthenticated.

The client subscribes to `/user/{own_user_id}` once and keeps it for the life of
the socket; group and DM channels are subscribed and unsubscribed as the user
switches conversations. 25 distinct channels were subscribed in the session.

### 6.4 Frame types

Every inbound payload has the same envelope:

```json
{ "channel": "<channel>", "id": "<6-char hex>",
  "data": { "type": "<type>", "alert": "<notification text>",
            "subject": { }, "received_at": 1785346055000 } }
```

`received_at` is Unix **milliseconds**, unlike every timestamp in the HTTP API.
`alert` is the push-notification string, usually `"<name>: <text>"`.

#### `line.create` — a group message

Arrives on `/user/{user_id}`, **not** on `/group/{group_id}`. `subject` is a
group message object with a few extra keys:

```json
{
  "channel": "/user/20000001",
  "id": "<6-char hex>",
  "data": {
    "alert": "Example Sender: Example message body",
    "subject": {
      "id": "170000000000000001",
      "source_guid": "<uuid v4 or 32-char hex>",
      "group_id": "10000001",
      "user_id": "20000002", "sender_id": "20000002", "sender_type": "user",
      "name": "Example Sender", "avatar_url": null,
      "text": "Example message body",
      "created_at": 1785346055,
      "updated_at": null,
      "deleted_at": null, "deletion_actor": null,
      "system": false,
      "parent_id": null, "picture_url": null,
      "pinned_at": null, "pinned_by": null,
      "location": { "lat": "", "lng": "", "name": null },
      "attachments": []
    },
    "type": "line.create",
    "received_at": 1785346055000
  }
}
```

Differences from the same message read over HTTP:

- `updated_at`, `deleted_at`, `deletion_actor` are present as **explicit
  `null`** rather than absent.
- `pinned_by` is **`null`**, where HTTP sends `""`.
- Extra keys `location`, `parent_id`, `picture_url` appear. `location` was
  `{"lat": "", "lng": "", "name": null}` on every frame — never populated.
- `platform` and `favorited_by` are **absent**.

`sender_type` `bot` and `system` both arrive on this channel too. A system
message carries `system: true`, `sender_id: "system"`, `name: "GroupMe"`, and an
`event` object inside `subject` — see [§6.5](#65-system-event-types).

#### `direct_message.create` — a DM

Also on `/user/{user_id}`. `subject` is a DM message object:

```json
{
  "channel": "/user/20000001",
  "id": "<6-char hex>",
  "data": {
    "alert": "Example Contact: hey",
    "subject": {
      "id": "170000000000000002",
      "chat_id": "20000001+20000002",
      "source_guid": "<uuid v4>",
      "recipient_id": "20000001",
      "user_id": "20000002", "sender_id": "20000002", "sender_type": "user",
      "name": "Example Contact",
      "avatar_url": "https://i.groupme.com/…",
      "text": "hey",
      "created_at": 1785347209,
      "favorited_by": [],
      "picture_url": null,
      "location": { "lat": "", "lng": "", "name": null },
      "attachments": []
    },
    "type": "direct_message.create",
    "received_at": 1785347209000
  }
}
```

> **The thread key is on the frame as `chat_id`.** It does not have to be
> derived from `sender_id` and `recipient_id`. Note also that the field is named
> `chat_id` here, while the same value is `conversation_id` on a DM read over
> HTTP.

> **DM edits and deletes DO arrive over the websocket.** They come as
> `direct_message.create` with `sender_id: "system"`, `sender_type: "system"`,
> `name: "GroupMe"` and an `event` object inside `subject`. There is no
> `direct_message.update` or `direct_message.delete` type — the mutation is
> delivered as a *new system message in the thread*, exactly as it is in a
> group:

```json
{
  "alert": "GroupMe: Example Contact edited to: “will”",
  "subject": {
    "id": "170000000000000006",
    "chat_id": "20000001+20000002",
    "recipient_id": "20000001",
    "sender_id": "system", "user_id": "system", "sender_type": "system",
    "name": "GroupMe",
    "text": "Example Contact edited to: “will”",
    "created_at": 1785347452,
    "event": { "type": "message.update",
               "data": { "message_id": "170000000000000002",
                         "sender_id": "20000002",
                         "updated_at": 1785347452,
                         "message": { "text": "will", "attachments": [] } } },
    "favorited_by": [], "attachments": []
  },
  "type": "direct_message.create",
  "received_at": 1785347452000
}
```

The delete form is identical with
`event.type: "message.deleted"` and `data: {message_id, deleted_at,
deletion_actor}`.

#### `like.create` — a reaction

On `/user/{user_id}`. The `subject` is **not** a message — it is a three-part
object, and the message is nested under `line`:

```json
{
  "channel": "/user/20000001",
  "id": "<6-char hex>",
  "data": {
    "alert": "Example Member reacted 👍 to Example message body",
    "subject": {
      "line": {
        "id": "170000000000000001",
        "group_id": "10000001",
        "user_id": "20000001",
        "name": "Example Sender", "avatar_url": "",
        "source_guid": "<uuid v4>",
        "text": "Example message body",
        "created_at": 1785346587,
        "favorited_at": 1785346684,
        "favorited_by": ["20000003"],
        "system": false,
        "location": { "lat": "", "lng": "", "name": null },
        "picture_url": null,
        "attachments": []
      },
      "reactions": [
        { "type": "unicode", "code": "👍", "user_ids": ["20000003"] }
      ],
      "user_id": "20000003",
      "user_reaction": { "type": "unicode", "code": "👍",
                         "user_ids": ["20000003"] }
    },
    "type": "like.create",
    "received_at": 1785346685000
  }
}
```

`subject.reactions` is the message's **full** reaction list after the change —
write it over the stored value. `subject.user_id` is who reacted;
`subject.user_reaction` is the single reaction they added. `line.favorited_at`
appears only here.

No `like.destroy` frame was observed. Removing a reaction produced nothing on
the socket, so un-reacting must be reconciled by refetching — **the same trap as
unpin**.

#### `typing` — bidirectional

The only frame the client **publishes**. Sent on `/group/{group_id}` and
`/direct_message/{a}_{b}`, never on `/user/{user_id}`:

```json
{ "channel": "/group/10000001",
  "data": { "type": "typing", "user_id": "20000001",
            "started": 1785346072997 },
  "clientId": "<31-char client id>",
  "id": "7",
  "ext": { "access_token": "<40-char token>" } }
```

The server acknowledges the publish with a bare
`{id, clientId, channel, successful: true}` — no `data`. Other subscribers
receive the same `data` with only an `id`, no `clientId` and no `ext`:

```json
{ "channel": "/group/10000001",
  "data": { "type": "typing", "user_id": "20000001",
            "started": 1785346072997 },
  "id": "7" }
```

`started` is Unix **milliseconds**. There is no "stopped typing" frame; expire
it client-side.

> A client's own typing frame is echoed back to it. Filter on
> `data.user_id == own_user_id`.

#### `read_receipt.create`

On `/direct_message/{a}_{b}` — the only payload type observed on a DM channel
besides `typing`, and it has no group equivalent:

```json
{ "channel": "/direct_message/20000001_20000002",
  "id": "<6-char hex>",
  "data": {
    "alert": "",
    "subject": { "id": "170000000000000001",
                 "chat_id": "20000001+20000002",
                 "message_id": "170000000000000001",
                 "user_id": "20000001",
                 "read_at": 1785347486 },
    "type": "read_receipt.create",
    "received_at": 1785347486000 } }
```

`subject.id` and `subject.message_id` were the same value. `alert` is the empty
string — this one is not a notification.

### 6.5 System event types

When `subject.system` is true (or `sender_type` is `"system"`), `subject.event`
carries the structured form of what happened. The same `event` objects appear on
system messages fetched over HTTP. Full census over this capture:

| `event.type` | Count | `event.data` keys |
|---|---|---|
| `message.deleted` | 107 | `message_id`, `deleted_at`, `deletion_actor` |
| `message.update` | 46 | `message_id`, `sender_id`, `updated_at`, `message{text,attachments}` |
| `membership.notifications.removed` | 25 | `removed_user`, `remover_user` |
| `membership.notifications.exited` | 19 | `removed_user` |
| `group.type_change` | 17 | `type`, `message_edit_period`, `user{id,nickname}` |
| `membership.announce.joined` | 16 | `user{id,nickname}` |
| `calendar.event.cancelled` | 15 | `conversation{id}`, `event{id,name}`, `user{id,nickname}` |
| `group.topic_change` | 14 | `topic`, `user{id,nickname}` |
| `membership.announce.added` | 12 | `adder_user`, `added_users` |
| `bot.add` | 10 | bot identity |
| `membership.announce.rejoined` | 6 | `user{id,nickname}` |
| `message.pinned` | 3 | `message_id`, `pinned`, `pinned_by`, `pinned_at` |
| `calendar.event.user.going` | 3 | `event{id,name}`, `user{id,nickname}` |
| `calendar.event.user.not_going` | 3 | as above |
| `calendar.event.user.undecided` | 3 | as above |
| `group.role_change_admin` | 2 | role change target |
| `poll.created` | 2 | `conversation{id}`, `poll{id,subject}`, `user{id,nickname}` |
| `calendar.event.created` | 2 | `event{id,name}`, `original_url`, `url`, `user{id,nickname}` |
| `group.avatar_change` | 2 | new avatar |

> **`event.data` ids split by family.** The `membership.*` and `group.*` events
> carry `user.id` as a JSON **number** (`{"id": 20000001, "nickname": "…"}`),
> while `message.*`, `calendar.*` and `poll.*` carry ids as **strings**. Same
> envelope, both types. See [§11.1](#111-serialization).

There is **no** `message.unpinned` and no `like.destroy`. See
[§5.4](#54-pin-and-unpin) and [§6.4](#64-frame-types).

### 6.6 What the socket does not carry

- **Group messages do not arrive on `/group/{group_id}`.** That channel carried
  only `typing`. Every message, edit, delete and reaction arrived on
  `/user/{user_id}`. A client that subscribes per-group and not to its own user
  channel receives nothing but typing indicators.
- **No un-react and no un-pin signal** ([§5.4](#54-pin-and-unpin),
  [§6.4](#64-frame-types)).
- **No group read receipts.** `read_receipt.create` appeared only on DM
  channels.
- **No backfill.** The socket delivers what happens while connected. History and
  anything missed during a disconnect must come from
  [§4.3](#43-message-history).

> This app still polls. The socket is documented because the protocol reference
> should be complete; the archiver's correctness comes from the HTTP backfill,
> which is resumable and verifiable against `response.count`.

---

## 7. Membership and group administration

Split across `api.groupme.com` **and** `v2.groupme.com`, with no obvious rule
governing which lives where. Mute, unmute and leave are on `v2`; everything else
is on `/v3`.

### 7.1 Reading membership

| Method | Path | Query | Status |
|---|---|---|---|
| `GET` | `/v3/groups/{group_id}/members` | `filter` | 200 |
| `GET` | `/v3/groups/{group_id}/pending_memberships` | — | 200 |
| `GET` | `/v3/groups/{group_id}/subgroups` | — | 200 |

Active members come back inside the group object via
`GET /v3/groups/{group_id}?include=members` ([§4.2](#42-conversation-lists)).
`/members` was only ever called with `filter=inactive`, which returns the people
who are *gone*:

```json
{ "memberships": [
    { "id": "1000000001", "user_id": "20000002",
      "name": "Example Person", "nickname": "Example",
      "image_url": "https://i.groupme.com/…",
      "state": "removed", "roles": ["user"] } ] }
```

`state` observed as `removed` (4,313), `exited` (6,567) and `exited_removed`
(88) — the third is a real third value, not a typo, and a client matching on
exactly two states will misclassify it. `image_url` is present only sometimes.
`roles` was `["user"]` on all 10,968 inactive memberships.

`pending_memberships` and `subgroups` both returned `{"response": []}` on every
call — 33 and 4 calls respectively. Path and status are confirmed; the populated
shape is not, and is not guessed at here.

> **`id` is the membership id, not the user id.** They are different numbers, and
> `…/members/{id}/update` and `…/memberships/{id}/destroy` both want the
> *membership* id.

### 7.2 Changing a membership

| Method | Path | Changes |
|---|---|---|
| `POST` | `/v3/groups/{group_id}/memberships/update` | **your own** membership |
| `POST` | `/v3/groups/{group_id}/members/{membership_id}/update` | **someone else's** role |

Two different nouns — `memberships` versus `members` — for the same concept,
distinguished by whether a membership id is in the path.

```http
POST /v3/groups/{group_id}/memberships/update
content-type: application/json

{ "membership": { "nickname": "SK" } }
```
```json
{ "meta": { "code": 200 },
  "response": { "id": "1000000001", "user_id": "20000001",
                "nickname": "SK", "muted": true,
                "image_url": null, "avatar_url": null,
                "autokicked": false, "app_installed": true } }
```

The request is wrapped in `"membership"`; the response is **not** wrapped — it
is the membership object directly under `response`.

```http
POST /v3/groups/{group_id}/members/{membership_id}/update
content-type: application/json

{ "role": "user" }
```
```json
{ "meta": { "code": 200 },
  "response": { "member": {
    "id": "1000000001", "user_id": "20000002",
    "nickname": "Example", "name": "Example Person",
    "muted": true, "image_url": "https://i.groupme.com/…",
    "autokicked": false, "roles": ["user"] } } }
```

Here the request is **unwrapped** and the response **is** wrapped, under
`member` — the exact inverse of the sibling endpoint. `role` is singular in the
request and `roles` is an array in the response. Observed values: `user`,
`admin`, `owner`.

### 7.3 Mute, unmute and leave

All three on `v2.groupme.com`, all `POST`, all `200`.

```http
POST https://v2.groupme.com/groups/{group_id}/memberships/mute
content-type: application/json

{ "duration": null }
```
```json
{ "meta": { "code": 200 },
  "response": { "membership": {
    "id": "1000000001", "user_id": "20000001",
    "nickname": "Example User", "avatar_url": null,
    "state": "muted",
    "created_at": 1732129896, "updated_at": 1785346492,
    "muted_until": 253402300800,
    "recap_enabled": false, "child_states": null,
    "has_sound_enabled": true, "autokicked": false } } }
```

`duration: null` means forever, and `muted_until` comes back as
**`253402300800`** — the year 9999. `unmute` takes **no body** and returns the
same object with `state: "active"` and `muted_until: null`.

```http
POST https://v2.groupme.com/groups/{group_id}/memberships/{membership_id}/destroy
```
```json
{ "meta": { "code": 200 }, "response": null }
```

No body in either direction. This is how the account **leaves** a group; whether
the same path removes *another* member was not exercised.

Note the `state` vocabulary here (`muted`, `active`) is disjoint from the
`state` vocabulary on an inactive membership (`removed`, `exited`,
`exited_removed`) in [§7.1](#71-reading-membership). Same field name, two
enumerations, two endpoints.

### 7.4 Changing a group

```http
POST /v3/groups/{group_id}/update
content-type: application/json

{ "description": "Example description" }
```

Returns `200` and **the entire group object**, members array included, under
`response` — not a diff, and not wrapped in `"group"`. Fields observed being set
this way: `description` (emits `group.topic_change`), `type` (emits
`group.type_change`), and the group avatar (emits `group.avatar_change`).

`type` observed as `private`, `closed` and `announcement`. Changing it to
`announcement` also changed `message_edit_period` to `43200`, reported inside
the resulting system event.

The update response includes keys absent from the `/v3/groups` list form —
`thread_id`, `expires_at`, `theme_id`, `theme_custom_url`, `visibility`
(observed `hidden`), `audio_message_disabled`, `locations`, `max_memberships`.

### 7.5 Blocking

| Method | Path | Query | Status |
|---|---|---|---|
| `POST` | `/v3/blocks` | `user`, `otherUser` | **201** |
| `DELETE` | `/v3/blocks` | `user`, `otherUser` | 200 |

Both parameters are query-string, both are **camelCase** `otherUser` — the only
camelCase parameter in the API apart from `acceptFiles`. `user` is the caller's
own id.

```http
POST /v3/blocks?user=20000001&otherUser=20000003
```
```json
{ "meta": { "code": 201 },
  "response": { "block": { "user_id": "20000001",
                           "blocked_user_id": "20000003" } } }
```

`DELETE /v3/blocks?user=…&otherUser=…` returns `200` with a **zero-length
body** — no envelope. There is no block *list* endpoint; blocked contacts are
surfaced through `is_blocked` on `/v4/relationships`
([§4.1](#41-identity-and-contacts)), which is why the client passes
`include_blocked=true` there.

### 7.6 Permanently out of scope — CAPTCHA

**Creating a group and adding a member to a group are both gated behind a
CAPTCHA** in the web client. Both flows were attempted during the capture
session and both stopped at the challenge, so **no endpoint was recorded for
either** — there is nothing to document, and nothing is guessed at here.

This project does not bypass CAPTCHAs. These two operations are **permanently
out of scope**, not "not yet implemented". No workaround is described here and
none should be added.

---

## 8. Ancillary surfaces

Feature areas that hang off a conversation. None is required to read or write
messages.

### 8.1 Calendar events

Six endpoints, all under `/v3/conversations/{conversation_id}/events/`, all
using a **verb as the last path segment** rather than HTTP method semantics —
`delete` is a `DELETE`, but so is `rsvp/delete`, and `create` is a `POST`.

| Method | Path | Query | Status |
|---|---|---|---|
| `GET` | `…/events/list` | `end_at`, `limit` | 200 |
| `GET` | `…/events/show` | `event_id` | 200, 404 |
| `POST` | `…/events/create` | — | **201** |
| `DELETE` | `…/events/delete` | `event_id` | 200 |
| `POST` | `…/events/rsvp` | `event_id`, `going` | 200 |
| `DELETE` | `…/events/rsvp/delete` | `event_id` | 200 |

`{conversation_id}` is a group id or a `+`-joined DM key; both were observed.
`end_at` on `list` is ISO-8601 **with a UTC offset**, not a `Z` timestamp.

```http
POST /v3/conversations/{conversation_id}/events/create
content-type: application/json

{ "name": "Example Event", "description": "…",
  "is_all_day": false,
  "start_at": "2026-07-29T14:29:19.110-04:00",
  "end_at": "2026-07-29T15:29:19.110-04:00",
  "timezone": "America/New_York",
  "scheduled_call": false,
  "reminders": [] }
```

The `201` response carries **three** siblings: `event`, `share_token`, and the
`message` that was posted to the conversation announcing it (with a
`calendar.event.created` event and an `event` attachment). The event object:

```json
{
  "event_id": "<32-char hex>", "conversation_id": "10000001",
  "name": "Example Event", "description": "…",
  "creator_id": "20000001",
  "start_at": "2026-07-29T14:29:19-04:00",
  "end_at": "2026-07-29T15:29:19-04:00",
  "is_all_day": false, "timezone": "America/New_York",
  "scheduled_call": false, "call_started": false,
  "reminders": [],
  "aesthetics": { "font": "NONE", "theme": "NONE", "effect": "NONE" },
  "going": ["20000001"], "not_going": [], "maybe_going": [], "waitlisted": [],
  "going_count": 1,
  "rsvp_list": { "20000001": "2026-07-29T17:29:30Z" },
  "created_at": "2026-07-29T17:29:30Z", "updated_at": "2026-07-29T17:29:30Z",
  "share_url": "https://groupme.com/join_event/10000001/<32-char hex>/XXXXXXXX",
  "deep_link_ios": "groupme://join_event/…",
  "deep_link_android": "groupme://groupme.com/join_event/…",
  "share_qr_code": "https://image.groupme.com/qr/events/…/preview/token/XXXXXXXX",
  "is_top_level": false
}
```

RSVP is a `POST` with the answer in the **query string**:
`…/events/rsvp?event_id=<id>&going=false`. It returns the whole updated event.
`…/events/rsvp/delete?event_id=<id>` withdraws the RSVP, also returning the
whole event.

`events/delete` returns `{"meta": {"code": 200}, "response": null}`, and a
subsequent `events/show` for the same id returns `404` with
`{"meta": {"code": 404, "errors": ["not found"]}}`.

> **`going_count` is unreliable.** Observed as `1` with a one-element `going`
> array, as `0` alongside a non-empty `going` array, and as **`-1`** after an
> RSVP flipped to not-going. Derive the count from the arrays.

> `maybe_going` came back as `null` from `events/create` and as `[]` from
> `events/show` for the same event, seconds apart. `aesthetics` is absent from
> the create response and present on show.

Event timestamps are **ISO-8601 strings** while message timestamps are Unix
integers. Both appear in the same API.

### 8.2 Polls

| Method | Path | Status | Purpose |
|---|---|---|---|
| `GET` | `/v3/poll/{conversation_id}` | 200, 401 | List polls in a conversation |
| `POST` | `/v3/poll/{conversation_id}` | **201** | Create a poll |
| `GET` | `/v3/poll/{conversation_id}/{poll_id}` | 200 | One poll |
| `POST` | `/v3/poll/{conversation_id}/{poll_id}/{option_id}` | 200 | Cast a vote |

Note the path segment is the singular **`poll`**, and voting is a `POST` to a
path whose last segment is the option id — there is no request body.

The list form returns `{"polls": [...], "continuation_token": …}`; every other
form returns `{"poll": {...}}`. In **all** of them each poll is nested one level
deeper under a `data` key:

```json
{ "poll": { "data": {
    "id": "1700000000000001",
    "conversation_id": "10000001",
    "subject": "Example poll question?",
    "owner_id": "20000001",
    "created_at": 1766515361,
    "expiration": 1767675561,
    "last_modified": 1767675575,
    "status": "active",
    "type": "single",
    "visibility": "anonymous",
    "options": [
      { "id": "1", "title": "Option A", "votes": 1 },
      { "id": "2", "title": "Option B" }
    ] } } }
```

Creation:

```http
POST /v3/poll/{conversation_id}
content-type: application/json

{ "subject": "Example poll question?",
  "options": [{ "title": "Option A" }, { "title": "Option B" }],
  "expiration": 1785349783,
  "visibility": "anonymous",
  "type": "single" }
```

The `201` response carries `poll` **and** the announcement `message` (with a
`poll.created` event and a `poll` attachment), the same pattern as event
creation.

A vote response adds two keys beside `poll`: `user_vote` (a single option id
string) and `user_votes` (an array). Casting a second vote on a `type: "single"`
poll silently moved the vote — the first option lost its `votes` key entirely
and the second gained `"votes": 1`.

- `status` observed as `active` and `past`.
- `visibility` observed as `anonymous` and `public`; only `public` polls carry
  `voter_ids` on options.
- **An option with zero votes omits `votes` entirely** rather than sending `0`.
- `meta.code` is `20000` / `20100` on these routes, not `200` / `201`.
- `GET /v3/poll/{conversation_id}` returned **`401`** with
  `{"meta": {"code": 40100, "errors": ["Not authorized"]}}` for a conversation
  the account could still list. A 401 on one conversation does not mean the
  token is bad.
- `continuation_token` was non-null even on an empty `polls` array. It was never
  fed back, so its semantics are **inferred**.

### 8.3 URL preview

```http
GET /v1/urls/preview?url=<percent-encoded>
```

Note the **`/v1`** — the only surviving `/v1` route on `api.groupme.com`.
Statuses observed: `200`, `403`, `404`, and `415`. The last one is not an HTTP
415 from GroupMe but an upstream failure passed through:

```json
{ "error": { "source": "iframely", "code": 415,
             "message": "Requested page error: 415" } }
```

A successful response is an Open Graph document, **not** the standard envelope —
its top-level `meta` is page metadata, not a status block:

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

Special-case it. Code that reads `meta.code` here gets `undefined`.

### 8.4 Profile update

```http
POST https://v2.groupme.com/users/{user_id}
content-type: application/json

{ "user": { "name": "Example User" } }
```

Returns `200` and the full user object under `response.user` — a much larger
shape than `GET /v3/users/me`, including `email_settings`,
`sms_disabled_expires_at`, `group_notification_sound`, `dm_notification_sound`,
`interests`, `major_codes`, `photo_urls`, `song_url` and a `needs_password`
field. The `GET` and `POST` forms of the same `v2` path return **different**
representations of the same user.

### 8.5 QR rendering

```
GET https://image.groupme.com/qr/join_group/{group_id}/{share_token}/preview
      ?avatarUrl=…&bgColor=…&fgColor=…&logoColor=…
GET https://image.groupme.com/qr/contact/{user_id}/{share_token}/preview
      ?bgColor=…&fgColor=…&logoColor=…
```

Returns a rendered image. `{share_token}` is the 8-character tail of the
`share_url` on the group or user object; the same token appears in
`share_qr_code_url`, which is simply this URL pre-built. A third form,
`/qr/events/{conversation_id}/{event_id}/preview/token/{share_token}`, appears as
`share_qr_code` on an event object.

---

## 9. Payload shapes

### Message

```json
{
  "id": "170000000000000001",
  "source_guid": "<uuid v4>",
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
| `id` | Decimal string. **Never parse as a JS number** — [§11.6](#116-identifiers) |
| `text` | **Nullable.** Attachment-only messages carry `"text": null` |
| `avatar_url` | **Nullable** |
| `sender_type` | `user` \| `bot` \| `system` |
| `pinned_by` | Empty **string** when unpinned, not `null` |
| `pinned_at` | Unix seconds when pinned |
| `platform` | `gm` on every group message; **absent on DMs** |
| `favorited_by` | User ids who reacted, **one entry per reaction** — deduplicate |
| `reactions` | **Absent unless the message has reactions.** See below |
| `source_guid` | Client-generated idempotency key; echoed back on send |
| `updated_at` | **Absent unless the message was edited.** Unix seconds |
| `deleted_at` | **Absent unless deleted.** Unix seconds |
| `deletion_actor` | **Absent unless deleted.** `admin` \| `sender` |
| `event` | Present only on system messages; [§6.5](#65-system-event-types) |

Exact key census over the capture — 6,891 group messages and 257 DM messages:

| Key | Group | DM |
|---|---|---|
| `id`, `source_guid`, `created_at`, `name`, `text`, `avatar_url`, `user_id`, `sender_id`, `sender_type`, `attachments`, `favorited_by`, `pinned_at`, `pinned_by` | 6891 | 257 |
| `group_id`, `system`, `platform` | 6891 | **0** |
| `conversation_id`, `recipient_id` | **0** | 257 |
| `event` | 298 | 9 |
| `reactions` | 197 | 6 |
| `deleted_at`, `deletion_actor` | 102 | 4 |
| `updated_at` | 45 | 5 |

`reactions` on a message read is the same array as the `like` response
([§5.5](#55-reactions)) but **without** `pack_id` and `pack_index`:

```json
"reactions": [ { "type": "unicode", "code": "🤣", "user_ids": ["20000002"] } ]
```

**DM conversation ids are `"{lower_user_id}+{higher_user_id}"`**, the two user
ids sorted **numerically** ascending and joined with `+`. Sorting them as
strings produces the wrong key for ids of differing length. The same pair is
joined with an **underscore** in a Bayeux channel name
([§6.3](#63-subscribing)).

`GET /v3/direct_messages` also returns a sibling `read_receipt` object next to
`direct_messages`:

```json
{ "id": "", "chat_id": "20000001+20000002",
  "message_id": "170000000000000001", "user_id": "20000002",
  "read_at": 1784752304 }
```

The websocket delivers a *different* representation of the same message —
explicit `null`s instead of absent keys, plus `location`, `parent_id` and
`picture_url`. See [§6.4](#64-frame-types).

### Attachments

An **open union** — GroupMe has shipped new `type` values without notice. An
unknown type must never fail the containing message; losing a message to an
unrecognised sticker is the worst outcome for an archive. This app parses
unknown types into a passthrough variant.

Full census of `attachments[].type` over this capture, with the exact key sets
observed for each:

| `type` | Count | Key set(s) |
|---|---|---|
| `reply` | 229 | `type`, `user_id`, `reply_id`, `base_reply_id` |
| `image` | 211 | `type`, `url` (135) — or `type`, `url`, `source_url`, `blur_hash` (76) |
| `mentions` | 76 | `type`, `user_ids`, `loci` |
| `copilot` | 46 | `type`, `conversation_id`, `message_id`, `part_id`, `prompt_sender` — plus `citations` (10) |
| `event` | 26 | `type`, `event_id`, `view` |
| `file` | 5 | `type`, `file_id` |
| `emoji` | 3 | `type`, `placeholder`, `charmap` |
| `poll` | 2 | `type`, `poll_id` |

```json
{ "type": "image",
  "url": "https://m.groupme.com/uploads/{hash}/1792x2400.original.jpeg",
  "source_url": "…", "blur_hash": "]47^xx~D4URPjF…" }

{ "type": "reply", "user_id": "20000002",
  "reply_id": "170000000000000001", "base_reply_id": "170000000000000001" }

{ "type": "mentions", "user_ids": ["20000001", "-1"], "loci": [[0, 12], [20, 8]] }

{ "type": "emoji", "placeholder": "�", "charmap": [[1, 5]] }

{ "type": "event", "event_id": "<32-char hex>", "view": "full" }

{ "type": "poll", "poll_id": "1700000000000001" }

{ "type": "file", "file_id": "<uuid v4>" }

{ "type": "copilot",
  "conversation_id": "<21-char nanoid>",
  "message_id": "<21-char nanoid>",
  "part_id": "<21-char nanoid>",
  "prompt_sender": "20000001",
  "citations": [ { "index": 1, "title": "…", "url": "https://…",
                   "publisher": "" } ] }
```

- `loci` is `[[start_char, length], …]`, parallel to `user_ids`.
- A `user_id` of **`"-1"`** is `@everyone`, not a real account. Joining it
  against a users table finds nothing; special-case it.
- `reply_id` is the message replied to; `base_reply_id` is the thread root.
  They differ when replying to a reply.
- `blur_hash` decodes to a blurred placeholder from ~30 bytes — useful for
  showing *something* offline when an image was never cached. Older messages
  lack it, so the two `image` key sets are a chronology, not a variant.
- `event.view` observed as `full` and `brief` — `full` on the creation message,
  `brief` on the cancellation message.
- `copilot` ids are **not** GroupMe ids. `conversation_id`, `message_id` and
  `part_id` are 21-character nanoid-shaped strings from a separate system, and
  joining them against message or conversation tables finds nothing. The
  containing message is an ordinary user message whose `text` is the assistant's
  reply. `citations` is present only when the answer cited sources.
- `file.file_id` resolves through
  `POST file.groupme.com/v1/{conversation_id}/fileData`
  ([§5.7](#57-uploads)); nothing in the attachment carries the filename.
- `poll.poll_id` and `event.event_id` resolve through
  [§8.2](#82-polls) and [§8.1](#81-calendar-events).

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

- With `omit=memberships`, `members` is **explicitly `null`** and
  `members_count` is `0` — **`0` here means "not loaded", not "empty group."**
  Don't persist it as truth. 900 of 900 group objects in the capture came back
  this way; see [§11.1](#111-serialization).
- `muted_until: 253402300800` is year-9999, i.e. muted forever.
- `like_icon.pack_id`/`pack_index` are **numbers** here and can be `null` on
  the whole `like_icon`. See [§11.1](#111-serialization).
- `unread_count`/`last_read_*` were `null` throughout; read state comes from
  `/v4/read_receipts` instead ([§4.5](#45-read-state)).
- `type` observed as `private`, `closed` and `announcement`.
  `POST …/groups/{id}/update` returns a superset of these keys —
  [§7.4](#74-changing-a-group).

### Membership

Two different shapes for the same object, depending on where it is read.

Inside a group payload (`?include=members`, or a group update response):

```json
{ "id": "1000000001", "user_id": "20000002",
  "nickname": "Example", "name": "Example Person",
  "image_url": "", "muted": true, "autokicked": false,
  "roles": ["admin", "owner"] }
```

From `GET /v3/groups/{id}/members?filter=inactive` — gains `state`, loses
`muted` and `autokicked`:

```json
{ "id": "1000000001", "user_id": "20000002",
  "name": "Example Person", "nickname": "Example",
  "state": "removed", "roles": ["user"] }
```

From a `v2` mute/unmute — a third shape again, with a **different `state`
vocabulary** ([§7.3](#73-mute-unmute-and-leave)).

> **`id` is the membership id, not the user id.** They are different numbers,
> and `…/members/{id}/update` and `…/memberships/{id}/destroy` want the
> membership id.

### Chat (DM thread)

```json
{
  "created_at": 1781230932, "updated_at": 1784752303,
  "messages_count": 108,
  "other_user": { "id": "20000002", "name": "Example Contact",
                  "avatar_url": "https://i.groupme.com/…" },
  "last_message": { },
  "message_deletion_period": 2147483647,
  "message_deletion_mode": ["sender"],
  "message_edit_period": 15,
  "requires_approval": false,
  "unread_count": null, "last_read_message_id": null, "last_read_at": null
}
```

A DM thread has **no id field of its own** — it is keyed by `other_user.id`, and
that is what `GET /v3/direct_messages?other_user_id=…` wants. The `+`-joined
form appears only as `conversation_id` on the messages inside it, and as the
path segment for `/v3/chats/{conversation_id}` and the delete/pin/receipt
routes. This app uses `other_user.id` as the conversation id.

`last_message` is a full message object; `messages_count` is the lifetime total,
matching `response.count` on a history fetch.

### Elsewhere

| Shape | Where |
|---|---|
| User (own) | [§4.1](#41-identity-and-contacts) |
| User (other) | [§4.1](#41-identity-and-contacts) |
| Relationship | [§4.1](#41-identity-and-contacts) |
| Read receipt | [§4.5](#45-read-state), [§5.6](#56-read-receipts) |
| Calendar event | [§8.1](#81-calendar-events) |
| Poll | [§8.2](#82-polls) |
| URL preview | [§8.3](#83-url-preview) |
| Realtime frames | [§6.4](#64-frame-types) |

---

## 10. Media, and why URLs are not archivable

Attachment URLs in message payloads point at `m.groupme.com`:

```
https://m.groupme.com/uploads/{hash}/1792x2400.original.jpeg
```

That returns **`301`** to `cdn2.groupme.com`, where the real object sits behind
an **Azure Blob Storage SAS signature**:

```
https://cdn2.groupme.com/uploads/{hash}/original.jpeg
    ?sv=…&se=…&sr=b&sp=r&sig=…
    &skoid=…&sktid=…&ske=…&sks=b&skt=…&skv=…&rsct=image%2Fjpeg
```

`se` is the expiry, `sig` the signature, `sp` the permission (`r` on reads,
`cw` on the upload URL from [§5.7](#57-uploads)), and `rsct` the content type
the CDN will respond with. Expiries observed were roughly a day out, and the
same object served a **different signature on every fetch** — the redirect is
minted per request.

Note the path also differs across the redirect: the `m.groupme.com` URL carries
a `{w}x{h}.original.{ext}` segment, the `cdn2` object is just `original.{ext}`.
The dimensions are a rendering hint, not part of the object key.

> **This is the single most important consequence for an archive.** Storing the
> attachment URL is not archiving the attachment — the `m.groupme.com` URL needs
> a live redirect and a fresh signature to resolve, so a "cached" message with a
> stored URL shows a broken image offline, and eventually breaks online too.
>
> The bytes must be downloaded and stored locally. This app follows the redirect
> at sync time, saves the object to a blob directory, and records the mapping in
> `media_cache`. `blur_hash`, where present, is the fallback for anything not yet
> fetched.

Avatars on `i.groupme.com` are unsigned and stable:

```
https://i.groupme.com/{w}x{h}.{ext}.{hash}
https://i.groupme.com/{w}x{h}.{ext}.{hash}.avatar
https://i.groupme.com/{w}x{h}.{ext}.{hash}.preview
https://i.groupme.com/{w}x{h}.{ext}.{hash}.large.avatar
```

The bare form, `.avatar` (cropped), `.preview` and `.large.avatar` were all
observed. **89 of the avatar requests in the capture returned `403`** while the
rest returned `200`, including repeated requests for the same object — treat
avatar fetch failure as routine and non-fatal, not as an auth problem.

Uploading to either host is [§5.7](#57-uploads).

---

## 11. Gotchas

Grouped by kind. Every one of these cost real debugging time at least once.

### 11.1 Serialization

**`#[serde(default)]`-style "the field is optional" is not enough.** A default
attribute fires only when the key is **absent**. A key present with an explicit
`null` goes to the field's own deserializer, and `Vec`, `bool`, `i64` and
`String` all reject it — failing the entire response, not just the field.

GroupMe sends explicit `null` constantly. `"members": null` came back on **900
of 900** group objects in the capture (every `/v3/groups` page with
`omit=memberships`). `like_icon` is `null` on some groups and an object on
others. Realtime frames send `"updated_at": null`, `"deleted_at": null`,
`"deletion_actor": null`, `"pinned_by": null` where HTTP omits the key. Pair a
default with a `null_as_default` deserializer on every non-`Option` field.

**Absent and `null` and empty-string are three different things, sometimes for
the same field.** `pinned_by` is `""` when unpinned over HTTP and `null` over
the socket. `read_receipt.id` is `""`. `members_count` is `0` when not loaded.
None of these means what the type suggests.

**Types are not stable across endpoints.**

| Field | One place | Another place |
|---|---|---|
| `like_icon.pack_id` / `pack_index` | number on a group | absent from `reactions` on a message read; number in the `like` response |
| `event.data` ids | **number** in `membership.*` and `group.*` events (`{"id": 20000001}`) | **string** in `message.*`, `calendar.*` and `poll.*` events |
| DM thread key | `conversation_id` on a message | `chat_id` on a websocket frame and on `read_receipt` |
| DM thread key separator | `+` in every path and payload | **`_`** in a Bayeux channel name |
| `maybe_going` on an event | `null` from `events/create` | `[]` from `events/show` |

Parse permissively and normalize on the way in.

**Timestamps come in three formats.** Unix **seconds** on messages, groups,
chats, polls and memberships; Unix **milliseconds** on websocket `received_at`
and `typing.started`; **ISO-8601 strings** on calendar events and
`/v4/relationships` (the latter with microsecond precision).

**Response keys differ for identical shapes.** `messages` for groups,
`direct_messages` for DMs. `message` versus `direct_message` on send. And the
request wrapper differs from the response wrapper on
`…/memberships/update` versus `…/members/{id}/update`
([§7.2](#72-changing-a-membership)) — one wraps the request and not the
response, the other does the reverse.

**Attachments are an open union.** GroupMe has shipped new `type` values
without notice; eight are catalogued in [§9](#attachments). An unknown type must
never fail the containing message — losing a message to an unrecognised sticker
is the worst outcome for an archive.

### 11.2 Pagination and history

**`per_page` vs `limit`.** Conversation lists take `per_page` and `page`;
message endpoints take `limit` and `before_id`.

**Pages arrive newest-first, so the cursor is the *last* element.** Taking
`messages[0]` stalls the backfill on page two
([§4.4](#44-pagination-and-cursors)).

**Terminate on an empty page, not a short one.** Short pages occur mid-history.
`response.count` is the conversation lifetime total and is the completeness
check.

**Empty is spelled several ways.** `"response": []`, `"response": null`,
`{"count": 0, "messages": []}`, and a zero-length body on a 404. All must be
handled as "no results" rather than as errors — this is the backfill
terminator, so getting it wrong means either an infinite loop or a truncated
archive.

**A deleted message is not deleted.** The row stays in `…/messages` with its id,
sender and `created_at` intact; only `text` is replaced with a tombstone and
`deleted_at`/`deletion_actor` are added. Detect it by key presence, not by
absence from the page — and never by matching the tombstone string, which
differs by actor (`"An admin deleted this message"` vs
`"This message was deleted"`).

**`updated_at` exists only after an edit.** Never-edited messages omit the key
rather than mirroring `created_at` — 50 of 7,148 messages carried it in this
capture. A schema that reads `updated_at` as "last touched" gets `null` for
almost everything.

**`favorited_by` has one entry per reaction, not per person.** A user who left
two different emoji appears twice. Deduplicate before counting reactors.

**DM messages have no `system` and no `platform` key.** Group messages carry
both on every row (6,891 of 6,891); DM rows carry neither (0 of 257) —
including DM *system* messages, which are identifiable only by
`sender_type: "system"`. Branching on `message.system == true` silently misses
every edit and delete notice in a DM.

### 11.3 Status codes and envelopes

**Do not assert `200`.** Success is spelled at least four ways:

| Operation | Success status |
|---|---|
| Send a message or DM, create a poll, create an event, block a user, `web_pings` | **201** |
| Edit, pin, unpin, react, `v3` read receipt, RSVP, group update, membership update, mute/unmute/leave, unblock | 200 |
| `v4` read receipt, `mark_all_read` | **202** |
| Delete a message | **204** |
| Upload bytes to the CDN | **201** |

**`meta.code` is not the HTTP status.** Observed `20000`, `20100`, `20200` and
`40100` on HTTP `200`/`201`/`202`/`401` responses — an internal five-digit
scheme layered over the HTTP one. Branch on the HTTP status; treat `meta.code`
as advisory. Asserting `meta.code == 200` rejects perfectly good responses.

**Eleven endpoints do not return the envelope**, several with a zero-length
body. The full list is in [§3](#endpoints-that-escape-the-envelope). Decoding
the body unconditionally throws on `DELETE …/messages/{id}` — the one call whose
success is hardest to retry safely.

**404 is routine on message routes**, with a zero-length body, for a group the
account has left. It is not a transport error.

**401 is per-resource, not per-token.** `GET /v3/poll/{conversation_id}`
returned `401` for one conversation while every other call on the same token
succeeded.

**403 is routine on avatars.** 89 avatar fetches returned `403` in the capture
while the rest returned `200`.

### 11.4 Path versions and shapes

**There is no single message resource path, and no single API version.**

| Operation | Version | Collection |
|---|---|---|
| Send to a group | `/v3` | `groups/{id}/messages` |
| Send a DM | `/v3` | `direct_messages` |
| Edit | **`/v4`** | `groups/{id}/messages/{id}` |
| Delete, pin, unpin | `/v3` | `conversations/{id}/messages/{id}` |
| React | `/v3` | `messages/{cid}/{mid}/like` |
| Read pins | `/v3` | `pinned/groups/{id}/messages` |
| Read receipt (write) | **`/v4`** *and* `/v3` | `read_receipts/{id}` / `conversations/{id}/read_receipt` |
| URL preview | **`/v1`** | `urls/preview` |
| Image upload | — | `m.groupme.com`, then `cdn2.groupme.com` |
| Avatar upload | — | `image.groupme.com` |
| File metadata | `/v1` | `file.groupme.com` |
| Mute, unmute, leave | — | `v2.groupme.com` |
| Sign-in | — | `v2.groupme.com` |

Three *different* `/v3` collections address the same message object
(`conversations/…`, `messages/…`, `pinned/groups/…`) and edit is under `/v4`
with a fourth noun. Nothing generalises; route each verb individually.

**Conversation ids are polymorphic.** `{conversation_id}` is a group id for a
group and `{lower}+{higher}` for a DM, on the same path. Both were observed on
delete, read receipts and events.

**Parameter casing is inconsistent.** Everything is `snake_case` except
`acceptFiles`, `otherUser` and the `m.groupme.com` upload body
(`senderId`, `groupId`, `fileSize`).

### 11.5 Rate limits

**429s were observed above roughly 10 requests per second** against a single
token. Sustained **~8 requests/second, serialised**, ran without a single 429
across the whole capture session — zero `429` responses appear in
`traffic.jsonl`. The threshold is approximate: it was found by driving the
client hard, not by a controlled binary search, and GroupMe publishes nothing.

Treat ~8 req/s serialised as the safe ceiling. This app serialises sync
requests and backs off exponentially (1s / 2s / 4s with jitter) on `429` and
`5xx`.

Note that a media backfill multiplies request count invisibly: one message with
an image is one API call plus a redirect plus a CDN fetch.

### 11.6 Identifiers

**IDs must be strings.** `170000000000000001` exceeds IEEE-754 integer
precision (2^53). Round-trip it through a JS `Number` or a JSON parser that
defaults to float and it silently corrupts into a different, valid-looking id.
Store as `TEXT`. They *do* fit in a signed 64-bit integer, so this app keeps a
parallel `id_sort INTEGER` column for ordering and cursors, and never uses it as
identity.

**Membership id ≠ user id.** Different numbers, and the member-management
routes want the membership id ([§7.2](#72-changing-a-membership)).

**`user_id: "-1"` in a `mentions` attachment is `@everyone`**, not an account.
Joining it against a users table finds nothing.

**`sender_id: "system"` is a literal string**, not a numeric id, on every system
message.

**`copilot` attachment ids are from a different system entirely** — 21-character
nanoid-shaped strings that do not join against anything in GroupMe
([§9](#attachments)).

**A DM thread has no id of its own.** It is keyed by the other user's id; the
`+`-joined form is derived, and must be sorted **numerically** ascending. String
sorting produces the wrong key for ids of differing length.

---

## 12. What this app calls

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

The websocket is documented in [§6](#6-realtime-faye--bayeux) but not used. The
archiver's correctness comes from the HTTP backfill, which is resumable and
verifiable against `response.count`; a socket that drops for thirty seconds
loses messages silently.

---

## 13. Out of scope and not observed

### Permanently out of scope

**Creating a group** and **adding a member to a group** are behind a CAPTCHA
([§7.6](#76-permanently-out-of-scope--captcha)). This project does not bypass
CAPTCHAs. No endpoint was recorded for either, nothing is guessed at, and no
workaround is described.

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

### Observed, but never populated

Path and status confirmed; the populated payload shape is not, and is not
guessed at:

| Endpoint | Calls | Always returned |
|---|---|---|
| `GET /v3/directories` | 4 | `[]` |
| `GET /v3/groups/{id}/pending_memberships` | 33 | `[]` |
| `GET /v3/groups/{id}/subgroups` | 4 | `[]` |
| `v2.groupme.com/users/{id}` → `relationship` | 11 | `null` |
| message `location` | every frame | `{"lat": "", "lng": "", "name": null}` |

### Not observed at all

Reachable in the product but absent from this capture. **Do not add these from
the public docs or from memory** — they belong here until a capture produces
them:

- **The file upload that produces a `file_id`.** The metadata read
  ([§5.7](#57-uploads)) and the `file` attachment
  ([§9](#attachments)) are both documented; the upload itself never appeared.
- **DM edit.** DM *deletes* are confirmed ([§5.3](#53-delete)) and DM edits are
  confirmed to *happen* over the socket ([§6.4](#64-frame-types)), but this
  account never issued one, so no request was recorded.
- **DM pin/unpin.** `/v3/pinned/direct_messages` reads pins
  ([§4.3](#43-message-history)); the write was never issued in a DM.
- **Token refresh.** `expires_at` exists ([§2.2](#22-sign-in)); no refresh call
  was seen.
- **`after_id` / `since_id`** on message history
  ([§4.4](#44-pagination-and-cursors)).
- **`continuation_token`** pagination on polls ([§8.2](#82-polls)).
- **A populated `relationship` object.**
- **The Copilot API.** Only its *attachment* was captured
  ([§9](#attachments)) — the conversation with the assistant happens somewhere
  this capture did not reach.
- **`PATCH`** on `/v4` routes ([§5.2](#52-edit)).
- **`like.destroy`** or any un-react frame ([§6.4](#64-frame-types)).
- **Group read receipts** over the socket ([§6.6](#66-what-the-socket-does-not-carry)).

---

## 14. Reproducing this capture

Two capture paths are needed, because HTTP and websocket require different
tools.

**HTTP.** `tools/capture_api.py` drives Chrome behind a selenium-wire MITM proxy
and logs full request/response detail to `tools/capture-out/traffic.jsonl`.
`tools/digest_capture.py` reduces that to a reviewable digest.

selenium-wire 5.1.0 has been unmaintained since 2023 and its vendored mitmproxy
is built against a pyOpenSSL X509 API that was removed in 23.3. Four call sites
break, and the symptom is that no page loads at all rather than an obvious
error. `capture_api.py` monkey-patches all four (`Cert.altnames`, `create_ca`,
`dummy_cert`, `CertStore.create_store`) onto `cryptography` at import time.

**Websocket.** The proxy cannot see inside an upgraded connection. Frames are
read with the Chrome DevTools Protocol —
`goog:loggingPrefs: {performance: ALL}` plus `Network.webSocketFrameSent` and
`Network.webSocketFrameReceived` — into
`tools/capture-out/websocket.jsonl`. `tools/analyze_ws.py` groups them by
channel and prints one representative of each shape.

Forcing Faye's long-poll fallback (`--force-longpoll`, which deletes
`window.WebSocket` and `window.EventSource` before page scripts run) makes the
frames visible to the proxy, but GroupMe's edge answers every long-poll with
`504`. It is not a usable substitute.

> **`traffic.jsonl` reaches 130 MB or more.** Never open it with a file-reading
> tool; script over it.

> **The capture output contains live credentials** — the access token, session
> cookies, and, if sign-in happens during recording, the password field and the
> 2FA PIN, plus the plaintext of every message fetched and every participant's
> real name and user id. `tools/capture-out/` and `tools/.chrome-profile/` are
> gitignored; verify with `git check-ignore -v` **before** writing anything
> sensitive. Revoke the token afterwards.
>
> Nothing from a capture goes into this file verbatim. See
> [Identifiers](#identifiers-in-this-document).
