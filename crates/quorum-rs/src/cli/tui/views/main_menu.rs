use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, Wrap};

use super::common::{ListState, render_error, render_key_hints, truncate};
use super::{View, ViewAction};
use crate::cli::tui::app::ViewId;
use crate::cli::tui::event::{self, AppEvent};
use crate::cli::workspace::RoomConfig;

/// Main menu — rooms table as the primary screen.
///
/// Enter = start deliberation (task input), d = detail, Tab = settings menu.
pub struct MainMenuView {
    rooms: Vec<(String, RoomConfig)>,
    default_room: Option<String>,
    list_state: ListState,
    task_input_active: bool,
    task_text: String,
    detail_visible: bool,
    /// Convergence threshold (`effort`) the operator wants for the
    /// deliberation. Stored as a string while the user types so
    /// partial input (`"0."`) doesn't get rounded away. Parsed on
    /// submit. Defaults to `"0.7"` — the same number every
    /// generated `workspace.yaml` ships with.
    pub threshold_text: String,
    /// `true` when Tab has moved focus from the task input to the
    /// threshold field. Keystrokes route to `threshold_text` instead
    /// of `task_text`.
    pub threshold_input_active: bool,
}

impl MainMenuView {
    pub fn new(rooms: HashMap<String, RoomConfig>, default_room: Option<String>) -> Self {
        let mut sorted: Vec<_> = rooms.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let count = sorted.len();
        Self {
            rooms: sorted,
            default_room,
            list_state: ListState::new(count),
            task_input_active: false,
            task_text: String::new(),
            detail_visible: false,
            threshold_text: String::from("0.7"),
            threshold_input_active: false,
        }
    }

    /// Parse the threshold field. Returns `None` (use policy default)
    /// when the field is empty or unparseable; clamped to `[0.0, 1.0]`
    /// when valid.
    fn parsed_threshold(&self) -> Option<f32> {
        let trimmed = self.threshold_text.trim();
        if trimmed.is_empty() {
            return None;
        }
        trimmed.parse::<f32>().ok().map(|v| v.clamp(0.0, 1.0))
    }

    /// Get the currently selected room, if any.
    fn selected_room(&self) -> Option<&(String, RoomConfig)> {
        self.rooms.get(self.list_state.selected)
    }
}

impl View for MainMenuView {
    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        let AppEvent::Terminal(event) = app_event else {
            return None;
        };

        // Task input mode
        if self.task_input_active {
            return self.update_task_input(event);
        }

        // Detail mode
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
            if event::is_enter(event) && !self.rooms.is_empty() {
                self.detail_visible = false;
                self.task_input_active = true;
                self.task_text.clear();
            }
            return None;
        }

        // Normal mode
        if event::is_key(event, 'q') || event::is_escape(event) {
            return Some(ViewAction::Quit);
        }
        if event::is_up(event) {
            self.list_state.up();
        }
        if event::is_down(event) {
            self.list_state.down();
        }
        // Enter = start deliberation
        if event::is_enter(event) && !self.rooms.is_empty() {
            self.task_input_active = true;
            self.task_text.clear();
            return None;
        }
        // d = detail panel
        if event::is_key(event, 'd') && self.selected_room().is_some() {
            self.detail_visible = true;
            return None;
        }
        // Tab = settings menu
        if event::is_tab(event) {
            return Some(ViewAction::Push(ViewId::SettingsMenu));
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Compute dynamic input height: wrap text to available width,
        // capped so title (2), key hints (1), and at least 1 table row remain visible.
        let input_height = if self.task_input_active {
            let inner_width = area.width.saturating_sub(2).max(1) as usize; // minus borders
            let char_count = self.task_text.chars().count().max(1);
            let wrapped_lines = char_count.div_ceil(inner_width);
            let reserved = 2 + 1 + 1; // title + key hints + min 1 table row
            let max_lines = area.height.saturating_sub(reserved + 2) as usize; // -2 for own borders
            let max_lines = max_lines.max(1); // always show at least 1 line
            (wrapped_lines.min(max_lines) as u16) + 2
        } else {
            0
        };

        // Single-line threshold input below the task input. Visible
        // whenever the task input is active.
        let threshold_height = if self.task_input_active { 3 } else { 0 };

        let chunks = Layout::vertical([
            Constraint::Length(2),                // title
            Constraint::Length(input_height),     // task input
            Constraint::Length(threshold_height), // threshold input
            Constraint::Min(0),                   // rooms table
            Constraint::Length(1),                // key hints
        ])
        .split(area);

        // Title
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " NSED ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("— Multi-Agent Deliberation"),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_type(BorderType::Plain),
        );
        frame.render_widget(title, chunks[0]);

        // Task input
        if self.task_input_active {
            let room_name = self
                .rooms
                .get(self.list_state.selected)
                .map(|(n, _)| n.as_str())
                .unwrap_or("?");
            let task_focused = !self.threshold_input_active;
            let task_style = if task_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let task_border = if task_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let input = Paragraph::new(self.task_text.as_str())
                .style(task_style)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(task_border)
                        .title(format!(" Task for '{room_name}' (Tab → threshold) ")),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(input, chunks[1]);

            // Threshold input (always shown while in task mode).
            let threshold_focused = self.threshold_input_active;
            let threshold_style = if threshold_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let threshold_border = if threshold_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let threshold_paragraph = Paragraph::new(self.threshold_text.as_str())
                .style(threshold_style)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(threshold_border)
                        .title(" Convergence threshold (0.0–1.0, default 0.7) "),
                );
            frame.render_widget(threshold_paragraph, chunks[2]);
        }

        // Rooms table + optional detail
        if self.rooms.is_empty() {
            render_error(
                frame,
                chunks[3],
                "No rooms configured — add rooms to nsed.yaml",
            );
        } else if self.detail_visible {
            let h_chunks =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(chunks[3]);
            self.draw_table(frame, h_chunks[0]);
            if let Some((name, config)) = self.selected_room() {
                draw_room_detail(
                    frame,
                    h_chunks[1],
                    name,
                    config,
                    self.default_room.as_deref(),
                );
            }
        } else {
            self.draw_table(frame, chunks[3]);
        }

        // Key hints
        let hints = if self.task_input_active {
            vec![
                ("Enter", "Start"),
                ("Tab", "Task↔Threshold"),
                ("Esc", "Cancel"),
            ]
        } else if self.detail_visible {
            vec![
                ("↑↓", "Navigate"),
                ("Enter", "Deliberate"),
                ("Esc", "Close"),
            ]
        } else {
            vec![
                ("↑↓", "Navigate"),
                ("Enter", "Deliberate"),
                ("d", "Detail"),
                ("Tab", "Settings"),
                ("q", "Quit"),
            ]
        };
        render_key_hints(frame, chunks[4], &hints);
    }
}

impl MainMenuView {
    fn update_task_input(&mut self, event: &crossterm::event::Event) -> Option<ViewAction> {
        if event::is_escape(event) {
            self.task_input_active = false;
            self.threshold_input_active = false;
            return None;
        }
        // Tab toggles focus between the task field and the threshold
        // field. Threshold field auto-selects all on focus so retyping
        // is easy (clear is achieved with Backspace once focused).
        if event::is_tab(event) {
            self.threshold_input_active = !self.threshold_input_active;
            return None;
        }
        if event::is_enter(event) && !self.task_text.is_empty() {
            self.task_input_active = false;
            self.threshold_input_active = false;
            if self.rooms.is_empty() || self.list_state.selected >= self.rooms.len() {
                return Some(ViewAction::SetStatus(
                    "No rooms available".into(),
                    super::StatusLevel::Error,
                ));
            }
            let (room_name, room_config) = &self.rooms[self.list_state.selected];
            let orchestrator = match &room_config.orchestrator {
                Some(o) => o.clone(),
                None => {
                    return Some(ViewAction::SetStatus(
                        format!("Room '{room_name}' has no orchestrator configured"),
                        super::StatusLevel::Error,
                    ));
                }
            };
            let effort_override = self.parsed_threshold();
            return Some(ViewAction::LaunchJob {
                orchestrator,
                task: self.task_text.clone(),
                room: Some(room_name.clone()),
                policy: None,
                effort_override,
            });
        }
        if let crossterm::event::Event::Key(key) = event
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            // Route printable / backspace keystrokes into whichever
            // field is currently focused.
            let target: &mut String = if self.threshold_input_active {
                &mut self.threshold_text
            } else {
                &mut self.task_text
            };
            match key.code {
                crossterm::event::KeyCode::Char(c) => {
                    target.push(c);
                    return None;
                }
                crossterm::event::KeyCode::Backspace => {
                    target.pop();
                    return None;
                }
                _ => {}
            }
        }
        None
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from(""),
            Cell::from("Room"),
            Cell::from("Policy"),
            Cell::from("Orchestrator"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        // Borders + header consume 3 rows; remainder is visible item rows.
        let visible_height = area.height.saturating_sub(3) as usize;
        self.list_state.set_visible_height(visible_height);
        let rows: Vec<Row> = self
            .rooms
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

                let is_default = self.default_room.as_deref() == Some(name.as_str());
                let marker = if is_default { "★" } else { " " };

                Row::new(vec![
                    Cell::from(Span::styled(marker, Style::default().fg(Color::Yellow))),
                    Cell::from(name.as_str()),
                    Cell::from(truncate(&config.policy, 25)),
                    Cell::from(config.orchestrator.as_deref().unwrap_or("default")),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(20),
                Constraint::Length(27),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Rooms ({}) — Select and deliberate ",
            self.rooms.len()
        )));

        frame.render_widget(table, area);
    }
}

/// Render a detail panel for a single room.
fn draw_room_detail(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    config: &RoomConfig,
    default_room: Option<&str>,
) {
    let is_default = default_room == Some(name);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Policy: ", Style::default().fg(Color::Cyan)),
            Span::raw(&config.policy),
        ]),
        Line::from(vec![
            Span::styled("Orchestrator: ", Style::default().fg(Color::Cyan)),
            Span::raw(config.orchestrator.as_deref().unwrap_or("(default)")),
        ]),
        Line::from(vec![
            Span::styled("Default: ", Style::default().fg(Color::Cyan)),
            if is_default {
                Span::styled("★ Yes", Style::default().fg(Color::Yellow))
            } else {
                Span::raw("No")
            },
        ]),
    ];

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to start a deliberation",
        Style::default().fg(Color::DarkGray),
    )));

    let detail = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Room: {name} ")),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(detail, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tui::views::StatusLevel;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn sample_rooms() -> HashMap<String, RoomConfig> {
        let mut map = HashMap::new();
        map.insert(
            "local".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: Some("local-orch".into()),
            },
        );
        map.insert(
            "remote".into(),
            RoomConfig {
                policy: "brainstorm".into(),
                orchestrator: Some("peeramid".into()),
            },
        );
        map
    }

    #[test]
    fn new_sorts_by_name() {
        let view = MainMenuView::new(sample_rooms(), Some("local".into()));
        assert_eq!(view.rooms[0].0, "local");
        assert_eq!(view.rooms[1].0, "remote");
    }

    #[test]
    fn q_quits() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        let action = view.update(&make_key(KeyCode::Char('q')));
        assert_eq!(action, Some(ViewAction::Quit));
    }

    #[test]
    fn escape_quits() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        let action = view.update(&make_key(KeyCode::Esc));
        assert_eq!(action, Some(ViewAction::Quit));
    }

    #[test]
    fn enter_activates_task_input() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        let action = view.update(&make_key(KeyCode::Enter));
        assert!(action.is_none());
        assert!(view.task_input_active);
    }

    #[test]
    fn d_opens_detail_panel() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        let action = view.update(&make_key(KeyCode::Char('d')));
        assert!(action.is_none());
        assert!(view.detail_visible);
    }

    #[test]
    fn tab_pushes_settings_menu() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        let action = view.update(&make_key(KeyCode::Tab));
        assert_eq!(action, Some(ViewAction::Push(ViewId::SettingsMenu)));
    }

    #[test]
    fn escape_in_detail_closes_detail() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none()); // Does NOT quit
        assert!(!view.detail_visible);
    }

    #[test]
    fn enter_in_detail_opens_task_input() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.detail_visible = true;

        let action = view.update(&make_key(KeyCode::Enter));
        assert!(action.is_none());
        assert!(view.task_input_active);
        assert!(!view.detail_visible);
    }

    #[test]
    fn task_input_enter_launches_job() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.task_text = "Review my code".into();

        let action = view.update(&make_key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::LaunchJob {
                orchestrator: "local-orch".into(),
                task: "Review my code".into(),
                room: Some("local".into()),
                policy: None,
                effort_override: Some(0.7),
            })
        );
        assert!(!view.task_input_active);
    }

    #[test]
    fn task_input_threshold_parsed_into_effort_override() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.task_text = "spicy debate".into();
        view.threshold_text = "0.42".into();

        let action = view.update(&make_key(KeyCode::Enter));
        match action {
            Some(ViewAction::LaunchJob {
                effort_override, ..
            }) => assert_eq!(effort_override, Some(0.42)),
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn threshold_parse_empty_yields_none() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.threshold_text.clear();
        assert!(view.parsed_threshold().is_none());
    }

    #[test]
    fn threshold_parse_clamps_above_one() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.threshold_text = "1.5".into();
        assert_eq!(view.parsed_threshold(), Some(1.0));
    }

    #[test]
    fn threshold_parse_clamps_negative() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.threshold_text = "-0.5".into();
        assert_eq!(view.parsed_threshold(), Some(0.0));
    }

    #[test]
    fn tab_in_task_input_toggles_threshold_focus() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        assert!(!view.threshold_input_active);
        view.update(&make_key(KeyCode::Tab));
        assert!(view.threshold_input_active);
        view.update(&make_key(KeyCode::Tab));
        assert!(!view.threshold_input_active);
    }

    #[test]
    fn keystrokes_in_threshold_focus_edit_threshold_text() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.threshold_input_active = true;
        view.threshold_text.clear();
        view.update(&make_key(KeyCode::Char('0')));
        view.update(&make_key(KeyCode::Char('.')));
        view.update(&make_key(KeyCode::Char('9')));
        assert_eq!(view.threshold_text, "0.9");
        // The task field shouldn't have been touched.
        assert_eq!(view.task_text, "");
    }

    #[test]
    fn task_input_empty_enter_ignored() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.task_text.clear();

        let action = view.update(&make_key(KeyCode::Enter));
        assert!(action.is_none());
    }

    #[test]
    fn task_input_escape_cancels() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.task_text = "some text".into();

        let action = view.update(&make_key(KeyCode::Esc));
        assert!(action.is_none());
        assert!(!view.task_input_active);
    }

    #[test]
    fn task_input_no_orchestrator_returns_error() {
        let mut rooms = HashMap::new();
        rooms.insert(
            "no-orch".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: None,
            },
        );
        let mut view = MainMenuView::new(rooms, None);
        view.task_input_active = true;
        view.task_text = "do something".into();

        let action = view.update(&make_key(KeyCode::Enter));
        assert!(matches!(
            action,
            Some(ViewAction::SetStatus(msg, StatusLevel::Error))
            if msg.contains("no orchestrator")
        ));
    }

    #[test]
    fn empty_rooms_no_crash() {
        let view = MainMenuView::new(HashMap::new(), None);
        assert!(view.rooms.is_empty());
    }

    #[test]
    fn task_input_typing_appends_characters() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;

        view.update(&make_key(KeyCode::Char('H')));
        view.update(&make_key(KeyCode::Char('i')));
        assert_eq!(view.task_text, "Hi");
    }

    #[test]
    fn task_input_backspace_removes_last_char() {
        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        view.task_text = "Hello".into();

        view.update(&make_key(KeyCode::Backspace));
        assert_eq!(view.task_text, "Hell");
    }

    /// Long text wraps without panic: dynamic layout accepts multi-line input.
    #[test]
    fn task_input_draw_does_not_panic_on_long_text() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut view = MainMenuView::new(sample_rooms(), None);
        view.task_input_active = true;
        // 200 chars on an 80-col terminal → 3 wrapped lines → height 5 (3 + 2 borders)
        view.task_text = "a".repeat(200);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                view.draw(frame, area);
            })
            .unwrap();

        // Verify the input area got more than the old fixed 3 rows.
        // With 200 chars / 78 inner width ≈ 3 lines → height = 5 (3 lines + 2 borders).
        // The test passes if draw() doesn't panic — the layout accepted dynamic height.
    }
}
