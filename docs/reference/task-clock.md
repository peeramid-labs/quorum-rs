---
title: Task Clock
order: 14
tagline: The date a task was issued, and why it is stated to the model rather than offered as a tool.
---
# Task clock

Every task may carry the date it was issued. When it does, the agent renders
a `<clock>` block at the top of the user message, ahead of the turn header.

## Wire form

`AgentContext.issued_at` — `Option<String>`, a `YYYY-MM-DD` date stamped by
the caller that publishes the task. Absent by default; a task that carries
none renders no clock at all, so a client that predates the field produces
byte-identical prompts.

```rust
AgentContext {
    issued_at: Some("2026-08-22".to_string()),
    ..context
}
```

## Rendered block

`prompts::clock_block(issued_at)` returns the empty string for `None`, and
otherwise:

```
<clock>
The current date is 2026-08-22. Your training data ends before this, so any
fact, price, version or event you recall unaided is out of date by an unknown
margin.
State the period a figure describes rather than presenting it as current, and
prefer a source you can retrieve now over one you remember.
</clock>
```

It rides in the user message, not the system prompt. The system prefix is
kept byte-identical across a session so the provider's prompt cache is reused
rather than re-billed, and a date is not constant.

## Why a statement and not a tool

A tool only helps if the model calls it, and a model does not know that its
own recall is stale — that is the nature of the gap. Left to itself it
answers a question about current prices from training data and presents the
figure as today's, with no signal that it consulted memory rather than the
world.

The date alone is close to inert for the same reason: `Today: 2026-08-22`
does not tell a model anything about its own limits. What changes the answer
is stating the implication — that unaided recall is old — which turns a
confident wrong figure into a figure labelled with the period it describes.

## Why the caller stamps it

The date is issued by whoever publishes the task, never read from the agent's
own clock. Agents run wherever their operator runs them, so a per-agent clock
lets two seats in the same round disagree about what day it is, and puts the
value outside the control of the system that has to reason about it. Stamping
at publish time also records in the deliberation history what the agents were
actually told, rather than what a replay machine's clock says later.
