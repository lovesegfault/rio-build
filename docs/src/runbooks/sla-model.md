# SLA Model Drift

The SLA sizer (ADR-023) fits a per-`(pname, system, tenant)` duration/memory
curve from observed builds and solves for the cheapest core count that hits
the configured tier targets. When the fit is wrong — stale samples, hw drift,
a pname whose behaviour changed upstream — builds get under- or
over-provisioned and `RioSlaPredictionDrift` fires.

This runbook walks: alert → identify pname → inspect solve → diagnose →
override or reset.

## Alert entry points

| Alert | Fires when | Section |
|---|---|---|
| `RioSlaPredictionDrift` | p50 of actual/predicted outside `[0.5, 2.0]` for 15m | [Diagnose a pname](#step-2-inspect-the-solve) |
| `RioSlaPriorDivergenceClamped` | fleet-prior parameter clamped at band edge for 10m | [Prior divergence](#riosla-priordivergenceclamped) |
| `RioSlaHwCostStale` | hw-band $/vCPU·hr snapshot >30m old | [Hw-cost stale](#riosla-hwcoststale) |
| `RioNodeclaimPoolIceMaskedHigh` | ≥3 cells reaping NodeClaims for `reason=ice` | [Admissible set shrinking](#rionodeclaimpool-icemaskedhigh) |
| `RioNodeclaimPoolStuckPending` | NodeClaim in-flight >90s | [Provisioning stuck](#rionodeclaimpool-stuckpending) |

The first three are model-accuracy alerts; the last two are provisioning
alerts that share the same `rio-cli sla` diagnostic surface.

## Step 1: Identify the offending pname

`RioSlaPredictionDrift` is fleet-wide (labelled by `dim=wall|mem`, not by
pname — `rio_scheduler_sla_prediction_ratio` is a histogram, so there is
no per-pname series to `topk` over). Find candidate keys via the CLI:

```bash
rio-cli sla mispredictors --top 10
```

```text
PNAME                    SYSTEM         TENANT       DIM    RATIO
chromium                 x86_64-linux   acme        wall    2.841
llvm                     aarch64-linux  acme         mem    0.312
…
```

The ring is in-memory (this leader's tenure only) and fills as builds
with a fitted curve complete. If empty, fall back to `rio-cli sla list`
and inspect high-traffic keys manually.

For each suspect pname, dump the cached fit:

```bash
rio-cli sla status <pname> --system x86_64-linux --tenant <tenant>
```

```text
Key:       chromium (x86_64-linux)
Fit:       Usl S=412.0s P=18200.0s Q=0.0031 p̄=28.0
Mem:       Coupled p90=42.1Gi
Stats:     n_eff=6.2 span=3.5 σ=0.412 tier=normal prior=per-key
Override:  (none)
```

Read `Stats:` first — `n_eff` and `σ` tell you whether the fit is trustworthy
before you look at what it predicts.

## Step 2: Inspect the solve

`sla explain` re-runs the tier walk in dry-run and prints why each tier was
accepted or rejected — the same gates dispatch evaluated:

```bash
rio-cli sla explain <pname> --system x86_64-linux --tenant <tenant>
```

```text
Key:       chromium (x86_64-linux)
Fit:       Usl S=412.0 P=18200.0 Q=0.0031 p̄=28.0 σ=0.412 n_eff_ring=6.2 fit_df=4.2 | M(c)=exp(19.80+0.42·ln c)
Prior:     per-key
Override:  (none)

TIER             C*        MEM CONSTRAINT       FEASIBLE
fast              -          - serial-floor     no
normal        18.40     38.2Gi -                yes
best-effort   28.00     46.9Gi no-bounds        yes
```

| Field | Meaning |
|---|---|
| `Fit:` | Duration model (`Amdahl`/`Capped`/`Usl`/`Probe`), `σ` residual, `n_eff_ring` effective sample count, mem model |
| `Prior:` | `per-key` (own samples), `fleet-prior` (partial-pooled), `none` (cold start) |
| `C*` | Solved core count for that tier; `-` if the tier was rejected before forming the quadratic |
| `CONSTRAINT` | Binding reject reason: `serial-floor` (S alone breaches target), `envelope` (infeasible at cap_c against p50∧p90∧p99), `mem-ceiling`, `disk-ceiling`, `no-bounds` (tier has no targets → feasible at cap_c), `-` (feasible) |

Dispatch picked the **first** `FEASIBLE yes` row.

## Step 3: Diagnose

### `n_eff` low (<8)

Few effective samples → wide confidence → headroom multiplier is high
(`headroom(n_eff)` ≈ 1.95 at n_eff=1, ≈ 1.32 at n_eff=100). Over-provisioning
is expected here; the model self-corrects as samples accrue. Only act if the
key is hot enough that waiting is unacceptable — see
[Override](#step-4-override-or-reset).

`Prior: none` with an empty candidate table = cold start. Dispatch is using
the probe shape from `[sla].probe`; nothing to fix unless the probe shape
itself is wrong (see `rio-cli sla defaults`).

### `σ` high (>0.3) or `span` low (<2)

High residual variance or all samples clustered at one core count → the curve
is under-constrained. `sla reset` forces a fresh probe ladder that will spread
samples across `c`. If the pname is genuinely noisy (e.g. test suites with
random seeds), pin it with `--cores` instead.

### hw bias / `hw_perf_factors` drift

`prediction_ratio` skewed on builds dispatched to one `hw_class` only (check
`rio-cli sla status <pname>` for the dispatch-time hw assignment, or the
build's structured log `hw_class` field) → the per-hw normalization factor
is stale. The
`hw_perf_samples` table is 7-day windowed; a hardware change takes up to a
week to wash through. No per-pname fix — the fleet-median recomputes every
estimator refresh tick (~60s). Cross-check
[`RioSlaPriorDivergenceClamped`](#riosla-priordivergenceclamped).

### Wrong tier

`explain` shows the key feasible at a tighter tier than you want (over-spend)
or rejected from the tier you expect (`serial-floor` / `envelope`):

- `serial-floor` on a tier you believe should fit → the fitted `S` is too
  high. Check upstream: did the pname grow a long single-threaded phase?
- `envelope` at low `cap_c` → `p̄` (parallelism cap) is too low. Often a
  `Capped` fit from a narrow sample span — `sla reset` to re-explore.
- Feasible at `fast` but you want `normal` cost → pin with `--tier=normal`.

## Step 4: Override or reset

All overrides are `(pname, system?, tenant?)`-scoped (NULL = wildcard,
most-specific wins) and take effect on the next estimator refresh (~60s).

### Pin to a named tier

```bash
rio-cli sla override <pname> --tier=normal --ttl 7d
```

Solve still runs; the candidate table is filtered to that tier only. Use when
the model is *right* but you want a different cost/latency trade-off.

### Pin ad-hoc p50/p90/p99 targets

```bash
rio-cli sla override <pname> --p90=20m --p99=1h --ttl 7d
```

Solve runs against a one-off tier built from these targets instead of the
config ladder. Any subset of `--p50`/`--p90`/`--p99` is accepted. Use when no
named tier fits (see `rio-cli sla defaults` for the ladder).

### Force cores/mem (bypass model)

```bash
rio-cli sla override <pname> --cores=16 --mem=32Gi --ttl 7d
```

Short-circuits the solve entirely. `explain` shows a single `(override)` row.
Use when the fit is unsalvageable and you know the right shape.

### Pin capacity type

```bash
rio-cli sla override <pname> --capacity=on-demand --ttl 7d
```

Filters the admissible hw set to one `karpenter.sh/capacity-type`. Combine
with any of the above. Use when spot interruption is the actual cause of
`prediction_ratio` skew.

### Reset (drop samples, refit from cold)

```bash
rio-cli sla reset <pname> --system x86_64-linux --tenant <tenant>
```

Deletes all `build_samples` for the key and evicts the cached fit. Next
dispatch falls back to the cold-start probe. Use when the pname's behaviour
changed upstream (version bump, build-system rewrite) and old samples are
poisoning the curve.

### Verify

```bash
rio-cli sla list --pname <pname>    # confirm override row
rio-cli sla status <pname>          # Override: line populated
rio-cli sla explain <pname>         # candidate table reflects override
```

To remove: `rio-cli sla clear <id>` (id from `sla list`).

## Reference: tier ladder & ceilings

```bash
rio-cli sla defaults
```

Prints the configured `[sla].tiers` (tightest first), the cold-start probe
shape, `max_cores`/`max_mem`/`max_disk` ceilings, and the hw-class set with
its reference class. Use this to pick a `--tier` value or to sanity-check
`--p90` against what the ladder already offers.

## Alert reference

### RioSla PredictionDrift

`histogram_quantile(0.5, rio_scheduler_sla_prediction_ratio_bucket)` outside
`[0.5, 2.0]` for 15m. The model is systematically off by ≥2× on `dim=wall` or
`dim=mem`. Follow [Step 1](#step-1-identify-the-offending-pname) → [Step
4](#step-4-override-or-reset). If many pnames drift simultaneously, suspect
hw drift (see `RioSlaPriorDivergenceClamped`) rather than per-key rot.

### RioSla PriorDivergenceClamped

`rio_scheduler_sla_prior_divergence{param}` pinned at `0.5` or `2.0` for 10m.
The fleet-median prior parameter has diverged from the operator-probe basis in
`[sla].probe` — the fleet is building things shaped very differently from what
the probe assumes. Not a per-pname issue: re-run the probe characterisation
and update `[sla].probe` in helm values, or widen the clamp band. `rio-cli sla
defaults` shows the current probe shape.

### RioSla HwCostStale

`rio_scheduler_sla_hw_cost_stale_seconds > 1800` for 5m. The spot-price poller
hasn't refreshed in >30m (it ticks every 10m; auto-clamp to helm seed at 60m).
Not a model-accuracy issue — cost ranking degrades, not sizing. Check
scheduler leader-lease (`kubectl -n rio-system get lease rio-scheduler`) and
`ec2:DescribeSpotPriceHistory` IRSA permissions. Cross-reference
`rio_scheduler_sla_hw_cost_fallback_total{reason}`.

### RioNodeclaimPool IceMaskedHigh

≥3 `(hw_class, capacity)` cells reaping NodeClaims for `reason=ice`
(InsufficientCapacity / quota / unfulfillable instance type). The admissible
set is shrinking toward `rio_scheduler_sla_hw_ladder_exhausted_total`. Check
Karpenter controller logs and AWS quota; `rio-cli sla override <pname>
--capacity=on-demand` on hot pnames as a stopgap.

### RioNodeclaimPool StuckPending

NodeClaim created but not Registered for >90s (~3× boot lead-time seed).
Either Launched=False (ICE — see above) or Launched=True but kubelet never
joined (AMI / nodeadm / CNI break). Not model-related; `kubectl get nodeclaims
-o wide` and inspect `Launched`/`Registered` conditions.

## Troubleshooting Matrix

| Symptom | Check | Fix |
|---|---|---|
| `explain` shows `(no fit — cold-start probe)` but pname built many times | `sla status` `tenant` arg | Model key is tenant-scoped; pass `--tenant`. |
| Override set but `explain` ignores it | `sla list` expiry column | `--ttl` lapsed, or wildcard precedence lost to a more-specific row. |
| `prediction_ratio` skewed only on `dim=mem` | `Fit:` line `M(c)=…` vs `M=p90` | `Coupled` mem fit with low `n_eff` over-extrapolates; `--mem` override. |
| Every pname drifts at once | `RioSlaPriorDivergenceClamped` firing? | hw_perf drift, not per-key rot — don't mass-override. |
| `reset` then immediate re-drift | upstream pname change mid-window | `--cores` pin until new behaviour stabilises; reset again after. |
