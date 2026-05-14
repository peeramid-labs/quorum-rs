# quorum-rs

**Get your distributed AI agent team on the same page.**

`quorum` is a parallel synchronisation engine for AI agents. Multiple agents
contribute their own perspectives in parallel, then the protocol drives the
team toward consensus on a single task. Pooling many models against a shared
goal substantially raises reasoning power over any single agent — and
token-efficient early-stop convergence detection ends the round the moment
agreement is reached, instead of burning the full iteration budget.

Deep tech dive: [arXiv:2601.16863](https://arxiv.org/abs/2601.16863).

## Crates

| Crate | Version | Description |
|---|---|---|
| [`quorum-rs`](crates/quorum-rs) | `0.6.0` | The SDK + reference runtime: agent traits (`NsedAgent`, `Tool`, `AiModel`, `PromptSet`), data types (`AgentContext`, `Proposal`, `Evaluation`), the reference `ProposerEvaluatorAgent` (ReAct loop over any OpenAI-compatible LLM), additional `ExecAgent` / `McpAgent` / `ClaudeAgent` integrations, NATS-based worker runtime, telemetry catalog, and middleware framework. |
| [`quorum-cli`](crates/quorum-cli) | `0.6.0` | User-facing CLI that loads an `agent.yml`, spawns workers, and exposes a local status dashboard. Distribution surface for the reference agent runtime. |
| [`llm-repair`](crates/llm-repair) | `0.6.0` | JSON-repair, markdown-extraction, and tool-call recovery for malformed LLM output. Useful for any Rust project that calls real-world LLMs and has to handle whatever they actually return. |
| [`quorum-crypto-core`](crates/quorum-crypto-core) | `0.6.0` | Ed25519 / secp256k1 / SHA3 cryptographic primitives + audit envelope. Optional dep of `quorum-rs` (gated behind the `audit` feature). |

## Install

As a library:

```toml
[dependencies]
quorum-rs = "0.6"
```

For the JSON-repair helpers in isolation:

```toml
[dependencies]
llm-repair = "0.6"
```

As a CLI binary:

```bash
cargo install quorum-cli
```

## How it works

Each round, every agent — potentially a different model, prompt, or
toolchain — independently proposes an answer, then evaluates the peer
proposals, and the protocol selects a quorum-backed outcome. Convergence
detection short-circuits the round as soon as agreement is high enough to
make further iteration unprofitable. The result: stronger answers than any
single agent, without paying for tokens you don't need.

Useful wherever single-model failure modes are expensive — security
review, technical decisions, content moderation, anywhere a wrong call is
costly.

`quorum-rs` is the open-source SDK + reference runtime for building agents
against this protocol. The orchestrator that drives a deliberation round
runs as a separate service.

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

- [Tutorials](docs/tutorials/) — learning-by-doing lessons _(coming as the SDK content stabilises)_
- [How-to guides](docs/how-to/) — task-oriented recipes (agent development, telemetry inspection, opt-out)
- [Reference](docs/reference/) — API surface, types, schemas, wire protocols
- [Explanation](docs/explanation/) — design rationale, tradeoffs

Runnable examples in [`examples/`](examples/) (Python exec + MCP agents) and
[`crates/quorum-rs/examples/`](crates/quorum-rs/examples/) (`cargo run --example` programs).

Per-crate rustdoc: <https://docs.rs/quorum-rs>, <https://docs.rs/quorum-cli>, <https://docs.rs/llm-repair>, <https://docs.rs/quorum-crypto-core>.

## Status

The published `0.6` series is the initial public release. Breaking changes are expected as the public API surface is polished — semver-respecting versions begin at `1.0` once the API stabilises.

## Contributing

Contributions welcome — issues, bug reports, and PRs are all good. PR titles follow [conventional commits](https://www.conventionalcommits.org/) (enforced by CI).

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
