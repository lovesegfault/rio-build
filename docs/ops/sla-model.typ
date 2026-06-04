#import "/lib/rio.typ": *

#show: rio.with(domains: none)

The @sla sizer (ADR-023) fits a per-`(pname, system, tenant)` duration/memory
curve from observed builds and solves for the cheapest core count that hits
the configured tier targets. When the fit is wrong — stale samples, hw drift,
a pname whose behaviour changed upstream — builds get under- or
over-provisioned and #(refs.alert)("RioSlaPredictionDrift") fires.

This runbook walks: alert → identify pname → inspect solve → diagnose →
override or reset.

= Alert entry points

#table(
  columns: 3,
  align: (left, left, left),
  [Alert], [Fires when], [Section],
  [#(refs.alert)("RioSlaPredictionDrift")],
  [p50 of actual/predicted outside `[0.5, 2.0]` for 15m],
  [#link(<step-2>)[Diagnose a pname]],

  [#(refs.alert)("RioSlaPriorDivergenceClamped")],
  [fleet-prior parameter clamped at band edge for 10m],
  [#link(<riosla-priordivergenceclamped>)[Prior divergence]],

  [#(refs.alert)("RioSlaHwCostStale")],
  [hw-band \$/vCPU·hr snapshot >30m old],
  [#link(<riosla-hwcoststale>)[Hw-cost stale]],

  [#(refs.alert)("RioNodeclaimPoolIceMaskedHigh")],
  [≥3 cells reaping NodeClaims for `reason=ice`],
  [#link(<rionodeclaimpool-icemaskedhigh>)[Admissible set shrinking]],

  [#(refs.alert)("RioNodeclaimPoolAllCellsIceMasked")],
  [SpawnIntents dropped — every hosting cell is #gls("ice")-masked],
  [#link(<rionodeclaimpool-icemaskedhigh>)[Admissible set shrinking]],

  [#(refs.alert)("RioNodeclaimPoolStuckPending")],
  [NodeClaim in-flight >2× cell `ice_timeout` (floor 90s, cap 3×maxLeadTime)],
  [#link(<rionodeclaimpool-stuckpending>)[Provisioning stuck]],

  [#(refs.alert)("RioNodeclaimPoolBootTimeoutLoop")],
  [A cell is repeatedly minting and reaping NodeClaims for `reason=boot-timeout`],
  [#link(<rionodeclaimpool-boottimeoutloop>)[Boot-timeout loop]],

  [#(refs.alert)("RioNodeclaimPoolNoHostingClass")],
  [SpawnIntents dropped — no configured hw-class or Pool can host them],
  [#link(<rionodeclaimpool-nohostingclass>)[No hosting class]],

  [#(refs.alert)("RioNodeclaimPoolStuckTerminating")],
  [A NodeClaim has had `deletionTimestamp` set >5m without @karpenter's finalizer clearing],
  [#link(<rionodeclaimpool-stuckterminating>)[Stuck terminating]],
)

The first three are model-accuracy alerts; the last six are provisioning
alerts that share the same `rio-cli sla` diagnostic surface.

= Step 1: Identify the offending pname <step-1>

#(refs.alert)("RioSlaPredictionDrift") is fleet-wide (labelled by `dim=wall|mem`, not by
pname — #(refs.metric)("rio_scheduler_sla_prediction_ratio") is a histogram, so there is
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
with a fitted curve complete. If empty (cold start, leader just
failed over, or no builds with a fit yet), fall back to
`rio-cli sla export-corpus -o /dev/stdout --min-n 5` to enumerate
pnames the estimator HAS fitted, then `sla status` each.
`sla list` lists *operator overrides* only — on a cluster with no
overrides it prints `(no overrides)` and gives no signal about which
pname is misbehaving.

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

= Step 2: Inspect the solve <step-2>

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

#table(
  columns: 2,
  align: (left, left),
  [Field], [Meaning],
  [`Fit:`],
  [Duration model (`Amdahl`/`Capped`/`Usl`/`Probe`), `σ` residual, `n_eff_ring` effective sample count, mem model],

  [`Prior:`],
  [`per-key` (own samples), `fleet-prior` (partial-pooled), `none` (cold start)],

  [`C*`],
  [Solved core count for that tier; `-` if the tier was rejected before forming the quadratic],

  [`CONSTRAINT`],
  [Binding reject reason: `serial-floor` (S alone breaches target), `envelope` (infeasible at cap_c against p50∧p90∧p99), `mem-ceiling`, `disk-ceiling`, `no-bounds` (tier has no targets → feasible at cap_c), `-` (feasible)],
)

Dispatch picked the *first* `FEASIBLE yes` row.

= Step 3: Diagnose

== `n_eff` low (\<8)

Few effective samples → wide confidence → headroom multiplier is high
(`headroom(n_eff)` ≈ 1.95 at `n_eff=1`, ≈ 1.32 at `n_eff=100`). Over-provisioning
is expected here; the model self-corrects as samples accrue. Only act if the
key is hot enough that waiting is unacceptable — see
#link(<step-4>)[Override].

`Prior: none` with an empty candidate table = cold start. Dispatch is using
the probe shape from `[sla].probe`; nothing to fix unless the probe shape
itself is wrong (see `rio-cli sla defaults`).

== `σ` high (>0.3) or `span` low (\<2)

High residual variance or all samples clustered at one core count → the curve
is under-constrained. `sla reset` forces a fresh probe ladder that will spread
samples across `c`. If the pname is genuinely noisy (e.g. test suites with
random seeds), pin it with `--cores` instead.

== hw bias / `hw_perf_factors` drift

`prediction_ratio` skewed on builds dispatched to one `hw_class` only (check
`rio-cli sla status <pname>` for the dispatch-time hw assignment, or the
build's structured log `hw_class` field) → the per-hw normalization factor
is stale. The
`hw_perf_samples` table is 7-day windowed; a hardware change takes up to a
week to wash through. No per-pname fix — the fleet-median recomputes every
estimator refresh tick (\~#qty("60", "s")). Cross-check
#link(<riosla-priordivergenceclamped>)[#(refs.alert)("RioSlaPriorDivergenceClamped")].

== Wrong tier

`explain` shows the key feasible at a tighter tier than you want (over-spend)
or rejected from the tier you expect (`serial-floor` / `envelope`):

- `serial-floor` on a tier you believe should fit → the fitted `S` is too
  high. Check upstream: did the pname grow a long single-threaded phase?
- `envelope` at low `cap_c` → `p̄` (parallelism cap) is too low. Often a
  `Capped` fit from a narrow sample span — `sla reset` to re-explore.
- Feasible at `fast` but you want `normal` cost → pin with `--tier=normal`.

= Step 4: Override or reset <step-4>

All overrides are `(pname, system?, tenant?)`-scoped (NULL = wildcard,
most-specific wins) and take effect on the next estimator refresh (\~#qty("60", "s")).

== Pin to a named tier

```bash
rio-cli sla override <pname> --tier=normal --ttl 7d
```

Solve still runs; the candidate table is filtered to that tier only. Use when
the model is _right_ but you want a different cost/latency trade-off.

== Pin ad-hoc p50/p90/p99 targets

```bash
rio-cli sla override <pname> --p90=20m --p99=1h --ttl 7d
```

Solve runs against a one-off tier built from these targets instead of the
config ladder. Any subset of `--p50`/`--p90`/`--p99` is accepted. Use when no
named tier fits (see `rio-cli sla defaults` for the ladder).

== Force cores/mem (bypass model)

```bash
rio-cli sla override <pname> --cores=16 --mem=32Gi --ttl 7d
```

Short-circuits the solve entirely. `explain` shows a single `(override)` row.
Use when the fit is unsalvageable and you know the right shape.

== Pin capacity type

```bash
rio-cli sla override <pname> --capacity=on-demand --ttl 7d
```

Filters the admissible hw set to one `karpenter.sh/capacity-type`. Combine
with any of the above. Use when spot interruption is the actual cause of
`prediction_ratio` skew.

== Reset (drop samples, refit from cold)

```bash
rio-cli sla reset <pname> --system x86_64-linux --tenant <tenant>
```

Deletes all `build_samples` for the key and evicts the cached fit. Next
dispatch falls back to the cold-start probe. Use when the pname's behaviour
changed upstream (version bump, build-system rewrite) and old samples are
poisoning the curve.

== Verify

```bash
rio-cli sla list --pname <pname>    # confirm override row
rio-cli sla status <pname>          # Override: line populated
rio-cli sla explain <pname>         # candidate table reflects override
```

To remove: `rio-cli sla clear <id>` (id from `sla list`).

= Reference: tier ladder & ceilings

```bash
rio-cli sla defaults
```

Prints the configured `[sla].tiers` (tightest first), the cold-start probe
shape, `max_cores`/`max_mem`/`max_disk` ceilings, and the hw-class set with
its reference class. Use this to pick a `--tier` value or to sanity-check
`--p90` against what the ladder already offers.

= Alert reference

== RioSla PredictionDrift

`histogram_quantile(0.5, rio_scheduler_sla_prediction_ratio_bucket)` outside
`[0.5, 2.0]` for 15m. The model is systematically off by ≥2× on `dim=wall` or
`dim=mem`. Follow #link(<step-1>)[Step 1] → #link(<step-4>)[Step 4]. If many
pnames drift simultaneously, suspect hw drift (see
#(refs.alert)("RioSlaPriorDivergenceClamped")) rather than per-key rot.

== RioSla PriorDivergenceClamped <riosla-priordivergenceclamped>

#(refs.metric)("rio_scheduler_sla_prior_divergence")`{param}` pinned at `0.5` or `2.0` for 10m.
The fleet-median prior parameter has diverged from the operator-probe basis in
`[sla].probe` — the fleet is building things shaped very differently from what
the probe assumes. Not a per-pname issue: re-run the probe characterisation
and update `[sla].probe` in helm values, or widen the clamp band. `rio-cli sla
defaults` shows the current probe shape.

== RioSla HwCostStale <riosla-hwcoststale>

`min(`#(refs.metric)("rio_scheduler_sla_hw_cost_stale_seconds")`) > 1800` for 5m. The spot-price poller
hasn't refreshed in >#qty("30", "min") (it ticks every #qty("10", "min"); auto-clamp to helm seed at #qty("60", "min")).
Not a model-accuracy issue — cost ranking degrades, not sizing. Check
scheduler leader-lease (`kubectl -n rio-system get lease rio-scheduler-leader`
— the name is `helm:scheduler.leaseName`, not the Deployment name) and
`ec2:DescribeSpotPriceHistory` @irsa permissions. Cross-reference
#(refs.metric)("rio_scheduler_sla_hw_cost_fallback_total")`{reason}`.
The gauge is per-replica and the standby's copy climbs forever (no
poller off-leader --- by design); the `min()` keys the alert to the
*fleet's freshest* snapshot, so a lone healthy replica keeps the alert
silent. That silence is intentional: one fresh poller is a working
cost source (merged_bug_235).

== RioNodeclaimPool IceMaskedHigh <rionodeclaimpool-icemaskedhigh>

Two coupled alerts watch the same failure from opposite ends:

- *#(refs.alert)("RioNodeclaimPoolIceMaskedHigh")* (cause-side): ≥3 `(hw_class, capacity)`
  cells reaping NodeClaims for `reason=ice|vanished`. The admissible set is
  shrinking toward #(refs.metric)("rio_scheduler_sla_hw_ladder_exhausted_total").
- *#(refs.alert)("RioNodeclaimPoolAllCellsIceMasked")* (consequence-side): cold-start
  SpawnIntents (`hw_class_names=[]`) dropped because *every* cell that
  could host them is ICE-masked. The build's drv stays `Ready` and
  unroutable; the scheduler logs `no registered executor advertises this
  system`. Fires even when only *one* cell is ICE'd (e.g. a kvm-only
  build whose single `metal-*:od` cell is failing) — the cause-side alert's
  ≥3-cell threshold doesn't reach that case.

Both mean *NodeClaim launches are failing in the cloud, not a
`[sla.hw_classes]` config gap*. Check the Karpenter controller log for
CreateFleet errors (capacity, quota, IAM) and
#(refs.metric)("rio_controller_nodeclaim_reaped_total")`{reason=~"ice|vanished"}`:

```bash
kubectl logs -n kube-system deploy/karpenter --since=15m | grep -iE 'failed launching|insufficient|UnfulfillableCapacity'
```

Common structural cause on a fresh AWS account: `AWSServiceRoleForEC2Spot`
does not exist (auto-created on first spot launch, but Karpenter's IAM role
lacks `iam:CreateServiceLinkedRole`). Symptom in the Karpenter log:
`AuthFailure.ServiceLinkedRoleCreationNotPermitted`. Fix once per account:

```bash
aws iam create-service-linked-role --aws-service-name spot.amazonaws.com
```

After fixing the cloud-side cause, the in-memory `IceBackoff` self-heals on
TTL expiry (`60s × 2^step`, capped at `sla.maxLeadTime`). To clear it
immediately, `kubectl rollout restart deploy/rio-scheduler -n rio-system` —
the backoff is lease-holder-only and @dag state recovers from PG.

For genuine spot capacity exhaustion (not structural), `rio-cli sla override
<pname> --capacity=on-demand` on hot pnames as a stopgap, or set
`capacityTypes: [on-demand]` on the affected `[sla.hw_classes]` entry. The
cold-start `fallback_cell` deliberately offers only the cheapest capacity
type per class (spot for default classes), so a structural spot failure
will NOT auto-escape to on-demand — that escape valve is the operator's
call (cost vs availability).

== RioNodeclaimPool StuckPending <rionodeclaimpool-stuckpending>

NodeClaim created but not Registered for >2× the cell's reap timeout
(#(refs.metric)("rio_controller_nodeclaim_ice_timeout_seconds")`{cell}` =
`max(2×lead_time_seed, q_0.99(boot))`), clamped to a 90s floor /
3×maxLeadTime cap (default 1800s) — \~90s for EBS cells (seed≈18s, ice\_timeout
≈36s, 2×=72s → floored at 90), \~30m for `metal-*` cells (seed=600s, ice\_timeout
≥1200s → capped at 1800s). The threshold sits above the controller's own
`ice_timeout` reap; a firing alert means the reaper failed, not just a slow
boot. Either Launched=False (ICE — see above) or Launched=True but kubelet
never joined (AMI / nodeadm / CNI break). Not model-related; check
rio-controller leases (`kubectl -n rio-system get leases`), then
`kubectl get nodeclaims -o wide` and inspect `Launched`/`Registered` conditions.

== RioNodeclaimPool BootTimeoutLoop <rionodeclaimpool-boottimeoutloop>

A cell has reaped ≥2 NodeClaims with `reason=boot-timeout` in a `4×maxLeadTime + 4×TICK`
window (default 2440s) for 5 minutes. `cover_deficit` mints a NodeClaim, kubelet
never registers within the 2×seed boot timeout, `health::classify` reaps it
(`ReapReason::BootTimeout` — NOT ICE-masked, since capacity exists and the
_boot_ failed), and `cover_deficit` re-mints. The loop is unbounded: each
cycle holds an idle metal instance for `2×seed` (\~#qty("20", "min")), with zero builds
completing and the kvm/nixos-test queue growing.

Distinct from `StuckPending` (which fires when the _reaper_ fails) — this
alert fires when the reaper succeeds repeatedly. The `>= 2` count gate
distinguishes a one-off slow boot (1 reap, never fires) from a sustained
loop; the `4×maxLeadTime + 4×TICK` window spans 2 full reap cycles plus
slack (`2×(2×seed + 2×TICK) ≤ 4×maxLeadTime + 4×TICK`) so the alert
holds — not flaps — for the loop's duration (r35 merged\_bug\_027 widened
3×→4×; r43 bug\_024 added the `+4×TICK` slack for the tick-grid latency
the bare `2×seed` model omits).

Likely causes: broken AMI image (post-release), nodeadm regression, EC2
firmware update, NVMe reorder breaking instance-store mounts. Diagnose:
`kubectl get nodeclaims -o wide`, then SSM/EC2-serial-console on a live
in-flight claim before the reaper deletes it. Stop the burn:
`kubectl -n rio-system scale --replicas=0 deploy/rio-controller` (the loop has no value),
then fix the AMI and re-roll.

== RioNodeclaimPool NoHostingClass <rionodeclaimpool-nohostingclass>

`fallback_cell` found no `[sla.hw_classes]` entry that admits the intent
*even with no ICE-masking* (`reason=no_hosting_class`), or the
provisioner's Pool-coverage filter dropped the intent
(`reason=no_pool_covers`). Either way: no NodeClaim is minted, the pod's
`nodeSelector`/`nodeAffinity` will never match a node, and the build is
permanently Pending. This alert and #(refs.alert)("RioNodeclaimPoolAllCellsIceMasked") are
the ONLY signals for the "no NodeClaim was ever minted" failure class —
every other nodeclaim alert is NodeClaim-derived, and a NodeClaim that was
never minted emits no series.

This alert is *config-static*: it never self-heals; the operator must
change `[sla.hw_classes]` or the build's declared features. If a class CAN
host the intent but every hosting cell is currently ICE-masked, that's a
*different* failure (`reason=all_cells_ice_masked`,
#(refs.alert)("RioNodeclaimPoolAllCellsIceMasked")) with the opposite operator action —
fix the cloud, don't touch the config. See #link(<rionodeclaimpool-icemaskedhigh>)[IceMaskedHigh].

Causes by `{{ $labels.reason }}`:

*`reason=no_hosting_class`* — `fallback_cell` found no `[sla.hw_classes]`
entry for the intent's `(arch, size, required_features)`:

- *`required_features` unmatched* — no `[sla.hw_classes.$h]` entry has
  `provides_features` covering the intent's `required_features`. Most likely
  after adding a new system-feature (`kvm`, `nixos-test`, a custom feature).
  Add or fix the hw-class. Check #(refs.metric)("rio_scheduler_unroutable_features_total")
  for the same shape on the scheduler side — if BOTH fire the misconfig is
  in `[sla.hw_classes]`; if only this one, the scheduler routed but the
  controller's `HwClassConfig` is stale (#qty("300", "s") refresh). See also
  `intent_dropped_total{reason="unknown_hw_class"}` — that reason fires when
  the SCHEDULER stamped a hwClass the CONTROLLER doesn't yet know; it
  self-heals within the GetHwClassConfig refresh and is a `warn!` not an alert.
- *Footprint exceeds every arch-matching class's ceiling* — an
  override-bypass (`--cores=N`) intent larger than every class's
  `max_cores`/`max_mem`. Check
  #(refs.metric)("rio_controller_nodeclaim_intent_dropped_total")`{reason="exceeds_cell_cap"}`
  for the same drv.
- *Featureless arch-unmappable system* — a non-@fod `system="builtin"` or
  `darwin-*` build with no `requiredSystemFeatures` to route on. There is no
  class to mint for an intent that constrains on neither arch nor features;
  the build cannot be scheduled. Operator must add a `[sla.hw_classes.$h]`
  with `provides_features` matching the feature the build SHOULD be
  declaring, or fix the build.

*`reason=no_pool_covers`* — a `[sla.hw_classes]` entry advertises a
feature/system but no `Pool` (Builder or Fetcher) covers the intent's
`(kind, system, effective_features)`. The provisioner would mint a NodeClaim
for it but the placer would never spawn a Job onto it — the node would be
permanently idle. `kubectl get pools.rio.build -A` and compare `spec.systems` /
`effective_features` against the dropped intent's `system` / features. Add
a Pool or remove the hwClass entry.

Diagnose: scheduler logs WARN with the unroutable `(system, features)` tuple
once per `(tenant, features)` edge. The controller logs WARN once per intent
drop (all five reasons: `no_hosting_class`, `all_cells_ice_masked`,
`no_pool_covers`, `exceeds_cell_cap`, `unknown_hw_class`). `tracey query rule
ctrl.pool.fetcher-affinity-from-intent` for the spec text.

== RioNodeclaimPool StuckTerminating <rionodeclaimpool-stuckterminating>

A NodeClaim has been in the terminating state (`metadata.deletionTimestamp`
set, Karpenter finalizer running) for >#qty("5", "min"). Healthy drain is \~#qtyrange("60", "90", "s"). The
NodeClaim still counts against `max_fleet_cores` (the EC2 instance bills
until the finalizer clears, by design — see `ffd.rs`), so a stuck finalizer
permanently reduces effective fleet capacity for that cell.

+ `kubectl get nodeclaims -o wide` — find the stuck object (the one with
  the oldest `Age` past `deletionTimestamp`).
+ `kubectl describe nodeclaim <name>` — check the `Disruption` and
  `Termination` conditions for the blocker.
+ Common causes: a non-draining DaemonSet pod (Karpenter waits for all
  non-DS pods to evict, but a DS pod with `tolerations: ["*"]` can block
  the Node delete); a stuck volume detach; the Karpenter controller
  itself crashlooping.
+ If the EC2 instance is already gone (spot interruption), the Karpenter
  finalizer is waiting on a Node delete that never arrives: `kubectl
  delete node <node-name> --force --grace-period=0`, then verify the
  finalizer clears.

= Troubleshooting Matrix

#table(
  columns: 3,
  align: (left, left, left),
  [Symptom], [Check], [Fix],
  [`explain` shows `(no fit — cold-start probe)` but pname built many times],
  [`sla status` `tenant` arg],
  [Model key is tenant-scoped; pass `--tenant`.],

  [Override set but `explain` ignores it],
  [`sla list` expiry column],
  [`--ttl` lapsed, or wildcard precedence lost to a more-specific row.],

  [`prediction_ratio` skewed only on `dim=mem`],
  [`Fit:` line `M(c)=…` vs `M=p90`],
  [`Coupled` mem fit with low `n_eff` over-extrapolates; `--mem` override.],

  [Every pname drifts at once],
  [#(refs.alert)("RioSlaPriorDivergenceClamped") firing?],
  [hw\_perf drift, not per-key rot — don't mass-override.],

  [`reset` then immediate re-drift],
  [upstream pname change mid-window],
  [`--cores` pin until new behaviour stabilises; reset again after.],
)
