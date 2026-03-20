# Plan 0409: Scheduler config validation — RetryPolicy.jitter_fraction + PoisonConfig.threshold

rev-p307 correctness at [`rio-scheduler/src/state/worker.rs:225`](../../rio-scheduler/src/state/worker.rs) and [`rio-scheduler/src/state/derivation.rs:624`](../../rio-scheduler/src/state/derivation.rs). [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) wires `RetryPolicy` and `PoisonConfig` into `scheduler.toml`, opening these fields to operator input. Before P0307, both were code-constructed only (defaults: `jitter_fraction=0.2`, `threshold=3`). Post-P0307, an operator can set nonsense values that either **panic** or **silently wrong**:

| Field | Bad value | Failure mode |
|---|---|---|
| `retry.jitter_fraction` | negative (e.g. `-0.5`) | `rand::random_range(-jf..=jf)` → `random_range(0.5..=-0.5)` → **panic** "low must be ≤ high" |
| `retry.jitter_fraction` | > 1.0 (e.g. `1.5`) | `clamped*(1-1.5) = clamped*(-0.5)` → negative → silently clamped to `Duration::ZERO` at `:239` |
| `poison.threshold` | `0` | `is_poisoned()`: `count >= 0` always true → every derivation poisons on first reconcile |

This is the same class as `tick_interval_secs=0` → `tokio::time::interval(ZERO)` panic, which [`main.rs:189-192`](../../rio-scheduler/src/main.rs) already guards with `anyhow::ensure!`. The fix is another `ensure!` block in the same config-load gate.

**Why not validate at the struct layer (serde custom deserialize):** the existing convention is main.rs-level `ensure!` (see `:176-192`: `store_addr` non-empty, `database_url` non-empty, `tick_interval_secs > 0`, and at `:295` size-class cutoffs). Keeping the pattern — all config validation in one visible block, error messages cite the TOML key.

## Entry criteria

- [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) merged (`cfg.retry: RetryPolicy` and `cfg.poison: PoisonConfig` fields exist on `Config`, threaded to the actor)

## Tasks

### T1 — `fix(scheduler):` main.rs — validate RetryPolicy.jitter_fraction ∈ [0.0, 1.0]

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) in the config-validation block (after `:189-192` `tick_interval_secs > 0` ensure, before the `_root_guard` span entry at `:194`):

```rust
// `RetryPolicy::backoff_duration` computes `random_range(-jf..=jf)` —
// rand panics if low > high, so jf < 0 crashes on first retry. And
// jf > 1 makes `clamped * (1 - jf)` negative, which the Duration
// clamp at worker.rs:239 silently turns into ZERO backoff (retries
// become thrashing, not backoff). [0.0, 1.0] inclusive.
anyhow::ensure!(
    (0.0..=1.0).contains(&cfg.retry.jitter_fraction),
    "retry.jitter_fraction must be in [0.0, 1.0], got {} \
     (negative panics rand::random_range; >1 silently zeros backoff)",
    cfg.retry.jitter_fraction
);
```

**CARE — `RangeInclusive::contains` is the cleanest spelling:** `(0.0..=1.0).contains(&jf)` matches the existing `:295` size-class validation idiom. Avoid `jf >= 0.0 && jf <= 1.0` — clippy `manual_range_contains` (the workspace is `--deny warnings`).

### T2 — `fix(scheduler):` main.rs — validate PoisonConfig.threshold > 0

Same block, immediately after T1's `ensure!`:

```rust
// `PoisonConfig::is_poisoned` checks `count >= threshold` — threshold=0
// makes `count >= 0` vacuously true, so every derivation poisons at
// DAG-merge time before dispatch. threshold=1 is the practical minimum
// (poison-on-first-failure — aggressive but valid for single-worker
// dev with require_distinct_workers=false).
anyhow::ensure!(
    cfg.poison.threshold > 0,
    "poison.threshold must be positive, got {} \
     (threshold=0 means is_poisoned() is always true — \
     every derivation poisons immediately)",
    cfg.poison.threshold
);
```

### T3 — `test(scheduler):` config-validation rejects out-of-range values

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) `#[cfg(test)]` mod (near P0307-T3's `poison_and_retry_load_from_toml` — same region). Factor the validation into a testable helper if P0307 didn't already:

```rust
// If main.rs validation is inline in `main()`, extract to
// `fn validate_config(cfg: &Config) -> anyhow::Result<()>` so it's
// callable from tests without spawning the full scheduler. The
// existing tick_interval/store_addr/database_url ensures move with it.

#[test]
fn config_rejects_negative_jitter() {
    let cfg = Config { retry: RetryPolicy { jitter_fraction: -0.1, ..Default::default() }, ..test_valid_config() };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("jitter_fraction"), "{err}");
    assert!(err.contains("-0.1"), "{err}");
}

#[test]
fn config_rejects_jitter_above_one() {
    let cfg = Config { retry: RetryPolicy { jitter_fraction: 1.5, ..Default::default() }, ..test_valid_config() };
    assert!(validate_config(&cfg).is_err());
}

#[test]
fn config_rejects_zero_poison_threshold() {
    let cfg = Config { poison: PoisonConfig { threshold: 0, ..Default::default() }, ..test_valid_config() };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("poison.threshold"), "{err}");
}

#[test]
fn config_accepts_boundary_values() {
    // jitter_fraction = 0.0 → deterministic (no jitter), valid
    // jitter_fraction = 1.0 → backoff ∈ [0, 2*clamped], valid (wide but sane)
    // threshold = 1 → poison on first failure, aggressive but valid
    for jf in [0.0, 1.0] {
        let cfg = Config { retry: RetryPolicy { jitter_fraction: jf, ..Default::default() }, ..test_valid_config() };
        validate_config(&cfg).expect(&format!("jf={jf} should be valid"));
    }
    let cfg = Config { poison: PoisonConfig { threshold: 1, ..Default::default() }, ..test_valid_config() };
    validate_config(&cfg).expect("threshold=1 should be valid");
}
```

**CARE — `test_valid_config()` helper:** the `Config` struct has required fields (`store_addr`, `database_url`) that `Default::default()` may leave empty — which would fail the `ensure!(!cfg.store_addr.is_empty())` before reaching the new checks. Either (a) a `test_valid_config()` that fills all required fields, or (b) order the ensures in `validate_config` so field-presence checks come AFTER bounds checks (less natural — prefer (a)).

**CARE — extraction to `validate_config` fn:** if this refactor changes call-site visibility (main.rs `async fn main` → inline ensures move to `fn validate_config(&Config) -> Result<()>`), the P0307 `poison_and_retry_load_from_toml` test may also want to call it — check at dispatch if P0307 left testable-hook infrastructure.

## Exit criteria

- `grep 'jitter_fraction' rio-scheduler/src/main.rs` → ≥1 hit (ensure check present)
- `grep 'poison.threshold' rio-scheduler/src/main.rs` → ≥1 hit (ensure check present)
- `cargo nextest run -p rio-scheduler config_rejects` → ≥3 passed (negative-jitter, jitter-above-one, zero-threshold)
- `cargo nextest run -p rio-scheduler config_accepts_boundary` → 1 passed
- Manual smoke: `scheduler.toml` with `[retry]\njitter_fraction = -0.5` → scheduler exits non-zero at startup with "jitter_fraction must be in [0.0, 1.0]" (not a rand panic 3 retries later)
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.retry.per-worker-budget]` — T1+T2 harden the TOML surface this marker describes at [`scheduler.md:110-123`](../../docs/src/components/scheduler.md). No bump — validation is a guardrail, not a behavior change. T3 adds a third verify site (P0307-T3 and the pre-existing default-test are the first two).

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1+T2: anyhow::ensure! jitter_fraction in 0..=1 and threshold>0 in config-validation block after :192. T3: extract validate_config(&Config) helper if not already; cfg(test) mod adds config_rejects_* + config_accepts_boundary tests."}
]
```

```
rio-scheduler/src/
└── main.rs                 # T1+T2: +2 ensure! blocks after :192
                            # T3: cfg(test) validation tests
```

## Dependencies

```json deps
{"deps": [307], "soft_deps": [], "note": "rev-p307 correctness ×2 (discovered_from=307). HARD-dep P0307: cfg.retry and cfg.poison must EXIST on Config (P0307-T1 adds them) and be threaded to the actor (P0307-T2) before validating makes sense — pre-P0307, the structs are code-constructed defaults only and can't hold out-of-range values. The reviewer noted 'Could batch with jitter_fraction fix' — both findings validated in one ensure block; they're the same-file same-region same-pattern fix. main.rs count=moderate (P0307 + P0355 + P0349 all touch it) — this plan's edits are localized to the :176-200 config-validation block + cfg(test) mod; no signature changes. Rebase-clean with P0307 (whose T1 adds fields at :42, T2 threads at actor-spawn near :380 — different regions)."}
```

**Depends on:** [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) — `cfg.retry` / `cfg.poison` fields + actor threading.
**Conflicts with:** [`main.rs`](../../rio-scheduler/src/main.rs) — P0307's T1 at `:42` (field adds) and T2 near `:380` (actor builder chain); T1+T2 here are at `:192-200` (validation block). Non-overlapping; additive ensures. [P0304-T95](plan-0304-trivial-batch-p0222-harness.md) (RebalancerConfig validation) touches the same config-validation region with a different `ensure!` — additive, sequence-independent.
