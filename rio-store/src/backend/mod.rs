//! Chunk storage backends.
//!
//! Stores BLAKE3-addressed chunks plus string-keyed blobs (the
//! stock-Nix narinfo/NAR sidecars). Four impls: S3 (prod), tiered
//! ([`TieredChunkBackend`] — Express read-through cache over S3),
//! filesystem (dev), memory (tests). Chunks are immutable and
//! content-addressed, so there's no update or rename. Delete goes
//! through the `pending_s3_deletes` outbox: GC sweep enqueues keys
//! in-transaction, the drain task calls `delete_by_key`.
//!
//! Chunk keys are `[u8; 32]` (BLAKE3), not strings — no path-traversal
//! concern. Blob keys are strings and pass `validate_blob_key`.
//! `get`/`get_blob` return `Bytes` (owned), not `AsyncRead`: chunks are
//! ≤256 KiB and blobs are narinfo-sized; buffering simplifies callers.
//! `exists_batch` (not `exists`) because PutPath checks hundreds of
//! chunks per call.
//!
//! S3 key scheme: `chunks/{aa}/{blake3-hex}` for chunks (the two-char
//! prefix spreads load across S3 shards per `store.typ`),
//! `{prefix}/{key}` for blobs.

mod tiered;

pub use tiered::TieredChunkBackend;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use bytes::Bytes;
use tokio::sync::Semaphore;
use tracing::debug;

/// Marker error: backend rejected the request due to auth/config, not a
/// transient fault. When this is in an anyhow chain, the grpc layer maps
/// to `FailedPrecondition` (non-retriable) instead of `Internal` — so a
/// client seeing STS AccessDenied fails fast instead of retrying forever.
///
/// Unit struct (no fields): the detailed message lives in the anyhow
/// `.context(...)` layer above this marker. The marker's only job is to
/// be `downcast_ref`-able from the grpc layer.
#[derive(Debug, thiserror::Error)]
#[error("storage backend authentication/configuration error")]
pub struct BackendAuthError;

/// Classify an AWS SDK error as permanent auth/config failure vs transient.
///
/// Covers two shapes:
/// 1. **Service-level auth** — request reached S3, S3 replied with an
///    auth error code. `ProvideErrorMetadata::code()` exposes these.
/// 2. **Credential-provider auth** — request never left: STS
///    AssumeRoleWithWebIdentity denied, IMDS unreachable, expired
///    token, etc. Surfaces as `DispatchFailure` with `.code() == None`
///    and the denial buried in the *source chain* — `SdkError`'s own
///    `Display` is a fixed variant label (`"dispatch failure"`),
///    NOT a flattened chain, so we walk `Error::source()` ourselves.
///
/// False negative (auth error classified as transient) → client retries
/// a few times then gives up; annoying but safe. False positive
/// (transient classified as auth) → client gives up on a recoverable
/// error; bad. The code-list is conservative (well-known AWS auth
/// codes only) and the string match is narrowed to source-chain Display
/// specifically to bias toward false negatives.
fn is_permanent_auth_error<E>(e: &E) -> bool
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
{
    // Service-level: explicit S3/IAM error codes.
    if let Some(code) = e.code()
        && matches!(
            code,
            "AccessDenied"
                | "InvalidAccessKeyId"
                | "SignatureDoesNotMatch"
                | "TokenRefreshRequired"
                | "ExpiredToken"
                | "InvalidToken"
        )
    {
        return true;
    }
    // Credential-provider level: STS denial or credential-chain
    // exhaustion buried in DispatchFailure. Walk Error::source()
    // (SdkError::Display alone is a fixed variant label). "AccessDenied"
    // catches the STS response body; "credentials" (lowercase) catches
    // the sdk's own "failed to load credentials" / "an error occurred
    // while loading credentials" wrappers.
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        let s = err.to_string();
        if s.contains("AccessDenied") || s.contains("credentials") {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Chokepoint: convert an aws-sdk error to anyhow, rooting at
/// [`BackendAuthError`] when [`is_permanent_auth_error`] matches so
/// `grpc::storage_error` / `gc::drain` can downcast it. Generic over
/// `SdkError<Op>` and the `Op::Error` service-error types alike (both
/// satisfy the bound), so call this from before *or* after
/// `.into_service_error()`.
///
/// `pub(crate)`: also the error chokepoint for `logs::chunks`'
/// `S3LogChunkStore`, so log-chunk S3 failures get the same
/// auth-vs-transient classification (and the same `BackendAuthError`
/// downcast in the gRPC layer) as NAR-chunk failures.
pub(crate) fn classify_s3_error<E>(e: E, msg: String) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    if is_permanent_auth_error(&e) {
        // Render e's full source chain into the context string:
        // SdkError's own Display is a fixed variant label ("dispatch
        // failure"); the STS/IAM detail is in .source(). The walk
        // mirrors `is_permanent_auth_error` above. BackendAuthError
        // must stay the chain ROOT (grpc::storage_error / gc::drain
        // downcast it as the innermost source), so `e` cannot be
        // inserted into the chain — it's rendered into the context.
        let mut detail = e.to_string();
        let mut cur = e.source();
        while let Some(s) = cur {
            use std::fmt::Write as _;
            let _ = write!(detail, ": {s}");
            cur = s.source();
        }
        anyhow::Error::new(BackendAuthError).context(format!("{msg}: {detail}"))
    } else {
        anyhow::Error::new(e).context(msg)
    }
}

/// One key that a [`ChunkBackend::delete_by_keys`] call failed to
/// delete, with the backend's error text. The key stays in
/// `pending_s3_deletes` (attempts incremented) and is retried on a
/// later drain tick — per-key failures must never be silently dropped.
#[derive(Debug)]
pub struct BatchDeleteFailure {
    pub key: String,
    pub error: String,
}

/// Trait for chunk storage backends.
#[async_trait::async_trait]
pub trait ChunkBackend: Send + Sync {
    /// Store a chunk. Idempotent: PUTting the same hash twice with the
    /// same content is a no-op (the content IS the hash, so "same hash
    /// different content" would be a BLAKE3 collision — not our problem).
    ///
    /// Caller guarantees `blake3::hash(data) == hash`. Backends don't
    /// re-verify on write (that's the caller's job — `chunker::chunk_nar`
    /// computed the hash from the data, they're tautologically consistent).
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()>;

    /// Fetch a chunk. `None` if not present — but "not present" is a
    /// DATA LOSS condition in practice: if the manifest says this hash
    /// exists, the chunk should be here. `None` means S3 lost it (or the
    /// manifest is corrupt). Caller propagates as an error, not a retry.
    ///
    /// Backends do NOT verify BLAKE3 on read — that's the caller's job
    /// (see `ChunkCache::get_verified`). Layered this way so the cache
    /// verifies exactly once regardless of whether the bytes came from
    /// the backend or the in-process LRU.
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>>;

    /// Batch existence check. Returns a `Vec<bool>` parallel to `hashes`:
    /// `result[i]` is `true` if `hashes[i]` is present.
    ///
    /// PutPath calls this BEFORE uploading to skip chunks that already
    /// exist (the dedup fast-path). For the memory backend it's a HashMap
    /// lookup loop; for S3 it's a batch of HeadObject calls. (PutPath uses
    /// the PG `chunks` table instead for dedup — one RTT vs N HeadObject calls.)
    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>>;

    /// Compute the storage key for a hash without doing I/O. Used by
    /// the chunk collect cycle to enqueue the key to
    /// `pending_s3_deletes` in the SAME PG transaction as the
    /// soft-delete (two-phase commit for S3 cleanup — enqueue
    /// atomically, delete later).
    ///
    /// Returns the backend-specific key: for S3 it's `prefix/aa/hex`
    /// (bucket-relative); for filesystem it's the relative path. The
    /// drain task passes this back to `delete_by_key`.
    fn key_for(&self, hash: &[u8; 32]) -> String;

    /// Delete by storage key (as returned by `key_for`). Used by the GC
    /// drain task, which stores string keys (not hashes) in
    /// `pending_s3_deletes` so it never needs to re-parse them back to
    /// `[u8; 32]`.
    ///
    /// `Err` on I/O failure (S3 down, permission denied). "Already gone"
    /// is `Ok` — idempotent, the drain might retry a partially-processed
    /// batch. The drain task increments `attempts` on Err and retries
    /// with backoff; after max attempts it stops (alert-worthy but not
    /// a process crash — S3 objects leak, PG state is correct).
    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()>;

    /// Delete many keys, returning the per-key failures. Same
    /// idempotence contract as [`delete_by_key`](Self::delete_by_key).
    ///
    /// `Ok(failures)` means the call ran to completion; every key NOT
    /// in `failures` was deleted (or was already gone). `Err` means
    /// the call as a whole failed (auth/transport) and nothing can be
    /// said per key — the caller treats all keys as not deleted.
    /// [`BackendAuthError`] in the chain keeps its drain semantics
    /// (stop the tick, don't burn retry attempts).
    ///
    /// The default implementation loops
    /// [`delete_by_key`](Self::delete_by_key) — correct for the
    /// filesystem/memory backends where a "batch" has no wire-level
    /// advantage. S3
    /// overrides this with `DeleteObjects` (1000 keys per round
    /// trip): the GC drain previously issued one `DeleteObject` per
    /// key, capping throughput at ~3.3 deletes/s per replica and
    /// letting the tombstone backlog grow without bound past ~12k
    /// enqueues/h.
    async fn delete_by_keys(&self, keys: &[String]) -> anyhow::Result<Vec<BatchDeleteFailure>> {
        let mut failures = Vec::new();
        for key in keys {
            if let Err(e) = self.delete_by_key(key).await {
                // Auth is call-level, not key-level: the same denial
                // will hit every remaining key, so surface it as a
                // whole-call failure the drain can break on.
                if e.downcast_ref::<BackendAuthError>().is_some() {
                    return Err(e);
                }
                failures.push(BatchDeleteFailure {
                    key: key.clone(),
                    error: format!("{e:#}"),
                });
            }
        }
        Ok(failures)
    }

    /// Store a string-keyed blob next to the chunk namespace.
    ///
    /// For the stock-Nix binary-cache sidecars (`{hash}.narinfo`,
    /// `nar/{hash}.nar.zst`, `nix-cache-info`), which the
    /// `[u8; 32]`-addressed API can't express. rio-store is the writer;
    /// stock Nix reads them straight from the bucket. Last-writer-wins.
    /// Backends reject `..` / absolute paths / the `chunks/` prefix and
    /// otherwise leave the key shape to the caller.
    async fn put_blob(&self, key: &str, data: Bytes) -> anyhow::Result<()>;

    /// Fetch a string-keyed blob. `None` if absent. Used by the compat
    /// reconciler and the once-only `nix-cache-info` write — not the
    /// hot read path.
    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<Bytes>>;

    /// Delete a string-keyed blob. Idempotent. Compat GC deletes the
    /// narinfo/NAR sidecars when the underlying path is GC'd.
    async fn delete_blob(&self, key: &str) -> anyhow::Result<()>;
}

/// Encode a chunk hash into its storage key (`{aa}/{hex}`).
///
/// Shared helper so S3 and filesystem use the same scheme. The two-char
/// prefix subdir avoids: (a) S3 shard hotspotting, (b) ext4 having 100k
/// files in one directory (slow readdir, though modern dir_index helps).
fn chunk_key(hash: &[u8; 32]) -> String {
    let hex = hex::encode(hash);
    // First two hex chars = first byte of hash. 256 buckets.
    // Indexing is safe: hex::encode of [u8;32] always produces 64 chars.
    format!("{}/{}", &hex[..2], hex)
}

/// Validate a chunk key before Path::join or S3 DeleteObject.
///
/// Keys come from pending_s3_deletes which we wrote via key_for, but
/// defense-in-depth: if a corrupted row or injected key got into the
/// table, Path::join would resolve `..` etc. Reject non-conforming
/// keys; drain increments attempts → stuck → operator alert.
/// Shape: `{2 hex chars}/{64 hex chars}`.
fn validate_chunk_key(key: &str) -> anyhow::Result<()> {
    let Some((prefix, hex)) = key.split_once('/') else {
        anyhow::bail!("invalid chunk key {key:?}: missing '/'");
    };
    if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("invalid chunk key {key:?}: prefix not 2 hex chars");
    }
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("invalid chunk key {key:?}: body not 64 hex chars");
    }
    // Also verify prefix matches first 2 chars of body (chunk_key's
    // own convention). Not strictly necessary for safety but catches
    // malformed rows earlier.
    if prefix != &hex[..2] {
        anyhow::bail!("invalid chunk key {key:?}: prefix doesn't match body");
    }
    Ok(())
}

/// Validate a blob key before `Path::join` / S3 PutObject. Reject
/// anything that escapes `blobs_dir` or collides with the chunk
/// namespace; otherwise the writer decides the layout.
fn validate_blob_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("invalid blob key: empty");
    }
    if key.starts_with('/') {
        anyhow::bail!("invalid blob key {key:?}: absolute path");
    }
    if key
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        anyhow::bail!("invalid blob key {key:?}: dot/empty path component");
    }
    if key == "chunks" || key.starts_with("chunks/") {
        anyhow::bail!("invalid blob key {key:?}: collides with chunk namespace");
    }
    Ok(())
}

// ============================================================================
// Memory backend (tests)
// ============================================================================

/// In-memory chunk storage. Test-only.
///
/// Uses `[u8; 32]` as the HashMap key directly — no hex-encoding needed.
/// Recovers from `RwLock` poison (warns + uses into_inner) so a panicking
/// test doesn't cascade into all other tests sharing the backend.
#[derive(Default)]
pub struct MemoryChunkBackend {
    inner: RwLock<HashMap<[u8; 32], Bytes>>,
    blobs: RwLock<HashMap<String, Bytes>>,
}

impl MemoryChunkBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: get a count of stored chunks. For dedup-ratio
    /// assertions in chunked PutPath tests.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test helper: corrupt a stored chunk's bytes. For BLAKE3-verify
    /// tests — overwrite with garbage, then assert `get_verified` returns Err.
    pub fn corrupt_for_test(&self, hash: &[u8; 32], garbage: Bytes) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*hash, garbage);
    }
}

#[async_trait::async_trait]
impl ChunkBackend for MemoryChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*hash, data);
        Ok(())
    }

    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        // Bytes::clone is cheap (Arc refcount bump).
        Ok(self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .cloned())
    }

    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        Ok(hashes.iter().map(|h| inner.contains_key(h)).collect())
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        // Memory backend doesn't have real keys; use hex for the
        // pending_s3_deletes table (drain task just needs A key,
        // delete_by_key parses it back). Consistent with chunk_key
        // but no directory structure.
        hex::encode(hash)
    }

    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        // Parse hex back to [u8; 32]. If it's not 64 hex chars,
        // this was never a valid key_for output — drain is
        // retrying a bad row. Error (attempts++, operator looks).
        let bytes = hex::decode(key).map_err(|e| anyhow::anyhow!("invalid key {key:?}: {e}"))?;
        let hash: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("key {key:?} is not 32 bytes"))?;
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&hash);
        Ok(())
    }

    async fn put_blob(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        self.blobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), data);
        Ok(())
    }

    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        validate_blob_key(key)?;
        Ok(self
            .blobs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned())
    }

    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        self.blobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        Ok(())
    }
}

// ============================================================================
// Filesystem backend (dev)
// ============================================================================

/// Filesystem chunk storage. Dev/single-node.
///
/// Layout: `{root}/chunks/{aa}/{blake3-hex}` for chunks,
/// `{root}/blobs/{key}` for narinfo/NAR sidecars. The two-level chunk
/// dir structure matches the S3 key scheme (so switching backends
/// doesn't surprise operators) and keeps per-directory file counts
/// reasonable.
// r[impl store.backend.filesystem]
pub struct FilesystemChunkBackend {
    /// `{root}/chunks`.
    base_dir: PathBuf,
    /// `{root}/blobs`. Sibling of `base_dir` so chunk and blob paths
    /// can never alias.
    blobs_dir: PathBuf,
}

impl FilesystemChunkBackend {
    /// Create a new filesystem backend. Creates `{root}/chunks/` and
    /// all 256 `{aa}/` subdirectories eagerly — small upfront cost (256
    /// mkdir calls, ~1ms) so `put()` never has to check-then-mkdir on
    /// the hot path — and makes the CREATION durable (merged_bug_033,
    /// the same base-case discipline as the log chunk store): every
    /// created level is fsynced child-to-root plus the deepest
    /// pre-existing ancestor; the `chunks/` fsync covers all 256
    /// subdir dirents at once (they live in its listing), after which
    /// `put()`'s `{aa}/` parent fsync genuinely ends at durable
    /// ground. Pre-fix the whole tree was created with ZERO dir
    /// fsyncs: a crash after a put lost dirents while manifest rows
    /// claimed durable coverage (first-boot window on the dev/
    /// standalone-VM backends; production is S3). Also creates
    /// `{root}/blobs/` shallow (`put_blob` mkdirs nested components on
    /// demand — `nar/` is the only one and it's rare).
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let base_dir = root.join("chunks");
        let blobs_dir = root.join("blobs");
        // Pre-scan BEFORE creating: which levels will be new?
        let mut created = Vec::new();
        let mut cursor = base_dir.clone();
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
        std::fs::create_dir_all(&base_dir)?;
        std::fs::create_dir_all(&blobs_dir)?;
        // Precreate all 256 two-char-hex subdirectories.
        for b in 0u8..=255 {
            std::fs::create_dir_all(base_dir.join(format!("{b:02x}")))?;
        }
        // The base case, through the same recorded chokepoint as the
        // log store (one witnessed fsync path, not two): chunks/'s
        // own contents (the 256 dirents), every created ancestor, and
        // the deepest pre-existing directory.
        crate::logs::chunks::fsync_dir_sync(&base_dir)?;
        for dir in created.iter().filter(|d| **d != base_dir) {
            crate::logs::chunks::fsync_dir_sync(dir)?;
        }
        if !created.is_empty() {
            crate::logs::chunks::fsync_dir_sync(&deepest_preexisting)?;
        }
        Ok(Self {
            base_dir,
            blobs_dir,
        })
    }

    fn chunk_path(&self, hash: &[u8; 32]) -> PathBuf {
        self.base_dir.join(chunk_key(hash))
    }

    fn blob_path(&self, key: &str) -> PathBuf {
        self.blobs_dir.join(key)
    }

    /// Atomic write: temp + fsync + rename + dir-fsync. Without all
    /// four a crash between this returning and the manifest commit
    /// leaves a key claiming a file that's zero-length or absent. Temp
    /// goes in the target's directory (rename atomicity needs same
    /// filesystem) with a random suffix so concurrent writes to the
    /// same key don't race on the temp name.
    async fn atomic_write(path: &std::path::Path, data: &Bytes) -> anyhow::Result<()> {
        let tmp_path = path.with_extension(format!(
            "{:016x}.tmp",
            uuid::Uuid::new_v4().as_u128() as u64
        ));
        let tmp_guard = scopeguard::guard(tmp_path.clone(), |p| {
            let _ = std::fs::remove_file(&p);
        });
        {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::File::create(&tmp_path).await?;
            f.write_all(data).await?;
            f.sync_all().await?;
        }
        tokio::fs::rename(&tmp_path, path).await?;
        scopeguard::ScopeGuard::into_inner(tmp_guard);

        // fsync parent dir. Without this, the rename's directory entry
        // can be lost on power failure even though the file data is
        // durable.
        if let Some(parent) = path.parent() {
            let dir = tokio::fs::File::open(parent).await?;
            dir.sync_all().await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ChunkBackend for FilesystemChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        let path = self.chunk_path(hash);
        debug!(path = %path.display(), size = data.len(), "FilesystemChunkBackend: storing chunk");
        // Constructor precreated the {aa}/ subdir; no mkdir on the hot path.
        Self::atomic_write(&path, &data).await
    }

    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        let path = self.chunk_path(hash);
        match tokio::fs::read(&path).await {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        // Sequential — filesystem `try_exists` is already fast (single
        // stat syscall). Parallelizing would add tokio task overhead
        // for no real gain on a local disk.
        let mut result = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let path = self.chunk_path(hash);
            // Propagate I/O errors (permission denied, disk failure).
            // Silently treating them as "not present" triggers re-uploads
            // of chunks that ARE present — load amplification, masks the
            // real problem.
            result.push(tokio::fs::try_exists(&path).await?);
        }
        Ok(result)
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        // Relative path (no base_dir) so pending_s3_deletes entries
        // survive a base_dir relocation. delete_by_key rejoins.
        chunk_key(hash)
    }

    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        // Validate key format before Path::join. Defense-in-depth
        // against corrupted/injected rows in pending_s3_deletes.
        validate_chunk_key(key)?;

        // key is the relative path from key_for. Rejoin to base_dir.
        let path = self.base_dir.join(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_blob(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        let path = self.blob_path(key);
        debug!(path = %path.display(), size = data.len(), "FilesystemChunkBackend: storing blob");
        // Blob keys can be nested (`nar/`); not precreated like chunk subdirs.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Self::atomic_write(&path, &data).await
    }

    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        validate_blob_key(key)?;
        match tokio::fs::read(self.blob_path(key)).await {
            Ok(data) => Ok(Some(Bytes::from(data))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        match tokio::fs::remove_file(self.blob_path(key)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

// ============================================================================
// S3 backend (prod)
// ============================================================================

/// S3 chunk storage.
///
/// Key scheme: `{prefix}/chunks/{aa}/{blake3-hex}` where `{aa}` is the
/// first two hex chars of the hash (prefix-partitioned for S3 shard
/// distribution).
pub struct S3ChunkBackend {
    client: Client,
    bucket: String,
    prefix: String,
    /// Replica-global gate on concurrent PutObject. The aws-sdk's
    /// default hyper client has NO in-flight connection cap (only
    /// `pool_max_idle_per_host`, which bounds *idle* returned-to-pool
    /// conns) — a request with no idle conn opens a new one and never
    /// queues. This semaphore IS the de-facto fd/connection bound for
    /// the chunk-PUT plane: at the default
    /// [`DEFAULT_CHUNK_UPLOAD_GLOBAL_PERMITS`](crate::cas::DEFAULT_CHUNK_UPLOAD_GLOBAL_PERMITS)
    /// (256), 256 concurrent PutObject → up to 256 TCP conns to the S3
    /// endpoint, regardless of how many `put_chunked` callers fan out
    /// at once. The per-ingest `buffer_unordered` width
    /// ([`DEFAULT_CHUNK_UPLOAD_CONCURRENCY`](crate::cas::DEFAULT_CHUNK_UPLOAD_CONCURRENCY))
    /// composes UNDER this — fairness (one giant NAR can't take all
    /// 256) plus the per-ingest `Bytes`-overshoot bound — so in-flight
    /// PutObject = `min(Σ per-ingest, global)`.
    put_gate: Arc<Semaphore>,
}

impl S3ChunkBackend {
    pub fn new(client: Client, bucket: String, prefix: String, global_put_permits: usize) -> Self {
        // Normalize: strip trailing slashes. `s3_key()` below joins with
        // a literal "/chunks/", so a prefix of "foo/" would produce
        // "foo//chunks/ab/..." — which S3 treats as a DISTINCT key from
        // "foo/chunks/ab/..." (no path normalization). I-040: an early
        // Helm default of `prefix = "chunks/"` wrote every object to the
        // double-slash key; self-consistent on read, but invisible to
        // tooling that expected the documented scheme. Stripping leading
        // slashes too would be surprising (a prefix of "/foo" is unusual
        // but intentional); trailing is almost always a config mistake.
        let normalized = prefix.trim_end_matches('/').to_string();
        Self {
            client,
            bucket,
            prefix: normalized,
            put_gate: Arc::new(Semaphore::new(global_put_permits)),
        }
    }

    fn s3_key(&self, hash: &[u8; 32]) -> String {
        let key = chunk_key(hash);
        if self.prefix.is_empty() {
            format!("chunks/{key}")
        } else {
            format!("{}/chunks/{key}", self.prefix)
        }
    }

    /// Bucket-relative key for a string-keyed blob: `{prefix}/{key}`.
    /// No `chunks/` segment — narinfo/NAR live at the prefix root,
    /// where stock Nix expects `{narinfo}` and `nar/{nar}` to resolve.
    fn blob_s3_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    async fn s3_put(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        // opt-02 gate lives HERE — replica-global before the
        // requests-total counter (it measures *dispatched* PUTs, not
        // parked) and before `chunk_op_override()` so the 5 s
        // `CHUNK_OP_ATTEMPT_TIMEOUT` clocks the S3 op only, never the
        // queue. `acquire_owned` so the permit's lifetime is tied to
        // this future (cancel-drop releases). The semaphore is
        // constructed in `new()` and never closed, so `expect` is
        // unreachable. Gating the helper (not the trait `put()`) means
        // every S3 PutObject path — chunk-put AND blob-put — is
        // covered; `TieredChunkBackend::put()` delegates here too.
        let _permit = self
            .put_gate
            .clone()
            .acquire_owned()
            .await
            .expect("put_gate never closed");
        debug!(bucket = %self.bucket, key = %key, size = data.len(), "S3ChunkBackend: uploading");
        metrics::counter!("rio_store_s3_requests_total", "operation" => "put_object").increment(1);
        // D5: per-attempt deadline at the seam (s3-op-census row:
        // put_object, body ≤ CHUNK_MAX, pre-buffered).
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.into())
            .customize()
            .config_override(chunk_op_override())
            .send()
            .await
            .map_err(|e| {
                classify_s3_error(
                    e,
                    format!("S3 PutObject failed for s3://{}/{key}", self.bucket),
                )
            })?;
        Ok(())
    }

    async fn s3_get(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        metrics::counter!("rio_store_s3_requests_total", "operation" => "get_object").increment(1);
        // D5: per-attempt deadline at the seam (s3-op-census row:
        // get_object, response body ≤ CHUNK_MAX).
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .customize()
            .config_override(chunk_op_override())
            .send()
            .await
        {
            // Buffer into Bytes — chunks are ≤256 KiB and blobs are
            // narinfo-sized; ByteStream::collect is zero-copy for a
            // single segment. The attempt timeout above covers only up
            // to response HEADERS — the body stream needs its own
            // clock (CHUNK_GET_BODY_TIMEOUT) or a black-holed body
            // read pins the caller forever.
            Ok(out) => {
                let collected = tokio::time::timeout(CHUNK_GET_BODY_TIMEOUT, out.body.collect())
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "S3 body read for s3://{}/{key} exceeded its typed bound \
                             ({CHUNK_GET_BODY_TIMEOUT:?}) — peer presumed black-holed",
                            self.bucket
                        )
                    })?;
                Ok(Some(
                    collected
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "S3 body read failed for s3://{}/{key}: {e}",
                                self.bucket
                            )
                        })?
                        .into_bytes(),
                ))
            }
            Err(err) => {
                let svc = err.into_service_error();
                if svc.is_no_such_key() {
                    Ok(None)
                } else {
                    // classify_s3_error roots auth errors at BackendAuthError
                    // so the gRPC layer maps them to FailedPrecondition.
                    Err(classify_s3_error(
                        svc,
                        format!("S3 GetObject failed for s3://{}/{key}", self.bucket),
                    ))
                }
            }
        }
    }

    /// DeleteObject is idempotent — a non-existent key returns success.
    /// D5: per-attempt deadline at the seam (s3-op-census row:
    /// delete_object, no body).
    async fn s3_delete(&self, key: &str) -> anyhow::Result<()> {
        metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_object")
            .increment(1);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .customize()
            .config_override(chunk_op_override())
            .send()
            .await
            .map_err(|e| {
                classify_s3_error(
                    e,
                    format!("S3 DeleteObject failed for s3://{}/{key}", self.bucket),
                )
            })?;
        Ok(())
    }
}

/// D5 (Q-108 closed at the seam): per-attempt timeout for the
/// chunk-plane S3 ops — PutObject / GetObject (headers) /
/// DeleteObject. The shared client deliberately ships NO
/// `TimeoutConfig` (rio_common::s3); pre-fix, an
/// established-then-black-holed connection on any of these ops
/// awaited response headers forever and the never-completing FIRST
/// attempt defeated the retry layer — only the NAR-hold envelope
/// (minutes-class, the budget law's backstop) ever cut it. This
/// override is applied per-operation (`config_override`, the HEAD
/// lane's worked-example shape) and is TIMEOUT-ONLY: `max_attempts`
/// stays the operator's `s3_max_attempts` knob (raised for
/// rustfs/MinIO churn — the envelope must not cancel the recoveries
/// the knob exists for), backoffs stay the SDK standard caps (1 s
/// initial, 20 s max — bounded).
///
/// Derivation (the op census, committed at
/// `rio-store/tests/gensets/s3-op-census.txt`): every body in this
/// class is pre-buffered and ≤ `CHUNK_MAX` (256 KiB) — p99 healthy
/// round-trip is tens of ms; 5 s is ~20× that with throttle-burst
/// headroom. Ordering law (R17, pinned by
/// `chunk_op_envelope_exhausts_inside_the_hold_grace`): the whole
/// retry ladder at the default knob (10 attempts) worst-cases at
/// ~3.6 min — strictly inside the smallest NAR-hold grace (15 min),
/// so the seam exhausts FIRST and the hold envelope stays the
/// backstop, never the first line.
pub(crate) const CHUNK_OP_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// D5: bound on the GetObject BODY collect — the attempt timeout
/// covers only up to response headers (the body is a separate
/// stream), so a black-holed body read needs its own clock.
/// Derivation: `CHUNK_MAX` (256 KiB) at an 8 KiB/s pathological floor
/// = 32 s; 30 s is the same order with the whole-op single-shot shape
/// (no retry below it — a failed collect surfaces to the caller's
/// retry machinery).
pub(crate) const CHUNK_GET_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// D5: per-attempt timeout for the LOG-plane S3 ops (zstd log-chunk
/// PutObject / GetObject / DeleteObject — `logs/chunks.rs`). Bigger
/// bodies than the chunk plane: the cut threshold is 8 MiB
/// uncompressed (~2 MiB at the typical 4:1 zstd ratio, worst case
/// approaching the threshold), so the bound is 15 s — ≥ the 8 MiB
/// worst body at 1 MiB/s with headroom. Same timeout-only discipline
/// as [`CHUNK_OP_ATTEMPT_TIMEOUT`].
pub(crate) const LOG_OP_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// D5: bound on the log-chunk GetObject BODY collect (see
/// [`CHUNK_GET_BODY_TIMEOUT`] for why the attempt timeout cannot
/// cover it): ≤ ~8 MiB compressed worst case at an 8 KiB/s
/// pathological floor would be 17 min — that is the black-hole shape,
/// not a lawful read; 60 s covers the worst lawful body at 136 KiB/s.
pub(crate) const LOG_GET_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The timeout-only per-op override for the chunk-plane ops. Retry
/// shape (attempts/backoff) deliberately NOT set — see
/// [`CHUNK_OP_ATTEMPT_TIMEOUT`].
fn chunk_op_override() -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::config::Config::builder().timeout_config(
        aws_sdk_s3::config::timeout::TimeoutConfig::builder()
            .operation_attempt_timeout(CHUNK_OP_ATTEMPT_TIMEOUT)
            .build(),
    )
}

/// The timeout-only per-op override for the log-plane ops
/// ([`LOG_OP_ATTEMPT_TIMEOUT`]). Exposed crate-wide for
/// `logs/chunks.rs` — one derivation site per op class.
pub(crate) fn log_op_override() -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::config::Config::builder().timeout_config(
        aws_sdk_s3::config::timeout::TimeoutConfig::builder()
            .operation_attempt_timeout(LOG_OP_ATTEMPT_TIMEOUT)
            .build(),
    )
}

#[async_trait::async_trait]
impl ChunkBackend for S3ChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        self.s3_put(&self.s3_key(hash), data).await
    }

    // TODO: gate `get()` under a replica-global semaphore too —
    // `chunk_prefetch_k` (default 64) per concurrent GetPath stream is
    // the read-side analogue of the per-ingest PUT fan-out, with no
    // global cap. Out of scope for opt-02 (PUT plane only).
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        self.s3_get(&self.s3_key(hash)).await
    }

    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        // Parallel HeadObject calls, bounded to 16 concurrent. Unbounded
        // would work but be antisocial (100 chunks = 100 simultaneous
        // requests; S3 can handle it but the caller's network might not).
        //
        // PutPath does NOT use this (it checks PG `chunks.uploaded_at`
        // via the upsert RETURNING in `upgrade_manifest_to_chunked` for
        // one RTT instead of N HeadObjects). The live caller is
        // StoreAdminService.VerifyChunks (admin.rs).
        //
        // Chunked into batches of 16, each batch awaited concurrently via
        // futures_util::future::join_all (which preserves creation order).
        // Output order preserved: chunks_of_16[i] maps to hashes[i*16..].
        //
        // The wave width is the SHARED bilateral constant
        // (merged_bug_023): rio_common::liveness derives the
        // worst-case VerifyChunks emission gap from it, and the
        // conformance test there binds that gap to the CLI's
        // inactivity bound. Widening or narrowing the fan-out here
        // re-derives the contract automatically.
        const CONCURRENCY: usize = rio_common::liveness::ADMIN_VERIFY_HEAD_CONCURRENCY;
        // bug_108: EVERY wave runs under the typed liveness budget —
        // the same ADMIN_VERIFY_WORST_WAVE const the emission-gap
        // arithmetic prices, now enforced at the await. A black-holed
        // connection (established, then silent; the shared client has
        // no TimeoutConfig, see rio_common::s3::default_client) elapses
        // into the typed WaveBudgetExceeded, which flows through the
        // admin classification into Status::unavailable, instead of
        // hanging the audit past the CLI's 120 s inactivity bound.
        let budget = rio_common::liveness::admin_verify_wave_budget();
        // merged_bug_006: the HEAD lane owns its own typed retry
        // envelope, SIZED INSIDE the wave budget (the const-asserted
        // ordering law lives beside the const in rio_common::liveness:
        // envelope.worst_case() <= ADMIN_VERIFY_WORST_WAVE <= emission
        // gap <= the CLI's 120 s bound). Per-attempt timeouts sit
        // BELOW the SDK retry layer; the wave budget stays ABOVE it as
        // the backstop — lawful churn-recovery ladders now COMPLETE
        // inside the budget instead of being cancelled by it. Every
        // wire knob derives from the ONE envelope value, so the bound
        // and the retry shape cannot drift apart. The client-wide
        // s3_max_attempts knob keeps governing the ops it was raised
        // for (puts/gets under rustfs churn); this chokepoint is
        // deliberately decoupled via per-operation config override.
        const HEAD_ENVELOPE: rio_common::liveness::RetryEnvelope =
            rio_common::liveness::ADMIN_VERIFY_HEAD_ENVELOPE;
        let head_config = aws_sdk_s3::config::Config::builder()
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard()
                    .with_max_attempts(HEAD_ENVELOPE.attempts)
                    .with_initial_backoff(HEAD_ENVELOPE.initial_backoff)
                    .with_max_backoff(HEAD_ENVELOPE.max_backoff),
            )
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_attempt_timeout(HEAD_ENVELOPE.attempt_timeout)
                    .build(),
            );
        let mut results = Vec::with_capacity(hashes.len());

        for batch in hashes.chunks(CONCURRENCY) {
            let futs: Vec<_> = batch
                .iter()
                .map(|hash| {
                    let key = self.s3_key(hash);
                    let client = self.client.clone();
                    let bucket = self.bucket.clone();
                    let head_config = head_config.clone();
                    async move {
                        metrics::counter!(
                            "rio_store_s3_requests_total", "operation" => "head_object"
                        )
                        .increment(1);
                        match client
                            .head_object()
                            .bucket(&bucket)
                            .key(&key)
                            .customize()
                            .config_override(head_config)
                            .send()
                            .await
                        {
                            Ok(_) => Ok(true),
                            Err(err) => {
                                let service_err = err.into_service_error();
                                if service_err.is_not_found() {
                                    Ok(false)
                                } else {
                                    Err(classify_s3_error(
                                        service_err,
                                        format!("S3 HeadObject failed for s3://{bucket}/{key}"),
                                    ))
                                }
                            }
                        }
                    }
                })
                .collect();

            // join_all preserves order: output[i] is the result of futs[i].
            // Unlike try_join_all, it doesn't short-circuit on first error —
            // we collect all results then propagate via the final collect().
            // This means a batch with one failing chunk still waits for the
            // other 15; slightly wasteful but simpler than early-abort.
            // The whole wave is bounded by the liveness budget: there
            // is no bare join_all left to call (bug_108).
            for r in budget.run(futures_util::future::join_all(futs)).await? {
                results.push(r?);
            }
        }

        Ok(results)
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        // Full S3 key (bucket-relative, includes prefix). The
        // pending_s3_deletes row stores this; drain task passes
        // it to delete_by_key verbatim.
        self.s3_key(hash)
    }

    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        self.s3_delete(key).await
    }

    /// Batched `DeleteObjects`: up to 1000 keys per round trip (the S3
    /// API maximum). `quiet(true)` — the response carries only the
    /// per-key errors, which map to [`BatchDeleteFailure`]s; a key
    /// absent from `errors()` was deleted (or didn't exist —
    /// idempotent, same as `DeleteObject`).
    async fn delete_by_keys(&self, keys: &[String]) -> anyhow::Result<Vec<BatchDeleteFailure>> {
        const S3_DELETE_OBJECTS_MAX: usize = 1000;
        let mut failures = Vec::new();
        for chunk in keys.chunks(S3_DELETE_OBJECTS_MAX) {
            metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_objects")
                .increment(1);
            let objects = chunk
                .iter()
                .map(|k| {
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key(k)
                        .build()
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("DeleteObjects key build failed: {e}"))?;
            let delete = aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(objects))
                .quiet(true)
                .build()
                .map_err(|e| anyhow::anyhow!("DeleteObjects request build failed: {e}"))?;
            let out = self
                .client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(delete)
                .send()
                .await
                .map_err(|e| {
                    classify_s3_error(
                        e,
                        format!(
                            "S3 DeleteObjects ({} keys) failed for s3://{}",
                            chunk.len(),
                            self.bucket
                        ),
                    )
                })?;
            for err in out.errors() {
                let error = format!(
                    "{}: {}",
                    err.code().unwrap_or("<no code>"),
                    err.message().unwrap_or("<no message>"),
                );
                // An error entry without a Key cannot be attributed to
                // any pending row. Reporting it under a placeholder key
                // would leave the genuinely-failed key OUT of the
                // failure list — the drain would drop its tombstone and
                // the object would leak with no retry. Fail the whole
                // call instead: the drain retries every row in the
                // batch (idempotent for the keys that did delete).
                let Some(key) = err.key().filter(|k| !k.is_empty()) else {
                    anyhow::bail!(
                        "S3 DeleteObjects for s3://{} returned an error entry \
                         without a key ({error}); treating the batch as failed",
                        self.bucket
                    );
                };
                failures.push(BatchDeleteFailure {
                    key: key.to_owned(),
                    error,
                });
            }
        }
        Ok(failures)
    }

    async fn put_blob(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        self.s3_put(&self.blob_s3_key(key), data).await
    }

    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        validate_blob_key(key)?;
        self.s3_get(&self.blob_s3_key(key)).await
    }

    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        validate_blob_key(key)?;
        self.s3_delete(&self.blob_s3_key(key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared hash fixtures. [0x00;32] and [0xFF;32] are edge-cases
    // (all zeros, all ones) — exercise any byte-specific bugs.
    const HASH_A: [u8; 32] = [0x00; 32];
    const HASH_B: [u8; 32] = [0xFF; 32];
    const HASH_C: [u8; 32] = [0xAB; 32];

    /// `is_permanent_auth_error` branch 2 (credential-provider) walks
    /// `Error::source()` — the prior `format!("{e}")` only saw the
    /// outer Display (`"dispatch failure"`), never the buried
    /// credential-chain message, so STS denial classified as transient
    /// and the builder retried forever.
    ///
    /// Builds: outer (no auth marker) → inner ("...credentials...").
    #[test]
    fn is_permanent_auth_error_walks_source_chain() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an error occurred while loading credentials")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer {
            inner: Inner,
            meta: aws_sdk_s3::error::ErrorMetadata,
        }
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                // Mimics SdkError::DispatchFailure's Display: a fixed
                // variant label, NOT a flattened chain.
                f.write_str("dispatch failure")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.inner)
            }
        }
        impl ProvideErrorMetadata for Outer {
            fn meta(&self) -> &aws_sdk_s3::error::ErrorMetadata {
                &self.meta
            }
        }
        let outer = || Outer {
            inner: Inner,
            meta: aws_sdk_s3::error::ErrorMetadata::builder().build(),
        };

        let e = outer();
        // Outer Display alone has no auth marker; the walk must reach Inner.
        assert!(!e.to_string().contains("credentials"));
        assert!(
            is_permanent_auth_error(&e),
            "must walk source() chain to find credential-provider message"
        );

        // classify_s3_error roots the anyhow chain at BackendAuthError
        // AND surfaces the source-chain detail in the context string —
        // the `BackendAuthError` doc promises "the detailed message
        // lives in the anyhow `.context(...)` layer above this marker".
        let any = classify_s3_error(outer(), "ctx".into());
        assert!(any.downcast_ref::<BackendAuthError>().is_some());
        let rendered = format!("{any}");
        assert!(
            rendered.contains("credentials"),
            "auth-branch context must include source-chain detail \
             (Inner's message), got: {rendered}"
        );
        assert!(rendered.starts_with("ctx: "), "caller msg preserved");
    }

    /// Service-level auth (`code()=Some("AccessDenied")`): the IAM
    /// action/principal/resource detail is in `e`'s top-level Display
    /// and must reach the operator-visible context. Pre-fix the chain
    /// was `["ctx", BackendAuthError]` only — the AWS detail was
    /// dropped on the floor.
    #[test]
    fn classify_s3_error_auth_preserves_service_detail() {
        #[derive(Debug)]
        struct Svc {
            meta: aws_sdk_s3::error::ErrorMetadata,
        }
        impl std::fmt::Display for Svc {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "AccessDenied: User arn:aws:iam::123:role/rio-store is not \
                     authorized to perform s3:DeleteObject on resource \
                     arn:aws:s3:::rio-chunks/chunks/ab/cd",
                )
            }
        }
        impl std::error::Error for Svc {}
        impl ProvideErrorMetadata for Svc {
            fn meta(&self) -> &aws_sdk_s3::error::ErrorMetadata {
                &self.meta
            }
        }
        let e = Svc {
            meta: aws_sdk_s3::error::ErrorMetadata::builder()
                .code("AccessDenied")
                .build(),
        };
        assert!(is_permanent_auth_error(&e));

        let any = classify_s3_error(e, "S3 DeleteObject failed for chunks/ab/cd".into());
        // Marker still at root for downcast.
        assert!(any.downcast_ref::<BackendAuthError>().is_some());
        // anyhow Display = outermost context = what `error = %e` logs.
        let rendered = format!("{any}");
        assert!(
            rendered.contains("s3:DeleteObject"),
            "IAM action must reach operator-visible context, got: {rendered}"
        );
        assert!(
            rendered.contains("arn:aws:iam::123:role/rio-store"),
            "principal ARN must reach operator-visible context, got: {rendered}"
        );
    }

    #[test]
    fn chunk_key_format() {
        // First two hex chars of [0xAB;32] = "ab". Full hex = "ab"*32.
        let key = chunk_key(&HASH_C);
        assert_eq!(key, format!("ab/{}", "ab".repeat(32)));
        // All-zeros: prefix "00".
        assert_eq!(&chunk_key(&HASH_A)[..2], "00");
        // All-ones: prefix "ff".
        assert_eq!(&chunk_key(&HASH_B)[..2], "ff");
    }

    /// validate_chunk_key rejects malformed keys before Path::join.
    #[test]
    fn validate_chunk_key_accepts_wellformed() {
        // Round-trip: chunk_key output always validates.
        assert!(validate_chunk_key(&chunk_key(&HASH_A)).is_ok());
        assert!(validate_chunk_key(&chunk_key(&HASH_B)).is_ok());
        assert!(validate_chunk_key(&chunk_key(&HASH_C)).is_ok());
    }

    #[test]
    fn validate_chunk_key_rejects_path_traversal() {
        // THE security case: Path::join("../etc/passwd") would
        // resolve outside base_dir. Reject before join.
        assert!(validate_chunk_key("../etc/passwd").is_err());
        assert!(validate_chunk_key("../../evil").is_err());
        // Absolute path also bad.
        assert!(validate_chunk_key("/etc/passwd").is_err());
    }

    #[test]
    fn validate_chunk_key_rejects_malformed() {
        // No slash.
        assert!(validate_chunk_key("ab1234").is_err());
        // Wrong prefix length.
        assert!(validate_chunk_key("abc/abcdef").is_err());
        // Non-hex prefix.
        assert!(validate_chunk_key("zz/abcdef").is_err());
        // Wrong body length.
        assert!(validate_chunk_key("ab/short").is_err());
        // Non-hex body.
        let bad_body = format!("ab/{}", "z".repeat(64));
        assert!(validate_chunk_key(&bad_body).is_err());
        // Prefix/body mismatch (ab/cd...)
        let mismatch = format!("ab/{}", "cd".repeat(32));
        assert!(validate_chunk_key(&mismatch).is_err());
        // Empty.
        assert!(validate_chunk_key("").is_err());
    }

    #[test]
    fn validate_blob_key_accepts_narinfo_shapes() {
        for k in [
            "abcd1234efgh5678.narinfo",
            "nar/abcdef1234567890.nar.zst",
            "nix-cache-info",
        ] {
            assert!(validate_blob_key(k).is_ok(), "{k}");
        }
    }

    #[test]
    fn validate_blob_key_rejects_traversal_and_collision() {
        for k in [
            "",
            "/etc/passwd",
            "../escape",
            "nar/../../escape",
            "nar//double",
            "./relative",
            "chunks",
            "chunks/00/abc",
        ] {
            assert!(validate_blob_key(k).is_err(), "{k}");
        }
    }

    // ------------------------------------------------------------------------
    // Memory backend
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn memory_put_get_roundtrip() -> anyhow::Result<()> {
        let backend = MemoryChunkBackend::new();
        let data = Bytes::from_static(b"chunk data");

        backend.put(&HASH_A, data.clone()).await?;
        let got = backend.get(&HASH_A).await?.expect("just stored");
        assert_eq!(got, data);
        Ok(())
    }

    #[tokio::test]
    async fn memory_get_missing_none() -> anyhow::Result<()> {
        let backend = MemoryChunkBackend::new();
        assert!(backend.get(&HASH_A).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn memory_exists_batch() -> anyhow::Result<()> {
        let backend = MemoryChunkBackend::new();
        backend.put(&HASH_A, Bytes::from_static(b"a")).await?;
        backend.put(&HASH_C, Bytes::from_static(b"c")).await?;

        // Order matters: result[i] must match input[i].
        let result = backend.exists_batch(&[HASH_A, HASH_B, HASH_C]).await?;
        assert_eq!(result, vec![true, false, true]);
        Ok(())
    }

    #[tokio::test]
    async fn memory_exists_batch_empty() -> anyhow::Result<()> {
        let backend = MemoryChunkBackend::new();
        assert_eq!(backend.exists_batch(&[]).await?, Vec::<bool>::new());
        Ok(())
    }

    #[tokio::test]
    async fn memory_len_tracks_count() -> anyhow::Result<()> {
        let backend = MemoryChunkBackend::new();
        assert!(backend.is_empty());
        backend.put(&HASH_A, Bytes::from_static(b"a")).await?;
        backend.put(&HASH_B, Bytes::from_static(b"b")).await?;
        assert_eq!(backend.len(), 2);
        // Same hash again — idempotent, count unchanged.
        backend.put(&HASH_A, Bytes::from_static(b"a")).await?;
        assert_eq!(backend.len(), 2);
        Ok(())
    }

    /// Common blob-API contract, parameterised over backend.
    async fn blob_roundtrip(b: &dyn ChunkBackend) -> anyhow::Result<()> {
        let narinfo = Bytes::from_static(b"StorePath: /nix/store/abc\n");
        let nar = Bytes::from_static(b"\x0d\x00\x00\x00nix-archive-1");

        b.put_blob("abc.narinfo", narinfo.clone()).await?;
        b.put_blob("nar/abc.nar.zst", nar.clone()).await?;
        assert_eq!(b.get_blob("abc.narinfo").await?, Some(narinfo.clone()));
        assert_eq!(b.get_blob("nar/abc.nar.zst").await?, Some(nar));
        assert_eq!(b.get_blob("missing.narinfo").await?, None);

        // Last-writer-wins.
        let v2 = Bytes::from_static(b"v2");
        b.put_blob("abc.narinfo", v2.clone()).await?;
        assert_eq!(b.get_blob("abc.narinfo").await?, Some(v2));

        // Idempotent delete.
        b.delete_blob("abc.narinfo").await?;
        assert_eq!(b.get_blob("abc.narinfo").await?, None);
        b.delete_blob("abc.narinfo").await?;

        // Traversal rejected on every entry point.
        for k in ["../escape", "/abs", "chunks/00/x"] {
            assert!(b.put_blob(k, Bytes::new()).await.is_err(), "put {k}");
            assert!(b.get_blob(k).await.is_err(), "get {k}");
            assert!(b.delete_blob(k).await.is_err(), "delete {k}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn memory_blob_roundtrip() -> anyhow::Result<()> {
        blob_roundtrip(&MemoryChunkBackend::new()).await
    }

    /// Blob namespace and chunk namespace don't alias.
    #[tokio::test]
    async fn memory_blob_separate_from_chunks() -> anyhow::Result<()> {
        let b = MemoryChunkBackend::new();
        b.put(&HASH_A, Bytes::from_static(b"chunk")).await?;
        b.put_blob("abc.narinfo", Bytes::from_static(b"blob"))
            .await?;
        assert_eq!(b.len(), 1, "blob must not count as a chunk");
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Filesystem backend
    // ------------------------------------------------------------------------

    #[test]
    // r[verify store.backend.filesystem]
    fn fs_precreates_subdirs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let _backend = FilesystemChunkBackend::new(dir.path())?;

        // Spot-check a few subdirs (all 256 would be excessive for a test).
        for prefix in ["00", "7f", "ab", "ff"] {
            let subdir = dir.path().join("chunks").join(prefix);
            assert!(subdir.is_dir(), "subdir {prefix} should exist");
        }
        Ok(())
    }

    #[tokio::test]
    async fn fs_put_get_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        let data = Bytes::from_static(b"filesystem chunk data");

        backend.put(&HASH_C, data.clone()).await?;
        let got = backend.get(&HASH_C).await?.expect("just stored");
        assert_eq!(got, data);

        // Verify it landed in the expected subdir (ab/).
        let expected_path = dir.path().join("chunks").join("ab").join("ab".repeat(32));
        assert!(expected_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn fs_get_missing_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        assert!(backend.get(&HASH_A).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn fs_exists_batch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        backend.put(&HASH_A, Bytes::from_static(b"a")).await?;
        backend.put(&HASH_C, Bytes::from_static(b"c")).await?;

        let result = backend.exists_batch(&[HASH_A, HASH_B, HASH_C]).await?;
        assert_eq!(result, vec![true, false, true]);
        Ok(())
    }

    /// The atomic-write property: a partially-written chunk (crash mid-put)
    /// should NOT be visible. We can't actually crash in a test, but we CAN
    /// verify that the .tmp file doesn't linger after a successful put.
    #[tokio::test]
    async fn fs_put_leaves_no_tmp() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        backend.put(&HASH_C, Bytes::from_static(b"data")).await?;

        // No .tmp files anywhere in chunks/ab/.
        let subdir = dir.path().join("chunks").join("ab");
        for entry in std::fs::read_dir(&subdir)? {
            let name = entry?.file_name();
            #[allow(clippy::disallowed_methods)] // test assertion display only
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".tmp"), "leftover .tmp file: {name}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn fs_blob_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        blob_roundtrip(&backend).await?;
        // Layout: blobs at {root}/blobs/{key}, sibling of chunks/.
        assert!(dir.path().join("blobs").join("nar").is_dir());
        assert!(dir.path().join("blobs").join("nar/abc.nar.zst").is_file());
        Ok(())
    }

    /// Only `NotFound` is soft; other I/O errors must propagate.
    #[tokio::test]
    async fn fs_blob_io_errors_propagate() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FilesystemChunkBackend::new(dir.path())?;
        std::fs::create_dir_all(dir.path().join("blobs").join("abc.narinfo"))?;
        for r in [
            backend.get_blob("abc.narinfo").await.map(|_| ()),
            backend.delete_blob("abc.narinfo").await,
        ] {
            let kind = r.unwrap_err().downcast::<std::io::Error>().unwrap().kind();
            assert_ne!(kind, std::io::ErrorKind::NotFound);
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // S3 backend (aws-smithy-mocks)
    // ------------------------------------------------------------------------

    use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
    use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::error::{NoSuchKey, NotFound};
    use aws_smithy_mocks::{RuleMode, mock, mock_client};

    fn make_s3_backend(client: Client) -> S3ChunkBackend {
        S3ChunkBackend::new(
            client,
            "test-bucket".into(),
            "test-prefix".into(),
            Semaphore::MAX_PERMITS,
        )
    }

    #[test]
    fn s3_key_format() {
        // Dummy client — s3_key() doesn't touch it.
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);

        let with_prefix = S3ChunkBackend::new(client.clone(), "b".into(), "myprefix".into(), 1);
        assert_eq!(
            with_prefix.s3_key(&HASH_C),
            format!("myprefix/chunks/ab/{}", "ab".repeat(32))
        );

        let no_prefix = S3ChunkBackend::new(client.clone(), "b".into(), "".into(), 1);
        assert_eq!(
            no_prefix.s3_key(&HASH_C),
            format!("chunks/ab/{}", "ab".repeat(32))
        );

        // Trailing slash normalized away at construction. I-040 regression
        // pin: an early Helm default of prefix="chunks/" produced
        // "chunks//chunks/ab/..." keys (current default is "").
        let trailing = S3ChunkBackend::new(client.clone(), "b".into(), "chunks/".into(), 1);
        let bare = S3ChunkBackend::new(client.clone(), "b".into(), "chunks".into(), 1);
        assert_eq!(
            trailing.s3_key(&HASH_C),
            bare.s3_key(&HASH_C),
            "I-040 pin: \"chunks/\" and \"chunks\" MUST produce identical keys — \
             any divergence strands data across a deploy that toggles the slash"
        );
        assert_eq!(
            trailing.s3_key(&HASH_C),
            format!("chunks/chunks/ab/{}", "ab".repeat(32)),
            "trailing slash in prefix must not produce double-slash key"
        );
        assert!(!trailing.s3_key(&HASH_C).contains("//"));

        // Multiple trailing slashes also stripped.
        let multi_trailing = S3ChunkBackend::new(client.clone(), "b".into(), "prod///".into(), 1);
        assert_eq!(
            multi_trailing.s3_key(&HASH_C),
            format!("prod/chunks/ab/{}", "ab".repeat(32))
        );

        // Prefix of just "/" → same as empty.
        let slash_only = S3ChunkBackend::new(client, "b".into(), "/".into(), 1);
        assert_eq!(
            slash_only.s3_key(&HASH_C),
            format!("chunks/ab/{}", "ab".repeat(32))
        );
    }

    #[tokio::test]
    async fn s3_get_found() -> anyhow::Result<()> {
        let rule = mock!(Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"s3 chunk data"))
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_s3_backend(client);

        let got = backend.get(&HASH_A).await?.expect("mock returns data");
        assert_eq!(got.as_ref(), b"s3 chunk data");
        Ok(())
    }

    /// NoSuchKey → Ok(None), not Err. "Not there" vs "can't tell" are
    /// different — callers need to distinguish miss from transient error.
    #[tokio::test]
    async fn s3_get_nosuchkey_none() -> anyhow::Result<()> {
        let rule = mock!(Client::get_object)
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_s3_backend(client);

        let got = backend.get(&HASH_A).await?;
        assert!(got.is_none());
        Ok(())
    }

    /// Transient server error → Err, NOT Ok(None). Conflating these makes
    /// every S3 hiccup look like data loss.
    #[tokio::test]
    async fn s3_get_server_error_propagates() {
        use aws_sdk_s3::error::ErrorMetadata;
        let rule = mock!(Client::get_object).then_error(|| {
            GetObjectError::generic(ErrorMetadata::builder().code("InternalError").build())
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule]);
        let backend = make_s3_backend(client);

        let result = backend.get(&HASH_A).await;
        assert!(
            result.is_err(),
            "transient error should be Err, not Ok(None)"
        );
    }

    /// opt-02: the replica-global PUT gate is wired into
    /// `S3ChunkBackend::put()` itself (not just composed at the
    /// `do_upload` layer). With `permits=1` and the one permit held
    /// externally, `put()` parks at `acquire_owned().await` and never
    /// reaches the SDK send — `tokio::time::timeout` observes the
    /// park. Dropping the external permit unblocks the next `put()`.
    #[tokio::test]
    async fn s3_put_gate_parks_when_exhausted() {
        use aws_sdk_s3::operation::put_object::PutObjectOutput;
        use std::time::Duration;

        let rule = mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);
        let backend = S3ChunkBackend::new(client, "b".into(), "".into(), 1);

        // Exhaust the gate externally.
        let held = backend.put_gate.clone().try_acquire_owned().expect("fresh");

        let parked = tokio::time::timeout(
            Duration::from_millis(50),
            backend.put(&HASH_A, Bytes::from_static(b"x")),
        )
        .await;
        assert!(
            parked.is_err(),
            "put() must park when the global gate is exhausted; \
             completed instead — gate not wired before send"
        );

        drop(held);
        tokio::time::timeout(
            Duration::from_millis(50),
            backend.put(&HASH_A, Bytes::from_static(b"x")),
        )
        .await
        .expect("put() must complete once a permit is available")
        .expect("mock PutObject returns Ok");
    }

    #[tokio::test]
    async fn s3_exists_batch_ordering() -> anyhow::Result<()> {
        // Three HeadObject calls: found, not-found, found.
        // exists_batch awaits each wave via futures_util::future::join_all,
        // so results stay in input order even though the calls run in
        // parallel.
        let r1 = mock!(Client::head_object).then_output(|| HeadObjectOutput::builder().build());
        let r2 = mock!(Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let r3 = mock!(Client::head_object).then_output(|| HeadObjectOutput::builder().build());
        // Sequential mode: join_all preserves the order futures were
        // CREATED (not the order they COMPLETE), so output[i] corresponds
        // to input[i] regardless of which mock fires first. (The sleep
        // impl backs the HEAD lane's per-op retry/timeout override —
        // the SDK refuses retry/timeout config without one.)
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&r1, &r2, &r3], |b| b
            .sleep_impl(TestTokioSleep));
        let backend = make_s3_backend(client);

        let result = backend.exists_batch(&[HASH_A, HASH_B, HASH_C]).await?;
        assert_eq!(result, vec![true, false, true]);
        Ok(())
    }

    /// bug_108 red, re-aimed by merged_bug_006: a black-holed
    /// connection — established, then silent forever — must fail
    /// TYPED within ADMIN_VERIFY_WORST_WAVE, not hang past the
    /// client's 120 s inactivity bound. Post-envelope, the proposition
    /// this certifies (R16) is sharper: the wave exhausts the
    /// envelope's per-attempt timeouts and fails via the CLASSIFIED
    /// SDK error BEFORE the budget elapses (the budget-elapse
    /// direction has its own witness in rio-common:
    /// `wave_budget_backstop_fires_beyond_the_envelope`). The client
    /// is the production constructor over a test connector
    /// (production wire shapes, no hand-rolled backend stub): a
    /// custom HttpClient whose connector future never resolves is
    /// exactly the established-then-black-holed peer.
    #[tokio::test(start_paused = true)]
    async fn black_holed_head_fails_typed_within_the_wave_budget() {
        use aws_smithy_runtime_api::client::http::{
            HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings,
            SharedHttpConnector,
        };
        use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
        use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;

        #[derive(Debug, Clone)]
        struct BlackHole;
        impl HttpConnector for BlackHole {
            fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
                HttpConnectorFuture::new(std::future::pending())
            }
        }
        impl HttpClient for BlackHole {
            fn http_connector(
                &self,
                _settings: &HttpConnectorSettings,
                _components: &RuntimeComponents,
            ) -> SharedHttpConnector {
                SharedHttpConnector::new(self.clone())
            }
        }

        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
            .http_client(BlackHole)
            .sleep_impl(TestTokioSleep)
            .build();
        let backend = make_s3_backend(Client::from_conf(cfg));

        let budget = rio_common::liveness::admin_verify_wave_budget();
        let outer = budget.duration() * 2;
        let started = tokio::time::Instant::now();
        match tokio::time::timeout(outer, backend.exists_batch(&[HASH_A])).await {
            Err(_elapsed) => panic!(
                "exists_batch hung past 2x the wave budget against a black-holed \
                 connection — neither the retry envelope nor the budget backstop \
                 is wired"
            ),
            Ok(Ok(_)) => panic!("a black-holed HEAD cannot succeed"),
            Ok(Err(e)) => {
                // Paused-clock elapsed assertion: the failure lands
                // within the wave const (envelope worst case 3.9 s
                // < 5 s — the envelope exhausts FIRST).
                let elapsed = started.elapsed();
                assert!(
                    elapsed <= rio_common::liveness::ADMIN_VERIFY_WORST_WAVE,
                    "a black-holed wave must fail within ADMIN_VERIFY_WORST_WAVE; \
                     took {elapsed:?}"
                );
                // The envelope (below the budget) classifies the
                // failure — the budget backstop must NOT be the layer
                // that fired.
                assert!(
                    e.downcast_ref::<rio_common::liveness::WaveBudgetExceeded>()
                        .is_none(),
                    "the per-attempt envelope must exhaust before the budget \
                     backstop fires; got the backstop's elapse: {e:#}"
                );
            }
        }
    }

    /// Paused-clock-respecting sleep for the SDK's retry/timeout
    /// plumbing (the SDK re-exports the `AsyncSleep` trait but not a
    /// tokio sleeper; this adapter is runtime infrastructure, not a
    /// wire/identity fixture).
    #[derive(Debug, Clone)]
    struct TestTokioSleep;
    impl aws_sdk_s3::config::AsyncSleep for TestTokioSleep {
        fn sleep(&self, duration: std::time::Duration) -> aws_sdk_s3::config::Sleep {
            aws_sdk_s3::config::Sleep::new(tokio::time::sleep(duration))
        }
    }

    /// Production `aws_sdk_s3::Client` over a SCRIPTED test connector
    /// (the BlackHole pattern extended to scripted responses,
    /// R13-conformant): the first `hung` connector calls never
    /// resolve — the established-then-recycled pooled connection the
    /// rustfs/MinIO churn produces — and every later call answers a
    /// bare HeadObject 200 on a fresh connection.
    fn scripted_churn_client(hung: usize) -> Client {
        use aws_smithy_runtime_api::client::http::{
            HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings,
            SharedHttpConnector,
        };
        use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
        use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug, Clone)]
        struct Scripted {
            hung: usize,
            calls: Arc<AtomicUsize>,
        }
        impl HttpConnector for Scripted {
            fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
                if self.calls.fetch_add(1, Ordering::SeqCst) < self.hung {
                    HttpConnectorFuture::new(std::future::pending())
                } else {
                    HttpConnectorFuture::new(async {
                        Ok(aws_smithy_runtime_api::http::Response::new(
                            aws_smithy_runtime_api::http::StatusCode::try_from(200)
                                .expect("static status"),
                            aws_sdk_s3::primitives::SdkBody::empty(),
                        ))
                    })
                }
            }
        }
        impl HttpClient for Scripted {
            fn http_connector(
                &self,
                _settings: &HttpConnectorSettings,
                _components: &RuntimeComponents,
            ) -> SharedHttpConnector {
                SharedHttpConnector::new(self.clone())
            }
        }

        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
            .http_client(Scripted {
                hung,
                calls: Arc::default(),
            })
            .sleep_impl(TestTokioSleep)
            .build();
        Client::from_conf(cfg)
    }

    /// merged_bug_006 red: a LAWFUL churn-recovery retry ladder — the
    /// exact rustfs/MinIO connection-churn recovery the raised
    /// attempt count exists for — must COMPLETE inside the wave
    /// budget. Attempts 1-2 hang (pooled connections black-holed by a
    /// server-side recycle); attempt 3 succeeds on a fresh
    /// connection. Proposition certified (R16): the HEAD lane's retry
    /// envelope sits BELOW the wave budget, so lawful churn recovery
    /// finishes inside it — the claim the 5 s const's "SDK retries
    /// included" sentence rides on. (The black-holed test certifies
    /// only the easy direction: a never-resolving wave dies typed.)
    #[tokio::test(start_paused = true)]
    async fn churn_recovery_ladder_completes_inside_the_wave_budget() {
        let backend = make_s3_backend(scripted_churn_client(2));

        let started = tokio::time::Instant::now();
        let result = backend.exists_batch(&[HASH_A]).await;
        let elapsed = started.elapsed();

        let exists = result.unwrap_or_else(|e| {
            panic!(
                "a lawful churn-recovery ladder (2 hung attempts, then success) \
                 must complete INSIDE the wave budget — cancelling it aborts \
                 whole audits with no resume cursor; got after {elapsed:?}: {e:#}"
            )
        });
        assert_eq!(exists, vec![true]);
        assert!(
            elapsed <= rio_common::liveness::ADMIN_VERIFY_WORST_WAVE,
            "the recovered ladder must fit the wave budget; took {elapsed:?}"
        );
    }

    /// D5 ordering law (R17): every op-class retry ladder at the
    /// DEFAULT `s3_max_attempts` knob (10) — per-attempt timeout ×
    /// attempts + SDK-standard capped backoffs (1 s initial, 20 s
    /// max) — plus its body clock exhausts STRICTLY inside the
    /// smallest NAR-hold grace (NAR_HOLD_GRACE_FACTOR × the default
    /// stall window = 15 min), so the seam is the first line and the
    /// hold envelope stays the backstop. An operator raising the
    /// knob re-derives: the relation holds to ~170 attempts; the
    /// knob doc cites this test.
    #[test]
    fn chunk_op_envelope_exhausts_inside_the_hold_grace() {
        use rio_common::liveness::RetryEnvelope;
        let sdk_standard = |attempt_timeout| RetryEnvelope {
            attempts: rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS,
            attempt_timeout,
            initial_backoff: std::time::Duration::from_secs(1),
            max_backoff: std::time::Duration::from_secs(20),
        };
        let grace = crate::substitute::DEFAULT_SUBSTITUTE_STALL_WINDOW
            * crate::substitute::NAR_HOLD_GRACE_FACTOR;
        let chunk = sdk_standard(CHUNK_OP_ATTEMPT_TIMEOUT).worst_case() + CHUNK_GET_BODY_TIMEOUT;
        assert!(
            chunk < grace,
            "chunk-plane worst ladder ({chunk:?}) must exhaust inside the hold grace ({grace:?})"
        );
        let log = sdk_standard(LOG_OP_ATTEMPT_TIMEOUT).worst_case() + LOG_GET_BODY_TIMEOUT;
        assert!(
            log < grace,
            "log-plane worst ladder ({log:?}) must exhaust inside the hold grace ({grace:?})"
        );
    }

    /// W9-BH (R16 statement, the PUT face): a black-holed chunk
    /// PutObject — established connection, response never arrives —
    /// fails TYPED within its op-class envelope's worst case
    /// (attempt-count axis = the production knob, time axis = the
    /// per-attempt const; R17 all-axes), instead of awaiting headers
    /// forever. Production client shape (standard retry at the
    /// default knob) over the BlackHole connector; paused clock.
    /// RED pre-fix (verbatim in the landing commit): the put future
    /// was still pending at 2x the ladder worst case — no per-op
    /// timeout existed at the seam.
    #[tokio::test(start_paused = true)]
    async fn black_holed_chunk_put_fails_typed_within_its_op_envelope() {
        let backend = make_s3_backend(black_holed_knob_client());
        let worst = rio_common::liveness::RetryEnvelope {
            attempts: rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS,
            attempt_timeout: CHUNK_OP_ATTEMPT_TIMEOUT,
            initial_backoff: std::time::Duration::from_secs(1),
            max_backoff: std::time::Duration::from_secs(20),
        }
        .worst_case();
        let started = tokio::time::Instant::now();
        match tokio::time::timeout(worst * 2, backend.put(&HASH_A, Bytes::from_static(b"x"))).await
        {
            Err(_elapsed) => panic!(
                "chunk put hung past 2x its op-envelope worst case against a \
                 black-holed connection — the per-op timeout is not wired"
            ),
            Ok(Ok(())) => panic!("a black-holed put cannot succeed"),
            Ok(Err(_e)) => {
                let elapsed = started.elapsed();
                assert!(
                    elapsed <= worst,
                    "a black-holed put must fail within the ladder worst case \
                     ({worst:?}); took {elapsed:?}"
                );
            }
        }
    }

    /// W9-BH (the GET face): same statement for GetObject — the
    /// header wait deadlines per attempt; the ladder exhausts typed.
    #[tokio::test(start_paused = true)]
    async fn black_holed_chunk_get_fails_typed_within_its_op_envelope() {
        let backend = make_s3_backend(black_holed_knob_client());
        let worst = rio_common::liveness::RetryEnvelope {
            attempts: rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS,
            attempt_timeout: CHUNK_OP_ATTEMPT_TIMEOUT,
            initial_backoff: std::time::Duration::from_secs(1),
            max_backoff: std::time::Duration::from_secs(20),
        }
        .worst_case();
        let started = tokio::time::Instant::now();
        match tokio::time::timeout(worst * 2, backend.get(&HASH_A)).await {
            Err(_elapsed) => panic!(
                "chunk get hung past 2x its op-envelope worst case against a \
                 black-holed connection — the per-op timeout is not wired"
            ),
            Ok(Ok(_)) => panic!("a black-holed get cannot succeed"),
            Ok(Err(_e)) => {
                let elapsed = started.elapsed();
                assert!(
                    elapsed <= worst,
                    "a black-holed get must fail within the ladder worst case \
                     ({worst:?}); took {elapsed:?}"
                );
            }
        }
    }

    /// Production-shaped client (standard retry at the DEFAULT
    /// `s3_max_attempts` knob — exactly `rio_common::s3::
    /// default_client`'s retry posture) over a connector whose
    /// futures never resolve: the established-then-black-holed peer.
    fn black_holed_knob_client() -> Client {
        use aws_smithy_runtime_api::client::http::{
            HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings,
            SharedHttpConnector,
        };
        use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
        use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;

        #[derive(Debug, Clone)]
        struct BlackHole;
        impl HttpConnector for BlackHole {
            fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
                HttpConnectorFuture::new(std::future::pending())
            }
        }
        impl HttpClient for BlackHole {
            fn http_connector(
                &self,
                _settings: &HttpConnectorSettings,
                _components: &RuntimeComponents,
            ) -> SharedHttpConnector {
                SharedHttpConnector::new(self.clone())
            }
        }

        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
            .retry_config(
                aws_sdk_s3::config::retry::RetryConfig::standard()
                    .with_max_attempts(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS),
            )
            .http_client(BlackHole)
            .sleep_impl(TestTokioSleep)
            .build();
        Client::from_conf(cfg)
    }

    #[test]
    fn blob_s3_key_format() {
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);

        // Blobs live at the prefix root, not under chunks/.
        let with_prefix = S3ChunkBackend::new(
            client.clone(),
            "b".into(),
            "prod".into(),
            Semaphore::MAX_PERMITS,
        );
        assert_eq!(with_prefix.blob_s3_key("abc.narinfo"), "prod/abc.narinfo");
        assert_eq!(
            with_prefix.blob_s3_key("nar/abc.nar.zst"),
            "prod/nar/abc.nar.zst"
        );

        let no_prefix = S3ChunkBackend::new(client, "b".into(), "".into(), Semaphore::MAX_PERMITS);
        assert_eq!(no_prefix.blob_s3_key("abc.narinfo"), "abc.narinfo");
    }

    /// Blob ops use blob_s3_key (no `chunks/` segment) and validate
    /// the key. The mock client is keyed by operation, so the get/put
    /// rule fires for whatever key is sent — blob_s3_key_format tests
    /// the actual key shape.
    #[tokio::test]
    async fn s3_blob_roundtrip() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
        use aws_sdk_s3::operation::put_object::PutObjectOutput;
        let put_r = mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let get_r = mock!(Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"narinfo body"))
                .build()
        });
        let miss_r = mock!(Client::get_object)
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let del_r =
            mock!(Client::delete_object).then_output(|| DeleteObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&put_r, &get_r, &miss_r, &del_r]
        );
        let b = make_s3_backend(client);

        b.put_blob("abc.narinfo", Bytes::from_static(b"x")).await?;
        assert_eq!(
            b.get_blob("abc.narinfo").await?.as_deref(),
            Some(b"narinfo body".as_slice())
        );
        assert!(b.get_blob("missing.narinfo").await?.is_none());
        b.delete_blob("abc.narinfo").await?;

        // Traversal rejected before any S3 call — every entry point.
        assert!(b.put_blob("../escape", Bytes::new()).await.is_err());
        assert!(b.get_blob("../escape").await.is_err());
        assert!(b.delete_blob("../escape").await.is_err());
        Ok(())
    }

    /// `delete_by_keys` batches N keys into ONE `DeleteObjects`
    /// request (the whole point — the per-object `DeleteObject` drain
    /// capped at ~3.3 deletes/s) and an error-free quiet response
    /// means zero failures.
    #[tokio::test]
    async fn s3_delete_by_keys_single_request() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::delete_objects::DeleteObjectsOutput;
        let del_r =
            mock!(Client::delete_objects).then_output(|| DeleteObjectsOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&del_r]);
        let b = make_s3_backend(client);

        let keys: Vec<String> = (0u8..3).map(|i| b.key_for(&[i; 32])).collect();
        let failures = b.delete_by_keys(&keys).await?;
        assert!(failures.is_empty(), "quiet success → no failures");
        assert_eq!(del_r.num_calls(), 1, "3 keys → ONE DeleteObjects call");
        Ok(())
    }

    /// Per-key errors in the `DeleteObjects` response surface as
    /// `BatchDeleteFailure`s (key + code/message) — the drain keeps
    /// those rows tombstoned for retry instead of silently dropping
    /// them with the successful majority.
    #[tokio::test]
    async fn s3_delete_by_keys_reports_per_key_errors() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::delete_objects::DeleteObjectsOutput;
        use aws_sdk_s3::types::Error as S3Error;
        let failed_key = "p/chunks/aa/".to_owned() + &"aa".repeat(32);
        let failed_key_rule = failed_key.clone();
        let del_r = mock!(Client::delete_objects).then_output(move || {
            DeleteObjectsOutput::builder()
                .errors(
                    S3Error::builder()
                        .key(&failed_key_rule)
                        .code("InternalError")
                        .message("We encountered an internal error.")
                        .build(),
                )
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&del_r]);
        let b = make_s3_backend(client);

        let keys = vec![failed_key.clone(), b.key_for(&[0xBB; 32])];
        let failures = b.delete_by_keys(&keys).await?;
        assert_eq!(failures.len(), 1, "only the errored key fails");
        assert_eq!(failures[0].key, failed_key);
        assert!(
            failures[0].error.contains("InternalError"),
            "error carries the S3 code; got {:?}",
            failures[0].error
        );
        Ok(())
    }

    /// An error entry WITHOUT a Key cannot be attributed to any
    /// pending row. Mapping it to `""` would let the genuinely-failed
    /// key be absent from the failure list — the drain would treat it
    /// as deleted, remove its tombstone, and the S3 object would leak
    /// with no retry and no operator signal. It must surface as a
    /// whole-call `Err` instead: the drain then bumps `attempts` on
    /// every row in the batch and retries (idempotent for the keys
    /// that did delete).
    #[tokio::test]
    async fn s3_delete_by_keys_keyless_error_fails_whole_call() {
        use aws_sdk_s3::operation::delete_objects::DeleteObjectsOutput;
        use aws_sdk_s3::types::Error as S3Error;
        let del_r = mock!(Client::delete_objects).then_output(|| {
            DeleteObjectsOutput::builder()
                .errors(
                    // No .key(...) — a malformed quiet-mode error entry.
                    S3Error::builder()
                        .code("InternalError")
                        .message("We encountered an internal error.")
                        .build(),
                )
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&del_r]);
        let b = make_s3_backend(client);

        let keys = vec![b.key_for(&[0xAA; 32]), b.key_for(&[0xBB; 32])];
        let res = b.delete_by_keys(&keys).await;
        let err = res.expect_err("keyless error entry must fail the whole call");
        assert!(
            err.to_string().contains("without a key"),
            "error should name the keyless entry; got {err:#}"
        );
    }
}
