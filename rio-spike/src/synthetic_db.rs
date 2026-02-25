use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use sqlx::{Connection, SqliteConnection};

/// Path metadata from `nix path-info --json`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NixPathInfo {
    pub path: String,
    pub nar_hash: String,
    pub nar_size: u64,
    #[serde(default)]
    pub deriver: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub signatures: Vec<String>,
    #[serde(default)]
    pub ca: Option<String>,
}

const SCHEMA_VERSION: &str = "10";

/// Generate a synthetic Nix store SQLite database at `db_path`.
///
/// The database contains the minimum schema required for Nix 2.20+
/// to recognize store paths: Config, ValidPaths, Refs, DerivationOutputs.
pub async fn generate_db(db_path: &Path, paths: &[NixPathInfo]) -> anyhow::Result<()> {
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let mut conn = SqliteConnection::connect(&url).await?;

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
        "CREATE INDEX IF NOT EXISTS IndexValidPathsPath ON ValidPaths(path)",
        "CREATE INDEX IF NOT EXISTS IndexValidPathsHash ON ValidPaths(hash)",
    ];

    for stmt in stmts {
        sqlx::query(stmt).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn insert_paths(conn: &mut SqliteConnection, paths: &[NixPathInfo]) -> anyhow::Result<()> {
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

    // Insert references
    for info in paths {
        let Some(&referrer_id) = path_to_id.get(&info.path) else {
            tracing::error!(path = %info.path, "referrer path not found in path_to_id after insertion -- this is a bug");
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

/// Compute the NAR hash and size of a store path by running `nix-store --dump`.
///
/// Returns `("sha256:<hex>", nar_size)`. This produces the same hash that
/// Nix records in its store DB, allowing the synthetic DB to pass verification.
pub fn compute_nar_hash(store_path: &Path) -> anyhow::Result<(String, u64)> {
    use sha2::{Digest, Sha256};

    let output = Command::new("nix-store")
        .args(["--dump"])
        .arg(store_path)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run nix-store --dump: {e}"))?;

    anyhow::ensure!(
        output.status.success(),
        "nix-store --dump failed for {}: {}",
        store_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let nar_size = output.stdout.len() as u64;
    let hash = Sha256::digest(&output.stdout);
    let hash_hex = hex::encode(hash);

    Ok((format!("sha256:{hash_hex}"), nar_size))
}

/// Query `nix path-info --json --recursive` for a store path and parse the result.
///
/// Note: `nix path-info --json` returns hashes in SRI format (`sha256-<base64>`),
/// which differs from the `sha256:<hex>` format produced by `compute_nar_hash` and
/// used by the local Nix store DB. Nix accepts both formats when reading the DB.
pub fn query_path_info(store_path: &str) -> anyhow::Result<Vec<NixPathInfo>> {
    let output = Command::new("nix")
        .args(["path-info", "--json", "--recursive", store_path])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run nix path-info: {e}"))?;

    anyhow::ensure!(
        output.status.success(),
        "nix path-info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let paths: Vec<NixPathInfo> = serde_json::from_slice(&output.stdout)?;
    Ok(paths)
}

/// CLI entry point for the sqlite-gen subcommand.
pub async fn run_sqlite_gen(output: &Path, store_path: &str) -> anyhow::Result<()> {
    tracing::info!(store_path, "querying nix path-info for closure");
    let paths = query_path_info(store_path)?;
    tracing::info!(path_count = paths.len(), "got path info for closure");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    generate_db(output, &paths).await?;
    tracing::info!(output = %output.display(), "SQLite DB written");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_paths() -> Vec<NixPathInfo> {
        vec![
            NixPathInfo {
                path: "/nix/store/aaaa-glibc-2.39".to_string(),
                nar_hash: "sha256:deadbeef".to_string(),
                nar_size: 1024,
                deriver: None,
                references: vec!["/nix/store/aaaa-glibc-2.39".to_string()],
                signatures: vec!["cache.nixos.org-1:abc123".to_string()],
                ca: None,
            },
            NixPathInfo {
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
    }

    #[tokio::test]
    async fn test_generate_db_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).await.unwrap();

        let mut conn = open_db(&db_path).await;

        // Verify we can look up paths by path (as Nix does)
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

        // WAL mode persists in the file
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

        // Schema version should still be set
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
        // Running again on the same DB should not fail (INSERT OR IGNORE)
        generate_db(&db_path, &paths).await.unwrap();

        let mut conn = open_db(&db_path).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ValidPaths")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}
