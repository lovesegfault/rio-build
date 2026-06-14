//! Stages 2+3: negotiate presence (bulk `Has` per object kind,
//! tenant-scoped, short-circuited by the cluster-ack table) and upload
//! the misses, largest-first so builds unblock early.
//!
//! Drv blobs go through `DrvBlobService::PutDrvBlobs` (write-through
//! idempotent; the store verifies digest ↔ canonical bytes ↔ drv_path
//! server-side). Source trees go through the existing castore put
//! surface, `StoreService::PutPathChunked`: one Begin frame carrying
//! the Directory DAG + per-file chunk manifest + the `novel` chunk
//! list (the `HasChunks` misses), then one Chunk frame per novel
//! digest. Chunk bytes are re-read from the ORIGIN tree at upload time
//! (the client CAS never stores chunk copies of local working trees —
//! ADR-024); the re-read is digest-verified so a mutated tree fails
//! loudly instead of uploading wrong content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use prost::Message;
use rio_evalstore::dirblob::{BuiltDir, BuiltEntry};
use rio_evalstore::ingest::{IngestConfig, IngestNode, ingest_tree};
use rio_proto::evaljob::SourceRoot;
use rio_proto::types::{
    ChunkData, ChunkMeta, ChunkedOutput, DrvBlob, HasChunksRequest, HasDirectoriesRequest,
    HasDrvsRequest, PutDrvBlobsRequest, PutPathChunkedBegin, PutPathChunkedRequest,
    put_path_chunked_request,
};
use tracing::{debug, instrument};

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
    drv_blobs: Vec<DrvBlob>,
    sources: Vec<SourceRoot>,
) -> anyhow::Result<UploadReport> {
    let mut report = UploadReport::default();

    for src in sources {
        let digest: Digest32 = src
            .dir_digest
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("source root dir_digest is not 32 bytes"))?;
        upload_source(&mut clients, &acks, &src, &digest).await?;
        report.acked_sources.push(digest);
    }

    report.acked_drvs = upload_drvs(&mut clients, &acks, drv_blobs).await?;
    Ok(report)
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

/// Source negotiation + upload. Presence key is the root Directory
/// digest: present + tenant-visible ⇒ the whole tree was committed by
/// a completed upload (`PutPathChunked` commits dirs + chunks + path
/// atomically), so the root bit short-circuits the entire tree.
async fn upload_source(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    src: &SourceRoot,
    digest: &Digest32,
) -> anyhow::Result<()> {
    if acks
        .lock()
        .expect("ack table mutex poisoned")
        .is_acked(ObjectKind::Directory, digest)
    {
        debug!(store_path = %src.store_path, "source: ack-short-circuit");
        return Ok(());
    }
    let bitmap = clients
        .directories
        .has_directories(clients.req(HasDirectoriesRequest {
            digests: vec![digest.to_vec()],
        })?)
        .await
        .context("HasDirectories")?
        .into_inner()
        .bitmap;
    if bitmap_bit(&bitmap, 0) {
        debug!(store_path = %src.store_path, "source: cluster-has");
    } else {
        let plan = plan_source_upload(src).context("planning source upload")?;
        put_source_chunked(clients, acks, src, plan).await?;
    }
    acks.lock()
        .expect("ack table mutex poisoned")
        .record(ObjectKind::Directory, &[*digest])?;
    Ok(())
}

/// Everything `PutPathChunked` needs for one source root, derived by
/// re-reading the origin tree (single-read ingest: NAR identity +
/// FastCDC plane in one walk).
struct SourcePlan {
    root_node: rio_proto::castore::RootNode,
    directories: Vec<rio_proto::castore::Directory>,
    /// Per-file chunk runs in canonical NAR walk order (repeats
    /// included, per the wire contract).
    chunk_manifest: Vec<ChunkMeta>,
    /// Global first-occurrence order of unique chunk digests — the
    /// order `novel` must follow.
    chunk_order: Vec<Digest32>,
    /// Where to re-read each unique chunk: (file, offset, len).
    chunk_locations: HashMap<Digest32, (PathBuf, u64, u32)>,
}

// r[impl bc.upload.origin-reread]
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

    let mut plan = SourcePlan {
        root_node: rio_proto::castore::RootNode { node: None },
        directories: Vec::new(),
        chunk_manifest: Vec::new(),
        chunk_order: Vec::new(),
        chunk_locations: HashMap::new(),
    };

    use std::os::unix::ffi::OsStrExt;

    // One walk produces the chunk manifest (NAR order) + locations;
    // the dirblob fold (shared canonical encode) produces the
    // Directory bodies + root digest.
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
                    if !plan.chunk_locations.contains_key(&c.digest) {
                        plan.chunk_order.push(c.digest);
                        plan.chunk_locations
                            .insert(c.digest, (fs_path.to_path_buf(), c.offset, c.len));
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
                    rio_proto::castore::Directory::decode(bytes.as_ref())
                        .context("decoding folded directory blob")
                })
                .collect::<anyhow::Result<_>>()?;
            plan.root_node = rio_proto::castore::RootNode {
                node: Some(rio_proto::castore::root_node::Node::DirDigest(
                    folded.root_digest.0.to_vec(),
                )),
            };
        }
        Some(BuiltEntry::File { .. }) | Some(BuiltEntry::Symlink { .. }) => {
            // Single-file / symlink source roots have no Directory DAG;
            // their castore form is an inline RootNode. The negotiation
            // key the worker reported (dir_digest) has no Directory to
            // resolve to, so the protocol needs a per-kind root.
            // TODO: support file/symlink roots end-to-end (RootNode::
            // file/symlink in evaljob.SourceRoot + Has keyed on the
            // file digest). Tree sources cover the dominant case
            // (flake inputs, local working trees).
            bail!("source root {origin:?} is not a directory (file/symlink roots: TODO)");
        }
        None => unreachable!("walk always returns an entry"),
    }
    Ok(plan)
}

/// Probe chunk presence and stream the `PutPathChunked` upload.
async fn put_source_chunked(
    clients: &mut Clients,
    acks: &Arc<Mutex<ClusterAckTable>>,
    src: &SourceRoot,
    plan: SourcePlan,
) -> anyhow::Result<()> {
    // Chunk negotiation: ack table first, then bulk HasChunks.
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

    // Read novel chunk bodies from the origin, digest-verified.
    let mut frames: Vec<PutPathChunkedRequest> = Vec::with_capacity(novel.len() + 1);
    frames.push(PutPathChunkedRequest {
        msg: Some(put_path_chunked_request::Msg::Begin(begin)),
    });
    for d in &novel {
        let (path, offset, len) = plan
            .chunk_locations
            .get(d)
            .expect("novel digests come from chunk_order");
        let data = read_chunk(path, *offset, *len)?;
        if blake3::hash(&data).as_bytes() != d {
            bail!(
                "chunk at {path:?}+{offset} no longer hashes to {} — origin mutated mid-upload",
                hex::encode(d)
            );
        }
        frames.push(PutPathChunkedRequest {
            msg: Some(put_path_chunked_request::Msg::Chunk(ChunkData {
                digest: d.to_vec(),
                data: data.into(),
            })),
        });
    }

    let req = clients.req(tokio_stream::iter(frames))?;
    clients
        .store
        .put_path_chunked(req)
        .await
        .context("PutPathChunked")?;

    let mut table = acks.lock().expect("ack table mutex poisoned");
    table.record(ObjectKind::Chunk, &present)?;
    table.record(ObjectKind::Chunk, &novel)?;
    Ok(())
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
