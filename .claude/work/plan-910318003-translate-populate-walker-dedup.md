# Plan 910318003: translate.rs post-BFS populate_* walker dedup + hash_derivation_modulo wrapper

consol-mc220 finding at [`rio-gateway/src/translate.rs:155`](../../rio-gateway/src/translate.rs) + `:267`. Two post-BFS populate walkers share ~30L of scaffold:

| Walker | Lines | Shared scaffold |
|---|---|---|
| `populate_input_srcs_sizes` | `:155-240` | `for node in nodes.iter()` → `StorePath::parse(&node.drv_path)` → `drv_cache.get(&sp)` → `debug!("… not in cache (BFS inconsistency)")` + `continue` on miss |
| `populate_ca_modular_hashes` | `:267-317` | identical node-walk + parse + cache-lookup + BFS-inconsistency-debug + continue |

The `let Some(drv) = drv_cache.get(&sp) else { debug!(… "BFS inconsistency") …; continue; }` pattern at `:176-179` and `:285-293` is byte-near-identical. [P0254](plan-0254-ca-metrics-vm-demo.md) added `:267`; [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md) (in-flight) adds a third consumer of the same scaffold (scheduler-side resolve-fetch needs the same "node → StorePath → cache lookup" walk when re-populating `ca_modular_hash` for recovered derivations).

Second dedup: the `hash_derivation_modulo` wrapper at [`translate.rs:294-316`](../../rio-gateway/src/translate.rs) (`resolve = |p| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp))` → `match hash_derivation_modulo(drv, path, &resolve, &mut hash_cache)` → `Ok(hash) | Err(e) => warn!(…)`) is near-identical to [`handler/build.rs:1150-1180`](../../rio-gateway/src/handler/build.rs) (same closure, same match, same warn-on-error). Both are "compute modular hash via drv_cache-backed resolver, warn-and-skip on error" — one hashes for `node.ca_modular_hash`, the other for `BuildResult.builtOutputs` realisation keys.

[P0388](plan-0388-translate-rs-generic-node-builder.md) collapsed the dual `DerivationNode` constructors — this is the next layer of translate.rs consolidation. [P0400](plan-0400-graph-page-skipped-worker-race.md) is doc-only at `translate.rs:360` (scheduler-doc cross-ref comment) — non-overlapping.

## Entry criteria

- [P0254](plan-0254-ca-metrics-vm-demo.md) merged (`populate_ca_modular_hashes` at `:267` + `collect_ca_inputs` consumer exist)
- [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md) merged (the third walker-consumer — without it the dedup only covers 2 sites and P0408 would need to re-inline the scaffold)

## Tasks

### T1 — `refactor(gateway):` translate.rs — extract `iter_cached_drvs` walker helper

NEW helper in [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs) (above `populate_input_srcs_sizes`):

```rust
/// Yield `(node_idx, &Derivation)` for every node whose `drv_path` is in
/// `drv_cache`. Logs BFS-inconsistency at debug for misses (cache was
/// populated BY the BFS — a miss means the BFS and this walk disagree
/// about what nodes exist). Used by every post-BFS populate_* pass.
///
/// Iterator because the callers need different mut-access: sizes sums
/// into `node.input_srcs_nar_size`, hashes writes `node.ca_modular_hash`.
/// Both need &mut nodes[idx] AFTER the lookup, so we can't yield &mut Node
/// (aliasing — drv_cache lookup borrows nodes immutably via drv_path).
/// Index-based iteration sidesteps the borrow-checker split.
fn iter_cached_drvs<'a>(
    nodes: &'a [types::DerivationNode],
    drv_cache: &'a HashMap<StorePath, Derivation>,
    walker_name: &'static str,
) -> impl Iterator<Item = (usize, &'a Derivation)> + 'a {
    nodes.iter().enumerate().filter_map(move |(idx, node)| {
        let sp = StorePath::parse(&node.drv_path).ok()?;
        match drv_cache.get(&sp) {
            Some(drv) => Some((idx, drv)),
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
```

Rewrite both callers:

```rust
// populate_input_srcs_sizes:
let mut all_srcs = HashSet::new();
for (_idx, drv) in iter_cached_drvs(nodes, drv_cache, "populate_input_srcs_sizes") {
    all_srcs.extend(drv.input_srcs().iter().cloned());
}
// … batch query unchanged …
for (idx, drv) in iter_cached_drvs(nodes, drv_cache, "populate_input_srcs_sizes") {
    nodes[idx].input_srcs_nar_size = drv.input_srcs().iter()
        .map(|s| sizes.get(s).copied().unwrap_or(0))
        .fold(0u64, |acc, x| acc.saturating_add(x));
}

// populate_ca_modular_hashes:
for (idx, _drv) in iter_cached_drvs(nodes, drv_cache, "populate_ca_modular_hashes") {
    if !nodes[idx].is_content_addressed { continue; }
    // … hash_derivation_modulo call unchanged (T2 extracts the wrapper) …
}
```

**Borrow-checker care:** the `for … in iter_cached_drvs(nodes, …)` holds an immutable borrow on `nodes` via the iterator. Writing `nodes[idx].field = …` INSIDE the loop body needs either (a) collect idxs first then second loop, or (b) a two-pass "collect into Vec<(idx, value)> → apply" pattern. Current code at `:225-239` already does two loops (collect srcs, then sum per node) — the helper preserves that structure. For `populate_ca_modular_hashes`, collect `(idx, hash)` pairs then assign: `let hashes: Vec<_> = iter_cached_drvs(…).filter_map(|(idx, drv)| …).collect(); for (idx, h) in hashes { nodes[idx].ca_modular_hash = h; }`. Check at dispatch whether `&mut nodes[idx]` inside the filter_map closure compiles (likely not — `nodes` borrowed by iterator).

### T2 — `refactor(gateway):` extract `compute_modular_hash_cached` wrapper

NEW helper (in `translate.rs` or promoted to `rio-nix::derivation` if worker/scheduler also need it — check at dispatch):

```rust
/// Compute `hash_derivation_modulo` for a drv via a `drv_cache`-backed
/// resolver. Shared memoization via `hash_cache` — pass the same map
/// across calls to reuse sub-hashes. Errors warn-and-return-None: the
/// caller decides what "no hash" means (skip CA resolve vs empty
/// builtOutputs). Both gateway callers (translate + handler/build)
/// treat None the same way — log + continue with a degraded result.
fn compute_modular_hash_cached(
    drv: &Derivation,
    drv_path: &str,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Option<[u8; 32]> {
    let resolve = |p: &str| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp));
    match rio_nix::derivation::hash_derivation_modulo(drv, drv_path, &resolve, hash_cache) {
        Ok(hash) => Some(hash),
        Err(e) => {
            warn!(drv_path = %drv_path, error = %e,
                "hash_derivation_modulo failed; caller will degrade");
            None
        }
    }
}
```

Rewrite both callsites:

- `translate.rs:294-316` → `if let Some(hash) = compute_modular_hash_cached(drv, &node.drv_path, drv_cache, &mut hash_cache) { node.ca_modular_hash = hash.to_vec(); }` (the existing warn! text about "scheduler resolve will skip" becomes the caller's responsibility OR the helper takes an `on_fail_note: &str` param for the warn! tail)
- `handler/build.rs:1150-1180` → `if let Some(hash) = compute_modular_hash_cached(drv_obj, drv_path, &drv_cache, &mut hash_cache) { … results.push(build_result.clone().with_outputs_from_drv(drv_obj, &hex::encode(hash))); } else { results.push(build_result.clone()); }`

**Where to put the helper:** `translate.rs` is the natural home (both callers import from it — `handler/build.rs` already `use crate::translate::reconstruct_dag`). If [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md)'s scheduler-side fetch ALSO needs modular-hash recomputation for recovered `ca_modular_hash` (check at dispatch — P0408 T1's fetch gets `drv_content` bytes, then may need to recompute `ca_modular_hash` for the recovered node), promote to `rio-nix::derivation::compute_modular_hash_cached` or `rio-common`. Prefer translate.rs-local unless 3rd consumer is confirmed cross-crate.

### T3 — `test(gateway):` iter_cached_drvs skips-miss + hash-wrapper error-path

MODIFY [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs) `#[cfg(test)] mod tests` (or wherever translate tests live):

```rust
#[test]
fn iter_cached_drvs_skips_cache_miss() {
    // 3 nodes, 2 in drv_cache, 1 miss. Walker yields 2 (idx, &drv)
    // pairs. The miss logs debug but doesn't panic.
    let nodes = vec![mk_node("a.drv"), mk_node("b.drv"), mk_node("c.drv")];
    let drv_cache: HashMap<_, _> = [
        (StorePath::parse("a.drv").unwrap(), mk_drv()),
        (StorePath::parse("c.drv").unwrap(), mk_drv()),
    ].into();
    let hits: Vec<_> = iter_cached_drvs(&nodes, &drv_cache, "test").map(|(i, _)| i).collect();
    assert_eq!(hits, vec![0, 2]);  // b.drv skipped
}

#[test]
fn compute_modular_hash_cached_none_on_resolver_miss() {
    // drv with inputDrvs referencing a path NOT in drv_cache →
    // hash_derivation_modulo returns InputNotFound → wrapper returns None.
    // (Doesn't panic, doesn't return garbage.)
    let drv = mk_drv_with_input("missing.drv");
    let drv_cache = HashMap::new();  // empty — guaranteed miss
    let mut hash_cache = HashMap::new();
    assert!(compute_modular_hash_cached(&drv, "parent.drv", &drv_cache, &mut hash_cache).is_none());
}
```

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c nextest-standalone)
- `grep -c 'fn iter_cached_drvs' rio-gateway/src/translate.rs` → 1 (helper defined)
- `grep -c 'iter_cached_drvs(' rio-gateway/src/translate.rs` → ≥4 (def + ≥3 call-sites: 2× in populate_input_srcs_sizes two-pass, ≥1× in populate_ca_modular_hashes)
- `grep 'BFS inconsistency' rio-gateway/src/translate.rs | wc -l` → 1 (the debug! string lives ONLY in the helper; both callers' inline copies deleted)
- `grep -c 'fn compute_modular_hash_cached' rio-gateway/src/translate.rs` → 1 (or `rio-nix/src/derivation/mod.rs` if promoted — check crate)
- `grep -c 'compute_modular_hash_cached(' rio-gateway/src/translate.rs rio-gateway/src/handler/build.rs` → ≥2 (both callers migrated)
- `grep 'let resolve = |p' rio-gateway/src/handler/build.rs` → 0 (inline closure at `:1156-1157` deleted — migrated to helper)
- `grep 'match rio_nix::derivation::hash_derivation_modulo' rio-gateway/src/handler/build.rs` → 0 (inline match at `:1158-1180` deleted — migrated to helper)
- `cargo nextest run -p rio-gateway translate::` → same test count as pre-dedup + 2 (T3's new tests)
- `wc -l rio-gateway/src/translate.rs` → ≤1250L (down from 1316L — ~60L net reduction from 2× populate-scaffold dedup + ~20L from hash-wrapper dedup, +~30L helper defs)
- **Mutation:** comment out the `None` branch in `iter_cached_drvs` (no debug! + no skip — return None directly) → test passes still (semantics unchanged). Comment out the `Some((idx, drv))` return → `iter_cached_drvs_skips_cache_miss` fails (zero hits). Proves test covers the happy path.

## Tracey

References existing markers:
- `r[gw.opcode.build-paths-with-results]` at [`gateway.md:320`](../../docs/src/components/gateway.md) — `handler/build.rs:1150` is inside `handle_build_paths_with_results`; T2's wrapper extraction doesn't change the opcode's behavior, only its implementation layout
- `r[sched.ca.resolve+2]` at [`scheduler.md:290`](../../docs/src/components/scheduler.md) — `populate_ca_modular_hashes` feeds the scheduler's CA-resolve via `ca_modular_hash`; T1's walker dedup doesn't change what's populated

No new markers. Refactor-only; no spec-behavior change.

## Files

```json files
[
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T1: +iter_cached_drvs helper (~25L) above :155; rewrite populate_input_srcs_sizes (:155-240) + populate_ca_modular_hashes (:267-317) to use it. T2: +compute_modular_hash_cached wrapper (~20L); rewrite :294-316 to use it. T3: +2 tests in cfg(test) mod. HOT count=26"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T2: rewrite :1150-1180 hash_derivation_modulo inline-match → compute_modular_hash_cached call. HOT count=28"}
]
```

```
rio-gateway/src/
├── translate.rs         # T1: +iter_cached_drvs, rewrite 2× populate_*
│                        # T2: +compute_modular_hash_cached, rewrite :294-316
│                        # T3: +2 tests
└── handler/build.rs     # T2: :1150-1180 → helper call
```

## Dependencies

```json deps
{"deps": [254, 408], "soft_deps": [388, 400, 384], "note": "consol-mc220 (discovered_from=254 via populate_ca_modular_hashes scaffold, +408 as third consumer). HARD-dep P0254: populate_ca_modular_hashes exists at :267 (P0254 added it) — without it T1 has only 1 caller. HARD-dep P0408: the scheduler-side recovery-resolve-fetch is the third walker-consumer — if P0408 adds a gateway-side or scheduler-side populate pass (check at dispatch), it should use iter_cached_drvs from the start. Sequencing P0408 BEFORE this plan means P0408's impl inlines the scaffold (same as the 2 existing), then this plan dedups 3; sequencing this plan FIRST means P0408 uses the helper directly. Prefer P0408-first: P0408 is correctness (recovery resolve), this is refactor — correctness lands earlier. Soft-dep P0388 (translate.rs generic node-builder — collapsed DerivationNode constructors at :45-90; non-overlapping with populate_* at :155+). Soft-dep P0400 (Graph page — touches translate.rs:360 doc-comment only; non-overlapping). Soft-dep P0384 (is_fixed_output strict+trait — touches rio-nix Derivation trait; if compute_modular_hash_cached promotes to rio-nix, P0384's trait changes apply). translate.rs HOT=26, handler/build.rs HOT=28 — both are busy but T1/T2 are localized (T1 at :155-317, T2 at :1150-1180)."}
```

**Depends on:** [P0254](plan-0254-ca-metrics-vm-demo.md) — `populate_ca_modular_hashes` at `:267`. [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md) — third walker-consumer; should land first (correctness > refactor).

**Conflicts with:** `translate.rs` count=26 — [P0388](plan-0388-translate-rs-generic-node-builder.md) touches `:45-90` constructors (non-overlapping with `:155-317`). [P0400](plan-0400-graph-page-skipped-worker-race.md) touches `:360` doc-comment only. `handler/build.rs` count=28 — [P0310](plan-0310-gateway-client-option-propagation.md) and others touch `handle_build_paths` body; T2's edit is localized to `:1150-1180` per-result loop (non-overlapping with submission/scheduler-client sections at `:1100-1145`).
