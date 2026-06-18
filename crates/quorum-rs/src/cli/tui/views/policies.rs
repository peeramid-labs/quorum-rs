use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use super::common::{ListState, render_error, render_key_hints, render_loading, truncate};
use super::{ConfigMutation, FetchRequest, View, ViewAction};
use crate::cli::tui::event::{self, AppEvent, DataEvent, PolicyInfo};
use crate::cli::tui::views::agents::LoadState;
use crate::cli::workspace::PolicyConfig;

/// Policies list view with tag filtering and inline detail panel.
pub struct PoliciesView {
    orchestrator: String,
    /// Policies defined in the local workspace (nsed.yaml), shown as a
    /// separate read-only section above the remote (orchestrator) list.
    local_policies: Vec<(String, PolicyConfig)>,
    policies: LoadState<Vec<PolicyInfo>>,
    list_state: ListState,
    filter_active: bool,
    filter_text: String,
    /// When true, shows a detail panel for the selected policy.
    detail_visible: bool,
}

impl PoliciesView {
    /// Build the policies view for `orchestrator`, with the workspace's local
    /// (nsed.yaml) policies shown as a read-only section above the remote list.
    pub fn new(orchestrator: String, local_policies: HashMap<String, PolicyConfig>) -> Self {
        let mut local: Vec<(String, PolicyConfig)> = local_policies.into_iter().collect();
        local.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            orchestrator,
            local_policies: local,
            policies: LoadState::NotLoaded,
            list_state: ListState::new(0),
            filter_active: false,
            filter_text: String::new(),
            detail_visible: false,
        }
    }

    fn filtered_policies(&self) -> Vec<&PolicyInfo> {
        match &self.policies {
            LoadState::Loaded(policies) => {
                if self.filter_text.is_empty() {
                    policies.iter().collect()
                } else {
                    let filter = self.filter_text.to_lowercase();
                    policies
                        .iter()
                        .filter(|p| {
                            p.tags.iter().any(|t| t.to_lowercase().contains(&filter))
                                || p.name.to_lowercase().contains(&filter)
                        })
                        .collect()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Get the currently selected policy from the filtered list.
    fn selected_policy(&self) -> Option<&PolicyInfo> {
        self.filtered_policies()
            .get(self.list_state.selected)
            .copied()
    }
}

impl View for PoliciesView {
    fn captures_input(&self) -> bool {
        self.filter_active
    }

    fn on_enter(&mut self) -> Vec<ViewAction> {
        self.policies = LoadState::Loading;
        vec![ViewAction::Fetch(FetchRequest::Policies {
            orchestrator: self.orchestrator.clone(),
            tag: None,
        })]
    }

    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        match app_event {
            AppEvent::Terminal(event) => {
                if self.filter_active {
                    return self.update_filter(event);
                }

                // In detail mode, Esc closes detail
                if self.detail_visible {
                    if event::is_escape(event) || event::is_key(event, 'q') {
                        self.detail_visible = false;
                        return None;
                    }
                    if event::is_up(event) {
                        self.list_state.up();
                    }
                    if event::is_down(event) {
                        self.list_state.down();
                    }
                    // Enter = create room from detail mode
                    if event::is_enter(event)
                        && let Some(policy) = self.selected_policy().cloned()
                    {
                        self.detail_visible = false;
                        return Some(ViewAction::WriteConfig(ConfigMutation::AddRoom {
                            name: format!("{}-room", policy.name),
                            policy: policy.policy_id,
                            orchestrator: self.orchestrator.clone(),
                        }));
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
                if event::is_key(event, '/') {
                    self.filter_active = true;
                    return None;
                }
                // Enter = main CTA: create room from policy
                if event::is_enter(event) {
                    let filtered = self.filtered_policies();
                    if let Some(policy) = filtered.get(self.list_state.selected) {
                        return Some(ViewAction::WriteConfig(ConfigMutation::AddRoom {
                            name: format!("{}-room", policy.name),
                            policy: policy.policy_id.clone(),
                            orchestrator: self.orchestrator.clone(),
                        }));
                    }
                }
                // d = detail panel
                if event::is_key(event, 'd') && self.selected_policy().is_some() {
                    self.detail_visible = true;
                    return None;
                }
                if event::is_key(event, 'r') {
                    self.policies = LoadState::Loading;
                    self.detail_visible = false;
                    return Some(ViewAction::Fetch(FetchRequest::Policies {
                        orchestrator: self.orchestrator.clone(),
                        tag: None,
                    }));
                }
                None
            }
            AppEvent::Data(DataEvent::PoliciesLoaded {
                orchestrator,
                policies,
            }) if *orchestrator == self.orchestrator => {
                self.list_state.set_count(policies.len());
                self.policies = LoadState::Loaded(policies.clone());
                None
            }
            AppEvent::Data(DataEvent::FetchError { context, error })
                if context.contains("policies") =>
            {
                self.policies = LoadState::Error(error.clone());
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(if self.filter_active { 3 } else { 0 }),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        // Filter input
        if self.filter_active {
            let input = Paragraph::new(format!("/{}", self.filter_text))
                .style(Style::default().fg(Color::Yellow))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Filter by tag "),
                );
            frame.render_widget(input, chunks[0]);
        }

        // Carve a top band for the local (nsed.yaml) section when present;
        // the remote (orchestrator) list takes the rest.
        let remote_area = if self.local_policies.is_empty() {
            chunks[1]
        } else {
            let local_h = ((self.local_policies.len() as u16 + 3).min(chunks[1].height / 2)).max(4);
            let split = Layout::vertical([Constraint::Length(local_h), Constraint::Min(0)])
                .split(chunks[1]);
            self.draw_local_section(frame, split[0]);
            split[1]
        };

        // Policy list + optional detail (remote / orchestrator)
        match &self.policies {
            LoadState::NotLoaded | LoadState::Loading => {
                render_loading(frame, remote_area, "Loading policies...");
            }
            LoadState::Error(e) => {
                render_error(frame, remote_area, e);
            }
            LoadState::Loaded(_) => {
                let visible_height = remote_area.height.saturating_sub(3) as usize;
                self.list_state.set_visible_height(visible_height);
                let filtered = self.filtered_policies();
                if filtered.is_empty() {
                    render_error(frame, remote_area, "No remote policies found");
                } else if self.detail_visible {
                    let h_chunks = Layout::horizontal([
                        Constraint::Percentage(45),
                        Constraint::Percentage(55),
                    ])
                    .split(remote_area);
                    self.draw_table(frame, h_chunks[0], &filtered);
                    if let Some(policy) = self.selected_policy() {
                        draw_policy_detail(frame, h_chunks[1], policy);
                    }
                } else {
                    self.draw_table(frame, remote_area, &filtered);
                }
            }
        }

        let hints = if self.filter_active {
            vec![("Enter/Esc", "Close filter"), ("Type", "Filter")]
        } else if self.detail_visible {
            vec![
                ("↑↓", "Navigate"),
                ("Enter", "Create Room"),
                ("Esc", "Close detail"),
            ]
        } else {
            vec![
                ("↑↓", "Navigate"),
                ("Enter", "Create Room"),
                ("d", "Detail"),
                ("/", "Filter"),
                ("r", "Refresh"),
                ("Esc", "Back"),
            ]
        };
        render_key_hints(frame, chunks[2], &hints);
    }
}

impl PoliciesView {
    fn update_filter(&mut self, event: &crossterm::event::Event) -> Option<ViewAction> {
        if event::is_escape(event) || event::is_enter(event) {
            self.filter_active = false;
            let count = self.filtered_policies().len();
            self.list_state.set_count(count);
            return None;
        }
        if let crossterm::event::Event::Key(key) = event
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            match key.code {
                crossterm::event::KeyCode::Char(c) => {
                    self.filter_text.push(c);
                }
                crossterm::event::KeyCode::Backspace => {
                    self.filter_text.pop();
                }
                _ => {}
            }
            let count = self.filtered_policies().len();
            self.list_state.set_count(count);
        }
        None
    }

    fn draw_table(&self, frame: &mut Frame, area: Rect, policies: &[&PolicyInfo]) {
        let header = Row::new(vec![
            Cell::from("Name"),
            Cell::from("Max Rounds"),
            Cell::from("Effort"),
            Cell::from("Type"),
            Cell::from("Tags"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        let visible = area.height.saturating_sub(3) as usize;
        let rows: Vec<Row> = policies
            .iter()
            .enumerate()
            .skip(self.list_state.scroll_offset)
            .take(visible.max(1))
            .map(|(i, policy)| {
                let style = if i == self.list_state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let policy_type = if policy.is_role_based {
                    "role-based"
                } else {
                    "static"
                };

                Row::new(vec![
                    Cell::from(truncate(&policy.name, 25)),
                    Cell::from(policy.max_rounds.to_string()),
                    Cell::from(format!("{:.2}", policy.effort)),
                    Cell::from(policy_type),
                    Cell::from(truncate(&policy.tags.join(", "), 30)),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(27),
                Constraint::Length(12),
                Constraint::Length(11),
                Constraint::Length(12),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Remote (orchestrator) ({}) ", policies.len())),
        );

        frame.render_widget(table, area);
    }

    /// Read-only table of policies defined in the local workspace (nsed.yaml).
    fn draw_local_section(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("Name"),
            Cell::from("Max Rounds"),
            Cell::from("Effort"),
            Cell::from("Type"),
        ])
        .style(Style::default().fg(Color::DarkGray));
        let visible = area.height.saturating_sub(3) as usize;
        let rows: Vec<Row> = self
            .local_policies
            .iter()
            .take(visible.max(1))
            .map(|(name, cfg)| {
                let kind = if cfg.roles.is_some() {
                    "role-based"
                } else {
                    "static"
                };
                Row::new(vec![
                    Cell::from(truncate(name, 25)),
                    Cell::from(cfg.max_rounds.to_string()),
                    Cell::from(format!("{:.2}", cfg.effort)),
                    Cell::from(kind),
                ])
                .style(Style::default().fg(Color::DarkGray))
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(27),
                Constraint::Length(12),
                Constraint::Length(11),
                Constraint::Min(12),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Local (nsed.yaml) ({}) ",
            self.local_policies.len()
        )));
        frame.render_widget(table, area);
    }
}

/// Render a detail panel for a single policy.
fn draw_policy_detail(frame: &mut Frame, area: Rect, policy: &PolicyInfo) {
    let policy_type = if policy.is_role_based {
        "Role-based"
    } else {
        "Static"
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("ID: ", Style::default().fg(Color::Cyan)),
            Span::raw(&policy.policy_id),
        ]),
        Line::from(vec![
            Span::styled("Type: ", Style::default().fg(Color::Cyan)),
            Span::raw(policy_type),
        ]),
        Line::from(vec![
            Span::styled("Max Rounds: ", Style::default().fg(Color::Cyan)),
            Span::raw(policy.max_rounds.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Effort: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{:.0}%", policy.effort * 100.0)),
        ]),
    ];

    if !policy.tags.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tags",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for tag in &policy.tags {
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(Color::DarkGray)),
                Span::raw(tag),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to create a room from this policy",
        Style::default().fg(Color::DarkGray),
    )));

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", policy.name)),
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

    fn sample_policies() -> Vec<PolicyInfo> {
        vec![
            PolicyInfo {
                policy_id: "abc123".into(),
                name: "code-review".into(),
                tags: vec!["review".into(), "security".into()],
                max_rounds: 3,
                effort: 0.85,
                is_role_based: true,
            },
            PolicyInfo {
                policy_id: "def456".into(),
                name: "brainstorm".into(),
                tags: vec!["creative".into()],
                max_rounds: 2,
                effort: 0.70,
                is_role_based: false,
            },
        ]
    }

    #[test]
    fn on_enter_triggers_fetch() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        let actions = view.on_enter();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ViewAction::Fetch(FetchRequest::Policies { orchestrator, tag }) if orchestrator == "orch" && tag.is_none()
        ));
    }

    #[test]
    fn policies_loaded() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        let event = AppEvent::Data(DataEvent::PoliciesLoaded {
            orchestrator: "orch".into(),
            policies: sample_policies(),
        });
        view.update(&event);
        assert!(matches!(view.policies, LoadState::Loaded(_)));
        assert_eq!(view.list_state.count, 2);
    }

    #[test]
    fn filter_by_tag() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.filter_text = "security".into();

        let filtered = view.filtered_policies();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "code-review");
    }

    #[test]
    fn filter_by_name() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.filter_text = "brain".into();

        let filtered = view.filtered_policies();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "brainstorm");
    }

    #[test]
    fn slash_activates_filter() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.update(&make_key(KeyCode::Char('/')));
        assert!(view.filter_active);
    }

    #[test]
    fn enter_creates_room() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.list_state.set_count(2);

        let action = view.update(&make_key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::WriteConfig(ConfigMutation::AddRoom {
                name: "code-review-room".into(),
                policy: "abc123".into(),
                orchestrator: "orch".into(),
            }))
        );
    }

    #[test]
    fn d_opens_detail_panel() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.list_state.set_count(2);

        let action = view.update(&make_key(KeyCode::Char('d')));
        assert!(action.is_none());
        assert!(view.detail_visible);
    }

    #[test]
    fn escape_in_detail_closes_detail() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.list_state.set_count(2);
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT pop
        assert!(!view.detail_visible);
    }

    #[test]
    fn enter_in_detail_creates_room() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.list_state.set_count(2);
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::WriteConfig(ConfigMutation::AddRoom {
                name: "code-review-room".into(),
                policy: "abc123".into(),
                orchestrator: "orch".into(),
            }))
        );
        assert!(!view.detail_visible);
    }

    #[test]
    fn navigation_works_in_detail_mode() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loaded(sample_policies());
        view.list_state.set_count(2);
        view.detail_visible = true;

        assert_eq!(view.list_state.selected, 0);
        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.list_state.selected, 1);
        assert!(view.detail_visible);
    }

    #[test]
    fn escape_pops() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn fetch_error_transitions_to_error_state() {
        let mut view = PoliciesView::new("orch".into(), HashMap::new());
        view.policies = LoadState::Loading;

        let event = AppEvent::Data(DataEvent::FetchError {
            context: "policies".into(),
            error: "orchestrator has empty token".into(),
        });
        let action = view.update(&event);
        assert!(action.is_none());
        assert!(matches!(view.policies, LoadState::Error(ref e) if e.contains("empty token")));
    }

    fn local_policy() -> PolicyConfig {
        PolicyConfig {
            agents: Some(vec!["a".into(), "b".into()]),
            roles: None,
            max_rounds: 3,
            effort: 0.7,
            sla: None,
            capabilities: None,
            tags: None,
            mode: Default::default(),
        }
    }

    #[test]
    fn new_sorts_local_policies() {
        let mut local = HashMap::new();
        local.insert("zeta".to_string(), local_policy());
        local.insert("alpha".to_string(), local_policy());
        let v = PoliciesView::new("orch".into(), local);
        assert_eq!(v.local_policies[0].0, "alpha");
        assert_eq!(v.local_policies[1].0, "zeta");
    }

    #[test]
    fn draw_with_local_section_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut local = HashMap::new();
        local.insert("epic".to_string(), local_policy());
        let mut view = PoliciesView::new("orch".into(), local);
        view.policies = LoadState::Loaded(vec![]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.draw(frame, frame.area()))
            .unwrap();
    }
}
