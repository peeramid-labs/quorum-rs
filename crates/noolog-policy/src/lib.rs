//! Deliberation policies as clients write them: the roster or roles, rounds,
//! effort, SLA and discovery tags a job runs under, and the content address
//! (`policy_id`) the orchestrator registers them by. A leaf crate, so crates
//! quorum-rs itself depends on (the files pipeline carries a job request with
//! an upload) can name the same type; quorum-rs re-exports it at its old paths.

mod sla;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use sla::PolicySla;

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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
