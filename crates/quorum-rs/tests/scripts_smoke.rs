//! Smoke tests for `scripts/telemetry_demo.sh`.
//!
//! The demo script is operator-facing documentation that ships inside
//! the repo. These tests exercise every mode to keep the contract
//! honest: if a future refactor (e.g. removing `eval`, restructuring
//! the argv handling) breaks the script, CI fails instead of a human
//! discovering it at a coderabbit review round.

use std::path::PathBuf;
use std::process::Command;

fn script_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/quorum-rs`; go up two levels
    // to the workspace root and then into `scripts/`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scripts/telemetry_demo.sh")
}

fn run_mode(mode: &str) -> std::process::Output {
    Command::new("bash")
        .arg(script_path())
        .arg(mode)
        .output()
        .unwrap_or_else(|e| panic!("spawning script in mode {mode:?} failed: {e}"))
}

/// Returns true only if `name` exists on PATH AND `name --version`
/// exits zero. `Output::status.success()` is checked explicitly —
/// `Command::output().is_ok()` would treat a non-zero exit as
/// "available", which can misclassify a broken install.
fn command_is_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `true` if the line uses `eval` as a standalone shell token (after
/// stripping any trailing comment). Token-based check rather than
/// `line.contains("eval ")` so that tab-separated forms (`eval\t…`),
/// punctuation-adjacent forms, and `# eval foo` comments are
/// classified correctly.
fn line_uses_eval(raw_line: &str) -> bool {
    // Strip everything after the first `#` to ignore in-line comments.
    let code = raw_line.split('#').next().unwrap_or("");
    code.split_whitespace().any(|tok| tok == "eval")
}

/// Every documented mode must exit 0 and produce non-empty stdout.
/// Modes that need `jq` are skipped when it is unavailable.
#[test]
fn script_modes_exit_zero_with_output() {
    let has_jq = command_is_available("jq");

    // `full`, `subjects`, `redaction` work without jq.
    for mode in ["full", "subjects", "redaction"] {
        let out = run_mode(mode);
        assert!(
            out.status.success(),
            "mode {mode:?} exit={}; stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !out.stdout.is_empty(),
            "mode {mode:?} produced empty stdout"
        );
    }
    // `types` and `agent-only` depend on jq.
    if has_jq {
        for mode in ["types", "agent-only"] {
            let out = run_mode(mode);
            assert!(
                out.status.success(),
                "mode {mode:?} exit={}; stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !out.stdout.is_empty(),
                "mode {mode:?} produced empty stdout"
            );
        }
    }
}

/// Unknown modes must exit with the script's documented contract:
/// exit code 2 (matches the explicit `exit 2` in the `*` case arm
/// alongside the usage line printed to stderr). Pinning the exact
/// code — not just non-success — catches a future refactor that
/// silently changes the contract for callers that branch on
/// status.
#[test]
fn script_rejects_unknown_mode() {
    let out = run_mode("not-a-real-mode");
    assert_eq!(
        out.status.code(),
        Some(2),
        "unknown mode should exit 2; status={:?}, stdout={}, stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The script must not use `eval` to dispatch the cargo command.
/// `eval` with a shell string is a well-known footgun (splits on
/// IFS, expands metachars); the proper pattern is a function or an
/// array. This test locks the removal so a future "quick fix" can't
/// regress it.
#[test]
fn script_does_not_use_eval() {
    let body = std::fs::read_to_string(script_path()).expect("reading demo script");
    for (lineno, line) in body.lines().enumerate() {
        // Skip whole-line comments; the detector strips trailing comments
        // itself, so we only need to short-circuit `# ... eval ...`.
        if line.trim_start().starts_with('#') {
            continue;
        }
        assert!(
            !line_uses_eval(line),
            "line {}: unexpected `eval` usage: {line}",
            lineno + 1
        );
    }
}

// ---------------------------------------------------------------------------
// Helper unit tests — guard the helpers themselves so the smoke tests
// keep meaning even if the helper logic is refactored. Both helpers
// were tightened in response to coderabbit findings on this PR.
// ---------------------------------------------------------------------------

/// `command_is_available` must inspect the child process's exit code,
/// not just `output().is_ok()`. A successfully-spawned command that
/// exits non-zero must be treated as unavailable.
#[test]
fn command_is_available_checks_exit_status() {
    // `false` exists on every Unix host and always exits 1. If this
    // returned `true` from `command_is_available`, the helper would
    // be checking only spawn success — which was the bug
    // coderabbit flagged.
    assert!(
        !command_is_available("false"),
        "a command that exits non-zero must NOT be reported as available"
    );
    // `true` exists on every Unix host and always exits 0.
    // (`true --version` happens to also exit 0 on GNU coreutils,
    // so this is the inverse half of the contract.)
    assert!(
        command_is_available("true"),
        "a command that exits zero must be reported as available"
    );
    // A genuinely missing binary must be reported as unavailable
    // without panicking — i.e. spawn failure must propagate as
    // `false`, not as an `unwrap()` panic.
    assert!(
        !command_is_available("nsed-definitely-not-a-real-binary-9d3b1c2e"),
        "a missing binary must be reported as unavailable"
    );
}

/// `line_uses_eval` must classify a wide variety of `eval` shapes
/// correctly: tab-separated, semicolon-adjacent, comment-stripped.
#[test]
fn line_uses_eval_token_classification() {
    // Positive cases — every one of these is a real `eval` invocation
    // and must trip the detector even though `line.contains("eval ")`
    // would only catch the first.
    for line in [
        "    eval \"$RUN\"",          // space-separated
        "\teval\t\"$RUN\"",           // tab-separated
        "if true; then eval foo; fi", // mid-line
        "  eval $cmd && echo done",   // followed by `$`
    ] {
        assert!(
            line_uses_eval(line),
            "expected eval-detector to flag: {line:?}"
        );
    }
    // Negative cases — words containing `eval` as a substring or
    // `eval` inside a comment must NOT trip the detector.
    for line in [
        "evaluate the result",    // substring of another word
        "eval_loop=foo",          // identifier prefix
        "echo done  # eval here", // inside a trailing comment
        "echo \"eval\"",          // a literal string, not a token
    ] {
        // Note: `echo \"eval\"` is technically a `\"eval\"` token, not
        // a bare `eval`. `split_whitespace` treats the surrounding
        // quotes as part of the token, so it does not match `== "eval"`.
        assert!(
            !line_uses_eval(line),
            "eval-detector should not flag: {line:?}"
        );
    }
}
