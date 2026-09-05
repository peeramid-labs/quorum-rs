//! Built-in [`ProviderFactory`] implementations.
//!
//! Each factory reproduces one arm of the old
//! [`crate::serve::build_worker`] dispatch verbatim, including its
//! "missing config section → skip cleanly (`Ok(None)`)" behaviour.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use super::ProviderFactory;
use crate::agents::config::{AgentConfig, ClaudeProviderConfig};
use crate::agents::exec_agent::ExecAgent;
use crate::agents::mcp_agent::{ClaudeAgent, McpAgent};
use crate::agents::{NsedAgent, ProposerEvaluatorAgent};
use crate::config::ProviderEntry;
use crate::llms::OpenAICompatibleModel;
use crate::prompts::defaults::DefaultPromptSet;
use crate::serve::instantiate_builtin_tools;

/// Resolve the base URL for an OpenAI-compatible provider, or `None` when it
/// can't be safely defaulted. An explicit `base_url` wins; an empty one
/// defaults to `api.openai.com` ONLY for the `openai` type (or an empty type) —
/// any other type with no `base_url` returns `None` so a typoed `type:` can't
/// leak the API key to OpenAI. Shared by the provider factory and `smoke-test`.
pub(crate) fn resolve_openai_base_url(provider_type: &str, base_url: &str) -> Option<String> {
    if !base_url.is_empty() {
        return Some(base_url.to_string());
    }
    if provider_type.is_empty() || provider_type == "openai" {
        Some("https://api.openai.com/v1".to_string())
    } else {
        None
    }
}

/// `exec` — external subprocess, no LLM.
pub struct ExecFactory;

impl ProviderFactory for ExecFactory {
    fn provider_type(&self) -> &str {
        "exec"
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        _provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        let exec_cfg = match agent_config.exec.clone() {
            Some(cfg) => cfg,
            None => {
                warn!(
                    agent = %agent_config.name,
                    "provider_type=exec but no `exec` section in agent config — skipping"
                );
                return Ok(None);
            }
        };
        Ok(Some(Arc::new(ExecAgent::new(
            agent_config.name.clone(),
            exec_cfg,
        ))))
    }
}

/// `mcp` — external subprocess speaking Model Context Protocol.
pub struct McpFactory;

impl ProviderFactory for McpFactory {
    fn provider_type(&self) -> &str {
        "mcp"
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        _provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        let mcp_cfg = match agent_config.mcp.clone() {
            Some(cfg) => cfg,
            None => {
                warn!(
                    agent = %agent_config.name,
                    "provider_type=mcp but no `mcp` section in agent config — skipping"
                );
                return Ok(None);
            }
        };
        Ok(Some(Arc::new(McpAgent::new(
            agent_config.name.clone(),
            mcp_cfg,
        ))))
    }
}

/// Detect the silent model-override trap: `claude.model` set to something other
/// than a non-empty `model_name`. Returns the `(claude_model, model_name)` pair to
/// warn about, or `None` when there is no ambiguity. Pure, so it's unit-testable.
fn model_field_conflict<'a>(
    claude_model: Option<&'a str>,
    model_name: &'a str,
) -> Option<(&'a str, &'a str)> {
    match claude_model {
        Some(m) if !model_name.is_empty() && model_name != m => Some((m, model_name)),
        _ => None,
    }
}

/// `claude` — Claude Code CLI subprocess.
///
/// Config resolution — also the reference for the generic
/// `provider_config` channel a third-party `codex` factory uses:
///
/// 1. the typed `claude:` section, if present (wins);
/// 2. else [`AgentConfig::provider_config`] deserialized into
///    [`ClaudeProviderConfig`];
/// 3. else defaults (the CLI is usable with zero config).
pub struct ClaudeFactory;

impl ProviderFactory for ClaudeFactory {
    fn provider_type(&self) -> &str {
        "claude"
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        _provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        let claude_cfg = match agent_config.claude.clone() {
            Some(cfg) => cfg,
            None if !agent_config.provider_config.is_empty() => agent_config
                .provider_config_as::<ClaudeProviderConfig>()
                .map_err(|e| {
                    anyhow::anyhow!(
                        "claude agent '{}': failed to parse `provider_config`: {e}",
                        agent_config.name
                    )
                })?,
            None => ClaudeProviderConfig::default(),
        };
        // Disambiguate the two model fields: `claude.model` wins, `model_name` is the
        // fallback. A silent divergence (e.g. `model_name: haiku` shadowed by
        // `claude.model: opus`) is a config trap — surface it loudly.
        if let Some((claude_model, model_name)) =
            model_field_conflict(claude_cfg.model.as_deref(), &agent_config.model_name)
        {
            tracing::warn!(
                agent = %agent_config.name,
                %claude_model,
                %model_name,
                "claude.model overrides model_name — model_name is IGNORED for this claude agent; set them equal or drop one to resolve the ambiguity"
            );
        }
        Ok(Some(Arc::new(ClaudeAgent::new(
            agent_config.clone(),
            claude_cfg,
            Arc::new(DefaultPromptSet::new()),
        ))))
    }
}

/// OpenAI-wire-compatible HTTP provider (`openai`, `ollama`,
/// `simulated`). One instance per `provider_type`; `requires_api_key`
/// is set at construction.
pub struct OpenAiCompatibleFactory {
    provider_type: String,
    requires_api_key: bool,
    /// Throttles keyed by endpoint, so every agent pointed at one provider
    /// shares its budget. Provider limits are per account, not per agent.
    throttles: std::sync::Mutex<HashMap<String, Throttle>>,
}

#[derive(Clone, Default)]
struct Throttle {
    semaphore: Option<Arc<tokio::sync::Semaphore>>,
    rate_limiter: Option<Arc<crate::llms::RateLimiter>>,
}

impl OpenAiCompatibleFactory {
    pub fn new(provider_type: impl Into<String>, requires_api_key: bool) -> Self {
        Self {
            provider_type: provider_type.into(),
            requires_api_key,
            throttles: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The throttle for `base_url`, created on first use. A poisoned lock
    /// falls back to an unthrottled build: losing a rate limit is worse than
    /// nothing, but not worse than refusing to start the fleet.
    fn throttle_for(&self, base_url: &str, provider: &ProviderEntry) -> Throttle {
        let Ok(mut map) = self.throttles.lock() else {
            warn!(
                base_url,
                "throttle registry poisoned — building unthrottled"
            );
            return Throttle::default();
        };
        // Keyed on the endpoint, not on how it was spelled: the budget belongs
        // to the host, so a trailing slash must not open a second one.
        map.entry(base_url.trim_end_matches('/').to_string())
            .or_insert_with(|| Throttle {
                semaphore: provider
                    .concurrency
                    .filter(|c| *c > 0)
                    .map(|c| Arc::new(tokio::sync::Semaphore::new(c))),
                rate_limiter: provider
                    .qps
                    .filter(|q| *q > 0.0)
                    .map(|q| Arc::new(crate::llms::RateLimiter::new(q))),
            })
            .clone()
    }
}

impl ProviderFactory for OpenAiCompatibleFactory {
    fn provider_type(&self) -> &str {
        &self.provider_type
    }

    fn requires_api_key(&self) -> bool {
        self.requires_api_key
    }

    fn build_agent(
        &self,
        agent_config: &AgentConfig,
        provider: &ProviderEntry,
    ) -> Result<Option<Arc<dyn NsedAgent>>> {
        let base_url = match resolve_openai_base_url(self.provider_type(), &provider.base_url) {
            Some(url) => url,
            None => {
                warn!(
                    agent = %agent_config.name,
                    provider_type = %self.provider_type(),
                    "no `base_url` set for non-openai provider type — skipping"
                );
                return Ok(None);
            }
        };

        let throttle = self.throttle_for(&base_url, provider);
        let mut llm =
            OpenAICompatibleModel::new(base_url, provider.api_key.clone(), provider.engine.clone());
        if let Some(sem) = throttle.semaphore {
            llm = llm.with_semaphore(sem);
        }
        if let Some(limiter) = throttle.rate_limiter {
            llm = llm.with_rate_limiter(limiter);
        }

        let builtin_tools = match instantiate_builtin_tools(agent_config) {
            Ok(tools) => tools,
            Err(reason) => {
                warn!(
                    agent = %agent_config.name,
                    reason = %reason,
                    "skipping agent: failed to instantiate builtin_tools"
                );
                return Ok(None);
            }
        };
        if !builtin_tools.is_empty() {
            info!(
                agent = %agent_config.name,
                count = builtin_tools.len(),
                "attached SDK-builtin tool grants"
            );
        }

        // Checked here as well as at config load: an agent whose config came
        // from the registry rather than a file never passes through the
        // loader, and this pair is what the provider rejects.
        agent_config
            .validate_search_tools()
            .map_err(|e| anyhow::anyhow!(e))?;

        let agent = ProposerEvaluatorAgent::new(
            agent_config.clone(),
            Box::new(llm),
            Box::new(DefaultPromptSet::new()),
            vec![],
            builtin_tools,
        );
        // Refuse to start rather than serve an agent whose tools collide: the
        // provider would reject every request it made, and which of the two
        // tools was meant is not ours to guess.
        agent.validate_tool_names()?;
        Ok(Some(Arc::new(agent)))
    }
}

#[cfg(test)]
mod tests {
    /// The rate limit belongs to the endpoint, so two spellings of the same
    /// endpoint must not each get their own budget — a stray trailing slash in
    /// one config entry would otherwise double the traffic we send that host.
    #[test]
    fn a_trailing_slash_does_not_buy_a_second_rate_budget() {
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let bare = ProviderEntry {
            provider_type: "openai".into(),
            base_url: "https://example.test/v1".into(),
            api_key: "k".into(),
            concurrency: Some(3),
            ..Default::default()
        };
        let slashed = ProviderEntry {
            base_url: "https://example.test/v1/".into(),
            ..bare.clone()
        };
        let a = factory.throttle_for(&bare.base_url, &bare);
        let b = factory.throttle_for(&slashed.base_url, &slashed);
        let (Some(sa), Some(sb)) = (a.semaphore, b.semaphore) else {
            panic!("a configured concurrency must produce a semaphore");
        };
        assert!(
            Arc::ptr_eq(&sa, &sb),
            "the same endpoint spelled two ways got two separate budgets"
        );
    }

    #[test]
    fn agents_on_one_endpoint_share_a_throttle() {
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let provider = ProviderEntry {
            provider_type: "openai".into(),
            base_url: "https://example.test/v1".into(),
            api_key: "k".into(),
            concurrency: Some(3),
            qps: Some(5.0),
            ..Default::default()
        };
        let a = factory.throttle_for(&provider.base_url, &provider);
        let b = factory.throttle_for(&provider.base_url, &provider);
        let (Some(sa), Some(sb)) = (a.semaphore, b.semaphore) else {
            panic!("a configured concurrency must produce a semaphore");
        };
        assert!(
            Arc::ptr_eq(&sa, &sb),
            "one endpoint, one budget — the limit is per account, not per agent"
        );
        assert_eq!(sa.available_permits(), 3);
    }

    #[test]
    fn separate_endpoints_do_not_share_a_budget() {
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let mut one = ProviderEntry {
            provider_type: "openai".into(),
            base_url: "https://one.test/v1".into(),
            concurrency: Some(2),
            ..Default::default()
        };
        let first = factory.throttle_for(&one.base_url, &one).semaphore.unwrap();
        one.base_url = "https://two.test/v1".into();
        let second = factory.throttle_for(&one.base_url, &one).semaphore.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn an_unconfigured_endpoint_stays_unthrottled() {
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let provider = ProviderEntry {
            provider_type: "openai".into(),
            base_url: "https://none.test/v1".into(),
            ..Default::default()
        };
        let t = factory.throttle_for(&provider.base_url, &provider);
        assert!(t.semaphore.is_none());
        assert!(t.rate_limiter.is_none());
    }

    #[test]
    fn a_zero_limit_is_treated_as_unset_not_as_a_total_block() {
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let provider = ProviderEntry {
            provider_type: "openai".into(),
            base_url: "https://zero.test/v1".into(),
            concurrency: Some(0),
            qps: Some(0.0),
            ..Default::default()
        };
        let t = factory.throttle_for(&provider.base_url, &provider);
        assert!(
            t.semaphore.is_none(),
            "a 0-permit semaphore would deadlock every request"
        );
        assert!(t.rate_limiter.is_none());
    }

    use super::*;

    #[test]
    fn model_field_conflict_flags_only_a_silent_override() {
        // Both set + differ → the trap (haiku shadowed by opus).
        assert_eq!(
            model_field_conflict(Some("opus"), "haiku"),
            Some(("opus", "haiku"))
        );
        // Equal → no ambiguity.
        assert_eq!(model_field_conflict(Some("haiku"), "haiku"), None);
        // claude.model unset → model_name is authoritative, no conflict.
        assert_eq!(model_field_conflict(None, "haiku"), None);
        // model_name empty → nothing to shadow.
        assert_eq!(model_field_conflict(Some("opus"), ""), None);
    }

    #[test]
    fn resolve_openai_base_url_rules() {
        // Explicit base_url always wins.
        assert_eq!(
            resolve_openai_base_url("anything", "https://x/v1").as_deref(),
            Some("https://x/v1")
        );
        // Empty + openai (or empty type) → the OpenAI default.
        assert_eq!(
            resolve_openai_base_url("openai", "").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            resolve_openai_base_url("", "").as_deref(),
            Some("https://api.openai.com/v1")
        );
        // Empty + any other type → None (don't leak the key to OpenAI).
        assert!(resolve_openai_base_url("groq", "").is_none());
    }

    fn resolve(yaml: &str, agent: &str) -> (AgentConfig, ProviderEntry) {
        let fleet: crate::config::AgentFleetConfig =
            serde_yaml::from_str(yaml).expect("fleet yaml must parse");
        crate::config::load_agent_from_config(&fleet, agent).expect("agent must resolve")
    }

    #[test]
    fn exec_without_section_skips() {
        let (cfg, provider) = resolve(
            r#"
providers:
  exec_local:
    type: exec
agents:
  - name: broken
    provider_id: exec_local
    model_name: custom
"#,
            "broken",
        );
        let built = ExecFactory.build_agent(&cfg, &provider).unwrap();
        assert!(built.is_none(), "exec without `exec:` section must skip");
    }

    #[test]
    fn exec_with_section_builds() {
        let (cfg, provider) = resolve(
            r#"
providers:
  exec_local:
    type: exec
agents:
  - name: runner
    provider_id: exec_local
    model_name: custom
    exec:
      command: ["echo", "hi"]
"#,
            "runner",
        );
        let agent = ExecFactory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("exec with section must build");
        assert_eq!(agent.name(), "runner");
    }

    #[test]
    fn mcp_without_section_skips() {
        let (cfg, provider) = resolve(
            r#"
providers:
  mcp_local:
    type: mcp
agents:
  - name: broken
    provider_id: mcp_local
    model_name: custom
"#,
            "broken",
        );
        assert!(McpFactory.build_agent(&cfg, &provider).unwrap().is_none());
    }

    #[test]
    fn mcp_with_section_builds() {
        let (cfg, provider) = resolve(
            r#"
providers:
  mcp_local:
    type: mcp
agents:
  - name: mcp-runner
    provider_id: mcp_local
    model_name: custom
    mcp:
      command: ["my-mcp-server"]
"#,
            "mcp-runner",
        );
        let agent = McpFactory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("mcp with section must build");
        assert_eq!(agent.name(), "mcp-runner");
    }

    #[test]
    fn claude_builds_with_or_without_section() {
        let (cfg, provider) = resolve(
            r#"
providers:
  claude_cli:
    type: claude
agents:
  - name: claude-agent
    provider_id: claude_cli
    model_name: claude-sonnet
"#,
            "claude-agent",
        );
        let agent = ClaudeFactory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("claude must build from defaults");
        assert_eq!(agent.name(), "claude-agent");
    }

    /// The built-in `claude` provider sources its typed config from the
    /// generic `provider_config` map (no `claude:` section) — the same
    /// pattern a third-party `codex` factory uses.
    #[test]
    fn claude_builds_from_provider_config() {
        let (cfg, provider) = resolve(
            r#"
providers:
  claude_cli:
    type: claude
agents:
  - name: claude-agent
    provider_id: claude_cli
    model_name: claude-sonnet
    provider_config:
      permission_mode: "acceptEdits"
      timeout_secs: 120
"#,
            "claude-agent",
        );
        assert!(
            cfg.claude.is_none(),
            "no typed claude: section in this fixture"
        );
        let agent = ClaudeFactory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("claude must build from provider_config");
        assert_eq!(agent.name(), "claude-agent");
    }

    /// Precedence: a present typed `claude:` section wins, and the
    /// `provider_config` map is not even parsed (here it would fail —
    /// `timeout_secs` is a string — proving it was ignored).
    #[test]
    fn claude_section_wins_over_provider_config() {
        let (cfg, provider) = resolve(
            r#"
providers:
  claude_cli:
    type: claude
agents:
  - name: claude-agent
    provider_id: claude_cli
    model_name: claude-sonnet
    claude:
      permission_mode: "acceptEdits"
    provider_config:
      timeout_secs: "not-a-number"
"#,
            "claude-agent",
        );
        let agent = ClaudeFactory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("typed claude: section must win, provider_config ignored");
        assert_eq!(agent.name(), "claude-agent");
    }

    /// A malformed `provider_config` (when it IS the config source) is a
    /// hard error, not a silent skip — the factory surfaces the parse
    /// failure so the operator fixes the YAML.
    #[test]
    fn claude_bad_provider_config_errors() {
        let (cfg, provider) = resolve(
            r#"
providers:
  claude_cli:
    type: claude
agents:
  - name: claude-agent
    provider_id: claude_cli
    model_name: claude-sonnet
    provider_config:
      timeout_secs: "not-a-number"
"#,
            "claude-agent",
        );
        let err = ClaudeFactory.build_agent(&cfg, &provider).unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to parse `provider_config`"),
            "got: {err}"
        );
    }

    #[test]
    fn openai_empty_base_url_defaults_to_openai_com() {
        let (cfg, provider) = resolve(
            r#"
providers:
  oai:
    type: openai
    api_key: "sk-real-key"
agents:
  - name: gpt
    provider_id: oai
    model_name: gpt-4o
"#,
            "gpt",
        );
        let factory = OpenAiCompatibleFactory::new("openai", true);
        let agent = factory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("openai with empty base_url must default and build");
        assert_eq!(agent.name(), "gpt");
    }

    #[test]
    fn non_openai_empty_base_url_skips() {
        let (cfg, provider) = resolve(
            r#"
providers:
  local_ollama:
    type: ollama
agents:
  - name: llama
    provider_id: local_ollama
    model_name: llama3
"#,
            "llama",
        );
        let factory = OpenAiCompatibleFactory::new("ollama", false);
        assert!(
            factory.build_agent(&cfg, &provider).unwrap().is_none(),
            "ollama without base_url must skip (no openai.com fallback)"
        );
    }

    #[test]
    fn ollama_with_base_url_builds() {
        let (cfg, provider) = resolve(
            r#"
providers:
  local_ollama:
    type: ollama
    base_url: "http://localhost:11434/v1"
agents:
  - name: llama
    provider_id: local_ollama
    model_name: llama3
"#,
            "llama",
        );
        let factory = OpenAiCompatibleFactory::new("ollama", false);
        let agent = factory
            .build_agent(&cfg, &provider)
            .unwrap()
            .expect("ollama with base_url must build");
        assert_eq!(agent.name(), "llama");
    }

    /// Launch-path guard, which is the one that matters in production: an
    /// agent whose config comes from the registry never passes through the
    /// loader, so the check above would never run for it.
    #[test]
    fn a_registry_config_setting_both_search_forms_never_starts() {
        let cfg = AgentConfig {
            name: "corepunk".to_string(),
            model_name: "llama3".to_string(),
            delegated_search: Some("search_web".to_string()),
            provider_executed_tools: vec!["search_web".to_string()],
            ..Default::default()
        };
        let provider = ProviderEntry {
            provider_type: "ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            ..Default::default()
        };
        let said = OpenAiCompatibleFactory::new("ollama", false)
            .build_agent(&cfg, &provider)
            .expect_err("a colliding seat must refuse to start")
            .to_string();
        assert!(said.contains("corepunk"), "{said}");
        assert!(said.contains("provider_executed_tools"), "{said}");
    }

    /// The bypass itself still starts. If this breaks, the backends that
    /// reject a mixed tool array lose their only route to search, and the
    /// failure reads as configured-and-working.
    #[test]
    fn the_delegated_search_bypass_alone_still_starts() {
        let cfg = AgentConfig {
            name: "corepunk".to_string(),
            model_name: "llama3".to_string(),
            delegated_search: Some("search_web".to_string()),
            ..Default::default()
        };
        let provider = ProviderEntry {
            provider_type: "ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            ..Default::default()
        };
        OpenAiCompatibleFactory::new("ollama", false)
            .build_agent(&cfg, &provider)
            .expect("the delegated form alone is the supported bypass")
            .expect("ollama with base_url must build");
    }
}
