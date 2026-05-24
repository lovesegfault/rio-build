//! `Begin` validation for `PutPathChunked` (ADR-022 §6.2).
//!
//! Everything here runs BEFORE any placeholder claim, S3 write, or
//! verify-driver work; every violation maps to `INVALID_ARGUMENT`
//! (except the `expected_outputs` membership gate, which is
//! `PERMISSION_DENIED` to match legacy `PutPath`). The output,
//! [`ValidatedBegin`], carries the per-output NAR segment lists the
//! verify task replays — the structural walk happens exactly once.

use std::collections::{HashMap, HashSet};

use prost::Message;
use tonic::Status;
use tracing::warn;

use rio_common::limits::{
    MAX_BATCH_OUTPUTS, MAX_DIR_NODES, MAX_INPUT_CLOSURE, MAX_NAR_SIZE, MAX_REFERENCES,
};
use rio_nix::refscan::CandidateSet;
use rio_nix::store_path::StorePath;
use rio_proto::castore::{Directory, RootNode, root_node};
use rio_proto::types::{NarEntryKind as ProtoNarEntryKind, NarIndexEntry, PutPathChunkedBegin};
use rio_proto::validated::ValidatedPathInfo;

use crate::chunker::CHUNK_MAX;
use crate::manifest::{self, Manifest, ManifestEntry};

/// Maximum NAR framing bytes materialized per output during validation.
///
/// The verify task replays the framing from [`NarSegment::Framing`]
/// buffers built here, so the framing IS held in memory for the
/// stream's lifetime (unlike chunk bodies, which stay one-in-flight).
/// Framing is ~100–200 bytes per tree entry; 64 MiB covers ~400k
/// entries — an order of magnitude past the largest real store paths
/// (glibc-locales ~20k files). A `Begin` whose tree exceeds this is
/// pathological; the cap turns an OOM into `INVALID_ARGUMENT`. The
/// handler charges these bytes against `nar_bytes_budget`.
pub(super) const MAX_NAR_FRAMING_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum tree entries visited per output walk. Matches rio-nix's
/// `MAX_DIRECTORY_ENTRIES` (the per-directory reader bound) applied to
/// the whole tree: deduplicated `Directory` bodies make an
/// exponentially-expanding DAG cheap to *send*, but the NAR it denotes
/// is materialized entry-by-entry, so the walk must be bounded by
/// count, not by `len(directories)`.
const MAX_WALK_ENTRIES: usize = 1_048_576;

/// Maximum directory nesting depth. Matches rio-nix's `MAX_NAR_DEPTH`.
const MAX_WALK_DEPTH: usize = 256;

/// Maximum NAR entry name length. Matches rio-nix's `MAX_NAME_LEN`.
const MAX_NAME_LEN: usize = 256;

/// Maximum symlink target length. Matches rio-nix's `MAX_TARGET_LEN`.
const MAX_TARGET_LEN: usize = 4096;

/// One piece of an output's canonical NAR byte stream.
#[derive(Debug)]
pub(super) enum NarSegment {
    /// Literal framing bytes (magic, parens, type tags, entry names,
    /// symlink targets, length prefixes, padding). Adjacent framing
    /// between two file-content runs is coalesced into one buffer,
    /// split at [`CHUNK_MAX`].
    ///
    /// Framing runs are persisted to the CAS as ordinary chunks (keyed
    /// by `digest = blake3(bytes)`) and interleaved into
    /// `manifest_data.chunk_list` at their positions, so the existing
    /// "concatenating the chunk list yields the NAR" invariant that
    /// `GetPath`, `nar_index::reassemble`, and the GC sweep all rely on
    /// holds for chunked uploads too. The builder never sends these —
    /// the server generates and uploads them itself.
    Framing { bytes: Vec<u8>, digest: [u8; 32] },
    /// A regular file's contents: the next `n_chunks` entries of this
    /// output's `chunk_manifest`. The verify task splices the chunk
    /// bodies in here, BLAKE3-hashes them, and rejects the upload if
    /// the result differs from `file_digest` — the builder-claimed
    /// `FileEntry.digest` that the commit transaction persists into
    /// `file_blobs` and that `ReadBlob`/`StatBlob`/`HasBlobs` later
    /// resolve content by. Without that check a compromised builder
    /// could register an arbitrary digest → its own bytes and have the
    /// store serve them for another path's file.
    FileContents {
        n_chunks: usize,
        file_digest: [u8; 32],
    },
}

/// One output of a validated `Begin`.
#[derive(Debug)]
pub(super) struct ValidatedOutput {
    /// Narinfo-shaped metadata. `nar_hash` is the CLAIMED hash (the
    /// commit path overwrites it with the computed one); `references`
    /// is the claimed, parsed, sorted, deduped ref list;
    /// `store_path_hash` is server-derived.
    pub info: ValidatedPathInfo,
    /// Ordered `(digest, len)` chunk list in canonical NAR walk order.
    pub chunk_manifest: Vec<([u8; 32], u32)>,
    /// The output's NAR byte stream as framing + content markers.
    pub segments: Vec<NarSegment>,
    /// Distinct `dir_digest`s reachable from `root_node` (sorted).
    /// Drives the `directories` refcount increment + `directory_paths`
    /// rows for this output.
    pub dir_digests: Vec<[u8; 32]>,
    /// Distinct `(file_digest, nar_offset, size)` (digest-sorted, first
    /// occurrence's offset wins). Drives the `file_blobs` rows.
    pub file_blobs: Vec<([u8; 32], u64, u64)>,
    /// Encoded `rio.castore.RootNode` for `nar_index.root_node`.
    pub root_node_encoded: Vec<u8>,
    /// Encoded `rio.types.NarIndex` for `nar_index.entries`.
    pub nar_index_entries: Vec<u8>,
    /// Serialized [`Manifest`] (the ordered chunk list) for
    /// `manifest_data.chunk_list`.
    pub chunk_list_bytes: Vec<u8>,
    /// Deduplicated `(hash, size)` chunk set referenced by this output
    /// (digest-sorted). Drives the per-manifest chunk refcount
    /// increment.
    pub unique_chunks: Vec<([u8; 32], u32)>,
}

/// A structurally-validated `Begin`.
#[derive(Debug)]
pub(super) struct ValidatedBegin {
    pub outputs: Vec<ValidatedOutput>,
    /// `Begin.novel` in wire order (== recomputed global-first-
    /// occurrence order over the novel subset).
    pub novel: Vec<[u8; 32]>,
    pub novel_set: HashSet<[u8; 32]>,
    /// digest → length agreed by every `chunk_manifest` occurrence.
    pub manifest_len: HashMap<[u8; 32], u32>,
    /// Validated `Directory` bodies keyed by their RECOMPUTED digest.
    pub directories: HashMap<[u8; 32], Directory>,
    /// Refscan candidate set: `input_closure ∪ {outputs[*].store_path}`.
    pub candidates: CandidateSet,
    /// `Σ manifest_len[d]` over `d ∉ novel` plus the materialized
    /// framing bytes — what the handler charges against
    /// `nar_bytes_budget` before claiming placeholders. Self-consistent
    /// with the attested tree, not attested itself; the verify task
    /// asserts the actual `cas::get` length per chunk.
    pub budget_bytes: u64,
}

/// Shorthand for the module's dominant error shape.
fn invalid(msg: impl Into<String>) -> Status {
    Status::invalid_argument(msg.into())
}

/// Validate a `Begin` against the assignment claims and the §6.2
/// bounds.
// r[impl store.put.chunked-bounds]
pub(super) fn validate_begin(
    begin: &PutPathChunkedBegin,
    claims: Option<&rio_auth::hmac::AssignmentClaims>,
) -> Result<ValidatedBegin, Status> {
    // --- Token binding -------------------------------------------------
    // The token's role gate is structural: `TokenRole` has exactly one
    // variant (`Builder`) and builders are exactly the callers this RPC
    // accepts, so any token that verified is role-accepted. The match
    // (not `if`) means adding a second role forces a decision here.
    if let Some(c) = claims {
        match c.role {
            rio_auth::hmac::TokenRole::Builder => {}
        }
    }

    // Deriver ↔ assignment binding. `claims.drv_hash` is the full .drv
    // store path for input-addressed derivations (the gateway populates
    // `DerivationNode.drv_hash` from `drv_path`) and an opaque modular
    // hash for CA derivations. Only the store-path form is comparable
    // to `Begin.deriver`; the CA case is authorized by the post-verify
    // CA-path recompute instead.
    if let Some(c) = claims
        && !begin.deriver.is_empty()
        && StorePath::parse(&c.drv_hash).is_ok()
        && begin.deriver != c.drv_hash
    {
        return Err(invalid(format!(
            "Begin.deriver {:?} does not match the assignment's derivation {:?}",
            begin.deriver, c.drv_hash
        )));
    }
    let deriver = if begin.deriver.is_empty() {
        None
    } else {
        Some(StorePath::parse(&begin.deriver).map_err(|e| invalid(format!("Begin.deriver: {e}")))?)
    };

    // --- input_closure -------------------------------------------------
    if begin.input_closure.len() > MAX_INPUT_CLOSURE {
        return Err(invalid(format!(
            "input_closure has {} entries, exceeds MAX_INPUT_CLOSURE {MAX_INPUT_CLOSURE}",
            begin.input_closure.len()
        )));
    }
    // blake3(sorted(input_closure)) == claims.input_closure_digest. An
    // empty claim digest means the scheduler didn't attest a closure
    // (legacy token / closure unavailable) — the refscan still runs
    // against the builder-supplied set, it just isn't attested.
    if let Some(c) = claims
        && !c.input_closure_digest.is_empty()
    {
        let mut sorted = begin.input_closure.clone();
        sorted.sort_unstable();
        let computed = rio_auth::hmac::AssignmentClaims::digest_input_closure(&sorted);
        if computed != c.input_closure_digest {
            return Err(invalid(
                "Begin.input_closure does not match the assignment's input_closure_digest",
            ));
        }
    }

    // --- Output headers ------------------------------------------------
    if begin.outputs.is_empty() {
        return Err(invalid("Begin.outputs must not be empty"));
    }
    if begin.outputs.len() > MAX_BATCH_OUTPUTS {
        return Err(invalid(format!(
            "Begin has {} outputs, exceeds MAX_BATCH_OUTPUTS {MAX_BATCH_OUTPUTS}",
            begin.outputs.len()
        )));
    }
    let mut output_paths: Vec<String> = Vec::with_capacity(begin.outputs.len());
    for o in &begin.outputs {
        if output_paths.iter().any(|p| p == &o.store_path) {
            return Err(invalid(format!(
                "duplicate output store_path {:?}",
                o.store_path
            )));
        }
        output_paths.push(o.store_path.clone());
    }

    // Refscan candidate set: input_closure ∪ output paths. Built before
    // the per-output loop so the refs ⊆ candidates check can use it.
    let candidate_paths: HashSet<&str> = begin
        .input_closure
        .iter()
        .map(String::as_str)
        .chain(output_paths.iter().map(String::as_str))
        .collect();
    let candidates =
        CandidateSet::from_paths(begin.input_closure.iter().chain(output_paths.iter()));

    // --- Directory bodies ---------------------------------------------
    if begin.directories.len() > MAX_DIR_NODES {
        return Err(invalid(format!(
            "Begin has {} directories, exceeds MAX_DIR_NODES {MAX_DIR_NODES}",
            begin.directories.len()
        )));
    }
    let mut directories: HashMap<[u8; 32], Directory> =
        HashMap::with_capacity(begin.directories.len());
    // Recursive descendant count per body, keyed by recomputed digest.
    // Used by the cross-body size-consistency pass below.
    let mut totals: HashMap<[u8; 32], u64> = HashMap::with_capacity(begin.directories.len());
    for d in &begin.directories {
        let total = validate_directory(d)?;
        // Canonical digest is recomputed server-side from the canonical
        // encode — the body is attested by content, never by claim.
        // r[impl store.castore.canonical-encoding]
        let digest = *blake3::hash(&d.encode_to_vec()).as_bytes();
        if directories.insert(digest, d.clone()).is_some() {
            return Err(invalid(format!(
                "duplicate Directory body {}",
                hex::encode(digest)
            )));
        }
        totals.insert(digest, total);
    }
    // Child-size consistency across bodies: a parent's claimed
    // `DirectoryEntry.size` must equal the child body's actual
    // recursive descendant count. The whole chain is digest-consistent
    // by construction (the size is covered by the parent's digest), so
    // a builder can fabricate any value — this is the snix
    // `Directory::validate` cross-check that catches it. Children whose
    // body is absent from the map are left to the reachability walk
    // (reachable-and-missing is its own rejection; unreachable is
    // harmless).
    for d in directories.values() {
        for e in &d.directories {
            let child: [u8; 32] = e.digest.as_slice().try_into().expect("validated 32 bytes");
            if let Some(actual) = totals.get(&child)
                && e.size != *actual
            {
                return Err(invalid(format!(
                    "DirectoryEntry \"{}\" claims child {} has {} descendants but its body \
                     has {actual}",
                    e.name.escape_ascii(),
                    hex::encode(child),
                    e.size,
                )));
            }
        }
    }

    // --- Per-output tree walk + chunk manifest -------------------------
    let mut outputs: Vec<ValidatedOutput> = Vec::with_capacity(begin.outputs.len());
    let mut manifest_len: HashMap<[u8; 32], u32> = HashMap::new();
    for (idx, o) in begin.outputs.iter().enumerate() {
        let ctx = format!("output {idx}");
        let store_path = StorePath::parse(&o.store_path)
            .map_err(|e| invalid(format!("{ctx}: store_path: {e}")))?;

        // Path ∈ expected_outputs for non-CA tokens — same gate and
        // same status code as `validate_put_metadata` step 6.
        if let Some(c) = claims
            && !c.is_ca
            && !c.expected_outputs.iter().any(|p| p == &o.store_path)
        {
            warn!(
                store_path = %o.store_path,
                executor_id = %c.executor_id,
                "PutPathChunked: path not in assignment's expected_outputs",
            );
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "{ctx}: path not authorized by assignment token"
            )));
        }

        if o.nar_size > MAX_NAR_SIZE {
            return Err(invalid(format!(
                "{ctx}: nar_size {} exceeds maximum {MAX_NAR_SIZE}",
                o.nar_size
            )));
        }
        let nar_hash: [u8; 32] = o.nar_hash.as_slice().try_into().map_err(|_| {
            invalid(format!(
                "{ctx}: nar_hash must be 32 bytes (SHA-256), got {}",
                o.nar_hash.len()
            ))
        })?;

        // References: bounded, parseable, subset of the candidate set.
        if o.refs.len() > MAX_REFERENCES {
            return Err(invalid(format!(
                "{ctx}: {} references exceed MAX_REFERENCES {MAX_REFERENCES}",
                o.refs.len()
            )));
        }
        let mut references: Vec<StorePath> = Vec::with_capacity(o.refs.len());
        for r in &o.refs {
            if !candidate_paths.contains(r.as_str()) {
                return Err(invalid(format!(
                    "{ctx}: reference {r:?} is not in input_closure ∪ output paths"
                )));
            }
            references
                .push(StorePath::parse(r).map_err(|e| invalid(format!("{ctx}: reference: {e}")))?);
        }
        references.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        references.dedup_by(|a, b| a.as_str() == b.as_str());

        // Chunk manifest: bounded, 32-byte digests, per-digest length
        // agreement across every occurrence in every output.
        if o.chunk_manifest.len() > manifest::MAX_CHUNKS {
            return Err(invalid(format!(
                "{ctx}: {} chunks exceed MAX_CHUNKS {}",
                o.chunk_manifest.len(),
                manifest::MAX_CHUNKS
            )));
        }
        let mut chunk_manifest: Vec<([u8; 32], u32)> = Vec::with_capacity(o.chunk_manifest.len());
        for (ci, c) in o.chunk_manifest.iter().enumerate() {
            let digest: [u8; 32] = c.hash.as_slice().try_into().map_err(|_| {
                invalid(format!(
                    "{ctx}: chunk_manifest[{ci}] digest must be 32 bytes, got {}",
                    c.hash.len()
                ))
            })?;
            if c.size == 0 || c.size as usize > CHUNK_MAX {
                return Err(invalid(format!(
                    "{ctx}: chunk_manifest[{ci}] size {} out of range (1..={CHUNK_MAX})",
                    c.size
                )));
            }
            match manifest_len.entry(digest) {
                std::collections::hash_map::Entry::Occupied(e) if *e.get() != c.size => {
                    return Err(invalid(format!(
                        "{ctx}: chunk {} declared with size {} here but {} elsewhere",
                        hex::encode(digest),
                        c.size,
                        e.get()
                    )));
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(c.size);
                }
            }
            chunk_manifest.push((digest, c.size));
        }

        // Tree walk: structure, reachability, per-file chunk runs,
        // framing, NAR index entries.
        let root_node = o
            .root_node
            .as_ref()
            .and_then(|r| r.node.as_ref())
            .ok_or_else(|| invalid(format!("{ctx}: root_node must be set")))?;
        let walk = walk_output(root_node, &directories, &chunk_manifest, &ctx)?;
        if walk.nar_size != o.nar_size {
            return Err(invalid(format!(
                "{ctx}: declared nar_size {} but the attested tree serializes to {}",
                o.nar_size, walk.nar_size
            )));
        }

        // The serialized Manifest is the reassembly source of truth for
        // GetPath and the GC sweep's refcount decrement: the FULL
        // interleaved framing + content sequence, whose concatenation
        // is exactly the NAR. Built by replaying the segments so the
        // manifest order matches the byte order.
        let mut full_list: Vec<ManifestEntry> =
            Vec::with_capacity(chunk_manifest.len() + walk.segments.len());
        {
            let mut cursor = 0usize;
            for seg in &walk.segments {
                match seg {
                    NarSegment::Framing { bytes, digest } => full_list.push(ManifestEntry {
                        hash: *digest,
                        size: bytes.len() as u32,
                    }),
                    NarSegment::FileContents { n_chunks, .. } => {
                        for _ in 0..*n_chunks {
                            let (h, s) = chunk_manifest[cursor];
                            cursor += 1;
                            full_list.push(ManifestEntry { hash: h, size: s });
                        }
                    }
                }
            }
        }
        if full_list.len() > manifest::MAX_CHUNKS {
            return Err(invalid(format!(
                "{ctx}: manifest would have {} entries (content + framing), exceeds \
                 MAX_CHUNKS {}",
                full_list.len(),
                manifest::MAX_CHUNKS
            )));
        }
        let mut unique_chunks: Vec<([u8; 32], u32)> = {
            let mut seen = HashSet::new();
            full_list
                .iter()
                .filter(|e| seen.insert(e.hash))
                .map(|e| (e.hash, e.size))
                .collect()
        };
        unique_chunks.sort_unstable_by_key(|(h, _)| *h);
        let chunk_list_bytes = Manifest { entries: full_list }.serialize();

        let info = ValidatedPathInfo {
            store_path_hash: store_path.sha256_digest().to_vec(),
            store_path,
            deriver: deriver.clone(),
            nar_hash,
            nar_size: o.nar_size,
            references,
            registration_time: 0,
            ultimate: false,
            signatures: Vec::new(),
            content_address: None,
        };

        outputs.push(ValidatedOutput {
            info,
            chunk_manifest,
            segments: walk.segments,
            dir_digests: walk.dir_digests,
            file_blobs: walk.file_blobs,
            root_node_encoded: RootNode {
                node: Some(root_node.clone()),
            }
            .encode_to_vec(),
            nar_index_entries: rio_proto::types::NarIndex {
                entries: walk.index_entries,
                root_digest: match root_node {
                    root_node::Node::DirDigest(d) => d.clone(),
                    _ => Vec::new(),
                },
            }
            .encode_to_vec(),
            chunk_list_bytes,
            unique_chunks,
        });
    }

    // --- novel: membership, no-dups, global-first-occurrence order ----
    let mut novel: Vec<[u8; 32]> = Vec::with_capacity(begin.novel.len());
    let mut novel_set: HashSet<[u8; 32]> = HashSet::with_capacity(begin.novel.len());
    for (i, d) in begin.novel.iter().enumerate() {
        let digest: [u8; 32] = d.as_slice().try_into().map_err(|_| {
            invalid(format!(
                "novel[{i}] digest must be 32 bytes, got {}",
                d.len()
            ))
        })?;
        if !manifest_len.contains_key(&digest) {
            return Err(invalid(format!(
                "novel[{i}] {} does not appear in any output's chunk_manifest",
                hex::encode(digest)
            )));
        }
        if !novel_set.insert(digest) {
            return Err(invalid(format!(
                "novel[{i}] {} is a duplicate",
                hex::encode(digest)
            )));
        }
        novel.push(digest);
    }
    // Recompute the global first-occurrence order and assert the novel
    // subsequence preserves it. Because the verify walk is sequential
    // and always expects `novel[next_novel]` next, this IS the
    // wire-order contract — checking it here turns a misordered `Begin`
    // into a fast reject instead of a mid-stream one.
    {
        let mut seen: HashSet<[u8; 32]> = HashSet::new();
        let mut expected: Vec<[u8; 32]> = Vec::with_capacity(novel.len());
        for o in &outputs {
            for (d, _) in &o.chunk_manifest {
                if seen.insert(*d) && novel_set.contains(d) {
                    expected.push(*d);
                }
            }
        }
        if expected != novel {
            return Err(invalid(
                "Begin.novel is not in global first-occurrence order over the outputs' \
                 chunk_manifests",
            ));
        }
    }

    // --- Budget: deduped-chunk fetch bytes + materialized framing -----
    let dedup_fetch_bytes: u64 = manifest_len
        .iter()
        .filter(|(d, _)| !novel_set.contains(*d))
        .map(|(_, len)| u64::from(*len))
        .sum();
    let framing_bytes: u64 = outputs
        .iter()
        .flat_map(|o| &o.segments)
        .map(|s| match s {
            NarSegment::Framing { bytes, .. } => bytes.len() as u64,
            NarSegment::FileContents { .. } => 0,
        })
        .sum();

    Ok(ValidatedBegin {
        outputs,
        novel,
        novel_set,
        manifest_len,
        directories,
        candidates,
        budget_bytes: dedup_fetch_bytes.saturating_add(framing_bytes),
    })
}

/// Structural validation of one `Directory` body — the snix
/// `Directory::validate` checks: entry names are single path components
/// (non-empty, no `/`, no NUL, not `.`/`..`, ≤ [`MAX_NAME_LEN`]), each
/// of the three lists is strictly sorted by name (which also forbids
/// intra-list duplicates), names are unique ACROSS the lists, child
/// digests are 32 bytes, symlink targets are bounded, and the
/// recursive descendant count does not overflow.
///
/// Returns the body's own recursive descendant count (computed from
/// its children's CLAIMED sizes); the caller cross-checks every
/// `DirectoryEntry.size` against the referenced child body's returned
/// count once all bodies are digested.
fn validate_directory(d: &Directory) -> Result<u64, Status> {
    fn check_name(name: &[u8]) -> Result<(), Status> {
        if name.is_empty() {
            return Err(invalid("Directory entry name must not be empty"));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(invalid(format!(
                "Directory entry name length {} exceeds {MAX_NAME_LEN}",
                name.len()
            )));
        }
        if name == b"." || name == b".." {
            return Err(invalid("Directory entry name must not be '.' or '..'"));
        }
        if name.contains(&b'/') || name.contains(&0) {
            return Err(invalid("Directory entry name must not contain '/' or NUL"));
        }
        Ok(())
    }
    fn check_sorted<'a>(names: impl Iterator<Item = &'a [u8]>, kind: &str) -> Result<(), Status> {
        let mut prev: Option<&[u8]> = None;
        for n in names {
            check_name(n)?;
            if let Some(p) = prev
                && p >= n
            {
                return Err(invalid(format!(
                    "Directory.{kind} entries are not strictly sorted by name"
                )));
            }
            prev = Some(n);
        }
        Ok(())
    }
    check_sorted(
        d.directories.iter().map(|e| e.name.as_slice()),
        "directories",
    )?;
    check_sorted(d.files.iter().map(|e| e.name.as_slice()), "files")?;
    check_sorted(d.symlinks.iter().map(|e| e.name.as_slice()), "symlinks")?;

    let mut all: HashSet<&[u8]> = HashSet::new();
    for n in d
        .directories
        .iter()
        .map(|e| e.name.as_slice())
        .chain(d.files.iter().map(|e| e.name.as_slice()))
        .chain(d.symlinks.iter().map(|e| e.name.as_slice()))
    {
        if !all.insert(n) {
            return Err(invalid(format!(
                "Directory entry name \"{}\" appears in more than one list",
                n.escape_ascii()
            )));
        }
    }
    for e in &d.directories {
        if e.digest.len() != 32 {
            return Err(invalid(format!(
                "DirectoryEntry digest must be 32 bytes, got {}",
                e.digest.len()
            )));
        }
    }
    for e in &d.files {
        if e.digest.len() != 32 {
            return Err(invalid(format!(
                "FileEntry digest must be 32 bytes, got {}",
                e.digest.len()
            )));
        }
    }
    for e in &d.symlinks {
        if e.target.is_empty() || e.target.len() > MAX_TARGET_LEN {
            return Err(invalid(format!(
                "SymlinkEntry target length {} out of range (1..={MAX_TARGET_LEN})",
                e.target.len()
            )));
        }
    }
    // Recursive descendant count: `len(files) + len(symlinks) +
    // Σ(1 + child.size)` — the same formula `castore::build` uses.
    // Overflow is rejected here; whether each `e.size` term is TRUE is
    // checked by the caller's cross-body pass once every child body's
    // own count is known (this function sees one body in isolation).
    let mut total: u64 = d.files.len() as u64 + d.symlinks.len() as u64;
    for e in &d.directories {
        total = total
            .checked_add(1)
            .and_then(|t| t.checked_add(e.size))
            .ok_or_else(|| invalid("Directory descendant count overflows u64"))?;
    }
    Ok(total)
}

/// Result of [`walk_output`].
struct WalkResult {
    segments: Vec<NarSegment>,
    nar_size: u64,
    dir_digests: Vec<[u8; 32]>,
    file_blobs: Vec<([u8; 32], u64, u64)>,
    index_entries: Vec<NarIndexEntry>,
}

/// NAR wire primitive: `u64-le(len) ++ bytes ++ zero-pad to 8`. Matches
/// `rio_nix::nar::sync_wire::write_bytes`; duplicated here because the
/// rio-nix primitives are module-private and this walker splices file
/// contents (which arrive later, from chunks) between the length prefix
/// and the padding — the round-trip golden test in `tests.rs` pins the
/// two implementations together.
fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
    buf.extend_from_slice(b);
    let pad = b.len().next_multiple_of(8) - b.len();
    buf.extend_from_slice(&[0u8; 8][..pad]);
}
fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_bytes(buf, s.as_bytes());
}

/// Walk one output's tree in canonical NAR order, producing the framing
/// segments, the recomputed NAR size, the reachable castore digest
/// sets, and the NAR index entries. Iterative (explicit work stack) so
/// an adversarial deep chain cannot blow the thread stack; bounded by
/// [`MAX_WALK_DEPTH`], [`MAX_WALK_ENTRIES`], and
/// [`MAX_NAR_FRAMING_BYTES`].
fn walk_output(
    root: &root_node::Node,
    directories: &HashMap<[u8; 32], Directory>,
    chunk_manifest: &[([u8; 32], u32)],
    ctx: &str,
) -> Result<WalkResult, Status> {
    /// One node to serialize.
    enum Node<'a> {
        File {
            digest: &'a [u8],
            size: u64,
            executable: bool,
        },
        Symlink {
            target: &'a [u8],
        },
        Dir {
            digest: [u8; 32],
        },
    }
    /// Work items, LIFO.
    enum Work<'a> {
        /// Emit `entry ( name <name> node` then the node body (and push
        /// the node's children). `entry_name: None` for the root.
        Open {
            node: Node<'a>,
            entry_name: Option<&'a [u8]>,
            /// '/'-joined path within the NAR ('' for the root).
            path: Vec<u8>,
            depth: usize,
        },
        /// Emit one literal `)` token (closes a node or an entry).
        Close,
    }

    let mut segments: Vec<NarSegment> = Vec::new();
    let mut framing: Vec<u8> = Vec::new();
    // Bytes already accounted for in `segments` (flushed framing + file
    // contents). The current NAR offset is always `flushed +
    // framing.len()`.
    let mut flushed: u64 = 0;
    // Framing bytes flushed into segments so far (excludes file
    // contents). `framing_flushed + framing.len()` is the running
    // materialized-framing total checked against the DoS cap.
    let mut framing_flushed: u64 = 0;
    let mut visited: usize = 0;
    let mut cursor: usize = 0; // position in chunk_manifest
    let mut dir_digest_set: HashSet<[u8; 32]> = HashSet::new();
    let mut file_blob_map: HashMap<[u8; 32], (u64, u64)> = HashMap::new();
    let mut index_entries: Vec<NarIndexEntry> = Vec::new();

    let root_node = match root {
        root_node::Node::DirDigest(d) => Node::Dir {
            digest: d.as_slice().try_into().map_err(|_| {
                invalid(format!(
                    "{ctx}: root dir_digest must be 32 bytes, got {}",
                    d.len()
                ))
            })?,
        },
        root_node::Node::File(f) => {
            if !f.name.is_empty() {
                return Err(invalid(format!("{ctx}: root FileEntry name must be empty")));
            }
            if f.digest.len() != 32 {
                return Err(invalid(format!(
                    "{ctx}: root FileEntry digest must be 32 bytes, got {}",
                    f.digest.len()
                )));
            }
            Node::File {
                digest: &f.digest,
                size: f.size,
                executable: f.executable,
            }
        }
        root_node::Node::Symlink(s) => {
            if !s.name.is_empty() {
                return Err(invalid(format!(
                    "{ctx}: root SymlinkEntry name must be empty"
                )));
            }
            if s.target.is_empty() || s.target.len() > MAX_TARGET_LEN {
                return Err(invalid(format!(
                    "{ctx}: root symlink target length {} out of range",
                    s.target.len()
                )));
            }
            Node::Symlink { target: &s.target }
        }
    };

    put_str(&mut framing, "nix-archive-1");
    let mut stack: Vec<Work> = vec![Work::Open {
        node: root_node,
        entry_name: None,
        path: Vec::new(),
        depth: 0,
    }];

    while let Some(work) = stack.pop() {
        let (node, entry_name, path, depth) = match work {
            Work::Close => {
                put_str(&mut framing, ")");
                continue;
            }
            Work::Open {
                node,
                entry_name,
                path,
                depth,
            } => (node, entry_name, path, depth),
        };
        if depth > MAX_WALK_DEPTH {
            return Err(invalid(format!(
                "{ctx}: directory nesting exceeds {MAX_WALK_DEPTH}"
            )));
        }
        visited += 1;
        if visited > MAX_WALK_ENTRIES {
            return Err(invalid(format!(
                "{ctx}: tree expands to more than {MAX_WALK_ENTRIES} entries"
            )));
        }
        // Bound the materialized framing as we go (not just at the end)
        // so a pathological tree is rejected before it OOMs rather than
        // after.
        if framing_flushed + framing.len() as u64 > MAX_NAR_FRAMING_BYTES {
            return Err(invalid(format!(
                "{ctx}: NAR framing exceeds {MAX_NAR_FRAMING_BYTES} bytes (tree has too many \
                 entries for a single upload)"
            )));
        }

        if let Some(name) = entry_name {
            put_str(&mut framing, "entry");
            put_str(&mut framing, "(");
            put_str(&mut framing, "name");
            put_bytes(&mut framing, name);
            put_str(&mut framing, "node");
        }
        put_str(&mut framing, "(");
        put_str(&mut framing, "type");
        match node {
            Node::File {
                digest,
                size,
                executable,
            } => {
                put_str(&mut framing, "regular");
                if executable {
                    put_str(&mut framing, "executable");
                    put_str(&mut framing, "");
                }
                put_str(&mut framing, "contents");
                framing.extend_from_slice(&size.to_le_bytes());

                // The contiguous chunk_manifest run for this file must
                // sum to exactly `size`.
                let run_start = cursor;
                let mut consumed: u64 = 0;
                while consumed < size {
                    let Some((_, len)) = chunk_manifest.get(cursor) else {
                        return Err(invalid(format!(
                            "{ctx}: chunk_manifest ends mid-file at \"{}\" ({consumed} of \
                             {size} bytes covered)",
                            path.escape_ascii(),
                        )));
                    };
                    consumed += u64::from(*len);
                    cursor += 1;
                }
                if consumed != size {
                    return Err(invalid(format!(
                        "{ctx}: chunk run for \"{}\" sums to {consumed} but FileEntry.size is \
                         {size}",
                        path.escape_ascii(),
                    )));
                }
                // Content offset = everything emitted so far (flushed
                // segments + the pending framing including the length
                // prefix just written). Matches `nar_ls`'s definition.
                let content_offset = flushed + framing.len() as u64;
                flushed += framing.len() as u64;
                framing_flushed += framing.len() as u64;
                flush_framing(&mut segments, &mut framing);
                let fdigest: [u8; 32] = digest.try_into().expect("validated 32 bytes");
                segments.push(NarSegment::FileContents {
                    n_chunks: cursor - run_start,
                    file_digest: fdigest,
                });
                flushed += size;
                let pad = (size.next_multiple_of(8) - size) as usize;
                framing.extend_from_slice(&[0u8; 8][..pad]);
                put_str(&mut framing, ")");

                file_blob_map
                    .entry(fdigest)
                    .or_insert((content_offset, size));
                index_entries.push(NarIndexEntry {
                    path: path.clone(),
                    kind: ProtoNarEntryKind::Regular.into(),
                    size,
                    executable,
                    nar_offset: content_offset,
                    target: Vec::new(),
                    file_digest: fdigest.to_vec(),
                    dir_digest: Vec::new(),
                });
            }
            Node::Symlink { target } => {
                put_str(&mut framing, "symlink");
                put_str(&mut framing, "target");
                put_bytes(&mut framing, target);
                put_str(&mut framing, ")");
                index_entries.push(NarIndexEntry {
                    path: path.clone(),
                    kind: ProtoNarEntryKind::Symlink.into(),
                    size: 0,
                    executable: false,
                    nar_offset: 0,
                    target: target.to_vec(),
                    file_digest: Vec::new(),
                    dir_digest: Vec::new(),
                });
            }
            Node::Dir { digest } => {
                put_str(&mut framing, "directory");
                let dir = directories.get(&digest).ok_or_else(|| {
                    invalid(format!(
                        "{ctx}: Directory body for reachable digest {} is missing from \
                         Begin.directories",
                        hex::encode(digest)
                    ))
                })?;
                dir_digest_set.insert(digest);
                index_entries.push(NarIndexEntry {
                    path: path.clone(),
                    kind: ProtoNarEntryKind::Directory.into(),
                    size: 0,
                    executable: false,
                    nar_offset: 0,
                    target: Vec::new(),
                    file_digest: Vec::new(),
                    dir_digest: digest.to_vec(),
                });

                // This directory's closing `)` pops after all children.
                stack.push(Work::Close);
                // Children in canonical NAR order = the three sorted
                // lists merged by name, pushed in REVERSE so they pop
                // forward. Each child is wrapped `entry ( … )`: the
                // wrapper's `)` is pushed first (pops after the child).
                let merged = merge_entries(dir, ctx)?;
                for (name, child) in merged.into_iter().rev() {
                    let mut child_path = path.clone();
                    if !child_path.is_empty() {
                        child_path.push(b'/');
                    }
                    child_path.extend_from_slice(name);
                    stack.push(Work::Close);
                    stack.push(Work::Open {
                        node: child,
                        entry_name: Some(name),
                        path: child_path,
                        depth: depth + 1,
                    });
                }
            }
        }
    }
    if cursor != chunk_manifest.len() {
        return Err(invalid(format!(
            "{ctx}: chunk_manifest has {} trailing entries not covered by any file",
            chunk_manifest.len() - cursor
        )));
    }
    let nar_size = flushed + framing.len() as u64;
    flush_framing(&mut segments, &mut framing);

    let mut dir_digests: Vec<[u8; 32]> = dir_digest_set.into_iter().collect();
    dir_digests.sort_unstable();
    let mut file_blobs: Vec<([u8; 32], u64, u64)> = file_blob_map
        .into_iter()
        .map(|(d, (o, s))| (d, o, s))
        .collect();
    file_blobs.sort_unstable_by_key(|(d, _, _)| *d);

    return Ok(WalkResult {
        segments,
        nar_size,
        dir_digests,
        file_blobs,
        index_entries,
    });

    /// Flush the pending framing buffer into one or more
    /// [`NarSegment::Framing`] segments, each ≤ [`CHUNK_MAX`] bytes
    /// (the framing runs become CAS chunks; the read path's per-chunk
    /// buffers assume the chunker's max). No-op on an empty buffer.
    ///
    /// TODO: each framing run becomes its own S3 object (~100–300 B
    /// between adjacent files), so a 20k-file output produces 40k+
    /// extra small objects — per-request PUT cost roughly doubles for
    /// file-heavy outputs and the chunks table grows accordingly. If
    /// this shows up in S3 request bills or chunk-row counts, coalesce
    /// each framing run into the preceding or following content chunk
    /// (one merged chunk per boundary, still ≤ CHUNK_MAX + framing
    /// overhead) at the cost of losing content-chunk dedup across
    /// outputs whose file contents match but whose entry names differ.
    fn flush_framing(segments: &mut Vec<NarSegment>, framing: &mut Vec<u8>) {
        if framing.is_empty() {
            return;
        }
        for piece in std::mem::take(framing).chunks(CHUNK_MAX) {
            segments.push(NarSegment::Framing {
                digest: *blake3::hash(piece).as_bytes(),
                bytes: piece.to_vec(),
            });
        }
    }

    /// Merge a directory's three sorted entry lists into canonical NAR
    /// order (byte-lexicographic by name). Names are globally unique
    /// across the lists ([`validate_directory`]), so ties are
    /// impossible.
    fn merge_entries<'a>(
        dir: &'a Directory,
        ctx: &str,
    ) -> Result<Vec<(&'a [u8], Node<'a>)>, Status> {
        let mut merged: Vec<(&'a [u8], Node<'a>)> =
            Vec::with_capacity(dir.directories.len() + dir.files.len() + dir.symlinks.len());
        let (mut di, mut fi, mut si) = (0, 0, 0);
        loop {
            let d = dir.directories.get(di).map(|e| e.name.as_slice());
            let f = dir.files.get(fi).map(|e| e.name.as_slice());
            let s = dir.symlinks.get(si).map(|e| e.name.as_slice());
            let next = [d, f, s].into_iter().flatten().min();
            let Some(next) = next else { break };
            if d == Some(next) {
                let e = &dir.directories[di];
                merged.push((
                    e.name.as_slice(),
                    Node::Dir {
                        digest: e
                            .digest
                            .as_slice()
                            .try_into()
                            .map_err(|_| invalid(format!("{ctx}: bad child dir digest length")))?,
                    },
                ));
                di += 1;
            } else if f == Some(next) {
                let e = &dir.files[fi];
                merged.push((
                    e.name.as_slice(),
                    Node::File {
                        digest: &e.digest,
                        size: e.size,
                        executable: e.executable,
                    },
                ));
                fi += 1;
            } else {
                let e = &dir.symlinks[si];
                merged.push((e.name.as_slice(), Node::Symlink { target: &e.target }));
                si += 1;
            }
        }
        Ok(merged)
    }
}
