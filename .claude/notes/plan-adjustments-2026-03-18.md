# Plan Adjustments — 2026-03-18 Design Review

User-validated design decisions across UNIMPL plans P0205-P0244. Scan found ~10
embedded assumptions/defaults worth validating; these are the deltas.

## Decisions by plan

| Plan | Original | **Decision** | Scope |
|---|---|---|---|
| **P0209** FUSE breaker | `threshold=5, auto_close=30s` | **+ wall-clock trip**: open if `last_success.elapsed() > 90s`. Spec marker updated. | Adds 2nd trip condition |
| **P0213** rate limit | 10/min burst 30, enabled by default | **Disabled by default.** `gateway.toml rate_limit` section enables. | Spec marker `r[gw.rate.per-tenant]` text updated |
| **P0216** `--json` | Thin wrappers (default) | **Confirmed** — thin wrappers | — |
| **P0219** poison filter | Add `per_worker_failures: HashMap` | **SCOPE CHANGE — see §P0219 below** | Different implementation |
| **P0229** rebalancer | Hardcoded 100/7d/0.3 | **Config-driven**, default `min_samples=500` | `scheduler.toml` plumbing |
| **P0237** `rio-cli wps` | `rio-controller` dep | **New `rio-crds` crate** — see §rio-crds below | NEW plan needed |

## §P0219 — scope change (OVERRIDES original plan)

**Original P0219:** Add `per_worker_failures: HashMap<WorkerId, u32>` alongside
existing `failed_workers: HashSet`. Exclude workers at ≥2 failures.

**User reframe:**

1. **Split `InfrastructureFailure` out of the poison-counting path.** Currently
   `completion.rs:162` ORs it with `TransientFailure` → both call
   `handle_transient_failure` → both count. New handler
   `handle_infrastructure_failure`: `reset_to_ready` + retry WITHOUT inserting
   into `failed_workers`. Worker-local issue (FUSE EIO, cgroup setup fail, OOM)
   is not the build's fault.

2. **Keep counting disconnects** (worker.rs:119). Defensive — a build that
   segfaults the daemon 3× IS poisoned. Accepts false-positive poisons from
   unrelated worker deaths; admin clears via `rio-cli poison clear`.

3. **Two configurable knobs** via `scheduler.toml`:
   - `poison_threshold: u32` (default 3 — current `POISON_THRESHOLD`)
   - `require_distinct_workers: bool` (default true — current HashSet semantics)
   When `false`: `failed_workers` becomes a `Vec` (or a counter), any 3 failures
   poison regardless of worker. Useful for single-worker dev deployments.

**Files change:** `completion.rs:162` (split match arm), new handler,
`state/derivation.rs` (knobs), `scheduler.toml` config struct. The
`per_worker_failures` HashMap from the original plan is **NOT needed** — the
split handles the same concern more directly.

**Spec marker `r[sched.retry.per-worker-budget]` (seeded @ 5f3c8c40) needs
rewording** — it describes the HashMap approach. New text describes the
InfrastructureFailure split + configurable knobs.

## §rio-crds — NEW crate extraction

**Problem:** P0237 (`rio-cli wps`) imports `rio_controller::crds::workerpoolset`
→ pulls kube-rs + k8s-openapi (~500 transitive deps) into a CLI binary.

**Decision:** Extract `rio-controller/src/crds/` → NEW crate `rio-crds`.
`rio-controller` and `rio-cli` both dep on it. CRD types isolated; reconciler
deps stay in rio-controller.

**Plan slot:** This should be a NEW plan **before** P0232 (WPS CRD struct) —
P0232 then adds `WorkerPoolSet` to `rio-crds`, not `rio-controller::crds`.
Suggest: insert as P0232's T0 task, OR renumber (risky with 41 docs) OR use a
placeholder slot. **Simplest: fold into P0232 as T0 "extract rio-crds crate
FIRST, then add WorkerPoolSet to it."**

## §GRPC timeout — NEW concern from P0209 discussion

**Problem:** `GRPC_STREAM_TIMEOUT=300s` (rio-common/src/grpc.rs:21) has 7 call
sites. User wants config-driven per-path timeouts. Lowering uniformly to 60s
would break large-NAR uploads (1GB @ 100Mbps = 80s).

**Decision:** Config-driven per-path. Each component reads its relevant timeout
from config:
- `worker.toml fuse_fetch_timeout_secs` (default 60) — P0209's concern
- `worker.toml upload_stream_timeout_secs` (default 300) — large NARs OK
- `gateway.toml nar_passthrough_timeout_secs` (default 300)
- `store.toml get_path_timeout_secs` (default 300)

**Plan slot:** This is wider than P0209 (FUSE-only). Options:
- Fold into P0209 as T0 "config-driven timeouts" — P0209 becomes MED not LOW
- Separate trivial plan (just config plumbing, no logic change)
- **Suggest: P0209 T0 = just the `fuse_fetch_timeout_secs` config knob** (that's
  the one P0209 actually needs); remaining 6 call sites → `TODO(phase5)` or a
  batch tooling plan.

## Spec marker edits needed (already seeded @ 5f3c8c40)

| Marker | Current text | Needed edit |
|---|---|---|
| `r[gw.rate.per-tenant]` | "Default quota: 10/min with burst 30" | "Disabled by default. When enabled via `gateway.toml rate_limit`, quota is operator-configured." |
| `r[worker.fuse.circuit-breaker]` | Only mentions threshold | Add: "Also opens if `last_success.elapsed() > wall_clock_trip` (default 90s) — catches degraded-but-alive store without waiting for N×timeout." |
| `r[sched.retry.per-worker-budget]` | Describes `per_worker_failures: HashMap` | Rewrite: describe `InfrastructureFailure` split + configurable `poison_threshold` + `require_distinct_workers` |

## Unchanged (confirmed defaults)

- P0205, P0206, P0207, P0208 (spike-gated), P0211, P0212 (24h cron), P0214,
  P0215, P0217, P0218, P0220 (spike-gated), P0221-P0228, P0230-P0236,
  P0238-P0244
