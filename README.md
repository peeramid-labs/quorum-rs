# quorum-rs

### One LLM guesses. A quorum argues until it's sure.

[![crates.io](https://img.shields.io/crates/v/quorum-rs.svg)](https://crates.io/crates/quorum-rs)
[![docs.rs](https://img.shields.io/docsrs/quorum-rs)](https://docs.rs/quorum-rs)
[![license](https://img.shields.io/crates/l/quorum-rs.svg)](#license)

`quorum-rs` runs one question past many models at once. They answer in parallel,
grade each other's work, and the protocol stops the moment they agree — so you
get a checked answer instead of a single model's first guess. Best where a wrong
call is expensive: security review, technical decisions, content moderation.

It's the open-source SDK, runtime, and `quorum` CLI for building agents against
this protocol. The orchestrator that runs a round is a separate service; an
invite code joins you to one. Paper: [arXiv:2601.16863](https://arxiv.org/abs/2601.16863).

## Quickstart

Invite code to live agent, five commands:

```bash
cargo install quorum-rs
quorum redeem eyJhbGc...        # invite → local NATS creds + operator token
quorum init                    # scaffold quorum.yml (providers + agents)
quorum smoke-test cortex-a     # gut-check: chat → tool-calling → deliberation
quorum serve                   # live — joins the orchestrator
```

Full walkthrough:
**[your first agent, from invite to live deliberation](docs/tutorials/first-agent-from-invite.md)**.

## Install

```bash
cargo add quorum-rs         # the SDK
cargo add llm-repair        # just the JSON-repair helpers
cargo install quorum-rs     # the CLI binary (same crate)
```

As a Claude Code plugin (slash commands wrapping the CLI):

```
/plugin marketplace add peeramid-labs/plugin-marketplace
/plugin install quorum
```

See [use the Claude Code plugin](docs/how-to/use-the-claude-code-plugin.md).

## Documentation

Start in [`docs/`](docs/), organised by what you need:

- **[Tutorials](docs/tutorials/)** — learn by doing: [your first agent from an invite](docs/tutorials/first-agent-from-invite.md).
- **[How-to](docs/how-to/)** — task recipes: [smoke-test an agent](docs/how-to/smoke-test-an-agent.md), [run a fleet](docs/how-to/run-an-agent-fleet.md), [register a provider](docs/how-to/register-a-custom-provider.md).
- **[Reference](docs/reference/)** — dry facts: API, types, config schemas, wire protocols, [telemetry catalog](docs/reference/telemetry.md).
- **[Explanation](docs/explanation/)** — design rationale and tradeoffs.

Runnable examples: [`examples/`](examples/) and
[`crates/quorum-rs/examples/`](crates/quorum-rs/examples/).
Rustdoc: [quorum-rs](https://docs.rs/quorum-rs) · [llm-repair](https://docs.rs/llm-repair) · [quorum-crypto-core](https://docs.rs/quorum-crypto-core).

## Crates

| Crate | Description |
|---|---|
| [`quorum-rs`](crates/quorum-rs) | SDK + reference runtime + `quorum` CLI. Agent traits and data types, the reference proposer-evaluator agent (a ReAct loop over any OpenAI-compatible model), exec/MCP/Claude agent integrations, the NATS worker runtime, and the `quorum` binary. `cargo install` gives the binary, `cargo add` gives the SDK. |
| [`llm-repair`](crates/llm-repair) | JSON-repair, markdown extraction, and tool-call recovery for malformed LLM output. Standalone-useful for any Rust project calling real-world models. |
| [`quorum-crypto-core`](crates/quorum-crypto-core) | Ed25519 / secp256k1 / SHA3 primitives + audit envelope. Optional dep of `quorum-rs` (`audit` feature). |

## Status

Early public release. Breaking changes are expected while the API is polished;
semver-stable releases begin at `1.0`. Current version:
[crates.io](https://crates.io/crates/quorum-rs).

## Contributing

Issues, bug reports, and PRs welcome. PR titles follow
[conventional commits](https://www.conventionalcommits.org/) (CI-enforced).

## License

Dual-licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at
your option. Unless you state otherwise, any contribution you submit is dual
licensed as above, without additional terms.
