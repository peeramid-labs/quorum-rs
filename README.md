# quorum-rs

> **In active development.** Crates land here as the [NSED SDK split](https://github.com/peeramid-labs/nsed/issues/357) progresses through Phases 2-5.

A Rust SDK for building multi-agent deliberation systems against the NSED protocol.

## What is this?

`quorum-rs` is the dual-licensed (`Apache-2.0 OR MIT`) open-source SDK for the [NSED](https://github.com/peeramid-labs/nsed) multi-agent deliberation protocol. Anyone can `cargo add quorum-rs` and run a working NSED agent without touching proprietary code.

The headline crate (`quorum-rs`) is accompanied by sister crates:

- [`llm-repair`](crates/llm-repair/) — JSON-repair, markdown-extraction, and tool-call recovery helpers for malformed LLM output. Generally useful for any Rust dev working with LLM-generated text, independent of the NSED protocol.
- `quorum-crypto-core` — signing / verification primitives.
- `quorum-cli-common` — shared CLI transport / request types.
- `quorum-cli` — agent-side CLI subcommands.

## Status

This repository is being populated from the parent [`peeramid-labs/nsed`](https://github.com/peeramid-labs/nsed) monorepo. During Phases 2-4 of the split, code lives here and is consumed from the parent repo via a git submodule and `path =` dependencies. Phase 5 cuts the apron strings: crates publish to crates.io and the submodule is removed.

See [#357 in `peeramid-labs/nsed`](https://github.com/peeramid-labs/nsed/issues/357) for the migration plan.

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
