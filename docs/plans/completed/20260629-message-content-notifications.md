# Include latest status-update message in notifications

## Overview
- Today a notification body shows only the parsed status **keyword** plus the link
  (e.g. `Monitoring\n<link>`). This plan adds the latest update's **message prose**
  so the body reads like `Monitoring — A fix has been implemented and we are
  monitoring the results\n<link>`.
- Only the **latest** update is surfaced. Each notification already fires for one
  specific `(id, updated)` bump, so the newest update is exactly the new
  information. Dedup is unaffected — it keys on `(id, updated)` only.
- Integrates by extending the existing pure helpers in `src/feed.rs`
  (`Entry`, `parse_feed`, `strip_html`, status parsing) and the body composition
  in `src/notify.rs`, with `src/daemon.rs` passing the new field through.

## Context (from discovery)
- Files involved: `src/feed.rs` (Entry, parse_feed, strip_html, status parsing,
  STATUS_KEYWORDS), `src/notify.rs` (build_body), `src/daemon.rs` (process_feed +
  test Entry constructor), `tests/fixtures/*.atom`, `README.md`.
- Patterns found: pure functions factored out for network-free unit tests; tests
  co-located in `#[cfg(test)] mod tests`; fixtures under `tests/fixtures/`.
- Pre-existing limitation (not introduced here): `strip_html` passes numeric/unknown
  HTML entities through verbatim (e.g. `&#9731;`, `&nbsp;`). When authoring fixture
  message text, pick prose that doesn't surface such entities (or accept verbatim
  output) so assertions stay clean.
- **Critical finding** — the three default feeds use THREE different formats
  (verified by fetching the real feeds; DeepSeek required the project's `wreq`
  client due to a TLS-fingerprint block):

  | Feed | Container | Latest-update markup | Keyword | Message |
  |------|-----------|----------------------|---------|---------|
  | Claude (Statuspage) | `<content>` | `<p><small>ts</small><br><strong>Monitoring</strong> - msg</p>` ×N stacked newest-first | `<strong>` in 1st `<p>` | inline after `</strong> - ` |
  | OpenAI (Instatus) | `<content>` (CDATA) | `<b>Status: Monitoring</b><br/><br/>msg<br/><br/><b>Affected components</b><ul>…` | after `Status:` | after first `<br><br>` |
  | DeepSeek (FlashDuty) | `<summary>` only | `<p><strong>Status:</strong> resolved</p><p>msg (中文 + English)</p>` | 1st `<p>` | **2nd** `<p>` (separate block) |

## Development Approach
- **testing approach**: Regular (code first, then tests) — matches the existing
  co-located `#[cfg(test)]` module style.
- complete each task fully before moving to the next; small, focused changes.
- **CRITICAL: every task MUST include new/updated tests** — success and edge cases.
- **CRITICAL: all tests must pass before starting the next task.**
- run `cargo test` after each change; maintain backward compatibility (feeds that
  don't fit the model degrade to today's keyword-only output).

## Testing Strategy
- **unit tests**: required for every task; network-free pure functions only.
- **fixtures**: capture the three REAL formats (trimmed to 1–2 entries) so we test
  against reality, not assumptions.
- **e2e tests**: none — this project has no UI/e2e harness. Notification delivery
  (`notify::send`) remains a thin macOS shim and is not unit-tested, as today.

## Progress Tracking
- mark completed items with `[x]` immediately when done.
- add newly discovered tasks with ➕ prefix; blockers with ⚠️ prefix.
- keep this plan in sync with actual work.

## Solution Overview
"Full Option A" — a generic **update-block extractor**. An *update* is modeled as:
a keyword-bearing block plus the prose blocks that follow it, stopping at the next
keyword or a list/components section. This single abstraction covers all three real
formats and degrades gracefully for unknown user-added feeds.

Key design decisions:
- **Normalize** rather than pass raw text through: keyword via canonical
  `find_status`, message prose extracted separately, recombined as
  `Keyword — message` for consistent output across providers.
- **Structural stop** at `<ul>`/`<li>` (locale-independent) rather than matching
  the literal English "Affected components".
- **ASCII-case-insensitive keyword match over the original string** so the returned
  byte position stays valid for slicing (a `to_lowercase()` copy can shift offsets
  for some non-ASCII chars — DeepSeek is non-ASCII).

## Technical Details
- `Entry` gains `detail: Option<String>` (keyword-stripped message prose) and keeps
  `status: Option<String>` (canonical keyword).
- Body composition (then append `\n<link>` if a link exists):
  - status + detail → `"Monitoring — <detail>"` (separator is ` — `, em-dash with spaces)
  - detail only → `"<detail>"` *(defensive/unreachable in practice — see note)*
  - status only → `"Monitoring"` (today's behavior)
  - neither → `""`
- **Note on the "detail only" branch:** `detail` is non-`None` only when
  `extract_latest_detail` found a keyword block, which guarantees the stripped text
  also contains that keyword, so `find_status` makes `status` `Some` too. Thus
  whenever `detail` is `Some`, `status` is `Some`. The branch stays for match
  exhaustiveness but is not a real runtime mode; do not assert it as a live case.
  Reachable modes are: both, status-only, neither.
- Truncation: `detail` capped at **200 chars** (char-safe, not byte-safe), append
  `…` when cut. Cap lives inside `extract_latest_detail` so stored `detail` is bounded.
- Processing flow in `parse_feed`: pick `status_text` (content → summary, unchanged);
  `status = find_status(&strip_html(&status_text)).map(canonical)`;
  `detail = extract_latest_detail(&status_text)` (operates on raw HTML for block
  boundaries).
- **Known limitations (non-default feeds only; Statuspage/Instatus/FlashDuty
  unaffected):** `extract_latest_detail` is a heuristic that degrades gracefully on
  arbitrary user feeds. A multi-paragraph message whose *later* paragraph begins with
  a status-keyword word may be truncated at that paragraph (the trailing paragraphs are
  dropped, not folded in). The terse-latest-update-then-stacked-header merge edge is
  handled: an emphasized `Status:`-label stacked header (keyword outside the span)
  breaks the fold instead of merging.

## What Goes Where
- **Implementation Steps** (`[ ]`): all code, tests, fixtures, and README update.
- **Post-Completion** (no checkboxes): manual on-device notification spot-check.

## Implementation Steps

### Task 1: Replace `parse_status` with `find_status` (position-aware)

**Files:**
- Modify: `src/feed.rs`

- [x] add `find_status(&str) -> Option<(usize, &'static str)>` returning the earliest
      keyword by position with the canonical capitalized keyword; word-boundary
      checked via `is_word_byte`.
- [x] match keywords **ASCII-case-insensitively over the original string** (not a
      `to_lowercase()` copy) so the byte position is valid for slicing original text.
- [x] remove `parse_status`; update its in-module caller in `parse_feed` to
      `find_status(&stripped).map(|(_, k)| k.to_string())`.
- [x] port existing `parse_status` tests to `find_status`: finds keyword, earliest
      by position wins, returns `None` when absent, word-boundary required,
      non-ASCII char before keyword keeps the returned byte position valid (assert
      the slice at that position starts with the keyword).
- [x] add a test asserting the returned position points at the keyword start.
- [x] run `cargo test` — must pass before Task 2.

### Task 2: Add `extract_latest_detail` block extractor

**Files:**
- Modify: `src/feed.rs`

- [x] add `extract_latest_detail(html: &str) -> Option<String>`.
- [x] split decoded HTML into ordered blocks on `<p>`/`</p>` **and** `<br><br>`
      boundaries (treat `<br/><br/>` with optional whitespace the same); carry the
      raw block (to detect `<ul`/`<li`) plus its `strip_html`-ed text; drop empties.
      A **single** `<br>` is deliberately *not* a split point: on Claude the
      timestamp sits in `<small>ts</small><br><strong>kw</strong>`, so keeping it in
      the keyword block lets "text after the keyword" discard it. Do not also split
      on single `<br>`.
- [x] find the first block whose stripped text contains a keyword (`find_status`);
      take its text **after** the keyword.
- [x] append following blocks that contain no keyword and no `<ul`/`<li`; stop at the
      first block that contains a keyword OR `<ul`/`<li`.
- [x] clean: collapse whitespace, strip a leading `-`/`–`/`:` and surrounding spaces,
      trim; return `None` if empty.
- [x] truncate to 200 **chars** (char-safe), appending `…` when cut.
- [x] write tests using inline XML/HTML strings: stops at next keyword (no timestamp
      leak), stops before a `<ul>` section, folds in a following non-keyword block,
      no-keyword → `None`, list-immediately-after-keyword → `None`, truncation at 200
      chars appends `…` on a non-ASCII string.
- [x] run `cargo test` — must pass before Task 3.

### Task 3: Add `detail` to `Entry` and populate it in `parse_feed`

**Files:**
- Modify: `src/feed.rs`
- Modify: `src/daemon.rs`

> `Entry` is built with a full struct literal in two places — `parse_feed`
> (`src/feed.rs`) and the `entry(...)` test constructor (`src/daemon.rs`, under
> `#[cfg(test)]`). `cargo test` compiles test code, so the new field must be added
> to **both** sites in this task or the Task 3 gate fails to compile.

- [x] add `detail: Option<String>` to `Entry`.
- [x] in `parse_feed`, set `detail = extract_latest_detail(&status_text)` using the
      raw content→summary text (status source preference unchanged).
- [x] add `detail: None` to the `entry(...)` test constructor in `src/daemon.rs` so
      the `#[cfg(test)]` build compiles.
- [x] update the existing `parse_feed` field-extraction test to assert `detail`.
- [x] run `cargo test` — must pass before Task 4.

### Task 4: Add real-format fixtures and `parse_feed` coverage

**Files:**
- Create: `tests/fixtures/openai.atom`
- Create: `tests/fixtures/deepseek.atom`
- Create: `tests/fixtures/claude_stacked.atom`
- Modify: `src/feed.rs`

- [x] add `openai.atom` (CDATA `<b>Status: …</b>` + `<br><br>` + `Affected
      components` `<ul>`), trimmed to 1 entry.
- [x] add `deepseek.atom` (`<summary>`-only, two `<p>`: status / bilingual message),
      trimmed to 1 entry.
- [x] add `claude_stacked.atom` (`<content>` with 3 `<p>` updates newest-first).
- [x] add `parse_feed` tests asserting `detail` for each fixture: Claude → latest
      update only (stops at "Identified"); OpenAI → message without "Affected
      components"/list; DeepSeek → bilingual message folded from 2nd `<p>` (exercises
      the summary fallback).
- [x] run `cargo test` — must pass before Task 5.

### Task 5: Thread `detail` through `build_body` and `process_feed`

**Files:**
- Modify: `src/notify.rs`
- Modify: `src/daemon.rs`

- [x] change `build_body` to `build_body(status: Option<&str>, detail: Option<&str>,
      link: Option<&str>) -> String` with the four compositions (` — ` separator) and
      optional `\n<link>` suffix.
- [x] update `process_feed` to pass `entry.detail.as_deref()`.
- [x] add the new `detail` field to the `entry(...)` test constructor in
      `daemon.rs` tests. (Note: already present from Task 3 — verified, not duplicated.)
- [x] update/extend `build_body` tests: the reachable modes (both / status-only /
      neither) each with and without a link, plus one defensive test for the
      unreachable detail-only branch (asserting match totality, not a live case).
- [x] run `cargo test` — must pass before Task 6.

### Task 6: Verify acceptance criteria
- [x] verify body now shows `Keyword — message` for all three default feed formats
      (via the fixture-backed tests). Added end-to-end test
      `build_body_renders_keyword_message_for_all_default_feed_formats` in
      `src/feed.rs` combining each fixture's parsed status+detail through
      `build_body`; pairs with the existing per-fixture `parse_feed` and `build_body`
      tests.
- [x] verify graceful fallbacks: keyword-only and detail-only and neither (covered by
      `build_body_status_only_*`, `build_body_neither_*`, and
      `build_body_detail_only_is_total` in `src/notify.rs`).
- [x] run full suite: `cargo test` — 58 passed.
- [x] run `cargo clippy --all-targets` (clean) and `cargo fmt --check` (clean).

### Task 7: [Final] Update documentation
- [x] update `README.md` "How it works" — body now shows the status keyword **and
      the latest update's message** plus the link.
- [x] move this plan to `docs/plans/completed/`.

## Post-Completion
*Items requiring manual intervention — no checkboxes, informational only.*

**Manual verification**:
- Install the LaunchAgent and spot-check a live or recent incident notification on
  device to confirm the message renders well within the macOS banner (the OS may
  visually truncate before the 200-char cap; that is expected).
