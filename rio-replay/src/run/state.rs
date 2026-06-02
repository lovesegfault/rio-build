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
//!   lines. What survives a crash is decided by ONE byte-oriented rule
//!   ([`split_torn_tail`]): a line is a record iff it ends in '\n'. The
//!   loader ignores a newline-less tail (even when its bytes happen to
//!   parse) and the next append truncates the same bytes away first, so
//!   reader and repairer always agree and the fragment can never merge
//!   with a new record into one mid-file corrupt line. Appends are not
//!   fsynced — node-loss/power-loss durability comes from the periodic S3
//!   sync, not from the local file.
//! - JSON documents (campaign.json, progress.json): write to `<name>.tmp`,
//!   fsync, rename over the target.
//! - Stage done-markers: empty files under `markers/<stage>.done`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    /// When the file ends in a torn line (no trailing '\n', per
    /// [`split_torn_tail`] — a crash or short write during an earlier
    /// append), the fragment is truncated away first. It was never a
    /// complete record, so dropping it is exactly the "a crash can only
    /// lose the tail line" model; appending after it instead would merge
    /// fragment and record into one line that never parses and, once
    /// another record follows, makes every load fail.
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

    /// Load every record from the file's complete-record prefix.
    ///
    /// Torn-ness is decided by [`split_torn_tail`] — the same rule the
    /// append-side repair applies. A final line without its '\n' was never
    /// durably written, so it is ignored with a warning EVEN IF its bytes
    /// happen to parse: the next append truncates those bytes away, and
    /// acting on a record here that the repair then deletes would flip a
    /// terminal job back to never-existed mid-campaign. Conversely, a
    /// newline-terminated line that fails to parse is not a torn tail at
    /// all (the appender terminates every line it writes) but real
    /// corruption, and corruption must be loud: skipping it would silently
    /// drop the newest record for some job today, and once the next append
    /// lands after it the same line sits mid-file where every load fails.
    pub fn load_jsonl<T: DeserializeOwned>(&self, file: StateFile) -> Result<Vec<T>> {
        let path = self.path(file.file_name());
        if !path.exists() {
            return Ok(Vec::new());
        }
        // Whole-file bytes, not a line-buffered text read: records carry
        // relayed build output, so a crash can cut an append mid multi-byte
        // character and the bytes after the last '\n' need not be valid
        // UTF-8. Reaching the torn-tail decision must not require decoding
        // the very bytes the decision exists to discard.
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let (complete, torn) = split_torn_tail(&bytes);
        if !torn.is_empty() {
            tracing::warn!(
                file = file.file_name(),
                dropped_bytes = torn.len(),
                "ignoring torn trailing jsonl line"
            );
        }
        let mut out = Vec::new();
        for (i, line) in complete.split(|&b| b == b'\n').enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<T>(line) {
                Ok(v) => out.push(v),
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
    /// Plain `fs::write` — a crash mid-write can leave a torn file, which
    /// is fine for files whose presence carries no meaning (they are
    /// re-written or re-uploaded wholesale). Files whose PRESENCE is a
    /// signal (e.g. the restore-complete sentinel) must use
    /// [`Self::write_bytes_atomic`] instead.
    pub fn write_bytes(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))
    }

    /// Atomic raw-byte rewrite — the same tmp + fsync + rename discipline
    /// as [`Self::write_json_atomic`], for callers that already hold the
    /// serialized bytes of a JSON document. For files whose existence is
    /// itself a signal (the restore path's campaign.json sentinel): a
    /// crash mid-write must leave either the old state or the new one,
    /// never a torn file that exists but cannot be parsed.
    pub fn write_bytes_atomic(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all().ok();
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
    }

    pub fn marker_done(&self, stage: &str) -> bool {
        self.path(&format!("markers/{stage}.done")).exists()
    }

    pub fn set_marker(&self, stage: &str) -> Result<()> {
        let path = self.path(&format!("markers/{stage}.done"));
        fs::write(&path, b"done\n").with_context(|| format!("write marker {}", path.display()))
    }
}

/// Split a JSONL buffer at the torn-tail boundary: the complete-record
/// prefix runs through the last `b'\n'` (empty when there is none), and
/// any bytes after it are a torn tail — an append cut short by a crash,
/// never part of any record.
///
/// THE single definition of "torn" for the campaign-state JSONL format.
/// [`StateDir::load_jsonl`] ignores the tail this function reports,
/// `truncate_torn_tail` (the append-side repair) cuts the file back to
/// the boundary it reports, and out-of-process consumers of the S3-synced
/// copies — which the artifact sync uploads byte-verbatim, torn tail and
/// all — must apply it before parsing. Deliberately byte-oriented: a
/// crash can cut an append mid multi-byte character, so the boundary must
/// be decidable without the tail being valid UTF-8.
pub fn split_torn_tail(bytes: &[u8]) -> (&[u8], &[u8]) {
    let keep = bytes
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |pos| pos + 1);
    bytes.split_at(keep)
}

/// Cut `f` back to its complete-record prefix (per [`split_torn_tail`]) so
/// the next write starts on a fresh line. No-op for empty or
/// newline-terminated files.
fn truncate_torn_tail(f: &mut File, file: StateFile) -> Result<()> {
    let len = f.metadata().context("stat")?.len();
    if len == 0 {
        return Ok(());
    }
    // Fast path for the every-append healthy case, without reading the
    // file: the boundary is the last '\n', so whether the tail is empty is
    // a property of the final byte alone, and feeding just that byte to
    // the shared rule answers it.
    let mut last = [0u8; 1];
    f.seek(SeekFrom::Start(len - 1)).context("seek to tail")?;
    f.read_exact(&mut last).context("read last byte")?;
    if split_torn_tail(&last).1.is_empty() {
        return Ok(());
    }
    // Torn: locate the boundary with the same rule the loader uses and
    // truncate to it. Reading the whole file here is fine — repair runs at
    // most once after a crash, and the loader reads the same bytes in full
    // on every resume anyway.
    f.seek(SeekFrom::Start(0)).context("seek to start")?;
    let mut bytes = Vec::with_capacity(len as usize);
    f.read_to_end(&mut bytes).context("read for tail repair")?;
    let keep = split_torn_tail(&bytes).0.len() as u64;
    f.set_len(keep).context("truncate")?;
    tracing::warn!(
        file = file.file_name(),
        dropped_bytes = bytes.len() as u64 - keep,
        "dropped torn trailing jsonl line before append"
    );
    Ok(())
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
    fn split_torn_tail_boundary_cases() {
        // The one rule both the loader and the append-side repair consume:
        // complete prefix through the last '\n', everything after is torn.
        assert_eq!(split_torn_tail(b""), (&b""[..], &b""[..]));
        assert_eq!(split_torn_tail(b"abc"), (&b""[..], &b"abc"[..]));
        assert_eq!(split_torn_tail(b"\n"), (&b"\n"[..], &b""[..]));
        assert_eq!(split_torn_tail(b"a\n"), (&b"a\n"[..], &b""[..]));
        assert_eq!(split_torn_tail(b"a\nb"), (&b"a\n"[..], &b"b"[..]));
        // Byte-defined: an invalid-UTF-8 tail must still be splittable.
        assert_eq!(
            split_torn_tail(b"a\n\xe2\x80"),
            (&b"a\n"[..], &b"\xe2\x80"[..])
        );
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
    fn torn_mid_utf8_character_tail_loads_and_repairs() {
        // The crash shape that wedges resume when torn-ness is judged on
        // text instead of bytes: an append cut mid multi-byte character
        // leaves an invalid-UTF-8 tail. Records carry relayed build stderr,
        // so non-ASCII bytes in the final line are routine. The loader must
        // never need the tail to be valid UTF-8 — resume loads the complete
        // prefix, a byte-verbatim restored copy loads on every replacement
        // pod, and the next append repairs the file on disk.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();

        // Producer-verbatim line for a record whose failure cause carries
        // non-ASCII compiler output (U+2018 = e2 80 98), cut one byte into
        // the character: exactly the prefix a crash can persist. The lost
        // suffix includes the terminating '\n'.
        let mut torn = rec("b", UnifiedClass::Verdict(Verdict::MatchBuilt), 1);
        torn.failure_cause = Some("error: expected \u{2018};\u{2019} before token".into());
        let mut line = serde_json::to_vec(&torn).unwrap();
        let cut = line.iter().position(|&b| b == 0xe2).unwrap() + 1;
        line.truncate(cut);
        assert!(
            std::str::from_utf8(&line).is_err(),
            "fixture must end mid-character"
        );
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&line).unwrap();
        drop(f);

        // Resume must load the complete prefix instead of failing the read.
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].job, "a");

        // The S3 sync/restore cycle propagates the torn bytes verbatim to
        // every replacement pod; each one must load too, or the campaign
        // crash-loops until an operator hand-edits the file.
        let dir2 = tempfile::tempdir().unwrap();
        let restored = StateDir::new(dir2.path()).unwrap();
        std::fs::copy(&path, restored.path("results.jsonl")).unwrap();
        let reloaded: Vec<JobRecord> = restored.load_jsonl(StateFile::Results).unwrap();
        assert_eq!(reloaded.len(), 1);

        // The next append self-heals the file: fragment gone, bytes valid.
        state
            .append_jsonl(
                StateFile::Results,
                &rec("c", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        let jobs: Vec<&str> = loaded.iter().map(|r| r.job.as_str()).collect();
        assert_eq!(jobs, ["a", "c"]);
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            std::str::from_utf8(&bytes).is_ok(),
            "repair must remove the mid-character fragment"
        );
        assert!(bytes.ends_with(b"\n"));
    }

    #[test]
    fn unterminated_complete_record_is_torn_not_durable() {
        // A crash can persist a record's bytes but not its terminating
        // '\n'. The appender's repair truncates such a tail on the next
        // append, so the loader must not honor it either — honoring it
        // would let resume act on a record that the very next append
        // deletes, flipping a terminal job back to never-existed
        // mid-campaign. Loader and repair must make the same call on the
        // same bytes: not newline-terminated, never durably written.
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
        // Crash exactly between record "b"'s JSON bytes and its '\n'.
        let path = state.path("results.jsonl");
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 1).unwrap();
        drop(f);

        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        let jobs: Vec<&str> = loaded.iter().map(|r| r.job.as_str()).collect();
        assert_eq!(
            jobs,
            ["a"],
            "an unterminated record parses but was never durably written; \
             the next append truncates it, so resume must not act on it"
        );

        // The append-side repair agrees byte-for-byte with what the loader
        // skipped: after the append, the file holds exactly a + the new
        // record and reloads as such.
        state
            .append_jsonl(
                StateFile::Results,
                &rec("c", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let loaded: Vec<JobRecord> = state.load_jsonl(StateFile::Results).unwrap();
        let jobs: Vec<&str> = loaded.iter().map(|r| r.job.as_str()).collect();
        assert_eq!(jobs, ["a", "c"]);
    }

    #[test]
    fn newline_terminated_corrupt_last_line_errors_loudly() {
        // The appender terminates every line it writes, so a
        // newline-terminated line that fails to parse is not a torn tail —
        // it is corruption (schema skew, disk damage, partial restore).
        // Skipping it because it happens to be last would silently drop the
        // newest record for some job, and one more append later the same
        // line sits mid-file where every load fails forever. Corruption
        // must be loud immediately, not after it becomes unrecoverable.
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        let path = state.path("results.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"not\":\"a job record\"}\n").unwrap();
        drop(f);

        let res: Result<Vec<JobRecord>> = state.load_jsonl(StateFile::Results);
        let err = res
            .expect_err("terminated corrupt line is not torn")
            .to_string();
        assert!(
            err.contains("line 2"),
            "error must name the corrupt line: {err}"
        );
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
    fn append_repairs_multi_kilobyte_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &rec("a", UnifiedClass::Verdict(Verdict::MatchBuilt), 1),
            )
            .unwrap();
        // A fragment far longer than any record: the boundary search must
        // walk all the way back to the previous newline.
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

    #[test]
    fn raw_bytes_atomic_rewrite() {
        // write_bytes_atomic follows the same tmp + rename discipline as
        // write_json_atomic: the target is replaced wholesale and no .tmp
        // sibling survives — required for files whose presence is itself a
        // signal (the restore path's campaign.json sentinel).
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        state
            .write_bytes_atomic("campaign.json", b"{\"campaignId\":\"c1\"}")
            .unwrap();
        assert_eq!(
            std::fs::read(state.path("campaign.json")).unwrap(),
            b"{\"campaignId\":\"c1\"}"
        );
        assert!(!state.path("campaign.tmp").exists());

        // Overwrite replaces the previous content in full.
        state
            .write_bytes_atomic("campaign.json", b"{\"campaignId\":\"c2\"}")
            .unwrap();
        assert_eq!(
            std::fs::read(state.path("campaign.json")).unwrap(),
            b"{\"campaignId\":\"c2\"}"
        );
        assert!(!state.path("campaign.tmp").exists());

        // Parent directories are created as needed (parity with
        // write_bytes).
        state
            .write_bytes_atomic("nested/dir/doc.json", b"{}")
            .unwrap();
        assert_eq!(
            std::fs::read(state.path("nested/dir/doc.json")).unwrap(),
            b"{}"
        );
    }
}
