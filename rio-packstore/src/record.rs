//! Self-describing, resyncable pack records.
//!
//! Wire layout: `magic(4) kind(1) flags(1) len(4 LE) digest(32) bytes`.
//! Packs alone can rebuild the index: every record carries its own
//! kind, length, and blake3 digest. After a corrupted length the
//! rebuild scanner resyncs forward to the next magic and verifies the
//! digest, so a mid-pack hole drops one record, not the tail.

use crate::{Digest, Error, Kind, RECORD_HEADER_LEN, Result};

pub(crate) const MAGIC: [u8; 4] = *b"RPK1";

// TODO: zstd compression (per pack or per large record, NEVER per
// blob — 140B mean blob size makes per-blob zstd a net loss, ADR-024)
// is deferred to a follow-up. `FLAG_ZSTD` in lib.rs reserves the flag
// bit; until it ships, readers reject any record with nonzero flags
// (the digest is over uncompressed bytes, so an unknown encoding
// cannot be verified — dropping the record is the safe answer).

/// Encode one record. The digest must be `blake3(payload)` — callers
/// compute it anyway for the index, so we don't rehash here.
pub(crate) fn encode(kind: Kind, payload: &[u8], digest: &Digest) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len()).map_err(|_| Error::TooLarge(payload.len() as u64))?;
    let mut buf = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    buf.extend_from_slice(&MAGIC);
    buf.push(kind.0);
    buf.push(0); // flags: no compression yet, see TODO above
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&digest.0);
    buf.extend_from_slice(payload);
    Ok(buf)
}

/// One verified record found by [`scan`]. Offsets are relative to the
/// start of the pack file.
pub(crate) struct ScanRecord {
    pub kind: Kind,
    pub digest: Digest,
    pub payload_offset: u64,
    pub payload_len: u32,
}

pub(crate) struct ScanOutcome {
    pub records: Vec<ScanRecord>,
    /// Offset where trailing garbage begins, when the file ends in a
    /// run of bytes that never resyncs to another valid record (the
    /// torn-tail case). `None` when the file ends cleanly. Physical
    /// truncation of a torn tail is ONLY legal for a process holding
    /// the exclusive GC flock that also holds the segment's writer
    /// flock exclusively (owner provably gone) — scanning never
    /// mutates anything.
    pub tail_garbage_from: Option<u64>,
}

/// Parse a whole pack image, resyncing over corruption.
pub(crate) fn scan(data: &[u8]) -> ScanOutcome {
    let mut records = Vec::new();
    let mut pos = 0usize;
    // Start of the current unparseable run; cleared whenever a record
    // verifies. If still set at EOF, the tail is garbage.
    let mut bad_run_start: Option<usize> = None;
    while pos < data.len() {
        if let Some((rec, next)) = try_parse(data, pos) {
            records.push(rec);
            bad_run_start = None;
            pos = next;
        } else {
            if bad_run_start.is_none() {
                bad_run_start = Some(pos);
            }
            // Resync: hunt for the next magic strictly past this byte.
            // A payload may contain the magic bytes, but we only get
            // here after corruption, and the digest check rejects any
            // false positive the hunt turns up.
            pos += 1;
            while pos + MAGIC.len() <= data.len() && data[pos..pos + MAGIC.len()] != MAGIC {
                pos += 1;
            }
            if pos + MAGIC.len() > data.len() {
                pos = data.len();
            }
        }
    }
    ScanOutcome {
        records,
        tail_garbage_from: bad_run_start.map(|p| p as u64),
    }
}

/// Try to parse and verify one record at `pos`. Returns the record and
/// the offset just past it.
fn try_parse(data: &[u8], pos: usize) -> Option<(ScanRecord, usize)> {
    let header = data.get(pos..pos + RECORD_HEADER_LEN)?;
    if header[0..4] != MAGIC {
        return None;
    }
    let kind = Kind(header[4]);
    if header[5] != 0 {
        // Unknown flags (reserved compression bit included): the
        // digest covers uncompressed bytes, so we cannot verify this
        // record — treat as corruption and resync past it.
        return None;
    }
    let len = u32::from_le_bytes(header[6..10].try_into().ok()?) as usize;
    let payload_start = pos + RECORD_HEADER_LEN;
    let payload_end = payload_start.checked_add(len)?;
    let payload = data.get(payload_start..payload_end)?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&header[10..42]);
    if blake3::hash(payload).as_bytes() != &digest {
        return None;
    }
    let rec = ScanRecord {
        kind,
        digest: Digest(digest),
        payload_offset: payload_start as u64,
        payload_len: len as u32,
    };
    Some((rec, payload_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: u8, payload: &[u8]) -> Vec<u8> {
        encode(Kind(kind), payload, &Digest::of(payload)).unwrap()
    }

    #[test]
    fn scan_empty_is_clean() {
        let out = scan(&[]);
        assert!(out.records.is_empty());
        assert_eq!(out.tail_garbage_from, None);
    }

    #[test]
    fn scan_roundtrips_records() {
        let mut data = rec(0, b"alpha");
        data.extend(rec(1, b"beta"));
        let out = scan(&data);
        assert_eq!(out.records.len(), 2);
        assert_eq!(out.records[0].digest, Digest::of(b"alpha"));
        assert_eq!(out.records[1].kind, Kind(1));
        assert_eq!(out.tail_garbage_from, None);
    }

    #[test]
    fn corrupted_length_drops_one_record_not_the_tail() {
        let r1 = rec(0, b"first");
        let mut r2 = rec(0, b"second");
        let r3 = rec(0, b"third");
        // Smash record 2's length field.
        r2[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut data = r1;
        let hole_at = data.len() as u64;
        data.extend(&r2);
        data.extend(&r3);
        let out = scan(&data);
        let digests: Vec<_> = out.records.iter().map(|r| r.digest).collect();
        assert_eq!(digests, vec![Digest::of(b"first"), Digest::of(b"third")]);
        // The hole resynced — the corruption is mid-pack, not tail garbage.
        assert_eq!(out.tail_garbage_from, None);
        assert!(hole_at > 0);
    }

    #[test]
    fn torn_tail_is_reported_not_parsed() {
        let r1 = rec(0, b"kept");
        let torn = rec(0, b"torn-away-payload");
        let mut data = r1.clone();
        data.extend(&torn[..torn.len() - 5]); // crash mid-write
        let out = scan(&data);
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.tail_garbage_from, Some(r1.len() as u64));
    }

    #[test]
    fn nonzero_flags_reject_record() {
        let mut data = rec(0, b"payload");
        data[5] = crate::FLAG_ZSTD;
        let out = scan(&data);
        assert!(out.records.is_empty());
        assert_eq!(out.tail_garbage_from, Some(0));
    }
}
