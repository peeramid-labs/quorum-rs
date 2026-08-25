//! Reading a job's audit trail off a live broker.
//!
//! The pure halves — signing a copy, verifying a record, tallying outcomes — are
//! covered by unit tests. What only a broker can show is that a reader subscribed
//! to the trail actually receives what an agent wrote, and that one unsound record
//! does not cost it the rest.

use super::common::{try_connect_nats, unique_id};
use quorum_crypto_core::VerifierRegistry;
use quorum_rs::agents::Proposal;
use quorum_rs::crypto::{AgentKeyPair, AuditTrailHook, verify_job_trail};
use quorum_rs::workers::WorkerHook;
use std::time::Duration;

#[tokio::test]
async fn a_reader_tallies_a_jobs_trail_and_names_the_agent_whose_record_failed() {
    let Some(nats) = try_connect_nats().await else {
        eprintln!("skipping: no NATS at NATS_URL");
        return;
    };
    let uid = unique_id();
    let prefix = format!("audit_{uid}");
    let job = format!("job_{uid}");

    let hook = AuditTrailHook::new(AgentKeyPair::generate().as_audit_signer(), "agent-a".into());
    let proposal = serde_json::to_vec(&Proposal {
        thought_process: "considered".into(),
        content: "the answer".into(),
        ..Default::default()
    })
    .unwrap();
    let working = format!("{prefix}.{job}.result.0.agent-a.propose");
    let copies = hook.audit_copies(&working, &proposal).await;
    let record = copies[0].1.clone();
    let subject = copies[0].0.clone();

    // The reader must be listening before anything is written: a trail is a live
    // stream, not a store, so a record published first is simply missed.
    let registry = VerifierRegistry::with_defaults();
    let reader = tokio::spawn({
        let nats = nats.clone();
        let prefix = prefix.clone();
        let job = job.clone();
        async move {
            verify_job_trail(&nats, &prefix, &job, &registry, Duration::from_millis(600)).await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    nats.publish(subject.clone(), record.clone().into())
        .await
        .unwrap();

    // The same record with its payload altered — what the trail exists to catch.
    let mut altered: serde_json::Value = serde_json::from_slice(&record).unwrap();
    altered["payload"]["content"] = serde_json::json!("a different answer");
    nats.publish(
        subject.clone(),
        serde_json::to_vec(&altered).unwrap().into(),
    )
    .await
    .unwrap();

    nats.publish(subject, b"not a record".to_vec().into())
        .await
        .unwrap();
    nats.flush().await.unwrap();

    let summary = reader.await.unwrap().expect("the trail is readable");
    assert_eq!(
        summary.verified, 1,
        "the untouched record verified: {summary:?}"
    );
    assert_eq!(
        summary.tampered,
        vec!["agent-a".to_string()],
        "and the altered one named its agent rather than being counted: {summary:?}"
    );
    assert_eq!(
        summary.unreadable, 1,
        "bytes that are not a record are a trail fault"
    );
    assert!(
        !summary.is_sound(),
        "a trail carrying a failure is not sound"
    );
}
