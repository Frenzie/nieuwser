use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::api::Client;
use crate::models::{Stream, UnreadCounts};
use crate::store::{DEFAULT_CACHE_BYTES, OutboxOp, Store};

pub struct SyncEngine {
    client: Client,
    store: Arc<Store>,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub pushed_ops: usize,
    pub new_items: usize,
    pub starred_items: usize,
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

        let newest = self.store.newest_item_usec()?;
        let ot = (newest > 0).then_some(newest / 1_000_000);
        let max_items = if ot.is_some() { usize::MAX } else { 500 };
        let new_items = self.pull_stream(Stream::All, ot, max_items).await?;
        let starred_items = self.pull_stream(Stream::Starred, None, 500).await?;
        self.reconcile_read(&counts).await?;
        self.reconcile_starred().await?;
        let evicted = self.store.enforce_cache_budget(DEFAULT_CACHE_BYTES)?;

        let unread = self.store.total_unread()?;
        self.store.set_state("last_sync_at", &now().to_string())?;
        self.store.set_state(
            "newest_item_usec",
            &self.store.newest_item_usec()?.to_string(),
        )?;

        Ok(SyncReport {
            pushed_ops,
            new_items,
            starred_items,
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
    ) -> Result<usize> {
        let mut continuation: Option<String> = None;
        let mut fetched = 0;
        let mut new_items = 0;
        while fetched < max_items {
            let page = self
                .client
                .stream_contents(
                    &stream.api_id(),
                    100,
                    None,
                    ot,
                    continuation.as_deref(),
                    None,
                )
                .await?;
            if page.items.is_empty() {
                break;
            }
            fetched += page.items.len();
            new_items += self.store.upsert_items(&page.items)?;
            match page.continuation {
                Some(next) if Some(&next) != continuation.as_ref() => {
                    continuation = Some(next);
                }
                _ => break,
            }
        }
        Ok(new_items)
    }

    /// Fetch the complete unread set when possible; otherwise leave local state alone.
    async fn reconcile_read(&self, counts: &UnreadCounts) -> Result<()> {
        let server_total = counts
            .unreadcounts
            .iter()
            .find(|c| c.id == "user/-/state/com.google/reading-list")
            .map(|c| c.count);
        let Some(server_total) = server_total else {
            return Ok(());
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
        }
        Ok(())
    }

    async fn reconcile_starred(&self) -> Result<()> {
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
        }
        Ok(())
    }

    async fn push_outbox(&mut self) -> Result<usize> {
        let mut pushed = 0;
        loop {
            let ops = self.store.outbox(100)?;
            if ops.is_empty() {
                break;
            }
            let ids: Vec<i64> = ops.iter().map(|op| op.id).collect();
            if let Err(error) = self.push_batch(&ops).await {
                self.store.bump_ops(&ids)?;
                return Err(error);
            }
            self.store.remove_ops(&ids)?;
            pushed += ops.len();
            if ops.len() < 100 {
                break;
            }
        }
        Ok(pushed)
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
            ("read", "user/-/state/com.google/read", ""),
            ("unread", "", "user/-/state/com.google/read"),
            ("star", "user/-/state/com.google/starred", ""),
            ("unstar", "", "user/-/state/com.google/starred"),
        ] {
            let ids: Vec<String> = ops
                .iter()
                .filter(|op| op.op == op_name)
                .filter_map(|op| op.item_id.as_deref())
                .map(|id| short_id(id).to_string())
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn item_json(starred: bool) -> serde_json::Value {
        let mut categories = Vec::new();
        if starred {
            categories.push("user/-/state/com.google/starred");
        }
        json!({
            "id": "1",
            "title": "Hello",
            "published": 1_700_000_000,
            "timestampUsec": "1700000000123456",
            "alternate": [{"href": "https://example.com/1"}],
            "content": {"content": "<p>Body</p>"},
            "categories": categories,
            "origin": {"streamId": "feed/1", "title": "One"}
        })
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
                "items": [item_json(false)]
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
                "itemRefs": [{"id": "1"}]
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
        assert_eq!(report.unread, 1);
        let items = store.items(&Stream::All, 10, 0, false, None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Hello");
        assert!(!items[0].read);
        assert!(items[0].starred);
        assert_eq!(
            store.feeds().unwrap()[0].folder_name.as_deref(),
            Some("News")
        );

        let second = engine.sync().await.unwrap();
        assert_eq!(second.new_items, 0);
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
                .any(|(k, v)| k == "ot" && v == "1700000000")
        );
    }
}
