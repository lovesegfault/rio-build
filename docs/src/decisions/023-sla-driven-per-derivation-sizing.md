# ADR-023: SLA-Driven Per-Derivation Sizing

## Status

Proposed

## Context

[ADR-020](020-per-derivation-capacity-manifest.md) replaced size-class bins with a per-derivation capacity manifest: pods sized to each derivation's measured peak, rounded to 4Gi/2-core buckets so successive builds of similar shape could reuse a pod. Two premises behind that bucketing have since collapsed:

- **P0537 made pods one-build-per-pod universally.** The "share a pod sequentially" rationale at [estimator.rs:67](../../rio-scheduler/src/estimator.rs) no longer applies — there is no second build to share with.
- **The composefs spike (ADR-022 reopen candidate) shows <10ms mount, zero warm upcalls.** FUSE-warmth amortization across a pod's lifetime is no longer a meaningful cost to protect.

What remains of bucketing is a quantization step between the scheduler's continuous estimate and Karpenter's continuous provisioning, costing on average half a bucket per pod for no structural benefit.

Separately, ADR-020 sizes pods to **fit** — replay last run's peak. It has no concept of a latency target. An operator who wants "builds finish within 20 minutes" has no knob; a build that took 45min on 8 cores will be given 8 cores again.

The two problems are linked. Memory is a hard floor (under-provision → OOM). CPU is fungible with wall-clock time: more cores buys a shorter build, up to the build's serial fraction. That makes CPU an optimization variable, not a measurement to replay — and the objective function it optimizes is the operator's latency SLA.

In HPC literature this is **moldable job scheduling**: the scheduler chooses processor count at submit time from a per-job speedup model under a deadline constraint ([Moldable Task Graphs, ACM TOPC](https://dl.acm.org/doi/10.1145/3630052)). Deadline-constrained moldable scheduling is NP-hard globally; per-job solve plus Karpenter bin-packing is a standard decomposition.

## Decision

r[scheduler.sla.model]

Replace fit-based sizing with SLA-driven sizing. The operator declares latency tiers; the scheduler learns per-derivation duration and memory as functions of allocated cores, and requests the cheapest `(cpu, mem)` that lands the derivation in its tightest feasible tier.

### Tiered SLA

r[scheduler.sla.tiers]

The operator configures an ordered list of latency tiers, each a **percentile envelope** rather than a single bound:

```yaml
sla:
  tiers:
    - {name: fast,   p50: 3m,  p90: 5m,  p99: 8m}
    - {name: normal, p50: 12m, p90: 20m, p99: 45m}
    - {name: slow,             p90: 60m}
    - {name: best-effort}
```

Each `(pname, system, tenant)` is assigned to the **tightest tier whose entire envelope is feasible** under the duration-distribution model (§Duration distribution). A single-bound tier (`{p90: 60m}`) is the degenerate case and behaves as the original ADR-020 design. Infeasible-at-any-tier derivations fall to `best-effort` and receive `ceil(peak_cpu × headroom)` cores — the cheapest allocation that doesn't slow them further.

The envelope shape is what drives the spot-vs-on-demand decision: a tight `p99` forces the solve toward on-demand (kills the interruption-retry tail); a loose `p99` with tight `p50` permits cheap spot at high `c*`. Operators express the distribution they will accept; the system chooses capacity type, hardware class, and core count to fit it at minimum expected cost.

### Duration distribution

r[scheduler.sla.distribution]

Percentile envelopes require a distribution, not a point estimate. The model composes three sources:

- **Base** `T(c, h)` — deterministic, from §Coupled-model in reference-seconds scaled by `factor[h]` and `bias[pname,h]`.
- **Run-to-run noise** `ε` — lognormal with σ from the weighted-NNLS fit residuals (already computed for CI-gated tier promotion).
- **Interruption tail** `G` — geometric retry count with `p = 1 − exp(−λ[h]·T(c,h))` on spot (`λ` = interruptions/sec), `p = 0` on on-demand. `(h, spot)` is treated as **infeasible when `p > 0.5`** — beyond that, expected retry count exceeds 2 and cost runs away. `λ[h]` is seeded from AWS Spot Advisor buckets and self-calibrated from observed pod terminations: the **controller** records the k8s `SpotInterruption` node condition ([controller/main.rs:465](../../rio-controller/src/main.rs)), not the builder, so a hostile build cannot fake an interruption.

`T_total = T(c,h)·(1+ε)·G`. The CDF is a geometric mixture of lognormals, `F(t) = Σₖ(1−p)pᵏ⁻¹·Φ(ln(t/(k·T))/σ)`; quantiles are computed by **truncating the sum at `K = ⌈ln(10⁻⁶)/ln p⌉` terms (≤6 for `p≤0.1`) and bisecting on `t`** — deterministic, ~1µs per candidate, and reproducible (Monte Carlo would make tier assignment non-deterministic across scheduler restarts). Because `p` itself depends on `T(c,h)`, the per-`(h,cap)` solve is a **bisection on `c`**: evaluate the envelope at a candidate `c`, tighten or loosen, converge. For each `(h, capacity_type)` in the NodePool's admitted set, find the minimum `c` satisfying *all* envelope constraints, compute `E[cost] = price[h, capacity_type] × c × E[T_total]`, and pick the minimum — `capacity_type` falls out of the solve rather than being configured.

### Coupled duration/memory model

r[scheduler.sla.coupled-model]

Per `(pname, system, tenant)`, fit two functions of allocated cores `c` from `build_samples`:

- `T(c) = S + P/min(c, p̄) + Q·c` — duration. `S` is the serial floor, `P` is parallelizable work-seconds, `p̄` is the **observed parallelism cap** (`max(peak_cpu_cores)` across samples — not fitted), and `Q` is the USL coherence term modeling retrograde scaling ([Gunther](https://www.perfdynamics.com/Manifesto/USLscalability.html)). With `p̄≥c_max ∧ Q=0` this is Amdahl; with `Q=0` alone it is the moldable-scheduling roofline form ([Benoit et al., ACM TOPC](https://icl.utk.edu/files/publications/2022/icl-utk-1568-2022.pdf)). The fit is **linear in `{1, 1/min(c,p̄), c}`** so it remains a single weighted-NNLS solve — no model-selection branching.

r[scheduler.sla.version-decay]

Sample weight is `wᵢ = 0.5^(ageᵢ/halflife) × 0.5^(vdistᵢ)` — recency decay (halflife = min(20 samples, 7d)) times **version-distance decay**, where `vdist` is the count of distinct `version` strings observed between the sample and the current build. Version is read from `drv.env["version"]` alongside `pname` ([translate.rs:499](../../rio-gateway/src/translate.rs)); no semver parsing — ordinal distance is robust to date- and git-rev-versioned packages. A major-version rewrite drops prior samples to ≤50% weight immediately and lets the new profile dominate within 2–3 runs; a patch bump barely perturbs the fit.
- `M(c)` — peak memory, fit as **p90 quantile regression** on `log M = a + b·log c`. Log-linear captures sub/super-linear power-law with two parameters; p90 (not mean) because least-squares puts ~50% of runs above the line and into OOM territory. This matches Autopilot/VPA practice ([EuroSys '20](https://research.google/pubs/autopilot-workload-autoscaling-at-google-scale/)) of sizing from a decaying-histogram percentile rather than a point estimate.

CPU is the single control variable; memory is derived. Allocation: solve for the smallest `c*` satisfying the tier's envelope (§Duration distribution) — under the scalar-tier MVP this reduces to `T(c) = 0.9·p90_bound`, linear in `c` at `Q=0` and quadratic otherwise. Clamp to `[1, min(p̄, c_opt, maxCores, max c s.t. M(c)·headroom(n) ≤ maxMem)]` where `c_opt = √(P/Q)` is the USL throughput peak (∞ at `Q=0`), and request `(c*, M(c*)·headroom(n))`.

Headroom is **confidence-scaled**, not flat: `headroom(n) = 1.25 + 0.5·exp(−n/8)` — ~1.75× at n=2 converging to 1.25× by n≈20. Autopilot's lesson is that aggressive learned limits cut slack but raise OOM rate; widening the margin while the fit is young pays a small cost premium for stability.

Coupling `M` to `c` is a deliberate departure from Autopilot's three independent loops. It is justified for build workloads (`-jN × per-compiler working set`) but is gated: when the `M(c)` fit has `R² < 0.7`, fall back to the independent p90 of observed `peak_mem` (ADR-020's behavior) and emit `rio_scheduler_sla_mem_fit_weak{pname}`.

A tier is **infeasible** when the model's minimum achievable duration `T_min = T(min(p̄, c_opt))` exceeds the tier, or when the `c*` needed pushes `M(c*)` past `maxMem`. Either condition demotes to the next tier.

### Hardware heterogeneity

r[scheduler.sla.hw-normalize]

Karpenter provisions across instance generations by spot cost — `values.yaml:498` admits gen 6/7/8, which spans Graviton2→4 within `aarch64-linux` and Milan/Ice Lake/Genoa/Sapphire Rapids within `x86_64-linux`. Phoronix LLVM-compile shows a **1.89× wall-clock spread** across that Graviton range and 1.31× across same-gen x86 vendors — 3–8× the model's ~11% duration margin. Pooling samples without correction makes `T(c)` systematically wrong on the slowest hardware Karpenter picks. Under a global-DB deployment the spread only widens.

Mitigation is **reference-second normalization** with a per-pname residual correction:

- **`hw_class`** is `{instance-cpu-manufacturer, instance-generation}` from Karpenter node labels, surfaced to the builder pod via downward API and recorded per `build_samples` row.
- **`hw_perf_factors{hw_class → factor}`** is a PG table seeded from OpenBenchmarking `build-linux-kernel` results and **self-calibrating**: the builder *supervisor* (not the untrusted build payload — the bench runs at init before any assignment is accepted, [main.rs:105](../../rio-builder/src/main.rs)) runs a ~5s single-threaded fixed-source compile loop on first start on an unknown `hw_class`, computes `factor = ref_time / observed_time`, and upserts. To resist noisy-neighbor interference (a hostile drv on another pod, same node, spinning during the bench), the upsert is the **median over ≥3 distinct-pod runs** with 3σ outlier rejection. Unbench'd classes default to `factor = 1.0`; reference is `sla.referenceHwClass` (or `min(factor)` once ≥1 row exists) so all factors are ≥1.
- **Fit in reference-seconds.** `T(c)` is fitted on `wall_secs × factor` and `cpu_seconds × factor`; `M(c)`, `p̄`, and raw `c` are core-*count* quantities and are not scaled (peak memory is workload-determined, not CPU-generation-determined).
- **Per-pname bias correction.** `bias[pname, hw_class] = median(wall_secs_ref_observed / T_ref(c))` over that class's samples captures the residual where a specific build scales non-uniformly across microarchitectures (e.g. memory-bandwidth-bound builds gain less from a faster ALU). It defaults to `1.0` for classes with `<3` samples. §Hardware-class targeting consumes it at solve time per candidate `h`; until that phase lands, the solve in reference-seconds sizes `c*` for the reference (slowest) class and faster placements simply finish early.

This is the [ACCESS](https://allocations.access-ci.org/) SU-normalization approach with a compile-specific benchmark ([build-linux-kernel](https://openbenchmarking.org/test/pts/build-linux-kernel-1.17.1)) in place of HPL, plus a fixed-effects residual term. A keyed approach (`hw_class` in the model key) was rejected: Karpenter places by cost, not class, so the scheduler cannot direct exploration samples to fill per-class cells — most would stay under the n≥3 gate indefinitely.

### Hardware-class targeting

r[scheduler.sla.hw-target]

Sizing `c*` for the slowest admitted class is SLA-correct but wastes ~`(1 − 1/f)` of core-seconds when Karpenter places on faster hardware — at `f=1.89` that is ~47%. Since the model can predict `T(c, h)` per class, the scheduler chooses the class rather than leaving it to Karpenter:

- `hw_cost_factors{hw_class, capacity_type → price_ratio, interrupt_rate}` is a PG table seeded from static helm config (`sla.hwCostRatio`, defaults from AWS on-demand list prices and Spot Advisor interruption buckets). An `hw_class` with no row defaults to `price_ratio = 1.0` and emits `rio_scheduler_hw_cost_unknown{hw_class}` so a new instance generation is included (conservatively priced) rather than silently excluded from the feasible set. With `sla.hwCostSource: spot`, a background poller refreshes `price_ratio` from `ec2:DescribeSpotPriceHistory` every ~10min (TLS+SigV4 via the scheduler's IRSA credentials; `aws-sdk-ec2` is already a dependency). Static config remains the fallback when the poller is disabled or the API fails; `rio_scheduler_hw_cost_stale_seconds` surfaces a stalled poller.
- For each `(h, capacity_type)` admitted by the NodePool, solve §Duration-distribution for the minimum `c*` satisfying the tier's full envelope; the feasible set is those where `c*` exists within `min(p̄, c_opt, maxCores)` and `M(c*)·headroom ≤ maxMem`.
- Choose `(h*, cap*) = argmin E[cost]` over the feasible set and emit the pod with `nodeSelector` on `{instance-cpu-manufacturer, instance-generation, karpenter.sh/capacity-type}` and `resources.requests.cpu = c*[h*, cap*]`.
- Builds infeasible on slow or high-interruption classes are excluded by construction; builds with loose envelopes where older spot is cheapest-per-result land there.

If the pinned pod is `Pending` beyond `sla.hwFallbackAfter` (default 30s), or a spot interruption occurs, the scheduler re-emits on the next-best feasible `(c*, h, cap)` — bounded by `|feasible set|` (~8-12 candidates), with `rio_scheduler_hw_ladder_exhausted_total` on exhaustion. This is part of phase 13; until it lands, capacity exhaustion surfaces as pending-pod backpressure and interruptions retry on the same allocation.

### Model staging

r[scheduler.sla.model-staging]

`S` and `P` are fitted always; `Q` and `p̄` enter as data permits:

| Stage | Gate | Active terms | Effect |
|---|---|---|---|
| Amdahl | `n ≥ 3 ∧ span ≥ 4×` | `S, P` (`p̄=∞, Q=0`) | baseline; what §Exploration converges to |
| Capped | first sample where `peak_cpu < cpu_limit × 0.85` | `S, P, p̄` | hard parallelism ceiling observed — never request `c > p̄` |
| USL | `n ≥ 6 ∧ span ≥ 8× ∧ (ΔBIC < −2 ∨ top-c residual > +20%)` | `S, P, p̄, Q` | retrograde scaling fitted — `c*` lands at `c_opt` instead of overshooting once |

All three are the *same* NNLS solve with progressively unfrozen columns; there is one code path, not three. The `avg_cores` saturation gate in §Exploration is what halts bumping *at* the cap/retrograde boundary empirically; the model is what lets the solver land there *predictively* on subsequent runs.

### Saturation-gated exploration

r[scheduler.sla.exploration]

The model needs samples at distinct `c` to fit. The scheduler obtains them via a control loop, gated so it never wastes cores a build demonstrably can't use:

- record `cpu_limit`, `peak_cpu`, `cpu_seconds`, `peak_mem`, `wall_secs` per completion;
- `avg_cores = cpu_seconds / wall_secs`; bump `c` only when `avg_cores / cpu_limit > 0.4` AND duration exceeds the target tier — peak alone is insufficient (a brief parallel burst saturates `peak_cpu` without indicating the build would benefit from more cores);
- the **first bump is ×4**, not ×2. Noise sensitivity in the Amdahl solve is dominated by sample *spacing*, not count: at ±15% run-to-run variance, `c=(8,16)` yields σ(Ŝ)≈43% of true S, while `c=(4,32)` yields σ≈16%. A wide first step buys a usable fit one run sooner;
- exploration is **capped at 3 bumps**. A build that still saturates after `probe → 4× → 16× → 64×` either exceeds `maxCores` or is gaming the gate (a spin-loop reads as `avg_cores≈limit`). Freeze at the cap and emit `rio_scheduler_sla_suspicious_scaling{pname}`;
- switch from heuristic bumping to solving `T(c)` directly only when **`span(c_max/c_min) ≥ 4` AND `n ≥ 3`**. Until then the fit is provisional and not used for tier assignment.

An OOM-kill at `(c, mem)` is a censored sample — `M(c) ≥ mem`, not `= mem`. It penalty-bumps the memory fit's lower bound at that `c` and triggers an automatic retry at `1.5× mem`, mirroring the existing duration-misclassify penalty path.

### Cold start and priors

r[scheduler.sla.cold-start]

With zero samples, the first run uses an operator-configured probe. Derivations without a `pname` (FODs, raw `builtins.derivation`) have no key to accumulate samples under and therefore use the active prior on **every** run, never learning — the existing closure-size duration proxy ([estimator.rs:134](../../rio-scheduler/src/estimator.rs)) places them in a tier, and the probe shape sizes them. FODs are network-bound fetches in practice, so the default-tier probe is the right answer regardless.

r[scheduler.sla.drv-signals]

Nix derivations carry structure the model can use to skip exploration:

- **`enableParallelBuilding`** ([translate.rs:499](../../rio-gateway/src/translate.rs)): when explicitly false, fix `p̄ = 1` immediately — the build is serial by declaration, so the ×4-bump loop is wasted. Absent (the historical stdenv default) is treated as unknown, since nixpkgs is migrating to `enableParallelBuildingByDefault=true`.
- **`preferLocalBuild`**: trivially-short drvs (`writeText`, `symlinkJoin`). Short-circuit to a fixed minimal probe; no `build_samples` row written.
- **`requiredSystemFeatures`**: `sla.featureProbes: {feature → {cpu, memPerCore, memBase}}` generalizes the `big-parallel` special case — `kvm`/`nixos-test` map to high-`memBase`/low-`cpu` (qemu guest RAM dominates), `benchmark` maps to on-demand-only.

The chosen `c*` is plumbed to the build via **`wopSetOptions.buildCores`** ([client.rs:273](../../rio-nix/src/protocol/client.rs)) so `NIX_BUILD_CORES = c*` inside the sandbox — the pod's `resources.requests.cpu` alone does not reach the build's `-j`.

The probe is expressed as `{cpu, memPerCore, memBase}` — not absolute memory — so that when the loop bumps cores after a single sample (no `M(c)` slope fittable yet), it requests `bumped_c × memPerCore + memBase` rather than replaying run-1's peak and OOMing. The probe's linear form is for operator intuition; once `n≥3`, the log-linear `M(c)` fit (parameters `a, b`) supersedes it. A second probe shape keyed on `requiredSystemFeatures ∋ big-parallel` is optional.

### Fleet-learned priors

r[scheduler.sla.fleet-prior]

The operator-configured `memPerCore`/`memBase`/`cpu` probe values are a **bootstrap**, not durable truth. Once enough pnames have a fitted `M(c)` (threshold: 50, configurable), the scheduler computes a fleet-aggregate prior — median `M(c)` fit parameters and median converged-`c*` across all fitted rows (cross-tenant; see §Threat model for why median+clamp makes this safe) — and uses that as the cold-start probe instead. Median, not mean, so a handful of outliers (LLVM, chromium) don't drag the prior.

The fleet prior is **clamped to `[0.5×, 2×]` of the operator-configured value**. Median is robust to a few outliers but not to systematic skew — if the first 50 fitted pnames happen to be LLVM-tier, an unclamped median would triple-size every cold start. The clamp makes operator config a guardrail rather than dead config: the system self-tunes within the band, and `rio_scheduler_sla_prior_divergence{param}` fires when the fleet median hits the clamp so the operator knows to widen it.

### Seed corpus

r[scheduler.sla.seed-corpus]

A new cluster has no `build_samples`, so every package pays the 2–3-run exploration cost. An operator standing up a second region, a staging mirror, or a fresh cluster after a migration already *has* fitted curves in the old deployment — they should carry over.

`rio-cli sla export-corpus [--tenant=<t>] [--min-n=3]` dumps fitted `{pname, system, S, P, Q, p̄, a, b, n, ref_hw_class}` rows to a JSON file. Curves are in reference-seconds (§Hardware heterogeneity), so a corpus exported on Graviton4 imports correctly on Sapphire Rapids — the importing cluster rescales by the ratio of reference factors. `sla.seedCorpus: <path>` (or `rio-cli sla import-corpus`) loads it on the new cluster. A seed entry slots into the precedence chain below the local fit and above the fleet-aggregate: the new cluster starts from the old cluster's curves and refines from its own samples, instead of cold-probing every package.

Privacy is a non-issue for the primary use case — the operator is moving their own data between their own clusters. The export omits raw samples (timing/frequency would leak tenant activity if the file *were* later shared) and includes only fitted parameters. An operator who wants to publish their corpus (e.g., a community nixpkgs reference set) can scrub pnames before doing so; rio does not ship one.

Prior precedence becomes: **per-tenant fit > seed corpus > fleet-aggregate (clamped) > operator config.**

The line this draws: operators own **intent** (tier boundaries, `maxCores`/`maxMem`, `headroom`) and the system owns **mechanism** (probe shape, slope priors). Operator config for mechanism is scaffolding the system climbs off of.

### Threat model

r[scheduler.sla.threat-model]

`pname` is read from `drv.env["pname"]` ([derivation.rs:530](../../rio-scheduler/src/state/derivation.rs)) — submitter-controlled. A hostile derivation can set `pname="hello"`, spin-loop 96 cores for an hour, and poison the model that every other tenant's `hello` build reads from. Builders are untrusted by design (ADR builder-airgap); the cgroup readings themselves are trustworthy (the supervisor reads them, not the build), but they measure *consumption*, not *useful work* — a spin-loop is indistinguishable from a real compile at the cgroup layer.

Mitigations, both required:

- **Tenant-scoped model.** The fit key is `(pname, system, tenant)`, not `(pname, system)`. A tenant can poison only its own curves. Cross-tenant sharing of the fleet-aggregate prior is median-based and clamped (below), so a single tenant's outliers don't propagate.
- **Outlier rejection on ingest.** Once a key has `n≥3` samples (so σ is defined), a completion sample whose `(wall_secs, peak_mem)` lies outside 3σ of the existing fit is recorded but **excluded from the fit** and increments `rio_scheduler_sla_outlier_rejected_total{pname}`. Combined with the 3-bump exploration cap, this bounds the resource a hostile drv can extract to `probe + 4×probe + 16×probe + 64×probe` cores before the loop freezes — a finite cost the tenant pays themselves.

A content-addressed key (drv hash) was rejected: every drv hash is unique, so the model would never accumulate samples. `(pname, tenant)` plus outlier rejection is the accepted compromise: a tenant can mis-size its own builds, but not anyone else's.

### Runtime overrides

r[scheduler.sla.override]

Per-pname overrides are stored in a PG `sla_overrides` table and managed via `rio-cli sla override <pname> [--tier=T] [--p50|--p90|--p99=D] [--cores=N] [--mem=B] [--capacity=spot|on-demand] [--ttl=D]`. The Estimator reads overrides on its existing ~60s refresh tick alongside `build_history`. `--tier` pins the target (system still solves for `c`); `--cores`/`--mem` pin the allocation directly (break-glass, bypasses the model). `--ttl` self-expires emergency overrides.

Resolution precedence for the **target tier**: override > learned (tightest feasible) > feature-probe match > global default.

### Observability

r[scheduler.sla.observability]

Per-decision: `sla_estimate` is `#[instrument]`-ed and emits a DEBUG span with `{pname, tier, c_star, hw_class, capacity_type, binding_constraint, prior_source, n_candidates_feasible}`. `rio-cli sla explain <pname>` dumps the full candidate table (`[(h, cap, c*, E[cost], binding_constraint, rejected_reason)]`) and the prior-precedence trace, so an operator can reconstruct *why* a build got the allocation it got.

Metrics (per `observability.md` `rio_scheduler_` convention):
- `rio_scheduler_sla_prediction_ratio{dimension=duration|memory}` — histogram of `actual / predicted`. Sustained skew off 1.0 is the **model-drift** alert; this is the "silently wrong" signal.
- `rio_scheduler_sla_envelope_result_total{tier, result=hit|miss, constraint}` — the SLO outcome the operator's dashboard is built on.
- `rio_scheduler_sla_infeasible_total{pname, reason=serial_floor|mem_ceiling|core_ceiling|interrupt_runaway}`
- `rio_scheduler_sla_{suspicious_scaling,outlier_rejected,hw_cost_unknown,hw_ladder_exhausted}_total`
- `rio_scheduler_sla_prior_divergence{param}` (gauge), `rio_scheduler_hw_cost_stale_seconds` (gauge)

### Global-deployment compatibility

With `build_samples`/`build_history` in a global database (Aurora DSQL / Spanner per the multi-region plan), the model is shared by construction: a new region reads every existing curve on join, the fleet-aggregate prior spans all regions, and the seed-corpus mechanism reduces to a first-deployment / disconnected-install convenience. Reference-second normalization (above) is what makes pooled samples coherent across regions' hardware. Per-cluster differences are confined to **mechanism ceilings** — `sla.maxCores`/`sla.maxMem` are read from per-cluster config (instance availability varies by region) while tiers, headroom, and overrides are global. `sla_overrides` gains an optional `cluster` column for `rio-cli sla override --cluster=<name>` when a region-specific pin is needed.

### Bucketing

r[scheduler.sla.no-bucketing]

The 4Gi/2-core rounding in `BucketedEstimate` is removed. Requests are `(c*, M(c*)·headroom)` rounded only to k8s-native granularity (millicores, bytes). Karpenter bin-packs heterogeneous requests at the node layer; pod-level quantization adds slack with no offsetting reuse under one-build-per-pod.

## Alternatives Considered

- **Keep ADR-020 fit-based sizing, add a global CPU multiplier knob.** Operator turns a dial until P95 latency looks right. Rejected: one multiplier can't be right for both `hello` (over-provisioned) and `chromium` (still over SLA). The per-pname model is the point.

- **Reactive CPU doubling without saturation gate.** Bump cores whenever duration exceeds SLA. Rejected: serial-bound builds and parallel-burst-but-serial-dominated builds run away to unschedulable requests. `avg_cores` gating and the two-sample Amdahl fit are what make the loop terminate.

- **Independent memory control loop.** Treat memory as a second optimization variable with its own bump logic. Rejected: two coupled control loops on correlated variables oscillate. Memory is correlated with cores by construction (`-jN` × per-job working set); modeling `M(c)` and deriving memory from the chosen `c` keeps a single control dimension.

- **CRD- or ConfigMap-backed overrides.** Rejected in favor of PG + `rio-cli`: the Estimator already refreshes from PG every tick, `rio-cli` is the established operator surface, and emergency YAML editing is worse UX than a one-line command. CRDs remain the mechanism for *static* policy (`BuilderPool.spec`); overrides are runtime state.

## Consequences

- **Positive:** Operators declare *intent* (latency tiers) instead of *mechanism* (pod sizes). No "which class is chromium" decisions; no bucket-granularity tuning. The probe-shape config itself becomes vestigial once fleet priors activate — the only durable knobs are tier boundaries, ceilings, and headroom.
- **Positive:** Cost converges downward automatically — over-provisioned builds shed cores until they sit at the tier's binding percentile bound.
- **Positive:** Infeasibility is explicit. `rio_scheduler_sla_infeasible_total{pname,reason=serial_floor|mem_ceiling}` surfaces builds the SLA can't cover, with the saturation hint pointing at whether a `--mem` override would help.
- **Negative:** Convergence costs builds. A new pname needs ~2–3 runs at different `c` before the model is trustworthy; the first of those may be over- or under-sized. Mitigated by the `memPerCore` prior (avoids OOM during exploration) and the `avg_cores` gate (avoids waste).
- **Negative:** Model brittleness. Amdahl is a two-parameter fit; builds whose duration is dominated by network fetches, flaky tests, or phase-dependent parallelism (wide compile, then single-threaded LTO link) won't fit cleanly. The loop still terminates (saturation gate + 3-bump cap), and the staged model (§Model staging) handles the two structured misfits — hard parallelism caps via observed `p̄`, retrograde scaling via fitted `Q`. Residual flapping is damped by a **Schmitt-trigger deadband** on tier reassignment: promote only when `T_min`'s 80% CI clears `0.85×` the tighter tier's binding bound, demote only when it exceeds `1.05×` the current tier's.
- **Negative:** `build_samples` retention becomes load-bearing. ADR-020's EMA could discard raw samples; curve fitting needs them. Retention policy (per-pname ring buffer of last K samples) becomes a real schema concern.

## Relationship to ADR-015 and ADR-020

Supersedes ADR-020's bucketing and fit-based sizing under `BuilderPool.spec.sizing: Manifest`. ADR-020's manifest transport (`GetCapacityManifest` RPC, manifest reconciler, resource-fit placement filter) is reused unchanged — this ADR changes what the manifest *contains*, not how it flows. ADR-015's `sizing: Static` path remains for operators who want explicit bins.

ADR-015's SITA-E isolation property (shorts don't queue behind longs) is *strengthened* at the pod layer — one build per pod is perfect isolation — but the shared queue did not vanish; it moved to **Karpenter's pending-pod queue**. Karpenter's bin-packing is cost-driven, not size-aware, so a burst of large pending pods can delay node provisioning for small ones. This is outside rio's control surface; if it proves material, the mitigation is per-tier Karpenter NodePools with provisioning priority.

## Implementation Phasing

### MVP — end-to-end SLA sizing on homogeneous-hw assumption

1. **Telemetry** — persist `cpu_limit_cores`, `cpu_seconds_total`, `version`, `tenant`, `hw_class`, `enable_parallel_building`, `prefer_local_build` in `build_samples`. cgroup already reads `usage_usec`; drv attrs extracted at `translate.rs:499`; `hw_class` via downward-API env on builder pod. **`c*` plumbed to `wopSetOptions.buildCores`** so `NIX_BUILD_CORES = c*` inside the sandbox. Migration + binds.
2. **Model** — `Estimator::sla_estimate(pname, system, tenant) -> (cpu_millicores, mem_bytes, tier)`; NNLS Amdahl + log-linear p90 `M(c)` fit; saturation-gated ×4-bump path until span≥4 ∧ n≥3. Replaces `bucketed_estimate`.
3. **Config** — `sla.{tiers, default, bigParallel, maxCores, maxMem, headroom}` in scheduler config; plumbed via helm values. Tiers accept envelope form from day one; phase 2 reads only the `p90` entry until phase 12 lands.
4. **Hysteresis + retention** — Schmitt-deadband tier gating; `build_samples` per-key ring buffer (last 32, configurable); recency-weighted fit (halflife = min(20 samples, 7d)).
5. **Threat-model hardening** — 3σ outlier-reject on sample ingest (skipped until n≥3); 3-bump exploration cap (`tenant` column lands in phase 1).

### v1.1 — operator surface, model staging, hardware normalization

6. **Overrides** — `sla_overrides` table + migration; `AdminService.{Set,List,Clear}SlaOverride` RPCs; `rio-cli sla override` subcommand.
7. **Surfacing** — `rio_scheduler_sla_infeasible_total{pname,reason}` and `rio_scheduler_sla_prior_divergence{param}` metrics; `rio-cli sla status [pname]` (learned `S`, `P`, `p̄`, `Q`, `M(c)` params, tier, sample count) and `rio-cli sla defaults` (configured vs fleet-learned vs active prior).
8. **Fleet-learned priors** — median-aggregate query over fitted pnames; activation gate at `n ≥ 50`; divergence check on Estimator refresh.
9. **Model staging** — observed-`p̄` cap; `Q` column unfreeze at `n≥6 ∧ span≥8×` with ΔBIC gate.
10. **Hardware normalization** — `hw_perf_factors` table + migration; self-calibration microbench in builder init; reference-second scaling on sample ingest; per-`(pname, hw_class)` bias-median in Estimator refresh.
11. **Seed corpus** — `rio-cli sla export-corpus` / `import-corpus`; `sla.seedCorpus` config; corpus loader in Estimator refresh path.

### v1.2 — distribution model and cost-optimal placement

12. **Duration distribution** — fit-residual σ surfaced from NNLS; truncated-mixture-CDF percentile evaluator with bisection; envelope-form tier feasibility check replacing the scalar solve from phase 2.
13. **Hardware-class + capacity-type targeting** — `hw_cost_factors{hw_class, capacity_type → price, interrupt_rate}` table seeded from helm; optional `ec2:DescribeSpotPriceHistory` poller; spot-interruption self-calibration from controller-observed node conditions; per-`(h, cap)` envelope solve in `sla_estimate`; `nodeSelector` + `karpenter.sh/capacity-type` emission in `build_executor_pod_spec`; fallback ladder + interruption-retry escalation.

### Testability hooks

The fit, percentile evaluator, and bisection solve are pure functions covered by table-driven and property tests (proptest invariants: `T(c)` monotone-decreasing on `[1, c_opt]`; envelope-solve returns `c ≤ maxCores ∨ infeasible`; `quantile(q)` monotone in `q`). Convergence and exploration behavior need a VM scenario (`nix/tests/scenarios/sla-sizing.nix`) that doesn't take 3 real multi-minute builds per assertion — three hooks make that tractable:

- `RIO_BUILDER_SCRIPT=<toml>`: builder reports `(wall_secs, peak_mem, peak_cpu, cpu_seconds)` from a `(pname, cpu_limit)`-keyed table instead of executing the drv (`#[cfg(feature="test-fixtures")]`).
- `AdminService.InjectBuildSample`: seed `build_samples` so "fleet-prior activates at n≥50" doesn't need 50 builds (gated behind the existing test-only auth path).
- Per-VM-worker `hw_class` label override so `hw-normalize` runs without real heterogeneous hardware.

## Future Work

- **Per-tier Karpenter NodePools with provisioning priority** — if the residual queue at the node-provision layer (§Relationship to ADR-015) proves to violate SITA in practice.
- **Live cross-tenant curve sharing within a deployment** — k-anonymized (k≥3) median over other tenants' fits for the same `(pname, system)`. Distinct from seed-corpus (which is operator-driven, point-in-time, between clusters); only relevant for multi-tenant SaaS deployments where tenants benefit from each other's exploration. Deferred until that deployment shape exists.
- **Community nixpkgs reference corpus** — a published seed-corpus file generated from a reference jobset. rio provides the export format; whether to publish one is a project decision, not a scheduler feature.
