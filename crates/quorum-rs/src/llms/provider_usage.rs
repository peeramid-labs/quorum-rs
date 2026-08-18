//! Usage figures a provider reports outside the OpenAI chat-completions schema.
//!
//! The OpenAI `usage` object models token counts and nothing else, so
//! [`async_openai::types::CompletionUsage`] silently drops any extension a
//! provider adds. Some gateways put the *exact* charge for the call in there —
//! the money the account was actually debited, not a figure derived from a
//! price list — and prompt-cache accounting alongside it.
//!
//! Everything here is optional on purpose. Self-hosted and direct-to-vendor
//! backends report none of it, and a missing field means "unknown", never
//! "zero": billing that read a missing cost as free would give the work away.

use serde_json::Value;

/// Non-standard `usage` fields read straight from the response JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProviderUsage {
    /// What the provider charged for this call, in USD.
    pub cost_usd: Option<f64>,
    /// Prompt tokens served from the provider's cache.
    pub cached_tokens: Option<u32>,
    /// Prompt tokens written into the provider's cache.
    pub cache_write_tokens: Option<u32>,
}

impl ProviderUsage {
    /// Read the extensions from a `usage` object.
    ///
    /// A cost that is negative or non-finite is dropped rather than passed on:
    /// it cannot be charged for, and letting it through would poison every sum
    /// it lands in.
    pub fn from_usage_value(usage: &Value) -> Self {
        Self {
            cost_usd: usage
                .get("cost")
                .and_then(Value::as_f64)
                .filter(|c| c.is_finite() && *c >= 0.0),
            cached_tokens: token_count(usage, "cached_tokens"),
            cache_write_tokens: token_count(usage, "cache_write_tokens"),
        }
    }

    /// Read the extensions from a whole chat-completions response body.
    ///
    /// A body that is not JSON, or that carries no `usage`, yields all-`None`.
    pub fn from_response_body(body: &str) -> Self {
        serde_json::from_str::<Value>(body)
            .ok()
            .as_ref()
            .and_then(|v| v.get("usage"))
            .map(Self::from_usage_value)
            .unwrap_or_default()
    }

    /// Whether the provider reported anything at all.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold one more call's figures into a running total.
    ///
    /// A field stays `None` until some call reports it, and thereafter sums
    /// only the calls that did. An agent talks to one backend for the whole of
    /// a turn, so in practice this is all-or-nothing; where it is not, a total
    /// over the calls that reported still beats throwing the evidence away.
    pub fn accumulate(&mut self, other: &Self) {
        add(&mut self.cost_usd, other.cost_usd, |a, b| a + b);
        add(
            &mut self.cached_tokens,
            other.cached_tokens,
            u32::saturating_add,
        );
        add(
            &mut self.cache_write_tokens,
            other.cache_write_tokens,
            u32::saturating_add,
        );
    }
}

fn add<T: Copy>(acc: &mut Option<T>, next: Option<T>, sum: impl Fn(T, T) -> T) {
    if let Some(n) = next {
        *acc = Some(match *acc {
            Some(a) => sum(a, n),
            None => n,
        });
    }
}

/// A cache counter from `prompt_tokens_details`, or from the `usage` root when
/// a provider hoists it there.
fn token_count(usage: &Value, field: &str) -> Option<u32> {
    usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get(field))
        .or_else(|| usage.get(field))
        .and_then(Value::as_u64)
        .map(|n| n.min(u32::MAX as u64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape a cost-reporting gateway actually returns.
    fn gateway_body() -> &'static str {
        r#"{
            "id": "gen-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 6, "completion_tokens": 2, "total_tokens": 8,
                "cost": 7.434e-07,
                "is_byok": false,
                "cost_details": {
                    "upstream_inference_cost": 7.434e-07,
                    "upstream_inference_prompt_cost": 4.074e-07,
                    "upstream_inference_completions_cost": 3.36e-07
                },
                "prompt_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 0}
            }
        }"#
    }

    #[test]
    fn reads_the_exact_cost_off_a_gateway_response() {
        let usage = ProviderUsage::from_response_body(gateway_body());
        assert_eq!(usage.cost_usd, Some(7.434e-07));
        assert_eq!(usage.cached_tokens, Some(0));
        assert_eq!(usage.cache_write_tokens, Some(0));
        assert!(!usage.is_empty());
    }

    #[test]
    fn a_backend_that_reports_no_cost_yields_nothing_rather_than_zero() {
        let body = r#"{"usage": {"prompt_tokens": 10, "completion_tokens": 5,
                                 "total_tokens": 15}}"#;
        let usage = ProviderUsage::from_response_body(body);
        assert_eq!(usage.cost_usd, None);
        assert_eq!(usage.cached_tokens, None);
        assert!(usage.is_empty());
    }

    #[test]
    fn a_response_without_usage_yields_nothing() {
        let body = r#"{"id": "x", "choices": []}"#;
        assert!(ProviderUsage::from_response_body(body).is_empty());
    }

    #[test]
    fn a_body_that_is_not_json_yields_nothing() {
        assert!(ProviderUsage::from_response_body("<html>502</html>").is_empty());
    }

    #[test]
    fn a_negative_cost_is_dropped() {
        let usage = ProviderUsage::from_usage_value(&json!({"cost": -0.5}));
        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn a_non_finite_cost_is_dropped() {
        // JSON cannot express NaN, but a string in the field must not be
        // coerced into one either.
        let usage = ProviderUsage::from_usage_value(&json!({"cost": "1e400"}));
        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn a_zero_cost_is_reported_as_zero() {
        // A free-tier model really did cost nothing. That is a fact about the
        // call, distinct from a backend that never said.
        let usage = ProviderUsage::from_usage_value(&json!({"cost": 0.0}));
        assert_eq!(usage.cost_usd, Some(0.0));
    }

    #[test]
    fn cache_counters_are_read_from_the_usage_root_when_hoisted_there() {
        let usage = ProviderUsage::from_usage_value(&json!({"cached_tokens": 128, "cost": 0.001}));
        assert_eq!(usage.cached_tokens, Some(128));
        assert_eq!(usage.cost_usd, Some(0.001));
    }

    #[test]
    fn nested_cache_counters_win_over_a_hoisted_one() {
        let usage = ProviderUsage::from_usage_value(&json!({
            "cached_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        assert_eq!(usage.cached_tokens, Some(64));
    }

    #[test]
    fn accumulating_sums_the_calls_of_one_react_loop() {
        let mut total = ProviderUsage::default();
        total.accumulate(&ProviderUsage {
            cost_usd: Some(0.001),
            cached_tokens: Some(10),
            cache_write_tokens: None,
        });
        total.accumulate(&ProviderUsage {
            cost_usd: Some(0.002),
            cached_tokens: Some(5),
            cache_write_tokens: Some(7),
        });
        assert_eq!(total.cost_usd, Some(0.003));
        assert_eq!(total.cached_tokens, Some(15));
        assert_eq!(total.cache_write_tokens, Some(7));
    }

    #[test]
    fn accumulating_only_silent_calls_leaves_the_total_unknown() {
        let mut total = ProviderUsage::default();
        total.accumulate(&ProviderUsage::default());
        total.accumulate(&ProviderUsage::default());
        assert!(total.is_empty());
    }

    #[test]
    fn a_silent_call_does_not_erase_a_cost_already_reported() {
        let mut total = ProviderUsage {
            cost_usd: Some(0.5),
            ..Default::default()
        };
        total.accumulate(&ProviderUsage::default());
        assert_eq!(total.cost_usd, Some(0.5));
    }

    #[test]
    fn an_oversized_cache_counter_saturates_rather_than_wrapping() {
        let usage = ProviderUsage::from_usage_value(&json!({
            "prompt_tokens_details": {"cached_tokens": 5_000_000_000u64}
        }));
        assert_eq!(usage.cached_tokens, Some(u32::MAX));
    }
}
