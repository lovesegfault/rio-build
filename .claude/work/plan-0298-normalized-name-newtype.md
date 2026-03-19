# Plan 0298: NormalizedName newtype — dedupe 4 ad-hoc trim/empty sites

Originates from [`rio-scheduler/src/grpc/mod.rs:189`](../../rio-scheduler/src/grpc/mod.rs) `TODO(phase4b)` — the last phase-tagged TODO in that file. The scheduler's `resolve_tenant_name` is the **fourth** callsite patching trim/empty normalization ad-hoc for tenant identifiers; gateway, store, and controller each grew their own.

**The bug class this prevents:** see `lang-gotchas.md` — "validate-trimmed-store-untrimmed." If the gateway trims `" team-a "` to `"team-a"` but the scheduler's admin `CreateTenant` stores the raw CRD field without trimming, `WHERE tenant_name = $1` never matches. Each site that grows its own `.trim()` is one more place to get this wrong.

**The sites:**
| Crate | File:line | Pattern | After P0294? |
|---|---|---|---|
| gateway | [`server.rs:354`](../../rio-gateway/src/server.rs) | `matched.comment().trim().to_string()` | stays |
| scheduler | [`grpc/mod.rs:199`](../../rio-scheduler/src/grpc/mod.rs) | `let name = name.trim(); if name.is_empty() { ... }` | stays |
| scheduler | [`admin/mod.rs:490`](../../rio-scheduler/src/admin/mod.rs) | delegates to `resolve_tenant_name` (same helper) | stays |
| controller | [`reconcilers/build.rs:236`](../../rio-controller/src/reconcilers/build.rs) | `b.spec.tenant.clone().unwrap_or_default()` (no trim!) | **DELETED by P0294** |
| store | [`cache_server/auth.rs:56`](../../rio-store/src/cache_server/auth.rs) | `.map(str::trim).filter(|t| !t.is_empty())` (on the *token*, not tenant name — but same pattern) | stays |

After [P0294](plan-0294-build-crd-full-rip.md) lands, the controller site evaporates. Soft-dep: if this lands first, convert `reconcilers/build.rs:236` anyway (one extra `NormalizedName::from_maybe_empty` call); P0294 deletes the file wholesale.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(common):` `NormalizedName` newtype with validating constructor

NEW section in [`rio-common/src/newtype.rs`](../../rio-common/src/newtype.rs) (after the `string_newtype!` macro, which lacks validation). This is hand-written rather than macro-generated because the constructor is fallible:

```rust
/// Tenant name (or similar human-readable identifier) with
/// construction-time normalization: trimmed, non-empty, no interior
/// whitespace. Use at boundaries where an external string (SSH key
/// comment, CRD field, HTTP header, proto field) becomes an
/// identifier we'll use in PG WHERE clauses or as a map key.
///
/// Three constructors, three intents:
/// - `new(s)` → `Result<Self, NameError>` — the caller WANTS a name
///   and will propagate the error. For CreateTenant, SubmitBuild.
/// - `from_maybe_empty(s)` → `Option<Self>` — the caller accepts
///   absence (single-tenant mode). Empty/whitespace → None. For
///   resolve_tenant_name, gateway key comment.
/// - `new_unchecked(s)` — test-only. Just wraps.
///
/// Storage invariant: if a `NormalizedName` exists, its inner string
/// is trimmed and non-empty. No `.trim()` calls downstream; PG
/// `WHERE tenant_name = $1` always matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedName(String);

#[derive(Debug, thiserror::Error)]
pub enum NameError {
    #[error("name is empty after trimming")]
    Empty,
    #[error("name contains interior whitespace: {0:?}")]
    InteriorWhitespace(String),
}

impl NormalizedName {
    pub fn new(s: &str) -> Result<Self, NameError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(NameError::Empty);
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(NameError::InteriorWhitespace(trimmed.into()));
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Empty/whitespace-only → None (single-tenant mode).
    /// Non-empty but with interior whitespace → still Err-equivalent
    /// (we return None here; caller treats as invalid). Alternatively
    /// return `Result<Option<Self>, NameError>` to distinguish —
    /// decide at impl time based on whether callers care.
    pub fn from_maybe_empty(s: &str) -> Option<Self> {
        Self::new(s).ok()
    }

    pub fn as_str(&self) -> &str { &self.0 }
    pub fn into_inner(self) -> String { self.0 }

    #[cfg(test)]
    pub fn new_unchecked(s: impl Into<String>) -> Self { Self(s.into()) }
}

// Display, Deref<Target=str>, Borrow<str>, AsRef<str> — copy the
// impl blocks from string_newtype! above (can't use the macro
// directly because it derives From<&str> infallibly).
```

Interior-whitespace rejection: `"team a"` is almost certainly a misconfigured authorized_keys comment (`# team-a` with a space instead of dash). Reject it loudly rather than creating a tenant named `"team a"` that nothing else can reference.

Proptest: `proptest! { fn roundtrip(s: String) { if let Ok(n) = NormalizedName::new(&s) { assert_eq!(n.as_str(), s.trim()); assert!(!n.as_str().is_empty()); } } }`.

### T2 — `refactor(gateway):` use `NormalizedName` at SSH-key-comment source

MODIFY [`rio-gateway/src/server.rs:354`](../../rio-gateway/src/server.rs). The comment field is the SOURCE of tenant names in the system — normalize here, everything downstream is clean.

```rust
// Before:
self.tenant_name = matched.comment().trim().to_string();

// After:
self.tenant_name = NormalizedName::from_maybe_empty(matched.comment());
// Field type changes: String → Option<NormalizedName>
```

Propagate the type change through `SessionContext.tenant_name` ([`handler/mod.rs:101`](../../rio-gateway/src/handler/mod.rs)) and the gRPC translation ([`translate.rs:545`](../../rio-gateway/src/translate.rs)). At the proto boundary, `.map(|n| n.into_inner()).unwrap_or_default()` — proto stays `string` (empty-string-as-absent is the proto convention already in use).

### T3 — `refactor(scheduler):` use `NormalizedName` in `resolve_tenant_name`

MODIFY [`rio-scheduler/src/grpc/mod.rs:192-210`](../../rio-scheduler/src/grpc/mod.rs):

```rust
pub(crate) async fn resolve_tenant_name(
    pool: &sqlx::PgPool,
    name: &str,
) -> Result<Option<Uuid>, Status> {
    // r[impl sched.tenant.resolve]
    let Some(name) = NormalizedName::from_maybe_empty(name) else {
        return Ok(None); // single-tenant mode, no PG roundtrip
    };
    sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = $1")
        .bind(name.as_str())
        // ... rest unchanged
}
```

The `// r[impl sched.tenant.resolve]` annotation goes here (marker exists at [`scheduler.md:91`](../../docs/src/components/scheduler.md) — currently has no `r[impl]` annotation per tracey).

Delete the `TODO(P0298)` comment block at `:189-191`.

### T4 — `refactor(scheduler):` use `NormalizedName` in `CreateTenant`

MODIFY [`rio-scheduler/src/admin/mod.rs`](../../rio-scheduler/src/admin/mod.rs) around `create_tenant` (`:704`). This is the **write path** — the most important conversion. If `CreateTenant` stores an untrimmed name, no trimmed lookup will ever find it.

```rust
// r[impl sched.admin.create-tenant]
async fn create_tenant(&self, req: Request<...>) -> Result<...> {
    let name = NormalizedName::new(&req.get_ref().tenant_name)
        .map_err(|e| Status::invalid_argument(format!("invalid tenant name: {e}")))?;
    // ... INSERT uses name.as_str()
}
```

### T5 — `refactor(store):` use `NormalizedName` pattern for cache-token trim

MODIFY [`rio-store/src/cache_server/auth.rs:56-57`](../../rio-store/src/cache_server/auth.rs). This trims the *token* not the tenant name, so it's semantically a different newtype (`CacheToken` or similar) — but the trim-then-empty-check pattern is identical. Either:
- (a) Generalize `NormalizedName` to `Normalized<T>` with a phantom tag — overkill for two types
- (b) Leave the `.map(str::trim).filter(|t| !t.is_empty())` but wrap the RESULT (the `tenant_name` from PG) in `NormalizedName` before inserting into `AuthenticatedTenant`

Prefer (b): the trim-on-token stays local; the thing that flows downstream (`AuthenticatedTenant`) carries a `NormalizedName`. Since PG stores already-normalized values (per T4), wrapping is `NormalizedName::new(...).expect("PG stores normalized names")` — or use `new_unchecked` with a debug_assert.

## Exit criteria

- `grep -rn '\.trim()' rio-gateway/src/server.rs rio-scheduler/src/grpc/mod.rs` — no tenant-name trim callsites remain (the pattern moved into `NormalizedName::new`)
- `create_tenant` rejects `"  "` and `"team a"` with `InvalidArgument` (unit test)
- Gateway with authorized_keys comment `"  team-a  "` → scheduler receives `"team-a"` (integration test via existing `ssh_hardening.rs` scaffolding)
- Proptest: `NormalizedName::new(s).map(|n| n.as_str())` is always trimmed and non-empty
- `grep -n 'TODO(P0298)' rio-scheduler/src/grpc/mod.rs` — empty (TODO closed)
- `r[sched.tenant.resolve]` shows `impl` in tracey (was uncovered)

## Tracey

References existing markers:
- `r[sched.tenant.resolve]` — T3 adds the `r[impl]` annotation (currently uncovered — no impl annotation exists)
- `r[sched.admin.create-tenant]` — T4 keeps the existing annotation, tightens the behavior

No new markers — this is a refactor of existing behavior, not new behavior.

## Files

```json files
[
  {"path": "rio-common/src/newtype.rs", "action": "MODIFY", "note": "T1: NormalizedName + NameError + proptest"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T2: tenant_name field type String→Option<NormalizedName>"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T2: SessionContext field type change"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T2: proto boundary conversion"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T3: resolve_tenant_name uses NormalizedName; delete TODO; add r[impl]"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T4: create_tenant validates via NormalizedName::new"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T5: AuthenticatedTenant carries NormalizedName"}
]
```

```
rio-common/src/
└── newtype.rs           # T1: NormalizedName type
rio-gateway/src/
├── server.rs            # T2: source-side normalization
├── handler/mod.rs       # T2: type propagation
└── translate.rs         # T2: proto boundary
rio-scheduler/src/
├── grpc/mod.rs          # T3: resolve_tenant_name + close TODO
└── admin/mod.rs         # T4: create_tenant write-path
rio-store/src/cache_server/
└── auth.rs              # T5: AuthenticatedTenant wraps NormalizedName
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [294], "note": "phase-cleanup: grpc/mod.rs:189 TODO(phase4b) orphan. 4th ad-hoc trim site. soft_dep 294: controller build.rs:236 site evaporates with Build CRD rip — if 294 lands first, one less conversion."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — phase4b fan-out root.
**Soft dep:** [P0294](plan-0294-build-crd-full-rip.md) — deletes `rio-controller/src/reconcilers/build.rs`, removing one conversion site. Order-agnostic.

**Conflicts with:**
- [`newtype.rs`](../../rio-common/src/newtype.rs) count=4, **UNIMPL=[]** — no contention. Clean append.
- [`grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) count=32, UNIMPL=[217, 259, 266, 287, 293]. P0259 (JWT) touches the auth-interceptor section; P0287/P0293 touch trace-linkage comments. `resolve_tenant_name` at `:192-210` is isolated — no overlap.
- [`translate.rs`](../../rio-gateway/src/translate.rs) count=18, UNIMPL=[226, 250]. T2 touches `:545` in `build_submit_request` — likely independent of P0226/P0250 targets, verify at dispatch.
- [`auth.rs`](../../rio-store/src/cache_server/auth.rs) — per [`phase-removal-mapping.md:10`](../notes/phase-removal-mapping.md) P0272 (per-tenant-narinfo-filter) owns `auth.rs:36` TODO. Different line range (`:36` vs `:56-75`). Should compose.
