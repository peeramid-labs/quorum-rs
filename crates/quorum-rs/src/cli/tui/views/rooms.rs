use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use super::common::{ListState, render_error, render_key_hints, render_loading, truncate};
use super::{FetchRequest, View, ViewAction};
use crate::cli::remote::DiscoveredRoom;
use crate::cli::tui::event::{self, AppEvent, DataEvent};
use crate::cli::tui::views::agents::LoadState;

/// Which sub-mode the Rooms view is in.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Browsing the room list.
    List,
    /// Filling in the create-room form.
    Create,
    /// Confirming deletion of the named room.
    ConfirmDelete(String),
}

/// Number of fields in the create form: id, tags, policy, visibility.
const FORM_FIELDS: usize = 4;

/// Create-room form state. `field` cycles id → tags → policy → visibility.
#[derive(Debug, Default, Clone)]
struct CreateForm {
    id: String,
    tags: String,
    /// Optional deliberation policy bound to the room (name or policy_id).
    policy: String,
    /// `false` = private, `true` = public.
    public: bool,
    field: usize,
}

impl CreateForm {
    fn visibility(&self) -> &'static str {
        if self.public { "public" } else { "private" }
    }
    /// Parse the comma-separated tags field into a trimmed, non-empty list.
    fn tag_list(&self) -> Vec<String> {
        self.tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
    /// The policy field as an `Option`, `None` when blank.
    fn policy_opt(&self) -> Option<String> {
        let p = self.policy.trim();
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
    }
}

/// Rooms management view — list, create (`POST /admin/api/rooms`), and
/// delete (`DELETE /admin/api/rooms/{id}`). Mutations require the `admin`
/// or `manage_rooms` role; a scoped manager only succeeds for rooms whose
/// tags their grants cover (the server enforces this, surfaced as an
/// error here).
pub struct RoomsView {
    orchestrator: String,
    rooms: LoadState<Vec<DiscoveredRoom>>,
    list_state: ListState,
    mode: Mode,
    form: CreateForm,
    /// Last create/delete failure (e.g. `server returned 403: …`). Shown as
    /// a persistent red banner until the next successful mutation — the
    /// transient status line is too easy to miss.
    last_error: Option<String>,
}

impl RoomsView {
    /// Construct a Rooms view targeting the named remote `orchestrator`.
    pub fn new(orchestrator: String) -> Self {
        Self {
            orchestrator,
            rooms: LoadState::NotLoaded,
            list_state: ListState::new(0),
            mode: Mode::List,
            form: CreateForm::default(),
            last_error: None,
        }
    }

    fn fetch(&self) -> ViewAction {
        ViewAction::Fetch(FetchRequest::Rooms {
            orchestrator: self.orchestrator.clone(),
        })
    }

    fn selected_room(&self) -> Option<&DiscoveredRoom> {
        match &self.rooms {
            LoadState::Loaded(rooms) => rooms.get(self.list_state.selected),
            _ => None,
        }
    }

    /// Handle a key while the create form is active.
    fn update_create(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{Event, KeyCode, KeyEventKind};
        if event::is_escape(ev) {
            self.mode = Mode::List;
            self.form = CreateForm::default();
            return None;
        }
        if event::is_enter(ev) {
            let id = self.form.id.trim().to_string();
            if id.is_empty() {
                self.last_error = Some("Room id required".to_string());
                return None;
            }
            // Keep the form open and clear any prior error. On success a
            // `RoomMutated` event closes the form; on failure a
            // `FetchError` populates `last_error` (shown in red) so the
            // operator sees exactly why and can edit + retry.
            self.last_error = None;
            return Some(ViewAction::Fetch(FetchRequest::CreateRoom {
                orchestrator: self.orchestrator.clone(),
                id,
                tags: self.form.tag_list(),
                visibility: self.form.visibility().to_string(),
                policy: self.form.policy_opt(),
            }));
        }
        if let Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Tab | KeyCode::Down => {
                    self.form.field = (self.form.field + 1) % FORM_FIELDS
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.form.field = (self.form.field + FORM_FIELDS - 1) % FORM_FIELDS
                }
                KeyCode::Backspace => match self.form.field {
                    0 => {
                        self.form.id.pop();
                    }
                    1 => {
                        self.form.tags.pop();
                    }
                    2 => {
                        self.form.policy.pop();
                    }
                    _ => {}
                },
                KeyCode::Char(c) => match self.form.field {
                    0 => self.form.id.push(c),
                    1 => self.form.tags.push(c),
                    2 => self.form.policy.push(c),
                    // Visibility field (3): space toggles.
                    3 if c == ' ' => self.form.public = !self.form.public,
                    _ => {}
                },
                KeyCode::Left | KeyCode::Right if self.form.field == 3 => {
                    self.form.public = !self.form.public;
                }
                _ => {}
            }
        }
        None
    }
}

impl View for RoomsView {
    fn on_enter(&mut self) -> Vec<ViewAction> {
        self.rooms = LoadState::Loading;
        vec![self.fetch()]
    }

    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        match app_event {
            AppEvent::Terminal(ev) => match self.mode.clone() {
                Mode::Create => self.update_create(ev),
                Mode::ConfirmDelete(id) => {
                    if event::is_key(ev, 'y') {
                        self.mode = Mode::List;
                        return Some(ViewAction::Fetch(FetchRequest::DeleteRoom {
                            orchestrator: self.orchestrator.clone(),
                            id,
                        }));
                    }
                    if event::is_escape(ev) || event::is_key(ev, 'n') {
                        self.mode = Mode::List;
                    }
                    None
                }
                Mode::List => {
                    if event::is_escape(ev) || event::is_key(ev, 'q') {
                        return Some(ViewAction::Pop);
                    }
                    if event::is_up(ev) {
                        self.list_state.up();
                    }
                    if event::is_down(ev) {
                        self.list_state.down();
                    }
                    if event::is_key(ev, 'n') {
                        self.mode = Mode::Create;
                        self.form = CreateForm::default();
                        return None;
                    }
                    if event::is_key(ev, 'd')
                        && let Some(room) = self.selected_room()
                    {
                        self.mode = Mode::ConfirmDelete(room.id.clone());
                        return None;
                    }
                    if event::is_key(ev, 'r') {
                        self.rooms = LoadState::Loading;
                        return Some(self.fetch());
                    }
                    None
                }
            },
            AppEvent::Data(DataEvent::RoomsLoaded {
                orchestrator,
                rooms,
            }) if *orchestrator == self.orchestrator => {
                self.list_state.set_count(rooms.len());
                self.rooms = LoadState::Loaded(rooms.clone());
                None
            }
            // A create/delete landed — close the form, clear the error,
            // and refresh the list to reflect it.
            AppEvent::Data(DataEvent::RoomMutated { orchestrator, .. })
                if *orchestrator == self.orchestrator =>
            {
                self.mode = Mode::List;
                self.form = CreateForm::default();
                self.last_error = None;
                self.rooms = LoadState::Loading;
                Some(self.fetch())
            }
            AppEvent::Data(DataEvent::FetchError { context, error }) if context == "rooms" => {
                // Persist the failure as a red banner (the transient status
                // line is too easy to miss) and keep the form open so the
                // operator can read the server message and retry. Keep the
                // (possibly stale) list visible on a mutation error; only a
                // failed initial load replaces it.
                self.last_error = Some(error.clone());
                if matches!(self.rooms, LoadState::Loading | LoadState::NotLoaded) {
                    self.rooms = LoadState::Error(error.clone());
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let form_height = match self.mode {
            Mode::Create => 9,
            Mode::ConfirmDelete(_) => 3,
            Mode::List => 0,
        };
        let err_height = if self.last_error.is_some() { 3 } else { 0 };
        let chunks = Layout::vertical([
            Constraint::Length(form_height),
            Constraint::Length(err_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

        if let Some(err) = &self.last_error {
            let banner = Paragraph::new(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red))
                    .title(" Error "),
            )
            .wrap(ratatui::widgets::Wrap { trim: true });
            frame.render_widget(banner, chunks[1]);
        }

        match &self.mode {
            Mode::Create => self.draw_create_form(frame, chunks[0]),
            Mode::ConfirmDelete(id) => {
                let prompt = Paragraph::new(Line::from(vec![
                    Span::styled("Delete room ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("\"{id}\""),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("?  (y / n)", Style::default().fg(Color::Yellow)),
                ]))
                .block(Block::default().borders(Borders::ALL).title(" Confirm "));
                frame.render_widget(prompt, chunks[0]);
            }
            Mode::List => {}
        }

        match &self.rooms {
            LoadState::NotLoaded | LoadState::Loading => {
                render_loading(frame, chunks[2], "Loading rooms...");
            }
            LoadState::Error(e) => render_error(frame, chunks[2], e),
            LoadState::Loaded(rooms) => {
                if rooms.is_empty() {
                    render_error(frame, chunks[2], "No rooms — press 'n' to create one");
                } else {
                    let split = Layout::horizontal([Constraint::Min(40), Constraint::Length(34)])
                        .split(chunks[2]);
                    self.draw_table(frame, split[0], rooms);
                    if let Some(room) = rooms.get(self.list_state.selected) {
                        draw_room_detail(frame, split[1], room);
                    }
                }
            }
        }

        let hints: Vec<(&str, &str)> = match self.mode {
            Mode::Create => vec![
                ("Tab/↑↓", "Field"),
                ("Space/←→", "Visibility"),
                ("Enter", "Create"),
                ("Esc", "Cancel"),
            ],
            Mode::ConfirmDelete(_) => vec![("y", "Delete"), ("n/Esc", "Cancel")],
            Mode::List => vec![
                ("↑↓", "Navigate"),
                ("n", "New room"),
                ("d", "Delete"),
                ("r", "Refresh"),
                ("Esc", "Back"),
            ],
        };
        render_key_hints(frame, chunks[3], &hints);
    }
}

impl RoomsView {
    fn draw_create_form(&self, frame: &mut Frame, area: Rect) {
        let field_line = |label: &str, value: String, active: bool| {
            let label_style = if active {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let cursor = if active { "_" } else { "" };
            Line::from(vec![
                Span::styled(format!("{label:<12}"), label_style),
                Span::raw(value),
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
            ])
        };
        let lines = vec![
            field_line("id:", self.form.id.clone(), self.form.field == 0),
            field_line("tags:", self.form.tags.clone(), self.form.field == 1),
            field_line("policy:", self.form.policy.clone(), self.form.field == 2),
            field_line(
                "visibility:",
                self.form.visibility().to_string(),
                self.form.field == 3,
            ),
            Line::from(Span::styled(
                "tags: comma-separated identities (no '*'); policy: optional, lets you submit \
                 to the room; visibility: space/←→ to toggle",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let form = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New room (Enter to create) "),
        );
        frame.render_widget(form, area);
    }

    fn draw_table(&self, frame: &mut Frame, area: Rect, rooms: &[DiscoveredRoom]) {
        let header = Row::new(vec![
            Cell::from("Room ID"),
            Cell::from("Visibility"),
            Cell::from("Tags"),
            Cell::from("Policy"),
            Cell::from("Fill"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        let visible = area.height.saturating_sub(3) as usize;
        let rows: Vec<Row> = rooms
            .iter()
            .enumerate()
            .skip(self.list_state.scroll_offset)
            .take(visible.max(1))
            .map(|(i, room)| {
                let style = if i == self.list_state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(truncate(&room.id, 24)),
                    Cell::from(room.visibility.clone()),
                    Cell::from(truncate(&room.tags.join(", "), 26)),
                    Cell::from(truncate(room.policy.as_deref().unwrap_or("—"), 18)),
                    fill_cell(room.eligible_agent_count, room.desired_agents),
                ])
                .style(style)
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(26),
                Constraint::Length(11),
                Constraint::Min(18),
                Constraint::Length(20),
                Constraint::Length(9),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Rooms ({}) ", rooms.len())),
        );
        frame.render_widget(table, area);
    }
}

/// Pure fill computation: returns the `eligible/desired` (or bare `eligible`)
/// label and whether the panel is filled — `Some(true/false)` against a policy
/// target, `None` when no policy is bound.
fn fill_status(eligible: usize, desired: Option<usize>) -> (String, Option<bool>) {
    match desired {
        Some(d) => (format!("{eligible}/{d}"), Some(eligible >= d)),
        None => (eligible.to_string(), None),
    }
}

/// Panel fill indicator: `eligible/desired` with a ✓ when the room can field
/// its panel, ✗ when short. When no policy is bound (`desired` is `None`) there
/// is no target — show the bare eligible count.
fn fill_cell(eligible: usize, desired: Option<usize>) -> Cell<'static> {
    let (label, ok) = fill_status(eligible, desired);
    match ok {
        Some(ok) => {
            let glyph = if ok { '✓' } else { '✗' };
            let color = if ok { Color::Green } else { Color::Red };
            Cell::from(format!("{label} {glyph}")).style(Style::default().fg(color))
        }
        None => Cell::from(label),
    }
}

/// Right-side detail for the selected room: policy, panel fill, tags, and the
/// concrete agent ids that would serve it.
fn draw_room_detail(frame: &mut Frame, area: Rect, room: &DiscoveredRoom) {
    let label = Style::default().fg(Color::Cyan);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("policy:  ", label),
            Span::raw(room.policy.as_deref().unwrap_or("— (none)").to_string()),
        ]),
        Line::from(vec![
            Span::styled("fill:    ", label),
            match fill_status(room.eligible_agent_count, room.desired_agents) {
                (label, Some(ok)) => Span::styled(
                    format!("{label} {}", if ok { '✓' } else { '✗' }),
                    Style::default().fg(if ok { Color::Green } else { Color::Red }),
                ),
                (label, None) => Span::raw(format!("{label} (no policy target)")),
            },
        ]),
        Line::from(vec![
            Span::styled("tags:    ", label),
            Span::raw(if room.tags.is_empty() {
                "—".to_string()
            } else {
                room.tags.join(", ")
            }),
        ]),
        Line::from(Span::styled(
            format!("agents ({}):", room.eligible_agent_count),
            label,
        )),
    ];
    if room.eligible_agent_ids.is_empty() {
        lines.push(Line::from(Span::styled(
            "  none eligible",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for id in &room.eligible_agent_ids {
            lines.push(Line::from(format!("  • {}", truncate(id, 28))));
        }
    }
    let panel = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Detail "))
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(panel, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn typ(c: char) -> AppEvent {
        key(KeyCode::Char(c))
    }

    fn sample() -> Vec<DiscoveredRoom> {
        vec![
            DiscoveredRoom {
                id: "noosphera".into(),
                tags: vec!["noosphera:0v1".into()],
                visibility: "public".into(),
                eligible_agent_count: 2,
                eligible_agent_ids: vec!["alice".into(), "bob".into()],
                policy: Some("noosphera:0v1".into()),
                desired_agents: Some(3),
            },
            DiscoveredRoom {
                id: "test-room".into(),
                tags: vec!["test:0v1".into()],
                visibility: "private".into(),
                eligible_agent_count: 0,
                eligible_agent_ids: vec![],
                policy: None,
                desired_agents: None,
            },
        ]
    }

    #[test]
    fn on_enter_fetches_rooms() {
        let mut v = RoomsView::new("orch".into());
        let actions = v.on_enter();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ViewAction::Fetch(FetchRequest::Rooms { orchestrator }) if orchestrator == "orch"
        ));
    }

    #[test]
    fn rooms_loaded_populates() {
        let mut v = RoomsView::new("orch".into());
        v.update(&AppEvent::Data(DataEvent::RoomsLoaded {
            orchestrator: "orch".into(),
            rooms: sample(),
        }));
        assert!(matches!(v.rooms, LoadState::Loaded(_)));
        assert_eq!(v.list_state.count, 2);
    }

    #[test]
    fn n_opens_create_then_type_and_submit() {
        let mut v = RoomsView::new("orch".into());
        v.rooms = LoadState::Loaded(sample());
        v.update(&typ('n'));
        assert_eq!(v.mode, Mode::Create);
        // Type an id.
        for c in "pl-test".chars() {
            v.update(&typ(c));
        }
        // Tab to tags, type a tag.
        v.update(&key(KeyCode::Tab));
        for c in "pl:test:0v1".chars() {
            v.update(&typ(c));
        }
        // Tab to policy, type a policy label.
        v.update(&key(KeyCode::Tab));
        for c in "noosphera:0v1".chars() {
            v.update(&typ(c));
        }
        // Tab to visibility, toggle to public.
        v.update(&key(KeyCode::Tab));
        v.update(&typ(' '));
        let action = v.update(&key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::Fetch(FetchRequest::CreateRoom {
                orchestrator: "orch".into(),
                id: "pl-test".into(),
                tags: vec!["pl:test:0v1".into()],
                visibility: "public".into(),
                policy: Some("noosphera:0v1".into()),
            }))
        );
        // Form stays open until the server confirms (RoomMutated); a
        // failure would surface in `last_error` instead.
        assert_eq!(v.mode, Mode::Create);

        // Success closes the form, clears any error, and refetches.
        let after = v.update(&AppEvent::Data(DataEvent::RoomMutated {
            orchestrator: "orch".into(),
            action: "created".into(),
            id: "pl-test".into(),
        }));
        assert_eq!(v.mode, Mode::List);
        assert!(v.last_error.is_none());
        assert!(matches!(
            after,
            Some(ViewAction::Fetch(FetchRequest::Rooms { .. }))
        ));
    }

    #[test]
    fn create_empty_id_sets_error_not_submits() {
        let mut v = RoomsView::new("orch".into());
        v.mode = Mode::Create;
        let action = v.update(&key(KeyCode::Enter));
        assert!(action.is_none());
        assert_eq!(v.mode, Mode::Create);
        assert!(v.last_error.is_some());
    }

    #[test]
    fn fetch_error_sets_persistent_banner_and_keeps_form() {
        let mut v = RoomsView::new("orch".into());
        v.mode = Mode::Create;
        let action = v.update(&AppEvent::Data(DataEvent::FetchError {
            context: "rooms".into(),
            error: "server returned 403: room tags outside your grant scope".into(),
        }));
        assert!(action.is_none());
        assert_eq!(v.mode, Mode::Create); // form stays open to edit + retry
        assert!(v.last_error.as_deref().unwrap().contains("403"));
    }

    #[test]
    fn d_confirms_then_y_deletes() {
        let mut v = RoomsView::new("orch".into());
        v.rooms = LoadState::Loaded(sample());
        v.list_state.set_count(2);
        v.update(&typ('d'));
        assert_eq!(v.mode, Mode::ConfirmDelete("noosphera".into()));
        let action = v.update(&typ('y'));
        assert_eq!(
            action,
            Some(ViewAction::Fetch(FetchRequest::DeleteRoom {
                orchestrator: "orch".into(),
                id: "noosphera".into(),
            }))
        );
        assert_eq!(v.mode, Mode::List);
    }

    #[test]
    fn confirm_delete_n_cancels() {
        let mut v = RoomsView::new("orch".into());
        v.mode = Mode::ConfirmDelete("x".into());
        let action = v.update(&typ('n'));
        assert!(action.is_none());
        assert_eq!(v.mode, Mode::List);
    }

    #[test]
    fn mutation_triggers_refetch() {
        let mut v = RoomsView::new("orch".into());
        let action = v.update(&AppEvent::Data(DataEvent::RoomMutated {
            orchestrator: "orch".into(),
            action: "created".into(),
            id: "x".into(),
        }));
        assert!(matches!(
            action,
            Some(ViewAction::Fetch(FetchRequest::Rooms { .. }))
        ));
    }

    #[test]
    fn escape_pops_from_list() {
        let mut v = RoomsView::new("orch".into());
        assert_eq!(v.update(&key(KeyCode::Esc)), Some(ViewAction::Pop));
    }

    #[test]
    fn escape_cancels_create_without_pop() {
        let mut v = RoomsView::new("orch".into());
        v.mode = Mode::Create;
        v.form.id = "abc".into();
        let action = v.update(&key(KeyCode::Esc));
        assert!(action.is_none());
        assert_eq!(v.mode, Mode::List);
        assert!(v.form.id.is_empty());
    }

    #[test]
    fn fill_status_filled_when_eligible_meets_target() {
        assert_eq!(fill_status(3, Some(3)), ("3/3".to_string(), Some(true)));
        assert_eq!(fill_status(5, Some(3)), ("5/3".to_string(), Some(true)));
    }

    #[test]
    fn fill_status_short_when_below_target() {
        assert_eq!(fill_status(1, Some(3)), ("1/3".to_string(), Some(false)));
        assert_eq!(fill_status(0, Some(2)), ("0/2".to_string(), Some(false)));
    }

    #[test]
    fn fill_status_no_target_when_no_policy() {
        assert_eq!(fill_status(2, None), ("2".to_string(), None));
    }

    #[test]
    fn draw_with_detail_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = RoomsView::new("orch".into());
        v.rooms = LoadState::Loaded(sample());
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| v.draw(frame, frame.area())).unwrap();
    }
}
