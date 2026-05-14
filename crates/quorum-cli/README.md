# quorum-cli

[![Crates.io](https://img.shields.io/crates/v/quorum-cli.svg)](https://crates.io/crates/quorum-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0%20OR%20MIT-blue.svg)](#license)

Command-line client for running multi-agent deliberation jobs against a `quorum-rs` orchestrator.

Ships the `quorum` binary plus a library re-export of the underlying command modules so other CLIs (e.g. an operator binary that wraps additional subcommands) can embed the same agent-side functionality without re-implementing it.

## Install

```bash
cargo install quorum-cli
```

MSRV: Rust 1.85 (uses Edition 2024).

## Commands

```text
quorum run <task>          Submit a deliberation task to the orchestrator
quorum status              Health check + agent status
quorum trace <job_id>      Show a deliberation trace (verdict + evaluations)
quorum tui                 Interactive terminal UI (live deliberation view)
```

All commands read `./nsed.yaml` by default (`--config <path>` to override). The config declares which orchestrators to talk to, which room or policy to use, and any shared context.

### `run` — submit a task

```bash
# Pass the task on the command line
quorum run "Should we adopt Lattice signatures for protocol envelopes?"

# Or read from a file (resolves relative to the workspace config dir)
quorum run --task-file task.md

# Override the room or pin a specific policy
quorum run "..." --room security --policy quorum-3a

# Capture the verdict + prompt + dispatch.json into a directory
quorum run "..." --output-dir runs/2026-05-14/

# Live TUI view of the deliberation as it streams
quorum run "..." --tui
```

### `status` — agents online?

```bash
quorum status                    # default orchestrator from nsed.yaml
quorum status -o my-orchestrator # target a specific one
```

### `trace` — replay a finished deliberation

```bash
quorum trace 01HXZQ...           # short view
quorum trace 01HXZQ... --verbose # include full evaluation justifications
```

### `tui` — interactive terminal UI

```bash
quorum tui
```

Browse orchestrators, agents, policies, and running jobs without leaving the terminal. Drill into a job for round-by-round proposals and evaluations.

## Workspace config (`nsed.yaml`)

```yaml
default_room: scratch

orchestrators:
  local:
    base_url: http://localhost:8080
    nats_url: nats://localhost:4222

rooms:
  scratch:
    policy: default
    orchestrator: local

policies:
  default:
    agents: [cortex-a, cortex-b, cortex-c]
    rounds: 3
```

See [`crates/quorum-cli/src/workspace`](src/workspace) for the full schema.

## Library use

```rust
use quorum_cli::commands;
use std::path::Path;

#[tokio::main]
async fn main() {
    let exit = commands::run::run(
        Path::new("nsed.yaml"),
        "Should we adopt Lattice signatures?",
        Some("security"),  // room
        None,              // policy
        &[],               // context files
        None,              // output_dir
        None,              // output_file
        false,             // force_output
    )
    .await;
    std::process::exit(u8::from(exit) as i32);
}
```

This is how the BSL `nsed-cli` (which adds operator subcommands like `serve` and `validate`) embeds the agent-side functionality.

## Non-goals

- Not a deliberation engine. The orchestrator implements the deliberation protocol; this CLI just talks to it.
- Not bundled with an orchestrator. To run agents + orchestrator in one process, see `nsed serve` (BSL).

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](../../LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](../../LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
