//! `Begin`-frame validation for `PutPathChunked` (ADR-022 §6.2).
//!
//! Everything in a [`PutPathChunkedBegin`] is attacker-controlled: the
//! builder runs adversary-supplied build instructions and the upload
//! happens after the sandbox has already executed them. The HMAC
//! assignment token (verified by the caller before this module runs)
//! proves the request comes from a pod the scheduler dispatched a
//! specific derivation to — nothing more. [`validate_begin`] is the
//! single gate between that untrusted message and every downstream
//! consumer (the placeholder claim, the verify task, the commit
//! transaction): it bounds every repeated field, decodes every digest,
//! recomputes every recomputable claim, and hands back a
//! [`ValidatedBegin`] whose invariants the rest of the handler relies
//! on without re-checking.
//!
//! No DB, no S3, no side effects — violations are rejected before any
//! placeholder row exists (`r[store.put.chunked-bounds]`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tonic::Status;

use rio_auth::hmac::AssignmentClaims;
use rio_common::grpc::check_bound;
use rio_common::limits::{
    FASTCDC_MAX_BYTES, MAX_BATCH_OUTPUTS, MAX_DIR_NODES, MAX_INPUT_CLOSURE, MAX_NAR_SIZE,
    MAX_REFERENCES,
};
use rio_nix::nar::MAX_NAR_ENTRIES;
use rio_nix::store_path::StorePath;
use rio_proto::castore::{Directory, RootNode, root_node};
use rio_proto::castore_util::{directory_digest, validate_directory};
use rio_proto::types::PutPathChunkedBegin;

use crate::castore_nar::{self, WalkEvent};
use crate::manifest::MAX_CHUNKS;

/// One regular file's slice of an output's `chunk_manifest`, in
/// canonical NAR walk order. The §6.3 verify task iterates these to
/// splice chunk bodies between framing tokens and to recompute each
/// file's whole-file BLAKE3 against [`Self::digest`] — without that
/// recompute a malicious builder could poison the cross-tenant
/// `file_digest → content` dedup namespace `ReadBlob` serves from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileRun {
    /// The claimed `FileEntry.digest` (whole-file BLAKE3). Verified by
    /// the §6.3 verify task, not here.
    pub digest: [u8; 32],
    /// `FileEntry.size` — the run's chunk sizes sum to exactly this.
    pub size: u64,
    /// Index range into the owning output's `chunk_manifest`. Empty
    /// for empty files.
    pub chunks: Range<usize>,
}

/// One output of [`ValidatedBegin`], fully decoded and bounds-checked.
#[derive(Debug)]
pub(crate) struct ValidatedOutput {
    pub store_path: StorePath,
    /// NAR SHA-256, computed by the builder's fused walk and committed
    /// as claimed (`r[store.integrity.verify-on-put+2]`).
    pub nar_hash: [u8; 32],
    /// NAR byte count — builder-computed, committed as claimed.
    pub nar_size: u64,
    /// Runtime references, scanned by the builder's fused walk
    /// (`r[builder.upload.references-scanned+2]`) and committed as
    /// claimed. Each is a member of `input_closure ∪ {sibling
    /// outputs}` (enforced below).
    pub references: Vec<StorePath>,
    pub root_node: RootNode,
    /// `(chunk_digest, size)` in canonical NAR walk order. Sizes are
    /// self-consistent with the attested tree (each file's run sums to
    /// its `FileEntry.size`); novel bodies are length-checked against
    /// them at receive time, and deduped digests were length-checked
    /// when first uploaded.
    pub chunk_manifest: Vec<([u8; 32], u32)>,
    /// Whether this output's store path is derived from its content
    /// (floating CA). Taken from the **claims**, never from the
    /// message: a builder that could self-assert CA status would
    /// bypass the `expected_outputs` membership check below and
    /// upload to arbitrary paths. `claims.is_ca` is per-derivation
    /// (the scheduler signs it), so every output of a CA derivation
    /// is CA and vice versa.
    pub is_ca: bool,
    /// Regular files in canonical NAR walk order, each mapped to its
    /// contiguous `chunk_manifest` run.
    pub file_runs: Vec<FileRun>,
}

/// A [`PutPathChunkedBegin`] that has passed every
/// `r[store.put.chunked-bounds]` check. Downstream phases consume this
/// instead of the raw proto. `DirectoryEntry.size` claims are verified
/// too — see [`check_directory_entry_sizes`].
#[derive(Debug)]
pub(crate) struct ValidatedBegin {
    pub outputs: Vec<ValidatedOutput>,
    /// Every Directory body, keyed by its **recomputed** digest. Every
    /// entry is reachable from at least one output's `root_node`, and
    /// every digest reachable from any `root_node` is present.
    pub directories: HashMap<[u8; 32], Directory>,
    /// The chunk digests the builder will send as `Chunk` frames, in
    /// the order it will send them. Membership and ordering against
    /// the global first-occurrence sequence are validated; whether
    /// each digest is *actually* absent from the store is a DB
    /// question the verify task answers.
    pub novel: Vec<[u8; 32]>,
}

/// Validate an untrusted `Begin` frame against the caller's verified
/// HMAC claims. See the module docs for the threat model and
/// [`ValidatedBegin`] for the produced invariants.
///
/// Claims mismatches (the deriver binding, the input-closure
/// attestation, output-path authorization) return `PERMISSION_DENIED`;
/// structural violations return `INVALID_ARGUMENT`. Both are terminal
/// for the RPC — nothing has been written yet.
// r[impl store.put.chunked-bounds]
pub(crate) fn validate_begin(
    begin: &PutPathChunkedBegin,
    claims: &AssignmentClaims,
) -> Result<ValidatedBegin, Status> {
    // ── Repeated-field bounds, BEFORE any per-element work. ──────────
    // An attacker can send millions of entries in any repeated field;
    // parsing or hashing them all before rejecting on count is the DoS.
    if begin.outputs.is_empty() {
        return Err(Status::invalid_argument(
            "PutPathChunked: Begin carries no outputs",
        ));
    }
    check_bound("outputs", begin.outputs.len(), MAX_BATCH_OUTPUTS)?;
    check_bound(
        "input_closure paths",
        begin.input_closure.len(),
        MAX_INPUT_CLOSURE,
    )?;
    check_bound("directories", begin.directories.len(), MAX_DIR_NODES)?;

    // ── Input-closure attestation. ───────────────────────────────────
    // The closure is the reference-scan candidate set: a builder that
    // could inject extra paths into it could launder a reference to a
    // path the build never saw. The scheduler signs
    // blake3(closure.join("\n")) into the claims at dispatch; the
    // builder must echo the closure byte-identically (same order —
    // both sides emit sorted). An empty claims digest means the
    // scheduler couldn't compute the closure (pre-P0589 token), not
    // that the closure is empty — the candidate set is then unattested
    // and the refscan falls back to trusting the echo.
    if !claims.input_closure_digest.is_empty() {
        let echoed = AssignmentClaims::digest_input_closure(&begin.input_closure);
        if echoed != claims.input_closure_digest {
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "input_closure_mismatch"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "PutPathChunked: input_closure ({} paths) does not match the \
                 assignment token's attested closure digest",
                begin.input_closure.len()
            )));
        }
    }
    // Each input_closure path must parse — the refs ⊆ closure check
    // below compares raw strings, so a malformed entry would otherwise
    // slip through as an unmatched (harmless) candidate; reject the
    // builder bug early instead.
    for p in &begin.input_closure {
        if let Err(e) = StorePath::parse(p) {
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: invalid input_closure path: {e}"
            )));
        }
    }

    // ── Deriver ↔ token binding. ─────────────────────────────────────
    // Ties the narinfo's recorded provenance to the build the token was
    // issued for; a builder cannot record derivation B as the deriver
    // of an output it built under derivation A's token. For CA
    // derivations `claims.drv_hash` is the modular hash (not derivable
    // from the deriver store path), and the binding that matters —
    // content → path — is enforced by the server-side CA-path
    // recompute at commit time instead.
    let deriver = StorePath::parse(&begin.deriver).map_err(|e| {
        Status::invalid_argument(format!("PutPathChunked: invalid deriver path: {e}"))
    })?;
    if !deriver.is_derivation() {
        return Err(Status::invalid_argument(
            "PutPathChunked: deriver is not a .drv path",
        ));
    }
    if !claims.is_ca && !deriver_matches_drv_hash(&deriver, &claims.drv_hash) {
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "deriver_mismatch"
        )
        .increment(1);
        return Err(Status::permission_denied(
            "PutPathChunked: deriver does not match the assignment token's drv_hash",
        ));
    }

    // ── Directory bodies → recomputed-digest map. ────────────────────
    // The digest is always derived from the body, never read from the
    // entry that referenced it: a body whose content doesn't match the
    // digest its parent claims simply fails the reachability check
    // below as "referenced but not supplied".
    let mut directories: HashMap<[u8; 32], Directory> =
        HashMap::with_capacity(begin.directories.len());
    for (i, d) in begin.directories.iter().enumerate() {
        validate_directory(d).map_err(|e| {
            Status::invalid_argument(format!("PutPathChunked: directories[{i}]: {e}"))
        })?;
        let digest = directory_digest(d);
        if directories.insert(digest, d.clone()).is_some() {
            // The builder dedups bodies across outputs before sending;
            // a duplicate is a malformed message, and silently keeping
            // either copy would hide a client bug.
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: directories[{i}] duplicates an earlier body \
                 (digest {})",
                hex::encode(digest)
            )));
        }
    }

    // ── Per-output scalar bounds + claims authorization. ─────────────
    // The reference candidate set: everything the build could have
    // legitimately observed (its declared input closure) plus the
    // sibling outputs of the same derivation (self-references and
    // cross-output references are normal). A reference outside this
    // set is a fabrication — the scanner could never have found it.
    // Outputs are inserted first so a duplicate output path is caught
    // here; the closure (which may legitimately overlap the outputs)
    // is extended in afterwards.
    let mut allowed_refs: HashSet<&str> =
        HashSet::with_capacity(begin.outputs.len() + begin.input_closure.len());
    for o in &begin.outputs {
        if !allowed_refs.insert(o.store_path.as_str()) {
            return Err(Status::invalid_argument(
                "PutPathChunked: duplicate output store_path",
            ));
        }
    }
    allowed_refs.extend(begin.input_closure.iter().map(String::as_str));

    let mut outputs = Vec::with_capacity(begin.outputs.len());
    for (i, o) in begin.outputs.iter().enumerate() {
        let store_path = StorePath::parse(&o.store_path).map_err(|e| {
            Status::invalid_argument(format!("PutPathChunked: outputs[{i}].store_path: {e}"))
        })?;
        // Same gate as PutPath's `validate_metadata` step 6: a non-CA
        // token authorizes exactly the paths the scheduler signed into
        // it. CA outputs are exempt because the path is not known at
        // sign time; their authorization is the path-derivation check
        // at commit (`r[sec.authz.ca-path-derived]`), binding the
        // claimed path to the claimed NAR hash and references.
        if !claims.is_ca
            && !claims
                .expected_outputs
                .iter()
                .any(|e| e == store_path.as_str())
        {
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "PutPathChunked: outputs[{i}] not authorized by assignment token"
            )));
        }
        let nar_hash: [u8; 32] = o.nar_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}].nar_hash must be 32 bytes (SHA-256), got {}",
                o.nar_hash.len()
            ))
        })?;
        if o.nar_size == 0 || o.nar_size > MAX_NAR_SIZE {
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}].nar_size {} outside 1..={MAX_NAR_SIZE}",
                o.nar_size
            )));
        }
        check_bound(
            "references (per output)",
            o.references.len(),
            MAX_REFERENCES,
        )?;
        let references: Vec<StorePath> = o
            .references
            .iter()
            .map(|r| {
                if !allowed_refs.contains(r.as_str()) {
                    return Err(Status::invalid_argument(format!(
                        "PutPathChunked: outputs[{i}] references a path outside \
                         input_closure ∪ outputs"
                    )));
                }
                StorePath::parse(r).map_err(|e| {
                    Status::invalid_argument(format!("PutPathChunked: outputs[{i}] reference: {e}"))
                })
            })
            .collect::<Result<_, _>>()?;
        check_bound("chunk_manifest entries", o.chunk_manifest.len(), MAX_CHUNKS)?;
        let mut blob_len: u64 = 0;
        let chunk_manifest: Vec<([u8; 32], u32)> = o
            .chunk_manifest
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let digest: [u8; 32] = c.digest.as_slice().try_into().map_err(|_| {
                    Status::invalid_argument(format!(
                        "PutPathChunked: outputs[{i}].chunk_manifest[{j}].digest must be \
                         32 bytes, got {}",
                        c.digest.len()
                    ))
                })?;
                // A zero-size chunk is meaningless (empty files
                // contribute zero chunks, not one empty chunk) and an
                // oversized one cannot have come from the agreed
                // FastCDC parameters — either way the digest would
                // never dedup against a legitimately-produced chunk.
                if c.size == 0 || c.size > FASTCDC_MAX_BYTES as u64 {
                    return Err(Status::invalid_argument(format!(
                        "PutPathChunked: outputs[{i}].chunk_manifest[{j}].size {} outside \
                         1..={FASTCDC_MAX_BYTES}",
                        c.size
                    )));
                }
                blob_len += c.size; // ≤ MAX_CHUNKS × FASTCDC_MAX ≪ u64::MAX
                Ok((digest, c.size as u32))
            })
            .collect::<Result<_, _>>()?;
        // The blob stream (file contents only) cannot exceed the NAR
        // that frames it. A manifest summing past the claimed nar_size
        // means at least one of the two is a lie; rejecting here is
        // cheaper than discovering it after streaming every chunk.
        if blob_len > o.nar_size {
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}] chunk_manifest sums to {blob_len} bytes, \
                 more than the claimed nar_size {}",
                o.nar_size
            )));
        }
        let root_node = decode_root_node(o.root_node.as_ref(), i)?;
        outputs.push(ValidatedOutput {
            store_path,
            nar_hash,
            nar_size: o.nar_size,
            references,
            root_node,
            chunk_manifest,
            is_ca: claims.is_ca,
            file_runs: Vec::new(), // filled by the tree walk below
        });
    }

    // ── Reachability: supplied bodies ⇔ reachable digests. ───────────
    // A BFS over the deduped DAG (not the expanded tree) — bounded by
    // the already-bounded distinct-body count. "Reachable but not
    // supplied" makes the tree unservable; "supplied but not
    // reachable" would upsert a refcounted `directories` row that
    // belongs to no path and never gets decremented.
    let reachable = reachable_digests(&outputs, &directories)?;
    if reachable.len() != directories.len() {
        let orphan = directories
            .keys()
            .find(|d| !reachable.contains(*d))
            .expect("count mismatch implies an unreachable key");
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: directory {} is not reachable from any output's root_node",
            hex::encode(orphan)
        )));
    }

    // ── DirectoryEntry.size (recursive descendant count). ────────────
    check_directory_entry_sizes(&directories)?;

    // ── Expanded tree walk: chunk-run alignment → FileRuns. ──────────
    // File boundaries MUST be chunk boundaries (per-file FastCDC); the
    // walk proves the claimed chunk list is exactly the concatenation
    // of per-file runs over the attested tree, with nothing left over.
    for (i, out) in outputs.iter_mut().enumerate() {
        out.file_runs = align_chunk_runs(i, out, &directories)?;
    }

    // ── Cross-occurrence size agreement. ─────────────────────────────
    // A chunk digest determines its content determines its size; the
    // same digest claimed at two different sizes (within or across
    // outputs) means at least one output's chunk-run alignment was
    // computed over a false size and would commit a corrupt manifest.
    // The verify task's fetch-time length assertion can only check one
    // of the two claims — the disagreement has to be caught here.
    {
        let mut sizes: HashMap<[u8; 32], u32> = HashMap::new();
        for (i, out) in outputs.iter().enumerate() {
            for (d, s) in &out.chunk_manifest {
                if *sizes.entry(*d).or_insert(*s) != *s {
                    return Err(Status::invalid_argument(format!(
                        "PutPathChunked: chunk {} is claimed at two different sizes \
                         (outputs[{i}] says {s})",
                        hex::encode(d)
                    )));
                }
            }
        }
    }

    // ── novel: membership, uniqueness, first-occurrence order. ───────
    let novel = validate_novel(&begin.novel, &outputs)?;

    Ok(ValidatedBegin {
        outputs,
        directories,
        novel,
    })
}

/// Bind `Begin.deriver` to `claims.drv_hash` for input-addressed
/// derivations.
///
/// The two known producer formats for `drv_hash` are the full `.drv`
/// store path (the scheduler's DAG node key for IA derivations,
/// `dag.proto`'s "Input-addressed: store path") and the bare
/// nixbase32 hash part (the spec's `hash_part(deriver)` form). Accept
/// exactly those two; anything else fails closed.
fn deriver_matches_drv_hash(deriver: &StorePath, drv_hash: &str) -> bool {
    if deriver.hash_part() == drv_hash {
        return true;
    }
    if let Ok(p) = StorePath::parse(drv_hash) {
        return p.hash_part() == deriver.hash_part();
    }
    false
}

/// Decode and length-check an output's `root_node`.
fn decode_root_node(root: Option<&RootNode>, i: usize) -> Result<RootNode, Status> {
    let Some(root) = root else {
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: outputs[{i}].root_node is unset"
        )));
    };
    match &root.node {
        None => Err(Status::invalid_argument(format!(
            "PutPathChunked: outputs[{i}].root_node oneof is unset"
        ))),
        Some(root_node::Node::DirDigest(d)) if d.len() != 32 => {
            Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}].root_node dir_digest must be 32 bytes, got {}",
                d.len()
            )))
        }
        Some(root_node::Node::File(f)) if f.digest.len() != 32 => {
            Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}].root_node file digest must be 32 bytes, got {}",
                f.digest.len()
            )))
        }
        // A root symlink's target is bounded the same way a child
        // symlink's is, but the Directory validator can't be reused:
        // the name rules differ (empty is required here, forbidden
        // there). Check the target directly.
        Some(root_node::Node::Symlink(s))
            if s.target.is_empty()
                || s.target.len() > rio_common::limits::MAX_CASTORE_TARGET_BYTES
                || s.target.contains(&0) =>
        {
            Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}].root_node symlink target is invalid \
                 ({} bytes)",
                s.target.len()
            )))
        }
        Some(_) => Ok(root.clone()),
    }
}

/// Collect every directory digest reachable from any output's root via
/// `DirectoryEntry.digest` edges. Errors on a referenced digest with
/// no supplied body. O(distinct bodies), not O(expanded tree).
fn reachable_digests(
    outputs: &[ValidatedOutput],
    directories: &HashMap<[u8; 32], Directory>,
) -> Result<HashSet<[u8; 32]>, Status> {
    let mut reached: HashSet<[u8; 32]> = HashSet::new();
    let mut frontier: Vec<[u8; 32]> = Vec::new();
    for (i, out) in outputs.iter().enumerate() {
        if let Some(root_node::Node::DirDigest(d)) = &out.root_node.node {
            let digest: [u8; 32] = d
                .as_slice()
                .try_into()
                .expect("decode_root_node checked the length");
            if reached.insert(digest) {
                frontier.push(digest);
            }
            // Surface the *root* body being missing with the output
            // index; child misses are reported by digest below.
            if !directories.contains_key(&digest) {
                return Err(Status::invalid_argument(format!(
                    "PutPathChunked: outputs[{i}].root_node references directory {} \
                     which is not in Begin.directories",
                    hex::encode(digest)
                )));
            }
        }
    }
    while let Some(digest) = frontier.pop() {
        let body = directories.get(&digest).ok_or_else(|| {
            Status::invalid_argument(format!(
                "PutPathChunked: directory {} is referenced but not in Begin.directories",
                hex::encode(digest)
            ))
        })?;
        for e in &body.directories {
            let child: [u8; 32] = e
                .digest
                .as_slice()
                .try_into()
                .expect("validate_directory checked the length");
            if reached.insert(child) {
                frontier.push(child);
            }
        }
    }
    // Every digest inserted into `reached` is also pushed onto
    // `frontier`, and every popped digest is looked up with a
    // missing-body error — so "reached ⊆ supplied" is fully covered by
    // the loop above.
    Ok(reached)
}

/// Verify every `DirectoryEntry.size` claim: the recursive descendant
/// count must equal the child's immediate entry count plus the sum of
/// the child's own directory entries' sizes. Each body's claim is
/// checked against its immediate child's claims only; because the DAG
/// is acyclic (a cycle requires a BLAKE3 collision) and every body is
/// checked, local consistency implies global correctness by induction.
///
/// The field participates in the canonical encoding (a lie splits the
/// dedup namespace) and snix consumers use it for inode allocation —
/// `u64::MAX` claims must not be committable.
fn check_directory_entry_sizes(directories: &HashMap<[u8; 32], Directory>) -> Result<(), Status> {
    for body in directories.values() {
        for e in &body.directories {
            let child_digest: [u8; 32] = e
                .digest
                .as_slice()
                .try_into()
                .expect("validate_directory checked the length");
            let child = directories
                .get(&child_digest)
                .expect("reachability check ran first");
            let immediate =
                (child.directories.len() + child.files.len() + child.symlinks.len()) as u64;
            let claimed_descendants: Option<u64> = child
                .directories
                .iter()
                .try_fold(immediate, |acc, c| acc.checked_add(c.size));
            if claimed_descendants != Some(e.size) {
                return Err(Status::invalid_argument(format!(
                    "PutPathChunked: DirectoryEntry.size {} for directory {} does not \
                     match its child's descendant count",
                    e.size,
                    hex::encode(child_digest)
                )));
            }
        }
    }
    Ok(())
}

/// Walk one output's expanded tree and carve its `chunk_manifest` into
/// per-file runs. Every regular file consumes the next contiguous run
/// of entries summing to exactly `FileEntry.size`; after the walk the
/// manifest must be fully consumed.
fn align_chunk_runs(
    i: usize,
    out: &ValidatedOutput,
    directories: &HashMap<[u8; 32], Directory>,
) -> Result<Vec<FileRun>, Status> {
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    // The walk enforces the shared ingest tree bounds (node count via
    // MAX_NAR_ENTRIES here, plus the depth and index-byte caps inside
    // TreeWalk itself) — `r[store.ingest.tree-bounds]` — so a tree the
    // NAR readers would reject is refused before any placeholder claim
    // or S3 write.
    for ev in castore_nar::walk(&out.root_node, directories, MAX_NAR_ENTRIES) {
        let ev = ev.map_err(|e| {
            Status::invalid_argument(format!("PutPathChunked: outputs[{i}] tree walk: {e}"))
        })?;
        let WalkEvent::File { digest, size, .. } = ev else {
            continue;
        };
        let start = cursor;
        let mut covered = 0u64;
        while covered < size {
            let Some((_, chunk_size)) = out.chunk_manifest.get(cursor) else {
                return Err(Status::invalid_argument(format!(
                    "PutPathChunked: outputs[{i}] chunk_manifest exhausted at entry \
                     {cursor} with {} bytes of file {} still uncovered",
                    size - covered,
                    hex::encode(digest)
                )));
            };
            covered += u64::from(*chunk_size);
            cursor += 1;
        }
        if covered != size {
            // The run overshot: a chunk straddles this file's end.
            // File boundaries MUST be chunk boundaries — that property
            // is what makes a file's chunk run identical wherever the
            // file appears and lets ReadBlob map offsets to exact
            // chunk windows.
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: outputs[{i}] chunk run for a {size}-byte file sums to \
                 {covered} bytes (file boundaries must be chunk boundaries)"
            )));
        }
        runs.push(FileRun {
            digest,
            size,
            chunks: start..cursor,
        });
    }
    if cursor != out.chunk_manifest.len() {
        // Trailing entries no file accounts for would acquire chunk
        // refcounts and (post-verify) durable flags for content that
        // is not part of any committed path — a fabricated-reference
        // vector against the GC.
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: outputs[{i}] chunk_manifest has {} trailing entries not \
             covered by any file in the tree",
            out.chunk_manifest.len() - cursor
        )));
    }
    Ok(runs)
}

/// Validate `Begin.novel`: every digest appears in some output's
/// chunk_manifest, no duplicates, and the relative order matches the
/// global first-occurrence order (outputs in message order, manifest
/// entries in walk order). The §6.3 verify task relies on this to
/// assert that the next `Chunk` frame off the wire is always exactly
/// `novel[next]` — the ordering check here is what makes that single
/// cursor sufficient (no out-of-order buffering, no digest → frame
/// map).
fn validate_novel(novel: &[Vec<u8>], outputs: &[ValidatedOutput]) -> Result<Vec<[u8; 32]>, Status> {
    // `novel` cannot legitimately exceed the number of distinct chunk
    // digests, which is bounded by outputs × MAX_CHUNKS; bound it
    // before parsing to keep the reject cheap.
    check_bound("novel digests", novel.len(), MAX_BATCH_OUTPUTS * MAX_CHUNKS)?;
    let mut first_occurrence: HashMap<[u8; 32], usize> = HashMap::new();
    for out in outputs {
        for (d, _) in &out.chunk_manifest {
            let next = first_occurrence.len();
            first_occurrence.entry(*d).or_insert(next);
        }
    }
    let mut prev: Option<usize> = None;
    let mut parsed = Vec::with_capacity(novel.len());
    for (j, d) in novel.iter().enumerate() {
        let digest: [u8; 32] = d.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "PutPathChunked: novel[{j}] must be 32 bytes, got {}",
                d.len()
            ))
        })?;
        let Some(&pos) = first_occurrence.get(&digest) else {
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: novel[{j}] does not appear in any output's chunk_manifest"
            )));
        };
        // Strictly increasing first-occurrence positions ⇒ a
        // duplicate-free subsequence of the first-occurrence order.
        if prev.is_some_and(|p| pos <= p) {
            return Err(Status::invalid_argument(format!(
                "PutPathChunked: novel[{j}] is out of global first-occurrence order \
                 (or duplicated)"
            )));
        }
        prev = Some(pos);
        parsed.push(digest);
    }
    Ok(parsed)
}

// r[verify store.put.chunked-bounds]
#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::castore::{DirectoryEntry, FileEntry, SymlinkEntry};
    use rio_proto::types::{ChunkMeta, ChunkedOutput};
    use tonic::Code;

    // Valid nixbase32 hash parts (alphabet excludes e/o/u/t).
    const OUT_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out-a";
    const OUT_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-out-b";
    const DEP_1: &str = "/nix/store/cccccccccccccccccccccccccccccccc-dep-1";
    const DEP_2: &str = "/nix/store/dddddddddddddddddddddddddddddddd-dep-2";
    const DERIVER: &str = "/nix/store/ffffffffffffffffffffffffffffffff-thing.drv";

    /// Chunk sizes for output A's 3-chunk / 2-file tree and output B's
    /// 2-chunk single-file root. `C1` is shared between A and B to
    /// exercise the global first-occurrence ordering of `novel`.
    const C1: ([u8; 32], u64) = ([0x11; 32], FASTCDC_MAX_BYTES as u64);
    const C2: ([u8; 32], u64) = ([0x22; 32], 37_856);
    const C3: ([u8; 32], u64) = ([0x33; 32], 1_000);
    const C4: ([u8; 32], u64) = ([0x44; 32], 5_000);

    fn meta(c: ([u8; 32], u64)) -> ChunkMeta {
        ChunkMeta {
            digest: c.0.to_vec(),
            size: c.1,
        }
    }

    /// Output A's tree:
    /// ```text
    /// /            (dir)
    ///   bin/       (dir)
    ///     app      (file, C1+C2)
    ///   empty      (file, 0 bytes, no chunks)
    ///   lib.so     (file, C3)
    ///   link -> bin/app
    /// ```
    /// Returns `(root_node, [root_body, bin_body])`.
    fn tree_a() -> (RootNode, Vec<Directory>) {
        let bin = Directory {
            files: vec![FileEntry {
                name: b"app".to_vec(),
                digest: vec![0xAA; 32],
                size: C1.1 + C2.1,
                executable: true,
            }],
            ..Default::default()
        };
        let root = Directory {
            directories: vec![DirectoryEntry {
                name: b"bin".to_vec(),
                digest: directory_digest(&bin).to_vec(),
                // bin has 1 immediate child and no child directories.
                size: 1,
            }],
            files: vec![
                FileEntry {
                    name: b"empty".to_vec(),
                    digest: blake3::hash(b"").as_bytes().to_vec(),
                    size: 0,
                    executable: false,
                },
                FileEntry {
                    name: b"lib.so".to_vec(),
                    digest: vec![0xBB; 32],
                    size: C3.1,
                    executable: false,
                },
            ],
            symlinks: vec![SymlinkEntry {
                name: b"link".to_vec(),
                target: b"bin/app".to_vec(),
            }],
        };
        let rn = RootNode {
            node: Some(root_node::Node::DirDigest(directory_digest(&root).to_vec())),
        };
        (rn, vec![root, bin])
    }

    /// Output B: a bare-file root (no directories) of C1 + C4.
    fn root_b() -> RootNode {
        RootNode {
            node: Some(root_node::Node::File(FileEntry {
                name: Vec::new(),
                digest: vec![0xCC; 32],
                size: C1.1 + C4.1,
                executable: false,
            })),
        }
    }

    fn begin() -> PutPathChunkedBegin {
        let (root_a, dirs) = tree_a();
        PutPathChunkedBegin {
            deriver: DERIVER.into(),
            outputs: vec![
                ChunkedOutput {
                    store_path: OUT_A.into(),
                    nar_hash: vec![0x01; 32],
                    nar_size: 1 << 20,
                    references: vec![DEP_1.into(), OUT_B.into()],
                    root_node: Some(root_a),
                    chunk_manifest: vec![meta(C1), meta(C2), meta(C3)],
                },
                ChunkedOutput {
                    store_path: OUT_B.into(),
                    nar_hash: vec![0x02; 32],
                    nar_size: 1 << 20,
                    references: vec![],
                    root_node: Some(root_b()),
                    chunk_manifest: vec![meta(C1), meta(C4)],
                },
            ],
            directories: dirs,
            // Global first-occurrence order is [C1, C2, C3, C4]; the
            // builder found C1 and C3 already durable.
            novel: vec![C2.0.to_vec(), C4.0.to_vec()],
            input_closure: vec![DEP_1.into(), DEP_2.into()],
        }
    }

    fn claims() -> AssignmentClaims {
        AssignmentClaims {
            executor_id: "builder-0".into(),
            drv_hash: DERIVER.into(),
            expected_outputs: vec![OUT_A.into(), OUT_B.into()],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: None,
            input_closure_digest: AssignmentClaims::digest_input_closure(&[
                DEP_1.into(),
                DEP_2.into(),
            ]),
        }
    }

    fn expect_code(begin: &PutPathChunkedBegin, claims: &AssignmentClaims, code: Code, why: &str) {
        match validate_begin(begin, claims) {
            Ok(_) => panic!("{why}: expected {code:?}, got Ok"),
            Err(s) => assert_eq!(
                s.code(),
                code,
                "{why}: wrong code, message: {}",
                s.message()
            ),
        }
    }

    // r[verify store.put.chunked-bounds]
    /// The well-formed two-output fixture validates, and the derived
    /// structures (file runs, the directory map, novel) come out
    /// exactly as the §6.3 verify task will consume them.
    #[test]
    fn well_formed_begin_validates_with_correct_file_runs() {
        let v = validate_begin(&begin(), &claims()).expect("fixture must validate");

        assert_eq!(v.outputs.len(), 2);
        assert_eq!(v.directories.len(), 2, "root + bin bodies");
        assert_eq!(v.novel, vec![C2.0, C4.0]);

        // Output A: walk order is bin/app, empty, lib.so. The empty
        // file gets an empty run; the runs partition the manifest.
        let a = &v.outputs[0];
        assert!(!a.is_ca);
        assert_eq!(
            a.file_runs,
            vec![
                FileRun {
                    digest: [0xAA; 32],
                    size: C1.1 + C2.1,
                    chunks: 0..2,
                },
                FileRun {
                    digest: *blake3::hash(b"").as_bytes(),
                    size: 0,
                    chunks: 2..2,
                },
                FileRun {
                    digest: [0xBB; 32],
                    size: C3.1,
                    chunks: 2..3,
                },
            ],
        );
        // Output B: a single-file root consumes its whole manifest.
        assert_eq!(v.outputs[1].file_runs.len(), 1);
        assert_eq!(v.outputs[1].file_runs[0].chunks, 0..2);
    }

    /// `claims.is_ca` exempts outputs from the expected_outputs
    /// membership check (the path is content-derived at commit time).
    /// The flag comes from the signed claims, NOT the message, so
    /// there is no `is_ca` field on ChunkedOutput to test forgery of.
    #[test]
    fn ca_claims_exempt_outputs_from_expected_outputs() {
        let mut c = claims();
        c.expected_outputs = vec![String::new()]; // CA dispatch shape
        c.is_ca = true;
        let v = validate_begin(&begin(), &c).expect("CA outputs skip the membership check");
        assert!(v.outputs.iter().all(|o| o.is_ca));
    }

    // r[verify store.put.chunked-bounds]
    /// Repeated-field count caps fire before any per-element work.
    #[test]
    fn rejects_oversized_repeated_fields() {
        let mut b = begin();
        let proto = b.outputs[0].clone();
        b.outputs = vec![proto; MAX_BATCH_OUTPUTS + 1];
        expect_code(&b, &claims(), Code::InvalidArgument, "outputs over cap");

        let b = PutPathChunkedBegin {
            outputs: vec![],
            ..begin()
        };
        expect_code(&b, &claims(), Code::InvalidArgument, "zero outputs");

        // input_closure over cap: the count check must fire before the
        // attestation digest (which would hash all 64k+1 paths).
        let mut b = begin();
        b.input_closure = (0..=MAX_INPUT_CLOSURE).map(|i| format!("p{i}")).collect();
        expect_code(&b, &claims(), Code::InvalidArgument, "closure over cap");
    }

    /// Tampering with the echoed input closure (the refscan candidate
    /// set) is caught by the attestation digest. An empty claims
    /// digest means "unattested", not "must be empty".
    #[test]
    fn rejects_tampered_input_closure() {
        let mut b = begin();
        b.input_closure.push(DEP_2.replace("dep-2", "dep-3"));
        expect_code(
            &b,
            &claims(),
            Code::PermissionDenied,
            "extra closure path not in the attested digest",
        );

        // Reordering is also a mismatch — the digest is order-
        // sensitive and both producers emit sorted.
        let mut b = begin();
        b.input_closure.reverse();
        expect_code(&b, &claims(), Code::PermissionDenied, "reordered closure");

        // No attestation → the echo is accepted as-is.
        let mut c = claims();
        c.input_closure_digest = String::new();
        let mut b = begin();
        b.input_closure.push(DEP_2.replace("dep-2", "dep-3"));
        // The extra path must still parse; dep-3 is a valid store path.
        validate_begin(&b, &c).expect("unattested closure is accepted");
    }

    /// The deriver must be a .drv path bound to the token's drv_hash.
    #[test]
    fn rejects_deriver_token_mismatch() {
        let mut b = begin();
        b.deriver = DERIVER.replace("ffff", "gggg");
        expect_code(
            &b,
            &claims(),
            Code::PermissionDenied,
            "deriver hash differs from the token's",
        );

        let mut b = begin();
        b.deriver = OUT_A.into(); // not a .drv
        expect_code(&b, &claims(), Code::InvalidArgument, "deriver not a .drv");

        // The bare-hash-part claims format is also accepted.
        let mut c = claims();
        c.drv_hash = "ffffffffffffffffffffffffffffffff".into();
        validate_begin(&begin(), &c).expect("bare hash-part drv_hash format");
    }

    /// A non-CA output not in the token's expected_outputs is an
    /// authorization failure, not a malformed message.
    #[test]
    fn rejects_unauthorized_output_path() {
        let mut c = claims();
        c.expected_outputs = vec![OUT_A.into()]; // OUT_B no longer authorized
        expect_code(
            &begin(),
            &c,
            Code::PermissionDenied,
            "output not in expected_outputs",
        );
    }

    // r[verify store.put.chunked-bounds]
    /// Per-output scalar bounds: hash length, nar_size range, the
    /// reference subset rule, chunk digest/size bounds, the
    /// blob-exceeds-nar cross-check, duplicate output paths.
    #[test]
    fn rejects_per_output_scalar_violations() {
        let mut b = begin();
        b.outputs[0].nar_hash = vec![0x01; 31];
        expect_code(&b, &claims(), Code::InvalidArgument, "31-byte nar_hash");

        let mut b = begin();
        b.outputs[0].nar_size = 0;
        expect_code(&b, &claims(), Code::InvalidArgument, "zero nar_size");

        let mut b = begin();
        b.outputs[0].nar_size = MAX_NAR_SIZE + 1;
        expect_code(&b, &claims(), Code::InvalidArgument, "nar_size over cap");

        // A reference to a path outside input_closure ∪ outputs is a
        // fabrication — the build could not have observed it.
        let mut b = begin();
        b.outputs[0].references = vec![DEP_2.replace("dep-2", "dep-9")];
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "reference outside closure",
        );

        let mut b = begin();
        b.outputs[0].chunk_manifest[0].digest = vec![0x11; 16];
        expect_code(&b, &claims(), Code::InvalidArgument, "16-byte chunk digest");

        let mut b = begin();
        b.outputs[0].chunk_manifest[0].size = 0;
        expect_code(&b, &claims(), Code::InvalidArgument, "zero-size chunk");

        let mut b = begin();
        b.outputs[0].chunk_manifest[0].size = FASTCDC_MAX_BYTES as u64 + 1;
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "chunk over FASTCDC max",
        );

        // The blob stream cannot exceed the NAR that frames it.
        let mut b = begin();
        b.outputs[0].nar_size = 100;
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "manifest sums past nar_size",
        );

        let mut b = begin();
        b.outputs[1].store_path = OUT_A.into();
        let mut c = claims();
        c.expected_outputs = vec![OUT_A.into()];
        expect_code(&b, &c, Code::InvalidArgument, "duplicate output path");

        let mut b = begin();
        b.outputs[0].root_node = None;
        expect_code(&b, &claims(), Code::InvalidArgument, "unset root_node");
    }

    // r[verify store.put.chunked-bounds]
    /// Directory-set violations: a body that fails structural
    /// validation, a duplicate body, a reachable-but-missing body, and
    /// a supplied-but-unreachable body.
    #[test]
    fn rejects_directory_set_violations() {
        // Unsorted body → validate_directory rejects it.
        let mut b = begin();
        b.directories[0].files.reverse();
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "unsorted directory body",
        );

        // Duplicate body (same canonical encoding → same digest).
        let mut b = begin();
        let dup = b.directories[1].clone();
        b.directories.push(dup);
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "duplicate directory body",
        );

        // Drop the bin body: reachable from root but not supplied.
        let mut b = begin();
        b.directories.remove(1);
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "missing reachable body",
        );

        // Add a body nothing references: it would be upserted with a
        // refcount that nothing ever decrements.
        let mut b = begin();
        b.directories.push(Directory {
            symlinks: vec![SymlinkEntry {
                name: b"orphan".to_vec(),
                target: b"x".to_vec(),
            }],
            ..Default::default()
        });
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "unreachable extra body",
        );
    }

    /// A `Begin` whose single output is a chain of `depth_below_root`
    /// single-child directories under the root directory. Bodies and
    /// `DirectoryEntry.size` descendant counts are built bottom-up so
    /// every other validation passes; only the nesting depth varies.
    fn begin_with_dir_chain(depth_below_root: usize) -> PutPathChunkedBegin {
        let mut bodies = Vec::with_capacity(depth_below_root + 1);
        let mut child: Option<([u8; 32], u64)> = None;
        for _ in 0..depth_below_root {
            let dir = match child {
                None => Directory::default(),
                Some((digest, size)) => Directory {
                    directories: vec![DirectoryEntry {
                        name: b"d".to_vec(),
                        digest: digest.to_vec(),
                        size,
                    }],
                    ..Default::default()
                },
            };
            let digest = directory_digest(&dir);
            let descendants =
                dir.directories.len() as u64 + dir.directories.iter().map(|e| e.size).sum::<u64>();
            bodies.push(dir);
            child = Some((digest, descendants));
        }
        let (child_digest, child_size) = child.expect("at least one level");
        let root = Directory {
            directories: vec![DirectoryEntry {
                name: b"d".to_vec(),
                digest: child_digest.to_vec(),
                size: child_size,
            }],
            ..Default::default()
        };
        let root_digest = directory_digest(&root);
        bodies.push(root);
        PutPathChunkedBegin {
            deriver: DERIVER.into(),
            outputs: vec![ChunkedOutput {
                store_path: OUT_A.into(),
                nar_hash: vec![0x01; 32],
                nar_size: 1 << 20,
                references: vec![],
                root_node: Some(RootNode {
                    node: Some(root_node::Node::DirDigest(root_digest.to_vec())),
                }),
                chunk_manifest: vec![],
            }],
            directories: bodies,
            novel: vec![],
            input_closure: vec![DEP_1.into(), DEP_2.into()],
        }
    }

    /// Directory nesting deeper than the NAR readers' `MAX_NAR_DEPTH`
    /// must be rejected at the validation boundary (bug_006): a
    /// committed deeper tree regenerates a NAR that re-ingest,
    /// substitution, the gateway restore, and stock Nix all reject with
    /// NestingTooDeep — the path would be 'complete' but permanently
    /// unservable. At exactly the readers' limit it must still validate.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn rejects_directory_nesting_deeper_than_nar_readers_accept() {
        use rio_nix::nar::MAX_NAR_DEPTH;

        // root is depth 0, so MAX_NAR_DEPTH + 1 levels below the root
        // puts the deepest body at depth MAX_NAR_DEPTH + 1.
        let b = begin_with_dir_chain(MAX_NAR_DEPTH + 1);
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "tree deeper than the NAR readers accept",
        );

        let b = begin_with_dir_chain(MAX_NAR_DEPTH);
        validate_begin(&b, &claims()).expect("a tree at exactly the depth limit validates");
    }

    /// A claimed descendant count that doesn't match the child body is
    /// rejected — see [`check_directory_entry_sizes`] for why it matters.
    #[test]
    fn rejects_directory_entry_size_lie() {
        let (_, dirs) = tree_a();
        let bin_digest = directory_digest(&dirs[1]);
        let mut b = begin();
        // Find the root body (the one with a "bin" entry) and lie
        // about bin's descendant count.
        for d in &mut b.directories {
            for e in &mut d.directories {
                if e.digest == bin_digest {
                    e.size = u64::MAX;
                }
            }
        }
        expect_code(&b, &claims(), Code::InvalidArgument, "descendant-count lie");
    }

    // r[verify store.put.chunked-bounds]
    /// Chunk-run alignment: file boundaries must be chunk boundaries,
    /// the manifest must cover every file, and nothing may be left
    /// over.
    #[test]
    fn rejects_misaligned_chunk_runs() {
        // C1's size no longer lands on bin/app's file boundary — the
        // run overshoots.
        let mut b = begin();
        b.outputs[0].chunk_manifest[1].size = C2.1 + 1;
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "chunk straddles a file boundary",
        );

        // Manifest exhausted before the file is covered.
        let mut b = begin();
        b.outputs[0].chunk_manifest.pop();
        expect_code(&b, &claims(), Code::InvalidArgument, "manifest too short");

        // Trailing entries no file accounts for.
        let mut b = begin();
        b.outputs[0].chunk_manifest.push(meta(([0x55; 32], 1_234)));
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "trailing manifest entries",
        );
    }

    /// The same chunk digest claimed at two different sizes — only the
    /// cross-occurrence agreement check in `validate_begin` catches this.
    #[test]
    fn rejects_cross_output_chunk_size_disagreement() {
        // C1 is shared between outputs A and B. Shrink B's copy and
        // grow B's other chunk by the same amount so B's run still
        // sums to its FileEntry.size — only the cross-occurrence
        // agreement check can catch this.
        let mut b = begin();
        b.outputs[1].chunk_manifest[0].size = C1.1 - 1;
        b.outputs[1].chunk_manifest[1].size = C4.1 + 1;
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "same digest at two sizes",
        );
    }

    // r[verify store.put.chunked-bounds]
    /// `novel` must be a duplicate-free subsequence of the global
    /// first-occurrence order over the outputs' chunk manifests.
    #[test]
    fn rejects_malformed_novel() {
        // A digest in no output's manifest.
        let mut b = begin();
        b.novel = vec![vec![0x99; 32]];
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "novel digest not in any manifest",
        );

        // Out of first-occurrence order (C4 first occurs after C2).
        let mut b = begin();
        b.novel = vec![C4.0.to_vec(), C2.0.to_vec()];
        expect_code(&b, &claims(), Code::InvalidArgument, "novel out of order");

        // Duplicate.
        let mut b = begin();
        b.novel = vec![C2.0.to_vec(), C2.0.to_vec()];
        expect_code(
            &b,
            &claims(),
            Code::InvalidArgument,
            "duplicate novel digest",
        );

        // Wrong length.
        let mut b = begin();
        b.novel = vec![vec![0x22; 31]];
        expect_code(&b, &claims(), Code::InvalidArgument, "31-byte novel digest");

        // Empty novel is legal — a fully-deduplicated upload sends no
        // chunk frames at all.
        let mut b = begin();
        b.novel = vec![];
        validate_begin(&b, &claims()).expect("empty novel is a fully-deduped upload");
    }
}
