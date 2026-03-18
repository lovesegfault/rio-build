# Plan 0257: JWT lib — claims + sign/verify (rio-common)

Per GT8: the spec is fully written at [`multi-tenancy.md:19-31`](../../docs/src/multi-tenancy.md) — `sub`/`iat`/`exp`/`jti` claims, ed25519 K8s Secret, `x-rio-tenant-token` header. [`rio-common/src/hmac.rs`](../../rio-common/src/hmac.rs) has a `Claims` struct for assignment-token HMAC — same module shape, different key.

**USER A6 impact:** `jti` is used as the rate-limit key (per-session, unbounded keyspace) — but that's [P0261](plan-0261-governor-lru-eviction.md)'s problem. This plan just defines the claim.

Only new Cargo dep for the entire JWT lane: `jsonwebtoken`.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[gw.jwt.claims]` seeded)

## Tasks

### T1 — `feat(common):` jwt module

NEW [`rio-common/src/jwt.rs`](../../rio-common/src/jwt.rs) — pattern from `hmac.rs`:

```rust
// r[impl gw.jwt.claims]
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// tenant_id UUID (server-resolved at mint — see r[sched.tenant.resolve])
    pub sub: Uuid,
    /// issued-at, unix epoch
    pub iat: i64,
    /// expiry, unix epoch (SSH session duration + grace per multi-tenancy.md:26)
    pub exp: i64,
    /// unique token ID — used for revocation (PG jwt_revoked table via
    /// scheduler) AND as the rate-limit key (per USER A6, see P0261)
    pub jti: String,
}

pub fn sign(claims: &Claims, key: &ed25519_dalek::SigningKey) -> Result<String> {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA),
        claims,
        &jsonwebtoken::EncodingKey::from_ed_der(&key.to_pkcs8_der()?),
    ).map_err(Into::into)
}

pub fn verify(token: &str, pubkey: &ed25519_dalek::VerifyingKey) -> Result<Claims> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_ed_der(&pubkey.to_public_key_der()?),
        &validation,
    )?;
    Ok(decoded.claims)
}
```

Module decl in `rio-common/src/lib.rs`.

### T2 — `feat(deps):` add jsonwebtoken

MODIFY [`Cargo.toml`](../../Cargo.toml) workspace deps: `jsonwebtoken = "9"`. Enable in `rio-common/Cargo.toml`.

### T3 — `test(common):` proptest roundtrip

```rust
// r[verify gw.jwt.claims]
proptest! {
    #[test]
    fn jwt_roundtrip(sub: Uuid, iat in 0i64..2_000_000_000, exp_delta in 1i64..86400) {
        let claims = Claims { sub, iat, exp: iat + exp_delta, jti: uuid::Uuid::new_v4().to_string() };
        let key = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let token = sign(&claims, &key).unwrap();
        let decoded = verify(&token, &key.verifying_key()).unwrap();
        prop_assert_eq!(decoded.sub, claims.sub);
        prop_assert_eq!(decoded.jti, claims.jti);
    }
}

#[test]
fn expired_jwt_rejected() {
    // exp in the past → verify() returns Err
}

#[test]
fn wrong_key_rejected() {
    // sign with key A, verify with key B → Err
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule gw.jwt.claims` shows impl + verify

## Tracey

References existing markers:
- `r[gw.jwt.claims]` — T1 implements, T3 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-common/src/jwt.rs", "action": "NEW", "note": "T1: Claims struct + sign/verify"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "T1: mod jwt decl"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "T2: jsonwebtoken workspace dep"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "T2: enable jsonwebtoken"}
]
```

```
rio-common/src/
├── lib.rs                        # T1: mod decl
└── jwt.rs                        # T1: Claims + sign/verify (NEW)
Cargo.toml                        # T2: jsonwebtoken dep
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "JWT spine head. MAX PARALLEL — no code-file collision with any 4b/4c plan. Only new dep for entire JWT lane."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — marker seeded.
**Conflicts with:** none. `rio-common/src/jwt.rs` is NEW. `Cargo.toml` workspace deps EOF-append.
