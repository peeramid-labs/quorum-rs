---
title: Run an agent fleet
order: 3
tagline: Boot a whole agent fleet from a single quorum.yml with one quorum serve command.
---
# Run an agent fleet

Task-oriented recipe. Assumes you've already got an invite code
from an admin and an LLM endpoint to point each agent at. If
you've never run an agent before, start with the
[tutorial](../tutorials/first-agent-from-invite.md) — it walks
through the same flow with hand-holding. This page is for when
you know the shape and just need to look up the exact commands.

## What `serve` does

One command, three things:

1. Loads `quorum.yml` (the unified config — orchestrators,
   rooms, policies AND the `providers:` / `agents:` fleet, all
   in one file).
2. Picks the right agent implementation for each `agents:` entry —
   `ProposerEvaluatorAgent` for LLM providers (OpenAI-compatible,
   Ollama, simulated), `ExecAgent` for stdin/stdout subprocesses,
   `McpAgent` for MCP, `ClaudeAgent` for the Claude CLI.
3. Wires them all into a single `MultiAgentRunner` and runs
   until SIGTERM/SIGINT.

On boot, `serve` also:

- registers each `agents:` entry with the orchestrator using the
  orchestrator entry's `token` — see the operator-token note in Step 1;
- pushes every **role-based** policy from the config to the orchestrator
  (idempotent, keyed by content hash), so OpenAI-compat model names
  (`nsed:<tag>`) and `--policy` runs resolve without a separate
  `quorum run`. Static agent-list policies dispatch by name and aren't
  pushed. A failed push is logged, never fatal.

> **Legacy split config still loads.** A pre-existing `nsed.yaml`
> + separate `agent.yml` pair is auto-detected and works
> unchanged. New setups should use the single `quorum.yml`.

Out of scope (use the proprietary `nsed serve` from the parent
repo for these):

- Booting a local orchestrator.
- Per-agent JWT challenge-response registration — `quorum serve`
  expects one shared `.creds` for the whole fleet (typically the
  one `quorum redeem` writes).
- Workspace policy push — policies are configured server-side.

## Prerequisites

- `quorum` binary installed: `cargo install quorum-rs`. While
  only pre-release crates are on crates.io, the bare command
  errors with `could not find quorum-rs ... with version *`;
  pass `--version "<latest>"` referring to the version on
  <https://crates.io/crates/quorum-rs>.
  - `quorum` checks crates.io for a newer release and prints a one-line
    upgrade notice at the start and end of a run (cached 24h, 3s timeout,
    silent when offline — never blocks). It's channel-aware: a stable build
    is told only about newer **stable** releases; an rc build about any newer
    version (a newer rc, or the stable that supersedes it).
- An invite code from your admin AND NATS credentials for the
  orchestrator. `quorum redeem <code>` writes both to `~/.nsed/`
  by default; pass `--out-dir DIR` to put them elsewhere.
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

> **The operator token is what attributes your agents.** On every boot
> `serve` registers each fleet agent with the orchestrator
> (`/credentials/register`) using the orchestrator entry's `token`,
> and *that* call is what records each `agent_id → operator` link. The
> bearer must be your **operator token** (`operator.token`, which carries
> the `manage_agents` role + a display name). `quorum init --invite`
> scaffolds `token: "file:~/.nsed/operator.token"` on the
> orchestrator entry for you. An env-var ref (`token:
> "${OPERATOR_TOKEN}"`) works too for CI/devops.
>
> **You can also omit `address` and `token` entirely** (or set them
> blank). A remote orchestrator entry with either field missing/blank
> inherits it from the redeemed `~/.nsed/` files — `address` from
> `$QUORUM_ORCHESTRATOR` then `~/.nsed/orchestrator`, `token` from
> `~/.nsed/operator.token` — so after `quorum redeem` a config need only
> name the orchestrator:
>
> ```yaml
> orchestrators:
>   remote:
>     mode: remote   # address + token inherited from ~/.nsed
> ```
>
> If nothing resolves (no config value, nothing redeemed), the bearer is
> blank → registration 403s → agents heartbeat **unattributed** and the
> orchestrator **drops** them. `serve` logs a loud `ERROR` when this
> happens (see the self-check table below).

## Step 2 — scaffold `quorum.yml`

```bash
quorum init --invite eyJhbGc...
```

Writes a single `quorum.yml` next to you: the orchestrator
entry (with `token: "file:~/.nsed/operator.token"`),
rooms/policies, and the `providers:` / `agents:` fleet — an
active OpenAI-compatible provider, commented stanzas for Claude
CLI / exec / MCP, and one agent entry. Pass `--config PATH` to
write elsewhere; `--force` overwrites an existing file.

> The legacy `quorum init --agent-fleet` (which scaffolded a
> separate `agent.yml`) still exists for back-compat, but the
> unified `quorum.yml` is preferred.

The fleet portion it generates:

```yaml
providers:
  openai:
    type: openai
    base_url: "https://api.openai.com/v1"
    api_key: "${OPENAI_API_KEY}"

agents:
  - name: cortex-a
    provider_id: openai
    model_name: gpt-4o-mini
```

`${OPENAI_API_KEY}` is resolved at runtime when `quorum serve` loads the config — keep the
key out of the YAML, set it in the shell that runs `quorum serve`.
Bump `model_name` to `gpt-4o` (or whatever your provider exposes)
when you're ready to spend more.

#### Per-agent strategy prompts (interactive `quorum init`)

On a TTY the wizard also asks, per agent, for the runtime knobs that smaller
models often need — all optional, sensible defaults pre-selected:

- **Engine strategies** (multi-select): `stream responses` (default on),
  `merge system prompt into first user message`, `disable native tool
  definitions`.
- **LLM-repair passes** (multi-select): `fix invalid JSON escapes` and
  `parse hallucinated tool-calls` (both default on), `request JSON mode output`.
  These drive the [`llm-repair`](https://docs.rs/llm-repair) passes that salvage
  malformed output.
- **Failure dumps** (`on` / `full` / `off`, default `on`): what to record on a
  parse/API error — `on` = metadata, `full` = raw payloads, `off` = nothing.

Each maps to a field on the agent in `quorum.yml` (`use_streaming`,
`merge_system_prompt`, `disable_native_tools`, `repair_invalid_escapes`,
`unwrap_hallucinated_tool_calls`, `json_mode`, `failure_dumps`) — edit them
there afterward, or leave the wizard's choices.

Read-vs-writable file access is a separate prompt pair: "Read as context, never
writable" (`read_paths`) and "Writable scope" (`write_dirs`).

For known model families the wizard skips the guesswork: if your `model_name`
matches a family we have a tested integration config for — **qwen3**, **gpt-oss**
(Harmony engine), **tongyi** — it offers the known-good strategy settings and,
on accept, applies them and skips the manual engine/repair prompts. `gpt-oss`'s
engine is a provider-level field, so the wizard advises setting
`engine: "gpt-oss"` on the provider rather than changing it for you.

### Layering customisations on top of the scaffold

Everything below shows just the `providers:` / `agents:` portion
of `quorum.yml`. The patterns differ only in which provider block
you uncomment and what `model_name`s you list under `agents:`.

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
— no API key in `quorum.yml` because Claude CLI handles its own
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

The TUI opens on a persistent tab bar — **Deliberate · Rooms · Agents ·
Policies · Settings**. Press the number keys `1`–`5` or `Tab`/`Shift-Tab` to
switch tabs (Settings holds Orchestrators + Config). The **Rooms**,
**Policies**, and **Deliberate** screens each split into a **Local
(quorum.yml)** section and a **Remote (orchestrator)** section, so rooms /
policies you defined in the config file aren't confused with runtime ones.
The **Rooms** tab lists each room's bound policy, panel fill (`eligible/desired
✓`), tags, and the agents that would serve it; the create-room form picks the
policy from a selector (`Space`/`←→`). A room whose eligible agents fall below its policy's
target shows a red `✗` on both the Rooms and Deliberate screens — it can't
start a deliberation until enough matching agents are online.

The `agents` field on `quorum status` should list every agent
from your `quorum.yml`. If an agent is missing, check the `serve`
logs — `RUST_LOG=info quorum serve …` shows per-agent startup +
any failures during build.

### Registration self-check

~20s after boot (once the first heartbeats have landed), `serve`
reads back `GET /agents` and logs a verdict per agent. The heartbeat
is fire-and-forget, so without this read-back a server-side drop or a
blank operator never reaches the agent process. Watch for:

| Log | Meaning | Fix |
|---|---|---|
| `ERROR … NOT visible at orchestrator` | heartbeat dropped — the agent has no operator link (orchestrator invariant) or isn't registered. Receives no jobs. | re-redeem the agent code with `operator_name` set |
| `WARN … operator is \`local\`` | the agent code was minted without `operator_name` → no grants/tags → fails grant-based eligibility | re-mint + redeem with `operator_name` |
| `WARN … operator set but has no tags` | the operator has no identity tags | add tags to the operator |
| `INFO … registered and attributed` | healthy | — |

The self-check runs only when the orchestrator is a reachable remote
(workspace `mode: remote`); it's a diagnostic and never blocks serving.

## Common issues

| Symptom | Likely cause |
|---|---|
| `no agents to run — fleet config has 0 agents` | `quorum.yml` parsed but no `agents:` section, or `--agent` filter matched nothing. Check `quorum serve --agent ALL --config quorum.yml`. |
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
    let fleet = load_config(Path::new("quorum.yml"))?;
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
- [Register a custom provider](register-a-custom-provider.md) — add a `provider.type` the SDK doesn't ship, without forking.
- [About the provider registry](../explanation/provider-registry.md) — how `serve` picks the agent implementation for each `provider.type`.
- [Redeem an invite code](redeem-invite-code.md) — what `quorum redeem` does under the hood + how to embed it in your own binary.
