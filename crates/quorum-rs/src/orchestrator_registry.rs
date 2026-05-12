//! Dynamic orchestrator registry for runtime orchestrator management.
//!
//! Tracks active orchestrator connections and supports adding new
//! orchestrators at runtime. New orchestrators are sent to the
//! [`MultiAgentRunner`](crate::multi_agent::MultiAgentRunner) via a channel
//! so it can spawn additional workers without restarting.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

/// Request to add a new orchestrator at runtime.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "status-server", derive(utoipa::ToSchema))]
pub struct AddOrchestratorRequest {
    /// Orchestrator HTTP URL (e.g. "http://orch-2:8080").
    pub url: String,
    /// Bearer token for authentication (supports `${ENV_VAR}` expansion).
    #[serde(default)]
    pub bearer_token: Option<String>,
    /// Optional orchestrator ID. Derived from URL hostname if omitted.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional list of agent names to connect. If empty, all agents connect.
    #[serde(default)]
    pub agent_names: Vec<String>,
}

/// An active orchestrator connection.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "status-server", derive(utoipa::ToSchema))]
pub struct ActiveOrchestrator {
    /// Human-readable orchestrator ID.
    pub id: String,
    /// HTTP URL of the orchestrator.
    pub url: String,
    /// NATS URL obtained from JWT registration.
    pub nats_url: String,
    /// Agent worker names connected to this orchestrator.
    pub connected_agents: Vec<String>,
    /// When this orchestrator was connected.
    #[cfg_attr(feature = "status-server", schema(value_type = String, format = "date-time"))]
    pub connected_at: DateTime<Utc>,
}

/// Shared registry of active orchestrators.
///
/// Thread-safe read/write access to the list of connected orchestrators,
/// plus a channel sender for dispatching new orchestrator requests to the
/// runner loop.
#[derive(Clone)]
pub struct OrchestratorRegistry {
    entries: Arc<RwLock<Vec<ActiveOrchestrator>>>,
    /// Bearer tokens per orchestrator ID (never exposed via API).
    bearer_tokens: Arc<RwLock<std::collections::HashMap<String, String>>>,
    add_tx: mpsc::UnboundedSender<AddOrchestratorRequest>,
}

impl OrchestratorRegistry {
    /// Create a new registry and the receiving end of the add-orchestrator channel.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<AddOrchestratorRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let registry = Self {
            entries: Arc::new(RwLock::new(Vec::new())),
            bearer_tokens: Arc::new(RwLock::new(std::collections::HashMap::new())),
            add_tx: tx,
        };
        (registry, rx)
    }

    /// List all active orchestrators.
    pub async fn list(&self) -> Vec<ActiveOrchestrator> {
        self.entries.read().await.clone()
    }

    /// Record or update an active orchestrator (upserts by id).
    ///
    /// On upsert, preserves any `connected_agents` that were added via
    /// [`add_agent_to_orchestrator`] since the previous registration.
    pub async fn add(&self, mut entry: ActiveOrchestrator) {
        let mut entries = self.entries.write().await;
        if let Some(existing) = entries.iter_mut().find(|e| e.id == entry.id) {
            // Merge previously-accumulated connected_agents into the new entry
            // to avoid losing agents registered between the initial add and this
            // upsert (e.g. on orchestrator reconnect / JWT renewal).
            for agent in &existing.connected_agents {
                if !entry.connected_agents.contains(agent) {
                    entry.connected_agents.push(agent.clone());
                }
            }
            *existing = entry;
        } else {
            entries.push(entry);
        }
    }

    /// Record an agent worker as connected to an orchestrator.
    pub async fn add_agent_to_orchestrator(&self, orch_id: &str, agent_name: &str) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.iter_mut().find(|e| e.id == orch_id) {
            if !entry.connected_agents.iter().any(|a| a == agent_name) {
                entry.connected_agents.push(agent_name.to_string());
            }
        }
    }

    /// Store a bearer token for an orchestrator (called during registration).
    pub async fn set_bearer_token(&self, orch_id: &str, token: String) {
        self.bearer_tokens
            .write()
            .await
            .insert(orch_id.to_string(), token);
    }

    /// Get the bearer token for an orchestrator (for budget proxy calls).
    pub async fn get_bearer_token(&self, orch_id: &str) -> Option<String> {
        self.bearer_tokens.read().await.get(orch_id).cloned()
    }

    /// Send a request to add a new orchestrator at runtime.
    pub fn request_add(&self, req: AddOrchestratorRequest) -> Result<(), String> {
        self.add_tx
            .send(req)
            .map_err(|e| format!("Runner channel closed: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_starts_empty() {
        let (registry, _rx) = OrchestratorRegistry::new();
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn add_and_list_orchestrator() {
        let (registry, _rx) = OrchestratorRegistry::new();
        registry
            .add(ActiveOrchestrator {
                id: "test".into(),
                url: "http://localhost:8080".into(),
                nats_url: "nats://localhost:4222".into(),
                connected_agents: vec!["ALPHA".into()],
                connected_at: Utc::now(),
            })
            .await;
        let list = registry.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "test");
        assert_eq!(list[0].connected_agents, vec!["ALPHA"]);
    }

    #[tokio::test]
    async fn add_agent_to_existing_orchestrator() {
        let (registry, _rx) = OrchestratorRegistry::new();
        registry
            .add(ActiveOrchestrator {
                id: "orch1".into(),
                url: "http://localhost:8080".into(),
                nats_url: "nats://localhost:4222".into(),
                connected_agents: vec!["ALPHA".into()],
                connected_at: Utc::now(),
            })
            .await;
        registry.add_agent_to_orchestrator("orch1", "BETA").await;
        let list = registry.list().await;
        assert_eq!(list[0].connected_agents, vec!["ALPHA", "BETA"]);
    }

    #[tokio::test]
    async fn add_agent_deduplicates() {
        let (registry, _rx) = OrchestratorRegistry::new();
        registry
            .add(ActiveOrchestrator {
                id: "orch1".into(),
                url: "http://localhost:8080".into(),
                nats_url: "nats://localhost:4222".into(),
                connected_agents: vec!["ALPHA".into()],
                connected_at: Utc::now(),
            })
            .await;
        registry.add_agent_to_orchestrator("orch1", "ALPHA").await;
        let list = registry.list().await;
        assert_eq!(list[0].connected_agents, vec!["ALPHA"]);
    }

    #[tokio::test]
    async fn request_add_sends_through_channel() {
        let (registry, mut rx) = OrchestratorRegistry::new();
        registry
            .request_add(AddOrchestratorRequest {
                url: "http://orch-2:8080".into(),
                bearer_token: Some("token123".into()),
                id: Some("secondary".into()),
                agent_names: vec!["ALPHA".into()],
            })
            .unwrap();

        let req = rx.recv().await.unwrap();
        assert_eq!(req.url, "http://orch-2:8080");
        assert_eq!(req.id, Some("secondary".into()));
        assert_eq!(req.agent_names, vec!["ALPHA"]);
    }

    #[tokio::test]
    async fn upsert_preserves_connected_agents() {
        let (registry, _rx) = OrchestratorRegistry::new();
        // Initial add with one agent
        registry
            .add(ActiveOrchestrator {
                id: "orch1".into(),
                url: "http://localhost:8080".into(),
                nats_url: "nats://localhost:4222".into(),
                connected_agents: vec![],
                connected_at: Utc::now(),
            })
            .await;
        // Register agents after connection
        registry.add_agent_to_orchestrator("orch1", "ALPHA").await;
        registry.add_agent_to_orchestrator("orch1", "BETA").await;
        // Upsert (e.g. on reconnect) with empty connected_agents
        registry
            .add(ActiveOrchestrator {
                id: "orch1".into(),
                url: "http://new-host:8080".into(),
                nats_url: "nats://new-host:4222".into(),
                connected_agents: vec![],
                connected_at: Utc::now(),
            })
            .await;
        let list = registry.list().await;
        assert_eq!(list.len(), 1);
        // URL updated, but agents preserved
        assert_eq!(list[0].url, "http://new-host:8080");
        assert!(list[0].connected_agents.contains(&"ALPHA".to_string()));
        assert!(list[0].connected_agents.contains(&"BETA".to_string()));
    }

    #[tokio::test]
    async fn request_add_fails_when_receiver_dropped() {
        let (registry, rx) = OrchestratorRegistry::new();
        drop(rx);
        let result = registry.request_add(AddOrchestratorRequest {
            url: "http://orch:8080".into(),
            bearer_token: None,
            id: None,
            agent_names: vec![],
        });
        assert!(result.is_err());
    }
}
