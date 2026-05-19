//! End-to-end example: bootstrap an agent's NATS credentials via a
//! JWT invite code.
//!
//! Reference for SDK consumers who want to embed
//! [`redeem_invite_with_orchestrator`] in their own binary rather
//! than shell out to `quorum redeem`. The redemption path is
//! one-step: the helper generates the NKey locally, presents only
//! the public half to the orchestrator, and returns a fully-formed
//! `RegistrationResult` (`.creds` + `nats_url` + `keypair`).
//!
//! # How to run
//!
//! ```bash
//! export ORCH_URL="http://localhost:8080"
//! export INVITE_CODE="eyJhbGc..."
//! cargo run --example redeem_invite
//! ```

use anyhow::{Context, Result};
use quorum_rs::nats_utils::{
    NatsAuth, RedeemInviteError, connect_nats, redeem_invite_with_orchestrator_with_retry,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let orch_url =
        std::env::var("ORCH_URL").context("ORCH_URL must be set (e.g. http://localhost:8080)")?;
    let invite_code = std::env::var("INVITE_CODE")
        .context("INVITE_CODE must be set — the JWT the admin minted for you")?;

    tracing::info!(orch_url = %orch_url, "Redeeming invite code…");

    // Generate the NKey once and reuse it across retry attempts —
    // see `redeem_invite_with_orchestrator_with_retry` docs for why.
    let keypair = nkeys::KeyPair::new_user();

    // Typed RedeemInviteError keeps the caller's UX logic readable.
    // is_retryable() drives the retry helper — anything else, fall
    // through to a tailored message.
    let result = match redeem_invite_with_orchestrator_with_retry(
        &orch_url,
        &invite_code,
        &keypair,
        5,
    )
    .await
    {
        Ok(r) => r,
        Err(RedeemInviteError::Expired) => {
            anyhow::bail!("Invite expired. Ask the admin for a fresh code.");
        }
        Err(RedeemInviteError::Replayed) => {
            anyhow::bail!("Invite already redeemed. Each code is single-use.");
        }
        Err(RedeemInviteError::Revoked) => {
            anyhow::bail!("Admin revoked this invite.");
        }
        Err(RedeemInviteError::InvalidCode) => {
            anyhow::bail!("Invite is invalid — tampered, wrong audience, or wrong code type.");
        }
        Err(RedeemInviteError::NotConfigured) => {
            anyhow::bail!(
                "Orchestrator does not have invites configured. Ask the admin to set \
                 APP_INVITES__SIGNING_SECRET on the orchestrator."
            );
        }
        Err(e) => anyhow::bail!("Redeem failed: {e}"),
    };

    tracing::info!(
        agent_pub = %result.keypair.public_key(),
        nats_url = %result.nats_url,
        "Redeemed; connecting to NATS…"
    );

    let auth = NatsAuth {
        inline_creds: Some(result.creds),
        ..Default::default()
    };
    let _client = connect_nats(&result.nats_url, Some(&auth)).await?;

    tracing::info!("Connected to NATS. Hand the client off to your agent worker.");
    Ok(())
}
