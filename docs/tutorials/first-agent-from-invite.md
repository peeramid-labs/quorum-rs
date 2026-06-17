# Your first agent — from invite code to live deliberation

You've been handed an invite code by someone running a quorum
orchestrator. By the end of this tutorial — about 15 minutes —
you'll have your own agent connected to that orchestrator,
actively contributing to deliberations, and you'll have seen its
first proposal land on the wire.

This tutorial assumes you're at a terminal and willing to copy/paste
commands. We pick one concrete path through the system. We don't
discuss alternatives or design rationale — those live in the
[how-to guides](../how-to/) and [explanation](../explanation/)
sections. Here we just build the thing.

## What you'll build

```
┌────────────────────┐         ┌──────────────────────────────┐
│  your laptop       │         │  api.peeramid.xyz (or your   │
│                    │         │  admin's orchestrator)       │
│   ┌────────────┐   │         │                              │
│   │ cortex-a   │◀──┼─NATS────│  ┌────────────────────────┐  │
│   │ (the agent │   │         │  │ deliberation rooms     │  │
│   │  you run)  │───┼────────▶│  │ — your agent shows up  │  │
│   └────────────┘   │         │  │   as a participant     │  │
│                    │         │  └────────────────────────┘  │
└────────────────────┘         └──────────────────────────────┘
```

One agent, running on your laptop, joined to a quorum on
someone else's orchestrator. We'll use GPT-4o-mini as the
agent's underlying LLM (cheap, fast, good enough for a first
run). Swapping for a different model — or using the Claude CLI
instead of an API key — is the last step.

## What you need

Before starting, have these ready:

- **Rust 1.85+ toolchain** (`rustup` installed). Verify:
  ```bash
  rustc --version
  ```
- **An invite code** from your admin. Looks like a long
  JWT (`eyJhbGc...`).
- **An LLM endpoint to drive the agent.** Any of:
  - An OpenAI-compatible API key (OpenAI itself, Groq, DeepSeek,
    Together, local llama.cpp, …),
  - The `claude` CLI on your `$PATH` (no key needed — Claude CLI
    handles its own auth),
  - A local Ollama instance,
  - Or your own subprocess-based agent driven via the exec / MCP
    protocols.

  We default to an OpenAI-compatible setup below. To pick a
  different runtime, jump to [Step 6 (alternative LLMs)](#step-6-alternative-llms)
  before you start the agent. The bootstrap command in Step 3
  scaffolds **all** of these in one file so you only have to
  uncomment the block you want.
- **About $0.50 of LLM credit** if you're going with a hosted
  OpenAI-compatible provider. The default model (`gpt-4o-mini`)
  is cheap.

## Step 1 — install the `quorum` CLI

```bash
cargo install quorum-rs
```

This takes 1-3 minutes the first time (Rust crates compile from
source). Verify the binary is on your `$PATH`:

```bash
quorum --version
```

Expected output: a version line starting with `quorum `. If you
get "command not found", your `~/.cargo/bin` isn't in `$PATH`
— add it to your shell profile:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
source ~/.zshrc
```

If `cargo install` says `could not find quorum-rs in registry with version *`, only pre-release versions are on crates.io right now — check <https://crates.io/crates/quorum-rs> for the latest and pass it explicitly, e.g. `cargo install quorum-rs --version "<latest-shown-on-crates-io>"`. Once a stable `0.x` ships the bare command above will just work.

✅ **Checkpoint:** `quorum --version` prints a version.

## Step 2 — redeem your invite code

```bash
quorum redeem eyJhbGc...    # ← paste your full invite code here
```

You'll see something like:

```
Redeeming invite at https://api.peeramid.xyz…

✓ Redeemed unified invite (operator + agent).

  Operator     : alice
  Token file   : /home/you/.nsed/operator.token
  Connect URL  : nats://api.peeramid.xyz:4222
  Agent pubkey : UABCDEFGHIJKLMNOP...
  Creds file   : /home/you/.nsed/agent.creds
  Seed file    : /home/you/.nsed/agent.seed
```

The three files are your operator identity, split by transport:

- `~/.nsed/operator.token` — bearer token. Authenticates you to
  the orchestrator's **HTTP API**. `quorum run`, `quorum status`,
  `quorum trace`, and `quorum tui` use it.
- `~/.nsed/agent.creds` — NATS User JWT + seed bundle.
  Authenticates you to the orchestrator's **NATS bus**. `quorum
  serve` reads it to start agents.
- `~/.nsed/agent.seed` — the raw NKey seed (mode 0600, never
  share). The `.creds` blob embeds a copy; the standalone file
  is for tooling that needs the seed in isolation.

Both `operator.token` and `agent.creds` represent the SAME
operator identity. The orchestrator pins the same `username`
(`alice` in the example) to both during redemption.

> **Picking a different directory?** Point `quorum redeem` at it
> with `--out-dir`:
>
> ```bash
> quorum redeem eyJhbGc... --out-dir ./creds
> # writes ./creds/{agent.seed,agent.creds,operator.token,orchestrator}
> ```
>
> You'll then need to point `quorum serve` at the same files
> with `--nats-creds ./creds/agent.creds` (Step 4) and export
> the bearer manually when running client commands.

> **What if the admin gave you a chat-only code?** You'll see "Redeemed
> operator invite (chat-only)" instead, and `agent.creds` won't
> be written. The bearer alone lets you submit tasks from the
> client side but you can't run an agent. Ask the admin to mint
> a unified code (`capabilities: ["chat", "agent"]`) and rerun.

Save the NATS URL — you'll need it in Step 4:

```bash
export NATS_URL=nats://api.peeramid.xyz:4222    # use what was printed above
```

✅ **Checkpoint:** `ls -la ~/.nsed/agent.creds` shows the file with mode 0600.

## Step 3 — scaffold your `agent.yml`

In a fresh directory of your choice:

```bash
mkdir ~/my-first-agent && cd ~/my-first-agent
```

Generate a starter `agent.yml`:

```bash
quorum init --agent-fleet --agents cortex-a
```

The flag is the important bit — without `--agent-fleet`, `quorum
init` writes the *client-side* `nsed.yaml` instead (Step 5 uses
that one). With `--agent-fleet` it writes an `agent.yml` with:

- An active **OpenAI-compatible** provider block (works for
  OpenAI, Groq, DeepSeek, Together, local llama.cpp — just change
  `base_url`).
- Commented stanzas for **Claude CLI**, **exec** (subprocess
  agent), and **MCP** — uncomment the one you want.
- One agent entry per name passed to `--agents` (default
  `cortex-a` if you omit it). `cortex-a` is the agent's NATS
  identity in the orchestrator's logs.

Open the file, pick your provider, then set the env var the
template references. For the default OpenAI block:

```bash
export OPENAI_API_KEY=sk-...
```

The `${OPENAI_API_KEY}` placeholder is resolved at runtime when
`quorum serve` loads the config, so no secrets live in the YAML.
If you uncomment a different block (Claude / exec / MCP), Step 6
has the per-provider notes.

> **Picking a different filename / location?** Pass `--config
> ./fleet/agent.yml` to `quorum init` to write there instead.
> You'll then need to point `quorum serve --config
> ./fleet/agent.yml` in Step 4.

✅ **Checkpoint:** `cat agent.yml` prints a `providers:` block
with `openai:` active and the env var you exported is set.

## Step 4 — start your agent

From the same directory:

```bash
quorum serve --nats-url $NATS_URL
```

`serve` reads `./agent.yml` and `~/.nsed/agent.creds` by default.
To point at different paths:

```bash
quorum serve \
  --config       ./fleet/agent.yml \
  --nats-url     $NATS_URL \
  --nats-creds   ./creds/nats.creds
```

You'll see startup logs like:

```
INFO starting fleet agent_count=1 nats_url=nats://api.peeramid.xyz:4222
INFO agent ready agent=cortex-a
INFO 🟢 Agent 'cortex-a' started
INFO Starting multi-agent runner with 1 agent(s): cortex-a
INFO 🍃 NATS Leaf Worker Connected: cortex-a
```

The agent is now subscribed to its task subject on NATS and
waiting for work. `quorum serve` runs in the foreground; leave
this terminal open and open a new one for Step 5.

✅ **Checkpoint:** Logs show `🟢 Agent 'cortex-a' started` and
`🍃 NATS Leaf Worker Connected`. No error lines following.

> **Troubleshooting:** If you see `failed to build agent ...
> NATS connection error`, double-check the URL printed in Step 2
> matches what you exported. If you see "no agents to run", the
> YAML isn't in the directory you're running from — `pwd` and
> verify `agent.yml` is there.

## Step 5 — submit a deliberation

In a fresh terminal (leave Step 4 running), submit a task to
the orchestrator and watch your agent pick it up.

First, export the bearer token from Step 2 — `quorum run` reads
it from `$QUORUM_DEMO_TOKEN`:

```bash
export QUORUM_DEMO_TOKEN=$(cat ~/.nsed/operator.token)
```

`quorum run` needs a small config (`nsed.yaml`) telling it which
orchestrator to talk to and which agents make up the room.
`quorum init` writes that file:

```bash
cd ~/my-first-agent
quorum init --orchestrator-url https://api.peeramid.xyz \
            --agents cortex-a \
            --room demo
```

This writes `nsed.yaml` next to your `agent.yml`. (Two yamls
for two distinct concerns: `agent.yml` configures the AGENT
PROCESS — `quorum serve` reads it — while `nsed.yaml`
configures task SUBMISSION — `quorum run` / `quorum tui` /
`quorum status` read it.) Now submit a task:

```bash
quorum run --room demo "What is the most efficient way to boil water?"
```

Watch the Step 4 terminal — within a few seconds you should see:

```
INFO cortex-a proposing on round 0 ...
INFO cortex-a evaluating round 0 ...
```

And the client terminal will print a deliberation outcome.

✅ **Checkpoint:** Your agent logged `proposing` AND
`evaluating` (the two phases per round) for at least one round,
and the client got a result.

> **Confirm via TUI (optional):** in a third terminal,
> `quorum tui` opens a live view of the orchestrator. Your
> agent appears in the agent list, the deliberation appears in
> the room view, and you can see your proposal text.
>
> (TUI ships in the released `quorum` binary. If you built from
> source and `tui` is missing, rebuild with
> `cargo build --release --features tui`.)

> **Scoped invites — targeting a policy directly.** If your
> invite was minted for a capability namespace (the redeem
> output's `tags` / `grants` look like `noosphera:*` rather than
> a plain room), you submit to that *policy* instead of a room:
>
> ```bash
> quorum run --policy noosphera:0v1 "your question here"
> # or just `quorum tui` and pick the policy from the list
> ```
>
> For your agent to be *picked* for that policy, it must
> advertise a matching capability tag — add to its `agent.yml`
> entry:
>
> ```yaml
> agents:
>   - name: cortex-a
>     # …provider/model…
>     capability_tags: ["noosphera:0v1"]   # matched by the policy's `noosphera:*` requirement
> ```
>
> The orchestrator matches a policy requirement `noosphera:*`
> against any agent tag under the `noosphera:` namespace
> (`noosphera:0v1`, `noosphera:exp`, …). An agent with **no**
> `capability_tags` stays eligible for everything (legacy
> default); add the tag only to scope it to specific policies.
> The `noosphera:*` policy itself must already be registered on
> the orchestrator — that's the admin/`manage_agents` side, not
> the agent's.

## Step 6 — alternative LLMs

You don't have to use OpenAI. Same `agent.yml` with different
provider entries:

### Claude CLI (no API key needed in the YAML)

Have `claude` on your `$PATH`? Use it directly:

```yaml
providers:
  claude_cli:
    type: claude

agents:
  - name: cortex-a
    provider_id: claude_cli
    model_name: claude-sonnet-4
```

The agent invokes the Claude CLI as a subprocess. Auth comes
from however you have `claude` configured (`claude config`).

### Groq / DeepSeek / any OpenAI-wire-compatible endpoint

```yaml
providers:
  groq:
    type: openai          # OpenAI wire format
    base_url: "https://api.groq.com/openai/v1"
    api_key: "${GROQ_API_KEY}"

agents:
  - name: cortex-a
    provider_id: groq
    model_name: llama-3.1-70b-versatile
```

Set `$GROQ_API_KEY` in your shell, restart `quorum serve`.

### Local Ollama

```yaml
providers:
  ollama:
    type: openai
    base_url: "http://localhost:11434/v1"
    api_key: "ollama"      # ignored but field required

agents:
  - name: cortex-a
    provider_id: ollama
    model_name: llama3.1
```

Start `ollama serve` first.

## What just happened

You bootstrapped an agent identity from a single invite code,
plugged it into a remote orchestrator's NATS bus, and it
contributed to a deliberation. The pieces:

- `quorum redeem` exchanged your single-use invite for two
  long-lived credentials: an HTTP bearer (for the client API)
  and a NATS User JWT (for the agent worker). It generated the
  NKey locally — the seed never crossed the network.
- `agent.yml` is the only mutable thing the operator owns: it
  lists agents, which provider runs each, and which model.
- `quorum serve` reads the YAML, dispatches each entry to the
  right `NsedAgent` implementation (here: `ProposerEvaluatorAgent`
  + `OpenAICompatibleModel`), wires it into a worker, and runs
  every agent in one process.
- The orchestrator's deliberation engine dispatched tasks to
  your agent via NATS; your agent proposed an answer, evaluated
  peers (well, itself, in a one-agent quorum), and the
  orchestrator computed the verdict.

The orchestrator (run by the admin who gave you the invite) is
the only thing not on your laptop. Everything else — agent
runtime, LLM calls, NKey storage — is local.

## Where to go next

- **Multiple agents:** add more entries under `agents:` in
  `agent.yml`. The [run-an-agent-fleet how-to](../how-to/run-an-agent-fleet.md)
  covers patterns (same provider × many models, multiple
  providers, mixing LLM + exec/MCP agents in one process).
- **Custom Rust agent:** when `ProposerEvaluatorAgent` doesn't
  fit your use case, implement the [`NsedAgent`](../how-to/agent-development.md)
  trait directly. You'll write a binary that uses
  `NatsNsedWorker` instead of `quorum serve`.
- **Non-Rust agent:** drive an agent from Python / TypeScript /
  anything via the [exec protocol](../reference/exec-agent-protocol.md)
  or [MCP protocol](../reference/mcp-agent-protocol.md). Add an
  entry to `agent.yml` with `type: exec` or `type: mcp`.
- **Run your own orchestrator:** that's a separate setup using
  the proprietary `nsed-orchestrator` binary, not covered by
  this OSS SDK.

## Cleanup

When you're done experimenting:

```bash
# Stop your agent
Ctrl-C   # in the quorum serve terminal

# Remove generated files
rm -rf ~/.nsed ~/my-first-agent
```

The invite code itself was single-use — it's already consumed
on the orchestrator side and can't be redeemed again. If you
ever want to come back, your admin can mint you a fresh one.
