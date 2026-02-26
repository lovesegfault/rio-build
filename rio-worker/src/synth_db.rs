//! Synthetic Nix store SQLite database generation.
//!
//! For each build, the worker synthesizes a minimal SQLite database in the
//! overlay upper layer so that Nix recognizes the input paths. The data
//! source is `StoreServiceClient.QueryPathInfo` for each path in the input
//! closure.
//!
//! Schema version 10 (Nix 2.20+). Tables: Config, ValidPaths, Refs,
//! DerivationOutputs, Realisations (empty, for future CA support).
//!
//! Key conventions:
//! - `registrationTime = 0` for input paths (not locally built)
//! - `ultimate = 0` for input paths (they were not built on this worker)
//! - `PRAGMA journal_mode=WAL` (matching Nix's expectation)
//! - `PRAGMA synchronous=OFF` for speed (DB is ephemeral)

use std::collections::HashMap;
use std::path::Path;

use sqlx::{Connection, SqliteConnection};

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

/// Convert a `PathInfo` protobuf message to `SynthPathInfo`.
pub fn path_info_to_synth(info: &rio_proto::types::PathInfo) -> SynthPathInfo {
    let nar_hash = format!("sha256:{}", hex::encode(&info.nar_hash));
    SynthPathInfo {
        path: info.store_path.clone(),
        nar_hash,
        nar_size: info.nar_size,
        deriver: if info.deriver.is_empty() {
            None
        } else {
            Some(info.deriver.clone())
        },
        references: info.references.clone(),
        signatures: info.signatures.clone(),
        ca: if info.content_address.is_empty() {
            None
        } else {
            Some(info.content_address.clone())
        },
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
/// and Realisations (empty, for future CA support).
pub async fn generate_db(db_path: &Path, paths: &[SynthPathInfo]) -> anyhow::Result<()> {
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

    tracing::info!(
        db_path = %db_path.display(),
        path_count = paths.len(),
        "synthetic Nix store DB generated"
    );

    Ok(())
}

async fn create_schema(conn: &mut SqliteConnection) -> anyhow::Result<()> {
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
        r#"CREATE TABLE IF NOT EXISTS Realisations (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            drvPath TEXT NOT NULL,
            outputName TEXT NOT NULL,
            outputPath TEXT NOT NULL,
            signatures TEXT DEFAULT ''
        )"#,
        "CREATE INDEX IF NOT EXISTS IndexValidPathsPath ON ValidPaths(path)",
        "CREATE INDEX IF NOT EXISTS IndexValidPathsHash ON ValidPaths(hash)",
    ];

    for stmt in stmts {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn insert_paths(conn: &mut SqliteConnection, paths: &[SynthPathInfo]) -> anyhow::Result<()> {
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
    async fn open_db(db_path: &Path) -> SqliteConnection {
        let url = format!("sqlite://{}", db_path.display());
        SqliteConnection::connect(&url).await.unwrap()
    }

    #[tokio::test]
    async fn test_generate_db_creates_valid_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).await.unwrap();

        let mut conn = open_db(&db_path).await;

        // Check schema version
        let version: String =
            sqlx::query_scalar("SELECT value FROM Config WHERE name = 'SchemaVersion'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(version, "10");

        // Check valid paths count
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // Check refs count (glibc self-ref + hello->glibc + hello self-ref)
        let ref_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Refs")
            .fetch_one(&mut conn)
            .await
            .unwrap();
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
        .await
        .unwrap();

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
    }

    #[tokio::test]
    async fn test_generate_db_has_realisations_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[]).await.unwrap();

        let mut conn = open_db(&db_path).await;

        // Realisations table should exist (empty, for future CA support)
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Realisations")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_generate_db_registration_time_and_ultimate() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).await.unwrap();

        let mut conn = open_db(&db_path).await;

        // All input paths should have registrationTime=0 and ultimate=0
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT registrationTime, ultimate FROM ValidPaths")
                .fetch_all(&mut conn)
                .await
                .unwrap();

        for (reg_time, ultimate) in rows {
            assert_eq!(reg_time, 0, "registrationTime must be 0 for input paths");
            assert_eq!(ultimate, 0, "ultimate must be 0 for input paths");
        }
    }

    #[tokio::test]
    async fn test_generate_db_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).await.unwrap();

        let mut conn = open_db(&db_path).await;

        let hash: String = sqlx::query_scalar("SELECT hash FROM ValidPaths WHERE path = ?1")
            .bind("/nix/store/bbbb-hello-2.12.2")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(hash, "sha256:cafebabe");
    }

    #[tokio::test]
    async fn test_generate_db_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).await.unwrap();

        let mut conn = open_db(&db_path).await;

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(journal, "wal");
    }

    #[tokio::test]
    async fn test_generate_db_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[]).await.unwrap();

        let mut conn = open_db(&db_path).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 0);

        let version: String =
            sqlx::query_scalar("SELECT value FROM Config WHERE name = 'SchemaVersion'")
                .fetch_one(&mut conn)
                .await
                .unwrap();
        assert_eq!(version, "10");
    }

    #[tokio::test]
    async fn test_generate_db_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        let paths = sample_paths();
        generate_db(&db_path, &paths).await.unwrap();
        generate_db(&db_path, &paths).await.unwrap();

        let mut conn = open_db(&db_path).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_path_info_to_synth() {
        let proto_info = rio_proto::types::PathInfo {
            store_path: "/nix/store/abc-hello".to_string(),
            nar_hash: vec![0xde, 0xad, 0xbe, 0xef],
            nar_size: 1024,
            deriver: "/nix/store/def-hello.drv".to_string(),
            references: vec!["/nix/store/ghi-glibc".to_string()],
            signatures: vec!["sig1".to_string()],
            content_address: String::new(),
            store_path_hash: vec![],
            registration_time: 0,
            ultimate: false,
        };

        let synth = path_info_to_synth(&proto_info);
        assert_eq!(synth.path, "/nix/store/abc-hello");
        assert_eq!(synth.nar_hash, "sha256:deadbeef");
        assert_eq!(synth.nar_size, 1024);
        assert_eq!(synth.deriver, Some("/nix/store/def-hello.drv".to_string()));
        assert_eq!(synth.references, vec!["/nix/store/ghi-glibc"]);
        assert!(synth.ca.is_none());
    }
}
