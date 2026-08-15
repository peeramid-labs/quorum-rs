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
- **Merged inbox.** Subject list + thread reader fold into one screen.

## How the behaviour is built up

- **Data model.** A `Message` carries `id`, `parent_id`, and `branch_id`; a
  `Thread` exposes `get`, `children`, `is_leaf`, `path_to_root`, `tip`, and
  `reply`. `to_deliberation_query_from(parent)` builds the root→node context,
  `migrate_linear` upgrades a flat transcript on load, and `push_message`
  linear-appends with auto-linking.
- **Reply-at-cursor.** Composing a child under the selected node carries the
  parent id + branch into the deliberation (`conversation_id = branch_id`). A
  top-level compose starts a new root branch; replying under a non-leaf node is
  detected as a fork. The user turn is appended via `Thread::reply`.
- **Collapsible newest-on-top reader.** The transcript renders newest-first with
  `↳ re:` tags, `[N ▸]` fold counts, timestamps, and fork indent by fork depth
  (`⑂N` marker at fork points). Rows expand/collapse (→/←/Enter), collapsed rows
  drop trailing blanks so End reaches the oldest message, and ▲/▼ indicators show
  items above/below the viewport.
- **Full-message view.** `o` opens the selected message full-screen (header +
  scrollable content); ↑↓/PgUp/PgDn scroll, Esc/q return.
- **New-root turn.** `n` starts a fresh line under the subject, rooted at
  nothing.
- **Merged subject+thread inbox.** Inbox rows sit above a live peek pane that
  shows the selected thread's newest turns below the list, so the latest is
  visible on one screen without opening it. Push-navigation moves from the list
  into the reader.
