//! Reading a job's audit trail off a live broker.
//!
//! The pure halves — signing a copy, verifying a record, tallying outcomes — are
//! covered by unit tests. What only a broker can show is that a reader subscribed
//! to the trail actually receives what an agent wrote, and that one unsound record
//! does not cost it the rest.

use super::common::{try_connect_nats, unique_id};
use futures::StreamExt;
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

/// A promoter settles a deliberation from the trail alone: it never opens the
/// repository, and it never sees the proposal except as bytes it relayed.
///
/// The unit tests cover signing and binding in isolation. What only a broker shows
/// is that the claim a seat publishes beside its result is the one a reader
/// subscribed to that job actually receives — on the same trail, verifying with
/// the same registry, with no separate subscription.
#[tokio::test]
async fn a_seats_candidate_is_published_where_a_reader_can_still_find_it() {
    let Some(nats) = try_connect_nats().await else {
        eprintln!("skipping: no NATS at NATS_URL");
        return;
    };
    let uid = unique_id();
    let prefix = format!("cand_{uid}");
    let job = format!("job_{uid}");

    let hook = AuditTrailHook::new(AgentKeyPair::generate().as_audit_signer(), "agent-a".into());
    let published = serde_json::to_vec(&Proposal {
        thought_process: "considered".into(),
        content: "the answer".into(),
        ..Default::default()
    })
    .unwrap();
    let working = format!("{prefix}.{job}.result.0.agent-a.propose");
    let reported = serde_json::json!({
        "job": job, "round": 0, "agent": "agent-a",
        "commit": "9f2c1b7e4d5a6083c1e2f3a4b5c6d7e8f9a0b1c2"
    });
    let (subject, claim) = hook
        .candidate_copy(&working, &reported, &published)
        .await
        .expect("the seat reported a candidate");

    // The claim goes to the job's event tree, which the per-job stream captures.
    // The audit subtree is not captured, and a claim published during a round is
    // read after the rounds end — so a reader arriving late would find nothing
    // there and the deliberation would settle as if no seat had spoken.
    assert_eq!(
        subject,
        format!("{prefix}.{job}.result.event.candidate"),
        "the claim is published where a late reader can still find it"
    );

    let mut on_event_tree = nats
        .subscribe(format!("{prefix}.{job}.result.event.candidate"))
        .await
        .expect("subscribe to the event tree");
    let mut on_audit_tree = nats
        .subscribe(format!("{prefix}.{job}.audit.>"))
        .await
        .expect("subscribe to the audit subtree");
    nats.flush().await.unwrap();

    nats.publish(subject, claim.into()).await.unwrap();
    nats.flush().await.unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(600), on_event_tree.next())
        .await
        .expect("the claim arrives on the event tree")
        .expect("a message");
    let registry = VerifierRegistry::with_defaults();
    assert_eq!(
        quorum_rs::crypto::read_audit_record(&msg.payload, &registry).unwrap(),
        quorum_rs::crypto::AuditRecord::Verified {
            agent_id: "agent-a".to_string(),
            signatures: 1,
        },
        "and verifies like any other signed record"
    );

    // It is deliberately NOT on the audit subtree any more; asserting that keeps
    // the two subjects from quietly drifting back together.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), on_audit_tree.next())
            .await
            .is_err(),
        "the claim no longer rides the uncaptured audit subtree"
    );
}
