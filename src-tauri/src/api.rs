//! HTTP client for the GroupMe v3 REST API.
//!
//! `GroupMeClient` handles auth headers, retry/backoff on 429 and 5xx,
//! page-walking for groups and chats, cursor-based message fetching, and
//! normalisation of GroupMe's inconsistent per-cursor message ordering so
//! callers always receive messages oldest-first.

use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;

use crate::model::{
    self, Chat, DirectMessagesPage, Envelope, Group, GroupMessagesPage, Me, Message,
};

pub const DEFAULT_BASE_URL: &str = "https://api.groupme.com/v3";
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
}

pub struct GroupMeClient {
    client: Client,
    token: String,
    base_url: String,
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
            base_delay_ms: 1000,
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Attaches the two auth headers GroupMe requires. The token never goes in
    /// the query string — the web client sends it as a header, and so do we.
    fn authed_get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("x-access-token", &self.token)
            .header("x-requested-with", "GroupMeWeb/1.2.3")
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
        let mut attempt = 0u32;
        loop {
            let resp = make_req().send().await?;
            let status = resp.status();

            match status {
                StatusCode::UNAUTHORIZED => return Err(ApiError::Unauthorized),
                StatusCode::NOT_FOUND => return Err(ApiError::NotFound),
                s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
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
            let items: Vec<T> = decode_envelope(resp).await?;
            let n = items.len();
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> GroupMeClient {
        let mut c = GroupMeClient::with_base_url("test-token", server.uri()).unwrap();
        // Skip real sleeps so retries complete instantly.
        c.base_delay_ms = 0;
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

        // Register success mock first so it has lower LIFO priority.
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "meta": {"code": 200},
                "response": {"id": "1"}
            })))
            .mount(&server)
            .await;

        // Register 429 mock second so it wins on the first request (LIFO),
        // then is exhausted, leaving the 200 mock to handle retries.
        Mock::given(method("GET"))
            .and(path("/users/me"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let me = client(&server).me().await.unwrap();
        assert_eq!(me.id, "1");
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
}
