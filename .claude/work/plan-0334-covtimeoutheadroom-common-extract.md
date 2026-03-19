# Plan 0334: Hoist covTimeoutHeadroom into common.nix — 9 verbatim copies

Consolidator finding. Every VM scenario file that sets `globalTimeout` has the same let-binding: `covTimeoutHeadroom = if common.coverage then 300 else 0;`. Nine verbatim copies. [P0241](plan-0241-vm-section-g-netpol.md) added the 9th at [`netpol.nix:42`](../../nix/tests/scenarios/netpol.nix). The pattern is identical to `covMemBump` which **already lives** in [`common.nix:59`](../../nix/tests/common.nix) (`covMemBump = if coverage then 256 else 0;`). Same shape, same rationale (coverage-instrumented binaries are slower/larger), same fix.

Nine sites (all `scenarios/*.nix`):

| File | Line | globalTimeout usage |
|---|---|---|
| [`security.nix:61`](../../nix/tests/scenarios/security.nix) | 61 | `600 + covTimeoutHeadroom` |
| [`leader-election.nix:493`](../../nix/tests/scenarios/leader-election.nix) | 493 | `globalTimeout + covTimeoutHeadroom` |
| [`protocol.nix:39`](../../nix/tests/scenarios/protocol.nix) | 39 | `(if cold then 600 else 300) + covTimeoutHeadroom` |
| [`netpol.nix:42`](../../nix/tests/scenarios/netpol.nix) | 42 | `600 + covTimeoutHeadroom` — P0241's addition |
| [`lifecycle.nix:1645`](../../nix/tests/scenarios/lifecycle.nix) | 1645 | `globalTimeout + covTimeoutHeadroom` |
| [`fod-proxy.nix:70`](../../nix/tests/scenarios/fod-proxy.nix) | 70 | `900 + covTimeoutHeadroom` |
| [`scheduling.nix:1109`](../../nix/tests/scenarios/scheduling.nix) | 1109 | `globalTimeout + covTimeoutHeadroom` |
| [`observability.nix:51`](../../nix/tests/scenarios/observability.nix) | 51 | `600 + covTimeoutHeadroom` |
| [`cli.nix:40`](../../nix/tests/scenarios/cli.nix) | 40 | `600 + covTimeoutHeadroom` |

[`common.nix:72`](../../nix/tests/common.nix) already re-exports `coverage` with a comment explicitly explaining this use case: "Re-exported so scenario mkTest can gate globalTimeout headroom on coverage mode (instrumented images inflate k3s airgap import time)." The comment describes the pattern that every scenario re-derives locally. ~12-line net reduction (9 deletions, 1 addition, some whitespace).

**In-flight:** [P0273](plan-0273-envoy-sidecar-grpcweb-mtls.md) T4 touches [`cli.nix`](../../nix/tests/scenarios/cli.nix) (curl gate + `0x80` trailer grep) — that edit is in the test body, not the let-block at `:40`. Structurally independent.

## Tasks

### T1 — `refactor(nix):` add covTimeoutHeadroom to common.nix rec block

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) at `:59` — add one line after `covMemBump`:

```nix
  # Instrumented binaries are ~2× RSS; bump VM memory.
  covMemBump = if coverage then 256 else 0;
  # Instrumented binaries + k3s airgap-image re-import are slower;
  # pad globalTimeout. 300s covers the observed k3s-full cold-import
  # delta under coverage (~4min vs ~1.5min) with slack.
  covTimeoutHeadroom = if coverage then 300 else 0;
```

The `inherit coverage;` re-export at `:72` stays — some scenarios may still want to branch on `coverage` for other reasons. Its comment can shrink now that the headroom is pre-computed: keep the first sentence, drop "(instrumented images inflate k3s airgap import time)" since that's now the `covTimeoutHeadroom` comment.

**Placement note:** `covMemBump` is in the `let ... in` binding block **before** `rec {` (line 59 is pre-`rec`). `covTimeoutHeadroom` should go in the same place — it's then referenced inside the `rec` block the same way `covMemBump` is. BUT — scenarios reference it as `common.covTimeoutHeadroom`, so it must be EXPORTED. Check how `covMemBump` is surfaced: if it's used only inside common.nix, it's not exported and a different placement is needed. Grep `grep covMemBump nix/tests/` at dispatch — if scenarios use `common.covMemBump`, mirror that; if not, put `covTimeoutHeadroom` inside the `rec` block directly so it's a public attribute.

### T2 — `refactor(nix):` delete 9 let-bindings, reference common.covTimeoutHeadroom

MODIFY all 9 scenario files. For each: delete the `covTimeoutHeadroom = if common.coverage then 300 else 0;` let-binding line and change the usage to `common.covTimeoutHeadroom`.

**Two deletion shapes.** Some files have it as the ONLY let-binding in a `let ... in` — deleting it means deleting the whole `let ... in` wrapper:

```nix
# Before (if it's the only binding):
let
  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
mkTest {
  globalTimeout = 600 + covTimeoutHeadroom;

# After:
mkTest {
  globalTimeout = 600 + common.covTimeoutHeadroom;
```

Others have it alongside other bindings — just delete the one line:

```nix
# Before:
let
  someOtherBinding = ...;
  covTimeoutHeadroom = if common.coverage then 300 else 0;
in

# After:
let
  someOtherBinding = ...;
in
```

Apply to: security.nix:61, leader-election.nix:493, protocol.nix:39, netpol.nix:42, lifecycle.nix:1645, fod-proxy.nix:70, scheduling.nix:1109, observability.nix:51, cli.nix:40.

**Behavioral drv-identity:** this is a pure refactor — the evaluated `globalTimeout` integers are IDENTICAL before/after (same `if coverage then 300 else 0` expression, just referenced through `common.` instead of a local let). **But** the drv hashes change (Nix hashes source text). Clause-4(a) of the fast-path precedent applies: nix/tests-only delta, rust-tier untouched. Verify via `nix eval .#checks.x86_64-linux.vm-security-standalone.config.globalTimeout` (or whichever attr exposes it) before/after — same integer.

## Exit criteria

- `/nbr .#ci` green — OR clause-4(a) fast-path (nix/tests-only delta, rust-∩=∅)
- `grep 'covTimeoutHeadroom = if' nix/tests/scenarios/*.nix` → 0 hits (all 9 local bindings deleted)
- `grep 'common.covTimeoutHeadroom' nix/tests/scenarios/*.nix | wc -l` → ≥9 (every usage site migrated; some files use it multiple times)
- `grep 'covTimeoutHeadroom' nix/tests/common.nix` → ≥1 hit (exported binding exists)
- `nix eval --impure --expr 'let c = import ./nix/tests/common.nix { coverage = true; pkgs = import <nixpkgs> {}; rio-workspace = null; lib = (import <nixpkgs> {}).lib; }; in c.covTimeoutHeadroom'` → `300` (or equivalent eval proving the binding is exported and evaluates correctly — adjust the import args to whatever common.nix actually takes; this is the spirit of the check)
- `nix fmt` clean — deleting `let ... in` wrappers may leave nixfmt wanting to reflow

## Tracey

No new markers. Pure nix-test-infra duplication removal. No `r[...]` marker covers coverage-mode VM timeout padding — it's build tooling, not spec'd behavior.

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: add covTimeoutHeadroom binding after :59 covMemBump (check let-vs-rec placement); shrink :72 comment"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T2: delete :61 let-binding, :67 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "MODIFY", "note": "T2: delete :493, :503 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/protocol.nix", "action": "MODIFY", "note": "T2: delete :39, :253 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/netpol.nix", "action": "MODIFY", "note": "T2: delete :42, :49 → common.covTimeoutHeadroom (P0241's site)"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T2: delete :1645, :1656 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T2: delete :70, :78 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T2: delete :1109, :1120 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T2: delete :51, :57 → common.covTimeoutHeadroom"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T2: delete :40, :46 → common.covTimeoutHeadroom (structurally independent of P0273-T4's test-body edit)"}
]
```

```
nix/tests/
├── common.nix                    # T1: +covTimeoutHeadroom binding
└── scenarios/
    ├── security.nix              # T2: delete let, use common.
    ├── leader-election.nix       # T2
    ├── protocol.nix              # T2
    ├── netpol.nix                # T2
    ├── lifecycle.nix             # T2
    ├── fod-proxy.nix             # T2
    ├── scheduling.nix            # T2
    ├── observability.nix         # T2
    └── cli.nix                   # T2
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [273, 241, 304], "note": "No hard deps — common.nix and all 9 scenario files exist on sprint-1. Soft-dep P0273 (UNIMPL, touches cli.nix T4 curl-gate): P0273 edits the test BODY, T2 here edits the let-block at :40 — different regions of the file, textual-merge clean. Soft-dep P0241 (DONE — discovered_from): netpol.nix:42 is P0241's addition, the 9th copy that triggered the consolidator finding. Soft-dep P0304-T15 (UNIMPL): also touches common.nix (kvmCheck body at :159ish) — different section, T1 here is at :59. Wide file-fan but shallow edits: each scenario file touch is 2 lines (delete binding, adjust usage). High collision COUNT but zero collision DEPTH — sed-equivalent."}
```

**Depends on:** none. All sites exist.

**Conflicts with:** `nix/tests/common.nix` — [P0304](plan-0304-trivial-batch-p0222-harness.md) T15 edits the `kvmCheck` body at `:159+`; T1 here adds at `:59`. Non-overlapping. `nix/tests/scenarios/scheduling.nix` — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11 TAIL-appends; T2 here edits `:1109`. Non-overlapping. `nix/tests/scenarios/lifecycle.nix` — [P0304](plan-0304-trivial-batch-p0222-harness.md) T9 extracts `submit_build_grpc` helper; T2 here edits `:1645`. Likely non-overlapping (T9 is in the test body, not the tail let-block). `nix/tests/scenarios/cli.nix` — [P0273](plan-0273-envoy-sidecar-grpcweb-mtls.md) T4 adds curl-gate in test body; [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T1+T2 add populated-state assertions. T2 here is at `:40-46` let-block, others are test-body. All additive/different-region. The 9-file touch LOOKS high-collision but each edit is a 1-2 line sed — `prio=50` mid-tier is appropriate.
