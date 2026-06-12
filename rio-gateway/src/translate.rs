//! DAG reconstruction and gRPC request building.
//!
//! Translates the per-session derivation cache into `SubmitBuildRequest`
//! messages for the scheduler, walking `inputDrvs` recursively to build
//! the full derivation graph.
// r[impl gw.dag.reconstruct+3]

use std::collections::{BTreeSet, HashMap, HashSet};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::{Derivation, DerivationLike};
use rio_nix::protocol::derived_path::OutputSpec;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tonic::transport::Channel;
use tracing::{debug, warn};

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
/// `populate_wanted_outputs`); the node/edge set is identical for
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

/// Fill `ca_modular_hash` on each CA node via `hash_derivation_modulo`.
///
/// The scheduler's CA-on-CA resolve queries `realisations` keyed on
/// `(modular_hash, output_name)`. The modular hash needs the full
/// transitive closure of parsed derivations (what BFS put in
/// `drv_cache`). Memoised via one shared `hash_cache` — for N CA
/// nodes sharing a common CA input, the common sub-hash is computed
/// once.
///
/// Best-effort: hash failure → warn, leave empty. Scheduler's
/// `collect_ca_inputs` skips empty; resolve degrades to worker-fail
/// + retry-with-backoff.
fn populate_ca_modular_hashes(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) {
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
    // IA nodes with statically-known paths: ca_modular_hash stays
    // empty — dead bytes on the wire. Deferred-IA (empty output
    // path) DOES need it: the scheduler writes a realisation on
    // completion keyed by this hash so the gateway's
    // wopQueryDerivationOutputMap can answer the client.
    let hashes: Vec<(usize, Vec<u8>)> =
        iter_cached_drvs(nodes, drv_cache, "populate_ca_modular_hashes")
            .filter(|(_, node, drv)| node.is_content_addressed || drv.has_unknown_output_paths())
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
// r[impl gw.dag.reconstruct+3]
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

/// Validate a DAG before SubmitBuild. Returns `Err(reason)` if the
/// DAG should be rejected — caller sends STDERR_ERROR with the
/// reason. Returns `Ok(())` if valid.
///
/// Checks:
/// - `__noChroot=1` in any node's env → reject (sandbox escape)
/// - `nodes.len() > MAX_DAG_NODES` → reject (early, before gRPC)
///
/// The scheduler ALSO enforces MAX_DAG_NODES (grpc/mod.rs:298);
/// this is an early reject to save the gRPC round-trip for obvious
/// over-size submissions. The __noChroot check is ONLY here — the
/// scheduler doesn't have the env (DerivationNode doesn't carry it).
pub fn validate_dag(
    nodes: &[types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) -> Result<(), String> {
    // MAX_DAG_NODES: early reject. Scheduler enforces too but
    // this saves a 100MB+ gRPC message for obvious over-size.
    if nodes.len() > rio_common::limits::MAX_DAG_NODES {
        return Err(format!(
            "DAG too large: {} nodes > {} max",
            nodes.len(),
            rio_common::limits::MAX_DAG_NODES
        ));
    }

    // r[impl gw.reject.nochroot]
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
    if let Some((_, node, _)) = iter_cached_drvs(nodes, drv_cache, "validate_dag")
        .find(|(_, _, drv)| StructuredEnv::new(drv.env()).bool("__noChroot") == Some(true))
    {
        return Err(format!(
            "derivation {} requests __noChroot (sandbox escape) — not permitted",
            node.drv_path
        ));
    }

    Ok(())
}

/// `__structuredAttrs`-aware env lookup, mirroring Nix's
/// `ParsedDerivation::get{String,Bool,Strings}Attr`.
///
/// When a derivation sets `__structuredAttrs = true`, Nix's
/// `derivationStrict` serializes user attrs into `env["__json"]` ONLY —
/// they do NOT appear as separate env keys. Direct `env.get("foo")`
/// returns None, so the ADR-023 sizing hints (and pre-existing
/// `requiredSystemFeatures` / `__noChroot`) were always None for
/// structuredAttrs drvs. JSON is checked first, then raw env, matching
/// upstream semantics.
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

pub(crate) struct StructuredEnv<'a> {
    env: &'a std::collections::BTreeMap<String, String>,
    json: Option<serde_json::Value>,
}

impl<'a> StructuredEnv<'a> {
    pub(crate) fn new(env: &'a std::collections::BTreeMap<String, String>) -> Self {
        let json = env.get("__json").and_then(|s| serde_json::from_str(s).ok());
        Self { env, json }
    }

    fn string(&self, key: &str) -> Option<String> {
        self.json
            .as_ref()
            .and_then(|j| j.get(key)?.as_str().map(String::from))
            .or_else(|| self.env.get(key).cloned())
    }

    /// [`Self::string`] with a `MAX_ATTR_LEN`-char clamp. ADR-023
    /// §Threat-model: `pname`/`version` are tenant-controlled and feed
    /// the per-tenant `SlaEstimator` cache key + `build_samples.pname`
    /// PG column; a 1 MiB pname is otherwise carried verbatim through
    /// proto → DerivationNode → ModelKey → cache key → PG.
    fn string_clamped(&self, key: &str) -> Option<String> {
        self.string(key).map(|mut s| {
            if s.chars().count() > MAX_ATTR_LEN {
                s = s.chars().take(MAX_ATTR_LEN).collect();
            }
            s
        })
    }

    pub(crate) fn bool(&self, key: &str) -> Option<bool> {
        self.json
            .as_ref()
            .and_then(|j| j.get(key)?.as_bool())
            .or_else(|| self.env.get(key).map(|v| v == "1" || v == "true"))
    }

    fn strings(&self, key: &str) -> Option<Vec<String>> {
        self.json
            .as_ref()
            .and_then(|j| {
                Some(
                    j.get(key)?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                )
            })
            .or_else(|| {
                self.env
                    .get(key)
                    .map(|s| s.split_whitespace().map(String::from).collect())
            })
    }

    /// [`Self::strings`] with a `MAX_LIST_LEN`-element / `MAX_ATTR_LEN`-
    /// char clamp. ADR-023 §Threat-model: `requiredSystemFeatures` is
    /// tenant-controlled and feeds the `derivations.required_features`
    /// PG `text[]` column, `SpawnIntent.required_features` on the wire,
    /// and the scheduler's in-memory `DerivationState`. Same threat as
    /// [`Self::string_clamped`] but for a list. See also
    /// `executor_service.rs`'s `MAX_HEARTBEAT_FEATURES` (the
    /// post-translate scheduler-side bound) and `snapshot.rs`'s LRU
    /// debounce-key clamp — both are second-line defenses behind this
    /// gateway-side bound at the trust boundary.
    fn strings_clamped(&self, key: &str) -> Option<Vec<String>> {
        self.strings(key).map(|mut v| {
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
        // Input-addressed derivations use the store path as the drv_hash.
        // This ensures every node has a unique, non-empty key in the DAG.
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
            .string_clamped("pname")
            .or_else(|| env.string_clamped("name"))
            .unwrap_or_default(),
        // ADR-023 sizing attrs. Nix bool env values are "1"/"" (older
        // stdenv) or "true"/"false" (newer). Absent stays None — for
        // enableParallelBuilding in particular, absent ≠ false (nixpkgs
        // is migrating to default-true; None means "unknown, explore").
        version: env.string_clamped("version"),
        enable_parallel_building: env.bool("enableParallelBuilding"),
        enable_parallel_checking: env.bool("enableParallelChecking"),
        prefer_local_build: env.bool("preferLocalBuild"),
        system: drv.platform().to_string(),
        required_features: env
            .strings_clamped("requiredSystemFeatures")
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
        // ADR-024 P2a: populated by populate_drv_digests() AFTER the
        // full BFS — input digests need every input drv parsed, which
        // only the complete drv_cache guarantees. The BasicDerivation
        // single-node fallback (no full drv) leaves both empty =
        // legacy edges-driven submission.
        drv_digest: Vec::new(),
        input_drv_digests: vec![],
    }
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

/// ADR-024 P2a: populate `drv_digest`/`input_drv_digests` on every
/// node and upload missing drv blobs to the store, turning an ssh-ng
/// submission into a digest-bearing one — the gateway feeds the same
/// scheduler path the native client will use.
///
/// All-or-nothing: the scheduler rejects mixed submissions, so any
/// failure (a node without a parsed drv in `drv_cache`, an input drv
/// missing from the cache, a `HasDrvs`/`PutDrvBlobs` error, no JWT to
/// authenticate the tenant-scoped put) leaves EVERY node digest-less —
/// the legacy edges path, which remains accepted (P2b retires it).
/// Mirrors `filter_and_inline_drv`'s posture: this is an upgrade, not
/// a correctness requirement, and the safe degrade is "submit legacy".
///
/// Byte-stability contract (`r[store.drv.verify-on-put]`): the
/// uploaded `body` is the canonical proto encoding from
/// [`rio_proto::derivation_util::to_proto`] +
/// [`canonical_encode`](rio_proto::derivation_util::canonical_encode);
/// the store verifies digest, canonical form, and drv_path recompute
/// before storing, and serves the same bytes back from `GetDrvBlob`.
// r[impl gw.submit.digest-populate]
pub async fn populate_digests_and_upload_drvs(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
    drv_blob_client: Option<&mut rio_proto::DrvBlobServiceClient<Channel>>,
    jwt_token: Option<&str>,
) {
    use rio_proto::derivation_util;

    let Some(client) = drv_blob_client else {
        return; // no store drv-blob endpoint configured — legacy submission
    };
    // PutDrvBlobs/HasDrvs are tenant-scoped; without a session JWT the
    // store rejects with "requires a tenant". Dual-mode/dev sessions
    // submit legacy.
    let Some(jwt) = jwt_token else {
        debug!("no session JWT; skipping drv digest population (legacy submission)");
        return;
    };

    // Pass 1: canonical proto + digest for every node AND every input
    // drv. reconstruct_dag puts the full closure in both `nodes` and
    // `drv_cache`, so inputs normally resolve via the node set; the
    // cache lookup is the defensive fallback. Any miss → legacy.
    let mut canon: HashMap<&str, ([u8; 32], Vec<u8>)> = HashMap::with_capacity(nodes.len());
    for node in nodes.iter() {
        let Ok(sp) = StorePath::parse(&node.drv_path) else {
            return;
        };
        let Some(drv) = drv_cache.get(&sp) else {
            debug!(drv_path = %node.drv_path,
                   "node missing from drv_cache; skipping digest population (legacy submission)");
            return;
        };
        let msg = derivation_util::to_proto(drv);
        let bytes = derivation_util::canonical_encode(&msg);
        let digest = derivation_util::derivation_digest(&msg);
        canon.insert(node.drv_path.as_str(), (digest, bytes));
    }
    let mut input_digests: Vec<Vec<Vec<u8>>> = Vec::with_capacity(nodes.len());
    for node in nodes.iter() {
        let sp = StorePath::parse(&node.drv_path).expect("parsed in pass 1");
        let drv = drv_cache.get(&sp).expect("present in pass 1");
        let mut per_node = Vec::with_capacity(drv.input_drvs().len());
        for input_path in drv.input_drvs().keys() {
            match canon.get(input_path.as_str()) {
                Some((digest, _)) => per_node.push(digest.to_vec()),
                None => {
                    // Input drv outside the node set (shouldn't happen
                    // for a reconstruct_dag closure) — bail to legacy
                    // rather than submit a reference the scheduler
                    // can't resolve.
                    debug!(drv_path = %node.drv_path, input = %input_path,
                           "input drv not in submission; skipping digest population");
                    return;
                }
            }
        }
        input_digests.push(per_node);
    }

    // Pass 2: presence-check, then upload misses. Batched: HasDrvs
    // accepts up to 65 536 digests, PutDrvBlobs up to 4 096 blobs —
    // we stay well under both and additionally bound put batches by
    // bytes so one RPC stays far below the gRPC message cap.
    let digests: Vec<Vec<u8>> = nodes
        .iter()
        .map(|n| canon[n.drv_path.as_str()].0.to_vec())
        .collect();
    const HAS_BATCH: usize = 32_768;
    let mut missing: Vec<usize> = Vec::new();
    for (batch_idx, batch) in digests.chunks(HAS_BATCH).enumerate() {
        let req = match crate::handler::with_jwt(
            types::HasDrvsRequest {
                digests: batch.to_vec(),
            },
            Some(jwt),
        ) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "HasDrvs request build failed; legacy submission");
                return;
            }
        };
        let bitmap = match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            client.has_drvs(req),
        )
        .await
        {
            Ok(Ok(r)) => r.into_inner().bitmap,
            Ok(Err(e)) => {
                warn!(error = %e, "HasDrvs failed; skipping digest population (legacy submission)");
                return;
            }
            Err(_) => {
                warn!("HasDrvs timed out; skipping digest population (legacy submission)");
                return;
            }
        };
        for (i, _) in batch.iter().enumerate() {
            let present = bitmap
                .get(i / 8)
                .is_some_and(|byte| (byte >> (i % 8)) & 1 == 1);
            if !present {
                missing.push(batch_idx * HAS_BATCH + i);
            }
        }
    }

    const PUT_BATCH_MAX_BLOBS: usize = 1_024;
    const PUT_BATCH_MAX_BYTES: usize = 8 * 1024 * 1024;
    let mut batch: Vec<types::DrvBlob> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut uploaded = 0usize;
    let mut flush = async |batch: &mut Vec<types::DrvBlob>| -> bool {
        if batch.is_empty() {
            return true;
        }
        let req = match crate::handler::with_jwt(
            types::PutDrvBlobsRequest {
                blobs: std::mem::take(batch),
            },
            Some(jwt),
        ) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "PutDrvBlobs request build failed; legacy submission");
                return false;
            }
        };
        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            client.put_drv_blobs(req),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                warn!(error = %e, "PutDrvBlobs failed; skipping digest population (legacy submission)");
                false
            }
            Err(_) => {
                warn!("PutDrvBlobs timed out; skipping digest population (legacy submission)");
                false
            }
        }
    };
    for idx in &missing {
        let node = &nodes[*idx];
        let (digest, bytes) = &canon[node.drv_path.as_str()];
        if batch.len() >= PUT_BATCH_MAX_BLOBS || batch_bytes + bytes.len() > PUT_BATCH_MAX_BYTES {
            if !flush(&mut batch).await {
                return;
            }
            batch_bytes = 0;
        }
        batch_bytes += bytes.len();
        uploaded += 1;
        batch.push(types::DrvBlob {
            digest: digest.to_vec(),
            drv_path: node.drv_path.clone(),
            body: bytes.clone(),
        });
    }
    if !flush(&mut batch).await {
        return;
    }

    // Pass 3: every blob is now in the store — mark the submission
    // digest-bearing. Mutation happens only on full success so a
    // partial failure can never produce a mixed submission. The
    // own-digest list is materialized first because `canon`'s keys
    // borrow `nodes` and the assignment below needs `iter_mut`.
    let own_digests: Vec<Vec<u8>> = nodes
        .iter()
        .map(|n| canon[n.drv_path.as_str()].0.to_vec())
        .collect();
    drop(canon);
    for ((node, own), inputs) in nodes.iter_mut().zip(own_digests).zip(input_digests) {
        node.drv_digest = own;
        node.input_drv_digests = inputs;
    }
    debug!(
        nodes = nodes.len(),
        uploaded, "populated drv digests (uploaded missing drv blobs)"
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
        let output = DerivationOutput::new("out", "/nix/store/test-out", "", "")?;
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
        let output = DerivationOutput::new("out", "/nix/store/test-out", hash_algo, hash)?;
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
        let node = build_node("/nix/store/test.drv", &basic);
        assert_eq!(node.is_content_addressed, want_ca, "basic: is_ca");
        assert_eq!(node.is_fixed_output, want_fod, "basic: strict is_fod");

        // Full Derivation path (via ATerm parse).
        let aterm = format!(
            r#"Derive([("out","/nix/store/aaa-out","{aterm_algo}","{hash}")],[],[],"x86_64-linux","/bin/sh",[],[])"#
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

        let node = build_node("/nix/store/test.drv", &drv);
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
                drv_path: "/nix/store/aaa-test.drv".into(),
                drv_hash: "aaa".into(),
                ..Default::default()
            },
            types::DerivationNode {
                drv_path: "/nix/store/bbb-test.drv".into(),
                drv_hash: "bbb".into(),
                ..Default::default()
            },
        ];
        let empty_cache = HashMap::new();
        assert!(validate_dag(&nodes, &empty_cache).is_ok());
    }

    // __noChroot rejection is hard to unit-test here because it
    // needs a Derivation in drv_cache with __noChroot=1 in env,
    // and constructing a full Derivation (not BasicDerivation)
    // requires ATerm parsing or a complex builder. Coverage comes
    // from the golden tests at tests/wire_opcodes/build.rs (seed
    // NOCHROOT_DRV_ATERM into the mock store so resolve_derivation
    // populates drv_cache, then drive opcodes 36 + 46 and assert
    // the failure BuildResult carries the "sandbox escape" message).

    #[test]
    fn test_single_node_no_features() -> anyhow::Result<()> {
        let drv = make_basic_drv(BTreeMap::new())?;
        let node = build_node("/nix/store/test.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(node.pname, "hello", "pname preferred over name");

        // name fallback when pname absent (raw derivation{} case).
        let mut env = BTreeMap::new();
        env.insert("name".into(), "rawbuild-1.0".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(
            node.pname, "rawbuild-1.0",
            "name fallback — less stable (includes version) but beats empty"
        );

        // neither → empty (no build_samples key possible).
        let drv = make_basic_drv(BTreeMap::new())?;
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        assert_eq!(
            node.version.as_deref().map(|s| s.chars().count()),
            Some(MAX_ATTR_LEN)
        );
        // name fallback also clamps.
        let mut env = BTreeMap::new();
        env.insert("name".into(), long.clone());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        // Multi-byte: clamp is by chars not bytes (don't split a code
        // point). 300×'é' (2 bytes each) → 256 chars = 512 bytes.
        let mb = "é".repeat(300);
        let mut env = BTreeMap::new();
        env.insert("pname".into(), mb);
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(node.pname.chars().count(), MAX_ATTR_LEN);
        assert_eq!(node.pname.len(), MAX_ATTR_LEN * 2, "bytes ≠ chars");
        // Under-threshold is unchanged (no spurious reallocation/copy).
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        let drv = make_basic_drv(env)?;
        assert_eq!(build_node("/nix/store/x.drv", &drv).pname, "hello");
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
        assert_eq!(node.required_features.len(), MAX_LIST_LEN);
        for f in &node.required_features {
            assert_eq!(f.chars().count(), MAX_ATTR_LEN);
        }

        // Under-threshold is unchanged.
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), "kvm big-parallel".into());
        let drv = make_basic_drv(env)?;
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let node = build_node("/nix/store/x.drv", &drv);
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
        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[]);

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
            "/nix/store/aaa-root-out",
            &[(child_path.as_str(), &["out"])],
        );
        let child_drv = make_test_derivation("/nix/store/bbb-child-out", &[]);

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

    #[tokio::test]
    async fn test_reconstruct_dag_unresolvable_inputdrv_fails() {
        // inputDrv not in cache AND store unreachable -> hard failure.
        // Regression: unresolvable inputDrv must fail, not produce a
        // stub leaf that silently hangs.
        let root_path = sp(&test_drv_path("root"));
        let missing_child = "/nix/store/cccccccccccccccccccccccccccccccc-missing.drv";

        let root_drv =
            make_test_derivation("/nix/store/aaa-root-out", &[(missing_child, &["out"])]);

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

        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[(bogus_child, &["out"])]);

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

        let a_drv = make_test_derivation("/nix/store/aaa-out", &[(b_path.as_str(), &["out"])]);
        let b_drv = make_test_derivation("/nix/store/bbb-out", &[(c_path.as_str(), &["out"])]);
        let c_drv = make_test_derivation("/nix/store/ccc-out", &[]);

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
        let root_drv = make_test_derivation("/nix/store/aaa-wide-out", &child_refs);

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
        let drv = make_test_derivation("/nix/store/aaa-out", &[]);

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
        drv_cache.insert(a, make_test_derivation("/nix/store/aaa-out", &[]));
        drv_cache.insert(c, make_test_derivation("/nix/store/ccc-out", &[]));

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
            "/nix/store/oooooooooooooooooooooooooooooooo-out",
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
        let ia_child = make_test_derivation("/nix/store/ia-out", &[]);

        // IA parent depending on the floating-CA child.
        let ia_parent = make_test_derivation(
            "/nix/store/parent-out",
            &[(ca_child_path.as_str(), &["out"])],
        );

        // IA parent depending only on the IA child (pure IA-on-IA).
        let ia_pure =
            make_test_derivation("/nix/store/pure-out", &[(ia_child_path.as_str(), &["out"])]);

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
        let ia_conc = make_test_derivation("/nix/store/ia-out", &[]);
        let ia_pure =
            make_test_derivation("/nix/store/pure-out", &[(ia_conc_path.as_str(), &["out"])]);

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

    // r[verify gw.dag.reconstruct+3]
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
            "/nix/store/ppp-parent-out",
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
        let a = make_test_derivation("/nix/store/aaa-out", &[(child_path.as_str(), &["out"])]);
        let b = make_test_derivation("/nix/store/bbb-out", &[(child_path.as_str(), &["dev"])]);
        let root = make_test_derivation(
            "/nix/store/rrr-out",
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

        let child = make_test_derivation("/nix/store/aaa-glibc-out", &[]);
        let parent = make_test_derivation(
            "/nix/store/ppp-parent-out",
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
            "/nix/store/aaa-root-out",
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
