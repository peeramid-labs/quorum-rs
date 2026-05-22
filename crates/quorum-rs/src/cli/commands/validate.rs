//! `quorum validate` — check that a workspace yaml deserialises cleanly
//! against the [`WorkspaceConfig`] schema and report a one-line summary.
//!
//! Pure CLI helper — no network calls, no LLM calls, no filesystem
//! mutation. Exits non-zero on parse failure so it can be wired into
//! a pre-commit hook or CI without further wrapping.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::workspace::WorkspaceConfig;

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
}
