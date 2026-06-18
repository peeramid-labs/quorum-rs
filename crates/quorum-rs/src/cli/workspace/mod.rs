#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

// Re-export shared SLA type from SDK — single source of truth.
pub use crate::scheduling::PolicySla;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub policies: HashMap<String, PolicyConfig>,
    #[serde(default)]
    pub orchestrators: HashMap<String, OrchestratorConfig>,
    #[serde(default)]
    pub rooms: HashMap<String, RoomConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<Vec<ContextRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_room: Option<String>,
    /// Agent fleet configuration reference (for `nsed serve`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
}

/// Agent fleet configuration — points to an existing agent config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// Path to agent fleet config YAML (relative to nsed.yaml parent directory).
    pub config_file: String,
    /// Optional port for the agent dashboard HTTP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_port: Option<u16>,
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

        let canonical = serde_json::to_string(&hashable).expect("PolicyConfig must serialize");
        let hash = Sha256::digest(canonical.as_bytes());
        format!("{hash:x}")
    }
}

/// Client-owned room — references a policy and optionally pins an orchestrator.
/// History is a property of the room (client-side), not the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RoomConfig {
    pub policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OrchestratorConfig {
    /// How this orchestrator is accessed.
    /// - `embedded`: in-process NATS + orchestrator (future, #171)
    /// - `remote`: orchestrator running elsewhere, agents register via JWT
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OrchestratorMode>,
    /// HTTP address of the orchestrator (e.g. `"http://localhost:8080"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Bearer token for API authentication.
    /// Supports `${ENV_VAR}` syntax for environment variable expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Direct NATS URL override (bypasses orchestrator registration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nats_url: Option<String>,
    /// Path to orchestrator settings YAML (relative to nsed.yaml parent).
    /// If present, `nsed serve` starts this orchestrator as a subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorMode {
    /// In-process NATS + orchestrator (future, #171)
    Embedded,
    /// Orchestrator running elsewhere, agents register via JWT
    Remote,
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

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("no policies defined")]
    NoPolicies,

    #[error("no orchestrators defined")]
    NoOrchestrators,

    #[error("no rooms defined")]
    NoRooms,

    #[error("orchestrator '{name}': remote mode requires an address")]
    RemoteMissingAddress { name: String },

    #[error("orchestrator '{name}': remote mode requires a token")]
    RemoteMissingToken { name: String },

    #[error("policy '{policy}': agents and roles are mutually exclusive")]
    AgentsAndRolesExclusive { policy: String },

    #[error("policy '{policy}': must specify either agents or roles")]
    NeitherAgentsNorRoles { policy: String },

    #[error("policy '{policy}' ({mode}): requires at least {min} agent(s), got {count}")]
    TooFewAgents {
        policy: String,
        count: usize,
        min: usize,
        mode: &'static str,
    },

    #[error("policy '{policy}': duplicate role name '{role}'")]
    DuplicateRole { policy: String, role: String },

    #[error("policy '{policy}', role '{role}': count must be >= 1")]
    RoleCountZero { policy: String, role: String },

    #[error("policy '{policy}', role '{role}': capabilities must not be empty")]
    EmptyCapabilities { policy: String, role: String },

    #[error("policy '{policy}': effort must be in [0.0, 1.0], got {value}")]
    InvalidConvergence { policy: String, value: f32 },

    #[error("policy '{policy}': max_rounds must be >= 1")]
    ZeroRounds { policy: String },

    #[error("policy '{policy}': sla.job_timeout_secs must be > 0")]
    ZeroTimeout { policy: String },

    #[error("policy '{policy}' ({mode}): total role count is {count}, need at least {min}")]
    TooFewRoleAgents {
        policy: String,
        count: u32,
        min: u32,
        mode: &'static str,
    },

    #[error(
        "policy '{policy}', role '{role}': pinned_agents count ({pinned}) exceeds role count ({count})"
    )]
    TooManyPinnedAgents {
        policy: String,
        role: String,
        pinned: usize,
        count: u8,
    },

    #[error("policy '{policy}', role '{role}': duplicate pinned agent '{agent}'")]
    DuplicatePinnedAgent {
        policy: String,
        role: String,
        agent: String,
    },

    #[error("policy '{policy}': too many agents ({count}), maximum is 255")]
    TooManyAgents { policy: String, count: usize },

    #[error("policy '{policy}', role '{role}': invalid capability tag '{tag}': {reason}")]
    InvalidCapability {
        policy: String,
        role: String,
        tag: String,
        reason: String,
    },

    #[error("policy '{policy}': invalid capability tag '{tag}': {reason}")]
    InvalidPolicyCapability {
        policy: String,
        tag: String,
        reason: String,
    },

    #[error("policy '{policy}': invalid tag '{tag}': {reason}")]
    InvalidPolicyTag {
        policy: String,
        tag: String,
        reason: String,
    },

    #[error("room '{room}': references unknown policy '{policy}'")]
    UnknownPolicy { room: String, policy: String },

    #[error("room '{room}': references unknown orchestrator '{orchestrator}'")]
    UnknownOrchestrator { room: String, orchestrator: String },

    #[error("default_room '{name}' does not match any defined room")]
    InvalidDefaultRoom { name: String },

    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse config YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("room '{name}' not found (available: {available})")]
    RoomNotFound { name: String, available: String },

    #[error(
        "multiple rooms defined but no --room flag or default_room set (available: {available})"
    )]
    AmbiguousRoom { available: String },

    #[error("policy '{policy}': mode 'moderator' requires exactly one role with moderator: true")]
    ModeratorRoleMissing { policy: String },

    #[error("policy '{policy}': at most one role may have moderator: true")]
    MultipleModeratorRoles { policy: String },

    #[error(
        "policy '{policy}': mode 'moderator' requires roles (not a flat agents list) \
         so a role can be designated moderator: true"
    )]
    ModeratorRequiresRoles { policy: String },

    #[error("{0}")]
    ConfigFree(String),
}

impl ConfigError {
    /// True for provisioning shortfalls — a policy that doesn't yet have
    /// enough agents to start. These are real, fixable states a management
    /// view should display (as a red fill indicator) rather than reject at
    /// load time, unlike structural errors (parse, unknown refs).
    pub fn is_provisioning(&self) -> bool {
        matches!(
            self,
            ConfigError::TooFewAgents { .. } | ConfigError::TooFewRoleAgents { .. }
        )
    }
}

/// Human-readable name for a `PolicyMode` used in validation error
/// messages. Kept as a simple free function rather than an `impl
/// Display` on the enum to avoid pulling the whole `PolicyMode` import
/// into the error messages module and to keep the strings stable even
/// if the enum ever gets a Display impl with different formatting.
fn policy_mode_name(mode: &PolicyMode) -> &'static str {
    match mode {
        PolicyMode::Deliberation => "deliberation",
        PolicyMode::Passthrough => "passthrough",
        PolicyMode::Moderator => "moderator",
    }
}

pub fn validate_capability_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err("capability tag must not be empty".to_string());
    }
    if tag == "*" {
        return Ok(());
    }
    if let Some(prefix) = tag.strip_suffix(":*") {
        if prefix.is_empty() {
            return Err("namespace before :* must not be empty".to_string());
        }
        return validate_tag_segment(prefix);
    }
    if tag.contains(':') {
        let parts: Vec<&str> = tag.splitn(2, ':').collect();
        validate_tag_segment(parts[0])?;
        validate_tag_segment(parts[1])?;
        return Ok(());
    }
    validate_tag_segment(tag)
}

fn validate_tag_segment(segment: &str) -> Result<(), String> {
    if segment.is_empty() {
        return Err("tag segment must not be empty".to_string());
    }
    if !segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "tag segment '{segment}' contains invalid characters (allowed: alphanumeric, -, _)"
        ));
    }
    Ok(())
}

impl WorkspaceConfig {
    /// Load workspace config from a YAML file, deserialize and validate.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = serde_yaml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Load `nsed.yaml` when present; otherwise synthesize a config-free
    /// single-orchestrator workspace from the redeemed `~/.nsed/` files
    /// (see [`crate::cli::endpoint::remote_workspace`]). The discovery and
    /// submit commands use this so onboarding needs no workspace file —
    /// `quorum redeem` → `quorum run`/`status`/`rooms` just work.
    pub fn load_or_remote_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            crate::cli::endpoint::remote_workspace().map_err(ConfigError::ConfigFree)
        }
    }

    /// Load for management views (the TUI). Same as
    /// [`Self::load_or_remote_default`], but treats provisioning shortfalls
    /// (too-few-agents) as non-fatal: an under-provisioned policy is a real
    /// state the UI surfaces as a red fill indicator, not a reason to refuse
    /// to open. Structural errors (parse, unknown refs, missing address) still
    /// fail.
    pub fn load_or_remote_default_for_view(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return crate::cli::endpoint::remote_workspace().map_err(ConfigError::ConfigFree);
        }
        let contents = std::fs::read_to_string(path)?;
        let config: Self = serde_yaml::from_str(&contents)?;
        match config.validate() {
            Ok(()) => Ok(config),
            Err(e) if e.is_provisioning() => Ok(config),
            Err(e) => Err(e),
        }
    }

    /// Resolve which room to use based on the priority chain:
    /// 1. Explicit `room_flag` (from --room CLI arg)
    /// 2. `default_room` from config
    /// 3. Auto-select if exactly one room
    /// 4. Error if multiple rooms and no default
    pub fn resolve_room<'a>(
        &'a self,
        room_flag: Option<&'a str>,
    ) -> Result<(&'a str, &'a RoomConfig), ConfigError> {
        let available = || {
            self.rooms
                .keys()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };

        if let Some(name) = room_flag {
            return self.rooms.get(name).map(|r| (name, r)).ok_or_else(|| {
                ConfigError::RoomNotFound {
                    name: name.to_string(),
                    available: available(),
                }
            });
        }

        if let Some(ref default) = self.default_room {
            return self
                .rooms
                .get_key_value(default.as_str())
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| ConfigError::InvalidDefaultRoom {
                    name: default.clone(),
                });
        }

        if self.rooms.len() == 1 {
            let (k, v) = self.rooms.iter().next().unwrap();
            return Ok((k.as_str(), v));
        }

        Err(ConfigError::AmbiguousRoom {
            available: available(),
        })
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        // Minimal config: at least agents or (orchestrators + policies + rooms)
        let has_rooms = !self.rooms.is_empty();

        // When rooms are defined, require full routing config
        if has_rooms {
            if self.orchestrators.is_empty() {
                return Err(ConfigError::NoOrchestrators);
            }
            if self.policies.is_empty() {
                return Err(ConfigError::NoPolicies);
            }
        }

        // Validate orchestrators
        for (name, orch) in &self.orchestrators {
            if orch.mode == Some(OrchestratorMode::Remote) {
                if orch.address.is_none() {
                    return Err(ConfigError::RemoteMissingAddress { name: name.clone() });
                }
                if orch.token.is_none() {
                    return Err(ConfigError::RemoteMissingToken { name: name.clone() });
                }
            }
        }

        // Validate policies
        for (policy_name, policy) in &self.policies {
            Self::validate_policy(policy_name, policy)?;
        }

        // Validate rooms
        for (room_name, room) in &self.rooms {
            if !self.policies.contains_key(&room.policy) {
                return Err(ConfigError::UnknownPolicy {
                    room: room_name.clone(),
                    policy: room.policy.clone(),
                });
            }
            if let Some(ref orch) = room.orchestrator
                && !self.orchestrators.contains_key(orch)
            {
                return Err(ConfigError::UnknownOrchestrator {
                    room: room_name.clone(),
                    orchestrator: orch.clone(),
                });
            }
        }

        if let Some(ref default) = self.default_room
            && !self.rooms.contains_key(default)
        {
            return Err(ConfigError::InvalidDefaultRoom {
                name: default.clone(),
            });
        }

        Ok(())
    }

    fn validate_policy(name: &str, policy: &PolicyConfig) -> Result<(), ConfigError> {
        match (&policy.agents, &policy.roles) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::AgentsAndRolesExclusive {
                    policy: name.to_string(),
                });
            }
            (None, None) => {
                return Err(ConfigError::NeitherAgentsNorRoles {
                    policy: name.to_string(),
                });
            }
            (Some(agents), None) => {
                if policy.mode == PolicyMode::Moderator {
                    return Err(ConfigError::ModeratorRequiresRoles {
                        policy: name.to_string(),
                    });
                }
                let min_agents = match policy.mode {
                    PolicyMode::Deliberation => 2,
                    PolicyMode::Passthrough | PolicyMode::Moderator => 1,
                };
                if agents.len() < min_agents {
                    return Err(ConfigError::TooFewAgents {
                        policy: name.to_string(),
                        count: agents.len(),
                        min: min_agents,
                        mode: policy_mode_name(&policy.mode),
                    });
                }
                if agents.len() > 255 {
                    return Err(ConfigError::TooManyAgents {
                        policy: name.to_string(),
                        count: agents.len(),
                    });
                }
            }
            (None, Some(roles)) => {
                let mut seen_roles = HashSet::new();
                let mut seen_pinned: HashSet<&String> = HashSet::new();
                let mut total_count: u32 = 0;
                let mut moderator_count: u32 = 0;

                for role in roles {
                    if !seen_roles.insert(&role.role) {
                        return Err(ConfigError::DuplicateRole {
                            policy: name.to_string(),
                            role: role.role.clone(),
                        });
                    }
                    if role.count == 0 {
                        return Err(ConfigError::RoleCountZero {
                            policy: name.to_string(),
                            role: role.role.clone(),
                        });
                    }
                    if role.capabilities.is_empty() {
                        return Err(ConfigError::EmptyCapabilities {
                            policy: name.to_string(),
                            role: role.role.clone(),
                        });
                    }
                    for tag in &role.capabilities {
                        validate_capability_tag(tag).map_err(|reason| {
                            ConfigError::InvalidCapability {
                                policy: name.to_string(),
                                role: role.role.clone(),
                                tag: tag.clone(),
                                reason,
                            }
                        })?;
                    }
                    if let Some(ref pinned) = role.pinned_agents {
                        if pinned.len() > role.count as usize {
                            return Err(ConfigError::TooManyPinnedAgents {
                                policy: name.to_string(),
                                role: role.role.clone(),
                                pinned: pinned.len(),
                                count: role.count,
                            });
                        }
                        for agent in pinned {
                            if !seen_pinned.insert(agent) {
                                return Err(ConfigError::DuplicatePinnedAgent {
                                    policy: name.to_string(),
                                    role: role.role.clone(),
                                    agent: agent.clone(),
                                });
                            }
                        }
                    }
                    if role.moderator {
                        moderator_count += 1;
                    }
                    total_count += role.count as u32;
                }

                if moderator_count > 1 {
                    return Err(ConfigError::MultipleModeratorRoles {
                        policy: name.to_string(),
                    });
                }
                if policy.mode == PolicyMode::Moderator && moderator_count == 0 {
                    return Err(ConfigError::ModeratorRoleMissing {
                        policy: name.to_string(),
                    });
                }

                let min_total: u32 = match policy.mode {
                    PolicyMode::Deliberation => 2,
                    PolicyMode::Passthrough | PolicyMode::Moderator => 1,
                };
                if total_count < min_total {
                    return Err(ConfigError::TooFewRoleAgents {
                        policy: name.to_string(),
                        count: total_count,
                        min: min_total,
                        mode: policy_mode_name(&policy.mode),
                    });
                }
            }
        }

        if policy.max_rounds == 0 {
            return Err(ConfigError::ZeroRounds {
                policy: name.to_string(),
            });
        }

        if let Some(caps) = &policy.capabilities {
            for tag in caps {
                validate_capability_tag(tag).map_err(|reason| {
                    ConfigError::InvalidPolicyCapability {
                        policy: name.to_string(),
                        tag: tag.clone(),
                        reason,
                    }
                })?;
            }
        }

        if let Some(tags) = &policy.tags {
            for tag in tags {
                validate_capability_tag(tag).map_err(|reason| ConfigError::InvalidPolicyTag {
                    policy: name.to_string(),
                    tag: tag.clone(),
                    reason,
                })?;
            }
        }

        if !(0.0..=1.0).contains(&policy.effort) {
            return Err(ConfigError::InvalidConvergence {
                policy: name.to_string(),
                value: policy.effort,
            });
        }

        if let Some(sla) = &policy.sla
            && sla.job_timeout_secs == 0
        {
            return Err(ConfigError::ZeroTimeout {
                policy: name.to_string(),
            });
        }

        Ok(())
    }
}
