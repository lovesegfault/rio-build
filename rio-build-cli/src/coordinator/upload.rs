//! Stages 2+3: negotiate presence (bulk `Has` per object kind,
//! tenant-scoped, short-circuited by the cluster-ack table) and upload
//! the misses, largest-first so builds unblock early.
//!
//! Drv blobs go through `DrvBlobService::PutDrvBlobs` (write-through
//! idempotent; the store verifies digest ↔ canonical bytes ↔ drv_path
//! server-side). Source roots go through the existing castore put
//! surface, `StoreService::PutPathChunked`: one Begin frame carrying
//! the Directory DAG (directory roots) or the inline root node
//! (single-file/symlink roots) + per-file chunk manifest + the `novel`
//! chunk list (the `HasChunks` misses), then one Chunk frame per novel
//! digest. For roots with a filesystem origin the chunk bytes are
//! re-read from the ORIGIN tree at upload time (the client CAS never
//! stores chunk copies of local working trees — ADR-024); the re-read
//! is digest-verified so a mutated tree fails loudly instead of
//! uploading wrong content. Roots with no origin (streamed: fetched
//! flake inputs, toFile text) are served from the client CAS itself —
//! its content records are digest-verified on read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use prost::Message;
use rio_evalstore::dirblob::{BuiltDir, BuiltEntry};
use rio_evalstore::ingest::{IngestConfig, IngestNode, chunk_bytes, ingest_tree};
use rio_evalstore::source_root_key;
use rio_proto::castore::root_node::Node as RootKind;
use rio_proto::castore::{Directory, FileEntry, RootNode, SymlinkEntry};
use rio_proto::evaljob::SourceRoot;
use rio_proto::types::{
    ChunkData, ChunkMeta, ChunkedOutput, DrvBlob, HasChunksRequest, HasDirectoriesRequest,
    HasDrvsRequest, PutDrvBlobsRequest, PutPathChunkedBegin, PutPathChunkedRequest,
    put_path_chunked_request,
};
use tracing::{debug, info, instrument, warn};

use crate::acks::{ClusterAckTable, ObjectKind};
use crate::coordinator::clients::{Clients, bitmap_bit};
use crate::coordinator::graph::Digest32;

/// PutDrvBlobs batch budget. Mean drv is ~3.4KB with a fat p99 tail;
/// 4MB keeps batches far below the 16MB message budget while still
/// amortizing the RPC.
const DRV_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Server-side blob-count cap per `PutDrvBlobs` call (rio-store's
/// `PUT_DRV_BATCH_MAX`). Tiny drvs hit this long before the byte
/// budget.
const DRV_BATCH_COUNT: usize = 4_096;

/// Server-side digest-count cap per `Has*` call (rio-store's
/// `HAS_BATCH_MAX`).
const HAS_BATCH: usize = 65_536;

/// What one upload batch achieved — fed back into the graph's ack
/// state and the eval parent's `AckFeedback`.
#[derive(Default)]
pub struct UploadReport {
    pub acked_drvs: Vec<Digest32>,
    pub acked_sources: Vec<Digest32>,
}

/// Negotiate + upload one batch of fresh objects (the fold outcome of
/// one or more worker frames). Sources first — they're the largest
/// objects and gate the same roots; within drvs, misses upload
/// largest-first.
#[instrument(skip_all, fields(component = "build-client", drvs = drv_blobs.len(), sources = sources.len()))]
pub async fn upload_batch(
    mut clients: Clients,
    acks: Arc<Mutex<ClusterAckTable>>,
    cas_root: PathBuf,
    drv_blobs: Vec<DrvBlob>,
    sources: Vec<SourceRoot>,
) -> anyhow::Result<UploadReport> {
    let mut report = UploadReport::default();

    for src in sources {
        let key: Digest32 = source_root_key(&src).ok_or_else(|| {
            anyhow::anyhow!(
                "source root {} carries neither a root node nor a 32-byte dir_digest",
                src.store_path
            )
        })?;
        upload_source(&mut clients, &acks, &cas_root, &src, &key).await?;
        report.acked_sources.push(key);
    }

    report.acked_drvs = upload_drvs(&mut clients, &acks, drv_blobs).await?;
    Ok(report)
}

/// The reported castore root kind, if any. `None` only on old-worker
/// frames, which can only describe directory roots (`dir_digest`).
fn root_kind(src: &SourceRoot) -> Option<&RootKind> {
    src.root_node.as_ref().and_then(|r| r.node.as_ref())
}

/// Drv negotiation + upload: ack-table short-circuit → bulk `HasDrvs`
/// → `PutDrvBlobs` the misses, largest body first.
// r[impl bc.negotiate.ack-short-circuit]
async fn upload_drvs(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    blobs: Vec<DrvBlob>,
) -> anyhow::Result<Vec<Digest32>> {
    let mut acked: Vec<Digest32> = Vec::new();
    let mut unknown: Vec<(Digest32, DrvBlob)> = Vec::new();
    {
        let table = acks.lock().expect("ack table mutex poisoned");
        for b in blobs {
            let d: Digest32 = b
                .digest
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("drv digest is not 32 bytes"))?;
            if table.is_acked(ObjectKind::Drv, &d) {
                acked.push(d);
            } else {
                unknown.push((d, b));
            }
        }
    }
    if unknown.is_empty() {
        return Ok(acked);
    }

    // One flat bulk Has over the batch (ADR-024: never a level walk),
    // sliced only by the server's HAS_BATCH_MAX request bound.
    let mut present: Vec<Digest32> = Vec::new();
    let mut misses: Vec<(Digest32, DrvBlob)> = Vec::new();
    for slice in unknown.chunks(HAS_BATCH) {
        let digests: Vec<Vec<u8>> = slice.iter().map(|(d, _)| d.to_vec()).collect();
        let bitmap = clients
            .drv_blobs
            .has_drvs(clients.req(HasDrvsRequest { digests })?)
            .await
            .context("HasDrvs")?
            .into_inner()
            .bitmap;
        for (i, (d, b)) in slice.iter().enumerate() {
            if bitmap_bit(&bitmap, i) {
                present.push(*d);
            } else {
                misses.push((*d, b.clone()));
            }
        }
    }

    // Largest-first: big drvs gate the most downstream bytes; a
    // re-`Has` after any disconnect only shrinks the miss set, so the
    // order is purely a latency choice.
    misses.sort_by_key(|(_, b)| std::cmp::Reverse(b.body.len()));

    // Split into size-bounded batches, preserving largest-first order.
    let mut pending: Vec<(Vec<DrvBlob>, Vec<Digest32>)> = Vec::new();
    let mut batch: Vec<DrvBlob> = Vec::new();
    let mut batch_digests: Vec<Digest32> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut uploaded: Vec<Digest32> = Vec::new();
    for (d, b) in misses {
        if (batch_bytes + b.body.len() > DRV_BATCH_BYTES || batch.len() >= DRV_BATCH_COUNT)
            && !batch.is_empty()
        {
            pending.push((
                std::mem::take(&mut batch),
                std::mem::take(&mut batch_digests),
            ));
            batch_bytes = 0;
        }
        batch_bytes += b.body.len();
        batch.push(b);
        batch_digests.push(d);
    }
    if !batch.is_empty() {
        pending.push((batch, batch_digests));
    }
    for (blobs, digests) in pending {
        debug!(count = blobs.len(), "PutDrvBlobs batch");
        clients
            .drv_blobs
            .put_drv_blobs(clients.req(PutDrvBlobsRequest { blobs })?)
            .await
            .context("PutDrvBlobs")?;
        uploaded.extend(digests);
    }

    // Record acks once per outcome class (one file write each).
    {
        let mut table = acks.lock().expect("ack table mutex poisoned");
        table.record(ObjectKind::Drv, &present)?;
        table.record(ObjectKind::Drv, &uploaded)?;
    }
    acked.extend(present);
    acked.extend(uploaded);
    Ok(acked)
}

/// Re-upload specific drv blobs unconditionally (stale-ack recovery:
/// the scheduler named these digests missing; presence was already
/// re-probed by the caller).
pub async fn reupload_drvs(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    blobs: Vec<DrvBlob>,
) -> anyhow::Result<()> {
    if blobs.is_empty() {
        return Ok(());
    }
    let digests: Vec<Digest32> = blobs
        .iter()
        .map(|b| {
            b.digest
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("drv digest is not 32 bytes"))
        })
        .collect::<anyhow::Result<_>>()?;
    for chunk in blobs.chunks(DRV_BATCH_COUNT) {
        clients
            .drv_blobs
            .put_drv_blobs(clients.req(PutDrvBlobsRequest {
                blobs: chunk.to_vec(),
            })?)
            .await
            .context("PutDrvBlobs (stale-ack recovery)")?;
    }
    acks.lock()
        .expect("ack table mutex poisoned")
        .record(ObjectKind::Drv, &digests)?;
    Ok(())
}

/// Source negotiation + upload, keyed by [`source_root_key`].
///
/// Directory roots keep the cluster `HasDirectories` probe: present +
/// tenant-visible ⇒ the whole tree was committed by a completed upload
/// (`PutPathChunked` commits dirs + chunks + path atomically), so the
/// root bit short-circuits the entire tree. Single-file and symlink
/// roots have no path-level Has RPC — they are KB-sized, the put is
/// idempotent, and the persistent ack table suppresses repeats.
// r[impl bc.upload.source-root-kinds]
async fn upload_source(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    cas_root: &Path,
    src: &SourceRoot,
    key: &Digest32,
) -> anyhow::Result<()> {
    // The ack table reuses ObjectKind::Directory for every root kind:
    // file/symlink keys are domain-separated (source_root_key), so they
    // cannot collide with real directory digests, and existing ack
    // files stay valid.
    if acks
        .lock()
        .expect("ack table mutex poisoned")
        .is_acked(ObjectKind::Directory, key)
    {
        debug!(store_path = %src.store_path, "source: ack-short-circuit");
        return Ok(());
    }
    let is_dir_root = matches!(root_kind(src), Some(RootKind::DirDigest(_)) | None);
    let cluster_has = if is_dir_root {
        let bitmap = clients
            .directories
            .has_directories(clients.req(HasDirectoriesRequest {
                digests: vec![key.to_vec()],
            })?)
            .await
            .context("HasDirectories")?
            .into_inner()
            .bitmap;
        bitmap_bit(&bitmap, 0)
    } else {
        false
    };
    if cluster_has {
        debug!(store_path = %src.store_path, "source: cluster-has");
    } else {
        let plan = if src.origin.is_empty() {
            plan_source_upload_cas(cas_root, src).context("planning CAS-read source upload")?
        } else {
            plan_source_upload(src).context("planning source upload")?
        };
        put_source_chunked(clients, acks, src, plan).await?;
    }
    acks.lock()
        .expect("ack table mutex poisoned")
        .record(ObjectKind::Directory, &[*key])?;
    Ok(())
}

/// Where one novel chunk's bytes come from at upload time.
enum ChunkSource {
    /// Re-read from the origin tree (local roots — the origin IS the
    /// byte store, ADR-024 not-a-mirror rule).
    File {
        path: PathBuf,
        offset: u64,
        len: u32,
    },
    /// Already in memory (CAS-read roots: the content record was
    /// fetched digest-verified at plan time).
    Bytes(Vec<u8>),
}

/// Everything `PutPathChunked` needs for one source root.
struct SourcePlan {
    root_node: RootNode,
    directories: Vec<Directory>,
    /// Per-file chunk runs in canonical NAR walk order (repeats
    /// included, per the wire contract).
    chunk_manifest: Vec<ChunkMeta>,
    /// Global first-occurrence order of unique chunk digests — the
    /// order `novel` must follow.
    chunk_order: Vec<Digest32>,
    /// Where to obtain each unique chunk's bytes.
    chunk_sources: HashMap<Digest32, ChunkSource>,
}

impl SourcePlan {
    fn empty() -> SourcePlan {
        SourcePlan {
            root_node: RootNode { node: None },
            directories: Vec::new(),
            chunk_manifest: Vec::new(),
            chunk_order: Vec::new(),
            chunk_sources: HashMap::new(),
        }
    }
}

/// Local-origin plan: re-read the origin at upload time (single-read
/// ingest: NAR identity + FastCDC plane in one walk) and verify the
/// recomputed root identity against what eval reported.
// r[impl bc.upload.origin-reread+2]
fn plan_source_upload(src: &SourceRoot) -> anyhow::Result<SourcePlan> {
    let origin = Path::new(&src.origin);
    let result = ingest_tree(origin, &IngestConfig::default())
        .with_context(|| format!("re-ingesting origin {origin:?}"))?;
    // The origin is the byte store; if it mutated since eval reported
    // the digest, the upload would commit content the skeleton never
    // referenced. Fail loudly.
    // TODO: ADR-024 allows re-ingest → re-negotiate the delta (at most
    // twice, then snapshot into the CAS). P3a hard-fails instead; the
    // escape hatch lands with the eval parent (P3b), which owns the
    // re-negotiation loop.
    if result.nar_sha256.as_slice() != src.nar_hash.as_slice() {
        bail!(
            "origin tree {origin:?} changed since eval: NAR sha256 {} != reported {}",
            hex::encode(result.nar_sha256),
            hex::encode(&src.nar_hash)
        );
    }

    let mut plan = SourcePlan::empty();

    use std::os::unix::ffi::OsStrExt;

    // One walk produces the chunk manifest (NAR order) + read
    // locations; the dirblob fold (shared canonical encode) produces
    // the Directory bodies + root digest.
    fn walk(
        node: &IngestNode,
        fs_path: &Path,
        plan: &mut SourcePlan,
    ) -> anyhow::Result<Option<BuiltEntry>> {
        match node {
            IngestNode::File(f) => {
                for c in &f.chunks {
                    plan.chunk_manifest.push(ChunkMeta {
                        digest: c.digest.to_vec(),
                        size: u64::from(c.len),
                    });
                    if !plan.chunk_sources.contains_key(&c.digest) {
                        plan.chunk_order.push(c.digest);
                        plan.chunk_sources.insert(
                            c.digest,
                            ChunkSource::File {
                                path: fs_path.to_path_buf(),
                                offset: c.offset,
                                len: c.len,
                            },
                        );
                    }
                }
                Ok(Some(BuiltEntry::File {
                    digest: rio_packstore::Digest(f.digest),
                    size: f.size,
                    executable: f.executable,
                }))
            }
            IngestNode::Symlink(s) => Ok(Some(BuiltEntry::Symlink {
                target: s.target.clone(),
            })),
            IngestNode::Dir(d) => {
                let mut built = BuiltDir::new();
                for entry in &d.entries {
                    let name_os = std::ffi::OsStr::from_bytes(&entry.name);
                    let child_path = fs_path.join(name_os);
                    if let Some(e) = walk(&entry.node, &child_path, plan)? {
                        built.push(entry.name.clone(), e);
                    }
                }
                Ok(Some(BuiltEntry::Dir(built)))
            }
        }
    }
    match walk(&result.root, origin, &mut plan)? {
        Some(BuiltEntry::Dir(built)) => {
            let folded = built.fold().context("folding source directories")?;
            if folded.root_digest.0.as_slice() != src.dir_digest.as_slice() {
                bail!(
                    "origin tree {origin:?} folds to root digest {} but eval reported {}",
                    hex::encode(folded.root_digest.0),
                    hex::encode(&src.dir_digest)
                );
            }
            plan.directories = folded
                .blobs
                .iter()
                .map(|(_, bytes)| {
                    Directory::decode(bytes.as_ref()).context("decoding folded directory blob")
                })
                .collect::<anyhow::Result<_>>()?;
            plan.root_node = RootNode {
                node: Some(RootKind::DirDigest(folded.root_digest.0.to_vec())),
            };
        }
        Some(BuiltEntry::File { .. }) => {
            // Single-file root: no Directory DAG, the inline root node
            // carries the identity. Cross-check the re-ingested root
            // against the eval-reported one, mirroring the dir_digest
            // check above.
            let IngestNode::File(f) = &result.root else {
                unreachable!("walk returned File for a non-file root")
            };
            match root_kind(src) {
                Some(RootKind::File(reported)) => {
                    if reported.digest.as_slice() != f.digest || reported.executable != f.executable
                    {
                        bail!(
                            "origin file {origin:?} no longer matches the eval-reported root \
                             (content digest or executable bit changed)"
                        );
                    }
                }
                other => bail!(
                    "origin {origin:?} re-ingested as a single file but eval reported {other:?}"
                ),
            }
            plan.root_node = RootNode {
                node: Some(RootKind::File(FileEntry {
                    // A NAR root has no name.
                    name: Vec::new(),
                    digest: f.digest.to_vec(),
                    size: f.size,
                    executable: f.executable,
                })),
            };
        }
        Some(BuiltEntry::Symlink { .. }) => {
            let IngestNode::Symlink(s) = &result.root else {
                unreachable!("walk returned Symlink for a non-symlink root")
            };
            match root_kind(src) {
                Some(RootKind::Symlink(reported)) => {
                    if reported.target != s.target {
                        bail!(
                            "origin symlink {origin:?} target changed since eval ({} != {})",
                            s.target.escape_ascii(),
                            reported.target.escape_ascii()
                        );
                    }
                }
                other => {
                    bail!("origin {origin:?} re-ingested as a symlink but eval reported {other:?}")
                }
            }
            plan.root_node = RootNode {
                node: Some(RootKind::Symlink(SymlinkEntry {
                    name: Vec::new(),
                    target: s.target.clone(),
                })),
            };
        }
        None => unreachable!("walk always returns an entry"),
    }
    Ok(plan)
}

/// CAS-read plan: the root has no filesystem origin (streamed — a
/// fetched flake input or toFile text). Bytes come out of the client
/// CAS instead: whole-file content records keyed by content blake3 and
/// Directory blobs keyed by dir_digest, every read digest-verified by
/// the pack store, so no changed-since-eval guard is needed.
// r[impl bc.upload.cas-read]
fn plan_source_upload_cas(cas_root: &Path, src: &SourceRoot) -> anyhow::Result<SourcePlan> {
    let pack = rio_packstore::PackStore::open(cas_root, rio_packstore::Options::default())
        .with_context(|| format!("opening client CAS at {cas_root:?}"))?;
    let mut plan = SourcePlan::empty();

    match root_kind(src) {
        Some(RootKind::File(f)) => {
            let data = cas_record(&pack, src, &f.digest)?;
            cas_add_file(&mut plan, &data);
            plan.root_node = RootNode {
                node: Some(RootKind::File(f.clone())),
            };
        }
        Some(RootKind::Symlink(s)) => {
            plan.root_node = RootNode {
                node: Some(RootKind::Symlink(s.clone())),
            };
        }
        Some(RootKind::DirDigest(_)) | None => {
            let root: Digest32 = src
                .dir_digest
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("source root dir_digest is not 32 bytes"))?;
            let mut seen_dirs: std::collections::HashSet<Digest32> =
                std::collections::HashSet::new();
            walk_cas_dir(&pack, src, &root, &mut plan, &mut seen_dirs)?;
            plan.root_node = RootNode {
                node: Some(RootKind::DirDigest(root.to_vec())),
            };
        }
    }
    Ok(plan)
}

/// One content/Directory record from the client CAS, digest-verified by
/// the pack store on read.
fn cas_record(
    pack: &rio_packstore::PackStore,
    src: &SourceRoot,
    digest: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let d: Digest32 = digest
        .try_into()
        .map_err(|_| anyhow::anyhow!("CAS record digest is not 32 bytes"))?;
    let bytes = pack.get(&rio_packstore::Digest(d))?.ok_or_else(|| {
        anyhow::anyhow!(
            "client CAS has no record {} (needed by {})",
            hex::encode(d),
            src.store_path
        )
    })?;
    Ok(bytes.to_vec())
}

/// Append one file's chunk run to the manifest; the first occurrence of
/// each chunk also registers its in-memory bytes.
fn cas_add_file(plan: &mut SourcePlan, data: &[u8]) {
    let (_digest, chunks) = chunk_bytes(data);
    for c in chunks {
        plan.chunk_manifest.push(ChunkMeta {
            digest: c.digest.to_vec(),
            size: u64::from(c.len),
        });
        if !plan.chunk_sources.contains_key(&c.digest) {
            plan.chunk_order.push(c.digest);
            let body = data[c.offset as usize..(c.offset + u64::from(c.len)) as usize].to_vec();
            plan.chunk_sources
                .insert(c.digest, ChunkSource::Bytes(body));
        }
    }
}

/// One Directory level of the CAS-read walk: DFS from the eval-reported
/// root digest. Within one Directory the canonical encode keeps the
/// three kind lists sorted independently; NAR walk order is the global
/// byte-lex merge across kinds, so entries are merged by name before
/// recursing. Hash-chained digests cannot cycle; depth is bounded by
/// the ingest-time NAR limits.
fn walk_cas_dir(
    pack: &rio_packstore::PackStore,
    src: &SourceRoot,
    digest: &Digest32,
    plan: &mut SourcePlan,
    seen_dirs: &mut std::collections::HashSet<Digest32>,
) -> anyhow::Result<()> {
    let bytes = cas_record(pack, src, digest)?;
    let dir = Directory::decode(bytes.as_slice()).context("decoding CAS directory blob")?;
    if seen_dirs.insert(*digest) {
        plan.directories.push(dir.clone());
    }

    enum Entry<'a> {
        Dir(&'a rio_proto::castore::DirectoryEntry),
        File(&'a FileEntry),
        Symlink,
    }
    let mut entries: Vec<(&[u8], Entry)> = Vec::new();
    entries.extend(
        dir.directories
            .iter()
            .map(|d| (d.name.as_slice(), Entry::Dir(d))),
    );
    entries.extend(
        dir.files
            .iter()
            .map(|f| (f.name.as_slice(), Entry::File(f))),
    );
    entries.extend(
        dir.symlinks
            .iter()
            .map(|s| (s.name.as_slice(), Entry::Symlink)),
    );
    entries.sort_by(|a, b| a.0.cmp(b.0));

    for (_, entry) in entries {
        match entry {
            Entry::Dir(d) => {
                let child: Digest32 = d
                    .digest
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("directory entry digest is not 32 bytes"))?;
                walk_cas_dir(pack, src, &child, plan, seen_dirs)?;
            }
            Entry::File(f) => {
                // Identical files repeat their chunk run per the wire
                // contract; a re-fetch per repeat is a local pack read.
                let data = cas_record(pack, src, &f.digest)?;
                cas_add_file(plan, &data);
            }
            Entry::Symlink => {}
        }
    }
    Ok(())
}

/// Probe chunk presence and stream the `PutPathChunked` upload, with
/// chunk stale-ack recovery — the upload-path sibling of
/// [`crate::coordinator::submit::submit_root`]'s drv recovery.
///
/// An `UNAVAILABLE` reject means the store could not find the backing
/// object for a chunk this upload referenced as already present (the
/// ack TTL outlived the cluster's GC grace, or a presence row outlived
/// its S3 object). The named acks are evicted (every chunk in the
/// upload when the reject names none), presence is re-probed, and the
/// upload retried — ONCE. A second reject is a hard error: either the
/// cluster is GCing faster than the ack TTL models (a config bug) or
/// the store keeps lying about presence; retrying forever would mask
/// both.
// r[impl bc.upload.stale-ack-once]
async fn put_source_chunked(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    src: &SourceRoot,
    plan: SourcePlan,
) -> anyhow::Result<()> {
    match try_put_source(clients, acks, src, &plan).await {
        Ok(()) => Ok(()),
        Err(PutSourceError::StaleChunks { missing, reject }) => {
            warn!(
                store_path = %src.store_path,
                missing = missing.len(),
                reject = %reject,
                "PutPathChunked rejected on chunks the cluster no longer holds — \
                 running stale-ack recovery"
            );
            acks.lock()
                .expect("ack table mutex poisoned")
                .evict(ObjectKind::Chunk, &missing)?;
            match try_put_source(clients, acks, src, &plan).await {
                Ok(()) => {
                    info!(store_path = %src.store_path, "stale-ack recovery succeeded on re-upload");
                    Ok(())
                }
                Err(PutSourceError::StaleChunks {
                    reject: second_reject,
                    ..
                }) => bail!(
                    "PutPathChunked for {} rejected twice on missing chunks — recovery \
                     evicted {} ack(s), re-probed presence and re-uploaded, but the second \
                     attempt was rejected again ({second_reject}); giving up (second reject \
                     is a hard error per ADR-024)",
                    src.store_path,
                    missing.len(),
                ),
                Err(PutSourceError::Other(e)) => Err(e),
            }
        }
        Err(PutSourceError::Other(e)) => Err(e),
    }
}

enum PutSourceError {
    /// `UNAVAILABLE` from the store: the backing object for at least
    /// one referenced chunk is gone — the listed acks are stale.
    /// `reject` keeps the store's status message so a misclassified
    /// transient fault still surfaces its real cause in logs/errors.
    StaleChunks {
        missing: Vec<Digest32>,
        reject: String,
    },
    Other(anyhow::Error),
}

/// Map a `PutPathChunked` failure. An `UNAVAILABLE` triggers stale-ack
/// recovery: the digests it names (shared formatter/parser in
/// `rio_proto::chunk_reject`) are the stale acks; an `UNAVAILABLE`
/// naming none falls back to every chunk in the upload — the store
/// proved at least one referenced object is unreachable but not which.
fn classify_put_status(status: tonic::Status, plan_chunks: &[Digest32]) -> PutSourceError {
    if status.code() == tonic::Code::Unavailable {
        let named: Vec<Digest32> =
            rio_proto::chunk_reject::parse_missing_chunk_digests(status.message())
                .into_iter()
                .filter(|d| plan_chunks.contains(d))
                .collect();
        return PutSourceError::StaleChunks {
            missing: if named.is_empty() {
                plan_chunks.to_vec()
            } else {
                named
            },
            reject: status.message().to_string(),
        };
    }
    PutSourceError::Other(anyhow::Error::new(status).context("PutPathChunked"))
}

/// One negotiation + upload attempt for a source root.
async fn try_put_source(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    src: &SourceRoot,
    plan: &SourcePlan,
) -> Result<(), PutSourceError> {
    let (novel, present) = negotiate_chunks(clients, acks, src, plan)
        .await
        .map_err(PutSourceError::Other)?;
    let frames = chunk_frames(src, plan, &novel).map_err(PutSourceError::Other)?;

    let req = clients
        .req(tokio_stream::iter(frames))
        .map_err(PutSourceError::Other)?;
    if let Err(status) = clients.store.put_path_chunked(req).await {
        return Err(classify_put_status(status, &plan.chunk_order));
    }

    let mut table = acks.lock().expect("ack table mutex poisoned");
    table
        .record(ObjectKind::Chunk, &present)
        .map_err(|e| PutSourceError::Other(e.into()))?;
    table
        .record(ObjectKind::Chunk, &novel)
        .map_err(|e| PutSourceError::Other(e.into()))?;
    Ok(())
}

/// Chunk negotiation for one source root: ack table first, then bulk
/// `HasChunks`. Returns `(novel, present)` — the misses to stream and
/// the cluster-confirmed hits to ack.
async fn negotiate_chunks(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    src: &SourceRoot,
    plan: &SourcePlan,
) -> anyhow::Result<(Vec<Digest32>, Vec<Digest32>)> {
    let (cached, unknown): (Vec<Digest32>, Vec<Digest32>) = {
        let table = acks.lock().expect("ack table mutex poisoned");
        plan.chunk_order
            .iter()
            .copied()
            .partition(|d| table.is_acked(ObjectKind::Chunk, d))
    };
    let mut novel: Vec<Digest32> = Vec::new();
    let mut present: Vec<Digest32> = Vec::new();
    for slice in unknown.chunks(HAS_BATCH) {
        let bitmap = clients
            .chunks
            .has_chunks(clients.req(HasChunksRequest {
                digests: slice.iter().map(|d| d.to_vec()).collect(),
            })?)
            .await
            .context("HasChunks")?
            .into_inner()
            .bitmap;
        for (i, d) in slice.iter().enumerate() {
            if bitmap_bit(&bitmap, i) {
                present.push(*d);
            } else {
                novel.push(*d);
            }
        }
    }
    debug!(
        path = %src.store_path,
        novel = novel.len(),
        cached = cached.len(),
        present = present.len(),
        "PutPathChunked"
    );
    Ok((novel, present))
}

/// Build the `PutPathChunked` frame sequence: the Begin frame plus one
/// Chunk frame per novel digest, with bodies gathered from the origin
/// re-read or the CAS-resident bytes — digest-verified either way
/// before they hit the wire.
fn chunk_frames(
    src: &SourceRoot,
    plan: &SourcePlan,
    novel: &[Digest32],
) -> anyhow::Result<Vec<PutPathChunkedRequest>> {
    let begin = PutPathChunkedBegin {
        deriver: String::new(), // sources have no deriver
        outputs: vec![ChunkedOutput {
            store_path: src.store_path.clone(),
            nar_hash: src.nar_hash.clone(),
            nar_size: src.nar_size,
            references: vec![],
            root_node: Some(plan.root_node.clone()),
            chunk_manifest: plan.chunk_manifest.clone(),
        }],
        directories: plan.directories.clone(),
        novel: novel.iter().map(|d| d.to_vec()).collect(),
        input_closure: vec![],
    };

    let mut frames: Vec<PutPathChunkedRequest> = Vec::with_capacity(novel.len() + 1);
    frames.push(PutPathChunkedRequest {
        msg: Some(put_path_chunked_request::Msg::Begin(begin)),
    });
    for d in novel {
        let data = match plan
            .chunk_sources
            .get(d)
            .expect("novel digests come from chunk_order")
        {
            ChunkSource::File { path, offset, len } => {
                let data = read_chunk(path, *offset, *len)?;
                if blake3::hash(&data).as_bytes() != d {
                    bail!(
                        "chunk at {path:?}+{offset} no longer hashes to {} — origin mutated \
                         mid-upload",
                        hex::encode(d)
                    );
                }
                data
            }
            ChunkSource::Bytes(body) => body.clone(),
        };
        frames.push(PutPathChunkedRequest {
            msg: Some(put_path_chunked_request::Msg::Chunk(ChunkData {
                digest: d.to_vec(),
                data: data.into(),
            })),
        });
    }
    Ok(frames)
}

fn read_chunk(path: &Path, offset: u64, len: u32) -> anyhow::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(path).with_context(|| format!("opening {path:?} for chunk read"))?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)
        .with_context(|| format!("reading chunk {path:?}+{offset}+{len}"))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recovery trigger at the client boundary: an `UNAVAILABLE`
    /// built with the store's shared formatter classifies as
    /// `StaleChunks` naming exactly the digest; one naming nothing from
    /// this upload falls back to the whole chunk set; any other code
    /// never triggers recovery.
    #[test]
    fn classify_put_status_extracts_stale_chunks() {
        let plan: Vec<Digest32> = vec![[0x11; 32], [0x22; 32]];

        let status = tonic::Status::unavailable(format!(
            "PutPathChunked: {}",
            rio_proto::chunk_reject::missing_chunk_digest_message(&hex::encode([0x22u8; 32]))
        ));
        match classify_put_status(status, &plan) {
            PutSourceError::StaleChunks { missing, .. } => {
                assert_eq!(missing, vec![[0x22; 32]]);
            }
            PutSourceError::Other(e) => panic!("expected StaleChunks, got {e:#}"),
        }

        // A reject naming no chunk of this upload (e.g. a transient S3
        // fault message) conservatively evicts the whole set, and the
        // original message is retained for the eventual hard error.
        let status = tonic::Status::unavailable("PutPathChunked: chunk upload failed: timeout");
        match classify_put_status(status, &plan) {
            PutSourceError::StaleChunks { missing, reject } => {
                assert_eq!(missing, plan);
                assert_eq!(reject, "PutPathChunked: chunk upload failed: timeout");
            }
            PutSourceError::Other(e) => panic!("expected StaleChunks, got {e:#}"),
        }

        // Anything that is not UNAVAILABLE is a plain error.
        assert!(matches!(
            classify_put_status(tonic::Status::invalid_argument("bad Begin"), &plan),
            PutSourceError::Other(_)
        ));
    }
}
