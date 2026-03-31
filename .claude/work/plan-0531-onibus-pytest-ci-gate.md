# Plan 531: onibus-pytest CI gate — wire test_scripts.py drift-detector into .#ci

`test_tracey_domains_matches_spec` at [`.claude/lib/test_scripts.py:813`](../../.claude/lib/test_scripts.py) **is failing on sprint-1 right now**, by design — it exists exactly to catch the drift that [`120bab69`](https://github.com/search?q=120bab69&type=commits) introduced when `worker.md → builder.md + fetcher.md` renamed the component spec without touching [`TRACEY_DOMAINS` at tracey.py:15-17](../../.claude/lib/onibus/tracey.py). The frozenset still has `worker` (no spec file uses it) and is missing `builder` + `fetcher` (74 marker occurrences across `docs/src/`). `TRACEY_DOMAIN_ALT` at [`:18`](../../.claude/lib/onibus/tracey.py) builds the regex alternation from this set; [`TRACEY_MARKER_RE` at :21](../../.claude/lib/onibus/tracey.py), [`cli.py:172-174`](../../.claude/lib/onibus/cli.py), and transitively [`plan_doc.py:44`](../../.claude/lib/onibus/plan_doc.py) via `tracey_markers()` all silently drop `r[builder.*]` / `r[fetcher.*]` markers. `onibus plan tracey-markers` returns an incomplete list; plan-doc QA validation is blind.

The test was **designed exactly for this** (docstring: "catches drift in either direction — spec adds a domain, or the constant has a phantom domain that no spec uses") but it's in `.claude/lib/test_scripts.py`, which is not a constituent of `.#ci`. Red on `nix develop -c pytest`, green on `/nixbuild .#ci`. P0517's implementer noticed running pytest locally; nobody else saw it. [P0304-T532](plan-0304-trivial-batch-p0222-harness.md) covers the data sync (part a) but explicitly scopes out the CI wiring ("a broader scope decision — all test_scripts.py tests would gate").

This plan is part (b): add a `checks.onibus-pytest` constituent so the drift detector **gates**. Model on [`codecov-matrix-sync` at flake.nix:2201-2217](../../flake.nix) — an eval-time correctness assertion that went 118 commits unnoticed until wired. Same pathology here.

The scope question T532 deferred: "all test_scripts.py tests would gate". Answer: yes, and that's correct — test_scripts.py is the onibus state-machine test suite. A failing test there means the DAG runner / merger / followup pipeline has a bug. It **should** gate merges.

**Verified sprint-1 baseline** — invocation matters:

| Invocation | Result | Why |
|---|---|---|
| `nix develop -c pytest .claude/lib/test_scripts.py` | **11 failed**, 96 passed, 5 skipped | `pytest`'s nixpkgs bash-wrapper prepends bare `python3-3.13.11/bin` to PATH; subprocess-onibus tests hit `#!/usr/bin/env python3` → bare interpreter → no pydantic |
| `nix develop -c python3 -m pytest .claude/lib/test_scripts.py` | **1 failed**, 106 passed, 5 skipped | No wrapper script → no PATH prepend → `env python3` resolves to the `-env` wrapper with pydantic |

The 10-test cluster (`test_dag_deps_cli`, `test_warn_cwd_elsewhere_fires`, etc.) subprocesses `.claude/bin/onibus` via [`_onibus_cli` at test_scripts.py:3405`](../../.claude/lib/test_scripts.py). `subprocess.run` inherits parent env (no `env=`), so PATH is whatever pytest's wrapper set it to. The wrapper at `$(which pytest)` → `/nix/store/...-python3.13-pytest-8.4.2/bin/pytest:15` prepends `/nix/store/slhpx9glq7...-python3-3.13.11/bin` (bare, no `-env` suffix). That's first in PATH; `#!/usr/bin/env python3` finds it; `import pydantic` fails.

`python3 -m pytest` runs pytest as a module — no bash wrapper, no PATH mangle. T2's checkPhase uses this form. **In the Nix derivation, 106 tests pass; T1 fixes the 1 remaining.** The `nix develop -c pytest` divergence is a dev-shell wart, not a derivation problem — T2's nix comment documents it so the next person who runs `pytest` locally and sees 11 red doesn't assume the CI check is broken.

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

**Verify locally:** `nix develop -c python3 -m pytest .claude/lib/test_scripts.py -q` → **0 failed** (was 1). Use `python3 -m pytest`, not bare `pytest` — see the invocation table in the intro.

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
# The whole suite gates (~107 tests), not just the drift detector —
# test_scripts.py IS the onibus tooling test suite. A red test there
# means the merger / followup pipeline / state models have a bug.
#
# DEV-SHELL DIVERGENCE: `nix develop -c pytest` shows 10 MORE failures
# than `nix develop -c python3 -m pytest`. The bare `pytest` binary is
# a nixpkgs bash-wrapper that prepends bare-python3 (no site-packages)
# to PATH; subprocess tests hit `#!/usr/bin/env python3` in onibus and
# get no pydantic. `python -m pytest` below bypasses the wrapper — PATH
# stays clean, subprocesses find the withPackages env. This check's
# result is authoritative; a local bare-pytest run is NOT.
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
      # _no_dag skipif at test_scripts.py:3415 reads this directly.
      # Absent → test_dag_deps_cli etc. skip instead of run.
      ./.claude/dag.jsonl
      # onibus/__init__.py reads this at import time.
      ./.claude/integration-branch
    ];
  };
  nativeBuildInputs = [
    (pkgs.python3.withPackages (ps: [ ps.pytest ps.pydantic ]))
    # conftest.py:18 tmp_repo fixture + several tests subprocess git.
    pkgs.git
  ];
  dontConfigure = true;
  dontBuild = true;
  doCheck = true;
  checkPhase = ''
    cd .claude/lib
    # `python -m pytest`, NOT bare `pytest` — see DEV-SHELL DIVERGENCE
    # note above. The bash wrapper for `pytest` prepends bare python3
    # to PATH; this derivation's PATH is clean going in, but the -m
    # form is defensive against nixpkgs python-wrapping changes.
    #
    # -x: stop at first failure. Suite runs ~60s; -x means the CI
    # log shows the ONE test that broke, not a cascade.
    python -m pytest test_scripts.py -x -v
  '';
  installPhase = "touch $out";
};
```

**Fileset verified complete** against sprint-1:
- `.claude/lib` + `.claude/bin`: sources + entrypoint
- `docs/src`: `test_tracey_domains_matches_spec` scans for `r[domain.*]` ([`:821`](../../.claude/lib/test_scripts.py))
- `.claude/dag.jsonl`: `_no_dag` skipif marker at [`:3414-3418`](../../.claude/lib/test_scripts.py) reads it directly via `_REAL_REPO`. Without it, `test_dag_deps_cli` / `test_dag_markers_cli` skip.
- `.claude/integration-branch`: `onibus/__init__.py` reads it at import time; `conftest.py:17` re-reads it for `tmp_repo` branch setup.
- known-flakes tests use `tmp_repo` (scratch), not the real file — no fileset entry needed.

**Python deps verified:** `pydantic` is the only non-stdlib dep beyond pytest. `onibus/__init__.py:18` imports it; nothing else external.

**Why `pkgs.git`:** `conftest.py:18-27` runs `git init` / `git commit` for the `tmp_repo` fixture. `test_known_flake_crud_edits_worktree_not_main` and the three `pre_ff_rename` tests all use it. Derivation sandbox has no ambient git.

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

This manual-allowlist pattern is itself a footgun ([P0304-T540](plan-0304-trivial-batch-p0222-harness.md) covers the second-manual-list drift class generically) — but for this plan, one-line add is correct. Don't scope-creep into allowlist→blocklist refactor here.

**Final verify:** `/nixbuild .#ci` green. The test that was red is now T1-fixed; the check that makes it visible is T2+T3.

## Exit criteria

- `nix develop -c python3 -m pytest .claude/lib/test_scripts.py -q` → **0 failed** (the `-m pytest` form — bare `pytest` shows spurious subprocess failures, see intro)
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
**Conflicts with:** `flake.nix` count=48 (HOT). T2 inserts in `miscChecks` block after `:556` — adjacent plans touching miscChecks: none currently. T3 edits `:1537-1543` inherit list — [P0533](plan-0533-cpuhints-k3s-suffix-fallthrough.md) touches `:1209` (different section, 300+ lines apart, clean rebase). [P0304-T538/T539](plan-0304-trivial-batch-p0222-harness.md) touch `:1179`/`:1209-1224` (same distance, clean). [P0525](plan-0525-ci-missing-codecov-matrix-sync.md) touches `:1267-1293` (also clean). `.claude/lib/onibus/tracey.py` count<5 — low-traffic, T532-only overlap.
