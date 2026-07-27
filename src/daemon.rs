//! The poll loop and its supporting pieces.
//!
//! [`is_eligible`] is a pure filter (dedup `(id, updated)` pair + age window) so
//! it can be unit-tested without any network or notification side effects.
//! [`process_feed`] fetches one feed and notifies on eligible entries, isolating
//! per-feed failures. [`run`] owns the shared HTTP client and tokio runtime and
//! drives the loop, honoring the shutdown flag between feeds and during the
//! inter-tick sleep.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};

use crate::config::{Config, Feed};
use crate::feed::{Entry, fetch_and_parse};
use crate::notify;
use crate::state::{SeenKey, SeenStore};

/// Global HTTP timeout for a single feed-fetch attempt.
///
/// The daemon typically runs behind a fake-IP proxy whose cold connect (TLS +
/// tunnel setup) takes a few seconds and varies with upstream latency; the
/// former 10s timed out routinely on a slow tick. 20s comfortably covers the
/// observed worst case while staying well under the 60s poll interval. This caps
/// a *single* attempt — `fetch_and_parse` retries transient failures on top of
/// it.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(20);

/// Sleep granularity for [`interruptible_sleep`]: poll the shutdown flag this
/// often so a shutdown is observed promptly without busy-waiting.
const SLEEP_SLICE: StdDuration = StdDuration::from_millis(500);

/// Build the dedup [`SeenKey`] for an entry's `(id, updated)` pair.
fn seen_key(entry: &Entry) -> SeenKey {
    SeenKey {
        id: entry.id.clone(),
        updated: entry.updated,
    }
}

/// Whether `entry` should trigger a notification right now.
///
/// Pure function of the seen-set and the age window: notify only if the
/// `(id, updated)` pair has not been seen *and* the entry was updated within the
/// last `max_age_minutes`. The age window kills restart/first-run storms; the
/// `(id, updated)` pair (not id alone) lets each progress update of the same
/// incident notify exactly once.
pub fn is_eligible(
    entry: &Entry,
    seen: &SeenStore,
    now: DateTime<Utc>,
    max_age_minutes: i64,
) -> bool {
    !seen.contains(&seen_key(entry)) && entry.updated >= now - Duration::minutes(max_age_minutes)
}

/// Fetch and process a single feed, notifying on each eligible entry.
///
/// Fetch/parse errors are logged at `warn` and swallowed so one dead feed never
/// crashes the loop or blocks the others. For every eligible entry a
/// notification is sent and, only if delivery succeeded, its `(id, updated)` key
/// recorded so it won't fire again. A failed send leaves the entry unseen so it
/// is retried on the next tick (while still within the age window).
fn process_feed(
    client: &wreq::Client,
    runtime: &tokio::runtime::Runtime,
    feed: &Feed,
    seen: &mut SeenStore,
    now: DateTime<Utc>,
    max_age_minutes: i64,
    sound: bool,
) {
    let entries = match fetch_and_parse(client, runtime, feed) {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!("skipping feed {} ({}): {err:#}", feed.name, feed.url);
            return;
        }
    };

    for entry in entries {
        if !is_eligible(&entry, seen, now, max_age_minutes) {
            continue;
        }

        let body = notify::build_body(
            entry.status.as_deref(),
            entry.detail.as_deref(),
            entry.link.as_deref(),
        );
        // Only record the entry as seen if delivery actually succeeded; a
        // transient send failure must not permanently suppress this update.
        if notify::send(&feed.name, &entry.title, &body, sound) {
            seen.insert(seen_key(&entry));
        }
    }
}

/// Sleep for `total`, polling `shutdown` in ~500ms slices.
///
/// Returns early as soon as `shutdown` is observed set, so a pending shutdown is
/// never delayed by more than one slice.
fn interruptible_sleep(total: StdDuration, shutdown: &AtomicBool) {
    let mut remaining = total;
    while !remaining.is_zero() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let slice = remaining.min(SLEEP_SLICE);
        std::thread::sleep(slice);
        remaining -= slice;
    }
}

/// Run the poll loop until `shutdown` is set.
///
/// Builds, once, a single-threaded tokio runtime plus a shared browser-emulating
/// [`wreq::Client`] (some status hosts reset non-browser TLS handshakes). Then on
/// each tick it polls every feed in turn — checking `shutdown` before and between
/// feeds so a single slow feed can't stretch shutdown beyond its own retry
/// budget (attempts × timeout), and never blocks the others — prunes and
/// persists the seen-store, and sleeps interruptibly until the next tick. A final
/// `save` is always performed before returning on shutdown.
///
/// If the runtime or client can't be built the daemon can't fetch anything, so we
/// log and return (a clean exit; launchd will relaunch).
pub fn run(config: &Config, seen: &mut SeenStore, seen_path: &Path, shutdown: &AtomicBool) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            log::error!("could not start async runtime: {err:#}");
            return;
        }
    };

    let client = match wreq::Client::builder()
        .emulation(config.tls_emulation)
        .timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            log::error!("could not build HTTP client: {err:#}");
            return;
        }
    };
    log::info!(
        "fetching feeds with {:?} TLS emulation",
        config.tls_emulation
    );

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let now = Utc::now();

        for feed in &config.feeds {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            process_feed(
                &client,
                &runtime,
                feed,
                seen,
                now,
                config.max_age_minutes,
                config.notification_sound,
            );
        }

        seen.prune(now, config.max_age_minutes);
        if let Err(err) = seen.save(seen_path) {
            log::error!(
                "failed to persist seen-store {}: {err:#}",
                seen_path.display()
            );
        }

        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        interruptible_sleep(StdDuration::from_secs(config.poll_interval_secs), shutdown);
    }

    // Final save on shutdown so a clean exit never loses recent state.
    if let Err(err) = seen.save(seen_path) {
        log::error!(
            "failed to persist seen-store on shutdown {}: {err:#}",
            seen_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, updated: DateTime<Utc>) -> Entry {
        Entry {
            id: id.to_string(),
            updated,
            title: "An incident".to_string(),
            link: Some("https://status.example.com/incidents/abc".to_string()),
            status: Some("Investigating".to_string()),
            detail: None,
        }
    }

    #[test]
    fn is_eligible_true_for_fresh_and_recent() {
        let now = Utc::now();
        let seen = SeenStore::default();
        let e = entry("incident-1", now - Duration::minutes(2));
        assert!(is_eligible(&e, &seen, now, 10));
    }

    #[test]
    fn is_eligible_false_when_already_seen() {
        let now = Utc::now();
        let updated = now - Duration::minutes(2);
        let e = entry("incident-1", updated);

        let mut seen = SeenStore::default();
        seen.insert(SeenKey {
            id: e.id.clone(),
            updated,
        });

        assert!(!is_eligible(&e, &seen, now, 10));
    }

    #[test]
    fn is_eligible_true_at_exact_age_boundary() {
        let now = Utc::now();
        let seen = SeenStore::default();
        // Exactly at the window edge: updated == now - max_age. The `>=` in the
        // age check must treat this as eligible.
        let e = entry("incident-1", now - Duration::minutes(10));
        assert!(
            is_eligible(&e, &seen, now, 10),
            "entry exactly at the age boundary must be eligible"
        );
    }

    #[test]
    fn is_eligible_false_when_too_old() {
        let now = Utc::now();
        let seen = SeenStore::default();
        // 11 minutes old, outside a 10-minute window.
        let e = entry("incident-1", now - Duration::minutes(11));
        assert!(!is_eligible(&e, &seen, now, 10));
    }

    #[test]
    fn is_eligible_dedups_on_id_and_updated_pair_not_id_alone() {
        let now = Utc::now();
        let old_updated = now - Duration::minutes(5);
        let new_updated = now - Duration::minutes(1);

        // The same incident id was already seen at an OLDER updated timestamp.
        let mut seen = SeenStore::default();
        seen.insert(SeenKey {
            id: "incident-1".to_string(),
            updated: old_updated,
        });

        // A progress update bumps <updated>: same id, new timestamp -> eligible.
        let updated_entry = entry("incident-1", new_updated);
        assert!(
            is_eligible(&updated_entry, &seen, now, 10),
            "a new (id, updated) pair must be eligible even when the id was seen before"
        );

        // The previously seen pair itself stays ineligible.
        let old_entry = entry("incident-1", old_updated);
        assert!(!is_eligible(&old_entry, &seen, now, 10));
    }
}
