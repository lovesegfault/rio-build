# Plan 0338: Wire TenantSigner into PutPath — `sign_for_tenant` has zero production call sites

Coordinator finding from [P0272](plan-0272-per-tenant-narinfo-filter.md) review (SAFETY GATE 3/3). **Coordinator initially misread P0272's scope** — expected it to wire per-tenant signing into PutPath, but P0272 is narinfo **filtering** (`r[store.tenant.narinfo-filter]` — cache-server read path), not **signing** (`r[store.tenant.sign-key]` — gRPC write path). P0272's impl correctly stayed in contract.

**The gap:** [P0256](plan-0256-per-tenant-signing-output-hash.md) (DONE, SAFETY GATE 2/3) created [`TenantSigner::sign_for_tenant()`](../../rio-store/src/signing.rs) at `:209` with full `r[impl store.tenant.sign-key]` annotation and 4 `r[verify ...]` unit tests. But `grep sign_for_tenant rio-store/src/` returns **zero hits outside `signing.rs` itself**. The function is a well-tested, spec-annotated **orphan**.

Meanwhile [`StoreServiceImpl.signer`](../../rio-store/src/grpc/mod.rs) at `:123` is still `Option<Arc<Signer>>` — the **old cluster-only type**. [`maybe_sign()`](../../rio-store/src/grpc/mod.rs) at `:271-305` calls `signer.sign(&fp)` synchronously at `:302`. Its only call site is [`put_path.rs:569`](../../rio-store/src/grpc/put_path.rs): `self.maybe_sign(&mut full_info)` — **no `tenant_id` in scope**.

**[P0256](plan-0256-per-tenant-signing-output-hash.md) validator's row-5 doc-bug was the EFFECT of this gap, not the cause.** The doc-comment at [`signing.rs:204`](../../rio-store/src/signing.rs) says "lets `maybe_sign` emit `key=tenant-foo-1` vs `key=cluster`" — forward-looking present tense, because `maybe_sign` doesn't call `sign_for_tenant` yet. Row 5 is absorbed here: this plan makes the comment TRUE, then no edit needed.

## Entry criteria

- [P0256](plan-0256-per-tenant-signing-output-hash.md) merged — DONE. `TenantSigner` + `sign_for_tenant()` exist at [`signing.rs:178-227`](../../rio-store/src/signing.rs).
- [P0259](plan-0259-jwt-verify-middleware.md) merged — UNIMPL as of this writing. **P0259 adds the tonic interceptor that attaches `rio_common::jwt::Claims` to request extensions** per `r[gw.jwt.verify]` — this is the `tenant_id` source for T2 below. Without P0259, there's no way to read the tenant UUID from a gRPC request on the store side.

## Tasks

### T1 — `refactor(store):` `StoreServiceImpl.signer` type change `Signer` → `TenantSigner`

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs):

At `:123`:
```rust
// Before:
signer: Option<Arc<Signer>>,
// After:
signer: Option<Arc<TenantSigner>>,
```

At `:231` `with_signer()`:
```rust
pub fn with_signer(mut self, signer: TenantSigner) -> Self {
    self.signer = Some(Arc::new(signer));
    self
}
```

At `:241` `signer()` accessor: return type `Option<Arc<TenantSigner>>`.

The `:237` doc-comment ("ResignPaths needs the SAME key as PutPath") stays correct — `TenantSigner` wraps the cluster `Signer` by value at [`signing.rs:179`](../../rio-store/src/signing.rs), so `ts.cluster_key_name()` at `:190` gives ResignPaths what it needs.

**Caller impact:** `grep 'with_signer\|\.signer()' rio-store/src/` at dispatch — find all call sites. [`main.rs`](../../rio-store/src/main.rs) constructs the service; it'll need to build a `TenantSigner::new(cluster_signer, pool.clone())` instead of passing the bare `Signer`. The `pool` is already in scope (main.rs connects to PG).

### T2 — `feat(store):` plumb `tenant_id` from JWT Claims → `maybe_sign`

MODIFY [`rio-store/src/grpc/put_path.rs`](../../rio-store/src/grpc/put_path.rs) — extract `tenant_id` from the P0259 interceptor's attached `Claims`:

```rust
// Near :242 where hmac_claims is extracted — add a second extraction
// for the JWT (different claims type — rio_common::jwt::Claims vs
// rio_common::hmac::Claims; the HMAC one has worker_id/drv_hash/
// expected_outputs, the JWT one has sub/iat/exp/jti).
//
// P0259's interceptor runs BEFORE the service method and attaches
// Claims to request.extensions() on successful verify. Absent token
// → dual-mode fallback (r[gw.jwt.dual-mode]) → None here → cluster key.
let tenant_id: Option<uuid::Uuid> = request
    .extensions()
    .get::<rio_common::jwt::Claims>()
    .map(|c| c.sub);
```

**mTLS bypass case** (gateway cert, no JWT): `tenant_id` is `None` → `sign_for_tenant(None, ...)` → cluster-key fallback at [`signing.rs:226`](../../rio-store/src/signing.rs). This is CORRECT: gateway-uploaded paths (via `nix copy` through ssh-ng) have no per-build tenant attribution — the gateway doesn't know which tenant's build produced the path at `PutPath` time. Cluster signature is right.

**Worker-uploaded paths** (HMAC token present): the **worker** carries an assignment JWT too — P0259 adds it. If `r[gw.jwt.verify]` runs on worker→store calls (check P0259's scope at dispatch — its dag note says "3 services" which includes store), `Claims.sub` is populated and tenant-key signing fires.

Then pass `tenant_id` through to the `maybe_sign` call at [`:569`](../../rio-store/src/grpc/put_path.rs):

```rust
// Before:
self.maybe_sign(&mut full_info);
// After:
self.maybe_sign(tenant_id, &mut full_info).await;
```

### T3 — `feat(store):` `maybe_sign` → async, call `sign_for_tenant`

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at `:271-305`:

```rust
// r[impl store.tenant.sign-key]
/// If a signer is configured, compute the narinfo fingerprint and
/// push a signature onto `info.signatures` using the tenant's key
/// (or cluster fallback — see `TenantSigner::sign_for_tenant`).
///
/// `tenant_id` comes from JWT `Claims.sub` (P0259 interceptor). `None`
/// means: no JWT (dual-mode fallback), OR mTLS bypass (gateway cert,
/// `nix copy` path — no per-build attribution), OR dev mode (no
/// interceptor). All three correctly fall through to cluster key.
///
/// async because `sign_for_tenant` hits PG for `tenant_keys` lookup.
/// Errors: `TenantKeyLookup` only when `tenant_id` is Some AND the
/// PG query fails — the None path is infallible. Log-and-cluster-
/// fallback on error (a transient PG hiccup shouldn't fail the
/// upload; the cluster sig is still valid, just not tenant-scoped).
async fn maybe_sign(&self, tenant_id: Option<uuid::Uuid>, info: &mut ValidatedPathInfo) {
    let Some(signer) = &self.signer else { return };

    // ... empty-refs warn block stays unchanged (r[impl store.signing.empty-refs-warn]) ...

    let refs: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
    let fp = rio_nix::narinfo::fingerprint(
        info.store_path.as_str(),
        &info.nar_hash,
        info.nar_size,
        &refs,
    );

    let (sig, was_tenant) = match signer.sign_for_tenant(tenant_id, &fp).await {
        Ok(result) => result,
        Err(e) => {
            // Transient PG failure — don't fail the upload. Fall back
            // to cluster key (which sign_for_tenant would do anyway on
            // the None path, but the error means we never reached the
            // Some→DB branch's success). Log loud so ops notices.
            warn!(error = %e, ?tenant_id, "tenant-key lookup failed; falling back to cluster key");
            (signer.sign_for_tenant(None, &fp).await.expect("None path is infallible").0, false)
        }
    };

    let key_label = if was_tenant { "tenant" } else { "cluster" };
    debug!(key = key_label, ?tenant_id, "signed narinfo fingerprint");
    info.signatures.push(sig);
}
```

The `:204` doc-comment at [`signing.rs`](../../rio-store/src/signing.rs) ("lets `maybe_sign` emit `key=tenant-foo-1` vs `key=cluster`") is now TRUE. Row 5's doc-bug closes without an edit.

**`:268-270` doc-comment** ("No error path: signing can't fail") becomes STALE — delete it and replace per above.

### T4 — `test(store):` integration test — PutPath with tenant JWT → tenant-key signature

NEW test in [`rio-store/tests/grpc/`](../../rio-store/tests/grpc/) (find the right file at dispatch — `grep 'maybe_sign\|with_signer' rio-store/tests/` for existing coverage):

```rust
// r[verify store.tenant.sign-key]
// End-to-end: PutPath request carrying a JWT with Claims.sub=TENANT_ID,
// where TENANT_ID has an active row in tenant_keys, produces a
// signature verifiable by the TENANT's pubkey and NOT by the cluster's.
//
// Mutation check: if maybe_sign passes None instead of the extracted
// tenant_id, the signature verifies with the CLUSTER key instead →
// this test fails on the tenant-key verify assert.
#[sqlx::test]
async fn put_path_with_tenant_jwt_signs_with_tenant_key(pool: PgPool) {
    // 1. Seed tenant_keys with a known ed25519 seed for tid=TENANT_ID.
    // 2. Build StoreServiceImpl with TenantSigner(cluster_signer, pool).
    // 3. PutPath with a request that has Claims{sub: TENANT_ID} in
    //    extensions (simulate the P0259 interceptor's attach).
    // 4. Query the signed narinfo back; extract the signature.
    // 5. Verify sig against TENANT's pubkey → OK.
    // 6. Verify sig against CLUSTER's pubkey → FAILS (wrong key).
    //    Step 6 is the mutation-resistance check — a buggy
    //    maybe_sign(None, ...) passes step 5's TYPE but step 6
    //    catches it (cluster-key sig passes cluster verify).
}
```

Also: the unit tests at [`grpc/mod.rs:617-626`](../../rio-store/src/grpc/mod.rs) `svc_with_signer()` need updating — they build `Signer::parse(...)`, which is now the wrong type. The pool-is-dangling trick at `:626` ("Pool is lazy — never connects since maybe_sign doesn't touch it") **breaks** — `sign_for_tenant(Some(tid))` DOES touch the pool. Those tests need `#[sqlx::test]` with a real pool OR need to pass `tenant_id: None` explicitly.

### T5 — `fix(store):` main.rs — construct TenantSigner

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs) — find the `with_signer(` call (grep at dispatch). Wrap the cluster `Signer` construction:

```rust
// Before (approximately):
let signer = Signer::parse(&signing_key_str)?;
let svc = StoreServiceImpl::new(pool.clone(), ...).with_signer(signer);

// After:
let cluster_signer = Signer::parse(&signing_key_str)?;
let tenant_signer = TenantSigner::new(cluster_signer, pool.clone());
let svc = StoreServiceImpl::new(pool.clone(), ...).with_signer(tenant_signer);
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'sign_for_tenant' rio-store/src/grpc/mod.rs` → ≥1 hit (T3: wired into maybe_sign)
- `grep 'Option<Arc<Signer>>' rio-store/src/grpc/mod.rs` → 0 hits (T1: old type replaced)
- `grep 'Option<Arc<TenantSigner>>' rio-store/src/grpc/mod.rs` → ≥1 hit (T1: new type)
- `grep 'async fn maybe_sign' rio-store/src/grpc/mod.rs` → 1 hit (T3: now async)
- `grep '\.await' rio-store/src/grpc/put_path.rs | grep maybe_sign` → 1 hit (T2: call site awaits)
- `grep 'tenant_id' rio-store/src/grpc/put_path.rs` → ≥2 hits (T2: extraction + pass-through)
- `grep 'jwt::Claims' rio-store/src/grpc/put_path.rs` → ≥1 hit (T2: extension get)
- `cargo nextest run -p rio-store put_path_with_tenant_jwt` → PASS (T4)
- **Mutation check (T4 step 6):** change T3's `maybe_sign` call to pass `None` instead of `tenant_id` → T4 test FAILS on step 6 (cluster-key verify unexpectedly passes). Then revert.
- `nix develop -c tracey query rule store.tenant.sign-key` → impl count ≥ 2 (signing.rs:194 existing + grpc/mod.rs new), verify count ≥ 5 (4 existing unit tests at signing.rs + 1 new integration test)
- `grep 'No error path: signing' rio-store/src/grpc/mod.rs` → 0 hits (T3: stale doc-comment removed)
- `grep 'forward-looking\|lets .maybe_sign. emit' rio-store/src/signing.rs` — the `:204` comment at signing.rs is now TRUE. No edit required (row 5 closes by being-made-correct, not by deletion).

## Tracey

References existing markers:
- `r[store.tenant.sign-key]` — [`store.md:188`](../../docs/src/components/store.md). T3 adds a **second** `r[impl ...]` site at the `maybe_sign` wiring (the first is at [`signing.rs:194`](../../rio-store/src/signing.rs) on `sign_for_tenant` itself). T4 adds a **fifth** `r[verify ...]` site (first four are unit tests at `signing.rs:446-600`). The spec text says "narinfo signing MUST use the tenant's active signing key … falling back to the cluster key" — `sign_for_tenant` is the mechanism, `maybe_sign` is where it actually happens on the PutPath write path. Both deserve `r[impl]`.
- `r[store.signing.empty-refs-warn]` — [`store.md:184`](../../docs/src/components/store.md). T3 **preserves** the existing `r[impl ...]` block at [`grpc/mod.rs:276-288`](../../rio-store/src/grpc/mod.rs) unchanged — it's orthogonal to tenant selection.
- `r[gw.jwt.verify]` — [`gateway.md:493`](../../docs/src/components/gateway.md). T2 **consumes** what P0259's interceptor produces. No new annotation — T2 is the read side, not the impl.

No new spec markers. `r[store.tenant.sign-key]` already fully describes the contract; this plan makes the impl whole.

## Files

```json files
[
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: :123,:231,:241 signer type Signer→TenantSigner. T3: :271-305 maybe_sign→async+sign_for_tenant+r[impl store.tenant.sign-key]. :617-626 test helper fix."},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T2: extract tenant_id from jwt::Claims extension near :242; pass to maybe_sign at :569 +.await"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T5: TenantSigner::new(cluster_signer, pool) at with_signer call"},
  {"path": "rio-store/tests/grpc/core.rs", "action": "MODIFY", "note": "T4: +put_path_with_tenant_jwt_signs_with_tenant_key integration test +r[verify store.tenant.sign-key] (or new file — grep at dispatch)"}
]
```

```
rio-store/src/
├── grpc/
│   ├── mod.rs           # T1: signer type; T3: maybe_sign async+wired
│   └── put_path.rs      # T2: tenant_id extraction + pass-through
└── main.rs              # T5: TenantSigner construction
rio-store/tests/grpc/
└── core.rs              # T4: integration test (or new file)
```

## Dependencies

```json deps
{"deps": [256, 259], "soft_deps": [272], "note": "P0256 DONE — TenantSigner exists. P0259 UNIMPL — BLOCKING: it adds the tonic interceptor that attaches jwt::Claims to request extensions on the store (r[gw.jwt.verify] says 'interceptor on scheduler and store'). Without P0259, T2's request.extensions().get::<Claims>() is always None → this plan is a no-op (cluster key every time). HARD dep. Soft-dep P0272: both touch the tenant path in rio-store but different sides — P0272 is cache_server (HTTP read), this is grpc (gRPC write). P0272 can land before/after/parallel; no file overlap. Coordinator misread P0272 as owning this wiring — P0272's contract is r[store.tenant.narinfo-filter], not r[store.tenant.sign-key]. discovered_from=coordinator review of P0272 (SAFETY GATE 3/3 scope check)."}
```

**Depends on:**
- [P0256](plan-0256-per-tenant-signing-output-hash.md) (DONE) — `TenantSigner` + `sign_for_tenant()` exist.
- [P0259](plan-0259-jwt-verify-middleware.md) (**UNIMPL, blocking**) — the tonic interceptor attaches `jwt::Claims` to request extensions. T2 reads from that. Without it, `tenant_id` is always `None`.

**Conflicts with:** [`rio-store/src/main.rs`](../../rio-store/src/main.rs) is count=25 in `onibus collisions top 30`. T5 is a 3-line change at the `with_signer` call site — small blast radius but check frontier before dispatch. [`grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) and [`put_path.rs`](../../rio-store/src/grpc/put_path.rs) not in top-30. [P0272](plan-0272-per-tenant-narinfo-filter.md) touches `cache_server/` only — zero file overlap.
