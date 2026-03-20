# Plan 0359: hostUsers:false + hostNetwork:true K8s admission guardrail

[P0286](plan-0286-privileged-hardening-device-plugin.md) post-PASS review. P0286 T2 adds `hostUsers: Some(false)` to the worker pod spec, gated only on `wp.spec.privileged != Some(true)`. But the CRD at [`rio-crds/src/workerpool.rs:248`](../../rio-crds/src/workerpool.rs) independently exposes `host_network: Option<bool>`. Kubernetes **rejects at admission** any pod spec with `hostUsers: false` + `hostNetwork: true` — the user-namespace remapping is incompatible with sharing the node's network namespace (kubelet returns `hostUsers=false is not allowed when hostNetwork is set`).

P0286's gate at [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) (the review cited `:283`; actual post-P0286 line will shift — grep for `host_users` at dispatch) checks `!privileged` but NOT `!host_network`. An operator who sets `WorkerPool.spec.hostNetwork: true` (valid per the CRD, documented at [`controller.md:34`](../../docs/src/components/controller.md) as "VM-test/airgap escape") with the new `privileged: false` default will get a StatefulSet that renders a rejected pod spec. The reconcile loop retries indefinitely; the operator sees `0/N Ready` with no rio-controller-surfaced reason.

**Latent today** — all shipped values files set `hostNetwork: false` ([`vmtest-full.yaml:63`](../../infra/helm/rio-build/values/vmtest-full.yaml)). The CRD contract is still broken: the schema accepts a combination the pod template can never satisfy.

**Two-layer fix:** (a) CRD-level CEL validation rejects the combination at `kubectl apply` time (fail-fast, clear message); (b) builder-level gate suppresses `hostUsers: false` when `host_network == Some(true)` (defense-in-depth for CRDs that pre-date the CEL rule). Layer (a) alone is insufficient because existing WorkerPools aren't re-validated; layer (b) alone gives silent degrade (hostUsers dropped without operator visibility). Both together: new WorkerPools get a clear error; old ones keep working with a logged degrade.

## Entry criteria

- [P0286](plan-0286-privileged-hardening-device-plugin.md) merged — `hostUsers: Some(false)` exists in `build_pod_spec`; `privileged: false` is the values.yaml default; `r[sec.pod.host-users-false]` marker exists in [`security.md`](../../docs/src/security.md)

## Tasks

### T1 — `fix(crds):` CEL rule rejects hostNetwork:true with privileged:false

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs). Add at struct-level (after the existing `#[x_kube(validation = "!self.ephemeral || ...")]` at `:50`):

```rust
// r[impl ctrl.crd.host-users-network-exclusive]
#[x_kube(
    validation = "!(has(self.hostNetwork) && self.hostNetwork) || (has(self.privileged) && self.privileged)",
    message = "hostNetwork:true requires privileged:true — Kubernetes rejects hostUsers:false with hostNetwork:true at admission; the non-privileged path sets hostUsers:false (see ADR-012)"
)]
```

The rule reads: "IF hostNetwork is true THEN privileged MUST be true". Equivalently: `hostNetwork → privileged`. This fails `kubectl apply` with the explicit message instead of a silent Pending pod.

**CRD regen:** After the struct edit, regenerate [`infra/helm/crds/workerpools.rio.build.yaml`](../../infra/helm/crds/workerpools.rio.build.yaml) — `just crds` or `nix build .#crds && cp result/*.yaml infra/helm/crds/`. The new `x-kubernetes-validations` entry must appear in the schema.

Also extend [`cel_rules_in_schema`](../../rio-crds/src/workerpool.rs) test at `:467` — add the new rule string to the expected set.

### T2 — `fix(controller):` builder-level gate suppresses hostUsers when hostNetwork

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) at the P0286-added `host_users` assignment (grep `host_users` at dispatch — P0286 inserts it near the `host_network` / `dns_policy` block around `:243-255`).

Current (P0286):
```rust
host_users: if wp.spec.privileged != Some(true) {
    Some(false)
} else {
    None
},
```

Revised:
```rust
// r[impl sec.pod.host-users-false]  (re-annotates P0286's marker here)
// K8s admission rejects hostUsers:false + hostNetwork:true (userns
// remap is incompatible with host netns). The CRD CEL rule catches
// NEW specs; this guard handles OLD specs that predate the rule.
// When hostNetwork is set, skip hostUsers — the operator has chosen
// the escape hatch (and should also set privileged:true per the CEL
// rule; if they didn't, this avoids a stuck-Pending StatefulSet).
host_users: if wp.spec.privileged != Some(true)
    && wp.spec.host_network != Some(true)
{
    Some(false)
} else {
    None
},
```

Emit a reconcile event when the suppression fires (operator visibility for the silent degrade):

```rust
if wp.spec.host_network == Some(true) && wp.spec.privileged != Some(true) {
    recorder.publish(Event {
        type_: EventType::Warning,
        reason: "HostUsersSuppressedForHostNetwork".into(),
        note: Some("hostNetwork:true forces hostUsers omitted (K8s admission). Set privileged:true explicitly, or drop hostNetwork.".into()),
        action: "Reconcile".into(),
        secondary: None,
    }).await?;
}
```

Place the event-emit in `reconcile()` before `build_pod_spec` is called — `build_pod_spec` is a pure fn; event emission needs the `Recorder`.

### T3 — `test(controller):` regression tests — both layers

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) tests module (grep `#[cfg(test)]` — existing tests for `build_pod_spec`):

```rust
#[test]
fn host_users_suppressed_when_host_network() {
    // r[verify sec.pod.host-users-false]
    let mut wp = minimal_wp();
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None; // default — non-privileged path
    let spec = build_pod_spec(&wp, &addrs(), "store:9000", 50, q("50Gi"));
    assert_eq!(spec.host_users, None,
        "hostUsers must be suppressed when hostNetwork:true (K8s admission rejects the combo)");
    assert_eq!(spec.host_network, Some(true));
}

#[test]
fn host_users_false_when_neither_escape_hatch() {
    let mut wp = minimal_wp();
    wp.spec.host_network = None;
    wp.spec.privileged = None;
    let spec = build_pod_spec(&wp, &addrs(), "store:9000", 50, q("50Gi"));
    assert_eq!(spec.host_users, Some(false),
        "default path (no escape hatch) must set hostUsers:false");
}
```

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) `cel_rules_in_schema` test — add assertion that the new validation rule string appears in the generated schema YAML.

The CEL rule itself is exercised by the apiserver at admission; a unit test can't prove it fires. **Optional** (not exit-gated): VM-test fragment in [`security.nix`](../../nix/tests/scenarios/security.nix) that `kubectl apply`s a WorkerPool with `hostNetwork: true, privileged: false` and asserts the apply is rejected with the CEL message. Defer to [P0360](plan-0360-device-plugin-vm-coverage.md)'s security.nix touch if both dispatch together.

### T4 — `docs(crds):` hostNetwork doc-comment notes the constraint

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) at the `host_network` field doc-comment (`:233-246`). Append after the "Caveat:" paragraph:

```rust
/// Constraint: `hostNetwork: true` requires `privileged: true`.
/// Kubernetes rejects `hostUsers: false` + `hostNetwork: true` at
/// admission (user-namespace remap is incompatible with host netns).
/// The non-privileged path sets `hostUsers: false` (ADR-012), so
/// hostNetwork implies the privileged escape hatch. CRD CEL
/// validation enforces this; see
/// `r[ctrl.crd.host-users-network-exclusive]`.
```

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) at `:34` (the `hostNetwork: false` comment line in the CRD YAML example) — extend:

```yaml
  hostNetwork: false                           # optional — true only for VM-test/airgap escapes; requires privileged:true (hostUsers:false incompatible with host netns)
```

And at `:347-352` (CRD Validation section), add the new rule to the bullet list:

```markdown
- `spec.hostNetwork: true` requires `spec.privileged: true` — Kubernetes rejects `hostUsers: false` combined with `hostNetwork: true` at pod admission; the non-privileged path sets `hostUsers: false` per ADR-012
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'host_users\|hostUsers' rio-controller/src/reconcilers/workerpool/builders.rs | grep 'host_network\|hostNetwork'` → ≥1 hit (T2: gate checks both)
- `grep 'hostNetwork.*privileged\|host-users-network-exclusive' rio-crds/src/workerpool.rs` → ≥2 hits (T1: CEL rule + T4: doc-comment)
- `grep 'hostNetwork.*requires.*privileged\|host-users-network-exclusive' infra/helm/crds/workerpools.rio.build.yaml` → ≥1 hit (T1: regen picked up the rule)
- `cargo nextest run -p rio-controller host_users_suppressed_when_host_network` → pass (T3)
- `cargo nextest run -p rio-controller host_users_false_when_neither_escape_hatch` → pass (T3: positive control — suppression doesn't over-fire)
- `cargo nextest run -p rio-crds cel_rules_in_schema` → pass with new rule asserted (T3)
- `nix develop -c tracey query rule ctrl.crd.host-users-network-exclusive` → non-empty rule text (spec marker parsed)
- **Mutation:** delete `&& wp.spec.host_network != Some(true)` → `host_users_suppressed_when_host_network` FAILS

## Tracey

References existing markers:
- `r[sec.pod.host-users-false]` — T2 re-annotates the gate (P0286 added this marker); T3 adds a second `r[verify]` site
- `r[ctrl.crd.workerpool]` — T1+T4 extend the CRD contract documented under this marker

Adds new markers to component specs:
- `r[ctrl.crd.host-users-network-exclusive]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions)

## Spec additions

New marker in [`docs/src/components/controller.md`](../../docs/src/components/controller.md), inserted after the CRD Validation bullet list (`:352`), standalone paragraph, blank line before, col 0:

```
r[ctrl.crd.host-users-network-exclusive]
The controller MUST reject `WorkerPool` specs with `hostNetwork: true` and `privileged` unset or false. Kubernetes admission rejects pod specs combining `hostUsers: false` with `hostNetwork: true` (user-namespace UID remapping is incompatible with the host network namespace). Since the non-privileged path sets `hostUsers: false` unconditionally (ADR-012, `r[sec.pod.host-users-false]`), `hostNetwork: true` implies the `privileged: true` escape hatch. CRD CEL validation enforces this at `kubectl apply` time; the builder additionally suppresses `hostUsers` when the combination is encountered in pre-existing specs (emitting a Warning event).
```

## Files

```json files
[
  {"path": "rio-crds/src/workerpool.rs", "action": "MODIFY", "note": "T1: +struct-level CEL validation hostNetwork→privileged after :50; T3: +cel_rules_in_schema assertion; T4: host_network doc-comment :246 +constraint note"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "T1: regen — new x-kubernetes-validations entry"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T2: host_users gate +host_network check; T3: +2 unit tests (suppressed + positive-control)"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T2: reconcile() emits Warning event when hostNetwork+!privileged (before build_pod_spec call)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: :34 hostNetwork comment + :352 CEL bullet; Spec additions: +r[ctrl.crd.host-users-network-exclusive] after :352"}
]
```

```
rio-crds/src/
└── workerpool.rs                              # T1: CEL rule; T3: test; T4: doc
infra/helm/crds/
└── workerpools.rio.build.yaml                 # T1: regen
rio-controller/src/reconcilers/workerpool/
├── builders.rs                                # T2: gate + T3: tests
└── mod.rs                                     # T2: event emit
docs/src/components/
└── controller.md                              # T4 + spec marker
```

## Dependencies

```json deps
{"deps": [286], "soft_deps": [0360], "note": "rev-p286 correctness finding. P0286 T2 adds host_users:Some(false) gated on !privileged — gate misses hostNetwork. K8s admission rejects hostUsers:false+hostNetwork:true. LATENT (all shipped values hostNetwork:false) but CRD contract broken. Two-layer: CEL at apply-time + builder suppress for pre-CEL specs. discovered_from=286. builders.rs HOT count=19 — T2 is 3-line gate extension + 2 test fns, additive at a single block. controller.md count=17 — :34 + :352 only, both pure-add. Soft-dep P0360 (device-plugin VM coverage) — if both dispatch together, P0360's security.nix fragment can include the CEL-reject kubectl-apply probe (optional T3 extension); sequence-independent otherwise."}
```

**Depends on:** [P0286](plan-0286-privileged-hardening-device-plugin.md) — `hostUsers: Some(false)` in builders.rs, `r[sec.pod.host-users-false]` marker, `privileged: false` default.

**Soft-dep:** [P0360](plan-0360-device-plugin-vm-coverage.md) — shares the security.nix VM touch; optional CEL-reject fragment can piggyback.

**Conflicts with:** [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=19 — HOT. T2 edits the P0286-inserted `host_users` block (~3 lines) and adds 2 tests. P0286 itself makes the largest edit (adds the block T2 modifies); sequence P0286 → this plan by hard-dep. [P0285](plan-0285-drainworker-disruptiontarget-watcher.md) touches PDB docstring (different section). [P0296](plan-0296-ephemeral-builders-opt-in.md) adds a new fn (different section). [`workerpool.rs`](../../rio-crds/src/workerpool.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T6 adds seccomp asserts to `cel_rules_in_schema` (same test fn T3 extends; both add assertions, rebase-clean). [`controller.md`](../../docs/src/components/controller.md) count=17 — [P0304-T73](plan-0304-trivial-batch-p0222-harness.md) blank-line sweep (mechanical, non-overlapping); [P0295-T39](plan-0295-doc-rot-batch-sweep.md) edits `r[ctrl.pool.ephemeral]` paragraph (different section).
