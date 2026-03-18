# Plan 0258: JWT issuance at gateway SSH auth

On successful SSH authentication (after tenant resolution from SSH key comment), mint a JWT and store it on the session context. Forward `jti` in `SubmitBuildRequest.jwt_jti` so the scheduler can check revocation (per USER Q4 — gateway stays PG-free, scheduler does the `jwt_revoked` lookup).

Per spec [`multi-tenancy.md:28`](../../docs/src/multi-tenancy.md): ed25519 signing key from K8s Secret. [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) handles the Secret plumbing + SIGHUP reload; this plan uses whatever key is loaded at startup.

## Entry criteria

- [P0257](plan-0257-jwt-lib-claims-sign-verify.md) merged (`jwt::sign()`, `Claims` struct)

## Tasks

### T1 — `feat(gateway):` mint JWT after tenant resolution

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) — near `:354` (after tenant resolution from SSH-comment — find exact anchor at dispatch):

```rust
// r[impl gw.jwt.issue]
// Mint a per-session JWT. jti is fresh per SSH connect — this is the
// rate-limit key (USER A6, see P0261) and the revocation lookup key
// (USER Q4, scheduler checks jwt_revoked table).
let now = chrono::Utc::now().timestamp();
let claims = jwt::Claims {
    sub: tenant_id,           // UUID, server-resolved
    iat: now,
    exp: now + SSH_SESSION_GRACE_SECS,  // multi-tenancy.md:26
    jti: uuid::Uuid::new_v4().to_string(),
};
let token = jwt::sign(&claims, &jwt_signing_key)?;
session_ctx.jwt_token = Some(token);
session_ctx.jwt_jti = Some(claims.jti.clone());
```

### T2 — `feat(gateway):` inject x-rio-tenant-token header + jti field

MODIFY [`rio-gateway/src/handler/mod.rs`](../../rio-gateway/src/handler/mod.rs) — where outbound gRPC requests are built:

```rust
// Inject JWT on every outbound call to scheduler/store.
if let Some(token) = &session_ctx.jwt_token {
    request.metadata_mut().insert(
        "x-rio-tenant-token",
        MetadataValue::try_from(token.as_str())?,
    );
}
```

Plus: `SubmitBuildRequest` gets a `jwt_jti: Option<String>` field populated from `session_ctx.jwt_jti` — scheduler uses this for the `jwt_revoked` lookup. (Proto field addition is tiny; coordinate with [P0248](plan-0248-types-proto-is-ca-field.md) if both touch the same message — verify at dispatch.)

### T3 — `test(gateway):` issuance unit

```rust
// r[verify gw.jwt.issue]
#[tokio::test]
async fn ssh_auth_mints_jwt_with_tenant_sub() {
    // Mock SSH auth success with tenant_id=X.
    // Assert session_ctx.jwt_token is Some, decodes to Claims{sub=X}.
    // Assert jti is unique across two sessions.
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule gw.jwt.issue` shows impl + verify

## Tracey

References existing markers:
- `r[gw.jwt.issue]` — T1 implements, T3 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T1: mint JWT near :354 after tenant-resolve"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T2: inject x-rio-tenant-token header + jti field"}
]
```

```
rio-gateway/src/
├── server.rs                     # T1: mint JWT after tenant-resolve
└── handler/mod.rs                # T2: header injection + jti forwarding
```

## Dependencies

```json deps
{"deps": [257], "soft_deps": [], "note": "JWT spine hop 2. server.rs moderate collision — no known 4c touch, verify at dispatch. jti forwarding per USER Q4 (gateway PG-free, scheduler checks revocation)."}
```

**Depends on:** [P0257](plan-0257-jwt-lib-claims-sign-verify.md) — `jwt::sign()`, `Claims`.
**Conflicts with:** `server.rs` moderate — check at dispatch, no known 4c touch.
