//! Citation resolution: map an evaluator's quoted `cite` back to the EXACT span
//! it came from in the proposal, tolerating the quote decorations models add.
//!
//! Evaluators are asked to quote a claim verbatim, but they routinely wrap it —
//! `"…"`, smart quotes `“…”`, a `Label: "…"` prefix, a markdown blockquote `> …`,
//! backticks `` `…` ``, list bullets, stray whitespace/newlines. A raw substring
//! search then fails and the claim can't be highlighted. [`resolve_cite`] strips
//! those decorations and matches whitespace-tolerantly, returning the exact
//! substring of the ORIGINAL proposal (for the client to locate/highlight), or
//! `None` when the quote genuinely isn't in the proposal — the signal to reject
//! the evaluation and retry.

/// Resolve `cite` to the exact substring of `proposal` it quotes, or `None`.
///
/// Tries, in order, each de-decorated candidate of `cite`:
/// 1. exact substring of the proposal;
/// 2. whitespace-collapsed match, mapped back to the original span.
pub fn resolve_cite(proposal: &str, cite: &str) -> Option<String> {
    for cand in candidates(cite) {
        if cand.is_empty() {
            continue;
        }
        if let Some(idx) = proposal.find(&cand) {
            return Some(proposal[idx..idx + cand.len()].to_string());
        }
        if let Some(span) = find_ws_tolerant(proposal, &cand) {
            return Some(span);
        }
    }
    None
}

/// Whether `cite` resolves to a span in `proposal`.
pub fn cite_resolves(proposal: &str, cite: &str) -> bool {
    resolve_cite(proposal, cite).is_some()
}

/// Substitute each claim with the exact proposal span it resolves to, leaving
/// unresolvable (and empty) claims unchanged. Non-destructive — for evaluation
/// paths WITHOUT a retry loop (e.g. exec agents), where the MCP path's
/// reject-and-retry isn't available. Grounds what it can; never drops a claim.
pub fn substitute_resolvable(proposal: &str, claims: &mut [super::ClaimAssessment]) {
    for c in claims.iter_mut() {
        if c.claim.trim().is_empty() {
            continue;
        }
        if let Some(span) = resolve_cite(proposal, &c.claim) {
            c.claim = span;
        }
    }
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

    // Strip one layer of matched wrapping quotes/backticks from every candidate so
    // far (regular, smart, guillemets, backticks).
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
        ('“', '”'),
        ('‘', '’'),
        ('«', '»'),
        ('`', '`'),
    ];
    let first = s.chars().next()?;
    let last = s.chars().next_back()?;
    for &(o, c) in PAIRS {
        if first == o && last == c && s.chars().count() >= 2 {
            let inner = &s[o.len_utf8()..s.len() - c.len_utf8()];
            return Some(inner);
        }
    }
    None
}

/// Collapse each run of ASCII whitespace to a single space, recording for every
/// output byte the byte offset in the original — so a match in the collapsed form
/// maps back to an exact original span.
fn collapse_ws(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    let mut prev_ws = false;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                map.push(i);
            }
            prev_ws = true;
        } else {
            out.push(ch);
            // one map entry per byte of the pushed char, back to the original offset
            for k in 0..ch.len_utf8() {
                map.push(i + k);
            }
            prev_ws = false;
        }
    }
    // Trim a trailing space we may have added.
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// Whitespace-tolerant match: find `cand` in `proposal` ignoring whitespace runs,
/// return the exact original-proposal substring.
fn find_ws_tolerant(proposal: &str, cand: &str) -> Option<String> {
    let (p_norm, map) = collapse_ws(proposal);
    let (c_norm, _) = collapse_ws(cand);
    if c_norm.is_empty() {
        return None;
    }
    let idx = p_norm.find(&c_norm)?;
    // Map normalized [idx, idx+len) back to original byte offsets. `map[j]` is the
    // original offset of the char whose normalized byte is `j`, so `map[idx]` is the
    // span start and `map[end_norm]` is the start of the char AFTER the match — the
    // span end. Both are char boundaries; a match reaching the string end has no
    // following char, so fall back to `proposal.len()`. (Mapping the last matched
    // byte instead lands mid-char on a multibyte tail and panics on slice.)
    let start = *map.get(idx)?;
    let end_norm = idx + c_norm.len();
    let end = map.get(end_norm).copied().unwrap_or(proposal.len());
    Some(proposal[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPOSAL: &str =
        "The algorithm sorts in O(n log n) time and is stable.\nIt uses a merge step.";

    #[test]
    fn exact_substring_resolves_to_itself() {
        assert_eq!(
            resolve_cite(PROPOSAL, "sorts in O(n log n) time").as_deref(),
            Some("sorts in O(n log n) time")
        );
    }

    #[test]
    fn strips_common_quote_wrappers() {
        for c in [
            "\"sorts in O(n log n) time\"",
            "“sorts in O(n log n) time”",
            "`sorts in O(n log n) time`",
            "'sorts in O(n log n) time'",
            "«sorts in O(n log n) time»",
        ] {
            assert_eq!(
                resolve_cite(PROPOSAL, c).as_deref(),
                Some("sorts in O(n log n) time"),
                "cite {c:?} should resolve"
            );
        }
    }

    #[test]
    fn strips_label_prefix_and_blockquote() {
        // `Label: "quote"`
        assert_eq!(
            resolve_cite(PROPOSAL, "Claim: \"sorts in O(n log n) time\"").as_deref(),
            Some("sorts in O(n log n) time")
        );
        // markdown blockquote
        assert_eq!(
            resolve_cite(PROPOSAL, "> sorts in O(n log n) time").as_deref(),
            Some("sorts in O(n log n) time")
        );
        // blockquote + quotes
        assert_eq!(
            resolve_cite(PROPOSAL, ">  \"sorts in O(n log n) time\"").as_deref(),
            Some("sorts in O(n log n) time")
        );
    }

    #[test]
    fn whitespace_tolerant_across_newlines() {
        // Cite collapses a newline the proposal has as a real span.
        assert_eq!(
            resolve_cite(PROPOSAL, "is stable. It uses").as_deref(),
            Some("is stable.\nIt uses")
        );
    }

    #[test]
    fn substitute_resolvable_grounds_and_preserves() {
        use crate::agents::{ClaimAssessment, ClaimVerdict};
        let mk = |claim: &str, v: ClaimVerdict| ClaimAssessment {
            claim_id: None,
            claim: claim.to_string(),
            verdict: v,
            reason: None,
        };
        let mut claims = vec![
            mk("\"sorts in O(n log n) time\"", ClaimVerdict::Verified),
            mk("not in the proposal at all", ClaimVerdict::Wrong),
            mk("", ClaimVerdict::Unverified),
        ];
        substitute_resolvable(PROPOSAL, &mut claims);
        assert_eq!(
            claims[0].claim, "sorts in O(n log n) time",
            "grounded (quotes stripped)"
        );
        assert_eq!(
            claims[1].claim, "not in the proposal at all",
            "unresolvable → unchanged"
        );
        assert_eq!(claims[2].claim, "", "empty left alone");
    }

    #[test]
    fn ws_tolerant_match_ending_on_multibyte_char() {
        // The double space forces the whitespace-tolerant path (exact find fails),
        // and the match ends on `é` (2 bytes). The span end must land on a char
        // boundary — previously it mapped to the last BYTE of `é` and sliced
        // mid-char, panicking.
        assert_eq!(resolve_cite("x café", "x  café").as_deref(), Some("x café"));
        // Multibyte at the end of a longer proposal, ws-tolerant via a newline.
        assert_eq!(
            resolve_cite("value is 5€\ndone", "value is 5€ done").as_deref(),
            Some("value is 5€\ndone")
        );
    }

    #[test]
    fn fabricated_cite_does_not_resolve() {
        assert!(!cite_resolves(PROPOSAL, "runs in constant time"));
        assert!(!cite_resolves(PROPOSAL, "\"quantum entanglement\""));
        assert!(resolve_cite(PROPOSAL, "").is_none());
    }
}
