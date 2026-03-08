//! Chunk storage backends.
//!
//! Stores BLAKE3-addressed chunks. Three impls: S3 (prod), filesystem
//! (dev), memory (tests). The trait is minimal — put/get/exists_batch —
//! because chunks are immutable and content-addressed: no update, no
//! rename, delete only via GC (not this trait's concern; that's the
//! `pending_s3_deletes` outbox pattern in a later phase).
//!
//! # Design
//!
//! - Keys are `[u8; 32]` (BLAKE3), not string. No path-traversal concern
//!   (hex-encoding a fixed-width array can't contain `../`).
//! - `get` returns `Bytes` (owned), not `AsyncRead`. Chunks are small
//!   (max 256 KiB); buffering into memory is fine and simplifies callers
//!   — no stream-juggling during reassembly.
//! - `exists_batch` (not `exists`): PutPath checks ~100s of chunks at
//!   once. A batch RPC (or local batch check) beats 100 sequential calls.
//!
//! # S3 key scheme
//!
//! `chunks/{aa}/{blake3-hex}` where `{aa}` is the first two hex chars.
//! Prefix-partitioning per `store.md:54` — S3 shards by key prefix, so
//! spreading across 256 prefixes avoids hotspotting a single shard when
//! a thousand workers hit the store at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use aws_sdk_s3::Client;
use bytes::Bytes;
use tracing::debug;

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
    /// (C4's `ChunkCache::get_verified`). Layered this way so the cache
    /// verifies exactly once regardless of whether the bytes came from
    /// the backend or the in-process LRU.
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>>;

    /// Batch existence check. Returns a `Vec<bool>` parallel to `hashes`:
    /// `result[i]` is `true` if `hashes[i]` is present.
    ///
    /// PutPath calls this BEFORE uploading to skip chunks that already
    /// exist (the dedup fast-path). For the memory backend it's a HashMap
    /// lookup loop; for S3 it's a batch of HeadObject calls (C3 might
    /// switch to checking the `chunks` PG table instead — faster than S3).
    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>>;

    /// Delete a chunk. Used by the GC drain task (pending_s3_deletes).
    ///
    /// `Err` on I/O failure (S3 down, permission denied). "Already gone"
    /// is `Ok` — idempotent, the drain might retry a partially-processed
    /// batch. The drain task increments `attempts` on Err and retries
    /// with backoff; after max attempts it stops (alert-worthy but not
    /// a process crash — S3 objects leak, PG state is correct).
    async fn delete(&self, hash: &[u8; 32]) -> anyhow::Result<()>;

    /// Compute the storage key for a hash without doing I/O. Used by
    /// GC sweep to enqueue the key to `pending_s3_deletes` in the SAME
    /// PG transaction as the refcount decrement (two-phase commit for
    /// S3 cleanup — enqueue atomically, delete later).
    ///
    /// Returns the backend-specific key: for S3 it's `prefix/aa/hex`
    /// (bucket-relative); for filesystem it's the relative path. The
    /// drain task passes this back to `delete_by_key`.
    fn key_for(&self, hash: &[u8; 32]) -> String;

    /// Delete by storage key (as returned by `key_for`). Used by the
    /// drain task which stores keys (not hashes) in pending_s3_deletes.
    ///
    /// Separate from `delete(hash)` because the drain reads string keys
    /// from PG; re-parsing them back to `[u8; 32]` would be pointless
    /// indirection.
    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()>;
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
}

impl MemoryChunkBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper: get a count of stored chunks. For dedup-ratio
    /// assertions in C3's PutPath tests.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test helper: corrupt a stored chunk's bytes. For C4's BLAKE3-verify
    /// test — overwrite with garbage, then assert `get_verified` returns Err.
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

    async fn delete(&self, hash: &[u8; 32]) -> anyhow::Result<()> {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);
        Ok(())
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
        self.delete(&hash).await
    }
}

// ============================================================================
// Filesystem backend (dev)
// ============================================================================

/// Filesystem chunk storage. Dev/single-node.
///
/// Layout: `{base_dir}/chunks/{aa}/{blake3-hex}`. The two-level dir
/// structure matches the S3 key scheme (so switching backends doesn't
/// surprise operators) and keeps per-directory file counts reasonable.
pub struct FilesystemChunkBackend {
    base_dir: PathBuf,
}

impl FilesystemChunkBackend {
    /// Create a new filesystem backend. Creates `{base_dir}/chunks/` and
    /// all 256 `{aa}/` subdirectories eagerly — small upfront cost (256
    /// mkdir calls, ~1ms) so `put()` never has to check-then-mkdir on
    /// the hot path.
    pub fn new(base_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let base_dir = base_dir.into().join("chunks");
        std::fs::create_dir_all(&base_dir)?;
        // Precreate all 256 two-char-hex subdirectories.
        for b in 0u8..=255 {
            std::fs::create_dir_all(base_dir.join(format!("{b:02x}")))?;
        }
        Ok(Self { base_dir })
    }

    fn chunk_path(&self, hash: &[u8; 32]) -> PathBuf {
        self.base_dir.join(chunk_key(hash))
    }
}

#[async_trait::async_trait]
impl ChunkBackend for FilesystemChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        let path = self.chunk_path(hash);
        debug!(path = %path.display(), size = data.len(), "FilesystemChunkBackend: storing chunk");

        // Atomic-write pattern: temp + fsync + rename + dir-fsync.
        // If we skip any of these, a crash between
        // put() returning and complete_manifest() committing leaves the
        // manifest claiming a chunk exists that's zero-length or absent.
        //
        // `.tmp` extension gives the rename atomicity (same directory,
        // same filesystem). tempfile::NamedTempFile would put the temp
        // in /tmp — cross-filesystem rename isn't atomic.
        let tmp_path = path.with_extension("tmp");
        {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::File::create(&tmp_path).await?;
            f.write_all(&data).await?;
            f.sync_all().await?;
        }
        tokio::fs::rename(&tmp_path, &path).await?;

        // fsync parent dir. Without this, the rename's directory entry
        // can be lost on power failure even though the file data is
        // durable. The parent is the {aa}/ subdir.
        if let Some(parent) = path.parent() {
            let dir = tokio::fs::File::open(parent).await?;
            dir.sync_all().await?;
        }

        Ok(())
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

    async fn delete(&self, hash: &[u8; 32]) -> anyhow::Result<()> {
        let path = self.chunk_path(hash);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // ENOENT = already gone. Idempotent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        // Relative path (no base_dir) so pending_s3_deletes entries
        // survive a base_dir relocation. delete_by_key rejoins.
        chunk_key(hash)
    }

    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        // key is the relative path from key_for. Rejoin to base_dir.
        // No path-traversal concern: chunk_key output is always
        // `{aa}/{hex}` from a [u8;32], no `..`.
        let path = self.base_dir.join(key);
        match tokio::fs::remove_file(&path).await {
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
        Self {
            client,
            bucket,
            prefix,
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
}

#[async_trait::async_trait]
impl ChunkBackend for S3ChunkBackend {
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        let key = self.s3_key(hash);
        debug!(bucket = %self.bucket, key = %key, size = data.len(), "S3ChunkBackend: uploading");
        metrics::counter!("rio_store_s3_requests_total", "operation" => "put_object").increment(1);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(data.into())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 PutObject failed for {key}: {e}"))?;

        Ok(())
    }

    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        let key = self.s3_key(hash);
        metrics::counter!("rio_store_s3_requests_total", "operation" => "get_object").increment(1);

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(output) => {
                // Collect into Bytes. Chunks are ≤256 KiB — buffering is fine
                // and avoids the caller having to deal with a stream.
                // aws_sdk's ByteStream::collect returns AggregatedBytes;
                // into_bytes() is zero-copy if it was a single segment,
                // one concat if it was chunked transfer (rare for small
                // objects).
                let data = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| anyhow::anyhow!("S3 body read failed for {key}: {e}"))?
                    .into_bytes();
                Ok(Some(data))
            }
            Err(err) => {
                let service_err = err.into_service_error();
                if service_err.is_no_such_key() {
                    // "Not present" — but this is usually data loss if the
                    // manifest claims the chunk exists. Caller decides.
                    Ok(None)
                } else {
                    // Transient error — propagate. Conflating this with
                    // Ok(None) makes every S3 blip look like data loss,
                    // which is the opposite problem.
                    Err(anyhow::anyhow!(
                        "S3 GetObject failed for {key}: {service_err}"
                    ))
                }
            }
        }
    }

    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        // Parallel HeadObject calls, bounded to 16 concurrent. Unbounded
        // would work but be antisocial (100 chunks = 100 simultaneous
        // requests; S3 can handle it but the caller's network might not).
        //
        // NOTE: C3 likely replaces this with a PG query against the
        // `chunks` table — one roundtrip instead of N. This impl is
        // correct but not the final story for production.
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
                        match client.head_object().bucket(bucket).key(&key).send().await {
                            Ok(_) => Ok(true),
                            Err(err) => {
                                let service_err = err.into_service_error();
                                if service_err.is_not_found() {
                                    Ok(false)
                                } else {
                                    Err(anyhow::anyhow!(
                                        "S3 HeadObject failed for {key}: {service_err}"
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

    async fn delete(&self, hash: &[u8; 32]) -> anyhow::Result<()> {
        self.delete_by_key(&self.s3_key(hash)).await
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        // Full S3 key (bucket-relative, includes prefix). The
        // pending_s3_deletes row stores this; drain task passes
        // it to delete_by_key verbatim.
        self.s3_key(hash)
    }

    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_object")
            .increment(1);
        // DeleteObject is idempotent: deleting a non-existent key
        // returns success (no NotFound error). So no special-case
        // for "already gone" — just send and return.
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("S3 DeleteObject failed for {key}: {e}"))?;
        Ok(())
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

    // ------------------------------------------------------------------------
    // Filesystem backend
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn fs_precreates_subdirs() -> anyhow::Result<()> {
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
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".tmp"), "leftover .tmp file: {name}");
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

        let no_prefix = S3ChunkBackend::new(client, "b".into(), "".into());
        assert_eq!(
            no_prefix.s3_key(&HASH_C),
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
        // MatchAny: we don't control which mock matches which call (they
        // race via buffered), but the OUTPUT order from buffered is
        // deterministic. Actually — this is subtle. buffered() preserves
        // the order futures were CREATED, not the order they COMPLETE.
        // The mock framework matches sequentially though... let me use
        // Sequential and rely on buffered's ordering guarantee.
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&r1, &r2, &r3]);
        let backend = make_s3_backend(client);

        let result = backend.exists_batch(&[HASH_A, HASH_B, HASH_C]).await?;
        assert_eq!(result, vec![true, false, true]);
        Ok(())
    }
}
