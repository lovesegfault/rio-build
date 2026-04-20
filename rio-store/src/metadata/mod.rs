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

use std::time::Duration;

use bytes::Bytes;
use rio_proto::validated::ValidatedPathInfo;
use tracing::warn;

mod chunked;
mod cluster_key_history;
mod inline;
mod queries;
pub mod tenant_keys;
pub mod upstreams;

// Public API — explicit re-exports so all external callers in grpc/,
// cas.rs keep their `metadata::foo` paths. Kept explicit (not
// `pub use chunked::*` etc.) so dead items in submodules
// surface as `unused` instead of being silently exported.
pub use chunked::{
    PlaceholderToken, complete_manifest_chunked, delete_manifest_chunked_uploading,
    mark_chunks_uploaded, upgrade_manifest_to_chunked,
};
pub use cluster_key_history::load_cluster_key_history;
pub use inline::{check_manifest_complete, complete_manifest_inline, insert_manifest_uploading};
#[cfg(test)]
pub use inline::{delete_manifest_uploading, manifest_uploading_age};
pub use queries::{
    append_signatures, find_missing_paths, get_manifest, get_manifest_batch, query_by_hash_part,
    query_path_info, query_path_info_batch,
};
pub use tenant_keys::get_active_signer;
pub use upstreams::{SigMode, Upstream};

// Error type lives in `crate::error` so the `schema` feature can
// compile it without pulling `bytes`/`rio_proto`. Re-exported here so
// every existing `metadata::MetadataError` / `metadata::Result` path
// keeps working unchanged.
pub use crate::error::{MetadataError, Result};

/// PG 40P01-retry backoff: ~50–150 ms (`100ms ± 50%`). One-shot — no
/// exponential growth (mult=1, single attempt). Just enough to
/// desynchronize two retrying txns so they don't re-collide in
/// lockstep.
const PG_DEADLOCK_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(100),
    mult: 1.0,
    cap: Duration::from_millis(100),
    jitter: rio_common::backoff::Jitter::Proportional(0.5),
};

/// 50–150ms jitter; see [`PG_DEADLOCK_BACKOFF`].
pub(crate) fn jitter() -> Duration {
    PG_DEADLOCK_BACKOFF.duration(0)
}

/// Execute a batch `UPDATE ... WHERE <key> = ANY($1)` with deadlock-safe
/// lock ordering. Sorts the input before binding so all callers acquire
/// PG row locks in the same deterministic order (prevents circular wait
/// → SQLSTATE 40P01). Wraps in a single retry-on-40P01: the sort SHOULD
/// prevent deadlock, but PG can still hit it on index-page splits under
/// extreme contention; one retry is cheap, unbounded retry masks real
/// problems.
///
/// The `body` closure receives the SORTED keys (owned, cloned once per
/// attempt) and must perform the full transaction (begin→UPDATE→commit).
/// On 40P01, the closure is re-invoked after jitter — PG aborts the
/// whole txn on deadlock, not just the failing statement.
///
/// Owned `Vec<Vec<u8>>` (not `&[Vec<u8>]`): the closure returns a
/// `Future` that must own its captures across `.await` points; a slice
/// borrow into the helper's stack would need higher-ranked trait bounds.
/// The one-clone cost (~KB for typical chunk batches) is negligible
/// versus PG roundtrips.
// r[impl store.chunk.lock-order]
pub async fn with_sorted_retry<T, F, Fut>(mut keys: Vec<Vec<u8>>, body: F) -> Result<T>
where
    F: Fn(Vec<Vec<u8>>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    keys.sort_unstable();
    match body(keys.clone()).await {
        Err(MetadataError::Deadlock(e)) => {
            warn!(error = %e, "40P01 on batch UPDATE; retrying once after jitter");
            tokio::time::sleep(jitter()).await;
            body(keys).await
        }
        r => r,
    }
}

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
    Chunked(Vec<([u8; 32], u32)>),
}

impl ManifestKind {
    /// Total NAR size in bytes this manifest will reassemble to.
    ///
    /// Inline = blob length. Chunked = sum of chunk sizes (u64 — see
    /// [`crate::manifest::Manifest::total_size`] for the u32-overflow
    /// rationale). GetPath checks this against `narinfo.nar_size`
    /// before streaming so manifest/narinfo drift fails fast with
    /// DATA_LOSS instead of delivering garbage.
    pub fn total_size(&self) -> u64 {
        match self {
            ManifestKind::Inline(bytes) => bytes.len() as u64,
            ManifestKind::Chunked(entries) => entries.iter().map(|(_, size)| *size as u64).sum(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers — NarinfoRow column list + validation epilogue + UPDATE SQL
//
// These are used by query_path_info and query_by_hash_part. Extracting
// them means adding a column to NarinfoRow requires editing ONE macro,
// not N SELECT strings.
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
    pub(crate) fn try_into_validated(self) -> Result<ValidatedPathInfo> {
        use rio_proto::types::PathInfo;
        // i64 → u64: PG stores nar_size and registration_time as bigint
        // (signed). Both are non-negative by construction (nar_size from a
        // Vec::len(); registration_time from epoch seconds). A negative
        // value is row-level corruption — surface it as InvariantViolation
        // rather than `as u64`-wrapping to a huge value that masquerades
        // as valid downstream.
        let nar_size = u64::try_from(self.nar_size).map_err(|_| {
            MetadataError::InvariantViolation(format!(
                "narinfo.nar_size for {} is negative ({})",
                self.store_path, self.nar_size
            ))
        })?;
        let registration_time = u64::try_from(self.registration_time).map_err(|_| {
            MetadataError::InvariantViolation(format!(
                "narinfo.registration_time for {} is negative ({})",
                self.store_path, self.registration_time
            ))
        })?;
        // Build raw PathInfo then delegate to the centralized TryFrom —
        // keeps validation logic in one place (rio-proto::validated), not
        // duplicated here.
        ValidatedPathInfo::try_from(PathInfo {
            store_path: self.store_path,
            store_path_hash: self.store_path_hash,
            deriver: self.deriver.unwrap_or_default(),
            nar_hash: self.nar_hash,
            nar_size,
            references: self.references,
            registration_time,
            ultimate: self.ultimate,
            signatures: self.signatures,
            content_address: self.ca.unwrap_or_default(),
        })
        .map_err(MetadataError::MalformedRow)
    }
}

/// Convert `Option<NarinfoRow>` → `Result<Option<ValidatedPathInfo>>`.
///
/// Shared epilogue for the three fetch_optional → validate queries.
/// DB-egress validation: a malformed row (garbage store_path, wrong-length
/// nar_hash) would otherwise propagate silently. Caught here at the trust
/// boundary — PG doesn't enforce these as CHECK constraints.
pub(crate) fn validate_row(row: Option<NarinfoRow>) -> Result<Option<ValidatedPathInfo>> {
    row.map(NarinfoRow::try_into_validated).transpose()
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

/// Finalize an upload inside a caller-owned tx/connection: narinfo
/// UPDATE, then flip `manifests.status = 'complete'` (binding
/// `inline_blob` iff `Some`). `PutPathBatch` calls this N times inside
/// one `pool.begin()` for cross-output atomicity; the pool-wrapping
/// [`complete_manifest_inline`] / [`complete_manifest_chunked`] each
/// wrap a single call.
pub async fn complete_manifest_in_conn(
    conn: &mut sqlx::PgConnection,
    info: &ValidatedPathInfo,
    inline_blob: Option<&[u8]>,
) -> Result<()> {
    if update_narinfo_complete(conn, info).await? == 0 {
        // insert_manifest_uploading MUST have run first. If rows_affected
        // is 0, delete_manifest_uploading raced us and won. The caller's
        // placeholder is gone; bailing here prevents a half-complete write.
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }
    // Flip status. inline_blob stays NULL in the chunked case — that's
    // what makes get_manifest() return Chunked instead of Inline.
    let manifest_result = match inline_blob {
        Some(blob) => {
            sqlx::query(
                r#"
                UPDATE manifests SET
                    status      = 'complete',
                    inline_blob = $2,
                    updated_at  = now()
                WHERE store_path_hash = $1
                "#,
            )
            .bind(&info.store_path_hash)
            .bind(blob)
            .execute(&mut *conn)
            .await?
        }
        None => {
            sqlx::query(
                r#"
                UPDATE manifests SET
                    status     = 'complete',
                    updated_at = now()
                WHERE store_path_hash = $1
                "#,
            )
            .bind(&info.store_path_hash)
            .execute(&mut *conn)
            .await?
        }
    };
    if manifest_result.rows_affected() == 0 {
        return Err(MetadataError::PlaceholderMissing {
            store_path: info.store_path.to_string(),
        });
    }
    Ok(())
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
        // PoolTimedOut = PG reachable but all pool connections
        // checked out. Maps to ResourceExhausted (backoff-retry),
        // not Connection (try-another-replica).
        assert!(matches!(e, MetadataError::ResourceExhausted(_)));
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

    /// Real 57P01 admin_shutdown → Connection (retriable, NOT Other).
    /// PG sends class-57 as `ErrorResponse` on the wire (→
    /// `sqlx::Error::Database`, not `Io`); without the explicit match
    /// arm a routine PG rolling restart surfaces as non-retriable
    /// `Internal`. PL/pgSQL `RAISE … USING ERRCODE` produces a real
    /// `Database` error carrying the SQLSTATE.
    #[tokio::test]
    async fn integration_admin_shutdown_is_connection() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        for code in ["57P01", "57P02", "57P03"] {
            let err: MetadataError = sqlx::query(&format!(
                "DO $$ BEGIN RAISE EXCEPTION 'shutdown' USING ERRCODE = '{code}'; END $$"
            ))
            .execute(&db.pool)
            .await
            .unwrap_err()
            .into();

            assert!(
                matches!(err, MetadataError::Connection(_)),
                "expected Connection for {code}, got {err:?}"
            );
        }
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
                MetadataError::Deadlock(sqlx::Error::PoolClosed),
                Code::Aborted,
            ),
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
            (
                MetadataError::CorruptManifest {
                    store_path: "/nix/store/x".into(),
                    source: crate::manifest::ManifestError::Empty,
                },
                Code::DataLoss,
            ),
            (
                MetadataError::MalformedRow(
                    rio_proto::validated::PathInfoValidationError::StorePath {
                        path: "bad".into(),
                        source: rio_nix::store_path::StorePathError::TooShort,
                    },
                ),
                Code::Internal,
            ),
            (
                MetadataError::ResourceExhausted("quota".into()),
                Code::ResourceExhausted,
            ),
            (
                MetadataError::RealisationConflict {
                    drv_hash: "ab".into(),
                    output_name: "out".into(),
                    existing: "/nix/store/a".into(),
                    attempted: "/nix/store/b".into(),
                },
                Code::AlreadyExists,
            ),
        ];
        for (err, expected_code) in cases {
            // MetadataError isn't Clone; reconstruct for the call.
            let code = metadata_status("test", clone_for_test(err)).code();
            assert_eq!(
                code, *expected_code,
                "wrong code for {err:?}: got {code:?}, expected {expected_code:?}"
            );
        }
    }

    /// upgrade_manifest_to_chunked's ON CONFLICT upsert must clear
    /// `deleted=false` when resurrecting a chunk. Without this,
    /// PutPath bumps refcount but leaves deleted=true → chunks row
    /// is inconsistent (refcount>0 but marked deleted). The drain
    /// re-check catches it either way, but self-consistent row state
    /// makes the chunks table correct on its own.
    #[tokio::test]
    async fn integration_chunked_upsert_clears_deleted() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed a "sweep just marked me dead" chunk: refcount=0,
        // deleted=true.
        let chunk_hash = vec![0xEEu8; 32];
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, deleted) \
             VALUES ($1, 0, 100, true)",
        )
        .bind(&chunk_hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // Set up placeholder for upgrade_manifest_to_chunked (requires
        // existing 'uploading' manifests row, which requires narinfo).
        let store_path_hash = vec![0xDDu8; 32];
        insert_manifest_uploading(&db.pool, &store_path_hash, "/nix/store/d-dummy", &[])
            .await
            .unwrap();

        // Upgrade with a chunk_list referencing our dead chunk.
        // Minimal Manifest: one entry. The upsert should bump
        // refcount 0→1 AND clear deleted→false.
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0xEEu8; 32],
                size: 100,
            }],
        };
        let _ = chunked::upgrade_manifest_to_chunked(
            &db.pool,
            &store_path_hash,
            &manifest.serialize(),
            std::slice::from_ref(&chunk_hash),
            &[100i64],
        )
        .await
        .unwrap();

        // Verify: refcount=1, deleted=false. refcount is PG INTEGER → i32.
        let (refcount, deleted): (i32, bool) =
            sqlx::query_as("SELECT refcount, deleted FROM chunks WHERE blake3_hash = $1")
                .bind(&chunk_hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(refcount, 1, "upsert bumped refcount");
        assert!(!deleted, "upsert cleared deleted=false (chunk resurrected)");
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
            MetadataError::Deadlock(_) => MetadataError::Deadlock(sqlx::Error::PoolClosed),
            MetadataError::PlaceholderMissing { store_path } => MetadataError::PlaceholderMissing {
                store_path: store_path.clone(),
            },
            MetadataError::InvariantViolation(s) => MetadataError::InvariantViolation(s.clone()),
            MetadataError::CorruptManifest { store_path, .. } => MetadataError::CorruptManifest {
                store_path: store_path.clone(),
                source: crate::manifest::ManifestError::Empty,
            },
            MetadataError::MalformedRow(_) => MetadataError::MalformedRow(
                rio_proto::validated::PathInfoValidationError::StorePath {
                    path: "x".into(),
                    source: rio_nix::store_path::StorePathError::TooShort,
                },
            ),
            MetadataError::ResourceExhausted(s) => MetadataError::ResourceExhausted(s.clone()),
            MetadataError::Other(_) => MetadataError::Other(sqlx::Error::RowNotFound),
            MetadataError::RealisationConflict {
                drv_hash,
                output_name,
                existing,
                attempted,
            } => MetadataError::RealisationConflict {
                drv_hash: drv_hash.clone(),
                output_name: output_name.clone(),
                existing: existing.clone(),
                attempted: attempted.clone(),
            },
        }
    }
}
