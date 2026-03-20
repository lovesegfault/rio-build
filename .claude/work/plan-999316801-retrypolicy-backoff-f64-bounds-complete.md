# Plan 999316801: RetryPolicy backoff f64 bounds — complete P0409's sweep

rev-p409 correctness at [`rio-scheduler/src/main.rs:178`](../../rio-scheduler/src/main.rs). [P0409](plan-0409-scheduler-config-validation-retry-poison.md) extracted `validate_config()` and added bounds checks for `retry.jitter_fraction` + `poison.threshold`. But three more `RetryPolicy` `f64` fields are operator-settable via `scheduler.toml` ([P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) wiring) and are NOT validated:

| Field | At | Consumer | Bad value → failure |
|---|---|---|---|
| `backoff_base_secs` | [`worker.rs:203`](../../rio-scheduler/src/state/worker.rs) | `:229` `base * multiplier.powi(n)` | negative → base negative → `.max(0.0)` at `:248` silently zeros backoff |
| `backoff_multiplier` | `:205` | `:229` `.powi(attempt)` | NaN → `powi(NaN)=NaN` → `.max(0.0)` swallows to zero; `<1.0` → shrinking backoff (first retry waits LESS than base) |
| `backoff_max_secs` | `:207` | `:230` `.min(max_secs)` | negative → `base.min(-5)` negative → zero backoff; NaN → `NaN.min(x)=NaN` → zero |

**No panic risk** — the [`worker.rs:248`](../../rio-scheduler/src/state/worker.rs) `#[allow(manual_clamp)]` `.max(0.0).min(MAX_BACKOFF_SECS)` handles NaN/infinity (IEEE 754 `NaN.max(x)=x`). But the result is **silent zero-backoff** — the same thrash-retry failure mode that [P0409's jitter check at `:204-209`](../../rio-scheduler/src/main.rs) explicitly guards. The scrutiny recipe at [`main.rs:175-177`](../../rio-scheduler/src/main.rs) ("grep for `Duration::from_secs_f64(<field>)` … check what happens at 0, negative, NaN") was applied to `jitter_fraction` but not swept across all P0307-exposed `f64`s.

This plan completes P0409's `RetryPolicy` sweep: three more `ensure!` blocks + rejection tests.

## Entry criteria

- [P0409](plan-0409-scheduler-config-validation-retry-poison.md) merged (**DONE** — `validate_config()` exists at `:178`, rejection-test mod at `:893`, `test_valid_config()` helper at `:905`)

## Tasks

### T1 — `fix(scheduler):` validate_config — backoff_base_secs finite + positive

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) in `validate_config()` after the `:204-209` `jitter_fraction` ensure:

```rust
// `RetryPolicy::backoff_duration` (worker.rs:229) computes
// `base_secs * multiplier.powi(attempt)` then `.max(0.0)` at :248.
// Negative base_secs → negative product → silently zero backoff
// (retries thrash). NaN/inf → .max(0.0) swallows but the INTENT
// was a real backoff. Require finite + positive — base_secs=0
// is also nonsense (zero backoff by design defeats the policy).
anyhow::ensure!(
    cfg.retry.backoff_base_secs.is_finite() && cfg.retry.backoff_base_secs > 0.0,
    "retry.backoff_base_secs must be finite and positive, got {} \
     (negative/NaN silently zero backoff via worker.rs:248 clamp)",
    cfg.retry.backoff_base_secs
);
```

### T2 — `fix(scheduler):` validate_config — backoff_multiplier finite + ≥1.0

Immediately after T1's `ensure!`:

```rust
// `multiplier.powi(attempt)` at worker.rs:229 — attempt grows, so
// multiplier < 1.0 means backoff SHRINKS with retries (attempt=2
// waits LESS than attempt=1). multiplier == 1.0 is valid (constant
// backoff). NaN.powi() = NaN → zero via clamp. Require finite + ≥1.0.
anyhow::ensure!(
    cfg.retry.backoff_multiplier.is_finite() && cfg.retry.backoff_multiplier >= 1.0,
    "retry.backoff_multiplier must be finite and >= 1.0, got {} \
     (<1.0 makes backoff SHRINK with retries; NaN silently zeros)",
    cfg.retry.backoff_multiplier
);
```

### T3 — `fix(scheduler):` validate_config — backoff_max_secs finite + positive

Immediately after T2's `ensure!`:

```rust
// `base.min(max_secs)` at worker.rs:230 — negative max_secs caps
// everything negative → zero via clamp. NaN.min(x) = NaN → zero.
// Infinity is HANDLED (worker.rs test_retry_backoff_infinity_clamped
// proves the 1-year clamp catches it), but it's still operator-
// error — no sane deployment wants unbounded backoff. Require
// finite + positive, and >= base_secs (max < base is contradictory).
anyhow::ensure!(
    cfg.retry.backoff_max_secs.is_finite()
        && cfg.retry.backoff_max_secs > 0.0
        && cfg.retry.backoff_max_secs >= cfg.retry.backoff_base_secs,
    "retry.backoff_max_secs must be finite, positive, and >= backoff_base_secs \
     (got max={}, base={})",
    cfg.retry.backoff_max_secs,
    cfg.retry.backoff_base_secs
);
```

**CARE — ordering vs T1:** the `max_secs >= base_secs` cross-check assumes `base_secs` is already validated finite+positive (T1). Place T1→T2→T3 in source order so an invalid `base_secs` fails with the T1 message, not a confusing T3 cross-check message.

### T4 — `test(scheduler):` rejection tests — negative/NaN/sub-1.0/max<base

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) `#[cfg(test)]` mod near `:893` (P0409's rejection tests). Add, reusing `test_valid_config()` at `:905`:

```rust
// r[verify sched.retry.per-worker-budget]
/// Negative backoff_base_secs → silently zero backoff via the
/// `.max(0.0)` at worker.rs:248. Same thrash-mode as jitter>1
/// but via a different field — both guarded at config-load.
#[test]
fn config_rejects_negative_backoff_base() {
    let cfg = Config {
        retry: RetryPolicy { backoff_base_secs: -5.0, ..Default::default() },
        ..test_valid_config()
    };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("backoff_base_secs"), "{err}");
    assert!(err.contains("-5"), "{err}");
}

#[test]
fn config_rejects_nan_backoff_base() {
    let cfg = Config {
        retry: RetryPolicy { backoff_base_secs: f64::NAN, ..Default::default() },
        ..test_valid_config()
    };
    assert!(validate_config(&cfg).is_err());
}

/// Multiplier < 1.0 → shrinking backoff (attempt=2 waits LESS than
/// attempt=1). The math works — it's just operator-error.
#[test]
fn config_rejects_sub_one_multiplier() {
    let cfg = Config {
        retry: RetryPolicy { backoff_multiplier: 0.5, ..Default::default() },
        ..test_valid_config()
    };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("backoff_multiplier"), "{err}");
    assert!(err.contains(">= 1.0"), "{err}");
}

#[test]
fn config_rejects_nan_multiplier() {
    let cfg = Config {
        retry: RetryPolicy { backoff_multiplier: f64::NAN, ..Default::default() },
        ..test_valid_config()
    };
    assert!(validate_config(&cfg).is_err());
}

/// max_secs < base_secs is contradictory (the "max" is below the
/// "base" — every backoff clamps to max, which defeats the
/// exponential). Catch at config-load with a clear message citing
/// both values.
#[test]
fn config_rejects_max_below_base() {
    let cfg = Config {
        retry: RetryPolicy {
            backoff_base_secs: 10.0,
            backoff_max_secs: 5.0,
            ..Default::default()
        },
        ..test_valid_config()
    };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("backoff_max_secs"), "{err}");
    assert!(err.contains(">= backoff_base_secs"), "{err}");
}

/// Boundary: multiplier=1.0 (constant backoff), base=max (no growth
/// room — every attempt waits base_secs). Both valid edge cases.
#[test]
fn config_accepts_backoff_boundaries() {
    // multiplier=1.0 → constant backoff, valid.
    let cfg = Config {
        retry: RetryPolicy { backoff_multiplier: 1.0, ..Default::default() },
        ..test_valid_config()
    };
    validate_config(&cfg).expect("multiplier=1.0 should be valid");

    // base==max → clamped immediately, no exponential room, valid.
    let cfg = Config {
        retry: RetryPolicy {
            backoff_base_secs: 30.0,
            backoff_max_secs: 30.0,
            ..Default::default()
        },
        ..test_valid_config()
    };
    validate_config(&cfg).expect("base==max should be valid");

    // Defaults (5.0, 2.0, 300.0) pass all checks.
    validate_config(&test_valid_config()).expect("defaults should be valid");
}
```

**CARE — `is_finite()` vs `RangeInclusive::contains`:** P0409's jitter check uses `(0.0..=1.0).contains(&jf)` because both endpoints are inclusive concrete values. For base/max (positive, no upper bound), `x.is_finite() && x > 0.0` is clearest — `contains` would need `f64::EPSILON..f64::MAX` or similar, less readable. The `is_finite() &&` idiom matches the existing `size_classes` check at [`main.rs:354`](../../rio-scheduler/src/main.rs) (`class.cutoff_secs.is_finite() && class.cutoff_secs > 0.0`).

**CARE — multiplier lower bound = 1.0 not > 1.0:** `multiplier == 1.0` means constant backoff (no exponential growth). Valid — it's `tokio::time::interval` semantics with `base_secs` period. The failure mode is strictly `multiplier < 1.0` (shrinking). Use `>= 1.0`.

## Exit criteria

- `grep 'backoff_base_secs' rio-scheduler/src/main.rs | grep -c 'ensure\|finite'` → ≥1 (T1 check present)
- `grep 'backoff_multiplier' rio-scheduler/src/main.rs | grep -c 'ensure\|>= 1.0'` → ≥1 (T2 check present)
- `grep 'backoff_max_secs' rio-scheduler/src/main.rs | grep -c 'ensure\|>= cfg.retry.backoff_base'` → ≥1 (T3 cross-check present)
- `cargo nextest run -p rio-scheduler config_rejects_negative_backoff_base config_rejects_nan_backoff_base config_rejects_sub_one_multiplier config_rejects_nan_multiplier config_rejects_max_below_base config_accepts_backoff_boundaries` → 6 passed
- **Mutation check:** remove the T1 `ensure!`, run `config_rejects_negative_backoff_base` → FAILS (validate_config returns `Ok(())`). Proves the test is load-bearing. Re-add.
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.retry.per-worker-budget]` — T1-T3 harden the TOML surface this marker describes at [`scheduler.md:110`](../../docs/src/components/scheduler.md). P0409 added the first `r[impl]` site at `:196`; T1-T3 extend the same `validate_config()` body, so the existing `r[impl]` annotation at `:196` covers the new ensures (no new `r[impl]` needed — same function, same guard class). T4's rejection tests add a second `r[verify]` site alongside P0409's `:893-1009` tests.

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1+T2+T3: +3 ensure! in validate_config() after :209 (backoff_base_secs finite+>0, backoff_multiplier finite+>=1, backoff_max_secs finite+>0+>=base). T4: +6 rejection/boundary tests in cfg(test) mod near :1009. HOT count=37."}
]
```

```
rio-scheduler/src/
└── main.rs                 # T1-T3: +3 ensure! after :209
                            # T4: +6 tests in cfg(test) mod
```

## Dependencies

```json deps
{"deps": [409], "soft_deps": [307, 999316802], "note": "rev-p409 correctness (discovered_from=409). HARD-dep P0409: validate_config() fn + test_valid_config() helper + config_rejects_* test mod all landed with P0409. Pre-P0409, T1-T3 would have no fn to insert into and T4 would have no test-infrastructure. Soft-dep P0307 (DONE — the wiring that made these fields operator-settable; discovered_from is 409 but the root surface is P0307). Soft-dep P999316802 (validate_config 4-crate extraction — DIFFERENT crates, no file overlap; mentioned because both complete P0409's pattern). main.rs count=37 HOT — T1-T3 are 3× ~10L ensure-block appends inside validate_config() (:209-222 region); T4 is cfg(test)-only append after :1009. P0409's edits are the immediate predecessor in the same regions — this plan extends P0409 in-place, not a conflict. P0304-T95/T96 (RebalancerConfig validation + min_samples guard) also touch validate_config() with a different ensure — additive, sequence-independent. P0304-T99931680101 (size_classes move into validate_config) — additive, sequence-independent (that T moves an ensure block INTO validate_config from :347; T1-T3 here append NEW ensures; both extend the same fn body non-overlappingly)."}
```

**Depends on:** [P0409](plan-0409-scheduler-config-validation-retry-poison.md) — `validate_config()` + `test_valid_config()` + rejection-test mod.
**Conflicts with:** [`main.rs`](../../rio-scheduler/src/main.rs) count=37. [P0304-T95/T96](plan-0304-trivial-batch-p0222-harness.md) add `min_samples` ensure in the same fn — additive, different subconfig. [P0304-T99931680101](plan-0304-trivial-batch-p0222-harness.md) moves `size_classes` ensure into `validate_config()` — additive, different subconfig. [P999316802](plan-999316802-validate-config-four-crate-extraction.md) touches gateway/controller/store/worker `main.rs`, NOT scheduler — zero file overlap.
