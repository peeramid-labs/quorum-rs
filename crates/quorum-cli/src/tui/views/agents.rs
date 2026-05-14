use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use super::common::{ListState, render_error, render_key_hints, render_loading, truncate};
use super::{FetchRequest, View, ViewAction};
use crate::remote::AgentInfo;
use crate::tui::event::{self, AppEvent, DataEvent};

/// Loading state for async data.
#[derive(Debug, Clone)]
pub enum LoadState<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Error(String),
}

/// Agents list view with inline detail panel.
pub struct AgentsView {
    /// Which orchestrator to fetch agents from.
    orchestrator: String,
    agents: LoadState<Vec<AgentInfo>>,
    list_state: ListState,
    /// When set, shows a detail panel for the selected agent.
    detail_visible: bool,
}

impl AgentsView {
    pub fn new(orchestrator: String) -> Self {
        Self {
            orchestrator,
            agents: LoadState::NotLoaded,
            list_state: ListState::new(0),
            detail_visible: false,
        }
    }

    fn agent_count(&self) -> usize {
        match &self.agents {
            LoadState::Loaded(agents) => agents.len(),
            _ => 0,
        }
    }

    /// Get the currently selected agent, if any.
    fn selected_agent(&self) -> Option<&AgentInfo> {
        match &self.agents {
            LoadState::Loaded(agents) => agents.get(self.list_state.selected),
            _ => None,
        }
    }
}

impl View for AgentsView {
    fn on_enter(&mut self) -> Vec<ViewAction> {
        self.agents = LoadState::Loading;
        vec![ViewAction::Fetch(FetchRequest::Agents {
            orchestrator: self.orchestrator.clone(),
        })]
    }

    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        match app_event {
            AppEvent::Terminal(event) => {
                // In detail mode, Esc closes detail (not pop)
                if self.detail_visible {
                    if event::is_escape(event) || event::is_key(event, 'q') {
                        self.detail_visible = false;
                        return None;
                    }
                    // Allow navigation even in detail mode
                    if event::is_up(event) {
                        self.list_state.up();
                    }
                    if event::is_down(event) {
                        self.list_state.down();
                    }
                    return None;
                }

                if event::is_escape(event) || event::is_key(event, 'q') {
                    return Some(ViewAction::Pop);
                }
                if event::is_up(event) {
                    self.list_state.up();
                }
                if event::is_down(event) {
                    self.list_state.down();
                }
                // d = detail panel
                if event::is_key(event, 'd') && self.selected_agent().is_some() {
                    self.detail_visible = true;
                    return None;
                }
                if event::is_key(event, 'r') {
                    self.agents = LoadState::Loading;
                    self.detail_visible = false;
                    return Some(ViewAction::Fetch(FetchRequest::Agents {
                        orchestrator: self.orchestrator.clone(),
                    }));
                }
                None
            }
            AppEvent::Data(DataEvent::AgentsLoaded {
                orchestrator,
                agents,
            }) if *orchestrator == self.orchestrator => {
                self.list_state.set_count(agents.len());
                self.agents = LoadState::Loaded(agents.clone());
                None
            }
            AppEvent::Data(DataEvent::FetchError { context, error })
                if context.contains("agents") =>
            {
                self.agents = LoadState::Error(error.clone());
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(0),    // table (+ detail)
            Constraint::Length(1), // hints
        ])
        .split(area);

        let visible_height = chunks[0].height.saturating_sub(3) as usize;
        self.list_state.set_visible_height(visible_height);

        match &self.agents {
            LoadState::NotLoaded | LoadState::Loading => {
                render_loading(frame, chunks[0], "Loading agents...");
            }
            LoadState::Error(e) => {
                render_error(frame, chunks[0], e);
            }
            LoadState::Loaded(agents) => {
                if agents.is_empty() {
                    render_error(frame, chunks[0], "No agents registered");
                } else if self.detail_visible {
                    // Split: table on left, detail on right
                    let h_chunks = Layout::horizontal([
                        Constraint::Percentage(45),
                        Constraint::Percentage(55),
                    ])
                    .split(chunks[0]);
                    self.draw_table(frame, h_chunks[0], agents);
                    if let Some(agent) = self.selected_agent() {
                        draw_agent_detail(frame, h_chunks[1], agent);
                    }
                } else {
                    self.draw_table(frame, chunks[0], agents);
                }
            }
        }

        let hints = if self.detail_visible {
            vec![
                ("↑↓", "Navigate"),
                ("r", "Refresh"),
                ("Esc", "Close detail"),
            ]
        } else {
            vec![
                ("↑↓", "Navigate"),
                ("d", "Detail"),
                ("r", "Refresh"),
                ("Esc", "Back"),
            ]
        };
        render_key_hints(frame, chunks[1], &hints);
    }
}

impl AgentsView {
    fn draw_table(&self, frame: &mut Frame, area: Rect, agents: &[AgentInfo]) {
        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("Agent"),
            Cell::from("Operator"),
            Cell::from("Model"),
            Cell::from("Provider"),
            Cell::from("Cost/Round"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        let visible = area.height.saturating_sub(3) as usize;
        let rows: Vec<Row> = agents
            .iter()
            .enumerate()
            .skip(self.list_state.scroll_offset)
            .take(visible.max(1))
            .map(|(i, agent)| {
                let style = if i == self.list_state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let status_icon = if agent.is_online { "✓" } else { "✗" };
                let status_color = if agent.is_online {
                    Color::Green
                } else {
                    Color::Red
                };

                let cost = agent
                    .estimated_cost_per_round
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "—".into());

                let operator = agent
                    .operator_display_name
                    .as_deref()
                    .or(agent.operator.as_deref())
                    .unwrap_or("—");

                Row::new(vec![
                    Cell::from(Span::styled(status_icon, Style::default().fg(status_color))),
                    Cell::from(truncate(&agent.agent_id, 20)),
                    Cell::from(truncate(operator, 18)),
                    Cell::from(truncate(&agent.model_name, 18)),
                    Cell::from(truncate(&agent.provider_id, 12)),
                    Cell::from(cost),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(22),
                Constraint::Length(20),
                Constraint::Length(20),
                Constraint::Length(14),
                Constraint::Min(10),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Agents ({}) ", self.agent_count())),
        );

        frame.render_widget(table, area);
    }
}

/// Render a detail panel for a single agent.
fn draw_agent_detail(frame: &mut Frame, area: Rect, agent: &AgentInfo) {
    let status = if agent.is_online {
        "● Online"
    } else {
        "○ Offline"
    };
    let status_color = if agent.is_online {
        Color::Green
    } else {
        Color::Red
    };

    let operator = agent
        .operator_display_name
        .as_deref()
        .or(agent.operator.as_deref())
        .unwrap_or("—");

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Cyan)),
            Span::styled(status, Style::default().fg(status_color)),
        ]),
        Line::from(vec![
            Span::styled("Operator: ", Style::default().fg(Color::Cyan)),
            Span::raw(operator),
        ]),
        Line::from(vec![
            Span::styled("Model: ", Style::default().fg(Color::Cyan)),
            Span::raw(&agent.model_name),
        ]),
        Line::from(vec![
            Span::styled("Provider: ", Style::default().fg(Color::Cyan)),
            Span::raw(&agent.provider_id),
        ]),
    ];

    if let Some(desc) = &agent.description {
        lines.push(Line::from(vec![
            Span::styled("Description: ", Style::default().fg(Color::Cyan)),
            Span::raw(desc),
        ]));
    }

    if let Some(job) = &agent.current_job {
        lines.push(Line::from(vec![
            Span::styled("Current Job: ", Style::default().fg(Color::Cyan)),
            Span::raw(job),
        ]));
    }

    // Pricing
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Pricing",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));

    let cost_str = agent
        .estimated_cost_per_round
        .map(|c| format!("${c:.4}"))
        .unwrap_or_else(|| "—".into());
    lines.push(Line::from(vec![
        Span::styled("  Cost/Round: ", Style::default().fg(Color::Cyan)),
        Span::raw(cost_str),
    ]));

    if let Some(input) = agent.input_price_per_mtok {
        lines.push(Line::from(vec![
            Span::styled("  Input $/MTok: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("${input:.2}")),
        ]));
    }
    if let Some(output) = agent.output_price_per_mtok {
        lines.push(Line::from(vec![
            Span::styled("  Output $/MTok: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("${output:.2}")),
        ]));
    }

    // Capabilities
    if !agent.capability_tags.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Capabilities",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled("  Tags: ", Style::default().fg(Color::Cyan)),
            Span::raw(agent.capability_tags.join(", ")),
        ]));
    }

    // Uptime / last seen
    if agent.uptime_secs.is_some() || agent.last_seen.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Activity",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(uptime) = agent.uptime_secs {
            let hours = uptime / 3600;
            let mins = (uptime % 3600) / 60;
            lines.push(Line::from(vec![
                Span::styled("  Uptime: ", Style::default().fg(Color::Cyan)),
                Span::raw(format!("{hours}h {mins}m")),
            ]));
        }
        if let Some(last) = &agent.last_seen {
            lines.push(Line::from(vec![
                Span::styled("  Last Seen: ", Style::default().fg(Color::Cyan)),
                Span::raw(last),
            ]));
        }
    }

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", agent.agent_id)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(detail, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn sample_agents() -> Vec<AgentInfo> {
        vec![
            AgentInfo {
                agent_id: "agent-a".into(),
                model_name: "gpt-4".into(),
                provider_id: "openai".into(),
                is_online: true,
                estimated_cost_per_round: Some(0.05),
                capability_tags: vec!["analysis".into()],
                operator: Some("acme".into()),
                operator_display_name: Some("Acme Corp".into()),
                description: Some("Code reviewer".into()),
                current_job: None,
                input_price_per_mtok: Some(2.50),
                output_price_per_mtok: Some(10.0),
                uptime_secs: Some(3661),
                last_seen: Some("2024-01-15T10:30:00Z".into()),
            },
            AgentInfo {
                agent_id: "agent-b".into(),
                model_name: "claude-3".into(),
                provider_id: "anthropic".into(),
                is_online: false,
                estimated_cost_per_round: None,
                capability_tags: vec![],
                operator: None,
                operator_display_name: None,
                description: None,
                current_job: None,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                uptime_secs: None,
                last_seen: None,
            },
        ]
    }

    #[test]
    fn on_enter_triggers_fetch() {
        let mut view = AgentsView::new("test-orch".into());
        let actions = view.on_enter();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ViewAction::Fetch(FetchRequest::Agents { orchestrator }) if orchestrator == "test-orch"
        ));
        assert!(matches!(view.agents, LoadState::Loading));
    }

    #[test]
    fn agents_loaded_updates_state() {
        let mut view = AgentsView::new("orch".into());
        let agents = sample_agents();
        let event = AppEvent::Data(DataEvent::AgentsLoaded {
            orchestrator: "orch".into(),
            agents: agents.clone(),
        });
        view.update(&event);
        assert!(matches!(view.agents, LoadState::Loaded(_)));
        assert_eq!(view.list_state.count, 2);
    }

    #[test]
    fn agents_from_different_orchestrator_ignored() {
        let mut view = AgentsView::new("orch".into());
        let event = AppEvent::Data(DataEvent::AgentsLoaded {
            orchestrator: "other".into(),
            agents: sample_agents(),
        });
        view.update(&event);
        assert!(matches!(view.agents, LoadState::NotLoaded));
    }

    #[test]
    fn escape_pops() {
        let mut view = AgentsView::new("orch".into());
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn d_opens_detail_panel() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(sample_agents());
        view.list_state.set_count(2);

        let action = view.update(&make_key(KeyCode::Char('d')));
        assert!(action.is_none()); // No ViewAction push
        assert!(view.detail_visible);
    }

    #[test]
    fn escape_in_detail_closes_detail() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(sample_agents());
        view.list_state.set_count(2);
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT pop
        assert!(!view.detail_visible);
    }

    #[test]
    fn navigation_works_in_detail_mode() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(sample_agents());
        view.list_state.set_count(2);
        view.detail_visible = true;

        assert_eq!(view.list_state.selected, 0);
        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.list_state.selected, 1);
        assert!(view.detail_visible); // Still in detail mode
    }

    #[test]
    fn r_refreshes() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(sample_agents());

        let action = view.update(&make_key(KeyCode::Char('r')));
        assert!(matches!(
            action,
            Some(ViewAction::Fetch(FetchRequest::Agents { .. }))
        ));
        assert!(matches!(view.agents, LoadState::Loading));
    }

    #[test]
    fn fetch_error_transitions_to_error_state() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loading;

        let event = AppEvent::Data(DataEvent::FetchError {
            context: "agents".into(),
            error: "orchestrator 'orch' has empty token".into(),
        });
        let action = view.update(&event);
        assert!(action.is_none());
        assert!(matches!(view.agents, LoadState::Error(ref e) if e.contains("empty token")));
    }

    #[test]
    fn selected_agent_returns_correct_agent() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(sample_agents());
        view.list_state.set_count(2);

        assert_eq!(view.selected_agent().unwrap().agent_id, "agent-a");
        assert_eq!(
            view.selected_agent()
                .unwrap()
                .operator_display_name
                .as_deref(),
            Some("Acme Corp")
        );

        view.list_state.selected = 1;
        assert_eq!(view.selected_agent().unwrap().agent_id, "agent-b");
        assert!(view.selected_agent().unwrap().operator.is_none());
    }

    #[test]
    fn d_on_empty_does_nothing() {
        let mut view = AgentsView::new("orch".into());
        view.agents = LoadState::Loaded(vec![]);
        view.list_state.set_count(0);

        let action = view.update(&make_key(KeyCode::Char('d')));
        assert!(action.is_none());
        assert!(!view.detail_visible);
    }
}
