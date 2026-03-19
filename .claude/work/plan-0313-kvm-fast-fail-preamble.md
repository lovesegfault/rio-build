# Plan 0313: KVM fast-fail preamble for VM tests

When nixbuild.net allocates a TCG builder (no `/dev/kvm`), VM tests run under QEMU software emulation — 10-20× slower. The k3s-full fixture's 4-minute `waitReady` becomes 40+ minutes and hits `globalTimeout`; the test driver kills the VM and the log shows only a timeout, not the root cause. [fix-p209](plan-0209-fuse-circuit-breaker-store-degraded.md) hit this three times during debug cycles (see the coordinator followup: "10s fail instead of 15min timeout").

The check is trivial: `/dev/kvm` readability on the **build host** (where the Python testScript runs, controlling the QEMU VMs). If absent, exit 77 with a distinctive marker before `start_all()` — the VM never boots. The known-flakes entry for TCG allocation already exists; this makes the symptom instant instead of 15-minute-delayed.

Both fixtures ([`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) and [`standalone.nix`](../../nix/tests/fixtures/standalone.nix)) export testScript snippets that scenarios splice into their `prelude` — [`lifecycle.nix:229-233`](../../nix/tests/scenarios/lifecycle.nix) shows the pattern: `${common.assertions}` → `start_all()` → `${fixture.waitReady}`. The KVM check is a new `common.kvmCheck` Nix-string attr, prepended before `start_all()` in every scenario that drives VMs.

## Tasks

### T1 — `feat(nix):` kvmCheck attr in common.nix

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) — add alongside `assertions` at `:97`:

```nix
  # Fast-fail when the build host lacks /dev/kvm. nixbuild.net
  # occasionally allocates a TCG builder (no KVM); under TCG,
  # k3s-full's ~4min waitReady balloons to 40+min and hits
  # globalTimeout. Prepend this BEFORE start_all() — the check
  # runs on the build host (test driver), not inside the VM.
  # Exit 77 is the automake "skip" convention; the distinctive
  # marker lets `onibus build excusable` pattern-match it.
  kvmCheck = ''
    import os, sys
    if not os.access("/dev/kvm", os.R_OK):
        print(
            "KVM-DENIED-BUILDER: /dev/kvm not readable — "
            "builder allocated without KVM; TCG would 10-20x the runtime",
            file=sys.stderr,
        )
        sys.exit(77)
  '';
```

Placement: after `assertions = builtins.readFile ./lib/assertions.py;` at `:97`, before the node constructor functions.

### T2 — `feat(nix):` prepend kvmCheck in scenario preludes

MODIFY each scenario's prelude to splice `${common.kvmCheck}` **before** `start_all()`:

| File | Line (approx) | Current | After |
|---|---|---|---|
| [`lifecycle.nix:229`](../../nix/tests/scenarios/lifecycle.nix) | prelude start | `''${common.assertions} start_all()` | `''${common.kvmCheck} ${common.assertions} start_all()` |
| [`scheduling.nix:86`](../../nix/tests/scenarios/scheduling.nix) | prelude start | (same pattern) | (same fix) |
| [`leader-election.nix:54`](../../nix/tests/scenarios/leader-election.nix) | prelude start | (same pattern) | (same fix) |
| [`observability.nix:61`](../../nix/tests/scenarios/observability.nix) | testScript start | (inline testScript, no prelude var) | (prepend at top) |
| [`security.nix`](../../nix/tests/scenarios/security.nix) | testScript start | (grep at dispatch) | (prepend at top) |
| [`protocol.nix:257`](../../nix/tests/scenarios/protocol.nix) | testScript start | (inline) | (prepend at top) |
| [`cli.nix:49`](../../nix/tests/scenarios/cli.nix) | testScript start | (inline) | (prepend at top) |

**Discovery at dispatch:** `grep -n 'start_all()\|testScript = ' nix/tests/scenarios/*.nix` to find the exact insertion point for each. The check goes immediately before `start_all()` (after `${common.assertions}` if present, since assertions.py is pure `def` statements).

**fod-proxy.nix (p243):** If P0243 merges before this plan, add it there too. If this plan lands first, leave a `TODO(P0313)` comment in [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) via the P0243 reviewer — or the serialization with P0243 (see Dependencies) handles it naturally.

### T3 — `test(nix):` negative-case validation

The fast path (KVM present) is covered by every existing VM test — `.#ci` green proves `kvmCheck` doesn't false-positive. The **negative** path (KVM absent → exit 77) is not testable in CI (we don't control builder allocation).

**Local validation only:** In a devshell on a machine without KVM (or with `/dev/kvm` chmod'd 000 temporarily):
```bash
nix build .#checks.x86_64-linux.vm-lifecycle-core 2>&1 | grep 'KVM-DENIED-BUILDER'
# exit code should be 77 (propagated through the test driver)
```

Document this as a **manual verification note** in the T1 comment block. No automated negative test.

## Exit criteria

- `/nbr .#ci` green (proves `kvmCheck` is a no-op when `/dev/kvm` exists)
- `grep -c 'common.kvmCheck\|kvmCheck}' nix/tests/scenarios/*.nix` ≥ 7 (all scenarios covered)
- `grep 'KVM-DENIED-BUILDER' nix/tests/common.nix` → ≥1 hit (the marker string is present)
- `python3 -c 'compile(open("nix/tests/common.nix").read().split("kvmCheck = ")[1].split("'"'"''"'"'")[1], "<kvmCheck>", "exec")'` — or simpler: extract the `kvmCheck` string and `python3 -c` it on a host WITH `/dev/kvm` → silent exit 0 (syntax valid, check passes)

## Tracey

No marker changes. VM test harness is not spec-covered — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:16`](../../.claude/lib/onibus/tracey.py). This is CI-robustness tooling, not product behavior.

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: kvmCheck Nix-string attr after assertions :97"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T2: prepend ${common.kvmCheck} in prelude before start_all() :229-232"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck in prelude :86-"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck in prelude :54-"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck at testScript top :61-"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck at testScript top"},
  {"path": "nix/tests/scenarios/protocol.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck at testScript top :257-"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T2: prepend kvmCheck at testScript top :49-"}
]
```

```
nix/tests/
├── common.nix                    # T1: kvmCheck attr (Nix string → Python)
└── scenarios/
    ├── lifecycle.nix             # T2: ${common.kvmCheck} before start_all()
    ├── scheduling.nix            # T2: same
    ├── leader-election.nix       # T2: same
    ├── observability.nix         # T2: same
    ├── security.nix              # T2: same
    ├── protocol.nix              # T2: same
    └── cli.nix                   # T2: same
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [243], "note": "No hard deps — additive Nix-string attr + one-line prepends. Soft: P0243 adds fod-proxy.nix which also needs the prepend; if P0243 merges first, add it here; if this lands first, P0243's implementer adds the one line. lifecycle.nix count=14 (highest scenario collision) but this is a one-line prepend at prelude top — trivial rebase."}
```

**Depends on:** none. Pure additive.

**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=14 — touched by [P0206](plan-0206-path-tenants-migration-upsert-completion.md), [P0215](plan-0215-worker-max-silent-time.md), [P0216](plan-0216-rio-cli-subcommands.md), [P0304](plan-0304-trivial-batch-p0222-harness.md) T9. But T2 is a **one-line prepend at a stable location** (prelude top, right before `start_all()`) — even if surrounding lines shift, the insertion point is unambiguous. [P0314](plan-0314-mkbuildhelper-v2-consolidation.md) rewrites the `build()` helpers lower in these files (lifecycle:339, scheduling:101) — non-overlapping sections.

**Related:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T10 (this batch run) teaches `onibus build excusable()` to recognize the `KVM-DENIED-BUILDER` marker. The two plans are independent but complementary — T10 there pattern-matches the marker T1 here emits.
