//! The `PutPathChunked` missing-chunk reject contract (ADR-024).
//!
//! `HasChunks` answers from the `chunks` table's durable flag, but the
//! flag can lie when a chunk row outlives its backing object (GC grace
//! shorter than the client ack TTL, or an S3 fault). The store detects
//! the lie when file-digest verification or the commit-time presence
//! proof needs the object back, and rejects the upload `UNAVAILABLE`
//! naming the digest. The build client then runs chunk stale-ack
//! recovery (`r[bc.upload.stale-ack-once]`): evict the named acks,
//! re-`HasChunks`, re-upload, retry once.
//!
//! Like [`crate::submit_reject`], the digest rides the status MESSAGE
//! — the formatter (rio-store) and the parser (build client) live HERE
//! so a reword on either side is a change to this module, covered by
//! the round-trip test below.

/// Format the reject message for a chunk whose backing object could
/// not be found. `digest_hex` is the lowercase 64-char hex digest.
pub fn missing_chunk_digest_message(digest_hex: &str) -> String {
    format!(
        "chunk {digest_hex} disappeared from the backing store — re-check presence \
         (HasChunks), re-upload it, and retry"
    )
}

/// Extract 32-byte chunk digests from a reject message: every 64-char
/// lowercase-hex token, sorted and deduped. Same scanner as the drv
/// reject parser — tolerant of wording around the digest, pinned to
/// the encoding by [`missing_chunk_digest_message`] and the round-trip
/// test.
pub fn parse_missing_chunk_digests(message: &str) -> Vec<[u8; 32]> {
    crate::submit_reject::parse_hex_digest_tokens(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE cross-crate pin: whatever the store formats, the client
    /// parses back — even with the handler's RPC prefix in front.
    #[test]
    fn format_parse_round_trip() {
        let digest = [0xC7u8; 32];
        let msg = format!(
            "PutPathChunked: {}",
            missing_chunk_digest_message(&hex::encode(digest))
        );
        assert_eq!(parse_missing_chunk_digests(&msg), vec![digest]);

        // A digest-free UNAVAILABLE (e.g. a transient S3 fault) parses
        // to nothing — the client falls back to conservative eviction.
        assert!(parse_missing_chunk_digests("PutPathChunked: chunk upload failed").is_empty());
    }
}
