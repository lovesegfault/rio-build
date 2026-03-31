# Plan 991893306: extract `assert_cel_rejects` helper — 3rd copy at threshold, 4th known coming

Consolidator finding (mc=70): [P0512](plan-0512-manifest-reconcile-vm-test.md) landed the 3rd CEL-reject heredoc block at [`lifecycle.nix:2353`](../../nix/tests/scenarios/lifecycle.nix) (manifest-bad-maxbuilds subtest). Three identical-shape blocks now, ~25L each:

| Site | Test | Rule |
|---|---|---|
| [`:2019-2043`](../../nix/tests/scenarios/lifecycle.nix) | ephemeral + maxConcurrentBuilds>1 | `maxConcurrentBuilds==1` |
| [`:2048-2072`](../../nix/tests/scenarios/lifecycle.nix) | ephemeralDeadlineSeconds without ephemeral | `ephemeral:true` gate |
| [`:2353-2377`](../../nix/tests/scenarios/lifecycle.nix) | Manifest + maxConcurrentBuilds>1 | `maxConcurrentBuilds==1` |

Pattern: `k3s_server.fail("kubectl apply --dry-run=server <<EOF\n" + ~14L YAML + "EOF")` + `assert expected_msg in result` + diagnostic f-string pointing at `builderpool.rs`.

**Evidence the 4th is coming:** [`rio-crds/src/builderpool.rs`](../../rio-crds/src/builderpool.rs) has 4 cross-field CEL rules (L73/87/113/128). Three are VM-tested. L113 (`hostNetwork:true → privileged:true`, `r[ctrl.crd.host-users-network-exclusive]` at [`:93`](../../rio-crds/src/builderpool.rs)) is **not** — `nix develop -c tracey query rule ctrl.crd.host-users-network-exclusive` shows `r[impl]` but no `r[verify]`. The 4th block is scheduled implicitly (tracey uncovered will surface it).

**Worth it now:** 3 copies is the extract-helper threshold. `lifecycle.nix` is at **29-plan collision count** (2578 lines); every ~25L chunk removed de-risks the next merge. [P0502](plan-0502-capacity-manifest-cel-constraint.md) just added a CEL rule this sprint — the pattern continues.

No in-flight collision: [P0516](plan-0516-manifest-quota-deadlock.md) touched `manifest.rs` + `manifest_tests.rs` only, [P0517](plan-0517-cov-halt-cascade-overcount.md) touches `.claude/` only. discovered_from=None. origin=consolidator.

## Tasks

### T1 — `refactor(test):` extract `assert_cel_rejects` helper alongside `sched_metric_wait`

[`lifecycle.nix:407-418`](../../nix/tests/scenarios/lifecycle.nix) has `sched_metric_wait` — add sibling at `:~420`:

```python
def assert_cel_rejects(name, spec_body, expected_msg):
    """Apply a deliberately-invalid BuilderPool spec via dry-run=server
    (admission-webhook CEL fires without creating the resource). .fail()
    asserts non-zero exit; the message assert proves it failed at the
    RIGHT rule (not, say, systems=[] or image missing). spec_body is
    the YAML body UNDER `spec:` (6-space indent)."""
    result = k3s_server.fail(
        "k3s kubectl apply --dry-run=server -f - 2>&1 <<'EOF'\n"
        "apiVersion: rio.build/v1alpha1\n"
        "kind: BuilderPool\n"
        f"metadata:\n  name: {name}\n  namespace: ${nsBuilders}\n"
        f"spec:\n{spec_body}\n"
        "EOF"
    )
    assert expected_msg in result, (
        f"CEL should reject {name!r} with {expected_msg!r} in the "
        f"message, got: {result!r}. If the apply succeeded or failed "
        f"for a different reason, the CEL rule at builderpool.rs isn't "
        f"in the deployed CRD — check `helm template | grep x-kubernetes-validations`."
    )
    print(f"{name}: CEL rejected with {expected_msg!r} ✓")
```

**Cross-reference [P0304](plan-0304-trivial-batch-p0222-harness.md)-T527 Route B:** that task documented the 4-site threshold for the FULL-APPLY BuilderPool heredocs (which include valid specs). This is the DIFFERENT, smaller CEL-reject pattern — invalid specs only, ~14L YAML body. The T527 Route-B reasoning ("CEL-reject cases NEED to show the exact invalid YAML") still applies: `spec_body` is a string arg, so the invalid YAML is inline at the call site. The helper just factors the boilerplate (kubectl cmd + apiVersion/kind/metadata + diagnostic f-string).

### T2 — `refactor(test):` delegate 3 existing sites to helper

Replace `:2019-2043`, `:2048-2072`, `:2353-2377` with:

```python
# ── CEL: ephemeral + maxConcurrentBuilds>1 rejected ───────────
# ctrl.pool.ephemeral-single-build — one-pod-per-build isolation
# breaks if a pod runs N builds (shared FUSE cache + overlayfs upper).
assert_cel_rejects(
    "ephemeral-bad-maxbuilds",
    "  ephemeral: true\n"
    "  replicas: {min: 0, max: 4}\n"
    "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
    "  maxConcurrentBuilds: 4\n"
    "  fuseCacheSize: 5Gi\n"
    "  systems: [x86_64-linux]\n"
    "  image: rio-all",
    "maxConcurrentBuilds==1",
)
```

Each site goes from ~25L to ~12L. Keep the per-site divider comment (prose breadcrumb — note `r[...]` marker tokens in these comments are already covered by [P0295](plan-0295-doc-rot-batch-sweep.md)-T497 sweep; use bare rule-id prose).

Net: ~3 × 13L saved = ~39L. lifecycle.nix 2578 → ~2539.

### T3 — `test(vm):` add 4th case — `hostNetwork:true` without `privileged:true`

Close the `r[ctrl.crd.host-users-network-exclusive]` verify gap. CEL at [`builderpool.rs:114`](../../rio-crds/src/builderpool.rs): `!(has(self.hostNetwork) && self.hostNetwork) || (has(self.privileged) && self.privileged)`.

```python
# ── CEL: hostNetwork without privileged rejected ──────────────
# ctrl.crd.host-users-network-exclusive — K8s rejects
# hostUsers:false with hostNetwork:true at admission; the
# non-privileged path sets hostUsers:false (ADR-012).
assert_cel_rejects(
    "hostnet-unprivileged",
    "  hostNetwork: true\n"
    "  replicas: {min: 0, max: 4}\n"
    "  autoscaling: {metric: queueDepth, targetValue: 2}\n"
    "  maxConcurrentBuilds: 1\n"
    "  fuseCacheSize: 5Gi\n"
    "  systems: [x86_64-linux]\n"
    "  image: rio-all",
    "hostNetwork:true requires privileged:true",
)
```

Add `# r[verify ctrl.crd.host-users-network-exclusive]` at the [`default.nix`](../../nix/tests/default.nix) subtests entry (wherever the manifest-pool subtest is wired — per CLAUDE.md VM-test placement rule, marker goes at the wiring not the scenario file).

## Exit criteria

- `/nbr .#ci` green (all 4 CEL rejects pass, including the new hostNetwork one)
- `grep 'def assert_cel_rejects' nix/tests/scenarios/lifecycle.nix` → 1 hit at `:~420`
- `grep -c 'assert_cel_rejects(' nix/tests/scenarios/lifecycle.nix` → ≥4 (3 delegated + 1 new)
- `grep -c 'kubectl apply --dry-run=server' nix/tests/scenarios/lifecycle.nix` → 1 hit (inside the helper; all bare heredocs gone)
- `wc -l nix/tests/scenarios/lifecycle.nix` → ≤2560 (was ~2578; ~39L delegated + ~15L helper - ~12L new case ≈ net -18L minimum)
- `nix develop -c tracey query rule ctrl.crd.host-users-network-exclusive` shows ≥1 `r[verify]` site
- `grep 'r\[verify ctrl.crd.host-users-network-exclusive\]' nix/tests/default.nix` → 1 hit (at subtests wiring, NOT scenario file — per CLAUDE.md placement rule)

## Tracey

References existing markers:
- `r[ctrl.crd.host-users-network-exclusive]` — T3 adds the missing `r[verify]` (currently `r[impl]`-only at [`builderpool.rs:93`](../../rio-crds/src/builderpool.rs)). The marker is at [`controller.md:425`](../../docs/src/components/controller.md).

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: assert_cel_rejects at :~420. T2: delegate :2019/:2048/:2353 (~39L net). T3: 4th case hostNetwork. HOT — 29-plan collision; P0295-T497 touches :2454/:2481/:2498, P0304-T991893302 touches :571/:3013/:3138/:3284/:3433 (all diff sections)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: r[verify ctrl.crd.host-users-network-exclusive] at subtests wiring. P0311-T497 also touches (sec.image.control-plane-minimal marker — diff entry)"}
]
```

```
nix/tests/
├── scenarios/lifecycle.nix  # T1: helper :420; T2: delegate 3 sites; T3: 4th case
└── default.nix              # T3: r[verify] marker at subtests wiring
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [512, 295], "note": "No hard deps — P0512 (soft, discovered_from) added the 3rd copy that tripped the threshold. P0295-T497 (soft) touches lifecycle.nix :2454/:2481/:2498 prose-markers — diff section but same file; land-order doesn't matter."}
```

**Depends on:** none (standalone refactor + test-gap close).

**Conflicts with:** `lifecycle.nix` is the HOTTEST file (29-plan collision). This plan's edits are at `:~420` (new helper), `:2019/:2048/:2353` (delegation), `:~2380` (new case). [P0295](plan-0295-doc-rot-batch-sweep.md)-T497 touches `:2454/:2481/:2498` — just below T2's `:2353` site; possible merge-proximity conflict on the `:2353` delegation. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T991893302 touches `:571/:3013/:3138/:3284/:3433` — well clear.
