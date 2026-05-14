use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Compute a centered rectangle within `area` using percentage dimensions.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vert = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vert[1])[1]
}

/// Truncate a string to `max_len` characters, adding "…" if truncated.
pub fn truncate(s: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_len {
        s.to_string()
    } else if max_len == 1 {
        "…".to_string()
    } else {
        let mut result: String = chars[..max_len - 1].iter().collect();
        result.push('…');
        result
    }
}

/// Render a status badge (green ✓ or red ✗).
pub fn status_badge(online: bool) -> Span<'static> {
    if online {
        Span::styled("✓", Style::default().fg(Color::Green))
    } else {
        Span::styled("✗", Style::default().fg(Color::Red))
    }
}

/// Render a key-hint bar at the bottom of a view.
pub fn render_key_hints(frame: &mut Frame, area: Rect, hints: &[(&str, &str)]) {
    let spans: Vec<Span> = hints
        .iter()
        .enumerate()
        .flat_map(|(i, (key, desc))| {
            let mut v = vec![
                Span::styled(
                    format!(" {key} "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {desc} ")),
            ];
            if i < hints.len() - 1 {
                v.push(Span::raw("│"));
            }
            v
        })
        .collect();

    let paragraph = Paragraph::new(Line::from(spans));
    frame.render_widget(paragraph, area);
}

/// Render a loading indicator centered in the given area.
pub fn render_loading(frame: &mut Frame, area: Rect, message: &str) {
    let block = Block::default().borders(Borders::NONE);
    let text = Paragraph::new(format!("⟳ {message}"))
        .style(Style::default().fg(Color::Yellow))
        .block(block);
    let centered = centered_rect(60, 20, area);
    frame.render_widget(text, centered);
}

/// Render an error message centered in the given area.
pub fn render_error(frame: &mut Frame, area: Rect, message: &str) {
    let text = Paragraph::new(format!("✗ {message}")).style(Style::default().fg(Color::Red));
    let centered = centered_rect(80, 20, area);
    frame.render_widget(text, centered);
}

/// Selection state for list-based views.
#[derive(Debug, Clone)]
pub struct ListState {
    pub selected: usize,
    pub count: usize,
    pub scroll_offset: usize,
    /// Last known visible height, updated at draw time.
    visible_height: usize,
}

impl ListState {
    pub fn new(count: usize) -> Self {
        Self {
            selected: 0,
            count,
            scroll_offset: 0,
            visible_height: usize::MAX,
        }
    }

    /// Move selection up, wrapping at the top.
    pub fn up(&mut self) {
        if self.count == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = self.count - 1;
        } else {
            self.selected -= 1;
        }
        self.adjust_scroll();
    }

    /// Move selection down, wrapping at the bottom.
    pub fn down(&mut self) {
        if self.count == 0 {
            return;
        }
        self.selected = (self.selected + 1) % self.count;
        self.adjust_scroll();
    }

    /// Update the item count and clamp selection and scroll offset.
    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        if count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            if self.selected >= count {
                self.selected = count - 1;
            }
            if self.scroll_offset > self.selected {
                self.scroll_offset = self.selected;
            }
        }
    }

    /// Record the visible height (call at draw time) and clamp scroll offset.
    pub fn set_visible_height(&mut self, visible_height: usize) {
        self.visible_height = visible_height;
        self.adjust_scroll();
    }

    fn adjust_scroll(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        if self.visible_height > 0 && self.selected >= self.scroll_offset + self.visible_height {
            self.scroll_offset = self.selected - self.visible_height + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 8), "hello w…");
    }

    #[test]
    fn truncate_one() {
        assert_eq!(truncate("hello", 1), "…");
    }

    #[test]
    fn truncate_zero() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_unicode() {
        assert_eq!(truncate("日本語テスト", 4), "日本語…");
    }

    #[test]
    fn list_state_up_down_wrap() {
        let mut state = ListState::new(3);
        assert_eq!(state.selected, 0);

        state.down();
        assert_eq!(state.selected, 1);

        state.down();
        assert_eq!(state.selected, 2);

        state.down(); // wraps
        assert_eq!(state.selected, 0);

        state.up(); // wraps
        assert_eq!(state.selected, 2);

        state.up();
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn list_state_empty() {
        let mut state = ListState::new(0);
        state.up();
        assert_eq!(state.selected, 0);
        state.down();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn list_state_set_count_clamp() {
        let mut state = ListState::new(5);
        state.selected = 4;
        state.set_count(3);
        assert_eq!(state.selected, 2);

        state.set_count(0);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn list_state_scroll_adjustment() {
        let mut state = ListState::new(20);
        state.set_visible_height(5);

        // Navigate to item 15 — scroll should follow
        for _ in 0..15 {
            state.down();
        }
        assert_eq!(state.selected, 15);
        assert_eq!(state.scroll_offset, 11); // 15 - 5 + 1

        // Navigate back to 0
        for _ in 0..15 {
            state.up();
        }
        assert_eq!(state.selected, 0);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn centered_rect_returns_inner() {
        let area = Rect::new(0, 0, 100, 50);
        let inner = centered_rect(50, 50, area);
        // Inner should be roughly centered
        assert!(inner.x > 0);
        assert!(inner.y > 0);
        assert!(inner.width < area.width);
        assert!(inner.height < area.height);
    }

    #[test]
    fn list_state_set_visible_height_clamps_scroll() {
        let mut state = ListState::new(10);
        // Navigate to item 8 with a visible window of 5
        state.set_visible_height(5);
        for _ in 0..8 {
            state.down();
        }
        assert_eq!(state.selected, 8);
        // scroll_offset = selected - visible + 1 = 8 - 5 + 1 = 4
        assert_eq!(state.scroll_offset, 4);
    }

    #[test]
    fn list_state_wrap_around_adjusts_scroll() {
        let mut state = ListState::new(10);
        state.set_visible_height(3);
        // Navigate down to item 9
        for _ in 0..9 {
            state.down();
        }
        assert_eq!(state.selected, 9);
        assert_eq!(state.scroll_offset, 7); // 9 - 3 + 1

        // Wrap around to 0
        state.down();
        assert_eq!(state.selected, 0);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn list_state_up_wrap_adjusts_scroll() {
        let mut state = ListState::new(5);
        state.set_visible_height(3);
        // At 0, go up → wraps to 4
        state.up();
        assert_eq!(state.selected, 4);
        assert_eq!(state.scroll_offset, 2); // 4 - 3 + 1
    }

    #[test]
    fn list_state_set_count_clamps_scroll_offset() {
        let mut state = ListState::new(10);
        state.selected = 7;
        state.scroll_offset = 5;
        state.set_count(3);
        // selected clamped to 2, scroll_offset clamped to 2
        assert_eq!(state.selected, 2);
        assert!(state.scroll_offset <= state.selected);
    }

    #[test]
    fn status_badge_online_and_offline() {
        let online = status_badge(true);
        assert_eq!(online.content.as_ref(), "✓");
        let offline = status_badge(false);
        assert_eq!(offline.content.as_ref(), "✗");
    }
}
