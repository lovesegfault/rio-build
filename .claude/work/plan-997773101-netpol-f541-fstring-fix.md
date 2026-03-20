# Plan 997773101: netpol.nix F541 — f-string-without-placeholder breaks first vm-netpol build

[`nix/tests/scenarios/netpol.nix`](../../nix/tests/scenarios/netpol.nix) landed at [`501d5a1e`](https://github.com/search?q=501d5a1e&type=commits) ([P0241](plan-0241-vm-section-g-netpol.md), 2026-03-19) but the testScript has **11 f-string segments with no placeholder** — pyflakes F541. The NixOS test-driver derivation runs pyflakes on the testScript at build time; F541 fails the build before QEMU ever spawns.

This never fired because vm-netpol-k3s was never built: [P0241](plan-0241-vm-section-g-netpol.md) merged via mode-4 fast-path (drv byte-identical at pre-merge tip — the VM test wasn't in the CI closure yet, fast-fail killed it before the new drv entered the build set). [P0311-T15](plan-0311-test-gap-batch-cli-recovery-dash.md) tracked it as "FIRST-build, never KVM-executed." The first `.#ci` run that actually attempts vm-netpol-k3s hits F541 at test-driver-compile time and goes red — **sprint-1 baseline is broken in a way nothing exercises yet.**

Second F541 instance after [fix-p289](plan-0289-port-specd-unlanded-test-trio.md) found `lifecycle.nix:1613,:1639` (latent from `ade1de0c`, cache-hidden same way). Both share the pattern: multi-line implicit-concat assert messages where some segments have `{placeholders}` and continuation segments don't — pyflakes fires per-segment, not per-concat-result.

## Entry criteria

- [P0241](plan-0241-vm-section-g-netpol.md) merged — netpol.nix exists

## Tasks

### T1 — `fix(tests):` strip f-prefix from placeholder-free segments in netpol.nix

MODIFY [`nix/tests/scenarios/netpol.nix`](../../nix/tests/scenarios/netpol.nix) — 11 F541 instances:

| Line | Segment | Fix |
|---|---|---|
| `:133` | `f"NetPol ALLOWS this — if blocked, either kube-router not "` | drop `f` |
| `:134` | `f"synced or policy over-broad. Subsequent rc!=0 asserts "` | drop `f` |
| `:138` | `f"reachable from worker netns (NetPol allow-rule firing)"` | drop `f` |
| `:162` | `f"NetPol NOT enforcing. rio-worker-egress does not allow "` | drop `f` |
| `:163` | `f"apiserver; kube-router should DROP. P0220 proved this IP "` | drop `f` |
| `:164` | `f"IS reachable without policy, so rc=0 means policy absent "` | drop `f` |
| `:165` | `f"or kube-router not loading it."` | drop `f` |
| `:185` | `f"IMDS curl succeeded (rc=0) — NetPol NOT enforcing. "` | drop `f` |
| `:186` | `f"169.254.169.254 is link-local, not in any allow-rule; "` | drop `f` |
| `:201` | `f"public egress to 1.1.1.1 succeeded (rc=0) — NetPol NOT "` | drop `f` |
| `:202` | `f"enforcing. 0.0.0.0/0 not in rio-worker-egress allow-rules."` | drop `f` |

Python implicit concat is per-literal: `f"a {x} " "b"` is valid (only the first needs `f`), `f"a {x} " f"b"` trips F541 on the second. Strip the `f` prefix from segments with no `{…}` — the concat result is identical.

**Quick scan at dispatch** in case sprint-1 added more sites between plan-write and dispatch:

```bash
python3 -c "
import re
lines = open('nix/tests/scenarios/netpol.nix').read().split(chr(10))
for i, line in enumerate(lines, 1):
    for m in re.finditer(r'f\"([^\"]*)\"', line):
        if '{' not in m.group(1):
            print(f'{i}: {m.group(0)[:60]}')
"
```

### T2 — `test(tests):` verify test-driver drv builds (pyflakes-clean)

The test-driver derivation is the gate — it runs pyflakes on the testScript body at build time. Build the driver standalone (doesn't need KVM, doesn't spawn QEMU):

```bash
/nixbuild '.#checks.x86_64-linux.vm-netpol-k3s.driverInteractive'
```

This is ~10s, proves the Python is lint-clean, and gives a `./result` symlink to the test-driver script. If F541 is gone this succeeds; if any segment was missed, pyflakes fires here.

**Mechanism note:** the testScript-body change invalidates the VM-test-run drv but the test-driver drv is separate (per the fix-p289 lesson — cached-green hides F541 until something invalidates the driver drv). T1 touches netpol.nix content → both drvs invalidate → pyflakes runs.

### T3 — `refactor(tests):` opportunistic sweep for other netpol-shape F541

The same multi-line-assert pattern may have propagated. fix-p289 fixed lifecycle.nix; this plan fixes netpol.nix. Scan the remaining scenario files at dispatch:

```bash
for f in nix/tests/scenarios/*.nix; do
  python3 -c "
import re, sys
lines = open('$f').read().split(chr(10))
hits = [(i, m.group(0)) for i, line in enumerate(lines, 1)
        for m in re.finditer(r'f\"([^\"]*)\"', line) if '{' not in m.group(1)]
if hits:
    print(f'{sys.argv[1]}:', *(f'{i}' for i, _ in hits))
" "$f"
done
```

Fix any additional F541 instances in the same commit (they'd fail the same way on first build). If the scan is clean, T3 is a no-op — document "scanned clean at `<sha>`" in the commit body.

## Exit criteria

- `/nixbuild '.#checks.x86_64-linux.vm-netpol-k3s.driverInteractive'` → build succeeds (test-driver drv compiles, pyflakes passes)
- T1 scan regex: 0 F541 matches in `nix/tests/scenarios/netpol.nix` post-fix
- T3 scan regex: 0 F541 matches across `nix/tests/scenarios/*.nix` OR documented per-file exemption
- `git diff 501d5a1e -- nix/tests/scenarios/netpol.nix | grep '^-.*f"' | wc -l` ≥ 11 (all affected lines touched)
- `/nbr .#ci` — either green, or the only vm-netpol failure is infra (exit-77 KVM-denied / builder roulette), NOT a pyflakes/F541 build-time error. **The fix is pyflakes-clean, not VM-executes-green** — P0311-T15 still owns the first KVM-executed run.

## Tracey

No markers. NetPol enforcement is platform behavior (k8s NetworkPolicy + kube-router CNI), not rio-build spec — same posture as [P0241](plan-0241-vm-section-g-netpol.md). This plan fixes a lint error in the testScript; it doesn't change what the test verifies.

## Files

```json files
[
  {"path": "nix/tests/scenarios/netpol.nix", "action": "MODIFY", "note": "T1: 11× f-prefix strip on placeholder-free segments :133,:134,:138,:162-165,:185-186,:201-202"}
]
```

```
nix/tests/scenarios/
└── netpol.nix            # T1: 11 f-prefix strips (F541 fix)
```

## Dependencies

```json deps
{"deps": [241], "soft_deps": [311, 334], "note": "deps=[P0241 (netpol.nix exists — file landed 501d5a1e)]. soft_dep=[P0311-T15 tracks 'vm-netpol first KVM run' — this plan unblocks that (T15 would hit F541 otherwise); P0334 touches nix/tests/scenarios/*.nix (covTimeoutHeadroom hoist) — different region of netpol.nix (let-binding at :42), rebase-clean]. discovered_from=coordinator (F541 pattern-match from fix-p289 lesson + P0311-T15 'never built' manifest)."}
```

**Depends on:** [P0241](plan-0241-vm-section-g-netpol.md) — netpol.nix landed at [`501d5a1e`](https://github.com/search?q=501d5a1e&type=commits).

**Conflicts with:** [`netpol.nix`](../../nix/tests/scenarios/netpol.nix) — [P0334](plan-0334-covtimeoutheadroom-common-extract.md) touches `:42` (`covTimeoutHeadroom` let-binding, 9th copy); this plan touches `:133-202` (testScript body). Non-overlapping hunks. [P0311-T15](plan-0311-test-gap-batch-cli-recovery-dash.md) references netpol.nix as "NO CODE CHANGE — reminder" (doesn't edit). No other UNIMPL plan touches this file.

**CLAUSE-4 candidate:** T1 is comment-only-equivalent from a Rust-tier perspective — `nix/tests/scenarios/*.nix` only, no `rio-*/src/` touch. VM drv hash changes (netpol.nix content differs); Rust tier drvs byte-identical. `.#checks.x86_64-linux.nextest` standalone rc=0 = clean fast-path. OR: `.driverInteractive` build succeeds = direct proof the fix worked at the layer it targets.
