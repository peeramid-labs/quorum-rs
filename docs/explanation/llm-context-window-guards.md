# About LLM context-window guards

Why the OpenAI-compat client's shrink-guard counts tool schemas
alongside messages, and why the SDK doesn't second-guess your
configured `max_tokens` beyond what's needed to fit the call.

## What the provider sees

For a single chat call, every OpenAI-compatible provider tokenizes:

- **Messages** — system prompt, conversation history, the current user
  turn. The provider's "input tokens" budget.
- **Tool schemas** — the JSON-schema body of every entry in the
  request's `tools` array. The provider has to send these into the
  model so it knows what tools are callable, so they consume budget
  too. Each non-trivial schema (`read_file`, `grep_search`,
  `pdf_query`) lands at a few hundred tokens.
- **Output budget** — `max_tokens` reserves space for the model's
  reply. The provider rejects with HTTP 400 when
  `input_tokens + tool_tokens + max_tokens > context_window`.

A shrink-guard that only counts message tokens under-reserves by
exactly the tool budget. With a 131k context window and ~1.9K tokens
of tools, requesting `max_tokens = 131072` ships ~131820 tokens of
intent — the provider always says no, the SDK retry loop tries again
with the same math, the propose phase eventually times out. From
the outside it looks like a hung LLM call; the fleet log shows a
tight 400-loop. This was the failure mode in #351.

## How the shrink-guard works

`OpenAICompatibleModel::chat_completion` runs one adaptive shrink
before every call:

```text
estimated_input  = (len(messages_json) + len(tools_json)) / 3
raw_available    = context_window − estimated_input
post_buffer      = raw_available − safety_buffer        (500 tokens)
final_max_tokens = max(post_buffer, SHRINK_FLOOR)       (200 tokens)
```

If `final_max_tokens < requested_max_tokens`, the SDK ships the
shrunk value and emits the shrink in telemetry. Below the 200-token
floor the SDK clamps to 200 (the provider gets *something* back to
produce a response) and emits a `context_emergency_shrink` event —
the floor case almost always produces a 200-token response that
breaks downstream JSON, and operators need the post-mortem.

The chars/3 ratio is conservative for English text — slightly tighter
than the typical 4 chars/token rule of thumb. Small over-subtraction
is harmless (the call still leaves with a non-zero output budget);
small under-subtraction is what bit us.

## Why no defensive cap on `max_tokens`

It's tempting to add a blanket cap like "halve `max_tokens` whenever
it exceeds half the context window" — the issue body suggested it.
The SDK doesn't, because:

- **Reasoning models legitimately need large output budgets.** o1,
  deepseek-r1, and Claude extended-thinking models routinely emit
  10k–40k tokens of chain-of-thought as part of the response. An
  agent configured with `max_tokens = 96k` on a 128k-context
  reasoning model is asking for a 96k response *intentionally*.
  Halving it to 48k silently truncates the model's reasoning.
- **The shrink-guard is already adaptive.** When input + tools grow,
  it shrinks `max_tokens` down to whatever fits. When input is small,
  it preserves the full configured budget. A fixed-ratio cap is less
  precise than the math the SDK already runs.
- **Hard output ceilings are model-specific.** Some providers do
  enforce a model-card output cap below `context_window` (Claude's
  classic 8k output on 200k context). The right fix for that is
  per-provider config, not a blanket ratio that misfires on every
  reasoning model.

The contract: `agent.max_tokens` is the largest output you're
willing to pay for. The SDK will fit the call inside `context_window`
on your behalf and won't trim your intent further than that.

## Why the heuristic, not real tokenization

The SDK doesn't run a tokenizer. Each provider tokenizes differently
(GPT-4 vs Claude vs Tongyi), and the SDK supports any OpenAI-compat
endpoint without per-provider plug-ins. The chars/3 ratio is
conservative across most natural-language inputs.

The `safety_buffer` (500 proactive, 100 on the reactive vLLM-400
retry) absorbs the heuristic's variance plus per-provider overhead
(system fingerprint, cache hints) that doesn't appear in our
serialized request body.

## What telemetry sees

When the shrink-guard fires, `LlmRequestComplete` carries
`max_tokens_shrunk_to_floor: bool` and `available_space_at_dispatch`.
The dedicated `ContextEmergencyShrink` event fires only on the floor
case (the bloat post-mortem), with the request's
`requested_max` / `floor_used` / `estimated_input` / `context_window`
intact so operators can reproduce the math from the event alone.
