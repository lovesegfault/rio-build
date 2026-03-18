# Plan 0213: gateway — per-tenant rate limiter + connection cap + RESOURCE_EXHAUSTED

Wave 2. Three pieces of backpressure: (1) per-tenant build-submit rate limiting via [`governor`](https://docs.rs/governor) keyed on `tenant_name`; (2) global connection cap via `Arc<Semaphore>`; (3) `RESOURCE_EXHAUSTED` gRPC code translation so sqlx pool-timeout surfaces as a retryable error instead of a generic failure.

**R-GOVN:** `DefaultKeyedRateLimiter` uses `dashmap`, no auto-eviction. Acceptable for 4b because `tenant_name` comes from the authorized_keys comment ([`server.rs:70`](../../rio-gateway/src/server.rs)) — operator-controlled, can't be forged by client, bounded keyspace. Document + `TODO(phase5)` for eviction if keys ever become client-controlled.

Closes `TODO(phase4b)` at [`rio-store/src/metadata/queries.rs:225`](../../rio-store/src/metadata/queries.rs). This is the **only Cargo.toml touch in 4b** (`governor` is the only new dep).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (markers `r[gw.rate.per-tenant]` and `r[gw.conn.cap]` exist in `gateway.md`)

## Tasks

### T1 — `feat(gateway):` add `governor` dependency

`Cargo.toml [workspace.dependencies]`: `governor = "0.6"`. `rio-gateway/Cargo.toml`: `governor.workspace = true`.

### T2 — `feat(gateway):` `TenantLimiter` — keyed rate limiter

NEW file `rio-gateway/src/ratelimit.rs`:

```rust
//! Per-tenant build-submit rate limiting.
//!
//! r[impl gw.rate.per-tenant]
//!
//! tenant_name is from the authorized_keys comment (server.rs:70) —
//! operator-controlled, cannot be forged by client. No key-eviction
//! needed (bounded keyspace). TODO(phase5): eviction if keys ever
//! become client-controlled.

use std::sync::Arc;
use governor::{DefaultKeyedRateLimiter, Quota};
use std::num::NonZeroU32;

pub struct TenantLimiter(Arc<DefaultKeyedRateLimiter<String>>);

impl TenantLimiter {
    pub fn new(per_minute: u32, burst: u32) -> Self {
        let quota = Quota::per_minute(NonZeroU32::new(per_minute).unwrap())
            .allow_burst(NonZeroU32::new(burst).unwrap());
        Self(Arc::new(DefaultKeyedRateLimiter::keyed(quota)))
    }

    /// None → key "__anon__".
    pub fn check(&self, tenant: Option<&str>) -> Result<(), governor::NotUntil<governor::clock::QuantaInstant>> {
        let key = tenant.unwrap_or("__anon__").to_string();
        self.0.check_key(&key)
    }
}
```

Default quota: `per_minute(10).allow_burst(30)`, configurable via `gateway.toml`.

### T3 — `feat(gateway):` hook rate limiter before `submit_build`

Hold `Arc<TenantLimiter>` on gateway shared state ([`server.rs`](../../rio-gateway/src/server.rs), same Arc passed to each session). At [`rio-gateway/src/handler/build.rs:235`](../../rio-gateway/src/handler/build.rs) before `submit_build`, read `ctx.tenant_name` (field at `server.rs:250`):

```rust
if let Err(not_until) = self.limiter.check(ctx.tenant_name.as_deref()) {
    let wait = not_until.wait_time_from(/* now */);
    stderr.error(format!(
        "rate limit: too many builds from tenant '{}' — wait ~{}s",
        ctx.tenant_name.as_deref().unwrap_or("anon"),
        wait.as_secs()
    )).await?;
    return Ok(());  // early return, DO NOT close connection
}
```

### T4 — `feat(gateway):` connection cap — `Arc<Semaphore>`

At [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs):

```rust
// r[impl gw.conn.cap]
let conn_sem = Arc::new(Semaphore::new(cfg.max_connections.unwrap_or(1000)));
```

In the accept loop, **before** spawning the session:

```rust
match conn_sem.clone().try_acquire_owned() {
    Ok(permit) => {
        // spawn session task, move permit in — dropped on disconnect
        tokio::spawn(async move {
            let _permit = permit;
            /* ... session ... */
        });
    }
    Err(_) => {
        // At cap — russh Disconnect::TooManyConnections before session spawn
    }
}
```

### T5 — `fix(store):` `RESOURCE_EXHAUSTED` — close `queries.rs:225` TODO

At [`rio-store/src/metadata/mod.rs:53`](../../rio-store/src/metadata/mod.rs) error enum: add `MetadataError::ResourceExhausted(String)` variant. Map sqlx pool-timeout → this.

At [`rio-store/src/grpc/mod.rs:84`](../../rio-store/src/grpc/mod.rs) error-conversion: `ResourceExhausted` → `Status::resource_exhausted`.

At [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) `SubmitBuild` error match: `Code::ResourceExhausted` → retryable `STDERR_ERROR` "scheduler backpressure — retry in ~10s".

Delete `TODO(phase4b)` at `queries.rs:225`.

### T6 — `test(gateway):` unit tests

- `TenantLimiter`: 30 checks pass (burst), 31st fails with `NotUntil`. Different tenant isolated (`check("a")` × 30 doesn't affect `check("b")`).
- `Semaphore`: 1001st `try_acquire_owned` on `Semaphore::new(1000)` returns `Err`.

### T7 — `test(vm):` security.nix rate-limit subtest

Extend [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) (TAIL append): configure rate to 2/min burst 3, fire 4 rapid builds from the same tenant SSH key → assert 4th gets `STDERR_ERROR` with body containing "rate limit".

Marker: `// r[verify gw.rate.per-tenant]` (in `.nix` file header per tracey `.nix` parser rules — col-0 comment before `{`, NOT inside testScript literal).

## Exit criteria

- `/nbr .#ci` green
- `security.nix`: 4th rapid build from same tenant rejected with "rate limit" in error body
- `grep 'TODO(phase4b)' rio-store/src/metadata/queries.rs` returns empty

## Tracey

References existing markers:
- `r[gw.rate.per-tenant]` — T2 implements (ratelimit.rs); T6+T7 verify
- `r[gw.conn.cap]` — T4 implements (server.rs semaphore); T6 verifies

## Files

```json files
[
  {"path": "Cargo.toml", "action": "MODIFY", "note": "T1: [workspace.dependencies] governor = 0.6"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "T1: governor.workspace = true"},
  {"path": "rio-gateway/src/ratelimit.rs", "action": "NEW", "note": "T2: TenantLimiter keyed governor; r[impl gw.rate.per-tenant]"},
  {"path": "rio-gateway/src/lib.rs", "action": "MODIFY", "note": "T2: pub mod ratelimit;"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T3: Arc<TenantLimiter> on shared state; T4: Arc<Semaphore> + try_acquire_owned in accept loop; r[impl gw.conn.cap]"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T3: limiter.check before submit_build :235; T5: Code::ResourceExhausted → retryable STDERR_ERROR"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T5: add MetadataError::ResourceExhausted variant at :53"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "T5: map sqlx pool-timeout → ResourceExhausted; delete TODO(phase4b) at :225"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T5: ResourceExhausted → Status::resource_exhausted at :84"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T7: rate-limit subtest (TAIL append); r[verify gw.rate.per-tenant] at file header"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "note governor quota defaults under r[gw.rate.per-tenant]"}
]
```

```
Cargo.toml                         # T1: governor dep
rio-gateway/
├── Cargo.toml                     # T1
└── src/
    ├── ratelimit.rs               # T2 (NEW)
    ├── lib.rs                     # T2: pub mod
    ├── server.rs                  # T3+T4: limiter + semaphore
    └── handler/build.rs           # T3+T5: check + ResourceExhausted
rio-store/src/
├── metadata/
│   ├── mod.rs                     # T5: variant
│   └── queries.rs                 # T5: close TODO
└── grpc/mod.rs                    # T5: status map
nix/tests/scenarios/security.nix   # T7: rate-limit VM test
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "ONLY Cargo.toml touch in 4b (governor). server.rs collides w/ P0217 (merge P0213 first); grpc/mod.rs distant from P0218."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — markers `r[gw.rate.per-tenant]` and `r[gw.conn.cap]` must exist.

**Conflicts with:**
- `Cargo.toml` — single-writer in 4b (`governor` is the only new workspace dep).
- [`server.rs`](../../rio-gateway/src/server.rs) — P0217 touches `:70` (tenant_name trim site, wraps in `NormalizedName`). **P0213 first** → P0217 rebases (P0217 is filler, goes last anyway).
- [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) — P0218 touches `:142` TODO; T5 touches `:84`. Distant, different functions, parallel OK.
