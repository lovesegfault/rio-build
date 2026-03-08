# Phase 3b: Production Hardening + Networking (Months 15-17)

**Goal:** Security hardening, network isolation, retry improvements, GC, and FOD support.

**Implements:** [Security](../security.md) (mTLS, HMAC, network policies, RBAC), [Error Taxonomy](../errors.md) (K8s-aware retry), [Store GC](../components/store.md#garbage-collection)

## Tasks

- [x] **mTLS between all rio components** — tonic `tls-aws-lc` feature, `TlsConfig` in each binary's config, cert-manager manifests (CA + per-component Certificates), SecretName CRD field for worker pods, plaintext health port split (shared HealthReporter so standby correctly shows NOT_SERVING)
- [x] **HMAC assignment tokens** — scheduler signs Claims{worker_id, drv_hash, expected_outputs, expiry} at dispatch; store verifies `x-rio-assignment-token` metadata on PutPath; mTLS bypass for gateway (only mTLS client without tokens)
- [x] **State recovery on LeaderAcquired** — migration 004 adds columns (expected_output_paths, output_names, is_fixed_output, failed_workers, keep_going, options_json); `recover_from_pg` rebuilds DAG + builds + ready queue; lease loop fire-and-forgets LeaderAcquired (NON-BLOCKING — renewal continues during recovery); recovery_complete flag gates dispatch; generation seeded via fetch_max from PG high-water mark
- [x] **K8s-aware retry + cancel** — Cancelled proto enum + DerivationStatus; cgroup.kill cancel via registry; CancelSignal on CancelBuild for sole-interest running drvs; failed_workers exclusion in best_worker; backoff_until deferred re-queue; backstop timeout (est×3 vs daemon_timeout+slack floor); DrainWorker(force) sends CancelSignal (preemption hook)
- [x] **RBAC, NetworkPolicy, PodDisruptionBudget** — per-WorkerPool PDB (controller-managed, maxUnavailable=1); scheduler/store ingress NetworkPolicies; Events Recorder in controller; PDB RBAC
- [x] **Pod scheduling** — topologySpreadConstraints + soft podAntiAffinity (topology_spread CRD field, default true); scheduler required anti-affinity (replicas=2 must spread); seccompProfile RuntimeDefault; dnsPolicy ClusterFirstWithHostNet
- [x] **EKS-specific** — EKS overlay (IRSA annotation patch + NLB target-type=ip); terraform module (VPC + EKS + nodegroups + IRSA); IMDSv2 hop-limit=1 in launch template metadata_options; smoke-test.sh (deploy, build hello, kill worker, verify reassign)
- [x] **Basic GC** — migration 005 (pending_s3_deletes + gc_roots); mark (recursive CTE over references from root seeds: gc_roots + uploading + grace period + scheduler extra_roots); sweep (DELETE CASCADE + chunk refcount decrement + enqueue S3 keys, SELECT FOR UPDATE TOCTOU guard, dry_run rollback); drain task (S3 DeleteObject, attempts/last_error, max=10); orphan scanner (stale uploading manifests >2h); ChunkBackend delete/key_for/delete_by_key; GcRoots actor command; **StoreAdminService.TriggerGC** (store-side RPC) + scheduler `AdminService.TriggerGC` proxy (collects extra_roots via GcRoots, proxies to store, forwards GCProgress stream); PinPath/UnpinPath RPCs; orphan scanner + drain task spawned in store main.rs
- [x] **FOD network egress proxy** — Squid Deployment + allowlist ConfigMap (nixos.org, github, gitlab, crates.io, pypi.org, etc); NetworkPolicy worker→proxy:3128 + proxy egress 0.0.0.0/0 EXCEPT RFC1918+link-local+loopback on 80/443; fod_proxy_url CRD field → RIO_FOD_PROXY_URL env → daemon spawn injects http_proxy env ONLY when is_fixed_output
- [x] **Gateway validation** — `translate::validate_dag` rejects `__noChroot=1` (sandbox escape) and `nodes.len() > MAX_DAG_NODES` (early, before SubmitBuild)
- [x] **nix.conf drift fix** — WORKER_NIX_CONF had `experimental-features=` (empty) but ConfigMap had `ca-derivations`; ConfigMap was never mounted. Fixed both: code has `ca-derivations`, ConfigMap mounted at `/etc/rio/nix.conf` as override (setup_nix_conf checks there first)
- [x] **Integration test** — `infra/eks/smoke-test.sh`: deploy, build hello, kill worker mid-build, verify reassign via rio_scheduler_worker_disconnects_total delta. NOT in ci-fast/slow (manual trigger, real AWS resources)

### Carried-forward TODOs (resolved or retagged)

- [x] **Build reconciler WatchBuild reconnect** — `BuildStatus.last_sequence` field; apply() idempotence gate reconnects when build_id non-empty + phase non-terminal; drain_stream backoff-retry on stream error (5 attempts, 1s/2s/4s/8s/16s); phase=Unknown on exhaustion
- [x] **`build_event_log` time-based sweep** — `handle_tick` runs `DELETE WHERE created_at < now() - 24h` every 360 ticks (~1h at 10s interval), fire-and-forget
- [x] **Delayed re-queue using computed backoff** — `DerivationState.backoff_until` + dispatch_ready defer check
- [x] **Worker cancel via `cgroup.kill`** — BuildSpawnContext.cancel_registry (drv_path → (cgroup_path, AtomicBool cancelled)); try_cancel_build writes cgroup.kill + sets flag; spawn_build_task checks flag in Err arm → Cancelled vs InfrastructureFailure
- [x] **Fuzz corpus persistence** — nightly targets get `s3CorpusSync=true`: download from `$RIO_FUZZ_CORPUS_S3/<target>/` before run, upload after. Best-effort `|| true` on sync failure. Smoke (30s) skips — too short, PR CI may lack AWS creds
- [→phase4] **`ClusterStatus.store_size_bytes`** — slow-refresh background task, low priority (dashboard can query store /metrics directly)
- [→phase4] **`PutChunk` RPC** — stubbed UNIMPLEMENTED; only needed if client-side chunking lands

### Deferred to Phase 4
- [ ] Custom seccomp profile — RuntimeDefault is set (Phase 3b); a rio-specific profile blocking ptrace/bpf etc under CAP_SYS_ADMIN is Phase 4
- [ ] FUSE read timeout + circuit breaker — FUSE kernel-side timeout is 60s default; a userspace-side timeout + breaker (fail fast when store is down) is Phase 4
- [ ] BuildStatus fine-grained conditions — current Scheduled/InputsResolved/Building/Succeeded/Failed; more granular (PerDrvProgress, Blocked, Throttled) is Phase 4

## Milestone

**PASS (Rust unit/integration coverage):** mTLS handshake (4 tests), HMAC sign/verify/tamper (10 tests), cancel registry (3 tests), failed_workers exclusion + retry backoff (updated poison tests), state recovery (2 tests), GC mark (5 tests), `__noChroot` validation (2 tests). 961 total tests, 126/126 tracey rules covered.

**vm-phase3b:** iteration 1 covers G1 (`__noChroot` rejection) end-to-end. Iteration 2 (T/B/A/S/C/F/E/D VM sections) pending NixOS module TLS env-var extension + k8s worker pod TLS mount — Rust paths fully unit-tested.

**EKS smoke:** manual trigger (`infra/eks/smoke-test.sh`). Deploys, builds hello, kills worker, verifies reassign via metrics delta.
