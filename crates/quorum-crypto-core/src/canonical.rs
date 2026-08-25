//! Canonical byte encoding for signed payloads.
//!
//! The canonical form ensures deterministic byte representation for signing.
//! Payload is length-prefixed (not null-separated) to handle binary payloads
//! that may contain NUL bytes:
//!
//! ```text
//! subject_utf8 || 0x00 || payload_len_le_u64 || payload_bytes || timestamp_le_u64 || 0x00 || agent_id_utf8
//! ```

/// Build canonical bytes for signing from structured fields.
///
/// The payload is length-prefixed (8-byte LE u64) so binary payloads containing
/// NUL bytes are unambiguous. Text fields (subject, agent_id) are separated by
/// 0x00 which is safe since they're UTF-8 (no embedded NULs).
pub fn canonical_bytes(
    subject: &str,
    payload_json: &[u8],
    timestamp: u64,
    agent_id: &str,
) -> Vec<u8> {
    let mut buf =
        Vec::with_capacity(subject.len() + payload_json.len() + agent_id.len() + 8 + 8 + 2);
    buf.extend_from_slice(subject.as_bytes());
    buf.push(0x00);
    // Length-prefix payload to handle binary content with embedded NULs
    buf.extend_from_slice(&(payload_json.len() as u64).to_le_bytes());
    buf.extend_from_slice(payload_json);
    buf.extend_from_slice(&timestamp.to_le_bytes());
    buf.push(0x00);
    buf.extend_from_slice(agent_id.as_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signing covers bytes, so a payload must survive a JSON round-trip unchanged.
    /// `serde_json/preserve_order` is what makes that true: without it a `Value`
    /// sorts object keys, and a record signed from a struct — whose fields
    /// serialize in declaration order — verifies against different bytes than it
    /// was signed over. If this fails, the feature was dropped and every
    /// struct-signed record will read as tampered.
    #[test]
    fn a_json_round_trip_preserves_key_order_that_signatures_depend_on() {
        let signed_bytes = br#"{"zeta":1,"alpha":2}"#;
        let parsed: serde_json::Value = serde_json::from_slice(signed_bytes).unwrap();
        assert_eq!(
            serde_json::to_vec(&parsed).unwrap(),
            signed_bytes,
            "a Value round-trip must reproduce the bytes a signature covers"
        );
    }

    #[test]
    fn canonical_bytes_deterministic() {
        let a = canonical_bytes(
            "proposal",
            b"{\"content\":\"hello\"}",
            1234567890,
            "agent-1",
        );
        let b = canonical_bytes(
            "proposal",
            b"{\"content\":\"hello\"}",
            1234567890,
            "agent-1",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_bytes_different_fields_differ() {
        let a = canonical_bytes("proposal", b"{}", 100, "agent-1");
        let b = canonical_bytes("evaluation", b"{}", 100, "agent-1");
        assert_ne!(a, b);

        let c = canonical_bytes("proposal", b"{}", 100, "agent-2");
        assert_ne!(a, c);
    }

    #[test]
    fn canonical_bytes_handles_nul_in_payload() {
        // Payload with embedded NUL bytes should produce unambiguous canonical form
        let with_nul = canonical_bytes("sub", b"pay\x00load", 42, "agent");
        let without_nul = canonical_bytes("sub", b"payload", 42, "agent");
        // Different payloads → different canonical bytes (length prefix differs)
        assert_ne!(with_nul, without_nul);
    }

    #[test]
    fn canonical_bytes_length_prefix_prevents_ambiguity() {
        // Two payloads that would be ambiguous with NUL separators:
        // "ab" + NUL + "cd" vs "ab\x00cd" as a single payload
        let two_byte_payload = canonical_bytes("s", b"ab", 1, "a");
        let four_byte_with_nul = canonical_bytes("s", b"ab\x00c", 1, "a");
        // Length prefix makes them different (2 vs 4)
        assert_ne!(two_byte_payload, four_byte_with_nul);
    }
}
