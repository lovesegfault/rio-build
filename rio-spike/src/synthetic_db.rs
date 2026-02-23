use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use rusqlite::Connection;
use serde::Deserialize;

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
pub fn generate_db(db_path: &Path, paths: &[NixPathInfo]) -> anyhow::Result<()> {
    let conn = Connection::open(db_path)?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=OFF;",
    )?;

    create_schema(&conn)?;
    insert_paths(&conn, paths)?;

    tracing::info!(
        db_path = %db_path.display(),
        path_count = paths.len(),
        "synthetic Nix store DB generated"
    );

    Ok(())
}

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS Config (
            name TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ValidPaths (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            hash TEXT NOT NULL,
            registrationTime INTEGER NOT NULL,
            deriver TEXT,
            narSize INTEGER,
            ultimate INTEGER DEFAULT 0,
            sigs TEXT DEFAULT '',
            ca TEXT DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS Refs (
            referrer INTEGER NOT NULL,
            reference INTEGER NOT NULL,
            PRIMARY KEY (referrer, reference),
            FOREIGN KEY (referrer) REFERENCES ValidPaths(id),
            FOREIGN KEY (reference) REFERENCES ValidPaths(id)
        );

        CREATE TABLE IF NOT EXISTS DerivationOutputs (
            drv INTEGER NOT NULL,
            id TEXT NOT NULL,
            path TEXT NOT NULL,
            PRIMARY KEY (drv, id),
            FOREIGN KEY (drv) REFERENCES ValidPaths(id)
        );

        CREATE INDEX IF NOT EXISTS IndexValidPathsPath ON ValidPaths(path);
        CREATE INDEX IF NOT EXISTS IndexValidPathsHash ON ValidPaths(hash);",
    )
}

fn insert_paths(conn: &Connection, paths: &[NixPathInfo]) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;

    // Set schema version
    tx.execute(
        "INSERT OR REPLACE INTO Config (name, value) VALUES ('SchemaVersion', ?1)",
        [SCHEMA_VERSION],
    )?;

    // Build path -> row ID mapping for references
    let mut path_to_id: HashMap<String, i64> = HashMap::new();

    // Insert all valid paths
    for info in paths {
        let sigs = info.signatures.join(" ");
        let ca = info.ca.as_deref().unwrap_or("");

        tx.execute(
            "INSERT OR IGNORE INTO ValidPaths (path, hash, registrationTime, deriver, narSize, ultimate, sigs, ca)
             VALUES (?1, ?2, 0, ?3, ?4, 0, ?5, ?6)",
            rusqlite::params![
                info.path,
                info.nar_hash,
                info.deriver,
                info.nar_size as i64,
                sigs,
                ca,
            ],
        )?;

        let id: i64 = tx.query_row(
            "SELECT id FROM ValidPaths WHERE path = ?1",
            [&info.path],
            |row| row.get(0),
        )?;
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
                tx.execute(
                    "INSERT OR IGNORE INTO Refs (referrer, reference) VALUES (?1, ?2)",
                    [referrer_id, reference_id],
                )?;
            } else {
                tracing::debug!(
                    referrer = %info.path,
                    reference = %ref_path,
                    "reference path not in closure, skipping"
                );
            }
        }
    }

    tx.commit()?;
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
pub fn run_sqlite_gen(output: &Path, store_path: &str) -> anyhow::Result<()> {
    tracing::info!(store_path, "querying nix path-info for closure");
    let paths = query_path_info(store_path)?;
    tracing::info!(path_count = paths.len(), "got path info for closure");

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    generate_db(output, &paths)?;
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

    #[test]
    fn test_generate_db_creates_valid_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).unwrap();

        let conn = Connection::open(&db_path).unwrap();

        // Check schema version
        let version: String = conn
            .query_row(
                "SELECT value FROM Config WHERE name = 'SchemaVersion'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "10");

        // Check valid paths count
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ValidPaths", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Check refs count (glibc self-ref + hello->glibc + hello self-ref)
        let ref_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM Refs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ref_count, 3);
    }

    #[test]
    fn test_generate_db_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).unwrap();

        let conn = Connection::open(&db_path).unwrap();

        // Verify we can look up paths by path (as Nix does)
        let hash: String = conn
            .query_row(
                "SELECT hash FROM ValidPaths WHERE path = ?1",
                ["/nix/store/bbbb-hello-2.12.2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash, "sha256:cafebabe");
    }

    #[test]
    fn test_generate_db_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).unwrap();

        let conn = Connection::open(&db_path).unwrap();

        // WAL mode persists in the file
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
    }

    #[test]
    fn test_generate_db_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[]).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ValidPaths", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Schema version should still be set
        let version: String = conn
            .query_row(
                "SELECT value FROM Config WHERE name = 'SchemaVersion'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "10");
    }

    #[test]
    fn test_generate_db_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        let paths = sample_paths();
        generate_db(&db_path, &paths).unwrap();
        // Running again on the same DB should not fail (INSERT OR IGNORE)
        generate_db(&db_path, &paths).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ValidPaths", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
