---
title: Claim citations
order: 10
tagline: How an evaluator's claim quote is grounded back to the exact proposal span it came from.
---

# About claim-citation grounding

When an evaluator assesses a proposal, each claim it flags carries a `cite` — a
quote from the proposal the claim is about. The system *grounds* that cite: it
resolves the quote back to the exact span of the proposal it came from. This
page explains why grounding exists, how it tolerates the noise models add, and
why the two evaluation paths handle a failed match differently.

## Why verbatim, and why ground at all

The user-facing client highlights each assessed claim inside the rendered
proposal. To do that it needs to *locate* the claim text as a literal substring
of the proposal. If the evaluator paraphrases ("the author argues sorting is
fast") there is nothing to highlight — no span matches. So evaluators are asked
to quote **verbatim**, and grounding replaces the submitted `cite` with the
exact proposal substring it resolves to. The client can then string-match that
value directly, with no fuzzy search.

Grounding is also a cheap **honesty check**. A cite that resolves to no span of
the proposal is either fabricated or paraphrased beyond recognition — in either
case the assessment isn't anchored to what the proposal actually says.

## Tolerating what models actually emit

Models rarely emit a clean substring. They wrap the quote: `"…"`, smart quotes
`“…”` / `‘…’`, guillemets `«…»`, backticks, a `Label: "…"` prefix, an em/en-dash
label (`Claim — …`), a markdown blockquote `> …`, or a list bullet (`- `, `* `,
`•`, `1.`). A raw substring search fails on all of these.

So resolution ([`resolve_cite`](../reference/mcp-agent-protocol.md)) tries a
sequence of *de-decorated* candidate forms of the cite — stripping those wrappers
— and, for each, an exact match followed by a whitespace-collapsed match that
maps back to the original span. The first hit wins and returns the **original**
proposal substring (so the highlight is exact even though the match was
tolerant). Only when every candidate fails does the cite count as unresolved.

## What the cite is matched against

A cite is matched against exactly **what the evaluator was shown**: the full
final solution, plus the first `EVAL_THOUGHT_LIMIT` characters of the
`thought_process`. This bound matters. Matching the *entire* thought process
would be wrong in two ways:

- **Correctness.** The evaluation prompt truncates the thought process at that
  limit. Matching beyond it could resolve a cite to reasoning the evaluator
  never saw — grounding a quote the model couldn't have made.
- **Cost.** The thought process can be large. Scanning the whole of it per
  claim, per candidate, per evaluator is unbounded work on adversarial input.

Grounding only against the shown window keeps the corpus equal to what was
presented. A cite drawn from paged-past-the-window content (fetched separately
via `read_proposal`) is not grounded — an accepted trade for the bound.

## Why the two paths diverge on a miss

There are two evaluation runtimes, and they treat an *unresolved* cite
differently — deliberately.

- **MCP path** (`nsed_evaluate`) has a tool-call retry loop. An unresolved cite
  is rejected: the tool returns an error telling the evaluator to re-quote
  verbatim, and the evaluation is re-submitted. This is the strict path — a
  claim either anchors to the proposal or it doesn't land.

- **Exec path** has no retry loop (the agent is a one-shot subprocess).
  Rejecting there would simply drop the evaluation. Instead it is
  **non-destructive** (`substitute_resolvable`): it grounds what it can and
  leaves an unresolvable claim unchanged rather than failing the whole
  evaluation. The claim is still recorded; it just won't highlight.

The divergence is not an inconsistency — it follows from whether a
reject-and-retry is available. The strict MCP behaviour is preferable where it
can be afforded; the exec path degrades gracefully where it can't.

## See also

- [MCP agent protocol](../reference/mcp-agent-protocol.md) — the `cite` field
  contract and the reject-and-retry response.
- [Middleware](middleware.md) — where deliberated content becomes a patch.
