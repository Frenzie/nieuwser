# Nieuwser — Handoff Plan

A native FreshRSS desktop client in Rust. Linux is the primary target; Windows and macOS
must remain buildable. Slint is the UI toolkit for the prototype.

## Decisions

- **UI:** Slint. The UI must stay replaceable, so all business logic lives in `nieuwser-core`.
- **Article view:** sanitized native rich-text reader view, no embedded webview in v1.
- **Sync:** FreshRSS Google Reader API (`/api/greader.php`), not Fever.
- **Storage:** SQLite via `rusqlite`; metadata kept indefinitely, ~500 MB default budget for
  article bodies and media. Never evict unread/starred bodies.
- **Offline:** yes. Local changes are written to an outbox and replayed on the next sync.
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

- `ot`/`nt` are seconds; `timestampUsec` is microseconds; `mark-all-as-read` `ts` is nanoseconds.
- Feeds belong to exactly one folder in FreshRSS.
- FreshRSS item IDs are numeric but may arrive as `tag:google.com,2005:reader/item/<id>`;
  use the last path segment for `edit-tag`.
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
Keyboard-first. Mark-read policy and themes come after the shell works.

## Milestones

- **M0 — scaffold/core:** workspace, API client, store, sync engine, CLI. *(in progress)*
- **M1 — read-only UI:** Slint three-pane shell backed by the store; refresh and article view.
- **M2 — offline writes:** read/star/mark-all, outbox replay, background sync.
- **M3 — polish:** keyboard, search, themes, settings/onboarding, cache budget.
- **M4 — content:** reader document IR, image cache, feed icons.
- **M5 — hardening:** large accounts, retries, packaging.

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

# UI (uses $NIEUWSER_DB or the XDG data dir)
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
