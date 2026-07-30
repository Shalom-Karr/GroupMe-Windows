//! HTTP client for the GroupMe REST API.
//!
//! `GroupMeClient` handles auth headers, retry/backoff on 429 and 5xx,
//! page-walking for groups and chats, cursor-based message fetching, and
//! normalisation of GroupMe's inconsistent per-cursor message ordering so
//! callers always receive messages oldest-first.
//!
//! The write side deliberately does not generalise. Sending, deleting and
//! reacting live under three *different* `/v3` collections, editing and read
//! receipts live under `/v4`, image upload is on another host entirely, and the
//! four verbs answer 201, 204, 200 and 202 respectively. Each route is spelled
//! out and each success status checked on its own; a shared "2xx then parse
//! JSON" helper would decode a zero-length `DELETE` body.

use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::model::{
    self, Chat, DirectMessagesPage, Envelope, Group, GroupMessagesPage, Me, Message, Reaction,
};

/// One entry of `GET /v4/read_receipts`. `conversation_id` is a group id or a
/// `+`-joined DM thread key; the archive stores DMs under the other
/// participant's user id, so the caller has to map it.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadReceipt {
    pub conversation_id: String,
    #[serde(default)]
    pub last_read_message_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ReadReceiptsPage {
    /// `Option` then unwrapped by the caller rather than a `null_as_default`
    /// deserializer, because `#[serde(default)]` alone does not survive an
    /// explicit `"receipts": null` — a shape GroupMe uses freely elsewhere.
    #[serde(default)]
    receipts: Option<Vec<ReadReceipt>>,
}

pub const DEFAULT_BASE_URL: &str = "https://api.groupme.com/v3";
/// Image upload is not on `api.groupme.com` at all, and takes raw bytes rather
/// than JSON. A field rather than a literal so tests can point it at wiremock.
pub const DEFAULT_UPLOAD_URL: &str = "https://image.groupme.com/pictures";
pub const MAX_PAGE_LIMIT: u32 = 100;

const MAX_RETRIES: u32 = 3;

/// Selects which page of messages to load relative to a known message ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    /// Fetch the newest available page; no cursor parameter is sent.
    Latest,
    /// Fetch the page immediately before this ID (`before_id`).
    Before(String),
    /// Fetch the page immediately after this ID (`after_id`).
    After(String),
    /// Fetch all messages newer than this ID (`since_id`).
    Since(String),
}

/// How far a failed request may be retried.
///
/// A 429 is a *refusal*: the request never reached the handler, so replaying it
/// cannot duplicate anything. A 5xx is ambiguous — the server may have created
/// the message and then failed on the way back — so replaying a send can
/// double-post. `source_guid` is described as an idempotency key, but the
/// capture only ever showed it echoed back for matching an optimistic row; that
/// GroupMe actually deduplicates on it is unverified, and a duplicated message
/// is not something the user can take back. Sends therefore retry on 429 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Idempotent in effect: retry 429 and 5xx. Edit, delete, react, read
    /// receipt and upload all converge on the same state when replayed.
    Full,
    /// Non-idempotent: retry 429, surface 5xx to the caller.
    RateLimitOnly,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized — access token rejected")]
    Unauthorized,
    #[error("rate limited after {attempts} attempts")]
    RateLimited { attempts: u32 },
    #[error("not found")]
    NotFound,
    #[error("groupme returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(String),
    /// The response did not come from GroupMe. A filtering proxy answered
    /// instead — typically its own block page, served as HTML with a 200 so
    /// nothing in the status code betrays it.
    ///
    /// This is its own variant because the consequences are entirely different
    /// from a decode bug: retrying cannot help, the archive quietly loses an
    /// entire class of content, and the only thing that resolves it is a change
    /// to the filter's policy — which is not this app's business to attempt. It
    /// exists so the app can say what happened instead of reporting a healthy
    /// sync over an archive that is missing every group.
    #[error(
        "{path} was intercepted by a network filter at {host} — GroupMe never saw the request, \
             so this content cannot be archived until that filter permits it"
    )]
    Intercepted { path: String, host: String },
}

// Clone is cheap (reqwest::Client is an Arc internally) and lets the sync
// engine and the client-UI command surface share one configured instance.
#[derive(Clone)]
pub struct GroupMeClient {
    client: Client,
    token: String,
    base_url: String,
    upload_url: String,
    /// Base delay in ms for exponential backoff (1 s, 2 s, 4 s).
    /// Tests set this to 0 so retries don't actually sleep.
    base_delay_ms: u64,
}

impl GroupMeClient {
    pub fn new(token: impl Into<String>) -> Result<Self, ApiError> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// Overrides the base URL so tests can point at a wiremock server.
    pub fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, ApiError> {
        let client = Client::builder().user_agent("GroupMeDesktop/0.1").build()?;
        Ok(Self {
            client,
            token: token.into(),
            base_url: base_url.into(),
            upload_url: DEFAULT_UPLOAD_URL.to_string(),
            base_delay_ms: 1000,
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// `/v4` is a sibling of `/v3` on the same host, not a separate service.
    /// Derived from `base_url` rather than configured separately so one mock
    /// server serves both prefixes.
    fn v4_url(&self, path: &str) -> String {
        match self.base_url.strip_suffix("/v3") {
            Some(root) => format!("{root}/v4{path}"),
            None => format!("{}{path}", self.base_url),
        }
    }

    /// Attaches the two auth headers GroupMe requires. The token never goes in
    /// the query string — the web client sends it as a header, and so do we.
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("x-access-token", &self.token)
            .header("x-requested-with", "GroupMeWeb/1.2.3")
    }

    fn authed_get(&self, url: &str) -> reqwest::RequestBuilder {
        self.authed(self.client.get(url))
    }

    /// Sends a request, retrying up to `MAX_RETRIES` times on 429 or 5xx with
    /// exponential backoff (1×, 2×, 4× `base_delay_ms`).
    ///
    /// The closure must produce a fresh `RequestBuilder` on each call — a
    /// consumed builder cannot be resent.
    async fn send_with_retry(
        &self,
        make_req: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<Response, ApiError> {
        self.send_with_policy(Retry::Full, make_req).await
    }

    /// As [`GroupMeClient::send_with_retry`], but `policy` decides whether a
    /// 5xx is replayed. See [`Retry`].
    async fn send_with_policy(
        &self,
        policy: Retry,
        make_req: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<Response, ApiError> {
        let mut attempt = 0u32;
        loop {
            let resp = make_req().send().await?;
            let status = resp.status();

            match status {
                StatusCode::UNAUTHORIZED => return Err(ApiError::Unauthorized),
                StatusCode::NOT_FOUND => return Err(ApiError::NotFound),
                s if s == StatusCode::TOO_MANY_REQUESTS
                    || (s.is_server_error() && policy == Retry::Full) =>
                {
                    if attempt >= MAX_RETRIES {
                        return if s == StatusCode::TOO_MANY_REQUESTS {
                            Err(ApiError::RateLimited {
                                attempts: attempt + 1,
                            })
                        } else {
                            let body = resp.text().await.unwrap_or_default();
                            Err(ApiError::Status {
                                status: s.as_u16(),
                                body,
                            })
                        };
                    }
                    let delay_ms = self.base_delay_ms * (1u64 << attempt);
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    attempt += 1;
                }
                s if !s.is_success() => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Status {
                        status: s.as_u16(),
                        body,
                    });
                }
                _ => return Ok(resp),
            }
        }
    }

    pub async fn me(&self) -> Result<Me, ApiError> {
        let url = self.api_url("/users/me");
        let resp = self.send_with_retry(|| self.authed_get(&url)).await?;
        let env: Envelope<Me> = resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;
        env.response
            .ok_or_else(|| ApiError::Decode("null response for /users/me".into()))
    }

    /// Walks all pages via `per_page=100&page=N`. Stops when a page is shorter
    /// than the limit (including empty).
    pub async fn groups(&self) -> Result<Vec<Group>, ApiError> {
        self.paginate("/groups").await
    }

    /// Walks all pages via `per_page=100&page=N`.
    pub async fn chats(&self) -> Result<Vec<Chat>, ApiError> {
        self.paginate("/chats").await
    }

    async fn paginate<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, ApiError>
    where
        Vec<T>: Default,
    {
        let mut all: Vec<T> = Vec::new();
        let url = self.api_url(path);
        let mut page = 1u32;
        loop {
            let page_str = page.to_string();
            let resp = self
                .send_with_retry(|| {
                    self.authed_get(&url)
                        .query(&[("per_page", "100"), ("page", page_str.as_str())])
                })
                .await?;
            // Parsed from text rather than straight off the response so an empty
            // first page can say why. A list endpoint answering 200 with nothing
            // is indistinguishable, downstream, from an account that owns no
            // groups — and that ambiguity hid every group vanishing from the
            // archive with no error anywhere.
            let status = resp.status();
            // The *final* URL, after any redirect reqwest followed. A body of
            // HTML on a 200 usually means the request ended up somewhere other
            // than the API, and the requested URL alone cannot show that.
            let final_url = resp.url().to_string();
            let body = resp
                .text()
                .await
                .map_err(|e| ApiError::Decode(e.to_string()))?;

            // A different host answering, or an HTML body, means this never
            // reached the API. Distinguished before parsing so the failure is
            // reported as interception rather than as malformed JSON.
            if let Some(host) = intercepting_host(&url, &final_url, &body) {
                return Err(ApiError::Intercepted {
                    path: path.to_string(),
                    host,
                });
            }
            // The status and a body excerpt go into the error itself. A bare
            // "expected value at line 1 column 1" is true of an empty body, an
            // HTML error page and a gzip frame alike, and says nothing about
            // which — that ambiguity cost a full debugging pass.
            let env: Envelope<Vec<T>> = serde_json::from_str(&body).map_err(|e| {
                ApiError::Decode(format!(
                    "{path} page {page} (HTTP {status}, {} bytes, final url {final_url}): {e}; \
                     body starts: {:?}",
                    body.len(),
                    body.chars().take(120).collect::<String>()
                ))
            })?;
            let items: Vec<T> = env.response.unwrap_or_default();
            let n = items.len();
            if page == 1 && n == 0 {
                log::warn!(
                    "{path} returned no items on page 1 (HTTP {status}): {}",
                    body.chars().take(400).collect::<String>()
                );
            }
            all.extend(items);
            if n < MAX_PAGE_LIMIT as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Fetches one page of group messages, returning them **oldest-first**.
    ///
    /// GroupMe sends `since_id` results newest-first while `before_id` and
    /// `after_id` are oldest-first. We sort by `id_sort_key` defensively after
    /// every fetch so callers always receive ascending order regardless of cursor.
    pub async fn group_messages(
        &self,
        group_id: &str,
        cursor: &Cursor,
    ) -> Result<Vec<Message>, ApiError> {
        let url = self.api_url(&format!("/groups/{group_id}/messages"));
        let resp = self
            .send_with_retry(|| {
                apply_cursor(self.authed_get(&url).query(&[("limit", "100")]), cursor)
            })
            .await?;
        let page: GroupMessagesPage = decode_envelope(resp).await?;
        let mut msgs = page.messages;
        sort_ascending(&mut msgs);
        Ok(msgs)
    }

    /// Fetches one page of DM messages, returning them **oldest-first**.
    ///
    /// Reads the `direct_messages` response key, not `messages`.
    pub async fn direct_messages(
        &self,
        other_user_id: &str,
        cursor: &Cursor,
    ) -> Result<Vec<Message>, ApiError> {
        let url = self.api_url("/direct_messages");
        let resp = self
            .send_with_retry(|| {
                apply_cursor(
                    self.authed_get(&url)
                        .query(&[("other_user_id", other_user_id), ("limit", "100")]),
                    cursor,
                )
            })
            .await?;
        let page: DirectMessagesPage = decode_envelope(resp).await?;
        let mut msgs = page.direct_messages;
        sort_ascending(&mut msgs);
        Ok(msgs)
    }

    /// Downloads raw bytes from a CDN URL (i.groupme.com / m.groupme.com).
    /// Does NOT send the access token — these are public CDN assets.
    pub async fn fetch_bytes(&self, url: &str) -> Result<(Vec<u8>, Option<String>), ApiError> {
        let resp = self.client.get(url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Status { status, body });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let bytes = resp.bytes().await?.to_vec();
        Ok((bytes, content_type))
    }

    // ---------------------------------------------------------------- writes

    /// `POST /v3/groups/{id}/messages` → **201**, `response.message`.
    ///
    /// The payload is wrapped in `"message"`; the edit route below is not. That
    /// asymmetry is real.
    pub async fn send_group_message(
        &self,
        group_id: &str,
        text: &str,
        attachments: Vec<Value>,
        source_guid: &str,
    ) -> Result<Message, ApiError> {
        let url = self.api_url(&format!("/groups/{group_id}/messages"));
        let body = json!({
            "message": {
                "source_guid": source_guid,
                "text": text,
                "attachments": attachments,
            }
        });
        let resp = self
            .send_with_policy(Retry::RateLimitOnly, || {
                self.authed(self.client.post(&url)).json(&body)
            })
            .await?;
        let resp = expect_status(resp, StatusCode::CREATED).await?;
        let created: CreatedMessage = decode_required(resp).await?;
        created
            .message
            .ok_or_else(|| ApiError::Decode("send returned no message".into()))
    }

    /// `POST /v3/direct_messages` → **201**, `response.direct_message`.
    ///
    /// A different endpoint, a different envelope key on the way in *and* out,
    /// and addressed by the recipient rather than by the thread — a DM thread
    /// has no id of its own.
    pub async fn send_direct_message(
        &self,
        recipient_id: &str,
        text: &str,
        attachments: Vec<Value>,
        source_guid: &str,
    ) -> Result<Message, ApiError> {
        let url = self.api_url("/direct_messages");
        let body = json!({
            "direct_message": {
                "source_guid": source_guid,
                "recipient_id": recipient_id,
                "text": text,
                "attachments": attachments,
            }
        });
        let resp = self
            .send_with_policy(Retry::RateLimitOnly, || {
                self.authed(self.client.post(&url)).json(&body)
            })
            .await?;
        let resp = expect_status(resp, StatusCode::CREATED).await?;
        let created: CreatedDirectMessage = decode_required(resp).await?;
        created
            .direct_message
            .ok_or_else(|| ApiError::Decode("send returned no direct_message".into()))
    }

    /// `PUT /v4/groups/{id}/messages/{id}` → **200**, `response.message`.
    ///
    /// A different API version *and* a different collection noun from every
    /// other message verb. The body is a bare `{text, attachments}` with no
    /// `"message"` wrapper and no `source_guid`.
    pub async fn edit_message(
        &self,
        conversation_id: &str,
        message_id: &str,
        text: &str,
        attachments: Vec<Value>,
    ) -> Result<Message, ApiError> {
        let url = self.v4_url(&format!("/groups/{conversation_id}/messages/{message_id}"));
        let body = json!({ "text": text, "attachments": attachments });
        let resp = self
            .send_with_retry(|| self.authed(self.client.put(&url)).json(&body))
            .await?;
        let resp = expect_status(resp, StatusCode::OK).await?;
        let edited: CreatedMessage = decode_required(resp).await?;
        edited
            .message
            .ok_or_else(|| ApiError::Decode("edit returned no message".into()))
    }

    /// `DELETE /v3/conversations/{id}/messages/{id}` → **204**, zero bytes.
    ///
    /// The one route that answers without the `meta`/`response` envelope. The
    /// body is never touched: decoding it as JSON is what breaks here.
    pub async fn delete_message(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.api_url(&format!(
            "/conversations/{conversation_id}/messages/{message_id}"
        ));
        let resp = self
            .send_with_retry(|| self.authed(self.client.delete(&url)))
            .await?;
        expect_status(resp, StatusCode::NO_CONTENT).await?;
        Ok(())
    }

    /// `POST /v3/messages/{conversation_id}/{message_id}/like` → **200**,
    /// `response.reactions`.
    ///
    /// A third path shape again: under `/v3/messages/`, carrying *both* ids,
    /// and nested under neither the group nor the chat.
    ///
    /// The response is the message's whole reaction list rather than a delta,
    /// so callers overwrite rather than merge.
    pub async fn like_message(
        &self,
        conversation_id: &str,
        message_id: &str,
        code: Option<&str>,
    ) -> Result<Vec<Reaction>, ApiError> {
        let url = self.api_url(&format!("/messages/{conversation_id}/{message_id}/like"));
        // `None` is the original like button, which predates emoji reactions and
        // takes no body at all. Sending `like_icon` with a fabricated code would
        // record a reaction the user did not choose.
        let body = code.map(|c| json!({ "like_icon": { "type": "unicode", "code": c } }));
        let resp = self
            .send_with_retry(|| {
                let req = self.authed(self.client.post(&url));
                match &body {
                    Some(b) => req.json(b),
                    None => req.body(""),
                }
            })
            .await?;
        let resp = expect_status(resp, StatusCode::OK).await?;
        Ok(decode_envelope::<ReactionsBody>(resp)
            .await?
            .reactions
            .unwrap_or_default())
    }

    /// `POST /v3/messages/{conversation_id}/{message_id}/unlike` → **200**,
    /// `response.reactions` (the reactions that remain).
    ///
    /// Sent bodiless. The capture's unlike carried the same `like_icon`
    /// envelope as `like`, but a removal targets the caller's own reaction,
    /// which the access token already identifies, and this signature carries no
    /// code to put there.
    pub async fn unlike_message(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<Reaction>, ApiError> {
        let url = self.api_url(&format!("/messages/{conversation_id}/{message_id}/unlike"));
        let resp = self
            .send_with_retry(|| self.authed(self.client.post(&url)).body(""))
            .await?;
        let resp = expect_status(resp, StatusCode::OK).await?;
        Ok(decode_envelope::<ReactionsBody>(resp)
            .await?
            .reactions
            .unwrap_or_default())
    }

    /// `GET /v4/read_receipts` → the entire read-state map in one call.
    ///
    /// This is the *only* source of read state. `unread_count`,
    /// `last_read_message_id` and `last_read_at` exist on the group and chat
    /// objects but were `null` throughout the capture, so a client that trusts
    /// them shows every conversation as unread forever.
    ///
    /// One request covers every conversation — 216 entries observed — so it is
    /// cheap enough to refresh each sync cycle rather than per conversation.
    /// `conversation_id` mixes group ids with `+`-joined DM thread keys.
    /// Enveloped like the rest of the API — `{"meta":…,"response":{"receipts":…}}`
    /// — and `/v4/read_receipts` is not one of the documented escapes (§3).
    /// Decoding the top level instead yields a silent empty list, which is
    /// indistinguishable from "you have read nothing" and leaves every
    /// conversation showing unread.
    pub async fn read_receipts(&self) -> Result<Vec<ReadReceipt>, ApiError> {
        let url = self.v4_url("/read_receipts");
        let resp = self
            .send_with_retry(|| self.authed(self.client.get(&url)))
            .await?;
        let env: Envelope<ReadReceiptsPage> = resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;
        Ok(env.response.and_then(|p| p.receipts).unwrap_or_default())
    }

    /// `POST /v4/read_receipts/{conversation_id}` → **202**.
    ///
    /// 202, not 200: the write is accepted asynchronously. The body carries
    /// only `{"status": "accepted"}` and is not read.
    pub async fn mark_read(
        &self,
        conversation_id: &str,
        last_read_message_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.v4_url(&format!("/read_receipts/{conversation_id}"));
        let body = json!({ "last_read_message_id": last_read_message_id });
        let resp = self
            .send_with_retry(|| self.authed(self.client.post(&url)).json(&body))
            .await?;
        expect_status(resp, StatusCode::ACCEPTED).await?;
        Ok(())
    }

    /// `POST image.groupme.com/pictures` — raw bytes, not multipart, not JSON —
    /// returning `{"payload": {"url": …}}` outside the usual envelope.
    ///
    /// The success status is the one thing here the capture did not pin down,
    /// so any 2xx is accepted rather than guessing a number.
    pub async fn upload_image(&self, bytes: Vec<u8>, mime: &str) -> Result<String, ApiError> {
        let url = self.upload_url.clone();
        let resp = self
            .send_with_retry(|| {
                self.authed(self.client.post(&url))
                    .header(reqwest::header::CONTENT_TYPE, mime)
                    .body(bytes.clone())
            })
            .await?;
        let uploaded: UploadResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::Decode(e.to_string()))?;
        uploaded
            .payload
            .url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| ApiError::Decode("upload returned no url".into()))
    }
}

/// `POST /v3/groups/{id}/messages` and `PUT /v4/groups/{id}/messages/{id}`.
#[derive(Deserialize)]
struct CreatedMessage {
    #[serde(default)]
    message: Option<Message>,
}

/// `POST /v3/direct_messages` — same object, different key.
#[derive(Deserialize)]
struct CreatedDirectMessage {
    #[serde(default)]
    direct_message: Option<Message>,
}

#[derive(Deserialize, Default)]
struct ReactionsBody {
    #[serde(default)]
    reactions: Option<Vec<Reaction>>,
}

/// The image service answers with a bare `{"payload": …}`, not `meta`/`response`.
#[derive(Deserialize)]
struct UploadResponse {
    payload: UploadPayload,
}

#[derive(Deserialize)]
struct UploadPayload {
    #[serde(default)]
    url: Option<String>,
}

/// Rejects a 2xx that is not the one this endpoint was observed to return.
///
/// `send_with_policy` has already mapped 401, 404 and every non-2xx, so this
/// only fires when a write succeeds with an unexpected shape — at which point
/// parsing the body against the documented one would be a guess.
async fn expect_status(resp: Response, expected: StatusCode) -> Result<Response, ApiError> {
    if resp.status() == expected {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(ApiError::Status { status, body })
}

/// Decodes `Envelope<T>` where a `null` response is a protocol violation rather
/// than the empty-page terminator [`decode_envelope`] tolerates.
async fn decode_required<T: DeserializeOwned>(resp: Response) -> Result<T, ApiError> {
    let env: Envelope<T> = resp
        .json()
        .await
        .map_err(|e| ApiError::Decode(e.to_string()))?;
    env.response
        .ok_or_else(|| ApiError::Decode("null response where a body was required".into()))
}

fn apply_cursor(req: reqwest::RequestBuilder, cursor: &Cursor) -> reqwest::RequestBuilder {
    match cursor {
        Cursor::Latest => req,
        Cursor::Before(id) => req.query(&[("before_id", id.as_str())]),
        Cursor::After(id) => req.query(&[("after_id", id.as_str())]),
        Cursor::Since(id) => req.query(&[("since_id", id.as_str())]),
    }
}

fn sort_ascending(msgs: &mut [Message]) {
    msgs.sort_by_key(|m| model::id_sort_key(&m.id));
}

/// Decodes a response body as `Envelope<T>`, returning `T::default()` when
/// GroupMe sends `"response": null` — the backfill terminator at the start of
/// a conversation's history.
/// Detects a response that came from something other than the API.
///
/// Two independent signals, because either can occur alone: the request ended on
/// a different host than it was sent to (a filter redirect), or the body is an
/// HTML document where JSON was expected (a transparent proxy substituting a
/// page without redirecting). Returns the host to name in the error.
///
/// Deliberately conservative — it must not misclassify a genuine API error. Both
/// checks require positive evidence, and a JSON body always passes.
fn intercepting_host(requested: &str, final_url: &str, body: &str) -> Option<String> {
    let host_of = |u: &str| {
        u.split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(|h| h.to_ascii_lowercase())
    };
    let want = host_of(requested);
    let got = host_of(final_url);

    if let (Some(want), Some(got)) = (&want, &got) {
        if want != got {
            return Some(got.clone());
        }
    }

    let head = body.trim_start();
    let looks_like_html = head.len() >= 5
        && (head[..5].eq_ignore_ascii_case("<html")
            || head
                .get(..9)
                .is_some_and(|s| s.eq_ignore_ascii_case("<!doctype")));
    if looks_like_html {
        return Some(
            got.or(want)
                .unwrap_or_else(|| "an unknown host".to_string()),
        );
    }
    None
}

async fn decode_envelope<T: DeserializeOwned + Default>(resp: Response) -> Result<T, ApiError> {
    let env: Envelope<T> = resp
        .json()
        .await
        .map_err(|e| ApiError::Decode(e.to_string()))?;
    Ok(env.response.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn a_filter_block_page_is_reported_as_interception_not_bad_json() {
        // The real shape of this: a content filter answered /v3/groups with its
        // own page, HTTP 200, on its own host. Parsed as JSON it is only
        // "expected value at line 1 column 1", which reads like our bug and
        // silently cost every group in the archive.
        let host = intercepting_host(
            "https://api.groupme.com/v3/groups",
            "https://filter.example.com/?error=access",
            "<!doctype html>\n<html lang=\"en\">",
        );
        assert_eq!(host.as_deref(), Some("filter.example.com"));

        // A transparent proxy that substitutes a page without redirecting.
        assert_eq!(
            intercepting_host(
                "https://api.groupme.com/v3/groups",
                "https://api.groupme.com/v3/groups",
                "<html><body>Blocked</body></html>",
            )
            .as_deref(),
            Some("api.groupme.com")
        );

        // Genuine API responses must never be misread as interception — an
        // error envelope is still the API talking, and needs its own handling.
        assert!(intercepting_host(
            "https://api.groupme.com/v3/groups",
            "https://api.groupme.com/v3/groups",
            r#"{"meta":{"code":200},"response":[]}"#,
        )
        .is_none());
        assert!(intercepting_host(
            "https://api.groupme.com/v3/groups",
            "https://api.groupme.com/v3/groups",
            r#"{"meta":{"code":404,"errors":["not found"]}}"#,
        )
        .is_none());
    }

    /// Synthetic throughout — this repository is public and GroupMe user ids
    /// are stable and correlatable.
    const GROUP: &str = "10000001";
    const USER: &str = "20000001";
    const MSG: &str = "170000000000000001";
    const DM_KEY: &str = "10000001+20000001";

    fn client(server: &MockServer) -> GroupMeClient {
        let mut c = GroupMeClient::with_base_url("test-token", server.uri()).unwrap();
        // Skip real sleeps so retries complete instantly.
        c.base_delay_ms = 0;
        c.upload_url = format!("{}/pictures", server.uri());
        c
    }

    // -------------------------------------------------------------------------
    // me()
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn me_parses_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"id": "123", "name": "Test User"}
            })))
            .mount(&server)
            .await;

        let me = client(&server).me().await.unwrap();
        assert_eq!(me.id, "123");
        assert_eq!(me.name.as_deref(), Some("Test User"));
    }

    // -------------------------------------------------------------------------
    // groups() — pagination
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn groups_paginates_two_pages_stops_on_short() {
        let server = MockServer::start().await;

        let full: Vec<serde_json::Value> =
            (0u32..100).map(|i| json!({"id": i.to_string()})).collect();
        let partial = vec![json!({"id": "200"}), json!({"id": "201"})];

        Mock::given(method("GET"))
            .and(path("/groups"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": full
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/groups"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": partial
            })))
            .mount(&server)
            .await;

        let groups = client(&server).groups().await.unwrap();
        assert_eq!(groups.len(), 102);
    }

    // -------------------------------------------------------------------------
    // group_messages() — cursor variants
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn group_messages_latest_sends_limit_no_cursor_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/groups/42/messages"))
            .and(query_param("limit", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"count": 1, "messages": [{"id": "1", "created_at": 1000}]}
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("42", &Cursor::Latest)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn group_messages_before_sends_before_id_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/groups/42/messages"))
            .and(query_param("before_id", "999"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"count": 1, "messages": [{"id": "998", "created_at": 900}]}
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("42", &Cursor::Before("999".into()))
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "998");
    }

    /// Regression: GroupMe sends `since_id` results newest-first, but every other
    /// cursor is oldest-first. We must normalise to oldest-first in all cases.
    #[tokio::test]
    async fn group_messages_since_normalises_to_oldest_first() {
        let server = MockServer::start().await;
        // Server intentionally returns newest-first: 300, 200, 100.
        Mock::given(method("GET"))
            .and(path("/groups/42/messages"))
            .and(query_param("since_id", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {
                    "count": 3,
                    "messages": [
                        {"id": "300", "created_at": 3000},
                        {"id": "200", "created_at": 2000},
                        {"id": "100", "created_at": 1000}
                    ]
                }
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("42", &Cursor::Since("50".into()))
            .await
            .unwrap();

        // Must come back as 100 → 200 → 300 (ascending).
        assert_eq!(msgs[0].id, "100");
        assert_eq!(msgs[1].id, "200");
        assert_eq!(msgs[2].id, "300");
        for w in msgs.windows(2) {
            assert!(
                model::id_sort_key(&w[0].id) < model::id_sort_key(&w[1].id),
                "messages must be strictly ascending by id_sort_key"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Empty and null response — backfill terminator
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn empty_page_returns_ok_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/groups/1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"count": 0, "messages": []}
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("1", &Cursor::Latest)
            .await
            .unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn null_response_returns_ok_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/groups/1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": null
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("1", &Cursor::Latest)
            .await
            .unwrap();
        assert!(msgs.is_empty());
    }

    // -------------------------------------------------------------------------
    // Error mapping
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn status_401_returns_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = client(&server).me().await.unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized));
    }

    // -------------------------------------------------------------------------
    // Retry / backoff
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn rate_limited_once_then_200_succeeds() {
        let server = MockServer::start().await;

        // Mount order is the tie-break: the first mock that still matches wins.
        // The 429 goes first and is exhausted after one response, which leaves
        // the 200 to answer the retry. Mounting them the other way round means
        // the 429 never fires and the test passes without retrying anything.
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"id": "1"}
            })))
            .mount(&server)
            .await;

        let me = client(&server).me().await.unwrap();
        assert_eq!(me.id, "1");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the 429 must actually have been served and retried"
        );
    }

    #[tokio::test]
    async fn rate_limited_four_times_returns_rate_limited_error() {
        let server = MockServer::start().await;
        // Always 429 — exhausts all retries.
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let err = client(&server).me().await.unwrap_err();
        // MAX_RETRIES=3 means 4 total attempts before giving up.
        assert!(matches!(err, ApiError::RateLimited { attempts: 4 }));
    }

    // -------------------------------------------------------------------------
    // Robust parsing
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn message_with_null_text_and_unknown_attachment_parses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/groups/1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {
                    "count": 1,
                    "messages": [{
                        "id": "1",
                        "text": null,
                        "attachments": [{"type": "quantum_sticker", "foo": 42}]
                    }]
                }
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .group_messages("1", &Cursor::Latest)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text.is_none());
        // An unknown attachment reports its real wire type rather than a
        // generic "other" — the payload is preserved verbatim rather than
        // collapsed, so the type string survives with it.
        assert_eq!(msgs[0].attachments[0].kind(), "quantum_sticker");
    }

    // -------------------------------------------------------------------------
    // direct_messages() — reads the right response key
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn direct_messages_reads_direct_messages_key_not_messages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/direct_messages"))
            .and(query_param("other_user_id", "99"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {
                    "count": 2,
                    "direct_messages": [
                        {"id": "10", "created_at": 1000},
                        {"id": "20", "created_at": 2000}
                    ]
                }
            })))
            .mount(&server)
            .await;

        let msgs = client(&server)
            .direct_messages("99", &Cursor::Latest)
            .await
            .unwrap();
        assert_eq!(msgs.len(), 2);
        // Oldest-first: 10 before 20.
        assert_eq!(msgs[0].id, "10");
        assert_eq!(msgs[1].id, "20");
    }

    // -------------------------------------------------------------------------
    // Routing — the write side shares no path shape
    // -------------------------------------------------------------------------

    /// `/v4` is a sibling prefix on the same host, so it has to be derived from
    /// `base_url` rather than hardcoded, or one mock server cannot serve both.
    #[test]
    fn v4_routes_are_derived_from_the_base_url() {
        let c = GroupMeClient::new("t").unwrap();
        assert_eq!(
            c.v4_url("/read_receipts/1"),
            "https://api.groupme.com/v4/read_receipts/1"
        );
        assert_eq!(c.api_url("/chats"), "https://api.groupme.com/v3/chats");

        // A base without the version suffix — a mock server — is used verbatim,
        // which is what lets both prefixes land on the same wiremock instance.
        let c = GroupMeClient::with_base_url("t", "http://127.0.0.1:9").unwrap();
        assert_eq!(
            c.v4_url("/read_receipts/1"),
            "http://127.0.0.1:9/read_receipts/1"
        );
    }

    // -------------------------------------------------------------------------
    // send_group_message() / send_direct_message()
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn sending_to_a_group_wraps_the_payload_and_expects_201() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .and(header("x-access-token", "test-token"))
            .and(body_json(json!({
                "message": {
                    "source_guid": "guid-1",
                    "text": "hello",
                    "attachments": [{"type": "image", "url": "https://example.invalid/a.png"}]
                }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "meta": {"code": 201},
                "response": {"message": {
                    "id": MSG, "source_guid": "guid-1", "group_id": GROUP,
                    "user_id": USER, "text": "hello", "created_at": 1785301508
                }}
            })))
            .mount(&server)
            .await;

        let att = vec![json!({"type": "image", "url": "https://example.invalid/a.png"})];
        let msg = client(&server)
            .send_group_message(GROUP, "hello", att, "guid-1")
            .await
            .unwrap();
        assert_eq!(msg.id, MSG);
        // The echo is what matches an optimistic local row to the server's copy.
        assert_eq!(msg.source_guid.as_deref(), Some("guid-1"));
    }

    /// A different endpoint, a different request envelope, and a different
    /// response key from the group form. None of it generalises.
    #[tokio::test]
    async fn sending_a_dm_uses_direct_messages_and_a_recipient_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/direct_messages"))
            .and(body_json(json!({
                "direct_message": {
                    "source_guid": "guid-2",
                    "recipient_id": USER,
                    "text": "hi",
                    "attachments": []
                }
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "meta": {"code": 201},
                "response": {"direct_message": {
                    "id": MSG, "source_guid": "guid-2",
                    "conversation_id": DM_KEY, "recipient_id": USER, "text": "hi"
                }}
            })))
            .mount(&server)
            .await;

        let msg = client(&server)
            .send_direct_message(USER, "hi", Vec::new(), "guid-2")
            .await
            .unwrap();
        assert_eq!(msg.id, MSG);
        assert_eq!(msg.conversation_id.as_deref(), Some(DM_KEY));
    }

    /// The non-idempotent case. A 5xx may mean the message was created and the
    /// response lost, so replaying it can post twice — which the user cannot
    /// take back. One attempt, then the error.
    #[tokio::test]
    async fn a_send_is_never_replayed_after_a_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let err = client(&server)
            .send_group_message(GROUP, "hello", Vec::new(), "guid-3")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Status { status: 502, .. }));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a 5xx on a send must not be retried — the write may already have landed"
        );
    }

    /// A 429 is a refusal, not an ambiguous outcome: nothing was created, so
    /// replaying it cannot duplicate anything.
    #[tokio::test]
    async fn a_send_is_replayed_after_429_because_nothing_was_created() {
        let server = MockServer::start().await;
        // Mounted first, and exhausted after one response, so it answers the
        // first attempt and the 201 below answers the retry.
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "meta": {"code": 201},
                "response": {"message": {"id": MSG, "source_guid": "guid-4"}}
            })))
            .mount(&server)
            .await;

        let msg = client(&server)
            .send_group_message(GROUP, "hello", Vec::new(), "guid-4")
            .await
            .unwrap();
        assert_eq!(msg.id, MSG);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_unexpected_success_status_on_a_send_is_an_error_not_a_parse() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200}, "response": {"message": {"id": MSG}}
            })))
            .mount(&server)
            .await;

        let err = client(&server)
            .send_group_message(GROUP, "hello", Vec::new(), "guid-5")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Status { status: 200, .. }));
    }

    // -------------------------------------------------------------------------
    // edit_message() — /v4, and a bare body
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn editing_puts_to_v4_with_an_unwrapped_body() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path(format!("/groups/{GROUP}/messages/{MSG}")))
            .and(header("x-access-token", "test-token"))
            // Not wrapped in "message", and carrying no source_guid — unlike the
            // create route on the same object.
            .and(body_json(json!({"text": "edited", "attachments": []})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"message": {
                    "id": MSG, "group_id": GROUP, "text": "edited",
                    "created_at": 1785303377, "updated_at": 1785303382
                }}
            })))
            .mount(&server)
            .await;

        let msg = client(&server)
            .edit_message(GROUP, MSG, "edited", Vec::new())
            .await
            .unwrap();
        assert_eq!(msg.text.as_deref(), Some("edited"));
    }

    // -------------------------------------------------------------------------
    // delete_message() — 204, zero-length body
    // -------------------------------------------------------------------------

    /// The one route with no envelope at all. Decoding JSON unconditionally on
    /// the message routes throws exactly here.
    #[tokio::test]
    async fn deleting_accepts_204_with_a_zero_length_body() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path(format!("/conversations/{GROUP}/messages/{MSG}")))
            .and(header("x-access-token", "test-token"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_message(GROUP, MSG).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_a_message_that_is_gone_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path(format!("/conversations/{GROUP}/messages/{MSG}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client(&server)
            .delete_message(GROUP, MSG)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotFound));
    }

    // -------------------------------------------------------------------------
    // like_message() / unlike_message()
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn reacting_posts_the_like_icon_and_returns_the_whole_list() {
        let server = MockServer::start().await;
        // Note the path shape: under /messages/, carrying both ids.
        Mock::given(method("POST"))
            .and(path(format!("/messages/{GROUP}/{MSG}/like")))
            .and(body_json(
                json!({"like_icon": {"type": "unicode", "code": "🤣"}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"reactions": [
                    {"type": "unicode", "pack_id": 0, "pack_index": 0,
                     "code": "🤣", "user_ids": [USER]}
                ]}
            })))
            .mount(&server)
            .await;

        let reactions = client(&server)
            .like_message(GROUP, MSG, Some("🤣"))
            .await
            .unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].display_char(), Some("🤣"));
        // Integers on the write path, strings on the read path. Both normalise.
        assert_eq!(reactions[0].pack_id().as_deref(), Some("0"));
    }

    /// The original like button predates emoji reactions and takes no body.
    /// Inventing a `like_icon` code would record a reaction the user did not pick.
    #[tokio::test]
    async fn a_plain_like_sends_no_body_at_all() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/messages/{GROUP}/{MSG}/like")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"reactions": []}
            })))
            .mount(&server)
            .await;

        let reactions = client(&server)
            .like_message(GROUP, MSG, None)
            .await
            .unwrap();
        assert!(reactions.is_empty());
        let sent = server.received_requests().await.unwrap();
        assert!(sent[0].body.is_empty(), "a plain like carries no payload");
    }

    #[tokio::test]
    async fn unliking_returns_the_reactions_that_remain() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/messages/{DM_KEY}/{MSG}/unlike")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"reactions": [
                    {"type": "unicode", "code": "👍", "user_ids": ["20000002"]}
                ]}
            })))
            .mount(&server)
            .await;

        let reactions = client(&server).unlike_message(DM_KEY, MSG).await.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].user_ids, vec!["20000002".to_string()]);
    }

    /// `"reactions": null` and `"response": null` both mean "none left".
    #[tokio::test]
    async fn a_null_reaction_list_is_empty_not_a_decode_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/messages/{GROUP}/{MSG}/unlike")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200}, "response": {"reactions": null}
            })))
            .mount(&server)
            .await;

        assert!(client(&server)
            .unlike_message(GROUP, MSG)
            .await
            .unwrap()
            .is_empty());
    }

    // -------------------------------------------------------------------------
    // mark_read() — 202, and on /v4
    // -------------------------------------------------------------------------

    /// 202, not 200: the write is accepted asynchronously.
    #[tokio::test]
    async fn marking_read_posts_to_v4_and_accepts_202() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/read_receipts/{DM_KEY}")))
            .and(header("x-access-token", "test-token"))
            .and(body_json(json!({"last_read_message_id": MSG})))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"status": "accepted"})))
            .mount(&server)
            .await;

        client(&server).mark_read(DM_KEY, MSG).await.unwrap();
    }

    #[tokio::test]
    async fn a_read_receipt_retries_after_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/read_receipts/{GROUP}")))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/read_receipts/{GROUP}")))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"status": "accepted"})))
            .mount(&server)
            .await;

        client(&server).mark_read(GROUP, MSG).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    // -------------------------------------------------------------------------
    // upload_image() — a different host, raw bytes, no envelope
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn uploading_posts_raw_bytes_and_returns_the_payload_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/pictures"))
            .and(header("content-type", "image/png"))
            .and(header("x-access-token", "test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                // Not the meta/response envelope — this service has its own shape.
                "payload": {"url": "https://i.groupme.com/100x100.png.abc"}
            })))
            .mount(&server)
            .await;

        let bytes = vec![0x89, 0x50, 0x4e, 0x47];
        let url = client(&server)
            .upload_image(bytes.clone(), "image/png")
            .await
            .unwrap();
        assert_eq!(url, "https://i.groupme.com/100x100.png.abc");
        assert_eq!(server.received_requests().await.unwrap()[0].body, bytes);
    }

    #[tokio::test]
    async fn an_upload_with_no_url_in_the_payload_is_a_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/pictures"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"payload": {}})))
            .mount(&server)
            .await;

        let err = client(&server)
            .upload_image(vec![1, 2, 3], "image/png")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Decode(_)));
    }

    /// An upload creates an inert blob rather than posting to a conversation,
    /// so replaying it duplicates nothing and it keeps the full retry policy.
    #[tokio::test]
    async fn an_upload_retries_after_a_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/pictures"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/pictures"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "payload": {"url": "https://i.groupme.com/100x100.png.abc"}
            })))
            .mount(&server)
            .await;

        let url = client(&server)
            .upload_image(vec![1, 2, 3], "image/jpeg")
            .await
            .unwrap();
        assert_eq!(url, "https://i.groupme.com/100x100.png.abc");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the request body must be rebuilt for each attempt"
        );
    }

    // -------------------------------------------------------------------------
    // Auth mapping applies to writes too
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn a_rejected_token_on_a_write_maps_to_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/groups/{GROUP}/messages")))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = client(&server)
            .send_group_message(GROUP, "hello", Vec::new(), "guid-6")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized));
    }
}
