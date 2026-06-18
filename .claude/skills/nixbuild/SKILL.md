---
name: nixbuild
description: Use when running checks or nix builds in this repo — the fast edit-loop check (clippy/docs/policy, no test execution), the full CI gate, .#coverage, or any single flake target. Keeps build output out of agent context — full log to /tmp/rio-dev/, short report on stdout.
---

## Invocation

```bash
.claude/bin/nixbuild                              # edit-loop quick check (default; same as --quick)
.claude/bin/nixbuild --checks                     # full CI gate via nix-fast-build (the merge bar)
.claude/bin/nixbuild .#checks.x86_64-linux.vm-lifecycle-core-k3s   # single check
.claude/bin/nixbuild .#coverage --keep-going      # extra args pass through to nix
```

**Quiet by default.** Stderr gets one line (the log path); stdout gets a short report at the end; the full log lands in `/tmp/rio-dev/rio-<branch>-<n>.log` (`<n>` auto-increments per branch). For interactive debugging where you want live streaming output, use plain `nix build -L` instead.

**Exit code mirrors the underlying nix invocation** — 0 on green, non-zero otherwise; the report is still printed on failure. Usage errors exit 1 before running anything.

## Modes

| Mode | What runs | When / how long |
|---|---|---|
| `--quick` (default) | All of `checks.<system>` **except** test execution — excludes `vm-*`, `nextest-*`, `fuzz-*`, `golden-*`, `mutants-*`, `cov-smoke`. That leaves per-crate clippy/clippy-test/doc, pre-commit, drift + policy checks, docs/dashboard renders. Untouched crates are cache hits, so nix decides what actually rebuilds. | Edit loop. Seconds-to-a-minute when little changed; a few minutes after a crate or docs change. |
| `--checks` | `nix-fast-build` over the whole granular matrix (streams eval into builds via nix-eval-jobs). Passes `--fail-fast`: stops at the first failed build/eval instead of finishing the rest of the matrix. | Merge gate — every change must pass before merge. ~1min when the tree is mostly cached; 20min+ when VM tests / nextest need real rebuilds. |
| `<flake-target>` | Single attr via `nix build`; leaves a `./result` out-link at the repo root. | One check, `.#coverage` (~25min uncached, needs KVM), debug builds. |

`--quick` is **not** the merge bar: it skips nextest, VM, fuzz, golden-conformance, and mutation runs by design. The exclude regex lives in `.claude/bin/nixbuild` — keep this table in sync with it.

From agent context, run `--checks` and heavyweight targets (`.#coverage`) as background tasks (or with a raised timeout): the report prints only at the end, so a killed foreground call yields neither report nor exit code — only the stderr `log → …` line telling you where the partial log is.

## Output

```
nixbuild quick: FAILED (exit 1)
targets: 69
log: /tmp/rio-dev/rio-<branch>-<n>.log (143608 bytes)
failed derivations:
  /nix/store/…-rust_xtask-0.1.0-clippy.drv
  /nix/store/…-rust_xtask-0.1.0-test-clippy.drv

--- last 80 lines of /tmp/rio-dev/rio-<branch>-<n>.log ---
…
```

- First line: `nixbuild <mode> [target]: OK|FAILED (exit <rc>)` — `<rc>` is also the process exit code.
- Single-target success adds `out: /nix/store/…  (./result)`.
- On failure: the failed `.drv` paths (run `nix log <drv>` for one derivation's full build log) and the last 80 log lines. The full log is always kept at the printed path.

## Related

- VM-test python validation without booting a VM (~10s, mypy+pyflakes): `nix build .#checks.x86_64-linux.vm-<name>.driverInteractive`
- Formatting: the post-edit hook runs `nix fmt` on edited files; the `pre-commit` check (included in `--quick`) catches remaining drift — fix with `nix develop -c treefmt`.
- Gate red and the cause isn't obvious? See `.claude/rules/ci-failure-patterns.md`.
