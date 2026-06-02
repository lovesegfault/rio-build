//! Campaign artifact storage (S3 or a local directory) plus state sync.
//!
//! Campaign artifacts live under `<prefix>/<campaign-id>/…` (default
//! prefix `replay/campaigns`), one object per state file with the same
//! relative names the local state directory uses. The local-directory
//! backend serves tests and `--no-s3` development runs; the S3 backend is
//! a thin adapter over the shared `aws_sdk_s3` client. The sync uploads
//! only files whose (length, mtime) signature changed since the previous
//! tick, and the download restores a previously synced campaign onto an
//! empty pod volume so resume can continue where the synced state left off.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::state::{StateDir, StateFile};

/// Byte-level campaign artifact storage keyed by S3-style object keys.
///
/// Two object classes, two read methods: `get_bytes` is for small
/// metadata documents (campaign.json, markers, JSONL state) that are fine
/// to buffer; `get_to_file` is the ONLY way to fetch bulk payloads
/// (archive members reach tens-to-hundreds of GiB and must never transit
/// memory as one buffer). Every implementation must stream `get_to_file`.
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()>;
    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Stream one object to `dest` (created/truncated), returning its
    /// SHA-256 (lowercase hex) and byte length so callers verify without a
    /// second pass; `Ok(None)` — with no file created — when the key does
    /// not exist.
    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<Option<(String, u64)>>;
    async fn exists(&self, key: &str) -> Result<bool>;
}

/// Local-filesystem implementation (tests, `--no-s3` dev runs): each key
/// becomes a relative path under the root directory.
pub struct LocalDirArtifactStore {
    root: PathBuf,
}

impl LocalDirArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn key_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl ArtifactStore for LocalDirArtifactStore {
    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        let path = self.key_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create artifact dir {}", parent.display()))?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("write {}", path.display()))
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.key_path(key);
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<Option<(String, u64)>> {
        use sha2::Digest as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let path = self.key_path(key);
        let mut src = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("create {}", dest.display()))?;
        let mut writer = tokio::io::BufWriter::new(file);
        let mut hasher = sha2::Sha256::new();
        let mut size: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = src
                .read(&mut buf)
                .await
                .with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
            writer
                .write_all(&buf[..n])
                .await
                .with_context(|| format!("write {}", dest.display()))?;
        }
        writer
            .flush()
            .await
            .with_context(|| format!("flush {}", dest.display()))?;
        Ok(Some((hex::encode(hasher.finalize()), size)))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.key_path(key).exists())
    }
}

/// S3 implementation. Uses `rio_common::s3::default_client` (IRSA
/// in-cluster, ambient AWS config locally) so credential and retry
/// resolution matches the rest of the workspace.
pub struct S3ArtifactStore {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3ArtifactStore {
    pub async fn new(bucket: String) -> Self {
        let client = rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
        Self { client, bucket }
    }

    pub fn from_client(client: aws_sdk_s3::Client, bucket: String) -> Self {
        Self { client, bucket }
    }
}

#[async_trait]
impl ArtifactStore for S3ArtifactStore {
    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes))
            .send()
            .await
            .with_context(|| format!("s3 put s3://{}/{key}", self.bucket))?;
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => {
                let data = out.body.collect().await.context("collect s3 body")?;
                Ok(Some(data.into_bytes().to_vec()))
            }
            Err(err) => {
                if err.as_service_error().is_some_and(|e| e.is_no_such_key()) {
                    return Ok(None);
                }
                Err(err).with_context(|| format!("s3 get s3://{}/{key}", self.bucket))
            }
        }
    }

    async fn get_to_file(&self, key: &str, dest: &Path) -> Result<Option<(String, u64)>> {
        use sha2::Digest as _;
        use tokio::io::AsyncWriteExt as _;

        let resp = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                return Ok(None);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("s3 get s3://{}/{key}", self.bucket));
            }
        };
        let mut body = resp.body;
        let file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("create {}", dest.display()))?;
        let mut writer = tokio::io::BufWriter::new(file);
        let mut hasher = sha2::Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = body
            .try_next()
            .await
            .with_context(|| format!("read s3://{}/{key}", self.bucket))?
        {
            hasher.update(&chunk);
            size += chunk.len() as u64;
            writer
                .write_all(&chunk)
                .await
                .with_context(|| format!("write {}", dest.display()))?;
        }
        writer
            .flush()
            .await
            .with_context(|| format!("flush {}", dest.display()))?;
        Ok(Some((hex::encode(hasher.finalize()), size)))
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                if err.as_service_error().is_some_and(|e| e.is_not_found()) {
                    return Ok(false);
                }
                Err(err).with_context(|| format!("s3 head s3://{}/{key}", self.bucket))
            }
        }
    }
}

/// Files synced from the state dir to the store on every sync tick. The
/// per-bucket JSONL written by the report stage and the per-job
/// `logs/*.log.zst` tails written by collect are picked up by the dynamic
/// enumerations in [`sync_state`] (so the uploaded campaign prefix carries
/// the complete artifact set). Log tails are uploaded by collect at capture
/// time already; re-enumerating them here retries any upload that failed at
/// capture.
const SYNCED_FILES: &[&str] = &[
    "campaign.json",
    "progress.json",
    "results.jsonl",
    "supply.jsonl",
    "dispatch.jsonl",
    "batches.jsonl",
    "supply-report.json",
    "timed-stats.json",
    "report/summary.md",
    "report/gate.json",
    "markers/plan.done",
    "markers/supply.done",
    "markers/report.done",
];

/// Tracks the last-synced `(length, mtime)` signature per file so
/// unchanged files are not re-uploaded. The JSONL streams are
/// append-only and the JSON documents are full rewrites, so a changed
/// length catches appends and the modification time catches same-length
/// rewrites; a rewrite that changes neither (same length within the
/// filesystem's mtime granularity) is picked up on the next tick that
/// moves either signal.
#[derive(Debug, Default)]
pub struct SyncTracker {
    last_sig: HashMap<String, (u64, Option<std::time::SystemTime>)>,
}

/// Upload changed state files to `<prefix>/<campaign-id>/<rel>`.
/// Returns how many objects were uploaded this tick.
pub async fn sync_state(
    state: &StateDir,
    store: &dyn ArtifactStore,
    prefix: &str,
    campaign_id: &str,
    tracker: &mut SyncTracker,
) -> Result<usize> {
    let mut rels: Vec<String> = SYNCED_FILES.iter().map(|s| s.to_string()).collect();
    // buckets/<bucket>.jsonl exist only after the report stage; enumerate them
    // dynamically rather than hardcoding the bucket list.
    if let Ok(entries) = std::fs::read_dir(state.path("buckets")) {
        for entry in entries.flatten() {
            // Bucket files are engine-written `<bucket>.jsonl` names (always
            // valid UTF-8); anything else in the directory is not ours to sync.
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".jsonl") {
                rels.push(format!("buckets/{name}"));
            }
        }
    }
    // logs/<job>.log.zst are uploaded by collect at capture time; enumerating
    // them here means an upload that failed at capture is retried by the next
    // sync tick instead of stranding the evidence the job record points at.
    if let Ok(entries) = std::fs::read_dir(state.path("logs")) {
        for entry in entries.flatten() {
            // Log tails are engine-written `<job>.log.zst` names (always
            // valid UTF-8); anything else in the directory is not ours to sync.
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".log.zst") {
                rels.push(format!("logs/{name}"));
            }
        }
    }
    // Upload data files first and `markers/*` last: a stage's done-marker
    // must never be visible in the store before the data it certifies (a
    // crash mid-tick would otherwise let a restored campaign trust a marker
    // whose data never made it to S3).
    rels.sort_by(|a, b| (a.starts_with("markers/"), a).cmp(&(b.starts_with("markers/"), b)));
    let mut uploaded = 0;
    for rel in &rels {
        let path = state.path(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let sig = (meta.len(), meta.modified().ok());
        if tracker.last_sig.get(rel.as_str()) == Some(&sig) {
            continue;
        }
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        store
            .put_bytes(&format!("{prefix}/{campaign_id}/{rel}"), bytes)
            .await?;
        tracker.last_sig.insert(rel.clone(), sig);
        uploaded += 1;
    }
    Ok(uploaded)
}

/// Resume support: if the local state dir has no campaign.json but the
/// store does, download the synced state files so resume can proceed
/// after a pod reschedule wiped the local volume. Returns true when a
/// campaign was restored from the store. The local campaign.json doubles
/// as the restore-complete sentinel and is written last, so a restore
/// that fails partway leaves no sentinel and is retried in full on the
/// next start. `buckets/*.jsonl` and
/// `logs/*.log.zst` are uploaded by [`sync_state`] but not restored here —
/// the report stage regenerates the bucket files from results.jsonl, and
/// the job records already carry their log keys (the local log copies are
/// only an upload staging area).
pub async fn download_state_if_missing(
    state: &StateDir,
    store: &dyn ArtifactStore,
    prefix: &str,
    campaign_id: &str,
) -> Result<bool> {
    if state.path("campaign.json").exists() {
        return Ok(false);
    }
    let Some(campaign) = store
        .get_bytes(&format!("{prefix}/{campaign_id}/campaign.json"))
        .await?
    else {
        return Ok(false);
    };
    for rel in SYNCED_FILES.iter().filter(|r| **r != "campaign.json") {
        if let Some(bytes) = store
            .get_bytes(&format!("{prefix}/{campaign_id}/{rel}"))
            .await?
        {
            state.write_bytes(rel, &bytes)?;
        }
    }
    // Written last: the local campaign.json is the already-restored sentinel
    // checked at the top of this function, so it must only become visible
    // after every other state file landed. A failure mid-restore then leaves
    // no sentinel and the next start retries the full restore (the downloads
    // are idempotent overwrites); a sentinel written first would make a
    // partial restore look complete, resume the campaign on near-empty state,
    // and let the periodic sync overwrite the durable store copies with that
    // empty state. Mirrors the data-before-markers ordering [`sync_state`]
    // enforces in the upload direction. The write itself is atomic (tmp +
    // fsync + rename, the state dir's JSON-document discipline): a torn
    // sentinel would exist-but-not-parse, skipping the retry that heals it
    // and crash-looping resume on an unreadable campaign.json instead.
    state.write_bytes_atomic("campaign.json", &campaign)?;
    Ok(true)
}

/// Convenience: which JSONL state files exist locally (used by resume logging).
pub fn local_state_files(state: &StateDir) -> Vec<&'static str> {
    StateFile::ALL
        .iter()
        .map(|f| f.file_name())
        .filter(|n| state.path(n).exists())
        .collect()
}

/// Strip the parent dir from a path for upload keys (helper for log
/// uploads). Returns an empty string when the path has no file name or
/// the name is not valid UTF-8 (the engine only ever names its own
/// ASCII files, so neither happens in practice).
pub fn file_name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::model::{Disposition, ExpectedSide, JobRecord, RioSide};

    fn rec(job: &str) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            mode: "leaf".into(),
            attempts: 0,
            build_ids: vec![],
            rio: RioSide::default(),
            expected: ExpectedSide::default(),
            nar_compare: Default::default(),
            verdict: None,
            disposition: Some(Disposition::NotAttempted.as_str().into()),
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

    #[tokio::test]
    async fn local_store_roundtrip_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDirArtifactStore::new(dir.path());
        assert!(
            !store
                .exists("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
        );
        assert!(
            store
                .get_bytes("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
                .is_none()
        );
        store
            .put_bytes("replay/campaigns/c1/campaign.json", b"{}".to_vec())
            .await
            .unwrap();
        assert!(
            store
                .exists("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get_bytes("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
                .unwrap(),
            b"{}"
        );
    }

    #[tokio::test]
    async fn sync_uploads_only_changed_files() {
        let sdir = tempfile::tempdir().unwrap();
        let adir = tempfile::tempdir().unwrap();
        let state = StateDir::new(sdir.path()).unwrap();
        let store = LocalDirArtifactStore::new(adir.path());
        let mut tracker = SyncTracker::default();

        state
            .write_json_atomic("campaign.json", &serde_json::json!({"campaignId": "c1"}))
            .unwrap();
        state.append_jsonl(StateFile::Results, &rec("a")).unwrap();
        // A log tail whose at-capture upload failed sits locally and is
        // picked up by the sync's logs/*.log.zst enumeration.
        state
            .write_bytes("logs/a.x86_64-linux.log.zst", b"zstd-bytes")
            .unwrap();
        let n1 = sync_state(&state, &store, "replay/campaigns", "c1", &mut tracker)
            .await
            .unwrap();
        assert_eq!(n1, 3, "campaign.json + results.jsonl + log tail");
        assert!(
            store
                .exists("replay/campaigns/c1/logs/a.x86_64-linux.log.zst")
                .await
                .unwrap()
        );

        // No change → nothing uploaded.
        let n2 = sync_state(&state, &store, "replay/campaigns", "c1", &mut tracker)
            .await
            .unwrap();
        assert_eq!(n2, 0);

        // Append → only results.jsonl re-uploaded.
        state.append_jsonl(StateFile::Results, &rec("b")).unwrap();
        let n3 = sync_state(&state, &store, "replay/campaigns", "c1", &mut tracker)
            .await
            .unwrap();
        assert_eq!(n3, 1);
        assert!(
            store
                .exists("replay/campaigns/c1/results.jsonl")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn get_to_file_streams_and_reports_digest_and_size() {
        use sha2::Digest as _;

        let adir = tempfile::tempdir().unwrap();
        let store = LocalDirArtifactStore::new(adir.path());
        let body = vec![7u8; 200 * 1024]; // larger than one 64 KiB chunk
        store
            .put_bytes("replay/archives/aa/archive.dwarfs", body.clone())
            .await
            .unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("archive.dwarfs");
        let (sha256, size) = store
            .get_to_file("replay/archives/aa/archive.dwarfs", &dest)
            .await
            .unwrap()
            .expect("the object exists");
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha256, hex::encode(sha2::Sha256::digest(&body)));
        assert_eq!(std::fs::read(&dest).unwrap(), body);

        // A missing key is a miss — and no destination file is created.
        let missing_dest = dest_dir.path().join("missing");
        assert!(
            store
                .get_to_file("replay/archives/aa/nope", &missing_dest)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!missing_dest.exists());
    }

    #[tokio::test]
    async fn s3_get_to_file_streams_and_maps_misses() {
        use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
        use aws_sdk_s3::primitives::ByteStream;
        use aws_sdk_s3::types::error::NoSuchKey;
        use aws_smithy_mocks::{RuleMode, mock, mock_client};
        use sha2::Digest as _;

        let body = b"dwarfs-image-bytes".to_vec();
        let get_hit = {
            let body = body.clone();
            mock!(aws_sdk_s3::Client::get_object)
                .match_requests(|req| req.key() == Some("replay/archives/aa/archive.dwarfs"))
                .then_output(move || {
                    GetObjectOutput::builder()
                        .body(ByteStream::from(body.clone()))
                        .build()
                })
        };
        let get_miss = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|req| req.key() == Some("replay/archives/aa/nope"))
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&get_hit, &get_miss]);
        let store = S3ArtifactStore::from_client(client, "rio-chunks".into());

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("archive.dwarfs");
        let (sha256, size) = store
            .get_to_file("replay/archives/aa/archive.dwarfs", &dest)
            .await
            .unwrap()
            .expect("the object exists");
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha256, hex::encode(sha2::Sha256::digest(&body)));
        assert_eq!(std::fs::read(&dest).unwrap(), body);

        let missing_dest = dir.path().join("missing");
        assert!(
            store
                .get_to_file("replay/archives/aa/nope", &missing_dest)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!missing_dest.exists());
    }

    /// [`ArtifactStore`] wrapper that records the order of uploaded keys.
    struct RecordingStore {
        inner: LocalDirArtifactStore,
        puts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ArtifactStore for RecordingStore {
        async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
            self.puts.lock().unwrap().push(key.to_string());
            self.inner.put_bytes(key, bytes).await
        }

        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get_bytes(key).await
        }

        async fn get_to_file(&self, key: &str, dest: &Path) -> Result<Option<(String, u64)>> {
            self.inner.get_to_file(key, dest).await
        }

        async fn exists(&self, key: &str) -> Result<bool> {
            self.inner.exists(key).await
        }
    }

    #[tokio::test]
    async fn sync_uploads_data_files_before_markers() {
        let sdir = tempfile::tempdir().unwrap();
        let adir = tempfile::tempdir().unwrap();
        let state = StateDir::new(sdir.path()).unwrap();
        let store = RecordingStore {
            inner: LocalDirArtifactStore::new(adir.path()),
            puts: std::sync::Mutex::new(Vec::new()),
        };
        let mut tracker = SyncTracker::default();

        state
            .write_json_atomic("campaign.json", &serde_json::json!({"campaignId": "c1"}))
            .unwrap();
        state.append_jsonl(StateFile::Results, &rec("a")).unwrap();
        state
            .write_bytes("buckets/match-built.jsonl", b"{}\n")
            .unwrap();
        state.set_marker("plan").unwrap();
        state.set_marker("supply").unwrap();

        let n = sync_state(&state, &store, "replay/campaigns", "c1", &mut tracker)
            .await
            .unwrap();
        assert_eq!(n, 5, "campaign.json + results + bucket + two markers");
        let keys = store.puts.lock().unwrap().clone();
        // A done-marker must never land in the store before the data it
        // certifies: every marker upload comes after every data upload.
        let first_marker = keys
            .iter()
            .position(|k| k.contains("/markers/"))
            .expect("markers were uploaded");
        let last_data = keys
            .iter()
            .rposition(|k| !k.contains("/markers/"))
            .expect("data files were uploaded");
        assert!(
            last_data < first_marker,
            "markers must be uploaded after every data file: {keys:?}"
        );
    }

    #[tokio::test]
    async fn s3_store_maps_misses_and_round_trips_bytes() {
        use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
        use aws_sdk_s3::operation::head_object::HeadObjectError;
        use aws_sdk_s3::primitives::ByteStream;
        use aws_sdk_s3::types::error::{NoSuchKey, NotFound};
        use aws_smithy_mocks::{RuleMode, mock, mock_client};

        let get_miss = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|req| req.key() == Some("replay/campaigns/c1/campaign.json"))
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let head_miss = mock!(aws_sdk_s3::Client::head_object)
            .match_requests(|req| req.key() == Some("replay/campaigns/c1/campaign.json"))
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let get_hit = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|req| req.key() == Some("replay/campaigns/c1/results.jsonl"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(b"{\"job\":\"a\"}\n"))
                    .build()
            });
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&get_miss, &head_miss, &get_hit]
        );
        let store = S3ArtifactStore::from_client(client, "rio-chunks".into());

        // A missing object is a miss (None / false), not an error.
        assert!(
            store
                .get_bytes("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .exists("replay/campaigns/c1/campaign.json")
                .await
                .unwrap()
        );
        // A present object round-trips its bytes.
        assert_eq!(
            store
                .get_bytes("replay/campaigns/c1/results.jsonl")
                .await
                .unwrap()
                .unwrap(),
            b"{\"job\":\"a\"}\n"
        );
    }

    /// [`ArtifactStore`] wrapper that fails `get_bytes` for keys with a
    /// given suffix while failures remain, then delegates (simulates a
    /// transient store error mid-restore).
    struct FailingGetStore {
        inner: LocalDirArtifactStore,
        fail_key_suffix: &'static str,
        failures_left: std::sync::Mutex<u32>,
    }

    #[async_trait]
    impl ArtifactStore for FailingGetStore {
        async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
            self.inner.put_bytes(key, bytes).await
        }

        async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
            if key.ends_with(self.fail_key_suffix) {
                let mut left = self.failures_left.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    anyhow::bail!("injected transient store failure for {key}");
                }
            }
            self.inner.get_bytes(key).await
        }

        async fn get_to_file(&self, key: &str, dest: &Path) -> Result<Option<(String, u64)>> {
            self.inner.get_to_file(key, dest).await
        }

        async fn exists(&self, key: &str) -> Result<bool> {
            self.inner.exists(key).await
        }
    }

    #[tokio::test]
    async fn failed_restore_leaves_no_sentinel_and_is_retried() {
        let adir = tempfile::tempdir().unwrap();
        let store = FailingGetStore {
            inner: LocalDirArtifactStore::new(adir.path()),
            fail_key_suffix: "/results.jsonl",
            failures_left: std::sync::Mutex::new(1),
        };
        store
            .put_bytes(
                "replay/campaigns/c1/campaign.json",
                b"{\"campaignId\":\"c1\"}".to_vec(),
            )
            .await
            .unwrap();
        let mut results = serde_json::to_vec(&rec("a")).unwrap();
        results.push(b'\n');
        store
            .put_bytes("replay/campaigns/c1/results.jsonl", results.clone())
            .await
            .unwrap();
        store
            .put_bytes("replay/campaigns/c1/markers/plan.done", b"done".to_vec())
            .await
            .unwrap();

        let sdir = tempfile::tempdir().unwrap();
        let state = StateDir::new(sdir.path()).unwrap();

        // First attempt: the results.jsonl download fails mid-restore.
        let err = download_state_if_missing(&state, &store, "replay/campaigns", "c1").await;
        assert!(err.is_err());
        // The local campaign.json is the restore-complete sentinel: if a
        // failed restore left it behind, every later start would skip the
        // restore, resume on near-empty state, and the periodic sync would
        // overwrite the durable store copies with that empty state.
        assert!(
            !state.path("campaign.json").exists(),
            "a failed restore must not leave the restore-complete sentinel behind"
        );

        // Second attempt (store healthy again): the full restore runs.
        let restored = download_state_if_missing(&state, &store, "replay/campaigns", "c1")
            .await
            .unwrap();
        assert!(restored);
        assert!(state.path("campaign.json").exists());
        assert_eq!(std::fs::read(state.path("results.jsonl")).unwrap(), results);
        assert!(state.path("markers/plan.done").exists());
    }

    #[tokio::test]
    async fn download_state_if_missing_restores_from_store() {
        let adir = tempfile::tempdir().unwrap();
        let store = LocalDirArtifactStore::new(adir.path());
        store
            .put_bytes(
                "replay/campaigns/c1/campaign.json",
                b"{\"campaignId\":\"c1\"}".to_vec(),
            )
            .await
            .unwrap();
        store
            .put_bytes("replay/campaigns/c1/results.jsonl", b"".to_vec())
            .await
            .unwrap();

        let sdir = tempfile::tempdir().unwrap();
        let state = StateDir::new(sdir.path()).unwrap();
        let restored = download_state_if_missing(&state, &store, "replay/campaigns", "c1")
            .await
            .unwrap();
        assert!(restored);
        assert!(state.path("campaign.json").exists());
        assert!(state.path("results.jsonl").exists());
        // The sentinel is written via tmp + rename; its scratch sibling
        // must not survive the restore.
        assert!(!state.path("campaign.tmp").exists());

        // Second call is a no-op (campaign.json now present locally).
        let again = download_state_if_missing(&state, &store, "replay/campaigns", "c1")
            .await
            .unwrap();
        assert!(!again);
    }
}
