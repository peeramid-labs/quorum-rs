//! Thread list — the "Threads" tab landing view.
//!
//! Lists stored threads (newest first) so the user can resume one or start a
//! new one. Selecting a thread opens it by id; `n` opens a fresh thread.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use super::common::{ListState, render_key_hints};
use super::{View, ViewAction};
use crate::cli::thread::{Thread, ThreadStore};
use crate::cli::tui::app::ViewId;
use crate::cli::tui::event::{self, AppEvent};

/// Lists stored threads for resume / new.
pub struct ThreadListView {
    store: ThreadStore,
    threads: Vec<Thread>,
    list_state: ListState,
    /// `true` after `d` — the next key confirms (`d`/`y`) or cancels the delete
    /// of the selected thread. Guards a destructive, single-copy removal.
    pending_delete: bool,
}

impl ThreadListView {
    pub fn new(store: ThreadStore) -> Self {
        let threads = store.list();
        let list_state = ListState::new(threads.len());
        Self {
            store,
            threads,
            list_state,
            pending_delete: false,
        }
    }

    fn reload(&mut self) {
        self.threads = self.store.list();
        self.list_state.set_count(self.threads.len());
    }

    /// Inbox row for a thread: last-activity time, subject, a preview of the
    /// newest turn, and the message count — newest-activity threads sort first.
    fn row(t: &Thread) -> String {
        use super::thread::{display_role, first_line, fmt_ts};
        let n = t.messages.len();
        match t.tip() {
            Some(tip) => format!(
                "{} {}  ·  [{}]: {}  ({n})",
                fmt_ts(tip.ts),
                t.subject,
                display_role(&tip.role),
                first_line(&tip.content, 44),
            ),
            None => format!("{}  (empty)", t.subject),
        }
    }

    /// Up to `max` newest turns of a thread, as collapsed preview lines for the
    /// peek pane — the same `[ts] [role] preview` shape the reader uses.
    fn peek_lines(t: &Thread, max: usize) -> Vec<Line<'static>> {
        use super::thread::{display_role, first_line, fmt_ts};
        t.messages
            .iter()
            .rev()
            .take(max)
            .map(|m| {
                Line::from(format!(
                    " {} [{}] {}",
                    fmt_ts(m.ts),
                    display_role(&m.role),
                    first_line(&m.content, 64),
                ))
            })
            .collect()
    }
}

impl View for ThreadListView {
    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        let AppEvent::Terminal(event) = app_event else {
            return None;
        };
        // Delete confirm: after `d`, the next key confirms (d/y) or cancels.
        if self.pending_delete {
            if (event::is_key(event, 'd') || event::is_key(event, 'y'))
                && let Some(t) = self.threads.get(self.list_state.selected)
            {
                let id = t.id.clone();
                self.store.delete(&id);
                self.reload();
            }
            self.pending_delete = false;
            return None;
        }
        if event::is_escape(event) || event::is_key(event, 'q') {
            return Some(ViewAction::Quit);
        }
        if event::is_key(event, 'd') {
            if !self.threads.is_empty() {
                self.pending_delete = true;
            }
            return None;
        }
        if event::is_up(event) {
            self.list_state.up();
            return None;
        }
        if event::is_down(event) {
            self.list_state.down();
            return None;
        }
        if event::is_key(event, 'n') {
            return Some(ViewAction::Push(ViewId::Thread { id: None }));
        }
        if event::is_enter(event) {
            let selected = self.threads.get(self.list_state.selected)?;
            return Some(ViewAction::Push(ViewId::Thread {
                id: Some(selected.id.clone()),
            }));
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Inbox list on top, a live peek of the selected thread's newest turns
        // below (one screen — see the latest without opening), then the hints.
        let sel = self.list_state.selected;
        let peek_h: u16 = if self
            .threads
            .get(sel)
            .is_some_and(|t| !t.messages.is_empty())
        {
            8
        } else {
            0
        };
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(peek_h),
            Constraint::Length(1),
        ])
        .split(area);
        let visible = chunks[0].height.saturating_sub(2) as usize;
        self.list_state.set_visible_height(visible);

        if self.threads.is_empty() {
            let empty = List::new(vec![ListItem::new(Line::from(Span::styled(
                "No threads yet — press n to start one.",
                Style::default().fg(Color::DarkGray),
            )))])
            .block(Block::default().borders(Borders::ALL).title(" Threads "));
            frame.render_widget(empty, chunks[0]);
        } else {
            let items: Vec<ListItem> = self
                .threads
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let selected = i == self.list_state.selected;
                    let style = if selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let prefix = if selected { "▸ " } else { "  " };
                    ListItem::new(Line::from(Span::styled(
                        format!("{prefix}{}", Self::row(t)),
                        style,
                    )))
                })
                .collect();
            let list =
                List::new(items).block(Block::default().borders(Borders::ALL).title(" Threads "));
            frame.render_widget(list, chunks[0]);
        }

        // Peek pane: the selected thread's newest turns, so the latest exchange
        // is visible without opening the thread.
        if peek_h > 0
            && let Some(t) = self.threads.get(sel)
        {
            let lines = Self::peek_lines(t, peek_h.saturating_sub(2) as usize);
            let peek = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Preview · {} ", t.subject)),
            );
            frame.render_widget(peek, chunks[1]);
        }

        if self.pending_delete {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " Delete this thread? d/y confirm · any other key cancels ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ))),
                chunks[2],
            );
        } else {
            render_key_hints(
                frame,
                chunks[2],
                &[
                    ("↑↓", "Nav"),
                    ("Enter", "Open"),
                    ("n", "New"),
                    ("d", "Delete"),
                ],
            );
        }
    }

    fn on_enter(&mut self) -> Vec<ViewAction> {
        // Pick up threads created / updated since the view was built.
        self.reload();
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::thread::{Message, Thread};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Terminal(Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn store_with(n: usize) -> (tempfile::TempDir, ThreadStore, Vec<String>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut ids = Vec::new();
        for i in 0..n {
            let mut t = Thread::new(format!("thread {i}"));
            t.updated = 100 + i as i64; // deterministic newest-first order
            store.save(&t).unwrap();
            ids.push(t.id);
        }
        (tmp, store, ids)
    }

    #[test]
    fn enter_opens_selected_thread_by_id() {
        let (_tmp, store, ids) = store_with(2);
        let mut view = ThreadListView::new(store);
        // Newest first: ids[1] (updated 101) is row 0.
        let action = view.update(&key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::Push(ViewId::Thread {
                id: Some(ids[1].clone())
            }))
        );
    }

    #[test]
    fn down_then_enter_opens_second_thread() {
        let (_tmp, store, ids) = store_with(2);
        let mut view = ThreadListView::new(store);
        view.update(&key(KeyCode::Down));
        let action = view.update(&key(KeyCode::Enter));
        assert_eq!(
            action,
            Some(ViewAction::Push(ViewId::Thread {
                id: Some(ids[0].clone())
            }))
        );
    }

    #[test]
    fn n_opens_a_fresh_thread() {
        let (_tmp, store, _) = store_with(1);
        let mut view = ThreadListView::new(store);
        assert_eq!(
            view.update(&key(KeyCode::Char('n'))),
            Some(ViewAction::Push(ViewId::Thread { id: None }))
        );
    }

    #[test]
    fn escape_quits_from_the_inbox_root() {
        let (_tmp, store, _) = store_with(0);
        let mut view = ThreadListView::new(store);
        assert_eq!(view.update(&key(KeyCode::Esc)), Some(ViewAction::Quit));
        assert_eq!(
            view.update(&key(KeyCode::Char('q'))),
            Some(ViewAction::Quit)
        );
    }

    #[test]
    fn enter_on_empty_list_is_noop() {
        let (_tmp, store, _) = store_with(0);
        let mut view = ThreadListView::new(store);
        assert_eq!(view.update(&key(KeyCode::Enter)), None);
    }

    #[test]
    fn row_is_an_inbox_line_with_subject_last_turn_and_count() {
        let mut t = Thread::new("audit");
        t.push_message(Message::now("user", "q1"));
        t.push_message(Message::now("assistant", "a1"));
        t.push_message(Message::now("user", "the latest question"));
        let row = ThreadListView::row(&t);
        assert!(row.contains("audit"), "{row}");
        assert!(row.contains("[you]: the latest question"), "{row}"); // newest turn preview
        assert!(row.contains("(3)"), "{row}"); // message count
        assert!(row.starts_with('['), "leads with a timestamp: {row}");
    }

    #[test]
    fn d_then_confirm_deletes_the_selected_thread() {
        let (_tmp, store, _ids) = store_with(2);
        let mut v = ThreadListView::new(store);
        assert_eq!(v.threads.len(), 2);
        v.update(&key(KeyCode::Char('d')));
        assert!(v.pending_delete, "first d arms the confirm");
        v.update(&key(KeyCode::Char('d')));
        assert!(!v.pending_delete);
        assert_eq!(v.threads.len(), 1, "confirmed delete + reload");
    }

    #[test]
    fn d_then_other_key_cancels_the_delete() {
        let (_tmp, store, _ids) = store_with(2);
        let mut v = ThreadListView::new(store);
        v.update(&key(KeyCode::Char('d')));
        assert!(v.pending_delete);
        v.update(&key(KeyCode::Down)); // any non-confirm key cancels
        assert!(!v.pending_delete);
        assert_eq!(v.threads.len(), 2, "nothing deleted");
    }

    #[test]
    fn peek_lines_show_newest_turns_capped() {
        let mut t = Thread::new("s");
        for i in 0..5 {
            t.push_message(Message::now("user", format!("msg{i}")));
        }
        let lines = ThreadListView::peek_lines(&t, 3);
        assert_eq!(lines.len(), 3, "capped to max");
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text[0].contains("msg4"), "newest first: {text:?}");
        assert!(text[2].contains("msg2"));
        assert!(text[0].contains("[you]"));
    }
}
