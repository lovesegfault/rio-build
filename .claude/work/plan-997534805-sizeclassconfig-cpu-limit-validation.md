# Plan 997534805: SizeClassConfig.cpu_limit_cores — NaN/neg validation

bughunt-mc238 correctness. [`rio-scheduler/src/assignment.rs:68`](../../rio-scheduler/src/assignment.rs) defines `SizeClassConfig.cpu_limit_cores: Option<f64>` — operator-settable via `scheduler.toml` `[[size_classes]]`. Consumer at [`:128`](../../rio-scheduler/src/assignment.rs) does `c > limit`. No `validate_config` check:

| Input | `c > limit` | Effect |
|---|---|---|
| `limit = nan` | always `false` | CPU-bump silently disabled — builds never bump even when CPU-bound |
| `limit < 0` | always `true` | every build bumps to next class — misroutes entire small-class queue |
| `limit = 0.0` | always `true` (unless `c == 0`) | same misroute; semantically degenerate |

Same failure class as [P0415](plan-0415-retrypolicy-backoff-f64-bounds-complete.md)'s backoff_multiplier NaN/neg but MISSED by the P0415 wave. **Cross-plan pattern:** [`main.rs:384-398`](../../rio-scheduler/src/main.rs) loop ALREADY validates `cutoff_secs` with `is_finite() && > 0.0` — `cpu_limit_cores` sits right beside it in the same struct, same TOML surface, zero scrutiny.

Fix is a 4-line `if let Some(limit)` addition inside the existing `:384` loop — same `anyhow::ensure!` shape as `cutoff_secs` at `:390-395`.

## Entry criteria

- [P0415](plan-0415-retrypolicy-backoff-f64-bounds-complete.md) merged (**DONE** — establishes the `is_finite() && >0.0` f64-bounds pattern)
- [P0416](plan-0416-validate-config-four-crate-extraction.md) merged (**DONE** — `validate_config()` fn exists at [`main.rs:178`](../../rio-scheduler/src/main.rs))

## Tasks

### T1 — `fix(scheduler):` cpu_limit_cores bounds check in size_classes loop

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) at [`:384-398`](../../rio-scheduler/src/main.rs). Add inside the existing `for class in &cfg.size_classes` loop, after the `cutoff_secs` ensure at `:390-395`:

```rust
for class in &cfg.size_classes {
    // ... existing cutoff_secs ensure at :390-395 ...
    anyhow::ensure!(
        class.cutoff_secs.is_finite() && class.cutoff_secs > 0.0,
        "size_classes[{}].cutoff_secs must be finite and positive, got {}",
        class.name, class.cutoff_secs
    );
    // r[impl sched.classify.cpu-bump]
    // cpu_limit_cores is Option<f64> — None means no CPU check. Some(nan)
    // or Some(neg) would silently disable or always-bump respectively
    // (assignment.rs:128 `c > limit` — NaN→always-false, neg→always-true).
    // Same bounds-check shape as cutoff_secs / P0415's backoff_*.
    // Missed by the P0415 wave (bughunt-mc238, P997534805).
    if let Some(limit) = class.cpu_limit_cores {
        anyhow::ensure!(
            limit.is_finite() && limit > 0.0,
            "size_classes[{}].cpu_limit_cores must be finite and positive when set, got {}",
            class.name, limit
        );
    }
    // ... existing gauge set at :396-397 ...
}
```

**NOTE:** [P0304](plan-0304-trivial-batch-p0222-harness.md)-T189 plans to move the `:384` loop's `cutoff_secs` ensure INTO `validate_config()` at `:178`. If T189 landed, add this cpu_limit_cores check in the same moved-location. If T189 unimplemented, add at `:384` inline (T189 will move both together later).

### T2 — `test(scheduler):` config_rejects_{nan,neg,zero}_cpu_limit_cores

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) `cfg(test)` mod. Add after the existing `config_rejects_*` tests (P0409 pattern at `:930+`, extended by P0415):

```rust
#[test]
fn config_rejects_nan_cpu_limit_cores() {
    let mut cfg = test_valid_config();
    cfg.size_classes = vec![rio_scheduler::SizeClassConfig {
        name: "small".into(),
        cutoff_secs: 30.0,
        mem_limit_bytes: 1 << 30,
        cpu_limit_cores: Some(f64::NAN),
    }];
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(
        err.contains("cpu_limit_cores") && err.contains("finite"),
        "NaN cpu_limit must be rejected with clear message, got: {err}"
    );
}

#[test]
fn config_rejects_negative_cpu_limit_cores() {
    let mut cfg = test_valid_config();
    cfg.size_classes = vec![rio_scheduler::SizeClassConfig {
        name: "small".into(),
        cutoff_secs: 30.0,
        mem_limit_bytes: 1 << 30,
        cpu_limit_cores: Some(-1.0),
    }];
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(
        err.contains("cpu_limit_cores") && err.contains("positive"),
        "negative cpu_limit must be rejected, got: {err}"
    );
}

#[test]
fn config_accepts_none_cpu_limit_cores() {
    let mut cfg = test_valid_config();
    cfg.size_classes = vec![rio_scheduler::SizeClassConfig {
        name: "small".into(),
        cutoff_secs: 30.0,
        mem_limit_bytes: 1 << 30,
        cpu_limit_cores: None,  // explicit: None is fine (CPU check disabled)
    }];
    validate_config(&cfg).expect("None cpu_limit_cores = no check, should be valid");
}
```

**NOTE:** `SizeClassConfig` may have more fields — grep the struct def at dispatch and fill in required fields. `test_valid_config()` helper exists per [`:942`](../../rio-scheduler/src/main.rs).

### T3 — `docs(scheduler):` assignment.rs — cite validation

MODIFY [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) at [`:65-68`](../../rio-scheduler/src/assignment.rs). Extend the `cpu_limit_cores` field doc-comment:

```rust
/// check. Optional so existing TOML without `cpu_limit_cores` keeps
/// working (None → no CPU check). When Some, main.rs validates
/// is_finite && >0 at startup (same shape as cutoff_secs / P0415
/// backoff_* bounds; bughunt-mc238 found this gap — `c > NaN` at :128
/// would silently disable the bump, `c > neg` would always-bump).
pub cpu_limit_cores: Option<f64>,
```

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'cpu_limit_cores.*is_finite\|cpu_limit_cores must be finite' rio-scheduler/src/main.rs` → ≥1 hit (T1: ensure clause present)
- `cargo nextest run -p rio-scheduler config_rejects_nan_cpu_limit config_rejects_negative_cpu_limit config_accepts_none_cpu_limit` → 3 passed (T2)
- **T2 mutation:** remove the `if let Some(limit) = class.cpu_limit_cores` block → `config_rejects_nan_cpu_limit_cores` FAILS (test proves the ensure is load-bearing)
- `grep 'bughunt-mc238\|P997534805\|NaN.*silently disable' rio-scheduler/src/assignment.rs` → ≥1 hit (T3: doc-comment cite)
- `nix develop -c tracey query rule sched.classify.cpu-bump` → shows `impl` site (T1 adds a second `r[impl sched.classify.cpu-bump]` annotation; existing one at [`assignment.rs:122`](../../rio-scheduler/src/assignment.rs) stays)

## Tracey

References existing markers:
- `r[sched.classify.cpu-bump]` — T1 adds a second `r[impl]` annotation at the validation site (same marker, validates the mechanism the existing `r[impl]` at [`assignment.rs:122`](../../rio-scheduler/src/assignment.rs) implements). T2's tests could carry `r[verify sched.classify.cpu-bump]` but they verify the bounds-check, not the bump-logic itself — leave unverified (the bump-logic has its own tests).

No new markers needed — `r[sched.classify.cpu-bump]` at [`scheduler.md:215`](../../docs/src/components/scheduler.md) already covers the mechanism.

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: +cpu_limit_cores if-let ensure inside :384 size_classes loop (or inside validate_config() if P0304-T189 moved the loop). T2: +3 config_rejects/accepts tests after :930"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T3: :65-68 field doc-comment — cite bounds-check + NaN/neg failure modes"}
]
```

```
rio-scheduler/src/
├── main.rs          # T1+T2: cpu_limit_cores ensure + 3 tests
└── assignment.rs    # T3: field doc-comment cite
```

## Dependencies

```json deps
{"deps": [415, 416], "soft_deps": [409, 304, 997534806], "note": "HARD-dep P0415 (DONE — is_finite()&&>0 f64-bounds pattern established for backoff_*; discovered_from=bughunter-mc238 cross-plan pattern-match). HARD-dep P0416 (DONE — validate_config() fn exists). Soft-dep P0409 (DONE — validate_config() + config_rejects_* test pattern originated here). Soft-dep P0304-T189 (moves the :384 size_classes cutoff_secs ensure INTO validate_config() — if T189 landed first, T1 adds cpu_limit_cores in the moved location; if T189 unimplemented, T1 adds at :384 inline and T189 moves both together later). Soft-dep P997534806 (ensure_required helper — different helper for string is_empty vs f64 is_finite; non-overlapping but both touch validate_config bodies across crates). COLLISION: rio-scheduler/src/main.rs count=38 HOT (5th highest). T1 is a 6-line additive insert inside an existing loop body at :384-398 — low semantic conflict. T2 is additive test-fns after :930 in the cfg(test) mod."}
```

**Depends on:** [P0415](plan-0415-retrypolicy-backoff-f64-bounds-complete.md) — f64-bounds pattern. [P0416](plan-0416-validate-config-four-crate-extraction.md) — validate_config() exists.

**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) count=38 HOT — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T189 moves the `:384` loop's cutoff_secs ensure into `validate_config()`; T1 here adds cpu_limit_cores adjacent. If both in-flight, sequence T189 FIRST (moves the loop, T1 adds into moved location). [P997534806](plan-997534806-ensure-required-helper-ten-site.md) touches `validate_config()` in all 5 crates for string `is_empty()` → `ensure_required()` migration — different lines (T1 is f64 bounds, that plan is string emptiness), additive, non-overlapping.
