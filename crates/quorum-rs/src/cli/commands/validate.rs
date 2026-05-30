//! `quorum validate` — check that a workspace yaml deserialises cleanly
//! against the [`WorkspaceConfig`] schema and report a one-line summary.
//!
//! Pure CLI helper — no network calls, no LLM calls, no filesystem
//! mutation. Exits non-zero on parse failure so it can be wired into
//! a pre-commit hook or CI without further wrapping.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::workspace::WorkspaceConfig;

/// Load the workspace yaml at `path`, validate it against the
/// [`WorkspaceConfig`] schema, and print a one-line summary.
///
/// Behaviour:
///
/// - On success: prints a single line to stdout naming the policy /
///   orchestrator / room counts and the resolved `default_room`
///   (`"(none)"` when the field is absent), and returns
///   [`ExitCode::SUCCESS`].
/// - On any failure (path missing, file unreadable, yaml syntax
///   error, schema validation error): prints the error to stderr
///   and returns [`ExitCode::FAILURE`].
///
/// The function performs no network calls, no LLM invocations, and
/// no filesystem mutation — safe to wire into pre-commit hooks or
/// CI pipelines.
pub fn run(path: &Path) -> ExitCode {
    let config = match WorkspaceConfig::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let default_room = config.default_room.as_deref().unwrap_or("(none)");

    println!(
        "valid — {} policies, {} orchestrators, {} rooms, default_room: {}",
        config.policies.len(),
        config.orchestrators.len(),
        config.rooms.len(),
        default_room,
    );

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitCode;
    use tempfile::tempdir;

    fn write_yaml(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("nsed.yaml");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn run_succeeds_on_minimal_valid_yaml() {
        let dir = tempdir().unwrap();
        let path = write_yaml(
            &dir,
            r#"orchestrators:
  primary:
    mode: remote
    address: "https://example.test"
    token: "${TOK}"
policies:
  default:
    agents: ["A", "B"]
    max_rounds: 1
    effort: 0.5
rooms:
  demo:
    policy: default
    orchestrator: primary
default_room: demo
"#,
        );
        assert_eq!(run(&path), ExitCode::SUCCESS);
    }

    #[test]
    fn run_fails_on_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.yaml");
        assert_eq!(run(&missing), ExitCode::FAILURE);
    }

    #[test]
    fn run_fails_on_malformed_yaml() {
        let dir = tempdir().unwrap();
        let path = write_yaml(&dir, "this is: : not valid yaml ::");
        assert_eq!(run(&path), ExitCode::FAILURE);
    }

    /// Workspace without `default_room` set — exercises the
    /// `unwrap_or("(none)")` branch on `default_room` resolution.
    /// `WorkspaceConfig::resolve_room` auto-picks the only room
    /// when no default is configured, so a single-room workspace
    /// validates cleanly without a `default_room:` key.
    #[test]
    fn run_succeeds_when_default_room_is_absent() {
        let dir = tempdir().unwrap();
        let path = write_yaml(
            &dir,
            r#"orchestrators:
  primary:
    mode: remote
    address: "https://example.test"
    token: "${TOK}"
policies:
  default:
    agents: ["A", "B"]
    max_rounds: 1
    effort: 0.5
rooms:
  only-one:
    policy: default
    orchestrator: primary
"#,
        );
        // Re-parse the config to confirm the field really is None;
        // a fixture regression that accidentally sets default_room
        // would silently pass `run()` without exercising the
        // intended branch.
        let parsed = crate::cli::workspace::WorkspaceConfig::load(&path).unwrap();
        assert!(
            parsed.default_room.is_none(),
            "fixture must not carry a default_room — branch coverage relies on it"
        );
        assert_eq!(run(&path), ExitCode::SUCCESS);
    }
}
