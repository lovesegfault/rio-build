# Plan 0316: Force `-accel kvm` — close the concurrent-VM race

Third refinement in the TCG fast-fail chain. [P0313](plan-0313-kvm-fast-fail-preamble.md) added `os.open(O_RDWR)` preamble (caught 4/7 broken builders at ~4s). [P0315](plan-0315-kvm-ioctl-probe.md) added `ioctl(KVM_CREATE_VM)` probe (caught the remaining 3/7 — GET_API_VERSION proved insufficient; the LSM gate is at CREATE_VM, [`common.nix:114-126`](../../nix/tests/common.nix)). Both are **single-shot probes** that run once before `start_all()`.

**New failure mode discovered during P0315's `.#ci` iterations:** `vm-protocol-warm-standalone` spawns 3 QEMU processes concurrently (control + worker + client, standalone fixture). On a shared slurm host, the `kvmCheck` probe's single `KVM_CREATE_VM` succeeds — then 2/3 QEMU children hit `EACCES` on their own `KVM_CREATE_VM` seconds later. Same syscall, same uid, same sandbox. This is a resource/LSM race at the host level (likely cgroup device-allow quantum, or another slurm job on the same host consuming KVM slots between our probe and our VM boots). The single-shot probe **cannot** pre-allocate N VMs because it doesn't know N at preamble time — the fixture determines node count, and the preamble runs before fixture evaluation.

The QEMUs that lose the race fall back to TCG silently (NixOS `qemu-vm.nix` default: `-machine accel=kvm:tcg`). Fix: force `virtualisation.qemu.options = [ "-accel" "kvm" ]` — **no TCG fallback**. QEMU then hard-fails with `"failed to initialize kvm: Permission denied"` instead of silently degrading. Fast-fail per-VM, not per-preamble.

**Why this keeps happening (nixbuild.net frontend blind spot):** [P0315 impl-2 log lines 910-1574](../../.claude/known-flakes.jsonl) show all `vm-test-run-*` derivations carry `kvmEnabled=True` + slurm constraint `kvm:y`. The nixbuild.net frontend **believes** every builder has KVM. Actual `/dev/kvm` state varies (bind-mounted-but-LSM-gated, openable-but-ioctl-denied, race-loses-under-load). Scheduling never routes away because the frontend's flag is stale. This is external infra — the [known-flakes entry](../../.claude/known-flakes.jsonl) `vm-lifecycle-recovery-k3s` uses `fix_owner: "P0179"` as the sentinel for "nixbuild.net-side, not ours." A feedback mechanism (exit-77 results → frontend flag refresh) would require nixbuild.net API coordination; out of rio-build scope. The `-accel kvm` fix is the best **in-code** mitigation: it makes the QEMU failure unambiguous and fast regardless of why the frontend scheduled us onto a broken/contended host.

**Relationship to kvmCheck:** complementary, not a replacement. `kvmCheck` fires at ~4s before any VM boots — catches the "builder is broken outright" case with one clean exit-77. `-accel kvm` fires per-VM at QEMU init — catches "this specific VM lost a concurrent race." A builder that fails `kvmCheck` never reaches QEMU; a builder that passes `kvmCheck` but races under concurrent load hits the `-accel kvm` gate. Keep both.

## Entry criteria

- [P0315](plan-0315-kvm-ioctl-probe.md) merged — DONE at [`a6178a38`](https://github.com/search?q=a6178a38&type=commits). The `kvmCheck` attr at [`common.nix:142-175`](../../nix/tests/common.nix) is the context T1's comment references.

## Tasks

### T1 — `fix(nix):` kvmOnly shared module in common.nix

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) — add a shared NixOS module attr after `kvmCheck` at `:175`, before `postgresqlConfig` at `:179`:

```nix
  # Force KVM-only acceleration — no TCG fallback. Complements kvmCheck:
  # the preamble is a SINGLE-SHOT probe before start_all(); it cannot
  # cover the concurrent-VM race. vm-protocol-warm-standalone (3 VMs:
  # control+worker+client) on a shared slurm host: kvmCheck's single
  # KVM_CREATE_VM succeeds, then 2/3 QEMU children get EACCES on their
  # own KVM_CREATE_VM seconds later — same syscall/uid/sandbox, host-
  # level resource/LSM race (cgroup device-allow quantum, or another
  # slurm job consuming slots between probe and boot).
  #
  # NixOS qemu-vm.nix default: -machine accel=kvm:tcg (fall back to TCG
  # on KVM failure). With -accel kvm appended, QEMU hard-fails with
  # "failed to initialize kvm: Permission denied" instead of silently
  # degrading to TCG. Per-VM fail, not per-preamble.
  #
  # Verify flag syntax at dispatch:
  #   grep -r 'accel' /nix/store/*-nixos-test-driver-*/ | grep -i machine
  # or inspect the generated run-*-vm script. nixpkgs qemu-vm.nix
  # composes options via `lib.concatStringsSep " " cfg.qemu.options`
  # appended AFTER its own -machine — later -accel wins.
  kvmOnly = {
    virtualisation.qemu.options = [ "-accel" "kvm" ];
  };
```

**Placement rationale:** `kvmOnly` is a NixOS module (attrset with `virtualisation.*`); `kvmCheck` is a Python string. Grouping them next to each other keeps the TCG-mitigation surface in one place for future archaeology.

### T2 — `fix(nix):` import kvmOnly in every node constructor

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) — each node constructor's `virtualisation` block. Six sites per `grep -n virtualisation nix/tests/common.nix`:

| Site (approx) | Constructor | Edit |
|---|---|---|
| `:311` | mkGatewayNode (or similar — grep at dispatch) | add `imports = [ kvmOnly ];` inside the module, OR inline `qemu.options` into the existing `virtualisation = {...}` block |
| `:388` | mkWorkerNode (or similar) | same |
| `:452-453` | another node constructor | same |
| (+3 more) | grep `-n 'virtualisation'` at dispatch | same |

**Two equivalent placements — pick whichever threads cleaner:**

```nix
# Option A: imports at module top
{
  imports = [ kvmOnly ];
  virtualisation = { cores = 4; ... };
}

# Option B: inline into existing block (no imports dance)
virtualisation = {
  cores = 4;
  qemu.options = [ "-accel" "kvm" ];
  ...
};
```

Option B is more explicit (reader sees the flag at the `virtualisation` block) but duplicates 6×. Option A is DRY but requires the reader to chase `kvmOnly`. **Implementer's call** — consistency across all 6 sites matters more than which form.

MODIFY [`nix/tests/fixtures/k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) at `:264` and `:298` — same edit for the k3s-agent / k3s-server `virtualisation` blocks. `grep -n virtualisation nix/tests/fixtures/k3s-full.nix` at dispatch for exact lines (`:179` is a `virtualisation.fileSystems` override — skip, that's storage not QEMU args).

### T3 — `fix(harness):` known-flakes entry — concurrent-VM race + frontend-flag context

MODIFY [`.claude/known-flakes.jsonl`](../../.claude/known-flakes.jsonl) at line 11 — the `vm-lifecycle-recovery-k3s` TCG entry. The `fix_description` ends with `[P0315 LANDED a6178a38: ...]`. Append:

```
[P0316 LANDED <sha>: -accel kvm forces per-VM hard-fail — closes the concurrent-VM race (protocol-warm 3×QEMU: kvmCheck single-shot probe succeeds, 2/3 children race-lose on their own CREATE_VM). New symptom: QEMU "failed to initialize kvm: Permission denied" in VM serial log — distinct from exit-77 (that's preamble-caught). nixbuild.net frontend flag is STALE (all derivations show kvmEnabled=True + slurm kvm:y; actual /dev/kvm state varies) — scheduling never routes away. Feedback mechanism (exit-77 → frontend flag refresh) would need nixbuild.net API coordination; out of rio-build scope.]
```

The `fix_owner` stays `P0179` (external-infra sentinel). Use `onibus flake` subcommands if they support in-place edits; otherwise edit the JSONL line directly.

### T4 — `fix(harness):` teach `onibus flake excusable` the new QEMU failure marker

The [`onibus flake excusable`](../../.claude/lib/onibus/build.py) pattern-match (via [P0304](plan-0304-trivial-batch-p0222-harness.md) T10's `_TCG_MARKERS`) currently recognizes `KVM-DENIED-BUILDER` (kvmCheck's marker). After T2, QEMU emits its own failure message — likely `"failed to initialize kvm"` or `"could not initialize KVM"` (verify exact string at dispatch: `grep -i 'initialize.*kvm' /nix/store/*-qemu-*/` or reproduce on a no-KVM host).

MODIFY [`.claude/lib/onibus/build.py`](../../.claude/lib/onibus/build.py) — add the QEMU-native failure string to `_TCG_MARKERS`. This keeps retry-once behavior consistent: `kvmCheck` exit-77, QEMU `-accel kvm` hard-fail, both → retry (a KVM-good builder on retry passes).

**Verify at dispatch** that P0304 T10 has landed (it owns `_TCG_MARKERS` creation). If P0304 is still UNIMPL, add the QEMU marker to P0304's T10 spec instead of editing `build.py` here — leave a `TODO(P0316)` in P0304's T10 block.

## Exit criteria

- `/nbr .#ci` green — proves `-accel kvm` is a no-op on KVM-capable builders (all VMs boot normally; the flag is KVM's default when KVM works)
- `grep -c 'accel.*kvm\|kvm.*accel' nix/tests/common.nix` → ≥2 (T1: the kvmOnly module + comment mentions)
- `grep 'qemu.options\|kvmOnly' nix/tests/common.nix nix/tests/fixtures/k3s-full.nix` → ≥6 hits (T2: all node constructors covered; exact count depends on Option A vs B)
- On a KVM-good host, inspect the generated QEMU cmdline: `nix build .#checks.x86_64-linux.vm-protocol-warm-standalone.driverInteractive && grep -o '\-accel[^"]*' result/bin/nixos-test-driver-*` → contains `-accel kvm` (T2: flag reaches the QEMU invocation)
- `grep 'concurrent-VM race\|P0316' .claude/known-flakes.jsonl` → ≥1 hit (T3: entry updated)
- `grep -i 'failed to initialize kvm\|initialize KVM' .claude/lib/onibus/build.py` → ≥1 hit OR `grep 'TODO(P0316)' .claude/work/plan-0304-*.md` → ≥1 hit (T4: marker added OR forwarded to P0304)

## Tracey

No marker changes. VM test harness is not spec-covered — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:15`](../../.claude/lib/onibus/tracey.py). This is CI-robustness tooling, not product behavior. Same disposition as [P0313](plan-0313-kvm-fast-fail-preamble.md) and [P0315](plan-0315-kvm-ioctl-probe.md).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: kvmOnly shared module attr after kvmCheck :175; T2: import/inline in each virtualisation block (:311, :388, :452, +3 more per grep)"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T2: import kvmOnly / inline qemu.options in virtualisation blocks :264 :298 (skip :179 — that's fileSystems not QEMU args)"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T3: append P0316 LANDED clause to vm-lifecycle-recovery-k3s fix_description line 11; document frontend-flag staleness"},
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T4: add QEMU 'failed to initialize kvm' marker to _TCG_MARKERS — CONDITIONAL on P0304 T10 having landed; otherwise forward to P0304 T10 spec"}
]
```

```
nix/tests/
├── common.nix                    # T1: kvmOnly module  T2: 6× virtualisation blocks
└── fixtures/
    └── k3s-full.nix              # T2: 2× virtualisation blocks
.claude/known-flakes.jsonl        # T3: fix_description append
.claude/lib/onibus/build.py       # T4: _TCG_MARKERS (conditional)
```

## Dependencies

```json deps
{"deps": [315], "soft_deps": [304], "note": "P0315 provides kvmCheck context + CREATE_VM semantics the T1 comment references. Soft: P0304 T10 owns _TCG_MARKERS creation — T4 depends on it existing; if P0304 UNIMPL at dispatch, T4 forwards to P0304's spec instead. discovered_from=315 (P0315's .#ci iterations surfaced the concurrent-VM race). No scenario edits — T2 touches only common.nix node constructors + k3s-full.nix virtualisation blocks; scenario files unchanged."}
```

**Depends on:** [P0315](plan-0315-kvm-ioctl-probe.md) — merged (DONE at [`a6178a38`](https://github.com/search?q=a6178a38&type=commits)). `kvmCheck` at [`common.nix:142-175`](../../nix/tests/common.nix) is the T1 placement anchor; the comment explains why single-shot-probe → per-VM-gate is the next refinement.

**Soft-dep:** [P0304](plan-0304-trivial-batch-p0222-harness.md) T10 — `_TCG_MARKERS` in `onibus flake excusable`. If P0304 lands first, T4 edits `build.py` directly. If this lands first, T4 adds a `TODO(P0316)` to P0304's T10 block.

**Conflicts with:** [`common.nix`](../../nix/tests/common.nix) is NOT in the top-30 collision matrix. [P0314](plan-0314-mkbuildhelper-v2-consolidation.md) touches `:540` (mkBuildHelper deletion) and `:26` (doc-comment) — non-overlapping with `:175-179` (T1 kvmOnly attr) and the scattered `virtualisation` blocks (T2). [`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) also touched by [P0304](plan-0304-trivial-batch-p0222-harness.md) T-bounceGatewayForSecret at `:486-524` — non-overlapping with `:264`/`:298` (T2 virtualisation blocks). [`known-flakes.jsonl`](../../.claude/known-flakes.jsonl) line 11 append-only — no conflict.

**Chain:** P0313 (`O_RDWR` — catches 4/7) → P0315 (`KVM_CREATE_VM` — catches 7/7 single-shot) → P0316 (`-accel kvm` — catches concurrent-VM race per-VM). Each refinement caught a gap the previous one couldn't structurally close.
