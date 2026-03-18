# Plan 0255: Quota enforcement — reject SubmitBuild over quota (SAFETY GATE 1/3)

**Absorbs 4b P0207 T5.** Per Q5 (confirmed default): enforcement is **gateway-side** in [`handler/build.rs`](../../rio-gateway/src/handler/build.rs), near [P0213](plan-0213-per-tenant-rate-limiter.md)'s rate-limit hook. User sees failure in `nix build` output immediately via `STDERR_ERROR`, before the SSH round-trip to scheduler completes.

This is **SAFETY GATE 1 of 3** for removing the `introduction.md:50` "unsafe before Phase 5" warning (per GT16). The other two are [P0256](plan-0256-per-tenant-signing-output-hash.md) (per-tenant signing) and [P0272](plan-0272-per-tenant-narinfo-filter.md) (narinfo filtering).

Per R7: adding a store RPC to the hot path adds latency. Mitigate by caching `tenant_store_bytes` with 30s TTL — quota is soft, a few MB of race-window overflow is acceptable. Document as "eventually-enforcing."

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[store.gc.tenant-quota-enforce]` seeded)
- [P0206](plan-0206-path-tenants-migration-scheduler-upsert.md) merged (`path_tenants` table exists)
- [P0207](plan-0207-mark-cte-tenant-retention-quota.md) merged (accounting populated — else SUM=0 always passes)
- [P0213](plan-0213-per-tenant-rate-limiter.md) merged (hook location + `STDERR_ERROR` pattern established)

## Tasks

### T1 — `feat(gateway):` quota check before submit

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) — near P0213's rate-limit hook (find at dispatch: `grep -n 'limiter.check\|STDERR_ERROR' rio-gateway/src/handler/build.rs`):

```rust
// r[impl store.gc.tenant-quota-enforce]
// Quota is eventually-enforcing: tenant_store_bytes cached 30s.
// A few MB of race-window overflow is fine — this is a soft limit.
let used = quota_cache.get_or_fetch(tenant_id, || {
    store_client.tenant_store_bytes(TenantStoreBytesRequest { tenant_id })
}).await?;
let limit = tenant.gc_max_store_bytes;
if used > limit {
    // DON'T close the connection — 4b P0213 pattern. User can GC and retry.
    send_stderr_error(&mut wire, format!(
        "tenant '{}' over quota: {} / {} — run `nix store gc` or request increase",
        tenant.name, ByteSize(used), ByteSize(limit)
    )).await?;
    metrics::counter!("rio_gateway_quota_rejections_total", "tenant" => tenant.name.clone())
        .increment(1);
    return Ok(());  // early return, connection stays open
}
```

MVP: `estimated_new_bytes = 0` — reject on CURRENT overflow only, not predictive.

### T2 — `feat(gateway):` 30s TTL cache

Simple `DashMap<Uuid, (u64, Instant)>` with TTL check. Or `moka::sync::Cache` if already in deps.

### T3 — `test(gateway):` mock over-quota → assert STDERR_ERROR

Unit: mock store RPC returning `used > limit` → assert `STDERR_ERROR` sent, connection NOT closed, metric incremented.

```rust
// r[verify store.gc.tenant-quota-enforce]
#[tokio::test]
async fn over_quota_sends_stderr_error() { ... }
```

### T4 — `test(vm):` security.nix TAIL fragment

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) — TAIL-append:

```python
# r[verify store.gc.tenant-quota-enforce]  (col-0 header comment, NOT in testScript)
# Seed tenant over quota, attempt build → expect STDERR_ERROR "over quota"
```

### T5 — `docs:` close multi-tenancy.md:82 deferral

MODIFY [`docs/src/multi-tenancy.md`](../../docs/src/multi-tenancy.md) — remove the `:82` deferral block (grep for content string, line number likely stale).

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule store.gc.tenant-quota-enforce` shows impl + verify

## Tracey

References existing markers:
- `r[store.gc.tenant-quota-enforce]` — T1 implements, T3+T4 verify (seeded by P0245, sibling to existing `r[store.gc.tenant-quota]`)

## Files

```json files
[
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T1: quota check near P0213's rate-limit hook (~20 lines)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T4: TAIL fragment (serial after 4c P0242)"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T5: close :82 deferral (grep content string)"}
]
```

```
rio-gateway/src/handler/
└── build.rs                      # T1: quota check (~20 lines near P0213 hook)
nix/tests/scenarios/
└── security.nix                  # T4: TAIL fragment
docs/src/
└── multi-tenancy.md              # T5: close deferral
```

## Dependencies

```json deps
{"deps": [245, 206, 207, 213], "soft_deps": [242], "note": "SAFETY GATE 1/3. handler/build.rs count=19+ — serial after P0213. security.nix TAIL-append serial after 4c P0242 (soft). R7: 30s TTL cache for tenant_store_bytes."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — marker. [P0206](plan-0206-path-tenants-migration-scheduler-upsert.md) — `path_tenants` exists. [P0207](plan-0207-mark-cte-tenant-retention-quota.md) — accounting populated. [P0213](plan-0213-per-tenant-rate-limiter.md) — hook location + STDERR_ERROR pattern.
**Conflicts with:** `handler/build.rs` count=19+, serial after P0213. `security.nix` TAIL-append, soft-serial after 4c P0242.
