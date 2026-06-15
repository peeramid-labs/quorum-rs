//! Built-in [`ProviderFactory`] implementations (#17).
//!
//! Each factory reproduces one arm of the old
//! [`crate::serve::build_worker`] dispatch verbatim, including its
//! "missing config section → skip cleanly (`Ok(None)`)" behaviour.

use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use super::ProviderFactory;
use crate::agents::config::AgentConfig;
use crate::agents::exec_agent::ExecAgent;
use crate::agents::mcp_agent::{ClaudeAgent, McpAgent};
use crate::agents::{NsedAgent, ProposerEvaluatorAgent};
use crate::config::ProviderEntry;
use crate::llms::OpenAICompatibleModel;
use crate::prompts::defaults::DefaultPromptSet;
use crate::serve::instantiate_builtin_tools;

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

/// `claude` — Claude Code CLI subprocess. Missing `claude:` section
/// falls back to defaults (the CLI is usable with zero config).
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
        let claude_cfg = agent_config.claude.clone().unwrap_or_default();
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
}

impl OpenAiCompatibleFactory {
    pub fn new(provider_type: impl Into<String>, requires_api_key: bool) -> Self {
        Self {
            provider_type: provider_type.into(),
            requires_api_key,
        }
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
        // Base-URL resolution mirrors the old dispatch: only `openai`
        // gets the api.openai.com default. Any other type with an
        // empty `base_url` is skipped so a typoed `type:` can't leak
        // the API key to api.openai.com.
        let base_url = if provider.base_url.is_empty() {
            if self.provider_type == "openai" {
                "https://api.openai.com/v1".to_string()
            } else {
                warn!(
                    agent = %agent_config.name,
                    provider_type = %self.provider_type,
                    "no `base_url` set for non-openai provider type — skipping"
                );
                return Ok(None);
            }
        } else {
            provider.base_url.clone()
        };

        let llm =
            OpenAICompatibleModel::new(base_url, provider.api_key.clone(), provider.engine.clone());

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

        Ok(Some(Arc::new(ProposerEvaluatorAgent::new(
            agent_config.clone(),
            Box::new(llm),
            Box::new(DefaultPromptSet::new()),
            vec![],
            builtin_tools,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
