#!/usr/bin/env bash
# scripts/telemetry_tail.sh — live tail of agent telemetry events.
#
# Subscribes to the agent-side telemetry tree on NATS and pretty-
# prints every event the agent emits. Useful for verifying agent
# emission wiring (PR #375) before the forwarder + sink land
# (sub-issues #365, #366).
#
# Requires: `nats` CLI on PATH (https://github.com/nats-io/natscli).
# Optional: `jq` for syntax-coloured JSON; falls back to raw output.
#
# Usage:
#   scripts/telemetry_tail.sh                          # tail all agents
#   scripts/telemetry_tail.sh CortexA                  # tail one agent
#   AGENT_ID=CortexA scripts/telemetry_tail.sh         # via env var
#
# Filtering by event type (post-tail):
#   scripts/telemetry_tail.sh | jq 'select(.type == "llm_request_complete")'
#   scripts/telemetry_tail.sh | jq 'select(.type | startswith("task_"))'
set -euo pipefail

if ! command -v nats >/dev/null 2>&1; then
  echo "error: nats CLI not on PATH" >&2
  echo "       install from https://github.com/nats-io/natscli" >&2
  exit 1
fi

agent_id="${1:-${AGENT_ID:-*}}"
subject="nsed.telemetry.agent.${agent_id}.>"

echo "# subscribing to ${subject}" >&2
echo "# Ctrl-C to stop" >&2

if command -v jq >/dev/null 2>&1; then
  exec nats sub --raw "${subject}" | jq --unbuffered -c '.'
else
  exec nats sub --raw "${subject}"
fi
