# quorum-rs

A Rust workspace for building **multi-agent deliberation systems** — cohorts of LLM-backed agents that propose, evaluate, and reach quorum on outcomes.

## Crates

| Crate | Version | Description |
|---|---|---|
| [`quorum-rs`](crates/quorum-rs) | `0.6.0` | The SDK: agent traits (`NsedAgent`, `Tool`, `AiModel`, `PromptSet`), data types (`AgentContext`, `Proposal`, `Evaluation`), reference agent implementations (`ExecAgent`, `McpAgent`, `ClaudeAgent`), NATS-based worker runtime, telemetry catalog, middleware framework. |
| [`llm-repair`](crates/llm-repair) | `0.6.0` | JSON-repair, markdown-extraction, and tool-call recovery for malformed LLM output. Useful for any Rust project that calls real-world LLMs and has to handle whatever they actually return. |
| [`quorum-crypto-core`](crates/quorum-crypto-core) | `0.6.0` | Ed25519 / secp256k1 / SHA3 cryptographic primitives + audit envelope. Optional dep of `quorum-rs` (gated behind the `audit` feature). |

## Install

```toml
[dependencies]
quorum-rs = "0.6"
```

For the JSON-repair helpers in isolation:

```toml
[dependencies]
llm-repair = "0.6"
```

## What's "deliberation"?

A protocol where multiple LLM-backed agents — each potentially a different model, prompt, or toolchain — independently propose answers to a task, then evaluate each other's proposals, then converge on a quorum-selected outcome. Useful for tasks where single-model failure modes are expensive (security review, technical decisions, content moderation).

`quorum-rs` is the open-source SDK for building agents against this protocol. The protocol itself is implemented by a separate orchestrator service.

## Quick example: `llm-repair`

```rust
use llm_repair::{extract_python_tool_calls, repair_truncated_json, clean_json_string};

// LLM returned a malformed JSON tool-call payload? Recover it.
let raw = r#"{"name":"search","arguments":{"q":"hello"#;  // truncated
let repaired = repair_truncated_json(raw);

// Model emitted a Python-style tool call instead of JSON? Extract it.
let py = r#"search(q="hello", limit=10)"#;
let calls = extract_python_tool_calls(py, &["search"]);

// Got JSON wrapped in markdown / backticks / commentary? Clean it.
let dirty = "```json\n{\"answer\": 42}\n```";
let clean = clean_json_string(dirty, false, None);
```

Built for the failure modes of small + open-weight models: truncation, invalid escapes (LaTeX), conversational wrappers, Python-syntax tool calls, markdown wrapping. Battle-tested in production deliberation runs.

See [`crates/llm-repair/README.md`](crates/llm-repair/README.md) for the full API.

## Documentation

See [`docs/`](docs/) — organized by the [Diátaxis](https://diataxis.fr) framework:

- [Tutorials](docs/tutorials/) — learning-by-doing lessons
- [How-to guides](docs/how-to/) — task-oriented recipes
- [Reference](docs/reference/) — API surface, types, schemas
- [Explanation](docs/explanation/) — design rationale, tradeoffs

Per-crate rustdoc: <https://docs.rs/quorum-rs>, <https://docs.rs/llm-repair>, <https://docs.rs/quorum-crypto-core>.

## Status

`quorum-rs` is a fresh extraction from a proprietary monorepo. The published `0.6` series mirrors the parent project's release line at the time of extraction. Breaking changes are expected as the public API surface is polished — semver-respecting versions begin at `1.0` once the API stabilises.

## Contributing

Contributions welcome — issues, bug reports, and PRs are all good. PR titles follow [conventional commits](https://www.conventionalcommits.org/) (enforced by CI).

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
