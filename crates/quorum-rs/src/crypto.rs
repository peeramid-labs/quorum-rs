//! Transparent cryptographic signing for NSED agents.
//!
//! - [`AgentKeyPair`] — ergonomic wrapper around Ed25519 for agent identity
//! - [`SigningHook`] — [`WorkerHook`] that wraps outbound payloads in [`AuditEnvelope`]
//!
//! Agent developers don't need to interact with crypto directly — the worker
//! builder installs [`SigningHook`] by default with an auto-generated keypair.

use crate::workers::WorkerHook;
use anyhow::Result;
use quorum_crypto_core::{AuditEnvelope, AuditSigner, signer::ed25519::Ed25519Signer};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// AgentKeyPair
// ---------------------------------------------------------------------------

/// Ergonomic wrapper around an Ed25519 signing key for agent identity.
///
/// Hides the [`AuditSigner`] trait — agent developers work with this struct
/// directly. The inner signer is always Ed25519 (matching NATS NKey scheme).
#[derive(Debug, Clone)]
pub struct AgentKeyPair {
    signer: Arc<Ed25519Signer>,
}

impl AgentKeyPair {
    /// Generate a new random keypair.
    pub fn generate() -> Self {
        Self {
            signer: Arc::new(Ed25519Signer::generate()),
        }
    }

    /// Create from a 32-byte seed (deterministic — same seed → same key).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signer: Arc::new(Ed25519Signer::from_seed(seed)),
        }
    }

    /// Create from the `NSED_AGENT_SEED` environment variable (hex-encoded 32 bytes).
    ///
    /// Returns `None` if the env var is missing or invalid.
    pub fn from_env(var_name: &str) -> Option<Self> {
        let hex_seed = std::env::var(var_name).ok()?;
        let bytes = hex::decode(hex_seed.trim()).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        Some(Self::from_seed(&seed))
    }

    /// Load from a config reference — `${VAR}`, `file:<path>`, or a literal —
    /// resolved by [`crate::config::resolve_env_token`], then read as a
    /// hex-encoded 32-byte seed.
    ///
    /// The config holds the *reference*; the seed itself stays in an env var or a
    /// key file so it never lands in a config that gets committed, copied, or
    /// serialized back out. A literal is accepted because the resolver allows one,
    /// but writing a seed inline puts a private key wherever that file goes.
    ///
    /// `None` when the reference resolves to nothing or does not decode to 32
    /// bytes — the caller decides whether an agent without a key may run.
    pub fn from_config_ref(raw: &str) -> Option<Self> {
        let resolved = crate::config::resolve_env_token("signing_key", raw);
        let bytes = hex::decode(resolved.trim()).ok()?;
        let seed: [u8; 32] = bytes.try_into().ok()?;
        Some(Self::from_seed(&seed))
    }

    /// Get the public key as hex string for display/logging.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signer.public_key_bytes())
    }

    /// Access the inner signer (for advanced use / interop with nsed-crypto).
    pub fn signer(&self) -> &Arc<Ed25519Signer> {
        &self.signer
    }

    /// Get an `Arc<dyn AuditSigner>` for use with envelope signing.
    pub fn as_audit_signer(&self) -> Arc<dyn AuditSigner> {
        self.signer.clone()
    }
}

/// Resolve a configured `signing_key` reference to the signer it names.
///
/// Returns a signer rather than key material on purpose. `${VAR}`, `file:<path>`
/// and a literal all name a *stored secret*, which this reads into a software
/// Ed25519 signer — but a token, a TPM or a secure enclave never surrenders its
/// private key, it signs on request. Handing back an [`AuditSigner`] is what lets
/// such a backend be added as another arm here without changing any caller.
///
/// `None` when the reference resolves to nothing usable. The caller decides
/// whether an agent without a signer may run; this does not invent one, because an
/// agent signing under a key nobody expects is worse than one that will not start.
pub fn signer_from_config_ref(raw: &str) -> Option<Arc<dyn AuditSigner>> {
    AgentKeyPair::from_config_ref(raw).map(|kp| kp.as_audit_signer())
}

pub use quorum_crypto_core::{AuditRecord, TrailSummary, read_audit_record};

/// The subject family carrying one job's audit trail.
///
/// A wildcard, because the trail spans every action a seat published — a reader
/// asks for the job, not for each action it happens to have taken.
pub fn job_trail_subject(subject_prefix: &str, job_id: &str) -> String {
    format!("{subject_prefix}.{job_id}.audit.>")
}

/// Read a job's audit trail until it falls quiet, verifying every record.
///
/// Returns when no record arrives for `idle`. A trail has no terminator — the
/// agents that write it do not announce that they are finished — so a reader
/// waits out a silence rather than looking for an end.
///
/// Verification failures are counted, never returned as errors: one unsound
/// record must not stop the reader from seeing the rest of the trail, which is
/// the whole reason it is read.
pub async fn verify_job_trail(
    nats: &async_nats::Client,
    subject_prefix: &str,
    job_id: &str,
    registry: &quorum_crypto_core::VerifierRegistry,
    idle: std::time::Duration,
) -> anyhow::Result<TrailSummary> {
    use futures::StreamExt;
    let subject = job_trail_subject(subject_prefix, job_id);
    let mut sub = nats
        .subscribe(subject.clone())
        .await
        .map_err(|e| anyhow::anyhow!("subscribe {subject}: {e}"))?;
    let mut summary = TrailSummary::default();
    while let Ok(Some(msg)) = tokio::time::timeout(idle, sub.next()).await {
        summary.record(read_audit_record(&msg.payload, registry));
    }
    Ok(summary)
}

/// The audit subject that mirrors a working result subject.
///
/// An agent publishes results on `{prefix}.{job}.result.{round}.{agent}.{action}`.
/// The signed copy goes to `{prefix}.{job}.audit.{action}`, matching the shape the
/// orchestrator already uses for its own trail, so one consumer pattern reads both.
///
/// `None` for anything that is not a result subject — control-plane traffic has no
/// audit counterpart, and inventing one would put heartbeats in the trail.
pub fn audit_subject_for(working: &str) -> Option<String> {
    let parts: Vec<&str> = working.split('.').collect();
    // prefix . job . "result" . round . agent . action
    if parts.len() != 6 || parts[2] != "result" {
        return None;
    }
    let action = parts[5];
    if action == "event" || action.is_empty() {
        return None;
    }
    Some(format!("{}.{}.audit.{}", parts[0], parts[1], action))
}

// ---------------------------------------------------------------------------
// Candidate attestation
// ---------------------------------------------------------------------------

/// Hex SHA-256 of an artifact, as published.
///
/// Over the **published bytes**, so a party that cannot read the artifact can
/// still check a claim about it by hashing what it relayed. That keeps working if
/// the payload is later encrypted.
///
/// Deliberately not the dylib's FNV hunk id: that is content addressing among
/// cooperating parties and is trivially collidable, so it cannot carry a claim
/// about *which* artifact was meant.
pub fn artifact_digest(published: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(published))
}

/// A claim about a specific artifact.
///
/// A deliberation is settled by parties who did not read it: the orchestrator ranks on
/// scores and promotes on ids. That only holds together if every claim names the
/// artifact it is about — otherwise a seat can be scored on one proposal and
/// attest a commit derived from another, and nothing in the chain notices.
///
/// Generic because the shape recurs: a [`Candidate`] is a claim about the proposal
/// it was judged on, an evaluation is a claim about the proposal it scored. Both
/// need the same binding, and the orchestrator checks it the same way for both — the
/// artifact digest is what joins separate claims into one chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct About<T> {
    /// Hex SHA-256 of the artifact this claim is about, per [`artifact_digest`].
    pub artifact: String,
    /// The claim itself.
    pub claim: T,
}

impl<T> About<T> {
    /// Bind a claim to the artifact bytes it is about.
    pub fn this(claim: T, artifact: &[u8]) -> Self {
        Self {
            artifact: artifact_digest(artifact),
            claim,
        }
    }

    /// Bind a claim to the absence of an artifact — a seat that skipped a round,
    /// an evaluation that scored nothing.
    ///
    /// Not a special case. The empty artifact has a digest like any other, so a
    /// skip is a claim *about* it rather than a claim about no artifact, and every
    /// claim keeps one shape.
    ///
    /// This is what separates a declared skip from silence. A seat that signs this
    /// has said "nothing, for this slot" and can be held to it; a seat that
    /// publishes nothing has said nothing at all, and no signature exists to
    /// distinguish it from one that was never asked. The orchestrator needs both, and
    /// they are not the same fact.
    pub fn nothing(claim: T) -> Self {
        Self::this(claim, b"")
    }

    /// Whether this claim is about the given artifact bytes.
    ///
    /// The check the orchestrator runs to join claims: an evaluation and a candidate
    /// that agree here are about the same thing, and ones that do not are not
    /// comparable — regardless of what either says.
    pub fn is_about(&self, artifact: &[u8]) -> bool {
        self.artifact == artifact_digest(artifact)
    }

    /// True for a declared skip — see [`About::nothing`] for why that is a claim
    /// rather than an absence.
    pub fn is_about_nothing(&self) -> bool {
        self.is_about(b"")
    }
}

/// A seat's claim that a commit is its candidate for a round.
///
/// This is what lets a deliberation be settled by a party that never reads it.
/// Ranking is a function of scores, and promotion is a function of ids, so a
/// orchestrator holding candidates can name the winning commit without opening the
/// repository — the signature binds a commit to the seat that produced it, and
/// that is the whole of what promotion needs to be checkable.
///
/// Wrap it in [`About`] to bind it to the proposal it was judged on. On its own it
/// says which commit a seat *claims*, and nothing ties that to what the evaluators
/// read.
///
/// **Only the commit travels.** The repository follows from the thread and the
/// branch from the job and the seat (`job/{job}/{agent}`), so anyone already
/// routing the job reconstructs both; anyone who cannot is not entitled to fetch
/// it. Carrying a repo url here would re-state a fact the receiver already holds
/// and turn a promotion record into a distribution channel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    /// The job this candidate was produced for.
    pub job: String,
    /// The round within that job.
    pub round: u32,
    /// The seat that produced it — the `{agent}` of its branch.
    pub agent: String,
    /// Hex commit sha.
    ///
    /// A string, not a number. A signed payload is verified by re-serializing an
    /// untyped parse, and a 40-hex-digit value has no exact numeric form there:
    /// as an integer it would come back as a float and read as tampered.
    pub commit: String,
}

impl Candidate {
    /// Read the candidate a seat reported, as the dylib emits it under
    /// `hook_state.pd_candidate`.
    ///
    /// `None` when the reported value is not a candidate, so a missing or
    /// malformed one yields nothing rather than a claim about nothing.
    pub fn reported(reported: &serde_json::Value) -> Option<Self> {
        // Deserialized whole so a missing or wrong-typed field refuses the lot
        // rather than being read one accessor at a time.
        let c: Self = serde_json::from_value(reported.clone()).ok()?;
        (!c.commit.is_empty()).then_some(c)
    }
}

// ---------------------------------------------------------------------------
// SigningHook
// ---------------------------------------------------------------------------

/// [`WorkerHook`] that wraps outbound NATS payloads in signed [`AuditEnvelope`]s.
///
/// Installed automatically by the worker builder. Agent developers don't
/// interact with this directly.
///
/// The hook:
/// 1. Deserializes the raw payload as `serde_json::Value`
/// 2. Extracts `agent_id` from the NATS subject
/// 3. Wraps in `AuditEnvelope::signed()` with the agent's keypair
/// 4. Replaces the payload bytes with the serialized envelope
///
/// If signing fails (shouldn't happen with Ed25519), the original payload
/// is passed through unchanged with a warning log.
#[derive(Debug)]
pub struct SigningHook {
    signer: Arc<dyn AuditSigner>,
    agent_id: String,
}

impl SigningHook {
    /// Create a new signing hook for the given agent.
    pub fn new(keypair: AgentKeyPair, agent_id: String) -> Self {
        Self::with_signer(keypair.as_audit_signer(), agent_id)
    }

    /// Create a hook over any [`AuditSigner`].
    ///
    /// The hook needs a thing that signs, not a key it can read. A token, a TPM or
    /// a secure enclave never surrenders its private key — it signs on request —
    /// so taking the signer rather than the keypair is what lets those back the
    /// same hook later without changing anything around it.
    pub fn with_signer(signer: Arc<dyn AuditSigner>, agent_id: String) -> Self {
        Self { signer, agent_id }
    }

    /// Extract the audit-relevant subject type from a NATS subject.
    ///
    /// Only deliberation content subjects are signed:
    /// - `nsed.{session}.result.{round}.{agent_id}.propose` → `Some("proposal")`
    /// - `nsed.{session}.result.{round}.{agent_id}.evaluate` → `Some("evaluation")`
    /// - Everything else (heartbeats, manifest ACKs, control messages) → `None`
    ///
    /// Returns `None` for subjects that should pass through unsigned.
    pub(crate) fn audit_subject_type(subject: &str) -> Option<&'static str> {
        let last = subject.rsplit('.').next().unwrap_or("");
        match last {
            "propose" => Some("proposal"),
            "evaluate" => Some("evaluation"),
            _ => None,
        }
    }
}

/// [`WorkerHook`] that publishes a *signed copy* of each result to an audit
/// subject, leaving the working payload exactly as the receiver expects it.
///
/// The counterpart to [`SigningHook`], and the one to prefer: that hook replaces
/// the payload with an envelope, which ties signing to delivery — a receiver that
/// parses the subject into a `Proposal` cannot read an envelope, so the message is
/// lost. Copying to a parallel subject is the shape the orchestrator's own audit
/// trail already uses, so signing can be switched on without a reader on the far
/// side agreeing first.
#[derive(Debug)]
pub struct AuditTrailHook {
    signer: Arc<dyn AuditSigner>,
    agent_id: String,
}

impl AuditTrailHook {
    /// Create a hook that signs a copy of every result under `agent_id`.
    pub fn new(signer: Arc<dyn AuditSigner>, agent_id: String) -> Self {
        Self { signer, agent_id }
    }
}

#[async_trait::async_trait]
impl WorkerHook for AuditTrailHook {
    async fn audit_copies(&self, subject: &str, payload: &[u8]) -> Vec<(String, Vec<u8>)> {
        let Some(audit_subject) = audit_subject_for(subject) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
            tracing::warn!(agent_id = %self.agent_id, "audit trail: payload is not JSON, not recorded");
            return Vec::new();
        };
        let subject_type = SigningHook::audit_subject_type(subject).unwrap_or("result");
        match AuditEnvelope::signed(value, subject_type, &self.agent_id, &*self.signer).await {
            Ok(envelope) => match serde_json::to_vec(&envelope) {
                Ok(bytes) => vec![(audit_subject, bytes)],
                Err(e) => {
                    tracing::warn!(agent_id = %self.agent_id, error = %e, "audit trail: envelope did not serialize");
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(agent_id = %self.agent_id, error = %e, "audit trail: signing failed");
                Vec::new()
            }
        }
    }
}

#[async_trait::async_trait]
impl WorkerHook for SigningHook {
    async fn before_publish(&self, subject: &str, payload: &mut Vec<u8>) -> Result<()> {
        // Only sign audit-relevant subjects (proposals, evaluations).
        // Control-plane messages (manifest ACKs, heartbeats) pass through unsigned.
        let Some(subject_type) = Self::audit_subject_type(subject) else {
            return Ok(());
        };

        // Parse payload as JSON Value for envelope wrapping
        let value: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    error = %e,
                    "SigningHook: payload is not JSON, passing through unsigned"
                );
                return Ok(());
            }
        };

        // Sign into an AuditEnvelope
        let signer = self.signer.clone();
        match AuditEnvelope::signed(value, subject_type, &self.agent_id, &*signer).await {
            Ok(envelope) => {
                // Replace payload with the serialized envelope
                match serde_json::to_vec(&envelope) {
                    Ok(signed_bytes) => {
                        *payload = signed_bytes;
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            error = %e,
                            "SigningHook: failed to serialize envelope, passing unsigned"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    error = %e,
                    "SigningHook: signing failed, passing unsigned"
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// A signing key is configured by reference so the seed lives in an env var or
    /// a key file, never in the config itself. The derived public key is what other
    /// parties see, and it must follow the seed rather than be declared beside it.
    #[test]
    fn a_signing_key_reference_resolves_to_a_stable_derived_identity() {
        let seed_hex = "11".repeat(32);
        unsafe { std::env::set_var("SDK_TEST_SIGNING_SEED", &seed_hex) };

        let from_ref = super::AgentKeyPair::from_config_ref("${SDK_TEST_SIGNING_SEED}")
            .expect("an env reference resolves");
        let from_literal =
            super::AgentKeyPair::from_config_ref(&seed_hex).expect("a literal resolves");
        assert_eq!(
            from_ref.public_key_hex(),
            from_literal.public_key_hex(),
            "the same seed derives the same identity however it was referenced"
        );
        assert_eq!(from_ref.public_key_hex().len(), 64);
    }

    /// A reference that resolves to nothing, or to something that is not a 32-byte
    /// seed, must not silently become a different identity — an agent signing with
    /// a key nobody expects is worse than one that fails to start.
    #[test]
    fn an_unusable_signing_key_reference_yields_no_identity() {
        assert!(super::AgentKeyPair::from_config_ref("${SDK_TEST_SIGNING_SEED_ABSENT}").is_none());
        assert!(super::AgentKeyPair::from_config_ref("not-hex").is_none());
        assert!(
            super::AgentKeyPair::from_config_ref(&"aa".repeat(16)).is_none(),
            "a 16-byte value is not a seed"
        );
    }

    use super::*;

    // ---- AgentKeyPair ----

    #[test]
    fn an_audit_subject_mirrors_the_result_it_copies() {
        assert_eq!(
            super::audit_subject_for("nsed.job1.result.0.agent-a.propose").as_deref(),
            Some("nsed.job1.audit.propose")
        );
        assert_eq!(
            super::audit_subject_for("nsed.job1.result.2.agent-b.evaluate").as_deref(),
            Some("nsed.job1.audit.evaluate")
        );
        // Control-plane traffic has no audit counterpart.
        for not_a_result in [
            "sphera.agent.heartbeat.agent-a",
            "nsed.job1.result.event.round_complete",
            "nsed.job1.task.agent-a.propose",
            "nsed.job1.result.0.agent-a",
        ] {
            assert!(
                super::audit_subject_for(not_a_result).is_none(),
                "{not_a_result} must not derive an audit subject"
            );
        }
    }

    /// A signature covers bytes, not meaning. A signer that serializes a typed
    /// struct writes fields in declaration order; a reader that verifies through
    /// `serde_json::Value` re-serializes them in sorted order. Same content, other
    /// bytes — so a record nobody touched can fail to verify.
    #[tokio::test]
    async fn a_record_signed_as_a_struct_still_verifies_when_read_as_json() {
        use quorum_crypto_core::{AuditEnvelope, VerifierRegistry};
        #[derive(serde::Serialize)]
        struct Typed {
            zeta: String,
            alpha: String,
        }
        let signer = super::AgentKeyPair::generate().as_audit_signer();
        let envelope = AuditEnvelope::signed(
            Typed {
                zeta: "z".into(),
                alpha: "a".into(),
            },
            "proposal",
            "agent-a",
            &*signer,
        )
        .await
        .unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();

        let registry = VerifierRegistry::with_defaults();
        assert_eq!(
            super::read_audit_record(&bytes, &registry).unwrap(),
            super::AuditRecord::Verified {
                agent_id: "agent-a".into(),
                signatures: 1
            },
            "a reader must not report an untouched record as tampered"
        );
    }

    /// The bar the claim discipline sets: a verifier must reject a tampered
    /// message. This drives the real path — the hook signs a result, the reader
    /// verifies it, and the same record with one byte of payload changed is
    /// reported as tampered rather than passing or being discarded as unparseable.
    #[tokio::test]
    async fn a_reader_verifies_a_recorded_result_and_rejects_a_tampered_one() {
        use crate::agents::Proposal;
        use crate::workers::WorkerHook;
        use quorum_crypto_core::VerifierRegistry;

        let hook = super::AuditTrailHook::new(
            super::AgentKeyPair::generate().as_audit_signer(),
            "agent-a".into(),
        );
        let proposal = serde_json::to_vec(&Proposal {
            thought_process: "considered".into(),
            content: "the answer".into(),
            ..Default::default()
        })
        .unwrap();
        let copies = hook
            .audit_copies("nsed.job1.result.0.agent-a.propose", &proposal)
            .await;
        let record = &copies[0].1;
        let registry = VerifierRegistry::with_defaults();

        assert_eq!(
            super::read_audit_record(record, &registry).unwrap(),
            super::AuditRecord::Verified {
                agent_id: "agent-a".to_string(),
                signatures: 1,
            },
            "a record straight off the trail verifies"
        );

        // Alter the payload after signing — the case the trail exists to catch.
        let mut tampered: serde_json::Value = serde_json::from_slice(record).unwrap();
        tampered["payload"]["content"] = serde_json::json!("a different answer");
        let tampered = serde_json::to_vec(&tampered).unwrap();
        assert_eq!(
            super::read_audit_record(&tampered, &registry).unwrap(),
            super::AuditRecord::Tampered {
                agent_id: "agent-a".to_string(),
            },
            "an altered payload does not verify"
        );

        // Stripping the signatures is not a way to look clean either.
        let mut stripped: serde_json::Value = serde_json::from_slice(record).unwrap();
        stripped["signatures"] = serde_json::json!([]);
        stripped["signature"] = serde_json::json!("");
        let stripped = serde_json::to_vec(&stripped).unwrap();
        assert_eq!(
            super::read_audit_record(&stripped, &registry).unwrap(),
            super::AuditRecord::Unsigned {
                agent_id: "agent-a".to_string(),
            },
            "a record with its signatures removed reads as unsigned, not as sound"
        );

        assert!(
            super::read_audit_record(b"not a record", &registry).is_err(),
            "bytes that are not a record are an error, not a verdict"
        );
    }

    /// The audit hook records a signed copy and leaves the working payload alone,
    /// so the receiver still parses the `Proposal` it expects. This is the property
    /// that lets signing be switched on without a reader agreeing first.
    #[tokio::test]
    async fn an_audit_copy_is_signed_while_the_working_payload_is_untouched() {
        use crate::agents::Proposal;
        use crate::workers::WorkerHook;

        let hook = super::AuditTrailHook::new(
            super::AgentKeyPair::generate().as_audit_signer(),
            "agent-a".into(),
        );
        let proposal = serde_json::to_vec(&Proposal {
            thought_process: "considered".into(),
            content: "the answer".into(),
            ..Default::default()
        })
        .unwrap();
        let subject = "nsed.job1.result.0.agent-a.propose";

        let mut working = proposal.clone();
        hook.before_publish(subject, &mut working).await.unwrap();
        assert_eq!(working, proposal, "the working payload is not rewritten");
        assert!(
            serde_json::from_slice::<Proposal>(&working).is_ok(),
            "the receiver still parses what it expects"
        );

        let copies = hook.audit_copies(subject, &proposal).await;
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].0, "nsed.job1.audit.propose");
        let envelope: serde_json::Value = serde_json::from_slice(&copies[0].1).unwrap();
        assert_eq!(envelope["agent_id"], "agent-a");
        assert!(
            envelope["signatures"]
                .as_array()
                .is_some_and(|s| !s.is_empty()),
            "the copy carries a signature: {envelope}"
        );

        assert!(
            hook.audit_copies("sphera.agent.heartbeat.agent-a", &proposal)
                .await
                .is_empty(),
            "control-plane traffic is not recorded"
        );
    }

    /// Signing rewrites a proposal into an envelope, and nothing on the receiving
    /// side unwraps one — the orchestrator parses that subject straight into a
    /// `Proposal`, which an envelope cannot satisfy. Installing this hook therefore
    /// changes the wire contract, and must stay an explicit choice rather than
    /// something a config key switches on. This test is the reason, kept executable.
    #[tokio::test]
    async fn signing_a_proposal_replaces_it_with_something_no_reader_parses() {
        use crate::agents::Proposal;
        use crate::workers::WorkerHook;

        let hook = super::SigningHook::new(super::AgentKeyPair::generate(), "agent-a".into());
        let proposal = serde_json::to_vec(&Proposal {
            thought_process: "considered".into(),
            content: "the answer".into(),
            ..Default::default()
        })
        .unwrap();

        let mut payload = proposal.clone();
        hook.before_publish("nsed.job1.result.0.agent-a.propose", &mut payload)
            .await
            .unwrap();
        assert_ne!(payload, proposal, "the payload is replaced by an envelope");
        assert!(
            serde_json::from_slice::<Proposal>(&payload).is_err(),
            "an envelope does not parse as the Proposal the receiver expects"
        );

        // A control-plane subject is left alone, so this is scoped, not universal.
        let mut heartbeat = proposal.clone();
        hook.before_publish("sphera.agent.heartbeat.agent-a", &mut heartbeat)
            .await
            .unwrap();
        assert_eq!(heartbeat, proposal, "non-audit subjects pass through");
    }

    #[test]
    fn keypair_generate_produces_unique_keys() {
        let a = AgentKeyPair::generate();
        let b = AgentKeyPair::generate();
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_from_seed_is_deterministic() {
        let seed = [42u8; 32];
        let a = AgentKeyPair::from_seed(&seed);
        let b = AgentKeyPair::from_seed(&seed);
        assert_eq!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_from_seed_different_seeds_differ() {
        let a = AgentKeyPair::from_seed(&[1u8; 32]);
        let b = AgentKeyPair::from_seed(&[2u8; 32]);
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }

    #[test]
    fn keypair_public_key_hex_is_64_chars() {
        let kp = AgentKeyPair::generate();
        assert_eq!(kp.public_key_hex().len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn keypair_from_env_missing_returns_none() {
        assert!(AgentKeyPair::from_env("NSED_TEST_NONEXISTENT_SEED_VAR").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_invalid_hex_returns_none() {
        // SAFETY: test-only env var manipulation, tests run single-threaded
        unsafe { std::env::set_var("NSED_TEST_BAD_SEED", "not-hex") };
        assert!(AgentKeyPair::from_env("NSED_TEST_BAD_SEED").is_none());
        unsafe { std::env::remove_var("NSED_TEST_BAD_SEED") };
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_wrong_length_returns_none() {
        unsafe { std::env::set_var("NSED_TEST_SHORT_SEED", "abcd1234") };
        assert!(AgentKeyPair::from_env("NSED_TEST_SHORT_SEED").is_none());
        unsafe { std::env::remove_var("NSED_TEST_SHORT_SEED") };
    }

    #[test]
    #[serial_test::serial]
    fn keypair_from_env_valid_works() {
        let seed = [99u8; 32];
        let hex_seed = hex::encode(seed);
        unsafe { std::env::set_var("NSED_TEST_VALID_SEED", &hex_seed) };
        let kp = AgentKeyPair::from_env("NSED_TEST_VALID_SEED").unwrap();
        let expected = AgentKeyPair::from_seed(&seed);
        assert_eq!(kp.public_key_hex(), expected.public_key_hex());
        unsafe { std::env::remove_var("NSED_TEST_VALID_SEED") };
    }

    #[test]
    fn keypair_as_audit_signer_returns_ed25519() {
        let kp = AgentKeyPair::generate();
        let signer = kp.as_audit_signer();
        assert_eq!(signer.algorithm(), "ed25519");
    }

    // ---- SigningHook ----

    #[test]
    fn audit_subject_type_extracts_proposal() {
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.1.agent-1.propose"),
            Some("proposal")
        );
    }

    #[test]
    fn audit_subject_type_extracts_evaluation() {
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.1.agent-1.evaluate"),
            Some("evaluation")
        );
    }

    #[test]
    fn audit_subject_type_returns_none_for_non_audit() {
        // Control-plane subjects should not be signed
        assert_eq!(
            SigningHook::audit_subject_type("nsed.abc.result.something"),
            None
        );
        assert_eq!(
            SigningHook::audit_subject_type("sphera.jobs.ack.job1.agent1"),
            None
        );
    }

    #[test]
    fn audit_subject_type_handles_empty() {
        assert_eq!(SigningHook::audit_subject_type(""), None);
    }

    #[tokio::test]
    async fn signing_hook_skips_non_audit_subjects() {
        let kp = AgentKeyPair::generate();
        let hook = SigningHook::new(kp, "agent".to_string());

        let original = serde_json::json!({"manifest": true});
        let mut payload = serde_json::to_vec(&original).unwrap();
        let original_bytes = payload.clone();

        // Manifest ACK subject — should pass through unsigned
        hook.before_publish("sphera.jobs.ack.job1.agent1", &mut payload)
            .await
            .unwrap();

        assert_eq!(
            payload, original_bytes,
            "Non-audit subject should not be wrapped"
        );
    }

    #[tokio::test]
    async fn signing_hook_wraps_json_in_envelope() {
        let kp = AgentKeyPair::from_seed(&[1u8; 32]);
        let hook = SigningHook::new(kp.clone(), "test-agent".to_string());

        let original = serde_json::json!({"content": "hello", "thought_process": "thinking"});
        let mut payload = serde_json::to_vec(&original).unwrap();

        hook.before_publish("nsed.session.result.1.test-agent.propose", &mut payload)
            .await
            .unwrap();

        // Payload should now be an AuditEnvelope
        let envelope: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&payload).unwrap();

        assert_eq!(envelope.agent_id(), "test-agent");
        assert_eq!(envelope.subject(), "proposal");
        assert_eq!(envelope.payload()["content"], "hello");
        assert_eq!(envelope.signature_count(), 1);
        assert!(envelope.has_role(&quorum_crypto_core::envelope::SignerRole::Author));
    }

    #[tokio::test]
    async fn signing_hook_envelope_verifies() {
        let kp = AgentKeyPair::from_seed(&[2u8; 32]);
        let hook = SigningHook::new(kp.clone(), "verify-agent".to_string());

        let mut payload = serde_json::to_vec(&serde_json::json!({"score": 8.5})).unwrap();

        hook.before_publish("nsed.session.result.1.verify-agent.evaluate", &mut payload)
            .await
            .unwrap();

        let mut envelope: AuditEnvelope<serde_json::Value> =
            serde_json::from_slice(&payload).unwrap();

        let registry = quorum_crypto_core::VerifierRegistry::with_defaults();
        assert!(envelope.verify_chain(&registry).unwrap());
    }

    #[tokio::test]
    async fn signing_hook_passes_through_non_json() {
        let kp = AgentKeyPair::generate();
        let hook = SigningHook::new(kp, "agent".to_string());

        let mut payload = b"not json".to_vec();
        let original = payload.clone();

        hook.before_publish("nsed.session.result.1.agent.propose", &mut payload)
            .await
            .unwrap();

        // Non-JSON payload should pass through unchanged
        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn signing_hook_deterministic_with_same_seed() {
        let seed = [3u8; 32];
        let hook1 = SigningHook::new(AgentKeyPair::from_seed(&seed), "agent".to_string());
        let hook2 = SigningHook::new(AgentKeyPair::from_seed(&seed), "agent".to_string());

        let json = serde_json::json!({"test": true});
        let mut p1 = serde_json::to_vec(&json).unwrap();
        let mut p2 = serde_json::to_vec(&json).unwrap();

        hook1
            .before_publish("nsed.s.result.1.agent.propose", &mut p1)
            .await
            .unwrap();
        hook2
            .before_publish("nsed.s.result.1.agent.propose", &mut p2)
            .await
            .unwrap();

        // Both should produce valid envelopes with the same public key
        let env1: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p1).unwrap();
        let env2: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p2).unwrap();

        assert_eq!(
            env1.signatures()[0].public_key,
            env2.signatures()[0].public_key
        );
    }

    #[tokio::test]
    async fn signing_hook_different_subjects_produce_different_envelopes() {
        let kp = AgentKeyPair::from_seed(&[4u8; 32]);
        let hook = SigningHook::new(kp, "agent".to_string());

        let json = serde_json::json!({"data": 1});
        let mut p1 = serde_json::to_vec(&json).unwrap();
        let mut p2 = serde_json::to_vec(&json).unwrap();

        hook.before_publish("nsed.s.result.1.agent.propose", &mut p1)
            .await
            .unwrap();
        hook.before_publish("nsed.s.result.1.agent.evaluate", &mut p2)
            .await
            .unwrap();

        let env1: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p1).unwrap();
        let env2: AuditEnvelope<serde_json::Value> = serde_json::from_slice(&p2).unwrap();

        assert_eq!(env1.subject(), "proposal");
        assert_eq!(env2.subject(), "evaluation");
        // Different subjects → different signatures
        assert_ne!(
            env1.signatures()[0].signature,
            env2.signatures()[0].signature
        );
    }

    /// The orchestrator must be able to settle a deliberation from ids alone.
    ///
    /// The attestation binds a commit to the seat that produced it, so the orchestrator
    /// that never opens the repository can still tell whose candidate it is
    /// promoting. This asserts the whole path the orchestrator sees — sign, wire,
    /// untyped parse, verify — because verification re-serializes an untyped
    /// parse, and a payload that does not survive that reads as tampered.
    #[tokio::test]
    async fn a_candidate_attestation_binds_a_commit_to_its_seat_without_carrying_the_answer() {
        use quorum_crypto_core::{AuditEnvelope, VerifierRegistry};

        let kp = super::AgentKeyPair::generate();
        let signer = kp.as_audit_signer();
        let registry = VerifierRegistry::with_defaults();

        let candidate = super::About::this(
            super::Candidate {
                job: "thread-t1_jobAAAA".into(),
                round: 2,
                agent: "Reviewer".into(),
                commit: "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2".into(),
            },
            br#"{"rationale":"why","ops":[]}"#,
        );

        let env = AuditEnvelope::signed(&candidate, "candidate", "Reviewer", signer.as_ref())
            .await
            .expect("a seat signs its candidate");
        let wire = serde_json::to_vec(&env).expect("the attestation serializes");

        // What the orchestrator actually holds: bytes, parsed without the author's types.
        let mut read: AuditEnvelope<serde_json::Value> =
            serde_json::from_slice(&wire).expect("the orchestrator parses it untyped");
        assert!(
            read.verify_chain(&registry).expect("verification runs"),
            "an untouched attestation verifies"
        );

        let payload = read.payload().clone();
        let mut keys: Vec<&str> = payload["claim"]
            .as_object()
            .expect("the claim is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["agent", "commit", "job", "round"],
            "a candidate names a commit and its seat, nothing else: {payload}"
        );
        assert!(
            payload["artifact"].as_str().is_some_and(|a| a.len() == 64),
            "and is bound to the artifact it is about: {payload}"
        );

        // The repository and branch are derived, not carried: `job/{job}/{agent}`
        // and the thread's repo. Adding them here would make a promotion record
        // into a way to hand out the answer.
        let as_text = serde_json::to_string(&payload).unwrap();
        for leaked in ["repo", "://", "content", "thought_process"] {
            assert!(
                !as_text.contains(leaked),
                "an attestation must not carry {leaked}: {as_text}"
            );
        }

        // Tampering with the commit is the attack this exists to catch: the orchestrator
        // pointed at a commit its signer never claimed.
        let mut forged: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        forged["payload"]["commit"] = serde_json::json!("0000000000000000000000000000000000000000");
        let mut forged: AuditEnvelope<serde_json::Value> =
            serde_json::from_value(forged).expect("the forgery still parses");
        assert!(
            !forged.verify_chain(&registry).unwrap_or(false),
            "a rewritten commit does not verify"
        );
    }

    /// Every way a reported candidate can be malformed yields nothing.
    ///
    /// `Candidate::reported` is the boundary between a payload this crate did not
    /// produce and a claim it will sign. A partial parse here would mean signing a
    /// claim whose slot or seat came from somewhere other than the seat that
    /// reported it, so each missing or wrong-typed field must refuse rather than
    /// default.
    #[test]
    fn a_malformed_candidate_reads_as_nothing() {
        let full = serde_json::json!({
            "job": "j", "round": 1, "agent": "a", "commit": "abc"
        });
        assert!(
            super::Candidate::reported(&full).is_some(),
            "precondition: the complete shape reads"
        );

        for (why, value) in [
            ("not an object", serde_json::json!("candidate")),
            (
                "no job",
                serde_json::json!({"round": 1, "agent": "a", "commit": "abc"}),
            ),
            (
                "no round",
                serde_json::json!({"job": "j", "agent": "a", "commit": "abc"}),
            ),
            (
                "no agent",
                serde_json::json!({"job": "j", "round": 1, "commit": "abc"}),
            ),
            (
                "no commit",
                serde_json::json!({"job": "j", "round": 1, "agent": "a"}),
            ),
            (
                "round is not a number",
                serde_json::json!({"job": "j", "round": "1", "agent": "a", "commit": "abc"}),
            ),
            (
                "round exceeds u32",
                serde_json::json!({"job": "j", "round": 4_294_967_296u64, "agent": "a", "commit": "abc"}),
            ),
            (
                "job is not a string",
                serde_json::json!({"job": 1, "round": 1, "agent": "a", "commit": "abc"}),
            ),
            (
                "agent is not a string",
                serde_json::json!({"job": "j", "round": 1, "agent": 1, "commit": "abc"}),
            ),
            (
                "commit is not a string",
                serde_json::json!({"job": "j", "round": 1, "agent": "a", "commit": 1}),
            ),
        ] {
            assert!(
                super::Candidate::reported(&value).is_none(),
                "a candidate with {why} must not bind: {value}"
            );
        }
    }

    /// The chain the orchestrator walks: a score and a commit are comparable only when
    /// they are about the same artifact.
    ///
    /// This is the half that makes the candidate binding worth anything. Ranking
    /// is per seat, so without a binding on the evaluation too, a seat could be
    /// scored on one proposal and attest a commit derived from another, and a
    /// orchestrator that ranks on scores and promotes on ids would have no way to see
    /// it. Both claims naming the same artifact is what closes that.
    ///
    /// `About` is generic precisely so this needs no evaluation-specific type: the
    /// orchestrator runs the same check for both.
    #[test]
    fn a_score_and_a_commit_are_comparable_only_about_the_same_artifact() {
        use crate::agents::Evaluation;

        let judged = br#"{"rationale":"the proposal that was read","ops":[]}"#;

        let candidate = super::About::this(
            super::Candidate {
                job: "j".into(),
                round: 1,
                agent: "Reviewer".into(),
                commit: "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2".into(),
            },
            judged,
        );
        let score = super::About::this(
            Evaluation {
                score: 0.75,
                justification: "grounded".into(),
                ..Default::default()
            },
            judged,
        );

        assert_eq!(
            candidate.artifact, score.artifact,
            "a score and a commit about the same proposal join"
        );
        assert!(candidate.is_about(judged) && score.is_about(judged));

        // The attack the binding exists to stop: a seat scored on what it showed,
        // promoting a commit built from something else.
        let elsewhere = super::About::this(
            super::Candidate {
                job: "j".into(),
                round: 1,
                agent: "Reviewer".into(),
                commit: "0badc0de0badc0de0badc0de0badc0de0badc0de".into(),
            },
            br#"{"rationale":"something nobody scored","ops":[]}"#,
        );
        assert_ne!(
            elsewhere.artifact, score.artifact,
            "a commit built from an unscored proposal does not join that score"
        );
        assert!(
            !elsewhere.is_about(judged),
            "and does not claim to be about the judged one"
        );
    }

    /// A skip is a claim about the empty artifact, not a claim about no artifact.
    ///
    /// Keeping one shape matters for what the orchestrator can conclude. A signed skip
    /// says "nothing, for this slot" and is attributable; silence says nothing at
    /// all and is indistinguishable from a seat that was never asked, or whose
    /// message was lost. Collapsing the two would let a dropped message read as a
    /// deliberate decline.
    #[test]
    fn a_skip_is_a_claim_about_the_empty_artifact() {
        let skipped = super::About::nothing(super::Candidate {
            job: "j".into(),
            round: 1,
            agent: "Reviewer".into(),
            commit: "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2".into(),
        });

        assert!(skipped.is_about_nothing(), "a skip is about nothing");
        assert!(
            skipped.is_about(b""),
            "which is the empty artifact, not a missing binding"
        );
        assert_eq!(
            skipped.artifact, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "the well-known SHA-256 of the empty input — a reader can recognise a \
             skip without being told which bytes produced it"
        );

        // A skip and a real claim are never confusable, in either direction.
        let real = super::About::this(
            super::Candidate {
                job: "j".into(),
                round: 1,
                agent: "Reviewer".into(),
                commit: "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2".into(),
            },
            br#"{"rationale":"a real proposal","ops":[]}"#,
        );
        assert!(!real.is_about_nothing(), "a real claim is not a skip");
        assert_ne!(real.artifact, skipped.artifact);
    }

    /// The dylib emits the candidate; this crate reads and binds it. They live in
    /// separate repositories, so nothing but a fixture keeps the two halves
    /// agreeing — rename a field on either side and the seat's commit silently
    /// stops being readable, with no compiler and no test to say so.
    ///
    /// The literal below is the shape `provider_response` returns under
    /// `hook_state.pd_candidate` (patch-deliberation, documented in
    /// `docs/reference/hooks-and-config.md`).
    #[test]
    fn the_candidate_the_dylib_emits_binds_to_the_artifact_it_was_judged_on() {
        let from_hook_state = serde_json::json!({
            "job": "thread-t1_jobAAAA",
            "round": 0,
            "agent": "AgentA",
            "commit": "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2"
        });
        let published = br#"{"rationale":"tightened the estimate","ops":[]}"#;

        let candidate =
            super::Candidate::reported(&from_hook_state).expect("the dylib's shape reads");
        assert_eq!(candidate.job, "thread-t1_jobAAAA");
        assert_eq!(candidate.round, 0);
        assert_eq!(candidate.agent, "AgentA");
        assert_eq!(candidate.commit, "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2");

        // A candidate that is not one yields nothing, rather than a claim bound to
        // whatever happened to be published.
        assert!(super::Candidate::reported(&serde_json::json!({})).is_none());
        assert!(
            super::Candidate::reported(&serde_json::json!({
                "job": "j", "round": 0, "agent": "a", "commit": ""
            }))
            .is_none(),
            "an empty commit is not a candidate"
        );

        // Bound, it answers the question the orchestrator actually asks: is this claim
        // about the artifact I hold?
        let bound = super::About::this(candidate, published);
        assert!(bound.is_about(published), "bound to what it was judged on");
        assert!(
            !bound.is_about(b"a different proposal"),
            "swapping the judged artifact must break the binding"
        );
    }
}
