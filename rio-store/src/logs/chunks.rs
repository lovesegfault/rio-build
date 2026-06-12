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
/// (ported from the retired scheduler-side log-flush codec, level 6):
/// build logs are highly compressible (~10:1 on typical compiler
/// output), and level 6 is the knee of the ratio/CPU curve for few-MiB
/// payloads.
const LOG_CHUNK_ZSTD_LEVEL: i32 = 6;

/// Upper bound on a single chunk's framed payload, in BOTH directions —
/// the kernel owns the constant ([`rio_log_kernel::MAX_CHUNK_PAYLOAD_BYTES`])
/// so the cutter's prospective arithmetic and this module's
/// compress/decompress refusals can never drift apart
/// (`store.log.write-read-bound`). Defense in depth on the read side
/// against a corrupt or malicious zstd frame with a huge declared
/// content size: the read path decompresses one chunk per concurrent
/// reader, so an unbounded `read_to_end` would let one bad object OOM
/// the replica.
pub(super) const MAX_DECOMPRESSED_CHUNK_BYTES: u64 = rio_log_kernel::MAX_CHUNK_PAYLOAD_BYTES;

// A truncated line (64 KiB) plus its frame prefix always fits a chunk:
// the kernel's ≥1-on-non-empty escape (a single line is always
// cuttable) can never produce a chunk the read path refuses.
const _: () = assert!(
    super::ingest::MAX_LINE_LEN as u64 + rio_log_kernel::LINE_LEN_PREFIX_BYTES
        <= rio_log_kernel::MAX_CHUNK_PAYLOAD_BYTES
);

// r[impl store.log.chunk-immutable]
// r[impl obs.log.exec-keyed+2]
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

/// Width of the per-line length prefix in the chunk payload (kernel-owned;
/// see `store.log.write-read-bound`).
const LINE_LEN_PREFIX_BYTES: usize = rio_log_kernel::LINE_LEN_PREFIX_BYTES as usize;

/// zstd-compress log lines into a chunk blob.
///
/// Each line is framed as a little-endian `u32` byte length followed by
/// the line's raw bytes. Length-prefix framing keeps the codec
/// injective over **arbitrary** line content: a line is whatever byte
/// string the worker sent (truncated to `MAX_LINE_LEN`), including
/// embedded `\n`, `\r\n`, NULs, and the empty string, and it must come
/// back out as exactly one line with exactly those bytes. (The previous
/// `\n`-terminated framing split a line containing `0x0A` into two at
/// read time, shifting every subsequent line's positional attribution
/// and serving line numbers the manifest row never claimed.) Zero lines
/// encode as a zero-byte payload.
///
/// A line longer than `u32::MAX` cannot be framed and is rejected as a
/// `Codec` error — unreachable from the ingest path, which truncates to
/// `MAX_LINE_LEN` (64 KiB) before cutting.
///
/// Pure sync function — callers compressing more than a trivial amount
/// of data run it under `tokio::task::spawn_blocking` (a few MiB of log
/// compresses in ~10-50 ms, long enough to stall a tokio worker).
pub fn compress_lines(lines: &[Vec<u8>]) -> Result<Vec<u8>, LogChunkError> {
    use std::io::Write as _;
    let payload_len: usize = lines.iter().map(|l| l.len() + LINE_LEN_PREFIX_BYTES).sum();
    // r[impl store.log.write-read-bound+2]
    // Defense in depth behind the cutter's bounded_contiguous_prefix_len:
    // a chunk this function would frame past the shared payload bound is
    // a chunk decompress_lines will refuse — refuse it HERE, before the
    // PUT, so a committed chunk is decodable by construction.
    if payload_len as u64 > MAX_DECOMPRESSED_CHUNK_BYTES {
        return Err(LogChunkError::Codec(std::io::Error::other(format!(
            "framed chunk payload of {payload_len} bytes exceeds the \
             {MAX_DECOMPRESSED_CHUNK_BYTES}-byte shared write/read bound"
        ))));
    }
    let mut encoder =
        zstd::stream::Encoder::new(Vec::with_capacity(payload_len / 4), LOG_CHUNK_ZSTD_LEVEL)
            .map_err(LogChunkError::Codec)?;
    for line in lines {
        let len = u32::try_from(line.len()).map_err(|_| {
            LogChunkError::Codec(std::io::Error::other(format!(
                "log line of {} bytes exceeds the u32 length-prefix range",
                line.len()
            )))
        })?;
        encoder
            .write_all(&len.to_le_bytes())
            .map_err(LogChunkError::Codec)?;
        encoder.write_all(line).map_err(LogChunkError::Codec)?;
    }
    encoder.finish().map_err(LogChunkError::Codec)
}

/// Decompress a chunk blob back into its log lines.
///
/// Inverse of [`compress_lines`]: decompress, then walk the payload
/// reading one `u32` little-endian length prefix and that many content
/// bytes per line. An empty decompressed payload is zero lines (not one
/// empty line) — see `codec_roundtrips_single_empty_line_vs_no_lines`.
/// A payload that ends mid-prefix or whose last prefix runs past the
/// end is corrupt (`Codec` error) — the framing is self-delimiting, so
/// a well-formed payload always consumes exactly to the end.
///
/// Refuses to decompress past `MAX_DECOMPRESSED_CHUNK_BYTES` (a
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
    let mut lines = Vec::new();
    let mut rest: &[u8] = &decoded;
    while !rest.is_empty() {
        // r[impl store.log.write-read-bound+2]
        // The absolute line-count backstop (bug_298's read axis). The
        // byte bound above already implies it — every framed line costs
        // at least LINE_LEN_PREFIX_BYTES, so a within-bound payload
        // holds at most MAX_CHUNK_LINES_ABS lines — but the implication
        // lives in two constants an edit could split; checking the
        // count directly keeps the allocation bound local and survives
        // any future re-tuning of the byte bound.
        if lines.len() as u64 >= rio_log_kernel::MAX_CHUNK_LINES_ABS {
            return Err(LogChunkError::Codec(std::io::Error::other(format!(
                "chunk holds more than {} lines (absolute read bound)",
                rio_log_kernel::MAX_CHUNK_LINES_ABS
            ))));
        }
        let Some((prefix, after_prefix)) = rest.split_at_checked(LINE_LEN_PREFIX_BYTES) else {
            return Err(LogChunkError::Codec(std::io::Error::other(format!(
                "chunk payload ends mid length-prefix ({} trailing bytes)",
                rest.len()
            ))));
        };
        let len = u32::from_le_bytes(
            prefix
                .try_into()
                .expect("split_at_checked returned 4 bytes"),
        ) as usize;
        let Some((content, after_content)) = after_prefix.split_at_checked(len) else {
            return Err(LogChunkError::Codec(std::io::Error::other(format!(
                "chunk line length prefix ({len} bytes) runs past the payload \
                 ({} bytes remain)",
                after_prefix.len()
            ))));
        };
        lines.push(content.to_vec());
        rest = after_content;
    }
    Ok(lines)
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
            // D5: per-attempt deadline at the seam (s3-op-census row:
            // log put_object, zstd body <= ~8 MiB worst).
            .customize()
            .config_override(crate::backend::log_op_override())
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
            // D5: per-attempt deadline at the seam (s3-op-census row:
            // log get_object); the body collect below carries its own
            // clock — the attempt timeout covers only headers.
            .customize()
            .config_override(crate::backend::log_op_override())
            .send()
            .await
        {
            Ok(output) => Ok(tokio::time::timeout(
                crate::backend::LOG_GET_BODY_TIMEOUT,
                output.body.collect(),
            )
            .await
            .map_err(|_| {
                LogChunkError::Backend(anyhow::anyhow!(
                    "S3 body read for {key} exceeded its typed bound ({:?}) — \
                     peer presumed black-holed",
                    crate::backend::LOG_GET_BODY_TIMEOUT
                ))
            })?
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
            // D5: per-attempt deadline at the seam (s3-op-census
            // row: log delete_object, no body).
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .customize()
                .config_override(crate::backend::log_op_override())
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
    /// Create the root directory (and parents) if absent, and make the
    /// CREATION durable (merged_bug_033): the chain law's fixpoint is
    /// "end at the deepest pre-existing directory", and whoever
    /// creates the anchor discharges that base case — `fsync_chain`
    /// hard-stops at the root by design, so on a fresh volume the
    /// root's own dirent is the one link no put ever makes durable.
    /// `new()` therefore fsyncs every level `create_dir_all` actually
    /// created PLUS the deepest pre-existing ancestor (whose data
    /// block now names the topmost created level), child-to-root —
    /// after which the per-put chain genuinely ends at durable ground.
    /// A pre-existing root keeps the old single-fsync behavior.
    // r[impl store.log.chunk-dirent-durable+2]
    pub fn new(root: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        // Pre-scan BEFORE creating: which levels will be new?
        let mut created = Vec::new();
        let mut cursor = root.clone();
        loop {
            if cursor.exists() {
                break;
            }
            created.push(cursor.clone());
            match cursor.parent() {
                Some(p) => cursor = p.to_path_buf(),
                None => break,
            }
        }
        let deepest_preexisting = cursor;
        std::fs::create_dir_all(&root)?;
        if created.is_empty() {
            // Nothing created: fsync the root's contents, as ever.
            fsync_dir_sync(&root)?;
        } else {
            // Child-to-root over the created levels (the root is
            // `created[0]` whenever it was new), then the base case.
            for dir in &created {
                fsync_dir_sync(dir)?;
            }
            fsync_dir_sync(&deepest_preexisting)?;
        }
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

/// fsync a DIRECTORY so the dirents created inside it are durable.
/// POSIX: `sync_all` on the file makes its CONTENT durable, but the
/// entry naming it lives in the parent directory's data — a crash
/// between file-sync and dirent-sync resurfaces as a missing chunk
/// whose manifest row exists. Called child-to-root over every ancestor
/// `durable_put` newly created, ending at the deepest pre-existing
/// directory.
async fn fsync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(test)]
    fsync_recorder::record(dir);
    let f = tokio::fs::File::open(dir).await?;
    f.sync_all().await
}

/// Synchronous twin of [`fsync_dir`] for construction-time use
/// (`new()` is sync): same recorder, same semantics. `pub(crate)` so
/// the CAS backend's construction (`crate::backend`) discharges its
/// base case through the SAME recorded chokepoint instead of minting
/// a second unwitnessed fsync path.
pub(crate) fn fsync_dir_sync(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(test)]
    fsync_recorder::record(dir);
    std::fs::File::open(dir)?.sync_all()
}

/// Test-only fsync_dir call recorder (sequence assertions).
#[cfg(test)]
pub(crate) mod fsync_recorder {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static LOG: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(crate) fn record(dir: &Path) {
        LOG.lock().unwrap().push(dir.to_path_buf());
    }

    /// EVERY recorded dir fsync, in call order — the LAW'S domain
    /// (merged_bug_033): the chain law obligates fsyncs up to and
    /// including the deepest pre-existing directory, which sits
    /// OUTSIDE the store root, so a witness scoped `under(root)` is
    /// structurally incapable of observing the base case. Base-case
    /// witnesses consume this; subtree-scoped assertions may still
    /// use [`under`] when the law they pin ends at the root.
    pub(crate) fn all() -> Vec<PathBuf> {
        LOG.lock().unwrap().clone()
    }

    /// All recorded dir fsyncs under `root`, in call order. A QUERY
    /// scope, not the law's domain — see [`all`].
    pub(crate) fn under(root: &Path) -> Vec<PathBuf> {
        LOG.lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with(root))
            .cloned()
            .collect()
    }
}

impl FilesystemLogChunkStore {
    // r[impl store.log.chunk-dirent-durable+2]
    /// PER-PUT SELF-SUFFICIENCY (bug_120): fsync the chunk's FULL
    /// ancestor chain, child-to-root through the store root,
    /// unconditionally — 3-4 directories, idempotent, cheap. The
    /// predecessor recipe classified ancestors with a point-in-time
    /// `exists()` probe racing a sibling put's `create_dir_all`,
    /// while the creator's fsync loop ran only after its multi-MiB
    /// body write and its error paths returned with no dir fsyncs:
    /// observing a directory said nothing about its dirent's
    /// durability, so a put could return `Created` (manifest row
    /// committed) with its chain's durability resting on a sibling
    /// under no obligation to complete — an implicit cross-task
    /// obligation transfer with no handoff protocol. Deleting the
    /// probe deletes the transfer: every `Created`/`Existed` implies
    /// its full chain was fsynced by THIS put.
    async fn fsync_chain(&self, path: &std::path::Path, key: &str) -> Result<(), LogChunkError> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let mut cursor = parent;
        loop {
            fsync_dir(cursor)
                .await
                .map_err(|e| io_backend(e, "fsync_dir", key))?;
            if cursor == self.root {
                break;
            }
            match cursor.parent() {
                Some(p) => cursor = p,
                None => break,
            }
        }
        Ok(())
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
                // Existed is a durability statement too: the creator
                // may have died between its body sync and its dir
                // fsyncs, and the caller inserts a manifest row either
                // way. The chain fsync is per PUT, never per outcome.
                self.fsync_chain(&path, key).await?;
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
        self.fsync_chain(&path, key).await?;
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
    // r[verify store.log.chunk-dirent-durable+2]
    /// W12-I (merged_bug_033, red-first): the chain law's BASE CASE is
    /// discharged by whoever CREATES the anchor. On a fresh volume
    /// `new()` creates the root (and possibly ancestors); the root's
    /// own dirent then lives in a directory `fsync_chain` never
    /// touches (it hard-stops at root), and the frozen witness
    /// `fsync_recorder::under(root)` was STRUCTURALLY incapable of
    /// observing the parent fsync (`starts_with(root)` filters it out
    /// — the measure could not entail the law at the base case). The
    /// re-scoped query (`all()`, the law's domain: every fsync the
    /// law obligates) makes the gap observable; pre-fix RED: with the
    /// recorder able to see, `new()` records NOTHING above the root —
    /// a crash after put() loses the whole store's dirent while
    /// manifest rows claim durable coverage.
    #[tokio::test]
    async fn new_discharges_the_dirent_chain_base_case() {
        use super::fsync_recorder;
        let tmp = tempfile::tempdir().unwrap();
        // A multi-level fresh-volume shape: every level below the
        // tempdir is created by new().
        let root = tmp.path().join("a").join("b").join("store");
        let baseline = fsync_recorder::all().len();
        let _store = super::FilesystemLogChunkStore::new(&root).unwrap();
        let recorded = fsync_recorder::all()[baseline..].to_vec();

        // The created levels, child-to-root, then the deepest
        // PRE-EXISTING directory (the tempdir — the one whose data
        // block now names "a/"). Pre-fix the record held only the
        // root's own contents fsync.
        assert_eq!(
            recorded,
            vec![
                root.clone(),
                tmp.path().join("a").join("b"),
                tmp.path().join("a"),
                tmp.path().to_path_buf(),
            ],
            "new() must fsync every level it created plus the deepest \
             pre-existing ancestor (the base case the per-put chain \
             law assumes)"
        );

        // The no-creation cell: a pre-existing root keeps the old
        // single-fsync behavior byte-stable.
        let baseline = fsync_recorder::all().len();
        let _store2 = super::FilesystemLogChunkStore::new(&root).unwrap();
        let recorded = fsync_recorder::all()[baseline..].to_vec();
        assert_eq!(
            recorded,
            vec![root.clone()],
            "a pre-existing root is fsynced once, exactly as before"
        );
    }

    // r[verify store.log.chunk-dirent-durable+2]
    /// The backend sibling (merged_bug_033's sweep face): the chunk
    /// CAS backend creates `chunks/` plus 256 subdirectories with —
    /// pre-fix — ZERO directory fsyncs; `put()` syncs only the
    /// `{aa}/` parent. The same base-case discipline applies: `new()`
    /// fsyncs every created level (the `chunks/` listing covers all
    /// 256 subdir dirents) plus the deepest pre-existing ancestor.
    #[tokio::test]
    async fn backend_new_discharges_the_dirent_chain_base_case() {
        use super::fsync_recorder;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("data");
        let baseline = fsync_recorder::all().len();
        let _backend = crate::backend::FilesystemChunkBackend::new(&base).unwrap();
        let recorded = fsync_recorder::all()[baseline..].to_vec();
        // 256 subdir fsyncs (their contents are empty but their
        // creation is what the chunks/ fsync makes durable), then the
        // created levels child-to-root (chunks/, data/), then the
        // pre-existing tempdir.
        assert!(
            recorded.contains(&base.join("chunks")),
            "the chunks/ listing (256 subdir dirents) must be fsynced: {recorded:?}"
        );
        assert!(
            recorded.contains(&base),
            "the created data/ level must be fsynced"
        );
        assert!(
            recorded.contains(&tmp.path().to_path_buf()),
            "the deepest pre-existing ancestor must be fsynced (the base case)"
        );
    }

    // r[verify store.log.chunk-dirent-durable+2]
    #[tokio::test]
    async fn filesystem_put_fsyncs_new_ancestors_child_to_root() {
        use super::{LogChunkStore as _, fsync_recorder};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("chunks");
        let store = super::FilesystemLogChunkStore::new(&root).unwrap();
        // Construction fsyncs the root once — the dirent chain for
        // every later put ends at a durable root.
        assert_eq!(
            fsync_recorder::under(&root),
            vec![root.clone()],
            "FilesystemLogChunkStore::new must fsync the root directory"
        );
        let key = "logs/ab/cd/chunk-000001";
        let outcome = store.put(key, b"line".to_vec()).await.unwrap();
        assert!(matches!(outcome, super::PutOutcome::Created));
        let after = fsync_recorder::under(&root);
        // After the put: the three newly-created ancestors child-to-
        // root, then the deepest PRE-EXISTING dir (the root itself —
        // its data block now names "logs/").
        assert_eq!(
            after[1..],
            vec![
                root.join("logs/ab/cd"),
                root.join("logs/ab"),
                root.join("logs"),
                root.clone(),
            ][..],
            "put must fsync each new ancestor (child→root) and the \
             pre-existing parent that gained a dirent"
        );
    }

    /// W11-P (bug_120). Proposition: **`Created` entails this put's
    /// OWN full-chain fsync** — at the recipe's own population:
    /// shared-prefix concurrent puts. The pre-fix recipe classified
    /// ancestors with an unsynchronized point-in-time `exists()`
    /// probe racing a sibling put's `create_dir_all`, while the
    /// creator's dir-fsync loop ran only AFTER its multi-MiB body
    /// write and its error paths returned with no dir fsyncs: a
    /// second put sharing a newly-created ancestor classified it
    /// preexisting, fsynced only its own leaf chain, and returned
    /// `Created` (manifest row committed) while the shared chain's
    /// durability rested on a put with no obligation to complete — a
    /// host crash in the window lost a chunk whose manifest row
    /// exists. Orchestrated here as the share-race with the injected
    /// creator abort: put A created the shared ancestors and died
    /// before any fsync (simulated by raw `create_dir_all` — exactly
    /// the on-disk state A leaves); put B must not trust A.
    // r[verify store.log.chunk-dirent-durable+2]
    #[tokio::test]
    async fn shared_prefix_put_fsyncs_its_full_chain_itself() {
        use super::{LogChunkStore as _, fsync_recorder};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("chunks");
        let store = super::FilesystemLogChunkStore::new(&root).unwrap();

        // Put A's partial progress: the shared ancestors exist, none
        // of their dirents is durable (A died before its fsync loop).
        std::fs::create_dir_all(root.join("logs/ab/cd/sessA")).unwrap();
        let baseline = fsync_recorder::under(&root).len();

        // Put B shares logs/ab/cd and adds its own leaf.
        let outcome = store
            .put("logs/ab/cd/sessB/chunk-000001", b"line".to_vec())
            .await
            .unwrap();
        assert!(matches!(outcome, super::PutOutcome::Created));

        let after = fsync_recorder::under(&root)[baseline..].to_vec();
        let chain = [
            root.join("logs/ab/cd/sessB"),
            root.join("logs/ab/cd"),
            root.join("logs/ab"),
            root.join("logs"),
            root.clone(),
        ];
        assert_eq!(
            after,
            chain.to_vec(),
            "left (pre-fix): B's exists() probe classified A's freshly \
             created ancestors preexisting and fsynced only its own leaf \
             chain — B returned Created (its manifest row commits) while \
             the shared chain's durability rested on a put that already \
             died; a host crash loses a chunk whose manifest row exists / \
             right: every put fsyncs its FULL ancestor chain child→root \
             unconditionally — Created entails own-chain durability, no \
             cross-task obligation transfer exists"
        );

        // The Existed face: a put that finds the object already present
        // still fsyncs the chain — the creator may have died between
        // its body sync and its dir fsyncs.
        let baseline = fsync_recorder::under(&root).len();
        let outcome = store
            .put("logs/ab/cd/sessB/chunk-000001", b"line".to_vec())
            .await
            .unwrap();
        assert!(matches!(outcome, super::PutOutcome::Existed));
        assert_eq!(
            fsync_recorder::under(&root)[baseline..].to_vec(),
            chain.to_vec(),
            "Existed is a durability statement too: the chain fsync is \
             unconditional per put, never per outcome"
        );
    }

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

    // r[verify store.log.write-read-bound+2]
    /// The absolute line-count backstop's boundary: a payload of
    /// exactly `MAX_CHUNK_LINES_ABS` empty lines is byte-bound-maximal
    /// (every line is one bare 4-byte frame, summing to exactly the
    /// decompression ceiling) and MUST decode — the backstop refuses
    /// only counts the byte bound already makes unframeable, so no
    /// grandfathered chunk is ever falsely refused.
    #[test]
    fn exactly_abs_line_count_decodes() {
        let n = rio_log_kernel::MAX_CHUNK_LINES_ABS as usize;
        // n zero-length frames = n * 4 bytes of payload, zstd of a
        // 16 MiB zero run is tiny and fast.
        let payload = vec![0u8; n * LINE_LEN_PREFIX_BYTES];
        let blob = zstd::stream::encode_all(&payload[..], 0).unwrap();
        let lines = decompress_lines(&blob).unwrap();
        assert_eq!(lines.len(), n);
        assert!(lines.iter().all(|l| l.is_empty()));
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

    /// Line content is arbitrary worker-supplied bytes (the builder
    /// forwards whole `STDERR_NEXT` payloads as one line without
    /// splitting; a hostile builder can send anything), so the codec
    /// must be injective over content that contains the byte values a
    /// delimiter-based framing would treat as structure: an embedded
    /// `\n` (the byte that split a line in two under the old framing —
    /// the `servedSpanExact` violation `seed-crash-embedded-newline`
    /// reproduces), a `\r\n` pair, a NUL, and the zero-length line. N
    /// lines in, the same N lines out, byte for byte.
    #[test]
    fn codec_roundtrips_delimiter_and_control_bytes() {
        let lines: Vec<Vec<u8>> = vec![
            b"two logical lines\nin one stored line".to_vec(),
            b"crlf terminated\r\n".to_vec(),
            b"embedded\0nul".to_vec(),
            b"".to_vec(),
            b"\n".to_vec(),
            vec![0xA0, 0x0A, 0x00, 0x00, 0x00, 0x00], // the minimized fuzz reproducer's line 10
        ];
        let blob = compress_lines(&lines).unwrap();
        let roundtripped = decompress_lines(&blob).unwrap();
        assert_eq!(
            roundtripped.len(),
            lines.len(),
            "a line containing a delimiter byte must not split into two"
        );
        assert_eq!(roundtripped, lines);
    }

    /// The length-prefix framing gives the decoder real failure modes a
    /// delimiter split never had: a payload that ends mid-prefix and a
    /// prefix whose declared length runs past the payload are both
    /// corruption, not lines. (Built by compressing raw payload bytes
    /// directly — `compress_lines` cannot produce these.)
    #[test]
    fn codec_rejects_malformed_framing() {
        let compress_raw = |payload: &[u8]| {
            zstd::stream::encode_all(payload, LOG_CHUNK_ZSTD_LEVEL).expect("in-memory zstd encode")
        };
        // Two trailing bytes where a 4-byte length prefix should start.
        let truncated_prefix = compress_raw(&[1, 0, 0, 0, b'x', 9, 9]);
        assert!(matches!(
            decompress_lines(&truncated_prefix),
            Err(LogChunkError::Codec(_))
        ));
        // A prefix declaring 200 content bytes with only 1 present.
        let overlong_length = compress_raw(&[200, 0, 0, 0, b'x']);
        assert!(matches!(
            decompress_lines(&overlong_length),
            Err(LogChunkError::Codec(_))
        ));
    }

    /// A frame whose decompressed size exceeds [`MAX_DECOMPRESSED_CHUNK_BYTES`]
    /// is rejected instead of ballooning into an unbounded allocation. A
    /// 17 MiB payload of zeros compresses to a few KiB — exactly the
    /// shape of a corrupt-or-malicious "small object, huge content" frame.
    #[test]
    fn codec_rejects_oversized_decompressed_payload() {
        // Built with RAW zstd, not compress_lines: the writer now
        // refuses over-bound payloads (store.log.write-read-bound), so
        // an oversized frame can only arrive from outside the write
        // path — a corrupt or attacker-crafted object — which is
        // exactly what the read-side defense exists for.
        let huge_line = vec![0u8; (MAX_DECOMPRESSED_CHUNK_BYTES + 1024) as usize];
        let mut payload = Vec::with_capacity(huge_line.len() + super::LINE_LEN_PREFIX_BYTES);
        payload.extend_from_slice(&(huge_line.len() as u32).to_le_bytes());
        payload.extend_from_slice(&huge_line);
        let blob = zstd::stream::encode_all(payload.as_slice(), 6).unwrap();
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

    // r[verify store.log.write-read-bound+2]
    /// The write-side half of the shared bound: the codec refuses to
    /// FRAME a payload the read path would refuse to decode.
    #[test]
    fn compress_refuses_over_bound_payload() {
        let lines: Vec<Vec<u8>> = (0..280).map(|_| vec![b'x'; 64 * 1024 - 64]).collect(); // ~17.9 MiB framed
        match compress_lines(&lines) {
            Err(LogChunkError::Codec(e)) => {
                assert!(e.to_string().contains("shared write/read bound"), "{e}");
            }
            other => panic!("expected Codec refusal, got {other:?}"),
        }
        // Polarity: just under the bound still compresses.
        let ok: Vec<Vec<u8>> = (0..10).map(|_| vec![b'x'; 1024]).collect();
        compress_lines(&ok).expect("an in-bound payload compresses");
    }

    /// The trailing-empty-line case: `["a", ""]` and `["a"]` must stay
    /// distinguishable through the round trip (the manifest row claims
    /// two lines; serving one would desynchronize the positional
    /// attribution). Under the old join/split framing this took a
    /// terminate-every-line convention to get right; under length
    /// prefixes the empty line is its own zero-length record and the
    /// case is pinned here so it stays covered.
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
        // One empty line -> a lone zero-length prefix -> one empty line.
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
