//! Thread view — an email-style reader + separate compose box.
//!
//! Read mode shows a [`Thread`]'s messages newest-first and scrollable; `w` opens
//! a compose box (Subject + Message for a new thread, a reply box for an existing
//! one). Sending assembles the prior messages + the new one
//! ([`Thread::to_deliberation_query`]) into one deliberation via
//! [`ViewAction::LaunchJob`]; the running job streams in the job-detail view
//! (`^D`), and the reply is recorded back into the thread on completion (a reply
//! arriving mid-scroll shows a CTA rather than yanking the view). `/model` / `^P`
//! swap the thread's model (policy); `^E` cycles effort; both show in the footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::{FetchRequest, StatusLevel, View, ViewAction};
use crate::cli::thread::{Message, Thread, ThreadStore};
use crate::cli::tui::event::{self, AppEvent};

/// Which compose field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Subject,
    Message,
}

/// The thread view is either reading the transcript or composing a turn. Compose
/// is a separate box you open (`w`) — reading isn't cluttered by an input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Read,
    Compose,
}

/// An agent's live `ask_user` question awaiting the operator's answer. Time-boxed
/// server-side to the round budget — if unanswered it expires and the agent
/// proceeds, and the pending state clears on the `tool_call_expired` event.
#[derive(Clone, Debug)]
struct AskQuestion {
    job_id: String,
    call_id: String,
    question: String,
    options: Vec<String>,
    /// Selected row: `0..options.len()` picks an option; `options.len()` is the
    /// "type your own" row.
    selected: usize,
    /// Free-text entry (auto-on when there are no options).
    typing: bool,
    answer: String,
}

impl AskQuestion {
    fn from_pending(
        job_id: String,
        call_id: String,
        arguments: &serde_json::Value,
    ) -> Option<Self> {
        let question = arguments.get("question")?.as_str()?.to_string();
        let options: Vec<String> = arguments
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let typing = options.is_empty();
        Some(Self {
            job_id,
            call_id,
            question,
            options,
            selected: 0,
            typing,
            answer: String::new(),
        })
    }
}

/// Interactive view of a single thread — an email-style compose (Subject +
/// Message, `Tab` between them) over the deliberation transcript.
pub struct ThreadView {
    thread: Thread,
    store: ThreadStore,
    orchestrator: String,
    /// Selectable models (policy names / ids) for the `/model` picker.
    models: Vec<String>,
    /// `Some(i)` while the model picker is open, with row `i` highlighted.
    picking: Option<usize>,
    subject_input: String,
    message_input: String,
    focus: Focus,
    /// Effort (convergence threshold) override for this thread, cycled with
    /// `Ctrl-E`. `None` uses the policy's default.
    effort: Option<f32>,
    /// Transcript selection cursor (message index) for opening a past turn's
    /// deliberation detail. `None` = nothing selected (compose is the focus).
    selected: Option<usize>,
    /// The node a composed reply roots under, captured from `selected` when the
    /// compose box opens (so browsing away doesn't move the target). `None`
    /// continues from the tip. Message id, not a row.
    reply_to: Option<String>,
    /// `true` while composing a NEW ROOT turn (`n`) — a fresh line under the same
    /// subject, rooted at nothing (its own branch), rather than a reply.
    compose_root: bool,
    /// Read (browse transcript) vs Compose (separate input box).
    mode: Mode,
    /// Transcript scroll offset in lines from the top (newest-first order). 0 =
    /// pinned to the newest. Advanced by PgUp/PgDn/Home/End; a new reply while
    /// scrolled-in keeps this fixed (never yanks the reader).
    scroll: u16,
    /// A reply landed while the reader was scrolled into history — surfaces a
    /// "new reply" CTA instead of yanking the view. Cleared on Home / scroll-top.
    unseen_reply: bool,
    /// Set when this session launched a turn and no reply has landed yet — drives
    /// the "deliberating…" state without relying on `pending_job` (which the
    /// loop only writes to the store, not back into this view). Cleared when a
    /// reply arrives; a reopened thread relies on `pending_job` instead, so a
    /// dead/unstuck thread (pending cleared, last turn a bare user message) no
    /// longer shows a phantom "deliberating…".
    sent_awaiting: bool,
    /// Transcript viewport height in lines, recorded on draw so `update` can
    /// scroll to keep the ↑↓ selection visible.
    view_h: u16,
    /// Large pastes held as `(placeholder, full_content)`: the placeholder shows
    /// in the compose box (Claude-style), the full content is expanded on send.
    /// Cleared with the message.
    pastes: Vec<(String, String)>,
    /// Insertion cursor: a byte offset into the focused compose buffer. Moved by
    /// ←/→ (char), Ctrl+←/→ (word), Home/End; reset to the end on focus change.
    cursor: usize,
    /// Ids of messages expanded to full content in the reader. Everything else
    /// shows a one-line preview (newest-on-top, email-style). Toggled with
    /// Enter/→ (expand) and ← (collapse) on the selected row.
    expanded: std::collections::HashSet<String>,
    /// Live `ask_user` questions (Claude-style), shown as a modal one at a time.
    /// Distinct concurrent questions stack here (front is shown); answering /
    /// skipping / resolving pops that one and reveals the next. Server-side
    /// moderator dedup collapses *same* questions before they reach here.
    question_queue: std::collections::VecDeque<AskQuestion>,
    /// `Some(id)` while a single message is open full-screen (`o`); its own
    /// scroll offset is `full_scroll`. Esc returns to the reader.
    full_view: Option<String>,
    full_scroll: u16,
}

impl ThreadView {
    /// Open a specific thread with the models selectable via the picker.
    pub fn with_thread(
        thread: Thread,
        store: ThreadStore,
        orchestrator: String,
        models: Vec<String>,
    ) -> Self {
        let subject_input = thread.subject.clone();
        let new_thread = thread.subject.trim().is_empty();
        let focus = if new_thread {
            Focus::Subject
        } else {
            Focus::Message
        };
        // A brand-new thread opens straight into compose (needs a subject + first
        // message); an existing thread opens in the transcript reader.
        let mode = if new_thread {
            Mode::Compose
        } else {
            Mode::Read
        };
        Self {
            thread,
            store,
            orchestrator,
            models,
            picking: None,
            subject_input,
            message_input: String::new(),
            focus,
            effort: None,
            selected: None,
            reply_to: None,
            compose_root: false,
            mode,
            scroll: 0,
            unseen_reply: false,
            sent_awaiting: false,
            view_h: 0,
            pastes: Vec::new(),
            cursor: 0,
            expanded: std::collections::HashSet::new(),
            question_queue: std::collections::VecDeque::new(),
            full_view: None,
            full_scroll: 0,
        }
    }

    // --- cursor-based compose editing (cursor is a byte offset) -------------

    /// Immutable view of the focused buffer.
    fn focused_buf_ref(&self) -> &str {
        match self.focus {
            Focus::Subject => &self.subject_input,
            Focus::Message => &self.message_input,
        }
    }

    /// Park the cursor at the end of the focused buffer (on focus change/clear).
    fn cursor_to_end(&mut self) {
        self.cursor = self.focused_buf_ref().len();
    }

    /// Insert a character at the cursor.
    fn insert_char(&mut self, c: char) {
        let at = self.cursor;
        self.focused_buf().insert(at, c);
        self.cursor = at + c.len_utf8();
    }

    /// Insert pasted text at the cursor: a small single-line paste goes in
    /// inline; a large or multi-line paste collapses to a `[paste #N: L lines]`
    /// placeholder (shown in the box, expanded on send) so it can't submit
    /// mid-paste.
    fn insert_paste(&mut self, text: String) {
        // Terminals deliver pasted line breaks as CR (or CRLF), not LF, so
        // normalize first — otherwise `contains('\n')` / `lines()` see one line
        // and a multi-line paste collapses to "[paste #N: 1 line]".
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.focus != Focus::Message || (!text.contains('\n') && text.chars().count() <= 120) {
            let at = self.cursor;
            self.focused_buf().insert_str(at, &text);
            self.cursor = at + text.len();
            return;
        }
        let n = self.pastes.len() + 1;
        let lines = text.lines().count().max(1);
        let marker = format!(
            "[paste #{n}: {lines} line{}]",
            if lines == 1 { "" } else { "s" }
        );
        let at = self.cursor;
        self.message_input.insert_str(at, &marker);
        self.cursor = at + marker.len();
        self.pastes.push((marker, text));
    }

    /// If a paste placeholder ends exactly at the cursor, remove it whole.
    fn pop_paste_block(&mut self) -> bool {
        if self.focus != Focus::Message {
            return false;
        }
        let before = &self.message_input[..self.cursor];
        if let Some(idx) = self.pastes.iter().position(|(m, _)| before.ends_with(m)) {
            let start = self.cursor - self.pastes[idx].0.len();
            self.message_input.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.pastes.remove(idx);
            return true;
        }
        false
    }

    /// Backspace: delete a paste block whole, else the char before the cursor.
    fn backspace(&mut self) {
        if self.pop_paste_block() || self.cursor == 0 {
            return;
        }
        let cur = self.cursor;
        let prev = prev_char_boundary(self.focused_buf_ref(), cur);
        self.focused_buf().replace_range(prev..cur, "");
        self.cursor = prev;
    }

    /// Ctrl-W: delete a paste block whole, else the word before the cursor.
    fn delete_word(&mut self) {
        if self.pop_paste_block() {
            return;
        }
        let cur = self.cursor;
        let start = prev_word_boundary(self.focused_buf_ref(), cur);
        self.focused_buf().replace_range(start..cur, "");
        self.cursor = start;
    }

    fn move_left(&mut self) {
        self.cursor = prev_char_boundary(self.focused_buf_ref(), self.cursor);
    }
    fn move_right(&mut self) {
        self.cursor = next_char_boundary(self.focused_buf_ref(), self.cursor);
    }
    fn move_word_left(&mut self) {
        self.cursor = prev_word_boundary(self.focused_buf_ref(), self.cursor);
    }
    fn move_word_right(&mut self) {
        self.cursor = next_word_boundary(self.focused_buf_ref(), self.cursor);
    }

    /// The composed message with paste placeholders expanded to full content.
    fn expanded_message(&self) -> String {
        let mut m = self.message_input.clone();
        for (marker, content) in &self.pastes {
            m = m.replace(marker.as_str(), content);
        }
        m
    }

    /// Persist the in-progress compose text (pastes expanded) onto the thread so
    /// it survives a restart or a failed send. Cleared once the turn is sent.
    fn save_draft(&mut self) {
        let text = self.expanded_message();
        self.thread.draft = if text.trim().is_empty() {
            None
        } else {
            Some(text)
        };
        let _ = self.store.save(&self.thread);
    }

    /// Restore a persisted draft into the compose box (called when the thread is
    /// reopened). The stored draft is already paste-expanded, so it comes back as
    /// plain text with no placeholders.
    fn restore_draft(&mut self) {
        if let Some(draft) = self.thread.draft.clone() {
            self.message_input = draft;
            self.pastes.clear();
            self.cursor = self.message_input.len();
        }
    }

    /// Cycle the effort override: default → 0.3 → 0.6 → 0.9 → default.
    fn next_effort(cur: Option<f32>) -> Option<f32> {
        match cur {
            None => Some(0.3),
            Some(e) if e < 0.45 => Some(0.6),
            Some(e) if e < 0.75 => Some(0.9),
            Some(_) => None,
        }
    }

    /// The compose buffer currently receiving keystrokes.
    fn focused_buf(&mut self) -> &mut String {
        match self.focus {
            Focus::Subject => &mut self.subject_input,
            Focus::Message => &mut self.message_input,
        }
    }

    /// Open the model picker, highlighting the thread's current model.
    fn open_picker(&mut self) {
        let start = self
            .thread
            .active_policy
            .as_deref()
            .and_then(|cur| self.models.iter().position(|m| m == cur))
            .unwrap_or(0);
        self.picking = Some(start);
    }

    // Selection is a VISUAL ROW in the reader's display order: row 0 = the pinned
    // thread root (top), then newest-first descendants below. ↑ moves toward the
    // top, ↓ toward the bottom; each keeps the picked row scrolled into view.

    /// ↑ — select the row above. From nothing, selects the top row (the root).
    fn select_up(&mut self) {
        if self.thread.messages.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None => 0,
            Some(r) => r.saturating_sub(1),
        });
        self.ensure_visible();
    }

    /// ↓ — select the row below (older); stepping past the oldest clears it.
    fn select_down(&mut self) {
        let n = self.thread.messages.len();
        match self.selected {
            Some(r) if r + 1 < n => self.selected = Some(r + 1),
            Some(_) => self.selected = None,
            // From nothing, ↓ selects the top row (symmetric with ↑) so the cursor
            // appears on the first keypress instead of being a no-op.
            None if n > 0 => self.selected = Some(0),
            None => {}
        }
        self.ensure_visible();
    }

    /// Reader row order: each thread root pinned at the top (oldest first), then
    /// its descendants newest-first with every fork's subtree grouped together
    /// and indented. Returns `(message index, fork indent depth)` per visual row.
    /// A post-order walk (descendants before the node) makes the newest turn sit
    /// directly under the root while keeping a fork's subthread contiguous.
    fn display_order(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut roots: Vec<(usize, &Message)> = self
            .thread
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.parent_id.is_none())
            .collect();
        roots.sort_by_key(|(_, m)| m.ts);
        let mut seen = std::collections::HashSet::new();
        for (ri, root) in roots {
            if !seen.insert(root.id.clone()) {
                continue;
            }
            out.push(ri);
            self.emit_descendants(&root.id, &mut out, &mut seen);
        }
        out
    }

    fn emit_descendants(
        &self,
        parent_id: &str,
        out: &mut Vec<usize>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let mut kids: Vec<(usize, &Message)> = self
            .thread
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.parent_id.as_deref() == Some(parent_id))
            .collect();
        // Newest sibling first; break same-second ties by creation index (the
        // later-stored message wins) so the order is stable.
        kids.sort_by_key(|(i, m)| (std::cmp::Reverse(m.ts), std::cmp::Reverse(*i)));
        for (ki, kid) in kids {
            if !seen.insert(kid.id.clone()) {
                continue; // cycle guard
            }
            self.emit_descendants(&kid.id, out, seen); // post-order → newest on top
            out.push(ki);
        }
    }

    /// Message index for a visual row (see [`display_order`]).
    fn row_message(&self, row: usize) -> Option<&Message> {
        let idx = *self.display_order().get(row)?;
        self.thread.messages.get(idx)
    }

    /// First transcript line index of a visual row (sum of the heights of the
    /// rows above it in newest-first order).
    fn row_line_offset(&self, row: usize) -> u16 {
        (0..row).fold(0u16, |off, r| {
            let h = self
                .row_message(r)
                .map(|m| self.message_height(m))
                .unwrap_or(0);
            off.saturating_add(h)
        })
    }

    /// Scroll so the selected row is within the viewport (no-op if unset).
    fn ensure_visible(&mut self) {
        let Some(row) = self.selected else { return };
        let top = self.row_line_offset(row);
        let h = self
            .row_message(row)
            .map(|m| self.message_height(m))
            .unwrap_or(1);
        if top < self.scroll {
            self.scroll = top;
        } else if self.view_h > 0 && top + h > self.scroll + self.view_h {
            self.scroll = (top + h).saturating_sub(self.view_h);
        }
    }

    /// The job id of the currently selected row, if it records one (an assistant
    /// reply from a completed deliberation).
    fn selected_job(&self) -> Option<String> {
        self.selected
            .and_then(|row| self.row_message(row))
            .and_then(|m| m.job_id.clone())
    }

    /// Handle a key while the model picker is open. Returns a status action on
    /// selection, `None` otherwise; sets `picking = None` on select/cancel.
    fn picker_key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        let i = self.picking?;
        if event::is_escape(ev) {
            self.picking = None;
            return None;
        }
        if event::is_up(ev) {
            self.picking = Some(i.saturating_sub(1));
            return None;
        }
        if event::is_down(ev) {
            self.picking = Some((i + 1).min(self.models.len().saturating_sub(1)));
            return None;
        }
        if event::is_enter(ev) {
            self.picking = None;
            let model = self.models.get(i)?.clone();
            self.thread.active_policy = Some(model.clone());
            let _ = self.store.save(&self.thread);
            return Some(ViewAction::SetStatus(
                format!("Model: {model}"),
                StatusLevel::Info,
            ));
        }
        None
    }

    /// True while the last message is a user question still awaiting its reply
    /// — a new submit would race the in-flight deliberation (same thread id →
    /// 409) and orphan the turn, so it is blocked until the reply lands.
    fn awaiting_reply(&self) -> bool {
        // A turn launched this session (reply not yet landed), or a thread
        // reopened with a still-pending job. NOT merely "last turn is a user
        // message" — a dead/unstuck thread has that but isn't deliberating.
        self.sent_awaiting || self.thread.pending_job.is_some()
    }

    /// Whether the thread already has a (now-immutable) subject.
    fn has_subject(&self) -> bool {
        !self.thread.subject.trim().is_empty()
    }

    /// Send the composed message: on the first send the typed subject is set
    /// (immutable thereafter); require a non-empty body; record the user
    /// message, persist, and emit the launch. Guides focus / status when
    /// something's missing rather than silently no-op'ing.
    fn send(&mut self) -> Option<ViewAction> {
        // Subject is set once, on the first send, then fixed for the thread.
        if !self.has_subject() {
            let subject = self.subject_input.trim().to_string();
            if subject.is_empty() {
                self.focus = Focus::Subject;
                return Some(ViewAction::SetStatus(
                    "Add a subject first".into(),
                    StatusLevel::Info,
                ));
            }
            self.thread.subject = subject;
        }
        let msg = self.expanded_message().trim().to_string();
        if msg.is_empty() {
            // Empty Enter on a thread still awaiting a reply = "check for it":
            // the reply may have landed server-side while we weren't watching.
            // A typed message never comes here — it always sends (below).
            if self.awaiting_reply()
                && let Some(job_id) = self.thread.pending_job.clone()
            {
                return Some(ViewAction::Fetch(FetchRequest::ReconcileThread {
                    orchestrator: self.orchestrator.clone(),
                    job_id,
                    thread_id: self.thread.id.clone(),
                }));
            }
            self.focus = Focus::Message;
            return None;
        }
        // A typed turn ALWAYS sends, even if a prior turn is still awaiting its
        // reply (a stopped/lost turn just yields a double-[user] the deliberation
        // reads as context). Never dead-end a filled query on the reconcile path.
        // New-root: root at nothing (fresh line under the subject). Otherwise
        // reply under the captured target (the node the cursor was on), else
        // continue from the tip. The context is the target's root→node path, so
        // a reply under an older node carries only its lineage (a fork).
        let parent_id = if self.compose_root {
            None
        } else {
            self.reply_to
                .clone()
                .or_else(|| self.thread.tip().map(|m| m.id.clone()))
        };
        let task = self
            .thread
            .to_deliberation_query_from(parent_id.as_deref(), &msg);
        // The new turn only — sent so a resumed session's delta prompt carries
        // just this, not the whole flattened `task`. The agent ignores it on a
        // fresh session (uses the full task), so it's always safe to send.
        let new_turn = Some(msg.clone());
        let uid = self.thread.reply(parent_id.as_deref(), "user", msg);
        // The new turn's branch is the per-branch conversation_id (a fork under
        // a non-leaf node got a fresh branch; a leaf reply kept the parent's).
        let conversation_id = self.thread.get(&uid).map(|m| m.branch_id.clone());
        self.thread.draft = None; // the draft became a turn
        let _ = self.store.save(&self.thread);
        let policy = self.thread.active_policy.clone();
        self.message_input.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.selected = None;
        self.reply_to = None;
        self.compose_root = false;
        self.sent_awaiting = true; // a turn is now in flight
        self.focus = Focus::Message;
        Some(ViewAction::LaunchJob {
            orchestrator: self.orchestrator.clone(),
            task,
            room: None,
            policy,
            effort_override: self.effort,
            thread_id: Some(self.thread.id.clone()),
            conversation_id,
            new_turn,
        })
    }

    /// Render the Message compose box (its own focus border + title/hints).
    fn render_message_box(&self, frame: &mut Frame, area: Rect, composing: bool) {
        let bs = if self.focus == Focus::Message {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = if self.awaiting_reply() {
            " Message — waiting for reply… ".to_string()
        } else if composing {
            " Message (Tab · Enter send · ^P model · ^E effort) ".to_string()
        } else {
            " Message (Enter send · ^P model · ^E effort) ".to_string()
        };
        // Char-wrap (not ratatui's word Wrap) so the wrapped rows line up exactly
        // with set_compose_cursor's `n / inner_w` math — otherwise the caret
        // drifts every time a line wraps at a word boundary.
        let inner_w = area.width.saturating_sub(2).max(1) as usize;
        let message = Paragraph::new(char_wrap(&self.message_input, inner_w)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(bs)
                .title(title),
        );
        frame.render_widget(message, area);
    }

    /// Transcript lines, NEWEST FIRST (row 0 = newest at the top). Each message
    /// is a role header + wrapped content; the selected row is marked (`▸`) +
    /// reversed for the ↑↓ cursor. Assistant replies render as `[noosphera]` and
    /// are indented so each answer sits visibly under its question.
    fn message_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        // Root-pinned, newest-first-below, fork-grouped order — carries each row's
        // fork indent depth, and lets a row peek the row below to drop a redundant
        // `re:` when its parent is already the adjacent line.
        let ordered: Vec<&Message> = self
            .display_order()
            .iter()
            .map(|i| &self.thread.messages[*i])
            .collect();
        for (row, m) in ordered.iter().enumerate() {
            let expanded = self.expanded.contains(&m.id);
            let picked = self.selected == Some(row);
            let color = match m.role.as_str() {
                "user" => Color::Cyan,
                "assistant" => Color::Green,
                _ => Color::DarkGray,
            };
            let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            if picked {
                style = style.add_modifier(Modifier::REVERSED);
            }
            // Leftmost mark: cursor when selected, else a ● on a thread origin OR
            // any branch-off (a message whose branch_id differs from its parent's)
            // so a nested sub-thread reads as its own thread — its own circle, and
            // it's already indented by fork depth. A plain in-branch continuation
            // stays blank.
            let is_branch_root = match m.parent_id.as_deref() {
                None => true,
                Some(pid) => self
                    .thread
                    .get(pid)
                    .is_none_or(|p| p.branch_id != m.branch_id),
            };
            let mark = if picked {
                "❯"
            } else if is_branch_root {
                "●"
            } else {
                " "
            };
            let preview = first_line(&m.content, PREVIEW);
            // `▾` expanded, `▸` collapsed-but-has-more, `·` nothing to expand.
            let fold = if expanded {
                "▾"
            } else if preview.ends_with('…') {
                "▸"
            } else {
                "·"
            };
            let detail = if picked && m.job_id.is_some() {
                "  (^D)"
            } else {
                ""
            };
            // Indent by fork depth — a leaf continuation stays flat, a fork's
            // subthread indents; a node with >1 reply is a branch point (`⑂N`).
            let indent = "  ".repeat(self.thread.fork_depth(&m.id));
            let children = self.thread.children(Some(&m.id)).len();
            let branch = if children > 1 {
                format!("  ⑂{children}")
            } else {
                String::new()
            };
            // Role as a plain, padded, colour-coded word (no brackets) — "you" for
            // the operator, "noosphera" for the consensus reply.
            let role_word = display_role(&m.role);
            // Timestamp is a dim, fixed-width LEADING column so it aligns down the
            // left edge and recedes; the tree indents to its right. The fork
            // `indent` goes at the FRONT of the body so a whole nested row shifts
            // right — making the branch structure visible instead of jamming the
            // markers into a flat left block.
            let ts_style = if picked {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", fmt_ts(m.ts)), ts_style),
                Span::styled(
                    format!("{indent}{mark} {fold} {role_word:<9} {preview}{branch}{detail}"),
                    style,
                ),
            ]));
            if expanded {
                // Align continuation under the node: past the timestamp column
                // (13 + a space) + the row's fork indent.
                let cont_pad = format!("{}{indent}     ", " ".repeat(14));
                for content_line in m.content.lines() {
                    lines.push(Line::from(format!("{cont_pad}{content_line}")));
                }
                lines.push(Line::from(""));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "No messages yet — press w to write.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines
    }

    /// Rendered height of a message row: one line collapsed, or a header + full
    /// wrapped content + a blank when expanded.
    fn message_height(&self, m: &Message) -> u16 {
        if self.expanded.contains(&m.id) {
            1 + m.content.lines().count() as u16 + 1
        } else {
            1
        }
    }

    /// `  ↳ re: <parent preview>` for a reply, or empty for a root turn.
    fn re_tag(&self, m: &Message) -> String {
        m.parent_id
            .as_deref()
            .and_then(|p| self.thread.get(p))
            .map(|parent| format!("  ↳ re: {}", first_line(&parent.content, RE_PREVIEW)))
            .unwrap_or_default()
    }

    /// Toggle the selected message's expanded/collapsed state.
    fn toggle_expand(&mut self, expand: Option<bool>) {
        let Some(m) = self.selected.and_then(|r| self.row_message(r)) else {
            return;
        };
        let id = m.id.clone();
        let want = expand.unwrap_or(!self.expanded.contains(&id));
        if want {
            self.expanded.insert(id);
        } else {
            self.expanded.remove(&id);
        }
        self.ensure_visible();
    }

    /// Open the selected message full-screen (`o`) — for reading a long reply in
    /// full rather than the inline expand.
    fn open_full(&mut self) {
        if let Some(m) = self.selected.and_then(|r| self.row_message(r)) {
            self.full_view = Some(m.id.clone());
            self.full_scroll = 0;
        }
    }

    /// Keys while a message is open full-screen: scroll + Esc/q back.
    fn full_view_key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{KeyCode, KeyEventKind};
        if event::is_escape(ev) {
            self.full_view = None;
            return None;
        }
        if let crossterm::event::Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => self.full_view = None,
                KeyCode::Up => self.full_scroll = self.full_scroll.saturating_sub(1),
                KeyCode::Down => self.full_scroll = self.full_scroll.saturating_add(1),
                KeyCode::PageUp => self.full_scroll = self.full_scroll.saturating_sub(10),
                KeyCode::PageDown => self.full_scroll = self.full_scroll.saturating_add(10),
                KeyCode::Home => self.full_scroll = 0,
                _ => {}
            }
        }
        None
    }

    /// Keys while an agent's `ask_user` question is showing: pick an option
    /// (↑↓ + Enter), type a free answer, or Esc to skip (the agent times out on
    /// its round budget). Returns a `RespondToolCall` fetch when answered.
    fn question_key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        if event::is_escape(ev) {
            self.question_queue.pop_front(); // skip this one — agent times out
            return None;
        }
        // Ctrl-C aborts the whole deliberation even while a question is up — the
        // agent is blocked in this job, so cancel it and drop all questions.
        if is_ctrl(ev, 'c') {
            let q = self.question_queue.front()?.clone();
            self.question_queue.clear();
            return Some(ViewAction::Fetch(FetchRequest::CancelJob {
                orchestrator: self.orchestrator.clone(),
                job_id: q.job_id,
                thread_id: self.thread.id.clone(),
            }));
        }
        let q = self.question_queue.front_mut()?;
        let n_opts = q.options.len();
        let answer: Option<String> = if q.typing {
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    // Guard against Ctrl-<letter> inserting the bare letter.
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        q.answer.push(c);
                        None
                    }
                    KeyCode::Backspace => {
                        q.answer.pop();
                        None
                    }
                    KeyCode::Enter => {
                        let a = q.answer.trim().to_string();
                        (!a.is_empty()).then_some(a)
                    }
                    _ => None,
                },
                _ => None,
            }
        } else if event::is_up(ev) {
            q.selected = q.selected.saturating_sub(1);
            None
        } else if event::is_down(ev) {
            if q.selected < n_opts {
                q.selected += 1;
            }
            None
        } else if event::is_enter(ev) {
            if q.selected < n_opts {
                Some(q.options[q.selected].clone())
            } else {
                q.typing = true; // the "type your own" row
                None
            }
        } else {
            None
        };
        let answer = answer?;
        let q = self.question_queue.pop_front()?; // reveal the next stacked one
        Some(ViewAction::Fetch(FetchRequest::RespondToolCall {
            orchestrator: self.orchestrator.clone(),
            job_id: q.job_id,
            call_id: q.call_id,
            result: answer,
        }))
    }

    /// Stack a question unless its `call_id` is already queued (the SSE and the
    /// on-reopen recovery fetch can both deliver the same call).
    fn enqueue_question(&mut self, q: AskQuestion) {
        if !self.question_queue.iter().any(|e| e.call_id == q.call_id) {
            self.question_queue.push_back(q);
        }
    }

    /// Cycle the effort override + report it (shared by both modes).
    fn cycle_effort(&mut self) -> ViewAction {
        self.effort = Self::next_effort(self.effort);
        let label = self
            .effort
            .map(|e| format!("{e:.2}"))
            .unwrap_or_else(|| "default".into());
        ViewAction::SetStatus(format!("Effort: {label}"), StatusLevel::Info)
    }

    /// Open deliberation detail: the selected past turn's job if one is picked,
    /// otherwise this thread's running job (resolved loop-side).
    fn open_detail(&self) -> ViewAction {
        if let Some(job_id) = self.selected_job() {
            return ViewAction::Push(crate::cli::tui::app::ViewId::JobDetail {
                job_id,
                orchestrator: self.orchestrator.clone(),
            });
        }
        ViewAction::OpenThreadJob {
            thread_id: self.thread.id.clone(),
            orchestrator: self.orchestrator.clone(),
        }
    }

    /// Enter compose mode (the separate input box) to write a reply.
    fn enter_compose(&mut self) {
        self.compose_root = false;
        // Capture the reply target now — the composed turn roots under the node
        // the cursor was on (else it continues from the tip).
        self.reply_to = self
            .selected
            .and_then(|r| self.row_message(r))
            .map(|m| m.id.clone());
        self.enter_compose_common();
    }

    /// Compose a NEW ROOT turn (`n`): a fresh line under the same subject,
    /// rooted at nothing rather than a reply.
    fn enter_compose_root(&mut self) {
        self.compose_root = true;
        self.reply_to = None;
        self.enter_compose_common();
    }

    fn enter_compose_common(&mut self) {
        self.mode = Mode::Compose;
        self.selected = None;
        self.focus = if self.has_subject() {
            Focus::Message
        } else {
            Focus::Subject
        };
        self.cursor_to_end();
    }

    /// Keys while reading the transcript.
    fn read_key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{KeyCode, KeyEventKind};
        if event::is_escape(ev) {
            return Some(ViewAction::Pop);
        }
        if event::is_up(ev) {
            self.select_up();
            return None;
        }
        if event::is_down(ev) {
            self.select_down();
            return None;
        }
        if let crossterm::event::Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.view_h.max(1)),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.view_h.max(1)),
                KeyCode::Home => {
                    self.scroll = 0;
                    self.unseen_reply = false;
                }
                KeyCode::End => self.scroll = u16::MAX, // clamped in draw
                // Expand/collapse the selected message's full content.
                KeyCode::Enter => self.toggle_expand(None),
                KeyCode::Right => self.toggle_expand(Some(true)),
                KeyCode::Left => self.toggle_expand(Some(false)),
                KeyCode::Char('o') => self.open_full(),
                KeyCode::Char('n') => self.enter_compose_root(),
                KeyCode::Char('w') | KeyCode::Char('r') | KeyCode::Char('i') => {
                    self.enter_compose()
                }
                _ => {}
            }
        }
        None
    }

    /// Keys while composing a turn in the separate box.
    fn compose_key(&mut self, ev: &crossterm::event::Event) -> Option<ViewAction> {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        if event::is_escape(ev) {
            // Cancel: a brand-new empty thread leaves the view; otherwise return
            // to the transcript reader.
            if !self.has_subject() && self.message_input.trim().is_empty() {
                return Some(ViewAction::Pop);
            }
            // Persist the draft so leaving compose (or closing later) doesn't
            // lose it — restored next time the thread is opened.
            self.save_draft();
            self.mode = Mode::Read;
            return None;
        }
        // Tab switches Subject/Message only while composing a new thread.
        if !self.has_subject()
            && let crossterm::event::Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Tab
        {
            self.focus = match self.focus {
                Focus::Subject => Focus::Message,
                Focus::Message => Focus::Subject,
            };
            self.cursor_to_end();
            return None;
        }
        if event::is_enter(ev) {
            if self.focus == Focus::Message && self.message_input.trim() == "/model" {
                self.message_input.clear();
                self.cursor = 0;
                self.open_picker();
                return None;
            }
            let action = self.send();
            // A launched turn returns to the reader (newest-first, at the top).
            if matches!(action, Some(ViewAction::LaunchJob { .. })) {
                self.mode = Mode::Read;
                self.scroll = 0;
            }
            return action;
        }
        // A bracketed paste arrives whole — insert it (collapsing a large one to
        // a placeholder) so an embedded newline can't submit mid-paste.
        if let crossterm::event::Event::Paste(text) = ev {
            self.insert_paste(text.clone());
            // A paste is costly to redo (from a source doc) — persist immediately.
            self.save_draft();
            return None;
        }
        if let crossterm::event::Event::Key(key) = ev
            && key.kind == KeyEventKind::Press
        {
            // Ctrl-W (vim) deletes the word before the cursor, or a paste block.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
                self.delete_word();
                self.save_draft();
                return None;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // `edited` gates the per-keystroke draft save so a crash mid-typing
            // can't lose work (moves don't touch the text, so they skip it).
            let edited = match key.code {
                KeyCode::Left if ctrl => {
                    self.move_word_left();
                    false
                }
                KeyCode::Right if ctrl => {
                    self.move_word_right();
                    false
                }
                KeyCode::Left => {
                    self.move_left();
                    false
                }
                KeyCode::Right => {
                    self.move_right();
                    false
                }
                KeyCode::Home => {
                    self.cursor = 0;
                    false
                }
                KeyCode::End => {
                    self.cursor_to_end();
                    false
                }
                KeyCode::Char(c) if !ctrl => {
                    self.insert_char(c);
                    true
                }
                KeyCode::Backspace => {
                    self.backspace();
                    true
                }
                _ => false,
            };
            if edited {
                self.save_draft();
            }
        }
        None
    }

    /// Read mode: the newest-first transcript with a scroll offset + key hints.
    fn draw_reader(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let lines = self.message_lines();
        let total = lines.len() as u16;
        let visible = chunks[0].height.saturating_sub(2);
        self.view_h = visible;
        let max_scroll = total.saturating_sub(visible);
        self.scroll = self.scroll.min(max_scroll);
        if self.scroll == 0 {
            self.unseen_reply = false;
        }
        let status = if self.awaiting_reply() {
            " · deliberating…"
        } else {
            ""
        };
        let cta = if self.unseen_reply {
            " · ▲ new reply (Home)"
        } else if self.thread.draft.is_some() {
            " · ✎ draft saved (w)"
        } else {
            ""
        };
        // Scroll indicators: how many lines are hidden above/below the viewport.
        let above = self.scroll;
        let below = max_scroll.saturating_sub(self.scroll);
        let scrollbar = match (above > 0, below > 0) {
            (true, true) => format!(" ▲{above} ▼{below}"),
            (true, false) => format!(" ▲{above}"),
            (false, true) => format!(" ▼{below}"),
            (false, false) => String::new(),
        };
        let transcript = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {}{status}{cta}{scrollbar} ", self.thread.subject)),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(transcript, chunks[0]);
        let hint = if self.awaiting_reply() {
            " ^C stop · ^D details · w follow-up · ↑↓ pick · ↵/→ expand · o full · PgDn scroll · Esc"
        } else {
            " w reply · n new-root · ↑↓ pick · ↵/→ expand · o full · ^D details · ^P model · Esc"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
    }

    /// Full-screen view of one message: header (timestamp/role/re) + full
    /// scrollable content. Opened with `o`, closed with Esc/q.
    fn draw_full(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let (title, body) = match self.full_view.as_deref().and_then(|id| self.thread.get(id)) {
            Some(m) => (
                format!(
                    " {} [{}]{} ",
                    fmt_ts(m.ts),
                    display_role(&m.role),
                    self.re_tag(m)
                ),
                m.content.clone(),
            ),
            None => (" (message unavailable) ".to_string(), String::new()),
        };
        // Clamp scroll to the content (physical lines; wrapping may exceed, so
        // this is a lower bound — good enough to stop scrolling into the void).
        let visible = chunks[0].height.saturating_sub(2);
        let total = body.lines().count() as u16;
        self.full_scroll = self.full_scroll.min(total.saturating_sub(visible));
        let para = Paragraph::new(body)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.full_scroll, 0));
        frame.render_widget(para, chunks[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ↑↓ · PgUp/PgDn scroll · Home top · Esc back",
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
    }

    /// The `ask_user` modal: the agent's question, its options (↑↓/Enter), a
    /// "type your own" row, and a note that it's time-boxed to the round.
    fn draw_question(&self, frame: &mut Frame, area: Rect) {
        let Some(q) = self.question_queue.front() else {
            return;
        };
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                format!("[{}] asks:", display_role("assistant")),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for wl in q.question.lines() {
            lines.push(Line::from(wl.to_string()));
        }
        lines.push(Line::from(""));
        for (i, opt) in q.options.iter().enumerate() {
            let sel = !q.typing && q.selected == i;
            let style = if sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let prefix = if sel { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(format!("{prefix}{opt}"), style)));
        }
        // The "type your own" row / the free-text input line.
        if q.typing {
            lines.push(Line::from(Span::styled(
                format!("✎ {}", q.answer),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            let sel = q.selected == q.options.len();
            let style = if sel {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let prefix = if sel { "▸ " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{prefix}✎ type your own…"),
                style,
            )));
        }
        lines.push(Line::from(""));
        let hint = if q.typing {
            "type · Enter send · Esc skip"
        } else {
            "↑↓ pick · Enter · Esc skip — time-boxed to the round"
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
        let title = match self.question_queue.len() {
            0 | 1 => " ✋ Question from the deliberation ".to_string(),
            n => format!(" ✋ Question from the deliberation  (+{} more) ", n - 1),
        };
        let para = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(title),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(para, area);
    }

    /// Compose mode: a new thread shows Subject + Message; an existing thread
    /// shows the reply box under a "Reply to: <subject>" header.
    fn draw_compose(&mut self, frame: &mut Frame, area: Rect) {
        if !self.has_subject() {
            let subject_border = if self.focus == Focus::Subject {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let c = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
            let subject = Paragraph::new(self.subject_input.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(subject_border)
                    .title(" Subject (Tab → Message · ^P model · ^E effort · Esc cancel) "),
            );
            frame.render_widget(subject, c[0]);
            self.render_message_box(frame, c[1], true);
            let focused = if self.focus == Focus::Subject {
                c[0]
            } else {
                c[1]
            };
            self.set_compose_cursor(frame, focused);
        } else {
            let c = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
            // New-root, the reply target (fork/continuation), else the subject.
            let header = if self.compose_root {
                format!(" ✎ new root turn in {} ", self.thread.subject)
            } else {
                match self.reply_to.as_deref().and_then(|id| self.thread.get(id)) {
                    Some(target) => {
                        let role = display_role(&target.role);
                        let preview: String = target
                            .content
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(48)
                            .collect();
                        format!(" ↳ replying under [{role}] {preview} ")
                    }
                    None => format!(" Reply to: {} ", self.thread.subject),
                }
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    header,
                    Style::default().fg(Color::DarkGray),
                ))),
                c[0],
            );
            self.render_message_box(frame, c[1], false);
            self.set_compose_cursor(frame, c[1]);
        }
    }

    /// Place the terminal cursor at the insertion point inside a bordered box,
    /// hard-wrapping at the inner width (an approximation of the paragraph wrap;
    /// exact for the common single-line case).
    fn set_compose_cursor(&self, frame: &mut Frame, area: Rect) {
        let inner_w = area.width.saturating_sub(2).max(1);
        let buf = self.focused_buf_ref();
        let before = &buf[..self.cursor.min(buf.len())];
        let n = before.chars().count() as u16;
        let (row, col) = (n / inner_w, n % inner_w);
        if area.height > 2 && row < area.height - 2 {
            frame.set_cursor_position((area.x + 1 + col, area.y + 1 + row));
        }
    }
}

/// True when `ev` is a Ctrl+`c` key press.
fn is_ctrl(ev: &crossterm::event::Event, c: char) -> bool {
    matches!(ev, crossterm::event::Event::Key(k)
        if k.kind == crossterm::event::KeyEventKind::Press
            && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
            && k.code == crossterm::event::KeyCode::Char(c))
}

/// Max chars of a collapsed message preview / a `↳ re:` parent preview.
const PREVIEW: usize = 60;
const RE_PREVIEW: usize = 28;

/// First line of `s`, truncated to `max` chars. Appends `…` when there is more
/// to see (the line was cut, or the message has further lines) — the reader uses
/// that as the "expandable" signal.
pub(crate) fn first_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max || s.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

/// Unix seconds → `[MM-DD HH:MM]` (UTC — chrono is built without the `clock`
/// feature, so local time isn't available here).
pub(crate) fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("[%m-%d %H:%M]").to_string())
        .unwrap_or_else(|| "[--/-- --:--]".into())
}

/// Hard char-wrap `s` into rows of at most `width` chars (char-safe). Used for
/// the compose box so its visual rows match `set_compose_cursor`'s `n / width`
/// caret math exactly (ratatui's word Wrap breaks earlier, drifting the caret).
/// `message_input` carries no `\n` (Enter sends; pastes collapse to markers), so
/// a single logical line is all this needs to handle. Always ≥1 row.
fn char_wrap(s: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut count = 0usize;
    for ch in s.chars() {
        cur.push(ch);
        count += 1;
        if count == width {
            lines.push(Line::from(std::mem::take(&mut cur)));
            count = 0;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

/// User-facing role label: an `assistant` turn renders as `noosphera` (the
/// product is "AI email", never "chat"/"assistant" user-facing).
pub(crate) fn display_role(role: &str) -> &str {
    match role {
        "assistant" => "noosphera",
        "user" => "you",
        other => other,
    }
}

/// Byte offset of the char boundary before `i` (or 0).
fn prev_char_boundary(s: &str, i: usize) -> usize {
    s[..i.min(s.len())]
        .char_indices()
        .next_back()
        .map(|(j, _)| j)
        .unwrap_or(0)
}

/// Byte offset of the char boundary after `i` (or `s.len()`).
fn next_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    s[i..]
        .char_indices()
        .nth(1)
        .map(|(j, _)| i + j)
        .unwrap_or(s.len())
}

/// Start of the word before `i`: skip whitespace back, then the word.
pub(crate) fn prev_word_boundary(s: &str, i: usize) -> usize {
    let head = &s[..i.min(s.len())];
    let trimmed = head.trim_end_matches(char::is_whitespace);
    trimmed
        .rfind(char::is_whitespace)
        .map(|j| j + 1)
        .unwrap_or(0)
}

/// End of the word after `i`: skip whitespace forward, then the word.
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

impl View for ThreadView {
    fn update(&mut self, app_event: &AppEvent) -> Option<ViewAction> {
        // A deliberation finished (live JobComplete) or a reopened thread's
        // pending job was reconciled — reload so the reply appears. If the reader
        // is scrolled into history, flag a CTA instead of yanking to the new top.
        let reply_landed = matches!(
            app_event,
            AppEvent::Data(crate::cli::tui::event::DataEvent::SseEvent(
                crate::cli::tui::event::SseEvent::JobComplete { .. },
            )) | AppEvent::Data(crate::cli::tui::event::DataEvent::ThreadReconciled { .. })
        );
        if reply_landed {
            let before = self.thread.messages.len();
            if let Some(fresh) = self.store.load(&self.thread.id) {
                self.thread = fresh;
            }
            if self.thread.messages.len() > before && self.scroll > 0 {
                self.unseen_reply = true;
            }
            // The in-flight turn resolved (a reply landed, or a dead job was
            // reconciled away) — stop showing "deliberating…".
            self.sent_awaiting = false;
            // The job is over, so any questions it was blocked on are moot.
            self.question_queue.clear();
            return None;
        }
        // An agent asked the operator a question (ask_user) — stack it.
        use crate::cli::tui::event::{DataEvent, SseEvent};
        if let AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallPending {
            job_id,
            call_id,
            arguments,
            ..
        })) = app_event
        {
            if let Some(q) = AskQuestion::from_pending(job_id.clone(), call_id.clone(), arguments) {
                self.enqueue_question(q);
            }
            return None;
        }
        // A question was answered (by us) or timed out — drop it from the stack.
        if let AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallResolved { call_id })) =
            app_event
        {
            self.question_queue.retain(|q| &q.call_id != call_id);
            return None;
        }
        // Recovered pending questions (on reopen) — stack every ask_user one.
        if let AppEvent::Data(DataEvent::ToolCallsLoaded { thread_id, calls }) = app_event
            && thread_id == &self.thread.id
        {
            for c in calls {
                if let Some(q) =
                    AskQuestion::from_pending(c.job_id.clone(), c.call_id.clone(), &c.arguments)
                {
                    self.enqueue_question(q);
                }
            }
            return None;
        }
        let AppEvent::Terminal(ev) = app_event else {
            return None;
        };
        if self.picking.is_some() {
            return self.picker_key(ev);
        }
        // A pending ask_user question captures all input until answered/skipped.
        if !self.question_queue.is_empty() {
            return self.question_key(ev);
        }
        // Full-screen message view captures all input (scroll + back).
        if self.full_view.is_some() {
            return self.full_view_key(ev);
        }
        // Global (both modes): model picker, effort, deliberation detail.
        if is_ctrl(ev, 'p') {
            self.open_picker();
            return None;
        }
        if is_ctrl(ev, 'e') {
            return Some(self.cycle_effort());
        }
        if is_ctrl(ev, 'd') {
            return Some(self.open_detail());
        }
        // Ctrl-C stops (kills) the thread's running deliberation, if any.
        if is_ctrl(ev, 'c')
            && let Some(job_id) = self.thread.pending_job.clone()
        {
            return Some(ViewAction::Fetch(FetchRequest::CancelJob {
                orchestrator: self.orchestrator.clone(),
                job_id,
                thread_id: self.thread.id.clone(),
            }));
        }
        match self.mode {
            Mode::Read => self.read_key(ev),
            Mode::Compose => self.compose_key(ev),
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(sel) = self.picking {
            let items: Vec<ratatui::widgets::ListItem> = self
                .models
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let style = if i == sel {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let prefix = if i == sel { "▸ " } else { "  " };
                    ratatui::widgets::ListItem::new(Line::from(Span::styled(
                        format!("{prefix}{m}"),
                        style,
                    )))
                })
                .collect();
            let list = ratatui::widgets::List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Pick a model (↑↓ · Enter · Esc) "),
            );
            frame.render_widget(list, area);
            return;
        }

        if !self.question_queue.is_empty() {
            self.draw_question(frame, area);
            return;
        }
        if self.full_view.is_some() {
            self.draw_full(frame, area);
            return;
        }
        match self.mode {
            Mode::Read => self.draw_reader(frame, area),
            Mode::Compose => self.draw_compose(frame, area),
        }
    }

    fn captures_input(&self) -> bool {
        true
    }

    fn active_model(&self) -> Option<&str> {
        self.thread.active_policy.as_deref()
    }

    fn active_effort(&self) -> Option<f32> {
        self.effort
    }

    fn on_enter(&mut self) -> Vec<ViewAction> {
        // Reload from the store so a reply appended while the deliberation ran
        // (persisted loop-side on JobComplete) appears when we return here.
        if let Some(fresh) = self.store.load(&self.thread.id) {
            self.thread = fresh;
            if self.subject_input.trim().is_empty() {
                self.subject_input = self.thread.subject.clone();
            }
        }
        // A draft saved before the TUI closed comes back into the compose box.
        self.restore_draft();
        // Returning to an existing thread lands in the reader at the newest turn,
        // with the cursor already ON it — row 0 is the pinned root, row 1 (when it
        // exists) is the newest turn. Starting at `None` left no visible cursor, so
        // the reader had to press ↑ then ↓ before anything highlighted.
        if self.has_subject() {
            self.mode = Mode::Read;
            self.scroll = 0;
            let rows = self.display_order().len();
            self.selected = (rows > 0).then(|| rows.min(2).saturating_sub(1));
            self.unseen_reply = false;
            self.full_view = None;
        }
        // Ask the orchestrator which of the caller's jobs are running so ^D / stop
        // resolve this thread's live job from the server, not local state.
        let mut actions = vec![ViewAction::Fetch(FetchRequest::RefreshThreadJobs {
            orchestrator: self.orchestrator.clone(),
        })];
        // A pending job whose reply never landed (the TUI was closed when the
        // deliberation finished) — reconcile it against the server.
        if let Some(job_id) = self.thread.pending_job.clone()
            && self.awaiting_reply()
        {
            actions.push(ViewAction::Fetch(FetchRequest::ReconcileThread {
                orchestrator: self.orchestrator.clone(),
                job_id: job_id.clone(),
                thread_id: self.thread.id.clone(),
            }));
            // Recover any ask_user question that fired while we were away.
            actions.push(ViewAction::Fetch(FetchRequest::PendingToolCalls {
                orchestrator: self.orchestrator.clone(),
                job_id,
                thread_id: self.thread.id.clone(),
            }));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view over a subject'd thread (focus starts on Message), temp-backed.
    fn view() -> (tempfile::TempDir, ThreadView) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let v = ThreadView::with_thread(
            Thread::new("t"),
            store,
            "orch".to_string(),
            vec!["nsed:a".into(), "nsed:b".into()],
        );
        (tmp, v)
    }

    #[test]
    fn reply_under_selected_node_forks_and_carries_only_its_lineage() {
        let (_t, mut v) = view();
        // Linear wsup? → Hi! → foo.
        let a = v.thread.reply(None, "user", "wsup?");
        let b = v.thread.reply(Some(&a), "assistant", "Hi!");
        let _c = v.thread.reply(Some(&b), "user", "foo");
        let root_branch = v.thread.get(&a).unwrap().branch_id.clone();

        // The root (wsup?) is pinned at row 0. Opening compose captures it as the
        // reply target.
        v.selected = Some(0);
        v.enter_compose();
        assert_eq!(v.reply_to.as_deref(), Some(a.as_str()));
        v.message_input = "rooted from wsup".into();
        let action = v.send().expect("launches");

        let new_msg = v.thread.messages.last().unwrap();
        assert_eq!(new_msg.parent_id.as_deref(), Some(a.as_str()));
        // `a` was a non-leaf (child `Hi!`) → fork → fresh branch.
        assert_ne!(new_msg.branch_id, root_branch);
        let new_branch = new_msg.branch_id.clone();
        v.selected = None; // (send already cleared it)

        match action {
            ViewAction::LaunchJob {
                task,
                conversation_id,
                ..
            } => {
                assert!(task.contains("wsup?"));
                assert!(task.contains("rooted from wsup"));
                assert!(
                    !task.contains("Hi!"),
                    "fork must not carry siblings: {task}"
                );
                assert!(!task.contains("foo"));
                assert_eq!(conversation_id.as_deref(), Some(new_branch.as_str()));
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn reply_with_no_selection_continues_from_tip() {
        let (_t, mut v) = view();
        let a = v.thread.reply(None, "user", "wsup?");
        let _b = v.thread.reply(Some(&a), "assistant", "Hi!");
        let tip_branch = v.thread.tip().unwrap().branch_id.clone();
        v.selected = None;
        v.enter_compose();
        assert!(v.reply_to.is_none());
        v.message_input = "and then?".into();
        match v.send().expect("launches") {
            ViewAction::LaunchJob {
                task,
                conversation_id,
                ..
            } => {
                // Whole lineage as context; same branch as the tip (continuation).
                assert!(
                    task.contains("wsup?") && task.contains("Hi!") && task.contains("and then?")
                );
                assert_eq!(conversation_id.as_deref(), Some(tip_branch.as_str()));
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn collapsed_row_shows_timestamp_role_preview_and_no_re_tag_when_linear() {
        let (_t, mut v) = view();
        let a = v.thread.reply(None, "user", "wsup?\nsecond line");
        let _b = v.thread.reply(Some(&a), "assistant", "Hi there");
        let rendered: Vec<String> = v.message_lines().iter().map(line_text).collect();
        // Root pinned on top; the assistant reply renders as a plain-word role.
        assert!(
            rendered[0].contains("wsup?"),
            "root pinned top: {:?}",
            rendered[0]
        );
        assert!(
            rendered.iter().any(|l| l.contains("noosphera")),
            "{rendered:?}"
        );
        // Linear reply → parent is the adjacent row / root, so NO redundant `re:`.
        assert!(
            !rendered.iter().any(|l| l.contains("↳ re:")),
            "linear chain must not duplicate the parent as a re: tag: {rendered:?}"
        );
        assert!(
            rendered[0].contains("[07-") || rendered[0].contains("["),
            "has a timestamp"
        );
        // The multi-line user turn collapses: preview + ellipsis, no extra line.
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("you") && l.contains("wsup?…"))
        );
        assert!(
            !rendered.iter().any(|l| l.contains("second line")),
            "collapsed hides content beyond the first line"
        );
    }

    #[test]
    fn root_pinned_top_then_newest_first_below() {
        let (_t, mut v) = view();
        let root = v.thread.reply(None, "user", "the thread start");
        let r1 = v.thread.reply(Some(&root), "assistant", "reply one");
        let _r2 = v.thread.reply(Some(&r1), "user", "reply two is newest");
        let rows: Vec<String> = v.message_lines().iter().map(line_text).collect();
        // Root pinned at the very top (row 0), marked ●.
        assert!(
            rows[0].contains("thread start") && rows[0].contains('●'),
            "root pinned top: {rows:?}"
        );
        // Below the root, newest-first: reply two (newest) above reply one.
        let two = rows.iter().position(|l| l.contains("reply two")).unwrap();
        let one = rows.iter().position(|l| l.contains("reply one")).unwrap();
        assert!(two < one, "newest reply sits above the older one: {rows:?}");
    }

    #[test]
    fn root_turn_carries_a_dot_marker() {
        let (_t, mut v) = view();
        let root = v.thread.reply(None, "user", "the thread start");
        let _b = v.thread.reply(Some(&root), "assistant", "a reply here");
        let rendered: Vec<String> = v.message_lines().iter().map(line_text).collect();
        let root_line = rendered
            .iter()
            .find(|l| l.contains("thread start"))
            .unwrap();
        let reply_line = rendered
            .iter()
            .find(|l| l.contains("a reply here"))
            .unwrap();
        assert!(root_line.contains('●'), "thread root marked: {root_line:?}");
        assert!(
            !reply_line.contains('●'),
            "a reply is not a root: {reply_line:?}"
        );
    }

    #[test]
    fn a_fork_reads_as_its_own_branch_root_no_re_tag() {
        let (_t, mut v) = view();
        let root = v.thread.reply(None, "user", "root question");
        let mid = v.thread.reply(Some(&root), "assistant", "an answer");
        let _tip = v.thread.reply(Some(&mid), "user", "a follow up");
        // A second reply under `mid` forks a new branch. It should read as its own
        // sub-thread: its own ● circle (+ indent), NOT a redundant `↳ re:` tag —
        // the tree structure is now carried by indent + branch circles.
        let _fork = v.thread.reply(Some(&mid), "user", "different angle");
        let rendered: Vec<String> = v.message_lines().iter().map(line_text).collect();
        let fork_line = rendered
            .iter()
            .find(|l| l.contains("different angle"))
            .expect("fork rendered");
        assert!(
            fork_line.contains('●'),
            "a branch-off is marked as its own branch root: {fork_line:?}"
        );
        assert!(
            !rendered.iter().any(|l| l.contains("↳ re:")),
            "the re: tag is retired — indent + branch circle convey parentage: {rendered:?}"
        );
    }

    #[test]
    fn expand_toggles_full_content() {
        let (_t, mut v) = view();
        let a = v
            .thread
            .reply(None, "user", "line one\nline two\nline three");
        v.selected = Some(0); // the only (newest) row
        v.toggle_expand(None);
        assert!(v.expanded.contains(&a));
        let shown: Vec<String> = v.message_lines().iter().map(line_text).collect();
        assert!(shown.iter().any(|l| l.contains("line two")));
        assert!(shown.iter().any(|l| l.contains("line three")));
        // Collapse again — the extra lines disappear.
        v.toggle_expand(None);
        assert!(!v.expanded.contains(&a));
        let hidden: Vec<String> = v.message_lines().iter().map(line_text).collect();
        assert!(!hidden.iter().any(|l| l.contains("line two")));
    }

    #[test]
    fn fmt_ts_is_month_day_hour_minute() {
        assert_eq!(fmt_ts(0), "[01-01 00:00]");
    }

    #[test]
    fn char_wrap_rows_match_cursor_math() {
        // Row r holds chars [r*w .. (r+1)*w] — exactly what set_compose_cursor's
        // `n / w`, `n % w` assumes, so the caret can't drift.
        let text: Vec<String> = char_wrap("abcdefghijkl", 5)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(text, vec!["abcde", "fghij", "kl"]);
        assert_eq!(char_wrap("", 5).len(), 1, "empty still gives a caret row");
        assert_eq!(
            char_wrap("abcdefghij", 5).len(),
            2,
            "exact multiple → no trailing row"
        );
        // char-safe on multibyte.
        assert_eq!(char_wrap("αβγδεζ", 3).len(), 2);
    }

    #[test]
    fn first_line_boundaries() {
        assert_eq!(first_line("", 10), "");
        assert_eq!(first_line("short", 10), "short"); // fits, no ellipsis
        assert_eq!(first_line("exactlyten", 10), "exactlyten"); // exactly max
        assert_eq!(first_line("elevenchars", 10), "elevenchar…"); // truncated
        assert_eq!(first_line("one\ntwo", 10), "one…"); // multiline → ellipsis
        assert_eq!(first_line("αβγδε", 3), "αβγ…"); // char-safe on multibyte
    }

    #[test]
    fn fmt_ts_handles_out_of_range() {
        assert!(fmt_ts(-1).starts_with('[')); // pre-epoch still formats
        assert!(fmt_ts(i64::MAX).starts_with('[')); // out-of-range → fallback
        assert!(fmt_ts(i64::MIN).starts_with('['));
    }

    #[test]
    fn re_tag_empty_when_parent_missing_and_render_survives() {
        // A reply whose parent id no longer resolves (stale target) renders no
        // re-tag rather than crashing.
        let (_t, mut v) = view();
        v.thread.push_message(Message::now("user", "root"));
        let mut orphan = Message::now("user", "orphan");
        orphan.parent_id = Some("ghost-id".into());
        let target = orphan.clone();
        v.thread.messages.push(orphan);
        assert_eq!(v.re_tag(&target), "");
        let _ = v.message_lines(); // must not panic
    }

    #[test]
    fn n_composes_a_new_root_carrying_no_history() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        let a = v.thread.reply(None, "user", "wsup?");
        let _b = v.thread.reply(Some(&a), "assistant", "Hi!");
        v.update(&plain(KeyCode::Char('n')));
        assert!(v.compose_root && v.reply_to.is_none());
        v.message_input = "fresh topic".into();
        match v.send().expect("launches") {
            ViewAction::LaunchJob {
                task,
                conversation_id,
                ..
            } => {
                assert!(task.contains("fresh topic"));
                assert!(
                    !task.contains("wsup?"),
                    "new root carries no lineage: {task}"
                );
                assert!(!task.contains("Hi!"));
                let new = v.thread.messages.last().unwrap();
                assert!(new.parent_id.is_none(), "rooted at nothing");
                assert_eq!(conversation_id.as_deref(), Some(new.branch_id.as_str()));
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
        assert!(!v.compose_root, "cleared after send");
    }

    #[test]
    fn o_opens_full_view_scrolls_and_esc_closes() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        let a = v
            .thread
            .reply(None, "user", "a long message body\nwith several lines");
        v.selected = Some(0); // newest (only) row
        v.update(&plain(KeyCode::Char('o')));
        assert_eq!(v.full_view.as_deref(), Some(a.as_str()));
        // Full view captures scroll.
        v.update(&plain(KeyCode::Down));
        assert_eq!(v.full_scroll, 1);
        // Esc returns to the reader.
        v.update(&plain(KeyCode::Esc));
        assert!(v.full_view.is_none());
    }

    #[test]
    fn fork_point_marked_and_forked_branch_indented() {
        let (_t, mut v) = view();
        let a = v.thread.reply(None, "user", "wsup?");
        let _hi = v.thread.reply(Some(&a), "assistant", "Hi!"); // leaf reply (depth 0)
        let fork = v.thread.reply(Some(&a), "user", "other"); // fork under a → depth 1
        let rendered: Vec<String> = v.message_lines().iter().map(line_text).collect();
        // The fork point `a` has two children → ⑂2.
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("wsup?") && l.contains("⑂2")),
            "{rendered:?}"
        );
        // The forked turn is indented (fork_depth 1 = two leading spaces before
        // its timestamp) while the depth-0 reply is not.
        assert_eq!(v.thread.fork_depth(&fork), 1);
        // Columns stay aligned; the fork's indent sits before its preview text,
        // so the forked turn's content starts further right than a depth-0 reply.
        let fork_line = rendered.iter().find(|l| l.contains("other")).unwrap();
        let hi_line = rendered.iter().find(|l| l.contains("Hi!")).unwrap();
        assert!(
            fork_line.find("other").unwrap() > hi_line.find("Hi!").unwrap(),
            "fork preview indented: {fork_line:?} vs {hi_line:?}"
        );
    }

    #[test]
    fn w_then_type_then_enter_sends_followup() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        let a = v.thread.reply(None, "user", "q1");
        let _r = v.thread.reply(Some(&a), "assistant", "a1");
        v.store.save(&v.thread).unwrap();
        v.update(&plain(KeyCode::Char('w')));
        assert_eq!(v.mode, Mode::Compose, "w enters compose");
        for c in "follow up".chars() {
            v.update(&plain(KeyCode::Char(c)));
        }
        assert_eq!(v.message_input, "follow up");
        let action = v.update(&plain(KeyCode::Enter));
        assert!(
            matches!(action, Some(ViewAction::LaunchJob { .. })),
            "Enter must send, got {action:?}"
        );
    }

    #[test]
    fn each_keystroke_persists_the_draft() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.update(&plain(KeyCode::Char('w'))); // enter compose
        assert_eq!(v.mode, Mode::Compose);
        v.update(&plain(KeyCode::Char('h')));
        v.update(&plain(KeyCode::Char('i')));
        // Persisted after each keystroke — a crash mid-typing can't lose it.
        assert_eq!(
            v.store.load(&v.thread.id).unwrap().draft.as_deref(),
            Some("hi")
        );
        v.update(&plain(KeyCode::Backspace));
        assert_eq!(
            v.store.load(&v.thread.id).unwrap().draft.as_deref(),
            Some("h")
        );
    }

    #[test]
    fn draft_saves_restores_and_clears_on_send() {
        let (_t, mut v) = view();
        v.message_input = "my 30-min draft".into();
        v.cursor = v.message_input.len();
        v.save_draft();
        // Persisted on the thread + durably in the store.
        assert_eq!(v.thread.draft.as_deref(), Some("my 30-min draft"));
        assert_eq!(
            v.store.load(&v.thread.id).unwrap().draft.as_deref(),
            Some("my 30-min draft")
        );
        // Reopen: a fresh compose box restores the draft.
        v.message_input.clear();
        v.restore_draft();
        assert_eq!(v.message_input, "my 30-min draft");
        // Sending consumes the draft.
        assert!(matches!(v.send(), Some(ViewAction::LaunchJob { .. })));
        assert!(v.thread.draft.is_none());
        assert!(v.store.load(&v.thread.id).unwrap().draft.is_none());
    }

    #[test]
    fn unstuck_thread_not_deliberating_but_a_fresh_send_is() {
        let (_t, mut v) = view();
        // Unstuck thread: last turn is a bare user message, no pending job.
        v.thread.reply(None, "user", "unanswered");
        v.store.save(&v.thread).unwrap();
        assert!(v.thread.pending_job.is_none());
        assert!(
            !v.awaiting_reply(),
            "no phantom 'deliberating' on a dead/unstuck thread"
        );
        // A real send this session → deliberating.
        v.message_input = "new query".into();
        assert!(matches!(v.send(), Some(ViewAction::LaunchJob { .. })));
        assert!(v.sent_awaiting && v.awaiting_reply());
        // The turn resolves (reply lands / reconciled) → clears.
        v.update(&AppEvent::Data(
            crate::cli::tui::event::DataEvent::ThreadReconciled {
                thread_id: v.thread.id.clone(),
            },
        ));
        assert!(!v.sent_awaiting && !v.awaiting_reply());
    }

    #[test]
    fn typed_message_sends_even_while_awaiting_a_prior_reply() {
        // Regression: a filled query must send even if a prior turn is still
        // awaiting its reply (pending_job set) — it used to dead-end on reconcile.
        let (_t, mut v) = view();
        v.thread.reply(None, "user", "still awaiting"); // last turn is user → awaiting
        v.thread.pending_job = Some("job-stuck".into());
        assert!(v.awaiting_reply());
        v.message_input = "my 30-min query".into();
        let action = v.send();
        assert!(
            matches!(action, Some(ViewAction::LaunchJob { .. })),
            "typed message must send, got {action:?}"
        );
    }

    #[test]
    fn empty_enter_while_awaiting_reconciles() {
        let (_t, mut v) = view();
        v.thread.reply(None, "user", "awaiting");
        v.thread.pending_job = Some("job-1".into());
        v.message_input.clear();
        assert!(matches!(
            v.send(),
            Some(ViewAction::Fetch(FetchRequest::ReconcileThread { .. }))
        ));
    }

    #[test]
    fn send_blank_message_is_noop() {
        let (_t, mut v) = view();
        v.message_input = "   ".to_string();
        assert!(v.send().is_none());
        assert!(v.thread.messages.is_empty());
    }

    #[test]
    fn send_requires_a_subject_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut v = ThreadView::with_thread(Thread::new(""), store, "orch".into(), vec![]);
        assert_eq!(v.focus, Focus::Subject);
        v.message_input = "a question".into(); // subject still empty
        match v.send().expect("guides") {
            ViewAction::SetStatus(msg, _) => assert!(msg.contains("subject")),
            other => panic!("expected SetStatus, got {other:?}"),
        }
        assert!(v.thread.messages.is_empty());
        assert_eq!(v.focus, Focus::Subject);
    }

    #[test]
    fn send_records_message_sets_subject_and_clears() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut v = ThreadView::with_thread(Thread::new(""), store, "orch".into(), vec![]);
        v.subject_input = "Q3 audit".into();
        v.message_input = "what is rust?".into();
        let action = v.send().expect("launches");
        assert_eq!(v.thread.subject, "Q3 audit");
        assert!(v.message_input.is_empty());
        assert_eq!(v.thread.messages.len(), 1);
        assert_eq!(v.thread.messages[0].content, "what is rust?");
        match action {
            ViewAction::LaunchJob {
                task, orchestrator, ..
            } => {
                assert_eq!(task, "Subject: Q3 audit\n\nwhat is rust?");
                assert_eq!(orchestrator, "orch");
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn send_second_turn_includes_prior_context() {
        let (_t, mut v) = view();
        v.thread.push_message(Message::now("user", "what is rust?"));
        v.thread
            .push_message(Message::now("assistant", "a systems language"));
        v.message_input = "vs go?".into();
        match v.send().expect("launches") {
            ViewAction::LaunchJob { task, .. } => {
                assert!(task.contains("[user] what is rust?"));
                assert!(task.contains("[assistant] a systems language"));
                assert!(task.ends_with("[user] vs go?"));
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn send_while_awaiting_with_no_pending_job_proceeds() {
        // A lost prior reply (no recoverable job) must not dead-end the thread.
        let (_t, mut v) = view();
        v.thread.push_message(Message::now("user", "first")); // awaiting, no pending_job
        v.message_input = "second".into();
        assert!(matches!(v.send(), Some(ViewAction::LaunchJob { .. })));
        assert_eq!(v.thread.messages.len(), 2, "the new turn is recorded");
    }

    #[test]
    fn send_carries_active_policy() {
        let (_t, mut v) = view();
        v.thread.active_policy = Some("nsed:review".into());
        v.message_input = "go".into();
        match v.send().expect("launches") {
            ViewAction::LaunchJob { policy, .. } => {
                assert_eq!(policy.as_deref(), Some("nsed:review"))
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn active_model_is_the_threads_policy() {
        let (_t, mut v) = view();
        assert_eq!(v.active_model(), None);
        v.thread.active_policy = Some("nsed:review".into());
        assert_eq!(v.active_model(), Some("nsed:review"));
    }

    #[test]
    fn picker_selects_a_model_and_persists() {
        let (_t, mut v) = view();
        v.open_picker();
        assert_eq!(v.picking, Some(0));
        // move down then select → models[1].
        let down = AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let enter = AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        if let AppEvent::Terminal(ev) = &down {
            v.picker_key(ev);
        }
        if let AppEvent::Terminal(ev) = &enter {
            assert!(matches!(v.picker_key(ev), Some(ViewAction::SetStatus(..))));
        }
        assert_eq!(v.thread.active_policy.as_deref(), Some("nsed:b"));
        assert_eq!(v.picking, None);
    }

    #[test]
    fn slash_model_in_message_opens_picker() {
        let (_t, mut v) = view();
        v.enter_compose(); // /model + Enter is a compose-mode shortcut
        v.message_input = "/model".into();
        let enter = AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        assert!(v.update(&enter).is_none());
        assert_eq!(v.picking, Some(0));
        assert!(v.message_input.is_empty());
    }

    fn ctrl(c: char) -> AppEvent {
        AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ))
    }

    fn arrow(up: bool) -> AppEvent {
        let code = if up {
            crossterm::event::KeyCode::Up
        } else {
            crossterm::event::KeyCode::Down
        };
        AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        ))
    }

    fn ctrl_arrow(left: bool) -> AppEvent {
        let code = if left {
            crossterm::event::KeyCode::Left
        } else {
            crossterm::event::KeyCode::Right
        };
        AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::CONTROL),
        ))
    }

    fn plain(code: crossterm::event::KeyCode) -> AppEvent {
        AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        ))
    }

    fn pending_ev(job: &str, call: &str, question: &str, options: &[&str]) -> AppEvent {
        use crate::cli::tui::event::{DataEvent, SseEvent};
        AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallPending {
            job_id: job.into(),
            call_id: call.into(),
            agent_id: "a".into(),
            arguments: serde_json::json!({ "question": question, "options": options }),
            round: 1,
        }))
    }

    fn resolved_ev(call: &str) -> AppEvent {
        use crate::cli::tui::event::{DataEvent, SseEvent};
        AppEvent::Data(DataEvent::SseEvent(SseEvent::ToolCallResolved {
            call_id: call.into(),
        }))
    }

    fn pending_call(
        job: &str,
        call: &str,
        question: &str,
        options: &[&str],
    ) -> crate::agents::PendingToolCall {
        crate::agents::PendingToolCall {
            call_id: call.into(),
            job_id: job.into(),
            agent_id: "a".into(),
            tool_name: "user_ask_user".into(),
            arguments: serde_json::json!({ "question": question, "options": options }),
            round: 1,
            phase: Default::default(),
            status: crate::agents::ToolCallStatus::Pending,
            created_at: 0,
            responded_at: None,
            result: None,
        }
    }

    #[test]
    fn on_enter_fetches_pending_questions_for_a_live_job() {
        let (_t, mut v) = view();
        v.thread.reply(None, "user", "q");
        v.thread.pending_job = Some("j1".into());
        v.store.save(&v.thread).unwrap();
        let actions = v.on_enter();
        assert!(actions.iter().any(|a| matches!(
            a,
            ViewAction::Fetch(FetchRequest::PendingToolCalls { job_id, .. }) if job_id == "j1"
        )));
    }

    #[test]
    fn recovered_tool_calls_surface_the_first_ask_user_question() {
        use crate::cli::tui::event::DataEvent;
        let (_t, mut v) = view();
        let calls = vec![pending_call("j1", "c1", "Which?", &["a", "b"])];
        v.update(&AppEvent::Data(DataEvent::ToolCallsLoaded {
            thread_id: v.thread.id.clone(),
            calls,
        }));
        assert_eq!(v.question_queue.front().unwrap().question, "Which?");
        assert_eq!(v.question_queue.front().unwrap().call_id, "c1");
    }

    #[test]
    fn distinct_questions_stack_and_answer_one_by_one() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.update(&pending_ev("j1", "c1", "Env?", &["dev", "prod"]));
        v.update(&pending_ev("j1", "c2", "Region?", &["eu", "us"]));
        assert_eq!(v.question_queue.len(), 2);
        assert_eq!(v.question_queue.front().unwrap().question, "Env?");
        // Same call again → not re-stacked (SSE + recovery dedup by call_id).
        v.update(&pending_ev("j1", "c1", "Env?", &["dev", "prod"]));
        assert_eq!(v.question_queue.len(), 2);
        // Answer the front → reveals the next.
        v.update(&plain(KeyCode::Enter)); // pick "dev"
        assert_eq!(v.question_queue.len(), 1);
        assert_eq!(v.question_queue.front().unwrap().question, "Region?");
        // Esc skips the last → queue empty.
        v.update(&plain(KeyCode::Esc));
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn resolved_removes_from_anywhere_in_the_stack() {
        let (_t, mut v) = view();
        v.update(&pending_ev("j", "c1", "A?", &["x"]));
        v.update(&pending_ev("j", "c2", "B?", &["y"]));
        // The second (not-shown) question expires/answers elsewhere → removed.
        v.update(&resolved_ev("c2"));
        assert_eq!(v.question_queue.len(), 1);
        assert_eq!(v.question_queue.front().unwrap().call_id, "c1");
    }

    #[test]
    fn ctrl_c_clears_the_whole_stack() {
        let (_t, mut v) = view();
        v.update(&pending_ev("j1", "c1", "A?", &["x"]));
        v.update(&pending_ev("j1", "c2", "B?", &["y"]));
        let action = v.update(&ctrl('c'));
        assert!(matches!(
            action,
            Some(ViewAction::Fetch(FetchRequest::CancelJob { .. }))
        ));
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn ask_user_ctrl_c_cancels_the_job_and_clears() {
        let (_t, mut v) = view();
        v.update(&pending_ev("j1", "c1", "Q?", &["a"]));
        let action = v.update(&ctrl('c'));
        match action {
            Some(ViewAction::Fetch(FetchRequest::CancelJob { job_id, .. })) => {
                assert_eq!(job_id, "j1")
            }
            other => panic!("expected CancelJob, got {other:?}"),
        }
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn ask_user_ctrl_char_does_not_type_into_the_answer() {
        let (_t, mut v) = view();
        v.update(&pending_ev("j", "c", "Name?", &[])); // typing mode
        v.update(&ctrl('w')); // Ctrl-W must not insert 'w'
        assert_eq!(v.question_queue.front().unwrap().answer, "");
    }

    #[test]
    fn job_complete_clears_a_stale_question() {
        use crate::cli::tui::event::{DataEvent, SseEvent};
        let (_t, mut v) = view();
        v.update(&pending_ev("j", "c", "Q?", &["a"]));
        v.update(&AppEvent::Data(DataEvent::SseEvent(
            SseEvent::JobComplete {
                status: "Success".into(),
                job_id: "j".into(),
                rounds_completed: 1,
                best_proposal_content: "x".into(),
                best_proposal_score: 0.5,
                best_proposal_author: "a".into(),
            },
        )));
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn ask_user_pick_option_answers_and_clears() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.update(&pending_ev("j1", "c1", "Which env?", &["dev", "prod"]));
        let q = v.question_queue.front().unwrap();
        assert_eq!(q.question, "Which env?");
        assert_eq!(q.options, vec!["dev", "prod"]);
        // ↓ to "prod", Enter → RespondToolCall + cleared.
        v.update(&plain(KeyCode::Down));
        let action = v.update(&plain(KeyCode::Enter));
        match action {
            Some(ViewAction::Fetch(FetchRequest::RespondToolCall {
                job_id,
                call_id,
                result,
                ..
            })) => {
                assert_eq!(
                    (job_id.as_str(), call_id.as_str(), result.as_str()),
                    ("j1", "c1", "prod")
                );
            }
            other => panic!("expected RespondToolCall, got {other:?}"),
        }
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn ask_user_free_text_and_custom_row_and_skip() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        // No options → straight to typing.
        v.update(&pending_ev("j", "c", "Name?", &[]));
        assert!(v.question_queue.front().unwrap().typing);
        for ch in "prod".chars() {
            v.update(&plain(KeyCode::Char(ch)));
        }
        let action = v.update(&plain(KeyCode::Enter));
        assert!(matches!(
            action,
            Some(ViewAction::Fetch(FetchRequest::RespondToolCall { result, .. })) if result == "prod"
        ));
        assert!(v.question_queue.is_empty());

        // With options: ↓ past them lands on the "type your own" row → Enter → typing.
        v.update(&pending_ev("j", "c2", "Q?", &["a"]));
        v.update(&plain(KeyCode::Down)); // onto the custom row (index == options.len())
        v.update(&plain(KeyCode::Enter));
        assert!(v.question_queue.front().unwrap().typing);
        // Esc skips (agent times out on its round budget).
        v.update(&plain(KeyCode::Esc));
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn ask_user_resolved_event_clears_only_the_matching_call() {
        let (_t, mut v) = view();
        v.update(&pending_ev("j", "c1", "Q?", &["a"]));
        v.update(&resolved_ev("other")); // different call — keep showing
        assert!(!v.question_queue.is_empty());
        v.update(&resolved_ev("c1")); // answered/expired — clear
        assert!(v.question_queue.is_empty());
    }

    #[test]
    fn existing_thread_starts_in_read_w_opens_compose_esc_returns() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view(); // subject "t" → Read
        assert_eq!(v.mode, Mode::Read);
        // 'w' opens the compose box; Esc (empty) returns to the reader.
        v.update(&plain(KeyCode::Char('w')));
        assert_eq!(v.mode, Mode::Compose);
        assert!(v.update(&plain(KeyCode::Esc)).is_none());
        assert_eq!(v.mode, Mode::Read);
        // Esc from the reader leaves the thread.
        assert_eq!(v.update(&plain(KeyCode::Esc)), Some(ViewAction::Pop));
    }

    #[test]
    fn sending_a_reply_returns_to_the_reader() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.enter_compose();
        v.message_input = "go".into();
        assert!(matches!(
            v.update(&plain(KeyCode::Enter)),
            Some(ViewAction::LaunchJob { .. })
        ));
        assert_eq!(
            v.mode,
            Mode::Read,
            "after launching, back to the transcript"
        );
    }

    /// A view over a thread saved in a temp store, ready for on_enter reconcile.
    fn view_over(t: Thread) -> (tempfile::TempDir, ThreadStore, ThreadView) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        store.save(&t).unwrap();
        let v = ThreadView::with_thread(t, store.clone(), "orch".into(), vec![]);
        (tmp, store, v)
    }

    fn reconciles(actions: &[ViewAction]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, ViewAction::Fetch(FetchRequest::ReconcileThread { .. })))
    }

    fn refreshes_jobs(actions: &[ViewAction]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, ViewAction::Fetch(FetchRequest::RefreshThreadJobs { .. })))
    }

    #[test]
    fn on_enter_always_refreshes_thread_jobs() {
        // So ^D / stop resolve the thread's running job from the server, even
        // when nothing is locally recorded as pending.
        let mut t = Thread::new("s");
        t.push_message(Message::now("user", "q"));
        let (_tmp, _s, mut v) = view_over(t);
        assert!(refreshes_jobs(&v.on_enter()));
    }

    #[test]
    fn on_enter_reconciles_a_pending_job() {
        let mut t = Thread::new("s");
        t.push_message(Message::now("user", "q")); // awaiting a reply
        t.pending_job = Some("job-9".into());
        let (_tmp, _s, mut v) = view_over(t);
        let actions = v.on_enter();
        assert!(refreshes_jobs(&actions));
        match actions.iter().find_map(|a| match a {
            ViewAction::Fetch(FetchRequest::ReconcileThread { job_id, .. }) => Some(job_id),
            _ => None,
        }) {
            Some(job_id) => assert_eq!(job_id, "job-9"),
            None => panic!("expected a ReconcileThread fetch, got {actions:?}"),
        }
    }

    #[test]
    fn on_enter_places_cursor_on_a_visible_row() {
        // Regression: the cursor started at `None` on entry, so nothing was
        // highlighted until the reader pressed ↑ then ↓. It must land ON a row.
        let mut t = Thread::new("s");
        let root = t.reply(None, "user", "root question");
        t.reply(Some(&root), "assistant", "the newest turn");
        let (_tmp, _s, mut v) = view_over(t);
        v.on_enter();
        assert!(
            v.selected.is_some(),
            "cursor must be visible on entry, not None"
        );
    }

    #[test]
    fn select_down_from_nothing_selects_the_top_row() {
        // ↓ from an empty selection used to be a no-op (the "press ↑ first" bug);
        // it now selects the top row so the first keypress moves the cursor.
        let (_t, mut v) = view();
        v.thread.reply(None, "user", "root");
        v.selected = None;
        v.select_down();
        assert_eq!(v.selected, Some(0));
    }

    #[test]
    fn on_enter_skips_reconcile_without_a_pending_job() {
        // Awaiting a reply but no recorded job id → nothing to reconcile against
        // (but the job refresh still fires).
        let mut t = Thread::new("s");
        t.push_message(Message::now("user", "q"));
        let (_tmp, _s, mut v) = view_over(t);
        let actions = v.on_enter();
        assert!(refreshes_jobs(&actions));
        assert!(!reconciles(&actions));
    }

    #[test]
    fn on_enter_skips_reconcile_when_reply_already_present() {
        // A completed turn (last message is the reply) → not awaiting, no reconcile.
        let mut t = Thread::new("s");
        t.push_message(Message::now("user", "q"));
        t.push_message(Message::now("assistant", "a"));
        let (_tmp, _s, mut v) = view_over(t);
        let actions = v.on_enter();
        assert!(refreshes_jobs(&actions));
        assert!(!reconciles(&actions));
    }

    #[test]
    fn thread_reconciled_event_reloads_the_reply() {
        let mut t = Thread::new("s");
        t.push_message(Message::now("user", "q"));
        t.pending_job = Some("job-9".into());
        let (_tmp, store, mut v) = view_over(t.clone());
        assert_eq!(v.thread.messages.len(), 1);
        // The reconcile task appended the reply to the store out-of-band…
        store.append_reply(&t.id, "recovered answer", "job-9", None);
        // …and signalled the view, which reloads to show it.
        let ev = AppEvent::Data(crate::cli::tui::event::DataEvent::ThreadReconciled {
            thread_id: t.id.clone(),
        });
        assert!(v.update(&ev).is_none());
        assert_eq!(v.thread.messages.len(), 2);
        assert_eq!(v.thread.messages[1].content, "recovered answer");
    }

    #[test]
    fn new_thread_starts_in_compose() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let v = ThreadView::with_thread(Thread::new(""), store, "orch".into(), vec![]);
        assert_eq!(v.mode, Mode::Compose);
    }

    fn paste(text: &str) -> AppEvent {
        AppEvent::Terminal(crossterm::event::Event::Paste(text.into()))
    }

    #[test]
    fn large_paste_collapses_to_placeholder_expands_on_send() {
        let (_t, mut v) = view();
        v.enter_compose();
        v.update(&paste("line1\nline2\nline3"));
        assert!(v.message_input.contains("[paste #1: 3 lines]"));
        assert_eq!(v.pastes.len(), 1);
        v.message_input.push_str(" summarize"); // typed after the paste
        match v.send().expect("launches") {
            ViewAction::LaunchJob { task, .. } => {
                assert!(task.contains("line2"), "paste expanded into the turn");
                assert!(task.contains("summarize"));
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
        assert!(v.pastes.is_empty(), "pastes cleared on send");
    }

    #[test]
    fn small_single_line_paste_goes_inline() {
        let (_t, mut v) = view();
        v.enter_compose();
        v.update(&paste("hi there"));
        assert_eq!(v.message_input, "hi there");
        assert!(v.pastes.is_empty());
    }

    #[test]
    fn cr_delimited_paste_counts_lines() {
        // Terminals send CR (or CRLF) for pasted newlines; the placeholder must
        // report the real line count, not collapse to "1 line".
        let (_t, mut v) = view();
        v.enter_compose();
        v.update(&paste("line1\rline2\r\nline3"));
        assert!(
            v.message_input.contains("[paste #1: 3 lines]"),
            "got {}",
            v.message_input
        );
    }

    #[test]
    fn ctrl_w_on_paste_block_drops_stored_text_from_send() {
        let (_t, mut v) = view();
        v.enter_compose();
        v.update(&paste("secret1\nsecret2\nsecret3")); // → placeholder, cursor after it
        v.update(&ctrl('w')); // deletes the whole block
        assert!(v.message_input.is_empty() && v.pastes.is_empty());
        // Type a real message and send — the removed paste must not resurface.
        for c in "hello".chars() {
            v.update(&plain(crossterm::event::KeyCode::Char(c)));
        }
        match v.send().expect("launches") {
            ViewAction::LaunchJob { task, .. } => {
                assert!(task.contains("hello"));
                assert!(!task.contains("secret2"), "dropped paste must not expand");
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }

    #[test]
    fn backspace_deletes_a_paste_block_whole() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.enter_compose();
        v.update(&paste("a\nb\nc"));
        assert_eq!(v.pastes.len(), 1);
        v.update(&plain(KeyCode::Backspace)); // one press removes the whole block
        assert!(v.message_input.is_empty());
        assert!(v.pastes.is_empty());
    }

    #[test]
    fn ctrl_w_deletes_trailing_word_or_paste_block() {
        let (_t, mut v) = view();
        v.enter_compose();
        v.message_input = "hello world".into();
        v.cursor = v.message_input.len(); // cursor at end
        v.update(&ctrl('w'));
        assert_eq!(v.message_input, "hello ");
        // With a trailing paste block, Ctrl-W removes the block whole.
        v.message_input.clear();
        v.cursor = 0;
        v.update(&paste("x\ny"));
        v.update(&ctrl('w'));
        assert!(v.message_input.is_empty() && v.pastes.is_empty());
    }

    #[test]
    fn cursor_word_and_char_navigation() {
        use crossterm::event::KeyCode;
        let (_t, mut v) = view();
        v.enter_compose();
        for c in "foo bar".chars() {
            v.update(&plain(KeyCode::Char(c)));
        }
        assert_eq!(v.cursor, 7); // at end
        // Ctrl-Left jumps to the start of "bar".
        v.update(&ctrl_arrow(true));
        assert_eq!(v.cursor, 4);
        // Left one char → into "foo ".
        v.update(&plain(KeyCode::Left));
        assert_eq!(v.cursor, 3);
        // Home / End.
        v.update(&plain(KeyCode::Home));
        assert_eq!(v.cursor, 0);
        v.update(&plain(KeyCode::End));
        assert_eq!(v.cursor, 7);
        // Insert mid-string: Ctrl-Left to "bar", type X → "foo Xbar".
        v.update(&ctrl_arrow(true));
        v.update(&plain(KeyCode::Char('X')));
        assert_eq!(v.message_input, "foo Xbar");
    }

    #[test]
    fn ctrl_c_cancels_the_running_job() {
        let (_t, mut v) = view();
        v.thread.push_message(Message::now("user", "q"));
        v.thread.pending_job = Some("job-run".into());
        match v.update(&ctrl('c')).expect("cancels") {
            ViewAction::Fetch(FetchRequest::CancelJob { job_id, .. }) => {
                assert_eq!(job_id, "job-run")
            }
            other => panic!("expected CancelJob, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_is_a_noop_without_a_running_job() {
        let (_t, mut v) = view();
        assert!(v.update(&ctrl('c')).is_none());
    }

    #[test]
    fn ctrl_p_opens_the_model_picker() {
        let (_t, mut v) = view();
        assert!(v.update(&ctrl('p')).is_none());
        assert_eq!(v.picking, Some(0));
    }

    #[test]
    fn up_selects_transcript_and_ctrl_d_opens_that_turns_detail() {
        let (_t, mut v) = view();
        v.thread.push_message(Message::now("user", "q"));
        let mut reply = Message::now("assistant", "a");
        reply.job_id = Some("job-42".into());
        v.thread.push_message(reply);

        // Row 0 is the pinned root (user "q"); the assistant reply is row 1.
        v.update(&arrow(true)); // ↑ from nothing → top row (root)
        assert_eq!(v.selected, Some(0));
        v.update(&arrow(false)); // ↓ → the assistant reply (job-42)
        assert_eq!(v.selected, Some(1));
        // Ctrl-D on it opens that turn's deliberation detail (not the running job).
        match v.update(&ctrl('d')).expect("opens detail") {
            ViewAction::Push(crate::cli::tui::app::ViewId::JobDetail { job_id, .. }) => {
                assert_eq!(job_id, "job-42")
            }
            other => panic!("expected Push(JobDetail), got {other:?}"),
        }
        // ↓ past the oldest clears the selection.
        v.update(&arrow(false));
        assert_eq!(v.selected, None);
    }

    #[test]
    fn ctrl_d_requests_the_thread_job_detail() {
        let (_t, mut v) = view();
        let ctrl_d = AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
        ));
        match v.update(&ctrl_d).expect("opens detail") {
            ViewAction::OpenThreadJob {
                thread_id,
                orchestrator,
            } => {
                assert_eq!(thread_id, v.thread.id);
                assert_eq!(orchestrator, "orch");
            }
            other => panic!("expected OpenThreadJob, got {other:?}"),
        }
    }

    #[test]
    fn reloads_thread_on_job_complete_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut t = Thread::new("t");
        t.push_message(Message::now("user", "q"));
        store.save(&t).unwrap();
        let mut v = ThreadView::with_thread(t.clone(), store.clone(), "orch".into(), vec![]);
        assert_eq!(v.thread.messages.len(), 1);

        // Reply lands in the store (as the loop does), then a JobComplete arrives.
        store.append_reply(&t.id, "the answer", "job-1", None);
        let done = AppEvent::Data(crate::cli::tui::event::DataEvent::SseEvent(
            crate::cli::tui::event::SseEvent::JobComplete {
                status: "success".into(),
                job_id: "job-1".into(),
                rounds_completed: 1,
                best_proposal_content: "the answer".into(),
                best_proposal_score: 1.0,
                best_proposal_author: "a".into(),
            },
        ));
        assert!(v.update(&done).is_none());
        assert_eq!(v.thread.messages.len(), 2, "reply reloaded inline");
        assert_eq!(v.thread.messages[1].content, "the answer");
    }

    #[test]
    fn tab_toggles_compose_focus_only_while_new() {
        // A new (subjectless) thread: Tab toggles Subject↔Message.
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut v = ThreadView::with_thread(Thread::new(""), store, "orch".into(), vec![]);
        assert_eq!(v.focus, Focus::Subject);
        let tab = AppEvent::Terminal(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        v.update(&tab);
        assert_eq!(v.focus, Focus::Message);

        // Once the subject is set, Tab is a no-op (subject immutable).
        let (_t2, mut v2) = view(); // subject "t"
        assert_eq!(v2.focus, Focus::Message);
        v2.update(&tab);
        assert_eq!(v2.focus, Focus::Message);
    }

    #[test]
    fn effort_key_cycles_and_is_carried_into_launch() {
        assert_eq!(ThreadView::next_effort(None), Some(0.3));
        assert_eq!(ThreadView::next_effort(Some(0.3)), Some(0.6));
        assert_eq!(ThreadView::next_effort(Some(0.6)), Some(0.9));
        assert_eq!(ThreadView::next_effort(Some(0.9)), None);

        let (_t, mut v) = view();
        v.effort = Some(0.6);
        v.message_input = "go".into();
        match v.send().expect("launches") {
            ViewAction::LaunchJob {
                effort_override, ..
            } => {
                assert_eq!(effort_override, Some(0.6))
            }
            other => panic!("expected LaunchJob, got {other:?}"),
        }
    }
}
