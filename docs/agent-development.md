# Agent Development Guide

This guide covers how to build, configure, and run NSED agents — either using the reference implementation or by implementing your own from scratch.

## Architecture Overview

```text
Orchestrator (NATS Admin)              Agent Process (Subscriber/Publisher)
========================              ====================================

  Creates stream "sphera_jobs"          WorkerConfig::new(nats_url, stream, consumer)
  Publishes task to:                   NatsNsedWorker subscribes to:
    nsed.{session}.task.{agent}.propose     nsed.*.task.{agent_id}.*
    nsed.{session}.task.{agent}.evaluate
                                      Agent receives AgentContext, runs propose/evaluate
  Watches for results on:             Publishes result to:
    nsed.{session}.result.{round}.*        nsed.{session}.result.{round}.{agent_id}.{action}

  Broadcasts manifests to:            Watches manifests on:
    nsed.jobs.manifest.{job_id}            nsed.jobs.manifest.>
```

The orchestrator and agents are **separately launchable**. The orchestrator manages the NATS stream and dispatches tasks; agents connect independently and can join/leave at any time.

## Crate Structure

| Crate | When to Use |
|---|---|
| `nsed-agent-sdk` | Building a fully custom agent from scratch |
| `nsed-agent` | Using or extending the reference implementation |

## Option 1: Use the Reference Agent

The fastest path — use `ProposerEvaluatorAgent` with your own model and configuration.

### Dependencies

```toml
[dependencies]
# Once published to crates.io:
nsed-agent = "0.1"
nsed-agent-sdk = "0.1"
# Or from git (pre-release):
# nsed-agent = { git = "https://github.com/peeramid-labs/nsed", branch = "dev" }
# nsed-agent-sdk = { git = "https://github.com/peeramid-labs/nsed", branch = "dev" }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

### Minimal Example

```rust
use nsed_agent::agents::ProposerEvaluatorAgent;
use nsed_agent::llms::OpenAICompatibleModel;
use nsed_agent::prompts::defaults::DefaultPromptSet;
use nsed_agent::workers::{NatsNsedWorker, NatsNsedWorkerExt, WorkerConfig};
use nsed_agent_sdk::agents::AgentConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let agent_config = AgentConfig {
        name: "my-agent".into(),
        provider_id: "together_ai".into(),
        model_name: "MiniMaxAI/MiniMax-M2.5".into(),
        temperature: 0.7,
        max_react_iterations: Some(10),
        ..Default::default()
    };

    let model = OpenAICompatibleModel::new(
        "https://api.together.xyz/v1".into(),
        std::env::var("TOGETHER_AI_API_KEY")?,
        None,
    );

    let agent = ProposerEvaluatorAgent::new(
        agent_config,
        Box::new(model),
        Box::new(DefaultPromptSet::new()),
        vec![],  // propose-phase tools
        vec![],  // evaluate-phase tools
    );

    // consumer_name must be unique per agent instance — duplicate names cause
    // NATS queue-group behavior where instances compete for messages.
    // Tip: append a UUID or hostname for multi-instance deployments.
    // For untrusted networks, use NATS JWT auth with scoped subject permissions
    // (see "Per-Agent Subject Isolation" section below).
    let config = WorkerConfig::new(
        "nats://localhost:4222".into(),
        "sphera_jobs".into(),
        "my_agent_consumer".into(),
    );

    // from_agent() auto-wires BSL extensions (user tool handler, chat support)
    let worker = NatsNsedWorker::from_agent(agent, config).await?;
    worker.run().await
}
```

### Running

```bash
# Terminal 1: Start NATS
docker run -d --name nats -p 4222:4222 nats:latest -js

# Terminal 2: Start orchestrator
cargo run -p nsed-orchestrator

# Terminal 3: Start your agent
TOGETHER_AI_API_KEY=... cargo run --bin my-agent
```

## Option 2: External Process Agent (Exec Provider)

Run **any language** as an NSED agent — Python, TypeScript, shell scripts, LangChain pipelines — without writing Rust or speaking NATS. The Rust exec agent bridges NATS ↔ stdin/stdout for you. For Claude CLI, use the dedicated [`type: claude` provider](#option-3b-claude-cli-provider-zero-code) instead.

```text
Orchestrator ──NATS──► NatsNsedWorker ──► ExecAgent ──stdin/stdout──► Your Script
```

### YAML Configuration

```yaml
providers:
  exec_local:
    type: exec

agents:
  - name: PYTHON_ANALYST
    provider_id: exec_local
    model_name: custom
    exec:
      command: ["python3", "my_agent.py"]
      # working_dir: "/opt/agents"
      # timeout_secs: 30
      # env:
      #   OPENAI_API_KEY: "sk-..."
```

### Minimal Python Agent

```python
#!/usr/bin/env python3
import json, sys

def main():
    envelope = json.loads(sys.stdin.read())
    phase = envelope["phase"]
    ctx = envelope["context"]

    if phase == "propose":
        result = {
            "thought_process": f"Analyzing: {ctx['task_description']}",
            "content": "My proposal...",
        }
    elif phase == "evaluate":
        result = {
            "evaluations": [
                {"target_id": c["id"], "score": 0.8, "justification": "Good work"}
                for c in ctx.get("candidates", [])
            ]
        }
    else:
        sys.exit(1)

    # Use delimiters to resist stdout pollution from libraries
    print("___NSED_START___")
    print(json.dumps(result))
    print("___NSED_END___")

if __name__ == "__main__":
    main()
```

See [`crates/nsed-cli/examples/exec_agent.py`](../crates/nsed-cli/examples/exec_agent.py) for a complete reference and [`docs/exec-agent-protocol.md`](exec-agent-protocol.md) for the full protocol specification.

## Option 3: MCP Agent (Tool-Capable External Process)

When your external agent needs to **research** during deliberation — reading past proposals, searching history, maintaining persistent memory — use the MCP provider. It extends the exec protocol with a bidirectional tool-calling channel via the [Model Context Protocol](https://modelcontextprotocol.io/).

```text
Orchestrator ──NATS──► NatsNsedWorker ──► McpAgent ──stdin+MCP──► Your Agent
                                              │
                                              └── pushes context via stdin
                                              └── MCP tools: search, read, scratchpad
                                              └── terminal: nsed_propose / nsed_evaluate
```

### YAML Configuration

```yaml
providers:
  mcp_local:
    type: mcp

agents:
  - name: RESEARCH_AGENT
    provider_id: mcp_local
    model_name: custom
    mcp:
      command: ["python3", "agents/mcp_agent.py"]
      timeout_secs: 60
      env:
        OPENAI_API_KEY: "sk-..."
```

### How It Works

1. NSED pushes the `AgentContext` as a JSON line to stdin (same format as exec)
2. MCP server starts on the same stdin/stdout pipes
3. Your agent reads context, calls MCP tools as needed, then submits via `nsed_propose` or `nsed_evaluate`

### Available MCP Tools

| Tool | Description |
|------|-------------|
| `nsed_get_context` | Refresh deliberation context (also pushed via stdin) |
| `nsed_read_proposal` | Read a past proposal by agent ID and round |
| `nsed_read_critiques` | Read evaluation feedback from evaluators |
| `nsed_search` | Full-text search across deliberation history |
| `nsed_update_scratchpad` | Write to persistent cross-round memory |
| `nsed_propose` | Submit proposal (terminal — ends phase) |
| `nsed_evaluate` | Submit evaluations (terminal — ends phase) |

### When to Use MCP vs Exec

| Use Case | Provider |
|----------|----------|
| Simple scripts, CLI tools, one-shot responses | `exec` |
| LLM agents that need to research before responding | `mcp` |
| Agents that maintain working memory across rounds | `mcp` |
| Maximum simplicity (no MCP library needed) | `exec` |

See [`crates/nsed-cli/examples/mcp_agent.py`](../crates/nsed-cli/examples/mcp_agent.py) for a complete reference and [`docs/mcp-agent-protocol.md`](mcp-agent-protocol.md) for the full protocol specification.

## Option 3b: Claude CLI Provider (Zero-Code)

Use Claude CLI as an NSED deliberation agent with zero code — just YAML config. NSED automatically constructs Claude CLI flags, provides system prompts, manages session continuity across rounds, and connects Claude to NSED deliberation tools via an in-process HTTP MCP server.

```yaml
providers:
  claude_cli:
    type: claude

agents:
  - name: CLAUDE_REVIEWER
    provider_id: claude_cli
    model_name: sonnet
    persona: "You are a security-focused code reviewer"
    system_prompt_override: "Review all proposals for security vulnerabilities"
    claude:
      permission_mode: bypassPermissions
      max_budget_usd: 0.50
      context_files: ["docs/architecture.md"]
      add_dirs: ["./src", "./docs"]
      # writable: false          # default: read-only (Write/Edit/NotebookEdit blocked)
      # disallowed_tools: ["Bash"]  # additional tools to block
      allowed_tools: ["Read", "Grep"]
```

### How It Works

1. NSED builds the Claude CLI command from agent config fields (model, system prompt, persona, budget, etc.)
2. An in-process HTTP MCP server starts on localhost, exposing all NSED deliberation tools
3. Claude CLI connects to the MCP server via `"type": "http"` in `--mcp-config`
4. Claude reads the task context, uses `nsed_get_context` for details, and submits via `nsed_propose` / `nsed_evaluate`
5. Session continuity across rounds via `--session-id` (round 1) / `--resume` (round 2+)

### AgentConfig → Claude CLI Flags

| Config Field | CLI Flag | Notes |
|--------------|----------|-------|
| `model_name` | `--model` | Model selection (sonnet, opus, etc.) |
| `system_prompt_override` | `--system-prompt` | Full system prompt replacement |
| `persona` | `--append-system-prompt` | Appended to default prompt |
| `claude.permission_mode` | `--permission-mode` | Default: `bypassPermissions` |
| `claude.max_budget_usd` | `--max-budget-usd` | Cost control per phase |
| `claude.allowed_tools` | `--allowed-tools` | Tool access filter |
| `claude.context_files` | `--append-system-prompt` | File contents inlined (no dir access granted) |
| `claude.add_dirs` | `--add-dir` | Grant tool access to specific directories |
| `claude.writable` | `--disallowed-tools` | `false` (default) blocks Write/Edit/NotebookEdit |
| `claude.agents` | `--agents` | Sub-agent definitions (see MCP protocol docs) |

See [`docs/mcp-agent-protocol.md`](mcp-agent-protocol.md#claude-cli-provider) for the full Claude provider specification.

## Option 4: Build a Custom Agent (MIT SDK Only)

For maximum flexibility, implement the `NsedAgent` trait directly. You only need `nsed-agent-sdk` — MIT licensed, no BSL dependency.

### Dependencies

```toml
[dependencies]
nsed-agent-sdk = { git = "https://github.com/peeramid-labs/nsed" }
async-trait = "0.1"
anyhow = "1"
tokio = { version = "1", features = ["full"] }
```

### Custom Agent

```rust
use nsed_agent_sdk::agents::{NsedAgent, AgentContext, AgentConfig, Proposal, Evaluation, TokenUsage};
use nsed_agent_sdk::workers::{NatsNsedWorker, WorkerConfig};
use async_trait::async_trait;
use anyhow::Result;

pub struct MyAgent {
    name: String,
}

#[async_trait]
impl NsedAgent for MyAgent {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn propose(&self, ctx: &AgentContext) -> Result<Proposal> {
        // Your proposal logic here
        // ctx.task_description — what the user asked
        // ctx.round_number — current deliberation round
        // ctx.previous_proposals — proposals from prior rounds
        // ctx.injection_content — any user injections

        Ok(Proposal {
            content: format!("My solution to: {}", ctx.task_description),
            thought_process: "Step-by-step reasoning...".into(),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            }),
        })
    }

    async fn evaluate(&self, ctx: &AgentContext) -> Result<Vec<Evaluation>> {
        // Evaluate each candidate proposal
        let mut evaluations = vec![];

        for (i, proposal) in ctx.candidate_proposals.iter().enumerate() {
            evaluations.push(Evaluation {
                proposal_index: i,
                agent_id: proposal.agent_id.clone(),
                score: 0.8,  // 0.0 to 1.0
                critique: "Good approach, well-structured.".into(),
                token_usage: None,
            });
        }

        Ok(evaluations)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let agent = MyAgent { name: "my-custom-agent".into() };
    let agent_config = AgentConfig {
        name: "my-custom-agent".into(),
        provider_id: "custom".into(),
        model_name: "my-model".into(),
        ..Default::default()
    };

    let config = WorkerConfig::new(
        "nats://localhost:4222".into(),
        "sphera_jobs".into(),
        "my_custom_consumer".into(),
    );

    let worker = NatsNsedWorker::new(agent, agent_config, config).await?;
    worker.run().await
}
```

The SDK also provides `SimpleOpenAIModel` — a minimal `AiModel` implementation that makes direct POST calls to any compatible API (no streaming, no rate limiting):

```rust
use nsed_agent_sdk::llms::SimpleOpenAIModel;

let model = SimpleOpenAIModel::new(
    "https://api.together.xyz/v1".into(),
    std::env::var("TOGETHER_AI_API_KEY")?,
);
```

### Custom LLM Backend

```rust
use nsed_agent_sdk::llms::{AiModel, RequestConfig};
use nsed_agent_sdk::agents::AgentConfig;
use async_trait::async_trait;
use async_openai::types::CreateChatCompletionResponse;

pub struct MyModel {
    // Your model client
}

#[async_trait]
impl AiModel for MyModel {
    async fn chat_completion(
        &self,
        config: &AgentConfig,
        request: RequestConfig,
    ) -> Result<(CreateChatCompletionResponse, String), Box<dyn std::error::Error + Send + Sync>> {
        // Call your LLM and return the response
        todo!()
    }
}
```

## Agent Configuration Reference

### `AgentConfig` Fields

| Field | Type | Default | Description |
|---|---|---|---|
| `name` | `String` | required | Unique agent identifier |
| `model` | `Option<String>` | `None` | Dotpath model reference: `"provider_id.model_key"`. Resolves provider and merges `ModelDef` fields. Replaces `provider_id` + `model_name` + LLM fields. |
| `provider_id` | `String` | `""` | References a provider in config (legacy; set automatically when `model` is used) |
| `model_name` | `String` | `""` | Model identifier sent to the API (legacy; set from `ModelDef` when `model` is used) |
| `temperature` | `f32` | `0.0` | Sampling temperature |
| `max_tokens` | `u32` | `0` | Max output tokens (0 = provider default) |
| `persona` | `Option<String>` | `None` | Personality injected into system prompt |
| `system_prompt_override` | `Option<String>` | `None` | Fully replaces auto-generated system prompt |
| `max_react_iterations` | `Option<i32>` | `None` (10) | Max ReAct tool-use loop iterations |
| `max_retries` | `Option<i32>` | `None` (3) | Max LLM API call retries |
| `context_window` | `i32` | `128000` | Model context window size in tokens |
| `max_scratchpad_size` | `i32` | `32768` | Max scratchpad memory in tokens |
| `scratchpad_limit` | `i32` | `2000` | Max scratchpad entries in context |
| `use_streaming` | `bool` | `true` | Use streaming API responses |
| `merge_system_prompt` | `bool` | `false` | Merge system into first user message |
| `disable_native_tools` | `bool` | `false` | Don't send tool definitions to API |
| `unwrap_hallucinated_tool_calls` | `bool` | `false` | Parse tool calls from plain text |
| `repair_invalid_escapes` | `bool` | `true` | Fix invalid JSON escapes in output |
| `json_mode` | `bool` | `false` | Request JSON output mode |
| `textual_feedback` | `bool` | `true` | Exchange textual critiques |
| `presence_penalty` | `Option<f32>` | `None` | Sampling presence penalty |
| `frequency_penalty` | `Option<f32>` | `None` | Sampling frequency penalty |
| `reasoning_effort` | `Option<String>` | `None` | Provider-specific reasoning hint |
| `capability_tags` | `Vec<String>` | `[]` | Tags for policy-based scheduling (e.g. `["security:owasp", "lang:rust"]`) |
| `description` | `Option<String>` | `None` | Short description of the agent's specialization |
| `input_price_per_mtok` | `Option<f64>` | `None` | Input cost per million tokens |
| `output_price_per_mtok` | `Option<f64>` | `None` | Output cost per million tokens |
| `exec` | `Option<ExecProviderConfig>` | `None` | Exec provider config (see [exec protocol](exec-agent-protocol.md)) |
| `mcp` | `Option<McpProviderConfig>` | `None` | MCP provider config (see [MCP protocol](mcp-agent-protocol.md)) |
| `claude` | `Option<ClaudeProviderConfig>` | `None` | Claude provider config (see [claude protocol](mcp-agent-protocol.md#claude-provider)) |

### `WorkerConfig` Fields

| Field | Type | Default | Description |
|---|---|---|---|
| `nats_url` | `String` | required | NATS server URL |
| `stream_name` | `String` | required | JetStream stream name |
| `consumer_name` | `String` | required | Durable consumer name |
| `subject_prefix` | `String` | `"nsed"` | Subject namespace |
| `scratchpad_retention_secs` | `u64` | `604800` (7 days) | Scratchpad TTL (0 = no TTL) |
| `nats_auth` | `Option<NatsAuth>` | `None` | NATS authentication credentials |

## YAML Configuration

Agents and providers are configured in the agent crate's `config/default.yml` (with `{NSED_ENV}.yml` overlay for local API keys). The orchestrator discovers agents automatically via NATS heartbeat — no agent configuration in orchestrator config is needed.

### Provider Configuration

Providers define LLM endpoints. Model definitions live under `models:` — agents
reference them via a dotpath `"provider_id.model_key"`.

```yaml
# crates/nsed-agent/config/default.yml
providers:
  # Together AI (OpenAI-compatible API)
  together_ai:
    type: openai
    base_url: "https://api.together.xyz/v1"
    # api_key: set via APP_PROVIDERS__TOGETHER_AI__API_KEY env var or in local.yml
    concurrency: 500
    qps: 50
    models:
      minimax-m2.5:
        model_name: "MiniMaxAI/MiniMax-M2.5"
        temperature: 0.7
        max_tokens: 4096
        context_window: 128000
        input_price_per_mtok: 0.15
        output_price_per_mtok: 0.60
      kimi-k2.5:
        model_name: "moonshotai/Kimi-K2.5"
        temperature: 0.7
        max_tokens: 4096
      qwen3-coder:
        model_name: "Qwen/Qwen3-Coder-Next-FP8"
        temperature: 0.5
        max_tokens: 8192
      gpt-oss-120b:
        model_name: "openai/gpt-oss-120b"
        temperature: 0.0
        reasoning_effort: "medium"

  # Local Ollama
  ollama_local:
    type: openai
    base_url: "http://localhost:11434/v1"
    api_key: "ollama"
    concurrency: 1
    models:
      qwen3-8b:
        model_name: "qwen3:8b"
        temperature: 0.5
        max_tokens: 8096
        merge_system_prompt: true
        disable_native_tools: true
        use_streaming: false

  # Claude CLI
  claude_cli:
    type: claude
    models:
      opus:
        model_name: "opus"
      sonnet:
        model_name: "sonnet"

  # Simulated (no API calls — for testing/demos)
  simulated:
    type: simulated
    latency_ms: 400
    concurrency: 100
```

### Agent Configuration

Agents use `model: "provider_id.model_key"` to reference a model definition.
LLM parameters (temperature, max_tokens, etc.) are inherited from the model
definition; agent-level overrides take precedence if explicitly set.

```yaml
agents:
  - name: "ANALYST"
    model: "together_ai.minimax-m2.5"
    persona: "You are a financial analyst..."
    capability_tags: ["finance", "analysis", "quantitative"]
    description: "Financial analysis specialist"

  - name: "CODER"
    model: "ollama_local.qwen3-8b"
    capability_tags: ["lang:rust", "lang:python", "coding"]

  - name: "PL_Product"
    model: "claude_cli.opus"
    claude:
      add_dirs: ["./product-docs"]
    persona: "Product domain expert"
    capability_tags: ["product"]
```

> **Backward compatibility:** the legacy flat format (`provider_id` + `model_name` +
> individual LLM fields on the agent) still works. When both `model` and `provider_id`
> are present, `model` takes precedence.

### Ensemble Patterns

Mix different models for diversity:

```yaml
agents:
  # Fast proposers (different perspectives)
  - name: "ALPHA"
    model: "together_ai.minimax-m2.5"

  - name: "BETA"
    model: "together_ai.kimi-k2.5"

  - name: "GAMMA"
    model: "together_ai.qwen3-coder"

  # Slow, careful critic (synthesizer)
  - name: "CRITIC"
    model: "together_ai.gpt-oss-120b"
```

## LLM Strategy Selection

The `OpenAICompatibleModel` auto-selects a strategy based on provider/engine:

| Strategy | Auto-Selected When | What It Does |
|---|---|---|
| `NativeStrategy` | Default | Standard tool calling API |
| `HarmonyStrategy` | `engine: "harmony"` | Encodes tools as XML in system prompt |
| `XmlRegexStrategy` | `engine: "vllm_xml_responses"` | XML tool calls with guided regex |

## Deployment Patterns

### Pattern 1: Quick Start (Development)

Orchestrator + all agents in two processes:

```bash
# Both processes with one command:
make dev-all

# Or simulation mode (no API keys needed):
make dev-sim-all
```

### Pattern 2: Separate Processes (Production)

Orchestrator on CPU node, agents on GPU nodes:

```bash
# CPU node: orchestrator
cargo run -p nsed-orchestrator

# GPU node 1: agent with API provider
cargo run -p nsed-agent --example standalone_agent

# GPU node 2: agent with local model
NSED_BASE_URL=http://localhost:11434/v1 NSED_MODEL=qwen3:8b \
  cargo run -p nsed-agent --example standalone_agent
```

> **About `standalone_agent`:** The [`standalone_agent`](../crates/nsed-agent/examples/standalone_agent.rs) example is a production-ready entry point that goes beyond the [minimal example](#minimal-example) above. It loads agent and provider configuration from the agent's `config/default.yml` (with environment overlay), supports NATS authentication via env vars (`NATS_TOKEN`, `NATS_USER`/`NATS_PASS`, `NATS_CREDS`), and optionally serves a live status dashboard (`make run-agent dashboard`). Use `NSED_AGENT_NAME` to select which agent from the config file to run (defaults to `DEFAULT`). Set `NSED_AGENT_NAME=ALL` to run all agents in a single process with a unified dashboard (see [Pattern 3: Multi-Agent Process](#pattern-3-multi-agent-process)).

### Pattern 3: Multi-Agent Process

Run all agents from `config/default.yml` in a single process with a unified dashboard:

```bash
# Run ALL agents from the agent config
make run-agents

# Run ALL agents with unified dashboard on port 9090
make run-agents dashboard

# Run a subset of agents by name
NSED_AGENT_NAME=REENTRY,STATIC,FUZZ make run-agents dashboard
```

Agents that fail to start (e.g. missing API key, NATS unreachable) are skipped — the runner continues with the remaining agents.

**Unified dashboard endpoints** (served on a single port):

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Multi-agent dashboard HTML |
| `GET` | `/api/agents` | List all agents with summary status |
| `GET` | `/api/agents/{name}/status` | Per-agent status snapshot |
| `GET` | `/api/agents/{name}/config` | Per-agent configuration |
| `POST` | `/api/agents/{name}/chat` | Chat with a specific agent |

**Programmatic usage** (`nsed-agent` BSL):

```rust
use nsed_agent::multi_agent::MultiAgentRunner;
use nsed_agent::workers::{NatsNsedWorker, NatsNsedWorkerExt, WorkerConfig};

let mut runner = MultiAgentRunner::new();

// Build and add agents
// runner.add_worker("AGENT_A", worker_a, config_a);
// runner.add_worker("AGENT_B", worker_b, config_b);

runner.enable_dashboard(9090);
runner.run().await?;
```

## Orchestrator Configuration

When agents use the JWT credential flow (see [Per-Agent Subject Isolation](#per-agent-subject-isolation-jwt-credentials)), they need to know which orchestrators to register with. This is configured via the `orchestrators` list in the agent YAML config.

### Process-Wide Orchestrators

Define a top-level `orchestrators` list in the agent config. All agents in the process register with every orchestrator in this list:

```yaml
# crates/nsed-agent/config/default.yml (or local.yml / simulation.yml)
orchestrators:
  - id: "primary"
    url: "http://localhost:8080"
    bearer_token: "${NSED_BEARER_TOKEN}"
  - id: "secondary"
    url: "http://orch-2:8080"
    bearer_token: "${NSED_BEARER_TOKEN_2}"
```

Each entry has the following fields:

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | `String` | No | Human-readable identifier. Derived from URL hostname if omitted. |
| `url` | `String` | Yes | HTTP base URL of the orchestrator. |
| `bearer_token` | `String` | No | Bearer token for API authentication. Supports `${VAR_NAME}` env var expansion. |

### Bearer Token Environment Variable Expansion

The `bearer_token` field supports `${VAR_NAME}` syntax. At startup, the agent resolves the environment variable and substitutes the value. If the variable is not set, a warning is logged and the raw string is used as-is.

```yaml
orchestrators:
  - url: "http://prod-orch:8080"
    bearer_token: "${NSED_BEARER_TOKEN}"   # resolved from env at startup
  - url: "http://staging-orch:8080"
    bearer_token: "literal-dev-token"       # used as-is (no expansion)
```

### Per-Agent Orchestrator Extensions

Individual agents can declare additional orchestrators in their `orchestrators` field. These are **additive** to the process-wide list:

```yaml
orchestrators:
  - id: "primary"
    url: "http://orch-1:8080"
    bearer_token: "${NSED_BEARER_TOKEN}"

agents:
  - name: "ALPHA"
    provider_id: "together_ai"
    model_name: "MiniMaxAI/MiniMax-M2.5"
    # No per-agent orchestrators — uses only the process-wide list.

  - name: "BETA"
    provider_id: "together_ai"
    model_name: "openai/gpt-oss-120b"
    orchestrators:
      - id: "special"
        url: "http://orch-special:8080"
        bearer_token: "${SPECIAL_TOKEN}"
    # BETA connects to BOTH "primary" AND "special".
```

### How the Final Orchestrator List Is Computed

The effective orchestrator list for each agent is:

```
config.orchestrators ++ agent.orchestrators
```

The process-wide list comes first, then the per-agent extensions are appended. The combined list is **deduplicated by `id`** (or by URL-derived hostname when `id` is omitted). When duplicates exist, **last wins** — so a per-agent entry with the same `id` as a process-wide entry overrides it.

### Fallback: Direct NATS Connection

When **no orchestrators** are configured (neither process-wide nor per-agent), the agent falls back to the `NATS_URL` environment variable for a direct NATS connection without JWT registration:

```bash
NATS_URL=nats://localhost:4222 cargo run -p nsed-agent --example standalone_agent
```

This is useful for development setups where the orchestrator and agents share the same NATS server without authentication.

### Runtime Orchestrator Addition

When the multi-agent dashboard is enabled, agents expose a `POST /api/orchestrators` endpoint for adding orchestrators at runtime without restarting:

```bash
curl -X POST http://localhost:9090/api/orchestrators \
  -H 'Content-Type: application/json' \
  -d '{
    "url": "http://new-orch:8080",
    "bearer_token": "my-token",
    "id": "new-orch",
    "agent_names": ["ALPHA", "BETA"]
  }'
```

The `agent_names` field is optional. If empty or omitted, all agents in the process connect to the new orchestrator. Active orchestrators can be listed via `GET /api/orchestrators`.

## NATS Authentication

By default, NSED connects to NATS **without authentication**. This is fine for development and single-machine deployments.

For production deployments where the NATS server is network-accessible, you should enable NATS-level authentication. NSED supports **token**, **user/password**, and **NKey credential file** authentication as first-class features.

### Standalone Agent (Environment Variables)

The standalone agent example reads auth from environment variables:

```bash
# Token auth
NATS_TOKEN=my-secret-token cargo run -p nsed-agent --example standalone_agent

# User/password auth
NATS_USER=agent-user NATS_PASS=agent-password cargo run -p nsed-agent --example standalone_agent

# NKey credential file
NATS_CREDS=/path/to/agent.creds cargo run -p nsed-agent --example standalone_agent
```

### Orchestrator (Config / Environment)

The orchestrator reads NATS auth from `config/local.yml` or environment variables:

```yaml
# config/local.yml
nats:
  url: "nats://nats-server:4222"
  auth:
    token: "my-secret-token"
    # --- OR ---
    # username: "orchestrator"
    # password: "orch-secret"
    # --- OR ---
    # creds_file: "/path/to/orchestrator.creds"
```

Environment variable overrides (take precedence over config):

```bash
APP_NATS__AUTH__TOKEN=my-secret-token
# --- OR ---
APP_NATS__AUTH__USERNAME=orchestrator
APP_NATS__AUTH__PASSWORD=orch-secret
# --- OR ---
APP_NATS__AUTH__CREDS_FILE=/path/to/orchestrator.creds
```

### Programmatic (SDK)

Use `NatsAuth` and `connect_nats()` from `nsed-agent-sdk`:

```rust
use nsed_agent_sdk::nats_utils::{NatsAuth, connect_nats};

let auth = NatsAuth {
    token: Some("my-secret-token".into()),
    ..Default::default()
};

let client = connect_nats("nats://nats-server:4222", Some(&auth)).await?;
```

For agents using `WorkerConfig`:

```rust
use nsed_agent::workers::WorkerConfig;
use nsed_agent_sdk::nats_utils::NatsAuth;

let config = WorkerConfig::new(nats_url, stream_name, consumer_name)
    .with_nats_auth(NatsAuth {
        token: Some("my-secret-token".into()),
        ..Default::default()
    });
```

### NATS Server Configuration

Configure your NATS server (`nats-server.conf`) with authentication:

```conf
# Token auth (simplest)
authorization {
  token: "my-secret-token"
}

# User/password with permissions
authorization {
  users = [
    # Orchestrator: full access
    { user: "orchestrator", password: "orch-secret",
      permissions: { publish: ">", subscribe: ">" } }

    # Agent: can only subscribe to its tasks and publish results
    { user: "agent", password: "agent-secret",
      permissions: {
        publish: ["nsed.*.result.>", "nsed.jobs.ack.>"]
        subscribe: ["nsed.*.task.>", "nsed.jobs.manifest.>"]
      } }
  ]
}
```

### NKey Authentication (Recommended for Production)

For production, use NATS NKey authentication with credential files:

```bash
# Generate keys
nsc add operator --name nsed
nsc add account --name ORCHESTRATOR
nsc add account --name AGENTS
nsc add user --name orchestrator --account ORCHESTRATOR
nsc add user --name agent-1 --account AGENTS

# Export credentials
nsc generate creds --name agent-1 --account AGENTS > agent-1.creds
```

Pass the credential file via `NatsAuth.creds_file` or `NATS_CREDS` environment variable.

### Per-Agent Subject Isolation (JWT Credentials)

When `credentials.enabled: true`, the orchestrator acts as a trusted authority that issues
per-agent NATS User JWTs with scoped publish/subscribe permissions. This provides
**cryptographic subject isolation** — agent ARCHIT physically cannot publish to DEVIN's
subjects because the NATS server rejects unauthorized access at the protocol level.

**Key ownership:** Agents generate their own Ed25519 NKey keypairs (private keys never
leave the agent). The orchestrator only sees the public key and signs a scoped JWT for it.

**Zero-config bootstrap:** Standalone agents only need `NSED_ORCHESTRATOR_URL` + `NSED_BEARER_TOKEN` —
the NATS server URL is provided by the orchestrator during registration.

#### How It Works

**Standalone agents**: Use the challenge-response registration protocol with a
SHA-256 hash commitment to hide the NATS URL until the agent proves key ownership:

```text
Agent                                    Orchestrator
  |  1. Generate User NKey (SU... seed)       |
  |  2. GET /credentials/challenge ────────>  |
  |  <── { pub_key, nats_url_hash, nonce }    |
  |  3. Sign "{nonce}:{pub_key}:{hash}"       |
  |  4. POST /credentials/register ────────>  |
  |  <── { user_jwt, nats_url }               |
  |  5. Verify SHA-256(nats_url) == hash      |
  |  6. Combine JWT + seed → .creds           |
  |  7. Connect to nats_url ══════════════> NATS Server
```

**Security properties:**
- The challenge only contains `hex(SHA-256(nats_url))` — an attacker who obtains a bearer token but never completes registration learns nothing about NATS infrastructure topology.
- The agent signs the hash, so the orchestrator cannot swap the URL between challenge and registration — the agent verifies `SHA-256(received_url) == signed_hash`.
- Nonces are single-use and time-limited to prevent replay attacks.

The SDK provides a helper that handles the entire flow:

```rust
use nsed_agent_sdk::nats_utils::{register_with_orchestrator, NatsAuth};

let result = register_with_orchestrator(
    "http://orchestrator:8080",
    "my-agent-id",
    "bearer-token",
).await?;

// result.nats_url — hash-verified NATS server URL from orchestrator
// result.creds    — .creds content for NatsAuth.inline_creds
// result.keypair  — the agent's Ed25519 NKey (for potential re-registration)

let auth = NatsAuth {
    inline_creds: Some(result.creds),
    ..Default::default()
};
```

Or use environment variables with `standalone_agent` (zero NATS config needed):

```bash
NSED_ORCHESTRATOR_URL=http://orchestrator:8080 \
NSED_BEARER_TOKEN=my-token \
NSED_AGENT_NAME=my-agent \
cargo run --example standalone_agent
```

#### Configuration

```yaml
# config/default.yml
credentials:
  enabled: true
  # Account NKey seed (SA... prefix). Auto-generated if not set.
  # Persist this to avoid re-registration after orchestrator restart.
  # account_seed: "SA..."
  jwt_expiry_secs: 86400      # 24 hours (default)
  challenge_expiry_secs: 300   # 5 minutes (default)
```

Environment variables:

```bash
APP_CREDENTIALS__ENABLED=true
APP_CREDENTIALS__ACCOUNT_SEED=SAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX
APP_CREDENTIALS__JWT_EXPIRY_SECS=86400
APP_CREDENTIALS__CHALLENGE_EXPIRY_SECS=300
```

#### Subject Permissions per Agent

Each JWT scopes the agent to its own subjects:

| Direction | Pattern | Purpose |
|-----------|---------|---------|
| Publish | `nsed.*.result.*.{agent_id}.*` | Proposals and evaluations |
| Publish | `*.agent.heartbeat.{agent_id}` | Heartbeats |
| Publish | `*.jobs.ack.>` | Job ACKs |
| Publish | `nsed.*.result.event.*` | SSE events |
| Publish | `_INBOX.>` | Request-reply |
| Subscribe | `nsed.*.task.{agent_id}.*` | Task assignments |
| Subscribe | `*.jobs.manifest.>` | Job manifests |
| Subscribe | `*.orchestrator.ping` | Orchestrator pings |
| Subscribe | `_INBOX.>`, `$JS.API.>` | Request-reply, JetStream |

#### NATS Server Setup

The NATS server must be configured with a JWT resolver for the orchestrator's Account key.
See [NATS JWT documentation](https://docs.nats.io/running-a-nats-service/configuration/securing_nats/auth_intro/jwt) for resolver setup.

### TLS

For encrypted connections, use `tls://` or `nats://` with TLS configured on the NATS server:

```bash
NATS_URL=tls://nats-server:4222
```

## Operational Telemetry

When wired (PR3 / PR4 of [#309](https://github.com/peeramid-labs/nsed/issues/309)), every agent process publishes **metrics-only** events on `telemetry.agent.{agent_id}.*`:

- `llm_request_start` / `_complete` / `_failed` / `_stalled` (G2)
- `tool_call_executed`
- `retry_loop_attempt` (with G4 cumulative cost / tokens)
- `task_accepted` / `task_completed` / `task_failed`
- `nats_connection_state` (G6)

The event catalog and the matching Rust types live in `nsed-agent-sdk::telemetry` and are stable now (PR1). Subjects, fields, redaction rules (no prompts / proposals / `thought_process` / secrets), trace correlation, and opt-out are documented in [`docs/agent-telemetry.md`](agent-telemetry.md).

### Wiring custom emission

```rust
use nsed_agent_sdk::{TelemetryEmitter, TelemetrySource, TelemetryEvent, derive_trace_id};

let emitter = TelemetryEmitter::new(
    nats_client.clone(),
    TelemetrySource::Agent { agent_id: "MyAgent".into() },
);
emitter.emit(&TelemetryEvent::TaskAccepted(/* ... */));
```

`emit()` is fire-and-forget: serialization or publish failure is silently dropped (counted via `emitter.dropped_count()`), so telemetry never gates the critical path.

### Opt-out

```yaml
# In your agent fleet config
telemetry:
  enabled: false
```

Default is `enabled: true`. PR1 only parses this field; emission wiring lands in follow-up PRs of the stack.

## Agent Dashboard

The standalone agent includes an optional browser-based dashboard for real-time monitoring and direct LLM interaction.

### Enabling the Dashboard

```bash
# Via make target
make run-agent dashboard

# Custom port
make run-agent dashboard port=9091

# Via environment variable
NSED_DASHBOARD_PORT=9090 cargo run -p nsed-agent --example standalone_agent
```

### Dashboard Features

The dashboard has three tabs below the status overview:

**Event Stream** — Chronological log of agent lifecycle events, color-coded by type:
- Green: `agent_accepted`, `task_complete`
- Blue: `agent_working`
- Red: `agent_error`
- Yellow: `connected`
- Heartbeats are filtered by default to reduce noise.

**Chat** — Direct conversation with the agent's LLM provider. Messages use the agent's persona but bypass NSED deliberation instructions via an `<internal_voice>` wrapper. Useful for testing the agent's model and verifying persona behavior. Enter sends, Shift+Enter for newlines.

**Config** — Read-only view of the agent's configuration: name, model, provider, persona, temperature, max_tokens, native thinking support, tool format.

### Dashboard API

For programmatic access or custom integrations:

```bash
# Get agent status snapshot
curl http://localhost:9090/api/status | jq

# Get agent configuration
curl http://localhost:9090/api/config | jq

# Chat with the agent's LLM
curl -X POST http://localhost:9090/api/chat \
  -H 'Content-Type: application/json' \
  -d '{"messages": [{"role": "user", "content": "Hello, who are you?"}]}'
```

### Programmatic Usage

When using `nsed-agent` as a library (BSL):

```rust
use nsed_agent::workers::{NatsNsedWorkerExt, NatsNsedWorkerStatusExt};

let worker = NatsNsedWorker::from_agent(agent, config).await?
    .with_status_server(9090);  // Enable dashboard on port 9090
worker.run().await
```

When using `nsed-agent-sdk` only (MIT), the status dashboard is not available (it requires the `status-server` feature from `nsed-agent`), but the worker itself supports status monitoring via `AgentStatusSnapshot`:

```rust
use nsed_agent_sdk::workers::NatsNsedWorker;

let worker = NatsNsedWorker::new(agent, agent_config, config).await?
    .with_status(9090);  // Enables status tracking (no HTTP server)
worker.run().await
```

## Debugging

### Environment Variables

| Variable | Description |
|---|---|
| `RUST_LOG=nsed_agent=debug` | Enable debug logging for agent crate |
| `RUST_LOG=nsed_agent::llm_repair=trace` | Trace LLM repair pipeline |
| `NSED_ENV=simulation` | Use simulated LLMs (no API calls) |
| `NSED_DASHBOARD_PORT=9090` | Enable agent status dashboard on given port |

### Common Issues

| Symptom | Cause | Fix |
|---|---|---|
| "Stream not found after 10 attempts" | Orchestrator not running | Start orchestrator first |
| Agent not receiving tasks | Wrong `stream_name` or `subject_prefix` | Match orchestrator config |
| JSON parse errors in proposals | Model doesn't support tool calling | Set `disable_native_tools: true` + `merge_system_prompt: true` |
| Tool calls not working | Model hallucinating tool format | Set `unwrap_hallucinated_tool_calls: true` |
| Context window overflow | History too large | Reduce `scratchpad_limit` or `context_window` |
| Dashboard port already in use | Another process on same port | Change `NSED_DASHBOARD_PORT` or use `port=NNNN` with make |
| Chat returns error | LLM provider unreachable or API key missing | Check provider config and API keys |
