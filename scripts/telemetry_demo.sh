#!/usr/bin/env bash
# Hands-on inspection harness for the telemetry foundation (#309 PR1).
#
# Runs the Rust example and, when invoked with `jq`, surfaces a few
# useful queries an operator or reviewer would run over the event
# stream. No NATS server required.
#
# Usage:
#   scripts/telemetry_demo.sh              # full output
#   scripts/telemetry_demo.sh types        # list distinct event `type` tags
#   scripts/telemetry_demo.sh subjects     # list distinct NATS subjects
#   scripts/telemetry_demo.sh agent-only   # only the agent-subtree events
#   scripts/telemetry_demo.sh redaction    # only the redaction-assert block
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Make sure cargo is on PATH — rustup's default install puts it in
# ~/.cargo/bin, which is not sourced by non-interactive shells.
if ! command -v cargo >/dev/null && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck source=/dev/null
  . "$HOME/.cargo/env"
fi

# Run the demo and capture its stdout. Build failures (or any non-zero
# exit from cargo) are surfaced with the full stderr rather than
# silently swallowed — otherwise a broken example would produce an
# empty-piped pipeline that looks like a working query.
#
# Using an explicit function instead of `eval "$RUN"` avoids the
# well-known IFS / metachar-expansion footguns of `eval` on strings.
run_demo_capture() {
  local stderr_log output
  stderr_log=$(mktemp)
  if ! output=$(cargo run -q -p quorum-rs --example telemetry_demo 2>"$stderr_log"); then
    echo "telemetry_demo failed to build or run:" >&2
    cat "$stderr_log" >&2
    rm -f "$stderr_log"
    exit 1
  fi
  rm -f "$stderr_log"
  printf '%s\n' "$output"
}

require_jq() {
  command -v jq >/dev/null || { echo "jq not installed" >&2; exit 2; }
}

mode="${1:-full}"
case "$mode" in
  full)
    # No piping — forward cargo's own stdout/stderr directly so a
    # build failure is visible as-is.
    exec cargo run -q -p quorum-rs --example telemetry_demo
    ;;
  types)
    require_jq
    run_demo_capture | awk '/^\{/' | jq -r '.type' | sort -u
    ;;
  subjects)
    run_demo_capture | awk '/^# subject: /{print $3}' | sort -u
    ;;
  agent-only)
    require_jq
    run_demo_capture | awk '/^\{/' | jq -c 'select(has("agent_id"))'
    ;;
  redaction)
    # Extract the redaction block only (between its section heading and
    # the next section). Portable across BSD (macOS) and GNU awk.
    run_demo_capture | awk '
      /=== 4\. Redaction invariant ===/ { in_block = 1; next }
      /=== [0-9]+\./                    { in_block = 0 }
      in_block                          { print }
    '
    ;;
  *)
    echo "Unknown mode: $mode" >&2
    echo "Usage: $0 [full|types|subjects|agent-only|redaction]" >&2
    exit 2
    ;;
esac
