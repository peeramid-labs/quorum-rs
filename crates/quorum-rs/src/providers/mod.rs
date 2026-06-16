//! Provider registry + factory pattern.
//!
//! Adding a new agent provider used to mean editing the
//! `match provider.provider_type { … }` dispatch in
//! [`crate::serve::build_worker`], plus scattered `matches!()` checks
//! for things like "is this provider local (no API key needed)?".
//! That closed the provider set to whatever the SDK shipped.
//!
//! This module inverts the dependency: every provider is a
//! [`ProviderFactory`] registered in a [`ProviderRegistry`]. The
//! dispatch becomes a single map lookup, and a third-party crate can
//! `register()` its own factory before calling
//! [`crate::serve::serve_fleet`] — no SDK code change required.
//!
//! ```text
//!   provider.provider_type ─► ProviderRegistry::build_agent
//!                                   │
//!                                   ▼
//!                          ProviderFactory::build_agent
//!                                   │
//!                                   ▼
//!                           Arc<dyn NsedAgent>
//! ```
//!
//! The built-in factories (`exec`, `mcp`, `claude`, `openai`,
//! `openai-oauth`, `ollama`, `simulated`) preserve the exact behaviour
//! of the old hard-coded dispatch, including the "skip this agent cleanly
//! (`Ok(None)`)" semantics when a config section is missing.

/// Built-in [`ProviderFactory`] implementations registered by
/// [`ProviderRegistry::with_builtins`].
pub mod builtins;
/// Shared subprocess spawn + timeout helpers for CLI-agent providers.
/// Public so third-party `ProviderFactory` crates (e.g. a `codex`
/// provider) can reuse the exact spawn/timeout plumbing the built-in
/// `exec` / `mcp` providers use, instead of reimplementing it.
pub mod cli_base;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::agents::NsedAgent;
use crate::agents::config::AgentConfig;
use crate::config::ProviderEntry;

/// Constructs an [`NsedAgent`] from resolved fleet config for one
/// `provider_type`. Register implementations in a [`ProviderRegistry`].
///
/// Returning `Ok(None)` means "skip this agent cleanly" — e.g. the
/// `provider_type` is `exec` but the agent has no `exec:` config
/// section. The fleet keeps booting its other agents. Return `Err`
/// only for failures that are worth surfacing as the worker-build
/// error (they don't currently abort the whole fleet either; the
/// caller logs and continues).
pub trait ProviderFactory: Send + Sync {
    /// The `provider.type` string this factory handles (e.g. `"exec"`,
    /// `"codex"`). The registry keys factories by this value.
    fn provider_type(&self) -> &str;

    /// Whether a *remote* deployment of this provider needs a real API
    /// key. Local providers (subprocess agents, the local simulator)
    /// return `false`, which exempts them from placeholder-API-key
    /// validation. Defaults to `false`.
    fn requires_api_key(&self) -> bool {
        false
    }

    /// Build the agent from its resolved [`AgentConfig`] and the
    /// [`ProviderEntry`] it points at.
    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>>;
}

/// Maps `provider_type` → [`ProviderFactory`]. Built-ins are
/// registered via [`ProviderRegistry::with_builtins`]; third parties
/// add their own with [`ProviderRegistry::register`].
pub struct ProviderRegistry {
    factories: HashMap<String, Arc<dyn ProviderFactory>>,
}

impl ProviderRegistry {
    /// An empty registry. Use [`ProviderRegistry::with_builtins`]
    /// unless you want to opt out of the SDK's built-in providers.
    pub fn empty() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// A registry preloaded with every built-in provider: `exec`,
    /// `mcp`, `claude`, `openai`, `ollama`, `simulated`.
    pub fn with_builtins() -> Self {
        let mut registry = Self::empty();
        registry.register(Arc::new(builtins::ExecFactory));
        registry.register(Arc::new(builtins::McpFactory));
        registry.register(Arc::new(builtins::ClaudeFactory));
        registry.register(Arc::new(builtins::OpenAICodexFactory));
        // One OpenAI-compatible factory per wire-compatible type. Only
        // `openai` requires an API key; `ollama` and `simulated` are
        // local (the old dispatch exempted them via `is_local`).
        registry.register(Arc::new(builtins::OpenAiCompatibleFactory::new(
            "openai", true,
        )));
        registry.register(Arc::new(builtins::OpenAiCompatibleFactory::new(
            "ollama", false,
        )));
        registry.register(Arc::new(builtins::OpenAiCompatibleFactory::new(
            "simulated",
            false,
        )));
        registry
    }

    /// Register (or replace) the factory for its `provider_type`.
    pub fn register(&mut self, factory: Arc<dyn ProviderFactory>) {
        self.factories
            .insert(factory.provider_type().to_string(), factory);
    }

    /// The factory for `provider_type`, if registered.
    pub fn get(&self, provider_type: &str) -> Option<&Arc<dyn ProviderFactory>> {
        self.factories.get(provider_type)
    }

    /// Sorted list of registered `provider_type`s — for diagnostics.
    pub fn provider_types(&self) -> Vec<String> {
        let mut types: Vec<String> = self.factories.keys().cloned().collect();
        types.sort();
        types
    }

    /// Whether `provider_type` is a local provider (registered and not
    /// requiring an API key). Unknown types are not local.
    pub fn is_local(&self, provider_type: &str) -> bool {
        self.factories
            .get(provider_type)
            .map(|factory| !factory.requires_api_key())
            .unwrap_or(false)
    }

    /// Dispatch to the matching factory. Unknown `provider_type`s are
    /// skipped (`Ok(None)`) with a warning that lists the supported
    /// set — mirroring the old catch-all arm.
    pub fn build_agent(
        &self,
        provider_type: &str,
        agent_config: &AgentConfig,
        provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        match self.factories.get(provider_type) {
            Some(factory) => factory.build_agent(agent_config, provider),
            None => {
                warn!(
                    agent = %agent_config.name,
                    provider_type = %provider_type,
                    supported = ?self.provider_types(),
                    "unknown provider_type — skipping"
                );
                Ok(None)
            }
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("provider_types", &self.provider_types())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fleet from YAML and resolve one agent's
    /// `(AgentConfig, ProviderEntry)` — same fixture style as the
    /// `serve` tests, immune to field additions on the config structs.
    fn resolve(yaml: &str, agent: &str) -> (AgentConfig, ProviderEntry) {
        let fleet: crate::config::AgentFleetConfig =
            serde_yaml::from_str(yaml).expect("fleet yaml must parse");
        crate::config::load_agent_from_config(&fleet, agent).expect("agent must resolve")
    }

    #[test]
    fn builtins_registers_all_provider_types() {
        let registry = ProviderRegistry::with_builtins();
        assert_eq!(
            registry.provider_types(),
            vec![
                "claude",
                "exec",
                "mcp",
                "ollama",
                "openai",
                "openai-oauth",
                "simulated"
            ]
        );
    }

    #[test]
    fn is_local_matches_old_dispatch_exemptions() {
        let registry = ProviderRegistry::with_builtins();
        // Local providers (no API key) — exempt from key validation.
        for local in [
            "exec",
            "mcp",
            "claude",
            "openai-oauth",
            "ollama",
            "simulated",
        ] {
            assert!(registry.is_local(local), "{local} must be local");
        }
        // Remote — needs a key.
        assert!(!registry.is_local("openai"));
        // Unknown — not local.
        assert!(!registry.is_local("definitely-not-a-provider"));
    }

    #[test]
    fn requires_api_key_only_for_openai() {
        let registry = ProviderRegistry::with_builtins();
        assert!(registry.get("openai").unwrap().requires_api_key());
        for local in [
            "exec",
            "mcp",
            "claude",
            "openai-oauth",
            "ollama",
            "simulated",
        ] {
            assert!(!registry.get(local).unwrap().requires_api_key());
        }
    }

    #[test]
    fn unknown_provider_type_skips_cleanly() {
        let registry = ProviderRegistry::with_builtins();
        let (cfg, provider) = resolve(
            r#"
providers:
  bogus:
    type: not-a-real-provider
    api_key: "sk-real-key"
agents:
  - name: ghost
    provider_id: bogus
    model_name: custom
"#,
            "ghost",
        );
        let result = registry
            .build_agent("not-a-real-provider", &cfg, &provider)
            .expect("unknown type must not error");
        assert!(result.is_none(), "unknown provider_type must skip (None)");
    }

    #[test]
    fn register_overrides_existing_factory() {
        struct Fake;
        impl ProviderFactory for Fake {
            fn provider_type(&self) -> &str {
                "exec"
            }
            fn requires_api_key(&self) -> bool {
                true
            }
            fn build_agent(
                &self,
                _agent_config: &AgentConfig,
                _provider: &ProviderEntry,
            ) -> Result<Option<Arc<dyn NsedAgent>>> {
                Ok(None)
            }
        }
        let mut registry = ProviderRegistry::with_builtins();
        assert!(registry.is_local("exec"));
        registry.register(Arc::new(Fake));
        // The replacement now owns the "exec" key and requires a key.
        assert!(!registry.is_local("exec"));
    }

    #[test]
    fn third_party_factory_is_dispatchable() {
        #[derive(Debug, Clone)]
        struct CustomAgent;
        #[async_trait::async_trait]
        impl NsedAgent for CustomAgent {
            async fn propose(
                &self,
                _context: &crate::agents::AgentContext,
            ) -> Result<crate::agents::Proposal> {
                unreachable!("not exercised in this test")
            }
            async fn evaluate(
                &self,
                _context: &crate::agents::AgentContext,
            ) -> Result<Vec<(String, crate::agents::Evaluation)>> {
                unreachable!("not exercised in this test")
            }
            fn name(&self) -> String {
                "custom-agent".to_string()
            }
        }
        struct CustomFactory;
        impl ProviderFactory for CustomFactory {
            fn provider_type(&self) -> &str {
                "custom"
            }
            fn build_agent(
                &self,
                _agent_config: &AgentConfig,
                _provider: &ProviderEntry,
            ) -> Result<Option<Arc<dyn NsedAgent>>> {
                Ok(Some(Arc::new(CustomAgent)))
            }
        }

        let mut registry = ProviderRegistry::empty();
        registry.register(Arc::new(CustomFactory));
        let (cfg, provider) = resolve(
            r#"
providers:
  mine:
    type: custom
    api_key: "sk-real-key"
agents:
  - name: my-agent
    provider_id: mine
    model_name: custom
"#,
            "my-agent",
        );
        let agent = registry
            .build_agent("custom", &cfg, &provider)
            .expect("custom factory must build")
            .expect("custom factory must yield an agent");
        assert_eq!(agent.name(), "custom-agent");
    }
}
