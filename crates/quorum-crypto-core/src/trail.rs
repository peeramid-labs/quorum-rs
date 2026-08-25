//! Reading and tallying a signed audit trail.
//!
//! Verification lives here rather than beside the transport that carries it, so a
//! reader on either side of the bus checks a record the same way. Only the loop
//! that subscribes belongs with the transport.

/// What reading one record off the audit trail produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditRecord {
    /// Every signature in the chain verified against the payload it covers.
    Verified {
        /// The agent that signed it.
        agent_id: String,
        signatures: usize,
    },
    /// The record parsed, but its chain did not verify — the payload or a
    /// signature was altered after signing, or a key does not match.
    Tampered { agent_id: String },
    /// The record carried no signature at all.
    Unsigned { agent_id: String },
}

/// Read and verify one record from the audit trail.
///
/// The payload is treated as opaque JSON: a reader checks provenance, and does not
/// need to know the shape of what was signed to do that.
///
/// A record whose chain fails is reported as [`AuditRecord::Tampered`] rather than
/// as an error, because failing to verify IS the finding — an audit reader that
/// discarded it as a parse problem would lose the one event it exists to catch.
/// `Err` is reserved for bytes that are not a record at all.
pub fn read_audit_record(
    bytes: &[u8],
    registry: &crate::VerifierRegistry,
) -> Result<AuditRecord, serde_json::Error> {
    let mut envelope: crate::AuditEnvelope<serde_json::Value> = serde_json::from_slice(bytes)?;
    let agent_id = envelope.agent_id().to_string();
    if envelope.signatures().is_empty() {
        return Ok(AuditRecord::Unsigned { agent_id });
    }
    let signatures = envelope.signatures().len();
    match envelope.verify_chain(registry) {
        Ok(true) => Ok(AuditRecord::Verified {
            agent_id,
            signatures,
        }),
        // A verifier error (unknown algorithm, malformed key) is not a reason to
        // call a record sound; it is a reason to not trust it.
        Ok(false) | Err(_) => Ok(AuditRecord::Tampered { agent_id }),
    }
}

/// What a job's audit trail amounted to.
///
/// A tally rather than a boolean: a trail with one tampered record among fifty is
/// a different situation from one that was never written, and collapsing both to
/// "not sound" throws away the part a reader acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrailSummary {
    /// Records whose whole signature chain verified.
    pub verified: usize,
    /// Agents whose records did not verify, in the order encountered. Named rather
    /// than counted, because the useful question is which agent's work to distrust.
    pub tampered: Vec<String>,
    /// Records carrying no signature at all.
    pub unsigned: usize,
    /// Bytes on the trail that were not a record. Kept separate from `tampered`:
    /// unreadable is a fault in the trail, not evidence against an agent.
    pub unreadable: usize,
}

impl TrailSummary {
    /// Fold one record's outcome into the tally.
    pub fn record(&mut self, outcome: Result<AuditRecord, serde_json::Error>) {
        match outcome {
            Ok(AuditRecord::Verified { .. }) => self.verified += 1,
            Ok(AuditRecord::Tampered { agent_id }) => self.tampered.push(agent_id),
            Ok(AuditRecord::Unsigned { .. }) => self.unsigned += 1,
            Err(_) => self.unreadable += 1,
        }
    }

    /// Whether every record on the trail verified.
    ///
    /// An empty trail is NOT sound: nothing was recorded, so nothing was shown. A
    /// reader asking this question wants evidence, and absence is not evidence.
    pub fn is_sound(&self) -> bool {
        self.verified > 0 && self.tampered.is_empty() && self.unsigned == 0 && self.unreadable == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tally distinguishes the situations a reader acts on differently, and
    /// treats an empty trail as unproven rather than clean — absence of evidence is
    /// the one answer a verifier must never give as reassurance.
    #[test]
    fn a_trail_summary_separates_the_ways_a_record_can_fail() {
        let mut s = TrailSummary::default();
        assert!(!s.is_sound(), "an empty trail has shown nothing");

        s.record(Ok(AuditRecord::Verified {
            agent_id: "a".into(),
            signatures: 1,
        }));
        assert!(s.is_sound(), "a trail of sound records is sound");

        s.record(Ok(AuditRecord::Unsigned {
            agent_id: "b".into(),
        }));
        assert!(!s.is_sound(), "an unsigned record is not a verified one");

        s.record(Ok(AuditRecord::Tampered {
            agent_id: "c".into(),
        }));
        s.record(Err(serde_json::from_slice::<u8>(b"x").unwrap_err()));
        assert_eq!(s.verified, 1);
        assert_eq!(
            s.tampered,
            vec!["c".to_string()],
            "the agent is named, not counted"
        );
        assert_eq!(s.unsigned, 1);
        assert_eq!(
            s.unreadable, 1,
            "unreadable is a fault in the trail, not against an agent"
        );
    }
}
