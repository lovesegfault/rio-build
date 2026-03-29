# Plan 476: Scheduler store_client reconnect on store rollout

Manual rsb testing on kind found that `helm upgrade` of the store deployment leaves the scheduler holding a stale gRPC channel. [`rio-scheduler/src/main.rs:335`](../../rio-scheduler/src/main.rs) calls `connect_store_with_retry(&cfg.store_addr, ...)` **once** at startup and passes the resulting `StoreServiceClient<Channel>` into the DAG actor ([`handle.rs:62`](../../rio-scheduler/src/actor/handle.rs)). When the store pod rolls, the old pod terminates, the kube-dns `rio-store` Service re-resolves to the new pod IP, but the scheduler's `Channel` still points at the old IP. RPCs fail with connection-refused; the scheduler doesn't reconnect; substitution silently breaks until the scheduler pod is *also* restarted — and in the right order.

This is the classic "eager connect caches stale DNS" bug. tonic's `Endpoint::connect_lazy()` re-resolves on each connection attempt; combined with HTTP/2 keepalive + idle-timeout the channel transparently reconnects when the underlying TCP connection drops. That's the standard fix — no custom reconnect loop needed.

Discovered during P0473 rsb testing when a store config change required a rollout and subsequent substitution RPCs went dark.

## Entry criteria

- P0473 merged (`ceaebfd6`). Already on `sprint-1`.

## Tasks

### T1 — `fix(scheduler):` use connect_lazy + keepalive for store channel

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) at `connect_store_with_retry` (`:733` doc-comment region). Replace the eager `Endpoint::connect()` with:

```rust
let channel = Endpoint::from_shared(store_addr.clone())?
    .http2_keep_alive_interval(Duration::from_secs(30))
    .keep_alive_timeout(Duration::from_secs(10))
    .keep_alive_while_idle(true)
    .connect_lazy();
```

`connect_lazy()` returns immediately without establishing a connection — first RPC triggers DNS resolution + connect. On connection drop (old pod terminated), next RPC re-resolves and reconnects. The keepalive settings detect half-open connections within ~40s instead of waiting for TCP timeout.

The existing `connect_store_with_retry` retry loop becomes unnecessary for the connect itself (lazy never fails at creation time), but may still be useful as a *readiness probe* — keep it as a pre-flight `HealthCheck` RPC if the scheduler should block startup until store is reachable. Alternatively, drop the retry loop entirely and let the first real RPC surface connection errors with context.

### T2 — `fix(scheduler):` reconnect-on-error wrapper for long-held client

If `connect_lazy` alone is insufficient (tonic's lazy channel caches the resolved IP after first connect and doesn't re-resolve on reconnect — verify against tonic docs/source), add an explicit re-resolve:

```rust
// Wrap StoreServiceClient with a reconnect-on-Unavailable shim.
// tonic's Channel caches the first resolved IP; on store rollout
// the IP changes but DNS re-resolution doesn't happen automatically.
async fn with_reconnect<T, F, Fut>(
    client: &mut StoreServiceClient<Channel>,
    endpoint: &Endpoint,
    f: F,
) -> Result<T, Status>
where F: Fn(&mut StoreServiceClient<Channel>) -> Fut, ...
{
    match f(client).await {
        Err(s) if s.code() == Code::Unavailable => {
            *client = StoreServiceClient::new(endpoint.connect_lazy());
            f(client).await
        }
        r => r,
    }
}
```

**Prefer T1 alone if it works** — verify with a local test (T3) before adding T2's complexity. tonic 0.12+ lazy channels *should* handle reconnect transparently; the wrapper is only needed if empirical testing shows otherwise.

### T3 — `test(vm):` store-rollout reconnect VM test

NEW subtest in [`nix/tests/scenarios/`](../../nix/tests/scenarios/) (likely `lifecycle.nix` or a new `rollout.nix`). Sequence:
1. Deploy scheduler + store, submit a substitution-requiring build, verify success
2. `kubectl rollout restart deployment/rio-store` (or systemd restart in standalone fixture)
3. Wait for new store pod ready
4. Submit another substitution-requiring build, verify success (without scheduler restart)

Before this fix step 4 fails with `Unavailable` / connection-refused.

### T4 — `docs(arch):` note reconnect semantics in scheduler component spec

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — brief note near the store-client section that the channel uses lazy-connect + keepalive for rollout resilience.

## Exit criteria

- `grep 'connect_lazy\|keep_alive_interval' rio-scheduler/src/main.rs` → ≥2 hits
- T3 VM test passes: `nix build .#checks.x86_64-linux.vm-<scenario>-<fixture>` with store-rollout step
- Manual rsb smoke: `helm upgrade rio-store ...` followed by `nix build --store ssh-ng://...` works without scheduler restart
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.merge.substitute-fetch]` — T1 makes the substitute-fetch RPC path resilient to store restarts

Adds new marker to component specs:
- `r[sched.store-client.reconnect]` → [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) (see Spec additions below)

## Spec additions

Add to [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) near the existing store-client prose:

```
r[sched.store-client.reconnect]
The scheduler's gRPC channel to rio-store MUST use lazy connection (`Endpoint::connect_lazy`) with HTTP/2 keepalive so store pod rollouts do not require a scheduler restart. On `Unavailable`, the channel re-resolves DNS and reconnects transparently.
```

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: connect_lazy + keepalive at connect_store_with_retry :733"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "T2 (conditional): reconnect wrapper if lazy alone insufficient"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T3: store-rollout reconnect subtest (or new rollout.nix)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: wire subtest with r[verify sched.store-client.reconnect]"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4 + spec addition: r[sched.store-client.reconnect]"}
]
```

```
rio-scheduler/src/
├── main.rs             # T1: connect_lazy
└── actor/handle.rs     # T2: wrapper (conditional)
nix/tests/
├── scenarios/lifecycle.nix  # T3: rollout subtest
└── default.nix         # T3: wire + r[verify]
docs/src/components/
└── scheduler.md        # T4 + spec
```

## Dependencies

```json deps
{"deps": [473], "soft_deps": [], "note": "Independent of P0474/P0475 — touches scheduler, not store. Can land in parallel."}
```

**Depends on:** P0473 (`ceaebfd6`, already merged) — `connect_store_with_retry` exists at `:733`.

**Conflicts with:** None on the rsb chain — this is scheduler-side, P0474/P0475 are store-side. Can dispatch in parallel.
