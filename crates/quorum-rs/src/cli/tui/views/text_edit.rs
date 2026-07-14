//! Shared single-buffer text-editing primitives — cursor motion, word/char
//! boundaries, wrapped-row counting, and caret rendering. Reused by every
//! text input (the thread compose box and the deliberation steering input) so
//! line editing behaves identically everywhere.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

/// Previous UTF-8 char boundary before byte offset `i` (0 at the start).
pub(crate) fn prev_char(s: &str, i: usize) -> usize {
    s[..i.min(s.len())]
        .char_indices()
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}

/// Next UTF-8 char boundary after byte offset `i` (len at the end).
pub(crate) fn next_char(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    s[i..]
        .char_indices()
        .nth(1)
        .map(|(j, _)| i + j)
        .unwrap_or(s.len())
}

/// Start of the word before byte offset `i` (skips trailing whitespace first).
pub(crate) fn prev_word_boundary(s: &str, i: usize) -> usize {
    let head = &s[..i.min(s.len())];
    let trimmed = head.trim_end_matches(char::is_whitespace);
    trimmed
        .rfind(char::is_whitespace)
        .map(|j| j + 1)
        .unwrap_or(0)
}

/// Start of the word after byte offset `i` (skips leading whitespace first).
pub(crate) fn next_word_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let rest = &s[i..];
    let ws = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
    let after_ws = i + ws;
    s[after_ws..]
        .find(char::is_whitespace)
        .map(|j| after_ws + j)
        .unwrap_or(s.len())
}

/// How many rows `text` occupies when wrapped to `width` columns (char-based,
/// matching ratatui's `Wrap`): each source line takes `ceil(chars / width)`
/// rows, at least one. Used to size + scroll a wrapped input.
pub(crate) fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut rows: usize = 0;
    for line in text.split('\n') {
        let chars = line.chars().count();
        rows += chars.div_ceil(w).max(1);
    }
    rows.max(1).min(u16::MAX as usize) as u16
}

/// Rows an inline wrapped input should occupy: its wrapped height, clamped to
/// `[1, max]`. Used to grow a steering / compose box as the message lengthens.
pub(crate) fn input_box_rows(text: &str, width: u16, max: u16) -> u16 {
    wrapped_line_count(text, width).clamp(1, max.max(1))
}

/// Vertical scroll to keep the tail (caret) of a wrapped input visible within
/// `visible_rows`: scroll past the overflow, `0` when it all fits.
pub(crate) fn input_scroll(text: &str, width: u16, visible_rows: u16) -> u16 {
    wrapped_line_count(text, width).saturating_sub(visible_rows.max(1))
}

/// Render `text` as multi-line `Text` with a reversed caret block at byte
/// offset `cursor` — a block over the char under the caret, or a trailing
/// block at end-of-line.
pub(crate) fn caret_text(text: &str, cursor: usize) -> Text<'static> {
    let cur = cursor.min(text.len());
    let mut lines = Vec::new();
    let mut line_start = 0usize;
    for seg in text.split('\n') {
        let seg_end = line_start + seg.len();
        if cur >= line_start && cur <= seg_end {
            lines.push(caret_line_from(seg, cur - line_start));
        } else {
            lines.push(Line::from(seg.to_string()));
        }
        line_start = seg_end + 1; // skip the '\n'
    }
    Text::from(lines)
}

/// One line with a reversed caret block at byte column `col`.
fn caret_line_from(seg: &str, col: usize) -> Line<'static> {
    let col = col.min(seg.len());
    let before = seg[..col].to_string();
    let at = seg[col..].chars().next();
    let caret = Style::default().add_modifier(Modifier::REVERSED);
    let (at_span, after) = match at {
        Some(c) => (
            Span::styled(c.to_string(), caret),
            seg[col + c.len_utf8()..].to_string(),
        ),
        None => (Span::styled(" ", caret), String::new()),
    };
    Line::from(vec![Span::raw(before), at_span, Span::raw(after)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_boundaries_walk_utf8() {
        let s = "a�b"; // multi-byte middle char
        assert_eq!(prev_char(s, s.len()), s.len() - 1);
        assert_eq!(next_char("ab", 0), 1);
        assert_eq!(next_char("ab", 2), 2); // clamped at end
        assert_eq!(prev_char("ab", 0), 0); // clamped at start
    }

    #[test]
    fn word_boundaries_jump_over_whitespace() {
        let s = "hello world foo";
        assert_eq!(&s[prev_word_boundary(s, s.len())..], "foo");
        assert_eq!(&s[..next_word_boundary(s, 0)], "hello");
    }

    #[test]
    fn wrapped_line_count_counts_wraps_and_newlines() {
        assert_eq!(wrapped_line_count("", 10), 1);
        assert_eq!(wrapped_line_count("abcdefghij", 5), 2); // 10 / 5
        assert_eq!(wrapped_line_count("abcdefghijkl", 5), 3); // ceil(12/5)
        assert_eq!(wrapped_line_count("a\nb\nc", 10), 3);
    }

    #[test]
    fn input_box_grows_and_scrolls_to_the_tail() {
        // Grows with content, capped at max.
        assert_eq!(input_box_rows("abc", 10, 6), 1);
        assert_eq!(input_box_rows("abcdefghijkl", 5, 6), 3); // ceil(12/5)
        assert_eq!(input_box_rows(&"x".repeat(100), 5, 6), 6); // capped
        // Fits → no scroll; overflows → scroll past the hidden head so the tail shows.
        assert_eq!(input_scroll("abc", 10, 4), 0);
        assert_eq!(input_scroll(&"x".repeat(100), 5, 4), 20 - 4); // 20 rows, show last 4
    }

    #[test]
    fn caret_text_marks_the_cursor_line() {
        let t = caret_text("ab\ncd", 4); // 2nd line, before 'd'
        assert_eq!(t.lines.len(), 2);
        let caret = t.lines[1]
            .spans
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .expect("a reversed caret block");
        assert_eq!(caret.content.as_ref(), "d");
    }
}
