# Stage-C calibration overrides for `retryPolicy.qnt`

One file per fix family (G1–G8 from the retry protocol inventory; G6 has no
file — every G6 commit is NOT-ENCODED by the design's pre-registration).
Each module instantiates the as-built `../retryPolicy.qnt`, defines a local
PRE-FIX variant of one entry-point action (the behavior the named historical
fix removed), and exposes it through a `calibStep` (and, where the
distinguishing baseline needs the same restricted alphabet, a
`baselineStep`). The reference-fold ghost keeps the as-built/post-fix
semantics in every override — it is the specification oracle the refinement
invariants compare against.

Run an override (serially — the Apalache server port is shared):

```
quint verify --backend=tlc --main=<module> --step=calibStep \
  --invariant=<predicted invariant> docs/spec/models/calibration/retry-<g>.qnt
```

The verdict table — every corpus commit, its classification, override
module, predicted vs. actual verdict, depth/state counts, and disposition —
lives in `docs/spec/models/retry-invariant-map.md` (the Stage-C calibration
section). A subset of the overrides is wired into `nix/quint.nix` as
permanent expect-violation checks (`quint-retry-calib-*`); the rest are
evidence modules, re-runnable on demand with the command above.
