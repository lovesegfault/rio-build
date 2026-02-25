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

use rusqlite::Connection;

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

const SCHEMA_VERSION: &str = "10";

/// Generate a synthetic Nix store SQLite database at `db_path`.
///
/// The database contains the minimum schema required for Nix 2.20+
/// to recognize store paths: Config, ValidPaths, Refs, DerivationOutputs,
/// and Realisations (empty, for future CA support).
pub fn generate_db(db_path: &Path, paths: &[SynthPathInfo]) -> anyhow::Result<()> {
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

        CREATE TABLE IF NOT EXISTS Realisations (
            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
            drvPath TEXT NOT NULL,
            outputName TEXT NOT NULL,
            outputPath TEXT NOT NULL,
            signatures TEXT DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS IndexValidPathsPath ON ValidPaths(path);
        CREATE INDEX IF NOT EXISTS IndexValidPathsHash ON ValidPaths(hash);",
    )
}

fn insert_paths(conn: &Connection, paths: &[SynthPathInfo]) -> anyhow::Result<()> {
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

        // registrationTime=0, ultimate=0 for input paths
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
    fn test_generate_db_has_realisations_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &[]).unwrap();

        let conn = Connection::open(&db_path).unwrap();

        // Realisations table should exist (empty, for future CA support)
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM Realisations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_generate_db_registration_time_and_ultimate() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).unwrap();

        let conn = Connection::open(&db_path).unwrap();

        // All input paths should have registrationTime=0 and ultimate=0
        let mut stmt = conn
            .prepare("SELECT registrationTime, ultimate FROM ValidPaths")
            .unwrap();
        let rows: Vec<(i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        for (reg_time, ultimate) in rows {
            assert_eq!(reg_time, 0, "registrationTime must be 0 for input paths");
            assert_eq!(ultimate, 0, "ultimate must be 0 for input paths");
        }
    }

    #[test]
    fn test_generate_db_path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db.sqlite");

        generate_db(&db_path, &sample_paths()).unwrap();

        let conn = Connection::open(&db_path).unwrap();

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
        generate_db(&db_path, &paths).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ValidPaths", [], |row| row.get(0))
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
