#import "/lib/rio.typ": *
#show: rio.with(domains: ("fetcher", "builder", "store"))


#gls("fod")-only executor. Same binary as rio-builder, launched with
`RIO_EXECUTOR_KIND=fetcher`.

Regular builds and FOD fetches have opposite network requirements --- builds
should be airgapped; fetches need the open internet. Running both on the same
pod type forces a leaky compromise. Splitting them lets builders be fully
airgapped while fetchers rely on the FOD hash check as their integrity
boundary. See @fetcher-rationale-split for the full rationale.

= Responsibilities

- Receive FOD build assignments from the scheduler via gRPC (the scheduler
  routes FODs here per #rref("sched.dispatch.fod-to-fetcher"))
- Execute the FOD fetch via `nix-daemon --stdio` with network access enabled in
  the sandbox
- Verify the output hash before upload (#rref("builder.fod.verify-hash"))
- Upload the verified output @nar to rio-store
- Heartbeat to the scheduler with `ExecutorKind::Fetcher`

= Differences from builder

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([Aspect], [Builder], [Fetcher]),
    [Workload], [Regular derivations], [Fixed-output derivations only],
    [Network],
    [Airgapped (#rref("builder.netpol.airgap"))],
    [Egress open via Cilium `world` entity (#rref("fetcher.netpol.egress-open"))],

    [Seccomp],
    [Standard builder profile],
    [Stricter (#rref("fetcher.sandbox.strict-seccomp"))],

    [Node pool],
    [`rio.build/builder` taint],
    [Dedicated `rio.build/fetcher` taint (#rref("fetcher.node.dedicated"))],

    [Rootfs], [Writable], [`readOnlyRootFilesystem: true`],
    [@crd],
    [`Pool{kind=Builder}`],
    [`Pool{kind=Fetcher}` (ADR-019 hardening forced)],

    [Namespace], [`rio-builders`], [`rio-fetchers`],
  ),
)

= Hash verification before upload

#r("fetcher.upload.hash-verify-before")[
  The fetcher MUST verify the FOD output hash *before* initiating upload to
  rio-store. A hash mismatch is reported as `BuildResultStatus::OutputRejected`
  and the output is discarded locally --- it never reaches the store. This is
  the integrity boundary that makes the egress-open NetworkPolicy safe: an
  attacker who compromises an upstream mirror or intercepts the fetch can at
  worst waste fetcher CPU; they cannot inject content into the store.
]

The verification uses `verify_fod_hashes()` (shared with the builder binary)
against the `outputHash` the scheduler included in the assignment. The
scheduler knows the expected hash before dispatch; the fetcher re-derives
`is_fod` from the `.drv` itself and cross-checks
(#rref("builder.executor.kind-gate")).

= Hashed mirrors

#r("fetcher.nixconf.hashed-mirrors")[
  The fetcher's `nix.conf` MUST set `hashed-mirrors` (default
  `http://tarballs.nixos.org/`). When a flat-hash FOD's origin URL is dead,
  `nix-daemon`'s `builtin:fetchurl` tries `{mirror}/{algo}/{hexlower-digest}`
  first and only falls back to the origin on miss. Only `outputHashMode =
  "flat"` derivations qualify --- recursive (NAR-hash) FODs skip the mirror
  because the on-the-wire bytes don't correspond to the declared hash. The Helm
  chart exposes this as `.Values.nixConf.hashedMirrors` so operators can point
  at an internal mirror; an empty value disables the lookup.
]

= Network isolation

#r("builder.netpol.airgap")[
  `builder-egress` NetworkPolicy (in `rio-builders`) allows: CoreDNS:53,
  `rio-scheduler.rio-system:9001`, `rio-store.rio-store:9002`. Nothing else.
  The Squid-FOD-proxy `:3128` rule is deleted. Optionally, if `Pool.spec.s3Direct:
  true`, the S3 VPC endpoint CIDR is added (for direct chunk upload; default is
  store-proxied).
]

#r("fetcher.netpol.egress-open+2")[
  `fetcher-egress` CiliumClusterwideNetworkPolicy (in `rio-fetchers`) allows
  the same three in-cluster targets as builders, plus `toEntities: [world]` on
  ports 80/443. The `world` entity matches any address Cilium does not
  recognise as a cluster identity --- it is address-family-agnostic and
  inherently excludes pod, node, service, and host-local ranges (so the IMDS
  endpoint at `fd00:ec2::254` / `169.254.169.254` is denied without an explicit
  carve-out). With DNS64 enabled at the resolver, IPv4-only upstreams are
  reached via the `64:ff9b::/96` synthesised prefix, which `world` matches.
]

#r("store.netpol.egress+2")[
  `store-egress` CiliumNetworkPolicy (in `rio-store`) allows: CoreDNS:53
  (UDP+TCP), postgres:5432 (in-cluster via `toEndpoints` label match;
  out-of-cluster via `toCIDRSet` on the deployment's `postgresCidr` --- the VPC
  IPv6 block in EKS), optionally S3 VPC endpoint:443. Nothing else. The store
  pod holds S3 and postgres credentials; a compromised store MUST NOT reach
  IMDS (`fd00:ec2::254` / `169.254.169.254`) for role escalation or arbitrary
  public IPs for exfiltration. Default-deny egress is the same defense-in-depth
  posture as `builder-egress`.
]

The Squid FOD proxy is deleted. The FOD hash check is the integrity boundary;
a domain allowlist adds operational friction for marginal gain.

= Sandbox hardening

#r("fetcher.sandbox.strict-seccomp")[
  Fetchers get a stricter seccomp profile (`rio-fetcher.json`) than builders:
  deny `ptrace`, `bpf`, `setns`, `process_vm_readv`/`writev`, `keyctl`,
  `add_key`. `mount` stays allowed (FUSE needs it). Pod `securityContext` sets
  `readOnlyRootFilesystem: true` --- the overlay upper-dir is a disk-backed
  emptyDir (the only writable mount), so writes still work but rootfs tampering
  does not. (Originally tmpfs; changed to disk-backed under ADR-023 so
  `SpawnIntent.disk_bytes` budgets `ephemeral-storage` correctly and XFS
  prjquota telemetry works.)
]

Fetchers face the open internet; the threat is a compromised upstream serving
an exploit payload. The FOD hash check catches content tampering, but a fetcher
that is itself rooted during the fetch (via a curl/git CVE) could pivot to the
node. The stricter profile shrinks that surface.

= Node isolation

#r("fetcher.node.dedicated+4")[
  Fetcher pods land on dedicated nodes carrying the
  `rio.build/fetcher=true:NoSchedule` taint and `rio.build/fetcher: "true"`
  label (§13e: same key for taint and label, mirroring the metal
  `rio.build/kvm` pattern). The `Pool` reconciler derives the toleration from
  `taints_routing_to(FETCHER_TAINT_KEY)` (the same `[sla.hw_classes.$h].taints`
  map `cover::build_nodeclaim` reads). Restrictive placement is the merge of
  constraints that agree by construction: the FOD intent's `hw_class_names` ⊇
  `{fetcher-*}` drives the pod's per-intent `nodeAffinity` via
  `cells_to_selector_terms`, AND the pool-static `nodeSelector{rio.build/fetcher:
  true}` (§13e B4) keys on `pool.spec.kind == Fetcher` --- the per-intent
  affinity is a projection of the pool-level invariant, not an independent
  opinion of it. The operator's `pool.spec.node_selector` MERGES with the
  pool-static constraint (r35 bug_044 --- the operator ADDS constraints like AZ
  pin or instance type; it cannot replace or weaken `rio.build/fetcher: true`,
  which the controller unconditionally inserts and the CEL admission rule
  guards). Builder NodeClaims keep their `rio.build/builder=true:NoSchedule`
  taint; fetcher NodeClaims get `rio.build/fetcher` instead ---
  `cover::build_nodeclaim` branches on `provides_features ∋ fetcher`. Neither
  can land on the other's nodes.
]

An attacker who escapes a fetcher pod lands on a node that runs only other
fetchers. Lateral movement stays inside the hash-check boundary.

= Related markers

The following markers defined in other chapters govern fetcher behaviour:

- #rref("ctrl.pool.reconcile") --- Pool CRD reconciler (`kind=Fetcher` arm)
- #rref("ctrl.pool.fetcher-hardening") --- ADR-019 hardening forced regardless
  of spec
- #rref("ctrl.pool.fetcher-spawn-builtin") --- spawn signal counts `builtin`
  FODs
- #rref("sched.dispatch.fod-to-fetcher") --- scheduler hard-filter routes FODs
  here
- #rref("sched.dispatch.fod-builtin-any-arch") --- `system="builtin"` FOD
  eligible on any fetcher
- #rref("sched.sla.reactive-floor") --- `resource_floor` doubled on explicit
  resource-exhaustion signals (FOD and non-FOD share the same path)

= Rationale

== Builder/fetcher split // supersedes ADR-019
<fetcher-rationale-split>

A single "worker" pod type that handled both regular derivation builds and
fixed-output derivation fetches forced a leaky compromise. Regular builds
execute arbitrary shell code from derivations and have no legitimate reason to
reach the internet --- a compromised build that can exfiltrate secrets or call
home is a real threat. FOD fetches download from arbitrary URLs by design, but
the output is #gls("ca", display: "content-addressed"): the scheduler knows the expected hash before
dispatch, and a tampered fetch produces a hash mismatch that
`verify_fod_hashes()` rejects before upload.

The single worker type was split into two distinct executor kinds with separate
CRDs, namespaces, network policies, and node pools. *Worker* became *Builder*
everywhere (crate, CRD, proto, metrics, tracey markers, docs); *Fetcher* is the
FOD-only executor --- same `rio-builder` binary, different `RIO_EXECUTOR_KIND`
env.

*Four-namespace layout.* `rio-system` (PSA #(refs.psa)("rio-system")) holds
scheduler, gateway, controller, dashboard, PostgreSQL. `rio-store`
(#(refs.psa)("rio-store")) holds the store in its own namespace so executor
NetworkPolicies can target it precisely. `rio-builders`
(#(refs.psa)("rio-builders")) and `rio-fetchers` (#(refs.psa)("rio-fetchers"))
hold the respective Jobs. `privileged` PSA
narrows to the two namespaces that need `CAP_SYS_ADMIN` for @fuse; the control
plane is #rref("sec.psa.control-plane-restricted").

*One CRD, two kinds.* A `Pool{kind=Builder}` lives in `rio-builders`; a
`Pool{kind=Fetcher}` lives in `rio-fetchers`. The reconciler spawns one-shot
Jobs up to `spec.maxConcurrent` against `GetSpawnIntents{kind=...}`, labels
pods `rio.build/role: {builder,fetcher}`, and for fetchers forces the ADR-019
hardening (#rref("ctrl.pool.fetcher-hardening")).

*Scheduler routing.* The `ExecutorKind` enum (`Builder | Fetcher`) is added to
the heartbeat payload and `ExecutorState`. `hard_filter()` gains one clause:

```rust
if drv.is_fixed_output != (executor.kind == ExecutorKind::Fetcher) {
    return false;
}
```

FODs route only to fetchers; non-FODs route only to builders. Dispatch
hard-filters by `ExecutorKind`: if no fetcher is available the FOD queues. The
scheduler NEVER sends a FOD to a builder, even under pressure --- this keeps
the builder airgap absolute. Fetcher concurrency is bounded by
`Pool.spec.maxConcurrent`; the reconciler spawns Jobs reactively against
`queued_fod_derivations`.

*Executor enforcement.* `rio-builder` re-derives `is_fod` from the `.drv`
itself. If `is_fod` disagrees with the pod's `RIO_EXECUTOR_KIND`, the executor
returns `ExecutorError::WrongKind` without spawning the daemon.
Defense-in-depth --- the scheduler should never misroute, but a bug or a
stale-generation race shouldn't grant a builder internet access.

*Upload path abstraction.* The `rio-builder` upload module exposes `trait
OutputUploader` with two impls: `StoreProxied` (default --- stream NAR to
rio-store, which writes chunks to S3) and `S3Direct` (builder writes chunks
directly, rio-store records metadata only). This abstraction leaves room for a
future `NodeLocal` impl where the builder writes outputs to a hostPath,
terminates, and a fresh short-lived "uploader" pod ships them.

*Alternatives rejected.* A configurable FOD-fallback-to-builders ("if no
fetchers, send FODs to builders with a warning") was rejected: it requires
builders to have conditional network access, which defeats the airgap --- a
queued FOD is preferable to a leaky builder. Gateway-side fetching (gateway
fetches FOD sources, uploads to store, dispatches a build-only job) would
eliminate fetcher pods entirely but puts arbitrary-URL fetching in the
gateway's trust domain, which also holds tenant SSH keys --- worse blast
radius. A single `ExecutorPool` CRD with a `spec.role` enum was rejected
because RBAC can't then scope "create fetcher pods" separately from "create
builder pods"; two kinds keep the trust boundaries in the type system.

*Consequences.* Migration was big-bang: proto, CRD, and crate renames landed
together. Four namespaces means cross-namespace RBAC for the controller and
`namespaceSelector`-based NetworkPolicies. `rio_worker_*` metrics became
`rio_builder_*`; rio_scheduler_queue_depth and
rio_scheduler_utilization (both retired with the placement layer) gained a `{kind}` label
(`builder`/`fetcher`) to track the split. The fetcher @seccomp
profile may be too strict for exotic fetchers (git-lfs, Mercurial, Subversion)
--- the profile starts as builder-profile-plus-denies and the allowlist widens
as real FODs hit denied syscalls. The VM test suite includes at least one
git-based FOD to catch this early.
