slint::include_modules!();

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use nieuwser_core::models::Stream;
use nieuwser_core::reader::html_to_text;
use nieuwser_core::store::ItemRow;
use nieuwser_core::{Store, SyncEngine};
use slint::{ComponentHandle, ModelRc, VecModel};

#[derive(Default)]
struct UiState {
    streams: Vec<Stream>,
    items: Vec<ItemRow>,
}

fn main() -> Result<()> {
    let db_path = db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Arc::new(Store::open(&db_path)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = MainWindow::new()?;
    let state = Arc::new(Mutex::new(UiState::default()));

    refresh_feeds(&ui, &store, &state)?;
    load_stream(&ui, &store, &state, 0)?;
    bind_callbacks(&ui, &store, &state, &runtime);

    if let Some((base, email)) = store.account()? {
        ui.set_settings_url(base.into());
        ui.set_settings_email(email.into());
        ui.set_settings_has_account(true);
    }
    if store.auth_token()?.is_none() {
        ui.set_settings_open(true);
        ui.set_status("Connect your FreshRSS account to sync.".into());
    } else {
        ui.set_status(format!("Ready. DB: {}", db_path.display()).into());
    }

    ui.run()?;
    Ok(())
}

fn bind_callbacks(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    runtime: &tokio::runtime::Runtime,
) {
    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_open_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                match store.account() {
                    Ok(Some((base, email))) => {
                        ui.set_settings_url(base.into());
                        ui.set_settings_email(email.into());
                        ui.set_settings_has_account(true);
                    }
                    _ => ui.set_settings_has_account(false),
                }
                ui.set_settings_status("".into());
                ui.set_settings_open(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_close_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_settings_has_account() {
                    ui.set_settings_open(false);
                    ui.set_settings_password("".into());
                    ui.set_settings_status("".into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        let handle = runtime.handle().clone();
        ui.on_save_settings(move |url, email, password| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let url = url.trim().to_string();
            let email = email.trim().to_string();
            let password = password.to_string();
            if url.is_empty() || email.is_empty() || password.is_empty() {
                ui.set_settings_status("Fill in the server URL, username, and password.".into());
                return;
            }
            ui.set_settings_busy(true);
            ui.set_settings_status("Connecting…".into());

            let ui_weak = ui.as_weak();
            let store = store.clone();
            let state = state.clone();
            handle.spawn(async move {
                let result = match SyncEngine::new(store.clone(), &url, &email, &password).await {
                    Ok(mut engine) => engine.sync().await,
                    Err(error) => Err(error),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_busy(false);
                        match result {
                            Ok(report) => {
                                ui.set_settings_open(false);
                                ui.set_settings_password("".into());
                                ui.set_settings_status("".into());
                                ui.set_settings_has_account(true);
                                ui.set_status(format!("{report}").into());
                                let index = ui.get_feed_index();
                                let _ = refresh_feeds(&ui, &store, &state);
                                let _ = load_stream(&ui, &store, &state, index);
                            }
                            Err(error) => {
                                if store.account().ok().flatten().is_some() {
                                    ui.set_settings_has_account(true);
                                }
                                ui.set_settings_status(
                                    format!("Connection failed: {error}").into(),
                                );
                            }
                        }
                    }
                });
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_select_feed(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                if let Err(error) = load_stream(&ui, &store, &state, index) {
                    ui.set_status(format!("Load failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_select_article(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = select_article(&ui, &state, index);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_toggle_read(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let index = ui.get_article_index();
            let Some(item) = selected_item(&state, index) else {
                return;
            };
            let read = !item.read;
            if let Err(error) = store.set_read(&[item.id.clone()], read) {
                ui.set_status(format!("Update failed: {error}").into());
                return;
            }
            ui.set_status(
                if read {
                    "Marked read (queued)"
                } else {
                    "Marked unread (queued)"
                }
                .into(),
            );
            let feed_index = ui.get_feed_index();
            let _ = load_stream(&ui, &store, &state, feed_index);
            ui.set_article_index(index);
            let _ = select_article(&ui, &state, index);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_toggle_star(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let index = ui.get_article_index();
            let Some(item) = selected_item(&state, index) else {
                return;
            };
            let starred = !item.starred;
            if let Err(error) = store.set_starred(&[item.id.clone()], starred) {
                ui.set_status(format!("Update failed: {error}").into());
                return;
            }
            ui.set_status(
                if starred {
                    "Starred (queued)"
                } else {
                    "Unstarred (queued)"
                }
                .into(),
            );
            let feed_index = ui.get_feed_index();
            let _ = load_stream(&ui, &store, &state, feed_index);
            ui.set_article_index(index);
            let _ = select_article(&ui, &state, index);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_mark_all_read(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let feed_index = ui.get_feed_index();
            let stream = current_stream(&state, feed_index);
            let ts_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64;
            if let Err(error) = store.mark_all_read(&stream, ts_ns) {
                ui.set_status(format!("Update failed: {error}").into());
                return;
            }
            let _ = refresh_feeds(&ui, &store, &state);
            let _ = load_stream(&ui, &store, &state, feed_index);
            ui.set_status("Queued mark-all-read".into());
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        let handle = runtime.handle().clone();
        ui.on_refresh(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let account = store.account().ok().flatten();
            let token = store.auth_token().ok().flatten();
            let (Some((base, _email)), Some(token)) = (account, token) else {
                ui.invoke_open_settings();
                ui.set_settings_status("Enter your FreshRSS details to sync.".into());
                return;
            };
            ui.set_syncing(true);
            ui.set_status("Syncing…".into());

            let ui_weak = ui.as_weak();
            let store = store.clone();
            let state = state.clone();
            handle.spawn(async move {
                let result = match SyncEngine::with_auth(store.clone(), &base, &token) {
                    Ok(mut engine) => engine.sync().await,
                    Err(error) => Err(error),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_syncing(false);
                        match result {
                            Ok(report) => {
                                ui.set_status(format!("{report}").into());
                                let index = ui.get_feed_index();
                                let _ = refresh_feeds(&ui, &store, &state);
                                let _ = load_stream(&ui, &store, &state, index);
                            }
                            Err(error) => {
                                if error.is_auth_error() {
                                    let _ = store.set_auth_token(None);
                                    ui.invoke_open_settings();
                                    ui.set_settings_status(
                                        "Session expired. Enter your password again.".into(),
                                    );
                                }
                                ui.set_status(format!("Sync failed: {error}").into());
                            }
                        }
                    }
                });
            });
        });
    }
}

fn refresh_feeds(ui: &MainWindow, store: &Store, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let feeds = store.feeds()?;
    let total_unread = store.total_unread()?;
    let mut rows = vec![
        FeedRow {
            id: "all".into(),
            title: "All items".into(),
            folder: String::new().into(),
            unread: total_unread as i32,
        },
        FeedRow {
            id: "starred".into(),
            title: "Starred".into(),
            folder: String::new().into(),
            unread: 0,
        },
    ];
    let mut streams = vec![Stream::All, Stream::Starred];

    for feed in feeds {
        if feed.id == "feed/unknown" {
            continue;
        }
        rows.push(FeedRow {
            id: feed.id.clone().into(),
            title: feed.title.into(),
            folder: feed.folder_name.unwrap_or_default().into(),
            unread: feed.unread_count as i32,
        });
        streams.push(Stream::Feed(feed.id));
    }

    state.lock().unwrap().streams = streams;
    ui.set_feeds(ModelRc::new(VecModel::from(rows)));
    Ok(())
}

fn load_stream(
    ui: &MainWindow,
    store: &Store,
    state: &Arc<Mutex<UiState>>,
    index: i32,
) -> Result<()> {
    let stream = {
        let state = state.lock().unwrap();
        state.streams.get(index.max(0) as usize).cloned()
    };
    let Some(stream) = stream else {
        return Ok(());
    };

    let items = store.items(&stream, 300, 0, false, None)?;
    let rows: Vec<ArticleRow> = items
        .iter()
        .map(|item| ArticleRow {
            id: item.id.clone().into(),
            title: display_title(&item.title).into(),
            feed: item.feed_title.clone().into(),
            time: relative_time(item.published).into(),
            read: item.read,
            starred: item.starred,
        })
        .collect();

    state.lock().unwrap().items = items;
    ui.set_articles(ModelRc::new(VecModel::from(rows)));
    ui.set_article_index(-1);
    ui.set_reader_title("".into());
    ui.set_reader_meta("".into());
    ui.set_reader_body("".into());
    Ok(())
}

fn select_article(ui: &MainWindow, state: &Arc<Mutex<UiState>>, index: i32) -> Result<()> {
    let Some(item) = selected_item(state, index) else {
        return Ok(());
    };
    let html = if !item.content_html.trim().is_empty() {
        &item.content_html
    } else {
        &item.summary_html
    };
    let body = if html.trim().is_empty() {
        "No cached content. Press Refresh to fetch this article.".to_string()
    } else {
        html_to_text(html)
    };

    ui.set_reader_title(display_title(&item.title).into());
    let mut meta = item.feed_title;
    if let Some(author) = item.author.filter(|a| !a.trim().is_empty()) {
        meta.push_str(" · ");
        meta.push_str(&author);
    }
    ui.set_reader_meta(meta.into());
    ui.set_reader_body(body.into());
    Ok(())
}

fn selected_item(state: &Arc<Mutex<UiState>>, index: i32) -> Option<ItemRow> {
    if index < 0 {
        return None;
    }
    state.lock().unwrap().items.get(index as usize).cloned()
}

fn current_stream(state: &Arc<Mutex<UiState>>, index: i32) -> Stream {
    state
        .lock()
        .unwrap()
        .streams
        .get(index.max(0) as usize)
        .cloned()
        .unwrap_or(Stream::All)
}

fn display_title(title: &str) -> String {
    if title.trim().is_empty() {
        "(untitled)".into()
    } else {
        title.to_string()
    }
}

fn relative_time(secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let delta = (now - secs).max(0);
    if delta < 60 {
        "now".into()
    } else if delta < 3_600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3_600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

fn db_path() -> PathBuf {
    if let Ok(path) = std::env::var("NIEUWSER_DB") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", "nieuwser")
        .map(|dirs| dirs.data_dir().join("nieuwser.db"))
        .unwrap_or_else(|| PathBuf::from("nieuwser.db"))
}
