use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use super::common::{ListState, render_key_hints, truncate};
use super::thread::{next_word_boundary, prev_word_boundary};
use super::{FetchRequest, View, ViewAction};
use crate::cli::tui::event::{self, AgentPhaseStatus, AppEvent, DataEvent, PhaseTracker, SseEvent};

/// Job status in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Connecting,
    Running,
    Complete,
    Failed,
    Disconnected,
}

/// Panel selection for keyboard navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Panel {
    Agents,
    Proposals,
    Evaluations,
    Budget,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Self::Agents => Self::Proposals,
            Self::Proposals => Self::Evaluations,
            Self::Evaluations => Self::Budget,
            Self::Budget => Self::Agents,
        }
    }
}

/// A submitted proposal in the live view.
#[derive(Debug, Clone)]
pub struct ProposalEntry {
    pub round: u32,
    pub agent_id: String,
    pub content: String,
    pub thought_process: String,
    pub score: Option<f32>,
}

/// Budget phase entry.
#[derive(Debug, Clone)]
pub struct BudgetPhaseEntry {
    pub round: Option<u32>,
    pub phase: String,
    pub budgeted_secs: f64,
    pub actual_secs: f64,
    pub under_budget: bool,
}

/// Live deliberation view — the core TUI feature.
pub struct JobDetailView {
    pub job_id: String,
    pub status: JobStatus,
    pub current_round: u32,
    pub total_rounds: u32,
    pub phase_tracker: PhaseTracker,
    pub proposals: Vec<ProposalEntry>,
    /// Evaluations keyed by target_id → Vec<(round, evaluator_id, score, justification)>.
    pub evaluations: HashMap<String, Vec<(u32, String, f32, String)>>,
    pub convergence_scores: Vec<f32>,
    pub budget_phases: Vec<BudgetPhaseEntry>,
    pub final_result: Option<JobCompleteData>,
    pub active_panel: Panel,
    pub show_thought_process: bool,
    pub show_justifications: bool,
    pub inject_input_active: bool,
    pub inject_text: String,
    /// Caret byte offset into `inject_text` (always on a char boundary).
    pub inject_cursor: usize,
    /// Full-screen compose overlay for the injection draft — room to read/edit
    /// a large or multi-line pasted message.
    pub inject_fullscreen: bool,
    pub injection_count: usize,
    /// Which round the user is currently browsing. `None` = follow the
    /// live round (auto-advance as new rounds start). `Some(n)` = pin
    /// the view to round `n` so the audience can study previous-round
    /// proposals + evaluations without losing them when the deliberation
    /// advances.
    pub viewed_round: Option<u32>,
    /// When true (default after completion), show the result summary.
    /// Tab toggles to the detailed panels view.
    pub show_result: bool,
    /// When set, show full-text detail for the selected evaluation target.
    pub eval_detail_target: Option<String>,
    pub eval_detail_scroll: usize,
    /// When true, show full-text detail for the selected proposal.
    pub proposal_detail_visible: bool,
    pub proposal_detail_scroll: usize,
    proposal_scroll: ListState,
    eval_scroll: ListState,
    orchestrator: String,
    stream_started: bool,
}

/// Final job result data.
#[derive(Debug, Clone)]
pub struct JobCompleteData {
    pub status: String,
    pub rounds_completed: u32,
    pub best_proposal_content: String,
    pub best_proposal_score: f32,
    pub best_proposal_author: String,
}

impl JobDetailView {
    pub fn new(job_id: String, orchestrator: String) -> Self {
        Self {
            job_id,
            status: JobStatus::Connecting,
            current_round: 0,
            total_rounds: 0,
            phase_tracker: PhaseTracker::default(),
            proposals: Vec::new(),
            evaluations: HashMap::new(),
            convergence_scores: Vec::new(),
            budget_phases: Vec::new(),
            final_result: None,
            active_panel: Panel::Agents,
            show_thought_process: false,
            show_justifications: false,
            inject_input_active: false,
            inject_text: String::new(),
            inject_cursor: 0,
            inject_fullscreen: false,
            injection_count: 0,
            viewed_round: None,
            show_result: true,
            eval_detail_target: None,
            eval_detail_scroll: 0,
            proposal_detail_visible: false,
            proposal_detail_scroll: 0,
            proposal_scroll: ListState::new(0),
            eval_scroll: ListState::new(0),
            orchestrator,
            stream_started: false,
        }
    }

    /// Process an SSE event and update internal state.
    pub fn handle_sse_event(&mut self, event: &SseEvent) {
        match event {
            SseEvent::Connected => {
                self.status = JobStatus::Running;
            }
            SseEvent::RoundStart {
                round,
                total_rounds,
            } => {
                self.current_round = *round;
                self.total_rounds = *total_rounds;
                self.phase_tracker.reset();
                // Close any open detail overlays (they reference previous round data).
                self.eval_detail_target = None;
                self.eval_detail_scroll = 0;
                self.proposal_detail_visible = false;
                self.proposal_detail_scroll = 0;
                // Reset scroll counts to match new round's filtered lists.
                self.proposal_scroll
                    .set_count(self.current_proposals().len());
                self.eval_scroll.set_count(self.current_evaluations().len());
            }
            SseEvent::AgentWorking { agent_id, action } => match action.as_str() {
                "propose" => {
                    self.phase_tracker.proposing.insert(agent_id.clone());
                }
                "evaluate" => {
                    self.phase_tracker.evaluating.insert(agent_id.clone());
                }
                _ => {}
            },
            SseEvent::ProposalSubmitted {
                round,
                agent_id,
                content,
                thought_process,
            } => {
                self.phase_tracker.proposed.insert(agent_id.clone());
                self.proposals.push(ProposalEntry {
                    round: *round,
                    agent_id: agent_id.clone(),
                    content: content.clone(),
                    thought_process: thought_process.clone(),
                    score: None,
                });
                self.proposal_scroll
                    .set_count(self.current_proposals().len());
            }
            SseEvent::EvaluationSubmitted {
                round,
                evaluator_id,
                evaluations,
            } => {
                self.phase_tracker.evaluated.insert(evaluator_id.clone());
                for eval in evaluations {
                    self.evaluations
                        .entry(eval.target_id.clone())
                        .or_default()
                        .push((
                            *round,
                            evaluator_id.clone(),
                            eval.score,
                            eval.justification.clone(),
                        ));
                }
                self.eval_scroll.set_count(self.current_evaluations().len());
            }
            SseEvent::BudgetPhaseComplete {
                round,
                phase,
                budgeted_secs,
                actual_secs,
                under_budget,
            } => {
                self.budget_phases.push(BudgetPhaseEntry {
                    round: *round,
                    phase: phase.clone(),
                    budgeted_secs: *budgeted_secs,
                    actual_secs: *actual_secs,
                    under_budget: *under_budget,
                });
            }
            SseEvent::RoundSummary {
                round,
                convergence_score,
                proposal_scores,
            } => {
                self.convergence_scores.push(*convergence_score);
                // Update proposal scores for current round
                for score in proposal_scores {
                    for proposal in &mut self.proposals {
                        if proposal.round == *round && proposal.agent_id == score.agent_id {
                            proposal.score = Some(score.aggregated_score);
                        }
                    }
                }
            }
            SseEvent::RoundComplete { .. } => {
                // Round is done, state already updated
            }
            SseEvent::JobComplete {
                status,
                rounds_completed,
                best_proposal_content,
                best_proposal_score,
                best_proposal_author,
                ..
            } => {
                self.status = if status.eq_ignore_ascii_case("success") {
                    JobStatus::Complete
                } else {
                    JobStatus::Failed
                };
                self.final_result = Some(JobCompleteData {
                    status: status.clone(),
                    rounds_completed: *rounds_completed,
                    best_proposal_content: best_proposal_content.clone(),
                    best_proposal_score: *best_proposal_score,
                    best_proposal_author: best_proposal_author.clone(),
                });
            }
            SseEvent::Timeout(_) => {
                // Don't overwrite a definitive Complete/Failed from JobComplete
                if self.status != JobStatus::Complete && self.status != JobStatus::Failed {
                    self.status = JobStatus::Failed;
                }
            }
            // ask_user HITL is surfaced in the thread reader, not here.
            SseEvent::ToolCallPending { .. } | SseEvent::ToolCallResolved { .. } => {}
            SseEvent::Unknown { .. } => {}
        }
    }

    /// The round number the user is currently looking at — pinned to
    /// [`Self::viewed_round`] if set, otherwise follows the live
    /// [`Self::current_round`].
    pub fn viewed_round_value(&self) -> u32 {
        self.viewed_round.unwrap_or(self.current_round)
    }

    /// `true` when the user is browsing live, `false` when pinned to a
    /// previous round.
    pub fn is_viewing_live(&self) -> bool {
        self.viewed_round.is_none() || self.viewed_round == Some(self.current_round)
    }

    /// Step one round backwards in the history. Pins [`viewed_round`]
    /// the first time it's called (otherwise the view would race against
    /// the live `current_round` advancing on every SSE event).
    pub fn view_previous_round(&mut self) {
        let target = self.viewed_round_value().saturating_sub(1).max(1);
        self.viewed_round = Some(target);
        self.proposal_scroll
            .set_count(self.current_proposals().len());
        self.eval_scroll.set_count(self.current_evaluations().len());
    }

    /// Step one round forwards in history. Clears the pin (returns to
    /// live-follow) when we hit the current round.
    pub fn view_next_round(&mut self) {
        let next = self.viewed_round_value().saturating_add(1);
        if next >= self.current_round {
            self.viewed_round = None; // back to live-follow
        } else {
            self.viewed_round = Some(next);
        }
        self.proposal_scroll
            .set_count(self.current_proposals().len());
        self.eval_scroll.set_count(self.current_evaluations().len());
    }

    /// Reset to live-follow (latest round).
    pub fn view_live(&mut self) {
        self.viewed_round = None;
        self.proposal_scroll
            .set_count(self.current_proposals().len());
        self.eval_scroll.set_count(self.current_evaluations().len());
    }

    /// Proposals for the round the user is currently viewing.
    fn current_proposals(&self) -> Vec<&ProposalEntry> {
        let target = self.viewed_round_value();
        self.proposals
            .iter()
            .filter(|p| p.round == target)
            .collect()
    }

    /// Evaluations for the round the user is currently viewing.
    fn current_evaluations(&self) -> HashMap<&str, Vec<(&str, f32, &str)>> {
        let target = self.viewed_round_value();
        let mut result: HashMap<&str, Vec<(&str, f32, &str)>> = HashMap::new();
        for (target_agent, evals) in &self.evaluations {
            let round_evals: Vec<_> = evals
                .iter()
                .filter(|(round, _, _, _)| *round == target)
                .map(|(_, evaluator, score, justification)| {
                    (evaluator.as_str(), *score, justification.as_str())
                })
                .collect();
            if !round_evals.is_empty() {
                result.insert(target_agent.as_str(), round_evals);
            }
        }
        result
    }

    /// Latest convergence score.
    pub fn latest_convergence(&self) -> Option<f32> {
        self.convergence_scores.last().copied()
    }

    fn clear_inject(&mut self) {
        self.inject_text.clear();
        self.inject_cursor = 0;
        self.inject_fullscreen = false;
    }

    /// Handle keyboard input when injection text input is active. A small line
    /// editor: caret movement (char + word), Home/End, insert/delete at the
    /// caret, bracketed paste, and a `^F` full-screen toggle. `Enter` sends.
    fn update_inject_input(&mut self, event: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{Event, KeyCode, KeyEventKind};

        if event::is_escape(event) {
            // Esc leaves full-screen first, then closes the input.
            if self.inject_fullscreen {
                self.inject_fullscreen = false;
            } else {
                self.inject_input_active = false;
            }
            return None;
        }
        if event::is_ctrl(event, 'f') {
            self.inject_fullscreen = !self.inject_fullscreen;
            return None;
        }
        if event::is_enter(event) && !self.inject_text.is_empty() {
            self.inject_input_active = false;
            if self.status != JobStatus::Running {
                self.clear_inject();
                return None;
            }
            let message = std::mem::take(&mut self.inject_text);
            self.clear_inject();
            return Some(ViewAction::InjectMessage {
                orchestrator: self.orchestrator.clone(),
                job_id: self.job_id.clone(),
                message,
            });
        }
        // Bracketed paste: the terminal delivers the whole clipboard as one
        // Event::Paste, not per-char Key events. Terminals send line breaks as
        // CR/CRLF — normalize so multi-line pastes keep their newlines. Inserted
        // at the caret.
        if let Event::Paste(text) = event {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            self.inject_text.insert_str(self.inject_cursor, &normalized);
            self.inject_cursor += normalized.len();
            return None;
        }
        // Word jumps — check before the plain arrows (which ignore modifiers).
        if event::is_ctrl_left(event) {
            self.inject_cursor = prev_word_boundary(&self.inject_text, self.inject_cursor);
            return None;
        }
        if event::is_ctrl_right(event) {
            self.inject_cursor = next_word_boundary(&self.inject_text, self.inject_cursor);
            return None;
        }
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Left => {
                    self.inject_cursor = prev_char(&self.inject_text, self.inject_cursor)
                }
                KeyCode::Right => {
                    self.inject_cursor = next_char(&self.inject_text, self.inject_cursor)
                }
                KeyCode::Home => self.inject_cursor = 0,
                KeyCode::End => self.inject_cursor = self.inject_text.len(),
                KeyCode::Char(c) => {
                    self.inject_text.insert(self.inject_cursor, c);
                    self.inject_cursor += c.len_utf8();
                }
                KeyCode::Backspace if self.inject_cursor > 0 => {
                    let prev = prev_char(&self.inject_text, self.inject_cursor);
                    self.inject_text.replace_range(prev..self.inject_cursor, "");
                    self.inject_cursor = prev;
                }
                _ => {}
            }
        }
        None
    }
}

/// Previous UTF-8 char boundary before byte offset `i` (0 at the start).
fn prev_char(s: &str, i: usize) -> usize {
    s[..i.min(s.len())]
        .char_indices()
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}

/// Next UTF-8 char boundary after byte offset `i` (len at the end).
fn next_char(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    s[i..]
        .char_indices()
        .nth(1)
        .map(|(j, _)| i + j)
        .unwrap_or(s.len())
}

/// Render `text` as multi-line `Text` with a reversed caret block at byte
/// offset `cursor` — a block over the char under the caret, or a trailing
/// block at end-of-line.
fn caret_text(text: &str, cursor: usize) -> Text<'static> {
    let cur = cursor.min(text.len());
    let mut lines = Vec::new();
    let mut line_start = 0usize;
    for seg in text.split('\n') {
        let seg_end = line_start + seg.len();
        if cur >= line_start && cur <= seg_end {
            lines.push(caret_line_from(seg, cur - line_start));
        } else {
            lines.push(Line::from(seg.to_string()));
        }
        line_start = seg_end + 1; // skip the '\n'
    }
    Text::from(lines)
}

/// One line with a reversed caret block at byte column `col`.
fn caret_line_from(seg: &str, col: usize) -> Line<'static> {
    let col = col.min(seg.len());
    let before = seg[..col].to_string();
    let at = seg[col..].chars().next();
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    let (at_span, after) = match at {
        Some(c) => (
            Span::styled(c.to_string(), caret),
            seg[col + c.len_utf8()..].to_string(),
        ),
        None => (Span::styled(" ", caret), String::new()),
    };
    Line::from(vec![Span::raw(before), at_span, Span::raw(after)])
}

impl View for JobDetailView {
    fn on_enter(&mut self) -> Vec<ViewAction> {
        if !self.stream_started {
            self.stream_started = true;
            vec![ViewAction::Fetch(FetchRequest::StartSseStream {
                orchestrator: self.orchestrator.clone(),
                job_id: self.job_id.clone(),
            })]
        } else {
            Vec::new()
        }
    }

    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        match app_event {
            AppEvent::Terminal(event) => {
                // Injection input mode
                if self.inject_input_active {
                    return self.update_inject_input(event);
                }

                // Stop (cancel) the deliberation while it's non-terminal. `x` is
                // the tmux-proof alias for `^C` (some terminals/tmux never deliver
                // Ctrl+C as a key event). Not gated on `Running`: a job the SSE
                // stream hasn't advanced yet (still Connecting) or a wedged one must
                // still be cancellable. The loop resolves the thread from its map.
                if (event::is_ctrl(event, 'c') || event::is_key(event, 'x'))
                    && !matches!(self.status, JobStatus::Complete | JobStatus::Failed)
                {
                    return Some(ViewAction::Fetch(FetchRequest::CancelJob {
                        orchestrator: self.orchestrator.clone(),
                        job_id: self.job_id.clone(),
                        thread_id: String::new(),
                    }));
                }

                // Detail mode for evaluations
                if self.eval_detail_target.is_some() {
                    if event::is_escape(event) || event::is_key(event, 'q') {
                        self.eval_detail_target = None;
                        self.eval_detail_scroll = 0;
                        return None;
                    }
                    if event::is_up(event) {
                        self.eval_detail_scroll = self.eval_detail_scroll.saturating_sub(1);
                    }
                    if event::is_down(event) {
                        self.eval_detail_scroll += 1;
                    }
                    return None;
                }

                // Detail mode for proposals
                if self.proposal_detail_visible {
                    if event::is_escape(event) || event::is_key(event, 'q') {
                        self.proposal_detail_visible = false;
                        self.proposal_detail_scroll = 0;
                        return None;
                    }
                    if event::is_up(event) {
                        self.proposal_detail_scroll = self.proposal_detail_scroll.saturating_sub(1);
                    }
                    if event::is_down(event) {
                        self.proposal_detail_scroll += 1;
                    }
                    return None;
                }

                if event::is_escape(event) || event::is_key(event, 'q') {
                    return Some(ViewAction::Pop);
                }
                // Round history navigation. ← (or `h`, or `[`, or
                // PgUp) steps back one round; → (or `l`, or `]`, or
                // PgDn) steps forward; `=` returns to live. Arrow keys
                // are the booth-friendly default — works on Mac
                // keyboards that have no PgUp/PgDn.
                if event::is_left(event) || event::is_page_up(event) || event::is_key(event, '[') {
                    self.view_previous_round();
                    return None;
                }
                if event::is_right(event) || event::is_page_down(event) || event::is_key(event, ']')
                {
                    self.view_next_round();
                    return None;
                }
                if event::is_key(event, '=') {
                    self.view_live();
                    return None;
                }
                if event::is_tab(event) {
                    if self.final_result.is_some() && self.show_result {
                        // First Tab press after completion: switch to panels
                        self.show_result = false;
                    } else {
                        self.active_panel = self.active_panel.next();
                        // Wrap back to result view after cycling all panels
                        if self.final_result.is_some() && self.active_panel == Panel::Agents {
                            self.show_result = true;
                        }
                    }
                }
                if event::is_key(event, 't') {
                    if self.final_result.is_some() && self.show_result {
                        // Switch to panels view so thoughts are visible
                        self.show_result = false;
                        self.active_panel = Panel::Proposals;
                    }
                    self.show_thought_process = !self.show_thought_process;
                }
                if event::is_key(event, 'j') && self.active_panel == Panel::Evaluations {
                    self.show_justifications = !self.show_justifications;
                } else if (event::is_key(event, '/') || event::is_key(event, 'i'))
                    && self.status == JobStatus::Running
                {
                    // `/` (chat convention) or `i` (legacy) activates the
                    // chat-style input. The input bar is always visible
                    // at the bottom while the job is running — this just
                    // routes keystrokes into it.
                    self.inject_input_active = true;
                    self.clear_inject();
                } else if (event::is_enter(event) || event::is_key(event, 'd'))
                    && self.active_panel == Panel::Evaluations
                {
                    let current_evals = self.current_evaluations();
                    let mut sorted_targets: Vec<_> = current_evals.keys().collect();
                    sorted_targets.sort();
                    if let Some(target) = sorted_targets.get(self.eval_scroll.selected) {
                        self.eval_detail_target = Some(target.to_string());
                        self.eval_detail_scroll = 0;
                    }
                } else if (event::is_enter(event) || event::is_key(event, 'd'))
                    && self.active_panel == Panel::Proposals
                {
                    let current = self.current_proposals();
                    if current.get(self.proposal_scroll.selected).is_some() {
                        self.proposal_detail_visible = true;
                        self.proposal_detail_scroll = 0;
                    }
                } else if event::is_up(event) {
                    match self.active_panel {
                        Panel::Proposals => self.proposal_scroll.up(),
                        Panel::Evaluations => self.eval_scroll.up(),
                        _ => {}
                    }
                } else if event::is_down(event) {
                    match self.active_panel {
                        Panel::Proposals => self.proposal_scroll.down(),
                        Panel::Evaluations => self.eval_scroll.down(),
                        _ => {}
                    }
                }
                None
            }
            AppEvent::Data(DataEvent::SseEvent(sse_event)) => {
                self.handle_sse_event(sse_event);
                None
            }
            AppEvent::Data(DataEvent::MessageInjected { job_id, .. }) if *job_id == self.job_id => {
                self.injection_count += 1;
                None
            }
            AppEvent::Data(DataEvent::FetchError { context, .. })
                if context.starts_with("sse_stream") =>
            {
                // SSE stream ended or failed — only transition if no terminal status yet
                if self.status != JobStatus::Complete && self.status != JobStatus::Failed {
                    self.status = JobStatus::Disconnected;
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Chat-style input is always visible while the job is running.
        // 3 rows: top + bottom border + 1 line of text.
        let chat_height = if self.status == JobStatus::Running {
            3
        } else {
            0
        };
        let chunks = Layout::vertical([
            Constraint::Length(3),           // header bar
            Constraint::Min(0),              // panels
            Constraint::Length(chat_height), // chat-style input bar
            Constraint::Length(1),           // hints
        ])
        .split(area);

        self.draw_header(frame, chunks[0]);

        // Detail views take over the panels area
        if self.inject_input_active && self.inject_fullscreen {
            self.draw_inject_fullscreen(frame, chunks[1]);
        } else if self.eval_detail_target.is_some() {
            self.draw_eval_detail(frame, chunks[1]);
        } else if self.proposal_detail_visible {
            self.draw_proposal_detail(frame, chunks[1]);
        } else {
            self.draw_panels(frame, chunks[1]);
        }

        if self.status == JobStatus::Running {
            self.draw_chat_input(frame, chunks[2]);
        }

        let hints = if self.eval_detail_target.is_some() || self.proposal_detail_visible {
            vec![("↑↓", "Scroll"), ("Esc", "Close")]
        } else if self.final_result.is_some() && self.show_result {
            vec![("Tab", "Details"), ("t", "Thoughts"), ("Esc", "Back")]
        } else if self.inject_input_active {
            vec![
                ("Enter", "Send"),
                ("^F", "Full screen"),
                ("^←→", "Word"),
                ("Esc", "Cancel"),
            ]
        } else {
            let mut h = vec![("Tab", "Panel"), ("↑↓", "Scroll")];
            if self.active_panel == Panel::Proposals || self.active_panel == Panel::Evaluations {
                h.push(("d", "Detail"));
            }
            h.push(("t", "Thoughts"));
            if self.active_panel == Panel::Evaluations {
                h.push(("j", "Justify"));
            }
            h.push(("PgUp/PgDn", "Round"));
            if self.status == JobStatus::Running {
                h.push(("/", "Steer"));
                h.push(("^C/x", "Stop"));
            }
            h.push(("Esc", "Back"));
            h
        };
        render_key_hints(frame, chunks[3], &hints);
    }
}

/// Map a convergence score in `[0.0, 1.0]` to a "vibes" emoji.
///
/// Used in the booth-friendly header — the audience can see at a glance
/// whether the team is fighting (broken-heart) or agreeing (sparkle).
fn convergence_heart(score: f32) -> &'static str {
    match score {
        s if s < 0.30 => "💔", // hatred / strong disagreement
        s if s < 0.50 => "🥺", // shaky, watching
        s if s < 0.70 => "🤔", // hmm, thinking
        s if s < 0.85 => "💛", // warming up
        s if s < 0.95 => "❤️", // there it is
        _ => "💖✨",           // perfect alignment
    }
}

/// Map a convergence score in `[0.0, 1.0]` to an RGB color that lerps
/// from hatred-red (0.0) through amber (~0.5) to vibrant green (1.0).
///
/// Used as the background tint on the header so the audience reads
/// the room temperature at a glance.
/// Header bg + fg for a given convergence score. Discrete buckets
/// using indexed colors only — Rgb() rendering breaks on terminals
/// without truecolor (SSH sessions, Terminal.app, low-color TERM),
/// where it can fall back to near-black and make the header unreadable.
/// Indexed colors are guaranteed to render across every terminal
/// ratatui supports.
fn convergence_palette(score: f32) -> (Color, Color) {
    let s = score.clamp(0.0, 1.0);
    if s < 0.30 {
        (Color::Red, Color::White)
    } else if s < 0.50 {
        (Color::LightRed, Color::Black)
    } else if s < 0.70 {
        (Color::Yellow, Color::Black)
    } else if s < 0.85 {
        (Color::LightGreen, Color::Black)
    } else {
        (Color::Green, Color::Black)
    }
}

impl JobDetailView {
    /// Render the always-on chat-style input bar at the bottom.
    /// Visible whenever the job is running. Pressing `/` (or any
    /// printable character that isn't a registered hotkey) activates
    /// input mode; Enter sends; Esc cancels.
    fn draw_chat_input(&self, frame: &mut Frame, area: Rect) {
        let (body, title, style) = if self.inject_input_active {
            (
                caret_text(&self.inject_text, self.inject_cursor),
                " ✉ steering (Enter=send · ^F=full screen · Esc=cancel) ",
                Style::default().fg(Color::Yellow),
            )
        } else {
            (
                Text::from("press / to steer the deliberation…"),
                " ✉ ",
                Style::default().fg(Color::DarkGray),
            )
        };
        let input_bar = Paragraph::new(body).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if self.inject_input_active {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        );
        frame.render_widget(input_bar, area);
    }

    /// Full-screen steering editor — room to read/edit a large or multi-line
    /// pasted message. Replaces the panels area while `^F` full-screen is on.
    fn draw_inject_fullscreen(&self, frame: &mut Frame, area: Rect) {
        let body = Paragraph::new(caret_text(&self.inject_text, self.inject_cursor))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" ✉ steering — full screen (Enter=send · ^F/Esc=exit · ^←→ word) ")
                    .border_style(Style::default().fg(Color::Yellow)),
            );
        frame.render_widget(body, area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let status_str = match self.status {
            JobStatus::Connecting => "⟳ Connecting...",
            JobStatus::Running => "● Running",
            JobStatus::Complete => "✨ Converged",
            JobStatus::Failed => "✗ Failed",
            JobStatus::Disconnected => "⚡ Disconnected",
        };
        let status_color = match self.status {
            JobStatus::Connecting => Color::Yellow,
            JobStatus::Running => Color::Green,
            JobStatus::Complete => Color::Cyan,
            JobStatus::Failed | JobStatus::Disconnected => Color::Red,
        };

        // Heart + colored bg encode convergence as room-temperature
        // for booth audiences — fight (red 💔) → consensus (green 💖).
        let conv_score = self.latest_convergence();
        let convergence_text = conv_score
            .map(|c| format!("  Convergence: {c:.2}"))
            .unwrap_or_default();
        let heart = conv_score.map(|c| format!(" {} ", convergence_heart(c)));
        let header_palette = conv_score.map(convergence_palette);

        // Round info: show whether the viewer is pinned to a past round.
        // `Round 1/3 (live)` while following, `Round 1/3 (history)` when
        // pinned. PgUp/PgDn navigates.
        let round_info = if self.total_rounds > 0 {
            let viewing = self.viewed_round_value();
            let suffix = if self.is_viewing_live() {
                "live"
            } else {
                "history"
            };
            format!("  [Round {viewing}/{} · {suffix}]", self.total_rounds)
        } else {
            String::new()
        };

        let inject_info = if self.injection_count > 0 {
            format!("  Injections: {}", self.injection_count)
        } else {
            String::new()
        };

        // When the convergence palette is active it carries the signal;
        // the per-span semantic colors (status green/red, cyan, magenta)
        // become low-contrast noise against a saturated bg. Force every
        // span to the palette's fg so the bg always reads cleanly.
        let forced_fg = header_palette.map(|(_, fg)| fg);
        let styled = |text: String, color: Color| -> Span<'_> {
            if let Some(fg) = forced_fg {
                Span::styled(text, Style::default().fg(fg))
            } else {
                Span::styled(text, Style::default().fg(color))
            }
        };

        let mut spans = vec![styled(status_str.to_string(), status_color)];
        if let Some(h) = heart {
            spans.push(Span::raw(h));
        }
        spans.push(Span::raw(round_info));
        spans.push(styled(convergence_text, Color::Cyan));
        spans.push(styled(inject_info, Color::Magenta));
        spans.push(Span::raw(format!("  Job: {}", truncate(&self.job_id, 20))));

        let block = Block::default().borders(Borders::ALL);
        let mut header_style = Style::default();
        if let Some((bg, fg)) = header_palette {
            header_style = header_style.bg(bg).fg(fg);
        }
        let header = Paragraph::new(Line::from(spans))
            .style(header_style)
            .block(block);

        frame.render_widget(header, area);
    }

    fn draw_panels(&self, frame: &mut Frame, area: Rect) {
        // Show result summary when toggled on and available
        if self.show_result
            && let Some(ref result) = self.final_result
        {
            self.draw_result(frame, area, result);
            return;
        }

        // 4-panel layout
        let cols = Layout::horizontal([
            Constraint::Percentage(30), // left (agents + budget)
            Constraint::Percentage(70), // right (proposals + evals)
        ])
        .split(area);

        let left = Layout::vertical([
            Constraint::Percentage(60), // agents
            Constraint::Percentage(40), // budget
        ])
        .split(cols[0]);

        let right = Layout::vertical([
            Constraint::Percentage(55), // proposals
            Constraint::Percentage(45), // evaluations
        ])
        .split(cols[1]);

        self.draw_agents_panel(frame, left[0]);
        self.draw_budget_panel(frame, left[1]);
        self.draw_proposals_panel(frame, right[0]);
        self.draw_evaluations_panel(frame, right[1]);
    }

    fn draw_agents_panel(&self, frame: &mut Frame, area: Rect) {
        let is_active = self.active_panel == Panel::Agents;
        let border_style = if is_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let agents = self.phase_tracker.all_agents();
        let rows: Vec<Row> = agents
            .iter()
            .map(|agent_id| {
                let status = self.phase_tracker.agent_status(agent_id);
                let icon_color = match status {
                    AgentPhaseStatus::Idle => Color::DarkGray,
                    AgentPhaseStatus::Proposing | AgentPhaseStatus::Evaluating => Color::Yellow,
                    AgentPhaseStatus::Proposed => Color::Green,
                    AgentPhaseStatus::Evaluated => Color::Cyan,
                };
                Row::new(vec![
                    Cell::from(Span::styled(status.icon(), Style::default().fg(icon_color))),
                    Cell::from(truncate(agent_id, 18)),
                    Cell::from(status.label()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(12),
            ],
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" Agents ({}) ", agents.len())),
        );

        frame.render_widget(table, area);
    }

    fn draw_budget_panel(&self, frame: &mut Frame, area: Rect) {
        let is_active = self.active_panel == Panel::Budget;
        let border_style = if is_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let lines: Vec<Line> = self
            .budget_phases
            .iter()
            .rev()
            .take(6) // show last 6 entries
            .rev()
            .map(|bp| {
                let budget_indicator = if bp.under_budget { "✓" } else { "!" };
                let indicator_color = if bp.under_budget {
                    Color::Green
                } else {
                    Color::Red
                };
                Line::from(vec![
                    Span::styled(budget_indicator, Style::default().fg(indicator_color)),
                    Span::raw(format!(
                        " R{} {}: {:.1}s / {:.1}s",
                        bp.round.map_or("?".to_string(), |r| r.to_string()),
                        bp.phase,
                        bp.actual_secs,
                        bp.budgeted_secs
                    )),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(" Budget "),
        );
        frame.render_widget(paragraph, area);
    }

    fn draw_proposals_panel(&self, frame: &mut Frame, area: Rect) {
        let is_active = self.active_panel == Panel::Proposals;
        let border_style = if is_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let current = self.current_proposals();
        let visible_height = area.height.saturating_sub(2) as usize; // minus borders
        let offset = self.proposal_scroll.scroll_offset;
        let rows: Vec<Row> = current
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_height)
            .map(|(i, p)| {
                let style = if is_active && i == self.proposal_scroll.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let score_str = p
                    .score
                    .map(|s| format!("{s:.2}"))
                    .unwrap_or_else(|| "—".into());

                let content = if self.show_thought_process {
                    format!("[{}] {}", p.thought_process, p.content)
                } else {
                    p.content.clone()
                };

                Row::new(vec![
                    Cell::from(truncate(&p.agent_id, 15)),
                    Cell::from(score_str),
                    Cell::from(truncate(&content, 60)),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(17),
                Constraint::Length(6),
                Constraint::Min(20),
            ],
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(
                    " Options R{} ({}) ",
                    self.viewed_round_value(),
                    current.len()
                )),
        );

        frame.render_widget(table, area);
    }

    fn draw_evaluations_panel(&self, frame: &mut Frame, area: Rect) {
        let is_active = self.active_panel == Panel::Evaluations;
        let border_style = if is_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let mut lines: Vec<Line> = Vec::new();
        let current_evals = self.current_evaluations();
        let mut sorted_targets: Vec<_> = current_evals.keys().collect();
        sorted_targets.sort();
        for (line_idx, target_id) in sorted_targets.iter().enumerate() {
            let evals = &current_evals[*target_id];
            let scores: Vec<String> = evals
                .iter()
                .map(|(evaluator, score, justification)| {
                    if self.show_justifications {
                        format!("{evaluator}:{score:.2} ({justification})")
                    } else {
                        format!("{evaluator}:{score:.2}")
                    }
                })
                .collect();
            let selected_style = if is_active && line_idx == self.eval_scroll.selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(truncate(target_id, 15), selected_style.fg(Color::Yellow)),
                Span::raw(" ← "),
                Span::styled(truncate(&scores.join("  "), 60), selected_style),
            ]));
        }

        let scroll_offset = self.eval_scroll.scroll_offset as u16;
        let paragraph = Paragraph::new(lines)
            .scroll((scroll_offset, 0))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(" Opinions "),
            );
        frame.render_widget(paragraph, area);
    }

    fn draw_result(&self, frame: &mut Frame, area: Rect, result: &JobCompleteData) {
        // Use the derived JobStatus for consistent display
        let (status_label, status_color) = match self.status {
            JobStatus::Complete => ("✨ Converged", Color::Green),
            JobStatus::Failed => ("✗ Failed", Color::Red),
            JobStatus::Disconnected => ("⚡ Disconnected", Color::Red),
            _ => ("● Running", Color::Yellow),
        };

        let lines = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                format!("  Status: {status_label} "),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("  Rounds completed: {}", result.rounds_completed)),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  💎 Crystallized option ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("by {} ", result.best_proposal_author),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("(score: {:.2})", result.best_proposal_score),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(""),
            Line::from(format!("  {}", result.best_proposal_content)),
        ];

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ✨ Quorum Convergence ✨ "),
        );
        frame.render_widget(paragraph, area);
    }

    /// Full-screen detail view for evaluations of a single target agent.
    fn draw_eval_detail(&self, frame: &mut Frame, area: Rect) {
        let target_id = match &self.eval_detail_target {
            Some(t) => t,
            None => return,
        };
        let all_evals = match self.evaluations.get(target_id) {
            Some(e) => e,
            None => return,
        };
        let target_round = self.viewed_round_value();
        let filtered_evals: Vec<_> = all_evals
            .iter()
            .filter(|(round, _, _, _)| *round == target_round)
            .collect();

        let mut lines: Vec<Line> = Vec::new();
        for (_, evaluator, score, justification) in &filtered_evals {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {evaluator} "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("score: {score:.2}"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            lines.push(Line::from(""));
            // Render the full justification text, preserving line breaks
            for text_line in justification.lines() {
                lines.push(Line::from(format!("    {text_line}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
        }

        let scroll = self.eval_detail_scroll as u16;
        let paragraph = Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(
                        " Opinions on {target_id} ({} voices) ",
                        filtered_evals.len()
                    )),
            );
        frame.render_widget(paragraph, area);
    }

    /// Full-screen detail view for the selected proposal.
    fn draw_proposal_detail(&self, frame: &mut Frame, area: Rect) {
        let current = self.current_proposals();
        let proposal = match current.get(self.proposal_scroll.selected) {
            Some(p) => p,
            None => return,
        };

        let score_str = proposal
            .score
            .map(|s| format!("{s:.2}"))
            .unwrap_or_else(|| "—".into());

        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("  Agent: {} ", proposal.agent_id),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("Round: {} ", proposal.round),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("Score: {score_str}"),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Line::from(""),
        ];

        // Thought process section
        if !proposal.thought_process.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Thought Process",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for text_line in proposal.thought_process.lines() {
                lines.push(Line::from(format!("    {text_line}")));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
        }

        // Option content section
        lines.push(Line::from(Span::styled(
            "  Option",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for text_line in proposal.content.lines() {
            lines.push(Line::from(format!("    {text_line}")));
        }

        // Show opinions on this option (viewed round only)
        if let Some(all_evals) = self.evaluations.get(&proposal.agent_id) {
            let target = self.viewed_round_value();
            let filtered: Vec<_> = all_evals
                .iter()
                .filter(|(round, _, _, _)| *round == target)
                .collect();
            if !filtered.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  Opinions Received ({})", filtered.len()),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
            }
            for (_, evaluator, eval_score, justification) in &filtered {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {evaluator}: "),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("{eval_score:.2}"),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
                for text_line in justification.lines() {
                    lines.push(Line::from(format!("      {text_line}")));
                }
                lines.push(Line::from(""));
            }
        }

        let scroll = self.proposal_detail_scroll as u16;
        let paragraph = Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(format!(" Proposal: {} ", proposal.agent_id)),
            );
        frame.render_widget(paragraph, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tui::event::{EvaluationEntry, ProposalScore};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn new_view() -> JobDetailView {
        JobDetailView::new("test-job-123".into(), "test-orch".into())
    }

    #[test]
    fn initial_state() {
        let view = new_view();
        assert_eq!(view.status, JobStatus::Connecting);
        assert_eq!(view.current_round, 0);
        assert_eq!(view.total_rounds, 0);
        assert!(view.proposals.is_empty());
        assert!(view.evaluations.is_empty());
        assert!(view.convergence_scores.is_empty());
        assert!(view.final_result.is_none());
    }

    #[test]
    fn on_enter_starts_stream() {
        let mut view = new_view();
        let actions = view.on_enter();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ViewAction::Fetch(FetchRequest::StartSseStream { job_id, .. }) if job_id == "test-job-123"
        ));
        assert!(view.stream_started);

        // Second call should not re-start
        let actions2 = view.on_enter();
        assert!(actions2.is_empty());
    }

    #[test]
    fn handle_connected() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::Connected);
        assert_eq!(view.status, JobStatus::Running);
    }

    #[test]
    fn handle_round_start() {
        let mut view = new_view();
        view.phase_tracker.proposing.insert("old".into());
        // Simulate open detail overlays from previous round.
        view.eval_detail_target = Some("prev-target".into());
        view.eval_detail_scroll = 5;
        view.proposal_detail_visible = true;
        view.proposal_detail_scroll = 3;

        view.handle_sse_event(&SseEvent::RoundStart {
            round: 2,
            total_rounds: 5,
        });

        assert_eq!(view.current_round, 2);
        assert_eq!(view.total_rounds, 5);
        assert!(view.phase_tracker.proposing.is_empty());
        // Detail overlays must be closed on round change.
        assert!(view.eval_detail_target.is_none());
        assert_eq!(view.eval_detail_scroll, 0);
        assert!(!view.proposal_detail_visible);
        assert_eq!(view.proposal_detail_scroll, 0);
    }

    #[test]
    fn handle_agent_working() {
        let mut view = new_view();

        view.handle_sse_event(&SseEvent::AgentWorking {
            agent_id: "a1".into(),
            action: "propose".into(),
        });
        assert!(view.phase_tracker.proposing.contains("a1"));

        view.handle_sse_event(&SseEvent::AgentWorking {
            agent_id: "a2".into(),
            action: "evaluate".into(),
        });
        assert!(view.phase_tracker.evaluating.contains("a2"));
    }

    #[test]
    fn handle_proposal_submitted() {
        let mut view = new_view();
        view.current_round = 1;
        view.handle_sse_event(&SseEvent::ProposalSubmitted {
            round: 1,
            agent_id: "agent-a".into(),
            content: "My proposal".into(),
            thought_process: "I thought about it".into(),
        });

        assert_eq!(view.proposals.len(), 1);
        assert_eq!(view.proposals[0].agent_id, "agent-a");
        assert_eq!(view.proposals[0].content, "My proposal");
        assert!(view.phase_tracker.proposed.contains("agent-a"));
        assert_eq!(view.proposal_scroll.count, 1);
    }

    #[test]
    fn handle_evaluation_submitted() {
        let mut view = new_view();
        view.current_round = 1;
        view.handle_sse_event(&SseEvent::EvaluationSubmitted {
            round: 1,
            evaluator_id: "eval-b".into(),
            evaluations: vec![
                EvaluationEntry {
                    target_id: "agent-a".into(),
                    score: 0.85,
                    justification: "Good work".into(),
                },
                EvaluationEntry {
                    target_id: "agent-c".into(),
                    score: 0.70,
                    justification: "Decent".into(),
                },
            ],
        });

        assert!(view.phase_tracker.evaluated.contains("eval-b"));
        assert_eq!(view.evaluations.len(), 2);
        assert_eq!(view.evaluations["agent-a"].len(), 1);
        assert_eq!(view.evaluations["agent-a"][0].2, 0.85);
        // Scroll count tracks number of targets, not total eval entries.
        assert_eq!(view.eval_scroll.count, 2);
    }

    #[test]
    fn handle_budget_phase_complete() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::BudgetPhaseComplete {
            round: Some(1),
            phase: "propose".into(),
            budgeted_secs: 30.0,
            actual_secs: 12.5,
            under_budget: true,
        });

        assert_eq!(view.budget_phases.len(), 1);
        assert!(view.budget_phases[0].under_budget);
    }

    #[test]
    fn handle_round_summary() {
        let mut view = new_view();
        view.proposals.push(ProposalEntry {
            round: 1,
            agent_id: "a1".into(),
            content: "test".into(),
            thought_process: "".into(),
            score: None,
        });

        view.handle_sse_event(&SseEvent::RoundSummary {
            round: 1,
            convergence_score: 0.85,
            proposal_scores: vec![ProposalScore {
                agent_id: "a1".into(),
                aggregated_score: 0.92,
            }],
        });

        assert_eq!(view.convergence_scores, vec![0.85]);
        assert_eq!(view.proposals[0].score, Some(0.92));
        assert_eq!(view.latest_convergence(), Some(0.85));
    }

    #[test]
    fn handle_job_complete_success() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "Success".into(),
            job_id: "j1".into(),
            rounds_completed: 3,
            best_proposal_content: "The answer".into(),
            best_proposal_score: 0.95,
            best_proposal_author: "agent-a".into(),
        });

        assert_eq!(view.status, JobStatus::Complete);
        let result = view.final_result.as_ref().unwrap();
        assert_eq!(result.best_proposal_content, "The answer");
        assert_eq!(result.best_proposal_score, 0.95);
    }

    #[test]
    fn handle_job_complete_failed() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "Failed".into(),
            job_id: "j1".into(),
            rounds_completed: 0,
            best_proposal_content: "".into(),
            best_proposal_score: 0.0,
            best_proposal_author: "".into(),
        });

        assert_eq!(view.status, JobStatus::Failed);
    }

    #[test]
    fn handle_timeout() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::Timeout("stream timeout".into()));
        assert_eq!(view.status, JobStatus::Failed);
    }

    #[test]
    fn timeout_after_complete_does_not_override() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "Success".into(),
            job_id: "j1".into(),
            rounds_completed: 1,
            best_proposal_content: "answer".into(),
            best_proposal_score: 0.9,
            best_proposal_author: "a1".into(),
        });
        assert_eq!(view.status, JobStatus::Complete);

        // SSE stream timeout arrives after job_complete (race condition)
        view.handle_sse_event(&SseEvent::Timeout("stream closed".into()));
        assert_eq!(view.status, JobStatus::Complete); // Must stay Complete
    }

    #[test]
    fn job_complete_case_insensitive() {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "success".into(), // lowercase
            job_id: "j1".into(),
            rounds_completed: 1,
            best_proposal_content: "ok".into(),
            best_proposal_score: 0.8,
            best_proposal_author: "a1".into(),
        });
        assert_eq!(view.status, JobStatus::Complete);
    }

    #[test]
    fn handle_unknown_event() {
        let mut view = new_view();
        let status_before = view.status;
        view.handle_sse_event(&SseEvent::Unknown {
            event_type: "future_event".into(),
        });
        assert_eq!(view.status, status_before);
    }

    #[test]
    fn tab_cycles_panels() {
        let mut view = new_view();
        assert_eq!(view.active_panel, Panel::Agents);

        view.update(&make_key(KeyCode::Tab));
        assert_eq!(view.active_panel, Panel::Proposals);

        view.update(&make_key(KeyCode::Tab));
        assert_eq!(view.active_panel, Panel::Evaluations);

        view.update(&make_key(KeyCode::Tab));
        assert_eq!(view.active_panel, Panel::Budget);

        view.update(&make_key(KeyCode::Tab));
        assert_eq!(view.active_panel, Panel::Agents);
    }

    #[test]
    fn t_toggles_thought_process() {
        let mut view = new_view();
        assert!(!view.show_thought_process);
        view.update(&make_key(KeyCode::Char('t')));
        assert!(view.show_thought_process);
        view.update(&make_key(KeyCode::Char('t')));
        assert!(!view.show_thought_process);
    }

    #[test]
    fn escape_pops() {
        let mut view = new_view();
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn current_proposals_filters_by_round() {
        let mut view = new_view();
        view.current_round = 2;
        view.proposals.push(ProposalEntry {
            round: 1,
            agent_id: "old".into(),
            content: "old".into(),
            thought_process: "".into(),
            score: None,
        });
        view.proposals.push(ProposalEntry {
            round: 2,
            agent_id: "current".into(),
            content: "current".into(),
            thought_process: "".into(),
            score: None,
        });

        let current = view.current_proposals();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].agent_id, "current");
    }

    #[test]
    fn full_deliberation_lifecycle() {
        let mut view = new_view();

        // Connected
        view.handle_sse_event(&SseEvent::Connected);
        assert_eq!(view.status, JobStatus::Running);

        // Round 1 start
        view.handle_sse_event(&SseEvent::RoundStart {
            round: 1,
            total_rounds: 3,
        });
        assert_eq!(view.current_round, 1);

        // Agents dispatched
        view.handle_sse_event(&SseEvent::AgentWorking {
            agent_id: "a1".into(),
            action: "propose".into(),
        });
        view.handle_sse_event(&SseEvent::AgentWorking {
            agent_id: "a2".into(),
            action: "propose".into(),
        });
        assert_eq!(view.phase_tracker.all_agents().len(), 2);

        // Proposals
        view.handle_sse_event(&SseEvent::ProposalSubmitted {
            round: 1,
            agent_id: "a1".into(),
            content: "proposal 1".into(),
            thought_process: "thinking 1".into(),
        });
        view.handle_sse_event(&SseEvent::ProposalSubmitted {
            round: 1,
            agent_id: "a2".into(),
            content: "proposal 2".into(),
            thought_process: "thinking 2".into(),
        });

        // Evaluations
        view.handle_sse_event(&SseEvent::AgentWorking {
            agent_id: "a1".into(),
            action: "evaluate".into(),
        });
        view.handle_sse_event(&SseEvent::EvaluationSubmitted {
            round: 1,
            evaluator_id: "a1".into(),
            evaluations: vec![EvaluationEntry {
                target_id: "a2".into(),
                score: 0.8,
                justification: "good".into(),
            }],
        });

        // Budget
        view.handle_sse_event(&SseEvent::BudgetPhaseComplete {
            round: Some(1),
            phase: "propose".into(),
            budgeted_secs: 30.0,
            actual_secs: 10.0,
            under_budget: true,
        });

        // Summary
        view.handle_sse_event(&SseEvent::RoundSummary {
            round: 1,
            convergence_score: 0.75,
            proposal_scores: vec![
                ProposalScore {
                    agent_id: "a1".into(),
                    aggregated_score: 0.8,
                },
                ProposalScore {
                    agent_id: "a2".into(),
                    aggregated_score: 0.85,
                },
            ],
        });
        assert_eq!(view.latest_convergence(), Some(0.75));

        // Complete
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "Success".into(),
            job_id: "j1".into(),
            rounds_completed: 1,
            best_proposal_content: "proposal 2".into(),
            best_proposal_score: 0.85,
            best_proposal_author: "a2".into(),
        });

        assert_eq!(view.status, JobStatus::Complete);
        assert!(view.final_result.is_some());
    }

    #[test]
    fn sse_data_event_dispatches_to_handler() {
        let mut view = new_view();
        let event = AppEvent::Data(DataEvent::SseEvent(SseEvent::Connected));
        view.update(&event);
        assert_eq!(view.status, JobStatus::Running);
    }

    #[test]
    fn i_activates_injection_input_when_running() {
        let mut view = new_view();
        view.status = JobStatus::Running;
        view.update(&make_key(KeyCode::Char('i')));
        assert!(view.inject_input_active);
    }

    #[test]
    fn i_ignored_when_not_running() {
        let mut view = new_view();
        assert_eq!(view.status, JobStatus::Connecting);
        view.update(&make_key(KeyCode::Char('i')));
        assert!(!view.inject_input_active);

        view.status = JobStatus::Complete;
        view.update(&make_key(KeyCode::Char('i')));
        assert!(!view.inject_input_active);
    }

    #[test]
    fn inject_input_escape_cancels() {
        let mut view = new_view();
        view.status = JobStatus::Running;
        view.inject_input_active = true;
        view.inject_text = "some text".into();

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none());
        assert!(!view.inject_input_active);
    }

    #[test]
    fn inject_input_enter_sends_message() {
        let mut view = new_view();
        view.status = JobStatus::Running;
        view.inject_input_active = true;
        view.inject_text = "Please reconsider".into();

        let action = view.update(&make_key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::InjectMessage {
                orchestrator: "test-orch".into(),
                job_id: "test-job-123".into(),
                message: "Please reconsider".into(),
            })
        );
        assert!(!view.inject_input_active);
        assert!(view.inject_text.is_empty());
    }

    #[test]
    fn inject_input_enter_rejected_when_job_complete() {
        let mut view = new_view();
        view.inject_input_active = true;
        view.inject_text = "Too late".into();
        view.status = JobStatus::Complete;

        let action = view.update(&make_key(KeyCode::Enter));
        assert!(action.is_none());
        assert!(!view.inject_input_active);
        assert!(view.inject_text.is_empty());
    }

    #[test]
    fn sse_stream_close_sets_disconnected() {
        let mut view = new_view();
        view.status = JobStatus::Running;

        let event = AppEvent::Data(DataEvent::FetchError {
            context: "sse_stream_closed".into(),
            error: "SSE stream ended unexpectedly".into(),
        });
        let action = view.update(&event);
        assert!(action.is_none());
        assert_eq!(view.status, JobStatus::Disconnected);
    }

    #[test]
    fn sse_stream_close_does_not_overwrite_complete() {
        let mut view = new_view();
        view.status = JobStatus::Complete;

        let event = AppEvent::Data(DataEvent::FetchError {
            context: "sse_stream_closed".into(),
            error: "SSE stream ended unexpectedly".into(),
        });
        view.update(&event);
        assert_eq!(view.status, JobStatus::Complete);
    }

    #[test]
    fn inject_input_empty_enter_ignored() {
        let mut view = new_view();
        view.inject_input_active = true;
        view.inject_text.clear();

        let action = view.update(&make_key(KeyCode::Enter));
        assert!(action.is_none());
        assert!(view.inject_input_active);
    }

    #[test]
    fn inject_input_typing() {
        let mut view = new_view();
        view.inject_input_active = true;

        view.update(&make_key(KeyCode::Char('H')));
        view.update(&make_key(KeyCode::Char('i')));
        assert_eq!(view.inject_text, "Hi");

        view.update(&make_key(KeyCode::Backspace));
        assert_eq!(view.inject_text, "H");
    }

    #[test]
    fn message_injected_increments_count() {
        let mut view = new_view();
        assert_eq!(view.injection_count, 0);

        let event = AppEvent::Data(DataEvent::MessageInjected {
            job_id: "test-job-123".into(),
            sequence: 1,
            round: 1,
        });
        view.update(&event);
        assert_eq!(view.injection_count, 1);

        view.update(&event);
        assert_eq!(view.injection_count, 2);
    }

    fn complete_view() -> JobDetailView {
        let mut view = new_view();
        view.handle_sse_event(&SseEvent::JobComplete {
            status: "Success".into(),
            job_id: "j1".into(),
            rounds_completed: 3,
            best_proposal_content: "answer".into(),
            best_proposal_score: 0.9,
            best_proposal_author: "a1".into(),
        });
        view
    }

    #[test]
    fn tab_on_result_switches_to_panels() {
        let mut view = complete_view();
        assert!(view.show_result);
        assert!(view.final_result.is_some());

        // First Tab: switch from result to panels view
        view.update(&make_key(KeyCode::Tab));
        assert!(!view.show_result);
        assert_eq!(view.active_panel, Panel::Agents);
    }

    #[test]
    fn tab_cycles_panels_then_back_to_result() {
        let mut view = complete_view();

        // Switch to panels
        view.update(&make_key(KeyCode::Tab));
        assert!(!view.show_result);
        assert_eq!(view.active_panel, Panel::Agents);

        // Cycle through all panels
        view.update(&make_key(KeyCode::Tab)); // → Proposals
        assert_eq!(view.active_panel, Panel::Proposals);
        view.update(&make_key(KeyCode::Tab)); // → Evaluations
        assert_eq!(view.active_panel, Panel::Evaluations);
        view.update(&make_key(KeyCode::Tab)); // → Budget
        assert_eq!(view.active_panel, Panel::Budget);

        // Wrap back to result view
        view.update(&make_key(KeyCode::Tab));
        assert!(view.show_result);
        assert_eq!(view.active_panel, Panel::Agents);
    }

    #[test]
    fn t_on_result_switches_to_proposals_panel() {
        let mut view = complete_view();
        assert!(view.show_result);
        assert!(!view.show_thought_process);

        view.update(&make_key(KeyCode::Char('t')));
        assert!(!view.show_result);
        assert!(view.show_thought_process);
        assert_eq!(view.active_panel, Panel::Proposals);
    }

    #[test]
    fn t_in_panels_toggles_without_switching_view() {
        let mut view = complete_view();
        view.show_result = false;
        view.active_panel = Panel::Proposals;

        view.update(&make_key(KeyCode::Char('t')));
        assert!(view.show_thought_process);
        assert!(!view.show_result); // stays in panels

        view.update(&make_key(KeyCode::Char('t')));
        assert!(!view.show_thought_process);
    }

    fn view_with_data() -> JobDetailView {
        let mut view = new_view();
        view.current_round = 1;
        view.handle_sse_event(&SseEvent::ProposalSubmitted {
            round: 1,
            agent_id: "agent-a".into(),
            content: "My detailed proposal".into(),
            thought_process: "I considered multiple angles".into(),
        });
        view.handle_sse_event(&SseEvent::EvaluationSubmitted {
            round: 1,
            evaluator_id: "agent-b".into(),
            evaluations: vec![EvaluationEntry {
                target_id: "agent-a".into(),
                score: 0.85,
                justification: "Well-reasoned approach with solid foundations".into(),
            }],
        });
        view
    }

    #[test]
    fn enter_on_evaluations_opens_detail() {
        let mut view = view_with_data();
        view.active_panel = Panel::Evaluations;

        view.update(&make_key(KeyCode::Enter));
        assert_eq!(view.eval_detail_target, Some("agent-a".into()));
        assert_eq!(view.eval_detail_scroll, 0);
    }

    #[test]
    fn d_on_evaluations_opens_detail() {
        let mut view = view_with_data();
        view.active_panel = Panel::Evaluations;

        view.update(&make_key(KeyCode::Char('d')));
        assert_eq!(view.eval_detail_target, Some("agent-a".into()));
    }

    #[test]
    fn escape_closes_eval_detail() {
        let mut view = view_with_data();
        view.eval_detail_target = Some("agent-a".into());
        view.eval_detail_scroll = 3;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT pop the view
        assert!(view.eval_detail_target.is_none());
        assert_eq!(view.eval_detail_scroll, 0);
    }

    #[test]
    fn scroll_in_eval_detail() {
        let mut view = view_with_data();
        view.eval_detail_target = Some("agent-a".into());

        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.eval_detail_scroll, 1);
        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.eval_detail_scroll, 2);
        view.update(&make_key(KeyCode::Up));
        assert_eq!(view.eval_detail_scroll, 1);
        view.update(&make_key(KeyCode::Up));
        assert_eq!(view.eval_detail_scroll, 0);
        // Can't go below 0
        view.update(&make_key(KeyCode::Up));
        assert_eq!(view.eval_detail_scroll, 0);
    }

    #[test]
    fn enter_on_proposals_opens_detail() {
        let mut view = view_with_data();
        view.active_panel = Panel::Proposals;

        view.update(&make_key(KeyCode::Enter));
        assert!(view.proposal_detail_visible);
        assert_eq!(view.proposal_detail_scroll, 0);
    }

    #[test]
    fn d_on_proposals_opens_detail() {
        let mut view = view_with_data();
        view.active_panel = Panel::Proposals;

        view.update(&make_key(KeyCode::Char('d')));
        assert!(view.proposal_detail_visible);
    }

    #[test]
    fn escape_closes_proposal_detail() {
        let mut view = view_with_data();
        view.proposal_detail_visible = true;
        view.proposal_detail_scroll = 5;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT pop
        assert!(!view.proposal_detail_visible);
        assert_eq!(view.proposal_detail_scroll, 0);
    }

    #[test]
    fn scroll_in_proposal_detail() {
        let mut view = view_with_data();
        view.proposal_detail_visible = true;

        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.proposal_detail_scroll, 1);
        view.update(&make_key(KeyCode::Up));
        assert_eq!(view.proposal_detail_scroll, 0);
    }

    #[test]
    fn enter_on_empty_evaluations_no_detail() {
        let mut view = new_view();
        view.active_panel = Panel::Evaluations;

        view.update(&make_key(KeyCode::Enter));
        assert!(view.eval_detail_target.is_none());
    }

    #[test]
    fn enter_on_empty_proposals_no_detail() {
        let mut view = new_view();
        view.active_panel = Panel::Proposals;

        view.update(&make_key(KeyCode::Enter));
        assert!(!view.proposal_detail_visible);
    }

    #[test]
    fn enter_on_agents_panel_does_not_open_detail() {
        let mut view = view_with_data();
        view.active_panel = Panel::Agents;

        view.update(&make_key(KeyCode::Enter));
        assert!(!view.proposal_detail_visible);
        assert!(view.eval_detail_target.is_none());
    }

    #[test]
    fn tab_ignored_in_eval_detail() {
        let mut view = view_with_data();
        view.eval_detail_target = Some("agent-a".into());
        let panel_before = view.active_panel;

        view.update(&make_key(KeyCode::Tab));
        // Tab is consumed by detail mode — panel doesn't change
        assert_eq!(view.active_panel, panel_before);
        assert!(view.eval_detail_target.is_some());
    }

    #[test]
    fn tab_ignored_in_proposal_detail() {
        let mut view = view_with_data();
        view.proposal_detail_visible = true;
        let panel_before = view.active_panel;

        view.update(&make_key(KeyCode::Tab));
        assert_eq!(view.active_panel, panel_before);
        assert!(view.proposal_detail_visible);
    }

    // ---- Round history navigation -----------------------------------

    fn view_at_round(round: u32) -> JobDetailView {
        let mut view = new_view();
        view.status = JobStatus::Running;
        view.current_round = round;
        view.total_rounds = round;
        view
    }

    #[test]
    fn defaults_to_following_live_round() {
        let view = view_at_round(2);
        assert!(view.viewed_round.is_none());
        assert_eq!(view.viewed_round_value(), 2);
        assert!(view.is_viewing_live());
    }

    #[test]
    fn page_up_pins_view_to_previous_round() {
        let mut view = view_at_round(3);
        view.update(&make_key(KeyCode::PageUp));
        assert_eq!(view.viewed_round, Some(2));
        assert!(!view.is_viewing_live());
    }

    #[test]
    fn page_up_clamps_at_round_one() {
        let mut view = view_at_round(1);
        view.update(&make_key(KeyCode::PageUp));
        // current=1, target = max(1.saturating_sub(1), 1) = 1.
        assert_eq!(view.viewed_round, Some(1));
    }

    #[test]
    fn page_down_returns_to_live_when_caught_up() {
        let mut view = view_at_round(3);
        view.viewed_round = Some(1);
        view.update(&make_key(KeyCode::PageDown));
        assert_eq!(view.viewed_round, Some(2));
        view.update(&make_key(KeyCode::PageDown));
        // next would be 3 = current_round → resets to None (live-follow).
        assert!(view.viewed_round.is_none());
        assert!(view.is_viewing_live());
    }

    #[test]
    fn equals_key_resets_to_live() {
        let mut view = view_at_round(5);
        view.viewed_round = Some(2);
        view.update(&make_key(KeyCode::Char('=')));
        assert!(view.viewed_round.is_none());
        assert!(view.is_viewing_live());
    }

    #[test]
    fn current_proposals_filter_by_viewed_round() {
        let mut view = view_at_round(3);
        view.proposals = vec![
            ProposalEntry {
                round: 1,
                agent_id: "A".into(),
                content: "R1".into(),
                thought_process: String::new(),
                score: None,
            },
            ProposalEntry {
                round: 2,
                agent_id: "A".into(),
                content: "R2".into(),
                thought_process: String::new(),
                score: None,
            },
            ProposalEntry {
                round: 3,
                agent_id: "A".into(),
                content: "R3".into(),
                thought_process: String::new(),
                score: None,
            },
        ];

        // Live-follow at round 3.
        assert_eq!(view.current_proposals().len(), 1);
        assert_eq!(view.current_proposals()[0].content, "R3");

        // Pin to round 1 → only the R1 proposal.
        view.viewed_round = Some(1);
        assert_eq!(view.current_proposals().len(), 1);
        assert_eq!(view.current_proposals()[0].content, "R1");
    }

    // ---- Chat-style input -------------------------------------------

    #[test]
    fn slash_activates_chat_input_when_running() {
        let mut view = view_at_round(1);
        assert!(!view.inject_input_active);
        view.update(&make_key(KeyCode::Char('/')));
        assert!(view.inject_input_active);
    }

    #[test]
    fn slash_ignored_when_not_running() {
        let mut view = new_view(); // status = Connecting
        view.update(&make_key(KeyCode::Char('/')));
        assert!(!view.inject_input_active);
    }

    #[test]
    fn legacy_i_still_activates_input() {
        let mut view = view_at_round(1);
        view.update(&make_key(KeyCode::Char('i')));
        assert!(view.inject_input_active);
    }

    // ---- Heart + color helpers --------------------------------------

    #[test]
    fn convergence_heart_low_score_is_broken() {
        assert_eq!(convergence_heart(0.0), "💔");
        assert_eq!(convergence_heart(0.29), "💔");
    }

    #[test]
    fn convergence_heart_high_score_is_sparkle() {
        assert_eq!(convergence_heart(0.96), "💖✨");
        assert_eq!(convergence_heart(1.0), "💖✨");
    }

    #[test]
    fn convergence_heart_monotonic_across_buckets() {
        // Pick one representative from each bucket; the emoji must
        // change as the score climbs through the thresholds.
        let buckets = [0.0, 0.3, 0.5, 0.7, 0.85, 0.95]
            .iter()
            .map(|&s| convergence_heart(s))
            .collect::<Vec<_>>();
        for window in buckets.windows(2) {
            assert_ne!(window[0], window[1], "buckets must differ: {buckets:?}");
        }
    }

    #[test]
    fn convergence_palette_endpoints() {
        // Red end — bright red bg + white text, must NOT be a Rgb color
        // (truecolor fallback was the original bug).
        let (bg, fg) = convergence_palette(0.0);
        assert_eq!(bg, Color::Red);
        assert_eq!(fg, Color::White);
        // Green end — bright green bg + black text.
        let (bg, fg) = convergence_palette(1.0);
        assert_eq!(bg, Color::Green);
        assert_eq!(fg, Color::Black);
    }

    #[test]
    fn convergence_palette_uses_indexed_colors_only() {
        // No Rgb anywhere — that's the whole point of this refactor.
        // Sample the full range; if any bucket sneaks in an Rgb the
        // truecolor-fallback bug returns.
        for s in [
            0.0, 0.1, 0.29, 0.3, 0.49, 0.5, 0.69, 0.7, 0.84, 0.85, 0.95, 1.0,
        ] {
            let (bg, fg) = convergence_palette(s);
            assert!(
                !matches!(bg, Color::Rgb(..)),
                "bg at s={s} is Rgb — terminals without truecolor render that as near-black"
            );
            assert!(
                !matches!(fg, Color::Rgb(..)),
                "fg at s={s} is Rgb — terminals without truecolor render that as near-black"
            );
        }
    }

    #[test]
    fn convergence_palette_buckets_progress_monotonically() {
        // Walk through buckets — every bucket boundary must yield a
        // different bg (no gaps where two adjacent scores share a palette).
        let probes = [0.0, 0.4, 0.6, 0.8, 1.0];
        let bgs: Vec<Color> = probes.iter().map(|&s| convergence_palette(s).0).collect();
        for window in bgs.windows(2) {
            assert_ne!(window[0], window[1], "palette buckets collapsed: {bgs:?}");
        }
    }

    fn make_paste(text: &str) -> AppEvent {
        AppEvent::Terminal(Event::Paste(text.into()))
    }

    fn make_ctrl(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn active_inject() -> JobDetailView {
        let mut v = new_view();
        v.status = JobStatus::Running;
        v.inject_input_active = true;
        v
    }

    #[test]
    fn inject_inserts_at_caret_not_only_at_end() {
        let mut v = active_inject();
        v.inject_text = "ac".into();
        v.inject_cursor = 1; // between a and c
        v.update(&make_key(KeyCode::Char('b')));
        assert_eq!(v.inject_text, "abc");
        assert_eq!(v.inject_cursor, 2);
    }

    #[test]
    fn inject_ctrl_arrows_jump_by_word() {
        let mut v = active_inject();
        v.inject_text = "hello world foo".into();
        v.inject_cursor = v.inject_text.len();
        v.update(&make_ctrl(KeyCode::Left));
        assert_eq!(&v.inject_text[v.inject_cursor..], "foo");
        v.update(&make_ctrl(KeyCode::Left));
        assert_eq!(&v.inject_text[v.inject_cursor..], "world foo");
        v.update(&make_ctrl(KeyCode::Right));
        assert_eq!(&v.inject_text[..v.inject_cursor], "hello world");
    }

    #[test]
    fn inject_home_end_and_backspace_at_caret() {
        let mut v = active_inject();
        v.inject_text = "abcd".into();
        v.inject_cursor = 4;
        v.update(&make_key(KeyCode::Home));
        assert_eq!(v.inject_cursor, 0);
        v.update(&make_key(KeyCode::End));
        assert_eq!(v.inject_cursor, 4);
        v.inject_cursor = 2; // between b and c
        v.update(&make_key(KeyCode::Backspace));
        assert_eq!(v.inject_text, "acd");
        assert_eq!(v.inject_cursor, 1);
    }

    #[test]
    fn inject_paste_inserts_at_caret() {
        let mut v = active_inject();
        v.inject_text = "ad".into();
        v.inject_cursor = 1;
        v.update(&make_paste("bc"));
        assert_eq!(v.inject_text, "abcd");
        assert_eq!(v.inject_cursor, 3);
    }

    #[test]
    fn inject_fullscreen_toggle_and_escape_layering() {
        let mut v = active_inject();
        v.update(&make_ctrl(KeyCode::Char('f')));
        assert!(v.inject_fullscreen);
        // Esc leaves full-screen but keeps the input open.
        v.update(&make_key(KeyCode::Esc));
        assert!(!v.inject_fullscreen);
        assert!(v.inject_input_active);
        // A second Esc closes the input.
        v.update(&make_key(KeyCode::Esc));
        assert!(!v.inject_input_active);
    }

    #[test]
    fn inject_send_resets_caret_and_fullscreen() {
        let mut v = active_inject();
        v.inject_text = "hi".into();
        v.inject_cursor = 2;
        v.inject_fullscreen = true;
        let action = v.update(&make_key(KeyCode::Enter));
        assert!(matches!(action, Some(ViewAction::InjectMessage { .. })));
        assert_eq!(v.inject_cursor, 0);
        assert!(!v.inject_fullscreen);
        assert!(v.inject_text.is_empty());
    }

    #[test]
    fn caret_text_places_block_on_the_cursor_line() {
        let t = caret_text("ab\ncd", 4); // 2nd line, before 'd'
        assert_eq!(t.lines.len(), 2, "one Line per source line");
        let l2 = &t.lines[1];
        let rendered: String = l2.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "cd");
        let caret = l2
            .spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .expect("a reversed caret block");
        assert_eq!(caret.content.as_ref(), "d");
    }

    #[test]
    fn ctrl_c_stops_the_running_deliberation() {
        let mut view = new_view();
        view.status = JobStatus::Running;
        let action = view.update(&make_ctrl(KeyCode::Char('c')));
        assert!(
            matches!(
                &action,
                Some(ViewAction::Fetch(FetchRequest::CancelJob { job_id, .. }))
                    if job_id == "test-job-123"
            ),
            "Ctrl-C in the detail view must cancel the job, got {action:?}"
        );
    }

    #[test]
    fn ctrl_c_ignored_when_not_running() {
        let mut view = new_view();
        view.status = JobStatus::Complete;
        assert!(view.update(&make_ctrl(KeyCode::Char('c'))).is_none());
    }

    #[test]
    fn plain_x_also_stops_the_deliberation() {
        // tmux-proof alias for ^C (some terminals never deliver Ctrl+C as a key).
        let mut view = new_view();
        view.status = JobStatus::Running;
        assert!(matches!(
            view.update(&make_key(KeyCode::Char('x'))),
            Some(ViewAction::Fetch(FetchRequest::CancelJob { .. }))
        ));
    }

    #[test]
    fn ctrl_c_stops_even_while_connecting() {
        // A mid-flight job the SSE stream hasn't advanced yet is still cancellable.
        let mut view = new_view();
        view.status = JobStatus::Connecting;
        assert!(matches!(
            view.update(&make_ctrl(KeyCode::Char('c'))),
            Some(ViewAction::Fetch(FetchRequest::CancelJob { .. }))
        ));
    }

    #[test]
    fn inject_input_accepts_bracketed_paste() {
        let mut view = new_view();
        view.status = JobStatus::Running;
        view.inject_input_active = true;
        // A bracketed paste arrives as one Event::Paste, not per-char Key events.
        let action = view.update(&make_paste("pasted steering text"));
        assert!(action.is_none());
        assert_eq!(view.inject_text, "pasted steering text");
        assert!(view.inject_input_active, "paste must not close the input");
    }

    #[test]
    fn inject_paste_normalizes_crlf() {
        let mut view = new_view();
        view.inject_input_active = true;
        view.update(&make_paste("line1\r\nline2\r"));
        assert_eq!(view.inject_text, "line1\nline2\n");
    }

    #[test]
    fn inject_paste_appends_to_typed_text() {
        let mut view = new_view();
        view.inject_input_active = true;
        view.inject_text = "typed ".into();
        view.inject_cursor = view.inject_text.len();
        view.update(&make_paste("pasted"));
        assert_eq!(view.inject_text, "typed pasted");
    }
}
