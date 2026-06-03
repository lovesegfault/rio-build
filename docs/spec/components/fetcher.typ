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
- Execute the FOD fetch natively (rio-exec sandbox / `builtin:fetchurl`
  re-exec) with network access enabled in
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

= builtin:fetchurl

#r("fetcher.fetchurl.sandboxed")[
  `builtin:fetchurl` derivations are executed by re-exec'ing the rio-builder
  binary inside a network-enabled rio-exec sandbox (`__builtin-fetchurl`
  subcommand): the fetch runs as the unprivileged build user, attached to the
  per-build cgroup, subject to the same timeout/cancellation machinery as any
  other build. The host `/nix/store` is exposed read-only inside that sandbox
  (the re-exec'd binary needs its own runtime closure); the fetched output is
  written to a dedicated writable mount and is still subject to the FOD hash
  gate (#rref("fetcher.upload.hash-verify-before")) before anything reaches
  the store. The sandbox's CA bundle comes from `RIO_CA_BUNDLE`; HTTPS fetches
  without a usable bundle fail with an actionable error rather than
  downgrading verification.
]

= Hashed mirrors

#r("fetcher.mirrors.hashed+2")[
  When a flat-hash FOD's origin URL is dead, `builtin:fetchurl` tries
  `{mirror}/{algo}/{base16-digest}` (the digest as declared in the
  derivation's `outputHash`, passed through unchanged) for each configured
  hashed mirror first
  and only falls back to the origin on miss. Only `outputHashMode = "flat"`
  derivations qualify --- recursive (NAR-hash) FODs skip the mirror because
  the on-the-wire bytes don't correspond to the declared hash. Mirrors are
  configured per pool (`Pool.spec.hashedMirrors` / `poolDefaults`) and reach
  the worker as `RIO_HASHED_MIRRORS`; an empty list disables the lookup.
]

#r("fetcher.mirrors.admission-accept-set")[
  The `hashedMirrors` admission accept-set MUST be single-sourced from the
  terminal fetch consumers' contracts and MUST equal: http(s)-scheme URLs
  whose remaining bytes are printable ASCII excluding comma. Every
  derivation of the constraint (the CEL admission rule, the reconciler's
  runtime filter) MUST be derived from the one defining pattern; an entry
  outside the set MUST be rejected at admission, and the reconciler MUST
  skip-and-warn --- never silently mangle --- such entries on resources
  admitted under older rules.
]

The accept-set is an intersection, not a style choice: the value transits a
comma-joined env (`RIO_HASHED_MIRRORS`) and a whitespace-split env
(`RIO_FETCHURL_MIRRORS`), the candidate loop skips `s3://` per candidate
(#rref("fetcher.divergence.s3-transport")), and the HTTP client serves only
http(s) --- so anything outside the set is either fragmented in transit or
dead weight that burns the per-candidate retry ladder. r17 merged_bug_003
found three independent spellings of this constraint (CEL, reconciler,
candidate loop) with three different accept-sets: the CEL admitted Unicode
the reconciler dropped, and the reconciler passed schemes the candidate loop
skipped, each layer mislabeling the population the next one saw. The single
defining pattern lives in `rio-crds` (`HASHED_MIRROR_URL_PATTERN`); its two
derivations are pinned equal axis-by-axis, the generated schema is pinned to
carry the rule verbatim, apiserver acceptance is witnessed end-to-end in the
fetcher-split scenario, and a CI deny-grep refuses pattern copies outside
the defining file.

= netrc credential scope

#r("fetcher.fetchurl.netrc-origin-scope")[
  netrc credential resolution MUST consume each fetch candidate's
  provenance: an exact `machine` entry (matching the candidate URL's
  host) MAY authenticate any candidate, but the `default` entry MUST
  only ever authenticate operator-configured mirror candidates ---
  never the derivation's tenant-controlled origin URL. An origin with
  no exact `machine` match is fetched unauthenticated.
]

The origin URL is attacker-chosen in a multi-tenant deployment: with
curl-style optional-netrc semantics (the oracle's
`CURL_NETRC_OPTIONAL`, `filetransfer.cc:566-567`, which applies
machine-and-default matching to every URL), a tenant submits a FOD
pointing at their own server and reads the operator's catch-all
credentials out of the `Authorization` header. Scoping the `default`
entry to mirrors keeps the catch-all secret inside operator-configured
infrastructure while exact `machine` entries remain a per-host opt-in
for authenticated origins. This is a deliberate, recorded divergence
from the oracle, in the same trust split that narrows `impureEnvVars`
sources to the operator-configured map. Accepted residual: per-attempt
HTTP status log lines remain a status oracle for the explicitly
opted-in `machine` hosts.

#r("fetcher.netrc-host-case-fold")[
  netrc `machine` matching MUST compare the entry's host name against
  the candidate URL's host ASCII-case-insensitively, and netrc keyword
  recognition MUST be ASCII-case-insensitive, matching both layers of
  the oracle's delegated parser (`curl_strequal` for keywords,
  `netrc.c:237-318`, and for the host comparison, `netrc.c:264`).
  Credential values MUST be used verbatim, never case-folded.
]

URL parsers normalize the host to lowercase before it reaches
credential lookup, so a byte-equality comparison silently disables
every `machine` entry an operator wrote in upper or mixed case --- the
fetch proceeds unauthenticated and fails with an HTTP status that says
nothing about netrc. Folding at the comparison (not at parse time)
keeps stored values byte-faithful. This is the opposite posture from
fixed-output hash-algorithm spellings, which are case-EXACT
system-wide (#rref("nix.hash.algos+1")): the two axes answer to
different oracles --- DNS-case-insensitive hostnames versus an
exact-set parser --- and MUST NOT share a normalization helper.

#r("fetcher.divergence.netrc-strict-parse")[
  The netrc parser MUST consume each keyword's value in the same step
  as its keyword --- one cursor, no token ever scanned twice, so a
  credential value can never be re-interpreted as a keyword or an
  entry delimiter --- and MUST fail closed, rejecting the whole file
  with a permanent error, on `macdef`, on quoted tokens, and on any
  token that is not a recognized keyword in keyword position. This
  diverges from the oracle's delegated parser, which skips
  unrecognized tokens and accepts macro definitions and quoted
  strings.
]

The leniencies being refused are the oracle's own confusion channels:
a skipped value-carrying keyword feeds its value back into keyword
position (under curl's lexer, `account password login Z` stores
`login` as the password, `netrc.c:290-299`); a `macdef` body ends at a
blank line (`netrc.c:153-156`) that a whitespace tokenizer cannot see,
so tolerating `macdef` would parse macro text as credentials; quoted
tokens carry escape processing (`netrc.c:163-226`) whose silent
mis-split truncates passwords. An operator netrc is a handful of
`machine`/`login`/`password` triples --- rejecting the exotic forms
loudly at first parse beats authenticating with mangled credentials or
phantom `default` entries.

#r("fetcher.netrc.delivery-unwired")[
  netrc credentials are NOT yet an operator-reachable capability:
  every production `SandboxOptions` construction MUST pass `netrc:
  None`, and no binary crate's `Config` may expose a netrc key.
  Wiring the capability MUST land as one change carrying: a file-path
  secret delivery following the `ca_bundle` pattern (a mounted secret,
  never inline config), an `impl` annotation on the producing knob
  that moves this rule out of the uncovered set and rewrites it as the
  delivery contract, and the origin-scope, case-fold, and strict-parse
  rules above exercised against the operator-delivered file.
]

This rule is DELIBERATELY left without an implementation annotation:
it names an absence, and its standing entry in `tracey query
uncovered` is the machine-visible reminder that the parser above is
gate-level code with no production delivery path --- correct under
test, unreachable in deployment. The committed config-schema snapshot
doubles as the tripwire: a netrc key appearing in the builder `Config`
fails the schema test whose failure message is the wiring checklist,
so the knob cannot land silently, partially, or without revisiting the
credential-scope rules.

#r("fetcher.divergence.s3-transport")[
  `s3://` URLs are not supported as a fetch transport (divergence from
  the oracle, which links aws-sdk). The limitation MUST be applied per
  candidate, never per fetch: an `s3://` candidate is skipped with a
  log line and without consuming attempt or backoff budget, while the
  remaining candidates --- in particular operator-configured hashed
  mirrors --- are still consulted. A fetch whose every candidate was
  skipped MUST fail with an error naming the unsupported scheme.
]

The per-candidate scope is what keeps the divergence harmless in the
population that actually hits it: a mirror-fronted pool whose
derivations carry `s3://` origin URLs (nixpkgs has them) builds
exactly as under the oracle, because the oracle too serves the content
from `hashed-mirrors` before consulting the origin. Only a fetch that
NEEDS the s3 transport --- no mirror serves the hash --- observes the
divergence, as a clean rejection instead of a download.

= Transfer contract

#r("fetcher.fetchurl.transfer-cap+2")[
  Every byte path of a fetch attempt MUST be metered against ONE typed
  transfer budget shared by all of the attempt's phases: the HTTP body
  charge and the DECOMPRESSED-restore charge (the dimension a
  decompression bomb amplifies) draw from the same meter, so the
  aggregate an attempt moves --- and the compressed-plus-restored
  payload it can co-occupy on disk --- never exceeds 1× the cap. A
  later phase MUST NOT receive a fresh budget. Budget exhaustion is a
  typed, permanent-for-candidate failure: never silent truncation
  (which would surface as a misleading FOD hash mismatch), and never a
  transient retry (the same candidate serves the same over-budget
  payload every time). A truncated body remains transient ---
  truncation is the connection's fault and a retry can succeed;
  exhaustion is the payload's nature.
]

The plain path is capped on purpose: the previous shape exempted it
("the HTTP body bounds itself --- the server cannot amplify"), but the
origin URL is tenant-controlled, so the server IS the adversary and
can stream arbitrarily many body bytes regardless of any header. The
single shared budget closes the same rule's second hole (round-16
bug_052): the unpack path's restore previously minted an independent
full budget, so one attempt could move 2× the documented bound and
hold compressed + restored payloads totalling 2× on disk
simultaneously.

#r("fetcher.fetchurl.transfer-progress")[
  Long transfers MUST emit a progress line on build stderr at a fixed
  byte cadence (16 MiB), so the sandbox pty --- which feeds the
  max-silent activity watch --- observes activity for as long as bytes
  genuinely flow. Any transfer sustaining at least
  #raw("PROGRESS_INTERVAL_BYTES / max_silent") (≈28 KiB/s at the 600s
  default) survives the silence policy; a fully stalled connection is
  partitioned off earlier by the HTTP client's idle read timeout
  (transient, candidate retried).
]

This restores a property of the oracle rather than copying its
mechanism: CppNix's curl layer drives its logger from the transfer's
progress callback (`filetransfer.cc`'s `XFERINFOFUNCTION` feeding the
JSON logger), so a slow `fetchurl` never trips its silence handling.
rio's fetch runs inside the same sandbox contract as any build
(#rref("fetcher.fetchurl.sandboxed")), so the equivalent signal is
plain stderr lines feeding the activity watch
(#rref("builder.exec.limits-isolated+2")). Transfers alive but slower
than the cadence floor are deliberately treated as silent: at that
rate a 100 MiB source takes over an hour, and the operator's
max-silent policy --- not the fetcher --- owns that call.

#r("fetcher.fetchurl.attempt-atomic")[
  A failed fetch attempt MUST leave nothing at the output path: the
  cleanup scope is the WHOLE fallible finalize (both materialization
  branches and every step after them, including the executable chmod),
  enforced by an RAII guard armed at finalize entry and disarmed only
  on the fully-successful path. A stranded output would poison the
  next candidate's attempt (restore onto an existing path fails) or
  reach the FOD hash gate half-finalized.
]

CppNix's builtinFetchurl never retries within the builtin after
materialization --- a post-restore failure fails the whole build ---
so the oracle has no need for this invariant; rio retries the next
candidate in-process, which makes attempt-atomicity rio-owned. The
guard form (failure scope = cleanup scope) exists because the
previous, branch-local hand cleanup was exactly one fallible step too
narrow: a chmod failure after a successful unpack stranded the
fully-restored tree.

#r("fetcher.fetchurl.permanence-at-source")[
  Every fetch failure MUST be classified transient or
  permanent-for-candidate at the statement that produces it, with the
  classification derived from that statement's own failure mode --- a
  boundary `map_err` over a composite multi-source call is forbidden
  (it re-creates the default bucket the closed permanence enum exists
  to eliminate). Where one statement's error value interleaves several
  sources (the unpack restore: payload decode, worker-filesystem I/O,
  budget exhaustion), the producing function MUST discriminate them
  structurally --- typed exhaustion by downcast, worker-local
  filesystem faults by errno presence --- never by matching error
  text. A deterministic precondition failure (an https candidate in a
  sandbox with no CA roots, where no attempt can ever verify a
  certificate) is permanent for the candidate even when it surfaces
  through a normally-transient transport step.
]

The motivating regression (round-16 merged_bug_068) had both polarity
errors at once: a blanket `map_err(PermanentForCandidate)` on the
whole finalize denied retries to transient `ENOSPC`/`EIO` restore
faults that the identical download-phase fault would have retried,
while every `send()` error --- including the deterministic no-roots
https case the error text itself special-cased --- burned the full
attempt-and-backoff ladder as "transient". Errno presence is the
decode/fs discriminator because the decode-side wrappers (xz
`InvalidData`, the metered-read cap wrapper) carry no OS errno, while
genuine worker filesystem faults always do.

The cadence and the cap are pinned at the unit level (injected-writer
cadence, over-cap permanence on both paths, bomb no-retry,
truncated-transient). The end-to-end composition --- progress line →
sandbox pty → activity watch → no silence kill during a slow genuine
transfer --- needs a per-pool `max_silent` override that `Pool.spec`
cannot express yet (worker pods get a fixed env list).
// TODO: add Pool.spec worker-env (or maxSilentTime) override, then a
// fetcher-split subtest: rate-limited ~192 MiB blob, pool max-silent
// shorter than the transfer, structural success-assert with ≥3×
// cadence headroom. Tracked as the C8c3 staged follow-up (round-15
// plan §4.5).

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
`rio_builder_*`; #(refs.metric)("rio_scheduler_queue_depth") and
#(refs.metric)("rio_scheduler_utilization") gained a `{kind}` label
(`builder`/`fetcher`) to track the split. The fetcher @seccomp
profile may be too strict for exotic fetchers (git-lfs, Mercurial, Subversion)
--- the profile starts as builder-profile-plus-denies and the allowlist widens
as real FODs hit denied syscalls. The VM test suite includes at least one
git-based FOD to catch this early.
