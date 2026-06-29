# status-notifications

A small macOS daemon that polls Atom/RSS status-page feeds (OpenAI, Claude, and
DeepSeek by default) and shows a native macOS notification for each new incident
or incident update. Instead of manually checking provider status pages, you get
a banner the moment an incident opens, progresses, or resolves.

## How it works

- Polls every configured feed once per `poll_interval_secs` (default 60s), one
  feed after another. A failure on one feed is logged and skipped — it never
  crashes the daemon or blocks the others.
- For each feed entry it builds a dedup key from the pair `(id, updated)`.
  Status-page incidents are a single stable-`id` entry whose `updated` timestamp
  bumps on every progress update, so each distinct update notifies exactly once.
- It only notifies for entries whose `updated` time is within the last
  `max_age_minutes` (default 10). This window prevents a notification storm on
  first run or after the machine has been asleep, without special-casing
  startup.
- A small "seen" state is persisted after each poll so a restart doesn't
  re-notify recent items.
- Runs as a per-user **LaunchAgent** (inside your GUI login session, which is
  required for banners to appear).

A notification shows the feed name as the title, the incident title as the
subtitle, and in the body the status keyword (e.g. *Investigating*,
*Monitoring*, *Resolved*) together with the latest update's message text plus a
link.

## Requirements

- macOS (the daemon uses launchd and `mac-notification-sys`; it is macOS-only).
- A Rust toolchain (`cargo`), e.g. via [rustup](https://rustup.rs).
- [`just`](https://github.com/casey/just) — optional but recommended; it drives
  build, install, uninstall, and log commands. You can run the equivalent
  `cargo` commands by hand if you prefer.

## Configuration

Config lives at:

```
~/Library/Application Support/status-notifications/config.toml
```

It is **created automatically with defaults on first run** if it doesn't exist.
The default file looks like this:

```toml
poll_interval_secs = 60
max_age_minutes = 10
tls_emulation = "chrome_137"

[[feeds]]
name = "OpenAI"
url = "https://status.openai.com/feed.atom"

[[feeds]]
name = "Claude"
url = "https://status.claude.com/history.atom"

[[feeds]]
name = "DeepSeek"
url = "https://status.deepseek.com/feed.atom"
```

### Fields

- `poll_interval_secs` (default `60`) — how often, in seconds, every feed is
  polled. Must be `>= 1`.
- `max_age_minutes` (default `10`) — only entries updated within this many
  minutes are eligible to notify. Larger values mean older incidents can still
  fire a banner (useful for testing); smaller values keep things tight. Must be
  between `1` and `525600` (one year).
- `tls_emulation` (default `"chrome_137"`) — the browser TLS/HTTP2 fingerprint
  used for all feed requests. Some status hosts sit behind middleboxes that
  **reset connections whose TLS handshake doesn't look like a real browser's**
  (observed with `status.deepseek.com`), so the daemon emulates a recent Chrome
  by default. Override with any [`wreq` emulation
  name](https://docs.rs/wreq-util/latest/wreq_util/enum.Emulation.html), e.g.
  `tls_emulation = "safari_17.0"` or `"firefox_136"`. An unrecognized name fails
  to parse (the daemon logs the error and exits), like any other bad key.
- `[[feeds]]` — one block per feed, each with:
  - `name` — shown as the notification title.
  - `url` — the Atom or RSS feed URL of the status page.

`poll_interval_secs`, `max_age_minutes`, and `tls_emulation` are all optional and
fall back to their defaults if omitted; only `feeds` really needs to be present.

### Adding or removing feeds

To track another provider, add a `[[feeds]]` block with its status-page
Atom/RSS URL:

```toml
[[feeds]]
name = "GitHub"
url = "https://www.githubstatus.com/history.atom"
```

To stop tracking one, delete its block. Restart the daemon after editing the
config (`just uninstall` then `just install`, or relaunch the foreground
process) so the changes take effect.

If `config.toml` exists but fails to parse — malformed TOML, an unknown or
misspelled key (e.g. `max_age_minute` instead of `max_age_minutes`), or a value
outside the valid ranges above — the daemon logs the error and exits non-zero
rather than silently falling back to defaults. Fix the typo and start it again.

### Log verbosity

There is no command-line interface: the binary is configured entirely through
`config.toml` and the `RUST_LOG` environment variable. Set `RUST_LOG` to control
log level, e.g. `RUST_LOG=debug` for verbose output (the default is `info`). The
installed LaunchAgent runs with `RUST_LOG=info`.

## Build, install, and run

Using `just` (run from the repo root):

- `just build` — build the release binary (`cargo build --release`).
- `just check` — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  and the test suite. Use this to verify a clean tree.
- `just run` — run in the **foreground** with debug logging, for testing. Stop
  with Ctrl-C; it shuts down cleanly and saves state.
- `just install` — build the release binary, copy it to `~/.local/bin/`, render
  the LaunchAgent plist from the template with absolute paths, write it to
  `~/Library/LaunchAgents/com.razmser.status-notifications.plist`, and load it
  with launchctl. The agent runs at login and is kept alive.
- `just uninstall` — unload and remove the LaunchAgent. Your config and state
  are retained.
- `just logs` — `tail -f` the daemon log.

## Logs

The installed daemon writes stdout and stderr to:

```
~/Library/Logs/status-notifications.log
```

Follow it live with `just logs`.

## Notification permission

The first time the daemon fires a notification, macOS prompts you to allow
notifications. Approve it (in System Settings under Notifications if you miss the
prompt).

Because this is a personal tool without its own signed `.app` bundle, it borrows
a system bundle identity (Script Editor) so notifications have a valid source.
As a result the permission prompt and the banners appear **under that borrowed
identity** (e.g. "Script Editor") rather than under "status-notifications" —
this is expected.

## State

The "seen" state is stored at:

```
~/Library/Application Support/status-notifications/seen.json
```

It is written automatically (atomically) after each poll and on shutdown. Don't
edit it by hand. If it ever becomes corrupt, the daemon tolerates it, logs a
warning, and resets to an empty state.
