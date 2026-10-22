# Nieuwser — Handoff Plan

A native FreshRSS desktop client in Rust. Linux is the primary target; Windows and macOS
must remain buildable. Slint is the UI toolkit for the prototype.

Status: M0–M4 complete; M5 substantially complete (2026-10-21).

## Decisions

- **UI:** Slint. The UI must stay replaceable, so all business logic lives in `nieuwser-core`.
- **Article view:** block-level document IR (headings, paragraphs, lists, quotes, code, rules,
  tables, images) rendered with native Slint widgets; inline links open externally. The reader is
  a virtualized Slint `ListView`, with the title/meta as model rows so off-screen blocks are not
  laid out. A skeleton covers loading, and Noto Emoji is bundled for consistent/fast emoji
  fallback. No embedded webview or Blitz.
- **Search:** SQLite FTS5 over title, author, summary, and content, using prefix terms with a
  feed-title fallback. Existing rows are backfilled once on migration.
- **Network safety:** HTTP response bodies are bounded. Remote images reject private, loopback,
  link-local, and metadata addresses; resolve and pin public addresses; do not follow redirects;
  enforce byte/pixel limits before decoding; and cap concurrent fetches.
- **Sync:** FreshRSS Google Reader API (`/api/greader.php`), not Fever.
- **Storage:** SQLite via `rusqlite`; metadata kept indefinitely, ~500 MB default budget for
  article bodies and media. Never evict unread/starred bodies. Oldest read/unstarred bodies are
  cleared after sync; a cleared body is fetched again when the article is opened.
- **Offline:** yes. Local changes are written to an outbox and replayed on the next sync.
- **Auth:** the settings dialog exchanges the password for a FreshRSS auth token via ClientLogin.
  Only the token is stored locally; the password is never persisted.
- **Release:** publishing a GitHub release, or running the Release workflow manually, builds
  Linux, macOS, and Windows packages and creates/updates a draft release with `SHA256SUMS`.
- **Scope:** single account, no feed management in the initial PoC.
- **Figma:** Slint's one-way "Figma to Slint" importer; imported screens go in `ui/imported/`
  and are not hand-edited.

## Repo layout

```
crates/nieuwser-core/   API client, models, SQLite store, sync engine, reader conversion
crates/nieuwser-cli/    headless sync/debug tool
apps/nieuwser/          Slint application
PLAN.md                 this file
```

The core crate has no UI dependencies. The CLI and UI both use the same store and sync engine.

## FreshRSS sync

Base URL is the server's API address, e.g. `https://freshrss.example.net/api/greader.php`.

1. **Auth:** `POST /accounts/ClientLogin` with `Email` and the *API password*. Response is plain
   text; use the `Auth=` line as `Authorization: GoogleLogin auth=<Auth>`.
2. **CSRF:** `GET /reader/api/0/token`; send the result as `T=` on writes.
3. **Metadata:** `subscription/list?output=json`, `tag/list?output=json`,
   `unread-count?output=json`. `output=json` is mandatory.
4. **Push before pull:** replay `outbox` via `edit-tag` (repeated `i=`, `a=`, `r=`) and
   `mark-all-as-read`.
5. **Pull:** `stream/contents/{stream}?output=json&n=100&nt=<seconds>&c=<continuation>`.
   `nt` is "newer than", in seconds. `continuation` anchors pagination.
6. **Reconcile:** fetch unread IDs (`stream/items/ids` + `xt=.../read`) and starred IDs, then
   update local state. Only reconcile when the fetched set is complete (not capped/truncated).

Gotchas:

- `ot`/`nt` are seconds and bound the crawl time: use `ot` for items newer than the last sync,
  `nt` only for upper-bound pagination. `timestampUsec` is microseconds; `mark-all-as-read` `ts`
  is nanoseconds.
- Feeds belong to exactly one folder in FreshRSS.
- IDs come in two forms: `stream/contents` returns
  `tag:google.com,2005:reader/item/<16-hex>`, while `stream/items/ids` returns the same ID as a
  decimal. Use the hex suffix for local IDs and `edit-tag`; match decimal IDs for reconciliation.
- Always send `Content-Type: application/x-www-form-urlencoded` on writes.

## Data model

- `account` (single row): base URL, email.
- `folders`: FreshRSS label stream IDs and names.
- `feeds`: `feed/<id>`, title, URLs, icon, folder, unread count.
- `items`: raw ID, feed, title, author, timestamps, URLs, summary/content HTML, read/starred.
- `outbox`: pending read/unread/star/unstar/mark-all-read operations.
- `sync_state`: watermarks and timestamps.

Local state changes update `items` and enqueue an outbox row in one transaction. Remote state
pulled from the server updates `items` without enqueueing.

## UI

Adaptive three-pane layout: feed tree with unread counts, article list, reader pane.
Below 900 px the same panes switch to a single-pane layout: the article list and reader fill
the window, the feed tree becomes a drawer opened from the list header, and the reader gets a
back button. Escape closes the drawer or returns from the reader to the list. The article list
loads 300 items at a time with a "Load more" button; empty states and a minimum window size keep
the layout usable. The list can be filtered to all, unread-only, or read-only articles, and the
chosen filter is remembered. The list reconciles rows by article ID instead of replacing the
model, preserving scroll/selection across refreshes; the active row is kept in view when moving
with `j`/`k`. Feed unread counts are right-aligned in a fixed-width column. Window size and
position are stored in SQLite and restored on launch; Wayland position saves are skipped
because that compositor does not expose them.

Reader blocks use native `StyledText` with inline emphasis, code, and clickable links. The reader
is a virtualized `ListView`; title/meta rows are part of the model, and a skeleton covers the
loading gap. Noto Emoji is bundled and registered as Slint's emoji fallback. Article images and
feed icons are fetched on demand, cached in SQLite, rendered natively, and loaded with bounded
concurrency; inline images are promoted out of paragraphs.

Keyboard-first: `j` next article, `k` previous, `↑`/`↓` scroll the reader, `r` toggle read,
`f` toggle star, `o`/`Ctrl+O` open the selected article(s) in the browser, `Esc` close settings,
close the compact feed drawer, or return from the compact reader to the article list, and `?`
opens the shortcut help. The five command keys are editable from Settings → Keyboard shortcuts;
bindings are stored in SQLite. Reader/navigation keys are fixed. In the compact reader, swipe up
at the bottom or down at the top to switch to the next/previous article with a live preview of
the adjacent article, swipe right returns to the article list, and three quick Space presses
advance to the next article.
Double-clicking or middle-clicking an article row opens it in the browser; the reader toolbar
also has a globe button for the selected article.
Mark-read-on-open is optional in Settings (default on) for both the reader and "Open in browser".
Light/dark theme is stored locally and also switches the standard Slint widget palette.

## Release

`.github/workflows/release.yml` runs on a published GitHub release or manual dispatch. It builds:

- Linux x86_64/aarch64: AppImage, `.deb`, `.tar.gz`
- macOS universal: `.dmg` and CLI `.zip` (ad-hoc signed)
- Windows x64: `.msi` and portable `.zip` (unsigned)
- `SHA256SUMS.txt` on a draft GitHub release

Linux packaging bundles the dynamically loaded xkbcommon libraries, supports
`AppImage --integrate`, and produces reproducible `.tar.gz`/`.deb` files. CI smoke-tests the
AppImage under Xvfb, runs clippy, formatting, and the full test suite. macOS/Windows packaging
paths exist but have not yet been exercised by a GitHub release run.

## Android (assessed, deferred)

Status: not started; deferred on 2026-10-22. No Android SDK/NDK or Rust Android targets are
installed in the dev environment, so this is an assessment from the code and Slint 1.18.1
documentation, not a verified build.

Feasible: Slint 1.18.1 supports Android through `backend-android-activity-06`. The app must be a
`cdylib` exporting `android_main`, call `slint::android::init(app)`, target API 26+, and use the
Skia renderer (the only Android renderer). `cargo apk` is the shortest build path.

Already suitable:

- `nieuwser-core` is UI-agnostic with no Linux-only dependencies. Bundled `rusqlite` keeps FTS5
  available; `reqwest`/rustls already pulls the Android platform verifier; `tokio`, `image`, and
  `scraper` cross-compile.
- The compact single-pane layout already activates below 900 px.

Blockers and required work:

1. **`arboard` does not compile for Android.** It is only used for "copy selected links"
   (`apps/nieuwser/src/main.rs:1928`). Make it non-Android and disable that action initially, or
   add a JNI `ClipboardManager` helper.
2. **Binary-only entry point.** `apps/nieuwser/src/main.rs:62` owns `main`. Add a library target
   (`crate-type = ["cdylib", "rlib"]`), move startup into a shared `run()`, and export
   `#[unsafe(no_mangle)] android_main` that calls `slint::android::init`.
3. **Database path.** `directories::ProjectDirs` returns `None` on Android, leaving an unwritable
   relative fallback (`apps/nieuwser/src/main.rs:1999`). Pass
   `AndroidApp::internal_data_path()` into the app instead.
4. **External links.** `open_external` spawns `xdg-open`/`open`/`cmd`
   (`apps/nieuwser/src/main.rs:1584`); Android needs a JNI `ACTION_VIEW` intent.
5. **Window constraints.** `min-width: 560px; min-height: 480px`
   (`apps/nieuwser/ui/app.slint:442`) exceeds phone dimensions. Lower them or make them
   desktop-only.
6. **Manifest/config:** `INTERNET` permission, app ID, icon, and a cleartext-HTTP policy for
   self-hosted FreshRSS servers (HTTPS needs none).
7. **Background sync.** The 15-minute `slint::Timer` only runs while the activity is alive. Sync
   on launch/resume first; real background sync needs WorkManager or a foreground service.

Resume checklist: install the `aarch64-linux-android` (and optionally `x86_64-linux-android`)
targets plus NDK/SDK/JDK; add the Slint feature and library target; gate `arboard`; pass the
Android data path; add the link-opening JNI shim; relax the size constraints; build an APK; then
add an Android job to CI.

Effort estimate: a launching APK with the current UI is roughly half a day to a day once the SDK
is installed; a usable mobile build is a few days; production background sync, signing, lifecycle
handling, and device testing are materially more.

## Milestones

- **M0 — scaffold/core:** workspace, API client, store, sync engine, CLI. *(done)*
- **M1 — read-only UI:** Slint three-pane shell backed by the store; refresh and article view. *(done)*
- **M2 — offline writes:** read/star/mark-all, outbox replay, background sync. *(done)*
- **M3 — polish:** keyboard, search, themes, settings/onboarding, cache budget. *(done)*
- **M4 — content:** reader document IR, image cache, feed icons. *(done)*
- **M5 — hardening:** large accounts, retries, packaging. *(packaging, search, security, and
  reader performance done; stress/retry validation remains)*

## Current status

Working today:

- `nieuwser-core`: greader auth, CSRF token, subscriptions/tags/unread counts, stream
  contents, item IDs/contents, edit-tag, mark-all-as-read, incremental pagination, outbox replay,
  unread/starred reconciliation, and bounded HTTP response reads.
- SQLite store: feeds/folders/items/outbox/sync state, local read/star changes, queries,
  cached media, a shared byte budget for bodies and media, and an FTS5 article index.
- `nieuwser-cli`: `sync`, `feeds`, `items`, `mark-read`, `mark-all-read`.
- Slint app: three-pane shell with a compact single-pane layout and feed drawer,
  settings/onboarding dialog, stored auth token, background sync
  (startup plus every 15 minutes), last-sync display, optional mark-read-on-open, keyboard
  navigation, local search, all/unread/read article filters, feed selection, paginated article
  list, virtualized rich reader, loading skeleton, bundled emoji fallback, cached feed icons and
  article images, persisted light/dark theme, restored window geometry, editable shortcuts and
  shortcut help, read/star/mark-all actions, manual refresh that preserves the open article, and
  browser opening from the reader toolbar, keyboard, double-click, and middle-click.
- Tests: 24 core tests plus 3 app tests, covering API parsing/auth, reader blocks,
  storage/migration, FTS search/backfill, read filtering, reconciliation, cache eviction, image
  URL safety, HTTP body limits, a mocked end-to-end sync, shortcut validation, article-list
  reveal, and long-reader rendering.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings`
  pass. GitHub CI runs the test matrix, formatting, and clippy.
- Validated against a local FreshRSS dev server: 9 feeds, 157 items, 34 unread; repeat sync
  reports 0 new items; mark read/unread and star/unstar round-trip; onboarding, token reuse on
  restart, expired-token re-prompt, startup auto-sync, selection-preserving refresh, optional
  read-on-open, keyboard navigation, local search, on-demand article restore after cache
  eviction, rich rendering of headings, lists, links, tables, images, and cached feed icons.
- Linux packaging validated locally: AppImage, `.deb`, and `.tar.gz` build; the AppImage bundles
  xkbcommon and supports desktop integration; repeated `.tar.gz`/`.deb` builds are byte-identical.

Recent changes (2026-10-22):

- Replaced the all-at-once reader with a virtualized `ListView`, added a loading skeleton, and
  registered bundled Noto Emoji as the fallback font.
- Added FTS5 search with one-time indexing of existing articles.
- Bounded HTTP bodies and hardened remote image fetching against SSRF, redirects, oversized
  images, and decode bombs; capped concurrent image fetches.
- Preserved article-list identity across refreshes, kept the active row visible on `j`/`k`, and
  right-aligned feed unread counts.
- Added the browser icon/button, `o`/`Ctrl+O`, and double/middle-click article opening.
- Added a compact single-pane layout below 900 px, with a feed drawer, reader back button, and
  Escape navigation.
- Added shortcut help (`?`) and editable single-key command bindings stored in SQLite.
- Added remembered window size and position.
- Added all/unread/read article filters and compact-reader vertical swipe pagination with adjacent
  article previews, plus right-swipe back to the list and triple-Space next navigation.
- Added the release workflow, reproducible Linux packaging, AppImage integration/smoke test,
  and clippy in CI.

Next up:

1. Finish M5: large-account/soak testing, retry/offline edge cases, and an end-to-end release run.
2. Exercise macOS and Windows packaging in CI.
3. Platform-native styling, fonts, and menu integration.


## How to run

```sh
# build everything
cargo build

# sync against a server (password via env; do not put it in shell history)
FRESHRSS_PASSWORD=... cargo run -p nieuwser-cli -- \
  --base https://freshrss.example.net/api/greader.php \
  --email me@example.net --db ./nieuwser.db sync

# inspect
cargo run -p nieuwser-cli -- --db ./nieuwser.db feeds
cargo run -p nieuwser-cli -- --db ./nieuwser.db items --stream all --limit 20

# UI; first run opens the account dialog. Set NIEUWSER_DB to override the data directory.
cargo run -p nieuwser
```

## Conventions

- Keep functions straightforward; do not split every step into a helper.
- Comments only where the reason is not obvious.
- Core code must build on Linux, Windows, and macOS; no Linux-only dependencies.
- Commit in small, working increments.

## Open questions

- Final app name and license.
- Whether to add an optional Blitz/HTML renderer for full fidelity after v1; the block reader
  remains the default and Blitz is explicitly out of scope for now.
- Multi-account and feed management timing.
- Android support: assessed as feasible but deferred; see the Android section for blockers and a
  resume checklist. Decide whether to pursue it after M5.
