//! Golden conformance test infrastructure.
//!
//! Provides field-level response parsing and byte comparison
//! for verifying rio-build output against live nix-daemon responses.

pub mod daemon;

use std::io::Cursor;
use std::sync::Arc;

use rio_build::store::MemoryStore;
use rio_nix::hash::NixHash;
use rio_nix::protocol::stderr::STDERR_WRITE;
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;

/// A single named field from a protocol response, stored as raw bytes.
#[derive(Debug)]
pub struct ResponseField {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

/// A store path entry used to populate MemoryStore for conformance tests.
#[derive(serde::Deserialize)]
pub struct StorePathEntry {
    pub path: String,
    pub deriver: Option<String>,
    /// SRI-format hash (e.g. "sha256-base64...")
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: Option<String>,
}

/// Build a MemoryStore populated with the given store path entries.
pub fn build_memory_store_from(entries: &[StorePathEntry]) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    for entry in entries {
        let path = StorePath::parse(&entry.path)
            .unwrap_or_else(|e| panic!("invalid store path '{}': {e}", entry.path));
        let deriver = entry
            .deriver
            .as_ref()
            .map(|d| StorePath::parse(d).unwrap_or_else(|e| panic!("invalid deriver: {e}")));
        let nar_hash =
            NixHash::parse(&entry.nar_hash).unwrap_or_else(|e| panic!("invalid nar_hash: {e}"));
        let references: Vec<StorePath> = entry
            .references
            .iter()
            .map(|r| StorePath::parse(r).unwrap_or_else(|e| panic!("invalid reference: {e}")))
            .collect();

        store.insert(
            rio_build::store::PathInfoBuilder::new(path, nar_hash, entry.nar_size)
                .deriver(deriver)
                .references(references)
                .registration_time(entry.registration_time)
                .ultimate(entry.ultimate)
                .sigs(entry.sigs.clone())
                .ca(entry.ca.clone())
                .build()
                .unwrap(),
            None,
        );
    }
    store
}

// ---------------------------------------------------------------------------
// Low-level byte readers
// ---------------------------------------------------------------------------

/// Read exactly `n` bytes from the cursor, returning them as a Vec.
async fn read_raw(cursor: &mut Cursor<Vec<u8>>, n: usize) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; n];
    cursor.read_exact(&mut buf).await.unwrap();
    buf
}

/// Read a u64 field and return its 8 raw bytes.
async fn read_u64_field(cursor: &mut Cursor<Vec<u8>>) -> Vec<u8> {
    read_raw(cursor, 8).await
}

/// Read a length-prefixed, padded string field and return ALL its raw bytes
/// (length prefix + string content + padding).
async fn read_string_field(cursor: &mut Cursor<Vec<u8>>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    // Read 8-byte length prefix
    let mut len_bytes = [0u8; 8];
    cursor.read_exact(&mut len_bytes).await.unwrap();
    let len = u64::from_le_bytes(len_bytes) as usize;

    // String content + padding to 8-byte boundary
    let padded_len = (len + 7) & !7;
    let mut content = vec![0u8; padded_len];
    if padded_len > 0 {
        cursor.read_exact(&mut content).await.unwrap();
    }

    let mut result = Vec::with_capacity(8 + padded_len);
    result.extend_from_slice(&len_bytes);
    result.extend_from_slice(&content);
    result
}

/// Read a count-prefixed string collection and return ALL raw bytes
/// (count prefix + each string's length prefix + content + padding).
async fn read_strings_field(cursor: &mut Cursor<Vec<u8>>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    // Read 8-byte count prefix
    let mut count_bytes = [0u8; 8];
    cursor.read_exact(&mut count_bytes).await.unwrap();
    let count = u64::from_le_bytes(count_bytes) as usize;

    let mut result = Vec::new();
    result.extend_from_slice(&count_bytes);

    for _ in 0..count {
        // Each element is a length-prefixed padded string
        let mut len_bytes = [0u8; 8];
        cursor.read_exact(&mut len_bytes).await.unwrap();
        let len = u64::from_le_bytes(len_bytes) as usize;
        let padded_len = (len + 7) & !7;

        result.extend_from_slice(&len_bytes);
        if padded_len > 0 {
            let mut content = vec![0u8; padded_len];
            cursor.read_exact(&mut content).await.unwrap();
            result.extend_from_slice(&content);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// STDERR activity message helpers
// ---------------------------------------------------------------------------

/// Skip a single STDERR activity message from the cursor.
///
/// Consumes the message payload (but NOT the initial message-type u64, which
/// should already have been read by the caller). Handles:
/// - `STDERR_START_ACTIVITY`: id + level + type + text + typed fields + parent
/// - `STDERR_STOP_ACTIVITY`: id
/// - `STDERR_RESULT`: activity_id + result_type + typed fields
async fn skip_stderr_activity_message(cursor: &mut Cursor<Vec<u8>>, msg_type: u64) {
    use rio_nix::protocol::stderr::{STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};

    match msg_type {
        STDERR_START_ACTIVITY => {
            // id + level + type + text
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
            let _ = read_string_field(cursor).await;
            // typed fields: count + (type + value) per field
            let count_bytes = read_u64_field(cursor).await;
            let count = u64::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
            for _ in 0..count {
                let type_bytes = read_u64_field(cursor).await;
                let field_type = u64::from_le_bytes(type_bytes.try_into().unwrap());
                if field_type == 0 {
                    let _ = read_u64_field(cursor).await;
                } else {
                    let _ = read_string_field(cursor).await;
                }
            }
            // parent activity id
            let _ = read_u64_field(cursor).await;
        }
        STDERR_STOP_ACTIVITY => {
            let _ = read_u64_field(cursor).await;
        }
        STDERR_RESULT => {
            // activity_id + result_type
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
            // typed fields
            let count_bytes = read_u64_field(cursor).await;
            let count = u64::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
            for _ in 0..count {
                let type_bytes = read_u64_field(cursor).await;
                let field_type = u64::from_le_bytes(type_bytes.try_into().unwrap());
                if field_type == 0 {
                    let _ = read_u64_field(cursor).await;
                } else {
                    let _ = read_string_field(cursor).await;
                }
            }
        }
        other => panic!("unexpected STDERR message type {other:#x}"),
    }
}

/// Returns true if `msg_type` is a STDERR activity message (START, STOP, or RESULT).
fn is_stderr_activity(msg_type: u64) -> bool {
    use rio_nix::protocol::stderr::{STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};
    matches!(
        msg_type,
        STDERR_START_ACTIVITY | STDERR_STOP_ACTIVITY | STDERR_RESULT
    )
}

// ---------------------------------------------------------------------------
// Field-level response parsers
// ---------------------------------------------------------------------------

/// Parse a handshake response into named fields.
pub async fn parse_handshake_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "magic2",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "version",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "features",
            bytes: read_strings_field(&mut cursor).await,
        },
        ResponseField {
            name: "version_string",
            bytes: read_string_field(&mut cursor).await,
        },
        ResponseField {
            name: "trusted",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
    ]
}

/// Parse a SetOptions response (after the handshake fields).
///
/// The nix-daemon sends only STDERR_LAST — no result value follows.
pub async fn parse_set_options_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![ResponseField {
        name: "stderr_last",
        bytes: read_u64_field(&mut cursor).await,
    }]
}

/// Parse an IsValidPath response (after handshake + SetOptions).
pub async fn parse_is_valid_path_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "valid",
            bytes: read_u64_field(&mut cursor).await,
        },
    ]
}

/// Parse a QueryPathInfo response (after handshake + SetOptions).
/// Handles both found and not-found cases.
pub async fn parse_query_path_info_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    let mut fields = vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "valid",
            bytes: read_u64_field(&mut cursor).await,
        },
    ];

    // Check if valid == 1 (true) to decide whether more fields follow
    let valid = u64::from_le_bytes(fields[1].bytes.clone().try_into().unwrap());
    if valid != 0 {
        fields.extend(vec![
            ResponseField {
                name: "deriver",
                bytes: read_string_field(&mut cursor).await,
            },
            ResponseField {
                name: "nar_hash",
                bytes: read_string_field(&mut cursor).await,
            },
            ResponseField {
                name: "references",
                bytes: read_strings_field(&mut cursor).await,
            },
            ResponseField {
                name: "reg_time",
                bytes: read_u64_field(&mut cursor).await,
            },
            ResponseField {
                name: "nar_size",
                bytes: read_u64_field(&mut cursor).await,
            },
            ResponseField {
                name: "ultimate",
                bytes: read_u64_field(&mut cursor).await,
            },
            ResponseField {
                name: "sigs",
                bytes: read_strings_field(&mut cursor).await,
            },
            ResponseField {
                name: "ca",
                bytes: read_string_field(&mut cursor).await,
            },
        ]);
    }

    fields
}

/// Parse a QueryValidPaths response (after handshake + SetOptions).
pub async fn parse_query_valid_paths_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "valid_paths",
            bytes: read_strings_field(&mut cursor).await,
        },
    ]
}

/// Parse an AddTempRoot response (after handshake + SetOptions).
pub async fn parse_add_temp_root_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "result",
            bytes: read_u64_field(&mut cursor).await,
        },
    ]
}

/// Parse a QueryMissing response (after handshake + SetOptions).
pub async fn parse_query_missing_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "will_build",
            bytes: read_strings_field(&mut cursor).await,
        },
        ResponseField {
            name: "will_substitute",
            bytes: read_strings_field(&mut cursor).await,
        },
        ResponseField {
            name: "unknown",
            bytes: read_strings_field(&mut cursor).await,
        },
        ResponseField {
            name: "download_size",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "nar_size",
            bytes: read_u64_field(&mut cursor).await,
        },
    ]
}

/// Parse a NarFromPath response (after handshake + SetOptions).
///
/// NarFromPath uses STDERR_WRITE streaming: zero or more STDERR_WRITE chunks
/// followed by STDERR_LAST. The daemon may also interleave activity messages
/// (STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_RESULT) before or
/// between data chunks. This parser handles the full STDERR message stream,
/// properly skipping activity messages and collecting only NAR data.
///
/// Returns two fields:
/// - `nar_data`: concatenated NAR content from all STDERR_WRITE chunks
/// - `stderr_last`: the final STDERR_LAST marker
pub fn parse_nar_from_path_fields(
    data: &[u8],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ResponseField>> + '_>> {
    Box::pin(async move {
        use rio_nix::protocol::stderr::STDERR_LAST;

        let mut cursor = Cursor::new(data.to_vec());
        let mut nar_data = Vec::new();

        loop {
            let msg_bytes = read_u64_field(&mut cursor).await;
            let msg = u64::from_le_bytes(msg_bytes.clone().try_into().unwrap());

            if msg == STDERR_LAST {
                return vec![
                    ResponseField {
                        name: "nar_data",
                        bytes: nar_data,
                    },
                    ResponseField {
                        name: "stderr_last",
                        bytes: msg_bytes,
                    },
                ];
            } else if msg == STDERR_WRITE {
                let chunk_field = read_string_field(&mut cursor).await;
                let chunk_len = u64::from_le_bytes(chunk_field[..8].try_into().unwrap()) as usize;
                nar_data.extend_from_slice(&chunk_field[8..8 + chunk_len]);
            } else if is_stderr_activity(msg) {
                skip_stderr_activity_message(&mut cursor, msg).await;
            } else {
                panic!("unexpected STDERR message type {msg:#x} in NarFromPath response");
            }
        }
    })
}

/// Parse a NarFromPath response as sent by the real nix-daemon.
///
/// The daemon sends: [activity messages...] STDERR_LAST <raw NAR bytes>
/// This differs from rio-build which uses STDERR_WRITE framing.
/// The function returns the raw NAR content (after STDERR_LAST) and the
/// STDERR_LAST marker.
pub fn parse_nar_from_path_daemon_fields(
    data: &[u8],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ResponseField>> + '_>> {
    Box::pin(async move {
        use rio_nix::protocol::stderr::STDERR_LAST;

        let mut cursor = Cursor::new(data.to_vec());

        // Skip activity messages until STDERR_LAST
        let last_bytes;
        loop {
            let msg_bytes = read_u64_field(&mut cursor).await;
            let msg = u64::from_le_bytes(msg_bytes.clone().try_into().unwrap());

            if msg == STDERR_LAST {
                last_bytes = msg_bytes;
                break;
            } else if is_stderr_activity(msg) {
                skip_stderr_activity_message(&mut cursor, msg).await;
            } else {
                panic!("unexpected STDERR message type {msg:#x} in NarFromPath daemon response");
            }
        }

        // Everything after STDERR_LAST is raw NAR data
        use tokio::io::AsyncReadExt;
        let mut nar_data = Vec::new();
        cursor.read_to_end(&mut nar_data).await.unwrap();

        vec![
            ResponseField {
                name: "nar_data",
                bytes: nar_data,
            },
            ResponseField {
                name: "stderr_last",
                bytes: last_bytes,
            },
        ]
    })
}

/// Parse a QueryPathFromHashPart response (after handshake + SetOptions).
pub async fn parse_query_path_from_hash_part_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "path",
            bytes: read_string_field(&mut cursor).await,
        },
    ]
}

/// Parse an AddSignatures response (after handshake + SetOptions).
pub async fn parse_add_signatures_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![
        ResponseField {
            name: "stderr_last",
            bytes: read_u64_field(&mut cursor).await,
        },
        ResponseField {
            name: "result",
            bytes: read_u64_field(&mut cursor).await,
        },
    ]
}

// ---------------------------------------------------------------------------
// STDERR activity stripping
// ---------------------------------------------------------------------------

/// Strip STDERR activity messages from an opcode response, returning only
/// the data from STDERR_LAST onward (STDERR_LAST + result fields).
///
/// Uses a structured parser to walk through activity messages rather than
/// scanning for byte patterns, which avoids false positives if the
/// STDERR_LAST byte pattern appears inside a string field.
pub async fn strip_stderr_activity(data: &[u8]) -> Vec<u8> {
    use rio_nix::protocol::stderr::STDERR_LAST;

    let mut cursor = Cursor::new(data.to_vec());

    loop {
        let msg_bytes = read_u64_field(&mut cursor).await;
        let msg = u64::from_le_bytes(msg_bytes.clone().try_into().unwrap());

        if msg == STDERR_LAST {
            // Return STDERR_LAST + everything after it
            let pos = cursor.position() as usize;
            let mut result = msg_bytes;
            result.extend_from_slice(&data[pos..]);
            return result;
        } else if is_stderr_activity(msg) {
            skip_stderr_activity_message(&mut cursor, msg).await;
        } else {
            // Not an activity and not STDERR_LAST — return the original data
            // unchanged and let the downstream parser handle it.
            return data.to_vec();
        }
    }
}

// ---------------------------------------------------------------------------
// Full-response parser: splits a complete response into handshake + opcode sections
// ---------------------------------------------------------------------------

/// Split a full protocol response into (handshake_bytes, remaining_bytes).
/// The handshake section ends after the first STDERR_LAST.
pub async fn split_handshake(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    // Parse handshake fields to find where they end
    let fields = parse_handshake_fields(data).await;
    let handshake_len: usize = fields.iter().map(|f| f.bytes.len()).sum();
    (
        data[..handshake_len].to_vec(),
        data[handshake_len..].to_vec(),
    )
}

/// Split remaining bytes after handshake into (set_options_bytes, remaining_bytes).
///
/// SetOptions response is exactly 8 bytes (STDERR_LAST, no result value).
pub fn split_set_options(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    assert!(
        data.len() >= 8,
        "SetOptions response too short: {} bytes (expected >= 8)",
        data.len()
    );
    (data[..8].to_vec(), data[8..].to_vec())
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Assert field-by-field byte equality between expected (nix-daemon) and actual
/// (rio-build) responses, skipping fields in the skip list.
///
/// Skipped fields are still validated for structural correctness (e.g.,
/// `version_string` must be a non-empty UTF-8 string, `trusted` must be
/// 0, 1, or 2) — they are only exempted from byte-equality comparison.
///
/// Fields present in one response but not the other are tolerated if the
/// extra fields are all in the skip list.
pub fn assert_field_conformance(
    expected: &[ResponseField],
    actual: &[ResponseField],
    skip: &[String],
) {
    // Validate structural correctness of skipped fields in both responses
    for fields in [expected, actual] {
        for f in fields.iter().filter(|f| skip.iter().any(|s| s == f.name)) {
            validate_skipped_field(f);
        }
    }

    // Filter to non-skipped fields for byte-level comparison
    let exp_filtered: Vec<_> = expected
        .iter()
        .filter(|f| !skip.iter().any(|s| s == f.name))
        .collect();
    let act_filtered: Vec<_> = actual
        .iter()
        .filter(|f| !skip.iter().any(|s| s == f.name))
        .collect();

    assert_eq!(
        exp_filtered.len(),
        act_filtered.len(),
        "non-skipped field count mismatch: expected {} fields, got {}\n\
         expected fields: {:?}\n\
         actual fields:   {:?}\n\
         skip list: {:?}",
        exp_filtered.len(),
        act_filtered.len(),
        expected.iter().map(|f| f.name).collect::<Vec<_>>(),
        actual.iter().map(|f| f.name).collect::<Vec<_>>(),
        skip,
    );

    for (i, (exp, act)) in exp_filtered.iter().zip(act_filtered.iter()).enumerate() {
        assert_eq!(
            exp.name, act.name,
            "field #{i} name mismatch: expected '{}', got '{}'",
            exp.name, act.name
        );

        if exp.bytes != act.bytes {
            let first_diff = exp
                .bytes
                .iter()
                .zip(act.bytes.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(exp.bytes.len().min(act.bytes.len()));

            let context_start = first_diff.saturating_sub(8);
            let context_end = (first_diff + 16).min(exp.bytes.len().max(act.bytes.len()));

            let exp_hex: Vec<String> = exp.bytes[context_start..context_end.min(exp.bytes.len())]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let act_hex: Vec<String> = act.bytes[context_start..context_end.min(act.bytes.len())]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();

            panic!(
                "byte mismatch in field '{}' (#{i}):\n\
                 expected {} bytes, got {} bytes\n\
                 first diff at byte offset {first_diff}\n\
                 expected (around diff): [{}]\n\
                 actual   (around diff): [{}]",
                exp.name,
                exp.bytes.len(),
                act.bytes.len(),
                exp_hex.join(" "),
                act_hex.join(" "),
            );
        }
    }
}

/// Validate structural correctness of a skipped field.
///
/// Fields that are skipped for byte-equality comparison (because they
/// legitimately differ between nix-daemon and rio-build) should still
/// have valid structure.
fn validate_skipped_field(field: &ResponseField) {
    match field.name {
        "version_string" => {
            // version_string is a wire string: u64(len) + content + padding
            assert!(
                field.bytes.len() >= 8,
                "version_string field too short ({} bytes)",
                field.bytes.len()
            );
            let len = u64::from_le_bytes(field.bytes[..8].try_into().unwrap()) as usize;
            assert!(len > 0, "version_string should not be empty");
            assert!(
                field.bytes.len() >= 8 + len,
                "version_string content truncated"
            );
            let content = &field.bytes[8..8 + len];
            assert!(
                std::str::from_utf8(content).is_ok(),
                "version_string is not valid UTF-8"
            );
        }
        "trusted" => {
            // trusted is a u64: 0 (not trusted), 1 (trusted), or 2 (unknown)
            assert_eq!(
                field.bytes.len(),
                8,
                "trusted field should be exactly 8 bytes"
            );
            let val = u64::from_le_bytes(field.bytes.clone().try_into().unwrap());
            assert!(val <= 2, "trusted field should be 0, 1, or 2, got {val}");
        }
        _ => {} // Unknown skipped fields — no specific validation
    }
}

/// Total byte count across all response fields.
pub fn fields_byte_len(fields: &[ResponseField]) -> usize {
    fields.iter().map(|f| f.bytes.len()).sum()
}

/// Assert that all bytes have been consumed from a data slice.
///
/// This catches spurious trailing data in protocol responses (e.g.,
/// a result value sent after STDERR_LAST when none is expected).
pub fn assert_fully_consumed(data: &[u8], fields: &[ResponseField], context: &str) {
    let consumed = fields_byte_len(fields);
    assert_eq!(
        consumed,
        data.len(),
        "{context}: expected all {expected} bytes consumed, but only {consumed} of {expected} consumed \
         ({remaining} trailing bytes: [{hex}])",
        expected = data.len(),
        consumed = consumed,
        remaining = data.len() - consumed,
        hex = data[consumed..]
            .iter()
            .take(32)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
}

// ---------------------------------------------------------------------------
// Client byte builders (shared between daemon exchange and replay tests)
// ---------------------------------------------------------------------------

/// Build wopIsValidPath client bytes for a given path.
pub async fn build_is_valid_path_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 1).await.unwrap();
    wire::write_string(&mut buf, path).await.unwrap();
    buf
}

/// Build wopQueryPathInfo client bytes for a given path.
pub async fn build_query_path_info_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 26).await.unwrap();
    wire::write_string(&mut buf, path).await.unwrap();
    buf
}

/// Build wopQueryValidPaths client bytes.
pub async fn build_query_valid_paths_bytes(paths: &[&str], substitute: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 31).await.unwrap();
    let owned: Vec<String> = paths.iter().map(|s| (*s).to_string()).collect();
    wire::write_strings(&mut buf, &owned).await.unwrap();
    wire::write_bool(&mut buf, substitute).await.unwrap();
    buf
}

/// Build wopAddTempRoot client bytes.
pub async fn build_add_temp_root_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 11).await.unwrap();
    wire::write_string(&mut buf, path).await.unwrap();
    buf
}

/// Build wopQueryMissing client bytes.
pub async fn build_query_missing_bytes(paths: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 40).await.unwrap();
    let owned: Vec<String> = paths.iter().map(|s| (*s).to_string()).collect();
    wire::write_strings(&mut buf, &owned).await.unwrap();
    buf
}

/// Build wopNarFromPath (38) client bytes for a given path.
pub async fn build_nar_from_path_bytes(path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 38).await.unwrap();
    wire::write_string(&mut buf, path).await.unwrap();
    buf
}

/// Build wopQueryPathFromHashPart (29) client bytes.
pub async fn build_query_path_from_hash_part_bytes(hash_part: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 29).await.unwrap();
    wire::write_string(&mut buf, hash_part).await.unwrap();
    buf
}

/// Build wopAddSignatures (37) client bytes.
pub async fn build_add_signatures_bytes(path: &str, sigs: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 37).await.unwrap();
    wire::write_string(&mut buf, path).await.unwrap();
    let owned: Vec<String> = sigs.iter().map(|s| (*s).to_string()).collect();
    wire::write_strings(&mut buf, &owned).await.unwrap();
    buf
}

/// Build wopQueryDerivationOutputMap (41) client bytes.
#[allow(dead_code)]
pub async fn build_query_derivation_output_map_bytes(drv_path: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 41).await.unwrap();
    wire::write_string(&mut buf, drv_path).await.unwrap();
    buf
}

/// Parse wopQueryDerivationOutputMap response fields.
///
/// Response: STDERR_LAST + u64(count) + (string name, string path) * count
#[allow(dead_code)]
pub async fn parse_query_derivation_output_map_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut fields = Vec::new();
    let mut cursor = Cursor::new(data.to_vec());

    fields.push(ResponseField {
        name: "stderr_last",
        bytes: read_u64_field(&mut cursor).await,
    });

    // Output count
    let count_bytes = read_u64_field(&mut cursor).await;
    let count = u64::from_le_bytes(count_bytes.clone().try_into().unwrap()) as usize;
    fields.push(ResponseField {
        name: "output_count",
        bytes: count_bytes,
    });

    // Per-output (name, path) pairs
    for i in 0..count {
        let name_label: &'static str = match i {
            0 => "output_0_name",
            1 => "output_1_name",
            2 => "output_2_name",
            _ => "output_n_name",
        };
        let path_label: &'static str = match i {
            0 => "output_0_path",
            1 => "output_1_path",
            2 => "output_2_path",
            _ => "output_n_path",
        };
        fields.push(ResponseField {
            name: name_label,
            bytes: read_string_field(&mut cursor).await,
        });
        fields.push(ResponseField {
            name: path_label,
            bytes: read_string_field(&mut cursor).await,
        });
    }

    fields
}
