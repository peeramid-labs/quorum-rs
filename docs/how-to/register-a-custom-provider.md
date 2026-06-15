# Register a custom provider

You want `quorum serve` to recognise a `provider.type` the SDK doesn't ship — without forking it. Implement a `ProviderFactory`, register it, and hand the registry to `serve_fleet`.

This is the right path when your provider needs **its own dispatch key** in the fleet YAML. If you only need a one-off custom agent, implement [`NsedAgent`](agent-development.md) and wire it to a worker directly instead.

## 1. Implement `ProviderFactory`

The factory receives the resolved `AgentConfig` and the `ProviderEntry` it points at, and returns an `Arc<dyn NsedAgent>`.

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
        // Return Ok(None) to skip this agent cleanly (e.g. a required
        // config section is missing) — the rest of the fleet still boots.
        let agent = CodexAgent::new(agent_config.name.clone());
        Ok(Some(Arc::new(agent)))
    }
}
```

Your agent type must implement `NsedAgent` (`Send + Sync + Debug + Clone`). See the [agent development guide](agent-development.md#path-2-custom-rust-agent) for that trait.

## 2. Register it and serve

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

## 3. Reference it from the fleet YAML

```yaml
providers:
  my_codex:
    type: codex          # ← matches CodexFactory::provider_type()
agents:
  - name: CODEX_ALPHA
    provider_id: my_codex
    model_name: codex-mini
```

## Notes

- **Default behaviour is unchanged.** `ServeOptions::default()` leaves `registry: None`, which falls back to `with_builtins()`. Existing fleets need no edits.
- **Unknown types fail soft.** A `provider.type` with no registered factory is skipped (`Ok(None)`) with a warning listing the registered set — it does not abort the fleet.
- **Overriding a built-in.** Registering a factory whose `provider_type()` equals a built-in (e.g. `"exec"`) replaces it; the last `register()` wins.

For the design rationale, see [About the provider registry](../explanation/provider-registry.md).
