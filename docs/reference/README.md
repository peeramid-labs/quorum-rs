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
| [Thread TUI](thread-tui.md) | `quorum` thread-TUI keymap (per screen), reader/inbox line formats, and the persisted Message/Thread data model. |
| [Glossary](glossary.md) | Key terms used across the documentation. |

Per-crate rustdoc on docs.rs: [`quorum-rs`](https://docs.rs/quorum-rs), [`llm-repair`](https://docs.rs/llm-repair), [`quorum-crypto-core`](https://docs.rs/quorum-crypto-core).
