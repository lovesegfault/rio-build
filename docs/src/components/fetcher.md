# rio-fetcher

FOD-only executor. Same binary as [rio-builder](builder.md), launched with `RIO_EXECUTOR_KIND=fetcher`.

See [ADR-019](../decisions/019-builder-fetcher-split.md) for the full rationale behind the builder/fetcher split. In short: regular builds and FOD fetches have opposite network requirements — builds should be airgapped; fetches need the open internet. Running both on the same pod type forces a leaky compromise. Splitting them lets builders be fully airgapped while fetchers rely on the FOD hash check as their integrity boundary.

## Responsibilities

- Receive FOD build assignments from scheduler via gRPC (scheduler routes FODs here per `r[sched.dispatch.fod-to-fetcher]`)
- Execute the FOD fetch via `nix-daemon --stdio` with network access enabled in the sandbox
- Verify the output hash before upload (`r[builder.fod.verify-hash]`)
- Upload the verified output NAR to rio-store
- Heartbeat to scheduler with `ExecutorKind::Fetcher`

## Differences from builder

| Aspect | Builder | Fetcher |
|---|---|---|
| Workload | Regular derivations | Fixed-output derivations only |
| Network | Airgapped (`r[builder.netpol.airgap]`) | Egress open minus RFC1918 (`r[fetcher.netpol.egress-open]`) |
| Seccomp | Standard builder profile | Stricter (`r[fetcher.sandbox.strict-seccomp]`) |
| Node pool | `rio.build/builder` taint | Dedicated `rio.build/fetcher` taint (`r[fetcher.node.dedicated]`) |
| Rootfs | Writable | `readOnlyRootFilesystem: true` |
| CRD | `BuilderPool` (size-classed) | `FetcherPool` (fixed replicas, no size classes) |
| Namespace | `rio-builders` | `rio-fetchers` |

## Hash verification before upload

r[fetcher.upload.hash-verify-before]

The fetcher MUST verify the FOD output hash **before** initiating upload to rio-store. A hash mismatch is reported as `BuildFailure` and the output is discarded locally — it never reaches the store. This is the integrity boundary that makes the egress-open NetworkPolicy safe: an attacker who compromises an upstream mirror or intercepts the fetch can at worst waste fetcher CPU; they cannot inject content into the store.

The verification uses `verify_fod_hashes()` (shared with the builder binary) against the `outputHash` the scheduler included in the assignment. The scheduler knows the expected hash before dispatch; the fetcher re-derives `is_fod` from the `.drv` itself and cross-checks (`r[builder.executor.kind-gate]`).

## Specification markers

The ADR-019–defined markers for this component live in [ADR-019](../decisions/019-builder-fetcher-split.md):

- `r[fetcher.netpol.egress-open]` — NetworkPolicy: 0.0.0.0/0 on 80/443 minus RFC1918/link-local/loopback
- `r[fetcher.sandbox.strict-seccomp]` — stricter seccomp (deny ptrace/bpf/setns/keyctl), readOnlyRootFilesystem
- `r[fetcher.node.dedicated]` — dedicated Karpenter NodePool with `rio.build/fetcher` taint
- `r[ctrl.fetcherpool.reconcile]` — FetcherPool CRD reconciler
- `r[sched.dispatch.fod-to-fetcher]` — scheduler hard-filter routes FODs here
- `r[sched.dispatch.no-fod-fallback]` — FODs queue rather than fall back to builders
