//! Append-only campaign state on the pod volume.
//!
//! Every campaign data file lives under one state directory and is
//! either an append-only JSONL stream or an atomically rewritten JSON
//! document; the artifact store periodically syncs the directory to S3
//! and resume reloads it to skip work that already reached a terminal
//! state.
//!
//! Atomicity model:
//! - JSONL appends: the full serialized line (with trailing '\n') is written
//!   with ONE `write_all` call on a file opened with `O_APPEND`. A process
//!   crash can only lose the tail line, never interleave or tear earlier
//!   lines; the loader skips a trailing partial line. Appends are not
//!   fsynced — node-loss/power-loss durability comes from the periodic S3
//!   sync, not from the local file.
//! - JSON documents (campaign.json, progress.json): write to `<name>.tmp`,
//!   fsync, rename over the target.
//! - Stage done-markers: empty files under `markers/<stage>.done`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::run::model::JobRecord;

/// The append-only JSONL streams a campaign maintains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFile {
    Results,
    Hydra,
    Warm,
    Batches,
    Supply,
    Dispatch,
}

impl StateFile {
    pub fn file_name(&self) -> &'static str {
        match self {
            StateFile::Results => "results.jsonl",
            StateFile::Hydra => "hydra.jsonl",
            StateFile::Warm => "warm.jsonl",
            StateFile::Batches => "batches.jsonl",
            StateFile::Supply => "supply.jsonl",
            StateFile::Dispatch => "dispatch.jsonl",
        }
    }

    pub const ALL: [StateFile; 6] = [
        StateFile::Results,
        StateFile::Hydra,
        StateFile::Warm,
        StateFile::Batches,
        StateFile::Supply,
        StateFile::Dispatch,
    ];
}

/// The campaign state directory and its read/write primitives.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("create state dir {}", root.display()))?;
        for sub in ["markers", "logs", "buckets", "report"] {
            let dir = root.join(sub);
            fs::create_dir_all(&dir)
                .with_context(|| format!("create state subdir {}", dir.display()))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Append one record as a single JSONL line (atomic at the line level).
    pub fn append_jsonl<T: Serialize>(&self, file: StateFile, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value).context("serialize jsonl record")?;
        line.push('\n');
        let path = self.path(file.file_name());
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {} for append", path.display()))?;
        f.write_all(line.as_bytes())
            .with_context(|| format!("append to {}", path.display()))?;
        Ok(())
    }

    /// Load every well-formed record. A trailing partial line (crash during
    /// append) is skipped with a warning; a malformed line in the middle is an
    /// error (state corruption must be loud).
    pub fn load_jsonl<T: DeserializeOwned>(&self, file: StateFile) -> Result<Vec<T>> {
        let path = self.path(file.file_name());
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        for l in reader.lines() {
            lines.push(l.context("read jsonl line")?);
        }
        let n = lines.len();
        for (i, line) in lines.into_iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<T>(&line) {
                Ok(v) => out.push(v),
                Err(e) if i + 1 == n => {
                    tracing::warn!(
                        file = file.file_name(),
                        error = %e,
                        "skipping torn trailing jsonl line"
                    );
                }
                Err(e) => {
                    anyhow::bail!("corrupt {} line {}: {e}", path.display(), i + 1);
                }
            }
        }
        Ok(out)
    }

    /// Atomic JSON document rewrite (tmp + rename).
    pub fn write_json_atomic<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(value).context("serialize json document")?;
        {
            let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read a JSON document; None when the file does not exist.
    pub fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.path(name);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        ))
    }

    /// Write raw bytes (log tails, rendered report) under the state dir.
    pub fn write_bytes(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
    }

    pub fn marker_done(&self, stage: &str) -> bool {
        self.path(&format!("markers/{stage}.done")).exists()
    }

    pub fn set_marker(&self, stage: &str) -> Result<()> {
        let path = self.path(&format!("markers/{stage}.done"));
        fs::write(&path, b"done\n").with_context(|| format!("write marker {}", path.display()))
    }
}

/// Reduce an append-only results stream to the latest record per job.
///
/// Returns a `BTreeMap` — the canonical, deterministically ordered shape
/// the run loop and the report path consume, so anything iterating the
/// latest records (per-bucket files, progress counts, log lines) is stable
/// across reloads.
pub fn latest_per_job(records: Vec<JobRecord>) -> BTreeMap<String, JobRecord> {
    let mut map = BTreeMap::new();
    for r in records {
        map.insert(r.job.clone(), r);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::model::{Bucket, HydraSide, RioSide};

    fn rec(job: &str, bucket: Bucket, attempts: u32) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: format!("/nix/store/{}-x.drv", "a".repeat(32)),
            mode: "leaf".into(),
            attempts,
            build_ids: vec![],
            rio: RioSide::default(),
            hydra: HydraSide::default(),
            nar_compare: Default::default(),
            bucket: bucket.as_str().into(),
            cascaded: false,
            signature: None,
            log_key: None,
            repro: String::new(),
            evidence: None,
            updated_at: "2026-05-26T00:00:00Z".into(),
        }
    }

    #[test]
    fn append_and_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(StateFile::Results, &rec("a", Bucket::NotAttempted, 0))
            .unwrap();
        state
            .append_jsonl(StateFile::Results, &rec("b", Bucket::MatchBuilt, 1))
            .unwrap();
        state
            .append_jsonl(StateFile::Results, &rec("a", Bucket::MatchBuilt, 1))
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 3);
        let latest = latest_per_job(loaded);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["a"].bucket, "match-built");
        assert_eq!(latest["a"].attempts, 1);
    }

    #[test]
    fn torn_trailing_line_is_skipped_but_mid_corruption_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(StateFile::Results, &rec("a", Bucket::MatchBuilt, 1))
            .unwrap();
        // Simulate a crash mid-append: torn trailing line.
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"job\":\"torn").unwrap();
        drop(f);
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 1);

        // Mid-file corruption must be loud.
        std::fs::write(&path, "{\"not\":\"a job record\"}\n").unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        let good = serde_json::to_string(&rec("b", Bucket::MatchBuilt, 1)).unwrap();
        f.write_all(format!("{good}\n").as_bytes()).unwrap();
        drop(f);
        let res: Result<Vec<JobRecord>> = state.load_jsonl(StateFile::Results);
        assert!(res.is_err(), "mid-file corruption must error");
    }

    #[test]
    fn supply_and_dispatch_state_files_named() {
        assert_eq!(StateFile::Supply.file_name(), "supply.jsonl");
        assert_eq!(StateFile::Dispatch.file_name(), "dispatch.jsonl");
        assert_eq!(StateFile::ALL.len(), 6);
    }

    #[test]
    fn json_document_atomic_rewrite_and_markers() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .write_json_atomic("progress.json", &serde_json::json!({"stage": "plan"}))
            .unwrap();
        let v: Option<serde_json::Value> = state.read_json("progress.json").unwrap();
        assert_eq!(v.unwrap()["stage"], "plan");
        assert!(!state.path("progress.tmp").exists());

        assert!(!state.marker_done("plan"));
        state.set_marker("plan").unwrap();
        assert!(state.marker_done("plan"));
        // Missing document reads as None.
        let missing: Option<serde_json::Value> = state.read_json("nope.json").unwrap();
        assert!(missing.is_none());
    }
}
