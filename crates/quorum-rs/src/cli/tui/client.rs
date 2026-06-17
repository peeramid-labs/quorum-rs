use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cli::remote::RemoteOrchestrator;
use crate::cli::request::{DeliberationRequest, build_request_raw_policy_id};
use crate::cli::tui::event::DataEvent;
use crate::cli::workspace::PolicyConfig;

/// Async bridge between `RemoteOrchestrator` and the TUI event loop.
///
/// Spawns tokio tasks that send results through an mpsc channel.
/// Views never call async functions directly — they request data
/// via `FetchRequest`, and the dispatcher routes them here.
pub struct TuiClient {
    tx: mpsc::UnboundedSender<DataEvent>,
    sse_handle: Option<JoinHandle<()>>,
}

impl TuiClient {
    pub fn new(tx: mpsc::UnboundedSender<DataEvent>) -> Self {
        Self {
            tx,
            sse_handle: None,
        }
    }

    /// Cancel any running SSE stream task.
    pub fn cancel_sse_stream(&mut self) {
        if let Some(handle) = self.sse_handle.take() {
            handle.abort();
        }
    }

    /// Fetch agents from a remote orchestrator.
    pub fn fetch_agents(&self, remote: RemoteOrchestrator, orch_name: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.agents().await {
                Ok(agents) => {
                    let _ = tx.send(DataEvent::AgentsLoaded {
                        orchestrator: orch_name,
                        agents,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "agents".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Check health of a remote orchestrator.
    pub fn check_health(&self, remote: RemoteOrchestrator, orch_name: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.health().await {
                Ok(resp) => {
                    let _ = tx.send(DataEvent::HealthResult {
                        orchestrator: orch_name,
                        result: Ok(resp),
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::HealthResult {
                        orchestrator: orch_name,
                        result: Err(e.to_string()),
                    });
                }
            }
        });
    }

    /// Start SSE event stream for a job, cancelling any previous stream.
    pub fn start_sse_stream(&mut self, remote: RemoteOrchestrator, job_id: String) {
        self.cancel_sse_stream();
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            match remote.open_sse_stream(&job_id).await {
                Ok(mut rx) => {
                    while let Some(event) = rx.recv().await {
                        if tx.send(DataEvent::SseEvent(event)).is_err() {
                            break;
                        }
                    }
                    // Stream closed without a terminal SSE frame — notify the view
                    let _ = tx.send(DataEvent::FetchError {
                        context: "sse_stream_closed".into(),
                        error: "SSE stream ended unexpectedly".into(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "sse_stream".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
        self.sse_handle = Some(handle);
    }

    /// Fetch policies from a remote orchestrator.
    pub fn fetch_policies(
        &self,
        remote: RemoteOrchestrator,
        orch_name: String,
        tag: Option<String>,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.policies(tag.as_deref()).await {
                Ok(policies) => {
                    let _ = tx.send(DataEvent::PoliciesLoaded {
                        orchestrator: orch_name,
                        policies,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "policies".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Fetch the room list — `GET /rooms` (grant-filtered).
    pub fn fetch_rooms(&self, remote: RemoteOrchestrator, orch_name: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.discover_rooms().await {
                Ok(rooms) => {
                    let _ = tx.send(DataEvent::RoomsLoaded {
                        orchestrator: orch_name,
                        rooms,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "rooms".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Create a room — `POST /admin/api/rooms`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_room(
        &self,
        remote: RemoteOrchestrator,
        orch_name: String,
        id: String,
        tags: Vec<String>,
        visibility: String,
        policy: Option<String>,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote
                .create_room(&id, &tags, &visibility, policy.as_deref())
                .await
            {
                Ok(_) => {
                    let _ = tx.send(DataEvent::RoomMutated {
                        orchestrator: orch_name,
                        action: "created".into(),
                        id,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "rooms".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Delete a room — `DELETE /admin/api/rooms/{id}`.
    pub fn delete_room(&self, remote: RemoteOrchestrator, orch_name: String, id: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.delete_room(&id).await {
                Ok(()) => {
                    let _ = tx.send(DataEvent::RoomMutated {
                        orchestrator: orch_name,
                        action: "deleted".into(),
                        id,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "rooms".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Config-free submit: resolve a remote policy label → `policy_id`
    /// (`GET /policies`), then submit a deliberation to `room` with `task`.
    /// Used by the main-menu launcher when a room carries a policy label
    /// but the client has no local `PolicyConfig` (no nsed.yaml).
    #[allow(clippy::too_many_arguments)]
    pub fn submit_with_remote_policy(
        &self,
        remote: RemoteOrchestrator,
        orch_name: String,
        room: String,
        policy_label: String,
        task: String,
        effort: Option<f32>,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let policies = match remote.discover_policies().await {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "submit".into(),
                        error: e.to_string(),
                    });
                    return;
                }
            };
            let Some(policy_id) = policies
                .iter()
                .find(|p| p.name == policy_label || p.policy_id == policy_label)
                .map(|p| p.policy_id.clone())
            else {
                let _ = tx.send(DataEvent::FetchError {
                    context: "submit".into(),
                    error: format!(
                        "policy {policy_label:?} not found on the orchestrator (GET /policies)"
                    ),
                });
                return;
            };
            let mut req = build_request_raw_policy_id(&policy_id, &task);
            req.room_id = room;
            if let Some(e) = effort {
                req.effort = Some(e);
            }
            match remote.submit(&req).await {
                Ok(job_id) => {
                    let _ = tx.send(DataEvent::JobSubmitted {
                        job_id,
                        orchestrator: orch_name,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "submit".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Submit a deliberation job (static policy — no push needed).
    pub fn submit_job(
        &self,
        remote: RemoteOrchestrator,
        req: DeliberationRequest,
        orch_name: String,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.submit(&req).await {
                Ok(job_id) => {
                    let _ = tx.send(DataEvent::JobSubmitted {
                        job_id,
                        orchestrator: orch_name,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "submit".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Push a role-based policy then submit the deliberation job.
    pub fn push_policy_and_submit(
        &self,
        remote: RemoteOrchestrator,
        policy_name: String,
        policy_config: PolicyConfig,
        req: DeliberationRequest,
        orch_name: String,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Step 1: push policy
            if let Err(e) = remote.push_policy(&policy_name, &policy_config).await {
                let _ = tx.send(DataEvent::FetchError {
                    context: "push_policy".into(),
                    error: e.to_string(),
                });
                return;
            }

            // Step 2: submit
            match remote.submit(&req).await {
                Ok(job_id) => {
                    let _ = tx.send(DataEvent::JobSubmitted {
                        job_id,
                        orchestrator: orch_name,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "submit".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Inject a message into a running deliberation.
    pub fn inject_message(&self, remote: RemoteOrchestrator, job_id: String, message: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.inject_message(&job_id, &message, None).await {
                Ok(resp) => {
                    let _ = tx.send(DataEvent::MessageInjected {
                        job_id,
                        sequence: resp.sequence,
                        round: resp.injected_at_round,
                    });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "inject".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }
}

impl Drop for TuiClient {
    fn drop(&mut self) {
        self.cancel_sse_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_client_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let _client = TuiClient::new(tx);
    }

    #[test]
    fn cancel_sse_stream_noop_when_none() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(tx);
        // Should not panic when no stream exists.
        client.cancel_sse_stream();
        assert!(client.sse_handle.is_none());
    }

    #[tokio::test]
    async fn cancel_sse_stream_aborts_handle() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(tx);

        // Spawn a task that will block forever.
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        client.sse_handle = Some(handle);

        client.cancel_sse_stream();
        assert!(client.sse_handle.is_none());
    }

    #[tokio::test]
    async fn drop_cancels_sse_stream() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(tx);

        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let abort_handle = handle.abort_handle();
        client.sse_handle = Some(handle);

        drop(client);
        // Yield to let the runtime propagate the abort.
        tokio::task::yield_now().await;
        assert!(abort_handle.is_finished());
    }
}
