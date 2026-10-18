slint::include_modules!();

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use nieuwser_core::media::fetch_image;
use nieuwser_core::models::Stream;
use nieuwser_core::reader::{Block, BlockKind, html_to_blocks};
use nieuwser_core::store::ItemRow;
use nieuwser_core::{Client, Store, SyncEngine};
use slint::{ComponentHandle, Model, ModelRc, StyledText, VecModel};

struct UiState {
    streams: Vec<Stream>,
    items: Vec<ItemRow>,
    handle: tokio::runtime::Handle,
    pending_icons: HashSet<String>,
    pending_media: HashSet<String>,
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
    let state = Arc::new(Mutex::new(UiState {
        streams: Vec::new(),
        items: Vec::new(),
        handle: runtime.handle().clone(),
        pending_icons: HashSet::new(),
        pending_media: HashSet::new(),
    }));

    refresh_feeds(&ui, &store, &state)?;
    load_stream(&ui, &store, &state, 0)?;
    bind_callbacks(&ui, &store, &state, &runtime);

    if let Some((base, email)) = store.account()? {
        ui.set_settings_url(base.into());
        ui.set_settings_email(email.into());
        ui.set_settings_has_account(true);
    }
    ui.set_last_synced(last_synced_text(&store).into());
    ui.set_mark_read_on_open(store.state("mark_read_on_open")?.as_deref() != Some("0"));
    let has_token = store.auth_token()?.is_some();
    if !has_token {
        ui.set_settings_open(true);
        ui.set_status("Connect your FreshRSS account to sync.".into());
    } else {
        ui.set_status(format!("Ready. DB: {}", db_path.display()).into());
    }

    if has_token {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        let handle = runtime.handle().clone();
        slint::Timer::single_shot(Duration::from_secs(1), move || {
            if let Some(ui) = ui_weak.upgrade() {
                start_sync_if_configured(&ui, &store, &state, &handle);
            }
        });
    }

    let timer_ui = ui.as_weak();
    let timer_store = store.clone();
    let timer_state = state.clone();
    let timer_handle = runtime.handle().clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(15 * 60),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                if !ui.get_settings_open() {
                    start_sync_if_configured(&ui, &timer_store, &timer_state, &timer_handle);
                }
            }
        },
    );

    ui.run()?;
    drop(timer);
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
                                ui.set_last_synced(last_synced_text(&store).into());
                                let index = ui.get_feed_index();
                                let _ = refresh_feeds(&ui, &store, &state);
                                let _ = reload_preserving_selection(&ui, &store, &state, index);
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
        ui.on_set_mark_read_on_open(move |value| {
            if let Some(ui) = ui_weak.upgrade() {
                if let Err(error) =
                    store.set_state("mark_read_on_open", if value { "1" } else { "0" })
                {
                    ui.set_status(format!("Settings failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_next_article(move || {
            if let Some(ui) = ui_weak.upgrade() {
                move_article(&ui, &store, &state, 1);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_prev_article(move || {
            if let Some(ui) = ui_weak.upgrade() {
                move_article(&ui, &store, &state, -1);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_open_link(move |link| {
            if let Err(error) = open_external(link.as_str()) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status(format!("Could not open link: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_search(move |_query| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_article_content_y(0.0);
                if let Err(error) = load_stream(&ui, &store, &state, ui.get_feed_index()) {
                    ui.set_status(format!("Search failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_select_feed(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_article_content_y(0.0);
                if let Err(error) = load_stream(&ui, &store, &state, index) {
                    ui.set_status(format!("Load failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_select_article(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = select_article(&ui, &store, &state, index, true, true);
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
            let _ = select_article(&ui, &store, &state, index, false, false);
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
            let _ = select_article(&ui, &store, &state, index, false, false);
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
            spawn_sync(&ui, &store, &state, &handle, base, token);
        });
    }
}

fn start_sync_if_configured(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    handle: &tokio::runtime::Handle,
) {
    let account = store.account().ok().flatten();
    let token = store.auth_token().ok().flatten();
    if let (Some((base, _email)), Some(token)) = (account, token) {
        spawn_sync(ui, store, state, handle, base, token);
    }
}

fn spawn_sync(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    handle: &tokio::runtime::Handle,
    base: String,
    token: String,
) {
    if ui.get_syncing() {
        return;
    }
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
                        ui.set_last_synced(last_synced_text(&store).into());
                        let index = ui.get_feed_index();
                        let _ = refresh_feeds(&ui, &store, &state);
                        let _ = reload_preserving_selection(&ui, &store, &state, index);
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
}

fn last_synced_text(store: &Store) -> String {
    match store
        .state("last_sync_at")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
    {
        Some(ts) => format!("Last sync {}", relative_time(ts)),
        None => "Never synced".into(),
    }
}

fn refresh_feeds(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let feeds = store.feeds()?;
    let total_unread = store.total_unread()?;
    let mut rows = vec![
        FeedRow {
            id: "all".into(),
            title: "All items".into(),
            folder: String::new().into(),
            unread: total_unread as i32,
            icon: slint::Image::default(),
        },
        FeedRow {
            id: "starred".into(),
            title: "Starred".into(),
            folder: String::new().into(),
            unread: 0,
            icon: slint::Image::default(),
        },
    ];
    let mut streams = vec![Stream::All, Stream::Starred];

    for feed in feeds {
        if feed.id == "feed/unknown" {
            continue;
        }
        let icon_url = feed.icon_url.clone();
        let icon = icon_url.as_deref().and_then(|url| cached_image(store, url));
        if icon.is_none() {
            if let Some(url) = icon_url.as_deref() {
                spawn_icon_fetch(ui, store, state, url);
            }
        }
        rows.push(FeedRow {
            id: feed.id.clone().into(),
            title: feed.title.into(),
            folder: feed.folder_name.unwrap_or_default().into(),
            unread: feed.unread_count as i32,
            icon: icon.unwrap_or_default(),
        });
        streams.push(Stream::Feed(feed.id));
    }

    state.lock().unwrap().streams = streams;
    ui.set_feeds(ModelRc::new(VecModel::from(rows)));
    Ok(())
}

fn cached_image(store: &Store, url: &str) -> Option<slint::Image> {
    let data = store.cached_media(url).ok().flatten()?;
    slint::Image::load_from_data(&data, None).ok()
}

fn spawn_icon_fetch(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>, url: &str) {
    let url = url.to_string();
    if !state.lock().unwrap().pending_icons.insert(url.clone()) {
        return;
    }
    let ui_weak = ui.as_weak();
    let store = store.clone();
    let state = state.clone();
    let handle = state.lock().unwrap().handle.clone();
    handle.spawn(async move {
        let _ = fetch_image(&store, &url).await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = refresh_feeds(&ui, &store, &state);
            }
        });
    });
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

    let query = ui.get_search_query();
    let items = store.items(&stream, 300, 0, false, Some(query.as_str()))?;
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
    ui.set_reader_blocks(ModelRc::new(VecModel::from(Vec::<ReaderBlock>::new())));
    Ok(())
}

fn reload_preserving_selection(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    stream_index: i32,
) -> Result<()> {
    let selected = selected_item(state, ui.get_article_index()).map(|item| item.id);
    load_stream(ui, store, state, stream_index)?;
    if let Some(id) = selected {
        let found = state
            .lock()
            .unwrap()
            .items
            .iter()
            .position(|item| item.id == id);
        if let Some(found) = found {
            ui.set_article_index(found as i32);
            select_article(ui, store, state, found as i32, false, false)?;
        }
    }
    Ok(())
}

fn select_article(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    index: i32,
    mark_read: bool,
    allow_fetch: bool,
) -> Result<()> {
    let Some(item) = selected_item(state, index) else {
        return Ok(());
    };
    let id = item.id.clone();
    let html = if !item.content_html.trim().is_empty() {
        &item.content_html
    } else {
        &item.summary_html
    };
    let needs_fetch = html.trim().is_empty();

    ui.set_reader_title(display_title(&item.title).into());
    let mut meta = item.feed_title;
    if let Some(author) = item.author.filter(|a| !a.trim().is_empty()) {
        meta.push_str(" · ");
        meta.push_str(&author);
    }
    ui.set_reader_meta(meta.into());
    if needs_fetch {
        set_reader_message(
            ui,
            if allow_fetch {
                "No cached content. Loading…"
            } else {
                "No cached content."
            },
        );
    } else {
        let blocks = html_to_blocks(html, item.href.as_deref());
        let mut image_urls = Vec::new();
        let rows: Vec<ReaderBlock> = blocks
            .into_iter()
            .map(|block| {
                if let Some(url) = block.url.as_deref() {
                    if cached_image(store, url).is_none() {
                        image_urls.push(url.to_string());
                    }
                }
                to_reader_block(store, block)
            })
            .collect();
        if rows.is_empty() {
            set_reader_message(ui, "No readable content in this article.");
        } else {
            ui.set_reader_blocks(ModelRc::new(VecModel::from(rows)));
            spawn_reader_images(ui, store, state, image_urls);
        }
    }

    if mark_read && ui.get_mark_read_on_open() && !item.read {
        store.set_read(&[id.clone()], true)?;
        refresh_feeds(ui, store, state)?;
        reload_preserving_selection(ui, store, state, ui.get_feed_index())?;
    }

    if allow_fetch && needs_fetch {
        let account = store.account().ok().flatten();
        let token = store.auth_token().ok().flatten();
        if let (Some((base, _)), Some(token)) = (account, token) {
            let ui_weak = ui.as_weak();
            let store = store.clone();
            let state = state.clone();
            let handle = state.lock().unwrap().handle.clone();
            handle.spawn(async move {
                let result = match Client::with_auth(&base, token) {
                    Ok(client) => client.item_contents(&[id.clone()]).await,
                    Err(error) => Err(error),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    let still_selected = selected_item(&state, ui.get_article_index())
                        .is_some_and(|item| item.id == id);
                    if !still_selected {
                        return;
                    }
                    match result {
                        Ok(items) if !items.is_empty() => {
                            let _ = store.upsert_items(&items);
                            let loaded = store.item(&id).ok().flatten().is_some_and(|item| {
                                !item.content_html.trim().is_empty()
                                    || !item.summary_html.trim().is_empty()
                            });
                            if loaded {
                                let _ = select_article(&ui, &store, &state, index, false, false);
                            } else {
                                set_reader_message(&ui, "No content available from the server.");
                            }
                        }
                        Ok(_) => set_reader_message(&ui, "No content available from the server."),
                        Err(error) => {
                            set_reader_message(&ui, &format!("Could not load article: {error}"))
                        }
                    }
                });
            });
        } else {
            set_reader_message(
                ui,
                "No cached content. Connect an account to fetch this article.",
            );
        }
    }
    Ok(())
}

fn to_reader_block(store: &Store, block: Block) -> ReaderBlock {
    let kind = match block.kind {
        BlockKind::Heading(level) => format!("h{level}"),
        BlockKind::Paragraph => "p".into(),
        BlockKind::ListItem { .. } => "li".into(),
        BlockKind::Quote => "quote".into(),
        BlockKind::Code => "code".into(),
        BlockKind::Rule => "rule".into(),
        BlockKind::Image => "image".into(),
    };
    let text = if block.kind == BlockKind::Code {
        StyledText::from_plain_text(&block.text)
    } else {
        StyledText::from_markdown(&block.text)
            .unwrap_or_else(|_| StyledText::from_plain_text(&block.text))
    };
    let source = block.url.unwrap_or_default();
    let image = (!source.is_empty())
        .then(|| cached_image(store, &source))
        .flatten();
    ReaderBlock {
        kind: kind.into(),
        label: block.label.into(),
        text,
        indent: match block.kind {
            BlockKind::ListItem { depth } => depth as i32,
            _ => 0,
        },
        source: source.into(),
        has_image: image.is_some(),
        image: image.unwrap_or_default(),
    }
}

fn spawn_reader_images(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    urls: Vec<String>,
) {
    for url in urls {
        if !state.lock().unwrap().pending_media.insert(url.clone()) {
            continue;
        }
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        let handle = state.lock().unwrap().handle.clone();
        handle.spawn(async move {
            let result = fetch_image(&store, &url).await;
            let _ = slint::invoke_from_event_loop(move || {
                state.lock().unwrap().pending_media.remove(&url);
                let Some(ui) = ui_weak.upgrade() else { return };
                let Ok(data) = result else { return };
                let Ok(image) = slint::Image::load_from_data(&data, None) else {
                    return;
                };
                let model = ui.get_reader_blocks();
                for index in 0..model.row_count() {
                    let Some(mut row) = model.row_data(index) else {
                        continue;
                    };
                    if row.source.as_str() == url.as_str() {
                        row.image = image.clone();
                        row.has_image = true;
                        model.set_row_data(index, row);
                    }
                }
            });
        });
    }
}

fn set_reader_message(ui: &MainWindow, message: &str) {
    let block = ReaderBlock {
        kind: "p".into(),
        label: "".into(),
        text: StyledText::from_plain_text(message),
        indent: 0,
        source: "".into(),
        image: slint::Image::default(),
        has_image: false,
    };
    ui.set_reader_blocks(ModelRc::new(VecModel::from(vec![block])));
}

fn open_external(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command.spawn().map(|_| ())
}

fn move_article(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>, delta: i32) {
    let len = state.lock().unwrap().items.len() as i32;
    if len == 0 {
        return;
    }
    let current = ui.get_article_index();
    let next = if current < 0 {
        0
    } else {
        (current + delta).clamp(0, len - 1)
    };
    ui.set_article_index(next);
    ui.set_article_content_y(next as f32 * 64.0);
    let _ = select_article(ui, store, state, next, true, true);
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
