# Plan 0301: cargo-mutants CI wiring — mutation testing weekly tier

Originates from [`verification.md:92`](../../docs/src/verification.md). [P0221](plan-0221-rio-bench-crate-hydra-doc.md) lands criterion benchmarks; this adds the mutation-testing half. [`cargo-mutants`](https://mutants.rs/) mutates source (swap `<` for `<=`, delete a statement, replace a return value with `Default::default()`), reruns the test suite, and flags mutations that SURVIVE — code paths the tests don't actually constrain.

**Why it matters here:** rio-build has protocol parsers, scheduler state machines, PG query builders — all places where an off-by-one or a wrong comparison operator compiles, runs, and passes "does it return something" tests. tracey answers "is this spec rule covered"; mutants answers "does the test that covers it actually catch bugs." Complementary signals.

**Cost:** mutation testing is expensive — O(mutations × test-suite-time). For ~200 source files × ~20 mutations each × ~2min nextest run = hours. Weekly tier, not per-push. `--in-place --no-shuffle` keeps it deterministic for diffing results week-over-week.

**Scoping:** start with high-signal targets — the scheduler state machine ([`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs)), the wire primitives ([`rio-nix/src/wire.rs`](../../rio-nix/src/wire.rs)), the assignment scoring ([`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs)). These have tight test coverage and the mutations that survive are genuine gaps. Don't mutate `main.rs` glue or tracing setup — high mutation count, low signal.

## Entry criteria

- [P0221](plan-0221-rio-bench-crate-hydra-doc.md) merged (criterion infra + `nix run .#bench` — establishes the "manual/weekly validation attrs outside `.#ci`" pattern)

## Tasks

### T1 — `feat(nix):` `cargo-mutants` in dev shell + `.#mutants` package

MODIFY [`flake.nix`](../../flake.nix) — add `cargo-mutants` (nixpkgs has it) to dev shell `nativeBuildInputs`. New package attr (not `checks.` — mutation test failures aren't build failures, they're findings):

```nix
packages.mutants = pkgs.stdenv.mkDerivation {
  name = "rio-mutants";
  src = commonArgs.src;
  nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
    pkgs.cargo-mutants
    rustToolchain
  ];
  buildInputs = commonArgs.buildInputs;
  # cargo-mutants writes to .mutants/ in cwd — use $TMPDIR
  buildPhase = ''
    export CARGO_TARGET_DIR=$TMPDIR/target
    cargo mutants \
      --in-place --no-shuffle \
      --config .config/mutants.toml \
      --output $out \
      --timeout-multiplier 2.0 \
      || true  # survived mutants → exit 2, not a build failure
  '';
  installPhase = ''
    # $out already has mutants.out/ — add a summary
    grep -c CAUGHT $out/mutants.out/outcomes.json > $out/caught-count || true
    grep -c MISSED $out/mutants.out/outcomes.json > $out/missed-count || true
  '';
};
```

The `|| true` on the build phase: `cargo mutants` exits 2 when mutants survive. That's the EXPECTED outcome (no codebase is 100% mutation-killed). The derivation succeeds; we diff `missed-count` week-over-week.

### T2 — `feat(tooling):` `.config/mutants.toml` scope config

NEW [`.config/mutants.toml`](../../.config/mutants.toml). cargo-mutants defaults to `.cargo/mutants.toml` but accepts `--config <path>`; the `.config/` location sits alongside `nextest.toml` and matches the existing Crane fileset pattern:

```toml
# Mutation testing scope. Start narrow — high-signal targets where
# surviving mutants are real test gaps. Expand over time as the
# missed-count baseline is understood.

# Files to mutate. Globs relative to workspace root.
examine_globs = [
  "rio-scheduler/src/state/derivation.rs",  # state machine transitions
  "rio-scheduler/src/assignment.rs",        # best_worker scoring + filters
  "rio-nix/src/wire.rs",                    # protocol primitives
  "rio-nix/src/aterm.rs",                   # .drv parser
  "rio-common/src/hmac.rs",                 # assignment token MAC
  "rio-store/src/manifest.rs",              # chunk manifest encoding
]

# Never mutate these — high churn, low signal.
exclude_globs = [
  "**/main.rs",
  "**/lib.rs",          # mostly re-exports + metric registration
  "**/*_test.rs",
  "**/tests/**",
]

# Skip mutants in functions matching these regexes (tracing spans,
# metric emission — mutations here are "did you call the metric" which
# the metrics_registered test already covers).
exclude_re = [
  "tracing::",
  "metrics::",
  "^describe_",
]

# Per-file timeout: 2× the baseline test time. If a mutation
# makes tests hang (infinite loop), kill after 2× and count as CAUGHT.
timeout_multiplier = 2.0
```

The Crane source filter already includes `./.config/nextest.toml` at [`flake.nix:180`](../../flake.nix) — either widen that entry to `./.config` or add `./.config/mutants.toml` alongside.

### T3 — `feat(tooling):` `just mutants` recipe + weekly workflow stub

MODIFY [`justfile`](../../justfile):

```just
# Run mutation testing locally (slow — budget ~30-60min for the
# scoped target set). Results in .mutants/out/. Diff against the
# last run to see new survivors.
mutants:
    cargo mutants --in-place --no-shuffle --config .config/mutants.toml
    @echo "Survived: $(jq '[.[] | select(.outcome == "Missed")] | length' .mutants/out/outcomes.json)"
    @echo "See .mutants/out/missed.txt for details"
```

Weekly workflow: add a `.github/workflows/mutants.yml` (or wherever rio-build's scheduled CI lives) that runs `nix-build-remote -- .#mutants`, extracts `missed-count`, and opens a tracking issue if it INCREASES from the last run. Don't fail the workflow on nonzero — mutation coverage is a trend metric, not a gate.

### T4 — `docs:` document mutants tier in verification.md

MODIFY [`verification.md`](../../docs/src/verification.md) — replace the `cargo-mutants` mention in the `:92` `> **Scheduled:**` block. Add a sentence under the CI-tier table explaining mutation testing is a weekly-tier trend metric, not a per-push gate.

## Exit criteria

- `just mutants` runs locally against the scoped target set in under 60min
- `nix build .#mutants` produces `result/mutants.out/outcomes.json` + `result/missed-count`
- `.config/mutants.toml` is in the Crane source filter (fileset union includes it)
- Initial baseline: `missed-count` recorded in the PR description for trend-tracking

## Tracey

Test-infra only — no domain markers. Mutants surfaces WHICH tests are weak, but the findings feed followups (test-gap severity), not new spec rules.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: cargo-mutants in devshell + .#mutants package; T2: .config/mutants.toml in fileset"},
  {"path": ".config/mutants.toml", "action": "NEW", "note": "T2: scope config"},
  {"path": "justfile", "action": "MODIFY", "note": "T3: mutants recipe"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T4: replace :92 Scheduled mention, document weekly tier"}
]
```

```
flake.nix                # T1: devshell + .#mutants; T2: fileset
.cargo/
└── mutants.toml         # T2: scope config (NEW)
justfile                 # T3: mutants recipe
docs/src/
└── verification.md      # T4: document
```

## Dependencies

```json deps
{"deps": [221], "soft_deps": [], "note": "phase-cleanup: verification.md:92 user-decided deferral. P0221 establishes the pattern for non-gating validation attrs outside .#ci (bench). Mutants follows same shape. cargo-mutants is in nixpkgs. Weekly tier — hours per run."}
```

**Depends on:** [P0221](plan-0221-rio-bench-crate-hydra-doc.md) — criterion bench infra. Establishes the `.#bench`-style manual-invoke pattern that `.#mutants` follows. Also: bench + mutants are the two halves of "are we fast AND are the tests real" — makes sense to land adjacent.

**Conflicts with:**
- [`flake.nix`](../../flake.nix) count=28, UNIMPL=[221, 274, 275, 282, 295]. P0221 is a hard dep → sequential. P0274/P0275/P0282 touch dashboard/docker sections — independent. T1's devshell addition and `.#mutants` package attr are both additive.
- [`justfile`](../../justfile) — not in collisions.jsonl (no UNIMPL touching it). Clean append.
- [`verification.md`](../../docs/src/verification.md) — shared `:92` line with [P0300](plan-0300-multi-nix-compat-matrix.md) (multi-Nix matrix is also in that Scheduled block). Both plans rewrite different clauses of the same sentence. Advisory-serial: whichever lands second rewrites the reduced sentence.
