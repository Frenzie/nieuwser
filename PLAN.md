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
5. **Pull:** `stream/contents/{stream}?output=json&n=500&ot=<seconds>&c=<continuation>`.
   `ot` is "newer than", in seconds; `nt` is only an upper bound. `continuation` anchors
   pagination. Initial sync caps at 5,000 items; later syncs page until exhausted.
6. **Reconcile:** fetch unread IDs (`stream/items/ids` + `xt=.../read`) and starred IDs, then
   update local state. Only reconcile when the fetched set is complete (not capped/truncated).
   Then fetch contents for missing unread/starred IDs via `stream/items/contents` in batches of
   50, capped at 1,000 per sync (starred first).

Gotchas:

- `ot`/`nt` are seconds and bound the crawl time: use `ot` for items newer than the last sync,
  `nt` only for upper-bound pagination. Store the server's `updated` time from `stream/contents`
  as the next `ot` minus a short overlap; the newest item timestamp is not a safe watermark.
  `timestampUsec` is microseconds; `mark-all-as-read` `ts` is also microseconds (FreshRSS
  compares it with microsecond entry IDs).
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
pulled from the server updates `items` without enqueueing. A permanent 4xx push failure is
dead-lettered so it cannot block later operations; the sync report shows the dead count.

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
bindings are stored in SQLite. Reader/navigation keys are fixed. In both layouts, three quick
presses of Space/`↓`/PageDown advance to the next article once the reader is at the bottom, and
Shift+Space/`↑`/PageUp go to the previous one at the top. A progress hint appears while the
gesture is counting; at the first/last article the presses fall back to scrolling. In the
compact reader, swipe up at the bottom or down at the top to switch to the next/previous article
with a live preview of the adjacent article, and swipe right returns to the article list.
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

## E-ink (assessed, deferred)

Status: not started; assessed on 2026-10-23 from the code, Slint 1.18.1 sources, FBInk, Plato,
and KOReader/koreader-base. No e-reader hardware, koxtoolchain, or cross targets are installed
here, so this is not a verified build.

Feasible: yes for Linux-based e-readers (Kobo, Kindle, PocketBook, Cervantes, reMarkable) with a
custom Slint platform. Android-based e-readers are the easier route: use the Android section and
let the OS handle refresh. "Swiftly" means low-latency page turns, not animations: partial e-ink
updates are roughly A2 ~120 ms, DU ~260 ms, and GL16/GC16 ~450 ms; full flashes take seconds.

Already suitable:

- `nieuwser-core` is UI-agnostic, native Rust, and has no browser/webview dependency. The compact
  single-pane layout already fits 6–8" screens.
- Slint 1.18.1 has a software renderer with dirty-region output, RGB565/RGBA targets, and a
  `/dev/fb0` display path (`i-slint-backend-linuxkms`, `SLINT_BACKEND_LINUXFB`, software renderer,
  `SLINT_SCALE_FACTOR`). Our headless UI test already drives the UI through
  `MinimalSoftwareWindow`.
- FBInk handles Kobo/Kindle/PocketBook/Cervantes/reMarkable framebuffer quirks and exposes
  `fbink_refresh_rect` with DU/A2/GL16/GC16 waveform modes plus input scanning; Rust bindings
  exist (`fbink-sys`, `fbink-rs`). FBInk is GPLv3+, compatible with this GPL-3.0-or-later app.
- Plato is a native Rust Kobo reader with its own framebuffer/input modules and update modes;
  KOReader/koreader-base document the device matrix and waveform tuning.

Blockers and required work:

1. **E-ink refresh.** Slint's LinuxFB path only memcpy's into `/dev/fb0`; it never issues the
   MXCFB/EPDC update ioctls, so Kobo/Kindle panels will not change. Take the dirty region returned
   by `SoftwareRenderer::render`, blit it, and call FBInk (or the device ioctls) with the right
   waveform. Prefer partial updates and run a periodic full GC16 to clear ghosting.
2. **Input.** The LinuxKMS backend's libinput path uses udev (`new_with_udev`, seat0), which
   e-readers do not run. Add evdev input (or libinput `new_from_path`), identify the touchscreen
   with FBInk's `fbink_input_scan`, handle rotation/coordinate quirks, and map hardware page
   buttons.
3. **UI profile.** A 6" Kobo is ~1072 px wide at scale 1, so the `<900 px` compact breakpoint does
   not fire; force compact mode. Add a DPI/scale pass (15 px text is tiny at 300 dpi), larger touch
   targets, no hover/double-click dependence, and an on-screen keyboard for account setup (or
   provision the token via the CLI/config). Hide the shortcut editor.
4. **E-ink behavior.** Disable animations and live swipe previews, use page-based reader movement,
   force a high-contrast light palette, and cap memory: the image cache can reach ~190 MB worst
   case and the reader cache is desktop-sized. Use Gray8/RGB565 targets, skip Noto Emoji, and
   bundle a text font (fontique otherwise links fontconfig).
5. **Build/runtime.** Gate `arboard` (X11 clipboard) and `open_external`; bundle CA roots
   (`webpki-roots`) because e-readers lack a platform store; test SQLite WAL on FAT32 (or put the
   DB on ext4) and keep the rollback-journal fallback; there is no window manager, clipboard, or
   browser.
6. **Deployment.** Kobo: `/mnt/onboard/.adds/` + KFMon/NickelMenu (which stops Nickel); Kindle:
   `/mnt/us` + KUAL; PocketBook has InkView quirks. Sync on launch; the 15-minute timer only runs
   while the app is open.

### Cross-compiling with koxtoolchain

koxtoolchain is not Rust: it builds GCC/C toolchains with crosstool-NG. Rust supplies the target
std; the toolchain supplies the linker, sysroot, and C compiler for bundled C dependencies
(SQLite, FBInk, TLS). The e-ink platform backend must exist first; cross-compiling the current
winit/X11 build will link libraries the device does not have.

Kobo (ARMv7 hard-float), following Plato's proven settings:

```sh
git clone https://github.com/koreader/koxtoolchain
cd koxtoolchain
./gen-tc.sh kobo
source refs/x-compile.sh kobo env bare   # adds ~/x-tools/arm-kobo-linux-gnueabihf/bin to PATH
rustup target add arm-unknown-linux-gnueabihf
export CC_arm_unknown_linux_gnueabihf=arm-kobo-linux-gnueabihf-gcc
export AR_arm_unknown_linux_gnueabihf=arm-kobo-linux-gnueabihf-ar
export CXX_arm_unknown_linux_gnueabihf=arm-kobo-linux-gnueabihf-g++
cargo build --release -p nieuwser --target arm-unknown-linux-gnueabihf
```

Workspace `.cargo/config.toml`:

```toml
[target.arm-unknown-linux-gnueabihf]
linker = "arm-kobo-linux-gnueabihf-gcc"
rustflags = ["-C", "target-feature=+v7,+vfp3,+neon"]
```

Other families use their own TC prefixes: `kobov4`/`kobov5` →
`arm-kobov4-linux-gnueabihf`/`arm-kobov5-linux-gnueabihf`; `kindlepw2` →
`arm-kindlepw2-linux-gnueabi` (soft-float, so match it with a soft-float Rust target);
`kindlehf` → `arm-kindlehf-linux-gnueabihf`; PocketBook and Cervantes have their own. Use `file`
and `readelf -A`/`readelf -V` to verify the ABI and GLIBC symbols. Rust's prebuilt std needs
glibc >= 2.17, so old Kindles may need a musl target or a std built with the TC sysroot
(`-Z build-std`, nightly).

Cargo snags: `libsqlite3-sys` bundled uses the `cc` crate (the `CC_<target>` variables above);
`reqwest`'s default rustls provider is aws-lc-rs, which may need extra CMake/cross setup—prefer
`rustls-no-provider` + `rustls`/ring if it fights back. Build FBInk as a static library with the
same TC (`make static` with the env sourced, plus the right `KOBO`/`KINDLE` variable) and link it
from a small `-sys` crate. Bundle CA roots. Test the binary with `readelf` before copying it to
the device.

Effort estimate: a weekend proves the framebuffer/refresh/input pipeline on one Kobo; a pleasant
Kobo build is a few weeks; Kindle/PocketBook and polish are longer. Android e-readers are the
shortest path if that device class is acceptable.

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
- Tests: 24 core tests plus 5 app tests, covering API parsing/auth, reader blocks,
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

Recent changes (2026-10-23):

- Made remote-ID reconciliation use indexed timestamp matches, skipped FTS reindexing for
  unchanged items, bounded adjacent-preview parsing, and throttled cache-budget checks after
  image fetches.
- Extended triple-press article navigation to the regular layout and to `↑`/`↓`/PageUp/PageDown,
  gated at the reader's top/bottom like the swipe gesture, with a progress hint.
- Assessed an e-ink port (custom Slint framebuffer backend, FBInk refresh, evdev input); see the
  E-ink section.
- Fixed compact-reader touch scrolling: the swipe handler pans the reader with relative drag
  deltas and only shows article previews at the top/bottom edges, so mid-article drags no longer
  jump or stall.

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
- E-ink support: assessed as feasible but deferred; see the E-ink section for blockers and the
  koxtoolchain cross-compile sketch. Decide between a native Kobo build and Android e-readers
  after M5.
