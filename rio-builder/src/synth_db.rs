//! Synthetic Nix store SQLite database generation.
//!
//! For each build, the worker synthesizes a minimal SQLite database in the
//! overlay upper layer so that Nix recognizes the input paths. The data
//! source is `StoreServiceClient.QueryPathInfo` for each path in the input
// r[impl builder.synth-db.per-build]
// r[impl builder.synth-db.derivation-outputs]
// r[impl builder.synth-db.refs-table]
// r[impl builder.nix.pinned-schema]
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

use rio_proto::validated::ValidatedPathInfo;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};
use tracing::instrument;

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
    paths: &[ValidatedPathInfo],
    drv_outputs: &[SynthDrvOutput],
) -> Result<(), sqlx::Error> {
    // SqliteConnectOptions::filename takes the path verbatim — no URI parsing.
    // Defense-in-depth for I-167: drv names can contain `?` which a
    // `sqlite://{path}?mode=rwc` string would mis-parse as a query param.
    // sanitize_build_id keeps such bytes out of the overlay dir name, but the
    // db_path is caller-controlled and this layer should not assume that.
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&opts).await?;

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
    paths: &[ValidatedPathInfo],
) -> Result<(), sqlx::Error> {
    let mut tx = conn.begin().await?;

    // Set schema version
    sqlx::query("INSERT OR REPLACE INTO Config (name, value) VALUES ('SchemaVersion', ?1)")
        .bind(SCHEMA_VERSION)
        .execute(&mut *tx)
        .await?;

    // Build path -> row ID mapping for references
    let mut path_to_id: HashMap<&str, i64> = HashMap::new();

    // Insert all valid paths
    for info in paths {
        let path = info.store_path.as_str();
        // Nix's ValidPaths.hash column is `sha256:<hex>`.
        let nar_hash = format!("sha256:{}", hex::encode(info.nar_hash));
        let deriver = info.deriver.as_ref().map(|d| d.to_string());
        let sigs = info.signatures.join(" ");
        let ca = info.content_address.as_deref().unwrap_or("");

        // registrationTime=0, ultimate=0 for input paths
        sqlx::query(
            "INSERT OR IGNORE INTO ValidPaths (path, hash, registrationTime, deriver, narSize, ultimate, sigs, ca)
             VALUES (?1, ?2, 0, ?3, ?4, 0, ?5, ?6)",
        )
        .bind(path)
        .bind(&nar_hash)
        .bind(&deriver)
        .bind(info.nar_size as i64)
        .bind(&sigs)
        .bind(ca)
        .execute(&mut *tx)
        .await?;

        let id: i64 = sqlx::query_scalar("SELECT id FROM ValidPaths WHERE path = ?1")
            .bind(path)
            .fetch_one(&mut *tx)
            .await?;
        path_to_id.insert(path, id);
    }

    // Insert references (critical for sandbox bind-mounts)
    for info in paths {
        let path = info.store_path.as_str();
        let Some(&referrer_id) = path_to_id.get(path) else {
            tracing::error!(
                path = %path,
                "referrer path not found in path_to_id after insertion -- this is a bug"
            );
            continue;
        };

        for ref_path in &info.references {
            if let Some(&reference_id) = path_to_id.get(ref_path.as_str()) {
                sqlx::query("INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?1, ?2)")
                    .bind(referrer_id)
                    .bind(reference_id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                tracing::debug!(
                    referrer = %path,
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

// r[verify builder.synth-db.per-build]
// r[verify builder.synth-db.derivation-outputs]
// r[verify builder.synth-db.refs-table]
// r[verify builder.nix.pinned-schema]
#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::store_path::StorePath;

    const P_GLIBC: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc-2.39";
    const P_HELLO: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-hello-2.12.2";
    const P_HELLO_DRV: &str = "/nix/store/cccccccccccccccccccccccccccccccc-hello-2.12.2.drv";
    const P_DRV: &str = "/nix/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-hello.drv";

    fn sp(s: &str) -> StorePath {
        StorePath::parse(s).unwrap()
    }

    fn vpi(path: &str, nar_hash: [u8; 32]) -> ValidatedPathInfo {
        ValidatedPathInfo {
            store_path: sp(path),
            store_path_hash: vec![],
            deriver: None,
            nar_hash,
            nar_size: 0,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        }
    }

    fn sample_paths() -> Vec<ValidatedPathInfo> {
        let mut glibc = vpi(P_GLIBC, [0xde; 32]);
        glibc.nar_size = 1024;
        glibc.references = vec![sp(P_GLIBC)];
        glibc.signatures = vec!["cache.nixos.org-1:abc123".to_string()];

        let mut hello = vpi(P_HELLO, [0xca; 32]);
        hello.nar_size = 2048;
        hello.deriver = Some(sp(P_HELLO_DRV));
        hello.references = vec![sp(P_GLIBC), sp(P_HELLO)];

        vec![glibc, hello]
    }

    /// Open a connection to an existing DB file for test assertions.
    async fn open_db(db_path: &Path) -> anyhow::Result<SqliteConnection> {
        let opts = SqliteConnectOptions::new().filename(db_path);
        Ok(SqliteConnection::connect_with(&opts).await?)
    }

    /// I-167 regression: a `?` in a parent directory component used to be
    /// parsed as the start of the sqlite:// query string. With
    /// SqliteConnectOptions::filename the path is taken verbatim.
    #[tokio::test]
    async fn test_generate_db_path_with_query_char() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let weird = dir.path().join("upper?id=deadbeef");
        std::fs::create_dir_all(&weird)?;
        let db_path = weird.join("db.sqlite");

        generate_db(&db_path, &sample_paths(), &[]).await?;

        assert!(db_path.exists(), "db file should exist at {db_path:?}");
        let mut conn = open_db(&db_path).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await?;
        assert_eq!(count, 2);
        Ok(())
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
        assert!(pairs.contains(&(P_GLIBC.to_string(), P_GLIBC.to_string())));
        assert!(pairs.contains(&(P_HELLO.to_string(), P_GLIBC.to_string())));
        assert!(pairs.contains(&(P_HELLO.to_string(), P_HELLO.to_string())));

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
        let paths = vec![vpi(P_DRV, [0xab; 32])];
        let drv_outputs = vec![
            SynthDrvOutput {
                drv_path: P_DRV.to_string(),
                output_name: "out".to_string(),
                output_path: "/nix/store/yyyy-hello".to_string(),
            },
            SynthDrvOutput {
                drv_path: P_DRV.to_string(),
                output_name: "dev".to_string(),
                output_path: "/nix/store/zzzz-hello-dev".to_string(),
            },
        ];

        generate_db(&db_path, &paths, &drv_outputs).await?;

        let mut conn = open_db(&db_path).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT d.id, d.path FROM DerivationOutputs d
               JOIN ValidPaths vp ON d.drv = vp.id
               WHERE vp.path = ?1
               ORDER BY d.id"#,
        )
        .bind(P_DRV)
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
        let paths = vec![vpi(P_DRV, [0xab; 32])];
        let drv_outputs = vec![
            SynthDrvOutput {
                drv_path: P_DRV.to_string(),
                output_name: "out".to_string(),
                output_path: "".to_string(), // CA floating — must skip
            },
            SynthDrvOutput {
                drv_path: P_DRV.to_string(),
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
            .bind(P_HELLO)
            .fetch_one(&mut conn)
            .await?;
        // ValidPaths.hash is `sha256:<hex>` (Nix's format).
        assert_eq!(hash, format!("sha256:{}", hex::encode([0xca; 32])));
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

    /// `ValidatedPathInfo` → ValidPaths row: deriver/sigs/ca columns are
    /// stringified at the bind site (the only conversion sink).
    #[tokio::test]
    async fn test_validated_to_row_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("db.sqlite");

        let mut info = vpi(P_HELLO, [0u8; 32]);
        info.deriver = Some(sp(P_HELLO_DRV));
        info.signatures = vec!["sig1".to_string(), "sig2".to_string()];
        info.content_address = Some("fixed:sha256:abc".to_string());
        generate_db(&db_path, &[info], &[]).await?;

        let mut conn = open_db(&db_path).await?;
        let (deriver, sigs, ca): (Option<String>, String, String) =
            sqlx::query_as("SELECT deriver, sigs, ca FROM ValidPaths WHERE path = ?1")
                .bind(P_HELLO)
                .fetch_one(&mut conn)
                .await?;
        assert_eq!(deriver.as_deref(), Some(P_HELLO_DRV));
        assert_eq!(sigs, "sig1 sig2");
        assert_eq!(ca, "fixed:sha256:abc");
        Ok(())
    }
}
