# Historical-fix calibration overrides

One Quint file per fix family. Each module instantiates
`../logBufferLifecycle.qnt` with exactly one calibration switch flipped to its
pre-fix value (see the "Calibration switches" const block in the main model)
and is `quint verify`'d against the invariant its family's analysis dossier
predicts. A FALSIFIES verdict is the machine-checked statement "this fix is
what keeps that invariant true at the model's bounds"; a HOLDS verdict is
dispositioned in the calibration table (the fix protects an unstated /
not-encoded property, is redundant with another mechanism, or belongs to
model B).

The durable record is the calibration table in
`../log-invariant-map.md` (every corpus commit → classification → invariant →
regime → verdict → disposition). Only the overrides that guard against a
plausible model regression are wired into CI as expect-violation checks
(`nix/quint.nix`, the `quint-log-calib-*` attrs); the rest exist here as
evidence and are re-runnable by hand:

```
quint verify --backend=tlc --main=<module> --invariant=<invariant> \
  docs/spec/models/calibration/<family>.qnt
```

Modules are named after the behavior they restore, not the commit that fixed
it; the fixing commit is in each module's header and in the table.
