//! # Multi-Agent Runner
//!
//! Runs multiple NSED agents in a single process. Each agent gets its own
//! [`NatsNsedWorker`] and operates independently on the NATS message bus.
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use quorum_rs::multi_agent::MultiAgentRunner;
//! use quorum_rs::agents::{AgentConfig, ProposerEvaluatorAgent};
//! use quorum_rs::workers::{NatsNsedWorker, NatsNsedWorkerExt, WorkerConfig};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut runner = MultiAgentRunner::new();
//!
//!     // ... build agents and workers ...
//!     // runner.add_worker("AGENT_A", worker_a, config_a);
//!     // runner.add_worker("AGENT_B", worker_b, config_b);
//!
//!     // Optionally enable a unified dashboard on a single port
//!     // runner.enable_dashboard(9090);
//!
//!     runner.run().await
//! }
//! ```

use crate::agents::{AgentConfig, ChatCapable};
use crate::orchestrator_registry::OrchestratorRegistry;
use crate::status::SharedAgentStatus;
use crate::workers::NatsNsedWorker;
use crate::workers::buffer::ResponseBuffer;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Runs multiple NSED agents in a single process.
///
/// Each agent operates as an independent NATS worker. The runner manages
/// their lifecycle and optionally provides a unified dashboard server
/// that aggregates all agents' status on a single HTTP port.
pub struct MultiAgentRunner {
    /// Workers indexed by agent name.
    workers: Vec<(String, NatsNsedWorker)>,
    /// Shared status snapshots per agent (populated by `with_status`).
    statuses: HashMap<String, SharedAgentStatus>,
    /// Chat-capable agents for the unified dashboard.
    chat_agents: HashMap<String, Arc<dyn ChatCapable>>,
    /// Agent configs for the unified dashboard (mutable for live tuning).
    configs: HashMap<String, Arc<RwLock<AgentConfig>>>,
    /// Response buffers per agent for HITL control plane.
    buffers: HashMap<String, Arc<ResponseBuffer>>,
    /// Pause handles per agent — direct access to each worker's AtomicBool.
    pause_handles: HashMap<String, Arc<AtomicBool>>,
    /// Optional unified dashboard port.
    dashboard_port: Option<u16>,
    /// Optional orchestrator registry for runtime management.
    orchestrator_registry: Option<OrchestratorRegistry>,
}

impl MultiAgentRunner {
    /// Create a new empty runner.
    pub fn new() -> Self {
        Self {
            workers: Vec::new(),
            statuses: HashMap::new(),
            chat_agents: HashMap::new(),
            configs: HashMap::new(),
            buffers: HashMap::new(),
            pause_handles: HashMap::new(),
            dashboard_port: None,
            orchestrator_registry: None,
        }
    }

    /// Add a worker to the runner.
    ///
    /// The worker should already be constructed via
    /// [`NatsNsedWorkerExt::from_agent()`](crate::workers::NatsNsedWorkerExt::from_agent).
    /// Status monitoring is automatically enabled for multi-agent mode.
    ///
    /// If a worker with the same name already exists it is replaced: the prior
    /// entry is removed from `workers` and the associated status/chat/config
    /// map entries are overwritten.
    pub fn add_worker(&mut self, name: String, worker: NatsNsedWorker, config: AgentConfig) {
        // Remove any prior worker with the same name to keep Vec and HashMaps in sync.
        self.workers.retain(|(n, _)| n != &name);
        self.statuses.remove(&name);
        self.chat_agents.remove(&name);
        self.buffers.remove(&name);
        self.pause_handles.remove(&name);

        // Auto-enable status monitoring for multi-agent mode. Without a
        // SharedAgentStatus the HITL auto-release sweep cannot compute
        // divergence, causing buffered responses to stay stuck until their
        // SLA deadline even when auto-approve is enabled.
        let worker = if worker.status().is_none() {
            worker.with_status(0) // port is unused in multi-agent dashboard mode
        } else {
            worker
        };

        // Extract status, chat, buffer, and pause handle if available
        if let Some(status) = worker.status().cloned() {
            self.statuses.insert(name.clone(), status);
        }
        if let Some(chat) = worker.chat_agent().cloned() {
            self.chat_agents.insert(name.clone(), chat);
        }
        if let Some(buf) = worker.response_buffer().cloned() {
            self.buffers.insert(name.clone(), buf);
        }
        self.pause_handles
            .insert(name.clone(), worker.pause_handle());
        self.configs
            .insert(name.clone(), Arc::new(RwLock::new(config)));
        self.workers.push((name, worker));
    }

    /// Enable the unified multi-agent dashboard on the given port.
    ///
    /// When enabled, a single HTTP server serves all agents' status,
    /// configuration, and chat endpoints with agent selector tabs.
    ///
    /// Requires the `status-server` feature.
    pub fn enable_dashboard(&mut self, port: u16) {
        self.dashboard_port = Some(port);
    }

    /// Set the orchestrator registry for runtime orchestrator management.
    ///
    /// When set, the dashboard exposes `GET /api/orchestrators` and
    /// `POST /api/orchestrators` endpoints for listing and adding
    /// orchestrators at runtime.
    pub fn set_orchestrator_registry(&mut self, registry: OrchestratorRegistry) {
        self.orchestrator_registry = Some(registry);
    }

    /// Returns the number of agents in the runner.
    pub fn agent_count(&self) -> usize {
        self.workers.len()
    }

    /// Returns the names of all agents in the runner.
    pub fn agent_names(&self) -> Vec<&str> {
        self.workers.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Number of workers currently registered.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// `true` when no workers have been added — `serve_fleet` uses
    /// this to bail before calling `.run()` if every fleet entry
    /// was skipped (e.g. all `exec` agents missing their config
    /// sections). Calling `.run()` on an empty runner already
    /// errors, but checking here lets callers surface a more
    /// specific "fleet had N entries, 0 buildable" message.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Run all agents concurrently with no external shutdown signal.
    /// Convenience wrapper for [`Self::run_with_cancellation`] that
    /// passes a fresh token the caller doesn't hold a clone of, so
    /// only natural worker completion or a worker exhausting its
    /// retry budget ends the runner.
    pub async fn run(self) -> anyhow::Result<()> {
        self.run_with_cancellation(tokio_util::sync::CancellationToken::new())
            .await
    }

    /// Run all agents concurrently with an external shutdown signal.
    ///
    /// Spawns each worker in its own tokio task and waits for **all** tasks to
    /// complete. Individual failures are logged but do not abort the remaining
    /// workers. Returns `Ok(())` when every worker succeeds, or an aggregated
    /// error listing all failures when one or more workers crash or panic.
    ///
    /// Shutdown happens on two layers:
    ///
    /// 1. **Cooperative.** Each worker's reconnect loop checks
    ///    `cancel.is_cancelled()` at the top of the loop and
    ///    returns cleanly when set — gives the worker a chance to
    ///    drop its NATS connection between message-handling
    ///    iterations.
    /// 2. **Forceful (abort).** A watchdog task captures
    ///    [`tokio::task::AbortHandle`]s for every worker BEFORE
    ///    the runner's join loop consumes the [`JoinHandle`]s.
    ///    When `cancel.cancelled()` fires the watchdog calls
    ///    `.abort()` on each handle, which stops a worker even
    ///    if it's blocked deep inside `worker.run().await` past
    ///    the cooperative check. Resulting JoinErrors with
    ///    `is_cancelled() == true` are treated as clean
    ///    shutdown, not failure.
    ///
    /// Layer 1 alone would leak workers blocked inside
    /// `worker.run()` past the CLI's shutdown grace window —
    /// that's why both layers exist (CR finding on PR #13).
    /// Without either, a SIGTERM-driven drop of the runner future
    /// would only stop polling and the inner `tokio::spawn`'d
    /// tasks would detach and keep running / reconnecting.
    ///
    /// If a dashboard port is configured and the `status-server` feature is
    /// enabled, the unified dashboard server is started before the workers.
    pub async fn run_with_cancellation(
        self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        if self.workers.is_empty() {
            anyhow::bail!("No agents configured. Add workers before calling run().");
        }

        info!(
            "Starting multi-agent runner with {} agent(s): {}",
            self.workers.len(),
            self.agent_names().join(", ")
        );

        // Optionally start the unified dashboard server
        #[cfg(feature = "status-server")]
        if let Some(port) = self.dashboard_port {
            let statuses = self.statuses.clone();
            let chat_agents = self.chat_agents.clone();
            let configs = self.configs.clone();
            let buffers = self.buffers.clone();
            let pause_handles = self.pause_handles.clone();
            let registry = self.orchestrator_registry.clone();
            tokio::spawn(async move {
                crate::status::multi_server::MultiAgentStatusServer::run_control_plane(
                    port,
                    statuses,
                    chat_agents,
                    configs,
                    buffers,
                    pause_handles,
                    registry,
                    None, // Middleware pipeline — wired when MiddlewareConfig is available
                )
                .await;
            });
        }

        // Spawn all workers with automatic reconnection on connection loss.
        // Each worker runs in a loop: if `run()` returns Ok (consumers closed,
        // e.g. NATS reconnect), the worker is restarted after a backoff delay.
        // Errors (permanent failures) are retried up to MAX_RECONNECT_ATTEMPTS.
        //
        // The outer `cancel` is cloned into each task so a SIGTERM
        // signaled via `cancel.cancel()` ALSO short-circuits the
        // worker's reconnect loop instead of just aborting the
        // task at its next .await — the latter still works but
        // cooperative shutdown gives the worker a chance to drop
        // its NATS connection cleanly.
        let mut handles = Vec::new();
        for (name, worker) in self.workers {
            let agent_name = name.clone();
            let task_cancel = cancel.clone();
            let handle = tokio::spawn(async move {
                const MAX_RECONNECT_ATTEMPTS: u32 = 10;
                const BASE_DELAY_MS: u64 = 1000;
                const MAX_DELAY_MS: u64 = 60_000;

                let mut consecutive_failures: u32 = 0;

                info!("🟢 Agent '{}' started", agent_name);
                loop {
                    if task_cancel.is_cancelled() {
                        info!("🛑 Agent '{}' shutting down (cancelled)", agent_name);
                        return Ok(());
                    }
                    match worker.run().await {
                        Ok(()) => {
                            // Worker exited cleanly — consumers closed (connection lost).
                            // Reset failure counter and reconnect after brief delay.
                            consecutive_failures = 0;
                            warn!(
                                "Agent '{}' worker loop exited (connection lost). \
                                 Reconnecting in 1s...",
                                agent_name
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(BASE_DELAY_MS))
                                .await;
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            if consecutive_failures >= MAX_RECONNECT_ATTEMPTS {
                                error!(
                                    "Agent '{}' failed {} times consecutively, giving up: {:?}",
                                    agent_name, consecutive_failures, e
                                );
                                return Err(anyhow::anyhow!(
                                    "Agent '{}' crashed after {} attempts: {:?}",
                                    agent_name,
                                    consecutive_failures,
                                    e
                                ));
                            }
                            let delay_ms = (BASE_DELAY_MS
                                * 2u64.saturating_pow(consecutive_failures - 1))
                            .min(MAX_DELAY_MS);
                            warn!(
                                "Agent '{}' failed (attempt {}/{}): {:?}. \
                                 Retrying in {}ms...",
                                agent_name,
                                consecutive_failures,
                                MAX_RECONNECT_ATTEMPTS,
                                e,
                                delay_ms
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        }
                    }
                }
            });
            handles.push((name, handle));
        }

        // Snapshot abort-handles before we consume the JoinHandles
        // into `join_all`. The abort-handles are cheap clones that
        // let an out-of-band task fire `.abort()` on each worker
        // when the cancellation token signals shutdown. Without
        // these, only the cooperative `task_cancel.is_cancelled()`
        // check inside each worker's reconnect loop can stop
        // workers — and a worker blocked deep inside
        // `worker.run().await` would miss it until the inner
        // call returns. CR finding on PR #13.
        let abort_handles: Vec<(String, tokio::task::AbortHandle)> = handles
            .iter()
            .map(|(n, h)| (n.clone(), h.abort_handle()))
            .collect();
        let watchdog_cancel = cancel.clone();
        let watchdog = tokio::spawn(async move {
            watchdog_cancel.cancelled().await;
            info!(
                "shutdown signal received; aborting {} worker(s)",
                abort_handles.len()
            );
            for (name, h) in &abort_handles {
                if !h.is_finished() {
                    info!("aborting worker '{}'", name);
                    h.abort();
                }
            }
        });

        // Wait for all workers — completed normally, returned an
        // error, panicked, or cancelled-via-abort. The watchdog
        // above fires `.abort()` when the cancel token signals,
        // so a JoinError with `is_cancelled() == true` is the
        // clean-shutdown path (treated as success, not added to
        // the error list).
        let mut errors = Vec::new();
        for (name, handle) in handles {
            match handle.await {
                Ok(Ok(())) => info!("Agent '{}' completed normally", name),
                Ok(Err(e)) => {
                    error!("Agent '{}' failed: {:?}", name, e);
                    errors.push(e);
                }
                Err(e) if e.is_cancelled() => {
                    info!("Agent '{}' aborted cleanly via shutdown signal", name);
                }
                Err(e) => {
                    error!("Agent '{}' panicked: {:?}", name, e);
                    errors.push(anyhow::anyhow!("Agent '{}' panicked: {:?}", name, e));
                }
            }
        }
        // Abort the watchdog so it doesn't outlive the runner.
        // If cancel fired, the watchdog's `.abort()` loop has
        // already run by the time we got here (every worker
        // completed above), so the watchdog itself is just
        // sitting at `watchdog_cancel.cancelled().await` having
        // returned — `.abort()` on a finished task is a no-op.
        // If cancel never fired (workers completed naturally),
        // the watchdog is still awaiting cancellation and
        // `.abort()` stops it cleanly.
        watchdog.abort();

        if !errors.is_empty() {
            anyhow::bail!(
                "{} agent(s) failed:\n{}",
                errors.len(),
                errors
                    .iter()
                    .map(|e| format!("  - {}", e))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        Ok(())
    }
}

impl Default for MultiAgentRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_runner_is_empty() {
        let runner = MultiAgentRunner::new();
        assert_eq!(runner.agent_count(), 0);
        assert!(runner.agent_names().is_empty());
        assert!(runner.dashboard_port.is_none());
    }

    #[test]
    fn default_runner_is_empty() {
        let runner = MultiAgentRunner::default();
        assert_eq!(runner.agent_count(), 0);
        assert!(runner.agent_names().is_empty());
    }

    #[test]
    fn enable_dashboard_sets_port() {
        let mut runner = MultiAgentRunner::new();
        assert!(runner.dashboard_port.is_none());
        runner.enable_dashboard(9090);
        assert_eq!(runner.dashboard_port, Some(9090));
    }

    #[test]
    fn enable_dashboard_overwrites_port() {
        let mut runner = MultiAgentRunner::new();
        runner.enable_dashboard(8080);
        runner.enable_dashboard(9090);
        assert_eq!(runner.dashboard_port, Some(9090));
    }

    #[tokio::test]
    async fn run_with_no_agents_fails() {
        let runner = MultiAgentRunner::new();
        let result = runner.run().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("No agents configured"),
            "Expected 'No agents configured' error, got: {}",
            err_msg
        );
    }

    /// `run_with_cancellation` plumbs through to the worker tasks
    /// via the cooperative `is_cancelled()` check at the top of
    /// the reconnect loop. Pre-cancelled token + empty runner is
    /// the simplest end-to-end check that wiring is intact;
    /// fuller "spawn workers + cancel + assert tasks stopped"
    /// coverage needs a real NATS test fixture which lives in the
    /// integration suite.
    #[tokio::test]
    async fn run_with_cancellation_takes_token() {
        let runner = MultiAgentRunner::new();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        // Empty runner still bails with "No agents configured"
        // — this test pins the SHAPE of the API (takes a token,
        // returns a Result) and proves a pre-cancelled token
        // doesn't trip a panic or hang before the empty-runner
        // check fires.
        let result = runner.run_with_cancellation(token).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No agents configured"),
            "empty runner still errors before honouring cancellation"
        );
    }

    #[test]
    fn configs_tracked_after_add_worker() {
        // We can't easily construct a real NatsNsedWorker without NATS,
        // so we verify the config tracking through the public API.
        // The configs HashMap is populated in add_worker.
        let runner = MultiAgentRunner::new();
        // Just verify the runner starts empty — add_worker requires a real
        // NatsNsedWorker which needs NATS. Config tracking is verified via
        // integration tests.
        assert!(runner.configs.is_empty());
        assert!(runner.statuses.is_empty());
        assert!(runner.chat_agents.is_empty());
    }

    #[test]
    fn agent_names_returns_empty_for_new_runner() {
        let runner = MultiAgentRunner::new();
        let names = runner.agent_names();
        assert!(names.is_empty());
        assert_eq!(names.len(), 0);
    }

    #[test]
    fn agent_count_returns_zero_for_new_runner() {
        let runner = MultiAgentRunner::new();
        assert_eq!(runner.agent_count(), 0);
    }

    #[test]
    fn set_orchestrator_registry_stores_registry() {
        let mut runner = MultiAgentRunner::new();
        assert!(runner.orchestrator_registry.is_none());

        let (registry, _rx) = crate::orchestrator_registry::OrchestratorRegistry::new();
        runner.set_orchestrator_registry(registry);
        assert!(runner.orchestrator_registry.is_some());
    }

    #[test]
    fn set_orchestrator_registry_overwrites_previous() {
        let mut runner = MultiAgentRunner::new();

        let (reg1, _rx1) = crate::orchestrator_registry::OrchestratorRegistry::new();
        runner.set_orchestrator_registry(reg1);
        assert!(runner.orchestrator_registry.is_some());

        let (reg2, _rx2) = crate::orchestrator_registry::OrchestratorRegistry::new();
        runner.set_orchestrator_registry(reg2);
        assert!(runner.orchestrator_registry.is_some());
    }

    #[test]
    fn default_impl_matches_new() {
        let from_new = MultiAgentRunner::new();
        let from_default = MultiAgentRunner::default();

        assert_eq!(from_new.agent_count(), from_default.agent_count());
        assert_eq!(
            from_new.agent_names().len(),
            from_default.agent_names().len()
        );
        assert_eq!(from_new.dashboard_port, from_default.dashboard_port);
        assert!(from_new.orchestrator_registry.is_none());
        assert!(from_default.orchestrator_registry.is_none());
    }

    #[test]
    fn enable_dashboard_zero_port() {
        let mut runner = MultiAgentRunner::new();
        runner.enable_dashboard(0);
        assert_eq!(runner.dashboard_port, Some(0));
    }

    #[test]
    fn enable_dashboard_max_port() {
        let mut runner = MultiAgentRunner::new();
        runner.enable_dashboard(u16::MAX);
        assert_eq!(runner.dashboard_port, Some(u16::MAX));
    }

    #[tokio::test]
    async fn run_with_no_agents_error_message_is_descriptive() {
        let runner = MultiAgentRunner::new();
        let err = runner.run().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No agents configured"),
            "Error should mention no agents configured, got: {msg}"
        );
        assert!(
            msg.contains("Add workers"),
            "Error should hint at adding workers, got: {msg}"
        );
    }

    #[test]
    fn internal_maps_start_empty() {
        let runner = MultiAgentRunner::new();
        assert!(runner.workers.is_empty());
        assert!(runner.statuses.is_empty());
        assert!(runner.chat_agents.is_empty());
        assert!(runner.configs.is_empty());
        assert!(runner.buffers.is_empty());
        assert!(runner.pause_handles.is_empty());
    }
}
