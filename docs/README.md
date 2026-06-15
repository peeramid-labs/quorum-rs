# quorum-rs documentation

Organized by the [Diátaxis](https://diataxis.fr) framework — four sections serving distinct user needs.

## Tutorials

Learning-by-doing material — single-path lessons with verification checkpoints at every step.

| Tutorial | What you'll have at the end |
|---|---|
| [Your first agent — from invite code to live deliberation](tutorials/first-agent-from-invite.md) | One agent running on your laptop, joined to a remote orchestrator, contributing to deliberations. ~15 minutes. |

## How-to

Goal-oriented recipes. Assume reader is already competent — get them to the answer.

| Guide | Goal |
|---|---|
| [Run an agent fleet via `quorum serve`](how-to/run-an-agent-fleet.md) | Load `agent.yml`, dispatch each entry to the right agent implementation, run the lot |
| [Agent development guide](how-to/agent-development.md) | Build a deliberation agent — reference impl, custom Rust trait, or non-Rust via exec/MCP |
| [Register a custom provider](how-to/register-a-custom-provider.md) | Make `quorum serve` recognise a `provider.type` the SDK doesn't ship — register a `ProviderFactory` without forking |
| [Redeem an invite code](how-to/redeem-invite-code.md) | Bootstrap NATS credentials from a JWT invite code, one command |
| [Verify what your agent publishes](how-to/verify-telemetry.md) | Subscribe to your agent's telemetry using an auditor JWT |
| [Inspect telemetry with the NATS CLI](how-to/inspect-telemetry-with-nats-cli.md) | Live-tail + filter events with `nats sub` and `jq` |
| [Opt out of telemetry](how-to/opt-out-telemetry.md) | Disable all telemetry emission from your agent |

## Reference

Authoritative descriptions — API surface, types, schemas.

| Document | Description |
|---|---|
| [Telemetry event catalog](reference/telemetry.md) | Every event type, its fields, and what is NOT sent |
| [Sandboxed builtin tools](reference/sandboxed-tools.md) | `read_file`, `grep_search` — args, return shape, shared access policy |
| [Chat completions API](reference/chat-completions.md) | OpenAI-compatible `/v1/chat/completions` endpoint shape |
| [Exec agent protocol](reference/exec-agent-protocol.md) | Wire protocol for stdin/stdout subprocess agents |
| [MCP agent protocol](reference/mcp-agent-protocol.md) | Wire protocol for tool-calling-aware external agents |
| [Glossary](reference/glossary.md) | Key terms used across the documentation |

Per-crate rustdoc: <https://docs.rs/quorum-rs>, <https://docs.rs/llm-repair>, <https://docs.rs/quorum-crypto-core>.

## Explanation

Discussion-mode material — why, design rationale, tradeoffs.

| Article | Topic |
|---|---|
| [Agent internals](explanation/agent-internals.md) | Library API surface + ReAct loop architecture |
| [About the provider registry](explanation/provider-registry.md) | Why `provider.type` dispatch is a registry of factories, and how a third party adds a provider without forking the SDK |
| [Agent ranking](explanation/agent-ranking.md) | Capability declaration + how agents earn rank through deliberation |
| [NATS topology](explanation/nats-topology.md) | Subjects, JetStream streams, JWT scopes |
| [Middleware system](explanation/middleware.md) | Pluggable validation / moderation pipeline + design rationale |
| [About telemetry design](explanation/telemetry-design.md) | Design principles, trace correlation, retention |
| [About scoped `read_file` tool](explanation/scoped-read-file.md) | Sandbox semantics for non-claude bots reading from configured roots |
| [About LLM context-window guards](explanation/llm-context-window-guards.md) | Why the shrink-guard counts tool schemas and why the SDK doesn't cap `max_tokens` |
| [About `compact_history` and scratchpad squeeze](explanation/compact-history-and-scratchpad-squeeze.md) | Why the agent gets a self-driven fold for older tool results and how the structured prompt is shaped |
