//! `PutPathChunked` upload: single-pass fused walk + chunked client-stream.
//!
//! ADR-022 §6.1: after the build exits, each output's directory tree is
//! walked **once**, in canonical NAR entry order. The walk generates the
//! NAR framing on the fly (no NAR byte stream is materialized) and feeds
//! every byte — framing, entry names, symlink targets, file contents —
//! into a per-output SHA-256 accumulator and Boyer-Moore reference
//! scanner. Per regular file, the same disk read additionally drives
//! FastCDC boundary detection emitting `(offset, len, blake3)` per chunk
//! and a whole-file BLAKE3 yielding the castore `FileEntry.digest`.
//! `Directory` bodies are built bottom-up during the same recursion. At
//! end-of-walk the builder holds `{chunk_manifest, file_digests,
//! root_node, refs, nar_hash, nar_size}` having read each output byte
//! exactly once.
//!
//! The send phase (`upload_outputs_chunked`) probes `HasChunks` for the
//! union of all outputs' chunk digests, computes `novel` as the
//! global-first-occurrence-ordered absent subset, streams
//! `Begin{deriver, outputs[], directories[], novel[], input_closure}`
//! and then one `Chunk` frame per `novel` digest in `novel` order. Novel
//! chunk bytes are re-read from the overlay upper at send time (the
//! walk records `(path, offset, len)` per first occurrence) so peak
//! memory stays O(one chunk) regardless of output size.
// r[impl builder.upload.fused-walk]
// r[impl builder.upload.chunked-manifest]

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use prost::Message;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::nar::{self, NarError};
use rio_nix::protocol::wire::{ZERO_PAD, padding_len};
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_nix::store_path::StorePath;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{
    ChunkRef, ChunkedOutputHeader, HasChunksRequest, PutPathChunkedBegin, PutPathChunkedChunk,
    PutPathChunkedRequest, put_path_chunked_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{StoreServiceClient, castore};

use rio_common::grpc::GRPC_STREAM_TIMEOUT;

use super::UploadError;
use super::common::{
    STREAM_CHANNEL_BUF, attach_assignment_token, await_dump_after_rx_drop, await_dump_bounded,
    nar_err_to_status, uploaded_info,
};

/// FastCDC size parameters. MUST match `rio-store/src/chunker.rs`
/// (`CHUNK_MIN`/`CHUNK_AVG`/`CHUNK_MAX`, the source of truth) — the
/// spec pins 16/64/256 KiB (`r[store.cas.fastcdc]`), the server
/// rejects any `chunk_manifest` entry whose size exceeds its own
/// `CHUNK_MAX`, and a divergence in `CHUNK_MIN`/`CHUNK_AVG` silently
/// stops builder-chunked and store-chunked content from deduplicating
/// against each other. Duplicated rather than imported because
/// rio-builder cannot depend on rio-store (the store links the whole
/// S3/PG dep tree); the `chunker_constants_match_rio_store` test below
/// pins the mirror via the dev-dependency.
// r[impl store.cas.fastcdc]
const CHUNK_MIN: usize = 16 * 1024;
const CHUNK_AVG: usize = 64 * 1024;
const CHUNK_MAX: usize = 256 * 1024;

/// Mirror of rio-nix's `MAX_NAR_DEPTH` / `MAX_DIRECTORY_ENTRIES`
/// (`pub(super)` there). The producer must not emit a tree the consumer
/// is guaranteed to reject.
const MAX_NAR_DEPTH: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Fused walk
// ---------------------------------------------------------------------------

/// Where a chunk's bytes live on disk: the regular file containing it
/// and the byte range within that file. Recorded for the FIRST
/// occurrence of each digest; the send phase re-reads this range for
/// every `novel` digest instead of buffering chunk bytes across the
/// whole walk.
#[derive(Debug, Clone)]
pub(super) struct ChunkSource {
    pub(super) file: PathBuf,
    pub(super) offset: u64,
    pub(super) len: u32,
}

/// Everything the fused walk learns about one output in its single
/// disk pass. Sufficient to build the output's `ChunkedOutputHeader`,
/// contribute its `Directory` bodies to `Begin.directories`, and
/// stream its novel chunks — and to drive the legacy
/// `PutPath`/`PutPathBatch` fallback without re-reading disk
/// (`store_path`/`parsed`/`references` are a superset of the legacy
/// `PreparedOutput`).
#[derive(Debug, Clone)]
pub(super) struct WalkedOutput {
    /// Basename under the overlay upper store (`"abc…-hello"`).
    pub(super) basename: String,
    /// `"/nix/store/{basename}"`.
    pub(super) store_path: String,
    /// Validated parse of `store_path`.
    pub(super) parsed: StorePath,
    /// SHA-256 over the full canonical NAR byte sequence.
    pub(super) nar_hash: [u8; 32],
    /// Total NAR bytes (framing + content).
    pub(super) nar_size: u64,
    /// Resolved, sorted reference paths found by the refscan.
    pub(super) references: Vec<String>,
    /// Castore root node (DirDigest for directory roots, inline
    /// File/Symlink entry otherwise).
    pub(super) root_node: castore::RootNode,
    /// Ordered `(digest, len)` over this output's regular-file contents
    /// in canonical NAR walk order. Per-file-aligned: each file's
    /// contiguous run sums to exactly that file's size.
    pub(super) chunk_manifest: Vec<ChunkRef>,
    /// Distinct `Directory` bodies reachable from `root_node`, keyed by
    /// `blake3(canonical_encode(body))`.
    pub(super) directories: HashMap<[u8; 32], castore::Directory>,
    /// First-occurrence on-disk location of each distinct chunk digest.
    pub(super) chunk_sources: HashMap<[u8; 32], ChunkSource>,
}

/// What a child node contributes to its parent `Directory` body.
enum WalkedNode {
    File {
        digest: [u8; 32],
        size: u64,
        executable: bool,
    },
    Dir {
        digest: [u8; 32],
        /// Recursive descendant count (`DirectoryEntry.size`).
        size: u64,
    },
    Symlink {
        target: Vec<u8>,
    },
}

/// Mutable accumulator state threaded through the recursive walk.
struct WalkState {
    /// SHA-256 over every NAR byte (framing + content).
    hasher: Sha256,
    /// Boyer-Moore reference scanner over the same byte sequence.
    scanner: RefScanSink,
    /// Total NAR bytes emitted so far.
    nar_size: u64,
    /// Ordered `(digest, len)` chunk list across all files walked so far.
    chunk_manifest: Vec<ChunkRef>,
    /// Distinct `Directory` bodies, keyed by digest.
    directories: HashMap<[u8; 32], castore::Directory>,
    /// First-occurrence source location per chunk digest.
    chunk_sources: HashMap<[u8; 32], ChunkSource>,
}

impl WalkState {
    /// Feed NAR framing/content bytes into the SHA-256 + refscan + size
    /// accumulators. The `RefScanSink::write` impl is infallible
    /// (returns `Ok` always), so the `expect` cannot fire.
    fn absorb(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.scanner
            .write_all(bytes)
            .expect("RefScanSink::write is infallible");
        self.nar_size += bytes.len() as u64;
    }

    /// NAR string framing: `u64le(len) ++ bytes ++ pad-to-8`. Byte
    /// layout matches `rio_nix::nar::sync_wire::write_bytes` — the
    /// `nar_roundtrip_matches_dump_path` test pins the equivalence
    /// against the `dump_path` oracle.
    fn write_wire_bytes(&mut self, bytes: &[u8]) {
        self.absorb(&(bytes.len() as u64).to_le_bytes());
        self.absorb(bytes);
        let pad = padding_len(bytes.len());
        if pad > 0 {
            self.absorb(&ZERO_PAD[..pad]);
        }
    }

    fn write_wire_str(&mut self, s: &str) {
        self.write_wire_bytes(s.as_bytes());
    }
}

/// Walk one output root. One disk read per regular file; no NAR byte
/// stream is materialized.
///
/// `candidates` is the refscan candidate set (transitive input closure
/// ∪ all declared output paths — see `collect_outputs`). Every NAR byte
/// the walk generates — framing, entry names, symlink targets, and
/// regular-file contents — goes through the [`RefScanSink`] alongside
/// the SHA-256 (see [`WalkState::absorb`]), so the scanner sees exactly
/// the byte sequence `dump_path_streaming` would emit and the resolved
/// reference list (sorted by [`CandidateSet::resolve`]) matches what
/// the retired separate pre-scan pass would have found.
// r[impl builder.upload.references-scanned+2]
pub(super) fn fused_walk_output(
    upper_store: &Path,
    basename: &str,
    candidates: &CandidateSet,
) -> Result<WalkedOutput, UploadError> {
    let store_path = format!("/nix/store/{basename}");
    let parsed = StorePath::parse(&store_path).map_err(|e| UploadError::UploadExhausted {
        path: store_path.clone(),
        source: tonic::Status::invalid_argument(format!(
            "output store path {store_path:?} from overlay upper is malformed: {e}"
        )),
    })?;

    let root = upper_store.join(basename);
    let mut state = WalkState {
        hasher: Sha256::new(),
        scanner: RefScanSink::new(candidates.hashes()),
        nar_size: 0,
        chunk_manifest: Vec::new(),
        directories: HashMap::new(),
        chunk_sources: HashMap::new(),
    };

    // NAR magic, then the recursive node walk.
    state.write_wire_str("nix-archive-1");
    let walked = walk_node(&mut state, &root, 0).map_err(|e| UploadError::UploadExhausted {
        path: store_path.clone(),
        source: nar_err_to_status(&root, e),
    })?;

    let root_node = castore::RootNode {
        node: Some(match walked {
            WalkedNode::Dir { digest, .. } => castore::root_node::Node::DirDigest(digest.to_vec()),
            WalkedNode::File {
                digest,
                size,
                executable,
            } => castore::root_node::Node::File(castore::FileEntry {
                name: Vec::new(),
                digest: digest.to_vec(),
                size,
                executable,
            }),
            WalkedNode::Symlink { target } => {
                castore::root_node::Node::Symlink(castore::SymlinkEntry {
                    name: Vec::new(),
                    target,
                })
            }
        }),
    };

    let WalkState {
        hasher,
        scanner,
        nar_size,
        chunk_manifest,
        directories,
        chunk_sources,
    } = state;
    let references = candidates.resolve(&scanner.into_found());

    Ok(WalkedOutput {
        basename: basename.to_string(),
        store_path,
        parsed,
        nar_hash: hasher.finalize().into(),
        nar_size,
        references,
        root_node,
        chunk_manifest,
        directories,
        chunk_sources,
    })
}

/// Recursive node walk. Emits the node's NAR framing into `state`,
/// chunks regular-file contents, builds `Directory` bodies bottom-up,
/// and returns the castore entry the parent embeds.
///
/// Mirrors `rio_nix::nar::fs::stream_node` byte-for-byte on the framing
/// side (same type tags, same sorted-entry order, same depth/entry
/// limits, same unsupported-file-type rejection).
fn walk_node(state: &mut WalkState, path: &Path, depth: usize) -> Result<WalkedNode, NarError> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    state.write_wire_str("(");
    state.write_wire_str("type");

    let node = if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|os_str| {
            NarError::Io(std::io::Error::other(format!(
                "symlink target is not valid UTF-8: {os_str:?}"
            )))
        })?;
        state.write_wire_str("symlink");
        state.write_wire_str("target");
        state.write_wire_str(&target);
        WalkedNode::Symlink {
            target: target.into_bytes(),
        }
    } else if metadata.is_dir() {
        state.write_wire_str("directory");
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::TooManyEntries(entries.len()));
        }
        // Canonical NAR order: byte-lexicographic by entry name.
        entries.sort_by_key(|e| e.file_name());

        let mut dir = castore::Directory::default();
        // Recursive descendant count: immediate children + the sum of
        // every child directory's own count (`DirectoryEntry.size`).
        let mut descendants: u64 = 0;
        for entry in entries {
            let name = entry.file_name().into_string().map_err(|os_str| {
                NarError::Io(std::io::Error::other(format!(
                    "directory entry name is not valid UTF-8: {os_str:?}"
                )))
            })?;
            // r[impl builder.nar.entry-name-safety]
            // Producer-side guard: a name the NAR consumer (and the
            // store's Directory validation) would reject must not be
            // emitted in the first place.
            nar::validate_entry_name(&name)?;
            state.write_wire_str("entry");
            state.write_wire_str("(");
            state.write_wire_str("name");
            state.write_wire_str(&name);
            state.write_wire_str("node");
            let child = walk_node(state, &entry.path(), depth + 1)?;
            state.write_wire_str(")");

            descendants += 1;
            let name = name.into_bytes();
            match child {
                WalkedNode::File {
                    digest,
                    size,
                    executable,
                } => dir.files.push(castore::FileEntry {
                    name,
                    digest: digest.to_vec(),
                    size,
                    executable,
                }),
                WalkedNode::Dir { digest, size } => {
                    descendants += size;
                    dir.directories.push(castore::DirectoryEntry {
                        name,
                        digest: digest.to_vec(),
                        size,
                    });
                }
                WalkedNode::Symlink { target } => {
                    dir.symlinks.push(castore::SymlinkEntry { name, target });
                }
            }
        }
        // Canonical encoding = default prost field-order encode of a
        // Directory whose lists are sorted by name (they are — entries
        // were pushed in sorted walk order). Matches
        // `rio_store::castore::build` / `r[store.castore.canonical-encoding]`.
        let body = dir.encode_to_vec();
        let digest = *blake3::hash(&body).as_bytes();
        state.directories.entry(digest).or_insert(dir);
        WalkedNode::Dir {
            digest,
            size: descendants,
        }
    } else if metadata.file_type().is_file() {
        use std::os::unix::fs::PermissionsExt;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let len = metadata.len();

        state.write_wire_str("regular");
        if executable {
            state.write_wire_str("executable");
            state.write_wire_str("");
        }
        state.write_wire_str("contents");
        // Length prefix, then the streamed contents, then pad-to-8.
        state.absorb(&len.to_le_bytes());
        let digest = chunk_file(state, path, len)?;
        let pad = padding_len(len as usize);
        if pad > 0 {
            state.absorb(&ZERO_PAD[..pad]);
        }
        WalkedNode::File {
            digest,
            size: len,
            executable,
        }
    } else {
        // FIFO/socket/device — same rejection as dump_path_streaming.
        return Err(NarError::UnsupportedFileType(path.to_path_buf()));
    };

    state.write_wire_str(")");
    Ok(node)
}

/// Stream one regular file's contents through the fused sinks:
/// SHA-256 and refscan (via `state.absorb`), whole-file BLAKE3 (the
/// castore `FileEntry.digest`), and FastCDC boundary detection
/// (per-chunk BLAKE3 plus `(offset, len)` source records). One
/// `read()` loop; every consumer sees the same bytes.
///
/// Chunks are per-file-aligned: the FastCDC stream restarts at every
/// file, so a chunk never spans two files and each file's contiguous
/// `chunk_manifest` run sums to exactly `len`. The server's chunk-run
/// validation rejects anything else.
fn chunk_file(state: &mut WalkState, path: &Path, len: u64) -> Result<[u8; 32], NarError> {
    let mut file = std::fs::File::open(path)?;
    let mut file_hasher = blake3::Hasher::new();

    // FastCDC v2020 streaming: feed the file through a Read adapter
    // that tees every byte into the NAR-stream sinks + the whole-file
    // hasher as the chunker pulls it. `StreamCDC` yields
    // `(offset, length, data)` per content-defined chunk; the data is
    // hashed and dropped (the send phase re-reads from disk).
    //
    // An empty file yields zero chunks (fastcdc returns `Error::Empty`
    // → the iterator ends) — a zero-length contiguous run summing to a
    // zero `FileEntry.size`, which the server accepts.
    //
    // Chunks are accumulated locally and folded into `state` after the
    // iterator (and the `TeeReader`'s `&mut` over the hash/scan half of
    // `state`) is dropped — the borrow checker can't see that the tee
    // and the chunk lists touch disjoint fields.
    let mut total_read: u64 = 0;
    let mut chunks: Vec<([u8; 32], u64, u32)> = Vec::new();
    {
        let tee = TeeReader {
            inner: &mut file,
            state,
            file_hasher: &mut file_hasher,
            total: &mut total_read,
            limit: len,
        };
        for result in fastcdc::v2020::StreamCDC::new(tee, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
            let chunk = result.map_err(|e| match e {
                fastcdc::v2020::Error::IoError(io) => NarError::Io(io),
                other => NarError::Io(std::io::Error::other(format!("fastcdc: {other}"))),
            })?;
            let digest = *blake3::hash(&chunk.data).as_bytes();
            chunks.push((digest, chunk.offset, chunk.length as u32));
        }
    }
    for (digest, offset, clen) in chunks {
        state.chunk_manifest.push(ChunkRef {
            hash: digest.to_vec(),
            size: clen,
        });
        state.chunk_sources.entry(digest).or_insert(ChunkSource {
            file: path.to_path_buf(),
            offset,
            len: clen,
        });
    }

    // Read-during-write detection: the NAR length prefix was already
    // emitted from `symlink_metadata().len()`. If the file shrank or
    // grew between the stat and the read loop, the framing no longer
    // describes the bytes — fail loud (same contract as
    // `dump_path_streaming`). The TeeReader caps reads at `len` so a
    // GROWN file can't over-feed the sinks; a shrunk file under-feeds.
    if total_read != len {
        return Err(NarError::Io(std::io::Error::other(format!(
            "file {path:?} changed during fused walk: stat said {len} bytes, read {total_read}. \
             Is the overlay upper being mutated?"
        ))));
    }

    Ok(*file_hasher.finalize().as_bytes())
}

/// `Read` adapter that tees every byte pulled by the FastCDC chunker
/// into the NAR-stream accumulators (SHA-256 + refscan + nar_size) and
/// the whole-file BLAKE3, and enforces the stat-time length cap.
struct TeeReader<'a, R> {
    inner: R,
    state: &'a mut WalkState,
    file_hasher: &'a mut blake3::Hasher,
    total: &'a mut u64,
    limit: u64,
}

impl<R: Read> Read for TeeReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Cap at the stat-time length so a file that GREW mid-walk
        // can't push extra bytes into the NAR-stream sinks past the
        // already-emitted length prefix.
        let remaining = self.limit - *self.total;
        if remaining == 0 {
            return Ok(0);
        }
        let cap = (buf.len() as u64).min(remaining) as usize;
        let n = self.inner.read(&mut buf[..cap])?;
        if n > 0 {
            self.state.absorb(&buf[..n]);
            self.file_hasher.update(&buf[..n]);
            *self.total += n as u64;
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Send phase: HasChunks → Begin → Chunk frames
// ---------------------------------------------------------------------------

/// True iff the store rejected `PutPathChunked` because it cannot
/// accept chunked uploads at all (no chunk backend configured, or a
/// pre-ADR-022 store binary that doesn't implement the RPC). The
/// caller falls back to the legacy `PutPath`/`PutPathBatch` path.
///
/// Deliberately narrow: other `FailedPrecondition` reasons (NAR-hash
/// mismatch, refs mismatch, incomplete stream) indicate a builder-side
/// walk bug that the legacy path would mask — those propagate as
/// errors instead.
pub(super) fn is_chunked_unsupported(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::Unimplemented
        || (status.code() == tonic::Code::FailedPrecondition
            && status
                .message()
                .contains(rio_proto::CHUNKED_REQUIRES_BACKEND_MSG))
}

/// Global first-occurrence digest order across all outputs' chunk
/// manifests: walk `outputs[0].chunk_manifest` start to end, then
/// `outputs[1]`, …, appending each digest the first time it is seen.
/// `Begin.novel` MUST be the `HasChunks`-absent subset of this list in
/// this order, and the `Chunk` frames MUST arrive in exactly that
/// order — the server recomputes the same order and rejects any other.
fn global_first_occurrence(outputs: &[WalkedOutput]) -> Vec<[u8; 32]> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for o in outputs {
        for c in &o.chunk_manifest {
            let digest: [u8; 32] = c
                .hash
                .as_slice()
                .try_into()
                .expect("walk emits 32-byte BLAKE3 digests");
            if seen.insert(digest) {
                order.push(digest);
            }
        }
    }
    order
}

/// Probe `HasChunks` for `digests` and return the absent subset in the
/// input order. An empty input skips the RPC entirely (an output with
/// no regular files has nothing to probe).
async fn probe_novel(
    chunk_client: &ChunkServiceClient<Channel>,
    digests: &[[u8; 32]],
) -> Result<Vec<[u8; 32]>, tonic::Status> {
    if digests.is_empty() {
        return Ok(Vec::new());
    }
    let mut client = chunk_client.clone();
    let mut req = tonic::Request::new(HasChunksRequest {
        digests: digests.iter().map(|d| d.to_vec()).collect(),
    });
    rio_proto::interceptor::inject_current(req.metadata_mut());
    let bitmap = rio_common::grpc::with_timeout_status(
        "HasChunks",
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        client.has_chunks(req),
    )
    .await?
    .into_inner()
    .bitmap;
    // Bit i set ⇔ digests[i] is durably present. Absent ⇒ novel. A
    // short bitmap treats the missing tail as absent (upload them; the
    // server-side put is idempotent).
    Ok(digests
        .iter()
        .enumerate()
        .filter(|(i, _)| bitmap.get(i / 8).is_none_or(|b| b & (1 << (i % 8)) == 0))
        .map(|(_, d)| *d)
        .collect())
}

/// Assemble the `Begin` frame from the walked outputs.
///
/// `directories` is the union of every output's body set, deduplicated
/// by digest. `novel` is the caller-computed `HasChunks`-absent subset
/// in global first-occurrence order. `input_closure` is
/// `WorkAssignment.input_closure` passed through verbatim — the store
/// asserts `blake3(sorted(input_closure))` matches the assignment
/// token's `input_closure_digest` claim.
fn assemble_begin(
    outputs: &[WalkedOutput],
    novel: &[[u8; 32]],
    deriver: &str,
    input_closure: &[String],
) -> PutPathChunkedBegin {
    let mut seen_dirs = HashSet::new();
    let mut directories = Vec::new();
    for o in outputs {
        for (digest, body) in &o.directories {
            if seen_dirs.insert(*digest) {
                directories.push(body.clone());
            }
        }
    }
    PutPathChunkedBegin {
        deriver: deriver.to_string(),
        outputs: outputs
            .iter()
            .map(|o| ChunkedOutputHeader {
                store_path: o.store_path.clone(),
                nar_hash: o.nar_hash.to_vec(),
                nar_size: o.nar_size,
                refs: o.references.clone(),
                root_node: Some(o.root_node.clone()),
                chunk_manifest: o.chunk_manifest.clone(),
            })
            .collect(),
        directories,
        novel: novel.iter().map(|d| d.to_vec()).collect(),
        input_closure: input_closure.to_vec(),
    }
}

/// One `PutPathChunked` attempt: `HasChunks` probe → `Begin` → one
/// `Chunk` frame per novel digest → half-close → await the response.
///
/// The chunk bytes are re-read from the overlay upper on the blocking
/// pool (`spawn_blocking` + `blocking_send`, same shape as the legacy
/// `spawn_dump_tee`). Each body's BLAKE3 is re-verified against the
/// walk-time digest before it is sent — a mismatch means the overlay
/// was mutated between walk and send and the server would reject the
/// frame anyway; failing client-side names the file.
async fn put_path_chunked_once(
    store_client: &StoreServiceClient<Channel>,
    chunk_client: &ChunkServiceClient<Channel>,
    outputs: &[WalkedOutput],
    assignment_token: &str,
    deriver: &str,
    input_closure: &[String],
) -> Result<bool, tonic::Status> {
    // Re-probe on every attempt: a retry after a mid-stream failure
    // must not re-send chunks the failed attempt already made durable,
    // and a chunk GC'd since the last attempt must be re-sent.
    let all_digests = global_first_occurrence(outputs);
    let novel = probe_novel(chunk_client, &all_digests).await?;
    let novel_count = novel.len() as u64;
    let dedup_count = (all_digests.len() - novel.len()) as u64;

    let begin = assemble_begin(outputs, &novel, deriver, input_closure);

    // Source map for the producer task: every novel digest's on-disk
    // location. The walk records the first occurrence per digest per
    // output; across outputs the first output containing the digest
    // wins (the bytes are identical by content-addressing).
    let mut sources: HashMap<[u8; 32], ChunkSource> = HashMap::new();
    for o in outputs {
        for (d, s) in &o.chunk_sources {
            sources.entry(*d).or_insert_with(|| s.clone());
        }
    }

    let (tx, rx) = mpsc::channel::<PutPathChunkedRequest>(STREAM_CHANNEL_BUF);

    // Producer: Begin first, then the novel chunk bodies in `novel`
    // order. Sync disk reads → spawn_blocking; `blocking_send` gives
    // backpressure against the gRPC send window.
    let novel_owned = novel.clone();
    let producer = tokio::task::spawn_blocking(move || -> Result<(), tonic::Status> {
        if tx
            .blocking_send(PutPathChunkedRequest {
                msg: Some(put_path_chunked_request::Msg::Begin(begin)),
            })
            .is_err()
        {
            // Receiver dropped = the RPC already failed; its status is
            // what the caller reports.
            return Ok(());
        }
        let mut reader = ChunkSourceReader::default();
        for digest in &novel_owned {
            let src = sources.get(digest).ok_or_else(|| {
                tonic::Status::internal(format!(
                    "novel digest {} has no recorded source (walk bug)",
                    hex::encode(digest)
                ))
            })?;
            let data = reader.read(src)?;
            let actual = *blake3::hash(&data).as_bytes();
            if actual != *digest {
                return Err(tonic::Status::data_loss(format!(
                    "chunk at {}+{} ({} bytes) hashed to {} during send but {} during the walk; \
                     overlay upper mutated after the build?",
                    src.file.display(),
                    src.offset,
                    src.len,
                    hex::encode(actual),
                    hex::encode(digest),
                )));
            }
            if tx
                .blocking_send(PutPathChunkedRequest {
                    msg: Some(put_path_chunked_request::Msg::Chunk(PutPathChunkedChunk {
                        digest: digest.to_vec(),
                        data,
                    })),
                })
                .is_err()
            {
                return Ok(());
            }
        }
        // tx drops here → channel closes → tonic half-closes the stream
        // → the server's verify walk sees end-of-stream.
        Ok(())
    });

    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    attach_assignment_token(&mut req, assignment_token)?;

    let mut client = store_client.clone();
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPathChunked",
        chunked_stream_timeout(outputs.len()),
        client.put_path_chunked(req),
    )
    .await;

    // Join the producer. Same three-case analysis as the legacy
    // `do_upload_streaming`: finished / observed rx-drop / parked in a
    // sync read. Only DUMP_JOIN_SLACK is waited — the gRPC budget was
    // already spent concurrently above.
    let producer_result = await_dump_after_rx_drop("chunked producer", producer).await?;

    // Error priority. A producer failure (send-phase re-read ENOENT,
    // truncation, the BLAKE3 mismatch that names a mutated file) drops
    // `tx`; the server sees the stream end before `novel` is exhausted
    // and returns a generic incomplete-stream `FailedPrecondition` — a
    // downstream symptom of the producer's root cause. Always log the
    // producer's diagnostic so it survives whichever error wins, and
    // when the server's verdict is that symptom, return the producer's
    // error instead (it carries the file + offset the operator needs).
    // Other server errors (Unavailable, InvalidArgument, a transport
    // failure) are independent faults and keep priority.
    if let Err(producer_err) = &producer_result {
        tracing::warn!(error = %producer_err, "chunked upload producer failed");
        if let Err(status) = &put_result
            && status.code() == tonic::Code::FailedPrecondition
        {
            return Err(producer_err.clone());
        }
    }
    let resp = put_result?;
    producer_result?;

    metrics::counter!("rio_builder_upload_chunks_total", "kind" => "novel").increment(novel_count);
    metrics::counter!("rio_builder_upload_chunks_total", "kind" => "deduped")
        .increment(dedup_count);
    Ok(resp.into_inner().created)
}

/// Stream timeout for one `PutPathChunked` attempt. Scales with output
/// count like the batch path (`batch_stream_timeout`) — the stream
/// carries every output's novel chunks serially.
fn chunked_stream_timeout(n_outputs: usize) -> std::time::Duration {
    GRPC_STREAM_TIMEOUT * (n_outputs.min(rio_common::limits::MAX_BATCH_OUTPUTS) as u32).max(1)
}

/// Cached-handle reader for the producer's chunk re-reads. Consecutive
/// novel digests usually live in the same file (FastCDC emits them in
/// file order and `novel` preserves first-occurrence order), so keeping
/// the last `File` open avoids an open/close syscall pair per chunk.
#[derive(Default)]
struct ChunkSourceReader {
    open: Option<(PathBuf, std::fs::File)>,
}

impl ChunkSourceReader {
    fn read(&mut self, src: &ChunkSource) -> Result<Vec<u8>, tonic::Status> {
        let io_err = |e: std::io::Error| {
            tonic::Status::internal(format!(
                "re-reading chunk at {}+{}: {e}",
                src.file.display(),
                src.offset
            ))
        };
        if self.open.as_ref().is_none_or(|(p, _)| *p != src.file) {
            let f = std::fs::File::open(&src.file).map_err(io_err)?;
            self.open = Some((src.file.clone(), f));
        }
        let (_, f) = self.open.as_mut().expect("just set");
        f.seek(SeekFrom::Start(src.offset)).map_err(io_err)?;
        let mut buf = vec![0u8; src.len as usize];
        f.read_exact(&mut buf).map_err(io_err)?;
        Ok(buf)
    }
}

/// Upload all walked outputs via `PutPathChunked` with retry.
///
/// Returns `Ok(results)` on commit, `Err(ChunkedUnsupported)` if the
/// store cannot accept chunked uploads (caller falls back to the
/// legacy path), or `Err(UploadExhausted)` after the retry budget.
/// Each retry re-probes `HasChunks` and re-sends only the still-missing
/// chunks (`r[store.put.idempotent]` re-drive semantics).
// r[impl builder.upload.batch+2]
#[instrument(skip_all, fields(outputs = outputs.len()))]
pub(super) async fn upload_outputs_chunked(
    store_client: &StoreServiceClient<Channel>,
    chunk_client: &ChunkServiceClient<Channel>,
    outputs: &[WalkedOutput],
    assignment_token: &str,
    deriver: &str,
    input_closure: &[String],
) -> Result<Vec<ValidatedPathInfo>, ChunkedUploadError> {
    use super::common::{MAX_UPLOAD_RETRIES, UPLOAD_BACKOFF};

    let total_novel_candidates: usize = outputs.iter().map(|o| o.chunk_manifest.len()).sum();
    tracing::info!(
        outputs = outputs.len(),
        chunk_manifest_entries = total_novel_candidates,
        "uploading build outputs (PutPathChunked)"
    );

    let mut last_err = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(UPLOAD_BACKOFF.duration(attempt - 1)).await;
        }
        match put_path_chunked_once(
            store_client,
            chunk_client,
            outputs,
            assignment_token,
            deriver,
            input_closure,
        )
        .await
        {
            Ok(created) => {
                let mut results = Vec::with_capacity(outputs.len());
                for o in outputs {
                    metrics::counter!("rio_builder_uploads_total", "status" => "success")
                        .increment(1);
                    metrics::counter!("rio_builder_upload_bytes_total").increment(o.nar_size);
                    results.push(
                        uploaded_info(
                            o.parsed.clone(),
                            o.nar_hash,
                            o.nar_size,
                            o.references.clone(),
                            deriver,
                        )
                        .map_err(ChunkedUploadError::Upload)?,
                    );
                }
                tracing::info!(
                    outputs = results.len(),
                    created,
                    "chunked upload committed atomically"
                );
                return Ok(results);
            }
            Err(e) if is_chunked_unsupported(&e) => {
                tracing::warn!(
                    error = %e,
                    "store cannot accept PutPathChunked; falling back to legacy upload"
                );
                return Err(ChunkedUploadError::Unsupported);
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "chunked upload attempt failed");
                last_err = Some(e);
            }
        }
    }

    metrics::counter!("rio_builder_uploads_total", "status" => "exhausted")
        .increment(outputs.len() as u64);
    Err(ChunkedUploadError::Upload(UploadError::UploadExhausted {
        path: "<chunked>".into(),
        source: last_err.expect("retry loop ran ≥1 times; each failure sets last_err"),
    }))
}

/// Outcome of the chunked upload attempt that the caller dispatches on.
#[derive(Debug)]
pub(super) enum ChunkedUploadError {
    /// The store cannot accept chunked uploads (no chunk backend, or
    /// the RPC is unimplemented). Fall back to the legacy path.
    Unsupported,
    /// A real failure after the retry budget — propagate.
    Upload(UploadError),
}

/// Run the fused walk for every output on the blocking pool, bounded by
/// the same per-output stream budget as the legacy ref-scan pre-pass.
/// Serial — the walk is disk-read bound and N is `MAX_BATCH_OUTPUTS`-small.
pub(super) async fn walk_all_outputs(
    upper_store: &Path,
    basenames: &[String],
    candidates: &Arc<CandidateSet>,
) -> Result<Vec<WalkedOutput>, UploadError> {
    let mut walked = Vec::with_capacity(basenames.len());
    for b in basenames {
        let upper = upper_store.to_path_buf();
        let basename = b.clone();
        let cands = Arc::clone(candidates);
        let store_path = format!("/nix/store/{b}");
        // Bounded join: a blocking thread parked in open()/read() (FIFO
        // in $out, wedged overlay) never returns and tokio cannot abort
        // it; without the timeout this await would hang the worker
        // forever. Same guard as the legacy scan_references.
        let out = await_dump_bounded(
            "fused walk",
            GRPC_STREAM_TIMEOUT,
            tokio::task::spawn_blocking(move || fused_walk_output(&upper, &basename, &cands)),
        )
        .await
        .map_err(|source| UploadError::UploadExhausted {
            path: store_path,
            source,
        })??;
        // Emitted here (not inside the spawn_blocking walk) so tests
        // using `metrics::set_default_local_recorder` — which installs
        // a *thread-local* recorder — observe it. This is the single
        // emission site for the references-count histogram.
        metrics::histogram!("rio_builder_upload_references_count")
            .record(out.references.len() as f64);
        walked.push(out);
    }
    Ok(walked)
}

// r[verify builder.upload.fused-walk]
// r[verify builder.upload.chunked-manifest]
#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture tree exercising every NAR node kind: nested dirs, an
    /// executable, a symlink, two byte-identical files, an empty file,
    /// and a file large enough to FastCDC into multiple chunks.
    fn fixture_tree(root: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("share/doc")).unwrap();
        std::fs::write(root.join("bin/tool"), b"#!/bin/sh\necho hello\n").unwrap();
        std::fs::set_permissions(
            root.join("bin/tool"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(root.join("share/doc/README"), vec![0xA5u8; 4096]).unwrap();
        std::fs::write(root.join("share/doc/COPY"), vec![0xA5u8; 4096]).unwrap();
        std::fs::write(root.join("empty"), b"").unwrap();
        // 600 KiB of varied bytes → several FastCDC chunks (max 256 KiB).
        std::fs::write(
            root.join("blob"),
            rio_test_support::fixtures::pseudo_random_bytes(7, 600 * 1024),
        )
        .unwrap();
        std::os::unix::fs::symlink("bin/tool", root.join("default")).unwrap();
    }

    fn walk_fixture() -> (tempfile::TempDir, WalkedOutput, Vec<u8>) {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        let basename = rio_test_support::fixtures::test_store_basename("fused");
        fixture_tree(&store_dir.join(&basename));
        let walked = fused_walk_output(
            &store_dir,
            &basename,
            &CandidateSet::from_paths(std::iter::empty::<&str>()),
        )
        .expect("walk succeeds");
        let oracle = nar::dump_path(&store_dir.join(&basename)).expect("dump_path oracle");
        (tmp, walked, oracle)
    }

    /// THE invariant: the fused walk's NAR hash/size over generated
    /// framing must equal SHA-256 of the byte stream `dump_path` (the
    /// eager oracle, byte-identical to `dump_path_streaming`) produces
    /// for the same tree. A framing divergence here means the store's
    /// reconstruction would reject every upload.
    #[test]
    fn fused_walk_matches_dump_path_oracle() {
        let (_tmp, walked, oracle) = walk_fixture();
        assert_eq!(walked.nar_size, oracle.len() as u64, "nar_size");
        let expected: [u8; 32] = Sha256::digest(&oracle).into();
        assert_eq!(walked.nar_hash, expected, "nar_hash");
    }

    /// The FastCDC parameters mirrored at the top of this module MUST
    /// equal `rio-store/src/chunker.rs`'s (the source of truth,
    /// reachable here via the dev-dependency). The production code
    /// cannot import them — rio-builder does not link the store's
    /// server dependency tree — so this test is the only thing that
    /// catches a drift, and a drift in `CHUNK_MIN`/`CHUNK_AVG` is
    /// silent in production: every upload still verifies and commits,
    /// but builder-chunked and store-chunked content stop sharing
    /// chunk digests.
    #[test]
    fn chunker_constants_match_rio_store() {
        assert_eq!(CHUNK_MIN, rio_store::chunker::CHUNK_MIN, "CHUNK_MIN");
        assert_eq!(CHUNK_AVG, rio_store::chunker::CHUNK_AVG, "CHUNK_AVG");
        assert_eq!(CHUNK_MAX, rio_store::chunker::CHUNK_MAX, "CHUNK_MAX");
    }

    /// Per-file alignment: each regular file's contiguous chunk run
    /// sums to exactly that file's size, in canonical NAR walk order.
    /// The server's chunk-run validation rejects anything else.
    #[test]
    fn fused_walk_chunks_are_per_file_aligned() {
        let (_tmp, walked, _) = walk_fixture();
        // Files in canonical walk order: bin/tool (21), blob (600 KiB),
        // empty (0 chunks), share/doc/COPY (4096), share/doc/README
        // (4096). Symlinks and dirs contribute no chunks.
        let file_sizes = [21u64, 600 * 1024, 4096, 4096];
        let mut cursor = 0usize;
        for size in file_sizes {
            let mut got = 0u64;
            while got < size {
                let c = &walked.chunk_manifest[cursor];
                cursor += 1;
                got += u64::from(c.size);
            }
            assert_eq!(got, size, "chunk run must sum to exactly the file size");
        }
        assert_eq!(
            cursor,
            walked.chunk_manifest.len(),
            "no chunks beyond the per-file runs"
        );
        // The 600 KiB blob must have produced more than one chunk
        // (CHUNK_MAX is 256 KiB) — otherwise this test isn't actually
        // exercising multi-chunk alignment.
        assert!(
            walked.chunk_manifest.len() > file_sizes.len(),
            "the large blob must FastCDC into multiple chunks"
        );
        for c in &walked.chunk_manifest {
            assert!(c.size > 0 && c.size as usize <= CHUNK_MAX, "chunk bounds");
        }
    }

    /// The two byte-identical files share one castore file digest and
    /// one chunk digest set; the empty file contributes zero chunks but
    /// still appears in the Directory body.
    #[test]
    fn fused_walk_dedupes_identical_content() {
        let (_tmp, walked, _) = walk_fixture();
        // README == COPY → their chunk digests are identical → the
        // distinct-source map has fewer entries than the manifest.
        let distinct: HashSet<&[u8]> = walked
            .chunk_manifest
            .iter()
            .map(|c| c.hash.as_slice())
            .collect();
        assert!(
            distinct.len() < walked.chunk_manifest.len(),
            "duplicate file contents must produce repeated manifest digests"
        );
        // Every distinct digest has a recorded source for the send phase.
        assert_eq!(distinct.len(), walked.chunk_sources.len());
    }

    /// Re-reading a chunk source from disk yields bytes that hash to
    /// the digest recorded during the walk — the send phase depends on
    /// this (it re-reads instead of buffering).
    #[test]
    fn chunk_sources_reread_to_recorded_digest() {
        let (_tmp, walked, _) = walk_fixture();
        let mut reader = ChunkSourceReader::default();
        for (digest, src) in &walked.chunk_sources {
            let data = reader.read(src).expect("re-read");
            assert_eq!(
                blake3::hash(&data).as_bytes(),
                digest,
                "re-read bytes must hash to the walk-time digest"
            );
        }
    }

    /// The walk finds references embedded in file contents AND in
    /// symlink targets — the full NAR byte sequence (framing included)
    /// goes through the scanner.
    #[test]
    fn fused_walk_scans_references_in_contents_and_symlink_targets() {
        let dep_a = "/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-glibc";
        let dep_b = "/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-only-in-symlink";
        let dep_c = "/nix/store/0000000000000000000000000000000z-absent";
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        let basename = rio_test_support::fixtures::test_store_basename("refs");
        let root = store_dir.join(&basename);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("conf"), format!("prefix={dep_a}\n")).unwrap();
        std::os::unix::fs::symlink(format!("{dep_b}/bin/sh"), root.join("sh")).unwrap();
        let walked = fused_walk_output(
            &store_dir,
            &basename,
            &CandidateSet::from_paths([dep_a, dep_b, dep_c]),
        )
        .expect("walk succeeds");
        assert_eq!(
            walked.references,
            vec![dep_a.to_string(), dep_b.to_string()],
            "refs in file contents + symlink targets found, sorted; absent dep not over-reported"
        );
    }

    /// The walk must reject anything that is not a regular file,
    /// symlink, or directory (same contract as `dump_path_streaming`).
    /// The motivating case is a FIFO (`mkfifo $out/pipe` in a hostile
    /// derivation), which would hang `File::open` in `open(O_RDONLY)`
    /// forever; the test binds a Unix-domain socket instead because it
    /// exercises the same metadata-type rejection branch without
    /// needing an extra dependency to create a FIFO.
    #[test]
    fn fused_walk_rejects_unsupported_file_type() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        let basename = rio_test_support::fixtures::test_store_basename("unsupported");
        let root = store_dir.join(&basename);
        std::fs::create_dir_all(&root).unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(root.join("sock")).unwrap();
        let err = fused_walk_output(
            &store_dir,
            &basename,
            &CandidateSet::from_paths(std::iter::empty::<&str>()),
        )
        .expect_err("non-regular file must be rejected");
        assert!(matches!(err, UploadError::UploadExhausted { .. }));
    }

    /// `novel` ordering: global first occurrence across outputs — every
    /// digest appears once, in the order it is first encountered when
    /// walking output 0's manifest then output 1's.
    #[test]
    fn global_first_occurrence_orders_across_outputs() {
        let mk = |digests: &[u8]| WalkedOutput {
            basename: String::new(),
            store_path: String::new(),
            parsed: StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x").unwrap(),
            nar_hash: [0; 32],
            nar_size: 0,
            references: vec![],
            root_node: castore::RootNode { node: None },
            chunk_manifest: digests
                .iter()
                .map(|b| ChunkRef {
                    hash: vec![*b; 32],
                    size: 1,
                })
                .collect(),
            directories: HashMap::new(),
            chunk_sources: HashMap::new(),
        };
        // Output 0: A B A. Output 1: C B D.
        let order = global_first_occurrence(&[mk(&[1, 2, 1]), mk(&[3, 2, 4])]);
        assert_eq!(
            order,
            vec![[1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]],
            "first-occurrence order: A B C D"
        );
    }

    /// The capability-fallback matcher is narrow: only `Unimplemented`
    /// or the specific no-chunk-backend `FailedPrecondition` trigger
    /// the legacy fallback. A verification `FailedPrecondition` (NAR
    /// hash mismatch — a builder bug) must NOT be masked by falling
    /// back.
    #[test]
    fn is_chunked_unsupported_matcher() {
        assert!(is_chunked_unsupported(&tonic::Status::unimplemented(
            "no such method"
        )));
        assert!(is_chunked_unsupported(&tonic::Status::failed_precondition(
            format!(
                "{}; this store is inline-only",
                rio_proto::CHUNKED_REQUIRES_BACKEND_MSG
            )
        )));
        assert!(!is_chunked_unsupported(
            &tonic::Status::failed_precondition("NAR hash mismatch for /nix/store/...")
        ));
        assert!(!is_chunked_unsupported(&tonic::Status::unavailable(
            rio_proto::CHUNKED_REQUIRES_BACKEND_MSG
        )));
    }
}
