//! Feed fetching, parsing, and normalization.
//!
//! Pure helpers (`strip_html`, `parse_status`) are factored out so they can be
//! unit-tested without any network or notification side effects. `parse_feed`
//! turns raw Atom/RSS into normalized [`Entry`]s, and `fetch_and_parse` wraps it
//! with a bounded HTTP GET over a shared browser-emulating [`wreq::Client`].

use anyhow::Context as _;
use chrono::{DateTime, Utc};

use crate::config::Feed;

/// Maximum number of bytes we keep from a feed response body. Statuspage /
/// Instatus feeds are tiny; this cap defends against a misbehaving or malicious
/// server trying to exhaust memory.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// A normalized feed entry, independent of whether the source was Atom or RSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable per-incident id (Atom `<id>`). Statuspage/Instatus keep this
    /// constant across updates of the same incident.
    pub id: String,
    /// `<updated>` if present, else `<published>`. Entries with neither are
    /// skipped during parsing (we can't age-check them).
    pub updated: DateTime<Utc>,
    /// Entry title (empty string if the feed omitted one).
    pub title: String,
    /// First link's href, if any.
    pub link: Option<String>,
    /// Parsed status keyword (e.g. "Monitoring"), if one was found in the
    /// status text.
    pub status: Option<String>,
}

/// Parse raw Atom/RSS XML into normalized [`Entry`]s.
///
/// `updated` falls back to `published`; entries with neither timestamp are
/// skipped (logged at debug). The status text source prefers the entry's
/// `<content>` body and falls back to its `<summary>`; the text is HTML-stripped
/// before keyword extraction.
pub fn parse_feed(xml: &[u8], base_uri: &str) -> anyhow::Result<Vec<Entry>> {
    // feed-rs reads from any `Read` and honors the encoding declared in the XML
    // prolog, so we hand it raw bytes rather than requiring valid UTF-8.
    //
    // The feed's own URL is passed as the base URI so feed-rs's id generator is
    // deterministic across polls: for entries that lack an explicit <id>/<guid>,
    // feed-rs synthesizes one by hashing entry content together with the base
    // URI. Without a stable base URI those synthetic ids could change between
    // polls, breaking the (id, updated) dedup key and re-notifying the same
    // entry. (Our default Statuspage/Instatus feeds have explicit <id>s and are
    // unaffected; this hardens user-added feeds.)
    let parser = feed_rs::parser::Builder::new()
        .base_uri(Some(base_uri))
        .build();
    let feed = parser.parse(xml).context("parsing feed XML")?;

    let mut entries = Vec::with_capacity(feed.entries.len());
    for entry in feed.entries {
        let Some(updated) = entry.updated.or(entry.published) else {
            log::debug!(
                "skipping feed entry with no updated/published timestamp (id={})",
                entry.id
            );
            continue;
        };

        let title = entry.title.map(|t| t.content).unwrap_or_default();
        // Prefer the canonical `rel="alternate"` link (the human-facing incident
        // page), falling back to the first link of any rel.
        let link = entry
            .links
            .iter()
            .find(|l| l.rel.as_deref() == Some("alternate"))
            .or_else(|| entry.links.first())
            .map(|l| l.href.clone());

        // Status text: prefer <content> body, fall back to <summary>.
        let status_text = entry
            .content
            .and_then(|c| c.body)
            .or_else(|| entry.summary.map(|s| s.content));
        let status = status_text.and_then(|text| parse_status(&strip_html(&text)));

        entries.push(Entry {
            id: entry.id,
            updated,
            title,
            link,
            status,
        });
    }

    Ok(entries)
}

/// Fetch a feed over the shared browser-emulating client and parse it into
/// [`Entry`]s.
///
/// The `client` is built once by the caller with a browser TLS/HTTP2 fingerprint
/// (some status hosts reset non-browser TLS handshakes); the async request is
/// driven to completion on the caller's `runtime`. Non-2xx responses are turned
/// into errors via `error_for_status`. The body is read as raw bytes (capped at
/// [`MAX_BODY_BYTES`]) so non-UTF-8 feeds — which declare their encoding in the
/// XML prolog — parse correctly.
pub fn fetch_and_parse(
    client: &wreq::Client,
    runtime: &tokio::runtime::Runtime,
    feed: &Feed,
) -> anyhow::Result<Vec<Entry>> {
    let body = runtime.block_on(async {
        let response = client
            .get(&feed.url)
            .send()
            .await
            .with_context(|| format!("fetching feed {} ({})", feed.name, feed.url))?
            .error_for_status()
            .with_context(|| format!("feed {} returned an error status", feed.name))?;

        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("reading feed body {} ({})", feed.name, feed.url))?;
        anyhow::Ok(bytes)
    })?;

    // Defensive cap: keep at most MAX_BODY_BYTES before handing to the parser.
    let body = &body[..body.len().min(MAX_BODY_BYTES)];

    parse_feed(body, &feed.url)
        .with_context(|| format!("parsing feed {} ({})", feed.name, feed.url))
}

/// Ordered set of status keywords. The first one (by position in the text)
/// that matches on a word boundary wins.
const STATUS_KEYWORDS: &[&str] = &[
    "Investigating",
    "Identified",
    "Monitoring",
    "Resolved",
    "Update",
    "Postmortem",
];

/// Remove HTML tags, decode a few common entities, and collapse whitespace.
///
/// - Drops anything between `<` and `>` (tags).
/// - Decodes `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`.
/// - Unrecognized entities (e.g. numeric `&#9731;` or unknown named ones) are
///   left as-is, verbatim.
/// - Collapses runs of whitespace to a single space and trims the result.
pub fn strip_html(input: &str) -> String {
    // First, drop tags.
    let mut without_tags = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => without_tags.push(ch),
            _ => {}
        }
    }

    // Decode a small set of common entities, leaving unknown ones verbatim.
    let decoded = decode_entities(&without_tags);

    // Collapse whitespace runs to a single space and trim.
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode a small, fixed set of HTML entities. Anything not recognized is
/// emitted verbatim (including the leading `&`).
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        if let Some((decoded, len)) = match_entity(tail) {
            out.push(decoded);
            rest = &tail[len..];
        } else {
            // Not a recognized entity: emit the '&' verbatim and continue.
            out.push('&');
            rest = &tail['&'.len_utf8()..];
        }
    }
    out.push_str(rest);
    out
}

/// If `s` starts with one of the recognized entities, return the decoded
/// character and the byte length of the entity that was consumed.
fn match_entity(s: &str) -> Option<(char, usize)> {
    const ENTITIES: &[(&str, char)] = &[
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
    ];
    ENTITIES
        .iter()
        .find(|(name, _)| s.starts_with(name))
        .map(|(name, ch)| (*ch, name.len()))
}

/// Return the first matching status keyword from the ordered set, matched
/// case-insensitively on a word boundary. "First" means the keyword whose
/// match occurs earliest in the text. Returns the canonical capitalized
/// keyword, or `None` if no keyword is present.
pub fn parse_status(input: &str) -> Option<String> {
    let lower = input.to_lowercase();
    let bytes = lower.as_bytes();

    let mut best: Option<usize> = None;
    let mut best_keyword: Option<&str> = None;

    for keyword in STATUS_KEYWORDS {
        let needle = keyword.to_lowercase();
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&needle) {
            let pos = from + rel;
            let end = pos + needle.len();
            let before_ok = pos == 0 || !is_word_byte(bytes[pos - 1]);
            let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
            if before_ok && after_ok {
                if best.is_none_or(|b| pos < b) {
                    best = Some(pos);
                    best_keyword = Some(keyword);
                }
                break;
            }
            from = pos + 1;
        }
    }

    best_keyword.map(|k| (*k).to_string())
}

/// A "word" byte for boundary purposes: ASCII alphanumeric or underscore.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Representative feed URL used as the feed-rs base URI in parse tests. All
    /// fixtures carry explicit `<id>`s, so the base URI does not affect their
    /// parsed ids.
    const TEST_BASE_URI: &str = "https://status.example.com/feed.atom";

    #[test]
    fn strip_html_removes_tags_and_collapses_whitespace() {
        let html = "<p>Hello   <b>world</b>\n\n &amp; goodbye</p>";
        assert_eq!(strip_html(html), "Hello world & goodbye");
    }

    #[test]
    fn strip_html_decodes_common_entities() {
        let html = "a &lt;b&gt; &quot;c&quot; &#39;d&#39;";
        assert_eq!(strip_html(html), "a <b> \"c\" 'd'");
    }

    #[test]
    fn strip_html_leaves_unrecognized_entity_verbatim() {
        // Numeric entity not in our small decode set must pass through as-is.
        let html = "snowman &#9731; here";
        assert_eq!(strip_html(html), "snowman &#9731; here");
        // An unknown named entity is also left verbatim.
        assert_eq!(strip_html("x &nbsp; y"), "x &nbsp; y");
    }

    #[test]
    fn parse_status_finds_keyword() {
        assert_eq!(
            parse_status("The incident has been Resolved."),
            Some("Resolved".to_string())
        );
        assert_eq!(
            parse_status("we are monitoring the situation"),
            Some("Monitoring".to_string())
        );
    }

    #[test]
    fn parse_status_returns_none_when_absent() {
        assert_eq!(parse_status("Everything is fine, nothing to report"), None);
    }

    #[test]
    fn parse_status_picks_first_when_multiple_present() {
        // "Monitoring" appears before "Resolved" in the text, so it wins even
        // though "Resolved" comes earlier in the keyword ordering. "First"
        // means earliest by position in the text, not by keyword order.
        let text = "We are Monitoring the fix. Later it was Resolved.";
        assert_eq!(parse_status(text), Some("Monitoring".to_string()));

        // And when "Update" leads the text, it is correctly the first match.
        let text2 = "Update: now Resolved.";
        assert_eq!(parse_status(text2), Some("Update".to_string()));
    }

    #[test]
    fn parse_status_requires_word_boundary() {
        // "Resolved" embedded in a larger word must not match.
        assert_eq!(parse_status("unResolvedness lingers"), None);
        // But adjacent punctuation is a valid boundary.
        assert_eq!(
            parse_status("(Investigating)"),
            Some("Investigating".to_string())
        );
    }

    #[test]
    fn parse_feed_extracts_expected_fields() {
        let xml = include_bytes!("../tests/fixtures/sample.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("sample.atom should parse");
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.id, "tag:status.example.com,2005:Incident/12345");
        assert_eq!(entry.title, "Elevated error rates on the API");
        // <updated> wins over <published>.
        assert_eq!(
            entry.updated,
            "2026-06-24T12:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            entry.link.as_deref(),
            Some("https://status.example.com/incidents/abc123")
        );
        assert_eq!(entry.status.as_deref(), Some("Monitoring"));
    }

    #[test]
    fn strip_html_passes_bare_ampersand_verbatim() {
        // A bare '&' (not the start of a recognized entity) must pass through
        // verbatim without panicking.
        assert_eq!(strip_html("a & b"), "a & b");
        assert_eq!(strip_html("ends with &"), "ends with &");
    }

    #[test]
    fn parse_status_handles_non_ascii_before_keyword() {
        // A multi-byte char immediately preceding the keyword is a valid word
        // boundary and must not corrupt byte-index handling.
        assert_eq!(parse_status("café Resolved"), Some("Resolved".to_string()));
    }

    #[test]
    fn parse_feed_prefers_content_over_summary_status() {
        // content_wins.atom: <content> says "Resolved", <summary> says
        // "Investigating". Content must win.
        let xml = include_bytes!("../tests/fixtures/content_wins.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("content_wins.atom should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("Resolved"));
    }

    #[test]
    fn parse_feed_handles_missing_title_and_link() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:example:feed</id>
  <title>No Title Or Link</title>
  <updated>2026-06-24T00:00:00Z</updated>
  <entry>
    <id>urn:example:entry:bare</id>
    <updated>2026-06-24T07:15:00Z</updated>
  </entry>
</feed>"#;
        let entries = parse_feed(xml.as_bytes(), TEST_BASE_URI).expect("feed should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "");
        assert_eq!(entries[0].link, None);
    }

    #[test]
    fn parse_feed_extracts_status_from_content_when_summary_lacks_it() {
        // content_only.atom has the keyword only in <content>; <summary> has none.
        let xml = include_bytes!("../tests/fixtures/content_only.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("content_only.atom should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("Investigating"));
    }

    #[test]
    fn parse_feed_skips_entry_without_timestamps() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:example:feed</id>
  <title>No Timestamp</title>
  <updated>2026-06-24T00:00:00Z</updated>
  <entry>
    <id>urn:example:entry:no-time</id>
    <title>An entry with no timestamps</title>
  </entry>
</feed>"#;
        let entries = parse_feed(xml.as_bytes(), TEST_BASE_URI).expect("feed should parse");
        assert!(
            entries.is_empty(),
            "entry without updated/published must be skipped"
        );
    }

    #[test]
    fn parse_feed_generates_stable_id_for_idless_entry() {
        // An Atom entry with no <id>: feed-rs synthesizes one by hashing the
        // entry content together with the base URI. Parsing twice with the same
        // base URI must yield the same synthetic id, otherwise the (id, updated)
        // dedup key would churn and re-notify the same entry every poll.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:example:feed</id>
  <title>No Entry Id</title>
  <updated>2026-06-24T00:00:00Z</updated>
  <entry>
    <title>Investigating an outage</title>
    <updated>2026-06-24T07:15:00Z</updated>
  </entry>
</feed>"#;
        let first = parse_feed(xml.as_bytes(), TEST_BASE_URI).expect("feed should parse");
        let second = parse_feed(xml.as_bytes(), TEST_BASE_URI).expect("feed should parse");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(!first[0].id.is_empty());
        assert_eq!(
            first[0].id, second[0].id,
            "synthetic id must be stable across polls with the same base URI"
        );
    }

    #[test]
    fn parse_feed_uses_published_when_updated_missing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:example:feed</id>
  <title>Published Only</title>
  <updated>2026-06-24T00:00:00Z</updated>
  <entry>
    <id>urn:example:entry:pub-only</id>
    <title>Published only</title>
    <published>2026-06-24T07:15:00Z</published>
  </entry>
</feed>"#;
        let entries = parse_feed(xml.as_bytes(), TEST_BASE_URI).expect("feed should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].updated,
            "2026-06-24T07:15:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }
}
