//! Golden conformance test infrastructure.
//!
//! Provides field-level response parsing and byte comparison
//! for verifying rio-gateway output against live nix-daemon responses.

pub mod daemon;

use std::io::Cursor;

use rio_nix::hash::NixHash;
use rio_nix::protocol::stderr::STDERR_WRITE;
use rio_test_support::grpc::MockStore;
use rio_test_support::wire_bytes;

/// A single named field from a protocol response, stored as raw bytes.
#[derive(Debug)]
pub struct ResponseField {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

/// A store path entry used to populate the mock gRPC store for conformance tests.
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

/// Seed a `MockStore` with the given store path entries.
///
/// Replaces the rio-build monolith's `build_memory_store_from` which seeded
/// a local `dyn Store` — we seed the gRPC `MockStore` instead. NAR data is
/// fetched via `nix-store --dump` for each entry.
pub fn seed_mock_store_from(store: &MockStore, entries: &[StorePathEntry]) {
    for entry in entries {
        let nar_hash = NixHash::parse(&entry.nar_hash)
            .unwrap_or_else(|e| panic!("invalid nar_hash '{}': {e}", entry.nar_hash));
        let nar_data = daemon::dump_nar(&entry.path);

        let info = rio_proto::types::PathInfo {
            store_path: entry.path.clone(),
            store_path_hash: vec![],
            deriver: entry.deriver.clone().unwrap_or_default(),
            nar_hash: nar_hash.digest().to_vec(),
            nar_size: entry.nar_size,
            references: entry.references.clone(),
            registration_time: entry.registration_time,
            ultimate: entry.ultimate,
            signatures: entry.sigs.clone(),
            content_address: entry.ca.clone().unwrap_or_default(),
        };
        store.seed(info, nar_data);
    }
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

    let mut len_bytes = [0u8; 8];
    cursor.read_exact(&mut len_bytes).await.unwrap();
    let len = u64::from_le_bytes(len_bytes) as usize;

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

/// Read a count-prefixed string collection and return ALL raw bytes.
async fn read_strings_field(cursor: &mut Cursor<Vec<u8>>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let mut count_bytes = [0u8; 8];
    cursor.read_exact(&mut count_bytes).await.unwrap();
    let count = u64::from_le_bytes(count_bytes) as usize;

    let mut result = Vec::new();
    result.extend_from_slice(&count_bytes);

    for _ in 0..count {
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
async fn skip_stderr_activity_message(cursor: &mut Cursor<Vec<u8>>, msg_type: u64) {
    use rio_nix::protocol::stderr::{STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};

    match msg_type {
        STDERR_START_ACTIVITY => {
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
            let _ = read_string_field(cursor).await;
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
            let _ = read_u64_field(cursor).await;
        }
        STDERR_STOP_ACTIVITY => {
            let _ = read_u64_field(cursor).await;
        }
        STDERR_RESULT => {
            let _ = read_u64_field(cursor).await;
            let _ = read_u64_field(cursor).await;
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

pub async fn parse_set_options_fields(data: &[u8]) -> Vec<ResponseField> {
    let mut cursor = Cursor::new(data.to_vec());
    vec![ResponseField {
        name: "stderr_last",
        bytes: read_u64_field(&mut cursor).await,
    }]
}

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

/// Parse a QueryPathInfo response. Handles both found and not-found cases.
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

/// Parse NarFromPath response. Both rio-gateway and nix-daemon now use the
/// same format: [activity messages...] STDERR_LAST <raw NAR bytes>.
pub fn parse_nar_from_path_fields(
    data: &[u8],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ResponseField>> + '_>> {
    Box::pin(async move {
        use rio_nix::protocol::stderr::STDERR_LAST;

        let mut cursor = Cursor::new(data.to_vec());

        // Skip activity messages until STDERR_LAST (or handle STDERR_WRITE
        // for backward compat with older rio-build behavior)
        let last_bytes;
        let mut nar_data = Vec::new();
        loop {
            let msg_bytes = read_u64_field(&mut cursor).await;
            let msg = u64::from_le_bytes(msg_bytes.clone().try_into().unwrap());

            if msg == STDERR_LAST {
                last_bytes = msg_bytes;
                break;
            } else if msg == STDERR_WRITE {
                // Legacy path: rio-build used STDERR_WRITE chunks.
                // rio-gateway now matches daemon (raw NAR after LAST).
                let chunk_field = read_string_field(&mut cursor).await;
                let chunk_len = u64::from_le_bytes(chunk_field[..8].try_into().unwrap()) as usize;
                nar_data.extend_from_slice(&chunk_field[8..8 + chunk_len]);
            } else if is_stderr_activity(msg) {
                skip_stderr_activity_message(&mut cursor, msg).await;
            } else {
                panic!("unexpected STDERR message type {msg:#x} in NarFromPath response");
            }
        }

        // If nar_data is still empty, read raw NAR bytes after STDERR_LAST
        // (the daemon format, and what rio-gateway now does too).
        if nar_data.is_empty() {
            use tokio::io::AsyncReadExt;
            cursor.read_to_end(&mut nar_data).await.unwrap();
        }

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
/// the data from STDERR_LAST onward.
pub async fn strip_stderr_activity(data: &[u8]) -> Vec<u8> {
    use rio_nix::protocol::stderr::STDERR_LAST;

    let mut cursor = Cursor::new(data.to_vec());

    loop {
        let msg_bytes = read_u64_field(&mut cursor).await;
        let msg = u64::from_le_bytes(msg_bytes.clone().try_into().unwrap());

        if msg == STDERR_LAST {
            let pos = cursor.position() as usize;
            let mut result = msg_bytes;
            result.extend_from_slice(&data[pos..]);
            return result;
        } else if is_stderr_activity(msg) {
            skip_stderr_activity_message(&mut cursor, msg).await;
        } else {
            return data.to_vec();
        }
    }
}

// ---------------------------------------------------------------------------
// Response splitting
// ---------------------------------------------------------------------------

/// Split a full protocol response into (handshake_bytes, remaining_bytes).
pub async fn split_handshake(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let fields = parse_handshake_fields(data).await;
    let handshake_len: usize = fields.iter().map(|f| f.bytes.len()).sum();
    (
        data[..handshake_len].to_vec(),
        data[handshake_len..].to_vec(),
    )
}

/// Split remaining bytes after handshake into (set_options_bytes, remaining_bytes).
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
/// (rio-gateway) responses, skipping fields in the skip list.
pub fn assert_field_conformance(
    expected: &[ResponseField],
    actual: &[ResponseField],
    skip: &[&str],
) {
    for fields in [expected, actual] {
        for f in fields.iter().filter(|f| skip.contains(&f.name)) {
            validate_skipped_field(f);
        }
    }

    let exp_filtered: Vec<_> = expected
        .iter()
        .filter(|f| !skip.contains(&f.name))
        .collect();
    let act_filtered: Vec<_> = actual.iter().filter(|f| !skip.contains(&f.name)).collect();

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

fn validate_skipped_field(field: &ResponseField) {
    match field.name {
        "version_string" => {
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
            assert_eq!(
                field.bytes.len(),
                8,
                "trusted field should be exactly 8 bytes"
            );
            let val = u64::from_le_bytes(field.bytes.clone().try_into().unwrap());
            assert!(val <= 2, "trusted field should be 0, 1, or 2, got {val}");
        }
        _ => {}
    }
}

pub fn fields_byte_len(fields: &[ResponseField]) -> usize {
    fields.iter().map(|f| f.bytes.len()).sum()
}

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
// Client byte builders
// ---------------------------------------------------------------------------

pub async fn build_is_valid_path_bytes(path: &str) -> Vec<u8> {
    wire_bytes![u64: 1, string: path]
}

pub async fn build_query_path_info_bytes(path: &str) -> Vec<u8> {
    wire_bytes![u64: 26, string: path]
}

pub async fn build_query_valid_paths_bytes(paths: &[&str], substitute: bool) -> Vec<u8> {
    wire_bytes![u64: 31, strings: paths, bool: substitute]
}

pub async fn build_add_temp_root_bytes(path: &str) -> Vec<u8> {
    wire_bytes![u64: 11, string: path]
}

pub async fn build_query_missing_bytes(paths: &[&str]) -> Vec<u8> {
    wire_bytes![u64: 40, strings: paths]
}

pub async fn build_nar_from_path_bytes(path: &str) -> Vec<u8> {
    wire_bytes![u64: 38, string: path]
}

pub async fn build_query_path_from_hash_part_bytes(hash_part: &str) -> Vec<u8> {
    wire_bytes![u64: 29, string: hash_part]
}

pub async fn build_add_signatures_bytes(path: &str, sigs: &[&str]) -> Vec<u8> {
    wire_bytes![u64: 37, string: path, strings: sigs]
}
