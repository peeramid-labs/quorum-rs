---
title: Provider-Executed Tools
order: 13
tagline: Tools the backend runs itself, where our job is to hand the arguments back rather than do the work.
---
# Provider-executed tools

Some backends offer tools they execute themselves — search being the common
one. The model asks for the tool, and the backend performs it; the caller never
implements anything. That is a different contract from the tools we register,
and mixing the two breaks the loop.

## Declaring one

```yaml
- name: "Example"
  provider_executed_tools: ["$some_provider_tool"]
```

Names are whatever that backend expects, verbatim. The list is empty for every
agent whose backend offers none, which is most of them.

The name also decides the shape sent on the wire, because backends disagree
about it:

| Name | Sent as | Used by |
| --- | --- | --- |
| `$web_search` | `{"type": "builtin_function", "function": {"name": "$web_search"}}` | a backend that wraps its own tools in a function envelope |
| `web_search` | `{"type": "web_search"}` | a router that takes the tool as a type on its own |

Sending the wrong shape is rejected before the model sees it, so a name that
does not match its backend fails the whole call rather than degrading.

## What happens on a call

1. The tool is declared alongside ours in `tools`, but with
   `"type": "builtin_function"` rather than `"function"` — that is what tells
   the backend it owns execution.
2. The model answers with `finish_reason: tool_calls` naming it.
3. We reply with a `role: "tool"` message whose content is the call's
   `arguments`, **unchanged**. Not a result we computed — the arguments
   themselves.
4. The backend runs the tool and continues the completion.

The ReAct loop recognises these names and echoes rather than dispatching. A
provider tool that reached local dispatch would fail as an unknown tool, since
we have no implementation for it and are not supposed to.

## What it costs

Two charges the per-token estimate does not model:

- **A fee per call**, on backends that charge one.
- **The result is billed as prompt tokens on the FOLLOWING request.** A single
  search observed in testing returned 8,245 tokens of results, which landed in
  the next call's `prompt_tokens`. The count is reported inside the tool call's
  own `arguments`, under `usage`, so it can be read before deciding whether to
  continue.

In a deliberation this multiplies: each agent that searches adds its results to
its own next prompt, every round.

## Availability is per endpoint, not per model

The same model reached through a different route generally cannot run these —
the tool belongs to the backend, not the weights. An agent that needs one must
point at the endpoint that offers it, which may constrain where that agent's
traffic goes.
