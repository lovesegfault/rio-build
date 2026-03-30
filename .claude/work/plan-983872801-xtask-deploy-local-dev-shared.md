# Plan 983872801: CONSOLIDATION — extract shared::deploy_local_dev from kind/k3s deploy()

[P0477](plan-0477-xtask-deploy-ux-autoconfig.md) shipped three dual-edits
across `kind/mod.rs::deploy` and `k3s/mod.rs::deploy` — JWT keypair
([`a84e5610`](https://github.com/search?q=a84e5610&type=commits)), SSH
tenant ([`0b93888b`](https://github.com/search?q=0b93888b&type=commits)),
and rollout-restart
([`ab729af8`](https://github.com/search?q=ab729af8&type=commits)). Each edit
touched both files identically; each bumped both `DEPLOY_STEPS` constants.
Diffing [`kind/mod.rs:123-180`](../../xtask/src/k8s/kind/mod.rs) against
[`k3s/mod.rs:83-135`](../../xtask/src/k8s/k3s/mod.rs) shows ~90% textual
overlap: chart-deps → CRDs → namespaces+ssh-secret → pg-secret →
jwt-keypair → helm-install → conditional rollout-restart.

The diverging 10%: kind passes `values/kind.yaml` + `.wait(300s)` on helm,
then chains a RustFS wait + bucket-create tail. k3s passes `values/dev.yaml`
with no helm-wait, no tail. EKS (not touched here) has a substantially
different deploy path (ECR push, IRSA, no local image load) and stays
separate.

Extraction eliminates the dual-maintenance burden flagged by consolidator:
next auth/secret/restart change is one edit, one `DEPLOY_STEPS` bump.

## Tasks

### T1 — `refactor(xtask):` extract shared::deploy_local_dev

NEW fn at [`xtask/src/k8s/shared.rs`](../../xtask/src/k8s/shared.rs):

```rust
pub struct LocalDeployOpts<'a> {
    pub values_file: &'a str,
    pub helm_wait: Option<Duration>,
}

/// Shared kind/k3s deploy sequence: chart-deps → CRDs → namespaces →
/// ssh-secret → pg-secret → jwt-keypair → helm install → conditional
/// rollout-restart. Returns the kube client so callers can chain
/// provider-specific tail steps (kind: RustFS wait + bucket create).
pub async fn deploy_local_dev(
    cfg: &XtaskConfig,
    log_level: &str,
    tenant: Option<&str>,
    opts: LocalDeployOpts<'_>,
) -> Result<kube::Client> { ... }
```

Body is the common 7-step sequence lifted from either existing `deploy()`.
The `helm_wait` option conditionally adds `.wait(d)` to the
`helm::Helm::upgrade_install` builder chain.

### T2 — `refactor(xtask):` kind/k3s deploy() → thin wrappers

MODIFY [`xtask/src/k8s/kind/mod.rs:123`](../../xtask/src/k8s/kind/mod.rs)
and [`xtask/src/k8s/k3s/mod.rs:83`](../../xtask/src/k8s/k3s/mod.rs):

```rust
// k3s
async fn deploy(&self, cfg: &XtaskConfig, log_level: &str, tenant: Option<&str>) -> Result<()> {
    shared::deploy_local_dev(cfg, log_level, tenant, shared::LocalDeployOpts {
        values_file: "infra/helm/rio-build/values/dev.yaml",
        helm_wait: None,
    }).await.map(|_| ())
}

// kind — keeps the RustFS tail
async fn deploy(&self, cfg: &XtaskConfig, log_level: &str, tenant: Option<&str>) -> Result<()> {
    let client = shared::deploy_local_dev(cfg, log_level, tenant, shared::LocalDeployOpts {
        values_file: "infra/helm/rio-build/values/kind.yaml",
        helm_wait: Some(Duration::from_secs(300)),
    }).await?;
    kube::wait_rollout(&client, NS, "rio-rustfs", Duration::from_secs(120)).await?;
    ui::step("create rio-chunks bucket", || create_bucket(&client)).await
}
```

### T3 — `refactor(xtask):` consolidate DEPLOY_STEPS

Move `DEPLOY_STEPS` into `shared.rs` as `pub const LOCAL_DEPLOY_STEPS: u64
= 7`. kind keeps `const DEPLOY_STEPS: u64 = shared::LOCAL_DEPLOY_STEPS +
2` (RustFS wait + bucket). k3s uses `shared::LOCAL_DEPLOY_STEPS` directly.
Next auth/secret change bumps one constant.

## Exit criteria

- `nix develop -c cargo build -p xtask` green
- `diff <(grep -A60 'fn deploy' xtask/src/k8s/kind/mod.rs) <(grep -A60 'fn deploy' xtask/src/k8s/k3s/mod.rs)` — bodies ≤15 lines each (was ~55)
- `grep 'DEPLOY_STEPS' xtask/src/k8s/ -r` — one canonical const in shared.rs, kind/k3s derive from it
- Manual: `cargo xtask k8s k3s deploy` and `cargo xtask k8s kind deploy` both succeed on a fresh cluster (smoke equivalent — no VM test for xtask dev-loop)

## Tracey

No domain markers. xtask is developer tooling outside the component spec
surface (`docs/src/components/` covers gateway/scheduler/store/etc., not
the local-dev deploy loop). `tracey query uncovered` correctly ignores
this plan.

## Files

```json files
[
  {"path": "xtask/src/k8s/shared.rs", "action": "MODIFY", "note": "T1+T3: new deploy_local_dev fn + LOCAL_DEPLOY_STEPS const"},
  {"path": "xtask/src/k8s/kind/mod.rs", "action": "MODIFY", "note": "T2+T3: deploy() → thin wrapper, DEPLOY_STEPS derives from shared"},
  {"path": "xtask/src/k8s/k3s/mod.rs", "action": "MODIFY", "note": "T2+T3: deploy() → thin wrapper, uses shared::LOCAL_DEPLOY_STEPS"}
]
```

```
xtask/src/k8s/
├── shared.rs         # T1: deploy_local_dev() + LocalDeployOpts + LOCAL_DEPLOY_STEPS
├── kind/mod.rs       # T2: deploy() → wrapper + RustFS tail
└── k3s/mod.rs        # T2: deploy() → wrapper
```

## Dependencies

```json deps
{"deps": [477], "soft_deps": [], "note": "P0477 DONE — it created the dual-maintenance burden this plan resolves"}
```

**Depends on:** [P0477](plan-0477-xtask-deploy-ux-autoconfig.md) (DONE) —
the three dual-edits this plan consolidates. Already merged; this is a
post-hoc refactor, no live-ordering constraint.

**Conflicts with:** none — xtask/src/k8s/ files not in collisions top-20.
No other UNIMPL plan touches these paths.
