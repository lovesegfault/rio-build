//! Write-ahead NAR ingest core, shared by PutPath/PutPathBatch (gRPC)
//! and `Substituter` (upstream binary-cache fetch).
//!
//! Both flows walk the same state machine:
//!
//! 1. [`claim_placeholder`] — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//! 2. caller acquires NAR bytes (gRPC stream / HTTP download)
//! 3. [`persist_nar`] — branch on size: inline (`manifests.inline_blob`)
//!    or chunked (`cas::put_chunked`)
//! 4. on any error after step 1: [`abort_placeholder`]
//!
//! Before this module existed, `Substituter::ingest` open-coded steps
//! 1/3/4 and had already drifted from `grpc/put_path/common.rs` once
//! (substitution lacked the `chunk_dedup_ratio` gauge). Factoring here
//! keeps the write-ahead invariants in one place; gRPC/substitute keep
//! their transport-specific bits (HMAC, sig_mode, error mapping) in
//! thin wrappers.

use std::sync::Arc;

use bytes::Bytes;
use sqlx::PgPool;
use tracing::{debug, warn};
use uuid::Uuid;

use rio_common::limits::nar_size_cap;
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::cas;
use crate::gc::orphan::ReapBy;
use crate::metadata::{self, MetadataError};
use crate::substitute::SUBSTITUTE_STALE_THRESHOLD;

/// Why [`AdmittedNar::admit`] refused a NAR. The transport layers map
/// this to their own error domains — the gRPC mapping
/// (`admit_denial_status`) preserves the wire codes the pre-witness
/// gates used (`InvalidArgument` for malformed shapes,
/// `PermissionDenied` for a path that is not the text content-address
/// of its bytes); substitution maps to `SubstituteError::DrvNotTextCa`
/// / `TooLarge`.
#[derive(Debug)]
pub enum AdmitDenial {
    /// NAR exceeds the path's class cap
    /// (`nar_size_cap(is_derivation)`). In the gRPC flows the
    /// streaming gates (`accumulate_chunk` / `apply_trailer`) fire
    /// first, so this arm is defense-in-depth there; substitution
    /// reaches it only if the declared/decompressed gates were
    /// bypassed (they are not — same cap, checked earlier).
    OverCap {
        limit: u64,
        actual: u64,
        is_derivation: bool,
    },
    /// `.drv` NAR is not a single regular-file NAR.
    NotSingleFile(String),
    /// `.drv` content length does not fit this platform's `usize`.
    LenOverflow,
    /// The text content-address could not be derived (bad name/refs).
    TextCaDerive(String),
    /// `.drv` path is not the text content-address of its bytes with
    /// the declared references.
    TextCaMismatch { claimed: String, derived: String },
}

impl std::fmt::Display for AdmitDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitDenial::OverCap {
                limit,
                actual,
                is_derivation,
            } => write!(
                f,
                "NAR of {actual} bytes exceeds the {} class cap ({limit} bytes)",
                if *is_derivation { ".drv" } else { "general" }
            ),
            AdmitDenial::NotSingleFile(e) => {
                write!(f, "a .drv upload must be a single regular-file NAR: {e}")
            }
            AdmitDenial::LenOverflow => {
                write!(f, ".drv content length does not fit this platform")
            }
            AdmitDenial::TextCaDerive(e) => {
                write!(f, "cannot derive the text content-address: {e}")
            }
            AdmitDenial::TextCaMismatch { claimed, derived } => write!(
                f,
                ".drv path {claimed} is not the text content-address of the uploaded bytes \
                 with the declared references (derived {derived})"
            ),
        }
    }
}

/// The NAR-residency admission witness: proof that a byte buffer has
/// passed every invariant the store demands of bytes it makes
/// resident. Private fields + a single fallible constructor mean a
/// persistence primitive that takes `AdmittedNar` CANNOT be handed
/// unchecked bytes — a new ingest route that skips admission is a
/// compile error, not a review hope.
///
/// Invariants established by [`Self::admit`]:
///
/// 1. **Class cap** — `len ≤ nar_size_cap(is_derivation)` (16 MiB for
///    `.drv` paths, 4 GiB otherwise). An over-cap resident `.drv`
///    would be permanently unfetchable by every capped consumer.
/// 2. **Text-CA binding** (`.drv` paths only) — the claimed path
///    equals `make_text(name, sha256(file bytes), references)`, so a
///    registered `.drv` path is always the unique preimage of its
///    bytes. This is the REGISTERED FAIL-CLOSED DIVERGENCE from
///    CppNix described at #r(store.put.drv-text-ca): the oracle's
///    `registerValidPath` (`local-store.cc:680-716`) accepts
///    source-CA byte-copies of derivation files; rio rejects them
///    because the modulo cache (M_068), the gateway derivation
///    cache, and the deriver-proof claims chain all key on
///    "registered `.drv` path ⇒ unique text-CA preimage".
/// 3. **Single preimage extraction** — for `.drv` paths the
///    single-file contents are located ONCE here
///    (`single_file_nar_content_range`) and retained, so no
///    downstream consumer re-parses the NAR (the pre-witness routes
///    each ran their own `extract_single_file`, and the one route
///    that skipped the binding could feed `populate_on_ingest`
///    forged bytes).
///
/// Non-`.drv` paths carry no preimage and no text-CA obligation; the
/// witness still pins the class cap.
// r[impl store.put.drv-text-ca+3]
pub struct AdmittedNar {
    nar_data: Vec<u8>,
    /// `.drv` paths: the extracted, text-CA-bound single-file
    /// contents. `None` for non-derivation paths.
    drv_preimage: Option<Vec<u8>>,
}

impl std::fmt::Debug for AdmittedNar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedNar")
            .field("nar_len", &self.nar_data.len())
            .field(
                "drv_preimage_len",
                &self.drv_preimage.as_ref().map(Vec::len),
            )
            .finish()
    }
}

impl AdmittedNar {
    /// Admit `nar_data` for residency at `info.store_path`. The ONLY
    /// constructor — see the type docs for the invariants.
    pub fn admit(info: &ValidatedPathInfo, nar_data: Vec<u8>) -> Result<Self, AdmitDenial> {
        let is_derivation = info.store_path.is_derivation();
        let limit = nar_size_cap(is_derivation);
        if nar_data.len() as u64 > limit {
            return Err(AdmitDenial::OverCap {
                limit,
                actual: nar_data.len() as u64,
                is_derivation,
            });
        }
        let drv_preimage = if is_derivation {
            let (off, len) =
                single_file_nar_content_range(&nar_data).map_err(AdmitDenial::NotSingleFile)?;
            let len = usize::try_from(len).map_err(|_| AdmitDenial::LenOverflow)?;
            let file_bytes = &nar_data[off..off + len];
            use std::io::Write as _;
            let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
            w.write_all(file_bytes)
                .map_err(|e| AdmitDenial::TextCaDerive(format!(".drv hash: {e}")))?;
            let hash = w.finish();
            let expected = rio_nix::store_path::StorePath::make_text(
                info.store_path.name(),
                &hash,
                &info.references,
            )
            .map_err(|e| AdmitDenial::TextCaDerive(e.to_string()))?;
            if expected != info.store_path {
                return Err(AdmitDenial::TextCaMismatch {
                    claimed: info.store_path.as_str().to_string(),
                    derived: expected.as_str().to_string(),
                });
            }
            Some(file_bytes.to_vec())
        } else {
            None
        };
        Ok(AdmittedNar {
            nar_data,
            drv_preimage,
        })
    }

    /// Admitted byte length (drives the inline-vs-chunked branch).
    pub fn len(&self) -> usize {
        self.nar_data.len()
    }

    /// True iff the admitted NAR is empty.
    pub fn is_empty(&self) -> bool {
        self.nar_data.is_empty()
    }

    /// The admitted NAR bytes. Reading is unrestricted — the hazard
    /// the witness exists for is PERSISTING unadmitted bytes, and the
    /// persistence primitives take the witness itself.
    pub fn bytes(&self) -> &[u8] {
        &self.nar_data
    }

    /// Take the text-CA-bound `.drv` preimage (for
    /// `populate_on_ingest`), leaving `None`. Callers take it BEFORE
    /// handing the witness to a persistence primitive — population
    /// runs after persist, but the NAR is consumed by then.
    pub fn take_drv_preimage(&mut self) -> Option<Vec<u8>> {
        self.drv_preimage.take()
    }

    /// Borrow the `.drv` preimage without consuming it.
    pub fn drv_preimage(&self) -> Option<&[u8]> {
        self.drv_preimage.as_deref()
    }

    /// Consume the witness into its raw bytes — for the persistence
    /// layer's terminal hand-off (inline blob / chunker input) only.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.nar_data
    }
}

/// Locate the contents of a single-regular-file NAR inside `nar`,
/// returning `(offset, length)` of the file bytes without copying them
/// and without the general-purpose parser's per-file size cap.
///
/// The framing is validated strictly: magic, `regular` type, zero
/// padding, a closing parenthesis, and no trailing bytes. Anything
/// else — directories, symlinks, truncation — is an error.
///
/// The `executable` marker is rejected: the flat content hash is over
/// the file bytes only, so the executable and non-executable variants
/// of the same content would otherwise verify against the same
/// content-derived path, and every legitimate flat producer (CppNix,
/// the builder's flat shape rules in `verify_fod_hashes` /
/// `finalize_floating_ca`) refuses to mint a flat output that is
/// executable in the first place.
pub(crate) fn single_file_nar_content_range(nar: &[u8]) -> Result<(usize, u64), String> {
    fn read_u64(nar: &[u8], pos: &mut usize) -> Result<u64, String> {
        let end = pos
            .checked_add(8)
            .ok_or_else(|| "length field overflows".to_string())?;
        let bytes = nar
            .get(*pos..end)
            .ok_or_else(|| "truncated NAR: expected a length field".to_string())?;
        *pos = end;
        Ok(u64::from_le_bytes(bytes.try_into().expect("8-byte slice")))
    }
    fn read_token<'a>(nar: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
        let len = read_u64(nar, pos)?;
        let len = usize::try_from(len).map_err(|_| "token length overflows".to_string())?;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| "token length overflows".to_string())?;
        let tok = nar
            .get(*pos..end)
            .ok_or_else(|| "truncated NAR: token".to_string())?;
        *pos = end;
        let pad = (8 - len % 8) % 8;
        let pad_end = pos
            .checked_add(pad)
            .ok_or_else(|| "padding overflows".to_string())?;
        let padding = nar
            .get(*pos..pad_end)
            .ok_or_else(|| "truncated NAR: token padding".to_string())?;
        if padding.iter().any(|b| *b != 0) {
            return Err("non-zero token padding".to_string());
        }
        *pos = pad_end;
        Ok(tok)
    }
    fn expect(nar: &[u8], pos: &mut usize, want: &[u8]) -> Result<(), String> {
        let tok = read_token(nar, pos)?;
        if tok != want {
            return Err(format!(
                "expected `{}`, found `{}`",
                std::str::from_utf8(want).unwrap_or("<non-utf8>"),
                std::str::from_utf8(tok).unwrap_or("<non-utf8>")
            ));
        }
        Ok(())
    }

    let mut pos = 0usize;
    expect(nar, &mut pos, b"nix-archive-1")?;
    expect(nar, &mut pos, b"(")?;
    expect(nar, &mut pos, b"type")?;
    let ty = read_token(nar, &mut pos)?;
    if ty != b"regular" {
        return Err(format!(
            "not a regular file (type `{}`)",
            std::str::from_utf8(ty).unwrap_or("<non-utf8>")
        ));
    }
    let tok = read_token(nar, &mut pos)?;
    if tok == b"executable" {
        return Err(
            "executable single-file NARs are not valid flat content-addressed outputs \
             (the flat hash ignores the bit; CppNix rejects this shape)"
                .to_string(),
        );
    }
    if tok != b"contents" {
        return Err(format!(
            "expected `contents`, found `{}`",
            std::str::from_utf8(tok).unwrap_or("<non-utf8>")
        ));
    }
    let len = read_u64(nar, &mut pos)?;
    let len_usize = usize::try_from(len).map_err(|_| "content length overflows".to_string())?;
    let offset = pos;
    let content_end = offset
        .checked_add(len_usize)
        .ok_or_else(|| "content length overflows".to_string())?;
    if nar.len() < content_end {
        return Err("truncated NAR: contents".to_string());
    }
    let pad = (8 - len_usize % 8) % 8;
    let pad_end = content_end
        .checked_add(pad)
        .ok_or_else(|| "content padding overflows".to_string())?;
    let padding = nar
        .get(content_end..pad_end)
        .ok_or_else(|| "truncated NAR: content padding".to_string())?;
    if padding.iter().any(|b| *b != 0) {
        return Err("non-zero content padding".to_string());
    }
    let mut tail = pad_end;
    expect(nar, &mut tail, b")")?;
    if tail != nar.len() {
        return Err("trailing bytes after the single-file NAR".to_string());
    }
    Ok((offset, len))
}

/// Result of [`claim_placeholder`].
pub enum PlaceholderClaim {
    /// Path is already `status='complete'`. Caller returns
    /// `created=false` (gRPC) or fetches the existing row (substitute)
    /// without writing anything.
    AlreadyComplete,
    /// We inserted (or stale-reclaimed-then-inserted) the
    /// `status='uploading'` placeholder. Caller now OWNS it and MUST
    /// [`abort_placeholder`] on any error path. The carried [`Uuid`] is
    /// the `manifests.claim_id` ownership token (M_052) — every
    /// owner-side cleanup passes it to `reap_one(ReapBy::Claim(id))`
    /// so a late-firing cleanup cannot reap a fresh re-upload at the
    /// same `store_path_hash`.
    Owned(Uuid),
    /// Another uploader holds a live (heartbeating) placeholder. Caller
    /// returns `aborted` so the client retries.
    Concurrent,
}

/// Per-caller observability hooks. The two ingest entry points emit
/// different metric names for the same events (stale-reclaim on the
/// PutPath hot path vs the substitution hot path are tracked
/// separately because they indicate different upstream-health
/// problems).
#[derive(Clone, Copy)]
pub struct IngestHooks {
    /// `metrics::counter!` name incremented when a stale `'uploading'`
    /// placeholder is reaped on the hot path. e.g.
    /// `rio_store_putpath_stale_reclaimed_total`.
    pub stale_reclaimed_metric: &'static str,
    /// Prefix for `warn!`/`debug!` log lines (e.g. `"PutPath"`,
    /// `"substitute"`).
    pub ctx_label: &'static str,
}

// r[impl store.put.idempotent]
// r[impl store.put.stale-reclaim]
/// Idempotency check + `status='uploading'` placeholder insert +
/// hot-path stale-reclaim. The shared step-1 of the write-ahead flow.
///
/// Flow:
/// 1. `check_manifest_complete` → [`PlaceholderClaim::AlreadyComplete`]
/// 2. `insert_manifest_uploading` → if inserted: [`PlaceholderClaim::Owned`]
/// 3. ON CONFLICT no-op: try `reap_one` with the stale threshold
///    (I-207 — a fetcher that died mid-upload leaves a placeholder
///    the orphan scanner won't reap for 15min, but the scheduler
///    retries within seconds). If reap succeeded, re-insert.
/// 4. Still not inserted → [`PlaceholderClaim::Concurrent`] (live
///    uploader's heartbeat keeps `updated_at` fresh, so reap_one's
///    threshold check protected it).
///
/// Per-caller metrics (`exists` / `concurrent_upload` on
/// `rio_store_put_path_total`) are NOT emitted here — that's a
/// PutPath-specific counter. Only the stale-reclaim counter (whose
/// name the caller supplies) is emitted.
pub async fn claim_placeholder(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    store_path_hash: &[u8],
    store_path: &str,
    refs: &[String],
    hooks: IngestHooks,
) -> Result<PlaceholderClaim, MetadataError> {
    if metadata::check_manifest_complete(pool, store_path_hash).await? {
        return Ok(PlaceholderClaim::AlreadyComplete);
    }

    // STRUCTURAL: insert_manifest_uploading takes references and writes
    // them into the placeholder narinfo. Mark's CTE walks them from
    // commit → the closure is GC-protected without holding a session
    // lock for the full upload.
    let mut claim =
        metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs).await?;

    if claim.is_none() {
        // r[impl store.substitute.stale-reclaim]
        // I-040: chunk-aware reap (reads manifest_data.chunk_list and
        // decrements refcounts) — the inline-only delete leaks chunk
        // refcounts when the stale placeholder is from an interrupted
        // `cas::put_chunked`.
        let threshold = SUBSTITUTE_STALE_THRESHOLD.as_secs() as i64;
        match crate::gc::orphan::reap_one(
            pool,
            store_path_hash,
            ReapBy::Stale { secs: threshold },
            chunk_backend,
        )
        .await
        {
            Ok(true) => {
                warn!(
                    %store_path,
                    threshold = ?SUBSTITUTE_STALE_THRESHOLD,
                    "{}: stale 'uploading' placeholder — reclaimed", hooks.ctx_label,
                );
                metrics::counter!(hooks.stale_reclaimed_metric).increment(1);
                // Propagate (?) — after reap_one Ok(true) the
                // placeholder is gone; collapsing Err into the
                // Concurrent path here would silently swallow a DB
                // failure with no log (asymmetric with line 101).
                claim =
                    metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs)
                        .await?;
            }
            Ok(false) => {} // not stale → live concurrent uploader
            Err(e) => warn!(error = %e,
                "{}: stale-reclaim failed (proceeding to concurrent-abort)", hooks.ctx_label),
        }
    }

    match claim {
        Some(id) => Ok(PlaceholderClaim::Owned(id)),
        None => Ok(PlaceholderClaim::Concurrent),
    }
}

/// How [`persist_nar`] failed. The caller maps this to its own error
/// domain (`tonic::Status` for gRPC, `SubstituteError` for
/// substitution) and decides whether to [`abort_placeholder`]: the
/// chunked path already rolled back internally.
#[derive(Debug)]
pub enum PersistError {
    /// `cas::put_chunked` failed. Its internal rollback
    /// (`delete_manifest_chunked_uploading`) already ran; the
    /// placeholder is GONE (best-effort). Caller's `abort_placeholder`
    /// is a harmless no-op but not required.
    Chunked(anyhow::Error),
    /// `complete_manifest_inline` failed. Caller still OWNS the
    /// placeholder and MUST `abort_placeholder`.
    Inline(MetadataError),
}

/// Persist an ADMITTED NAR for ONE output — the [`AdmittedNar`]
/// witness is the only accepted byte carrier, so every persistence
/// route has passed the class cap and (for `.drv`) the text-CA
/// binding by construction. Branches on
/// `nar.len()` vs [`cas::INLINE_THRESHOLD`]: inline goes to
/// `manifests.inline_blob` in one tx; chunked goes through
/// [`cas::put_chunked`] (FastCDC + S3 + refcounts, own write-ahead +
/// rollback).
///
/// Caller must hold a [`PlaceholderClaim::Owned`] for
/// `info.store_path_hash`. Emits `rio_store_chunk_dedup_ratio` on the
/// chunked branch.
pub async fn persist_nar(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    info: &ValidatedPathInfo,
    claim: Uuid,
    nar: AdmittedNar,
    chunk_upload_max_concurrent: usize,
    hooks: IngestHooks,
) -> Result<(), PersistError> {
    if let Some(backend) = cas::should_chunk(chunk_backend, nar.len()) {
        let stats = cas::put_chunked(
            pool,
            backend,
            info,
            claim,
            &nar,
            chunk_upload_max_concurrent,
        )
        .await
        .map_err(PersistError::Chunked)?;
        debug!(
            store_path = %info.store_path.as_str(),
            total_chunks = stats.total_chunks,
            deduped = stats.deduped_chunks,
            ratio = stats.dedup_ratio(),
            "{}: chunked upload completed", hooks.ctx_label,
        );
        metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
    } else {
        metadata::complete_manifest_inline(pool, info, claim, Bytes::from(nar.into_bytes()))
            .await
            .map_err(PersistError::Inline)?;
        debug!(store_path = %info.store_path.as_str(), "{}: inline upload completed", hooks.ctx_label);
    }
    Ok(())
}

/// Heartbeat cadence for [`PlaceholderGuard`]. Matches
/// `cas::HEARTBEAT_TIME_INTERVAL` (the chunk-upload heartbeat) and is
/// ≪ `SUBSTITUTE_STALE_THRESHOLD` (300s), so a live owner survives ≥9
/// missed heartbeats before stale-reclaim takes it.
const PLACEHOLDER_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// RAII owner of an `'uploading'` placeholder: heartbeats while held,
/// reaps on drop. See [`spawn_placeholder_guard`].
// r[impl store.put.drop-cleanup+2]
pub(crate) struct PlaceholderGuard {
    heartbeat: tokio::task::JoinHandle<()>,
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    store_path_hash: Vec<u8>,
    /// `r[store.put.placeholder-claim+2]`: ownership token from
    /// [`PlaceholderClaim::Owned`]. The drop-path reap filters
    /// `claim_id = $claim` so it's a no-op if our row was already
    /// reaped (orphan scanner / `cas::put_chunked` rollback) and a
    /// fresh re-upload now holds the slot.
    claim: Uuid,
    defused: bool,
}

impl PlaceholderGuard {
    /// Stop heartbeating and skip the drop-path reap. Call after the
    /// placeholder has been flipped to `'complete'` (or explicitly
    /// `abort_upload`ed — the reap would be a no-op, but the spawn is
    /// wasted).
    pub(crate) fn defuse(mut self) {
        self.defused = true;
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if self.defused {
            return;
        }
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.take();
        let store_path_hash = std::mem::take(&mut self.store_path_hash);
        let claim = self.claim;
        rio_common::task::spawn_monitored("put-path-placeholder-reap", async move {
            if let Err(e) = crate::gc::orphan::reap_one(
                &pool,
                &store_path_hash,
                ReapBy::Claim(claim),
                chunk_backend.as_ref(),
            )
            .await
            {
                warn!(
                    store_path_hash = %hex::encode(&store_path_hash),
                    error = %e,
                    "drop-path placeholder cleanup failed; orphan scanner will reclaim",
                );
            }
        });
    }
}

// r[impl store.put.drop-cleanup+2]
/// Drop-safety + liveness for a [`PlaceholderClaim::Owned`] placeholder.
/// Returns a [`PlaceholderGuard`] that:
///
/// - **heartbeats** `manifests.updated_at` every 30s while held, so
///   `r[store.put.stale-reclaim]`'s `reap_one(SUBSTITUTE_STALE_
///   THRESHOLD)` never reaps a live owner during a long ingest
///   (6 GB/50 Mbps ≈ 16 min);
/// - **on Drop** (owning future dropped — tonic aborts on client
///   RST_STREAM; a `try_substitute` caller times out — without an
///   explicit [`abort_placeholder`] or `'complete'` flip), spawns
///   `reap_one`. `reap_one` filters `status='uploading'` so firing after
///   an explicit abort/complete is a harmless no-op.
///
/// Call [`PlaceholderGuard::defuse`] on success.
///
/// Shared by `PutPath` and `Substituter::try_upstream`; both run inline
/// in a request handler future and so share the same drop hazard.
pub fn spawn_placeholder_guard(
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    store_path_hash: Vec<u8>,
    claim: Uuid,
) -> PlaceholderGuard {
    let heartbeat = {
        let pool = pool.clone();
        let hash = store_path_hash.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PLACEHOLDER_HEARTBEAT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick fires immediately; skip it
            loop {
                tick.tick().await;
                cas::heartbeat_uploading(&pool, &hash, claim).await;
            }
        })
    };
    PlaceholderGuard {
        heartbeat,
        pool,
        chunk_backend,
        store_path_hash,
        claim,
        defused: false,
    }
}

/// Best-effort placeholder cleanup after a failed ingest. Chunk-aware
/// (reads `manifest_data.chunk_list` and decrements refcounts).
/// `claim` is the ownership token from [`PlaceholderClaim::Owned`] —
/// `reap_one` filters `claim_id = $claim` so this is a no-op if the
/// row was already reaped (orphan scanner / `cas::put_chunked` rollback)
/// AND a fresh re-upload now holds the slot.
pub async fn abort_placeholder(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    store_path_hash: &[u8],
    claim: Uuid,
) {
    if let Err(e) =
        crate::gc::orphan::reap_one(pool, store_path_hash, ReapBy::Claim(claim), chunk_backend)
            .await
    {
        warn!(
            store_path_hash = %hex::encode(store_path_hash),
            error = %e,
            "abort_placeholder: cleanup failed; orphan scanner will reclaim",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info_for_nar, test_store_path};

    /// Canonical single-file NAR of `contents`.
    fn nar_of_file(contents: &[u8]) -> Vec<u8> {
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: contents.to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        nar
    }

    /// Genuine text-CA `.drv` info for `drv_text` (no references).
    fn genuine_drv_info(drv_text: &[u8]) -> (ValidatedPathInfo, Vec<u8>) {
        use std::io::Write as _;
        let nar = nar_of_file(drv_text);
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(drv_text).unwrap();
        let path = rio_nix::store_path::StorePath::make_text("a.drv", &w.finish(), &[]).unwrap();
        let info = make_path_info_for_nar(path.as_str(), &nar);
        (info, nar)
    }

    // r[verify store.put.drv-text-ca+3]
    /// The witness's three invariants at the constructor: genuine
    /// text-CA `.drv` admitted with its preimage retained; forged
    /// path refused; non-single-file `.drv` refused; over-cap `.drv`
    /// refused at the CLASS bound (16 MiB, not 4 GiB); non-`.drv`
    /// admitted with no preimage.
    #[test]
    fn admit_enforces_class_cap_and_text_ca() {
        let drv_text = br#"Derive([("out","/nix/store/cccccccccccccccccccccccccccccccc-l","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let (info, nar) = genuine_drv_info(drv_text);
        let mut admitted = AdmittedNar::admit(&info, nar.clone()).expect("genuine .drv admitted");
        assert_eq!(
            admitted.drv_preimage(),
            Some(drv_text.as_slice()),
            "preimage extracted once at admission"
        );
        assert_eq!(admitted.bytes(), &nar[..]);
        let taken = admitted.take_drv_preimage().unwrap();
        assert_eq!(taken, drv_text.to_vec());
        assert!(admitted.take_drv_preimage().is_none(), "take is one-shot");

        // Forged: same bytes at a different (non-preimage) .drv path.
        let forged_info = make_path_info_for_nar(&test_store_path("forged.drv"), &nar);
        let err = AdmittedNar::admit(&forged_info, nar.clone()).unwrap_err();
        assert!(
            matches!(err, AdmitDenial::TextCaMismatch { .. }),
            "got {err:?}"
        );

        // Not a single-file NAR.
        let dir_node = rio_nix::nar::NarNode::Directory { entries: vec![] };
        let mut dir_nar = Vec::new();
        rio_nix::nar::serialize(&mut dir_nar, &dir_node).unwrap();
        let dir_info = make_path_info_for_nar(&test_store_path("dir.drv"), &dir_nar);
        assert!(matches!(
            AdmittedNar::admit(&dir_info, dir_nar).unwrap_err(),
            AdmitDenial::NotSingleFile(_)
        ));

        // Over the .drv CLASS cap (16 MiB + 1 file byte) — refused even
        // though far under the generic 4 GiB bound.
        let big = nar_of_file(&vec![0u8; rio_common::limits::MAX_DRV_NAR_BYTES as usize]);
        let big_info = make_path_info_for_nar(&test_store_path("big.drv"), &big);
        let err = AdmittedNar::admit(&big_info, big).unwrap_err();
        assert!(
            matches!(
                err,
                AdmitDenial::OverCap {
                    is_derivation: true,
                    limit,
                    ..
                } if limit == rio_common::limits::MAX_DRV_NAR_BYTES
            ),
            "got {err:?}"
        );

        // Non-.drv: admitted, no preimage, no text-CA obligation.
        let plain = nar_of_file(b"hello");
        let plain_info = make_path_info_for_nar(&test_store_path("plain"), &plain);
        let a = AdmittedNar::admit(&plain_info, plain).expect("non-.drv admitted");
        assert!(a.drv_preimage().is_none());
    }
}
