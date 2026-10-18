//! Shared scheduling types used by both orchestrators and agents: the
//! deliberation policy a client writes, its content address (`policy_id`),
//! and the SLA it sets.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;

/// SLA constraints attached to a policy.
///
/// Defines the operational boundaries for a deliberation: timeouts, token limits,
/// and per-agent response deadlines. Agents receive this to configure their
/// LLM calls and timeout behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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

/// Execution mode for a policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Passthrough,
    Moderator,
    #[default]
    Deliberation,
}

/// Content-addressable deliberation policy.
/// Defines SLA, rounds, convergence, roles/agents, capabilities, and discovery tags.
/// `policy_id = sha256(canonical JSON)` — same config produces the same hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PolicyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<RoleConfig>>,
    #[serde(default = "default_rounds", alias = "rounds")]
    pub max_rounds: u32,
    #[serde(alias = "convergence_threshold", default = "default_effort")]
    pub effort: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sla: Option<PolicySla>,
    /// Generic variables passed to every job this policy starts, forwarded
    /// verbatim to the seats. What they mean is between the deployment's
    /// configuration and its own agent code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Execution mode — controls which orchestration path handles this policy.
    #[serde(default, skip_serializing_if = "is_deliberation")]
    pub mode: PolicyMode,
}

fn is_deliberation(mode: &PolicyMode) -> bool {
    *mode == PolicyMode::Deliberation
}

impl PolicyConfig {
    /// Compute the content-addressable policy ID (SHA-256 hash of canonical JSON).
    ///
    /// Strips CLI-only fields (e.g. `context` on roles) to match the
    /// orchestrator's `PolicyConfig` serialization.
    pub fn policy_id(&self) -> String {
        use sha2::{Digest, Sha256};

        /// Minimal role struct matching orchestrator's `PolicyRole` serialization.
        #[derive(serde::Serialize)]
        struct HashableRole {
            role: String,
            count: u8,
            capabilities: Vec<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pinned_agents: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            moderator: bool,
        }

        // Mirror the orchestrator's PolicyConfig serialization so both sides
        // canonicalize on the same JSON and compute identical content hashes.
        // The field name here MUST match the server-side field (`max_rounds`)
        // — renaming it to `rounds` would produce different policy_ids for
        // the same logical policy, breaking hash-based lookup.
        #[derive(serde::Serialize)]
        struct HashablePolicy {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            agents: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            roles: Option<Vec<HashableRole>>,
            max_rounds: u32,
            effort: f32,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            sla: Option<PolicySla>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            capabilities: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            tags: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "is_deliberation")]
            mode: PolicyMode,
        }

        let hashable = HashablePolicy {
            agents: self.agents.clone(),
            roles: self.roles.as_ref().map(|roles| {
                roles
                    .iter()
                    .map(|r| HashableRole {
                        role: r.role.clone(),
                        count: r.count,
                        capabilities: r.capabilities.clone(),
                        pinned_agents: r.pinned_agents.clone(),
                        moderator: r.moderator,
                    })
                    .collect()
            }),
            max_rounds: self.max_rounds,
            effort: self.effort,
            sla: self.sla.clone(),
            capabilities: self.capabilities.clone(),
            tags: self.tags.clone(),
            mode: self.mode,
        };

        let canonical = serde_json::to_string(&hashable).unwrap_or_default();
        let hash = Sha256::digest(canonical.as_bytes());
        format!("{hash:x}")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RoleConfig {
    pub role: String,
    #[serde(default = "default_role_count")]
    pub count: u8,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub context: Option<Vec<ContextRef>>,
    /// Pre-assigned agent IDs for this role. Must be ≤ `count`.
    /// Remaining slots are filled at runtime by capability matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_agents: Option<Vec<String>>,
    /// If true, this role receives moderator-routed traffic.
    /// At most one role per policy may set this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub moderator: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ContextRef {
    pub name: String,
    pub path: String,
}

fn default_rounds() -> u32 {
    3
}

fn default_effort() -> f32 {
    0.6
}

fn default_role_count() -> u8 {
    1
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

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn reviewed(context: Option<Vec<ContextRef>>) -> PolicyConfig {
        serde_json::from_value::<PolicyConfig>(serde_json::json!({
            "roles": [{"role": "reviewer", "capabilities": ["lang:rust"]}],
            "max_rounds": 2
        }))
        .map(|mut p| {
            if let Some(roles) = p.roles.as_mut() {
                roles[0].context = context;
            }
            p
        })
        .unwrap()
    }

    #[test]
    fn the_address_ignores_client_only_context_and_tracks_everything_else() {
        let plain = reviewed(None);
        let with_context = reviewed(Some(vec![ContextRef {
            name: "spec".into(),
            path: "SPEC.md".into(),
        }]));
        assert_eq!(plain.policy_id(), with_context.policy_id());
        assert_eq!(plain.policy_id().len(), 64);
        let longer = PolicyConfig {
            max_rounds: 3,
            ..plain.clone()
        };
        assert_ne!(plain.policy_id(), longer.policy_id());
    }
}
