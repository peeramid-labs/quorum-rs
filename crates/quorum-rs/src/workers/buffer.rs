//! Response buffer for HITL (Human-in-the-Loop) agent control.
//!
//! Sits between LLM completion and NATS publish, allowing operators to
//! inspect, hold, release, or reject agent responses before they reach
//! the orchestrator.

use crate::agents::OperatorAnnotation;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// BufferedResponse
// ---------------------------------------------------------------------------

/// Callback to ack the original NATS JetStream message.
///
/// Wrapping the ack operation in a trait object lets the buffer work with
/// both real NATS messages and test mocks without exposing the private
/// `async_nats::jetstream::context::AckContext` type.
#[async_trait::async_trait]
pub trait AckHandle: Send + Sync {
    /// Acknowledge the underlying message.
    async fn ack(&self) -> anyhow::Result<()>;
}

/// [`AckHandle`] backed by a real NATS JetStream message.
pub struct NatsAckHandle(pub async_nats::jetstream::Message);

#[async_trait::async_trait]
impl AckHandle for NatsAckHandle {
    async fn ack(&self) -> anyhow::Result<()> {
        self.0
            .ack()
            .await
            .map_err(|e| anyhow::anyhow!("ack failed: {}", e))
    }
}

/// No-op [`AckHandle`] for entries whose NATS message was already acked
/// at buffer-push time.
///
/// When the HITL buffer holds a response for operator review, the original
/// JetStream message must be acked **immediately** to prevent redelivery.
/// The response stays in the buffer (not published to the orchestrator) until
/// the operator approves or the hold timer expires.  This handle replaces
/// `NatsAckHandle` so that `drain_buffer()` doesn't attempt a redundant ack.
pub struct PreAckedHandle;

#[async_trait::async_trait]
impl AckHandle for PreAckedHandle {
    async fn ack(&self) -> anyhow::Result<()> {
        Ok(()) // Already acked at push time
    }
}

/// A completed agent response held in the buffer before NATS publication.
pub struct BufferedResponse {
    /// Unique ID for this buffer entry (UUID).
    pub id: String,
    /// Action type: `"propose"` or `"evaluate"`.
    pub action: String,
    /// Session / job ID.
    pub job_id: String,
    /// Deliberation round number.
    pub round: u32,
    /// NATS subject to publish to when released.
    pub reply_subject: String,
    /// Serialized payload bytes.
    pub payload: Vec<u8>,
    /// When the response was completed by the LLM.
    pub created_at: Instant,
    /// When the response should auto-release (created_at + hold_duration).
    pub release_at: Instant,
    /// Handle to ack the original NATS message when the entry is released.
    pub ack_handle: Box<dyn AckHandle>,
    /// Dedup key for idempotency.
    pub msg_id: String,
    /// Operator annotations accumulated during HITL review.
    pub annotations: Vec<OperatorAnnotation>,
    /// Whether the payload was edited by an operator.
    pub edited: bool,
    /// Whether the operator has stopped (reversibly rejected) this entry.
    ///
    /// Stopped entries remain in the buffer but are skipped by
    /// [`ResponseBuffer::drain_ready`] — they won't be published to NATS
    /// until un-stopped. This allows the operator to undo a rejection and
    /// continue editing.
    pub stopped: bool,
}

// ---------------------------------------------------------------------------
// BufferEntrySummary
// ---------------------------------------------------------------------------

/// Lightweight summary of a buffered entry for the dashboard UI.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BufferEntrySummary {
    /// Buffer entry ID.
    pub id: String,
    /// Action type: `"propose"` or `"evaluate"`.
    pub action: String,
    /// Full session/job ID (frontend truncates to last 4 chars).
    pub job_id: String,
    /// Deliberation round number.
    pub round: u32,
    /// Milliseconds since the response was buffered.
    pub age_ms: u64,
    /// Milliseconds until auto-release. Negative = overdue (held by pause).
    pub release_in_ms: i64,
    /// Whether the operator has stopped (reversibly rejected) this entry.
    pub stopped: bool,
}

/// Full detail of a buffered entry, including the deserialized payload content.
///
/// Used by the dashboard when the operator clicks a specific buffer entry
/// to inspect, edit, or annotate the response before release.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BufferEntryDetail {
    #[serde(flatten)]
    #[schema(inline)]
    pub summary: BufferEntrySummary,
    /// Deserialized response payload (the Proposal or Evaluation JSON).
    /// Can be an object (proposal) or array (evaluation pairs).
    pub content: serde_json::Value,
}

// ---------------------------------------------------------------------------
// ResponseBuffer
// ---------------------------------------------------------------------------

/// Thread-safe response buffer with pause/resume and manual release/reject.
///
/// Hold duration is dynamically adjustable via [`set_hold_duration`] to support
/// adaptive SLA — low-scoring agents can have their hold increased to give
/// operators more review time.
pub struct ResponseBuffer {
    pending: RwLock<VecDeque<BufferedResponse>>,
    /// Base hold duration as configured at startup (milliseconds).
    base_hold_duration_ms: u64,
    /// Current effective hold duration (milliseconds). Updated atomically
    /// by the adaptive SLA system.
    hold_duration_ms: AtomicU64,
    paused: AtomicBool,
    /// Response SLA in milliseconds. When set (> 0), [`push_with_deadline`]
    /// computes `release_at` relative to the task-receive time instead of
    /// using the fixed hold duration. This keeps responses in the buffer
    /// for the full SLA duration — card reaching the bottom = auto-release.
    response_sla_ms: AtomicU64,
    /// Whether auto-approve mode is enabled for this agent.
    ///
    /// When enabled, entries are auto-released immediately if the agent's
    /// effective divergence score is below [`auto_approve_threshold`].
    auto_approve: AtomicBool,
    /// Divergence threshold for auto-approve, stored as `value × 1000`
    /// (e.g. 1000 = 1.0 = 100%).
    ///
    /// The threshold only gates entries that are **already pending** with a
    /// future `release_at`: [`auto_release_if_eligible`](Self::auto_release_if_eligible)
    /// promotes them to "ready now" when the reported divergence is at or
    /// below this value. New entries pushed via
    /// [`push_with_deadline`](Self::push_with_deadline) while `auto_approve`
    /// is on **skip the SLA timer entirely** and never consult the
    /// threshold — they land with `release_at = now` regardless.
    ///
    /// Consequently the only way to actually gate new responses by
    /// divergence is the mid-flight toggle pattern: disable `auto_approve`
    /// so incoming entries accumulate under the SLA deadline, then later
    /// re-enable `auto_approve` with a lower threshold to drain only the
    /// low-divergence entries.
    ///
    /// The default of `1000` (100%) combined with `auto_approve: true`
    /// makes the out-of-the-box behavior a true pass-through that never
    /// holds responses. Operators who want divergence-gated review must
    /// disable `auto_approve`; lowering the threshold alone does not gate
    /// new arrivals.
    auto_approve_threshold_milli: AtomicU64,
}

impl ResponseBuffer {
    /// Create a new buffer with the given hold duration.
    ///
    /// The hold duration initializes the response SLA for deadline-based
    /// release via [`push_with_deadline`]. The dashboard can later override
    /// the SLA via [`set_response_sla`].
    ///
    /// `Duration::ZERO` means responses drain immediately (pass-through mode).
    /// Any non-zero duration is used as-is — callers control the review window.
    pub fn new(hold_duration: Duration) -> Self {
        let ms = hold_duration.as_millis() as u64;
        Self {
            pending: RwLock::new(VecDeque::new()),
            base_hold_duration_ms: ms,
            hold_duration_ms: AtomicU64::new(ms),
            paused: AtomicBool::new(false),
            response_sla_ms: AtomicU64::new(ms),
            auto_approve: AtomicBool::new(true),
            auto_approve_threshold_milli: AtomicU64::new(1000), // default: 1.0 (100%) — release everything
        }
    }

    /// The current effective hold duration.
    pub fn hold_duration(&self) -> Duration {
        Duration::from_millis(self.hold_duration_ms.load(Ordering::Relaxed))
    }

    /// The base hold duration as configured at startup.
    pub fn base_hold_duration(&self) -> Duration {
        Duration::from_millis(self.base_hold_duration_ms)
    }

    /// Dynamically update the effective hold duration.
    ///
    /// Used by the adaptive SLA system to slow down or speed up the buffer
    /// based on agent scores.
    pub fn set_hold_duration(&self, duration: Duration) {
        self.hold_duration_ms
            .store(duration.as_millis() as u64, Ordering::Relaxed);
    }

    /// Set the response SLA (used by [`push_with_deadline`] to compute release time).
    ///
    /// `Duration::ZERO` disables the deadline (pass-through).
    /// Any non-zero value is used as-is.
    pub fn set_response_sla(&self, sla: Duration) {
        self.response_sla_ms
            .store(sla.as_millis() as u64, Ordering::Relaxed);
    }

    /// Current response SLA duration, or `None` if not configured.
    pub fn response_sla(&self) -> Option<Duration> {
        let ms = self.response_sla_ms.load(Ordering::Relaxed);
        if ms == 0 {
            None
        } else {
            Some(Duration::from_millis(ms))
        }
    }

    /// Add a completed response to the buffer.
    pub async fn push(&self, entry: BufferedResponse) {
        self.pending.write().await.push_back(entry);
    }

    /// Add a completed response, computing `release_at` from the SLA deadline.
    ///
    /// `release_at = task_received + response_sla`
    ///
    /// The buffer holds the response for the full SLA duration. The rainfall
    /// animation on the dashboard runs for exactly this duration — card
    /// reaching the bottom = SLA expired = auto-release.
    ///
    /// The orchestrator already sets round timeouts that respect all agent
    /// SLAs, so no reserve is needed at the buffer level.
    ///
    /// Falls back to the caller-provided `release_at` if no SLA is configured.
    pub async fn push_with_deadline(&self, mut entry: BufferedResponse, task_received: Instant) {
        // When auto-approve is ON, bypass the SLA timer entirely —
        // release immediately on the next drain_ready() call.
        if self.auto_approve.load(Ordering::Relaxed) {
            entry.release_at = Instant::now();
            self.pending.write().await.push_back(entry);
            return;
        }
        let sla_ms = self.response_sla_ms.load(Ordering::Relaxed);
        if sla_ms > 0 {
            let sla = Duration::from_millis(sla_ms);
            let deadline = task_received + sla;
            let now = Instant::now();
            entry.release_at = if deadline > now { deadline } else { now };
        } else {
            entry.release_at = Instant::now();
        }
        self.pending.write().await.push_back(entry);
    }

    /// Drain entries whose `release_at` has passed, unless paused.
    ///
    /// Returns the drained entries (caller is responsible for publishing + ack).
    /// When paused, always returns an empty vec.
    pub async fn drain_ready(&self) -> Vec<BufferedResponse> {
        if self.paused.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let now = Instant::now();
        let mut pending = self.pending.write().await;
        let mut ready = Vec::new();
        let mut remaining = VecDeque::with_capacity(pending.len());
        for entry in pending.drain(..) {
            if now >= entry.release_at && !entry.stopped {
                ready.push(entry);
            } else {
                remaining.push_back(entry);
            }
        }
        *pending = remaining;
        ready
    }

    /// Return summaries of all buffered entries (for the dashboard UI).
    pub async fn list(&self) -> Vec<BufferEntrySummary> {
        let now = Instant::now();
        let pending = self.pending.read().await;
        pending
            .iter()
            .map(|entry| {
                let age = now.duration_since(entry.created_at);
                let release_in = if now >= entry.release_at {
                    -(now.duration_since(entry.release_at).as_millis() as i64)
                } else {
                    entry.release_at.duration_since(now).as_millis() as i64
                };
                BufferEntrySummary {
                    id: entry.id.clone(),
                    action: entry.action.clone(),
                    job_id: entry.job_id.clone(),
                    round: entry.round,
                    age_ms: age.as_millis() as u64,
                    release_in_ms: release_in,
                    stopped: entry.stopped,
                }
            })
            .collect()
    }

    /// Manually release a specific entry by ID, regardless of hold duration.
    ///
    /// Returns the entry if found, or `None` if not in the buffer.
    pub async fn release(&self, id: &str) -> Option<BufferedResponse> {
        let mut pending = self.pending.write().await;
        if let Some(pos) = pending.iter().position(|e| e.id == id) {
            pending.remove(pos)
        } else {
            None
        }
    }

    /// Reject (discard) a specific entry by ID.
    ///
    /// Returns the entry if found (caller should ack the message without
    /// publishing), or `None` if not in the buffer.
    pub async fn reject(&self, id: &str) -> Option<BufferedResponse> {
        // Same removal logic as release — the caller decides whether to publish.
        self.release(id).await
    }

    /// Remove all entries whose `job_id` does NOT match the given current job.
    ///
    /// Returns the removed entries (caller is responsible for ack-ing them).
    /// This prevents stale responses from previous deliberations from
    /// lingering in the operator review queue.
    pub async fn drain_stale(&self, current_job_id: &str) -> Vec<BufferedResponse> {
        let mut pending = self.pending.write().await;
        let mut stale = Vec::new();
        let mut remaining = VecDeque::with_capacity(pending.len());
        for entry in pending.drain(..) {
            if entry.job_id != current_job_id {
                stale.push(entry);
            } else {
                remaining.push_back(entry);
            }
        }
        *pending = remaining;
        stale
    }

    /// Pause the buffer: `drain_ready()` will return empty and the worker
    /// should also stop pulling new NATS tasks.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    /// Resume the buffer: `drain_ready()` resumes normal operation.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    /// Whether the buffer is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    // -- Auto-approve controls -----------------------------------------------

    /// Enable or disable auto-approve mode.
    ///
    /// When enabled, entries whose agent divergence is below the configured
    /// threshold are auto-released immediately instead of waiting for the
    /// hold timer or manual operator action.
    pub fn set_auto_approve(&self, enabled: bool) {
        self.auto_approve.store(enabled, Ordering::Relaxed);
    }

    /// Whether auto-approve mode is currently enabled.
    pub fn is_auto_approve(&self) -> bool {
        self.auto_approve.load(Ordering::Relaxed)
    }

    /// Set the divergence threshold for auto-approve (0.0 to 1.0).
    ///
    /// Values are clamped to `[0.0, 1.0]`. Stored internally as thousandths.
    pub fn set_auto_approve_threshold(&self, threshold: f32) {
        let clamped = threshold.clamp(0.0, 1.0);
        self.auto_approve_threshold_milli
            .store((clamped * 1000.0) as u64, Ordering::Relaxed);
    }

    /// Current auto-approve divergence threshold (0.0 to 1.0).
    pub fn auto_approve_threshold(&self) -> f32 {
        self.auto_approve_threshold_milli.load(Ordering::Relaxed) as f32 / 1000.0
    }

    /// When auto-approve is enabled and the agent's divergence is at or
    /// below the threshold, mark all non-stopped pending entries for
    /// immediate release. The gate is strictly `div > threshold` → block,
    /// so a threshold of `1.0` (the default) releases every entry because
    /// `compute_divergence` clamps to `[0.0, 1.0]`.
    ///
    /// When divergence is `None` (no scores yet), the operator's explicit
    /// opt-in to auto-approve takes precedence — entries are released.
    /// The threshold only blocks release when we **have** divergence data
    /// strictly exceeding the threshold.
    ///
    /// Returns the number of entries marked for auto-release.
    pub async fn auto_release_if_eligible(&self, divergence: Option<f32>) -> usize {
        if !self.auto_approve.load(Ordering::Relaxed) {
            return 0;
        }
        // When we have divergence data, check against threshold.
        // When we don't (no scores yet), trust the operator's explicit click.
        if let Some(div) = divergence {
            if div > self.auto_approve_threshold() {
                return 0; // Divergence too high → require manual review
            }
        }

        let now = Instant::now();
        let mut pending = self.pending.write().await;
        let mut count = 0;
        for entry in pending.iter_mut() {
            if !entry.stopped && entry.release_at > now {
                entry.release_at = now;
                count += 1;
            }
        }
        count
    }

    /// Number of entries currently in the buffer.
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Whether the buffer is empty.
    pub async fn is_empty(&self) -> bool {
        self.pending.read().await.is_empty()
    }

    /// Return the full detail of a specific buffer entry, including
    /// the deserialized response payload (for operator inspection/editing).
    pub async fn get_detail(&self, id: &str) -> Option<BufferEntryDetail> {
        let now = Instant::now();
        let pending = self.pending.read().await;
        pending.iter().find(|e| e.id == id).map(|entry| {
            let age = now.duration_since(entry.created_at);
            let release_in = if now >= entry.release_at {
                -(now.duration_since(entry.release_at).as_millis() as i64)
            } else {
                entry.release_at.duration_since(now).as_millis() as i64
            };
            let content = serde_json::from_slice(&entry.payload).unwrap_or(serde_json::Value::Null);
            BufferEntryDetail {
                summary: BufferEntrySummary {
                    id: entry.id.clone(),
                    action: entry.action.clone(),
                    job_id: entry.job_id.clone(),
                    round: entry.round,
                    age_ms: age.as_millis() as u64,
                    release_in_ms: release_in,
                    stopped: entry.stopped,
                },
                content,
            }
        })
    }

    /// Update the payload of a specific buffer entry (operator edit).
    ///
    /// Returns `true` if the entry was found and updated, `false` otherwise.
    pub async fn update_payload(&self, id: &str, new_payload: Vec<u8>) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.payload = new_payload;
            true
        } else {
            false
        }
    }

    /// Add an operator comment to a buffer entry without modifying the payload.
    ///
    /// Returns `true` if the entry was found and annotated, `false` otherwise.
    pub async fn add_comment(&self, id: &str, annotation: OperatorAnnotation) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.annotations.push(annotation);
            true
        } else {
            false
        }
    }

    /// Mark a buffer entry for immediate release by setting its `release_at`
    /// to now.
    ///
    /// The entry stays in the buffer — the worker's `drain_buffer()` loop will
    /// pick it up on the next cycle (≤500ms) and handle the NATS publish.
    /// This avoids needing a NATS client in the status server.
    ///
    /// **Note:** The stopped flag is preserved. Stopped entries must be explicitly
    /// unstopped (or use [`force_release`]) before they can drain.
    ///
    /// Returns `true` if the entry was found and marked, `false` otherwise.
    pub async fn mark_for_release(&self, id: &str) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.release_at = Instant::now();
            true
        } else {
            false
        }
    }

    /// Atomically unstop **and** mark a buffer entry for immediate release.
    ///
    /// Combines [`unstop`] + [`mark_for_release`] in a single lock acquisition,
    /// eliminating the race window where `drain_ready()` could observe the entry
    /// as unstopped with a stale (already-passed) `release_at`.
    ///
    /// Returns `true` if the entry was found, `false` otherwise.
    pub async fn force_release(&self, id: &str) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.stopped = false;
            entry.release_at = Instant::now();
            true
        } else {
            false
        }
    }

    /// Stop (reversibly reject) a buffer entry.
    ///
    /// Stopped entries remain in the buffer but are skipped by
    /// [`drain_ready`] — they won't auto-release. The operator can later
    /// call [`unstop`] to make the entry eligible for release again.
    ///
    /// Returns `true` if the entry was found and stopped, `false` otherwise.
    pub async fn stop(&self, id: &str) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.stopped = true;
            true
        } else {
            false
        }
    }

    /// Un-stop a previously stopped buffer entry.
    ///
    /// The entry becomes eligible for [`drain_ready`] again. If its
    /// `release_at` has already passed, it will drain on the next cycle.
    ///
    /// Returns `true` if the entry was found and un-stopped, `false` otherwise.
    pub async fn unstop(&self, id: &str) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.stopped = false;
            true
        } else {
            false
        }
    }

    /// Update the payload of a buffer entry AND record an edit annotation.
    ///
    /// Marks the entry as `edited = true` and appends the annotation.
    /// Returns `true` if the entry was found and updated, `false` otherwise.
    pub async fn update_payload_with_annotation(
        &self,
        id: &str,
        new_payload: Vec<u8>,
        annotation: OperatorAnnotation,
    ) -> bool {
        let mut pending = self.pending.write().await;
        if let Some(entry) = pending.iter_mut().find(|e| e.id == id) {
            entry.payload = new_payload;
            entry.edited = true;
            entry.annotations.push(annotation);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Adaptive SLA computation
// ---------------------------------------------------------------------------

/// Compute an adaptive hold duration based on an agent's mean score.
///
/// Low-scoring agents get longer hold durations, giving operators more time
/// to review and potentially intervene before release. Well-converged agents
/// flow at the base speed.
///
/// # Formula
///
/// ```text
/// soft_norm  = score / (1 + |score|)       ∈ (-1, +1)
/// positive   = (soft_norm + 1) / 2         ∈ (0, 1)
/// multiplier = 1 + (1 - positive) × amplification
/// result     = base × multiplier
/// ```
///
/// `mean_score` is the rolling mean of `aggregated_score` values — signed QV
/// sums that can exceed [-1, +1]. The soft-normalization compresses any
/// magnitude into (-1, +1) before mapping to the hold multiplier.
///
/// With `amplification = 3.0`:
/// - Score +3.0 → positive ≈ 0.875 → 1.375× base
/// - Score +1.0 → positive = 0.75  → 1.75× base
/// - Score  0.0 → positive = 0.50  → 2.50× base
/// - Score -1.0 → positive = 0.25  → 3.25× base
/// - Score -3.0 → positive ≈ 0.125 → 3.625× base
///
/// Returns `base` unchanged if `mean_score` is `None` (no scores yet).
pub fn compute_adaptive_hold(
    base: Duration,
    mean_score: Option<f32>,
    amplification: f32,
) -> Duration {
    let Some(score) = mean_score else {
        return base;
    };
    let positive = soft_normalize_positive(score);
    let multiplier = 1.0 + (1.0 - positive) * amplification;
    Duration::from_secs_f64(base.as_secs_f64() * multiplier as f64)
}

/// Compute the effective divergence score for an agent (0.0 = converged,
/// 1.0 = fully divergent).
///
/// `aggregated_score` is a signed QV sum (`Σ score_q_s`) that can be any real
/// number — positive means endorsed, negative means rejected, magnitude grows
/// with evaluator count. We soft-normalize to (-1, +1) then map to [0, 1]
/// divergence.
///
/// - Score divergence: `(1 − soft_norm(mean_score)) / 2`
///   where `soft_norm(s) = s / (1 + |s|)` maps ℝ → (-1, +1).
///   Score +∞ → divergence 0, score −∞ → divergence 1, score 0 → divergence 0.5.
/// - Std-dev divergence: `clamp(0, 1, score_std_dev / 1.0)`
///   (signed QV scores per evaluator ∈ [-1, +1]; std_dev ≥ 1.0 = maximal disagreement)
/// - Effective: `max(score_divergence, std_dev_divergence)`
///
/// Returns `None` if no scores are available (divergence unknown).
/// Soft-normalize a signed score from ℝ → (0, 1).
///
/// Maps `s / (1 + |s|)` from ℝ → (-1, +1), then shifts to (0, 1):
/// - score → +∞  ⟹ 1.0
/// - score = 0   ⟹ 0.5
/// - score → -∞  ⟹ 0.0
fn soft_normalize_positive(score: f32) -> f32 {
    let soft = score / (1.0 + score.abs());
    ((soft + 1.0) / 2.0).clamp(0.0, 1.0)
}

pub fn compute_divergence(mean_score: Option<f32>, score_std_dev: Option<f32>) -> Option<f32> {
    let score_div = mean_score.map(|s| 1.0 - soft_normalize_positive(s));
    let std_div = score_std_dev.map(|sd| sd.clamp(0.0, 1.0));
    match (score_div, std_div) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op ack handle for testing — buffer tests never actually ack.
    struct NoopAckHandle;

    #[async_trait::async_trait]
    impl AckHandle for NoopAckHandle {
        async fn ack(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Create a minimal BufferedResponse for testing.
    fn make_entry(id: &str, action: &str, job_id: &str, hold: Duration) -> BufferedResponse {
        let now = Instant::now();
        BufferedResponse {
            id: id.to_string(),
            action: action.to_string(),
            job_id: job_id.to_string(),
            round: 1,
            reply_subject: format!("nsed.{}.result.1.agent.{}", job_id, action),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + hold,
            ack_handle: Box::new(NoopAckHandle),
            msg_id: format!("msg-{}", id),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        }
    }

    #[tokio::test]
    async fn test_buffer_push_and_len() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert_eq!(buf.len().await, 0);
        assert!(buf.is_empty().await);

        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(10)))
            .await;
        buf.push(make_entry(
            "b",
            "evaluate",
            "job-2",
            Duration::from_secs(10),
        ))
        .await;
        assert_eq!(buf.len().await, 2);
        assert!(!buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_buffer_drain_respects_hold_duration() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // Not enough time has passed — nothing should drain
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty());
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_buffer_drain_releases_ready() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        // hold=0 means release_at == created_at (immediately ready)
        buf.push(make_entry("a", "propose", "job-1", Duration::ZERO))
            .await;
        buf.push(make_entry("b", "evaluate", "job-2", Duration::ZERO))
            .await;

        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 2);
        assert!(buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_buffer_pause_stops_drain() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("a", "propose", "job-1", Duration::ZERO))
            .await;

        buf.pause();
        assert!(buf.is_paused());

        let drained = buf.drain_ready().await;
        assert!(drained.is_empty(), "paused buffer should not drain");
        assert_eq!(buf.len().await, 1, "entry should still be in buffer");
    }

    #[tokio::test]
    async fn test_buffer_resume_releases_overdue() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("a", "propose", "job-1", Duration::ZERO))
            .await;

        buf.pause();
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty());

        buf.resume();
        assert!(!buf.is_paused());
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
    }

    #[tokio::test]
    async fn test_buffer_release_by_id() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;
        buf.push(make_entry(
            "b",
            "evaluate",
            "job-2",
            Duration::from_secs(60),
        ))
        .await;

        let released = buf.release("a").await;
        assert!(released.is_some());
        assert_eq!(released.unwrap().id, "a");
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_buffer_reject_by_id() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        let rejected = buf.reject("a").await;
        assert!(rejected.is_some());
        assert_eq!(rejected.unwrap().id, "a");
        assert!(buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_buffer_release_unknown_id() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        let released = buf.release("nonexistent").await;
        assert!(released.is_none());
        assert_eq!(buf.len().await, 1, "existing entry should remain");
    }

    #[tokio::test]
    async fn test_buffer_list_returns_summaries() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry(
            "entry-1",
            "propose",
            "job-abcd1234",
            Duration::from_secs(30),
        ))
        .await;

        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "entry-1");
        assert_eq!(list[0].action, "propose");
        assert_eq!(list[0].job_id, "job-abcd1234");
        assert_eq!(list[0].round, 1);
        assert!(list[0].release_in_ms > 0, "should still be holding");
    }

    #[tokio::test]
    async fn test_buffer_zero_hold_drains_immediately() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        for i in 0..5 {
            buf.push(make_entry(
                &format!("e{}", i),
                "propose",
                &format!("job-{}", i),
                Duration::ZERO,
            ))
            .await;
        }
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 5);
        assert!(buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_get_detail_returns_content() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        let payload = serde_json::json!({"title": "My proposal", "content": "Hello world"});
        let now = Instant::now();
        buf.push(BufferedResponse {
            id: "detail-1".to_string(),
            action: "propose".to_string(),
            job_id: "job-xyz".to_string(),
            round: 3,
            reply_subject: "nsed.job-xyz.result.3.agent.propose".to_string(),
            payload: serde_json::to_vec(&payload).unwrap(),
            created_at: now,
            release_at: now + Duration::from_secs(30),
            ack_handle: Box::new(NoopAckHandle),
            msg_id: "msg-detail-1".to_string(),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        })
        .await;

        let detail = buf.get_detail("detail-1").await;
        assert!(detail.is_some());
        let detail = detail.unwrap();
        assert_eq!(detail.summary.id, "detail-1");
        assert_eq!(detail.summary.action, "propose");
        assert_eq!(detail.summary.job_id, "job-xyz");
        assert_eq!(detail.summary.round, 3);
        assert!(detail.summary.release_in_ms > 0);
        assert_eq!(detail.content["title"], "My proposal");
        assert_eq!(detail.content["content"], "Hello world");
    }

    #[tokio::test]
    async fn test_get_detail_returns_none_for_unknown_id() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(30)))
            .await;
        assert!(buf.get_detail("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_get_detail_invalid_payload_returns_null_content() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        let now = Instant::now();
        buf.push(BufferedResponse {
            id: "bad-json".to_string(),
            action: "propose".to_string(),
            job_id: "job-1".to_string(),
            round: 1,
            reply_subject: "nsed.job-1.result.1.agent.propose".to_string(),
            payload: b"not valid json!".to_vec(),
            created_at: now,
            release_at: now + Duration::from_secs(30),
            ack_handle: Box::new(NoopAckHandle),
            msg_id: "msg-bad".to_string(),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        })
        .await;

        let detail = buf.get_detail("bad-json").await.unwrap();
        assert_eq!(detail.content, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn test_update_payload_replaces_content() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry(
            "upd-1",
            "evaluate",
            "job-1",
            Duration::from_secs(30),
        ))
        .await;

        let new_payload = serde_json::json!({"scores": [8, 9, 7]});
        let updated = buf
            .update_payload("upd-1", serde_json::to_vec(&new_payload).unwrap())
            .await;
        assert!(updated);

        // Verify via get_detail
        let detail = buf.get_detail("upd-1").await.unwrap();
        assert_eq!(detail.content["scores"], serde_json::json!([8, 9, 7]));
    }

    #[tokio::test]
    async fn test_update_payload_unknown_id_returns_false() {
        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(30)))
            .await;

        let result = buf.update_payload("nonexistent", b"{}".to_vec()).await;
        assert!(!result);
        // Original entry unaffected
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_add_comment_records_annotation() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry(
            "ann-1",
            "propose",
            "job-1",
            Duration::from_secs(30),
        ))
        .await;

        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "Looks good".to_string(),
            timestamp: "2026-03-02T12:00:00Z".to_string(),
            original_content_hash: None,
            signatures: Vec::new(),
        };

        assert!(buf.add_comment("ann-1", annotation).await);

        // Verify via drain_ready won't drain (still held), but we can
        // check the entry is still there
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_add_comment_unknown_id_returns_false() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::from_secs(30));
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(30)))
            .await;

        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Comment,
            comment: "test".to_string(),
            timestamp: "2026-03-02T12:00:00Z".to_string(),
            original_content_hash: None,
            signatures: Vec::new(),
        };

        assert!(!buf.add_comment("nonexistent", annotation).await);
    }

    #[tokio::test]
    async fn test_update_payload_with_annotation_marks_edited() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("edit-1", "propose", "job-1", Duration::ZERO))
            .await;

        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Fixed wording".to_string(),
            timestamp: "2026-03-02T12:00:00Z".to_string(),
            original_content_hash: Some("abc123".to_string()),
            signatures: Vec::new(),
        };

        let new_payload = serde_json::json!({"content": "edited"});
        assert!(
            buf.update_payload_with_annotation(
                "edit-1",
                serde_json::to_vec(&new_payload).unwrap(),
                annotation
            )
            .await
        );

        // Drain the entry and verify it has annotations and edited flag
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        let entry = &drained[0];
        assert!(entry.edited);
        assert_eq!(entry.annotations.len(), 1);
        assert_eq!(entry.annotations[0].annotation_type, AnnotationType::Edit);
        assert_eq!(entry.annotations[0].comment, "Fixed wording");
    }

    #[tokio::test]
    async fn test_buffer_concurrent_push_drain() {
        use std::sync::Arc;

        let buf = Arc::new(ResponseBuffer::new(Duration::ZERO));
        let mut handles = Vec::new();

        // Spawn 10 pushers
        for i in 0..10 {
            let buf = buf.clone();
            handles.push(tokio::spawn(async move {
                buf.push(make_entry(
                    &format!("c{}", i),
                    "propose",
                    &format!("job-{}", i),
                    Duration::ZERO,
                ))
                .await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // Drain all
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 10);
        assert!(buf.is_empty().await);
    }

    // -------------------------------------------------------------------
    // Adaptive SLA tests
    // -------------------------------------------------------------------

    #[test]
    fn test_compute_adaptive_hold_high_score() {
        let base = Duration::from_secs(10);
        // score = 0.8 → soft = 0.8/1.8 ≈ 0.444 → positive ≈ 0.722
        // multiplier = 1 + 0.278 * 3 = 1.833
        let hold = super::compute_adaptive_hold(base, Some(0.8), 3.0);
        let expected_secs = 18.33;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "hold={:?}",
            hold
        );
    }

    #[test]
    fn test_compute_adaptive_hold_low_score() {
        let base = Duration::from_secs(10);
        // score = -0.8 → soft = -0.8/1.8 ≈ -0.444 → positive ≈ 0.278
        // multiplier = 1 + 0.722 * 3 = 3.167
        let hold = super::compute_adaptive_hold(base, Some(-0.8), 3.0);
        let expected_secs = 31.67;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "hold={:?}",
            hold
        );
    }

    #[test]
    fn test_compute_adaptive_hold_no_score() {
        let base = Duration::from_secs(10);
        let hold = super::compute_adaptive_hold(base, None, 3.0);
        assert_eq!(hold, base);
    }

    #[test]
    fn test_compute_adaptive_hold_perfect_score() {
        let base = Duration::from_secs(10);
        // score = 1.0 → soft = 0.5 → positive = 0.75
        // multiplier = 1 + 0.25 * 3 = 1.75
        let hold = super::compute_adaptive_hold(base, Some(1.0), 3.0);
        let expected_secs = 17.5;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "hold={:?}",
            hold
        );
    }

    #[test]
    fn test_compute_adaptive_hold_zero_score() {
        let base = Duration::from_secs(10);
        // score = 0.0 → soft = 0 → positive = 0.5
        // multiplier = 1 + 0.5 * 3 = 2.5
        let hold = super::compute_adaptive_hold(base, Some(0.0), 3.0);
        let expected_secs = 25.0;
        assert!(
            (hold.as_secs_f64() - expected_secs).abs() < 0.5,
            "hold={:?}",
            hold
        );
    }

    #[test]
    fn test_set_hold_duration_atomic() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert_eq!(buf.hold_duration(), Duration::from_secs(10));
        assert_eq!(buf.base_hold_duration(), Duration::from_secs(10));

        buf.set_hold_duration(Duration::from_secs(25));
        assert_eq!(buf.hold_duration(), Duration::from_secs(25));
        // Base should remain unchanged
        assert_eq!(buf.base_hold_duration(), Duration::from_secs(10));
    }

    // -------------------------------------------------------------------
    // SLA-based release tests
    // -------------------------------------------------------------------

    #[test]
    fn test_response_sla_default_matches_hold_duration() {
        // SLA initializes to the hold_duration value — no floor applied.
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert_eq!(buf.response_sla(), Some(Duration::from_secs(10)));

        let buf_long = ResponseBuffer::new(Duration::from_secs(600));
        assert_eq!(buf_long.response_sla(), Some(Duration::from_secs(600)));

        // Sub-second durations work too
        let buf_fast = ResponseBuffer::new(Duration::from_millis(500));
        assert_eq!(buf_fast.response_sla(), Some(Duration::from_millis(500)));
    }

    #[test]
    fn test_response_sla_zero_hold_is_none() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        assert!(buf.response_sla().is_none());
    }

    #[test]
    fn test_set_response_sla() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        buf.set_response_sla(Duration::from_secs(600));
        assert_eq!(buf.response_sla(), Some(Duration::from_secs(600)));
    }

    #[test]
    fn test_set_response_sla_uses_exact_value() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        // Any non-zero value is accepted as-is — no floor.
        buf.set_response_sla(Duration::from_secs(30));
        assert_eq!(buf.response_sla(), Some(Duration::from_secs(30)));

        buf.set_response_sla(Duration::from_millis(1));
        assert_eq!(buf.response_sla(), Some(Duration::from_millis(1)));

        // Setting to 0 means passthrough
        buf.set_response_sla(Duration::ZERO);
        assert_eq!(buf.response_sla(), None, "zero means passthrough");
    }

    #[tokio::test]
    async fn test_push_with_deadline_sla() {
        // set_response_sla(60s) — used as-is, no floor.
        // release_at = task_received + 60s.
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        buf.set_auto_approve(false);
        buf.set_response_sla(Duration::from_secs(60));

        let task_received = Instant::now();
        let entry = make_entry("sla-1", "propose", "job-1", Duration::from_secs(10));
        buf.push_with_deadline(entry, task_received).await;

        // Entry should not drain immediately (~60s hold)
        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "should hold for full SLA (~60s), not drain immediately"
        );
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_push_with_deadline_no_sla_fallback() {
        // SLA = 0 (pass-through mode) — should use caller's release_at
        let buf = ResponseBuffer::new(Duration::ZERO);
        // response_sla_ms is also 0 because hold_duration is ZERO

        let task_received = Instant::now();
        let entry = make_entry("nosla-1", "propose", "job-1", Duration::ZERO);
        buf.push_with_deadline(entry, task_received).await;

        // Should drain immediately since no SLA and caller set release_at = now + 0
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1, "should drain immediately when no SLA set");
    }

    #[tokio::test]
    async fn test_push_with_deadline_past_deadline_clamps() {
        // SLA = 600s (above floor), task received 620s ago — past deadline.
        // No reserve — deadline = received + 600s.
        // 620s ago means we're 20s past deadline → should drain immediately.
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_response_sla(Duration::from_secs(600));

        let task_received = Instant::now() - Duration::from_secs(620);
        let entry = make_entry("late-1", "evaluate", "job-1", Duration::from_secs(60));
        buf.push_with_deadline(entry, task_received).await;

        // Should drain immediately since we're past the SLA deadline
        let drained = buf.drain_ready().await;
        assert_eq!(
            drained.len(),
            1,
            "past-deadline entry should drain immediately"
        );
    }

    /// Regression test: a buffer created with short hold_duration (e.g. 10s from
    /// agent config) must NOT auto-release entries before the operator has had
    /// time to review.  The minimum operator review window is 5 minutes (300s).
    ///
    /// Previously, `response_sla_ms` was initialized from `hold_duration_ms`,
    /// so a 10s hold → 10s SLA → release_at = task_received + 7s.  By the time
    /// the control plane called `set_response_sla(300s)` the first task was
    /// already in the buffer with a 7s deadline.
    #[tokio::test]
    async fn test_short_hold_duration_uses_exact_sla() {
        // Agent configured with 10s hold_duration — SLA is 10s, no floor.
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        buf.set_auto_approve(false);

        let task_received = Instant::now();
        let entry = make_entry("p1", "propose", "job-1", Duration::from_secs(10));
        buf.push_with_deadline(entry, task_received).await;

        // Entry should be held for ~10s (the configured SLA).
        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        assert!(
            list[0].release_in_ms > 0 && list[0].release_in_ms <= 10_000,
            "release_in_ms should be in (0, 10_000], got {}",
            list[0].release_in_ms
        );
    }

    #[tokio::test]
    async fn test_response_sla_matches_hold_on_construction() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        let sla = buf.response_sla();
        assert_eq!(sla, Some(Duration::from_secs(10)));
    }

    /// Duration::ZERO is pass-through mode — no holding at all.
    /// The SLA floor should NOT apply in pass-through mode.
    #[tokio::test]
    async fn test_zero_hold_is_passthrough_no_sla_floor() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        // Zero hold means "no buffering" — SLA should remain 0
        let sla = buf.response_sla();
        assert_eq!(sla, None, "pass-through mode should have no SLA");
    }

    #[tokio::test]
    async fn test_drain_stale_removes_entries_from_other_jobs() {
        let buf = ResponseBuffer::new(Duration::from_secs(300));
        buf.push(make_entry(
            "a",
            "propose",
            "old-job",
            Duration::from_secs(300),
        ))
        .await;
        buf.push(make_entry(
            "b",
            "evaluate",
            "old-job",
            Duration::from_secs(300),
        ))
        .await;
        buf.push(make_entry(
            "c",
            "propose",
            "current-job",
            Duration::from_secs(300),
        ))
        .await;
        assert_eq!(buf.len().await, 3);

        let stale = buf.drain_stale("current-job").await;
        assert_eq!(stale.len(), 2, "should drain 2 old-job entries");
        assert_eq!(buf.len().await, 1, "should keep 1 current-job entry");

        // Verify the remaining entry is the current job
        let list = buf.list().await;
        assert_eq!(list[0].id, "c");
        assert_eq!(list[0].job_id, "current-job");
    }

    #[tokio::test]
    async fn test_drain_stale_no_op_when_all_current() {
        let buf = ResponseBuffer::new(Duration::from_secs(300));
        buf.push(make_entry(
            "a",
            "propose",
            "job-1",
            Duration::from_secs(300),
        ))
        .await;
        buf.push(make_entry(
            "b",
            "evaluate",
            "job-1",
            Duration::from_secs(300),
        ))
        .await;

        let stale = buf.drain_stale("job-1").await;
        assert!(stale.is_empty());
        assert_eq!(buf.len().await, 2);
    }

    #[tokio::test]
    async fn test_drain_stale_empty_buffer() {
        let buf = ResponseBuffer::new(Duration::from_secs(300));
        let stale = buf.drain_stale("any-job").await;
        assert!(stale.is_empty());
    }

    /// Verify PreAckedHandle is a no-op — ack always succeeds.
    ///
    /// This is the architectural invariant: when HITL buffers a response,
    /// the NATS message is acked IMMEDIATELY at push time to prevent
    /// JetStream redelivery.  The PreAckedHandle replaces NatsAckHandle
    /// so drain_buffer()'s ack() call is harmless.
    #[tokio::test]
    async fn test_pre_acked_handle_is_noop() {
        let handle = PreAckedHandle;
        assert!(
            handle.ack().await.is_ok(),
            "PreAckedHandle.ack() should always succeed"
        );
        // Can call multiple times safely (idempotent)
        assert!(handle.ack().await.is_ok());
    }

    /// Verify that a buffer entry with PreAckedHandle drains and releases
    /// correctly — the full HITL flow.
    #[tokio::test]
    async fn test_buffer_entry_with_pre_acked_handle_drains_correctly() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        let now = Instant::now();
        buf.push(BufferedResponse {
            id: "pre-acked-1".to_string(),
            action: "propose".to_string(),
            job_id: "job-1".to_string(),
            round: 1,
            reply_subject: "nsed.job-1.result.1.agent.propose".to_string(),
            payload: b"{\"content\":\"test\"}".to_vec(),
            created_at: now,
            release_at: now,                      // immediate drain for test
            ack_handle: Box::new(PreAckedHandle), // ← pre-acked
            msg_id: "msg-pre-acked-1".to_string(),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        })
        .await;

        assert_eq!(buf.len().await, 1);

        // Drain should succeed and entry should have PreAckedHandle
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        // Calling ack on the drained entry should be a no-op (not error)
        assert!(drained[0].ack_handle.ack().await.is_ok());
        assert!(buf.is_empty().await);
    }

    // -----------------------------------------------------------------------
    // mark_for_release tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mark_for_release_sets_release_at_to_now() {
        // Buffer entry with a far-future release_at should NOT drain normally
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        let now = Instant::now();
        let far_future = now + Duration::from_secs(3600);
        buf.push(BufferedResponse {
            id: "mark-1".to_string(),
            action: "propose".to_string(),
            job_id: "job-1".to_string(),
            round: 1,
            reply_subject: "nsed.job-1.result.1.agent.propose".to_string(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: far_future,
            ack_handle: Box::new(NoopAckHandle),
            msg_id: "msg-mark-1".to_string(),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        })
        .await;

        // Before mark: drain_ready should return nothing (entry is far future)
        assert!(buf.drain_ready().await.is_empty());
        assert_eq!(buf.len().await, 1);

        // Mark for release
        let found = buf.mark_for_release("mark-1").await;
        assert!(found, "mark_for_release should find the entry");

        // After mark: drain_ready should pick it up immediately
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "mark-1");
        assert!(buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_mark_for_release_unknown_id_returns_false() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        let found = buf.mark_for_release("nonexistent").await;
        assert!(
            !found,
            "mark_for_release should return false for unknown ID"
        );
    }

    #[tokio::test]
    async fn test_mark_for_release_only_affects_target_entry() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        let now = Instant::now();
        let far_future = now + Duration::from_secs(3600);

        // Push two entries with far-future release times
        for i in 0..2 {
            buf.push(BufferedResponse {
                id: format!("entry-{}", i),
                action: "propose".to_string(),
                job_id: "job-1".to_string(),
                round: 1,
                reply_subject: format!("nsed.job-1.result.1.agent{}.propose", i),
                payload: b"{}".to_vec(),
                created_at: now,
                release_at: far_future,
                ack_handle: Box::new(NoopAckHandle),
                msg_id: format!("msg-{}", i),
                annotations: Vec::new(),
                edited: false,
                stopped: false,
            })
            .await;
        }
        assert_eq!(buf.len().await, 2);

        // Mark only entry-0 for release
        buf.mark_for_release("entry-0").await;

        // Only entry-0 should drain; entry-1 stays
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "entry-0");
        assert_eq!(buf.len().await, 1); // entry-1 remains
    }

    #[tokio::test]
    async fn test_mark_for_release_while_paused_still_marks() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        let now = Instant::now();
        buf.push(BufferedResponse {
            id: "paused-mark-1".to_string(),
            action: "evaluate".to_string(),
            job_id: "job-2".to_string(),
            round: 1,
            reply_subject: "nsed.job-2.result.1.agent.evaluate".to_string(),
            payload: b"{}".to_vec(),
            created_at: now,
            release_at: now + Duration::from_secs(3600),
            ack_handle: Box::new(NoopAckHandle),
            msg_id: "msg-paused-1".to_string(),
            annotations: Vec::new(),
            edited: false,
            stopped: false,
        })
        .await;

        buf.pause();

        // Mark succeeds even when paused
        assert!(buf.mark_for_release("paused-mark-1").await);

        // But drain_ready respects pause — returns nothing
        assert!(buf.drain_ready().await.is_empty());
        assert_eq!(buf.len().await, 1);

        // Resume and drain
        buf.resume();
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "paused-mark-1");
    }

    #[tokio::test]
    async fn test_mark_for_release_preserves_stopped_flag() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        buf.push(make_entry(
            "stopped-release",
            "propose",
            "job-1",
            Duration::from_secs(3600),
        ))
        .await;

        // Stop the entry first
        assert!(buf.stop("stopped-release").await);

        // Even with hold=0, stopped entries don't drain
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty(), "stopped entry should not drain");

        // mark_for_release sets release_at to now but preserves stopped
        assert!(buf.mark_for_release("stopped-release").await);

        // Still stopped — must explicitly unstop before drain
        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "stopped entry should not drain even after mark_for_release"
        );

        // Unstop → now it should drain immediately
        assert!(buf.unstop("stopped-release").await);
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "stopped-release");
    }

    #[tokio::test]
    async fn test_force_release_atomically_unstops_and_releases() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        buf.push(make_entry(
            "atomic-rel",
            "propose",
            "job-1",
            Duration::from_secs(3600),
        ))
        .await;

        // Stop the entry
        assert!(buf.stop("atomic-rel").await);
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty(), "stopped entry should not drain");

        // force_release atomically clears stopped + sets release_at = now
        assert!(buf.force_release("atomic-rel").await);

        // Should drain immediately in one step
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "atomic-rel");
    }

    #[tokio::test]
    async fn test_force_release_nonexistent_returns_false() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        assert!(!buf.force_release("no-such-entry").await);
    }

    // -----------------------------------------------------------------------
    // stop / unstop tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stopped_entry_not_drained() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        // hold=0 → release_at == now (immediately ready)
        buf.push(make_entry("stop-1", "propose", "job-1", Duration::ZERO))
            .await;

        // Stop the entry
        assert!(buf.stop("stop-1").await);

        // Even though release_at has passed, stopped entries must not drain
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty(), "stopped entry should not drain");
        assert_eq!(buf.len().await, 1, "entry should still be in buffer");
    }

    #[tokio::test]
    async fn test_unstop_makes_entry_drainable() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("unstop-1", "evaluate", "job-1", Duration::ZERO))
            .await;

        // Stop then unstop
        assert!(buf.stop("unstop-1").await);
        assert!(buf.unstop("unstop-1").await);

        // Entry should now drain normally
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "unstop-1");
        assert!(buf.is_empty().await);
    }

    #[tokio::test]
    async fn test_stop_unknown_id_returns_false() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        assert!(!buf.stop("nonexistent").await);
    }

    #[tokio::test]
    async fn test_unstop_unknown_id_returns_false() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        assert!(!buf.unstop("nonexistent").await);
    }

    #[tokio::test]
    async fn test_stop_only_affects_target_entry() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("s-1", "propose", "job-1", Duration::ZERO))
            .await;
        buf.push(make_entry("s-2", "evaluate", "job-1", Duration::ZERO))
            .await;

        // Stop only s-1
        assert!(buf.stop("s-1").await);

        // Only s-2 should drain
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "s-2");
        // s-1 still in buffer
        assert_eq!(buf.len().await, 1);
    }

    #[tokio::test]
    async fn test_stopped_entry_visible_in_list() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry(
            "vis-1",
            "propose",
            "job-1",
            Duration::from_secs(60),
        ))
        .await;

        buf.stop("vis-1").await;

        let entries = buf.list().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].stopped, "stopped flag should be true in list");
    }

    #[tokio::test]
    async fn test_stopped_entry_visible_in_detail() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.push(make_entry(
            "vis-d-1",
            "propose",
            "job-1",
            Duration::from_secs(60),
        ))
        .await;

        buf.stop("vis-d-1").await;

        let detail = buf.get_detail("vis-d-1").await;
        assert!(detail.is_some());
        assert!(
            detail.unwrap().summary.stopped,
            "stopped flag should be true in detail"
        );
    }

    #[tokio::test]
    async fn test_stop_while_paused_still_stops() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("sp-1", "propose", "job-1", Duration::ZERO))
            .await;

        buf.pause();
        assert!(buf.stop("sp-1").await);
        buf.resume();

        // Resumed but stopped — should NOT drain
        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "stopped entry should not drain even after resume"
        );
        assert_eq!(buf.len().await, 1);

        // Unstop → should drain
        buf.unstop("sp-1").await;
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
    }

    // -------------------------------------------------------------------
    // Reply subject preservation tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_reply_subject_preserved_after_edit() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        let entry = make_entry("rs-1", "propose", "job-A", Duration::ZERO);
        let original_subject = entry.reply_subject.clone();
        buf.push(entry).await;

        // Edit the payload
        let new_payload = br#"{"content":"edited by operator"}"#.to_vec();
        let annotation = OperatorAnnotation {
            annotation_type: AnnotationType::Edit,
            comment: "Improved wording".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            original_content_hash: None,
            signatures: Vec::new(),
        };
        assert!(
            buf.update_payload_with_annotation("rs-1", new_payload.clone(), annotation)
                .await
        );

        // Drain and verify reply_subject is unchanged
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].reply_subject, original_subject,
            "reply_subject must survive edits"
        );
        assert_eq!(drained[0].payload, new_payload, "payload should be updated");
        assert!(drained[0].edited, "edited flag should be set");
        assert_eq!(drained[0].annotations.len(), 1);
    }

    #[tokio::test]
    async fn test_reply_subject_preserved_after_multiple_edits() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        let entry = make_entry("rs-2", "evaluate", "job-B", Duration::ZERO);
        let original_subject = entry.reply_subject.clone();
        buf.push(entry).await;

        // First edit
        buf.update_payload_with_annotation(
            "rs-2",
            b"v2".to_vec(),
            OperatorAnnotation {
                annotation_type: AnnotationType::Edit,
                comment: "First edit".into(),
                timestamp: "t1".into(),
                original_content_hash: None,
                signatures: Vec::new(),
            },
        )
        .await;

        // Second edit
        buf.update_payload_with_annotation(
            "rs-2",
            b"v3".to_vec(),
            OperatorAnnotation {
                annotation_type: AnnotationType::Edit,
                comment: "Second edit".into(),
                timestamp: "t2".into(),
                original_content_hash: None,
                signatures: Vec::new(),
            },
        )
        .await;

        // Add comment
        buf.add_comment(
            "rs-2",
            OperatorAnnotation {
                annotation_type: AnnotationType::Comment,
                comment: "LGTM".into(),
                timestamp: "t3".into(),
                original_content_hash: None,
                signatures: Vec::new(),
            },
        )
        .await;

        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].reply_subject, original_subject,
            "reply_subject must survive multiple edits"
        );
        assert_eq!(
            drained[0].payload, b"v3",
            "payload should reflect last edit"
        );
        assert_eq!(drained[0].annotations.len(), 3, "all annotations preserved");
    }

    #[tokio::test]
    async fn test_reply_subject_preserved_after_stop_edit_unstop() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        let entry = make_entry("rs-3", "propose", "job-C", Duration::ZERO);
        let original_subject = entry.reply_subject.clone();
        buf.push(entry).await;

        // Simulate regen flow: stop → edit → unstop → drain
        assert!(buf.stop("rs-3").await);

        // Edit while stopped (regen replaces content)
        buf.update_payload_with_annotation(
            "rs-3",
            br#"{"content":"regenerated proposal"}"#.to_vec(),
            OperatorAnnotation {
                annotation_type: AnnotationType::Edit,
                comment: "Regenerated by operator".into(),
                timestamp: "t1".into(),
                original_content_hash: None,
                signatures: Vec::new(),
            },
        )
        .await;

        // Still stopped — should not drain
        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "stopped entry should not drain even after edit"
        );

        // Unstop
        assert!(buf.unstop("rs-3").await);

        // Now should drain with original reply_subject
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].reply_subject, original_subject,
            "reply_subject must survive stop→edit→unstop cycle"
        );
        assert_eq!(
            std::str::from_utf8(&drained[0].payload).unwrap(),
            r#"{"content":"regenerated proposal"}"#
        );
    }

    #[tokio::test]
    async fn test_double_stop_is_idempotent() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("ds-1", "propose", "j", Duration::ZERO))
            .await;

        assert!(buf.stop("ds-1").await);
        assert!(buf.stop("ds-1").await); // second stop is fine
        assert!(buf.drain_ready().await.is_empty());

        assert!(buf.unstop("ds-1").await);
        assert_eq!(buf.drain_ready().await.len(), 1);
    }

    #[tokio::test]
    async fn test_double_unstop_is_idempotent() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("du-1", "propose", "j", Duration::ZERO))
            .await;
        buf.stop("du-1").await;

        assert!(buf.unstop("du-1").await);
        assert!(buf.unstop("du-1").await); // second unstop is fine
        assert_eq!(buf.drain_ready().await.len(), 1);
    }

    // -------------------------------------------------------------------
    // Edit on non-existent/already-drained entries
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_edit_nonexistent_returns_false() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        let result = buf
            .update_payload_with_annotation(
                "ghost",
                b"new".to_vec(),
                OperatorAnnotation {
                    annotation_type: AnnotationType::Edit,
                    comment: "".into(),
                    timestamp: "t".into(),
                    original_content_hash: None,
                    signatures: Vec::new(),
                },
            )
            .await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_edit_after_drain_returns_false() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("ed-1", "propose", "j", Duration::ZERO))
            .await;
        buf.drain_ready().await; // drains it

        let result = buf
            .update_payload_with_annotation(
                "ed-1",
                b"too late".to_vec(),
                OperatorAnnotation {
                    annotation_type: AnnotationType::Edit,
                    comment: "".into(),
                    timestamp: "t".into(),
                    original_content_hash: None,
                    signatures: Vec::new(),
                },
            )
            .await;
        assert!(!result, "cannot edit an already-drained entry");
    }

    // -------------------------------------------------------------------
    // Mixed: multiple entries, selective stop/edit/drain
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_selective_stop_only_blocks_target() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("m-1", "propose", "j", Duration::ZERO))
            .await;
        buf.push(make_entry("m-2", "evaluate", "j", Duration::ZERO))
            .await;
        buf.push(make_entry("m-3", "propose", "j", Duration::ZERO))
            .await;

        buf.stop("m-2").await;

        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 2, "only non-stopped entries should drain");
        let ids: Vec<&str> = drained.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"m-1"));
        assert!(ids.contains(&"m-3"));
        assert!(!ids.contains(&"m-2"));

        // m-2 still in buffer
        assert_eq!(buf.len().await, 1);
        assert!(buf.get_detail("m-2").await.is_some());
    }

    #[tokio::test]
    async fn test_job_id_and_action_preserved_through_full_lifecycle() {
        use crate::agents::{AnnotationType, OperatorAnnotation};

        let buf = ResponseBuffer::new(Duration::ZERO);
        let mut entry = make_entry("lc-1", "evaluate", "job-XYZ", Duration::ZERO);
        entry.round = 3;
        entry.reply_subject = "nsed.job-XYZ.result.3.agent.evaluate".into();
        buf.push(entry).await;

        // Stop
        buf.stop("lc-1").await;

        // Edit
        buf.update_payload_with_annotation(
            "lc-1",
            b"edited".to_vec(),
            OperatorAnnotation {
                annotation_type: AnnotationType::Edit,
                comment: "regen".into(),
                timestamp: "t".into(),
                original_content_hash: None,
                signatures: Vec::new(),
            },
        )
        .await;

        // Unstop
        buf.unstop("lc-1").await;

        // Drain
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        let e = &drained[0];
        assert_eq!(e.job_id, "job-XYZ");
        assert_eq!(e.action, "evaluate");
        assert_eq!(e.round, 3);
        assert_eq!(e.reply_subject, "nsed.job-XYZ.result.3.agent.evaluate");
        assert!(e.edited);
    }

    // -------------------------------------------------------------------
    // compute_divergence tests
    // -------------------------------------------------------------------

    #[test]
    fn test_compute_divergence_both_signals() {
        // score = 0.6 → soft = 0.6/1.6 = 0.375 → (1-0.375)/2 = 0.3125
        // std_div = 0.25 / 1.0 = 0.25
        // effective = max(0.3125, 0.25) = 0.3125
        let div = super::compute_divergence(Some(0.6), Some(0.25));
        assert!((div.unwrap() - 0.3125).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_score_only() {
        // score = 1.0 → soft = 0.5 → (1-0.5)/2 = 0.25
        let div = super::compute_divergence(Some(1.0), None);
        assert!((div.unwrap() - 0.25).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_score_low() {
        // score = -0.8 → soft = -0.8/1.8 = -0.444 → (1+0.444)/2 = 0.722
        let div = super::compute_divergence(Some(-0.8), None);
        assert!((div.unwrap() - 0.722).abs() < 0.02);
    }

    #[test]
    fn test_compute_divergence_std_dev_only() {
        // std_div = 0.25 / 1.0 = 0.25
        let div = super::compute_divergence(None, Some(0.25));
        assert!((div.unwrap() - 0.25).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_none() {
        let div = super::compute_divergence(None, None);
        assert!(div.is_none());
    }

    #[test]
    fn test_compute_divergence_perfect_score() {
        // score = 3.0 (strong endorsement) → soft = 0.75 → (1-0.75)/2 = 0.125
        // std_dev = 0 → std_div = 0
        // effective = max(0.125, 0) = 0.125
        let div = super::compute_divergence(Some(3.0), Some(0.0));
        assert!((div.unwrap() - 0.125).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_worst_score() {
        // score = -3.0 → soft = -0.75 → (1+0.75)/2 = 0.875
        // std_dev = 1.2 → std_div = 1.2/1.0 → clamped 1.0
        // effective = max(0.875, 1.0) = 1.0
        let div = super::compute_divergence(Some(-3.0), Some(1.2));
        assert!((div.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_compute_divergence_large_positive_score() {
        // Large positive → very low divergence (asymptotically → 0)
        let div = super::compute_divergence(Some(10.0), None);
        // soft = 10/11 ≈ 0.909 → (1-0.909)/2 ≈ 0.045
        assert!(
            div.unwrap() < 0.1,
            "large positive should give low divergence"
        );
    }

    #[test]
    fn test_compute_divergence_large_negative_score() {
        // Large negative → very high divergence (asymptotically → 1)
        let div = super::compute_divergence(Some(-10.0), None);
        // soft = -10/11 ≈ -0.909 → (1+0.909)/2 ≈ 0.955
        assert!(
            div.unwrap() > 0.9,
            "large negative should give high divergence"
        );
    }

    // -------------------------------------------------------------------
    // Auto-approve tests
    // -------------------------------------------------------------------

    #[test]
    fn test_auto_approve_default_on() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert!(
            buf.is_auto_approve(),
            "auto-approve should be ON by default"
        );
    }

    #[test]
    fn test_auto_approve_toggle() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert!(buf.is_auto_approve());
        buf.set_auto_approve(false);
        assert!(!buf.is_auto_approve());
        buf.set_auto_approve(true);
        assert!(buf.is_auto_approve());
    }

    #[test]
    fn test_auto_approve_threshold_default() {
        // Default is 1.0 (100%) — combined with auto_approve=true, this
        // makes the buffer a true pass-through that releases every entry
        // regardless of divergence. Operators who want the old 50% gate
        // must set it explicitly.
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        assert!(
            (buf.auto_approve_threshold() - 1.0).abs() < 0.01,
            "auto_approve_threshold default should be 1.0 (release everything)"
        );
    }

    #[tokio::test]
    async fn test_default_config_releases_every_entry_regardless_of_divergence() {
        // End-to-end check of the new pass-through default:
        // `auto_approve = true` + `threshold = 1.0` should release
        // every pending entry on the next `auto_release_if_eligible`
        // call, no matter what divergence value is reported.
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        // Do NOT call set_auto_approve / set_auto_approve_threshold —
        // we explicitly want to exercise the fresh defaults.
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;
        buf.push(make_entry("b", "propose", "job-2", Duration::from_secs(60)))
            .await;
        buf.push(make_entry("c", "propose", "job-3", Duration::from_secs(60)))
            .await;

        // Maximum possible divergence (the compute_divergence formula
        // clamps to [0, 1], so 1.0 is the worst case). The default
        // threshold of 1.0 still permits release because the gate is
        // `div > threshold`, not `div >= threshold`.
        let count = buf.auto_release_if_eligible(Some(1.0)).await;
        assert_eq!(
            count, 3,
            "all three entries should auto-release under the default 100% threshold"
        );
    }

    #[test]
    fn test_auto_approve_threshold_set_and_get() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        buf.set_auto_approve_threshold(0.75);
        assert!((buf.auto_approve_threshold() - 0.75).abs() < 0.01);
        buf.set_auto_approve_threshold(0.1);
        assert!((buf.auto_approve_threshold() - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_auto_approve_threshold_clamped() {
        let buf = ResponseBuffer::new(Duration::from_secs(10));
        buf.set_auto_approve_threshold(-0.5);
        assert!((buf.auto_approve_threshold() - 0.0).abs() < 0.01);
        buf.set_auto_approve_threshold(2.0);
        assert!((buf.auto_approve_threshold() - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_auto_release_when_eligible() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // Divergence 0.2 < threshold 0.5 → should auto-release
        let count = buf.auto_release_if_eligible(Some(0.2)).await;
        assert_eq!(count, 1);
        // Now drain_ready() should pick it up
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
    }

    #[tokio::test]
    async fn test_auto_release_skipped_when_disabled() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(false);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        let count = buf.auto_release_if_eligible(Some(0.2)).await;
        assert_eq!(count, 0);
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn test_auto_release_skipped_when_divergence_above_threshold() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.3);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // Divergence 0.5 >= threshold 0.3 → no auto-release
        let count = buf.auto_release_if_eligible(Some(0.5)).await;
        assert_eq!(count, 0);
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn test_auto_release_with_no_divergence_data_trusts_operator() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // None divergence → operator explicitly enabled auto, trust the click
        let count = buf.auto_release_if_eligible(None).await;
        assert_eq!(count, 1);
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
    }

    #[tokio::test]
    async fn test_auto_release_respects_stopped_flag() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;
        // Stop the entry
        buf.stop("a").await;

        // Low divergence but entry is stopped → should NOT auto-release
        let count = buf.auto_release_if_eligible(Some(0.1)).await;
        assert_eq!(count, 0);
        let drained = buf.drain_ready().await;
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn test_auto_release_at_exact_threshold_releases() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // Divergence exactly at threshold → eligible (inclusive boundary).
        // Operator sets threshold to X% meaning "approve up to X%".
        let count = buf.auto_release_if_eligible(Some(0.5)).await;
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_auto_release_above_threshold_does_not_release() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;

        // Divergence strictly above threshold → not eligible
        let count = buf.auto_release_if_eligible(Some(0.51)).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_auto_release_multiple_entries() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.5);
        buf.push(make_entry("a", "propose", "job-1", Duration::from_secs(60)))
            .await;
        buf.push(make_entry(
            "b",
            "evaluate",
            "job-1",
            Duration::from_secs(60),
        ))
        .await;
        buf.push(make_entry("c", "propose", "job-2", Duration::from_secs(60)))
            .await;

        let count = buf.auto_release_if_eligible(Some(0.2)).await;
        assert_eq!(count, 3);
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 3);
    }

    // -------------------------------------------------------------------
    // OVERDUE invariant: buffered items always have release_in_ms >= 0
    // -------------------------------------------------------------------
    //
    // User-visible invariant: "if a card is in the rainfall, it should
    // NOT be possible to be overdue, because the SLA math is pre-
    // calculated."  The backend guarantees this in two ways:
    //
    //   1. push_with_deadline() clamps release_at to max(deadline, now),
    //      so release_in_ms >= 0 immediately after push.
    //
    //   2. drain_ready() removes entries once now >= release_at, so any
    //      entry still in the buffer has either:
    //        a) release_at in the future (release_in_ms > 0), or
    //        b) release_at just passed but drain hasn't fired yet
    //           (acceptable race; < 500ms drain cycle).
    //
    // The tests below verify both properties.

    /// After push_with_deadline (SLA configured), release_in_ms must be
    /// non-negative — the entry is guaranteed to stay in the buffer for
    /// at least the SLA duration minus reserve.
    #[tokio::test]
    async fn test_invariant_release_in_ms_non_negative_after_push() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_response_sla(Duration::from_secs(600));

        let task_received = Instant::now();
        let entry = make_entry("inv-1", "propose", "job-1", Duration::from_secs(60));
        buf.push_with_deadline(entry, task_received).await;

        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        assert!(
            list[0].release_in_ms >= 0,
            "invariant: buffered item must have release_in_ms >= 0, got {}",
            list[0].release_in_ms,
        );
    }

    /// When the agent takes a long time generating (tool calls etc.),
    /// task_received is well in the past. push_with_deadline clamps
    /// release_at to max(deadline, now), so release_in_ms >= 0 even
    /// though the SLA has technically expired relative to task_received.
    #[tokio::test]
    async fn test_invariant_slow_agent_still_non_negative() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_response_sla(Duration::from_secs(600));

        // Agent took 700s to generate — 100s past SLA.
        let task_received = Instant::now() - Duration::from_secs(700);
        let entry = make_entry("inv-2", "propose", "job-1", Duration::from_secs(60));
        buf.push_with_deadline(entry, task_received).await;

        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        assert!(
            list[0].release_in_ms >= 0,
            "invariant: even for slow agents, release_in_ms must be >= 0, got {}",
            list[0].release_in_ms,
        );

        // The entry should be immediately drainable since release_at was
        // clamped to `now` (past deadline).
        let drained = buf.drain_ready().await;
        assert_eq!(
            drained.len(),
            1,
            "past-deadline entry should drain immediately"
        );
    }

    /// Paused + past-deadline: release_in_ms can go negative (held by
    /// operator pause), but this is fine because the dashboard shows
    /// "Releasing…" instead of "OVERDUE". The key check: drain_ready
    /// returns nothing while paused, keeping the entry in the buffer.
    #[tokio::test]
    async fn test_invariant_paused_entry_stays_in_buffer() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("inv-3", "propose", "job-1", Duration::ZERO))
            .await;

        // Pause BEFORE drain → entry should stay in buffer
        buf.pause();

        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "paused buffer must not drain — entry stays visible in rainfall"
        );
        assert_eq!(buf.len().await, 1, "entry must remain in buffer");

        // Even with release_at passed, entry is still listed
        let list = buf.list().await;
        assert_eq!(list.len(), 1);
        // release_in_ms may be 0 or slightly negative here — that's OK,
        // the frontend shows "Releasing…" not "OVERDUE"
    }

    /// Stopped entry: release_at can pass but the entry stays in the
    /// buffer (operator can un-stop and release). Verify it doesn't drain.
    #[tokio::test]
    async fn test_invariant_stopped_entry_stays_in_buffer() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        buf.push(make_entry("inv-4", "propose", "job-1", Duration::ZERO))
            .await;

        buf.stop("inv-4").await;

        // release_at == now (hold=0), but stopped → should NOT drain
        let drained = buf.drain_ready().await;
        assert!(
            drained.is_empty(),
            "stopped entry must not drain even though release_at passed"
        );
        assert_eq!(buf.len().await, 1);
    }

    /// The full invariant cycle: push → list (positive) → wait → drain
    /// → verify the entry never existed in a "buffered + overdue" state
    /// without the operator's knowledge.
    #[tokio::test]
    async fn test_invariant_full_lifecycle_no_surprise_overdue() {
        let buf = ResponseBuffer::new(Duration::from_secs(60));
        buf.set_auto_approve(false);
        buf.set_response_sla(Duration::from_secs(600));

        let task_received = Instant::now();
        let entry = make_entry("inv-5", "propose", "job-1", Duration::from_secs(60));
        buf.push_with_deadline(entry, task_received).await;

        // Immediately after push: release_in_ms > 0
        let snap1 = buf.list().await;
        assert!(snap1[0].release_in_ms > 0, "snap1: should be positive");

        // Simulate time passing (we can't fast-forward Instant, but we
        // can verify get_detail gives the same guarantee)
        let detail = buf.get_detail("inv-5").await.unwrap();
        assert!(
            detail.summary.release_in_ms > 0,
            "get_detail: should be positive immediately after push"
        );

        // Entry has not been drained — still in buffer
        assert_eq!(buf.len().await, 1, "entry still buffered");
    }

    // -------------------------------------------------------------------
    // Additional coverage: stop/unstop, mark_for_release, divergence,
    // auto_release_if_eligible
    // -------------------------------------------------------------------

    /// Test that stop("job_1") prevents entries from appearing in
    /// drain_ready(), and unstop("job_1") makes them drainable again.
    #[tokio::test]
    async fn test_buffer_stop_and_unstop() {
        let buf = ResponseBuffer::new(Duration::ZERO);
        // Push two entries for different jobs; hold=0 means immediately ready
        buf.push(make_entry("e1", "propose", "job_1", Duration::ZERO))
            .await;
        buf.push(make_entry("e2", "evaluate", "job_2", Duration::ZERO))
            .await;

        // Stop e1 — it should no longer appear in drain_ready
        assert!(buf.stop("e1").await);

        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1, "only e2 should drain");
        assert_eq!(drained[0].id, "e2");
        assert_eq!(buf.len().await, 1, "e1 should still be in buffer");

        // Unstop e1 — it should now be drainable
        assert!(buf.unstop("e1").await);
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1, "e1 should drain after unstop");
        assert_eq!(drained[0].id, "e1");
        assert!(buf.is_empty().await);
    }

    /// Test that mark_for_release("id") sets the entry's release_at to
    /// now, making it immediately drainable even with a long hold.
    #[tokio::test]
    async fn test_buffer_mark_for_release() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        buf.push(make_entry(
            "mr-1",
            "propose",
            "job-1",
            Duration::from_secs(600),
        ))
        .await;
        buf.push(make_entry(
            "mr-2",
            "evaluate",
            "job-1",
            Duration::from_secs(600),
        ))
        .await;

        // Neither should drain yet (600s hold)
        assert!(buf.drain_ready().await.is_empty());

        // Mark only mr-1 for release
        assert!(buf.mark_for_release("mr-1").await);

        // mr-1 should drain immediately; mr-2 stays
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, "mr-1");
        assert_eq!(buf.len().await, 1, "mr-2 should still be held");

        // Unknown ID returns false
        assert!(!buf.mark_for_release("nonexistent").await);
    }

    /// Test compute_divergence with specific scenarios:
    /// - Strong endorsement (low divergence)
    /// - Rejected with high stddev (saturates at 1.0)
    /// - Empty recent scores (should return None)
    #[test]
    fn test_buffer_compute_divergence() {
        // High positive score with zero stddev → low divergence
        // score = 3.0 → soft = 0.75 → (1-0.75)/2 = 0.125
        let div = super::compute_divergence(Some(3.0), Some(0.0));
        assert!(
            div.unwrap() < 0.2,
            "strong endorsement should have low divergence, got {}",
            div.unwrap()
        );

        // Negative score + high stddev → divergence saturates at 1.0
        // score = -2.0 → soft = -0.667 → (1+0.667)/2 = 0.833
        // std_dev = 1.5 → std_div = 1.5 → clamped 1.0
        // effective = max(0.833, 1.0) = 1.0
        let div_high = super::compute_divergence(Some(-2.0), Some(1.5));
        assert!(
            (div_high.unwrap() - 1.0).abs() < 0.01,
            "rejected + high stddev should saturate divergence, got {}",
            div_high.unwrap()
        );

        // No scores at all → None (divergence unknown)
        let div_empty = super::compute_divergence(None, None);
        assert!(
            div_empty.is_none(),
            "empty recent scores should return None"
        );
    }

    /// Test that entries with divergence below the auto-approve threshold
    /// are auto-released, while high-divergence entries are held.
    #[tokio::test]
    async fn test_buffer_auto_release_if_eligible() {
        let buf = ResponseBuffer::new(Duration::from_secs(600));
        buf.set_auto_approve(true);
        buf.set_auto_approve_threshold(0.4);

        // Push three entries with long hold
        buf.push(make_entry(
            "ar-1",
            "propose",
            "job-1",
            Duration::from_secs(600),
        ))
        .await;
        buf.push(make_entry(
            "ar-2",
            "evaluate",
            "job-1",
            Duration::from_secs(600),
        ))
        .await;
        // Stop ar-2 to test that stopped entries are excluded
        buf.stop("ar-2").await;
        buf.push(make_entry(
            "ar-3",
            "propose",
            "job-2",
            Duration::from_secs(600),
        ))
        .await;

        // Low divergence (0.1 < threshold 0.4) → non-stopped entries should
        // be marked for immediate release
        let count = buf.auto_release_if_eligible(Some(0.1)).await;
        assert_eq!(count, 2, "only non-stopped entries should be auto-released");

        // Drain: ar-1 and ar-3 should drain; ar-2 stays (stopped)
        let drained = buf.drain_ready().await;
        assert_eq!(drained.len(), 2);
        let ids: Vec<&str> = drained.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"ar-1"));
        assert!(ids.contains(&"ar-3"));
        assert!(!ids.contains(&"ar-2"));
        assert_eq!(buf.len().await, 1, "ar-2 should remain (stopped)");

        // Now test high divergence: push a new entry and try with div above threshold
        buf.push(make_entry(
            "ar-4",
            "propose",
            "job-3",
            Duration::from_secs(600),
        ))
        .await;
        let count_high = buf.auto_release_if_eligible(Some(0.6)).await;
        assert_eq!(
            count_high, 0,
            "high divergence should not auto-release any entries"
        );
        assert!(
            buf.drain_ready().await.is_empty(),
            "no entries should drain when divergence is above threshold"
        );
    }
}
