# Plan 0153: Multi-tenancy foundation — tenants table + resolve + SSH key comment

## Design

The phase-4a multi-tenancy model is **D1 scheduler-side resolution**: the gateway captures a tenant **name** (not UUID) and forwards it as an opaque string; the scheduler resolves name→UUID via the new `tenants` table. The store stays tenant-unaware (D4) — no downstream consumer needs the UUID in phase 4.

**Migration 009 Part A** (`4177108`): `tenants(tenant_id UUID PK, tenant_name TEXT UNIQUE, gc_retention_hours INT DEFAULT 168, gc_max_store_bytes BIGINT, cache_token TEXT UNIQUE, created_at TIMESTAMPTZ)`. Pre-FK cleanup NULLs out existing `tenant_id` values in `builds`/`derivations` (valid-UUID-format but reference nothing pre-009). Both FKs are `ON DELETE SET NULL` — deleting a tenant leaves historical builds intact. Partial indexes `WHERE tenant_id IS NOT NULL` since most rows are NULL in single-tenant mode.

**Scheduler resolve** (`388fe5c`): `resolve_tenant(name) → Option<Uuid>` db helper; `grpc/mod.rs` replaces the boundary `parse::<Uuid>()` with this lookup. Type-change cascade: `BuildInfo.tenant_id` changes from `Option<String>` to `Option<Uuid>` (scheduler was already parsing as UUID but storing the string). Empty string → `None` (single-tenant mode, no PG lookup). Unknown tenant → `Status::invalid_argument`. No pool + non-empty tenant → `FailedPrecondition`.

**Gateway capture** (`c2104c4`): the tenant name lives in the **server-side `authorized_keys` entry's comment field**, NOT the client's key (SSH key auth sends raw key data only — the client never transmits a comment). `auth_publickey` changed from `.any(|authorized| ...)` to `.find(...)` to get the matched entry, then reads `.comment()`. Plumbing chain: `ConnectionHandler.tenant_name` → `exec_request` → `session::run_protocol` → `SessionContext::new` → `build_submit_request`. Session span gets `fields(tenant = %tenant_name)`.

## Files

```json files
[
  {"path": "migrations/009_phase4.sql", "action": "NEW", "note": "Part A: tenants table, FKs, partial indexes, pre-FK NULL-out"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "resolve_tenant helper; RecoveryBuildRow.tenant_id → Option<Uuid>"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "parse::<Uuid>() → resolve_tenant; InvalidArgument on unknown"},
  {"path": "rio-scheduler/src/state/build.rs", "action": "MODIFY", "note": "BuildInfo.tenant_id: Option<String> → Option<Uuid>"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "MergeDagRequest.tenant_id → Option<Uuid>"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "auth_publickey .any → .find; read matched entry's .comment()"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "tenant_name plumbing through run_protocol + SessionContext"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "build_submit_request carries tenant_name verbatim"},
  {"path": "rio-controller/src/crds/build.rs", "action": "MODIFY", "note": "comment clarified: tenant NAME not UUID; scheduler resolves"}
]
```

## Tracey

- `r[impl sched.tenant.resolve]` — tagged retroactively in `20e557f`
- `r[verify sched.tenant.resolve]` — tagged retroactively in `20e557f`
- `r[impl gw.auth.tenant-from-key-comment]` — tagged retroactively in `20e557f`
- `r[verify gw.auth.tenant-from-key-comment]` — tagged retroactively in `20e557f`

4 marker annotations (all retroactive — round-1 code was written first, markers applied in the dedicated tracey commit at the end of round 1).

## Entry

- Depends on P0148: phase 3b complete

## Exit

Merged as `4177108`, `388fe5c`, `c2104c4` (3 commits). `.#ci` green. Tests: `test_submit_build_rejects_unknown_tenant`, `test_submit_build_resolves_known_tenant`, `load_authorized_keys` preserves comment. VM smoke test in P0157.
