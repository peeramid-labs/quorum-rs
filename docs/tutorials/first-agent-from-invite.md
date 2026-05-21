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
- **An OpenAI API key** with credit on it. Set it in your
  shell:
  ```bash
  export OPENAI_API_KEY=sk-...
  ```
  Don't have one? Skip ahead to [Step 6 (alternative LLMs)](#step-6-alternative-llms);
  Claude CLI works without needing an API key here.
- **About $0.50 of LLM credit** to comfortably complete a few
  deliberations. The default model (`gpt-4o-mini`) is cheap.

## Step 1 — install the `quorum` CLI

```bash
cargo install quorum-rs --version 0.7.0-rc.2
```

This takes 1-3 minutes the first time (Rust crates compile from
source). Verify the binary is on your `$PATH`:

```bash
quorum --version
```

Expected output: something like `quorum 0.7.0-rc.2`. If you get
"command not found", your `~/.cargo/bin` isn't in `$PATH` — add
it to your shell profile:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc   # or ~/.bashrc
source ~/.zshrc
```

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

The interesting bits:

- `~/.nsed/operator.token` — your HTTP bearer for `quorum run`
  later when you submit tasks AS A CLIENT.
- `~/.nsed/agent.creds` — your NATS credentials. `quorum serve`
  picks these up automatically when starting your agent.
- `~/.nsed/agent.seed` — the private half of your NATS identity.
  Don't share this anywhere. The file is mode 0600.

> **What if the admin gave you a chat-only code?** You'll see "Redeemed
> operator invite (chat-only)" instead, and `agent.creds` won't
> be written. You can chat against the orchestrator with the
> bearer token but you can't run an agent. Ask the admin to mint
> a unified code (`capabilities: ["chat", "agent"]`) and rerun.

Save the NATS URL — you'll need it in Step 4:

```bash
export NATS_URL=nats://api.peeramid.xyz:4222    # use what was printed above
```

✅ **Checkpoint:** `ls -la ~/.nsed/agent.creds` shows the file with mode 0600.

## Step 3 — create your `agent.yml`

In a fresh directory of your choice:

```bash
mkdir ~/my-first-agent && cd ~/my-first-agent
```

Create `agent.yml` with this content:

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

Three things to note:

- The `${OPENAI_API_KEY}` placeholder is resolved from your
  shell environment at agent-build time. No secrets in the YAML.
- `cortex-a` is the agent's NATS identity in the orchestrator's
  logs. Pick whatever you like; case-insensitive alphanumeric +
  `-` / `_`.
- `gpt-4o-mini` is the model — anything OpenAI offers works
  here.

✅ **Checkpoint:** `cat agent.yml` prints the file contents.

## Step 4 — start your agent

From the same directory:

```bash
quorum serve --nats-url $NATS_URL
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

First, set the bearer token from Step 2:

```bash
export QUORUM_DEMO_TOKEN=$(cat ~/.nsed/operator.token)
```

Bootstrap a client config (this is the OTHER `init` — for the
client side, not the agent side):

```bash
cd ~/my-first-agent
quorum init --orchestrator-url https://api.peeramid.xyz \
            --agents cortex-a \
            --room demo
```

This writes `nsed.yaml` next to your `agent.yml`. Now submit a
task:

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
