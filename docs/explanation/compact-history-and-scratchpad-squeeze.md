---
title: Compact history
order: 11
tagline: Why agents fold older tool results into a summary to survive context-window pressure.
---

# Compact history

Why agents get a self-driven mechanism to fold older tool results
into a shorter form, and what trade-offs the design accepts.

## The problem

Long-running agents accumulate tool-call history across rounds. A
single ReAct loop on a kernel-review task can reach 350k tokens of
input within four iterations: file reads dominate, and each
subsequent iteration carries every prior `tool_result` verbatim. The
SDK's downstream shrink-guard then clamps `max_tokens` to its floor
to fit the request inside the model's window; the model emits
truncated JSON; the request fails parsing and the agent enters a
tight retry loop.

Three options were considered:

- **Sliding window.** Drop the oldest N tool calls. Cheap, but loses
  the file paths and citations the agent has been reasoning over.
- **Token-bucket eviction.** Drop tool results whose serialized form
  exceeds a per-call cap. This ships as the per-call output cap;
  necessary but not sufficient — the cap stops a single call from
  blowing the budget but doesn't reclaim space already spent by
  older calls.
- **LLM-summarised fold.** Call the agent's own model to compress
  older tool results into a structured summary that REPLACES them in
  the conversation. Costs one extra LLM round-trip; preserves the
  evidence the agent has cited; the only option that reduces input
  on the next iteration.

The third option is the `compact_history` MCP tool, plus a paired
scratchpad squeeze that runs alongside it.

## What the tool does

```text
compact_history(keep_last_n_calls: int = compact_history_default_keep)
```

When the agent issues this tool call, NSED:

1. Walks the message history and identifies tool-result boundaries.
   Under `disable_native_tools`, tool outputs are rewritten into
   `User` messages prefixed with `Tool Output (` — those count as
   boundaries too.
2. Anchors the keep cut at the parent **assistant** turn of the Nth
   most-recent tool result, never at the tool message itself. If the
   cut landed on the tool message, the assistant turn that emitted
   the tool call would stay in the retained preamble while its
   tool result got folded — the next provider request would fail
   with an orphaned `tool_calls` validation error.
3. Serialises every foldable entry into a single text payload —
   native `Tool` messages, the assistant `tool_calls` that emitted
   them, AND the User-message `Tool Output (...)` rewrites used when
   `disable_native_tools` is on. (Without that last branch the
   summariser saw zero tool evidence in text-only mode.)
4. Builds a one-shot LLM call with a structured 7-section template
   (User Request and Primary Intent, Methodology & Technical
   Concepts, References and Quotes, Errors / Fixes / Learnings,
   User Messages, Pending Work, Current Work). Each section starts
   with `Capture all` / `List all` and spells out concrete
   **Look for:** signals so weaker summarisers can't get away with
   vague headlines.
5. Replaces the folded section with a synthetic compaction pair.
   Native-tool runs get a `compact_history(...)` assistant
   tool_calls + Tool message; `disable_native_tools` runs get a
   plain assistant text turn + a `Tool Output (compact_history): ...`
   user message — emitting native tool roles in a request that sets
   `tools: None` would fail provider protocol validation.
6. Appends the same summary to the agent's persistent **scratchpad**
   (with a `[compacted_history]` marker), then runs the scratchpad
   squeeze if the appended candidate has crossed
   `scratchpad_squeeze_fraction`.

## The scratchpad squeeze

Scratchpads cap at `max_scratchpad_size`. When `compact_history` (or
the context-pressure auto-invoke) appends a summary the candidate
buffer can cross the cap. The squeeze fires when length /
`max_scratchpad_size` ≥
`scratchpad_squeeze_fraction` (default 0.95):

1. Splits the scratchpad at a UTF-8 boundary 75% in.
2. Calls the model with a 9-section deliberation-aware template
   (Primary Request, Findings & Stances, Rounds & Phases, Team
   Disagreements, Decisions Made, References & Quotes, Errors &
   Fixes, Current Focus, Optional Next Step) — each section names
   exactly what to capture.
3. Returns the compressed older section concatenated with the
   verbatim recent 25%.
4. Rejects the result and falls back to the original if the squeezed
   candidate is no shorter, or still over `max_scratchpad_size` —
   pathological summariser output never gets persisted.

## Trade-offs

The design intentionally accepts:

- **One extra LLM round-trip per fold.** Same model, same provider,
  same auth path. Cost is bounded: with default
  `compact_history_default_keep = 2` and a 90%-utilization auto-trip,
  a worst-case run folds at most once per ReAct iteration.
- **Lossy by construction.** The summariser decides what to drop.
  The structured template puts file paths, line ranges, verbatim
  quotes, and identifiers in priority slots so the agent's
  *anchoring* evidence stays even when narrative around it goes.
- **Eval-phase scratchpad writes are skipped.** During
  `Evaluating` the in-memory scratchpad is a subset extracted from
  the canonical store (`extract_evaluation_sections`). Persisting
  the squeezed value back would wipe non-evaluation sections, so
  the durable write is left for the next explicit
  `update_scratchpad`.

## Auto-invocation under context pressure

Relying on the model to call `compact_history` voluntarily was the
original design. In practice weaker models forget the tool exists and
walk into the shrink-guard floor anyway. The auto-invoke path runs
*after* the system prompt and scratchpad have been merged into the
final message list, serialises `{messages, tools}` (so any active
tool schemas count toward the budget), and triggers only when **all
three** preconditions hold:

- `prompt_chars / chars_per_token >= 0.9 * context_window` — the
  90% utilisation threshold.
- `context_window > 0` — providers that don't expose the window
  return `0` from the SDK and the auto-path stays out, leaving the
  shrink-guard alone to handle overflow.
- `message_count > 4` — fewer messages (system + user + at most one
  assistant + one tool turn) leave nothing useful to fold; skipping
  avoids burning a summariser call that can't reduce input.

Two safety nets layer over the basic check:

- **Verify shrink before swap.** The auto-path serialises the full
  request shape (`serde_json::to_string(&{messages, tools})`) on both
  sides of the comparison and only replaces `messages` with
  `result.new_messages` when the post-fold serialized request is
  *strictly smaller* than the pre-fold one. Comparing the same
  envelope on both sides is what makes the check honest: a verbose
  summary that net-grows the next request is rejected, the original
  history stays, and the shrink-guard takes over instead. Without
  this guard a wordy summariser could trade a 200k-char history for
  a 220k-char fold-plus-summary and silently make things worse.
- **Stage the scratchpad commit.** The summary is appended to a
  *candidate* buffer first; the candidate is only written back to
  `scratchpad_content` (and persisted) after the squeeze either
  succeeds or returns a candidate already under
  `max_scratchpad_size`. A too-large candidate that the squeezer
  can't shrink stays unpersisted so it doesn't re-bloat the next turn.

## What this does not solve

- It does not reduce the size of any single tool call. That's the
  per-call output cap.
- It does not preserve token-perfect fidelity. A summariser hallucination
  is possible; the synthetic `compact_history` tool call leaves the
  fact of the fold visible so an evaluator can see when the agent
  was reasoning from a summary rather than the original tool output.

## Configuration knobs

Two `AgentConfig` fields, validated at config load time
(`validate_compaction_knobs`):

| Field | Default | Range |
|---|---|---|
| `scratchpad_squeeze_fraction` | `0.95` | `(0.0, 1.0]` |
| `compact_history_default_keep` | `2` | `>= 1` |

Bad values fail loud at startup rather than producing degenerate
behavior at runtime.

## Implementation surface

- `crates/quorum-rs/src/agents/nsed_agent.rs::compact_message_history`
  — the fold itself, including the assistant-boundary cut and the
  user-message tool-output detection.
- `crates/quorum-rs/src/agents/nsed_agent.rs::squeeze_scratchpad_if_full`
  — the paired scratchpad compaction.
- `crates/quorum-rs/src/agents/config.rs::AgentConfig::validate_compaction_knobs`
  — fail-fast bounds check on the two configurable knobs.
- Live integration tests (under `crates/quorum-rs/tests/openrouter_models/`)
  assert the 7-section template round-trips through real OpenRouter models.
