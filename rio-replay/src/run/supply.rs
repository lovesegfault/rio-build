//! Supply planning — what the target must build vs what the campaign
//! hands it.
//!
//! A faithful replay makes the target cluster BUILD exactly what the
//! recorded clients built, while being GIVEN everything those builds merely
//! consumed. This module owns that boundary for the campaign engine:
//!
//! - [`workload_set`]: which derivations the target must build itself
//!   (everything with a recorded expected outcome, minus impure-demoted
//!   derivations whose recorded environment cannot be forwarded).
//! - [`walk_closure`]: the archive-side closure walk over `inputDrvs` /
//!   `inputSrcs` for one or more request roots.
//! - [`resolve_source`]: the supply ladder deciding where each needed path
//!   comes from (target substituter, archive, relay, or nowhere) under the
//!   campaign's dependency policy.
//! - [`plan_uploads`]: reference-safe upload planning (what to send, from
//!   where, in which order) for one closure.
//! - [`topo_levels`] / [`split_batches`]: pure ordering and sizing of a
//!   planned batch into reference-safe levels and capped sub-batches.
//! - [`UploadClaims`]: cross-request dedup so concurrently replayed requests
//!   upload a shared dependency exactly once.
//!
//! Planning only records WHAT to upload. Payload bytes for embedded paths
//! are dumped by the supply execution arms at send time; the only bytes
//! materialized here are derivation ATerm texts (tiny, and already read for
//! the closure walk).

pub mod exec;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rio_nix::derivation::Derivation;
use rio_nix::nar::{self, NarNode};
use rio_nix::narinfo::NarInfo;
use rio_nix::protocol::pathinfo::ValidPathInfo;
use rio_nix::store_path::nixbase32;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::archive::reader::ReplayArchive;
use crate::archive::schema::ClosureRecord;
use crate::narhash::NarHash;

use super::spec::SupplyDependencies;

/// Derivations whose outputs the target must BUILD (never be given).
///
/// Every derivation with a recorded expected outcome (any outcome), minus
/// impure-demoted derivations: rio cannot forward the recording client's
/// `impureEnvVars` values, so replaying those builds would diverge — their
/// outputs are supplied like any other dependency and the build is skipped.
#[derive(Debug, Clone, Default)]
pub struct WorkloadSet {
    /// Full `.drv` store paths.
    pub drvs: BTreeSet<String>,
    /// Derivations that have an expected-outcome record but are demoted
    /// because the archive lists impure environment variables for them.
    /// Kept separately so the supply stage can record the skip reason for
    /// each demoted unit.
    pub demoted_impure: BTreeSet<String>,
}

/// Compute the workload set for an archive: every drv with a recorded
/// expected outcome, minus drvs listed in the archive's impure-env map
/// (those land in [`WorkloadSet::demoted_impure`] instead).
pub fn workload_set(archive: &ReplayArchive) -> WorkloadSet {
    let impure = archive.impure_env();
    let mut drvs = BTreeSet::new();
    let mut demoted_impure = BTreeSet::new();
    for record in archive.outcome_records() {
        if impure.contains_key(&record.drv) {
            demoted_impure.insert(record.drv.clone());
        } else {
            drvs.insert(record.drv.clone());
        }
    }
    WorkloadSet {
        drvs,
        demoted_impure,
    }
}

/// One derivation in a request closure.
///
/// The parsed derivation is deliberately not retained: at prewarm-union
/// scale (hundreds of thousands to millions of derivations) the parsed env
/// maps would dominate memory, while everything the planner needs fits in
/// the few small fields below. Re-read the ATerm from the archive if a
/// future consumer needs more than these.
#[derive(Debug, Clone)]
pub struct ClosureNode {
    /// Full `.drv` store path.
    pub drv_path: String,
    /// Output name → store path (empty for floating/CA outputs whose path is
    /// only known post-build).
    pub outputs: BTreeMap<String, String>,
    /// Input source store paths (non-derivation build inputs).
    pub input_srcs: Vec<String>,
    /// Input derivation path → output names this node consumes (empty when
    /// the closure was built from adjacency records, which do not record
    /// the consumed output names; nothing in the planner needs them).
    pub input_drvs: BTreeMap<String, Vec<String>>,
}

/// Topologically ordered (dependencies before dependents), deduplicated
/// closure of one or more request roots.
#[derive(Debug, Clone)]
pub struct Closure {
    /// Closure nodes, children before parents.
    pub topo: Vec<ClosureNode>,
    /// Every path relevant for a target-validity probe: all drv paths, all
    /// inputSrcs, and all known (non-empty) output paths.
    pub all_paths: BTreeSet<String>,
}

/// Walk `inputDrvs`/`inputSrcs` from the archive starting at `roots` (`.drv`
/// store paths).
///
/// When the archive declares the `dependency_closures` capability the
/// closure is assembled from its adjacency records without reading any
/// embedded `.drv`; otherwise every derivation along the walk is read and
/// parsed from its ATerm text. Both constructions produce the same closure
/// (same membership, same children-before-parents ordering guarantee, same
/// probe-path set). Errors if a derivation the walk needs is missing from
/// the backing source, naming both the missing path and the root that
/// needed it.
pub fn walk_closure(archive: &ReplayArchive, roots: &[String]) -> Result<Closure> {
    if archive.capabilities().dependency_closures {
        closure_from_adjacency(archive.closures(), roots)
    } else {
        closure_from_drv_texts(archive, roots)
    }
}

/// Closure construction over embedded `.drv` ATerm texts: read and parse
/// each derivation the walk reaches.
fn closure_from_drv_texts(archive: &ReplayArchive, roots: &[String]) -> Result<Closure> {
    walk_from(roots, |drv_path, root| {
        let text = archive
            .read_drv(drv_path)
            .with_context(|| format!("walking the closure of replay root {root}"))?;
        let drv = Derivation::parse(&text)
            .with_context(|| format!("parsing {drv_path} (closure of replay root {root})"))?;
        let outputs: BTreeMap<String, String> = drv
            .outputs()
            .iter()
            .map(|o| (o.name().to_string(), o.path().to_string()))
            .collect();
        let input_srcs: Vec<String> = drv.input_srcs().iter().cloned().collect();
        let input_drvs: BTreeMap<String, Vec<String>> = drv
            .input_drvs()
            .iter()
            .map(|(dep, outs)| (dep.clone(), outs.iter().cloned().collect()))
            .collect();
        Ok(ClosureNode {
            drv_path: drv_path.to_string(),
            outputs,
            input_srcs,
            input_drvs,
        })
    })
}

/// Closure construction over recorded adjacency records (drv → direct
/// inputs / sources / declared outputs): no derivation text is touched.
fn closure_from_adjacency(records: &[ClosureRecord], roots: &[String]) -> Result<Closure> {
    let by_drv: HashMap<&str, &ClosureRecord> = records
        .iter()
        .map(|record| (record.drv.as_str(), record))
        .collect();
    walk_from(roots, |drv_path, root| {
        let record = by_drv.get(drv_path).ok_or_else(|| {
            anyhow!(
                "derivation {drv_path} has no dependency-closure record in the archive \
                 (closure of replay root {root})"
            )
        })?;
        Ok(ClosureNode {
            drv_path: record.drv.clone(),
            outputs: record
                .outputs
                .iter()
                .map(|(name, path)| (name.clone(), path.clone().unwrap_or_default()))
                .collect(),
            input_srcs: record.srcs.clone(),
            input_drvs: record
                .inputs
                .iter()
                .map(|input| (input.clone(), Vec::new()))
                .collect(),
        })
    })
}

/// Iterative post-order DFS shared by both closure constructions: each
/// derivation is loaded exactly once via `load_node` (given the drv path and
/// the root that pulled it in) and emitted only after all of its input
/// derivations, which yields the children-before-parents order the upload
/// planner relies on. Iterative so a deep dependency chain cannot overflow
/// the call stack.
fn walk_from(
    roots: &[String],
    load_node: impl Fn(&str, &str) -> Result<ClosureNode>,
) -> Result<Closure> {
    /// One in-progress DFS frame: the loaded node plus the input derivations
    /// still to descend into.
    struct Frame {
        node: ClosureNode,
        children: Vec<String>,
        next_child: usize,
    }

    /// Prepare the DFS frame for one loaded node (children = its inputDrvs).
    fn frame_for(node: ClosureNode) -> Frame {
        let children: Vec<String> = node.input_drvs.keys().cloned().collect();
        Frame {
            node,
            children,
            next_child: 0,
        }
    }

    let mut topo: Vec<ClosureNode> = Vec::new();
    let mut all_paths: BTreeSet<String> = BTreeSet::new();
    // Marked when a derivation is pushed (not when it is emitted): each node
    // is loaded exactly once even with diamond dependencies, and a malformed
    // archive with a dependency cycle cannot loop forever.
    let mut visited: BTreeSet<String> = BTreeSet::new();

    for root in roots {
        if !visited.insert(root.clone()) {
            continue;
        }
        let mut stack: Vec<Frame> = vec![frame_for(load_node(root, root)?)];
        while let Some(top) = stack.last_mut() {
            let next_child = if top.next_child < top.children.len() {
                let child = top.children[top.next_child].clone();
                top.next_child += 1;
                Some(child)
            } else {
                None
            };
            match next_child {
                Some(child) => {
                    if visited.insert(child.clone()) {
                        stack.push(frame_for(load_node(&child, root)?));
                    }
                }
                None => {
                    let frame = stack.pop().expect("loop condition guarantees a frame");
                    let node = frame.node;
                    all_paths.insert(node.drv_path.clone());
                    all_paths.extend(node.input_srcs.iter().cloned());
                    all_paths.extend(node.outputs.values().filter(|p| !p.is_empty()).cloned());
                    topo.push(node);
                }
            }
        }
    }

    Ok(Closure { topo, all_paths })
}

/// Where a needed (non-derivation) store path comes from.
#[derive(Debug, Clone, PartialEq)]
// The Relay variant carries the full narinfo. These are planning values
// (a few per needed path), not hot-path data — boxing would only complicate
// the call sites that construct and match them.
#[allow(clippy::large_enum_variant)]
pub enum PathSource {
    /// A target-cluster substituter has it; the target will substitute it
    /// itself (the engine moves no bytes).
    TargetSubstituter,
    /// Embedded in the archive; upload from there.
    Archive,
    /// Relayed from one of the recording's substituters.
    Relay {
        /// The recording substituter that has the path.
        substituter_url: String,
        /// Its narinfo there (drives the fetch and the path-info upload).
        narinfo: NarInfo,
    },
    /// Output of a workload drv (must be built), withheld by the dependency
    /// policy, or nothing has it.
    NotSupplied {
        /// True when the path is an output the target is expected to build.
        workload: bool,
    },
}

/// The supply ladder for one missing, non-derivation path, in this exact
/// precedence:
///
/// 1. output of a workload drv            → [`PathSource::NotSupplied`] `{ workload: true }`
/// 2. any target substituter has narinfo  → [`PathSource::TargetSubstituter`]
/// 3. archive embeds the path             → [`PathSource::Archive`]
/// 4. a recording substituter has it      → [`PathSource::Relay`]
/// 5. otherwise                           → [`PathSource::NotSupplied`] `{ workload: false }`
///
/// Workload outputs come first unconditionally: handing the target a path it
/// is supposed to produce would corrupt the measurement (the build would be
/// skipped as already-valid), so they are never supplied even when a
/// substituter or the archive could.
///
/// `dependencies` adjusts the ladder below that first rung:
///
/// - [`SupplyDependencies::Substituters`]: the full ladder above applies.
/// - [`SupplyDependencies::EmbeddedOnly`]: the target-substituter rung is
///   skipped — even paths the target's substituters could provide are
///   uploaded from the archive or relayed, taking the recording-side caches
///   out of the measurement entirely.
/// - [`SupplyDependencies::None`]: dependency outputs are never delivered by
///   any mechanism — every non-workload path that is not an input source
///   resolves to [`PathSource::NotSupplied`] `{ workload: false }` and the
///   target builds it itself. Input sources (members of `input_srcs`) cannot
///   be built, so they stay uploadable through the archive/relay rungs.
///
/// Probe results are passed in by the caller: the supply stage probes the
/// target and the relay substituters once for the union of needed paths, and
/// the per-request path reuses the same maps. `workload_outputs` is the set
/// of output paths produced by workload drvs (membership is all that is
/// checked, so the caller may include outputs of workload drvs outside this
/// closure too); `input_srcs` is the union of the closure nodes' input
/// sources; `archive_embeds` answers "does the archive embed this path".
pub fn resolve_source(
    path: &str,
    workload_outputs: &BTreeSet<String>,
    target_coverage: &BTreeSet<String>,
    input_srcs: &BTreeSet<String>,
    archive_embeds: impl Fn(&str) -> bool,
    relay_narinfos: &HashMap<String, (String, NarInfo)>,
    dependencies: SupplyDependencies,
) -> PathSource {
    if workload_outputs.contains(path) {
        return PathSource::NotSupplied { workload: true };
    }
    if dependencies == SupplyDependencies::None && !input_srcs.contains(path) {
        return PathSource::NotSupplied { workload: false };
    }
    if dependencies == SupplyDependencies::Substituters && target_coverage.contains(path) {
        return PathSource::TargetSubstituter;
    }
    if archive_embeds(path) {
        return PathSource::Archive;
    }
    if let Some((substituter_url, narinfo)) = relay_narinfos.get(path) {
        return PathSource::Relay {
            substituter_url: substituter_url.clone(),
            narinfo: narinfo.clone(),
        };
    }
    PathSource::NotSupplied { workload: false }
}

/// One planned upload.
#[derive(Debug, Clone)]
pub struct UploadItem {
    /// Full store path being uploaded.
    pub store_path: String,
    /// Path metadata sent ahead of the NAR (the daemon-protocol path-info).
    pub info: ValidPathInfo,
    /// Where the NAR bytes come from at send time.
    pub payload: UploadPayload,
}

/// Where an [`UploadItem`]'s NAR bytes come from.
#[derive(Debug, Clone)]
// Same trade-off as PathSource: the Relay variant carries the narinfo and
// that is fine for planning data.
#[allow(clippy::large_enum_variant)]
pub enum UploadPayload {
    /// Derivation ATerm text wrapped as a single-file NAR. The bytes here ARE
    /// the NAR (not the raw ATerm) — drv text is tiny and already in memory
    /// at plan time, so it is carried inline.
    DrvText(Vec<u8>),
    /// Embedded store path; bytes are dumped from the archive at send time.
    ArchivePath,
    /// Fetched from this substituter at send time.
    Relay {
        /// Substituter base URL to fetch from.
        substituter_url: String,
        /// The narinfo describing the NAR to fetch (URL, compression, size).
        narinfo: NarInfo,
    },
}

/// Reference-safe upload plan for one closure.
#[derive(Debug, Clone)]
pub struct UploadPlan {
    /// Relayed paths whose uncompressed `nar_size` is at or above the
    /// large-NAR threshold passed to [`plan_uploads`], each sent
    /// individually (streaming) BEFORE the batch.
    pub large: Vec<UploadItem>,
    /// Everything else, in reference-safe order: a path appears only after
    /// every reference of it that is also being uploaded by this plan.
    pub batch: Vec<UploadItem>,
    /// `(path, reason)` pairs that could not be planned — informational; the
    /// affected requests degrade rather than abort.
    pub skipped: Vec<(String, String)>,
}

/// Build the reference-safe upload plan for one closure given resolved
/// sources and the set of paths the target already has.
///
/// What gets an [`UploadItem`]:
///
/// - every closure derivation whose `.drv` path is not in `target_valid`
///   (as [`UploadPayload::DrvText`]);
/// - every non-derivation closure path that is not in `target_valid` and
///   resolved to [`PathSource::Archive`] or [`PathSource::Relay`].
///
/// [`PathSource::TargetSubstituter`] paths are the target's own job to
/// fetch; workload outputs ([`PathSource::NotSupplied`] `{ workload: true }`)
/// are the point of the replay and never supplied; paths with no source at
/// all land in [`UploadPlan::skipped`] and the affected requests degrade.
/// Relayed paths whose `nar_size` is at or above `large_nar_threshold_bytes`
/// route to [`UploadPlan::large`] for individual streaming.
///
/// Batch ordering: per node, inputSrcs → outputs → the `.drv` itself, nodes
/// in children-first order, then settled in passes so every reference that
/// is also being uploaded lands before its referrer. Self-references are
/// ignored; references to paths the target already has, builds itself, or
/// that are simply not uploaded do not constrain ordering. Items that still
/// have an unsatisfied uploaded-reference after the passes (a cycle, or a
/// reference whose own upload was skipped) are skipped — except derivation
/// texts, which are never skipped and are placed after whatever of their
/// references could be planned.
pub fn plan_uploads(
    closure: &Closure,
    sources: &HashMap<String, PathSource>,
    target_valid: &BTreeSet<String>,
    archive: &ReplayArchive,
    large_nar_threshold_bytes: u64,
) -> Result<UploadPlan> {
    /// A batch candidate; ordering uses `item.info.references`.
    struct Candidate {
        item: UploadItem,
        is_drv: bool,
    }

    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();

    let mut large: Vec<UploadItem> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for node in &closure.topo {
        // Non-derivation paths this node needs: its sources, then its
        // declared outputs (floating/CA outputs have no path to supply).
        let node_paths = node
            .input_srcs
            .iter()
            .chain(node.outputs.values().filter(|path| !path.is_empty()));
        for path in node_paths {
            if closure_drvs.contains(path.as_str()) || !seen.insert(path.clone()) {
                continue;
            }
            if target_valid.contains(path) {
                continue;
            }
            match sources.get(path) {
                // The target substitutes these itself; workload outputs are
                // exactly what the replay wants built, not given.
                Some(
                    PathSource::TargetSubstituter | PathSource::NotSupplied { workload: true },
                ) => {}
                Some(PathSource::NotSupplied { workload: false }) => {
                    skipped.push((
                        path.clone(),
                        "not a workload output, not in any target substituter, not embedded \
                         in the archive, and no recording substituter has it"
                            .to_string(),
                    ));
                }
                None => {
                    skipped.push((
                        path.clone(),
                        "path was never resolved by the supply phase (planner input gap) — \
                         not uploaded"
                            .to_string(),
                    ));
                }
                Some(PathSource::Archive) => match archive_upload_item(archive, path) {
                    Ok(item) => candidates.push(Candidate {
                        item,
                        is_drv: false,
                    }),
                    Err(reason) => skipped.push((path.clone(), reason)),
                },
                Some(PathSource::Relay {
                    substituter_url,
                    narinfo,
                }) => match info_from_narinfo(path, narinfo) {
                    Ok(info) => {
                        let item = UploadItem {
                            store_path: path.clone(),
                            info,
                            payload: UploadPayload::Relay {
                                substituter_url: substituter_url.clone(),
                                narinfo: narinfo.clone(),
                            },
                        };
                        if narinfo.nar_size >= large_nar_threshold_bytes {
                            large.push(item);
                        } else {
                            candidates.push(Candidate {
                                item,
                                is_drv: false,
                            });
                        }
                    }
                    Err(reason) => skipped.push((path.clone(), reason)),
                },
            }
        }

        // The .drv itself.
        if !seen.insert(node.drv_path.clone()) || target_valid.contains(&node.drv_path) {
            continue;
        }
        let item = drv_text_item(archive, node)?;
        candidates.push(Candidate { item, is_drv: true });
    }

    // Reference-safe ordering. Everything planned (batch candidates + large)
    // constrains ordering; large items stream individually before the batch,
    // so they count as satisfied from the start.
    let planned: BTreeSet<String> = candidates
        .iter()
        .map(|cand| cand.item.store_path.clone())
        .chain(large.iter().map(|item| item.store_path.clone()))
        .collect();
    let mut satisfied: BTreeSet<String> =
        large.iter().map(|item| item.store_path.clone()).collect();
    let mut pending = candidates;
    let mut batch: Vec<UploadItem> = Vec::new();

    // Settle passes. Candidates are seeded in topo (children-first) order, so
    // cross-node references are already satisfied within pass 1; only
    // same-node sibling-output references can miss it. Expect 1–3 passes in
    // practice — the quadratic worst case needs an adversarial reference
    // graph that real store paths do not produce — so this loop does not
    // need (or want) a smarter scheduler.
    while !pending.is_empty() {
        let mut progressed = false;
        let mut still_pending = Vec::with_capacity(pending.len());
        for cand in pending {
            let ready = cand.item.info.references.iter().all(|reference| {
                reference == &cand.item.store_path
                    || !planned.contains(reference)
                    || satisfied.contains(reference)
            });
            if ready {
                satisfied.insert(cand.item.store_path.clone());
                batch.push(cand.item);
                progressed = true;
            } else {
                still_pending.push(cand);
            }
        }
        pending = still_pending;
        if progressed || pending.is_empty() {
            continue;
        }
        // Stuck: a reference cycle among uploads, or a reference whose own
        // upload was skipped. Derivation texts are never skipped — place the
        // first stuck one (after whatever of its references could be
        // planned) and try again, since that may unblock the rest. Anything
        // else degrades to a skip naming the unsatisfied reference(s).
        if let Some(pos) = pending.iter().position(|cand| cand.is_drv) {
            let cand = pending.remove(pos);
            satisfied.insert(cand.item.store_path.clone());
            batch.push(cand.item);
            continue;
        }
        for cand in pending.drain(..) {
            let unsatisfied: Vec<String> = cand
                .item
                .info
                .references
                .iter()
                .filter(|reference| {
                    *reference != &cand.item.store_path
                        && planned.contains(*reference)
                        && !satisfied.contains(*reference)
                })
                .cloned()
                .collect();
            skipped.push((
                cand.item.store_path,
                format!(
                    "unsatisfied upload reference(s): {}",
                    unsatisfied.join(", ")
                ),
            ));
        }
    }

    tracing::debug!(
        drvs = closure.topo.len(),
        batch = batch.len(),
        large = large.len(),
        skipped = skipped.len(),
        "planned uploads for closure"
    );

    Ok(UploadPlan {
        large,
        batch,
        skipped,
    })
}

/// Store-dir prefix (e.g. `/nix/store/`) of a full store path.
fn store_dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[..=idx],
        None => "/nix/store/",
    }
}

/// Narinfo `References:` / `Deriver:` entries are basenames; the wire
/// path-info wants full store paths. Already-full paths pass through.
fn full_store_path(store_dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        name.to_string()
    } else {
        format!("{store_dir}{name}")
    }
}

/// Build the wire path-info for a path described by a narinfo (archive
/// sidecar or relay substituter): NAR hash decoded to raw bytes, references
/// and deriver expanded to full paths, content address carried through. No
/// signatures and not ultimate — the target re-validates content against the
/// NAR hash, and the recording cache's signatures mean nothing to it.
fn info_from_narinfo(
    store_path: &str,
    narinfo: &NarInfo,
) -> std::result::Result<ValidPathInfo, String> {
    // One decoder for every NarHash spelling (NarHash::parse) — the archive
    // format layer accepts both the nixbase32 and hex narinfo forms, so the
    // planner must decode whatever the format admitted.
    let nar_hash = NarHash::parse(&narinfo.nar_hash).map_err(|err| {
        format!(
            "narinfo NarHash {:?} is not decodable: {err:#}",
            narinfo.nar_hash
        )
    })?;
    let store_dir = store_dir_of(store_path);
    Ok(ValidPathInfo {
        deriver: narinfo
            .deriver
            .as_deref()
            .map(|deriver| full_store_path(store_dir, deriver)),
        nar_hash: nar_hash.digest().to_vec(),
        references: narinfo
            .references
            .iter()
            .map(|reference| full_store_path(store_dir, reference))
            .collect(),
        registration_time: 0,
        nar_size: narinfo.nar_size,
        ultimate: false,
        signatures: Vec::new(),
        content_address: narinfo.ca.clone(),
    })
}

/// Plan the upload of an archive-embedded path. The payload bytes stay in
/// the archive until send time ([`UploadPayload::ArchivePath`]).
fn archive_upload_item(
    archive: &ReplayArchive,
    path: &str,
) -> std::result::Result<UploadItem, String> {
    let narinfo = archive
        .narinfo(path)
        .ok_or_else(|| "embedded in the archive but has no narinfo sidecar".to_string())?;
    let info = info_from_narinfo(path, narinfo)?;
    Ok(UploadItem {
        store_path: path.to_string(),
        info,
        payload: UploadPayload::ArchivePath,
    })
}

/// Plan the upload of a derivation's ATerm text as a single-file NAR.
///
/// The path-info mirrors what a Nix client computes when it adds a `.drv`:
/// `nar_hash`/`nar_size` describe the single-file NAR wrapping the text, the
/// references are exactly what the text mentions (its inputDrvs and
/// inputSrcs), and the content address is `text:sha256:` over the RAW ATerm
/// bytes (not the NAR).
fn drv_text_item(archive: &ReplayArchive, node: &ClosureNode) -> Result<UploadItem> {
    let aterm = archive
        .read_drv(&node.drv_path)
        .with_context(|| format!("planning the upload of {}", node.drv_path))?;
    let aterm_bytes = aterm.into_bytes();
    let text_hash: [u8; 32] = Sha256::digest(&aterm_bytes).into();

    let nar_node = NarNode::Regular {
        executable: false,
        contents: aterm_bytes,
    };
    let mut nar_bytes = Vec::new();
    nar::serialize(&mut nar_bytes, &nar_node)
        .with_context(|| format!("NAR-wrapping the derivation text of {}", node.drv_path))?;
    let nar_hash: [u8; 32] = Sha256::digest(&nar_bytes).into();

    let references: Vec<String> = node
        .input_drvs
        .keys()
        .chain(node.input_srcs.iter())
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();

    Ok(UploadItem {
        store_path: node.drv_path.clone(),
        info: ValidPathInfo {
            deriver: None,
            nar_hash: nar_hash.to_vec(),
            references,
            registration_time: 0,
            nar_size: nar_bytes.len() as u64,
            ultimate: false,
            signatures: Vec::new(),
            content_address: Some(format!("text:sha256:{}", nixbase32::encode(&text_hash))),
        },
        payload: UploadPayload::DrvText(nar_bytes),
    })
}

/// Cross-request upload dedup: the first request to claim a path uploads it;
/// the others either treat it as already present or wait for the claim to
/// land before planning a batch that references it.
#[derive(Debug, Default)]
pub struct UploadClaims {
    /// Store path → claim state, with a `Notify` per pending claim to wake
    /// waiters on completion or release.
    claims: Mutex<HashMap<String, ClaimState>>,
}

/// State of one claimed path.
#[derive(Debug)]
enum ClaimState {
    /// Someone is uploading this path; waiters subscribe to the `Notify`.
    Pending(Arc<Notify>),
    /// The upload landed.
    Done,
}

/// What [`UploadClaims::claim`] tells the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// You won the claim; upload the path and call
    /// [`UploadClaims::complete`] (or [`UploadClaims::release`] on failure).
    Won,
    /// Someone already completed it; treat the path as present.
    AlreadyDone,
    /// Someone else is uploading it; [`UploadClaims::wait`] for it.
    MustWait,
}

impl UploadClaims {
    /// New, empty claim table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the claim table. A panic mid-claim cannot corrupt the map (every
    /// mutation is a single insert/remove), so poisoning is recovered from
    /// to keep the other replay requests going.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, ClaimState>> {
        self.claims.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Try to become the uploader for `path`.
    pub fn claim(&self, path: &str) -> ClaimOutcome {
        let mut claims = self.lock();
        match claims.get(path) {
            Some(ClaimState::Done) => ClaimOutcome::AlreadyDone,
            Some(ClaimState::Pending(_)) => ClaimOutcome::MustWait,
            None => {
                claims.insert(
                    path.to_string(),
                    ClaimState::Pending(Arc::new(Notify::new())),
                );
                ClaimOutcome::Won
            }
        }
    }

    /// Mark a won claim as landed and wake every waiter.
    pub fn complete(&self, path: &str) {
        let mut claims = self.lock();
        if let Some(ClaimState::Pending(notify)) = claims.insert(path.to_string(), ClaimState::Done)
        {
            notify.notify_waiters();
        }
    }

    /// Release a claim that will never complete (the upload failed) so
    /// another request can retry it. Wakes every waiter; a completed claim
    /// stays completed and an unclaimed path is a no-op.
    pub fn release(&self, path: &str) {
        let mut claims = self.lock();
        if !matches!(claims.get(path), Some(ClaimState::Pending(_))) {
            return;
        }
        if let Some(ClaimState::Pending(notify)) = claims.remove(path) {
            notify.notify_waiters();
        }
    }

    /// Wait (bounded) for someone else's claim: `true` = the upload landed,
    /// `false` = the claim was released without completing (or was never
    /// made), or the timeout expired first.
    ///
    /// Missed-notification safety: `Notify::notify_waiters` only wakes
    /// futures that are already registered, so the `notified()` future is
    /// created and enabled BEFORE the claim state is re-checked. A
    /// completion or release that lands before the re-check is observed
    /// directly; one that lands after it wakes the registered future. Either
    /// way nothing is missed.
    pub async fn wait(&self, path: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notify = match self.lock().get(path) {
                Some(ClaimState::Done) => return true,
                None => return false,
                Some(ClaimState::Pending(notify)) => Arc::clone(notify),
            };

            let mut notified = std::pin::pin!(notify.notified());
            // Register interest first, then re-check the state (see above).
            notified.as_mut().enable();
            match self.lock().get(path) {
                Some(ClaimState::Done) => return true,
                None => return false,
                Some(ClaimState::Pending(current)) => {
                    // The claim may have been released and re-claimed since
                    // the snapshot above. In that case `notify` belongs to
                    // the dead claim and will never fire again — loop to
                    // re-register on the live one (the deadline still bounds
                    // the total wait).
                    if !Arc::ptr_eq(current, &notify) {
                        continue;
                    }
                }
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                // Timed out — report the state as it stands right now.
                return matches!(self.lock().get(path), Some(ClaimState::Done));
            }
            // Woken: loop to re-read the state. Done → true; removed →
            // false; if the claim was released and instantly re-claimed by
            // another request, keep waiting on the new claim until the
            // deadline.
        }
    }
}

/// Group `batch` items into topological upload levels: an item's level is
/// `1 + max(level of its planned references)`, and items with no planned
/// references are level 0. A reference is "planned" when it names another
/// item of the batch and is not already valid on the target; self-references
/// never constrain. Returns indices into `batch` grouped by ascending level,
/// preserving batch order within a level.
///
/// `batch` is expected in [`plan_uploads`] order (references before
/// referrers). The one exception that order allows — a force-placed
/// derivation text whose reference appears later — is treated as
/// unconstraining here, mirroring the planner's own decision to place it.
pub fn topo_levels(batch: &[UploadItem], target_valid: &BTreeSet<String>) -> Vec<Vec<usize>> {
    let index_of: HashMap<&str, usize> = batch
        .iter()
        .enumerate()
        .map(|(index, item)| (item.store_path.as_str(), index))
        .collect();

    let mut level_of: Vec<usize> = vec![0; batch.len()];
    let mut levels: Vec<Vec<usize>> = Vec::new();
    for (index, item) in batch.iter().enumerate() {
        let mut level = 0;
        for reference in &item.info.references {
            if reference == &item.store_path || target_valid.contains(reference) {
                continue;
            }
            if let Some(&reference_index) = index_of.get(reference.as_str())
                && reference_index < index
            {
                level = level.max(level_of[reference_index] + 1);
            }
        }
        level_of[index] = level;
        if levels.len() <= level {
            levels.resize_with(level + 1, Vec::new);
        }
        levels[level].push(index);
    }
    levels
}

/// Split one level's items into sub-batches respecting both the byte cap
/// (sum of NAR sizes) and the entry cap, preserving order. An item larger
/// than `max_bytes` on its own gets its own sub-batch. Returns indices into
/// `items`.
pub fn split_batches(items: &[&UploadItem], max_bytes: u64, max_entries: usize) -> Vec<Vec<usize>> {
    let max_entries = max_entries.max(1);
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_bytes: u64 = 0;
    for (index, item) in items.iter().enumerate() {
        let size = item.info.nar_size;
        if size > max_bytes {
            if !current.is_empty() {
                batches.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            batches.push(vec![index]);
            continue;
        }
        if !current.is_empty() && (current.len() >= max_entries || current_bytes + size > max_bytes)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(index);
        current_bytes += size;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const DEP_DRV: &str = "/nix/store/a1111111111111111111111111111111-dep.drv";
    const APP_DRV: &str = "/nix/store/a2222222222222222222222222222222-app.drv";
    const IMPURE_DRV: &str = "/nix/store/a3333333333333333333333333333333-impure.drv";
    const CACHED_DRV: &str = "/nix/store/a4444444444444444444444444444444-cached.drv";
    const SRC: &str = "/nix/store/b1111111111111111111111111111111-src.txt";
    const DEP_OUT: &str = "/nix/store/c1111111111111111111111111111111-dep";
    const APP_OUT: &str = "/nix/store/c2222222222222222222222222222222-app";

    /// The default large-NAR threshold (64 MiB) used by the planning tests.
    const LARGE_THRESHOLD: u64 = 64 * 1024 * 1024;

    fn fixture() -> PathBuf {
        crate::test_manifest_dir().join("tests/fixtures/archive/v0-basic")
    }

    fn open_fixture() -> ReplayArchive {
        ReplayArchive::open(&fixture()).unwrap()
    }

    /// Fabricate a relay narinfo for `store_path` (valid nixbase32 NarHash;
    /// `references` are basenames, like the narinfo text format).
    fn fake_narinfo(store_path: &str, nar_size: u64, references: &[&str]) -> NarInfo {
        NarInfo {
            store_path: store_path.to_string(),
            url: "nar/fabricated.nar".to_string(),
            compression: "none".to_string(),
            nar_hash: format!("sha256:{}", nixbase32::encode(&[0x42u8; 32])),
            nar_size,
            references: references.iter().map(|r| r.to_string()).collect(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
            file_hash: None,
            file_size: None,
        }
    }

    /// One decoder for narinfo NarHash values: the nixbase32 spelling
    /// cache.nixos.org serves and the hex spelling rio-store records both
    /// decode to the same digest, so a hex-sidecar archive (or a hex-serving
    /// relay cache) plans exactly like a nixbase32 one instead of having its
    /// paths silently skipped.
    #[test]
    fn info_from_narinfo_accepts_nixbase32_and_hex_narhash() {
        let digest = [0x42u8; 32];
        let mut base32_form = fake_narinfo(DEP_OUT, 100, &[]);
        base32_form.nar_hash = format!("sha256:{}", nixbase32::encode(&digest));
        let mut hex_form = fake_narinfo(DEP_OUT, 100, &[]);
        hex_form.nar_hash = format!("sha256:{}", hex::encode(digest));

        let from_base32 = info_from_narinfo(DEP_OUT, &base32_form).unwrap();
        let from_hex = info_from_narinfo(DEP_OUT, &hex_form)
            .expect("the hex NarHash spelling must decode, not skip the path");
        assert_eq!(from_base32.nar_hash, digest.to_vec());
        assert_eq!(from_hex.nar_hash, digest.to_vec());

        // A genuinely undecodable NarHash still degrades to a skip reason
        // naming the value.
        let mut bad = fake_narinfo(DEP_OUT, 100, &[]);
        bad.nar_hash = "md5:abcd".to_string();
        let err = info_from_narinfo(DEP_OUT, &bad).unwrap_err();
        assert!(err.contains("md5:abcd"), "{err}");
    }

    /// Skip reason recorded for `path`, or a panic naming what IS skipped.
    fn skip_reason(plan: &UploadPlan, path: &str) -> String {
        plan.skipped
            .iter()
            .find(|(skipped_path, _)| skipped_path == path)
            .map(|(_, reason)| reason.clone())
            .unwrap_or_else(|| panic!("{path} should be skipped, skipped = {:?}", plan.skipped))
    }

    /// The module's core ordering invariant: every reference of a batch item
    /// that is also uploaded by the plan must land before it — earlier in
    /// `batch`, or anywhere in `large` (large items stream before the batch).
    fn assert_batch_reference_safe(plan: &UploadPlan) {
        let large: BTreeSet<&str> = plan.large.iter().map(|i| i.store_path.as_str()).collect();
        let planned: BTreeSet<&str> = plan
            .batch
            .iter()
            .map(|i| i.store_path.as_str())
            .chain(large.iter().copied())
            .collect();
        let mut uploaded: BTreeSet<&str> = large.clone();
        for item in &plan.batch {
            for reference in &item.info.references {
                if reference == &item.store_path || !planned.contains(reference.as_str()) {
                    continue;
                }
                assert!(
                    uploaded.contains(reference.as_str()),
                    "{} is uploaded before its reference {reference}",
                    item.store_path
                );
            }
            uploaded.insert(item.store_path.as_str());
        }
    }

    #[test]
    fn workload_set_excludes_impure_demoted() {
        let archive = open_fixture();
        let workload = workload_set(&archive);
        assert!(workload.drvs.contains(DEP_DRV));
        assert!(workload.drvs.contains(APP_DRV));
        // impure.drv has an expected-outcome record but is demoted via
        // impure-env.json: its outputs get supplied like dependencies
        // instead of rebuilt.
        assert!(!workload.drvs.contains(IMPURE_DRV));
        // cached.drv was a cache hit at record time — no expected outcome.
        assert!(!workload.drvs.contains(CACHED_DRV));
        assert_eq!(workload.drvs.len(), 2);
        // The demotion is reported so the supply stage can record per-unit
        // skip reasons.
        let demoted: Vec<&str> = workload.demoted_impure.iter().map(String::as_str).collect();
        assert_eq!(demoted, vec![IMPURE_DRV]);
    }

    #[test]
    fn walk_closure_topo_order_and_all_paths() {
        let archive = open_fixture();

        let app = walk_closure(&archive, &[APP_DRV.to_string()]).unwrap();
        let order: Vec<&str> = app.topo.iter().map(|n| n.drv_path.as_str()).collect();
        assert_eq!(order, vec![DEP_DRV, APP_DRV], "children before parents");
        for path in [DEP_DRV, APP_DRV, SRC, DEP_OUT, APP_OUT] {
            assert!(
                app.all_paths.contains(path),
                "all_paths must contain {path}"
            );
        }

        // Two roots sharing a dependency: the shared node appears exactly
        // once and before both dependents.
        let multi = walk_closure(&archive, &[APP_DRV.to_string(), IMPURE_DRV.to_string()]).unwrap();
        let order: Vec<&str> = multi.topo.iter().map(|n| n.drv_path.as_str()).collect();
        assert_eq!(order.iter().filter(|p| **p == DEP_DRV).count(), 1);
        let pos = |p: &str| order.iter().position(|x| *x == p).unwrap();
        assert!(pos(DEP_DRV) < pos(APP_DRV));
        assert!(pos(DEP_DRV) < pos(IMPURE_DRV));

        // A derivation the archive does not embed is an error naming it.
        let missing = "/nix/store/f9999999999999999999999999999999-ghost.drv";
        let err = format!(
            "{:#}",
            walk_closure(&archive, &[missing.to_string()]).unwrap_err()
        );
        assert!(err.contains("ghost.drv"), "{err}");
    }

    /// Both closure constructions must agree: membership, the
    /// children-before-parents ordering guarantee, the probe-path set, and
    /// the per-node fields the planner consumes (outputs, input sources,
    /// input-derivation paths). The adjacency form does not record which
    /// outputs of an input derivation are consumed, so only the input
    /// KEYS are compared.
    fn assert_closures_agree(from_drvs: &Closure, from_adjacency: &Closure) {
        let drv_order = |closure: &Closure| -> Vec<String> {
            closure.topo.iter().map(|n| n.drv_path.clone()).collect()
        };
        let drvs_a: BTreeSet<String> = drv_order(from_drvs).into_iter().collect();
        let drvs_b: BTreeSet<String> = drv_order(from_adjacency).into_iter().collect();
        assert_eq!(drvs_a, drvs_b, "closure membership must agree");
        assert_eq!(
            from_drvs.all_paths, from_adjacency.all_paths,
            "probe-path sets must agree"
        );
        for closure in [from_drvs, from_adjacency] {
            let pos: HashMap<&str, usize> = closure
                .topo
                .iter()
                .enumerate()
                .map(|(index, node)| (node.drv_path.as_str(), index))
                .collect();
            for node in &closure.topo {
                for input in node.input_drvs.keys() {
                    if let Some(&input_pos) = pos.get(input.as_str()) {
                        assert!(
                            input_pos < pos[node.drv_path.as_str()],
                            "{input} must precede its consumer {}",
                            node.drv_path
                        );
                    }
                }
            }
        }
        let by_drv = |closure: &Closure| -> BTreeMap<String, ClosureNode> {
            closure
                .topo
                .iter()
                .map(|node| (node.drv_path.clone(), node.clone()))
                .collect()
        };
        let nodes_a = by_drv(from_drvs);
        let nodes_b = by_drv(from_adjacency);
        for (drv, node_a) in &nodes_a {
            let node_b = &nodes_b[drv];
            assert_eq!(node_a.outputs, node_b.outputs, "{drv} outputs");
            assert_eq!(node_a.input_srcs, node_b.input_srcs, "{drv} input sources");
            let inputs_a: BTreeSet<&String> = node_a.input_drvs.keys().collect();
            let inputs_b: BTreeSet<&String> = node_b.input_drvs.keys().collect();
            assert_eq!(inputs_a, inputs_b, "{drv} input derivations");
        }
    }

    #[test]
    fn closure_from_adjacency_matches_aterm_walk() {
        // The v0 fixture has no closures member, so synthesize the same
        // adjacency from its parsed derivation texts and run that through
        // the adjacency-based construction.
        let archive = open_fixture();
        let roots = vec![
            APP_DRV.to_string(),
            IMPURE_DRV.to_string(),
            CACHED_DRV.to_string(),
        ];
        let from_drvs = walk_closure(&archive, &roots).unwrap();

        let mut records = Vec::new();
        for drv_path in [DEP_DRV, APP_DRV, IMPURE_DRV, CACHED_DRV] {
            let drv = Derivation::parse(&archive.read_drv(drv_path).unwrap()).unwrap();
            records.push(ClosureRecord {
                drv: drv_path.to_string(),
                inputs: drv.input_drvs().keys().cloned().collect(),
                srcs: drv.input_srcs().iter().cloned().collect(),
                outputs: drv
                    .outputs()
                    .iter()
                    .map(|o| (o.name().to_string(), Some(o.path().to_string())))
                    .collect(),
            });
        }
        let from_adjacency = closure_from_adjacency(&records, &roots).unwrap();
        assert_closures_agree(&from_drvs, &from_adjacency);

        // The v1 fixture carries a real closures member and declares the
        // dependency-closures capability, so the public entry point picks
        // the adjacency path on its own; it must agree with an explicit
        // ATerm walk over the same archive.
        let v1 = ReplayArchive::open(
            &crate::test_manifest_dir().join("tests/fixtures/archive/v1-basic"),
        )
        .unwrap();
        assert!(v1.capabilities().dependency_closures);
        let v1_roots: Vec<String> = v1.workload_units().iter().cloned().collect();
        let v1_from_archive = walk_closure(&v1, &v1_roots).unwrap();
        let v1_from_drvs = closure_from_drv_texts(&v1, &v1_roots).unwrap();
        assert_closures_agree(&v1_from_drvs, &v1_from_archive);

        // A root with no adjacency record is an error naming it, exactly
        // like a missing derivation text on the ATerm path.
        let missing = "/nix/store/f9999999999999999999999999999999-ghost.drv";
        let err = format!(
            "{:#}",
            closure_from_adjacency(&records, &[missing.to_string()]).unwrap_err()
        );
        assert!(err.contains("ghost.drv"), "{err}");
    }

    #[test]
    fn resolve_source_ladder() {
        let archive = open_fixture();
        let embeds = |path: &str| archive.has_embedded(path);

        let target_cached = "/nix/store/d1111111111111111111111111111111-target-cached";
        let relay_only = "/nix/store/d2222222222222222222222222222222-relay-only";
        let unknown = "/nix/store/e1111111111111111111111111111111-unknown";

        let workload_outputs: BTreeSet<String> = [DEP_OUT.to_string(), APP_OUT.to_string()]
            .into_iter()
            .collect();
        let input_srcs: BTreeSet<String> = [SRC.to_string()].into_iter().collect();
        // The target could substitute dep's output and a relay has it too —
        // the workload rung must still win.
        let target_has: BTreeSet<String> = [target_cached.to_string(), DEP_OUT.to_string()]
            .into_iter()
            .collect();
        let mut relay = HashMap::new();
        relay.insert(
            DEP_OUT.to_string(),
            (
                "https://cache.example.org".to_string(),
                fake_narinfo(DEP_OUT, 120, &[]),
            ),
        );
        relay.insert(
            relay_only.to_string(),
            (
                "https://relay.example.org".to_string(),
                fake_narinfo(relay_only, 64, &[]),
            ),
        );
        let resolve = |path: &str, target_coverage: &BTreeSet<String>| {
            resolve_source(
                path,
                &workload_outputs,
                target_coverage,
                &input_srcs,
                embeds,
                &relay,
                SupplyDependencies::Substituters,
            )
        };

        // (a) workload output: never supplied, whatever else could supply it.
        assert_eq!(
            resolve(DEP_OUT, &target_has),
            PathSource::NotSupplied { workload: true }
        );
        // (b) the target substituter wins for non-workload paths…
        assert_eq!(
            resolve(target_cached, &target_has),
            PathSource::TargetSubstituter
        );
        // …and beats the archive for embedded paths.
        let src_in_target: BTreeSet<String> = [SRC.to_string()].into_iter().collect();
        assert_eq!(resolve(SRC, &src_in_target), PathSource::TargetSubstituter);
        // (c) embedded in the archive and not reachable by the target.
        assert_eq!(resolve(SRC, &target_has), PathSource::Archive);
        // (d) only a recording substituter has it.
        match resolve(relay_only, &target_has) {
            PathSource::Relay {
                substituter_url,
                narinfo,
            } => {
                assert_eq!(substituter_url, "https://relay.example.org");
                assert_eq!(narinfo.store_path, relay_only);
            }
            other => panic!("expected Relay, got {other:?}"),
        }
        // (e) nothing has it.
        assert_eq!(
            resolve(unknown, &target_has),
            PathSource::NotSupplied { workload: false }
        );
    }

    #[test]
    fn resolve_source_policy_short_circuits() {
        // One path per ladder situation: covered by a target substituter
        // (and also embedded), an embedded dependency output, an embedded
        // input source, a relay-only dependency output, a workload output.
        let covered = "/nix/store/d1111111111111111111111111111111-target-covered";
        let embedded_dep = "/nix/store/d2222222222222222222222222222222-embedded-dep";
        let embedded_src = "/nix/store/d3333333333333333333333333333333-embedded-src";
        let relay_only = "/nix/store/d4444444444444444444444444444444-relay-only";
        let workload_out = "/nix/store/d5555555555555555555555555555555-workload-out";

        let workload_outputs: BTreeSet<String> = [workload_out.to_string()].into_iter().collect();
        let target_coverage: BTreeSet<String> = [covered.to_string()].into_iter().collect();
        let input_srcs: BTreeSet<String> = [embedded_src.to_string()].into_iter().collect();
        let embedded: BTreeSet<String> = [
            covered.to_string(),
            embedded_dep.to_string(),
            embedded_src.to_string(),
        ]
        .into_iter()
        .collect();
        let embeds = |path: &str| embedded.contains(path);
        let mut relay = HashMap::new();
        relay.insert(
            relay_only.to_string(),
            (
                "https://relay.example.org".to_string(),
                fake_narinfo(relay_only, 64, &[]),
            ),
        );
        let resolve = |path: &str, dependencies: SupplyDependencies| {
            resolve_source(
                path,
                &workload_outputs,
                &target_coverage,
                &input_srcs,
                embeds,
                &relay,
                dependencies,
            )
        };

        // substituters: the full ladder — coverage delegates, embedded
        // content uploads, relay fills the rest, workload outputs are never
        // supplied.
        let full = SupplyDependencies::Substituters;
        assert_eq!(resolve(covered, full), PathSource::TargetSubstituter);
        assert_eq!(resolve(embedded_dep, full), PathSource::Archive);
        assert_eq!(resolve(embedded_src, full), PathSource::Archive);
        assert!(matches!(
            resolve(relay_only, full),
            PathSource::Relay { .. }
        ));
        assert_eq!(
            resolve(workload_out, full),
            PathSource::NotSupplied { workload: true }
        );

        // embedded-only: the target-substituter rung is skipped — the
        // covered path uploads from the archive instead of being delegated.
        let hermetic = SupplyDependencies::EmbeddedOnly;
        assert_eq!(resolve(covered, hermetic), PathSource::Archive);
        assert_eq!(resolve(embedded_dep, hermetic), PathSource::Archive);
        assert_eq!(resolve(embedded_src, hermetic), PathSource::Archive);
        assert!(matches!(
            resolve(relay_only, hermetic),
            PathSource::Relay { .. }
        ));
        assert_eq!(
            resolve(workload_out, hermetic),
            PathSource::NotSupplied { workload: true }
        );

        // none: dependency outputs are never delivered by any mechanism —
        // covered, embedded, and relayed dependency outputs all resolve to
        // not-supplied; only the input source stays uploadable; workload
        // outputs are unchanged.
        let self_hosted = SupplyDependencies::None;
        assert_eq!(
            resolve(covered, self_hosted),
            PathSource::NotSupplied { workload: false }
        );
        assert_eq!(
            resolve(embedded_dep, self_hosted),
            PathSource::NotSupplied { workload: false }
        );
        assert_eq!(resolve(embedded_src, self_hosted), PathSource::Archive);
        assert_eq!(
            resolve(relay_only, self_hosted),
            PathSource::NotSupplied { workload: false }
        );
        assert_eq!(
            resolve(workload_out, self_hosted),
            PathSource::NotSupplied { workload: true }
        );
    }

    #[test]
    fn plan_uploads_reference_order_and_large_routing() {
        let archive = open_fixture();
        let closure = walk_closure(&archive, &[APP_DRV.to_string()]).unwrap();

        // dep's output is pretend-relayed at 65 MiB so it routes to `large`;
        // app's output is a workload output (the target builds it itself).
        let big = 65 * 1024 * 1024_u64;
        let mut sources: HashMap<String, PathSource> = HashMap::new();
        sources.insert(SRC.to_string(), PathSource::Archive);
        sources.insert(
            DEP_OUT.to_string(),
            PathSource::Relay {
                substituter_url: "https://cache.example.org".to_string(),
                narinfo: fake_narinfo(DEP_OUT, big, &[]),
            },
        );
        sources.insert(
            APP_OUT.to_string(),
            PathSource::NotSupplied { workload: true },
        );

        let plan = plan_uploads(
            &closure,
            &sources,
            &BTreeSet::new(),
            &archive,
            LARGE_THRESHOLD,
        )
        .unwrap();

        assert!(plan.skipped.is_empty(), "skipped: {:?}", plan.skipped);
        assert_batch_reference_safe(&plan);

        // Large routing: the 65 MiB relay item streams individually.
        assert_eq!(plan.large.len(), 1);
        assert_eq!(plan.large[0].store_path, DEP_OUT);
        assert_eq!(plan.large[0].info.nar_size, big);
        match &plan.large[0].payload {
            UploadPayload::Relay {
                substituter_url, ..
            } => assert_eq!(substituter_url, "https://cache.example.org"),
            other => panic!("expected a Relay payload, got {other:?}"),
        }

        // Reference-safe batch order: src before dep.drv before app.drv.
        let batch_paths: Vec<&str> = plan.batch.iter().map(|i| i.store_path.as_str()).collect();
        assert_eq!(batch_paths, vec![SRC, DEP_DRV, APP_DRV]);

        // The embedded source's path-info comes from the narinfo sidecar.
        let src_item = &plan.batch[0];
        assert!(matches!(src_item.payload, UploadPayload::ArchivePath));
        let sidecar = archive.narinfo(SRC).unwrap();
        assert_eq!(src_item.info.nar_size, sidecar.nar_size);
        let decoded = NarHash::parse(&sidecar.nar_hash).unwrap();
        assert_eq!(src_item.info.nar_hash, decoded.digest());
        assert!(src_item.info.references.is_empty());
        assert!(src_item.info.signatures.is_empty());
        assert!(!src_item.info.ultimate);

        // Every DrvText item: text: CA over the raw ATerm bytes, references
        // = inputDrvs ∪ inputSrcs, nar_size = the single-file NAR's length.
        for (drv_path, expected_refs) in [
            (DEP_DRV, vec![SRC.to_string()]),
            (APP_DRV, vec![DEP_DRV.to_string()]),
        ] {
            let item = plan
                .batch
                .iter()
                .find(|i| i.store_path == drv_path)
                .unwrap_or_else(|| panic!("{drv_path} missing from batch"));
            let UploadPayload::DrvText(nar_bytes) = &item.payload else {
                panic!("{drv_path} should be a DrvText upload");
            };

            let aterm = archive.read_drv(drv_path).unwrap();
            let mut expected_nar = Vec::new();
            nar::serialize(
                &mut expected_nar,
                &NarNode::Regular {
                    executable: false,
                    contents: aterm.clone().into_bytes(),
                },
            )
            .unwrap();
            assert_eq!(
                nar_bytes, &expected_nar,
                "{drv_path}: payload must be the single-file NAR of the text"
            );
            assert_eq!(item.info.nar_size, expected_nar.len() as u64);
            let expected_nar_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
            assert_eq!(item.info.nar_hash, expected_nar_hash.to_vec());

            let text_hash: [u8; 32] = Sha256::digest(aterm.as_bytes()).into();
            let expected_ca = format!("text:sha256:{}", nixbase32::encode(&text_hash));
            assert_eq!(
                item.info.content_address.as_deref(),
                Some(expected_ca.as_str())
            );
            assert_eq!(item.info.references, expected_refs, "{drv_path} references");
            assert!(item.info.deriver.is_none());
            assert!(item.info.signatures.is_empty());
            assert!(!item.info.ultimate);
        }
    }

    #[test]
    fn plan_uploads_skips_unsatisfiable_cycle_but_never_drvs() {
        let archive = open_fixture();
        let closure = walk_closure(&archive, &[APP_DRV.to_string()]).unwrap();

        // Fabricate a reference cycle between the two relay-supplied outputs
        // (narinfo references are basenames). Real store paths cannot do
        // this, but the planner must degrade to skips, not hang or abort.
        let dep_out_base = DEP_OUT.strip_prefix("/nix/store/").unwrap();
        let app_out_base = APP_OUT.strip_prefix("/nix/store/").unwrap();
        let mut sources: HashMap<String, PathSource> = HashMap::new();
        sources.insert(SRC.to_string(), PathSource::Archive);
        sources.insert(
            DEP_OUT.to_string(),
            PathSource::Relay {
                substituter_url: "https://cache.example.org".to_string(),
                narinfo: fake_narinfo(DEP_OUT, 120, &[app_out_base]),
            },
        );
        sources.insert(
            APP_OUT.to_string(),
            PathSource::Relay {
                substituter_url: "https://cache.example.org".to_string(),
                narinfo: fake_narinfo(APP_OUT, 200, &[dep_out_base]),
            },
        );

        let plan = plan_uploads(
            &closure,
            &sources,
            &BTreeSet::new(),
            &archive,
            LARGE_THRESHOLD,
        )
        .unwrap();

        // Both cycle members degrade to skips naming the reference that
        // could not be satisfied; nothing else is skipped.
        assert_eq!(plan.skipped.len(), 2, "skipped: {:?}", plan.skipped);
        let dep_reason = skip_reason(&plan, DEP_OUT);
        assert!(
            dep_reason.contains("unsatisfied upload reference"),
            "{dep_reason}"
        );
        assert!(dep_reason.contains(APP_OUT), "{dep_reason}");
        let app_reason = skip_reason(&plan, APP_OUT);
        assert!(
            app_reason.contains("unsatisfied upload reference"),
            "{app_reason}"
        );
        assert!(app_reason.contains(DEP_OUT), "{app_reason}");

        // Derivation texts are never skipped: both are still placed, in
        // reference-safe order, alongside the archive-embedded source.
        let batch_paths: Vec<&str> = plan.batch.iter().map(|i| i.store_path.as_str()).collect();
        assert_eq!(batch_paths, vec![SRC, DEP_DRV, APP_DRV]);
        assert!(plan.large.is_empty());
        assert_batch_reference_safe(&plan);
    }

    #[test]
    fn plan_uploads_target_valid_and_skip_reasons() {
        let archive = open_fixture();
        let closure = walk_closure(&archive, &[APP_DRV.to_string()]).unwrap();

        // The target already has the embedded source; dep's output resolved
        // to "nothing has it"; app's output was never resolved at all.
        let target_valid: BTreeSet<String> = [SRC.to_string()].into_iter().collect();
        let mut sources: HashMap<String, PathSource> = HashMap::new();
        sources.insert(
            DEP_OUT.to_string(),
            PathSource::NotSupplied { workload: false },
        );
        // APP_OUT deliberately absent from the sources map.

        let plan =
            plan_uploads(&closure, &sources, &target_valid, &archive, LARGE_THRESHOLD).unwrap();

        // The already-valid source is not planned, and its absence does not
        // constrain ordering: both derivation texts are still placed.
        assert!(plan.large.is_empty());
        let batch_paths: Vec<&str> = plan.batch.iter().map(|i| i.store_path.as_str()).collect();
        assert!(!batch_paths.contains(&SRC), "{batch_paths:?}");
        assert_eq!(batch_paths, vec![DEP_DRV, APP_DRV]);
        assert_batch_reference_safe(&plan);

        // Skip reasons spell out what was tried and what is actually missing.
        assert!(
            skip_reason(&plan, DEP_OUT).contains("not embedded in the archive"),
            "{:?}",
            plan.skipped
        );
        assert!(
            skip_reason(&plan, APP_OUT).contains("never resolved by the supply phase"),
            "{:?}",
            plan.skipped
        );
        assert_eq!(plan.skipped.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claims_first_wins_then_done_and_release_wakes_waiters() {
        let claims = Arc::new(UploadClaims::new());
        let path = "/nix/store/p1111111111111111111111111111111-shared-dep";

        // First claimer wins; a second claimer must wait.
        assert_eq!(claims.claim(path), ClaimOutcome::Won);
        assert_eq!(claims.claim(path), ClaimOutcome::MustWait);

        // The waiter is woken by complete() and reports the path as landed.
        // Generous timeout: it only bounds a missed-wakeup bug — the wait
        // returns as soon as the notification (or the prior completion) is
        // observed, so the test stays fast.
        let waiter = {
            let claims = Arc::clone(&claims);
            tokio::spawn(async move { claims.wait(path, Duration::from_secs(2)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        claims.complete(path);
        assert!(waiter.await.unwrap(), "wait() must see the completed claim");
        assert_eq!(claims.claim(path), ClaimOutcome::AlreadyDone);

        // Fresh path: the claim is released (upload failed) — the waiter
        // wakes with `false` and the path can be claimed again.
        let path2 = "/nix/store/p2222222222222222222222222222222-flaky-dep";
        assert_eq!(claims.claim(path2), ClaimOutcome::Won);
        let waiter = {
            let claims = Arc::clone(&claims);
            tokio::spawn(async move { claims.wait(path2, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        claims.release(path2);
        assert!(
            !waiter.await.unwrap(),
            "a released claim must not read as landed"
        );
        assert_eq!(claims.claim(path2), ClaimOutcome::Won);

        // Timeout path: nobody completes the claim — wait() reports false.
        let path3 = "/nix/store/p3333333333333333333333333333333-stuck-dep";
        assert_eq!(claims.claim(path3), ClaimOutcome::Won);
        assert!(!claims.wait(path3, Duration::from_millis(25)).await);
    }

    /// A minimal upload item for the pure ordering/sizing helpers.
    fn item(store_path: &str, nar_size: u64, references: &[&str]) -> UploadItem {
        UploadItem {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                deriver: None,
                nar_hash: vec![0u8; 32],
                references: references.iter().map(|r| r.to_string()).collect(),
                registration_time: 0,
                nar_size,
                ultimate: false,
                signatures: Vec::new(),
                content_address: None,
            },
            payload: UploadPayload::DrvText(Vec::new()),
        }
    }

    #[test]
    fn topo_levels_respects_references() {
        let src = "/nix/store/t1111111111111111111111111111111-src";
        let drv = "/nix/store/t2222222222222222222222222222222-thing.drv";
        let out_first = "/nix/store/t3333333333333333333333333333333-out1";
        let out_second = "/nix/store/t4444444444444444444444444444444-out2";
        let already_valid = "/nix/store/t5555555555555555555555555555555-on-target";

        // The drv's only reference is already valid on the target, so it has
        // no planned references and stays independent (level 0); the second
        // output references its sibling output, pushing it one level deeper.
        let batch = vec![
            item(src, 10, &[]),
            item(drv, 10, &[already_valid]),
            item(out_first, 10, &[src]),
            item(out_second, 10, &[src, out_first]),
        ];
        let target_valid: BTreeSet<String> = [already_valid.to_string()].into_iter().collect();

        let levels = topo_levels(&batch, &target_valid);

        // src + the independent drv at level 0 (batch order preserved), the
        // first output at level 1, the sibling-referencing output at level 2.
        assert_eq!(levels, vec![vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn split_batches_caps_bytes_and_entries() {
        let a = item("/nix/store/s1111111111111111111111111111111-a", 100, &[]);
        let b = item("/nix/store/s2222222222222222222222222222222-b", 100, &[]);
        let c = item("/nix/store/s3333333333333333333333333333333-c", 300, &[]);
        let d = item("/nix/store/s4444444444444444444444444444444-d", 50, &[]);
        let items: Vec<&UploadItem> = vec![&a, &b, &c, &d];
        // The byte cap closes [a, b]; c exceeds the cap on its own and gets
        // its own sub-batch; d starts a fresh one.
        assert_eq!(
            split_batches(&items, 250, 3),
            vec![vec![0, 1], vec![2], vec![3]]
        );

        // A single oversized item is its own sub-batch.
        let big = item("/nix/store/s5555555555555555555555555555555-big", 1000, &[]);
        let items: Vec<&UploadItem> = vec![&big];
        assert_eq!(split_batches(&items, 250, 3), vec![vec![0]]);

        // The entry cap also closes sub-batches when bytes never would.
        let small: Vec<UploadItem> = (0..5)
            .map(|index| {
                item(
                    &format!("/nix/store/s666666666666666666666666666666{index}-small{index}"),
                    1,
                    &[],
                )
            })
            .collect();
        let items: Vec<&UploadItem> = small.iter().collect();
        assert_eq!(
            split_batches(&items, 1024 * 1024, 2),
            vec![vec![0, 1], vec![2, 3], vec![4]]
        );
    }
}
