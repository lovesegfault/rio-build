# Stage-C calibration overrides for the as-built protocol models

One file per fix family of the controller-formal calibration corpus
(G-A/G-B/G-G over `spawnCoherence.qnt`, M1/M2/M3-M4/FFD-cover over
`nodeclaimLifecycle.qnt`; the families whose every member is NOT-ENCODED
have no file), one file per fix family of the refcount corpus
(`refcount-*.qnt` over `chunkLiveness.qnt` / `chunkCollect.qnt`), and —
the executor-lifecycle campaign's variant of the layout — one file per
*representative* of the executor corpus
(`executor-<family>-<slug>.qnt` over `executorSession.qnt` /
`executorDelivery.qnt`; a family with two falsifying representatives
has two files). Each module instantiates the as-built model, defines a
local PRE-FIX variant of one action (the behavior the named
historical fix removed), and exposes it through a `calibStep`. The
violation latches inside the pre-fix action keep the AS-BUILT oracle: the
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
disposition — lives next to the owning campaign's invariant map
(`controller-invariant-map.md`, `refcount-invariant-map.md`,
`executor-invariant-map.md`, each in its Stage-C calibration section).
A subset of the overrides is wired into `nix/quint.nix` as permanent
expect-violation checks (`quint-ctrl-calib-*`, `quint-refcount-calib-*`,
`quint-executor-calib-*`); the rest are evidence modules, re-runnable on
demand with the command above.
