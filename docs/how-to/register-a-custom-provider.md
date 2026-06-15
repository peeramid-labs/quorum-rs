# Register a custom provider

You want `quorum serve` to recognise a `provider.type` the SDK doesn't ship — without forking it. Implement a `ProviderFactory`, register it, and hand the registry to `serve_fleet`.

This is the right path when your provider needs **its own dispatch key** in the fleet YAML. If you only need a one-off custom agent, implement [`NsedAgent`](agent-development.md) and wire it to a worker directly instead.

A complete, runnable version of everything below lives in the test [`crates/quorum-rs/tests/custom_provider_e2e.rs`](../../crates/quorum-rs/tests/custom_provider_e2e.rs) — it builds a `demo_cli` provider and drives it through `build_worker` over NATS. Copy it as a starting point.

## 1. Read your config from `provider_config`

Your provider doesn't get a typed section on the core `AgentConfig` (that's reserved for the built-ins). Instead, declare your own config type and read it from the generic [`provider_config`](../explanation/provider-registry.md) map with `AgentConfig::provider_config_as`:

```rust
#[derive(serde::Deserialize)]
struct CodexConfig {
    command: Vec<String>,      // argv of the codex CLI
    #[serde(default)]
    permission_mode: String,
    #[serde(default)]
    sandbox: bool,
}
```

## 2. Implement `ProviderFactory`

The factory receives the resolved `AgentConfig` and the `ProviderEntry` it points at, reads its config, and returns an `Arc<dyn NsedAgent>`.

```rust
use std::sync::Arc;
use quorum_rs::agents::config::AgentConfig;
use quorum_rs::agents::NsedAgent;
use quorum_rs::config::ProviderEntry;
use quorum_rs::providers::ProviderFactory;

struct CodexFactory;

impl ProviderFactory for CodexFactory {
    fn provider_type(&self) -> &str {
        "codex"
    }

    // `false` (the default) exempts this provider from placeholder
    // API-key validation. Return `true` if a remote deployment of
    // your provider needs a real key.
    fn requires_api_key(&self) -> bool {
        false
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        _provider: &ProviderEntry,
    ) -> anyhow::Result<Option<Arc<dyn NsedAgent>>> {
        // Deserialize the whole `provider_config:` block into your type.
        // (Return Ok(None) instead to skip this agent cleanly — the rest
        // of the fleet still boots.)
        let config: CodexConfig = agent_config.provider_config_as()?;
        let agent = CodexAgent::new(agent_config.name.clone(), config);
        Ok(Some(Arc::new(agent)))
    }
}
```

Your agent type must implement `NsedAgent` (`Send + Sync + Debug + Clone`). See the [agent development guide](agent-development.md#path-2-custom-rust-agent) for that trait.

### Reuse the subprocess plumbing

If your provider spawns a CLI (codex, aider, …), don't reimplement the spawn/timeout dance — the same helpers the built-in `exec`/`mcp` providers use are public in `quorum_rs::providers::cli_base`:

```rust
use quorum_rs::providers::cli_base;

// inside your NsedAgent::propose / evaluate:
let timeout = cli_base::effective_timeout(self.config.timeout_secs, ctx);
let mut child = cli_base::spawn_child(
    "codex",                 // provider label for error messages
    &self.name,
    &self.config.command,    // argv
    None,                    // working_dir
    &self.config.env,        // base env
    &[("CODEX_PERMISSION_MODE", self.config.permission_mode.clone())], // extra env, layered last
)?;
// drive child.stdin/stdout however your CLI's protocol works.
```

## 3. Register it and serve

```rust
use std::sync::Arc;
use quorum_rs::config::load_config;
use quorum_rs::providers::ProviderRegistry;
use quorum_rs::serve::{serve_fleet, ServeOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fleet = load_config(std::path::Path::new("agent.yml"))?;

    let mut registry = ProviderRegistry::with_builtins(); // keep exec/mcp/claude/openai/…
    registry.register(Arc::new(CodexFactory));

    let opts = ServeOptions {
        registry: Some(Arc::new(registry)),
        ..Default::default()
    };
    serve_fleet(&fleet, opts).await
}
```

`ProviderRegistry::with_builtins()` keeps every built-in provider; `register()` adds (or overrides) one type. If you want **only** your providers, start from `ProviderRegistry::empty()`.

## 4. Reference it from the fleet YAML

Operators add your agent with a `provider_config:` block — the map your factory deserializes in step 1. No Rust on their side:

```yaml
providers:
  my_codex:
    type: codex          # ← matches CodexFactory::provider_type()
agents:
  - name: CODEX_ALPHA
    provider_id: my_codex
    model_name: codex-mini
    provider_config:     # ← deserialized into your CodexConfig
      command: ["codex", "--exec"]
      permission_mode: "auto"
      sandbox: true
```

## Notes

- **Default behaviour is unchanged.** `ServeOptions::default()` leaves `registry: None`, which falls back to `with_builtins()`. Existing fleets need no edits.
- **Unknown types fail soft.** A `provider.type` with no registered factory is skipped (`Ok(None)`) with a warning listing the registered set — it does not abort the fleet.
- **Overriding a built-in.** Registering a factory whose `provider_type()` equals a built-in (e.g. `"exec"`) replaces it; the last `register()` wins.
- **The built-in `claude` provider uses this too.** `ClaudeFactory` reads its config from `provider_config` when no typed `claude:` section is present — so `provider_config_as` is exercised by a real shipping provider, not just custom ones.
- **API keys.** A provider whose factory returns `requires_api_key() == false` (the default) is treated as local and needs **no** `api_key` in its YAML — a local CLI provider like `codex` is keyless. Return `true` only if a remote deployment genuinely needs a key (then the entry needs one, unless its `base_url` is `localhost`).

For the design rationale, see [About the provider registry](../explanation/provider-registry.md).
