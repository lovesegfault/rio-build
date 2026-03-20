# Plan 0349: Wire `spawn_pubkey_reload` in scheduler+store main.rs — close P0260 orphan

[P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) post-PASS review. `spawn_pubkey_reload` at [`jwt_interceptor.rs:147`](../../rio-common/src/jwt_interceptor.rs) (p260 worktree ref) is ORPHANED — zero prod callers. [`scheduler/main.rs:697`](../../rio-scheduler/src/main.rs) and [`store/main.rs:496`](../../rio-store/src/main.rs) still pass `jwt_interceptor(None)`. P0260 built the helper but did NOT wire it (the P0260 files-fence lacks scheduler/store main.rs). Same orphan pattern as [P0338](plan-0338-tenant-signer-wiring-putpath.md)'s `TenantSigner` — helper exists, nobody calls it.

[P0343](plan-0343-extract-spawn-health-plaintext-common.md) explicitly scopes this OUT at [`:10`](plan-0343-extract-spawn-health-plaintext-common.md): "Pattern (b) pubkey-loader+SIGHUP is P0260's responsibility — **This plan does NOT cover (b)**." P0260 owns the HELPER; nobody owns the WIRING. This plan closes the gap.

**CONDITIONAL:** Coordinator steered P0343 at runtime to also wire this. If P0343's final commits wire `spawn_pubkey_reload` in scheduler+store main.rs → close this plan as **SUPERSEDED** at dispatch. Verify with `grep spawn_pubkey_reload rio-scheduler/src/main.rs rio-store/src/main.rs` — if ≥2 hits, P0343 handled it.

The ConfigMap mount is already in helm templates (`jwt-pubkey-configmap.yaml` — grep `infra/helm/` at dispatch). The only missing piece is the main.rs wiring.

> **[ERRATUM — bughunter mc=112]:** The ConfigMap OBJECT exists in
> helm; the volumeMount/volume/env-var do NOT. scheduler.yaml + store.yaml
> + gateway.yaml had zero `jwt` mounts. [P0357](plan-0357-helm-jwt-pubkey-mount.md)
> closes the gap. P0349's main.rs wiring is correct; the Helm half was
> orphaned. Same class as P0272/P0338 helper-exists-nobody-calls-it.

## Entry criteria

- [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) merged — `spawn_pubkey_reload` helper + `JwtPubkey` type + `parse_jwt_pubkey` exist in rio-common
- **CONDITIONAL entry check:** `grep spawn_pubkey_reload rio-scheduler/src/main.rs rio-store/src/main.rs` → if ≥2 hits, SKIP this plan (P0343 steered-wire already handled it); else proceed

## Tasks

### T1 — `fix(scheduler):` wire spawn_pubkey_reload in main.rs

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs). Replace `jwt_interceptor(None)` at `:697` (current sprint-1 line; drifts with P0343 merge — re-grep) with:

```rust
// Load JWT pubkey from K8s ConfigMap mount (if configured). SIGHUP
// reloads from the same path — kubelet remounts the ConfigMap on
// rotation, then sends SIGHUP via the downwardAPI preStop hook.
// r[impl gw.jwt.verify]
let jwt_pubkey = match &cfg.jwt.key_path {
    Some(path) => {
        let initial = rio_common::jwt_interceptor::parse_jwt_pubkey(
            &tokio::fs::read(path).await
                .with_context(|| format!("reading JWT pubkey from {path:?}"))?,
        )?;
        let shared = Arc::new(RwLock::new(initial));
        rio_common::jwt_interceptor::spawn_pubkey_reload(
            Arc::clone(&shared),
            path.clone(),
            serve_shutdown.clone(),
        );
        Some(shared)
    }
    None => {
        tracing::warn!(
            "jwt.key_path unset — interceptor runs in inert mode \
             (all RPCs pass, Claims extension never attached)"
        );
        None
    }
};
// ... (later, in the tonic layer install)
.layer(rio_common::jwt_interceptor::jwt_interceptor(jwt_pubkey))
```

**Check at dispatch:** the exact `Config` field name (`cfg.jwt.key_path` vs `cfg.jwt_key_path` vs env-only `RIO_JWT__KEY_PATH`) — P0260 defines it. Also check if P0260 already adds `jwt: JwtConfig` to the scheduler config struct; if not, add it.

### T2 — `fix(store):` wire spawn_pubkey_reload in main.rs

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs). Same shape as T1 at `:496` (current sprint-1 line). Store's config struct may need the same `jwt: JwtConfig` addition if P0260 only added it to gateway.

### T3 — `test(common):` spawn_pubkey_reload integration test

NEW test in [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) `#[cfg(test)]`. Components are tested (per review: `sighup_reload` at [`signal.rs:198,242`](../../rio-common/src/signal.rs), `parse_jwt_pubkey` at `:509,527`), but the wrapper COMPOSITION (`SIGHUP → tokio::fs::read → parse → RwLock write-swap`) is not:

```rust
/// spawn_pubkey_reload: SIGHUP triggers file re-read + RwLock swap.
/// Writes key-A to a tempfile, spawns reload, swaps to key-B on disk,
/// sends SIGHUP (via kill(getpid(), SIGHUP)), polls RwLock until swap
/// observed. Proves the composition works end-to-end.
// r[verify gw.jwt.verify]
#[tokio::test]
async fn sighup_swaps_pubkey() {
    // ... (tempfile with key-A → Arc<RwLock> → spawn_pubkey_reload →
    //      overwrite tempfile with key-B → kill(SIGHUP) → poll-until-swapped)
}
```

**Caveat:** SIGHUP is process-wide. If other tests run concurrently and a SIGHUP handler is installed globally (tokio's `signal::unix::signal(SignalKind::hangup())`), this test may race. Check at dispatch if `sighup_reload` uses a per-call handler or a global. If global, the test may need `#[serial]` or a mock-signal injection point.

### T4 — `docs:` fix stale jwt_interceptor.rs:188 comment

MODIFY [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) at `:188` (p260 worktree ref). Current comment: "P0260 wires the key from K8s ConfigMap mount; until then interceptor is inert". This is stale — P0260 built the helper but didn't wire; THIS plan wires. Update to:

```rust
// Wired in scheduler+store main.rs: cfg.jwt.key_path → parse_jwt_pubkey →
// Arc<RwLock> → spawn_pubkey_reload (SIGHUP swaps). When key_path is unset
// (dev, or pre-key-rotation-infra clusters), interceptor is inert: all RPCs
// pass, Claims extension never attached. See r[gw.jwt.verify].
```

Also fix security.nix:698 self-referencing `TODO(P0260)` (routed to [P0304](plan-0304-trivial-batch-p0222-harness.md) T53 in this batch) — it should re-tag to `TODO(P0349)` if the VM fixture extraServiceEnv work is future, or delete if moot. Cross-reference at dispatch.

## Exit criteria

- `/nixbuild .#ci` green
- T1: `grep 'spawn_pubkey_reload' rio-scheduler/src/main.rs` → ≥1 hit
- T1: `grep 'jwt_interceptor(None)' rio-scheduler/src/main.rs` → 0 hits (the `None` call is gone; replaced with `jwt_pubkey` variable)
- T2: `grep 'spawn_pubkey_reload' rio-store/src/main.rs` → ≥1 hit
- T2: `grep 'jwt_interceptor(None)' rio-store/src/main.rs` → 0 hits
- T3: `cargo nextest run -p rio-common sighup_swaps_pubkey` → pass
- T4: `grep 'P0260 wires' rio-common/src/jwt_interceptor.rs` → 0 hits (stale forward-ref removed)
- T4: `grep 'Wired in scheduler+store\|spawn_pubkey_reload.*SIGHUP swaps' rio-common/src/jwt_interceptor.rs` → ≥1 hit (corrected comment)
- `nix develop -c tracey query rule gw.jwt.verify` shows ≥1 `impl` site in rio-scheduler/src/main.rs or rio-store/src/main.rs (T1/T2 add the annotation)
- `nix develop -c tracey query rule gw.jwt.verify` shows ≥1 `verify` site in rio-common (T3 test annotation)
- **Sanity:** `grep 'jwt_interceptor' rio-gateway/src/main.rs` → gateway does NOT use this (gateway is the JWT ISSUER, not verifier — `r[gw.jwt.issue]` vs `r[gw.jwt.verify]`). If grep finds a call, something is wrong.

## Tracey

References existing markers:
- `r[gw.jwt.verify]` at [`gateway.md:493`](../../docs/src/components/gateway.md) — T1 and T2 implement (scheduler+store verify the JWT the gateway issued); T3 verifies the SIGHUP swap
- `r[gw.jwt.dual-mode]` at [`gateway.md:497`](../../docs/src/components/gateway.md) — T1/T2 implement the "key-present → verify" half of dual-mode (the "key-absent → inert" half is the `None` branch)

No new markers. The helper (`spawn_pubkey_reload`) is plumbing for `r[gw.jwt.verify]`; wiring it makes the marker's `impl` site complete across all three verifier services (scheduler, store; gateway issues not verifies).

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: replace jwt_interceptor(None) at :697 with key-load + spawn_pubkey_reload + interceptor(Some(shared)). r[impl gw.jwt.verify]. HOT count=31."},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T2: same as T1 at :496. HOT count=27."},
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "MODIFY", "note": "T3: +sighup_swaps_pubkey integration test; T4: fix :188 stale 'P0260 wires' comment. p260 worktree ref :147 for spawn_pubkey_reload def."},
  {"path": "rio-scheduler/src/config.rs", "action": "MODIFY", "note": "T1: +jwt: JwtConfig field IF P0260 didn't add it (check at dispatch — P0260 may have added only to gateway config)"},
  {"path": "rio-store/src/config.rs", "action": "MODIFY", "note": "T2: +jwt: JwtConfig field IF P0260 didn't add it"}
]
```

```
rio-scheduler/src/
├── main.rs                       # T1: wire spawn_pubkey_reload
└── config.rs                     # T1: (maybe) +jwt config field
rio-store/src/
├── main.rs                       # T2: wire spawn_pubkey_reload
└── config.rs                     # T2: (maybe) +jwt config field
rio-common/src/jwt_interceptor.rs # T3: +test; T4: fix comment
```

## Dependencies

```json deps
{"deps": [260], "soft_deps": [343, 259], "note": "HARD dep P0260: spawn_pubkey_reload helper + JwtPubkey type + parse_jwt_pubkey live in jwt_interceptor.rs (p260 adds at :147). discovered_from=260 (post-PASS review). SOFT P0343: touches scheduler/store main.rs for spawn_health_plaintext extraction — different sections (health-spawn block vs jwt-interceptor layer install), but same files. If P0343 lands first, the :697/:496 line refs drift — re-grep. **CONDITIONAL SUPERSEDE:** coord steered P0343 at runtime to also wire this. If P0343's final commits include spawn_pubkey_reload in scheduler+store main.rs (grep at dispatch), close this plan as SUPERSEDED — the T3 test and T4 comment fix can route to P0304 as follow-on trivials. SOFT P0259: the interceptor itself arrived with P0259 (the None-passing calls are its legacy). P0259 is DONE; transitive. SCHEDULER/STORE main.rs are HOT (31/27) — T1/T2 are ~15L additive blocks at the tonic-layer-install site; same-file as P0343 but different anchor (layer(...) vs spawn_monitored(...))."}
```

**Depends on:** [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) — the helper. [P0343](plan-0343-extract-spawn-health-plaintext-common.md) soft-dep for line-ref drift + conditional-supersede check.

**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) count=31 — [P0343](plan-0343-extract-spawn-health-plaintext-common.md) T2, [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) T1 (config load at `:42`), [P0303](plan-0303-scheduler-gauge-leader-gate.md) (DONE). Each touches different sections; T1 is at the tonic layer-install site. [`rio-store/src/main.rs`](../../rio-store/src/main.rs) count=27 — P0343 T3, [P0295](plan-0295-doc-rot-batch-sweep.md) T19 (StoreServer→StoreServiceImpl rename at `:632`). Non-overlapping.
