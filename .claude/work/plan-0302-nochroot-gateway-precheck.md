# Plan 0302: `__noChroot` gateway precheck — annotate + verify (impl already landed)

Originates from [`verification.md:68`](../../docs/src/verification.md) §F security-test breakdown. **Scope changed from the mapping doc:** the check is **already implemented** at both checkpoints the spec describes. The gap is annotation + verification, not the feature.

The spec marker `r[gw.reject.nochroot]` at [`gateway.md:465`](../../docs/src/components/gateway.md) describes two rejection points:
1. `validate_dag` checks every node's drv_cache entry → **implemented at [`translate.rs:287-340`](../../rio-gateway/src/translate.rs)**
2. `wopBuildDerivation` checks the inline BasicDerivation's env directly → **implemented at [`handler/build.rs:522-542`](../../rio-gateway/src/handler/build.rs)**

`tracey query rule gw.reject.nochroot` shows **zero** `r[impl]` and **zero** `r[verify]` annotations. The comment at [`translate.rs:646`](../../rio-gateway/src/translate.rs) explains why there's no unit test: "`__noChroot` rejection is hard to unit-test here because it needs a Derivation in drv_cache with `__noChroot=1` in env" — the drv_cache is populated by BFS over a real store, not constructible in isolation.

So the ~10 LoC estimate from the mapping doc becomes: 2 annotation lines + 1 wire-level test for the BasicDerivation path (which IS unit-testable — `wopBuildDerivation` reads a BasicDerivation off the wire, env included).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (phase4b fan-out root)

## Tasks

### T1 — `fix(gateway):` `r[impl gw.reject.nochroot]` annotations at both sites

MODIFY [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs) — add annotation above `validate_dag` at `:287`:

```rust
// r[impl gw.reject.nochroot]
/// Validate a DAG before SubmitBuild. Returns `Err(reason)` if the
/// DAG should be rejected — caller sends STDERR_ERROR with the
/// reason. ...
pub fn validate_dag(...)
```

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) — add annotation above the inline check at `:522`:

```rust
// r[impl gw.reject.nochroot]
// Check __noChroot on the BasicDerivation DIRECTLY. validate_dag
// (called below) checks drv_cache entries, but if the full drv
// ...
```

Two `r[impl]` annotations for one marker is valid per tracey — both sites enforce the same requirement. `tracey query rule gw.reject.nochroot` will show both.

### T2 — `test(gateway):` wire-level `wopBuildDerivation` `__noChroot` rejection

NEW test in [`rio-gateway/tests/wire_opcodes/build.rs`](../../rio-gateway/tests/wire_opcodes/build.rs). This is the tractable path: `wopBuildDerivation` reads a `BasicDerivation` straight off the wire (u64 opcode + drv_path string + BasicDerivation fields), and `BasicDerivation` carries the env map. Construct raw bytes with `__noChroot=1` in env, assert `STDERR_ERROR` comes back:

```rust
// r[verify gw.reject.nochroot]
#[tokio::test]
async fn wop_build_derivation_rejects_nochroot() {
    let mut harness = TestHarness::new().await;

    // Construct BasicDerivation wire bytes with __noChroot=1 in env.
    // BasicDerivation wire format (rio-nix/src/derivation.rs
    // read_basic_derivation):
    //   outputs: count-prefixed (name, path, hashAlgo, hash) tuples
    //   inputSrcs: string list
    //   platform: string
    //   builder: string
    //   args: string list
    //   env: count-prefixed (key, value) string pairs
    let mut buf = Vec::new();
    write_u64(&mut buf, OP_BUILD_DERIVATION);
    write_string(&mut buf, "/nix/store/00000000000000000000000000000000-evil.drv");
    // BasicDerivation:
    write_u64(&mut buf, 1);  // 1 output
    write_string(&mut buf, "out");
    write_string(&mut buf, "/nix/store/11111111111111111111111111111111-evil");
    write_string(&mut buf, "");  // hashAlgo (input-addressed)
    write_string(&mut buf, "");  // hash
    write_u64(&mut buf, 0);      // 0 inputSrcs
    write_string(&mut buf, "x86_64-linux");
    write_string(&mut buf, "/bin/sh");
    write_u64(&mut buf, 0);      // 0 args
    write_u64(&mut buf, 1);      // 1 env pair
    write_string(&mut buf, "__noChroot");
    write_string(&mut buf, "1");
    write_u64(&mut buf, 0);      // buildMode (Normal)

    harness.send_raw(&buf).await;
    let resp = harness.read_stderr_message().await;

    // STDERR_ERROR with "sandbox escape" in the message.
    assert!(matches!(resp, StderrMsg::Error { msg, .. } if msg.contains("sandbox escape")));
}
```

The `validate_dag` path (drv_cache lookup) stays untested at the wire level per the `:646` comment — the VM scenario in T3 covers it.

### T3 — `test(vm):` full-DAG `__noChroot` rejection via real store

New subtest in [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) (or wherever security VM scenarios live — `grep -l STDERR_ERROR nix/tests/scenarios/*.nix` to find the right home). Col-0 marker before `{` per tracey `.nix` parser rule:

```nix
# r[verify gw.reject.nochroot]
# Scenario: submit a .drv with __noChroot=1 in env via nix build
# against the rio gateway. Asserts STDERR_ERROR with "sandbox escape"
# comes back and the build is NOT forwarded to the scheduler
# (rio_scheduler_builds_submitted_total stays 0).
```

Scenario script: write a `.drv` with `__noChroot = "1"` in env (via `builtins.derivation { __noChroot = true; ... }` in a `.nix` expr, or hand-crafted ATerm), submit via `nix build --store ssh-ng://gateway`, assert the nix client sees an error AND the scheduler metric is unchanged. This exercises the `validate_dag` path (full DAG, drv in cache via `wopAddToStoreNar` → `wopBuildPathsWithResults`).

## Exit criteria

- `tracey query rule gw.reject.nochroot` shows 2× `impl` (translate.rs + handler/build.rs) and 2× `verify` (wire_opcodes/build.rs + security.nix)
- Wire-level test: `wopBuildDerivation` with `__noChroot=1` in inline BasicDerivation env → `STDERR_ERROR` with "sandbox escape"
- VM test: real `.drv` with `__noChroot=1` via `nix build` → error at gateway, `rio_scheduler_builds_submitted_total` unchanged

## Tracey

References existing markers:
- `r[gw.reject.nochroot]` — T1 adds 2× `r[impl]` (currently **uncovered**); T2+T3 add 2× `r[verify]` (currently **untested**)

No new markers — the spec already describes exactly what the existing code does.

## Files

```json files
[
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T1: r[impl gw.reject.nochroot] annotation above validate_dag"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T1: r[impl gw.reject.nochroot] annotation above inline check"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "T2: wire-level wopBuildDerivation nochroot test"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T3: VM scenario via nix build + metric assert"}
]
```

```
rio-gateway/src/
├── translate.rs         # T1: annotation
└── handler/build.rs     # T1: annotation
rio-gateway/tests/wire_opcodes/
└── build.rs             # T2: wire-level test
nix/tests/scenarios/
└── security.nix         # T3: VM scenario
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "phase-cleanup: verification.md:68 user-decided deferral. SCOPE CHANGED from mapping doc: impl already exists at both spec checkpoints (translate.rs:287, handler/build.rs:522). Gap is 2 r[impl] annotations + 1 wire test + 1 VM test. Marker r[gw.reject.nochroot] exists but shows 0 impl / 0 verify in tracey."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — phase4b fan-out root.

**Conflicts with:**
- [`translate.rs`](../../rio-gateway/src/translate.rs) count=18, UNIMPL=[226, 250]. T1 is a one-line comment insert at `:287`. Whatever P0226/P0250 do, a comment insert composes.
- [`handler/build.rs`](../../rio-gateway/src/handler/build.rs) — check collisions at dispatch. T1 is a one-line comment insert at `:522`.
- [`wire_opcodes/build.rs`](../../rio-gateway/tests/wire_opcodes/build.rs) — test file, EOF-append. Low contention.
- [`security.nix`](../../nix/tests/scenarios/security.nix) — if the file doesn't exist yet, NEW. P0242 (VM section I security) may create it — if so, this appends a subtest. Soft ordering: if P0242 is unmerged at dispatch, coordinate.
