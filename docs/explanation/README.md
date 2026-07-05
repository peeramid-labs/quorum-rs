# Explanation

Discussion-mode material — why, design rationale, tradeoffs.

Read these away from the keyboard when you want to understand a design decision rather than just use the resulting code.

| Article | Topic |
|---|---|
| [Understanding rooms and policies](rooms-and-policies.md) | Why a room (where a job lives + who can watch) and a policy (who deliberates + how) are separate, and the public/private visibility rules that bite. |
| [Policy-as-model and the thread](policy-as-model-and-threads.md) | Why the interactive client treats a policy as a model and a thread as the conversation (AI-email framing, not chat) — client-owned transcript, mid-thread policy swap, own TUI. |
| [Agent internals](agent-internals.md) | Library API surface + ReAct loop architecture inside `quorum-rs`. |
| [About the provider registry](provider-registry.md) | Why `provider.type` dispatch is a registry of factories, and how a third-party crate adds a provider without forking the SDK. |
| [Agent ranking](agent-ranking.md) | Capability declaration + how agents earn rank through deliberation outcomes. |
| [NATS topology](nats-topology.md) | Subjects, JetStream streams, JWT scopes, dispatch model. |
| [Middleware system](middleware.md) | Pluggable validation/moderation pipeline and the design rationale behind it. |
| [About telemetry design](telemetry-design.md) | Principles, trace correlation, retention, and what is intentionally *not* emitted. |
| [About scoped `read_file`](scoped-read-file.md) | Sandbox semantics for non-Claude agents reading from configured roots. |
| [About LLM context-window guards](llm-context-window-guards.md) | Why the shrink-guard counts tool schemas and why `max_tokens` isn't capped defensively. |
| [About `compact_history` & scratchpad squeeze](compact-history-and-scratchpad-squeeze.md) | Why agents self-fold older tool results, and the shape of the structured summarisation prompt. |
| [Why persona layer stacking, and why CWD-relative paths](persona-layer-stacking.md) | Why `persona:` accepts a layered form, why files are read at parse time, and why paths resolve against process CWD instead of the yaml file's parent. |
