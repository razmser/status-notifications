//! Feed fetching, parsing, and normalization.
//!
//! Pure helpers (`strip_html`, `find_status`) are factored out so they can be
//! unit-tested without any network or notification side effects. `parse_feed`
//! turns raw Atom/RSS into normalized [`Entry`]s, and `fetch_and_parse` wraps it
//! with a bounded, retried HTTP GET over a shared browser-emulating
//! [`wreq::Client`].

use std::time::Duration as StdDuration;

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

/// How many times a single feed fetch is attempted before giving up.
///
/// The daemon's typical environment is a fake-IP proxy that drops individual
/// streams and times out cold connections often enough that a single attempt is
/// too brittle: one blip would skip the feed for that whole tick and miss an
/// incident update. Retrying on a fresh connection clears the vast majority of
/// these. Three attempts bounds the worst case while keeping recovery quick.
const FETCH_MAX_ATTEMPTS: u8 = 3;

/// Backoff between fetch attempts. A fixed (non-jittered) short sleep: the
/// failure we are papering over is a brief proxy reset that clears in well under
/// this, so the next attempt usually succeeds without adding meaningful latency.
const FETCH_RETRY_BACKOFF: StdDuration = StdDuration::from_millis(1500);

/// Fetch a feed over the shared browser-emulating client and parse it into
/// [`Entry`]s.
///
/// The `client` is built once by the caller with a browser TLS/HTTP2 fingerprint
/// (some status hosts reset non-browser TLS handshakes); the async request is
/// driven to completion on the caller's `runtime`. Transient failures — a
/// transport error (timeout, broken pipe, TLS reset), a mid-body reset, or an
/// HTTP 5xx — are retried up to [`FETCH_MAX_ATTEMPTS`] times with
/// [`FETCH_RETRY_BACKOFF`] (see [`retry_loop`]); a deterministic HTTP 4xx is
/// returned immediately. The body is read as raw bytes (capped at
/// [`MAX_BODY_BYTES`]) so non-UTF-8 feeds — which declare their encoding in the
/// XML prolog — parse correctly. Parsing itself is not retried: a parse failure
/// is deterministic, so retrying cannot help.
pub fn fetch_and_parse(
    client: &wreq::Client,
    runtime: &tokio::runtime::Runtime,
    feed: &Feed,
) -> anyhow::Result<Vec<Entry>> {
    let body = fetch_with_retry(client, runtime, feed)?;

    // Defensive cap: keep at most MAX_BODY_BYTES before handing to the parser.
    let body = &body[..body.len().min(MAX_BODY_BYTES)];

    parse_feed(body, &feed.url)
        .with_context(|| format!("parsing feed {} ({})", feed.name, feed.url))
}

/// The outcome of one fetch attempt, split so [`retry_loop`] can tell transient
/// failures (worth retrying) from deterministic ones (not).
enum FetchAttempt {
    /// Body bytes of a successful response.
    Ok(Vec<u8>),
    /// Transient failure: a transport error, a mid-body reset, or an HTTP 5xx.
    /// Retrying on a fresh connection usually clears it.
    Retry(anyhow::Error),
    /// Deterministic failure: an HTTP 4xx (e.g. 404). Another attempt cannot
    /// help, so it is returned to the caller without retrying.
    Fatal(anyhow::Error),
}

/// Fetch the feed body, retrying transient failures via [`retry_loop`].
fn fetch_with_retry(
    client: &wreq::Client,
    runtime: &tokio::runtime::Runtime,
    feed: &Feed,
) -> anyhow::Result<Vec<u8>> {
    retry_loop(FETCH_MAX_ATTEMPTS, FETCH_RETRY_BACKOFF, || {
        fetch_once(client, runtime, feed)
    })
}

/// Drive `attempt` up to `max_attempts` times, retrying on
/// [`FetchAttempt::Retry`] after sleeping `backoff`, and returning immediately
/// on [`FetchAttempt::Ok`] or [`FetchAttempt::Fatal`]. After the final attempt
/// the last error is returned.
///
/// Factored out — with `backoff` and `attempt` taken as parameters — so the
/// retry policy can be unit-tested with canned [`FetchAttempt`] sequences and a
/// zero-length backoff: no network, no sleeping.
fn retry_loop<F>(max_attempts: u8, backoff: StdDuration, mut attempt: F) -> anyhow::Result<Vec<u8>>
where
    F: FnMut() -> FetchAttempt,
{
    let mut last_err: Option<anyhow::Error> = None;
    for n in 1..=max_attempts {
        match attempt() {
            FetchAttempt::Ok(bytes) => return Ok(bytes),
            FetchAttempt::Fatal(err) => return Err(err),
            FetchAttempt::Retry(err) => {
                log::debug!("fetch attempt {n}/{max_attempts} failed (will retry): {err:#}");
                last_err = Some(err);
                if n < max_attempts {
                    std::thread::sleep(backoff);
                }
            }
        }
    }
    Err(last_err.expect("max_attempts >= 1 guarantees at least one attempt ran"))
}

/// Perform a single fetch attempt and classify its outcome.
///
/// Each failure site tags its result transient ([`FetchAttempt::Retry`]) or
/// deterministic ([`FetchAttempt::Fatal`]) at the point where the distinction is
/// known, rather than by introspecting the resulting error afterwards: a request
/// that never produced a response, a 5xx, or a mid-body reset is transient; a
/// 4xx is not.
fn fetch_once(
    client: &wreq::Client,
    runtime: &tokio::runtime::Runtime,
    feed: &Feed,
) -> FetchAttempt {
    // Every failure carries whether it is worth retrying.
    let outcome: Result<Vec<u8>, (anyhow::Error, bool)> = runtime.block_on(async {
        let response = match client.get(&feed.url).send().await {
            Ok(response) => response,
            Err(err) => {
                return Err((
                    anyhow::Error::new(err)
                        .context(format!("fetching feed {} ({})", feed.name, feed.url)),
                    true,
                ));
            }
        };

        let status = response.status();
        if status.is_client_error() {
            return Err((
                anyhow::anyhow!("feed {} returned client error status {}", feed.name, status),
                false,
            ));
        }
        if status.is_server_error() {
            return Err((
                anyhow::anyhow!("feed {} returned server error status {}", feed.name, status),
                true,
            ));
        }

        match response.bytes().await {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(err) => Err((
                anyhow::Error::new(err)
                    .context(format!("reading feed body {} ({})", feed.name, feed.url)),
                true,
            )),
        }
    });

    match outcome {
        Ok(bytes) => FetchAttempt::Ok(bytes),
        Err((err, retryable)) => {
            if retryable {
                FetchAttempt::Retry(err)
            } else {
                FetchAttempt::Fatal(err)
            }
        }
    }
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
///
/// # Known limitations (non-default feeds only)
///
/// The default feeds (Statuspage, Instatus, FlashDuty) are unaffected by these;
/// they are residual graceful-degradation edges for arbitrary user feeds:
///
/// - A multi-paragraph message whose *later* paragraph happens to begin with a
///   status-keyword word (so that paragraph parses as an update header) is
///   truncated at that paragraph — the trailing paragraphs are dropped rather
///   than folded in.
pub fn extract_latest_detail(html: &str) -> Option<String> {
    let blocks = split_into_blocks(html);

    // The first block that carries a status keyword starts the latest update.
    // A single pass yields both the block index and the keyword match (byte
    // position + canonical keyword) so `find_status` is not recomputed.
    let (kw_index, (pos, keyword)) = blocks
        .iter()
        .enumerate()
        .find_map(|(i, (_, stripped))| find_status(stripped).map(|m| (i, m)))?;

    let mut parts: Vec<String> = Vec::new();

    // Text that follows the keyword within its own block (discards any leading
    // timestamp, which sits *before* the keyword and is thus dropped).
    let (_, kw_stripped) = &blocks[kw_index];
    let after = &kw_stripped[pos + keyword.len()..];
    if !after.trim().is_empty() {
        parts.push(after.to_string());
    }

    // Fold in following blocks until the next update header or a list/components
    // section, both of which mark the end of this update's prose.
    //
    // A list (`<ul`/`<li`) always stops the fold, even before any message text
    // has been collected (so the OpenAI "Affected components" list never folds
    // in). It is therefore checked first.
    //
    // A following block is a *new update header* when its status keyword is the
    // first meaningful token (see `is_update_header`) — the shape every real
    // header uses (Claude `ts <strong>kw</strong>`, OpenAI `<b>Status: kw</b>`,
    // DeepSeek `<strong>Status:</strong> kw`). A prose block that merely mentions
    // a keyword mid-sentence is NOT a header and is folded in.
    //
    // Safeguard: when the keyword block contributed no inline after-text (parts
    // is still empty) and the very next non-list block *looks* like a header, it
    // is usually the message for an empty-after-text header (the OpenAI/DeepSeek
    // shape, e.g. `<b>Status: kw</b>` followed by `<b>Update:</b> msg`). Fold it
    // anyway instead of dropping it — parts then becomes non-empty, so the next
    // header block stops the loop normally.
    //
    // The exception is a block that is itself a *fresh emphasized stacked header*
    // (`<strong>Status:</strong> kw`): its leading emphasis wraps only a label
    // with the keyword OUTSIDE the span, marking it as a genuine older update.
    // Folding it would merge two updates, so for that shape we break instead.
    // A plain-prose message block (OpenAI/DeepSeek empty-after message) has no
    // such leading emphasized label, so it still folds.
    for (raw, stripped) in &blocks[kw_index + 1..] {
        if contains_list(raw) {
            break;
        }
        if is_update_header(stripped) {
            if parts.is_empty() && !is_emphasized_stacked_header(raw) {
                parts.push(stripped.clone());
                continue;
            }
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

/// Upper bound on how far the tag matchers scan for a closing `>`. A real tag
/// (`<p ...>`, `<b ...>`, …) is short; capping the scan keeps a crafted body
/// like `<p <p <p …` (many openers with no `>`) from degrading to O(n²) over a
/// multi-megabyte body. An opener with no `>` within the window is treated as a
/// non-tag.
const MAX_TAG_SCAN: usize = 256;

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
    // Scan for the closing '>' within a bounded window (see `MAX_TAG_SCAN`). An
    // opener with no '>' nearby is treated as a non-tag (returns None).
    let limit = (after + MAX_TAG_SCAN).min(b.len());
    let gt = b[after..limit].iter().position(|&c| c == b'>')?;
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

/// Whether a *following* block begins a new update (and so ends the current
/// update's prose). Position-based: a block is a header when its status keyword
/// is the **first meaningful token** of the block's stripped text — that is, the
/// text preceding the keyword is empty once an optional leading Statuspage
/// timestamp and an optional leading `Status:` label are removed.
///
/// This flags all three real header shapes and rejects prose:
/// - Claude   `Jun 24, 12:30 UTC Monitoring - msg` → after the timestamp, the
///   keyword leads. ✓
/// - OpenAI   `Status: Monitoring` → after the `Status:` label, the keyword
///   leads. ✓
/// - DeepSeek `Status: resolved` → likewise. ✓ (detected consistently with the
///   keyword-block scan, so stacked DeepSeek updates stop instead of merging.)
/// - Prose    `We are actively monitoring and a fix is deployed.` → the keyword
///   is mid-sentence, not first, so this is folded, not treated as a boundary.
fn is_update_header(stripped: &str) -> bool {
    let Some((pos, _)) = find_status(stripped) else {
        return false;
    };
    let before = &stripped[..pos];
    // (a) A leading Statuspage timestamp (digits + month/zone tokens) is not
    //     prose: if everything before the keyword looks like a timestamp, the
    //     keyword leads.
    if is_timestamp_like(before) {
        return true;
    }
    // (b) Otherwise allow an optional leading `Status:` label; the keyword leads
    //     iff nothing meaningful remains before it.
    strip_status_label(before).trim().is_empty()
}

/// Whether the empty-parts safeguard's candidate block is a fresh *stacked*
/// update header rather than the message for the current (empty-after) header.
///
/// Used **only** in the empty-parts branch to choose fold-vs-break. It keys on
/// where the status keyword sits relative to the block's leading `<strong>`/`<b>`
/// emphasis span:
/// - `<strong>Status:</strong> resolved` — the emphasis wraps only the `Status:`
///   label and the keyword sits OUTSIDE it. This is a genuine older stacked
///   header, so the safeguard breaks (does not merge it in).
/// - `<b>Update:</b> Traffic is recovering.` — the emphasis wraps the keyword
///   (`Update`) itself; this is a message that merely bolds its own label, so it
///   folds.
/// - plain-prose message (no leading emphasis span) — folds.
fn is_emphasized_stacked_header(raw: &str) -> bool {
    let Some(inner) = leading_emphasis_inner(raw) else {
        return false;
    };
    // The leading emphasis wraps a bare label (no keyword inside) → the keyword
    // sits outside the span → this is a stacked header.
    find_status(&strip_html(inner)).is_none()
}

/// If `raw` opens (after optional whitespace) with a `<strong>` or `<b>` span,
/// return the raw inner slice up to the matching close tag (or end of input when
/// the span is unterminated). Returns `None` when the block does not lead with
/// an emphasis span.
fn leading_emphasis_inner(raw: &str) -> Option<&str> {
    let t = raw.trim_start();
    for (name, close) in [("strong", "</strong>"), ("b", "</b>")] {
        if let Some(open_len) = emphasis_open_len(t, name) {
            let rest = &t[open_len..];
            let end = find_ascii_ci(rest.as_bytes(), close.as_bytes()).unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
    }
    None
}

/// If `s` starts with an opening `<NAME>` / `<NAME ...>` tag (ASCII
/// case-insensitive; `NAME` must be followed by `>` or whitespace so `<b>`
/// matches but `<blockquote>`/`<br>` do not), return the byte length of the
/// opener through its `>`. The scan for the closing `>` is bounded (like
/// `match_p_tag`) so a crafted opener cannot degrade to a long scan.
fn emphasis_open_len(s: &str, name: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.first() != Some(&b'<') {
        return None;
    }
    let mut i = 1;
    for &nc in name.as_bytes() {
        if i >= b.len() || (b[i] | 0x20) != (nc | 0x20) {
            return None;
        }
        i += 1;
    }
    if i >= b.len() || (b[i] != b'>' && !b[i].is_ascii_whitespace()) {
        return None;
    }
    let limit = (i + MAX_TAG_SCAN).min(b.len());
    let gt = b[i..limit].iter().position(|&c| c == b'>')?;
    Some(i + gt + 1)
}

/// Whether `s` looks like a leading Statuspage timestamp: it must contain a
/// clock time (a `:` flanked by digits, e.g. `12:30`) and consist solely of
/// characters plausibly part of a timestamp — letters (month names, `UTC`/`PDT`),
/// digits, and the punctuation `,` `:` `/` `+` `-` `.` plus whitespace.
///
/// The clock-time requirement is what keeps prose lead-ins like `By 3pm, ` or
/// `Within 5 minutes ` (a digit but no `HH:MM`) from being mistaken for a
/// timestamp, which would otherwise flag a prose block as an update header. Real
/// Statuspage timestamps ("Jun 24, 12:30 UTC") always carry a clock time and so
/// still qualify.
fn is_timestamp_like(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut has_clock = false;
    let mut prev_digit = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let ok = c.is_ascii_alphanumeric()
            || c.is_ascii_whitespace()
            || matches!(c, ',' | ':' | '/' | '+' | '-' | '.');
        if !ok {
            return false;
        }
        // A clock time is a ':' with a digit on both sides (e.g. "12:30").
        if c == ':' && prev_digit && chars.peek().is_some_and(char::is_ascii_digit) {
            has_clock = true;
        }
        prev_digit = c.is_ascii_digit();
    }
    has_clock
}

/// Strip an optional leading `Status` label (case-insensitive) followed by a
/// `:` (after optional whitespace), returning the remainder. Anything that is
/// not exactly such a label is returned unchanged.
///
/// Deliberately distinct from [`strip_leading_status_label`], do NOT merge them:
/// this header-detection helper matches *only* the literal `Status` and tolerates
/// whitespace before the colon (`Status :`), whereas the other also matches any
/// [`STATUS_KEYWORDS`] entry and requires the pre-colon word to be a single run
/// of ASCII letters (no interior whitespace). Folding them would change which
/// `before`-the-keyword prefixes [`is_update_header`] accepts.
fn strip_status_label(s: &str) -> &str {
    let t = s.trim_start();
    if let Some(head) = t.get(..6)
        && head.eq_ignore_ascii_case("status")
        && let Some(body) = t[6..].trim_start().strip_prefix(':')
    {
        return body.trim_start();
    }
    s
}

/// Collapse whitespace, drop a single leading status/update label and a single
/// leading `-`/`–`/`—`/`:` separator (and surrounding spaces), and trim.
fn clean_detail(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    // A folded empty-after-text header's message block can begin with its own
    // "Status:"/"Update:"-style label (e.g. OpenAI `<b>Update:</b> msg` →
    // "Update: msg"); strip it so the rendered detail does not leak the label.
    let unlabeled = strip_leading_status_label(trimmed);
    let stripped = unlabeled
        .strip_prefix(['-', '–', '—', ':'])
        .unwrap_or(unlabeled);
    stripped.trim().to_string()
}

/// Strip a single leading `<Word>:` label (after optional whitespace) when
/// `<Word>` is the literal `Status` or one of the [`STATUS_KEYWORDS`] (e.g.
/// `Update:`, `Monitoring:`). Conservative: an arbitrary `Word:` prefix from
/// real message prose (e.g. `Note:`) is left untouched, and a label is only
/// recognized when the word is a single run of ASCII letters.
///
/// Deliberately distinct from [`strip_status_label`], do NOT merge them: this
/// `clean_detail` helper also accepts any [`STATUS_KEYWORDS`] entry and rejects
/// whitespace before the colon, whereas the header-detection helper matches only
/// the literal `Status` and tolerates `Status :`. See that function for why the
/// split matters.
fn strip_leading_status_label(s: &str) -> &str {
    let t = s.trim_start();
    let Some(colon) = t.find(':') else {
        return s;
    };
    let word = &t[..colon];
    let is_label = !word.is_empty()
        && word.bytes().all(|b| b.is_ascii_alphabetic())
        && (word.eq_ignore_ascii_case("status")
            || STATUS_KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k)));
    if is_label {
        t[colon + ':'.len_utf8()..].trim_start()
    } else {
        s
    }
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

    #[test]
    fn retry_loop_succeeds_on_first_attempt() {
        let calls = std::cell::RefCell::new(0u32);
        let body = retry_loop(3, std::time::Duration::ZERO, || {
            *calls.borrow_mut() += 1;
            FetchAttempt::Ok(vec![1, 2, 3])
        })
        .expect("first attempt succeeds");
        assert_eq!(body, vec![1, 2, 3]);
        assert_eq!(*calls.borrow(), 1, "must not retry after success");
    }

    #[test]
    fn retry_loop_retries_then_succeeds() {
        let calls = std::cell::RefCell::new(0u32);
        let body = retry_loop(3, std::time::Duration::ZERO, || {
            let mut c = calls.borrow_mut();
            *c += 1;
            if *c < 3 {
                FetchAttempt::Retry(anyhow::anyhow!("transient"))
            } else {
                FetchAttempt::Ok(vec![9])
            }
        })
        .expect("third attempt succeeds");
        assert_eq!(body, vec![9]);
        assert_eq!(*calls.borrow(), 3);
    }

    #[test]
    fn retry_loop_exhausts_and_returns_last_error() {
        let calls = std::cell::RefCell::new(0u32);
        let err = retry_loop(3, std::time::Duration::ZERO, || {
            *calls.borrow_mut() += 1;
            FetchAttempt::Retry(anyhow::anyhow!("still transient"))
        })
        .expect_err("all attempts fail");
        assert_eq!(*calls.borrow(), 3, "must try exactly max_attempts times");
        assert!(
            format!("{err:#}").contains("still transient"),
            "last error must be surfaced: {err:#}"
        );
    }

    #[test]
    fn retry_loop_fatal_does_not_retry() {
        // A deterministic 4xx must short-circuit after a single attempt: another
        // request cannot change a 404.
        let calls = std::cell::RefCell::new(0u32);
        let err = retry_loop(3, std::time::Duration::ZERO, || {
            *calls.borrow_mut() += 1;
            FetchAttempt::Fatal(anyhow::anyhow!("404 not found"))
        })
        .expect_err("fatal error returned");
        assert_eq!(*calls.borrow(), 1, "fatal must not be retried");
        assert!(
            format!("{err:#}").contains("404"),
            "fatal error surfaced: {err:#}"
        );
    }

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
    fn extract_latest_detail_folds_keyword_in_prose_openai_shape() {
        // OpenAI/Instatus shape where the message prose itself contains a status
        // keyword word ("monitoring"). The prose block carries no emphasis tag, so
        // it must be folded in, not mistaken for a new update header and dropped.
        let html = "<b>Status: Monitoring</b><br/><br/>A fix has been deployed and we are monitoring the results.<br/><br/><b>Affected components</b><ul><li>API</li></ul>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("A fix has been deployed and we are monitoring the results.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_keyword_in_prose_deepseek_shape() {
        // DeepSeek/FlashDuty shape where the separate message block contains the
        // keyword word ("resolved"). With no emphasis tag it is prose, not a
        // header, and must be folded in.
        let html = "<p><strong>Status:</strong> resolved</p><p>The incident is resolved and service is restored.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("The incident is resolved and service is restored.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_bolded_prose_with_keyword_openai_shape() {
        // OpenAI/Instatus shape where the message block BOLDS an unrelated word
        // (`<b>actively</b>`) AND restates a status keyword ("monitoring") as an
        // ordinary mid-sentence word. The keyword is outside the emphasis span, so
        // the block is prose, not a new header: it must be folded in (not dropped),
        // and the `<ul>` components section must still be excluded.
        let html = "<b>Status: Monitoring</b><br/><br/>We are <b>actively</b> monitoring and a fix is deployed.<br/><br/><b>Affected components</b><ul><li>API</li></ul>";
        let detail = extract_latest_detail(html).expect("message must fold, not drop");
        assert!(
            detail.contains("actively monitoring"),
            "folded message lost: {detail:?}"
        );
        assert!(
            !detail.contains("Affected components"),
            "leaked list header: {detail:?}"
        );
        assert!(!detail.contains("API"), "leaked component list: {detail:?}");
        assert_eq!(detail, "We are actively monitoring and a fix is deployed.");
    }

    #[test]
    fn extract_latest_detail_folds_bolded_prose_with_keyword_deepseek_shape() {
        // DeepSeek/FlashDuty shape: the separate message block bolds a word
        // (`<b>fully</b>`) and also contains the keyword "resolved" as prose. The
        // keyword is outside the bold span, so the block folds in.
        let html = "<p><strong>Status:</strong> resolved</p><p>Service is <b>fully</b> resolved and operational.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("Service is fully resolved and operational.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_bolded_keyword_after_empty_header_openai_shape() {
        // Regression for the drop bug: an empty-after-text header
        // (`<b>Status: Monitoring</b>`) is followed by a message block that
        // itself bolds a keyword word (`<b>Update:</b> Traffic ...`). Under a
        // bold-based predicate that block was misclassified as a new header and
        // the whole message was dropped (detail = None). With position-based
        // detection plus the fold-when-parts-empty safeguard it must fold in,
        // and the trailing `<ul>` components must still be excluded.
        let html = "<b>Status: Monitoring</b><br/><br/><b>Update:</b> Traffic is recovering.<br/><br/><b>Affected components</b><ul><li>API</li></ul>";
        // Exact string pins the CHANGE 2 label-strip fix: the folded block's own
        // leading "Update:" label must be removed (no "Update: " leak) and the
        // trailing components list must be excluded.
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("Traffic is recovering.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_message_ending_in_bolded_keyword() {
        // A message that ends in a bolded keyword (`...is now <strong>resolved</strong>.`)
        // following an empty-after-text header must fold, not be dropped: the
        // keyword is the last token, not the first, so the block is prose.
        let html = "<b>Status: Monitoring</b><br/><br/>Traffic is recovering and the incident is now <strong>resolved</strong>.";
        let detail = extract_latest_detail(html).expect("message must fold, not drop");
        assert!(
            detail.starts_with("Traffic is recovering and the incident is now resolved"),
            "folded message lost or mangled: {detail:?}"
        );
    }

    #[test]
    fn extract_latest_detail_stops_at_stacked_deepseek_header_no_merge() {
        // Regression for the merge bug: two stacked DeepSeek-shape updates whose
        // headers put the keyword OUTSIDE the emphasis span
        // (`<strong>Status:</strong> kw`). Only the latest update's message is
        // surfaced; the second `Status:` header must stop the fold rather than
        // merge the older update in.
        let html = "<p><strong>Status:</strong> resolved</p><p>The service is back to normal.</p>\
<p><strong>Status:</strong> identified</p><p>We found the root cause.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("The service is back to normal.")
        );
    }

    #[test]
    fn extract_latest_detail_terse_latest_then_stacked_header_no_merge() {
        // CHANGE 3 regression: the LATEST update is terse (keyword only, no
        // message), immediately followed by an older emphasized stacked header
        // and its message. The empty-parts safeguard must NOT fold the older
        // header in (which would merge two updates): it recognizes the leading
        // emphasized `Status:` label (keyword OUTSIDE the span) as a fresh header
        // and breaks, yielding an empty -> None detail rather than a merge.
        let html = "<p><strong>Status:</strong> monitoring</p><p><strong>Status:</strong> identified</p><p>We found the cause.</p>";
        let detail = extract_latest_detail(html);
        assert!(
            detail.as_deref().is_none_or(str::is_empty),
            "terse latest update must not merge the older stacked update: {detail:?}"
        );
        let detail = detail.unwrap_or_default();
        assert!(
            !detail.contains("identified"),
            "merged older keyword: {detail:?}"
        );
        assert!(
            !detail.contains("We found the cause"),
            "merged older message: {detail:?}"
        );
    }

    #[test]
    fn is_timestamp_like_requires_a_clock_time() {
        // CHANGE 1: a real Statuspage timestamp (with HH:MM) qualifies.
        assert!(is_timestamp_like("Jun 24, 12:30 UTC "));
        assert!(is_timestamp_like("12:30"));
        // Prose lead-ins that merely contain a digit but no clock time must not.
        assert!(!is_timestamp_like("By 3pm, "));
        assert!(!is_timestamp_like("Within 5 minutes "));
        // A bare date with no time also no longer qualifies.
        assert!(!is_timestamp_like("Jun 24 "));
    }

    #[test]
    fn is_timestamp_like_prose_prefix_is_not_a_header_boundary() {
        // End-to-end: a message paragraph beginning "By 3pm, we will <keyword>"
        // must fold as prose (not be mistaken for a timestamped header), so the
        // earlier message keeps it.
        let html = "<p><strong>Monitoring</strong> - working on it.</p><p>By 3pm, we expect monitoring to confirm recovery.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("working on it. By 3pm, we expect monitoring to confirm recovery.")
        );
    }

    #[test]
    fn extract_latest_detail_folds_inline_and_following_block() {
        // Inline post-keyword prose in the keyword block PLUS a following
        // non-keyword block: both must be combined into the detail.
        let html = "<p><strong>Monitoring</strong> - first part</p><p>second part</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("first part second part")
        );
    }

    #[test]
    fn extract_latest_detail_strips_leading_colon_separator() {
        // Post-keyword text beginning with ": " has the separator stripped.
        let html = "<p><strong>Monitoring</strong>: the fix is live.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("the fix is live.")
        );
    }

    #[test]
    fn extract_latest_detail_strips_leading_en_dash_separator() {
        // Post-keyword text beginning with "– " (en-dash) has it stripped.
        let html = "<p><strong>Monitoring</strong> – the fix is live.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("the fix is live.")
        );
    }

    #[test]
    fn extract_latest_detail_keeps_pre_and_lone_br_in_keyword_block() {
        // `<pre>` must not be matched as a `<p>` split point, and a lone `<br>` is
        // deliberately not a split point, so the whole keyword block stays intact.
        let html = "<p><strong>Monitoring</strong> - text with <pre>code</pre> and a lone <br> break here.</p>";
        assert_eq!(
            extract_latest_detail(html).as_deref(),
            Some("text with code and a lone break here.")
        );
    }

    #[test]
    fn truncate_chars_returns_exact_max_unchanged() {
        // A string of EXACTLY `max` chars must be returned unchanged with no
        // ellipsis appended (guards the boundary against an off-by-one).
        let exact = "a".repeat(DETAIL_MAX_CHARS);
        let out = truncate_chars(&exact, DETAIL_MAX_CHARS);
        assert_eq!(out, exact);
        assert!(!out.ends_with('…'));
        assert_eq!(out.chars().count(), DETAIL_MAX_CHARS);
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
        // No <content>/<summary> body, so there is no message prose to extract.
        assert!(entries[0].detail.is_none());
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
