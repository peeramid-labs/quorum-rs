//! `LlmRequestSpan` — RAII helper that wraps an `AiModel::chat_completion`
//! call site with the four telemetry events the catalog defines for an
//! LLM request lifecycle.
//!
//! - `start(emitter, ctx, request_id, attempt, model, provider_id,
//!   estimated_input_tokens)` (sync) emits [`LlmRequestStart`],
//!   captures wall-clock start, and spawns the heartbeat task that
//!   fires [`LlmRequestStalled`] every [`STALLED_HEARTBEAT_INTERVAL`].
//! - `async complete(&result, cost_usd, messages_chars,
//!   max_tokens_requested)` cancels the heartbeat (awaiting its exit so
//!   no Stalled can race the terminal) and emits [`LlmRequestComplete`]
//!   with TTFT, generation timing, token usage, and `provider_backend`
//!   pulled from the result struct.
//! - `async fail(&LlmError)` cancels the heartbeat the same way and
//!   emits [`LlmRequestFailed`] with `error_class` + `http_status`
//!   derived via [`LlmError::classify`].
//! - On `Drop` without a terminal call (panic path), the heartbeat is
//!   aborted synchronously and a synthetic
//!   [`LlmRequestFailed`] with `error_class = other` is emitted so
//!   dashboards never see an unbalanced Start.
//!
//! Lives next to [`AiModel`](crate::llms::AiModel) because it
//! instruments the trait, but kept in its own file so `llms/mod.rs`
//! stays focused on the trait + result types and isn't read as a
//! kitchen-sink module. Telemetry consumers import via
//! `use quorum_rs::llms::LlmRequestSpan` (re-exported by mod.rs).

use crate::llms::ChatCompletionResult;
use crate::telemetry::{
    ContextEmergencyShrink, FinishReason as TelemFinishReason, LlmError, LlmRequestComplete,
    LlmRequestFailed, LlmRequestStalled, LlmRequestStart, RecentToolOutput, TelemetryContext,
    TelemetryEmitterMux, TelemetryEvent,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Heartbeat interval for `llm_request_stalled` emissions during
/// in-flight LLM requests. First emission is at `+30s`, not `+0s` —
/// a request that completes inside the first interval emits no
/// stalled events.
pub const STALLED_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// RAII span that emits LlmRequestStart on construction and one of
/// LlmRequestComplete / LlmRequestFailed via [`complete`] / [`fail`].
/// If the span is dropped without either being called (panic, early
/// return that doesn't go through the matched branches), the Drop
/// impl emits a synthetic `LlmRequestFailed` with
/// `error_class = other` so dashboards don't see a perpetually-pending
/// request.
///
/// `attempt`, `model`, and `estimated_input_tokens` are not stored:
/// they ship out in the [`LlmRequestStart`] event during [`start`]
/// and the terminal events don't reference them.
/// [`LlmRequestStalled`] only carries `request_id`, `elapsed_ms`,
/// `ttft_received`, and `last_token_ms` — fields specific to the
/// in-flight state, not duplicates of the start payload — so storing
/// the start fields on the span would be dead weight.
pub struct LlmRequestSpan {
    emitter: Option<TelemetryEmitterMux>,
    ctx: TelemetryContext,
    request_id: String,
    provider_id: String,
    start_ts: Instant,
    /// Set to `true` by [`complete`] / [`fail`] so the Drop impl
    /// knows whether a terminal event was already emitted. Without
    /// this flag a panic between `start` and the matched branches
    /// would leak a Start with no terminal event.
    terminated: bool,
    /// Cancellation handle for the heartbeat task. `notify_one`
    /// wakes the spawned loop's `select!` arm so it exits cleanly
    /// before any further interval tick can fire — guaranteeing no
    /// `llm_request_stalled` is emitted after a terminal event.
    stalled_cancel: Arc<Notify>,
    /// JoinHandle for the heartbeat task. Held so terminal paths can
    /// `await` task exit and confirm no in-flight emit is racing the
    /// terminal event. `None` when the request has no emitter (the
    /// task is never spawned in that case).
    stalled_handle: Option<JoinHandle<()>>,
}

impl LlmRequestSpan {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        emitter: Option<&TelemetryEmitterMux>,
        ctx: &TelemetryContext,
        request_id: &str,
        attempt: u32,
        model: &str,
        provider_id: &str,
        estimated_input_tokens: u32,
        context_utilization_pct: f64,
        recent_tool_output_bytes: u64,
    ) -> Self {
        Self::start_with_stalled_interval(
            emitter,
            ctx,
            request_id,
            attempt,
            model,
            provider_id,
            estimated_input_tokens,
            context_utilization_pct,
            recent_tool_output_bytes,
            STALLED_HEARTBEAT_INTERVAL,
        )
    }

    /// Same as [`start`] but with a configurable heartbeat cadence.
    /// Production code should use [`start`] (fixed 30s); this exists so
    /// tests can exercise the spawn/cancel contract on a sub-second
    /// timer without sleeping for minutes.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_stalled_interval(
        emitter: Option<&TelemetryEmitterMux>,
        ctx: &TelemetryContext,
        request_id: &str,
        attempt: u32,
        model: &str,
        provider_id: &str,
        estimated_input_tokens: u32,
        context_utilization_pct: f64,
        recent_tool_output_bytes: u64,
        stalled_interval: Duration,
    ) -> Self {
        if let Some(em) = emitter {
            em.emit(&TelemetryEvent::LlmRequestStart(LlmRequestStart {
                common: ctx.common(),
                request_id: request_id.to_string(),
                model: model.to_string(),
                provider_id: provider_id.to_string(),
                attempt,
                estimated_input_tokens,
                context_utilization_pct,
                recent_tool_output_bytes,
            }));
        }

        let start_ts = Instant::now();
        let stalled_cancel = Arc::new(Notify::new());

        // `tokio::time::interval_at` anchors the schedule once at
        // `start_ts + stalled_interval`. A re-arming
        // `sleep(stalled_interval)` loop would let emit-time work
        // (JSON serialize + spawn) shift later ticks: a request
        // living exactly 3×interval could emit only 2 events because
        // drift slides the third tick past the terminal cancel.
        // `interval_at` measures the period from the anchor, not
        // from the previous tick.
        //
        // The `biased select!` arm checks the cancellation `Notify`
        // before each ticker poll. Combined with the async
        // `cancel_heartbeat` await on the terminal path, no Stalled
        // emission can race the terminal event on the wire.
        let stalled_handle = if let Some(em) = emitter {
            let emitter_clone = em.clone();
            let ctx_clone = ctx.clone();
            let request_id_clone = request_id.to_string();
            let cancel = stalled_cancel.clone();
            Some(tokio::spawn(async move {
                // Anchor the ticker to `start_ts + stalled_interval`,
                // not "now + stalled_interval". The spawn task may
                // not run immediately after the spawn call (runtime
                // scheduling can defer the first poll by several ms),
                // and anchoring on the spawn's `Instant::now()` would
                // leak that scheduling delay into the cadence —
                // making "the third event for a request that lives
                // exactly 3×interval" miss its window. We compute
                // the offset against `start_ts.elapsed()` so the
                // ticker's first tick lands at `start_ts + interval`
                // regardless of how late the spawn body started.
                // `tokio::time::Instant` ticks identically to
                // `std::time::Instant` so the algebra is exact.
                let until_first_tick = stalled_interval.saturating_sub(start_ts.elapsed());
                let mut ticker = tokio::time::interval_at(
                    tokio::time::Instant::now() + until_first_tick,
                    stalled_interval,
                );
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.notified() => break,
                        _ = ticker.tick() => {
                            emitter_clone.emit(&TelemetryEvent::LlmRequestStalled(
                                LlmRequestStalled {
                                    common: ctx_clone.common(),
                                    request_id: request_id_clone.clone(),
                                    elapsed_ms: start_ts.elapsed().as_millis() as u64,
                                    // Default values — providers that
                                    // surface streaming partials can populate
                                    // these once the span is adapted to
                                    // streamed responses.
                                    ttft_received: false,
                                    last_token_ms: None,
                                },
                            ));
                        }
                    }
                }
            }))
        } else {
            None
        };

        Self {
            emitter: emitter.cloned(),
            ctx: ctx.clone(),
            request_id: request_id.to_string(),
            provider_id: provider_id.to_string(),
            start_ts,
            terminated: false,
            stalled_cancel,
            stalled_handle,
        }
    }

    /// Cancel the heartbeat task and **await** its exit. Used by the
    /// async terminal paths (`complete`, `fail`) so any in-flight
    /// `Stalled` emission has fully landed (including the inner
    /// fire-and-forget publish-spawn) before the caller emits the
    /// terminal event. Without the await, Tokio's cooperative
    /// cancellation could let an already-polled tick run its
    /// synchronous emit body to completion in parallel with the
    /// terminal — observable on NATS as a race between the Stalled
    /// publish and the terminal publish.
    async fn cancel_heartbeat(&mut self) {
        self.stalled_cancel.notify_one();
        if let Some(handle) = self.stalled_handle.take() {
            // The task exits the loop on the next `select!` poll
            // because the `cancel.notified()` arm is `biased` — so
            // even a tick that already fired before we notified
            // cannot enter another iteration. Awaiting drains any
            // emit-body that was already executing synchronously.
            let _ = handle.await;
        }
    }

    /// Synchronous cancellation used from `Drop`. Aborts the heartbeat
    /// task without awaiting — `Drop` cannot run async code. Tokio's
    /// `abort()` is cooperative (cancellation lands at the next
    /// `.await` boundary), so a tick already inside its synchronous
    /// emit body can complete one final emission. That residual is
    /// acceptable on the panic path because the panic itself already
    /// breaks the "exact ordering" contract — the terminal event isn't
    /// going to arrive in a recognisable order anyway. For clean
    /// terminations the async `cancel_heartbeat` from `complete` /
    /// `fail` runs first, so this Drop call is a no-op double-cancel.
    fn cancel_heartbeat_sync(&mut self) {
        self.stalled_cancel.notify_one();
        if let Some(handle) = self.stalled_handle.take() {
            handle.abort();
        }
    }

    /// Emit the terminal-success event.
    ///
    /// `messages_chars` and `max_tokens_requested` are request-side
    /// inputs the caller already has at dispatch time (the call site
    /// computes `messages_json.len()` for `estimated_input_tokens`
    /// anyway). `response_chars` and `tool_calls_emitted` are derived
    /// from `result.response` here so the caller doesn't have to.
    pub async fn complete(
        &mut self,
        result: &ChatCompletionResult,
        cost_usd: f64,
        messages_chars: u32,
        max_tokens_requested: Option<u32>,
    ) {
        // Cancel the heartbeat BEFORE emitting the terminal event AND
        // await its exit. Awaiting drains any synchronous emit body
        // that was already running on a tick we couldn't interrupt
        // — without this, Tokio's cooperative cancellation lets the
        // tick finish in parallel with the terminal emit, racing the
        // Stalled publish against the Complete publish on the wire.
        self.cancel_heartbeat().await;
        if let Some(ref emitter) = self.emitter {
            let response = &result.response;
            let usage = response.usage.as_ref();
            let input_tokens = usage.map(|u| u.prompt_tokens).unwrap_or(0);
            let output_tokens = usage.map(|u| u.completion_tokens).unwrap_or(0);
            let response_chars: u32 = response
                .choices
                .iter()
                .map(|c| c.message.content.as_deref().unwrap_or("").chars().count() as u32)
                .sum();
            let tool_calls_emitted: u32 = response
                .choices
                .iter()
                .map(|c| {
                    c.message
                        .tool_calls
                        .as_ref()
                        .map(|tc| tc.len())
                        .unwrap_or(0) as u32
                })
                .sum();
            let finish_reason = response
                .choices
                .first()
                .and_then(|c| c.finish_reason)
                .map(|fr| match fr {
                    async_openai::types::FinishReason::Stop => TelemFinishReason::Stop,
                    async_openai::types::FinishReason::Length => TelemFinishReason::Length,
                    async_openai::types::FinishReason::ToolCalls => TelemFinishReason::ToolCalls,
                    async_openai::types::FinishReason::ContentFilter => TelemFinishReason::Error,
                    // FunctionCall is the legacy / deprecated tool-calling
                    // shape predating the `tool_calls` array. For
                    // telemetry it's still a tool invocation, not a clean
                    // Stop, so dashboards counting tool usage stay honest
                    // when a provider falls back to the old field.
                    async_openai::types::FinishReason::FunctionCall => TelemFinishReason::ToolCalls,
                })
                .unwrap_or(TelemFinishReason::Stop);
            emitter.emit(&TelemetryEvent::LlmRequestComplete(LlmRequestComplete {
                common: self.ctx.common(),
                request_id: self.request_id.clone(),
                latency_ms: self.start_ts.elapsed().as_millis() as u64,
                ttft_ms: result.timing.ttft_ms,
                generation_ms: result.timing.generation_ms,
                input_tokens,
                output_tokens,
                reasoning_tokens: usage
                    .as_ref()
                    .and_then(|u| {
                        u.completion_tokens_details
                            .as_ref()
                            .and_then(|d| d.reasoning_tokens)
                    })
                    .unwrap_or(0),
                cached_tokens: usage
                    .as_ref()
                    .and_then(|u| {
                        u.prompt_tokens_details
                            .as_ref()
                            .and_then(|d| d.cached_tokens)
                    })
                    .unwrap_or(0),
                cost_usd,
                reported_cost_usd: result.provider_usage.cost_usd,
                cache_write_tokens: result.provider_usage.cache_write_tokens,
                finish_reason,
                provider_backend: result.provider_backend.clone(),
                claim_assessments_emitted: None,
                disagreements_emitted: None,
                messages_chars,
                max_tokens_requested,
                response_chars,
                tool_calls_emitted,
                max_tokens_shrunk_to_floor: result
                    .shrink_info
                    .as_ref()
                    .is_some_and(|s| s.floor_used),
                available_space_at_dispatch: result.shrink_info.as_ref().map(|s| s.available_space),
            }));
            // `recent_tool_outputs` stays empty: the per-task ledger
            // lives in the agent's task loop, not the LLM call.
            if let Some(ref shrink) = result.shrink_info
                && shrink.floor_used
            {
                emitter.emit(&TelemetryEvent::ContextEmergencyShrink(
                    ContextEmergencyShrink {
                        common: self.ctx.common(),
                        available_space: shrink.available_space,
                        requested_max: shrink.requested_max,
                        floor_used: shrink.floor,
                        estimated_input: shrink.estimated_input,
                        context_window: shrink.context_window,
                        recent_tool_outputs: Vec::<RecentToolOutput>::new(),
                    },
                ));
            }
        }
        self.terminated = true;
    }

    pub async fn fail(&mut self, error: &LlmError) {
        // Cancel heartbeat first AND await its exit. See `complete`
        // for the cooperative-cancellation rationale.
        self.cancel_heartbeat().await;
        if let Some(ref emitter) = self.emitter {
            let (error_class, http_status) = error.classify();
            emitter.emit(&TelemetryEvent::LlmRequestFailed(LlmRequestFailed {
                common: self.ctx.common(),
                request_id: self.request_id.clone(),
                error_class,
                http_status,
                retry_after_ms: match error {
                    LlmError::RateLimit { retry_after_ms, .. } => *retry_after_ms,
                    _ => None,
                },
                latency_ms: self.start_ts.elapsed().as_millis() as u64,
                provider_id: self.provider_id.clone(),
                provider_backend: None,
            }));
        }
        self.terminated = true;
    }
}

impl Drop for LlmRequestSpan {
    fn drop(&mut self) {
        // Always cancel the heartbeat — even when the span terminated
        // cleanly via complete()/fail() (which already cancelled),
        // calling again is a no-op. When the span is dropped without
        // a terminal call (panic path), this is the only chance to
        // stop the task.
        self.cancel_heartbeat_sync();
        if self.terminated {
            return;
        }
        let Some(ref emitter) = self.emitter else {
            return;
        };
        // Span dropped without complete/fail — typically a panic
        // mid-request. Emit a synthetic terminal event so dashboards
        // see one Failed per Start, never an unbalanced Start.
        emitter.emit(&TelemetryEvent::LlmRequestFailed(LlmRequestFailed {
            common: self.ctx.common(),
            request_id: self.request_id.clone(),
            error_class: crate::telemetry::LlmErrorClass::Other,
            http_status: None,
            retry_after_ms: None,
            latency_ms: self.start_ts.elapsed().as_millis() as u64,
            provider_id: self.provider_id.clone(),
            provider_backend: None,
        }));
    }
}
