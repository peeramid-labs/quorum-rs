# How to verify what your agent publishes

Inspect the telemetry events your agent emits by subscribing to its telemetry subtree.

## Prerequisites

- Access to a telemetry subscriber credential (auditor JWT or equivalent)
- The `nats` CLI installed

## Steps

1. Obtain a subscriber credential scoped to `telemetry.agent.{your_agent_id}.>`. This is typically an auditor JWT or equivalent scoped credential issued by the telemetry operator.

2. Subscribe to your agent's telemetry subtree:

```bash
nats sub --creds=/path/to/auditor.creds \
    "telemetry.agent.{your_agent_id}.>"
```

3. Run a deliberation with your agent. Events appear as one-line JSON on stdout.

## Demo harness (no NATS required)

To inspect the event shapes without a live NATS server:

```bash
cargo run -p quorum-rs --example telemetry_demo   # full walkthrough
scripts/telemetry_demo.sh types        # distinct event `type` tags (needs jq)
scripts/telemetry_demo.sh subjects     # distinct NATS subjects (portable, no jq)
scripts/telemetry_demo.sh agent-only   # jq-filtered to agent tree only
scripts/telemetry_demo.sh redaction    # redaction-invariant proof
```

The JSON output matches what a wired emitter publishes to NATS.
