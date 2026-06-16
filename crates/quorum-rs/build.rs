//! Capture the git commit sha + commit date into compile-time env so
//! `quorum --version` can report which build you're running. Falls back
//! to "unknown" when git isn't available (e.g. a source tarball / CI
//! without `.git`). Uses the *commit* date (not wall-clock) so builds
//! stay reproducible.

use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let date =
        git(&["show", "-s", "--format=%cs", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=QUORUM_GIT_SHA={sha}");
    println!("cargo:rustc-env=QUORUM_GIT_DATE={date}");

    // Re-run when HEAD moves (handles submodule git-dir indirection).
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
