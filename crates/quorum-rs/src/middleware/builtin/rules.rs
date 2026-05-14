//! Rule-based middleware — fast regex matching for blocklist, PII, content length.
//!
//! Runs on both edit and release stages for early feedback. Pure computation,
//! no I/O, no LLM calls. Microsecond execution time.

use crate::middleware::{AgentMiddleware, MiddlewareContext, MiddlewareStage, MiddlewareVerdict};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Default PII patterns
// ---------------------------------------------------------------------------

struct PiiPattern {
    regex: Regex,
    category: &'static str,
}

static DEFAULT_PII_PATTERNS: LazyLock<Vec<PiiPattern>> = LazyLock::new(|| {
    vec![
        PiiPattern {
            regex: Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap(),
            category: "email",
        },
        PiiPattern {
            regex: Regex::new(r"\b\d{3}[-.]?\d{3}[-.]?\d{4}\b").unwrap(),
            category: "phone",
        },
        PiiPattern {
            regex: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
            category: "ssn",
        },
        PiiPattern {
            regex: Regex::new(r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13})\b")
                .unwrap(),
            category: "credit_card",
        },
    ]
});

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Rule-based middleware configuration from YAML.
#[derive(Debug, Deserialize)]
struct RuleBasedConfig {
    /// Regex patterns to block. Each pattern is compiled and matched against content.
    #[serde(default)]
    blocklist: Vec<String>,
    /// Path to a file containing blocklist patterns (one per line).
    #[serde(default)]
    blocklist_file: Option<String>,
    /// Maximum content length in characters. 0 = no limit.
    #[serde(default = "default_max_length")]
    max_content_length: usize,
    /// Enable default PII pattern detection (email, phone, SSN, credit card).
    #[serde(default = "default_true")]
    pii_patterns: bool,
    /// Detect binary/control characters in content.
    #[serde(default = "default_true")]
    binary_detection: bool,
}

fn default_max_length() -> usize {
    50000
}

fn default_true() -> bool {
    true
}

impl Default for RuleBasedConfig {
    fn default() -> Self {
        Self {
            blocklist: Vec::new(),
            blocklist_file: None,
            max_content_length: default_max_length(),
            pii_patterns: true,
            binary_detection: true,
        }
    }
}

// ---------------------------------------------------------------------------
// RuleBasedMiddleware
// ---------------------------------------------------------------------------

/// Fast regex-based content validation middleware.
///
/// Checks: blocklist patterns, content length, PII patterns, binary data.
/// PII matches produce `Warn` (annotate but allow). All others produce `Block`.
#[derive(Debug)]
pub struct RuleBasedMiddleware {
    name: String,
    blocklist: Vec<Regex>,
    max_content_length: usize,
    pii_enabled: bool,
    binary_detection: bool,
    stages: Vec<MiddlewareStage>,
}

impl RuleBasedMiddleware {
    /// Create from YAML config JSON and active stages.
    pub fn from_config(
        config: &serde_json::Value,
        stages: Vec<MiddlewareStage>,
    ) -> Result<Self, String> {
        let cfg: RuleBasedConfig = if config.is_null() {
            RuleBasedConfig::default()
        } else {
            serde_json::from_value(config.clone())
                .map_err(|e| format!("Invalid rule_based config: {e}"))?
        };

        let mut blocklist = Vec::new();

        // Compile inline patterns
        for pattern in &cfg.blocklist {
            let re = Regex::new(pattern)
                .map_err(|e| format!("Invalid blocklist regex '{}': {e}", pattern))?;
            blocklist.push(re);
        }

        // Load patterns from file (if specified)
        if let Some(ref path) = cfg.blocklist_file {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue; // Skip comments and blank lines
                        }
                        let re = Regex::new(line).map_err(|e| {
                            format!("Invalid blocklist regex '{}' from {}: {e}", line, path)
                        })?;
                        blocklist.push(re);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = path,
                        error = %e,
                        "Blocklist file not found — proceeding without file patterns"
                    );
                }
            }
        }

        Ok(Self {
            name: "rule_based".to_string(),
            blocklist,
            max_content_length: cfg.max_content_length,
            pii_enabled: cfg.pii_patterns,
            binary_detection: cfg.binary_detection,
            stages,
        })
    }

    /// Create a pass-through middleware (used for unimplemented builtins).
    pub fn passthrough(name: &str, stages: Vec<MiddlewareStage>) -> Self {
        Self {
            name: name.to_string(),
            blocklist: Vec::new(),
            max_content_length: 0,
            pii_enabled: false,
            binary_detection: false,
            stages,
        }
    }

    /// Extract text content from the middleware context.
    fn extract_text(ctx: &MiddlewareContext) -> String {
        if let Some(s) = ctx.content.as_str() {
            s.to_string()
        } else {
            // For structured content (JSON objects), serialize to string for scanning
            ctx.content.to_string()
        }
    }
}

#[async_trait]
impl AgentMiddleware for RuleBasedMiddleware {
    async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
        let text = Self::extract_text(ctx);

        // 1. Content length check
        if self.max_content_length > 0 && text.chars().count() > self.max_content_length {
            return MiddlewareVerdict::block(
                "format",
                format!(
                    "Content exceeds {} character limit ({} chars)",
                    self.max_content_length,
                    text.chars().count()
                ),
            );
        }

        // 2. Binary/control character detection
        if self.binary_detection {
            let control_count = text
                .chars()
                .filter(|c| c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
                .count();
            if control_count > 0 {
                return MiddlewareVerdict::block(
                    "format",
                    format!(
                        "Content contains {} control/binary characters",
                        control_count
                    ),
                );
            }
        }

        // 3. Blocklist check
        for re in &self.blocklist {
            if re.is_match(&text) {
                return MiddlewareVerdict::block(
                    "blocklist",
                    format!("Content matches blocklist pattern: {}", re.as_str()),
                );
            }
        }

        // 4. PII detection (Warn, not Block — annotate but allow)
        if self.pii_enabled {
            for pattern in DEFAULT_PII_PATTERNS.iter() {
                if pattern.regex.is_match(&text) {
                    return MiddlewareVerdict::warn(
                        "pii",
                        format!(
                            "Content may contain {} — review before release",
                            pattern.category
                        ),
                    );
                }
            }
        }

        MiddlewareVerdict::pass()
    }

    fn stages(&self) -> Vec<MiddlewareStage> {
        self.stages.clone()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(content: &str) -> MiddlewareContext {
        MiddlewareContext {
            content: serde_json::json!(content),
            action: "propose".to_string(),
            agent_id: "test-agent".to_string(),
            job_id: "test-job".to_string(),
            round: 1,
            stage: MiddlewareStage::Release,
            metadata: serde_json::json!(null),
            hook_state: std::collections::HashMap::new(),
        }
    }

    fn default_mw() -> RuleBasedMiddleware {
        RuleBasedMiddleware::from_config(
            &serde_json::json!(null),
            vec![MiddlewareStage::Edit, MiddlewareStage::Release],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn clean_content_passes() {
        let mw = default_mw();
        let ctx = make_ctx("This is a clean proposal about software architecture.");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Pass);
    }

    #[tokio::test]
    async fn content_exceeding_length_blocks() {
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!({"max_content_length": 10}),
            vec![MiddlewareStage::Release],
        )
        .unwrap();
        let ctx = make_ctx("This content is definitely longer than ten characters");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("format"));
    }

    #[tokio::test]
    async fn blocklist_match_blocks() {
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!({"blocklist": ["forbidden_word"]}),
            vec![MiddlewareStage::Release],
        )
        .unwrap();
        let ctx = make_ctx("This text contains a forbidden_word in it.");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("blocklist"));
    }

    #[tokio::test]
    async fn blocklist_no_match_passes() {
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!({"blocklist": ["never_matches_this"]}),
            vec![MiddlewareStage::Release],
        )
        .unwrap();
        let ctx = make_ctx("Normal content without banned patterns.");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Pass);
    }

    #[tokio::test]
    async fn email_detection_warns() {
        let mw = default_mw();
        let ctx = make_ctx("Please contact alice@example.com for details.");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Warn);
        assert_eq!(verdict.category.as_deref(), Some("pii"));
        assert!(verdict.reason.as_deref().unwrap().contains("email"));
    }

    #[tokio::test]
    async fn phone_detection_warns() {
        let mw = default_mw();
        let ctx = make_ctx("Call us at 555-123-4567 for support.");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Warn);
        assert_eq!(verdict.category.as_deref(), Some("pii"));
    }

    #[tokio::test]
    async fn ssn_detection_warns() {
        let mw = default_mw();
        let ctx = make_ctx("SSN: 123-45-6789");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Warn);
        assert_eq!(verdict.category.as_deref(), Some("pii"));
        assert!(verdict.reason.as_deref().unwrap().contains("ssn"));
    }

    #[tokio::test]
    async fn binary_content_blocks() {
        let mw = default_mw();
        let content = "Normal text\x01\x02\x03 with binary".to_string();
        let ctx = make_ctx(&content);
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Block);
        assert_eq!(verdict.category.as_deref(), Some("format"));
    }

    #[tokio::test]
    async fn pii_disabled_skips_detection() {
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!({"pii_patterns": false}),
            vec![MiddlewareStage::Release],
        )
        .unwrap();
        let ctx = make_ctx("Contact alice@example.com");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Pass);
    }

    #[tokio::test]
    async fn passthrough_always_passes() {
        let mw = RuleBasedMiddleware::passthrough("test", vec![MiddlewareStage::Release]);
        let ctx = make_ctx("Anything goes here 🎉");
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Pass);
    }

    #[tokio::test]
    async fn json_content_scanned() {
        // Non-string content should be serialized and scanned
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!({"blocklist": ["secret_key"]}),
            vec![MiddlewareStage::Release],
        )
        .unwrap();
        let ctx = MiddlewareContext {
            content: serde_json::json!({"field": "has secret_key in it"}),
            action: "propose".to_string(),
            agent_id: "a".to_string(),
            job_id: "j".to_string(),
            round: 1,
            stage: MiddlewareStage::Release,
            metadata: serde_json::json!(null),
            hook_state: std::collections::HashMap::new(),
        };
        let verdict = mw.execute(&ctx).await;
        assert_eq!(verdict.verdict, crate::middleware::Verdict::Block);
    }

    #[test]
    fn invalid_regex_returns_error() {
        let result = RuleBasedMiddleware::from_config(
            &serde_json::json!({"blocklist": ["[invalid"]}),
            vec![MiddlewareStage::Release],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid blocklist regex"));
    }

    #[test]
    fn default_config_works() {
        let mw = RuleBasedMiddleware::from_config(
            &serde_json::json!(null),
            vec![MiddlewareStage::Edit, MiddlewareStage::Release],
        );
        assert!(mw.is_ok());
    }
}
