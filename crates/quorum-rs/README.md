# quorum-rs

[![Crates.io](https://img.shields.io/crates/v/quorum-rs.svg)](https://crates.io/crates/quorum-rs)
[![Docs.rs](https://docs.rs/quorum-rs/badge.svg)](https://docs.rs/quorum-rs)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

Rust SDK **and** CLI for multi-agent deliberation systems — agents that propose, evaluate, and reach quorum on outcomes over a NATS-based orchestration protocol. Library and binary ship in the same crate.

## Install

> **Status: pre-release.** No stable `0.x.y` has shipped yet — `0.7.0-rc.2` is the latest on crates.io. Cargo doesn't pick pre-releases without an explicit version pin, so commands below pin the RC.

As a library:

```toml
[dependencies]
quorum-rs = "0.7.0-rc.2"
```

As the `quorum` CLI (`run`, `status`, `trace`, `tui`, `init`):

```bash
cargo install quorum-rs --version 0.7.0-rc.2
```

Default features build the binary (`cli` + `tui`). To use as a pure library and skip the CLI deps (clap, ratatui, etc.):

```toml
[dependencies]
quorum-rs = { version = "0.7.0-rc.2", default-features = false, features = ["audit"] }
```

MSRV: Rust 1.85 (uses Edition 2024).

## CLI quick reference

```text
quorum init                Bootstrap an `nsed.yaml` workspace config
quorum run <task>          Submit a deliberation task to the orchestrator
quorum status              Health check + agent status
quorum trace <job_id>      Show a deliberation trace (verdict + evaluations)
quorum tui                 Interactive terminal UI (live deliberation view)
```

All commands read `./nsed.yaml` by default (`--config <path>` to override).

## What's inside

| Module | Key types | Purpose |
|---|---|---|
| `agents` | `NsedAgent`, `AgentContext`, `Proposal`, `Evaluation`, `ChatCapable`, `OutputLeakDetector` | Agent trait + deliberation data structures + pluggable output guard |
| `agents::config` | `AgentConfig`, `TaskPrecision` | Agent configuration (model, provider, limits) |
| `llms` | `AiModel`, `RequestConfig`, `SimpleOpenAIModel`, `OpenAICompatibleModel`, `RateLimiter` | LLM abstraction + production streaming client + simulator / stub for tests |
| `prompts` | `PromptSet`, `DefaultPromptSet` | Prompt template interface + benchmark-validated default proposer/evaluator templates |
| `tools` | `Tool`, `ToolDefinition`, `ScopedGrepTool`, `ScopedReadFileTool` | Tool-use interface (OpenAI function calling) + sandboxed filesystem tools |
| `workers` | `NatsNsedWorker`, `WorkerConfig`, `NatsScratchpadStore`, `WorkerHook`, `UserToolHandlerFactory` | NATS JetStream worker runtime |
| `middleware` | `AgentMiddleware`, `MiddlewarePipeline`, `MiddlewareConfig`, `RuleBasedMiddleware`, `LlmModerationMiddleware`, `BinaryMiddleware`, `DylibMiddleware` | Pluggable validation/moderation + YAML config + external-process dispatch |
| `status` | `AgentStatusSnapshot`, `SharedAgentStatus`, `server`, `multi_server` | Real-time status types + optional HTTP dashboard (feature `status-server`) |
| `nats_utils` | `connect_nats`, `sanitize_subject_component`, `NatsAuth` | NATS helpers + authentication |
| `telemetry` | `TelemetryEvent`, `TelemetryConfig`, `TelemetryEmitter` | Per-agent telemetry event catalog |

## Quick start — build a custom agent

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
    fn name(&self) -> &str {
        &self.name
    }

    async fn propose(&self, context: &AgentContext) -> Result<Proposal> {
        // your proposal logic — call self.llm, return a Proposal
        unimplemented!()
    }

    async fn evaluate(
        &self,
        context: &AgentContext,
        proposals: &[Proposal],
    ) -> Result<Vec<Evaluation>> {
        // your evaluation logic — return Vec<Evaluation> with stance + scores
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

## Use the reference agent

If you want the full ReAct loop, structured proposer/evaluator outputs, retry + repair, tool injection, and benchmark-validated prompts, use `ProposerEvaluatorAgent` instead of implementing `NsedAgent` from scratch:

```rust
use quorum_rs::agents::{AgentConfig, ProposerEvaluatorAgent};
use quorum_rs::llms::OpenAICompatibleModel;
use quorum_rs::prompts::defaults::DefaultPromptSet;
use quorum_rs::workers::{NatsNsedWorker, NatsNsedWorkerExt, WorkerConfig};

# async fn run() -> anyhow::Result<()> {
let agent_config = AgentConfig {
    name: "cortex-a".into(),
    provider_id: "openai".into(),
    model_name: "gpt-4o".into(),
    ..Default::default()
};

let agent = ProposerEvaluatorAgent::new(
    agent_config,
    Box::new(OpenAICompatibleModel::new(
        "https://api.openai.com/v1".into(),
        std::env::var("OPENAI_API_KEY")?,
        None,
    )),
    Box::new(DefaultPromptSet::new()),
    vec![],
    vec![],
);

let worker_config = WorkerConfig::new(
    "nats://localhost:4222".into(),
    "sphera_jobs".into(),
    "cortex_a_consumer".into(),
);
let worker = NatsNsedWorker::from_agent(agent, worker_config).await?;
worker.run().await
# }
```

## Pre-built agent shells

For agents you don't write in Rust:

| Type | Use when |
|---|---|
| `ExecAgent` | The agent is a process you exec (any language); deliberation I/O via stdin/stdout JSON |
| `McpAgent` / `ClaudeAgent` | The agent is an MCP server (Claude Code, generic MCP) wrapped via the SDK runtime |

## Features

- `default = ["audit"]`
- `audit` — enables cryptographic signing of agent outputs via `quorum-crypto-core`
- `status-server` — embedded axum dashboard for live agent + fleet status (HTTP)

## Sister crates

Part of the [`quorum-rs` workspace](https://github.com/peeramid-labs/quorum-rs):

- [`llm-repair`](https://crates.io/crates/llm-repair) — JSON-repair / markdown-extraction / tool-call recovery for malformed LLM output
- [`quorum-crypto-core`](https://crates.io/crates/quorum-crypto-core) — ed25519 / secp256k1 / SHA3 + audit envelope (used by the `audit` feature)

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/peeramid-labs/quorum-rs/blob/HEAD/LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
