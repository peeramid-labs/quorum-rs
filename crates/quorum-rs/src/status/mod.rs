//! Agent status monitoring types + optional HTTP dashboard.
//!
//! Provides [`AgentStatusSnapshot`] — a real-time view of agent state for
//! operator monitoring — and the associated event/task log entry types.
//!
//! Enable the `status-server` feature to serve a lightweight dashboard
//! via an embedded axum server (see [`server`] / [`multi_server`]).

pub mod agent_events;

#[cfg(feature = "status-server")]
pub mod server;

#[cfg(feature = "status-server")]
pub mod multi_server;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use utoipa::ToSchema;

/// Maximum number of recent tasks retained in the snapshot.
const MAX_RECENT_TASKS: usize = 20;

/// Maximum number of event log entries retained in the snapshot.
///
/// The event log is a fixed-size ring buffer, not a time window: it holds the
/// most recent `MAX_EVENT_LOG` lifecycle events per agent regardless of age.
/// Time-scoped views (e.g. the 24h error feed) filter this buffer by timestamp,
/// so on an agent that emits more than this many events within the window, the
/// oldest in-window entries are evicted before the cutoff.
pub(crate) const MAX_EVENT_LOG: usize = 200;

/// Maximum number of recent peer evaluation scores retained.
const MAX_RECENT_SCORES: usize = 50;

/// Shared handle to the agent status snapshot.
pub type SharedAgentStatus = Arc<RwLock<AgentStatusSnapshot>>;

/// Real-time snapshot of agent state for operator monitoring.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentStatusSnapshot {
    pub agent_id: String,
    pub model_name: String,
    pub provider_id: String,
    pub nats_connected: bool,
    pub current_job: Option<String>,
    pub current_round: Option<u32>,
    /// Current deliberation phase: `"propose"`, `"evaluate"`, or `null`.
    pub current_phase: Option<String>,
    pub uptime_secs: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    #[schema(value_type = Vec<TaskLogEntry>)]
    pub recent_tasks: VecDeque<TaskLogEntry>,
    pub scratchpad_keys: u64,
    /// Chronological event log for the dashboard event stream.
    #[schema(value_type = Vec<EventLogEntry>)]
    pub event_log: VecDeque<EventLogEntry>,
    /// Whether the agent is paused (HITL control plane).
    pub is_paused: bool,
    /// Number of responses currently held in the HITL buffer.
    pub buffered_count: u32,
    /// Rolling error rate: `tasks_failed / (tasks_completed + tasks_failed)`.
    pub error_rate: f32,
    /// Recent peer evaluation scores received from the orchestrator.
    /// Primary divergence indicator — consistently low scores flag a problem.
    #[schema(value_type = Vec<ScoreEntry>)]
    pub recent_scores: VecDeque<ScoreEntry>,
    /// Rolling mean of `recent_scores`. `None` if no scores received yet.
    pub mean_score: Option<f32>,
    /// Standard deviation of recent scores — higher values indicate divergence.
    pub score_std_dev: Option<f32>,
    /// Whether the agent is flagged for operator attention.
    pub is_flagged: bool,
    /// Human-readable reason why the agent is flagged.
    pub flag_reason: Option<String>,
}

/// Log entry for a completed task.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaskLogEntry {
    pub timestamp: String,
    pub action: String,
    pub job_id: String,
    pub round: u32,
    /// `"ok"` or `"error"`.
    pub status: String,
    pub duration_ms: u64,
    /// Truncated preview of the response content (proposal text or evaluation summary).
    /// Kept short to avoid bloating the status snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_preview: Option<String>,
}

/// Event log entry for the dashboard event stream.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EventLogEntry {
    pub timestamp: String,
    /// Event type: `"agent_accepted"`, `"agent_working"`, `"task_complete"`,
    /// `"agent_error"`, `"heartbeat"`, `"connected"`, etc.
    pub event_type: String,
    /// Job or session ID (if applicable).
    pub job_id: Option<String>,
    /// Human-readable detail string.
    pub detail: String,
}

/// Score received from a peer evaluator via the orchestrator.
///
/// Tracks how this agent's proposals are rated by others — a consistently
/// low `mean_score` across the `recent_scores` deque is the primary
/// divergence flag in the HITL control plane UI.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ScoreEntry {
    pub timestamp: String,
    /// Session / job ID.
    pub job_id: String,
    /// Deliberation round number.
    pub round: u32,
    /// ID of the evaluator who scored this agent.
    pub evaluator: String,
    /// The evaluation score.
    pub score: f32,
}

impl AgentStatusSnapshot {
    /// Create a new snapshot with identity fields populated.
    pub fn new(agent_id: String, model_name: String, provider_id: String) -> Self {
        Self {
            agent_id,
            model_name,
            provider_id,
            nats_connected: false,
            current_job: None,
            current_round: None,
            current_phase: None,
            uptime_secs: 0,
            tasks_completed: 0,
            tasks_failed: 0,
            recent_tasks: VecDeque::with_capacity(MAX_RECENT_TASKS + 1),
            scratchpad_keys: 0,
            event_log: VecDeque::with_capacity(MAX_EVENT_LOG + 1),
            is_paused: false,
            buffered_count: 0,
            error_rate: 0.0,
            recent_scores: VecDeque::with_capacity(MAX_RECENT_SCORES + 1),
            mean_score: None,
            score_std_dev: None,
            is_flagged: false,
            flag_reason: None,
        }
    }

    /// Record a completed task. Trims the log to `MAX_RECENT_TASKS`.
    pub fn push_task(&mut self, entry: TaskLogEntry) {
        if entry.status == "ok" {
            self.tasks_completed += 1;
        } else {
            self.tasks_failed += 1;
        }
        let total = self.tasks_completed + self.tasks_failed;
        // total is always ≥ 1 here because we just incremented a counter above.
        self.error_rate = self.tasks_failed as f32 / total as f32;
        self.recent_tasks.push_front(entry);
        if self.recent_tasks.len() > MAX_RECENT_TASKS {
            self.recent_tasks.pop_back();
        }
    }

    /// Record a peer evaluation score and update `mean_score`, `score_std_dev`,
    /// and flagging state.
    pub fn push_score(&mut self, entry: ScoreEntry) {
        self.recent_scores.push_back(entry);
        if self.recent_scores.len() > MAX_RECENT_SCORES {
            self.recent_scores.pop_front();
        }
        // Recompute rolling mean and std dev.
        // n is always ≥ 1 here because we just pushed an entry above.
        let n = self.recent_scores.len();
        let sum: f32 = self.recent_scores.iter().map(|s| s.score).sum();
        let mean = sum / n as f32;
        self.mean_score = Some(mean);
        if n >= 2 {
            let variance: f32 = self
                .recent_scores
                .iter()
                .map(|s| (s.score - mean).powi(2))
                .sum::<f32>()
                / n as f32;
            self.score_std_dev = Some(variance.sqrt());
        } else {
            self.score_std_dev = None;
        }
        self.check_flags();
    }

    /// Evaluate flagging conditions based on recent score history.
    ///
    /// Scores are signed QV sums (`Σ score_q_s`): positive = endorsed, negative
    /// = rejected.  Thresholds are calibrated for this range.
    ///
    /// - **Rejected**: Recent 3-score average < −0.3 → flagged.
    /// - **High divergence**: Score std_dev > 1.5 → flagged.
    /// - Flags are cleared when conditions no longer apply.
    pub fn check_flags(&mut self) {
        let n = self.recent_scores.len();

        // Check recent-3 average for persistently rejected proposals
        if n >= 3 {
            let recent_3_avg: f32 = self
                .recent_scores
                .iter()
                .rev()
                .take(3)
                .map(|s| s.score)
                .sum::<f32>()
                / 3.0;
            /// Score below this threshold flags the agent as persistently rejected.
            const LOW_SCORE_THRESHOLD: f32 = -0.3;
            if recent_3_avg < LOW_SCORE_THRESHOLD {
                self.is_flagged = true;
                self.flag_reason = Some(format!("Low scores: recent avg {:.2}", recent_3_avg));
                return;
            }
        }

        // Check std_dev for high divergence flag
        if let Some(std_dev) = self.score_std_dev {
            /// Std-dev above this threshold flags the agent as high-divergence.
            const HIGH_DIVERGENCE_THRESHOLD: f32 = 1.5;
            if std_dev > HIGH_DIVERGENCE_THRESHOLD {
                self.is_flagged = true;
                self.flag_reason = Some(format!("High divergence: std_dev {:.1}", std_dev));
                return;
            }
        }

        // No flag conditions met — clear
        self.is_flagged = false;
        self.flag_reason = None;
    }

    /// Append a lifecycle event to the event stream log.
    pub fn push_event(&mut self, event_type: &str, job_id: Option<&str>, detail: &str) {
        self.event_log.push_back(EventLogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            event_type: event_type.to_string(),
            job_id: job_id.map(String::from),
            detail: detail.to_string(),
        });
        if self.event_log.len() > MAX_EVENT_LOG {
            self.event_log.pop_front();
        }
    }
}

/// Create a new shared status snapshot handle.
pub fn new_shared_status(
    agent_id: String,
    model_name: String,
    provider_id: String,
) -> SharedAgentStatus {
    Arc::new(RwLock::new(AgentStatusSnapshot::new(
        agent_id,
        model_name,
        provider_id,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_push_task_trims() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..25 {
            snap.push_task(TaskLogEntry {
                timestamp: format!("2025-01-01T00:00:{:02}Z", i),
                action: "propose".into(),
                job_id: format!("job-{}", i),
                round: 1,
                status: if i % 3 == 0 {
                    "error".into()
                } else {
                    "ok".into()
                },
                duration_ms: 100,
                content_preview: None,
            });
        }
        assert_eq!(snap.recent_tasks.len(), MAX_RECENT_TASKS);
        // i % 3 == 0 → errors at 0,3,6,9,12,15,18,21,24 = 9 errors
        assert_eq!(snap.tasks_failed, 9);
        assert_eq!(snap.tasks_completed, 16);
    }

    #[test]
    fn test_snapshot_counters() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_task(TaskLogEntry {
            timestamp: "t".into(),
            action: "propose".into(),
            job_id: "j1".into(),
            round: 1,
            status: "ok".into(),
            duration_ms: 50,
            content_preview: None,
        });
        snap.push_task(TaskLogEntry {
            timestamp: "t".into(),
            action: "evaluate".into(),
            job_id: "j1".into(),
            round: 1,
            status: "error".into(),
            duration_ms: 100,
            content_preview: None,
        });
        assert_eq!(snap.tasks_completed, 1);
        assert_eq!(snap.tasks_failed, 1);
        assert_eq!(snap.recent_tasks.len(), 2);
        // Most recent first
        assert_eq!(snap.recent_tasks[0].action, "evaluate");
    }

    #[test]
    fn test_event_log_push_and_trim() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..250 {
            snap.push_event("test", Some("job1"), &format!("event {}", i));
        }
        assert_eq!(snap.event_log.len(), MAX_EVENT_LOG);
        // Oldest events should have been trimmed
        assert!(snap.event_log.front().unwrap().detail.contains("50"));
    }

    #[test]
    fn test_event_log_entry_fields() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("agent_accepted", Some("job-123"), "Accepted manifest");
        assert_eq!(snap.event_log.len(), 1);
        let entry = snap.event_log.front().unwrap();
        assert_eq!(entry.event_type, "agent_accepted");
        assert_eq!(entry.job_id.as_deref(), Some("job-123"));
        assert_eq!(entry.detail, "Accepted manifest");
        assert!(!entry.timestamp.is_empty());
    }

    #[test]
    fn test_snapshot_new_defaults() {
        let snap = AgentStatusSnapshot::new("agent-1".into(), "gpt-4".into(), "openai".into());
        assert_eq!(snap.agent_id, "agent-1");
        assert_eq!(snap.model_name, "gpt-4");
        assert_eq!(snap.provider_id, "openai");
        assert!(!snap.nats_connected);
        assert!(snap.current_job.is_none());
        assert!(snap.current_round.is_none());
        assert!(snap.current_phase.is_none());
        assert_eq!(snap.uptime_secs, 0);
        assert_eq!(snap.tasks_completed, 0);
        assert_eq!(snap.tasks_failed, 0);
        assert!(snap.recent_tasks.is_empty());
        assert_eq!(snap.scratchpad_keys, 0);
        assert!(snap.event_log.is_empty());
    }

    #[test]
    fn test_push_event_without_job_id() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("heartbeat", None, "Still alive");
        assert_eq!(snap.event_log.len(), 1);
        let entry = snap.event_log.front().unwrap();
        assert_eq!(entry.event_type, "heartbeat");
        assert!(entry.job_id.is_none());
        assert_eq!(entry.detail, "Still alive");
    }

    #[test]
    fn test_push_task_ordering() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..3 {
            snap.push_task(TaskLogEntry {
                timestamp: format!("t{}", i),
                action: format!("action-{}", i),
                job_id: "j1".into(),
                round: 1,
                status: "ok".into(),
                duration_ms: 10,
                content_preview: None,
            });
        }
        // LIFO order: most recent first
        assert_eq!(snap.recent_tasks[0].action, "action-2");
        assert_eq!(snap.recent_tasks[1].action, "action-1");
        assert_eq!(snap.recent_tasks[2].action, "action-0");
    }

    #[test]
    fn test_snapshot_serialization() {
        let mut snap =
            AgentStatusSnapshot::new("agent-x".into(), "claude".into(), "anthropic".into());
        snap.nats_connected = true;
        snap.tasks_completed = 5;
        snap.push_event("connected", None, "Connected to NATS");
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"agent_id\":\"agent-x\""));
        assert!(json.contains("\"nats_connected\":true"));
        assert!(json.contains("\"tasks_completed\":5"));
        assert!(json.contains("\"event_log\""));
    }

    #[tokio::test]
    async fn test_new_shared_status() {
        let shared = new_shared_status("agent-1".into(), "model".into(), "provider".into());
        let snap = shared.read().await;
        assert_eq!(snap.agent_id, "agent-1");
        assert_eq!(snap.model_name, "model");
        assert_eq!(snap.provider_id, "provider");
        assert!(!snap.nats_connected);
    }

    // -------------------------------------------------------------------
    // HITL control plane fields
    // -------------------------------------------------------------------

    #[test]
    fn test_snapshot_new_has_default_hitl_fields() {
        let snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        assert!(!snap.is_paused);
        assert_eq!(snap.buffered_count, 0);
        assert_eq!(snap.error_rate, 0.0);
        assert!(snap.recent_scores.is_empty());
        assert!(snap.mean_score.is_none());
    }

    #[test]
    fn test_snapshot_error_rate_computed() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..4 {
            snap.push_task(TaskLogEntry {
                timestamp: "t".into(),
                action: "propose".into(),
                job_id: format!("j{}", i),
                round: 1,
                status: if i == 3 { "error" } else { "ok" }.into(),
                duration_ms: 10,
                content_preview: None,
            });
        }
        // 3 ok + 1 error → error_rate = 0.25
        assert!((snap.error_rate - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn test_snapshot_error_rate_zero_tasks() {
        let snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        assert_eq!(snap.error_rate, 0.0);
    }

    #[test]
    fn test_snapshot_push_score_updates_mean() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_score(ScoreEntry {
            timestamp: "t1".into(),
            job_id: "j1".into(),
            round: 1,
            evaluator: "BETA".into(),
            score: 6.0,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t2".into(),
            job_id: "j1".into(),
            round: 1,
            evaluator: "GAMMA".into(),
            score: 9.0,
        });
        snap.push_score(ScoreEntry {
            timestamp: "t3".into(),
            job_id: "j2".into(),
            round: 1,
            evaluator: "BETA".into(),
            score: 3.0,
        });
        assert_eq!(snap.recent_scores.len(), 3);
        // mean = (6 + 9 + 3) / 3 = 6.0
        assert!((snap.mean_score.unwrap() - 6.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_snapshot_push_score_trims_to_max() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        for i in 0..60 {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: 1,
                evaluator: "e".into(),
                score: i as f32,
            });
        }
        assert_eq!(snap.recent_scores.len(), MAX_RECENT_SCORES);
        // Oldest (0..9) trimmed; remaining are 10..59
        assert_eq!(snap.recent_scores.front().unwrap().score, 10.0);
    }

    #[test]
    fn test_score_entry_serialization() {
        let entry = ScoreEntry {
            timestamp: "2025-01-01T00:00:00Z".into(),
            job_id: "job-abc".into(),
            round: 3,
            evaluator: "BETA".into(),
            score: 7.5,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let roundtripped: ScoreEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.job_id, "job-abc");
        assert_eq!(roundtripped.round, 3);
        assert_eq!(roundtripped.evaluator, "BETA");
        assert!((roundtripped.score - 7.5).abs() < f32::EPSILON);
    }

    // -------------------------------------------------------------------
    // Divergence flagging tests
    // -------------------------------------------------------------------

    fn push_n_scores(snap: &mut AgentStatusSnapshot, scores: &[f32]) {
        for (i, &s) in scores.iter().enumerate() {
            snap.push_score(ScoreEntry {
                timestamp: format!("t{}", i),
                job_id: "j".into(),
                round: 1,
                evaluator: "e".into(),
                score: s,
            });
        }
    }

    #[test]
    fn test_push_score_computes_std_dev() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        push_n_scores(&mut snap, &[4.0, 6.0, 8.0]);
        // mean = 6.0, variance = ((4-6)^2 + (6-6)^2 + (8-6)^2) / 3 = 8/3
        // std_dev = sqrt(8/3) ≈ 1.633
        let std_dev = snap.score_std_dev.unwrap();
        assert!((std_dev - 1.633).abs() < 0.01, "std_dev was {}", std_dev);
    }

    #[test]
    fn test_std_dev_none_with_single_score() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        push_n_scores(&mut snap, &[5.0]);
        assert!(snap.score_std_dev.is_none());
    }

    #[test]
    fn test_check_flags_low_scores() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        // Recent 3 scores consistently negative (rejected proposals)
        push_n_scores(&mut snap, &[-0.5, -0.8, -0.4]);
        assert!(snap.is_flagged, "should be flagged for low scores");
        assert!(
            snap.flag_reason.as_ref().unwrap().contains("Low scores"),
            "flag_reason: {:?}",
            snap.flag_reason
        );
    }

    #[test]
    fn test_check_flags_high_divergence() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        // Scores with high variance: std_dev > 1.5
        push_n_scores(&mut snap, &[-2.0, 2.0, -2.0, 2.0, -2.0, 2.0]);
        assert!(
            snap.score_std_dev.unwrap() > 1.5,
            "std_dev should be > 1.5, got {}",
            snap.score_std_dev.unwrap()
        );
        assert!(snap.is_flagged);
        assert!(
            snap.flag_reason.as_ref().unwrap().contains("divergence"),
            "flag_reason: {:?}",
            snap.flag_reason
        );
    }

    #[test]
    fn test_check_flags_clears_when_scores_improve() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        // Start flagged with negative scores
        push_n_scores(&mut snap, &[-0.5, -0.6, -0.7]);
        assert!(snap.is_flagged);

        // Add positive scores — recent 3 avg now above -0.3
        push_n_scores(&mut snap, &[0.8, 0.9, 1.0]);
        assert!(!snap.is_flagged, "flag should be cleared after good scores");
        assert!(snap.flag_reason.is_none());
    }

    #[test]
    fn test_check_flags_not_flagged_with_good_scores() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        // Positive signed scores = endorsed proposals
        push_n_scores(&mut snap, &[0.7, 0.8, 0.9]);
        assert!(!snap.is_flagged);
        assert!(snap.flag_reason.is_none());
    }

    #[test]
    fn test_snapshot_new_has_default_flag_fields() {
        let snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        assert!(!snap.is_flagged);
        assert!(snap.flag_reason.is_none());
        assert!(snap.score_std_dev.is_none());
    }

    #[test]
    fn test_agent_summary_includes_flag_fields_in_serialization() {
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        // Consistently rejected proposals → triggers low-score flag
        push_n_scores(&mut snap, &[-0.5, -0.6, -0.7]);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"is_flagged\":true"));
        assert!(json.contains("\"flag_reason\""));
        assert!(json.contains("\"score_std_dev\""));
    }

    #[test]
    fn test_402_payment_sets_paused_and_flagged() {
        // Simulates the worker's 402 Payment Required handling:
        // the worker detects a 402 error and sets is_paused + is_flagged + flag_reason.
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        assert!(!snap.is_paused);
        assert!(!snap.is_flagged);
        assert!(snap.flag_reason.is_none());

        // Simulate what the worker does on 402
        snap.is_paused = true;
        snap.is_flagged = true;
        snap.flag_reason = Some("Provider payment required (402) — agent paused".to_string());

        assert!(snap.is_paused);
        assert!(snap.is_flagged);
        assert_eq!(
            snap.flag_reason.as_deref(),
            Some("Provider payment required (402) — agent paused")
        );

        // Verify it serializes correctly for the dashboard
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"is_paused\":true"));
        assert!(json.contains("\"is_flagged\":true"));
        assert!(json.contains("402"));
        assert!(json.contains("payment"));
    }

    // -------------------------------------------------------------------
    // Event lifecycle ordering tests
    // -------------------------------------------------------------------

    #[test]
    fn test_event_lifecycle_ordering_non_buffered() {
        // Simulates the non-buffered lifecycle:
        // connected → agent_working → task_complete
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("connected", None, "NATS connected");
        snap.push_event("agent_working", Some("job-1"), "Round 1 propose");
        snap.push_event("task_complete", Some("job-1"), "propose ok 150ms");

        let types: Vec<&str> = snap
            .event_log
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert_eq!(types, vec!["connected", "agent_working", "task_complete"]);
    }

    #[test]
    fn test_event_lifecycle_ordering_buffered() {
        // Simulates the buffered lifecycle:
        // connected → agent_working → response_buffered → buffer_released → task_complete
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("connected", None, "NATS connected");
        snap.push_event("agent_working", Some("job-1"), "Round 1 propose");
        snap.push_event(
            "response_buffered",
            Some("job-1"),
            "Round 1 propose buffered 5000ms hold",
        );
        snap.push_event(
            "buffer_released",
            Some("job-1"),
            "Round 1 propose released from buffer",
        );
        snap.push_event("task_complete", Some("job-1"), "Round 1 propose released");

        let types: Vec<&str> = snap
            .event_log
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec![
                "connected",
                "agent_working",
                "response_buffered",
                "buffer_released",
                "task_complete"
            ]
        );

        // All buffered events should have the same job_id
        let job_events: Vec<_> = snap
            .event_log
            .iter()
            .filter(|e| e.job_id.as_deref() == Some("job-1"))
            .collect();
        assert_eq!(job_events.len(), 4, "4 events should reference job-1");
    }

    #[test]
    fn test_event_lifecycle_with_stop_unstop() {
        // Simulates: buffered → operator stops → edits → unstops → released
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("agent_working", Some("job-1"), "Round 1 propose");
        snap.push_event(
            "response_buffered",
            Some("job-1"),
            "Round 1 propose buffered 5000ms hold",
        );
        snap.push_event(
            "buffer_stopped",
            Some("entry-1"),
            "Response stopped by operator",
        );
        snap.push_event("buffer_unstopped", Some("entry-1"), "Stop removed");
        snap.push_event("buffer_released", Some("job-1"), "Round 1 propose released");
        snap.push_event("task_complete", Some("job-1"), "Round 1 propose released");

        let types: Vec<&str> = snap
            .event_log
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec![
                "agent_working",
                "response_buffered",
                "buffer_stopped",
                "buffer_unstopped",
                "buffer_released",
                "task_complete"
            ]
        );
    }

    #[test]
    fn test_event_lifecycle_with_reject() {
        // Simulates: buffered → operator rejects
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("agent_working", Some("job-1"), "Round 1 propose");
        snap.push_event(
            "response_buffered",
            Some("job-1"),
            "Round 1 propose buffered",
        );
        snap.push_event(
            "buffer_rejected",
            Some("job-1"),
            "propose rejected by operator",
        );

        let types: Vec<&str> = snap
            .event_log
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec!["agent_working", "response_buffered", "buffer_rejected"]
        );
        // No task_complete after reject — task is discarded
    }

    #[test]
    fn test_event_lifecycle_multi_agent_interleaved() {
        // Two agents working concurrently — events interleave correctly
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        snap.push_event("agent_working", Some("job-A"), "Round 1 propose");
        snap.push_event("agent_working", Some("job-B"), "Round 1 evaluate");
        snap.push_event(
            "response_buffered",
            Some("job-A"),
            "Round 1 propose buffered",
        );
        snap.push_event("task_complete", Some("job-B"), "evaluate ok");
        snap.push_event("buffer_released", Some("job-A"), "Round 1 propose released");
        snap.push_event("task_complete", Some("job-A"), "Round 1 propose released");

        assert_eq!(snap.event_log.len(), 6);
        // Verify job-A events are in order (not necessarily contiguous)
        let job_a: Vec<&str> = snap
            .event_log
            .iter()
            .filter(|e| e.job_id.as_deref() == Some("job-A"))
            .map(|e| e.event_type.as_str())
            .collect();
        assert_eq!(
            job_a,
            vec![
                "agent_working",
                "response_buffered",
                "buffer_released",
                "task_complete"
            ]
        );
    }

    #[test]
    fn test_event_buffered_count_tracking() {
        // Verify buffered_count field tracks correctly
        let mut snap = AgentStatusSnapshot::new("a".into(), "m".into(), "p".into());
        assert_eq!(snap.buffered_count, 0);

        snap.buffered_count = 3;
        assert_eq!(snap.buffered_count, 3);

        snap.buffered_count = 0;
        assert_eq!(snap.buffered_count, 0);
    }
}
