# Plan 0454: Four-namespace helm split — per-role netpol, fetcher NodePool, xtask multi-ns

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the single `rio-system` namespace (PSA `privileged` everywhere) into four: `rio-system` + `rio-store` at `baseline`, `rio-builders` + `rio-fetchers` at `privileged`. The privileged label narrows to the two namespaces that actually need `CAP_SYS_ADMIN` for FUSE; control plane drops to baseline.

The Squid `fod-proxy` is deleted. Builders lose ALL internet egress (airgapped: DNS + scheduler + store only). Fetchers get `0.0.0.0/0:80,443` minus RFC1918/link-local — the FOD hash check at `verify_fod_hashes()` is the integrity boundary, a domain allowlist is operational friction for marginal gain. A dedicated `rio-fetcher` Karpenter NodePool with `rio.build/fetcher=true:NoSchedule` taint keeps fetchers off builder nodes and vice versa — an escaped fetcher lands on a node running only other fetchers.

This plan is the **infra half** of the split. [P0451](plan-0451-builderpool-fetcherpool-crds.md) landed the `BuilderPool`/`FetcherPool` CRDs; [P0453](plan-0453-scheduler-executor-kind-routing.md) landed the scheduler routing. Here: helm renders four namespaces, NetworkPolicies go per-namespace with `namespaceSelector` cross-rules, xtask creates all four before `helm upgrade`, and `xtask k8s status` lists across them.

## Entry criteria

- [P0451](plan-0451-builderpool-fetcherpool-crds.md) merged — `BuilderPool`/`FetcherPool` CRDs exist, reconcilers know which namespace to create STS in
- [P0453](plan-0453-scheduler-executor-kind-routing.md) merged — scheduler `hard_filter()` routes FODs to fetchers; `ExecutorKind` is in heartbeat

## Tasks

### T1 — `feat(helm):` namespace.yaml — four namespaces, split PSA

Replace the single-namespace template at [`templates/namespace.yaml`](../../infra/helm/rio-build/templates/namespace.yaml) with a range over four:

| Name | PSA `pod-security.kubernetes.io/enforce` | Label `kubernetes.io/metadata.name` |
|---|---|---|
| `rio-system` | `baseline` | (auto) |
| `rio-store` | `baseline` | (auto) |
| `rio-builders` | `privileged` | (auto) |
| `rio-fetchers` | `privileged` | (auto) |

Values shape: `.Values.namespaces` list of `{name, psa}` objects (or a map keyed by role — pick whichever keeps the template flatter). Keep `.Values.namespace.create` gate so xtask can set `namespace.create=false` when it pre-creates them (same pattern as today at [`eks/deploy.rs:86`](../../xtask/src/k8s/eks/deploy.rs)).

All four get `app.kubernetes.io/part-of: rio-build` so the NetworkPolicy `namespaceSelector` rules in T2 can match by that label instead of enumerating names.

### T2 — `feat(helm):` networkpolicy.yaml — per-namespace split with namespaceSelector cross-rules

Rewrite [`templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml). Current single-ns `rio-worker-egress` (`:16`) with its `fod-proxy:3128` rule (`:46-49`) goes away. New policies, one per target namespace:

**`builder-egress`** (in `rio-builders`) — `# r[impl builder.netpol.airgap]`
- `podSelector: {matchLabels: {rio.build/role: builder}}` (reconciler-set per P0451)
- Egress: CoreDNS:53/UDP (kube-system via `namespaceSelector`), scheduler:9001 (rio-system via `namespaceSelector` + `podSelector`), store:9002 (rio-store via `namespaceSelector` + `podSelector`)
- Conditional S3 VPC endpoint CIDR if `.Values.builder.s3Direct` (wrap in `{{- if }}`)
- NO `0.0.0.0/0`. NO fod-proxy. Metadata block is implicit (no rule = deny).

**`fetcher-egress`** (in `rio-fetchers`) — `# r[impl fetcher.netpol.egress-open]`
- Same three as builder-egress, PLUS:
- `ipBlock: {cidr: 0.0.0.0/0, except: [10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 127.0.0.0/8]}` on ports 80,443
- The `169.254.0.0/16` except-entry blocks IMDS (`169.254.169.254`) — per ADR-019 "metadata-service block is inherited from the link-local deny"

**`store-ingress`** (in `rio-store`)
- `podSelector: {matchLabels: {app.kubernetes.io/name: rio-store}}`
- Ingress from: `namespaceSelector` matching `rio-system`, `rio-builders`, `rio-fetchers` (three `from:` blocks, or one `from:` with three `namespaceSelector` entries) on port 9002

**`scheduler-ingress`** (in `rio-system`)
- Ingress from: `namespaceSelector` matching `rio-builders`, `rio-fetchers` on port 9001
- Keep existing gateway→scheduler same-ns rule

Keep `rio-controller-egress` (`:58-79`) — controller still needs apiserver reach, but now additionally needs to watch/patch STS in `rio-builders` and `rio-fetchers` namespaces. The RFC1918 allow already covers that (apiserver is the same endpoint regardless of which namespace you're CRUDing).

Delete the `fod-proxy` ingress/egress rules entirely.

### T3 — `feat(helm):` store.yaml — move to rio-store namespace

[`templates/store.yaml`](../../infra/helm/rio-build/templates/store.yaml) — set `metadata.namespace: rio-store` on the Deployment, Service, and any ConfigMap/Secret that `store.yaml` renders. Service DNS name becomes `rio-store.rio-store.svc` — grep for hardcoded `rio-store.rio-system` in other templates and configmaps, update.

### T4 — `feat(helm):` values.yaml — fetcher NodePool, builder rename, delete fodProxy

At [`values.yaml`](../../infra/helm/rio-build/values.yaml):

1. **Delete** `fodProxy:` section (`:246-`).
2. **Rename** Karpenter `nodePools` entries `rio-worker-preferred` (`:307`), `rio-worker-fallback` (`:329`), and any third `rio-worker-*` → `rio-builder-preferred`, `rio-builder-fallback`, etc. Update `taints` to `rio.build/builder=true:NoSchedule` + labels `rio.build/node-role: builder`.
3. **Add** a new `rio-fetcher` NodePool entry — `# r[impl fetcher.node.dedicated]`:
   - `instanceTypes: [t3a.medium, t3a.large]` (network-bound, not CPU — cheap burstables)
   - `taints: [{key: rio.build/fetcher, value: "true", effect: NoSchedule}]`
   - `labels: {rio.build/node-role: fetcher}`
   - Low weight so Karpenter doesn't prefer it for unrelated pods
4. Update `.Values.namespace` → `.Values.namespaces` per T1 shape.
5. Update `workerPool:` section (`:549-`) — image rename already done by P0451; verify `tolerations`/`nodeSelector` reference the new `rio.build/builder` taint+label.

Delete [`templates/fod-proxy.yaml`](../../infra/helm/rio-build/templates/fod-proxy.yaml) entirely.

### T5 — `feat(xtask):` k8s providers — ensure_namespace ×4, deploy multi-ns aware

At [`xtask/src/k8s/mod.rs:54`](../../xtask/src/k8s/mod.rs): replace `pub const NS: &str = "rio-system"` with four consts (or a `pub const NAMESPACES: &[(&str, bool)] = &[("rio-system", false), ("rio-store", false), ("rio-builders", true), ("rio-fetchers", true)]` where bool is `privileged`). Keep `NS` as an alias for `rio-system` to avoid touching every call site that means "the control-plane ns" — only multi-ns-aware code iterates `NAMESPACES`.

At each provider's deploy path ([`eks/deploy.rs:60`](../../xtask/src/k8s/eks/deploy.rs), [`kind/mod.rs:111`](../../xtask/src/k8s/kind/mod.rs), [`k3s/mod.rs:90`](../../xtask/src/k8s/k3s/mod.rs)): replace the single `kube::ensure_namespace(&client, NS, true)` with a loop over `NAMESPACES`. The SSH secret and postgres secret still go to `rio-system` only (control-plane resources). S3 creds may need duplicating into `rio-store` if the store Deployment reads them — check `store.yaml` envFrom.

The helm release stays anchored at `rio-system` (`--namespace rio-system`); cross-namespace resources render via explicit `metadata.namespace:` in templates. Helm handles this fine.

### T6 — `feat(xtask):` status.rs — list across four namespaces

At [`xtask/src/k8s/status.rs`](../../xtask/src/k8s/status.rs): the `Api::namespaced(client, NS)` calls (`:83`, `:93`) need to iterate. `scheduler_leader` (`:83`) stays `rio-system` (lease lives there). `WorkerPool` listing (`:93`) becomes `BuilderPool` in `rio-builders` + `FetcherPool` in `rio-fetchers` — two API calls, merge into the report. Pod/Deployment listings: group by namespace in the output so `xtask k8s status` shows a per-ns breakdown.

### T7 — `refactor(xtask):` kube.rs — scheduler_leader + list helpers take ns param

[`xtask/src/kube.rs:138`](../../xtask/src/kube.rs) `scheduler_leader` already takes `ns: &str` — good, no change. Audit the other helpers (`:103`, `:115`, `:128`, `:167`, `:212`, `:224`, `:247`, `:280`, `:319`) — they already take `ns`. The work is at the **call sites** in T5/T6, not here. If any helper hardcodes `NS`, parameterize it.

## Exit criteria

- `/nixbuild .#ci` green
- `helm template infra/helm/rio-build | yq 'select(.kind=="Namespace") | .metadata.name'` → four names: `rio-system rio-store rio-builders rio-fetchers`
- `helm template infra/helm/rio-build | yq 'select(.kind=="NetworkPolicy") | .metadata.name'` → includes `builder-egress`, `fetcher-egress`, `store-ingress`, `scheduler-ingress`; does NOT include `rio-worker-egress` or anything matching `fod-proxy`
- `grep -r 'fodProxy\|fod-proxy' infra/helm/` → 0 hits
- `grep 'rio-worker-preferred\|rio-worker-fallback' infra/helm/rio-build/values.yaml` → 0 hits
- On a deployed cluster: `kubectl -n rio-builders run probe --rm -it --image=busybox --restart=Never -- wget -T5 -qO- https://github.com` → **fails** (connection refused/timeout)
- On a deployed cluster: `kubectl -n rio-fetchers run probe --rm -it --image=busybox --restart=Never -- wget -T5 -qO- https://github.com` → **succeeds**
- On a deployed cluster: `kubectl -n rio-fetchers run probe --rm -it --image=busybox --restart=Never -- wget -T5 -qO- http://169.254.169.254/latest/meta-data/` → **fails**

## Tracey

Implements (new `r[impl]` annotations land in the helm templates as `# r[impl ...]` comments):
- `r[builder.netpol.airgap]` — T2 `builder-egress` policy in `networkpolicy.yaml`
- `r[fetcher.netpol.egress-open]` — T2 `fetcher-egress` policy in `networkpolicy.yaml`
- `r[fetcher.node.dedicated]` — T4 `rio-fetcher` NodePool in `values.yaml` (or `karpenter.yaml` if NodePools render from a dedicated template)

`r[verify]` markers for the three rules go at the VM-test `subtests = [...]` wiring in `nix/tests/default.nix` per CLAUDE.md placement rules — deferred to whichever plan adds the netpol VM test scenario (probe pods above are manual exit-criteria, not CI).

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/namespace.yaml", "action": "MODIFY", "note": "T1: 1→4 namespaces, split PSA baseline/privileged"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T2: per-ns split; builder-egress airgap, fetcher-egress 0/0:80,443 except RFC1918+link-local; store/scheduler-ingress cross-ns via namespaceSelector. Delete fod-proxy rules."},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "T3: metadata.namespace → rio-store on Deployment/Service"},
  {"path": "infra/helm/rio-build/templates/fod-proxy.yaml", "action": "DELETE", "note": "T4: Squid gone, hash check is the boundary"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T4: delete fodProxy section; rename rio-worker-* pools → rio-builder-*; add rio-fetcher NodePool (t3a, rio.build/fetcher taint); namespaces list"},
  {"path": "xtask/src/k8s/mod.rs", "action": "MODIFY", "note": "T5: NS const → NAMESPACES &[(&str,bool)]; keep NS alias for control-plane"},
  {"path": "xtask/src/k8s/eks/deploy.rs", "action": "MODIFY", "note": "T5: ensure_namespace loop over 4"},
  {"path": "xtask/src/k8s/k3s/mod.rs", "action": "MODIFY", "note": "T5: ensure_namespace loop over 4"},
  {"path": "xtask/src/k8s/kind/mod.rs", "action": "MODIFY", "note": "T5: ensure_namespace loop over 4"},
  {"path": "xtask/src/k8s/status.rs", "action": "MODIFY", "note": "T6: list BuilderPool/FetcherPool + pods across 4 namespaces; per-ns grouping"},
  {"path": "xtask/src/kube.rs", "action": "MODIFY", "note": "T7: audit — helpers already take ns param; parameterize any stragglers"}
]
```

```
infra/helm/rio-build/
├── templates/
│   ├── namespace.yaml        # T1: four-ns range
│   ├── networkpolicy.yaml    # T2: per-ns split, namespaceSelector cross-rules
│   ├── store.yaml            # T3: → rio-store ns
│   └── fod-proxy.yaml        # T4: DELETE
└── values.yaml               # T4: fetcher NodePool, builder rename, -fodProxy
xtask/src/
├── k8s/
│   ├── mod.rs                # T5: NAMESPACES const
│   ├── eks/deploy.rs         # T5: ensure_namespace ×4
│   ├── k3s/mod.rs            # T5: ensure_namespace ×4
│   ├── kind/mod.rs           # T5: ensure_namespace ×4
│   └── status.rs             # T6: multi-ns listing
└── kube.rs                   # T7: ns param audit
```

## Dependencies

```json deps
{"deps": [451, 453], "soft_deps": [], "note": "P0451 lands BuilderPool/FetcherPool CRDs + reconcilers that set rio.build/role labels (T2 podSelectors depend on them) and know which namespace to create STS in. P0453 lands ExecutorKind routing — without it, deploying this helm split means FODs queue forever (no fetcher assignment)."}
```

**Depends on:** [P0451](plan-0451-builderpool-fetcherpool-crds.md) (CRDs + reconciler ns-awareness), [P0453](plan-0453-scheduler-executor-kind-routing.md) (FOD→fetcher routing).
**Conflicts with:** any plan touching `values.yaml` Karpenter section or `networkpolicy.yaml`. `fod-proxy.yaml` deletion is terminal — any in-flight plan adding to the proxy allowlist is obsolete.
