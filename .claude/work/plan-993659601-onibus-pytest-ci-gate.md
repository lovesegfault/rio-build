# Plan 993659601: onibus-pytest CI gate — wire test_scripts.py drift-detector into .#ci

`test_tracey_domains_matches_spec` at [`.claude/lib/test_scripts.py:813`](../../.claude/lib/test_scripts.py) **is failing on sprint-1 right now**, by design — it exists exactly to catch the drift that [`120bab69`](https://github.com/search?q=120bab69&type=commits) introduced when `worker.md → builder.md + fetcher.md` renamed the component spec without touching [`TRACEY_DOMAINS` at tracey.py:15-17](../../.claude/lib/onibus/tracey.py). The frozenset still has `worker` (no spec file uses it) and is missing `builder` + `fetcher` (74 marker occurrences across `docs/src/`). `TRACEY_DOMAIN_ALT` at [`:18`](../../.claude/lib/onibus/tracey.py) builds the regex alternation from this set; [`TRACEY_MARKER_RE` at :21](../../.claude/lib/onibus/tracey.py), [`cli.py:172-174`](../../.claude/lib/onibus/cli.py), and transitively [`plan_doc.py:44`](../../.claude/lib/onibus/plan_doc.py) via `tracey_markers()` all silently drop `r[builder.*]` / `r[fetcher.*]` markers. `onibus plan tracey-markers` returns an incomplete list; plan-doc QA validation is blind.

The test was **designed exactly for this** (docstring: "catches drift in either direction — spec adds a domain, or the constant has a phantom domain that no spec uses") but it's in `.claude/lib/test_scripts.py`, which is not a constituent of `.#ci`. Red on `nix develop -c pytest`, green on `/nixbuild .#ci`. P0517's implementer noticed running pytest locally; nobody else saw it. [P0304-T532](plan-0304-trivial-batch-p0222-harness.md) covers the data sync (part a) but explicitly scopes out the CI wiring ("a broader scope decision — all test_scripts.py tests would gate").

This plan is part (b): add a `checks.onibus-pytest` constituent so the drift detector **gates**. Model on [`codecov-matrix-sync` at flake.nix:2201-2217](../../flake.nix) — an eval-time correctness assertion that went 118 commits unnoticed until wired. Same pathology here.

The scope question T532 deferred: "all test_scripts.py tests would gate". Answer: yes, and that's correct — test_scripts.py is the onibus state-machine test suite. A failing test there means the DAG runner / merger / followup pipeline has a bug. It **should** gate merges. The scary part is already past: the suite currently has one red test (T1 here fixes it) and the rest is green on sprint-1.

T1 includes the T532 data sync so this plan is self-contained: T1 makes the test green, T2 makes it gate. If T532 merges first via P0304 dispatch, T1 is a no-op (frozenset already synced). Either order works.

## Tasks

### T1 — `fix(tooling):` TRACEY_DOMAINS sync — add builder+fetcher, drop worker

At [`.claude/lib/onibus/tracey.py:15-17`](../../.claude/lib/onibus/tracey.py):

```python
# before
TRACEY_DOMAINS: frozenset[str] = frozenset({
    "common", "ctrl", "dash", "gw", "obs", "proto", "sched", "sec", "store", "worker"
})

# after
TRACEY_DOMAINS: frozenset[str] = frozenset({
    "builder", "common", "ctrl", "dash", "fetcher", "gw", "obs", "proto", "sched", "sec", "store"
})
```

Update the module docstring at `:1-8` — drop the `dash seeded by P0245` historical note (stale), add a line pointing at the test: "`test_tracey_domains_matches_spec` gates via `checks.onibus-pytest` — adding a domain to `docs/src/components/` without touching this set fails CI."

**Verify locally:** `nix develop -c pytest .claude/lib/test_scripts.py::test_tracey_domains_matches_spec` → PASS.

**Supersedes [P0304-T532](plan-0304-trivial-batch-p0222-harness.md)** — same 2-string edit. Idempotent; if T532 merged first, this diff is empty.

### T2 — `feat(ci):` add onibus-pytest constituent to miscChecks

In [`flake.nix` miscChecks block after `:556`](../../flake.nix), alongside `deny` / `tracey-validate` / `helm-lint`, add:

```nix
# Onibus state-machine tests (DAG runner / merger / plan-doc validation).
# Source: .claude/lib/test_scripts.py + .claude/lib/onibus/.
#
# Why this gates: test_tracey_domains_matches_spec catches TRACEY_DOMAINS
# drift vs docs/src/ spec markers. 120bab69 (worker→builder+fetcher rename)
# desynced the frozenset; onibus plan tracey-markers silently dropped
# r[builder.*]/r[fetcher.*] for weeks. The test was red on local pytest,
# green on .#ci — nobody saw it. Gates now.
#
# The whole suite gates, not just the drift detector — test_scripts.py
# IS the onibus tooling test suite. A red test there means the merger /
# followup pipeline / state models have a bug.
onibus-pytest = pkgs.stdenv.mkDerivation {
  pname = "rio-onibus-pytest";
  inherit version;
  src = pkgs.lib.fileset.toSource {
    root = unfilteredRoot;
    fileset = pkgs.lib.fileset.unions [
      ./.claude/lib
      ./.claude/bin
      # test_tracey_domains_matches_spec scans docs/src for r[domain.*]
      # prefixes — needs the spec files present.
      ./docs/src
    ];
  };
  nativeBuildInputs = [
    (pkgs.python3.withPackages (ps: [ ps.pytest ps.pydantic ]))
  ];
  dontConfigure = true;
  dontBuild = true;
  doCheck = true;
  checkPhase = ''
    cd .claude/lib
    # -x: stop at first failure. onibus tests are fast (~2s total);
    # the -x just means CI log shows the ONE test that broke, not
    # a cascade of dependent-failures.
    python -m pytest test_scripts.py -x -v
  '';
  installPhase = "touch $out";
};
```

**Check the fileset is complete:** test_scripts.py imports from `onibus.*` (relative-importable from `.claude/lib/`), reads `docs/src/**/*.md` (the drift test), and some tests use `tmp_path`/`monkeypatch` (no repo-root dependency). If any test reaches for `.claude/known-flakes.jsonl` or `.claude/dag.jsonl` directly (not via monkeypatch), add those to the fileset. **Grep at dispatch:** `grep -n 'REPO_ROOT\|Path(".\.claude' .claude/lib/test_scripts.py` — monkeypatched paths are fine, direct reads need inclusion.

**Check python deps:** `grep '^import\|^from' .claude/lib/test_scripts.py .claude/lib/onibus/*.py | grep -v '^from onibus\|^from \.\|^import re\|^import sys\|^import os\|^import json\|^import pathlib\|^import typing\|^import argparse\|^import pytest' | sort -u` — anything non-stdlib beyond `pydantic` needs adding to `withPackages`.

### T3 — `feat(ci):` wire onibus-pytest into checks.* + githubActions.matrix.checks

**checks.\*** at [`flake.nix:2179`](../../flake.nix): already `// miscChecks` at `:2186`, so T2's addition is picked up automatically. **Verify:** `nix eval .#checks.x86_64-linux.onibus-pytest.name` → `"rio-onibus-pytest-<version>"`.

**githubActions.matrix.checks** at [`:1533-1546`](../../flake.nix): this is a MANUAL allowlist (`inherit (miscChecks) deny tracey-validate helm-lint crds-drift tfvars-fresh;`). Add `onibus-pytest` to the inherit list:

```nix
inherit (miscChecks)
  deny
  tracey-validate
  helm-lint
  crds-drift
  tfvars-fresh
  onibus-pytest  # ← new
  ;
```

This manual-allowlist pattern is itself a footgun ([P0304-T993659601](plan-0304-trivial-batch-p0222-harness.md) covers the second-manual-list drift class generically) — but for this plan, one-line add is correct. Don't scope-creep into allowlist→blocklist refactor here.

**Final verify:** `/nixbuild .#ci` green. The test that was red is now T1-fixed; the check that makes it visible is T2+T3.

## Exit criteria

- `nix develop -c pytest .claude/lib/test_scripts.py::test_tracey_domains_matches_spec` PASSES
- `grep -E '"builder"|"fetcher"' .claude/lib/onibus/tracey.py` → ≥2 hits (both domains in frozenset)
- `grep '"worker"' .claude/lib/onibus/tracey.py` → 0 hits in TRACEY_DOMAINS
- `nix build .#checks.x86_64-linux.onibus-pytest` builds green
- `nix eval .#githubActions.matrix.checks --json | jq 'has("onibus-pytest")'` → `true`
- `/nixbuild .#ci` green
- **Mutation check:** temporarily remove `"builder"` from TRACEY_DOMAINS → `nix build .#checks.x86_64-linux.onibus-pytest` FAILS with `drift: spec has {'builder'}, constant has set()`

## Tracey

No spec markers. This is tooling — the drift detector GUARDS tracey markers, it doesn't implement a spec'd behavior. No `r[impl]` / `r[verify]` annotations.

The `worker` → `builder` + `fetcher` domain rename itself was [`120bab69`](https://github.com/search?q=120bab69&type=commits); the `r[builder.*]` and `r[fetcher.*]` markers already exist in [`docs/src/components/builder.md`](../../docs/src/components/builder.md), [`docs/src/components/fetcher.md`](../../docs/src/components/fetcher.md), [`docs/src/decisions/019-builder-fetcher-split.md`](../../docs/src/decisions/019-builder-fetcher-split.md), [`docs/src/security.md`](../../docs/src/security.md). This plan doesn't add or bump any — it makes the tooling SEE them.

## Files

```json files
[
  {"path": ".claude/lib/onibus/tracey.py", "action": "MODIFY", "note": "T1: frozenset :15-17 add builder+fetcher, drop worker; docstring :1-8 update"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: miscChecks onibus-pytest constituent after :556; T3: githubActions.matrix.checks inherit at :1537-1543"}
]
```

```
.claude/lib/onibus/
└── tracey.py            # T1: TRACEY_DOMAINS sync
flake.nix                # T2+T3: miscChecks.onibus-pytest + matrix wiring
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [304], "note": "Self-contained — T1 makes the test green, T2+T3 make it gate. Soft-dep P0304 only for T532 supersession: if P0304 dispatches T532 first, T1 is a no-op diff (idempotent). No ordering constraint."}
```

**Depends on:** none. T1's data sync + T2's CI gate are one atomic unit — the plan ships green.
**Soft-dep:** [P0304-T532](plan-0304-trivial-batch-p0222-harness.md) — same `tracey.py:15-17` edit. Idempotent (frozenset equality); whoever merges first wins, second is empty diff. Prefer this plan lands first (it's self-contained; T532-alone leaves the test un-gated and the next domain-add will rot the same way).
**Conflicts with:** `flake.nix` count=48 (HOT). T2 inserts in `miscChecks` block after `:556` — adjacent plans touching miscChecks: none currently. T3 edits `:1537-1543` inherit list — [P993659603](plan-993659603-cpuhints-k3s-suffix-fallthrough.md) touches `:1209` (different section, 300+ lines apart, clean rebase). [P0304-T538/T539](plan-0304-trivial-batch-p0222-harness.md) touch `:1179`/`:1209-1224` (same distance, clean). [P0525](plan-0525-ci-missing-codecov-matrix-sync.md) touches `:1267-1293` (also clean). `.claude/lib/onibus/tracey.py` count<5 — low-traffic, T532-only overlap.
