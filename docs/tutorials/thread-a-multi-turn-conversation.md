---
title: "Thread a conversation"
tagline: Send a second turn the council can actually answer.
---

# Thread a multi-turn conversation

In this tutorial we will hold a three-turn conversation with a council, where
each turn depends on the one before. Along the way we will look at exactly what
the council receives, and see the two ways a thread loses its own question.

There are two endpoints, and which one you need is decided by a single question:
**do you choose the council per request?**

| | `POST /v1/chat/completions` | `POST /deliberation` |
| --- | --- | --- |
| council / rounds per request | no — fixed by the policy | **yes** (`agent_names`, `deliberation_rounds`) |
| takes | `messages[]`, server flattens | `user_query` — one string you build |
| threading | `x-nsed-session-id` header | `conversation_id` + `new_turn` |

If you are building a chat that lets a user pick who deliberates, you need
`/deliberation`, and this tutorial takes that path. A client that is happy with a
fixed policy should prefer the compat endpoint, because the server does the
flattening for it.

You need a running orchestrator and an operator token. We will use `curl`, so
nothing has to be installed.

## 1. Ask the first question

Pick a thread id now and keep it for every turn. It is the `conversation_id`;
each turn still gets its own `room_id`.

```bash
THREAD="thread-9f2c"

curl -s https://api.example.xyz/deliberation \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{
    \"room_id\": \"room-$(openssl rand -hex 4)\",
    \"conversation_id\": \"$THREAD\",
    \"user_query\": \"What is the sound of one hand clapping?\",
    \"agent_names\": [\"Corepunk01\", \"Corepunk02\", \"Corepunk03\"],
    \"deliberation_rounds\": 2
  }"
```

The first turn needs no `new_turn` — the query *is* the new turn.

## 2. Ask a follow-up that cannot stand alone

Here is the shape to copy. Send **both**: the whole thread as `user_query`, and
this turn alone as `new_turn`.

```bash
curl -s https://api.example.xyz/deliberation \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{
    \"room_id\": \"room-$(openssl rand -hex 4)\",
    \"conversation_id\": \"$THREAD\",
    \"user_query\": \"[user] What is the sound of one hand clapping?\n\n[assistant] A Zen koan attributed to Hakuin Ekaku…\n\n[user] yo what about forests?\",
    \"new_turn\": \"yo what about forests?\",
    \"agent_names\": [\"Corepunk01\", \"Corepunk02\", \"Corepunk03\"],
    \"deliberation_rounds\": 2
  }"
```

Two rules in that request, and the next two steps show why each is needed.

**`user_query` carries the whole thread**, rendered as `[role] content` blocks
separated by blank lines — the same shape the compat endpoint produces from a
`messages[]` array, and the same one
`quorum_rs::conversation::flatten_conversation` produces for the TUI. Match it
exactly: a lone `user` message is sent **bare**, with no prefix, and every other
case is labelled.

**`new_turn` carries only this send's message.** Not the thread, not a summary.

## 3. Look at what the council actually received

Do this once and the rest of the tutorial explains itself. The task the agents
deliberated on is in the room's manifest:

```bash
nats --creds ~/.nsed/agent.creds -s "$NATS_URL" \
  kv get nsed_hist_room-<the id you sent> manifest --raw \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["user_query"])'
```

```text
[user] What is the sound of one hand clapping?

[assistant] A Zen koan attributed to Hakuin Ekaku…

[user] yo what about forests?
```

That string is what a **stateless** agent sees. The deliberation core takes one
task *string* — the agents are N proposers and evaluators, each building its own
prompt — and for an agent talking to a plain LLM, every call rebuilds that prompt
from scratch. There is no session holding the earlier turns. This string is the
only place they exist.

## 4. Why both fields, and not just `new_turn`

`new_turn` reaches an agent through `AgentContext::delta_task()`, which returns
`new_turn` when it is set and the full task otherwise. Each agent takes what it
can use:

- A **session-capable** agent — the Claude/MCP path — resumes the provider-side
  session that `conversation_id` identifies. Earlier turns are already in that
  session, so it wants the delta and nothing more. Re-sending the thread would
  duplicate what the session already holds.
- A **stateless** agent — anything on a plain LLM, which is most councils —
  reads the full task. It has no session. Give it only `new_turn` and it receives
  one line with no context; `yo what about forests?` becomes unanswerable.

Send both and each agent type is served correctly. That is the whole design: the
same request satisfies a council mixing both kinds, which is normal.

## 5. See what goes wrong if you write prose instead

Now break it deliberately. Put the thread in `user_query` as a narrative rather
than as labelled turns:

```json
{
  "user_query": "Context — the conversation so far:\n\n1. You asked: What is the sound of one hand clapping?\n   Council answered: A Zen koan… (600 more words)\n\n---\n\nFollow-up:\nyo what about forests?"
}
```

Read it back with step 3's command. It is stored exactly as written, and two
things follow — both observed on a real thread:

- **The question is buried.** That payload was 4 698 characters and the live
  question 140 of them — **3 %** — sitting last, after the longest text in the
  request.
- **Prior answers outrank it.** Each pasted answer dwarfs every question, so the
  council's own earlier output becomes the bulk of its next instruction. The
  agents closed an answer about medicine, law and warfare with the koan from
  three turns earlier, and one asked the user whether to prioritise a Zen
  metaphor while evaluating it.

Neither is misbehaviour. Each agent weighed the text it was told to solve, and it
was told the whole narrative was the task. Labelled turns and a `new_turn` give
it somewhere to look instead.

## 6. Keep the thread going

Every later turn repeats step 2: append the newest exchange to the `user_query`
you render, set `new_turn` to just the new message, keep the same
`conversation_id`, and use a fresh `room_id`.

## What you did

You threaded a conversation while still choosing the council per turn. Two rules:

> Render `user_query` as `[role]` turns — never as prose. Set `new_turn` to this
> message alone, and keep `conversation_id` stable across the thread.

## Next

- [Policy & sessions](../explanation/policy-as-model-and-sessions.md) — why the
  client owns the transcript, and why policy is the model.
- [Chat Completions reference](../reference/chat-completions.md) — the other
  endpoint, for clients that accept a fixed policy and want the server to
  flatten for them.
