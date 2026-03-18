# Audit Batch C Decisions — 2026-03-18 (single-plan)

Items 24-29 from `.claude/notes/plan-decision-audit.md`. Final batch.

| # | Plan | Issue | **Decision** | Delta |
|---|---|---|---|---|
| 24 | **P0252** | Only `(Queued, Skipped)` allowed — if cascade runs after `find_newly_ready()`, targets may be Ready already | **Also allow `(Ready, Skipped)`.** Matches `DependencyFailed` precedent (completion.rs:678-680). Order-independent. | State transition widened |
| 25 | **P0273** + **P0282** | `envoyproxy/envoy:v1.29-latest` + IfNotPresent = node skew; diverges from VM-test digest | **Envoy Gateway via nixhelm chart.** Architecture change: Gateway API CRDs replace sidecar. Chart version pinned by nixhelm flake input. | P0273 scope rewrite; P0282 becomes Gateway/HTTPRoute resources not sidecar container |
| 26 | — | (P0271 keyset cursor tiebreaker) | **Already fixed by Batch A #3** — cursor includes `build_id`, query uses compound `<` | — |
| 27 | **P0212** | `run_gc` signature missing `grace_hours` + `extra_roots` — hardcodes or won't compile | **`GcParams { dry_run, grace_hours, extra_roots }` struct.** Extensible. Cron passes `{ false, 2, vec![] }`. | Signature change |
| 28 | **P0238** | Second SSA manager `.force()`-writing `status.conditions` clobbers existing Failed/Cancelled | **Refactor ALL condition writes to new manager `"rio-controller-build-conditions"`.** Move Failed/Cancelled arms too. One manager for conditions, one for rest of status. Granular managedFields. | Bigger than "reuse MANAGER" but cleaner |
| 29 | **P0264** | `None => ""` grants full cross-tenant access on wiring bug | **Fail-closed: `None => Err(Status::unauthenticated)`.** Free assertion — unreachable if P0259 wiring correct. Loud failure on security boundary. | Match arm change |

## #25 — Envoy Gateway architecture

**Supersedes the sidecar decision at `ff9c16e4`.** Instead of envoy-as-container-in-dashboard-pod with a static `envoy-dashboard.yaml` bootstrap:

- Envoy Gateway operator + CRDs deployed via nixhelm chart (pinned by nixhelm flake rev)
- Dashboard exposes a `Gateway` + `HTTPRoute` (or `GRPCRoute` if supported for gRPC-Web)
- Envoy instances managed by the operator, not hand-wired in Pod spec
- gRPC-Web translation via Gateway API filter (need to verify: does Envoy Gateway expose `grpc_web` filter config through a `BackendTrafficPolicy` or similar?)

**Unchanged from ff9c16e4:**
- Scheduler is COMPLETELY untouched (no tonic-web, no third port)
- mTLS upstream to scheduler
- R5 (`.accept_http1`) still disappears — Envoy handles HTTP/1.1

**Open question for P0273 dispatch:** Envoy Gateway's gRPC-Web support status. It has `GRPCRoute` for gRPC; gRPC-Web may need an `EnvoyPatchPolicy` escape hatch. Verify at dispatch.

**VM test impact:** Envoy Gateway operator image + chart both need to be in docker-pulled.nix + nixhelm. P0273's `nix/docker-pulled.nix` edit grows.

## Audit complete — 29/29

| Batch | Items | Bugs found | Decisions |
|---|---|---|---|
| A (proto/migration) | 7 | 3 migrations would fail | P0261's premise killed at B1 |
| B1 (spec-markers) | 7 | 4 (seccomp regression, 3 won't-compile) | refcount=1 overrode xmax; P0261 → RESERVED |
| B2 (multi-plan) | 9 | 6 | InputsResolved proto event; both-proto+CRD |
| C (single-plan) | 6 | 4 | Envoy Gateway architecture change |
| **Total** | **29** | **17** | 1 plan eliminated, 1 arch changed |
