//! Feed-related helpers: HTML stripping and status-keyword parsing.
//!
//! These are pure helpers used by the feed-parsing code (added in a later
//! task); they are factored out so they can be unit-tested without any
//! network or notification side effects.

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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
}
