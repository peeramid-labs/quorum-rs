# Reference

Authoritative descriptions — API surface, types, schemas. Mirrors the code structure.

| Document | What it pins down |
|---|---|
| [Telemetry event catalog](telemetry.md) | Every published event type, its fields, retention class, and what is *not* sent. |
| [Sandboxed builtin tools](sandboxed-tools.md) | `read_file`, `grep_search` — argument schema, return shape, shared access policy. |
| [Chat completions API](chat-completions.md) | OpenAI-compatible `/v1/chat/completions` request/response shapes. |
| [Exec agent protocol](exec-agent-protocol.md) | Wire protocol for stdin/stdout subprocess agents. |
| [MCP agent protocol](mcp-agent-protocol.md) | Wire protocol for tool-calling-aware external agents. |
| [Glossary](glossary.md) | Key terms used across the documentation. |

Per-crate rustdoc on docs.rs: [`quorum-rs`](https://docs.rs/quorum-rs), [`quorum-cli`](https://docs.rs/quorum-cli), [`llm-repair`](https://docs.rs/llm-repair), [`quorum-crypto-core`](https://docs.rs/quorum-crypto-core).
