//! `quorum redeem <code>` — redeem a JWT invite code, generate a
//! fresh NKey locally, persist `.creds` + `.seed`, and print a
//! summary to the terminal.
//!
//! This is the one-step bootstrap path for 3rd-party agent
//! operators. The operator never has to share a pubkey with the
//! admin in advance — `quorum redeem` generates the NKey on the
//! redeeming host and presents the public half to the orchestrator
//! at redeem time. The seed never crosses the network.
//!
//! UX defaults:
//!
//! - Seed at `~/.nsed/agent.seed` (override with `--seed-out PATH`).
//! - Creds at `~/.nsed/agent.creds` (override with `--creds-out PATH`).
//! - Orchestrator URL defaults to `https://api.peeramid.xyz`; setting
//!   `NSED_ENV=local` (or `dev`/`development`) flips it to
//!   `http://localhost:8080`. `--url` or `$ORCH_URL` override.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::nats_utils::{RedeemInviteError, redeem_invite_with_orchestrator_with_retry};

/// Production orchestrator URL — the default `quorum redeem`
/// points at when the operator hasn't passed `--url` or
/// `$ORCH_URL`. Matches the URL the `nsed init` wizard suggests.
pub const PRODUCTION_ORCHESTRATOR_URL: &str = "https://api.peeramid.xyz";

/// Local orchestrator URL — used by [`default_orchestrator_url`]
/// when `NSED_ENV` is `local`/`dev`/`development`. Matches the
/// default port `nsed serve` binds when run without overrides.
pub const LOCAL_ORCHESTRATOR_URL: &str = "http://localhost:8080";

/// Resolve the default orchestrator URL based on `NSED_ENV`. The
/// `--url` flag and `$ORCH_URL` env var still win when set; this
/// only kicks in when neither is provided.
///
/// Returns [`LOCAL_ORCHESTRATOR_URL`] when `NSED_ENV` ∈
/// {`local`, `dev`, `development`} (case-insensitive, whitespace-
/// trimmed). Otherwise returns [`PRODUCTION_ORCHESTRATOR_URL`].
pub fn default_orchestrator_url() -> String {
    let mode = std::env::var("NSED_ENV").unwrap_or_default();
    let normalized = mode.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "local" | "dev" | "development" => LOCAL_ORCHESTRATOR_URL.to_string(),
        _ => PRODUCTION_ORCHESTRATOR_URL.to_string(),
    }
}

/// Default seed file location. `~/.nsed/agent.seed`.
fn default_seed_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("agent.seed");
    Some(p)
}

/// Default creds file location. `~/.nsed/agent.creds`.
pub fn default_creds_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("agent.creds");
    Some(p)
}

pub async fn run(
    code: &str,
    orchestrator_url: &str,
    seed_out: Option<&Path>,
    creds_out: Option<&Path>,
    force: bool,
    max_attempts: u32,
) -> Result<()> {
    let resolved_creds = match creds_out {
        Some(p) => p.to_path_buf(),
        None => default_creds_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default creds path — pass --creds-out PATH explicitly."
            )
        })?,
    };
    let resolved_seed = match seed_out {
        Some(p) => p.to_path_buf(),
        None => default_seed_path().ok_or_else(|| {
            anyhow::anyhow!("Cannot determine default seed path — pass --seed-out PATH explicitly.")
        })?,
    };
    for (label, path) in [("creds", &resolved_creds), ("seed", &resolved_seed)] {
        if path.exists() && !force {
            anyhow::bail!(
                "{label} file already exists at {}. Pass --force to overwrite (only do this \
                 if you're sure the existing file is no longer needed).",
                path.display()
            );
        }
    }

    eprintln!("Redeeming invite at {orchestrator_url}…");
    let result = match redeem_invite_with_orchestrator_with_retry(
        orchestrator_url,
        code,
        max_attempts,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(redeem_error_message(e)),
    };

    let seed = result
        .keypair
        .seed()
        .map_err(|e| anyhow::anyhow!("Failed to extract NKey seed from redeem result: {e}"))?;

    for (label, path, content) in [
        ("creds", &resolved_creds, result.creds.as_str()),
        ("seed", &resolved_seed, seed.as_str()),
    ] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }
        write_secret_file(path, content)
            .with_context(|| format!("Failed to write {label} file at {}", path.display()))?;
    }

    println!();
    println!("✓ Redeemed invite. NATS credentials are ready.");
    println!();
    println!("  Connect URL : {}", result.nats_url);
    println!("  Agent pubkey: {}", result.keypair.public_key());
    println!("  Creds file  : {}", resolved_creds.display());
    println!("  Seed file   : {}", resolved_seed.display());
    println!();
    println!("Both files are written mode 0600 on Unix. Keep the seed");
    println!("private — it's the long-lived half of your NATS identity.");
    Ok(())
}

/// Map a typed [`RedeemInviteError`] to an actionable
/// `anyhow::Error` for CLI output.
fn redeem_error_message(e: RedeemInviteError) -> anyhow::Error {
    match e {
        RedeemInviteError::Expired => {
            anyhow::anyhow!("This invite code has expired. Ask the admin for a fresh code.")
        }
        RedeemInviteError::Replayed => anyhow::anyhow!(
            "This invite code was already redeemed. Each code is single-use — ask the admin \
             for a fresh code."
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

fn write_secret_file(path: &Path, content: &str) -> std::io::Result<()> {
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
        f.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            writeln!(f)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut s = content.to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        std::fs::write(path, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── default_orchestrator_url ─────────────────────────────────────

    /// Helper: set NSED_ENV for the duration of one resolver call and
    /// restore the prior value afterwards. Tests are serialised on the
    /// `nsed_env` group so they don't race each other's mutations.
    fn with_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let prev = std::env::var("NSED_ENV").ok();
        // SAFETY: serialised via `serial_test::serial(nsed_env)` on
        // every caller; no other code mutates NSED_ENV during the
        // call window.
        unsafe {
            match value {
                Some(v) => std::env::set_var("NSED_ENV", v),
                None => std::env::remove_var("NSED_ENV"),
            }
        }
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("NSED_ENV", v),
                None => std::env::remove_var("NSED_ENV"),
            }
        }
        out
    }

    #[test]
    #[serial_test::serial(nsed_env)]
    fn default_url_unset_env_returns_production() {
        let out = with_env(None, default_orchestrator_url);
        assert_eq!(out, PRODUCTION_ORCHESTRATOR_URL);
    }

    #[test]
    #[serial_test::serial(nsed_env)]
    fn default_url_local_env_returns_localhost() {
        for variant in [
            "local",
            "LOCAL",
            "  local  ",
            "dev",
            "development",
            "Development",
        ] {
            let out = with_env(Some(variant), default_orchestrator_url);
            assert_eq!(out, LOCAL_ORCHESTRATOR_URL, "variant {variant:?}");
        }
    }

    #[test]
    #[serial_test::serial(nsed_env)]
    fn default_url_production_or_unknown_returns_production() {
        for variant in ["production", "staging", "prod", "", "  "] {
            let out = with_env(Some(variant), default_orchestrator_url);
            assert_eq!(out, PRODUCTION_ORCHESTRATOR_URL, "variant {variant:?}");
        }
    }

    #[test]
    fn redeem_error_message_translates_expired() {
        let msg = redeem_error_message(RedeemInviteError::Expired).to_string();
        assert!(
            msg.contains("expired") && msg.contains("fresh code"),
            "{msg}"
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
    async fn redeem_writes_creds_and_seed_on_success() {
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

        let creds_body = std::fs::read_to_string(&creds_path).unwrap();
        assert!(creds_body.contains("eyJ.test.jwt"), "creds must embed JWT");
        assert!(
            creds_body.contains("BEGIN USER NKEY SEED"),
            "creds must embed seed"
        );
        let seed_body = std::fs::read_to_string(&seed_path).unwrap();
        assert!(
            seed_body.trim().starts_with("SU"),
            "seed file must contain SU-prefixed nkey: {seed_body}"
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
        assert_eq!(
            std::fs::read_to_string(&creds_path).unwrap(),
            "pre-existing"
        );
        assert!(
            !seed_path.exists(),
            "seed must not be written when creds overwrite is blocked"
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
        assert!(!creds_path.exists());
        assert!(!seed_path.exists());
    }
}
