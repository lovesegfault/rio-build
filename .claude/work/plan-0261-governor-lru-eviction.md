# Plan 0261: RESERVED — Governor LRU eviction (OBSOLETED by audit B1 #11)

---

**STATUS: RESERVED.** Audit Batch B1 #11 (2026-03-18): rate-limit key is `Claims.sub` (tenant UUID), NOT `jti`. Bounded keyspace — operator creates tenants; dashmap grows to `|tenants|` and stops. No eviction needed. See `.claude/notes/audit-decisions-batch-B1.md`.

The original premise: "jti keying → unbounded keyspace → LRU mandatory." Overridden: jti-keying lets a user mint tokens to escape limits (5 tokens = 5 buckets). `sub`-keying is true per-tenant AND bounded.

**What P0213 should do instead:** key on `Claims.sub` (tenant UUID from JWT) when JWT present; `tenant_name` (SSH comment) as the fallback for dual-mode. Both are bounded. The `TODO(phase5): eviction` in P0213 can be closed — it was predicated on jti keying, which isn't happening.

**If revived later:** only if a decision is made to add a per-session inner limit (two-tier: jti inner + sub outer). That was offered and declined.

---

_Original plan body below, preserved for reference._

---

## Entry criteria

- [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) merged (JWT claims structure finalized, `jti` populated)
- [P0213](plan-0213-per-tenant-rate-limiter.md) merged (`ratelimit.rs` exists, governor wired)

## Tasks

### T1 — `feat(gateway):` change rate-key from tenant_id to jti

MODIFY [`rio-gateway/src/ratelimit.rs`](../../rio-gateway/src/ratelimit.rs) (file LANDS with P0213 — verify at dispatch: `test -f rio-gateway/src/ratelimit.rs`):

```rust
// Before (P0213): rate-key = tenant_id (bounded keyspace)
// After (USER A6): rate-key = jti (unbounded — fresh per SSH connect)
let rate_key = session_ctx.jwt_jti.as_deref()
    .unwrap_or("anonymous");  // SSH-comment fallback has no jti
```

### T2 — `feat(gateway):` LRU eviction wrapper

Wrap `governor::DefaultKeyedRateLimiter` in a bounded map. Two approaches:

**Option A (preferred — moka):**
```rust
use moka::sync::Cache;

pub struct BoundedRateLimiter {
    inner: Cache<String, Arc<DefaultDirectRateLimiter>>,
}

impl BoundedRateLimiter {
    pub fn new(max_keys: u64, idle_ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(max_keys)      // LRU eviction
                .time_to_idle(idle_ttl)       // keys idle >10min dropped
                .build(),
        }
    }
    pub fn check_key(&self, key: &str) -> Result<(), NotUntil<_>> {
        let limiter = self.inner.get_with(key.to_string(), || {
            Arc::new(DefaultDirectRateLimiter::direct(Quota::per_second(...)))
        });
        limiter.check()
    }
}
```

**Option B (periodic sweep, if moka adds unwanted dep weight):**
Background task every 60s: `dashmap.retain(|_, state| state.last_used > now - 600s)`.

### T3 — `feat(deps):` add moka if not present

Check `Cargo.toml` — `moka` may already be in deps. If not, add `moka = { version = "0.12", features = ["sync"] }`.

### T4 — `refactor(gateway):` delete P0213's TODO

Remove the `TODO(phase5): eviction` comment P0213 left.

### T5 — `test(gateway):` eviction under key-churn

```rust
#[tokio::test]
async fn rate_limiter_evicts_idle_keys() {
    let limiter = BoundedRateLimiter::new(100, Duration::from_millis(50));
    // Churn 1000 keys. Advance clock past idle_ttl.
    // Assert inner cache size ≤ 100.
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'TODO\(phase5\)' rio-gateway/src/ratelimit.rs` → 0

## Tracey

none — this is internal resilience. `r[gw.rate.per-tenant]` exists at gateway.md:657 but this plan doesn't change its contract (rate-limiting still happens); the key change is an implementation detail.

## Files

```json files
[
  {"path": "rio-gateway/src/ratelimit.rs", "action": "MODIFY", "note": "T1+T2+T4: jti key + LRU wrapper + delete TODO (file lands with P0213)"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "T3: moka dep if not present (check first)"}
]
```

```
rio-gateway/src/
└── ratelimit.rs                  # T1-T4 (file LANDS with P0213)
Cargo.toml                        # T3: moka (conditional)
```

## Dependencies

```json deps
{"deps": [260, 213], "soft_deps": [], "note": "USER A6: MANDATORY LRU (jti = unbounded keyspace). NOT assess-then-close. ratelimit.rs NEW from P0213 — zero prior collision. R15: frontier won't surface this until ratelimit.rs exists."}
```

**Depends on:** [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) — `jti` populated in session context. [P0213](plan-0213-per-tenant-rate-limiter.md) — `ratelimit.rs` exists.
**Conflicts with:** `ratelimit.rs` is NEW from P0213 — zero prior collision history.
