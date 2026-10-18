use serde::{Deserialize, Deserializer};

fn de_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|n| n as i64)),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SubscriptionList {
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Category {
    pub id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TagList {
    #[serde(default)]
    pub tags: Vec<Tag>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Tag {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UnreadCounts {
    #[serde(default)]
    pub max: i64,
    #[serde(default)]
    pub unreadcounts: Vec<UnreadCount>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UnreadCount {
    pub id: String,
    #[serde(default)]
    pub count: i64,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub newest_item_timestamp_usec: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ItemRefs {
    #[serde(default)]
    pub item_refs: Vec<ItemRef>,
    #[serde(default)]
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub id: String,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub timestamp_usec: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StreamContents {
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub continuation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub published: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub updated: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub crawl_time_msec: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub timestamp_usec: Option<i64>,
    #[serde(default)]
    pub canonical: Vec<Link>,
    #[serde(default)]
    pub alternate: Vec<Link>,
    #[serde(default)]
    pub summary: Option<Content>,
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Link {
    pub href: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Content {
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub stream_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

impl Item {
    pub fn feed_id(&self) -> Option<&str> {
        self.origin.as_ref().map(|o| o.stream_id.as_str())
    }

    pub fn feed_title(&self) -> Option<&str> {
        self.origin.as_ref().and_then(|o| o.title.as_deref())
    }

    pub fn link(&self) -> Option<&str> {
        self.alternate
            .first()
            .or_else(|| self.canonical.first())
            .map(|l| l.href.as_str())
    }

    pub fn body_html(&self) -> &str {
        self.content
            .as_ref()
            .map(|c| c.content.as_str())
            .or_else(|| self.summary.as_ref().map(|c| c.content.as_str()))
            .unwrap_or("")
    }

    pub fn usec(&self) -> i64 {
        self.timestamp_usec
            .filter(|n| *n > 0)
            .or_else(|| self.published.map(|n| n * 1_000_000))
            .or_else(|| self.updated.map(|n| n * 1_000_000))
            .or_else(|| self.crawl_time_msec.map(|n| n * 1_000))
            .unwrap_or(0)
    }

    pub fn published_secs(&self) -> i64 {
        self.published
            .filter(|n| *n > 0)
            .unwrap_or_else(|| self.usec() / 1_000_000)
    }

    pub fn is_read(&self) -> bool {
        self.categories
            .iter()
            .any(|c| c == "user/-/state/com.google/read")
    }

    pub fn is_starred(&self) -> bool {
        self.categories
            .iter()
            .any(|c| c == "user/-/state/com.google/starred")
    }

    /// FreshRSS usually returns bare numeric ids, but accept Google Reader style too.
    pub fn short_id(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stream {
    All,
    Starred,
    Feed(String),
    Folder(String),
}

impl Stream {
    pub fn api_id(&self) -> String {
        match self {
            Stream::All => "user/-/state/com.google/reading-list".into(),
            Stream::Starred => "user/-/state/com.google/starred".into(),
            Stream::Feed(id) | Stream::Folder(id) => id.clone(),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Stream::All),
            "starred" => Some(Stream::Starred),
            v if v.starts_with("feed/") => Some(Stream::Feed(v.into())),
            v if v.starts_with("user/-/label/") => Some(Stream::Folder(v.into())),
            _ => None,
        }
    }
}
