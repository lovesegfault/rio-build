// Project-wide glossary entries. Three groups:
//   "Notation"     — math symbols (ADR-023; rendered via print-glossary in §Notation)
//   "Rio concepts" — rio/Nix concepts (ADR-023 preamble + project glossary)
//   "Terms"        — acronyms (rendered as an appendix before References)
//
// Body text references entries with `@key`; glossarium back-links each
// entry to every page it's cited on. The top-level Glossary chapter
// (docs/glossary.typ) prints all entries with `show-all: true`.
#import "/lib/refs.typ": refs

#let notation = (
  (
    (
      key: "c",
      short: $c$,
      description: [allocated cores (the control variable)],
    ),
    (
      key: "cstar",
      short: $c^*$,
      description: [the chosen allocation: smallest $c$ satisfying the tier envelope],
    ),
    (
      key: "Tc",
      short: $T(c)$,
      description: [predicted median build duration at $c$ cores (reference-seconds)],
    ),
    (
      key: "SPQ",
      short: [$S$, $P$, $Q$],
      description: [serial floor; parallelizable work; @usl coherence term — fitted],
    ),
    (
      key: "pbar",
      short: $macron(p)$,
      description: [observed parallelism cap (p90 of `avg_cores`)],
    ),
    (
      key: "copt",
      short: $c_"opt"$,
      description: [$sqrt(P slash Q)$, the @usl throughput peak ($oo$ at $Q=0$)],
    ),
    (
      key: "tmin",
      short: $T_"min"$,
      description: [$T(min(macron(p), c_"opt"))$, best achievable duration],
    ),
    (
      key: "Mc",
      short: [$M(c)$; $a$, $b$],
      description: [predicted peak memory; its log-linear fit parameters],
    ),
    (
      key: "D",
      short: $D$,
      description: [predicted peak ephemeral-storage (scalar)],
    ),
    (
      key: "wi",
      short: $w_i$,
      description: [per-sample weight (recency × version-distance decay)],
    ),
    (
      key: "neff",
      short: $n_"eff"$,
      description: [Kish effective sample size $(sum w_i)^2 slash (sum w_i^2)$],
    ),
    (
      key: "sigma",
      short: $sigma$,
      description: [std. dev. of log-residuals $ln("obs" slash "fit")$],
    ),
    (
      key: "h",
      short: $h$,
      description: [a hardware class `{manufacturer, generation, storage}`],
    ),
    (
      key: "factorh",
      short: $bold("factor")[h]$,
      description: [per-$h$ performance-factor vector $in RR^K$ (`alu`, `membw`, `ioseq`); scalar before phase 13a],
    ),
    (
      key: "alphapname",
      short: $bold(alpha)["pname"]$,
      description: [per-pname hardware-mixture weights $in Delta^(K-1)$],
    ),
    (
      key: "Kdim",
      short: $K$,
      description: [microbench dimension count ($= 3$)],
    ),
    (
      key: "Hset",
      short: $H$,
      description: [the configured `sla.hwClasses` minus ICE-masked cells],
    ),
    (
      key: "Aset",
      short: $A$,
      description: [admissible $(h, "cap")$ set for an intent],
    ),
    (
      key: "tauset",
      short: $tau$,
      description: [`sla.hwCostTolerance` — modeled-cost slack for $A$],
    ),
    (
      key: "epsh",
      short: $epsilon_h$,
      description: [`sla.hwExploreEpsilon` — per-dispatch $h$-pin probability],
    ),
    (
      key: "biash",
      short: $"bias"["pname", h]$,
      description: [per-pname residual correction for $h$ (post-$bold(alpha)$ rank-$K$ residual)],
    ),
    (
      key: "lambdah",
      short: $lambda[h]$,
      description: [spot-interruption rate for $h$ (interruptions/sec)],
    ),
    (
      key: "p",
      short: $p$,
      description: [per-attempt interruption probability $1 - e^(-lambda T)$],
    ),
    (
      key: "thetahat",
      short: $hat(theta)$,
      description: [any fitted model parameter, in the partial-pooling blend],
    ),
    (
      key: "span",
      short: [span],
      description: [$max(c) slash min(c)$ over a key's samples],
    ),
  )
    .enumerate()
    .map(((i, e)) => (
      e
        + (
          group: "Notation",
          // glossarium sorts on `sort` (default: alphabetical by short); zero-pad
          // the definition index so the printed order matches the table above.
          sort: if i < 10 { "0" + str(i) } else { str(i) },
        )
    ))
)

#let rio-concepts = (
  (
    key: "drv",
    short: [derivation],
    description: [Nix's hermetic build recipe — a content-addressed description of inputs, build script, and environment. One derivation = one build job; "drv" for short.],
  ),
  (
    key: "operator",
    short: [operator],
    description: [The cluster admin who deploys rio and sets policy (SLA tiers, resource ceilings, headroom). Distinct from a @tenant, who submits builds.],
  ),
  (
    key: "pname",
    short: `pname`,
    description: [The Nix package name (`drv.env["pname"]`, e.g. `"chromium"`). Stable across versions and rebuilds; the primary key the model accumulates samples under.],
  ),
  (
    key: "tenant",
    short: `tenant`,
    description: [A rio auth principal (API token / org). Builds are billed and isolated per tenant; the model is keyed `(pname, system, tenant)` so one tenant cannot poison another's curves.],
  ),
  (
    key: "build_samples",
    short: `build_samples`,
    description: [Existing PostgreSQL table: one row per completed build with cgroup-measured `wall_secs`, `cpu_seconds`, `peak_mem`, `cpu_limit`. The sole data source for every fit in this ADR.],
  ),
  (
    key: "karpenter",
    short: "Karpenter",
    description: [The Kubernetes node autoscaler. rio uses its cloud-provider layer only: rio's controller creates *NodeClaim* CRs directly (instance-type/@captype requirements + an *EC2NodeClass* for AMI/subnet/IAM), Karpenter resolves each to an `ec2:CreateFleet` call, and rio owns deletion. Karpenter's reactive provisioner (Pending-pod → NodePool) and disruption controller are bypassed.],
  ),
  (
    key: "system",
    short: `system`,
    description: [The Nix platform string (`x86_64-linux`, `aarch64-linux`). Part of the model key alongside @pname and @tenant.],
  ),
  (
    key: "captype",
    short: [capacity type],
    description: [EC2 purchase mode: *on-demand* (fixed price, never reclaimed) or *spot* (\~0.3× price, may be interrupted with 2min notice). The percentile-envelope shape is what drives this choice.],
  ),
  (
    key: "supervisor",
    short: [supervisor],
    description: [The trusted per-pod rio agent that runs the @drv inside a sandbox and reads cgroup counters _outside_ it. Distinct from the untrusted build payload.],
  ),
  (
    key: "estimator",
    short: [Estimator],
    description: [The scheduler's in-memory cache of `FittedParams` per key — fields: $S, P, Q, macron(p), c_"opt", sigma, n_"eff"$, span, frozen, max_c, min_c, saturated, last_wall, bootstrap CI. Populated on the completion-ingest path; read on every dispatch.],
  ),
  (
    key: "scheduler",
    short: [scheduler],
    description: [The rio control-plane service that owns the @estimator and emits PodSpecs — distinct from kube-scheduler.],
  ),
  (
    key: "controller",
    short: [controller],
    description: [The rio reconciler that watches Nodes/NodeClaims and writes @pg; runs separately from the @scheduler.],
  ),
  (
    key: "pool",
    short: [#(refs.crd)("Pool")],
    plural: [#(refs.crd)("Pool")s],
    description: [k8s CRD declaring an executor pool ---
      `spec.{`#(refs.crd-field)("Pool", "systems"),
      #(refs.crd-field)("Pool", "features"),
      #(refs.crd-field)("Pool", "privileged"),
      #(refs.crd-field)("Pool", "hostNetwork")`, …}` per
      `rio-crds/src/pool.rs`.],
  ),
  (
    key: "aterm",
    short: [ATerm],
    description: [The serialization format used for `.drv` files. A simple S-expression-like format: `Derive([outputs], [inputDrvs], [inputSrcs], platform, builder, [args], [env])`.],
  ),
  (
    key: "closure",
    short: [closure],
    description: [The transitive set of all store paths required by a given store path, including itself and all its runtime dependencies. A closure is self-contained: copying it to another machine provides everything needed.],
  ),
  (
    key: "store-path",
    short: [store path],
    description: [A path in `/nix/store/` identified by a hash and a name, e.g. `/nix/store/aaaa...-hello-2.12.1`. The hash encodes the derivation inputs (input-addressed) or output content (content-addressed).],
  ),
  (
    key: "modular-hash",
    short: [modular derivation hash],
    description: [The hash computed by `hashDerivationModulo` in Nix. For input-addressed derivations, this includes the derivation's full inputs. For CA derivations, it excludes output paths and depends only on the derivation's fixed attributes. Used for deduplication in DAG merging.],
  ),
  (
    key: "narinfo",
    short: [narinfo],
    description: [A text file describing a store path's metadata: store path, NAR hash, NAR size, references, deriver, signatures. Served by binary caches at `/<hash>.narinfo`.],
  ),
  (
    key: "nixbase32",
    short: [nixbase32],
    description: [Nix's custom base32 encoding (characters: `0123456789abcdfghijklmnpqrsvwxyz` --- note missing `e`, `o`, `t`, `u`). Used in store path hashes.],
  ),
  (
    key: "fastcdc",
    short: [FastCDC],
    description: [Fast Content-Defined Chunking. An algorithm that splits byte streams into variable-size chunks at content-dependent boundaries. Enables deduplication even when data shifts within a file.],
  ),
  (
    key: "blake3",
    short: [BLAKE3],
    description: [A fast cryptographic hash function used internally by rio-build for chunk content addressing. Not used for Nix-facing hashes (those use SHA-256).],
  ),
  (
    key: "worker-protocol",
    short: [worker protocol],
    description: [The Nix daemon's RPC protocol, spoken over SSH channels. Uses little-endian u64 integers, padded strings, and a STDERR streaming loop for progress/log reporting.],
  ),
  (
    key: "stderr-loop",
    short: [STDERR loop],
    description: [The Nix protocol's mechanism for streaming progress, logs, and errors during an operation. The server sends `STDERR_NEXT`, `STDERR_START_ACTIVITY`, etc. messages until `STDERR_LAST` signals the operation result follows.],
  ),
  (
    key: "build-hook",
    short: [build hook],
    description: [Nix's mechanism for delegating builds to remote machines via `--builders`. The local daemon invokes a hook program that connects to the remote builder. rio-build supports this mode but with reduced scheduling optimization (no DAG visibility).],
  ),
  (
    key: "overlayfs",
    short: [overlayfs],
    description: [A Linux union filesystem that layers a writable "upper" directory over one or more read-only "lower" directories. rio-build uses a single FUSE lower (the chunk-store mount) under a tmpfs upper, mounted at `/nix/store` in the child's mount namespace --- the build sees ONLY its declared inputs (`r[builder.overlay.stacked-lower]`). The nix-daemon subprocess runs in a chroot store outside the overlay.],
  ),
  (
    key: "seccomp",
    short: [seccomp],
    description: [Secure Computing Mode. A Linux kernel feature that restricts which system calls a process can make. The controller sets `PoolSpec.seccompProfile` (default `Localhost` profile denying `bpf`/`setns`/`process_vm_writev`; the read-side trace syscalls stay allowed for sanitizer/debugger check phases, Yama-confined to descendants) plus granular capabilities (`SYS_ADMIN`, `SYS_CHROOT`) instead of `privileged: true`.],
  ),
  (
    key: "networkpolicy",
    short: [NetworkPolicy],
    description: [A Kubernetes resource that controls network traffic between pods. rio-build uses NetworkPolicies to restrict executor egress to only the scheduler and store, blocking access to the Kubernetes API and cloud metadata services.],
  ),
  (
    key: "write-ahead-manifest",
    short: [write-ahead manifest],
    description: [rio-store's pattern for durable writes: chunk references are written to a pending manifest before uploading chunks, then promoted to committed after all chunks are verified. Protects against orphaned chunks and broken manifests.],
  ),
  (
    key: "inline-storage",
    short: [inline storage],
    description: [A fast-path in rio-store for NARs below 256 KiB (`INLINE_THRESHOLD`, compile-time const) that bypasses FastCDC chunking. Stored directly in the PostgreSQL `manifests.inline_blob` BYTEA column --- inline blobs *never touch S3*.],
  ),
  // build-request / build-derivation: removed — disambiguator entries
  // ("build (request)" vs "build (derivation)") that no prose references.
  // The distinction is made inline where it matters (e.g., gateway.typ's
  // SubmitBuild flow).
  (
    key: "blob",
    short: [blob],
    description: [An opaque binary object. In rio-store: chunks (FastCDC pieces keyed by BLAKE3) live in S3; inline NARs (\< 256 KiB) live in a PostgreSQL BYTEA column, not S3.],
  ),
  (
    key: "derivedpath",
    short: [DerivedPath],
    description: [A Nix type representing either a plain store path (Opaque) or a derivation output reference (Built). Format: `drvPath!output1,output2` or `drvPath!*` for all outputs.],
  ),
  (
    key: "singleflight",
    short: [singleflight],
    description: [A concurrency pattern where multiple concurrent requests for the same key share a single in-flight operation. Used in rio-store to avoid duplicate S3 fetches.],
  ),
  (
    key: "leader-election",
    short: [leader election],
    description: [A distributed coordination pattern where one instance is selected as the active leader. rio-build uses Kubernetes Lease objects (via `rio_lease`) for #(refs.leased-components)() leader election; the scheduler lease ensures only one instance owns the in-memory DAG at a time.],
  ),
  (
    key: "gc-root",
    short: [GC root],
    description: [A store path that is protected from garbage collection. In rio-build, GC roots include outputs of active/queued builds, paths referenced by `wopAddTempRoot`, and paths within the configured retention period.],
  ),
  (
    key: "assignment-token",
    short: [assignment token],
    description: [An HMAC-SHA256-signed token issued by the scheduler when dispatching work to an executor. Claims: `(executor_id, drv_hash, expected_outputs, is_ca, is_fixed_output, tenant, expiry_unix)` — the optional fields use serde defaults, and `is_fixed_output` is only emitted when the scheduler's `sign_fod_claims` rollout gate is armed. Verified by rio-store on `PutPath`.],
  ),
  (
    key: "prefetch-hint",
    short: [prefetch hint],
    description: [A message sent by the scheduler to an executor via the `BuildExecution` stream before assigning a build, listing input closure paths that the executor's FUSE cache should pre-warm. Converts serial "fetch then build" into overlapped execution.],
  ),
  (
    key: "poison-derivation",
    short: [poison derivation],
    description: [A derivation that has consistently failed on multiple different executors (default threshold: 3 distinct executors). Marked as poisoned to prevent infinite retry loops. Auto-expires after a configurable TTL (default: 24h).],
  ),
  (
    key: "dag-actor",
    short: [DAG actor],
    description: [The single-owner Tokio task in the scheduler that owns the in-memory global DAG. All DAG mutations (merges, completions, cancellations) are processed sequentially via an `mpsc` channel, eliminating lock contention.],
  ),
  // rio-fuse: removed — never appears as a concept name in prose (only as
  // the `/var/rio/fuse-store` mount path); the `fuse` key covers the
  // filesystem concept and builder.typ §FUSE describes the module.
  (
    key: "temp-root",
    short: [temp root],
    description: [A connection-scoped temporary GC root registered via `wopAddTempRoot`. Prevents GC of store paths that a client is actively using during an SSH session. Lost on gateway restart; the store's #(refs.const)("DEFAULT_GC_GRACE_HOURS")-hour GC grace period provides safety.],
  ),
  (
    key: "backpressure",
    short: [backpressure],
    description: [A flow control mechanism where a downstream component signals upstream to slow down. In rio-build, the scheduler applies backpressure via gRPC flow control windows and bounded actor queue depth.],
  ),
).map(e => e + (group: "Rio concepts"))

#let terms = (
  (key: "sla", short: "SLA", long: "Service-Level Agreement"),
  (
    key: "ice",
    short: "ICE",
    long: "Insufficient Capacity Error",
    description: [AWS EC2's signal that no instance of the requested type is available in the requested AZ; the trigger for the §Hardware-class targeting fallback ladder.],
  ),
  (key: "az", short: "AZ", long: "Availability Zone"),
  (
    key: "nnls",
    short: "NNLS",
    long: "Non-Negative Least Squares",
    description: [Constrained least-squares solved by the Lawson–Hanson active-set method.],
  ),
  (
    key: "usl",
    short: "USL",
    long: "Universal Scalability Law",
    description: [Gunther's three-parameter throughput model with a coherence term for retrograde scaling.],
  ),
  (
    key: "mad",
    short: "MAD",
    long: "Median Absolute Deviation",
    description: [Robust scale estimator; $1.4826 dot.op "MAD"$ is a consistent estimator of $sigma$ under normality.],
  ),
  (
    key: "aicc",
    short: "AICc",
    long: "corrected Akaike Information Criterion",
    description: [Small-sample-corrected model-selection criterion; $Delta"AICc" < -2$ favors the larger model.],
  ),
  (key: "ema", short: "EMA", long: "Exponentially-Weighted Moving Average"),
  (key: "cdf", short: "CDF", long: "Cumulative Distribution Function"),
  (
    key: "fod",
    short: "FOD",
    long: "Fixed-Output Derivation",
    description: [A Nix derivation whose output hash is declared in advance; typically a network fetch.],
  ),
  (key: "oom", short: "OOM", long: "Out-Of-Memory"),
  (key: "vpa", short: "VPA", long: "Vertical Pod Autoscaler"),
  (key: "irsa", short: "IRSA", long: "IAM Roles for Service Accounts"),
  (key: "pg", short: "PG", long: "PostgreSQL"),
  (key: "crd", short: "CRD", long: "Custom Resource Definition"),
  (key: "lto", short: "LTO", long: "Link-Time Optimization"),
  (
    key: "sita",
    short: "SITA-E",
    long: "Size-Interval Task Assignment with Equal load",
    description: [Queueing-theoretic dispatch policy: route jobs to size-segregated servers so short jobs never wait behind long ones.],
  ),
  (
    key: "nar",
    short: "NAR",
    long: "Nix ARchive",
    description: [A deterministic archive format for serializing store paths. Unlike tar, NAR produces identical output for identical directory trees regardless of filesystem metadata (timestamps, permissions).],
  ),
  (
    key: "cas",
    short: "CAS",
    long: "Content-Addressable Store",
    description: [A storage system where objects are identified by the hash of their content. Enables deduplication: identical content is stored once regardless of how many paths reference it.],
  ),
  (
    key: "ca",
    short: "CA",
    long: "Content-Addressed",
    description: [A derivation whose output store path is determined by the _content_ of the output, not by its inputs. Enables early cutoff: if rebuilding produces the same output, downstream builds can be skipped.],
  ),
  (
    key: "ifd",
    short: "IFD",
    long: "Import-From-Derivation",
    description: [When Nix evaluation depends on a build result (e.g., `import (pkgs.runCommand ...)`). The evaluator blocks until the build completes, creating a tight eval-build dependency cycle.],
  ),
  (
    key: "dag",
    short: "DAG",
    long: "Directed Acyclic Graph",
    description: [The dependency structure of derivations --- each derivation depends on its inputs, forming a graph with no cycles. rio-build's scheduler operates on the global DAG of all concurrent builds.],
  ),
  (
    key: "fuse",
    short: "FUSE",
    long: "Filesystem in Userspace",
    description: [A Linux kernel interface that allows implementing filesystems in user-space programs. rio-build uses FUSE (via the `fuser` crate) to present a lazy-fetching store view at `/var/rio/fuse-store`, backed by remote content from rio-store.],
  ),
  (
    key: "mtls",
    short: "mTLS",
    long: "Mutual TLS",
    description: [rio-build does *not* use application-level mTLS --- transport encryption is handled by Cilium WireGuard at L3; rio components speak plaintext gRPC over the encrypted overlay.],
  ),
  (
    key: "imdsv2",
    short: "IMDSv2",
    long: "Instance Metadata Service version 2",
    description: [AWS's token-based metadata endpoint. Setting hop limit=1 on worker nodes prevents containers from accessing node metadata, a defense-in-depth measure.],
  ),
  (
    key: "pdb",
    short: "PDB",
    long: "PodDisruptionBudget",
    description: [A Kubernetes resource that limits how many pods can be simultaneously unavailable during voluntary disruptions (e.g., node drain). rio-build creates PDBs to maintain minimum build capacity.],
  ),
  (
    key: "nlb",
    short: "NLB",
    long: "Network Load Balancer",
    description: [An AWS L4 load balancer used for the gateway's SSH ingress. Configured with extended idle timeout (3600s) to support long-running build sessions.],
  ),
  (
    key: "hpa",
    short: "HPA",
    long: "Horizontal Pod Autoscaler",
    description: [A Kubernetes built-in autoscaling mechanism. rio-build does NOT use HPA for builders --- builder pods are one-shot Jobs spawned/reaped by rio-controller based on scheduler queue depth.],
  ),
).map(e => e + (group: "Terms"))

#let glossary-entries = rio-concepts + notation + terms
