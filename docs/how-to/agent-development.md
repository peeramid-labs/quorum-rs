# Agent development guide

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
use quorum_rs::tools::{ScopedReadFileTool, ScopedGrepTool};
use std::path::PathBuf;

let sandbox = vec![
    Box::new(ScopedReadFileTool::new(
        "cortex-a",
        &[PathBuf::from("/var/data/corpus")],
    )) as Box<dyn quorum_rs::tools::Tool>,
    Box::new(ScopedGrepTool::new(
        "cortex-a",
        &[PathBuf::from("/var/data/corpus")],
    )),
];
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

```rust
use quorum_rs::agents::{NsedAgent, AgentContext, AgentConfig, Proposal, Evaluation, Stance};
use quorum_rs::llms::SimpleOpenAIModel;
use quorum_rs::workers::{NatsNsedWorker, WorkerConfig};
use async_trait::async_trait;
use anyhow::Result;

struct MyAgent {
    name: String,
    llm: SimpleOpenAIModel,
}

#[async_trait]
impl NsedAgent for MyAgent {
    fn name(&self) -> &str { &self.name }

    async fn propose(&self, context: &AgentContext) -> Result<Proposal> {
        // Your proposal logic — call self.llm and shape the result.
        unimplemented!()
    }

    async fn evaluate(
        &self,
        context: &AgentContext,
        proposals: &[Proposal],
    ) -> Result<Vec<Evaluation>> {
        // Your evaluation logic — return Vec<Evaluation> with stance + scores.
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
- [NATS topology](../explanation/nats-topology.md) — subjects, JetStream streams, JWT scopes
- [Sandboxed builtin tools](../reference/sandboxed-tools.md) — `read_file`, `grep_search` reference
