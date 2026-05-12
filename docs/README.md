# Agent SDK documentation

Documentation for developers building agents with `nsed-agent-sdk`.

## Reference

| Document | Description |
|---|---|
| [Telemetry event catalog](reference/telemetry.md) | Every event type, its fields, and what is NOT sent |
| [Sandboxed builtin tools](reference/sandboxed-tools.md) | `read_file`, `grep_search`, `pdf_query` — args, return shape, shared access policy |

## How-to

| Guide | Goal |
|---|---|
| [Verify what your agent publishes](how-to/verify-telemetry.md) | Subscribe to your agent's telemetry using an auditor JWT |
| [Inspect telemetry with the NATS CLI](how-to/inspect-telemetry-with-nats-cli.md) | Live-tail + filter events with `nats sub` and `jq` |
| [Opt out of telemetry](how-to/opt-out-telemetry.md) | Disable all telemetry emission from your agent |

## Explanation

| Article | Topic |
|---|---|
| [About telemetry design](explanation/telemetry-design.md) | Design principles, trace correlation, retention |
| [About scoped `read_file` tool](explanation/scoped-read-file.md) | Sandbox semantics for non-claude bots reading from configured roots |
| [About LLM context-window guards](explanation/llm-context-window-guards.md) | Why the shrink-guard counts tool schemas and why the SDK doesn't cap `max_tokens` |
| [About `compact_history` and scratchpad squeeze](explanation/compact-history-and-scratchpad-squeeze.md) | Why the agent gets a self-driven fold for older tool results and how the structured prompt is shaped |
