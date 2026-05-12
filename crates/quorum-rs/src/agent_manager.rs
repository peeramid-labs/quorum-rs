//! # Agent Lifecycle Manager
//!
//! Manages the lifecycle of NSED agents — spawn, stop, replace, and hot-reload.
//! Uses the YAML config file as the source of truth, with atomic writes for
//! crash recovery.
//!
//! The manager is shared (behind `Arc<RwLock>`) with the REST handlers so
//! agents can be added/removed at runtime without restarting the process.

use crate::agents::{AgentConfig, ChatCapable};
use crate::status::SharedAgentStatus;
use crate::workers::NatsNsedWorker;
use crate::workers::buffer::ResponseBuffer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// A running agent with its worker task handle and metadata.
pub struct RunningAgent {
    /// The agent's current config.
    pub config: Arc<RwLock<AgentConfig>>,
    /// Task handle for the worker — `abort()` to stop.
    pub handle: JoinHandle<()>,
    /// Shared status snapshot (for dashboard).
    pub status: Option<SharedAgentStatus>,
    /// Chat capability (for dashboard).
    pub chat: Option<Arc<dyn ChatCapable>>,
    /// HITL response buffer.
    pub buffer: Option<Arc<ResponseBuffer>>,
    /// Pause handle.
    pub pause: Arc<AtomicBool>,
}

/// Snapshot of dashboard-relevant maps extracted from the manager.
pub struct DashboardMaps {
    pub configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
    pub statuses: HashMap<String, SharedAgentStatus>,
    pub chats: HashMap<String, Arc<dyn ChatCapable>>,
    pub buffers: HashMap<String, Arc<ResponseBuffer>>,
    pub pauses: HashMap<String, Arc<AtomicBool>>,
}

/// Manages agent lifecycle — spawn, stop, replace, hot-reload.
///
/// Thread-safe: wrap in `Arc<RwLock<AgentManager>>` for shared access
/// between the REST API and the main runner.
pub struct AgentManager {
    /// Currently running agents, keyed by name.
    pub running: HashMap<String, RunningAgent>,
    /// Path to the YAML config file (source of truth).
    config_path: Option<PathBuf>,
    /// Serializes YAML persistence to prevent concurrent read-modify-write races.
    persist_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AgentManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self {
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
            running: HashMap::new(),
            config_path: None,
        }
    }

    /// Set the config file path for persistence.
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Register a running agent. Replaces any existing agent with the same name.
    pub fn register(
        &mut self,
        name: String,
        worker: NatsNsedWorker,
        config: AgentConfig,
    ) -> &RunningAgent {
        // Stop existing agent if present
        if let Some(existing) = self.running.remove(&name) {
            existing.handle.abort();
        }

        // Extract metadata from worker before consuming it
        let status = worker.status().cloned();
        let chat = worker.chat_agent().cloned();
        let buffer = worker.response_buffer().cloned();
        let pause = worker.pause_handle();
        let config = Arc::new(RwLock::new(config));

        let agent_name = name.clone();
        let handle = tokio::spawn(async move {
            tracing::info!("🟢 Agent '{}' started", agent_name);
            if let Err(e) = worker.run().await {
                tracing::error!("Agent '{}' crashed: {:?}", agent_name, e);
            }
        });

        self.running.insert(
            name.clone(),
            RunningAgent {
                config,
                handle,
                status,
                chat,
                buffer,
                pause,
            },
        );

        self.running.get(&name).unwrap()
    }

    /// Stop and remove an agent by name. Returns true if the agent existed.
    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(agent) = self.running.remove(name) {
            agent.handle.abort();
            tracing::info!("🔴 Agent '{}' stopped", name);
            true
        } else {
            false
        }
    }

    /// Check if an agent with the given name is running.
    pub fn has_agent(&self, name: &str) -> bool {
        self.running.contains_key(name)
    }

    /// Get the names of all running agents.
    pub fn agent_names(&self) -> Vec<String> {
        self.running.keys().cloned().collect()
    }

    /// Get agent count.
    pub fn count(&self) -> usize {
        self.running.len()
    }

    /// Extract dashboard maps (configs, statuses, buffers, etc.) for the control plane.
    /// This is a snapshot — subsequent register/remove calls won't update these maps.
    pub fn dashboard_maps(&self) -> DashboardMaps {
        let mut configs = HashMap::new();
        let mut statuses = HashMap::new();
        let mut chats = HashMap::new();
        let mut buffers = HashMap::new();
        let mut pauses = HashMap::new();

        for (name, agent) in &self.running {
            configs.insert(name.clone(), agent.config.clone());
            if let Some(ref s) = agent.status {
                statuses.insert(name.clone(), s.clone());
            }
            if let Some(ref c) = agent.chat {
                chats.insert(name.clone(), c.clone());
            }
            if let Some(ref b) = agent.buffer {
                buffers.insert(name.clone(), b.clone());
            }
            pauses.insert(name.clone(), agent.pause.clone());
        }

        DashboardMaps {
            configs,
            statuses,
            chats,
            buffers,
            pauses,
        }
    }

    /// Persist the given agent configs to the YAML config file.
    /// Uses atomic write (temp file + rename) for crash safety.
    pub async fn persist_agents_to_yaml(&self, agents: &[AgentConfig]) -> Result<(), String> {
        let _guard = self.persist_lock.lock().await;
        let Some(ref path) = self.config_path else {
            return Err("No config path set".to_string());
        };

        // Read current YAML, update agents section, write back
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Failed to read config: {e}"))?;

        let mut yaml: serde_yaml::Value =
            serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse YAML: {e}"))?;

        // Serialize agents to YAML value
        let agents_val =
            serde_yaml::to_value(agents).map_err(|e| format!("Failed to serialize agents: {e}"))?;

        // Update the agents key
        if let serde_yaml::Value::Mapping(ref mut map) = yaml {
            map.insert(serde_yaml::Value::String("agents".to_string()), agents_val);
        } else {
            return Err("YAML root is not a mapping — cannot update agents key".to_string());
        }

        // Atomic write: temp file + rename
        let yaml_str =
            serde_yaml::to_string(&yaml).map_err(|e| format!("Failed to serialize YAML: {e}"))?;
        let tmp_path = path.with_extension("yml.tmp");
        tokio::fs::write(&tmp_path, &yaml_str)
            .await
            .map_err(|e| format!("Failed to write temp file: {e}"))?;
        tokio::fs::rename(&tmp_path, path)
            .await
            .map_err(|e| format!("Failed to rename temp file: {e}"))?;

        tracing::info!(path = %path.display(), agents = agents.len(), "Config file updated");
        Ok(())
    }
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_manager_default_is_empty() {
        let mgr = AgentManager::new();
        assert_eq!(mgr.count(), 0);
        assert!(mgr.agent_names().is_empty());
        assert!(!mgr.has_agent("x"));
    }
}
