use std::sync::Arc;

pub mod harmony;
pub mod native;
pub mod xml_regex;

// Re-export trait and types from SDK
pub use crate::llms::{ChatStrategy, RequestOverrides};

pub struct StrategyResolver;

impl StrategyResolver {
    pub fn resolve(engine: Option<&str>) -> Arc<dyn ChatStrategy> {
        match engine {
            Some("vllm") => Arc::new(xml_regex::XmlRegexStrategy::new(Some("vllm".to_string()))),
            Some("vllm_xml_responses") => Arc::new(xml_regex::XmlRegexStrategy::new(Some(
                "vllm_responses".to_string(),
            ))),
            Some("gpt-oss") | Some("harmony") => Arc::new(harmony::HarmonyStrategy),
            // Pass the engine string to NativeStrategy so it can handle minor quirks (e.g. Cloudflare vs Together)
            Some(other) => Arc::new(native::NativeStrategy::new(other)),
            None => Arc::new(native::NativeStrategy::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_resolution() {
        // vLLM -> XmlRegex
        let strategy = StrategyResolver::resolve(Some("vllm"));
        assert!(!strategy.supports_streaming()); // XmlRegex returns false

        // GPT-OSS -> Harmony
        let strategy = StrategyResolver::resolve(Some("gpt-oss"));
        assert!(!strategy.supports_streaming()); // Harmony returns false
        assert_eq!(strategy.endpoint_suffix(), "/completions");

        // Harmony -> Harmony
        let strategy = StrategyResolver::resolve(Some("harmony"));
        assert_eq!(strategy.endpoint_suffix(), "/completions");

        // Native -> Native
        let strategy = StrategyResolver::resolve(Some("native"));
        assert!(strategy.supports_streaming()); // Native returns true
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");

        // None -> Native
        let strategy = StrategyResolver::resolve(None);
        assert!(strategy.supports_streaming());
    }

    #[test]
    fn test_strategy_resolution_unknown_engine_falls_back_to_native() {
        // Some("cloudflare") or any other unknown engine string -> NativeStrategy
        let strategy = StrategyResolver::resolve(Some("cloudflare"));
        assert!(strategy.supports_streaming());
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");

        let strategy = StrategyResolver::resolve(Some("together"));
        assert!(strategy.supports_streaming());
        assert_eq!(strategy.endpoint_suffix(), "/chat/completions");

        // vllm_xml_responses -> XmlRegexStrategy (not HarmonyStrategy or the Some(other) branch)
        let strategy = StrategyResolver::resolve(Some("vllm_xml_responses"));
        assert!(!strategy.supports_streaming());
        // XmlRegexStrategy uses /responses; HarmonyStrategy uses /completions — this
        // distinguishes the two non-streaming strategies and catches regressions.
        assert_eq!(
            strategy.endpoint_suffix(),
            "/responses",
            "vllm_xml_responses should resolve to XmlRegexStrategy (/responses), not HarmonyStrategy (/completions)"
        );
    }
}
