use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use super::common::{ListState, render_key_hints};
use super::{View, ViewAction};
use crate::tui::app::ViewId;
use crate::tui::event::{self, AppEvent};

const MENU_ITEMS: &[(&str, &str, ViewId)] = &[
    (
        "Agents",
        "View connected agents and their status",
        ViewId::Agents,
    ),
    (
        "Policies",
        "Browse and manage deliberation policies",
        ViewId::Policies,
    ),
    (
        "Orchestrators",
        "Check orchestrator health and config",
        ViewId::Orchestrators,
    ),
    ("Config", "View workspace configuration", ViewId::Settings),
];

/// Settings sub-menu — Agents, Policies, Orchestrators, Config.
pub struct SettingsMenuView {
    list_state: ListState,
}

impl Default for SettingsMenuView {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsMenuView {
    pub fn new() -> Self {
        Self {
            list_state: ListState::new(MENU_ITEMS.len()),
        }
    }
}

impl View for SettingsMenuView {
    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        let AppEvent::Terminal(event) = app_event else {
            return None;
        };

        if event::is_escape(event) || event::is_key(event, 'q') {
            return Some(ViewAction::Pop);
        }
        if event::is_up(event) {
            self.list_state.up();
            return None;
        }
        if event::is_down(event) {
            self.list_state.down();
            return None;
        }
        if event::is_enter(event) {
            let (_, _, view_id) = &MENU_ITEMS[self.list_state.selected];
            return Some(ViewAction::Push(view_id.clone()));
        }

        // Number key shortcuts (1-4)
        if let Event::Key(key_event) = event
            && let crossterm::event::KeyCode::Char(c) = key_event.code
            && let Some(idx) = c.to_digit(10)
        {
            let idx = idx as usize;
            if idx >= 1 && idx <= MENU_ITEMS.len() {
                self.list_state.selected = idx - 1;
                let (_, _, view_id) = &MENU_ITEMS[idx - 1];
                return Some(ViewAction::Push(view_id.clone()));
            }
        }

        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Min(0),    // menu
            Constraint::Length(1), // key hints
        ])
        .split(area);

        let visible_height = chunks[0].height.saturating_sub(2) as usize; // borders
        self.list_state.set_visible_height(visible_height);

        let items: Vec<ListItem> = MENU_ITEMS
            .iter()
            .enumerate()
            .map(|(i, (label, desc, _))| {
                let style = if i == self.list_state.selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let prefix = if i == self.list_state.selected {
                    "▸ "
                } else {
                    "  "
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{prefix}{} ", i + 1), style),
                    Span::styled(label.to_string(), style),
                    Span::styled(format!("  {desc}"), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title(" Settings "));
        frame.render_widget(list, chunks[0]);

        render_key_hints(
            frame,
            chunks[1],
            &[
                ("↑↓", "Navigate"),
                ("Enter", "Select"),
                ("1-4", "Jump"),
                ("Esc", "Back"),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key_event(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    #[test]
    fn initial_selection_is_zero() {
        let view = SettingsMenuView::new();
        assert_eq!(view.list_state.selected, 0);
    }

    #[test]
    fn escape_pops() {
        let mut view = SettingsMenuView::new();
        let action = view.update(&make_key_event(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Pop));
    }

    #[test]
    fn enter_pushes_agents() {
        let mut view = SettingsMenuView::new();
        let action = view.update(&make_key_event(KeyCode::Enter));
        assert_eq!(action, Some(ViewAction::Push(ViewId::Agents)));
    }

    #[test]
    fn number_key_jumps() {
        let mut view = SettingsMenuView::new();
        let action = view.update(&make_key_event(KeyCode::Char('2')));
        assert_eq!(action, Some(ViewAction::Push(ViewId::Policies)));
        assert_eq!(view.list_state.selected, 1);
    }

    #[test]
    fn down_moves_selection() {
        let mut view = SettingsMenuView::new();
        view.update(&make_key_event(KeyCode::Down));
        assert_eq!(view.list_state.selected, 1);
    }

    #[test]
    fn number_out_of_range_ignored() {
        let mut view = SettingsMenuView::new();
        let action = view.update(&make_key_event(KeyCode::Char('9')));
        assert!(action.is_none());
    }

    #[test]
    fn tick_ignored() {
        let mut view = SettingsMenuView::new();
        let action = view.update(&AppEvent::Tick);
        assert!(action.is_none());
    }
}
