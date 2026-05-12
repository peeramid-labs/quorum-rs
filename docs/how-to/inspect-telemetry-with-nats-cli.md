# Inspect agent telemetry with the NATS CLI

Live-tail and filter agent telemetry events without standing up a
forwarder or sink. Useful while #309's full pipeline (sub-issues
[#365 forwarder](https://github.com/peeramid-labs/nsed/issues/365)
/ [#366 sinks](https://github.com/peeramid-labs/nsed/issues/366))
is still landing.

## Prerequisites

- The `nats` CLI on `$PATH` ([install](https://github.com/nats-io/natscli)).
- Optional: `jq` for filtering and pretty-printing JSON.
- An auditor JWT scoped to the agent(s) you want to read. See
  [How to verify what your agent publishes](verify-telemetry.md)
  for issuance.
- Your `nats` CLI configured with that auditor's `.creds` file:

      nats context save auditor --creds=/path/to/auditor.creds

## Tail all events from one agent

    scripts/telemetry_tail.sh CortexA

The script subscribes to `telemetry.agent.CortexA.>` and pipes
each event through `jq -c` if available. Each line is one event in
the catalog (see [event reference](../reference/telemetry.md)).

## Tail every agent's events

    scripts/telemetry_tail.sh

Subject becomes `telemetry.agent.*.>` — one stream of every
agent in your scope. Pre-deployment debugging only; in production a
forwarder (#365) handles fan-in.

## Filter by event type

The script writes one JSON object per line; pipe through `jq` to
filter:

    scripts/telemetry_tail.sh | jq 'select(.type == "llm_request_complete")'
    scripts/telemetry_tail.sh | jq 'select(.type | startswith("task_"))'
    scripts/telemetry_tail.sh | jq 'select(.type == "llm_request_failed" and .error_class != "transport")'

## Aggregate over a time window

Tail for `N` seconds, then aggregate:

    timeout 60 scripts/telemetry_tail.sh \
      | jq -s 'group_by(.type) | map({type: .[0].type, count: length})'

## Compute p95 LLM latency by provider_backend

    timeout 60 scripts/telemetry_tail.sh \
      | jq -s '
          [.[] | select(.type == "llm_request_complete")]
          | group_by(.provider_backend // "unknown")
          | map({
              backend: .[0].provider_backend,
              p95_ms: (sort_by(.latency_ms) | .[(length * 95 / 100) | floor].latency_ms),
              n: length,
            })
        '

## Spot context-overflow rate

    scripts/telemetry_tail.sh | \
      jq 'select(.type == "llm_request_failed" and .error_class == "context_overflow")
          | "agent=\(.agent_id) model=\(.provider_id) ts=\(.ts)"'

## Spot prompt-exposure detections

    scripts/telemetry_tail.sh | \
      jq 'select(.type == "prompt_exposure_detected" and .blocked == true)
          | {agent_id, terminal_tool, hit_count, sample_hits}'

## Notes

- Field naming: `null` and "field absent" are equivalent on the
  wire (the catalog uses `#[serde(skip_serializing_if = "Option::is_none")]`).
  When filtering on optional fields, use `// null` defaults: `select(.queue_wait_ms // null)`.
- The `nosess-{agent}-{uuid}` `trace_id` shape on
  `nats_connection_state` events flags the no-task-context case.
  Filter those out with `select(.job_id != null)` if you only want
  task-scoped events.
