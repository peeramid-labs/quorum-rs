//! App-level `ask_user` overlay.
//!
//! A blocked agent's `ask_user` question must surface + be answerable on ANY screen — the
//! thread reader, the live job-detail (`^D`), a menu — not only the thread view. Views are
//! recreated when a sub-view is pushed onto the stack (their state is dropped), so the
//! pending-question queue can't live in a view; it lives here, above the stack, and the app
//! renders it over whatever view is current. This is the fix for questions vanishing while
//! the operator was in the detail screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::collections::VecDeque;

use crate::cli::tui::event::{self, AppEvent, DataEvent, SseEvent};
use crate::cli::tui::views::{FetchRequest, ViewAction};

/// One pending `ask_user` question awaiting the operator's answer.
#[derive(Clone, Debug)]
struct AskQuestion {
    job_id: String,
    call_id: String,
    question: String,
    options: Vec<String>,
    /// `0..options.len()` picks an option; `options.len()` is the "type your own" row.
    selected: usize,
    /// Free-text entry (auto-on when there are no options).
    typing: bool,
    answer: String,
}

impl AskQuestion {
    fn from_pending(
        job_id: String,
        call_id: String,
        arguments: &serde_json::Value,
    ) -> Option<Self> {
        let question = arguments.get("question")?.as_str()?.to_string();
        let options: Vec<String> = arguments
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let typing = options.is_empty();
        Some(Self {
            job_id,
            call_id,
            question,
            options,
            selected: 0,
            typing,
            answer: String::new(),
        })
    }
}

/// The app-level overlay: the pending-question queue plus the context (orchestrator, thread)
/// needed to answer/cancel. `Default` = empty + no context.
#[derive(Default)]
pub struct AskModal {
    queue: VecDeque<AskQuestion>,
    orchestrator: String,
    thread_id: Option<String>,
}

/// What the app loop should do after [`AskModal::drive`] handled one event.
pub enum ModalOutcome {
    /// The overlay wants nothing — pass the event to the current view.
    Idle,
    /// The overlay swallowed the event (a question fired/resolved, or a keystroke while a
    /// question shows) — don't dispatch it to the view.
    Consumed,
    /// The operator answered/cancelled — run this action, then don't dispatch to the view.
    Action(ViewAction),
}

impl AskModal {
    /// An empty overlay with no context yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the orchestrator (+ thread, for Ctrl-C cancel) an answer routes to. The app calls
    /// this whenever it opens or watches a deliberation, so a question that fires later is
    /// answerable. A `None` thread keeps any previously-set one.
    pub fn set_context(&mut self, orchestrator: String, thread_id: Option<String>) {
        if !orchestrator.is_empty() {
            self.orchestrator = orchestrator;
        }
        if thread_id.is_some() {
            self.thread_id = thread_id;
        }
    }

    /// A question is showing and must capture input + render over the current view.
    pub fn is_active(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Ingest a data event: enqueue a fired/recovered `ask_user` question, or drop a
    /// resolved one. Returns `true` when it was a question event (so the caller need not
    /// also dispatch it to the current view). Terminal (key) events go through [`Self::key`].
    pub fn ingest(&mut self, ev: &AppEvent) -> bool {
        match ev {
            AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallPending {
                job_id,
                call_id,
                arguments,
                ..
            })) => {
                if let Some(q) =
                    AskQuestion::from_pending(job_id.clone(), call_id.clone(), arguments)
                {
                    self.enqueue(q);
                }
                true
            }
            AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallResolved { call_id })) => {
                self.queue.retain(|q| &q.call_id != call_id);
                true
            }
            AppEvent::Data(DataEvent::ToolCallsLoaded { calls, .. }) => {
                for c in calls {
                    if let Some(q) =
                        AskQuestion::from_pending(c.job_id.clone(), c.call_id.clone(), &c.arguments)
                    {
                        self.enqueue(q);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Drop every pending question — the job they blocked on is over.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// Drive the overlay for one app event, returning what the caller should do. This keeps
    /// the app loop a single match: the overlay decides whether it swallowed the event
    /// ([`ModalOutcome::Consumed`]), produced an answer/cancel to run
    /// ([`ModalOutcome::Action`]), or wants nothing ([`ModalOutcome::Idle`], event flows to
    /// the view). A `JobComplete` clears stale questions but is NOT consumed — the view still
    /// needs it.
    pub fn drive(&mut self, ev: &AppEvent) -> ModalOutcome {
        if let AppEvent::Data(DataEvent::SseEvent(SseEvent::JobComplete { .. })) = ev {
            self.clear();
        }
        if self.ingest(ev) {
            return ModalOutcome::Consumed;
        }
        // While a question shows, keystrokes go to it (not the view); data events pass through.
        if self.is_active()
            && let AppEvent::Terminal(term) = ev
        {
            return match self.key(term) {
                Some(action) => ModalOutcome::Action(action),
                None => ModalOutcome::Consumed,
            };
        }
        ModalOutcome::Idle
    }

    fn enqueue(&mut self, q: AskQuestion) {
        if !self.queue.iter().any(|e| e.call_id == q.call_id) {
            self.queue.push_back(q);
        }
    }

    /// Keys while a question is showing: pick an option (↑↓ + Enter), type a free answer
    /// (Enter sends), Esc to skip (agent times out), Ctrl-C to cancel the whole job.
    /// Returns a fetch when answered/cancelled.
    pub fn key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        if event::is_escape(ev) {
            self.queue.pop_front(); // skip — the agent times out on its round budget
            return None;
        }
        // Ctrl-C cancels the blocked job (and drops all its questions). Only possible with a
        // known thread; otherwise fall through (Esc still skips).
        if event::is_ctrl(ev, 'c')
            && let Some(thread_id) = self.thread_id.clone()
        {
            let q = self.queue.front()?.clone();
            self.queue.clear();
            return Some(ViewAction::Fetch(FetchRequest::CancelJob {
                orchestrator: self.orchestrator.clone(),
                job_id: q.job_id,
                thread_id,
            }));
        }
        let q = self.queue.front_mut()?;
        let answer: Option<String> = if q.typing {
            type_answer_key(q, ev)
        } else {
            pick_option_key(q, ev)
        };
        let answer = answer?;
        let q = self.queue.pop_front()?; // reveal the next stacked one
        Some(ViewAction::Fetch(FetchRequest::RespondToolCall {
            orchestrator: self.orchestrator.clone(),
            job_id: q.job_id,
            call_id: q.call_id,
            result: answer,
        }))
    }

    /// Draw the modal centred over `area` (a `Clear` punches it out of the view beneath).
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let Some(q) = self.queue.front() else {
            return;
        };
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                "[agent] asks:".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for wl in q.question.lines() {
            lines.push(Line::from(wl.to_string()));
        }
        lines.push(Line::from(""));
        for (i, opt) in q.options.iter().enumerate() {
            let sel = !q.typing && q.selected == i;
            let style = if sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let prefix = if sel { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(format!("{prefix}{opt}"), style)));
        }
        if q.typing {
            lines.push(Line::from(Span::styled(
                format!("✎ {}", q.answer),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            let sel = q.selected == q.options.len();
            let style = if sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let prefix = if sel { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{prefix}✎ type your own…"),
                style,
            )));
        }
        lines.push(Line::from(""));
        let hint = if q.typing {
            "type · Enter send · Esc skip"
        } else {
            "↑↓ pick · Enter · Esc skip — time-boxed to the round"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
        let title = match self.queue.len() {
            0 | 1 => " (?) Question from the deliberation ".to_string(),
            n => format!(" (?) Question from the deliberation  (+{} more) ", n - 1),
        };
        let para = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(title),
            )
            .wrap(Wrap { trim: false });
        let popup = centered_rect(70, 60, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(para, popup);
    }
}

/// Free-text mode: type into the answer, `Enter` sends a non-empty answer.
fn type_answer_key(q: &mut AskQuestion, ev: &crossterm::event::Event) -> Option<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    let Event::Key(key) = ev else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            q.answer.push(c);
            None
        }
        KeyCode::Backspace => {
            q.answer.pop();
            None
        }
        KeyCode::Enter => {
            let a = q.answer.trim().to_string();
            (!a.is_empty()).then_some(a)
        }
        _ => None,
    }
}

/// Option-pick mode: ↑↓ move the cursor, `Enter` picks an option or drops into the
/// "type your own" row.
fn pick_option_key(q: &mut AskQuestion, ev: &crossterm::event::Event) -> Option<String> {
    let n_opts = q.options.len();
    if event::is_up(ev) {
        q.selected = q.selected.saturating_sub(1);
    } else if event::is_down(ev) {
        if q.selected < n_opts {
            q.selected += 1;
        }
    } else if event::is_enter(ev) {
        if q.selected < n_opts {
            return Some(q.options[q.selected].clone());
        }
        q.typing = true; // the "type your own" row
    }
    None
}

/// A rectangle `pct_x`% × `pct_y`% of `area`, centred.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::PendingToolCall;

    fn pending_event(call_id: &str, question: &str, options: &[&str]) -> AppEvent {
        AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallPending {
            job_id: "job1".into(),
            call_id: call_id.into(),
            agent_id: "BDBot".into(),
            arguments: serde_json::json!({ "question": question, "options": options }),
            round: 1,
        }))
    }

    fn press(c: char) -> crossterm::event::Event {
        use crossterm::event::{Event, KeyCode, KeyEvent};
        Event::Key(KeyEvent::from(KeyCode::Char(c)))
    }
    fn key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::from(code))
    }

    #[test]
    fn ingest_activates_and_resolve_clears() {
        let mut m = AskModal::new();
        assert!(!m.is_active());
        assert!(m.ingest(&pending_event("c1", "Which env?", &["dev", "prod"])));
        assert!(
            m.is_active(),
            "a pending ask_user question activates the overlay"
        );
        // A non-question event is not consumed.
        assert!(
            !m.ingest(&AppEvent::Data(DataEvent::SseEvent(SseEvent::Timeout(
                "x".into()
            ))))
        );
        // Resolve clears it.
        assert!(m.ingest(&AppEvent::Data(DataEvent::SseEvent(
            SseEvent::ToolCallResolved {
                call_id: "c1".into()
            }
        ))));
        assert!(!m.is_active(), "resolving the call clears the overlay");
    }

    #[test]
    fn recovered_calls_are_deduped() {
        let mut m = AskModal::new();
        m.ingest(&pending_event("c1", "Q?", &[]));
        // The on-reopen recovery fetch delivers the same call — must not double-stack.
        let recovered: PendingToolCall = serde_json::from_value(serde_json::json!({
            "call_id": "c1", "job_id": "job1", "agent_id": "BDBot",
            "tool_name": "user_ask_user", "arguments": { "question": "Q?" },
            "round": 1, "phase": "Proposing", "status": "Pending", "created_at": 0,
        }))
        .unwrap();
        m.ingest(&AppEvent::Data(DataEvent::ToolCallsLoaded {
            thread_id: "t1".into(),
            calls: vec![recovered],
        }));
        assert!(m.is_active());
        m.ingest(&AppEvent::Data(DataEvent::SseEvent(
            SseEvent::ToolCallResolved {
                call_id: "c1".into(),
            },
        )));
        assert!(!m.is_active(), "one dedup'd call → one resolve clears it");
    }

    #[test]
    fn answering_an_option_emits_respond_with_context() {
        let mut m = AskModal::new();
        m.set_context("orch-a".into(), Some("t1".into()));
        m.ingest(&pending_event("c1", "Which env?", &["dev", "prod"]));
        // ↓ to "prod", Enter → RespondToolCall.
        assert!(m.key(&key(crossterm::event::KeyCode::Down)).is_none());
        let action = m
            .key(&key(crossterm::event::KeyCode::Enter))
            .expect("answer emits an action");
        match action {
            ViewAction::Fetch(FetchRequest::RespondToolCall {
                orchestrator,
                job_id,
                call_id,
                result,
            }) => {
                assert_eq!(orchestrator, "orch-a");
                assert_eq!(job_id, "job1");
                assert_eq!(call_id, "c1");
                assert_eq!(result, "prod");
            }
            other => panic!("expected RespondToolCall, got {other:?}"),
        }
        assert!(!m.is_active(), "answering pops the question");
    }

    #[test]
    fn drive_consumes_questions_actions_and_passes_the_rest() {
        let mut m = AskModal::new();
        m.set_context("orch".into(), Some("t1".into()));
        // A non-question event with no active question → Idle (view handles it).
        assert!(matches!(
            m.drive(&AppEvent::Data(DataEvent::SseEvent(SseEvent::Timeout(
                "x".into()
            )))),
            ModalOutcome::Idle
        ));
        // A question event → Consumed.
        assert!(matches!(
            m.drive(&pending_event("c1", "Q?", &[])),
            ModalOutcome::Consumed
        ));
        // A keystroke while active but not an answer (a char, typing mode) → Consumed.
        assert!(matches!(
            m.drive(&AppEvent::Terminal(press('h'))),
            ModalOutcome::Consumed
        ));
        // Enter → an Action to run, then the queue empties.
        assert!(matches!(
            m.drive(&AppEvent::Terminal(key(crossterm::event::KeyCode::Enter))),
            ModalOutcome::Action(_)
        ));
        assert!(!m.is_active());
    }

    #[test]
    fn typed_free_answer_sends_on_enter() {
        let mut m = AskModal::new();
        m.set_context("orch-a".into(), None);
        m.ingest(&pending_event("c1", "Name?", &[])); // no options → typing
        for c in "hi".chars() {
            assert!(m.key(&press(c)).is_none());
        }
        let action = m
            .key(&key(crossterm::event::KeyCode::Enter))
            .expect("typed answer sends");
        assert!(matches!(
            action,
            ViewAction::Fetch(FetchRequest::RespondToolCall { result, .. }) if result == "hi"
        ));
    }
}
