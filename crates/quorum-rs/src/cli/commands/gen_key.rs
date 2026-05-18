//! `quorum gen-key` — generate an NKey seed for invite-code agent
//! bootstrap.
//!
//! This is step 1 of the brainless 3rd-party agent flow:
//!
//! 1. Operator runs `quorum gen-key` → generates an Ed25519 NKey
//!    seed, persists it locally, prints the public key.
//! 2. Operator pastes the public key into the admin's
//!    `POST /admin/api/invites/agent` request.
//! 3. Admin shares the resulting invite code back.
//! 4. Operator runs `quorum redeem <code>` → reads the persisted
//!    seed, redeems, writes `.creds`.
//!
//! The seed file is mode `0600` on Unix. Writing fails loudly when
//! a seed already exists unless `--force` is passed, so an
//! accidental re-run doesn't replace a working credential silently.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Default seed file location. `~/.nsed/agent.seed`.
///
/// Returns `None` only on the (rare) case where neither `$HOME` nor
/// `$USERPROFILE` is set — in which case the caller MUST pass an
/// explicit `--out` path.
pub fn default_seed_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("agent.seed");
    Some(p)
}

pub fn run(out: Option<&Path>, force: bool) -> Result<()> {
    let target = match out {
        Some(p) => p.to_path_buf(),
        None => default_seed_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default seed path — neither $HOME nor $USERPROFILE is set. \
                 Pass --out PATH explicitly."
            )
        })?,
    };

    if target.exists() && !force {
        anyhow::bail!(
            "Seed file already exists at {}. Pass --force to overwrite (this invalidates \
             any agent that's currently using the old seed).",
            target.display()
        );
    }

    let kp = nkeys::KeyPair::new_user();
    let seed = kp
        .seed()
        .map_err(|e| anyhow::anyhow!("Failed to extract NKey seed: {e}"))?;
    let pub_key = kp.public_key();

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    write_seed_file(&target, &seed)
        .with_context(|| format!("Failed to write seed file at {}", target.display()))?;

    eprintln!("Wrote agent NKey seed to {}", target.display());
    eprintln!();
    eprintln!("Send this public key to the admin (it's safe to share over any channel):");
    println!("{pub_key}");
    eprintln!();
    eprintln!("Then once the admin gives you an invite code, run:");
    eprintln!("    quorum redeem <CODE>");
    Ok(())
}

/// Write the seed to disk, restricting permissions to owner-only on
/// Unix. On Windows we skip the chmod (file permissions don't map
/// cleanly); the seed still lives under the user-profile dir which
/// is typically ACL-protected.
fn write_seed_file(path: &Path, seed: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        writeln!(f, "{seed}")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{seed}\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn gen_key_writes_seed_and_prints_pubkey() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        run(Some(&seed_path), false).expect("gen-key must succeed");
        let body = std::fs::read_to_string(&seed_path).unwrap();
        // NKey User SEEDS are SU-prefixed (the matching public keys
        // are UA-prefixed — different artefact, different prefix).
        assert!(
            body.trim().starts_with("SU"),
            "seed must be SU-prefixed nkey shape: {body}"
        );
    }

    #[test]
    fn gen_key_refuses_to_overwrite_without_force() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        run(Some(&seed_path), false).unwrap();
        let err = run(Some(&seed_path), false).unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "second run must mention --force: {err}"
        );
    }

    #[test]
    fn gen_key_overwrites_with_force() {
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        run(Some(&seed_path), false).unwrap();
        let first = std::fs::read_to_string(&seed_path).unwrap();
        run(Some(&seed_path), true).expect("--force must succeed");
        let second = std::fs::read_to_string(&seed_path).unwrap();
        assert_ne!(
            first.trim(),
            second.trim(),
            "force-regen must produce a new seed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gen_key_seed_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        run(Some(&seed_path), false).unwrap();
        let meta = std::fs::metadata(&seed_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "seed file must be owner-read-write only");
    }
}
