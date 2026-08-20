use std::sync::Arc;

pub mod harmony;
pub mod native;
pub mod xml_regex;

// Re-export trait and types from SDK
pub use crate::llms::{ChatStrategy, RequestOverrides};

use crate::agents::config::AgentConfig;

/// Round a sampling value to two decimals as an `f64`, or `None` when it
/// cannot be sent at all.
///
/// An `f32` widened for serialisation prints its binary approximation —
/// `0.7` becomes `0.699999988079071` — and backends that validate decimal
/// places reject that outright. Two places is the precision these knobs are
/// ever configured with.
///
/// A non-finite value has no JSON spelling, and the serialiser substitutes
/// `null` — a knob nobody set, failing the request at the backend. `None`
/// drops it, so the backend applies its own default.
pub(crate) fn round_sampling_value(v: f32) -> Option<f64> {
    v.is_finite().then(|| ((v as f64) * 100.0).round() / 100.0)
}

/// Attach `max_tokens` and the sampling params to a request body, omitting
/// each one that must not be sent.
///
/// Two backends' rules meet here: some fix the sampling params server-side and
/// reject any value rather than clamping it, and some reject `max_tokens: 0`
/// instead of reading it as "unlimited". Both are omissions, and `0.0` /`0`
/// are legitimate values, so neither can be encoded in the value itself.
pub fn apply_sampling_params(
    body: &mut serde_json::Value,
    agent: &AgentConfig,
    max_tokens: u32,
    presence_penalty: Option<f32>,
) {
    if max_tokens > 0 {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if agent.omit_sampling_params {
        return;
    }
    if let Some(t) = round_sampling_value(agent.temperature) {
        body["temperature"] = serde_json::json!(t);
    }
    if let Some(pp) = presence_penalty.and_then(round_sampling_value) {
        body["presence_penalty"] = serde_json::json!(pp);
    }
}

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
    use crate::agents::config::AgentConfig;

    fn agent(omit: bool) -> AgentConfig {
        AgentConfig {
            temperature: 0.7,
            omit_sampling_params: omit,
            ..Default::default()
        }
    }

    #[test]
    fn sampling_values_serialise_without_binary_noise() {
        // 0.7f32 widens to 0.699999988079071; backends that cap decimal
        // places reject that as an illegal parameter.
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &agent(false), 128, Some(1.5));
        assert_eq!(body["temperature"].to_string(), "0.7");
        assert_eq!(body["presence_penalty"].to_string(), "1.5");
        assert_eq!(round_sampling_value(0.333_333), Some(0.33));
    }

    /// `.nan` and `.inf` are legal YAML, so a config can hold one. JSON has no
    /// way to write either, and the serialiser silently substitutes `null` —
    /// the request then fails at the backend over a parameter nobody set on
    /// purpose. Omitting the knob sends the backend's own default instead.
    #[test]
    fn a_non_finite_sampling_value_is_omitted_rather_than_sent_as_null() {
        let mut hostile = agent(false);
        hostile.temperature = f32::NAN;
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &hostile, 128, Some(f32::INFINITY));
        assert!(
            body.get("temperature").is_none(),
            "a non-finite temperature reached the body: {body}"
        );
        assert!(
            body.get("presence_penalty").is_none(),
            "a non-finite presence_penalty reached the body: {body}"
        );
    }

    #[test]
    fn sampling_params_are_sent_by_default() {
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &agent(false), 512, Some(1.5));
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
        assert!((body["presence_penalty"].as_f64().unwrap() - 1.5).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 512);
    }

    #[test]
    fn omit_flag_drops_the_keys_rather_than_zeroing_them() {
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &agent(true), 512, Some(1.5));
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("presence_penalty").is_none(), "{body}");
        // max_tokens is orthogonal — a fixed-sampling backend still wants it.
        assert_eq!(body["max_tokens"], 512);
    }

    #[test]
    fn max_tokens_zero_is_omitted_not_sent_as_zero() {
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &agent(false), 0, None);
        assert!(
            body.get("max_tokens").is_none(),
            "0 means unset here; some backends reject it outright: {body}"
        );
        assert!(
            body.get("temperature").is_some(),
            "still a sampling-param sender"
        );
    }

    #[test]
    fn absent_presence_penalty_is_not_serialised_as_null() {
        let mut body = serde_json::json!({});
        apply_sampling_params(&mut body, &agent(false), 8, None);
        assert!(body.get("presence_penalty").is_none(), "{body}");
    }

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
