mod config;
mod daemon;
mod doh;
mod feed;
mod notify;
mod state;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use env_logger::Env;

fn main() {
    // Default to `info` when RUST_LOG is unset.
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    // Establish the notification identity first: if it can't be set, there's no
    // point running a notifier, so fail loudly and exit non-zero.
    if let Err(err) = notify::init() {
        log::error!("could not initialize notifications: {err:#}");
        std::process::exit(1);
    }

    // Load (or auto-create) the config. A malformed *existing* config is a
    // user-authored error we refuse to mask: log and exit non-zero.
    let config = match config::load_or_create() {
        Ok(config) => config,
        Err(err) => {
            log::error!("could not load config: {err:#}");
            std::process::exit(1);
        }
    };

    let seen_path = match config::seen_path() {
        Ok(path) => path,
        Err(err) => {
            log::error!("could not resolve seen-store path: {err:#}");
            std::process::exit(1);
        }
    };

    let mut seen = state::SeenStore::load(&seen_path);

    // Shared shutdown flag set by the signal handler (SIGINT + SIGTERM, the
    // latter via ctrlc's `termination` feature).
    let shutdown = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&shutdown);
    if let Err(err) = ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::Relaxed);
    }) {
        log::error!("could not install signal handler: {err:#}");
        std::process::exit(1);
    }

    log::info!(
        "status-notifications starting: {} feed(s), polling every {}s",
        config.feeds.len(),
        config.poll_interval_secs
    );

    daemon::run(&config, &mut seen, &seen_path, &shutdown);

    log::info!("status-notifications shut down cleanly");
}
