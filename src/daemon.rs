//! The poll loop and its supporting pieces.
//!
//! [`is_eligible`] is a pure filter (dedup `(id, updated)` pair + age window) so
//! it can be unit-tested without any network or notification side effects.
//! [`process_feed`] fetches one feed and notifies on eligible entries, isolating
//! per-feed failures. [`run`] owns the shared HTTP agent and drives the loop,
//! honoring the shutdown flag between feeds and during the inter-tick sleep.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};

use crate::config::{Config, Feed};
use crate::feed::{Entry, fetch_and_parse};
use crate::notify;
use crate::state::{SeenKey, SeenStore};

/// Global HTTP timeout for a feed fetch.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// `User-Agent` header sent on every feed request.
const USER_AGENT: &str = concat!("status-notifications/", env!("CARGO_PKG_VERSION"));

/// Sleep granularity for [`interruptible_sleep`]: poll the shutdown flag this
/// often so a shutdown is observed promptly without busy-waiting.
const SLEEP_SLICE: StdDuration = StdDuration::from_millis(500);

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
    let key = SeenKey {
        id: entry.id.clone(),
        updated: entry.updated,
    };
    !seen.contains(&key) && entry.updated >= now - Duration::minutes(max_age_minutes)
}

/// Fetch and process a single feed, notifying on each eligible entry.
///
/// Fetch/parse errors are logged at `warn` and swallowed so one dead feed never
/// crashes the loop or blocks the others. For every eligible entry a
/// notification is sent and its `(id, updated)` key recorded so it won't fire
/// again.
fn process_feed(
    agent: &ureq::Agent,
    feed: &Feed,
    seen: &mut SeenStore,
    now: DateTime<Utc>,
    max_age_minutes: i64,
) {
    let entries = match fetch_and_parse(agent, feed) {
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

        let body = notify::build_body(entry.status.as_deref(), entry.link.as_deref());
        notify::send(&feed.name, &entry.title, &body);

        seen.insert(SeenKey {
            id: entry.id,
            updated: entry.updated,
        });
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
/// Builds the shared [`ureq::Agent`] once (global timeout + real `User-Agent`),
/// then on each tick polls every feed in turn — checking `shutdown` before and
/// between feeds so a hung fetch can't stretch shutdown to `feeds × timeout` —
/// prunes and persists the seen-store, and sleeps interruptibly until the next
/// tick. A final `save` is always performed before returning on shutdown.
pub fn run(config: &Config, seen: &mut SeenStore, seen_path: &Path, shutdown: &AtomicBool) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let now = Utc::now();

        for feed in &config.feeds {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            process_feed(&agent, feed, seen, now, config.max_age_minutes);
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
