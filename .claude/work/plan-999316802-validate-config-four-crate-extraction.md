# Plan 999316802: validate_config() four-crate extraction — spread P0409 pattern

consol-mc225 feature at [`rio-controller/src/main.rs:162`](../../rio-controller/src/main.rs). [P0409](plan-0409-scheduler-config-validation-retry-poison.md) extracted `fn validate_config(cfg: &Config) -> anyhow::Result<()>` at [`rio-scheduler/src/main.rs:178`](../../rio-scheduler/src/main.rs) — 5 `ensure!` checks, unit-tested via `config_rejects_*` tests at `:893-1009`, rationale doc-comment at `:164-177`. Four other crates still have INLINE `ensure!` in `main()`:

| Crate | Inline ensures at | Count | Shape |
|---|---|---|---|
| `rio-gateway` | [`main.rs:203-218`](../../rio-gateway/src/main.rs) + `:358-363` | 5 | 4× required-addr-nonempty + 1× jwt.required↔key_path consistency |
| `rio-controller` | [`main.rs:162-174`](../../rio-controller/src/main.rs) | 2 | scheduler_addr nonempty + `autoscaler_poll_secs > 0` |
| `rio-store` | [`main.rs:188-191`](../../rio-store/src/main.rs) | 1 | database_url nonempty |
| `rio-worker` | [`main.rs:57-64`](../../rio-worker/src/main.rs) | 2 | scheduler_addr + store_addr nonempty |

**Why extract now:** controller's `autoscaler_poll_secs > 0` at `:171-174` is the 5th copy of the `interval(ZERO)` panic guard (same rationale, same comment structure as scheduler's `tick_interval_secs > 0` at `:187-194`). Untestable while inline. [P0409's doc-comment at `:164-170`](../../rio-scheduler/src/main.rs) ("Extracted from `main()` so the checks are unit-testable without spinning up the full scheduler") applies verbatim to the controller case.

**Distinct from [P0412](plan-0412-standing-guard-config-tests-four-crates.md):** P0412 adds `figment::Jail` roundtrip tests (catches orphan fields — structural wiring). This plan extracts the VALIDATION target (catches nonsense values — bounds checks). Complementary, orthogonal: P0412 proves `[foo]` reaches `cfg.foo`; this plan proves `cfg.foo = -5` is rejected. P0412's note says "p409 non-colliding" — correct: P0412 adds `#[test]` fns, this extracts `fn validate_config` + adds DIFFERENT `#[test]` fns (rejection tests, not roundtrip tests).

## Entry criteria

- [P0409](plan-0409-scheduler-config-validation-retry-poison.md) merged (**DONE** — the extraction precedent + `test_valid_config()` helper shape)

## Tasks

### T1 — `refactor(controller):` extract validate_config — autoscaler_poll_secs > 0

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs). Move the inline ensures at `:162-174` into a new `fn validate_config(cfg: &Config) -> anyhow::Result<()>` placed before `async fn main()` at `:122`:

```rust
/// Config validation — bounds checks on operator-settable fields.
///
/// Extracted from `main()` so the checks are unit-testable without
/// spinning up the full controller (kube-client connect, reconciler
/// spawn). Every `ensure!` documents a specific crash or silent-wrong
/// that occurs AFTER startup if the bad value gets through.
///
/// See rio-scheduler/src/main.rs:164-177 for the scrutiny recipe:
/// grep for `interval(..<field>)` / `from_secs(<field>)` in consumer
/// code; check what happens at 0, negative, very-large.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or controller.toml)"
    );
    // `tokio::time::interval(ZERO)` panics. Autoscaler::run feeds
    // `from_secs(cfg.autoscaler_poll_secs)` into interval() —
    // `autoscaler_poll_secs = 0` would panic inside spawn_monitored
    // (logged, controller survives, but autoscaling silently dead).
    anyhow::ensure!(
        cfg.autoscaler_poll_secs > 0,
        "autoscaler_poll_secs must be positive (tokio::time::interval panics on ZERO)"
    );
    Ok(())
}
```

Replace the inline `:162-174` block in `main()` with:

```rust
validate_config(&cfg)?;
```

**CARE — `store_addr.is_empty()` at `:179-184` is a `warn!` not `ensure!` — NOT moved.** It's soft (warns, doesn't reject). Keep inline.

### T2 — `refactor(gateway):` extract validate_config — 5 ensures

MODIFY [`rio-gateway/src/main.rs`](../../rio-gateway/src/main.rs). Move the inline ensures at `:203-218` AND `:358-363` into `fn validate_config` before `async fn main()` at `:157`:

```rust
/// Config validation — see rio-scheduler/src/main.rs:164-177.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.host_key.as_os_str().is_empty(),
        "host_key is required (set --host-key, RIO_HOST_KEY, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.authorized_keys.as_os_str().is_empty(),
        "authorized_keys is required (set --authorized-keys, RIO_AUTHORIZED_KEYS, or gateway.toml)"
    );
    // jwt.required=true + key_path=None is a misconfiguration: can't
    // mint, can't degrade → every SSH connect rejected. Fail loud at
    // startup instead of rejecting every connection at runtime.
    anyhow::ensure!(
        !(cfg.jwt.required && cfg.jwt.key_path.is_none()),
        "jwt.required=true but jwt.key_path is unset — cannot mint JWTs, \
         would reject every SSH connection (set RIO_JWT__KEY_PATH or unset RIO_JWT__REQUIRED)"
    );
    Ok(())
}
```

Replace the `:203-218` block with `validate_config(&cfg)?;`. Delete the now-duplicate `:358-363` jwt ensure (moved into the fn).

**CARE — the `:358-363` jwt ensure is AFTER `:220` `_root_guard` span entry — it fired AFTER gRPC connect attempts.** Moving it into `validate_config()` makes it fire BEFORE connect — correct (fail fast, same as scheduler).

### T3 — `refactor(store):` extract validate_config — database_url

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs). Single ensure at `:188-191`. Smallest extraction but creates the hook for future bounds (gc timing, chunk_backend validation):

```rust
/// Config validation — see rio-scheduler/src/main.rs:164-177. Only
/// one check today (database_url) but creates the hook for gc.*,
/// chunk_backend.*, signing.* bounds as they become operator-settable.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg.database_url.is_empty(),
        "database_url is required (set --database-url, RIO_DATABASE_URL, or store.toml)"
    );
    Ok(())
}
```

### T4 — `refactor(worker):` extract validate_config — scheduler_addr + store_addr

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs). Two ensures at `:57-64`:

```rust
/// Config validation — see rio-scheduler/src/main.rs:164-177.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or worker.toml)"
    );
    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or worker.toml)"
    );
    Ok(())
}
```

**CARE — worker's `Config` lives in [`config.rs`](../../rio-worker/src/config.rs), not `main.rs`.** Place `validate_config` in `main.rs` (near the ensure call-site, consistent with other crates) OR in `config.rs` (near the struct). Prefer `main.rs` for cross-crate consistency — scheduler's is in `main.rs`.

### T5 — `test(controller):` config_rejects_zero_autoscaler_poll

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) `#[cfg(test)] mod tests` at `:493`. Add a `test_valid_config()` helper + rejection test:

```rust
/// All required fields filled with valid values — so rejection
/// tests can patch ONE field and prove that specific check fires.
fn test_valid_config() -> Config {
    Config {
        scheduler_addr: "http://localhost:9000".into(),
        store_addr: "http://localhost:9001".into(),
        autoscaler_poll_secs: 30,
        ..Default::default()
    }
}

/// `autoscaler_poll_secs = 0` → `tokio::time::interval(ZERO)` panics
/// inside Autoscaler::run. validate_config catches at startup.
#[test]
fn config_rejects_zero_autoscaler_poll() {
    let cfg = Config { autoscaler_poll_secs: 0, ..test_valid_config() };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("autoscaler_poll_secs"), "{err}");
}

#[test]
fn config_rejects_empty_scheduler_addr() {
    let cfg = Config { scheduler_addr: String::new(), ..test_valid_config() };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("scheduler_addr"), "{err}");
}

#[test]
fn config_accepts_valid() {
    validate_config(&test_valid_config()).expect("valid config should pass");
}
```

### T6 — `test(gateway):` config_rejects_* — empty-addrs + jwt-consistency

MODIFY [`rio-gateway/src/main.rs`](../../rio-gateway/src/main.rs) `#[cfg(test)] mod tests` at `:434`. Same helper + rejection shape:

```rust
fn test_valid_config() -> Config {
    Config {
        scheduler_addr: "http://localhost:9000".into(),
        store_addr: "http://localhost:9001".into(),
        host_key: "/tmp/host_key".into(),
        authorized_keys: "/tmp/authorized_keys".into(),
        ..Default::default()
    }
}

#[test]
fn config_rejects_empty_required_addrs() {
    for patch in [
        |c: &mut Config| c.scheduler_addr = String::new(),
        |c: &mut Config| c.store_addr = String::new(),
        |c: &mut Config| c.host_key = Default::default(),
        |c: &mut Config| c.authorized_keys = Default::default(),
    ] {
        let mut cfg = test_valid_config();
        patch(&mut cfg);
        assert!(validate_config(&cfg).is_err(), "patched config should be rejected");
    }
}

/// jwt.required=true without key_path is a misconfiguration — can't
/// mint, can't degrade. validate_config catches before SSH listener
/// spawns (not at first-connect).
#[test]
fn config_rejects_jwt_required_without_key() {
    let mut cfg = test_valid_config();
    cfg.jwt.required = true;
    cfg.jwt.key_path = None;
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("jwt.required"), "{err}");
}

#[test]
fn config_accepts_valid() {
    validate_config(&test_valid_config()).expect("valid config should pass");
}
```

### T7 — `test(store):` config_rejects_empty_database_url

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs) `#[cfg(test)] mod tests` at `:509`:

```rust
fn test_valid_config() -> Config {
    Config {
        database_url: "postgres://localhost/rio".into(),
        ..Default::default()
    }
}

#[test]
fn config_rejects_empty_database_url() {
    let cfg = Config { database_url: String::new(), ..test_valid_config() };
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(err.contains("database_url"), "{err}");
}

#[test]
fn config_accepts_valid() {
    validate_config(&test_valid_config()).expect("valid config should pass");
}
```

### T8 — `test(worker):` config_rejects_empty_addrs

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) `#[cfg(test)] mod tests` at `:971`:

```rust
fn test_valid_config() -> Config {
    Config {
        scheduler_addr: "http://localhost:9000".into(),
        store_addr: "http://localhost:9001".into(),
        ..Default::default()
    }
}

#[test]
fn config_rejects_empty_addrs() {
    for patch in [
        |c: &mut Config| c.scheduler_addr = String::new(),
        |c: &mut Config| c.store_addr = String::new(),
    ] {
        let mut cfg = test_valid_config();
        patch(&mut cfg);
        assert!(validate_config(&cfg).is_err());
    }
}

#[test]
fn config_accepts_valid() {
    validate_config(&test_valid_config()).expect("valid config should pass");
}
```

## Exit criteria

- `grep -c 'fn validate_config' rio-gateway/src/main.rs rio-controller/src/main.rs rio-store/src/main.rs rio-worker/src/main.rs` → 4 (one per crate)
- `grep -c 'validate_config(&cfg)?' rio-gateway/src/main.rs rio-controller/src/main.rs rio-store/src/main.rs rio-worker/src/main.rs` → 4 (call-site in each main())
- `grep -c 'fn test_valid_config' rio-gateway/src/main.rs rio-controller/src/main.rs rio-store/src/main.rs rio-worker/src/main.rs` → 4 (helper in each)
- `grep -c 'config_rejects_\|config_accepts_valid' rio-controller/src/main.rs` → ≥3 (T5: 2 rejects + 1 accept)
- `grep -c 'config_rejects_\|config_accepts_valid' rio-gateway/src/main.rs` → ≥3 (T6: 2 rejects + 1 accept)
- `grep 'anyhow::ensure' rio-gateway/src/main.rs | grep -v 'fn validate_config' | wc -l` → count == moved-ensure-count (proves ensures are in the fn, not still-inline; re-check at dispatch — the `:358` jwt ensure should be gone from `main()`)
- `cargo nextest run -p rio-controller config_` → ≥3 passed
- `cargo nextest run -p rio-gateway config_` → ≥3 passed
- `cargo nextest run -p rio-store config_` → ≥2 passed
- `cargo nextest run -p rio-worker config_` → ≥2 passed
- **Mutation (controller suffices):** revert `validate_config` call-site to no-op (`Ok(())`), run `config_rejects_zero_autoscaler_poll` → the test still passes (it calls `validate_config` directly, not `main()`). Proves the test exercises the fn. Then patch the ensure to `>= 0` → test FAILS. Proves the check is load-bearing.
- `/nbr .#ci` green

## Tracey

No new markers. The extraction is test-infrastructure + refactor, not spec-behavior. The individual ensure checks are per-crate config bounds — no cross-crate spec-mandated invariant. `r[sched.retry.per-worker-budget]` is scheduler-specific (P0409's domain); controller's `autoscaler_poll_secs` has no spec marker (it's a local implementation knob, not a spec-mandated behavior). Same reasoning as [P0412's Tracey section](plan-0412-standing-guard-config-tests-four-crates.md): "test-infrastructure, not spec-behavior."

## Files

```json files
[
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T1: extract validate_config before :122; replace :162-174 inline ensures with call. T5: +test_valid_config + 3 tests in cfg(test) mod :493+. HOT count=25"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T2: extract validate_config before :157; replace :203-218+:358-363 with single call. T6: +test_valid_config + 3 tests in cfg(test) mod :434+. HOT count=17"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T3: extract validate_config before :176; replace :188-191 with call. T7: +test_valid_config + 2 tests in cfg(test) mod :509+. HOT count=30"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T4: extract validate_config before :28; replace :57-64 with call. T8: +test_valid_config + 2 tests in cfg(test) mod :971+. HOT count=40"}
]
```

```
rio-controller/src/main.rs    # T1+T5: extract + 3 tests
rio-gateway/src/main.rs       # T2+T6: extract + 3 tests
rio-store/src/main.rs         # T3+T7: extract + 2 tests
rio-worker/src/main.rs        # T4+T8: extract + 2 tests
```

## Dependencies

```json deps
{"deps": [409], "soft_deps": [412, 307, 999316801], "note": "consol-mc225 feature (discovered_from=null per consolidator origin; P0409 is the pattern precedent). HARD-dep P0409: the fn-shape, doc-comment, test_valid_config helper convention, and rejection-test-mod structure all landed with P0409 — this plan copies that pattern 4×. Without P0409, the 'see rio-scheduler/src/main.rs:164-177' cross-ref is dangling. SOFT-dep P0412 (standing-guard Jail tests — touches ALL FOUR same main.rs files at cfg(test) mod boundary): P0412-T1..T4 add all_subconfigs_* tests, this plan adds validate_config fn + config_rejects_* tests. BOTH pure-additive to cfg(test) mods; both modify main() body (P0412 doesn't, this plan does via extract). Sequence-independent: P0412's all_subconfigs_* tests call rio_common::config::load (Jail-wrapped), NOT validate_config — they test wiring not bounds. This plan's config_rejects_* call validate_config directly. Non-overlapping test fns + non-overlapping main() regions (P0412 doesn't touch main() body, only cfg(test)). Soft-dep P0307 (DONE — the PoisonConfig/RetryPolicy wiring that motivated P0409). Soft-dep P999316801 (RetryPolicy backoff f64 bounds — scheduler-only, zero file overlap; mentioned because both complete P0409's pattern). rio-worker/src/main.rs HOT=40 highest — T4+T8 are fn-extract-before-main + cfg(test)-append; the main() body only changes at :57-64 (ensure block → 1-line call), low semantic conflict."}
```

**Depends on:** [P0409](plan-0409-scheduler-config-validation-retry-poison.md) — the fn-shape + test-infrastructure precedent.
**Conflicts with:** [P0412](plan-0412-standing-guard-config-tests-four-crates.md) touches all four same `main.rs` files at `cfg(test)` mod — both pure-additive, P0412 adds `all_subconfigs_*`, this adds `validate_config` + `config_rejects_*`. Rebase-clean either order. Sequence preference: this plan FIRST (extracts the fn), then P0412 (adds roundtrip tests that can optionally also call `validate_config` on the loaded config — bonus coverage). [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) HOT count=40, [`rio-store/src/main.rs`](../../rio-store/src/main.rs) count=30, [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) count=25 — all extractions are localized to one region (inline ensures → fn call), low semantic conflict.
