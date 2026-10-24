use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::api::Client;
use crate::models::{Stream, UnreadCounts};
use crate::store::{DEFAULT_CACHE_BYTES, OutboxOp, Store};
use crate::{Error, Result};

pub struct SyncEngine {
    client: Client,
    store: Arc<Store>,
}

const WATERMARK_OVERLAP_SECONDS: i64 = 5 * 60;
const CONTENT_PAGE_SIZE: u32 = 500;
const CONTENT_BATCH_SIZE: usize = 50;
const INITIAL_SYNC_ITEMS: usize = 5_000;
const INITIAL_STARRED_ITEMS: usize = 1_000;
const BACKFILL_LIMIT: usize = 1_000;
const MAX_PAGES_PER_STREAM: usize = 1_000;
const TAG_READ: &str = "user/-/state/com.google/read";
const TAG_STARRED: &str = "user/-/state/com.google/starred";
const ITEM_TAG_PREFIX: &str = "tag:google.com,2005:reader/item/";

#[derive(Debug, Default)]
pub struct SyncReport {
    pub pushed_ops: usize,
    pub new_items: usize,
    pub starred_items: usize,
    pub backfilled: usize,
    pub failed_ops: i64,
    pub evicted: usize,
    pub feeds: usize,
    pub unread: i64,
    pub duration_ms: u128,
}

impl fmt::Display for SyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pushed {} ops, {} new items, {} starred, {} feeds, {} unread",
            self.pushed_ops, self.new_items, self.starred_items, self.feeds, self.unread
        )?;
        if self.backfilled > 0 {
            write!(f, ", {} backfilled", self.backfilled)?;
        }
        if self.failed_ops > 0 {
            write!(f, ", {} failed ops", self.failed_ops)?;
        }
        if self.evicted > 0 {
            write!(f, ", {} bodies evicted", self.evicted)?;
        }
        write!(f, ", {} ms", self.duration_ms)
    }
}

impl SyncEngine {
    pub async fn new(
        store: Arc<Store>,
        base_url: &str,
        email: &str,
        password: &str,
    ) -> Result<Self> {
        let client = Client::login(base_url, email, password).await?;
        store.set_account(client.base_url(), email)?;
        store.set_auth_token(Some(client.auth_token()))?;
        Ok(Self { client, store })
    }

    pub fn with_auth(store: Arc<Store>, base_url: &str, auth_token: &str) -> Result<Self> {
        Ok(Self {
            client: Client::with_auth(base_url, auth_token.to_string())?,
            store,
        })
    }

    pub async fn sync(&mut self) -> Result<SyncReport> {
        let started = Instant::now();
        let pushed_ops = self.push_outbox().await?;

        let subscriptions = self.client.subscriptions().await?;
        let tags = self.client.tags().await?;
        let counts = self.client.unread_counts().await?;
        self.store.reconcile_feeds(&subscriptions, &tags)?;
        self.store.set_unread_counts(&counts.unreadcounts)?;

        let ot = self.sync_watermark()?;
        let max_items = if ot.is_some() {
            usize::MAX
        } else {
            INITIAL_SYNC_ITEMS
        };
        let pull_started = now();
        let (new_items, all_updated) = self.pull_stream(Stream::All, ot, max_items).await?;
        let starred_max = if ot.is_some() {
            usize::MAX
        } else {
            INITIAL_STARRED_ITEMS
        };
        let (starred_items, starred_updated) =
            self.pull_stream(Stream::Starred, ot, starred_max).await?;
        let unread_remote = self.reconcile_read(&counts).await?;
        let starred_remote = self.reconcile_starred().await?;

        // Organic pulls only cover new/modified items. State changes on older
        // articles (and starred items beyond the initial window) need content
        // fetched by ID.
        let mut backfilled = 0;
        if let Some(remote) = &starred_remote {
            backfilled = self.backfill_missing(remote, BACKFILL_LIMIT).await?;
        }
        if let Some(remote) = &unread_remote {
            let remaining = BACKFILL_LIMIT.saturating_sub(backfilled);
            if remaining > 0 {
                backfilled += self.backfill_missing(remote, remaining).await?;
            }
        }
        let evicted = self.store.enforce_cache_budget(DEFAULT_CACHE_BYTES)?;

        let unread = self.store.total_unread()?;
        let failed_ops = self.store.dead_outbox_count()?;
        self.store.set_state("last_sync_at", &now().to_string())?;
        let server_time = all_updated.or(starred_updated).filter(|time| *time > 0);
        let next = (server_time.unwrap_or(pull_started) - WATERMARK_OVERLAP_SECONDS).max(0);
        self.store.set_state("ot_watermark", &next.to_string())?;

        Ok(SyncReport {
            pushed_ops,
            new_items,
            starred_items,
            backfilled,
            failed_ops,
            evicted,
            feeds: subscriptions.len(),
            unread,
            duration_ms: started.elapsed().as_millis(),
        })
    }

    async fn pull_stream(
        &self,
        stream: Stream,
        ot: Option<i64>,
        max_items: usize,
    ) -> Result<(usize, Option<i64>)> {
        let mut continuation: Option<String> = None;
        let mut seen_continuations = HashSet::new();
        let mut pages = 0;
        let mut fetched = 0;
        let mut new_items = 0;
        let mut updated = None;
        let mut page_size = CONTENT_PAGE_SIZE;
        while fetched < max_items && pages < MAX_PAGES_PER_STREAM {
            let page = loop {
                match self
                    .client
                    .stream_contents(
                        &stream.api_id(),
                        page_size,
                        None,
                        ot,
                        continuation.as_deref(),
                        None,
                    )
                    .await
                {
                    Ok(page) => break page,
                    Err(error) if page_size > 100 && is_response_limit_error(&error) => {
                        page_size = 100;
                    }
                    Err(error) => return Err(error),
                }
            };
            if updated.is_none() {
                updated = page.updated;
            }
            if page.items.is_empty() {
                break;
            }
            fetched += page.items.len();
            pages += 1;
            new_items += self.store.upsert_items(&page.items)?;
            match page.continuation {
                Some(next)
                    if Some(&next) != continuation.as_ref()
                        && seen_continuations.insert(next.clone()) =>
                {
                    continuation = Some(next);
                }
                _ => break,
            }
        }
        Ok((new_items, updated))
    }

    async fn backfill_missing(&self, remote: &HashSet<String>, limit: usize) -> Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let mut missing = self.store.missing_remote_ids(remote)?;
        missing.truncate(limit);
        let mut fetched = 0;
        for chunk in missing.chunks(CONTENT_BATCH_SIZE) {
            let items = self.client.item_contents(chunk).await?;
            fetched += items.len();
            self.store.upsert_items(&items)?;
        }
        Ok(fetched)
    }

    /// The next `ot` is the start of the last successful sync, not the newest item:
    /// items modified after an old sync but before the newest item must not be skipped.
    fn sync_watermark(&self) -> Result<Option<i64>> {
        if let Some(value) = self.store.state("ot_watermark")?
            && let Ok(value) = value.parse::<i64>()
        {
            return Ok(Some(value));
        }
        if let Some(value) = self.store.state("last_sync_at")?
            && let Ok(value) = value.parse::<i64>()
        {
            return Ok(Some((value - WATERMARK_OVERLAP_SECONDS).max(0)));
        }
        let newest = self.store.newest_item_usec()?;
        Ok((newest > 0).then_some((newest / 1_000_000 - WATERMARK_OVERLAP_SECONDS).max(0)))
    }

    /// Fetch the complete unread set when possible; otherwise leave local state alone.
    async fn reconcile_read(&self, counts: &UnreadCounts) -> Result<Option<HashSet<String>>> {
        let server_total = counts
            .unreadcounts
            .iter()
            .find(|c| c.id == "user/-/state/com.google/reading-list")
            .map(|c| c.count);
        let Some(server_total) = server_total else {
            return Ok(None);
        };

        let mut remote = HashSet::new();
        let mut continuation: Option<String> = None;
        let mut complete = false;
        for _ in 0..20 {
            let page = self
                .client
                .item_ids(
                    "user/-/state/com.google/reading-list",
                    1000,
                    None,
                    continuation.as_deref(),
                    Some("user/-/state/com.google/read"),
                )
                .await?;
            remote.extend(
                page.item_refs
                    .into_iter()
                    .map(|r| short_id(&r.id).to_string()),
            );
            match page.continuation {
                Some(next) if Some(&next) != continuation.as_ref() => {
                    continuation = Some(next);
                }
                _ => {
                    complete = true;
                    break;
                }
            }
        }
        if complete && remote.len() as i64 >= server_total {
            self.store.reconcile_read_state(&remote)?;
            Ok(Some(remote))
        } else {
            Ok(None)
        }
    }

    async fn reconcile_starred(&self) -> Result<Option<HashSet<String>>> {
        let mut remote = HashSet::new();
        let mut continuation: Option<String> = None;
        let mut complete = false;
        for _ in 0..10 {
            let page = self
                .client
                .item_ids(
                    "user/-/state/com.google/starred",
                    1000,
                    None,
                    continuation.as_deref(),
                    None,
                )
                .await?;
            remote.extend(
                page.item_refs
                    .into_iter()
                    .map(|r| short_id(&r.id).to_string()),
            );
            match page.continuation {
                Some(next) if Some(&next) != continuation.as_ref() => {
                    continuation = Some(next);
                }
                _ => {
                    complete = true;
                    break;
                }
            }
        }
        if complete {
            self.store.reconcile_starred_state(&remote)?;
            Ok(Some(remote))
        } else {
            Ok(None)
        }
    }

    async fn push_outbox(&mut self) -> Result<usize> {
        let mut pushed = 0;
        loop {
            let ops = self.store.outbox(100)?;
            if ops.is_empty() {
                break;
            }
            let ids: Vec<i64> = ops.iter().map(|op| op.id).collect();
            match self.push_batch(&ops).await {
                Ok(()) => {
                    self.store.remove_ops(&ids)?;
                    pushed += ops.len();
                }
                Err(error) if is_permanent_error(&error) => {
                    // A permanent 4xx must not block later operations, but a
                    // batch can contain more than the failing operation. Retry
                    // individually so only the bad operations are discarded.
                    for op in &ops {
                        match self.push_one(op).await {
                            Ok(()) => {
                                self.store.remove_ops(&[op.id])?;
                                pushed += 1;
                            }
                            Err(error) if is_permanent_error(&error) => {
                                self.store.dead_letter_ops(&[op.id])?;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                Err(error) => {
                    // Transient failures keep every unsent operation for the next sync.
                    return Err(error);
                }
            }
            if ops.len() < 100 {
                break;
            }
        }
        Ok(pushed)
    }

    async fn push_one(&mut self, op: &OutboxOp) -> Result<()> {
        match op.op.as_str() {
            "mark_all_read" => {
                let stream = op.stream_id.as_deref().unwrap_or("");
                self.client
                    .mark_all_read(stream, op.ts_us.unwrap_or(0))
                    .await
            }
            "read" => {
                let id = short_id(op.item_id.as_deref().unwrap_or("")).to_string();
                self.client
                    .edit_tag(&[id], &["user/-/state/com.google/read"], &[])
                    .await
            }
            "unread" => {
                let id = short_id(op.item_id.as_deref().unwrap_or("")).to_string();
                self.client
                    .edit_tag(&[id], &[], &["user/-/state/com.google/read"])
                    .await
            }
            "star" => {
                let id = short_id(op.item_id.as_deref().unwrap_or("")).to_string();
                self.client
                    .edit_tag(&[id], &["user/-/state/com.google/starred"], &[])
                    .await
            }
            "unstar" => {
                let id = short_id(op.item_id.as_deref().unwrap_or("")).to_string();
                self.client
                    .edit_tag(&[id], &[], &["user/-/state/com.google/starred"])
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn push_batch(&mut self, ops: &[OutboxOp]) -> Result<()> {
        let mark_all: Vec<&OutboxOp> = ops.iter().filter(|op| op.op == "mark_all_read").collect();
        for op in mark_all {
            let stream = op.stream_id.as_deref().unwrap_or("");
            self.client
                .mark_all_read(stream, op.ts_us.unwrap_or(0))
                .await?;
        }

        for (op_name, add, remove) in [
            ("read", TAG_READ, ""),
            ("unread", "", TAG_READ),
            ("star", TAG_STARRED, ""),
            ("unstar", "", TAG_STARRED),
        ] {
            let ids: Vec<String> = ops
                .iter()
                .filter(|op| op.op == op_name)
                .filter_map(|op| op.item_id.as_deref())
                .map(canonical_item_id)
                .collect();
            for chunk in ids.chunks(100) {
                let add_tags: Vec<&str> = (!add.is_empty()).then_some(add).into_iter().collect();
                let remove_tags: Vec<&str> =
                    (!remove.is_empty()).then_some(remove).into_iter().collect();
                self.client.edit_tag(chunk, &add_tags, &remove_tags).await?;
            }
        }
        Ok(())
    }
}

fn canonical_item_id(id: &str) -> String {
    if id.is_empty() || id.starts_with(ITEM_TAG_PREFIX) {
        id.to_string()
    } else {
        format!("{ITEM_TAG_PREFIX}{id}")
    }
}

fn is_permanent_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Http { status, .. }
            if status.is_client_error()
                && *status != reqwest::StatusCode::UNAUTHORIZED
                && *status != reqwest::StatusCode::FORBIDDEN
                && *status != reqwest::StatusCode::REQUEST_TIMEOUT
                && *status != reqwest::StatusCode::TOO_MANY_REQUESTS
    )
}

fn is_response_limit_error(error: &Error) -> bool {
    matches!(error, Error::Other(message) if message.contains("response limit"))
}

fn short_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn item_json(starred: bool) -> serde_json::Value {
        item_json_with("1", starred, false, "1700000000123456")
    }

    fn item_json_with(
        id: &str,
        starred: bool,
        read: bool,
        timestamp_usec: &str,
    ) -> serde_json::Value {
        let mut categories = Vec::new();
        if read {
            categories.push("user/-/state/com.google/read");
        }
        if starred {
            categories.push("user/-/state/com.google/starred");
        }
        json!({
            "id": id,
            "title": format!("Item {id}"),
            "published": 1_700_000_000,
            "timestampUsec": timestamp_usec,
            "alternate": [{"href": format!("https://example.com/{id}")}],
            "content": {"content": "<p>Body</p>"},
            "categories": categories,
            "origin": {"streamId": "feed/1", "title": "One"}
        })
    }

    async fn mount_minimal_sync(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("token"))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/subscription/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subscriptions": [{"id": "feed/1", "title": "One"}]
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/tag/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tags": []})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/unread-count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "unreadcounts": [
                    {"id": "feed/1", "count": 0},
                    {"id": "user/-/state/com.google/reading-list", "count": 0}
                ]
            })))
            .mount(server)
            .await;
        for stream in [
            "user/-/state/com.google/reading-list",
            "user/-/state/com.google/starred",
        ] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/greader.php/reader/api/0/stream/contents/{stream}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "items": [],
                    "updated": 1700000500
                })))
                .mount(server)
                .await;
        }
        for stream in [
            "user/-/state/com.google/reading-list",
            "user/-/state/com.google/starred",
        ] {
            Mock::given(method("GET"))
                .and(path("/api/greader.php/reader/api/0/stream/items/ids"))
                .and(query_param("s", stream))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "itemRefs": []
                })))
                .mount(server)
                .await;
        }
    }

    async fn mount_edit_tag(server: &MockServer, statuses: Vec<u16>) {
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/greader.php/reader/api/0/edit-tag"))
            .respond_with(move |_request: &Request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(statuses.get(call).copied().unwrap_or(200))
            })
            .mount(server)
            .await;
    }

    fn store_with_read_op() -> Arc<Store> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .reconcile_feeds(
                &[crate::models::Subscription {
                    id: "feed/1".into(),
                    title: "One".into(),
                    ..Default::default()
                }],
                &[],
            )
            .unwrap();
        let item: crate::models::Item = serde_json::from_value(item_json(false)).unwrap();
        store.upsert_items(&[item]).unwrap();
        store.set_read(&["1".into()], true).unwrap();
        store
    }

    async fn assert_transient_replay(first_status: u16, later_statuses: Vec<u16>) {
        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());
        mount_minimal_sync(&server).await;
        let mut statuses = vec![first_status];
        statuses.extend(later_statuses);
        mount_edit_tag(&server, statuses).await;

        let store = store_with_read_op();
        let mut engine = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        let error = engine.sync().await.unwrap_err();
        assert!(matches!(
            error,
            Error::Http { status, .. } if status.as_u16() == first_status
        ));
        assert_eq!(store.outbox(10).unwrap().len(), 1);
        assert_eq!(store.dead_outbox_count().unwrap(), 0);

        let mut retry = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        let report = retry.sync().await.unwrap();
        assert_eq!(report.pushed_ops, 1);
        assert!(store.outbox(10).unwrap().is_empty());
        assert_eq!(store.dead_outbox_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn retains_ops_after_transient_server_errors_and_replays() {
        assert_transient_replay(503, vec![200]).await;
        assert_transient_replay(429, vec![200]).await;
    }

    #[tokio::test]
    async fn retains_ops_after_auth_failure_and_replays() {
        // Client::write retries once with a fresh CSRF token on 401.
        assert_transient_replay(401, vec![401, 200]).await;
    }

    #[tokio::test]
    async fn retains_successful_ops_when_a_later_op_in_the_batch_fails() {
        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());
        mount_minimal_sync(&server).await;
        mount_edit_tag(&server, vec![200, 503, 200, 200]).await;

        let store = store_with_read_op();
        store.set_starred(&["1".into()], true).unwrap();
        assert_eq!(store.outbox(10).unwrap().len(), 2);

        let mut failed = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        assert!(failed.sync().await.is_err());
        assert_eq!(store.outbox(10).unwrap().len(), 2);
        assert_eq!(store.dead_outbox_count().unwrap(), 0);

        let mut retry = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        let report = retry.sync().await.unwrap();
        assert_eq!(report.pushed_ops, 2);
        assert!(store.outbox(10).unwrap().is_empty());
        assert_eq!(store.dead_outbox_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn retains_ops_after_network_failure_and_replays() {
        let store = store_with_read_op();
        let mut failed = SyncEngine::with_auth(
            store.clone(),
            "http://127.0.0.1:1/api/greader.php",
            "secret",
        )
        .unwrap();
        assert!(failed.sync().await.is_err());
        assert_eq!(store.outbox(10).unwrap().len(), 1);
        assert_eq!(store.dead_outbox_count().unwrap(), 0);

        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());
        mount_minimal_sync(&server).await;
        mount_edit_tag(&server, vec![200]).await;
        let mut retry = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        let report = retry.sync().await.unwrap();
        assert_eq!(report.pushed_ops, 1);
        assert!(store.outbox(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn retains_mark_all_ops_and_their_timestamp_after_transient_failure() {
        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());
        mount_minimal_sync(&server).await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/greader.php/reader/api/0/mark-all-as-read"))
            .respond_with({
                let calls = calls.clone();
                move |_request: &Request| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(if call == 0 { 503 } else { 200 })
                }
            })
            .mount(&server)
            .await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .reconcile_feeds(
                &[crate::models::Subscription {
                    id: "feed/1".into(),
                    title: "One".into(),
                    ..Default::default()
                }],
                &[],
            )
            .unwrap();
        let item: crate::models::Item = serde_json::from_value(item_json(false)).unwrap();
        store.upsert_items(&[item]).unwrap();
        store
            .mark_all_read(&Stream::All, 1_700_000_000_123_456)
            .unwrap();

        let mut failed = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        assert!(failed.sync().await.is_err());
        assert_eq!(store.outbox(10).unwrap().len(), 1);
        assert_eq!(store.dead_outbox_count().unwrap(), 0);

        let mut retry = SyncEngine::with_auth(store.clone(), &base, "secret").unwrap();
        let report = retry.sync().await.unwrap();
        assert_eq!(report.pushed_ops, 1);
        assert!(store.outbox(10).unwrap().is_empty());
        assert!(store.item("1").unwrap().unwrap().read);

        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .rev()
            .find(|request| {
                request
                    .url
                    .path()
                    .ends_with("/reader/api/0/mark-all-as-read")
            })
            .unwrap();
        let form: Vec<(String, String)> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        assert!(
            form.iter()
                .any(|(key, value)| key == "ts" && value == "1700000000123456")
        );
    }

    #[test]
    fn builds_canonical_item_tags() {
        assert_eq!(
            canonical_item_id("1234abcd"),
            "tag:google.com,2005:reader/item/1234abcd"
        );
        assert_eq!(
            canonical_item_id("tag:google.com,2005:reader/item/1234abcd"),
            "tag:google.com,2005:reader/item/1234abcd"
        );
    }

    #[test]
    fn recognizes_permanent_errors() {
        let http = |status| Error::Http {
            status,
            body: String::new(),
        };
        assert!(is_permanent_error(&http(reqwest::StatusCode::BAD_REQUEST)));
        assert!(is_permanent_error(&http(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        )));
        assert!(!is_permanent_error(&http(
            reqwest::StatusCode::UNAUTHORIZED
        )));
        assert!(!is_permanent_error(&http(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_permanent_error(&Error::Other("offline".into())));
    }

    #[test]
    fn recognizes_response_limit_errors() {
        assert!(is_response_limit_error(&Error::Other(
            "greader response exceeded the 33554432-byte response limit".into()
        )));
        assert!(!is_response_limit_error(&Error::Other("offline".into())));
    }

    #[tokio::test]
    async fn syncs_feeds_items_and_state() {
        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());

        Mock::given(method("POST"))
            .and(path("/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Auth=secret\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/subscription/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subscriptions": [{
                    "id": "feed/1",
                    "title": "One",
                    "categories": [{"id": "user/-/label/News", "label": "News"}]
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/tag/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tags": [{"id": "user/-/label/News", "label": "News"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/unread-count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "unreadcounts": [
                    {"id": "feed/1", "count": 1},
                    {"id": "user/-/state/com.google/reading-list", "count": 1}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/greader.php/reader/api/0/stream/contents/user/-/state/com.google/reading-list",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [item_json(false)],
                "updated": 1700000500
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/greader.php/reader/api/0/stream/contents/user/-/state/com.google/starred",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [item_json(true)]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/stream/items/ids"))
            .and(query_param("s", "user/-/state/com.google/reading-list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "itemRefs": [{"id": "1"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/stream/items/ids"))
            .and(query_param("s", "user/-/state/com.google/starred"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "itemRefs": [{"id": "1"}, {"id": "2"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/greader.php/reader/api/0/stream/items/contents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [item_json_with("2", true, true, "1700000000000002")]
            })))
            .mount(&server)
            .await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut engine = SyncEngine::new(store.clone(), &base, "me", "pw")
            .await
            .unwrap();
        let report = engine.sync().await.unwrap();

        assert_eq!(report.feeds, 1);
        assert_eq!(report.new_items, 1);
        assert_eq!(report.backfilled, 1);
        assert_eq!(report.unread, 1);
        let items = store.items(&Stream::All, 10, 0, false, None).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Item 1");
        assert!(!items[0].read);
        assert!(items[0].starred);
        let backfilled = store.item("2").unwrap().unwrap();
        assert_eq!(backfilled.title, "Item 2");
        assert!(backfilled.read);
        assert!(backfilled.starred);
        assert_eq!(
            store.feeds().unwrap()[0].folder_name.as_deref(),
            Some("News")
        );

        let second = engine.sync().await.unwrap();
        assert_eq!(second.new_items, 0);
        assert_eq!(second.backfilled, 0);
        let requests = server.received_requests().await.unwrap();
        let last = requests
            .iter()
            .rev()
            .find(|r| {
                r.url
                    .path()
                    .ends_with("/stream/contents/user/-/state/com.google/reading-list")
            })
            .unwrap();
        assert!(
            last.url
                .query_pairs()
                .any(|(k, v)| k == "ot" && v == "1700000200")
        );
    }

    #[tokio::test]
    async fn dead_letters_permanent_push_failures_and_continues() {
        let server = MockServer::start().await;
        let base = format!("{}/api/greader.php", server.uri());

        Mock::given(method("POST"))
            .and(path("/api/greader.php/accounts/ClientLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Auth=secret\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("token"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/subscription/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subscriptions": [{"id": "feed/1", "title": "One"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/tag/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tags": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/unread-count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "unreadcounts": [
                    {"id": "feed/1", "count": 1},
                    {"id": "user/-/state/com.google/reading-list", "count": 1}
                ]
            })))
            .mount(&server)
            .await;
        for stream in [
            "user/-/state/com.google/reading-list",
            "user/-/state/com.google/starred",
        ] {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/api/greader.php/reader/api/0/stream/contents/{stream}"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "items": [],
                    "updated": 1700000500
                })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/stream/items/ids"))
            .and(query_param("s", "user/-/state/com.google/reading-list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "itemRefs": [{"id": "1"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/greader.php/reader/api/0/stream/items/ids"))
            .and(query_param("s", "user/-/state/com.google/starred"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "itemRefs": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/greader.php/reader/api/0/edit-tag"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .reconcile_feeds(
                &[crate::models::Subscription {
                    id: "feed/1".into(),
                    title: "One".into(),
                    ..Default::default()
                }],
                &[],
            )
            .unwrap();
        let item: crate::models::Item = serde_json::from_value(item_json(false)).unwrap();
        store.upsert_items(&[item]).unwrap();
        store.set_read(&["1".into()], true).unwrap();
        let mut engine = SyncEngine::new(store.clone(), &base, "me", "pw")
            .await
            .unwrap();
        let report = engine.sync().await.unwrap();

        assert_eq!(report.failed_ops, 1);
        assert_eq!(store.dead_outbox_count().unwrap(), 1);
        assert!(store.outbox(10).unwrap().is_empty());
        assert!(!store.item("1").unwrap().unwrap().read);

        let requests = server.received_requests().await.unwrap();
        let edit = requests
            .iter()
            .find(|r| r.url.path().ends_with("/reader/api/0/edit-tag"))
            .unwrap();
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&edit.body)
            .into_owned()
            .collect();
        assert!(
            pairs
                .iter()
                .any(|(key, value)| { key == "i" && value == "tag:google.com,2005:reader/item/1" })
        );
    }
}
