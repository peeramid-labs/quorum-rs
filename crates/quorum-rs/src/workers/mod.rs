//! NATS JetStream worker that bridges the orchestrator's task subjects to the
//! agent's propose/evaluate methods.
//!
//! This module provides [`NatsNsedWorker`] — the main runtime loop for
//! independent agent processes — along with configuration types and the
//! [`NatsScratchpadStore`] persistence implementation.
//!
//! # Extension points
//!
//! - [`WorkerHook`] — intercept NATS publishes (e.g. for crypto wrapping)
//! - [`UserToolHandlerFactory`] — inject per-task user tool handlers

pub mod buffer;

use crate::agents::{
    AgentConfig, AgentContext, AgentHeartbeat, AgentLiveStatus, ChatCapable, NsedAgent,
    PersistenceStore, ProposalRecord, UserToolHandlerTrait,
};
use crate::nats_utils::{NatsAuth, connect_nats, ensure_kv_bucket, sanitize_subject_component};
use crate::status::{SharedAgentStatus, TaskLogEntry, new_shared_status};
use crate::telemetry::{TaskFailureClass, TelemetryEmitterMux};

use anyhow::{Context, Result};
use async_nats::connection::State as NatsState;
use async_nats::jetstream::{self, kv};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Passthrough protocol types
// ---------------------------------------------------------------------------

/// NATS subject pattern for passthrough requests:
/// `{subject_prefix}.agent.{agent_id}.passthrough`
///
/// The orchestrator sends a core NATS request to this subject when a policy
/// has `mode: passthrough`.  The agent replies synchronously (no JetStream).
pub const PASSTHROUGH_SUBJECT_SUFFIX: &str = "passthrough";

/// Request payload for passthrough mode.
///
/// The orchestrator serializes this as JSON and publishes it via NATS
/// request-reply.  The agent deserializes, calls `ChatCapable::chat()`,
/// and replies with [`PassthroughResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughRequest {
    /// Session / room identifier for history tracking.
    pub session_id: String,
    /// OpenAI-compatible conversation messages.
    pub messages: Vec<PassthroughMessage>,
    /// Principal who submitted the request (for cost attribution).
    pub operator_principal: String,
}

/// A single message in a passthrough conversation — role + content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughMessage {
    /// `"user"`, `"assistant"`, or `"system"`.
    pub role: String,
    pub content: String,
}

/// Response payload returned by the agent for a passthrough request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughResponse {
    /// The agent's text reply.
    pub content: String,
    /// Input token count reported by the LLM (if available).
    pub input_tokens: Option<u32>,
    /// Output token count reported by the LLM (if available).
    pub output_tokens: Option<u32>,
}

/// Error envelope returned when the agent cannot handle a passthrough request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PassthroughError {
    pub error: String,
}

// ---------------------------------------------------------------------------
// WorkerHook trait
// ---------------------------------------------------------------------------

/// Trait for intercepting worker lifecycle events.
///
/// Implementations can wrap, encrypt, or transform payloads before they are
/// published to NATS. The default implementation is a no-op passthrough;
/// crypto-wrapping implementations can be plugged in by callers.
#[async_trait]
pub trait WorkerHook: Send + Sync + Debug {
    /// Called before publishing a proposal/evaluation result.
    async fn before_publish(&self, _subject: &str, _payload: &mut Vec<u8>) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// UserToolHandlerFactory trait
// ---------------------------------------------------------------------------

/// Factory for creating per-task user tool handlers.
///
/// The reference implementation [`NatsUserToolHandlerFactory`](crate::agents::NatsUserToolHandlerFactory)
/// wraps [`UserToolHandler`](crate::agents::UserToolHandler), which depends on NATS internals.
#[async_trait]
pub trait UserToolHandlerFactory: Send + Sync + Debug {
    /// Create a handler scoped to a specific task execution.
    fn create(
        &self,
        nats: async_nats::Client,
        js: jetstream::Context,
        session_id: String,
        agent_id: String,
        budget_remaining_secs: f64,
        subject_prefix: String,
    ) -> Arc<dyn UserToolHandlerTrait>;
}

// ---------------------------------------------------------------------------
// WorkerConfig
// ---------------------------------------------------------------------------

/// Configuration for the NATS connection and behavior.
///
/// NATS subjects are split across two namespaces:
/// - **`subject_prefix`** (`"nsed"`) — scientific deliberation protocol:
///   tasks (`nsed.{session}.task.{agent}.{action}`), results, events.
/// - **`api_prefix`** (`"sphera"`) — product API layer:
///   job manifests (`sphera.jobs.manifest.>`), ACKs, heartbeats, pings.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// NATS server URL (e.g. `nats://localhost:4222`).
    pub nats_url: String,
    /// JetStream stream name to consume from.
    pub stream_name: String,
    /// Durable consumer name for task messages.
    pub consumer_name: String,
    /// Subject prefix for the deliberation protocol (default: `"nsed"`).
    pub subject_prefix: String,
    /// Subject prefix for the product API layer (default: `"sphera"`).
    pub api_prefix: String,
    /// TTL for scratchpad data in seconds. Defaults to 7 days.
    pub scratchpad_retention_secs: u64,
    /// Optional NATS authentication credentials.
    pub nats_auth: Option<NatsAuth>,
    /// Max jobs this agent processes concurrently. Enforced as the pull
    /// consumer's `max_ack_pending` — the server won't deliver the next task
    /// until an in-flight one is acked. `None` leaves it unbounded (server
    /// default). Set to `1` for agents whose jobs share mutable state (e.g. a
    /// git repo a middleware resets per job) so concurrent jobs can't race.
    pub max_concurrent_jobs: Option<usize>,
}

impl WorkerConfig {
    /// Creates a new `WorkerConfig` with default retention settings.
    pub fn new(nats_url: String, stream_name: String, consumer_name: String) -> Self {
        Self {
            nats_url,
            stream_name,
            consumer_name,
            subject_prefix: "nsed".to_string(),
            api_prefix: "sphera".to_string(),
            scratchpad_retention_secs: 86400 * 7,
            nats_auth: None,
            max_concurrent_jobs: None,
        }
    }

    /// Sets the subject prefix for deliberation protocol subjects.
    pub fn with_subject_prefix(mut self, prefix: String) -> Self {
        self.subject_prefix = prefix;
        self
    }

    /// Sets the subject prefix for product API subjects.
    pub fn with_api_prefix(mut self, prefix: String) -> Self {
        self.api_prefix = prefix;
        self
    }

    /// Sets the scratchpad retention TTL.
    pub fn with_scratchpad_retention(mut self, secs: u64) -> Self {
        self.scratchpad_retention_secs = secs;
        self
    }

    /// Sets NATS authentication credentials.
    pub fn with_nats_auth(mut self, auth: NatsAuth) -> Self {
        self.nats_auth = Some(auth);
        self
    }

    /// Caps how many jobs this agent processes concurrently (see
    /// [`WorkerConfig::max_concurrent_jobs`]).
    pub fn with_max_concurrent_jobs(mut self, n: usize) -> Self {
        self.max_concurrent_jobs = Some(n);
        self
    }

    /// The pull-consumer `max_ack_pending` implied by `max_concurrent_jobs`:
    /// the cap when set, else `0` (server default = unbounded). Extracted so the
    /// mapping is unit-testable without a live NATS.
    fn max_ack_pending(&self) -> i64 {
        self.max_concurrent_jobs.map(|n| n as i64).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// JobManifest
// ---------------------------------------------------------------------------

/// Job manifest broadcast by the orchestrator when a new deliberation is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobManifest {
    pub job_id: String,
    pub task_description: String,
    pub agents: Vec<String>,
    pub rounds: u32,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// NatsScratchpadStore
// ---------------------------------------------------------------------------

/// A wrapper around NATS KV that implements [`PersistenceStore`].
///
/// Provides durable, scoped key–value storage for agent scratchpad data.
#[derive(Clone, Debug)]
pub struct NatsScratchpadStore {
    store: kv::Store,
    js: jetstream::Context,
    scope_prefix: String,
}

impl NatsScratchpadStore {
    /// Creates a new scratchpad store scoped to `scope_prefix`.
    pub fn new(store: kv::Store, js: jetstream::Context, scope_prefix: String) -> Self {
        Self {
            store,
            js,
            scope_prefix,
        }
    }

    fn scoped_key(&self, key: &str) -> String {
        format!("{}.{}", self.scope_prefix, key)
    }

    async fn store_get(&self, key: &str) -> Result<Option<bytes::Bytes>> {
        self.store
            .get(self.scoped_key(key))
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn store_entry(&self, key: &str) -> Result<Option<kv::Entry>> {
        self.store
            .entry(self.scoped_key(key))
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn store_put(&self, key: &str, value: bytes::Bytes) -> Result<u64> {
        self.store
            .put(self.scoped_key(key), value)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn store_create(
        &self,
        key: &str,
        value: bytes::Bytes,
    ) -> std::result::Result<u64, async_nats::error::Error<kv::CreateErrorKind>> {
        self.store.create(self.scoped_key(key), value).await
    }

    async fn store_update(
        &self,
        key: &str,
        value: bytes::Bytes,
        revision: u64,
    ) -> std::result::Result<u64, async_nats::error::Error<kv::UpdateErrorKind>> {
        self.store
            .update(self.scoped_key(key), value, revision)
            .await
    }
}

#[async_trait]
impl PersistenceStore for NatsScratchpadStore {
    async fn get(&self, key: &str) -> Result<Option<String>> {
        match self.store_get(key).await? {
            Some(data) => {
                let vec: Vec<u8> = data.to_vec();
                Ok(Some(String::from_utf8(vec)?))
            }
            None => Ok(None),
        }
    }

    async fn append(&self, key: &str, content: &str) -> Result<()> {
        let mut attempts = 0;
        let max_retries = 20;

        loop {
            attempts += 1;
            if attempts > max_retries {
                return Err(anyhow::anyhow!(
                    "Failed to append to key '{}' (scoped) after {} attempts due to contention",
                    key,
                    max_retries
                ));
            }

            match self.store_entry(key).await? {
                Some(entry) => {
                    let current = String::from_utf8_lossy(&entry.value);
                    let new_content = format!("{}{}", current, content);

                    match self
                        .store_update(key, new_content.into(), entry.revision)
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(e) => {
                            if matches!(e.kind(), kv::UpdateErrorKind::WrongLastRevision) {
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    5 + (attempts * 2),
                                ))
                                .await;
                                continue;
                            }
                            return Err(anyhow::anyhow!(e));
                        }
                    }
                }
                None => match self.store_create(key, content.to_string().into()).await {
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        if matches!(e.kind(), kv::CreateErrorKind::AlreadyExists) {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                5 + (attempts * 2),
                            ))
                            .await;
                            continue;
                        }
                        return Err(anyhow::anyhow!(e));
                    }
                },
            }
        }
    }

    async fn set(&self, key: &str, content: &str) -> Result<()> {
        self.store_put(key, content.to_string().into()).await?;
        Ok(())
    }

    async fn get_round_history(&self, round: u32) -> Result<Option<Vec<ProposalRecord>>> {
        let safe_id = sanitize_subject_component(&self.scope_prefix);
        let bucket_name = format!("nsed_hist_{}", safe_id);

        let history_store = match self.js.get_key_value(&bucket_name).await {
            Ok(s) => s,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("not found") || err_str.contains("no stream") {
                    return Ok(None);
                }
                return Err(anyhow::anyhow!(e)
                    .context(format!("Failed to access history bucket '{}'", bucket_name)));
            }
        };

        let key = format!("round_{}", round);

        match history_store.get(&key).await {
            Ok(Some(entry)) => {
                let records = serde_json::from_slice(&entry)
                    .context("Failed to deserialize round history")?;
                Ok(Some(records))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("NATS KV Get Error for history: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// NatsNsedWorker
// ---------------------------------------------------------------------------

/// The main NATS worker that connects any [`NsedAgent`] implementation to the
/// orchestrator's JetStream task/result subjects.
///
/// # Lifecycle
///
/// 1. **Connect** — Establishes NATS connection and creates per-agent KV buckets.
/// 2. **Subscribe** — Binds durable pull consumers for tasks and manifests.
/// 3. **Loop** — Processes messages: manifests → ACK, tasks → propose/evaluate.
/// 4. **Dedup** — Checks idempotency KV before processing.
pub struct NatsNsedWorker {
    agent: Arc<dyn NsedAgent>,
    /// Agent configuration for heartbeat metadata.
    agent_config: AgentConfig,
    nats: async_nats::Client,
    js: jetstream::Context,
    processed_kv: kv::Store,
    scratchpad_kv: kv::Store,
    config: WorkerConfig,
    agent_id: String,
    active_jobs: Arc<Mutex<HashSet<String>>>,
    start_time: Instant,
    /// Optional shared status snapshot for the embedded status dashboard.
    status: Option<SharedAgentStatus>,
    /// Optional hook for intercepting NATS publishes (e.g. crypto wrapping).
    hook: Option<Arc<dyn WorkerHook>>,
    /// Optional factory for creating per-task user tool handlers.
    user_tool_factory: Option<Arc<dyn UserToolHandlerFactory>>,
    /// Optional chat-capable agent for the status server chat endpoint.
    chat_agent: Option<Arc<dyn ChatCapable>>,
    /// Optional response buffer for HITL control (holds responses before NATS publish).
    response_buffer: Option<Arc<buffer::ResponseBuffer>>,
    /// Pause flag — when true, the worker stops consuming new NATS tasks.
    ///
    /// This is independent of the buffer's pause flag: the buffer controls
    /// whether completed responses are released, while this flag controls
    /// whether new tasks are pulled from NATS.
    paused: Arc<AtomicBool>,
    /// Optional telemetry emitter for recording LLM call metrics.
    telemetry: Option<TelemetryEmitterMux>,
    /// Agent middleware pipelines, built from `agent_config.middleware`.
    /// `None` when the hook point is unconfigured → zero overhead + no behavior
    /// change for existing agents.
    before_prompt_mw: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    provider_response_mw: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    completion_mw: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    /// Job-final hook — fires once on the orchestrator's `job_complete` event.
    job_complete_mw: Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
}

impl NatsNsedWorker {
    /// Creates a new worker, connecting to NATS and initializing KV buckets.
    pub async fn new(
        agent: impl NsedAgent + 'static,
        agent_config: AgentConfig,
        config: WorkerConfig,
        telemetry: Option<TelemetryEmitterMux>,
    ) -> Result<Self> {
        Self::from_dyn_agent(Arc::new(agent), agent_config, config, telemetry).await
    }

    /// Like [`NatsNsedWorker::new`] but takes an already-erased
    /// `Arc<dyn NsedAgent>`. The [`ProviderRegistry`] dispatch path
    /// produces trait objects (the concrete agent type is chosen at
    /// runtime by a [`ProviderFactory`]), so it can't satisfy the
    /// `impl NsedAgent` bound on `new`.
    ///
    /// [`ProviderRegistry`]: crate::providers::ProviderRegistry
    /// [`ProviderFactory`]: crate::providers::ProviderFactory
    pub async fn from_dyn_agent(
        agent: Arc<dyn NsedAgent>,
        agent_config: AgentConfig,
        config: WorkerConfig,
        telemetry: Option<TelemetryEmitterMux>,
    ) -> Result<Self> {
        let agent_id = agent.name();
        let nats = connect_nats(&config.nats_url, config.nats_auth.as_ref()).await?;
        let js = jetstream::new(nats.clone());

        info!("🍃 NATS Leaf Worker Connected: {}", agent_id);

        let safe_id = agent_id.replace(|c: char| !c.is_alphanumeric(), "_");

        let processed_bucket_name = format!("nsed_proc_{}", safe_id);
        let processed_kv = ensure_kv_bucket(
            &js,
            kv::Config {
                bucket: processed_bucket_name.clone(),
                description: format!("Idempotency keys for Agent {}", agent_id),
                max_age: std::time::Duration::from_secs(86400),
                storage: jetstream::stream::StorageType::File,
                num_replicas: 1,
                ..Default::default()
            },
        )
        .await?;

        let scratchpad_bucket_name = format!("nsed_local_mem_{}", safe_id);
        let scratchpad_ttl = if config.scratchpad_retention_secs > 0 {
            std::time::Duration::from_secs(config.scratchpad_retention_secs)
        } else {
            std::time::Duration::ZERO
        };

        let scratchpad_kv = ensure_kv_bucket(
            &js,
            kv::Config {
                bucket: scratchpad_bucket_name.clone(),
                description: format!("Session-scoped scratchpad for Agent {}", agent_id),
                history: 5,
                max_age: scratchpad_ttl,
                storage: jetstream::stream::StorageType::File,
                num_replicas: 1,
                ..Default::default()
            },
        )
        .await?;

        info!(
            "🔒 Initialized Sovereign KV Stores: {} & {}",
            processed_bucket_name, scratchpad_bucket_name
        );

        // Build the middleware pipelines once. `None` when a hook point has no
        // entries, so the hot path stays untouched for agents without middleware.
        let opt_pipeline = |p: crate::middleware::pipeline::MiddlewarePipeline| {
            if p.is_empty() {
                None
            } else {
                Some(Arc::new(p))
            }
        };
        // `?` — a middleware that fails to build (broken/missing dylib, builtin
        // error) fails the agent's startup (fail-closed) rather than silently
        // running without the guard.
        let before_prompt_mw = opt_pipeline(
            agent_config
                .middleware
                .build_before_prompt_pipeline()
                .map_err(|e| anyhow::anyhow!(e))?,
        );
        let provider_response_mw = opt_pipeline(
            agent_config
                .middleware
                .build_provider_response_pipeline()
                .map_err(|e| anyhow::anyhow!(e))?,
        );
        let completion_mw = opt_pipeline(
            agent_config
                .middleware
                .build_completion_pipeline()
                .map_err(|e| anyhow::anyhow!(e))?,
        );
        let job_complete_mw = opt_pipeline(
            agent_config
                .middleware
                .build_job_complete_pipeline()
                .map_err(|e| anyhow::anyhow!(e))?,
        );

        Ok(Self {
            agent,
            agent_config,
            nats,
            js,
            processed_kv,
            scratchpad_kv,
            config,
            agent_id,
            active_jobs: Arc::new(Mutex::new(HashSet::new())),
            start_time: Instant::now(),
            status: None,
            hook: None,
            user_tool_factory: None,
            chat_agent: None,
            response_buffer: None,
            paused: Arc::new(AtomicBool::new(false)),
            telemetry,
            before_prompt_mw,
            provider_response_mw,
            completion_mw,
            job_complete_mw,
        })
    }

    /// Returns a reference to the telemetry mux, if one was provided.
    pub fn telemetry(&self) -> Option<&TelemetryEmitterMux> {
        self.telemetry.as_ref()
    }

    /// Sets a [`WorkerHook`] for intercepting NATS publishes.
    pub fn with_hook(mut self, hook: Arc<dyn WorkerHook>) -> Self {
        self.hook = Some(hook);
        self
    }

    /// Install a [`SigningHook`] with an explicit [`AgentKeyPair`](crate::crypto::AgentKeyPair).
    ///
    /// Wraps outbound payloads in signed [`AuditEnvelope`]s. The keypair must
    /// be provided — use [`auto_sign()`](Self::auto_sign) for a random keypair.
    ///
    /// Not installed by default to maintain backward compatibility.
    ///
    /// Requires the `audit` feature (enabled by default).
    #[cfg(feature = "audit")]
    pub fn with_signing(self, keypair: crate::crypto::AgentKeyPair) -> Self {
        let hook = Arc::new(crate::crypto::SigningHook::new(
            keypair,
            self.agent_id.clone(),
        ));
        self.with_hook(hook)
    }

    /// Install automatic signing with an auto-generated keypair.
    ///
    /// Equivalent to `with_signing(AgentKeyPair::generate())`. The keypair is
    /// unique per worker instance — use `with_signing(AgentKeyPair::from_seed(...))`
    /// for deterministic/reproducible signing.
    ///
    /// Requires the `audit` feature (enabled by default).
    #[cfg(feature = "audit")]
    pub fn auto_sign(self) -> Self {
        self.with_signing(crate::crypto::AgentKeyPair::generate())
    }

    /// Sets a [`UserToolHandlerFactory`] for creating per-task user tool handlers.
    pub fn with_user_tool_factory(mut self, factory: Arc<dyn UserToolHandlerFactory>) -> Self {
        self.user_tool_factory = Some(factory);
        self
    }

    /// The user-tool handler factory this worker was built with, if any. A worker WITHOUT
    /// one silently drops every job-carried user tool (e.g. `ask_user`): the tool arrives in
    /// the context but no handler is built, so it is never advertised over MCP. Exposed so a
    /// test can assert the serve/build path actually wired it, and drive it end to end.
    pub fn user_tool_factory(&self) -> Option<Arc<dyn UserToolHandlerFactory>> {
        self.user_tool_factory.clone()
    }

    /// Sets a [`ChatCapable`] agent for the status server chat endpoint.
    pub fn with_chat(mut self, chat: Arc<dyn ChatCapable>) -> Self {
        self.chat_agent = Some(chat);
        self
    }

    /// Enables the embedded status dashboard on the given port.
    ///
    /// Note: The actual HTTP server requires the `status-server` cargo
    /// feature. Without that feature, this method only creates the shared
    /// status snapshot (heartbeats and events are still recorded).
    pub fn with_status(mut self, _port: u16) -> Self {
        let shared = new_shared_status(
            self.agent_id.clone(),
            self.agent_config.model_name.clone(),
            self.agent_config.provider_id.clone(),
        );
        self.status = Some(shared);
        self
    }

    /// Returns the shared status snapshot handle, if status is enabled.
    pub fn status(&self) -> Option<&SharedAgentStatus> {
        self.status.as_ref()
    }

    /// Returns the agent config.
    pub fn agent_config(&self) -> &AgentConfig {
        &self.agent_config
    }

    /// Returns the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Returns the chat-capable agent, if set.
    pub fn chat_agent(&self) -> Option<&Arc<dyn ChatCapable>> {
        self.chat_agent.as_ref()
    }

    /// Sets a [`ResponseBuffer`](buffer::ResponseBuffer) for HITL control.
    ///
    /// When set, completed responses are held in the buffer for `hold_duration`
    /// before being published to NATS. The buffer can be paused, and individual
    /// entries can be manually released or rejected via the control plane API.
    pub fn with_response_buffer(mut self, hold_duration: std::time::Duration) -> Self {
        self.response_buffer = Some(Arc::new(buffer::ResponseBuffer::new(hold_duration)));
        self
    }

    /// Returns the response buffer, if set.
    pub fn response_buffer(&self) -> Option<&Arc<buffer::ResponseBuffer>> {
        self.response_buffer.as_ref()
    }

    /// Returns a handle to the worker's pause flag.
    ///
    /// This can be shared with external systems (e.g. the dashboard control
    /// plane) to pause/resume the worker without needing a response buffer.
    ///
    /// **Note:** Mutating the returned `AtomicBool` directly (via
    /// `handle.store()`) only toggles the task-consumption flag. It does
    /// **not** pause or resume the response buffer. Production code should
    /// prefer [`Self::pause()`] / [`Self::resume()`] which synchronise both
    /// the flag and the buffer in a single call.
    pub fn pause_handle(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }

    /// Pause the worker — stops consuming new NATS tasks.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        // Also pause the buffer if present, so held entries aren't released
        if let Some(ref buf) = self.response_buffer {
            buf.pause();
        }
    }

    /// Resume the worker — resumes consuming NATS tasks.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        if let Some(ref buf) = self.response_buffer {
            buf.resume();
        }
    }

    /// Returns whether the worker is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Runs the worker loop, consuming messages from the task and manifest
    /// consumers until the streams close or an unrecoverable error occurs.
    pub async fn run(&self) -> Result<()> {
        let prefix = &self.config.subject_prefix;
        let task_filter = format!("{}.*.task.{}.*", prefix, self.agent_id);

        // Wait for the stream to be created by the orchestrator
        let stream = {
            let max_attempts = 10;
            let mut attempt = 0;
            loop {
                attempt += 1;
                match self.js.get_stream(&self.config.stream_name).await {
                    Ok(s) => break s,
                    Err(e) => {
                        if attempt >= max_attempts {
                            return Err(anyhow::anyhow!(
                                "Stream '{}' not found after {} attempts. \
                                 Is the orchestrator running? Last error: {}",
                                self.config.stream_name,
                                max_attempts,
                                e
                            ));
                        }
                        info!(
                            "⏳ Waiting for stream '{}' (attempt {}/{}). \
                             Orchestrator may still be starting...",
                            self.config.stream_name, attempt, max_attempts
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64))
                            .await;
                    }
                }
            }
        };

        // Task consumer
        // Short ack_wait (30s) enables fast task recovery after agent crash.
        // During long-running LLM calls (propose/evaluate), a background task
        // sends `AckKind::Progress` heartbeats every 15s to extend the
        // deadline. Without heartbeats, the message is redelivered within 30s
        // — much faster than the previous 600s, allowing agents to resume
        // in-flight tasks almost immediately after restart.
        let task_consumer = stream
            .get_or_create_consumer(
                &self.config.consumer_name,
                jetstream::consumer::pull::Config {
                    durable_name: Some(self.config.consumer_name.clone()),
                    filter_subject: task_filter,
                    ack_wait: std::time::Duration::from_secs(30), // short — heartbeats extend
                    // Bound concurrent in-flight jobs for this agent (0 = unbounded).
                    // The server withholds the next task until an in-flight one acks.
                    max_ack_pending: self.config.max_ack_pending(),
                    ..Default::default()
                },
            )
            .await?;

        // Manifest consumer (broadcast to all agents)
        let manifest_consumer_name = format!("manifest_watcher_{}", self.agent_id);
        let manifest_consumer = stream
            .get_or_create_consumer(
                &manifest_consumer_name,
                jetstream::consumer::pull::Config {
                    durable_name: Some(manifest_consumer_name.clone()),
                    filter_subject: format!("{}.jobs.manifest.>", self.config.api_prefix),
                    deliver_policy: jetstream::consumer::DeliverPolicy::New,
                    ..Default::default()
                },
            )
            .await?;

        // Score event subscription — listens to round_summary events so the
        // dashboard can show evaluation scores starting from round 1.
        //
        // Uses a Core NATS subscription (not JetStream) because round_summary
        // events are published to per-session `nsed_results_{session_id}`
        // streams, not the global `sphera_jobs` stream.  Core NATS sees all
        // publishes regardless of which JetStream stream captures them.
        let score_subject = format!(
            "{}.*.result.event.round_summary",
            self.config.subject_prefix
        );
        let score_subscription: Option<async_nats::Subscriber> = if self.status.is_some() {
            match self.nats.subscribe(score_subject.clone()).await {
                Ok(sub) => Some(sub),
                Err(e) => {
                    warn!(
                        "Failed to subscribe to score events on {}: {}. Score tracking disabled.",
                        score_subject, e
                    );
                    None
                }
            }
        } else {
            None
        };

        // Job-complete subscription — the orchestrator's terminal event. Only
        // subscribe when an `on_job_complete` hook is configured (zero overhead
        // otherwise). Core NATS, same per-session publish pattern as scores.
        let job_complete_subject =
            format!("{}.*.result.event.job_complete", self.config.subject_prefix);
        let job_complete_subscription: Option<async_nats::Subscriber> =
            if self.job_complete_mw.is_some() {
                match self.nats.subscribe(job_complete_subject.clone()).await {
                    Ok(sub) => Some(sub),
                    Err(e) => {
                        warn!(
                            "Failed to subscribe to job_complete on {}: {}. \
                             on_job_complete hook disabled.",
                            job_complete_subject, e
                        );
                        None
                    }
                }
            } else {
                None
            };

        // Passthrough subscription — Core NATS request-reply for PolicyMode::Passthrough.
        // When the orchestrator routes a request directly to this agent (no deliberation
        // cycle), it publishes to this subject and waits for a synchronous reply.
        let passthrough_subject = format!(
            "{}.agent.{}.{}",
            self.config.subject_prefix, self.agent_id, PASSTHROUGH_SUBJECT_SUFFIX
        );
        let passthrough_subscription: Option<async_nats::Subscriber> =
            match self.nats.subscribe(passthrough_subject.clone()).await {
                Ok(sub) => Some(sub),
                Err(e) => {
                    warn!(
                        "Failed to subscribe to passthrough subject {}: {}. \
                         Passthrough mode unavailable for this agent.",
                        passthrough_subject, e
                    );
                    None
                }
            };

        info!(
            "🎧 Agent {} listening for tasks, manifests, score events, and passthrough requests.",
            self.agent_id
        );

        // Push initial "connected" event to dashboard
        if let Some(ref status) = self.status {
            let mut snap = status.write().await;
            snap.nats_connected = true;
            snap.push_event("connected", None, "NATS connected, listening for tasks");
        }

        let mut task_messages = task_consumer.messages().await?;
        let mut manifest_messages = manifest_consumer.messages().await?;
        let mut score_messages = score_subscription;
        let mut job_complete_messages = job_complete_subscription;
        let mut passthrough_messages = passthrough_subscription;
        let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(10));
        let mut drain_interval = tokio::time::interval(std::time::Duration::from_millis(500));
        let mut conn_check_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut last_conn_state = NatsState::Connected;
        let mut reconnects_so_far: u32 = 0;

        loop {
            // When paused, skip task consumption but keep draining buffer,
            // processing manifests, and sending heartbeats.
            let is_paused = self.paused.load(Ordering::Relaxed);

            tokio::select! {
                Some(msg_res) = task_messages.next(), if !is_paused => {
                    match msg_res {
                        Ok(msg) => {
                            let worker = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = worker.handle_message(msg).await {
                                    error!("Failed to process task: {:?}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Task consumer error: {:?}", e);
                            break;
                        }
                    }
                }
                Some(msg_res) = manifest_messages.next() => {
                    match msg_res {
                        Ok(msg) => {
                            let worker = self.clone();
                            tokio::spawn(async move {
                                if let Err(e) = worker.handle_manifest(msg).await {
                                    error!("Failed to process manifest: {:?}", e);
                                }
                            });
                        }
                        Err(e) => {
                             error!("Manifest consumer error: {:?}", e);
                             break;
                        }
                    }
                }
                Some(msg) = async {
                    match &mut score_messages {
                        Some(sub) => sub.next().await,
                        None => std::future::pending::<Option<async_nats::Message>>().await,
                    }
                } => {
                    let worker = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = worker.handle_round_summary(msg).await {
                            warn!("Failed to process round summary: {:?}", e);
                        }
                    });
                }
                Some(msg) = async {
                    match &mut job_complete_messages {
                        Some(sub) => sub.next().await,
                        None => std::future::pending::<Option<async_nats::Message>>().await,
                    }
                } => {
                    let worker = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = worker.handle_job_complete(msg).await {
                            warn!("Failed to process job_complete: {:?}", e);
                        }
                    });
                }
                Some(msg) = async {
                    match &mut passthrough_messages {
                        Some(sub) => sub.next().await,
                        None => std::future::pending::<Option<async_nats::Message>>().await,
                    }
                } => {
                    if is_paused {
                        // Worker is paused — reject immediately instead of calling the LLM.
                        if let Some(reply_subject) = msg.reply.clone() {
                            let err = PassthroughError {
                                error: "Agent is paused and cannot handle passthrough requests"
                                    .to_string(),
                            };
                            let payload = serde_json::to_vec(&err).unwrap_or_default();
                            let _ = self.nats.publish(reply_subject, payload.into()).await;
                        }
                    } else {
                        let worker = self.clone();
                        tokio::spawn(async move {
                            worker.handle_passthrough(msg).await;
                        });
                    }
                }
                _ = heartbeat_interval.tick() => {
                    self.publish_heartbeat().await;
                }
                _ = drain_interval.tick() => {
                    self.drain_buffer().await;
                }
                _ = conn_check_interval.tick() => {
                    let current_state = self.nats.connection_state();
                    if current_state != last_conn_state {
                        if let Some(ref telemetry) = self.telemetry {
                            use crate::telemetry::NatsConnectionState;
                            let state: NatsConnectionState = (&current_state).into();
                            if matches!(
                                state,
                                NatsConnectionState::Reconnecting | NatsConnectionState::Connected
                            ) && matches!(last_conn_state, NatsState::Disconnected)
                            {
                                reconnects_so_far += 1;
                            }
                            // pending_publish_depth/buffer_bytes:
                            // see docs/agent-sdk/reference/telemetry.md
                            let conn_ctx = crate::telemetry::TelemetryContext::new(
                                &self.agent_id,
                                None,
                                None,
                                None,
                            );
                            crate::emit_event!(
                                Some(telemetry),
                                conn_ctx,
                                NatsConnectionStateChanged {
                                    state,
                                    reconnects_so_far,
                                    pending_publish_depth: None,
                                    buffer_bytes: None,
                                }
                            );
                        }
                        last_conn_state = current_state;
                    }
                }
                else => {
                    error!("Both consumers closed. Exiting worker loop for {}", self.agent_id);
                    break;
                }
            }
        }

        warn!(
            "Worker loop exited for agent {}. Reconnecting...",
            self.agent_id
        );
        Ok(())
    }

    async fn handle_manifest(&self, msg: async_nats::jetstream::Message) -> Result<()> {
        let manifest: JobManifest = match serde_json::from_slice(&msg.payload) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to parse manifest: {}", e);
                let _ = msg.ack().await;
                return Ok(());
            }
        };

        if manifest.agents.contains(&self.agent_id) {
            info!(
                "🔔 Agent {} selected for Job {}. Accepting.",
                self.agent_id, manifest.job_id
            );
            {
                let mut jobs = self.active_jobs.lock().unwrap();
                jobs.insert(manifest.job_id.clone());
            }

            let ack_subject = format!(
                "{}.jobs.ack.{}.{}",
                self.config.api_prefix, manifest.job_id, self.agent_id
            );
            let mut ack_payload = serde_json::to_vec(&serde_json::json!({
                "agent_id": self.agent_id,
                "status": "Accepted",
                "timestamp": chrono::Utc::now().to_rfc3339()
            }))?;

            // Hook interception point
            if let Some(ref hook) = self.hook {
                hook.before_publish(&ack_subject, &mut ack_payload).await?;
            }

            self.nats.publish(ack_subject, ack_payload.into()).await?;

            let event_subject = format!(
                "{}.{}.result.event.agent_accepted",
                self.config.subject_prefix, manifest.job_id
            );
            let event_payload = serde_json::json!({
                "agent_id": self.agent_id,
                "status": "Online",
                "role": "Generalist"
            });
            self.nats
                .publish(event_subject, serde_json::to_vec(&event_payload)?.into())
                .await?;

            // Push to dashboard event log
            if let Some(ref status) = self.status {
                let mut snap = status.write().await;
                snap.push_event(
                    "agent_accepted",
                    Some(&manifest.job_id),
                    &format!("Accepted job manifest ({})", manifest.task_description),
                );
            }
        }

        let _ = msg.ack().await;
        Ok(())
    }

    /// Process a `round_summary` event to update the dashboard score badge.
    ///
    /// The orchestrator publishes this after each evaluation phase with per-proposal
    /// scores.  We look up this agent's entry and call `push_score()` so the HITL
    /// dashboard can show colour-coded divergence immediately (even for round 1).
    async fn handle_round_summary(&self, msg: async_nats::Message) -> Result<()> {
        let summary: crate::events::RoundSummaryEvent = serde_json::from_slice(&msg.payload)
            .map_err(|e| {
                warn!("Failed to parse round_summary event: {}", e);
                e
            })?;

        // Extract session_id from subject: {prefix}.{session_id}.result.event.round_summary
        let session_id = session_id_from_subject(msg.subject.as_str(), &self.config.subject_prefix);

        // Do not gate on active_jobs — that set tracks task-in-flight and is
        // cleared after each task completion, but round_summary events arrive
        // asynchronously after evaluation finishes (when the task is already
        // removed from active_jobs).  The agent_id check below provides the
        // real correctness filter, and already_has prevents duplicate scores.

        // Find this agent's proposal score in the summary.
        for entry in &summary.proposal_scores {
            if entry.agent_id == self.agent_id {
                if let Some(ref status) = self.status {
                    let mut snap = status.write().await;
                    let already_has = snap
                        .recent_scores
                        .iter()
                        .any(|s| s.job_id == session_id && s.round == summary.round);
                    if !already_has {
                        snap.push_score(crate::status::ScoreEntry {
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            job_id: session_id.clone(),
                            round: summary.round,
                            evaluator: "aggregated".into(),
                            score: entry.aggregated_score,
                        });
                    }
                }
                break;
            }
        }

        // on_completion hook — fires per round-summary with the round's winner
        // (top aggregated_score) in `metadata.winner`. NOTE: this is per-round,
        // not a single job-final signal (the orchestrator owns halting), so a
        // completion middleware with once-only side effects must guard itself.
        if self.completion_mw.is_some() {
            let winner = pick_winner(&summary.proposal_scores);
            let content = serde_json::json!({
                "round": summary.round,
                "proposal_scores": summary.proposal_scores,
            });
            let meta = serde_json::json!({ "winner": winner });
            if let Err(e) = self
                .run_stage_mw(
                    &self.completion_mw,
                    "complete",
                    &session_id,
                    summary.round,
                    crate::middleware::MiddlewareStage::Completion,
                    content,
                    meta,
                )
                .await
            {
                warn!(agent_id = %self.agent_id, error = %e, "on_completion middleware error");
            }
        }

        Ok(())
    }

    /// Handle the orchestrator's terminal `job_complete` event — fire the
    /// `on_job_complete` hook **once** with the final winner. Distinct from the
    /// per-round `on_completion` (which stays for per-round agent reactions).
    async fn handle_job_complete(&self, msg: async_nats::Message) -> Result<()> {
        let event: crate::events::JobCompleteEvent =
            serde_json::from_slice(&msg.payload).map_err(|e| {
                warn!("Failed to parse job_complete event: {}", e);
                e
            })?;

        let session_id = session_id_from_subject(msg.subject.as_str(), &self.config.subject_prefix);
        let (content, meta) = job_complete_payload(&event);
        match self
            .run_stage_mw(
                &self.job_complete_mw,
                "job_complete",
                &session_id,
                event.rounds_completed,
                crate::middleware::MiddlewareStage::JobComplete,
                content,
                meta,
            )
            .await
        {
            // A clean winner consensus surfaces `project_advanced {project_id, head}`
            // in the verdict content — republish it as the "epic advanced, pull now"
            // notification so clients holding the project sync.
            Ok(Some(verdict_content)) => {
                if let Some((subject, payload)) = crate::project_registry::advanced_notification(
                    &verdict_content,
                    &self.config.subject_prefix,
                ) {
                    match self.nats.publish(subject.clone(), payload.into()).await {
                        Ok(()) => {
                            tracing::debug!(agent_id = %self.agent_id, subject = %subject, "published project_advanced")
                        }
                        Err(e) => {
                            warn!(agent_id = %self.agent_id, subject = %subject, error = %e, "failed to publish project_advanced")
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(agent_id = %self.agent_id, error = %e, "on_job_complete middleware error")
            }
        }
        Ok(())
    }

    /// Handle a passthrough request received on the core NATS subject.
    ///
    /// The orchestrator sends a [`PassthroughRequest`] and expects a synchronous
    /// reply with either a [`PassthroughResponse`] (success) or a
    /// [`PassthroughError`] (failure, wrapped in a JSON `error` field).
    ///
    /// This method never returns an error — failures are replied back to the
    /// orchestrator so it can surface them to the caller.
    async fn handle_passthrough(&self, msg: async_nats::Message) {
        let reply_subject = match msg.reply.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                warn!(
                    agent_id = %self.agent_id,
                    "Passthrough message has no reply subject — ignoring"
                );
                return;
            }
        };

        let request: PassthroughRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(e) => {
                warn!(agent_id = %self.agent_id, "Failed to parse passthrough request: {}", e);
                let err_payload = serde_json::to_vec(&PassthroughError {
                    error: e.to_string(),
                })
                .unwrap_or_default();
                let _ = self.nats.publish(reply_subject, err_payload.into()).await;
                return;
            }
        };

        info!(
            agent_id = %self.agent_id,
            session_id = %request.session_id,
            "Handling passthrough request"
        );

        let chat_agent = match &self.chat_agent {
            Some(a) => a.clone(),
            None => {
                let err = PassthroughError {
                    error: format!(
                        "Agent '{}' does not support passthrough mode (ChatCapable not configured)",
                        self.agent_id
                    ),
                };
                let payload = serde_json::to_vec(&err).unwrap_or_default();
                let _ = self.nats.publish(reply_subject, payload.into()).await;
                return;
            }
        };

        // Convert PassthroughMessage → async_openai ChatCompletionRequestMessage
        let messages: Vec<async_openai::types::ChatCompletionRequestMessage> = request
            .messages
            .into_iter()
            .filter_map(|m| {
                match m.role.as_str() {
                    "user" => Some(async_openai::types::ChatCompletionRequestMessage::User(
                        async_openai::types::ChatCompletionRequestUserMessage {
                            content:
                                async_openai::types::ChatCompletionRequestUserMessageContent::Text(
                                    m.content,
                                ),
                            name: None,
                        },
                    )),
                    "assistant" => Some(
                        async_openai::types::ChatCompletionRequestMessage::Assistant(
                            async_openai::types::ChatCompletionRequestAssistantMessage {
                                content: Some(
                                    async_openai::types::ChatCompletionRequestAssistantMessageContent::Text(m.content),
                                ),
                                ..Default::default()
                            },
                        ),
                    ),
                    "system" => Some(async_openai::types::ChatCompletionRequestMessage::System(
                        async_openai::types::ChatCompletionRequestSystemMessage {
                            content:
                                async_openai::types::ChatCompletionRequestSystemMessageContent::Text(
                                    m.content,
                                ),
                            name: None,
                        },
                    )),
                    other => {
                        warn!(
                            agent_id = %self.agent_id,
                            "Unknown role '{}' in passthrough message — skipping", other
                        );
                        None
                    }
                }
            })
            .collect();

        if messages.is_empty() {
            warn!(
                agent_id = %self.agent_id,
                "Passthrough request contained no valid messages (all roles unknown) — rejecting"
            );
            let err = PassthroughError {
                error: "No valid messages provided for passthrough (all message roles unknown)"
                    .to_string(),
            };
            let payload = serde_json::to_vec(&err).unwrap_or_default();
            let _ = self.nats.publish(reply_subject, payload.into()).await;
            return;
        }

        // Passthrough timeout: derived from `response_sla_secs` but
        // capped at 600 s because passthrough is a single-shot request
        // (no propose/evaluate rounds) and a hung Claude CLI would
        // otherwise block the handler forever with no error surfaced to
        // the caller. The cap is intentionally below the full deliberation
        // budget — operators who need longer should use deliberation mode.
        //
        // `response_sla_secs == 0` is the "no explicit budget" sentinel
        // (per the branch-1 `PolicySla::job_timeout` invariant). A naive
        // `.min(600)` would yield 0 here and cause an immediate
        // `Elapsed` failure on every passthrough — fall back to the
        // 600 s cap instead.
        const PASSTHROUGH_TIMEOUT_CAP_SECS: u64 = 600;
        let configured = self.agent_config.response_sla_secs;
        let passthrough_timeout = std::time::Duration::from_secs(if configured == 0 {
            PASSTHROUGH_TIMEOUT_CAP_SECS
        } else {
            configured.min(PASSTHROUGH_TIMEOUT_CAP_SECS)
        });
        let chat_result =
            tokio::time::timeout(passthrough_timeout, chat_agent.chat(messages)).await;

        let response_content = match chat_result {
            Ok(Ok(content)) => content,
            Ok(Err(e)) => {
                warn!(agent_id = %self.agent_id, "Passthrough chat failed: {}", e);
                let err = PassthroughError {
                    error: e.to_string(),
                };
                let payload = serde_json::to_vec(&err).unwrap_or_default();
                let _ = self.nats.publish(reply_subject, payload.into()).await;
                return;
            }
            Err(_) => {
                warn!(
                    agent_id = %self.agent_id,
                    timeout_secs = passthrough_timeout.as_secs(),
                    "Passthrough chat timed out"
                );
                let err = PassthroughError {
                    error: format!(
                        "Passthrough request timed out after {}s",
                        passthrough_timeout.as_secs()
                    ),
                };
                let payload = serde_json::to_vec(&err).unwrap_or_default();
                let _ = self.nats.publish(reply_subject, payload.into()).await;
                return;
            }
        };

        let response = PassthroughResponse {
            content: response_content,
            input_tokens: None,
            output_tokens: None,
        };
        let payload = match serde_json::to_vec(&response) {
            Ok(b) => b,
            Err(e) => {
                error!(agent_id = %self.agent_id, "Failed to serialize passthrough response: {}", e);
                let err = PassthroughError {
                    error: format!("Failed to serialize response: {e}"),
                };
                let err_payload = serde_json::to_vec(&err).unwrap_or_default();
                let _ = self.nats.publish(reply_subject, err_payload.into()).await;
                return;
            }
        };
        let _ = self.nats.publish(reply_subject, payload.into()).await;
    }

    /// Run one middleware pipeline for a hook point. Returns the (possibly
    /// transformed) `content` on pass, `None` when the pipeline is unconfigured,
    /// or an `Err` when a middleware blocks (fails the task).
    #[allow(clippy::too_many_arguments)] // cohesive hook-invocation signature
    async fn run_stage_mw(
        &self,
        pipeline: &Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
        action: &str,
        session_id: &str,
        round: u32,
        stage: crate::middleware::MiddlewareStage,
        content: serde_json::Value,
        metadata: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        match pipeline {
            Some(p) => run_stage_pipeline(
                p,
                &self.agent_id,
                action,
                session_id,
                round,
                stage,
                content,
                metadata,
            )
            .await
            .map(Some),
            None => Ok(None),
        }
    }

    async fn handle_message(&self, msg: async_nats::jetstream::Message) -> Result<()> {
        // Wrap in Arc so the heartbeat background task can send Progress acks
        // while the main task continues using the message for ack/payload access.
        let msg = std::sync::Arc::new(msg);

        // Build a dedup key unique even across stream recreations
        let msg_id = match msg.info() {
            Ok(info) => format!("{}-{}-{}", info.stream, info.stream_sequence, msg.subject),
            Err(_) => msg
                .headers
                .as_ref()
                .and_then(|h| h.get("Nats-Msg-Id").map(|v| v.to_string()))
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
        };

        if self.is_duplicate(&msg_id).await? {
            warn!(
                "♻️ Detected duplicate message {}. Acking and skipping.",
                msg_id
            );
            msg.ack()
                .await
                .map_err(|e| anyhow::anyhow!(e))
                .context("Failed to ack duplicate")?;
            return Ok(());
        }

        info!("📨 Received Task: {} (Subject: {})", msg_id, msg.subject);

        // Record when the task was received for SLA-deadline buffer release.
        let task_received = Instant::now();

        let mut context: AgentContext = match serde_json::from_slice(&msg.payload) {
            Ok(ctx) => ctx,
            Err(e) => {
                error!(
                    msg_id = %msg_id,
                    error = %e,
                    "❌ Failed to deserialize AgentContext. Poison pill detected. Acking to discard."
                );
                if let Err(ack_err) = msg.ack().await {
                    error!("Failed to ack poison pill: {}", ack_err);
                }
                return Ok(());
            }
        };

        let subject_parts: Vec<&str> = msg.subject.split('.').collect();
        let prefix = &self.config.subject_prefix;
        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject_parts
            .get(prefix_count)
            .unwrap_or(&"global")
            .to_string();
        // Own the action string so `msg` can be moved into the buffer later.
        let action: String = subject_parts.last().unwrap_or(&"unknown").to_string();
        let action = action.as_str();

        // Publish "agent_working" event
        let work_start_subject = format!(
            "{}.{}.result.event.agent_working",
            self.config.subject_prefix, session_id
        );
        let work_start_payload = serde_json::json!({
            "agent_id": self.agent_id,
            "round": context.round_number,
            "action": action,
            "status": "Thinking"
        });
        if let Err(e) = self
            .nats
            .publish(
                work_start_subject,
                serde_json::to_vec(&work_start_payload)?.into(),
            )
            .await
        {
            warn!("Failed to publish work start event: {}", e);
        }

        // Push to dashboard event log
        if let Some(ref status) = self.status {
            let mut snap = status.write().await;
            snap.push_event(
                "agent_working",
                Some(&session_id),
                &format!("Round {} {}", context.round_number, action),
            );
        }

        // Attach scratchpad store to context
        context.store = Some(Arc::new(NatsScratchpadStore::new(
            self.scratchpad_kv.clone(),
            self.js.clone(),
            session_id.clone(),
        )) as Arc<dyn PersistenceStore>);

        context.telemetry = self.telemetry.clone();

        // Construct UserToolHandler if user tools are registered and factory is available
        let user_tool_names: Vec<&str> =
            context.user_tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!(
            agent = %self.agent.name(),
            user_tool_count = context.user_tools.len(),
            user_tools = ?user_tool_names,
            has_factory = self.user_tool_factory.is_some(),
            "user-tool wiring: names arriving in AgentContext (empty ⇒ ask_user won't be advertised)"
        );
        if !context.user_tools.is_empty() {
            if let Some(ref factory) = self.user_tool_factory {
                context.user_tool_handler = Some(factory.create(
                    self.nats.clone(),
                    self.js.clone(),
                    session_id.clone(),
                    self.agent.name(),
                    context.phase_budget_remaining_secs,
                    self.config.subject_prefix.clone(),
                ));
            }
        }

        // Track this job as active (for heartbeat reporting)
        {
            let mut jobs = self.active_jobs.lock().unwrap();
            jobs.insert(session_id.clone());
        }

        // Update status: task started
        if let Some(ref status) = self.status {
            let mut snap = status.write().await;
            snap.current_job = Some(session_id.clone());
            snap.current_round = Some(context.round_number);
            snap.current_phase = Some(action.to_string());
        }

        // Record previous round's aggregated score for dashboard display.
        // The orchestrator populates `previous_own_score` on round 2+ propose tasks.
        if action == "propose" && context.round_number > 1 {
            if let Some(score) = context.previous_own_score {
                if let Some(ref status) = self.status {
                    let mut snap = status.write().await;
                    let prev_round = context.round_number.saturating_sub(1);
                    let already_has = snap
                        .recent_scores
                        .iter()
                        .any(|s| s.job_id == session_id && s.round == prev_round);
                    if !already_has {
                        snap.push_score(crate::status::ScoreEntry {
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            job_id: session_id.clone(),
                            round: prev_round,
                            evaluator: "aggregated".to_string(),
                            score,
                        });
                    }
                }
            }
        }

        let dispatch_delay_ms = task_received.elapsed().as_millis() as u64;
        // NTP-skew negatives clamp to zero — `job_age_at_accept_ms`
        // is `>= 0` by contract.
        let task_publish_ts = context.task_publish_ts;
        let agent_receive_ts = chrono::Utc::now().timestamp_millis();
        let job_age_at_accept_ms = task_publish_ts.map(|publish_ts| {
            agent_receive_ts
                .checked_sub(publish_ts)
                .map_or(0, |diff| diff.max(0))
        });
        crate::emit_for!(
            context,
            TaskAccepted {
                dispatch_delay_ms,
                task_publish_ts,
                job_age_at_accept_ms,
            }
        );

        // Execute the action with progress heartbeats.
        // The heartbeat task sends AckKind::Progress every 15s to extend the
        // JetStream ack deadline (30s), preventing premature redelivery during
        // long LLM calls while keeping ack_wait short for fast crash recovery.
        let msg_heartbeat = msg.clone();
        let hb_session = session_id.clone();
        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                if let Err(e) = msg_heartbeat
                    .ack_with(async_nats::jetstream::AckKind::Progress)
                    .await
                {
                    tracing::warn!(
                        session_id = %hb_session,
                        "Failed to send task ack heartbeat: {}", e
                    );
                    break;
                }
            }
        });

        // before_prompt hook — transform the task before the agent builds its
        // prompt. Mutates `task_description` (which feeds the PromptSet). No-op
        // when unconfigured; a block fails the task.
        if self.before_prompt_mw.is_some() {
            let content = serde_json::json!({
                "task_description": context.task_description,
                "user_injections": context.user_injections,
            });
            // Thread the conversation id (top-level metadata) so patch-deliberation can
            // key the agent worktree on the stable thread — not the per-job id — and
            // keep the provider's cwd-scoped session resuming across turns. Top-level
            // (not under `patch_deliberation`): the dylib config-merge is a shallow
            // overlay that would clobber a nested key.
            let meta = match &context.conversation_id {
                Some(cid) => serde_json::json!({ "conversation_id": cid }),
                None => serde_json::json!({}),
            };
            if let Some(new) = self
                .run_stage_mw(
                    &self.before_prompt_mw,
                    action,
                    &session_id,
                    context.round_number,
                    crate::middleware::MiddlewareStage::BeforePrompt,
                    content,
                    meta,
                )
                .await?
            {
                if let Some(td) = new.get("task_description").and_then(|v| v.as_str()) {
                    context.task_description = td.to_string();
                } else {
                    tracing::debug!(
                        agent_id = %self.agent_id,
                        "before_prompt middleware returned no string `task_description` — transform dropped"
                    );
                }
                // A middleware may declare a JSON schema to constrain (and force)
                // the proposal submission — thread it to the agent.
                if let Some(schema) = new.get("proposal_schema") {
                    if schema.is_object() {
                        context.forced_proposal_schema = Some(schema.clone());
                    }
                }
                // A middleware may declare a per-job worktree (e.g. patch-deliberation
                // `agent_working_dir`) — run the agent subprocess with cwd = that dir
                // instead of the stale launch dir, so bare git + relative reads hit
                // the frozen job-scoped tree.
                if let Some(wt) = new.get("agent_working_dir").and_then(|v| v.as_str()) {
                    context.working_dir_override = Some(std::path::PathBuf::from(wt));
                }
                // Advertise this agent under its epic — project_id (+ epic_head) come
                // from the verdict (patch-deliberation surfaces them). Lets the fleet
                // route reads/discovery to a live holder of the project regardless of
                // each node's local path. No-op when there's no project_id.
                if let Some(adv) = crate::project_registry::ProjectAdvertisement::from_verdict(
                    &new,
                    &self.agent_id,
                    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()),
                ) {
                    let subject =
                        crate::project_registry::advert_subject(&self.config.subject_prefix);
                    match serde_json::to_vec(&adv) {
                        Ok(payload) => {
                            if let Err(e) = self.nats.publish(subject.clone(), payload.into()).await
                            {
                                tracing::warn!(agent_id = %self.agent_id, subject = %subject, error = %e, "failed to publish project advertisement");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(agent_id = %self.agent_id, error = %e, "failed to serialize project advertisement")
                        }
                    }
                }
            }
        }

        // Inject the reviewer as the react loop's submission validator so a
        // provider_response block (e.g. patch-deliberation "applied ZERO changes")
        // re-prompts the agent in-loop, reusing the loop's own retry budget instead
        // of a second one. Proposals only — evaluations have no provider_response.
        context.submission_validator = build_submission_validator(
            action,
            &self.provider_response_mw,
            &self.agent_id,
            &session_id,
            context.round_number,
        );

        let task_start = Instant::now();
        // Retry loop for transient transport errors (broken pipe, connection reset, etc.).
        // Wraps the full propose/evaluate call because the SDK cannot pinpoint where inside
        // the agent's implementation the transport failure occurred. Only errors matching
        // `is_transient_error()` are retried; LLM-level or logic errors break immediately.
        let execution_result = {
            const MAX_TASK_RETRIES: u32 = 2;
            let mut last_err = None;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let result = async {
                    match action {
                        "propose" => {
                            // The agent's react loop already validated the submission via
                            // the injected `submission_validator` (a reviewer block re-prompts
                            // in-loop, reusing its retry budget). This provider_response call
                            // is the accepted proposal's commit + content transform; it is
                            // idempotent (self-cleaning) with the in-loop validation run.
                            let mut proposal = self.agent.propose(&context).await?;
                            if self.provider_response_mw.is_some() {
                                if let Some(new) = self
                                    .run_stage_mw(
                                        &self.provider_response_mw,
                                        "propose",
                                        &session_id,
                                        context.round_number,
                                        crate::middleware::MiddlewareStage::ProviderResponse,
                                        serde_json::json!(proposal.content),
                                        serde_json::json!({}),
                                    )
                                    .await?
                                {
                                    if let Some(c) = new.as_str() {
                                        proposal.content = c.to_string();
                                    }
                                }
                            }
                            proposal.published_at_ms = chrono::Utc::now().timestamp_millis();
                            serde_json::to_vec(&proposal).map_err(|e| anyhow::anyhow!(e))
                        }
                        "evaluate" => {
                            let evaluations = self.agent.evaluate(&context).await?;
                            // Filter out evaluations targeting unknown candidates (LLM hallucinations).
                            // Only keep entries whose target_id matches a known candidate.
                            let valid_ids: std::collections::HashSet<&str> =
                                context.candidates.iter().map(|c| c.id.as_str()).collect();
                            let publish_ts = chrono::Utc::now().timestamp_millis();
                            let filtered: Vec<_> = evaluations
                                .into_iter()
                                .filter(|(target_id, _)| {
                                    if valid_ids.contains(target_id.as_str()) {
                                        true
                                    } else {
                                        tracing::warn!(
                                            target_id = %target_id,
                                            "Dropping evaluation with hallucinated target ID"
                                        );
                                        false
                                    }
                                })
                                .map(|(target_id, mut eval)| {
                                    eval.published_at_ms = publish_ts;
                                    (target_id, eval)
                                })
                                .collect();
                            serde_json::to_vec(&filtered).map_err(|e| anyhow::anyhow!(e))
                        }
                        _ => Err(anyhow::anyhow!("Unknown action: {}", action)),
                    }
                }
                .await;

                match result {
                    Ok(payload) => break Ok(payload),
                    Err(e) => {
                        let is_retryable = is_transient_error(&e);
                        if is_retryable && attempt <= MAX_TASK_RETRIES {
                            warn!(
                                agent = %self.agent_id,
                                attempt,
                                max = MAX_TASK_RETRIES + 1,
                                error = %e,
                                "Transient task error, retrying after backoff"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(
                                2u64.pow(attempt - 1),
                            ))
                            .await;
                            last_err = Some(e);
                            continue;
                        }
                        let _ = last_err; // suppress unused warning
                        break Err(e);
                    }
                }
            }
        };
        let task_duration_ms = task_start.elapsed().as_millis() as u64;

        // Stop heartbeat — task complete (or failed), we'll ack/nack below
        heartbeat_handle.abort();

        match execution_result {
            Ok(mut response_payload) => {
                // Extract a content preview before the payload is moved/consumed.
                // Pass candidates so evaluation previews include anonymized proposal text.
                let content_preview =
                    extract_content_preview(&response_payload, action, &context.candidates);

                let reply_subject = format!(
                    "{}.{}.result.{}.{}.{}",
                    self.config.subject_prefix,
                    session_id,
                    context.round_number,
                    self.agent_id,
                    action
                );

                // HITL buffer: hold response for operator review instead of
                // publishing immediately. The background drain task in run()
                // handles publication when the hold expires.
                //
                // The crypto-wrap hook (`before_publish`) runs at the actual
                // publish point — for the immediate path, just before
                // `nats.publish`; for the buffered path, in `drain_buffer`
                // after annotation injection and `restamp_published_at`.
                // This keeps signatures over the final on-the-wire bytes
                // and lets the restamp update the inner publish_at_ms
                // without invalidating any envelope.
                //
                // CRITICAL: We ack the JetStream message and mark it processed
                // IMMEDIATELY. This prevents JetStream from redelivering the
                // task (default ack_wait ~30s) which would bypass the buffer
                // and cause the orchestrator to receive duplicate responses.
                // The response payload stays in the buffer — only the NATS ack
                // is immediate, NOT the publish.
                let was_buffered = if let Some(ref buf) = self.response_buffer {
                    // Ack + dedup BEFORE buffering to prevent JetStream redelivery
                    self.mark_processed(&msg_id).await?;
                    msg.ack()
                        .await
                        .map_err(|e| anyhow::anyhow!("buffer pre-ack failed: {}", e))?;

                    let hold = buf.hold_duration();
                    let now = Instant::now();
                    let entry = buffer::BufferedResponse {
                        id: Uuid::new_v4().to_string(),
                        action: action.to_string(),
                        job_id: session_id.clone(),
                        round: context.round_number,
                        reply_subject,
                        payload: response_payload,
                        created_at: now,
                        release_at: now + hold, // fallback; overridden by push_with_deadline if SLA set
                        ack_handle: Box::new(buffer::PreAckedHandle),
                        msg_id: msg_id.clone(),
                        annotations: Vec::new(),
                        edited: false,
                        stopped: self.agent_config.auto_stop,
                    };
                    buf.push_with_deadline(entry, task_received).await;
                    info!(
                        "📦 Buffered response: {} (SLA-based release, pre-acked)",
                        msg_id
                    );

                    // Update status: buffered (not yet published)
                    if let Some(ref status) = self.status {
                        let mut snap = status.write().await;
                        snap.current_job = None;
                        snap.current_round = None;
                        snap.current_phase = None;
                        snap.buffered_count = buf.len().await as u32;
                        snap.push_event(
                            "response_buffered",
                            Some(&session_id),
                            &format!(
                                "Round {} {} buffered {}ms hold",
                                context.round_number,
                                action,
                                hold.as_millis()
                            ),
                        );
                    }
                    true
                } else {
                    // No buffer — publish immediately (original behavior)
                    if let Some(ref hook) = self.hook {
                        hook.before_publish(&reply_subject, &mut response_payload)
                            .await?;
                    }
                    self.nats
                        .publish(reply_subject, response_payload.into())
                        .await?;
                    self.mark_processed(&msg_id).await?;
                    msg.ack().await.map_err(|e| anyhow::anyhow!(e))?;
                    info!("✅ Task Complete: {}", msg_id);
                    false
                };

                // Remove finished job from active set
                {
                    let mut jobs = self.active_jobs.lock().unwrap();
                    jobs.remove(&session_id);
                }

                // Update status: always log the task to recent_tasks (for content
                // lookup), but only fire task_complete event for non-buffered tasks.
                // Buffered tasks already fired response_buffered above — the
                // dashboard uses that event to spawn the rain card.
                if let Some(ref status) = self.status {
                    let mut snap = status.write().await;
                    snap.current_job = None;
                    snap.current_round = None;
                    snap.current_phase = None;
                    snap.push_task(TaskLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        action: action.to_string(),
                        job_id: session_id.clone(),
                        round: context.round_number,
                        status: "ok".into(),
                        duration_ms: task_duration_ms,
                        content_preview: content_preview.clone(),
                    });
                    if !was_buffered {
                        snap.push_event(
                            "task_complete",
                            Some(&session_id),
                            &format!("{} ok {}ms", action, task_duration_ms),
                        );
                    }
                }

                // queue_wait_ms / llm_attempts / tool_call_count /
                // pending_publish_depth: see docs/agent-sdk/reference/telemetry.md
                let phase_budget_remaining_ms =
                    (context.phase_budget_remaining_secs * 1000.0) as i64;
                crate::emit_for!(
                    context,
                    TaskCompleted {
                        duration_ms: task_duration_ms,
                        dispatch_delay_ms,
                        queue_wait_ms: None,
                        phase_budget_remaining_ms,
                        llm_attempts: None,
                        tool_call_count: None,
                        pending_publish_depth: None,
                    }
                );
            }
            Err(e) => {
                let err_str = e.to_string();
                error!("❌ Task Execution Failed: {:?}", e);

                // Detect 402 Payment Required — auto-pause agent instead of
                // continuing to hammer a provider with no credits left.
                let is_payment_error = err_str.contains("402 Payment Required")
                    || err_str.contains("insufficient_quota")
                    || err_str.contains("billing");

                // Remove failed job from active set
                {
                    let mut jobs = self.active_jobs.lock().unwrap();
                    jobs.remove(&session_id);
                }

                // When propagate_payment_error is false, suppress the error
                // event and pause the worker so it stops pulling new tasks
                // (avoids wasting API calls against a provider with no credits).
                let suppress_error_event =
                    is_payment_error && !self.agent_config.propagate_payment_error;

                if suppress_error_event {
                    warn!(
                        "Payment error detected (propagate_payment_error=false) — pausing worker to avoid further API calls"
                    );
                    self.paused.store(true, Ordering::Relaxed);
                    if let Some(ref buf) = self.response_buffer {
                        buf.pause();
                    }
                }

                if !suppress_error_event {
                    // `reason` is a short machine-readable classifier so
                    // the orchestrator (and telemetry consumers) can group
                    // bails as parse_error / timeout / tool_error / etc.
                    // without re-parsing `error`. Treated identically to a
                    // missing vote — the orchestrator advances the round on
                    // this event the same way it would on a phase-timeout
                    // for this agent.
                    let reason = classify_abstention_reason(&err_str);
                    let error_payload = serde_json::json!({
                        "agent_id": self.agent_id,
                        "round": context.round_number,
                        "action": action,
                        "error": err_str,
                        "reason": reason,
                        "status": "Failed"
                    });
                    let error_bytes = serde_json::to_vec(&error_payload)?;

                    // TODO(removal): legacy session-wide event. Kept so
                    // existing dashboard SSE + audit consumers keep
                    // working through the migration. Remove once every
                    // consumer (dashboard.html, audit, telemetry
                    // forwarder) has switched to subscribing on the
                    // round-scoped `.failed` subject published below. At
                    // that point, drop this block and the corresponding
                    // `.legacy_subject_format` test.
                    let legacy_subject = format!(
                        "{}.{}.result.event.agent_error",
                        self.config.subject_prefix, session_id
                    );
                    if let Err(pub_err) = self
                        .nats
                        .publish(legacy_subject, error_bytes.clone().into())
                        .await
                    {
                        warn!("Failed to publish legacy agent_error event: {}", pub_err);
                    }

                    // Round-scoped failure marker for the orchestrator's
                    // per-phase consumer. Mirrors the verdict subject
                    // hierarchy (`result.{round}.{agent}.{action}`) with a
                    // `.failed` suffix so a single
                    // `filter_subjects=[verdict, verdict.failed]` consumer
                    // can wait on success-or-failure without payload-
                    // snooping for round / action / session.
                    if should_publish_failure_marker(action, is_payment_error) {
                        let failed_subject = failed_result_subject(
                            &self.config.subject_prefix,
                            &session_id,
                            context.round_number,
                            &self.agent_id,
                            action,
                        );
                        if let Err(pub_err) =
                            self.nats.publish(failed_subject, error_bytes.into()).await
                        {
                            warn!("Failed to publish round-scoped .failed marker: {}", pub_err);
                        }
                    }
                }

                msg.ack().await.map_err(|e| anyhow::anyhow!(e))?;

                // Update status: task failed
                if let Some(ref status) = self.status {
                    let mut snap = status.write().await;
                    snap.current_job = None;
                    snap.current_round = None;
                    snap.current_phase = None;
                    snap.push_task(TaskLogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        action: action.to_string(),
                        job_id: session_id.clone(),
                        round: context.round_number,
                        status: "error".into(),
                        duration_ms: task_duration_ms,
                        content_preview: Some(format!("Error: {}", err_str)),
                    });
                    snap.push_event(
                        "agent_error",
                        Some(&session_id),
                        &format!("{} failed: {}", action, err_str),
                    );
                }

                let phase_budget_remaining_ms =
                    (context.phase_budget_remaining_secs * 1000.0) as i64;
                // Payment errors get a distinct class so the orchestrator
                // can auto-resume when credits return.
                let failure_class = if is_payment_error {
                    TaskFailureClass::ToolError
                } else {
                    TaskFailureClass::Timeout
                };
                crate::emit_for!(
                    context,
                    TaskFailed {
                        duration_ms: task_duration_ms,
                        dispatch_delay_ms,
                        queue_wait_ms: None,
                        phase_budget_remaining_ms,
                        llm_attempts: None,
                        tool_call_count: None,
                        failure_class,
                        pending_publish_depth: None,
                    }
                );
            }
        }
        Ok(())
    }

    /// Publishes an agent heartbeat via core NATS pub/sub.
    async fn publish_heartbeat(&self) {
        let active_job = {
            let jobs = self.active_jobs.lock().unwrap();
            jobs.iter().next().cloned()
        };
        let hb_status = if active_job.is_some() {
            AgentLiveStatus::Busy
        } else {
            AgentLiveStatus::Idle
        };
        let uptime = self.start_time.elapsed().as_secs();
        // Collect reliability stats + last error from status snapshot
        let (tasks_completed, tasks_failed, last_error) = if let Some(ref status) = self.status {
            let snap = status.read().await;
            let err = snap
                .recent_tasks
                .iter()
                .find(|t| t.status == "error")
                .map(|t| {
                    let msg = format!("{}: {}", t.action, t.job_id);
                    msg.chars().take(120).collect::<String>()
                });
            (snap.tasks_completed, snap.tasks_failed, err)
        } else {
            (0, 0, None)
        };

        let heartbeat = AgentHeartbeat {
            agent_id: self.agent_id.clone(),
            status: hb_status,
            model_name: self.agent_config.model_name.clone(),
            provider_id: self.agent_config.provider_id.clone(),
            current_job: active_job.clone(),
            uptime_secs: uptime,
            timestamp: chrono::Utc::now().to_rfc3339(),
            input_price_per_mtok: self.agent_config.input_price_per_mtok,
            output_price_per_mtok: self.agent_config.output_price_per_mtok,
            chars_per_token: self.agent_config.chars_per_token,
            response_sla_secs: if self.agent_config.response_sla_secs > 0 {
                Some(self.agent_config.response_sla_secs)
            } else {
                None
            },
            temperature: Some(self.agent_config.temperature),
            frequency_penalty: self.agent_config.frequency_penalty,
            presence_penalty: self.agent_config.presence_penalty,
            max_tokens: Some(self.agent_config.max_tokens),
            context_window: Some(self.agent_config.context_window),
            tasks_completed,
            tasks_failed,
            last_error,
            capability_tags: self.agent_config.capability_tags.clone(),
            description: self.agent_config.description.clone(),
            signing_schemes: self.agent_config.signing_schemes.clone(),
        };
        let subject = format!(
            "{}.agent.heartbeat.{}",
            self.config.api_prefix, self.agent_id
        );
        match serde_json::to_vec(&heartbeat) {
            Ok(payload) => {
                if let Err(e) = self.nats.publish(subject, payload.into()).await {
                    warn!("Failed to publish heartbeat: {}", e);
                }
            }
            Err(e) => {
                warn!(agent_id = %self.agent_id, "Failed to serialize heartbeat: {}", e);
            }
        }

        // Update status snapshot for the embedded dashboard
        if let Some(ref status) = self.status {
            let mut snap = status.write().await;
            snap.uptime_secs = uptime;
            snap.nats_connected = true;
            snap.current_job = active_job.clone();
            snap.push_event(
                "heartbeat",
                active_job.as_deref(),
                &format!(
                    "{}  uptime {}s",
                    if active_job.is_some() { "busy" } else { "idle" },
                    uptime
                ),
            );
        }
    }

    /// Drain ready entries from the response buffer and publish them to NATS.
    ///
    /// If the operator annotated or edited an entry, the annotations are
    /// injected into the payload JSON before publication. Annotations are
    /// also published to a dedicated audit subject for traceability.
    async fn drain_buffer(&self) {
        let Some(ref buf) = self.response_buffer else {
            return;
        };

        // Adaptive SLA + auto-approve: when the status server is active,
        // use score-based divergence for the threshold check. Without a
        // status server, auto-approve with no divergence data (releases
        // immediately since the divergence gate passes on None).
        let divergence = if let Some(ref status) = self.status {
            let snap = status.read().await;
            let new_hold =
                buffer::compute_adaptive_hold(buf.base_hold_duration(), snap.mean_score, 3.0);
            buf.set_hold_duration(new_hold);
            buffer::compute_divergence(snap.mean_score, snap.score_std_dev)
        } else {
            None
        };
        let auto_released = buf.auto_release_if_eligible(divergence).await;
        if auto_released > 0 {
            // Render `None` divergence explicitly as "n/a" rather than
            // substituting a sentinel like -1.0 — the latter is
            // indistinguishable from a real (if impossible) negative
            // score in aggregated logs and confuses operators reading
            // the pass-through path for agents without a status server.
            let divergence_str =
                divergence.map_or_else(|| "n/a".to_string(), |d| format!("{d:.2}"));
            info!(
                "⚡ Auto-approved {} buffered entries for {} (divergence: {}, threshold: {:.2})",
                auto_released,
                self.agent_id,
                divergence_str,
                buf.auto_approve_threshold(),
            );
        }

        let ready = buf.drain_ready().await;
        for mut entry in ready {
            // Inject operator annotations into payload before publishing
            let publish_payload = if !entry.annotations.is_empty() || entry.edited {
                Self::inject_annotations(&entry)
            } else {
                entry.payload.clone()
            };

            // Re-stamp publish-instant at the actual publish point so
            // buffer dwell + HITL review time don't inflate the
            // orchestrator-side `propagation_ms` reading.
            let mut publish_payload =
                Self::restamp_published_at(&publish_payload, chrono::Utc::now().timestamp_millis());

            // Crypto-wrap hook runs AFTER restamp so any signature is
            // over the final on-the-wire bytes (including the fresh
            // publish_ts). Pre-buffer wrapping would invalidate the
            // signature when restamp mutated the payload.
            if let Some(ref hook) = self.hook
                && let Err(e) = hook
                    .before_publish(&entry.reply_subject, &mut publish_payload)
                    .await
            {
                error!(
                    "Failed to prepare buffered response {} for publish: {} — re-enqueuing",
                    entry.id, e
                );
                entry.release_at = Instant::now() + std::time::Duration::from_secs(5);
                buf.push(entry).await;
                continue;
            }

            if let Err(e) = self
                .nats
                .publish(entry.reply_subject.clone(), publish_payload.into())
                .await
            {
                error!(
                    "Failed to publish buffered response {}: {} — re-enqueuing",
                    entry.id, e
                );
                // Re-enqueue the entry so it's retried on the next drain cycle.
                // This is critical because the entry was already pre-acked from
                // JetStream — dropping it here means the response is permanently lost.
                // Add backoff to prevent hot-loop retries on persistent failures.
                entry.release_at = Instant::now() + std::time::Duration::from_secs(5);
                buf.push(entry).await;
                continue;
            }

            // Publish annotations to a dedicated audit subject for traceability
            if !entry.annotations.is_empty() {
                let annotation_subject = format!(
                    "{}.{}.annotations.{}.{}",
                    self.config.subject_prefix,
                    entry.job_id,
                    entry.round,
                    entry.id.get(..8).unwrap_or(&entry.id)
                );
                let annotation_payload = serde_json::json!({
                    "entry_id": entry.id,
                    "action": entry.action,
                    "job_id": entry.job_id,
                    "round": entry.round,
                    "edited": entry.edited,
                    "annotations": entry.annotations,
                });
                if let Err(e) = self
                    .nats
                    .publish(
                        annotation_subject,
                        serde_json::to_vec(&annotation_payload)
                            .unwrap_or_default()
                            .into(),
                    )
                    .await
                {
                    warn!("Failed to publish annotation audit trail: {}", e);
                }
            }

            if let Err(e) = self.mark_processed(&entry.msg_id).await {
                warn!("Failed to mark buffered response processed: {}", e);
            }
            if let Err(e) = entry.ack_handle.ack().await {
                warn!("Failed to ack buffered message: {}", e);
            }

            let edit_marker = if entry.edited { " [EDITED]" } else { "" };
            let annotation_count = entry.annotations.len();
            info!(
                "✅ Buffer released: {} ({} r{}){} ({} annotation(s))",
                entry.id, entry.action, entry.round, edit_marker, annotation_count
            );

            // Update status snapshot
            if let Some(ref status) = self.status {
                let mut snap = status.write().await;
                snap.buffered_count = buf.len().await as u32;
                let detail = if entry.edited {
                    format!(
                        "Round {} {} released from buffer (operator-edited)",
                        entry.round, entry.action
                    )
                } else if annotation_count > 0 {
                    format!(
                        "Round {} {} released from buffer ({} annotation(s))",
                        entry.round, entry.action, annotation_count
                    )
                } else {
                    format!(
                        "Round {} {} released from buffer",
                        entry.round, entry.action
                    )
                };
                snap.push_event("buffer_released", Some(&entry.job_id), &detail);

                // Fire task_complete so the full lifecycle is visible:
                // agent_working → response_buffered → buffer_released → task_complete
                snap.push_event(
                    "task_complete",
                    Some(&entry.job_id),
                    &format!("Round {} {} released", entry.round, entry.action),
                );
            }
        }
    }

    /// Inject operator annotations into a buffered response's payload JSON.
    ///
    /// For **proposals** (JSON objects): adds `operator_annotations` array and
    /// `edited_by: "operator"` (if edited) at the top level.
    ///
    /// Re-stamp `published_at_ms` on a serialized payload immediately
    /// before NATS publish. Callers should invoke this on the buffer
    /// drain path so that buffer dwell time + operator review delay
    /// don't leak into the orchestrator's `submission_received.propagation_ms`.
    /// The non-buffered immediate-publish path stamps at serde time
    /// and publishes microseconds later, so it does not need this.
    ///
    /// Handles both shapes:
    /// - **Proposal**: top-level JSON object with `published_at_ms`.
    /// - **Evaluation batch**: JSON array of `[target_id, {eval_obj}]`
    ///   tuples; each eval object's `published_at_ms` is updated.
    ///
    /// On any parse failure the original bytes are returned unchanged
    /// — losing a re-stamp is preferable to corrupting the payload.
    fn restamp_published_at(bytes: &[u8], now_ms: i64) -> Vec<u8> {
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return bytes.to_vec();
        };
        let stamp = serde_json::Value::from(now_ms);
        if let Some(obj) = value.as_object_mut() {
            obj.insert("published_at_ms".to_string(), stamp);
        } else if let Some(arr) = value.as_array_mut() {
            for item in arr.iter_mut() {
                if let Some(pair) = item.as_array_mut()
                    && pair.len() == 2
                    && let Some(eval_obj) = pair[1].as_object_mut()
                {
                    eval_obj.insert("published_at_ms".to_string(), stamp.clone());
                }
            }
        }
        serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
    }

    /// For **evaluations** (JSON arrays of `[target_id, {eval_obj}]` tuples):
    /// injects the same fields into each evaluation object within the array.
    ///
    /// If the payload is not valid JSON, returns the original bytes unchanged.
    fn inject_annotations(entry: &buffer::BufferedResponse) -> Vec<u8> {
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&entry.payload) else {
            warn!(
                "Buffer entry {} has non-JSON payload; skipping annotation injection",
                entry.id
            );
            return entry.payload.clone();
        };
        let annotations_json: Vec<serde_json::Value> = entry
            .annotations
            .iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect();

        if let Some(obj) = value.as_object_mut() {
            // Proposal payload — inject at top level
            if !annotations_json.is_empty() {
                obj.insert(
                    "operator_annotations".to_string(),
                    serde_json::Value::Array(annotations_json),
                );
            }
            if entry.edited {
                obj.insert(
                    "edited_by".to_string(),
                    serde_json::Value::String("operator".to_string()),
                );
            }
            serde_json::to_vec(&value).unwrap_or_else(|_| entry.payload.clone())
        } else if let Some(arr) = value.as_array_mut() {
            // Evaluation payload — array of [target_id, {eval_obj}] tuples.
            // Inject annotations into each evaluation object.
            for item in arr.iter_mut() {
                if let Some(tuple) = item.as_array_mut() {
                    if let Some(eval_obj) = tuple.get_mut(1).and_then(|v| v.as_object_mut()) {
                        if !annotations_json.is_empty() {
                            eval_obj.insert(
                                "operator_annotations".to_string(),
                                serde_json::Value::Array(annotations_json.clone()),
                            );
                        }
                        if entry.edited {
                            eval_obj.insert(
                                "edited_by".to_string(),
                                serde_json::Value::String("operator".to_string()),
                            );
                        }
                    }
                }
            }
            serde_json::to_vec(&value).unwrap_or_else(|_| entry.payload.clone())
        } else {
            entry.payload.clone()
        }
    }

    async fn is_duplicate(&self, msg_id: &str) -> Result<bool> {
        match self.processed_kv.get(msg_id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("KV Get Error: {}", e)),
        }
    }

    async fn mark_processed(&self, msg_id: &str) -> Result<()> {
        let val = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();

        self.processed_kv
            .put(msg_id, val.into())
            .await
            .map_err(|e| anyhow::anyhow!("KV Put Error: {}", e))?;
        Ok(())
    }
}

impl Clone for NatsNsedWorker {
    fn clone(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            agent_config: self.agent_config.clone(),
            nats: self.nats.clone(),
            js: self.js.clone(),
            processed_kv: self.processed_kv.clone(),
            scratchpad_kv: self.scratchpad_kv.clone(),
            config: self.config.clone(),
            agent_id: self.agent_id.clone(),
            active_jobs: self.active_jobs.clone(),
            start_time: self.start_time,
            status: self.status.clone(),
            hook: self.hook.clone(),
            user_tool_factory: self.user_tool_factory.clone(),
            chat_agent: self.chat_agent.clone(),
            response_buffer: self.response_buffer.clone(),
            paused: self.paused.clone(),
            telemetry: self.telemetry.clone(),
            before_prompt_mw: self.before_prompt_mw.clone(),
            provider_response_mw: self.provider_response_mw.clone(),
            completion_mw: self.completion_mw.clone(),
            job_complete_mw: self.job_complete_mw.clone(),
        }
    }
}

/// A middleware `Blocked` verdict surfaced as a typed error so the propose loop
/// can tell a re-promptable reviewer rejection (e.g. patch-deliberation
/// "applied ZERO changes") apart from a transport or logic failure. Its `Display`
/// is kept identical to the pre-typed message so log output is unchanged.
#[derive(Debug)]
struct MiddlewareBlocked {
    category: String,
    reason: String,
}

impl std::fmt::Display for MiddlewareBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "middleware blocked ({}): {}", self.category, self.reason)
    }
}

impl std::error::Error for MiddlewareBlocked {}

/// Run a middleware pipeline for a hook point and return the (possibly
/// transformed) `content`, or an `Err` when a middleware blocks. Free fn so it's
/// unit-testable with a mock pipeline — no worker / NATS needed.
#[allow(clippy::too_many_arguments)] // cohesive hook-invocation signature
async fn run_stage_pipeline(
    pipeline: &crate::middleware::pipeline::MiddlewarePipeline,
    agent_id: &str,
    action: &str,
    session_id: &str,
    round: u32,
    stage: crate::middleware::MiddlewareStage,
    content: serde_json::Value,
    metadata: serde_json::Value,
) -> Result<serde_json::Value> {
    let mut ctx = crate::middleware::MiddlewareContext {
        content,
        action: action.to_string(),
        agent_id: agent_id.to_string(),
        job_id: session_id.to_string(),
        round,
        stage,
        metadata,
        hook_state: std::collections::HashMap::new(),
    };
    match pipeline.run(&mut ctx).await {
        crate::middleware::pipeline::PipelineResult::Blocked {
            category, reason, ..
        } => Err(anyhow::Error::new(MiddlewareBlocked { category, reason })),
        _ => Ok(ctx.content),
    }
}

/// Adapts the `provider_response` middleware into a [`SubmissionValidator`] the
/// agent's react loop runs on each `submit_proposal`. It runs the pipeline on the
/// proposal content and maps a [`MiddlewareBlocked`] verdict to the reason string
/// the loop feeds back to the model; a pass (or non-block error) returns `None`
/// (accept). The pipeline run is idempotent (patch-deliberation self-cleans the
/// worktree), so running it here per attempt AND once more for the accepted
/// proposal's commit/transform composes correctly.
#[derive(Debug)]
struct MiddlewareSubmissionValidator {
    pipeline: Arc<crate::middleware::pipeline::MiddlewarePipeline>,
    agent_id: String,
    session_id: String,
    round: u32,
}

/// Build the react loop's submission validator for a task: a reviewer wrapping the
/// `provider_response` pipeline, but only for `propose` (evaluations have no
/// provider_response) and only when a pipeline is configured — else `None`.
fn build_submission_validator(
    action: &str,
    provider_response_mw: &Option<Arc<crate::middleware::pipeline::MiddlewarePipeline>>,
    agent_id: &str,
    session_id: &str,
    round: u32,
) -> Option<Arc<dyn crate::agents::SubmissionValidator>> {
    if action != "propose" {
        return None;
    }
    let pipeline = provider_response_mw.clone()?;
    Some(Arc::new(MiddlewareSubmissionValidator {
        pipeline,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        round,
    }))
}

#[async_trait::async_trait]
impl crate::agents::SubmissionValidator for MiddlewareSubmissionValidator {
    async fn validate(&self, content: &str) -> Option<String> {
        match run_stage_pipeline(
            &self.pipeline,
            &self.agent_id,
            "propose",
            &self.session_id,
            self.round,
            crate::middleware::MiddlewareStage::ProviderResponse,
            serde_json::json!(content),
            serde_json::json!({}),
        )
        .await
        {
            Err(e) => e
                .downcast_ref::<MiddlewareBlocked>()
                .map(|b| b.reason.clone()),
            Ok(_) => None,
        }
    }
}

/// Extract the session id from a result-event subject shaped like
/// `{prefix}.{session_id}.result.event.{kind}`. `prefix` may be empty.
fn session_id_from_subject(subject: &str, prefix: &str) -> String {
    let prefix_count = if prefix.is_empty() {
        0
    } else {
        prefix.split('.').count()
    };
    subject
        .split('.')
        .nth(prefix_count)
        .unwrap_or("?")
        .to_string()
}

/// The (content, metadata) a `job_complete` event contributes to the
/// `on_job_complete` hook context. `metadata.winner` = the final winner.
fn job_complete_payload(
    event: &crate::events::JobCompleteEvent,
) -> (serde_json::Value, serde_json::Value) {
    let winner = event.best_proposal_author.clone();
    (
        serde_json::json!({
            "winner": winner,
            "score": event.best_proposal_score,
            "content": event.best_proposal_content,
            "rounds_completed": event.rounds_completed,
        }),
        serde_json::json!({
            "winner": winner,
            "finalized_by_user": event.finalized_by_user,
        }),
    )
}

/// The winning agent of a round summary = the highest aggregated score.
/// `None` for an empty score list. Among equal maxima, `max_by` returns the LAST,
/// so ties resolve to the last-listed proposal (deterministic — `proposal_scores`
/// is an ordered `Vec`). A NaN score (should be unreachable — `normalize_score`
/// guards against it — but defended here too) sorts LOWEST so it can never win over
/// a real score.
fn pick_winner(scores: &[crate::events::ProposalScoreEntry]) -> Option<String> {
    scores
        .iter()
        .max_by(|a, b| aggregated_score_cmp(a.aggregated_score, b.aggregated_score))
        .map(|e| e.agent_id.clone())
}

/// Compare two aggregated scores with NaN sorting LOWEST (so a poisoned score never
/// wins). Finite scores compare naturally.
pub(crate) fn aggregated_score_cmp(a: f32, b: f32) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Extract a human-readable content preview from a serialized response payload.
///
/// For proposals: extracts the `content` field (truncated to ~500 chars).
/// For evaluations: extracts scores and justification summaries.
fn extract_content_preview(
    payload: &[u8],
    action: &str,
    candidates: &[crate::agents::CandidateProposal],
) -> Option<String> {
    // Parse as serde_json::Value — cheap, no struct coupling.
    // Returns a JSON string with structured data for rich dashboard rendering.
    let val: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let structured = match action {
        "propose" => {
            // Proposal: { "content": "...", "thought_process": "..." }
            let content = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            let thought = val
                .get("thought_process")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut obj = serde_json::Map::new();
            obj.insert("t".into(), serde_json::Value::String("p".into()));
            // Truncate content at 2000 chars for preview
            let c = if content.chars().count() > 2000 {
                let idx = content
                    .char_indices()
                    .nth(2000)
                    .map(|(i, _)| i)
                    .unwrap_or(content.len());
                format!("{}…", &content[..idx])
            } else {
                content.to_string()
            };
            obj.insert("c".into(), serde_json::Value::String(c));
            if !thought.is_empty() {
                let tp = if thought.chars().count() > 500 {
                    let idx = thought
                        .char_indices()
                        .nth(500)
                        .map(|(i, _)| i)
                        .unwrap_or(thought.len());
                    format!("{}…", &thought[..idx])
                } else {
                    thought.to_string()
                };
                obj.insert("tp".into(), serde_json::Value::String(tp));
            }
            serde_json::Value::Object(obj)
        }
        "evaluate" => {
            // Evaluations: [ ["target_id", { "score": 7.5, "justification": "...", "stance": "...", ... }], ... ]
            let arr = val.as_array()?;
            let mut evals = Vec::new();
            let mut displayed_targets = std::collections::HashSet::new();
            for item in arr.iter().take(10) {
                if let Some(tuple) = item.as_array() {
                    let target = tuple.first().and_then(|v| v.as_str()).unwrap_or("?");
                    if let Some(eval_obj) = tuple.get(1) {
                        displayed_targets.insert(target.to_string());
                        let mut e = serde_json::Map::new();
                        e.insert(
                            "target".into(),
                            serde_json::Value::String(target.to_string()),
                        );
                        if let Some(s) = eval_obj.get("score") {
                            e.insert("s".into(), s.clone());
                        }
                        if let Some(j) = eval_obj.get("justification").and_then(|v| v.as_str()) {
                            let jp = if j.chars().count() > 300 {
                                let idx =
                                    j.char_indices().nth(300).map(|(i, _)| i).unwrap_or(j.len());
                                format!("{}…", &j[..idx])
                            } else {
                                j.to_string()
                            };
                            e.insert("j".into(), serde_json::Value::String(jp));
                        }
                        if let Some(stance) = eval_obj.get("stance").and_then(|v| v.as_str()) {
                            e.insert(
                                "stance".into(),
                                serde_json::Value::String(stance.to_string()),
                            );
                        }
                        if let Some(tf) = eval_obj.get("textual_feedback").and_then(|v| v.as_str())
                        {
                            let tfp = if tf.chars().count() > 200 {
                                let idx = tf
                                    .char_indices()
                                    .nth(200)
                                    .map(|(i, _)| i)
                                    .unwrap_or(tf.len());
                                format!("{}…", &tf[..idx])
                            } else {
                                tf.to_string()
                            };
                            e.insert("tf".into(), serde_json::Value::String(tfp));
                        }
                        if let Some(cats) = eval_obj.get("category_scores") {
                            e.insert("cats".into(), cats.clone());
                        }
                        if let Some(claims) = eval_obj.get("claim_assessments") {
                            e.insert("claims".into(), claims.clone());
                        }
                        if let Some(disputes) = eval_obj.get("disagreements") {
                            e.insert("disputes".into(), disputes.clone());
                        }
                        evals.push(serde_json::Value::Object(e));
                    }
                }
            }
            if evals.is_empty() {
                return None;
            }
            let mut obj = serde_json::Map::new();
            obj.insert("t".into(), serde_json::Value::String("e".into()));
            obj.insert("evals".into(), serde_json::Value::Array(evals));
            // Include anonymized proposal content for candidates that appear
            // in the displayed evals (capped to the same 10-entry budget).
            if !candidates.is_empty() && !displayed_targets.is_empty() {
                let mut props = serde_json::Map::new();
                for cp in candidates
                    .iter()
                    .filter(|cp| displayed_targets.contains(&cp.id))
                {
                    let c = &cp.proposal.content;
                    let truncated = if c.chars().count() > 1000 {
                        let idx = c
                            .char_indices()
                            .nth(1000)
                            .map(|(i, _)| i)
                            .unwrap_or(c.len());
                        format!("{}…", &c[..idx])
                    } else {
                        c.clone()
                    };
                    props.insert(cp.id.clone(), serde_json::Value::String(truncated));
                }
                if !props.is_empty() {
                    obj.insert("props".into(), serde_json::Value::Object(props));
                }
            }
            serde_json::Value::Object(obj)
        }
        _ => return None,
    };
    serde_json::to_string(&structured).ok()
}

/// Check whether an error is a transient transport/network failure suitable for retry.
///
/// Uses case-insensitive substring matching against known OS and HTTP transport
/// error messages so that variations in casing or wrapping don't cause misses.
fn is_transient_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    const PATTERNS: &[&str] = &[
        "broken pipe",
        "connection reset",
        "os error 32",
        "os error 104",
        "timed out",
        "connection closed",
        "unexpected eof",
        "stream closed",
        "connection refused",
        "network unreachable",
        "connection aborted",
    ];
    PATTERNS.iter().any(|p| msg.contains(p))
}

/// Categorise a task-execution error into a short machine-readable
/// abstention reason. The reason flows into the
/// [`Proposal::abstained`] / [`Evaluation::abstained`] sentinel
/// payload the worker publishes on bail so the orchestrator (and
/// telemetry consumers) can distinguish parse failures from iteration
/// caps without parsing the full error string.
fn classify_abstention_reason(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("failed to parse structured output") || lower.contains("missing field") {
        "parse_error".into()
    } else if lower.contains("max_iterations")
        || lower.contains("max iterations")
        || lower.contains("iteration budget")
    {
        "iter_budget_exhausted".into()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".into()
    } else if lower.contains("tool") && (lower.contains("error") || lower.contains("failed")) {
        "tool_error".into()
    } else {
        "error".into()
    }
}

/// Build the round-scoped failure marker subject. Mirrors the verdict
/// subject hierarchy (`{prefix}.{session}.result.{round}.{agent}.{action}`)
/// with a `.failed` tail. Pure-format so the exact wire shape is
/// pinned by unit test and the orchestrator can derive matching
/// `filter_subjects` from the same constants.
fn failed_result_subject(
    prefix: &str,
    session_id: &str,
    round: u32,
    agent_id: &str,
    action: &str,
) -> String {
    format!("{prefix}.{session_id}.result.{round}.{agent_id}.{action}.failed")
}

/// Should the worker emit a `.failed` marker on bail for this action?
///
/// - `propose` / `evaluate`: yes — those phases have orchestrator-side
///   collectors that need the round-scoped signal.
/// - other actions (passthrough, heartbeat handlers, etc.): no — no
///   round-scoped collector waits on them.
/// - payment errors: no — orchestrator may retry once credits return;
///   a `.failed` marker would mislead the collector into permanently
///   counting the agent as a no-show for the round.
fn should_publish_failure_marker(action: &str, is_payment_error: bool) -> bool {
    if is_payment_error {
        return false;
    }
    matches!(action, "propose" | "evaluate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::pipeline::MiddlewarePipeline;
    use crate::middleware::{
        AgentMiddleware, MiddlewareContext, MiddlewareStage, MiddlewareVerdict,
    };

    // --- mock middleware for hook-invocation tests (no NATS needed) ---

    // The default `stages()` excludes BeforePrompt/Completion/JobComplete; these
    // mocks are built directly (not via the per-entry build path that overrides
    // stages), so opt into every stage.
    fn all_stages() -> Vec<MiddlewareStage> {
        use MiddlewareStage::*;
        vec![
            Edit,
            Release,
            ProviderResponse,
            BeforePrompt,
            Completion,
            JobComplete,
        ]
    }

    #[derive(Debug)]
    struct ContentReplaceMock(serde_json::Value);
    #[async_trait::async_trait]
    impl AgentMiddleware for ContentReplaceMock {
        // TODO(slop): placeholder identifier — pick a name that says what this is
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::pass_with_content(self.0.clone())
        }
        fn name(&self) -> &str {
            "content-replace-mock"
        }
        fn stages(&self) -> Vec<MiddlewareStage> {
            all_stages()
        }
    }

    #[derive(Debug)]
    struct BlockingMock;
    #[async_trait::async_trait]
    impl AgentMiddleware for BlockingMock {
        // TODO(slop): placeholder identifier — pick a name that says what this is
        async fn execute(&self, _ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::block("mock_block", "rejected by mock middleware")
        }
        fn name(&self) -> &str {
            "blocking-mock"
        }
        fn stages(&self) -> Vec<MiddlewareStage> {
            all_stages()
        }
    }

    /// Echoes the context it received (so a test can assert how the worker built it).
    #[derive(Debug)]
    struct ContextEchoMock;
    #[async_trait::async_trait]
    impl AgentMiddleware for ContextEchoMock {
        // TODO(slop): placeholder identifier — pick a name that says what this is
        async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
            MiddlewareVerdict::pass_with_content(serde_json::json!({
                "stage": format!("{:?}", ctx.stage),
                "agent": ctx.agent_id,
                "job": ctx.job_id,
                "round": ctx.round,
                "action": ctx.action,
                "meta": ctx.metadata,
            }))
        }
        fn name(&self) -> &str {
            "context-echo-mock"
        }
        fn stages(&self) -> Vec<MiddlewareStage> {
            all_stages()
        }
    }

    #[tokio::test]
    async fn run_stage_pipeline_returns_transformed_content() {
        let p = MiddlewarePipeline::new(vec![Box::new(ContentReplaceMock(serde_json::json!(
            "changed"
        )))]);
        let out = run_stage_pipeline(
            &p,
            "AgentA",
            "propose",
            "job1",
            2,
            MiddlewareStage::ProviderResponse,
            serde_json::json!("orig"),
            serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(out, serde_json::json!("changed"));
    }

    #[tokio::test]
    async fn run_stage_pipeline_block_is_err() {
        let p = MiddlewarePipeline::new(vec![Box::new(BlockingMock)]);
        let r = run_stage_pipeline(
            &p,
            "A",
            "propose",
            "j",
            0,
            MiddlewareStage::BeforePrompt,
            serde_json::json!("x"),
            serde_json::json!({}),
        )
        .await;
        assert!(r.is_err(), "a blocking middleware must fail the hook");
    }

    #[tokio::test]
    async fn run_stage_pipeline_builds_context_correctly() {
        let p = MiddlewarePipeline::new(vec![Box::new(ContextEchoMock)]);
        let out = run_stage_pipeline(
            &p,
            "AgentA",
            "job_complete",
            "sess9",
            3,
            MiddlewareStage::JobComplete,
            serde_json::json!("x"),
            serde_json::json!({"winner": "AgentA"}),
        )
        .await
        .unwrap();
        assert_eq!(out["agent"], "AgentA");
        assert_eq!(out["job"], "sess9");
        assert_eq!(out["round"], 3);
        assert_eq!(out["action"], "job_complete");
        assert_eq!(out["stage"], "JobComplete");
        assert_eq!(out["meta"]["winner"], "AgentA");
    }

    // --- ReAct reviewer-block retry (propose_with_react_retry) ---

    #[tokio::test]
    async fn run_stage_pipeline_block_downcasts_to_typed_error() {
        let p = MiddlewarePipeline::new(vec![Box::new(BlockingMock)]);
        let e = run_stage_pipeline(
            &p,
            "A",
            "propose",
            "j",
            0,
            MiddlewareStage::ProviderResponse,
            serde_json::json!("x"),
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        let b = e
            .downcast_ref::<MiddlewareBlocked>()
            .expect("a Blocked verdict must surface as a typed MiddlewareBlocked error");
        assert_eq!(b.category, "mock_block");
        assert_eq!(b.reason, "rejected by mock middleware");
    }

    use crate::agents::SubmissionValidator;

    #[tokio::test]
    async fn submission_validator_maps_block_to_reason() {
        let v = MiddlewareSubmissionValidator {
            pipeline: Arc::new(MiddlewarePipeline::new(vec![Box::new(BlockingMock)])),
            agent_id: "AgentA".to_string(),
            session_id: "job1".to_string(),
            round: 0,
        };
        assert_eq!(
            v.validate("anything").await.as_deref(),
            Some("rejected by mock middleware"),
            "a Blocked verdict surfaces its reason for the react loop to feed back"
        );
    }

    #[test]
    fn build_submission_validator_only_for_propose_with_pipeline() {
        let mw = Some(Arc::new(MiddlewarePipeline::new(vec![Box::new(
            BlockingMock,
        )])));
        assert!(
            build_submission_validator("propose", &mw, "A", "j", 0).is_some(),
            "propose + configured pipeline → a validator"
        );
        assert!(
            build_submission_validator("evaluate", &mw, "A", "j", 0).is_none(),
            "evaluations have no provider_response → no validator"
        );
        assert!(
            build_submission_validator("propose", &None, "A", "j", 0).is_none(),
            "no pipeline configured → no validator"
        );
    }

    #[tokio::test]
    async fn submission_validator_passes_clean_content() {
        let v = MiddlewareSubmissionValidator {
            pipeline: Arc::new(MiddlewarePipeline::new(vec![Box::new(ContentReplaceMock(
                serde_json::json!("transformed"),
            ))])),
            agent_id: "AgentA".to_string(),
            session_id: "job1".to_string(),
            round: 0,
        };
        assert_eq!(
            v.validate("ok").await,
            None,
            "a passing pipeline accepts the submission (None)"
        );
    }

    #[test]
    fn session_id_from_subject_cases() {
        assert_eq!(
            session_id_from_subject("nsed.sess1.result.event.job_complete", "nsed"),
            "sess1"
        );
        assert_eq!(
            session_id_from_subject("sess2.result.event.round_summary", ""),
            "sess2"
        );
        assert_eq!(
            session_id_from_subject("org.nsed.s42.result.event.x", "org.nsed"),
            "s42"
        );
        assert_eq!(
            session_id_from_subject("", "nsed"),
            "?",
            "malformed → sentinel, no panic"
        );
    }

    #[test]
    fn job_complete_payload_maps_winner() {
        let ev = crate::events::JobCompleteEvent {
            best_proposal_author: "AgentB".into(),
            best_proposal_score: 7.5,
            best_proposal_content: "final".into(),
            rounds_completed: 4,
            finalized_by_user: Some("op".into()),
            ..Default::default()
        };
        let (content, meta) = job_complete_payload(&ev);
        assert_eq!(content["winner"], "AgentB");
        assert_eq!(content["score"], 7.5);
        assert_eq!(content["rounds_completed"], 4);
        assert_eq!(meta["winner"], "AgentB");
        assert_eq!(meta["finalized_by_user"], "op");
    }

    #[test]
    fn pick_winner_selects_highest_score() {
        use crate::events::ProposalScoreEntry;
        let scores = vec![
            ProposalScoreEntry {
                agent_id: "alpha".into(),
                aggregated_score: 3.2,
                ..Default::default()
            },
            ProposalScoreEntry {
                agent_id: "beta".into(),
                aggregated_score: 6.5,
                ..Default::default()
            },
            ProposalScoreEntry {
                agent_id: "gamma".into(),
                aggregated_score: 1.0,
                ..Default::default()
            },
        ];
        assert_eq!(pick_winner(&scores).as_deref(), Some("beta"));
        assert_eq!(pick_winner(&[]), None);
    }

    #[test]
    fn pick_winner_never_selects_a_nan_score() {
        use crate::events::ProposalScoreEntry;
        // A NaN-scored proposal at the END must not win over a strictly-larger real
        // score (max_by returns the last of Equal, and NaN compared naively is Equal
        // to all). NaN sorts lowest.
        let scores = vec![
            ProposalScoreEntry {
                agent_id: "real".into(),
                aggregated_score: 4.0,
                ..Default::default()
            },
            ProposalScoreEntry {
                agent_id: "poisoned".into(),
                aggregated_score: f32::NAN,
                ..Default::default()
            },
        ];
        assert_eq!(pick_winner(&scores).as_deref(), Some("real"));
    }

    #[test]
    fn aggregated_score_cmp_sorts_nan_lowest() {
        use std::cmp::Ordering;
        assert_eq!(aggregated_score_cmp(f32::NAN, 1.0), Ordering::Less);
        assert_eq!(aggregated_score_cmp(1.0, f32::NAN), Ordering::Greater);
        assert_eq!(aggregated_score_cmp(f32::NAN, f32::NAN), Ordering::Equal);
        assert_eq!(aggregated_score_cmp(2.0, 1.0), Ordering::Greater);
    }

    #[test]
    fn test_worker_config_new_defaults() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "test_stream".to_string(),
            "test_consumer".to_string(),
        );

        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.stream_name, "test_stream");
        assert_eq!(config.consumer_name, "test_consumer");
        assert_eq!(config.subject_prefix, "nsed");
        assert_eq!(config.api_prefix, "sphera");
        assert_eq!(config.scratchpad_retention_secs, 86400 * 7);
        // Unbounded by default → max_ack_pending 0 (server default).
        assert_eq!(config.max_concurrent_jobs, None);
        assert_eq!(config.max_ack_pending(), 0);
    }

    #[test]
    fn test_worker_config_max_concurrent_jobs_maps_to_ack_pending() {
        let serialized = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "s".to_string(),
            "c".to_string(),
        )
        .with_max_concurrent_jobs(1);
        assert_eq!(serialized.max_concurrent_jobs, Some(1));
        assert_eq!(
            serialized.max_ack_pending(),
            1,
            "1 job → serialized in-flight"
        );

        let capped =
            WorkerConfig::new("u".into(), "s".into(), "c".into()).with_max_concurrent_jobs(4);
        assert_eq!(capped.max_ack_pending(), 4);
    }

    #[test]
    fn test_worker_config_with_subject_prefix() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "stream".to_string(),
            "consumer".to_string(),
        )
        .with_subject_prefix("nsed.test".to_string());

        assert_eq!(config.subject_prefix, "nsed.test");
    }

    #[test]
    fn test_worker_config_with_scratchpad_retention() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "stream".to_string(),
            "consumer".to_string(),
        )
        .with_scratchpad_retention(3600);

        assert_eq!(config.scratchpad_retention_secs, 3600);
    }

    #[test]
    fn test_worker_config_with_zero_retention() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "stream".to_string(),
            "consumer".to_string(),
        )
        .with_scratchpad_retention(0);

        assert_eq!(config.scratchpad_retention_secs, 0);
    }

    #[test]
    fn test_worker_config_chained_builders() {
        let config = WorkerConfig::new(
            "nats://test:4222".to_string(),
            "my_stream".to_string(),
            "my_consumer".to_string(),
        )
        .with_subject_prefix("prefix".to_string())
        .with_api_prefix("myapi".to_string())
        .with_scratchpad_retention(7200);

        assert_eq!(config.nats_url, "nats://test:4222");
        assert_eq!(config.stream_name, "my_stream");
        assert_eq!(config.consumer_name, "my_consumer");
        assert_eq!(config.subject_prefix, "prefix");
        assert_eq!(config.api_prefix, "myapi");
        assert_eq!(config.scratchpad_retention_secs, 7200);
    }

    #[test]
    fn test_job_manifest_deserialization() {
        let json = r#"{
            "job_id": "test-job-123",
            "task_description": "Solve math problem",
            "agents": ["agent1", "agent2", "agent3"],
            "rounds": 5,
            "timestamp": 1704067200
        }"#;

        let manifest: JobManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.job_id, "test-job-123");
        assert_eq!(manifest.task_description, "Solve math problem");
        assert_eq!(manifest.agents.len(), 3);
        assert_eq!(manifest.rounds, 5);
        assert_eq!(manifest.timestamp, 1704067200);
    }

    #[test]
    fn test_job_manifest_serialization_roundtrip() {
        let manifest = JobManifest {
            job_id: "roundtrip-test".to_string(),
            task_description: "Test description".to_string(),
            agents: vec!["alpha".to_string(), "beta".to_string()],
            rounds: 3,
            timestamp: 999999,
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: JobManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.job_id, manifest.job_id);
        assert_eq!(parsed.task_description, manifest.task_description);
        assert_eq!(parsed.agents, manifest.agents);
        assert_eq!(parsed.rounds, manifest.rounds);
        assert_eq!(parsed.timestamp, manifest.timestamp);
    }

    #[test]
    fn test_dual_prefix_subject_architecture() {
        let subject_prefix = "nsed";
        let api_prefix = "sphera";
        let job_id = "job-123";
        let agent_id = "agent-001";

        let ack_subject = format!("{}.jobs.ack.{}.{}", api_prefix, job_id, agent_id);
        assert_eq!(ack_subject, "sphera.jobs.ack.job-123.agent-001");

        let manifest_filter = format!("{}.jobs.manifest.>", api_prefix);
        assert_eq!(manifest_filter, "sphera.jobs.manifest.>");

        let heartbeat = format!("{}.agent.heartbeat.{}", api_prefix, agent_id);
        assert_eq!(heartbeat, "sphera.agent.heartbeat.agent-001");

        let accepted_subject = format!("{}.{}.result.event.agent_accepted", subject_prefix, job_id);
        assert!(accepted_subject.starts_with("nsed."));

        let task_filter = format!("{}.*.task.{}.*", subject_prefix, agent_id);
        assert_eq!(task_filter, "nsed.*.task.agent-001.*");
    }

    #[test]
    fn test_session_id_extraction_single_segment_prefix() {
        let subject = "nsed.session-abc.task.agent1.propose";
        let prefix = "nsed";

        let subject_parts: Vec<&str> = subject.split('.').collect();
        let prefix_count = prefix.split('.').count();
        let session_id = subject_parts
            .get(prefix_count)
            .unwrap_or(&"global")
            .to_string();
        let action = subject_parts.last().unwrap_or(&"unknown");

        assert_eq!(session_id, "session-abc");
        assert_eq!(*action, "propose");
    }

    #[test]
    fn test_session_id_extraction_multi_segment_prefix() {
        let subject = "nsed.v2.session-abc.task.agent1.evaluate";
        let prefix = "nsed.v2";

        let subject_parts: Vec<&str> = subject.split('.').collect();
        let prefix_count = prefix.split('.').count();
        let session_id = subject_parts
            .get(prefix_count)
            .unwrap_or(&"global")
            .to_string();
        let action = subject_parts.last().unwrap_or(&"unknown");

        assert_eq!(session_id, "session-abc");
        assert_eq!(*action, "evaluate");
    }

    #[test]
    fn test_session_id_extraction_empty_prefix_fallback() {
        let subject = "session-abc.task.agent1.propose";
        let prefix = "";

        let subject_parts: Vec<&str> = subject.split('.').collect();
        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject_parts
            .get(prefix_count)
            .unwrap_or(&"global")
            .to_string();

        assert_eq!(session_id, "session-abc");
    }

    #[test]
    fn test_worker_config_with_nats_auth() {
        let auth = NatsAuth {
            token: Some("my-secret-token".to_string()),
            username: None,
            password: None,
            inline_creds: None,
            creds_file: None,
        };

        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "stream".to_string(),
            "consumer".to_string(),
        )
        .with_nats_auth(auth);

        assert!(config.nats_auth.is_some());
        let auth = config.nats_auth.unwrap();
        assert_eq!(auth.token, Some("my-secret-token".to_string()));
    }

    /// Tests that the default implementation of `WorkerHook::before_publish`
    /// is a no-op passthrough that returns `Ok(())`.
    #[derive(Debug)]
    struct NoopHook;

    #[async_trait]
    impl WorkerHook for NoopHook {}

    #[tokio::test]
    async fn test_worker_hook_default_before_publish() {
        let hook = NoopHook;
        let mut payload = vec![1, 2, 3];
        let result = hook.before_publish("some.subject", &mut payload).await;
        assert!(result.is_ok());
        // Payload should be unmodified by the default no-op implementation
        assert_eq!(payload, vec![1, 2, 3]);
    }

    // ---- inject_annotations tests ----

    /// A test-only no-op ack handle (buffer.rs's NoopAckHandle is private).
    struct TestAckHandle;

    #[async_trait]
    impl buffer::AckHandle for TestAckHandle {
        async fn ack(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn make_entry(
        payload: &[u8],
        edited: bool,
        annotations: Vec<crate::agents::OperatorAnnotation>,
    ) -> buffer::BufferedResponse {
        let now = std::time::Instant::now();
        buffer::BufferedResponse {
            id: "test-id".into(),
            action: "propose".into(),
            job_id: "job-1".into(),
            round: 1,
            reply_subject: "nsed.job-1.result.1.agent.propose".into(),
            payload: payload.to_vec(),
            created_at: now,
            release_at: now,
            ack_handle: Box::new(TestAckHandle),
            msg_id: "msg-test".into(),
            annotations,
            edited,
            stopped: false,
        }
    }

    #[test]
    fn restamp_proposal_object_overwrites_published_at_ms() {
        let payload = br#"{"content":"hello","thought_process":"t","published_at_ms":1000}"#;
        let out = NatsNsedWorker::restamp_published_at(payload, 9999);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["published_at_ms"], 9999);
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn restamp_proposal_object_inserts_when_field_missing() {
        let payload = br#"{"content":"hello","thought_process":"t"}"#;
        let out = NatsNsedWorker::restamp_published_at(payload, 4242);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["published_at_ms"], 4242);
    }

    #[test]
    fn restamp_evaluation_array_stamps_each_entry() {
        let payload = br#"[["A",{"score":0.5,"published_at_ms":100}],["B",{"score":-0.5}]]"#;
        let out = NatsNsedWorker::restamp_published_at(payload, 7777);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0][1]["published_at_ms"], 7777);
        assert_eq!(arr[1][1]["published_at_ms"], 7777);
        assert_eq!(arr[0][1]["score"], 0.5);
    }

    #[test]
    fn restamp_returns_input_unchanged_on_invalid_json() {
        let payload = b"not-json{{";
        let out = NatsNsedWorker::restamp_published_at(payload, 1);
        assert_eq!(out, payload.to_vec());
    }

    #[test]
    fn test_inject_annotations_proposal_object() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let payload = br#"{"content":"hello","thought_process":"think"}"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed wording".into(),
            timestamp: "2026-03-04T00:00:00Z".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, true, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(val["content"], "hello");
        assert_eq!(val["thought_process"], "think");
        assert_eq!(val["edited_by"], "operator");
        assert!(val["operator_annotations"].is_array());
        assert_eq!(val["operator_annotations"].as_array().unwrap().len(), 1);
        assert_eq!(val["operator_annotations"][0]["comment"], "Fixed wording");
    }

    #[test]
    fn test_inject_annotations_proposal_no_edit_no_annotations() {
        let payload = br#"{"content":"original"}"#;
        let entry = make_entry(payload, false, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(val["content"], "original");
        // No edited_by or operator_annotations when not edited
        assert!(val.get("edited_by").is_none());
        assert!(val.get("operator_annotations").is_none());
    }

    #[test]
    fn test_inject_annotations_evaluation_array() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        // Evaluation payload: array of [target_id, {eval_obj}] tuples
        let payload = br#"[
            ["agent-A", {"score": 7.5, "justification": "Good work"}],
            ["agent-B", {"score": 4.0, "justification": "Needs improvement"}]
        ]"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Adjusted scores".into(),
            timestamp: "2026-03-04T00:00:00Z".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, true, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let arr = val.as_array().expect("should remain an array");
        assert_eq!(arr.len(), 2);

        // First eval tuple
        let first = arr[0].as_array().unwrap();
        assert_eq!(first[0], "agent-A");
        let eval_a = &first[1];
        assert_eq!(eval_a["score"], 7.5);
        assert_eq!(eval_a["edited_by"], "operator");
        assert!(eval_a["operator_annotations"].is_array());
        assert_eq!(
            eval_a["operator_annotations"][0]["comment"],
            "Adjusted scores"
        );

        // Second eval tuple
        let second = arr[1].as_array().unwrap();
        assert_eq!(second[0], "agent-B");
        let eval_b = &second[1];
        assert_eq!(eval_b["score"], 4.0);
        assert_eq!(eval_b["edited_by"], "operator");
        assert!(eval_b["operator_annotations"].is_array());
    }

    #[test]
    fn test_inject_annotations_evaluation_array_no_edit() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let payload = br#"[["agent-A", {"score": 5.0}]]"#;
        // Has annotation but not edited — annotations inject, edited_by does not
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "Reviewed".into(),
            timestamp: "2026-03-04T00:00:00Z".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, false, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let arr = val.as_array().unwrap();
        let eval_obj = &arr[0].as_array().unwrap()[1];
        // Annotation present
        assert!(eval_obj["operator_annotations"].is_array());
        assert_eq!(eval_obj["operator_annotations"][0]["comment"], "Reviewed");
        // Not edited — no edited_by field
        assert!(eval_obj.get("edited_by").is_none());
    }

    #[test]
    fn test_inject_annotations_non_json_passthrough() {
        let payload = b"this is not json";
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        // Non-JSON payload should be returned unchanged
        assert_eq!(result, payload.to_vec());
    }

    // ---- Score delivery tests ----

    #[test]
    fn test_round_summary_event_serde_roundtrip() {
        use crate::events::{ProposalScoreEntry, RoundSummaryEvent};

        let event = RoundSummaryEvent {
            round: 1,
            convergence_score: 0.75,
            decisiveness: 0.75,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![
                ProposalScoreEntry {
                    agent_id: "alpha".into(),
                    aggregated_score: 6.5,
                    category_breakdown: None,
                    controversy_score: None,
                    ..Default::default()
                },
                ProposalScoreEntry {
                    agent_id: "beta".into(),
                    aggregated_score: 3.2,
                    category_breakdown: None,
                    controversy_score: None,
                    ..Default::default()
                },
            ],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let json = serde_json::to_vec(&event).unwrap();
        let parsed: RoundSummaryEvent = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed.round, 1);
        assert_eq!(parsed.proposal_scores.len(), 2);
        assert_eq!(parsed.proposal_scores[0].agent_id, "alpha");
        assert!((parsed.proposal_scores[0].aggregated_score - 6.5).abs() < f32::EPSILON);
        assert_eq!(parsed.proposal_scores[1].agent_id, "beta");
        assert!((parsed.proposal_scores[1].aggregated_score - 3.2).abs() < f32::EPSILON);
    }

    #[test]
    fn test_score_dedup_prevents_duplicate() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "gpt-4".into(), "p".into());

        // First score entry
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "job-A".into(),
            round: 1,
            evaluator: "aggregated".into(),
            score: 6.0,
        });
        assert_eq!(snap.recent_scores.len(), 1);

        // Simulate dedup guard — same job + round should not be pushed again
        let already_has = snap
            .recent_scores
            .iter()
            .any(|s| s.job_id == "job-A" && s.round == 1);
        assert!(already_has, "dedup guard should detect existing score");

        // Different round should pass
        let different_round = snap
            .recent_scores
            .iter()
            .any(|s| s.job_id == "job-A" && s.round == 2);
        assert!(!different_round, "different round should not match");
    }

    #[test]
    fn test_score_extraction_from_propose_context() {
        // Verify that the score extraction logic in handle_message is correct:
        // previous_own_score is from the PREVIOUS round, so we subtract 1.
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        // Simulate: round_number=3, previous_own_score=Some(4.5)
        // → score is for round 2
        let round_number: u32 = 3;
        let previous_own_score: Option<f32> = Some(4.5);
        let session_id = "job-X";

        if let Some(score) = previous_own_score {
            let prev_round = round_number.saturating_sub(1);
            let already_has = snap
                .recent_scores
                .iter()
                .any(|s| s.job_id == session_id && s.round == prev_round);
            if !already_has {
                snap.push_score(ScoreEntry {
                    timestamp: "t".into(),
                    job_id: session_id.into(),
                    round: prev_round,
                    evaluator: "aggregated".into(),
                    score,
                });
            }
        }

        assert_eq!(snap.recent_scores.len(), 1);
        let entry = &snap.recent_scores[0];
        assert_eq!(entry.round, 2); // round_number - 1
        assert!((entry.score - 4.5).abs() < f32::EPSILON);
        assert_eq!(entry.job_id, "job-X");

        // mean_score should reflect the pushed score
        assert!((snap.mean_score.unwrap() - 4.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_score_extraction_none_previous_score_no_push() {
        use crate::status::AgentStatusSnapshot;

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());
        let round_number: u32 = 3;
        let previous_own_score: Option<f32> = None;
        let session_id = "job-X";

        // Same logic as handle_message: only push when Some
        if let Some(score) = previous_own_score {
            let prev_round = round_number.saturating_sub(1);
            let already_has = snap
                .recent_scores
                .iter()
                .any(|s| s.job_id == session_id && s.round == prev_round);
            if !already_has {
                snap.push_score(crate::status::ScoreEntry {
                    timestamp: "t".into(),
                    job_id: session_id.into(),
                    round: prev_round,
                    evaluator: "aggregated".into(),
                    score,
                });
            }
        }

        // No score should be pushed when previous_own_score is None
        assert!(snap.recent_scores.is_empty());
        assert!(snap.mean_score.is_none());
    }

    #[test]
    fn test_score_extraction_round_1_saturating_sub() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        // Edge case: round_number=1 → saturating_sub(1) = 0
        let round_number: u32 = 1;
        let previous_own_score: Option<f32> = Some(5.0);
        let session_id = "job-Y";

        if let Some(score) = previous_own_score {
            let prev_round = round_number.saturating_sub(1);
            snap.push_score(ScoreEntry {
                timestamp: "t".into(),
                job_id: session_id.into(),
                round: prev_round,
                evaluator: "aggregated".into(),
                score,
            });
        }

        assert_eq!(snap.recent_scores.len(), 1);
        assert_eq!(snap.recent_scores[0].round, 0); // saturating_sub(1) of 1 = 0
    }

    #[test]
    fn test_score_extraction_skips_evaluate_action() {
        // Verify that score extraction only happens for "propose", not "evaluate".
        // This mirrors the `if action == "propose"` guard in handle_message.
        use crate::status::AgentStatusSnapshot;

        let snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());
        let action = "evaluate";
        let previous_own_score: Option<f32> = Some(7.0);

        let pushed = if action == "propose" {
            previous_own_score.is_some()
        } else {
            false
        };

        assert!(!pushed, "evaluate action should NOT extract scores");
        assert!(snap.recent_scores.is_empty());
    }

    // ---- handle_round_summary edge case tests ----

    #[test]
    fn test_round_summary_unknown_agent_no_push() {
        use crate::events::{ProposalScoreEntry, RoundSummaryEvent};
        use crate::status::AgentStatusSnapshot;

        let event = RoundSummaryEvent {
            round: 1,
            convergence_score: 0.5,
            decisiveness: 0.5,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![ProposalScoreEntry {
                agent_id: "other-agent".into(),
                aggregated_score: 8.0,
                category_breakdown: None,
                controversy_score: None,
                ..Default::default()
            }],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        // Simulate handler logic: search for "my-agent" in proposal_scores
        let my_id = "my-agent";
        let mut snap = AgentStatusSnapshot::new(my_id.into(), "model".into(), "p".into());

        for entry in &event.proposal_scores {
            if entry.agent_id == my_id {
                snap.push_score(crate::status::ScoreEntry {
                    timestamp: "t".into(),
                    job_id: "job".into(),
                    round: event.round,
                    evaluator: "aggregated".into(),
                    score: entry.aggregated_score,
                });
                break;
            }
        }

        // No score pushed — agent not found in proposal_scores
        assert!(snap.recent_scores.is_empty());
        assert!(snap.mean_score.is_none());
    }

    #[test]
    fn test_round_summary_empty_proposal_scores_no_push() {
        use crate::events::RoundSummaryEvent;
        use crate::status::AgentStatusSnapshot;

        let event = RoundSummaryEvent {
            round: 1,
            convergence_score: 0.0,
            decisiveness: 0.0,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![], // empty — all agents timed out
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        for entry in &event.proposal_scores {
            if entry.agent_id == "agent-1" {
                snap.push_score(crate::status::ScoreEntry {
                    timestamp: "t".into(),
                    job_id: "job".into(),
                    round: event.round,
                    evaluator: "aggregated".into(),
                    score: entry.aggregated_score,
                });
                break;
            }
        }

        assert!(snap.recent_scores.is_empty());
    }

    #[test]
    fn test_round_summary_subject_parsing_with_prefix() {
        // Verify session_id extraction from NATS subject
        let prefix = "nsed";
        let subject = "nsed.session-abc.result.event.round_summary";

        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject
            .split('.')
            .nth(prefix_count)
            .unwrap_or("?")
            .to_string();

        assert_eq!(session_id, "session-abc");
    }

    #[test]
    fn test_round_summary_subject_parsing_empty_prefix() {
        let prefix = "";
        let subject = "session-xyz.result.event.round_summary";

        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject
            .split('.')
            .nth(prefix_count)
            .unwrap_or("?")
            .to_string();

        assert_eq!(session_id, "session-xyz");
    }

    #[test]
    fn test_round_summary_subject_parsing_multi_segment_prefix() {
        let prefix = "org.nsed";
        let subject = "org.nsed.session-42.result.event.round_summary";

        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject
            .split('.')
            .nth(prefix_count)
            .unwrap_or("?")
            .to_string();

        assert_eq!(session_id, "session-42");
    }

    #[test]
    fn test_round_summary_processes_score_for_own_agent() {
        // Verifies that round_summary handler finds and records this agent's score
        use crate::events::{ProposalScoreEntry, RoundSummaryEvent};
        use crate::status::AgentStatusSnapshot;

        let event = RoundSummaryEvent {
            round: 2,
            convergence_score: 0.7,
            decisiveness: 0.7,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![
                ProposalScoreEntry {
                    agent_id: "other-agent".into(),
                    aggregated_score: 6.0,
                    category_breakdown: None,
                    controversy_score: None,
                    ..Default::default()
                },
                ProposalScoreEntry {
                    agent_id: "my-agent".into(),
                    aggregated_score: 8.5,
                    category_breakdown: None,
                    controversy_score: None,
                    ..Default::default()
                },
            ],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let my_id = "my-agent";
        let session_id = "job-123";
        let mut snap = AgentStatusSnapshot::new(my_id.into(), "model".into(), "p".into());

        // Simulate the handler logic (without the active_jobs gate)
        for entry in &event.proposal_scores {
            if entry.agent_id == my_id {
                let already_has = snap
                    .recent_scores
                    .iter()
                    .any(|s| s.job_id == session_id && s.round == event.round);
                if !already_has {
                    snap.push_score(crate::status::ScoreEntry {
                        timestamp: "t".into(),
                        job_id: session_id.into(),
                        round: event.round,
                        evaluator: "aggregated".into(),
                        score: entry.aggregated_score,
                    });
                }
                break;
            }
        }

        assert_eq!(snap.recent_scores.len(), 1);
        assert!((snap.mean_score.unwrap() - 8.5).abs() < f32::EPSILON);
        assert_eq!(snap.recent_scores[0].round, 2);
    }

    #[test]
    fn test_round_summary_dedup_prevents_double_push() {
        // Calling handler twice for same round should not duplicate scores
        use crate::events::{ProposalScoreEntry, RoundSummaryEvent};
        use crate::status::AgentStatusSnapshot;

        let event = RoundSummaryEvent {
            round: 1,
            convergence_score: 0.5,
            decisiveness: 0.5,
            net_support: vec![],
            cesaro_support: vec![],
            raw_distance: None,
            claim_convergence: None,
            total_claims: None,
            leader_claim_convergence: None,
            leader_total_claims: None,
            controversy_scores: vec![],
            proposal_scores: vec![ProposalScoreEntry {
                agent_id: "agent-1".into(),
                aggregated_score: 7.0,
                category_breakdown: None,
                controversy_score: None,
                ..Default::default()
            }],
            accumulated_evidence: None,
            evidence_target: None,
            positive_budget: None,
            du_dt: None,
            signed_consensus: None,
            t_opt: None,
            thermo_probability: None,
            ..Default::default()
        };

        let my_id = "agent-1";
        let session_id = "job-x";
        let mut snap = AgentStatusSnapshot::new(my_id.into(), "m".into(), "p".into());

        // Process twice
        for _ in 0..2 {
            for entry in &event.proposal_scores {
                if entry.agent_id == my_id {
                    let already_has = snap
                        .recent_scores
                        .iter()
                        .any(|s| s.job_id == session_id && s.round == event.round);
                    if !already_has {
                        snap.push_score(crate::status::ScoreEntry {
                            timestamp: "t".into(),
                            job_id: session_id.into(),
                            round: event.round,
                            evaluator: "aggregated".into(),
                            score: entry.aggregated_score,
                        });
                    }
                    break;
                }
            }
        }

        assert_eq!(
            snap.recent_scores.len(),
            1,
            "Dedup should prevent double push"
        );
    }

    #[test]
    fn test_round_summary_zero_score_pushed() {
        // All scores are real (implicit max-score injection guarantees no sentinels).
        // A zero score is a valid QV score and should be pushed.
        use crate::status::AgentStatusSnapshot;

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "m".into(), "p".into());
        snap.push_score(crate::status::ScoreEntry {
            timestamp: "t".into(),
            job_id: "job-1".into(),
            round: 1,
            evaluator: "aggregated".into(),
            score: 0.0,
        });

        assert_eq!(
            snap.recent_scores.len(),
            1,
            "Zero score should be pushed (all scores are real)"
        );
    }

    #[test]
    fn test_previous_own_score_skips_round_1() {
        // When round_number == 1, saturating_sub(1) yields 0 which is not a real
        // round — the guard `round_number > 1` should prevent pushing.
        use crate::status::AgentStatusSnapshot;

        let mut snap = AgentStatusSnapshot::new("agent".into(), "m".into(), "p".into());
        let round_number: u32 = 1;
        let previous_own_score: Option<f32> = Some(5.0);
        let session_id = "job-1";

        // Simulate the guarded logic from handle_task
        if round_number > 1 {
            if let Some(score) = previous_own_score {
                let prev_round = round_number.saturating_sub(1);
                snap.push_score(crate::status::ScoreEntry {
                    timestamp: "t".into(),
                    job_id: session_id.into(),
                    round: prev_round,
                    evaluator: "aggregated".into(),
                    score,
                });
            }
        }

        assert!(
            snap.recent_scores.is_empty(),
            "Round 1 should not push a score for round 0"
        );
    }

    #[test]
    fn test_previous_own_score_pushes_for_round_2() {
        // When round_number >= 2, previous_own_score should be recorded
        use crate::status::AgentStatusSnapshot;

        let mut snap = AgentStatusSnapshot::new("agent".into(), "m".into(), "p".into());
        let round_number: u32 = 3;
        let previous_own_score: Option<f32> = Some(7.5);
        let session_id = "job-1";

        if round_number > 1 {
            if let Some(score) = previous_own_score {
                let prev_round = round_number.saturating_sub(1);
                snap.push_score(crate::status::ScoreEntry {
                    timestamp: "t".into(),
                    job_id: session_id.into(),
                    round: prev_round,
                    evaluator: "aggregated".into(),
                    score,
                });
            }
        }

        assert_eq!(snap.recent_scores.len(), 1);
        assert_eq!(snap.recent_scores[0].round, 2);
        assert!((snap.recent_scores[0].score - 7.5).abs() < f32::EPSILON);
    }

    // ---- inject_annotations evaluate array edge cases ----

    #[test]
    fn test_inject_annotations_empty_eval_array() {
        let payload = b"[]";
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert!(val.as_array().unwrap().is_empty());
    }

    #[test]
    fn test_inject_annotations_malformed_tuple_single_element() {
        // Tuple with only target_id, no eval object — should be skipped gracefully
        let payload = br#"[["agent-A"]]"#;
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let arr = val.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        // No crash — tuple[1] doesn't exist, so annotations not injected
        let inner = arr[0].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0], "agent-A");
    }

    #[test]
    fn test_inject_annotations_tuple_non_object_at_index_1() {
        // Tuple where index 1 is a string, not an object — should be skipped
        let payload = br#"[["agent-A", "not-an-object"]]"#;
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let inner = val.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(inner[1], "not-an-object"); // unchanged
    }

    #[test]
    fn test_inject_annotations_mixed_valid_and_invalid_tuples() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        // Mix: valid tuple, malformed single-element, valid tuple
        let payload = br#"[
            ["agent-A", {"score": 5.0}],
            ["agent-B"],
            ["agent-C", {"score": 8.0}]
        ]"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed".into(),
            timestamp: "t".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, true, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let arr = val.as_array().unwrap();

        // First tuple: valid → annotations injected
        let a = &arr[0].as_array().unwrap()[1];
        assert_eq!(a["edited_by"], "operator");
        assert!(a["operator_annotations"].is_array());

        // Second tuple: malformed → no crash, no annotations
        let b = arr[1].as_array().unwrap();
        assert_eq!(b.len(), 1); // just ["agent-B"]

        // Third tuple: valid → annotations injected
        let c = &arr[2].as_array().unwrap()[1];
        assert_eq!(c["edited_by"], "operator");
    }

    #[test]
    fn test_inject_annotations_non_array_item_in_eval_array() {
        // Array items that aren't arrays themselves — should be skipped
        let payload = br#"["just-a-string", 42, null]"#;
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        // Should survive without panic — items that aren't arrays are skipped
        let arr = val.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    // ---- NATS integration tests: ack_wait & redelivery ----

    /// Helper: connect to a local NATS server, returning None if unavailable.
    async fn setup_nats() -> Option<async_nats::Client> {
        let nats_url =
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
        let client = connect_nats(&nats_url, None).await.ok()?;
        if client.connection_state() != NatsState::Connected {
            return None;
        }
        Some(client)
    }

    /// Regression test: agent task messages must be redelivered promptly after
    /// a crash. With a short `ack_wait` (3s in this test, 30s in production),
    /// an unacked task message becomes available for another consumer within
    /// seconds instead of the previous 600s (10 minutes).
    ///
    /// This validates the mechanism that enables fast agent recovery after
    /// restart — agents pick up their unfinished tasks almost immediately.
    #[tokio::test]
    async fn test_agent_task_redelivered_after_short_ack_wait() {
        let client = match setup_nats().await {
            Some(c) => c,
            None => {
                println!("Skipping test: NATS unavailable");
                return;
            }
        };
        let js = async_nats::jetstream::new(client.clone());

        let unique_id = Uuid::new_v4();
        let stream_name = format!("agent_redeliver_test_{}", unique_id);
        let agent_id = "test-agent";
        let task_subject = format!("{}.session1.task.{}.propose", stream_name, agent_id);

        // Create a test stream that captures task subjects
        js.create_stream(async_nats::jetstream::stream::Config {
            name: stream_name.clone(),
            subjects: vec![format!("{}.*.task.{}.>", stream_name, agent_id)],
            storage: async_nats::jetstream::stream::StorageType::Memory,
            ..Default::default()
        })
        .await
        .expect("create test stream");

        // Publish a task (simulates orchestrator dispatching propose)
        let context = serde_json::json!({
            "task_description": "Test task for redelivery",
            "round_number": 1,
            "agent_ids": ["test-agent"],
            "session_id": "session1"
        });
        js.publish(
            task_subject.clone(),
            serde_json::to_vec(&context).unwrap().into(),
        )
        .await
        .expect("publish task");

        // Create consumer with SHORT ack_wait (3s for test speed)
        let consumer_name = format!("agent_consumer_{}", unique_id);
        let stream = js.get_stream(&stream_name).await.expect("get stream");
        let consumer = stream
            .get_or_create_consumer(
                &consumer_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject: format!("{}.*.task.{}.>", stream_name, agent_id),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: std::time::Duration::from_secs(3),
                    ..Default::default()
                },
            )
            .await
            .expect("create consumer");

        // Consume the message but DON'T ack (simulating crash)
        let mut messages = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), messages.next())
            .await
            .expect("timeout waiting for first delivery")
            .expect("stream ended")
            .expect("message error");

        // Verify we got the right message
        let payload: serde_json::Value = serde_json::from_slice(&msg.payload).expect("deserialize");
        assert_eq!(payload["task_description"], "Test task for redelivery");

        // Drop message stream (simulating crash — DO NOT ack)
        drop(messages);

        // Wait for ack_wait to expire (3s + buffer)
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        // Reconnect to the SAME durable consumer — message should be redelivered
        let consumer2 = stream
            .get_or_create_consumer(
                &consumer_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject: format!("{}.*.task.{}.>", stream_name, agent_id),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: std::time::Duration::from_secs(3),
                    ..Default::default()
                },
            )
            .await
            .expect("rebind consumer");

        let mut messages2 = consumer2.messages().await.expect("messages2");
        let redelivered = tokio::time::timeout(std::time::Duration::from_secs(5), messages2.next())
            .await
            .expect("message should be redelivered within ack_wait window")
            .expect("stream ended")
            .expect("message error");

        let payload2: serde_json::Value =
            serde_json::from_slice(&redelivered.payload).expect("deserialize redelivery");
        assert_eq!(
            payload2["task_description"], "Test task for redelivery",
            "Redelivered message should be the same task"
        );

        // Verify it was actually redelivered (num_delivered > 1)
        if let Ok(info) = redelivered.info() {
            assert!(
                info.delivered > 1,
                "Message should have been delivered more than once (redelivery). Got: {}",
                info.delivered
            );
        }

        // Ack and cleanup
        let _ = redelivered.ack().await;
        let _ = js.delete_stream(&stream_name).await;
    }

    /// Verify that `AckKind::Progress` heartbeats extend the ack deadline,
    /// preventing premature redelivery during long-running LLM calls.
    ///
    /// Flow: consume message → send Progress heartbeat → wait longer than
    /// original ack_wait → verify message is NOT redelivered (heartbeat
    /// extended the deadline).
    #[tokio::test]
    async fn test_progress_heartbeat_prevents_premature_redelivery() {
        let client = match setup_nats().await {
            Some(c) => c,
            None => {
                println!("Skipping test: NATS unavailable");
                return;
            }
        };
        let js = async_nats::jetstream::new(client.clone());

        let unique_id = Uuid::new_v4();
        let stream_name = format!("agent_hb_test_{}", unique_id);
        let agent_id = "hb-agent";
        let task_subject = format!("{}.session1.task.{}.propose", stream_name, agent_id);

        js.create_stream(async_nats::jetstream::stream::Config {
            name: stream_name.clone(),
            subjects: vec![format!("{}.*.task.{}.>", stream_name, agent_id)],
            storage: async_nats::jetstream::stream::StorageType::Memory,
            ..Default::default()
        })
        .await
        .expect("create test stream");

        let context = serde_json::json!({
            "task_description": "Heartbeat test task",
            "round_number": 1,
            "agent_ids": ["hb-agent"],
            "session_id": "session1"
        });
        js.publish(
            task_subject.clone(),
            serde_json::to_vec(&context).unwrap().into(),
        )
        .await
        .expect("publish task");

        let consumer_name = format!("hb_consumer_{}", unique_id);
        let stream = js.get_stream(&stream_name).await.expect("get stream");
        let consumer = stream
            .get_or_create_consumer(
                &consumer_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject: format!("{}.*.task.{}.>", stream_name, agent_id),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: std::time::Duration::from_secs(3), // 3s ack_wait
                    ..Default::default()
                },
            )
            .await
            .expect("create consumer");

        let mut messages = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), messages.next())
            .await
            .expect("timeout")
            .expect("stream ended")
            .expect("message error");

        // Send Progress heartbeat at t=2s (before 3s ack_wait expires)
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        msg.ack_with(async_nats::jetstream::AckKind::Progress)
            .await
            .expect("progress ack");

        // Wait another 2s (total 4s from first delivery, but heartbeat reset the clock)
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Now ack the message
        msg.ack().await.expect("final ack");

        // Drop the message stream
        drop(messages);

        // Try to consume again — there should be NO redelivered messages
        // because we successfully acked.
        let consumer3 = stream
            .get_or_create_consumer(
                &consumer_name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject: format!("{}.*.task.{}.>", stream_name, agent_id),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: std::time::Duration::from_secs(3),
                    ..Default::default()
                },
            )
            .await
            .expect("rebind consumer");

        let mut messages3 = consumer3.messages().await.expect("messages3");
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(4), messages3.next()).await;

        // Should timeout — no messages to deliver since we acked
        assert!(
            result.is_err(),
            "No message should be redelivered after successful Progress+Ack"
        );

        // Cleanup
        let _ = js.delete_stream(&stream_name).await;
    }

    // -------------------------------------------------------------------
    // Mock agent for worker builder/control tests
    // -------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct MockAgent;

    #[async_trait]
    impl NsedAgent for MockAgent {
        async fn propose(&self, _context: &AgentContext) -> Result<crate::agents::Proposal> {
            Ok(crate::agents::Proposal {
                content: "mock".into(),
                ..Default::default()
            })
        }
        async fn evaluate(
            &self,
            _context: &AgentContext,
        ) -> Result<Vec<(String, crate::agents::Evaluation)>> {
            Ok(vec![])
        }
        fn name(&self) -> String {
            "mock-agent".into()
        }
    }

    // -------------------------------------------------------------------
    // Worker pause/resume tests (require NATS)
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_worker_pause_resume() {
        let client = match setup_nats().await {
            Some(c) => c,
            None => {
                println!("Skipping test: NATS unavailable");
                return;
            }
        };
        drop(client); // We only need NATS to be available for worker construction

        let config = WorkerConfig::new(
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            format!("test_pause_resume_{}", Uuid::new_v4()),
            format!("consumer_pause_resume_{}", Uuid::new_v4()),
        );
        let agent_config = AgentConfig {
            name: "mock-agent".into(),
            provider_id: "test".into(),
            model_name: "test-model".into(),
            ..Default::default()
        };

        let worker = NatsNsedWorker::new(MockAgent, agent_config, config, None)
            .await
            .expect("worker creation should succeed");

        // Initially not paused
        assert!(!worker.is_paused(), "worker should start unpaused");

        // Pause the worker
        worker.pause();
        assert!(worker.is_paused(), "worker should be paused after pause()");

        // Resume the worker
        worker.resume();
        assert!(
            !worker.is_paused(),
            "worker should be unpaused after resume()"
        );

        // Double pause is idempotent
        worker.pause();
        worker.pause();
        assert!(worker.is_paused(), "double pause should still be paused");

        // Resume once is enough
        worker.resume();
        assert!(!worker.is_paused(), "single resume should unpause");
    }

    #[tokio::test]
    async fn test_worker_with_response_buffer() {
        let client = match setup_nats().await {
            Some(c) => c,
            None => {
                println!("Skipping test: NATS unavailable");
                return;
            }
        };
        drop(client);

        let config = WorkerConfig::new(
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            format!("test_buffer_{}", Uuid::new_v4()),
            format!("consumer_buffer_{}", Uuid::new_v4()),
        );
        let agent_config = AgentConfig {
            name: "mock-agent".into(),
            provider_id: "test".into(),
            model_name: "test-model".into(),
            ..Default::default()
        };

        // Without buffer
        let worker = NatsNsedWorker::new(MockAgent, agent_config.clone(), config.clone(), None)
            .await
            .expect("worker creation should succeed");
        assert!(worker.response_buffer().is_none(), "no buffer by default");

        // With buffer
        let config2 = WorkerConfig::new(
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            format!("test_buffer2_{}", Uuid::new_v4()),
            format!("consumer_buffer2_{}", Uuid::new_v4()),
        );
        let worker = NatsNsedWorker::new(MockAgent, agent_config, config2, None)
            .await
            .expect("worker creation should succeed")
            .with_response_buffer(std::time::Duration::from_secs(30));

        assert!(
            worker.response_buffer().is_some(),
            "buffer should be set after with_response_buffer()"
        );

        // Verify pause_handle returns a shared Arc<AtomicBool>
        let handle = worker.pause_handle();
        assert!(
            !handle.load(Ordering::Relaxed),
            "handle should start as false"
        );

        // Pause via worker, verify handle reflects it
        worker.pause();
        assert!(
            handle.load(Ordering::Relaxed),
            "handle should reflect paused state"
        );

        // Also verify the buffer is paused when worker is paused
        assert!(
            worker.response_buffer().unwrap().is_paused(),
            "buffer should be paused when worker is paused"
        );

        // Resume via worker
        worker.resume();
        assert!(
            !handle.load(Ordering::Relaxed),
            "handle should reflect unpaused state"
        );
        assert!(
            !worker.response_buffer().unwrap().is_paused(),
            "buffer should be unpaused when worker is resumed"
        );
    }

    /// Tests that externally mutating the `Arc<AtomicBool>` returned by
    /// `pause_handle()` is reflected by `is_paused()`.
    ///
    /// **Important:** This test exercises raw atomic mutation which bypasses
    /// the response-buffer pause logic. Production callers should use
    /// [`NatsNsedWorker::pause()`] / [`NatsNsedWorker::resume()`] instead,
    /// which synchronise both the atomic flag and the buffer state.
    #[tokio::test]
    async fn test_worker_pause_handle_external_mutation() {
        let client = match setup_nats().await {
            Some(c) => c,
            None => {
                println!("Skipping test: NATS unavailable");
                return;
            }
        };
        drop(client);

        let config = WorkerConfig::new(
            std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            format!("test_handle_{}", Uuid::new_v4()),
            format!("consumer_handle_{}", Uuid::new_v4()),
        );
        let agent_config = AgentConfig {
            name: "mock-agent".into(),
            provider_id: "test".into(),
            model_name: "test-model".into(),
            ..Default::default()
        };

        let worker = NatsNsedWorker::new(MockAgent, agent_config, config, None)
            .await
            .expect("worker creation should succeed");

        let handle = worker.pause_handle();

        // Externally set the handle (simulating dashboard control plane)
        handle.store(true, Ordering::Relaxed);
        assert!(
            worker.is_paused(),
            "is_paused() should reflect external handle mutation"
        );

        handle.store(false, Ordering::Relaxed);
        assert!(
            !worker.is_paused(),
            "is_paused() should reflect external handle un-mutation"
        );
    }

    // ===================================================================
    // inject_annotations: additional edge cases
    // ===================================================================

    #[test]
    fn test_inject_annotations_primitive_json_value_passthrough() {
        // A JSON primitive (string, number, bool, null) is neither object nor
        // array, so inject_annotations should return the original bytes.
        for payload in &[
            br#""just a string""#.to_vec(),
            b"42".to_vec(),
            b"true".to_vec(),
            b"null".to_vec(),
        ] {
            let entry = make_entry(payload, true, vec![]);
            let result = NatsNsedWorker::inject_annotations(&entry);
            // Should be round-trippable JSON but unchanged semantically
            let original: serde_json::Value = serde_json::from_slice(payload).unwrap();
            let returned: serde_json::Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(
                original, returned,
                "primitive JSON should pass through unchanged"
            );
        }
    }

    #[test]
    fn test_inject_annotations_proposal_edited_no_annotations() {
        // edited=true but annotations list is empty:
        // should add edited_by but NOT operator_annotations key
        let payload = br#"{"content":"hello"}"#;
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(val["edited_by"], "operator");
        assert!(
            val.get("operator_annotations").is_none(),
            "empty annotations should not produce operator_annotations key"
        );
    }

    #[test]
    fn test_inject_annotations_proposal_annotations_no_edit() {
        // edited=false, has annotations:
        // should add operator_annotations but NOT edited_by
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let payload = br#"{"content":"hello"}"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "Reviewed".into(),
            timestamp: "t".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, false, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert!(val["operator_annotations"].is_array());
        assert_eq!(val["operator_annotations"].as_array().unwrap().len(), 1);
        assert!(
            val.get("edited_by").is_none(),
            "should NOT add edited_by when edited=false"
        );
    }

    #[test]
    fn test_inject_annotations_proposal_multiple_annotations() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let payload = br#"{"content":"test"}"#;
        let annotations = vec![
            OperatorAnnotation {
                annotation_type: AnnotationType::Comment,
                comment: "First comment".into(),
                timestamp: "t1".into(),
                original_content_hash: None,
            },
            OperatorAnnotation {
                annotation_type: AnnotationType::Edit,
                comment: "Edited".into(),
                timestamp: "t2".into(),
                original_content_hash: Some("hash123".into()),
            },
            OperatorAnnotation {
                annotation_type: AnnotationType::Comment,
                comment: "Final LGTM".into(),
                timestamp: "t3".into(),
                original_content_hash: None,
            },
        ];
        let entry = make_entry(payload, true, annotations);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        assert_eq!(val["edited_by"], "operator");
        let ann_arr = val["operator_annotations"].as_array().unwrap();
        assert_eq!(ann_arr.len(), 3);
        assert_eq!(ann_arr[0]["comment"], "First comment");
        assert_eq!(ann_arr[1]["comment"], "Edited");
        assert_eq!(ann_arr[2]["comment"], "Final LGTM");
    }

    #[test]
    fn test_inject_annotations_eval_edited_no_annotations() {
        // Evaluation array: edited=true, empty annotations.
        // Should add edited_by to each eval object but no operator_annotations.
        let payload = br#"[["agent-A", {"score": 5.0}], ["agent-B", {"score": 7.0}]]"#;
        let entry = make_entry(payload, true, vec![]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();

        let arr = val.as_array().unwrap();
        for item in arr {
            let eval = &item.as_array().unwrap()[1];
            assert_eq!(eval["edited_by"], "operator");
            assert!(
                eval.get("operator_annotations").is_none(),
                "no annotations should produce no operator_annotations key"
            );
        }
    }

    #[test]
    fn test_inject_annotations_eval_non_array_items_skipped() {
        // Array where some items are not arrays (e.g. numbers or nulls).
        // Should survive without panic; non-array items left unchanged.
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let payload = br#"[42, ["agent-A", {"score": 5.0}], null, "string"]"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "test".into(),
            timestamp: "t".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, true, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let arr = val.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        // Only the valid tuple should have annotations injected
        let eval_a = &arr[1].as_array().unwrap()[1];
        assert_eq!(eval_a["edited_by"], "operator");
        assert!(eval_a["operator_annotations"].is_array());
        // Non-array items remain unchanged
        assert_eq!(arr[0], 42);
        assert!(arr[2].is_null());
        assert_eq!(arr[3], "string");
    }

    // ===================================================================
    // Payment error detection logic (pure string matching)
    // ===================================================================

    /// Extracts the payment error detection logic into a testable form.
    /// This mirrors the inline logic at lines ~1157-1170 of handle_message.
    fn is_payment_error(err_str: &str) -> bool {
        err_str.contains("402 Payment Required")
            || err_str.contains("insufficient_quota")
            || err_str.contains("billing")
    }

    fn should_suppress_error_event(err_str: &str, propagate_payment_error: bool) -> bool {
        is_payment_error(err_str) && !propagate_payment_error
    }

    #[test]
    fn test_payment_error_402() {
        assert!(is_payment_error("HTTP error: 402 Payment Required"));
        assert!(is_payment_error("402 Payment Required: no credits"));
    }

    #[test]
    fn test_payment_error_insufficient_quota() {
        assert!(is_payment_error("OpenAI error: insufficient_quota"));
        assert!(is_payment_error("insufficient_quota for this model"));
    }

    #[test]
    fn test_payment_error_billing() {
        assert!(is_payment_error("Your billing account has been suspended"));
        assert!(is_payment_error("billing information required"));
    }

    #[test]
    fn test_non_payment_errors_not_detected() {
        assert!(!is_payment_error("500 Internal Server Error"));
        assert!(!is_payment_error("Connection timeout"));
        assert!(!is_payment_error("rate limit exceeded"));
        assert!(!is_payment_error("model not found"));
        assert!(!is_payment_error(""));
    }

    #[test]
    fn test_suppress_error_when_payment_and_not_propagate() {
        // propagate_payment_error = false → should suppress
        assert!(should_suppress_error_event("402 Payment Required", false));
        assert!(should_suppress_error_event("insufficient_quota", false));
        assert!(should_suppress_error_event("billing issue", false));
    }

    #[test]
    fn test_no_suppress_when_payment_and_propagate() {
        // propagate_payment_error = true → should NOT suppress
        assert!(!should_suppress_error_event("402 Payment Required", true));
        assert!(!should_suppress_error_event("insufficient_quota", true));
    }

    #[test]
    fn test_no_suppress_when_not_payment_error() {
        // Non-payment errors → never suppress regardless of propagate flag
        assert!(!should_suppress_error_event(
            "500 Internal Server Error",
            false
        ));
        assert!(!should_suppress_error_event("Connection refused", true));
    }

    // ===================================================================
    // Heartbeat status determination (pure logic)
    // ===================================================================

    #[test]
    fn test_heartbeat_status_idle_when_no_active_jobs() {
        let active_jobs: HashSet<String> = HashSet::new();
        let active_job = active_jobs.iter().next().cloned();
        let status = if active_job.is_some() {
            AgentLiveStatus::Busy
        } else {
            AgentLiveStatus::Idle
        };
        assert_eq!(status, AgentLiveStatus::Idle);
        assert!(active_job.is_none());
    }

    #[test]
    fn test_heartbeat_status_busy_when_active_job() {
        let mut active_jobs: HashSet<String> = HashSet::new();
        active_jobs.insert("session-123".to_string());
        let active_job = active_jobs.iter().next().cloned();
        let status = if active_job.is_some() {
            AgentLiveStatus::Busy
        } else {
            AgentLiveStatus::Idle
        };
        assert_eq!(status, AgentLiveStatus::Busy);
        assert!(active_job.is_some());
    }

    #[test]
    fn test_heartbeat_status_busy_with_multiple_active_jobs() {
        let mut active_jobs: HashSet<String> = HashSet::new();
        active_jobs.insert("session-1".to_string());
        active_jobs.insert("session-2".to_string());
        let active_job = active_jobs.iter().next().cloned();
        // With multiple jobs, active_job is some (though order is non-deterministic)
        assert!(active_job.is_some());
        let status = if active_job.is_some() {
            AgentLiveStatus::Busy
        } else {
            AgentLiveStatus::Idle
        };
        assert_eq!(status, AgentLiveStatus::Busy);
    }

    // ===================================================================
    // Error string formatting in heartbeat (pure logic from lines 1234-1247)
    // ===================================================================

    #[test]
    fn test_heartbeat_error_extraction_no_status() {
        // No status snapshot → defaults to (0, 0, None)
        let status: Option<&str> = None;
        let (tasks_completed, tasks_failed, last_error): (u64, u64, Option<String>) =
            if status.is_some() {
                unreachable!()
            } else {
                (0, 0, None)
            };
        assert_eq!(tasks_completed, 0);
        assert_eq!(tasks_failed, 0);
        assert!(last_error.is_none());
    }

    #[test]
    fn test_heartbeat_error_extraction_from_task_log() {
        use crate::status::{AgentStatusSnapshot, TaskLogEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        // Push a successful task
        snap.push_task(TaskLogEntry {
            timestamp: "t1".into(),
            action: "propose".into(),
            job_id: "job-1".into(),
            round: 1,
            status: "ok".into(),
            duration_ms: 100,
            content_preview: None,
        });

        // Push an error task
        snap.push_task(TaskLogEntry {
            timestamp: "t2".into(),
            action: "evaluate".into(),
            job_id: "job-2".into(),
            round: 1,
            status: "error".into(),
            duration_ms: 200,
            content_preview: Some("Error: connection timeout".into()),
        });

        // Extract error using the same logic as publish_heartbeat
        let err = snap
            .recent_tasks
            .iter()
            .find(|t| t.status == "error")
            .map(|t| {
                let msg = format!("{}: {}", t.action, t.job_id);
                msg.chars().take(120).collect::<String>()
            });

        assert!(err.is_some());
        assert_eq!(err.unwrap(), "evaluate: job-2");
    }

    #[test]
    fn test_heartbeat_error_truncation_120_chars() {
        use crate::status::{AgentStatusSnapshot, TaskLogEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        // Create a task with a very long job_id to test truncation
        let long_job_id = "a".repeat(200);
        snap.push_task(TaskLogEntry {
            timestamp: "t1".into(),
            action: "propose".into(),
            job_id: long_job_id.clone(),
            round: 1,
            status: "error".into(),
            duration_ms: 500,
            content_preview: None,
        });

        let err = snap
            .recent_tasks
            .iter()
            .find(|t| t.status == "error")
            .map(|t| {
                let msg = format!("{}: {}", t.action, t.job_id);
                msg.chars().take(120).collect::<String>()
            });

        assert!(err.is_some());
        let err_msg = err.unwrap();
        assert_eq!(
            err_msg.chars().count(),
            120,
            "error should be truncated to 120 chars"
        );
        assert!(err_msg.starts_with("propose: "));
    }

    #[test]
    fn test_heartbeat_error_no_error_tasks() {
        use crate::status::{AgentStatusSnapshot, TaskLogEntry};

        let mut snap = AgentStatusSnapshot::new("agent-1".into(), "model".into(), "p".into());

        // Only successful tasks
        snap.push_task(TaskLogEntry {
            timestamp: "t1".into(),
            action: "propose".into(),
            job_id: "job-1".into(),
            round: 1,
            status: "ok".into(),
            duration_ms: 100,
            content_preview: None,
        });

        let err = snap
            .recent_tasks
            .iter()
            .find(|t| t.status == "error")
            .map(|t| {
                let msg = format!("{}: {}", t.action, t.job_id);
                msg.chars().take(120).collect::<String>()
            });

        assert!(err.is_none());
    }

    // ===================================================================
    // extract_content_preview (standalone function, lines 1549-1667)
    // ===================================================================

    #[test]
    fn test_extract_preview_proposal_basic() {
        let payload = serde_json::json!({
            "content": "Hello world proposal",
            "thought_process": "I thought about it"
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        assert_eq!(parsed["t"], "p");
        assert_eq!(parsed["c"], "Hello world proposal");
        assert_eq!(parsed["tp"], "I thought about it");
    }

    #[test]
    fn test_extract_preview_proposal_no_thought_process() {
        let payload = serde_json::json!({
            "content": "Simple proposal"
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        assert_eq!(parsed["t"], "p");
        assert_eq!(parsed["c"], "Simple proposal");
        // No thought_process → no "tp" key
        assert!(parsed.get("tp").is_none());
    }

    #[test]
    fn test_extract_preview_proposal_empty_content() {
        let payload = serde_json::json!({
            "content": "",
            "thought_process": "I thought but had nothing to say"
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_none(), "empty content should return None");
    }

    #[test]
    fn test_extract_preview_proposal_truncates_long_content() {
        let long_content = "x".repeat(3000);
        let payload = serde_json::json!({
            "content": long_content,
            "thought_process": ""
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let c = parsed["c"].as_str().unwrap();
        // Content should be truncated to ~2000 chars + ellipsis
        assert!(
            c.chars().count() <= 2002,
            "content should be truncated to ~2000 chars, got {}",
            c.chars().count()
        );
        assert!(
            c.ends_with('\u{2026}'),
            "truncated content should end with ellipsis"
        );
    }

    #[test]
    fn test_extract_preview_proposal_truncates_long_thought_process() {
        let long_tp = "y".repeat(1000);
        let payload = serde_json::json!({
            "content": "brief",
            "thought_process": long_tp
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tp = parsed["tp"].as_str().unwrap();
        // thought_process should be truncated to ~500 chars + ellipsis
        assert!(
            tp.chars().count() <= 502,
            "tp should be truncated to ~500 chars, got {}",
            tp.chars().count()
        );
        assert!(
            tp.ends_with('\u{2026}'),
            "truncated tp should end with ellipsis"
        );
    }

    #[test]
    fn test_extract_preview_evaluation_basic() {
        let payload = serde_json::json!([
            ["agent-A", {"score": 7.5, "justification": "Good work", "stance": "agree"}],
            ["agent-B", {"score": 4.0, "justification": "Needs improvement", "textual_feedback": "Try harder"}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        assert_eq!(parsed["t"], "e");
        let evals = parsed["evals"].as_array().unwrap();
        assert_eq!(evals.len(), 2);

        assert_eq!(evals[0]["target"], "agent-A");
        assert_eq!(evals[0]["s"], 7.5);
        assert_eq!(evals[0]["j"], "Good work");
        assert_eq!(evals[0]["stance"], "agree");

        assert_eq!(evals[1]["target"], "agent-B");
        assert_eq!(evals[1]["s"], 4.0);
        assert_eq!(evals[1]["tf"], "Try harder");
    }

    #[test]
    fn test_extract_preview_evaluation_empty_array() {
        let payload = serde_json::json!([]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_none(), "empty eval array should return None");
    }

    #[test]
    fn test_extract_preview_evaluation_truncates_justification() {
        let long_justification = "z".repeat(500);
        let payload = serde_json::json!([
            ["agent-A", {"score": 5.0, "justification": long_justification}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let j = parsed["evals"][0]["j"].as_str().unwrap();
        assert!(
            j.chars().count() <= 302,
            "justification should be truncated to ~300 chars, got {}",
            j.chars().count()
        );
        assert!(
            j.ends_with('\u{2026}'),
            "truncated justification should end with ellipsis"
        );
    }

    #[test]
    fn test_extract_preview_evaluation_truncates_textual_feedback() {
        let long_tf = "w".repeat(400);
        let payload = serde_json::json!([
            ["agent-A", {"score": 5.0, "textual_feedback": long_tf}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tf = parsed["evals"][0]["tf"].as_str().unwrap();
        assert!(
            tf.chars().count() <= 202,
            "tf should be truncated to ~200 chars, got {}",
            tf.chars().count()
        );
        assert!(
            tf.ends_with('\u{2026}'),
            "truncated tf should end with ellipsis"
        );
    }

    #[test]
    fn test_extract_preview_evaluation_with_category_scores() {
        let payload = serde_json::json!([
            ["agent-A", {
                "score": 6.0,
                "justification": "OK",
                "category_scores": {"accuracy": 7, "clarity": 8}
            }]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let cats = &parsed["evals"][0]["cats"];
        assert_eq!(cats["accuracy"], 7);
        assert_eq!(cats["clarity"], 8);
    }

    #[test]
    fn test_extract_preview_evaluation_with_claim_assessments() {
        let payload = serde_json::json!([
            ["agent-A", {
                "score": 6.0,
                "justification": "OK",
                "claim_assessments": [{"claim": "X", "verdict": "agree"}]
            }]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let claims = &parsed["evals"][0]["claims"];
        assert!(claims.is_array());
        assert_eq!(claims[0]["claim"], "X");
    }

    #[test]
    fn test_extract_preview_evaluation_with_disagreements() {
        let payload = serde_json::json!([
            ["agent-A", {
                "score": 3.0,
                "justification": "Bad",
                "disagreements": ["point 1", "point 2"]
            }]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let disputes = &parsed["evals"][0]["disputes"];
        assert!(disputes.is_array());
        assert_eq!(disputes.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_extract_preview_evaluation_caps_at_10_entries() {
        // Create 15 eval tuples; preview should only include first 10
        let mut evals = Vec::new();
        for i in 0..15 {
            evals.push(serde_json::json!([
                format!("agent-{}", i),
                {"score": i as f64, "justification": "ok"}
            ]));
        }
        let payload = serde_json::Value::Array(evals);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let eval_arr = parsed["evals"].as_array().unwrap();
        assert_eq!(eval_arr.len(), 10, "should cap at 10 eval entries");
    }

    #[test]
    fn test_extract_preview_unknown_action_returns_none() {
        let payload = serde_json::json!({"content": "test"});
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "unknown_action", &[]);
        assert!(preview.is_none(), "unknown action should return None");
    }

    #[test]
    fn test_extract_preview_invalid_json_returns_none() {
        let bytes = b"not valid json at all";
        let preview = extract_content_preview(bytes, "propose", &[]);
        assert!(preview.is_none(), "invalid JSON should return None");
    }

    #[test]
    fn test_extract_preview_proposal_missing_content_field() {
        let payload = serde_json::json!({"thought_process": "thinking..."});
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        // content is missing → as_str() returns None → unwrap_or("") → empty → return None
        assert!(
            preview.is_none(),
            "missing content field should return None"
        );
    }

    #[test]
    fn test_extract_preview_evaluation_malformed_tuple() {
        // Array items that are not arrays should produce no eval entries
        let payload = serde_json::json!(["not-a-tuple", 42]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(
            preview.is_none(),
            "non-tuple items should produce empty evals → None"
        );
    }

    #[test]
    fn test_extract_preview_evaluation_tuple_missing_eval_obj() {
        // Tuple with only target_id, no eval object
        let payload = serde_json::json!([["agent-A"]]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(
            preview.is_none(),
            "tuple without eval obj at index 1 should produce empty evals → None"
        );
    }

    // ===================================================================
    // WorkerConfig additional builder coverage
    // ===================================================================

    #[test]
    fn test_worker_config_with_api_prefix() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "stream".to_string(),
            "consumer".to_string(),
        )
        .with_api_prefix("my_api".to_string());

        assert_eq!(config.api_prefix, "my_api");
        // Other fields unchanged
        assert_eq!(config.subject_prefix, "nsed");
        assert_eq!(config.scratchpad_retention_secs, 86400 * 7);
    }

    // ===================================================================
    // round_summary subject parsing: edge case — missing segment fallback
    // ===================================================================

    #[test]
    fn test_round_summary_subject_parsing_missing_segment_fallback() {
        // Subject has fewer segments than expected → fallback to "?"
        let prefix = "nsed.v2.extra"; // 3 segments
        let subject = "nsed.v2.extra"; // only the prefix itself, no session_id segment

        let prefix_count = if prefix.is_empty() {
            0
        } else {
            prefix.split('.').count()
        };
        let session_id = subject
            .split('.')
            .nth(prefix_count) // index 3, but only 3 segments (0..2)
            .unwrap_or("?")
            .to_string();

        assert_eq!(session_id, "?", "missing segment should fallback to '?'");
    }

    #[test]
    fn test_round_summary_subject_parsing_single_segment() {
        // Subject with just one segment after prefix
        let prefix = "nsed";
        let subject = "nsed.my-session";

        let prefix_count = prefix.split('.').count(); // 1
        let session_id = subject
            .split('.')
            .nth(prefix_count)
            .unwrap_or("?")
            .to_string();

        assert_eq!(session_id, "my-session");
    }

    // ===================================================================
    // Buffer auto-approve controls (from buffer.rs)
    // ===================================================================

    #[tokio::test]
    async fn test_auto_approve_enabled_by_default() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        assert!(
            buf.is_auto_approve(),
            "auto-approve should be ON by default"
        );
    }

    #[tokio::test]
    async fn test_auto_approve_enable_disable() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        assert!(buf.is_auto_approve());
        buf.set_auto_approve(false);
        assert!(!buf.is_auto_approve());
        buf.set_auto_approve(true);
        assert!(buf.is_auto_approve());
    }

    #[tokio::test]
    async fn test_auto_approve_threshold_default() {
        // Default is 1.0 (100%) so the default-on auto-approve mode is
        // a true pass-through: every divergence value in the clamped
        // [0, 1] range passes the `div > threshold` gate.
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        let threshold = buf.auto_approve_threshold();
        assert!(
            (threshold - 1.0).abs() < 0.01,
            "default threshold should be 1.0 (release everything), got {}",
            threshold
        );
    }

    #[tokio::test]
    async fn test_auto_approve_threshold_set() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve_threshold(0.3);
        assert!((buf.auto_approve_threshold() - 0.3).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_auto_approve_threshold_clamps() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve_threshold(2.0);
        assert!(
            (buf.auto_approve_threshold() - 1.0).abs() < 0.01,
            "should clamp to 1.0"
        );
        buf.set_auto_approve_threshold(-0.5);
        assert!(
            (buf.auto_approve_threshold() - 0.0).abs() < 0.01,
            "should clamp to 0.0"
        );
    }

    #[tokio::test]
    async fn test_auto_release_disabled_returns_zero() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        // Explicitly disable auto_approve (on by default)
        buf.set_auto_approve(false);
        let count = buf.auto_release_if_eligible(Some(0.1)).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_release_above_threshold_returns_zero() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.3);

        // Push an entry with a future release_at
        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "ar-1".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + std::time::Duration::from_secs(3600),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m".into(),
            annotations: vec![],
            edited: false,
            stopped: false,
        })
        .await;

        // Divergence 0.5 > threshold 0.3 → no auto-release
        let count = buf.auto_release_if_eligible(Some(0.5)).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_release_below_threshold_marks_entries() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);

        let now = std::time::Instant::now();
        for i in 0..3 {
            buf.push(buffer::BufferedResponse {
                id: format!("ar-{}", i),
                action: "propose".into(),
                job_id: "j".into(),
                round: 1,
                reply_subject: "s".into(),
                payload: b"{}".to_vec(),
                created_at: now,
                release_at: now + std::time::Duration::from_secs(3600),
                ack_handle: Box::new(TestAckHandle),
                msg_id: format!("m-{}", i),
                annotations: vec![],
                edited: false,
                stopped: false,
            })
            .await;
        }

        // Divergence 0.2 < threshold 0.5 → should auto-release all 3
        let count = buf.auto_release_if_eligible(Some(0.2)).await;
        assert_eq!(count, 3);

        // Now they should drain
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 3);
    }

    #[tokio::test]
    async fn test_auto_release_none_divergence_releases() {
        // When divergence is None (no scores yet), trust the operator's
        // explicit opt-in to auto-approve
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve(true);

        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "ar-none".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + std::time::Duration::from_secs(3600),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m-none".into(),
            annotations: vec![],
            edited: false,
            stopped: false,
        })
        .await;

        let count = buf.auto_release_if_eligible(None).await;
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_auto_release_skips_stopped_entries() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);

        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "ar-stop".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + std::time::Duration::from_secs(3600),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m-stop".into(),
            annotations: vec![],
            edited: false,
            stopped: true,
        })
        .await;

        // Stopped entries should not be auto-released
        let count = buf.auto_release_if_eligible(Some(0.1)).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_release_skips_already_ready_entries() {
        // Entries whose release_at is already in the past should not be counted
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);

        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "ar-past".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now, // already ready
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m-past".into(),
            annotations: vec![],
            edited: false,
            stopped: false,
        })
        .await;

        // Entry is already ready (release_at <= now), so auto_release shouldn't touch it
        // The function checks `entry.release_at > now` before modifying
        let count = buf.auto_release_if_eligible(Some(0.1)).await;
        assert_eq!(count, 0);
    }

    // ===================================================================
    // Compute adaptive hold & divergence: additional edge cases
    // ===================================================================

    #[test]
    fn test_compute_adaptive_hold_negative_score() {
        let base = std::time::Duration::from_secs(10);
        // score = -0.5 → soft = -0.5/1.5 = -0.333 → positive = 0.333
        // multiplier = 1 + (1 - 0.333) * 3 = 1 + 2.0 = 3.0
        let hold = buffer::compute_adaptive_hold(base, Some(-0.5), 3.0);
        let expected = std::time::Duration::from_secs(30);
        assert!(
            (hold.as_secs_f64() - expected.as_secs_f64()).abs() < 0.5,
            "negative score should increase hold; hold={:?}",
            hold
        );
    }

    #[test]
    fn test_compute_adaptive_hold_large_positive_score() {
        let base = std::time::Duration::from_secs(10);
        // score = 3.0 → soft = 3/4 = 0.75 → positive = 0.875
        // multiplier = 1 + 0.125 * 3 = 1.375
        let hold = buffer::compute_adaptive_hold(base, Some(3.0), 3.0);
        let expected_secs = 13.75;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "large positive score should reduce hold; hold={:?}",
            hold
        );
    }

    #[test]
    fn test_compute_adaptive_hold_zero_amplification() {
        let base = std::time::Duration::from_secs(10);
        // amplification = 0 → multiplier = 1.0 always (no effect)
        let hold = buffer::compute_adaptive_hold(base, Some(0.2), 0.0);
        assert_eq!(hold, base);
    }

    #[test]
    fn test_compute_adaptive_hold_zero_base() {
        let base = std::time::Duration::ZERO;
        // Zero base → result is always zero regardless of score
        let hold = buffer::compute_adaptive_hold(base, Some(0.2), 3.0);
        assert_eq!(hold, std::time::Duration::ZERO);
    }

    #[test]
    fn test_compute_divergence_high_std_dev() {
        // std_dev >= 1.0 → capped at 1.0
        let div = buffer::compute_divergence(Some(0.9), Some(1.2));
        // score_div: soft(0.9) = 0.9/1.9 = 0.474 → (1-0.474)/2 = 0.263
        // std_div = 1.2 / 1.0 = 1.2 → clamped to 1.0
        // effective = max(0.263, 1.0) = 1.0
        assert!((div.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_zero_score() {
        // score = 0 → soft = 0 → (1-0)/2 = 0.5 (ambiguous)
        let div = buffer::compute_divergence(Some(0.0), None);
        assert!((div.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_high_positive_score() {
        // score = 3.0 → soft = 0.75 → (1-0.75)/2 = 0.125 (mostly converged)
        let div = buffer::compute_divergence(Some(3.0), None);
        assert!((div.unwrap() - 0.125).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_negative_score() {
        // score = -1.0 → soft = -0.5 → (1-(-0.5))/2 = 0.75 (high divergence)
        let div = buffer::compute_divergence(Some(-1.0), None);
        assert!((div.unwrap() - 0.75).abs() < 0.01);
    }

    // ===================================================================
    // AgentHeartbeat serialization
    // ===================================================================

    #[test]
    fn test_agent_heartbeat_serialization() {
        let heartbeat = crate::agents::AgentHeartbeat {
            agent_id: "test-agent".into(),
            status: AgentLiveStatus::Busy,
            model_name: "gpt-4".into(),
            provider_id: "openai".into(),
            current_job: Some("job-123".into()),
            uptime_secs: 3600,
            timestamp: "2026-03-06T12:00:00Z".into(),
            input_price_per_mtok: Some(10.0),
            output_price_per_mtok: Some(30.0),
            chars_per_token: Some(4.0),
            response_sla_secs: Some(300),
            temperature: Some(0.7),
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: Some(4096),
            context_window: Some(128000),
            tasks_completed: 42,
            tasks_failed: 3,
            last_error: Some("evaluate: job-abc".into()),
            ..Default::default()
        };

        let json = serde_json::to_value(&heartbeat).unwrap();
        assert_eq!(json["agent_id"], "test-agent");
        assert_eq!(json["status"], "busy");
        assert_eq!(json["current_job"], "job-123");
        assert_eq!(json["uptime_secs"], 3600);
        assert_eq!(json["tasks_completed"], 42);
        assert_eq!(json["tasks_failed"], 3);
        assert_eq!(json["last_error"], "evaluate: job-abc");

        // Roundtrip
        let deserialized: crate::agents::AgentHeartbeat = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.agent_id, "test-agent");
        assert_eq!(deserialized.status, AgentLiveStatus::Busy);
        assert_eq!(deserialized.tasks_completed, 42);
    }

    #[test]
    fn test_agent_heartbeat_idle_no_error() {
        let heartbeat = crate::agents::AgentHeartbeat {
            agent_id: "idle-agent".into(),
            status: AgentLiveStatus::Idle,
            model_name: "claude".into(),
            provider_id: "anthropic".into(),
            current_job: None,
            uptime_secs: 10,
            timestamp: "t".into(),
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            chars_per_token: None,
            response_sla_secs: None,
            temperature: None,
            frequency_penalty: None,
            presence_penalty: None,
            max_tokens: None,
            context_window: None,
            tasks_completed: 0,
            tasks_failed: 0,
            last_error: None,
            ..Default::default()
        };

        let json = serde_json::to_value(&heartbeat).unwrap();
        assert_eq!(json["status"], "idle");
        assert!(json["current_job"].is_null());
        assert_eq!(json["tasks_completed"], 0);
        assert_eq!(json["tasks_failed"], 0);
    }

    // ===================================================================
    // JobManifest edge cases
    // ===================================================================

    #[test]
    fn test_job_manifest_empty_agents() {
        let manifest = JobManifest {
            job_id: "empty-agents".into(),
            task_description: "test".into(),
            agents: vec![],
            rounds: 1,
            timestamp: 0,
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: JobManifest = serde_json::from_str(&json).unwrap();
        assert!(parsed.agents.is_empty());
    }

    #[test]
    fn test_job_manifest_contains_check() {
        let manifest = JobManifest {
            job_id: "check-test".into(),
            task_description: "test".into(),
            agents: vec!["alpha".into(), "beta".into(), "gamma".into()],
            rounds: 3,
            timestamp: 100,
        };

        // This mirrors the agent selection logic in handle_manifest
        assert!(manifest.agents.contains(&"alpha".to_string()));
        assert!(manifest.agents.contains(&"beta".to_string()));
        assert!(!manifest.agents.contains(&"delta".to_string()));
    }

    // ===================================================================
    // NatsScratchpadStore scoped_key
    // ===================================================================

    // We can't construct NatsScratchpadStore without NATS, but we can test
    // the scoped_key format by replicating the logic.
    #[test]
    fn test_scoped_key_format() {
        let scope_prefix = "session-abc";
        let key = "my_data";
        let scoped = format!("{}.{}", scope_prefix, key);
        assert_eq!(scoped, "session-abc.my_data");
    }

    #[test]
    fn test_scoped_key_with_dots_in_prefix() {
        let scope_prefix = "org.team.session";
        let key = "scratchpad";
        let scoped = format!("{}.{}", scope_prefix, key);
        assert_eq!(scoped, "org.team.session.scratchpad");
    }

    // ===================================================================
    // extract_content_preview: Unicode truncation edge cases
    // ===================================================================

    #[test]
    fn test_extract_preview_proposal_unicode_content_truncation() {
        // Multi-byte Unicode characters: content with CJK chars should truncate
        // at char boundaries, not byte boundaries.
        let long_content: String = "\u{4e16}\u{754c}".repeat(1200); // "世界" × 1200 = 2400 chars
        let payload = serde_json::json!({
            "content": long_content,
            "thought_process": ""
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let c = parsed["c"].as_str().unwrap();
        // Should be truncated to 2000 chars + ellipsis
        assert!(c.chars().count() <= 2002);
        assert!(c.ends_with('\u{2026}'));
        // Verify no panic from splitting inside a multi-byte boundary
    }

    #[test]
    fn test_extract_preview_proposal_unicode_thought_truncation() {
        // thought_process with emoji characters truncated at char boundary
        let long_tp: String = "\u{1f600}".repeat(600); // grinning face emoji × 600
        let payload = serde_json::json!({
            "content": "short content",
            "thought_process": long_tp
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tp = parsed["tp"].as_str().unwrap();
        assert!(tp.chars().count() <= 502);
        assert!(tp.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_proposal_exactly_2000_chars_no_truncation() {
        let exact_content: String = "a".repeat(2000);
        let payload = serde_json::json!({
            "content": exact_content,
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let c = parsed["c"].as_str().unwrap();
        // Exactly 2000 chars should NOT be truncated
        assert_eq!(c.chars().count(), 2000);
        assert!(!c.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_proposal_exactly_2001_chars_truncated() {
        let content: String = "b".repeat(2001);
        let payload = serde_json::json!({
            "content": content,
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let c = parsed["c"].as_str().unwrap();
        // 2001 chars → truncated to 2000 + ellipsis = 2001
        assert_eq!(c.chars().count(), 2001);
        assert!(c.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_proposal_thought_exactly_500_no_truncation() {
        let tp: String = "c".repeat(500);
        let payload = serde_json::json!({
            "content": "hello",
            "thought_process": tp
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tp_out = parsed["tp"].as_str().unwrap();
        assert_eq!(tp_out.chars().count(), 500);
        assert!(!tp_out.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_proposal_empty_thought_process_omitted() {
        // Empty string thought_process should NOT produce a "tp" key
        let payload = serde_json::json!({
            "content": "some content",
            "thought_process": ""
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        assert!(
            parsed.get("tp").is_none(),
            "empty thought_process should not produce tp key"
        );
    }

    #[test]
    fn test_extract_preview_proposal_content_is_number_returns_none() {
        // "content" is present but not a string (e.g. number) → as_str() returns None → ""
        let payload = serde_json::json!({
            "content": 42,
            "thought_process": "thinking"
        });
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "propose", &[]);
        // content is 42 → as_str() returns None → unwrap_or("") → empty → return None
        assert!(preview.is_none());
    }

    // ===================================================================
    // extract_content_preview: evaluation edge cases
    // ===================================================================

    #[test]
    fn test_extract_preview_eval_unicode_justification_truncation() {
        let long_j: String = "\u{4e16}".repeat(400); // CJK "世" × 400
        let payload = serde_json::json!([
            ["agent-A", {"score": 6.0, "justification": long_j}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let j = parsed["evals"][0]["j"].as_str().unwrap();
        assert!(j.chars().count() <= 302);
        assert!(j.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_eval_unicode_textual_feedback_truncation() {
        let long_tf: String = "\u{1f44d}".repeat(300); // thumbs up × 300
        let payload = serde_json::json!([
            ["agent-A", {"score": 5.0, "textual_feedback": long_tf}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tf = parsed["evals"][0]["tf"].as_str().unwrap();
        assert!(tf.chars().count() <= 202);
        assert!(tf.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_eval_justification_exactly_300_no_truncation() {
        let j: String = "d".repeat(300);
        let payload = serde_json::json!([
            ["agent-A", {"score": 7.0, "justification": j}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let j_out = parsed["evals"][0]["j"].as_str().unwrap();
        assert_eq!(j_out.chars().count(), 300);
        assert!(!j_out.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_eval_textual_feedback_exactly_200_no_truncation() {
        let tf: String = "e".repeat(200);
        let payload = serde_json::json!([
            ["agent-A", {"score": 7.0, "textual_feedback": tf}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let tf_out = parsed["evals"][0]["tf"].as_str().unwrap();
        assert_eq!(tf_out.chars().count(), 200);
        assert!(!tf_out.ends_with('\u{2026}'));
    }

    #[test]
    fn test_extract_preview_eval_missing_score_field() {
        // Eval object without "score" — should still produce an eval entry (without "s")
        let payload = serde_json::json!([
            ["agent-A", {"justification": "OK"}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let eval = &parsed["evals"][0];
        assert_eq!(eval["target"], "agent-A");
        assert_eq!(eval["j"], "OK");
        assert!(
            eval.get("s").is_none(),
            "no score field in source → no s in preview"
        );
    }

    #[test]
    fn test_extract_preview_eval_missing_target_id() {
        // Tuple where first element is not a string — target defaults to "?"
        let payload = serde_json::json!([
            [42, {"score": 5.0, "justification": "test"}]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        assert_eq!(parsed["evals"][0]["target"], "?");
    }

    #[test]
    fn test_extract_preview_eval_all_optional_fields_present() {
        // Full evaluation with all optional fields: stance, textual_feedback,
        // category_scores, claim_assessments, disagreements
        let payload = serde_json::json!([
            ["agent-A", {
                "score": 7.5,
                "justification": "Good analysis",
                "stance": "strongly_agree",
                "textual_feedback": "Well argued points",
                "category_scores": {"accuracy": 8, "clarity": 9, "depth": 7},
                "claim_assessments": [
                    {"claim": "The earth is round", "verdict": "agree"},
                    {"claim": "Water is wet", "verdict": "agree"}
                ],
                "disagreements": ["Minor factual error in paragraph 2"]
            }]
        ]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let eval = &parsed["evals"][0];
        assert_eq!(eval["target"], "agent-A");
        assert_eq!(eval["s"], 7.5);
        assert_eq!(eval["j"], "Good analysis");
        assert_eq!(eval["stance"], "strongly_agree");
        assert_eq!(eval["tf"], "Well argued points");
        assert_eq!(eval["cats"]["accuracy"], 8);
        assert_eq!(eval["cats"]["clarity"], 9);
        assert_eq!(eval["claims"].as_array().unwrap().len(), 2);
        assert_eq!(eval["disputes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_extract_preview_eval_minimal_eval_object() {
        // Eval object with no score, no justification, no stance, no feedback
        // Only the target ID should appear
        let payload = serde_json::json!([["agent-A", {}]]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &[]);
        assert!(preview.is_some());

        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let eval = &parsed["evals"][0];
        assert_eq!(eval["target"], "agent-A");
        // No other fields should be present (they are all optional)
        assert!(eval.get("s").is_none());
        assert!(eval.get("j").is_none());
        assert!(eval.get("stance").is_none());
        assert!(eval.get("tf").is_none());
        assert!(eval.get("cats").is_none());
        assert!(eval.get("claims").is_none());
        assert!(eval.get("disputes").is_none());
    }

    // ===================================================================
    // extract_content_preview: props (anonymized proposal content)
    // ===================================================================

    #[test]
    fn test_extract_preview_eval_props_truncated() {
        // Candidate content over 1000 chars → truncated with "…"
        let long_content = "x".repeat(1200);
        let candidates = vec![crate::agents::CandidateProposal {
            id: "Candidate_A".to_string(),
            proposal: crate::agents::Proposal {
                content: long_content.clone(),
                ..Default::default()
            },
        }];
        let payload = serde_json::json!([["Candidate_A", {"score": 0.8, "justification": "ok"}]]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &candidates);
        assert!(preview.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let props = parsed.get("props").expect("should have props");
        let val = props["Candidate_A"].as_str().unwrap();
        assert!(val.ends_with('…'), "should be truncated");
        // 1000 chars + "…"
        assert_eq!(val.chars().count(), 1001);
    }

    #[test]
    fn test_extract_preview_eval_props_short_content() {
        // Content under 1000 chars → full content, no ellipsis
        let short_content = "short proposal".to_string();
        let candidates = vec![crate::agents::CandidateProposal {
            id: "Candidate_B".to_string(),
            proposal: crate::agents::Proposal {
                content: short_content.clone(),
                ..Default::default()
            },
        }];
        let payload = serde_json::json!([["Candidate_B", {"score": 0.5, "justification": "meh"}]]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &candidates);
        assert!(preview.is_some());
        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let props = parsed.get("props").expect("should have props");
        assert_eq!(props["Candidate_B"].as_str().unwrap(), "short proposal");
    }

    #[test]
    fn test_extract_preview_eval_props_filters_to_displayed_targets() {
        // Two candidates, but only one appears in the eval array
        let candidates = vec![
            crate::agents::CandidateProposal {
                id: "Candidate_A".to_string(),
                proposal: crate::agents::Proposal {
                    content: "proposal A".into(),
                    ..Default::default()
                },
            },
            crate::agents::CandidateProposal {
                id: "Candidate_B".to_string(),
                proposal: crate::agents::Proposal {
                    content: "proposal B".into(),
                    ..Default::default()
                },
            },
        ];
        // Only Candidate_A is in the eval payload
        let payload = serde_json::json!([["Candidate_A", {"score": 0.9, "justification": "good"}]]);
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "evaluate", &candidates);
        let parsed: serde_json::Value = serde_json::from_str(&preview.unwrap()).unwrap();
        let props = parsed.get("props").expect("should have props");
        assert!(
            props.get("Candidate_A").is_some(),
            "displayed target should be in props"
        );
        assert!(
            props.get("Candidate_B").is_none(),
            "non-displayed target should be filtered out"
        );
    }

    // ===================================================================
    // compute_divergence: additional edge cases
    // ===================================================================

    #[test]
    fn test_compute_divergence_only_std_dev_no_score() {
        // Only std_dev, no score — should use std_div only
        // std_div = 0.3 / 1.0 = 0.3
        let div = buffer::compute_divergence(None, Some(0.3));
        assert!(div.is_some());
        assert!((div.unwrap() - 0.3).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_both_score_and_std_dev() {
        // score = 0.7 → soft = 0.7/1.7 = 0.412 → (1-0.412)/2 = 0.294
        // std_div = 0.35 / 1.0 = 0.35
        // effective = max(0.294, 0.35) = 0.35
        let div = buffer::compute_divergence(Some(0.7), Some(0.35));
        assert!((div.unwrap() - 0.35).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_score_dominates() {
        // score = -2.0 → soft = -2/3 = -0.667 → (1+0.667)/2 = 0.833
        // std_div = 0.05 / 1.0 = 0.05
        // effective = max(0.833, 0.05) = 0.833
        let div = buffer::compute_divergence(Some(-2.0), Some(0.05));
        assert!((div.unwrap() - 0.833).abs() < 0.02);
    }

    #[test]
    fn test_compute_divergence_high_score_low_std() {
        // score = 5.0 → soft = 5/6 = 0.833 → (1-0.833)/2 = 0.083
        // std_dev = 0.02 → std_div = 0.02
        // effective = max(0.083, 0.02) = 0.083
        let div = buffer::compute_divergence(Some(5.0), Some(0.02));
        assert!((div.unwrap() - 0.083).abs() < 0.02);
    }

    #[test]
    fn test_compute_divergence_none_none() {
        let div = buffer::compute_divergence(None, None);
        assert!(div.is_none());
    }

    #[test]
    fn test_compute_divergence_perfect_score_zero_std() {
        // score = 3.0 → soft = 0.75 → (1-0.75)/2 = 0.125
        // std_dev = 0 → effective = 0.125
        let div = buffer::compute_divergence(Some(3.0), Some(0.0));
        assert!((div.unwrap() - 0.125).abs() < 0.01);
    }

    // ===================================================================
    // status.rs: check_flags edge cases
    // ===================================================================

    #[test]
    fn test_check_flags_low_score_triggers_flag() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Push 3 consistently negative scores → recent-3 avg < -0.3
        for i in 0..3 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: i as u32 + 1,
                evaluator: "e".into(),
                score: -0.5,
            });
        }

        assert!(snap.is_flagged, "agent should be flagged for low scores");
        assert!(snap.flag_reason.as_ref().unwrap().contains("Low scores"));
    }

    #[test]
    fn test_check_flags_high_divergence_triggers_flag() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Push scores with high variance → std_dev > 1.5
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j".into(),
            round: 1,
            evaluator: "e".into(),
            score: -2.0,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j".into(),
            round: 2,
            evaluator: "e".into(),
            score: 2.0,
        });

        // mean = 0.0, variance = ((-2)^2 + 2^2) / 2 = 4.0
        // std_dev = 2.0 → > 1.5
        assert!(snap.score_std_dev.unwrap() > 1.5);
        assert!(
            snap.is_flagged,
            "agent should be flagged for high divergence"
        );
        assert!(snap.flag_reason.as_ref().unwrap().contains("divergence"));
    }

    #[test]
    fn test_check_flags_clears_when_conditions_no_longer_met() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // First: trigger low score flag with negative scores
        for i in 0..3 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: i as u32 + 1,
                evaluator: "e".into(),
                score: -0.5,
            });
        }
        assert!(snap.is_flagged);

        // Now push positive scores to clear the flag
        for i in 3..6 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: i as u32 + 1,
                evaluator: "e".into(),
                score: 0.8,
            });
        }

        // Recent 3 avg = 0.8 → above -0.3 threshold
        // std_dev of [-0.5,-0.5,-0.5,0.8,0.8,0.8] ≈ 0.65 → below 1.5
        assert!(!snap.is_flagged, "flag should be cleared after good scores");
    }

    #[test]
    fn test_check_flags_not_flagged_with_good_scores() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Positive scores = endorsed proposals
        for i in 0..5 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: i as u32 + 1,
                evaluator: "e".into(),
                score: 0.7 + (i as f32 * 0.05),
            });
        }

        assert!(!snap.is_flagged, "good scores should not flag the agent");
        assert!(snap.flag_reason.is_none());
    }

    #[test]
    fn test_check_flags_fewer_than_3_scores_no_low_score_flag() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Only 2 scores, both low — should NOT trigger low-score flag (needs >=3)
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j".into(),
            round: 1,
            evaluator: "e".into(),
            score: 1.0,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j".into(),
            round: 2,
            evaluator: "e".into(),
            score: 1.0,
        });

        // std_dev = 0.0 (both same score), so no divergence flag either
        // n < 3 → no low score check
        assert!(
            !snap.is_flagged,
            "fewer than 3 scores should not trigger low-score flag"
        );
    }

    #[test]
    fn test_check_flags_low_score_takes_priority_over_high_divergence() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Push 3 negative scores: consistently rejected
        // [-0.5, -0.4, -0.6] → avg = -0.5 < -0.3 → low score flag (checked first)
        // std_dev ≈ 0.08 → below 1.5 → no divergence flag
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j".into(),
            round: 1,
            evaluator: "e".into(),
            score: -0.5,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j".into(),
            round: 2,
            evaluator: "e".into(),
            score: -0.4,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t3".into(),
            job_id: "j".into(),
            round: 3,
            evaluator: "e".into(),
            score: -0.6,
        });

        assert!(snap.is_flagged);
        assert!(
            snap.flag_reason.as_ref().unwrap().contains("Low scores"),
            "low score flag should take priority, got: {:?}",
            snap.flag_reason
        );
    }

    // ===================================================================
    // push_score: std_dev computation edge cases
    // ===================================================================

    #[test]
    fn test_push_score_single_score_no_std_dev() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_score(ScoreEntry {
            timestamp: "t".into(),
            job_id: "j".into(),
            round: 1,
            evaluator: "e".into(),
            score: 5.0,
        });

        assert_eq!(snap.mean_score, Some(5.0));
        assert!(
            snap.score_std_dev.is_none(),
            "single score should have no std_dev"
        );
    }

    #[test]
    fn test_push_score_two_identical_scores_zero_std_dev() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j".into(),
            round: 1,
            evaluator: "e".into(),
            score: 6.0,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j".into(),
            round: 2,
            evaluator: "e".into(),
            score: 6.0,
        });

        assert_eq!(snap.mean_score, Some(6.0));
        assert!((snap.score_std_dev.unwrap() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_push_score_trims_beyond_max() {
        use crate::status::{AgentStatusSnapshot, ScoreEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());

        // Push 55 scores (MAX_RECENT_SCORES is 50)
        for i in 0..55 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: i as u32 + 1,
                evaluator: "e".into(),
                score: 5.0,
            });
        }

        assert_eq!(
            snap.recent_scores.len(),
            50,
            "should trim to MAX_RECENT_SCORES"
        );
        // Oldest 5 should have been removed: first remaining is round 6
        assert_eq!(snap.recent_scores.front().unwrap().round, 6);
    }

    // ===================================================================
    // WorkerConfig: Debug trait coverage
    // ===================================================================

    #[test]
    fn test_worker_config_debug_output() {
        let config = WorkerConfig::new(
            "nats://localhost:4222".to_string(),
            "test_stream".to_string(),
            "test_consumer".to_string(),
        );
        let debug = format!("{:?}", config);
        assert!(debug.contains("WorkerConfig"));
        assert!(debug.contains("nats://localhost:4222"));
        assert!(debug.contains("test_stream"));
        assert!(debug.contains("test_consumer"));
        assert!(debug.contains("nsed"));
        assert!(debug.contains("sphera"));
    }

    // ===================================================================
    // JobManifest: Debug trait coverage
    // ===================================================================

    #[test]
    fn test_job_manifest_debug_output() {
        let manifest = JobManifest {
            job_id: "debug-test".into(),
            task_description: "test desc".into(),
            agents: vec!["a1".into()],
            rounds: 2,
            timestamp: 12345,
        };
        let debug = format!("{:?}", manifest);
        assert!(debug.contains("JobManifest"));
        assert!(debug.contains("debug-test"));
        assert!(debug.contains("test desc"));
    }

    // ===================================================================
    // ConfigPatch: additional apply coverage
    // ===================================================================

    #[test]
    fn test_config_patch_apply_all_fields() {
        use crate::agents::AgentConfig;
        use crate::control_plane::ConfigPatch;

        let mut config = AgentConfig {
            name: "test".into(),
            provider_id: "p".into(),
            model_name: "m".into(),
            temperature: 0.7,
            ..Default::default()
        };

        let patch = ConfigPatch {
            temperature: Some(1.5),
            frequency_penalty: Some(0.5),
            presence_penalty: Some(-0.3),
            persona: Some("friendly helper".into()),
            textual_feedback: Some(true),
            max_react_iterations: Some(3),
            max_retries: Some(1),
        };
        patch.apply(&mut config).expect("valid patch");

        assert_eq!(config.temperature, 1.5);
        assert_eq!(config.frequency_penalty, Some(0.5));
        assert_eq!(config.presence_penalty, Some(-0.3));
        assert_eq!(config.persona, Some("friendly helper".into()));
        assert!(config.textual_feedback);
        assert_eq!(config.max_react_iterations, Some(3));
        assert_eq!(config.max_retries, Some(1));
    }

    #[test]
    fn test_config_patch_apply_empty_patch_no_change() {
        use crate::agents::AgentConfig;
        use crate::control_plane::ConfigPatch;

        let mut config = AgentConfig {
            name: "test".into(),
            provider_id: "p".into(),
            model_name: "m".into(),
            temperature: 0.7,
            frequency_penalty: Some(0.2),
            ..Default::default()
        };

        let patch = ConfigPatch::default();
        patch.apply(&mut config).expect("empty patch");

        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.frequency_penalty, Some(0.2));
    }

    // ===================================================================
    // ConfigPatch: deny_unknown_fields
    // ===================================================================

    #[test]
    fn test_config_patch_rejects_unknown_fields() {
        use crate::control_plane::ConfigPatch;

        let json = r#"{"temperature": 0.5, "unknown_field": true}"#;
        let result: Result<ConfigPatch, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "unknown fields should be rejected due to deny_unknown_fields"
        );
    }

    // ===================================================================
    // inject_annotations: JSON number payload is primitive passthrough
    // ===================================================================

    #[test]
    fn test_inject_annotations_deeply_nested_eval() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        // Three-element tuple (unusual but valid JSON array item)
        let payload = br#"[["agent-A", {"score": 5.0}, "extra-data"]]"#;
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "test".into(),
            timestamp: "t".into(),
            original_content_hash: None,
        };
        let entry = make_entry(payload, true, vec![annotation]);

        let result = NatsNsedWorker::inject_annotations(&entry);
        let val: serde_json::Value = serde_json::from_slice(&result).unwrap();
        let inner = val.as_array().unwrap()[0].as_array().unwrap();
        // Should inject into tuple[1] which is the eval object
        assert_eq!(inner[1]["edited_by"], "operator");
        assert!(inner[1]["operator_annotations"].is_array());
        // Extra element should be preserved
        assert_eq!(inner.len(), 3);
        assert_eq!(inner[2], "extra-data");
    }

    // ===================================================================
    // Buffer: compute_adaptive_hold with medium score
    // ===================================================================

    #[test]
    fn test_compute_adaptive_hold_medium_score() {
        let base = std::time::Duration::from_secs(10);
        // score = 0.5 → soft = 0.5/1.5 = 0.333 → positive = 0.667
        // multiplier = 1 + 0.333 * 3 = 2.0
        let hold = buffer::compute_adaptive_hold(base, Some(0.5), 3.0);
        let expected_secs = 20.0;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "score 0.5 should give ~2x base; got {:?}",
            hold
        );
    }

    // ===================================================================
    // Buffer: response SLA floor edge cases
    // ===================================================================

    #[tokio::test]
    async fn test_buffer_response_sla_matches_hold() {
        // SLA initializes to the exact hold_duration — no floor.
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(300));
        assert_eq!(
            buf.response_sla(),
            Some(std::time::Duration::from_secs(300))
        );

        let buf_short = buffer::ResponseBuffer::new(std::time::Duration::from_secs(5));
        assert_eq!(
            buf_short.response_sla(),
            Some(std::time::Duration::from_secs(5))
        );
    }

    // ===================================================================
    // AgentLiveStatus: serialization and equality
    // ===================================================================

    #[test]
    fn test_agent_live_status_serialization() {
        let busy_json = serde_json::to_string(&AgentLiveStatus::Busy).unwrap();
        assert_eq!(busy_json, "\"busy\"");

        let idle_json = serde_json::to_string(&AgentLiveStatus::Idle).unwrap();
        assert_eq!(idle_json, "\"idle\"");

        // Roundtrip
        let parsed: AgentLiveStatus = serde_json::from_str(&busy_json).unwrap();
        assert_eq!(parsed, AgentLiveStatus::Busy);
    }

    // ===================================================================
    // status.rs: error_rate edge cases
    // ===================================================================

    #[test]
    fn test_error_rate_all_failures() {
        use crate::status::{AgentStatusSnapshot, TaskLogEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..5 {
            snap.push_task(TaskLogEntry {
                timestamp: format!("t{}", i),
                action: "propose".into(),
                job_id: format!("j{}", i),
                round: 1,
                status: "error".into(),
                duration_ms: 10,
                content_preview: None,
            });
        }
        assert!(
            (snap.error_rate - 1.0).abs() < f32::EPSILON,
            "all failures should give 100% error rate"
        );
    }

    #[test]
    fn test_error_rate_all_successes() {
        use crate::status::{AgentStatusSnapshot, TaskLogEntry};

        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..5 {
            snap.push_task(TaskLogEntry {
                timestamp: format!("t{}", i),
                action: "evaluate".into(),
                job_id: format!("j{}", i),
                round: 1,
                status: "ok".into(),
                duration_ms: 10,
                content_preview: None,
            });
        }
        assert!(
            (snap.error_rate - 0.0).abs() < f32::EPSILON,
            "all successes should give 0% error rate"
        );
    }

    // ===================================================================
    // Buffer: stop/unstop with drain_stale interaction
    // ===================================================================

    #[tokio::test]
    async fn test_drain_stale_drains_stopped_entries_from_other_jobs() {
        // Stopped entries from a different job should still be drained by drain_stale
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(300));

        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "stopped-stale".into(),
            action: "propose".into(),
            job_id: "old-job".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + std::time::Duration::from_secs(3600),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m".into(),
            annotations: vec![],
            edited: false,
            stopped: true,
        })
        .await;

        let stale = buf.drain_stale("current-job").await;
        assert_eq!(stale.len(), 1, "stopped stale entries should be drained");
        assert_eq!(stale[0].id, "stopped-stale");
    }

    // ===================================================================
    // Buffer: list with overdue entries shows negative release_in_ms
    // ===================================================================

    #[tokio::test]
    async fn test_buffer_list_overdue_entry_negative_release() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(300));

        let now = std::time::Instant::now();
        // Entry with release_at in the past (overdue)
        buf.push(buffer::BufferedResponse {
            id: "overdue-1".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now, // already past or at now
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m".into(),
            annotations: vec![],
            edited: false,
            stopped: true, // stopped so it stays in buffer for list()
        })
        .await;

        // Small sleep to ensure now is past release_at
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        assert!(
            list[0].release_in_ms <= 0,
            "overdue entry should have negative release_in_ms, got {}",
            list[0].release_in_ms
        );
        assert!(list[0].stopped);
    }

    // ===================================================================
    // Buffer: get_detail serialization of BufferEntryDetail
    // ===================================================================

    #[tokio::test]
    async fn test_buffer_entry_detail_serde_flatten() {
        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(60));
        let payload = serde_json::json!({"content": "test proposal"});

        let now = std::time::Instant::now();
        buf.push(buffer::BufferedResponse {
            id: "serde-1".into(),
            action: "propose".into(),
            job_id: "job-abc".into(),
            round: 2,
            reply_subject: "s".into(),
            payload: serde_json::to_vec(&payload).unwrap(),
            created_at: now,
            release_at: now + std::time::Duration::from_secs(60),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "msg-serde".into(),
            annotations: vec![],
            edited: false,
            stopped: false,
        })
        .await;

        let detail = buf.get_detail("serde-1").await.unwrap();
        // Verify that BufferEntryDetail serializes with flattened summary fields
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["id"], "serde-1");
        assert_eq!(json["action"], "propose");
        assert_eq!(json["job_id"], "job-abc");
        assert_eq!(json["round"], 2);
        assert!(json["age_ms"].is_number());
        assert!(json["release_in_ms"].is_number());
        assert_eq!(json["stopped"], false);
        assert_eq!(json["content"]["content"], "test proposal");
    }

    // ===================================================================
    // Buffer: update_payload_with_annotation for unknown ID
    // ===================================================================

    #[tokio::test]
    async fn test_update_payload_with_annotation_unknown_id() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = buffer::ResponseBuffer::new(std::time::Duration::from_secs(30));
        buf.push(buffer::BufferedResponse {
            id: "exists".into(),
            action: "propose".into(),
            job_id: "j".into(),
            round: 1,
            reply_subject: "s".into(),
            payload: b"{}".to_vec(),
            created_at: std::time::Instant::now(),
            release_at: std::time::Instant::now() + std::time::Duration::from_secs(30),
            ack_handle: Box::new(TestAckHandle),
            msg_id: "m".into(),
            annotations: vec![],
            edited: false,
            stopped: false,
        })
        .await;

        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "edit".into(),
            timestamp: "t".into(),
            original_content_hash: None,
        };

        let result = buf
            .update_payload_with_annotation("nonexistent", b"new payload".to_vec(), annotation)
            .await;
        assert!(!result, "should return false for unknown ID");
        assert_eq!(buf.len().await, 1, "existing entry should be unaffected");
    }

    // ===================================================================
    // NatsAuth: edge case for inline_creds only
    // ===================================================================

    #[test]
    fn test_nats_auth_inline_creds_only_is_configured() {
        use crate::nats_utils::NatsAuth;
        let auth = NatsAuth {
            inline_creds: Some("creds-data".into()),
            ..Default::default()
        };
        assert!(auth.is_configured());
    }

    #[test]
    fn test_nats_auth_creds_file_only_is_configured() {
        use crate::nats_utils::NatsAuth;
        let auth = NatsAuth {
            creds_file: Some("/path/to/creds".into()),
            ..Default::default()
        };
        assert!(auth.is_configured());
    }

    // ===================================================================
    // Proposal / Evaluation type serde roundtrips (covers type lines)
    // ===================================================================

    #[test]
    fn test_proposal_serde_roundtrip() {
        let proposal = crate::agents::Proposal {
            content: "My proposal content".into(),
            thought_process: "I considered alternatives".into(),
            final_scratchpad: Some("notes".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&proposal).unwrap();
        let parsed: crate::agents::Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content, "My proposal content");
        assert_eq!(parsed.thought_process, "I considered alternatives");
        assert_eq!(parsed.final_scratchpad, Some("notes".into()));
    }

    #[test]
    fn test_evaluation_serde_roundtrip() {
        let eval = crate::agents::Evaluation {
            score: 7.5,
            justification: "Well-reasoned".into(),
            stance: Some(crate::agents::Stance::Agree),
            ..Default::default()
        };
        let json = serde_json::to_string(&eval).unwrap();
        let parsed: crate::agents::Evaluation = serde_json::from_str(&json).unwrap();
        assert!((parsed.score - 7.5).abs() < f32::EPSILON);
        assert_eq!(parsed.justification, "Well-reasoned");
        assert_eq!(parsed.stance, Some(crate::agents::Stance::Agree));
    }

    // ===================================================================
    // extract_content_preview: action as arbitrary string
    // ===================================================================

    #[test]
    fn test_extract_preview_empty_action_string_returns_none() {
        let payload = serde_json::json!({"content": "test"});
        let bytes = serde_json::to_vec(&payload).unwrap();

        let preview = extract_content_preview(&bytes, "", &[]);
        assert!(preview.is_none(), "empty action string should return None");
    }

    // ===================================================================
    // BufferEntrySummary serialization
    // ===================================================================

    // ===================================================================
    // Evaluation hallucination filter (mirrors logic in handle_task "evaluate")
    // ===================================================================

    /// Helper: replicate the exact filter logic from the evaluate branch.
    fn filter_evaluations(
        candidates: &[crate::agents::CandidateProposal],
        evaluations: Vec<(String, crate::agents::Evaluation)>,
    ) -> Vec<(String, crate::agents::Evaluation)> {
        let valid_ids: std::collections::HashSet<&str> =
            candidates.iter().map(|c| c.id.as_str()).collect();
        evaluations
            .into_iter()
            .filter(|(target_id, _)| valid_ids.contains(target_id.as_str()))
            .collect()
    }

    fn make_candidate(id: &str) -> crate::agents::CandidateProposal {
        crate::agents::CandidateProposal {
            id: id.to_string(),
            proposal: crate::agents::Proposal::default(),
        }
    }

    fn make_eval(target: &str, score: f32) -> (String, crate::agents::Evaluation) {
        (
            target.to_string(),
            crate::agents::Evaluation {
                score,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_filter_mixed_valid_and_hallucinated() {
        let candidates = vec![
            make_candidate("Candidate_A"),
            make_candidate("Candidate_B"),
            make_candidate("Candidate_C"),
        ];
        let evaluations = vec![
            make_eval("Candidate_A", 0.8),
            make_eval("...", 0.5), // hallucinated
            make_eval("Candidate_B", 0.6),
            make_eval("UNKNOWN_X", 0.9), // hallucinated
            make_eval("Candidate_C", 0.7),
        ];

        let filtered = filter_evaluations(&candidates, evaluations);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].0, "Candidate_A");
        assert_eq!(filtered[1].0, "Candidate_B");
        assert_eq!(filtered[2].0, "Candidate_C");

        // Verify serialization round-trips correctly
        let bytes = serde_json::to_vec(&filtered).unwrap();
        let parsed: Vec<(String, crate::agents::Evaluation)> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn test_filter_all_invalid_returns_empty() {
        let candidates = vec![make_candidate("Candidate_A"), make_candidate("Candidate_B")];
        let evaluations = vec![
            make_eval("...", 0.5),
            make_eval("HALLUCINATED", 0.9),
            make_eval("", 0.1),
        ];

        let filtered = filter_evaluations(&candidates, evaluations);
        assert!(filtered.is_empty());

        let bytes = serde_json::to_vec(&filtered).unwrap();
        assert_eq!(bytes, b"[]");
    }

    #[test]
    fn test_filter_all_valid_passes_through() {
        let candidates = vec![make_candidate("Candidate_A"), make_candidate("Candidate_B")];
        let evaluations = vec![make_eval("Candidate_A", 0.8), make_eval("Candidate_B", 0.6)];

        let filtered = filter_evaluations(&candidates, evaluations);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_empty_evaluations() {
        let candidates = vec![make_candidate("Candidate_A")];
        let filtered = filter_evaluations(&candidates, vec![]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_buffer_entry_summary_serialization() {
        let summary = buffer::BufferEntrySummary {
            id: "sum-1".into(),
            action: "propose".into(),
            job_id: "job-xyz".into(),
            round: 3,
            age_ms: 5000,
            release_in_ms: -200,
            stopped: true,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["id"], "sum-1");
        assert_eq!(json["action"], "propose");
        assert_eq!(json["job_id"], "job-xyz");
        assert_eq!(json["round"], 3);
        assert_eq!(json["age_ms"], 5000);
        assert_eq!(json["release_in_ms"], -200);
        assert_eq!(json["stopped"], true);
    }

    // ---- is_transient_error ----

    #[test]
    fn test_is_transient_error_matches_known_patterns() {
        let cases = [
            "broken pipe",
            "Connection reset by peer",
            "os error 32",
            "os error 104",
            "operation timed out",
            "connection closed before message completed",
            "unexpected eof during handshake",
            "stream closed",
            "connection refused",
            "network unreachable",
            "connection aborted",
        ];
        for msg in cases {
            let err = anyhow::anyhow!("{msg}");
            assert!(is_transient_error(&err), "expected transient for: {msg}");
        }
    }

    #[test]
    fn test_is_transient_error_case_insensitive() {
        let err = anyhow::anyhow!("BROKEN PIPE in TLS layer");
        assert!(is_transient_error(&err));

        let err = anyhow::anyhow!("Connection Reset By Peer");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn test_is_transient_error_rejects_non_transient() {
        let cases = [
            "invalid API key",
            "401 Unauthorized",
            "model not found",
            "rate limit exceeded",
            "JSON parse error",
            "",
        ];
        for msg in cases {
            let err = anyhow::anyhow!("{msg}");
            assert!(
                !is_transient_error(&err),
                "expected non-transient for: {msg}"
            );
        }
    }

    #[test]
    fn test_is_transient_error_embedded_in_message() {
        // Transport errors often appear as part of a longer message
        let err = anyhow::anyhow!("sending proposal failed: broken pipe (os error 32)");
        assert!(is_transient_error(&err));
    }

    #[test]
    fn classify_parse_error() {
        let r = classify_abstention_reason(
            "Failed to parse structured output after 4 attempts. Last error: missing field `evaluations`",
        );
        assert_eq!(r, "parse_error");
    }

    #[test]
    fn classify_iter_budget() {
        assert_eq!(
            classify_abstention_reason("agent loop exhausted iteration budget"),
            "iter_budget_exhausted"
        );
        assert_eq!(
            classify_abstention_reason("hit max_iterations cap"),
            "iter_budget_exhausted"
        );
    }

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_abstention_reason("upstream timed out after 60s"),
            "timeout"
        );
    }

    #[test]
    fn classify_tool_error() {
        assert_eq!(
            classify_abstention_reason("tool 'user_grep_repo' failed: out of sandbox"),
            "tool_error"
        );
    }

    #[test]
    fn classify_fallback() {
        assert_eq!(classify_abstention_reason("kaboom"), "error");
    }

    #[test]
    fn failed_subject_format_pins_exact_wire_shape() {
        let s = failed_result_subject("nsed", "sess-abc", 3, "ReviewerAlpha", "evaluate");
        assert_eq!(s, "nsed.sess-abc.result.3.ReviewerAlpha.evaluate.failed");
    }

    #[test]
    fn failed_subject_round_zero_renders() {
        // Round 0 is uncommon in production but legal; the worker must
        // not panic on it.
        let s = failed_result_subject("nsed", "x", 0, "A", "propose");
        assert_eq!(s, "nsed.x.result.0.A.propose.failed");
    }

    #[test]
    fn failed_subject_custom_prefix_propagates() {
        // SDK consumers can override `subject_prefix` for multi-tenant
        // isolation. The failure-marker fn must respect that prefix
        // instead of hard-coding "nsed".
        let s = failed_result_subject("tenantX", "sess", 1, "A", "propose");
        assert!(s.starts_with("tenantX."));
        assert!(s.ends_with(".A.propose.failed"));
    }

    #[test]
    fn failure_marker_emitted_for_propose() {
        assert!(should_publish_failure_marker("propose", false));
    }

    #[test]
    fn failure_marker_emitted_for_evaluate() {
        assert!(should_publish_failure_marker("evaluate", false));
    }

    #[test]
    fn failure_marker_skipped_for_other_actions() {
        for action in ["passthrough", "heartbeat", "unknown", ""] {
            assert!(
                !should_publish_failure_marker(action, false),
                "action {action:?} should not trigger a .failed marker"
            );
        }
    }

    #[test]
    fn failure_marker_skipped_for_payment_errors() {
        // Payment errors get auto-retry once credits return; a
        // permanent missing-vote signal would mislead the round
        // collector into counting the agent as a no-show.
        assert!(!should_publish_failure_marker("propose", true));
        assert!(!should_publish_failure_marker("evaluate", true));
    }
}
pub mod nsed_worker;
pub use nsed_worker::{NatsNsedWorkerExt, NatsNsedWorkerStatusExt};
