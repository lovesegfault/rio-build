# Plan 0259: JWT verify middleware (scheduler + store) + jti revocation check

> **PRE-IMPL CORRECTION (P0245 validation, 2026-03):** Two internal contradictions — the prose at line 5 is correct, the code snippets contradict it.
>
> **Contradiction 1 — T2 line 43 + json-files line 106 include `rio-controller/src/main.rs`:** WRONG. Line 3 of THIS document already says "Audit B1 #9: controller has no gRPC server — kube reconcile loop + raw-TCP /healthz only. No tonic ingress to protect." And `r[gw.jwt.verify]` at [`gateway.md:495`](../../docs/src/components/gateway.md) confirms: "(Controller has no gRPC ingress — kube reconcile loop + raw-TCP /healthz only.)" **Remove `rio-controller/src/main.rs` from T2 scope and json-files fence.** The exit-criterion grep at line 90 is already correct (`wc -l` → 2, scheduler+store only) — the T2 prose and json-files fence never caught up. Also fix the `json deps` note at line 130: "EXIT GREP wc -l = 3" → "= 2".
>
> **Contradiction 2 — T3 line 57 `if let Some(jti) = &request.jwt_jti`:** WRONG. This reads a proto body field. Line 5 of THIS document says "Scheduler reads `jti` from the interceptor-attached `Claims` extension — NO proto body field." Per [P0258's pre-impl correction](plan-0258-jwt-issuance-gateway.md) the `SubmitBuildRequest.jwt_jti` proto field **will not exist**. T3's code should be:
> ```rust
> // Claims are attached by jwt_interceptor (T1). jti is always present
> // in a valid token (r[gw.jwt.claims]); if extensions().get() is None
> // the interceptor didn't run, which is a wiring bug not an auth state.
> let claims = req.extensions().get::<jwt::Claims>()
>     .ok_or_else(|| Status::internal("jwt_interceptor not wired"))?;
> let revoked: bool = sqlx::query_scalar!(
>     "SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)",
>     &claims.jti
> ).fetch_one(&pool).await?;
> ```
> The `Option` wrapping disappears (jti is required in `Claims`); the extension fetch replaces the proto field read.


tonic interceptor: extract `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request extensions. **Wired in TWO `main.rs` files** — scheduler, store. (Audit B1 #9: controller has no gRPC server — kube reconcile loop + raw-TCP /healthz only. No tonic ingress to protect.) Per R11: easy to miss one, creating an unauth'd backdoor. Exit criterion includes a 2-way grep count.

**USER Q4 + Audit A #2:** scheduler ADDITIONALLY checks `jti NOT IN jwt_revoked` (PG table from [P0249](plan-0249-migration-batch-014-015-016.md) migration 016). Gateway stays PG-free. Scheduler reads `jti` from the interceptor-attached `Claims` extension — NO proto body field. Store does NOT check revocation (scheduler is the ingress choke point for builds; store trusts scheduler-validated requests).

Closes the GT15 caveat [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) added to `proto.md:252` — this plan makes the present-tense claim true.

## Entry criteria

- [P0258](plan-0258-jwt-issuance-gateway.md) merged (tokens are minted + forwarded)
- [P0249](plan-0249-migration-batch-014-015-016.md) merged (`jwt_revoked` table exists for scheduler check)

## Tasks

### T1 — `feat(common):` JWT interceptor

NEW [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) — pattern from [`rio-proto/src/interceptor.rs`](../../rio-proto/src/interceptor.rs):

```rust
// r[impl gw.jwt.verify]
// Audit B2 #21: Arc<RwLock> for P0260's SIGHUP hot-swap. RwLock plenty
// for annual rotation (read-heavy, ~1 write/year).
pub fn jwt_interceptor(
    pubkey: Arc<RwLock<ed25519_dalek::VerifyingKey>>,
) -> impl tonic::service::Interceptor {
    move |mut req: tonic::Request<()>| {
        let token = req.metadata()
            .get("x-rio-tenant-token")
            .ok_or_else(|| Status::unauthenticated("missing x-rio-tenant-token"))?
            .to_str()
            .map_err(|_| Status::unauthenticated("invalid token encoding"))?;
        let claims = jwt::verify(token, &pubkey)
            .map_err(|e| Status::unauthenticated(format!("JWT verify failed: {e}")))?;
        req.extensions_mut().insert(claims);
        Ok(req)
    }
}
```

### T2 — `feat(scheduler,store,controller):` wire interceptor in 3 main.rs

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs), [`rio-store/src/main.rs`](../../rio-store/src/main.rs), [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) — each server builder:

```rust
.layer(tonic::service::interceptor(jwt_interceptor(jwt_pubkey)))
```

### T3 — `feat(scheduler):` jti revocation check (USER Q4)

In scheduler's `SubmitBuild` handler (NOT in the interceptor — interceptor is shared across services and store/controller have no PG):

```rust
// r[impl gw.jwt.verify] — scheduler-specific revocation check per USER Q4.
// Gateway stays PG-free; scheduler does the lookup in the same codepath
// where it already does tenant-resolve.
if let Some(jti) = &request.jwt_jti {
    let revoked: bool = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)",
        jti
    ).fetch_one(&pool).await?;
    if revoked {
        return Err(Status::unauthenticated("token revoked"));
    }
}
```

### T4 — `docs:` close multi-tenancy.md:19 + remove proto.md:252 caveat

MODIFY [`docs/src/multi-tenancy.md`](../../docs/src/multi-tenancy.md) — close `:19` deferral block.
MODIFY [`docs/src/components/proto.md`](../../docs/src/components/proto.md) — remove the GT15 caveat P0245 added at `:252`.

### T5 — `test:` interceptor rejects invalid/expired

```rust
// r[verify gw.jwt.verify]
#[tokio::test]
async fn invalid_jwt_returns_unauthenticated() { ... }
#[tokio::test]
async fn expired_jwt_returns_unauthenticated() { ... }
#[tokio::test]
async fn revoked_jti_rejected_by_scheduler() {
    // Insert jti into jwt_revoked, attempt SubmitBuild → Status::unauthenticated
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'jwt_interceptor' rio-scheduler/src/main.rs rio-store/src/main.rs | wc -l` → 2 (R11 — both wired; controller has no gRPC ingress)
- `nix develop -c tracey query rule gw.jwt.verify` shows impl + verify

## Tracey

References existing markers:
- `r[gw.jwt.verify]` — T1+T3 implement, T5 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "NEW", "note": "T1: tonic interceptor"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "T1: mod decl"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T2: wire interceptor"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T2: wire interceptor"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T2: wire interceptor"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T3: jti revocation check in SubmitBuild"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T4: close :19 deferral"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T4: remove GT15 caveat at :252"}
]
```

```
rio-common/src/
├── lib.rs                        # mod decl
└── jwt_interceptor.rs            # T1: interceptor (NEW)
rio-scheduler/src/
├── main.rs                       # T2: wire
└── grpc/mod.rs                   # T3: jti revocation check
rio-store/src/main.rs             # T2: wire
rio-controller/src/main.rs        # T2: wire
docs/src/
├── multi-tenancy.md              # T4
└── components/proto.md           # T4
```

## Dependencies

```json deps
{"deps": [258, 249], "soft_deps": [], "note": "R11: EXIT GREP wc -l = 3 (all three main.rs wired). USER Q4: scheduler does jti check, gateway PG-free. scheduler/main.rs + store/main.rs moderate — no 4c touch after P0235."}
```

**Depends on:** [P0258](plan-0258-jwt-issuance-gateway.md) — tokens minted + jti forwarded. [P0249](plan-0249-migration-batch-014-015-016.md) — `jwt_revoked` table for T3.
**Conflicts with:** `scheduler/main.rs` + `store/main.rs` moderate — no 4c touch after P0235 (controller). [P0273](plan-0273-envoy-sidecar-grpc-web.md) does NOT touch scheduler main.rs (Envoy sidecar per USER A1).
