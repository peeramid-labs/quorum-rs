# Use the thread TUI

> Recipe for holding a branching, email-style conversation with a
> quorum deliberation. Assumes `quorum` is installed and pointed at
> an orchestrator (a room/policy configured, or the default remote).
> The *why* behind branching and per-branch sessions lives in
> [Thread tree TUI](../explanation/thread-tree-tui.md).

## What a thread is

A durable, client-owned transcript of a conversation whose replies
are deliberations. It's a **tree**, not a flat log: each turn roots
under the one you replied to, so you can continue a line, fork off an
older turn, or start a fresh line under the same subject. Each branch
carries its own session (`conversation_id`), so a fork doesn't disturb
the branch it came from.

## Open the inbox

```sh
quorum        # opens the TUI on the Threads tab
```

The list is your inbox — newest-activity first. Each row is
`[MM-DD HH:MM] <subject>  ·  [role]: <newest-turn preview>  (<count>)`.
The pane below previews the selected thread's most recent turns, so you
see the latest exchange without opening it.

- `↑↓` — move between threads
- `Enter` — open the selected thread
- `n` — start a new thread
- `d` — delete the selected thread (`d`/`y` confirms, any other key cancels)

## Start a thread

1. Press `n`. The compose box opens.
2. Type a **subject**, `Tab` to the message, type your first turn.
3. `Enter` sends. The subject is fixed once set; the **live deliberation detail
   opens automatically** so you can watch the rounds. `Esc` returns to the
   thread — the run keeps going and its answer lands as a `[noosphera]` turn.

## Read a thread

Messages are newest-on-top, one collapsed line each:

```
▸▾ [07-07 13:42] [noosphera] here's the summary…  ⑂2  ↳ re: what changed?
```

- `↑↓` — pick a message
- `Enter` / `→` — expand the selected message inline; `←` collapses
- `o` — open it full-screen (scroll with `↑↓`/`PgUp`/`PgDn`, `Esc` back)
- `↳ re:` shows which turn a reply answers; `⑂N` marks a fork point with
  `N` branches; forked branches are indented, linear turns are not
- `▲N`/`▼N` in the title count lines hidden above/below the viewport

## Continue, fork, or branch

The turn you compose roots under wherever the cursor is:

- **Continue the conversation** — press `w` without selecting anything.
  The reply continues from the newest turn (same branch → the session
  resumes).
- **Fork off an older turn** — `↑↓` to that turn, then `w`. The reply
  roots under it and carries only *that* turn's lineage as context; the
  compose header shows `↳ replying under [role] …`. Replying under a
  turn that already has replies starts a **new branch** (a fresh session,
  cold-seeded from the path).
- **Start a fresh line** — press `n` inside a thread. A new root turn
  under the same subject, rooted at nothing.

## Steer a running deliberation

In the deliberation detail, press `/` to open the steering input and inject a
message into the live rounds. It's a line editor: `^←`/`^→` jump by word,
`Home`/`End` snap to the ends, and a **paste inserts at the caret** (multi-line
kept). Press `^F` for a full-screen editor when the message is large; `Esc`
leaves full-screen, then closes the input. `Enter` sends.

## Stop a runaway deliberation

`^C` (or `x`, if your terminal/tmux swallows Ctrl keys) cancels the running
deliberation — from the reader or the detail view (a compliance kill-switch too).
The turn is left unanswered; write a follow-up to continue. When a turn has
already finished with no answer, `^D`/`Enter` shows `⚠ No running deliberation`.

## Swap the model or effort

- `^P` (or type `/model`) — pick the policy acting as the model
- `^E` — cycle the effort (convergence threshold)

Both show in the footer. Policy can change mid-thread.
