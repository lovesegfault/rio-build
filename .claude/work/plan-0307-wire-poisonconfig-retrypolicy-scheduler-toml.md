# Plan 0307: Wire PoisonConfig + RetryPolicy to scheduler.toml

[P0219](plan-0219-per-worker-failure-budget.md) shipped `PoisonConfig` at [`derivation.rs:545`](../../rio-scheduler/src/state/derivation.rs) and `RetryPolicy` at [`worker.rs:158`](../../rio-scheduler/src/state/worker.rs), both with `::default()` as the only construction path. The spec at [`scheduler.md:121`](../../docs/src/components/scheduler.md) explicitly promises TOML configurability: "Both knobs are configurable via `scheduler.toml`: `poison_threshold` (default 3, current POISON_THRESHOLD), `require_distinct_workers` (default true — HashSet semantics; false = any N failures poison, for single-worker dev deployments)." But [`actor/mod.rs:238-239`](../../rio-scheduler/src/actor/mod.rs) hardcodes `RetryPolicy::default()` and `PoisonConfig::default()`, and the `with_poison_config` builder at [`actor/mod.rs:310`](../../rio-scheduler/src/actor/mod.rs) is never called from `main.rs`.

The spec-vs-code gap: `r[sched.retry.per-worker-budget]` claims both knobs are TOML-configurable. They are not. The `size_classes` field in `main.rs` ([`main.rs:42`](../../rio-scheduler/src/main.rs)) is the pattern to follow: `Vec<SizeClassConfig>` in the `Config` struct, `#[serde(default)]`, loaded via `rio_common::config::load("scheduler", cli)` at [`main.rs:152`](../../rio-scheduler/src/main.rs).

## Entry criteria

- [P0219](plan-0219-per-worker-failure-budget.md) merged (`PoisonConfig` + `RetryPolicy` structs exist) — **DONE**

## Tasks

### T1 — `feat(scheduler):` add poison + retry fields to Config struct

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) — `struct Config` near `:42`:

```rust
/// Poison-derivation thresholds. See scheduler.toml [[poison]] table.
/// r[sched.retry.per-worker-budget] — scheduler.md:121 specifies these
/// as TOML-configurable.
#[serde(default)]
poison: rio_scheduler::PoisonConfig,
/// Per-worker retry backoff. See scheduler.toml [[retry]] table.
#[serde(default)]
retry: rio_scheduler::RetryPolicy,
```

`PoisonConfig` and `RetryPolicy` need `#[derive(Deserialize)]` — they may already have it from P0219; check at dispatch. If not, add `serde::Deserialize` to their derive lists in [`derivation.rs:545`](../../rio-scheduler/src/state/derivation.rs) and [`worker.rs:158`](../../rio-scheduler/src/state/worker.rs). Both already `impl Default`, so `#[serde(default)]` works.

Also update the `impl Default for Config` block near `:79` — add `poison: Default::default()` and `retry: Default::default()`.

### T2 — `feat(scheduler):` thread config → actor builder

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) — wherever the actor is constructed (find `SchedulerActor::new` or the builder chain; likely after `cfg.size_classes` is consumed). Add:

```rust
// r[impl sched.retry.per-worker-budget]
// scheduler.md:121 — "Both knobs are configurable via scheduler.toml".
// P0219 shipped the structs and the builder; this wires them.
.with_poison_config(cfg.poison)
.with_retry_policy(cfg.retry)  // builder may not exist — check actor/mod.rs; add if needed
```

If `with_retry_policy` doesn't exist at [`actor/mod.rs`](../../rio-scheduler/src/actor/mod.rs), add it alongside `with_poison_config` at `:310`:

```rust
pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
    self.retry_policy = policy;
    self
}
```

### T3 — `test(scheduler):` TOML round-trip

NEW test in [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) `#[cfg(test)]` block (or wherever config tests live — check `config_defaults_are_stable` from P0097 for the idiom):

```rust
// r[verify sched.retry.per-worker-budget]
#[test]
fn poison_and_retry_load_from_toml() {
    let toml = r#"
        [poison]
        poison_threshold = 5
        require_distinct_workers = false

        [retry]
        initial_backoff_ms = 500
        max_backoff_ms = 30000
    "#;  // field names match struct — verify at impl
    let cfg: Config = figment::Figment::new()
        .merge(figment::providers::Toml::string(toml))
        .extract()
        .expect("toml parses");
    assert_eq!(cfg.poison.poison_threshold, 5);
    assert!(!cfg.poison.require_distinct_workers);
    // + retry field assertions
}

#[test]
fn poison_and_retry_default_when_absent() {
    // Empty TOML → #[serde(default)] → struct Default impl.
    let cfg: Config = figment::Figment::new()
        .merge(figment::providers::Toml::string(""))
        .extract()
        .expect("empty toml parses");
    assert_eq!(cfg.poison, PoisonConfig::default());
    assert_eq!(cfg.retry, RetryPolicy::default());
}
```

### T4 — `docs:` scheduler.toml example snippet

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — near `:121` where the spec claims TOML configurability, add a fenced example:

```toml
[poison]
poison_threshold = 3          # failures before derivation is poisoned
require_distinct_workers = true  # HashSet: 3 DISTINCT workers must fail

[retry]
# ... (match RetryPolicy field names)
```

Placement: after the `r[sched.retry.per-worker-budget]` paragraph, before `r[sched.admin.list-workers]`.

### T5 — `test(scheduler):` figment::Jail standing-guard — catch the NEXT orphan

Port the Jail test pattern from [`rio-store/src/main.rs:594-667`](../../rio-store/src/main.rs) as a standing guard so the next `with_X` builder added without a `Config` field is a **failing test**, not a silent orphan.

**The orphan pattern this guards against:** P0219 shipped `PoisonConfig` + `with_poison_config` builder, zero TOML side. The struct exists, the builder exists, `main.rs` never calls the builder from config. `config_defaults_are_stable` at [`main.rs:688`](../../rio-scheduler/src/main.rs) is structurally blind — it only checks `Config`-struct fields; if the field isn't ON `Config`, the test doesn't know to miss it. [P0218](plan-0218-store-nar-budget-wiring.md) added the store-side equivalent **with** Jail tests at store/main.rs:640,658 proving TOML→Config→builder. Scheduler got the builder, not the guard.

**Proof scheduler can use Jail:** [`lease/mod.rs:570`](../../rio-scheduler/src/lease/mod.rs) already does `figment::Jail::expect_with(|jail| { jail.set_env(...); ... })` — no missing dependency.

NEW tests in [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) `#[cfg(test)]` block (after T3's tests):

```rust
/// Standing guard: TOML → Config → Default-struct roundtrip for EVERY
/// sub-config. If someone adds `with_foo_config` without adding
/// `Config.foo`, the absent-key test FAILS when they try to extend
/// this list (forcing them to notice), and the all-keys test FAILS
/// if figment can't deserialize the field (wrong serde attrs etc).
///
/// Pattern from rio-store/src/main.rs:640 (P0218).
/// `#[allow(clippy::result_large_err)]` — figment::Error is 208B, API-fixed.
#[test]
#[allow(clippy::result_large_err)]
fn all_subconfigs_roundtrip_toml() {
    figment::Jail::expect_with(|jail| {
        // Every sub-config table with at least one non-default value.
        // When you add Config.newfield: ADD IT HERE or this test's
        // doc-comment is a lie.
        jail.create_file("scheduler.toml", r#"
            [poison]
            poison_threshold = 7

            [retry]
            initial_backoff_ms = 333
        "#)?;
        let cfg: Config = rio_common::config::load("scheduler", CliArgs::default()).unwrap();
        assert_eq!(cfg.poison.poison_threshold, 7);
        assert_eq!(cfg.retry.initial_backoff_ms, 333);  // field name: verify at impl
        Ok(())
    });
}

/// Empty scheduler.toml → every sub-config gets its Default impl.
/// If Config.foo is added WITHOUT #[serde(default)], this fails
/// with a figment missing-field error — catches the "new required
/// field breaks existing deployments" mode.
#[test]
#[allow(clippy::result_large_err)]
fn all_subconfigs_default_when_absent() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("scheduler.toml", r#"listen_addr = "0.0.0.0:9001""#)?;
        let cfg: Config = rio_common::config::load("scheduler", CliArgs::default()).unwrap();
        assert_eq!(cfg.poison, PoisonConfig::default());
        assert_eq!(cfg.retry, RetryPolicy::default());
        // size_classes: already covered by config_defaults_are_stable
        Ok(())
    });
}
```

**Why bundled here, not standalone:** no point adding Jail tests before the wiring exists — T1-T2 create `Config.poison`/`Config.retry`, T5 then proves they roundtrip. A standalone plan would have nothing to assert against. The consolidator's followup explicitly says "Worth it if: bundled into the already-proposed PoisonConfig+RetryPolicy wiring plan."

**Overlap with T3:** T3's `poison_and_retry_load_from_toml` tests the SPECIFIC fields. T5's `all_subconfigs_roundtrip_toml` is the GENERIC pattern — both are valuable; T3 is the unit test, T5 is the "extend-this-when-you-add-a-field" convention carrier. Keep both, OR collapse T3 into T5 at impl time if the duplication feels heavy (~30 lines per test, 2-3 tests total).

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule sched.retry.per-worker-budget` shows impl + verify (T2's `r[impl]`, T3's `r[verify]`)
- `grep '[poison]' docs/src/components/scheduler.md` → ≥1 hit (T4 TOML example)
- `grep 'with_poison_config\|with_retry_policy' rio-scheduler/src/main.rs` → ≥2 hits (both wired)
- `cargo nextest run -p rio-scheduler poison_and_retry` → 2 tests pass
- `cargo nextest run -p rio-scheduler all_subconfigs` → 2 tests pass (T5: Jail roundtrip + default-when-absent)
- `grep -c 'figment::Jail::expect_with' rio-scheduler/src/main.rs` ≥ 2 (T5: store had 4; scheduler gets at least 2)

## Tracey

References existing markers:
- `r[sched.retry.per-worker-budget]` — T2 implements (the TOML-configurability half), T3 verifies. The marker at [`scheduler.md:110`](../../docs/src/components/scheduler.md) already specifies both the infra-failure-doesn't-count behavior (P0219's impl) AND the TOML configurability (this plan's impl).

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: Config struct poison+retry fields; T2: wire to actor builder; T3: TOML round-trip tests; T5: Jail standing-guard tests (all_subconfigs_*)"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T2: with_retry_policy builder (if absent — check :310 neighborhood)"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T1: PoisonConfig derive(Deserialize) if absent"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "T1: RetryPolicy derive(Deserialize) if absent"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: TOML example snippet near :121"}
]
```

```
rio-scheduler/src/
├── main.rs                       # T1: Config fields; T2: wire; T3+T5: tests
├── actor/mod.rs                  # T2: with_retry_policy (maybe)
└── state/
    ├── derivation.rs             # T1: derive(Deserialize) (maybe)
    └── worker.rs                 # T1: derive(Deserialize) (maybe)
docs/src/components/scheduler.md  # T4: TOML example
```

## Dependencies

```json deps
{"deps": [219], "soft_deps": [218], "note": "P0219 shipped PoisonConfig+RetryPolicy+with_poison_config builder; this wires them to scheduler.toml per spec promise at scheduler.md:121. size_classes pattern (main.rs:42). T5 ports the figment::Jail standing-guard pattern from P0218's store/main.rs:640 — consolidator followup explicitly says bundle-here-not-standalone. discovered_from=P0219."}
```

**Depends on:** [P0219](plan-0219-per-worker-failure-budget.md) — merged (DONE). `PoisonConfig`/`RetryPolicy` structs + `with_poison_config` builder exist.
**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) count=29; [`actor/mod.rs`](../../rio-scheduler/src/actor/mod.rs) count=34 — both hot. But this is a small additive change (new struct fields, new builder call); low semantic conflict risk. [`scheduler.md`](../../docs/src/components/scheduler.md) count=18 — TOML snippet is append-only near `:125`, non-overlapping with marker edits.
