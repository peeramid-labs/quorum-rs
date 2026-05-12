//! Shared scheduling types used by both orchestrators and agents.
//!
//! These types are the canonical definitions — both `nsed-orchestrator` and `nsed-cli`
//! re-use them to avoid schema drift.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

/// SLA constraints attached to a policy.
///
/// Defines the operational boundaries for a deliberation: timeouts, token limits,
/// and per-agent response deadlines. Agents receive this to configure their
/// LLM calls and timeout behavior.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PolicySla {
    /// Total job timeout in seconds. The BudgetManager uses this as the
    /// wall-clock envelope for the entire deliberation and divides it
    /// adaptively across rounds and phases.
    ///
    /// The sentinel `0` means "no explicit budget" — callers should go
    /// through [`PolicySla::job_timeout`] rather than reading this field
    /// directly, so the `0 → None` rule lives in exactly one place.
    #[serde(alias = "phase_timeout_secs")]
    pub job_timeout_secs: u64,
    /// Maximum seconds an individual agent has to respond within a phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_sla_secs: Option<u64>,
    /// Maximum tokens per agent response (maps to LLM `max_tokens` parameter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl PolicySla {
    /// Return the whole-job wall-clock budget as an [`Option<Duration>`],
    /// mapping the sentinel `0` to `None`.
    ///
    /// This is the canonical accessor for `job_timeout_secs`. Readers that
    /// translate the SLA into a deadline — the CLI request builder, the
    /// OpenAI compat layer's JIT budget calculation, the scheduler's
    /// resolved-policy construction — must go through this helper so the
    /// "`0` means no explicit budget" rule is enforced in exactly one
    /// place and no caller accidentally forwards `0` as a real expiry.
    pub fn job_timeout(&self) -> Option<Duration> {
        if self.job_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(self.job_timeout_secs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_timeout_nonzero_returns_some_duration() {
        let sla = PolicySla {
            job_timeout_secs: 600,
            response_sla_secs: None,
            max_tokens: None,
        };
        assert_eq!(sla.job_timeout(), Some(Duration::from_secs(600)));
    }

    #[test]
    fn job_timeout_zero_maps_to_none() {
        // Regression guard: `0` is the sentinel for "no explicit budget"
        // and must never be forwarded as a real deadline.
        let sla = PolicySla {
            job_timeout_secs: 0,
            response_sla_secs: None,
            max_tokens: None,
        };
        assert_eq!(sla.job_timeout(), None);
    }

    #[test]
    fn job_timeout_u64_max_preserved() {
        // Extreme value still round-trips — the helper is not a cap.
        let sla = PolicySla {
            job_timeout_secs: u64::MAX,
            response_sla_secs: None,
            max_tokens: None,
        };
        assert_eq!(sla.job_timeout(), Some(Duration::from_secs(u64::MAX)));
    }

    #[test]
    fn phase_timeout_secs_alias_deserializes() {
        // Legacy wire payloads still use `phase_timeout_secs`; the alias
        // must resolve it into the canonical `job_timeout_secs` field.
        let json = r#"{"phase_timeout_secs": 900}"#;
        let sla: PolicySla = serde_json::from_str(json).unwrap();
        assert_eq!(sla.job_timeout_secs, 900);
        assert_eq!(sla.job_timeout(), Some(Duration::from_secs(900)));
    }

    #[test]
    fn serialization_emits_canonical_job_timeout_secs_key() {
        let sla = PolicySla {
            job_timeout_secs: 120,
            response_sla_secs: None,
            max_tokens: None,
        };
        let json = serde_json::to_string(&sla).unwrap();
        assert!(
            json.contains("\"job_timeout_secs\":120"),
            "serialization must emit the canonical key, got: {json}"
        );
        assert!(
            !json.contains("phase_timeout_secs"),
            "legacy key must not appear in canonical output: {json}"
        );
    }
}
