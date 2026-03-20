# Plan 0412: Standing-guard config tests — spread P0307 pattern to four crates

consol-mc220 finding. [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) landed `all_subconfigs_roundtrip_toml` + `all_subconfigs_default_when_absent` at [`rio-scheduler/src/main.rs:875-935`](../../rio-scheduler/src/main.rs) — the `figment::Jail` standing-guard pair that catches the NEXT config orphan (a `with_foo_config` builder added without a `Config.foo` field is a failing test, not a silent no-op). The doc-comment at [`main.rs:847-860`](../../rio-scheduler/src/main.rs) explicitly cites the P0219-class failure mode: `PoisonConfig` + `with_poison_config` shipped, zero TOML wiring, `config_defaults_are_stable` STRUCTURALLY BLIND (only checks `Config`-struct fields — if the field isn't there, the test doesn't know to miss it).

Four crates have `struct Config` + `rio_common::config::load("<component>", cli)` + a builder chain but LACK the standing-guard pair:

| Crate | Config at | `load()` at | Existing Jail tests | Has `all_subconfigs_*`? |
|---|---|---|---|---|
| `rio-store` | [`main.rs:50`](../../rio-store/src/main.rs) | `:185` | 4× at `:634,:653,:680,:698` (chunk_backend env + nar_budget per-field roundtrip) | NO |
| `rio-gateway` | [`main.rs:23`](../../rio-gateway/src/main.rs) | check at dispatch | none | NO |
| `rio-controller` | [`main.rs:27`](../../rio-controller/src/main.rs) | `:135` | none | NO |
| `rio-worker` | `config.rs` (separate file) | [`main.rs:36`](../../rio-worker/src/main.rs) | none | NO |

`rio-store` has PER-FIELD Jail roundtrips (P0218's `nar_buffer_budget_toml_roundtrip` + env-var tagged-enum tests) but NOT the standing-guard "every sub-table proven-wired + empty-toml proves-defaults" pair. The distinction: per-field tests prove a specific field works; the standing-guard pair proves the STRUCTURE is sound (no required-field surprise in deployments, no orphan builder).

~50L per crate (the pair + one banner comment). Pure-additive to each crate's existing `#[cfg(test)] mod tests`. [P0409](plan-0409-scheduler-config-validation-retry-poison.md) is non-colliding: it adds `ensure!` validation blocks in `main()`, this adds `#[test]` fns in the test mod.

## Entry criteria

- [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) merged (**DONE** — the pattern exists at [`rio-scheduler/src/main.rs:875`](../../rio-scheduler/src/main.rs))

## Tasks

### T1 — `test(store):` all_subconfigs_roundtrip + default_when_absent standing-guard pair

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs) `#[cfg(test)] mod tests` (near existing Jail tests at `:634-708`). Add the standing-guard pair:

```rust
// -----------------------------------------------------------------------
// figment::Jail standing-guard tests — catch the NEXT orphan.
// Per-field tests above prove individual keys; this pair proves STRUCTURE:
// every sub-config table wired + empty-toml defaults hold. See
// rio-scheduler/src/main.rs:847-935 for the pattern rationale + the
// P0219 failure mode that motivated it.
// -----------------------------------------------------------------------

/// Standing guard: TOML → Config roundtrip for EVERY sub-config table via
/// the REAL `rio_common::config::load` path. When you add `Config.newfield`:
/// ADD IT HERE or this test's doc-comment is a lie.
#[test]
#[allow(clippy::result_large_err)]
fn all_subconfigs_roundtrip_toml() {
    figment::Jail::expect_with(|jail| {
        // Every sub-config table with at least one NON-default value.
        jail.create_file(
            "store.toml",
            r#"
            nar_buffer_budget_bytes = 99999

            [chunk_backend]
            kind = "filesystem"
            base_dir = "/custom/path"
            "#,
        )?;
        let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
        assert_eq!(cfg.nar_buffer_budget_bytes, Some(99999));
        assert!(
            matches!(cfg.chunk_backend, ChunkBackendKind::Filesystem { .. }),
            "[chunk_backend] table must thread through figment"
        );
        Ok(())
    });
}

/// Empty store.toml → every sub-config gets its Default impl. Catches
/// "new required field breaks existing deployments" (figment missing-field).
#[test]
#[allow(clippy::result_large_err)]
fn all_subconfigs_default_when_absent() {
    figment::Jail::expect_with(|jail| {
        // listen_addr proves TOML IS loaded (empty vs missing file).
        jail.create_file("store.toml", r#"listen_addr = "0.0.0.0:9002""#)?;
        let cfg: Config = rio_common::config::load("store", CliArgs::default()).unwrap();
        assert_eq!(cfg.chunk_backend, ChunkBackendKind::default());
        assert!(cfg.nar_buffer_budget_bytes.is_none());
        Ok(())
    });
}
```

**Grep at dispatch:** `grep 'struct Config' rio-store/src/main.rs -A 50` → list every sub-config field (`chunk_backend`, `nar_buffer_budget_bytes`, `signing`, `gc`, …). Each needs one assert in BOTH tests.

### T2 — `test(gateway):` all_subconfigs_roundtrip + default_when_absent

MODIFY [`rio-gateway/src/main.rs`](../../rio-gateway/src/main.rs) `#[cfg(test)] mod tests` (create if absent). Gateway has `Config` at `:23` with multiple sub-fields (grep `#[serde]` → 10+ fields at `:117-152`).

Same pattern as T1. Gateway-specific sub-configs: `ratelimit` (P0213), `jwt`, `quota`, `ssh_listen` — grep at dispatch for the current `Config` field list.

### T3 — `test(controller):` all_subconfigs_roundtrip + default_when_absent

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) `#[cfg(test)] mod tests` (create if absent — `grep 'cfg(test)' rio-controller/src/main.rs`). Controller `Config` at `:27` — simpler (fewer sub-tables; mostly scaling timing + reconcile intervals).

### T4 — `test(worker):` all_subconfigs_roundtrip + default_when_absent

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) or [`rio-worker/src/config.rs`](../../rio-worker/src/config.rs) — whichever has the `#[cfg(test)]` mod. Worker `Config` is in `config.rs` (separate file); `main.rs:36` consumes. Check at dispatch where the existing config tests live.

Worker sub-configs: `fuse`, `executor`, `upload` timeouts — grep at dispatch.

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c nextest-standalone)
- `grep -c 'fn all_subconfigs_roundtrip_toml' rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/` → 4 (one per crate)
- `grep -c 'fn all_subconfigs_default_when_absent' rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/` → 4 (one per crate)
- `grep 'catch the NEXT orphan\|figment::Jail standing-guard' rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/` → ≥4 hits (banner comment in each; pointer to rio-scheduler pattern rationale)
- `cargo nextest run -p rio-store all_subconfigs` → 2 passed
- `cargo nextest run -p rio-gateway all_subconfigs` → 2 passed
- `cargo nextest run -p rio-controller all_subconfigs` → 2 passed
- `cargo nextest run -p rio-worker all_subconfigs` → 2 passed
- **Mutation (one crate suffices):** pick `rio-gateway`, delete one `#[serde(default)]` from a sub-struct field → `all_subconfigs_default_when_absent` fails with figment missing-field error. Proves the test is load-bearing for "new required field breaks deployments" class.

## Tracey

No new markers. The standing-guard pair is test-infrastructure, not spec-behavior. `r[sched.retry.per-worker-budget]` at [`scheduler.md:110`](../../docs/src/components/scheduler.md) is what P0307's scheduler-side guard serves; the four-crate spread has no per-crate spec-marker equivalent (it's the SAME abstraction-pattern applied to different `Config` structs, not a spec-mandated cross-crate invariant).

## Files

```json files
[
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T1: +all_subconfigs_roundtrip_toml + all_subconfigs_default_when_absent (~50L) after existing Jail tests at :708. HOT count=29"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T2: +standing-guard pair (~50L). Config at :23, 10+ serde fields :117-152. HOT count=17"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T3: +standing-guard pair (~50L). Config at :27, load at :135. HOT count=24"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T4: +standing-guard pair (~50L) — OR rio-worker/src/config.rs if cfg(test) mod lives there (check at dispatch). HOT count=39"}
]
```

```
rio-store/src/main.rs      # T1: +2 tests after :708
rio-gateway/src/main.rs    # T2: +2 tests
rio-controller/src/main.rs # T3: +2 tests
rio-worker/src/{main.rs|config.rs}  # T4: +2 tests
```

## Dependencies

```json deps
{"deps": [307], "soft_deps": [409, 218, 213], "note": "consol-mc220 (discovered_from=307). P0307 provides the pattern (rio-scheduler/src/main.rs:875-935). Soft-dep P0409 (config validation for retry.jitter_fraction/poison.threshold — touches rio-scheduler/src/main.rs ensure! block not the test mod; non-colliding, this plan doesn't touch scheduler). Soft-dep P0218 (store nar_budget TOML roundtrip — already landed, provides the rio-store :680 per-field test precedent). Soft-dep P0213 (gateway ratelimit config — if P0213 added gateway Config.ratelimit sub-struct, T2 asserts it). All four tasks are PURE-ADDITIVE to cfg(test) mods — no hunk overlap with any concurrent main.rs producer. rio-worker/src/main.rs HOT=39 is the highest; T4 is cfg(test)-only append, trivial_rebase:true always."}
```

**Depends on:** [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) — the pattern + rationale comment at scheduler `main.rs:847-860` that each crate's banner points at.

**Conflicts with:** `rio-worker/src/main.rs` count=39 is hot, but T4 is a `#[cfg(test)]` mod append — zero overlap with `main()` body or `Config` struct changes. `rio-store/src/main.rs` count=29 — same. Four-crate pure-additive = parallel-safe with any concurrent plan.
