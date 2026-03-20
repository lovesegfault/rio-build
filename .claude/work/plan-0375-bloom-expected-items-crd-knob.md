# Plan 0375: bloom_expected_items WorkerPool CRD knob → env injection

**rev-p288 feature gap.** [P0288](plan-0288-bloom-fill-ratio-gauge.md) landed `rio_worker_bloom_fill_ratio` gauge + `worker.toml bloom_expected_items` TOML knob + `RIO_BLOOM_EXPECTED_ITEMS` env var ([`config.rs:131-132`](../../rio-worker/src/config.rs)). The observability spec at [`observability.md:185`](../../docs/src/observability.md) says "Operators bump `bloom_expected_items` in `worker.toml` for long-lived pools" — but there's **no k8s knob**. The alert fires, the operator sees `fill_ratio >= 0.5`, and the only remediation is… `kubectl rollout restart sts/<pool>` (which resets the filter but doesn't raise the ceiling).

Convention is **CRD field → env injection** (same as `fuseThreads`, `fusePassthrough`, `daemonTimeoutSecs` — all `Option<T>` in [`WorkerPoolSpec`](../../rio-crds/src/workerpool.rs), injected as `RIO_*` env vars at [`builders.rs:580-593`](../../rio-controller/src/reconcilers/workerpool/builders.rs) when Some). Without this, the TOML knob is vestigial — nobody mounts a `worker.toml` ConfigMap; everything is env-driven.

## Entry criteria

- [P0288](plan-0288-bloom-fill-ratio-gauge.md) merged (`bloom_expected_items` config field + `RIO_BLOOM_EXPECTED_ITEMS` env var exist) — **DONE**

## Tasks

### T1 — `feat(crds):` WorkerPoolSpec.bloom_expected_items field

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) — add alongside `fuse_threads` at `:157`:

```rust
/// Bloom filter capacity (expected number of paths). The worker's
/// FUSE cache bloom filter never shrinks — evicted paths stay as
/// stale positives. For long-lived STS pools churning past the
/// default (50k), the fill ratio climbs and FPR degrades. Bump
/// this before restarting the pool. Not applicable to ephemeral
/// pools (fresh pod = fresh bloom).
///
/// Default (unset) → worker uses its compile-time 50_000. See
/// `rio_worker_bloom_fill_ratio` gauge for when to tune.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub bloom_expected_items: Option<u64>,
```

`u64` not `usize` — CRD schema types must be platform-independent; usize varies. The cast to usize at injection time (`n as usize`) is safe on 64-bit (and the worker crashes long before 2^32 paths on 32-bit anyway).

Add to `cel_rules_in_schema` test assert list near `:647` so the CRD schema test sees the new field.

### T2 — `feat(controller):` inject RIO_BLOOM_EXPECTED_ITEMS

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — alongside `fuse_threads` at `:580-581`:

```rust
// r[impl ctrl.pool.bloom-knob]
if let Some(n) = wp.spec.bloom_expected_items {
    e.push(env("RIO_BLOOM_EXPECTED_ITEMS", &n.to_string()));
}
```

Same "only inject when explicitly set" semantics as the other optional knobs — `None` means "worker's compiled-in default", not "inject 50000" (comment at `:575-579` explains the drift-avoidance rationale).

### T3 — `feat(crds):` WPS PoolTemplate.bloom_expected_items (optional pass-through)

MODIFY [`rio-crds/src/workerpoolset.rs`](../../rio-crds/src/workerpoolset.rs) — if `PoolTemplate` mirrors other WorkerPool knobs (check at dispatch — some template fields are deliberately omitted as "per-class only"). If PoolTemplate should carry it:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub bloom_expected_items: Option<u64>,
```

And propagate in [`workerpoolset/builders.rs`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) — after `:135` (the `// --- Unset optional ---` block, p234-worktree ref):

```rust
bloom_expected_items: template.bloom_expected_items,
```

**Decision heuristic:** if `fuse_threads` is in PoolTemplate, `bloom_expected_items` should be too (both are worker-tuning knobs). If not, skip T3 (the WPS child uses the WorkerPool default, which is `None` → worker compile-time).

### T4 — `docs(obs):` observability.md remediation text — mention CRD knob

MODIFY [`docs/src/observability.md:185`](../../docs/src/observability.md). "Operators bump `bloom_expected_items` in `worker.toml`" → "Operators set `spec.bloomExpectedItems` on the WorkerPool (or `poolTemplate.bloomExpectedItems` on a WorkerPoolSet) — injects `RIO_BLOOM_EXPECTED_ITEMS`; the pod restart that applies the CRD edit also resets the filter."

If there's an Alerting-section bullet list (check `:303+` — "rev-p288 Alerting-§-list-bloom doc-bug" from the sink suggests there's a `## Alerting` section that should list the bloom fill-ratio alert), add a remediation step there too.

### T5 — `test(controller):` env injection unit test

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — near the existing env-injection tests (grep `RIO_FUSE_THREADS` in `#[cfg(test)]` to find the pattern):

```rust
// r[verify ctrl.pool.bloom-knob]
#[test]
fn bloom_expected_items_env_injected_when_set() {
    let mut wp = test_wp("bloom-test");
    wp.spec.bloom_expected_items = Some(200_000);
    let sts = build_statefulset(&wp).expect("build ok");
    let envs = collect_container_env(&sts, "rio-worker");
    assert_eq!(
        envs.get("RIO_BLOOM_EXPECTED_ITEMS").map(|s| s.as_str()),
        Some("200000"),
        "bloom_expected_items should inject env var"
    );
}

#[test]
fn bloom_expected_items_not_injected_when_unset() {
    let wp = test_wp("bloom-default");  // spec.bloom_expected_items = None
    let sts = build_statefulset(&wp).expect("build ok");
    let envs = collect_container_env(&sts, "rio-worker");
    assert!(
        !envs.contains_key("RIO_BLOOM_EXPECTED_ITEMS"),
        "unset bloom_expected_items should NOT inject env (worker compile-time default wins)"
    );
}
```

Adjust `build_statefulset` / `collect_container_env` helper names to match existing test fixtures.

## Exit criteria

- `/nbr .#ci` green
- `grep 'bloom_expected_items' rio-crds/src/workerpool.rs` → ≥2 hits (field decl + test assert)
- `grep 'RIO_BLOOM_EXPECTED_ITEMS' rio-controller/src/reconcilers/workerpool/builders.rs` → ≥1 hit (T2 injection)
- `grep 'r\[impl ctrl.pool.bloom-knob\]' rio-controller/src/reconcilers/workerpool/builders.rs` → 1 hit
- T5: `cargo nextest run -p rio-controller bloom_expected_items_env` → ≥2 passed
- `grep 'bloomExpectedItems\|bloom_expected_items' docs/src/observability.md` → ≥1 hit (T4: remediation updated)
- `nix develop -c tracey query rule ctrl.pool.bloom-knob` → non-empty rule text + ≥1 impl + ≥1 verify (T2 impl + T5 verify)
- T3 conditional: `grep 'bloom_expected_items' rio-crds/src/workerpoolset.rs rio-controller/src/reconcilers/workerpoolset/builders.rs` → ≥2 hits IF PoolTemplate mirrors the knob; 0 hits if skipped with rationale comment
- `just crd-regen && git diff --exit-code infra/helm/crds/` — regen is idempotent (OR shows the new `bloomExpectedItems` field in `workerpools.rio.build.yaml`)

## Tracey

References existing markers:
- `r[obs.metric.bloom-fill-ratio]` — T4 extends the remediation text (doc-side, no annotation change)

Adds new markers to component specs:
- `r[ctrl.pool.bloom-knob]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions below)

## Spec additions

**T2 — new `r[ctrl.pool.bloom-knob]`** (goes to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after the existing `r[ctrl.pool.ephemeral-single-build]` block at `:111`, standalone paragraph, blank line before, col 0):

```
r[ctrl.pool.bloom-knob]
`WorkerPoolSpec.bloomExpectedItems` (optional) injects `RIO_BLOOM_EXPECTED_ITEMS` into the worker container env. Unset → worker uses its compile-time default (50k). Same only-inject-when-set semantics as `fuseThreads` and `daemonTimeoutSecs`: injecting the default would pin it at controller-build time, not worker-build time.
```

## Files

```json files
[
  {"path": "rio-crds/src/workerpool.rs", "action": "MODIFY", "note": "T1: +bloom_expected_items Option<u64> field after :157 + test assert :647"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T2: inject RIO_BLOOM_EXPECTED_ITEMS after :580; T5: 2 env-injection tests"},
  {"path": "rio-crds/src/workerpoolset.rs", "action": "MODIFY", "note": "T3: PoolTemplate.bloom_expected_items (conditional — only if fuse_threads is there)"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T3: propagate template.bloom_expected_items to child spec (conditional)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: :185 remediation text — worker.toml → spec.bloomExpectedItems"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T2 spec-addition: +r[ctrl.pool.bloom-knob] marker after :111"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "T1: CRD regen — +bloomExpectedItems in spec schema"},
  {"path": "infra/helm/crds/workerpoolsets.rio.build.yaml", "action": "MODIFY", "note": "T3 conditional: CRD regen — +bloomExpectedItems in poolTemplate schema"}
]
```

```
rio-crds/src/
├── workerpool.rs             # T1: field + test
└── workerpoolset.rs          # T3: PoolTemplate field (conditional)
rio-controller/src/reconcilers/
├── workerpool/builders.rs    # T2: env inject + T5: tests
└── workerpoolset/builders.rs # T3: propagate (conditional)
docs/src/
├── observability.md          # T4: remediation text
└── components/controller.md  # T2: new marker
infra/helm/crds/
├── workerpools.rio.build.yaml      # T1: regen
└── workerpoolsets.rio.build.yaml   # T3: regen (conditional)
```

## Dependencies

```json deps
{"deps": [288], "soft_deps": [234, 0374], "note": "T1-T5 depend on P0288 (DONE — bloom_expected_items config.rs field + RIO_BLOOM_EXPECTED_ITEMS env parse exist). Soft-dep P0234 (T3 touches workerpoolset/builders.rs — P0234 may have shifted line refs; non-overlapping, T3 adds one field to the passthrough block). Soft-dep P0374 (also touches workerpoolset/mod.rs, but T3 here touches workerpoolset/builders.rs — different file in same module; no conflict). discovered_from=288-review (rev-p288). HOT file: rio-controller/src/reconcilers/workerpool/builders.rs count=21 — T2+T5 are both additive (env line + 2 test fns), low conflict surface. CRD regen MUST follow field addition (T1/T3 cause schema drift; `just crd-regen` or equivalent)."}
```

**Depends on:** [P0288](plan-0288-bloom-fill-ratio-gauge.md) — `config.rs:131` `bloom_expected_items` field + figment env layering at `RIO_BLOOM_EXPECTED_ITEMS` exist. DONE.
**Conflicts with:** [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=21 — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T99 (POOL_LABEL const at `:59`), T103 (child_name RFC-1123 guard); [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T7 (Unconfined test near `:702`). All additive, non-overlapping sections. [`observability.md`](../../docs/src/observability.md) is multi-touched but T4 is a single sentence edit at `:185`.
