//! Log-chunk storage: key scheme, line codec, and the storage backend trait.
//!
//! A "chunk" is one immutable zstd blob holding a contiguous run of log
//! lines for one execution, written exactly once and never overwritten.
//! Chunks are keyed by *position* (`exec_id`, `session_id`, `chunk_seq`),
//! not by content — two executions can emit identical bytes and still get
//! distinct objects. This is deliberately a separate abstraction from
//! [`crate::backend::ChunkBackend`] (BLAKE3 content-addressed NAR chunks
//! under `chunks/`): the keying, the prefix, the idempotence story, and
//! the delete path are all different.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::RwLock;

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata as _;
use tracing::debug;
use uuid::Uuid;

/// zstd compression level for log chunks.
///
/// Ported verbatim from the scheduler's flusher
/// (`rio-scheduler/src/logs/flush.rs::compress_with_prefix`, level 6):
/// build logs are highly compressible (~10:1 on typical compiler
/// output), and level 6 is the knee of the ratio/CPU curve for few-MiB
/// payloads.
const LOG_CHUNK_ZSTD_LEVEL: i32 = 6;

/// Upper bound on a single chunk's decompressed size.
///
/// 2x the ingest path's 8 MiB uncompressed cut threshold — a legitimate
/// chunk can never approach this. Defense in depth against a corrupt or
/// malicious zstd frame with a huge declared content size: the read
/// path decompresses one chunk per concurrent reader, so an unbounded
/// `read_to_end` would let one bad object OOM the replica.
const MAX_DECOMPRESSED_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

/// Build the S3 object key for one log chunk:
/// `logs/{drv_hash}/{exec_id}/{session_id}/{chunk_seq:08}.zst`.
///
/// - `drv_hash` is the [`rio_nix::store_path::drv_log_hash`] 32-char
///   form (NOT the `derivations.drv_hash` DAG key).
/// - The key is bucket-relative and complete — callers do not prepend
///   the store's chunk prefix. The `logs/` prefix is shared with the
///   legacy single-blob layout and is what the S3 lifecycle rule and
///   the store's IAM grant match on.
/// - The zero-padded `chunk_seq` keeps a session's objects
///   lexicographically ordered in S3 listings (debugging convenience
///   only; reads go through the `drv_log_chunks` manifest).
///
/// The format is pinned by `key_format_is_stable` — it is stored
/// verbatim in `drv_log_chunks.s3_key`, so changing it strands every
/// previously-written chunk.
pub fn log_chunk_key(drv_hash: &str, exec_id: &Uuid, session_id: &Uuid, chunk_seq: u32) -> String {
    format!("logs/{drv_hash}/{exec_id}/{session_id}/{chunk_seq:08}.zst")
}

/// Errors from the log-chunk codec and storage backends.
///
/// A typed enum (not `anyhow`) so the gRPC layer can map each failure
/// class to a precise status code, mirroring [`crate::error::MetadataError`]:
/// `NotFound` → the manifest points at a missing object (data loss,
/// surfaced loudly, not retried); `Backend` → transient or auth S3
/// failure (the inner `anyhow` chain roots at
/// [`crate::backend::BackendAuthError`] for permanent auth errors, so the
/// existing `grpc::storage_error` downcast keeps working); `Codec` → the
/// stored blob is not valid zstd (corruption).
#[derive(Debug, thiserror::Error)]
pub enum LogChunkError {
    /// The object is not in the backend. When the key came from a
    /// `drv_log_chunks` manifest row this is a data-loss condition, not
    /// a cache miss — the caller decides how loudly to surface it.
    #[error("log chunk not found: {key}")]
    NotFound { key: String },

    /// Backend I/O failure (S3 unreachable, throttled, auth denied).
    /// Permanent auth failures root the chain at
    /// [`crate::backend::BackendAuthError`].
    #[error("log chunk backend error: {0:#}")]
    Backend(#[source] anyhow::Error),

    /// The blob is not valid zstd / not a valid line payload.
    #[error("log chunk codec error: {0}")]
    Codec(#[source] std::io::Error),
}

/// Outcome of an idempotent [`LogChunkStore::put`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object did not exist and was written.
    Created,
    /// The object already existed; nothing was written.
    ///
    /// **Caller contract:** callers MUST guarantee that a re-PUT of an
    /// existing key would carry the identical bytes — in practice by
    /// never reusing a key for a different payload: mint a fresh
    /// `chunk_seq` for every cut *attempt* (not every cut *success*),
    /// so a retried cut never collides with a predecessor that may have
    /// committed without the caller seeing the response. Under that
    /// contract, `Existed` means "a previous attempt's PUT succeeded
    /// but its ack was lost" and the caller proceeds as if it had
    /// written the object.
    Existed,
}

/// zstd-compress log lines into a chunk blob.
///
/// Every line — including the last — is terminated with `\n`, matching
/// the scheduler's flusher convention ("its content always ends with
/// `\n`"). The terminator-on-every-line scheme is what makes the
/// trailing-empty-line case round-trip: `["a", ""]` encodes as
/// `"a\n\n"`, which [`decompress_lines`] strips one terminator from and
/// splits into exactly `["a", ""]`. Zero lines encode as a zero-byte
/// payload.
///
/// Pure sync function — callers compressing more than a trivial amount
/// of data run it under `tokio::task::spawn_blocking` (a few MiB of log
/// compresses in ~10-50 ms, long enough to stall a tokio worker).
pub fn compress_lines(lines: &[Vec<u8>]) -> Result<Vec<u8>, LogChunkError> {
    use std::io::Write as _;
    let payload_len: usize = lines.iter().map(|l| l.len() + 1).sum();
    let mut encoder =
        zstd::stream::Encoder::new(Vec::with_capacity(payload_len / 4), LOG_CHUNK_ZSTD_LEVEL)
            .map_err(LogChunkError::Codec)?;
    for line in lines {
        encoder.write_all(line).map_err(LogChunkError::Codec)?;
        encoder.write_all(b"\n").map_err(LogChunkError::Codec)?;
    }
    encoder.finish().map_err(LogChunkError::Codec)
}

/// Decompress a chunk blob back into its log lines.
///
/// Inverse of [`compress_lines`]: decompress, strip exactly one trailing
/// `\n` (the final line's terminator), split on `\n`. An empty
/// decompressed payload is zero lines (not one empty line) — see
/// `codec_roundtrips_single_empty_line_vs_no_lines`.
///
/// Refuses to decompress past [`MAX_DECOMPRESSED_CHUNK_BYTES`] (a
/// `Codec` error) so a corrupt frame cannot balloon into an unbounded
/// allocation.
pub fn decompress_lines(blob: &[u8]) -> Result<Vec<Vec<u8>>, LogChunkError> {
    let decoder = zstd::stream::Decoder::new(blob).map_err(LogChunkError::Codec)?;
    let mut decoded = Vec::new();
    // .take(N+1): if the frame yields more than N bytes we read exactly
    // N+1 of them and can tell "over the bound" apart from "exactly at
    // the bound" without decompressing the rest.
    decoder
        .take(MAX_DECOMPRESSED_CHUNK_BYTES + 1)
        .read_to_end(&mut decoded)
        .map_err(LogChunkError::Codec)?;
    if decoded.len() as u64 > MAX_DECOMPRESSED_CHUNK_BYTES {
        return Err(LogChunkError::Codec(std::io::Error::other(format!(
            "chunk decompresses past the {MAX_DECOMPRESSED_CHUNK_BYTES}-byte bound"
        ))));
    }
    if decoded.is_empty() {
        return Ok(Vec::new());
    }
    // Strip the final line's terminator before splitting so split('\n')
    // yields exactly the encoded lines (and not a phantom trailing empty
    // line). A blob produced by compress_lines always ends with '\n';
    // tolerate one that doesn't rather than erroring (the legacy flusher
    // has the same unwrap_or fallback).
    let raw: &[u8] = decoded.strip_suffix(b"\n").unwrap_or(&decoded);
    Ok(raw.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect())
}

/// Storage backend for log chunks.
///
/// Two impls: [`S3LogChunkStore`] (prod) and [`MemoryLogChunkStore`]
/// (tests). Keys are the full bucket-relative strings produced by
/// [`log_chunk_key`]; the trait does no key construction or validation
/// of its own.
#[async_trait::async_trait]
pub trait LogChunkStore: Send + Sync + 'static {
    /// Store a chunk with `If-None-Match: *` semantics: an object that
    /// already exists is `Ok(PutOutcome::Existed)`, not an error. A
    /// retried PUT of the same key carries the same bytes by
    /// construction (see [`PutOutcome::Existed`]), so "lost the race"
    /// and "won the race" are both success.
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError>;

    /// Fetch a chunk. [`LogChunkError::NotFound`] if absent.
    async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError>;

    /// Best-effort batch delete for the TTL sweep. Missing keys are not
    /// an error (the sweep may retry a partially-processed batch, and
    /// the S3 lifecycle rule may have expired an object first).
    async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError>;
}

// ============================================================================
// S3 backend (prod)
// ============================================================================

/// S3-backed log-chunk storage.
///
/// Not unit-tested here — the in-process tests use
/// [`MemoryLogChunkStore`]; this impl's behavior (the `If-None-Match`
/// 412 mapping in particular) is exercised by the `log-service` VM
/// scenario against a real S3 API.
pub struct S3LogChunkStore {
    client: Client,
    bucket: String,
}

impl S3LogChunkStore {
    /// `client`/`bucket` are the same ones `S3ChunkBackend` uses — log
    /// chunks live in the chunks bucket under the `logs/` prefix. No
    /// additional key prefix is applied (see [`log_chunk_key`]).
    pub fn new(client: Client, bucket: String) -> Self {
        Self { client, bucket }
    }
}

#[async_trait::async_trait]
impl LogChunkStore for S3LogChunkStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
        debug!(bucket = %self.bucket, key, size = body.len(), "S3LogChunkStore: uploading");
        metrics::counter!("rio_store_s3_requests_total", "operation" => "put_object").increment(1);
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .content_type("application/zstd")
            .body(body.into())
            .send()
            .await
        {
            Ok(_) => Ok(PutOutcome::Created),
            Err(err) => {
                // `If-None-Match: *` makes S3 reject the PUT with HTTP 412
                // (code "PreconditionFailed") when the object already
                // exists. That is the idempotent-retry success path, not a
                // failure.
                //
                // A 409 ("ConditionalRequestConflict") is deliberately NOT
                // treated as Existed: it means a concurrent operation
                // prevented S3 from evaluating the condition at all — it
                // does NOT assert the object exists. Mapping it to Existed
                // would let the caller record a manifest row and ack lines
                // for an object that may never have been written. AWS's
                // guidance is to retry; letting it fall through to the
                // Backend error path hands it to the ingest task's
                // cut-retry machinery, whose next attempt gets a clean
                // 200 or a definitive 412.
                let service_err = err.into_service_error();
                if service_err.code() == Some("PreconditionFailed") {
                    return Ok(PutOutcome::Existed);
                }
                Err(LogChunkError::Backend(crate::backend::classify_s3_error(
                    service_err,
                    format!("S3 PutObject failed for {key}"),
                )))
            }
        }
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
        metrics::counter!("rio_store_s3_requests_total", "operation" => "get_object").increment(1);
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => Ok(output
                .body
                .collect()
                .await
                .map_err(|e| {
                    LogChunkError::Backend(anyhow::anyhow!("S3 body read failed for {key}: {e}"))
                })?
                .into_bytes()
                .to_vec()),
            Err(err) => {
                let service_err = err.into_service_error();
                if service_err.is_no_such_key() {
                    Err(LogChunkError::NotFound {
                        key: key.to_string(),
                    })
                } else {
                    Err(LogChunkError::Backend(crate::backend::classify_s3_error(
                        service_err,
                        format!("S3 GetObject failed for {key}"),
                    )))
                }
            }
        }
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError> {
        // Sequential DeleteObject calls. The TTL sweep deletes in batches
        // of a few hundred keys once an hour — not a hot path. S3's
        // DeleteObjects (plural) API would do 1000 per request but
        // requires assembling the XML delete document and parsing
        // per-key errors out of the response; not worth it until the
        // sweep's request count is a measured problem.
        for key in keys {
            metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_object")
                .increment(1);
            // DeleteObject is idempotent: deleting a non-existent key
            // returns success.
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| {
                    LogChunkError::Backend(crate::backend::classify_s3_error(
                        e,
                        format!("S3 DeleteObject failed for {key}"),
                    ))
                })?;
        }
        Ok(())
    }
}

// ============================================================================
// Filesystem backend (dev / standalone VM)
// ============================================================================

/// Filesystem-backed log-chunk storage for deployments without S3 (the
/// standalone VM tests, single-node dev stores). One file per key under
/// `root`, with the key's `/` separators mapped to directories — so the
/// on-disk layout mirrors the S3 layout and survives a store process
/// restart, which is the property the `log-service` VM scenario
/// exercises.
///
/// **Idempotence.** `put` uses `O_EXCL` (`create_new`): an existing
/// file is [`PutOutcome::Existed`]. A file can exist with partial
/// content only if a previous `put` died mid-write — and that key can
/// never be re-`put` (the cutter mints a fresh `chunk_seq` per attempt)
/// nor read (its manifest row was never inserted), so the partial file
/// is an invisible orphan, not a correctness hazard. There is no
/// tmp-and-rename dance for the same reason.
///
/// **Key validation.** `get`/`delete_batch` keys come from
/// `drv_log_chunks.s3_key` rows; a corrupt or hand-edited row must not
/// be able to escape `root`, so every component is checked against
/// `..`/`.`/empty before the path is built.
pub struct FilesystemLogChunkStore {
    root: std::path::PathBuf,
}

impl FilesystemLogChunkStore {
    /// Create the root directory (and parents) if absent.
    pub fn new(root: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Map a chunk key to a path under `root`, rejecting traversal.
    fn path_for(&self, key: &str) -> Result<std::path::PathBuf, LogChunkError> {
        if key.starts_with('/')
            || key
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(LogChunkError::Backend(anyhow::anyhow!(
                "refusing log chunk key with traversal/empty components: {key:?}"
            )));
        }
        let mut path = self.root.clone();
        path.extend(key.split('/'));
        Ok(path)
    }
}

#[async_trait::async_trait]
impl LogChunkStore for FilesystemLogChunkStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
        let path = self.path_for(key)?;
        debug!(path = %path.display(), size = body.len(), "FilesystemLogChunkStore: storing chunk");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_backend(e, "create_dir_all", key))?;
        }
        // O_EXCL gives the same "first writer wins" semantics as the S3
        // backend's `If-None-Match: *`.
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(PutOutcome::Existed);
            }
            Err(e) => return Err(io_backend(e, "create", key)),
        };
        use tokio::io::AsyncWriteExt as _;
        file.write_all(&body)
            .await
            .map_err(|e| io_backend(e, "write", key))?;
        // Make the chunk durable before the caller inserts the manifest
        // row that makes it reachable — the same ordering the S3
        // backend gets for free from PutObject's response semantics.
        file.sync_all()
            .await
            .map_err(|e| io_backend(e, "sync", key))?;
        Ok(PutOutcome::Created)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
        let path = self.path_for(key)?;
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(LogChunkError::NotFound {
                key: key.to_string(),
            }),
            Err(e) => Err(io_backend(e, "read", key)),
        }
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError> {
        for key in keys {
            let path = self.path_for(key)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                // Missing keys are not an error (the sweep may retry a
                // partially-processed batch).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_backend(e, "remove", key)),
            }
        }
        Ok(())
    }
}

/// Wrap a filesystem I/O error as a backend error with enough context
/// to find the failing key.
fn io_backend(e: std::io::Error, op: &str, key: &str) -> LogChunkError {
    LogChunkError::Backend(anyhow::anyhow!("filesystem {op} failed for {key}: {e}"))
}

// ============================================================================
// Memory backend (tests)
// ============================================================================

/// In-memory log-chunk storage for tests.
///
/// Recovers from `RwLock` poison (uses `into_inner`) so a panicking test
/// doesn't cascade into other tests sharing the store, mirroring
/// [`crate::backend::MemoryChunkBackend`].
#[derive(Default)]
pub struct MemoryLogChunkStore {
    objects: RwLock<HashMap<String, Vec<u8>>>,
}

impl MemoryLogChunkStore {
    /// Test helper: number of stored chunks.
    pub fn len(&self) -> usize {
        self.objects.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test helper: every stored key, sorted. For asserting which chunks
    /// an ingest session actually cut.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .objects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

#[async_trait::async_trait]
impl LogChunkStore for MemoryLogChunkStore {
    async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
        let mut objects = self.objects.write().unwrap_or_else(|e| e.into_inner());
        if objects.contains_key(key) {
            // Match S3's If-None-Match semantics: the existing object is
            // left untouched and the caller is told it already existed.
            return Ok(PutOutcome::Existed);
        }
        objects.insert(key.to_string(), body);
        Ok(PutOutcome::Created)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
        self.objects
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| LogChunkError::NotFound {
                key: key.to_string(),
            })
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError> {
        let mut objects = self.objects.write().unwrap_or_else(|e| e.into_inner());
        for key in keys {
            objects.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three storage-semantics tests, run against both the memory
    /// and the filesystem backends: idempotent put, NotFound-distinguishing
    /// get, missing-tolerant batch delete. The S3 backend's equivalents
    /// are exercised by the `log-service` VM scenario.
    async fn assert_put_is_idempotent(store: &dyn LogChunkStore) {
        let outcome1 = store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        let outcome2 = store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        assert_eq!(outcome1, PutOutcome::Created);
        assert_eq!(outcome2, PutOutcome::Existed);
        assert_eq!(
            store.get("logs/a/b/c/00000000.zst").await.unwrap(),
            b"data".to_vec()
        );
    }

    async fn assert_get_missing_is_not_found(store: &dyn LogChunkStore) {
        match store.get("logs/missing").await {
            Err(LogChunkError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    async fn assert_delete_batch_ignores_missing(store: &dyn LogChunkStore) {
        store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        store
            .delete_batch(&[
                "logs/a/b/c/00000000.zst".to_string(),
                "logs/never/existed.zst".to_string(),
            ])
            .await
            .unwrap();
        assert!(matches!(
            store.get("logs/a/b/c/00000000.zst").await,
            Err(LogChunkError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn filesystem_store_put_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemLogChunkStore::new(dir.path()).unwrap();
        assert_put_is_idempotent(&store).await;
    }

    #[tokio::test]
    async fn filesystem_store_get_missing_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemLogChunkStore::new(dir.path()).unwrap();
        assert_get_missing_is_not_found(&store).await;
    }

    #[tokio::test]
    async fn filesystem_store_delete_batch_ignores_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemLogChunkStore::new(dir.path()).unwrap();
        assert_delete_batch_ignores_missing(&store).await;
    }

    /// The whole point of the filesystem backend: a chunk written by one
    /// process incarnation is readable by the next (the standalone VM
    /// test restarts the store process and the logs must survive).
    #[tokio::test]
    async fn filesystem_store_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = FilesystemLogChunkStore::new(dir.path()).unwrap();
            store
                .put("logs/h/e/s/00000000.zst", b"persisted".to_vec())
                .await
                .unwrap();
        }
        let reopened = FilesystemLogChunkStore::new(dir.path()).unwrap();
        assert_eq!(
            reopened.get("logs/h/e/s/00000000.zst").await.unwrap(),
            b"persisted".to_vec()
        );
    }

    /// A key from a hostile or corrupt manifest row must not escape the
    /// root directory.
    #[tokio::test]
    async fn filesystem_store_rejects_traversal_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemLogChunkStore::new(dir.path()).unwrap();
        for key in [
            "../escape.zst",
            "logs/../../escape.zst",
            "/abs/path.zst",
            "logs//x.zst",
        ] {
            assert!(
                store.put(key, b"x".to_vec()).await.is_err(),
                "put({key}) should be rejected"
            );
            assert!(
                store.get(key).await.is_err(),
                "get({key}) should be rejected"
            );
        }
        // Nothing escaped the tempdir.
        assert!(!dir.path().parent().unwrap().join("escape.zst").exists());
    }

    #[test]
    fn key_format_is_stable() {
        // The key format is load-bearing: it is stored verbatim in
        // drv_log_chunks.s3_key and the S3 lifecycle rule matches on the
        // logs/ prefix. Pin it exactly.
        let exec = Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap();
        let sess = Uuid::parse_str("01900000-0000-7000-8000-000000000002").unwrap();
        let key = log_chunk_key("0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm", &exec, &sess, 7);
        assert_eq!(
            key,
            "logs/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm/01900000-0000-7000-8000-000000000001/01900000-0000-7000-8000-000000000002/00000007.zst"
        );
    }

    #[test]
    fn codec_roundtrips_lines_including_non_utf8() {
        let lines: Vec<Vec<u8>> = vec![
            b"building /nix/store/...".to_vec(),
            vec![0xff, 0xfe, b'x'], // non-UTF-8 — build output is raw bytes
            b"".to_vec(),           // empty line
            b"line with\ttab".to_vec(),
        ];
        let blob = compress_lines(&lines).unwrap();
        assert_eq!(decompress_lines(&blob).unwrap(), lines);
    }

    /// A frame whose decompressed size exceeds [`MAX_DECOMPRESSED_CHUNK_BYTES`]
    /// is rejected instead of ballooning into an unbounded allocation. A
    /// 17 MiB payload of zeros compresses to a few KiB — exactly the
    /// shape of a corrupt-or-malicious "small object, huge content" frame.
    #[test]
    fn codec_rejects_oversized_decompressed_payload() {
        let huge_line = vec![0u8; (MAX_DECOMPRESSED_CHUNK_BYTES + 1024) as usize];
        let blob = compress_lines(&[huge_line]).unwrap();
        assert!(
            blob.len() < 64 * 1024,
            "precondition: the bomb must be small compressed ({} bytes)",
            blob.len()
        );
        match decompress_lines(&blob) {
            Err(LogChunkError::Codec(_)) => {}
            other => panic!("expected Codec error for oversized payload, got {other:?}"),
        }
    }

    /// The trailing-empty-line ambiguity: after a join('\n')/split('\n')
    /// round trip, `["a", ""]` and `["a"]` are indistinguishable unless
    /// the codec terminates every line (including the last) with the
    /// delimiter and the decoder strips exactly one terminator before
    /// splitting. This is the convention the scheduler's flusher/reader
    /// pair already uses; pin it here so the two codecs agree for as long
    /// as both exist.
    #[test]
    fn codec_roundtrips_trailing_empty_line() {
        let lines: Vec<Vec<u8>> = vec![b"a".to_vec(), b"".to_vec()];
        let blob = compress_lines(&lines).unwrap();
        assert_eq!(decompress_lines(&blob).unwrap(), lines);
    }

    #[test]
    fn codec_roundtrips_single_empty_line_vs_no_lines() {
        // Zero lines -> empty payload -> zero lines.
        let none: Vec<Vec<u8>> = vec![];
        assert_eq!(
            decompress_lines(&compress_lines(&none).unwrap()).unwrap(),
            none
        );
        // One empty line -> a lone terminator -> one empty line.
        let one_empty: Vec<Vec<u8>> = vec![b"".to_vec()];
        assert_eq!(
            decompress_lines(&compress_lines(&one_empty).unwrap()).unwrap(),
            one_empty
        );
    }

    #[test]
    fn codec_rejects_garbage() {
        assert!(decompress_lines(b"not zstd").is_err());
    }

    #[tokio::test]
    async fn memory_store_put_is_idempotent() {
        let store = MemoryLogChunkStore::default();
        let outcome1 = store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        let outcome2 = store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        assert_eq!(outcome1, PutOutcome::Created);
        assert_eq!(outcome2, PutOutcome::Existed);
        assert_eq!(
            store.get("logs/a/b/c/00000000.zst").await.unwrap(),
            b"data".to_vec()
        );
    }

    #[tokio::test]
    async fn memory_store_get_missing_is_not_found() {
        let store = MemoryLogChunkStore::default();
        // The error variant must distinguish "object absent" (a manifest
        // row pointing at a missing object = data loss, surfaced as a
        // distinct condition) from transient backend failures (retried).
        match store.get("logs/missing").await {
            Err(LogChunkError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_store_delete_batch_ignores_missing_keys() {
        let store = MemoryLogChunkStore::default();
        store
            .put("logs/a/b/c/00000000.zst", b"data".to_vec())
            .await
            .unwrap();
        store
            .delete_batch(&[
                "logs/a/b/c/00000000.zst".to_string(),
                "logs/never/existed.zst".to_string(),
            ])
            .await
            .unwrap();
        assert!(matches!(
            store.get("logs/a/b/c/00000000.zst").await,
            Err(LogChunkError::NotFound { .. })
        ));
    }
}
