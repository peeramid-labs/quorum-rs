//! End-to-end example: bootstrap an agent's NATS credentials via a
//! JWT invite code.
//!
//! This is the redemption path for 3rd-party agent operators — no
//! long-lived bearer token, no challenge-response gymnastics. The
//! admin minted a single-use, short-TTL code carrying the agent's
//! pre-shared `user_pub_key`; this program redeems it for a scoped
//! NATS User JWT and connects.
//!
//! # How to run
//!
//! 1. Generate an NKey for this agent (one-time):
//!
//!    ```bash
//!    cargo run --example redeem_invite -- --print-pubkey
//!    ```
//!
//!    Save the seed string somewhere safe (e.g. into
//!    `NSED_AGENT_NKEY_SEED`) and send the public key to the admin
//!    out-of-band.
//!
//! 2. Admin mints a code via `POST /admin/api/invites/agent` on the
//!    orchestrator. Admin shares the code (single-use, short-TTL).
//!
//! 3. Redeem and connect:
//!
//!    ```bash
//!    export NSED_AGENT_NKEY_SEED="SU..."     # from step 1
//!    export ORCH_URL="http://localhost:8080"
//!    export INVITE_CODE="eyJhbGc..."
//!    cargo run --example redeem_invite
//!    ```
//!
//! # What this example does NOT do
//!
//! - Persist the NKey seed for you. Real deployments hand seed
//!   management to `nsed init` / their own secret manager.
//! - Run the agent worker loop. After connecting, a real binary
//!   would hand the `async_nats::Client` to a worker; this example
//!   just prints a confirmation and exits.

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

    // --print-pubkey escape hatch: generate a fresh keypair, print
    // the seed (to stderr — keep it out of stdout pipes) and the
    // public key, then exit. Operator pastes the pubkey into their
    // admin's request and saves the seed for step 3.
    if std::env::args().any(|a| a == "--print-pubkey") {
        let kp = nkeys::KeyPair::new_user();
        let seed = kp.seed().context("KeyPair::seed")?;
        let pub_key = kp.public_key();
        eprintln!("# Save the seed below (e.g. into NSED_AGENT_NKEY_SEED).");
        eprintln!("# DO NOT share it with anyone — the admin only needs the public key.");
        eprintln!("NSED_AGENT_NKEY_SEED={seed}");
        println!("{pub_key}");
        return Ok(());
    }

    let orch_url =
        std::env::var("ORCH_URL").context("ORCH_URL must be set (e.g. http://localhost:8080)")?;
    let invite_code = std::env::var("INVITE_CODE")
        .context("INVITE_CODE must be set — the JWT the admin minted for you")?;
    let seed = std::env::var("NSED_AGENT_NKEY_SEED")
        .context("NSED_AGENT_NKEY_SEED must be set (run `--print-pubkey` to generate one)")?;
    let keypair = nkeys::KeyPair::from_seed(&seed)
        .map_err(|e| anyhow::anyhow!("Invalid NSED_AGENT_NKEY_SEED: {e}"))?;

    tracing::info!(
        orch_url = %orch_url,
        agent_pub = %keypair.public_key(),
        "Redeeming invite code…"
    );

    // The typed RedeemInviteError surface keeps the caller's UX
    // logic readable. is_retryable() drives the retry helper — for
    // anything else, fall through to a tailored message.
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
            anyhow::bail!("Invite already redeemed on another host. Each code is single-use.");
        }
        Err(RedeemInviteError::Revoked) => {
            anyhow::bail!("Admin revoked this invite.");
        }
        Err(RedeemInviteError::InvalidCode) => {
            anyhow::bail!(
                "Invite is invalid — wrong audience, tampered, or this is an \
                     operator-flow code (not an agent-flow code)."
            );
        }
        Err(RedeemInviteError::NotConfigured) => {
            anyhow::bail!(
                "Orchestrator does not have invites configured. Ask the admin to \
                     set APP_INVITES__SIGNING_SECRET on the orchestrator."
            );
        }
        Err(e) => {
            anyhow::bail!("Redeem failed: {e}");
        }
    };

    tracing::info!(
        agent_id = %result.keypair.public_key(),
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
