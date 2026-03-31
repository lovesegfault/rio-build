# Plan 993659603: cpuHints -k3s suffix fallthrough — close the missing-entry drift direction

Consolidator finding: two catchup-fix commits in six days ([`d6f74e27`](https://github.com/search?q=d6f74e27&type=commits) "add cpuHints for ca-cutoff/chaos/cli/netpol", [`fa55ef13`](https://github.com/search?q=fa55ef13&type=commits) via [P0519](plan-0519-prod-parity-coverage-exclude.md) "add vm-lifecycle-prod-parity-k3s") where newly-added `-k3s` VM tests fell through [`cpuHints.${name} or 4` at flake.nix:1209](../../flake.nix) to the default `4`, when every `-k3s` test needs `8` (k3s-server is 8-core 6GB + k3s-agent 8-core 4GB, per the table comments at `:1180-1205`). Census: 13 of the 15 table entries at `:1162-1206` are `= 8`, all with the `-k3s` suffix. The two non-8 groups are `-standalone` tests at `3` or `5`.

Not load-bearing today: `cpuFloor=64` at the outer clamp ensures every VM test gets at least 64 cores regardless of cpuHints. But cpuHints feeds the ×4 formula at `:1126` ("cpuHints is still consulted for the ×4 formula"), and [`:1114`](../../flake.nix)'s comment warns about the failure mode it guards ("vm-scheduling-core once got 5 CPUs for 4 VMs → 16 vCPUs on 5 → TCG fallback → qemu stall"). The `or 4` default on a test that needs 8 is a latent wrong-answer — benign under the current floor, wrong if the floor is ever lowered or the formula changes.

More to the point: the next VM-test plan WILL forget the cpuHints entry again. Both catchup commits were reactive ("oh, I added a test but it's not in the table").

Complementary to [P0304-T539](plan-0304-trivial-batch-p0222-harness.md), which catches the OPPOSITE drift direction (`cpuHints` has a key for a deleted test — dead entry, silent no-op). T539's `deadHints` assert guards key-exists-test-doesn't; this plan's suffix fallthrough guards test-exists-key-doesn't (for the `-k3s` subset where the right answer is structurally knowable). Together they close the drift class in both directions.

## Tasks

### T1 — `feat(nix):` cpuHints suffix-aware fallthrough

At [`flake.nix:1209`](../../flake.nix), change the `or` default:

```nix
# before
pkgs.lib.mapAttrs (name: withMinCpu (cpuHints.${name} or 4)) allTests

# after
pkgs.lib.mapAttrs (
  name:
  withMinCpu (
    cpuHints.${name}
      # k3s fixture: 2-node cluster, k3s-server 8-core + k3s-agent
      # 8-core. Every -k3s test in the table is 8; encode that as the
      # suffix default so new -k3s tests don't fall through to 4.
      # Catchup-fix precedent: d6f74e27 + fa55ef13 both added
      # forgotten -k3s entries. T539 catches the OTHER direction
      # (dead entries for deleted tests).
      or (if pkgs.lib.hasSuffix "-k3s" name then 8 else 4)
  )
) allTests
```

**Optional hardening — worth it if T539's deadHints assert landed:** once the suffix default is in, the `-k3s = 8` explicit entries become redundant (they all equal the suffix default). KEEP them — explicit is better than implicit, and the table is documentation ("3 VMs but k3s-server is 8-core 6GB..."). But if someone later deletes them thinking "redundant", the suffix fallthrough means behavior doesn't change. Belt-and-suspenders.

**Verify:** temporarily delete `vm-lifecycle-core-k3s = 8;` from the table → `nix eval --impure --expr '(import ./flake.nix).outputs' 2>&1` still resolves `vm-lifecycle-core-k3s` to 8 via suffix. Temporarily delete `vm-protocol-warm-standalone = 3;` → resolves to 4 (non-k3s default, WRONG for that test but proves the branch works; restore the entry).

## Exit criteria

- `grep 'hasSuffix.*-k3s' flake.nix` → ≥1 hit at `:~1209`
- `nix eval .#checks.x86_64-linux --apply builtins.attrNames --json` succeeds (no eval error from the expression change)
- Mutation: delete any `-k3s = 8;` entry from cpuHints → `nix eval` still succeeds, test's cpuHint still resolves to 8 (via suffix fallthrough)
- `/nixbuild .#ci` green (no VM-test allocation change — every test either has an explicit entry OR falls through to the same value it would have before)

## Tracey

No spec markers. Build-infrastructure resource hints — no `r[impl]` / `r[verify]`.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: :1209 or-4 → or-(if hasSuffix -k3s then 8 else 4). Single-expression change inside mapAttrs."}
]
```

```
flake.nix                # T1: cpuHints suffix fallthrough :1209
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [304, 519], "note": "1-expression change, no hard deps. Soft-dep P0304-T539 (complementary — dead-entry direction; this plan is missing-entry direction; together close the drift class). Soft-dep P0519 (provenance — fa55ef13 was its catchup fix; discovered_from=consolidator)."}
```

**Depends on:** none.
**Soft-dep:** [P0304-T539](plan-0304-trivial-batch-p0222-harness.md) — complementary. T539 = `assert deadHints == []` catches entries for deleted tests; T1 here = `hasSuffix "-k3s" → 8` catches the default for new `-k3s` tests. Ship either order. [P0519](plan-0519-prod-parity-coverage-exclude.md) — provenance ref (DONE).
**Conflicts with:** `flake.nix` count=48 (HOT). `:1209` is inside the `let cpuHints = ... in mapAttrs` block (`:1162-1210`). [P0304-T538](plan-0304-trivial-batch-p0222-harness.md) touches `:1209-1224` (overlapping — T538 rewrites the vmTestsCov `removeAttrs` block with a let-assert, which is AFTER the mapAttrs; re-check line numbers at dispatch, likely `:~1215+` post-T538). [P0304-T539](plan-0304-trivial-batch-p0222-harness.md) deletes `:1190` (`vm-fod-proxy-k3s`) + optionally adds deadHints assert near `:1207` — adjacent but non-overlapping hunks with T1's `:1209` expression edit. [P993659601](plan-993659601-onibus-pytest-ci-gate.md) touches `:556+` (miscChecks) + `:1537-1543` (matrix inherit) — 300+/700+ lines away, clean rebase.
