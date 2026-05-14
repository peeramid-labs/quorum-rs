use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use super::common::{ListState, render_error, render_key_hints};
use super::{FetchRequest, View, ViewAction};
use crate::remote::HealthResponse;
use crate::tui::event::{self, AppEvent, DataEvent};
use crate::workspace::{OrchestratorConfig, OrchestratorMode};

/// Health check result for an orchestrator.
#[derive(Debug, Clone)]
pub enum HealthStatus {
    Unknown,
    Checking,
    Healthy(HealthResponse),
    Error(String),
}

/// Orchestrators list view with inline detail panel.
pub struct OrchestratorsView {
    orchestrators: Vec<(String, OrchestratorConfig)>,
    health: HashMap<String, HealthStatus>,
    list_state: ListState,
    loading: bool,
    /// When true, shows a detail panel for the selected orchestrator.
    detail_visible: bool,
}

impl OrchestratorsView {
    pub fn new(orchestrators: HashMap<String, OrchestratorConfig>) -> Self {
        let mut sorted: Vec<_> = orchestrators.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let count = sorted.len();
        Self {
            orchestrators: sorted,
            health: HashMap::new(),
            list_state: ListState::new(count),
            loading: false,
            detail_visible: false,
        }
    }

    /// Update orchestrator list from config reload.
    pub fn update_orchestrators(&mut self, orchestrators: HashMap<String, OrchestratorConfig>) {
        let mut sorted: Vec<_> = orchestrators.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        // Prune stale health entries for removed orchestrators.
        let new_names: std::collections::HashSet<&str> =
            sorted.iter().map(|(n, _)| n.as_str()).collect();
        self.health.retain(|k, _| new_names.contains(k.as_str()));
        self.list_state.set_count(sorted.len());
        self.orchestrators = sorted;
    }

    /// Get the currently selected orchestrator, if any.
    fn selected_orch(&self) -> Option<&(String, OrchestratorConfig)> {
        self.orchestrators.get(self.list_state.selected)
    }
}

impl View for OrchestratorsView {
    fn on_enter(&mut self) -> Vec<ViewAction> {
        // Fire health checks for all remote orchestrators
        let mut actions = Vec::new();
        for (name, config) in &self.orchestrators {
            if config.mode_str() == "remote" {
                self.health.insert(name.clone(), HealthStatus::Checking);
                actions.push(ViewAction::Fetch(FetchRequest::Health {
                    orchestrator: name.clone(),
                }));
            }
        }
        self.loading = !actions.is_empty();
        actions
    }

    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        match app_event {
            AppEvent::Terminal(event) => {
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
                if event::is_key(event, 'd') && self.selected_orch().is_some() {
                    self.detail_visible = true;
                    return None;
                }
                None
            }
            AppEvent::Data(DataEvent::HealthResult {
                orchestrator,
                result,
            }) => {
                match result {
                    Ok(resp) => {
                        self.health
                            .insert(orchestrator.clone(), HealthStatus::Healthy(resp.clone()));
                    }
                    Err(e) => {
                        self.health
                            .insert(orchestrator.clone(), HealthStatus::Error(e.clone()));
                    }
                }
                // Check if all checks are done
                self.loading = self
                    .health
                    .values()
                    .any(|h| matches!(h, HealthStatus::Checking));
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(0),    // table
            Constraint::Length(1), // hints
        ])
        .split(area);

        if self.orchestrators.is_empty() {
            render_error(frame, chunks[0], "No orchestrators configured in nsed.yaml");
        } else if self.detail_visible {
            let h_chunks =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(chunks[0]);
            self.draw_table(frame, h_chunks[0]);
            if let Some((name, config)) = self.selected_orch() {
                draw_orch_detail(frame, h_chunks[1], name, config, self.health.get(name));
            }
        } else {
            self.draw_table(frame, chunks[0]);
        }

        let hints = if self.detail_visible {
            vec![("↑↓", "Navigate"), ("Esc", "Close detail")]
        } else {
            vec![("↑↓", "Navigate"), ("d", "Detail"), ("Esc", "Back")]
        };
        render_key_hints(frame, chunks[1], &hints);
    }
}

impl OrchestratorsView {
    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("Status"),
            Cell::from("Name"),
            Cell::from("Mode"),
            Cell::from("Address"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        let visible_height = area.height.saturating_sub(3) as usize;
        self.list_state.set_visible_height(visible_height);
        let rows: Vec<Row> = self
            .orchestrators
            .iter()
            .enumerate()
            .skip(self.list_state.scroll_offset)
            .take(visible_height.max(1))
            .map(|(i, (name, config))| {
                let style = if i == self.list_state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };

                let status = match self.health.get(name) {
                    Some(HealthStatus::Healthy(_)) => "✓",
                    Some(HealthStatus::Error(_)) => "✗",
                    Some(HealthStatus::Checking) => "⟳",
                    _ => "·",
                };

                let status_color = match self.health.get(name) {
                    Some(HealthStatus::Healthy(_)) => Color::Green,
                    Some(HealthStatus::Error(_)) => Color::Red,
                    Some(HealthStatus::Checking) => Color::Yellow,
                    _ => Color::DarkGray,
                };

                Row::new(vec![
                    Cell::from(Span::styled(status, Style::default().fg(status_color))),
                    Cell::from(name.as_str()),
                    Cell::from(config.mode_str()),
                    Cell::from(config.address_display()),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(20),
                Constraint::Length(10),
                Constraint::Min(30),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Orchestrators "),
        );

        frame.render_widget(table, area);
    }
}

/// Render a detail panel for a single orchestrator.
fn draw_orch_detail(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    config: &OrchestratorConfig,
    health: Option<&HealthStatus>,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Mode: ", Style::default().fg(Color::Cyan)),
            Span::raw(config.mode_str()),
        ]),
        Line::from(vec![
            Span::styled("Address: ", Style::default().fg(Color::Cyan)),
            Span::raw(config.address_display()),
        ]),
    ];

    if config.token.is_some() {
        lines.push(Line::from(vec![
            Span::styled("Token: ", Style::default().fg(Color::Cyan)),
            Span::raw("••••••••"),
        ]));
    }

    if let Some(url) = &config.nats_url {
        lines.push(Line::from(vec![
            Span::styled("NATS URL: ", Style::default().fg(Color::Cyan)),
            Span::raw(url),
        ]));
    }

    if let Some(cfg) = &config.config_file {
        lines.push(Line::from(vec![
            Span::styled("Config file: ", Style::default().fg(Color::Cyan)),
            Span::raw(cfg),
        ]));
    }

    // Health status
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Health",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));

    match health {
        Some(HealthStatus::Healthy(resp)) => {
            lines.push(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Cyan)),
                Span::styled("● Healthy", Style::default().fg(Color::Green)),
            ]));
            if !resp.nats_connection.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  NATS: ", Style::default().fg(Color::Cyan)),
                    Span::raw(&resp.nats_connection),
                ]));
            }
            if !resp.timestamp.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  Timestamp: ", Style::default().fg(Color::Cyan)),
                    Span::raw(&resp.timestamp),
                ]));
            }
        }
        Some(HealthStatus::Error(e)) => {
            lines.push(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Cyan)),
                Span::styled("● Error", Style::default().fg(Color::Red)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Error: ", Style::default().fg(Color::Cyan)),
                Span::raw(e),
            ]));
        }
        Some(HealthStatus::Checking) => {
            lines.push(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Cyan)),
                Span::styled("⟳ Checking...", Style::default().fg(Color::Yellow)),
            ]));
        }
        _ => {
            lines.push(Line::from(vec![
                Span::styled("  Status: ", Style::default().fg(Color::Cyan)),
                Span::raw("Not checked"),
            ]));
        }
    }

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {name} ")),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(detail, area);
}

/// Helper trait for OrchestratorConfig display.
trait OrchestratorDisplay {
    fn mode_str(&self) -> &str;
    fn address_display(&self) -> String;
}

impl OrchestratorDisplay for OrchestratorConfig {
    fn mode_str(&self) -> &str {
        match &self.mode {
            Some(OrchestratorMode::Remote) => "remote",
            Some(OrchestratorMode::Embedded) => "embedded",
            None => {
                if self.address.is_some() {
                    "remote"
                } else {
                    "local"
                }
            }
        }
    }

    fn address_display(&self) -> String {
        self.address.clone().unwrap_or_else(|| "localhost".into())
    }
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

    fn sample_orchestrators() -> HashMap<String, OrchestratorConfig> {
        let mut map = HashMap::new();
        map.insert(
            "local".into(),
            OrchestratorConfig {
                mode: None,
                address: None,
                token: None,
                nats_url: None,
                config_file: Some("config.yml".into()),
            },
        );
        map.insert(
            "remote".into(),
            OrchestratorConfig {
                mode: Some(OrchestratorMode::Remote),
                address: Some("https://api.example.com".into()),
                token: Some("${TOKEN}".into()),
                nats_url: None,
                config_file: None,
            },
        );
        map
    }

    #[test]
    fn new_sorts_by_name() {
        let view = OrchestratorsView::new(sample_orchestrators());
        assert_eq!(view.orchestrators[0].0, "local");
        assert_eq!(view.orchestrators[1].0, "remote");
    }

    #[test]
    fn on_enter_fires_health_checks_for_remote() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        let actions = view.on_enter();
        // Only the "remote" orchestrator should trigger a health check
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ViewAction::Fetch(FetchRequest::Health { orchestrator }) if orchestrator == "remote"
        ));
        assert!(view.loading);
    }

    #[test]
    fn escape_pops() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn d_opens_detail_panel() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        let action = view.update(&make_key(KeyCode::Char('d')));
        assert!(action.is_none());
        assert!(view.detail_visible);
    }

    #[test]
    fn escape_in_detail_closes_detail() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT pop
        assert!(!view.detail_visible);
    }

    #[test]
    fn navigation_in_detail_mode() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        view.detail_visible = true;

        assert_eq!(view.list_state.selected, 0);
        view.update(&make_key(KeyCode::Down));
        assert_eq!(view.list_state.selected, 1);
        assert!(view.detail_visible);
    }

    #[test]
    fn health_result_updates_status() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        view.health.insert("remote".into(), HealthStatus::Checking);
        view.loading = true;

        let event = AppEvent::Data(DataEvent::HealthResult {
            orchestrator: "remote".into(),
            result: Ok(HealthResponse {
                status: "ok".into(),
                nats_connection: "connected".into(),
                timestamp: "now".into(),
            }),
        });
        view.update(&event);

        assert!(matches!(
            view.health.get("remote"),
            Some(HealthStatus::Healthy(_))
        ));
        assert!(!view.loading);
    }

    #[test]
    fn health_error_updates_status() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        view.health.insert("remote".into(), HealthStatus::Checking);
        view.loading = true;

        let event = AppEvent::Data(DataEvent::HealthResult {
            orchestrator: "remote".into(),
            result: Err("connection refused".into()),
        });
        view.update(&event);

        assert!(matches!(
            view.health.get("remote"),
            Some(HealthStatus::Error(_))
        ));
    }

    #[test]
    fn empty_orchestrators() {
        let view = OrchestratorsView::new(HashMap::new());
        assert!(view.orchestrators.is_empty());
        assert_eq!(view.list_state.count, 0);
    }

    #[test]
    fn update_orchestrators_prunes_stale_health() {
        let mut view = OrchestratorsView::new(sample_orchestrators());
        view.health
            .insert("remote".into(), HealthStatus::Error("fail".into()));
        view.health
            .insert("local".into(), HealthStatus::Error("fail".into()));

        // Update with only "local" — "remote" should be pruned from health map.
        let mut new = HashMap::new();
        new.insert(
            "local".into(),
            OrchestratorConfig {
                mode: None,
                address: None,
                token: None,
                nats_url: None,
                config_file: Some("config.yml".into()),
            },
        );
        view.update_orchestrators(new);

        assert_eq!(view.orchestrators.len(), 1);
        assert!(view.health.contains_key("local"));
        assert!(!view.health.contains_key("remote"));
    }
}
