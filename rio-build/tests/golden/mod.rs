//! Golden conformance test infrastructure.
//!
//! Provides field-level response parsing and byte comparison
//! for verifying rio-build output against live nix-daemon responses.

pub mod daemon;

use std::io::Cursor;
use std::sync::Arc;

use rio_build::store::MemoryStore;
use rio_nix::hash::NixHash;
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
            rio_build::store::traits::PathInfo {
                path,
                deriver,
                nar_hash,
                references,
                registration_time: entry.registration_time,
                nar_size: entry.nar_size,
                ultimate: entry.ultimate,
                sigs: entry.sigs.clone(),
                ca: entry.ca.clone(),
            },
            None,
        );
    }
    store
}

// ---------------------------------------------------------------------------
// Field-level response parsers
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

// ---------------------------------------------------------------------------
// STDERR activity stripping
// ---------------------------------------------------------------------------

/// Strip STDERR activity messages from an opcode response.
///
/// The real nix-daemon may send STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY,
/// and STDERR_RESULT messages before STDERR_LAST, while rio-build skips
/// those. This function finds the STDERR_LAST marker and returns only the
/// data from that point onward (STDERR_LAST + result fields).
pub fn strip_stderr_activity(data: &[u8]) -> &[u8] {
    let last_bytes = rio_nix::protocol::stderr::STDERR_LAST.to_le_bytes();
    // Search for the 8-byte STDERR_LAST pattern
    for i in 0..data.len().saturating_sub(7) {
        if data[i..i + 8] == last_bytes {
            return &data[i..];
        }
    }
    // If STDERR_LAST not found, return the original data (let the parser handle the error)
    data
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
    (data[..8].to_vec(), data[8..].to_vec())
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// Assert field-by-field byte equality between expected (nix-daemon) and actual
/// (rio-build) responses, skipping fields in the skip list.
///
/// Fields present in one response but not the other are tolerated if the
/// extra fields are all in the skip list.
pub fn assert_field_conformance(
    expected: &[ResponseField],
    actual: &[ResponseField],
    skip: &[String],
) {
    // Filter to non-skipped fields for comparison
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
