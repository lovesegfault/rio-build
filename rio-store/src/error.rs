//! Metadata/DB-layer error type.
//!
//! Extracted from `metadata/mod.rs` so it compiles with only `sqlx` +
//! `thiserror` — no `bytes`, no `rio_proto`. This lets the `schema`
//! feature (realisations table accessors, used by rio-scheduler) build
//! without pulling the full server dependency tree (aws-sdk-s3, axum,
//! moka, reqwest). The two variants that DO carry server-side types
//! (`CorruptManifest`, `MalformedRow`) are `#[cfg(feature = "server")]`
//! so the lean build drops them.
//!
//! Re-exported from `metadata` for back-compat: existing
//! `metadata::MetadataError` paths keep working when `server` is on.

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

    /// Deadlock detected (PG code 40P01). Two transactions have a
    /// circular lock-wait on overlapping row sets. Retriable — PG
    /// aborted one txn; retry will likely succeed. Prevention: sort
    /// batch-UPDATE input so all writers acquire locks in the same
    /// order (see `metadata::with_sorted_retry`). Maps to `aborted`.
    #[error("deadlock detected (retry)")]
    Deadlock(#[source] sqlx::Error),

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
    #[cfg(feature = "server")]
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
    #[cfg(feature = "server")]
    #[error("malformed narinfo row: {0}")]
    MalformedRow(#[from] rio_proto::validated::PathInfoValidationError),

    /// Backpressure / quota exhaustion: PG pool timeout under load,
    /// signature count cap, or similar capacity conditions. Maps to
    /// `resource_exhausted`. Pool-timeout: retriable (transient). Sig-cap:
    /// per-path permanent — the UPDATE already committed and `DISTINCT`
    /// dedup means retry hits the same cardinality>cap forever (closer
    /// to `FAILED_PRECONDITION` semantics, but mapped here for now).
    ///
    /// Distinct from [`Connection`](Self::Connection): that's "PG
    /// unreachable" (connect failed, TCP reset, TLS error);
    /// this is "PG reachable but at capacity" (pool timed out
    /// waiting for a free connection). The gRPC code is different
    /// — `unavailable` tells clients to try another replica;
    /// `resource_exhausted` tells them to back off.
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

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
    /// - class `57` operator intervention (`57P01` admin_shutdown,
    ///   `57P02` crash_shutdown, `57P03` cannot_connect_now)
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
                Some("40P01") => MetadataError::Deadlock(e),
                // Class-57 operator intervention: PG is restarting/
                // recovering. Arrives as Database(..) (PG sends
                // ErrorResponse before close), NOT Io — without this
                // arm a routine PG rolling restart would surface as
                // non-retriable Internal.
                Some("57P01") | Some("57P02") | Some("57P03") => MetadataError::Connection(e),
                _ => MetadataError::Other(e),
            },
            // PoolTimedOut: all connections checked out, waited
            // pool_timeout for one to free, none did. PG is UP but
            // busy. Client should back off and retry. Distinct from
            // PoolClosed/Io/Tls which mean "can't reach PG at all".
            sqlx::Error::PoolTimedOut => {
                MetadataError::ResourceExhausted("database pool exhausted".into())
            }
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => {
                MetadataError::Connection(e)
            }
            _ => MetadataError::Other(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, MetadataError>;
