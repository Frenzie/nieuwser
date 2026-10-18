# Nieuwser

A native, offline-first desktop client for [FreshRSS](https://freshrss.org), written in Rust with Slint. Linux is the primary target; Windows and macOS may or may not work. (They build.)

All business logic lives in a UI-agnostic core, so the Slint frontend can be replaced without touching sync or storage.

## Features

- Three-pane reader: feed tree, article list, block-based reader view (no webview).
- Sync via the FreshRSS Google Reader API (`/api/greader.php`), including offline writes
  queued in an outbox and replayed on the next sync.
- Token-based auth: the password is exchanged for an auth token and never stored.
- SQLite cache with a ~500 MB budget for article bodies and media; unread/starred bodies are never evicted.
- Local search, light/dark themes, and keyboard-first navigation (`j`/`k` for the next/previous
  article, `↑`/`↓`/`PageUp`/`PageDown`/`Space`/`Home`/`End` to scroll the reader).
- Resizable panes (drag the dividers) and right-click menus for feeds and articles,
  including bulk read/star actions on multi-selected articles (`Ctrl`/`Shift`+click).

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

The release workflow runs on a push to the `release` branch, or manually with
`workflow_dispatch` and an optional version override. It reads the version from
`[workspace.package] version` in `Cargo.toml` and creates a **draft** GitHub release.

Assets produced:

- Linux x86_64/aarch64: `.AppImage`, `.deb`, `.tar.gz`
- macOS universal: `.dmg` and a CLI `.zip`
- Windows x64: `.msi` and a portable `.zip`
- `SHA256SUMS.txt`

Builds are unsigned: the macOS app is ad-hoc signed only, and the Windows packages have no
code-signing certificate, so Gatekeeper/SmartScreen will warn. Review the draft release and
publish it manually. Local packaging scripts live in `packaging/`.

## License

GPL-3.0-or-later (see `Cargo.toml`).
