use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::common::render_key_hints;
use super::{View, ViewAction};
use crate::cli::tui::event::{self, AppEvent};
use crate::cli::workspace::WorkspaceConfig;

/// Settings view — read-only display of workspace configuration.
pub struct SettingsView {
    config_path: String,
    default_room: Option<String>,
    policy_count: usize,
    room_count: usize,
    orchestrator_count: usize,
    shared_contexts: Vec<String>,
    agents_config_file: Option<String>,
}

impl SettingsView {
    pub fn from_config(config: &WorkspaceConfig, config_path: &str) -> Self {
        Self {
            config_path: config_path.to_string(),
            default_room: config.default_room.clone(),
            policy_count: config.policies.len(),
            room_count: config.rooms.len(),
            orchestrator_count: config.orchestrators.len(),
            shared_contexts: config
                .shared
                .as_ref()
                .map(|refs| refs.iter().map(|c| c.path.clone()).collect())
                .unwrap_or_default(),
            agents_config_file: config.agents.as_ref().map(|a| a.config_file.clone()),
        }
    }
}

impl View for SettingsView {
    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        let AppEvent::Terminal(event) = app_event else {
            return None;
        };

        if event::is_escape(event) || event::is_key(event, 'q') {
            return Some(ViewAction::Pop);
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(0),    // content
            Constraint::Length(1), // hints
        ])
        .split(area);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Config file: ", Style::default().fg(Color::Cyan)),
                Span::raw(&self.config_path),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Default room: ", Style::default().fg(Color::Cyan)),
                Span::raw(self.default_room.as_deref().unwrap_or("(not set)")),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Policies: ", Style::default().fg(Color::Cyan)),
                Span::raw(self.policy_count.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Rooms: ", Style::default().fg(Color::Cyan)),
                Span::raw(self.room_count.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Orchestrators: ", Style::default().fg(Color::Cyan)),
                Span::raw(self.orchestrator_count.to_string()),
            ]),
        ];

        if let Some(ref agents_file) = self.agents_config_file {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Agents config: ", Style::default().fg(Color::Cyan)),
                Span::raw(agents_file),
            ]));
        }

        if !self.shared_contexts.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Shared contexts:",
                Style::default().fg(Color::Cyan),
            )));
            for ctx in &self.shared_contexts {
                lines.push(Line::from(format!("  • {ctx}")));
            }
        }

        let paragraph =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Settings "));
        frame.render_widget(paragraph, chunks[0]);

        render_key_hints(frame, chunks[1], &[("Esc", "Back")]);
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

    fn minimal_config() -> WorkspaceConfig {
        WorkspaceConfig {
            policies: Default::default(),
            orchestrators: Default::default(),
            rooms: Default::default(),
            shared: None,
            default_room: Some("test-room".into()),
            agents: None,
        }
    }

    #[test]
    fn from_config_captures_fields() {
        let config = minimal_config();
        let view = SettingsView::from_config(&config, "nsed.yaml");
        assert_eq!(view.config_path, "nsed.yaml");
        assert_eq!(view.default_room.as_deref(), Some("test-room"));
        assert_eq!(view.policy_count, 0);
        assert_eq!(view.room_count, 0);
        assert_eq!(view.orchestrator_count, 0);
    }

    #[test]
    fn escape_pops() {
        let config = minimal_config();
        let mut view = SettingsView::from_config(&config, "nsed.yaml");
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn q_pops() {
        let config = minimal_config();
        let mut view = SettingsView::from_config(&config, "nsed.yaml");
        let action = view.update(&make_key(KeyCode::Char('q')));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn other_keys_ignored() {
        let config = minimal_config();
        let mut view = SettingsView::from_config(&config, "nsed.yaml");
        let action = view.update(&make_key(KeyCode::Char('x')));
        assert!(action.is_none());
    }
}
