use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};

use crate::Result;
use crate::models::{Item, Stream, Subscription, Tag, UnreadCount};

pub struct Store {
    conn: Mutex<Connection>,
}

pub const DEFAULT_CACHE_BYTES: u64 = 500 * 1024 * 1024;
/// A permanent 4xx marks an operation dead so it cannot block later operations.
pub const MAX_PUSH_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone)]
pub struct FeedRow {
    pub id: String,
    pub title: String,
    pub folder_id: Option<String>,
    pub folder_name: Option<String>,
    pub unread_count: i64,
    pub icon_url: Option<String>,
    pub html_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ItemSummary {
    pub id: String,
    pub feed_id: String,
    pub feed_title: String,
    pub title: String,
    pub author: Option<String>,
    pub published: i64,
    pub href: Option<String>,
    pub read: bool,
    pub starred: bool,
    pub timestamp_usec: i64,
}

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub id: String,
    pub feed_id: String,
    pub feed_title: String,
    pub title: String,
    pub author: Option<String>,
    pub published: i64,
    pub href: Option<String>,
    pub content_html: String,
    pub summary_html: String,
    pub read: bool,
    pub starred: bool,
    pub timestamp_usec: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Read,
    Unread,
    Star,
    Unstar,
    MarkAllRead,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Unread => "unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
            Self::MarkAllRead => "mark_all_read",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "read" => Self::Read,
            "unread" => Self::Unread,
            "star" => Self::Star,
            "unstar" => Self::Unstar,
            "mark_all_read" => Self::MarkAllRead,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct OutboxOp {
    pub id: i64,
    pub item_id: Option<String>,
    pub stream_id: Option<String>,
    pub op: OpKind,
    pub ts_us: Option<i64>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // Transactions roll back on drop, so a panic in another thread leaves the data consistent.
        self.conn.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS account (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                base_url    TEXT NOT NULL,
                email       TEXT NOT NULL,
                auth_token  TEXT,
                created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS folders (
                id    TEXT PRIMARY KEY,
                name  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS feeds (
                id            TEXT PRIMARY KEY,
                title         TEXT NOT NULL,
                url           TEXT,
                html_url      TEXT,
                icon_url      TEXT,
                folder_id     TEXT REFERENCES folders(id) ON DELETE SET NULL,
                unread_count  INTEGER NOT NULL DEFAULT 0,
                updated_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS feeds_folder ON feeds(folder_id);

            CREATE TABLE IF NOT EXISTS items (
                id              TEXT PRIMARY KEY,
                feed_id         TEXT REFERENCES feeds(id) ON DELETE CASCADE,
                title           TEXT NOT NULL,
                author          TEXT,
                published       INTEGER,
                updated         INTEGER,
                timestamp_usec  INTEGER NOT NULL DEFAULT 0,
                crawl_time_msec INTEGER NOT NULL DEFAULT 0,
                href            TEXT,
                summary_html    TEXT,
                content_html    TEXT,
                read            INTEGER NOT NULL DEFAULT 0,
                starred         INTEGER NOT NULL DEFAULT 0,
                fetched_at      INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS items_feed_time ON items(feed_id, timestamp_usec DESC);
            CREATE INDEX IF NOT EXISTS items_time ON items(timestamp_usec DESC);
            CREATE INDEX IF NOT EXISTS items_unread ON items(read, timestamp_usec DESC);

            CREATE TABLE IF NOT EXISTS outbox (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id     TEXT,
                stream_id   TEXT,
                op          TEXT NOT NULL,
                ts_us       INTEGER,
                created_at  INTEGER NOT NULL,
                attempts    INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS outbox_item ON outbox(item_id);

            CREATE TABLE IF NOT EXISTS sync_state (
                key    TEXT PRIMARY KEY,
                value  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS media (
                url         TEXT PRIMARY KEY,
                data        BLOB NOT NULL,
                fetched_at  INTEGER NOT NULL
            );
            "#,
        )?;
        let has_auth_token: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('account') WHERE name = 'auth_token'",
            [],
            |row| row.get(0),
        )?;
        if has_auth_token == 0 {
            conn.execute("ALTER TABLE account ADD COLUMN auth_token TEXT", [])?;
        }

        // v0.1 sent mark-all-read timestamps in nanoseconds; FreshRSS compares
        // them against microsecond entry IDs, so convert any queued operation.
        let has_ts_us: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('outbox') WHERE name = 'ts_us'",
            [],
            |row| row.get(0),
        )?;
        if has_ts_us == 0 {
            let has_ts_ns: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('outbox') WHERE name = 'ts_ns'",
                [],
                |row| row.get(0),
            )?;
            if has_ts_ns > 0 {
                conn.execute_batch(
                    "ALTER TABLE outbox RENAME COLUMN ts_ns TO ts_us;
                     UPDATE outbox SET ts_us = ts_us / 1000
                     WHERE op = 'mark_all_read' AND ts_us IS NOT NULL;",
                )?;
            } else {
                conn.execute("ALTER TABLE outbox ADD COLUMN ts_us INTEGER", [])?;
            }
        }

        let had_fts: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'items_fts'",
            [],
            |row| row.get(0),
        )?;
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
                title, author, summary_html, content_html,
                content='items', content_rowid='rowid',
                tokenize='unicode61 remove_diacritics 2'
            );
            CREATE TRIGGER IF NOT EXISTS items_fts_ai AFTER INSERT ON items BEGIN
                INSERT INTO items_fts(rowid, title, author, summary_html, content_html)
                VALUES (new.rowid, new.title, new.author, new.summary_html, new.content_html);
            END;
            CREATE TRIGGER IF NOT EXISTS items_fts_ad AFTER DELETE ON items BEGIN
                INSERT INTO items_fts(items_fts, rowid, title, author, summary_html, content_html)
                VALUES ('delete', old.rowid, old.title, old.author, old.summary_html, old.content_html);
            END;
            DROP TRIGGER IF EXISTS items_fts_au;
            CREATE TRIGGER items_fts_au
                AFTER UPDATE OF title, author, summary_html, content_html ON items
                WHEN old.title IS NOT new.title
                  OR old.author IS NOT new.author
                  OR old.summary_html IS NOT new.summary_html
                  OR old.content_html IS NOT new.content_html
            BEGIN
                INSERT INTO items_fts(items_fts, rowid, title, author, summary_html, content_html)
                VALUES ('delete', old.rowid, old.title, old.author, old.summary_html, old.content_html);
                INSERT INTO items_fts(rowid, title, author, summary_html, content_html)
                VALUES (new.rowid, new.title, new.author, new.summary_html, new.content_html);
            END;
            "#,
        )?;
        if had_fts == 0 {
            conn.execute("INSERT INTO items_fts(items_fts) VALUES ('rebuild')", [])?;
        }
        Ok(())
    }

    pub fn set_account(&self, base_url: &str, email: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO account(id, base_url, email, created_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET base_url = excluded.base_url, email = excluded.email",
            params![base_url, email, now()],
        )?;
        Ok(())
    }

    pub fn account(&self) -> Result<Option<(String, String)>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT base_url, email FROM account WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_auth_token(&self, token: Option<&str>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE account SET auth_token = ?1 WHERE id = 1",
            params![token],
        )?;
        Ok(())
    }

    pub fn auth_token(&self) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT auth_token FROM account WHERE id = 1 AND auth_token IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Upsert folders/feeds from the server and remove feeds no longer subscribed.
    pub fn reconcile_feeds(&self, subscriptions: &[Subscription], tags: &[Tag]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = now();

        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS seen_feeds (id TEXT PRIMARY KEY);
             DELETE FROM seen_feeds;
             CREATE TEMP TABLE IF NOT EXISTS seen_folders (id TEXT PRIMARY KEY);
             DELETE FROM seen_folders;",
        )?;

        for tag in tags {
            if !is_label(&tag.id) {
                continue;
            }
            let name = tag
                .label
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| tag.id.rsplit('/').next().unwrap_or(&tag.id));
            tx.execute(
                "INSERT OR IGNORE INTO seen_folders(id) VALUES (?1)",
                params![tag.id],
            )?;
            tx.execute(
                "INSERT INTO folders(id, name) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                params![tag.id, name],
            )?;
        }

        for sub in subscriptions {
            let folder_id = sub
                .categories
                .iter()
                .find(|c| is_label(&c.id))
                .map(|c| c.id.clone());
            if let Some(folder_id) = &folder_id {
                tx.execute(
                    "INSERT OR IGNORE INTO seen_folders(id) VALUES (?1)",
                    params![folder_id],
                )?;
                tx.execute(
                    "INSERT INTO folders(id, name) VALUES (?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                    params![
                        folder_id,
                        sub.categories
                            .iter()
                            .find(|c| is_label(&c.id))
                            .map(|c| c.label.as_str())
                            .unwrap_or(folder_id)
                    ],
                )?;
            }

            tx.execute(
                "INSERT INTO feeds(id, title, url, html_url, icon_url, folder_id, unread_count, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title,
                    url = excluded.url,
                    html_url = excluded.html_url,
                    icon_url = excluded.icon_url,
                    folder_id = excluded.folder_id,
                    updated_at = excluded.updated_at",
                params![
                    sub.id,
                    if sub.title.is_empty() { &sub.id } else { &sub.title },
                    sub.url,
                    sub.html_url,
                    sub.icon_url,
                    folder_id,
                    now
                ],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO seen_feeds(id) VALUES (?1)",
                params![sub.id],
            )?;
        }

        // A 200 with an empty subscription list would otherwise cascade away
        // every feed and item. Only prune when the server returned at least one
        // feed; an account with no subscriptions keeps its local archive.
        if !subscriptions.is_empty() {
            tx.execute(
                "DELETE FROM feeds WHERE id NOT IN (SELECT id FROM seen_feeds)",
                [],
            )?;
            tx.execute(
                "DELETE FROM folders WHERE id NOT IN (SELECT id FROM seen_folders)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_unread_counts(&self, counts: &[UnreadCount]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for count in counts {
            if count.id.starts_with("feed/") {
                tx.execute(
                    "UPDATE feeds SET unread_count = ?1 WHERE id = ?2",
                    params![count.count, count.id],
                )?;
            }
        }
        if let Some(total) = counts
            .iter()
            .find(|c| c.id == "user/-/state/com.google/reading-list")
        {
            tx.execute(
                "INSERT INTO sync_state(key, value) VALUES ('unread_total', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![total.count.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_items(&self, items: &[Item]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = now();
        let mut affected_feeds = HashSet::new();
        let mut new_items = 0;

        for item in items {
            let Some(feed_id) = item.feed_id() else {
                continue;
            };
            let id = item.short_id();
            let is_new: i64 = tx.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )?;
            let feed_id = feed_id.to_string();
            let feed_title = item.feed_title().unwrap_or(&feed_id).to_string();
            affected_feeds.insert(feed_id.clone());

            tx.execute(
                "INSERT INTO feeds(id, title, unread_count, updated_at) VALUES (?1, ?2, 0, ?3)
                 ON CONFLICT(id) DO NOTHING",
                params![feed_id, feed_title, now],
            )?;

            let content = item.body_html();
            let summary = item
                .summary
                .as_ref()
                .map(|c| c.content.as_str())
                .unwrap_or("");
            let usec = item.usec();

            tx.execute(
                r#"
                INSERT INTO items
                    (id, feed_id, title, author, published, updated, timestamp_usec,
                     crawl_time_msec, href, summary_html, content_html, read, starred, fetched_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                ON CONFLICT(id) DO UPDATE SET
                    feed_id = CASE WHEN excluded.feed_id = 'feed/unknown'
                                   THEN items.feed_id ELSE excluded.feed_id END,
                    title = excluded.title,
                    author = COALESCE(excluded.author, items.author),
                    published = COALESCE(excluded.published, items.published),
                    updated = COALESCE(excluded.updated, items.updated),
                    timestamp_usec = MAX(items.timestamp_usec, excluded.timestamp_usec),
                    crawl_time_msec = MAX(items.crawl_time_msec, excluded.crawl_time_msec),
                    href = COALESCE(excluded.href, items.href),
                    summary_html = CASE WHEN excluded.summary_html = '' THEN items.summary_html
                                        ELSE excluded.summary_html END,
                    content_html = CASE WHEN excluded.content_html = '' THEN items.content_html
                                        ELSE excluded.content_html END,
                    read = CASE WHEN EXISTS (SELECT 1 FROM outbox
                                             WHERE outbox.item_id = items.id
                                               AND outbox.op IN ('read', 'unread'))
                                THEN items.read ELSE excluded.read END,
                    starred = CASE WHEN EXISTS (SELECT 1 FROM outbox
                                                WHERE outbox.item_id = items.id
                                                  AND outbox.op IN ('star', 'unstar'))
                                   THEN items.starred ELSE excluded.starred END,
                    fetched_at = excluded.fetched_at
                "#,
                params![
                    id,
                    feed_id,
                    item.title,
                    item.author,
                    item.published,
                    item.updated,
                    usec,
                    item.crawl_time_msec.unwrap_or(0),
                    item.link(),
                    summary,
                    content,
                    item.is_read() as i32,
                    item.is_starred() as i32,
                    now
                ],
            )?;
            if is_new != 0 {
                new_items += 1;
            }
        }

        for feed_id in affected_feeds {
            tx.execute(
                "UPDATE feeds SET unread_count =
                    (SELECT COUNT(*) FROM items WHERE feed_id = ?1 AND read = 0)
                 WHERE id = ?1",
                params![feed_id],
            )?;
        }
        tx.commit()?;
        Ok(new_items)
    }

    pub fn feeds(&self) -> Result<Vec<FeedRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT f.id, f.title, f.folder_id, fo.name, f.unread_count, f.icon_url, f.html_url
             FROM feeds f
             LEFT JOIN folders fo ON fo.id = f.folder_id
             ORDER BY (fo.name IS NULL), fo.name COLLATE NOCASE, f.title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FeedRow {
                id: row.get(0)?,
                title: row.get(1)?,
                folder_id: row.get(2)?,
                folder_name: row.get(3)?,
                unread_count: row.get(4)?,
                icon_url: row.get(5)?,
                html_url: row.get(6)?,
            })
        })?;
        let mut feeds = Vec::new();
        for row in rows {
            feeds.push(row?);
        }
        Ok(feeds)
    }

    pub fn items(
        &self,
        stream: &Stream,
        limit: i64,
        offset: i64,
        unread_only: bool,
        search: Option<&str>,
    ) -> Result<Vec<ItemSummary>> {
        self.items_filtered(stream, limit, offset, unread_only.then_some(false), search)
    }

    pub fn items_filtered(
        &self,
        stream: &Stream,
        limit: i64,
        offset: i64,
        read: Option<bool>,
        search: Option<&str>,
    ) -> Result<Vec<ItemSummary>> {
        let (filter, mut values) = stream_where(stream);
        let mut clauses = vec![filter];
        if let Some(read) = read {
            clauses.push(if read {
                "items.read = 1".into()
            } else {
                "items.read = 0".into()
            });
        }
        if let Some(term) = search.filter(|s| !s.trim().is_empty()) {
            let pattern = format!("%{}%", term.trim());
            if let Some(query) = fts_query(term) {
                clauses.push(
                    "(items.rowid IN (SELECT rowid FROM items_fts WHERE items_fts MATCH ?)
                      OR COALESCE(feeds.title, '') LIKE ?)"
                        .into(),
                );
                values.push(Value::Text(query));
                values.push(Value::Text(pattern));
            } else {
                clauses.push(
                    "(items.title LIKE ? OR items.author LIKE ?
                      OR items.content_html LIKE ? OR items.summary_html LIKE ?
                      OR COALESCE(feeds.title, '') LIKE ?)"
                        .into(),
                );
                for _ in 0..5 {
                    values.push(Value::Text(pattern.clone()));
                }
            }
        }
        values.push(Value::Integer(limit.clamp(1, 1000)));
        values.push(Value::Integer(offset.max(0)));

        let sql = format!(
            "SELECT items.id, items.feed_id, COALESCE(feeds.title, ''),
                    items.title, items.author, items.published, items.href,
                    items.read, items.starred, items.timestamp_usec
             FROM items
             LEFT JOIN feeds ON feeds.id = items.feed_id
             WHERE {}
             ORDER BY items.timestamp_usec DESC, items.id DESC
             LIMIT ? OFFSET ?",
            clauses.join(" AND ")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok(ItemSummary {
                id: row.get(0)?,
                feed_id: row.get(1)?,
                feed_title: row.get(2)?,
                title: row.get(3)?,
                author: row.get(4)?,
                published: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                href: row.get(6)?,
                read: row.get::<_, i64>(7)? != 0,
                starred: row.get::<_, i64>(8)? != 0,
                timestamp_usec: row.get(9)?,
            })
        })?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    pub fn item(&self, id: &str) -> Result<Option<ItemRow>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT items.id, items.feed_id, COALESCE(feeds.title, ''),
                        items.title, items.author, items.published, items.href,
                        COALESCE(items.content_html, ''), COALESCE(items.summary_html, ''),
                        items.read, items.starred, items.timestamp_usec
                 FROM items LEFT JOIN feeds ON feeds.id = items.feed_id
                 WHERE items.id = ?1",
                params![id],
                |row| {
                    Ok(ItemRow {
                        id: row.get(0)?,
                        feed_id: row.get(1)?,
                        feed_title: row.get(2)?,
                        title: row.get(3)?,
                        author: row.get(4)?,
                        published: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                        href: row.get(6)?,
                        content_html: row.get(7)?,
                        summary_html: row.get(8)?,
                        read: row.get::<_, i64>(9)? != 0,
                        starred: row.get::<_, i64>(10)? != 0,
                        timestamp_usec: row.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn cache_bytes(&self) -> Result<u64> {
        let conn = self.conn();
        let bytes: i64 = conn.query_row(
            "SELECT
                 (SELECT COALESCE(SUM(
                     LENGTH(CAST(COALESCE(content_html, '') AS BLOB)) +
                     LENGTH(CAST(COALESCE(summary_html, '') AS BLOB))
                 ), 0) FROM items) +
                 (SELECT COALESCE(SUM(LENGTH(data)), 0) FROM media)",
            [],
            |row| row.get(0),
        )?;
        Ok(bytes.max(0) as u64)
    }

    /// Clears bodies of the oldest read, unstarred items until the cache fits.
    pub fn enforce_cache_budget(&self, max_bytes: u64) -> Result<usize> {
        let mut bytes: u64 = self.cache_bytes()?;
        if bytes <= max_bytes {
            return Ok(0);
        }

        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut evicted = 0;
        loop {
            let candidates: Vec<(String, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT id,
                            LENGTH(CAST(COALESCE(content_html, '') AS BLOB)) +
                            LENGTH(CAST(COALESCE(summary_html, '') AS BLOB)) AS size
                     FROM items
                     WHERE read = 1 AND starred = 0
                       AND (LENGTH(CAST(COALESCE(content_html, '') AS BLOB)) +
                            LENGTH(CAST(COALESCE(summary_html, '') AS BLOB))) > 0
                     ORDER BY timestamp_usec ASC, id ASC
                     LIMIT 100",
                )?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if candidates.is_empty() {
                break;
            }
            for (id, size) in candidates {
                tx.execute(
                    "UPDATE items SET content_html = '', summary_html = '' WHERE id = ?1",
                    params![id],
                )?;
                bytes = bytes.saturating_sub(size.max(0) as u64);
                evicted += 1;
                if bytes <= max_bytes {
                    break;
                }
            }
            if bytes <= max_bytes {
                break;
            }
        }
        while bytes > max_bytes {
            let media: Vec<(String, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT url, LENGTH(data) FROM media
                     ORDER BY fetched_at ASC, url ASC LIMIT 100",
                )?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if media.is_empty() {
                break;
            }
            for (url, size) in media {
                tx.execute("DELETE FROM media WHERE url = ?1", params![url])?;
                bytes = bytes.saturating_sub(size.max(0) as u64);
                evicted += 1;
                if bytes <= max_bytes {
                    break;
                }
            }
        }
        tx.commit()?;
        Ok(evicted)
    }

    pub fn cached_media(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT data FROM media WHERE url = ?1",
                params![url],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn put_media(&self, url: &str, data: &[u8]) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO media(url, data, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(url) DO UPDATE SET data = excluded.data,
                                           fetched_at = excluded.fetched_at",
            params![url, data, now()],
        )?;
        Ok(())
    }

    pub fn set_read(&self, ids: &[String], read: bool) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let op = if read { OpKind::Read } else { OpKind::Unread };
        for id in ids {
            tx.execute(
                "UPDATE items SET read = ?1 WHERE id = ?2",
                params![read as i32, id],
            )?;
            tx.execute(
                "DELETE FROM outbox WHERE item_id = ?1 AND op IN ('read', 'unread')",
                params![id],
            )?;
            tx.execute(
                "INSERT INTO outbox(item_id, op, created_at) VALUES (?1, ?2, ?3)",
                params![id, op.as_str(), now()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_starred(&self, ids: &[String], starred: bool) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let op = if starred {
            OpKind::Star
        } else {
            OpKind::Unstar
        };
        for id in ids {
            tx.execute(
                "UPDATE items SET starred = ?1 WHERE id = ?2",
                params![starred as i32, id],
            )?;
            tx.execute(
                "DELETE FROM outbox WHERE item_id = ?1 AND op IN ('star', 'unstar')",
                params![id],
            )?;
            tx.execute(
                "INSERT INTO outbox(item_id, op, created_at) VALUES (?1, ?2, ?3)",
                params![id, op.as_str(), now()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_all_read(&self, stream: &Stream, ts_us: i64) -> Result<()> {
        let (filter, values) = stream_where(stream);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            &format!("UPDATE items SET read = 1 WHERE {filter}"),
            params_from_iter(values.iter()),
        )?;
        tx.execute(
            &format!(
                "DELETE FROM outbox WHERE op IN ('read', 'unread')
                 AND item_id IN (SELECT items.id FROM items WHERE {filter})"
            ),
            params_from_iter(values.iter()),
        )?;
        tx.execute(
            "INSERT INTO outbox(stream_id, op, ts_us, created_at)
             VALUES (?1, 'mark_all_read', ?2, ?3)",
            params![stream.api_id(), ts_us, now()],
        )?;
        tx.execute(
            "UPDATE feeds SET unread_count =
                (SELECT COUNT(*) FROM items WHERE items.feed_id = feeds.id AND items.read = 0)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn outbox(&self, limit: i64) -> Result<Vec<OutboxOp>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, item_id, stream_id, op, ts_us FROM outbox
                 WHERE attempts < ?2 ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit, MAX_PUSH_ATTEMPTS], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut ops = Vec::new();
        for row in rows {
            let (id, item_id, stream_id, op, ts_us) = row?;
            // Ops from a newer version stay queued instead of being guessed at or dropped.
            let Some(op) = OpKind::parse(&op) else {
                continue;
            };
            ops.push(OutboxOp {
                id,
                item_id,
                stream_id,
                op,
                ts_us,
            });
        }
        Ok(ops)
    }

    pub fn remove_ops(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn dead_letter_ops(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE outbox SET attempts = ?2 WHERE id = ?1",
                params![id, MAX_PUSH_ATTEMPTS],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn dead_outbox_count(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE attempts >= ?1",
            params![MAX_PUSH_ATTEMPTS],
            |row| row.get(0),
        )?)
    }

    /// The caller must ensure `remote_unread` is the complete unread set.
    pub fn reconcile_read_state(&self, remote_unread: &HashSet<String>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        fill_remote_ids(&tx, remote_unread)?;
        tx.execute(
            "UPDATE items SET read = 1 WHERE read = 0
             AND id NOT IN (SELECT id FROM remote_ids)
             AND timestamp_usec NOT IN (SELECT ts FROM remote_timestamps)
             AND NOT EXISTS (SELECT 1 FROM outbox
                             WHERE outbox.item_id = items.id
                               AND outbox.op IN ('read', 'unread')
                               AND outbox.attempts < ?1)",
            params![MAX_PUSH_ATTEMPTS],
        )?;
        tx.execute(
            "UPDATE items SET read = 0 WHERE read = 1
             AND (id IN (SELECT id FROM remote_ids)
                  OR timestamp_usec IN (SELECT ts FROM remote_timestamps))
             AND NOT EXISTS (SELECT 1 FROM outbox
                             WHERE outbox.item_id = items.id
                               AND outbox.op IN ('read', 'unread')
                               AND outbox.attempts < ?1)",
            params![MAX_PUSH_ATTEMPTS],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The caller must ensure `remote_starred` is the complete starred set.
    pub fn reconcile_starred_state(&self, remote_starred: &HashSet<String>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        fill_remote_ids(&tx, remote_starred)?;
        tx.execute(
            "UPDATE items SET starred = 0 WHERE starred = 1
             AND id NOT IN (SELECT id FROM remote_ids)
             AND timestamp_usec NOT IN (SELECT ts FROM remote_timestamps)
             AND NOT EXISTS (SELECT 1 FROM outbox
                             WHERE outbox.item_id = items.id
                               AND outbox.op IN ('star', 'unstar')
                               AND outbox.attempts < ?1)",
            params![MAX_PUSH_ATTEMPTS],
        )?;
        tx.execute(
            "UPDATE items SET starred = 1
             WHERE (id IN (SELECT id FROM remote_ids)
                    OR timestamp_usec IN (SELECT ts FROM remote_timestamps))
               AND NOT EXISTS (SELECT 1 FROM outbox
                               WHERE outbox.item_id = items.id
                                 AND outbox.op IN ('star', 'unstar')
                                 AND outbox.attempts < ?1)",
            params![MAX_PUSH_ATTEMPTS],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns remote IDs that are not in the local items table. FreshRSS reports
    /// decimal IDs, which map to our microsecond timestamps; other servers may
    /// report the same short IDs we store.
    pub fn missing_remote_ids(&self, ids: &HashSet<String>) -> Result<Vec<String>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS wanted_ids (id TEXT PRIMARY KEY);
             DELETE FROM wanted_ids;",
        )?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO wanted_ids(id) VALUES (?1)")?;
            for id in ids {
                insert.execute(params![id])?;
            }
        }
        let missing = {
            let mut stmt = tx.prepare(
                "SELECT w.id FROM wanted_ids w
                 WHERE NOT EXISTS (
                     SELECT 1 FROM items i
                     WHERE i.id = w.id
                        OR i.id = replace(
                            w.id, 'tag:google.com,2005:reader/item/', ''
                        )
                 )
                   AND NOT EXISTS (SELECT 1 FROM items i
                                   WHERE w.id NOT GLOB '*[^0-9]*'
                                     AND i.timestamp_usec = CAST(w.id AS INTEGER))",
            )?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?
        };
        tx.commit()?;
        Ok(missing)
    }

    pub fn newest_item_usec(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(timestamp_usec), 0) FROM items",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn total_unread(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(
            conn.query_row("SELECT COUNT(*) FROM items WHERE read = 0", [], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_label(id: &str) -> bool {
    id.contains("/label/")
}

/// Loads remote IDs into temp tables keyed by both forms the API reports them in.
fn fill_remote_ids(tx: &Transaction<'_>, ids: &HashSet<String>) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS remote_ids (id TEXT PRIMARY KEY);
         DELETE FROM remote_ids;
         CREATE TEMP TABLE IF NOT EXISTS remote_timestamps (ts INTEGER PRIMARY KEY);
         DELETE FROM remote_timestamps;",
    )?;
    for id in ids {
        tx.execute(
            "INSERT OR IGNORE INTO remote_ids(id) VALUES (?1)",
            params![id],
        )?;
        // FreshRSS reports decimal entry IDs here; matching them as integers
        // keeps the timestamp lookup indexed instead of scanning every item.
        if let Ok(ts) = id.parse::<i64>() {
            tx.execute(
                "INSERT OR IGNORE INTO remote_timestamps(ts) VALUES (?1)",
                params![ts],
            )?;
        }
    }
    Ok(())
}

fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

fn stream_where(stream: &Stream) -> (String, Vec<Value>) {
    match stream {
        Stream::All => ("1 = 1".into(), vec![]),
        Stream::Starred => ("items.starred = 1".into(), vec![]),
        Stream::Feed(id) => ("items.feed_id = ?".into(), vec![Value::Text(id.clone())]),
        Stream::Folder(id) => (
            "items.feed_id IN (SELECT id FROM feeds WHERE folder_id = ?)".into(),
            vec![Value::Text(id.clone())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Category, Content, Origin};

    fn subscription(id: &str, title: &str, folder: Option<&str>) -> Subscription {
        Subscription {
            id: id.into(),
            title: title.into(),
            categories: folder
                .map(|f| {
                    vec![Category {
                        id: format!("user/-/label/{f}"),
                        label: f.into(),
                    }]
                })
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    fn item(id: &str, feed: &str, usec: i64, read: bool) -> Item {
        Item {
            id: id.into(),
            title: format!("Item {id}"),
            published: Some(usec / 1_000_000),
            timestamp_usec: Some(usec),
            content: Some(Content {
                content: format!("<p>Body {id}</p>"),
            }),
            origin: Some(Origin {
                stream_id: feed.into(),
                title: Some("Feed".into()),
                html_url: None,
            }),
            categories: if read {
                vec!["user/-/state/com.google/read".into()]
            } else {
                vec![]
            },
            ..Default::default()
        }
    }

    #[test]
    fn feeds_items_and_state() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(
                &[subscription("feed/1", "One", Some("News"))],
                &[Tag {
                    id: "user/-/label/News".into(),
                    label: Some("News".into()),
                }],
            )
            .unwrap();
        store
            .upsert_items(&[item("1", "feed/1", 1_000_000, false)])
            .unwrap();

        let feeds = store.feeds().unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].folder_name.as_deref(), Some("News"));
        assert_eq!(feeds[0].unread_count, 1);

        let items = store.items(&Stream::All, 10, 0, false, None).unwrap();
        assert_eq!(items.len(), 1);
        assert!(!items[0].read);
        assert_eq!(
            store
                .items_filtered(&Stream::All, 10, 0, Some(false), None)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .items_filtered(&Stream::All, 10, 0, Some(true), None)
                .unwrap()
                .is_empty()
        );

        store.set_read(&["1".into()], true).unwrap();
        assert!(store.items(&Stream::All, 10, 0, false, None).unwrap()[0].read);
        assert!(
            store
                .items_filtered(&Stream::All, 10, 0, Some(false), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .items_filtered(&Stream::All, 10, 0, Some(true), None)
                .unwrap()
                .len(),
            1
        );
        let ops = store.outbox(10).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, OpKind::Read);

        store.remove_ops(&[ops[0].id]).unwrap();
        assert!(store.outbox(10).unwrap().is_empty());
    }

    #[test]
    fn normalizes_google_reader_item_ids() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        let mut item = item(
            "tag:google.com,2005:reader/item/abc",
            "feed/1",
            1_000_000,
            false,
        );
        item.id = "tag:google.com,2005:reader/item/abc".into();
        store.upsert_items(&[item]).unwrap();
        assert!(store.item("abc").unwrap().is_some());
        assert!(
            store
                .item("tag:google.com,2005:reader/item/abc")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconciles_remote_read_state() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        store
            .upsert_items(&[
                item("1", "feed/1", 1_000_000, false),
                item("2", "feed/1", 2_000_000, false),
            ])
            .unwrap();

        store
            .reconcile_read_state(&HashSet::from(["1".to_string()]))
            .unwrap();
        let items = store.items(&Stream::All, 10, 0, false, None).unwrap();
        assert!(!items.iter().find(|i| i.id == "1").unwrap().read);
        assert!(items.iter().find(|i| i.id == "2").unwrap().read);

        // FreshRSS reports decimal entry IDs in stream/items/ids.
        store
            .reconcile_read_state(&HashSet::from(["2000000".to_string()]))
            .unwrap();
        store
            .reconcile_starred_state(&HashSet::from(["2000000".to_string()]))
            .unwrap();
        let items = store.items(&Stream::All, 10, 0, false, None).unwrap();
        assert!(items.iter().find(|i| i.id == "1").unwrap().read);
        assert!(!items.iter().find(|i| i.id == "2").unwrap().read);
        assert!(!items.iter().find(|i| i.id == "1").unwrap().starred);
        assert!(items.iter().find(|i| i.id == "2").unwrap().starred);
    }

    #[test]
    fn empty_subscription_list_does_not_delete_local_feeds() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        store
            .upsert_items(&[item("1", "feed/1", 1_000_000, false)])
            .unwrap();

        store.reconcile_feeds(&[], &[]).unwrap();
        assert_eq!(store.feeds().unwrap().len(), 1);
        assert!(store.item("1").unwrap().is_some());
    }

    #[test]
    fn finds_remote_ids_missing_locally() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        store
            .upsert_items(&[
                item("abc", "feed/1", 1_000_000, false),
                item("def", "feed/1", 2_000_000, false),
            ])
            .unwrap();
        let mut wanted = HashSet::from([
            "abc".to_string(),
            "2000000".to_string(),
            "999".to_string(),
            "tag:google.com,2005:reader/item/abc".to_string(),
        ]);
        let missing = store.missing_remote_ids(&wanted).unwrap();
        assert_eq!(missing, vec!["999".to_string()]);

        wanted.remove("999");
        assert!(store.missing_remote_ids(&wanted).unwrap().is_empty());
    }

    #[test]
    fn reconciliation_preserves_pending_state() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        store
            .upsert_items(&[item("1", "feed/1", 1_000_000, false)])
            .unwrap();
        store.set_read(&["1".into()], true).unwrap();
        store.set_starred(&["1".into()], true).unwrap();

        // The server has not seen these queued changes yet.
        store
            .reconcile_read_state(&HashSet::from(["1".to_string()]))
            .unwrap();
        store.reconcile_starred_state(&HashSet::new()).unwrap();
        let pending = store.item("1").unwrap().unwrap();
        assert!(pending.read);
        assert!(pending.starred);

        let op_ids: Vec<i64> = store.outbox(10).unwrap().iter().map(|op| op.id).collect();
        store.dead_letter_ops(&op_ids).unwrap();
        assert!(store.outbox(10).unwrap().is_empty());
        assert_eq!(store.dead_outbox_count().unwrap(), 2);
        store
            .reconcile_read_state(&HashSet::from(["1".to_string()]))
            .unwrap();
        store.reconcile_starred_state(&HashSet::new()).unwrap();
        let reconciled = store.item("1").unwrap().unwrap();
        assert!(!reconciled.read);
        assert!(!reconciled.starred);
    }

    #[test]
    fn retains_outbox_rows_across_reopen() {
        let dir =
            std::env::temp_dir().join(format!("nieuwser-outbox-{}-{}", std::process::id(), now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
                .unwrap();
            store
                .upsert_items(&[item("1", "feed/1", 1_000_000, false)])
                .unwrap();
            store.set_read(&["1".into()], true).unwrap();
            assert_eq!(store.outbox(10).unwrap().len(), 1);
        }
        {
            let store = Store::open(&path).unwrap();
            let ops = store.outbox(10).unwrap();
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].op, OpKind::Read);
            assert_eq!(ops[0].item_id.as_deref(), Some("1"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn queues_mark_all_read_in_microseconds() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();
        store
            .upsert_items(&[item("1", "feed/1", 1_000_000, false)])
            .unwrap();

        store
            .mark_all_read(&Stream::All, 1_700_000_000_123_456)
            .unwrap();
        let ops = store.outbox(10).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, OpKind::MarkAllRead);
        assert_eq!(ops[0].ts_us, Some(1_700_000_000_123_456));
    }

    #[test]
    fn migrates_nanosecond_mark_all_read_outbox_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id TEXT,
                stream_id TEXT,
                op TEXT NOT NULL,
                ts_ns INTEGER,
                created_at INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO outbox(stream_id, op, ts_ns, created_at)
            VALUES ('user/-/state/com.google/reading-list', 'mark_all_read', 1700000000000000000, 0);",
        )
        .unwrap();
        let store = Store::from_connection(conn).unwrap();
        let ops = store.outbox(10).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].ts_us, Some(1_700_000_000_000_000));
    }

    #[test]
    fn stores_account_token() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.account().unwrap().is_none());
        assert!(store.auth_token().unwrap().is_none());

        store
            .set_account("https://example.com/api/greader.php", "me")
            .unwrap();
        assert!(store.auth_token().unwrap().is_none());
        store.set_auth_token(Some("token")).unwrap();
        assert_eq!(store.auth_token().unwrap().as_deref(), Some("token"));
        store.set_auth_token(None).unwrap();
        assert!(store.auth_token().unwrap().is_none());
    }

    #[test]
    fn migrates_account_token_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                base_url TEXT NOT NULL,
                email TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        let store = Store::from_connection(conn).unwrap();
        store
            .set_account("https://example.com/api/greader.php", "me")
            .unwrap();
        store.set_auth_token(Some("token")).unwrap();
        assert_eq!(store.auth_token().unwrap().as_deref(), Some("token"));
    }

    #[test]
    fn searches_titles_authors_and_bodies() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();

        let mut first = item("1", "feed/1", 2_000_000, true);
        first.title = "Rust news".into();
        let mut second = item("2", "feed/1", 1_000_000, true);
        second.author = Some("Alice".into());
        second.content = Some(Content {
            content: "<p>FreshRSS rocks</p>".into(),
        });
        store.upsert_items(&[first.clone(), second]).unwrap();

        let search = |term: &str| {
            store
                .items(&Stream::All, 10, 0, false, Some(term))
                .unwrap()
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(search("rust"), vec!["1"]);
        assert_eq!(search("rus"), vec!["1"]);
        assert_eq!(search("alice"), vec!["2"]);
        assert_eq!(search("freshrss"), vec!["2"]);
        assert_eq!(search("one"), vec!["1", "2"]);

        first.title = "Python news".into();
        store.upsert_items(&[first]).unwrap();
        assert!(search("rust").is_empty());
        assert_eq!(search("python"), vec!["1"]);
    }

    #[test]
    fn backfills_search_index_for_existing_items() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (
                id TEXT PRIMARY KEY,
                feed_id TEXT,
                title TEXT NOT NULL,
                author TEXT,
                published INTEGER,
                updated INTEGER,
                timestamp_usec INTEGER NOT NULL DEFAULT 0,
                crawl_time_msec INTEGER NOT NULL DEFAULT 0,
                href TEXT,
                summary_html TEXT,
                content_html TEXT,
                read INTEGER NOT NULL DEFAULT 0,
                starred INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL
            );
            INSERT INTO items(id, feed_id, title, content_html, fetched_at)
            VALUES ('1', 'feed/1', 'Legacy rust', '<p>body</p>', 0);",
        )
        .unwrap();
        let store = Store::from_connection(conn).unwrap();
        let results = store
            .items(&Stream::All, 10, 0, false, Some("legacy"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "1");
    }

    #[test]
    fn cache_budget_evicts_oldest_media() {
        let store = Store::open_in_memory().unwrap();
        store.put_media("a", &[1u8; 1000]).unwrap();
        store.put_media("b", &[2u8; 1000]).unwrap();
        assert_eq!(store.cache_bytes().unwrap(), 2000);
        assert_eq!(store.enforce_cache_budget(1500).unwrap(), 1);
        assert!(store.cached_media("a").unwrap().is_none());
        assert!(store.cached_media("b").unwrap().is_some());
    }

    #[test]
    fn cache_budget_evicts_only_old_read_bodies() {
        let store = Store::open_in_memory().unwrap();
        store
            .reconcile_feeds(&[subscription("feed/1", "One", None)], &[])
            .unwrap();

        let mut old_read = item("1", "feed/1", 1_000_000, true);
        old_read.content = Some(Content {
            content: "a".repeat(1000),
        });
        let mut new_read = item("2", "feed/1", 2_000_000, true);
        new_read.content = Some(Content {
            content: "b".repeat(1000),
        });
        let mut unread = item("3", "feed/1", 500_000, false);
        unread.content = Some(Content {
            content: "c".repeat(1000),
        });
        let mut starred = item("4", "feed/1", 600_000, true);
        starred.content = Some(Content {
            content: "d".repeat(1000),
        });
        starred
            .categories
            .push("user/-/state/com.google/starred".into());

        store
            .upsert_items(&[old_read, new_read, unread, starred])
            .unwrap();
        assert_eq!(store.cache_bytes().unwrap(), 4000);

        assert_eq!(store.enforce_cache_budget(2500).unwrap(), 2);
        assert!(store.item("1").unwrap().unwrap().content_html.is_empty());
        assert!(store.item("2").unwrap().unwrap().content_html.is_empty());
        assert!(!store.item("3").unwrap().unwrap().content_html.is_empty());
        assert!(!store.item("4").unwrap().unwrap().content_html.is_empty());
        assert!(store.cache_bytes().unwrap() <= 2500);
        assert_eq!(store.enforce_cache_budget(2500).unwrap(), 0);
    }
}
