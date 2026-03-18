# Plan 0217: `NormalizedName` newtype — tenant-name refactor

Wave 3 filler. Pure refactor — zero behavior change. Today tenant names are ad-hoc `.trim()`ed at 4+ sites across 4 crates. This plan adds a `NormalizedName` newtype in `rio-common` that centralizes normalization (trim, reject empty/whitespace-only) via `TryFrom<&str>`.

Closes `TODO(phase4b)` at [`rio-scheduler/src/grpc/mod.rs:189`](../../rio-scheduler/src/grpc/mod.rs) ("4th site" — the other 3 already existed before the TODO was written).

**Schedule late.** Wide file footprint across 4 crates; two of the touched files collide with P0207 (auth.rs) and P0213 (server.rs). Both of those change the SHAPE of what this plan wraps — merge them first.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged

## Tasks

### T1 — `refactor(common):` `NormalizedName` newtype

NEW (or extend existing) `rio-common/src/tenant.rs`:

```rust
/// Normalized tenant name: trimmed, non-empty.
/// Centralizes the 4+ ad-hoc .trim() sites across the codebase.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NormalizedName(String);

#[derive(Debug, thiserror::Error)]
#[error("tenant name is empty or whitespace-only")]
pub struct EmptyTenantName;

impl TryFrom<&str> for NormalizedName {
    type Error = EmptyTenantName;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let t = s.trim();
        if t.is_empty() {
            Err(EmptyTenantName)
        } else {
            Ok(Self(t.to_string()))
        }
    }
}

impl std::fmt::Display for NormalizedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for NormalizedName {
    fn as_ref(&self) -> &str { &self.0 }
}

impl From<NormalizedName> for String {
    fn from(n: NormalizedName) -> String { n.0 }
}
```

`pub mod tenant;` in `rio-common/src/lib.rs` (or add to existing tenant module).

### T2 — `refactor:` replace ad-hoc `.trim()` sites

- [`rio-scheduler/src/grpc/mod.rs:189`](../../rio-scheduler/src/grpc/mod.rs) — the `TODO(phase4b)` site. Delete TODO.
- [`rio-gateway/src/server.rs:70`](../../rio-gateway/src/server.rs) — authorized_keys comment trim. **After P0213** — P0213 reads `tenant_name` for the rate limiter; the type change goes through both.
- [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) — tenant lookup. **After P0207** — P0207 changes the struct to carry `(tenant_id, tenant_name)`; wrap the name field.
- `grep -rn 'tenant.*\.trim()\|\.trim().*tenant' rio-*/src/` for any other sites.

## Exit criteria

- `/nbr .#ci` green (includes all existing tests — zero behavior change proven by existing test suite)
- Zero behavior change — existing tests pass unchanged
- `grep 'TODO(phase4b)' rio-scheduler/src/grpc/mod.rs` returns empty

## Tracey

None — pure refactor, zero behavior change.

## Files

```json files
[
  {"path": "rio-common/src/tenant.rs", "action": "NEW", "note": "T1: NormalizedName newtype + TryFrom + Display + AsRef + From (NEW or extend existing)"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "T1: pub mod tenant; (if not already present)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T2: replace :189 trim → NormalizedName::try_from; delete TODO(phase4b)"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T2: replace :70 authorized_keys comment trim (after P0213 adds limiter)"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T2: wrap tenant_name field (after P0207 adds tenant_id tuple)"}
]
```

```
rio-common/src/
├── tenant.rs                      # T1 (NEW or extend)
└── lib.rs                         # T1: pub mod
rio-scheduler/src/grpc/mod.rs      # T2: :189
rio-gateway/src/server.rs          # T2: :70 (after P0213)
rio-store/src/cache_server/auth.rs # T2: name field (after P0207)
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [207, 213], "note": "Schedule LATE. Merge after P0207 (auth.rs struct shape) and after P0213 (server.rs rate-limiter reads tenant_name)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md).

**Conflicts with:**
- [`server.rs:70`](../../rio-gateway/src/server.rs) — **P0213** adds `Arc<TenantLimiter>` and reads `tenant_name` for rate limiting. P0217 changes the **type** of `tenant_name`. **Merge P0213 first** → P0217 rebases (filler goes last anyway).
- [`cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) — **P0207** changes `AuthenticatedTenant` to `(tenant_id, tenant_name)` tuple. P0217 wraps the name. **Merge P0207 first** → P0217 rebases.
