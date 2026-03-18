# Plan 0223: seccomp Localhost profile + WorkerPoolSpec field

phase4c.md:59 — close the Phase-4 seccomp tracking items at [`security.md:53`](../../docs/src/security.md), [`:173`](../../docs/src/security.md), [`:175`](../../docs/src/security.md). The RuntimeDefault profile (shipped in Phase 3b) blocks ~40 syscalls but leaves `ptrace`, `bpf`, `setns`, `process_vm_{read,write}v` available under `CAP_SYS_ADMIN`. A sandbox escape gets those syscalls; a Localhost profile that denies them reduces the attacker's toolkit.

**Per Q1 default: Localhost profile is PRODUCTION-ONLY.** The k3s VM test nodes may not have `/var/lib/kubelet/seccomp/` populated, and adding an init-container + hostPath ConfigMap mount is ~20 lines of complexity for a VM-only verify. Instead: the profile JSON ships in the repo, the builder emits the correct `securityContext`, and the **unit test** verifies the builder output. VM tests continue using `RuntimeDefault`. Document this explicitly.

**CRD regen note:** this plan modifies `WorkerPoolSpec`. [P0235](plan-0235-wps-main-wire-rbac-crd-regen.md) also regenerates CRDs. Whoever merges second re-runs `nix-build-remote -- .#crds && ./scripts/split-crds.sh result`. Both plan docs carry this note.

## Tasks

### T1 — `feat(infra):` seccomp-rio-worker.json profile

NEW `infra/helm/rio-build/files/seccomp-rio-worker.json` (creates the `files/` directory):

```json
{
  "defaultAction": "SCMP_ACT_ALLOW",
  "architectures": ["SCMP_ARCH_X86_64", "SCMP_ARCH_AARCH64"],
  "syscalls": [
    {
      "names": ["ptrace", "bpf", "setns", "process_vm_readv", "process_vm_writev"],
      "action": "SCMP_ACT_ERRNO",
      "errnoRet": 1
    }
  ]
}
```

This is a **denylist on top of RuntimeDefault** — `defaultAction: ALLOW` because we layer on what RuntimeDefault already blocks. The five syscalls chosen:
- `ptrace` — debugger attach; lets an escapee inspect/modify other worker processes
- `bpf` — load BPF programs; kernel-side code execution
- `setns` — join another namespace; defeats namespace isolation
- `process_vm_readv`/`writev` — cross-process memory access without ptrace

Rationale comment in the JSON header explaining why each syscall is blocked + pointing to security.md.

### T2 — `feat(controller):` seccomp_profile field on WorkerPoolSpec

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — around the existing seccomp comment near `:180`:

```rust
/// Seccomp profile kind. Default: RuntimeDefault (blocks ~40 syscalls).
/// Localhost requires the profile JSON at /var/lib/kubelet/seccomp/<path>
/// on every node — production EKS/GKE only; VM test fixtures use
/// RuntimeDefault. See docs/src/security.md#seccomp.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub seccomp_profile: Option<SeccompProfileKind>,

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SeccompProfileKind {
    RuntimeDefault,
    Localhost { localhost_profile: String },  // path under /var/lib/kubelet/seccomp/
    Unconfined,  // NOT recommended; for debugging only
}
```

### T3 — `feat(controller):` conditionalize builders.rs securityContext

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) near `:259` where the pod spec's `security_context` is constructed:

```rust
// r[impl worker.seccomp.localhost-profile]
let seccomp = match &spec.seccomp_profile {
    Some(SeccompProfileKind::Localhost { localhost_profile }) => json!({
        "type": "Localhost",
        "localhostProfile": localhost_profile,
    }),
    Some(SeccompProfileKind::Unconfined) => json!({"type": "Unconfined"}),
    // Default: RuntimeDefault (None case + explicit RuntimeDefault)
    _ => json!({"type": "RuntimeDefault"}),
};
```

The existing builder likely sets `RuntimeDefault` unconditionally — this makes it conditional on the spec field.

### T4 — `test(controller):` unit test for builder output + profile JSON parse

NEW test in `rio-controller/src/reconcilers/workerpool/builders.rs` test module:

```rust
// r[verify worker.seccomp.localhost-profile]
#[test]
fn seccomp_localhost_emits_correct_security_context() {
    let spec = WorkerPoolSpec {
        seccomp_profile: Some(SeccompProfileKind::Localhost {
            localhost_profile: "rio-worker.json".into(),
        }),
        ..test_spec()
    };
    let sts = build_statefulset(&spec, /*...*/);
    let sc = &sts.spec.template.spec.security_context;
    assert_eq!(sc["seccompProfile"]["type"], "Localhost");
    assert_eq!(sc["seccompProfile"]["localhostProfile"], "rio-worker.json");
}

#[test]
fn seccomp_default_is_runtime_default() {
    let spec = WorkerPoolSpec { seccomp_profile: None, ..test_spec() };
    let sts = build_statefulset(&spec, /*...*/);
    assert_eq!(sts.spec.template.spec.security_context["seccompProfile"]["type"], "RuntimeDefault");
}

#[test]
fn seccomp_profile_json_is_valid() {
    let json = include_str!("../../../../infra/helm/rio-build/files/seccomp-rio-worker.json");
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["defaultAction"], "SCMP_ACT_ALLOW");
    let blocked = &parsed["syscalls"][0]["names"];
    assert!(blocked.as_array().unwrap().iter().any(|s| s == "ptrace"));
}
```

### T5 — `docs:` close security.md deferrals + spec ¶

MODIFY [`docs/src/security.md`](../../docs/src/security.md):

1. Add `r[worker.seccomp.localhost-profile]` spec paragraph near the existing seccomp blockquote at `:53`:

```markdown
r[worker.seccomp.localhost-profile]
Worker pods MAY be configured with a Localhost seccomp profile (`WorkerPoolSpec.seccompProfile: Localhost`) that denies `ptrace`, `bpf`, `setns`, `process_vm_readv`, `process_vm_writev` on top of RuntimeDefault's ~40-syscall denylist. The profile JSON lives at `infra/helm/rio-build/files/seccomp-rio-worker.json` and MUST be installed at `/var/lib/kubelet/seccomp/rio-worker.json` on every node before the WorkerPool is applied (node-level install is outside rio-controller's scope — use a DaemonSet or node-prep script). **VM test fixtures use RuntimeDefault**; Localhost is production-only. Default remains RuntimeDefault.
```

2. Close the three deferral mentions: the `:53` blockquote's "CUSTOM profile... tracked for Phase 4" → "shipped in Phase 4c; see `r[worker.seccomp.localhost-profile]`"; the `:173` "custom seccomp profile... Phase 4 enhancement" → update to past tense; the `:175` "custom profile tracked for Phase 4" → same.

### T6 — `feat(controller):` CRD regen

Run `nix-build-remote --no-nom --dev -- .#crds && ./scripts/split-crds.sh result` and commit the updated `infra/helm/crds/workerpool.yaml`. **On rebase against P0235:** re-run this command — the regen is deterministic and will pick up both the seccomp field (this plan) and the WPS CRD (P0232/P0235).

## Exit criteria

- `/nbr .#ci` green
- `tracey query rule worker.seccomp.localhost-profile` shows spec + impl + verify
- `seccomp_profile_json_is_valid` test passes (profile parses, contains `ptrace`)
- `seccomp_localhost_emits_correct_security_context` test passes
- security.md no longer contains "custom profile... Phase 4" forward-references

## Tracey

Adds new marker to component specs:
- `r[worker.seccomp.localhost-profile]` → [`docs/src/security.md`](../../docs/src/security.md) (see ## Spec additions below)

## Spec additions

**`r[worker.seccomp.localhost-profile]`** — placed in `docs/src/security.md` near the existing seccomp blockquote (`:53`), standalone paragraph, blank line before, col 0:

```
r[worker.seccomp.localhost-profile]
Worker pods MAY be configured with a Localhost seccomp profile (`WorkerPoolSpec.seccompProfile: Localhost`) that denies `ptrace`, `bpf`, `setns`, `process_vm_readv`, `process_vm_writev` on top of RuntimeDefault's ~40-syscall denylist. The profile JSON lives at `infra/helm/rio-build/files/seccomp-rio-worker.json` and MUST be installed at `/var/lib/kubelet/seccomp/rio-worker.json` on every node before the WorkerPool is applied (node-level install is outside rio-controller's scope — use a DaemonSet or node-prep script). VM test fixtures use RuntimeDefault; Localhost is production-only. Default remains RuntimeDefault.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/files/seccomp-rio-worker.json", "action": "NEW", "note": "T1: Localhost profile denying ptrace/bpf/setns/process_vm_*"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T2: seccomp_profile: Option<SeccompProfileKind> field near :180"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T3+T4: conditionalize securityContext near :259; r[impl]+r[verify] tests"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T5: r[worker.seccomp.localhost-profile] spec ¶ + close 3 deferrals at :53,:173,:175"},
  {"path": "infra/helm/crds/workerpool.yaml", "action": "MODIFY", "note": "T6: CRD regen (seccomp_profile schema field)"}
]
```

```
infra/helm/rio-build/files/           # NEW directory
└── seccomp-rio-worker.json           # T1
rio-controller/src/
├── crds/workerpool.rs                # T2: seccomp_profile field
└── reconcilers/workerpool/builders.rs # T3+T4: r[impl]+r[verify]
docs/src/security.md                  # T5: spec ¶ + close deferrals
infra/helm/crds/workerpool.yaml       # T6: regen
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [235], "note": "Wave-1 frontier. WPS spine head — P0232 deps this (PoolTemplate inherits seccomp_profile). Soft conflict with P0235 on CRD regen yaml — whoever merges second re-runs regen."}
```

**Depends on:** none.
**Conflicts with:** `crds/workerpool.rs` — [P0232](plan-0232-wps-crd-struct-crdgen.md) deps on this plan so PoolTemplate inherits `seccomp_profile` consciously. CRD regen yaml: soft conflict with P0235; deterministic regen, whoever merges second re-runs.
