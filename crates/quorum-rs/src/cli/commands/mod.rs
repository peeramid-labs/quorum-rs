//! CLI subcommand implementations.
//!
//! Each submodule owns one `quorum` subcommand and exposes a `run()`
//! entry point that `main.rs` dispatches into after `clap` parses
//! the user's flags. Submodules are kept thin — argument parsing
//! and signal handling live here, while the actual work
//! (orchestrator HTTP calls, deliberation streaming, agent fleet
//! boot, etc.) lives in the SDK so library consumers can drive the
//! same flow from their own binary.
//!
//! | Module | Subcommand | What it does |
//! |---|---|---|
//! | [`common`] | — | Shared helpers (orchestrator resolution, env-token expansion). |
//! | [`init`] | `quorum init` | Bootstrap a workspace yaml or agent fleet config. |
//! | [`redeem`] | `quorum redeem` | Exchange a JWT invite code for NATS credentials. |
//! | [`run`] | `quorum run` | Dispatch a deliberation task into a room. |
//! | [`serve`] | `quorum serve` | Boot a fleet of agents from `agent.yml`. |
//! | [`status`] | `quorum status` | Health check + agent listing. |
//! | [`trace`] | `quorum trace` | Display a deliberation trace. |
//! | [`validate`] | `quorum validate` | Schema-check a workspace yaml. |

pub mod common;
pub mod init;
pub mod init_wizard;
pub mod redeem;
pub mod run;
pub mod serve;
pub mod status;
pub mod trace;
pub mod validate;
