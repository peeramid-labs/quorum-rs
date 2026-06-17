use std::collections::HashSet;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::cli::remote::{AgentInfo, DiscoveredRoom, HealthResponse};

/// Unified event type for the TUI event loop.
#[derive(Debug)]
pub enum AppEvent {
    /// Terminal input event (key press, resize, etc.).
    Terminal(Event),
    /// Periodic tick for UI refresh (animations, spinners).
    Tick,
    /// Async data arrived from the client layer.
    Data(DataEvent),
}

/// Async data events delivered from `TuiClient` via mpsc channel.
#[derive(Debug, Clone)]
pub enum DataEvent {
    PoliciesLoaded {
        orchestrator: String,
        policies: Vec<PolicyInfo>,
    },
    AgentsLoaded {
        orchestrator: String,
        agents: Vec<AgentInfo>,
    },
    RoomsLoaded {
        orchestrator: String,
        rooms: Vec<DiscoveredRoom>,
    },
    /// A room create/delete succeeded; carries a human verb for the status
    /// line. The view refetches to refresh the list.
    RoomMutated {
        orchestrator: String,
        action: String,
        id: String,
    },
    HealthResult {
        orchestrator: String,
        result: Result<HealthResponse, String>,
    },
    SseEvent(SseEvent),
    JobSubmitted {
        job_id: String,
        orchestrator: String,
    },
    /// A message was successfully injected into a running deliberation.
    MessageInjected {
        job_id: String,
        sequence: usize,
        round: u32,
    },
    FetchError {
        context: String,
        error: String,
    },
}

/// SSE event types consumed by the live deliberation view.
#[derive(Debug, Clone, PartialEq)]
pub enum SseEvent {
    Connected,
    RoundStart {
        round: u32,
        total_rounds: u32,
    },
    AgentWorking {
        agent_id: String,
        action: String,
    },
    ProposalSubmitted {
        round: u32,
        agent_id: String,
        content: String,
        thought_process: String,
    },
    EvaluationSubmitted {
        round: u32,
        evaluator_id: String,
        evaluations: Vec<EvaluationEntry>,
    },
    BudgetPhaseComplete {
        round: Option<u32>,
        phase: String,
        budgeted_secs: f64,
        actual_secs: f64,
        under_budget: bool,
    },
    RoundSummary {
        round: u32,
        convergence_score: f32,
        proposal_scores: Vec<ProposalScore>,
    },
    RoundComplete {
        round: u32,
        total_rounds: u32,
    },
    JobComplete {
        status: String,
        job_id: String,
        rounds_completed: u32,
        best_proposal_content: String,
        best_proposal_score: f32,
        best_proposal_author: String,
    },
    Timeout(String),
    Unknown {
        event_type: String,
    },
}

/// Evaluation entry within an `EvaluationSubmitted` event.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationEntry {
    pub target_id: String,
    pub score: f32,
    pub justification: String,
}

/// Proposal score from a `RoundSummary` event.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposalScore {
    pub agent_id: String,
    pub aggregated_score: f32,
}

/// Policy info returned from the policies API.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PolicyInfo {
    pub policy_id: String,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_rounds", alias = "rounds")]
    pub max_rounds: u32,
    #[serde(alias = "convergence_threshold", default = "default_effort")]
    pub effort: f32,
    #[serde(default)]
    pub is_role_based: bool,
}

fn default_rounds() -> u32 {
    3
}

fn default_effort() -> f32 {
    0.85
}

/// Tracks per-round agent lifecycle state for the live view.
#[derive(Debug, Default, Clone)]
pub struct PhaseTracker {
    pub proposing: HashSet<String>,
    pub proposed: HashSet<String>,
    pub evaluating: HashSet<String>,
    pub evaluated: HashSet<String>,
}

impl PhaseTracker {
    /// Reset all sets at the start of a new round.
    pub fn reset(&mut self) {
        self.proposing.clear();
        self.proposed.clear();
        self.evaluating.clear();
        self.evaluated.clear();
    }

    /// Agents dispatched for a phase but not yet delivered.
    pub fn stragglers(&self, phase: &str) -> Vec<String> {
        let (dispatched, delivered) = match phase {
            "propose" => (&self.proposing, &self.proposed),
            "evaluate" => (&self.evaluating, &self.evaluated),
            _ => return Vec::new(),
        };
        let mut result: Vec<_> = dispatched.difference(delivered).cloned().collect();
        result.sort();
        result
    }

    /// Current status of an agent.
    pub fn agent_status(&self, agent_id: &str) -> AgentPhaseStatus {
        if self.evaluated.contains(agent_id) {
            AgentPhaseStatus::Evaluated
        } else if self.evaluating.contains(agent_id) {
            AgentPhaseStatus::Evaluating
        } else if self.proposed.contains(agent_id) {
            AgentPhaseStatus::Proposed
        } else if self.proposing.contains(agent_id) {
            AgentPhaseStatus::Proposing
        } else {
            AgentPhaseStatus::Idle
        }
    }

    /// All known agents (union of all sets), sorted.
    pub fn all_agents(&self) -> Vec<String> {
        let mut agents: HashSet<String> = HashSet::new();
        agents.extend(self.proposing.iter().cloned());
        agents.extend(self.proposed.iter().cloned());
        agents.extend(self.evaluating.iter().cloned());
        agents.extend(self.evaluated.iter().cloned());
        let mut result: Vec<_> = agents.into_iter().collect();
        result.sort();
        result
    }
}

/// Agent lifecycle status within a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPhaseStatus {
    Idle,
    Proposing,
    Proposed,
    Evaluating,
    Evaluated,
}

impl AgentPhaseStatus {
    /// Icon representation for TUI display.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Idle => "·",
            Self::Proposing => "⟳",
            Self::Proposed => "✓",
            Self::Evaluating => "⟳",
            Self::Evaluated => "✓✓",
        }
    }

    /// Short label for display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Proposing => "proposing",
            Self::Proposed => "proposed",
            Self::Evaluating => "evaluating",
            Self::Evaluated => "evaluated",
        }
    }
}

/// Event loop configuration.
pub struct EventLoopConfig {
    /// Tick interval for UI refresh.
    pub tick_rate: Duration,
}

impl Default for EventLoopConfig {
    fn default() -> Self {
        Self {
            tick_rate: Duration::from_millis(250),
        }
    }
}

/// Spawn the terminal event polling loop.
///
/// Sends `AppEvent::Terminal` for key/mouse/resize events and
/// `AppEvent::Tick` at the configured interval.
pub fn spawn_terminal_event_loop(config: EventLoopConfig) -> mpsc::UnboundedReceiver<AppEvent> {
    let (tx, rx) = mpsc::unbounded_channel();

    std::thread::spawn(move || {
        loop {
            match event::poll(config.tick_rate) {
                Ok(true) => match event::read() {
                    Ok(evt) => {
                        if tx.send(AppEvent::Terminal(evt)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {
                    if tx.send(AppEvent::Tick).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    rx
}

/// Helper to check if a key event matches a character.
pub fn is_key(event: &Event, c: char) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
            ..
        }) if *ch == c
    )
}

/// Helper to check if a key event is Escape.
pub fn is_escape(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Esc,
            ..
        })
    )
}

/// Helper to check if a key event is Enter.
pub fn is_enter(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            ..
        })
    )
}

/// Helper to check for up navigation (Up arrow or k).
pub fn is_up(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Up | KeyCode::Char('k'),
            ..
        })
    )
}

/// Helper to check for down navigation (Down arrow or j).
pub fn is_down(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Down | KeyCode::Char('j'),
            ..
        })
    )
}

/// Helper to check for Tab key.
pub fn is_tab(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Tab,
            ..
        })
    )
}

/// Helper to check for PageUp.
pub fn is_page_up(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::PageUp,
            ..
        })
    )
}

/// Helper to check for PageDown.
pub fn is_page_down(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::PageDown,
            ..
        })
    )
}

/// Helper to check for Left arrow (or `h`, vim-style).
pub fn is_left(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Left | KeyCode::Char('h'),
            ..
        })
    )
}

/// Helper to check for Right arrow (or `l`, vim-style).
pub fn is_right(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Right | KeyCode::Char('l'),
            ..
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_tracker_reset() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("a".into());
        tracker.proposed.insert("a".into());
        tracker.evaluating.insert("b".into());
        tracker.evaluated.insert("b".into());

        tracker.reset();

        assert!(tracker.proposing.is_empty());
        assert!(tracker.proposed.is_empty());
        assert!(tracker.evaluating.is_empty());
        assert!(tracker.evaluated.is_empty());
    }

    #[test]
    fn phase_tracker_stragglers() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("a".into());
        tracker.proposing.insert("b".into());
        tracker.proposed.insert("a".into());

        let stragglers = tracker.stragglers("propose");
        assert_eq!(stragglers, vec!["b"]);
    }

    #[test]
    fn phase_tracker_stragglers_evaluate() {
        let mut tracker = PhaseTracker::default();
        tracker.evaluating.insert("x".into());
        tracker.evaluating.insert("y".into());
        tracker.evaluated.insert("y".into());

        let stragglers = tracker.stragglers("evaluate");
        assert_eq!(stragglers, vec!["x"]);
    }

    #[test]
    fn phase_tracker_stragglers_unknown_phase() {
        let tracker = PhaseTracker::default();
        assert!(tracker.stragglers("unknown").is_empty());
    }

    #[test]
    fn phase_tracker_agent_status() {
        let mut tracker = PhaseTracker::default();
        assert_eq!(tracker.agent_status("a"), AgentPhaseStatus::Idle);

        tracker.proposing.insert("a".into());
        assert_eq!(tracker.agent_status("a"), AgentPhaseStatus::Proposing);

        tracker.proposed.insert("a".into());
        assert_eq!(tracker.agent_status("a"), AgentPhaseStatus::Proposed);

        tracker.evaluating.insert("a".into());
        assert_eq!(tracker.agent_status("a"), AgentPhaseStatus::Evaluating);

        tracker.evaluated.insert("a".into());
        assert_eq!(tracker.agent_status("a"), AgentPhaseStatus::Evaluated);
    }

    #[test]
    fn phase_tracker_all_agents() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("b".into());
        tracker.proposed.insert("a".into());
        tracker.evaluating.insert("c".into());

        let agents = tracker.all_agents();
        assert_eq!(agents, vec!["a", "b", "c"]);
    }

    #[test]
    fn agent_phase_status_icon_and_label() {
        assert_eq!(AgentPhaseStatus::Idle.icon(), "·");
        assert_eq!(AgentPhaseStatus::Proposing.icon(), "⟳");
        assert_eq!(AgentPhaseStatus::Proposed.icon(), "✓");
        assert_eq!(AgentPhaseStatus::Evaluating.icon(), "⟳");
        assert_eq!(AgentPhaseStatus::Evaluated.icon(), "✓✓");

        assert_eq!(AgentPhaseStatus::Idle.label(), "idle");
        assert_eq!(AgentPhaseStatus::Proposing.label(), "proposing");
        assert_eq!(AgentPhaseStatus::Proposed.label(), "proposed");
        assert_eq!(AgentPhaseStatus::Evaluating.label(), "evaluating");
        assert_eq!(AgentPhaseStatus::Evaluated.label(), "evaluated");
    }

    #[test]
    fn event_loop_config_default() {
        let config = EventLoopConfig::default();
        assert_eq!(config.tick_rate, Duration::from_millis(250));
    }

    // ---- Key helper tests ----

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn char_event(c: char) -> Event {
        key_event(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn is_key_matches_char() {
        assert!(is_key(&char_event('q'), 'q'));
        assert!(!is_key(&char_event('q'), 'x'));
    }

    #[test]
    fn is_key_rejects_modified_char() {
        let ctrl_q = key_event(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(!is_key(&ctrl_q, 'q'));
    }

    #[test]
    fn is_key_rejects_non_key_event() {
        let resize = Event::Resize(80, 24);
        assert!(!is_key(&resize, 'q'));
    }

    #[test]
    fn is_escape_matches() {
        assert!(is_escape(&key_event(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_escape(&char_event('q')));
    }

    #[test]
    fn is_enter_matches() {
        assert!(is_enter(&key_event(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!is_enter(&char_event('q')));
    }

    #[test]
    fn is_up_matches_arrow_and_k() {
        assert!(is_up(&key_event(KeyCode::Up, KeyModifiers::NONE)));
        assert!(is_up(&char_event('k')));
        assert!(!is_up(&char_event('j')));
        assert!(!is_up(&key_event(KeyCode::Down, KeyModifiers::NONE)));
    }

    #[test]
    fn is_down_matches_arrow_and_j() {
        assert!(is_down(&key_event(KeyCode::Down, KeyModifiers::NONE)));
        assert!(is_down(&char_event('j')));
        assert!(!is_down(&char_event('k')));
        assert!(!is_down(&key_event(KeyCode::Up, KeyModifiers::NONE)));
    }

    #[test]
    fn is_tab_matches() {
        assert!(is_tab(&key_event(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(!is_tab(&char_event('t')));
    }

    // ---- PolicyInfo deserialization tests ----

    #[test]
    fn policy_info_defaults() {
        let json = r#"{"policy_id": "p1", "name": "Test"}"#;
        let info: PolicyInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.policy_id, "p1");
        assert_eq!(info.name, "Test");
        assert!(info.tags.is_empty());
        assert_eq!(info.max_rounds, 3);
        assert!((info.effort - 0.85).abs() < f32::EPSILON);
        assert!(!info.is_role_based);
    }

    #[test]
    fn policy_info_full() {
        let json = r#"{
            "policy_id": "p2",
            "name": "Full",
            "tags": ["security", "review"],
            "rounds": 5,
            "effort": 0.9,
            "is_role_based": true
        }"#;
        let info: PolicyInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.max_rounds, 5);
        assert!((info.effort - 0.9).abs() < f32::EPSILON);
        assert_eq!(info.tags, vec!["security", "review"]);
        assert!(info.is_role_based);
    }

    #[test]
    fn policy_info_convergence_threshold_alias() {
        let json = r#"{
            "policy_id": "p3",
            "name": "Legacy",
            "convergence_threshold": 0.75
        }"#;
        let info: PolicyInfo = serde_json::from_str(json).unwrap();
        assert!((info.effort - 0.75).abs() < f32::EPSILON);
    }

    // ---- DataEvent / SseEvent coverage ----

    #[test]
    fn sse_event_unknown_variant() {
        let evt = SseEvent::Unknown {
            event_type: "custom".into(),
        };
        assert!(matches!(evt, SseEvent::Unknown { event_type } if event_type == "custom"));
    }

    #[test]
    fn data_event_debug_format() {
        let evt = DataEvent::FetchError {
            context: "test".into(),
            error: "oops".into(),
        };
        let debug = format!("{evt:?}");
        assert!(debug.contains("FetchError"));
        assert!(debug.contains("oops"));
    }

    #[test]
    fn phase_tracker_stragglers_empty_sets() {
        let tracker = PhaseTracker::default();
        assert!(tracker.stragglers("propose").is_empty());
        assert!(tracker.stragglers("evaluate").is_empty());
    }

    #[test]
    fn phase_tracker_all_agents_deduplicates() {
        let mut tracker = PhaseTracker::default();
        tracker.proposing.insert("a".into());
        tracker.proposed.insert("a".into());
        tracker.evaluating.insert("a".into());
        tracker.evaluated.insert("a".into());
        let agents = tracker.all_agents();
        assert_eq!(agents, vec!["a"]);
    }
}
