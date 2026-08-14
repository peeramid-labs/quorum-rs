---
title: Thread TUI roadmap
order: 16
tagline: Roadmap for an email-style, branching, newest-on-top conversation thread view.
---

# Thread TUI roadmap

Turns the thread reader into an email-style, branching conversation view.

## Locked design

- **Newest-on-top, flat.** Messages render newest-first (latest at the top, no
  scrolling to reach it). Reading *down* goes older / toward the root.
- **`↳ re:` tags, no depth indent.** Each message tags its parent
  (`↳ re: <preview>`, which sits below it). Indentation is reserved *only* for
  distinguishing **forked** branches.
- **Collapsed by default.** First-line preview + `[N ▸]` on nodes with hidden
  descendants; arrow to expand; full-view for any message.
- **Timestamps** to the minute: `[MM-DD, HH:MM]`.
- **Reply at cursor.** `w` on any message composes a reply *under that node*.
  The deliberation context is the root→node path (`to_deliberation_query_from`).
- **Per-branch session identity.** `branch_id` **is** the `conversation_id`.
  Replying to a leaf continues that branch (session resume); replying under a
  non-leaf node forks → fresh `branch_id`/`conversation_id`, cold-seeded from the
  path (claude-code resume is linear, so forks replay context).
- **Merged inbox (later).** Subject list + thread reader fold into one screen.

## Roadmap

- **T017 — data model.** DONE (`edb6c38`). `Message.id/parent_id/branch_id`;
  `Thread::{get,children,is_leaf,path_to_root,tip,reply}`;
  `to_deliberation_query_from(parent)`; `migrate_linear` on load; `push_message`
  linear-append auto-link. Fully unit-tested in `cli/thread.rs`.
- **T018 — reply-at-cursor.** DONE. Compose a child under the selected node; carry the
  parent id + branch through `ViewAction::LaunchJob` into the deliberation
  (`conversation_id = branch_id`); a top-level compose starts a new root branch;
  fork detection (reply under a non-leaf). Update the send path + `mod.rs`
  JobSubmitted/JobComplete wiring; append the user turn via `Thread::reply`.
- **T019 — collapsible newest-on-top reader.** DONE (collapsed rows; expand/collapse; ▲/▼ indicators; fork indent by fork_depth; `⑂N` marker at fork points; collapsed rows drop trailing blanks so End reaches the oldest). Rewrite the transcript render:
  newest-first, `↳ re:` tags, `[N ▸]` fold counts, timestamps, fork indent,
  expand/collapse (→/←/Enter). **Fold in the scroll fix**: last-message boundary
  bug + ▲/▼ scrollbar indicators (items above/below).
- **T020 — full-message view.** DONE. `o` opens the selected message full-screen (header + scrollable content); ↑↓/PgUp/PgDn scroll, Esc/q back.
- **T021 — polish.** IN PROGRESS: `n` new-root turn done (fresh line under the subject, rooted at nothing). TODO: local time (chrono `clock` feature), empty-state/keymap cohesion.
- **T022 — merged subject+thread inbox.** IN PROGRESS: inbox rows + a live peek pane (selected thread's newest turns shown below the list — one screen, see the latest without opening). Push-nav (list→reader) covers the rest of the email UX; full inline-navigation within the peek deferred (uncertain ROI vs push-nav).

## For the /loop

Each iteration: `git log feat/thread-tui` for the last `Txxx` commit, pick the
next incomplete piece, implement TDD (`cargo test -p quorum-rs --features tui`),
clippy+fmt+typos, push to forgejo `feat/thread-tui`. If a piece blocks, note it
here and move to the next tractable one.
