# quorum-rs documentation

## Tutorials

Learning-by-doing material — single-path lessons with verification checkpoints at every step.

| Tutorial | What you'll have at the end |
|---|---|
| [Your first agent — from invite code to live deliberation](tutorials/first-agent-from-invite.md) | One agent running on your laptop, joined to a remote orchestrator, contributing to deliberations. ~15 minutes. |
| [Thread a conversation](tutorials/thread-a-multi-turn-conversation.md) | A second turn the council can answer — follow-ups threaded onto a prior deliberation. |

## How-to

Goal-oriented recipes. Assume reader is already competent — get them to the answer.

| Guide | Goal |
|---|---|
| [Run an agent fleet via `quorum serve`](how-to/run-an-agent-fleet.md) | Load `quorum.yml`, dispatch each agent entry to the right implementation, run the lot |
| [Agent development guide](how-to/agent-development.md) | Build a deliberation agent — reference impl, custom Rust trait, or non-Rust via exec/MCP |
| [Register a custom provider](how-to/register-a-custom-provider.md) | Make `quorum serve` recognise a `provider.type` the SDK doesn't ship — register a `ProviderFactory` without forking |
| [Redeem an invite code](how-to/redeem-invite-code.md) | Bootstrap NATS credentials from a JWT invite code, one command |
| [Verify what your agent publishes](how-to/verify-telemetry.md) | Subscribe to your agent's telemetry using an auditor JWT |
| [Inspect telemetry with the NATS CLI](how-to/inspect-telemetry-with-nats-cli.md) | Live-tail + filter events with `nats sub` and `jq` |
| [Opt out of telemetry](how-to/opt-out-telemetry.md) | Disable all telemetry emission from your agent |
| [Register a device](how-to/register-a-device.md) | Mint an operator token from a device key pair — self-serve, no invite code or admin |
| [Smoke-test an agent](how-to/smoke-test-an-agent.md) | Verify one of your agents in three stages — chat, tool-calling, full NSED |
| [Limit job concurrency](how-to/limit-agent-job-concurrency.md) | Cap an agent to N concurrent jobs so tasks sharing mutable state can't race |
| [Expose the agent dashboard on the LAN](how-to/expose-agent-dashboard-on-lan.md) | Reach the per-agent dashboard from other hosts, and lock it down |
| [Compose a persona from shared files](how-to/compose-persona-from-shared-files.md) | Build a persona from stacked layers of shared markdown files |
| [Dump outgoing prompts](how-to/dump-outgoing-prompts.md) | Capture each agent's exact model payload to hunt prompt bloat |
| [Use the thread TUI](how-to/use-the-thread-tui.md) | Hold a branching, email-style conversation whose replies are deliberations |
| [Use the Claude Code plugin](how-to/use-the-claude-code-plugin.md) | Every quorum lifecycle command as a slash command inside Claude Code |

## Reference

Authoritative descriptions — API surface, types, schemas.

| Document | Description |
|---|---|
| [Telemetry event catalog](reference/telemetry.md) | Every event type, its fields, and what is NOT sent |
| [Sandboxed builtin tools](reference/sandboxed-tools.md) | `read_file`, `grep_search` — args, return shape, shared access policy |
| [Chat completions API](reference/chat-completions.md) | OpenAI-compatible `/v1/chat/completions` endpoint shape |
| [Exec agent protocol](reference/exec-agent-protocol.md) | Wire protocol for stdin/stdout subprocess agents |
| [MCP agent protocol](reference/mcp-agent-protocol.md) | Wire protocol for tool-calling-aware external agents |
| [Dashboard config](reference/dashboard-config.md) | Whether the agent dashboard starts, where it binds, how it authenticates |
| [`persona` yaml shapes](reference/persona-yaml-shapes.md) | The two shapes the `persona` field on an agent entry accepts |
| [`openrouter` config](reference/openrouter-provider-config.md) | Fields of the openrouter block — provider routing, ZDR, web search |
| [Thread TUI](reference/thread-tui.md) | Keys, line formats, and persisted data model of the thread TUI |
| [Task clock](reference/task-clock.md) | The date a task was issued, stated to the model rather than offered as a tool |
| [Model health](reference/model-health.md) | How an agent decides its own model is unusable, and what benches it |
| [Agent identity keys](reference/agent-identity.md) | Keys an agent declares, signatures on a held response, what is carried but not yet checked |
| [Proposal structure metadata](reference/proposal-structure.md) | The measured shape evaluators see on each candidate, and the tools that address it by line |
| [Provider-executed tools](reference/provider-tools.md) | Tools the backend runs itself; the SDK hands the arguments back |
| [Sampling parameters](reference/sampling-params.md) | Which request fields are sent, when omitted, and backends that fix them server-side |
| [Task SKIP protocol](reference/skip.md) | How an agent sits a task out as an explicit answer instead of silence |
| [Glossary](reference/glossary.md) | Key terms used across the documentation |

Per-crate rustdoc: <https://docs.rs/quorum-rs>, <https://docs.rs/llm-repair>, <https://docs.rs/quorum-crypto-core>.

## Explanation

Discussion-mode material — why, design rationale, tradeoffs.

| Article | Topic |
|---|---|
| [Agent internals](explanation/agent-internals.md) | Library API surface + ReAct loop architecture |
| [About the provider registry](explanation/provider-registry.md) | Why `provider.type` dispatch is a registry of factories, and how a third party adds a provider without forking the SDK |
| [Agent ranking](explanation/agent-ranking.md) | Capability declaration + how agents earn rank through deliberation |
| [Rooms & policies](explanation/rooms-and-policies.md) | Why a policy (who deliberates and how) is orthogonal to a room (who can watch) |
| [Policy & sessions](explanation/policy-as-model-and-sessions.md) | Why the interactive client treats policy as the model and the session as the thread |
| [About model-down benching](explanation/model-down-benching.md) | How an agent whose remote model 404s benches itself, and why a success resets it |
| [Claim citations](explanation/claim-citation-grounding.md) | How an evaluator's claim quote is grounded back to the exact proposal span |
| [Working dir override](explanation/agent-working-directory.md) | How a `before_prompt` middleware picks the directory an agent's subprocess runs in |
| [NATS topology](explanation/nats-topology.md) | Subjects, JetStream streams, JWT scopes |
| [Middleware system](explanation/middleware.md) | Pluggable validation / moderation pipeline + design rationale |
| [About telemetry design](explanation/telemetry-design.md) | Design principles, trace correlation, retention |
| [About scoped `read_file` tool](explanation/scoped-read-file.md) | Sandbox semantics for non-claude bots reading from configured roots |
| [About LLM context-window guards](explanation/llm-context-window-guards.md) | Why the shrink-guard counts tool schemas and why the SDK doesn't cap `max_tokens` |
| [About `compact_history` and scratchpad squeeze](explanation/compact-history-and-scratchpad-squeeze.md) | Why the agent gets a self-driven fold for older tool results and how the structured prompt is shaped |
| [About provider-reported cost](explanation/provider-reported-cost.md) | Why the SDK carries the charge a provider reported alongside its own estimate, and why that figure is optional |
| [Persona layer stacking](explanation/persona-layer-stacking.md) | Why personas stack in layers and why md paths resolve against process CWD |
| [Thread TUI roadmap](explanation/thread-tree-tui.md) | Roadmap for the email-style, branching, newest-on-top thread view |
