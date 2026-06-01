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
//!   lines; the loader skips a trailing partial line, and the next append
//!   truncates it away first so the fragment can never merge with a new
//!   record into one mid-file corrupt line. Appends are not fsynced —
//!   node-loss/power-loss durability comes from the periodic S3 sync, not
//!   from the local file.
//! - JSON documents (campaign.json, progress.json): write to `<name>.tmp`,
//!   fsync, rename over the target.
//! - Stage done-markers: empty files under `markers/<stage>.done`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::run::model::JobRecord;

/// The append-only JSONL streams a campaign maintains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFile {
    Results,
    Batches,
    Supply,
    Dispatch,
}

impl StateFile {
    pub fn file_name(&self) -> &'static str {
        match self {
            StateFile::Results => "results.jsonl",
            StateFile::Batches => "batches.jsonl",
            StateFile::Supply => "supply.jsonl",
            StateFile::Dispatch => "dispatch.jsonl",
        }
    }

    pub const ALL: [StateFile; 4] = [
        StateFile::Results,
        StateFile::Batches,
        StateFile::Supply,
        StateFile::Dispatch,
    ];
}

/// The campaign state directory and its read/write primitives.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
    /// Serializes JSONL appends across clones. The torn-tail repair in
    /// [`StateDir::append_jsonl`] is a stat → truncate → write sequence; two
    /// concurrent appenders interleaving it could truncate away each other's
    /// just-written record, so the whole sequence runs under this lock.
    append_lock: Arc<Mutex<()>>,
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
        Ok(Self {
            root,
            append_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Append one record as a single JSONL line (atomic at the line level).
    ///
    /// When the file ends in a torn line (no trailing '\n' — a crash or
    /// short write during an earlier append), the fragment is truncated away
    /// first. It was never a complete record, so dropping it is exactly the
    /// "a crash can only lose the tail line" model; appending after it
    /// instead would merge fragment and record into one line that never
    /// parses and, once another record follows, makes every load fail.
    pub fn append_jsonl<T: Serialize>(&self, file: StateFile, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value).context("serialize jsonl record")?;
        line.push('\n');
        let path = self.path(file.file_name());
        // Poisoning is irrelevant here: the lock guards no in-process state,
        // and the file itself is re-checked (and repaired) on every append.
        let _guard = self
            .append_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut f = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {} for append", path.display()))?;
        truncate_torn_tail(&mut f, file)
            .with_context(|| format!("repair torn tail of {}", path.display()))?;
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

/// If `f` does not end in '\n', truncate it back to just past the last
/// newline (to empty when there is none), so the next write starts on a
/// fresh line. No-op for empty or newline-terminated files.
fn truncate_torn_tail(f: &mut File, file: StateFile) -> Result<()> {
    let len = f.metadata().context("stat")?.len();
    if len == 0 {
        return Ok(());
    }
    let mut last = [0u8; 1];
    f.seek(SeekFrom::Start(len - 1)).context("seek to tail")?;
    f.read_exact(&mut last).context("read last byte")?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let keep = match last_newline_position(f, len - 1)? {
        Some(pos) => pos + 1,
        None => 0,
    };
    f.set_len(keep).context("truncate")?;
    tracing::warn!(
        file = file.file_name(),
        dropped_bytes = len - keep,
        "dropped torn trailing jsonl line before append"
    );
    Ok(())
}

/// Byte offset of the last '\n' in `f` before offset `end`, scanning
/// backwards in chunks; `None` when the region holds no newline.
fn last_newline_position(f: &mut File, end: u64) -> Result<Option<u64>> {
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let mut hi = end;
    while hi > 0 {
        let lo = hi.saturating_sub(CHUNK as u64);
        let n = (hi - lo) as usize;
        f.seek(SeekFrom::Start(lo))
            .context("seek for newline scan")?;
        f.read_exact(&mut buf[..n])
            .context("read for newline scan")?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(Some(lo + i as u64));
        }
        hi = lo;
    }
    Ok(None)
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
    use crate::run::model::{Disposition, ExpectedSide, RioSide, UnifiedClass, Verdict};

    fn rec(job: &str, class: UnifiedClass, attempts: u32) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: format!("/nix/store/{}-x.drv", "a".repeat(32)),
            mode: "leaf".into(),
            attempts,
            build_ids: vec![],
            rio: RioSide::default(),
            expected: ExpectedSide::default(),
            nar_compare: Default::default(),
            verdict: class.verdict().map(|v| v.as_str().into()),
            disposition: class.disposition().map(|d| d.as_str().into()),
            cascaded: false,
            failure_cause: None,
            flaky: false,
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
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Disposition(Disposition::NotAttempted), 0),
            )
            .unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 3);
        let latest = latest_per_job(loaded);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest["a"].verdict.as_deref(), Some("match-built"));
        assert_eq!(latest["a"].attempts, 1);
    }

    #[test]
    fn torn_trailing_line_is_skipped_but_mid_corruption_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
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
        let good = serde_json::to_string(&rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1))
            .unwrap();
        f.write_all(format!("{good}\n").as_bytes()).unwrap();
        drop(f);
        let res: Result<Vec<JobRecord>> = state.load_jsonl(StateFile::Results);
        assert!(res.is_err(), "mid-file corruption must error");
    }

    #[test]
    fn append_after_torn_tail_truncates_fragment_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        // Simulate a crash mid-append: torn trailing line, no newline.
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"job\":\"torn").unwrap();
        drop(f);

        // The next append must drop the fragment instead of merging with it.
        state
            .append_jsonl(
                StateFile::Results,
                &rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "torn fragment must not swallow the next record"
        );
        assert_eq!(loaded[0].job, "a");
        assert_eq!(loaded[1].job, "b");

        // Once a further record follows, the file must still load: a merged
        // line would now sit mid-file and fail loudly forever.
        state
            .append_jsonl(
                StateFile::Results,
                &rec("c", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 3);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 3);
        assert!(!text.contains("torn"));
    }

    #[test]
    fn append_after_torn_tail_with_no_prior_newline_truncates_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        // The whole file is one torn fragment (crash during the very first
        // append): there is no earlier newline to cut back to.
        let path = state.path("results.jsonl");
        std::fs::write(&path, b"{\"job\":\"torn").unwrap();

        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].job, "a");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("torn"));
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn append_repairs_torn_tail_longer_than_one_scan_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        // A fragment longer than one backwards-scan chunk: the search for
        // the previous newline must walk across chunk boundaries.
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&vec![b'x'; 10_000]).unwrap();
        drop(f);

        state
            .append_jsonl(
                StateFile::Results,
                &rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].job, "b");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("xxxx"), "fragment must be gone");
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn append_leaves_healthy_file_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let path = state.path("results.jsonl");
        let before = std::fs::read(&path).unwrap();

        state
            .append_jsonl(
                StateFile::Results,
                &rec("c", UnifiedClass::Verdict(Verdict::MatchBuilt), 2),
            )
            .unwrap();
        let after = std::fs::read(&path).unwrap();
        assert!(after.len() > before.len());
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "a healthy newline-terminated file must be appended to verbatim"
        );
    }

    #[test]
    fn concurrent_appends_after_torn_tail_lose_no_records() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("seed", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"job\":\"torn").unwrap();
        drop(f);

        // All appenders in one process share the repair lock through Clone:
        // no append may be truncated away by a concurrent repair.
        std::thread::scope(|s| {
            for i in 0..8 {
                let state = state.clone();
                s.spawn(move || {
                    state
                        .append_jsonl(
                            StateFile::Results,
                            &rec(
                                &format!("j{i}"),
                                UnifiedClass::Verdict(Verdict::MatchBuilt),
                                1,
                            ),
                        )
                        .unwrap();
                });
            }
        });

        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(
            loaded.len(),
            9,
            "every concurrent append must survive the torn-tail repair"
        );
    }

    #[test]
    fn supply_and_dispatch_state_files_named() {
        assert_eq!(StateFile::Supply.file_name(), "supply.jsonl");
        assert_eq!(StateFile::Dispatch.file_name(), "dispatch.jsonl");
        assert_eq!(StateFile::ALL.len(), 4);
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
