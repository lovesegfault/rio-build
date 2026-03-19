# Plan 343: Extract `spawn_health_plaintext` — 3× main.rs duplication → rio-common

Consolidator finding (mc=77). [P0259](plan-0259-jwt-verify-middleware.md) was the 10th commit in sprint-1 that touched ≥2 of the three service `main.rs` files in lockstep (`jwt_interceptor(None)` layer install). [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) (in-flight) will be the 11th (SIGHUP reload). Each touch re-grows the paired-diff surface. The three files share two near-identical patterns:

**Pattern (a) — plaintext health server spawn** (~15L each, 3 copies):
- [`rio-scheduler/src/main.rs:644-652`](../../rio-scheduler/src/main.rs) — `spawn_monitored("health-plaintext", ...)` inside `if let Some(ref tls) = server_tls`
- [`rio-store/src/main.rs:462-470`](../../rio-store/src/main.rs) — same, inside `if server_tls.is_some()`
- [`rio-gateway/src/main.rs:329-338`](../../rio-gateway/src/main.rs) — same, but **unconditional** (gateway's main listener is SSH, not tonic — health server is always separate)

**Pattern (b) — JWT pubkey loader + SIGHUP swap** — coordinator already steered [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) to put this in [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) alongside the existing `JwtPubkey` type at [`:71`](../../rio-common/src/jwt_interceptor.rs). **This plan does NOT cover (b)** — P0260 owns it.

**This plan extracts (a) only.** NEW [`rio-common/src/server.rs`](../../rio-common/src/server.rs) with `spawn_health_plaintext(health_service, health_addr, shutdown)`. Three call sites become one-liners.

The three copies are byte-divergent in ways that matter for the helper signature:
- **Scheduler/store**: gated on `server_tls.is_some()` (only spawn plaintext when main port is mTLS). Gateway: unconditional.
- **Scheduler/store**: clone `health_service` BEFORE the conditional (see [`scheduler/main.rs:632`](../../rio-scheduler/src/main.rs) "cloning shares the underlying state"). Gateway: moves the original.
- **Log message**: scheduler/store say "spawning plaintext health server for K8s probes (mTLS on main port)"; gateway says "starting gRPC health server".
- **Task name**: scheduler/store use `"health-plaintext"`; gateway uses `"health-server"`.

The helper should NOT decide when to spawn (caller knows whether TLS is on, and gateway always wants it). It should just wrap the tonic-serve boilerplate: `Server::builder().add_service(h).serve_with_shutdown(addr, shutdown).await` plus the error-log on exit.

## Entry criteria

- [P0259](plan-0259-jwt-verify-middleware.md) merged — **DONE** (the 10th paired-main.rs commit; this plan prevents the pattern continuing for health-server touches)

## Tasks

### T1 — `feat(common):` add `rio-common/src/server.rs` with `spawn_health_plaintext`

NEW [`rio-common/src/server.rs`](../../rio-common/src/server.rs):

```rust
//! Tonic server startup boilerplate shared by scheduler/store/gateway main.rs.
//!
//! Extracted from three near-identical copies (scheduler:644, store:462,
//! gateway:329). Each was ~15L of the same `Server::builder().add_service(health)
//! .serve_with_shutdown(...).await` inside a spawn_monitored. Any tonic-health
//! upgrade or shutdown-signal change had to be three-way synced.

use std::net::SocketAddr;

use tokio_util::sync::CancellationToken;
use tonic_health::server::HealthService;
use tonic_health::pb::health_server::HealthServer;

use crate::task::spawn_monitored;

/// Spawn a plaintext tonic server with ONLY `grpc.health.v1.Health`, on a
/// dedicated port, sharing the SAME `HealthReporter` state as the caller's
/// main server.
///
/// **Why separate port:** K8s gRPC readiness probes can't do mTLS. When the
/// main port is mTLS, the probe needs a plaintext endpoint. The health_service
/// passed here is a `.clone()` of the one on the main port — cloning
/// `HealthServer<HealthService>` shares the underlying `Arc<RwLock<HashMap>>`
/// status map, so `set_serving()` / `set_not_serving()` on the reporter
/// propagates to BOTH ports. See `r[sched.health.shared-reporter]`.
///
/// **Why `cancelled_owned`:** the spawned task outlives the caller's stack
/// frame, so it needs owned access to the token. Pass the CHILD token (not
/// the parent) — health server should survive the drain window same as the
/// main server (K8s probe gets NOT_SERVING during drain, not ECONNREFUSED).
///
/// **Caller decides whether to call this.** Scheduler/store gate on
/// `server_tls.is_some()` (only need plaintext when main is mTLS). Gateway
/// always calls (its main listener is SSH, not tonic — health is always
/// separate).
pub fn spawn_health_plaintext(
    health_service: HealthServer<HealthService>,
    health_addr: SocketAddr,
    shutdown: CancellationToken,
) {
    tracing::info!(addr = %health_addr, "spawning plaintext health server for K8s probes");
    spawn_monitored("health-plaintext", async move {
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_shutdown(health_addr, shutdown.cancelled_owned())
            .await
        {
            tracing::error!(error = %e, "plaintext health server failed");
        }
    });
}
```

MODIFY [`rio-common/src/lib.rs`](../../rio-common/src/lib.rs) at [`:18`](../../rio-common/src/lib.rs) — add `pub mod server;` after `pub mod signal;`.

**Check at dispatch:** `rio-common/Cargo.toml` — may not already depend on `tonic-health`. If not, add `tonic-health = { workspace = true }`. All three call sites already depend on it (the dep exists at workspace level, just may not be in rio-common's feature set).

### T2 — `refactor(scheduler):` scheduler main.rs uses the helper

MODIFY [`rio-scheduler/src/main.rs:632-661`](../../rio-scheduler/src/main.rs):

```rust
// Before: ~20 lines of clone + conditional + spawn_monitored block.
// After:
let server_tls = rio_common::tls::load_server_tls(&cfg.tls)
    .map_err(|e| anyhow::anyhow!("server TLS config: {e}"))?;

// r[impl sched.health.shared-reporter]
// HealthServer<HealthService> clone shares the status map — the
// health-toggle loop writes once, both ports see it.
if server_tls.is_some() {
    rio_common::server::spawn_health_plaintext(
        health_service.clone(),
        cfg.health_addr,
        serve_shutdown.clone(),
    );
    info!("server mTLS enabled — clients must present CA-signed certs");
}
```

The existing `r[impl sched.health.shared-reporter]` annotation at [`:625`](../../rio-scheduler/src/main.rs) moves to the `.clone()` call (the shared-state guarantee is the clone, not the spawn). The `let _ = tls;` unused-borrow-suppression hack at [`:660`](../../rio-scheduler/src/main.rs) goes away (changed `if let Some(ref tls)` → `if server_tls.is_some()`).

### T3 — `refactor(store):` store main.rs uses the helper

MODIFY [`rio-store/src/main.rs:446-472`](../../rio-store/src/main.rs) — same shape as T2:

```rust
let server_tls = rio_common::tls::load_server_tls(&cfg.tls)
    .map_err(|e| anyhow::anyhow!("server TLS config: {e}"))?;
if server_tls.is_some() {
    rio_common::server::spawn_health_plaintext(
        health_service.clone(),
        cfg.health_addr,
        serve_shutdown.clone(),
    );
    info!("server mTLS enabled — clients must present CA-signed certs");
}
```

Drops the existing comment block at [`:446-452`](../../rio-store/src/main.rs) ("Same pattern as scheduler: K8s gRPC probes can't do mTLS, ...") — that comment lives in the helper's doc now.

### T4 — `refactor(gateway):` gateway main.rs uses the helper

MODIFY [`rio-gateway/src/main.rs:327-338`](../../rio-gateway/src/main.rs). Gateway is unconditional (main listener is SSH):

```rust
// Gateway has no tonic main-server (SSH accept loop instead). Health
// is always on a separate plaintext port. Same shared-reporter pattern
// applies: the SIGTERM drain loop at :291-315 flips NOT_SERVING via
// this reporter.
rio_common::server::spawn_health_plaintext(
    health_service,
    cfg.health_addr,
    serve_shutdown.clone(),
);
```

The gateway currently passes `health_service` (moved, not cloned) — fine, it's the only use. The task name changes from `"health-server"` to `"health-plaintext"` (consolidated). Check: any log-grep test or alert rule matching `task = "health-server"` — grep `infra/` and `nix/tests/` for `health-server` at dispatch; rename in lockstep or add a note.

## Exit criteria

- `/nbr .#ci` green
- T1: `grep 'pub fn spawn_health_plaintext' rio-common/src/server.rs` → 1 hit
- T1: `grep 'pub mod server' rio-common/src/lib.rs` → 1 hit
- T1: `cargo build -p rio-common` → success (proves `tonic-health` dep present or added)
- T2-T4: `grep -c 'spawn_health_plaintext\|server::spawn_health' rio-scheduler/src/main.rs rio-store/src/main.rs rio-gateway/src/main.rs` → ≥3 total (each file calls the helper)
- T2-T4: `grep -c 'Server::builder().*add_service(health_service' rio-scheduler/src/main.rs rio-store/src/main.rs rio-gateway/src/main.rs` → ≤3 total (only the MAIN tonic server in scheduler/store — gateway should be 0 since its main is SSH). The plaintext-health copies are gone.
- T2-T4 net line count: `wc -l rio-{scheduler,store,gateway}/src/main.rs` before/after — expect net ~−30L across the three (each ~15L block → ~5L call + import)
- T2: `grep 'r\[impl sched.health.shared-reporter\]' rio-scheduler/src/main.rs` → 1 hit (annotation moved, not dropped)
- T2: `grep 'let _ = tls' rio-scheduler/src/main.rs` → 0 (unused-borrow hack removed with the `if let Some(ref tls)` → `is_some()` refactor)
- T4: `grep 'task = "health-server"\|"health-server"' infra/ nix/tests/ -r` — any hit → rename in lockstep to `"health-plaintext"` OR leave the gateway's task name as a param (prefer rename — one name for one thing)
- `nix develop -c tracey query rule sched.health.shared-reporter` shows the `impl` site (moved, still present)

## Tracey

References existing markers:
- `r[sched.health.shared-reporter]` — T2 moves (not drops) the `r[impl]` annotation. The helper's doc comment references this marker. The guarantee ("cloned HealthServer shares status map") now lives at two places: the helper's doc (generic) and scheduler's call site (scheduler-specific: "toggle loop writes once, both see it").

No new markers. This is pure dedup — no behavior change, no new spec surface. Store and gateway don't have their own `r[*.health.shared-reporter]` markers (store's comment at [`main.rs:446-452`](../../rio-store/src/main.rs) says "same architectural pattern — no reason to diverge"; gateway's unconditional call doesn't need the TLS-vs-plaintext split justification). The one scheduler marker covers the pattern.

## Files

```json files
[
  {"path": "rio-common/src/server.rs", "action": "NEW", "note": "T1: spawn_health_plaintext helper — wraps tonic Server::builder + health_service + serve_with_shutdown + error-log, ~35L incl doc"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "T1: +pub mod server; after :18 pub mod signal"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "T1: +tonic-health dep IF not already present (workspace-level dep exists, may need local entry)"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T2: :625-661 spawn block → helper call; move r[impl sched.health.shared-reporter] to .clone() line; drop let _ = tls hack"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T3: :446-472 spawn block → helper call; drop 'Same pattern as scheduler' comment (lives in helper doc now)"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T4: :327-338 spawn block → helper call (unconditional — gateway's main is SSH); task name 'health-server' → 'health-plaintext' (grep infra/ nix/tests/ for alert-rule renames)"}
]
```

```
rio-common/src/
├── server.rs                 # T1: NEW — spawn_health_plaintext
├── lib.rs                    # T1: +pub mod server
└── Cargo.toml                # T1: tonic-health dep (if needed)
rio-scheduler/src/main.rs     # T2: ~−15L, spawn block → helper
rio-store/src/main.rs         # T3: ~−15L, spawn block → helper
rio-gateway/src/main.rs       # T4: ~−10L, spawn block → helper
```

## Dependencies

```json deps
{"deps": [259], "soft_deps": [260], "note": "Consolidator finding (mc=77): 10th paired-main.rs commit was P0259's jwt_interceptor(None) install. This plan extracts pattern (a) spawn_health_plaintext — 3 copies, ~15L each. Pattern (b) pubkey-loader+SIGHUP is P0260's responsibility (coordinator already steered; JwtPubkey type at jwt_interceptor.rs:71 is the hot-swap handle). discovered_from=consolidator (mc=77). Soft-dep P0260: it touches the same three main.rs files (SIGHUP reload install). If P0260 lands first, T2-T4's line refs drift. If THIS lands first, P0260 has less context to copy-paste (it can use the helper's shutdown-token pattern as a template for its SIGHUP-reload spawn). Sequence preference: THIS plan first — it's pure refactor, shrinks the surface P0260 touches."}
```

**Depends on:** [P0259](plan-0259-jwt-verify-middleware.md) — merged (DONE). Not a hard code dependency (the health-spawn blocks predate P0259), but the consolidator's "10th paired-diff" count is as-of-P0259.

**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) count=30, [`rio-store/src/main.rs`](../../rio-store/src/main.rs) count=26, [`rio-gateway/src/main.rs`](../../rio-gateway/src/main.rs) count=<20 (all HOT per collision matrix). T2-T4 each delete ~15L and insert ~5L at well-delimited blocks (the `spawn_monitored("health-plaintext", ...)` call is a clear anchor). [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) (UNIMPL) touches all three at the `jwt_interceptor(None)` → `jwt_interceptor(pubkey)` swap site AND adds a SIGHUP handler — different sections (jwt-layer vs health-spawn), but same-file × 3. Sequence THIS first: smaller diff for P0260. [`rio-common/src/lib.rs`](../../rio-common/src/lib.rs) — one-line append, zero conflict. [`rio-common/Cargo.toml`](../../rio-common/Cargo.toml) — may conflict with P0260 if it also adds a dep to rio-common; both additive, trivial merge.
