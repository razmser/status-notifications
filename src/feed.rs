//! Feed fetching, parsing, and normalization.
//!
//! Pure helpers (`strip_html`, `parse_status`) are factored out so they can be
//! unit-tested without any network or notification side effects. `parse_feed`
//! turns raw Atom/RSS into normalized [`Entry`]s, and `fetch_and_parse` wraps it
//! with a bounded HTTP GET over a shared [`ureq::Agent`].

use std::io::Read;

use anyhow::Context as _;
use chrono::{DateTime, Utc};

use crate::config::Feed;

/// Maximum number of bytes we will read from a feed response body. Statuspage
/// / Instatus feeds are tiny; this cap defends against a misbehaving or
/// malicious server trying to exhaust memory.
const MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;

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
pub fn parse_feed(xml: &str) -> anyhow::Result<Vec<Entry>> {
    let feed = feed_rs::parser::parse(xml.as_bytes()).context("parsing feed XML")?;

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
        let link = entry.links.first().map(|l| l.href.clone());

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

/// Fetch a feed over the shared HTTP agent and parse it into [`Entry`]s.
///
/// The `agent` is built once by the caller with a global timeout and a real
/// `User-Agent`; non-2xx responses already surface as errors via ureq's
/// `http_status_as_error` default. The body is read through a bounded reader
/// capped at [`MAX_BODY_BYTES`].
pub fn fetch_and_parse(agent: &ureq::Agent, feed: &Feed) -> anyhow::Result<Vec<Entry>> {
    let response = agent
        .get(&feed.url)
        .call()
        .with_context(|| format!("fetching feed {} ({})", feed.name, feed.url))?;

    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut body)
        .with_context(|| format!("reading feed body {} ({})", feed.name, feed.url))?;

    parse_feed(&body).with_context(|| format!("parsing feed {} ({})", feed.name, feed.url))
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
        let xml = include_str!("../tests/fixtures/sample.atom");
        let entries = parse_feed(xml).expect("sample.atom should parse");
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
    fn parse_feed_extracts_status_from_content_when_summary_lacks_it() {
        // content_only.atom has the keyword only in <content>; <summary> has none.
        let xml = include_str!("../tests/fixtures/content_only.atom");
        let entries = parse_feed(xml).expect("content_only.atom should parse");
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
        let entries = parse_feed(xml).expect("feed should parse");
        assert!(
            entries.is_empty(),
            "entry without updated/published must be skipped"
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
        let entries = parse_feed(xml).expect("feed should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].updated,
            "2026-06-24T07:15:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }
}
