//! Synthetic Nix store SQLite database generation.
//!
//! For each build, the worker synthesizes a minimal SQLite database in the
//! overlay upper layer so that Nix recognizes the input paths. The data
//! source is `StoreServiceClient.QueryPathInfo` for each path in the input
// r[impl worker.synth-db.per-build]
// r[impl worker.synth-db.derivation-outputs]
// r[impl worker.synth-db.refs-table]
// r[impl worker.nix.pinned-schema]
//! closure.
//!
//! Schema version 10 (Nix 2.20+). Tables: Config, ValidPaths, Refs,
//! DerivationOutputs, Realisations (empty pre-build by design — nix-daemon
//! INSERTs here post-CA-build; rio never populates — scheduler resolves CA
//! inputs before dispatch per phase5.md).
//!
//! Key conventions:
//! - `registrationTime = 0` for input paths (not locally built)
//! - `ultimate = 0` for input paths (they were not built on this worker)
//! - `PRAGMA journal_mode=WAL` (matching Nix's expectation)
//! - `PRAGMA synchronous=OFF` for speed (DB is ephemeral)

use std::collections::HashMap;
use std::path::Path;

use sqlx::{Connection, SqliteConnection};
use tracing::instrument;

/// Path metadata for synthetic DB insertion.
///
/// This is the subset of `PathInfo` fields needed for the Nix store DB.
/// Populated from `StoreServiceClient.QueryPathInfo` responses.
#[derive(Debug, Clone)]
pub struct SynthPathInfo {
    /// Full store path (e.g. "/nix/store/abc...-hello-1.0").
    pub path: String,
    /// NAR hash in `sha256:<hex>` format.
    pub nar_hash: String,
    /// NAR size in bytes.
    pub nar_size: u64,
    /// Derivation that produced this path (if known).
    pub deriver: Option<String>,
    /// Runtime dependency store paths.
    pub references: Vec<String>,
    /// Cryptographic signatures.
    pub signatures: Vec<String>,
    /// Content address (empty for input-addressed).
    pub ca: Option<String>,
}

/// A derivation's output map entry for the DerivationOutputs table.
///
/// nix-daemon's `queryPartialDerivationOutputMap()` reads this table to
/// determine the expected output paths for a derivation. Without it,
/// `initialOutputs[...].known` is None → `scratchPath = makeFallbackPath()`
/// → the builder writes `$out` to the REAL path but nix-daemon checks the
/// fallback path → "builder failed to produce output path".
///
/// **CA floating outputs** (empty path in the .drv) MUST NOT be represented
/// here. nix-daemon's `queryStaticPartialDerivationOutputMap` does
/// `parseStorePath(row.path)` unconditionally — an empty string aborts the
/// daemon. The executor filters empty-path outputs before constructing these;
/// `insert_drv_outputs` also skips them defensively.
#[derive(Debug, Clone)]
pub struct SynthDrvOutput {
    /// Full .drv store path (must also be in ValidPaths).
    pub drv_path: String,
    /// Output name (e.g., "out", "dev").
    pub output_name: String,
    /// Full output store path. Must be non-empty (CA floating outputs
    /// are not representable here — see struct doc).
    pub output_path: String,
}

impl From<rio_proto::validated::ValidatedPathInfo> for SynthPathInfo {
    fn from(info: rio_proto::validated::ValidatedPathInfo) -> Self {
        let nar_hash = format!("sha256:{}", hex::encode(info.nar_hash));
        SynthPathInfo {
            path: info.store_path.to_string(),
            nar_hash,
            nar_size: info.nar_size,
            deriver: info.deriver.map(|d| d.to_string()),
            references: info.references.into_iter().map(|r| r.to_string()).collect(),
            signatures: info.signatures,
            ca: info.content_address,
        }
    }
}

/// Nix store DB schema version.
///
/// COUPLING WARNING: This must match the schema version expected by the
/// `nix-daemon` binary on the worker. Nix 2.20+ expects version 10. If the
/// worker's Nix version changes, this constant AND the CREATE TABLE statements
/// in `create_schema` may need to be updated together.
const SCHEMA_VERSION: &str = "10";

/// Generate a synthetic Nix store SQLite database at `db_path`.
///
/// The database contains the minimum schema required for Nix 2.20+
/// to recognize store paths: Config, ValidPaths, Refs, DerivationOutputs,
/// and Realisations (empty pre-build by design — nix-daemon writes it
/// post-CA-build; rio never populates).
#[instrument(skip_all, fields(path_count = paths.len(), drv_output_count = drv_outputs.len()))]
pub async fn generate_db(
    db_path: &Path,
    paths: &[SynthPathInfo],
    drv_outputs: &[SynthDrvOutput],
) -> Result<(), sqlx::Error> {
    // sqlx sqlite URI format: sqlite:///absolute/path or sqlite://relative
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let mut conn = SqliteConnection::connect(&url).await?;

    // PRAGMAs must be executed separately (sqlx doesn't support multi-statement strings)
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&mut conn)
        .await?;
    sqlx::query("PRAGMA synchronous=OFF")
        .execute(&mut conn)
        .await?;

    create_schema(&mut conn).await?;
    insert_paths(&mut conn, paths).await?;
    insert_drv_outputs(&mut conn, drv_outputs).await?;

    tracing::info!(
        db_path = %db_path.display(),
        path_count = paths.len(),
        drv_output_count = drv_outputs.len(),
        "synthetic Nix store DB generated"
    );

    Ok(())
}

async fn create_schema(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    // sqlx executes one statement at a time, so issue each CREATE separately.
    let stmts = [
        r#"CREATE TABLE IF NOT EXISTS Config (
            name TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        )"#,
        r#"CREATE TABLE IF NOT EXISTS ValidPaths (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            hash TEXT NOT NULL,
            registrationTime INTEGER NOT NULL,
            deriver TEXT,
            narSize INTEGER,
            ultimate INTEGER DEFAULT 0,
            sigs TEXT DEFAULT '',
            ca TEXT DEFAULT ''
        )"#,
        r#"CREATE TABLE IF NOT EXISTS Refs (
            referrer INTEGER NOT NULL,
            reference INTEGER NOT NULL,
            PRIMARY KEY (referrer, reference),
            FOREIGN KEY (referrer) REFERENCES ValidPaths(id),
            FOREIGN KEY (reference) REFERENCES ValidPaths(id)
        )"#,
        r#"CREATE TABLE IF NOT EXISTS DerivationOutputs (
            drv INTEGER NOT NULL,
            id TEXT NOT NULL,
            path TEXT NOT NULL,
            PRIMARY KEY (drv, id),
            FOREIGN KEY (drv) REFERENCES ValidPaths(id)
        )"#,
        // Realisations: empty pre-build by design — nix-daemon INSERTs
        // here post-CA-build; rio never populates (scheduler resolves CA
        // inputs before dispatch per phase5.md). Schema matches real Nix
        // (outputPath is an INTEGER FK to ValidPaths(id), not a TEXT
        // path — the real Nix daemon joins on the FK).
        r#"CREATE TABLE IF NOT EXISTS Realisations (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            drvPath TEXT NOT NULL,
            outputName TEXT NOT NULL,
            outputPath INTEGER NOT NULL,
            signatures TEXT DEFAULT '',
            FOREIGN KEY (outputPath) REFERENCES ValidPaths(id) ON DELETE CASCADE
        )"#,
        "CREATE INDEX IF NOT EXISTS IndexValidPathsPath ON ValidPaths(path)",
        "CREATE INDEX IF NOT EXISTS IndexValidPathsHash ON ValidPaths(hash)",
        // IndexReferrer/IndexReference: real Nix schema has these. When
        // sandbox=true, nix-daemon walks Refs to compute the closure to
        // bind-mount into the chroot. Without IndexReferrer, a 1000+ path
        // closure walk is O(n²).
        "CREATE INDEX IF NOT EXISTS IndexReferrer ON Refs(referrer)",
        "CREATE INDEX IF NOT EXISTS IndexReference ON Refs(reference)",
    ];

    for stmt in stmts {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn insert_paths(
    conn: &mut SqliteConnection,
    paths: &[SynthPathInfo],
) -> Result<(), sqlx::Error> {
    let mut tx = conn.begin().await?;

    // Set schema version
    sqlx::query("INSERT OR REPLACE INTO Config (name, value) VALUES ('SchemaVersion', ?1)")
        .bind(SCHEMA_VERSION)
        .execute(&mut *tx)
        .await?;

    // Build path -> row ID mapping for references
    let mut path_to_id: HashMap<String, i64> = HashMap::new();

    // Insert all valid paths
    for info in paths {
        let sigs = info.signatures.join(" ");
        let ca = info.ca.as_deref().unwrap_or("");

        // registrationTime=0, ultimate=0 for input paths
        sqlx::query(
            "INSERT OR IGNORE INTO ValidPaths (path, hash, registrationTime, deriver, narSize, ultimate, sigs, ca)
             VALUES (?1, ?2, 0, ?3, ?4, 0, ?5, ?6)",
        )
        .bind(&info.path)
        .bind(&info.nar_hash)
        .bind(&info.deriver)
        .bind(info.nar_size as i64)
        .bind(&sigs)
        .bind(ca)
        .execute(&mut *tx)
        .await?;

        let id: i64 = sqlx::query_scalar("SELECT id FROM ValidPaths WHERE path = ?1")
            .bind(&info.path)
            .fetch_one(&mut *tx)
            .await?;
        path_to_id.insert(info.path.clone(), id);
    }

    // Insert references (critical for sandbox bind-mounts)
    for info in paths {
        let Some(&referrer_id) = path_to_id.get(&info.path) else {
            tracing::error!(
                path = %info.path,
                "referrer path not found in path_to_id after insertion -- this is a bug"
            );
            continue;
        };

        for ref_path in &info.references {
            if let Some(&reference_id) = path_to_id.get(ref_path) {
                sqlx::query("INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?1, ?2)")
                    .bind(referrer_id)
                    .bind(reference_id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                tracing::debug!(
                    referrer = %info.path,
                    reference = %ref_path,
                    "reference path not in closure, skipping"
                );
            }
        }
    }

    tx.commit().await?;
    Ok(())
}

/// Populate the DerivationOutputs table.
///
/// Without this, nix-daemon's `queryPartialDerivationOutputMap(drvPath)`
/// returns empty → `initialOutputs[outputName].known` stays None → nix-daemon
/// builds at `makeFallbackPath()` (a hash of `"rewrite:<drvPath>:name:<out>"`
/// with all-zero content hash) instead of the real output path. The builder's
/// `$out` env var has the REAL path (from the BasicDerivation we send on the
/// wire), so the builder writes there, but nix-daemon checks the fallback path
/// → "builder failed to produce output path".
async fn insert_drv_outputs(
    conn: &mut SqliteConnection,
    drv_outputs: &[SynthDrvOutput],
) -> Result<(), sqlx::Error> {
    if drv_outputs.is_empty() {
        return Ok(());
    }
    let mut tx = conn.begin().await?;
    for out in drv_outputs {
        // drv column is an FK to ValidPaths(id). The .drv must already be
        // in ValidPaths (insert_paths runs first). If it's not found, this
        // is a bug in the caller (closure didn't include the .drv).
        let drv_id: Option<i64> = sqlx::query_scalar("SELECT id FROM ValidPaths WHERE path = ?1")
            .bind(&out.drv_path)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(drv_id) = drv_id else {
            tracing::warn!(
                drv_path = %out.drv_path,
                output = %out.output_name,
                "DerivationOutputs insert skipped: .drv not in ValidPaths (closure bug?)"
            );
            continue;
        };
        // Defensive: CA floating outputs have no path yet. Inserting ""
        // makes nix-daemon's parseStorePath("") abort (core dump). The
        // executor already filters these before calling us; this is a
        // seatbelt for any other caller.
        if out.output_path.is_empty() {
            tracing::warn!(
                drv_path = %out.drv_path,
                output = %out.output_name,
                "DerivationOutputs insert skipped: empty output path (CA floating?)"
            );
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO DerivationOutputs (drv, id, path) VALUES (?1, ?2, ?3)")
            .bind(drv_id)
            .bind(&out.output_name)
            .bind(&out.output_path)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

// r[verify worker.synth-db.per-build]
// r[verify worker.synth-db.derivation-outputs]
// r[verify worker.synth-db.refs-table]
// r[verify worker.nix.pinned-schema]
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_paths() -> Vec<SynthPathInfo> {
        vec![
            SynthPathInfo {
                path: "/nix/store/aaaa-glibc-2.39".to_string(),
                nar_hash: "sha256:deadbeef".to_string(),
                nar_size: 1024,
                deriver: None,
                references: vec!["/nix/store/aaaa-glibc-2.39".to_string()],
                signatures: vec!["cache.nixos.org-1:abc123".to_string()],
                ca: None,
            },
            SynthPathInfo {
                path: "/nix/store/bbbb-hello-2.12.2".to_string(),
                nar_hash: "sha256:cafebabe".to_string(),
                nar_size: 2048,
                deriver: Some("/nix/store/cccc-hello-2.12.2.drv".to_string()),
                references: vec![
                    "/nix/store/aaaa-glibc-2.39".to_string(),
                    "/nix/store/bbbb-hello-2.12.2".to_string(),
                ],
                signatures: vec![],
                ca: None,
            },
        ]
    }

    /// Open a connection to an existing DB file for test assertions.
    async fn open_db(db_path: &Path) -> anyhow::Result<SqliteConnection> {
        let url = format!("sqlite://{}", db_path.display());
        Ok(SqliteConnection::connect(&url).await?)
    }

    #[tokio::test]
    async fn test_generate_db_creates_valid_schema() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths(), &[]).await?;

        let mut conn = open_db(&db_path).await?;

        // Check schema version
        let version: String =
            sqlx::query_scalar("SELECT value FROM Config WHERE name = 'SchemaVersion'")
                .fetch_one(&mut conn)
                .await?;
        assert_eq!(version, "10");

        // Check valid paths count
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 2);

        // Check refs count (glibc self-ref + hello->glibc + hello self-ref)
        let ref_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Refs")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(ref_count, 3);

        // Verify specific FK pairs (referrer.path, reference.path) — not just count
        let pairs: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT vp_referrer.path, vp_reference.path
               FROM Refs r
               JOIN ValidPaths vp_referrer ON r.referrer = vp_referrer.id
               JOIN ValidPaths vp_reference ON r.reference = vp_reference.id
               ORDER BY vp_referrer.path, vp_reference.path"#,
        )
        .fetch_all(&mut conn)
        .await?;

        // Expected: glibc->glibc (self), hello->glibc, hello->hello (self)
        assert!(pairs.contains(&(
            "/nix/store/aaaa-glibc-2.39".to_string(),
            "/nix/store/aaaa-glibc-2.39".to_string()
        )));
        assert!(pairs.contains(&(
            "/nix/store/bbbb-hello-2.12.2".to_string(),
            "/nix/store/aaaa-glibc-2.39".to_string()
        )));
        assert!(pairs.contains(&(
            "/nix/store/bbbb-hello-2.12.2".to_string(),
            "/nix/store/bbbb-hello-2.12.2".to_string()
        )));

        // Verify all required indexes exist (matches real Nix schema).
        // IndexReferrer/IndexReference are critical for nix-daemon's sandbox
        // closure walk: without them, a 1000+ path closure is O(n²).
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'Index%'",
        )
        .fetch_all(&mut conn)
        .await?;
        for expected in [
            "IndexValidPathsPath",
            "IndexValidPathsHash",
            "IndexReferrer",
            "IndexReference",
        ] {
            assert!(
                indexes.iter().any(|i| i == expected),
                "missing index {expected}; got indexes: {indexes:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_derivation_outputs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        // .drv file MUST be in ValidPaths for the FK to resolve.
        let paths = vec![SynthPathInfo {
            path: "/nix/store/xxxx-hello.drv".to_string(),
            nar_hash: "sha256:abcd".to_string(),
            nar_size: 512,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        }];
        let drv_outputs = vec![
            SynthDrvOutput {
                drv_path: "/nix/store/xxxx-hello.drv".to_string(),
                output_name: "out".to_string(),
                output_path: "/nix/store/yyyy-hello".to_string(),
            },
            SynthDrvOutput {
                drv_path: "/nix/store/xxxx-hello.drv".to_string(),
                output_name: "dev".to_string(),
                output_path: "/nix/store/zzzz-hello-dev".to_string(),
            },
        ];

        generate_db(&db_path, &paths, &drv_outputs).await?;

        let mut conn = open_db(&db_path).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT d.id, d.path FROM DerivationOutputs d
               JOIN ValidPaths vp ON d.drv = vp.id
               WHERE vp.path = '/nix/store/xxxx-hello.drv'
               ORDER BY d.id"#,
        )
        .fetch_all(&mut conn)
        .await?;

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            ("dev".to_string(), "/nix/store/zzzz-hello-dev".to_string())
        );
        assert_eq!(
            rows[1],
            ("out".to_string(), "/nix/store/yyyy-hello".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_derivation_outputs_skips_empty_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        // CA floating output: path is "" in the .drv (computed post-build).
        // Inserting "" would make nix-daemon's parseStorePath("") abort.
        // The insert must be SKIPPED.
        let paths = vec![SynthPathInfo {
            path: "/nix/store/xxxx-ca.drv".to_string(),
            nar_hash: "sha256:abcd".to_string(),
            nar_size: 512,
            deriver: None,
            references: vec![],
            signatures: vec![],
            ca: None,
        }];
        let drv_outputs = vec![
            SynthDrvOutput {
                drv_path: "/nix/store/xxxx-ca.drv".to_string(),
                output_name: "out".to_string(),
                output_path: "".to_string(), // CA floating — must skip
            },
            SynthDrvOutput {
                drv_path: "/nix/store/xxxx-ca.drv".to_string(),
                output_name: "dev".to_string(),
                output_path: "/nix/store/yyyy-ca-dev".to_string(), // IA — insert
            },
        ];

        generate_db(&db_path, &paths, &drv_outputs).await?;

        let mut conn = open_db(&db_path).await?;
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, path FROM DerivationOutputs ORDER BY id")
                .fetch_all(&mut conn)
                .await?;
        assert_eq!(rows.len(), 1, "empty-path row must be skipped");
        assert_eq!(
            rows[0],
            ("dev".to_string(), "/nix/store/yyyy-ca-dev".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_derivation_outputs_skips_missing_drv() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        // .drv NOT in ValidPaths → FK resolve fails → warn + skip (not error).
        let drv_outputs = vec![SynthDrvOutput {
            drv_path: "/nix/store/missing.drv".to_string(),
            output_name: "out".to_string(),
            output_path: "/nix/store/some-output".to_string(),
        }];

        generate_db(&db_path, &[], &drv_outputs).await?;

        let mut conn = open_db(&db_path).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM DerivationOutputs")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 0, "should skip insert when .drv not in ValidPaths");
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_has_realisations_table() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[], &[]).await?;

        let mut conn = open_db(&db_path).await?;

        // Realisations table should exist (empty pre-build by design)
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Realisations")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_registration_time_and_ultimate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths(), &[]).await?;

        let mut conn = open_db(&db_path).await?;

        // All input paths should have registrationTime=0 and ultimate=0
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT registrationTime, ultimate FROM ValidPaths")
                .fetch_all(&mut conn)
                .await?;

        for (reg_time, ultimate) in rows {
            assert_eq!(reg_time, 0, "registrationTime must be 0 for input paths");
            assert_eq!(ultimate, 0, "ultimate must be 0 for input paths");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_path_lookup() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths(), &[]).await?;

        let mut conn = open_db(&db_path).await?;

        let hash: String = sqlx::query_scalar("SELECT hash FROM ValidPaths WHERE path = ?1")
            .bind("/nix/store/bbbb-hello-2.12.2")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(hash, "sha256:cafebabe");
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_pragmas() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths(), &[]).await?;

        let mut conn = open_db(&db_path).await?;

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(journal, "wal");
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[], &[]).await?;

        let mut conn = open_db(&db_path).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 0);

        let version: String =
            sqlx::query_scalar("SELECT value FROM Config WHERE name = 'SchemaVersion'")
                .fetch_one(&mut conn)
                .await?;
        assert_eq!(version, "10");
        Ok(())
    }

    #[tokio::test]
    async fn test_generate_db_idempotent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        let paths = sample_paths();
        generate_db(&db_path, &paths, &[]).await?;
        generate_db(&db_path, &paths, &[]).await?;

        let mut conn = open_db(&db_path).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn test_synth_from_validated() -> anyhow::Result<()> {
        use rio_nix::store_path::StorePath;
        use rio_proto::validated::ValidatedPathInfo;
        use rio_test_support::fixtures::{test_drv_path, test_store_path};

        let hello = test_store_path("hello");
        let hello_drv = test_drv_path("hello");
        let glibc = test_store_path("glibc");

        let v = ValidatedPathInfo {
            store_path: StorePath::parse(&hello)?,
            store_path_hash: vec![],
            deriver: Some(StorePath::parse(&hello_drv)?),
            nar_hash: {
                let mut h = [0u8; 32];
                h[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
                h
            },
            nar_size: 1024,
            references: vec![StorePath::parse(&glibc)?],
            registration_time: 0,
            ultimate: false,
            signatures: vec!["sig1".to_string()],
            content_address: None,
        };

        let synth = SynthPathInfo::from(v);
        assert_eq!(synth.path, hello);
        assert_eq!(
            synth.nar_hash,
            "sha256:deadbeef00000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(synth.nar_size, 1024);
        assert_eq!(synth.deriver, Some(hello_drv));
        assert_eq!(synth.references, vec![glibc]);
        assert!(synth.ca.is_none());
        Ok(())
    }
}
