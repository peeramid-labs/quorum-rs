# About threading a conversation into one task

Threading is the point: a council should be able to answer "yo what about
forests?" because it remembers what came before. The difficulty is that a job
carries **one** task string, so a thread has to be flattened into it — and there
is currently more than one convention for doing that, none of them marking which
turn is the live one.

This is about which convention to keep and what the flattened text should look
like. It is not a how-to; nothing here is wired yet.

## There are two formats, and they disagree

**`[role]` tags**, produced by the OpenAI-compatible layer when a client posts a
`messages` array:

```text
[user] What is Rust?

[assistant] Rust is a systems programming language…

[user] How does it compare to Go?
```

A single user message deliberately gets **no** prefix, for backward
compatibility — so the tag appears only from the second turn onward.

**A prose scaffold**, produced by the client instead, which is what production
threads actually send:

```text
Context — this is a follow-up. The conversation so far:

1. You asked: what is the sound of one hand clap?
   Council answered: … (≈1 400 characters)

2. You asked: yo what about forests?
   Council answered: … Forests are where the universe stops being a math
   problem and starts being an experience.

---

Follow-up:
Should we trust life-altering decisions (in medicine, law, and warfare) to
systems that even their creators cannot fully explain?
```

Two live jobs, inspected: neither contained a single `[user]` tag, and both
contained `You asked:` and `Follow-up:`. So the `[role]` convention — the one the
compat layer documents and tests — is not the one in use. A client that flattens
its own thread and posts it as one message never reaches that code path, because
one message means no prefix.

That is the confusion worth resolving first: **`[role]` is a format for callers
who hand over a `messages` array and let the orchestrator flatten. It is not the
format for a caller that flattens its own thread.** Two conventions for one job
means neither can be relied on downstream.

## Why either shape loses the question

Whichever convention produces it, the flattened string is wrapped as
`<task>"…"</task>` and the agent is told to *"Solve the provided task."* The task
is therefore the entire transcript. Measured on the two jobs above:

| | total | the live question | question share |
| --- | --- | --- | --- |
| earlier turn | 1 718 chars | `yo what about forests?` | **1.3 %** |
| next turn | 4 698 chars | the trust question | **3 %** |

The council duly answered the transcript. From that run: one agent's plan ended
`Conclusion: Re-link to the previous themes (the forest/the koan)`; its answer
closed `The sound of one hand clapping? It's the silence after a bomb drops`; a
third asked the user whether to prioritise `the Zen/forest metaphor` while
evaluating a question about medicine, law and warfare. None of that is
misbehaviour — each agent weighed the text it was told to solve.

Four properties of the shape cause it, and they apply to `[role]` tags equally:

**The question is buried by construction.** It sits last, after the longest
material in the payload. Recency is positional only.

**Prior answers outweigh the question.** Every answer block dwarfs every
question, so the council's own previous output becomes the bulk of its next
instruction — and reads as established substance rather than a draft it produced.

**A flat list asserts equality.** `1.`, `2.`, `3.` — or a run of `[user]` tags —
are peers. The live turn differs only in position.

**Roles are prose to be inferred.** Contrast `<user_updates>`, which this
codebase already gets right:

```text
<user_updates>
  The user provided the following clarifications during deliberation.
  Integrate these into your work — later updates take priority.
  <update round="2">actually, focus on the legal angle</update>
</user_updates>
```

A named element, an explicit instruction on how to weigh it, and recency as
**data** (`round="2"`) rather than position. That is the missing pattern.

It also compounds: each answer becomes the next turn's context, so 1 718 grew to
4 698 in two turns while the question stayed one line. In that example turns 1
and 2 are the *same question* — flattening carries duplicates through as fact.

## The shape to aim for

Keep threading. Separate the three things now fused into one string:

```text
<task>Should we trust life-altering decisions (in medicine, law, and warfare)
to systems that even their creators cannot fully explain?</task>

<conversation_background>
  Earlier turns of this thread, for continuity only. Do NOT answer these; they
  are already answered. Use them to resolve references in the task ("it",
  "that", "as you said") and to avoid repeating yourself.
  <turn n="1" asked="what is the sound of one hand clap?">
    A Zen koan; the council concluded the question dissolves rather than
    resolves.
  </turn>
  <turn n="2" asked="yo what about forests?">
    Answered across a physical, a perceptual and a Zen layer.
  </turn>
</conversation_background>
```

Why each part earns its place:

- **`<task>` holds only the live question**, so *"solve the provided task"* has
  an unambiguous referent.
- **Background is named as background**, with an explicit prohibition against
  answering it. Without that sentence a model will still try to be helpful about
  turn 1.
- **Prior answers are summarised, not pasted.** Continuity needs what was
  *settled*, not the full text. This is the only part that costs anything to
  build, and it is what stops the payload compounding.
- **Turns keep their numbers**, so "your second answer" resolves.

Watch the ratio rather than the wording: if the live question is a single-digit
percentage of the payload, the structure is wrong however good the prose.

## Pick one convention

`[role]` tags and the prose scaffold should not both survive. Of the two, the
tagged form is the better base — it is machine-generated, already tested, and
role-labelled — but it needs the two things it lacks: a marker for the live turn,
and summarised rather than verbatim history. A `<task>` plus
`<conversation_background>` split gives both, and a client that already flattens
its own thread can emit it directly.

## The machinery already exists, unused at both ends

Two slots are built for precisely this:

- `JobPayload.conversation_id` and `JobPayload.new_turn` thread into
  `AgentContext`. **Both were `None`** on the jobs inspected — the client
  flattens instead of using them.
- `AgentContext::delta_task()` returns `new_turn` when present and the full
  `task_description` otherwise. Its docstring: this "stops a resumed thread from
  re-sending its whole flattened history."

Closing the gap needs both ends, and neither alone suffices:

1. **Clients** send the live question as the query (or `new_turn`), and prior
   turns as structured, summarised background.
2. **The default agent must honour `delta_task()`.** Today only the MCP agent
   path calls it — `ProposerEvaluatorAgent` reads `task_description` directly, so
   a correctly-structured request would still be flattened at the prompt.

## What not to do instead

**Don't drop the history.** "yo what about forests?" is unanswerable alone. The
problem is weight and labelling, not presence.

**Don't summarise silently.** If background is condensed, say so in the prompt,
or an agent will quote a summary back as the user's words.

**Don't rely on ordering.** "Last one wins" is not a property a model reliably
extracts from a list. Name the live turn.

## Related

- [Rooms and policies](rooms-and-policies.md) — a room is an access boundary,
  not a conversation.
- [Compact history and the scratchpad squeeze](compact-history-and-scratchpad-squeeze.md)
  — the same pressure inside one deliberation, once tool output crowds out the
  task.
