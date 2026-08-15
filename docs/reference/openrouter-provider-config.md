---
title: openrouter config
order: 8
tagline: Fields of the openrouter agent-config block for provider routing, ZDR, and web search.
---

# openrouter config

Reference for the `openrouter:` block in an agent's configuration. It carries
OpenRouter-specific request extensions — provider routing, Zero-Data-Retention,
reasoning-token handling, and the web-search plugin — injected into the request
body as `"provider": { … }` (and `plugins: [ … ]` for web search).

The block is **OpenRouter-only**: it applies when the agent's base URL is
OpenRouter. Non-OpenRouter providers ignore or reject it, so set it only for
OpenRouter agents. All fields are optional; an omitted block emits no provider
extensions.

## Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider_sort` | string | OpenRouter price-first | Provider prioritization: `throughput` \| `latency` \| `price`. |
| `zdr` | bool | unset | When `true`, restricts routing to Zero-Data-Retention endpoints (compliance for sensitive workloads). |
| `allow_fallbacks` | bool | unset (fallbacks on) | When `false`, disables automatic fallback to backup providers on primary failure — deterministic routing, hard fail over silent reroute. |
| `ignore` | string[] | `[]` | Provider slugs to exclude from routing (e.g. `["nextbit", "ionstream"]`). Injected as `provider.ignore`. |
| `only` | string[] | `[]` | Provider allowlist — route ONLY to these slugs (e.g. `["akashml/fp8"]`). Setting both `only` and `ignore` means "allowlist minus ignore". Use to pin a model to a variant with a larger advertised context window. Injected as `provider.only`. |
| `exclude_reasoning` | bool | unset | When `true`, strip reasoning tokens from the visible `content` stream (`reasoning.exclude: true`). The model still reasons at the configured effort; only the chain-of-thought portion is omitted. Use for models that dump reasoning into `content` and starve the final tool call. Switches the request to the unified `reasoning: { effort, exclude }` object (drops the legacy top-level `reasoning_effort`). |
| `web_search` | object | unset → `plugins: []` | Enables the OpenRouter web-search plugin. See [Web search](#web-search). Omitted means **no outbound network** — web access is opt-in per agent. |

## Web search

`web_search` enables OpenRouter's web-search plugin (`plugins: [{ "id": "web", … }]`).
An empty object still enables the plugin at OpenRouter's defaults (native/exa
engine, 5 results). Omitting `web_search` entirely emits `plugins: []` — the
explicit no-outbound-network default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `engine` | string | OpenRouter default | Search backend: `native` (provider built-in, e.g. OpenAI/xAI), `exa`, `firecrawl`, `parallel`, `perplexity`. `native` avoids the per-request exa surcharge on models that browse natively. |
| `max_results` | u32 | 5 | Maximum results to fetch. |
| `search_prompt` | string | OpenRouter default | Overrides the prompt prepended to the injected search results. |

## Example

```yaml
openrouter:
  provider_sort: throughput
  allow_fallbacks: false
  ignore:
    - nextbit
  only:
    - akashml/fp8
    - parasail/fp8
  exclude_reasoning: true
  web_search:
    engine: native
    max_results: 3
```

## See also

- OpenRouter provider routing: <https://openrouter.ai/docs/guides/routing/provider-selection>
- OpenRouter web-search plugin: <https://openrouter.ai/docs/guides/features/plugins/web-search>
