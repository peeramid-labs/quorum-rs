# quorum-rs

A Rust workspace of crates for building **multi-agent deliberation systems**: tools for running cohorts of LLM-backed agents that propose, evaluate, and reach quorum on outcomes.

## Crates

| Crate | Status | Description |
|---|---|---|
| [`llm-repair`](crates/llm-repair) | `0.1.0` — usable today | JSON-repair, markdown-extraction, and tool-call recovery for malformed LLM output. Useful for any Rust project that calls real-world LLMs and has to deal with whatever they actually return. |
| [`quorum-rs`](crates/quorum-rs) | placeholder — in development | Top-level SDK: agent trait + reference implementation, LLM dispatch, tool framework, prompt sets, worker runtime. Lands incrementally. |

More crates land here as the SDK grows — a crypto-core for agent signing/verification, a CLI for running agents against an orchestrator, and middleware framework. See [Roadmap](#roadmap).

## Use `llm-repair` today

```toml
[dependencies]
llm-repair = "0.1"
```

```rust
use llm_repair::{extract_python_tool_calls, repair_truncated_json, clean_json_string};

// LLM returned a malformed JSON tool-call payload? Recover it.
let raw = r#"{"name":"search","arguments":{"q":"hello"#;  // truncated
let repaired = repair_truncated_json(raw);
// → {"name":"search","arguments":{"q":"hello"}}

// Model emitted a Python-style tool call instead of JSON? Extract it.
let py = r#"search(q="hello", limit=10)"#;
let calls = extract_python_tool_calls(py, &["search"]);

// Got JSON wrapped in markdown / backticks / commentary? Clean it.
let dirty = "```json\n{\"answer\": 42}\n```";
let clean = clean_json_string(dirty, false, None);
```

Built for the failure modes of small + open-weight models: truncation, invalid escapes (LaTeX), conversational wrappers, Python-syntax tool calls, markdown wrapping. Battle-tested in production deliberation runs.

See [`crates/llm-repair/src/lib.rs`](crates/llm-repair/src/lib.rs) for the full API.

## What's "deliberation"?

A protocol where multiple LLM-backed agents — each potentially a different model, prompt, or toolchain — independently propose answers to a task, then evaluate each other's proposals, then converge on a quorum-selected outcome. Useful for tasks where single-model failure modes are expensive (security review, technical decisions, content moderation).

`quorum-rs` is the open-source SDK for building agents against this protocol. The protocol itself is implemented by a separate orchestrator service.

## Roadmap

The SDK rolls out incrementally from the [`nsed` proprietary monorepo](https://github.com/peeramid-labs/nsed). Order of arrival:

1. **`llm-repair`** ✅ shipped
2. **`quorum-rs`** SDK content — agent traits, LLM dispatch, tool framework, prompt sets, worker runtime
3. **`quorum-crypto-core`** — Ed25519 / secp256k1 / SHA3 primitives for agent signing
4. **`quorum-cli`** — CLI for running agents against an orchestrator
5. **Documentation** — tutorials, how-tos, reference, explanation (see [`docs/`](docs/))
6. **Publication** — all crates to crates.io with stable APIs

Breaking changes are expected on `quorum-rs` itself during this rollout. `llm-repair` follows semver from `0.1.0`.

## Documentation

See [`docs/`](docs/) — organized by the [Diátaxis](https://diataxis.fr) framework (tutorials, how-to, reference, explanation). Content lands alongside the SDK crates it documents.

## Contributing

Contributions welcome — issues, bug reports, and PRs are all good. PR titles follow [conventional commits](https://www.conventionalcommits.org/) (enforced by CI).

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
