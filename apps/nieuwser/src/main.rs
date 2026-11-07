slint::include_modules!();

mod accounts;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use nieuwser_core::media::fetch_image;
use nieuwser_core::models::Stream;
use nieuwser_core::reader::{Block, BlockKind, html_to_blocks, html_to_text};
use nieuwser_core::store::ItemSummary;
use nieuwser_core::{Client, Store, SyncEngine, SyncReport};
use slint::{ComponentHandle, Model, ModelRc, StyledText, VecModel};

#[derive(Clone)]
struct ReaderView {
    title: String,
    meta: String,
    blocks: ModelRc<ReaderBlock>,
}

struct UiState {
    streams: Vec<Stream>,
    expanded_folders: HashSet<String>,
    items: Vec<ItemSummary>,
    loaded: usize,
    selected: HashSet<String>,
    anchor: Option<String>,
    handle: tokio::runtime::Handle,
    push_generation: u64,
    pending_icons: HashSet<String>,
    pending_media: HashSet<String>,
    undo: VecDeque<UndoAction>,
}

#[derive(Debug)]
struct ReadChange {
    id: String,
    was_read: bool,
}

#[derive(Debug)]
struct StarChange {
    id: String,
    was_starred: bool,
}

#[derive(Debug)]
enum UndoAction {
    Read(Vec<ReadChange>),
    Starred(Vec<StarChange>),
}

impl UndoAction {
    fn len(&self) -> usize {
        match self {
            Self::Read(changes) => changes.len(),
            Self::Starred(changes) => changes.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

const ARTICLE_PAGE: i64 = 300;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const PUSH_DEBOUNCE: Duration = Duration::from_secs(2);
const ICON_IMAGE_SIZE: u32 = 64;
const READER_IMAGE_SIZE: u32 = 1024;
const IMAGE_CACHE_LIMIT: usize = 48;
const READER_CACHE_LIMIT: usize = 24;
const UNDO_LIMIT: usize = 20;
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

static REMOTE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

thread_local! {
    // HashMap::new is not const on the supported Rust version.
    #[allow(clippy::missing_const_for_thread_local)]
    static IMAGE_CACHE: RefCell<HashMap<(String, u32), slint::Image>> = RefCell::new(HashMap::new());
    static READER_CACHE: RefCell<VecDeque<(String, ReaderView)>> =
        const { RefCell::new(VecDeque::new()) };
}

fn main() -> Result<()> {
    configure_fonts()?;
    let db_path = db_path()?;
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
        expanded_folders: HashSet::new(),
        items: Vec::new(),
        loaded: 0,
        selected: HashSet::new(),
        anchor: None,
        handle: runtime.handle().clone(),
        push_generation: 0,
        pending_icons: HashSet::new(),
        pending_media: HashSet::new(),
        undo: VecDeque::new(),
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

    let synced_ui = ui.as_weak();
    let synced_store = store.clone();
    let synced_timer = slint::Timer::default();
    synced_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(15),
        move || {
            if let Some(ui) = synced_ui.upgrade() {
                ui.set_last_synced(last_synced_text(&synced_store).into());
            }
        },
    );

    ui.run()?;
    save_pane_widths(&store, ui.get_feed_width(), ui.get_list_width());
    let restart = RESTART_REQUESTED.swap(false, Ordering::SeqCst);
    drop(synced_timer);
    drop(timer);
    drop(ui);
    drop(state);
    drop(store);
    runtime.shutdown_timeout(Duration::from_millis(100));
    if restart {
        relaunch()?;
    }
    Ok(())
}

enum SaveOutcome {
    Synced(SyncReport),
    Restart,
}

async fn save_account(
    store: Arc<Store>,
    url: &str,
    email: &str,
    password: &str,
) -> Result<SaveOutcome> {
    let client = Client::login(url, email, password).await?;
    let base = client.base_url().to_string();
    let key = accounts::account_key(&base, email);
    let current = store.account()?;
    let current_key = current
        .as_ref()
        .map(|(base, email)| accounts::account_key(base, email));
    let override_db = std::env::var_os("NIEUWSER_DB").is_some();

    if current_key.as_deref() == Some(key.as_str()) || (override_db && current.is_none()) {
        store.set_account(&base, email)?;
        store.set_auth_token(Some(client.auth_token()))?;
        let mut engine = SyncEngine::with_auth(store, &base, client.auth_token())?;
        let report = engine.sync().await?;
        return Ok(SaveOutcome::Synced(report));
    }

    if override_db {
        anyhow::bail!("NIEUWSER_DB overrides account storage; unset it to switch accounts");
    }

    let root = accounts::data_root();
    let path = accounts::account_db_path(&root, &key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = Store::open(&path)?;
    if let Some((existing_base, existing_email)) = target.account()?
        && (existing_base != base || existing_email != email)
    {
        anyhow::bail!(
            "account database {} belongs to another account",
            path.display()
        );
    }
    target.set_account(&base, email)?;
    target.set_auth_token(Some(client.auth_token()))?;
    drop(target);
    accounts::write_active_key(&root, &key)?;
    Ok(SaveOutcome::Restart)
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
        ui.on_save_pane_widths(move |feed_width, list_width| {
            save_pane_widths(&store, feed_width, list_width);
            if let Some(ui) = ui_weak.upgrade() {
                resize_sidebar_titles(&ui);
            }
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
                let result = save_account(store.clone(), &url, &email, &password).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_busy(false);
                        match result {
                            Ok(SaveOutcome::Synced(report)) => {
                                ui.set_settings_open(false);
                                ui.set_settings_password("".into());
                                ui.set_settings_status("".into());
                                ui.set_settings_has_account(true);
                                ui.set_status(format!("{report}").into());
                                ui.set_last_synced(last_synced_text(&store).into());
                                let _ = refresh_feeds(&ui, &store, &state);
                                let _ = reload_preserving_selection(
                                    &ui,
                                    &store,
                                    &state,
                                    ui.get_feed_index(),
                                );
                            }
                            Ok(SaveOutcome::Restart) => {
                                ui.set_settings_open(false);
                                ui.set_settings_password("".into());
                                ui.set_settings_status("".into());
                                ui.set_status("Account switched. Restarting…".into());
                                RESTART_REQUESTED.store(true, Ordering::SeqCst);
                                if let Err(error) = slint::quit_event_loop() {
                                    RESTART_REQUESTED.store(false, Ordering::SeqCst);
                                    ui.set_settings_status(
                                        format!(
                                            "Restart failed: {error}. Restart Nieuwser manually."
                                        )
                                        .into(),
                                    );
                                }
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
                if let Err(error) = refresh_feeds(&ui, &store, &state) {
                    ui.set_status(format!("Load failed: {error}").into());
                    return;
                }
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
                if let Err(error) = refresh_feeds(&ui, &store, &state) {
                    ui.set_status(format!("Search failed: {error}").into());
                    return;
                }
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
                if let Some(folder_id) = folder_at(&state, index) {
                    set_folder_expanded(&ui, &state, &folder_id, true);
                }
                if let Err(error) = load_stream(&ui, &store, &state, index) {
                    ui.set_status(format!("Load failed: {error}").into());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_toggle_folder(move |index| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let Some(folder_id) = folder_at(&state, index) else {
                return;
            };
            let expand = {
                let state = state.lock().unwrap();
                !state.expanded_folders.contains(&folder_id)
            };
            set_folder_expanded(&ui, &state, &folder_id, expand);
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
        let state = state.clone();
        ui.on_select_all(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let count = select_all_in_view(&state);
                sync_article_rows(&ui, &state);
                ui.set_status(
                    if count == 0 {
                        "No articles to select.".to_string()
                    } else {
                        format!("Selected {count} article(s)")
                    }
                    .into(),
                );
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let state = state.clone();
        ui.on_move_list_selection(move |delta| {
            if let Some(ui) = ui_weak.upgrade() {
                move_list_selection(&ui, &state, delta);
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
            let Some(item) = selected_item_or_first(&state, ui.get_article_index()) else {
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
            let Some(item) = selected_item_or_first(&state, ui.get_article_index()) else {
                return;
            };
            mark_selected_starred(&ui, &store, &state, !item.starred);
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let state = state.clone();
        ui.on_undo(move || {
            if let Some(ui) = ui_weak.upgrade() {
                undo_last(&ui, &store, &state);
            }
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
            state.lock().unwrap().undo.clear();
            let _ = refresh_feeds(&ui, &store, &state);
            let _ = load_stream(&ui, &store, &state, ui.get_feed_index());
            schedule_push(&ui, &store, &state);
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
        let result = {
            let _guard = REMOTE_LOCK.lock().await;
            match SyncEngine::with_auth(store.clone(), &base, &token) {
                Ok(mut engine) => engine.sync().await,
                Err(error) => Err(error),
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_syncing(false);
                match result {
                    Ok(report) => {
                        ui.set_status(format!("{report}").into());
                        ui.set_last_synced(last_synced_text(&store).into());
                        let _ = refresh_feeds(&ui, &store, &state);
                        let _ =
                            reload_preserving_selection(&ui, &store, &state, ui.get_feed_index());
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

fn schedule_push(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>) {
    let (generation, handle) = {
        let mut state = state.lock().unwrap();
        state.push_generation = state.push_generation.wrapping_add(1);
        (state.push_generation, state.handle.clone())
    };

    let ui_weak = ui.as_weak();
    let store = store.clone();
    let state = state.clone();
    handle.spawn(async move {
        tokio::time::sleep(PUSH_DEBOUNCE).await;
        {
            let state = state.lock().unwrap();
            if state.push_generation != generation {
                return;
            }
        }
        if store.outbox(1).map(|ops| ops.is_empty()).unwrap_or(true) {
            return;
        }
        let Some((base, _)) = store.account().ok().flatten() else {
            return;
        };
        let Some(token) = store.auth_token().ok().flatten() else {
            return;
        };

        let pushed = {
            let _guard = REMOTE_LOCK.lock().await;
            let Ok(mut engine) = SyncEngine::with_auth(store.clone(), &base, &token) else {
                return;
            };
            match engine.push_pending().await {
                Ok(pushed) => pushed,
                Err(error) => {
                    if error.is_auth_error() {
                        let _ = store.set_auth_token(None);
                        let ui_weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_status("Session expired while syncing read state.".into());
                            }
                        });
                    }
                    return;
                }
            }
        };
        if pushed == 0 {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_last_synced(last_synced_text(&store).into());
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
    let timestamp = store
        .state("last_contact_at")
        .ok()
        .flatten()
        .or_else(|| store.state("last_sync_at").ok().flatten())
        .and_then(|value| value.parse::<i64>().ok());
    match timestamp {
        Some(ts) => format!("Synced {}", sync_age(ts)),
        None => "Never synced".into(),
    }
}

fn sync_age(secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    sync_age_at(secs, now)
}

fn sync_age_at(secs: i64, now: i64) -> String {
    if secs <= 0 {
        return "just now".into();
    }
    let delta = (now - secs).max(0);
    if delta < 60 {
        "just now".into()
    } else if delta < 3_600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3_600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

struct SidebarRow {
    id: String,
    title: String,
    folder: String,
    folder_id: String,
    count: i64,
    icon_url: Option<String>,
    kind: &'static str,
    indent: i32,
    collapsed: bool,
    shown: bool,
}

fn build_sidebar(
    feeds: Vec<nieuwser_core::store::FeedRow>,
    counts: &HashMap<String, i64>,
    expanded: &HashSet<String>,
    total: i64,
    starred: i64,
) -> (Vec<SidebarRow>, Vec<Stream>) {
    let mut rows = vec![
        SidebarRow {
            id: "all".into(),
            title: "All items".into(),
            folder: String::new(),
            folder_id: String::new(),
            count: total,
            icon_url: None,
            kind: "all",
            indent: 0,
            collapsed: false,
            shown: true,
        },
        SidebarRow {
            id: "starred".into(),
            title: "Starred".into(),
            folder: String::new(),
            folder_id: String::new(),
            count: starred,
            icon_url: None,
            kind: "starred",
            indent: 0,
            collapsed: false,
            shown: true,
        },
    ];
    let mut streams = vec![Stream::All, Stream::Starred];

    let mut groups: Vec<(String, String, Vec<SidebarRow>)> = Vec::new();
    let mut ungrouped = Vec::new();
    for feed in feeds {
        if feed.id == "feed/unknown" {
            continue;
        }
        let count = counts.get(&feed.id).copied().unwrap_or(0);
        if count == 0 {
            continue;
        }
        let shown = feed
            .folder_id
            .as_deref()
            .is_none_or(|folder_id| expanded.contains(folder_id));
        let row = SidebarRow {
            id: feed.id.clone(),
            title: feed.title,
            folder: feed.folder_name.clone().unwrap_or_default(),
            folder_id: feed.folder_id.clone().unwrap_or_default(),
            count,
            icon_url: feed.icon_url,
            kind: "feed",
            indent: 1,
            collapsed: false,
            shown,
        };
        match feed.folder_id {
            Some(folder_id) => {
                let name = feed
                    .folder_name
                    .unwrap_or_else(|| folder_id.rsplit('/').next().unwrap_or(&folder_id).into());
                match groups.iter_mut().find(|group| group.0 == folder_id) {
                    Some(group) => group.2.push(row),
                    None => groups.push((folder_id, name, vec![row])),
                }
            }
            None => {
                let mut row = row;
                row.indent = 0;
                ungrouped.push(row);
            }
        }
    }

    for (folder_id, name, feeds) in groups {
        let count = feeds.iter().map(|feed| feed.count).sum();
        let is_collapsed = !expanded.contains(&folder_id);
        rows.push(SidebarRow {
            id: folder_id.clone(),
            title: name,
            folder: String::new(),
            folder_id: folder_id.clone(),
            count,
            icon_url: None,
            kind: "folder",
            indent: 0,
            collapsed: is_collapsed,
            shown: true,
        });
        streams.push(Stream::Folder(folder_id));
        for feed in feeds {
            streams.push(Stream::Feed(feed.id.clone()));
            rows.push(feed);
        }
    }
    for feed in ungrouped {
        streams.push(Stream::Feed(feed.id.clone()));
        rows.push(feed);
    }

    (rows, streams)
}

fn folder_at(state: &Arc<Mutex<UiState>>, index: i32) -> Option<String> {
    let state = state.lock().unwrap();
    match state.streams.get(index.max(0) as usize) {
        Some(Stream::Folder(id)) => Some(id.clone()),
        _ => None,
    }
}

fn set_folder_expanded(
    ui: &MainWindow,
    state: &Arc<Mutex<UiState>>,
    folder_id: &str,
    expanded: bool,
) {
    {
        let mut state = state.lock().unwrap();
        if expanded {
            state.expanded_folders.insert(folder_id.to_string());
        } else {
            state.expanded_folders.remove(folder_id);
        }
    }

    let model = ui.get_feeds();
    for row_index in 0..model.row_count() {
        let Some(mut row) = model.row_data(row_index) else {
            continue;
        };
        if row.folder_id.as_str() != folder_id {
            continue;
        }
        match row.kind.as_str() {
            "folder" => row.collapsed = !expanded,
            "feed" => row.shown = expanded,
            _ => continue,
        }
        model.set_row_data(row_index, row);
    }
}

fn sidebar_title_budget(feed_width: f32) -> usize {
    (((feed_width.max(160.0) - 118.0) / 7.0).floor() as usize).clamp(8, 64)
}

fn shorten_title(title: &str, max_chars: usize) -> String {
    let chars: Vec<char> = title.chars().collect();
    if chars.len() <= max_chars {
        return title.into();
    }
    let keep = max_chars.saturating_sub(1);
    let left = (keep * 3 / 5).max(1);
    let right = keep - left;
    let mut shortened = String::with_capacity(max_chars * 4);
    shortened.extend(&chars[..left]);
    shortened.push('…');
    shortened.extend(&chars[chars.len() - right..]);
    shortened
}

fn resize_sidebar_titles(ui: &MainWindow) {
    let budget = sidebar_title_budget(ui.get_feed_width());
    let model = ui.get_feeds();
    for index in 0..model.row_count() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        if row.kind.as_str() != "feed" && row.kind.as_str() != "folder" {
            continue;
        }
        let title = shorten_title(row.full_title.as_str(), budget);
        if row.title.as_str() != title {
            row.title = title.into();
            model.set_row_data(index, row);
        }
    }
}

fn refresh_feeds(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let selected = current_stream(state, ui.get_feed_index());
    let read = article_read_filter(ui.get_article_filter().as_str());
    let query = ui.get_search_query();
    let search = Some(query.as_str());

    let feeds = store.feeds()?;
    let counts = store.feed_counts(read, search)?;
    let total = store.count_items(&Stream::All, read, search)?;
    let starred = store.count_items(&Stream::Starred, read, search)?;
    let expanded = state.lock().unwrap().expanded_folders.clone();
    let (sidebar, streams) = build_sidebar(feeds, &counts, &expanded, total, starred);
    let title_budget = sidebar_title_budget(ui.get_feed_width());

    let mut rows = Vec::with_capacity(sidebar.len());
    for row in sidebar {
        let icon = match row.icon_url.as_deref() {
            Some(url) => {
                let icon = cached_image(url, ICON_IMAGE_SIZE);
                if icon.is_none() {
                    spawn_icon_fetch(ui, store, state, &row.id, url);
                }
                icon.unwrap_or_default()
            }
            None => slint::Image::default(),
        };
        let full_title = row.title;
        let title = if row.kind == "feed" || row.kind == "folder" {
            shorten_title(&full_title, title_budget)
        } else {
            full_title.clone()
        };
        rows.push(FeedRow {
            id: row.id.into(),
            title: title.into(),
            full_title: full_title.into(),
            folder: row.folder.into(),
            folder_id: row.folder_id.into(),
            unread: row.count.clamp(0, i32::MAX as i64) as i32,
            icon,
            kind: row.kind.into(),
            indent: row.indent,
            collapsed: row.collapsed,
            shown: row.shown,
        });
    }

    let index = streams
        .iter()
        .position(|stream| *stream == selected)
        .unwrap_or(0);
    state.lock().unwrap().streams = streams;
    if let Some(model) = ui.get_feeds().as_any().downcast_ref::<VecModel<FeedRow>>() {
        model.set_vec(rows);
    } else {
        ui.set_feeds(ModelRc::new(VecModel::from(rows)));
    }
    ui.set_feed_index(index as i32);
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
        state.loaded = state.items.len();
        state.selected.clear();
        state.anchor = None;
    }
    sync_article_rows(ui, state);
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

// Keep feed, folder, and "All items" badges in sync without re-reading feeds.
fn filtered_read_delta(ui: &MainWindow, unread_delta: i32) -> i32 {
    match ui.get_article_filter().as_str() {
        "unread" => unread_delta,
        "read" => -unread_delta,
        _ => 0,
    }
}

fn adjust_unread(ui: &MainWindow, feed_id: &str, unread_delta: i32) {
    let delta = filtered_read_delta(ui, unread_delta);
    if delta == 0 {
        return;
    }
    let model = ui.get_feeds();
    let folder_id = (0..model.row_count()).find_map(|index| {
        let row = model.row_data(index)?;
        (row.id.as_str() == feed_id && row.kind.as_str() == "feed")
            .then(|| row.folder_id.to_string())
    });
    for index in 0..model.row_count() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        let in_folder = !row.folder_id.is_empty()
            && folder_id.as_deref() == Some(row.folder_id.as_str())
            && row.kind.as_str() == "folder";
        if row.id.as_str() == "all" || row.id.as_str() == feed_id || in_folder {
            row.unread = (row.unread + delta).max(0);
            model.set_row_data(index, row);
        }
    }
}

fn adjust_starred(ui: &MainWindow, delta: i32) {
    if delta == 0 {
        return;
    }
    let model = ui.get_feeds();
    for index in 0..model.row_count() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        if row.id.as_str() == "starred" {
            row.unread = (row.unread + delta).max(0);
            model.set_row_data(index, row);
        }
    }
}

fn adjust_starred_read(ui: &MainWindow, unread_delta: i32) {
    adjust_starred(ui, filtered_read_delta(ui, unread_delta));
}

fn load_more(ui: &MainWindow, store: &Store, state: &Arc<Mutex<UiState>>) -> Result<()> {
    let (stream, offset) = {
        let state = state.lock().unwrap();
        let index = ui.get_feed_index().max(0) as usize;
        (state.streams.get(index).cloned(), state.loaded)
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
    state.loaded += items.len();
    state.items.extend(items);
    let model = ui.get_articles();
    for item in &state.items[start..] {
        let _ = model.push_row(article_row(item, state.selected.contains(&item.id)));
    }
    Ok(())
}

fn pin_item(items: &mut Vec<ItemSummary>, lead: &ItemSummary) {
    if items.iter().any(|item| item.id == lead.id) {
        return;
    }
    items.push(lead.clone());
    items.sort_by(|a, b| {
        b.timestamp_usec
            .cmp(&a.timestamp_usec)
            .then_with(|| b.id.cmp(&a.id))
    });
}

fn reload_preserving_selection(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    stream_index: i32,
) -> Result<()> {
    let (selected, anchor, lead, reader_open) = {
        let state = state.lock().unwrap();
        let index = ui.get_article_index();
        let lead = state
            .items
            .get(index.max(0) as usize)
            .filter(|_| index >= 0)
            .cloned();
        (
            state.selected.clone(),
            state.anchor.clone(),
            lead,
            ui.get_compact_page().as_str() == "reader",
        )
    };
    clear_reader_cache();
    load_stream(ui, store, state, stream_index)?;
    {
        let mut state = state.lock().unwrap();
        if let Some(lead) = &lead {
            pin_item(&mut state.items, lead);
        }
        let items = &state.items;
        state.selected = selected
            .into_iter()
            .filter(|id| items.iter().any(|item| &item.id == id))
            .collect();
        state.anchor = anchor.filter(|id| state.selected.contains(id));
    }
    sync_article_rows(ui, state);
    if let Some(lead) = lead {
        let found = state
            .lock()
            .unwrap()
            .items
            .iter()
            .position(|item| item.id == lead.id);
        if let Some(found) = found {
            ui.set_article_index(found as i32);
            ui.invoke_reveal_article(found as i32);
            select_article(ui, store, state, found as i32, false, false)?;
            if reader_open {
                ui.set_compact_page("reader".into());
            }
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
                    let markdown = if matches!(block.kind, BlockKind::Heading(_)) {
                        format!("**{}**", chunk.trim())
                    } else {
                        chunk
                    };
                    StyledText::from_markdown(&markdown)
                        .unwrap_or_else(|_| StyledText::from_plain_text(&markdown))
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

fn move_list_selection(ui: &MainWindow, state: &Arc<Mutex<UiState>>, delta: i32) {
    let Some(next) = shift_select_in_view(state, ui.get_article_index(), delta) else {
        return;
    };
    ui.set_article_index(next);
    ui.invoke_reveal_article(next);
    sync_article_rows(ui, state);
}

fn shift_select_in_view(state: &Arc<Mutex<UiState>>, current: i32, delta: i32) -> Option<i32> {
    if delta == 0 {
        return None;
    }
    let (start, len) = {
        let state = state.lock().unwrap();
        let len = state.items.len();
        if len == 0 {
            return None;
        }
        let start = if current < 0 {
            if delta > 0 { 0 } else { len as i32 - 1 }
        } else {
            current.min(len as i32 - 1)
        };
        (start, len)
    };
    if current < 0 {
        apply_selection(state, start, false, false);
        return Some(start);
    }

    {
        let mut state = state.lock().unwrap();
        let anchor_is_valid = state
            .anchor
            .as_ref()
            .is_some_and(|anchor| state.items.iter().any(|item| &item.id == anchor));
        if !anchor_is_valid {
            state.anchor = Some(state.items[start as usize].id.clone());
        }
    }
    let next = (start + delta).clamp(0, len as i32 - 1);
    apply_selection(state, next, false, true);
    Some(next)
}

fn select_all_in_view(state: &Arc<Mutex<UiState>>) -> usize {
    let mut state = state.lock().unwrap();
    state.selected = state.items.iter().map(|item| item.id.clone()).collect();
    state.anchor = state.items.first().map(|item| item.id.clone());
    state.items.len()
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
    let starred = {
        let mut state = state.lock().unwrap();
        let starred = state
            .items
            .iter()
            .find(|item| item.id == id)
            .is_some_and(|item| item.starred);
        if let Some(item) = state.items.iter_mut().find(|item| item.id == id) {
            item.read = true;
        }
        starred
    };
    sync_article_rows(ui, state);
    adjust_unread(ui, feed_id, -1);
    if starred {
        adjust_starred_read(ui, -1);
    }
    schedule_push(ui, store, state);
    Ok(())
}

fn read_changes(state: &Arc<Mutex<UiState>>, ids: &[String], read: bool) -> Vec<ReadChange> {
    let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
    state
        .lock()
        .unwrap()
        .items
        .iter()
        .filter(|item| ids.contains(item.id.as_str()) && item.read != read)
        .map(|item| ReadChange {
            id: item.id.clone(),
            was_read: item.read,
        })
        .collect()
}

fn star_changes(state: &Arc<Mutex<UiState>>, ids: &[String], starred: bool) -> Vec<StarChange> {
    let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
    state
        .lock()
        .unwrap()
        .items
        .iter()
        .filter(|item| ids.contains(item.id.as_str()) && item.starred != starred)
        .map(|item| StarChange {
            id: item.id.clone(),
            was_starred: item.starred,
        })
        .collect()
}

fn record_undo(state: &Arc<Mutex<UiState>>, action: UndoAction) {
    if action.is_empty() {
        return;
    }
    let mut state = state.lock().unwrap();
    state.undo.push_back(action);
    while state.undo.len() > UNDO_LIMIT {
        state.undo.pop_front();
    }
}

fn undo_last(ui: &MainWindow, store: &Arc<Store>, state: &Arc<Mutex<UiState>>) {
    let action = state.lock().unwrap().undo.pop_back();
    let Some(action) = action else {
        ui.set_status("Nothing to undo.".into());
        return;
    };
    let result = match &action {
        UndoAction::Read(changes) => restore_read(ui, store, state, changes),
        UndoAction::Starred(changes) => restore_starred(ui, store, state, changes),
    };
    match result {
        Ok(()) => {
            let remaining = state.lock().unwrap().undo.len();
            let hint = if remaining > 0 {
                " Ctrl+Z for more."
            } else {
                ""
            };
            ui.set_status(
                format!("Undid last change for {} article(s).{hint}", action.len()).into(),
            );
        }
        Err(error) => {
            state.lock().unwrap().undo.push_back(action);
            ui.set_status(format!("Undo failed: {error}").into());
        }
    }
}

fn restore_read(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    changes: &[ReadChange],
) -> Result<(), String> {
    let mut read = Vec::new();
    let mut unread = Vec::new();
    for change in changes {
        if change.was_read {
            read.push(change.id.clone());
        } else {
            unread.push(change.id.clone());
        }
    }
    set_items_read(ui, store, state, &read, true)?;
    set_items_read(ui, store, state, &unread, false)?;
    Ok(())
}

fn restore_starred(
    ui: &MainWindow,
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    changes: &[StarChange],
) -> Result<(), String> {
    let mut starred = Vec::new();
    let mut unstarred = Vec::new();
    for change in changes {
        if change.was_starred {
            starred.push(change.id.clone());
        } else {
            unstarred.push(change.id.clone());
        }
    }
    set_items_starred(store, state, &starred, true)?;
    set_items_starred(store, state, &unstarred, false)?;
    adjust_starred(ui, starred.len() as i32 - unstarred.len() as i32);
    sync_article_rows(ui, state);
    schedule_push(ui, store, state);
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
    let changes = read_changes(state, &ids, read);
    if changes.is_empty() {
        return;
    }
    let changed_ids: Vec<String> = changes.iter().map(|change| change.id.clone()).collect();
    if let Err(error) = set_items_read(ui, store, state, &changed_ids, read) {
        ui.set_status(format!("Update failed: {error}").into());
        return;
    }
    record_undo(state, UndoAction::Read(changes));
    ui.set_status(
        format!(
            "Marked {} article(s) {} (queued) — Ctrl+Z to undo",
            changed_ids.len(),
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
    let mut starred_unread_delta = 0;
    {
        let state = state.lock().unwrap();
        for item in &state.items {
            if id_set.contains(item.id.as_str()) && item.read != read {
                *deltas.entry(item.feed_id.clone()).or_default() += if read { -1 } else { 1 };
                if item.starred {
                    starred_unread_delta += if read { -1 } else { 1 };
                }
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
    adjust_starred_read(ui, starred_unread_delta);
    schedule_push(ui, store, state);
    Ok(())
}

fn set_items_starred(
    store: &Arc<Store>,
    state: &Arc<Mutex<UiState>>,
    ids: &[String],
    starred: bool,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    store
        .set_starred(ids, starred)
        .map_err(|error| error.to_string())?;
    let id_set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut state = state.lock().unwrap();
    for item in &mut state.items {
        if id_set.contains(item.id.as_str()) {
            item.starred = starred;
        }
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
    let changes = star_changes(state, &ids, starred);
    if changes.is_empty() {
        return;
    }
    let changed_ids: Vec<String> = changes.iter().map(|change| change.id.clone()).collect();
    if let Err(error) = set_items_starred(store, state, &changed_ids, starred) {
        ui.set_status(format!("Update failed: {error}").into());
        return;
    }
    let delta = if starred {
        changed_ids.len() as i32
    } else {
        -(changed_ids.len() as i32)
    };
    adjust_starred(ui, delta);
    sync_article_rows(ui, state);
    record_undo(state, UndoAction::Starred(changes));
    schedule_push(ui, store, state);
    ui.set_status(
        format!(
            "{} {} article(s) (queued) — Ctrl+Z to undo",
            if starred { "Starred" } else { "Unstarred" },
            changed_ids.len()
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

fn selected_item_or_first(state: &Arc<Mutex<UiState>>, index: i32) -> Option<ItemSummary> {
    selected_item(state, index).or_else(|| {
        let state = state.lock().unwrap();
        state
            .items
            .iter()
            .find(|item| state.selected.contains(&item.id))
            .cloned()
    })
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

const READER_FONT_FAMILY: &str = "Nieuwser Reader";
const READER_LINE_HEIGHT: f32 = 1.625;
const NOTO_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoEmoji-VariableFont_wght.ttf");

fn configure_fonts() -> Result<()> {
    use slint::fontique_011::fontique::{Blob, GenericFamily};

    let mut fonts = slint::fontique_011::shared_collection();
    let family = fonts
        .register_fonts(Blob::new(Arc::new(NOTO_EMOJI)), None)
        .first()
        .map(|(family, _)| *family)
        .ok_or_else(|| anyhow::anyhow!("bundled Noto Emoji font is invalid"))?;
    fonts.set_generic_families(GenericFamily::Emoji, std::iter::once(family));
    register_reader_font(&mut fonts);
    Ok(())
}

/// Registers a copy of the system sans font under our own family, with its line
/// metrics rewritten so that StyledText (which has no line-height property)
/// still lays out with a fixed reading line height.
fn register_reader_font(fonts: &mut slint::fontique_011::fontique::Collection) {
    use slint::fontique_011::fontique::{
        Attributes, Blob, FontInfoOverride, FontStyle, FontWeight, GenericFamily, QueryFamily,
        QueryStatus, SourceCache,
    };

    let mut source_cache = SourceCache::default();
    for (weight, style) in [
        (FontWeight::NORMAL, FontStyle::Normal),
        (FontWeight::BOLD, FontStyle::Normal),
        (FontWeight::NORMAL, FontStyle::Italic),
    ] {
        let mut query = fonts.query(&mut source_cache);
        query.set_families([
            QueryFamily::Named("Noto Sans"),
            QueryFamily::Generic(GenericFamily::SansSerif),
        ]);
        query.set_attributes(Attributes {
            weight,
            style,
            ..Default::default()
        });
        let mut found = None;
        query.matches_with(|font| {
            found = Some(font.blob.data().to_vec());
            QueryStatus::Stop
        });
        drop(query);

        let Some(mut data) = found else { continue };
        if !retune_line_metrics(&mut data, READER_LINE_HEIGHT) {
            continue;
        }
        fonts.register_fonts(
            Blob::new(Arc::new(data)),
            Some(FontInfoOverride {
                family_name: Some(READER_FONT_FAMILY),
                ..Default::default()
            }),
        );
    }
}

fn retune_line_metrics(data: &mut [u8], target: f32) -> bool {
    let mut tuned = false;
    for directory in sfnt_directories(data) {
        let Some(tables) = sfnt_tables(data, directory) else {
            continue;
        };
        let Some(head) = tables.head else { continue };
        let Some(hhea) = tables.hhea else { continue };
        let Some(upm) = head
            .checked_add(18)
            .and_then(|offset| read_u16(data, offset))
            .filter(|&upm| upm > 0)
        else {
            continue;
        };
        let target_units = (target * f32::from(upm)).round() as i32;

        if let (Some(ascender), Some(descender)) = (
            hhea.checked_add(4)
                .and_then(|offset| read_i16(data, offset)),
            hhea.checked_add(6)
                .and_then(|offset| read_i16(data, offset)),
        ) {
            let gap = target_units - (i32::from(ascender) - i32::from(descender));
            if let Some(offset) = hhea.checked_add(8) {
                tuned |= write_i16(data, offset, gap);
            }
        }

        if let Some(os2) = tables.os2 {
            if let (Some(ascender), Some(descender)) = (
                os2.checked_add(68)
                    .and_then(|offset| read_i16(data, offset)),
                os2.checked_add(70)
                    .and_then(|offset| read_i16(data, offset)),
            ) {
                let gap = target_units - (i32::from(ascender) - i32::from(descender));
                if let Some(offset) = os2.checked_add(72) {
                    tuned |= write_i16(data, offset, gap);
                }
            }
        }
    }
    tuned
}

struct SfntTables {
    head: Option<usize>,
    hhea: Option<usize>,
    os2: Option<usize>,
}

fn sfnt_directories(data: &[u8]) -> Vec<usize> {
    if data.get(..4) == Some(b"ttcf") {
        let count = read_u32(data, 8).unwrap_or(0).min(64) as usize;
        (0..count)
            .filter_map(|index| 12usize.checked_add(index.checked_mul(4)?))
            .filter_map(|offset| read_u32(data, offset))
            .map(|offset| offset as usize)
            .filter(|&offset| offset.checked_add(12).is_some_and(|end| end <= data.len()))
            .collect()
    } else if data.len() >= 12 {
        vec![0]
    } else {
        Vec::new()
    }
}

fn sfnt_tables(data: &[u8], directory: usize) -> Option<SfntTables> {
    let count = directory
        .checked_add(4)
        .and_then(|offset| read_u16(data, offset))? as usize;
    let mut tables = SfntTables {
        head: None,
        hhea: None,
        os2: None,
    };
    for index in 0..count {
        let record = directory
            .checked_add(12)?
            .checked_add(index.checked_mul(16)?)?;
        let tag = data.get(record..record.checked_add(4)?)?;
        let offset = record
            .checked_add(8)
            .and_then(|offset| read_u32(data, offset))? as usize;
        match tag {
            b"head" => tables.head = Some(offset),
            b"hhea" => tables.hhea = Some(offset),
            b"OS/2" => tables.os2 = Some(offset),
            _ => {}
        }
    }
    Some(tables)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    read_u16(data, offset).map(|value| value as i16)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn write_i16(data: &mut [u8], offset: usize, value: i32) -> bool {
    let Ok(value) = i16::try_from(value) else {
        return false;
    };
    let Some(end) = offset.checked_add(2) else {
        return false;
    };
    let Some(slot) = data.get_mut(offset..end) else {
        return false;
    };
    slot.copy_from_slice(&value.to_be_bytes());
    true
}

fn db_path() -> Result<PathBuf> {
    accounts::resolve_db_path(&accounts::data_root())
}

fn relaunch() -> Result<()> {
    let exe = match std::env::var_os("APPIMAGE").filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .map_err(|error| anyhow::anyhow!("could not determine the executable path: {error}"))?,
    };
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    command
        .spawn()
        .map_err(|error| anyhow::anyhow!("could not restart Nieuwser: {error}"))?;
    Ok(())
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
    fn retunes_font_line_metrics() {
        let mut data = NOTO_EMOJI.to_vec();
        assert!(retune_line_metrics(&mut data, READER_LINE_HEIGHT));
        let directory = sfnt_directories(&data)[0];
        let tables = sfnt_tables(&data, directory).unwrap();
        let upm = read_u16(&data, tables.head.unwrap() + 18).unwrap();
        let hhea = tables.hhea.unwrap();
        let ascender = read_i16(&data, hhea + 4).unwrap();
        let descender = read_i16(&data, hhea + 6).unwrap();
        let gap = read_i16(&data, hhea + 8).unwrap();
        let ratio = (f32::from(ascender) - f32::from(descender) + f32::from(gap)) / f32::from(upm);
        assert!((ratio - READER_LINE_HEIGHT).abs() < 0.002);
    }

    #[test]
    fn headings_render_bold() {
        let block = to_reader_block(
            Block {
                kind: BlockKind::Heading(2),
                label: String::new(),
                text: "Section".into(),
                url: None,
            },
            None,
        );
        let expected = StyledText::from_markdown("**Section**").unwrap();
        assert_eq!(block.chunks.row_data(0), Some(expected));
    }

    #[test]
    fn long_reader_renders_and_scrolls() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(window))).unwrap();
        let ui = MainWindow::new().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(1000, 800));

        let mut blocks: Vec<ReaderBlock> = vec![ReaderBlock {
            kind: "image".into(),
            label: "Image".into(),
            text: StyledText::default(),
            chunks: ModelRc::new(VecModel::from(Vec::<StyledText>::new())),
            indent: 0,
            source: "".into(),
            image: image_from_rgba(vec![255, 0, 0, 255], 1, 1),
            has_image: true,
        }];
        blocks.extend((0..300).map(|index| {
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
        }));
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
        // Row heights are estimated until rendered, so the end keeps moving while scrolling to it.
        let scroll_to_end = || {
            for _ in 0..4 {
                ui.invoke_scroll_reader(-100_000.0);
                slint::platform::update_timers_and_animations();
                let _ = ui.window().take_snapshot();
            }
        };
        scroll_to_end();
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
        scroll_to_end();
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

        let touch_drag = |from: (f32, f32), to: (f32, f32)| {
            use i_slint_core::input::TouchPhase;
            let pos = |p: (f32, f32)| i_slint_core::lengths::LogicalPoint::new(p.0, p.1);
            let touch = |point, phase| {
                slint::platform::WindowEvent::internal(slint::platform::InternalEvent::Touch {
                    id: 0,
                    position: point,
                    phase,
                })
            };
            ui.window()
                .dispatch_event(touch(pos(from), TouchPhase::Started));
            for step in 1..=4 {
                let progress = step as f32 / 4.0;
                let x = from.0 + (to.0 - from.0) * progress;
                let y = from.1 + (to.1 - from.1) * progress;
                ui.window()
                    .dispatch_event(touch(pos((x, y)), TouchPhase::Moved));
                slint::platform::update_timers_and_animations();
            }
            ui.window()
                .dispatch_event(touch(pos(to), TouchPhase::Ended));
            slint::platform::update_timers_and_animations();
        };

        ui.window().set_size(slint::PhysicalSize::new(600, 700));
        ui.set_compact_page("reader".into());
        ui.set_article_index(50);

        // Dragging upward over the image at the top scrolls instead of navigating.
        ui.invoke_scroll_reader(100_000.0);
        let image_before = ui.window().take_snapshot().unwrap();
        touch_drag((300.0, 400.0), (300.0, 260.0));
        let image_after = ui.window().take_snapshot().unwrap();
        assert_ne!(image_before.as_bytes(), image_after.as_bytes());

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

        let before_confirm = ui.window().take_snapshot().unwrap();
        ui.invoke_open_mark_all_confirm();
        slint::platform::update_timers_and_animations();
        let confirm = ui.window().take_snapshot().unwrap();
        assert_ne!(before_confirm.as_bytes(), confirm.as_bytes());
        ui.invoke_close_mark_all_confirm();
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use nieuwser_core::store::ItemSummary;

    fn summary(id: &str) -> ItemSummary {
        ItemSummary {
            id: id.into(),
            feed_id: "feed/1".into(),
            feed_title: "Feed".into(),
            title: format!("Article {id}"),
            author: None,
            published: 1_700_000_000,
            href: None,
            read: false,
            starred: false,
            timestamp_usec: 1_700_000_000_000_000,
        }
    }

    #[test]
    fn selects_every_loaded_item_in_the_current_view() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = Arc::new(Mutex::new(UiState {
            streams: Vec::new(),
            expanded_folders: HashSet::new(),
            items: vec![summary("a"), summary("b"), summary("c")],
            loaded: 0,
            selected: HashSet::from(["old".to_string()]),
            anchor: None,
            handle: runtime.handle().clone(),
            push_generation: 0,
            pending_icons: HashSet::new(),
            pending_media: HashSet::new(),
            undo: VecDeque::new(),
        }));

        assert_eq!(select_all_in_view(&state), 3);
        let state = state.lock().unwrap();
        assert_eq!(
            state.selected,
            HashSet::from(["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(state.anchor.as_deref(), Some("a"));
    }

    #[test]
    fn shift_arrows_extend_the_selection_from_its_anchor() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = Arc::new(Mutex::new(UiState {
            streams: Vec::new(),
            expanded_folders: HashSet::new(),
            items: vec![summary("a"), summary("b"), summary("c"), summary("d")],
            loaded: 0,
            selected: HashSet::from(["b".to_string()]),
            anchor: Some("b".to_string()),
            handle: runtime.handle().clone(),
            push_generation: 0,
            pending_icons: HashSet::new(),
            pending_media: HashSet::new(),
            undo: VecDeque::new(),
        }));

        assert_eq!(shift_select_in_view(&state, 1, 1), Some(2));
        assert_eq!(
            state.lock().unwrap().selected,
            HashSet::from(["b".to_string(), "c".to_string()])
        );
        assert_eq!(shift_select_in_view(&state, 2, 1), Some(3));
        assert_eq!(
            state.lock().unwrap().selected,
            HashSet::from(["b".to_string(), "c".to_string(), "d".to_string()])
        );
        assert_eq!(shift_select_in_view(&state, 3, -1), Some(2));
        assert_eq!(
            state.lock().unwrap().selected,
            HashSet::from(["b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn shift_arrows_start_at_the_edge_when_no_row_is_active() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = Arc::new(Mutex::new(UiState {
            streams: Vec::new(),
            expanded_folders: HashSet::new(),
            items: vec![summary("a"), summary("b")],
            loaded: 0,
            selected: HashSet::new(),
            anchor: None,
            handle: runtime.handle().clone(),
            push_generation: 0,
            pending_icons: HashSet::new(),
            pending_media: HashSet::new(),
            undo: VecDeque::new(),
        }));

        assert_eq!(shift_select_in_view(&state, -1, 1), Some(0));
        assert_eq!(
            state.lock().unwrap().selected,
            HashSet::from(["a".to_string()])
        );
        assert_eq!(shift_select_in_view(&state, 0, -1), Some(0));
        assert_eq!(shift_select_in_view(&state, 1, 1), Some(1));
    }

    #[test]
    fn toggle_actions_fall_back_to_the_first_selected_item() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let state = Arc::new(Mutex::new(UiState {
            streams: Vec::new(),
            expanded_folders: HashSet::new(),
            items: vec![summary("a"), summary("b")],
            loaded: 0,
            selected: HashSet::from(["b".to_string()]),
            anchor: Some("b".to_string()),
            handle: runtime.handle().clone(),
            push_generation: 0,
            pending_icons: HashSet::new(),
            pending_media: HashSet::new(),
            undo: VecDeque::new(),
        }));

        assert_eq!(selected_item_or_first(&state, -1).unwrap().id, "b");
        assert_eq!(selected_item_or_first(&state, 1).unwrap().id, "b");
    }

    #[test]
    fn pins_the_open_item_in_chronological_order() {
        let mut items = vec![summary("new"), summary("old")];
        items[0].timestamp_usec = 30;
        items[1].timestamp_usec = 10;
        let mut lead = summary("mid");
        lead.timestamp_usec = 20;

        pin_item(&mut items, &lead);
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old"]);

        pin_item(&mut items, &lead);
        assert_eq!(items.len(), 3);
    }
}

#[cfg(test)]
mod undo_tests {
    use super::*;
    use nieuwser_core::models::{Item, Subscription};
    use nieuwser_core::store::ItemSummary;

    fn summary(id: &str, read: bool, starred: bool) -> ItemSummary {
        ItemSummary {
            id: id.into(),
            feed_id: "feed/1".into(),
            feed_title: "Feed".into(),
            title: format!("Article {id}"),
            author: None,
            published: 1_700_000_000,
            href: None,
            read,
            starred,
            timestamp_usec: 1_700_000_000_000_000,
        }
    }

    fn state_with(items: Vec<ItemSummary>) -> Arc<Mutex<UiState>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        Arc::new(Mutex::new(UiState {
            streams: Vec::new(),
            expanded_folders: HashSet::new(),
            items,
            loaded: 0,
            selected: HashSet::new(),
            anchor: None,
            handle: runtime.handle().clone(),
            push_generation: 0,
            pending_icons: HashSet::new(),
            pending_media: HashSet::new(),
            undo: VecDeque::new(),
        }))
    }

    #[test]
    fn snapshots_only_items_whose_state_changes() {
        let state = state_with(vec![summary("a", false, false), summary("b", true, true)]);
        let ids = vec!["a".to_string(), "b".to_string(), "missing".to_string()];

        let changes = read_changes(&state, &ids, true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].id, "a");
        assert!(!changes[0].was_read);
        assert!(read_changes(&state, &ids, true).len() == 1);
        assert!(read_changes(&state, &["b".to_string()], true).is_empty());

        let changes = star_changes(&state, &ids, true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].id, "a");
        assert!(!changes[0].was_starred);
        assert!(star_changes(&state, &["b".to_string()], true).is_empty());
    }

    #[test]
    fn undo_history_keeps_a_bounded_tail() {
        let state = state_with(Vec::new());
        for index in 0..UNDO_LIMIT + 5 {
            record_undo(
                &state,
                UndoAction::Read(vec![ReadChange {
                    id: index.to_string(),
                    was_read: false,
                }]),
            );
        }
        let state = state.lock().unwrap();
        assert_eq!(state.undo.len(), UNDO_LIMIT);
        match state.undo.front().unwrap() {
            UndoAction::Read(changes) => assert_eq!(changes[0].id, "5"),
            UndoAction::Starred(_) => panic!("unexpected star action"),
        }
    }

    #[test]
    fn setting_stars_updates_the_store_and_visible_state() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .reconcile_feeds(
                &[Subscription {
                    id: "feed/1".into(),
                    title: "Feed".into(),
                    ..Default::default()
                }],
                &[],
            )
            .unwrap();
        store
            .upsert_items(&[Item {
                id: "a".into(),
                timestamp_usec: Some(1_000_000),
                content: Some(nieuwser_core::models::Content {
                    content: "<p>Body</p>".into(),
                }),
                origin: Some(nieuwser_core::models::Origin {
                    stream_id: "feed/1".into(),
                    title: Some("Feed".into()),
                    html_url: None,
                }),
                ..Default::default()
            }])
            .unwrap();
        let state = state_with(vec![summary("a", false, false)]);

        set_items_starred(&store, &state, &["a".to_string()], true).unwrap();
        assert!(store.item("a").unwrap().unwrap().starred);
        assert!(state.lock().unwrap().items[0].starred);

        set_items_starred(&store, &state, &["a".to_string()], false).unwrap();
        assert!(!store.item("a").unwrap().unwrap().starred);
        assert!(!state.lock().unwrap().items[0].starred);
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;

    fn feed(
        id: &str,
        title: &str,
        folder_id: Option<&str>,
        folder_name: Option<&str>,
    ) -> nieuwser_core::store::FeedRow {
        nieuwser_core::store::FeedRow {
            id: id.into(),
            title: title.into(),
            folder_id: folder_id.map(Into::into),
            folder_name: folder_name.map(Into::into),
            unread_count: 0,
            icon_url: None,
            html_url: None,
        }
    }

    #[test]
    fn groups_matching_feeds_under_folders_and_hides_empty_ones() {
        let feeds = vec![
            feed("feed/1", "One", Some("user/-/label/News"), Some("News")),
            feed("feed/2", "Two", Some("user/-/label/News"), Some("News")),
            feed("feed/3", "Three", None, None),
            feed("feed/4", "Four", Some("user/-/label/Tech"), Some("Tech")),
        ];
        let counts = HashMap::from([
            ("feed/1".to_string(), 2),
            ("feed/2".to_string(), 0),
            ("feed/3".to_string(), 1),
            ("feed/4".to_string(), 0),
        ]);

        let (rows, streams) = build_sidebar(feeds, &counts, &HashSet::new(), 3, 1);

        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["all", "starred", "user/-/label/News", "feed/1", "feed/3"]
        );
        assert_eq!(
            streams,
            vec![
                Stream::All,
                Stream::Starred,
                Stream::Folder("user/-/label/News".into()),
                Stream::Feed("feed/1".into()),
                Stream::Feed("feed/3".into()),
            ]
        );

        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[1].count, 1);
        assert_eq!(rows[2].kind, "folder");
        assert_eq!(rows[2].count, 2);
        assert_eq!(rows[2].indent, 0);
        assert!(rows[2].collapsed);
        assert_eq!(rows[3].kind, "feed");
        assert_eq!(rows[3].indent, 1);
        assert!(!rows[3].shown);
        assert_eq!(rows[4].indent, 0);
    }

    #[test]
    fn expanded_folders_reveal_children() {
        let feeds = vec![
            feed("feed/1", "One", Some("user/-/label/News"), Some("News")),
            feed("feed/3", "Three", None, None),
        ];
        let counts = HashMap::from([("feed/1".to_string(), 2), ("feed/3".to_string(), 1)]);
        let expanded = HashSet::from(["user/-/label/News".to_string()]);

        let (rows, streams) = build_sidebar(feeds, &counts, &expanded, 3, 0);

        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["all", "starred", "user/-/label/News", "feed/1", "feed/3"]
        );
        assert_eq!(
            streams,
            vec![
                Stream::All,
                Stream::Starred,
                Stream::Folder("user/-/label/News".into()),
                Stream::Feed("feed/1".into()),
                Stream::Feed("feed/3".into()),
            ]
        );
        assert_eq!(rows[2].count, 2);
        assert!(!rows[2].collapsed);
        assert!(rows[3].shown);
        assert!(rows[4].shown);
    }

    #[test]
    fn shortens_long_titles_from_the_middle() {
        assert_eq!(shorten_title("Hacker News", 20), "Hacker News");
        let shortened = shorten_title("The Very Long Feed Name Substack", 24);
        assert_eq!(shortened.chars().count(), 24);
        assert!(shortened.starts_with("The Very Long"));
        assert!(shortened.ends_with("Substack"));
        assert!(shortened.contains('…'));
    }
}

#[cfg(test)]
mod sync_label_tests {
    use super::*;

    #[test]
    fn formats_sync_age() {
        let now = 1_000_000;
        assert_eq!(sync_age_at(now, now), "just now");
        assert_eq!(sync_age_at(now - 59, now), "just now");
        assert_eq!(sync_age_at(now - 60, now), "1m ago");
        assert_eq!(sync_age_at(now - 3_600, now), "1h ago");
        assert_eq!(sync_age_at(now - 86_400, now), "1d ago");
    }
}
