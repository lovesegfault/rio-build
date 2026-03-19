# Plan 0315: KVM ioctl probe — close the O_RDWR gap

[P0313](plan-0313-kvm-fast-fail-preamble.md) landed a fast-fail preamble at [`common.nix:126-138`](../../nix/tests/common.nix): `os.open("/dev/kvm", O_RDWR)` before `start_all()`, exit 77 with `KVM-DENIED-BUILDER` marker on failure. The merger's own two `.#ci` iterations proved it catches **4 of 7** TCG builders at ~4s — but 3/7 (protocol-warm, fod-proxy, observability in merge-37/38) **pass the O_RDWR check yet still fall back to TCG**.

Those 3 builders have `/dev/kvm` bind-mounted and openable-for-write (same nspawn config: `--bind=/dev/kvm`, `--private-users=identity`), but QEMU's next step after `open()` — the `KVM_CREATE_VM` ioctl — fails. Host-side `/dev/kvm` state varies per builder (different kernel module load state, different cgroup device-allow, different user-namespace mapping for the ioctl path). `O_RDWR` openability is **necessary but not sufficient** for KVM; QEMU's actual probe is an ioctl.

The stronger check: `ioctl(fd, KVM_GET_API_VERSION)` — a read-only probe (no VM created, no resources allocated) that returns the KVM API version integer. This is exactly what QEMU does first ([qemu `kvm_init()`](https://gitlab.com/qemu-project/qemu/-/blob/master/accel/kvm/kvm-all.c) calls `KVM_GET_API_VERSION` before `KVM_CREATE_VM`). If this ioctl fails, `KVM_CREATE_VM` will also fail. Python's `fcntl.ioctl()` is in stdlib; `KVM_GET_API_VERSION` is `_IO(0xAE, 0x00)` = `0xAE00` (no size/direction encoding — it's a bare `_IO` macro).

The [known-flakes entry for `vm-lifecycle-recovery-k3s`](../../.claude/known-flakes.jsonl) already tracks this gap: "[P0313 LANDED 5bbb5469: preamble fires exit-77 + KVM-DENIED-BUILDER at ~4s (catches 4/7 O_RDWR-deniable); ioctl-gap remains 3/7.]"

## Entry criteria

- [P0313](plan-0313-kvm-fast-fail-preamble.md) merged — DONE at [`5bbb5469`](https://github.com/search?q=5bbb5469&type=commits). The `kvmCheck` attr exists at [`common.nix:126`](../../nix/tests/common.nix); scenario preludes already splice it.

## Tasks

### T1 — `fix(nix):` kvmCheck ioctl probe after O_RDWR open

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) at `:126-138` — the existing `kvmCheck` string. Keep the `O_RDWR` open (cheap, catches 4/7, distinguishes ENOENT from EACCES). Add `ioctl(fd, KVM_GET_API_VERSION)` **before** `os.close()`:

```nix
  kvmCheck = ''
    import os, sys, fcntl
    _KVM_GET_API_VERSION = 0xAE00  # _IO(KVMIO, 0x00); KVMIO=0xAE
    try:
        _kvm_fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
    except OSError as _kvm_err:
        print(
            f"KVM-DENIED-BUILDER: cannot open /dev/kvm O_RDWR ({_kvm_err.strerror}) — "
            "builder allocated without KVM; TCG would 10-20x the runtime",
            file=sys.stderr, flush=True,
        )
        sys.exit(77)
    try:
        _kvm_ver = fcntl.ioctl(_kvm_fd, _KVM_GET_API_VERSION)
    except OSError as _kvm_err:
        os.close(_kvm_fd)
        print(
            f"KVM-DENIED-BUILDER: /dev/kvm opened O_RDWR but "
            f"ioctl(KVM_GET_API_VERSION) failed ({_kvm_err.strerror}) — "
            "device node present but KVM unusable; 3/7 ioctl-gap builders "
            "(P0313 caught the other 4/7 at open())",
            file=sys.stderr, flush=True,
        )
        sys.exit(77)
    os.close(_kvm_fd)
    # _kvm_ver is typically 12 (KVM_API_VERSION since linux 2.6.22).
    # Don't assert on the value — any successful ioctl return means
    # the kernel's KVM subsystem answered.
  '';
```

**Two distinct messages** so the log distinguishes the 4/7 (open fails) from the 3/7 (ioctl fails). Both emit `KVM-DENIED-BUILDER` so [`onibus flake excusable`](../../.claude/lib/onibus/build.py) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T10's `_TCG_MARKERS` — still pattern-matches. [CORRECTION P0317: excusable() did NOT grep markers at this plan's merge — _NEXTEST_FAIL_RE only. P0317 T1 added _VM_FAIL_RE; P0304 T10 adds _TCG_MARKERS supplementary grant.]

**Net delta:** ~15 lines. The `import fcntl` is new. `_kvm_ver` is captured only so a debugger can inspect it; the value is not asserted (any successful return = KVM reachable).

### T2 — `docs:` update kvmCheck comment block

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) at `:106-125` — the comment block above `kvmCheck`. The current text says "The check opens /dev/kvm O_RDWR (what QEMU actually does)" — correct but incomplete. Append after `:112`:

```nix
  #
  # After the open, also probe ioctl(KVM_GET_API_VERSION) — QEMU's
  # actual first ioctl (accel/kvm/kvm-all.c kvm_init). Merge-37/38
  # observed 3/7 TCG builders where O_RDWR open succeeds but this
  # ioctl fails: /dev/kvm bind-mounted (nspawn --bind=/dev/kvm),
  # openable, but host-side KVM state unusable (module load state,
  # cgroup device-allow on the ioctl path, userns mapping). O_RDWR
  # is necessary-not-sufficient. The ioctl catches all 7.
```

Update the "Manual negative-path verification" block at `:120-125` to mention the ioctl path too:

```nix
  # Manual negative-path verification (not CI-testable — we don't
  # control builder allocation):
  #   On a host without /dev/kvm:
  #     nix build .#checks.x86_64-linux.vm-lifecycle-core 2>&1 \
  #       | grep 'KVM-DENIED-BUILDER: cannot open'
  #   On a host with /dev/kvm but KVM module unloaded (sudo rmmod kvm_intel):
  #     ... | grep 'KVM-DENIED-BUILDER:.*ioctl.*failed'
  # Both → exit 77 propagated through the driver.
```

### T3 — `fix(harness):` update known-flakes ioctl-gap note

MODIFY [`.claude/known-flakes.jsonl`](../../.claude/known-flakes.jsonl) — the `vm-lifecycle-recovery-k3s` entry's `fix_description` currently ends with "[P0313 LANDED 5bbb5469: ... ioctl-gap remains 3/7.]". After this plan lands, append:

```
[P0315 LANDED <sha>: ioctl(KVM_GET_API_VERSION) probe closes the 3/7 gap. New symptom remains: builder failed with exit code 77.]
```

Use `onibus flake` subcommands if they support in-place JSON edits; otherwise edit the line directly. The `fix_owner` field stays `P0179` (the entry tracks the nixbuild.net-side infra issue; P0313 and this plan are bridges).

## Exit criteria

- `/nbr .#ci` green — proves the ioctl probe is a no-op on KVM-capable builders (the 3/7 that previously slipped through would now exit-77; on a KVM-good builder the ioctl succeeds silently)
- `grep 'fcntl.ioctl' nix/tests/common.nix` → ≥1 hit (T1: ioctl call present)
- `grep '0xAE00\|KVM_GET_API_VERSION' nix/tests/common.nix` → ≥2 hits (T1: constant defined + comment)
- `grep -c 'KVM-DENIED-BUILDER' nix/tests/common.nix` → ≥2 (T1: two distinct failure messages, both carry the marker)
- `python3 -c 'import fcntl; fd=__import__("os").open("/dev/kvm",2); print(fcntl.ioctl(fd, 0xAE00))'` on a KVM-good host → prints `12` and exits 0 (T1: constant is correct; KVM_API_VERSION has been 12 since linux 2.6.22)
- `grep 'ioctl-gap\|P0315' .claude/known-flakes.jsonl` → ≥1 hit (T3: entry updated)

## Tracey

No marker changes. VM test harness is not spec-covered — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:15`](../../.claude/lib/onibus/tracey.py). This is CI-robustness tooling, not product behavior. Same disposition as [P0313](plan-0313-kvm-fast-fail-preamble.md).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: add fcntl.ioctl(fd, 0xAE00) probe inside kvmCheck :126-138; T2: extend comment block :106-125 with ioctl rationale + rmmod negative-path verification"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T3: update vm-lifecycle-recovery-k3s fix_description — ioctl-gap closed"}
]
```

```
nix/tests/
└── common.nix              # T1: ioctl probe (~15 lines)  T2: comment update
.claude/known-flakes.jsonl  # T3: fix_description append
```

## Dependencies

```json deps
{"deps": [313], "soft_deps": [304], "note": "P0313 provides the kvmCheck attr + scenario splices — this plan extends the attr in place, scenarios unchanged. Soft: P0304 T10 teaches onibus flake excusable() to match KVM-DENIED-BUILDER; both T1 messages here keep that marker so T10's pattern still matches. No new scenario edits — the splice points already carry ${common.kvmCheck}."}
```

**Depends on:** [P0313](plan-0313-kvm-fast-fail-preamble.md) — merged (DONE). The `kvmCheck` attr at [`common.nix:126`](../../nix/tests/common.nix) is the edit target; all 7+ scenario preludes already splice it (P0313 T2), so T1 here is a single-file change.

**Conflicts with:** [`common.nix`](../../nix/tests/common.nix) is NOT in the top-30 collision matrix. [P0314](plan-0314-mkbuildhelper-v2-consolidation.md) T1/T2 touches `common.nix` at `:540` (mkBuildHelper deletion) and `:26` (doc-comment) — non-overlapping sections (`kvmCheck` is `:97-138`). Trivial merge.

**Net:** 4/7 → 7/7 catch rate. The 3/7 builders that previously slipped through P0313's open-check and hit the 15-min `globalTimeout` now exit-77 at ~4s like the others.
