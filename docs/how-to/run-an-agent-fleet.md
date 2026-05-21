# Run an agent fleet via `quorum serve`

Task-oriented recipe. Assumes you've already got an invite code
from an admin and an LLM endpoint to point each agent at. If
you've never run an agent before, start with the
[tutorial](../tutorials/first-agent-from-invite.md) — it walks
through the same flow with hand-holding. This page is for when
you know the shape and just need to look up the exact commands.

## What `serve` does

One command, three things:

1. Loads `agent.yml` (your fleet config).
2. Picks the right agent implementation for each entry —
   `ProposerEvaluatorAgent` for LLM providers (OpenAI-compatible,
   Ollama, simulated), `ExecAgent` for stdin/stdout subprocesses,
   `McpAgent` for MCP, `ClaudeAgent` for the Claude CLI.
3. Wires them all into a single `MultiAgentRunner` and runs
   until SIGTERM/SIGINT.

Out of scope (use the proprietary `nsed serve` from the parent
repo for these):

- Booting a local orchestrator.
- Per-agent JWT challenge-response registration — `quorum serve`
  expects one shared `.creds` for the whole fleet (typically the
  one `quorum redeem` writes).
- Workspace policy push — policies are configured server-side.

## Prerequisites

- `quorum` binary installed: `cargo install quorum-rs --version 0.7.0-rc.2`.
- An invite code from your admin AND NATS credentials for the
  orchestrator. `quorum redeem <code>` writes both to `~/.nsed/`.
- An OpenAI-compatible LLM endpoint with an API key OR the
  `claude` CLI on `$PATH`.

## Step 1 — redeem your invite

```bash
quorum redeem eyJhbGc...
```

Writes:

- `~/.nsed/operator.token` — HTTP bearer token (used by `quorum run`)
- `~/.nsed/agent.creds` — NATS `.creds` file (used by `quorum serve`)
- `~/.nsed/agent.seed` — NKey seed; keep private

For chat-only codes only the `.token` is written; you can't run
agents off a chat-only invite. Ask the admin for a unified code
(`capabilities: ["chat", "agent"]`).

## Step 2 — write `agent.yml`

The minimal shape:

```yaml
providers:
  openai:
    type: openai
    base_url: "https://api.openai.com/v1"
    api_key: "${OPENAI_API_KEY}"

agents:
  - name: cortex-a
    provider_id: openai
    model_name: gpt-4o
```

The `${OPENAI_API_KEY}` env reference is resolved at agent-build
time — keep the key out of the YAML.

### Multiple agents on the same LLM

```yaml
providers:
  openai:
    type: openai
    base_url: "https://api.openai.com/v1"
    api_key: "${OPENAI_API_KEY}"

agents:
  - name: cortex-a
    provider_id: openai
    model_name: gpt-4o
  - name: cortex-b
    provider_id: openai
    model_name: gpt-4o-mini
  - name: cortex-c
    provider_id: openai
    model_name: gpt-4o
```

`quorum serve` runs all three concurrently in one process.

### Multiple providers

```yaml
providers:
  openai:
    type: openai
    base_url: "https://api.openai.com/v1"
    api_key: "${OPENAI_API_KEY}"
  groq:
    type: openai      # OpenAI wire-compatible
    base_url: "https://api.groq.com/openai/v1"
    api_key: "${GROQ_API_KEY}"

agents:
  - name: cortex-fast
    provider_id: groq
    model_name: llama-3.1-70b-versatile
  - name: cortex-thoughtful
    provider_id: openai
    model_name: gpt-4o
```

### Claude CLI as the agent runtime

```yaml
providers:
  claude_cli:
    type: claude

agents:
  - name: cortex-claude
    provider_id: claude_cli
    model_name: claude-sonnet-4
```

Requires `claude` on `$PATH`. The agent invokes the CLI directly
— no API key in `agent.yml` because Claude CLI handles its own
auth.

### Python / TypeScript / any-language agent via the exec protocol

```yaml
providers:
  exec_local:
    type: exec

agents:
  - name: cortex-python
    provider_id: exec_local
    model_name: custom
    exec:
      command: ["python3", "examples/exec_agent.py"]
```

The subprocess receives deliberation context on stdin and writes
its result to stdout wrapped in delimiter tokens. See the
[exec protocol reference](../reference/exec-agent-protocol.md).

## Step 3 — run

Pointing at production:

```bash
export OPENAI_API_KEY=sk-...
quorum serve --nats-url nats://api.peeramid.xyz:4222
```

Pointing at a local dev orchestrator:

```bash
quorum serve --nats-url nats://localhost:4222
```

`serve` reads `~/.nsed/agent.creds` by default. Override with
`--nats-creds PATH` if your creds live elsewhere. Override the
config path with `--config PATH`.

Restrict to a subset of agents (handy for debugging one at a
time):

```bash
quorum serve --agent cortex-a --agent cortex-c
```

## Step 4 — verify

In another shell:

```bash
quorum status                      # health check + agent list
quorum run --room demo "<topic>"   # submit a deliberation
quorum tui                         # interactive TUI
```

The `agents` field on `quorum status` should list every agent
from your `agent.yml`. If an agent is missing, check the `serve`
logs — `RUST_LOG=info quorum serve …` shows per-agent startup +
any failures during build.

## Common issues

| Symptom | Likely cause |
|---|---|
| `no agents to run — fleet config has 0 agents` | `agent.yml` parsed but no `agents:` section, or `--agent` filter matched nothing. Check `quorum serve --agent ALL --config agent.yml`. |
| `no agents successfully started` | Every fleet entry hit a build error. `RUST_LOG=info quorum serve` shows per-agent failures. |
| `unknown provider_type — skipping` | A `type:` value the dispatcher doesn't know about — likely a typo. Supported types: `openai`, `ollama`, `simulated`, `exec`, `mcp`, `claude`. |
| `Agent '<name>' failed ... NATS connection error` | NATS creds bad / orchestrator NATS URL wrong. Verify with `nats sub '>' --server <nats-url> --creds ~/.nsed/agent.creds` (needs `nats-cli`). |
| Worker logs hang / never reconnect after SIGKILL | Use SIGTERM (`Ctrl-C`), not SIGKILL — the cancellation handler only fires on SIGTERM/SIGINT. |

## How shutdown works

`Ctrl-C` (SIGINT) and SIGTERM both trigger graceful shutdown.
The CLI signals a [`CancellationToken`](https://docs.rs/tokio-util/0.7/tokio_util/sync/struct.CancellationToken.html);
each worker's reconnect loop sees `is_cancelled()` at the top of
its next iteration and returns cleanly, **and** an abort
watchdog calls `.abort()` on every worker `JoinHandle` so
workers blocked deep inside `worker.run().await` past the
cooperative check stop too. The CLI then waits 250ms for NATS
connections to drop cleanly before exiting.

Hard-kill (`SIGKILL` / `kill -9`) bypasses all of this — the
process disappears immediately and the orchestrator notices via
NATS heartbeat timeout (default ~30s).

## Embedding `serve_fleet` in your own binary

The SDK exposes the same flow as a library function so you can
wrap it with custom telemetry, dashboards, supervision, etc.:

```rust,no_run
use std::path::Path;
use quorum_rs::config::load_config;
use quorum_rs::nats_utils::NatsAuth;
use quorum_rs::serve::{ServeOptions, serve_fleet};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fleet = load_config(Path::new("agent.yml"))?;
    let cancel = tokio_util::sync::CancellationToken::new();

    // Wire your own SIGTERM handler that calls cancel.cancel()
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_cancel.cancel();
    });

    let opts = ServeOptions {
        nats_url: "nats://api.peeramid.xyz:4222".into(),
        nats_auth: Some(NatsAuth {
            creds_file: Some("/path/to/agent.creds".into()),
            ..Default::default()
        }),
        cancel: Some(cancel),
        ..Default::default()
    };
    serve_fleet(&fleet, opts).await
}
```

## See also

- [Tutorial: your first agent from an invite code](../tutorials/first-agent-from-invite.md) — hand-holding walkthrough.
- [Agent development guide](agent-development.md) — when you outgrow `serve` and want to write a custom `NsedAgent` impl.
- [Redeem an invite code](redeem-invite-code.md) — what `quorum redeem` does under the hood + how to embed it in your own binary.
