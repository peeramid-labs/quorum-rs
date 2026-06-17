//! `quorum redeem <code>` — redeem a JWT invite code, generate a
//! fresh NKey locally, persist `.creds` + `.seed` (when the code
//! grants NATS credentials) and/or `.token` (when the code grants
//! an HTTP bearer), and print a summary to the terminal.
//!
//! The CLI dispatches on the code's `aud` claim:
//!
//!   * `aud = "nsed-operator-redeem"` → POST `/redeem` (the
//!     `/admin/api/invites` family — chat users + operators).
//!     With `capabilities: ["agent"]` on the code, this path also
//!     mints NATS creds and writes them to disk; with chat-only
//!     codes it just writes the bearer token.
//!   * `aud = "nsed-agent-redeem"` → POST `/redeem-agent` (the
//!     `/admin/api/agent-invites` family — pure NATS agent
//!     workers). Always mints creds; never mints an HTTP bearer.
//!   * no `aud` (legacy pre-#444 codes) → falls through to the
//!     operator path; pre-#444 the only mint endpoint was
//!     `/admin/api/invites`.
//!
//! The operator never has to share a pubkey with the admin in
//! advance — `quorum redeem` generates the NKey on the redeeming
//! host and presents the public half to the orchestrator at redeem
//! time. The seed never crosses the network.
//!
//! UX defaults:
//!
//! - Seed at `~/.nsed/agent.seed` (override with `--seed-out PATH`).
//! - Creds at `~/.nsed/agent.creds` (override with `--creds-out PATH`).
//! - Bearer token at `~/.nsed/operator.token` (override with
//!   `--token-out PATH`).
//! - Orchestrator URL defaults to `https://api.peeramid.xyz`; setting
//!   `NSED_ENV=local` (or `dev`/`development`) flips it to
//!   `http://localhost:8080`. `--url` or `$ORCH_URL` override.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::nats_utils::{
    AUD_AGENT_REDEEM, AUD_OPERATOR_REDEEM, RedeemInviteError, format_nats_creds, invite_audience,
    redeem_invite_with_orchestrator_with_retry,
    redeem_operator_invite_with_orchestrator_with_retry,
};

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

/// Default operator bearer-token file location. `~/.nsed/operator.token`.
/// Mode 0600 on Unix; one-line, no trailing whitespace, suitable for
/// `Authorization: Bearer $(cat ~/.nsed/operator.token)`.
fn default_token_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let mut p = PathBuf::from(home);
    p.push(".nsed");
    p.push("operator.token");
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

// Each argument maps to a distinct CLI flag (`--seed-out`,
// `--creds-out`, `--token-out`, `--force`, `--seed-in`,
// `--max-attempts`) — collapsing them into a struct would just
// move the same arity into a builder.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    code: &str,
    orchestrator_url: &str,
    seed_out: Option<&Path>,
    creds_out: Option<&Path>,
    token_out: Option<&Path>,
    force: bool,
    seed_in: Option<&Path>,
    max_attempts: u32,
) -> Result<()> {
    // Resolve the NKey FIRST — surfaces `--seed-in` errors before
    // any orchestrator round-trip and before audience decode, so a
    // malformed seed file fails cleanly regardless of what the
    // invite code looks like.
    let keypair = load_or_generate_keypair(seed_in)?;

    // Pick the right endpoint by looking at the code's audience pin.
    // Pre-#444 codes don't carry an `aud` — those went to
    // /admin/api/invites only, so fall through to the operator path.
    // Anything else (malformed JWT, unrecognised audience) bails
    // before any side effect with the same UX as the orchestrator's
    // own `invalid_code` 401.
    let aud = invite_audience(code).context("invite code is not in JWT shape")?;
    match aud.as_deref() {
        Some(AUD_AGENT_REDEEM) => {
            run_agent_redeem(
                code,
                orchestrator_url,
                seed_out,
                creds_out,
                force,
                keypair,
                max_attempts,
            )
            .await
        }
        Some(AUD_OPERATOR_REDEEM) | None => {
            run_operator_redeem(
                code,
                orchestrator_url,
                seed_out,
                creds_out,
                token_out,
                force,
                keypair,
                max_attempts,
            )
            .await
        }
        Some(other) => {
            anyhow::bail!(
                "invite code carries unknown audience {other:?}. \
                 Expected {AUD_OPERATOR_REDEEM:?} or {AUD_AGENT_REDEEM:?}."
            )
        }
    }
}

/// Operator-code path: POSTs `/redeem`, gets back an HTTP bearer
/// token and (for unified codes carrying the `agent` capability)
/// a scoped NATS User JWT + creds. Used for invites minted via
/// `/admin/api/invites` — the `/invite` admin page's output.
#[allow(clippy::too_many_arguments)]
async fn run_operator_redeem(
    code: &str,
    orchestrator_url: &str,
    seed_out: Option<&Path>,
    creds_out: Option<&Path>,
    token_out: Option<&Path>,
    force: bool,
    keypair: nkeys::KeyPair,
    max_attempts: u32,
) -> Result<()> {
    // Resolve all three output paths upfront and validate uniqueness
    // BEFORE the orchestrator round-trip OR any file write. A typo
    // like `--token-out=~/.nsed/agent.creds` would otherwise have
    // the bearer-write clobber the NATS creds file (or vice versa)
    // after a successful redeem — same foot-gun the existing
    // `--seed-out`/`--creds-out` collision check guards against.
    //
    // Creds + seed paths are resolved even for chat-only redeems so
    // the collision check runs regardless. They're only WRITTEN
    // when the response carries `user_jwt` + `nats_url`.
    let resolved_token = match token_out {
        Some(p) => p.to_path_buf(),
        None => default_token_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine default token path — pass --token-out PATH explicitly."
            )
        })?,
    };
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
    for (a_label, a_path, b_label, b_path) in [
        (
            "--token-out",
            &resolved_token,
            "--creds-out",
            &resolved_creds,
        ),
        ("--token-out", &resolved_token, "--seed-out", &resolved_seed),
        ("--creds-out", &resolved_creds, "--seed-out", &resolved_seed),
    ] {
        if a_path == b_path {
            anyhow::bail!(
                "{a_label} and {b_label} resolve to the same path ({}). \
                 Each file format is distinct; one would overwrite the other.",
                a_path.display()
            );
        }
    }
    if resolved_token.exists() && !force {
        anyhow::bail!(
            "token file already exists at {}. Pass --force to overwrite (only do this \
             if you're sure the existing file is no longer needed).",
            resolved_token.display()
        );
    }

    // Pubkey is presented to the orchestrator. The orchestrator
    // silently ignores it for chat-only codes (`capabilities: ["chat"]`).
    let pub_key = keypair.public_key();

    eprintln!("Redeeming invite at {orchestrator_url}…");
    let resp = match redeem_operator_invite_with_orchestrator_with_retry(
        orchestrator_url,
        code,
        Some(&pub_key),
        Some("quorum-cli"),
        max_attempts,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return Err(redeem_error_message(e)),
    };

    // Always write the bearer.
    if let Some(parent) = resolved_token.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    write_secret_file(&resolved_token, &resp.token)
        .with_context(|| format!("Failed to write token file at {}", resolved_token.display()))?;

    // Persist the orchestrator HTTP address beside the token so the
    // config-free client (`quorum run`/`status`/`rooms` with no nsed.yaml)
    // can reach this orchestrator later. Best-effort: a write failure here
    // must not undo a successful redeem — warn and continue.
    let endpoint_path =
        match crate::cli::endpoint::persist_endpoint(&resolved_token, orchestrator_url) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("warning: could not persist orchestrator endpoint: {e}");
                None
            }
        };

    // For unified codes we also got NATS creds back — write them in
    // the same `.creds` + `.seed` layout the agent path uses, so
    // downstream tooling (yaml `nats.auth.creds_file`, etc.) doesn't
    // need to know which redeem path the operator went through.
    let unified = match (&resp.user_jwt, &resp.nats_url) {
        (Some(jwt), Some(url)) => Some((jwt.clone(), url.clone())),
        _ => None,
    };

    if let Some((user_jwt, nats_url)) = unified.as_ref() {
        for (label, path) in [("creds", &resolved_creds), ("seed", &resolved_seed)] {
            if path.exists() && !force {
                anyhow::bail!(
                    "{label} file already exists at {}. Pass --force to overwrite.",
                    path.display()
                );
            }
        }
        let seed = keypair
            .seed()
            .map_err(|e| anyhow::anyhow!("Failed to extract NKey seed: {e}"))?;
        let creds = format_nats_creds(user_jwt, &seed);
        for (label, path, content) in [
            ("creds", &resolved_creds, creds.as_str()),
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
        println!("✓ Redeemed unified invite (operator + agent).");
        println!();
        println!("  Operator     : {}", resp.name);
        println!("  Token file   : {}", resolved_token.display());
        if let Some(p) = &endpoint_path {
            println!("  Endpoint file: {}", p.display());
        }
        println!("  Connect URL  : {nats_url}");
        println!("  Agent pubkey : {pub_key}");
        println!("  Creds file   : {}", resolved_creds.display());
        println!("  Seed file    : {}", resolved_seed.display());
    } else {
        println!();
        println!("✓ Redeemed operator invite (chat-only).");
        println!();
        println!("  Operator    : {}", resp.name);
        println!("  Token file  : {}", resolved_token.display());
        if let Some(p) = &endpoint_path {
            println!("  Endpoint    : {}", p.display());
        }
        if let Some(budget) = resp.budget {
            println!("  Budget cap  : {budget} credits");
        }
    }
    println!();
    println!("Files are written mode 0600 on Unix. Keep them private —");
    println!("the bearer is the only way to authenticate as this operator.");
    Ok(())
}

/// Agent-code path: POSTs `/redeem-agent`, mints NATS creds only.
/// Used for invites minted via `/admin/api/agent-invites`.
#[allow(clippy::too_many_arguments)]
async fn run_agent_redeem(
    code: &str,
    orchestrator_url: &str,
    seed_out: Option<&Path>,
    creds_out: Option<&Path>,
    force: bool,
    keypair: nkeys::KeyPair,
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
    // Catch the foot-gun where a typo sets `--seed-out` and
    // `--creds-out` to the same path: the second write would
    // overwrite the first and the operator would silently lose
    // either the seed or the creds depending on order.
    if resolved_creds == resolved_seed {
        anyhow::bail!(
            "--seed-out and --creds-out resolve to the same path ({}). The creds blob and \
             the raw seed are different on-disk formats; one would overwrite the other.",
            resolved_creds.display()
        );
    }
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
        &keypair,
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

/// Resolve the NKey: caller-supplied `--seed-in` PATH wins (lets an
/// operator pre-stage a seed and keep the same pubkey across
/// redemptions). Otherwise generate a fresh keypair. Either way the
/// key is allocated BEFORE the redeem call so the retry helper
/// presents the SAME pubkey on every attempt — a 5xx after the
/// orchestrator marked the JTI redeemed would otherwise consume the
/// invite on attempt N and present a new pubkey on attempt N+1 →
/// 409 Replayed and a lost invite.
fn load_or_generate_keypair(seed_in: Option<&Path>) -> Result<nkeys::KeyPair> {
    match seed_in {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read seed file at {}", path.display()))?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                anyhow::bail!(
                    "--seed-in file {} is empty. Expected a `SU…`-prefixed NKey seed.",
                    path.display()
                );
            }
            nkeys::KeyPair::from_seed(trimmed).with_context(|| {
                format!(
                    "Failed to parse seed from {} — must be a `SU…`-prefixed NKey user seed",
                    path.display()
                )
            })
        }
        None => Ok(nkeys::KeyPair::new_user()),
    }
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
        RedeemInviteError::Decode(inner) => inner.context(
            "Orchestrator accepted the invite but the SDK couldn't process the response. \
             The invite is now consumed — ask the admin for a fresh code",
        ),
    }
}

fn write_secret_file(path: &Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
        drop(f);
        // `OpenOptionsExt::mode` only applies when the file is newly
        // created — if the caller passed `--force` and the target
        // already existed with looser perms, the truncate kept those
        // perms. Re-stamp explicitly to close that window.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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

    // ── JWT-shaped code helpers ───────────────────────────────────────
    //
    // The aud-dispatch in `run()` decodes the JWT payload to pick an
    // endpoint, so test inputs need to be JWT-shaped even though the
    // mock server never verifies signatures. Both helpers produce a
    // 3-segment dot-separated string with a base64url payload that
    // parses as `{"sub": ..., "exp": ..., "aud": ...}`.

    fn agent_invite_code() -> String {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"bot-1","exp":9999999999,"aud":"nsed-agent-redeem"}"#);
        format!("{header}.{payload}.AAAA")
    }

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
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            None, // token_out
            false,
            None,
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
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            None, // token_out
            false,
            None,
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
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            None, // token_out
            false,
            None,
            1,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("expired"), "got: {err}");
        assert!(!creds_path.exists());
        assert!(!seed_path.exists());
    }

    /// Coderabbit (RUSTSEC-style nit): `--force` overwriting a
    /// pre-existing file with broader perms would previously leave
    /// the file at the OLD perms (e.g. 0644) because
    /// `OpenOptionsExt::mode` only applies on file creation. The
    /// post-write `set_permissions` call must clamp every successful
    /// write to 0600 regardless of prior state. Test sets up a
    /// world-readable creds file, runs `--force` redeem, asserts the
    /// mode is exactly 0600 after.
    #[cfg(unix)]
    #[tokio::test]
    async fn redeem_force_overwrites_to_0600_even_from_looser_perms() {
        use std::os::unix::fs::PermissionsExt;
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
        // Seed both files at world-readable perms — what a
        // misconfigured editor / earlier broken redeem might leave.
        std::fs::write(&creds_path, "stale-creds").unwrap();
        std::fs::write(&seed_path, "stale-seed").unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&seed_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        run(
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            None, // token_out
            /* force = */ true,
            None,
            1,
        )
        .await
        .expect("force redeem against 200 mock must succeed");

        for path in [&creds_path, &seed_path] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "{} must end at 0600 after --force overwrite; got {mode:o}",
                path.display()
            );
        }
    }

    /// Coderabbit: a typo passing the SAME path to `--seed-out` and
    /// `--creds-out` would silently overwrite one file with the
    /// other (creds writes first, then seed clobbers it). Reject
    /// upfront — no orchestrator round-trip, no partial state.
    #[tokio::test]
    async fn redeem_rejects_identical_seed_and_creds_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let same = tmp.path().join("agent.both");
        let err = run(
            &agent_invite_code(),
            "http://orchestrator.invalid",
            Some(&same),
            Some(&same),
            None, // token_out
            false,
            None,
            1,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("same path") || msg.contains("resolve to the same"),
            "error must explain the collision; got: {msg}"
        );
        // Neither file must have been touched.
        assert!(!same.exists());
    }

    /// Operator-redeem path has a third secret file (`--token-out`).
    /// All three pairs must be checked for collision — token vs
    /// creds, token vs seed, creds vs seed — before any write hits
    /// the disk. Failure mode this guards against: a typo like
    /// `--token-out=~/.nsed/agent.creds` would have the bearer
    /// write clobber the NATS creds after a successful redeem,
    /// silently losing one secret to recover the other.
    #[tokio::test]
    async fn redeem_operator_rejects_token_creds_path_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let same = tmp.path().join("collide");
        // Use a JWT-shaped operator code so dispatch routes to the
        // operator path and the collision check fires. No HTTP
        // mock needed — the collision check is upstream of the
        // network call.
        let err = run(
            &operator_invite_code(),
            "http://orchestrator.invalid",
            Some(&tmp.path().join("agent.seed")),
            Some(&same), // creds
            Some(&same), // token — collides with creds
            false,
            None,
            1,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--token-out") && msg.contains("--creds-out"),
            "error must name both colliding flags; got: {msg}"
        );
        assert!(!same.exists(), "no file may have been written");
    }

    #[tokio::test]
    async fn redeem_operator_rejects_token_seed_path_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let same = tmp.path().join("collide");
        let err = run(
            &operator_invite_code(),
            "http://orchestrator.invalid",
            Some(&same), // seed
            Some(&tmp.path().join("agent.creds")),
            Some(&same), // token — collides with seed
            false,
            None,
            1,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--token-out") && msg.contains("--seed-out"),
            "error must name both colliding flags; got: {msg}"
        );
        assert!(!same.exists(), "no file may have been written");
    }

    /// `--seed-in` lets the operator pre-stage a seed and reuse the
    /// same pubkey across redemptions. The CLI must load that seed,
    /// pass it to the SDK helper, and end up with a creds file whose
    /// embedded seed matches the supplied one verbatim.
    #[tokio::test]
    async fn redeem_loads_seed_from_seed_in_when_supplied() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_jwt": "eyJ.fake.jwt",
                "nats_url": "nats://localhost:4222",
                "agent_id": "bot-1",
            })))
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let staged = tmp.path().join("staged.seed");
        let expected_kp = nkeys::KeyPair::new_user();
        let expected_seed = expected_kp.seed().unwrap();
        let expected_pub = expected_kp.public_key();
        std::fs::write(&staged, &expected_seed).unwrap();

        let seed_path = tmp.path().join("agent.seed");
        let creds_path = tmp.path().join("agent.creds");
        run(
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            None, // token_out
            false,
            Some(&staged),
            1,
        )
        .await
        .expect("redeem with --seed-in must succeed");

        // The written seed file must contain the SUPPLIED seed
        // verbatim — proves `--seed-in` was honoured end-to-end
        // and not silently overridden by a fresh keypair.
        let written = std::fs::read_to_string(&seed_path).unwrap();
        assert_eq!(
            written.trim(),
            expected_seed,
            "written seed must match the supplied --seed-in seed"
        );
        // The body posted to the orchestrator must carry the
        // matching pubkey.
        let reqs = mock_server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["user_pub_key"].as_str(), Some(expected_pub.as_str()));
    }

    /// `--seed-in` pointing at an empty file is a foot-gun. Reject
    /// before any orchestrator round-trip.
    #[tokio::test]
    async fn redeem_rejects_empty_seed_in_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty.seed");
        std::fs::write(&empty, "").unwrap();
        let err = run(
            "code",
            "http://orchestrator.invalid",
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            None, // token_out
            false,
            Some(&empty),
            1,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "must mention emptiness: {err}"
        );
    }

    /// Garbage in the seed file (anything that isn't an `SU…` NKey
    /// seed) must produce a clear error before the redeem call,
    /// not after — otherwise the JTI would be consumed on a
    /// nonsense pubkey.
    #[tokio::test]
    async fn redeem_rejects_malformed_seed_in_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bad = tmp.path().join("bad.seed");
        std::fs::write(&bad, "not-a-real-nkey-seed").unwrap();
        let err = run(
            "code",
            "http://orchestrator.invalid",
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            None, // token_out
            false,
            Some(&bad),
            1,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("parse seed") || err.to_string().contains("NKey"),
            "must mention seed-parse failure: {err}"
        );
    }

    // ── aud-dispatch (operator-token vs agent-credential routing) ──

    /// Build a JWT-shaped invite code with `aud=nsed-operator-redeem`.
    /// Same construction as the orchestrator's `sign_invite`, minus
    /// the HMAC (the test mock doesn't verify signatures — the real
    /// orchestrator does, that's its concern).
    fn operator_invite_code() -> String {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"alice","exp":9999999999,"aud":"nsed-operator-redeem"}"#);
        format!("{header}.{payload}.AAAA")
    }

    /// Regression for the user-reported bug: `quorum redeem` against
    /// an operator-token code (from `/admin/api/invites`, e.g. the
    /// `/invite` admin page) used to always POST `/redeem-agent` and
    /// get back `invalid_code` because of the audience-pin mismatch.
    /// The new dispatch must route to `/redeem` for operator codes
    /// and write the bearer to `--token-out`.
    #[tokio::test]
    async fn redeem_dispatches_operator_code_to_redeem_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        // /redeem is the only endpoint that should fire. /redeem-agent
        // has `.expect(0)` so the test hard-fails if the CLI ever
        // misroutes again.
        Mock::given(method("POST"))
            .and(path("/redeem"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "op-abc-123",
                "name": "alice",
                "budget": 5.0,
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let token_path = tmp.path().join("operator.token");

        run(
            &operator_invite_code(),
            &mock_server.uri(),
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            Some(&token_path),
            false,
            None,
            1,
        )
        .await
        .expect("operator redeem must succeed via /redeem");

        let token = std::fs::read_to_string(&token_path).unwrap();
        assert_eq!(token.trim(), "op-abc-123");
        // chat-only response → no .creds or .seed written
        assert!(!tmp.path().join("agent.creds").exists());
        assert!(!tmp.path().join("agent.seed").exists());
    }

    /// The agent path mints NATS creds only — it has no HTTP bearer and
    /// must NOT write the `orchestrator` endpoint file (config-free client
    /// resolution is an operator concern). Regression guard so a future
    /// refactor doesn't wire persistence into the agent path.
    #[tokio::test]
    async fn agent_redeem_does_not_persist_endpoint() {
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
        run(
            &agent_invite_code(),
            &mock_server.uri(),
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            Some(&tmp.path().join("operator.token")),
            false,
            None,
            1,
        )
        .await
        .expect("agent redeem must succeed");

        assert!(
            !tmp.path().join("orchestrator").exists(),
            "agent redeem must not write the orchestrator endpoint file"
        );
    }

    /// Config-free onboarding: an operator redeem must persist the
    /// orchestrator HTTP address beside the token (`<token_dir>/orchestrator`)
    /// so `quorum run`/`status`/`rooms` can reach it later with no nsed.yaml.
    #[tokio::test]
    async fn redeem_operator_persists_orchestrator_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "op-abc-123",
                "name": "alice",
                "budget": 5.0,
            })))
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let token_path = tmp.path().join("operator.token");

        run(
            &operator_invite_code(),
            &mock_server.uri(),
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            Some(&token_path),
            false,
            None,
            1,
        )
        .await
        .expect("operator redeem must succeed");

        let endpoint = std::fs::read_to_string(tmp.path().join("orchestrator")).unwrap();
        assert_eq!(endpoint.trim(), mock_server.uri().trim_end_matches('/'));
    }

    /// Unified-code path: operator code with `capabilities=[chat,agent]`
    /// means the `/redeem` response carries `user_jwt` + `nats_url`.
    /// The CLI must write BOTH the bearer AND the `.creds`/`.seed`
    /// in that case so the operator can chat AND run an agent
    /// without a second tool.
    #[tokio::test]
    async fn redeem_dispatches_unified_code_writes_token_and_creds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "op-unified-xyz",
                "name": "alice",
                "user_jwt": "eyJ.unified.jwt",
                "nats_url": "nats://localhost:4222",
                "agent_id": "alice",
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let token_path = tmp.path().join("operator.token");
        let creds_path = tmp.path().join("agent.creds");
        let seed_path = tmp.path().join("agent.seed");

        run(
            &operator_invite_code(),
            &mock_server.uri(),
            Some(&seed_path),
            Some(&creds_path),
            Some(&token_path),
            false,
            None,
            1,
        )
        .await
        .expect("unified redeem must succeed");

        assert_eq!(
            std::fs::read_to_string(&token_path).unwrap().trim(),
            "op-unified-xyz"
        );
        let creds = std::fs::read_to_string(&creds_path).unwrap();
        assert!(
            creds.contains("eyJ.unified.jwt"),
            "creds blob must embed user_jwt"
        );
        let seed = std::fs::read_to_string(&seed_path).unwrap();
        assert!(
            seed.trim().starts_with("SU"),
            "seed file must contain SU-prefixed nkey: {seed}"
        );
    }

    /// Agent-code path (`aud=nsed-agent-redeem`) must keep using
    /// `/redeem-agent`. Regression guard so future refactors don't
    /// accidentally collapse the dispatch.
    #[tokio::test]
    async fn redeem_dispatches_agent_code_to_redeem_agent_endpoint() {
        use base64::Engine;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"bot-1","exp":9999999999,"aud":"nsed-agent-redeem"}"#);
        let agent_code = format!("{header}.{payload}.AAAA");

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/redeem-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_jwt": "eyJ.agent.jwt",
                "nats_url": "nats://localhost:4222",
                "agent_id": "bot-1",
            })))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redeem"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&mock_server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        run(
            &agent_code,
            &mock_server.uri(),
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            Some(&tmp.path().join("operator.token")),
            false,
            None,
            1,
        )
        .await
        .expect("agent redeem must succeed via /redeem-agent");
        // Agent path doesn't issue a bearer.
        assert!(!tmp.path().join("operator.token").exists());
    }

    /// Unknown audience must bail before any network call.
    #[tokio::test]
    async fn redeem_rejects_unknown_audience_without_network() {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"x","exp":9,"aud":"nsed-mystery-redeem"}"#);
        let unknown = format!("{header}.{payload}.AAAA");

        let tmp = tempfile::TempDir::new().unwrap();
        let err = run(
            &unknown,
            "http://orchestrator.invalid",
            Some(&tmp.path().join("agent.seed")),
            Some(&tmp.path().join("agent.creds")),
            Some(&tmp.path().join("operator.token")),
            false,
            None,
            1,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown audience"),
            "must mention unknown audience: {err}"
        );
    }
}
