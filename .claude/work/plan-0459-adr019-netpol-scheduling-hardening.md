# Plan 459: ADR-019 netpol+scheduling hardening — DS fetcher scheduling, store-egress, DNS TCP/53

Three correctness gaps in [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md)'s four-namespace split, surfaced by bughunter post-merge. Bundled into one plan because all three touch [`networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml) and/or [`values.yaml`](../../infra/helm/rio-build/values.yaml), and together they close the ADR-019 security posture.

**T1 is CRITICAL:** the `devicePlugin` and `seccompInstaller` DaemonSets at [`values.yaml:496`](../../infra/helm/rio-build/values.yaml) have `nodeSelector: {rio.build/node-role: builder}` — they schedule on builder nodes ONLY. Fetcher nodes (label `rio.build/node-role: fetcher` per P0454-T4) get neither DS. Without the device plugin, the `smarter-devices/fuse` extended resource is never advertised on fetcher nodes → FetcherPool pods requesting `resources.limits["smarter-devices/fuse"]: 1` stay **Pending** forever. Without the seccomp installer, pods that DO schedule would CrashLoop on the missing Localhost profile. Production fetchers literally cannot start.

**T2:** [`networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml) has `store-ingress` (`:211`) but NO `store-egress`. With `policyTypes: [Ingress]` only, store egress is unrestricted — the store pod can reach IMDS (`169.254.169.254`), the K8s apiserver, arbitrary public IPs. The store holds S3 credentials (presigned-URL minting) and postgres creds; a compromised store should not be able to reach IMDS for role escalation.

**T3:** all four egress policies (builder-egress `:42`, fetcher-egress `:96`, rio-controller-egress `:155`, rio-dashboard-egress `:275`) allow CoreDNS on `UDP/53` only. DNS falls back to TCP for responses >512 bytes (DNSSEC, large SRV records, long CNAME chains). Missing `TCP/53` → intermittent resolution failures on large responses. Low-frequency but real.

## Entry criteria

- [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) merged — four-namespace split + `rio-fetcher` NodePool + per-ns NetworkPolicies exist

## Tasks

### T1 — `fix(helm):` devicePlugin+seccompInstaller DS — schedule on fetcher nodes too

At [`values.yaml:496`](../../infra/helm/rio-build/values.yaml), both DaemonSets have:

```yaml
nodeSelector:
  rio.build/node-role: builder
tolerations:
  - key: rio.build/builder
    operator: Equal
    value: "true"
    effect: NoSchedule
```

`nodeSelector` is matchLabels-only (no set-based `In`). Two options:

**Option A (preferred — minimal values.yaml change):** switch both DaemonSets' scheduling to `nodeAffinity` with `matchExpressions: [{key: rio.build/node-role, operator: In, values: [builder, fetcher]}]`, and add a second toleration for `rio.build/fetcher`. Template-side: `templates/device-plugin.yaml` + `templates/seccomp-installer.yaml` already render `.nodeSelector` and `.tolerations` verbatim from values — extend them to also render `.nodeAffinity` when set. values.yaml:

```yaml
devicePlugin:
  nodeSelector: {}  # cleared — nodeAffinity takes over
  nodeAffinity:
    requiredDuringSchedulingIgnoredDuringExecution:
      nodeSelectorTerms:
        - matchExpressions:
            - {key: rio.build/node-role, operator: In, values: [builder, fetcher]}
  tolerations:
    - {key: rio.build/builder, operator: Equal, value: "true", effect: NoSchedule}
    - {key: rio.build/fetcher, operator: Equal, value: "true", effect: NoSchedule}
```

Same for `seccompInstaller:` at `:525`.

**Option B (simpler but adds a label):** add a second label `rio.build/executor-node: "true"` to BOTH NodePools (builder + fetcher) in the Karpenter section, change both DS `nodeSelector` to match `rio.build/executor-node: "true"`, keep both tolerations. Less template churn, one extra label. Choose whichever keeps the template diff smaller.

Update the comments at `:494` and `:522` — they say "builder nodes only" / "Same nodeSelector as builderPool" which becomes wrong.

### T2 — `fix(helm):` add store-egress NetworkPolicy — DNS + postgres + S3, deny IMDS

At [`networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml), after `store-ingress` (`:240`), add:

```yaml
---
# r[impl store.netpol.egress]
#
# Store egress: DNS + postgres + S3 VPC endpoint. The store pod holds
# S3 creds (presigned-URL minting) + postgres creds — a compromised
# store should NOT reach IMDS for role escalation or arbitrary public
# IPs for exfil. No rule for 169.254.0.0/16 or 0.0.0.0/0 → DENIED
# (default-deny for listed policyTypes).
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: store-egress
  namespace: {{ $ns.store.name }}
  labels:
    app.kubernetes.io/part-of: rio-build
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/name: rio-store
  policyTypes: [Egress]
  egress:
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - {protocol: UDP, port: 53}
        - {protocol: TCP, port: 53}
    # Postgres: RFC1918 on 5432. External PG (RDS, CloudSQL) lives in
    # the VPC's private CIDR. Link-local + public implicitly DENIED.
    - to:
        - ipBlock: {cidr: 10.0.0.0/8}
        - ipBlock: {cidr: 172.16.0.0/12}
        - ipBlock: {cidr: 192.168.0.0/16}
      ports: [{protocol: TCP, port: 5432}]
    {{- with .Values.networkPolicy.storeS3Cidr }}
    # S3 VPC endpoint CIDR — store writes chunks directly. Same
    # operator-set pattern as builderS3Cidr.
    - to:
        - ipBlock: {cidr: {{ . }}}
      ports: [{protocol: TCP, port: 443}]
    {{- end }}
```

Add `.Values.networkPolicy.storeS3Cidr` to [`values.yaml`](../../infra/helm/rio-build/values.yaml) alongside `builderS3Cidr` (same commented-out-by-default pattern).

### T3 — `fix(helm):` DNS TCP/53 in all egress policies

At [`networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml), the CoreDNS egress rule in four policies has `ports: [{protocol: UDP, port: 53}]` only. Add TCP/53:

- `builder-egress` `:42`
- `fetcher-egress` `:96`
- `rio-controller-egress` `:155`
- `rio-dashboard-egress` `:275`

Change each to:

```yaml
ports:
  - {protocol: UDP, port: 53}
  - {protocol: TCP, port: 53}
```

T2's new `store-egress` already has both (per the snippet above) — keep that.

### T4 — `test(vm):` netpol.nix — store-egress IMDS-deny + DNS-TCP probes

At [`nix/tests/scenarios/netpol.nix`](../../nix/tests/scenarios/netpol.nix), extend the existing nsenter-probe pattern with:

1. **store-egress IMDS deny:** nsenter into a `rio-store` pod netns, `curl -sf --max-time 5 http://169.254.169.254/latest/meta-data/` → MUST fail (rc≠0). Positive control first: `nc -z <postgres-clusterip> 5432` → MUST succeed (proves the policy isn't over-broad).
2. **DNS TCP/53:** nsenter into a builder pod netns, `dig +tcp +short rio-scheduler.rio-system.svc.cluster.local @<coredns-ip>` → MUST resolve (proves TCP/53 is allowed). Without T3, this returns `connection refused` from the policy deny.

Add `# r[verify store.netpol.egress]` at the `netpol` subtests wiring in [`nix/tests/default.nix`](../../nix/tests/default.nix) (NOT in `netpol.nix` header — per CLAUDE.md VM-test placement).

## Exit criteria

- `/nixbuild .#ci` green
- `helm template infra/helm/rio-build | yq 'select(.kind=="NetworkPolicy") | .metadata.name'` → includes `store-egress`
- `helm template infra/helm/rio-build | yq 'select(.kind=="NetworkPolicy" and .metadata.name=="store-egress") | .spec.policyTypes'` → `[Egress]`
- `helm template infra/helm/rio-build | yq 'select(.kind=="NetworkPolicy") | select(.spec.egress[].ports[] | select(.port==53 and .protocol=="TCP"))' | grep -c 'name:'` → ≥5 (builder, fetcher, controller, dashboard, store all have TCP/53)
- `helm template infra/helm/rio-build | yq 'select(.kind=="DaemonSet" and .metadata.name=="smarter-device-manager") | .spec.template.spec.tolerations[] | select(.key=="rio.build/fetcher")'` → non-empty (DS tolerates fetcher taint)
- On a deployed cluster with a fetcher node: `kubectl -n rio-fetchers get pod -l rio.build/role=fetcher -o jsonpath='{.items[0].status.phase}'` → `Running` (NOT `Pending`)
- `grep 'builder nodes only\|Same nodeSelector as builderPool' infra/helm/rio-build/values.yaml` → 0 hits (stale comments updated)

## Tracey

References existing markers:
- `r[fetcher.node.dedicated]` — T1 fixes the DS scheduling gap; fetcher nodes now get device-plugin + seccomp-installer (implied by "fetchers need FUSE" in [ADR-019:95](../../docs/src/decisions/019-builder-fetcher-split.md))
- `r[sec.pod.fuse-device-plugin]` — T1 (the device plugin must run wherever pods request `smarter-devices/fuse`)
- `r[builder.netpol.airgap]` — T3 refines (DNS egress now UDP+TCP)
- `r[fetcher.netpol.egress-open]` — T3 refines (DNS egress now UDP+TCP)

Adds new marker to ADR-019:
- `r[store.netpol.egress]` → [`docs/src/decisions/019-builder-fetcher-split.md`](../../docs/src/decisions/019-builder-fetcher-split.md) (see ## Spec additions below)

## Spec additions

Add to [`docs/src/decisions/019-builder-fetcher-split.md`](../../docs/src/decisions/019-builder-fetcher-split.md) after `r[fetcher.netpol.egress-open]` (`:81`):

```
r[store.netpol.egress]

`store-egress` NetworkPolicy (in `rio-store`) allows: CoreDNS:53 (UDP+TCP), postgres:5432 on RFC1918, optionally S3 VPC endpoint:443. Nothing else. The store pod holds S3 and postgres credentials; a compromised store MUST NOT reach IMDS (`169.254.169.254`) for role escalation or arbitrary public IPs for exfiltration. Default-deny egress is the same defense-in-depth posture as `builder-egress`.
```

## Files

```json files
[
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T1: devicePlugin+seccompInstaller nodeAffinity In [builder,fetcher] + dual tolerations; T2: networkPolicy.storeS3Cidr"},
  {"path": "infra/helm/rio-build/templates/device-plugin.yaml", "action": "MODIFY", "note": "T1: render .nodeAffinity when set (or Option-B: nodeSelector stays, new label)"},
  {"path": "infra/helm/rio-build/templates/seccomp-installer.yaml", "action": "MODIFY", "note": "T1: render .nodeAffinity when set"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T2: add store-egress policy; T3: TCP/53 in 4 existing DNS rules"},
  {"path": "nix/tests/scenarios/netpol.nix", "action": "MODIFY", "note": "T4: store-egress IMDS-deny + DNS-TCP probes"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T4: r[verify store.netpol.egress] at netpol subtests wiring"},
  {"path": "docs/src/decisions/019-builder-fetcher-split.md", "action": "MODIFY", "note": "Spec additions: r[store.netpol.egress] marker"}
]
```

```
infra/helm/rio-build/
├── templates/
│   ├── device-plugin.yaml        # T1: nodeAffinity render
│   ├── seccomp-installer.yaml    # T1: nodeAffinity render
│   └── networkpolicy.yaml        # T2: +store-egress; T3: +TCP/53 ×4
└── values.yaml                   # T1: DS scheduling; T2: storeS3Cidr
nix/tests/
├── scenarios/netpol.nix          # T4: store-egress + DNS-TCP probes
└── default.nix                   # T4: r[verify] wiring
docs/src/decisions/
└── 019-builder-fetcher-split.md  # spec: r[store.netpol.egress]
```

## Dependencies

```json deps
{"deps": [454], "soft_deps": [], "note": "P0454 landed the four-ns split + rio-fetcher NodePool + per-ns NetworkPolicies. All three fixes are correctness gaps in P0454's output. discovered_from=bughunter (DS scheduling + store-egress + DNS-TCP all from the same sweep)."}
```

**Depends on:** [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) — the four-ns split + NetworkPolicies + `rio-fetcher` NodePool exist.
**Conflicts with:** [P0460](plan-0460-psa-restricted-control-plane.md) touches `values.yaml` `:44-45` (namespaces PSA) — non-overlapping with T1's `:496`/`:525` DS sections. `networkpolicy.yaml` is low-traffic post-P0454. `netpol.nix` is low-traffic.
