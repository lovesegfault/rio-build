# Stage-C calibration overrides for the as-built protocol models

One file per fix family of the controller-formal calibration corpus
(G-A/G-B/G-G over `spawnCoherence.qnt`, M1/M2/M3-M4/FFD-cover over
`nodeclaimLifecycle.qnt`; the families whose every member is NOT-ENCODED
have no file), one file per fix family of the refcount corpus
(`refcount-*.qnt` over `chunkLiveness.qnt` / `chunkCollect.qnt`), one
file per fix family of the gateway connection-lifecycle corpus
(`gw-f*.qnt` over `gwConnLifecycle.qnt`), one file per fix family of
the closure-evidence corpus (`closure-f*.qnt` over `closureEvidence.qnt`,
the F1–F14 representatives of the closure-evidence-formal Phase 0d
gate; the families whose members are NOT-ENCODED — F15/F16/F17 — have
no file; the closure-evidence corpus additionally carries two Phase-1
fix regression pins, `closure-c3-no-reprobe.qnt` and
`closure-a17-unfenced.qnt`, which freeze the PRE-Phase-1-fix production
actions — the settlement-less fail-fast and the unfenced PG apply —
rather than a historical origin/main fix, so the wired expect-violation
checks over them keep the fixed defect classes permanently
re-discoverable), one file per
transferred family of the substitution-replacement C-prime corpus
(`mat-*.qnt` over `materializationJob.qnt` — the §9.3 calibration
transfer: the closure-evidence family successors plus the Phase-A/B
fix regression pins B1/B3/B4/B5a, which freeze the pre-fix production
behaviors the Phase-B bug harvest removed; the two liveness rows
`mat-b1-claim-refuses-marked.qnt` / `mat-b3-no-redial.qnt` are
WITNESS-FLIP modules — their calibStep makes the paired reachability
witness UNVIOLABLE instead of falsifying an invariant, and
`mat-f1-no-presence-recheck.qnt` is the by-construction row whose
calibStep explores exactly the pre-fix-only delta space and must NOT
falsify the §9.1 conjunction), and — the executor-lifecycle campaign
— the single re-encoded pull-era
override (`executor-f4-pull-establish-early.qnt`, over the re-targeted
live `executorSession.qnt` rather than a frozen as-built encoding). The
executor corpus's as-built representatives
(`executor-<family>-<slug>.qnt` over `executorSessionAsBuilt.qnt`) were
retired with the as-built model on 2026-05-29, and the
`executor-f2d-*.qnt` pair over `executorDelivery.qnt` was deleted with
Model D at the 1d builder collapse — git history is the archive; the
executor invariant map's Stage-C tables and retirement records hold
their verdicts. Each module instantiates the model it names, defines a
local PRE-FIX variant of one action (the behavior the named
historical fix removed), and exposes it through a `calibStep`. The
violation latches inside the pre-fix action keep the instantiated
model's oracle: the behavior vals are reverted, the violation vals are
not, so a falsification means that model's invariant set re-finds that
bug class.

The gateway files follow the same pattern with one addition forced by
the model's scale: the full-alphabet `step` of `gwConnLifecycle.qnt`
does not exhaust inside the per-check budget (the Stage-B B-measure), so
each `gw-f*.qnt` override restricts its `calibStep` to the owning
family's letters (the same single-rich-dimension principle as the wired
`gwConnLifecycleFam*` checks) and the as-built baseline is an explicit
`baselineStep` in the same file — the same alphabet and constants with
the as-built action(s) restored — rather than the imported full `step`.
T-direction (permissiveness) overrides additionally re-introduce the
over-tight pre-fix guard and are run against the named P-property.

Run an override (serially — the bundled Apalache server port is shared):

```
quint verify --backend=tlc --main=<module> --step=calibStep \
  --invariant=<predicted invariant> docs/spec/models/calibration/controller-<family>.qnt
```

Distinguishing baselines: where an override runs at non-regime constants
(e.g. `gbCalibAckOnlyNew` at CEILING=2) or pins a module-local invariant
(e.g. `m2CalibInflightDropOnSight`), the as-built baseline is the same
module run WITHOUT `--step` (the imported as-built `step` over identical
constants); it must HOLD the same invariant for the falsification to be
attributable to the reverted behavior. Overrides at standard regime
constants use the wired Stage-B regime checks as their baseline.

The verdict table — every corpus commit, its classification, override
module, predicted vs. actual verdict, depth/state counts, and
disposition — lives next to the owning campaign's invariant map
(`controller-invariant-map.md`, `refcount-invariant-map.md`,
`executor-invariant-map.md`, `closure-evidence-invariant-map.md`,
`gw-session-invariant-map.md`,
`substitution-replacement-invariant-map.md`, each in its Stage-C /
Phase-0d / Phase-C-prime calibration section).
A subset of the overrides is wired into `nix/quint.nix` as permanent
expect-violation checks (`quint-ctrl-calib-*`, `quint-refcount-calib-*`,
`quint-executor-calib-*`, `quint-closure-calib-*`, `quint-gw-calib-*`,
`quint-materialization-calib-*`);
the rest are evidence modules, re-runnable on demand with the command
above.
