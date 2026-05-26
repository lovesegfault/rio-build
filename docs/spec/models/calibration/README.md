# Stage-C calibration overrides for the controller reconcile models

One file per fix family of the controller-formal calibration corpus
(G-A/G-B/G-G over `spawnCoherence.qnt`, M1/M2/M3-M4/FFD-cover over
`nodeclaimLifecycle.qnt`; the families whose every member is NOT-ENCODED
have no file). Each module instantiates the as-built model, defines a
local PRE-FIX variant of one tick action (the behavior the named
historical fix removed), and exposes it through a `calibStep`. The
violation latches inside the pre-fix tick keep the AS-BUILT oracle: the
behavior vals are reverted, the violation vals are not, so a
falsification means the as-built invariant set re-finds that bug class.

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
disposition — lives in `docs/spec/models/controller-invariant-map.md`
(the Stage-C calibration section). A subset of the overrides is wired
into `nix/quint.nix` as permanent expect-violation checks
(`quint-ctrl-calib-*`); the rest are evidence modules, re-runnable on
demand with the command above.
