# Plan 998049903: NormalizedName post-impl hardening — interior-ws warn + struct-invariant + CHECK constraint

Post-PASS review of [P0298](plan-0298-normalized-name-newtype.md). Three edge-case findings that land AFTER P0298's newtype migration, each tightening a silent-degrade path:

**(1) Gateway silently drops interior-whitespace comments without warn.** P0298-T2 at [`server.rs`](../../rio-gateway/src/server.rs) (the p298 branch has it at `~:703`) uses `NormalizedName::from_maybe_empty(matched.comment())` which returns `None` for both empty-comment (single-tenant mode, intentional) AND interior-whitespace (`"team a"` — misconfigured `authorized_keys`). The doc-comment above says "Empty or malformed comment (interior whitespace — `"team a"`) → None (single-tenant mode)" — but degrading a misconfigured multi-tenant key to anonymous single-tenant is a silent security-mode downgrade. No warn, no metric, no audit trail. The operator who typo'd `team a` in `authorized_keys` sees builds succeed in single-tenant mode and never learns their tenant isolation is broken.

**(2) Store auth struct-invariant break in release builds.** P0298-T5 at [`cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) (p298 branch `~:98`) wraps the PG-returned `tenant_name` with `NormalizedName::new(&name).ok()` + `debug_assert!(tenant_name.is_some())`. In release builds, `.ok()` + stripped `debug_assert!` means a PG row with a non-normalized name (manual INSERT, migration, bug) produces `AuthenticatedTenant { tenant_id: Some(id), tenant_name: None }` — authenticated-but-anonymous. Downstream code assuming `tenant_id.is_some() → tenant_name.is_some()` (the pre-P0298 invariant) breaks. The `debug_assert!` catches it in tests; release silently violates the struct contract.

**(3) The struct-invariant break is unreachable IFF PG rejects non-normalized names at write time.** A `CHECK (tenant_name = trim(tenant_name) AND tenant_name <> '' AND tenant_name !~ '\s')` constraint on `tenants.tenant_name` makes the `debug_assert!` provably dead — PG enforces the same invariant `NormalizedName::new` checks. The constraint also covers the manual-INSERT/migration bypass paths the doc-comment worries about.

## Entry criteria

- [P0298](plan-0298-normalized-name-newtype.md) merged (`NormalizedName` newtype + `from_maybe_empty` + the 7-site migration exist)

## Tasks

### T1 — `fix(gateway):` warn on interior-whitespace comment; distinguish from empty

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) at the P0298-T2 `from_maybe_empty` site (grep `NormalizedName::from_maybe_empty(matched.comment())` at dispatch — p298 branch has it around `:703`). Replace the silent `from_maybe_empty` with explicit branching:

```rust
// Normalize via the shared newtype. Three cases:
//   - Ok(name)       → multi-tenant mode with a valid tenant name
//   - Err(Empty)     → single-tenant mode (intentional — empty comment)
//   - Err(InteriorWhitespace) → MISCONFIGURED authorized_keys.
//     Degrade to single-tenant (same as Empty — the comment isn't a
//     usable tenant identifier) but WARN: the operator typo'd
//     `# team a` instead of `# team-a` and their tenant isolation
//     is silently off. The warn makes it visible in logs; the
//     rio_gateway_auth_degraded_total counter makes it alertable.
self.tenant_name = match NormalizedName::new(matched.comment()) {
    Ok(name) => Some(name),
    Err(NameError::Empty) => None,  // single-tenant mode
    Err(NameError::InteriorWhitespace(raw)) => {
        warn!(
            comment = %raw,
            key_fingerprint = %matched.fingerprint(),
            "authorized_keys comment has interior whitespace — \
             degrading to single-tenant mode; fix the comment"
        );
        metrics::counter!("rio_gateway_auth_degraded_total",
            "reason" => "interior_whitespace").increment(1);
        None
    }
};
```

This preserves P0298's `Option<NormalizedName>` type but surfaces the misconfig path. Add `describe_counter!("rio_gateway_auth_degraded_total", ...)` to [`rio-gateway/src/lib.rs`](../../rio-gateway/src/lib.rs) in `describe_metrics()`.

### T2 — `fix(store):` auth.rs struct-invariant — fail-loud instead of silent-degrade in release

MODIFY [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) at the P0298-T5 wrap site (grep `NormalizedName::new(&name).ok()` at dispatch — p298 branch `~:98`). Replace `.ok()` + `debug_assert!` with an explicit error branch that preserves the invariant in release:

```rust
Ok(Some((id, name))) => {
    // PG stores already-normalized names (CreateTenant validates
    // via NormalizedName::new before INSERT, and migration 019
    // adds a CHECK constraint enforcing the same invariant). If
    // normalization rejects a PG-stored name, something bypassed
    // BOTH — fail the request with 500 so the operator notices,
    // don't silently authenticate with tenant_name=None (that
    // breaks the tenant_id.is_some()→tenant_name.is_some()
    // invariant downstream code relies on).
    let tenant_name = match NormalizedName::new(&name) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(
                tenant_id = %id, stored_name = %name, error = %e,
                "PG-stored tenant_name failed normalization — \
                 write-path bypass (manual INSERT? pre-CHECK row?)"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "tenant record malformed",
            ).into_response();
        }
    };
    request.extensions_mut().insert(AuthenticatedTenant {
        tenant_id: Some(id),
        tenant_name: Some(tenant_name),
    });
    next.run(request).await
}
```

This keeps the struct invariant `tenant_id.is_some() ↔ tenant_name.is_some()` unconditionally. T3's CHECK constraint makes this branch provably unreachable for rows written post-migration; it remains a defense for pre-migration rows.

### T3 — `feat(migrations):` CHECK constraint on `tenants.tenant_name` — PG enforces NormalizedName invariant

NEW [`migrations/019_tenant_name_check.sql`](../../migrations/019_tenant_name_check.sql):

```sql
-- PG-side enforcement of the NormalizedName invariant (trimmed,
-- non-empty, no interior whitespace). rio-common/src/newtype.rs
-- NormalizedName::new checks the same three properties.
--
-- This makes the auth.rs "normalization rejected a PG-stored name"
-- branch provably dead for post-migration rows. Pre-migration rows
-- that violate the constraint fail ADD CONSTRAINT below — surface
-- them to the operator rather than silently accepting bad data.
--
-- `~` is POSIX regex match. `\s` matches any whitespace (space, tab,
-- newline — the full Unicode whitespace class via PG's regex engine).
-- The regex check subsumes trim-equality (leading/trailing whitespace
-- matches `\s`), but the trim check is cheaper so keep both.

-- Validate existing rows first (NOT VALID → VALIDATE pattern if the
-- table is large; tenants is typically <100 rows, so direct ADD is
-- fine). If this fails on an existing row, the operator sees which
-- tenant_name is malformed and can fix it before re-running.
ALTER TABLE tenants
    ADD CONSTRAINT tenant_name_normalized
    CHECK (
        tenant_name = trim(tenant_name)
        AND tenant_name <> ''
        AND tenant_name !~ '\s'
    );
```

Check at dispatch: does `009_phase4.sql:15` already have any CHECK on `tenant_name`? It has `NOT NULL UNIQUE` — no normalization check. This migration is purely additive.

### T4 — `test(gateway):` interior-whitespace comment emits warn + counter

NEW test in [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) `#[cfg(test)]` mod (or `tests/ssh_hardening.rs` if the auth-publickey test harness lives there — check at dispatch):

```rust
// r[verify gw.auth.tenant-from-key-comment]
#[tokio::test]
async fn interior_whitespace_comment_warns_and_degrades() {
    let recorder = CountingRecorder::install();
    let handler = test_handler_with_keys(&[
        authorized_key_entry("ssh-ed25519 AAAA...", "team a"),  // typo: space not dash
    ]);

    let auth = handler.auth_publickey(
        &test_client_key_matching_first_entry(),
    ).await;

    // Degrades to single-tenant (Accept, tenant_name=None):
    assert!(matches!(auth, Auth::Accept));
    assert_eq!(handler.tenant_name, None);
    // But WARN emitted + counter bumped:
    assert_eq!(
        recorder.counter("rio_gateway_auth_degraded_total"), 1,
        "should emit auth_degraded counter for interior-ws"
    );
}
```

### T5 — `test(store):` auth.rs fail-loud on non-normalized PG row

NEW test in [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) `#[cfg(test)]` mod:

```rust
/// T2 regression: PG row with non-normalized tenant_name (bypass via
/// manual INSERT, pre-CHECK migration, or CreateTenant bug) fails
/// the auth request with 500, NOT silently authenticating with
/// tenant_name=None.
// r[verify store.cache.auth-bearer]
#[tokio::test]
async fn non_normalized_pg_name_fails_request() {
    let pool = test_pool().await;
    // Bypass CreateTenant's normalization AND the CHECK constraint
    // (seed BEFORE migration 019, or use a test-only raw INSERT):
    sqlx::query("INSERT INTO tenants (tenant_id, tenant_name, cache_token) VALUES ($1, $2, $3)")
        .bind(Uuid::new_v4())
        .bind("  bad  ")  // untrimmed — NormalizedName::new rejects
        .bind("test-token")
        .execute(&pool).await.unwrap();

    let auth = CacheAuth { pool, allow_unauthenticated: false };
    let resp = auth.authenticate(request_with_bearer("test-token")).await;

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR,
        "should fail-loud, not silently degrade to tenant_name=None");
}
```

**Pre-migration-019 seeding:** the test needs to insert a bad row that the CHECK constraint would reject. Either (a) run the test against a schema that stops at migration 018, or (b) use `ALTER TABLE tenants DROP CONSTRAINT IF EXISTS tenant_name_normalized` in test setup. Prefer (b) — simpler, and the test is about the auth.rs fail-loud path not the constraint.

### T6 — `test(scheduler):` migration 019 rejects non-normalized INSERT

NEW test in [`rio-scheduler/tests/migrations.rs`](../../rio-scheduler/tests/migrations.rs) (or wherever migration-integrity tests live — check at dispatch):

```rust
// r[verify sched.admin.create-tenant]
#[tokio::test]
async fn migration_019_check_rejects_non_normalized_name() {
    let pool = apply_all_migrations().await;
    for bad in &["  team  ", "team a", "", "  "] {
        let result = sqlx::query(
            "INSERT INTO tenants (tenant_id, tenant_name) VALUES ($1, $2)"
        )
        .bind(Uuid::new_v4())
        .bind(bad)
        .execute(&pool).await;
        assert!(
            result.is_err(),
            "CHECK constraint should reject {bad:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("tenant_name_normalized"),
            "error should name the constraint — got: {err}");
    }
}
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'InteriorWhitespace' rio-gateway/src/server.rs` → ≥1 hit (T1: explicit branch on the error variant)
- `grep 'rio_gateway_auth_degraded_total' rio-gateway/src/lib.rs rio-gateway/src/server.rs` → ≥2 hits (T1: describe + emit)
- `grep '.ok()' rio-store/src/cache_server/auth.rs | grep NormalizedName` → empty (T2: `.ok()` silent-degrade removed)
- `grep 'debug_assert' rio-store/src/cache_server/auth.rs | grep tenant_name` → empty (T2: debug_assert dropped — no longer needed, T3's CHECK + T2's fail-loud cover it)
- `test -f migrations/019_tenant_name_check.sql` (T3: migration exists)
- `grep 'tenant_name_normalized' migrations/019_*.sql` → ≥1 hit (T3: constraint named)
- `cargo nextest run -p rio-gateway interior_whitespace_comment_warns_and_degrades` → pass (T4)
- `cargo nextest run -p rio-store non_normalized_pg_name_fails_request` → pass (T5)
- `cargo nextest run migration_019_check_rejects_non_normalized_name` → pass (T6)
- **T1 mutation:** replace the `InteriorWhitespace(_)` arm with `=> None` (no warn) → T4 fails on counter assertion
- **T2 mutation:** restore `.ok()` + `debug_assert!` → T5 fails (release-build struct invariant break; the test runs in release profile via nextest)
- `nix develop -c tracey query rule gw.auth.tenant-from-key-comment` → shows ≥1 `verify` (T4 adds it)

## Tracey

References existing markers:
- `r[gw.auth.tenant-from-key-comment]` — T1 tightens the degrade path (interior-ws → warn+single-tenant, not silent); T4 adds `r[verify]`
- `r[store.cache.auth-bearer]` — T2 preserves the struct invariant; T5 adds `r[verify]`
- `r[sched.admin.create-tenant]` — T3's CHECK constraint enforces the same invariant `CreateTenant` applies; T6 adds `r[verify]`

No new markers — this hardens existing behavior.

## Files

```json files
[
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T1: from_maybe_empty → explicit new()+match; warn+counter on InteriorWhitespace; T4: test"},
  {"path": "rio-gateway/src/lib.rs", "action": "MODIFY", "note": "T1: +describe_counter rio_gateway_auth_degraded_total"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T2: .ok()+debug_assert → explicit Err→500; preserve tenant_id↔tenant_name invariant; T5: test"},
  {"path": "migrations/019_tenant_name_check.sql", "action": "NEW", "note": "T3: CHECK constraint tenant_name=trim∧≠''∧!~whitespace"},
  {"path": "rio-scheduler/tests/migrations.rs", "action": "MODIFY", "note": "T6: +migration_019_check_rejects_non_normalized_name test"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T1: +rio_gateway_auth_degraded_total row in Gateway Metrics table"}
]
```

```
rio-gateway/src/
├── server.rs             # T1: explicit InteriorWhitespace branch + T4 test
└── lib.rs                # T1: describe_counter
rio-store/src/cache_server/
└── auth.rs               # T2: fail-loud + T5 test
migrations/
└── 019_tenant_name_check.sql  # T3: CHECK constraint
rio-scheduler/tests/
└── migrations.rs         # T6: constraint rejection test
docs/src/
└── observability.md      # T1: new counter row
```

## Dependencies

```json deps
{"deps": [298], "soft_deps": [304, 260], "note": "discovered_from=298 (rev-p298). Three post-impl edge cases: (1) gateway from_maybe_empty silently degrades interior-ws to single-tenant — needs explicit warn+counter to distinguish misconfig from intent; (2) store auth.rs .ok()+debug_assert breaks struct invariant in release (tenant_id:Some + tenant_name:None — authenticated-but-anonymous); (3) CHECK constraint makes (2)'s branch provably dead for post-migration rows + covers manual-INSERT bypass. Soft-dep P0304-T46 (migration-number-collision detector — T3 allocates migration 019; check at dispatch no other UNIMPL plan claims 019). Soft-dep P0260 (touches server.rs auth_publickey — T1 edits the same function at the tenant_name assignment; P0260 is DONE so the JWT dual-mode block at :700-724 is final; T1's edit is just before it at the from_maybe_empty line)."}
```

**Depends on:** [P0298](plan-0298-normalized-name-newtype.md) — `NormalizedName` newtype + `from_maybe_empty` + 7-site migration.

**Conflicts with:** [`server.rs`](../../rio-gateway/src/server.rs) — T1 edits the P0298-inserted `from_maybe_empty` call (single site). [`auth.rs`](../../rio-store/src/cache_server/auth.rs) — T2 edits the P0298-inserted `.ok()+debug_assert` block (single site). [`observability.md`](../../docs/src/observability.md) count=24 (HOT) — T1 adds one Gateway Metrics table row. [`migrations/`](../../migrations/) — T3 allocates `019_*`; grep `019` in UNIMPL plans' Files fences at dispatch (P0304-T46 collision detector should flag if another plan claims 019).
