//! Store path metadata persistence in PostgreSQL.
//!
//! CRUD operations for the `narinfo` and `manifests` tables defined in
//! `migrations/002_store.sql`.
//!
//! # Storage model (phase 2c)
//!
//! NAR content lives in one of two places, determined by `manifests.inline_blob`:
//!
//! - **Inline** (`inline_blob IS NOT NULL`): whole NAR stored directly in the
//!   manifests row. No `manifest_data` row. Used for small NARs (<256KB by
//!   default).
//! - **Chunked** (`inline_blob IS NULL`): NAR split by FastCDC; chunks in S3;
//!   `manifest_data.chunk_list` holds the ordered (blake3, size) list.
//!
//! This invariant (`inline_blob IS NOT NULL <=> no manifest_data row`) is
//! enforced by code, not by a CHECK constraint — PG can't express "row in
//! another table must not exist".
//!
//! # Write-ahead pattern
//!
//! 1. `insert_manifest_uploading()` — writes placeholder narinfo + manifest
//!    with `status='uploading'`. Protects the upload from concurrent GC.
//! 2. Caller writes inline_blob or uploads chunks.
//! 3. `complete_manifest_inline()` / `complete_manifest_chunked()` — fills
//!    real narinfo metadata + flips `status='complete'` atomically.
//!
//! On failure between 1 and 3, `delete_manifest_uploading()` reclaims the
//! placeholder. It only touches rows where `nar_size = 0` (the placeholder
//! marker — real NARs are always >0), so it's safe even if a concurrent
//! upload already succeeded.
//!
//! `query_path_info()` and `find_missing_paths()` filter on
//! `manifests.status = 'complete'`, so placeholders are never exposed.

use bytes::Bytes;
use rio_proto::validated::{PathInfoValidationError, ValidatedPathInfo};

mod chunked;
mod inline;
mod queries;

// Public API — re-exports so all external callers in grpc/, cas.rs,
// cache_server.rs, content_index.rs keep their `metadata::foo` paths.
pub use chunked::*;
pub use inline::*;
pub use queries::*;

/// Typed error for the metadata/DB layer. Replaces `anyhow::Result` so
/// callers can discriminate retriable failures (connection, serialization)
/// from permanent ones (corruption) and map to precise gRPC status codes.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// Row not found. From `sqlx::Error::RowNotFound`. Most metadata
    /// queries use `fetch_optional` so this is rare; shows up on
    /// `fetch_one` sites.
    #[error("not found")]
    NotFound,

    /// Unique or FK constraint violation (PG codes 23505, 23503).
    /// Usually means a concurrent writer won the race — the caller's
    /// operation is a no-op, not a failure. Maps to `already_exists`.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Connection-level failure: pool exhausted, TCP reset, TLS error.
    /// Retriable — the operation never reached PG. Maps to `unavailable`.
    #[error("connection error: {0}")]
    Connection(#[source] sqlx::Error),

    /// Serialization failure (PG code 40001). Two transactions conflicted
    /// under REPEATABLE READ or SERIALIZABLE isolation. Retriable — the
    /// transaction was aborted cleanly, retry will likely succeed.
    /// Maps to `aborted`.
    #[error("serialization failure (retry)")]
    Serialization,

    /// The write-ahead placeholder from `insert_manifest_uploading` is
    /// gone — `delete_manifest_uploading` raced us and won, or a crashed
    /// previous upload already cleaned it up. The caller's UPDATE hit
    /// `rows_affected() == 0`. Retriable. Maps to `aborted`.
    #[error("placeholder missing for {store_path} (concurrently deleted?)")]
    PlaceholderMissing { store_path: String },

    /// Database state violates a code-enforced invariant that PG's
    /// schema can't express (e.g., `inline_blob IS NULL` but no
    /// `manifest_data` row — PG can't CHECK "row in another table
    /// exists"). This is corruption: manual DB surgery, a CASCADE we
    /// didn't expect, or a bug in cleanup ordering. NOT retriable.
    /// Maps to `internal`.
    #[error("invariant violation: {0}")]
    InvariantViolation(String),

    /// A `manifest_data.chunk_list` blob failed to deserialize.
    /// Written by us, read by us — if it doesn't round-trip, either
    /// the write was torn or the format version is wrong. NOT
    /// retriable. Maps to `data_loss`.
    #[error("corrupt manifest_data for {store_path}: {source}")]
    CorruptManifest {
        store_path: String,
        #[source]
        source: crate::manifest::ManifestError,
    },

    /// A narinfo row failed validation (bad store_path, wrong-length
    /// nar_hash). PG's schema doesn't enforce these as CHECK
    /// constraints. Caught at the egress boundary. NOT retriable.
    /// Maps to `internal`.
    #[error("malformed narinfo row: {0}")]
    MalformedRow(#[from] PathInfoValidationError),

    /// Unclassified sqlx error. Maps to `internal`.
    #[error("database error: {0}")]
    Other(#[source] sqlx::Error),
}

impl From<sqlx::Error> for MetadataError {
    /// Classify sqlx errors by their PostgreSQL SQLSTATE code.
    ///
    /// Codes per the PG docs, Appendix A:
    /// - `23505` unique_violation, `23503` foreign_key_violation
    /// - `40001` serialization_failure
    ///
    /// Connection-level errors (`Io`, `Tls`, `PoolTimedOut`, `PoolClosed`)
    /// are distinguished from query-level errors so callers can retry with
    /// backoff instead of propagating as internal.
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => MetadataError::NotFound,
            sqlx::Error::Database(db_err) => match db_err.code().as_deref() {
                Some("23505") | Some("23503") => {
                    MetadataError::Conflict(db_err.message().to_string())
                }
                Some("40001") => MetadataError::Serialization,
                _ => MetadataError::Other(e),
            },
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed => MetadataError::Connection(e),
            _ => MetadataError::Other(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, MetadataError>;

/// How a NAR's content is stored. Returned by [`get_manifest`].
///
/// This is the one place callers branch on inline-vs-chunked. GetPath reads
/// this; the binary cache HTTP server reads this; future GC reads this.
/// Encapsulating the branch here means the "check inline_blob FIRST, only
/// then query manifest_data" rule lives in exactly one SQL query.
#[derive(Debug)]
pub enum ManifestKind {
    /// Whole NAR stored in `manifests.inline_blob`.
    Inline(Bytes),
    /// NAR chunked; reassemble from this ordered list.
    /// Each entry is `(blake3_digest, chunk_size_bytes)`.
    ///
    /// E1 returns an empty Vec here (chunking lands in C3) — this is
    /// future-proofing the return type so GetPath doesn't need a second
    /// rewrite when chunking lands.
    Chunked(Vec<([u8; 32], u32)>),
}

// ---------------------------------------------------------------------------
// Shared helpers — NarinfoRow column list + validation epilogue + UPDATE SQL
//
// These three are used by query_path_info, query_by_hash_part, and
// content_index::lookup. Extracting them means adding a column to
// NarinfoRow requires editing ONE macro, not 3 SELECT strings.
// ---------------------------------------------------------------------------

/// Expands to the 10-column SELECT list for `NarinfoRow`, aliased `n.*`.
/// A macro (not a const) so `concat!` can embed it in query literals —
/// `concat!` only accepts literal tokens, not const expressions, and we
/// want compile-time strings (no per-query `format!` alloc).
#[macro_export]
#[doc(hidden)]
macro_rules! narinfo_cols {
    () => {
        r#"n.store_path, n.store_path_hash, n.deriver, n.nar_hash, n.nar_size,
           n."references", n.signatures, n.ca, n.registration_time, n.ultimate"#
    };
}

#[derive(sqlx::FromRow)]
pub(crate) struct NarinfoRow {
    store_path: String,
    store_path_hash: Vec<u8>,
    deriver: Option<String>,
    nar_hash: Vec<u8>,
    nar_size: i64,
    references: Vec<String>,
    signatures: Vec<String>,
    ca: Option<String>,
    registration_time: i64,
    ultimate: bool,
}

impl NarinfoRow {
    pub(crate) fn try_into_validated(
        self,
    ) -> std::result::Result<ValidatedPathInfo, PathInfoValidationError> {
        use rio_proto::types::PathInfo;
        // Build raw PathInfo then delegate to the centralized TryFrom —
        // keeps validation logic in one place (rio-proto::validated), not
        // duplicated here.
        ValidatedPathInfo::try_from(PathInfo {
            store_path: self.store_path,
            store_path_hash: self.store_path_hash,
            deriver: self.deriver.unwrap_or_default(),
            nar_hash: self.nar_hash,
            nar_size: self.nar_size as u64,
            references: self.references,
            // Now actually roundtrip (was 0/false before phase2c).
            // `as u64` cast: registration_time is Unix epoch seconds,
            // non-negative in practice. A negative value in the DB would
            // be corruption; the cast wraps, which is detectable downstream.
            registration_time: self.registration_time as u64,
            ultimate: self.ultimate,
            signatures: self.signatures,
            content_address: self.ca.unwrap_or_default(),
        })
    }
}

/// Convert `Option<NarinfoRow>` → `Result<Option<ValidatedPathInfo>>`.
///
/// Shared epilogue for the three fetch_optional → validate queries.
/// DB-egress validation: a malformed row (garbage store_path, wrong-length
/// nar_hash) would otherwise propagate silently. Caught here at the trust
/// boundary — PG doesn't enforce these as CHECK constraints.
pub(crate) fn validate_row(row: Option<NarinfoRow>) -> Result<Option<ValidatedPathInfo>> {
    row.map(NarinfoRow::try_into_validated)
        .transpose()
        .map_err(MetadataError::MalformedRow)
}

/// Fill the real narinfo fields (replacing placeholder zeros).
///
/// Shared by `complete_manifest_inline` and `complete_manifest_chunked` —
/// both do the exact same 9-column UPDATE before their manifest-table
/// operation diverges. Returns rows_affected so callers can check for
/// the placeholder-raced-away case.
pub(super) async fn update_narinfo_complete(
    tx: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
) -> std::result::Result<u64, sqlx::Error> {
    let deriver_str = info.deriver.as_ref().map(|d| d.to_string());
    let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
    let ca_str = info.content_address.as_deref();

    sqlx::query(
        r#"
        UPDATE narinfo SET
            deriver           = $2,
            nar_hash          = $3,
            nar_size          = $4,
            "references"      = $5,
            signatures        = $6,
            ca                = $7,
            registration_time = $8,
            ultimate          = $9
        WHERE store_path_hash = $1
        "#,
    )
    .bind(&info.store_path_hash)
    .bind(deriver_str)
    .bind(info.nar_hash.as_slice())
    .bind(info.nar_size as i64)
    .bind(&refs_str)
    .bind(&info.signatures)
    .bind(ca_str)
    .bind(info.registration_time as i64)
    .bind(info.ultimate)
    .execute(&mut *tx)
    .await
    .map(|r| r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    // =======================================================================
    // MetadataError classification (From<sqlx::Error>)
    // =======================================================================

    #[test]
    fn classify_row_not_found() {
        let e: MetadataError = sqlx::Error::RowNotFound.into();
        assert!(matches!(e, MetadataError::NotFound));
    }

    #[test]
    fn classify_pool_timed_out() {
        let e: MetadataError = sqlx::Error::PoolTimedOut.into();
        assert!(matches!(e, MetadataError::Connection(_)));
    }

    #[test]
    fn classify_pool_closed() {
        let e: MetadataError = sqlx::Error::PoolClosed.into();
        assert!(matches!(e, MetadataError::Connection(_)));
    }

    /// Decode errors, column-type mismatches, protocol weirdness —
    /// anything not explicitly classified lands in Other.
    #[test]
    fn classify_unknown_falls_through_to_other() {
        let e: MetadataError = sqlx::Error::ColumnNotFound("x".into()).into();
        assert!(matches!(e, MetadataError::Other(_)));
    }

    // =======================================================================
    // Integration: trigger real PG SQLSTATE codes, assert classification
    // =======================================================================

    /// Real 23505 unique_violation → Conflict. Insert the same PK twice.
    #[tokio::test]
    async fn integration_unique_violation_is_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let hash = vec![0xAAu8; 32];

        // First insert: OK.
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, '/nix/store/a-x', $2, 0)",
        )
        .bind(&hash)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();

        // Second insert on same PK: 23505.
        let err: MetadataError = sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, '/nix/store/a-x', $2, 0)",
        )
        .bind(&hash)
        .bind(vec![0u8; 32])
        .execute(&db.pool)
        .await
        .unwrap_err()
        .into();

        assert!(
            matches!(err, MetadataError::Conflict(_)),
            "expected Conflict for 23505 unique_violation, got {err:?}"
        );
    }

    /// Real 23503 foreign_key_violation → Conflict. Insert a manifests
    /// row whose store_path_hash FK doesn't exist in narinfo.
    #[tokio::test]
    async fn integration_fk_violation_is_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let err: MetadataError = sqlx::query("INSERT INTO manifests (store_path_hash) VALUES ($1)")
            .bind(vec![0xBBu8; 32]) // no matching narinfo row
            .execute(&db.pool)
            .await
            .unwrap_err()
            .into();

        assert!(
            matches!(err, MetadataError::Conflict(_)),
            "expected Conflict for 23503 foreign_key_violation, got {err:?}"
        );
    }

    /// PlaceholderMissing: call complete_manifest_inline WITHOUT
    /// insert_manifest_uploading first. rows_affected() == 0 on both
    /// UPDATEs → PlaceholderMissing, NOT a sqlx error.
    #[tokio::test]
    async fn integration_complete_without_placeholder_is_placeholder_missing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let info = rio_test_support::fixtures::make_path_info(
            &rio_test_support::fixtures::test_store_path("noplaceholder"),
            b"nar",
            [0xCCu8; 32],
        );

        let err = complete_manifest_inline(&db.pool, &info, Bytes::from_static(b"nar"))
            .await
            .expect_err("should fail without placeholder");

        assert!(
            matches!(err, MetadataError::PlaceholderMissing { .. }),
            "expected PlaceholderMissing, got {err:?}"
        );
    }

    // =======================================================================
    // metadata_status gRPC mapping — verified via the actual mapper
    // =======================================================================

    /// Verifies the grpc.rs metadata_status function produces the right
    /// codes. Not just the From<sqlx::Error> classification — the
    /// full chain: sqlx error → MetadataError variant → tonic::Code.
    #[test]
    fn grpc_status_code_mapping() {
        use crate::grpc::metadata_status;
        use tonic::Code;

        let cases: &[(MetadataError, Code)] = &[
            (MetadataError::NotFound, Code::NotFound),
            (MetadataError::Conflict("dup".into()), Code::AlreadyExists),
            (
                MetadataError::Connection(sqlx::Error::PoolClosed),
                Code::Unavailable,
            ),
            (MetadataError::Serialization, Code::Aborted),
            (
                MetadataError::PlaceholderMissing {
                    store_path: "/nix/store/x".into(),
                },
                Code::Aborted,
            ),
            (
                MetadataError::InvariantViolation("x".into()),
                Code::Internal,
            ),
            (
                MetadataError::Other(sqlx::Error::RowNotFound),
                Code::Internal,
            ),
        ];
        for (err, expected_code) in cases {
            // MetadataError isn't Clone; reconstruct for the call.
            // (We move out of the match-tuple via shadowing.)
            let code = metadata_status("test", clone_for_test(err)).code();
            assert_eq!(
                code, *expected_code,
                "wrong code for {err:?}: got {code:?}, expected {expected_code:?}"
            );
        }
    }

    /// Test-only shallow clone. MetadataError can't derive Clone (holds
    /// sqlx::Error which isn't Clone); this reconstructs equivalent
    /// variants for the mapping test above.
    fn clone_for_test(e: &MetadataError) -> MetadataError {
        match e {
            MetadataError::NotFound => MetadataError::NotFound,
            MetadataError::Conflict(s) => MetadataError::Conflict(s.clone()),
            MetadataError::Connection(_) => MetadataError::Connection(sqlx::Error::PoolClosed),
            MetadataError::Serialization => MetadataError::Serialization,
            MetadataError::PlaceholderMissing { store_path } => MetadataError::PlaceholderMissing {
                store_path: store_path.clone(),
            },
            MetadataError::InvariantViolation(s) => MetadataError::InvariantViolation(s.clone()),
            MetadataError::CorruptManifest { .. } => unreachable!("not in test cases"),
            MetadataError::MalformedRow(_) => unreachable!("not in test cases"),
            MetadataError::Other(_) => MetadataError::Other(sqlx::Error::RowNotFound),
        }
    }
}
