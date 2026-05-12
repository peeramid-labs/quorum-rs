//! NATS JetStream worker that bridges the orchestrator's task subjects to the
//! agent's propose/evaluate methods.
//!
//! This module re-exports the core worker types from `nsed-agent-sdk` and
//! provides convenience constructors that wire up BSL extensions (user tool
//! handler factory, chat capability, status server).

// Re-export all SDK worker types for backward compatibility
pub use crate::workers::{
    JobManifest, NatsNsedWorker, NatsScratchpadStore, UserToolHandlerFactory, WorkerConfig,
    WorkerHook,
};

use crate::agents::ChatCapable;
use crate::agents::{NatsUserToolHandlerFactory, ProposerEvaluatorAgent};
use crate::telemetry::TelemetryEmitterMux;

use anyhow::Result;
use std::sync::Arc;

/// Extension trait that provides a convenience constructor for
/// [`NatsNsedWorker`] pre-configured with BSL extensions.
///
/// This wires up:
/// - `ProposerEvaluatorAgent` as both the agent and the `ChatCapable` implementation
/// - `NatsUserToolHandlerFactory` for user tool support
pub trait NatsNsedWorkerExt {
    /// Creates a new worker from a `ProposerEvaluatorAgent`, automatically
    /// configuring BSL extensions (user tool handler factory + chat).
    /// `telemetry` is the per-agent multi-endpoint mux built by
    /// `connect_endpoints` at the loader; pass `None` to disable
    /// telemetry emission for this worker.
    fn from_agent(
        agent: ProposerEvaluatorAgent,
        config: WorkerConfig,
        telemetry: Option<TelemetryEmitterMux>,
    ) -> impl std::future::Future<Output = Result<NatsNsedWorker>> + Send;
}

impl NatsNsedWorkerExt for NatsNsedWorker {
    async fn from_agent(
        agent: ProposerEvaluatorAgent,
        config: WorkerConfig,
        telemetry: Option<TelemetryEmitterMux>,
    ) -> Result<NatsNsedWorker> {
        let agent_config = agent.config.clone();
        let chat_agent: Arc<dyn ChatCapable> = Arc::new(agent.clone());

        let worker = NatsNsedWorker::new(agent, agent_config, config, telemetry)
            .await?
            .with_user_tool_factory(Arc::new(NatsUserToolHandlerFactory))
            .with_chat(chat_agent);

        Ok(worker)
    }
}

/// Extension trait for enabling the status server with BSL chat support.
pub trait NatsNsedWorkerStatusExt {
    /// Enables the embedded status dashboard on the given port.
    ///
    /// When the `status-server` feature is enabled, this spawns an axum HTTP
    /// server with the chat endpoint connected to the agent's `ChatCapable`
    /// implementation.
    fn with_status_server(self, port: u16) -> Self;
}

impl NatsNsedWorkerStatusExt for NatsNsedWorker {
    fn with_status_server(self, _port: u16) -> Self {
        let worker = self.with_status(_port);

        #[cfg(feature = "status-server")]
        {
            if let Some(status) = worker.status().cloned() {
                let chat = worker.chat_agent().cloned();
                let config = worker.agent_config().clone();
                // Forward the worker's telemetry mux into the status
                // server so the api_error middleware emits events on
                // 4xx/5xx responses. `None` when the worker has no
                // telemetry configured — the layer stays a
                // transparent pass-through in that case.
                let api_error_telemetry = worker.telemetry().cloned().map(|emitter| {
                    std::sync::Arc::new(crate::api_error_middleware::ApiErrorTelemetry::new(
                        emitter,
                        worker.agent_id(),
                    ))
                });
                tokio::spawn(crate::status::server::StatusServer::run(
                    _port,
                    status,
                    chat,
                    config,
                    api_error_telemetry,
                ));
            }
        }

        worker
    }
}
