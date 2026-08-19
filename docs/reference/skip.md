# Task SKIP protocol

An agent may answer any deliberation task with an explicit SKIP instead of
working it. SKIP is a protocol answer, never silence: the orchestrator can
distinguish a deliberate abstention from a timeout or a failure.

## Deciding

`NsedAgent::task_disposition(&self, context, action) -> TaskDisposition` is
the strategy hook. The provided default returns `Work`, so every agent that
does not override it is unaffected. Implementations own the strategy; the
SDK carries no configuration for it.

## Wire form

| action | carrier |
|---|---|
| `propose` | a `Proposal` with `skipped: true` and empty content, published on the normal result subject |
| `evaluate` (and any other action) | an empty payload on `{prefix}.{session}.result.{round}.{agent}.{action}.skipped` |

The evaluate skip is its own subject because an empty evaluation batch on
the normal subject is indistinguishable from a model that produced nothing.
The subject mirrors the `.failed` hierarchy: a consumer with
`filter_subjects = [verdict, verdict.failed, verdict.skipped]` separates
work, failure and abstention without payload inspection.

## Orchestrator semantics

- The skipping agent counts as **responded** — the phase never waits on it.
- A skipped proposal produces **no record**: nothing exists to evaluate,
  vote on, or win.
- No failure is counted and no timeout is reported; the agent remains a
  fully active participant in the next phase and round.

## No LLM call

A skipped task must be answered before any model invocation — a round the
agent sits out buys no tokens.
