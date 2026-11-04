use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client as HttpClient, StatusCode};
use serde::de::DeserializeOwned;
use url::Url;

use crate::models::{
    Item, ItemRefs, StreamContents, Subscription, SubscriptionList, Tag, TagList, UnreadCounts,
};
use crate::{Error, Result, http_body};

/// Turns a server or API URL into the greader API base.
pub fn normalize_api_url(input: &str) -> Result<String> {
    let mut base = input.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err(Error::Other("API URL is empty".into()));
    }
    if base.ends_with("/api") {
        base.push_str("/greader.php");
    } else if !base.ends_with("/greader.php") {
        base.push_str("/api/greader.php");
    }
    Url::parse(&base)?;
    Ok(base)
}

/// API bases to try for a server or API URL.
///
/// FreshRSS serves its API at `<root>/api/greader.php` in some installations
/// and at `<root>/p/api/greader.php` in others. Users may also paste the web
/// UI or API-page URL. Return the canonical normalization first, then both
/// layouts, and let the caller try the next candidate only when a path is not
/// found.
fn login_candidates(input: &str) -> Result<Vec<String>> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Other("API URL is empty".into()));
    }
    let mut url = Url::parse(trimmed)?;
    url.set_query(None);
    url.set_fragment(None);

    let mut candidates = Vec::new();
    push_candidate(&mut candidates, normalize_api_url(url.as_str())?);

    let path = url.path().trim_end_matches('/');
    if let Some(index) = path.find("/greader.php") {
        // The input is already an API URL, possibly with an endpoint such as
        // `/accounts/ClientLogin` appended.
        let mut endpoint = url.clone();
        endpoint.set_path(&path[..index + "/greader.php".len()]);
        return Ok(vec![endpoint.to_string()]);
    }

    // Strip the FreshRSS web UI/API suffixes to find the installation root.
    let mut root = path;
    loop {
        let stripped = root
            .strip_suffix("/i")
            .or_else(|| root.strip_suffix("/p"))
            .or_else(|| root.strip_suffix("/api"));
        match stripped {
            Some(value) => root = value,
            None => break,
        }
    }
    for suffix in ["/p/api/greader.php", "/api/greader.php"] {
        let mut candidate = url.clone();
        candidate.set_path(&format!("{root}{suffix}"));
        push_candidate(&mut candidates, candidate.to_string());
    }
    Ok(candidates)
}

fn push_candidate(candidates: &mut Vec<String>, candidate: String) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

const MAX_LOGIN_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 64 * 1024;
const MAX_WRITE_BYTES: usize = 1024 * 1024;
const MAX_API_BYTES: usize = 32 * 1024 * 1024;

fn build_http() -> Result<HttpClient> {
    // The total timeout also covers reading the body, so it must fit a 32 MB page on a slow link.
    Ok(HttpClient::builder()
        .user_agent(concat!("Nieuwser/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}

pub struct Client {
    http: HttpClient,
    base: String,
    auth: String,
    token: Option<String>,
}

impl Client {
    pub async fn login(api_url: &str, email: &str, password: &str) -> Result<Self> {
        let candidates = login_candidates(api_url)?;
        let http = build_http()?;
        let request_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("Email", email)
            .append_pair("Passwd", password)
            .finish();

        let mut last_error = None;
        for base in candidates {
            let response = http
                .post(format!("{base}/accounts/ClientLogin"))
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(request_body.clone())
                .send()
                .await?;
            let status = response.status();
            let response_body =
                http_body::text(response, MAX_LOGIN_BYTES, "login response").await?;

            if status.is_success() {
                if let Some(auth) = response_body
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("Auth="))
                    .map(str::to_string)
                {
                    return Ok(Self {
                        http,
                        base,
                        auth,
                        token: None,
                    });
                }
                // A catch-all page may answer 200 for a wrong API path.
                last_error = Some(Error::Other(format!(
                    "no Auth token in login response from {base}: {}",
                    clip(&response_body, 500)
                )));
                continue;
            }

            let error = Error::Http {
                status,
                body: clip(&response_body, 500),
            };
            if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
                last_error = Some(error);
                continue;
            }
            return Err(error);
        }
        Err(last_error.unwrap_or_else(|| Error::Other("no greader API endpoint found".into())))
    }

    pub fn with_auth(api_url: &str, auth: String) -> Result<Self> {
        Ok(Self {
            http: build_http()?,
            base: normalize_api_url(api_url)?,
            auth,
            token: None,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn auth_token(&self) -> &str {
        &self.auth
    }

    fn auth_header(&self) -> String {
        format!("GoogleLogin auth={}", self.auth)
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        Ok(Url::parse(&format!("{}{path}", self.base))?)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        let mut url = self.endpoint(path)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("output", "json");
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        let response = self
            .http
            .get(url)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await?;
        let status = response.status();
        let body = http_body::text(response, MAX_API_BYTES, "greader response").await?;
        if !status.is_success() {
            return Err(Error::Http {
                status,
                body: clip(&body, 500),
            });
        }
        serde_json::from_str(&body).map_err(|e| {
            Error::Other(format!(
                "invalid JSON from {path}: {e}; body: {}",
                clip(&body, 500)
            ))
        })
    }

    async fn token(&mut self) -> Result<String> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        let response = self
            .http
            .get(self.endpoint("/reader/api/0/token")?)
            .header(AUTHORIZATION, self.auth_header())
            .send()
            .await?;
        let status = response.status();
        let body = http_body::text(response, MAX_TOKEN_BYTES, "CSRF token response").await?;
        if !status.is_success() {
            return Err(Error::Http {
                status,
                body: clip(&body, 500),
            });
        }
        let token = body.trim().to_string();
        if token.is_empty() {
            return Err(Error::Other("empty CSRF token".into()));
        }
        self.token = Some(token.clone());
        Ok(token)
    }

    /// POST with the CSRF token. Retries once with a fresh token on 401/403.
    async fn write(&mut self, path: &str, pairs: Vec<(String, String)>) -> Result<String> {
        for attempt in 0..2 {
            let token = self.token().await?;
            let mut all = pairs.clone();
            all.push(("T".into(), token));
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(all.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .finish();

            let response = self
                .http
                .post(self.endpoint(path)?)
                .header(AUTHORIZATION, self.auth_header())
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await?;
            let status = response.status();
            let text = http_body::text(response, MAX_WRITE_BYTES, "write response").await?;

            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                self.token = None;
                if attempt == 0 {
                    continue;
                }
            }
            if !status.is_success() {
                return Err(Error::Http {
                    status,
                    body: clip(&text, 500),
                });
            }
            return Ok(text);
        }
        unreachable!("write retries are bounded")
    }

    pub async fn subscriptions(&self) -> Result<Vec<Subscription>> {
        let list: SubscriptionList = self
            .get_json("/reader/api/0/subscription/list", &[])
            .await?;
        Ok(list.subscriptions)
    }

    pub async fn tags(&self) -> Result<Vec<Tag>> {
        let list: TagList = self.get_json("/reader/api/0/tag/list", &[]).await?;
        Ok(list.tags)
    }

    pub async fn unread_counts(&self) -> Result<UnreadCounts> {
        self.get_json("/reader/api/0/unread-count", &[]).await
    }

    pub async fn stream_contents(
        &self,
        stream: &str,
        n: u32,
        nt: Option<i64>,
        ot: Option<i64>,
        c: Option<&str>,
        xt: Option<&str>,
    ) -> Result<StreamContents> {
        let mut params = vec![("n", n.to_string()), ("r", "n".into())];
        if let Some(value) = nt {
            params.push(("nt", value.to_string()));
        }
        if let Some(value) = ot {
            params.push(("ot", value.to_string()));
        }
        if let Some(value) = c {
            params.push(("c", value.to_string()));
        }
        if let Some(value) = xt {
            params.push(("xt", value.to_string()));
        }
        self.get_json(&format!("/reader/api/0/stream/contents/{stream}"), &params)
            .await
    }

    pub async fn item_ids(
        &self,
        stream: &str,
        n: u32,
        nt: Option<i64>,
        c: Option<&str>,
        xt: Option<&str>,
    ) -> Result<ItemRefs> {
        let mut params = vec![("s", stream.to_string()), ("n", n.to_string())];
        if let Some(value) = nt {
            params.push(("nt", value.to_string()));
        }
        if let Some(value) = c {
            params.push(("c", value.to_string()));
        }
        if let Some(value) = xt {
            params.push(("xt", value.to_string()));
        }
        self.get_json("/reader/api/0/stream/items/ids", &params)
            .await
    }

    pub async fn item_contents(&self, ids: &[String]) -> Result<Vec<Item>> {
        let mut url = self.endpoint("/reader/api/0/stream/items/contents")?;
        url.query_pairs_mut().append_pair("output", "json");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(ids.iter().map(|id| ("i", id.as_str())))
            .finish();
        let response = self
            .http
            .post(url)
            .header(AUTHORIZATION, self.auth_header())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let text = http_body::text(response, MAX_API_BYTES, "item contents response").await?;
        if !status.is_success() {
            return Err(Error::Http {
                status,
                body: clip(&text, 500),
            });
        }
        let contents: StreamContents = serde_json::from_str(&text).map_err(|e| {
            Error::Other(format!(
                "invalid item contents JSON: {e}; body: {}",
                clip(&text, 500)
            ))
        })?;
        Ok(contents.items)
    }

    pub async fn edit_tag(
        &mut self,
        item_ids: &[String],
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        let mut pairs = vec![("ac".into(), "edit-tags".into())];
        for id in item_ids {
            pairs.push(("i".into(), id.clone()));
        }
        for tag in add {
            pairs.push(("a".into(), (*tag).into()));
        }
        for tag in remove {
            pairs.push(("r".into(), (*tag).into()));
        }
        self.write("/reader/api/0/edit-tag", pairs).await?;
        Ok(())
    }

    pub async fn mark_all_read(&mut self, stream: &str, ts_us: i64) -> Result<()> {
        self.write(
            "/reader/api/0/mark-all-as-read",
            vec![
                ("s".into(), stream.into()),
                ("ts".into(), ts_us.to_string()),
            ],
        )
        .await?;
        Ok(())
    }
}

fn clip(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn api_url_normalization() {
        assert_eq!(
            normalize_api_url("https://example.com").unwrap(),
            "https://example.com/api/greader.php"
        );
        assert_eq!(
            normalize_api_url("https://example.com/api/greader.php/").unwrap(),
            "https://example.com/api/greader.php"
        );
        assert_eq!(
            normalize_api_url("https://example.com/api/").unwrap(),
            "https://example.com/api/greader.php"
        );
        assert_eq!(
            normalize_api_url("https://example.com/feeds/p/api/").unwrap(),
            "https://example.com/feeds/p/api/greader.php"
        );
        assert_eq!(
            normalize_api_url("https://example.com/feeds/p/api/greader.php").unwrap(),
            "https://example.com/feeds/p/api/greader.php"
        );
    }

    #[test]
    fn login_candidates_cover_freshrss_layouts() {
        assert_eq!(
            login_candidates("https://example.com/feeds/").unwrap(),
            [
                "https://example.com/feeds/api/greader.php",
                "https://example.com/feeds/p/api/greader.php",
            ]
        );
        assert_eq!(
            login_candidates("https://example.com/feeds/p/i/").unwrap(),
            [
                "https://example.com/feeds/p/i/api/greader.php",
                "https://example.com/feeds/p/api/greader.php",
                "https://example.com/feeds/api/greader.php",
            ]
        );
        assert_eq!(
            login_candidates("https://example.com/feeds/p/api/greader.php").unwrap(),
            ["https://example.com/feeds/p/api/greader.php"]
        );
        assert_eq!(
            login_candidates("https://example.com/feeds/p/api/greader.php/accounts/ClientLogin")
                .unwrap(),
            ["https://example.com/feeds/p/api/greader.php"]
        );
    }

    #[tokio::test]
    async fn login_and_write() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_string("SID=abc\nAuth=secret\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("csrf"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/greader.php/reader/api/0/edit-tag"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;

        let mut client = Client::login(&server.uri(), "me", "pw").await.unwrap();
        client
            .edit_tag(
                &["1".into(), "2".into()],
                &["user/-/state/com.google/read"],
                &[],
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[2].body.clone()).unwrap();
        assert!(body.contains("i=1"));
        assert!(body.contains("i=2"));
        assert!(body.contains("a=user%2F-%2Fstate%2Fcom.google%2Fread"));
        assert!(body.contains("T=csrf"));
    }

    #[tokio::test]
    async fn login_falls_back_to_p_api_for_subdirectory_installs() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/feeds/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/feeds/p/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Auth=secret\n"))
            .mount(&server)
            .await;

        let client = Client::login(&format!("{}/feeds/", server.uri()), "me", "pw")
            .await
            .unwrap();
        assert_eq!(
            client.base_url(),
            format!("{}/feeds/p/api/greader.php", server.uri())
        );
    }

    #[tokio::test]
    async fn login_does_not_retry_another_path_after_auth_failure() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/feeds/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/feeds/p/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Auth=secret\n"))
            .mount(&server)
            .await;

        assert!(
            Client::login(&format!("{}/feeds/", server.uri()), "me", "wrong")
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn parses_stream_contents() {
        let body = json!({
            "items": [{
                "id": "123",
                "title": "Hello",
                "published": 1700000000,
                "timestampUsec": "1700000000123456",
                "alternate": [{"href": "https://example.com/post"}],
                "content": {"content": "<p>Body</p>"},
                "categories": ["user/-/state/com.google/read"],
                "origin": {"streamId": "feed/7", "title": "Feed"}
            }],
            "continuation": "120",
            "updated": 1700000000
        });
        let contents: StreamContents = serde_json::from_value(body).unwrap();
        let item = &contents.items[0];
        assert_eq!(item.short_id(), "123");
        assert_eq!(item.usec(), 1_700_000_000_123_456);
        assert!(item.is_read());
        assert_eq!(item.body_html(), "<p>Body</p>");
        assert_eq!(contents.continuation.as_deref(), Some("120"));
        assert_eq!(contents.updated, Some(1_700_000_000));
    }
}
