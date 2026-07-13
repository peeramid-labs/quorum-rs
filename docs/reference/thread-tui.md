# Thread TUI reference

Authoritative surface of the `quorum` thread TUI: keys per screen, line
formats, and the persisted data model. Task-oriented usage is in
[Use the thread TUI](../how-to/use-the-thread-tui.md); the design rationale is in
[Thread tree TUI](../explanation/thread-tree-tui.md).

## Screens and keys

### Inbox (Threads tab)

| Key | Action |
|---|---|
| `↑` / `↓` | Move the selection |
| `Enter` | Open the selected thread |
| `n` | Start a new thread |
| `d` | Delete the selected thread — `d`/`y` confirms, any other key cancels |
| `q` / `Esc` | Quit |

A live preview pane shows the selected thread's newest turns.

### Reader

| Key | Action |
|---|---|
| `↑` / `↓` | Select a message |
| `Enter` / `→` | Expand the selected message's full content inline |
| `←` | Collapse it |
| `o` | Open the selected message full-screen |
| `w` / `r` / `i` | Compose a reply under the selected turn (else the tip) |
| `n` | Compose a new root turn (fresh line under the subject) |
| `PgUp` / `PgDn` | Scroll the transcript |
| `Home` / `End` | Scroll to newest / oldest |
| `^D` | Open the selected turn's deliberation detail |
| `^C` | Cancel the running deliberation |
| `^P` / `/model` | Pick the policy (model) |
| `^E` | Cycle the effort |
| `Esc` | Back to the inbox |

### Compose

| Key | Action |
|---|---|
| `Enter` | Send (empty `Enter` on an awaiting thread re-checks for a lost reply) |
| `←` / `→` | Move the caret by character |
| `^←` / `^→` | Move by word |
| `Home` / `End` | Caret to start / end |
| `Backspace` | Delete the char (or a whole paste block) before the caret |
| `^W` | Delete the previous word (or a paste block) |
| `Tab` | Switch Subject ↔ Message (new thread only) |
| `Esc` | Back to the reader (draft saved) |

Large or multi-line pastes collapse to a `[paste #N: L lines]` marker, expanded
on send.

### Full-message view

`↑` / `↓` / `PgUp` / `PgDn` / `Home` scroll; `Esc` / `q` returns to the reader.

### Deliberation detail

Opened automatically when a turn is sent (and via `^D` from the reader). Shows
the live rounds — proposals, evaluations, convergence.

| Key | Action |
|---|---|
| `Tab` | Cycle panels |
| `↑` / `↓` | Scroll the active panel |
| `d` / `Enter` | Open the selected proposal/evaluation detail |
| `←` / `→` / `PgUp` / `PgDn` | Step through rounds (`=` back to live) |
| `t` | Toggle thought process |
| `/` | Steer — open the injection input (running only) |
| `^C` | Stop (cancel) the running deliberation |
| `Esc` / `q` | Back to the thread |

#### Steer (injection input)

Activated with `/` while the job runs. A line editor:

| Key | Action |
|---|---|
| `Enter` | Send the steering message into the deliberation |
| `^F` | Toggle a full-screen editor (room for a large / multi-line paste) |
| `←` / `→` | Move the caret by character |
| `^←` / `^→` | Move the caret by word |
| `Home` / `End` | Caret to start / end |
| `Backspace` | Delete the char before the caret |
| paste | Bracketed paste inserts at the caret (newlines preserved) |
| `Esc` | Leave full-screen, then close the input |

## Line formats

Reader order: each thread **root is pinned at the top** (oldest first), then its
descendants **newest-first** below, with every fork's subtree grouped and
indented. Reader row (one line collapsed):

```
{mark} {fold} [MM-DD HH:MM] {role}  {indent}{preview}{  ⑂N}{  ↳ re: parent}{  (^D)}
```

- `mark` — `❯` on the selected row, `●` on a thread root (no parent), else a space
- `fold` — `▾` expanded, `▸` collapsed-with-more, `·` if nothing to expand
- `role` — `you` (operator), or `noosphera` for an assistant turn (padded, colour-coded)
- `indent` — two spaces per fork depth, before the preview (linear turns have none)
- `⑂N` — present when the turn has `N > 1` direct replies (a fork point)
- `↳ re:` — the parent turn's preview, shown **only for a fork** (the parent isn't the row directly below); a linear reply omits it since its parent is the next line

Inbox row: `[MM-DD HH:MM] <subject>  ·  [role]: <newest preview>  (<count>)`.

Timestamps are UTC (`chrono` is built without the `clock` feature).

## Data model (persisted per thread)

One JSON file per thread under `~/.nsed/threads/{id}.json`
(`$NSED_THREAD_DIR` overrides the directory).

`Message`:

| Field | Meaning |
|---|---|
| `id` | Stable node id (empty on pre-tree JSON; backfilled on load) |
| `parent_id` | Parent node; `None` for a root turn |
| `branch_id` | Per-branch identity = the `conversation_id`; a leaf reply inherits it, a fork gets a fresh one |
| `role` | `user` / `assistant` |
| `content` | Turn text |
| `policy_id` | Policy that produced an assistant turn |
| `job_id` | Deliberation that produced the turn |
| `ts` | Unix seconds |

`Thread`:

| Field | Meaning |
|---|---|
| `id` `subject` `created` `updated` | Identity + timestamps |
| `active_policy` | Current model (policy) |
| `pending_job` | Job id of an in-flight turn awaiting its reply; cleared when the reply lands or the job is found dead on reconcile |
| `draft` | Unsent compose text (pastes expanded), persisted on every edit; restored on reopen, cleared on send |
| `messages` | The turns, a tree via `parent_id` |

## Deliberation context and sessions

A sent turn's task is the flattened **root→parent path** (not the whole thread),
subject-prefixed. Its `conversation_id` is the turn's `branch_id`: a reply to a
leaf reuses the branch (the claude session resumes); a reply under a non-leaf
node starts a fresh branch (a new session, cold-seeded from the path). A cancel
or a dead job leaves the turn reply-less, so a follow-up flattens the sent turn
plus the new one (`…[user] stopped [user] follow-up`).
