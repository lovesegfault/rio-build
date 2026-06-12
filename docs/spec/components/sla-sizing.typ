#import "/lib/rio.typ": *
#import "/lib/glossary.typ": glossary-entries
#import "/lib/mc/sla-sizing.typ": (
  duration-margin, geom-lognormal-figure, geom-lognormal-max,
  headroom-coverage-figure, lever-arm-figure, lever-arm-ratio,
)
#let dur-margin-pct = calc.round(100 * duration-margin(0.1), digits: 0)
// This chapter prints visible glossary sections (Rio-concepts /
// Notation / Terms), so it owns the `<key>` anchors — tell `rio()` not
// to emit its hidden anchor set.
#provides-glossary()

#show: rio.with(
  domains: ("sched.sla", "sched.admin"),
  paper: (
    title: [SLA-Driven Per-Derivation Sizing],
    supertitle: "ARCHITECTURE DECISION RECORD",
    status: "Accepted",
    date: "2026-04",
    bib: "/lib/bib.yml",
  ),
)

#info(title: [How to read this document])[
  Rio is a Nix remote-build service: clients submit @drv:pl (hermetic build recipes), and Rio runs each in its own ephemeral Kubernetes pod. *This ADR is about choosing the pod size and provisioning the node it runs on.* Today the scheduler replays last run's peak and @karpenter provisions reactively; this proposal makes the scheduler solve for the cheapest allocation that meets an @operator\-declared latency target, and the controller provision capacity ahead of the build DAG's frontier so each layer starts on a warm node.

  The rio terms below are assumed throughout — start with @pname / @system / @tenant (the model key) and @build_samples (the only data source).

  *Evaluating the decision* (in document order): §2.1 Tiered-SLA → §2.12 Threat-model → §3 Alternatives → §4 Consequences. *Mechanism:* §2.2 Coupled-model + @alg-estimate, §2.3 Exploration + @alg-explore, §2.7 Duration distribution + @alg-quantile; §2.8 layers on the multi-hardware cost solve (@alg-estimate-full). MVP = phases 1–5, v1.1 = phases 6–11, v1.2 = phases 12–13 (§Implementation Phasing).
]

#heading(level: 2, numbering: none, outlined: false)[Rio concepts]

#print-glossary(
  glossary-entries,
  groups: ("Rio concepts",),
  show-all: true,
  user-print-group-heading: (..) => [],
  disable-back-references: true,
)

#heading(level: 2, numbering: none, outlined: false)[Notation]

Hatted symbols ($hat(S)$, $hat(theta)$, $hat(sigma)$) denote fitted estimates of the corresponding true quantity.

#print-glossary(
  glossary-entries,
  groups: ("Notation",),
  show-all: true,
  user-print-group-heading: (..) => [],
  user-print-back-references: muted-backrefs,
)

= Context

Today rio sizes each build pod by *replaying the last run's measured peak* CPU and memory, rounded to 4Gi/2-core buckets. The rounding existed so that successive builds of similar shape could share a pod sequentially, but bucketing no longer buys reuse — each pod runs exactly one build, and storage warm-up is now sub-#qty("10", "ms") — so what remains is a quantization step between the scheduler's continuous estimate and @karpenter#"'"s continuous provisioning, costing on average half a bucket per pod for no structural benefit.

More fundamentally, replay-the-peak sizes pods to *fit*, not to *finish on time*. An operator who wants "builds finish within 20 minutes" has no knob; a build that took #qty("45", "min") on 8 cores will be given 8 cores again.

The two problems are linked. Memory is a hard floor (under-provision → @oom). CPU is fungible with wall-clock time: more cores buys a shorter build, up to the build's serial fraction. That makes CPU an optimization variable, not a measurement to replay — and the objective function it optimizes is the operator's latency @sla.

In high-performance-computing literature this is *moldable job scheduling*: the scheduler chooses processor count at submit time from a per-job speedup model under a deadline constraint @benoit2022. Deadline-constrained moldable scheduling is NP-hard globally; per-job solve plus Karpenter bin-packing is a standard decomposition.

= Decision

Replace fit-based sizing with SLA-driven sizing. The operator declares latency tiers; the scheduler learns per-derivation duration and memory as functions of allocated cores, and requests the cheapest `(cpu, mem)` that lands the derivation in its tightest feasible tier.

Rio's existing scheduler→builder sizing pipeline is reused unchanged — this ADR changes what it _carries_, not how it flows.

#figure(
  caption: [Data and compute flow. The slow ingest path writes `FittedParams`; the fast dispatch path only reads it — everything per-key (NNLS refit, bootstrap) happens off the hot path.],
  diagram(
    spacing: (14mm, 10mm),
    node-stroke: 0.5pt,
    node((0, 0), [build\ completes], shape: fletcher.shapes.pill),
    edge("-|>"),
    node((1, 0), [`build_samples`\ (PG)], shape: fletcher.shapes.cylinder),
    edge("-|>", [debounced\ refit], label-size: 0.78em, label-side: left),
    node(
      (2.2, 0),
      [`FittedParams`\ cache],
      fill: accent.lighten(85%),
      name: <fp>,
    ),
    node((0, 1), [drv\ dispatch], shape: fletcher.shapes.pill),
    edge("-|>", [O(1) lookup], label-size: 0.8em, label-side: right),
    node((2.2, 1), [solve $c^*$], name: <solve>),
    edge(<fp>, <solve>, "..>", label-size: 0.8em),
    edge(<solve>, "r", "-|>"),
    node((3.0, 1), [PodSpec]),
    edge("-|>"),
    node((3.8, 1), [Karpenter], shape: fletcher.shapes.hexagon),
  ),
) <fig-flow>

== Tiered SLA

#r("sched.sla.tier-envelope")

The operator configures an ordered list of latency tiers, each a *percentile envelope* rather than a single bound:

```yaml
sla:
  tiers:
    - {name: fast,   p50: 3m,  p90: 5m,  p99: 8m}
    - {name: normal, p50: 12m, p90: 20m, p99: 45m}
    - {name: slow,             p90: 60m}
    - {name: best-effort}
```

Each `(pname, system, tenant)` is assigned to the *tightest tier whose entire envelope is feasible* — i.e., the model predicts every percentile bound can be met at some core count $<=$ `maxCores` (formally: §Duration distribution). A single-bound tier (`{p90: 60m}`) is the degenerate case. Infeasible-at-any-tier derivations fall to `best-effort` and receive $C = min(macron(p), c_"opt", "maxCores")$ cores — the model's best-throughput point, clamped (allocating beyond $c_"opt"$ at @usl stage is retrograde). When the emitted disk request would exceed `maxDisk` the build is infeasible regardless of $c$ and is *failed* with `infeasible_total{reason=disk_ceiling}`, not sent to best-effort.

The envelope shape is what drives the @captype decision: a tight `p99` forces the solve toward on-demand (kills the interruption-retry tail); a loose `p99` with tight `p50` permits cheap spot at high @cstar. Operators express the distribution they will accept; the system chooses capacity type, hardware class, and core count to fit it at minimum expected cost.

== Coupled duration/memory model <coupled-model>

#r("sched.sla.mem-coupled")

#idea(title: [The whole model in one line])[
  $T(c) = S + P slash c + Q c$ is the entire duration model — Amdahl with a coherence penalty. Everything below (column normalization, Kish $n_"eff"$, Student-$t$ leverage, pinball loss) is numerical hygiene to make this three-parameter fit robust at $n_"eff" in [3, 10]$. On first read, skip to *CPU is the single control variable*.
]

#r("sched.sla.model-key-tenant-scoped")

*Duration* is modeled per `(pname, system, tenant)` from `build_samples` as
$ T(c) = S + P / min(c, macron(p)) + Q dot.op min(c, macron(p)) $
where $S$ is the serial floor, $P$ is parallelizable work-seconds, and $Q$ is the @usl coherence term modeling retrograde scaling @gunther2007. With $macron(p) >= c_"max" and Q = 0$ this is Amdahl; with $Q = 0$ alone it is the moldable-scheduling roofline form @benoit2022.

The cap $macron(p)$ is the *observed parallelism* ceiling: the recency-weighted p90 of `avg_cores` over *uncensored* samples only, meaning those with `peak_cpu` $< 0.85 dot.op$ `cpu_limit` where the cap was observable. Average is used rather than peak, so a spin-loop reads as $macron(p) approx "limit"$ only if it sustained; a brief burst does not inflate it. To keep the cap from ratcheting downward against the policy's own past action, *$macron(p)$ is floored at $max{"cpu_limit"_i : "avg_cores"_i >= 0.85 dot.op "cpu_limit"_i and "vdist"_i = 0}$* — the highest cap at which the build was *censored* this epoch, inert when that set is empty. The floor prevents post-convergence samples, which have `avg_cores` $<= c^*$ and are cgroup-censored, from pulling $macron(p)$ down to $c^*$ and falsely capping the feasibility check $c^* <= C$. It does not over-shoot when an unsaturated exploration sample already revealed the true $macron(p)$.

#r("sched.sla.fit-nnls")

The fit is *linear in* ${1, 1 slash min(c, macron(p)), min(c, macron(p))}$, so it remains a single weighted-@nnls solve across all model stages. The §Model-staging $Q$-unfreeze is gated by $Delta"AICc"$, but the *solve* is one @nnls call with progressively unfrozen columns, not three code paths. @nnls forcing $S >= 0$ restricts to the $sigma >= kappa$ regime in Gunther's parameterization, where contention dominates coherence — typical of build workloads, but a regime restriction nonetheless. The design matrix is *column-normalized* before the solve and coefficients denormalized after, since raw column norms differ by \~#mul(200) and without normalization that ratio would dominate the condition number and make the solve numerically unstable.

Each sample carries weight
$ w_i = 0.5^("age"_i slash "halflife") dot.op 0.5^("vdist"_i) $
combining recency decay with *version-distance decay*. The halflife is 20 *samples*, with no wall-clock arm: a key built monthly would otherwise asymptote at $n_"eff" approx 1$ and never leave §Exploration. The $"vdist"$ term is the count of distinct `version` strings observed between the sample and the current build. Version is read from `drv.env["version"]` alongside `pname` (#(refs.gh)("rio-gateway/src/translate.rs:452")), and no semver parsing is done — ordinal distance is robust to date- and git-rev-versioned packages. A major-version rewrite drops prior samples to $<= 50%$ weight immediately and lets the new profile dominate within 2–3 runs, while a patch bump barely perturbs the fit.

This leaves the question of design-matrix rank after convergence, when every fresh sample lands at the same $c^*$. The 32-slot ring buffer reserves *anchor slots* for the widest-span samples: one per distinct `cpu_limit` value, holding the *highest-weight* sample at that value, never displaced by recency, with a *weight floor $w_"anchor" >= 0.5^("vdist") slash n_"anchors"$*. Without the weight floor the anchored rows would recency-decay to numerical zero and the matrix would degenerate to rank-1; the anchors keep it at full rank.

All sample-count gates throughout this ADR use the *Kish effective sample size* @kish1965
$ n_"eff" = (sum_i w_i)^2 / (sum_i w_i^2) $
rather than raw $n$. After a version bump the ring buffer may hold 32 samples but $n_"eff" approx 2$; gating on raw $n$ would wrongly keep the model at the @usl stage. Span-tracking resets when $"vdist"$ jumps.

*Peak memory* $M(c)$ is fit as *p90 quantile regression* on $log M = a + b log c$ once $n_"eff" >= 10$. The fit minimizes the weighted pinball loss $rho_tau (u) = u(tau - bb(1)[u<0])$ at $tau = 0.9$ @koenker2005 rather than sorting and picking the 90th percentile; the solver is neutral, LP @koenker2005 or IRLS both work. Log-linear captures sub/super-linear power-law with two parameters, and p90 is chosen over mean because least-squares puts \~50% of runs above the line and into @oom territory. This matches Autopilot/@vpa practice @rzadca2020 of sizing from a decaying-histogram percentile rather than a point estimate.

Below $n_"eff" = 10$, p90 is not estimable. Instead the model fits ordinary least squares on $log M$ and applies the small-sample prediction-interval factor $exp(t_(0.9, max(3, n_"eff" - 2)) dot.op hat(sigma)_"resid" dot.op sqrt(1 + h_0))$, with leverage
$ h_0 = 1 / (sum w_i) + (log c^* - overline(log c))^2 / S_(x x), $
$S_(x x) = sum w_i (log c_i - overline(log c))^2$, and $overline(log c) := (sum w_i log c_i) slash (sum w_i)$. Because $n_"eff" >= sum w_i$ under sub-unit weights, substituting $1 slash n_"eff"$ for the first term would *narrow* the interval and is therefore anti-conservative. Degrees of freedom are floored at 3: the Student-$t$ factor, not $z = 1.28$, is what widens the interval under extrapolation, which is exactly the post-bump case.

#r("sched.sla.disk-scalar")

*Peak ephemeral-storage* $D$ is the *scalar p90* of observed `peak_disk_bytes` with the same recency × version-distance weighting. Disk does not scale with $c$: $N$ parallel compilers produce the same artifact set as one, so there is no $D(c)$ curve and no exploration interaction. This was validated empirically (@fig-disk-probe) across 6 packages at j4 vs j64, with peak ratio #num("1.000+-0.012") and max Δ #qty("62", "MiB") (internal validation 2026-04-06, c7a.16xlarge; raw data: `023-data/disk-probe.tsv`).

#figure(
  caption: [Peak ephemeral-storage at $j=4$ vs $j=64$ across six packages (`023-data/disk-probe.tsv`). Bars are visually identical per package — disk does not scale with $c$.],
  lq.diagram(
    width: 11cm,
    height: 4.2cm,
    ylabel: [peak disk (GiB)],
    xaxis: (
      ticks: ("openssl", "mesa", "ripgrep", "hugo", "numpy", "zig")
        .enumerate()
        .map(((i, p)) => (i, p)),
      subticks: none,
    ),
    legend: (position: top + left),
    lq.bar(
      range(6).map(i => i - 0.18),
      (1.516, 3.075, 1.502, 4.976, 5.227, 4.855),
      width: 0.36,
      label: [$j = 4$],
    ),
    lq.bar(
      range(6).map(i => i + 0.18),
      (1.516, 3.075, 1.502, 4.916, 5.232, 4.855),
      width: 0.36,
      label: [$j = 64$],
    ),
  ),
) <fig-disk-probe>

#r("sched.sla.disk-reaches-ephemeral-storage+2")

The disk input to `pod_ephemeral_request` MUST be a fitted, typed envelope —
`floor <= fitted <= ceiling` (`DiskFitEnvelope`): the per-pname WITNESSED
disk fit — the observed p90 aggregated over every peaked sample
(probe-shaped fits included), consumed only at a witnessed population
($n >= 3$ observed peaks) and shrink-floored at
$max("p90", "newest peak" times 1.2)$ (live060-c: no shrink below recent
observed reality plus headroom; both constants are R17-violable typed
envelopes carrying the measured-at-first-samples rider) — retires the
`sla.defaultDisk` cold-start prior, the floor is the probe's own scratch
footprint, and the ceiling is `sla.maxDisk`; below the population gate the
prior stands, and a flat default is a cold-start prior that a WITNESSED
population MUST retire, never a steady state.

live060-c (the activation-safety amendment): the live builder fleet
(EBS-only ext4, no prjquota) has never produced a disk observation — every
completion records a NULL peak — so the pre-fix "any observation retires
the prior" law was vacuously safe and would have become a single-sample
fleet-wide collapse hazard the moment provisioning (live060-a) landed: the
first sparse, unrepresentative peaks would have retired the prior with no
population evidence. The gate lives inside the ONE warm-fit producer
(`aggregate_disk_p90`), so every consumer — the envelope's `fitted`, the
`exceeds_ceiling` reject gates, the explore lanes — is witnessed by
construction; the close lands INERT on the unprovisioned fleet
($n = 0 < 3$) and the land-order pin (live060-c at wave position 5,
live060-a at position 8) is safe in both directions.

Enforcement (round-10, bug_132/R24 — commentary, not an additional
requirement): the rule above is carried by construction. `DiskRequest` is a
newtype mintable only by `DiskFitEnvelope`, and every dispatch lane's output
type (`IntentDecision`, `SolveResult`, `AdmissibleSet`,
`SolveFullResult::BestEffort`, `ExploreDecision`) carries it, so a raw
`disk_p90` projection no longer type-checks at any emission seam — rustc is
the lane census (the wave-8 implementation wired the envelope into the
explore lane only and left the solve lanes floor-less open-coded; the lane
divergence is now unrepresentable). The Feasible reject gates keep reading
the *witnessed* observation through one shared predicate
(`exceeds_ceiling`) — `fit.D > maxDisk` is the genuine c-invariant "cannot
fit" gate per @alg-estimate, gated and floored by the producer like every
other consumer (live060-c), and the clamped request by construction can
never trip it;
`sla explain`'s `disk-ceiling` label reads the same predicate, so the explain
surface mirrors the solve gates by construction.

Measurement reads kubelet's own XFS/ext4 *project quota* on the emptyDir, a node-local scratch volume. Kubelet (`LocalStorageCapacityIsolationFSQuotaMonitoring=true`) assigns the project ID; the @supervisor scrapes it via `FS_IOC_FSGETXATTR` on the dir inode and reads usage with `quotactl_fd()`. This requires Linux $>= 5.14$ and piggybacks the @supervisor#"'"s existing `CAP_SYS_ADMIN` from the overlay mount; the older `quotactl()` needs a block-device path the container cannot see. The read is kernel-tracked $O(1)$ and polled in the #qty("1", "Hz") `cpu_poll` loop. On the gp3-root pool today — `-o prjquota` lands with the NVMe-backed EC2NodeClass in v1.1 — kubelet falls back to \~#qty("60", "s") `du` polls and $D$ is *recorded as NULL*. The no-neighbor-eviction guarantee below holds only on the prjquota-enabled pool, and the request falls back to `sla.defaultDisk`. A statvfs-delta fallback was rejected because other pods on the same node would cross-contaminate it. The build tempdir lands inside this subtree by default — nix ≥2.30 sets `build-dir = ${stateDir}/builds` — so the quota captures intermediate object files, not just outputs.

ENOSPC — kubelet `DiskPressure` eviction or `StorageFull` from rio's read-through store cache — is classified like @oom: penalty-bump @D to #mul(1.5) observed and automatically retry. The `overlays` emptyDir `sizeLimit` $= D dot.op "headroom"(n_"eff")$, and the pod's `resources.{requests,limits}.ephemeral-storage` $= D dot.op "headroom" + "fuseCacheBytes" + "LOG_BUDGET_BYTES"$. Limits equal requests with no burst, per project policy; the pod limit must strictly exceed the sum of emptyDir sizeLimits plus container-log/writable-layer slack. A build that overshoots is therefore evicted itself rather than triggering node-level `DiskPressure` that could evict a neighbor. Karpenter selects nodes with sufficient local storage, closing a pre-existing gap where size-class storage set emptyDir `sizeLimit` but never reached the pod's resource requests (#(refs.gh)("rio-controller/src/reconcilers/pool/pod.rs:75")).

CPU is the single control variable; memory and disk are derived. Allocation: solve for the smallest @cstar satisfying the tier's envelope (§Duration distribution). Under the scalar-tier MVP this reduces to $T(c) = e^(-z_q hat(sigma)) dot.op "p90"_"bound"$ with
$
  z_q = t_(q, max(3, min(n_"eff", n_"distinct-c") - n_"par")) dot.op sqrt(1 + 1 slash sum w_i),
$
where $n_"par" in {2, 3}$ is the model's parameter count and $n_"distinct-c"$ is the count of distinct `cpu_limit` values. Both $n_"eff"$ and $n_"distinct-c"$ must bind: Kish $n_"eff"$ measures weight dispersion, not design-point support, and post-convergence the latter is the limiting quantity.

$z_q$ is the Student-$t$ prediction-interval factor evaluated at the design centroid. Evaluating at the centroid keeps $beta$ constant, so the closed-form quadratic below holds; @alg-quantile uses the same centroid $z_q$ as a fixed input, accepting under-coverage at extrapolation in exchange for not recomputing $h_0 (c)$ per bisection step. At large $n_"eff"$ with $n_"distinct-c"$ unbounded, $z_q -> Phi^(-1) (q) = 1.2816$; in practice $n_"distinct-c"$ binds post-convergence. At $n_"eff" = 3$, df $= max(3, n_"eff" - 2) = 3$ gives $t_(0.9, 3) sqrt(4 slash 3) approx 1.89$ — wider than asymptotic $z = 1.28$, narrower than the textbook $t_(0.9, 1) = 3.08$. The df floor trades small-sample conservatism against the $n_"eff" >= 3$ gate already excluding $"df" < 1$.

#r("sched.sla.solve-citardauq")

The solve is linear in $c$ at $Q = 0$ and quadratic otherwise. Apply the cancellation-free quadratic form @higham2002[§1.8] *only when $beta < 0 and beta^2 >= 4 Q P$*; otherwise the tier is infeasible — discriminant negative or target below the serial floor:

$
  c^* = P / q, wide q = -1/2 (beta + sign(beta) sqrt(beta^2 - 4 Q P)), wide beta = S - e^(-z_q hat(sigma)) dot.op "p90"_"bound"
$

which degenerates to the Amdahl solution as $Q -> 0$. *Reject the tier as infeasible* if $c^* > C := min(macron(p), c_"opt", "maxCores")$ (@alg-estimate's $P slash q <= C$ check; clamping would silently miss the bound), where $c_"opt" := sqrt(P slash Q)$ if $Q > 0$ else $+oo$ is the @usl throughput peak. Otherwise clamp the lower end to 1 and request $(c^*, M(c^*) dot.op "headroom"(n_"eff"))$.

#r("sched.sla.solve-reject-not-clamp")

#memo(title: [Implementer gotcha])[
  The $c^* <= C$ check is *not* a clamp — clamping to $C$ would silently miss the bound (the capped $T$ is still above the target there). @alg-estimate rejects the tier instead.
]

#r("sched.sla.headroom-confidence-scaled")

Headroom is *confidence-scaled*:

$ "headroom"(n_"eff") = 1.25 + 0.7 / sqrt(n_"eff") $

The $1 slash sqrt(n_"eff")$ term tracks the standard error of the fitted percentile — parameter uncertainty, which shrinks as $sqrt(n)$ — and the 1.25 floor covers irreducible run-to-run noise. Coverage of $1.25 times M_"p90"$ is $Phi(z_0.9 + ln 1.25 slash sigma_M)$, where the $z_0.9$ term is the p90 fit's own quantile (@fig-headroom-cov): $>=$ p99 when $sigma_M <= 0.12$, $tilde.op$ p97 at $sigma_M = 0.4$. Memory residuals are plausibly tighter than duration's $sigma in [0.1, 0.4]$, since peak RSS is near-deterministic for a fixed input set.

#memo[Neither the duration-residual $sigma$ range $[0.1, 0.4]$ used throughout this document nor the $sigma_M$ range is *measured*. The duration range is an assumption pending live `build_samples` data; gate (d) in §Implementation Phasing is the empirical check. The 1.25 floor is provisional pending the same data, and the OOM-reactive penalty-bump (§Exploration, last paragraph; spec marker `r[sched.sla.reactive-floor+4]`) is the safety net if it proves too low.]

Autopilot's evaluation @rzadca2020[§4.3] suggests that aggressive learned limits cut slack but raise @oom rate; widening the margin while the fit is young pays a small cost premium for stability.

#headroom-coverage-figure() <fig-headroom-cov>

Coupling $M$ to $c$ is a deliberate departure from Autopilot's three independent loops. It is justified for build workloads (`-jN` × per-compiler working set) but is gated: when the $M(c)$ fit's *Koenker–Machado pseudo-$R^1$* (the quantile-regression analogue of $R^2$: one minus the ratio of fitted-model pinball loss to intercept-only pinball loss; @koenker1999) is $< 0.7$, fall back to the independent p90 of observed `peak_mem` (the replay-the-peak behavior) and emit #(refs.metric)("rio_scheduler_sla_mem_fit_weak_total")`{tenant}`.

A tier is *infeasible* when the model's minimum achievable duration $T_"min" = T(min(macron(p), c_"opt"))$ exceeds the tier, or when the $c^*$ needed pushes $M(c^*) dot.op "headroom"$ past `maxMem`. Either demotes to the next tier ($D > "maxDisk"$ is $c$-invariant, so it fails outright per @alg-estimate, not demote).

#algorithm(
  caption: [Per-dispatch sizing, MVP — single hardware class, scalar p90 tier. This is what phases 1–5 ship; @alg-estimate-full extends it with the per-$(h, "cap")$ cost solve and admissible-set emission once §Hardware-class targeting lands.],
)[
  + *function* #smallcaps[SlaEstimate]#sub[MVP]$("drv")$
    + $"fit" <- "Estimator.cached"[("drv.pname", "drv.system", "drv.tenant")]$ #rann[FittedParams or ∅]
    + *if* $"fit" != emptyset and "fit".D + "budgets" > "maxDisk"$ *then* fail `disk_ceiling` #rann[$c$-invariant; raw $D$ (no headroom) — the request is clamped at `maxDisk` so this is the genuine "cannot fit" gate]
    + *if* `drv.enableParallelBuilding` is `false` *then* seed $macron(p) := 1$ #rann[seed only; saturation gate may revise — non-stdenv builders parallelize regardless]
    + *if* $"fit" = emptyset or "fit".n_"eff" < 3 or ("fit.span" < 4 and not "fit.frozen")$ *then return* #smallcaps[Explore]$("fit")$ #rann[@alg-explore]
    + $C <- min("fit".macron(p), "fit".c_"opt", "maxCores")$
    + *for each* $"tier" in "sla.tiers"$ tightest-first *do*
      + $beta_"tier" <- S - e^(-z_q hat(sigma)) dot.op "tier.p90"$ #rann[$z_q$ per §Coupled-model; not asymptotic 1.2816]
      + *if* $beta_"tier" < 0 and beta_"tier"^2 >= 4 Q P$ *then* #rann[precondition first — avoids complex √]
        + $c^* <- P slash q$, $q = -1/2 (beta_"tier" + "sgn"(beta_"tier") sqrt(beta_"tier"^2 - 4 Q P))$ #rann[citardauq — cancellation-free quadratic, §Coupled-model]
        + $c^* <- max(1, c^*)$
        + *if* $c^* <= C and M(c^*) dot.op "headroom" <= "maxMem"$ *then return* $"PodSpec"{c^*, ...}$
    + *return* best-effort $"PodSpec"{C, ...}$
] <alg-estimate>

#figure(
  caption: [Dispatch decision tree. @alg-estimate is the lower branch (MVP); the dashed box is where @alg-estimate-full expands the per-$(h, "cap")$ loop.],
  placement: auto,
  diagram(
    spacing: (8mm, 14mm),
    node-stroke: 0.5pt,
    node((0, 0), [dispatch(drv)], shape: fletcher.shapes.pill, name: <in>),
    edge("-|>"),
    node(
      (0, 1),
      align(
        center,
      )[fit immature?\ #text(size: 0.78em)[$emptyset or n_"eff" < 3 or$\ $("span" < 4 and not "frozen")$]],
      shape: fletcher.shapes.diamond,
      inset: 6pt,
      name: <gate>,
    ),
    edge(<gate>, <explore>, "-|>", [yes], label-size: 0.8em, label-side: left),
    node((-1.6, 2), [#smallcaps[Explore]\ (@alg-explore)], name: <explore>),
    edge(<gate>, <feas>, "-|>", [no], label-size: 0.8em, label-side: right),
    node(
      (1.6, 2),
      align(
        center,
      )[∃ feasible tier?\ #text(size: 0.78em)[$beta < 0, "disc" >= 0, P slash q <= C$]],
      shape: fletcher.shapes.diamond,
      inset: 8pt,
      name: <feas>,
    ),
    edge(<feas>, <pod>, "-|>", [tightest], label-size: 0.8em, label-side: left),
    node((1.6, 3.2), [`PodSpec`{$c^*$}], name: <pod>),
    edge(<feas>, <best>, "-|>", [none], label-size: 0.8em, label-side: left),
    node(
      (3.45, 2),
      align(
        center,
      )[best-effort\ #text(size: 0.82em)[$C$]],
      name: <best>,
    ),
    node(
      enclose: (<feas>, <pod>, (0.1, 2), (2.85, 2)),
      stroke: (dash: "dashed", paint: gray),
      inset: 6pt,
      corner-radius: 4pt,
      snap: false,
      [],
    ),
    node(
      (1.6, 3.85),
      text(size: 0.75em, fill: gray)[v1.2: × $(h, "cap")$, admissible set],
      stroke: none,
    ),
  ),
) <fig-dispatch>

== Saturation-gated exploration <exploration>

#r("sched.sla.explore-saturation-gate")

The model needs samples at distinct $c$ to fit. The scheduler obtains them via a control loop, gated so it never wastes cores a build demonstrably can't use:

- record `cpu_limit`, `peak_cpu`, `cpu_seconds`, `peak_mem`, `wall_secs` per completion (@lst-cgroup);
- $"avg_cores" = "cpu_seconds" slash "wall_secs"$; bump $c$ only when $"avg_cores" slash "cpu_limit" > 0.4$ AND duration exceeds the target tier — peak alone is insufficient (a brief parallel burst saturates `peak_cpu` without indicating the build would benefit from more cores);
#r("sched.sla.explore-x4-first-bump")
#r("sched.sla.explore-freeze")

- the *first bump is #mul(4)*, not #mul(2). Noise sensitivity in the Amdahl solve is dominated by sample _spacing_, not count: $hat(S) = (c_2 T_2 - c_1 T_1) slash (c_2 - c_1)$, so $sigma(hat(S)) prop sqrt((c_1 T_1)^2 + (c_2 T_2)^2) slash (c_2 - c_1)$ — at ±15% run-to-run variance and $S slash P = 1 slash 8$, the #mul(2) span $c in {8, 16}$ gives $sigma(hat(S)) slash S$ roughly #lever-arm-ratio that of the #mul(8) span $c in {4, 32}$ (@fig-lever-arm). A wide first step buys a usable fit one run sooner;
- exploration freezes at *span $>= 4$ or `maxCores`* — i.e., one #mul(4) bump on the up-path. A build that saturates at $4 dot.op "probe"$ has provided the @fig-lever-arm pair; a build that still saturates at `maxCores` (spin-loop) emits #(refs.metric)("rio_scheduler_sla_suspicious_scaling_total")`{tenant}`;
- when frozen *unsaturated* (probe met SLA, or one bump landed under-utilized), halve $c$ instead — the key reaches span≥4 in $<= 2$ further runs and the solver can then cost-minimize downward;
- switch from heuristic bumping to solving $T(c)$ directly only when $"span"(c_"max" slash c_"min") >= 4 and n_"eff" >= 3$. Until then the fit is provisional and not used for tier assignment.

#lever-arm-figure() <fig-lever-arm>

#figure(
  caption: [Telemetry source — the builder @supervisor samples cgroup v2 counters outside the build sandbox, so a hostile build cannot spoof them. `peak_cpu_cores` is just $Delta thin mono("usage_usec") slash Delta t$; the @nnls fit consumes nothing the kernel doesn't already track.],
  supplement: [Listing],
  kind: "listing",
)[
  // Stylized; closest source is rio-builder/src/executor/monitors.rs:113
  #codly(offset: 112)
  ```rust
  let cpu_stat_path = root.join("cpu.stat");
  let mut last_usage = fs::read_to_string(&cpu_stat_path)
      .ok()
      .and_then(|c| parse_cpu_stat_usage_usec(&c));
  let mut last_t = Instant::now();

  loop {
      tokio::time::sleep(POLL_INTERVAL).await;
      let now_usage = fs::read_to_string(&cpu_stat_path)
          .ok()
          .and_then(|c| parse_cpu_stat_usage_usec(&c));
      let now_t = Instant::now();
      if let (Some(a), Some(b)) = (last_usage, now_usage) {
          let cores = (b - a) as f64 / (now_t - last_t).as_micros() as f64;
          snapshot.record_cpu(cores); // → peak_cpu_cores, cpu_seconds_total
      }
      (last_usage, last_t) = (now_usage, now_t);
  }
  ```
] <lst-cgroup>

Exploration state is *derivable from `build_samples`* over the current version only ($"vdist" = 0$, so a version bump resets it). From the set of `cpu_limit` values seen:
$
  "distinct_c" = abs({"cpu_limit"})
  wide & "span" = max slash min \
  c_"up" = min(4 dot.op max, "maxCores")
  wide & c_"down" = max(1, floor(min slash 2))
$
Max/min over the set, not most-recent, so concurrent dispatches at different $c$ don't regress. Config validation requires $4 <= "sla.probe.cpu" <= "maxCores" slash 4$ (i.e., $"maxCores" >= 16$) so both paths can reach span $>= 4$ before their hard bound (as-shipped phase-12 enforces only $<= "maxCores"$; the $slash 4$ bound is §Phasing-13a tightening); a key that freezes at a bound with span $< 4$ is marked `frozen` and the solver engages with whatever span it has. The @estimator caches this derived tuple in `FittedParams` on the completion-ingest path, so dispatch reads no @pg; a leader failover reconstructs identical state from @pg on its first refresh.

#algorithm(
  caption: [Exploration — chooses the next $c$ when the fit is not yet trustworthy. Reads only the cached exploration tuple from `FittedParams`. Terminates: with `probe ≥ 4`, either path reaches `span ≥ 4` (and the solver engages) or hits a hard ceiling/floor within 3 steps.],
)[
  + *function* #smallcaps[Explore]$("fit")$ #rann[callers (@alg-estimate, @alg-estimate-full) handle `enableParallelBuilding`; returns $A := H$ (full hw-class set, §Hardware-class targeting)]
    + *if* $"fit" = emptyset$ *then return* $"prior"("drv")$ #rann[seed-corpus → fleet → operator]
    + *if* $"fit.span" >= 4$ *or* $"fit.max_c" = "maxCores"$ *or* $"fit.min_c" = 1$ *then*
      + *return* $"PodSpec"{"fit.max_c", ...}$ #rann[freeze; solver takes over]
    + *if* $"fit.saturated" and "fit.last_wall" > "tier.p90"$ *then*
      + *return* $"PodSpec"{"fit".c_"up", ...}$ #rann[#mul(4) bump for lever-arm]
    + *else return* $"PodSpec"{"fit".c_"down", ...}$ #rann[halve from $min(c)$]
] <alg-explore>

#tip(title: [Why the freeze gate matters])[
  The `frozen` flag is what makes the Explore↔SlaEstimate handoff terminate: a key that hits a ceiling/floor before reaching span $>= 4$ is marked `frozen`, and SlaEstimate's $("span" < 4 and not "frozen")$ gate then lets the solver engage with whatever span it has. Without it, the two algorithms ping-pong forever.
]

An OOM-kill at $(c, "mem")$ is a censored sample — $M(c) >= "mem"$, not $= "mem"$. The fit ingests it as a *flagged synthetic* observation $(c, 1.5 dot.op "mem")$ (excluded from MAD-reject so the eventual real observation cannot be discarded as the outlier), and triggers an automatic retry at $min(1.5 dot.op "mem", "maxMem")$. *If `mem` is already $= "maxMem"$, the retry path fails `mem_ceiling`* (so it cannot loop) — best-effort with $M(C) > "maxMem"$ should solve $c$ down from $C$ until $M(c) dot.op "headroom" <= "maxMem"$ or fail; mirroring the duration-misclassify penalty path.

== Model staging

$S$ and $P$ are fitted always; $Q$ and $macron(p)$ enter as data permits:

#figure(
  caption: [Model-staging gates. Entry/exit hysteresis prevents flapping when $n_"eff"$ drifts across a threshold as samples age; stage latches per version-epoch and resets only on a `vdist` jump.],
  kind: table,
  table(
    columns: (auto, 5.6cm, auto, 1fr),
    align: (left, left, left, left),
    table.header([Stage], [Enter / exit], [Active terms], [Effect]),
    [Amdahl],
    [$n_"eff" >= 3$, span $>=$ #mul(4)],
    [$S, P$],
    [baseline; what §Exploration converges to],

    [Capped],
    [first $"peak_cpu" < 0.85 dot.op "cpu_limit"$],
    [$S, P, macron(p)$],
    [never request $c > macron(p)$],

    [USL],
    [enter $n_"eff" >= 10$, span $>=$ #mul(8), $Delta"AICc" < -2$; exit $n_"eff" < 7$],
    [$S, P, macron(p), Q$ (ridge)],
    [$c^*$ lands at $c_"opt"$ instead of overshooting],
  ),
) <tbl-staging-gates>

All three are the _same_ @nnls solve with progressively unfrozen columns; there is one code path, not three. Once the Capped stage activates, samples with $c > macron(p)$ have their $1 slash min(c, macron(p))$ basis column collinear with the intercept; they are kept (still informing $S + P$) but the design matrix is rank-deficient and @nnls fits the constant — equivalently, drop *only when at least one $c <= macron(p)$ sample remains* (so the fit is never empty). $Delta"AICc"$ is the change in the @aicc @burnham2002 between the 2- and 3-parameter models; $< -2$ means the third parameter earns its keep — deliberately conservative, since with $Q$ constrained $>= 0$ the boundary-null distribution is $1/2 chi^2_0 + 1/2 chi^2_1$ @selfliang1987, not $chi^2_1$. The ridge penalty $+lambda Q^2$ (Tikhonov regularization on $Q$ only, with $lambda$ set from residual variance) is applied by appending a $sqrt(lambda)$ row to the design-matrix $c$-column — it shrinks $Q$ toward zero (i.e., toward Amdahl) continuously rather than as a discrete jump. The `avg_cores` saturation gate in §Exploration is what halts bumping _at_ the cap/retrograde boundary empirically; the model is what lets the solver land there _predictively_ on subsequent runs.

#figure(
  caption: [$T(c)$ under the three model stages for one synthetic build ($S = 30, P = 2000, Q = 0.05, macron(p) = 24$). Amdahl alone falls forever; Capped flattens at $macron(p)$; @usl turns back up past $c_"opt" = sqrt(P slash Q)$ — the solver clamps to $min(macron(p), c_"opt")$ so it never lands on the rising tail.],
  placement: auto,
  {
    let cs = range(1, 257)
    let pbar = 24
    lq.diagram(
      width: 11cm,
      height: 5.5cm,
      xscale: "log",
      yscale: "log",
      xlabel: [$c$ (cores)],
      ylabel: [$T(c)$ (s)],
      legend: (position: top + right),
      lq.plot(cs, cs.map(c => 30 + 2000 / c), label: [Amdahl ($Q = 0$)]),
      lq.plot(
        cs,
        cs.map(c => 30 + 2000 / calc.min(c, pbar)),
        label: [Capped ($macron(p) = #pbar$)],
      ),
      lq.plot(
        cs,
        cs.map(c => 30 + 2000 / c + 0.05 * c),
        label: [@usl ($Q > 0$, uncapped)],
      ),
      lq.vlines(pbar, calc.sqrt(2000 / 0.05), stroke: (dash: "dashed")),
    )
  },
) <fig-usl>

#figure(
  placement: auto,
  caption: [Forward staging transitions (USL has a hysteresis exit to Capped — see @tbl-staging-gates). Same @nnls solve, progressively unfrozen columns; only a `vdist` jump resets to Probe.],
  diagram(
    spacing: (28mm, 0mm),
    node-stroke: 0.5pt,
    node((0, 0), [Probe], shape: fletcher.shapes.circle, name: <probe>),
    edge(
      "-|>",
      align(center)[$n_"eff" >= 3$\ span $>=$ #mul(4)],
      label-size: 0.75em,
      label-side: left,
      label-sep: 0.4em,
    ),
    node((1, 0), align(center)[Amdahl\ $S, P$], name: <amdahl>),
    edge(
      "-|>",
      align(center)[first\ unsaturated],
      label-size: 0.75em,
      label-side: left,
      label-sep: 0.4em,
    ),
    node((2, 0), align(center)[Capped\ $+ macron(p)$], name: <capped>),
    edge(
      "-|>",
      align(center)[$n_"eff" >= 10$\ span $>=$ #mul(8)\ $Delta"AICc" < -2$],
      label-size: 0.72em,
      label-side: left,
      label-sep: 0.35em,
    ),
    node((3, 0), align(center)[USL\ $+ Q$], name: <usl>),
    edge(<amdahl>, <probe>, "..|>", bend: 30deg),
    edge(<capped>, <probe>, "..|>", bend: 38deg),
    edge(
      <usl>,
      <probe>,
      "..|>",
      [`vdist` jump],
      bend: 45deg,
      label-size: 0.75em,
      label-side: right,
    ),
  ),
) <fig-staging>

== Tier reassignment

#r("sched.sla.reassign-schmitt")

Tier assignment is recomputed on each Estimator refresh from the current fit. To prevent flapping when @tmin straddles a tier boundary, reassignment uses a *Schmitt-trigger deadband* (asymmetric two-threshold hysteresis): promote to a tighter tier only when the 80% confidence interval on @tmin is below #mul(0.85) that tier's binding bound; demote only when it exceeds #mul(1.05) the current tier's. The interval is computed by *weighted case (pairs) bootstrap*: \~500 reps, resampling row indices with $P(i) prop w_i$, refitting @nnls _unweighted_ @lawson1974 (resampling already encodes the weights; refitting weighted would double-count). Replicates whose resampled design matrix has $< 2$ distinct $c$ are discarded as rank-deficient. The pairs bootstrap is known-inconsistent at an active @nnls constraint boundary ($Q = 0$) @andrews2000; the resulting CI is used only for the Schmitt deadband, not inference, so coverage error is tolerated rather than corrected. Per surviving replicate $b$ compute

$
  hat(T)_"min"^((b)) = hat(S)^((b)) + hat(P)^((b)) / min(macron(p), hat(c)_"opt"^((b))) + hat(Q)^((b)) dot.op min(macron(p), hat(c)_"opt"^((b)))
$

and take its empirical 10th/90th percentiles (bootstrapping $hat(S)$ alone would under-state $T_"min"$ in the Capped/@usl stages; $macron(p)$ is held fixed across replicates — its sampling variance is small relative to $S, P, Q$). At least 100 replicates must survive the rank-deficiency filter, else the CI is treated as $[0, oo)$ and tier reassignment is skipped. The bootstrap is *debounced per key to once per 2 sample-arrivals* (the CI moves slowly), and the cached CI is *invalidated whenever $n_"eff"$ drops by $>$ 50% or $|Delta T_"min"|$ exceeds half the cached CI width* so the Schmitt trigger never compares a fresh point estimate against a stale interval.

== Hardware heterogeneity

Karpenter provisions across instance generations by spot cost — #(refs.gh)("infra/helm/rio-build/values.yaml:506") admits gen 6/7/8, which spans Graviton2→4 within `aarch64-linux` and Milan/Ice Lake/Genoa/Sapphire Rapids within `x86_64-linux`. Phoronix LLVM-compile benchmarks @phoronix2024 @openbenchmarking2024 show a *#mul(1.89) wall-clock spread* across that Graviton range and #mul(1.31) across same-gen x86 vendors — #mul[2–6] the model's $tilde.op #dur-margin-pct%$ duration margin ($e^(z_0.9 dot.op 0.1) - 1$, the p90-to-median gap at the $sigma = 0.1$ floor of the §Duration distribution range). Pooling samples without correction makes $T(c)$ systematically wrong on the slowest hardware Karpenter picks. Under a global-DB deployment the spread only widens.

#figure(
  caption: [Per-generation $bold("factor")[h]."alu"$ from LLVM-compile benchmarks @phoronix2024 @openbenchmarking2024, normalized to slowest = 1.0. The shaded band is the model's $tilde.op #dur-margin-pct%$ duration margin ($e^(z_0.9 dot.op 0.1) - 1$, the p90-to-median gap at the $sigma = 0.1$ floor of the §Duration distribution range) — every class differs from the reference by more than the margin, so pooling without normalization is wrong by construction. The `membw` and `ioseq` dimensions show comparable spreads across `r`/`x` families and `ebs` vs `nvme` storage classes respectively.],
  {
    let hw = (
      ("Graviton2", 1.00),
      ("Ice Lake", 1.18),
      ("Milan", 1.31),
      ("Graviton3", 1.46),
      ("Genoa", 1.62),
      ("Sapphire Rapids", 1.71),
      ("Graviton4", 1.89),
    )
    lq.diagram(
      width: 11cm,
      height: 5cm,
      xlabel: $bold("factor")[h]."alu"$,
      xlim: (0.85, 2.0),
      yaxis: (
        ticks: hw.enumerate().map(((i, (n, _))) => (i, n)),
        subticks: none,
      ),
      lq.rect(
        1 - duration-margin(0.1),
        -0.5,
        width: 2 * duration-margin(0.1),
        height: 7,
        fill: gray.transparentize(80%),
      ),
      lq.hbar(hw.map(((_, f)) => f), range(hw.len())),
    )
  },
) <fig-hw-factor>

#r("sched.sla.hw-ref-seconds")

Mitigation is *$K$-dimensional reference-second normalization*: a per-$h$ microbench *vector* $bold("factor")[h] in RR^K$ ($K = 3$: int-throughput, memory bandwidth, sequential I/O) and a per-pname *mixture* $bold(alpha)["pname"] in Delta^(K - 1)$ (the simplex), so the effective scalar factor is the dot product $bold(alpha)["pname"] dot.op bold("factor")[h]$. This is Ernest's @ernest2016 parametric basis with PARIS-style @paris2017 fingerprint/residual decomposition, but without PARIS's offline corpus or matrix completion. CherryPick @cherrypick2017 and Selecta @selecta2018 are the canonical Bayesian-optimization alternatives, rejected in §Alternatives. The $K = 3$ basis matches the axes cloud instance families segment on: gen-$n -> n + 1$ improves int-throughput, `r`/`x` families improve membw, and `*d`/`i*` families improve ioseq.

*`hw_class`* is the tuple `{instance-cpu-manufacturer, instance-generation, storage}` derived from Karpenter node labels. Getting it into a build sample is the first plumbing problem: neither the downward API nor an admission webhook can expose node *labels*, since the webhook fires before `spec.nodeName` is bound, and builders are air-gapped from the apiserver. Instead the @controller's pod-annotator stamps `rio.build/hw-class` *after bind* via its Node informer and exposes it through a downward-API *volume* (#(refs.gh)("docs/spec/components/controller.typ")). The builder reads the annotation and includes it in its completion sample, so no server-side join is needed.

`hw_class` is recorded NULL when the annotation never landed — interrupted builds, annotator race — or when the class has $< 3$ distinct `pod_id` benches. In both cases the sample feeds the per-key fit *with $bold("factor") := bold(1)$*, the reference-class vector. This is bias-neutral, slightly mis-normalized but bounded by the seed-vs-true ratio, and the sample is excluded from `hw_perf_factors`/`bias`. Anchor slots (§Coupled-model) on such samples are kept for design-matrix rank. The `storage ∈ {ebs, nvme}` component is derived from the NodeClaim's `nodeClassRef`: `rio-nvme` carries `instanceStorePolicy: RAID0` and stamps `rio.build/storage: nvme` (§Hardware-class targeting). This distinguishes local instance-store NVMe (`*d` families — c6id/m6id/r6id/i4i; \~400k IOPS, \$0 marginal) from the gp3 root volume (80k IOPS ceiling at max provisioning since 2025-09; 3k at default).

*`hw_perf_factors{hw_class → (alu, membw, ioseq)}`* is a PG table seeded from OpenBenchmarking and *self-calibrating*. The builder _supervisor_ — not the untrusted build payload — runs three single-threaded probes at init before accepting any assignment, freeing the working set after (#(refs.gh)("rio-builder/src/runtime/setup.rs:95")):
- `alu` (\~#qty("5", "s")): fixed-source compile loop (current).
- `ioseq` (\~#qty("5", "s")): `O_DIRECT|O_DSYNC` 1 MiB sequential writes to `/var/rio/overlays` — NVMe RAID0 or gp3 root per `nodeClassRef`. `O_DIRECT` defeats page cache; target size #qty("256", "MiB") looped to stay under `ephemeral-storage`.
- `membw` (\~#qty("3", "s")): STREAM triad @mccalpin1995 with each of 3 arrays at $4 times max("LLC")$ — #qty("1.5", "GiB") on c7a.48xlarge → \~#qty("4.6", "GiB") working set.
`alu` and `ioseq` run concurrently; `membw` runs alone after, since it saturates the memory bus and would contaminate `alu`.

The bench runs *only when the controller stamps `rio.build/hw-bench-needed=true` on the pod*. That annotation is set at pod-create time when two conditions hold. First, the scheduler reports via `AdminService.HwClassSampled` — cached per controller-tick from the `HwTable` snapshot — that *any* $h$ in the intent's $A$ has fewer than `trust_threshold` distinct tenants in some K=3 dimension (`trust_threshold` is the scheduler's `FLEET_MEDIAN_MIN_TENANTS`, default 5 — the same per-dimension gate `cross_tenant_median` uses, so honest pods keep benching until single-tenant capture is impossible). The actual $h$ is fixed only at kube-scheduler bind, so the create-time check is over $A$; this over-benches at most until every $h in A$ reaches the threshold. Second, `resources.requests.memory >= sla.hwBenchMemFloor` (default #qty("8", "GiB")), so STREAM cannot OOM a `preferLocalBuild`/fetcher pod. The threshold is reached only as builds *complete* on $h$ — samples land at completion-ingest, not bench-finish — so early pods on a fresh $h$ re-bench, bounded by the per-dimension distinct-tenant gate above. The annotation is never absent: `build_job` stamps it `true` or `false` on every pod template it creates. A stamped `false` skips the K=3 bench — only the phase-10 scalar `alu` probe still runs, and with default-uniform $bold(alpha)$ the model degrades to scalar. A truly absent annotation fails loudly rather than silently skipping: the downward-API fieldRef resolves to the empty string, which the config loader rejects for a bool field, so the pod fails at startup.

#r("sched.sla.hw-bench-append-only")

Each probe computes $bold("factor")_d = "observed_throughput"_d slash "ref_throughput"_d$ per dimension $d$, so $bold("factor")[h]_d >= 1$ on faster hardware, and *appends* a row to `hw_perf_samples(hw_class, pod_id, factor jsonb, at)`. The Estimator's refresh tick aggregates $bold("factor")[h]_d = median("factor"_d)$ per dimension over rows with $>= 3$ distinct `pod_id`, with #gls("mad")-based outlier rejection. Here $MAD = median_i abs(r_i - median(r))$, and the 1.4826 factor used below makes $1.4826 dot.op MAD$ a consistent estimator of $sigma$ under normality @hampel1974. Append-only writes avoid the last-write-wins race that a single-row upsert would have under concurrent benches; the same append-then-aggregate split applies to `interrupt_rate` self-calibration.

The reference is `sla.referenceHwClass`, or per-dimension $min$ once $>= 1$ row exists, so all $bold("factor")[h]_d >= 1$. *Unbench'd classes use $bold("factor") := bold(1)$ in the per-@pname fit*, per the NULL-handling above. This is slightly mis-normalized but bounded by the seed-vs-true ratio, and excluded from `hw_perf_factors`/`bias`. It avoids both the discontinuous refit when $bold("factor")[h]$ lands *and* the rank-loss that down-weighting to zero would cause for anchor slots. The bench-at-init annotation narrows but does not close the race between the 3-distinct-`pod_id` and per-@pname $n_"eff"$ counters.

*The per-pname mixture* $bold(alpha)["pname"]$ is fitted on $T_"ref" (c) slash "wall_secs"_"obs" approx bold(alpha) dot.op bold("factor")[h]$ over the pname's samples *with bench'd `hw_class`*. NULL-/unbench'd samples carry $bold("factor") = bold(1)$, which is redundant with the simplex constraint and would mis-pair the *true* $h$'s residual with the reference vector; they are excluded here but kept in the $T_"ref"$ fit. Four numerical concerns govern the fit.

*Identifiability.* With $K = 3$ unknowns and the simplex constraint $bold(alpha) dot.op bold(1) = 1$ supplying one row, the bare floor is $>= K - 1 = 2$ distinct non-reference $h$ with $op("rank"){bold("factor")[h_1], dots.h, (1, dots.h, 1)} = K$. The gate is set at *$>= 3$* — one degree of overdetermination, equivalently pairwise angular spread $> theta_min$ after centering — for noise margin. Below the gate the @nnls active-set picks an arbitrary vertex, so $bold(alpha)$ stays at the prior.

*Conditioning.* $bold(alpha)$-@nnls gets the same column-normalize as $T(c)$'s, plus a *Dirichlet-MAP ridge toward the prior*: $+ lambda_alpha norm(bold(alpha) - bold(alpha)_"prior")^2$, with $lambda_alpha$ capped so the ridge never outweighs a single observed sample. The ridge is needed because within-family generations move `alu` and `membw` near-proportionally.

*Prior.* $bold(alpha)_"prior"."ioseq"$ is seeded from the per-@pname I/O *saturation*: cgroup `io.stat` rbytes+wbytes per wall-second, divided by the *observed* sample's $bold("factor")[h_i]."ioseq" dot.op "ref_throughput"$. This is the fraction of that hardware's I/O ceiling the build consumed; `io.pressure` is a *stall* metric and so hardware-dependent in the wrong way. The seed keeps an I/O-bound build from being pulled toward a CPU-heavy fleet's median before the rank gate passes. Otherwise the prior is fleet-median $bold(alpha)$ over fitted pnames once $>= 50$ exist — the same machinery as the $M(c)$ prior in §Fleet-learned priors, with the same $[0.5 times, 2 times]$ clamp on each component — else uniform $(1 slash K, dots.h)$.

*Deconfounding.* The cost-solve correlates $c$ with $h$ because each $c^*$ is routed to its cost-optimal $h$. Fitting $bold(alpha)$ from $T_"ref" (c) slash "wall"$ at varying $c$ would confound Amdahl slope with hardware speedup. So $bold(alpha)$ is fitted on the *$c$-residualized* speedup $exp(-hat(epsilon)_i) = T_"ref" (c_i) slash "wall"_i$, the per-sample log-residual already computed for $hat(sigma)$. This is $c$-independent in expectation when $T_"ref"$ is correctly shaped, breaking the confound. The $bold(alpha)$ fit is a simplex-constrained least-squares via KKT projection; post-hoc $L^1$-normalize sits outside the alternating-least-squares (ALS) objective and can break monotonicity @gillis2014.

The mixture $bold(alpha)$ is *per-pname only*, so once the rank gate passes it generalizes to unseen $h$. An I/O-bound build — high `peak_io_pressure_pct` from cgroup `io.pressure some avg10` — lands at $bold(alpha)."ioseq" approx 1$, and the solve routes it to `storage=nvme` where $bold("factor")[h]."ioseq"$ is large. The same dot product routes compute-bound builds to faster CPU generations.

*Residual bias* $"bias"["pname", h] = median_(n >= 3)("wall_secs"_"obs" dot.op (bold(alpha) dot.op bold("factor")[h]) slash T_"ref" (c))$ captures whatever the rank-$K$ model misses. The most important case is *per-phase heterogeneity*: a build whose link step is membw-bound and compile is ALU-bound has no single $bold(alpha)$ that scales $S$ and $P$ correctly, so the per-$h$ bias absorbs the per-term error. The bias is held at $1.0$ until $>= 3$ samples on that $h$, since a 1-sample median commits to noise, and thereafter *held at last-observed value*. Decaying toward $1.0$ would silently break SLA when the residual is genuine, as in the per-phase case above. $epsilon_h$-exploration re-samples excluded cells at $tilde.op epsilon_h slash |H without A|$ per dispatch, so reaching $n = 3$ on a never-admissible cell takes $tilde.op 3 |H without A| slash epsilon_h approx 500$ dispatches. This is slow but bounded; the $bold(alpha)$-model's cross-$h$ generalization carries SLA in the meantime, not the per-cell `bias`.

The remaining concern is how $T_"ref"$ and $bold(alpha)$ are *fitted jointly in reference-seconds.* The alternation is *heuristic*, not exact block-coordinate descent on a single objective: $ln T_"ref" (c) = ln(S + P slash c + Q c)$ is nonlinear in $(S, P, Q)$, so neither alternant is a closed-form descent on the natural log-space loss. The $T_"ref"$ @nnls fits $(S, P, Q)$ on $"wall"_i dot.op (bold(alpha) dot.op bold("factor")[h_i])$ at fixed $bold(alpha)$, with raw-space squared loss estimating the *mean* $T_"median" e^(sigma^2 slash 2)$ (see §Duration distribution). The simplex-LS then fits $bold(alpha)$ at fixed $T_"ref"$ on $exp(-hat(epsilon)_i) approx bold(alpha) dot.op bold("factor")[h_i]$, where $hat(epsilon)_i = ln "wall"_i - ln T_"ref" (c_i)$ is the $c$-residualized speedup. This is $c$-independent in expectation *at the fixed point* where $T_"ref"$ is correctly shaped; there is no per-iterate guarantee. $M(c)$, $macron(p)$, and raw $c$ are core-_count_ quantities and are not scaled. The procedure iterates until $norm(Delta bold(alpha))_1 < 10^(-2)$ or 5 rounds, emitting #(refs.metric)("rio_scheduler_sla_als_round_cap_hit_total")`{tenant}` if the cap is reached without convergence. Convergence is *not* analytically guaranteed and is one of the §Phasing-13a empirical gates: cross-$h$ $bold(alpha)$ recovery on a deliberately $c arrow.l.r h$-correlated probe set.

The objective minimum is *unique up to $h$-exploration*. When one $(h, "cap")$ price-dominates and every sample lands there, the rank gate never passes and $bold(alpha)$ stays at the prior. §Hardware-class targeting's $epsilon_h$-exploration supplies cross-$h$ observations independent of capacity exhaustion.

This is the #link("https://allocations.access-ci.org/exchange_calculator")[ACCESS SU-normalization] approach generalized from a single benchmark to a $K = 3$ basis, plus the residual bias. A keyed approach (`hw_class` in the model key) was rejected — see §Alternatives.

== Duration distribution

#r("sched.sla.quantile-geo-lognormal")

Percentile envelopes require a distribution, not a point estimate. The model composes three sources: a deterministic base, multiplicative lognormal run-to-run noise, and a geometric retry tail from spot interruption.

The *base* $T(c, h)$ is deterministic, taken from §Coupled-model in reference-seconds and scaled by $(bold(alpha)["pname"] dot.op bold("factor")[h])^(-1) dot.op "bias"["pname", h]$ (§Hardware heterogeneity).

*Run-to-run noise* enters as $T_"attempt" = T dot.op exp(epsilon)$ with $epsilon tilde.op cal(N)(0, sigma^2)$, so $T$ is the _median_ attempt duration and $sigma$ is the standard deviation of log-residuals $ln("obs" slash "fit")$ from the @nnls fit. @nnls minimizes squared residuals on $T$, an estimator of the *mean* $T_"true" e^(sigma^2 slash 2)$, so $hat(S), hat(P), hat(Q)$ carry a conservative $e^(sigma^2 slash 2) <= 1.083$ inflation over $sigma in [0.1, 0.4]$. *$hat(sigma)$ is floored at $max(delta t slash "median"("wall_secs"), 0.05)$.* The first arm is the cgroup-poll quantization; the second is half the assumed $sigma in [0.1, 0.4]$ lower bound. The range is *unmeasured* — see §Implementation Phasing gate (d) — and a tighter floor would over-claim near-determinism. Under-stated $hat(sigma)$ tightens $z_q hat(sigma)$ and *under-sizes* $c^*$, missing the envelope, so the floor errs toward over-allocation. This is accepted because the §Coupled-model $z_q$ correction dominates at low $n_"eff"$.

The *interruption tail* $G$ is a geometric retry count with $p = 1 - exp(-lambda[h] dot.op T(c,h))$ on spot (where $lambda$ is interruptions/sec) and $p = 0$ on on-demand. $(h, "spot")$ is treated as *infeasible when* $p > 0.5$: beyond that, expected attempt count $EE[G] = 1 slash (1-p)$ exceeds 2 and cost runs away.

The interruption rate $lambda[h] = "EMA"("interrupts"_h) slash "EMA"("spot-node-seconds"_h)$ is a *rate*, with the exposure denominator tracked from the controller's Node informer so "never interrupted" is distinguishable from "never scheduled there". It is seeded from #link("https://aws.amazon.com/ec2/spot/instance-advisor/")[AWS Spot Advisor] buckets. The interrupt signal is the k8s *Event* with `reason=SpotInterrupted` that Karpenter's interruption controller emits on the NodeClaim (#link("https://github.com/aws/karpenter-provider-aws/blob/main/pkg/controllers/interruption/events/events.go")[source]); there is no NodeClaim status condition for it, and the `karpenter.sh/disrupted` taint is generic to all disruption types. The @controller watches the Event and appends `(h, interrupt_at)` and `(h, node_seconds)` rows to @pg — the same append-then-aggregate split as `hw_perf_samples` — and the @scheduler\-lease-holder computes and persists the @ema. A hostile build cannot fake it. Event delivery is best-effort (apiserver rate-limits, 1h TTL); a missed Event under-counts $lambda$, biasing toward spot, bounded by the seed-decay floor. The @ema halflife for $lambda$ is wall-clock #qty("24", "h"), decoupled from the per-@pname sample halflife, since interrupts are sparse.

The seed is a *Gamma-Poisson partial-pooling prior*, not an initial condition:
$
  hat(lambda)[h] = ("EMA"("interrupts"_h) + n_lambda dot.op lambda_"seed" [h]) / ("EMA"("exposure"_h) + n_lambda)
$
with the EMA terms as defined above and $n_lambda = #qty("1", "day") dot.op max(1, "EMA"_(24"h")("node_count"_(h, "spot")))$. The prior contributes \~50% at one wall-clock day of exposure regardless of fleet size, and the $max(1, dot.op)$ floor keeps it from vanishing when spot is exiled and `node_count` $-> 0$ — which would otherwise freeze $hat(lambda)$ at the spike. With pooling, a single interrupt does not spike $hat(lambda)$ to a value that exiles spot for #qty("24", "h") then collapses back when exposure goes to zero; the seed-decay-only design has a $tilde.op #qty("48", "h")$ limit cycle under persistent capacity stress.

#idea(title: [Intuition before the CDF])[
  In plain terms: wall-clock = (base duration) × (lognormal noise) × (geometric retry count from spot interruptions). $F(t)$ below is just the closed-form @cdf of that product so we can answer "what $c$ makes the p99 land under 8 minutes?" without Monte Carlo.
]

The total wall time is

$ T_"total" = sum_(i<G) tau_i + T dot.op exp(epsilon_G) $

where $tau_i < T dot.op exp(epsilon_i)$ are partial-attempt durations before interruption. The implementation models the upper tail by $G dot.op T dot.op exp(epsilon)$ with a single $epsilon tilde.op cal(N)(0, sigma^2)$. Its $q$-quantiles fall short of the *full-attempt* sum $sum_(i <= G) T dot.op exp(epsilon_i)$ by $<= #geom-lognormal-max$ across $sigma in [0.1, 0.4], p in [0.05, 0.5], q in [0.5, 0.99]$ (@fig-mc-geom); conditional on $G = k$, the single-$epsilon$ mean $k T dot.op e^(sigma^2 slash 2) >= EE[T_"total" | G = k] approx ((k + 1) slash 2) T dot.op e^(sigma^2 slash 2)$ (each $tau_i$ is a *partial* attempt with $EE[tau_i | "interrupt"] approx T slash 2$ for $lambda T <= 1$), so the single-$epsilon$ model is conservative for the true distribution. The $<= #geom-lognormal-max$ shortfall against the upper bound is within the $<= 8%$ under-statement budget already absorbed by $EE^"upper"$'s on-demand bias (§Hardware-class targeting). Its @cdf is the geometric mixture of lognormals

#geom-lognormal-figure() <fig-mc-geom>

$ F(t) = sum_(k >= 1) (1-p) p^(k-1) Phi(ln(t slash (k T)) / sigma) $

where $Phi$ is the standard-normal @cdf.

#algorithm(
  caption: [Quantile of the geometric-lognormal mixture. Evaluating $F$ is $O(K_p)$ normal-CDF calls; geometric bisection converges in $ceil(log_2 (ln("hi" slash "lo") slash epsilon_"rel"))$ steps. At $p = 0.5$, $K_p = 20$ and the cost is $tilde.op 260$ $Phi$-calls per envelope bound (still \~µs); deterministic across restarts where Monte Carlo would not be.],
)[
  + *function* #smallcaps[Quantile]$(q; T, sigma, p, z_q)$ #rann[requires $q <= 0.99 and p <= 0.5$; $z_q$ per §Coupled-model]
    + $sigma <- max(sigma, 10^(-3))$ #rann[guard: deterministic builds]
    + *if* $p = 0$ *then return* $T dot.op exp(sigma dot.op z_q)$ #rann[on-demand: pure lognormal, small-$n_"eff"$ widened]
    + $K_p <- ceil(ln 10^(-6) slash ln p)$ #rann[tail mass $< 10^(-6)$; $K_p <= 20$]
    + $sigma' <- cases(sigma dot.op z_q slash Phi^(-1)(q) & "if" q != 0.5, sigma dot.op sqrt(1 + 1 slash sum w) & "if" q = 0.5)$ #rann[inflate for parameter uncertainty; the ratio is $0 slash 0$ at $q = 0.5$; the limit is $sqrt(1 + 1 slash sum w) dot.op phi(0) slash f_t (0; "df") approx 1.09 sqrt(...)$ so the branch under-inflates by $tilde.op 9%$ — the median is $sigma$-insensitive so the gap is immaterial]
    + *function* $F(t) := sum_(k=1)^(K_p) (1-p) p^(k-1) Phi(ln(t slash (k T)) slash sigma')$
    + $"lo" <- T dot.op exp(-3 sigma'); quad "hi" <- K_p dot.op T dot.op exp(sigma' dot.op max(3, Phi^(-1)(q) + 1))$
    + *assert* $F("lo") < q < F("hi")$
    + *while* $ln("hi" slash "lo") > epsilon_"rel"$ *do* #rann[$epsilon_"rel" = 10^(-3)$]
      + $"mid" <- sqrt("lo" dot.op "hi")$ #rann[geometric midpoint → relative tolerance]
      + *if* $F("mid") < q$ *then* $"lo" <- "mid"$ *else* $"hi" <- "mid"$
    + *return* $sqrt("lo" dot.op "hi")$
] <alg-quantile>

The solve runs once per *$(h, "cap")$* candidate (§Hardware-class targeting). $T$ uses $"bias"["pname", h] slash (bold(alpha)["pname"] dot.op bold("factor")[h])$ directly — the multiplier on $T_"ref"$ per §Hardware heterogeneity — and $p$ uses $lambda[h]$. $p$ is computed from median $T$, a $<= e^(sigma^2 slash 2) - 1 approx 8.3%$ under-statement at $sigma <= 0.4$ that is absorbed by the on-demand bias of $EE^"upper"$. $(h, "spot")$ is rejected iff $p$ at $c = C$ exceeds $0.5$; otherwise the bisection lower bound is $c_lambda = min{c : T(c, h) <= ln 2 slash lambda[h]}$, keeping the $p <= 0.5$ precondition of @alg-quantile invariant.

*Per-dispatch upper bound:* $|"tiers"| dot.op |H| dot.op 2 dot.op ceil(log_2 "maxCores") dot.op |"bounds"| dot.op (K_p dot.op 13)$ $Phi$-calls, with $H$ the configured `sla.hwClasses`. At $|H| = 6$, $"maxCores" = 512$ that is $approx 4 dot.op 12 dot.op 9 dot.op 3 dot.op 260 approx 3.4 dot.op 10^5$, a few ms; on-demand candidates short-circuit at the $p = 0$ branch.

The result is *memoized on `(fit_hash, override_hash, inputs_gen)`*. `fit_hash` and `override_hash` are per-key, so a key's refit invalidates only its own entry. `inputs_gen` is *derived* from the solve-relevant projection of the *shared* solve inputs — `HwTable`/`factor[h]` and `CostTable`/`(price, λ, node_count, cells, stale_clamp)` — at poll time; no caller bumps. (`sla.{tiers, hwClasses, hwCostTolerance, maxCores, maxMem, maxDisk}` are restart-only.) Most `compute_spawn_intents` ticks are therefore pure cache hits, and the actor's #qty("5", "s") RPC budget holds at $10^4$ Ready drvs. The memo stores `(c*, A, candidates)` so the per-dispatch $epsilon_h$-draw can read $A$ without re-solving. A drv whose key is uncached — first appearance after an `inputs_gen` change — falls back to @alg-estimate's MVP solve in-tick.

For each $(h, "cap")$, find the minimum $c$ satisfying _all_ envelope constraints, compute

$
  EE["cost"]^"upper" = "price"[h, "cap"] dot.op c dot.op (T slash 3600) dot.op e^(sigma^2 slash 2) slash (1 - p)
$

with $"price"[h, "cap"] := min_"az" "price_per_vcpu_hr"$ of the smallest instance type in $h$ fitting $(c^*, M(c^*))$ — Karpenter picks the cheapest, so min is the realistic comparator. `capacity_type` falls out of the solve rather than being configured.

== Hardware-class targeting

Sizing $c^*$ for the slowest admitted class is SLA-correct but wastes $tilde.op (1 - 1 slash f)$ of core-seconds when Karpenter places on faster hardware — at $f = 1.89$ that is \~47%. Since the model can predict $T(c, h)$ per class, the scheduler *constrains* the class set and lets the provisioner choose within it:

=== Cost model and admissible set

The cost solve reads from `hw_cost_factors{region, az, instance_type, capacity_type → price_per_vcpu_hr, interrupt_rate}`, a *cluster-local* @pg table. It is cluster-local because spot price varies #mul[2–5] across @az:pl and \~#mul(2) across instance sizes within one `hw_class`; under global-DB each region's poller would race-write. The solve resolves `price_ratio[h]` as the \$/vCPU-hr of the smallest instance type in $h$ that fits $(c^*, M(c^*))$ — i.e., what Karpenter would actually launch. `price_per_vcpu_hr` is *#gls("ema")-smoothed with a #qtyrange("2", "4", "h") wall-clock halflife*, decoupled from the per-@pname sample halflife, to damp oscillation. The table is seeded from static helm config; with `sla.hwCostSource: spot` it is refreshed from `ec2:DescribeSpotPriceHistory` every \~#qty("10", "min") by the *lease-holding scheduler replica only* via @irsa. The @ema state (price, $lambda$ numerator/denominator) is *persisted to @pg each tick* so a lease failover resumes the smoothed value rather than resetting to seed, and #(refs.metric)("rio_scheduler_sla_hw_cost_stale_seconds") surfaces a stalled poller. If `_hw_cost_stale_seconds > 6 × pollInterval` the solve clamps `price_ratio[h]` to the helm seed and increments #(refs.metric)("rio_scheduler_sla_hw_cost_fallback_total")`{reason}`.

From this table the *admissible set* is constructed. For each $(h, "cap")$ with at least one instance type fitting $(c^*, M(c^*))$, compute $EE["cost"]^"upper"$ per §Duration distribution and let $EE^"min"$ be the minimum over all candidates. The *admissible set* $A$ is every $(h, "cap")$ with $EE["cost"]^"upper" <= (1 + tau_"enter") dot.op EE^"min"$ for cells not in the *previous* tick's $A$, or $<= (1 + tau_"exit") dot.op EE^"min"$ for cells already in $A$. The Schmitt deadband $tau_"enter" = tau$, $tau_"exit" = 1.3 tau$ prevents EMA noise on `price_per_core` from flapping a boundary cell; default `sla.hwCostTolerance` $tau = 0.15$. $tau$ is the operator's "accept hardware within X% of optimal" knob: small $tau$ prunes to the model's argmin, while large $tau$ delegates more of the choice to the provisioner's live price/availability data. The conservative $EE^"upper"$ biases mildly toward on-demand inclusion, which is accepted.

Sizing for the admissible set then takes $c^* := max_((h, "cap") in A) c^*_(h, "cap")$ and *re-filters $A$ to cells with an instance type fitting $(c^*, M(c^*))$ AND with $EE["cost"]^"upper"(c^*)$ still within $(1 + tau)$ of $EE^"min"$*. A fast cell may have been admitted on a small instance that does not exist at the larger $c^*$, or its cost at $c^*$ may now exceed the tolerance. The argmax cell trivially survives both checks since its own $c^*_(h, "cap") = c^*$, so the re-filtered set is never empty.

The emitted $c^*$ satisfies the SLA on whichever admissible class the provisioner places the pod. This holds because $"Quantile"_h (q;c)$ is monotone-decreasing in $c$ on $[1, c_"opt"]$, and $c_"opt"$ is $h$-invariant: $bold(alpha) dot.op bold("factor")[h]$ is a scalar multiplier on $T$ that leaves $T$'s shape unchanged. Faster placements simply finish early. The re-filter bounds *realized \$-cost* on every placement in $A'$ at $(1 + tau) EE^"min"$ by construction. The *core-count* ratio $c^* slash min_(A') c^*_(h, "cap")$ is not $tau$-bounded — it diverges as a slow cell's effective bound approaches the serial floor $S$ — so the re-filter additionally drops cells with $c^*_(h, "cap") < c^* slash k$ (default $k = 2$). This caps capacity over-allocation at $2 times$ while leaving \$-cost at $(1 + tau)$. The argmax cell has $c^*_(h, "cap") = c^*$ so it survives, and $A' != emptyset$ remains provable.

=== Forecast-driven provisioning <sec-forecast>

#warning(title: [Why rio provisions, and why an admissible set])[
  Reactive provisioning (Karpenter watches Pending pods, creates NodeClaims) cannot see the DAG: it learns of layer-$N+1$'s demand only when layer-$N$ completes, costing one node-boot of latency per layer (#qtyrange("40", "90", "s") × DAG depth). The scheduler has the DAG, per-dep ETAs, and per-intent $c^*$ — everything needed to provision layer-$N+1$'s capacity *while layer-$N$ runs*. No surveyed production workflow scheduler (Airflow, Spark DRA, Dataproc, Cloud Composer) does DAG-structural lookahead; predictive autoscalers like Netflix Scryer @nflxscryer2013 are time-series-based, not graph-based — to our knowledge this 1-layer-ahead ETA→NodeClaim approach is novel. The controller drives Karpenter's NodeClaim CRD directly so Karpenter's cloud-provider machinery (launch templates, `CreateFleet`, spot-interruption handling, AMI/IAM resolution) is retained without its reactive provisioner.

  Within that, the scheduler's edge over the cloud provider is the *time* model ($T(c, h)$, $bold(alpha)$, $lambda$); the cloud provider's edge is *live per-type per-@az price and availability*. Emitting an admissible set $A$ per intent (rather than a single $h$) lets each side optimize what it knows: the scheduler excludes classes its time model rules expensive; for placement on *existing* nodes kube-scheduler picks any $h in A$, and for *new* capacity the controller's per-cell deficit routes each unplaced intent to one cell (its EMA-cheapest in $A$) and `CreateFleet` picks within that cell's instance-type menu. Node reuse across intents means each provisioned $h$ accumulates samples across the workload mix; the explicit $epsilon_h$-pin (below) supplies cross-$h$ observations when one cell durably price-dominates.
]

#r("sched.sla.intent-from-solve")[
  The scheduler exposes one `SpawnIntent{intent_id, cores, mem, disk}` per
  queued derivation (FOD and non-FOD) in `GetSpawnIntents`. `cores` is
  `ceil(solve_tier(c_star))` for fitted keys, probe defaults otherwise;
  `prefer_local_build` / `enable_parallel_building=false` pin `cores=1`.
]

*Forecast emission* extends `compute_spawn_intents` to walk the DAG twice: the *Ready* frontier as in v1.0, and a *forecast* frontier of `Queued` derivations whose every incomplete dependency is *running* with $"ETA" < max_((h,"cap") in A) "lead_time"[h, "cap"]$ — or *substitution-active* (round-9 F1, live_049 lever 3): a dep carrying a store-active materialization job (claimed, or claimable now) contributes the typed static substitution prior `SUBSTITUTING_DEP_ETA_PRIOR_SECS` instead of a fitted-curve ETA, through the same lead gates. Here $"ETA" = max(0, T(c, h_"placed") - "elapsed")$ evaluated at the running dep's dispatched $c$ and bound $h$, and `lead_time` is the per-cell learned boot horizon described below. Each `SpawnIntent` carries $(A, c^*, M, D, "eta")$ with `eta = 0` for Ready and the max-dep-ETA for forecast; the eta's *source* (fitted curve vs substitution prior) is a scheduler-side type, not a wire field — the controller's per-cell gate below is source-agnostic. An intent contributes to cell $(h, "cap")$'s demand only when $"eta" < "lead_time"[h, "cap"]$, so a slow-boot cell such as `metal` starts provisioning earlier than a fast one such as `ebs`. During the interval where only the slow cell's window is open the intent's demand is unambiguously that cell's, shrinking cross-$A$ ambiguity to the fastest cell's window.

*Affinity encoding* maps each `hw_class` to a node-label conjunction via `sla.hwClasses: {h → [{key, value}...]}`. For example, `intel-8 → [{instance-cpu-manufacturer: intel}, {instance-generation: "8"}]` on EKS, or `genoa → [{rio.build/hw-tier: genoa}]` on operator-labeled k3s. $A$ serializes as one `nodeSelectorTerms` entry per *$(h, "cap")$ cell* — OR'd across terms, AND'd within — each term carrying that $h$'s label conjunction *plus* `karpenter.sh/capacity-type = cap`. The capacity-type must be in the term: the envelope solve may admit $(h, "od")$ while rejecting $(h, "spot")$ on interruption tail, so encoding per-$h$ only would let a pod with $A = {(h_1, "od")}$ land on a warm $h_1$-spot node and violate p99. On the wire this is `SpawnIntent.node_affinity: repeated NodeSelectorTerm`, a new field; the v1.0 `node_selector` map stays for compat. When $D dot.op "headroom" + "budgets"$ exceeds the EBS-root `volumeSize`, `rio.build/storage = nvme` is appended to every term. The *emitted* request is $min(D dot.op "headroom" + "budgets", "maxDisk")$, clamped so it always schedules; when the clamp binds, headroom shrinks toward 1.0 and ENOSPC-retry covers the rare actual-overflow during low-$n_"eff"$ exploration. Config-load asserts `maxDisk <= dataVolumeSize - kubeletReserve`.

#memo(title: [Why one DAG layer])[
  A `Queued` dep has no progress-grounded ETA; propagating $"ETA"(B) = "ETA"(A) + T_"ref" (B)$ compounds one $sigma_"resid"$ per hop, and the seconds-mass of $T_"ref"$ (trivial drvs near zero — §Cold start `preferLocalBuild`) would admit wide-fanout chains (a single batch RPC fanning out to tens of thousands of trivial intents). Layer-$N+2$ is provisioned reactively when layer-$N+1$ starts; the marginal latency a transitive forecast would save is $<= min(T_"ref" (B), "lead_time")$, which the #qty("10", "s") poll amortizes for short $B$. A bounded-$k=2$ extension is one `kahn_topo` pass if mid-weight chains ($T_"ref" tilde.op$ #qtyrange("30", "60", "s")) prove common.

  The substitution prior is *not* an exception to this cutoff: a dep with an active materialization job resolves on the store plane directly, independent of its own subtree, so the contribution is job-grounded direct evidence — one fold, no propagation, no $sigma_"resid"$ compounding. That is why a `Queued` dep with an active job contributes the same prior while a `Queued` dep without one still kills the walk.
]

The *NodeClaim pool* (@fig-provisioning, @alg-pool) is reconciled each tick by simulating placement via *first-fit-decreasing* — Ready before forecast, large before small — so the deficit is the *unplaced residual*, not a per-cell average. An intent with $A = {h_1, h_2}$ that fits on an existing $h_2$ node consumes no $h_1$ demand.

The sim matches real placement only if kube-scheduler uses the same item order *and* bin-selection rule. Both are achieved with *zero custom scheduler code*. For *item order*, the controller stamps each builder pod with a `PriorityClass` whose `value` equals its $c^*$-bucket. The chart ships a *fixed* set of 10 PriorityClasses — buckets $0..9$, covering $c^* in [1, 1023]$ — regardless of `maxCores`. Config-load asserts `maxCores < 1024`, `globalDefault: false`, `preemptionPolicy: Never`, so a runtime `maxCores` increase cannot reference a non-existent class; the apiserver Priority admission plugin would otherwise *reject* pod-create. The in-tree `PrioritySort` queue plugin @k8s-prioritysort then dequeues largest-$c^*$-bucket first. Within a bucket the order is FIFO, so the sim's exact-$c^*$ sort packs slightly tighter than reality; `placement_sim_mismatch_total` covers the residual. For *bin selection*, the chart deploys a second `kube-scheduler` instance — `schedulerName: rio-packed`, *stock upstream image*, config-only `NodeResourcesFit.scoringStrategy: MostAllocated` — because on EKS the managed control-plane scheduler's `LeastAllocated` default is immutable @eks-roadmap-1468. *The sim's bin-selection rule is `MostAllocated`*: $op("argmax", limits: #true)_n sum "requested" slash "allocatable"(n)$ after placement, matching kube-scheduler's `allocatable` divisor rather than `capacity`. It is not $argmin "price"$. Placing on existing nodes is sunk-cost, so the sim must predict *which* pods fit, and for that its rule must match kube-scheduler's rather than optimize independently.

Residual sim/placement divergence — tie-breaks, races with consolidation, in-flight NodeClaims the sim projected as capacity but kube-scheduler cannot yet bind to — surfaces as `placement_sim_mismatch_total`, and the next-tick deficit loop covers it. Pods are *Pending at most one reconcile tick* for capacity, not never-Pending.

Unplaced intents are routed to their cheapest open $A$-cell and covered with new NodeClaims: one *anchor* — the smallest type fitting $max_U c^*$ — plus $ceil("remaining" slash "bulk.cores")$ of the *bulk* type at best \$/core. Core-waste is bounded by $"anchor.cores" + "bulk.cores"$: anchor over-fit can exceed one *cores*-menu-step when $M$ or $D$ forces a wider type, so the bound is the anchor's full core count plus the bulk ceiling. The asymptotic claim — cost-ratio $-> 1$ as $sum_U c^* -> infinity$ — holds; the constant is type-menu-dependent. NodeClaim `requirements` carry the cell's $h$-label conjunction plus the instance-type set, and Karpenter's `CreateFleet(Type=instant, AllocationStrategy=price-capacity-optimized)` @karpenter-createfleet retains within-cell spot diversification and per-@az fallback.

*Pod creation is gated on placement.* Builder pods are created only for Ready intents that the FFD sim placed on a *Registered* node (`placeable` in @alg-pool excludes intents the sim placed on in-flight NodeClaims), with `nodeAffinity` over $A$. An `unplaced` Ready intent gets a NodeClaim this tick and a pod once that NodeClaim registers. The first Ready layer of a fresh DAG, which no forecast preceded, pays one boot before its pods exist; layers $>= 2$ are forecast-warmed so their pods bind immediately. A #qty("192", "core") node provisioned for one large forecast intent hosts subsequent small builds until empty.

#memo(title: [Karpenter contract — shim NodePool, no patch])[
  NodeClaims reference EC2NodeClass directly (`rio-nvme` / `rio-default` by storage); the controller stamps `rio.build/*` + `karpenter.sh/nodepool: rio-nodeclaim-shim` on `metadata.labels` (Karpenter copies NodeClaim labels → Node; `rio.build/*` would otherwise come from a NodePool template). The *shim* NodePool exists only to satisfy Karpenter's state-tracking lookup (which otherwise logs `NodePool "" not found` at ERROR per NodeClaim per backoff-tick); it has `limits: {cpu: 0}` so Karpenter's provisioner never creates from it, and `disruption.budgets: [{nodes: "0"}]` so the disruption controller considers but never acts — rio owns deletion. Karpenter's *liveness* controller is not nodepool-gated and deletes any NodeClaim whose `Registered` stays non-True past a hardcoded #qty("15", "min") @karpenter-liveness; rio's @ice timeout ($q_0.99 ("boot")$ — empirically #qty("33", "s") on `m7i.large`) fires well before that, so rio always reaps stuck NodeClaims first and no Karpenter patch is needed. On non-Karpenter deployments the NodeClaim layer is a no-op (nodes pre-exist); the same `nodeAffinity` lands pods on operator-labeled nodes via kube-scheduler.
]
*Lead time is learned*, not configured. The success condition is $"boot" - "eta_error" <= "lead_time"$: a node created at $"forecast_eta" - "lead_time"$ is ready at that plus `boot`, and the build is ready at $"forecast_eta" + "eta_error"$. So $"lead_time"[h, "cap"] = q_0.9 ("boot"[h, "cap"] - "eta_error")$ — a quantile target on the *paired* difference. The closed-loop SLI below tunes the quantile, so 0.9 is the seed, not a cost-derived critical fractile.

Both signals are observable for the same intent at completion. `boot` is NodeClaim `Registered.time − creationTimestamp`. `eta_error` is actual-ready − forecast-ETA, recorded *only for intents whose pod bound to a NodeClaim created during that intent's forecast window* — warm-node placements have no fresh `boot` to pair against — and *excluding* completions reached via the `failed_builders` retry path. A spot-interrupted dep at $T - epsilon$ has `eta_error` $approx$ full-duration, polluting the sketch's left tail. The exclusion conditions on no-interrupt and so under-represents long completions by $tilde.op p dot.op (e^(sigma^2) - 1) in [0.05%, 8.7%]$; the closed-loop Schmitt's narrow-back arm corrects the resulting *over*-provisioning (provision-early idle), accepted as the lesser bias.

The controller maintains *two quantile sketches (HdrHistogram) per cell*: $z = "boot" - "eta_error"$ for `lead_time`, and `boot` alone for the @ice timeout and the consolidation break-even. The histogram is fixed-bucket at 2 significant figures over 1 ms–24 h (≈1% relative error, the same guarantee class as the DDSketch it replaced), merges exactly across replicas (bucket counts add), and its V2 wire format is a published cross-language spec. HdrHistogram is unsigned, so the signed $z$ is clamped at $0$ on record: a negative $z$ means the node beat its forecast and no lead is needed, and every consumer compares `eta < lead_time`, for which $0$ and negative are equivalent. To track regime shifts — boot distribution changes on AMI rollout, `eta_error` shrinks as fits mature — a *sliding pair* of sketches is kept per cell: the active sketch and a half-life-old shadow. On each half-life boundary, active replaces shadow and a fresh sketch becomes active, so the effective window is one half-life. Sketches serialize to @pg as `BYTEA` in the HdrHistogram V2 format, prefixed with a `u32` schema-version tag so a format or bucket-config change deserializes to seed-fallback rather than silently wrong quantiles, alongside `hw_cost_factors`.

The sketches are *seeded* from `sla.leadTimeSeed[h, cap]`, operator-supplied. `xtask k8s probe-boot` creates one NodeClaim per configured cell, reports the single-observation boot, and deletes. It also serves as the Karpenter naked-NodeClaim conformance check on version bumps and verifies the `limits:{cpu:0}` shim NodePool causes Karpenter's provisioner to skip rather than try-and-fail. Each sketch initializes with the seed at synthetic count $n_"seed" = 1 slash (1 - q) = 10$, the minimum for a stable $q_0.9$ (cf. §Coupled-model's $n_"eff" >= 3$ gate). The scheduler reads `lead_time` from the same @pg row, or directly from `sla.leadTimeSeed` in phase 13a where the controller-side sketch is not yet running.

SLI #(refs.metric)("rio_controller_nodeclaim_forecast_hit_ewma")`{h, cap}` is the closed-loop check with a Schmitt deadband, the same $0.85 times$/$1.05 times$ as §Tier reassignment. A sustained dip below $0.85 dot.op "target"$ widens the quantile by $Delta q = 0.02$ per Schmitt firing, capped at $q <= 0.99$ and `lead_time` $<=$ `sla.maxLeadTime`. #(refs.metric)("rio_controller_nodeclaim_lead_time_q_at_cap")`{h,cap}` distinguishes "still adapting" from "structurally cannot cover": a fat-tailed `eta_error` the model cannot cover would otherwise ratchet $q -> 1$ and idle-cost unbounded. A sustained-high above $1.05 dot.op "target"$ narrows it back, so the estimator does not ratchet.

*Consolidation is also learned.* An empty NodeClaim of `cores` is kept while the per-cell idle-gap hazard satisfies
$
  lambda(t) dot.op EE[c_"arrival" dot.op bb(1){c_"arrival" <= "cores"}] > "cores" / q_0.5 ("boot"[h, "cap"])
$
— that is, while the *fitting* cores recovered by the next arrival's avoided cold-start exceed the marginal idle cores, evaluated at median boot. An arrival with $c^* > "cores"$ cannot land here and contributes $0$. The sketch yields quantiles, not means; for the right-skewed boot distribution median $<$ mean, so the keep-condition is *harder* to satisfy than the true expected-value break-even. This errs toward bounded idle spend at the cost of forgone warm-hit savings. The size term matters: a #qty("192", "core") node needs $tilde.op 48 times$ the arrival rate of a #qty("4", "core") node to justify the same idle.

The expected fitting-core term $EE[c_"arrival" dot.op bb(1){dots.c}]$ is the *current tick's* per-cell mean over `intents`, already computed for @alg-pool. It is *defined as $0$ when `intents` is $bot$ or empty*, so consolidate-only mode after a partition reaps all idle nodes — the intended bound-idle behavior.

The hazard $lambda(t)$ is estimated from `idle_gap[h, cap]` — time-to-next-pod after a node empties — via the *Nelson–Aalen* cumulative-hazard estimator, equivalently the discrete derivative of Kaplan–Meier @kaplan1958, with right-censored events. A node deleted at `consolidate_after` contributes a censored observation, so the estimator is consistent under arrival-rate stationarity: the censoring threshold is set from past data and the gap from workload arrival, hence non-informative when the hazard is stationary across the estimation window. Per-cell scoping limits the heterogeneity exposure, but the censoring threshold is itself derived from past hazard estimates, so a structural feedback remains. This is accepted because the consolidation threshold is a cost heuristic, not an inference target.

KM is non-identifiable past the largest *uncensored* gap, so `consolidate_after` cannot grow data-driven beyond what has been observed. With probability `sla.consolidateExploreEpsilon` (default $0.02$), an empty node is held to `sla.maxConsolidationTime` — or $2 times$ current `consolidate_after` if unset — to extend the observable horizon. A floor `consolidate_after` $>= q_0.5 ("boot"[h, "cap"]) slash 2$ prevents a transient lull from collapsing the threshold to always-delete; recovery from that state takes $O(1 slash epsilon)$ hold-open events. For DAG-shaped workloads the hazard is high right after a layer completes and drops sharply, so the learned threshold naturally tracks the inter-layer gap.

#figure(
  caption: [v1.2 forecast-driven provisioning. The scheduler emits Ready *and* forecast intents; the controller's NodeClaim-pool reconciler covers the per-$(h, "cap")$ deficit `lead_time` ahead; kube-scheduler bin-packs Ready pods onto the warm pool. One inert shim NodePool (`limits=0`, `budgets=0`) satisfies Karpenter's state lookup; NodeClaims reference EC2NodeClass directly.],
  placement: auto,
  diagram(
    spacing: (22mm, 12mm),
    node-stroke: 0.5pt,
    node(
      (0, 0),
      align(center)[DAG\ #text(size: 0.7em)[Ready ∪ forecast]],
      corner-radius: 3pt,
    ),
    node(
      (1, 0),
      align(center)[`SlaEstimate`\ #text(size: 0.7em)[$-> (A, c^*, "eta")$]],
      corner-radius: 3pt,
    ),
    node(
      (2, -0.5),
      align(
        center,
      )[NodeClaim pool\ #text(size: 0.7em)[FFD pack, learned\ `lead_time` / consolidate]],
      corner-radius: 3pt,
    ),
    node(
      (2, 0.5),
      align(center)[Pod (Ready only)\ #text(size: 0.7em)[`nodeAffinity:` $A$]],
      corner-radius: 3pt,
    ),
    node(
      (3, -0.5),
      align(center)[`CreateFleet`\ #text(size: 0.7em)[price-cap-opt]],
      corner-radius: 3pt,
      stroke: gray,
    ),
    node(
      (3, 0.5),
      align(center)[kube-scheduler\ #text(size: 0.7em)[bin-pack on pool]],
      corner-radius: 3pt,
      stroke: gray,
    ),
    edge((0, 0), (1, 0), "-|>"),
    edge((1, 0), (2, -0.5), "-|>", text(size: 0.7em)[all intents]),
    edge((1, 0), (2, 0.5), "-|>", text(size: 0.7em)[`eta = 0`]),
    edge((2, -0.5), (3, -0.5), "-|>"),
    edge((2, 0.5), (3, 0.5), "-|>"),
    edge(
      (3, -0.5),
      (3, 0.5),
      "..>",
      text(size: 0.7em)[warm node],
      label-side: right,
    ),
  ),
) <fig-provisioning>

*Capacity backoff.* @ice fires at NodeClaim time, not pod-Pending: when a NodeClaim's `Launched` condition goes `False` (Karpenter exhausted `CreateFleet` retries), or `Registered` is not `True` past $cases(2 dot.op "leadTimeSeed"[h, "cap"] & "if" n_"real" < 100, q_0.99 ("boot"[h, "cap"]) & "otherwise")$ ($q_0.99$ needs $>= 1 slash (1 - 0.99) = 100$ real observations to be a sample upper bound with reasonable probability; below that the seed-derived floor protects against premature @ice on a slow @az), the controller deletes the NodeClaim and reports that $(h, "cap")$ cell as `unfulfillable` in its next `AckSpawnedIntents` request. The *scheduler* owns the @ice state (in-memory, lease-holder only): on each `unfulfillable` report it marks the cell fleet-wide infeasible for #qty("60", "s") $-> #qty("120", "s") -> dots.c$ doubling per consecutive @ice on the same cell, capped at `sla.maxLeadTime`, reset on first success. The mask is *read-time*: the memo holds the full-$H$ solve and is never overwritten; each dispatch computes $A without "ice_masked"$, so unmasking is free. The controller does not write @pg; @ice ownership stays where the lease is. Because provisioning runs `lead_time` ahead, a capacity-dry cell is detected *before* any Ready intent depends on it — the next `GetSpawnIntents` already excludes it. A spot interruption mid-build records the interrupt for $(h, "spot")$ per §Duration distribution (incrementing the $lambda[h]$ numerator); the build's `failed_builders` retry path re-dispatches (and the pool reconciler covers the re-dispatch's intent on the next tick). The attempted $(h, "cap")$ cells are persisted on the @pg `builds` row. @ice state is per-cell and *not* in `inputs_gen`; the read-time mask touches only intents whose cached `A` intersects the masked cell. It is scheduler-side and reported to the controller via `GetSpawnIntentsResponse.ice_masked` each tick (so the controller's @alg-pool iteration excludes the same cells); it is in-memory only, so a scheduler lease handoff costs at most one wasted NodeClaim round per masked cell; the 5-consecutive-$bot$ counter (@sec-forecast) is similarly transient. *Two exhaustion exits:* (a) ladder *step* count reaches $min(ceil(max("tier bounds", "sla.ladderBudget") slash "lead_time" slash 4), 8)$ (the $slash 4$ caps capacity-retry latency at \~¼ of the tier's wall-clock budget), or (b) @ice has masked all of $H times {"spot","od"}$ — both route to demote (or `infeasible_total{reason=capacity_exhausted}` at the terminal tier), *never* to the best-effort fallback, which is reserved for envelope-infeasibility and emits with $A = H$. #(refs.metric)("rio_scheduler_sla_hw_ladder_exhausted_total")`{tenant,exit}` distinguishes the two.

*$h$-sample sufficiency and $epsilon_h$-exploration.* With the $K = 3$ mixture (§Hardware heterogeneity), $bold(alpha)["pname"]$ is identified once $>= K$ distinct non-reference $h$ with sufficient angular spread are observed for that pname, and then *generalizes to unseen $h$* ($bold("factor")[h_"new"]$ is measured at boot, not learned from pname samples). The per-cell `bias` table is residual, so its starving costs at most the rank-$K$ approximation error plus any per-phase heterogeneity. PARIS @paris2017 reports cross-hardware prediction RMSE in this range on its fingerprint basis; *the bound is unmeasured here* and is one of the four §Phasing-13a empirical gates. kube-scheduler bin-packing and @ice\-masking supply *some* cross-$h$ observations, but when one cell durably price-dominates neither fires, and delegating the pick to `CreateFleet` (price-capacity-optimized) would just re-select the dominant cell. So with probability `sla.hwExploreEpsilon` (default $0.02$) per intent, the scheduler *pins* a single $h_"explore" tilde.op "Unif"(H without A)$ ($A$-cells already get organic samples; falls back to $H without {argmin_H "price"}$ if $A = H$), restricts the solve to $(h_"explore", *)$ and emits its resulting $A' subset.eq {h_"explore"} times {"spot","od"}$ (capacity-type still cell-encoded), and *solves $c^*$ for that $h_"explore"$* (so the SLA envelope holds on placement; the $epsilon_h$-fraction does not erode tier $p_q$ bounds). $epsilon_h$-exploration provides cross-$h$ *coverage* (so the rank gate can pass); the $c arrow.l.r h$ *deconfounding* comes from the $c$-residualization in the $bold(alpha)$-fit (§Hardware heterogeneity), not from the randomization. Cost: $epsilon_h$ of dispatches at a uniformly-random cell's $EE["cost"]$ instead of the optimal; bounded by $epsilon_h dot.op (max_H EE["cost"] slash EE^"min" - 1)$. *The coin is outside the memoization and deterministic in `drv_hash`; the drawn $h_"explore"$ is pinned in the SolveCache `MemoEntry`*: the memo caches the deterministic `(c*, A)` from the full solve; each dispatch reads the cached `A`, draws the $epsilon_h$ coin from a PRNG seeded with $"hash"("drv_hash")$, and on a hit emits the *pinned* $h_"explore"$ (or draws and pins one if unset), solving only $(h_"explore", *)$. The pin is *carried across memo invalidation* — `inputs_gen` governs memo staleness only, not selector identity — so `compute_spawn_intents` stays deterministic given $("DAG state", "fit cache")$ and the controller's selector-drift reap never deletes an explore Job mid-provisioning on a `solve_relevant_hash` projection bug. The pin clears (re-drawn from $H without A$ on the next $epsilon_h$ hit) when the pinned class graduates into $A$ or is removed from $H$; this is the over-stickiness release valve so hot pnames don't explore exactly one class per process lifetime. The cached `A` is never overwritten by an exploration result, so subsequent dispatches of the same key still see the cost-filtered $A$. On cache miss (first dispatch, $A = emptyset$) or $A = H$ the draw falls back to $H without {argmin_H "price"}$.

#algorithm(
  caption: [Controller NodeClaim-pool reconcile, per tick. First-fit-decreasing simulation mirrors kube-scheduler (Ready before forecast, large before small) so the deficit reflects what placement will actually do; cells in the same $A$ share free capacity. $A_i^"open" := cases(i.A & "if" i."eta" = 0, {(h,"cap") in i.A : i."eta" < "lead_time"[h,"cap"]} & "otherwise")$ is the per-cell gate (Ready intents always have a non-empty open set; forecast intents with $A_i^"open" = emptyset$ fall to `unplaced` and the per-cell deficit step's $A_i^"open" != emptyset$ guard skips them — revisited next tick). `menu[h, cap]` is the instance-type set for that cell from `sla.hwClasses` × `hw_cost_factors`. `consolidate_after(h, cap, cores)` is *computed* per tick — the first $t$ at which the @sec-forecast break-even inequality flips, given the current KM $hat(lambda)(t)$ and the current-tick $EE[c_"arrival" dot.op bb(1){dots.c}]$ — not a stored table.],
)[
  + *function* #smallcaps[ReconcilePool]$("intents", "live")$ #rann[`intents = ⊥` on RPC error — distinct from $emptyset$]
    + *if* $"intents" = bot$ *then* skip to consolidate-only after $5$ consecutive $bot$ ticks #rann[demand unknown; bound idle on partition]
    + $"free"[n] <- cases("allocatable"(n) - "requests"(n) & "if Registered", "spec.resources.requests"(n) & "if in-flight")$ #rann[$("cpu","mem","disk")$ tuple; project capacity]
    + $"unplaced" <- emptyset$; $"placeable" <- emptyset$
    + *for each* $i in "intents"$ sorted by $(i."eta" = 0, i.c^*)$ descending *do* #rann[Ready first, then FFD]
      + $n <- op("argmax", limits: #true)_(n in "live": "cell"(n) in A_i^"open" and "free"[n] >= (i.c^*, i.M, i.D)) ("requested"(n)."cpu" + i.c^*) slash "allocatable"(n)."cpu"$ #rann[component-wise fit; `MostAllocated` cpu-weighted, `allocatable` divisor]
      + *if* $n$ exists *then* $"free"[n] <- "free"[n] - (i.c^*, i.M, i.D)$; $"placeable" <- "placeable" union {(i, "Registered"(n))}$
      + *else* $"unplaced" <- "unplaced" union {i}$
    + *create pods* for ${i : (i, "true") in "placeable" and i."eta" = 0}$ #rann[bind-ready only; layer 1 of a fresh DAG waits one boot]
    + *for each* $(h, "cap")$ minus @ice\-masked *do*
      + $U <- {i in "unplaced" : A_i^"open" != emptyset and (h, "cap") = argmin_((h', "cap"') in A_i^"open") "price_per_core"(h', "cap"')}$
      + *if* $U = emptyset$ *then continue*
      + $"anchor" <- argmin_(t in "menu"[h,"cap"]: t >= (max_U i.c^*, max_U i.M, max_U i.D)) t."price_per_core"$ #rann[component-wise; if no single $t$ fits all maxes, sort $U$ on the overflowing dimension, bisect, recurse (base case: $|U| = 1$ with no fitting $t$ surfaces as `placement_sim_mismatch_total{reason=menu_gap}`; controller config-load asserts $max_"menu" >= ("maxCores", "maxMem", "maxDisk")$ to make this unreachable absent split-brain config) — `created_this_tick` accumulates across recursions; $N$'s min is the two-term law $min(n_"pack", floor("budget" slash "chunk"))$ (live_049 L1: the flat per-cell-per-tick cap is retired — `ctrl.nodeclaim.mint-deficit-proportional`)]
      + $"bulk" <- argmin_(t in "menu"[h,"cap"]: t."mem" slash t."cores" >= "median"_U (i.M slash i.c^*)) t."price_per_core"$, *or* $"anchor"$ if filter empty
      + $"budget" <- "sla.maxFleetCores" - sum_"Registered" "allocatable"(n)."cpu" - "in-flight" - "created_this_tick"$; *if* $"budget" <= 0$ *then* skip cell #rann[`created_this_tick` initialized to 0 at function entry; cells iterated round-robin from rotating start so no cell starves under sustained pressure]
      + *if* $"budget" < "anchor.cores"$ *then* skip cell
      + $N <- min(1 + ceil(max(0, sum_U i.c^* - "anchor.cores") slash "bulk.cores"), 1 + floor(max(0, "budget" - "anchor.cores") slash "bulk.cores"))$ #rann[$>= 1$; the two-term law --- demand and budget, no flat per-tick cap (RETIRED, live_049 L1, `ctrl.nodeclaim.mint-deficit-proportional`); mem/disk under-coverage from $sum c^*$-only $N$ self-corrects next tick]
      + create NodeClaims: $1 times "anchor"$ then $(N - 1) times "bulk"$; `created_this_tick += anchor.cores + (N-1)·bulk.cores` #rann[core-waste $<= "anchor.cores" + "bulk.cores"$; the create burst is budget-shaped --- `CreateFleet` rate pressure is absorbed by Karpenter batching + cloud-provider backoff (worst-case pricing at the component recap's config row)]
    + $"reserved" <- {n : exists thin i, "FFDsim placed" i "on" n}$ #rann[Ready *or* forecast — has demand this tick]
    + *for each* $n in "live"$, $n$ NodeClaim-backed, $"Registered"(n)$, $n in.not "reserved"$, $"occupancy"(n) = 0$ *do*
      + $"threshold"(n) <- "consolidate_after"("cell"(n), "cores"(n))$, *or* `sla.maxConsolidationTime` if $n in "hold-open"$ #rann[$n$ enters `hold-open` at each $1 -> 0$ occupancy transition with prob. $epsilon_"consolidate"$, persisted as node annotation, *cleared whenever $"occupancy"(n) > 0$* (level-triggered, so a between-tick blip clears); not re-drawn per tick]
      + *if* $"idle"(n) > "threshold"(n)$ *then* delete $n$
    + *for each* $n in "live"$, $n$ NodeClaim-backed, with kubelet `NotReady` for $> 2 dot.op q_0.9 ("boot"["cell"(n)])$, *or* $n$ marked wedged by the controller's open-attempt clustering (#rref("ctrl.nodeclaim.wedge-cluster"): $>= 2$ distinct derivations whose open attempts ran out their intent deadlines on $n$ inside the 30-minute window, node attribution from the ledger's `source_node` only) — *and* the controller requires kubelet `Ready` for this arm (so an apiserver↔node partition does not evict a healthy node), *and* per-tick fleet-wide reaping is capped at $min(3, ceil(0.05 dot.op |"live"|))$ nodes (so a single-tick blip cannot cascade) — the evidence is ledger facts the controller reads itself; the removed `GetSpawnIntentsResponse.dead_nodes` plays no part since the scheduler-side heartbeat detector retired (the 1d proto sweep deleted the field; number reserved) *do* cordon, evict, delete $n$ #rann[Karpenter disruption gated; rio owns health incl. kubelet-up-runtime-hung]
    + #text(
        size: 0.85em,
      )[#rann[$"idle"(n) := "now" - max({"pod.deletionTimestamp on " n} union {n."status.registeredAt"})$, so lease-handoff does not reset and forecast nodes have a defined value]]
] <alg-pool>

=== Full sizing algorithm

#algorithm(
  caption: [Per-dispatch sizing, full v1.2 — extends @alg-estimate with the per-$(h, "cap")$ envelope solve and admissible-set emission. $H$ is `sla.hwClasses` minus @ice\-backed-off cells.],
)[
  + *function* #smallcaps[SlaEstimate]$("drv")$
    + $"fit" <- "Estimator.cached"[("drv.pname", "drv.system", "drv.tenant")]$
    + *if* $"fit" != emptyset and "fit".D + "budgets" > "maxDisk"$ *then* fail `disk_ceiling`
    + *if* `drv.enableParallelBuilding` is `false` *then* seed $macron(p) := 1$
    + *if* $"fit" = emptyset or "fit".n_"eff" < 3 or ("fit.span" < 4 and not "fit.frozen")$ *then return* #smallcaps[Explore]$("fit")$ #rann[@alg-explore]
    + $C <- min("fit".macron(p), "fit".c_"opt", "maxCores")$
    + *if* $|H| > 1$, *with prob.* $epsilon_h$: $h_"explore" tilde.op "Unif"(H without A_"cached")$ #rann[cached $A$ from memo; $H without {argmin_H "price"}$ on miss or when $A = H$]; solve only $(h_"explore", *)$ below; *if* feasible *return* its $"PodSpec"{c^*, ..., A'}$ #rann[$A' subset.eq {h_"explore"} times {"spot","od"}$; capacity-type encoded; if $h_"explore"$ infeasible at every tier, abandon the draw and run the unrestricted solve]
    + *for each* $"tier" in "sla.tiers"$ tightest-first *do*
      + $"candidates" <- emptyset$
      + *for each* $(h, "cap") in H times {"spot","on-demand"}$ *do*
        + *if* $"cap" = "spot" and p(C; h, lambda[h]) > 0.5$ *then continue* #rann[spot infeasible]
        + $c_"lo" <- "if" "cap" = "spot" "then" ceil(c_(lambda, h)) "else" 1$
        + $c^* <- min thin c in [c_"lo", C]$ s.t. \ #h(2em) $forall (q, "bound") in "tier": #smallcaps[Quantile] _(h,"cap")(q; c) <= "bound"$ #rann[@alg-quantile]
        + *if* $c^*$ exists *and* $M(c^*) dot.op "headroom" <= "maxMem"$ *then*
          + $"candidates" <- "candidates" union {(h, "cap", c^*, EE["cost"]^"upper")}$
      + *if* $"candidates" != emptyset$ *then break*
    + *if* $"candidates" = emptyset and$ all of $H$ ICE-masked *then* demote / fail #rann[§Hardware-class targeting]
    + *if* $"candidates" = emptyset$ *then return* best-effort $"PodSpec"{C, ..., "nodeAffinity": H}$
    + $EE^"min" <- min_"candidates" EE["cost"]^"upper"$
    + $A <- {(h, "cap") in "candidates" : EE["cost"]^"upper" <= (1 + tau) dot.op EE^"min"}$ #rann[$tau$ = `sla.hwCostTolerance`]
    + $c^* <- max_((h, "cap") in A) c^*_(h, "cap")$ #rann[SLA-correct on slowest $h in A$]
    + $A' <- {(h, "cap") in A : exists "type fitting" (c^*, M(c^*)) and EE["cost"]_(h, "cap")^"upper" (c^*) <= (1+tau) EE^"min" and c^*_(h, "cap") >= c^* slash k}$ #rann[re-check fit, cost, capacity-ratio at $c^*$; $k=2$]
    + #text(
        size: 0.85em,
      )[#rann[the argmax cell survives all three checks ⇒ $A' != emptyset$ provably]]
    + *return* $"PodSpec"{c^*, M(c^*) dot.op "headroom", min(D dot.op "headroom" + "budgets", "maxDisk"), "nodeAffinity": A'}$
] <alg-estimate-full>

== Cold start and priors

With zero samples, the first run uses an operator-configured probe. Derivations whose ATerm output spec carries a fixed hash (@fod:pl) are routed to the *fetcher* pool regardless of `pname` (many `fetch*` set `pname`; the discriminator is the output spec, not env). They still write `build_samples` and learn per-key $T(c)$ (e.g. `fetchCargoVendor` parallel-unpacks hundreds of archives — not $c$-invariant); they are excluded only from the *fleet-aggregate prior* (network-dominated FODs would skew it). Derivations without a `pname` fall back to `name` (which every derivation must set) — a less stable key (version-suffixed, so a per-version cold start), but per the implementation's "some history beats none" stance.

Nix derivations carry structure the model can use to skip exploration:

- *`enableParallelBuilding`* (#(refs.gh)("rio-gateway/src/translate.rs:452")): when explicitly false, *seed* $macron(p) = 1$ (the saturation gate still runs and can revise upward, since non-stdenv builders — Go, Rust, Bazel — parallelize regardless of this stdenv-only flag). Absent (the historical stdenv default) is treated as unknown, since nixpkgs is migrating to `enableParallelBuildingByDefault=true`. The `enableParallelChecking` conjunction applies only in @alg-estimate-full (§Phasing-13a).
- *`preferLocalBuild`*: trivially-short drvs (`writeText`, `symlinkJoin`). Short-circuit to a fixed minimal probe; writes a `build_samples` row but is excluded from the fit and the fleet-aggregate prior.
- *`requiredSystemFeatures`*: `sla.featureProbes: {feature → {cpu, memPerCore, memBase}}` generalizes the `big-parallel` special case — `kvm`/`nixos-test` map to high-`memBase`/low-`cpu` (qemu guest RAM dominates), `benchmark` maps to on-demand-only. Config validation enforces `featureProbes[*].cpu <= maxCores/4` (the same span-reachability invariant as `sla.probe.cpu`; without it, a `featureProbes.big-parallel.cpu = maxCores` first-run would freeze at `span = 1` and yield a rank-1 fit).

#r("sched.sla.cores-reach-nix-build-cores")

The chosen $c^*$ is plumbed to the build via *`wopSetOptions.buildCores`* (#(refs.gh)("rio-builder/src/executor/mod.rs:791")) so `NIX_BUILD_CORES` $= c^*$ inside the sandbox — the pod's `resources.requests.cpu` alone does not reach the build's `-j`.

The probe is expressed as `{cpu, memPerCore, memBase}` — not absolute memory — so that when the loop bumps cores after a single sample (no $M(c)$ slope fittable yet), it requests $"bumped_"c dot.op "memPerCore" + "memBase"$ rather than replaying run-1's peak and OOMing. The probe's linear form is for operator intuition; once $n_"eff" >= 3$, the log-linear $M(c)$ fit (parameters $a, b$) supersedes it. A second probe shape keyed on `requiredSystemFeatures ∋ big-parallel` is optional.

== Fleet-learned priors

The operator-configured `memPerCore`/`memBase`/`cpu` probe values are a *bootstrap*, not durable truth. Once enough pnames have a fitted $M(c)$ (threshold: 50, configurable), the scheduler computes a fleet-aggregate prior — *median over tenants* of each tenant's median $M(c)$ fit parameters and converged-$c^*$ (one vote per tenant; see §Threat model for why) — and uses that as the cold-start probe instead. Median, not mean, so a handful of outliers (LLVM, chromium) don't drag the prior.

The fleet prior is *clamped to [#mul(0.5), #mul(2)] of the operator-configured value*. Median is robust to a few outliers but not to systematic skew — if the first 50 fitted pnames happen to be LLVM-tier, an unclamped median would triple-size every cold start. The clamp makes operator config a guardrail rather than dead config: the system self-tunes within the band, and #(refs.metric)("rio_scheduler_sla_prior_divergence")`{param}` fires when the fleet median hits the clamp so the operator knows to widen it.

== Seed corpus

A new cluster has no `build_samples`, so every package pays the 2–3-run exploration cost. An operator standing up a second region, a staging mirror, or a fresh cluster after a migration already _has_ fitted curves in the old deployment — they should carry over.

`rio-cli sla export-corpus [--tenant=<t>] [--min-n=3]` dumps fitted `{pname, system, version, S, P, Q, p̄, a, b, α, n_eff, ref_hw_class, ref_factor_vec}` rows to a JSON file (`version` so the importing cluster can apply $"vdist"$ down-weighting; without it a stale curve pools at full $n_0$ weight). ($bold(alpha)$, `ref_factor_vec` are §Phasing-13a targets; phase-12 export omits them.) Curves are in reference-seconds (§Hardware heterogeneity), so a corpus exported on Graviton4 imports correctly on Sapphire Rapids — the importing cluster rescales by $(bold(alpha) dot.op bold("factor")_"old_ref") slash (bold(alpha) dot.op bold("factor")_"new_ref")$ per pname. `sla.seedCorpus: <path>` (or `rio-cli sla import-corpus`) loads it on the new cluster. A seed entry is the highest-priority $hat(theta)_"prior"$ source in the partial-pooling blend (below): the new cluster starts from the old cluster's curves and pools toward its own fit as $n_"eff"$ grows, instead of cold-probing every package.

Privacy is a non-issue for the primary use case — the operator is moving their own data between their own clusters. The export omits raw samples (timing/frequency would leak tenant activity if the file _were_ later shared) and includes only fitted parameters. An operator who wants to publish their corpus (e.g., a community nixpkgs reference set) can scrub pnames before doing so; rio does not ship one.

#r("sched.sla.prior-partial-pool")

Priors are not a hard precedence switch but *partial-pooled*:

$
  hat(theta) = (n_"eff" dot.op hat(theta)_"pname" + n_0 dot.op hat(theta)_"prior") / (n_"eff" + n_0)
$

with $n_0 approx 3$ pseudo-counts (an empirical-Bayes shrinkage in the style of @gelman2007[§12]), where $hat(theta)_"prior"$ is the first available of seed-corpus → fleet-aggregate (clamped) → operator config, *each first converted to the $(S, P, Q, a, b)$ basis* so they're commensurable for pooling. The operator's linear probe `memBase + c·memPerCore` has no exact image in the power-law basis, so it is approximated by fixing $b = 1$ (linear scaling) and $a = ln("memBase" + "memPerCore")$ to match $M(1)$ (deliberately over-estimating by $"memBase" dot.op (c-1)$ at $c > 1$ — conservative during the $n_0$-dominated phase); $P = ("tier.p90" slash 2) dot.op "probe.cpu"$ (a central estimate), $Q = 0$, and $S = "tier.p90" slash 2$ so prior $T_"min" = S + P slash "maxCores" in ["tier.p90" slash 2, 5 dot.op "tier.p90" slash 8]$ (the bootstrap's $>= 100$-survivor gate then suppresses promotion until $hat(S)$ is data-dominated). This smooths the cold-start cliff (a pname at $n_"eff" = 1$ is mostly prior; at $n_"eff" = 10$ mostly its own fit) without wasting the prior's information once samples arrive.

The line this draws: operators own *intent* (tier boundaries, `maxCores`/`maxMem`) and the system owns *mechanism* (probe shape, slope priors). Operator config for mechanism is scaffolding the system climbs off of.

== Threat model

`pname` is read from `drv.env["pname"]` (#(refs.gh)("rio-gateway/src/translate.rs:452")) — submitter-controlled. A hostile derivation can set `pname="hello"`, spin-loop 96 cores for an hour, and poison the model that every other tenant's `hello` build reads from. Builders are untrusted by design — they are network-isolated with no apiserver access; the cgroup readings themselves are trustworthy (the @supervisor reads them, not the build payload), but they measure _consumption_, not _useful work_ — a spin-loop is indistinguishable from a real compile at the cgroup layer.

#warning(title: [What the model cannot defend against])[
  cgroup counters measure *consumption*, not *useful work* — a spin-loop is indistinguishable from `clang` at this layer. The tenant-scoped key and outlier rejection bound *exploration-phase* damage; steady-state defense is *tenant billing*, not the model. Read this section as "blast-radius containment", not "attack prevention".
]

Mitigations, both required:

- *Tenant-scoped model.* The fit key is `(pname, system, tenant)`, not `(pname, system)`. A tenant can poison only its own curves. The cross-tenant fleet-aggregate prior is computed as *median-over-tenants of per-tenant medians* (one vote per tenant — a Sybil-@pname spammer cannot capture >50% of rows by registering many keys) and clamped to $[#mul(0.5), #mul(2)]$ of operator config; the clamp is the load-bearing bound (worst case: #mul(2) over-provision of every cold start).
#r("sched.sla.outlier-mad-reject")
- *Outlier rejection on ingest.* Once a key has @neff $>= 5$, a completion sample whose log-residual exceeds $3 dot.op 1.4826 dot.op max(MAD, hat(sigma)_"resid" slash 1.4826, delta t slash "wall_secs")$ with $delta t = 1$s the cgroup poll interval, is recorded but *excluded from the fit* (#gls("mad")-based; the $hat(sigma)$-floor makes the threshold $approx 3 hat(sigma)$ when MAD degenerates, and the absolute floor covers the $hat(sigma) approx 0$ deterministic-build case). This increments #(refs.metric)("rio_scheduler_sla_outlier_rejected_total")`{tenant}`. Combined with the exploration freeze (@alg-explore), the per-key core-seconds a hostile drv can extract is bounded by $"buildTimeout" dot.op ("probe" + 4 dot.op "probe")$ on the up-path (one bump to span $>= 4$) — concurrent dispatches of the same key all read identical `FittedParams` and explore at the same level, so the practical limiter is the *tenant's billing quota*, not this bound. Post-freeze, a sustained spin-loop has $macron(p) approx "maxCores"$ and steady-state cost is `maxCores · buildTimeout` per dispatch — tenant billing remains the only steady-state limiter.

A content-addressed key (drv hash) was rejected: every drv hash is unique, so the model would never accumulate samples. `(pname, tenant)` plus outlier rejection is the accepted compromise: a tenant can mis-size its own builds, but not anyone else's. *Same-pname configuration variants* (`.override`, `pkgsCross`, `clangStdenv`, `doCheck`) share the key — the dip-test metric flags bimodality but the fit pools them; an input-closure-hash bucket as a fourth key dimension is §Future work.

#warning(title: [Untrusted-path gaps closed in §Phasing 13a])[
  Three sandbox-escape paths reach beyond per-tenant scope at `538458a5` and are *closed in 13a*, not the model:

  #table(
    columns: (auto, 1fr, 1fr),
    align: (left, left, left),
    table.header([Gap], [Exposure], [13a closure]),
    [(a)],
    [Read-path `AdminService` RPCs (`SlaStatus`/`SlaExplain`/`ExportSlaCorpus`/`ListSlaOverrides`/`ListTenants`/`ListPoisoned`) call only `ensure_leader`, not `ensure_service_caller`. `tenant` is a request body field, so an escaped builder can dump any tenant's `FittedParams`.],
    [Gate on `ensure_service_caller`.],

    [(b)],
    [`hw_perf_samples` is global with no tenant column. An escaped builder forging $bold("factor")$ with 3 distinct `pod_id` skews fleet-wide normalization; MAD-reject is per-`hw_class` and cannot detect a fresh class with only attacker rows.],
    [Add `submitting_tenant` column with per-tenant median-of-medians aggregation matching the fleet-prior pattern. Raise `FLEET_MEDIAN_MIN_TENANTS` $2 -> 5$ so two colluding tenants cannot capture the median.],

    [(c)],
    [`ImportSlaCorpus`/`sla.seedCorpus` reach the solver without finite/range checks.],
    [`is_finite`/range validation: `ref_factor_vec` per-dim $in [0.1, 10]$, $Q >= 0$, `n_eff` $<= 32$; $S, P in [0, "buildTimeout"_"ref"]$; $a, b in [0, "sla.maxMem"]$.],

    [(d)],
    [A tenant's wide layer-2 fanout emits forecast intents that consume shared `maxFleetCores`, capturing fleet capacity ahead of other tenants' Ready intents.],
    [`sla.maxForecastCoresPerTenant` ceiling; per tenant, Ready cores are subtracted from the budget before forecast intents are admitted.],
  )

  An attacker who spin-loops their own key to $c^* approx "maxCores"$ reaches bucket-9 and queue-jumps other tenants' lower-bucket pods at the kube-scheduler layer. `preemptionPolicy: Never` limits this to ordering (no eviction); the spin-loop is billed at `maxCores · buildTimeout` per dispatch, so steady-state is billing-bounded. Accepted as a latency-only impact.

  Additionally: `InjectBuildSample` becomes `#[cfg(feature = "test-fixtures")]` (currently runtime-env-gated). `Estimator.cache` gets a per-tenant LRU cap (`sla.maxKeysPerTenant`, default 50_000), and `pname`/`version` are length-clamped at 256B in `translate.rs` — without this, a tenant submitting random-pname drvs grows the leader's heap unbounded.
]

== Runtime overrides

Per-pname overrides are stored in a PG `sla_overrides` table and managed via `rio-cli sla override <pname> [--tier=T] [--p50|--p90|--p99=D] [--cores=N] [--mem=B] [--capacity=spot|on-demand] [--ttl=D]`. `rio-cli sla reset <pname>` (`AdminService.ResetSlaModel`) clears `build_samples` for the key — the runbook step for "model is wrong for pname X." The Estimator reads overrides on its existing \~#qty("60", "s") refresh tick alongside `build_samples`. `--tier` pins the target (system still solves for $c$); `--cores`/`--mem` pin the allocation directly (break-glass, bypasses the model). `--ttl` self-expires emergency overrides.

#r("sched.sla.override-precedence")

Resolution precedence for the *target tier*: override > learned (tightest feasible) > feature-probe match > global default.

== Observability

Per-decision: `sla_estimate` is `#[instrument]`-ed and emits a DEBUG span with `{pname, tier, c_star, fit_hash, hw_class, capacity_type, binding_constraint, prior_source, n_candidates_feasible}` (the `fit_hash` enables retrospective `sla explain --at <hash>` against the @pg `build_samples` snapshot). `rio-cli sla explain <pname>` dumps the full candidate table — $(h, "cap", c^*, EE["cost"], "binding_constraint", "rejected_reason")$ per row — and the prior-precedence trace, so an operator can reconstruct _why_ a build got the allocation it got.

// The five exception-counter names in the diagnostics bullet below share
// the `_sla_` infix; render via refs.metric so a rename breaks the docs
// build, and join for the compact display.
#let _sla-diag-counters = (
  "rio_scheduler_sla_suspicious_scaling_total",
  "rio_scheduler_sla_outlier_rejected_total",
  "rio_scheduler_sla_hw_cost_unknown_total",
  "rio_scheduler_sla_mem_fit_weak_total",
  "rio_scheduler_sla_als_round_cap_hit_total",
)

Metrics (per #cross-link("/spec/system/observability.typ")[observability spec] `rio_scheduler_` convention):
- #(refs.metric)("rio_scheduler_sla_prediction_ratio")`{dim=wall|mem}` — histogram of `actual / predicted`. Sustained skew off 1.0 is the *model-drift* alert; this is the "silently wrong" signal.
- #(refs.metric)("rio_scheduler_sla_residual_multimodal_total")`{tenant}` — incremented when Hartigan's dip test (a unimodality test @hartigan1985) yields $p < 0.05$ on log-residuals (often an upstream cache-hit/miss split, not something the model should fit). Exception-path only; pname identity is in the span.
- #(refs.metric)("rio_scheduler_resource_floor_bumps_total")`{reason=cgroup_oom}` — penalty-bump retries (`timeout` is the only other live label arm); `sla_prediction_ratio` is blind to censored samples so this is the under-provisioning signal.
- #(refs.metric)("rio_scheduler_sla_envelope_result_total")`{tier, result=hit|miss, constraint}` — the SLO outcome the operator's dashboard is built on.
- #(refs.metric)("rio_scheduler_sla_infeasible_total")`{tenant, reason=serial_floor|mem_ceiling|disk_ceiling|core_ceiling|interrupt_runaway|capacity_exhausted}`
- #(_sla-diag-counters.map(n => (refs.metric)(n)).join(", "))`{tenant}`; #(refs.metric)("rio_controller_ddsketch_seed_fallback_total")`{h,cap}`; #(refs.metric)("rio_scheduler_sla_hw_ladder_exhausted_total")`{tenant,exit}`
- #(refs.metric)("rio_scheduler_sla_prior_divergence")`{param}` (gauge), #(refs.metric)("rio_scheduler_sla_hw_cost_stale_seconds") (gauge)

Per-`pname` identity is in the per-decision span and `rio-cli sla explain`, not metric labels — `pname` is unbounded cardinality.

== Global-deployment compatibility

With `build_samples` in a global database (Aurora DSQL / Spanner per the multi-region plan), the model is shared by construction: a new region reads every existing curve on join, the fleet-aggregate prior spans all regions, and the seed-corpus mechanism reduces to a first-deployment / disconnected-install convenience. Reference-second normalization (above) is what makes pooled samples coherent across regions' hardware. Per-cluster differences are confined to *mechanism ceilings* — `sla.maxCores`/`sla.maxMem` and the `hw_cost_factors` table are per-cluster (instance availability and spot price vary by region/AZ) while tiers, headroom, and overrides are global. `sla_overrides` gains an optional `cluster` column for `rio-cli sla override --cluster=<name>` when a region-specific pin is needed.

== Bucketing

The 4Gi/2-core rounding in `BucketedEstimate` is removed. Requests are $(c^*, M(c^*) dot.op "headroom")$ rounded only to k8s-native granularity (millicores, bytes). Karpenter bin-packs heterogeneous requests at the node layer; pod-level quantization adds slack with no offsetting reuse under one-build-per-pod.

= Alternatives Considered

- *Keep replay-the-peak sizing, add a global CPU multiplier knob.* Operator turns a dial until P95 latency looks right. Rejected: one multiplier can't be right for both `hello` (over-provisioned) and `chromium` (still over SLA). The per-pname model is the point.

- *Reactive CPU doubling without saturation gate.* Bump cores whenever duration exceeds SLA. Rejected: serial-bound builds and parallel-burst-but-serial-dominated builds run away to unschedulable requests. `avg_cores` gating and the two-sample Amdahl fit are what make the loop terminate.

- *Independent memory control loop.* Treat memory as a second optimization variable with its own bump logic. Rejected: two coupled control loops on correlated variables oscillate. Memory is correlated with cores by construction (`-jN` × per-job working set); modeling $M(c)$ and deriving memory from the chosen $c$ keeps a single control dimension.

- *Use Kubernetes @vpa / Autopilot directly.* Rejected: @vpa#"'"s reactive loop adjusts a long-lived pod's requests over many observations; rio pods are one-shot — there is no second observation on the same pod, and @vpa cannot consume a per-@pname history. The §Coupled-model fit _is_ the analogue, keyed on @pname instead of pod identity.

- *Scalar p90 bound instead of percentile envelope.* Rejected: a single bound cannot express the spot-vs-on-demand trade-off — the envelope's `p99` constraint is what forces on-demand when the interruption tail matters, while a loose `p99` with tight `p50` is what permits cheap spot. Scalar remains the degenerate case (`{p90: 60m}`) for operators who don't need that distinction.

- *Richer speedup-model shape* (per-phase parallelism, learned regression on more features). Rejected: the staged Amdahl→Capped→@usl model is linear in its basis, so it stays one weighted-@nnls solve with no model-selection branching, and the staging covers the two structured misfits in practice. A richer model would need more samples per key than the 32-ring-buffer retains.

- *Operator declares @captype per tier* instead of deriving spot/on-demand from the envelope solve. Rejected: it forces the operator to predict interruption behavior per workload, which the model already knows from $lambda[h]$ and $T(c,h)$. The envelope shape (`p99` tightness) _is_ the operator's intent; capacity type is mechanism.

- *Scheduler picks a single $(h, "cap")$ cell* ($"argmin"_A EE["cost"]$ or — as the shipped phase-12 mechanism does — softmax-sample over a hard-coded `Band` enum, emit a single `nodeSelector` value) instead of an admissible set. Rejected: the scheduler's price snapshot is a per-$h$ @ema median, strictly coarser than `CreateFleet`'s live per-type per-@az data, so the half of the optimization the scheduler is worse at would dominate the pick. Single-pick also forfeits within-$A$ node reuse (a pod pinned to one $h$ cannot land on a warm node of an equally-admissible $h'$).

- *Reactive NodePool provisioning* (Karpenter's provisioner watches Pending pods, creates NodeClaims) instead of forecast-driven. Rejected: cannot see the DAG, so each layer pays one node-boot of latency (#qtyrange("40", "90", "s") × depth — minutes on deep graphs); cannot pre-detect capacity-dry cells, so @ice fires only after a tenant-visible Pending stall. The scheduler has the DAG, per-dep ETAs, and per-intent $c^*$ — strictly more information for the provisioning decision. Karpenter's cloud-provider layer (NodeClaim → `CreateFleet`, spot SQS, AMI/IAM, termination) is retained; only its reactive *provisioner* loop is bypassed. On fixed-node deployments (k3s) the NodeClaim layer is absent and forecast emission degrades to a no-op, so this is not a portability cost.

- *Static `lead_time` / `consolidate_after` / packing-policy knobs.* Rejected: each has a directly observable signal (`boot[h, cap]`, `eta_error`, `idle_gap[h, cap]`, intent-size distribution) that the system tracks anyway; a knob is justified only when the optimum depends on something rio cannot observe. The surviving knobs that *look* like mechanism — `sla.hwCostTolerance` $tau$, `sla.hwExploreEpsilon` $epsilon_h$, `sla.consolidateExploreEpsilon`, `sla.ladderBudget` — are intent in disguise: $tau$ is "how much modeled-cost slack the operator tolerates for placement flexibility" (a risk-appetite, not a tuning constant); $epsilon_h$ is "what fraction of dispatches the operator spends on $bold(alpha)$-identifiability" and `consolidateExploreEpsilon` is "how much idle-node spend the operator authorizes for hazard-horizon learning" (both probe budgets); `ladderBudget` is "how long a tenant may wait on capacity before the build fails" (an SLA, expressed in seconds rather than as a tier). `sla.hwBenchMemFloor` is a safety floor (STREAM working-set vs pod limit), not a tuning knob. `sla.hwClasses` and `sla.referenceHwClass` are the irreducible operator inputs (which hardware is permitted, and which one is the reference-second basis); `sla.leadTimeSeed` and `sla.maxConsolidationTime` are an initial condition and a cost cap respectively.

- *Operator-maintained per-pname size table.* Rejected as primary mechanism: shifts the fitting burden to humans, drifts on every nixpkgs bump, and the override path (§Runtime overrides) already provides this as break-glass.

- *Per-`hw_class` model key* instead of reference-second normalization. Rejected: Karpenter places by cost, not class, so the scheduler cannot direct exploration samples to fill per-class cells — most would stay under the $n_"eff" >= 3$ gate indefinitely.
- *Bayesian-optimization instance selection* (CherryPick @cherrypick2017, Selecta @selecta2018) instead of the parametric $T(c)$ fit. Rejected: BO's per-candidate sampling cost is a full build, so reaching the GP's confidence bound costs more than the §Exploration geometric ladder; and the GP surrogate gives no closed-form $c^*$ or $c_"opt"$, so the envelope solve and the @ice ladder lose their $O(1)$ per-intent solve.

- *#gls("crd")- or ConfigMap-backed overrides.* Rejected in favor of @pg + `rio-cli`: the Estimator already refreshes from @pg every tick, `rio-cli` is the established operator surface, and emergency YAML editing is worse UX than a one-line command. @crd:pl remain the mechanism for _static_ policy (#(refs.crd)("Pool")`.spec`); overrides are runtime state.

= Consequences

- *Positive:* Operators declare _intent_ (latency tiers) instead of _mechanism_ (pod sizes). No "which class is chromium" decisions; no bucket-granularity tuning. The probe-shape config itself becomes vestigial once fleet priors activate — the only durable knobs are tier boundaries, ceilings, and headroom.
- *Positive:* Cost converges downward automatically — over-provisioned builds shed cores until they sit at the tier's binding percentile bound. The steady-state core-second savings vs replay-the-peak-with-#mul(1.5)-headroom is *unmeasured* (no production `build_samples` yet); the back-of-envelope from the 6-package §Coupled-model probe puts it at $approx 55%$ (replay averages \~#mul(2.2) over converged $c^*$), against a one-time exploration overhead of $<= 3$ runs at $<= 4 dot.op "probe.cpu"$ cores.
- *Positive:* Infeasibility is explicit. #(refs.metric)("rio_scheduler_sla_infeasible_total")`{tenant,reason}` surfaces builds the SLA can't cover; `rio-cli sla explain` shows which pname and whether a `--mem` override would help.
- *Negative:* Convergence costs builds. A new pname needs \~2–3 runs at different $c$ before the model is trustworthy; the first of those may be over- or under-sized. Mitigated by the `memPerCore` prior (avoids OOM during exploration) and the `avg_cores` gate (avoids waste).
- *Negative:* Model brittleness. Amdahl is a two-parameter fit; builds whose duration is dominated by network fetches, flaky tests, or phase-dependent parallelism (wide compile, then single-threaded @lto link) won't fit cleanly. The loop still terminates (saturation gate + span-$>= 4$ freeze), and the staged model (§Model staging) handles the two structured misfits — hard parallelism caps via observed $macron(p)$, retrograde scaling via fitted $Q$. Residual flapping is damped by §Tier reassignment's Schmitt-trigger deadband.
- *Positive:* Bounded blast radius — a bad fit affects one `(pname, system, tenant)` key. `rio-cli sla reset <pname>` clears it.
- *Negative:* `build_samples` retention becomes load-bearing. An @ema of peaks could discard raw samples; curve fitting needs them. Retention policy (per-pname ring buffer of last K samples) becomes a real schema concern.
- *Negative:* Forecast provisioning trades pending-pod latency for *idle-node cost on forecast error*. An over-estimated dep ETA pre-warms capacity that sits idle until the dep actually completes; an under-estimate leaks one boot-latency. Both are bounded (`lead_time` learns from `eta_error`; idle nodes are reaped at the learned break-even), but a workload with high-variance build times pays more idle than a low-variance one. The @sita @harchol-balter1999 isolation property is structurally satisfied: the controller's per-$(h, "cap")$ packer sees each intent's $c^*$, so a burst of large forecast intents does not starve small ones — they get their own NodeClaims. The residual shared-fate is per-node (a #qty("192", "core") spot interrupt evicts every build on it), bounded by the largest type in `sla.hwClasses`.
- *Negative:* The controller now owns node lifecycle (create, @ice\-detect, consolidate-delete, *unhealthy-node reaping*) and depends on Karpenter's NodeClaim API as a stable contract. Karpenter's NodeClaim/NodeClass v1 is GA, but the cloud-provider behavior rio relies on (naked-NodeClaim launch, liveness-timeout semantics, label propagation from EC2NodeClass) is not covered by rio's CI — Karpenter version bumps and AMI changes require re-running `xtask k8s probe-boot` (which doubles as the naked-NodeClaim conformance check). The controller becomes *lease-elected* (it was previously single-replica by convention; @alg-pool's NodeClaim-create and sketch-persist are not idempotent under concurrent execution).
- *Negative:* The fleet vCPU ceiling moves from Karpenter (NodePool `limits.cpu` × pool count) to rio's `sla.maxFleetCores` check in @alg-pool — a scheduler bug that emits $10^4$ forecast intents is now bounded by rio's own gate, not Karpenter's. The shim NodePool's `limits:{cpu:0}` makes Karpenter's provisioner inert; it provides no backstop. *An AWS Service Quota on running On-Demand/Spot vCPUs, or an AWS Budgets action, is the recommended cloud-side independent guardrail.*
- *Negative:* The mental model is substantially more complex than replay-the-peak (\~18 config keys, $tilde.op 12$ metrics, 5-stage model). `rio-cli sla explain` (§Observability) is the load-bearing debuggability surface; if it cannot fully reconstruct a decision, operators have no recourse short of reading `FittedParams` rows.
- *Negative:* Phases 1–12 ship without §Phasing-13a's anchor weight-floor and sample-ordinal halflife (the *§Decision text describes the v1.2 target*, not the as-shipped phase-12 state) — so a converged key's design matrix can degenerate to rank-1 in the current deployment. This is the largest known correctness gap between this document's normative §2 and `538458a5`; §Phasing-13a is the closure.
- *Negative:* Model-stage hard-resets on every `version` bump (§Model-staging), so a weekly-patched daily-built key spends a fixed fraction of dispatches in §Exploration permanently. The soft `0.5^vdist` weight decay applies to *samples*, not to the stage latch — accepted (a stage that never resets would freeze on a major-version's stale curve), but the cost is real for fast-release-cadence packages.
- *Negative:* A scheduler restart longer than $5 dot.op "JOB_REQUEUE" approx #qty("50", "s")$ causes the controller to enter consolidate-only mode and reap idle nodes (@sec-forecast — by design, to bound idle on partition); a rolling deploy that is *not* fast loses the warm pool. `sla.referenceHwClass` change has no live-rescale: every `FittedParams` is silently mis-normalized until a refit; the only safe path is `sla reset --all` + seed-corpus re-import.
- *Negative:* Allocation is non-deterministic by design ($epsilon_h$-exploration, time-varying cost tables). Two submissions of the same drv may receive different $(c^*, h, "cap")$; the per-decision span (§Observability) captures the inputs, but tenant-facing latency variance is a UX cost.

= Implementation Phasing

== MVP (phases 1–5) — end-to-end SLA sizing on homogeneous-hw assumption

+ *Telemetry* — persist `cpu_limit_cores`, `peak_cpu_cores`, `cpu_seconds_total`, `peak_disk_bytes`, `peak_io_pressure_pct`, `version`, `tenant`, `hw_class`, `enable_parallel_building`, `prefer_local_build` in `build_samples`. cgroup already reads `usage_usec`; project-quota read from kubelet's emptyDir quota ID and `io.pressure` parsed alongside; drv attrs at #(refs.gh)("rio-gateway/src/translate.rs:452"); `hw_class` via the controller's pod annotation read through the downward-API (#(refs.gh)("docs/spec/components/controller.typ"); the builder reports it directly, no cross-process Node-label lookup). *$c^*$ plumbed to `wopSetOptions.buildCores`* so `NIX_BUILD_CORES` $= c^*$ inside the sandbox. *$D$ emitted as `resources.requests.ephemeral-storage`* (closes pre-existing wiring gap at #(refs.gh)("rio-controller/src/reconcilers/pool/pod.rs:75")). Migration and bind columns.
+ *Model* — `Estimator` caches `FittedParams` per key (incl. bootstrap CI); fit + bootstrap run on the *completion-ingest path* (the only event that moves $hat(S)$); refresh tick is *incremental* (`WHERE completed_at > $last_tick`, \~#qty("170", "ms") at 100k keys vs \~#qty("100", "s") full-scan); dispatch reads the cached fit and runs the solve only (\~#qtyrange("1", "3", "ms")). Unit newtypes (`RawCores`, `RefSeconds`, `WallSeconds`, `MemBytes`, `DiskBytes` via `rio_common::newtype`) and `enum DurationFit { Probe, Amdahl{..}, Capped{..}, Usl{..} }` make the staging type-safe. Replaces `bucketed_estimate`.
+ *Config* — `sla.{tiers, defaultTier, defaultDisk, probe, featureProbes, maxCores, maxMem, maxDisk}` in scheduler config (`headroom` is the §Coupled-model formula, not a key; the FUSE cache budget is the per-kind controller TOML `[nodeclaim_pool].{fuse_cache_bytes,fetcher_fuse_cache_bytes}` --- the #(refs.crd-field)("Pool", "fuseCacheBytes") field is CEL-rejected per `pool.rs:268-275` --- and the per-build log budget is `const LOG_BUDGET_BYTES`, no CRD/config exposure); plumbed via helm values. Tiers accept envelope form from day one; phase 2 reads only the `p90` entry until phase 12 lands.
+ *Hysteresis + retention* — Schmitt-deadband tier gating; `build_samples` per-key ring buffer (last 32, configurable); recency-weighted fit (halflife = 20 samples).
+ *Threat-model hardening* — #gls("mad")-based outlier-reject on sample ingest ($3 dot.op 1.4826 dot.op MAD$ on log-residuals, skipped until $n_"eff" >= 5$); exploration freeze gates (`tenant` column lands in phase 1).

== v1.1 (phases 6–11) — operator surface, model staging, hardware normalization

6. *Overrides + reset* — `sla_overrides` table + migration; `AdminService.{Set,List,Clear}SlaOverride` RPCs; `rio-cli sla override` subcommand.
+ *Surfacing* — #(refs.metric)("rio_scheduler_sla_infeasible_total")`{tenant,reason}` and #(refs.metric)("rio_scheduler_sla_prior_divergence")`{param}` metrics; `rio-cli sla status [pname]` (learned $S$, $P$, $macron(p)$, $Q$, $M(c)$ params, tier, sample count) and `rio-cli sla defaults` (configured vs fleet-learned vs active prior).
+ *Fleet-learned priors* — median-aggregate query over fitted pnames; activation gate at $>= 50$ keys with fitted $M(c)$; divergence check on Estimator refresh.
+ *Model staging* — observed-$macron(p)$ (recency-weighted p90) cap; $Q$ column unfreeze at $n_"eff" >= 10 and "span" >=$ #mul(8) $and Delta"AICc" < -2$ with ridge penalty.
+ *Hardware normalization* — `hw_perf_factors` table + migration; scalar `alu` self-calibration microbench in builder init (the $K = 3$ vector + $bold(alpha)$ ALS is phase 13a); reference-second scaling on sample ingest; per-`(pname, hw_class)` bias-median in Estimator refresh. Second `EC2NodeClass` (`rio-nvme`, `instanceStorePolicy: RAID0`); NixOS AMI mounts NVMe RAID0 at `/var/lib/kubelet` with `-o prjquota` (`eks-node.nix`); drop io2-quota note at #(refs.gh)("infra/helm/rio-build/values.yaml:483").
+ *Seed corpus* — `rio-cli sla export-corpus` / `import-corpus`; `sla.seedCorpus` config; corpus loader in Estimator refresh path.

== v1.2 (phases 12–13) — distribution model and cost-optimal placement

12. *Duration distribution* — fit-residual @sigma surfaced from @nnls; truncated-mixture-@cdf percentile evaluator with bisection; envelope-form tier feasibility check replacing the scalar solve from phase 2; lease-gated `ec2:DescribeSpotPriceHistory` poller via @irsa with `sla.hwCostSource` config + `_hw_cost_stale_seconds` gauge.
+ #set enum(numbering: n => [13#"abcdefgh".at(n - 1).])
  + *Hardware-class targeting (scheduler)* — \~2.0–2.4k LoC. Refactors the shipped phase-12 mechanism and ships standalone on the existing reactive reconciler; 13b replaces that reconciler.

    #memo[The shipped `sample_weight` is wall-clock; this phase corrects it to the sample-ordinal halflife specified in §Coupled-model. Wall-clock decay strands monthly-built keys.]

    *Phase-12 refactor* (\~500 LoC of the total):
    - `Band` enum → config-driven `HwClass` keyed throughout `cost.rs`/`solve.rs`/actor state.
    - Remove `softmax_pick`, `pending_intents` selector-pin, and `sla.{hwSoftmaxTemp, hwFallbackAfterSecs}` config — $A$ is deterministic given memoized inputs.
    - Rewire `cost.rs:ladder_cap` to divide by `lead_time` instead of the removed `hw_fallback_after_secs`.
    - `CostTable` exposes per-type `menu(h,cap)`, not per-band scalar.

    *Config:*
    - Net-new `sla.{hwClasses, hwCostTolerance, hwExploreEpsilon, hwBenchMemFloor, leadTimeSeed[h, cap], maxFleetCores, ladderBudget, referenceHwClass}`.
    - `sla.referenceHwClass` change rejected at config-load unless `--allow-reference-change`, forcing the operator through `sla reset --all` + corpus re-import.

    *Model + estimator:*
    - Switch `sample_weight` from wall-clock `halflife_secs` to sample-ordinal halflife=20.
    - $sigma'$ inflation + $z_q$ param in `quantile()` — small-$n_"eff"$ widening per @alg-quantile.
    - Per-pname $bold(alpha)$ simplex-LS + bounded heuristic alternation + ridge in Estimator refit.
    - Ring-buffer anchor slots with weight floor; uncensored $macron(p)$.
    - $lambda$ Gamma-Poisson partial pooling. The lease-gated spot-price poller is already shipped from phase 12.
    - FOD fleet-prior exclusion keyed on output-spec, not pname-absence.

    *Solve + dispatch:*
    - Per-$(h, "cap")$ envelope solve + admissible-set + $c^* slash k$ re-filter, memoized on `inputs_gen`.
    - $epsilon_h$ single-$h$-pin short-circuit in `sla_estimate`.
    - `compute_spawn_intents` 1-layer forecast pass.

    *Storage + migration:*
    - `hw_cost_factors{region, az, instance_type, capacity_type}` cluster-local table with #gls("pg")-persisted state.
    - Migration `hw_perf_samples.factor → jsonb`: `DROP VIEW hw_perf_factors` first; `ALTER COLUMN ... TYPE jsonb USING jsonb_build_object('alu', factor)` to preserve scalar rows; `HwTable` load switches to app-side aggregation since per-dimension MAD-reject is awkward as a view; key-addressed `{"alu":…,"membw":…,…}` so a future $K$-change pads rather than invalidates.
    - STREAM/`O_DIRECT` probes in `hw_bench.rs` (`spawn_measure → JoinHandle<[f64;K]>`).

    *Proto + RPC:*
    - `SpawnIntent` proto gains `(node_affinity: repeated NodeSelectorTerm, eta_seconds)` as new fields.
    - `AdminService.HwClassSampled` RPC — read-only, controller-allowlisted — for the bench-needed gate.
    - `SeedCorpus` v2 with corpus-level `ref_factor_vec` (one per export); `SeedEntry` gains `{α, n_eff, version}` with `#[serde(default)]` so v1 corpus files import.

    *§Threat-model gaps:*
    - Gap (a): gate read-path `AdminService` RPCs on `ensure_service_caller`.
    - Gap (b): `hw_perf_samples.submitting_tenant` column + median-of-medians aggregation.
    - Gap (c): `is_finite`/clamp on `ImportSlaCorpus`.
    - Gap (d): `sla.maxForecastCoresPerTenant` ceiling on forecast emission.

    *Metrics:*
    - Granular `infeasible_total{reason}` — 6 reasons replacing the shipped 2.
    - `hw_ladder_exhausted_total{tenant,exit}`, `hw_cost_unknown_total`, `resize_retry_total{reason}`.
    - Retire `_ice_backoff_total` (subsumed by `hw_ladder_exhausted_total`).

    *Ships standalone:* the existing `pool` reconciler stamps `node_affinity` and `rio.build/hw-bench-needed` so gate (b) data accrues, and filters `eta_seconds > 0`; NodePools are unchanged.

    *Empirical gates before enabling cost-solve:*
    + Cross-$h$ $bold(alpha)$ recovery on a $c arrow.l.r h$-correlated probe set.
    + Rank-$K$ residual `bias` spread $<= tau$ on production data, restricted to pnames with $>= 5$ samples on $>= 3$ cells so the per-cell median's SE is below $tau$.
    + Per-phase model error (chromium-shaped: link-membw vs compile-ALU) within a *fixed* relative-error threshold, e.g. $<= 0.15$ — not the CI deadband, which is inversely stringent in $n_"eff"$.
    + *Realized envelope-miss rate* $<= 1.2 dot.op (1 - q)$ per tier on a held-out window, stratified by $n_"eff"$ bucket *with $>= 100 slash (1 - q)$ dispatches per bucket*. Extend the window until met; p99 in the $[3, 5)$ bucket may need $>$ 1 month, in which case rely on $z_q$ widening as the de-facto guarantee there. This is the end-to-end SLO check that gates (a)–(c) cannot test.
  + *Forecast provisioning (controller)* — \~3.5–4.5k LoC Rust + \~400 nix all-in, including `rio-lease` extraction.

    *Prerequisite (gates the rest):* `xtask k8s probe-boot` — single-obs `leadTimeSeed` plus Karpenter conformance:
    - Naked NodeClaim launches.
    - Shim `limits:{cpu:0}` skipped, verified by `Nominated`-event-absent and zero NodeClaims labeled shim.
    - `Registered.lastTransitionTime` populated.
    - Controller-stamped `karpenter.sh/nodepool` survives to Node.
    - `budgets:nodes:"0"` blocks drift.

    *Helm + cluster resources:*
    - `kube-scheduler-packed` Deployment: *stock* `registry.k8s.io/kube-scheduler:$eksVersion` image, config-only `MostAllocated`; `system:kube-scheduler` + `system:volume-scheduler` ClusterRoleBindings; dedicated `Lease` Role for `resourceNames:[rio-packed]`; `replicas: 2` with leader-elect; `kube_scheduler_pending_pods{queue="active"}` alert if `> 0` for `> 60s`; CiliumClusterwideNetworkPolicy apiserver egress.
    - 10 `PriorityClass` resources, `value` equals the bucket index $0..9$, `preemptionPolicy: Never`. Builder pods only — control-plane pods stay on the default scheduler so cannot queue behind builders.
    - 12 *builder* NodePools → 1 shim (`limits=0`, `budgets=0`). Revert is `git revert` of the helm change + `helm upgrade`; the 12 NodePools are stateless templates and recreate cleanly.
    - RBAC `nodeclaims:{create,delete,list,watch}`.

    *Config:* `sla.{maxLeadTime, maxConsolidationTime, nodeClassRef}` (+ the DEPRECATED-IGNORED `maxNodeClaimsPerCellPerTick` row, retained parse-only — live_049 L1), with config-load assertion `maxCores < 1024`. The worst-case create burst is budget-shaped: `⌊remaining-budget/min-chunk⌋` creates per class per tick (illustration: `⌊30000/191⌋ ≈ 157` at the launchability-grounded anchor chunk, higher with smaller fitted chunks, vs ≤8/cell pre-retirement), absorbed by the controller's kube-client QPS posture and Karpenter's CreateFleet batching + cloud-provider backoff — EC2 `RequestLimitExceeded` above the rate is absorbed by that backoff (the Karpenter provisioner itself is inert here — no second backstop).

    *Controller:*
    - *Lease-elected.* Extract `rio_scheduler::lease` to a shared `rio-lease` crate and reuse; `kube-rs` has no `runtime::leader_election` module @kube-rs-485, and @alg-pool's NodeClaim-create + sketch-persist are not idempotent under concurrent execution.
    - `nodeclaim_pool` reconciler: NodeClaim CRD bindings via `kube::CustomResource`; FFD sim; anchor+bulk pack bounded by demand and the standing `sla.maxFleetCores` budget brake (the flat per-tick cap is RETIRED, live_049 L1 --- helm row parse-only); per-cell sliding HdrHistogram pair → @pg as version-tagged HdrHistogram-V2 `BYTEA` (`hdrhistogram` crate, `serialization` feature); Nelson–Aalen consolidation with $epsilon$-hold-open; $bot$-ticks-then-consolidate-only; unhealthy-node reaping.
    - Retire `nodepoolbudget` reconciler (the v1.0 reactive-NodePool budget controller).
    - `pool/jobs.rs` Job-create *moved* into the placeable-gated path — not deleted; `build_job`/`apply_intent_resources`/`reap_stale_for_intents` are reused.
    - `pool/pod.rs:{schedulerName=rio-packed, priorityClassName}`.

    *Proto + RPC:*
    - `GetSpawnIntentsResponse.dead_nodes` for the runtime-hung-node detector (as-planned wire flow, since removed — the 1d sweep reserved the field and the shipped OA2 successor clusters open-attempt ledger facts controller-side instead) — scheduler-side accumulation from executor-removal tombstones, scheduler→controller on the existing intent-poll reply. `AckSpawnedIntents` is controller→scheduler with `Empty` return.
    - `AckSpawnedIntentsRequest` gains `unfulfillable_cells` (new proto field).

    *Metrics:* #(refs.metric)("rio_controller_nodeclaim_reaped_total")`{h,cap,reason}`, #(refs.metric)("rio_controller_nodeclaim_forecast_hit_ewma")`{h,cap}`, #(refs.metric)("rio_controller_nodeclaim_lead_time_q_at_cap")`{h,cap}`.

    *VM-test coverage:* kwok-provider Karpenter in `k3s-full.nix`, \~400 LoC nix packaging — KWOK controller + kwok-provider Karpenter images airgap-loaded; second `kube-scheduler` image; controller's `nodeClassRef` config-driven for `KWOKNodeClass` vs `EC2NodeClass`; test fixture's `sla.hwClasses` uses kwok-native `karpenter.kwok.sh/instance-*` labels. Live boot-timing distribution and `probe-boot` conformance remain EKS-only.

== Testability hooks

The fit, percentile evaluator, and bisection solve are pure functions covered by table-driven and property tests (proptest invariants: $T(c)$ monotone-decreasing on $[1, c_"opt"]$; envelope-solve returns $c <= "maxCores" or "infeasible"$; $"quantile"(q)$ monotone in $q$). Convergence and exploration behavior need a VM scenario (`nix/tests/scenarios/sla-sizing.nix`) that doesn't take 3 real multi-minute builds per assertion — three hooks make that tractable:

- `RIO_BUILDER_SCRIPT=<toml>`: builder reports `(wall_secs, peak_mem, peak_cpu, cpu_seconds)` from a `(pname, cpu_limit)`-keyed table instead of executing the drv (`#[cfg(feature="test-fixtures")]`).
- `AdminService.InjectBuildSample`: seed `build_samples` so "fleet-prior activates at ≥50 fitted keys" doesn't need 50 builds (gated behind the existing test-only auth path).
- Per-VM-worker `hw_class` label override so `hw-normalize` runs without real heterogeneous hardware.

= Future Work

- *In-place pod resize for OOM* (KEP-1287, GA at k8s 1.35 @kep-1287) — replace the §Coupled-model #mul(1.5)-mem-bump-and-retry path with an in-place memory grow on the running pod, saving one restart.
- *Higher-$K$ microbench* (add `llc` cache-miss rate, `fsync` latency) — if empirical residual `bias` spread on production data exceeds $tau$, indicating the rank-3 approximation is limiting.
- *Bootstrap-replicate $c^*$ solve* — if the realized p90-miss rate at $n_"eff" in [3, 10)$ stays material after the $z_q$ Student-$t$ widening (§Coupled-model) and partial pooling, propagate $(S, P, Q)$ uncertainty into the envelope solve: precompute $T_b (c)$ for the existing 500 bootstrap replicates on a coarse $c$-grid (rayon-parallel over $b$, in the background solve cache, not the actor hot path), and require the 90th-percentile replicate to satisfy the bound. The grid-precompute is $O(500 dot.op |"grid"|)$ per fit, three orders cheaper than per-bisection-step evaluation.
- *Live cross-tenant curve sharing within a deployment* — k-anonymized ($k >= 3$) median over other tenants' fits for the same `(pname, system)`. Distinct from seed-corpus (which is operator-driven, point-in-time, between clusters); only relevant for multi-tenant SaaS deployments where tenants benefit from each other's exploration. Deferred until that deployment shape exists.
- *Input-closure-hash key dimension* — bucket `(pname, system, tenant, hash(stdenv ∪ override-args))` so configuration variants (`.override`, `pkgsCross`, `clangStdenv`) get separate curves. Triggered by sustained `_residual_multimodal_total` on packages with known variant-heavy usage.
- *Survivorship correction on $T(c)$ fit* — only completed builds write samples; on spot, $P("interrupt") prop T$, so the fit population is short-biased. Either weight samples by $e^(lambda T)$ (inverse-survival), or include interrupted attempts as right-censored observations (duration $>= tau_i$).
- *Spot-retry correlation* — the geometric retry count $G$ assumes IID Bernoulli with fixed $p$; real interrupts are temporally clustered (an AZ/type capacity event), so $P(G >= k)$ is fatter-tailed than $p^(k-1)$. A per-cell short-memory hazard (failure → next-$k$-seconds elevated $lambda$) would tighten p99 coverage on spot.
- *Heteroskedastic-aware @nnls* — multiplicative-lognormal noise gives $"Var" prop T^2$; raw-space loss over-weights low-$c$/high-$T$ rows. Weight rows by $1 slash hat(T)_i^2$ (iteratively-reweighted) or fit in log-space.
- *Community nixpkgs reference corpus* — a published seed-corpus file generated from a reference jobset. rio provides the export format; whether to publish one is a project decision, not a scheduler feature.

= Normative requirements

Requirements without a natural home in the design prose above (wire-level
and operational invariants).

#r("sched.sla.reactive-floor+4")[
  `SchedHint.resource_floor: ResourceFloor { mem_bytes, disk_bytes, deadline_secs }`
  (default zeros) is the per-dimension reactive floor for cold-start safety.
  An explicit worker-reported resource-exhaustion signal (`CgroupOom` --- the
  build cgroup hit `memory.max` while the pod survived to report it --- or
  `TimedOut`) MUST call `bump_floor_or_count`: if the relevant
  dimension is already at its ceiling (`Ceilings.max_{mem,disk}` / `86400`
  for deadline), increment `infra_count` (or `timeout_count` for deadline)
  and return `promoted=false`; otherwise set the dimension to
  `min(max(floor, last_intent) * 2, ceiling)` and return `promoted=true`.
  `last_intent` is `state.sched.last_intent.{mem,disk,deadline}_*`,
  stamped by the pull mint (the mint is the dispatch decision; the
  stream-era dispatch writer is gone --- live_040). `solve_intent_for` MUST clamp its solved
  (mem, disk) at `resource_floor` before returning, and MUST clamp
  (cores, mem, disk) at `Ceilings.max_{cores,mem,disk}`. Persisted as
  `derivations.floor_*` (`M_044`) so failover doesn't reset to zero. No
  `cores` floor: OOM/DiskPressure are mem/disk under-provision;
  DeadlineExceeded is a wall-time bound, not a parallelism bound.
]
The controller-reported arm of the previous revision (k8s
`OomKilled`/`EvictedDiskPressure`/`DeadlineExceeded` promoting the floor via
`ReportExecutorTermination`) retired with that RPC and the stream-era
disconnect correlation that gave it a first-report-wins dedup. The
pod-terminal `ReportAttemptOutcome` second installment still changes no
floor or budget (it is a classification fill, re-reported every controller
tick). A pod that dies before any worker report now reaches the
establishment sweep carrying its witnessed-terminal mark
(#rref("sched.attempt.witnessed-terminal")), and the establishment feeds
the witnessed reason through a per-reason disposition table: witnessed
`OOMKilled` --- the per-container kubelet attribution, the one structurally
unambiguous controller-witnessed reason --- promotes the MEM floor exactly
once per attempt (the establishment transaction's append+decide `won` flag
is precisely the durable dedup the previous revision's re-introduction note
demanded, keyed to the attempt's first classification); EVERY other
witnessed letter is classify-only --- it establishes on the witnessed clock
and never touches the floor. In particular `EvictedDiskPressure` carries no
promotion authority: the controller folds node-condition and pod-attributed
eviction shapes into that one letter, so promoting it would re-create the
retired ambient over-fire on the disk axis. The worker-reported arms remain
the promotion source for MEM (`CgroupOom`) and DEADLINE (`TimedOut`); DISK
still has NO promoting producer (`actor/floor.rs` annotates the parked
arm; its designed first producer is a worker-side quota-attributed signal,
not the witnessed letter), and the disk residual remains the
retry/establishment counters (retry-poison) until that lane ships.
Repeated pod-level OOM loops now self-heal in about one witnessed window
plus one doubling instead of recurring on the dispatch deadline; the
operator levers (`rio-cli sla override` / the probe floor) stay
available.

#r("sched.sla.cost-leader-edge-reload+1")[
  On a false→true leader edge, the cost-table poller MUST reload
  `sla_ema_state` from PG before its first `persist()`. A failed reload
  MUST NOT proceed to `persist()` (which would overwrite the previous
  leader's evolved EMA with this replica's stale startup snapshot) and
  MUST be retried within the bounded reload-retry envelope
  (`COST_RELOAD_RETRY_SECS`) — per failure, chain-total: every failed
  reload re-arms the envelope, including one initiated by the retry
  itself — never deferred to the next poll tick.
]

#r("sched.admin.spawn-intents.feature-filter")[
  When `GetSpawnIntentsRequest.filter_features` is set, the returned
  `intents` MUST only include Ready derivations whose
  `requiredSystemFeatures` is a subset of `features` --- i.e., derivations
  an executor advertising exactly `features` would pass `hard_filter`'s
  feature check for. Feature-gated pools (`features ≠ ∅`) MUST exclude
  derivations with empty `requiredSystemFeatures` --- those are owned by
  the featureless pool. The subset check alone (∅ ⊆ anything) would
  over-count (I-181). The `kind` filter MUST be applied alongside (the
  ADR-019 airgap boundary), and `systems` (when non-empty) MUST intersect
  `intent.system` so a per-arch pool sees only its own backlog
  (I-107/I-143). Unset `filter_features` (default) = unfiltered,
  preserving CLI behavior.
]

*Retired (1c' deletion commit B — the placement layer):* the intent-match
override (`intent_id == drv_hash` reserved a stream worker for the
derivation it was spawned for) was the stream dispatcher's way of keeping a
purpose-sized pod for its purpose. Under pull-mode delivery the binding is
structural: the pod's HMAC-attested intent IS the only derivation it can
pull, so no scheduler-side override exists to apply.

= Glossary

#print-glossary(
  glossary-entries,
  groups: ("Terms",),
  show-all: true,
  user-print-group-heading: (..) => [],
  user-print-back-references: muted-backrefs,
)
