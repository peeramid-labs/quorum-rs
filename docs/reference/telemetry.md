---
title: Telemetry catalog
order: 5
tagline: Every event type published by quorum-rs telemetry, with fields and redaction rules.
---
# Telemetry catalog

Every event type published by `quorum-rs::telemetry`.

## Subject layout

```text
telemetry.agent.{agent_id}.{event_type}
```

The `agent_id` is bound by the agent's telemetry JWT, which grants publish permission only for `telemetry.agent.{agent_id}.>`.

**Multi-endpoint deployments.** Agents publish to one or more destinations in parallel via [`TelemetryEmitterMux`]. Each endpoint owns its own NATS connection, credentials, and subject prefix. `mux.emit(&event)` fans out to every endpoint; `mux.emit_for("name", &event)` targets one. Endpoint names must be unique within the list — `TelemetryEmitterMux::new` returns `TelemetryMuxError::DuplicateNames` on collision.

Per-endpoint `subject_prefix` defaults to `telemetry.agent` when omitted; setting it explicitly takes precedence (e.g. `tenant.x.telemetry` for tenanted deployments). The telemetry JWT is bound to the prefix the agent uses, so any custom prefix must be reflected in the JWT permissions in lockstep.

## Config schema

```yaml
telemetry:
  enabled: true
  endpoints:
    - name: service        # service operator's NATS
      nats_url: nats://orchestrator.example.com:4222
      creds: /etc/nsed/agent-service.creds
      # subject_prefix omitted → defaults to telemetry.agent
    - name: own            # agent operator's own dashboard
      nats_url: nats://my-grafana-stack.local:4222
      creds: /etc/nsed/agent-own.creds
      subject_prefix: tenant.x.telemetry  # explicit override
```

An empty `endpoints` list (or block omitted) with `enabled: true` is allowed and results in **no telemetry emitted** — the mux falls through silently. Operators relying on telemetry should declare at least one endpoint.

Overlay merges replace the list wholesale — env-specific overlays must restate every endpoint, not just the additions.

## Common fields

Every agent-side event carries these fields:

| Field | Type | Description |
|---|---|---|
| `agent_id` | string | Configured agent name |
| `job_id` | string | Deliberation identifier |
| `round` | int | Round number (1-indexed) |
| `phase` | `"propose" \| "evaluate" \| "consensus_check"` | Current phase |
| `ts` | int | Unix milliseconds at emission time |
| `trace_id` | string | 32-char hex (128-bit) for cross-process correlation |

Every event also carries a serde `"type"` discriminant (e.g. `"type": "task_completed"`).

## Events

### `llm_request_start`

Fired when the agent dispatches a call to its LLM provider.

| Field | Type | Description |
|---|---|---|
| `request_id` | string | Correlates with the terminal event |
| `model` | string | Configured model id |
| `provider_id` | string | e.g. `openrouter`, `together_ai` |
| `attempt` | int | 1 for first try; 2+ on retries |
| `estimated_input_tokens` | int | Heuristic, for planning |
| `context_utilization_pct` | float | `100 × estimated_input_tokens / context_window`. `0.0` when the agent doesn't expose `context_window` |
| `recent_tool_output_bytes` | int | Running `tool_call_executed.output_bytes` total for this task so far. Pairs with the per-call values to attribute context bloat |

Not sent: message text, system prompt, tool schemas.

### `llm_request_complete`

Fired when the LLM response finishes.

| Field | Type | Description |
|---|---|---|
| `request_id` | string | Matches the prior `llm_request_start` |
| `latency_ms` | int | Wall-clock request duration |
| `ttft_ms` | int \| null | Time to first streamed token (`null` for non-streaming providers) |
| `generation_ms` | int \| null | First-token to finish (`null` for non-streaming providers) |
| `input_tokens` | int | From provider usage stats |
| `output_tokens` | int | From provider usage stats |
| `reasoning_tokens` | int | For reasoning-model families |
| `cached_tokens` | int | Prompt-cache hits |
| `cache_write_tokens` | int \| null | Prompt tokens written into the provider's cache. `null` when the backend does not report cache accounting |
| `cost_usd` | float | Our own estimate, priced from the model's configured rate against the token counts |
| `reported_cost_usd` | float \| null | What the provider said the call cost, in USD, read from the non-standard `usage.cost` field. `null` for backends that report no cost — self-hosted and direct-to-vendor endpoints never do. Where both are present, this one is the charge and `cost_usd` is the guess |
| `finish_reason` | enum | `stop` / `length` / `tool_calls` / `error` |
| `provider_backend` | string \| null | e.g. `openrouter/deepinfra` |
| `claim_assessments_emitted` | int \| null | Evaluate-phase only |
| `disagreements_emitted` | int \| null | Evaluate-phase only |
| `messages_chars` | int | Char count of the serialized input messages JSON. Tokenizer-agnostic request-size proxy; pairs with `input_tokens` to spot tokenization anomalies |
| `max_tokens_requested` | int \| null | The `max_tokens` cap the agent asked the provider for; correlates `finish_reason == length` with reactive-shrink retry paths |
| `response_chars` | int | Char count of the generated content across choices, post-strip-reasoning. Pairs with `output_tokens` |
| `tool_calls_emitted` | int | Number of tool calls the model produced in this request. Distinct from `tool_call_executed` (per-call) and `task_completed.tool_call_count` (cumulative) |
| `max_tokens_shrunk_to_floor` | bool | `true` only when the SDK's shrink-guard had to clamp `max_tokens` UP to its floor (`shrink_info.floor_used`) — the bloat post-mortem case. A non-floor adaptive shrink leaves this `false`, so dashboards `GROUP BY` this column to find genuine truncation candidates without being polluted by routine adaptive shrinks. Joins with `finish_reason == length` to surface shrink-induced truncation |
| `available_space_at_dispatch` | int \| null | Remaining context window when the call left the SDK (`context_window - estimated_input_tokens`). `null` when the provider strategy doesn't expose `context_window` |

Not sent: response content, tool call arguments, reasoning text.

### `llm_request_failed`

Fired on any call that did not return a well-formed response.

| Field | Type | Description |
|---|---|---|
| `request_id` | string | Matches the prior `llm_request_start` |
| `error_class` | enum | `transport` / `rate_limit` / `payment_required` / `server_error` / `context_overflow` / `parse` / `other` |
| `http_status` | int \| null | When applicable |
| `retry_after_ms` | int \| null | For 429 / 503 with retry hint |
| `latency_ms` | int | Up to failure point |
| `provider_id` | string | Which provider |
| `provider_backend` | string \| null | Sub-backend when the provider fans out |

Not sent: error body text, stack trace, raw provider response.

### `llm_request_stalled`

Fired every 30s while an in-flight `llm_request_start` has no terminal event. Implemented by `LlmRequestSpan`: `start()` spawns a heartbeat task; `complete()` / `fail()` notify cancellation before emitting the terminal event so no `llm_request_stalled` is published after the matching terminal. First emission is at `+30s` (not `+0s`) — a request that completes inside the first interval emits zero stalled events.

| Field | Type | Description |
|---|---|---|
| `request_id` | string | Matches the original start |
| `elapsed_ms` | int | Time since dispatch |
| `ttft_received` | bool | `false` = provider never sent a first token. Currently always `false` — providers that surface streaming partials need to wire this. |
| `last_token_ms` | int \| null | When the last token arrived. Today always `null` for the same reason. |

### `api_error`

Fired by the agent's status-server middleware on every HTTP response with `status >= 400`. Mirrors the orchestrator's `api_error` event but omits `operator_principal` — the agent dashboard is loopback-bound and not authenticated as an operator.

| Field | Type | Description |
|---|---|---|
| `http_status` | int | HTTP status returned to the client |
| `error_code` | string \| null | Stable error code when supplied by the handler; `null` for raw 4xx/5xx |
| `endpoint` | string | Matched route template (e.g. `/api/status`); falls back to raw path |
| `method` | string | HTTP method |
| `duration_ms` | int | Wall-clock from request receipt to response |

### `tool_call_executed`

Fired when the agent runs an internal tool.

| Field | Type | Description |
|---|---|---|
| `tool_name` | string | Which tool |
| `latency_ms` | int | Execution duration |
| `success` | bool | Whether the tool returned vs errored |
| `output_bytes` | int | Length of the tool result payload. Pairs with `llm_request_start.recent_tool_output_bytes` for context-bloat attribution |
| `output_tokens_estimated` | int | `ceil(output_bytes / 4)`, always populated by the built-in agent. The schema permits `null` for custom callers that genuinely have no tokenizer hint; consumers that only ingest the built-in agent's events can treat this column as guaranteed. |
| `truncated` | bool | `true` when the tool's own `max_bytes` cap clipped the result. Per-tool wrapper-dependent — currently always `false` until each tool surfaces a structured `truncated` marker |
| `paginated` | bool | `true` when the tool emitted a `next_offset` cursor. Per-tool wrapper-dependent in the same way |

Not sent: tool arguments, tool output, search queries.

### `deliberation_context_assembled`

Fired once per `propose` / `evaluate` call, recording what prior-round context the agent assembled into its prompt and whether it wrote its scratchpad. Lets a dashboard confirm a serving agent actually inspects its own past proposals/evals across rounds — the same signals `quorum smoke-test` prints in-process.

| Field | Type | Description |
|---|---|---|
| `scratchpad_loaded_chars` | int | Scratchpad characters loaded into the prompt from the persistent store |
| `scratchpad_written` | bool | `true` when the agent wrote its scratchpad during this call |
| `scratchpad_written_chars` | int | Size of the scratchpad the agent produced |
| `prior_own_proposal_included` | bool | `true` when the prior round's own proposal was fed back into the prompt |
| `prior_score_included` | bool | `true` when the prior round's own score was fed back in |
| `prior_critiques_count` | int | Number of prior-round evaluator critiques fed into the prompt |
| `candidates_count` | int | Candidates fed to the evaluate phase (`0` in propose) |
| `previous_round_matrix_included` | bool | `true` when the cross-agent previous-round matrix was included |

### `retry_loop_attempt`

Fired each time the structured-output retry loop iterates.

| Field | Type | Description |
|---|---|---|
| `attempt` | int | Current iteration |
| `reason` | enum | `empty_content` / `schema_error` / `truncated` / `hallucinated_tool` |
| `cumulative_latency_ms` | int | All attempts so far this task |
| `cumulative_cost_usd` | float | Cost burned so far across retries |
| `cumulative_input_tokens` | int | Token-budget pressure indicator |
| `cumulative_output_tokens` | int | Ditto |

Not sent: the offending text, the parse error detail.

### `task_accepted` / `task_completed` / `task_failed`

Task-level bookends.

| Field | Type | Description |
|---|---|---|
| `dispatch_delay_ms` | int | Orchestrator-publish to agent pickup |
| `task_publish_ts` | int \| null | `task_accepted` only: Unix milliseconds the orchestrator stamped on the task envelope at publish time. `null` on payloads from an older orchestrator that does not stamp a publish timestamp |
| `job_age_at_accept_ms` | int \| null | `task_accepted` only: `agent_receive_ts − task_publish_ts`. Distinguishes a fresh dispatch (sub-second) from a resurrection (minutes / hours after a container restart). `null` when `task_publish_ts` is unset |
| `queue_wait_ms` | int \| null | task_accepted to first llm_request_start (omitted until wired — see below) |
| `duration_ms` | int | End-to-end wall-clock (completed/failed only) |
| `phase_budget_remaining_ms` | int | `0` = hit the wall |
| `llm_attempts` | int \| null | Total LLM request count, 1 + retries (omitted until wired) |
| `tool_call_count` | int \| null | Internal tool calls executed (omitted until wired) |
| `pending_publish_depth` | int \| null | `> 0` at submit time = NATS backpressure (omitted until wired) |
| `failure_class` | enum | `task_failed` only: `llm_exhausted` / `tool_error` / `timeout` / `context_overflow` / `parse_retry_exhausted` / `empty_content_after_retries` |

Not sent: what the agent proposed.

#### Population status (current implementation)

The four fields below are typed `Option<u64>`/`Option<u32>` and the
worker emits `null` (omitted from the JSON via
`#[serde(skip_serializing_if = "Option::is_none")]`) until the
wiring to populate them is in place. Operators
should treat absent fields as "not yet measured", **not** as
"measured zero" — those are different signals.

| Field | Status |
|---|---|
| `queue_wait_ms` | omitted. Needs first-`llm_request_start` timestamp recorded by the agent |
| `llm_attempts` | omitted. Needs the structured-output retry counter exposed by each `NsedAgent` impl |
| `tool_call_count` | omitted. Needs `AgentResponse.tool_usage` summed per-task |
| `pending_publish_depth` | omitted. Needs `async_nats::Client` introspection at submit time |

The schema slot exists today so consumers can lock onto the contract;
once population lands the values become `Some(n)` without a wire-shape
change.

### `nats_connection_state`

Fired on every state transition of the agent's NATS client.

These events are **process-level** (no task scope), so the common
envelope's `job_id`, `round`, and `phase` are omitted. `trace_id`
keeps the catalog's 32-char lowercase-hex shape but hashes a
session-less `(agent_id, uuid_v4)` seed instead of the task
tuple — distinct events get distinct traces (so one agent's
state transitions don't alias under a single id), but consumers
parsing `trace_id` see a uniform shape across the whole catalog.

| Field | Type | Description |
|---|---|---|
| `state` | enum | `connected` / `disconnected` / `reconnecting` / `closed` |
| `reconnects_so_far` | int | Cumulative reconnect count |
| `pending_publish_depth` | int \| null | Local publish buffer depth (omitted until wired — see below) |
| `buffer_bytes` | int \| null | Local publish buffer size in bytes (omitted until wired — see below) |

#### Population status (current implementation)

`pending_publish_depth` and `buffer_bytes` are `Option<u32>` /
`Option<u64>` and the worker emits `null` (omitted from the JSON)
until `async_nats::Client::statistics()` (or equivalent
introspection) is wired. Treat
absent fields as "not yet measured", not "buffer is empty".

### `context_emergency_shrink`

Fires once per LLM call where the SDK clamped `max_tokens` UP to its
floor (typically 200). Pairs with
`llm_request_complete.max_tokens_shrunk_to_floor`: both surface the
same `shrink_info.floor_used` condition. Healthy `available > floor`
adaptive shrinks leave both unset.

| Field | Type | Description |
|---|---|---|
| `available_space` | int | True remaining headroom at the moment the SDK applied the floor (saturating non-negative). Pre-clamp value, so `0` and `199` are reported as `0`/`199` rather than collapsing to the floor — dashboards see actual context pressure |
| `requested_max` | int | `max_tokens` the agent originally asked for |
| `floor_used` | int | The 200-or-similar floor the SDK rewrote `max_tokens` to |
| `estimated_input` | int | What we tried to send to the provider |
| `context_window` | int | Model's hard context limit |
| `recent_tool_outputs` | array | Top-N (default 5) `{tool, bytes}` contributors to bloat in this task |

Not sent: tool result content, message text, prompt fragments. Only
`tool` (a public name) and `bytes` (a count) are in
`recent_tool_outputs` — content is never disclosed.

The `recent_tool_outputs` array is currently emitted empty. The
per-task running ledger is owned by `generate_structured_output` and
threaded into `react_loop` as a `&mut u64`, so structured-output
schema retries inherit the prior attempt's bloat. The top-N
attribution is added in a follow-up that reuses that same per-task
ledger.

### `claude_subprocess_spawn` / `_exit` / `_session_lock_collision`

Lifecycle events for `provider_type: claude_cli`. Emitted around each
`run_claude_attempt`. The session jsonl at
`~/.claude/projects/<munged>/<session_id>.jsonl` is treated as the
implicit lock — non-zero size before spawn means a prior run left
state that the next spawn will collide with.

`claude_subprocess_spawn`:

| Field | Type | Description |
|---|---|---|
| `session_id` | string | Stable UUID — namespaces claude's session jsonl at `~/.claude/projects/<munged_working_dir>/<session_id>.jsonl` (`/`s in the working dir replaced with `-`) |
| `lock_present_at_spawn` | bool | `true` = previous run failed to release the lock; this spawn will collide |

`claude_subprocess_exit` (correlates by `session_id`):

| Field | Type | Description |
|---|---|---|
| `session_id` | string | Same UUID as the matching spawn event |
| `exit_code` | int | Process exit code |
| `wallclock_ms` | int | Time alive |
| `session_lock_released` | bool | `true` = post-exit cleanup removed the lock; `false` = leak that will collide with the next spawn |

`claude_session_lock_collision` (fired by spawn on prior-lock discovery):

| Field | Type | Description |
|---|---|---|
| `session_id` | string | Session that collided |
| `prior_lock_age_secs` | int | mtime delta on the lock file at discovery |
| `prior_pid` | int \| null | PID parsed from the lock file when claude writes one |

Not sent: lock file contents, command line, env vars, stdout / stderr
text. Only the path-shaped `session_id` is emitted.

### `prompt_exposure_detected`

Paired with the `PromptExposureMiddleware` guardrail. Fires when the guardrail sees dictionary hits on terminal-tool content.

> **Note.** The event variant exists in the SDK schema for completeness, but `quorum-rs` does not ship a default detector that emits it. Agents can attach any [`OutputLeakDetector`](https://docs.rs/quorum-rs/latest/quorum_rs/agents/trait.OutputLeakDetector.html) implementation via `ProposerEvaluatorAgent::with_output_guard(...)`; whether this event fires depends on the chosen detector.

| Field | Type | Description |
|---|---|---|
| `terminal_tool` | string | Which terminal tool produced the scanned content |
| `blocked` | bool | `true` = guardrail rejected + forced retry; `false` = under threshold |
| `hit_count` | int | Total hits across all categories |
| `response_length_chars` | int | Length of the scanned content |
| `suspicion_score` | float | Log-scaled score: `hit_count * log2(1 + len / unit_chars)` |
| `xml_tag_hits` | int | Detections against XML-tag dictionary |
| `tool_name_hits` | int | Detections against tool-name dictionary |
| `instruction_hits` | int | Detections against instruction-phrase dictionary |
| `wrong_acronym_hits` | int | Known-wrong NSED expansions |
| `sample_hits` | string[] | Capped sample of dictionary-sourced hit labels |

Not sent: scanned response content, proposal text, evaluation justifications.

Invariant: `hit_count == xml_tag_hits + tool_name_hits + instruction_hits + wrong_acronym_hits`.

## Events NOT emitted

The following are explicitly never sent. Redaction is enforced at the type layer — the struct fields do not exist on any `TelemetryEvent` variant:

- Prompts, system instructions, personas, thinking rules
- `thought_process`
- Proposal content, evaluation justifications, claim assessments
- User queries or follow-up nudges
- Raw LLM response bodies or tool outputs
- Token strings, API keys, or secret material
- NATS credentials or session cookies
