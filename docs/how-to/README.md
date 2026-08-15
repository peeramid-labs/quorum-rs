# How-to guides

Goal-oriented recipes. Assume the reader is already competent — get them to the answer.

| Guide | When you reach for it |
|---|---|
| [Run an agent fleet via `quorum serve`](run-an-agent-fleet.md) | You have a `quorum.yml` and need to run every agent in it — multiple providers, multiple models, mixing LLM / exec / MCP / Claude CLI in one process. |
| [Smoke-test an agent](smoke-test-an-agent.md) | You want to verify a serving agent actually participates in real deliberations — `quorum smoke-test <agent_id>` runs the full protocol N times and reports a participation rate. |
| [Limit an agent's concurrent jobs](limit-agent-job-concurrency.md) | An agent's jobs mutate shared state (e.g. a git repo a middleware resets per job) and concurrent jobs race — cap it with `max_concurrent_jobs`. |
| [Agent development](agent-development.md) | You want to build a deliberation agent — reference impl, custom Rust trait, or non-Rust via exec/MCP. |
| [Register a custom provider](register-a-custom-provider.md) | You want `quorum serve` to recognise a `provider.type` the SDK doesn't ship — register a `ProviderFactory` without forking. |
| [Redeem an invite code](redeem-invite-code.md) | You have a single-use JWT invite code and need NATS credentials in one command. |
| [Register a device (self-serve, no invite)](register-a-device.md) | Mint an operator token from a device nkey by signing a challenge — the anonymous activation funnel; `POST /register` create and idempotent-login paths. |
| [Verify what your agent publishes](verify-telemetry.md) | You hold an auditor JWT and want to subscribe to your agent's telemetry stream. |
| [Inspect telemetry with the NATS CLI](inspect-telemetry-with-nats-cli.md) | You want to live-tail and filter events with `nats sub` + `jq`. |
| [Opt out of telemetry](opt-out-telemetry.md) | You need every telemetry emission disabled at the agent boundary. |
| [Expose the agent dashboard on the LAN](expose-agent-dashboard-on-lan.md) | You want the unified per-agent dashboard (status, chat capture, buffer inspection, live config) reachable from another host — and you understand it ships unauthenticated. |
| [Compose a persona from shared files](compose-persona-from-shared-files.md) | Your fleet shares a 4–30 line persona block (review style, output format, safety rules) across many agents and you want a single-source-of-truth file rather than copies. |
| [Use the thread TUI](use-the-thread-tui.md) | You want to hold a branching, email-style conversation with a deliberation — reply, fork off an older turn, start a new line, expand / full-view, and delete threads from the inbox. |
| [Use the Claude Code plugin](use-the-claude-code-plugin.md) | You want `/quorum:init`, `:redeem`, `:run`, `:serve`, `:status`, `:trace`, `:validate`, `:tui` as slash commands inside Claude Code, with `.env` auto-detection and failure-mode guidance on top of the CLI. |

For the underlying *why*, jump to [explanation](../explanation/README.md). For schema and field-level definitions, jump to [reference](../reference/README.md).
