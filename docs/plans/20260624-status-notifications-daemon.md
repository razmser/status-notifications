# Status Notifications Daemon

## Overview
- A long-running, per-user macOS daemon (`status-notifications`) that polls configured Atom/RSS status-page feeds on a fixed interval and fires a **native macOS notification** for each new incident or incident update.
- Solves the problem of manually checking provider status pages (OpenAI, Claude, DeepSeek, …) — you get a banner the moment an incident opens, progresses, or resolves.
- Runs as a **LaunchAgent** (per-user GUI session) so notifications appear; managed entirely through a `Justfile` (build / check / install / uninstall / logs / run).

## Context (from discovery)
- Project dir `/Users/razmser/projects/status-notifications` is an empty git repo on branch `master` — greenfield, no existing code/conventions to match.
- All design decisions settled in a prior brainstorm (captured below); no re-litigation needed.
- Target platform is macOS only (uses `mac-notification-sys` + launchd).

## Development Approach
- **Testing approach**: Regular (code first, then tests within the same task).
- Complete each task fully before moving to the next; small, focused changes.
- **Every task with logic MUST include unit tests** (success + error/edge cases) as separate checklist items.
- **All tests must pass before starting the next task.**
- Pure/business logic (filter rule, status parsing, HTML strip, seen-store, config defaults) is factored to be testable **without** network or notification side effects.
- `cargo fmt` + `cargo clippy -D warnings` clean at every task boundary.

## Testing Strategy
- **Unit tests** (required per task):
  - status keyword extraction + HTML strip/whitespace-collapse
  - notification-eligibility filter (dedup `(id, updated)` + age window)
  - config default round-trip (serialize defaults → parse back)
  - seen-store save/load round-trip + age-based prune + atomic write
  - feed parsing against a small embedded Atom fixture string
- **No e2e/UI framework** — this is a headless daemon. Manual verification (real notification banner, launchd load) is listed under Post-Completion.

## Progress Tracking
- Mark completed items `[x]` immediately when done.
- New tasks get ➕ prefix; blockers get ⚠️ prefix.
- Keep this file in sync with actual work.

## Solution Overview
Single binary crate, synchronous and single-threaded (no async runtime). One poll loop iterates feeds sequentially; per-feed failures are isolated (logged, skipped — never crash). Notification eligibility is a pure function of the in-memory seen-set and a max-age window. State (the seen-set) is persisted atomically once per tick so restarts don't re-notify recent items, while the age window prevents notification storms on first run / after downtime.

**Key design decisions & rationale:**
- **Sync + `ureq`** over tokio/reqwest — workload is 3 GETs per minute; async adds a large dependency for zero benefit at this scale.
- **Dedup key = `(id, updated)`** — Statuspage/Instatus incidents are a single stable-`<id>` Atom entry whose `<updated>` bumps on each progress update; the pair makes each update notify exactly once.
- **Age window (`max_age_minutes`, default 10)** — notify only if `updated >= now − window`; kills restart/first-run storms without special-casing first run, and lets the seen-set be pruned to a tiny bounded size.
- **Single Application Support folder** for both config and state (macOS convention via `directories::ProjectDirs`).
- **Auto-create default config and keep running** — it's a daemon; no reason to force a restart.

## Technical Details

**Dependencies** (`Cargo.toml`): `ureq` **v3** (blocking HTTP; rustls is already the default backend — no OpenSSL; `http_status_as_error` defaults to `true` so non-2xx already surfaces as an error; timeout is configured via an `Agent`/`Config` builder using `timeout_global`, **not** a per-request `.timeout()` as in v2), `feed-rs` (parses Atom + RSS), `serde` (derive), `toml`, `serde_json`, `chrono` (serde feature), `mac-notification-sys` (requires a one-time `set_application(bundle_id)` before sending), `anyhow`, `log`, `env_logger`, `ctrlc` (with `termination` feature for SIGTERM), `directories`.

**Module layout** (`src/`):
- `main.rs` — CLI entry; init `env_logger` (default `info`); load/auto-create config; install signal handler; run daemon loop.
- `config.rs` — `Config`/`Feed` structs; `paths` (config dir, config file, state file) via `ProjectDirs`; `load_or_create()`.
- `feed.rs` — `fetch_and_parse(agent, feed) -> Result<Vec<Entry>>`: `ureq` GET on a shared `Agent` (global 10s timeout, real `User-Agent`; non-2xx already errors via `http_status_as_error`) → read body through a bounded reader (`.take(N)`, ~5 MB cap) → `feed-rs` parse → normalized `Entry`s; status parsing + HTML strip helpers.
- `state.rs` — `SeenKey(id, updated)`; `SeenStore` over `HashSet<SeenKey>`; `load`, `save` (atomic temp+rename), `prune(max_age)`, `contains`, `insert`.
- `notify.rs` — `init()` calling `set_application(...)` once (loud failure on error); `send(title, subtitle, body)` wrapping `mac-notification-sys::send_notification`.
- `daemon.rs` — `run(config, seen, shutdown)`: the poll loop; `is_eligible(entry, seen, now, max_age)` pure filter; `interruptible_sleep`.

**Data types:**
```rust
struct Config { poll_interval_secs: u64 /*60*/, max_age_minutes: i64 /*10*/, feeds: Vec<Feed> }
struct Feed   { name: String, url: String }
struct Entry  { id: String, updated: DateTime<Utc>, title: String, link: Option<String>, status: Option<String> }
```

**Notification eligibility (pure):** `!seen.contains((id,updated)) && updated >= now - Duration::minutes(max_age)`. On notify → `seen.insert(key)`. Save + prune once per tick after all feeds.

**Entry normalization:** `updated` = `<updated>` else `<published>`; if both missing, **skip** the entry (debug log — can't age-check). `link` = first link href. `status` = first keyword in `{Investigating, Identified, Monitoring, Resolved, Update, Postmortem}` found in the tag-stripped status text, else `None`. **Status text source precedence:** prefer `content.body`, fall back to `summary.content` (the keyword often lives only in `content` on Statuspage/Instatus feeds).

**Notification layout:** title = `feed.name`; subtitle = `entry.title`; body = `status` line (if any) then `link` on next line; if no status, just link; if neither, empty. Default sound.

**Paths:** `~/Library/Application Support/status-notifications/{config.toml, seen.json}`. `seen.json` = `{"seen":[{"id":"...","updated":"<RFC3339>"}]}`, atomic write (temp file in same dir + rename).

**Shutdown:** `ctrlc` handler sets `AtomicBool`; the poll loop checks the flag **between feeds** (so a hung fetch can't stretch shutdown to `feeds × timeout`) and `interruptible_sleep` polls it in ~500ms slices up to `poll_interval`; on shutdown, `save()` then exit cleanly.

**Malformed existing config:** if `config.toml` exists but fails to parse, **log the error and exit non-zero** (the file is user-authored — silently falling back to defaults would mask a typo). This differs from the seen-store, where a corrupt `seen.json` is tolerated and reset (it's machine-written, not user-authored).

## What Goes Where
- **Implementation Steps** (checkboxes): all crate code, tests, `Justfile`, plist template, README.
- **Post-Completion** (no checkboxes): real-banner verification, launchd load on the actual machine, macOS notification-permission prompt.

## Implementation Steps

### Task 1: Crate scaffolding & dependencies

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs` (temporary `fn main`)
- Create: `.gitignore`

- [ ] `cargo init --name status-notifications` (binary crate) in project root
- [ ] add all dependencies to `Cargo.toml` with appropriate features (`ureq` rustls, `chrono` serde, `serde` derive, `ctrlc` termination)
- [ ] add `.gitignore` for `/target`
- [ ] confirm `cargo build` succeeds with a stub `main`
- [ ] run `cargo build` — must succeed before next task

### Task 2: Config module (`config.rs`)

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (wire `mod config;`)

- [ ] define `Config` and `Feed` structs with serde derives and `#[serde(default)]` defaults (interval 60, age 10)
- [ ] implement `default_config()` returning the three default feeds (OpenAI, Claude, DeepSeek)
- [ ] implement path helpers via `directories::ProjectDirs` (config dir / `config.toml` / `seen.json`)
- [ ] implement `load_or_create()`: if `config.toml` missing, create dir + write serialized defaults (log info), then return defaults; if present but malformed → return an error (caller logs + exits non-zero, per Technical Details)
- [ ] write test: defaults round-trip (serialize default config to TOML → parse back → equals defaults)
- [ ] write test: parsing a minimal TOML (only `feeds`) applies interval/age defaults
- [ ] write test: parsing malformed TOML returns an error (not silent default fallback)
- [ ] run tests — must pass before next task

### Task 3: HTML strip & status parsing (`feed.rs` helpers)

**Files:**
- Create: `src/feed.rs`
- Modify: `src/main.rs` (wire `mod feed;`)

- [ ] implement `strip_html(&str) -> String` (drop `<...>` tags, decode a few common entities like `&amp;`/`&lt;`/`&gt;`/`&#39;`, collapse whitespace)
- [ ] implement `parse_status(&str) -> Option<String>` returning the first matching keyword from the ordered set, case-insensitive on a word boundary
- [ ] write test: `strip_html` removes tags and collapses whitespace on a sample HTML block
- [ ] write test: `parse_status` finds `Resolved`/`Monitoring`/etc. and returns `None` when absent
- [ ] write test: `parse_status` picks the FIRST status when multiple updates are present
- [ ] write test: an unrecognized HTML entity (e.g. numeric `&#9731;`) is left as-is (intentional, not accidental)
- [ ] run tests — must pass before next task

### Task 4: Feed fetch & entry normalization (`feed.rs`)

**Files:**
- Modify: `src/feed.rs`
- Create: `tests/fixtures/sample.atom`, `tests/fixtures/content_only.atom` (embedded via `include_str!`)

- [ ] define `Entry` struct
- [ ] implement `parse_feed(&str) -> Result<Vec<Entry>>` using `feed-rs`: map id, `updated` (fallback `published`, skip if both missing), title, first link href, and `status` from status text with **`content.body` preferred, falling back to `summary.content`**
- [ ] implement `fetch_and_parse(agent, feed) -> Result<Vec<Entry>>`: GET on a shared `ureq::Agent` (built once with `timeout_global` ~10s + real `User-Agent`); rely on `http_status_as_error` (default) for non-2xx; read body via a bounded reader (`.take(~5 MB)`) → `parse_feed`
- [ ] add a small real-shaped Atom fixture (one incident, one update) for tests
- [ ] add a `content_only.atom` fixture where the status keyword lives **only** in `<content>` (not `<summary>`)
- [ ] write test: `parse_feed` on the fixture yields expected id/title/updated/link/status
- [ ] write test: status is extracted from `content` when `summary` lacks it (uses `content_only.atom`)
- [ ] write test: entry with no `updated`/`published` is skipped
- [ ] write test: entry with `published` only uses it as `updated`
- [ ] run tests — must pass before next task

### Task 5: Seen-store with atomic persistence & prune (`state.rs`)

**Files:**
- Create: `src/state.rs`
- Modify: `src/main.rs` (wire `mod state;`)

- [ ] define `SeenKey { id: String, updated: DateTime<Utc> }` (serde) and `SeenStore` wrapping `HashSet<SeenKey>`
- [ ] implement `load(path)` (missing file → empty store; corrupt → log warn + empty), `save(path)` with atomic temp-file + rename in the same dir
- [ ] implement `contains`, `insert`, and `prune(now, max_age_minutes)` dropping keys older than the window
- [ ] write test: save → load round-trip preserves keys
- [ ] write test: `prune` removes only keys older than the window
- [ ] write test: `load` on missing/corrupt file returns empty store without error
- [ ] run tests — must pass before next task

### Task 6: Notification sender (`notify.rs`)

**Files:**
- Create: `src/notify.rs`
- Modify: `src/main.rs` (wire `mod notify;`)

- [ ] implement `init()` calling `set_application(&get_bundle_identifier_or_default("Script Editor"))` (or chosen bundle id) **once**; return its error to the caller so startup fails loudly if identity can't be set — this is NOT a per-send error to swallow
- [ ] implement `build_body(status: Option<&str>, link: Option<&str>) -> String` per layout rules (status line + link / link only / empty)
- [ ] implement `send(title, subtitle, body)` wrapping `send_notification` (default sound, `subtitle` as `Option`); log+swallow send errors so the loop never crashes
- [ ] write test: `build_body` covers all four combinations (both / status-only / link-only / neither)
- [ ] run tests — must pass before next task
- [ ] note: `init` and `send` against the OS are verified manually (Post-Completion); `set_application` failure surfaces in the Task 8 smoke run

### Task 7: Daemon loop, filter & graceful shutdown (`daemon.rs`)

**Files:**
- Create: `src/daemon.rs`
- Modify: `src/main.rs` (wire `mod daemon;`)

- [ ] implement pure `is_eligible(entry, seen, now, max_age_minutes) -> bool` (dedup pair + age window)
- [ ] implement `process_feed(agent, feed, &mut seen, now, max_age)`: fetch_and_parse (errors logged warn + skipped), for each eligible entry build+send notification then insert key
- [ ] implement `interruptible_sleep(total, &AtomicBool)` polling the flag in ~500ms slices
- [ ] implement `run(config, &mut seen, shutdown)`: build the shared `ureq::Agent`; loop feeds (checking the shutdown flag **between feeds**) → prune → save → interruptible_sleep; on shutdown flag, save and return
- [ ] write test: `is_eligible` true for fresh+recent, false when already seen, false when too old
- [ ] write test: a feed update with a new `updated` is eligible even though the same `id` was seen before
- [ ] run tests — must pass before next task

### Task 8: Wire `main.rs` end to end

**Files:**
- Modify: `src/main.rs`

- [ ] init `env_logger` (default filter `info` when `RUST_LOG` unset)
- [ ] call `notify::init()` early; on error, log and exit non-zero (no point running if notifications can't fire)
- [ ] `config::load_or_create()` (on malformed existing config: log + exit non-zero), `state::SeenStore::load(state_path)`
- [ ] install `ctrlc` handler setting a shared `Arc<AtomicBool>` (SIGINT + SIGTERM)
- [ ] call `daemon::run(...)`; log startup (feeds, interval) and clean-shutdown messages
- [ ] `cargo run` locally (foreground) and confirm it polls + persists `seen.json` without panicking
- [ ] run full `cargo test` — must pass before next task

### Task 9: Justfile

**Files:**
- Create: `Justfile`

- [ ] `build`: `cargo build --release`
- [ ] `check`: `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test`
- [ ] `run`: `RUST_LOG=debug cargo run`
- [ ] `install`: build release; copy binary to `~/.local/bin/`; render plist from template with absolute binary path; write to `~/Library/LaunchAgents/com.razmser.status-notifications.plist`; `launchctl unload` if present then `launchctl load`
- [ ] `uninstall`: `launchctl unload` + remove plist (keep config/state)
- [ ] `logs`: `tail -f ~/Library/Logs/status-notifications.log`
- [ ] verify `just check` passes end to end

### Task 10: LaunchAgent plist template

**Files:**
- Create: `contrib/com.razmser.status-notifications.plist` (template consumed by `just install`)

- [ ] author plist: `Label` `com.razmser.status-notifications`; `ProgramArguments` = installed binary path (templated); `RunAtLoad=true`; `KeepAlive=true`; `EnvironmentVariables` `RUST_LOG=info`; `StandardOutPath`/`StandardErrorPath` = `~/Library/Logs/status-notifications.log`
- [ ] ensure `just install` substitutes the absolute paths correctly (no `~` left unexpanded in the written plist)
- [ ] `plutil -lint` the rendered plist as part of install (or manually verify once)

### Task 11: Verify acceptance criteria
- [ ] verify all Overview requirements implemented (poll, dedup `(id,updated)`, age window, native notification with status+link, configurable feeds/interval/age, auto-create config, graceful shutdown)
- [ ] verify edge cases: dead feed isolated; entry with missing timestamps skipped; corrupt seen.json tolerated; first-run no storm
- [ ] run full test suite: `cargo test`
- [ ] run `just check` (fmt + clippy -D warnings + test) clean

### Task 12: [Final] Documentation
- [ ] write `README.md`: what it does, config location/format + example, install/uninstall via `just`, log location, notification-permission note
- [ ] update/create `CLAUDE.md` if any non-obvious conventions emerged
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion
*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Manual verification:**
- Run `just run` and confirm a real macOS notification banner appears (will trigger the one-time macOS permission prompt to allow notifications; approve it). Easiest trigger: temporarily set `max_age_minutes` very high so a current feed entry qualifies, or point a feed at a recently-active status page.
- Confirm notification shows feed name (title), incident title (subtitle), and status + link (body).
- `just install`, then confirm the agent is loaded: `launchctl list | grep status-notifications`, and that it survives logout/login.
- Confirm logs land in `~/Library/Logs/status-notifications.log`.
- `just uninstall` and confirm the agent unloads and the plist is removed (config/state retained).

**Environment notes:**
- macOS-only (launchd + `mac-notification-sys`). Binary must run inside the GUI session (LaunchAgent, not LaunchDaemon) for banners to appear.
- Notification "source app" identity is whatever bundle id `notify::init()` passes to `set_application` (defaulting to a borrowed system bundle like "Script Editor"); it may show generically without a signed `.app` bundle — acceptable for a personal tool.
