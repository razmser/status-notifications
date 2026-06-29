//! Feed fetching, parsing, and normalization.
//!
//! Pure helpers (`strip_html`, `find_status`) are factored out so they can be
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
    /// Latest update's message prose (keyword-stripped, length-capped), if one
    /// could be extracted from the status text.
    pub detail: Option<String>,
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
        let status = status_text
            .as_deref()
            .and_then(|text| find_status(&strip_html(text)).map(|(_, k)| k.to_string()));
        // Detail extraction runs on the raw HTML (block boundaries depend on
        // tags), so it uses `status_text` before stripping.
        let detail = status_text.as_deref().and_then(extract_latest_detail);

        entries.push(Entry {
            id: entry.id,
            updated,
            title,
            link,
            status,
            detail,
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
/// - Drops anything between `<` and `>` (tags), replacing each tag with a single
///   space so adjacent text never fuses across a tag boundary (e.g.
///   `ts</small><br><strong>Monitoring` becomes `ts Monitoring`, not
///   `tsMonitoring`). Without this, a keyword glued to preceding text would lose
///   its word boundary and go undetected.
/// - Decodes `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`.
/// - Unrecognized entities (e.g. numeric `&#9731;` or unknown named ones) are
///   left as-is, verbatim.
/// - Collapses runs of whitespace to a single space and trims the result, so the
///   inserted spaces never produce doubled or leading/trailing whitespace.
pub fn strip_html(input: &str) -> String {
    // First, drop tags, leaving a space where each tag was.
    let mut without_tags = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                without_tags.push(' ');
            }
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

/// Find the earliest status keyword in `input`, matched ASCII-case-insensitively
/// on a word boundary. Returns the byte position where the match starts (valid
/// for slicing `input`) together with the canonical capitalized keyword, or
/// `None` if no keyword is present. "Earliest" means the match occurring first
/// by byte position in the text, not by keyword order.
///
/// The match runs over the **original** string (comparing bytes
/// case-insensitively) rather than a `to_lowercase()` copy: lowercasing can
/// change byte lengths for some non-ASCII characters, which would shift the
/// returned position relative to the original text and corrupt later slicing.
pub fn find_status(input: &str) -> Option<(usize, &'static str)> {
    let bytes = input.as_bytes();

    let mut best: Option<(usize, &'static str)> = None;

    for keyword in STATUS_KEYWORDS {
        let needle = keyword.as_bytes();
        let mut from = 0;
        while let Some(rel) = find_ascii_ci(&bytes[from..], needle) {
            let pos = from + rel;
            let end = pos + needle.len();
            let before_ok = pos == 0 || !is_word_byte(bytes[pos - 1]);
            let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
            if before_ok && after_ok {
                if best.is_none_or(|(b, _)| pos < b) {
                    best = Some((pos, keyword));
                }
                break;
            }
            from = pos + 1;
        }
    }

    best
}

/// Find the first occurrence of `needle` in `haystack`, comparing bytes
/// ASCII-case-insensitively. Returns the byte offset of the match, or `None`.
fn find_ascii_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h.eq_ignore_ascii_case(n))
    })
}

/// A "word" byte for boundary purposes: ASCII alphanumeric or underscore.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Maximum number of **characters** (not bytes) kept in an extracted detail
/// message. Notifications are short-lived banners; the cap keeps the body
/// bounded regardless of how verbose a status update is.
const DETAIL_MAX_CHARS: usize = 200;

/// Extract the latest update's message prose from a raw HTML status body.
///
/// Models an *update* as a keyword-bearing block plus the prose blocks that
/// follow it, stopping at the next keyword block or a list/components section.
/// The single abstraction covers all three real provider formats (Statuspage,
/// Instatus, FlashDuty) and degrades to `None` for feeds it can't model.
///
/// Returns the cleaned, length-capped message (with the leading keyword and any
/// `Keyword - ` / `Status:` lead-in removed), or `None` when no keyword block is
/// present or the message would be empty.
pub fn extract_latest_detail(html: &str) -> Option<String> {
    let blocks = split_into_blocks(html);

    // The first block that carries a status keyword starts the latest update.
    let kw_index = blocks
        .iter()
        .position(|(_, stripped)| find_status(stripped).is_some())?;

    let mut parts: Vec<String> = Vec::new();

    // Text that follows the keyword within its own block (discards any leading
    // timestamp, which sits *before* the keyword and is thus dropped).
    let (_, kw_stripped) = &blocks[kw_index];
    let (pos, keyword) = find_status(kw_stripped)?;
    let after = &kw_stripped[pos + keyword.len()..];
    if !after.trim().is_empty() {
        parts.push(after.to_string());
    }

    // Fold in following blocks until the next keyword or a list/components
    // section, both of which mark the end of this update's prose.
    for (raw, stripped) in &blocks[kw_index + 1..] {
        if find_status(stripped).is_some() || contains_list(raw) {
            break;
        }
        parts.push(stripped.clone());
    }

    let cleaned = clean_detail(&parts.join(" "));
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_chars(&cleaned, DETAIL_MAX_CHARS))
}

/// Split raw HTML into ordered blocks on `<p>`/`</p>` and `<br><br>` (double
/// break) boundaries. A **single** `<br>` is deliberately not a split point so a
/// `<small>ts</small><br><strong>kw</strong>` timestamp stays in the keyword
/// block (and is later discarded as text *before* the keyword).
///
/// Each block is returned as `(raw, stripped)`: the raw slice retains tags so
/// `<ul>`/`<li>` sections can be detected, while `stripped` is the
/// `strip_html`-ed text used for keyword matching. Blocks whose stripped text is
/// empty are dropped.
fn split_into_blocks(html: &str) -> Vec<(String, String)> {
    let mut raw_blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        if let Some(len) = match_p_tag(rest) {
            raw_blocks.push(std::mem::take(&mut current));
            i += len;
            continue;
        }
        if let Some(len) = match_double_br(rest) {
            raw_blocks.push(std::mem::take(&mut current));
            i += len;
            continue;
        }
        // Not a boundary: copy one whole UTF-8 char into the current block.
        let ch = rest.chars().next().expect("non-empty rest");
        let len = ch.len_utf8();
        current.push_str(&rest[..len]);
        i += len;
    }
    raw_blocks.push(current);

    raw_blocks
        .into_iter()
        .map(|raw| {
            let stripped = strip_html(&raw);
            (raw, stripped)
        })
        .filter(|(_, stripped)| !stripped.is_empty())
        .collect()
}

/// If `s` starts with a `<p>`/`<p ...>`/`</p>`/`</p ...>` tag, return its byte
/// length. `<pre>`, `<param>`, etc. are not matched (the char after `p` must be
/// `>` or whitespace).
fn match_p_tag(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.is_empty() || b[0] != b'<' {
        return None;
    }
    let mut i = 1;
    if i < b.len() && b[i] == b'/' {
        i += 1;
    }
    if i >= b.len() || (b[i] | 0x20) != b'p' {
        return None;
    }
    let after = i + 1;
    if after >= b.len() || (b[after] != b'>' && !b[after].is_ascii_whitespace()) {
        return None;
    }
    let gt = s[after..].find('>')?;
    Some(after + gt + 1)
}

/// If `s` starts with a single `<br>`/`<br/>`/`<br />` tag, return its byte
/// length. Tags like `<break>` are rejected (the chars between `br` and `>` must
/// be only optional whitespace and an optional `/`).
fn match_br_tag(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 4 || b[0] != b'<' || (b[1] | 0x20) != b'b' || (b[2] | 0x20) != b'r' {
        return None;
    }
    let mut i = 3;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && b[i] == b'/' {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && b[i] == b'>' {
        Some(i + 1)
    } else {
        None
    }
}

/// If `s` starts with two consecutive `<br>` tags (separated only by optional
/// whitespace), return the total byte length consumed.
fn match_double_br(s: &str) -> Option<usize> {
    let first = match_br_tag(s)?;
    let b = s.as_bytes();
    let mut i = first;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let second = match_br_tag(&s[i..])?;
    Some(i + second)
}

/// Whether `raw` contains the start of a list (`<ul`/`<li`), matched
/// ASCII-case-insensitively. Used as a locale-independent stop boundary for the
/// "Affected components" section.
fn contains_list(raw: &str) -> bool {
    find_ascii_ci(raw.as_bytes(), b"<ul").is_some()
        || find_ascii_ci(raw.as_bytes(), b"<li").is_some()
}

/// Collapse whitespace, drop a single leading `-`/`–`/`:` separator (and its
/// surrounding spaces), and trim.
fn clean_detail(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    let stripped = trimmed.strip_prefix(['-', '–', ':']).unwrap_or(trimmed);
    stripped.trim().to_string()
}

/// Truncate `s` to at most `max` characters (char-safe), appending `…` when the
/// string was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
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
    fn find_status_finds_keyword() {
        assert_eq!(
            find_status("The incident has been Resolved.").map(|(_, k)| k),
            Some("Resolved")
        );
        assert_eq!(
            find_status("we are monitoring the situation").map(|(_, k)| k),
            Some("Monitoring")
        );
    }

    #[test]
    fn find_status_returns_none_when_absent() {
        assert_eq!(find_status("Everything is fine, nothing to report"), None);
    }

    #[test]
    fn find_status_picks_first_when_multiple_present() {
        // "Monitoring" appears before "Resolved" in the text, so it wins even
        // though "Resolved" comes earlier in the keyword ordering. "First"
        // means earliest by position in the text, not by keyword order.
        let text = "We are Monitoring the fix. Later it was Resolved.";
        assert_eq!(find_status(text).map(|(_, k)| k), Some("Monitoring"));

        // And when "Update" leads the text, it is correctly the first match.
        let text2 = "Update: now Resolved.";
        assert_eq!(find_status(text2).map(|(_, k)| k), Some("Update"));
    }

    #[test]
    fn find_status_requires_word_boundary() {
        // "Resolved" embedded in a larger word must not match.
        assert_eq!(find_status("unResolvedness lingers"), None);
        // But adjacent punctuation is a valid boundary.
        assert_eq!(
            find_status("(Investigating)").map(|(_, k)| k),
            Some("Investigating")
        );
    }

    #[test]
    fn find_status_returns_position_at_keyword_start() {
        let text = "We are Monitoring the fix.";
        let (pos, keyword) = find_status(text).expect("keyword present");
        assert_eq!(keyword, "Monitoring");
        assert_eq!(pos, text.find("Monitoring").unwrap());
        // The returned position must slice the original text at the keyword.
        assert!(text[pos..].starts_with("Monitoring"));
    }

    #[test]
    fn extract_latest_detail_stops_at_next_keyword_no_timestamp_leak() {
        // Claude/Statuspage shape: stacked <p> updates, newest first, each with a
        // <small>timestamp</small><br><strong>keyword</strong> lead-in. Only the
        // latest update's message is returned, and the timestamp (which precedes
        // the keyword) must not leak in.
        let html = "<p><small>Jun 24, 12:30 PDT</small><br><strong>Monitoring</strong> - A fix has been implemented.</p>\
<p><small>Jun 24, 12:00 PDT</small><br><strong>Identified</strong> - The root cause was found.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("A fix has been implemented.")
        );
    }

    #[test]
    fn extract_latest_detail_stops_before_list_section() {
        // OpenAI/Instatus shape: <b>Status: kw</b>, message, then an "Affected
        // components" <ul>. The list section must be excluded.
        let html = "<b>Status: Monitoring</b><br/><br/>A fix has been deployed and traffic is recovering.<br/><br/><b>Affected components</b><ul><li>API</li></ul>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("A fix has been deployed and traffic is recovering.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_following_block() {
        // DeepSeek/FlashDuty shape: keyword in one <p>, the (bilingual) message in
        // a separate following <p>. The second block is folded into the detail.
        let html =
            "<p><strong>Status:</strong> resolved</p><p>The issue has been fixed. 问题已解决。</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("The issue has been fixed. 问题已解决。")
        );
    }

    #[test]
    fn extract_latest_detail_returns_none_without_keyword() {
        let html = "<p>All systems are operational and nothing is amiss.</p>";
        assert_eq!(extract_latest_detail(html), None);
    }

    #[test]
    fn extract_latest_detail_returns_none_when_list_immediately_follows_keyword() {
        // Keyword block with no inline prose, immediately followed by a list:
        // nothing to surface, so the detail is empty -> None.
        let html = "<p><strong>Monitoring</strong></p><ul><li>API degraded</li></ul>";
        assert_eq!(extract_latest_detail(html), None);
    }

    #[test]
    fn extract_latest_detail_truncates_at_200_chars_with_ellipsis() {
        // Non-ASCII message longer than the cap: truncation must be char-safe and
        // append a single ellipsis.
        let long = "あ".repeat(250);
        let html = format!("<p><strong>Monitoring</strong> - {long}</p>");
        let result = extract_latest_detail(&html).expect("keyword present");
        assert_eq!(result.chars().count(), DETAIL_MAX_CHARS + 1);
        assert!(result.ends_with('…'));
        assert!(result.starts_with('あ'));
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
        assert_eq!(
            entry.detail.as_deref(),
            Some("A fix has been deployed and we are monitoring the results.")
        );
    }

    #[test]
    fn parse_feed_extracts_detail_claude_stacked_latest_update_only() {
        // Claude/Statuspage: three stacked <p> updates, newest first. Only the
        // latest update's message is surfaced; extraction stops at the next
        // keyword ("Identified") and the leading timestamp does not leak in.
        let xml = include_bytes!("../tests/fixtures/claude_stacked.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("claude_stacked.atom should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("Monitoring"));
        let detail = entries[0].detail.as_deref().expect("detail present");
        assert_eq!(
            detail,
            "A fix has been implemented and we are monitoring the results."
        );
        assert!(!detail.contains("Identified"), "must not leak older update");
        assert!(!detail.contains("UTC"), "must not leak timestamp");
    }

    #[test]
    fn parse_feed_extracts_detail_openai_without_components_list() {
        // OpenAI/Instatus: CDATA <b>Status: kw</b> + message + "Affected
        // components" <ul>. The list section is excluded from the detail.
        let xml = include_bytes!("../tests/fixtures/openai.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("openai.atom should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("Monitoring"));
        let detail = entries[0].detail.as_deref().expect("detail present");
        assert_eq!(detail, "A fix has been deployed and traffic is recovering.");
        assert!(
            !detail.contains("Affected components"),
            "must not include the components section"
        );
        assert!(!detail.contains("API"), "must not include the list items");
    }

    #[test]
    fn parse_feed_extracts_detail_deepseek_from_summary_fallback() {
        // DeepSeek/FlashDuty: <summary>-only (no <content>), with the keyword in
        // the first <p> and the bilingual message in a separate second <p>. This
        // exercises the content->summary fallback and the following-block fold.
        let xml = include_bytes!("../tests/fixtures/deepseek.atom");
        let entries = parse_feed(xml, TEST_BASE_URI).expect("deepseek.atom should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status.as_deref(), Some("Resolved"));
        assert_eq!(
            entries[0].detail.as_deref(),
            Some(
                "服务已恢复正常，所有功能可正常使用。 Service has returned to normal and all features are available."
            )
        );
    }

    #[test]
    fn build_body_renders_keyword_message_for_all_default_feed_formats() {
        // End-to-end acceptance: each of the three real default-feed formats,
        // once parsed, composes through `build_body` into the final
        // "Keyword — message\n<link>" notification body. This closes the gap
        // between the per-field fixture assertions and the body composition.
        let cases = [
            (
                &include_bytes!("../tests/fixtures/claude_stacked.atom")[..],
                "Monitoring — A fix has been implemented and we are monitoring the results.",
            ),
            (
                &include_bytes!("../tests/fixtures/openai.atom")[..],
                "Monitoring — A fix has been deployed and traffic is recovering.",
            ),
            (
                &include_bytes!("../tests/fixtures/deepseek.atom")[..],
                "Resolved — 服务已恢复正常，所有功能可正常使用。 Service has returned to normal and all features are available.",
            ),
        ];

        for (xml, expected_first_line) in cases {
            let entries = parse_feed(xml, TEST_BASE_URI).expect("fixture should parse");
            let entry = &entries[0];
            let body = crate::notify::build_body(
                entry.status.as_deref(),
                entry.detail.as_deref(),
                entry.link.as_deref(),
            );
            let first_line = body.lines().next().expect("body has a first line");
            assert_eq!(first_line, expected_first_line);
        }
    }

    #[test]
    fn strip_html_passes_bare_ampersand_verbatim() {
        // A bare '&' (not the start of a recognized entity) must pass through
        // verbatim without panicking.
        assert_eq!(strip_html("a & b"), "a & b");
        assert_eq!(strip_html("ends with &"), "ends with &");
    }

    #[test]
    fn find_status_handles_non_ascii_before_keyword() {
        // A multi-byte char immediately preceding the keyword is a valid word
        // boundary and must not corrupt byte-index handling. The returned
        // position must still slice the original (non-ASCII) text at the keyword.
        let text = "café Resolved";
        let (pos, keyword) = find_status(text).expect("keyword present");
        assert_eq!(keyword, "Resolved");
        assert!(
            text[pos..].starts_with("Resolved"),
            "position must remain valid for slicing original non-ASCII text"
        );
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
