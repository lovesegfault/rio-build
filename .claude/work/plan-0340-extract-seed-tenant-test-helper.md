# Plan 0340: Extract `seed_tenant` test helper — 25 INSERT INTO tenants copies

Consolidator count at scan time was 18 copies across 6 files; a fresh grep finds **25** across 9 — the number grew since the scan. Every integration test that needs a tenant row hand-rolls `sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")`. Three variants coexist: name-only (`RETURNING tenant_id`), name+`gc_retention_hours` (gc/mark.rs tests), name+`cache_token` (cache_server tests). [P0272](plan-0272-per-tenant-narinfo-filter.md) added 4 more for narinfo filtering; [P0338](plan-0338-tenant-signer-wiring-putpath.md) will add more when it wires `TenantSigner` into `PutPath`.

The duplication isn't dangerous (it's SQL — no logic to drift) but it's noise that hides what each test actually cares about. A test at [`cache_server/auth.rs:199`](../../rio-store/src/cache_server/auth.rs) that says `sqlx::query("INSERT INTO tenants (tenant_name, cache_token) VALUES ('team-a', 'secret')")` takes three lines to set up one fact: "tenant team-a has token 'secret'". The `tenant_id` return value goes unused in 8 of the 25 sites.

Sequenced AFTER P0338 so the extraction sweeps P0338's additions in the same pass — one migration, not two.

## Entry criteria

- [P0338](plan-0338-tenant-signer-wiring-putpath.md) merged (adds its own `INSERT INTO tenants` sites in rio-store signing tests; extraction sweeps them)

## Tasks

### T1 — `feat(test-support):` seed_tenant helper + builder

NEW pub functions in [`rio-test-support/src/pg.rs`](../../rio-test-support/src/pg.rs) — add after the `TestDb` impl block (around `:390`, before `impl Drop`):

```rust
/// Seed a tenant row and return its UUID. The simplest case: just
/// a name. Covers ~60% of call sites (grep says 15/25).
///
/// For tests that need `gc_retention_hours`, `gc_max_store_bytes`,
/// or `cache_token`, use [`TenantSeed`] instead.
pub async fn seed_tenant(pool: &PgPool, name: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seed_tenant INSERT failed")
}

/// Builder for the non-trivial seed cases. Every field except `name`
/// is a column default (NULL or the schema default). Chain `.with_*`
/// for the columns the test actually cares about.
///
/// ```ignore
/// let tid = TenantSeed::new("gc-test")
///     .with_retention_hours(168)
///     .seed(&db.pool).await;
/// ```
#[derive(Debug)]
pub struct TenantSeed {
    name: String,
    gc_retention_hours: Option<i32>,
    gc_max_store_bytes: Option<i64>,
    cache_token: Option<String>,
}

impl TenantSeed {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gc_retention_hours: None,
            gc_max_store_bytes: None,
            cache_token: None,
        }
    }

    pub fn with_retention_hours(mut self, hours: i32) -> Self {
        self.gc_retention_hours = Some(hours);
        self
    }

    pub fn with_max_store_bytes(mut self, bytes: i64) -> Self {
        self.gc_max_store_bytes = Some(bytes);
        self
    }

    pub fn with_cache_token(mut self, token: impl Into<String>) -> Self {
        self.cache_token = Some(token.into());
        self
    }

    /// INSERT and return `tenant_id`. Columns not `.with_*`'d take
    /// their schema default (NULL for optional fields).
    pub async fn seed(self, pool: &PgPool) -> uuid::Uuid {
        sqlx::query_scalar(
            "INSERT INTO tenants (tenant_name, gc_retention_hours, gc_max_store_bytes, cache_token) \
             VALUES ($1, $2, $3, $4) RETURNING tenant_id",
        )
        .bind(self.name)
        .bind(self.gc_retention_hours)
        .bind(self.gc_max_store_bytes)
        .bind(self.cache_token)
        .fetch_one(pool)
        .await
        .expect("TenantSeed INSERT failed")
    }
}
```

Also add to `rio-test-support/src/lib.rs`:
```rust
pub use pg::{seed_tenant, TenantSeed};
```

**Why a builder, not a flat-args fn:** the grep shows 3 distinct column sets today (`name`, `name+retention`, `name+token`). A 4-arg fn with `Option` defaults works but reads badly at call sites — `seed_tenant(pool, "x", None, None, Some("token"))` vs `TenantSeed::new("x").with_cache_token("token").seed(pool)`. The builder makes the columns-that-matter visible. The bare `seed_tenant()` covers the common case without builder noise.

**Why `.expect()` not `-> Result`:** every existing call site `.unwrap()`s. Tests want to panic on seed failure — there's no useful recovery and `?`-bubbling just obscures the failing line.

### T2 — `refactor(store):` migrate rio-store call sites

MODIFY across `rio-store/`:

| File | Lines | Count | Variant |
|---|---|---|---|
| [`signing.rs`](../../rio-store/src/signing.rs) | `:475`, `:548` | 2 | name-only → `seed_tenant` |
| [`gc/mark.rs`](../../rio-store/src/gc/mark.rs) | `:472`, `:520`, `:527`, `:589` | 4 | `:472/:520/:527` name+retention → `TenantSeed::new().with_retention_hours()`; `:589` name-only |
| [`metadata/tenant_keys.rs`](../../rio-store/src/metadata/tenant_keys.rs) | `:64` | 1 | name-only |
| [`metadata/queries.rs`](../../rio-store/src/metadata/queries.rs) | `:375`, `:381` | 2 | name-only |
| [`cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) | `:199`, `:224`, `:250`, `:284`, `:319`, `:353`, `:382` | 7 | name+cache_token → `TenantSeed::new().with_cache_token()` |
| [`cache_server/mod.rs`](../../rio-store/src/cache_server/mod.rs) | `:585`, `:780`, `:787` | 3 | name+cache_token |

Plus any sites P0338 adds (grep at dispatch time; the P0338 plan doc doesn't spell out line numbers for its test seeds).

Add `use rio_test_support::{seed_tenant, TenantSeed};` to each file's `#[cfg(test)] mod tests` use block. The `seed_tenant` import may go unused in files that only need `TenantSeed` (cache_server) — allow it or split the imports per-file; implementer's call.

### T3 — `refactor(scheduler):` migrate rio-scheduler call sites

MODIFY across `rio-scheduler/`:

| File | Lines | Count |
|---|---|---|
| [`admin/tests.rs`](../../rio-scheduler/src/admin/tests.rs) | `:979`, `:1029`, `:1034` | 3 |
| [`grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) | `:468` | 1 |
| [`actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) | `:1003`, `:1008` | 2 |

All name-only → `seed_tenant`. Same `use` addition.

**Do NOT touch [`db.rs:348`](../../rio-scheduler/src/db.rs)** — that's the production `create_tenant` RPC handler, not test setup. It takes all four columns because the admin API accepts them. Leave it.

### T4 — `test(test-support):` smoke test for the builder

NEW test in [`rio-test-support/src/pg.rs`](../../rio-test-support/src/pg.rs) `#[cfg(test)]`:

```rust
/// TenantSeed: columns not .with_*()'d take their schema default.
/// One concrete check: gc_retention_hours NULL when unset, set when
/// .with_retention_hours() chained. Proves the Option-bind plumbing.
#[tokio::test]
async fn tenant_seed_optional_columns() {
    // Use rio-scheduler's migrations — tenants lives in the scheduler schema.
    let db = TestDb::new(&rio_scheduler::MIGRATOR).await;

    let bare = seed_tenant(&db.pool, "bare").await;
    let full = TenantSeed::new("full")
        .with_retention_hours(168)
        .with_cache_token("sekrit")
        .seed(&db.pool)
        .await;

    let (bare_retention, bare_token): (Option<i32>, Option<String>) =
        sqlx::query_as("SELECT gc_retention_hours, cache_token FROM tenants WHERE tenant_id = $1")
            .bind(bare)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(bare_retention, None);
    assert_eq!(bare_token, None);

    let (full_retention, full_token): (Option<i32>, Option<String>) =
        sqlx::query_as("SELECT gc_retention_hours, cache_token FROM tenants WHERE tenant_id = $1")
            .bind(full)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(full_retention, Some(168));
    assert_eq!(full_token.as_deref(), Some("sekrit"));
}
```

**Caveat:** this test makes `rio-test-support` depend on `rio-scheduler` (for `MIGRATOR`). Check at dispatch if that's a cycle — `rio-scheduler` already `dev-depends` on `rio-test-support`. If it is, put the test in `rio-scheduler/src/db.rs` `#[cfg(test)]` instead (same assertions, lives where the migrations already are).

## Exit criteria

- `/nixbuild .#ci` green
- `grep -rn 'INSERT INTO tenants' rio-store/src/ rio-scheduler/src/ --include='*.rs' | grep -v 'db.rs:' | wc -l` → 0 (all test-side copies migrated; `db.rs` production handler exempt)
- `grep -c 'pub async fn seed_tenant\|pub struct TenantSeed' rio-test-support/src/pg.rs` → 2 (both exported)
- `grep -c 'seed_tenant\|TenantSeed' rio-store/src/cache_server/auth.rs` ≥ 7 (all 7 sites migrated)
- `grep -c 'seed_tenant' rio-scheduler/src/admin/tests.rs` ≥ 3 (three migrated)
- `cargo nextest run -p rio-store cache_server::` — all auth tests pass (behavior preserved)
- `cargo nextest run -p rio-scheduler admin::tests::` — all admin tests pass
- `cargo nextest run tenant_seed_optional_columns` → 1 passed (T4: Option-bind plumbing)
- If P0338's additions exist at dispatch: `grep 'INSERT INTO tenants' rio-store/src/signing.rs` → 0 (P0338's new sites also swept)

## Tracey

No markers. `rio-test-support` has no spec domain — it's test plumbing, not shipped behavior. The `tenants` table schema is spec'd under `r[sched.tenant.resolve]` at [`scheduler.md:91`](../../docs/src/components/scheduler.md) but this plan doesn't change the schema, just how tests seed it.

## Files

```json files
[
  {"path": "rio-test-support/src/pg.rs", "action": "MODIFY", "note": "T1: +seed_tenant fn + TenantSeed builder ~:390; T4: +tenant_seed_optional_columns test (OR relocate to scheduler if dep-cycle)"},
  {"path": "rio-test-support/src/lib.rs", "action": "MODIFY", "note": "T1: pub use pg::{seed_tenant, TenantSeed}"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T2: migrate :475 :548 + P0338's additions (grep at dispatch)"},
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "T2: migrate :472 :520 :527 (with_retention_hours) + :589 (bare)"},
  {"path": "rio-store/src/metadata/tenant_keys.rs", "action": "MODIFY", "note": "T2: migrate :64"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "T2: migrate :375 :381"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T2: migrate 7 sites :199-:382 (all with_cache_token)"},
  {"path": "rio-store/src/cache_server/mod.rs", "action": "MODIFY", "note": "T2: migrate :585 :780 :787 (with_cache_token)"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "T3: migrate :979 :1029 :1034"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T3: migrate :468"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T3: migrate :1003 :1008"}
]
```

```
rio-test-support/src/
├── pg.rs                          # T1: +seed_tenant +TenantSeed, T4: +test
└── lib.rs                         # T1: re-export
rio-store/src/
├── signing.rs                     # T2: 2+ sites
├── gc/mark.rs                     # T2: 4 sites
├── metadata/{tenant_keys,queries}.rs  # T2: 3 sites
└── cache_server/{auth,mod}.rs     # T2: 10 sites
rio-scheduler/src/
├── admin/tests.rs                 # T3: 3 sites
├── grpc/tests.rs                  # T3: 1 site
└── actor/tests/completion.rs      # T3: 2 sites
```

## Dependencies

```json deps
{"deps": [338], "soft_deps": [272], "note": "HARD dep P0338: coordinator explicit guidance — sequence AFTER TenantSigner wiring so the extraction sweeps P0338's new INSERT sites in the same pass. One migration not two. P0272 soft-dep: merged already (added 4 of the 25 sites); discovered_from. Wide file-fan (11 files) but all are test-only sections in #[cfg(test)] — sed-shallow, low semantic-conflict risk. signing.rs is hot (P0338 touches it) — the P0338 dep serializes."}
```

**Depends on:** [P0338](plan-0338-tenant-signer-wiring-putpath.md) — adds its own `INSERT INTO tenants` test seeds when it wires `sign_for_tenant` into `PutPath`. Sweeping them here avoids a second migration pass. P0338 depends on P0256+P0259 (both jwt infrastructure); this transitively inherits those.

**Conflicts with:** [`grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) count=13; [`signing.rs`](../../rio-store/src/signing.rs) touched by P0338 (dep serializes). `rio-test-support/src/pg.rs` is NOT in collisions-top-50 (grpc.rs is at 18, pg.rs is cold). `cache_server/auth.rs` and `gc/mark.rs` are cold. 11-file fan is wide but each edit is a test-mod-only `sqlx::query_scalar` → `seed_tenant` one-liner swap.
