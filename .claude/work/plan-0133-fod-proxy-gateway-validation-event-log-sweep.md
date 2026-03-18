# Plan 0133: FOD egress proxy + gateway validation + event_log sweep

## Design

Three small, independent features packed into one commit during the initial phase-3b burst. Each was a one-file change in its respective crate; none justified a standalone commit.

**FOD egress proxy (D1/D3/D4):** Fixed-output derivations (fetchers) need internet access; regular builds are sandboxed. Phase 3a let FODs hit the internet directly from the worker pod — no egress control, no allowlist, no audit log. This plan routed FOD traffic through a Squid forward proxy.

`spawn_daemon_in_namespace` takes `fod_proxy: Option<&str>`. When Some, sets `http_proxy`/`https_proxy`/`HTTP_PROXY`/`HTTPS_PROXY` on the daemon Command (both cases for compat). The executor computes it from `is_fixed_output && env.fod_proxy_url` — non-FOD builds never see proxy env vars (the sandbox blocks network anyway; this just reduces confusion in `/proc/PID/environ` during debug). `Config.fod_proxy_url` → `RIO_FOD_PROXY_URL` env var, unset = direct internet.

`deploy/base/fod-proxy.yaml`: ConfigMap with squid.conf allowlist (`nixos.org`, `github`, `gitlab`, `crates.io`, `pypi.org`, `npmjs`, `rubygems`, `golang`, `debian`, `kernel.org`, `sourceforge`, `gnu.org`), `http_access deny all` fallthrough, `cache deny all` (this is access control not caching). Deployment runs as `uid=31` (squid), tmpfs spool, TCP probe on 3128.

NetworkPolicy (prod overlay): `rio-worker-egress` gets exception for `rio-fod-proxy:3128`. New `rio-fod-proxy-egress`: DNS + `0.0.0.0/0` EXCEPT RFC1918 + link-local + loopback on 80/443 (blocks metadata bypass via proxy). Squid does domain allowlisting; NetworkPolicy just opens the internet for the proxy itself.

**Gateway validation (G1/G2):** `translate::validate_dag` rejects two things before `SubmitBuild`: (a) any derivation with `__noChroot=1` in env — sandbox escape, allowed in single-user Nix for bootstrap, never in multi-tenant; (b) `nodes.len() > MAX_DAG_NODES` — scheduler already enforces, this saves the gRPC round-trip. Wired into `handle_build_derivation` before `filter_and_inline_drv` (no point inlining a DAG we're rejecting). Caller catches `Err(reason)` → `STDERR_ERROR` + `BuildResult::failure`.

**event_log sweep (H1):** `build_event_log` table grew unboundedly. `handle_tick` now fires `DELETE WHERE created_at < now() - 24h` every 360 ticks (~1h at 10s interval), fire-and-forget via `spawn`. Safety net for the terminal-cleanup delete that already existed.

## Files

```json files
[
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "fod_proxy param + http_proxy env injection"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "compute fod_proxy from is_fixed_output && env.fod_proxy_url"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "fod_proxy_url field"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "plumb config"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "ExecutorEnv.fod_proxy_url"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "validate_dag: __noChroot + MAX_DAG_NODES checks"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "call validate_dag before filter_and_inline_drv"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "handle_tick % 360 \u2192 event_log DELETE"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "remove resolved TODO"},
  {"path": "infra/base/fod-proxy.yaml", "action": "NEW", "note": "Squid Deployment + allowlist ConfigMap + Service :3128"},
  {"path": "infra/overlays/prod/kustomization.yaml", "action": "MODIFY", "note": "add fod-proxy resource"},
  {"path": "infra/overlays/prod/networkpolicy.yaml", "action": "MODIFY", "note": "worker\u2192proxy exception + proxy egress"}
]
```

## Tracey

No tracey markers landed in this commit. FOD proxy, `__noChroot` rejection, and event_log sweep are not spec-rule-tracked — they're operational/security features with no normative `r[...]` requirements in `docs/src/`.

## Entry

- Depends on P0127: phase 3a complete (`spawn_daemon_in_namespace`, `translate.rs`, `handle_tick`).
- Depends on P0132: NetworkPolicy base exists in prod overlay (FOD policy extends it).

## Exit

Merged as `865f6db` (1 commit). `.#ci` green at merge. Tests: `validate_dag_rejects_oversized` (100k+1 nodes → Err), `validate_dag_accepts_normal_size_no_nochroot`. `__noChroot` end-to-end via vm-phase3b G1 section. FOD proxy env injection: spawn mock command with `is_fod=true` + proxy set → assert env vars present; `is_fod=false` → not present.
