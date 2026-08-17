---
title: "Thread a conversation"
tagline: Send a second turn the council can actually answer.
---

# Thread a multi-turn conversation

In this tutorial we will hold a three-turn conversation with a council, where
each turn depends on the one before. Along the way we will look at exactly what
the council receives, and see what happens when a client assembles the thread
itself instead of letting the server do it.

You need a running orchestrator and an operator token. We will use `curl`, so
nothing has to be installed.

## 1. Ask the first question

A turn is a normal Chat Completions request. The `model` names a **policy**, not
an LLM:

```bash
curl -sD headers.txt https://api.example.xyz/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "nsed:fast",
    "messages": [
      {"role": "user", "content": "What is the sound of one hand clapping?"}
    ]
  }'
```

Two things come back. The answer, and — in the response headers — the id of the
thread it belongs to:

```bash
grep -i x-nsed-session-id headers.txt
```

```text
x-nsed-session-id: room-7145388f
```

Keep that value. It is how the next turn joins this conversation rather than
starting a new one.

## 2. Ask a follow-up that cannot stand alone

Now the part that matters. Send the **whole conversation** as messages — the
first question, the answer you received, and the new question:

```bash
curl -s https://api.example.xyz/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'x-nsed-session-id: room-7145388f' \
  -d '{
    "model": "nsed:fast",
    "messages": [
      {"role": "user",      "content": "What is the sound of one hand clapping?"},
      {"role": "assistant", "content": "A Zen koan attributed to Hakuin Ekaku…"},
      {"role": "user",      "content": "yo what about forests?"}
    ]
  }'
```

Notice what we did **not** do: we did not write a summary, a preamble, or a
"the conversation so far" header. We handed over the turns and let the server
render them.

The council answers the forest question, and knows the koan is the context —
"yo what about forests?" is meaningless without it.

## 3. Look at what the council actually received

This is the step worth doing at least once. The task the agents deliberated on
is stored in the room's manifest. If you have NATS credentials:

```bash
nats --creds ~/.nsed/agent.creds -s "$NATS_URL" \
  kv get nsed_hist_room-7145388f manifest --raw \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["user_query"])'
```

You should see the turns rendered with role labels, blank-line separated:

```text
[user] What is the sound of one hand clapping?

[assistant] A Zen koan attributed to Hakuin Ekaku…

[user] yo what about forests?
```

That is the **flatten**. The deliberation core takes one task *string* — the
agents are N proposers and evaluators, each building its own prompt — so the
server renders your `messages[]` into this shape, labelling each turn so an agent
can tell your question from its own earlier answer.

Notice also what the first turn looked like: a lone `user` message is passed
through **bare**, with no `[user]` prefix. That is deliberate, so single-turn
callers are unaffected.

## 4. See what goes wrong if you flatten it yourself

Now break it on purpose. Send the same three turns, but pre-flattened by hand
into one message — the way a client does if it builds its own transcript:

```bash
curl -s https://api.example.xyz/v1/chat/completions \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "nsed:fast",
    "messages": [
      {"role": "user", "content": "Context — the conversation so far:\n\n1. You asked: What is the sound of one hand clapping?\n   Council answered: A Zen koan attributed to Hakuin Ekaku… (600 more words)\n\n---\n\nFollow-up:\nyo what about forests?"}
    ]
  }'
```

Read that back with the command from step 3 and you will see the difference: no
`[role]` labels anywhere. Because it is a **single** message, the bare-passthrough
rule from step 3 applies, so the server hands the prose to the agents exactly as
written.

Two consequences follow, and both are visible in the answers:

- **The question is buried.** In one real thread the payload was 4 698
  characters and the live question was 140 of them — **3 %**, sitting last, after
  the longest text in the request.
- **Prior answers outrank it.** Each pasted answer is far longer than any
  question, so the council's own earlier output becomes the bulk of its next
  instruction. In that thread the agents closed an answer about medicine, law and
  warfare with the koan from three turns earlier, and one asked the user whether
  to prioritise a Zen metaphor while evaluating it.

Neither is misbehaviour. Each agent weighed the text it was told to solve, and it
was told the whole transcript was the task.

## 5. Keep the thread going

Every later turn is step 2 again: append the newest exchange to `messages[]`,
send the same `x-nsed-session-id`, and let the server flatten. The client owns
the transcript — that is what lets you reload a conversation from your own store,
and swap policy mid-thread by sending a different `model` on the next turn.

## What you did

You held a threaded conversation by sending **turns, not prose**, and you looked
at the flattened task to confirm what the council received. The rule is short:

> Send `messages[]`. Never pre-flatten. One turn per array entry, with its role.

The renderer lives in `quorum_rs::conversation::flatten_conversation`, and the
compat layer calls it — so a thread assembled by the TUI and one assembled by the
server produce the same string, and a resumed conversation reads the same either
way. That is also why a client should not reimplement it: a second copy is a
second set of rules to drift.

## Next

- [Policy & sessions](../explanation/policy-as-model-and-sessions.md) — why the
  client owns the transcript, and why policy is the model.
- [Chat Completions reference](../reference/chat-completions.md) — the full
  request and response surface.
