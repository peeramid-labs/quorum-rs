---
title: About provider-reported cost
order: 20
tagline: Why the SDK carries the charge a provider reported alongside its own estimate, and why that figure is optional.
---
# About provider-reported cost

Every LLM call has two costs attached to it: the one we work out, and the one
we are charged. They are not the same number, and until recently the SDK only
ever surfaced the first.

## The estimate and the charge

The estimate is arithmetic. Take the token counts a provider returns in
`usage`, multiply by the per-million-token rate on file for that model, add
the two halves together. It is cheap, it works everywhere, and it is wrong by
however much the rate on file has drifted from what the provider actually
bills — a model repriced upstream, a discount tier, a cache hit that was
billed at a fraction, a reasoning surcharge nobody wrote down.

The charge is what the account was debited. Some gateways return it inline, in
the same `usage` object as the token counts:

```json
"usage": {
  "prompt_tokens": 6, "completion_tokens": 2, "total_tokens": 8,
  "cost": 7.434e-07,
  "cost_details": { "upstream_inference_cost": 7.434e-07 },
  "prompt_tokens_details": { "cached_tokens": 0, "cache_write_tokens": 0 }
}
```

No second request, no reconciliation job, no price list to keep current. The
exact figure arrives with the response that generated it.

## Why it has to be read from the raw JSON

`cost` and `cost_details` are gateway extensions. The OpenAI chat-completions
schema models token counts and nothing else, so
`async_openai::types::CompletionUsage` has no field for them and deserialising
into it drops them on the floor.

So [`ProviderUsage`](../../crates/quorum-rs/src/llms/provider_usage.rs) reads
them off the `serde_json::Value` the transport already has in hand — the
non-streaming body before it is deserialised, and the final `usage` chunk on
the streaming path — and rides alongside the typed response on
`ChatCompletionResult`.

## Why every field is optional

Only some backends report cost. vLLM, Together, Cloudflare Workers AI, Ollama
and every direct-to-vendor endpoint report none of it, and a simulated model
has no provider at all.

That makes `Option` load-bearing rather than decorative. A missing `cost` means
*unknown*, and a consumer that read it as `0.0` would conclude the work was
free and give it away. The distinction is preserved all the way out:

- `ChatCompletionResult::provider_usage` — per call.
- `AgentResponse::provider_usage` — summed over a ReAct loop's calls.
- `TokenUsage::reported_cost_usd` — on the proposal or evaluation that reaches
  the orchestrator, next to the token counts it belongs with.
- `llm_request_complete.reported_cost_usd` — in telemetry, beside the
  `cost_usd` estimate rather than replacing it, so the two can be compared.

Anything downstream that meters on this must therefore say what it does when
the field is absent, and say so out loud. Silently falling back to the estimate
is fine; silently falling back without a trace of having done so is how a
regression to guesswork goes unnoticed for a quarter.

## Cache accounting rides along

`cached_tokens` and `cache_write_tokens` come from the same object and are
captured on the same terms. They are not used for billing here — prompt-cache
pricing is a separate question — but they were free to read while we were
already in the JSON, and a cache-hit rate is hard to reconstruct after the
fact.
