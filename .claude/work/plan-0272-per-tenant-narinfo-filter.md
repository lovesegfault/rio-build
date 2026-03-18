# Plan 0272: Per-tenant narinfo filtering (SAFETY GATE 3/3)

**Absorbs 4b P0207 deferral.** When auth present, narinfo endpoint JOINs `path_tenants WHERE tenant_id = auth.tenant_id`. Anonymous requests unfiltered (backward compat). Closes 4b P0207's `TODO(phase5)` at `cache_server/auth.rs` (landed with P0207 — verify at dispatch).

**SAFETY GATE 3 of 3.** With P0255+P0256+this merged: `introduction.md:50` "unsafe before Phase 5" warning removal unblocked.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[store.tenant.narinfo-filter]` seeded)
- [P0207](plan-0207-mark-cte-tenant-retention-quota.md) merged (`path_tenants` populated + `AuthenticatedTenant` type)
- [P0206](plan-0206-path-tenants-migration-scheduler-upsert.md) merged (`path_tenants` table)

## Tasks

### T1 — `feat(store):` tenant-filtered narinfo

MODIFY `rio-store/src/cache_server/` narinfo handler (find at dispatch: `rg 'narinfo' rio-store/src/cache_server/`):

```rust
// r[impl store.tenant.narinfo-filter]
let filter = match auth {
    Some(AuthenticatedTenant { tenant_id, .. }) => {
        // Tenant sees only their own paths.
        format!("JOIN path_tenants pt ON pt.store_path = p.store_path
                 WHERE pt.tenant_id = '{}'", tenant_id)
    }
    None => {
        // Anonymous: unfiltered (backward compat — public caches).
        String::new()
    }
};
```

### T2 — `fix(store):` close P0207's TODO

MODIFY `rio-store/src/cache_server/auth.rs` (verify at dispatch: `grep 'TODO(phase5)' rio-store/src/cache_server/`) — delete the TODO.

### T3 — `docs:` close security.md:69 deferral

MODIFY [`docs/src/security.md`](../../docs/src/security.md) — close `:69` deferral ("per-tenant path visibility").

### T4 — `test(vm):` tenant A cannot narinfo tenant B's path

Fragment in security.nix or standalone test:
```nix
# r[verify store.tenant.narinfo-filter]  (col-0 header)
# Tenant A uploads path X. Tenant B queries narinfo for X → 404.
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule store.tenant.narinfo-filter` shows impl + verify
- **With P0255+P0256+this merged: `introduction.md:50` warning removal unblocked** (P0284 closeout does the removal)

## Tracey

References existing markers:
- `r[store.tenant.narinfo-filter]` — T1 implements, T4 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T1+T2: tenant filter + close P0207 TODO"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T3: close :69 deferral"}
]
```

```
rio-store/src/cache_server/
└── auth.rs                       # T1+T2: filter + close TODO
docs/src/
└── security.md                   # T3: close deferral
```

## Dependencies

```json deps
{"deps": [245, 207, 206], "soft_deps": [], "note": "SAFETY GATE 3/3. cache_server/ no collision entry. Parallel with P0255/P0256."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md). [P0207](plan-0207-mark-cte-tenant-retention-quota.md) — `path_tenants` populated. [P0206](plan-0206-path-tenants-migration-scheduler-upsert.md) — table.
**Conflicts with:** `cache_server/` no collision entry. Parallel with P0255/P0256.
