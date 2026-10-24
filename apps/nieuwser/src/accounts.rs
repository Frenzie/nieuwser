//! Per-server/account database selection.
//!
//! Normal installs keep one SQLite database per FreshRSS base URL + username
//! under `<data dir>/accounts/<key>/nieuwser.db`. A small pointer file records
//! which account is active. The previous single `<data dir>/nieuwser.db` is
//! migrated on first launch when it already contains an account.
//!
//! `NIEUWSER_DB` remains a single-database escape hatch for tests and manual
//! use; switching accounts is disabled while it is set.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nieuwser_core::Store;

const LEGACY_DB_NAME: &str = "nieuwser.db";
const ACCOUNTS_DIR: &str = "accounts";
const ACTIVE_ACCOUNT_FILE: &str = "active-account";

pub fn data_root() -> PathBuf {
    if let Some(path) = std::env::var_os("NIEUWSER_DATA_DIR") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", "nieuwser")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn legacy_db_path(root: &Path) -> PathBuf {
    root.join(LEGACY_DB_NAME)
}

pub fn account_db_path(root: &Path, key: &str) -> PathBuf {
    root.join(ACCOUNTS_DIR).join(key).join(LEGACY_DB_NAME)
}

pub fn active_account_path(root: &Path) -> PathBuf {
    root.join(ACTIVE_ACCOUNT_FILE)
}

/// Stable, filesystem-safe key for a normalized server URL and username.
/// FNV-1a is used to avoid pulling in a hash dependency; the 64-bit space is
/// ample for the handful of accounts a client will hold.
pub fn account_key(base_url: &str, email: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in base_url
        .bytes()
        .chain(std::iter::once(0))
        .chain(email.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn valid_key(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn read_active_key(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(active_account_path(root)).ok()?;
    let key = text.trim();
    valid_key(key).then(|| key.to_string())
}

pub fn write_active_key(root: &Path, key: &str) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("create data directory {}", root.display()))?;
    let active = active_account_path(root);
    let temp = root.join(format!(".active-account.{}.tmp", std::process::id()));
    std::fs::write(&temp, format!("{key}\n"))
        .with_context(|| format!("write active account {}", temp.display()))?;
    if std::fs::rename(&temp, &active).is_err() {
        // Windows and some filesystems do not replace an existing file.
        let _ = std::fs::remove_file(&active);
        std::fs::rename(&temp, &active)
            .with_context(|| format!("write active account {}", active.display()))?;
    }
    Ok(())
}

fn migrate_legacy_db(legacy: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create account directory {}", parent.display()))?;
    }
    for suffix in ["-wal", "-shm", "-journal", ""] {
        let from = PathBuf::from(format!("{}{suffix}", legacy.display()));
        if from.exists() {
            let to = PathBuf::from(format!("{}{suffix}", target.display()));
            std::fs::rename(&from, &to)
                .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn resolve_managed_db_path(root: &Path) -> Result<PathBuf> {
    if let Some(key) = read_active_key(root) {
        let db = account_db_path(root, &key);
        if db.is_file() {
            return Ok(db);
        }
    }

    let legacy = legacy_db_path(root);
    if legacy.is_file() {
        let account = {
            let store = Store::open(&legacy)?;
            store.account()?
        };
        if let Some((base, email)) = account {
            let key = account_key(&base, &email);
            let target = account_db_path(root, &key);
            if target.is_file() {
                write_active_key(root, &key)?;
                return Ok(target);
            }
            // Point at the target before moving so an interrupted migration
            // retries on the next launch instead of orphaning the database.
            write_active_key(root, &key)?;
            migrate_legacy_db(&legacy, &target)?;
            return Ok(target);
        }
    }
    Ok(legacy)
}

pub fn resolve_db_path(root: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NIEUWSER_DB") {
        return Ok(PathBuf::from(path));
    }
    resolve_managed_db_path(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nieuwser_core::models::{Content, Item, Origin, Subscription};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn seed_read_op(store: &Store, id: &str, timestamp_usec: i64) {
        store
            .reconcile_feeds(
                &[Subscription {
                    id: "feed/1".into(),
                    title: "One".into(),
                    ..Default::default()
                }],
                &[],
            )
            .unwrap();
        store
            .upsert_items(&[Item {
                id: id.into(),
                title: format!("Item {id}"),
                timestamp_usec: Some(timestamp_usec),
                content: Some(Content {
                    content: "<p>Body</p>".into(),
                }),
                origin: Some(Origin {
                    stream_id: "feed/1".into(),
                    title: Some("One".into()),
                    html_url: None,
                }),
                ..Default::default()
            }])
            .unwrap();
        store.set_read(&[id.to_string()], true).unwrap();
        assert_eq!(store.outbox(10).unwrap().len(), 1);
    }

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("nieuwser-accounts-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn account_key_is_stable_and_distinct() {
        let a = account_key("https://example.com/api/greader.php", "me");
        let b = account_key("https://example.com/api/greader.php", "me");
        let c = account_key("https://other.example/api/greader.php", "me");
        let d = account_key("https://example.com/api/greader.php", "you");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(valid_key(&a));
    }

    #[test]
    fn migrates_a_legacy_account_database() {
        let root = temp_root();
        let legacy = legacy_db_path(&root);
        let base = "https://example.com/api/greader.php";
        let email = "me";
        {
            let store = Store::open(&legacy).unwrap();
            store.set_account(base, email).unwrap();
            store.set_auth_token(Some("token")).unwrap();
            store.set_state("marker", "kept").unwrap();
            seed_read_op(&store, "1", 1_000_000);
        }

        let path = resolve_managed_db_path(&root).unwrap();
        let key = account_key(base, email);
        assert_eq!(path, account_db_path(&root, &key));
        assert!(path.is_file());
        assert!(!legacy.exists());
        assert_eq!(read_active_key(&root).as_deref(), Some(key.as_str()));

        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.account().unwrap(),
            Some((base.to_string(), email.to_string()))
        );
        assert_eq!(store.auth_token().unwrap().as_deref(), Some("token"));
        assert_eq!(store.state("marker").unwrap().as_deref(), Some("kept"));
        assert_eq!(store.outbox(10).unwrap().len(), 1);
        assert!(store.item("1").unwrap().is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn separate_databases_keep_outboxes_and_articles_isolated() {
        let root = temp_root();
        let first = account_db_path(&root, "first");
        let second = account_db_path(&root, "second");
        for path in [&first, &second] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }

        {
            let store = Store::open(&first).unwrap();
            seed_read_op(&store, "1", 1_000_000);
        }
        {
            let store = Store::open(&second).unwrap();
            seed_read_op(&store, "2", 2_000_000);
        }

        let first = Store::open(&first).unwrap();
        let second = Store::open(&second).unwrap();
        assert_eq!(first.outbox(10).unwrap()[0].item_id.as_deref(), Some("1"));
        assert_eq!(second.outbox(10).unwrap()[0].item_id.as_deref(), Some("2"));
        assert!(first.item("1").unwrap().is_some());
        assert!(first.item("2").unwrap().is_none());
        assert!(second.item("2").unwrap().is_some());
        assert!(second.item("1").unwrap().is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolves_the_active_account_and_keeps_a_bootstrap_database() {
        let root = temp_root();
        let empty = legacy_db_path(&root);
        {
            let store = Store::open(&empty).unwrap();
            assert!(store.account().unwrap().is_none());
        }
        assert_eq!(resolve_managed_db_path(&root).unwrap(), empty);

        let key = account_key("https://example.com/api/greader.php", "me");
        let target = account_db_path(&root, &key);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        Store::open(&target).unwrap();
        write_active_key(&root, &key).unwrap();
        assert_eq!(resolve_managed_db_path(&root).unwrap(), target);
        std::fs::remove_dir_all(&root).ok();
    }
}
