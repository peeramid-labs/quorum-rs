//! Resolve a quoted citation back to the exact span of the source it quotes.
//!
//! An evaluator that assesses a claim is asked to quote the source **verbatim**.
//! Resolution takes that quote and finds the span it came from, returning the
//! original text and its offsets — so a claim can be located, highlighted, and
//! re-checked by anyone holding the same two strings.
//!
//! Pure string logic — no async, no I/O, no agent types. Resolution is a total
//! function of two strings, so it can be replayed over a stored record to
//! re-derive exactly what the runtime decided.
//!
//! ```
//! # use quorum_rs::agents::cite::resolve_cite;
//! let source = "The algorithm **sorts in O(n log n) time** and is stable.";
//! let span = resolve_cite(source, "sorts in O(n log n) TIME");
//! // Emphasis and case differ; the span is still the original text.
//! assert_eq!(span.map(|s| s.text).as_deref(), Some("sorts in O(n log n) time"));
//! ```
//!
//! # The normalisation ladder — the contract
//!
//! A cite is tried against the source in rungs, most literal first. The first
//! rung that hits wins. Implementers porting this to another language should
//! The regression corpus in `tests/fixtures/cite_vectors.json` pins them.
//!
//! For each *de-decorated candidate form* of the cite (below), in order:
//!
//! 1. **Exact substring.** Byte-for-byte.
//! 2. **Normalised substring.** Both sides are normalised, the match is found in
//!    normalised space, then mapped back to original offsets.
//!
//! ## De-decoration applied to the cite
//!
//! Tried in this order, always including the untouched trimmed original:
//!
//! 1. the trimmed cite as submitted;
//! 2. leading blockquote (`>`), list bullets (`-`, `*`, `•`) and ordered-list
//!    markers (`1.`, `2)`) stripped per line, lines rejoined with a space;
//! 3. everything after a `Label: ` / `Label — ` / `Label - ` / `Label – ` prefix;
//! 4. one layer of matched wrapping quotes removed from each of the above:
//!    `"…"`, `'…'`, `“…”`, `‘…’`, `«…»`, `` `…` ``.
//!
//! ## Normalisations applied to BOTH sides
//!
//! 1. **Markdown emphasis markers dropped.** A run of `*` is removed when it
//!    hugs text on at least one side (`**bold**`, `*italic*`). Emphasis is
//!    frequently in the source rather than the quote, so both sides are
//!    normalised.
//! 2. **Whitespace runs collapsed** to a single space, and the ends trimmed.
//!    Covers re-wrapped quotes and newline differences.
//! 3. **Case folded** (Unicode lowercase). Covers a re-typed leading capital.
//!
//! ## Deliberately NOT normalised
//!
//! These are omitted because each would let a cite resolve to text the author
//! did not write. Rejecting a real quote is a smaller harm than accepting a
//! fabricated one.
//!
//! - **Underscores.** `_` is an identifier character (`claim_id`) far more often
//!   than an emphasis marker; stripping it would conflate distinct symbols.
//! - **Asterisks surrounded by whitespace.** `a * b` is arithmetic, not emphasis.
//! - **Punctuation and quote characters *inside* a quote.** Only a matched
//!   *wrapping* pair is stripped; interior punctuation is significant.
//! - **Unicode width/confusable folding** (NFKC and friends). A non-breaking
//!   hyphen and a hyphen-minus stay distinct. This has not been observed to
//!   cause false rejections, and folding them risks equating characters the
//!   author chose deliberately.
//! - **Ellipsis elision.** A cite containing `...` in place of omitted words
//!   does not resolve, and must not: the elided material is exactly where a
//!   quote's meaning gets changed.
//! - **Stemming, synonyms, paraphrase, translation.** Out of scope by
//!   construction — a paraphrase is what verbatim citation exists to exclude.
//!
//! # Invariants
//!
//! - The returned [`CiteSpan::text`] is **always the original source text**,
//!   never the normalised form. Two cites of one sentence that differ only in
//!   decoration resolve to the same span, which is what lets independent
//!   evaluations of the same claim be grouped.
//! - `source[span.start..span.end] == span.text` always holds.
//! - Emphasis markers around a match are excluded from the span: the span
//!   covers the words, not the markup.
//! - An empty or whitespace-only cite never resolves.

/// A resolved citation: where in the source the quote came from.
///
/// # Offsets
///
/// `start` and `end` are **byte offsets** into the source string that was
/// passed to [`resolve_cite`], as is conventional for Rust `str` slicing:
/// `&source[start..end]` is valid and equals [`text`](Self::text).
///
/// Byte offsets are *not* what every client counts in. A browser measures a
/// DOM text node in UTF-16 code units, and Python in Unicode scalar values, so
/// slicing with these numbers there will silently misplace highlights as soon as the
/// source contains non-ASCII — which cited prose routinely does (typographic
/// dashes, curly quotes, accented names). Convert explicitly with
/// [`utf16_range`](Self::utf16_range) or [`char_range`](Self::char_range)
/// rather than assuming the units match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteSpan {
    /// Byte offset of the span start in the source.
    pub start: usize,
    /// Byte offset one past the span end in the source.
    pub end: usize,
    /// The exact original source text of the span, decoration excluded.
    pub text: String,
}

impl CiteSpan {
    /// The span as `(start, end)` in **UTF-16 code units** — what a browser
    /// needs to select a range in a DOM text node.
    ///
    /// `source` must be the same string the span was resolved against.
    ///
    /// ```
    /// # use quorum_rs::agents::cite::resolve_cite;
    /// // U+2011 NON-BREAKING HYPHEN is 3 bytes but 1 UTF-16 unit.
    /// let source = "a war\u{2011}criminal state was alleged";
    /// let span = resolve_cite(source, "state was alleged");
    /// assert_eq!(span.as_ref().map(|s| s.start), Some(17), "bytes");
    /// assert_eq!(span.map(|s| s.utf16_range(source)), Some((15, 32)));
    /// ```
    pub fn utf16_range(&self, source: &str) -> (usize, usize) {
        let start = source[..self.start].encode_utf16().count();
        (
            start,
            start + source[self.start..self.end].encode_utf16().count(),
        )
    }

    /// The span as `(start, end)` in **Unicode scalar values** (`char`s) — what
    /// a Python or Go client indexing runes needs.
    ///
    /// `source` must be the same string the span was resolved against.
    pub fn char_range(&self, source: &str) -> (usize, usize) {
        let start = source[..self.start].chars().count();
        (start, start + source[self.start..self.end].chars().count())
    }
}

/// Resolve `cite` to the span of `source` it quotes, or `None` if it quotes
/// nothing there.
///
/// See the [crate docs](crate) for the normalisation ladder this applies and
/// the normalisations it deliberately withholds.
pub fn resolve_cite(source: &str, cite: &str) -> Option<CiteSpan> {
    for cand in candidates(cite) {
        if cand.is_empty() {
            continue;
        }
        if let Some(idx) = source.find(&cand) {
            return Some(span_of(source, idx, idx + cand.len()));
        }
        if let Some(span) = find_normalized(source, &cand) {
            return Some(span);
        }
    }
    None
}

/// Whether `cite` resolves to a span of `source`.
pub fn cite_resolves(source: &str, cite: &str) -> bool {
    resolve_cite(source, cite).is_some()
}

fn span_of(source: &str, start: usize, end: usize) -> CiteSpan {
    let (start, end) = trim_emphasis_edges(source, start, end);
    CiteSpan {
        start,
        end,
        text: source[start..end].to_string(),
    }
}

/// Shrink a span so it excludes markdown emphasis markers at its edges.
///
/// Without this the literal rung and the normalised rung disagree: quoting
/// `**No Tribunal**` byte-for-byte would span the markers while quoting
/// `no tribunal` would not, and the two would no longer be one key. The span
/// covers the words, never the markup.
fn trim_emphasis_edges(source: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    let bytes = source.as_bytes();
    while start < end && bytes[start] == b'*' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b'*' {
        end -= 1;
    }
    (start, end)
}

/// De-decorated candidate forms of a raw cite, most-specific first. Always
/// includes the trimmed original so an already-clean cite still matches.
fn candidates(cite: &str) -> Vec<String> {
    let mut out = Vec::new();
    let push = |s: &str, out: &mut Vec<String>| {
        let t = s.trim().to_string();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    };

    let trimmed = cite.trim();
    push(trimmed, &mut out);

    // Strip leading markdown blockquote / list markers, line by line, then rejoin.
    let deblocked: String = trimmed
        .lines()
        .map(|l| {
            l.trim_start()
                .trim_start_matches('>')
                .trim_start_matches(['-', '*', '•'])
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')'])
                .trim()
        })
        .collect::<Vec<_>>()
        .join(" ");
    push(&deblocked, &mut out);

    // `Label: "quote"` / `Label - quote` → take the part after the first separator.
    for sep in [": ", " — ", " - ", " – "] {
        if let Some(pos) = deblocked.find(sep) {
            push(&deblocked[pos + sep.len()..], &mut out);
        }
    }

    let snapshot = out.clone();
    for s in &snapshot {
        if let Some(inner) = strip_wrapping_quotes(s) {
            push(inner, &mut out);
        }
    }
    out
}

/// If `s` is wrapped in a matched pair of quotes/backticks, return the inside.
fn strip_wrapping_quotes(s: &str) -> Option<&str> {
    const PAIRS: &[(char, char)] = &[
        ('"', '"'),
        ('\'', '\''),
        ('\u{201c}', '\u{201d}'),
        ('\u{2018}', '\u{2019}'),
        ('\u{ab}', '\u{bb}'),
        ('`', '`'),
    ];
    let first = s.chars().next()?;
    let last = s.chars().next_back()?;
    for &(o, c) in PAIRS {
        if first == o && last == c && s.chars().count() >= 2 {
            return Some(&s[o.len_utf8()..s.len() - c.len_utf8()]);
        }
    }
    None
}

/// Byte ranges of markdown emphasis marker runs to drop.
///
/// A run of `*` counts as emphasis when it hugs text on at least one side. A
/// run with whitespace (or a string boundary) on BOTH sides is arithmetic, not
/// markup, and is kept — otherwise `a * b` would match a source saying `a b`.
fn emphasis_runs(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'*' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'*' {
            i += 1;
        }
        let open_side_blank = start == 0
            || s[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let close_side_blank =
            i == bytes.len() || s[i..].chars().next().is_some_and(char::is_whitespace);
        if !(open_side_blank && close_side_blank) {
            runs.push((start, i));
        }
    }
    runs
}

/// Normalise `s` for matching, recording for every output byte the byte offset
/// in the original — so a match in normalised space maps back to an exact
/// original span.
///
/// Applies, in one pass: emphasis-marker removal, whitespace-run collapsing,
/// and case folding. See the [crate docs](crate) for what is left alone.
fn normalize(s: &str) -> (String, Vec<usize>) {
    let drops = emphasis_runs(s);
    let mut out = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let mut prev_ws = false;
    let mut drop_idx = 0usize;

    for (i, ch) in s.char_indices() {
        while drop_idx < drops.len() && drops[drop_idx].1 <= i {
            drop_idx += 1;
        }
        if drop_idx < drops.len() && i >= drops[drop_idx].0 && i < drops[drop_idx].1 {
            continue;
        }
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                map.push(i);
            }
            prev_ws = true;
            continue;
        }
        // One map entry per emitted byte, all pointing at the original char's
        // offset — case folding can change a char's byte length, so the entry
        // count must follow the OUTPUT, not the input.
        for folded in ch.to_lowercase() {
            let before = out.len();
            out.push(folded);
            for _ in before..out.len() {
                map.push(i);
            }
        }
        prev_ws = false;
    }
    // Trim a trailing space we may have added.
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// Normalisation-tolerant match: find `cand` in `source` ignoring the rungs the
/// ladder allows, and return the exact original span.
fn find_normalized(source: &str, cand: &str) -> Option<CiteSpan> {
    let (src_norm, map) = normalize(source);
    let (cand_norm, _) = normalize(cand);
    if cand_norm.is_empty() {
        return None;
    }
    let idx = src_norm.find(&cand_norm)?;

    // Map the normalised match back. `map[j]` is the original offset of the char
    // that produced normalised byte `j`. The start is `map[idx]`.
    //
    // The end is taken from the LAST MATCHED byte, extended to that char's end —
    // not from the byte after the match. Reading the following char's offset
    // would swallow anything dropped in between: a match ending just before a
    // closing `**` would report a span including those markers.
    let start = *map.get(idx)?;
    let last = *map.get(idx + cand_norm.len() - 1)?;
    let end = last
        + source[last..]
            .chars()
            .next()
            .map_or(0, |c| c.len_utf8())
            .min(source.len() - last);
    Some(span_of(source, start, end))
}

// ─── Shared grounding seam ──────────────────────────────────────────────────
//
// Every evaluation runtime grounds its claim citations through [`ground_all`].
// Runtimes differ ONLY in the [`GroundingPolicy`] they pass and in how they act
// on the returned misses — never in how a cite is resolved. Adding a runtime
// means implementing [`Groundable`] (two accessors), not another matcher.

/// How many times an evaluator may be asked to re-quote unresolvable claim
/// citations before its evaluation is accepted without them.
///
/// Shared by every runtime that can re-ask, so the strict paths cannot drift
/// apart on how persistent they are. Each runtime counts against this with its
/// OWN counter — a cite budget must never be drawn from a budget that parse or
/// transport failures also draw on, or a batch that merely needs re-quoting
/// could exhaust the recovery attempts a genuinely malformed batch needs.
pub const REQUOTE_BUDGET: u32 = 2;

/// What a runtime does with a cite that resolves to no span of its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundingPolicy {
    /// Drop the offending claim and let the caller raise a retry. For runtimes
    /// with a tool-error loop: the evaluator can re-quote, so an unanchored
    /// claim never has to be accepted.
    Reject,
    /// Keep the claim as written. For one-shot runtimes with no way to ask for
    /// a re-quote, where dropping would silently destroy evaluator signal.
    Repair,
}

/// A cite that matched no span of the proposal it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedCite {
    pub target_id: String,
    pub cite: String,
}

impl std::fmt::Display for UnresolvedCite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "  • [{}] {:?}", self.target_id, self.cite)
    }
}

/// A claim assessment carrying a citation. Runtimes deserialize into different
/// claim types (internal vs MCP wire shape); this is the two-accessor bridge
/// that lets one grounding pass serve all of them.
pub trait GroundableClaim {
    /// The cite as the evaluator submitted it.
    fn cite(&self) -> &str;
    /// Cross-round claim identifier, when the evaluator supplied one.
    fn claim_id(&self) -> Option<&str>;
    /// Record a successful resolution: replace the cite with the exact original
    /// span text, and attach where it landed.
    fn set_resolved(&mut self, text: String, anchor: super::ClaimAnchor);
    /// Assign a generated identifier to a claim that arrived without one.
    fn set_claim_id(&mut self, id: String);
}

impl GroundableClaim for super::ClaimAssessment {
    fn cite(&self) -> &str {
        &self.claim
    }
    fn claim_id(&self) -> Option<&str> {
        self.claim_id.as_deref()
    }
    fn set_resolved(&mut self, text: String, anchor: super::ClaimAnchor) {
        self.claim = text;
        self.anchor = Some(anchor);
    }
    fn set_claim_id(&mut self, id: String) {
        self.claim_id = Some(id);
    }
}

impl GroundableClaim for super::mcp_tools::McpClaimAssessment {
    fn cite(&self) -> &str {
        &self.claim
    }
    fn claim_id(&self) -> Option<&str> {
        self.claim_id.as_deref()
    }
    fn set_resolved(&mut self, text: String, anchor: super::ClaimAnchor) {
        self.claim = text;
        self.anchor = Some(anchor);
    }
    fn set_claim_id(&mut self, id: String) {
        self.claim_id = Some(id);
    }
}

/// An evaluation whose claim citations can be grounded. Implemented by each
/// runtime's own evaluation item so they share [`ground_all`] rather than each
/// carrying a copy of the grounding logic.
pub trait Groundable {
    /// The claim type this runtime deserializes into.
    type Claim: GroundableClaim;
    /// Id of the candidate proposal this evaluation targets.
    fn target_id(&self) -> &str;
    /// The claim assessments to ground, in place.
    fn claims_mut(&mut self) -> &mut Vec<Self::Claim>;
}

impl Groundable for (String, super::Evaluation) {
    type Claim = super::ClaimAssessment;
    fn target_id(&self) -> &str {
        &self.0
    }
    fn claims_mut(&mut self) -> &mut Vec<Self::Claim> {
        &mut self.1.claim_assessments
    }
}

impl Groundable for super::nsed_agent::BatchEvaluationItem {
    type Claim = super::ClaimAssessment;
    fn target_id(&self) -> &str {
        &self.agent_id
    }
    fn claims_mut(&mut self) -> &mut Vec<Self::Claim> {
        &mut self.claim_assessments
    }
}

impl Groundable for super::mcp_tools::EvaluationItem {
    type Claim = super::mcp_tools::McpClaimAssessment;
    fn target_id(&self) -> &str {
        &self.target_id
    }
    fn claims_mut(&mut self) -> &mut Vec<Self::Claim> {
        &mut self.claim_assessments
    }
}

/// Ground every claim citation in `evaluations` against the candidate it
/// targets, returning the cites that matched nothing.
///
/// A resolved cite is **replaced by the exact proposal substring** it quotes, so
/// downstream consumers can locate it by plain string match and two evaluators
/// quoting the same sentence converge on the same key. An unresolved cite is
/// reported, and dropped when `policy` is [`GroundingPolicy::Reject`].
///
/// Misses are per-claim: one bad cite never invalidates its sibling claims or
/// any other evaluation in the batch.
///
/// The match corpus is the candidate's full `content` plus the leading
/// [`EVAL_THOUGHT_LIMIT`](crate::prompts::defaults::EVAL_THOUGHT_LIMIT)
/// characters of its `thought_process` — exactly what the evaluation prompt
/// inlined. Matching the whole thought process would ground quotes the
/// evaluator was never shown, and scans an unbounded body per claim.
///
/// A cite that resolves inside `content` gets a
/// [`ClaimAnchor::AnswerBody`](super::ClaimAnchor::AnswerBody) carrying UTF-16
/// offsets into that exact string, computed here — at the only place that holds
/// the string the match was made against. One that resolves only in the thought
/// window gets [`ClaimAnchor::ThoughtWindow`](super::ClaimAnchor::ThoughtWindow)
/// and no offsets, because the answer body does not contain it.
///
/// A claim that arrives without a `claim_id` is given a stable one derived from
/// its target, its resolved text and `round`. The orchestrator previously
/// fabricated a positional `claim_{idx}` label for these, which changes whenever
/// the claim order does and so cannot be linked to.
///
/// An evaluation targeting an id that is not on the board has no corpus, so its
/// claims are left untouched rather than reported or dropped.
pub fn ground_all<E: Groundable>(
    candidates: &[super::CandidateProposal],
    round: u32,
    evaluations: &mut [E],
    policy: GroundingPolicy,
) -> Vec<UnresolvedCite> {
    let corpus_by_id: std::collections::HashMap<&str, (&str, String)> = candidates
        .iter()
        .map(|c| {
            let thoughts_shown: String = c
                .proposal
                .thought_process
                .chars()
                .take(crate::prompts::defaults::EVAL_THOUGHT_LIMIT)
                .collect();
            (c.id.as_str(), (c.proposal.content.as_str(), thoughts_shown))
        })
        .collect();

    let mut unresolved = Vec::new();
    for e in evaluations.iter_mut() {
        let Some((content, thoughts)) = corpus_by_id.get(e.target_id()) else {
            continue;
        };
        let target_id = e.target_id().to_string();
        // Positions of the claims that failed to ground. Tracked by index, not
        // by text, so pruning can never remove a different claim that happens
        // to share the same string.
        let mut missed_at = Vec::new();
        for (idx, ca) in e.claims_mut().iter_mut().enumerate() {
            // A blank cite is only legitimate as a cross-round back-reference,
            // which needs a claim_id to point at. Blank AND id-less carries no
            // identity: it cannot be highlighted, and claim-convergence drops
            // it, so it is a miss rather than a silent pass.
            if ca.cite().trim().is_empty() {
                if ca.claim_id().map(str::trim).unwrap_or("").is_empty() {
                    missed_at.push((idx, String::new()));
                }
                continue;
            }
            let cite = ca.cite();
            // Answer body first: an offset into `content` is the useful result,
            // so a cite present in both must anchor there.
            let resolved = resolve_cite(content, cite)
                .map(|span| {
                    let (start_utf16, end_utf16) = span.utf16_range(content);
                    (
                        span.text,
                        super::ClaimAnchor::AnswerBody {
                            start_utf16,
                            end_utf16,
                        },
                    )
                })
                .or_else(|| {
                    resolve_cite(thoughts, cite)
                        .map(|span| (span.text, super::ClaimAnchor::ThoughtWindow))
                });
            match resolved {
                Some((text, anchor)) => {
                    if ca.claim_id().map(str::trim).unwrap_or("").is_empty() {
                        ca.set_claim_id(claim_fingerprint(&target_id, &text, round));
                    }
                    ca.set_resolved(text, anchor);
                }
                None => missed_at.push((idx, cite.to_string())),
            }
        }
        if missed_at.is_empty() {
            continue;
        }
        if policy == GroundingPolicy::Reject {
            let bad: std::collections::HashSet<usize> = missed_at.iter().map(|(i, _)| *i).collect();
            let mut idx = 0;
            e.claims_mut().retain(|_| {
                let keep = !bad.contains(&idx);
                idx += 1;
                keep
            });
        }
        unresolved.extend(missed_at.into_iter().map(|(_, cite)| UnresolvedCite {
            target_id: target_id.clone(),
            cite,
        }));
    }
    unresolved
}

/// Stable 6-hex-character identifier for a claim, derived from what the claim
/// actually is — its target, its resolved text and the round it first appeared
/// in — so the same claim yields the same id wherever it is recomputed, and a
/// reordered list does not renumber anything.
///
/// FNV-1a, spelled out rather than taken from `DefaultHasher`, whose output is
/// explicitly not stable across releases; an id that changes under a toolchain
/// upgrade would silently break cross-round tracking.
fn claim_fingerprint(target_id: &str, claim: &str, round: u32) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    eat(target_id.as_bytes());
    eat(b"\x1f");
    eat(claim.as_bytes());
    eat(b"\x1f");
    eat(&round.to_le_bytes());
    format!("{:06x}", hash & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str =
        "The algorithm sorts in O(n log n) time and is stable.\nIt uses a merge step.";

    #[test]
    fn exact_substring_resolves_to_itself() {
        let span = resolve_cite(SOURCE, "sorts in O(n log n) time").unwrap();
        assert_eq!(span.text, "sorts in O(n log n) time");
        assert_eq!(&SOURCE[span.start..span.end], span.text);
    }

    #[test]
    fn strips_common_quote_wrappers() {
        for c in [
            "\"sorts in O(n log n) time\"",
            "\u{201c}sorts in O(n log n) time\u{201d}",
            "`sorts in O(n log n) time`",
            "'sorts in O(n log n) time'",
            "\u{ab}sorts in O(n log n) time\u{bb}",
            "> sorts in O(n log n) time",
            "- sorts in O(n log n) time",
            "Claim: \"sorts in O(n log n) time\"",
        ] {
            assert_eq!(
                resolve_cite(SOURCE, c).map(|s| s.text).as_deref(),
                Some("sorts in O(n log n) time"),
                "cite {c:?} should resolve"
            );
        }
    }

    #[test]
    fn collapses_whitespace_runs_and_newlines() {
        assert_eq!(
            resolve_cite(SOURCE, "is stable. It uses a merge step.")
                .map(|s| s.text)
                .as_deref(),
            Some("is stable.\nIt uses a merge step.")
        );
    }

    #[test]
    fn resolves_through_markdown_emphasis_on_either_side() {
        // Emphasis in the SOURCE, quoted plain.
        let src = "The algorithm **sorts in O(n log n) time** and is stable.";
        let span = resolve_cite(src, "sorts in O(n log n) time").unwrap();
        assert_eq!(span.text, "sorts in O(n log n) time");
        assert_eq!(&src[span.start..span.end], span.text);

        // Emphasis in the CITE, source plain.
        assert_eq!(
            resolve_cite(SOURCE, "**sorts in O(n log n) time**")
                .map(|s| s.text)
                .as_deref(),
            Some("sorts in O(n log n) time")
        );
        // Single-asterisk italics covering part of the quote.
        assert_eq!(
            resolve_cite(SOURCE, "*sorts* in O(n log n) time")
                .map(|s| s.text)
                .as_deref(),
            Some("sorts in O(n log n) time")
        );
    }

    #[test]
    fn resolves_case_insensitively() {
        let src = "No international tribunal has formally declared it.";
        assert_eq!(
            resolve_cite(src, "no international tribunal has formally declared it.")
                .map(|s| s.text)
                .as_deref(),
            Some("No international tribunal has formally declared it.")
        );
    }

    #[test]
    fn returns_the_original_span_not_the_normalised_form() {
        let src = "It holds that **No Tribunal** has  ruled.";
        let a = resolve_cite(src, "no tribunal").expect("lowercase cite resolves");
        let b = resolve_cite(src, "**No Tribunal**").expect("emphasised cite resolves");
        assert_eq!(a.text, "No Tribunal", "original casing, markup excluded");
        assert_eq!(a, b, "differently decorated cites converge on one span");
        assert_eq!(&src[a.start..a.end], a.text);

        // Whitespace maps back to the original run, not the collapsed form.
        assert_eq!(
            resolve_cite(src, "has ruled.").map(|s| s.text).as_deref(),
            Some("has  ruled.")
        );
    }

    #[test]
    fn does_not_strip_underscores_or_spaced_asterisks() {
        assert!(!cite_resolves("the claimid field", "the claim_id field"));
        assert!(!cite_resolves("compute a b here", "compute a * b here"));
        assert_eq!(
            resolve_cite("the claim_id field", "the claim_id field")
                .map(|s| s.text)
                .as_deref(),
            Some("the claim_id field")
        );
        assert_eq!(
            resolve_cite("compute a * b here", "compute a * b here")
                .map(|s| s.text)
                .as_deref(),
            Some("compute a * b here")
        );
    }

    #[test]
    fn ellipsis_elision_does_not_resolve() {
        // The elided middle is exactly where a quote's meaning gets changed.
        assert!(!cite_resolves(
            "From a normative perspective, the pattern of documented conduct provides \
             the evidentiary basis for the claim.",
            "From a normative perspective, the pattern ... provides the evidentiary basis \
             for the claim."
        ));
    }

    #[test]
    fn fabricated_cite_does_not_resolve() {
        assert!(!cite_resolves(SOURCE, "runs in constant time"));
        assert!(!cite_resolves(SOURCE, "\"quantum entanglement\""));
        assert!(resolve_cite(SOURCE, "").is_none());
        assert!(resolve_cite(SOURCE, "   ").is_none());
    }

    #[test]
    fn span_ending_on_a_multibyte_char_is_a_char_boundary() {
        let span = resolve_cite("x café", "x  café").unwrap();
        assert_eq!(span.text, "x café");
        let span = resolve_cite("value is 5\u{20ac}\ndone", "value is 5\u{20ac} done").unwrap();
        assert_eq!(span.text, "value is 5\u{20ac}\ndone");
    }

    #[test]
    fn offsets_convert_to_utf16_and_char_units() {
        // U+2011 is 3 bytes / 1 UTF-16 unit / 1 char — byte offsets alone would
        // misplace a highlight in a browser.
        let src = "a war\u{2011}criminal state was alleged";
        let span = resolve_cite(src, "state was alleged").unwrap();
        assert_eq!(&src[span.start..span.end], "state was alleged");
        assert_eq!(span.start, 17, "byte offset");
        assert_eq!(
            span.utf16_range(src),
            (15, 32),
            "UTF-16 start trails the byte start by the 2 extra bytes of U+2011"
        );
        assert_eq!(span.char_range(src), (15, 32));
        // Bytes exceed UTF-16 units across the whole source by exactly that much.
        assert_eq!(src.len(), 32 + 2);
    }
}

#[cfg(test)]
mod grounding_tests {
    use super::*;
    use crate::agents::{CandidateProposal, ClaimAssessment, ClaimVerdict, Evaluation, Proposal};

    const CONTENT: &str = "The algorithm sorts in O(n log n) time and is stable.";
    const THOUGHTS: &str = "I considered a quicksort but the pivot is adversarial.";

    fn candidates() -> Vec<CandidateProposal> {
        vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                thought_process: THOUGHTS.to_string(),
                content: CONTENT.to_string(),
                ..Default::default()
            },
        }]
    }

    fn claim(text: &str) -> ClaimAssessment {
        ClaimAssessment {
            claim: text.to_string(),
            verdict: ClaimVerdict::Verified,
            ..Default::default()
        }
    }

    /// One `(target_id, Evaluation)` tuple — the exact shape the native
    /// runtime assembles, so this exercises its real call path.
    fn native_eval(claims: Vec<ClaimAssessment>) -> (String, Evaluation) {
        (
            "Candidate_A".to_string(),
            Evaluation {
                claim_assessments: claims,
                ..Default::default()
            },
        )
    }

    /// Reproduces the production defect: the native runtime published a cite
    /// that matches no span of the proposal it targets, and nothing noticed.
    /// Grounding must now surface it.
    #[test]
    fn native_unresolvable_cite_is_reported() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![claim(
            "no international tribunal has ruled on this",
        )])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Repair);

        assert_eq!(unresolved.len(), 1, "fabricated cite must be reported");
        assert_eq!(unresolved[0].target_id, "Candidate_A");
        assert_eq!(
            unresolved[0].cite,
            "no international tribunal has ruled on this"
        );
    }

    #[test]
    fn repair_keeps_the_unresolvable_claim() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![
            claim("**sorts in O(n log n) time**"),
            claim("runs in constant time"),
        ])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Repair);

        let claims = &evals[0].1.claim_assessments;
        assert_eq!(claims.len(), 2, "repair never drops a claim");
        assert_eq!(
            claims[0].claim, "sorts in O(n log n) time",
            "resolvable cite is replaced by the exact proposal span"
        );
        assert_eq!(claims[1].claim, "runs in constant time", "left as written");
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn reject_drops_only_the_unresolvable_claim() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![
            claim("**sorts in O(n log n) time**"),
            claim("runs in constant time"),
        ])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        let claims = &evals[0].1.claim_assessments;
        assert_eq!(claims.len(), 1, "only the bad claim is dropped");
        assert_eq!(claims[0].claim, "sorts in O(n log n) time");
        assert_eq!(unresolved.len(), 1, "retry signal is still raised");
    }

    /// A second evaluation in the same batch must not be collateral damage.
    #[test]
    fn reject_does_not_touch_a_clean_sibling_evaluation() {
        let mut cands = candidates();
        cands.push(CandidateProposal {
            id: "Candidate_B".to_string(),
            proposal: Proposal {
                content: "A hash join avoids the sort entirely.".to_string(),
                ..Default::default()
            },
        });
        let mut evals = vec![
            native_eval(vec![claim("runs in constant time")]),
            (
                "Candidate_B".to_string(),
                Evaluation {
                    claim_assessments: vec![claim("A hash join avoids the sort entirely.")],
                    ..Default::default()
                },
            ),
        ];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].target_id, "Candidate_A");
        assert!(evals[0].1.claim_assessments.is_empty());
        assert_eq!(
            evals[1].1.claim_assessments.len(),
            1,
            "clean evaluation survives a sibling's bad cite"
        );
    }

    /// Every policy resolves a decorated-but-real cite to the same span. The
    /// runtimes differ only in what they do with a MISS.
    #[test]
    fn policies_agree_on_a_resolvable_cite() {
        for policy in [GroundingPolicy::Reject, GroundingPolicy::Repair] {
            let cands = candidates();
            let mut evals = vec![native_eval(vec![claim("> \"sorts in O(n log n)  time\"")])];

            let unresolved = ground_all(&cands, 1, &mut evals, policy);

            assert!(unresolved.is_empty(), "{policy:?} must resolve this cite");
            assert_eq!(
                evals[0].1.claim_assessments[0].claim, "sorts in O(n log n) time",
                "{policy:?} must produce the exact proposal span"
            );
        }
    }

    #[test]
    fn grounds_against_the_shown_thought_window() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![claim("the pivot is adversarial")])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        assert!(unresolved.is_empty());
        assert_eq!(
            evals[0].1.claim_assessments[0].claim,
            "the pivot is adversarial"
        );
    }

    /// A cite quoting thought-process text BEYOND the window the evaluator was
    /// shown must not ground — the corpus is bounded to what was presented.
    #[test]
    fn does_not_ground_past_the_thought_window() {
        let tail = "the sentinel phrase lives here";
        let cands = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                thought_process: format!(
                    "{}{tail}",
                    "x".repeat(crate::prompts::defaults::EVAL_THOUGHT_LIMIT)
                ),
                content: CONTENT.to_string(),
                ..Default::default()
            },
        }];
        let mut evals = vec![native_eval(vec![claim(tail)])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Repair);

        assert_eq!(
            unresolved.len(),
            1,
            "beyond the shown window must not ground"
        );
    }

    /// Closes the empty-cite bypass: a blank claim with no `claim_id` carries no
    /// identity at all — it cannot be grounded, cannot be highlighted, and is
    /// dropped from claim-convergence downstream. Treat it as unresolvable.
    #[test]
    fn blank_claim_without_claim_id_is_unresolvable() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![claim("   ")])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        assert_eq!(unresolved.len(), 1, "identity-less claim must be reported");
        assert!(evals[0].1.claim_assessments.is_empty());
    }

    /// A blank claim WITH a `claim_id` is a legitimate cross-round
    /// back-reference — there is nothing to ground, and it must survive.
    #[test]
    fn blank_claim_with_claim_id_is_a_backreference() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![ClaimAssessment {
            claim_id: Some("a1b2c3".to_string()),
            claim: String::new(),
            verdict: ClaimVerdict::Contested,
            ..Default::default()
        }])];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        assert!(unresolved.is_empty(), "back-reference is not a bad cite");
        assert_eq!(evals[0].1.claim_assessments.len(), 1);
    }

    // ── Anchors: offsets a client highlights by ─────────────────────────────

    /// Slice `content` by a UTF-16 anchor the way a JS client would.
    fn slice_utf16(content: &str, start: usize, end: usize) -> String {
        let units: Vec<u16> = content.encode_utf16().collect();
        String::from_utf16(&units[start..end]).expect("anchor slices on a boundary")
    }

    fn anchor_of(ca: &ClaimAssessment) -> (usize, usize) {
        match ca.anchor.as_ref().expect("claim is anchored") {
            crate::agents::ClaimAnchor::AnswerBody {
                start_utf16,
                end_utf16,
            } => (*start_utf16, *end_utf16),
            other => panic!("expected an answer-body anchor, got {other:?}"),
        }
    }

    #[test]
    fn emitted_offsets_slice_to_the_emitted_cite() {
        for (content, submitted) in [
            // plain
            (
                "The system sorts in O(n log n) time.",
                "sorts in O(n log n) time",
            ),
            // emphasis in the source, quoted plain
            (
                "The system **sorts in O(n log n) time** here.",
                "sorts in O(n log n) time",
            ),
            // emphasis in the cite, source plain
            (
                "The system sorts in O(n log n) time.",
                "**sorts in O(n log n) time**",
            ),
            // case drift
            ("No tribunal has ruled on it.", "no tribunal has ruled"),
            // quote wrapper + whitespace drift
            ("It is stable.\nIt merges.", "\"It is stable. It merges.\""),
        ] {
            let cands = vec![CandidateProposal {
                id: "Candidate_A".to_string(),
                proposal: Proposal {
                    content: content.to_string(),
                    ..Default::default()
                },
            }];
            let mut evals = vec![native_eval(vec![claim(submitted)])];

            let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);
            assert!(unresolved.is_empty(), "cite {submitted:?} should resolve");

            let ca = &evals[0].1.claim_assessments[0];
            let (start, end) = anchor_of(ca);
            assert_eq!(
                slice_utf16(content, start, end),
                ca.claim,
                "content sliced by the anchor must equal the emitted cite \
                 (content {content:?}, submitted {submitted:?})"
            );
        }
    }

    /// Byte offsets and UTF-16 offsets diverge the moment the content is not
    /// ASCII, and cited prose routinely is not.
    #[test]
    fn anchor_offsets_are_utf16_not_bytes() {
        let content = "A war\u{2011}criminal state was alleged by critics.";
        let cands = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                content: content.to_string(),
                ..Default::default()
            },
        }];
        let mut evals = vec![native_eval(vec![claim("state was alleged")])];

        assert!(ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject).is_empty());

        let ca = &evals[0].1.claim_assessments[0];
        let (start, end) = anchor_of(ca);
        assert_eq!(slice_utf16(content, start, end), "state was alleged");

        // The same numbers read as bytes would land in the wrong place.
        let byte_start = content.find("state was alleged").unwrap();
        assert_ne!(start, byte_start, "U+2011 makes the two units disagree");
        assert_eq!(start, 15, "UTF-16 units");
        assert_eq!(byte_start, 17, "bytes — U+2011 costs 2 more");
    }

    /// A cite that only appears in the thought window must not come back with
    /// offsets into the answer body — the answer body does not contain it, so
    /// any offset would highlight the wrong text.
    #[test]
    fn thought_window_match_carries_no_answer_offsets() {
        let cands = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                content: "Final answer: 42.".to_string(),
                thought_process: "I first considered a merge step.".to_string(),
                ..Default::default()
            },
        }];
        let mut evals = vec![native_eval(vec![claim("I first considered a merge step.")])];

        assert!(ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject).is_empty());

        assert_eq!(
            evals[0].1.claim_assessments[0].anchor,
            Some(crate::agents::ClaimAnchor::ThoughtWindow),
            "no offsets, and explicit about where it matched"
        );
    }

    /// A cite present in BOTH must anchor to the answer body — that is the one
    /// a client can act on.
    #[test]
    fn answer_body_wins_when_a_cite_appears_in_both() {
        let shared = "the merge step is stable";
        let cands = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                content: format!("Final answer: {shared}."),
                thought_process: format!("I reasoned that {shared}."),
                ..Default::default()
            },
        }];
        let mut evals = vec![native_eval(vec![claim(shared)])];

        assert!(ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject).is_empty());
        anchor_of(&evals[0].1.claim_assessments[0]);
    }

    // ── Stable claim ids ────────────────────────────────────────────────────

    #[test]
    fn generated_claim_id_is_stable_and_content_derived() {
        let run = |round: u32| {
            let cands = candidates();
            let mut evals = vec![native_eval(vec![claim("sorts in O(n log n) time")])];
            ground_all(&cands, round, &mut evals, GroundingPolicy::Reject);
            evals[0].1.claim_assessments[0]
                .claim_id
                .clone()
                .expect("an id is generated")
        };
        let first = run(1);
        assert_eq!(first.len(), 6, "6 hex chars");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(first, run(1), "same inputs, same id");
        assert_ne!(first, run(2), "round participates in the identity");
    }

    /// An id the evaluator echoed back from an earlier round is preserved —
    /// that is what cross-round tracking depends on.
    #[test]
    fn an_existing_claim_id_is_never_overwritten() {
        let cands = candidates();
        let mut evals = vec![native_eval(vec![ClaimAssessment {
            claim_id: Some("deadbe".to_string()),
            claim: "sorts in O(n log n) time".to_string(),
            verdict: ClaimVerdict::Verified,
            ..Default::default()
        }])];

        ground_all(&cands, 3, &mut evals, GroundingPolicy::Reject);

        assert_eq!(
            evals[0].1.claim_assessments[0].claim_id.as_deref(),
            Some("deadbe")
        );
    }

    /// The anchor and id must survive serialization: they travel to the
    /// orchestrator inside `Evaluation`, and a span that is right in memory but
    /// lost on the wire is the same bug as never computing it.
    #[test]
    fn anchor_and_claim_id_survive_the_wire_round_trip() {
        let content = "A war\u{2011}criminal state was alleged by critics.";
        let cands = vec![CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: Proposal {
                content: content.to_string(),
                ..Default::default()
            },
        }];
        let mut evals = vec![native_eval(vec![claim("state was alleged")])];
        ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        let json = serde_json::to_string(&evals[0].1).expect("Evaluation serializes");
        assert!(
            json.contains("\"anchor\"") && json.contains("answer_body"),
            "the anchor must actually be on the wire: {json}"
        );

        let back: Evaluation = serde_json::from_str(&json).expect("round trips");
        let before = &evals[0].1.claim_assessments[0];
        let after = &back.claim_assessments[0];
        assert_eq!(after.anchor, before.anchor, "anchor survives");
        assert_eq!(after.claim_id, before.claim_id, "claim id survives");
        assert_eq!(after.claim, before.claim);

        let (start, end) = anchor_of(after);
        assert_eq!(
            slice_utf16(content, start, end),
            after.claim,
            "the property still holds after a round trip"
        );
    }

    /// A claim with no anchor serializes without the key, so a consumer can
    /// distinguish "did not resolve" from "resolved at offset 0".
    #[test]
    fn an_unanchored_claim_omits_the_field_entirely() {
        let eval = Evaluation {
            claim_assessments: vec![claim("never grounded")],
            ..Default::default()
        };
        let json = serde_json::to_string(&eval).unwrap();
        assert!(!json.contains("anchor"), "absent, not null: {json}");
    }

    /// An evaluation aimed at an id that is not on the board has no corpus to
    /// ground against; leave it untouched rather than deleting its claims.
    #[test]
    fn unknown_target_id_is_left_alone() {
        let cands = candidates();
        let mut evals = vec![(
            "Candidate_ZZ".to_string(),
            Evaluation {
                claim_assessments: vec![claim("runs in constant time")],
                ..Default::default()
            },
        )];

        let unresolved = ground_all(&cands, 1, &mut evals, GroundingPolicy::Reject);

        assert!(unresolved.is_empty());
        assert_eq!(evals[0].1.claim_assessments.len(), 1);
    }
}
