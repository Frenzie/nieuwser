# Nieuwser — Handoff Plan

A native FreshRSS desktop client in Rust. Linux is the primary target; Windows and macOS
must remain buildable. Slint is the UI toolkit for the prototype.

Status: M0/M1/M2 complete; M3 in progress (2026-10-18).

## Decisions

- **UI:** Slint. The UI must stay replaceable, so all business logic lives in `nieuwser-core`.
- **Article view:** sanitized native rich-text reader view, no embedded webview in v1.
- **Sync:** FreshRSS Google Reader API (`/api/greader.php`), not Fever.
- **Storage:** SQLite via `rusqlite`; metadata kept indefinitely, ~500 MB default budget for
  article bodies and media. Never evict unread/starred bodies.
- **Offline:** yes. Local changes are written to an outbox and replayed on the next sync.
- **Auth:** the settings dialog exchanges the password for a FreshRSS auth token via ClientLogin.
  Only the token is stored locally; the password is never persisted.
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
Keyboard-first: `j`/`↓` next article, `k`/`↑` previous, `m` toggle read, `s` toggle star,
`Esc` close settings. Mark-read-on-open is optional in Settings (default on).

## Milestones

- **M0 — scaffold/core:** workspace, API client, store, sync engine, CLI. *(done)*
- **M1 — read-only UI:** Slint three-pane shell backed by the store; refresh and article view. *(done)*
- **M2 — offline writes:** read/star/mark-all, outbox replay, background sync. *(done)*
- **M3 — polish:** keyboard, search, themes, settings/onboarding, cache budget. *(in progress)*
- **M4 — content:** reader document IR, image cache, feed icons.
- **M5 — hardening:** large accounts, retries, packaging.

## Current status

Working today:

- `nieuwser-core`: greader auth, CSRF token, subscriptions/tags/unread counts, stream
  contents, item IDs/contents, edit-tag, mark-all-as-read, incremental pagination, outbox replay,
  unread/starred reconciliation.
- SQLite store: feeds/folders/items/outbox/sync state, local read/star changes, queries.
- `nieuwser-cli`: `sync`, `feeds`, `items`, `mark-read`, `mark-all-read`.
- Slint app: three-pane shell, settings/onboarding dialog, stored auth token, background sync
  (startup plus every 15 minutes), last-sync display, optional mark-read-on-open, keyboard
  navigation, feed selection, article list, plain-text reader view, read/star/mark-all actions,
  manual refresh that preserves the open article.
- Tests: 10 core tests covering API parsing/auth, storage/migration, reconciliation, and a
  mocked end-to-end sync.
- Validated against a local FreshRSS dev server: 9 feeds, 157 items, 34 unread; repeat sync
  reports 0 new items; mark read/unread and star/unstar round-trip; onboarding, token reuse on
  restart, expired-token re-prompt, startup auto-sync, selection-preserving refresh, optional
  read-on-open, and keyboard navigation all work.

Next up:

1. Cache budget/eviction and image/feed-icon caching.
2. Structured reader document IR (headings, links, lists, images) instead of plain text.
3. Search in the desktop UI (the store/CLI already support it).
4. Themes and layout polish.


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
- Whether to add a webview for full HTML fidelity after v1.
- Cache budget semantics (500 MB) and eviction policy.
- Multi-account and feed management timing.
