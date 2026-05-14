// Glossary entries for ADR-023. Two groups:
//   "Notation" — math symbols (rendered via print-glossary in §Notation)
//   "Terms"    — acronyms (rendered as an appendix before References)
//
// Body text references entries with `@key`; glossarium back-links each
// entry to every page it's cited on.

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
    key: "builderpool",
    short: [`BuilderPool`],
    description: [The CRD that scopes a builder pool's `nodeClassRef`, ephemeral budgets, and `spec.sizing: Static|Sla` opt-out.],
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
).map(e => e + (group: "Terms"))

#let glossary-entries = rio-concepts + notation + terms
