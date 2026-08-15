# Explanation

Discussion-mode material — why, design rationale, tradeoffs.

Read these away from the keyboard when you want to understand a design decision rather than use the resulting code.

| Article | Topic |
|---|---|
| [Understanding rooms and policies](rooms-and-policies.md) | Why a room (where a job lives + who can watch) and a policy (who deliberates + how) are separate, and the public/private visibility rules that bite. |
| [Policy-as-model and the chat session](policy-as-model-and-sessions.md) | Why the interactive client treats a policy as a model and a session as a thread — client-owned transcript, mid-chat policy swap, and why we build our own TUI. |
| [Agent internals](agent-internals.md) | Library API surface + ReAct loop architecture inside `quorum-rs`. |
| [About the provider registry](provider-registry.md) | Why `provider.type` dispatch is a registry of factories, and how a third-party crate adds a provider without forking the SDK. |
| [Agent ranking](agent-ranking.md) | Capability declaration + how agents earn rank through deliberation outcomes. |
| [About model-down benching](model-down-benching.md) | How an agent whose remote model 404s benches itself from scheduling, why the bench escalates with consecutive strikes, and why a successful task resets it. |
| [NATS topology](nats-topology.md) | Subjects, JetStream streams, JWT scopes, dispatch model. |
| [Middleware system](middleware.md) | Pluggable validation/moderation pipeline and the design rationale behind it. |
| [Claim-citation grounding](claim-citation-grounding.md) | Why evaluator claims quote verbatim, how resolution tolerates model wrappers, what the cite matches against (the shown thought window), and why the MCP path rejects-and-retries while exec is non-destructive. |
| [Agent working-directory override](agent-working-directory.md) | Why `AgentContext.working_dir_override` exists, how a `before_prompt` middleware sets it via `agent_working_dir`, and what a custom agent must do with it. |
| [About telemetry design](telemetry-design.md) | Principles, trace correlation, retention, and what is intentionally *not* emitted. |
| [About scoped `read_file`](scoped-read-file.md) | Sandbox semantics for non-Claude agents reading from configured roots. |
| [About LLM context-window guards](llm-context-window-guards.md) | Why the shrink-guard counts tool schemas and why `max_tokens` isn't capped defensively. |
| [About `compact_history` & scratchpad squeeze](compact-history-and-scratchpad-squeeze.md) | Why agents self-fold older tool results, and the shape of the structured summarisation prompt. |
| [Why persona layer stacking, and why CWD-relative paths](persona-layer-stacking.md) | Why `persona:` accepts a layered form, why files are read at parse time, and why paths resolve against process CWD instead of the yaml file's parent. |
