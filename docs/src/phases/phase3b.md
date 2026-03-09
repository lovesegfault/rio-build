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

**vm-phase3b:** iteration 3 — 9 test sections (T1-T3 mTLS, B1 HMAC, S1 state recovery, F1 Build CRD watch dedup, C1 GC dry-run, G1 `__noChroot`, C2 real GC sweep + PinPath). 4 VMs (added k8s node via `common.mkK3sNode`). F1 proved the Build reconciler watch dedup fix: `rio_controller_build_watch_spawns_total == 1` across multiple status-patch → reconcile cycles. C2 exercised the non-dry-run GC commit path + PinPath/UnpinPath round-trip.

Iteration 2 had caught a **latent TLS SNI bug**: `load_client_tls` set `domain_name` globally via OnceLock, but gateway/worker connect to BOTH scheduler AND store — fixed domain_name would mismatch one of the two server certs. Fix: remove `domain_name` override; tonic derives SNI from URL host per-connection.

**Validation-round bugs fixed (iteration 3):** Deep code review of Phase 3b code found 4 CRITICAL + 5 HIGH issues in areas NOT covered by iteration 2:
- **C1** Build reconciler spawned duplicate watch tasks on every reconcile (each status patch triggered a re-reconcile → new `spawn_reconnect_watch`). Fixed via `Ctx.watching` DashMap dedup gate.
- **C2** GC sweep-vs-PutPath TOCTOU for shared chunks: sweep's `FOR UPDATE OF manifests` locks manifest row, not chunk rows. PutPath for a different path sharing chunk X could run after sweep enqueued X → chunk gets S3-deleted while still referenced. Fixed via `pending_s3_deletes.blake3_hash` + drain re-check + upsert clears `deleted=false`.
- **C3** Orphan scanner's DELETE had no status guard — upload completing between SELECT and DELETE got its valid narinfo reaped. Fixed via `WHERE EXISTS (... status='uploading')`.
- **C4** Orphan-completion in recovery didn't call `check_build_completion` → if the orphan-completed drv was the LAST outstanding one, build stayed Active forever.
- **H1-H5:** backstop timeout didn't persist `failed_worker` to PG; drain lacked `SKIP LOCKED` for multi-replica; gateway reconnect counter never reset on success; backstop block duplicated `reassign_derivations`; GC sweep/orphan had ~50 lines duplicated.
- **M1-M5:** HMAC key CRLF not trimmed; `GcRoots` no dedup; Assigned-with-NULL-worker silently skipped; PinPath FK check via string match; backstop NaN never fires.

**Validation-round-2 bugs fixed (iteration 4, 23 commits 54c5baa..c57158d, 976→991 tests):** Fresh deep review found 5 CRITICAL + 9 HIGH + 7 MEDIUM bugs + 2 hollow VM test sections (S1 never ran recovery; C2 never committed).
- **X1** (CRIT) mTLS bypass defeated HMAC: `has_peer_certs + no token → bypass`. Compromised worker omits token → uploads arbitrary paths. Fixed via x509-parser CN check: only `CN=rio-gateway` bypasses.
- **X2** (CRIT) Build CRD stuck at `build_id="submitted"` — no resubmit path. Fixed: apply() detects orphaned sentinel (no watch running) → falls through to resubmit.
- **X3** (CRIT) Orphan scanner stale `chunk_list` from outer SELECT → multi-replica race decrements wrong chunks. Fixed: re-read `chunk_list` INSIDE tx with `FOR UPDATE OF m` (mirrors sweep.rs pattern).
- **X4** (CRIT) Transient-failure retry wrote Failed to PG, in-mem went Ready → crash → recovery loads Failed but never enqueues → hang. Fixed: write Ready to PG matching final in-mem state.
- **X5** (CRIT) Recovery missed all-complete builds (crash between last-drv-Completed and build-Succeeded). Fixed: post-recovery `check_build_completion` sweep for all builds.
- **X6** (HIGH) `reassign_derivations` had no POISON_THRESHOLD check → 3 disconnects leave Ready-but-undispatchable. Fixed via `poison_and_cascade` helper.
- **X7** (HIGH) FastCDC duplicate chunks crash PG ON CONFLICT same-row-twice. Fixed: dedup before UNNEST.
- **X8** (HIGH) Sweep didn't delete realisations (no FK to narinfo) → dangling `wopQueryRealisation` → 404. Fixed: explicit DELETE before narinfo CASCADE.
- **X9** (HIGH) Auto-pin live-build INPUTS via `scheduler_live_pins` table + CTE seed (e). Scheduler pins at dispatch, unpins at terminal.
- **X10-X21**: gateway reconnect-reset-on-event (not Ok()), controller build_id/phase/scopeguard/cleanup fixes, poison-TTL PG clear, HMAC expiry clamp, backoff infinity clamp, s3_deletes_stuck gauge, pg_try_advisory_lock for TriggerGC, dispatch_ready after LeaderAcquired.
- **V1** (VM test hollow): S1 never ran recovery (`always_leader()` from boot). Fixed: wire scheduler to k3s Lease. kubeconfig copy before `waitForControlPlane` → scheduler standby → acquire → `LeaderAcquired` → recovery ACTUALLY runs. Asserts journalctl log + metric.
- **V2** (VM test hollow): C2 never committed (all in grace → `unreachable=vec[]` → for-batch body never runs). Fixed: backdate S1's output past grace → sweep DELETES 1 path → proves commit.

**Validation-round-3 bugs fixed (iteration 5, 9 commits fbadbfc..7d98a3c, 991→994 tests):** Deep review of round-2 code found 1 HIGH + 2 MEDIUM bugs + 2 more hollow VM sections + ~200 lines of needless duplication.
- **Y1** (HIGH) Controller watch-dedup delete+recreate race: `watching` keyed by `{ns}/{name}` → old drain_stream's scopeguard removes NEW entry after delete+recreate → duplicate watch (C1 regression). Fixed: key by `b.uid()` (unique per K8s object).
- **Y2** (MED) Pin leak on orphan-completion: `handle_reconcile_assignments` orphan-complete branch never called `unpin_live_inputs` → pins leak until next restart. Fixed + test seeds pins before reconcile.
- **Y3** (MED) Orphan scanner reap-then-reupload race: X3's `FOR UPDATE` re-checked `status='uploading'` but NOT the stale threshold → fresh re-upload with same hash gets reaped. Fixed: `AND updated_at < threshold` in both FOR UPDATE + DELETE EXISTS.
- **Y5** (LOW, latent) `poison_and_cascade` unconditional PG write on transition failure → in-mem ≠ PG + spurious cascade. Unreachable today (3 callers guarantee precondition); fixed via debug_assert! + early-return.
- **Y4** (LOW, comment) Advisory-lock-on-panic comment wrong: `PoolConnection::drop` returns to pool (not closes) → lock leaks until sqlx recycles. No panic points currently — corrected comment, noted to avoid `.unwrap()`.
- **D1-D5** (duplication): Extracted `persist_status` (13 sites), `record_failure_and_check_poison` (3 sites), `unpin_best_effort` (5 sites), `ensure_running` (4 sites), `TERMINAL_STATUSES` const (2 SQL queries). ~200 lines net reduction in actor/*.
- **V3** (VM test hollow): S1 recovered 0 rows (B1+S1-setup terminal before restart). Fixed: background 15s-sleep build before restart → PG has non-terminal row → recovery loads real data. PG query asserts ≥1 non-terminal.
- **V4** (VM test hollow): B1 didn't prove HMAC verifier active (broken `load` → verifier=None → PutPath accepts all → B1 passes anyway). Fixed: assert `rio_store_hmac_bypass_total{cn="rio-gateway"}` ≥ 1 after seedBusybox — metric only increments when verifier non-None.

**EKS smoke:** manual trigger (`infra/eks/smoke-test.sh`). Deploys, builds hello, kills worker, verifies reassign via metrics delta.
