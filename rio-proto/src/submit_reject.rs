//! The `SubmitBuild` stale-digest reject contract (ADR-024).
//!
//! When the scheduler's submit-time bulk-verify finds drv digests that
//! are neither in the submission nor in the store, it rejects with
//! `FAILED_PRECONDITION` and the client runs stale-ack recovery
//! (`r[bc.submit.stale-ack-once]`): evict acks, re-`Has`, re-upload,
//! resubmit once. The digest list rides the status MESSAGE — both the
//! formatter (scheduler) and the parser (build client) live HERE so
//! the two sides cannot drift apart silently: a reword on one side is
//! a change to this module, covered by the round-trip test below.

/// Format the reject message naming every missing digest.
/// `missing_hex` entries are lowercase 64-char hex; callers pass them
/// sorted and deduped for deterministic output.
pub fn missing_drv_digests_message(missing_hex: &[String]) -> String {
    format!(
        "unknown drv digests (not in this submission, not in the store): [{}] — \
         re-check presence (HasDrvs), upload the missing drv blobs (PutDrvBlobs), \
         and resubmit",
        missing_hex.join(", ")
    )
}

/// Extract 32-byte digests from a reject message: every 64-char
/// lowercase-hex token. Scanning for hex tokens (rather than matching
/// the exact sentence) keeps the client tolerant of message rewording
/// AROUND the list, but the list encoding itself is pinned by
/// [`missing_drv_digests_message`] and the round-trip test.
pub fn parse_missing_drv_digests(message: &str) -> Vec<[u8; 32]> {
    let mut out = Vec::new();
    let bytes = message.as_bytes();
    let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let mut i = 0;
    while i < bytes.len() {
        if !is_hex(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_hex(bytes[i]) {
            i += 1;
        }
        if i - start == 64
            && let Ok(raw) = hex::decode(&message[start..i])
            && let Ok(d) = <[u8; 32]>::try_from(raw.as_slice())
        {
            out.push(d);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE cross-crate pin: whatever the scheduler formats, the client
    /// parses back — byte-identical digest sets.
    #[test]
    fn format_parse_round_trip() {
        let digests = [[0xABu8; 32], [0x01; 32], [0x42; 32]];
        let mut hexes: Vec<String> = digests.iter().map(hex::encode).collect();
        hexes.sort();
        let msg = missing_drv_digests_message(&hexes);
        let parsed = parse_missing_drv_digests(&msg);
        let mut expected: Vec<[u8; 32]> = digests.to_vec();
        expected.sort_unstable();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn ignores_short_hex_and_dedups() {
        let d = hex::encode([0x42u8; 32]);
        let msg = format!("deadbeef {d} cafe {d}");
        assert_eq!(parse_missing_drv_digests(&msg), vec![[0x42u8; 32]]);
    }

    #[test]
    fn uppercase_hex_is_not_a_digest_token() {
        // The formatter emits lowercase (`hex::encode`); uppercase is
        // never produced and must not parse.
        let msg = format!("[{}]", hex::encode([0xABu8; 32]).to_uppercase());
        assert!(parse_missing_drv_digests(&msg).is_empty());
    }
}
