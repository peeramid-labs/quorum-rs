# Rust examples

Runnable `cargo run --example` programs against the `quorum-rs` crate.

| Example | What it demonstrates |
|---|---|
| [`telemetry_demo.rs`](telemetry_demo.rs) | Hands-on inspection of the agent-only telemetry foundation: trace-id determinism, subject derivation rules, the structural redaction invariant, and `TelemetryConfig` YAML parsing (including `enabled: false`). |

## Running

From the workspace root:

```bash
cargo run -p quorum-rs --example telemetry_demo
```

Output is printed to stdout in four labelled sections; each section
asserts the property it exercises and panics on regression — so a green
run is the contract that the demonstrated invariants still hold.

## Reading order

- Per-event field-level reference: [`docs/reference/telemetry.md`](../../../docs/reference/telemetry.md)
- Design rationale and what is *not* emitted:
  [`docs/explanation/telemetry-design.md`](../../../docs/explanation/telemetry-design.md)

## Adding a new example

Each example lives in its own `*.rs` file in this directory and is
auto-discovered by Cargo. Drop a `//!`-doc-comment header explaining what
the example demonstrates and how to read its output; that becomes the
`cargo doc` entry. Add a row to the table above so the README stays the
canonical index.
