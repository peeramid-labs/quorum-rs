# How-to guides

Goal-oriented recipes. Assume the reader is already competent — get them to the answer.

| Guide | When you reach for it |
|---|---|
| [Run an agent fleet via `quorum serve`](run-an-agent-fleet.md) | You have an `agent.yml` and need to run every agent in it — multiple providers, multiple models, mixing LLM / exec / MCP / Claude CLI in one process. |
| [Agent development](agent-development.md) | You want to build a deliberation agent — reference impl, custom Rust trait, or non-Rust via exec/MCP. |
| [Redeem an invite code](redeem-invite-code.md) | You have a single-use JWT invite code and need NATS credentials in one command. |
| [Verify what your agent publishes](verify-telemetry.md) | You hold an auditor JWT and want to subscribe to your agent's telemetry stream. |
| [Inspect telemetry with the NATS CLI](inspect-telemetry-with-nats-cli.md) | You want to live-tail and filter events with `nats sub` + `jq`. |
| [Opt out of telemetry](opt-out-telemetry.md) | You need every telemetry emission disabled at the agent boundary. |

For the underlying *why*, jump to [explanation](../explanation/README.md). For schema and field-level definitions, jump to [reference](../reference/README.md).
