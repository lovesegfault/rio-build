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
use std::sync::RwLock;

use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use bytes::Bytes;
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
fn classify_s3_error<E>(e: E, msg: String) -> anyhow::Error
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
    /// GC sweep to enqueue the key to `pending_s3_deletes` in the SAME
    /// PG transaction as the refcount decrement (two-phase commit for
    /// S3 cleanup — enqueue atomically, delete later).
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
    /// Creates `{root}/chunks/` with all 256 `{aa}/` subdirs eagerly
    /// (~1 ms upfront) so `put` never check-then-mkdirs on the hot
    /// path, and `{root}/blobs/` shallow (`put_blob` mkdirs nested
    /// components on demand — `nar/` is the only one and it's rare).
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let base_dir = root.join("chunks");
        let blobs_dir = root.join("blobs");
        std::fs::create_dir_all(&base_dir)?;
        std::fs::create_dir_all(&blobs_dir)?;
        // Precreate all 256 two-char-hex subdirectories.
        for b in 0u8..=255 {
            std::fs::create_dir_all(base_dir.join(format!("{b:02x}")))?;
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
}

impl S3ChunkBackend {
    pub fn new(client: Client, bucket: String, prefix: String) -> Self {
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
        debug!(bucket = %self.bucket, key = %key, size = data.len(), "S3ChunkBackend: uploading");
        metrics::counter!("rio_store_s3_requests_total", "operation" => "put_object").increment(1);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.into())
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
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            // Buffer into Bytes — chunks are ≤256 KiB and blobs are
            // narinfo-sized; ByteStream::collect is zero-copy for a
            // single segment.
            Ok(out) => Ok(Some(
                out.body
                    .collect()
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("S3 body read failed for s3://{}/{key}: {e}", self.bucket)
                    })?
                    .into_bytes(),
            )),
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
    async fn s3_delete(&self, key: &str) -> anyhow::Result<()> {
        metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_object")
            .increment(1);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
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

#[async_trait::async_trait]
impl ChunkBackend for S3ChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        self.s3_put(&self.s3_key(hash), data).await
    }

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
        // join_all. Simpler than pulling in futures-util just for buffered().
        // Output order preserved: chunks_of_16[i] maps to hashes[i*16..].
        const CONCURRENCY: usize = 16;
        let mut results = Vec::with_capacity(hashes.len());

        for batch in hashes.chunks(CONCURRENCY) {
            let futs: Vec<_> = batch
                .iter()
                .map(|hash| {
                    let key = self.s3_key(hash);
                    let client = self.client.clone();
                    let bucket = self.bucket.clone();
                    async move {
                        metrics::counter!(
                            "rio_store_s3_requests_total", "operation" => "head_object"
                        )
                        .increment(1);
                        match client.head_object().bucket(&bucket).key(&key).send().await {
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
            for r in futures_util::future::join_all(futs).await {
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
        S3ChunkBackend::new(client, "test-bucket".into(), "test-prefix".into())
    }

    #[test]
    fn s3_key_format() {
        // Dummy client — s3_key() doesn't touch it.
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);

        let with_prefix = S3ChunkBackend::new(client.clone(), "b".into(), "myprefix".into());
        assert_eq!(
            with_prefix.s3_key(&HASH_C),
            format!("myprefix/chunks/ab/{}", "ab".repeat(32))
        );

        let no_prefix = S3ChunkBackend::new(client.clone(), "b".into(), "".into());
        assert_eq!(
            no_prefix.s3_key(&HASH_C),
            format!("chunks/ab/{}", "ab".repeat(32))
        );

        // Trailing slash normalized away at construction. I-040 regression
        // pin: an early Helm default of prefix="chunks/" produced
        // "chunks//chunks/ab/..." keys (current default is "").
        let trailing = S3ChunkBackend::new(client.clone(), "b".into(), "chunks/".into());
        let bare = S3ChunkBackend::new(client.clone(), "b".into(), "chunks".into());
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
        let multi_trailing = S3ChunkBackend::new(client.clone(), "b".into(), "prod///".into());
        assert_eq!(
            multi_trailing.s3_key(&HASH_C),
            format!("prod/chunks/ab/{}", "ab".repeat(32))
        );

        // Prefix of just "/" → same as empty.
        let slash_only = S3ChunkBackend::new(client, "b".into(), "/".into());
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

    #[tokio::test]
    async fn s3_exists_batch_ordering() -> anyhow::Result<()> {
        // Three HeadObject calls: found, not-found, found.
        // exists_batch uses buffered() (not buffer_unordered) so results
        // stay in input order even though the calls run in parallel.
        let r1 = mock!(Client::head_object).then_output(|| HeadObjectOutput::builder().build());
        let r2 = mock!(Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let r3 = mock!(Client::head_object).then_output(|| HeadObjectOutput::builder().build());
        // Sequential mode: buffered() preserves the order futures were
        // CREATED (not the order they COMPLETE), so output[i] corresponds
        // to input[i] regardless of which mock fires first.
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&r1, &r2, &r3]);
        let backend = make_s3_backend(client);

        let result = backend.exists_batch(&[HASH_A, HASH_B, HASH_C]).await?;
        assert_eq!(result, vec![true, false, true]);
        Ok(())
    }

    #[test]
    fn blob_s3_key_format() {
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);

        // Blobs live at the prefix root, not under chunks/.
        let with_prefix = S3ChunkBackend::new(client.clone(), "b".into(), "prod".into());
        assert_eq!(with_prefix.blob_s3_key("abc.narinfo"), "prod/abc.narinfo");
        assert_eq!(
            with_prefix.blob_s3_key("nar/abc.nar.zst"),
            "prod/nar/abc.nar.zst"
        );

        let no_prefix = S3ChunkBackend::new(client, "b".into(), "".into());
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
