# Plan 0348: golden-matrix unstable/lix input auto-bump — locked revs defeat "surfaces breakage early"

[P0300](plan-0300-multi-nix-compat-matrix.md) post-PASS review. [`weekly.yml:31`](../../.github/workflows/weekly.yml) runs `.#golden-matrix` against the LOCKED `nix-unstable` and `lix` flake-input revs. [`verification.md:18`](../../docs/src/verification.md) claims the matrix "surfaces breakage early" — but without a bump step, it runs against the same locked revs every week. Currently locked at `nix-unstable=c14fb048`, `lix=774f95759` (both 2026-03-19 — fresh **now**, stale next week).

Three options:
- **(a) inline bump in weekly.yml** — `nix flake update nix-unstable lix --no-write-lock-file` before `nix build .#golden-matrix`. Lock file stays pinned for dev+CI; weekly run tests tip.
- **(b) document manual-bump requirement** — punt; operator reads `verification.md` note "bump nix-unstable/lix monthly" and forgets.
- **(c) dependabot-style lock-bump PR** — separate bot opens PRs bumping the lock, merge-on-green. Highest infra cost.

**Choose (a).** Lowest cost, closes the gap, doesn't touch the lock file (dev+CI reproducibility preserved). The weekly run is non-gating (P0300 intentionally put it in `packages.golden-matrix` not `checks.*`), so a breaking upstream change shows up as a red weekly — a signal, not a gate.

## Entry criteria

- [P0300](plan-0300-multi-nix-compat-matrix.md) merged — `weekly.yml`, the 4 flake inputs, and `.#golden-matrix` exist

## Tasks

### T1 — `fix(ci):` weekly.yml — bump unstable/lix inputs before matrix build

MODIFY [`.github/workflows/weekly.yml`](../../.github/workflows/weekly.yml) at `:31` (p300 worktree ref). Before the `nix build .#golden-matrix` step, add:

```yaml
- name: Bump unstable inputs (no lockfile write)
  run: |
    # Test against tip of nix-unstable + lix. --no-write-lock-file means
    # the flake.lock stays pinned for dev+CI reproducibility; only this
    # weekly run sees the bumped refs. nix-pinned + nix-stable stay locked
    # (those are the "guaranteed compatible" baselines).
    nix flake update nix-unstable lix --no-write-lock-file
```

**Or** (if `--no-write-lock-file` doesn't survive across steps — nix caches the lock in `.lock` in the flake source, check at dispatch):

```yaml
- name: Build golden matrix with bumped unstable inputs
  run: |
    nix build .#golden-matrix \
      --override-input nix-unstable github:NixOS/nix/master \
      --override-input lix git+https://git.lix.systems/lix-project/lix
```

The `--override-input` form is more robust (no lockfile race) but requires knowing the ref-style each input uses. Check `flake.nix` inputs block at dispatch.

### T2 — `docs:` verification.md — document the bump

MODIFY [`docs/src/verification.md`](../../docs/src/verification.md) at `:18` (the "surfaces breakage early" claim). Add a parenthetical:

```markdown
The weekly run bumps `nix-unstable` and `lix` inputs before building (via
`--override-input` in `weekly.yml`); `nix-pinned` and `nix-stable` remain at
their locked revs. Dev and CI always use locked revs — only the weekly run
tests tip.
```

## Exit criteria

- `/nixbuild .#ci` green
- T1: `grep 'override-input\|flake update nix-unstable' .github/workflows/weekly.yml` → ≥1 hit
- T1: `grep 'no-write-lock-file\|--override-input' .github/workflows/weekly.yml` → ≥1 hit
- T2: `grep 'bumps.*nix-unstable.*lix\|override-input' docs/src/verification.md` → ≥1 hit
- `git diff --stat` shows ZERO changes to `flake.lock` (lockfile untouched — the bump is weekly-runtime-only)
- Manual check: `nix build .#golden-matrix --override-input nix-unstable github:NixOS/nix/master --dry-run` → eval succeeds (proves the override form works; does not need to actually build)

## Tracey

No markers. `.#golden-matrix` is test infrastructure, not spec'd behavior (P0300 explicitly noted "Test-infra only, no tracey markers"). The `verification.md` edit is informational.

## Files

```json files
[
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T2: note the weekly-only input bump at :18 (surfaces-breakage-early claim)"}
]
```

```
.github/workflows/weekly.yml   # T1: +override-input step
docs/src/verification.md       # T2: +parenthetical at :18
```

## Dependencies

```json deps
{"deps": [300], "soft_deps": [], "note": "HARD dep P0300: weekly.yml and the 4 flake inputs (nix-pinned/nix-stable/nix-unstable/lix) arrive with it. discovered_from=300 (post-PASS review). No conflicts: weekly.yml is new from P0300, nothing else touches it. verification.md is low-traffic; P0295-T37 (from this same batch) adds cold-cache cost at :27 — non-overlapping lines."}
```

**Depends on:** [P0300](plan-0300-multi-nix-compat-matrix.md) — the weekly workflow and flake inputs. [`weekly.yml:31`](../../.github/workflows/weekly.yml) and [`verification.md:18`](../../docs/src/verification.md) are p300 worktree refs — re-grep at dispatch.

**Conflicts with:** None. `weekly.yml` is P0300-only. `verification.md` also touched by P0295-T37 (this batch) at `:27` — different lines.
