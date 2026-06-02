//! DAG reconstruction and gRPC request building.
//!
//! Translates the per-session derivation cache into `SubmitBuildRequest`
//! messages for the scheduler, walking `inputDrvs` recursively to build
//! the full derivation graph.
// r[impl gw.dag.reconstruct+4]

use std::collections::{BTreeSet, HashMap, HashSet};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::{Derivation, DerivationLike, SizingHint};
use rio_nix::protocol::derived_path::OutputSpec;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// Per-node inline threshold. Most .drv files are 1-10 KB; 64 KB is
/// a generous cap. Anything larger is probably a generated derivation
/// with a huge env (flake inputs serialized) — not worth the bandwidth
/// savings, let the worker fetch it.
const MAX_INLINE_DRV_BYTES: usize = 64 * 1024;

/// Total budget across ALL inlined nodes in one SubmitBuild. Half the
/// gRPC message limit (32 MB). Cold cache with 10k drvs × 10 KB each
/// = 100 MB — WAY over. The budget means we inline the first ~1600
/// average-size drvs, then the rest fall back to worker-fetch. That's
/// still a huge win over inlining zero.
const INLINE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

use crate::drv_cache::{max_transitive_inputs, resolve_derivations_batch};

/// Reconstruct the full derivation DAG starting from a root derivation.
///
/// Performs a BFS walk of `inputDrvs` to discover all transitive dependencies,
/// fetching missing derivations from the store via gRPC as needed.
///
/// Returns `(nodes, edges)` for `SubmitBuildRequest`.
///
/// `root_outputs` is the root request's output selection (`^out,dev` /
/// `^*`). `None` means the opcode carries no selection
/// (`wopBuildDerivation`) — treated like `^*`: every declared output of
/// the root is wanted. It only feeds `wanted_output_names` (see
/// [`populate_wanted_outputs`]); the node/edge set is identical for
/// every value.
///
/// NOTE: all store lookups here are ANONYMOUS (no JWT). This is build
/// INPUT resolution — `.drv` files and their `input_srcs` may have been
/// uploaded via a different tenant context. See `resolve_derivation`
/// in `handler/mod.rs` for the full rationale.
pub async fn reconstruct_dag(
    root_path: &StorePath,
    root_drv: &Derivation,
    root_outputs: Option<&OutputSpec>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<(Vec<types::DerivationNode>, Vec<types::DerivationEdge>)> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    // Level-batched BFS (P0539). The old per-child
    // `resolve_derivation().await` was N sequential RTTs to rio-store
    // (~210s for a 1085-node closure). Processing one BFS level at a
    // time lets us fire all of that level's cache-miss `GetPath` calls
    // concurrently via [`resolve_derivations_batch`]. Same node/edge set
    // as the old walk; only fetch latency changes.
    visited.insert(root_path.to_string());
    let mut current: Vec<(StorePath, Derivation)> = vec![(root_path.clone(), root_drv.clone())];

    let cap = max_transitive_inputs();
    let mut count = 0usize;
    let mut levels = 0usize;
    let mut fetched = 0usize;

    while !current.is_empty() {
        levels += 1;
        let mut frontier: Vec<StorePath> = Vec::new();

        for (drv_path, drv) in current.drain(..) {
            count += 1;
            if count > cap {
                return Err(anyhow::anyhow!(
                    "transitive input limit exceeded: {count} derivations \
                     (max {cap}; raise RIO_MAX_TRANSITIVE_INPUTS to allow larger DAGs)"
                ));
            }
            nodes.push(build_node(drv_path.as_str(), &drv));

            for child_path_str in drv.input_drvs().keys() {
                edges.push(types::DerivationEdge {
                    parent_drv_path: drv_path.to_string(),
                    child_drv_path: child_path_str.clone(),
                });
                if visited.insert(child_path_str.clone()) {
                    // An unparseable store path here means the parent .drv is
                    // corrupt — fail hard rather than silently dropping the edge
                    // (which would leave the DAG incomplete and cause a confusing
                    // "edge references unknown node" error downstream).
                    let child_sp = StorePath::parse(child_path_str).map_err(|e| {
                        anyhow::anyhow!(
                            "corrupted derivation '{drv_path}': invalid inputDrv path \
                             '{child_path_str}': {e}"
                        )
                    })?;
                    frontier.push(child_sp);
                }
            }
        }

        if frontier.is_empty() {
            break;
        }
        let frontier_len = frontier.len();
        // Gate at enqueue, not after fetch. The `count > cap` check
        // above lags one BFS level: a 3-node `current` can declare 3M
        // unique inputDrvs and `count` is still 4 when we hand the
        // frontier to `resolve_derivations_batch` — which buffers ALL
        // of them as parsed `Derivation`s before `insert_drv_bounded`
        // can fire. Reject here so the cap actually bounds peak memory.
        if count + frontier_len > cap {
            return Err(anyhow::anyhow!(
                "transitive input limit exceeded: {} derivations \
                 (max {cap}; raise RIO_MAX_TRANSITIVE_INPUTS to allow larger DAGs)",
                count + frontier_len
            ));
        }
        // If any child can't be resolved (store unreachable, .drv missing
        // from store), the build cannot proceed: a stub leaf with
        // system="" would never match any worker and hang forever. Fail
        // now with a clear error. The level-wide error context loses the
        // specific parent path the per-child loop reported, but the
        // failing child path is still in the underlying error.
        current = resolve_derivations_batch(frontier, store_client, drv_cache)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "cannot resolve {frontier_len} dependencies at BFS level {levels}: {e} \
                     (store unreachable or .drv missing; build cannot proceed)"
                )
            })?;
        fetched += frontier_len;
    }

    debug!(
        nodes = nodes.len(),
        edges = edges.len(),
        levels,
        store_fetches = fetched,
        "DAG reconstruction complete"
    );

    // Populate ca_modular_hash for CA nodes. AFTER BFS so
    // hash_derivation_modulo has the full drv_cache to recurse
    // over (InputNotFound otherwise). Memoised via a single
    // shared hash_cache across all nodes — the recursive walk
    // hits every sub-hash once regardless of how many CA nodes
    // reference it. Best-effort: hash failure → log, leave empty
    // (scheduler's collect_ca_inputs skips; resolve degrades to
    // worker-fail-on-placeholder + retry).
    populate_ca_modular_hashes(&mut nodes, drv_cache);

    // Populate needs_resolve for the ia.deferred case: an IA (or
    // fixed-CA) derivation whose inputDrvs include a floating-CA
    // child has that child's placeholder path embedded in its
    // env/args — it needs resolve even though it's not floating-CA
    // itself. AFTER BFS so every child is in drv_cache.
    populate_needs_resolve(&mut nodes, drv_cache);

    // Populate wanted_output_names: the union of every consumer's
    // inputDrvs output-name set ∪ the root request's OutputSpec.
    // AFTER BFS so every consumer's parsed inputDrvs map is in
    // drv_cache. Empty = all declared outputs wanted.
    populate_wanted_outputs(&mut nodes, drv_cache, root_path.as_str(), root_outputs);

    Ok((nodes, edges))
}

/// Yield `(node_idx, &node, &Derivation)` for every node whose
/// `drv_path` is in `drv_cache`. Logs BFS-inconsistency at debug
/// for misses (the cache was populated BY the BFS — a miss means
/// the BFS and this walk disagree about what nodes exist; our bug,
/// not the operator's). Shared scaffold for every post-BFS
/// `populate_*` pass.
///
/// Index-based because callers need to write `nodes[idx].field`
/// AFTER the lookup. Yielding `&mut Node` would alias the iterator's
/// own immutable borrow of `nodes` (it reads `node.drv_path`). The
/// collect-then-apply pattern at each call-site sidesteps the split.
fn iter_cached_drvs<'a>(
    nodes: &'a [types::DerivationNode],
    drv_cache: &'a HashMap<StorePath, Derivation>,
    walker_name: &'static str,
) -> impl Iterator<Item = (usize, &'a types::DerivationNode, &'a Derivation)> + 'a {
    nodes.iter().enumerate().filter_map(move |(idx, node)| {
        let sp = StorePath::parse(&node.drv_path).ok()?;
        match drv_cache.get(&sp) {
            Some(drv) => Some((idx, node, drv)),
            None => {
                debug!(
                    drv_path = %node.drv_path,
                    walker = walker_name,
                    "drv not in cache (BFS inconsistency)"
                );
                None
            }
        }
    })
}

/// Compute [`hash_derivation_modulo`] via a `drv_cache`-backed
/// resolver. Pass the same `hash_cache` across calls to reuse
/// sub-hashes. Errors warn-and-return-`None` — both callers
/// (translate's populate + handler's builtOutputs) treat "no hash"
/// as log + degrade-gracefully.
///
/// [`hash_derivation_modulo`]: rio_nix::derivation::hash_derivation_modulo
pub(crate) fn compute_modular_hash_cached(
    drv: &Derivation,
    drv_path: &str,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Option<[u8; 32]> {
    let resolve = |p: &str| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp));
    match rio_nix::derivation::hash_derivation_modulo(drv, drv_path, &resolve, hash_cache) {
        Ok(hash) => Some(hash),
        Err(e) => {
            warn!(
                drv_path = %drv_path,
                error = %e,
                "hash_derivation_modulo failed; caller will degrade"
            );
            None
        }
    }
}

/// Fill `ca_modular_hash` on EVERY node with a cached full derivation
/// via `hash_derivation_modulo`.
///
/// Three consumers, three node populations:
///   - CA nodes: the scheduler's CA-on-CA resolve queries `realisations`
///     keyed on `(modular_hash, output_name)`;
///   - deferred-IA nodes (empty output path): the scheduler writes a
///     realisation on completion keyed by this hash so the gateway's
///     `wopQueryDerivationOutputMap` can answer the client;
///   - plain IA nodes with statically-known paths: the hash is the
///     identity evidence the scheduler's ingress inline-content binding
///     and Follow-up store-evidence displacement consume — it lets the
///     scheduler verify a declared IA output path against inline bytes
///     by seeding `input_addressed_output_paths`' hash cache with the
///     children's hashes, no store access needed. (Previously these
///     nodes carried no hash — "dead bytes on the wire" — which made
///     every IA declared path unverifiable at ingress.)
///
/// The modular hash needs the full transitive closure of parsed
/// derivations (what BFS put in `drv_cache`). Memoised via one shared
/// `hash_cache` — for N nodes sharing a common input, the common
/// sub-hash is computed once.
///
/// Best-effort: hash failure → warn, leave empty. Scheduler's
/// `collect_ca_inputs` skips empty; resolve degrades to worker-fail
/// and retry-with-backoff; ingress treats a missing hash as no-evidence
/// (fail-closed for authoritative claims, declaration-only for
/// store-backed ones).
// r[impl gw.dag.modulo-hash-all-nodes]
fn populate_ca_modular_hashes(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) {
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
    let hashes: Vec<(usize, Vec<u8>)> =
        iter_cached_drvs(nodes, drv_cache, "populate_ca_modular_hashes")
            .filter_map(|(idx, node, drv)| {
                compute_modular_hash_cached(drv, &node.drv_path, drv_cache, &mut hash_cache)
                    .map(|h| (idx, h.to_vec()))
            })
            .collect();
    for (idx, h) in hashes {
        nodes[idx].ca_modular_hash = h;
    }
}

/// Set `needs_resolve` for nodes with unresolved-path inputs (`ia.deferred`).
///
/// ADR-018 Appendix B: Nix's `shouldResolve` returns true for IA
/// derivations when they're "deferred" — i.e., they have an input whose
/// output path is a placeholder at eval time. The parent's env/args
/// reference that placeholder, so dispatch-time resolve must rewrite it
/// to the realized path.
///
/// [`build_node`] already set `needs_resolve = has_ca_floating_outputs()`
/// (self-floating always resolves). This pass ORs in the
/// any-child-has-unknown-output-path case — that covers BOTH floating-CA
/// children AND deferred-IA children. CppNix's `derivationStrict`
/// propagates the deferred kind upward (every IA whose input has an
/// unknown path becomes `DerivationOutput::Deferred{}` with empty path
/// itself), so every node in a deferred chain has empty output paths and
/// this propagates transitively in a single pass; no fixpoint needed.
/// Concrete-IA and FOD children have non-empty paths so are unaffected.
///
/// AFTER BFS so every child drv is in `drv_cache`. Missing children
/// (BFS inconsistency) → skip; the node keeps its self-computed value.
/// Same degrade as `populate_ca_modular_hashes`.
fn populate_needs_resolve(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) {
    let deferred: Vec<usize> = iter_cached_drvs(nodes, drv_cache, "populate_needs_resolve")
        .filter(|(_, node, _)| !node.needs_resolve)
        .filter(|(_, _, drv)| {
            drv.input_drvs().keys().any(|child_path| {
                StorePath::parse(child_path)
                    .ok()
                    .and_then(|sp| drv_cache.get(&sp))
                    .is_some_and(|child| child.has_unknown_output_paths())
            })
        })
        .map(|(idx, _, _)| idx)
        .collect();
    for idx in deferred {
        nodes[idx].needs_resolve = true;
    }
}

/// Fill `wanted_output_names` on each node: the union of every
/// consumer's `inputDrvs[node]` output-name set, ∪ the root request's
/// `OutputSpec` for the root node. EMPTY means "all declared outputs
/// wanted" — the conservative pre-existing behaviour.
///
/// Also marks the root node `explicitly_requested`: it is the node the
/// client named as a build target, and a multi-target request can fold
/// it inside another target's closure where it is no longer a
/// structural root of the combined submission — the scheduler's
/// roots-only prune keys on the flag to keep verifying and retaining
/// it (`dedup_dag` ORs the flag across duplicate copies).
///
/// - A `^*` root (`OutputSpec::All`) and a root with no spec at all
///   (`wopBuildDerivation` carries no output selection) keep the empty
///   sentinel. Empty SATURATES the union: "all" ∪ X = "all", so a node
///   that any contributor wants in full stays empty no matter what the
///   other contributors name.
/// - A `^out,dev` root (`OutputSpec::Names`) contributes those names to
///   the root node's union. Within one BFS the root has no consumers
///   (a consumer would be a cycle), but the union is written
///   symmetrically so the same node reached as a *dependency* of
///   another root in a multi-root submission merges correctly in
///   `dedup_dag`.
/// - A BFS-reachable non-root node always has at least one consumer
///   naming it (the BFS only discovers nodes via some parent's
///   inputDrvs). If its consumer is somehow missing from `drv_cache`
///   (BFS inconsistency — our bug), the node keeps the empty (= all
///   wanted) sentinel: conservative, never under-wanting.
///
/// The result is sorted for determinism — the scheduler's PG upsert
/// unions arrays with `ORDER BY 1`, so a sorted wire value keeps the
/// in-memory and persisted sets byte-identical.
///
/// Only the scheduler's cache-hit / substitutability classification
/// reads this; `output_names` / `expected_output_paths` keep the full
/// declared set (assignment-token allowlist, GC pins, client report).
// r[impl gw.dag.reconstruct+4]
fn populate_wanted_outputs(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
    root_path: &str,
    root_outputs: Option<&OutputSpec>,
) {
    // child drv_path → union of every consumer's named outputs.
    // BTreeSet gives the sorted-dedup for free. Owned strings: a
    // borrowed map would carry `iter_cached_drvs`'s unified borrow of
    // `nodes` into the write-back loop below, which needs `nodes`
    // mutably (the same split the sibling passes sidestep with their
    // collect-then-apply shape).
    let mut wanted: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (_, _, drv) in iter_cached_drvs(nodes, drv_cache, "populate_wanted_outputs") {
        for (child_path, output_names) in drv.input_drvs() {
            wanted
                .entry(child_path.clone())
                .or_default()
                .extend(output_names.iter().cloned());
        }
    }

    // The root's own demand. Names joins the union; All / no-spec pins
    // the root to the empty (= all wanted) sentinel, overriding any
    // consumer contribution (all ∪ X = all).
    let mut root_wants_all = false;
    match root_outputs {
        Some(OutputSpec::Names(names)) => {
            wanted
                .entry(root_path.to_string())
                .or_default()
                .extend(names.iter().cloned());
        }
        Some(OutputSpec::All) | None => root_wants_all = true,
    }

    for node in nodes.iter_mut() {
        if node.drv_path == root_path {
            // The client named this node as a build target. The flag —
            // not root-ness of the combined submission — is what the
            // scheduler's prune treats as authoritative demand.
            node.explicitly_requested = true;
        }
        if root_wants_all && node.drv_path == root_path {
            // Already empty from build_node, but be explicit: the
            // saturated "all wanted" sentinel wins over anything.
            node.wanted_output_names.clear();
            continue;
        }
        if let Some(names) = wanted.get(node.drv_path.as_str()) {
            node.wanted_output_names = names.iter().cloned().collect();
        }
        // else: no consumer names it and it isn't a Names-root —
        // keep the empty (= all wanted) sentinel from build_node.
    }
}

/// Validate a DAG before SubmitBuild — the CHEAP checks only. Returns
/// `Err(reason)` if the DAG should be rejected — caller sends
/// STDERR_ERROR with the reason. Returns `Ok(offenders)` if valid,
/// where `offenders` lists derivations that declare an
/// `outputHash`/`outputHashAlgo` the builder cannot verify or finalize:
/// those are NOT rejected here — the caller must pass them to
/// [`reject_unrealized_fod_offenders`], which exempts them only when
/// every declared output is already present and visible to the
/// submitting tenant, or substitutable from that tenant's configured
/// upstreams (one bounded tenant-scoped probe; rejects otherwise,
/// fail-closed).
///
/// Checks:
/// - `__noChroot=1` in any node's env → reject (sandbox escape)
/// - `nodes.len() > MAX_DAG_NODES` → reject (early, before gRPC)
/// - floating-CA-shaped outputs declaring an output path → reject
///   (CppNix cannot produce that shape; applies to unverifiable-algo
///   offenders too)
/// - declared-hash (fixed-output) outputs with a verifiable algo:
///   declared path must derive from the declared hash
/// - outputs with an unverifiable algo → classified as offenders and
///   returned (the FOD hash gate and floating-CA finalization are
///   fail-closed worker-side; a build of such a node could only ever
///   fail after burning a pod — but a node whose outputs already exist
///   never dispatches, so the realization probe decides)
///
/// The expensive declared-output-path binding (one
/// `hashDerivationModulo`-shaped pass over every cached derivation)
/// deliberately does NOT live here: it runs later in the pipeline via
/// [`validate_output_path_bindings`], on the blocking pool, behind the
/// rate-limit and quota gates — see the pipeline order in
/// `handler/build.rs`. Keeping this function cheap means a
/// rate-limited or over-quota client cannot make the gateway burn CPU
/// hashing its closure first.
///
/// The scheduler ALSO enforces MAX_DAG_NODES (grpc/mod.rs:298);
/// this is an early reject to save the gRPC round-trip for obvious
/// over-size submissions. The __noChroot check is ONLY here — the
/// scheduler doesn't have the env (DerivationNode doesn't carry it).
pub fn validate_dag(
    nodes: &[types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) -> Result<Vec<UnverifiableFodOffender>, String> {
    // MAX_DAG_NODES: early reject. Scheduler enforces too but
    // this saves a 100MB+ gRPC message for obvious over-size.
    if nodes.len() > rio_common::limits::MAX_DAG_NODES {
        return Err(format!(
            "DAG too large: {} nodes > {} max",
            nodes.len(),
            rio_common::limits::MAX_DAG_NODES
        ));
    }

    // r[impl gw.reject.nochroot+2]
    // __noChroot check: iterate nodes, look up each drv in the
    // cache (it was populated during BFS), check env. Nodes
    // without a cached drv (BasicDerivation fallback) are
    // skipped — we don't have the env. A __noChroot drv
    // arriving via BasicDerivation is a corner case (client
    // sent a pre-parsed BasicDerivation without inputDrvs);
    // the build would fail at the worker's sandbox anyway
    // (sandbox=true, sandbox-fallback=false), so the check
    // here is best-effort early rejection.
    //
    // Why reject: __noChroot=1 tells nix-daemon to skip the
    // sandbox. That's a sandbox escape — the build sees /etc,
    // $HOME, the host network, everything. Allowed in single-
    // user Nix for bootstrap derivations; NEVER allowed in a
    // multi-tenant build farm. A malicious .drv could use this
    // to exfiltrate secrets from the worker.
    for (_, node, drv) in iter_cached_drvs(nodes, drv_cache, "validate_dag") {
        match StructuredEnv::new(drv.env()).bool_attr("__noChroot") {
            Ok(Some(true)) => {
                return Err(format!(
                    "derivation {} requests __noChroot (sandbox escape) — not permitted",
                    node.drv_path
                ));
            }
            Ok(_) => {}
            // Fail-closed: a sandbox-shape attribute the gateway cannot
            // type (wrong-typed __noChroot, unparseable __json) is
            // rejected — never guessed at. Oracle parity: getBoolAttr →
            // getBoolean THROWS on non-bools; "absent" is the only safe
            // default, and an unreadable blob is not "absent".
            Err(e) => {
                return Err(format!(
                    "derivation {}: __noChroot is unreadable ({e}) — \
                     wrong-typed sandbox attributes are not permitted",
                    node.drv_path
                ));
            }
        }
    }

    // r[impl gw.reject.unsupported-hash-algo+4]
    // Outputs whose declared hash algorithm the builder cannot verify
    // are CLASSIFIED here and decided by the realization probe
    // (`reject_unrealized_fod_offenders`). For fixed-output derivations
    // the builder's `verify_fod_hashes` is fail-closed (it is the sole
    // worker-side content check between an egress-open fetcher and the
    // signed cache); for floating-CA outputs (hash_algo set, hash
    // empty) `FloatingCaSpec::from_outputs` is equally fail-closed
    // (`CaUnsupportedAlgo`) — but only after the build has run to
    // completion on a builder pod. A node that would BUILD with such an
    // algo can therefore only ever fail and is rejected; a node whose
    // declared outputs are already realized in the store never
    // dispatches (the scheduler cache-cuts it), so rejecting the whole
    // submission for it would block otherwise-valid DAGs that merely
    // reference a legacy (e.g. md5) FOD that already exists. Offender
    // nodes skip the declared-hash binding below (the algo cannot be
    // parsed), but the floating-CA shape rule still applies to them.
    // Input-addressed outputs (no hash, no algo) are untouched.
    //
    // r[impl gw.reject.output-path-mismatch+2]
    // Declared-hash (fixed-output) outputs: bind the declared path to
    // the declared hash and enforce CppNix's single-'out' shape rule.
    // Without this a junk outputHash would exempt an arbitrary declared
    // path from every submission-time trusted-plane binding (the
    // builder-side check is defense in depth; the store independently
    // re-verifies FOD uploads and rejects descriptor-less ones under a
    // scheduler-signed fixed-output assignment — but only at upload
    // time, after a pod has already run).
    let mut offenders = Vec::new();
    for (_, node, drv) in iter_cached_drvs(nodes, drv_cache, "validate_dag") {
        let unverifiable = drv.outputs().iter().any(|o| {
            (!o.hash().is_empty() || !o.hash_algo().is_empty())
                && !fod_algo_verifiable(o.hash_algo())
        });
        if unverifiable {
            // The floating-CA shape rule applies to every output,
            // offenders included — a path-declaring floating-CA output
            // must never slip through via an unparseable algo.
            validate_floating_ca_shape(
                &node.drv_path,
                drv.outputs()
                    .iter()
                    .map(|o| (o.name(), o.path(), o.hash_algo(), o.hash())),
            )?;
            let out = drv
                .outputs()
                .iter()
                .find(|o| {
                    (!o.hash().is_empty() || !o.hash_algo().is_empty())
                        && !fod_algo_verifiable(o.hash_algo())
                })
                .expect("checked by `unverifiable` above");
            offenders.push(UnverifiableFodOffender {
                drv_path: node.drv_path.clone(),
                output_name: out.name().to_string(),
                algo: out.hash_algo().to_string(),
                declared_paths: drv.outputs().iter().map(|o| o.path().to_string()).collect(),
            });
            continue;
        }
        validate_declared_hash_outputs(
            &node.drv_path,
            drv.outputs()
                .iter()
                .map(|o| (o.name(), o.path(), o.hash_algo(), o.hash())),
        )?;
    }

    Ok(offenders)
}

/// A derivation that declares an `outputHash`/`outputHashAlgo` the
/// builder cannot verify or finalize, classified by [`validate_dag`]
/// (or built directly from an inline `BasicDerivation`). The
/// submission-time decision for these nodes is made by
/// [`reject_unrealized_fod_offenders`]: exempt iff every declared
/// output path is already realized for the submitting tenant or
/// substitutable from its upstreams.
#[derive(Debug, Clone)]
pub struct UnverifiableFodOffender {
    pub drv_path: String,
    pub output_name: String,
    pub algo: String,
    /// Every declared output path of the derivation (not just the
    /// offending output's): exemption requires the WHOLE node to be a
    /// guaranteed cache-hit, mirroring the scheduler's skip-dispatch
    /// predicate (all expected outputs present).
    pub declared_paths: Vec<String>,
}

/// Cap on the total number of store paths the unverifiable-algo
/// realization probe will check in one submission. A DAG referencing
/// more legacy-algo derivations than this is rejected fail-closed —
/// the probe must stay one bounded RPC, not a vector for amplifying
/// store load.
pub(crate) const MAX_FOD_EXEMPTION_PROBE_PATHS: usize = 1024;

/// Decide the unverifiable-algo offenders collected by
/// [`validate_dag`]: exempt a node iff every one of its declared
/// output paths either is already present and visible to the
/// submitting tenant (the scheduler cache-cuts it) or is
/// substitutable from the tenant's configured upstreams (the
/// scheduler's substitute lane completes it without dispatching);
/// reject otherwise. This mirrors exactly the scheduler's no-dispatch
/// predicate, evaluated with the same tenant identity.
///
/// Fail-closed by construction: any offender with an empty or
/// unparseable declared path (floating-CA with an unsupported algo has
/// no path at all), an offender set larger than
/// [`MAX_FOD_EXEMPTION_PROBE_PATHS`], an indeterminate probe answer
/// (upstream 429/5xx/deadline), a store error, or a probe timeout all
/// reject the submission — the gate never exempts a node it cannot
/// prove will skip dispatch. The probe is a single bounded
/// `FindMissingPaths` carrying the session tenant token (anonymous
/// only in dual-mode, matching the scheduler's anonymous merge probe
/// in that mode), so the answer is the same one the scheduler will
/// act on at merge time.
///
/// The exemption can go stale between this probe and dispatch (GC
/// races, or a substitute fetch that fails after a positive upstream
/// probe): the node then dispatches and fails at the worker's
/// fail-closed FOD gate — a node-level failure instead of a
/// submission rejection, never an unverified build.
// r[impl gw.reject.unsupported-hash-algo+4]
pub(crate) async fn reject_unrealized_fod_offenders(
    offenders: &[UnverifiableFodOffender],
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
) -> Result<(), String> {
    if offenders.is_empty() {
        return Ok(());
    }

    let remediation = "re-pin the derivation to a supported outputHashAlgo (supported: sha1, \
                       sha256, sha512, optionally 'r:'-prefixed) or copy its output into the \
                       store first";

    let mut probe_paths: Vec<String> = Vec::new();
    for offender in offenders {
        if offender.declared_paths.is_empty()
            || offender
                .declared_paths
                .iter()
                .any(|p| p.is_empty() || StorePath::parse(p).is_err())
        {
            return Err(format!(
                "derivation {} output '{}' declares unsupported outputHashAlgo '{}' and its \
                 declared output paths cannot be checked for prior realization — {remediation}",
                offender.drv_path, offender.output_name, offender.algo
            ));
        }
        probe_paths.extend(offender.declared_paths.iter().cloned());
    }

    if probe_paths.len() > MAX_FOD_EXEMPTION_PROBE_PATHS {
        return Err(format!(
            "{} derivations declare unsupported outputHashAlgo values ({} output paths > {} \
             probe cap) — {remediation}",
            offenders.len(),
            probe_paths.len(),
            MAX_FOD_EXEMPTION_PROBE_PATHS
        ));
    }

    // The probe carries the session tenant token (gw.jwt.propagate):
    // the exemption must be decided with the same tenant-scoped answer
    // the scheduler's merge-time cache-cut and substitute lane will
    // act on. In dual-mode (no JWT) nothing is attached and the store
    // answers anonymously — matching the scheduler's anonymous merge
    // probe in that mode.
    let probe_req = crate::handler::with_jwt(
        types::FindMissingPathsRequest {
            store_paths: probe_paths,
        },
        jwt_token,
    )
    .map_err(|e| {
        format!(
            "cannot verify prior realization of derivations declaring unsupported \
             outputHashAlgo values (probe construction failed: {e}) — {remediation}"
        )
    })?;
    let (missing, substitutable): (HashSet<String>, HashSet<String>) = match tokio::time::timeout(
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        store_client.find_missing_paths(probe_req),
    )
    .await
    {
        Ok(Ok(r)) => {
            let r = r.into_inner();
            (
                r.missing_paths.into_iter().collect(),
                // indeterminate_paths are deliberately NOT collected as
                // exemptable: neither confirmed-present nor confirmed-
                // substitutable answers fail closed.
                r.substitutable_paths.into_iter().collect(),
            )
        }
        Ok(Err(e)) => {
            return Err(format!(
                "cannot verify prior realization of derivations declaring unsupported \
                 outputHashAlgo values (store error: {e}) — {remediation}"
            ));
        }
        Err(_) => {
            return Err(format!(
                "cannot verify prior realization of derivations declaring unsupported \
                 outputHashAlgo values (store probe timed out) — {remediation}"
            ));
        }
    };

    for offender in offenders {
        // Exempt iff every declared output is present-and-visible to
        // the submitting tenant OR substitutable from its upstreams —
        // i.e. the node will cache-cut or substitute, never dispatch.
        // Plain-missing and indeterminate paths reject (fail-closed).
        if let Some(missing_path) = offender
            .declared_paths
            .iter()
            .find(|p| missing.contains(*p) && !substitutable.contains(*p))
        {
            return Err(format!(
                "derivation {} output '{}' declares unsupported outputHashAlgo '{}' and its \
                 declared output {} is not already realized in the store for this tenant (nor \
                 substitutable from its configured upstreams) — the build could only fail \
                 after burning a pod; {remediation}",
                offender.drv_path, offender.output_name, offender.algo, missing_path
            ));
        }
        info!(
            drv_path = %offender.drv_path,
            algo = %offender.algo,
            "exempting unverifiable-outputHashAlgo derivation: all declared outputs already \
             realized or substitutable (node will cache-cut or substitute, never dispatch)"
        );
    }

    Ok(())
}

/// Trusted-plane binding of declared output paths to the paths the
/// derivation itself derives to. Returns `Err(reason)` when any cached
/// derivation declares an input-addressed output path that does not
/// match the derived one — the caller rejects the submission before
/// `SubmitBuild` exactly like a [`validate_dag`] rejection.
///
/// Workers are untrusted, so this is the trusted-plane enforcement
/// (the builder-side fixed-output binding is defense in depth):
/// without it, any tenant could declare another derivation's
/// not-yet-built input-addressed output path on a crafted .drv and
/// have arbitrary content built, signed and served at that path.
/// Mirrors CppNix, which recomputes IA output paths from the
/// derivation (hashDerivationModulo + makeOutputPath) and never
/// trusts the declared ones.
///
/// Scope: every input-addressed output with a NON-EMPTY declared path
/// is validated — paths that match the derivation are accepted, paths
/// that do not parse as store paths are rejected outright. (A malformed
/// declared path cannot alias a store object, but it CAN reach the
/// worker glue and the result pipeline as a tenant-controlled string
/// where a store path is expected; workers are untrusted
/// (`sec.trust.workers-untrusted`), so the trusted plane must not
/// forward it.) Empty declared paths (deferred IA) have nothing to
/// validate. Fixed-output outputs are bound to their declared hash by
/// the declared-hash gate in [`validate_dag`], and floating-CA outputs
/// have no static path to check. Nodes without a cached full derivation
/// (BasicDerivation fallback) are skipped like the cheap checks;
/// closure-incomplete derivations are rejected fail-closed — an
/// attacker must not be able to dodge the check by withholding an
/// input drv.
///
/// Cost: roughly two full ATerm serializations + SHA-256 per cached
/// derivation (CppNix pays the same at instantiation). The pipeline
/// therefore runs this AFTER the rate-limit and quota gates and on the
/// blocking pool (`spawn_blocking`) — see
/// `handler::build::enforce_output_path_bindings` — so a single
/// adversarial closure cannot stall the session reactor or bypass the
/// per-tenant limiter.
// r[impl gw.reject.output-path-mismatch+2]
pub(crate) fn validate_output_path_bindings(
    nodes: &[types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) -> Result<(), String> {
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
    let resolve = |p: &str| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp));
    for (_, node, drv) in iter_cached_drvs(nodes, drv_cache, "validate_output_path_bindings") {
        // Per-output classification: only plain input-addressed
        // outputs (empty hash_algo) with a parseable declared path
        // are bound by the derivation hash; content-bound outputs
        // (fixed-output / floating-CA) are governed by the
        // content-hash rules and deferred (empty) paths have
        // nothing to validate. (A floating-CA output that DOES
        // declare a path is rejected earlier by validate_dag's
        // gw.reject.floating-ca-declared-path+1 shape rule, so it can
        // never reach — let alone exempt itself from — this gate.)
        // The derivation-level fast path
        // therefore applies only when EVERY output is
        // content-bound — neither a deferred output nor a
        // floating-CA output may exempt a sibling static path
        // from validation. Mixed shapes (which Nix itself never
        // produces: "can't mix derivation output types") fall
        // through to input_addressed_output_paths(), which
        // refuses them, and the submission is rejected
        // fail-closed rather than half-validated.
        if drv.outputs().iter().all(|o| !o.hash_algo().is_empty()) {
            continue;
        }
        if !drv
            .outputs()
            .iter()
            .any(|o| o.hash_algo().is_empty() && !o.path().is_empty())
        {
            continue;
        }
        let derived = rio_nix::derivation::input_addressed_output_paths(
            drv,
            &node.drv_path,
            &resolve,
            &mut hash_cache,
        )
        .map_err(|e| {
            format!(
                "cannot derive output paths for {} (rejecting rather than trusting the \
                 declared ones): {e}",
                node.drv_path
            )
        })?;
        for output in drv.outputs() {
            // Content-bound outputs are not derivation-path-derived.
            if !output.hash_algo().is_empty() {
                continue;
            }
            // Deferred IA (empty declared path): the path is computed at
            // resolution time; nothing to bind yet.
            if output.path().is_empty() {
                continue;
            }
            // A non-empty declared path that does not parse as a store
            // path is rejected fail-closed: it can never equal the
            // derivation-derived path, and forwarding it would hand the
            // worker glue and result pipeline a tenant-controlled string
            // where a store path is expected.
            if let Err(e) = StorePath::parse(output.path()) {
                return Err(format!(
                    "derivation {} declares output '{}' at {:?}, which is not a valid store \
                     path: {e} — declared output paths must parse as store paths",
                    node.drv_path,
                    output.name(),
                    output.path(),
                ));
            }
            match derived.get(output.name()) {
                Some(expected) if expected.as_str() == output.path() => {}
                Some(expected) => {
                    return Err(format!(
                        "derivation {} declares output '{}' at {} but the derivation \
                         derives to {} — declared output paths must match the derivation",
                        node.drv_path,
                        output.name(),
                        output.path(),
                        expected.as_str(),
                    ));
                }
                None => {
                    return Err(format!(
                        "derivation {} output '{}' has no derivable output path",
                        node.drv_path,
                        output.name(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Trusted-plane binding of declared-hash (fixed-output) output paths to
/// their declared hash. Mirrors rio-builder's
/// `validate_fixed_output_declarations` (the worker-side check is
/// defense in depth — workers are untrusted) and CppNix:
/// a CAFixed output's path is never trusted, every consumer recomputes
/// `makeFixedOutputPath`, and `BasicDerivation::type()` requires that a
/// derivation with a fixed output consists of exactly one output named
/// `out` ("only one fixed output is allowed", "can't mix derivation
/// output types").
///
/// `outputs` yields `(name, path, hash_algo, hash)` tuples so both the
/// cached full [`Derivation`] and the wire `BasicDerivation` can be
/// checked with one implementation.
///
/// Scope: a non-empty declared path must parse as a store path
/// (rejected otherwise — same fail-closed rule as the input-addressed
/// binding gate); empty declared paths (deferred fixed-output shape)
/// are skipped. For parseable declared paths any malformed algo/hash
/// or any mismatch with `StorePath::make_fixed_output(declared hash)`
/// is rejected fail-closed — otherwise a junk outputHash would exempt
/// an arbitrary declared path from validation.
///
/// Keep the accepted algo set in sync with [`fod_algo_verifiable`]
/// (the algo gate runs first, so unsupported algos already carry their
/// own error message).
///
/// Also enforces the floating-CA shape rule
/// (`gw.reject.floating-ca-declared-path+1`): an output with an algo but
/// no hash must not declare an output path — see the inline comment.
pub(crate) fn validate_declared_hash_outputs<'a>(
    drv_path: &str,
    outputs: impl Iterator<Item = (&'a str, &'a str, &'a str, &'a str)>,
) -> Result<(), String> {
    use rio_nix::hash::{HashAlgo, NixHash};

    let outputs: Vec<(&str, &str, &str, &str)> = outputs.collect();

    validate_floating_ca_shape(drv_path, outputs.iter().copied())?;

    let declared_hash: Vec<&(&str, &str, &str, &str)> = outputs
        .iter()
        .filter(|(_, _, algo, hash)| !algo.is_empty() && !hash.is_empty())
        .collect();
    if declared_hash.is_empty() {
        return Ok(());
    }

    // Shape rule (CppNix `BasicDerivation::type()`).
    if outputs.len() != 1 {
        let reason = if declared_hash.len() == outputs.len() {
            "only one fixed output is allowed"
        } else {
            "fixed-output and non-fixed outputs cannot be mixed in one derivation"
        };
        return Err(format!(
            "derivation {drv_path} declares a fixed-output hash but has {} outputs — {reason}",
            outputs.len()
        ));
    }
    let (name, declared_path, raw_algo, raw_hash) = outputs[0];
    if name != "out" {
        return Err(format!(
            "derivation {drv_path}: the single fixed output must be named \"out\", not \"{name}\""
        ));
    }

    // Deferred (empty) declared path: the fixed-output path is computed at
    // resolution time; nothing to bind yet.
    if declared_path.is_empty() {
        return Ok(());
    }
    // A non-empty declared path that does not parse as a store path is
    // rejected fail-closed — the trusted plane must not forward a
    // tenant-controlled non-store-path string to untrusted workers.
    if let Err(e) = StorePath::parse(declared_path) {
        return Err(format!(
            "derivation {drv_path} output '{name}' declares path {declared_path:?}, which is \
             not a valid store path: {e} — declared output paths must parse as store paths"
        ));
    }

    let drv_sp = StorePath::parse(drv_path)
        .map_err(|e| format!("derivation path {drv_path} is not a valid store path: {e}"))?;
    let drv_name = drv_sp
        .name()
        .strip_suffix(".drv")
        .unwrap_or_else(|| drv_sp.name());

    let (recursive, algo_str) = match raw_algo.strip_prefix("r:") {
        Some(rest) => (true, rest),
        None => (false, raw_algo),
    };
    let algo: HashAlgo = algo_str.parse().map_err(|_| {
        format!(
            "derivation {drv_path} output '{name}' declares unsupported outputHashAlgo '{raw_algo}'"
        )
    })?;
    // Length-discriminated decode (base16 / nixbase32 / base64) — CppNix
    // accepts all three encodings for outputHash, so the gate must too.
    // r[impl nix.hash.fod-decode]
    let hash = NixHash::parse_nonsri_unprefixed(algo, raw_hash).map_err(|e| {
        format!(
            "derivation {drv_path} output '{name}': outputHash is not a valid base16, \
             nixbase32, or base64 {algo} hash: {e}"
        )
    })?;
    let expected = StorePath::make_fixed_output(drv_name, &hash, recursive, &[]).map_err(|e| {
        format!("derivation {drv_path} output '{name}': cannot derive fixed-output path: {e}")
    })?;
    if expected.as_str() != declared_path {
        return Err(format!(
            "derivation {drv_path} declares fixed output '{name}' at {declared_path} but the \
             declared hash derives to {} — declared output paths must match the derivation",
            expected.as_str()
        ));
    }
    Ok(())
}

/// Floating-CA shape rule (`gw.reject.floating-ca-declared-path+1`):
/// an output that sets `outputHashAlgo` with an EMPTY `outputHash`
/// (floating content-addressed) must not declare an output path. The
/// AUTHORITATIVE enforcement is the typed parse boundary
/// (`nix.drv.output-typed` — the shape is unrepresentable past
/// `DerivationOutput::new`); this validator is residual defense in
/// depth over the legacy string view and is slated for deletion once
/// the gateway gates dispatch on the typed model.
pub(crate) fn validate_floating_ca_shape<'a>(
    drv_path: &str,
    outputs: impl Iterator<Item = (&'a str, &'a str, &'a str, &'a str)>,
) -> Result<(), String> {
    if let Some((name, path, algo, _)) = outputs
        .into_iter()
        .find(|(_, path, algo, hash)| !algo.is_empty() && hash.is_empty() && !path.is_empty())
    {
        return Err(format!(
            "derivation {drv_path} output '{name}' declares outputHashAlgo '{algo}' with no \
             outputHash (floating content-addressed) but also declares output path {path} — \
             CppNix refuses this shape and rio rejects it: floating-CA outputs must leave the \
             output path empty"
        ));
    }
    Ok(())
}

/// True iff `algo` (`"sha256"`, `"r:sha512"`, …) is an outputHashAlgo
/// the builder can verify/finalize. Mirrors rio-builder's
/// `FodHashAlgo::from_nix_str` (FOD verification) and
/// `FloatingCaSpec::from_outputs` (floating-CA finalization), which
/// accept the same set — the three must stay in sync, which is cheap
/// because Nix's supported set has been fixed for years.
pub(crate) fn fod_algo_verifiable(algo: &str) -> bool {
    matches!(
        algo.strip_prefix("r:").unwrap_or(algo),
        "sha1" | "sha256" | "sha512"
    )
}

/// Clamp for tenant-controlled string attrs (`pname`/`version`/`name`)
/// that become cache keys / PG columns. 256 chars: longest real nixpkgs
/// pname is ~90; this leaves headroom for monorepos with path-style
/// pnames while bounding the per-key cost.
const MAX_ATTR_LEN: usize = 256;

/// Cap on tenant-controlled string-list attrs
/// (`requiredSystemFeatures`). Mirror of `executor_service.rs`'s
/// `MAX_HEARTBEAT_FEATURES` (64) — the gateway is the trust boundary;
/// the scheduler's heartbeat bound is the second line of defense. Same
/// rationale: a list with thousands of entries is buggy or hostile,
/// and 64 is well past any legitimate `requiredSystemFeatures` set.
const MAX_LIST_LEN: usize = 64;

/// The shared `__structuredAttrs`-aware lookup lives in
/// [`rio_nix::derivation::StructuredEnv`] (it is also used by
/// rio-builder's native-executor glue — one parser, no JSON-vs-env
/// precedence drift). The gateway-specific ADR-023 clamping policy
/// stays HERE, as an extension trait over the shared type: the clamps
/// exist because these attrs feed gateway-owned cache keys / PG columns
/// / wire messages, which is this crate's threat model, not rio-nix's.
pub(crate) use rio_nix::derivation::StructuredEnv;

/// Gateway-side clamped accessors over [`StructuredEnv`].
pub(crate) trait ClampedAttrs {
    /// String attr with a `MAX_ATTR_LEN`-char clamp. ADR-023
    /// §Threat-model: `pname`/`version` are tenant-controlled and feed
    /// the per-tenant `SlaEstimator` cache key + `build_samples.pname`
    /// PG column; a 1 MiB pname is otherwise carried verbatim through
    /// proto → DerivationNode → ModelKey → cache key → PG.
    fn string_clamped(&self, hint: SizingHint) -> Option<String>;

    /// String-list attr with a `MAX_LIST_LEN`-element / `MAX_ATTR_LEN`-
    /// char clamp. ADR-023 §Threat-model: `requiredSystemFeatures` is
    /// tenant-controlled and feeds the `derivations.required_features`
    /// PG `text[]` column, `SpawnIntent.required_features` on the wire,
    /// and the scheduler's in-memory `DerivationState`. Same threat as
    /// [`ClampedAttrs::string_clamped`] but for a list. See also
    /// `executor_service.rs`'s `MAX_HEARTBEAT_FEATURES` (the
    /// post-translate scheduler-side bound) and `snapshot.rs`'s LRU
    /// debounce-key clamp — both are second-line defenses behind this
    /// gateway-side bound at the trust boundary.
    fn strings_clamped(&self, hint: SizingHint) -> Option<Vec<String>>;
}

impl ClampedAttrs for StructuredEnv<'_> {
    fn string_clamped(&self, hint: SizingHint) -> Option<String> {
        self.lenient_string(hint).map(|mut s| {
            if s.chars().count() > MAX_ATTR_LEN {
                s = s.chars().take(MAX_ATTR_LEN).collect();
            }
            s
        })
    }

    fn strings_clamped(&self, hint: SizingHint) -> Option<Vec<String>> {
        self.lenient_strings(hint).map(|mut v| {
            v.truncate(MAX_LIST_LEN);
            for s in &mut v {
                if s.chars().count() > MAX_ATTR_LEN {
                    *s = s.chars().take(MAX_ATTR_LEN).collect();
                }
            }
            v
        })
    }
}

/// Build the proto `DerivationNode` for any [`DerivationLike`].
///
/// Both [`Derivation`] (full BFS path) and
/// [`BasicDerivation`](rio_nix::derivation::BasicDerivation)
/// (single-node fallback) route through here — the
/// [`DerivationLike`] trait (P0384) unifies the accessor surface so
/// the struct-literal is written once. Before the trait existed the
/// two paths were hand-rolled separately and drifted on every
/// `DerivationNode` field-add (the `is_fixed_output` divergence P0384
/// fixed; the dual `is_content_addressed` annotations P0250 added).
///
/// `drv_content` is left zeroed — [`filter_and_inline_drv`] fills it
/// AFTER FindMissingPaths batching (see call-site comments on the
/// wrappers).
///
/// `is_fixed_output` is the strict [`DerivationLike::is_fixed_output`]
/// predicate (single `out` with both `hash_algo` AND `hash` set) —
/// matches the worker's strict recompute at executor/mod.rs:344.
// r[impl sched.ca.detect]
// Both CA kinds: floating (hash_algo set, hash empty) and
// fixed-output (hash also set). Cutoff applies to either — the
// output's nar_hash is what gets compared, not the input addressing.
pub fn build_node<D: DerivationLike>(drv_path: &str, drv: &D) -> types::DerivationNode {
    let (output_names, expected_output_paths): (Vec<_>, Vec<_>) = drv
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .unzip();
    let env = StructuredEnv::new(drv.env());
    types::DerivationNode {
        drv_path: drv_path.to_string(),
        // The DAG key is the declared .drv store path for EVERY node
        // shape (IA, floating-CA, FOD, hook fallback) — SubmitBuild
        // ingress rejects drv_hash != drv_path
        // (sched.merge.ingress-identity-binding), so this assignment is
        // load-bearing, not an IA-specific convention. CA content
        // identity travels separately in ca_modular_hash and keys
        // realisations, never the DAG.
        drv_hash: drv_path.to_string(),
        // pname → name fallback: stdenv's mkDerivation sets both;
        // raw derivation{} calls typically only set name. Without
        // the fallback, raw derivations get pname="" → never match
        // build_samples (keyed on pname,system) → cold-start probe
        // sizing every time. name includes version suffix so it's a
        // LESS stable key (hello-2.12 vs hello-2.13 are different
        // rows), but some history beats none. Clamped at MAX_ATTR_LEN
        // (§Threat-model: tenant-controlled, becomes a cache key).
        pname: env
            .string_clamped(SizingHint::Pname)
            .or_else(|| env.string_clamped(SizingHint::Name))
            .unwrap_or_default(),
        // ADR-023 sizing attrs. Nix bool env values are "1"/"" (older
        // stdenv) or "true"/"false" (newer). Absent stays None — for
        // enableParallelBuilding in particular, absent ≠ false (nixpkgs
        // is migrating to default-true; None means "unknown, explore").
        version: env.string_clamped(SizingHint::Version),
        enable_parallel_building: env.lenient_bool(SizingHint::EnableParallelBuilding),
        enable_parallel_checking: env.lenient_bool(SizingHint::EnableParallelChecking),
        prefer_local_build: env.lenient_bool(SizingHint::PreferLocalBuild),
        system: drv.platform().to_string(),
        required_features: env
            .strings_clamped(SizingHint::RequiredSystemFeatures)
            .unwrap_or_default(),
        output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths,
        // Empty here (= "all declared outputs wanted").
        // populate_wanted_outputs() narrows this to the union of every
        // consumer's inputDrvs output-name set ∪ the root request's
        // OutputSpec AFTER the full BFS — same staging as
        // ca_modular_hash / needs_resolve below. The BasicDerivation
        // single-node fallback never runs the pass, so it keeps the
        // conservative all-wanted sentinel.
        wanted_output_names: Vec::new(),
        // False here — populate_wanted_outputs() flags the BFS root
        // (the node the client named as a build target); every other
        // node is demanded only as a dependency. The BasicDerivation
        // fallback's caller flags its single node directly.
        explicitly_requested: false,
        drv_content: Vec::new(),
        drv_content_authoritative: false,
        is_content_addressed: drv.is_content_addressed(),
        // Empty here — populate_ca_modular_hashes() fills AFTER the
        // full BFS so hash_derivation_modulo has the complete
        // drv_cache to resolve transitive inputDrvs over. Doing it
        // inline would be a partial-closure recurse (InputNotFound
        // for inputs the BFS hasn't visited yet).
        ca_modular_hash: Vec::new(),
        // ADR-018 Appendix B: floating-CA self always resolves.
        // populate_needs_resolve() ORs in the ia.deferred case
        // (IA-with-floating-CA-input) AFTER BFS — needs the
        // drv_cache to look up children's addressing mode.
        needs_resolve: drv.has_ca_floating_outputs(),
    }
}

/// Cap on the serialized size of an inline `BasicDerivation` carried in
/// the content-bound single-node fallback ([`build_fallback_node`]).
/// 16× the per-node inline-optimization cap and well under the 4 MiB
/// tonic default message limit; a derivation bigger than this almost
/// certainly has a pathological env, and the size-unbounded path
/// (upload the `.drv`, let the worker fetch it from the store) is
/// always available.
///
/// This is an alias of the shared SubmitBuild ingress bound
/// ([`rio_common::limits::MAX_DRV_CONTENT_BYTES`]): the scheduler
/// validates the same constant, so a fallback submission the gateway
/// accepts is never size-rejected downstream.
pub(crate) const MAX_FALLBACK_INLINE_DRV_BYTES: usize = rio_common::limits::MAX_DRV_CONTENT_BYTES;

/// Build the submission node for the content-bound single-node
/// fallback: like [`build_node`], but the serialized derivation is
/// embedded in `drv_content` so the worker can execute it even though
/// the `.drv` exists in no store (the client never uploaded it and the
/// gateway deliberately does not write it — re-serialized content
/// would not text-hash to the client's claimed `.drv` path, so caching
/// or uploading it would poison later full-DAG builds).
///
/// Rejects derivations whose serialized form exceeds
/// [`MAX_FALLBACK_INLINE_DRV_BYTES`] with remediation guidance — that
/// path cannot work any other way (the worker has nowhere to fetch the
/// derivation from), so failing fast at submission is the only honest
/// answer.
// r[impl gw.hook.inline-drv-content+4]
pub fn build_fallback_node(
    drv_path: &str,
    basic: &rio_nix::derivation::BasicDerivation,
) -> Result<types::DerivationNode, String> {
    // Producer contract: the single-node fallback exists ONLY for
    // content-bound derivations (fixed-output / floating-CA), whose
    // output paths are governed by content-hash rules. The scheduler
    // unconditionally rejects authoritative inline content with
    // input-addressed outputs (nothing binds declared IA paths to
    // derivation text), so minting such a node here would manufacture a
    // guaranteed scheduler rejection — one that, before this guard, was
    // misreported to the client as a transient failure. The handler's
    // inline IA gate rejects these earlier with client remediation
    // (gw.reject.output-path-mismatch+2); this guard makes the
    // producer's contract self-enforcing.
    // r[impl gw.build.scheduler-rejection-permanent]
    if !basic.is_content_addressed() {
        return Err(format!(
            "cannot build '{drv_path}': the full derivation is not in the store and inline \
             input-addressed derivations cannot be validated — upload the .drv first with \
             `nix copy --derivation` or use --store ssh-ng:// so the worker can fetch it \
             from the store"
        ));
    }
    let aterm = basic.to_aterm();
    if aterm.len() > MAX_FALLBACK_INLINE_DRV_BYTES {
        return Err(format!(
            "cannot build '{drv_path}': the full derivation is not in the store and its inline \
             form is {} bytes (> {} byte cap for the single-node fallback) — upload the .drv \
             first with `nix copy --derivation` or use --store ssh-ng:// so the worker can \
             fetch it from the store",
            aterm.len(),
            MAX_FALLBACK_INLINE_DRV_BYTES
        ));
    }
    let mut node = build_node(drv_path, basic);
    node.drv_content = aterm.into_bytes();
    // The inline bytes are the only copy of this derivation anywhere —
    // mark them authoritative so the scheduler persists them with the
    // derivation row and a post-failover dispatch still carries them.
    node.drv_content_authoritative = true;
    // r[impl gw.hook.fallback-built-outputs]
    // A content-addressed fallback node must carry the modular hash of
    // the inline derivation (CppNix `staticOutputHashes` over the
    // received BasicDerivation = hashDerivationModulo with empty
    // inputDrvs) so the scheduler registers the realisation under the
    // exact id the client will register and look up, and merge-time
    // cache hits apply to resubmissions. The producer guard above means
    // every node reaching this point is content-addressed. Degrade like
    // populate_ca_modular_hashes: never reject on hash failure.
    let lifted = rio_nix::derivation::Derivation::from_basic(basic);
    match rio_nix::derivation::hash_derivation_modulo(
        &lifted,
        drv_path,
        &|_| None,
        &mut HashMap::new(),
    ) {
        Ok(hash) => node.ca_modular_hash = hash.to_vec(),
        Err(e) => warn!(
            drv_path = %drv_path,
            error = %e,
            "failed to hash inline fallback derivation; builtOutputs enrichment degraded"
        ),
    }
    Ok(node)
}

/// Inline .drv content into nodes whose outputs are missing from the
/// store — i.e., nodes that will actually dispatch. Saves one worker
/// → store round-trip per dispatched derivation (the `GetPath` fetch
/// in `fetch_drv_from_store`).
///
/// Gated by FindMissingPaths: cache-hit nodes stay empty (the scheduler
/// short-circuits them to Completed, they never dispatch). This is the
/// difference between "inline everything" (100 MB for a cold 10k-node
/// DAG) and "inline what's needed" (usually a handful of nodes).
///
/// Budget-capped at 16 MB total. First-come-first-serve — if we blow
/// the budget, remaining nodes fall back to worker-fetch. Not optimal
/// ordering (critical-path would be nice) but simple and correct.
///
/// On any error (FindMissingPaths timeout, store down, etc.): log and
/// skip inlining entirely. The worker-fetch path is the SAFE DEFAULT
/// — this is an optimization, not a correctness requirement.
pub async fn filter_and_inline_drv(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
    store_client: &mut StoreServiceClient<Channel>,
) {
    // Collect all NON-EMPTY expected output paths across the DAG.
    // One batched FindMissingPaths call instead of N.
    //
    // Floating-CA outputs have path="" (computed post-build from NAR
    // hash — `DerivationOutput::path()` returns empty until built).
    // The store's `validate_store_path` rejects the WHOLE BATCH on
    // any empty path, so one CA node poisons inlining for the entire
    // DAG. Filter them here; CA nodes are handled in the
    // `will_dispatch` check below (empty path → always inline).
    let all_outputs: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.expected_output_paths.iter())
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();

    // FindMissingPaths only if we have IA outputs to check. Pure-CA
    // DAGs (all floating) skip straight to the inline loop — every
    // floating-CA node dispatches (output path unknown → can't
    // cache-hit by path, so there's nothing for the store to gate).
    //
    // Timeout matches the other gateway store calls. On any error:
    // skip inlining (safe degrade — worker fetches from store).
    let missing: HashSet<String> = if all_outputs.is_empty() {
        HashSet::new()
    } else {
        // Anonymous lookup — this gates whether to inline .drv content
        // (an optimization), not whether the tenant can see outputs.
        // Tenant-scoped miss here would over-inline (worker still
        // fetches, no harm) but anonymous keeps the cache-hit
        // calculation accurate across upload contexts.
        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            // no-jwt: anonymous lookup — gates whether to inline .drv
            // content (an optimization), not tenant-scoped visibility.
            store_client.find_missing_paths(types::FindMissingPathsRequest {
                store_paths: all_outputs,
            }),
        )
        .await
        {
            Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
            Ok(Err(e)) => {
                warn!(error = %e, "FindMissingPaths failed; skipping .drv inlining (worker will fetch)");
                return;
            }
            Err(_) => {
                warn!("FindMissingPaths timed out; skipping .drv inlining (worker will fetch)");
                return;
            }
        }
    };

    // Walk nodes; inline those with ANY missing WANTED output.
    let mut total_inlined: usize = 0;
    let mut inlined_count: usize = 0;
    let mut skipped_budget: usize = 0;
    let mut budget_exhausted = false;

    for node in nodes.iter_mut() {
        // At least one WANTED output missing → this node will dispatch.
        // Empty path = floating-CA (unknown until built) → ALWAYS
        // dispatches: can't cache-hit by path, and the scheduler's
        // `maybe_resolve_ca` REQUIRES drv_content to rewrite
        // placeholder paths. The scheduler's store-fetch fallback
        // (`fetch_drv_content_from_store`) depends on its
        // startup-time store connection succeeding — a race we must
        // not rely on (layer-9 ca-cutoff failure: scheduler boots
        // before store ready → store_client=None → fallback dead).
        // All outputs present → cache hit → never dispatches → skip.
        //
        // r[impl sched.merge.wanted-outputs+2]
        // The will-dispatch prediction mirrors the scheduler's
        // demand-driven cache-hit criterion: a node whose only missing
        // outputs are ones no consumer's inputDrvs names (and the root
        // didn't select) classifies as a hit / pending-substitute and
        // never dispatches, so inlining its drv_content is wasted
        // budget. The wanted subset is resolved by the SAME
        // `verifiable_wanted_paths` the scheduler classifies with. Its
        // `None` (no concrete wanted path: all floating-CA, or no
        // declared name matches) falls back to the all-declared
        // criterion — the conservative direction is "inline it":
        // over-inlining costs bytes, under-inlining costs a
        // worker→store round-trip on a node that DOES dispatch.
        let will_dispatch = match rio_common::wanted_outputs::verifiable_wanted_paths(
            &node.output_names,
            &node.expected_output_paths,
            &node.wanted_output_names,
        ) {
            None => node
                .expected_output_paths
                .iter()
                .any(|p| p.is_empty() || missing.contains(p)),
            Some(wanted) => wanted.iter().any(|p| missing.contains(*p)),
        };
        if !will_dispatch {
            continue;
        }

        // Budget fast-path. Once the per-node gate below has rejected
        // ANY node, we stop serializing — otherwise every remaining
        // will-dispatch node pays parse + cache lookup + full
        // `to_aterm()` before rejection. On a 150k-node cold DAG that's
        // ~148k throwaway serializations with no `.await` in the loop →
        // multi-second tokio worker stall. The flag arms on first
        // rejection; this sacrifices bin-packing tiny nodes into the
        // last few KB of headroom (irrelevant: real .drvs >100 bytes,
        // MAX_INLINE_DRV_BYTES filters huge ones). `total_inlined`
        // stays accurate for the debug! metric below — it's NOT
        // saturated to the cap.
        if budget_exhausted {
            skipped_budget += 1;
            continue;
        }

        // Look up the Derivation. drv_path is the key we used in
        // reconstruct_dag. If it's not in cache (shouldn't happen —
        // reconstruct_dag populates it) or won't parse, skip.
        let Ok(sp) = StorePath::parse(&node.drv_path) else {
            continue;
        };
        let Some(drv) = drv_cache.get(&sp) else {
            continue;
        };

        // Serialize. to_aterm() is deterministic (BTreeMap iteration)
        // so this is the same bytes the store has.
        let aterm = drv.to_aterm();
        let aterm_bytes = aterm.into_bytes();

        // Per-node size gate. Huge derivations (flake inputs dumped
        // into env) aren't worth it — worker fetches those.
        if aterm_bytes.len() > MAX_INLINE_DRV_BYTES {
            continue;
        }

        // Budget gate. Once we hit 16 MB, arm the fast-path flag so
        // subsequent iterations skip BEFORE `to_aterm()`. Remaining
        // nodes fall back to worker-fetch. We still loop to count
        // skipped_budget for the metric, but no more inlining happens.
        if total_inlined + aterm_bytes.len() > INLINE_BUDGET_BYTES {
            skipped_budget += 1;
            budget_exhausted = true;
            continue;
        }

        total_inlined += aterm_bytes.len();
        inlined_count += 1;
        node.drv_content = aterm_bytes;
    }

    debug!(
        inlined = inlined_count,
        bytes = total_inlined,
        skipped_over_budget = skipped_budget,
        "inlined .drv content for will-dispatch nodes"
    );
}

/// Build a `SubmitBuildRequest` from nodes and edges.
///
/// `tenant_name` is `Option<&NormalizedName>` — the proto boundary
/// convention is empty-string-as-absent, so `None` (single-tenant
/// mode) serializes to `""`, `Some(n)` serializes to the normalized
/// inner. The type guarantees no leading/trailing/interior whitespace
/// ever reaches the scheduler.
///
/// Build-option fields (`max_silent_time`/`build_timeout`/`build_cores`/
/// `keep_going`) are hardcoded to defaults: ssh-ng clients never send
/// `wopSetOptions` (see [`handle_set_options`](crate::handler)), so the only
/// way to set these is via the gRPC path (rio-cli), which constructs
/// `SubmitBuildRequest` directly without going through this helper.
pub fn build_submit_request(
    nodes: Vec<types::DerivationNode>,
    edges: Vec<types::DerivationEdge>,
    priority_class: &str,
    tenant_name: Option<&NormalizedName>,
) -> types::SubmitBuildRequest {
    types::SubmitBuildRequest {
        // Proto convention: empty string = absent/single-tenant.
        tenant_name: tenant_name.map(|n| n.to_string()).unwrap_or_default(),
        priority_class: priority_class.to_string(),
        nodes,
        edges,
        max_silent_time: 0,
        build_timeout: 0,
        build_cores: 0,
        keep_going: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::derivation::BasicDerivation;
    use std::collections::{BTreeMap, BTreeSet};

    use rio_nix::derivation::DerivationOutput;
    use rstest::rstest;

    fn make_basic_drv(env: BTreeMap<String, String>) -> anyhow::Result<BasicDerivation> {
        let output = DerivationOutput::new(
            "out",
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-out",
            "",
            "",
        )?;
        Ok(BasicDerivation::new(
            vec![output],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            env,
        )?)
    }

    /// Same as make_basic_drv but with a configurable single output.
    fn make_basic_drv_with_output(hash_algo: &str, hash: &str) -> anyhow::Result<BasicDerivation> {
        // Floating-CA outputs (algo set, hash empty) must not declare a
        // path — the typed constructor enforces the oracle's shape rule.
        let path = if !hash_algo.is_empty() && hash.is_empty() {
            ""
        } else {
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-out"
        };
        let output = DerivationOutput::new("out", path, hash_algo, hash)?;
        Ok(BasicDerivation::new(
            vec![output],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            BTreeMap::new(),
        )?)
    }

    // r[verify sched.ca.detect]
    /// `is_content_addressed` + `is_fixed_output` detection across both
    /// `DerivationLike` impls. Three drv shapes × two source paths
    /// (`BasicDerivation` fallback, full `Derivation`). Both route through
    /// `build_node<D>` so the matrix proves trait dispatch is correct, not
    /// that the builder logic differs (it doesn't).
    ///
    /// Regression guard for the pre-P0388 divergence shape: same proto
    /// field, two hand-rolled struct literals, drift on every
    /// `DerivationNode` field-add. Also covers the pre-fix loose
    /// per-output predicate that made `is_fixed_output=true` for
    /// floating-CA on the fallback path, diverging from the full-DAG
    /// path; worker's strict recompute at executor/mod.rs:344 saw false →
    /// warn! at :346 fired spuriously.
    #[rstest]
    #[case::ia("", "", "", false, false)]
    #[case::floating_ca("sha256", "r:sha256", "", true, false)]
    #[case::fod("sha256", "sha256", "deadbeef", true, true)]
    fn build_node_ca_fod_detection(
        #[case] basic_algo: &str,
        #[case] aterm_algo: &str,
        #[case] hash: &str,
        #[case] want_ca: bool,
        #[case] want_fod: bool,
    ) -> anyhow::Result<()> {
        // BasicDerivation path (single-node fallback).
        let basic = make_basic_drv_with_output(basic_algo, hash)?;
        let node = build_node("/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi.drv", &basic);
        assert_eq!(node.is_content_addressed, want_ca, "basic: is_ca");
        assert_eq!(node.is_fixed_output, want_fod, "basic: strict is_fod");

        // Full Derivation path (via ATerm parse). Floating shapes carry
        // an empty declared path (typed-boundary parity with the helper).
        let out_path = if !aterm_algo.is_empty() && hash.is_empty() {
            ""
        } else {
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out"
        };
        let aterm = format!(
            r#"Derive([("out","{out_path}","{aterm_algo}","{hash}")],[],[],"x86_64-linux","/bin/sh",[],[])"#
        );
        let node = build_node(&test_drv_path("ca-test"), &Derivation::parse(&aterm)?);
        assert_eq!(node.is_content_addressed, want_ca, "full: is_ca");
        assert_eq!(node.is_fixed_output, want_fod, "full: strict is_fod");
        Ok(())
    }

    #[test]
    fn test_build_submit_request_carries_tenant_name() {
        let name = NormalizedName::new("team-foo").unwrap();
        let req = build_submit_request(vec![], vec![], "ci", Some(&name));
        assert_eq!(req.tenant_name, "team-foo");
        assert_eq!(req.priority_class, "ci");

        // None → empty string on the wire (proto's empty-as-absent
        // convention for single-tenant mode).
        let req_empty = build_submit_request(vec![], vec![], "ci", None);
        assert_eq!(
            req_empty.tenant_name, "",
            "None tenant_name → empty string (single-tenant mode)"
        );
    }

    #[test]
    fn test_single_node_extracts_features() -> anyhow::Result<()> {
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), "kvm big-parallel".into());
        let drv = make_basic_drv(env)?;

        let node = build_node("/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi.drv", &drv);
        assert_eq!(
            node.required_features,
            vec!["kvm".to_string(), "big-parallel".to_string()],
            "requiredSystemFeatures should be extracted from BasicDerivation env"
        );
        Ok(())
    }

    #[test]
    fn validate_dag_rejects_oversized() {
        // Build MAX_DAG_NODES+1 nodes to trigger.
        // No drv_cache needed — the size check fires first.
        let oversized: Vec<types::DerivationNode> = (0..=rio_common::limits::MAX_DAG_NODES)
            .map(|i| types::DerivationNode {
                drv_path: format!("/nix/store/node{i}.drv"),
                drv_hash: format!("node{i}"),
                ..Default::default()
            })
            .collect();
        let empty_cache = HashMap::new();
        let result = validate_dag(&oversized, &empty_cache);
        assert!(
            result.is_err(),
            "{} nodes > {} max should reject",
            oversized.len(),
            rio_common::limits::MAX_DAG_NODES
        );
        assert!(result.unwrap_err().contains("DAG too large"));
    }

    #[test]
    fn validate_dag_accepts_normal_size_no_nochroot() {
        // A few nodes, empty cache (BasicDerivation fallback path),
        // no __noChroot → Ok.
        let nodes = vec![
            types::DerivationNode {
                drv_path: "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-test.drv".into(),
                drv_hash: "aaa".into(),
                ..Default::default()
            },
            types::DerivationNode {
                drv_path: "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-test.drv".into(),
                drv_hash: "bbb".into(),
                ..Default::default()
            },
        ];
        let empty_cache = HashMap::new();
        assert!(validate_dag(&nodes, &empty_cache).is_ok());
    }

    // The __noChroot=1 happy-path rejection is wire-tested at
    // tests/wire_opcodes/build.rs (seed NOCHROOT_DRV_ATERM into the
    // mock store so resolve_derivation populates drv_cache, then drive
    // opcodes 36 + 46 and assert the failure BuildResult carries the
    // "sandbox escape" message). The typed matrix below covers the
    // fail-closed arms via hand-built ATerms.

    /// Helper: a cached single-node DAG whose drv carries `extra_env`.
    fn nochroot_fixture(
        extra_env: &str,
    ) -> (Vec<types::DerivationNode>, HashMap<StorePath, Derivation>) {
        let drv_path = "/nix/store/nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn-nochroot-probe.drv";
        let aterm = format!(
            r#"Derive([("out","/nix/store/gsqizyqxzjbdjyb1jav5zjndvsadgs15-out","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/gsqizyqxzjbdjyb1jav5zjndvsadgs15-out"){extra_env}])"#
        );
        let drv = Derivation::parse(&aterm).expect("test ATerm parses");
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: drv_path.into(),
            ..Default::default()
        };
        let mut cache = HashMap::new();
        cache.insert(sp(drv_path), drv);
        (vec![node], cache)
    }

    /// Wrong-typed `__noChroot` is rejected fail-closed (oracle
    /// `getBoolean` throws — `1` and `"true"` are NOT booleans);
    /// `false`/absent stay accepted; `true` keeps the sandbox-escape
    /// rejection.
    // r[verify gw.reject.nochroot+2]
    #[test]
    fn validate_dag_rejects_wrong_typed_nochroot() {
        // JSON number 1: the classic "truthy but not a boolean".
        let (nodes, cache) = nochroot_fixture(r#",("__json","{\"__noChroot\":1}")"#);
        let err = validate_dag(&nodes, &cache).unwrap_err();
        assert!(err.contains("__noChroot is unreadable"), "{err}");

        // JSON string "true": also not a boolean.
        let (nodes, cache) = nochroot_fixture(r#",("__json","{\"__noChroot\":\"true\"}")"#);
        let err = validate_dag(&nodes, &cache).unwrap_err();
        assert!(err.contains("__noChroot is unreadable"), "{err}");

        // JSON false: a real boolean, sandbox stays on → accepted.
        let (nodes, cache) = nochroot_fixture(r#",("__json","{\"__noChroot\":false}")"#);
        assert!(validate_dag(&nodes, &cache).is_ok());

        // JSON true: the original sandbox-escape rejection.
        let (nodes, cache) = nochroot_fixture(r#",("__json","{\"__noChroot\":true}")"#);
        let err = validate_dag(&nodes, &cache).unwrap_err();
        assert!(err.contains("sandbox escape"), "{err}");

        // Flat env "1" (the non-structured spelling): still rejected.
        let (nodes, cache) = nochroot_fixture(r#",("__noChroot","1")"#);
        let err = validate_dag(&nodes, &cache).unwrap_err();
        assert!(err.contains("sandbox escape"), "{err}");
    }

    /// An unparseable `__json` blob on a cached drv is rejected — the
    /// gateway cannot read the derivation's sandbox intent, so it does
    /// not guess (pre-fix: the lenient reader degraded the whole blob
    /// to "no structured attrs" and waved the derivation through).
    // r[verify gw.reject.nochroot+2]
    #[test]
    fn validate_dag_rejects_unparseable_json_blob() {
        let (nodes, cache) = nochroot_fixture(r#",("__json","{not json")"#);
        let err = validate_dag(&nodes, &cache).unwrap_err();
        assert!(err.contains("__noChroot is unreadable"), "{err}");
    }

    /// A consistent input-addressed derivation (declared paths == the
    /// paths it derives to) passes; declaring somebody else's
    /// well-formed path is rejected; malformed declared paths are out
    /// of scope for this gate (they cannot alias a real store object).
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn validate_dag_binds_ia_declared_paths_to_the_derivation() {
        let drv_path = "/nix/store/cccccccccccccccccccccccccccccccc-mine.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "ccc".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();

        let aterm_with = |out_path: &str| -> Derivation {
            let aterm = format!(
                r#"Derive([("out","{out_path}","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","{out_path}")])"#
            );
            Derivation::parse(&aterm).expect("test ATerm parses")
        };

        // Compute the honest path for THIS derivation (declared paths are
        // masked during the computation, so any placeholder works here).
        let probe = aterm_with("/nix/store/dddddddddddddddddddddddddddddddd-mine");
        let mut hash_cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let honest = rio_nix::derivation::input_addressed_output_paths(
            &probe,
            drv_path,
            &resolve,
            &mut hash_cache,
        )
        .expect("derive")["out"]
            .as_str()
            .to_owned();

        // Consistent declaration → accepted.
        let mut cache = HashMap::new();
        cache.insert(key.clone(), aterm_with(&honest));
        assert!(
            validate_output_path_bindings(std::slice::from_ref(&node), &cache).is_ok(),
            "consistent IA declaration must pass"
        );

        // Declaring somebody else's (well-formed) path → rejected.
        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";
        let mut cache = HashMap::new();
        cache.insert(key.clone(), aterm_with(victim));
        let err = validate_output_path_bindings(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("must match the derivation"),
            "squatted path must be rejected: {err}"
        );
        assert!(err.contains(victim), "error names the declared path: {err}");

        // Malformed declared path (not a store path): formerly this
        // gate's fail-closed arm, now unrepresentable — the typed parse
        // boundary rejects it before the drv cache is ever populated,
        // so a tenant-controlled non-store-path string has no route to
        // untrusted workers.
        let aterm = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","mine"),("out","/nix/store/zzz-output")])"#;
        assert!(
            Derivation::parse(aterm).is_err(),
            "malformed declared path must fail at the parse boundary"
        );

        // Deferred IA (EMPTY declared path) stays out of scope — nothing
        // to validate until resolution computes the path.
        let mut cache = HashMap::new();
        cache.insert(key.clone(), aterm_with(""));
        assert!(
            validate_output_path_bindings(std::slice::from_ref(&node), &cache).is_ok(),
            "deferred (empty) declared paths have nothing to validate"
        );
    }

    /// The declared-hash gate likewise rejects a non-empty declared path
    /// that does not parse as a store path (and keeps skipping empty ones).
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn declared_hash_gate_rejects_malformed_declared_path() {
        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fetch.drv";
        let hex_hash = "5a".repeat(32);

        // Non-empty, unparseable declared path → rejected.
        let outputs = vec![("out", "/nix/store/zzz-evil", "sha256", hex_hash.as_str())];
        let err = validate_declared_hash_outputs(drv_path, outputs.into_iter()).unwrap_err();
        assert!(
            err.contains("not a valid store path"),
            "malformed declared path must be rejected: {err}"
        );

        // Empty declared path (deferred fixed-output) → still skipped.
        let outputs = vec![("out", "", "sha256", hex_hash.as_str())];
        assert!(
            validate_declared_hash_outputs(drv_path, outputs.into_iter()).is_ok(),
            "empty declared path keeps the deferred carve-out"
        );
    }

    /// A crafted derivation pairing a floating-CA output with a squatted
    /// well-formed static path must not dodge the gate via the
    /// content-bound fast path; genuinely all-content-bound derivations
    /// (all-floating-CA, single-output FOD) keep their fast path.
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn validate_dag_rejects_squatted_path_next_to_floating_ca() {
        let drv_path = "/nix/store/cccccccccccccccccccccccccccccccc-camix.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "ccc".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();
        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";

        // Mixed floating-CA + squatted static path → rejected fail-closed.
        let mixed = Derivation::parse(&format!(
            r#"Derive([("ca","","r:sha256",""),("evil","{victim}","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("ca",""),("evil","{victim}")])"#
        ))
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), mixed);
        let err = validate_output_path_bindings(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("cannot derive output paths"),
            "mixed CA + static shape must be rejected fail-closed: {err}"
        );

        // All-floating-CA → still accepted (content-bound fast path).
        let all_ca = Derivation::parse(
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","")])"#,
        )
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), all_ca);
        assert!(
            validate_output_path_bindings(std::slice::from_ref(&node), &cache).is_ok(),
            "all-floating-CA derivations keep the fast path"
        );

        // Single-output FOD → unchanged, accepted.
        let fod = Derivation::parse(
            r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src","sha256","abababababababababababababababababababababababababababababababab")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src")])"#,
        )
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key, fod);
        assert!(
            validate_output_path_bindings(std::slice::from_ref(&node), &cache).is_ok(),
            "fixed-output derivations are unchanged"
        );
    }

    /// A crafted derivation pairing a deferred (empty-path) output with a
    /// squatted well-formed one must not dodge the path gate via any
    /// drv-level skip.
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn validate_dag_rejects_squatted_path_next_to_deferred_output() {
        let drv_path = "/nix/store/cccccccccccccccccccccccccccccccc-mixed.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "ccc".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();

        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";
        let aterm = format!(
            r#"Derive([("evil","{victim}","",""),("out","","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("evil","{victim}"),("out","")])"#
        );
        let drv = Derivation::parse(&aterm).expect("test ATerm parses");

        let mut cache = HashMap::new();
        cache.insert(key, drv);
        let err = validate_output_path_bindings(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("must match the derivation") || err.contains("derive"),
            "mixed deferred+squatted shape must be rejected: {err}"
        );
    }

    /// Build a linear input-addressed chain of `n` cached derivations
    /// whose declared output paths are the HONEST derived ones
    /// (constructed bottom-up, since each parent's derivation hash
    /// depends on its child's final form). Returns root-first nodes and
    /// the populated cache.
    /// `squat`: optionally make node `at` declare the output path of the
    /// (deeper, already-built) node `steal_from` instead of its honest
    /// one — every OTHER node stays consistent relative to the tampered
    /// node, so the tampered node is the only mismatch.
    fn ia_chain_with(
        n: usize,
        squat: Option<(usize, usize)>,
    ) -> (Vec<types::DerivationNode>, HashMap<StorePath, Derivation>) {
        let drv_path = |i: usize| format!("/nix/store/{i:0>32}-chain-{i}.drv");
        let mut cache: HashMap<StorePath, Derivation> = HashMap::new();
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        for i in (0..n).rev() {
            let inputs = if i + 1 < n {
                format!(r#"[("{}",["out"])]"#, drv_path(i + 1))
            } else {
                "[]".to_owned()
            };
            // Probe with a placeholder declared path (declared paths are
            // masked out of the computation), derive the honest path,
            // then store the final form.
            let probe = Derivation::parse(&format!(
                r#"Derive([("out","","","")],{inputs},[],"x86_64-linux","/bin/sh",["-c","echo"],[("out","")])"#
            ))
            .expect("probe ATerm parses");
            let honest = {
                let resolve = |p: &str| StorePath::parse(p).ok().and_then(|sp| cache.get(&sp));
                rio_nix::derivation::input_addressed_output_paths(
                    &probe,
                    &drv_path(i),
                    &resolve,
                    &mut hash_cache,
                )
                .expect("derive honest path")["out"]
                    .as_str()
                    .to_owned()
            };
            let declared = match squat {
                Some((at, steal_from)) if at == i => {
                    let victim_key = StorePath::parse(&drv_path(steal_from)).unwrap();
                    cache[&victim_key].outputs()[0].path().to_owned()
                }
                _ => honest,
            };
            let final_drv = Derivation::parse(&format!(
                r#"Derive([("out","{declared}","","")],{inputs},[],"x86_64-linux","/bin/sh",["-c","echo"],[("out","{declared}")])"#
            ))
            .expect("final ATerm parses");
            cache.insert(StorePath::parse(&drv_path(i)).unwrap(), final_drv);
        }
        let nodes = (0..n)
            .map(|i| types::DerivationNode {
                drv_path: drv_path(i),
                drv_hash: format!("{i}"),
                ..Default::default()
            })
            .collect();
        (nodes, cache)
    }

    fn honest_ia_chain(n: usize) -> (Vec<types::DerivationNode>, HashMap<StorePath, Derivation>) {
        ia_chain_with(n, None)
    }

    /// A 600-deep honest chain passes the binding gate with a cold
    /// per-submission hash cache — depth is never a rejection cause.
    /// (Regression test for the former 512-level recursion cap, which
    /// turned deep-but-legitimate DAGs into whole-submission
    /// rejections.)
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn validate_output_path_bindings_accepts_deep_ia_chain() {
        let (nodes, cache) = honest_ia_chain(600);
        assert!(
            validate_output_path_bindings(&nodes, &cache).is_ok(),
            "a deep honest chain must not be rejected"
        );
    }

    /// The same 600-deep chain with one node's declared path swapped to
    /// another derivation's path is still rejected — removing the depth
    /// cap must not weaken the gate at depth.
    #[test]
    fn validate_output_path_bindings_still_rejects_squat_in_deep_chain() {
        // Node 300 declares node 400's output path; every other node is
        // consistent relative to the tampered node (the realistic
        // attacker shape), so node 300 is the single mismatch.
        let (nodes, cache) = ia_chain_with(600, Some((300, 400)));
        let err = validate_output_path_bindings(&nodes, &cache).unwrap_err();
        assert!(
            err.contains(&nodes[300].drv_path) && err.contains("must match the derivation"),
            "squat at depth must be rejected naming the drv: {err}"
        );
    }

    /// Structural pin for the pipeline split: `validate_dag` performs
    /// only the cheap checks and no longer runs the output-path binding
    /// pass — a squatted IA path passes `validate_dag` but is caught by
    /// `validate_output_path_bindings`. This is NOT a bypass: the
    /// handler pipeline always runs the binding gate before
    /// `SubmitBuild` (behind rate-limit/quota, on the blocking pool) —
    /// see the wopBuildDerivation / wopBuildPathsWithResults squatting
    /// wire tests in `tests/wire_opcodes/build.rs`, which assert the
    /// rejection end-to-end.
    #[test]
    fn validate_dag_no_longer_runs_the_binding_pass() {
        let drv_path = "/nix/store/cccccccccccccccccccccccccccccccc-split.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "ccc".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();
        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";
        let aterm = format!(
            r#"Derive([("out","{victim}","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","{victim}")])"#
        );
        let mut cache = HashMap::new();
        cache.insert(key, Derivation::parse(&aterm).expect("test ATerm parses"));

        assert!(
            validate_dag(std::slice::from_ref(&node), &cache).is_ok(),
            "validate_dag holds only the cheap checks"
        );
        assert!(
            validate_output_path_bindings(std::slice::from_ref(&node), &cache).is_err(),
            "the binding gate still rejects the squat"
        );
    }

    /// Any output declaring a hash algorithm the builder cannot handle
    /// is rejected at submission: a FOD with an md5 hash and a
    /// A floating-CA-shaped output (algo set, hash EMPTY) that also
    /// declares a non-empty output path is rejected: CppNix refuses to
    /// parse that shape, and accepting it would exempt the declared
    /// path from every output-path binding. Proper floating-CA (empty
    /// path) stays accepted; the rule applies to any declared path
    /// string, parseable or not, and to mixed multi-output shapes.
    // r[verify gw.reject.floating-ca-declared-path+1]
    #[test]
    fn validate_dag_rejects_floating_ca_with_declared_path() {
        // The shape is now UNREPRESENTABLE: the typed parse boundary
        // rejects a floating-CA output declaring a path with the
        // oracle's wording (derivations.cc:339-340), so validate_dag
        // can never see one. These pins witness the boundary at the
        // gateway's own input channel (the session drv cache is
        // populated via Derivation::parse).
        let parse_with_outputs = |outs: &[(&str, &str, &str, &str)]| {
            let rendered: Vec<String> = outs
                .iter()
                .map(|(n, p, a, h)| format!(r#"("{n}","{p}","{a}","{h}")"#))
                .collect();
            let env: Vec<String> = outs
                .iter()
                .map(|(n, p, _, _)| format!(r#"("{n}","{p}")"#))
                .collect();
            Derivation::parse(&format!(
                r#"Derive([{}],[],[],"x86_64-linux","/bin/sh",[],[{}])"#,
                rendered.join(","),
                env.join(",")
            ))
        };
        let victim = "/nix/store/cccccccccccccccccccccccccccccccc-victim";

        // (a) single floating-CA output declaring a path → unparseable,
        // with the oracle's message.
        let err = parse_with_outputs(&[("out", victim, "r:sha256", "")]).unwrap_err();
        assert!(
            err.to_string()
                .contains("content-addressing derivation output should not specify output path"),
            "oracle wording: {err}"
        );

        // (b) proper floating-CA (empty path) parses and passes the gate.
        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "bbb".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();
        let mut cache = HashMap::new();
        cache.insert(
            key.clone(),
            parse_with_outputs(&[("out", "", "r:sha256", "")]).expect("legal floating shape"),
        );
        assert!(validate_dag(std::slice::from_ref(&node), &cache).is_ok());

        // (c) mixed multi-output: the offending output rejects the whole
        // parse even with an innocent sibling.
        assert!(
            parse_with_outputs(&[("out", victim, "r:sha256", ""), ("doc", "", "", "")]).is_err()
        );

        // (d) the declared "path" need not parse as a store path — the
        // SHAPE is the violation, checked before path syntax.
        let err = parse_with_outputs(&[("out", "not-a-store-path", "sha256", "")]).unwrap_err();
        assert!(
            err.to_string().contains("should not specify output path"),
            "{err}"
        );

        // (e) the squat-binding protection survives over representable
        // shapes: an IA output declaring the victim's path is rejected by
        // the binding gate (declared != derived), not silently accepted.
        let mut cache = HashMap::new();
        cache.insert(
            key,
            parse_with_outputs(&[("out", victim, "", "")]).expect("legal IA shape"),
        );
        let err = validate_output_path_bindings(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("must match the derivation"),
            "IA squat must still be rejected by the binding gate: {err}"
        );
    }

    /// Unverifiable outputHashAlgo values are CLASSIFIED (returned as
    /// offenders for the realization probe), not rejected outright:
    /// fixed-output md5 and floating-CA md5/blake3 alike. Verifiable
    /// algorithms (sha256, r:sha512) pass in both shapes with no
    /// offenders, and input-addressed outputs (no hash, no algo) are
    /// never classified.
    // r[verify gw.reject.unsupported-hash-algo+4]
    #[test]
    fn validate_dag_rejects_unverifiable_fod_algo() {
        let fod_drv_at = |algo: &str, hash: &str, path: &str| -> Derivation {
            let aterm = format!(
                r#"Derive([("out","{path}","{algo}","{hash}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{path}")])"#
            );
            Derivation::parse(&aterm).expect("test ATerm parses")
        };
        let fod_drv = |algo: &str, hash: &str| {
            fod_drv_at(
                algo,
                hash,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src",
            )
        };
        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "bbb".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();

        // md5 FOD → classified as an offender (NOT an immediate Err),
        // carrying the algo, the offending output and every declared
        // path; the declared-hash binding is skipped for it (a junk
        // declared path does not produce the "must match" error).
        let mut cache = HashMap::new();
        cache.insert(key.clone(), fod_drv("md5", &"de".repeat(16)));
        let offenders = validate_dag(std::slice::from_ref(&node), &cache).unwrap();
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].drv_path, drv_path);
        assert_eq!(offenders[0].output_name, "out");
        assert_eq!(offenders[0].algo, "md5");
        assert_eq!(
            offenders[0].declared_paths,
            vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".to_string()]
        );

        // sha256 and r:sha512 → accepted with NO offenders, with
        // hash-consistent declared paths and correctly-sized digests
        // (the declared-hash binding gate requires both).
        for (algo, digest_len) in [("sha256", 32usize), ("r:sha512", 64)] {
            let digest = vec![0xabu8; digest_len];
            let nix_hash = rio_nix::hash::NixHash::new(
                algo.strip_prefix("r:").unwrap_or(algo).parse().unwrap(),
                digest.clone(),
            )
            .unwrap();
            let honest =
                StorePath::make_fixed_output("src", &nix_hash, algo.starts_with("r:"), &[])
                    .unwrap();
            let mut cache = HashMap::new();
            cache.insert(
                key.clone(),
                fod_drv_at(algo, &hex::encode(&digest), honest.as_str()),
            );
            assert!(
                validate_dag(std::slice::from_ref(&node), &cache)
                    .unwrap()
                    .is_empty(),
                "{algo} must be accepted with no offenders"
            );
        }

        // Floating-CA (algo set, hash EMPTY, path EMPTY — the only shape
        // CppNix can produce) with a supported algo → accepted, no
        // offenders.
        let mut cache = HashMap::new();
        cache.insert(key.clone(), fod_drv_at("r:sha256", "", ""));
        assert!(
            validate_dag(std::slice::from_ref(&node), &cache)
                .unwrap()
                .is_empty()
        );

        // Floating-CA with an algo the builder's CA finalization cannot
        // produce (md5, blake3) → classified; its declared path is empty,
        // which the realization probe rejects (floating-CA is never
        // exempt — see reject_unrealized_fod_offenders tests).
        for algo in ["md5", "blake3"] {
            let mut cache = HashMap::new();
            cache.insert(key.clone(), fod_drv_at(algo, "", ""));
            let offenders = validate_dag(std::slice::from_ref(&node), &cache).unwrap();
            assert_eq!(offenders.len(), 1, "{algo}: classified, not rejected");
            assert_eq!(offenders[0].algo, algo);
            assert_eq!(offenders[0].declared_paths, vec![String::new()]);
        }

        // An offender that ALSO violates the floating-CA shape rule
        // (algo set, hash empty, path declared) never reaches the gate:
        // the typed parse boundary rejects the shape outright, so the
        // realization exemption cannot apply to it by construction.
        let aterm = r#"Derive([("out","/nix/store/cccccccccccccccccccccccccccccccc-squat","md5","")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/cccccccccccccccccccccccccccccccc-squat")])"#;
        let err = Derivation::parse(aterm).unwrap_err();
        assert!(
            err.to_string().contains("should not specify output path"),
            "{err}"
        );

        // Input-addressed output (no hash, no algo) → never checked by
        // the ALGO gate. The IA path gate does apply, so give the
        // fixture its honest derived path (any placeholder works for
        // the computation — declared paths are masked out of it).
        let ia_with = |out_path: &str| -> Derivation {
            let aterm = format!(
                r#"Derive([("out","{out_path}","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","{out_path}")])"#
            );
            Derivation::parse(&aterm).expect("test ATerm parses")
        };
        let mut hash_cache = HashMap::new();
        let resolve_none = |_: &str| -> Option<&Derivation> { None };
        let honest = rio_nix::derivation::input_addressed_output_paths(
            &ia_with("/nix/store/dddddddddddddddddddddddddddddddd-src"),
            drv_path,
            &resolve_none,
            &mut hash_cache,
        )
        .expect("derive")["out"]
            .as_str()
            .to_owned();
        let mut cache = HashMap::new();
        cache.insert(key.clone(), ia_with(&honest));
        assert!(
            validate_dag(std::slice::from_ref(&node), &cache)
                .unwrap()
                .is_empty()
        );
    }

    /// The content-bound single-node fallback carries the serialized
    /// derivation; oversized derivations are rejected with remediation.
    // r[verify gw.hook.inline-drv-content+4]
    #[test]
    fn build_fallback_node_inlines_the_basic_derivation() {
        use rio_nix::derivation::BasicDerivation;

        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fetch.drv";
        // A FOD-shaped BasicDerivation with one input source.
        let basic = BasicDerivation::new(
            vec![
                rio_nix::derivation::DerivationOutput::new(
                    "out",
                    "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fetch",
                    "r:sha256",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                )
                .unwrap(),
            ],
            ["/nix/store/cccccccccccccccccccccccccccccccc-src".to_string()]
                .into_iter()
                .collect(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec!["-c".into(), "echo hi".into()],
            [(
                "out".to_string(),
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fetch".to_string(),
            )]
            .into_iter()
            .collect(),
        )
        .expect("test BasicDerivation constructs");

        let node = build_fallback_node(drv_path, &basic).expect("under the cap");
        assert_eq!(node.drv_path, drv_path);
        assert!(node.is_fixed_output, "FOD detection preserved");
        assert!(
            node.drv_content_authoritative,
            "fallback nodes carry the only copy of the derivation — must be marked authoritative"
        );
        assert!(
            !build_node(drv_path, &basic).drv_content_authoritative,
            "ordinary nodes are not authoritative (worker fetches the .drv from the store)"
        );
        // r[verify gw.hook.fallback-built-outputs]
        // Content-addressed fallback nodes carry the modular hash of the
        // inline derivation (staticOutputHashes parity: the lifted
        // inputDrvs-less derivation), so the scheduler registers the
        // realisation under the id the client will look up.
        let lifted = rio_nix::derivation::Derivation::from_basic(&basic);
        let expected_hash = rio_nix::derivation::hash_derivation_modulo(
            &lifted,
            drv_path,
            &|_| None,
            &mut HashMap::new(),
        )
        .expect("no-input derivation always hashes");
        assert_eq!(
            node.ca_modular_hash,
            expected_hash.to_vec(),
            "FOD fallback node carries the modular hash of the inline derivation"
        );

        // A non-content-bound (input-addressed / deferred-IA) inline
        // derivation is REFUSED by the producer: the scheduler
        // unconditionally rejects authoritative inline IA content, so
        // minting the node would only manufacture a guaranteed rejection
        // misreported as transient. The handler's inline IA gate rejects
        // these earlier; this is the producer's own contract.
        // r[verify gw.build.scheduler-rejection-permanent]
        let ia_basic = BasicDerivation::new(
            vec![rio_nix::derivation::DerivationOutput::new("out", "", "", "").unwrap()],
            Default::default(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec!["-c".into(), "echo hi".into()],
            [("out".to_string(), String::new())].into_iter().collect(),
        )
        .expect("IA-shaped BasicDerivation constructs");
        let ia_err = build_fallback_node(
            "/nix/store/cccccccccccccccccccccccccccccccc-plain.drv",
            &ia_basic,
        )
        .expect_err("non-content-bound fallback nodes are refused");
        assert!(
            ia_err.contains("input-addressed") && ia_err.contains("upload the .drv"),
            "refusal names the cause and the remediation: {ia_err}"
        );
        assert_eq!(
            node.drv_content,
            basic.to_aterm().into_bytes(),
            "drv_content is exactly the serialized BasicDerivation"
        );
        // The inlined bytes are a parseable derivation (what the worker
        // will do with them).
        let reparsed = Derivation::parse(std::str::from_utf8(&node.drv_content).unwrap()).unwrap();
        assert_eq!(reparsed.platform(), "x86_64-linux");
        assert_eq!(reparsed.input_srcs().len(), 1, "input sources preserved");

        // Over the cap → Err with actionable remediation.
        let huge_env = "x".repeat(MAX_FALLBACK_INLINE_DRV_BYTES + 1);
        let huge = BasicDerivation::new(
            vec![
                rio_nix::derivation::DerivationOutput::new(
                    "out",
                    "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fetch",
                    "r:sha256",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                )
                .unwrap(),
            ],
            Default::default(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            [("big".to_string(), huge_env)].into_iter().collect(),
        )
        .expect("constructs");
        let err = build_fallback_node(drv_path, &huge).unwrap_err();
        assert!(err.contains("nix copy --derivation"), "{err}");
        assert!(err.contains("byte cap"), "{err}");
    }

    /// The realization probe: offenders are exempt iff every declared
    /// output is already present; everything uncertain rejects.
    // r[verify gw.reject.unsupported-hash-algo+4]
    #[tokio::test]
    async fn reject_unrealized_fod_offenders_decides_by_realization() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let offender = |paths: &[&str]| UnverifiableFodOffender {
            drv_path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-legacy.drv".into(),
            output_name: "out".into(),
            algo: "md5".into(),
            declared_paths: paths.iter().map(|s| s.to_string()).collect(),
        };
        let realized = "/nix/store/cccccccccccccccccccccccccccccccc-legacy-out";
        let missing = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-legacy-out";

        let (store, mut store_client, _handle) = spawn_mock_store_with_client().await?;
        store.seed(
            rio_proto::validated::ValidatedPathInfo {
                store_path: StorePath::parse(realized)?,
                nar_hash: [0u8; 32],
                nar_size: 1,
                store_path_hash: vec![],
                deriver: None,
                references: vec![],
                signatures: vec![],
                content_address: None,
                registration_time: 0,
                ultimate: false,
            },
            vec![0u8; 1],
        );

        // No offenders → Ok without any store RPC.
        reject_unrealized_fod_offenders(&[], &mut store_client, None)
            .await
            .expect("empty offender set is a no-op");
        assert_eq!(
            store
                .calls
                .find_missing_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no probe RPC for an empty offender set"
        );

        // All declared outputs realized → exempt.
        reject_unrealized_fod_offenders(&[offender(&[realized])], &mut store_client, None)
            .await
            .expect("realized offender is exempt");

        // Any declared output missing → rejected, naming the algo and
        // the remediation.
        let err = reject_unrealized_fod_offenders(&[offender(&[missing])], &mut store_client, None)
            .await
            .unwrap_err();
        assert!(err.contains("outputHashAlgo 'md5'"), "{err}");
        assert!(
            err.contains("sha256"),
            "remediation names the supported set: {err}"
        );

        // Two offenders, one realized one not → rejected, naming the
        // unrealized one.
        let err = reject_unrealized_fod_offenders(
            &[offender(&[realized]), offender(&[missing])],
            &mut store_client,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains(missing), "{err}");

        // Empty declared path (floating-CA with unsupported algo) →
        // rejected WITHOUT a probe RPC.
        let before = store
            .calls
            .find_missing_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        let err = reject_unrealized_fod_offenders(&[offender(&[""])], &mut store_client, None)
            .await
            .unwrap_err();
        assert!(err.contains("cannot be checked"), "{err}");
        assert_eq!(
            store
                .calls
                .find_missing_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            before,
            "no probe RPC when a declared path is empty/unparseable"
        );

        // Oversized offender set → rejected without a probe RPC.
        let many: Vec<UnverifiableFodOffender> = (0..(MAX_FOD_EXEMPTION_PROBE_PATHS + 1))
            .map(|_| offender(&[realized]))
            .collect();
        let err = reject_unrealized_fod_offenders(&many, &mut store_client, None)
            .await
            .unwrap_err();
        assert!(err.contains("probe cap"), "{err}");

        // Declared output missing but substitutable from the tenant's
        // upstreams → exempt (the scheduler's substitute lane completes
        // it without dispatching).
        let substitutable_path = "/nix/store/ssssssssssssssssssssssssssssssss-legacy-out";
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(substitutable_path.to_string());
        reject_unrealized_fod_offenders(
            &[offender(&[substitutable_path])],
            &mut store_client,
            None,
        )
        .await
        .expect("substitutable offender is exempt");

        // Missing and INDETERMINATE (upstream probe failed) → fail-closed
        // rejection, even though it is also seeded substitutable.
        let indeterminate_path = "/nix/store/iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii-legacy-out";
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(indeterminate_path.to_string());
        store
            .state
            .indeterminate
            .write()
            .unwrap()
            .push(indeterminate_path.to_string());
        let err = reject_unrealized_fod_offenders(
            &[offender(&[indeterminate_path])],
            &mut store_client,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not already realized"), "{err}");

        // The probe carries the session tenant token when one exists,
        // and stays anonymous (None) in dual-mode.
        reject_unrealized_fod_offenders(
            &[offender(&[realized])],
            &mut store_client,
            Some("tok-fod-probe"),
        )
        .await
        .expect("realized offender exempt with a token too");
        {
            let meta = store.calls.find_missing_metadata.read().unwrap();
            assert_eq!(
                meta.last().unwrap().as_deref(),
                Some("tok-fod-probe"),
                "tenant token forwarded on the probe"
            );
            assert!(
                meta.iter().rev().nth(1).unwrap().is_none(),
                "dual-mode probes stay anonymous"
            );
        }

        // Store error → fail-closed rejection.
        store
            .faults
            .fail_find_missing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err =
            reject_unrealized_fod_offenders(&[offender(&[realized])], &mut store_client, None)
                .await
                .unwrap_err();
        assert!(err.contains("store error"), "{err}");

        Ok(())
    }

    #[test]
    fn fod_algo_verifiable_table() {
        for ok in ["sha1", "sha256", "sha512", "r:sha1", "r:sha256", "r:sha512"] {
            assert!(fod_algo_verifiable(ok), "{ok} must be verifiable");
        }
        for bad in ["md5", "r:md5", "blake3", "sha3-256", ""] {
            assert!(!fod_algo_verifiable(bad), "{bad} must not be verifiable");
        }
    }

    /// Declared-hash (fixed-output) outputs are bound to their declared
    /// hash at submission: the declared path must equal
    /// `make_fixed_output(declared hash)`. A junk hash can no longer
    /// exempt an arbitrary (victim) path from validation.
    // r[verify gw.reject.output-path-mismatch+2]
    #[test]
    fn validate_dag_binds_declared_hash_outputs() {
        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fetch.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "bbb".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();
        let fod_at = |algo: &str, hash: &str, path: &str| -> Derivation {
            let aterm = format!(
                r#"Derive([("out","{path}","{algo}","{hash}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{path}")])"#
            );
            Derivation::parse(&aterm).expect("test ATerm parses")
        };
        let digest = vec![0x5au8; 32];
        let nix_hash =
            rio_nix::hash::NixHash::new("sha256".parse().unwrap(), digest.clone()).unwrap();
        let hex_hash = hex::encode(&digest);

        // Honest flat-sha256 and r:sha256 declarations → accepted, in
        // every encoding CppNix accepts (base16 / nixbase32 / base64).
        // r[verify nix.hash.fod-decode]
        use base64::Engine;
        let encodings = [
            hex_hash.clone(),
            rio_nix::store_path::nixbase32::encode(&digest),
            base64::engine::general_purpose::STANDARD.encode(&digest),
        ];
        for recursive in [false, true] {
            let algo = if recursive { "r:sha256" } else { "sha256" };
            let honest = StorePath::make_fixed_output("fetch", &nix_hash, recursive, &[]).unwrap();
            for declared in &encodings {
                let mut cache = HashMap::new();
                cache.insert(key.clone(), fod_at(algo, declared, honest.as_str()));
                assert!(
                    validate_dag(std::slice::from_ref(&node), &cache).is_ok(),
                    "honest {algo} declaration with hash {declared:?} must pass"
                );
            }
        }

        // Junk hash + somebody else's well-formed path → rejected,
        // naming the declared and derived paths.
        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";
        let mut cache = HashMap::new();
        cache.insert(key.clone(), fod_at("sha256", &hex_hash, victim));
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("must match the derivation") && err.contains(victim),
            "squatted FOD path must be rejected naming the path: {err}"
        );

        // Non-hex hash with a parseable path → rejected fail-closed.
        let mut cache = HashMap::new();
        cache.insert(key.clone(), fod_at("sha256", "nothex!", victim));
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(err.contains("base16"), "{err}");

        // Wrong-length digest (sha512 algo, 32-byte hash) → rejected: the
        // length matches none of sha512's three encodings.
        let mut cache = HashMap::new();
        cache.insert(key.clone(), fod_at("sha512", &hex_hash, victim));
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(
            err.contains("outputHash") && err.contains("base16"),
            "{err}"
        );

        // Empty declared path on a hash-declaring output: formerly this
        // gate's "deferred FOD" carve-out, now unrepresentable — the
        // typed boundary rejects it at parse (oracle validatePath
        // analog), so the gate has no carve-out to maintain.
        let err = Derivation::parse(&format!(
            r#"Derive([("out","","sha256","{hex_hash}")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#
        ))
        .unwrap_err();
        assert!(err.to_string().contains("bad path ''"), "{err}");
    }

    /// CppNix's shape rule for fixed-output derivations, enforced at
    /// submission: exactly one output, named "out"; hash-declaring and
    /// plain outputs cannot be mixed.
    #[test]
    fn validate_dag_rejects_mixed_declared_hash_shapes() {
        let drv_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fetch.drv";
        let node = types::DerivationNode {
            drv_path: drv_path.into(),
            drv_hash: "bbb".into(),
            ..Default::default()
        };
        let key = StorePath::parse(drv_path).unwrap();
        let hex_hash = "5a".repeat(32);
        let victim = "/nix/store/ffffffffffffffffffffffffffffffff-victim";

        // Declared-hash output + IA sibling → rejected (mixing).
        let mixed = Derivation::parse(&format!(
            r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fetch","sha256","{hex_hash}"),("doc","{victim}","","")],[],[],"x86_64-linux","/bin/sh",[],[("out",""),("doc","")])"#
        ))
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), mixed);
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(err.contains("cannot be mixed"), "{err}");

        // Two declared-hash outputs → rejected (only one allowed).
        // (Fixed outputs must declare paths under the typed boundary.)
        let p_out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fetch";
        let p_src = "/nix/store/dddddddddddddddddddddddddddddddd-src";
        let two = Derivation::parse(&format!(
            r#"Derive([("out","{p_out}","sha256","{hex_hash}"),("src","{p_src}","sha256","{hex_hash}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{p_out}"),("src","{p_src}")])"#
        ))
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), two);
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(err.contains("only one fixed output"), "{err}");

        // Single declared-hash output not named "out" → rejected.
        let misnamed = Derivation::parse(&format!(
            r#"Derive([("src","{p_src}","sha256","{hex_hash}")],[],[],"x86_64-linux","/bin/sh",[],[("src","{p_src}")])"#
        ))
        .expect("test ATerm parses");
        let mut cache = HashMap::new();
        cache.insert(key.clone(), misnamed);
        let err = validate_dag(std::slice::from_ref(&node), &cache).unwrap_err();
        assert!(err.contains("must be named"), "{err}");
    }

    #[test]
    fn test_single_node_no_features() -> anyhow::Result<()> {
        let drv = make_basic_drv(BTreeMap::new())?;
        let node = build_node("/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi.drv", &drv);
        assert!(node.required_features.is_empty());
        Ok(())
    }

    /// pname falls back to name for raw derivation{} calls that only
    /// set name. Without this, pname="" → no build_samples match →
    /// cold-start probe sizing every time.
    #[test]
    fn test_pname_fallback_to_name() -> anyhow::Result<()> {
        // pname wins when both set (stdenv mkDerivation case).
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        env.insert("name".into(), "hello-2.12".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname, "hello", "pname preferred over name");

        // name fallback when pname absent (raw derivation{} case).
        let mut env = BTreeMap::new();
        env.insert("name".into(), "rawbuild-1.0".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(
            node.pname, "rawbuild-1.0",
            "name fallback — less stable (includes version) but beats empty"
        );

        // neither → empty (no build_samples key possible).
        let drv = make_basic_drv(BTreeMap::new())?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname, "");

        Ok(())
    }

    /// ADR-023 sizing attrs: version + enableParallelBuilding +
    /// preferLocalBuild extracted from drv.env. Nix encodes bools as
    /// "1"/"" (older stdenv) or "true"/"false" (newer) — both forms
    /// must parse.
    #[test]
    fn test_extracts_adr023_attrs() -> anyhow::Result<()> {
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        env.insert("version".into(), "2.12".into());
        env.insert("enableParallelBuilding".into(), "1".into());
        env.insert("preferLocalBuild".into(), "true".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.version.as_deref(), Some("2.12"));
        assert_eq!(node.enable_parallel_building, Some(true));
        assert_eq!(node.enable_parallel_checking, None, "absent stays None");
        assert_eq!(node.prefer_local_build, Some(true));

        // Explicit false: "" and "false" both → Some(false), distinct
        // from absent. enableParallelBuilding="" is how older stdenv
        // spells false.
        let mut env = BTreeMap::new();
        env.insert("enableParallelBuilding".into(), "".into());
        env.insert("enableParallelChecking".into(), "1".into());
        env.insert("preferLocalBuild".into(), "false".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.enable_parallel_building, Some(false));
        assert_eq!(node.enable_parallel_checking, Some(true));
        assert_eq!(node.prefer_local_build, Some(false));
        Ok(())
    }

    /// ADR-023 §Threat-model: tenant-controlled `pname`/`version` are
    /// clamped at 256 chars before they become cache keys / PG columns.
    #[test]
    fn test_clamps_pname_and_version() -> anyhow::Result<()> {
        let long = "x".repeat(10_000);
        let mut env = BTreeMap::new();
        env.insert("pname".into(), long.clone());
        env.insert("version".into(), long.clone());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        assert_eq!(
            node.version.as_deref().map(|s| s.chars().count()),
            Some(MAX_ATTR_LEN)
        );
        // name fallback also clamps.
        let mut env = BTreeMap::new();
        env.insert("name".into(), long.clone());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        // Multi-byte: clamp is by chars not bytes (don't split a code
        // point). 300×'é' (2 bytes each) → 256 chars = 512 bytes.
        let mb = "é".repeat(300);
        let mut env = BTreeMap::new();
        env.insert("pname".into(), mb);
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        assert_eq!(node.pname.len(), MAX_ATTR_LEN * 2, "bytes ≠ chars");
        // Under-threshold is unchanged (no spurious reallocation/copy).
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        let drv = make_basic_drv(env)?;
        assert_eq!(
            build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv).pname,
            "hello"
        );
        Ok(())
    }

    /// ADR-023 §Threat-model: tenant-controlled `requiredSystemFeatures`
    /// is clamped to `MAX_LIST_LEN` elements / `MAX_ATTR_LEN` chars at
    /// the gateway trust boundary before it reaches the
    /// `derivations.required_features` PG `text[]` column,
    /// `SpawnIntent.required_features`, or the scheduler's in-memory
    /// `DerivationState`. The scheduler-side `MAX_HEARTBEAT_FEATURES`
    /// (executor_service.rs) is the second line of defense; this is the
    /// first.
    #[test]
    fn test_clamps_required_features() -> anyhow::Result<()> {
        // Element count: 100 entries → truncated to MAX_LIST_LEN (64).
        // Whitespace-joined env representation (non-structuredAttrs path).
        let many: String = (0..100)
            .map(|i| format!("f{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), many);
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.required_features.len(), MAX_LIST_LEN);
        assert_eq!(node.required_features[0], "f0", "head preserved");
        assert_eq!(
            node.required_features[MAX_LIST_LEN - 1],
            format!("f{}", MAX_LIST_LEN - 1),
            "truncate keeps prefix order"
        );

        // Per-element length: a 1000-char element → 256 chars.
        let long = "x".repeat(1000);
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), long);
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.required_features.len(), 1);
        assert_eq!(node.required_features[0].chars().count(), MAX_ATTR_LEN);

        // Both at once: 100 entries × 1000 chars each → 64 × 256.
        // Use the structuredAttrs JSON path so each element is distinct
        // (the env path is whitespace-split, so a long single token can't
        // also carry a count).
        let json_features: Vec<String> = (0..100)
            .map(|i| format!("{i}{}", "y".repeat(1000)))
            .collect();
        let mut env = BTreeMap::new();
        env.insert(
            "__json".into(),
            serde_json::json!({ "requiredSystemFeatures": json_features }).to_string(),
        );
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.required_features.len(), MAX_LIST_LEN);
        for f in &node.required_features {
            assert_eq!(f.chars().count(), MAX_ATTR_LEN);
        }

        // Under-threshold is unchanged.
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), "kvm big-parallel".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.required_features, vec!["kvm", "big-parallel"]);
        Ok(())
    }

    /// `__structuredAttrs = true` derivations have their user attrs in
    /// `env["__json"]` only, not as separate env keys. Without the
    /// __json fallback, all five hints (pname/version/epb/plb/features)
    /// were always None → structuredAttrs builds got the full probe
    /// shape instead of the minimal-pod / serial-pin paths.
    #[test]
    fn test_extracts_adr023_attrs_from_structured_attrs() -> anyhow::Result<()> {
        let mut env = BTreeMap::new();
        // What Nix actually writes: name + __structuredAttrs flag + the
        // JSON blob. No top-level pname/version/enableParallelBuilding.
        env.insert("name".into(), "foo-1.0".into());
        env.insert("__structuredAttrs".into(), "1".into());
        env.insert(
            "__json".into(),
            serde_json::json!({
                "pname": "foo",
                "version": "1.0",
                "enableParallelBuilding": false,
                "preferLocalBuild": true,
                "requiredSystemFeatures": ["kvm", "big-parallel"],
            })
            .to_string(),
        );
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.pname, "foo", "pname from __json, not name fallback");
        assert_eq!(node.version.as_deref(), Some("1.0"));
        assert_eq!(
            node.enable_parallel_building,
            Some(false),
            "JSON bool false → Some(false), not None"
        );
        assert_eq!(node.prefer_local_build, Some(true));
        assert_eq!(node.required_features, vec!["kvm", "big-parallel"]);

        // Malformed __json → falls through to raw env (no panic).
        let mut env = BTreeMap::new();
        env.insert("__json".into(), "{not json".into());
        env.insert("version".into(), "2.0".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.version.as_deref(), Some("2.0"));
        Ok(())
    }

    /// ADR-023: absent enableParallelBuilding is None, NOT Some(false).
    /// Historical stdenv default was unset; nixpkgs migrating to
    /// default-true. The SLA model treats None as "unknown — explore",
    /// Some(false) as "fix p̄=1". Conflating them would pin every
    /// legacy derivation to one core.
    #[test]
    fn test_absent_adr023_attrs_are_none_not_false() -> anyhow::Result<()> {
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/mj459285d27za2vpn2gggwqzk4c7glz9.drv", &drv);
        assert_eq!(node.version, None);
        assert_eq!(
            node.enable_parallel_building, None,
            "absent ≠ false: None means explore, Some(false) means fix p̄=1"
        );
        assert_eq!(node.prefer_local_build, None);
        Ok(())
    }

    // -------------------------------------------------------------------
    // reconstruct_dag unit tests
    // -------------------------------------------------------------------
    //
    // reconstruct_dag calls resolve_derivation which checks drv_cache FIRST
    // before hitting the store. By pre-populating drv_cache with all needed
    // derivations, we can test reconstruct_dag without a live store.

    use rio_proto::StoreServiceClient;
    use rio_test_support::fixtures::test_drv_path;

    /// A store client whose first RPC fails (lazy connect to dead port).
    /// Used to verify reconstruct_dag fails hard on unresolvable inputDrvs.
    fn unreachable_store() -> StoreServiceClient<tonic::transport::Channel> {
        StoreServiceClient::new(rio_test_support::grpc::dead_channel())
    }

    /// Parse a minimal ATerm derivation with the given inputDrvs.
    /// Format: Derive([outputs],[inputDrvs],[inputSrcs],system,builder,args,env)
    fn make_test_derivation(out_path: &str, input_drvs: &[(&str, &[&str])]) -> Derivation {
        let outputs = format!(r#"[("out","{out_path}","","")]"#);
        let inputs: Vec<String> = input_drvs
            .iter()
            .map(|(path, outs)| {
                let outs_str: Vec<String> = outs.iter().map(|o| format!(r#""{o}""#)).collect();
                format!(r#"("{path}",[{}])"#, outs_str.join(","))
            })
            .collect();
        let input_drvs_str = format!("[{}]", inputs.join(","));
        let aterm = format!(
            r#"Derive({outputs},{input_drvs_str},[],"x86_64-linux","/bin/sh",[],[("out","{out_path}")])"#
        );
        Derivation::parse(&aterm).expect("test ATerm should parse")
    }

    /// Parse a deferred-IA derivation: `("out","","","")` — empty
    /// path/algo/hash. CppNix's `derivationStrict` emits this shape for
    /// any IA whose input has an unknown output path. Distinct from
    /// [`make_test_derivation`] which emits a *concrete* `out_path`.
    fn make_deferred_derivation(input_drvs: &[(&str, &[&str])]) -> Derivation {
        let inputs: Vec<String> = input_drvs
            .iter()
            .map(|(path, outs)| {
                let outs_str: Vec<String> = outs.iter().map(|o| format!(r#""{o}""#)).collect();
                format!(r#"("{path}",[{}])"#, outs_str.join(","))
            })
            .collect();
        let input_drvs_str = format!("[{}]", inputs.join(","));
        let aterm = format!(
            r#"Derive([("out","","","")],{input_drvs_str},[],"x86_64-linux","/bin/sh",[],[])"#
        );
        Derivation::parse(&aterm).expect("test ATerm should parse")
    }

    fn sp(s: &str) -> StorePath {
        StorePath::parse(s).expect("valid test store path")
    }

    #[tokio::test]
    async fn test_reconstruct_dag_single_node_no_inputs() {
        let root_path = sp(&test_drv_path("root"));
        let root_drv =
            make_test_derivation("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-root-out", &[]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        let (nodes, edges) = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 1, "single derivation -> 1 node");
        assert_eq!(nodes[0].drv_path, root_path.to_string());
        assert_eq!(nodes[0].system, "x86_64-linux");
        assert!(edges.is_empty(), "no inputDrvs -> 0 edges");
    }

    #[tokio::test]
    async fn test_reconstruct_dag_one_input_drv() {
        let root_path = sp(&test_drv_path("root"));
        let child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-child.drv");

        let root_drv = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-root-out",
            &[(child_path.as_str(), &["out"])],
        );
        let child_drv =
            make_test_derivation("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-child-out", &[]);

        let mut store = unreachable_store();
        // Pre-populate cache so resolve_derivation finds the child without gRPC.
        let mut cache = HashMap::new();
        cache.insert(child_path.clone(), child_drv);

        let (nodes, edges) = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 2, "root + 1 inputDrv -> 2 nodes");
        assert_eq!(edges.len(), 1, "1 inputDrv relationship -> 1 edge");
        assert_eq!(edges[0].parent_drv_path, root_path.to_string());
        assert_eq!(edges[0].child_drv_path, child_path.to_string());

        // Both nodes should have correct drv_path set.
        let paths: std::collections::HashSet<String> =
            nodes.iter().map(|n| n.drv_path.clone()).collect();
        assert!(paths.contains(&root_path.to_string()));
        assert!(paths.contains(&child_path.to_string()));
    }

    /// Plain IA nodes (statically-known output paths) now carry the
    /// modular hash too — they are no longer "dead bytes on the wire":
    /// the scheduler's ingress inline-content binding seeds
    /// `input_addressed_output_paths`' hash cache from sibling nodes'
    /// hashes to verify declared IA paths without store access.
    // r[verify gw.dag.modulo-hash-all-nodes]
    #[tokio::test]
    async fn populate_modular_hashes_covers_plain_ia_nodes() {
        let root_path = sp(&test_drv_path("ia-root"));
        let child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ia-child.drv");

        // Plain IA: non-empty declared output paths, no hash algo.
        let child_aterm = format!(
            r#"Derive([("out","/nix/store/{}-ia-child","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo c"],[("out","/nix/store/{}-ia-child")])"#,
            "c".repeat(32),
            "c".repeat(32),
        );
        let child_drv = Derivation::parse(&child_aterm).expect("child ATerm");
        let root_aterm = format!(
            r#"Derive([("out","/nix/store/{}-ia-root","","")],[("{}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo r"],[("out","/nix/store/{}-ia-root")])"#,
            "d".repeat(32),
            child_path.as_str(),
            "d".repeat(32),
        );
        let root_drv = Derivation::parse(&root_aterm).expect("root ATerm");

        let mut store = unreachable_store();
        let mut cache = HashMap::new();
        // Production parity: the root .drv is in the session drv_cache
        // (the client uploaded it before requesting the build).
        cache.insert(root_path.clone(), root_drv.clone());
        cache.insert(child_path.clone(), child_drv.clone());

        let (nodes, _edges) = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 2);
        for node in &nodes {
            assert!(
                !node.is_content_addressed,
                "fixture nodes are plain IA: {}",
                node.drv_path
            );
            assert!(
                !node.ca_modular_hash.is_empty(),
                "plain IA node {} must carry the modular hash",
                node.drv_path
            );
            assert_eq!(node.ca_modular_hash.len(), 32);
        }

        // The child's populated hash equals a direct hash_derivation_modulo
        // computation — i.e. it IS the value a consumer can seed a hash
        // cache with.
        let child_node = nodes
            .iter()
            .find(|n| n.drv_path == child_path.as_str())
            .expect("child node present");
        let mut direct_cache = HashMap::new();
        let no_resolve = |_: &str| -> Option<&Derivation> { None };
        let direct = rio_nix::derivation::hash_derivation_modulo(
            &child_drv,
            child_path.as_str(),
            &no_resolve,
            &mut direct_cache,
        )
        .expect("leaf IA hash");
        assert_eq!(
            child_node.ca_modular_hash,
            direct.to_vec(),
            "populated hash equals the direct computation"
        );
    }

    #[tokio::test]
    async fn test_reconstruct_dag_unresolvable_inputdrv_fails() {
        // inputDrv not in cache AND store unreachable -> hard failure.
        // Regression: unresolvable inputDrv must fail, not produce a
        // stub leaf that silently hangs.
        let root_path = sp(&test_drv_path("root"));
        let missing_child = "/nix/store/cccccccccccccccccccccccccccccccc-missing.drv";

        let root_drv = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-root-out",
            &[(missing_child, &["out"])],
        );

        let mut store = unreachable_store();
        let mut cache = HashMap::new(); // child NOT in cache

        let result = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache).await;

        let err = result.expect_err("unresolvable inputDrv must fail reconstruct_dag");
        let msg = err.to_string();
        // P0539: level-batched BFS reports the level + count instead of
        // the specific parent path (one batch can have children from
        // many parents). The failing CHILD path is still surfaced via
        // the underlying GetPath error — that's the operationally useful
        // part (which .drv is missing from the store).
        assert!(
            msg.contains("cannot resolve") && msg.contains("dependencies"),
            "error should mention unresolvable dependency batch, got: {msg}"
        );
        assert!(
            msg.contains(missing_child),
            "error should include the missing child path, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_reconstruct_dag_invalid_inputdrv_path_fails() {
        // inputDrv is not a valid store path -> hard failure (corrupt .drv).
        let root_path = sp(&test_drv_path("root"));
        let bogus_child = "/not/a/store/path";

        let root_drv = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-root-out",
            &[(bogus_child, &["out"])],
        );

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        let result = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache).await;

        let err = result.expect_err("invalid inputDrv path must fail reconstruct_dag");
        let msg = err.to_string();
        assert!(
            msg.contains("corrupted derivation"),
            "error should mention corruption, got: {msg}"
        );
        assert!(
            msg.contains("invalid inputDrv path"),
            "error should mention invalid path, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_reconstruct_dag_transitive_chain() {
        // A -> B -> C chain. All in cache.
        let a_path = sp(&test_drv_path("a"));
        let b_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let c_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-c.drv");

        let a_drv = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out",
            &[(b_path.as_str(), &["out"])],
        );
        let b_drv = make_test_derivation(
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-out",
            &[(c_path.as_str(), &["out"])],
        );
        let c_drv = make_test_derivation("/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-out", &[]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();
        cache.insert(b_path.clone(), b_drv);
        cache.insert(c_path.clone(), c_drv);

        let (nodes, edges) = reconstruct_dag(&a_path, &a_drv, None, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 3, "A->B->C chain -> 3 nodes");
        assert_eq!(edges.len(), 2, "A->B and B->C -> 2 edges");
    }

    /// bug_351: a wide BFS frontier must be rejected BEFORE
    /// `resolve_derivations_batch` fires any RPCs. Pre-fix the
    /// `count > cap` check lagged one level: `count` was still
    /// `1 + parents` while the unbounded frontier was handed to the
    /// fetch loop. With cap=10 and 15 distinct frontier children all
    /// uncached, the dead store would error first ("store unreachable
    /// or .drv missing") — proving fetch ran before the gate. Post-fix
    /// the cap fires at enqueue.
    #[tokio::test]
    async fn test_reconstruct_dag_wide_frontier_rejected_before_fetch() {
        crate::drv_cache::override_max_transitive_inputs(10);

        // Root with 15 distinct inputDrv children, none cached.
        let children: Vec<String> = (0..15)
            .map(|i| format!("/nix/store/{:032}-child{i}.drv", i))
            .collect();
        let child_refs: Vec<(&str, &[&str])> = children
            .iter()
            .map(|p| (p.as_str(), &["out"][..]))
            .collect();
        let root_path = sp(&test_drv_path("wide"));
        let root_drv = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-wide-out",
            &child_refs,
        );

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        let err = reconstruct_dag(&root_path, &root_drv, None, &mut store, &mut cache)
            .await
            .expect_err("over-cap frontier must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("transitive input limit exceeded"),
            "gate must fire BEFORE fetch; got error from fetch path instead: {msg}"
        );
        // 1 (root processed) + 15 (frontier) = 16 — the error should
        // report the would-be total, proving the frontier was counted.
        assert!(
            msg.contains("16"),
            "error should report count+frontier: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // filter_and_inline_drv
    // -------------------------------------------------------------------

    /// Core behavior: only nodes with MISSING outputs get inlined.
    /// Cache-hit nodes stay empty → SubmitBuild doesn't bloat for
    /// derivations that never dispatch.
    #[tokio::test]
    async fn test_filter_and_inline_drv_gates_on_missing() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, mut store_client, _handle) = spawn_mock_store_with_client().await?;

        // Two derivations: "cached" (output in store), "missing" (not).
        let cached_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-cached.drv");
        let cached_out = "/nix/store/cccccccccccccccccccccccccccccccc-cached-out";
        let missing_path = sp("/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing.drv");
        let missing_out = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing-out";

        let cached_drv = make_test_derivation(cached_out, &[]);
        let missing_drv = make_test_derivation(missing_out, &[]);

        // Seed the "cached" output into MockStore so FindMissingPaths
        // reports it as present. Content doesn't matter — just the key.
        store.seed(
            rio_proto::validated::ValidatedPathInfo {
                store_path: rio_nix::store_path::StorePath::parse(cached_out)?,
                nar_hash: [0u8; 32],
                nar_size: 1,
                store_path_hash: vec![],
                deriver: None,
                references: vec![],
                signatures: vec![],
                content_address: None,
                registration_time: 0,
                ultimate: false,
            },
            vec![0u8; 1],
        );

        let mut cache = HashMap::new();
        cache.insert(cached_path.clone(), cached_drv.clone());
        cache.insert(missing_path.clone(), missing_drv.clone());

        let mut nodes = vec![
            build_node(cached_path.as_str(), &cached_drv),
            build_node(missing_path.as_str(), &missing_drv),
        ];

        // Pre: both empty.
        assert!(nodes[0].drv_content.is_empty());
        assert!(nodes[1].drv_content.is_empty());

        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;

        // Post: cached stays empty (won't dispatch), missing is inlined.
        assert!(
            nodes[0].drv_content.is_empty(),
            "cache-hit node should NOT be inlined (won't dispatch)"
        );
        assert!(
            !nodes[1].drv_content.is_empty(),
            "missing-output node SHOULD be inlined (will dispatch)"
        );

        // The inlined content is the ATerm — roundtrip-parse to prove
        // it's real, not garbage.
        let inlined = std::str::from_utf8(&nodes[1].drv_content)?;
        let reparsed = Derivation::parse(inlined)?;
        assert_eq!(reparsed.platform(), "x86_64-linux");
        assert_eq!(
            inlined,
            missing_drv.to_aterm(),
            "inlined bytes = exactly what to_aterm() produces"
        );

        Ok(())
    }

    // r[verify sched.merge.wanted-outputs+2]
    /// Will-dispatch prediction is evaluated over the WANTED subset: a
    /// node whose only missing output is one nothing wants classifies
    /// as a cache hit at the scheduler and never dispatches, so its
    /// drv_content must NOT be inlined (wasted budget). The same node
    /// with the empty (= all declared outputs wanted) sentinel keeps
    /// the conservative behaviour and IS inlined.
    #[tokio::test]
    async fn test_filter_and_inline_drv_missing_unwanted_output_not_inlined() -> anyhow::Result<()>
    {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, mut store_client, _handle) = spawn_mock_store_with_client().await?;

        // Two outputs: `out` (seeded → present) and `debug` (missing).
        let drv_path = sp("/nix/store/wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww-multi.drv");
        let out_base = "/nix/store/wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww-multi";
        let drv = make_multi_output_derivation(out_base, &["debug", "out"], &[]);
        store.seed(
            rio_proto::validated::ValidatedPathInfo {
                store_path: rio_nix::store_path::StorePath::parse(&format!("{out_base}-out"))?,
                nar_hash: [0u8; 32],
                nar_size: 1,
                store_path_hash: vec![],
                deriver: None,
                references: vec![],
                signatures: vec![],
                content_address: None,
                registration_time: 0,
                ultimate: false,
            },
            vec![0u8; 1],
        );

        let mut cache = HashMap::new();
        cache.insert(drv_path.clone(), drv.clone());

        // wanted = {out}: the only missing output (debug) is unwanted →
        // the scheduler will classify this as a hit → must NOT inline.
        let mut node = build_node(drv_path.as_str(), &drv);
        node.wanted_output_names = vec!["out".into()];
        let mut nodes = vec![node];
        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;
        assert!(
            nodes[0].drv_content.is_empty(),
            "all WANTED outputs present → predicted cache hit → not inlined"
        );

        // wanted = [] (all declared outputs wanted): debug is missing
        // and wanted → will dispatch → inlined. Guards against the
        // wanted filter accidentally narrowing the empty sentinel.
        let mut nodes = vec![build_node(drv_path.as_str(), &drv)];
        assert!(nodes[0].wanted_output_names.is_empty());
        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;
        assert!(
            !nodes[0].drv_content.is_empty(),
            "empty wanted sentinel → all outputs wanted → missing debug → inlined"
        );

        Ok(())
    }

    /// Store unreachable → skip inlining entirely. Safe degrade:
    /// worker will fetch. This is an OPTIMIZATION, not correctness.
    #[tokio::test]
    async fn test_filter_and_inline_drv_store_error_skips_safely() {
        let drv_path = sp(&test_drv_path("x"));
        let drv = make_test_derivation("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out", &[]);

        let mut cache = HashMap::new();
        cache.insert(drv_path.clone(), drv.clone());

        let mut nodes = vec![build_node(drv_path.as_str(), &drv)];

        // Dead store — FindMissingPaths will fail.
        let mut dead_store = unreachable_store();

        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        // On error: nothing inlined, no panic, function just returns.
        // Worker-fetch path handles this.
        assert!(
            nodes[0].drv_content.is_empty(),
            "store error → skip inlining (safe degrade)"
        );
    }

    /// Empty expected_output_paths → nothing to gate on → skip.
    /// (BasicDerivation single-node fallback has no expected outputs.)
    #[tokio::test]
    async fn test_filter_and_inline_drv_no_expected_outputs_skips() {
        let mut dead_store = unreachable_store();
        let cache = HashMap::new();

        // Node with no expected_output_paths (like the single-node fallback).
        let mut nodes = vec![types::DerivationNode {
            drv_path: test_drv_path("x"),
            drv_hash: "x".into(),
            expected_output_paths: vec![], // KEY: empty
            ..Default::default()
        }];

        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        // Empty Vec → all_outputs empty → skips FindMissingPaths
        // (doesn't hit the dead store) → will_dispatch=false (no
        // elements to .any() over) → not inlined.
        assert!(nodes[0].drv_content.is_empty());
    }

    /// Floating-CA nodes (expected_output_paths = [""]) must ALWAYS
    /// inline. Their output paths are unknown until built (computed
    /// post-build from NAR hash), so they can't cache-hit by path
    /// and the scheduler's maybe_resolve_ca REQUIRES drv_content to
    /// rewrite placeholders.
    ///
    /// Regression (layer-9 ca-cutoff): previously, the empty string
    /// was sent to FindMissingPaths, which rejected the whole batch
    /// ("invalid store path"), causing the gateway to skip inlining
    /// entirely. The scheduler's store-fetch fallback then depended
    /// on a startup race (scheduler connects to store before store
    /// ready → store_client=None → fallback dead → dispatch
    /// unresolved → worker fails on placeholder).
    #[tokio::test]
    async fn test_filter_and_inline_drv_floating_ca_always_inlined() {
        let ca_path = sp("/nix/store/cacacacacacacacacacacacacacacaca-ca.drv");
        // Floating-CA ATerm: output path empty, hashAlgo set, hash
        // empty. Mirrors what nix produces for __contentAddressed.
        let ca_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#;
        let ca_drv = Derivation::parse(ca_aterm).expect("CA ATerm should parse");

        let mut cache = HashMap::new();
        cache.insert(ca_path.clone(), ca_drv.clone());

        // build_node produces expected_output_paths=[""] for
        // the floating-CA output (DerivationOutput::path() = "").
        let mut nodes = vec![build_node(ca_path.as_str(), &ca_drv)];
        assert_eq!(
            nodes[0].expected_output_paths,
            vec![String::new()],
            "floating-CA output path should be empty string"
        );

        // Dead store — must NOT matter. Empty paths are filtered
        // before FindMissingPaths, so a pure-CA DAG never hits it.
        let mut dead_store = unreachable_store();
        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        assert!(
            !nodes[0].drv_content.is_empty(),
            "floating-CA node must be inlined (empty path → always dispatches)"
        );
        // Inlined bytes = the ATerm we parsed.
        assert_eq!(
            std::str::from_utf8(&nodes[0].drv_content).unwrap(),
            ca_drv.to_aterm(),
        );
    }

    /// Mixed DAG (IA + CA): CA empty strings must not poison
    /// FindMissingPaths for IA nodes. Pre-fix, one CA node made the
    /// store reject the whole batch → no inlining for anyone.
    #[tokio::test]
    async fn test_filter_and_inline_drv_ca_does_not_poison_ia() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (_store, mut store_client, _h) = spawn_mock_store_with_client().await?;

        // IA node: output missing from (empty) mock store → will inline.
        let ia_path = sp("/nix/store/iaiaiaiaiaiaiaiaiaiaiaiaiaiaiaia-ia.drv");
        let ia_out = "/nix/store/iaiaiaiaiaiaiaiaiaiaiaiaiaiaiaia-ia-out";
        let ia_drv = make_test_derivation(ia_out, &[]);

        // CA node: empty output path.
        let ca_path = sp("/nix/store/cacacacacacacacacacacacacacacaca-ca.drv");
        let ca_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#;
        let ca_drv = Derivation::parse(ca_aterm)?;

        let mut cache = HashMap::new();
        cache.insert(ia_path.clone(), ia_drv.clone());
        cache.insert(ca_path.clone(), ca_drv.clone());

        let mut nodes = vec![
            build_node(ia_path.as_str(), &ia_drv),
            build_node(ca_path.as_str(), &ca_drv),
        ];

        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;

        // Both inlined: IA because missing, CA because empty-path.
        // Pre-fix: CA's "" poisoned the batch → store rejected →
        // gateway bailed → BOTH stayed empty.
        assert!(
            !nodes[0].drv_content.is_empty(),
            "IA node should inline (output missing); CA must not poison the batch"
        );
        assert!(
            !nodes[1].drv_content.is_empty(),
            "CA node should inline (empty path → always dispatches)"
        );
        Ok(())
    }

    /// Once the budget gate rejects ANY node, the fast-path arms and
    /// every subsequent node (including tiny ones that would fit in
    /// the headroom) skips `to_aterm()`.
    ///
    /// Regression for the dead fast-path: `total_inlined >=
    /// INLINE_BUDGET_BYTES` could only be true on EXACT equality
    /// (the per-node gate caps `total_inlined ≤ BUDGET`), so once
    /// budget was nearly full every remaining will-dispatch node
    /// still paid the full serialize before rejection — ~148k
    /// throwaway `to_aterm()` calls on a 150k-node cold DAG, no
    /// `.await` in the loop → multi-second tokio worker stall.
    ///
    /// Structural assertion: trailing 1 KiB nodes have empty
    /// `drv_content`. Pre-fix they'd bin-pack into the few-KB
    /// headroom left after the last 60 KiB rejection.
    #[tokio::test]
    async fn test_filter_and_inline_drv_budget_exhaustion_short_circuits() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (_store, mut store_client, _h) = spawn_mock_store_with_client().await?;

        // Derivation whose to_aterm() is ~pad bytes (env var dominates).
        // 60 KiB < MAX_INLINE_DRV_BYTES (64 KiB) so the per-node size
        // gate doesn't fire. INLINE_BUDGET_BYTES (16 MiB) / 60 KiB ≈
        // 273 — somewhere around node 273-279 the budget gate trips.
        let make_padded = |out: &str, pad: usize| -> Derivation {
            let pad_val = "x".repeat(pad);
            let aterm = format!(
                r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",[],[("PAD","{pad_val}"),("out","{out}")])"#
            );
            Derivation::parse(&aterm).expect("padded ATerm parses")
        };

        // 280 × ~60 KiB nodes (fills budget, then a few rejected),
        // then 5 × ~1 KiB nodes. All outputs missing → all dispatch.
        const N_BIG: usize = 280;
        const N_SMALL: usize = 5;
        let mut cache = HashMap::new();
        let mut nodes = Vec::with_capacity(N_BIG + N_SMALL);
        for i in 0..(N_BIG + N_SMALL) {
            // nixbase32 omits e/o/u/t — pad with '0' (valid).
            let drv_path = sp(&format!("/nix/store/{i:0>32}-b{i}.drv"));
            let out = format!("/nix/store/{i:0>32}-b{i}-out");
            let pad = if i < N_BIG { 60 * 1024 } else { 1024 };
            let drv = make_padded(&out, pad);
            nodes.push(build_node(drv_path.as_str(), &drv));
            cache.insert(drv_path, drv);
        }

        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;

        // Find the first rejection (first will-dispatch node with
        // empty drv_content). All subsequent nodes — including the
        // trailing 1 KiB ones — must also be empty.
        let first_empty = nodes
            .iter()
            .position(|n| n.drv_content.is_empty())
            .expect("budget must exhaust before all 280×60KiB are inlined");
        assert!(
            first_empty < N_BIG,
            "first rejection should be a 60 KiB node (budget ≈ 273 of them)"
        );
        for (i, n) in nodes.iter().enumerate().skip(first_empty) {
            assert!(
                n.drv_content.is_empty(),
                "node {i} inlined after budget exhausted at {first_empty} — \
                 fast-path didn't arm (pre-fix: 1 KiB tail nodes bin-pack into headroom)"
            );
        }
        // Explicit witness: the 1 KiB tail is fully skipped.
        for n in &nodes[N_BIG..] {
            assert!(n.drv_content.is_empty(), "1 KiB tail must be skipped");
        }

        // total_inlined stayed accurate (≤ budget). Sum is the actual
        // bytes inlined, not the cap.
        let total: usize = nodes.iter().map(|n| n.drv_content.len()).sum();
        assert!(total <= INLINE_BUDGET_BYTES);
        Ok(())
    }

    // -------------------------------------------------------------------
    // iter_cached_drvs + compute_modular_hash_cached (P0413 walker dedup)
    // -------------------------------------------------------------------

    /// 3 nodes, 2 in drv_cache, 1 miss. Helper yields exactly the
    /// 2 cached indices; the miss is debug-logged and skipped.
    /// Mutation-anchor: returning `None` unconditionally → `hits` is
    /// empty → assert fires.
    #[test]
    fn cached_drv_walker_skips_cache_miss() {
        let a = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv");
        let b = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let c = sp("/nix/store/cccccccccccccccccccccccccccccccc-c.drv");
        let mk_node = |p: &StorePath| types::DerivationNode {
            drv_path: p.to_string(),
            ..Default::default()
        };
        let nodes = vec![mk_node(&a), mk_node(&b), mk_node(&c)];

        // Only a + c in the cache; b is the miss (BFS-inconsistency).
        let mut drv_cache = HashMap::new();
        drv_cache.insert(
            a,
            make_test_derivation("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out", &[]),
        );
        drv_cache.insert(
            c,
            make_test_derivation("/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-out", &[]),
        );

        let hits: Vec<usize> = iter_cached_drvs(&nodes, &drv_cache, "test")
            .map(|(i, _, _)| i)
            .collect();
        assert_eq!(hits, vec![0, 2], "indices 0+2 cached; 1 skipped");
    }

    /// inputDrv not in cache → `hash_derivation_modulo` returns
    /// `InputNotFound` → wrapper returns `None` (no panic, no garbage).
    #[test]
    fn modular_hash_wrapper_none_on_resolver_miss() {
        let drv = make_test_derivation(
            "/nix/store/gsqizyqxzjbdjyb1jav5zjndvsadgs15-out",
            &[(
                "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing.drv",
                &["out"],
            )],
        );
        let drv_cache = HashMap::new(); // empty — guaranteed miss
        let mut hash_cache = HashMap::new();
        assert!(
            compute_modular_hash_cached(
                &drv,
                "/nix/store/pppppppppppppppppppppppppppppppp-parent.drv",
                &drv_cache,
                &mut hash_cache
            )
            .is_none(),
            "resolver miss → InputNotFound → None (warn-and-degrade)"
        );
    }

    // r[verify sched.ca.detect]
    /// `populate_needs_resolve`: IA parent depending on a floating-CA
    /// child gets `needs_resolve = true` (the ia.deferred case from
    /// ADR-018 Appendix B). IA-on-IA stays false. Floating-CA self
    /// stays true (set earlier by `build_node`, unchanged here).
    #[test]
    fn populate_needs_resolve_ia_deferred() {
        let ca_child_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca.drv");
        let ia_child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ia.drv");
        let ia_parent_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-parent.drv");
        let ia_pure_path = sp("/nix/store/dddddddddddddddddddddddddddddddd-pure.drv");

        // Floating-CA child: hash_algo set, hash empty.
        let ca_child_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let ca_child = Derivation::parse(ca_child_aterm).unwrap();

        // IA child: all-empty output tuple.
        let ia_child = make_test_derivation("/nix/store/x265isadxs1xhsd5larxdal956cxmsk1-out", &[]);

        // IA parent depending on the floating-CA child.
        let ia_parent = make_test_derivation(
            "/nix/store/pj8izfqdpab526ki2jvdgjfmvjs5zs9x-out",
            &[(ca_child_path.as_str(), &["out"])],
        );

        // IA parent depending only on the IA child (pure IA-on-IA).
        let ia_pure = make_test_derivation(
            "/nix/store/9js5mjfd9addln5rwamdijq5mj4x9j7d-out",
            &[(ia_child_path.as_str(), &["out"])],
        );

        let mut drv_cache = HashMap::new();
        drv_cache.insert(ca_child_path.clone(), ca_child.clone());
        drv_cache.insert(ia_child_path.clone(), ia_child.clone());
        drv_cache.insert(ia_parent_path.clone(), ia_parent.clone());
        drv_cache.insert(ia_pure_path.clone(), ia_pure.clone());

        let mut nodes = vec![
            build_node(ca_child_path.as_str(), &ca_child),
            build_node(ia_child_path.as_str(), &ia_child),
            build_node(ia_parent_path.as_str(), &ia_parent),
            build_node(ia_pure_path.as_str(), &ia_pure),
        ];

        // build_node already set needs_resolve from self.
        assert!(nodes[0].needs_resolve, "floating-CA self → true pre-pass");
        assert!(!nodes[2].needs_resolve, "IA parent → false pre-pass");

        populate_needs_resolve(&mut nodes, &drv_cache);

        assert!(nodes[0].needs_resolve, "floating-CA unchanged");
        assert!(!nodes[1].needs_resolve, "IA leaf → still false");
        assert!(
            nodes[2].needs_resolve,
            "ia.deferred: IA with floating-CA input → needs_resolve=true"
        );
        assert!(
            !nodes[3].needs_resolve,
            "IA with only-IA inputs → still false"
        );
    }

    // r[verify sched.ca.detect]
    /// `populate_needs_resolve`: 2+ IA levels stacked above a
    /// floating-CA leaf. CppNix's `derivationStrict` propagates the
    /// deferred kind upward — every IA whose input has an unknown output
    /// path becomes deferred-IA `("out","","","")`. The OLD predicate
    /// (`has_ca_floating_outputs`) was true for the CA leaf only, so
    /// IA-mid was flagged but IA-grandparent was NOT (its child IA-mid
    /// has empty algo). The grandparent then dispatched with `/1<hash>`
    /// placeholders unresolved → worker `path '/1…' is not in the Nix
    /// store`. The new predicate (`has_unknown_output_paths`) is true
    /// for both the CA leaf and every deferred-IA above it.
    #[test]
    fn populate_needs_resolve_ia_deferred_chain() {
        let ca_leaf_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca.drv");
        let ia_mid_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-mid.drv");
        let ia_gp_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-gp.drv");
        let ia_conc_path = sp("/nix/store/dddddddddddddddddddddddddddddddd-conc.drv");
        let ia_pure_path = sp("/nix/store/ffffffffffffffffffffffffffffffff-pure.drv");

        // Floating-CA leaf: hash_algo set, hash empty.
        let ca_leaf = Derivation::parse(
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
        )
        .unwrap();
        // Deferred-IA mid: ("out","","","") depending on the CA leaf.
        let ia_mid = make_deferred_derivation(&[(ca_leaf_path.as_str(), &["out"])]);
        // Deferred-IA grandparent: depends on deferred-IA mid (NOT on
        // the CA leaf directly). This is the case the old predicate missed.
        let ia_gp = make_deferred_derivation(&[(ia_mid_path.as_str(), &["out"])]);
        // Concrete-IA child (non-empty out_path) and a parent on it —
        // proves concrete-IA-on-concrete-IA still stays false.
        let ia_conc = make_test_derivation("/nix/store/x265isadxs1xhsd5larxdal956cxmsk1-out", &[]);
        let ia_pure = make_test_derivation(
            "/nix/store/9js5mjfd9addln5rwamdijq5mj4x9j7d-out",
            &[(ia_conc_path.as_str(), &["out"])],
        );

        let mut drv_cache = HashMap::new();
        for (p, d) in [
            (&ca_leaf_path, &ca_leaf),
            (&ia_mid_path, &ia_mid),
            (&ia_gp_path, &ia_gp),
            (&ia_conc_path, &ia_conc),
            (&ia_pure_path, &ia_pure),
        ] {
            drv_cache.insert(p.clone(), d.clone());
        }

        let mut nodes = vec![
            build_node(ca_leaf_path.as_str(), &ca_leaf),
            build_node(ia_mid_path.as_str(), &ia_mid),
            build_node(ia_gp_path.as_str(), &ia_gp),
            build_node(ia_conc_path.as_str(), &ia_conc),
            build_node(ia_pure_path.as_str(), &ia_pure),
        ];

        // Pre-pass: only the floating-CA leaf is self-floating.
        assert!(nodes[0].needs_resolve);
        assert!(!nodes[1].needs_resolve);
        assert!(!nodes[2].needs_resolve);

        populate_needs_resolve(&mut nodes, &drv_cache);

        assert!(nodes[0].needs_resolve, "floating-CA leaf unchanged");
        assert!(
            nodes[1].needs_resolve,
            "IA-mid (child = floating-CA) → true"
        );
        assert!(
            nodes[2].needs_resolve,
            "IA-grandparent (child = deferred-IA) → true (regression: \
             has_ca_floating_outputs would have left this false)"
        );
        assert!(!nodes[3].needs_resolve, "concrete-IA leaf → still false");
        assert!(
            !nodes[4].needs_resolve,
            "IA with only concrete-IA input → still false"
        );
    }

    // -------------------------------------------------------------------
    // populate_wanted_outputs
    // -------------------------------------------------------------------

    /// Multi-output test derivation: each name in `output_names` gets the
    /// concrete IA path `<out_base>-<name>`. Same ATerm shape as
    /// [`make_test_derivation`] but with N outputs instead of one.
    fn make_multi_output_derivation(
        out_base: &str,
        output_names: &[&str],
        input_drvs: &[(&str, &[&str])],
    ) -> Derivation {
        let outputs: Vec<String> = output_names
            .iter()
            .map(|n| format!(r#"("{n}","{out_base}-{n}","","")"#))
            .collect();
        let inputs: Vec<String> = input_drvs
            .iter()
            .map(|(path, outs)| {
                let outs_str: Vec<String> = outs.iter().map(|o| format!(r#""{o}""#)).collect();
                format!(r#"("{path}",[{}])"#, outs_str.join(","))
            })
            .collect();
        let aterm = format!(
            r#"Derive([{}],[{}],[],"x86_64-linux","/bin/sh",[],[])"#,
            outputs.join(","),
            inputs.join(",")
        );
        Derivation::parse(&aterm).expect("test ATerm should parse")
    }

    // r[verify gw.dag.reconstruct+4]
    /// A node consumed by one parent that names only `{out}` of its three
    /// declared outputs gets `wanted_output_names == ["out"]`. The `^*`
    /// root keeps the empty (= all declared outputs wanted) sentinel.
    /// `output_names` / `expected_output_paths` keep the FULL declared
    /// set — the wanted set is an additional field, not a narrowing.
    #[test]
    fn populate_wanted_outputs_from_consumer_input_drvs() {
        let parent_path = sp("/nix/store/pppppppppppppppppppppppppppppppp-parent.drv");
        let child_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc.drv");

        // Child declares debug/dev/out; the parent's inputDrvs entry for
        // it names only {out}.
        let child = make_multi_output_derivation(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc",
            &["debug", "dev", "out"],
            &[],
        );
        let parent = make_test_derivation(
            "/nix/store/mjc59j05djg72j3gf2pp7487bwwg7cgr-parent-out",
            &[(child_path.as_str(), &["out"])],
        );

        let mut drv_cache = HashMap::new();
        drv_cache.insert(parent_path.clone(), parent.clone());
        drv_cache.insert(child_path.clone(), child.clone());

        let mut nodes = vec![
            build_node(parent_path.as_str(), &parent),
            build_node(child_path.as_str(), &child),
        ];
        // Pre-pass: build_node leaves the field empty (= all wanted).
        assert!(nodes[0].wanted_output_names.is_empty());
        assert!(nodes[1].wanted_output_names.is_empty());

        populate_wanted_outputs(
            &mut nodes,
            &drv_cache,
            parent_path.as_str(),
            Some(&OutputSpec::All),
        );

        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "^* root → empty (= all declared outputs wanted)"
        );
        assert_eq!(
            nodes[1].wanted_output_names,
            vec!["out"],
            "child wanted = the one output its only consumer's inputDrvs names"
        );
        // The declared-output arrays are NOT narrowed (they stay
        // index-paired and load-bearing for the assignment token / GC
        // pins / client output report).
        assert_eq!(nodes[1].output_names, vec!["debug", "dev", "out"]);
        assert_eq!(nodes[1].expected_output_paths.len(), 3);
    }

    /// Two parents naming different subsets of the same child →
    /// the child's wanted set is the sorted union.
    #[test]
    fn populate_wanted_outputs_unions_across_consumers() {
        let root_path = sp("/nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-root.drv");
        let a_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv");
        let b_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let child_path = sp("/nix/store/dddddddddddddddddddddddddddddddd-glibc.drv");

        let child = make_multi_output_derivation(
            "/nix/store/dddddddddddddddddddddddddddddddd-glibc",
            &["debug", "dev", "out"],
            &[],
        );
        // A consumes child^out, B consumes child^dev.
        let a = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out",
            &[(child_path.as_str(), &["out"])],
        );
        let b = make_test_derivation(
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-out",
            &[(child_path.as_str(), &["dev"])],
        );
        let root = make_test_derivation(
            "/nix/store/zjsm32y5pjmp8y5v8a5gdls3m0az5lx1-out",
            &[(a_path.as_str(), &["out"]), (b_path.as_str(), &["out"])],
        );

        let mut drv_cache = HashMap::new();
        drv_cache.insert(root_path.clone(), root.clone());
        drv_cache.insert(a_path.clone(), a.clone());
        drv_cache.insert(b_path.clone(), b.clone());
        drv_cache.insert(child_path.clone(), child.clone());

        let mut nodes = vec![
            build_node(root_path.as_str(), &root),
            build_node(a_path.as_str(), &a),
            build_node(b_path.as_str(), &b),
            build_node(child_path.as_str(), &child),
        ];

        populate_wanted_outputs(
            &mut nodes,
            &drv_cache,
            root_path.as_str(),
            Some(&OutputSpec::All),
        );

        assert_eq!(
            nodes[3].wanted_output_names,
            vec!["dev", "out"],
            "union of both consumers' inputDrvs sets, sorted for determinism"
        );
        // The intermediate consumers are each named {out} by the root.
        assert_eq!(nodes[1].wanted_output_names, vec!["out"]);
        assert_eq!(nodes[2].wanted_output_names, vec!["out"]);
    }

    /// The root request's OutputSpec seeds the root node's wanted set:
    /// `^out` → `["out"]`; `^*` → empty (= all); no spec at all (the
    /// `wopBuildDerivation` path carries no output selection) → empty.
    #[test]
    fn populate_wanted_outputs_root_outputs_spec() {
        let root_path = sp("/nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-multi.drv");
        let root = make_multi_output_derivation(
            "/nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-multi",
            &["debug", "dev", "out"],
            &[],
        );
        let mut drv_cache = HashMap::new();
        drv_cache.insert(root_path.clone(), root.clone());

        // `^dev,out` → exactly those names, sorted.
        let mut nodes = vec![build_node(root_path.as_str(), &root)];
        populate_wanted_outputs(
            &mut nodes,
            &drv_cache,
            root_path.as_str(),
            Some(&OutputSpec::Names(vec!["out".into(), "dev".into()])),
        );
        assert_eq!(
            nodes[0].wanted_output_names,
            vec!["dev", "out"],
            "^dev,out root → exactly the requested names, sorted"
        );

        // `^*` → empty (= all declared outputs wanted).
        let mut nodes = vec![build_node(root_path.as_str(), &root)];
        populate_wanted_outputs(
            &mut nodes,
            &drv_cache,
            root_path.as_str(),
            Some(&OutputSpec::All),
        );
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "^* root → empty sentinel (all declared outputs wanted)"
        );

        // No spec (wopBuildDerivation carries none) → empty (= all).
        let mut nodes = vec![build_node(root_path.as_str(), &root)];
        populate_wanted_outputs(&mut nodes, &drv_cache, root_path.as_str(), None);
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "no root spec → empty sentinel (all declared outputs wanted)"
        );
    }

    /// The BFS root — the node the client named as a build target — is
    /// marked `explicitly_requested` for every OutputSpec shape (Names,
    /// All, and the spec-less wopBuildDerivation path); dependency
    /// nodes are not. The scheduler's roots-only prune keys on this
    /// flag to retain a requested target that another requested
    /// target's closure swallowed (it is then NOT a structural root of
    /// the combined submission).
    #[test]
    fn populate_wanted_outputs_marks_root_explicitly_requested() {
        let parent_path = sp("/nix/store/pppppppppppppppppppppppppppppppp-parent.drv");
        let child_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc.drv");

        let child =
            make_test_derivation("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-glibc-out", &[]);
        let parent = make_test_derivation(
            "/nix/store/mjc59j05djg72j3gf2pp7487bwwg7cgr-parent-out",
            &[(child_path.as_str(), &["out"])],
        );

        let mut drv_cache = HashMap::new();
        drv_cache.insert(parent_path.clone(), parent.clone());
        drv_cache.insert(child_path.clone(), child.clone());

        for spec in [
            Some(OutputSpec::Names(vec!["out".into()])),
            Some(OutputSpec::All),
            None,
        ] {
            let mut nodes = vec![
                build_node(parent_path.as_str(), &parent),
                build_node(child_path.as_str(), &child),
            ];
            populate_wanted_outputs(&mut nodes, &drv_cache, parent_path.as_str(), spec.as_ref());
            assert!(
                nodes[0].explicitly_requested,
                "the requested root must be flagged for spec {spec:?}"
            );
            assert!(
                !nodes[1].explicitly_requested,
                "a node demanded only as a dependency must NOT be flagged (spec {spec:?})"
            );
        }
    }

    /// `reconstruct_dag` wires the pass: the post-BFS node list carries
    /// consumer-derived wanted sets without the caller doing anything
    /// beyond passing the root's OutputSpec. Mirrors production where
    /// `resolve_derivation` has already inserted the root into
    /// `drv_cache` before `reconstruct_dag` runs.
    #[tokio::test]
    async fn test_reconstruct_dag_populates_wanted_outputs() {
        let root_path = sp(&test_drv_path("root"));
        let child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc.drv");

        let child = make_multi_output_derivation(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc",
            &["debug", "dev", "out"],
            &[],
        );
        let root = make_test_derivation(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-root-out",
            &[(child_path.as_str(), &["dev"])],
        );

        let mut store = unreachable_store();
        let mut cache = HashMap::new();
        // Production inserts the root via resolve_derivation before
        // calling reconstruct_dag; the populate pass reads consumers'
        // inputDrvs out of the cache.
        cache.insert(root_path.clone(), root.clone());
        cache.insert(child_path.clone(), child);

        let (nodes, _edges) = reconstruct_dag(
            &root_path,
            &root,
            Some(&OutputSpec::Names(vec!["out".into()])),
            &mut store,
            &mut cache,
        )
        .await
        .expect("reconstruct should succeed");

        let by_path: HashMap<&str, &types::DerivationNode> =
            nodes.iter().map(|n| (n.drv_path.as_str(), n)).collect();
        assert_eq!(
            by_path[root_path.as_str()].wanted_output_names,
            vec!["out"],
            "root wanted = the request's ^out"
        );
        assert_eq!(
            by_path[child_path.as_str()].wanted_output_names,
            vec!["dev"],
            "child wanted = the output the root's inputDrvs entry names"
        );
    }
}
