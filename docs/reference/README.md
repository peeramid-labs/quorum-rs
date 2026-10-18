# Reference

Authoritative descriptions — API surface, types, schemas. Mirrors the code structure.

| Document | What it pins down |
|---|---|
| [Telemetry event catalog](telemetry.md) | Every published event type, its fields, retention class, and what is *not* sent. |
| [Sandboxed builtin tools](sandboxed-tools.md) | `read_file`, `grep_search` — argument schema, return shape, shared access policy. |
| [Chat completions API](chat-completions.md) | OpenAI-compatible `/v1/chat/completions` request/response shapes. |
| [Exec agent protocol](exec-agent-protocol.md) | Wire protocol for stdin/stdout subprocess agents. |
| [MCP agent protocol](mcp-agent-protocol.md) | Wire protocol for tool-calling-aware external agents. |
| [Dashboard configuration](dashboard-config.md) | `--dashboard-port` / `--dashboard-bind` flags, `dashboard_port` yaml field, `QUORUM_DASHBOARD_BIND` env var, `status-server` feature gate. |
| [`persona` yaml shapes](persona-yaml-shapes.md) | Grammar for the inline-string and stacked-layer forms `persona:` accepts; path semantics and error modes. |
| [`openrouter` agent-config block](openrouter-provider-config.md) | OpenRouter-only request extensions: provider routing (`provider_sort`/`ignore`/`only`), ZDR, reasoning-token handling, and the opt-in `web_search` plugin. |
| [Thread TUI](thread-tui.md) | `quorum` thread-TUI keymap (per screen), reader/inbox line formats, and the persisted Message/Thread data model. |
| [Task clock](task-clock.md) | `AgentContext.issued_at` and the `<clock>` block it renders: wire form, wording, and why the caller stamps the date. |
| [Model health](model-health.md) | How an agent judges its own model — catalog listing plus a serving probe — and the fail-open rule that decides when it benches itself. |
| [Agent identity keys](agent-identity.md) | The keys an agent declares, the signatures an operator attaches to a held response, what is carried but not yet checked, and the fleet bearer token. |
| [Content fetch protocol](content-fetch.md) | Reading the bytes behind a content address from a responder over NATS: request, reply headers, checks. |
| [NATS key codec](nats-key-codec.md) | Encoding any string as a NATS KV key or subject token, and back. |
| [Feed](feed.md) | Channels and JMAP-`Email`-shaped records: addressing, record fields, drafts, message ids, blog entries and slugs, and the `FeedStore` contract (`noolog-feed`). |
| [Files pipeline](files-pipeline.md) | Uploads staged in NATS, admitted by owner subject, stepped and committed; signed job requests (`quorum_rs::pipeline`). |
| [JMAP](jmap.md) | JMAP (RFC 8620) and JMAP Mail (RFC 8621) data types written from the RFCs, their coverage and conformance checks (`jmap-core`, `jmap-mail`). |
| [Proposal structure metadata](proposal-structure.md) | The measured shape evaluators see on each candidate, and the tools that address it by line. |
| [Provider-executed tools](provider-tools.md) | Tools the backend runs itself; the SDK hands the arguments back rather than doing the work. |
| [Sampling parameters](sampling-params.md) | Which request fields are sent, when they are omitted, and how to talk to a backend that fixes them server-side. |
| [Task SKIP protocol](skip.md) | How an agent sits a task out as an explicit answer instead of silence. |
| [Glossary](glossary.md) | Key terms used across the documentation. |

Per-crate rustdoc on docs.rs: [`quorum-rs`](https://docs.rs/quorum-rs), [`llm-repair`](https://docs.rs/llm-repair), [`quorum-crypto-core`](https://docs.rs/quorum-crypto-core).
