use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nieuwser_core::models::Stream;
use nieuwser_core::{Store, SyncEngine};

#[derive(Parser)]
#[command(name = "nieuwser-cli", about = "Sync and inspect a FreshRSS account")]
struct Cli {
    #[arg(long, env = "NIEUWSER_API_URL")]
    base: Option<String>,
    #[arg(long, env = "NIEUWSER_EMAIL")]
    email: Option<String>,
    #[arg(long, env = "FRESHRSS_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    #[arg(long, env = "NIEUWSER_DB", default_value = "nieuwser.db")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pull feeds/items and push pending local changes.
    Sync,
    /// List feeds with unread counts.
    Feeds,
    /// List cached articles.
    Items {
        #[arg(long, default_value = "all")]
        stream: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        search: Option<String>,
    },
    /// Mark local articles read or unread.
    MarkRead {
        ids: Vec<String>,
        #[arg(long)]
        unread: bool,
    },
    /// Mark a stream read locally and queue the server update.
    MarkAllRead {
        #[arg(long, default_value = "all")]
        stream: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Sync => {
            let base = cli.base.context("--base or NIEUWSER_API_URL is required")?;
            let email = cli.email.context("--email or NIEUWSER_EMAIL is required")?;
            let password = cli
                .password
                .context("--password or FRESHRSS_PASSWORD is required")?;
            let store = Arc::new(Store::open(&cli.db)?);
            let mut engine = SyncEngine::new(store, &base, &email, &password).await?;
            println!("{}", engine.sync().await?);
        }
        Command::Feeds => {
            let store = Store::open(&cli.db)?;
            for feed in store.feeds()? {
                let folder = feed.folder_name.unwrap_or_else(|| "-".into());
                println!("{:>5}  {:<24} {}", feed.unread_count, folder, feed.title);
            }
        }
        Command::Items {
            stream,
            limit,
            unread,
            search,
        } => {
            let stream = Stream::parse(&stream).context("unknown stream")?;
            let store = Store::open(&cli.db)?;
            for item in store.items(&stream, limit, 0, unread, search.as_deref())? {
                let flag = if item.read { " " } else { "*" };
                let star = if item.starred { "+" } else { " " };
                println!("{flag}{star} {:<80} [{}]", truncate(&item.title, 80), item.feed_title);
            }
        }
        Command::MarkRead { ids, unread } => {
            let store = Store::open(&cli.db)?;
            store.set_read(&ids, !unread)?;
            println!("queued {} article(s)", ids.len());
        }
        Command::MarkAllRead { stream } => {
            let stream = Stream::parse(&stream).context("unknown stream")?;
            let store = Store::open(&cli.db)?;
            let ts_ns = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;
            store.mark_all_read(&stream, ts_ns)?;
            println!("queued mark-all-read for {}", stream.api_id());
        }
    }
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
