# Plan 0425: ensure_required() helper — 10-site DRY + whitespace-trim

rev-p416 + coordinator PRIORITY. Ten identical `"X is required (set --flag, ENV, or crate.toml)"` error-templates across 5 `validate_config()` fns:

| Crate | Line | Field | Template |
|---|---|---|---|
| scheduler | [`:181`](../../rio-scheduler/src/main.rs) | `store_addr` | `"store_addr is required (set --store-addr, RIO_STORE_ADDR, or scheduler.toml)"` |
| scheduler | [`:185`](../../rio-scheduler/src/main.rs) | `database_url` | same shape |
| gateway | [`:167`](../../rio-gateway/src/main.rs) | `scheduler_addr` | same shape |
| gateway | [`:171`](../../rio-gateway/src/main.rs) | `store_addr` | same shape |
| gateway | [`:175`](../../rio-gateway/src/main.rs) | `host_key` | same shape |
| gateway | [`:179`](../../rio-gateway/src/main.rs) | `authorized_keys` | same shape |
| controller | [`:134`](../../rio-controller/src/main.rs) | `scheduler_addr` | same shape |
| store | [`:181`](../../rio-store/src/main.rs) | `database_url` | same shape |
| worker | [`:34`](../../rio-worker/src/main.rs) | `scheduler_addr` | same shape |
| worker | [`:38`](../../rio-worker/src/main.rs) | `store_addr` | same shape |

**IRONY:** [P0416](plan-0416-validate-config-four-crate-extraction.md) spread the pattern 4× — the 10-instance count is P0416's own side-effect (pre-P0416, only scheduler had 2). Consolidating the template it spread.

**Correctness angle:** all 10 sites guard with `!cfg.field.is_empty()` — whitespace-only (`"   "`) passes. Operator sets `RIO_SCHEDULER_ADDR="  "` (trailing space typo) → validation passes → gRPC connect fails with a cryptic `transport error: tcp connect error: invalid socket address syntax: '  '` buried in startup. The shared helper `trim()`s before `is_empty()` — catches the whitespace case with the same clear "X is required" message.

**Helper location:** [`rio-common/src/config.rs`](../../rio-common/src/config.rs) — already the layered-config home per its module docstring; all 5 binaries import `rio_common::config::load`.

## Entry criteria

- [P0416](plan-0416-validate-config-four-crate-extraction.md) merged (**DONE** — `validate_config()` exists in all 5 crates with the 10 `is_empty()` sites)

## Tasks

### T1 — `refactor(common):` ensure_required helper in config.rs

MODIFY [`rio-common/src/config.rs`](../../rio-common/src/config.rs). Add after `load()` at ~`:90`:

```rust
/// Validate a required string config field is non-empty (after trim).
///
/// Returns `Ok(())` if `value.trim()` is non-empty, `Err(anyhow)` with
/// a standardized message otherwise.
///
/// DRYs the 10× identical `ensure!(!field.is_empty(), "X is required
/// (set --flag, RIO_ENV, or crate.toml)")` template spread across 5
/// crates' `validate_config()` (P0416 spread it 4×; P0425
/// consolidates + adds trim).
///
/// The trim catches `RIO_FOO="  "` whitespace-typo — pre-helper, bare
/// `is_empty()` accepted it, startup failed with a cryptic tcp-connect
/// "invalid socket address syntax" buried in logs (rev-p416 correctness
/// finding).
///
/// # Arguments
///
/// - `value` — the Config field's current value
/// - `field` — user-visible field name (e.g., `"scheduler_addr"`)
/// - `flag` — CLI flag form (e.g., `"--scheduler-addr"`)
/// - `env` — env var form (e.g., `"RIO_SCHEDULER_ADDR"`)
/// - `component` — the TOML filename stem (e.g., `"gateway"` → `gateway.toml`)
pub fn ensure_required(
    value: &str,
    field: &str,
    flag: &str,
    env: &str,
    component: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty(),
        "{field} is required (set {flag}, {env}, or {component}.toml)"
    );
    Ok(())
}
```

**Alternative signature (fewer args, convention-based derivation):**

```rust
/// Same as above but derives flag/env from `field` by convention:
/// field `"scheduler_addr"` → flag `"--scheduler-addr"`, env `"RIO_SCHEDULER_ADDR"`.
/// Shorter at call-sites; loses the ability to customize per-field naming.
pub fn ensure_required_conv(
    value: &str,
    field: &str,
    component: &str,
) -> anyhow::Result<()> {
    let flag = format!("--{}", field.replace('_', "-"));
    let env = format!("RIO_{}", field.to_uppercase());
    anyhow::ensure!(
        !value.trim().is_empty(),
        "{field} is required (set {flag}, {env}, or {component}.toml)"
    );
    Ok(())
}
```

**Prefer the convention-based form** — all 10 sites follow the `field_name` → `--field-name` → `RIO_FIELD_NAME` convention. Zero customization needed today. Implementer's call.

### T2 — `refactor(workspace):` migrate 10 validate_config sites to ensure_required

MODIFY each of the 5 crates' `main.rs` `validate_config()`:

```rust
// BEFORE (rio-scheduler/src/main.rs:180-186)
anyhow::ensure!(
    !cfg.store_addr.is_empty(),
    "store_addr is required (set --store-addr, RIO_STORE_ADDR, or scheduler.toml)"
);
anyhow::ensure!(
    !cfg.database_url.is_empty(),
    "database_url is required (set --database-url, RIO_DATABASE_URL, or scheduler.toml)"
);

// AFTER
use rio_common::config::ensure_required_conv as required;
required(&cfg.store_addr, "store_addr", "scheduler")?;
required(&cfg.database_url, "database_url", "scheduler")?;
```

Same transform for gateway (`:167-179`, 4 sites), controller (`:134`, 1 site), store (`:181`, 1 site), worker (`:34-38`, 2 sites).

Net: 10×(4-line ensure) → 10×(1-line call) = ~30L removed, 1 helper added = ~15L, net -15L + correctness fix.

### T3 — `test(common):` ensure_required trim behavior

MODIFY [`rio-common/src/config.rs`](../../rio-common/src/config.rs) `cfg(test)` mod (or add one). Test the trim:

```rust
#[test]
fn ensure_required_rejects_whitespace_only() {
    let err = ensure_required_conv("   ", "scheduler_addr", "gateway")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("scheduler_addr is required"),
        "whitespace-only must be rejected as empty, got: {err}"
    );
}

#[test]
fn ensure_required_accepts_leading_trailing_whitespace_around_value() {
    // "  http://foo  " → trim → "http://foo" non-empty → OK
    // (The trimmed value is NOT returned — validation only; caller
    // may want to trim separately before using. This helper proves
    // a real value exists; gRPC connect will still see the padded
    // string. Consider returning the trimmed value if the latter
    // becomes an observed problem.)
    ensure_required_conv("  http://foo  ", "addr", "gateway")
        .expect("padded-but-nonempty should pass");
}

#[test]
fn ensure_required_conv_derives_flag_and_env() {
    let err = ensure_required_conv("", "database_url", "store")
        .unwrap_err()
        .to_string();
    assert!(err.contains("--database-url"), "flag derived by convention");
    assert!(err.contains("RIO_DATABASE_URL"), "env derived by convention");
    assert!(err.contains("store.toml"), "toml filename from component");
}
```

### T4 — `test(workspace):` per-crate whitespace-rejection test

MODIFY each crate's `validate_config` test section. Add one test per crate proving a whitespace-only required field is rejected:

```rust
// rio-scheduler/src/main.rs — after existing config_rejects_* tests at :930+
#[test]
fn config_rejects_whitespace_store_addr() {
    let mut cfg = test_valid_config();
    cfg.store_addr = "   ".into();
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(
        err.contains("store_addr is required"),
        "whitespace-only store_addr must be rejected as empty, got: {err}"
    );
}
```

One such test per crate (5 tests total) — proves T2's migration preserved validation AND added trim. Pre-helper, this test would PASS validation (whitespace-only `!is_empty()` is true) — so the test is a regression guard against accidentally reverting to bare `is_empty()`.

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'fn ensure_required' rio-common/src/config.rs` → ≥1 hit (T1: helper defined)
- `grep 'trim().is_empty()' rio-common/src/config.rs` → ≥1 hit inside ensure_required body (T1: trim applied)
- `grep 'is required (set --' rio-scheduler/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-store/src/main.rs rio-worker/src/main.rs` → 0 hits (T2: all 10 inline templates replaced by helper call)
- `grep 'ensure_required\|required(&cfg\.' rio-scheduler/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-store/src/main.rs rio-worker/src/main.rs` → ≥10 hits (T2: 10 call-sites)
- `cargo nextest run -p rio-common ensure_required` → ≥3 passed (T3)
- `cargo nextest run config_rejects_whitespace` → 5 passed (T4: one per crate)
- **T4 mutation:** revert T1's `trim()` (use bare `is_empty()`) → all 5 `config_rejects_whitespace_*` tests FAIL (whitespace passes bare is_empty check). Proves trim is load-bearing.
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'validate_config or standing_guard'` → all pass (T2 didn't break P0412's standing-guard tests)

## Tracey

No markers. Config validation hardening; not spec-behavior. The "X is required" message is an operator-facing diagnostic, not a protocol contract.

## Files

```json files
[
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "T1: +ensure_required/_conv helper after load() ~:90. T3: +3 ensure_required tests in cfg(test) mod"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T2: validate_config :180-186 — 2 ensures → 2 required() calls. T4: +config_rejects_whitespace_store_addr after :930"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T2: validate_config :167-179 — 4 ensures → 4 required() calls. T4: +config_rejects_whitespace_scheduler_addr"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T2: validate_config :134 — 1 ensure → 1 required() call. T4: +config_rejects_whitespace_scheduler_addr"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T2: validate_config :181 — 1 ensure → 1 required() call. T4: +config_rejects_whitespace_database_url"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T2: validate_config :34-38 — 2 ensures → 2 required() calls. T4: +config_rejects_whitespace_scheduler_addr"}
]
```

```
rio-common/src/
└── config.rs        # T1+T3: ensure_required helper + tests
rio-scheduler/src/
└── main.rs          # T2+T4: 2 migrations + whitespace test
rio-gateway/src/
└── main.rs          # T2+T4: 4 migrations + whitespace test
rio-controller/src/
└── main.rs          # T2+T4: 1 migration + whitespace test
rio-store/src/
└── main.rs          # T2+T4: 1 migration + whitespace test
rio-worker/src/
└── main.rs          # T2+T4: 2 migrations + whitespace test
```

## Dependencies

```json deps
{"deps": [416], "soft_deps": [409, 412, 415, 424, 304], "note": "HARD-dep P0416 (DONE — validate_config() exists in all 5 crates with the 10 is_empty() sites; IRONY: P0416 spread the pattern this plan DRYs; discovered_from=416). Soft-dep P0409 (DONE — scheduler validate_config() + test pattern originated here). Soft-dep P0412 (DONE — standing-guard config tests in 4 crates; T2 touches the same validate_config bodies they assert against — non-breaking, the roundtrip tests check deserialization not validation messages). Soft-dep P0415 (DONE — RetryPolicy f64 bounds; different ensures in same validate_config body, non-overlapping). Soft-dep P0424 (cpu_limit_cores validation — also touches scheduler main.rs validate_config / :384 loop; additive different-line, non-overlapping). Soft-dep P0304-T197/T198 (hoist standing-guard rationale to rio-common/src/config.rs module-doc — T1 here adds a fn AFTER the module docstring; T197/T198 edit the docstring; non-overlapping). COLLISION: main.rs files are HOT (scheduler=38, store=32, controller=26, gateway=20, worker=41). T2 replaces 2-4 line blocks in validate_config() — localized, low semantic conflict. All 5 crates touched — expect class-3 rebuild (crane rehashes all of them)."}
```

**Depends on:** [P0416](plan-0416-validate-config-four-crate-extraction.md) — the 10 `is_empty()` sites this plan DRYs.

**Conflicts with:** [`rio-common/src/config.rs`](../../rio-common/src/config.rs) low-traffic — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T197/T198 add module-doc paragraphs; T1 here adds a fn after `load()`. Non-overlapping. All 5 `main.rs` files — T2 edits `validate_config()` bodies; [P0424](plan-0424-sizeclassconfig-cpu-limit-validation.md) adds `cpu_limit_cores` ensure in scheduler's `:384` loop; [P0304](plan-0304-trivial-batch-p0222-harness.md)-T189 moves scheduler's `cutoff_secs` ensure into `validate_config()`. All additive-in-same-region; rebase-clean.
