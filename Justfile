# status-notifications — task runner
#
# The `install` recipe renders the LaunchAgent plist from the template
# `contrib/com.razmser.status-notifications.plist`. The template uses these
# placeholders (Task 10 authors the template to match):
#   __BINARY__  -> absolute path to the installed binary
#                  ($HOME/.local/bin/status-notifications)
#   __LOG__     -> absolute path to the log file
#                  ($HOME/Library/Logs/status-notifications.log)

set shell := ["bash", "-uc"]

label := "com.razmser.status-notifications"

# Default: list available recipes.
default:
    @just --list

# Build the release binary.
build:
    cargo build --release

# Format check + clippy (deny warnings) + tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Run in the foreground with debug logging.
run:
    RUST_LOG=debug cargo run

# Build, install the binary, render + load the LaunchAgent.
install:
    #!/usr/bin/env bash
    set -euo pipefail

    template="contrib/{{label}}.plist"
    if [[ ! -f "$template" ]]; then
        echo "error: plist template not found: $template" >&2
        exit 1
    fi

    cargo build --release

    bin_dir="$HOME/.local/bin"
    binary="$bin_dir/status-notifications"
    mkdir -p "$bin_dir"
    cp "target/release/status-notifications" "$binary"

    log_dir="$HOME/Library/Logs"
    log_file="$log_dir/status-notifications.log"
    mkdir -p "$log_dir"

    agents_dir="$HOME/Library/LaunchAgents"
    plist="$agents_dir/{{label}}.plist"
    mkdir -p "$agents_dir"

    sed -e "s|__BINARY__|$binary|g" \
        -e "s|__LOG__|$log_file|g" \
        "$template" > "$plist"

    launchctl unload "$plist" 2>/dev/null || true
    launchctl load "$plist"
    echo "installed: $binary"
    echo "loaded:    $plist"

# Unload + remove the LaunchAgent (config/state retained).
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail

    plist="$HOME/Library/LaunchAgents/{{label}}.plist"
    launchctl unload "$plist" 2>/dev/null || true
    rm -f "$plist"
    echo "removed: $plist"
    echo "note: config and state are retained at:"
    echo "  $HOME/Library/Application Support/status-notifications/"

# Follow the daemon log.
logs:
    tail -f "$HOME/Library/Logs/status-notifications.log"
