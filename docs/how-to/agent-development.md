---
title: Develop own agent
order: 9
tagline: Build a deliberation agent in Rust and connect it to a quorum-rs orchestrator.
---
# Develop own agent

How to build a deliberation agent in Rust and connect it to a quorum-rs orchestrator.

## Three paths

| Goal | Path |
|---|---|
| Run an existing reference agent | Use [`ProposerEvaluatorAgent`](#path-1-reference-agent) — built-in ReAct loop, structured proposer/evaluator outputs, retry+repair, benchmark-validated prompts |
| Write a custom Rust agent | Implement the [`NsedAgent`](#path-2-custom-rust-agent) trait |
| Plug a non-Rust agent | Use the [exec](../reference/exec-agent-protocol.md) or [MCP](../reference/mcp-agent-protocol.md) protocol — Python, TypeScript, anything that can read stdin / write stdout |

## Path 1: reference agent

`quorum_rs::agents::ProposerEvaluatorAgent` ships the full reference implementation. Construct it with an LLM client + prompt set + tools, wire it to a NATS worker, run.

```rust
use quorum_rs::agents::{AgentConfig, ProposerEvaluatorAgent};
use quorum_rs::llms::OpenAICompatibleModel;
use quorum_rs::prompts::defaults::DefaultPromptSet;
use quorum_rs::workers::{NatsNsedWorker, NatsNsedWorkerExt, WorkerConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent_config = AgentConfig {
        name: "cortex-a".into(),
        provider_id: "openai".into(),
        model_name: "gpt-4o".into(),
        ..Default::default()
    };

    let llm = OpenAICompatibleModel::new(
        "https://api.openai.com/v1".into(),
        std::env::var("OPENAI_API_KEY")?,
        None,
    );

    let agent = ProposerEvaluatorAgent::new(
        agent_config,
        Box::new(llm),
        Box::new(DefaultPromptSet::new()),
        vec![],  // extra context tools
        vec![],  // sandbox tools
    );

    let worker_config = WorkerConfig::new(
        "nats://localhost:4222".into(),
        "sphera_jobs".into(),
        "cortex_a_consumer".into(),
    );

    let worker = NatsNsedWorker::from_agent(agent, worker_config).await?;
    worker.run().await
}
```

### Adding tools

Pass concrete `Tool` impls into the `extra_context_tools` / `sandbox_tools` vectors:

```rust
use quorum_rs::tools::{ScopedReadFileTool, ScopedGrepTool, Tool};
use std::path::PathBuf;

// ScopedReadFileTool::new(agent_name, roots: &[PathBuf]) — infallible.
let read = ScopedReadFileTool::new("cortex-a", &[PathBuf::from("/var/data/corpus")])
    .with_max_bytes(1 << 20);

// ScopedGrepTool::new(agent_name, roots: &[String], max_bytes, max_results,
// timeout_secs) — returns Err on an empty roots allow-list.
let grep = ScopedGrepTool::new(
    "cortex-a".into(),
    &["/var/data/corpus".to_string()],
    1 << 20, // max_bytes
    100,     // max_results
    10,      // timeout_secs
)?;

let sandbox: Vec<Box<dyn Tool>> = vec![Box::new(read), Box::new(grep)];
```

See [Sandboxed builtin tools](../reference/sandboxed-tools.md) for the full catalog.

### Adding an output guard

If you want the agent to refuse outputs that leak internal prompt structure, attach an `OutputLeakDetector` impl:

```rust
use std::sync::Arc;

let agent = ProposerEvaluatorAgent::new(/* ... */)
    .with_output_guard(Arc::new(MyDetector::new()));
```

The SDK ships the trait but no default detector — bring your own (regex registry, LLM-as-judge, content classifier, …).

## Path 2: custom Rust agent

Implement [`NsedAgent`](https://docs.rs/quorum-rs/latest/quorum_rs/agents/trait.NsedAgent.html) directly when you need full control over proposal + evaluation logic.

`NsedAgent` requires `Send + Sync + Debug + Clone` (the worker clones the agent across task boundaries). Derive `Clone`; provide a `Debug` impl — `SimpleOpenAIModel` is `Clone` but not `Debug`, so skip that field.

```rust
use quorum_rs::agents::{NsedAgent, AgentContext, AgentConfig, Proposal, Evaluation};
use quorum_rs::llms::SimpleOpenAIModel;
use quorum_rs::workers::{NatsNsedWorker, WorkerConfig};
use async_trait::async_trait;
use anyhow::Result;

#[derive(Clone)]
struct MyAgent {
    name: String,
    llm: SimpleOpenAIModel,
}

impl std::fmt::Debug for MyAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MyAgent").field("name", &self.name).finish()
    }
}

#[async_trait]
impl NsedAgent for MyAgent {
    fn name(&self) -> String { self.name.clone() }

    async fn propose(&self, context: &AgentContext) -> Result<Proposal> {
        // Your proposal logic — call self.llm and shape the result.
        unimplemented!()
    }

    async fn evaluate(
        &self,
        context: &AgentContext,
    ) -> Result<Vec<(String, Evaluation)>> {
        // Return (target_agent_id, Evaluation) pairs — your verdict on each
        // peer proposal. Peer proposals come from `context` (and the RAG
        // context tools), not as a separate argument.
        unimplemented!()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let agent = MyAgent {
        name: "my-agent".into(),
        llm: SimpleOpenAIModel::new(
            "https://api.openai.com/v1".into(),
            std::env::var("OPENAI_API_KEY")?,
            "gpt-4o-mini".into(),
        ),
    };

    let config = WorkerConfig::new(
        "nats://localhost:4222".into(),
        "sphera_jobs".into(),
        "my_agent_consumer".into(),
    );

    let worker = NatsNsedWorker::new(agent, AgentConfig::default(), config, None).await?;
    worker.run().await
}
```

The `AgentContext` argument carries everything the agent needs about the current deliberation: round number, task description, persistence handle, peer proposals from earlier rounds, scratchpad, telemetry context.

### Running it under `quorum serve`

Hand-wiring a worker in `main()` (above) is one option. The other is to let `quorum serve` boot your agent from the `providers:` / `agents:` fleet in `quorum.yml` like any built-in provider — register a [`ProviderFactory`](../explanation/provider-registry.md) for a custom `provider.type` and pass the registry via `ServeOptions.registry`. Then operators add your agent to the fleet with a few lines of YAML, no Rust. See [Register a custom provider](register-a-custom-provider.md).

## Path 3: non-Rust agent

Skip Rust entirely. Implement the [exec protocol](../reference/exec-agent-protocol.md) for a one-shot stdin/stdout agent, or the [MCP protocol](../reference/mcp-agent-protocol.md) for a tool-calling-aware agent. Both let you write the agent in Python, TypeScript, Go — anything that can speak the JSON wire protocol.

Reference Python implementations are in the workspace `examples/` directory: [`exec_agent.py`](../../examples/exec_agent.py), [`mcp_agent.py`](../../examples/mcp_agent.py).

## NATS authentication

For local development, an unauthenticated NATS broker is fine. For production deployments, the orchestrator exposes JWT-based agent registration. Set one of:

```bash
NATS_TOKEN=<short-lived-jwt> ./my-agent
NATS_USER=agent-user NATS_PASS=password ./my-agent
NATS_CREDS=/path/to/agent.creds ./my-agent
```

The worker reads these from the environment if they're set; otherwise it connects anonymously.

## Bootstrap with an invite code

The orchestrator can mint single-use, short-TTL JWT **invite codes** that an agent redeems on its own host for a freshly scoped NATS User JWT — no long-lived bearer token to ship, and the agent's NKey seed never crosses the network.

Two ways to consume from the SDK:

### Option A: one-shot CLI — `quorum redeem`

The shipping `quorum` binary handles the operator UX for you:

```bash
# Default: hits https://api.peeramid.xyz.
$ quorum redeem eyJhbGc...
Redeeming invite at https://api.peeramid.xyz…

✓ Redeemed invite. NATS credentials are ready.

  Connect URL : nats://api.peeramid.xyz:4222
  Agent pubkey: UABCDEFG12345...
  Creds file  : /home/operator/.nsed/agent.creds
  Seed file   : /home/operator/.nsed/agent.seed
```

Set `NSED_ENV=local` (or `dev` / `development`) to flip the
default to `http://localhost:8080`; pass `--url` to override
explicitly.

Then point the agent process at `~/.nsed/agent.creds` via `NATS_CREDS` (or via [`NatsAuth::creds_file`](../../crates/quorum-rs/src/nats_utils.rs)).

### Option B: embed the SDK helper

For agents that want to redeem at boot without writing files:

```rust
use quorum_rs::nats_utils::{
    redeem_invite_with_orchestrator_with_retry, NatsAuth, connect_nats, RedeemInviteError,
};

let result = match redeem_invite_with_orchestrator_with_retry(
    "http://orch.example.com:8080",
    &invite_code,
    5, // retry attempts
).await {
    Ok(r) => r,
    Err(RedeemInviteError::Expired) => bail!("Invite expired; ask for a fresh code."),
    Err(RedeemInviteError::Replayed) => bail!("Already redeemed."),
    Err(RedeemInviteError::Revoked) => bail!("Admin revoked this invite."),
    Err(e) if e.is_retryable() => bail!("Transient failure: {e}"),
    Err(e) => return Err(e.into()),
};

let auth = NatsAuth { inline_creds: Some(result.creds), ..Default::default() };
let nats = connect_nats(&result.nats_url, Some(&auth)).await?;
// Hand `nats` to your NatsNsedWorker.
```

The helper generates a fresh `nkeys::KeyPair` on every call, presents only the public half to `/redeem-agent`, and assembles a `.creds` blob locally. The returned `RegistrationResult { creds, nats_url, keypair }` carries the keypair if you want to persist the seed.

Typed [`RedeemInviteError`] variants (`InvalidCode` / `Expired` / `Revoked` / `Replayed` / `NotConfigured` / `KvUnavailable` / `Unexpected` / `Transport`) let you `match` on outcome without parsing the orchestrator's JSON error body. `is_retryable()` distinguishes transient from permanent failures.

For the full operator → admin → operator handshake, see the recipe at [redeem an invite code](redeem-invite-code.md).

## Status dashboard

When you build with the `status-server` feature, every `NatsNsedWorker` can serve a live status dashboard:

```rust
let worker = NatsNsedWorker::new(/* … */)
    .await?
    .with_status_server(8080);
worker.run().await
```

Browse to `http://localhost:8080` to see agent identity, current job, recent events, peer evaluation scores.

## See also

- [Agent internals](../explanation/agent-internals.md) — design rationale + ReAct loop architecture
- [Register a custom provider](register-a-custom-provider.md) — give your custom agent its own `provider.type` via a `ProviderFactory`
- [About the provider registry](../explanation/provider-registry.md) — how `provider.type` dispatch resolves to an agent
- [NATS topology](../explanation/nats-topology.md) — subjects, JetStream streams, JWT scopes
- [Sandboxed builtin tools](../reference/sandboxed-tools.md) — `read_file`, `grep_search` reference
