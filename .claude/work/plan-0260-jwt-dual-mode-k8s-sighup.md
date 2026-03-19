# Plan 0260: JWT dual-mode config + K8s Secret/ConfigMap + SIGHUP reload

**USER A5: dual-mode is PERMANENT.** The SSH-comment auth branch is NEVER deleted. Two auth paths maintained forever. Operator chooses per-deployment via `gateway.toml auth_mode`. `r[gw.auth.tenant-from-key-comment]` stays unbumped.

K8s plumbing per spec [`multi-tenancy.md:28-31`](../../docs/src/multi-tenancy.md): ed25519 signing key in a K8s Secret, public key distributed via ConfigMap, SIGHUP triggers pubkey reload (for key rotation).

## Entry criteria

- [P0259](plan-0259-jwt-verify-middleware.md) merged (interceptor exists, expects a pubkey)

## Dispatch note — `Claims.sub:Uuid` vs `tenant_name:String` resolution gap

**Bughunter independently found what [P0258](plan-0258-jwt-issuance-gateway.md) already worked around.** P0258 merged at [`8ed0b368`](https://github.com/search?q=8ed0b368&type=commits) with the JWT mint block **commented out** behind a `TODO(P0260)` gate at [`server.rs:452-475`](../../rio-gateway/src/server.rs). The impl left a clean anchor: `if let Some(signing_key) = &self.jwt_signing_key { let _ = signing_key; /* commented mint */ }`. `main.rs` never calls `with_jwt_signing_key`, so the block is dead code until this plan closes it.

The root tension: [`Claims.sub`](../../rio-common/src/jwt.rs) at `:46` is `Uuid`. The gateway at mint-time has only `tenant_name: String` (from the `authorized_keys` comment — [`server.rs:445`](../../rio-gateway/src/server.rs)). Per `r[sched.tenant.resolve]` at [`scheduler.md:91`](../../docs/src/components/scheduler.md), **the scheduler owns the `tenants` table** — the gateway is PG-free for stateless N-replica HA. Name→UUID resolution is a scheduler-side operation that runs at `submit_build` time, not auth time.

**Four options** — P0258's TODO at `:458-460` lists two; bughunter adds two more. **Choose one** before implementing T4:

| | Approach | Cost | Spec impact |
|---|---|---|---|
| **(a)** | `ResolveTenant` gRPC RPC — gateway→scheduler round-trip in `auth_publickey` before minting | +1 sync RPC in the SSH-auth hot path (every connect). Scheduler already has the `SELECT tenant_id FROM tenants WHERE tenant_name = $1` query (per `r[sched.tenant.resolve]`); this just exposes it. ~1-2ms if warm; can spike under PG load. Makes SSH auth latency depend on scheduler availability. | None — `r[gw.jwt.issue]` says "On successful SSH authentication, mint with `sub` = resolved UUID"; this resolves at auth time, which is what the spec wants. New RPC under `r[sched.tenant.resolve]`. |
| **(b)** | Defer mint until after `submit_build` returns (scheduler response carries the resolved UUID) | Zero extra RPC — piggy-backs on the first build submission. But: a session that connects and never submits a build never gets a JWT. More importantly, the JWT is supposed to be attached to **every** gRPC call including the first `submit_build` itself — chicken-and-egg. | **BREAKS `r[gw.jwt.issue]`.** Spec says "On successful SSH auth" — `:491`. Deferring to post-submit changes the timing contract. Would need a `tracey bump`. |
| **(c)** | `Claims.sub: String` (tenant name, not UUID) | Zero RPC, zero spec-timing change. Tenant names are bounded-keyspace (operator-assigned, `tenants` table is small), so rate-limiting on `sub` still works (per the `:73` comment at [`jwt.rs`](../../rio-common/src/jwt.rs) — "rate-limit keys off `sub`, bounded keyspace"). | **BREAKS `r[gw.jwt.claims]`** at [`gateway.md:487`](../../docs/src/components/gateway.md): "`sub` = tenant_id UUID (server-resolved)". Every downstream consumer of `Claims.sub` (scheduler handlers, store filter) expects `Uuid` and would need to re-resolve. Pushes the problem around rather than solving it. |
| **(d)** | `authorized_keys` comment convention carries UUID directly (`tenant-uuid=<uuid>` instead of `<name>`) | Zero RPC, zero type change. Operator sets the UUID when adding the key. Gateway parses it, trusts it (comment is server-side, can't be client-forged). | Operational burden: key-add becomes a two-step (look up UUID, then add key). Easy to fat-finger. Migrating existing keys needs a rewrite. `r[gw.auth.tenant-from-key-comment]` text would need updating (currently says "tenant name"). |

**Recommendation: (a)**, with a cached-per-connection result. The round-trip happens once per SSH connect, not per-request. A `tokio::time::timeout(500ms)` wrapper degrades gracefully: on scheduler-unreachable, fall back to the SSH-comment branch (T4's dual-mode `else if !config.jwt_required` arm covers this — the session proceeds with `tenant_name` attribution, JWT absent). This preserves the PG-free-gateway property (no direct DB connection) and doesn't change any spec text.

**If choosing (a):** add a T0 to this plan: `feat(proto): ResolveTenant RPC` — request `{tenant_name: string}`, response `{tenant_id: string /* UUID */}`, `InvalidArgument` on unknown. Wire it in [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) (count=33, hot file — check collision frontier). Add `r[impl sched.tenant.resolve]` to the handler (the existing annotation is presumably on the `submit_build` inline query — check at dispatch with `tracey query rule sched.tenant.resolve`).

---

**ESCALATION (bughunter mc70):** two constraints that NARROW the option space to **(a) or (d) only**.

**Option (c) is FORECLOSED.** [P0257](plan-0257-jwt-lib-claims-sign-verify.md) merged `Claims.sub: Uuid` at [`jwt.rs:46`](../../rio-common/src/jwt.rs) with proptest coverage (`jwt_roundtrip(sub: Uuid, ...)`). Changing it to `String` now means: (1) breaking the merged type, (2) invalidating the proptest, (3) re-resolving `sub` to UUID at every downstream consumer — the scheduler handlers and store filter that the (c) row above already said "would need to re-resolve." P0257 made the cost concrete. Strike (c).

**Option (b) was already struck** by the table (chicken-and-egg with the first `submit_build`). No change.

**T4's snippet is wrong-layer.** The snippet at `:82-90` below reads `req.metadata().get("x-rio-tenant-token")` — that's the gRPC-interceptor-side check, the VERIFY half. But the gateway is the ISSUER — it MINTS the token in `auth_publickey`. The verify half lives in the scheduler/store interceptor (P0259's layer). The gateway never reads `x-rio-tenant-token` back from a header; it attaches the minted token to outbound gRPC calls. T4 needs a rewrite: the dual-mode branch is "did I successfully mint?" not "did the request carry a token?". Rough shape:

```rust
// In auth_publickey, after the commented mint block at server.rs:496-506:
// r[impl gw.jwt.dual-mode]
if self.jwt_token.is_some() {
    // JWT mode: minted token will be attached to outbound gRPC metadata
    // by the session's interceptor layer. tenant_id already resolved
    // (via ResolveTenant RPC if option (a), or parsed from authorized_keys
    // UUID comment if option (d)).
} else if config.jwt_required {
    // Hard-fail: jwt_required=true but mint failed (no signing key loaded,
    // or ResolveTenant returned InvalidArgument). Reject the SSH auth.
    return Ok(Auth::reject());
} else {
    // Dual-mode fallback: r[gw.auth.tenant-from-key-comment] STANDS.
    // Session proceeds with tenant_name attribution, no JWT on outbound calls.
}
```

**Remaining option space: (a) ResolveTenant RPC, (d) authorized_keys UUID convention.** Both viable. (a) is more work (proto change, hot-file edit) but zero operator burden. (d) is less code but pushes UUID-lookup onto the key-add operator workflow.

**P0260 is TERMINAL for this gap** (P0261 is RESERVED — not for JWT). The gate at [`server.rs:483-506`](../../rio-gateway/src/server.rs) stays dead code until this plan decides and closes it.

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
{"deps": [259, 258], "soft_deps": [255], "note": "USER A5: dual-mode PERMANENT. SSH-comment branch NEVER deleted. security.nix TAIL-append soft-serial after P0255. r[gw.auth.tenant-from-key-comment] unbumped. P0258 dep: closes the TODO(P0260) gate at server.rs:452-475 — P0258 left the mint block commented out pending the name→UUID resolution decision above. See Dispatch note — option (a) adds a ResolveTenant RPC as T0; (d) avoids RPC but needs authorized_keys migration. (b)/(c) both break spec markers."}
```

**Depends on:** [P0259](plan-0259-jwt-verify-middleware.md) — interceptor + pubkey consumer.
**Conflicts with:** `security.nix` TAIL-append soft-serial after [P0255](plan-0255-quota-reject-submitbuild.md). `server.rs` serial after [P0258](plan-0258-jwt-issuance-gateway.md) (transitive).
