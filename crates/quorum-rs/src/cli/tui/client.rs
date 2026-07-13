use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cli::remote::{JobResult, RemoteError, RemoteOrchestrator};
use crate::cli::request::{DeliberationRequest, build_request_raw_policy_id};
use crate::cli::tui::event::DataEvent;
use crate::cli::workspace::PolicyConfig;

/// What a reconcile of a thread's pending job should do, given the server's
/// result lookup.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileAction {
    /// A completed deliberation with content — record it as the reply.
    AppendReply(String),
    /// Terminally done with nothing to record (completed-empty, failed, or the
    /// status entry is gone / gc'd) — drop the stale pending marker so the
    /// thread isn't stuck "deliberating" forever.
    ClearStale,
    /// Still live (pending/running/claimed) or a transient error — keep waiting.
    Wait,
}

/// Decide the reconcile action from a `result()` lookup. Pure, so the
/// stuck-marker logic is unit-testable without a live orchestrator.
fn reconcile_decision(res: &Result<JobResult, RemoteError>) -> ReconcileAction {
    match res {
        Ok(r) if r.status == "completed" => {
            match r.result.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(content) => ReconcileAction::AppendReply(content.to_string()),
                None => ReconcileAction::ClearStale,
            }
        }
        Ok(r) if r.status.starts_with("failed") => ReconcileAction::ClearStale,
        // pending / running / claimed — the deliberation is still live.
        Ok(_) => ReconcileAction::Wait,
        // 404: the status entry is gone (job gc'd / never existed) → stale.
        Err(RemoteError::ApiError { status: 404, .. }) => ReconcileAction::ClearStale,
        // 5xx / network / parse — transient; keep the marker and retry later.
        Err(_) => ReconcileAction::Wait,
    }
}

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

    /// Reconcile a reopened thread's pending job against the server: fetch its
    /// result and, if the deliberation has finished, append the reply to the
    /// store and signal a reload. A still-running or failed job is left alone.
    pub fn reconcile_thread(&self, remote: RemoteOrchestrator, job_id: String, thread_id: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let store = crate::cli::thread::ThreadStore::new();
            let res = remote.result(&job_id).await;
            match reconcile_decision(&res) {
                ReconcileAction::AppendReply(content) => {
                    store.append_reply(&thread_id, &content, &job_id, None);
                    let _ = tx.send(DataEvent::ThreadReconciled { thread_id });
                }
                ReconcileAction::ClearStale => {
                    // Dead/gone job — drop the marker so the thread unsticks and
                    // a new turn isn't shadowed by a phantom "deliberating…".
                    store.clear_pending_job(&thread_id);
                    let _ = tx.send(DataEvent::ThreadReconciled { thread_id });
                }
                ReconcileAction::Wait => {}
            }
        });
    }

    /// Query the caller's active deliberations and map each thread-scoped job to
    /// its thread, so `^D`/stop resolve a thread's running job from the server.
    pub fn refresh_thread_jobs(&self, remote: RemoteOrchestrator) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(jobs) = remote.deliberations().await {
                let jobs = super::thread_jobs_from_list(&jobs);
                if !jobs.is_empty() {
                    let _ = tx.send(DataEvent::ThreadJobsLoaded { jobs });
                }
            }
        });
    }

    /// Cancel a thread's running deliberation. On success, clears the thread's
    /// pending marker (so a follow-up can send) and signals a reload.
    pub fn cancel_job(&self, remote: RemoteOrchestrator, job_id: String, thread_id: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match remote.cancel(&job_id).await {
                Ok(()) => {
                    crate::cli::thread::ThreadStore::new().clear_pending_job(&thread_id);
                    let _ = tx.send(DataEvent::ThreadReconciled { thread_id });
                }
                Err(e) => {
                    let _ = tx.send(DataEvent::FetchError {
                        context: "cancel".into(),
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Answer an agent's pending `ask_user` question. The blocked agent resumes
    /// with the result (or times out on its own round budget if we're too slow).
    pub fn respond_tool_call(
        &self,
        remote: RemoteOrchestrator,
        job_id: String,
        call_id: String,
        result: String,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = remote
                .respond_to_tool_call(&job_id, &call_id, &result)
                .await
            {
                let _ = tx.send(DataEvent::FetchError {
                    context: "answer".into(),
                    error: e.to_string(),
                });
            }
        });
    }

    /// Fetch a job's pending `ask_user` questions and hand them to the thread
    /// view (recovers one that fired while the view wasn't focused).
    pub fn fetch_pending_tool_calls(
        &self,
        remote: RemoteOrchestrator,
        job_id: String,
        thread_id: String,
    ) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(calls) = remote.pending_tool_calls(&job_id).await {
                let _ = tx.send(DataEvent::ToolCallsLoaded { thread_id, calls });
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
        conversation_id: Option<String>,
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
            // Per-branch session key (falls back to the room id when absent).
            if conversation_id.is_some() {
                req.conversation_id = conversation_id;
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

    fn jr(status: &str, result: Option<&str>) -> Result<JobResult, RemoteError> {
        Ok(JobResult {
            job_id: "j".into(),
            status: status.into(),
            result: result.map(String::from),
        })
    }

    #[test]
    fn reconcile_decision_covers_the_state_space() {
        use ReconcileAction::*;
        // Completed with content → record it.
        assert_eq!(
            reconcile_decision(&jr("completed", Some("answer"))),
            AppendReply("answer".into())
        );
        // Completed but empty, or failed → terminal, unstick.
        assert_eq!(reconcile_decision(&jr("completed", Some("  "))), ClearStale);
        assert_eq!(reconcile_decision(&jr("completed", None)), ClearStale);
        assert_eq!(reconcile_decision(&jr("failed: boom", None)), ClearStale);
        // Still live → keep waiting.
        assert_eq!(reconcile_decision(&jr("pending", None)), Wait);
        assert_eq!(reconcile_decision(&jr("running: propose", None)), Wait);
        assert_eq!(reconcile_decision(&jr("claimed", None)), Wait);
        // 404 (status entry gone / gc'd) → stale marker, unstick.
        assert_eq!(
            reconcile_decision(&Err(RemoteError::ApiError {
                status: 404,
                body: String::new()
            })),
            ClearStale
        );
        // Transient errors → keep the marker, retry later.
        assert_eq!(
            reconcile_decision(&Err(RemoteError::ApiError {
                status: 503,
                body: String::new()
            })),
            Wait
        );
        assert_eq!(
            reconcile_decision(&Err(RemoteError::ParseError("x".into()))),
            Wait
        );
    }

    #[test]
    fn tui_client_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let _client = TuiClient::new(tx);
    }

    #[tokio::test]
    async fn refresh_thread_jobs_maps_active_thread_jobs() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/deliberations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jobs": [
                    {"job_id": "thread-aaa_111", "status": "running"},
                    {"job_id": "thread-bbb_222", "status": "completed"}, // terminal → dropped
                    {"job_id": "pl_fast_333", "status": "running"}       // not a thread → dropped
                ]
            })))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = TuiClient::new(tx);
        let remote = RemoteOrchestrator::new(&server.uri(), "tok").unwrap();
        client.refresh_thread_jobs(remote);

        match rx.recv().await.unwrap() {
            DataEvent::ThreadJobsLoaded { jobs } => {
                assert_eq!(
                    jobs.get("thread-aaa_111").map(String::as_str),
                    Some("thread-aaa")
                );
                assert_eq!(jobs.len(), 1, "only the active thread job is mapped");
            }
            other => panic!("expected ThreadJobsLoaded, got {other:?}"),
        }
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
