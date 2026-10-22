# Nieuwser

A native, offline-first desktop client for [FreshRSS](https://freshrss.org), written in Rust with Slint. Linux is the primary target; Windows and macOS may or may not work. (They build.)

All business logic lives in a UI-agnostic core, so the Slint frontend can be replaced without touching sync or storage.

## Features

- Three-pane reader: feed tree, article list, block-based reader view (no webview).
  Below 900 px it becomes a single-pane list/reader layout with a feed drawer. The article list
  can show all, unread, or read articles.
- Sync via the FreshRSS Google Reader API (`/api/greader.php`), including offline writes
  queued in an outbox and replayed on the next sync.
- Token-based auth: the password is exchanged for an auth token and never stored.
- Remembers the window size and position between launches (position is skipped on Wayland).
- SQLite cache with a ~500 MB budget for article bodies and media; unread/starred bodies are never evicted.
- Local search, light/dark themes, and keyboard-first navigation (`j`/`k` for the next/previous
  article, `r`/`f` for read/star, `↑`/`↓`/`PageUp`/`PageDown`/`Space`/`Home`/`End` to scroll the
  reader, `Esc` to close settings or the feed drawer / return to the article list in compact mode,
  `?` for shortcut help). In the compact reader, swipe up at the bottom or down at the top to
  switch articles with a live preview; swipe right to return to the list.
- Customizable command keys for next/previous, read, star, and open-in-browser via
  Settings → Keyboard shortcuts.
- Resizable panes in the wide layout (drag the dividers) and right-click menus for feeds and
  articles, including bulk read/star actions on multi-selected articles (`Ctrl`/`Shift`+click).

## Requirements

- Rust stable (edition 2024)
- On Linux: `pkg-config`, plus fontconfig and FreeType development headers
  (Debian/Ubuntu: `libfontconfig-dev libfreetype-dev`)
- A FreshRSS account and its API URL, e.g. `https://freshrss.example.net/api/greader.php`

## Layout

```
crates/nieuwser-core/   API client, models, SQLite store, sync engine, reader conversion
crates/nieuwser-cli/    headless sync/debug tool
apps/nieuwser/          Slint application
PLAN.md                 handoff plan, decisions, data model, and status
```

## Build and run

```sh
make build   # cargo build --release -p nieuwser

# Desktop app; first run opens the account dialog. Set NIEUWSER_DB to override the database path.
make run     # release build; use "make dev" for a debug build
```

Release builds are much faster; the debug build is only useful for quick iteration.

The CLI shares the same store and sync engine:

```sh
# Sync (password via env; avoid putting it in shell history)
FRESHRSS_PASSWORD=... cargo run -p nieuwser-cli -- \
  --base https://freshrss.example.net/api/greader.php \
  --email me@example.net --db ./nieuwser.db sync

cargo run -p nieuwser-cli -- --db ./nieuwser.db feeds
cargo run -p nieuwser-cli -- --db ./nieuwser.db items --stream all --limit 20
```

Environment variables `NIEUWSER_API_URL`, `NIEUWSER_EMAIL`, and `NIEUWSER_DB` are also accepted.

## Development

```sh
cargo test -p nieuwser-core   # core tests
cargo fmt --all --check
```

Current status and open work are tracked in [PLAN.md](PLAN.md).

## Releasing

The release workflow runs when a GitHub release is published, or manually with
`workflow_dispatch` and an optional version override. A published release uses its tag as
the version and gets the assets attached directly. A manual run with no existing release
creates a **draft** GitHub release instead.

Assets produced:

- Linux x86_64/aarch64: `.AppImage`, `.deb`, `.tar.gz`
- macOS universal: `.dmg` and a CLI `.zip`
- Windows x64: `.msi` and a portable `.zip`
- `SHA256SUMS.txt`

Builds are unsigned: the macOS app is ad-hoc signed only, and the Windows packages have no
code-signing certificate, so Gatekeeper/SmartScreen will warn. Review the draft release and
publish it manually. Local packaging scripts live in `packaging/`.

The Linux AppImage bundles the X11 keyboard libraries winit loads at runtime. Run it with
`--integrate` to install its desktop entry and icon into the user application menu.

## License

GPL-3.0-or-later (see `Cargo.toml`).
