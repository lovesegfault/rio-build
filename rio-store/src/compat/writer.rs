//! Post-commit writer for the stock-Nix binary-cache object pair.
//!
//! One [`CompatWriter::write_after_commit`] call per committed path:
//! compress the (already hash-verified, in-RAM) NAR per the configured
//! codec, publish `nar/{file-hash}.nar.<ext>`, render the narinfo via
//! [`rio_nix::narinfo::NarInfo::render`] with `FileHash`/`FileSize`
//! populated, publish `{hash-part}.narinfo`, then record the compressed
//! object's digest in `narinfo.compat_file_hash` so GC (P0581) and the
//! reconciler (P0582) can find what has been written.
//!
//! # Write mode
//!
//! `write_mode = sync_after_commit` (the only mode): the write runs
//! inline in the upload handler AFTER the PG commit flipped the path to
//! `'complete'`, and the RPC response waits for it. A failure is logged
//! and metered but never fails the RPC — the rio-native commit already
//! succeeded, `compat_file_hash` stays NULL, and the P0582 reconciler
//! retries later.
//!
//! # Memory bound
//!
//! The compressed NAR is buffered in RAM before upload (the blob API is
//! `Bytes`-shaped, and the object key embeds the hash of the compressed
//! bytes, so a single-pass stream to the final key is impossible
//! anyway). Peak extra RSS per write is ≤ the compressed size ≤
//! `nar_size` ≤ `MAX_NAR_SIZE` (4 GiB), on top of the raw NAR the
//! handler already holds under the `nar_bytes_budget` semaphore — i.e.
//! compat at most doubles a single upload's transient footprint while
//! the write is in flight.

use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::{STORE_DIR, nixbase32};
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::config::CompatCompression;
use crate::metadata;

/// Bucket-relative key of the binary-cache marker object.
const NIX_CACHE_INFO_KEY: &str = "nix-cache-info";

/// Body of `nix-cache-info`. `Priority: 40` ranks the bucket below
/// cache.nixos.org (priority 30) so a client configured with both
/// prefers the public CDN for paths present in either.
const NIX_CACHE_INFO_BODY: &str = "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n";

/// Read-buffer size for the compress-and-hash loop. One chunk of
/// compressor output is hashed and appended per `read()`, with an
/// `.await` between chunks so a multi-GiB NAR never pins a tokio
/// worker for the whole compression pass.
const COMPRESS_READ_BUF: usize = 256 * 1024;

/// How a compat write failed. The caller never propagates this into
/// the RPC result — see [`CompatWriter::write_after_commit`].
#[derive(Debug, thiserror::Error)]
pub enum CompatError {
    /// The compression codec failed (I/O error from the encoder).
    #[error("NAR compression ({codec}) failed: {source}")]
    Compress {
        codec: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// `put_blob` to the compat bucket failed (S3 fault, auth, …).
    #[error("compat object upload failed for {key}: {source}")]
    Upload {
        key: String,
        #[source]
        source: anyhow::Error,
    },

    /// Recording `narinfo.compat_file_hash` failed. The S3 objects are
    /// in place but invisible to GC/reconciler bookkeeping; the
    /// reconciler re-writes them (idempotent — same content, same
    /// keys) once the column update succeeds on a later pass.
    #[error("compat_file_hash update failed: {0}")]
    Db(#[from] metadata::MetadataError),

    /// The narinfo row vanished (GC won a race) between the upload's
    /// commit and the `compat_file_hash` update, so the freshly-written
    /// object pair was rolled back (deleted). Surfaced as an error so
    /// the caller counts it on `rio_store_compat_write_failures_total`
    /// and never emits success metrics for objects that are no longer
    /// supposed to exist. `cleanup_failures > 0` means that many of the
    /// rollback deletes ALSO failed — those objects are orphaned in the
    /// compat bucket (warn-logged with their keys) until an operator
    /// removes them, because without a narinfo row the GC coupling can
    /// never find them.
    #[error(
        "narinfo row for {store_path} vanished before the compat_file_hash update; \
         publication rolled back ({cleanup_failures} cleanup delete(s) failed)"
    )]
    PathVanished {
        store_path: String,
        cleanup_failures: usize,
    },
}

/// Publishes the stock-Nix compat object pair after a path commit.
///
/// Construction is gated on `binary_cache_compat.enabled` in `main.rs`
/// — a disabled deployment never builds one, so the upload handlers'
/// `Option<Arc<CompatWriter>>` is the runtime toggle
/// (`r[store.compat.runtime-toggle]`).
pub struct CompatWriter {
    pool: PgPool,
    /// Where `put_blob` goes: the chunk backend itself when
    /// `binary_cache_compat.bucket` is unset (compat objects live next
    /// to `chunks/` under the same prefix), or a dedicated
    /// S3-standard backend when a separate bucket is configured.
    blob_target: Arc<dyn ChunkBackend>,
    compression: CompatCompression,
    /// `nix-cache-info` bootstrap latch: set once the marker object is
    /// known to exist so steady-state writes skip the existence probe.
    cache_info: tokio::sync::OnceCell<()>,
}

impl CompatWriter {
    /// `blob_target` is the backend whose `put_blob` namespace stock
    /// Nix will read (`{hash}.narinfo`, `nar/…`, `nix-cache-info`).
    pub fn new(
        pool: PgPool,
        blob_target: Arc<dyn ChunkBackend>,
        compression: CompatCompression,
    ) -> Self {
        Self {
            pool,
            blob_target,
            compression,
            cache_info: tokio::sync::OnceCell::new(),
        }
    }

    /// The `Compression:` field value for the configured codec —
    /// identical to the config's serde string by construction.
    fn compression_label(&self) -> &'static str {
        match self.compression {
            CompatCompression::Zstd => "zstd",
            CompatCompression::Xz => "xz",
            CompatCompression::None => "none",
        }
    }

    /// NAR object suffix for the configured codec.
    fn nar_suffix(&self) -> &'static str {
        match self.compression {
            CompatCompression::Zstd => "nar.zst",
            CompatCompression::Xz => "nar.xz",
            CompatCompression::None => "nar",
        }
    }

    /// Best-effort, never-fails post-commit entry point: times the
    /// write, emits the compat metrics, and logs failures instead of
    /// propagating them — the rio-native commit already succeeded and
    /// MUST NOT be failed retroactively by the compat layer
    /// (`r[store.compat.write-after-commit]`). On failure
    /// `narinfo.compat_file_hash` stays NULL and the P0582 reconciler
    /// backfills.
    // r[impl obs.metric.compat]
    pub async fn write_after_commit(&self, info: &ValidatedPathInfo, nar: &Bytes) {
        let start = std::time::Instant::now();
        match self.write(info, nar).await {
            Ok(file_size) => {
                metrics::histogram!("rio_store_compat_write_seconds", "result" => "ok")
                    .record(start.elapsed().as_secs_f64());
                metrics::counter!("rio_store_compat_write_bytes_total").increment(file_size);
                debug!(
                    store_path = %info.store_path,
                    file_size,
                    compression = self.compression_label(),
                    "binary-cache compat objects published"
                );
            }
            Err(e) => {
                metrics::histogram!("rio_store_compat_write_seconds", "result" => "error")
                    .record(start.elapsed().as_secs_f64());
                metrics::counter!("rio_store_compat_write_failures_total").increment(1);
                warn!(
                    store_path = %info.store_path,
                    error = %e,
                    "binary-cache compat write failed; compat_file_hash stays NULL \
                     (reconciler backfills); the upload itself is unaffected"
                );
            }
        }
    }

    /// Publish the compat object pair for one committed path and
    /// record `compat_file_hash`. Returns the compressed object size
    /// in bytes.
    ///
    /// `info` must be the post-commit metadata: server-verified
    /// `nar_hash`/`nar_size` and the final `signatures` (the rio
    /// ed25519 signature produced at sign time goes into the published
    /// `Sig:` lines verbatim — no re-signing here). `nar` is the
    /// hash-verified NAR the handler still holds in RAM.
    pub async fn write(&self, info: &ValidatedPathInfo, nar: &Bytes) -> Result<u64, CompatError> {
        // Bootstrap marker first (best-effort, once per process): a
        // bucket whose first-ever object pair lands before
        // `nix-cache-info` is briefly un-bootstrapped, which stock Nix
        // tolerates for reads but `nix copy --to` of a *different*
        // client would not create for us.
        self.ensure_cache_info().await;

        // ── compress + hash ─────────────────────────────────────────
        let (file_bytes, file_hash) = self.compress_and_hash(info, nar).await?;
        let file_size = file_bytes.len() as u64;

        // ── nar/{file-hash}.nar.<ext> ───────────────────────────────
        // r[impl store.compat.nar-on-put]
        // NAR object first, narinfo second: a reader that can see the
        // narinfo must always be able to fetch the NAR it points at.
        let nar_key = format!(
            "nar/{}.{}",
            nixbase32::encode(&file_hash),
            self.nar_suffix()
        );
        self.blob_target
            .put_blob(&nar_key, file_bytes)
            .await
            .map_err(|source| CompatError::Upload {
                key: nar_key.clone(),
                source,
            })?;

        // ── {hash-part}.narinfo ─────────────────────────────────────
        // r[impl store.compat.narinfo-on-put]
        let narinfo_key = format!("{}.narinfo", info.store_path.hash_part());
        let narinfo_text = render_narinfo(
            info,
            &nar_key,
            self.compression_label(),
            &file_hash,
            file_size,
        );
        self.blob_target
            .put_blob(&narinfo_key, Bytes::from(narinfo_text))
            .await
            .map_err(|source| CompatError::Upload {
                key: narinfo_key.clone(),
                source,
            })?;

        // ── narinfo.compat_file_hash ────────────────────────────────
        // NULL → "not yet written"; the digest of the *compressed*
        // object → "written, and this is the nar/ key to GC with"
        // (P0581 reads it, P0582 skips rows where it is set).
        let updated =
            metadata::set_compat_file_hash(&self.pool, &info.store_path_hash, &file_hash).await?;
        if updated == 0 {
            // The path was GC'd (or never existed under this hash —
            // shouldn't happen post-commit) between the commit and this
            // update. Without a row to carry compat_file_hash the GC
            // coupling can never find these objects, so take them back
            // out — and treat the whole write as FAILED, not published:
            // returning [`CompatError::PathVanished`] makes
            // `write_after_commit` count it on
            // `rio_store_compat_write_failures_total` instead of
            // reporting success bytes/latency for objects we just
            // deleted. Each rollback delete that itself fails is
            // warn-logged with its key (that object is orphaned in the
            // bucket until an operator removes it) and counted into the
            // returned error.
            warn!(
                store_path = %info.store_path,
                "narinfo row vanished before compat_file_hash update; \
                 removing freshly-written compat objects"
            );
            let mut cleanup_failures = 0usize;
            for key in [narinfo_key.as_str(), nar_key.as_str()] {
                if let Err(e) = self.blob_target.delete_blob(key).await {
                    cleanup_failures += 1;
                    warn!(
                        key,
                        error = %e,
                        "compat rollback delete failed; object is orphaned in the compat bucket"
                    );
                }
            }
            return Err(CompatError::PathVanished {
                store_path: info.store_path.to_string(),
                cleanup_failures,
            });
        }

        Ok(file_size)
    }

    /// Compress `nar` per the configured codec while computing
    /// SHA-256 of the compressed stream. For `none` the original
    /// buffer is reused (refcount bump) and the file hash is the NAR
    /// hash the server already verified — no second hashing pass.
    async fn compress_and_hash(
        &self,
        info: &ValidatedPathInfo,
        nar: &Bytes,
    ) -> Result<(Bytes, [u8; 32]), CompatError> {
        use async_compression::tokio::bufread as ac;

        let codec = self.compression_label();
        let mut encoder: Box<dyn tokio::io::AsyncRead + Send + Unpin + '_> = match self.compression
        {
            CompatCompression::Zstd => Box::new(ac::ZstdEncoder::new(&nar[..])),
            CompatCompression::Xz => Box::new(ac::XzEncoder::new(&nar[..])),
            CompatCompression::None => {
                return Ok((nar.clone(), info.nar_hash));
            }
        };

        let mut out = Vec::new();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; COMPRESS_READ_BUF];
        loop {
            let n = encoder
                .read(&mut buf)
                .await
                .map_err(|source| CompatError::Compress { codec, source })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.extend_from_slice(&buf[..n]);
        }
        Ok((Bytes::from(out), hasher.finalize().into()))
    }

    /// Write `nix-cache-info` if it is not already present. Latched on
    /// success so the steady state is zero extra requests; failures are
    /// logged and retried on the next write (the narinfo/NAR pair is
    /// still published — a missing marker degrades read priority, not
    /// substitutability).
    async fn ensure_cache_info(&self) {
        let result = self
            .cache_info
            .get_or_try_init(|| async {
                if self
                    .blob_target
                    .get_blob(NIX_CACHE_INFO_KEY)
                    .await?
                    .is_none()
                {
                    self.blob_target
                        .put_blob(
                            NIX_CACHE_INFO_KEY,
                            Bytes::from_static(NIX_CACHE_INFO_BODY.as_bytes()),
                        )
                        .await?;
                    info!("wrote {NIX_CACHE_INFO_KEY} to the binary-cache compat bucket");
                }
                Ok::<(), anyhow::Error>(())
            })
            .await;
        if let Err(e) = result {
            warn!(error = %e, "nix-cache-info bootstrap failed; will retry on next compat write");
        }
    }
}

/// Build the stock-Nix narinfo text for a committed path.
///
/// Field mapping from the store's metadata: `References:` and
/// `Deriver:` carry store-path *basenames* (the text format's
/// convention; the signature fingerprint inside `Sig:` was computed
/// over full paths at sign time and is copied verbatim), `NarHash`/
/// `FileHash` are `sha256:<nixbase32>`.
fn render_narinfo(
    info: &ValidatedPathInfo,
    nar_url: &str,
    compression: &str,
    file_hash: &[u8; 32],
    file_size: u64,
) -> String {
    NarInfo {
        store_path: info.store_path.as_str().to_owned(),
        url: nar_url.to_owned(),
        compression: compression.to_owned(),
        nar_hash: format!("sha256:{}", nixbase32::encode(&info.nar_hash)),
        nar_size: info.nar_size,
        references: info
            .references
            .iter()
            .map(|r| basename(r.as_str()))
            .collect(),
        deriver: info.deriver.as_ref().map(|d| basename(d.as_str())),
        sigs: info.signatures.clone(),
        ca: info.content_address.clone(),
        file_hash: Some(format!("sha256:{}", nixbase32::encode(file_hash))),
        file_size: Some(file_size),
    }
    .render()
}

/// `/nix/store/{hash}-{name}` → `{hash}-{name}`. Falls back to the
/// input for anything that doesn't carry the store dir prefix (cannot
/// happen for a `StorePath`, but total beats panicking in a
/// best-effort writer).
fn basename(path: &str) -> String {
    path.strip_prefix(STORE_DIR)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or(path)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::{make_nar, make_path_info_for_nar, test_store_path};

    /// The rendered narinfo for a signed path round-trips through the
    /// stock parser with every field the compat bucket needs: URL ↔
    /// FileHash agreement, basename references/deriver, verbatim Sig
    /// lines, and the `sha256:<nixbase32>` hash spelling.
    // r[verify store.compat.narinfo-on-put]
    #[test]
    fn render_narinfo_round_trips_with_stock_parser() {
        let path = test_store_path("compat-render");
        let (nar, _) = make_nar(b"compat narinfo body");
        let mut info = make_path_info_for_nar(&path, &nar);
        info.references = vec![
            rio_nix::store_path::StorePath::parse(&test_store_path("dep-a")).unwrap(),
            rio_nix::store_path::StorePath::parse(&test_store_path("dep-b")).unwrap(),
        ];
        info.deriver = Some(
            rio_nix::store_path::StorePath::parse(&test_store_path("compat-render.drv")).unwrap(),
        );
        info.signatures = vec!["rio-cluster-1:c2ln".to_owned()];
        info.content_address = Some("fixed:r:sha256:0000".to_owned());

        let file_hash = [7u8; 32];
        let url = format!("nar/{}.nar.zst", nixbase32::encode(&file_hash));
        let text = render_narinfo(&info, &url, "zstd", &file_hash, 1234);

        let parsed =
            NarInfo::parse(&text).expect("compat narinfo must parse with the stock parser");
        assert_eq!(parsed.store_path, info.store_path.as_str());
        assert_eq!(parsed.url, url);
        assert_eq!(parsed.compression, "zstd");
        assert_eq!(
            parsed.nar_hash,
            format!("sha256:{}", nixbase32::encode(&info.nar_hash))
        );
        assert_eq!(parsed.nar_size, info.nar_size);
        assert_eq!(
            parsed.file_hash.as_deref(),
            Some(format!("sha256:{}", nixbase32::encode(&file_hash)).as_str())
        );
        assert_eq!(parsed.file_size, Some(1234));
        // Basenames, not full paths, in the text format.
        assert_eq!(parsed.references.len(), 2);
        for r in &parsed.references {
            assert!(!r.starts_with('/'), "reference {r:?} must be a basename");
        }
        assert!(
            parsed
                .deriver
                .as_deref()
                .is_some_and(|d| !d.starts_with('/')),
            "deriver must be a basename"
        );
        assert_eq!(parsed.sigs, info.signatures);
        assert_eq!(parsed.ca, info.content_address);
    }

    /// Codec → (label, suffix) table stays in sync with the config
    /// enum: the `Compression:` field and the URL extension must agree
    /// or stock Nix downloads an object it can't decode.
    /// (`#[tokio::test]` because `connect_lazy` needs a reactor.)
    #[tokio::test]
    async fn compression_label_matches_suffix() {
        let pool = sqlx::PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let backend: Arc<dyn ChunkBackend> = Arc::new(crate::backend::MemoryChunkBackend::new());
        for (codec, label, suffix) in [
            (CompatCompression::Zstd, "zstd", "nar.zst"),
            (CompatCompression::Xz, "xz", "nar.xz"),
            (CompatCompression::None, "none", "nar"),
        ] {
            let w = CompatWriter::new(pool.clone(), Arc::clone(&backend), codec);
            assert_eq!(w.compression_label(), label);
            assert_eq!(w.nar_suffix(), suffix);
        }
    }

    /// zstd round-trip: compress_and_hash output decompresses back to
    /// the input and the returned digest is the digest of the
    /// *compressed* bytes (what FileHash/compat_file_hash carry).
    #[tokio::test]
    async fn compress_and_hash_zstd_round_trips() {
        let pool = sqlx::PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let backend: Arc<dyn ChunkBackend> = Arc::new(crate::backend::MemoryChunkBackend::new());
        let w = CompatWriter::new(pool, backend, CompatCompression::Zstd);

        let payload = vec![0xABu8; 64 * 1024];
        let (nar, _) = make_nar(&payload);
        let info = make_path_info_for_nar(&test_store_path("compat-zstd"), &nar);
        let nar = Bytes::from(nar);

        let (compressed, digest) = w.compress_and_hash(&info, &nar).await.unwrap();
        assert_ne!(compressed, nar, "zstd output must differ from input");
        let expected: [u8; 32] = Sha256::digest(&compressed).into();
        assert_eq!(digest, expected, "digest must cover the compressed bytes");

        // Decode with the same async-compression backend the
        // substituter's production decoder uses.
        let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .await
            .expect("valid zstd frame");
        assert_eq!(Bytes::from(decompressed), nar);
    }

    /// `compression = none`: the published object IS the NAR — same
    /// bytes (zero-copy) and the file hash equals the verified
    /// nar_hash, so no second hashing pass happens.
    #[tokio::test]
    async fn compress_and_hash_none_is_identity() {
        let pool = sqlx::PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let backend: Arc<dyn ChunkBackend> = Arc::new(crate::backend::MemoryChunkBackend::new());
        let w = CompatWriter::new(pool, backend, CompatCompression::None);

        let (nar, nar_hash) = make_nar(b"identity body");
        let info = make_path_info_for_nar(&test_store_path("compat-none"), &nar);
        let nar = Bytes::from(nar);

        let (out, digest) = w.compress_and_hash(&info, &nar).await.unwrap();
        assert_eq!(out, nar);
        assert_eq!(digest, nar_hash);
    }

    #[test]
    fn basename_strips_store_dir() {
        assert_eq!(
            basename("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo-1.0"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo-1.0"
        );
        // Defensive total-ness for non-store-dir input.
        assert_eq!(basename("relative-thing"), "relative-thing");
    }

    /// GC-race rollback: when the narinfo row is gone by the time the
    /// `compat_file_hash` UPDATE runs (0 rows), `write()` must delete
    /// the freshly-published pair and return `PathVanished` — never
    /// `Ok` — so the caller's success metrics can't describe objects
    /// that no longer exist. Uses a real (empty) PG so the UPDATE
    /// genuinely affects 0 rows.
    #[tokio::test]
    async fn write_rolls_back_when_narinfo_row_is_gone() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let backend = Arc::new(crate::backend::MemoryChunkBackend::new());
        let w = CompatWriter::new(
            db.pool.clone(),
            Arc::clone(&backend) as Arc<dyn ChunkBackend>,
            CompatCompression::Zstd,
        );

        let path = test_store_path("compat-vanished");
        let (nar, _) = make_nar(b"row vanished before bookkeeping");
        let info = make_path_info_for_nar(&path, &nar);
        let nar = Bytes::from(nar);

        let err = w
            .write(&info, &nar)
            .await
            .expect_err("0-row compat_file_hash update must not be a success");
        assert!(
            matches!(
                &err,
                CompatError::PathVanished {
                    cleanup_failures: 0,
                    ..
                }
            ),
            "expected PathVanished with clean rollback, got {err:?}"
        );

        // Both objects were rolled back; only the nix-cache-info
        // bootstrap marker remains. The nar key is recomputed via the
        // same (deterministic) compression pass write() used.
        let narinfo_key = format!("{}.narinfo", info.store_path.hash_part());
        assert!(
            backend.get_blob(&narinfo_key).await.unwrap().is_none(),
            "rolled-back narinfo object must be deleted"
        );
        let (_, file_hash) = w.compress_and_hash(&info, &nar).await.unwrap();
        let nar_key = format!("nar/{}.nar.zst", nixbase32::encode(&file_hash));
        assert!(
            backend.get_blob(&nar_key).await.unwrap().is_none(),
            "rolled-back NAR object must be deleted"
        );
        assert!(
            backend.get_blob("nix-cache-info").await.unwrap().is_some(),
            "bootstrap marker is independent of the rolled-back path"
        );
    }
}
