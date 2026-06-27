# Use the Claude Code plugin

> Recipe for operators who want every quorum lifecycle step
> (init / redeem / run / serve / status / trace / validate / tui)
> exposed as a slash command inside Claude Code, with `.env`
> auto-detection and structured failure-mode guidance on top of
> the binary. Assumes Claude Code is already installed and signed
> in.

## What the plugin gives you

| Slash command | Wraps |
|---|---|
| `/quorum:init` | `quorum init` (interactive workspace setup) with pre-flight `.env` scan + provider key detection. |
| `/quorum:redeem <code>` | `quorum redeem` — JWT invite → `~/.nsed/agent.creds` + `operator.token` at `0600`. |
| `/quorum:run "<task>"` | `quorum run` — dispatch into the workspace's default room; stream the verdict. |
| `/quorum:serve` | `quorum serve` — boot the agent fleet from `quorum.yml`. |
| `/quorum:status` | `quorum status` — orchestrator health + agent listing. |
| `/quorum:trace <job_id>` | `quorum trace` — per-round deliberation history. |
| `/quorum:validate` | `quorum validate` — schema-check the workspace yaml. |
| `/quorum:tui` | `quorum tui` — interactive TUI. |

Plus an auto-loaded skill (`quorum.md`) that carries the mental
model + common-failure-mode catalog, so every session starts
from the same architectural baseline instead of asking Claude
to re-derive it from `quorum.yml` contents.

## The recipe

### 1. Install the binary first

```bash
cargo install --git https://github.com/peeramid-labs/quorum quorum-rs --features cli,tui,status-server
quorum --version
```

The plugin shells out to `quorum`; if it isn't on `$PATH` every
slash command fails before talking to the orchestrator.

### 2. Add the marketplace + install the plugin

In Claude Code:

```
/plugin marketplace add peeramid-labs/plugin-marketplace
/plugin install quorum
```

The marketplace lives at
<https://github.com/peeramid-labs/plugin-marketplace>; the
quorum plugin sits under `quorum/` in that repo. After install,
`/plugin list` shows `quorum` with status `enabled`.

### 3. Run through a first deployment

```
/quorum:init
```

What Claude does behind the prompt:

- Reads `$PWD/.env` to spot `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` /
  `OPENROUTER_API_KEY` / `GOOGLE_API_KEY` /
  `PEERAMID_OPERATOR_TOKEN`.
- Checks whether `api.peeramid.xyz` answers `/health` — if not,
  suggests an embedded orchestrator instead.
- Drops the operator into `quorum init`'s interactive wizard
  with sensible defaults pre-pinned to the detected env.
- After the wizard writes `quorum.yml`, runs `quorum validate`
  and surfaces the resolved `default_room`, orchestrator count,
  policy count.

Then if the operator has an invite code:

```
/quorum:redeem <eyJhbGc...>
```

Then to dispatch:

```
/quorum:run "Pick the best strategy for X under constraint Y"
```

Or to bring up an agent fleet:

```
/quorum:serve
```

## When the plugin saves time vs the bare CLI

- Operators who haven't memorised which `.env` keys map to which
  provider entries. The plugin auto-suggests.
- Mixed-orchestrator workspaces where `--workspace PATH` /
  `--room NAME` resolution gets ambiguous. The plugin walks the
  decision tree out loud.
- Post-failure diagnosis: a `503 no available server` on
  `/api/runtime/nats` doesn't decode well as a raw curl error;
  the plugin's failure catalog explains it as "orchestrator
  container not running" and points at the deploy.

## When to skip the plugin

- CI / scripted runs — call the binary directly; the plugin's
  prompts are interactive.
- Audit reviews — the plugin doesn't replace reading the
  generated yaml. Always inspect `quorum.yml` before committing
  it.
- Multi-tenant orchestrator admin work — those flows touch
  endpoints (`/api/operators`, `/admin/api/invites`) the
  plugin doesn't wrap; use the upstream `nsed-orchestrator`
  CLI / dashboard instead.

## See also

- [tutorials/first-agent-from-invite.md](../tutorials/first-agent-from-invite.md)
  — the same flow without the plugin, useful for understanding
  what the slash commands hide.
- [reference/cli.md](../reference/cli.md) — the binary's flag
  surface, source of truth.
- <https://github.com/peeramid-labs/plugin-marketplace> — the
  marketplace + plugin source, including the prompts each slash
  command sends.
