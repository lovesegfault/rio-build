# Plan 0260: JWT dual-mode config + K8s Secret/ConfigMap + SIGHUP reload

**USER A5: dual-mode is PERMANENT.** The SSH-comment auth branch is NEVER deleted. Two auth paths maintained forever. Operator chooses per-deployment via `gateway.toml auth_mode`. `r[gw.auth.tenant-from-key-comment]` stays unbumped.

K8s plumbing per spec [`multi-tenancy.md:28-31`](../../docs/src/multi-tenancy.md): ed25519 signing key in a K8s Secret, public key distributed via ConfigMap, SIGHUP triggers pubkey reload (for key rotation).

## Entry criteria

- [P0259](plan-0259-jwt-verify-middleware.md) merged (interceptor exists, expects a pubkey)

## Tasks

### T1 — `feat(helm):` JWT Secret + ConfigMap templates

NEW `infra/helm/rio-build/templates/jwt-signing-secret.yaml`:
```yaml
{{- if .Values.jwt.enabled }}
apiVersion: v1
kind: Secret
metadata:
  name: rio-jwt-signing
type: Opaque
data:
  ed25519_seed: {{ .Values.jwt.signingSeed | b64enc }}
{{- end }}
```

NEW `infra/helm/rio-build/templates/jwt-pubkey-configmap.yaml`:
```yaml
{{- if .Values.jwt.enabled }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: rio-jwt-pubkey
data:
  ed25519_pubkey: {{ .Values.jwt.publicKey }}
{{- end }}
```

`values.yaml`: `jwt: { enabled: false, signingSeed: "", publicKey: "" }`.

### T2 — `feat(common):` jwt_required config flag

MODIFY [`rio-common/src/config.rs`](../../rio-common/src/config.rs):

```rust
/// Dual-mode PERMANENT (USER A5). false = SSH-comment fallback allowed;
/// true = JWT required (reject if x-rio-tenant-token absent).
/// Operator choice per-deployment.
pub jwt_required: bool,  // default false
```

### T3 — `feat(common):` SIGHUP pubkey reload

MODIFY [`rio-common/src/signal.rs`](../../rio-common/src/signal.rs) — extend the existing `shutdown_signal` pattern with a SIGHUP handler that re-reads the pubkey ConfigMap mount. The interceptor from P0259 reads from an `Arc<RwLock<VerifyingKey>>` so reload is a write-lock swap.

### T4 — `feat(gateway):` dual-mode branch

MODIFY gateway auth path — two-branched:
```rust
// r[impl gw.jwt.dual-mode]
// USER A5: dual-mode PERMANENT. Both branches stay maintained.
let tenant_id = if let Some(token) = req.metadata().get("x-rio-tenant-token") {
    let claims = jwt::verify(token, &pubkey)?;
    claims.sub
} else if !config.jwt_required {
    // SSH-comment fallback — r[gw.auth.tenant-from-key-comment] STANDS
    resolve_tenant_from_ssh_comment(&ssh_key_comment)?
} else {
    return Err(Status::unauthenticated("JWT required, x-rio-tenant-token absent"));
};
```

### T5 — `test(vm):` both auth paths resolve same tenant

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) — TAIL fragment (serial after [P0255](plan-0255-quota-reject-submitbuild.md)'s fragment):

```nix
# r[verify gw.jwt.dual-mode]  (col-0 header comment)
# SSH-comment auth → tenant X. JWT auth with sub=X → same tenant.
# Both paths produce the same build attribution.
```

### T6 — `docs:` close integration.md:19 deferral

MODIFY [`docs/src/integration.md`](../../docs/src/integration.md) — close the `:19` deferral block.

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule gw.jwt.dual-mode` shows impl + verify
- `rg 'gw.auth.tenant-from-key-comment' docs/src/components/gateway.md` — marker STILL PRESENT, unbumped (dual-mode preserves it)

## Tracey

References existing markers:
- `r[gw.jwt.dual-mode]` — T4 implements, T5 verifies (seeded by P0245, does NOT bump `r[gw.auth.tenant-from-key-comment]`)
- `r[gw.auth.tenant-from-key-comment]` — referenced-as-preserved in T4 fallback branch (NOT bumped per USER A5)

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/jwt-signing-secret.yaml", "action": "NEW", "note": "T1: ed25519 Secret"},
  {"path": "infra/helm/rio-build/templates/jwt-pubkey-configmap.yaml", "action": "NEW", "note": "T1: pubkey ConfigMap"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T1: jwt.enabled/signingSeed/publicKey"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "T2: jwt_required bool"},
  {"path": "rio-common/src/signal.rs", "action": "MODIFY", "note": "T3: SIGHUP pubkey reload"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T4: dual-mode branch"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T5: TAIL fragment (serial after P0255)"},
  {"path": "docs/src/integration.md", "action": "MODIFY", "note": "T6: close :19 deferral"}
]
```

```
infra/helm/rio-build/templates/
├── jwt-signing-secret.yaml       # T1: NEW
└── jwt-pubkey-configmap.yaml     # T1: NEW
rio-common/src/
├── config.rs                     # T2: jwt_required
└── signal.rs                     # T3: SIGHUP reload
rio-gateway/src/
└── server.rs                     # T4: dual-mode branch
nix/tests/scenarios/
└── security.nix                  # T5: TAIL (after P0255)
```

## Dependencies

```json deps
{"deps": [259], "soft_deps": [255], "note": "USER A5: dual-mode PERMANENT. SSH-comment branch NEVER deleted. security.nix TAIL-append soft-serial after P0255. r[gw.auth.tenant-from-key-comment] unbumped."}
```

**Depends on:** [P0259](plan-0259-jwt-verify-middleware.md) — interceptor + pubkey consumer.
**Conflicts with:** `security.nix` TAIL-append soft-serial after [P0255](plan-0255-quota-reject-submitbuild.md). `server.rs` serial after [P0258](plan-0258-jwt-issuance-gateway.md) (transitive).
