# ADR-019: Builder/Fetcher Split with Network Isolation

## Status
Accepted

## Context

rio-build currently runs a single "worker" pod type that handles both regular derivation builds and fixed-output derivation (FOD) fetches. These two workloads have opposite security requirements:

**Regular builds** execute arbitrary shell code from derivations. They need rio-store (inputs, output upload) and rio-scheduler (assignment stream, log upload) but have no legitimate reason to reach the internet. A compromised build that can exfiltrate secrets or call home is a real threat --- builds may process proprietary source, credentials baked into derivation env, or outputs of prior builds.

**FOD fetches** download source tarballs, git clones, and archives from arbitrary URLs. They need unrestricted egress by design. But the output is content-addressed: the scheduler knows the expected hash before dispatch, and a tampered fetch produces a hash mismatch that `verify_fod_hashes()` rejects before upload. An attacker who compromises a fetcher can at worst waste resources or DoS upstream mirrors --- they cannot inject malicious content into the store.

The current design routes both through the same pods with the same NetworkPolicy. The compromise is a Squid `fod-proxy` with a domain allowlist: workers egress only to the proxy, which filters by hostname. This is leaky (any worker can reach the proxy, so any build can fetch if it sets `http_proxy`) and operationally annoying (every new upstream domain needs an allowlist entry).

[ADR-012](012-privileged-builder-pods.md) notes that `CAP_SYS_ADMIN` pods are a significant escape surface and "the seccomp profile is the real security boundary." Splitting builders from fetchers lets us tighten that boundary differently per role: builders get a standard profile with no network; fetchers get a stricter profile (read-only rootfs, minimal syscalls) because they face the open internet, with the FOD hash check as the integrity backstop.

## Decision

Split the single worker type into two distinct executor kinds with separate CRDs, namespaces, network policies, and node pools.

### Terminology

- **Worker** → **Builder** everywhere (crate, CRD, proto, metrics, tracey markers, docs). "Worker" was generic; "builder" says what it does.
- **Fetcher** is the new FOD-only executor. Same binary as builder (`rio-builder`), different `RIO_EXECUTOR_KIND` env.

### Four-namespace layout

| Namespace | PSA | Contents |
|---|---|---|
| `rio-system` | baseline | scheduler, gateway, controller, dashboard, PostgreSQL |
| `rio-store` | baseline | store (own ns so executor NetworkPolicies can target it precisely) |
| `rio-builders` | privileged | builder StatefulSets |
| `rio-fetchers` | privileged | fetcher StatefulSets |

`privileged` PSA narrows to the two namespaces that need `CAP_SYS_ADMIN` for FUSE. Control plane drops to `baseline`.

### Two CRDs

r[ctrl.builderpool.reconcile]

`BuilderPool` (renamed `WorkerPool`) lives in `rio-builders`. Same spec as today minus `fodProxyUrl`. Reconciler labels pods `rio.build/role: builder`.

r[ctrl.fetcherpool.reconcile]

`FetcherPool` is new, lives in `rio-fetchers`. Minimal spec (`replicas`, `systems`, `nodeSelector`, `tolerations`, `resources`) --- no size-class because fetches are network-bound, not CPU-predictable. Reconciler labels pods `rio.build/role: fetcher` and sets stricter `securityContext` (`readOnlyRootFilesystem: true`, stricter seccomp).

`BuilderPoolSet` (renamed `WorkerPoolSet`) generates size-class `BuilderPool` children per [ADR-015](015-size-class-routing.md). Unchanged semantics; the rename is cosmetic.

### Scheduler routing


The `ExecutorKind` enum (`Builder | Fetcher`) is added to the heartbeat payload and `ExecutorState`. `hard_filter()` gains one clause:

```rust
if drv.is_fixed_output != (executor.kind == ExecutorKind::Fetcher) {
    return false;
}
```

FODs route only to fetchers. Non-FODs route only to builders.


The overflow chain (`find_executor_with_overflow()`) is skipped for FODs --- fetchers have no size classes to overflow through. If no fetcher is available, the FOD queues. The scheduler NEVER sends a FOD to a builder, even under pressure. This keeps the builder airgap absolute.

The `CutoffRebalancer` operates on builder pools only. Fetcher replica count is a fixed `FetcherPool.spec.replicas` (or a simple HPA on queue depth; not duration-EMA).

### Executor enforcement


`rio-builder` re-derives `is_fod` from the `.drv` itself (it already does, per `wkr-fod-flag-trust` remediation). If `is_fod` disagrees with the pod's `RIO_EXECUTOR_KIND`, the executor returns `ExecutorError::WrongKind` without spawning the daemon. Defense-in-depth --- the scheduler should never misroute, but a bug or a stale-generation race shouldn't grant a builder internet access.

### Network isolation

r[builder.netpol.airgap]

`builder-egress` NetworkPolicy (in `rio-builders`) allows: CoreDNS:53, `rio-scheduler.rio-system:9001`, `rio-store.rio-store:9002`. Nothing else. The `fod-proxy:3128` rule is deleted. Optionally, if `BuilderPool.spec.s3Direct: true`, the S3 VPC endpoint CIDR is added (for direct chunk upload; default is store-proxied).

r[fetcher.netpol.egress-open]

`fetcher-egress` NetworkPolicy (in `rio-fetchers`) allows the same three, plus `0.0.0.0/0` on ports 80/443, **except** RFC1918 (`10/8`, `172.16/12`, `192.168/16`), link-local (`169.254/16`), and loopback. The metadata-service block (`169.254.169.254`) is inherited from the link-local deny.

r[store.netpol.egress]

`store-egress` NetworkPolicy (in `rio-store`) allows: CoreDNS:53 (UDP+TCP), postgres:5432 on RFC1918, optionally S3 VPC endpoint:443. Nothing else. The store pod holds S3 and postgres credentials; a compromised store MUST NOT reach IMDS (`169.254.169.254`) for role escalation or arbitrary public IPs for exfiltration. Default-deny egress is the same defense-in-depth posture as `builder-egress`.

The Squid `fod-proxy` is deleted. The FOD hash check is the integrity boundary; a domain allowlist adds operational friction for marginal gain.

### Sandbox hardening

r[fetcher.sandbox.strict-seccomp]

Fetchers get a stricter seccomp profile (`rio-fetcher.json`) than builders: deny `ptrace`, `bpf`, `setns`, `process_vm_readv`/`writev`, `keyctl`, `add_key`. `mount` stays allowed (FUSE needs it). Pod `securityContext` sets `readOnlyRootFilesystem: true` --- the overlay upper-dir is a `tmpfs` emptyDir, so writes still work but rootfs tampering does not.

Rationale: fetchers face the open internet; the threat is a compromised upstream serving an exploit payload. The FOD hash check catches content tampering, but a fetcher that is itself rooted during the fetch (via a curl/git CVE) could pivot to the node. The stricter profile shrinks that surface.

### Node isolation

r[fetcher.node.dedicated]

A Karpenter NodePool (`rio-fetcher`) with taint `rio.build/fetcher=true:NoSchedule` and label `rio.build/node-role: fetcher`. `FetcherPool` reconciler sets the matching toleration and nodeSelector. Builder NodePools keep their existing `rio.build/builder=true:NoSchedule` taint. Neither can land on the other's nodes.

An attacker who escapes a fetcher pod lands on a node that runs only other fetchers. Lateral movement stays inside the hash-check boundary.

### Upload path abstraction

The `rio-builder` upload module exposes `trait OutputUploader` with two impls: `StoreProxied` (default --- stream NAR to rio-store, which writes chunks to S3) and `S3Direct` (builder writes chunks directly, rio-store records metadata only). This abstraction leaves room for a future `NodeLocal` impl where the builder writes outputs to a hostPath, terminates, and a fresh short-lived "uploader" pod ships them --- useful if builder pods are aggressively ephemeral and you want upload to outlive the build sandbox.

## Alternatives Considered

- **Keep fod-proxy, add a fetcher label:** Minimal change --- tag some workers as fetchers, only they can reach the proxy. Still requires the domain allowlist and a proxy Deployment to maintain. Chosen design deletes the proxy entirely; hash check is sufficient.

- **Single CRD with `spec.role` field:** One `ExecutorPool` CRD, role enum discriminates. Less duplication but one reconciler handles two security postures, and RBAC can't scope "create fetcher pods" separately from "create builder pods." Two CRDs keep the trust boundaries in the type system.

- **Configurable FOD fallback to builders:** Let operators enable "if no fetchers, send FODs to builders with a warning." Rejected: this requires builders to have conditional network access, which defeats the airgap. A queued FOD is preferable to a leaky builder.

- **Gateway-side fetching (plan-0117, abandoned):** Gateway fetches FOD sources, uploads to store, then scheduler dispatches a build-only job that reads from store. Eliminates fetcher pods entirely but puts arbitrary-URL fetching in the gateway's trust domain, which also holds tenant SSH keys. Worse blast radius.

## Consequences

**Migration** is big-bang: proto, CRD, crate renames must land together or CI breaks. The only deployment is dev EKS via `xtask`, so destroy-and-redeploy is acceptable. A new SQL migration `ALTER TABLE ... RENAME COLUMN` handles the schema (frozen-migration rule prohibits editing shipped `.sql`, but adding new ones is fine).

**Operational:** four namespaces instead of one means cross-namespace RBAC for the controller (watch/patch STS in `rio-builders` and `rio-fetchers`) and `namespaceSelector`-based NetworkPolicies. The helm chart and `xtask` deploy flow both need to create all four namespaces before installing.

**Observability:** `rio_worker_*` metrics become `rio_builder_*`; Grafana dashboards need regenerating. New metrics `rio_scheduler_fod_queue_depth` and `rio_scheduler_fetcher_utilization` track the split.

**Fetcher seccomp may be too strict** for exotic fetchers (git-lfs, Mercurial, Subversion). The profile starts as builder-profile-plus-denies; allowlist widens as real FODs hit denied syscalls. The VM test suite should include at least one git-based FOD to catch this early.
