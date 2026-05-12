# nsed-agent-sdk

![Coverage](https://img.shields.io/badge/coverage-89.3%25-brightgreen?logo=rust)

Trait definitions, data types, and runtime for building NSED-compatible deliberation agents. Implement the traits, connect to an orchestrator, and your agent participates in multi-agent deliberation sessions.

## What's Inside

| Module | Key Types | Purpose |
|---|---|---|
| `agents` | `NsedAgent`, `AgentContext`, `Proposal`, `Evaluation`, `ChatCapable`, `ClaimAssessment`, `DisagreementPoint`, `CategoryScores`, `Stance`, `ClaimVerdict` | Agent trait + deliberation data structures (including structured evaluation types) |
| `agents::config` | `AgentConfig`, `TaskPrecision` | Agent configuration (model, provider, limits) |
| `llms` | `AiModel`, `RequestConfig`, `SimpleOpenAIModel` | Language model abstraction + minimal client |
| `prompts` | `PromptSet` | Prompt template interface |
| `tools` | `Tool`, `ToolDefinition` | Tool-use interface (OpenAI function calling format) |
| `workers` | `NatsNsedWorker`, `WorkerConfig`, `NatsScratchpadStore`, `WorkerHook` | NATS JetStream worker runtime |
| `status` | `AgentStatusSnapshot`, `EventLogEntry`, `SharedAgentStatus` | Real-time status monitoring types |
| `nats_utils` | `validate_nats_name`, `sanitize_subject_component`, `connect_nats` | NATS helpers + authentication |

## Quick Start — Build a Custom Agent

```rust
use quorum_rs::agents::{NsedAgent, AgentContext, AgentConfig, Proposal, Evaluation, Stance};
use quorum_rs::llms::SimpleOpenAIModel;
use quorum_rs::workers::{NatsNsedWorker, WorkerConfig};
use async_trait::async_trait;
use anyhow::Result;

struct MyAgent {
    name: String,
    model: SimpleOpenAIModel,
}

#[async_trait]
impl NsedAgent for MyAgent {
    fn name(&self) -> String { self.name.clone() }

    async fn propose(&self, ctx: &AgentContext) -> Result<Proposal> {
        // Your proposal logic — call self.model, use ctx.task_description, etc.
        Ok(Proposal {
            content: format!("My solution to: {}", ctx.task_description),
            thought_process: "Reasoning...".into(),
            token_usage: None,
        })
    }

    async fn evaluate(&self, ctx: &AgentContext) -> Result<Vec<Evaluation>> {
        // Score each candidate proposal
        Ok(ctx.candidate_proposals.iter().map(|_p| Evaluation {
            score: 0.8,
            justification: "Solid approach.".into(),
            stance: Some(Stance::Agree),
            ..Default::default()
        }).collect())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let agent = MyAgent {
        name: "my-agent".into(),
        model: SimpleOpenAIModel::new(
            "https://api.openai.com/v1".into(),
            std::env::var("OPENAI_API_KEY")?,
        ),
    };

    let agent_config = AgentConfig {
        name: "my-agent".into(),
        provider_id: "openai".into(),
        model_name: "gpt-4o-mini".into(),
        ..Default::default()
    };

    let config = WorkerConfig::new(
        "nats://localhost:4222".into(),
        "sphera_jobs".into(),
        "my_agent_consumer".into(),
    );

    let worker = NatsNsedWorker::new(agent, agent_config, config).await?;
    worker.run().await
}
```

## Implement a Custom LLM

```rust
use quorum_rs::llms::{AiModel, RequestConfig};
use quorum_rs::agents::AgentConfig;
use async_trait::async_trait;
use async_openai::types::CreateChatCompletionResponse;

struct MyModel { /* ... */ }

#[async_trait]
impl AiModel for MyModel {
    async fn chat_completion(
        &self,
        config: &AgentConfig,
        request: RequestConfig,
    ) -> Result<(CreateChatCompletionResponse, String), Box<dyn std::error::Error + Send + Sync>> {
        // Your LLM call here
        todo!()
    }
}
```

## Traits Overview

| Trait | Required Methods | Purpose |
|---|---|---|
| `NsedAgent` | `propose()`, `evaluate()`, `name()` | Core agent logic — your main implementation target |
| `AiModel` | `chat_completion()` | LLM provider abstraction |
| `PromptSet` | `get_system_message()`, `get_proposer_prompt()`, `get_batch_evaluator_prompt()`, `get_summarizer_prompt()` | Prompt template collection |
| `Tool` | `name()`, `schema()`, `call()` | Tool-use in OpenAI function calling format |
| `PersistenceStore` | `get()`, `set()`, `append()`, `get_round_history()` | Durable key-value storage for agent memory |
| `ChatStrategy` | `prepare_request()`, `parse_response()` | Provider-specific request/response adaptation |
| `TokenEstimator` | `estimate_tokens()` | Token counting for budget management |
| `WorkerHook` | `before_publish()` | Intercept NATS publishes (default: passthrough) |
| `ChatCapable` | `chat()` | Direct LLM conversation (bypasses deliberation) |
| `UserToolHandlerFactory` | `create()` | Factory for per-task user tool handlers |

All DynClone-enabled traits (`NsedAgent`, `AiModel`, `PromptSet`, `Tool`) can be used as `Box<dyn Trait>` and cloned.

## Runtime Types

| Type | Purpose |
|---|---|
| `NatsNsedWorker` | NATS JetStream worker — connects agent to orchestrator, handles task dispatch, idempotency, scratchpad, heartbeats |
| `WorkerConfig` | Connection config: NATS URL, stream name, consumer name, subject prefix, auth |
| `NatsScratchpadStore` | `PersistenceStore` backed by NATS KV — durable agent memory across rounds |
| `JobManifest` | Job manifest broadcast by orchestrator — lists selected agents and task |
| `SimpleOpenAIModel` | Minimal `AiModel` — direct POST to `/chat/completions`, no streaming/rate limiting |
| `AgentStatusSnapshot` | Real-time agent state: identity, counters, current job, event log |

## Crate Relationships

```text
nsed-agent-sdk  <--  nsed-agent  <--  nsed-orchestrator
   traits             impls              server
   + runtime          + extensions
```

- **This crate** (`nsed-agent-sdk`): Trait definitions, data types, worker runtime, and minimal LLM client. **MIT licensed** — safe to depend on without any commercial restrictions.
- **`nsed-agent`**: Reference implementations (OpenAI-compatible model with rate limiting + strategies, default prompts, LLM repair, tool implementations, user tool handler). Re-exports SDK types and adds extension traits.
- **`nsed-orchestrator`**: The NATS-based orchestrator server.

### What You Need

| Goal | Depend On |
|---|---|
| Build a fully custom agent from scratch | `nsed-agent-sdk` only |
| Use the reference agent + LLM client | `nsed-agent` |
| Run the orchestrator | `nsed-orchestrator` |

### Extension Points

The worker runtime provides trait-based hooks so `nsed-agent` can inject additional functionality without modifying SDK code:

| Trait | Purpose | Implementation in `nsed-agent` |
|---|---|---|
| `WorkerHook` | Intercept NATS publishes before send | Crypto wrapping for commit-reveal |
| `UserToolHandlerFactory` | Create per-task user tool handlers | `NatsUserToolHandlerFactory` |
| `ChatCapable` | Direct LLM chat for status dashboard | `ProposerEvaluatorAgent::chat()` |

See the [Agent Development Guide](../../docs/agent-development.md) for complete examples of both approaches.

