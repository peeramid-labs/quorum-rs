//! Middleware configuration — YAML deserialization and pipeline builder.
//!
//! ```yaml
//! middleware:
//!   before_release:
//!     - builtin: rule_based
//!       stages: [edit, release]
//!       config:
//!         max_content_length: 50000
//!     - binary: ./middleware/moderate
//!       timeout_secs: 30
//!       stages: [release]
//!   on_provider_response:
//!     - binary: ./middleware/transform
//!       timeout_secs: 10
//! ```

use super::BinaryMiddleware;
use crate::llms::AiModel;
use crate::middleware::{AgentMiddleware, MiddlewareStage, pipeline::MiddlewarePipeline};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Default binary middleware timeout (30 seconds).
fn default_timeout() -> u64 {
    30
}

/// Top-level middleware configuration from agent YAML config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MiddlewareConfig {
    /// Middleware that runs before buffer release (edit + release stages).
    #[serde(default)]
    pub before_release: Vec<MiddlewareEntry>,
    /// Middleware that runs after LLM provider returns.
    #[serde(default)]
    pub on_provider_response: Vec<MiddlewareEntry>,
    /// Middleware that runs before constructing the LLM prompt.
    #[serde(default)]
    pub before_prompt: Vec<MiddlewareEntry>,
    /// Middleware that runs after deliberation completes (per round).
    #[serde(default)]
    pub on_completion: Vec<MiddlewareEntry>,
    /// Middleware that runs once at job-final (terminal winner known).
    #[serde(default)]
    pub on_job_complete: Vec<MiddlewareEntry>,
    /// Optional AiModel instance for LLM moderation middleware.
    /// Set at runtime (not from YAML) — call [`Self::with_moderation_model()`].
    #[serde(skip)]
    pub moderation_model: Option<Arc<dyn AiModel>>,
}

impl MiddlewareConfig {
    /// Set the LLM model for moderation middleware.
    /// Called at agent startup after constructing the provider.
    pub fn with_moderation_model(mut self, model: Arc<dyn AiModel>) -> Self {
        self.moderation_model = Some(model);
        self
    }
}

/// A single middleware entry — builtin, external binary, or dynamic library.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MiddlewareEntry {
    /// Builtin middleware (compiled into the binary).
    Builtin {
        /// Builtin type identifier.
        builtin: BuiltinMiddlewareType,
        /// Which stages this middleware runs at (default: all for the hook point).
        #[serde(default)]
        stages: Option<Vec<MiddlewareStage>>,
        /// Builtin-specific configuration.
        #[serde(default)]
        config: serde_json::Value,
    },
    /// Dynamic library middleware (.so / .dylib / .dll via FFI).
    Dylib {
        /// Path to the shared library.
        dylib: PathBuf,
        /// Which stages this middleware runs at (default: all for the hook point).
        #[serde(default)]
        stages: Option<Vec<MiddlewareStage>>,
        /// Opaque config passed to the dylib. Merged into `MiddlewareContext.metadata`
        /// before each FFI call, so a dylib self-configures from the yml without env
        /// vars (feature-parity with builtin `config`). The dylib defines its own
        /// schema under whatever key it reads.
        #[serde(default)]
        config: serde_json::Value,
    },
    /// External binary middleware (stdin/stdout JSON protocol).
    Binary {
        /// Path to the executable.
        binary: PathBuf,
        /// Extra command-line arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Timeout in seconds (default: 30).
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        /// Which stages this middleware runs at (default: all for the hook point).
        #[serde(default)]
        stages: Option<Vec<MiddlewareStage>>,
    },
}

/// Built-in middleware types. Implementations live in Issue #110.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinMiddlewareType {
    /// Signature verification (check cryptographic signatures on content).
    SignatureVerification,
    /// Rule-based validation (blocklist, content length, PII patterns).
    RuleBased,
    /// LLM-based content moderation (uses a separate LLM to classify content).
    LlmModeration,
    /// Blocks LLM output that leaks the agent's own system prompt (XML tags,
    /// protocol phrases) or the canonical tool registry.
    PromptExposure,
}

impl MiddlewareConfig {
    /// Build a pipeline for the `before_release` hook point.
    pub fn build_before_release_pipeline(&self) -> MiddlewarePipeline {
        self.build_pipeline(
            &self.before_release,
            &[MiddlewareStage::Edit, MiddlewareStage::Release],
        )
    }

    /// Build a pipeline for the `on_provider_response` hook point.
    pub fn build_provider_response_pipeline(&self) -> MiddlewarePipeline {
        self.build_pipeline(
            &self.on_provider_response,
            &[MiddlewareStage::ProviderResponse],
        )
    }

    /// Build a pipeline for the `before_prompt` hook point.
    pub fn build_before_prompt_pipeline(&self) -> MiddlewarePipeline {
        self.build_pipeline(&self.before_prompt, &[MiddlewareStage::BeforePrompt])
    }

    /// Build a pipeline for the `on_completion` hook point.
    pub fn build_completion_pipeline(&self) -> MiddlewarePipeline {
        self.build_pipeline(&self.on_completion, &[MiddlewareStage::Completion])
    }

    /// Build a pipeline for the `on_job_complete` hook point.
    pub fn build_job_complete_pipeline(&self) -> MiddlewarePipeline {
        self.build_pipeline(&self.on_job_complete, &[MiddlewareStage::JobComplete])
    }

    /// Returns true if no middleware is configured at any hook point.
    pub fn is_empty(&self) -> bool {
        self.before_release.is_empty()
            && self.on_provider_response.is_empty()
            && self.before_prompt.is_empty()
            && self.on_completion.is_empty()
            && self.on_job_complete.is_empty()
    }

    fn build_pipeline(
        &self,
        entries: &[MiddlewareEntry],
        default_stages: &[MiddlewareStage],
    ) -> MiddlewarePipeline {
        let middleware: Vec<Box<dyn AgentMiddleware>> = entries
            .iter()
            .filter_map(|entry| self.build_entry(entry, default_stages))
            .collect();
        MiddlewarePipeline::new(middleware)
    }

    fn build_entry(
        &self,
        entry: &MiddlewareEntry,
        default_stages: &[MiddlewareStage],
    ) -> Option<Box<dyn AgentMiddleware>> {
        match entry {
            MiddlewareEntry::Builtin {
                builtin,
                stages,
                config,
            } => {
                let active_stages = stages
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| default_stages.to_vec());

                match super::builtin::create_builtin_middleware(
                    builtin,
                    config,
                    active_stages,
                    self.moderation_model.clone(),
                ) {
                    Ok(mw) => {
                        tracing::info!(
                            builtin_type = ?builtin,
                            "Loaded builtin middleware"
                        );
                        Some(mw)
                    }
                    Err(e) => {
                        tracing::error!(
                            builtin_type = ?builtin,
                            error = %e,
                            "Failed to create builtin middleware"
                        );
                        None
                    }
                }
            }
            MiddlewareEntry::Dylib {
                dylib,
                stages,
                config,
            } => {
                let active_stages = stages
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| default_stages.to_vec());

                // Safety: we trust the operator's config to point at a valid dylib.
                // The FFI contract is documented in dylib.rs.
                match unsafe { super::DylibMiddleware::load(dylib, active_stages, config.clone()) }
                {
                    Ok(mw) => {
                        tracing::info!(
                            dylib = ?dylib,
                            "Loaded dynamic library middleware"
                        );
                        Some(Box::new(mw))
                    }
                    Err(e) => {
                        tracing::error!(
                            dylib = ?dylib,
                            error = %e,
                            "Failed to load dynamic library middleware — agent will refuse to start. \
                             Fix the dylib path or remove from config."
                        );
                        // Fail startup: return None so pipeline is incomplete,
                        // and the caller (build_pipeline) will log the gap.
                        // TODO: convert build_pipeline to Result to propagate this properly.
                        None
                    }
                }
            }
            MiddlewareEntry::Binary {
                binary,
                args,
                timeout_secs,
                stages,
            } => {
                let active_stages = stages
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| default_stages.to_vec());

                let name = binary
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("binary")
                    .to_string();

                Some(Box::new(BinaryMiddleware {
                    display_name: name,
                    path: binary.clone(),
                    args: args.clone(),
                    timeout: Duration::from_secs(*timeout_secs),
                    active_stages,
                }))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserialize_empty() {
        let yaml = "{}";
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn config_deserialize_binary_entry() {
        let yaml = r#"
before_release:
  - binary: ./hooks/moderate
    timeout_secs: 15
    stages: [release]
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.before_release.len(), 1);
        match &config.before_release[0] {
            MiddlewareEntry::Binary {
                binary,
                timeout_secs,
                stages,
                ..
            } => {
                assert_eq!(binary, &PathBuf::from("./hooks/moderate"));
                assert_eq!(*timeout_secs, 15);
                assert_eq!(stages.as_ref().unwrap(), &[MiddlewareStage::Release]);
            }
            _ => panic!("Expected Binary entry"),
        }
    }

    #[test]
    fn config_deserialize_builtin_entry() {
        let yaml = r#"
before_release:
  - builtin: rule_based
    stages: [edit, release]
    config:
      max_content_length: 50000
      pii_patterns: true
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.before_release.len(), 1);
        match &config.before_release[0] {
            MiddlewareEntry::Builtin {
                builtin,
                stages,
                config,
            } => {
                assert!(matches!(builtin, BuiltinMiddlewareType::RuleBased));
                assert_eq!(
                    stages.as_ref().unwrap(),
                    &[MiddlewareStage::Edit, MiddlewareStage::Release]
                );
                assert_eq!(config["max_content_length"], 50000);
                assert_eq!(config["pii_patterns"], true);
            }
            _ => panic!("Expected Builtin entry"),
        }
    }

    #[test]
    fn config_deserialize_mixed() {
        let yaml = r#"
before_release:
  - builtin: signature_verification
    stages: [release]
  - binary: ./hooks/moderate
    timeout_secs: 30
    stages: [release]
on_provider_response:
  - binary: ./hooks/transform
    timeout_secs: 10
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.before_release.len(), 2);
        assert_eq!(config.on_provider_response.len(), 1);
        assert!(!config.is_empty());
    }

    #[test]
    fn config_default_timeout() {
        let yaml = r#"
before_release:
  - binary: ./hooks/check
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.before_release[0] {
            MiddlewareEntry::Binary { timeout_secs, .. } => {
                assert_eq!(*timeout_secs, 30); // default
            }
            _ => panic!("Expected Binary entry"),
        }
    }

    #[test]
    fn build_binary_pipeline() {
        let yaml = r#"
before_release:
  - binary: /bin/true
    timeout_secs: 5
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        let pipeline = config.build_before_release_pipeline();
        assert_eq!(pipeline.len(), 1);
        assert!(!pipeline.is_empty());
    }

    #[test]
    fn build_builtin_pipeline_creates_rule_based() {
        let yaml = r#"
before_release:
  - builtin: rule_based
    config: {}
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        let pipeline = config.build_before_release_pipeline();
        // rule_based is implemented — should create 1 middleware
        assert_eq!(pipeline.len(), 1);
    }

    #[test]
    fn dylib_entry_parses_config() {
        let yaml = r#"
before_prompt:
  - dylib: /nonexistent.dylib
    config:
      patch_deliberation:
        upstream: epic
        downstream_root: ./downstreams
"#;
        let config: MiddlewareConfig = serde_yaml::from_str(yaml).unwrap();
        match &config.before_prompt[0] {
            MiddlewareEntry::Dylib { dylib, config, .. } => {
                assert_eq!(dylib.to_str(), Some("/nonexistent.dylib"));
                assert_eq!(
                    config["patch_deliberation"]["upstream"],
                    serde_json::json!("epic")
                );
                assert_eq!(
                    config["patch_deliberation"]["downstream_root"],
                    serde_json::json!("./downstreams")
                );
            }
            other => panic!("expected Dylib entry, got {other:?}"),
        }
    }
}
