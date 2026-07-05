# Policy-as-model and the thread

This is an *explanation* document — the reasoning behind how the interactive
client models a conversation. Its companion,
[Understanding rooms and policies](rooms-and-policies.md), explains the two
server concepts; this one explains how the **client** should present them.

## Framing: AI email, not chat

The product is positioned as **AI email**, not a chat client — that is the
differentiator. The naming follows: a user-facing conversation is a **thread**
(email vocabulary — thread, message, subject), never a "chat". The word "chat"
is reserved for the *category we are not*. Internally, the engine under a thread
stays a **deliberation**. Keep that split when naming anything user-visible.

The one-line version:

> **Policy is the model. A thread is the conversation.** A user picks a policy
> the way they pick a model, types a question, and gets an answer; continuing
> the thread, resuming it later, and swapping the policy mid-thread are all
> properties of a client-owned *thread*, not of a room they must choose first.

## Two axes, three vocabularies

Every surface already encodes the same two orthogonal questions under different
names. The confusion is purely lexical:

| Axis | REST | Completions API | Interactive client |
|---|---|---|---|
| **How it runs** (recipe) | `policy_id` | `model` (`nsed:<tag>`) | policy picker → *the "model"* |
| **Where it lives / resume / who sees** | `room` | `room_id` = session id (`x-nsed-session-id`) | *the thread* |

The Chat Completions layer already made the call: a client sends a `model`
(= policy) and a message array; the server auto-mints the `room_id` as a
thread/session id and returns it as `x-nsed-session-id` so the client can resume.
The server code calls it `session_id` / `thread_prefix` /
`find_active_job_for_session`. So **room and thread are the same underlying
thing** — "thread" is the user-facing name for it.

## Why the client owns the transcript (Completions, not Responses)

Two continuation models exist. They are not equivalent for a thread client:

- **Chat Completions** — the client holds the full message array and resends it.
  Server-side, a new turn folds the messages into the single task string the
  deliberation core consumes (see [why the `[role]` flatten
  exists](#a-note-on-the-role-flatten)). The client is the source of truth for
  the conversation.
- **Responses API** — the server holds the thread; the client sends
  `previous_response_id`. Parameters are *locked* for the thread's life — a
  follow-up cannot change the effort or the policy.

For a thread that must **store, restore, and swap policy mid-conversation**, the
client must own the transcript:

- **Restore** = reload the local thread file and replay — independent of the
  server's history retention (NATS `nsed_hist_*` has a TTL).
- **Swap policy mid-thread** = the next turn simply carries a different policy.
  On the Completions path a changed `policy_id` deliberately forces a *fresh*
  deliberation, which the client stitches into the same thread. The Responses
  path forbids this — its thread is parameter-locked.

So the interactive client speaks **Chat Completions and keeps its own thread
store**; the Responses API remains the cleaner fit for external stateful clients
that want server-held threads.

## What a thread is, client-side

A thread is a small client-owned record (not the existing `sessions.json`,
which is unrelated Claude-CLI-UUID plumbing):

```text
Thread {
  id, subject, created, updated,
  active_policy,          // the "model" in force; recorded per message too
  orchestrator,
  server_thread,          // x-nsed-session-id, for cheap same-policy continuation
  messages: [ { role, content, policy_id, job_id, ts } ],
}
```

The transcript is the durable artefact; `server_thread` is an optimisation for
same-policy follow-ups, and falls back to replaying the transcript when the
server thread has expired.

## Tool calls work over this — for the record

A deliberation is **not** text-only. When a request carries `tools`, an agent
that calls one surfaces a standard `tool_calls` response
(`finish_reason: "tool_calls"` on Chat Completions; `requires_action` +
`function_call` on Responses), the client executes it, and the result is fed
back to the blocked agent — a real closed loop on both compat surfaces. So the
thread model does not preclude tool use; a client's tools ride down as
`user_tools`.

## Why our own TUI, not a wrapped coding agent

A tool-capable OpenAI-compatible client (e.g. OpenCode) can drive quorum today —
the compat surface was built for exactly that. Wrapping *Claude Code*
specifically has two frictions: it speaks the Anthropic Messages API
(`/v1/messages`), which the orchestrator does not yet route (a proxy or that
surface would be needed); and a deliberation's per-turn latency (rounds ×
agents) fits interactive single-model expectations poorly. More fundamentally,
the deliberation UX — rounds, competing proposals, convergence, the AI-email
framing, and the coming *channel* model — will diverge from any generic
coding-agent client over time. That divergence is why the interactive client is
built in-house rather than borrowed. The design principle it commits to is the
one at the top: **policy is the model, a thread is the conversation.**

### A note on the `[role]` flatten

The deliberation core consumes one task *string*, not a message array (agents
are N proposers/evaluators, provider-agnostic, each building their own prompt).
The compat layer therefore flattens `messages[]` into one string with `[role]`
prefixes so agents can still tell a user's question from a prior answer. This is
server-internal — clients speak vanilla Chat Completions. A structured
turns-with-deliberation-native-roles input is the clean future, and shares the
same design space as the channel work.

## See also

- [Understanding rooms and policies](rooms-and-policies.md) — the two server
  concepts and the public/private visibility rules.
- [Glossary](../reference/glossary.md) — room, policy, thread, job, effort.
- [Run an agent fleet](../how-to/run-an-agent-fleet.md) — `quorum.yml` in
  practice.
