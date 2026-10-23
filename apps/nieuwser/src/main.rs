slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use nieuwser_core::media::fetch_image;
use nieuwser_core::models::Stream;
use nieuwser_core::reader::{Block, BlockKind, html_to_blocks, html_to_text};
use nieuwser_core::store::ItemSummary;
use nieuwser_core::{Client, Store, SyncEngine};
use slint::{ComponentHandle, Model, ModelRc, StyledText, VecModel};

#[derive(Clone)]
struct ReaderView {
    title: String,
    meta: String,
    blocks: ModelRc<ReaderBlock>,
}

struct UiState {
    streams: Vec<Stream>,
    items: Vec<ItemSummary>,
    selected: HashSet<String>,
    anchor: Option<String>,
    handle: tokio::runtime::Handle,
    pending_icons: HashSet<String>,
    pending_media: HashSet<String>,
}

const ARTICLE_PAGE: i64 = 300;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const ICON_IMAGE_SIZE: u32 = 64;
const READER_IMAGE_SIZE: u32 = 1024;
const IMAGE_CACHE_LIMIT: usize = 48;
const READER_CACHE_LIMIT: usize = 24;
const PREVIEW_HTML_BYTES: usize = 16 * 1024;
const MIN_WINDOW_WIDTH: f32 = 560.0;
const MIN_WINDOW_HEIGHT: f32 = 480.0;
const SHORTCUTS: [(&str, &str, &str); 5] = [
    ("next", "shortcut_next", "j"),
    ("prev", "shortcut_prev", "k"),
    ("read", "shortcut_read", "r"),
    ("star", "shortcut_star", "f"),
    ("open", "shortcut_open", "o"),
];

static IMAGE_FETCH_SLOTS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(4));

thread_local! {
    // HashMap::new is not const on the supported Rust version.
    #[allow(clippy::missing_const_for_thread_local)]
    static IMAGE_CACHE: RefCell<HashMap<(String, u32), slint::Image>> = RefCell::new(HashMap::new());
    static READER_CACHE: RefCell<VecDeque<(String, ReaderView)>> =
        const { RefCell::new(VecDeque::new()) };
}

fn main() -> Result<()> {
    configure_emoji_font()?;
    let db_path = db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Arc::new(Store::open(&db_path)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = MainWindow::new()?;
    apply_window_geometry(&store, &ui);
    apply_shortcuts(&ui, &store);
    ui.set_article_filter(saved_article_filter(&store).into());
    ui.set_feed_width(saved_length(&store, "feed_width", 260.0).max(160.0));
    ui.set_list_width(saved_length(&store, "list_width", 400.0).max(240.0));
    let state = Arc::new(Mutex::new(UiState {
        streams: Vec::new(),
        items: Vec::new(),
        selected: HashSet::new(),
        anchor: None,
        handle: runtime.handle().clone(),
        pending_icons: HashSet::new(),
        pending_media: HashSet::new(),
    }));

    refresh_feeds(&ui, &store, &state)?;
    load_stream(&ui, &store, &state, 0)?;
    bind_callbacks(&ui, &store, &state, &runtime);

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.window().on_close_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                save_window_geometry(&store, ui.window());
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    if let Some((base, email)) = store.account()? {
        ui.set_settings_url(base.into());
        ui.set_settings_email(email.into());
        ui.set_settings_has_account(true);
    }
    ui.set_last_synced(last_synced_text(&store).into());
    ui.set_mark_read_on_open(store.state("mark_read_on_open")?.as_deref() != Some("0"));
    ui.set_mark_read_on_open_browser(
        store.state("mark_read_on_open_browser")?.as_deref() != Some("0"),
    );
    let dark_mode = store.state("dark_mode")?.as_deref() == Some("1");
    ui.set_dark_mode(dark_mode);
    ui.global::<Theme>().set_dark(dark_mode);
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
            if let Some(ui) = timer_ui.upgrade()
                && !ui.get_settings_open()
            {
                start_sync_if_configured(&ui, &timer_store, &timer_state, &timer_handle);
            }
        },
    );

    ui.run()?;
    save_pane_widths(&store, ui.get_feed_width(), ui.get_list_width());
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
        let store = store.clone();
        ui.on_save_pane_widths(move |feed_width, list_width| {
            save_pane_widths(&store, feed_width, list_width);
        });
    }

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
            if let Some(ui) = ui_weak.upgrade()
                && ui.get_settings_has_account()
            {
                ui.set_settings_open(false);
                ui.set_settings_password("".into());
                ui.set_settings_status("".into());
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
            if let Some(ui) = ui_weak.upgrade()
                && let Err(error) =
                    store.set_state("mark_read_on_open", if value { "1" } else { "0" })
            {
                ui.set_status(format!("Settings failed: {error}").into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_set_mark_read_on_open_browser(move |value| {
            if let Some(ui) = ui_weak.upgrade()
                && let Err(error) =
                    store.set_state("mark_read_on_open_browser", if value { "1" } else { "0" })
            {
                ui.set_status(format!("Settings failed: {error}").into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_set_dark_mode(move |value| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_dark_mode(value);
                ui.global::<Theme>().set_dark(value);
                if let Err(error) = store.set_state("dark_mode", if value { "1" } else { "0" }) {
                    ui.set_status(format!("Settings failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_set_shortcut(move |action, value| {
            if let Some(ui) = ui_weak.upgrade() {
                set_shortcut(&store, &ui, action.as_str(), value.as_str());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_reset_shortcuts(move || {
            if let Some(ui) = ui_weak.upgrade() {
                for (_, key, default) in SHORTCUTS {
                    let _ = store.set_state(key, default);
                }
                apply_shortcuts(&ui, &store);
                ui.set_shortcuts_status("".into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_set_article_filter(move |filter| {
            if let Some(ui) = ui_weak.upgrade() {
                let filter = normalize_article_filter(filter.as_str());
                ui.set_article_filter(filter.into());
                let _ = store.set_state("article_filter", filter);
                if let Err(error) = load_stream(&ui, &store, &state, ui.get_feed_index()) {
                    ui.set_status(format!("Load failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_return_to_list(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_article_filter() == "all" {
                    ui.set_compact_page("list".into());
                } else if let Err(error) = load_stream(&ui, &store, &state, ui.get_feed_index()) {
                    ui.set_status(format!("Load failed: {error}").into());
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
            if let Err(error) = open_external(link.as_str())
                && let Some(ui) = ui_weak.upgrade()
            {
                ui.set_status(format!("Could not open link: {error}").into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        let generation = Rc::new(Cell::new(0u64));
        ui.on_search(move |_query| {
            let current = generation.get() + 1;
            generation.set(current);
            let generation = generation.clone();
            let ui_weak = ui_weak.clone();
            let store = store.clone();
            let state = state.clone();
            slint::Timer::single_shot(SEARCH_DEBOUNCE, move || {
                if generation.get() != current {
                    return;
                }
                let Some(ui) = ui_weak.upgrade() else { return };
                ui.set_article_content_y(0.0);
                if let Err(error) = load_stream(&ui, &store, &state, ui.get_feed_index()) {
                    ui.set_status(format!("Search failed: {error}").into());
                }
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_select_feed(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_article_content_y(0.0);
                ui.set_compact_page("list".into());
                ui.set_feed_drawer_open(false);
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
        ui.on_select_article(move |index, ctrl, shift| {
            if let Some(ui) = ui_weak.upgrade() {
                if !apply_selection(&state, index, ctrl, shift) {
                    return;
                }
                ui.set_article_index(index);
                ui.set_compact_page("reader".into());
                sync_article_rows(&ui, &state);
                let mark_read = !ctrl && !shift;
                let _ = select_article(&ui, &store, &state, index, mark_read, mark_read);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_context_article(move |index| {
            if let Some(ui) = ui_weak.upgrade() {
                if !context_selection(&state, index) {
                    return;
                }
                ui.set_article_index(index);
                sync_article_rows(&ui, &state);
                let _ = select_article(&ui, &store, &state, index, false, false);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_set_selection_read(move |read| {
            if let Some(ui) = ui_weak.upgrade() {
                mark_selected_read(&ui, &store, &state, read);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_set_selection_starred(move |starred| {
            if let Some(ui) = ui_weak.upgrade() {
                mark_selected_starred(&ui, &store, &state, starred);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_open_selection(move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_selected(&ui, &store, &state, ui.get_mark_read_on_open_browser());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_copy_selection_links(move || {
            if let Some(ui) = ui_weak.upgrade() {
                copy_selected_links(&ui, &state);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_load_more(move || {
            if let Some(ui) = ui_weak.upgrade()
                && let Err(error) = load_more(&ui, &store, &state)
            {
                ui.set_status(format!("Could not load more articles: {error}").into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_toggle_read(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(item) = selected_item(&state, ui.get_article_index()) else {
                return;
            };
            mark_selected_read(&ui, &store, &state, !item.read);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_toggle_star(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(item) = selected_item(&state, ui.get_article_index()) else {
                return;
            };
            mark_selected_starred(&ui, &store, &state, !item.starred);
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
            // FreshRSS compares mark-all-read timestamps with microsecond entry IDs.
            let ts_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64;
            if let Err(error) = store.mark_all_read(&stream, ts_us) {
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

fn saved_length(store: &Store, key: &str, default: f32) -> f32 {
    store
        .state(key)
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
        .filter(|value: &f32| value.is_finite())
        .unwrap_or(default)
}

fn saved_i32(store: &Store, key: &str) -> Option<i32> {
    store.state(key).ok().flatten()?.parse().ok()
}

// Wayland does not expose window positions; saving the fallback (0, 0) would move the next
// X11 launch to the top-left corner.
fn wayland_position_unavailable() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn apply_window_geometry(store: &Store, ui: &MainWindow) {
    let width = saved_length(store, "window_width", 0.0);
    let height = saved_length(store, "window_height", 0.0);
    if width >= MIN_WINDOW_WIDTH && height >= MIN_WINDOW_HEIGHT {
        ui.window().set_size(slint::PhysicalSize::new(
            width.round() as u32,
            height.round() as u32,
        ));
    }

    if !wayland_position_unavailable()
        && let (Some(x), Some(y)) = (saved_i32(store, "window_x"), saved_i32(store, "window_y"))
    {
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
    }
}

fn save_window_geometry(store: &Store, window: &slint::Window) {
    let size = window.size();
    let _ = store.set_state("window_width", &size.width.to_string());
    let _ = store.set_state("window_height", &size.height.to_string());
    if !wayland_position_unavailable() {
        let position = window.position();
        let _ = store.set_state("window_x", &position.x.to_string());
        let _ = store.set_state("window_y", &position.y.to_string());
    }
}

fn normalize_shortcut(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let key = chars.next()?;
    if chars.next().is_some() || !key.is_ascii() || key.is_whitespace() || key == '?' {
        return None;
    }
    Some(key.to_ascii_lowercase().to_string())
}

fn normalize_article_filter(value: &str) -> &'static str {
    match value {
        "unread" => "unread",
        "read" => "read",
        _ => "all",
    }
}

fn saved_article_filter(store: &Store) -> &'static str {
    let value = store.state("article_filter").ok().flatten();
    normalize_article_filter(value.as_deref().unwrap_or("all"))
}

fn article_read_filter(value: &str) -> Option<bool> {
    match value {
        "unread" => Some(false),
        "read" => Some(true),
        _ => None,
    }
}

fn shortcut_value(store: &Store, key: &str, default: &str) -> String {
    store
        .state(key)
        .ok()
        .flatten()
        .and_then(|value| normalize_shortcut(&value))
        .unwrap_or_else(|| default.into())
}

fn apply_shortcuts(ui: &MainWindow, store: &Store) {
    for (action, key, default) in SHORTCUTS {
        let value = shortcut_value(store, key, default);
        match action {
            "next" => ui.set_shortcut_next(value.into()),
            "prev" => ui.set_shortcut_prev(value.into()),
            "read" => ui.set_shortcut_read(value.into()),
            "star" => ui.set_shortcut_star(value.into()),
            "open" => ui.set_shortcut_open(value.into()),
            _ => {}
        }
    }
}

fn set_shortcut(store: &Store, ui: &MainWindow, action: &str, raw_value: &str) {
    let Some(index) = SHORTCUTS.iter().position(|(name, _, _)| *name == action) else {
        return;
    };
    let Some(value) = normalize_shortcut(raw_value) else {
        ui.set_shortcuts_status("Use a single printable key (not ?).".into());
        apply_shortcuts(ui, store);
        return;
    };

    let values: Vec<String> = SHORTCUTS
        .iter()
        .map(|(_, key, default)| shortcut_value(store, key, default))
        .collect();
    if values
        .iter()
        .enumerate()
        .any(|(other, existing)| other != index && existing == &value)
    {
        ui.set_shortcuts_status("That key is already in use.".into());
        apply_shortcuts(ui, store);
        return;
    }

    match store.set_state(SHORTCUTS[index].1, &value) {
        Ok(()) => {
            apply_shortcuts(ui, store);
            ui.set_shortcuts_status("".into());
        }
        Err(error) => {
            ui.set_shortcuts_status(format!("Could not save shortcut: {error}").into());
            apply_shortcuts(ui, store);
        }
    }
}

fn save_pane_widths(store: &Store, feed_width: f32, list_width: f32) {
    let _ = store.set_state("feed_width", &format!("{feed_width:.0}"));
    let _ = store.set_state("list_width", &format!("{list_width:.0}"));
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
        let icon = icon_url
            .as_deref()
            .and_then(|url| cached_image(url, ICON_IMAGE_SIZE));
        if icon.is_none()
            && let Some(url) = icon_url.as_deref()
        {
            spawn_icon_fetch(ui, store, state, &feed.id, url);
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

fn cached_image(url: &str, max_size: u32) -> Option<slint::Image> {
    IMAGE_CACHE.with(|cache| cache.borrow().get(&(url.to_string(), max_size)).cloned())
}

fn remember_image(url: &str, max_size: u32, image: slint::Image) -> slint::Image {
    IMAGE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= IMAGE_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert((url.to_string(), max_size), image.clone());
        image
    })
}

fn decode_image(data: &[u8], max_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(nieuwser_core::media::MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(nieuwser_core::media::MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(nieuwser_core::media::MAX_IMAGE_PIXELS * 4);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let image = if image.width() > max_size || image.height() > max_size {
        image.resize(max_size, max_size, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some((rgba.into_raw(), width, height))
}

fn image_from_rgba(data: Vec<u8>, width: u32, height: u32) -> slint::Image {
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&data, width, height);
    slint::Image::from_rgba8(buffer)
}

async fn decoded_image(
    store: Arc<Store>,
    url: String,
    max_size: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    let data = fetch_image(&store, &url).await.ok()?;
    tokio::task::spawn_blocking(move || decode_image(&data, max_size))
        .await
        .ok()
        .flatten()
}

fn spawn_icon_fetch(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    feed_id: &str,
    url: &str,
) {
    let url = url.to_string();
    if !state.lock().unwrap().pending_icons.insert(url.clone()) {
        return;
    }
    let ui_weak = ui.as_weak();
    let store = store.clone();
    let state = state.clone();
    let feed_id = feed_id.to_string();
    let handle = state.lock().unwrap().handle.clone();
    handle.spawn(async move {
        let decoded = decoded_image(store, url.clone(), ICON_IMAGE_SIZE).await;
        let _ = slint::invoke_from_event_loop(move || {
            state.lock().unwrap().pending_icons.remove(&url);
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some((data, width, height)) = decoded else {
                return;
            };
            let image = remember_image(&url, ICON_IMAGE_SIZE, image_from_rgba(data, width, height));
            let model = ui.get_feeds();
            for index in 0..model.row_count() {
                let Some(mut row) = model.row_data(index) else {
                    continue;
                };
                if row.id.as_str() == feed_id {
                    row.icon = image;
                    model.set_row_data(index, row);
                    break;
                }
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
    let read = article_read_filter(ui.get_article_filter().as_str());
    let items = store.items_filtered(&stream, ARTICLE_PAGE, 0, read, Some(query.as_str()))?;
    let has_more = items.len() as i64 == ARTICLE_PAGE;

    {
        let mut state = state.lock().unwrap();
        state.items = items;
        state.selected.clear();
        state.anchor = None;
    }
    push_articles(ui, state);
    ui.set_has_more_articles(has_more);
    ui.set_article_index(-1);
    update_adjacent_previews(ui, store, state, -1);
    ui.set_compact_page("list".into());
    ui.set_reader_title("".into());
    ui.set_reader_meta("".into());
    let rows = make_reader_rows("", "", Vec::new());
    ui.set_reader_blocks(ModelRc::new(VecModel::from(rows)));
    ui.set_reader_loading(false);
    Ok(())
}

fn article_row(item: &ItemSummary, selected: bool) -> ArticleRow {
    ArticleRow {
        id: item.id.clone().into(),
        title: display_title(&item.title).into(),
        feed: item.feed_title.clone().into(),
        time: relative_time(item.published).into(),
        read: item.read,
        starred: item.starred,
        selected,
    }
}

fn article_rows(items: &[ItemSummary], selected: &HashSet<String>) -> Vec<ArticleRow> {
    items
        .iter()
        .map(|item| article_row(item, selected.contains(&item.id)))
        .collect()
}

fn push_articles(ui: &MainWindow, state: &Arc<Mutex<UiState>>) {
    let rows = {
        let state = state.lock().unwrap();
        article_rows(&state.items, &state.selected)
    };
    reconcile_articles(ui, rows);
}

fn sync_article_rows(ui: &MainWindow, state: &Arc<Mutex<UiState>>) {
    let rows = {
        let state = state.lock().unwrap();
        article_rows(&state.items, &state.selected)
    };
    reconcile_articles(ui, rows);
}

fn reconcile_articles(ui: &MainWindow, rows: Vec<ArticleRow>) {
    let model = ui.get_articles();
    if let Some(model) = model.as_any().downcast_ref::<VecModel<ArticleRow>>() {
        reconcile_article_model(model, rows);
    } else {
        ui.set_articles(ModelRc::new(VecModel::from(rows)));
    }
}

fn reconcile_article_model(model: &VecModel<ArticleRow>, rows: Vec<ArticleRow>) {
    let old_len = model.row_count();
    let overlap = old_len.min(rows.len());
    let prefix = (0..overlap)
        .take_while(|&index| {
            model
                .row_data(index)
                .is_some_and(|old| old.id == rows[index].id)
        })
        .count();
    let suffix = (0..overlap - prefix)
        .take_while(|&offset| {
            let old_index = old_len - 1 - offset;
            let new_index = rows.len() - 1 - offset;
            model
                .row_data(old_index)
                .is_some_and(|old| old.id == rows[new_index].id)
        })
        .count();

    for index in (prefix..old_len - suffix).rev() {
        model.remove(index);
    }
    for (offset, row) in rows[prefix..rows.len() - suffix]
        .iter()
        .cloned()
        .enumerate()
    {
        model.insert(prefix + offset, row);
    }
    for (index, row) in rows.iter().enumerate() {
        if index >= prefix && index < rows.len() - suffix {
            continue;
        }
        if model
            .row_data(index)
            .is_none_or(|old| !same_article_row(&old, row))
        {
            model.set_row_data(index, row.clone());
        }
    }
}

fn same_article_row(a: &ArticleRow, b: &ArticleRow) -> bool {
    a.id == b.id
        && a.title == b.title
        && a.feed == b.feed
        && a.time == b.time
        && a.read == b.read
        && a.starred == b.starred
        && a.selected == b.selected
}

// Keep feed and "All items" unread badges in sync without re-reading feeds.
fn adjust_unread(ui: &MainWindow, feed_id: &str, delta: i32) {
    let model = ui.get_feeds();
    for index in 0..model.row_count() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        if row.id.as_str() == "all" || row.id.as_str() == feed_id {
            row.unread = (row.unread + delta).max(0);
            model.set_row_data(index, row);
        }
    }
}

fn load_more(ui: &MainWindow, store: &Store, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let (stream, offset) = {
        let state = state.lock().unwrap();
        let index = ui.get_feed_index().max(0) as usize;
        (state.streams.get(index).cloned(), state.items.len())
    };
    let Some(stream) = stream else {
        return Ok(());
    };
    let query = ui.get_search_query();
    let read = article_read_filter(ui.get_article_filter().as_str());
    let items = store.items_filtered(
        &stream,
        ARTICLE_PAGE,
        offset as i64,
        read,
        Some(query.as_str()),
    )?;
    if items.is_empty() {
        ui.set_has_more_articles(false);
        return Ok(());
    }
    ui.set_has_more_articles(items.len() as i64 == ARTICLE_PAGE);
    let mut state = state.lock().unwrap();
    let start = state.items.len();
    state.items.extend(items);
    let model = ui.get_articles();
    for item in &state.items[start..] {
        let _ = model.push_row(article_row(item, state.selected.contains(&item.id)));
    }
    Ok(())
}

fn reload_preserving_selection(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    stream_index: i32,
) -> Result<()> {
    let (selected, anchor, lead) = {
        let state = state.lock().unwrap();
        let lead = state
            .items
            .get(ui.get_article_index().max(0) as usize)
            .filter(|_| ui.get_article_index() >= 0)
            .map(|item| item.id.clone());
        (state.selected.clone(), state.anchor.clone(), lead)
    };
    clear_reader_cache();
    load_stream(ui, store, state, stream_index)?;
    {
        let mut state = state.lock().unwrap();
        let items = &state.items;
        state.selected = selected
            .into_iter()
            .filter(|id| items.iter().any(|item| &item.id == id))
            .collect();
        state.anchor = anchor.filter(|id| state.selected.contains(id));
    }
    sync_article_rows(ui, state);
    if let Some(id) = lead {
        let found = state
            .lock()
            .unwrap()
            .items
            .iter()
            .position(|item| item.id == id);
        if let Some(found) = found {
            ui.set_article_index(found as i32);
            ui.invoke_reveal_article(found as i32);
            select_article(ui, store, state, found as i32, false, false)?;
        }
    }
    Ok(())
}

fn update_adjacent_previews(
    ui: &MainWindow,
    store: &Store,
    state: &Arc<Mutex<UiState>>,
    index: i32,
) {
    if index < 0 {
        set_adjacent_preview(ui, store, true, None);
        set_adjacent_preview(ui, store, false, None);
        return;
    }
    let (previous, next) = {
        let state = state.lock().unwrap();
        let index = index as usize;
        (
            index
                .checked_sub(1)
                .and_then(|i| state.items.get(i))
                .cloned(),
            state.items.get(index + 1).cloned(),
        )
    };
    set_adjacent_preview(ui, store, true, previous.as_ref());
    set_adjacent_preview(ui, store, false, next.as_ref());
}

fn set_adjacent_preview(
    ui: &MainWindow,
    store: &Store,
    previous: bool,
    item: Option<&ItemSummary>,
) {
    let (title, meta, excerpt) = match item {
        Some(item) => (
            display_title(&item.title),
            summary_meta(item),
            preview_excerpt(store, &item.id),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    if previous {
        ui.set_reader_prev_title(title.into());
        ui.set_reader_prev_meta(meta.into());
        ui.set_reader_prev_excerpt(excerpt.into());
    } else {
        ui.set_reader_next_title(title.into());
        ui.set_reader_next_meta(meta.into());
        ui.set_reader_next_excerpt(excerpt.into());
    }
}

fn summary_meta(item: &ItemSummary) -> String {
    let mut meta = item.feed_title.clone();
    if let Some(author) = item
        .author
        .as_ref()
        .filter(|author| !author.trim().is_empty())
    {
        meta.push_str(" · ");
        meta.push_str(author);
    }
    meta
}

fn preview_excerpt(store: &Store, id: &str) -> String {
    let Ok(Some(item)) = store.item(id) else {
        return String::new();
    };
    let html = if !item.content_html.trim().is_empty() {
        &item.content_html
    } else {
        &item.summary_html
    };
    preview_text(html)
}

// A preview shows at most 80 words, so parsing a whole long article wastes
// milliseconds on every adjacent-article update; 16 KB is far more than 80 words.
fn preview_text(html: &str) -> String {
    let mut end = html.len().min(PREVIEW_HTML_BYTES);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    html_to_text(&html[..end])
        .split_whitespace()
        .take(80)
        .collect::<Vec<_>>()
        .join(" ")
}

fn select_article(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    index: i32,
    mark_read: bool,
    allow_fetch: bool,
) -> Result<()> {
    let Some(summary) = selected_item(state, index) else {
        update_adjacent_previews(ui, store, state, -1);
        return Ok(());
    };
    update_adjacent_previews(ui, store, state, index);
    let id = summary.id.clone();
    ui.invoke_scroll_reader_to_top();
    ui.set_reader_loading(false);

    let mut meta = summary.feed_title.clone();
    if let Some(author) = summary.author.as_ref().filter(|a| !a.trim().is_empty()) {
        meta.push_str(" · ");
        meta.push_str(author);
    }
    ui.set_reader_title(display_title(&summary.title).into());
    ui.set_reader_meta(meta.into());

    if let Some(view) = cached_reader(&id) {
        hydrate_reader_images(ui, store, state, &view.blocks);
        ui.set_reader_title(view.title.into());
        ui.set_reader_meta(view.meta.into());
        ui.set_reader_blocks(view.blocks);
        if mark_read && ui.get_mark_read_on_open() && !summary.read {
            mark_item_read(ui, store, state, &id, &summary.feed_id)?;
        }
        return Ok(());
    }

    let Some(item) = store.item(&id)? else {
        return Ok(());
    };
    let html = if !item.content_html.trim().is_empty() {
        &item.content_html
    } else {
        &item.summary_html
    };
    let needs_fetch = html.trim().is_empty();

    let mut meta = item.feed_title.clone();
    if let Some(author) = item.author.as_ref().filter(|a| !a.trim().is_empty()) {
        meta.push_str(" · ");
        meta.push_str(author);
    }
    let title = display_title(&item.title).to_string();
    ui.set_reader_title(title.clone().into());
    ui.set_reader_meta(meta.clone().into());
    if needs_fetch {
        if allow_fetch {
            ui.set_reader_loading(true);
            let rows = make_reader_rows(&title, &meta, Vec::new());
            ui.set_reader_blocks(ModelRc::new(VecModel::from(rows)));
        } else {
            set_reader_message(ui, "No cached content.");
        }
    } else {
        ui.set_reader_loading(true);
        let blocks = html_to_blocks(html, item.href.as_deref());
        let mut image_urls = Vec::new();
        let rows: Vec<ReaderBlock> = blocks
            .into_iter()
            .map(|block| {
                let image = block
                    .url
                    .as_deref()
                    .and_then(|url| cached_image(url, READER_IMAGE_SIZE));
                if block.url.is_some() && image.is_none() {
                    image_urls.push(block.url.clone().unwrap_or_default());
                }
                to_reader_block(block, image)
            })
            .collect();
        if rows.is_empty() {
            set_reader_message(ui, "No readable content in this article.");
        } else {
            let rows = make_reader_rows(&title, &meta, rows);
            let blocks = ModelRc::new(VecModel::from(rows));
            ui.set_reader_blocks(blocks.clone());
            ui.set_reader_loading(false);
            spawn_reader_images(ui, store, state, image_urls);
            remember_reader(
                id.clone(),
                ReaderView {
                    title,
                    meta,
                    blocks,
                },
            );
        }
    }

    if mark_read && ui.get_mark_read_on_open() && !item.read {
        mark_item_read(ui, store, state, &id, &item.feed_id)?;
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
                    Ok(client) => client.item_contents(std::slice::from_ref(&id)).await,
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

// Keep each text item small: Slint starves input on very large blocks.
fn text_chunks(text: &str) -> Vec<String> {
    const MAX_CHARS: usize = 4000;
    const MAX_LINES: usize = 200;
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut lines = 0;
    for line in text.lines() {
        let mut rest = line;
        loop {
            let cut = rest
                .char_indices()
                .nth(MAX_CHARS)
                .map_or(rest.len(), |(i, _)| {
                    rest[..i]
                        .rfind(char::is_whitespace)
                        .filter(|&i| i > 0)
                        .unwrap_or(i)
                });
            current.push_str(&rest[..cut]);
            current.push('\n');
            lines += 1;
            if lines >= MAX_LINES || current.len() >= MAX_CHARS {
                chunks.push(std::mem::take(&mut current));
                lines = 0;
            }
            rest = rest[cut..].trim_start_matches(char::is_whitespace);
            if rest.is_empty() {
                break;
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn to_reader_block(block: Block, image: Option<slint::Image>) -> ReaderBlock {
    let kind = match block.kind {
        BlockKind::Heading(level) => format!("h{level}"),
        BlockKind::Paragraph => "p".into(),
        BlockKind::ListItem { .. } => "li".into(),
        BlockKind::Quote => "quote".into(),
        BlockKind::Code => "code".into(),
        BlockKind::Rule => "rule".into(),
        BlockKind::Image => "image".into(),
        BlockKind::Table => "table".into(),
    };
    let text = if block.kind == BlockKind::Image {
        StyledText::from_markdown(&block.text)
            .unwrap_or_else(|_| StyledText::from_plain_text(&block.text))
    } else {
        StyledText::default()
    };
    let chunks = if matches!(block.kind, BlockKind::Rule | BlockKind::Image) {
        Vec::new()
    } else {
        text_chunks(&block.text)
            .into_iter()
            .map(|chunk| {
                if block.kind == BlockKind::Code {
                    StyledText::from_plain_text(&chunk)
                } else {
                    StyledText::from_markdown(&chunk)
                        .unwrap_or_else(|_| StyledText::from_plain_text(&chunk))
                }
            })
            .collect()
    };
    let source = block.url.unwrap_or_default();
    let has_image = image.is_some();
    ReaderBlock {
        kind: kind.into(),
        label: block.label.into(),
        text,
        chunks: ModelRc::new(VecModel::from(chunks)),
        indent: match block.kind {
            BlockKind::ListItem { depth } => depth as i32,
            _ => 0,
        },
        source: source.into(),
        has_image,
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
            let permit = IMAGE_FETCH_SLOTS.acquire().await;
            let decoded = match permit {
                Ok(_permit) => decoded_image(store, url.clone(), READER_IMAGE_SIZE).await,
                Err(_) => None,
            };
            let _ = slint::invoke_from_event_loop(move || {
                state.lock().unwrap().pending_media.remove(&url);
                let Some(ui) = ui_weak.upgrade() else { return };
                let Some((data, width, height)) = decoded else {
                    return;
                };
                let image = remember_image(
                    &url,
                    READER_IMAGE_SIZE,
                    image_from_rgba(data, width, height),
                );
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

fn text_reader_block(kind: &str, text: &str) -> ReaderBlock {
    ReaderBlock {
        kind: kind.into(),
        label: text.into(),
        text: StyledText::default(),
        chunks: ModelRc::new(VecModel::from(Vec::<StyledText>::new())),
        indent: 0,
        source: "".into(),
        image: slint::Image::default(),
        has_image: false,
    }
}

fn make_reader_rows(title: &str, meta: &str, content: Vec<ReaderBlock>) -> Vec<ReaderBlock> {
    let mut rows = Vec::with_capacity(content.len() + 3);
    if title.is_empty() {
        if content.is_empty() {
            rows.push(text_reader_block("empty", "Select an article to read."));
        }
    } else {
        rows.push(text_reader_block("title", title));
        if !meta.is_empty() {
            rows.push(text_reader_block("meta", meta));
        }
        rows.push(text_reader_block("rule", ""));
    }
    rows.extend(content);
    rows
}

fn set_reader_message(ui: &MainWindow, message: &str) {
    let block = ReaderBlock {
        kind: "p".into(),
        label: "".into(),
        text: StyledText::from_plain_text(message),
        chunks: ModelRc::new(VecModel::from(Vec::<StyledText>::new())),
        indent: 0,
        source: "".into(),
        image: slint::Image::default(),
        has_image: false,
    };
    let rows = make_reader_rows(
        ui.get_reader_title().as_str(),
        ui.get_reader_meta().as_str(),
        vec![block],
    );
    ui.set_reader_blocks(ModelRc::new(VecModel::from(rows)));
    ui.set_reader_loading(false);
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
    ui.set_compact_page("reader".into());
    ui.invoke_reveal_article(next);
    apply_selection(state, next, false, false);
    sync_article_rows(ui, state);
    let _ = select_article(ui, store, state, next, true, true);
}

fn apply_selection(state: &Arc<Mutex<UiState>>, index: i32, ctrl: bool, shift: bool) -> bool {
    let mut state = state.lock().unwrap();
    let Some(item) = state.items.get(index.max(0) as usize) else {
        return false;
    };
    let id = item.id.clone();
    if shift {
        let anchor = state.anchor.clone();
        let anchor_index = anchor
            .and_then(|anchor| state.items.iter().position(|item| item.id == anchor))
            .unwrap_or(index.max(0) as usize);
        let (first, last) = (
            anchor_index.min(index.max(0) as usize),
            anchor_index.max(index.max(0) as usize),
        );
        if !ctrl {
            state.selected.clear();
        }
        let ids: Vec<String> = state.items[first..=last]
            .iter()
            .map(|item| item.id.clone())
            .collect();
        state.selected.extend(ids);
    } else if ctrl {
        if !state.selected.remove(&id) {
            state.selected.insert(id.clone());
        }
        state.anchor = Some(id);
    } else {
        state.selected.clear();
        state.selected.insert(id.clone());
        state.anchor = Some(id);
    }
    true
}

fn context_selection(state: &Arc<Mutex<UiState>>, index: i32) -> bool {
    let mut state = state.lock().unwrap();
    let Some(item) = state.items.get(index.max(0) as usize) else {
        return false;
    };
    let id = item.id.clone();
    if !state.selected.contains(&id) {
        state.selected.clear();
        state.selected.insert(id.clone());
        state.anchor = Some(id);
    }
    true
}

fn cached_reader(id: &str) -> Option<ReaderView> {
    READER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let index = cache.iter().position(|(key, _)| key == id)?;
        let entry = cache.remove(index)?;
        let view = entry.1.clone();
        cache.push_back(entry);
        Some(view)
    })
}

fn remember_reader(id: String, view: ReaderView) {
    READER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(index) = cache.iter().position(|(key, _)| *key == id) {
            cache.remove(index);
        }
        cache.push_back((id, view));
        while cache.len() > READER_CACHE_LIMIT {
            cache.pop_front();
        }
    });
}

fn clear_reader_cache() {
    READER_CACHE.with(|cache| cache.borrow_mut().clear());
}

// Images may have finished loading while another article was open. Apply anything
// already decoded and retry the rest for the cached model.
fn hydrate_reader_images(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    blocks: &ModelRc<ReaderBlock>,
) {
    let mut missing = Vec::new();
    for index in 0..blocks.row_count() {
        let Some(mut row) = blocks.row_data(index) else {
            continue;
        };
        if row.source.is_empty() || row.has_image {
            continue;
        }
        let url = row.source.to_string();
        if let Some(image) = cached_image(&url, READER_IMAGE_SIZE) {
            row.image = image;
            row.has_image = true;
            blocks.set_row_data(index, row);
        } else {
            missing.push(url);
        }
    }
    if !missing.is_empty() {
        spawn_reader_images(ui, store, state, missing);
    }
}

fn mark_item_read(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    id: &str,
    feed_id: &str,
) -> Result<()> {
    store.set_read(&[id.to_string()], true)?;
    {
        let mut state = state.lock().unwrap();
        if let Some(item) = state.items.iter_mut().find(|item| item.id == id) {
            item.read = true;
        }
    }
    sync_article_rows(ui, state);
    adjust_unread(ui, feed_id, -1);
    Ok(())
}

fn mark_selected_read(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    read: bool,
) {
    let ids = selected_ids(state);
    if ids.is_empty() {
        return;
    }
    if let Err(error) = set_items_read(ui, store, state, &ids, read) {
        ui.set_status(format!("Update failed: {error}").into());
        return;
    }
    ui.set_status(
        format!(
            "Marked {} article(s) {} (queued)",
            ids.len(),
            if read { "read" } else { "unread" }
        )
        .into(),
    );
}

fn set_items_read(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    ids: &[String],
    read: bool,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut deltas: HashMap<String, i32> = HashMap::new();
    {
        let state = state.lock().unwrap();
        for item in &state.items {
            if id_set.contains(item.id.as_str()) && item.read != read {
                *deltas.entry(item.feed_id.clone()).or_default() += if read { -1 } else { 1 };
            }
        }
    }
    store
        .set_read(ids, read)
        .map_err(|error| error.to_string())?;
    {
        let mut state = state.lock().unwrap();
        for item in &mut state.items {
            if id_set.contains(item.id.as_str()) {
                item.read = read;
            }
        }
    }
    sync_article_rows(ui, state);
    for (feed_id, delta) in deltas {
        adjust_unread(ui, &feed_id, delta);
    }
    Ok(())
}

fn mark_selected_starred(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    starred: bool,
) {
    let ids = selected_ids(state);
    if ids.is_empty() {
        return;
    }
    if let Err(error) = store.set_starred(&ids, starred) {
        ui.set_status(format!("Update failed: {error}").into());
        return;
    }
    {
        let mut state = state.lock().unwrap();
        for item in &mut state.items {
            if ids.contains(&item.id) {
                item.starred = starred;
            }
        }
    }
    sync_article_rows(ui, state);
    ui.set_status(
        format!(
            "{} {} article(s) (queued)",
            if starred { "Starred" } else { "Unstarred" },
            ids.len()
        )
        .into(),
    );
}

fn selected_ids(state: &Arc<Mutex<UiState>>) -> Vec<String> {
    state.lock().unwrap().selected.iter().cloned().collect()
}

fn selected_links(state: &Arc<Mutex<UiState>>) -> Vec<String> {
    let state = state.lock().unwrap();
    let mut seen = HashSet::new();
    state
        .items
        .iter()
        .filter(|item| state.selected.contains(&item.id))
        .filter_map(|item| item.href.as_deref())
        .filter(|href| !href.trim().is_empty())
        .filter(|href| seen.insert(href.to_string()))
        .map(ToString::to_string)
        .collect()
}

fn open_selected(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    mark_read: bool,
) {
    let links = selected_links(state);
    if links.is_empty() {
        ui.set_status("No link for the selected article(s).".into());
        return;
    }
    let mut opened_links = HashSet::new();
    for link in &links {
        if open_external(link).is_ok() {
            opened_links.insert(link.as_str());
        }
    }
    let opened = opened_links.len();
    if mark_read && opened > 0 {
        let ids: Vec<String> = {
            let state = state.lock().unwrap();
            state
                .items
                .iter()
                .filter(|item| state.selected.contains(&item.id))
                .filter(|item| {
                    item.href
                        .as_deref()
                        .is_some_and(|href| opened_links.contains(href))
                })
                .map(|item| item.id.clone())
                .collect()
        };
        if let Err(error) = set_items_read(ui, store, state, &ids, true) {
            ui.set_status(
                format!("Opened {opened} link(s), but marking read failed: {error}").into(),
            );
            return;
        }
    }
    ui.set_status(format!("Opened {opened} link(s).").into());
}

fn copy_selected_links(ui: &MainWindow, state: &Arc<Mutex<UiState>>) {
    let links = selected_links(state);
    if links.is_empty() {
        ui.set_status("No link for the selected article(s).".into());
        return;
    }
    let result = set_clipboard_text(links.join("\n"));
    match result {
        Ok(()) => ui.set_status(format!("Copied {} link(s).", links.len()).into()),
        Err(error) => ui.set_status(format!("Could not copy link: {error}").into()),
    }
}

// Keep the clipboard alive so X11 keeps serving after this call.
static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();

fn set_clipboard_text(text: String) -> Result<(), arboard::Error> {
    let slot = CLIPBOARD.get_or_init(|| Mutex::new(None));
    let mut slot = slot.lock().unwrap();
    if slot.is_none() {
        *slot = Some(arboard::Clipboard::new()?);
    }
    slot.as_mut().unwrap().set_text(text)
}

fn selected_item(state: &Arc<Mutex<UiState>>, index: i32) -> Option<ItemSummary> {
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

fn configure_emoji_font() -> Result<()> {
    use slint::fontique_011::fontique::{Blob, GenericFamily};

    const NOTO_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoEmoji-VariableFont_wght.ttf");

    let mut fonts = slint::fontique_011::shared_collection();
    let family = fonts
        .register_fonts(Blob::new(Arc::new(NOTO_EMOJI)), None)
        .first()
        .map(|(family, _)| *family)
        .ok_or_else(|| anyhow::anyhow!("bundled Noto Emoji font is invalid"))?;
    fonts.set_generic_families(GenericFamily::Emoji, std::iter::once(family));
    Ok(())
}

fn db_path() -> PathBuf {
    if let Ok(path) = std::env::var("NIEUWSER_DB") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("", "", "nieuwser")
        .map(|dirs| dirs.data_dir().join("nieuwser.db"))
        .unwrap_or_else(|| PathBuf::from("nieuwser.db"))
}

#[cfg(test)]
mod article_model_tests {
    use super::*;

    fn article(id: &str) -> ArticleRow {
        ArticleRow {
            id: id.into(),
            title: format!("Article {id}").into(),
            feed: "Feed".into(),
            time: "now".into(),
            read: false,
            starred: false,
            selected: false,
        }
    }

    #[test]
    fn reconciles_insertions_removals_and_updates_in_place() {
        let model = VecModel::from(vec![article("a"), article("b"), article("c")]);
        reconcile_article_model(&model, vec![article("a"), article("c"), article("d")]);
        assert_eq!(model.row_count(), 3);
        assert_eq!(model.row_data(0).unwrap().id, "a");
        assert_eq!(model.row_data(1).unwrap().id, "c");
        assert_eq!(model.row_data(2).unwrap().id, "d");

        let mut changed = article("a");
        changed.read = true;
        reconcile_article_model(&model, vec![changed.clone(), article("c"), article("d")]);
        assert!(model.row_data(0).unwrap().read);
        assert_eq!(model.row_data(0).unwrap().id, "a");
        assert_eq!(model.row_data(1).unwrap().id, "c");
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn preview_text_reads_only_the_start_of_long_articles() {
        let html = format!("<p>{}</p>", "é ".repeat(20_000));
        let text = preview_text(&html);
        assert_eq!(text.split_whitespace().count(), 80);
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    #[test]
    fn normalizes_editable_shortcuts() {
        assert_eq!(normalize_shortcut("J"), Some("j".into()));
        assert_eq!(normalize_shortcut(","), Some(",".into()));
        assert_eq!(normalize_shortcut("jk"), None);
        assert_eq!(normalize_shortcut("?"), None);
        assert_eq!(normalize_shortcut(" "), None);
        assert_eq!(normalize_shortcut("é"), None);
    }

    #[test]
    fn uses_freshrss_action_defaults_where_keys_do_not_conflict() {
        let defaults: Vec<_> = SHORTCUTS.iter().map(|(_, _, default)| *default).collect();
        assert_eq!(defaults, ["j", "k", "r", "f", "o"]);
    }
}

#[cfg(test)]
mod reader_layout_tests {
    use super::*;
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };

    struct Headless(Rc<MinimalSoftwareWindow>);

    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn long_reader_renders_and_scrolls() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(window))).unwrap();
        let ui = MainWindow::new().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(1000, 800));

        let blocks: Vec<ReaderBlock> = (0..300)
            .map(|index| {
                let text = StyledText::from_plain_text(&format!(
                    "Paragraph {index}: Lorem ipsum dolor sit amet, consectetur adipiscing elit."
                ));
                ReaderBlock {
                    kind: "p".into(),
                    label: "".into(),
                    text: text.clone(),
                    chunks: ModelRc::new(VecModel::from(vec![text])),
                    indent: 0,
                    source: "".into(),
                    image: slint::Image::default(),
                    has_image: false,
                }
            })
            .collect();
        let rows = make_reader_rows("A long article", "Example feed", blocks);
        ui.set_reader_blocks(ModelRc::new(VecModel::from(rows)));
        ui.show().unwrap();
        slint::platform::update_timers_and_animations();

        let started = std::time::Instant::now();
        let first = ui.window().take_snapshot().unwrap();
        eprintln!("long reader first frame: {:?}", started.elapsed());
        ui.invoke_scroll_reader(-500.0);
        slint::platform::update_timers_and_animations();
        let second = ui.window().take_snapshot().unwrap();

        assert_eq!(first.width(), second.width());
        assert_eq!(first.height(), second.height());
        assert_ne!(first.as_bytes(), second.as_bytes());

        let articles: Vec<ArticleRow> = (0..100)
            .map(|index| ArticleRow {
                id: format!("{index}").into(),
                title: format!("Article {index}").into(),
                feed: "Feed".into(),
                time: "now".into(),
                read: false,
                starred: false,
                selected: index == 50,
            })
            .collect();
        ui.set_articles(ModelRc::new(VecModel::from(articles)));
        slint::platform::update_timers_and_animations();
        let list_before = ui.window().take_snapshot().unwrap();
        ui.set_article_index(50);
        ui.invoke_reveal_article(50);
        slint::platform::update_timers_and_animations();
        let list_after = ui.window().take_snapshot().unwrap();
        assert_ne!(list_before.as_bytes(), list_after.as_bytes());
        ui.window().set_size(slint::PhysicalSize::new(600, 700));
        ui.set_compact_page("list".into());
        slint::platform::update_timers_and_animations();
        let compact_list = ui.window().take_snapshot().unwrap();
        ui.set_compact_page("reader".into());
        slint::platform::update_timers_and_animations();
        let compact_reader = ui.window().take_snapshot().unwrap();
        assert_ne!(compact_list.as_bytes(), compact_reader.as_bytes());

        let next_calls = Rc::new(Cell::new(0));
        ui.on_next_article({
            let next_calls = next_calls.clone();
            move || next_calls.set(next_calls.get() + 1)
        });
        let prev_calls = Rc::new(Cell::new(0));
        ui.on_prev_article({
            let prev_calls = prev_calls.clone();
            move || prev_calls.set(prev_calls.get() + 1)
        });
        let list_calls = Rc::new(Cell::new(0));
        ui.on_return_to_list({
            let list_calls = list_calls.clone();
            move || list_calls.set(list_calls.get() + 1)
        });
        let key = |key: slint::platform::Key| slint::platform::WindowEvent::KeyPressed {
            text: key.into(),
        };
        ui.set_article_index(50);
        // The first scroll uses pre-layout metrics; relayout, then close the remaining gap.
        ui.invoke_scroll_reader(-100_000.0);
        let _ = ui.window().take_snapshot();
        ui.invoke_scroll_reader(-100_000.0);
        slint::platform::update_timers_and_animations();
        let _ = ui.window().take_snapshot();
        ui.window()
            .dispatch_event(key(slint::platform::Key::DownArrow));
        slint::platform::update_timers_and_animations();
        ui.window()
            .dispatch_event(key(slint::platform::Key::DownArrow));
        slint::platform::update_timers_and_animations();
        assert_eq!(next_calls.get(), 0);
        ui.window()
            .dispatch_event(key(slint::platform::Key::DownArrow));
        slint::platform::update_timers_and_animations();
        assert_eq!(next_calls.get(), 1);

        ui.set_article_index(50);
        ui.invoke_scroll_reader(100_000.0);
        slint::platform::update_timers_and_animations();
        let _ = ui.window().take_snapshot();
        ui.window()
            .dispatch_event(key(slint::platform::Key::UpArrow));
        slint::platform::update_timers_and_animations();
        ui.window()
            .dispatch_event(key(slint::platform::Key::UpArrow));
        slint::platform::update_timers_and_animations();
        assert_eq!(prev_calls.get(), 0);
        ui.window()
            .dispatch_event(key(slint::platform::Key::UpArrow));
        slint::platform::update_timers_and_animations();
        assert_eq!(prev_calls.get(), 1);

        // Away from the matching edge, the same presses only scroll.
        for _ in 0..3 {
            ui.window()
                .dispatch_event(key(slint::platform::Key::DownArrow));
            slint::platform::update_timers_and_animations();
        }
        assert_eq!(next_calls.get(), 1);

        // The regular layout responds even when compact-page still says "list".
        ui.window().set_size(slint::PhysicalSize::new(1000, 800));
        ui.set_compact_page("list".into());
        ui.set_article_index(50);
        ui.invoke_scroll_reader(-100_000.0);
        let _ = ui.window().take_snapshot();
        ui.invoke_scroll_reader(-100_000.0);
        slint::platform::update_timers_and_animations();
        let _ = ui.window().take_snapshot();
        for _ in 0..3 {
            ui.window()
                .dispatch_event(key(slint::platform::Key::PageDown));
            slint::platform::update_timers_and_animations();
        }
        assert_eq!(next_calls.get(), 2);

        let drag = |from: (f32, f32), to: (f32, f32)| {
            let pos = |p: (f32, f32)| slint::LogicalPosition::new(p.0, p.1);
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: pos(from),
                    button: slint::platform::PointerEventButton::Left,
                });
            for step in 1..=4 {
                let progress = step as f32 / 4.0;
                let x = from.0 + (to.0 - from.0) * progress;
                let y = from.1 + (to.1 - from.1) * progress;
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                        position: pos((x, y)),
                    });
                slint::platform::update_timers_and_animations();
            }
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: pos(to),
                    button: slint::platform::PointerEventButton::Left,
                });
            slint::platform::update_timers_and_animations();
        };

        ui.window().set_size(slint::PhysicalSize::new(600, 700));
        ui.set_compact_page("reader".into());
        ui.set_article_index(50);

        // A downward swipe at the top goes to the previous article.
        ui.invoke_scroll_reader(100_000.0);
        let _ = ui.window().take_snapshot();
        let previous = prev_calls.get();
        drag((300.0, 200.0), (300.0, 340.0));
        assert_eq!(prev_calls.get(), previous + 1);

        // An upward swipe at the bottom goes to the next article.
        ui.invoke_scroll_reader(-100_000.0);
        let _ = ui.window().take_snapshot();
        ui.invoke_scroll_reader(-100_000.0);
        let _ = ui.window().take_snapshot();
        let upcoming = next_calls.get();
        drag((300.0, 400.0), (300.0, 260.0));
        assert_eq!(next_calls.get(), upcoming + 1);

        // Away from the edges, the same drags scroll the reader instead.
        ui.invoke_scroll_reader(100_000.0);
        let _ = ui.window().take_snapshot();
        ui.invoke_scroll_reader(-300.0);
        let _ = ui.window().take_snapshot();
        let before = ui.window().take_snapshot().unwrap();
        let previous = prev_calls.get();
        let upcoming = next_calls.get();
        drag((300.0, 200.0), (300.0, 340.0));
        let after = ui.window().take_snapshot().unwrap();
        assert_ne!(before.as_bytes(), after.as_bytes());
        drag((300.0, 400.0), (300.0, 260.0));
        assert_eq!(prev_calls.get(), previous);
        assert_eq!(next_calls.get(), upcoming);

        // A right swipe returns to the list from anywhere in the reader.
        let lists = list_calls.get();
        drag((100.0, 400.0), (320.0, 400.0));
        assert_eq!(list_calls.get(), lists + 1);
    }
}
