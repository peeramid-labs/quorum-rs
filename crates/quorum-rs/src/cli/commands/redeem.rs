//! `quorum redeem <code>` — redeem a JWT invite code for NATS
//! credentials and write `.creds` to disk.
//!
//! Step 4 of the 3rd-party agent bootstrap flow (see
//! [`super::gen_key`] for the full sequence). Reads the persisted
//! NKey seed, POSTs to the orchestrator's `/redeem-agent`, writes a
//! `.creds` file the agent worker can hand directly to NATS.
//!
//! UX defaults intentionally match `gen-key`:
//!
//! - Seed at `~/.nsed/agent.seed` (override with `--seed PATH`).
//! - Creds at `~/.nsed/agent.creds` (override with `--creds-out PATH`).
//! - Orchestrator URL from `--url` or `$ORCH_URL` (no default —
//!   admins always tell the operator what to point at).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::nats_utils::{RedeemInviteError, redeem_invite_with_orchestrator_with_retry};

/// Default creds file location. `~/.nsed/agent.creds`. See
/// [`super::gen_key::default_seed_path`] for the parallel convention.
pub fn default_creds_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("agent.creds");
    Some(p)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    code: &str,
    orchestrator_url: &str,
    seed_path: Option<&Path>,
    creds_out: Option<&Path>,
    force: bool,
    max_attempts: u32,
) -> Result<()> {
    // Resolve seed path with the same default convention as `gen-key`.
    let resolved_seed = match seed_path {
        Some(p) => p.to_path_buf(),
        None => super::gen_key::default_seed_path().ok_or_else(|| {
            anyhow::anyhow!("Cannot determine default seed path — pass --seed PATH explicitly.")
        })?,
    };
    let seed_str = std::fs::read_to_string(&resolved_seed).with_context(|| {
        format!(
            "Failed to read NKey seed at {}. Run `quorum gen-key` first.",
            resolved_seed.display()
        )
    })?;
    let seed_str = seed_str.trim();
    let keypair = nkeys::KeyPair::from_seed(seed_str).map_err(|e| {
        anyhow::anyhow!("Seed file at {:?} is not a valid NKey: {e}", resolved_seed)
    })?;

    let resolved_creds = match creds_out {
        Some(p) => p.to_path_buf(),
        None => default_creds_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default creds path — pass --creds-out PATH explicitly."
            )
        })?,
    };
    if resolved_creds.exists() && !force {
        anyhow::bail!(
            "Creds file already exists at {}. Pass --force to overwrite (only do this \
             if you're sure the existing creds are no longer needed).",
            resolved_creds.display()
        );
    }

    eprintln!("Redeeming invite at {}…", orchestrator_url);
    let result = match redeem_invite_with_orchestrator_with_retry(
        orchestrator_url,
        code,
        &keypair,
        max_attempts,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(redeem_error_message(e)),
    };

    if let Some(parent) = resolved_creds.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    write_creds_file(&resolved_creds, &result.creds)
        .with_context(|| format!("Failed to write creds file at {}", resolved_creds.display()))?;

    eprintln!("Wrote NATS credentials to {}", resolved_creds.display());
    eprintln!(
        "Connect with: nats://{}",
        result.nats_url.trim_start_matches("nats://")
    );
    eprintln!();
    eprintln!(
        "You can now start your agent. Point it at the creds file above and the URL \
         {}.",
        result.nats_url
    );
    Ok(())
}

/// Map a typed [`RedeemInviteError`] to an actionable
/// `anyhow::Error` for CLI output. The bail messages here are the
/// human-facing UX — kept terse, action-oriented, no "see docs".
fn redeem_error_message(e: RedeemInviteError) -> anyhow::Error {
    match e {
        RedeemInviteError::Expired => {
            anyhow::anyhow!("This invite code has expired. Ask the admin for a fresh code.")
        }
        RedeemInviteError::Replayed => anyhow::anyhow!(
            "This invite code was already redeemed on another host. Each code is single-use \
             — ask the admin for a fresh code."
        ),
        RedeemInviteError::Revoked => anyhow::anyhow!("The admin revoked this invite code."),
        RedeemInviteError::InvalidCode => anyhow::anyhow!(
            "This invite code is invalid. Common causes: tampered during copy/paste, wrong \
             code type (operator-token vs agent-credential), or signing-secret mismatch \
             between minting and redeem orchestrators."
        ),
        RedeemInviteError::NotConfigured => anyhow::anyhow!(
            "The orchestrator does not have invite codes configured. Ask the admin to set \
             APP_INVITES__SIGNING_SECRET on the orchestrator."
        ),
        RedeemInviteError::KvUnavailable => anyhow::anyhow!(
            "The orchestrator's backing store is temporarily unreachable. Try again in a minute."
        ),
        RedeemInviteError::Unexpected { status, body } => {
            anyhow::anyhow!("Unexpected response from orchestrator: HTTP {status} body={body:?}")
        }
        RedeemInviteError::Transport(inner) => inner.context("Failed to reach orchestrator"),
    }
}

fn write_creds_file(path: &Path, creds: &str) -> std::io::Result<()> {
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
        f.write_all(creds.as_bytes())?;
        if !creds.ends_with('\n') {
            writeln!(f)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut content = creds.to_string();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        std::fs::write(path, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redeem_error_message_translates_expired() {
        let msg = redeem_error_message(RedeemInviteError::Expired).to_string();
        assert!(
            msg.contains("expired"),
            "message must mention expired: {msg}"
        );
        assert!(
            msg.contains("fresh code"),
            "message must guide user to ask admin: {msg}"
        );
    }

    #[test]
    fn redeem_error_message_translates_replayed() {
        let msg = redeem_error_message(RedeemInviteError::Replayed).to_string();
        assert!(msg.contains("single-use"));
    }

    #[test]
    fn redeem_error_message_translates_invalid_code() {
        let msg = redeem_error_message(RedeemInviteError::InvalidCode).to_string();
        assert!(msg.contains("invalid"));
    }

    #[test]
    fn redeem_error_message_translates_revoked() {
        let msg = redeem_error_message(RedeemInviteError::Revoked).to_string();
        assert!(msg.contains("revoked"));
    }

    #[test]
    fn redeem_error_message_translates_not_configured() {
        let msg = redeem_error_message(RedeemInviteError::NotConfigured).to_string();
        assert!(msg.contains("APP_INVITES__SIGNING_SECRET"));
    }

    #[tokio::test]
    async fn redeem_succeeds_against_mock_orchestrator() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_jwt": "eyJ.test.jwt",
                "nats_url": "nats://localhost:4222",
                "agent_id": "bot-1",
            })))
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        let creds_path = tmp.path().join("agent.creds");
        // Generate a seed first via gen-key (covers the integration
        // between the two subcommands).
        super::super::gen_key::run(Some(&seed_path), false).unwrap();

        run(
            "fake.invite.code",
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            false,
            1,
        )
        .await
        .expect("redeem must succeed against 200 mock");

        let body = std::fs::read_to_string(&creds_path).unwrap();
        assert!(body.contains("eyJ.test.jwt"), "creds must embed JWT");
        assert!(
            body.contains("BEGIN USER NKEY SEED"),
            "creds must embed seed"
        );
    }

    #[tokio::test]
    async fn redeem_refuses_to_overwrite_existing_creds_without_force() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_jwt": "eyJ.x.y",
                "nats_url": "nats://localhost:4222",
                "agent_id": "bot-1",
            })))
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        let creds_path = tmp.path().join("agent.creds");
        super::super::gen_key::run(Some(&seed_path), false).unwrap();
        std::fs::write(&creds_path, "pre-existing").unwrap();

        let err = run(
            "code",
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            false,
            1,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "must mention --force: {err}"
        );
        // Original file untouched.
        assert_eq!(
            std::fs::read_to_string(&creds_path).unwrap(),
            "pre-existing"
        );
    }

    #[tokio::test]
    async fn redeem_surfaces_expired_clearly() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "expired"
            })))
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let seed_path = tmp.path().join("agent.seed");
        let creds_path = tmp.path().join("agent.creds");
        super::super::gen_key::run(Some(&seed_path), false).unwrap();

        let err = run(
            "stale.code",
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            false,
            1,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("expired"), "got: {err}");
        // No creds file written on error.
        assert!(!creds_path.exists());
    }
}
