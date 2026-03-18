# Phase 4c Partition — FINAL (P0220–P0244, 25 plans)

---

## §0 — Assumptions & Open Questions (confirm before dispatch)

### Assumptions made by this partition

| # | Assumption | Basis | Falsification signal |
|---|---|---|---|
| **A1** | 4b migration is **012** (`012_path_tenants.sql`) → 4c `build_samples` is **013** | `plan-0206` header says "migration 012_path_tenants.sql"; `migrations/` ends at 011 | `ls migrations/` at P0227 dispatch shows ≠012 last → renumber |
| **A2** | NetPol pre-verify outcome defaults to **kube-router ENFORCES** (stock k3s) | k3s ships kube-router for NetPol by default; `--disable-network-policy` flag exists to turn it OFF (implies default ON) | P0220 interactive check shows curl succeeds → re-plan P0241 with Calico preload |
| **A3** | `BuildStatus` conditions (Scheduled/InputsResolved/Building) ship; `criticalPathRemaining` + `workers` fields **DEFER to Phase 5** | phase4c.md:60 explicitly permits: "include if straightforward, else defer the fields (not the conditions)" | If BuildEvent already carries worker IDs, fields are trivial — revisit in P0238 plan doc |
| **A4** | `admin/builds.rs:22` cursor pagination **DEFERS to Phase 5** | `TODO(phase4c)` in code but NOT in phase4c.md task list; needs proto field + keyset query | User wants it in 4c → bundle with P0231 (only admin.proto touch) |
| **A5** | `cgroup.rs:301` resolves as **doc-comment improvement only** — no unit test of unreachable code | Code already says `TODO(phase5)`; phase4c.md:63 says "low priority"; path unreachable under `privileged:true` | User wants the #[ignore] test stub → 30min addition to P0244 |
| **A6** | RwLock for SizeClassConfig uses **`parking_lot::RwLock`** (sync, no `.await`) | `parking_lot` already in `Cargo.lock`; keeps `classify()` sync; writes hourly → near-zero contention | If actor loop already awaits elsewhere, tokio::sync::RwLock is fine |
| **A7** | `cas.rs:564` scopeguard resolves as **assess-then-document** — may be a no-op | Existing comment (:558-565) says leak is SELF-HEALING (~100 bytes, cleaned on next get); "scopeguard here would need careful Drop ordering with the Shared clone" | If sustained cancellation IS observable in production, implement proper guard |
| **A8** | `rio-cli cutoffs` + `wps` go in **NEW files** (`src/cutoffs.rs`, `src/wps.rs`), two match arms in main.rs | Minimizes conflict with 4b P0216 | — |
| **A9** | VM test `default.nix` registrations are **SOFT-conflict**, not hard-deps | Both P0241/P0243 add 1 import + 1 mkTest; independent lines; coordinator serializes dispatch, not dag | — |

### Open questions for user confirmation

| # | Question | Default if unanswered |
|---|---|---|
| **Q1** | Seccomp Localhost profile on k3s VM nodes: does `/var/lib/kubelet/seccomp/` exist, or does P0223 need a hostPath init-container to install the profile? | Document as "production EKS/GKE only; VM test uses RuntimeDefault" — no init-container |
| **Q2** | Grafana dashboards: ship as Helm chart, ConfigMap, or raw JSONs under `infra/helm/grafana/`? | Raw JSONs + minimal ConfigMap wrapper; milestone says "render" not "auto-deploy" |
| **Q3** | `rio_scheduler_class_drift_total` OR rename existing `misclassifications_total`? Existing counter has different semantics (penalty-trigger = 2× cutoff), phase doc wants class-drift (classify(actual) ≠ assigned) | **Sibling metric** `rio_scheduler_class_drift_total` — decouples alerting from penalty logic; update observability.md to document both |
| **Q4** | Does FOD-proxy test need a `kubectl logs squid | grep TCP_DENIED` assertion, or is build-failure sufficient signal? | Both — build-failure proves blocking, log-grep proves it was Squid that blocked (not network misconfig) |

---

## §1 — Ground-Truth Reconciliation (verified at `6b5d4f4`)

| # | Claim in phase4c.md | Reality on disk | Resolution |
|---|---|---|---|
| GT1 | L19: `rio_scheduler_misclassification_total` counter — new | **EXISTS with DIFFERENT semantics** at `completion.rs:336` (`lib.rs:147` registers). Existing: "actual > 2× cutoff" (penalty trigger). Phase doc wants: "classify(actual) ≠ assigned" (cutoff drift) | **P0228 adds sibling `rio_scheduler_class_drift_total`** — decoupled from penalty. Update `observability.md` to clarify both. **NOT a drop.** |
| GT2 | L64: `__noChroot` gateway pre-check | **FULLY IMPLEMENTED** — `handler/build.rs:522-540,578,711,856` + `translate.rs:280-333` | P0226 = tracey-annotation-only: `r[gw.reject.nochroot]` spec ¶ + `r[impl]` comments on existing code + `r[verify]` golden test |
| GT3 | L17: `ema_peak_cpu_cores` for CPU-bump | **COLUMN EXISTS** — queried at `db.rs:931`, EMA-updated at `db.rs:1128,1141` | P0230 is `assignment.rs` + struct field ONLY. No migration, no db.rs change. |
| GT4 | L48: "New `dockerImages.squid` ~25 lines" | **EXISTS** — `nix/docker.nix:192-224` (`fod-proxy` image with squid+cacert) | P0243 reuses. Scope shrinks ~25 lines. |
| GT5 | L38,44: helm templates | **EXIST** — `infra/helm/rio-build/templates/{networkpolicy.yaml,fod-proxy.yaml}` | P0241/P0243 scenario-only; no helm template work |
| GT6 | L13: "Migration 009 Part D" | `migrations/` ends at 011; P0206 creates 012; `sqlx::migrate!` checksums | **NEW file `013_build_samples.sql`** (or next-free at dispatch). Delete stale 009:8 comment. |
| GT7 | L67: `errors.md:75` poison-PG deferral | grep finds **no `Phase 4` at :75** — already updated | DROP from P0244 sweep |
| GT8 | L68: `errors.md:97` per-build timeout | **STILL says** "Not implemented (Phase 4)" at :97 | Keep in P0244; 4b P0214 implements timeout — P0244 verifies merge then removes |
| GT9 | L71: `observability.md:116,166,269` | **Actual**: :117, :185, :303 | P0244 greps `'(Phase 4+)'` not line numbers |
| GT10 | L63: `cgroup.rs:301` TODO | **Already `TODO(phase5)`** not phase4c | P0244: improve doc comment → "unreachable under privileged:true; covered by ADR-012 device-plugin VM test when that lands" |
| GT11 | L62: `cas.rs:564` scopeguard | Comment :558-565 says **SELF-HEALING**; warns "careful Drop ordering with Shared clone" | P0225: assess, document conclusion. Likely: improve comment + delete TODO tag. Only add guard if a test can demonstrate observable leak. |
| GT12 | L84: `sched.rebalancer.cpu-bump` marker | Existing family: `sched.classify.{smallest-covering,mem-bump,penalty-overwrite}` at scheduler.md:157-171 | **RENAME → `sched.classify.cpu-bump`** for family consistency |
| GT13 | — (not in phase doc) | `cache_server/mod.rs:77` `TODO(phase4c)`: /nix-cache-info behind auth | Bundle into P0225 (rio-store small deferrals) |
| GT14 | — | `admin/builds.rs:22` `TODO(phase4c)` cursor pagination | **DEFER Phase 5** — not in task list; retag in P0244 |
| GT15 | — | New dirs `infra/helm/grafana/`, `infra/helm/rio-build/files/`, `rio-bench/` do NOT exist | P0221/P0222/P0223 create them — zero conflict |
| GT16 | — | 4b plan docs **EXIST on disk** at `.claude/work/plan-02{04..19}-*.md` | All 4b-symbolic deps **resolved to concrete plan numbers** below |

---

## §2 — Hot-File Serialization Matrix (4b deps RESOLVED)

Collision counts from `.claude/collisions.jsonl`. 4b touchers verified via grep of `.claude/work/plan-02{04..19}-*.md`.

| File | Count | 4b touchers | 4c touchers | Serialization |
|---|---|---|---|---|
| `rio-scheduler/src/actor/mod.rs` | **31** (#2) | — | P0230, P0231 | P0231 deps P0230 (already serial via spine) |
| `rio-scheduler/src/db.rs` | **29** | P0206 | P0227, P0228, P0229 | Already serial via spine deps (227→228→229) |
| `rio-proto/proto/types.proto` | **27** | P0205, P0214 | P0231 | P0231 deps [P0205, P0214]; EOF-append messages only |
| `rio-scheduler/src/main.rs` | **26** | — | P0227 | Single 4c touch (retention task spawn, ~5 lines) |
| `flake.nix` | **23** | — | P0221 | Single 4c touch (`.#bench` alias, TAIL-append) |
| `rio-scheduler/src/actor/completion.rs` | **20** | **P0206, P0219** | P0228 | P0228 deps [P0206, P0219] — HARD serial |
| `rio-gateway/src/handler/build.rs` | **19** | — | P0226 (comment only) | Comment-only touch; safe parallel |
| `rio-controller/src/main.rs` | **17** | **P0212** | P0235 | P0235 deps P0212 — HARD serial |
| `rio-scheduler/src/admin/mod.rs` | **14** | — | P0231 | Single 4c touch |
| `rio-scheduler/src/actor/command.rs` | **9** | — | P0231 | Single 4c touch |
| `rio-controller/src/scaling.rs` | **8** | — | P0234 | Single 4c touch (hook at :195) |
| `rio-scheduler/src/assignment.rs` | **7** | — | P0230 | Single 4c touch (bundles RwLock-read + CPU-bump) |
| `nix/tests/scenarios/lifecycle.nix` | **6** | **P0206, P0207** | P0239 | P0239 deps [P0206, P0207]; different fragment keys |
| `rio-cli/src/main.rs` | 2 | **P0216** | P0236, P0237 | P0236 deps P0216; P0237 deps P0236 |
| `rio-proto/proto/admin.proto` | 2 | — | P0231 | Single 4c touch |
| `rio-controller/src/crds/workerpool.rs` | — | — | P0223 | P0232 deps P0223 (PoolTemplate inherits seccomp field) |
| `nix/tests/default.nix` | — | — | P0241, P0243 | **SOFT conflict** — coordinator dispatches one at a time, no dag dep |
| CRD regen (`infra/helm/crds/*.yaml`) | — | — | P0223, P0235 | Whoever merges second re-runs regen. Note in both plans. |

**Typed-edge legend** (used in deps column below):
- `table` — SQL table must exist (sqlx compile-check)
- `type` — Rust type must exist
- `proto` — generated proto types needed
- `module` — module must exist to wire/extend
- `file` — hot-file serialization only (no semantic dep)
- `decision` — design branches on outcome
- `install` — CRD/RBAC must be live in cluster

---

## §3 — Partition Table (25 plans, P0220–P0244)

### Wave 0 — Decision gate (1 plan, dispatch FIRST)

#### P0220 — NetPol pre-verify spike (DECISION GATE)
| | |
|---|---|
| **Tasks** | phase4c.md:41 — verify kube-router NetPol enforcement on stock k3s. Interactive: apply deny-all egress NetPol, `kubectl exec worker curl 1.1.1.1`. Outcome A (blocks) → P0241 stock k3s. Outcome B (passes) → `/plan` promotes Calico-preload plan before P0241. |
| **Primary files** | NONE (spike). Write outcome to `.claude/state/followups-pending.jsonl` as `Followup{origin:"P0220", payload:{"netpol":"kube-router-ok"\|"needs-calico"}}` + update phase4c.md:41 with result |
| **Deps** | — |
| **Tracey** | — |
| **Complexity** | LOW (~30min interactive) |
| **Exit** | Followup row written. NO `.#ci` needed (spike-only). |
| **Command** | `nix-build-remote --dev --no-nom -- -L .#checks.x86_64-linux.vm-lifecycle-core-k3s.driverInteractive` |

---

### Wave 1 — Immediate frontier (10 plans, no intra-4c deps)

#### P0221 — rio-bench crate + Hydra comparison doc
| | |
|---|---|
| **Tasks** | phase4c.md:54-55 — criterion benches: SubmitBuild latency N∈{1,10,100,1000}, dispatch throughput via mocked worker channel. DAG generators: `linear(n)`, `binary_tree(depth)`, `shared_diamond(n, frac)`. `nix run .#bench` alias. Hydra procedure doc. |
| **Primary files** | NEW `rio-bench/{Cargo.toml,src/lib.rs,benches/submit_build.rs,benches/dispatch.rs}`; MODIFY `Cargo.toml` (workspace member + criterion dev-dep); MODIFY `flake.nix` (package + `apps.bench`); MODIFY `docs/src/capacity-planning.md` (Hydra section) |
| **Deps** | — |
| **Tracey** | — |
| **Serialization** | `Cargo.toml` + `flake.nix` TAIL-append; verify no 4b plan adds a crate (none found) |
| **Risk** | criterion needs `harness = false` in `[[bench]]`; verify edition 2024 compat. Milestone p99<50ms@N=100 is a TARGET not a gate — bench infra is deliverable. |
| **Exit** | `/nbr .#ci` green. `nix develop -c cargo bench -p rio-bench -- --test` smoke passes. |

#### P0222 — Grafana dashboard JSONs
| | |
|---|---|
| **Tasks** | phase4c.md:53 — 4 dashboards: Build Overview (active builds, completion rate, p50/p99), Worker Utilization (per-class replicas, CPU/mem, queue depth), Store Health (cache hit, GC sweep, S3 latency), Scheduler (dispatch latency, ready-queue, backstop timeouts, cancel signals). Metric names from observability.md — DO NOT invent. |
| **Primary files** | NEW `infra/helm/grafana/{build-overview,worker-utilization,store-health,scheduler}.json` + ConfigMap wrapper |
| **Deps** | — |
| **Tracey** | — |
| **Serialization** | NEW directory, zero conflict |
| **Risk** | `rio_scheduler_class_load_fraction` won't exist until P0229 — panel shows "no data", which is FINE (milestone = "render" not "populated"). Verify each PromQL query against `lib.rs` metric registrations. |
| **Exit** | `/nbr .#ci` green. `jq .` validates each JSON. |

#### P0223 — seccomp Localhost profile + WorkerPoolSpec field
| | |
|---|---|
| **Tasks** | phase4c.md:59 — `seccomp-rio-worker.json` Localhost profile denying `ptrace,bpf,setns,process_vm_readv,process_vm_writev`; `seccomp_profile: Option<SeccompProfileKind>` in WorkerPoolSpec (default RuntimeDefault); builders.rs conditionalize. Close `security.md:53,173,175` deferrals. |
| **Primary files** | NEW `infra/helm/rio-build/files/seccomp-rio-worker.json` (creates dir); MODIFY `rio-controller/src/crds/workerpool.rs` (~:180 seccomp comment); MODIFY `rio-controller/src/reconcilers/workerpool/builders.rs` (~:259); MODIFY `docs/src/security.md` (spec ¶ + close 3 deferrals) |
| **Deps** | — |
| **Tracey** | `r[worker.seccomp.localhost-profile]` in security.md + `r[impl]` at builders.rs + `r[verify]` unit test (profile JSON parses + builder emits correct securityContext) |
| **Serialization** | `crds/workerpool.rs` — P0232 deps this so PoolTemplate inherits seccomp field. SOFT conflict with P0235 on CRD regen yaml — whoever merges second re-runs regen. |
| **Risk** | Localhost path is NODE path not container path. k3s VM nodes may not have `/var/lib/kubelet/seccomp/`. **MITIGATION per Q1:** document as production-only; VM test uses RuntimeDefault. If user wants VM verify, add init-container or hostPath ConfigMap mount (+~20 lines). |
| **Exit** | `/nbr .#ci` green. `tracey query rule worker.seccomp.localhost-profile` shows impl+verify. |

#### P0224 — CN-list config + SAN check for HMAC bypass
| | |
|---|---|
| **Tasks** | phase4c.md:61 — `hmac_bypass_cns: Vec<String>` in store config (default `["rio-gateway"]`); SAN check via `x509-parser::TbsCertificate::subject_alternative_name()` → match `GeneralName::DNSName` against allowlist. Bypass if CN OR any SAN DNS in list. |
| **Primary files** | MODIFY `rio-store/src/grpc/put_path.rs` (:65-127 CN block + SAN block); MODIFY `rio-store/src/config.rs`; MODIFY `docs/src/components/store.md` (spec ¶) |
| **Deps** | — |
| **Tracey** | `r[store.hmac.san-bypass]` in store.md + `r[impl]` at put_path.rs + `r[verify]` unit test (extend `make_cert_with_cn` helper → `make_cert_with_san`; test SAN-only cert with no CN bypasses) |
| **Serialization** | None (put_path.rs not hot) |
| **Risk** | `subject_alternative_name()` returns `ParsedExtension` — careful match on `GeneralName::DNSName` vs IA5String encoding. Unit test MUST cover SAN-only cert (CN empty). |
| **Exit** | `/nbr .#ci` green. `tracey query rule store.hmac.san-bypass` shows impl+verify. |

#### P0225 — rio-store small deferrals (scopeguard assess + /nix-cache-info auth)
| | |
|---|---|
| **Tasks** | (a) phase4c.md:62 + GT11 — `cas.rs:564`: ASSESS scopeguard viability. Existing comment says leak is SELF-HEALING (~100B, next-get cleans). "Careful Drop ordering with Shared clone" warning. **Resolution:** either implement if trivially correct, OR improve the comment (quantify leak bound, add test showing self-heal) and delete TODO tag. (b) GT13 — `cache_server/mod.rs:77`: move `/nix-cache-info` route before auth middleware OR use nested router merge. |
| **Primary files** | MODIFY `rio-store/src/cas.rs` (:540-566); MODIFY `rio-store/src/cache_server/mod.rs` (:77-82) |
| **Deps** | — |
| **Tracey** | — |
| **Serialization** | None |
| **Risk** | **Do NOT blindly `scopeguard::defer!` per A7.** The :563 warning is real: `Shared` clone is held while scope runs; naive defer changes when the last clone drops. Safe path: add a unit test that aborts an awaiting task, assert inflight map eventually empty (proves self-heal), improve comment, delete TODO. |
| **Exit** | `/nbr .#ci` green. Unauth `GET /nix-cache-info` → 200 (test at :527 adjusted). |

#### P0226 — Verification gaps: cross-tenant isolation + __noChroot tracey
| | |
|---|---|
| **Tasks** | phase4c.md:64-65 — (a) cross-tenant ListBuilds test: seed 2 tenants + 2 builds, filter by tenant A → only A's. (b) **__noChroot tracey annotation only** (GT2): `r[gw.reject.nochroot]` spec ¶ in gateway.md + `r[impl]` comments at `translate.rs:280` + `handler/build.rs:522` + `r[verify]` golden test (ATerm with `__noChroot="1"` → STDERR_ERROR). |
| **Primary files** | MODIFY `rio-scheduler/src/admin/tests.rs` (~30 lines); MODIFY `docs/src/components/gateway.md` (spec ¶); MODIFY `rio-gateway/src/translate.rs` (comment only); MODIFY `rio-gateway/src/handler/build.rs` (comment only); MODIFY or NEW golden test (~20 lines if fake drv_cache entry needed) |
| **Deps** | — |
| **Tracey** | `r[gw.reject.nochroot]` — spec+impl+verify all in this plan |
| **Serialization** | `handler/build.rs` count=19 but comment-only touch → safe parallel |
| **Risk** | `translate.rs:646` says "__noChroot rejection hard to unit-test here" — existing coverage may be weak. Budget 1h for NEW golden test if needed, not 15min annotation. |
| **Exit** | `/nbr .#ci` green. `tracey query rule gw.reject.nochroot` shows spec+impl+verify. |

#### P0227 — Migration 013: build_samples + retention task spawn
| | |
|---|---|
| **Tasks** | phase4c.md:13 — NEW numbered migration (013 per A1, resolve via `ls migrations/` at dispatch). Schema: `build_samples(id BIGSERIAL PK, pname TEXT, system TEXT, duration_secs DOUBLE PRECISION, peak_memory_bytes BIGINT, completed_at TIMESTAMPTZ DEFAULT now())` + `CREATE INDEX build_samples_completed_at_idx ON build_samples(completed_at)`. Retention query fn `delete_samples_older_than(pool, days)` in db.rs. Spawn retention task in main.rs on 1h interval. DELETE stale 009:8 comment "Part D (appended later)". |
| **Primary files** | NEW `migrations/013_build_samples.sql`; MODIFY `migrations/009_phase4.sql` (delete :8 stale comment); MODIFY `rio-scheduler/src/db.rs` (query stubs); MODIFY `rio-scheduler/src/main.rs` (spawn, ~5 lines); run `cargo sqlx prepare --workspace` → commit `.sqlx/` |
| **Deps** | `file:[P0206]` (4b migration 012 must exist first; same `migrations/` dir + `.sqlx/` regen) |
| **Tracey** | — |
| **Serialization** | `db.rs` count=29, `main.rs` count=26 — but both are small additions (query stubs + spawn line). Retention task spawn goes near existing bg-task block. |
| **Risk** | **NEVER append to 009** despite stale comment. Migration number: verify P0206 merged 012 first. `.sqlx/` regen must happen IN THIS PLAN — sqlx compile-check fails for P0228 without it. |
| **Exit** | `/nbr .#ci` green. `sqlx migrate run` applies clean; `SELECT * FROM build_samples` empty but schema correct. |

#### P0238 — BuildStatus fine-grained conditions (fields DEFERRED)
| | |
|---|---|
| **Tasks** | phase4c.md:60 — conditions ONLY (Scheduled/InputsResolved/Building) wired in `drain_stream` event handler. `criticalPathRemaining` + `workers` fields **DEFERRED Phase 5** per A3. SSA condition patch pattern from `scaling.rs:436 scaling_condition` — `lastTransitionTime` only on STATUS CHANGE (not every event). Add `TODO(phase5)` for deferred fields. Update `controller.md:30,43` → "conditions implemented, fields deferred Phase 5". |
| **Primary files** | MODIFY `rio-controller/src/reconcilers/build.rs` (drain_stream at :357,:426); MODIFY `rio-controller/src/crds/build.rs` (if conditions typed); MODIFY `docs/src/components/controller.md` (:30,:43) |
| **Deps** | — (no 4b plan touches reconcilers/build.rs — verified) |
| **Tracey** | — |
| **Serialization** | Single 4c touch on build.rs; no 4b collision |
| **Risk** | SSA condition patch inside event loop: use `apiVersion+kind` pattern from `workerpool/mod.rs:230-250` (3a bug). Per lang-gotchas memory: test asserts `.metadata.managedFields` entry for field manager, NOT just status value. |
| **Exit** | `/nbr .#ci` green. Unit test: mock BuildEvent stream → conditions transition correctly. |

#### P0240 — VM Section F+J scheduling fragments (cancel + load)
| | |
|---|---|
| **Tasks** | phase4c.md:43,47 — (F) submit slow build → CancelBuild mid-run → `rio_scheduler_cancel_signals_total` increments + **cgroup gone within 5s via direct `ls /sys/fs/cgroup/rio/...` NOT via prometheus** (tooling-gotchas: Tick-stale gauges 10s > 5s budget). (J) 50-drv DAG → `rio_scheduler_derivations_completed_total >= 50`. |
| **Primary files** | MODIFY `nix/tests/scenarios/scheduling.nix` (fragment keys: `cancel-timing` + `load-50drv`, TAIL-append) |
| **Deps** | — (tests existing features; `cancel_signals_total` + `derivations_completed_total` already exist) |
| **Tracey** | — |
| **Serialization** | scheduling.nix 699L, fragment pattern, no other 4c touch. No 4b plan found touching scheduling.nix. |
| **Risk** | cgroup-gone assertion: `kubectl exec worker ls /sys/fs/cgroup/rio/{drv-hash}` returning nonzero = gone. NOT a prom scrape. |
| **Exit** | `/nbr .#ci` green incl `vm-scheduling-*`. |

#### P0242 — VM Section I security fragments (cache auth + mTLS)
| | |
|---|---|
| **Tasks** | phase4c.md:46 — (i) unauth `curl /{hash}.narinfo` → 401, with Bearer from tenants seed → 200. Bonus: unauth `/nix-cache-info` → 200 (verifies P0225's route move). (ii) EXTEND existing `mtls-reject` subtest: gRPC QueryPathInfo no client cert → handshake fails. |
| **Primary files** | MODIFY `nix/tests/scenarios/security.nix` (fragment keys, TAIL-append) |
| **Deps** | weak:[P0225] (for /nix-cache-info bonus assert — skippable if P0225 not merged) |
| **Tracey** | — |
| **Serialization** | security.nix 403L, no other 4c touch |
| **Risk** | Find existing `mtls-reject` subtest — EXTEND don't duplicate. If not found, phase doc is stale — write fresh. |
| **Exit** | `/nbr .#ci` green incl `vm-security-*`. |

#### P0243 — VM FOD-proxy scenario + registration
| | |
|---|---|
| **Tasks** | phase4c.md:48-49 — Squid pod via existing `fod-proxy` image (GT4), busybox httpd origin (`drvs.coldBootstrapServer` pattern), ConfigMap patch allowlists local origin. Assert: (1) allowlisted FOD → success + `TCP_MISS` in squid log, (2) non-allowlisted → build fails + `TCP_DENIED/403`, (3) non-FOD → no `http_proxy` env (sentinel build writes `env` to `$out`). Register `vm-fod-proxy-k3s` in default.nix. |
| **Primary files** | NEW `nix/tests/scenarios/fod-proxy.nix`; MODIFY `nix/tests/default.nix` (import + mkTest) |
| **Deps** | — |
| **Tracey** | — |
| **Serialization** | **SOFT conflict with P0241 on default.nix** — both add 1 import + 1 mkTest. Coordinator dispatches sequentially, NOT a dag dep. |
| **Risk** | ConfigMap patch: Squid won't hot-reload. Either `kubectl delete pod` (restart) after patch, OR use helm `extraValues.fodProxy.extraAllowlist` if chart supports it. Check `fod-proxy.yaml:11` squid.conf template. |
| **Exit** | `/nbr .#ci` green incl new `vm-fod-proxy-k3s`. |

---

### Wave 2 — SITA-E spine (linear, 4 plans)

#### P0228 — completion.rs: build_samples write + class_drift_total counter
| | |
|---|---|
| **Tasks** | phase4c.md:14,19 + GT1 — (a) `insert_build_sample(pname, system, duration_secs, peak_memory_bytes)` call next to EMA update in success path. (b) **SIBLING counter `rio_scheduler_class_drift_total`** (GT1 correction): increment when `classify(actual_duration, actual_mem, &self.size_classes) != assigned_class`. Different from existing `misclassifications_total` (= penalty trigger, 2× cutoff). Register in lib.rs. Clarify both in `observability.md`. |
| **Primary files** | MODIFY `rio-scheduler/src/actor/completion.rs` (~15 lines near :298 EMA update + ~5 lines near :336 misclass block); MODIFY `rio-scheduler/src/db.rs` (`insert_build_sample` fn near :1087); MODIFY `rio-scheduler/src/lib.rs` (register counter near :147); MODIFY `docs/src/observability.md` (clarify both counters) |
| **Deps** | `table:[P0227]` + `file:[P0206, P0219]` — completion.rs serial after BOTH 4b touchers |
| **Tracey** | — |
| **Serialization** | **completion.rs count=20 — ONLY 4c touch.** P0206 adds `upsert_path_tenants` call in same handler; P0219 adds per-worker failure tracking. `git log -p completion.rs` at dispatch to find current success-path anchor. |
| **Risk** | `classify()` needs a `&[SizeClassConfig]` — at this point still static `&self.size_classes` (RwLock not yet wired). Fine — drift measured against config-at-dispatch. When P0230 wires RwLock, read site updates or stays pointing at Arc. |
| **Exit** | `/nbr .#ci` green. Integration: submit+complete → 1 row in `build_samples`. |

#### P0229 — CutoffRebalancer + class_load_fraction + convergence test
| | |
|---|---|
| **Tasks** | phase4c.md:15,18,21 — SITA-E algorithm: query 7d samples → sort by duration → partition into N classes with equal `sum(duration)` (cumsum then bisect at cumsum/N) → EMA smooth (α=0.3) against prev cutoffs → min_samples gate (100). `rio_scheduler_class_load_fraction` gauge emitted per recompute. Convergence test: seed bimodal (100@10s + 100@300s) → boundary ~150s ±ε in ≤3 iters. Test edge cases: uniform (degenerate), empty (skip). Pure module — spawning in P0230. |
| **Primary files** | NEW `rio-scheduler/src/rebalancer.rs` (~200 lines); NEW `rio-scheduler/src/rebalancer/tests.rs`; MODIFY `rio-scheduler/src/lib.rs` (mod decl + gauge register); MODIFY `rio-scheduler/src/db.rs` (`query_build_samples_last_7d`); MODIFY `docs/src/components/scheduler.md` (spec ¶ near :157-171) |
| **Deps** | `table:[P0228]` — convergence test seeds via `insert_build_sample` |
| **Tracey** | `r[sched.rebalancer.sita-e]` in scheduler.md + `r[impl]` at rebalancer.rs + `r[verify]` at tests.rs |
| **Serialization** | `db.rs` serial via dep chain. `lib.rs` TAIL-append near :147. |
| **Risk** | Edge cases: all-same-duration → degenerate cutoffs (handle gracefully); EMA α=0.3 means ~3 iters to converge — test tolerance must account for damping. |
| **Exit** | `/nbr .#ci` green. `tracey query rule sched.rebalancer.sita-e` shows impl+verify. |

#### P0230 — RwLock wire + CPU-bump classify (single assignment.rs+actor/mod.rs touch)
| | |
|---|---|
| **Tasks** | phase4c.md:16-17 + GT3 — (a) `Arc<parking_lot::RwLock<Vec<SizeClassConfig>>>` in DagActor (per A6); spawn rebalancer task with Arc clone; rebalancer writes `*lock.write() = new`. (b) `classify()` signature unchanged — CALLERS `.read()` then pass slice. (c) `cpu_limit_cores: Option<f64>` on SizeClassConfig at :55; classify() reads `ema_peak_cpu_cores` (already queried per GT3), bump if exceeds — mirror mem-bump pattern. Proptest: cpu-bump never goes DOWN. |
| **Primary files** | MODIFY `rio-scheduler/src/actor/mod.rs` (Arc field + rebalancer spawn); MODIFY `rio-scheduler/src/assignment.rs` (:55 struct field, :81 classify cpu-bump, :538 test helper extend); MODIFY `rio-scheduler/src/actor/dispatch.rs` (classify callsite: `.read()` guard); MODIFY `rio-scheduler/src/rebalancer.rs` (write-to-lock); MODIFY `docs/src/components/scheduler.md` (spec ¶ next to :164 mem-bump) |
| **Deps** | `module:[P0229]` |
| **Tracey** | **`r[sched.classify.cpu-bump]`** (RENAMED from phase doc's `sched.rebalancer.cpu-bump` per GT12 — family consistency with `sched.classify.mem-bump`) + `r[impl]` at assignment.rs + `r[verify]` proptest |
| **Serialization** | **actor/mod.rs count=31 (#2 hottest) + assignment.rs count=7 — SINGLE 4c touch each, bundled here.** Dispatch SOLO if concurrent plans touch actor/. |
| **Risk** | `parking_lot::RwLock` is sync — verify no `.await` held across read guard. Grep classify callsites: likely 1-2 in dispatch.rs. If an await IS between read() and classify(), clone under lock then drop guard. |
| **Exit** | `/nbr .#ci` green. `tracey query rule sched.classify.cpu-bump` shows impl+verify. Integration: seed bimodal, tick rebalancer, assert classify() output changes. |

#### P0231 — GetSizeClassStatus RPC (HUB)
| | |
|---|---|
| **Tasks** | phase4c.md:20 — `GetSizeClassSnapshot` ActorCommand (pattern: `ClusterSnapshot` at command.rs:199; O(n) ready-queue scan). Handler: read RwLock for effective_cutoff, scan queue per class, marshal to proto. Response: `SizeClassStatus{name, effective_cutoff_secs, configured_cutoff_secs, queued, running, sample_count}`. |
| **Primary files** | MODIFY `rio-proto/proto/admin.proto` (rpc after :36); MODIFY `rio-proto/proto/types.proto` (messages at EOF); MODIFY `rio-scheduler/src/actor/command.rs` (variant near :199); MODIFY `rio-scheduler/src/actor/mod.rs` (match arm); NEW `rio-scheduler/src/admin/sizeclass.rs`; MODIFY `rio-scheduler/src/admin/mod.rs` (wire) |
| **Deps** | `type:[P0230]` (reads the RwLock) + `file:[P0205, P0214]` (types.proto serial after 4b) |
| **Tracey** | — (hub is plumbing; autoscaler+cli get behavior markers) |
| **Serialization** | **types.proto count=27 HOTTEST** — EOF-append ONLY, serial after P0205+P0214. admin.proto count=2, low. actor/mod.rs serial after P0230 (dep enforces). |
| **Risk** | **Y-join blocker:** 3 downstream (P0234, P0236, P0237). `.#ci` includes proto regen — syntax error fails fast. Include proto roundtrip test. If P0231 stalls, entire right-half of phase stalls. |
| **Exit** | `/nbr .#ci` green. `grpcurl GetSizeClassStatus` on fresh scheduler returns all classes with sample_count=0. |

---

### Wave 3 — WPS spine (linear, 4 plans)

#### P0232 — WPS CRD struct + crdgen wire
| | |
|---|---|
| **Tasks** | phase4c.md:25-27 — `WorkerPoolSetSpec`, `SizeClassSpec`, `PoolTemplate` (subset of WorkerPoolSpec, includes `seccomp_profile` from P0223), `CutoffLearningConfig`, `WorkerPoolSetStatus`, `ClassStatus`. `#[derive(CustomResource, KubeSchema)]` on spec. `any_object` passthrough for k8s types. Add to crdgen.rs. DO NOT regen yet (P0235 does that). |
| **Primary files** | NEW `rio-controller/src/crds/workerpoolset.rs` (~150 lines, pattern from workerpool.rs:31-44,224-235); MODIFY `rio-controller/src/crds/mod.rs` (pub mod); MODIFY `rio-controller/src/bin/crdgen.rs` |
| **Deps** | `type:[P0223]` — PoolTemplate inherits seccomp_profile field consciously |
| **Tracey** | — |
| **Serialization** | All new files + 2 one-line mod appends |
| **Risk** | `KubeSchema` + CEL validation: verify kube-rs version supports it (check workerpool.rs already uses it per phase4c.md:26). |
| **Exit** | `/nbr .#ci` green. `nix-build-remote -- .#crds` produces YAML including WorkerPoolSet (don't commit yet). |

#### P0233 — WPS child builder + reconciler (no status yet)
| | |
|---|---|
| **Tasks** | phase4c.md:28-30,32 — `build_child_workerpool(wps, class) -> WorkerPool`: name=`{wps}-{class}`, sizeClass=class.name, merge shared + PoolTemplate, `controller_owner_ref(&())`. Reconciler: `apply` per-class SSA patch finalizer-wrapped (pattern workerpool/mod.rs:84-109); `cleanup` explicit child delete. Status refresh OMITTED (→ P0234, needs hub). Close `controller.md:100` + `scheduler.md:37,123` WPS deferrals. |
| **Primary files** | NEW `rio-controller/src/reconcilers/workerpoolset/{mod.rs,builders.rs}` (~250 lines total); MODIFY `rio-controller/src/reconcilers/mod.rs`; MODIFY `docs/src/components/{controller,scheduler}.md` (spec ¶ + close deferrals) |
| **Deps** | `type:[P0232]` |
| **Tracey** | `r[ctrl.wps.reconcile]` in controller.md + `r[impl]` at mod.rs + `r[verify]` DEFERRED to P0239 VM test (see §5 cross-plan note) |
| **Serialization** | New directory. reconcilers/mod.rs one-line. |
| **Risk** | `controller_owner_ref(&())` — `&()` because DynamicType=() for static CRDs. Type inference is unforgiving; copy literal snippet from workerpool/mod.rs. |
| **Exit** | `/nbr .#ci` green. Unit test: 3-class WPS → 3 WorkerPool patches with correct ownerRef UID. |

#### P0234 — WPS status refresh + per-class autoscaler (Y-JOIN)
| | |
|---|---|
| **Tasks** | phase4c.md:31,33,91 — (a) Status refresh in workerpoolset/mod.rs: call GetSizeClassStatus → write effective_cutoff + queued per class → **SSA patch WITH apiVersion+kind** (3a-bug, pattern workerpool/mod.rs:230-250). (b) scaling.rs:195 hook: call GetSizeClassStatus, iterate classes, `compute_desired(queued, target, min, max)` per child, SSA field manager `"rio-controller-wps-autoscaler"` (pattern :338), skip-deleting guard (:227-230). Close `challenges.md:138` WPS deferral. |
| **Primary files** | MODIFY `rio-controller/src/reconcilers/workerpoolset/mod.rs` (status block); MODIFY `rio-controller/src/scaling.rs` (:195); MODIFY `docs/src/components/controller.md` (spec ¶s); MODIFY `docs/src/challenges.md` (:138) |
| **Deps** | `proto:[P0231]` + `module:[P0233]` — **Y-JOIN of both spines** |
| **Tracey** | `r[ctrl.wps.autoscale]` + `r[ctrl.wps.cutoff-status]` in controller.md + `r[impl]` at scaling.rs/mod.rs + `r[verify]` unit test (mock GetSizeClassStatus → desired replicas computed). `r[verify]` for autoscale also in P0239 VM. |
| **Serialization** | scaling.rs count=8, single 4c touch. workerpoolset/mod.rs serial via dep. |
| **Risk** | SSA apiVersion+kind: include literal diff from 3a bug in plan doc. Per lang-gotchas: test asserts `.metadata.managedFields` has `"rio-controller-wps-autoscaler"` entry, NOT just replica value. |
| **Exit** | `/nbr .#ci` green. `tracey query rule ctrl.wps.autoscale` + `ctrl.wps.cutoff-status` show impl+verify. |

#### P0235 — WPS main.rs wire + RBAC + CRD regen
| | |
|---|---|
| **Tasks** | phase4c.md:34-35 — third `Controller::new(wps_api).owns(Api::<WorkerPool>::all()).run()`; `join!` → `join3` (or 3-arg join!). RBAC: add `workerpoolsets`+`workerpoolsets/status`+`workerpoolsets/finalizers` to ClusterRole. Regen: `nix-build-remote -- .#crds && ./scripts/split-crds.sh result` → commit `infra/helm/crds/*.yaml` (picks up BOTH P0223 seccomp AND P0232 WPS). Close `capacity-planning.md:64` + `decisions/015-size-class-routing.md:54-58` WPS deferrals. |
| **Primary files** | MODIFY `rio-controller/src/main.rs` (:315-364); MODIFY `infra/helm/rio-build/templates/rbac.yaml`; COMMIT `infra/helm/crds/*.yaml` (regen); MODIFY `docs/src/{capacity-planning.md,decisions/015-size-class-routing.md}` |
| **Deps** | `module:[P0234]` + `file:[P0212]` (4b gc_schedule in same join!) |
| **Tracey** | — |
| **Serialization** | **main.rs count=17, serial after P0212.** Single 4c touch. CRD regen picks up P0223's seccomp field too — diff should show 2 CRDs changed. |
| **Risk** | **NEVER local `nix build`** — use `/nbr .#crds` or raw `nix-build-remote --no-nom --dev -- .#crds`. `split-crds.sh result` runs on symlink. If P0223 merged first its regen already committed — re-regen here picks up both. |
| **Exit** | `/nbr .#ci` green. Controller log shows 3 reconcilers. `kubectl get crd workerpoolsets.rio.build` works in VM. |

---

### Wave 4 — CLI + VM tests (fan-out)

#### P0236 — rio-cli cutoffs subcommand
| | |
|---|---|
| **Tasks** | phase4c.md:36 — calls GetSizeClassStatus, prints `name|configured|effective|queued|running|samples` table. `--json` flag. |
| **Primary files** | NEW `rio-cli/src/cutoffs.rs`; MODIFY `rio-cli/src/main.rs` (:96 enum variant + match arm) |
| **Deps** | `proto:[P0231]` + `file:[P0216]` |
| **Tracey** | — |
| **Serialization** | main.rs serial after P0216. P0237 deps this. |
| **Exit** | `/nbr .#ci` green. |

#### P0237 — rio-cli wps subcommand
| | |
|---|---|
| **Tasks** | phase4c.md:36 — `wps get`/`describe` via kube-rs `Api<WorkerPoolSet>`. Shows spec + child WorkerPool status. |
| **Primary files** | NEW `rio-cli/src/wps.rs`; MODIFY `rio-cli/src/main.rs` |
| **Deps** | `type:[P0232]` + `file:[P0236]` + `file:[P0216]` |
| **Tracey** | — |
| **Serialization** | main.rs serial after P0236 (both add variants) |
| **Risk** | Verify P0216 set up `kube::Client::try_default()` — if not, add. |
| **Exit** | `/nbr .#ci` green. |

#### P0239 — VM PDB + Section H WPS lifecycle fragments
| | |
|---|---|
| **Tasks** | phase4c.md:40,45 — PDB: assert `{pool}-pdb` exists, `maxUnavailable:1`, ownerRef→WorkerPool; delete WP → PDB GC'd. Section H: apply WPS CR → 3 children with correct sizeClass+ownerRef → delete WPS → `retry()` until children gone. |
| **Primary files** | MODIFY `nix/tests/scenarios/lifecycle.nix` (fragment keys in `fragments = {` at :406: `pdb-ownerref` + `wps-lifecycle`); possibly MODIFY `nix/tests/default.nix` if new mkTest variant needed |
| **Deps** | `install:[P0235]` + `file:[P0206, P0207]` (4b lifecycle.nix gc-sweep extensions) |
| **Tracey** | **`r[verify ctrl.wps.reconcile]` + `r[verify ctrl.wps.autoscale]`** — comments at col-0 in .nix file header, NOT inside testScript literal (per tracey-adoption memory) |
| **Serialization** | lifecycle.nix count=6, fragment keys (attrset additions). 4b P0206/P0207 touch gc-sweep subtest ~:1800 — different section. |
| **Risk** | Check if CRDs applied by fixture or testScript. If `helm-render.nix` doesn't include workerpoolset CRD → add or apply via kubectl in testScript. May need separate mkTest (`vm-lifecycle-wps-k3s`) if k3s-full fixture setup is wrong shape. |
| **Exit** | `/nbr .#ci` green incl lifecycle VM tests. |

#### P0241 — VM Section G NetPol scenario + registration
| | |
|---|---|
| **Tasks** | phase4c.md:44,49 — NEW netpol.nix on k3s-full with `networkPolicy.enabled=true`. Assert `curl 169.254.169.254` blocked, `curl 1.1.1.1:80` blocked. Ingress SKIPPED (Phase 5). Register `vm-netpol-k3s` in default.nix. **Design branches on P0220 outcome** — default: stock k3s. If P0220 says needs-calico: add ~3 images to `nix/docker-pulled.nix` + k3s-full `--flannel-backend=none` param. |
| **Primary files** | NEW `nix/tests/scenarios/netpol.nix`; MODIFY `nix/tests/default.nix` |
| **Deps** | `decision:[P0220]` |
| **Tracey** | — |
| **Serialization** | **SOFT conflict with P0243 on default.nix** — coordinator dispatches sequentially |
| **Risk** | If P0220 outcome = Calico, plan BALLOONS ~50 lines + 3 docker images + fixture param. Re-estimate at dispatch. |
| **Exit** | `/nbr .#ci` green incl new `vm-netpol-k3s`. |

---

### Wave 5 — Closeout

#### P0244 — Doc-sync sweep + TODO retags + phase [x]
| | |
|---|---|
| **Tasks** | phase4c.md:66-85 — (1) **Deferral-block sweep (RESIDUAL only — implementing plans handled their own):** `errors.md:97` (4b P0214 implements timeout — verify merge, remove); `multi-tenancy.md:70,111,114` (4b→done); `observability.md:117,185,303` (grep `'(Phase 4+)'` not line#); `scheduler.md:384,392,398,413` (4a/4b tenant FK); `worker.md:190,194` (4b NAR scanner); `challenges.md:73,100` (4b FUSE breaker; :138 done by P0234); `failure-modes.md:52` (4b FUSE); `verification.md:10,83,94,68-73` (consolidate status). (2) `cgroup.rs:301` → improve comment to "unreachable under privileged:true; covered by ADR-012 device-plugin VM test when that lands"; keep `TODO(phase5)` tag. (3) `admin/builds.rs:22` retag `TODO(phase4c)` → `TODO(phase5)` (per A4). (4) `contributing.md:85` → phase5. (5) Mark all `[ ]`→`[x]` in `phase4{a,b,c}.md`. (6) Final `tracey query validate` = 0 errors. |
| **Primary files** | ~15 docs files + `rio-worker/src/cgroup.rs` (comment only) + `rio-scheduler/src/admin/builds.rs` (1-line retag) + `docs/src/phases/phase4{a,b,c}.md` |
| **Deps** | ALL (P0220-P0243). Practically: deps on leaf-tips [P0237, P0239, P0241, P0243, P0238, P0240, P0242] — transitive covers rest. |
| **Tracey** | — (but run `tracey query validate` as exit gate) |
| **Serialization** | LAST plan |
| **Risk** | Line numbers in phase doc are STALE (verified observability.md drift). Grep for content strings: `'(Phase 4+)'`, `'Phase 4 deferral'`, `'deferred to Phase 4'`, `'Not implemented (Phase 4)'`. Each implementing plan already removed its OWN deferral block — this sweep catches (a) already-done-in-prior-phase residuals, (b) 4b-implemented deferrals. |
| **Exit** | `/nbr .#ci` green. `grep -r 'Phase 4 deferral\|Not implemented (Phase 4)\|(Phase 4+)' docs/src/` → only legitimate Phase 5 mentions. `tracey query validate` = 0. `grep 'TODO(phase4' rio-*/src/` = 0. |

---

## §4 — Dependency Graph

```
DECISION GATE (dispatch FIRST):
  P0220 ─decision─→ P0241

WAVE 1 FRONTIER (10 plans, no intra-4c deps, dispatch @ t=0 once 4b merged):
  P0221  P0222  P0223  P0224  P0225  P0226  P0227  P0238  P0240  P0242  P0243
  (coordinator picks ≤10; P0220 dispatches first then slots free for 11th)

SITA-E SPINE (critical path leg 1, 5 hops):
  P0227 ─table→ P0228 ─table→ P0229 ─module→ P0230 ─type→ P0231 (HUB)
    ↑file:P0206                                    ↑file:P0205,P0214
    ↑file:P0206,P0219 (completion.rs)

WPS SPINE (leg 2, 4 hops, Y-joins at P0234):
  P0223 ─type→ P0232 ─type→ P0233 ─module→ P0234 ─module→ P0235
                                              ↑proto:P0231    ↑file:P0212
                               │                              │
                               └─type→ P0237 ←file:P0236      └─install→ P0239
                                              ↑proto:P0231            ↑file:P0206,P0207
                                              ↑file:P0216

Y-JOIN: P0234 needs BOTH P0231 (SITA-E hub) AND P0233 (WPS reconciler)

TERMINAL:
  {all} → P0244
```

### Critical path (9 hops, ~18 days @ 2 days/hop)

```
P0227 → P0228 → P0229 → P0230 → P0231 → P0234 → P0235 → P0239 → P0244
```

WPS spine (P0223→P0232→P0233) runs parallel, converges at P0234. At ≤10 parallel implementers, everything off critical path fits in its shadow.

### Topo tiers (by longest path from a root)

| Tier | Plans | Width |
|---|---|---|
| 0 | P0220 P0221 P0222 P0223 P0224 P0225 P0226 P0227 P0238 P0240 P0242 P0243 | **12** |
| 1 | P0228 P0232 P0241 | 3 |
| 2 | P0229 P0233 | 2 |
| 3 | P0230 | 1 |
| 4 | P0231 | 1 (HUB) |
| 5 | P0234 P0236 | 2 |
| 6 | P0235 P0237 | 2 |
| 7 | P0239 | 1 |
| 8 | P0244 | 1 |

### Dispatch priority within Wave 1 (by downstream unblock count)

| Priority | Plan | Unblocks | Rationale |
|---|---|---|---|
| 1 | **P0220** | 1 (but DECISION) | Outcome needed before P0241 doc body finalized |
| 2 | **P0227** | 6 (entire SITA-E + hub + fan-out) | Critical path head |
| 3 | **P0223** | 5 (entire WPS spine + P0237) | WPS spine head + seccomp feeds PoolTemplate |
| 4-10 | P0221-P0226, P0238, P0240, P0242, P0243 | 0-1 each | Fill slots; all terminal-except-closeout |

---

## §5 — Tracey Marker Distribution (8 markers, all placed)

| Marker | Spec ¶ lands in | `r[impl]` plan | `r[verify]` plan | Note |
|---|---|---|---|---|
| `sched.rebalancer.sita-e` | scheduler.md (P0229) | P0229 rebalancer.rs | P0229 convergence test | Self-contained |
| `sched.classify.cpu-bump` | scheduler.md (P0230) | P0230 assignment.rs | P0230 proptest | **RENAMED from `sched.rebalancer.cpu-bump`** (GT12) |
| `ctrl.wps.reconcile` | controller.md (P0233) | P0233 workerpoolset/mod.rs | **P0239** VM lifecycle.nix | Cross-plan verify — see note below |
| `ctrl.wps.autoscale` | controller.md (P0234) | P0234 scaling.rs | P0234 unit + **P0239** VM | Unit in same plan, VM later |
| `ctrl.wps.cutoff-status` | controller.md (P0234) | P0234 workerpoolset/mod.rs | P0234 unit test | Self-contained |
| `gw.reject.nochroot` | gateway.md (P0226) | P0226 (comment on EXISTING code) | P0226 golden test | Annotation-only plan |
| `store.hmac.san-bypass` | store.md (P0224) | P0224 put_path.rs | P0224 unit test | Self-contained |
| `worker.seccomp.localhost-profile` | security.md (P0223) | P0223 builders.rs | P0223 unit test | Self-contained |

**Cross-plan `r[verify]` note:** `ctrl.wps.reconcile` has `r[impl]` in P0233 but `r[verify]` in P0239. Between P0233 merge and P0239 merge, `tracey query untested` shows this rule. **EXPECTED, TRANSIENT.** Do NOT add a dummy `r[verify]` in P0233 to silence — that defeats the tool. P0233's plan doc should note: "r[verify] lands in P0239 VM test."

**.nix annotation placement:** Per tracey-adoption memory — `r[verify]` comments in `.nix` files go at **col-0 BEFORE the `{`**, NOT inline in testScript string literals (parser doesn't see those). P0239 puts them at file header.

---

## §6 — Risks & Mitigations

| # | Risk | Plan(s) | Mitigation |
|---|---|---|---|
| R1 | **misclassification task dropped silently** if GT1 not heeded | P0228 | Plan doc explicitly says: existing counter = penalty-trigger (2× cutoff), new sibling `class_drift_total` = classify-mismatch. Update observability.md with both. |
| R2 | **cas.rs scopeguard applied naively breaks Drop ordering** | P0225 | Plan doc quotes cas.rs:558-565 verbatim. Safe path: assess → document self-heal bound → delete TODO. Only implement if test can demonstrate observable leak. |
| R3 | **types.proto conflict with 4b** — 27-collision file, P0205+P0214 touch | P0231 | Hard dep on [P0205, P0214]. EOF-append only. Include proto roundtrip test so syntax errors fail fast. |
| R4 | **actor/mod.rs 31-collision — P0230 conflict with concurrent plans** | P0230 | Dispatch SOLO. No other 4c plan concurrent when P0230 in flight (coordinator policy, not dag). |
| R5 | **SSA status patch without apiVersion+kind** (3a bug redux) | P0234, P0238 | Both plan docs include literal failing/passing diff from workerpool/mod.rs:230-250. Tests assert `.metadata.managedFields` entry NOT just value (per lang-gotchas). |
| R6 | **cgroup-gone-5s vs 10s prom scrape** — assertion too tight for metric poll | P0240 | Assert via `kubectl exec worker ls /sys/fs/cgroup/rio/...` directly (per tooling-gotchas memory), NOT via prometheus. |
| R7 | **Seccomp Localhost profile path not installed on k3s VM nodes** | P0223 | Document as production-only; VM test uses RuntimeDefault. OR: init-container copies from ConfigMap to hostPath (+~20 lines). Decide per Q1. |
| R8 | **Squid won't hot-reload ConfigMap allowlist** | P0243 | `kubectl delete pod {squid}` after patch (restart), OR use helm `extraValues` if `fod-proxy.yaml` supports it. |
| R9 | **NetPol outcome = Calico** balloons P0241 scope | P0241 | P0220 runs FIRST. If Calico needed, `/plan` promotes fresh Calico-preload plan before P0241 dispatches. Budget: +50 lines docker-pulled.nix + fixture param. |
| R10 | **`parking_lot::RwLock` read held across await** | P0230 | Verify at implement: grep classify callsites, check for `.await` between `.read()` and use. If found, clone under lock then drop guard. |
| R11 | **CRD regen conflict** — P0223 and P0235 both regen | P0223, P0235 | Deterministic regen. Whoever merges second re-runs. Note in both plan docs: "on rebase, `nix-build-remote -- .#crds && ./scripts/split-crds.sh result`". |
| R12 | **completion.rs anchor moved by 4b** — P0206/P0219 refactor the success block | P0228 | Plan doc says "git log -p completion.rs at dispatch to find current EMA call site" — don't trust :298 line number. |
| R13 | **sqlx compile-check fails in P0228 if P0227 didn't commit .sqlx/** | P0228 | P0227 exit criteria includes `cargo sqlx prepare --workspace` → commit `.sqlx/`. |
| R14 | **HUB stall cascades** — P0231 CI failure blocks 3 downstream | P0231 | Include proto roundtrip test in P0231 test matrix. `.#ci` proto regen fails fast on syntax. Consider dispatching P0231 with elevated priority once P0230 merges. |
| R15 | **helm-render.nix doesn't include WPS CRD** — P0239 VM test can't apply the CR | P0239 | Check at dispatch: `grep workerpoolset nix/helm-render.nix`. If absent, add kubectl apply of regenerated CRD in testScript setup. |

---

## §7 — Coordinator Dispatch + Exit Gates

### dag-append commands (run after 4b all DONE)

```bash
cd /root/src/rio-build/main

# Wave 0-1 (frontier roots)
python3 .claude/lib/state.py dag-append '{"plan":220,"title":"NetPol pre-verify (DECISION)","deps":[],"crate":"nix/tests","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":221,"title":"rio-bench crate + Hydra doc","deps":[],"crate":"rio-bench","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":222,"title":"Grafana dashboards","deps":[],"crate":"infra","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":223,"title":"seccomp Localhost profile","deps":[],"crate":"rio-controller,infra","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":224,"title":"CN-list + SAN HMAC bypass","deps":[],"crate":"rio-store","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":225,"title":"rio-store: scopeguard assess + nix-cache-info","deps":[],"crate":"rio-store","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":226,"title":"Verify: cross-tenant + noChroot tracey","deps":[],"crate":"rio-scheduler,rio-gateway","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":227,"title":"Migration 013 build_samples + retention","deps":[206],"crate":"rio-scheduler","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":238,"title":"BuildStatus conditions (fields DEFERRED)","deps":[],"crate":"rio-controller","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":240,"title":"VM Section F+J scheduling","deps":[],"crate":"nix/tests","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":242,"title":"VM Section I security","deps":[],"crate":"nix/tests","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":243,"title":"VM FOD-proxy scenario","deps":[],"crate":"nix/tests","status":"UNIMPL","complexity":"MED"}'

# SITA-E spine
python3 .claude/lib/state.py dag-append '{"plan":228,"title":"completion: build_samples write + class_drift counter","deps":[227,206,219],"crate":"rio-scheduler","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":229,"title":"CutoffRebalancer + gauge + convergence","deps":[228],"crate":"rio-scheduler","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":230,"title":"RwLock wire + CPU-bump classify","deps":[229],"crate":"rio-scheduler","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":231,"title":"GetSizeClassStatus RPC (HUB)","deps":[230,205,214],"crate":"rio-proto,rio-scheduler","status":"UNIMPL","complexity":"MED"}'

# WPS spine
python3 .claude/lib/state.py dag-append '{"plan":232,"title":"WPS CRD struct + crdgen","deps":[223],"crate":"rio-controller","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":233,"title":"WPS child builder + reconciler","deps":[232],"crate":"rio-controller","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":234,"title":"WPS status + per-class autoscaler (Y-JOIN)","deps":[231,233],"crate":"rio-controller","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":235,"title":"WPS main wire + RBAC + CRD regen","deps":[234,212],"crate":"rio-controller,infra","status":"UNIMPL","complexity":"LOW"}'

# CLI + VM
python3 .claude/lib/state.py dag-append '{"plan":236,"title":"rio-cli cutoffs","deps":[231,216],"crate":"rio-cli","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":237,"title":"rio-cli wps","deps":[232,236,216],"crate":"rio-cli","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":239,"title":"VM PDB + Section H WPS","deps":[235,206,207],"crate":"nix/tests","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":241,"title":"VM Section G NetPol","deps":[220],"crate":"nix/tests","status":"UNIMPL","complexity":"MED"}'

# Closeout
python3 .claude/lib/state.py dag-append '{"plan":244,"title":"Doc-sync sweep + phase [x]","deps":[237,239,241,243,238,240,242,221,222,224,225,226],"crate":"docs","status":"UNIMPL","complexity":"LOW"}'

# Regenerate collision matrix with new plans' file lists
python3 .claude/lib/state.py collisions-regen

# Verify frontier
python3 .claude/lib/state.py dag-render | grep 'UNIMPL.*deps-met'
# Expect: P0220-P0227, P0238, P0240, P0242, P0243 = 12 ready (once 4b DONE)
```

### Coordinator dispatch script

```bash
# Day 1 — P0220 FIRST (decision gate, ~30min), then fill 10 slots
/implement P0220
# → P0220 completes, writes followup. Then:
/implement P0227 P0223 P0221 P0222 P0224 P0225 P0226 P0238 P0240 P0243
# = 10 plans, max width. P0242 queued.

# As slots free (after /merge-impl):
# P0227 merged → /implement P0228 (verify P0206+P0219 DONE first — completion.rs serial)
# P0223 merged → /implement P0232
# slot freed   → /implement P0242

# P0228 merged → /implement P0229
# P0232 merged → /implement P0233
# P0220 outcome written → /implement P0241 (SOFT serialize with P0243 on default.nix)

# P0229 merged → /implement P0230 (SOLO — actor/mod.rs #2 hottest, pause conflicting dispatches)
# P0233 merged → (P0234 still blocked on P0231)

# P0230 merged → /implement P0231 (HUB — elevated priority)

# P0231 merged → /implement P0234 P0236 (both unblocked)
# P0234 merged → /implement P0235 (verify P0212 DONE)
# P0236 merged → /implement P0237

# P0235 merged → /implement P0239 (verify P0206+P0207 DONE for lifecycle.nix)

# All merged → /implement P0244
```

### Phase exit gate (P0244's final check = phase4c milestone)

```bash
NIXBUILDNET_REUSE_BUILD_FAILURES=false nix-build-remote --no-nom -- -L .#ci
# → includes vm-netpol-k3s + vm-fod-proxy-k3s + all extended scenarios

nix develop -c tracey query validate
# → 0 errors with 8 new markers

nix develop -c tracey query status
# → all 8 4c markers show impl+verify (ctrl.wps.reconcile verify may show 1 not 2 if only VM — fine)

nix-build-remote --no-nom -- .#bench && cargo bench -p rio-bench -- submit_build
# → inspect p99 < 50ms @ N=100 (target, not gate)

grep -r 'Phase 4 deferral\|Not implemented (Phase 4)\|(Phase 4+)' docs/src/
# → empty or Phase-5-only

grep 'TODO(phase4' rio-*/src/
# → empty (all retagged or resolved)
```

---

## §8 — Hidden Dependencies (implementer checks at dispatch, NOT in dag)

| Plan | Check | Command |
|---|---|---|
| P0227 | Migration number free after 4b | `ls migrations/ \| sort -V \| tail -1` → expect 012, use 013 |
| P0227 | Commit `.sqlx/` regen | `nix develop -c cargo sqlx prepare --workspace` before commit |
| P0228 | completion.rs EMA anchor moved by 4b | `git log -p --since='1 month' -- rio-scheduler/src/actor/completion.rs` |
| P0230 | No `.await` between `.read()` and `classify()` | `grep -A3 'classify(' rio-scheduler/src/actor/dispatch.rs` |
| P0231 | types.proto EOF clean (no 4b trailing conflict) | `git log -p -- rio-proto/proto/types.proto \| head -50` |
| P0233 | `controller_owner_ref(&())` literal pattern | Copy from `rio-controller/src/reconcilers/workerpool/mod.rs` verbatim |
| P0234 | Admin client constructed in controller | `grep -rn 'connect_admin\|AdminServiceClient' rio-controller/src/` — if absent, add |
| P0235 | CRD regen picks up BOTH seccomp AND WPS | `diff` regenerated yaml — expect 2 CRDs changed if P0223 merged |
| P0237 | `kube::Client::try_default()` in rio-cli | `grep try_default rio-cli/src/` — add if P0216 didn't |
| P0239 | helm-render includes workerpoolset CRD | `grep workerpoolset nix/helm-render.nix` — if absent, kubectl apply in testScript |
| P0241 | P0220 outcome recorded | `jq 'select(.origin=="P0220")' .claude/state/followups-pending.jsonl` |
| P0243 | Squid ConfigMap supports `extraAllowlist` | `grep -A5 allowlist infra/helm/rio-build/templates/fod-proxy.yaml` |
| P0244 | All 4b deferrals actually closed by 4b | For each grep hit, `git log --all --oneline -S'<deferral text>'` shows 4b removal |

---

## Summary table (for rio-planner expansion)

| P# | Title | Deps | Tracey | Crate | Cplx | Hot files |
|---|---|---|---|---|---|---|
| 0220 | NetPol pre-verify (DECISION) | — | — | nix/tests | LOW | — |
| 0221 | rio-bench + Hydra doc | — | — | rio-bench | MED | Cargo.toml,flake.nix |
| 0222 | Grafana dashboards | — | — | infra | LOW | — |
| 0223 | seccomp Localhost profile | — | worker.seccomp.localhost-profile | rio-controller | MED | workerpool.rs |
| 0224 | CN-list + SAN HMAC bypass | — | store.hmac.san-bypass | rio-store | LOW | — |
| 0225 | rio-store: scopeguard assess + nix-cache-info | — | — | rio-store | LOW | — |
| 0226 | Verify: cross-tenant + noChroot tracey | — | gw.reject.nochroot | rio-sched,rio-gw | LOW | — |
| 0227 | Migration 013 build_samples + retention | 206 | — | rio-scheduler | LOW | db.rs(29),sched/main(26) |
| 0228 | completion: samples write + class_drift counter | 227,206,219 | — | rio-scheduler | LOW | **completion.rs(20)** |
| 0229 | CutoffRebalancer + gauge + convergence | 228 | sched.rebalancer.sita-e | rio-scheduler | MED | db.rs |
| 0230 | RwLock wire + CPU-bump classify | 229 | sched.classify.cpu-bump (RENAMED) | rio-scheduler | MED | **actor/mod.rs(31)**,assignment.rs(7) |
| 0231 | GetSizeClassStatus RPC (HUB) | 230,205,214 | — | rio-proto,rio-sched | MED | **types.proto(27)** |
| 0232 | WPS CRD struct + crdgen | 223 | — | rio-controller | MED | — |
| 0233 | WPS builder + reconciler | 232 | ctrl.wps.reconcile | rio-controller | MED | — |
| 0234 | WPS status + per-class autoscaler (Y-JOIN) | 231,233 | ctrl.wps.autoscale, ctrl.wps.cutoff-status | rio-controller | MED | scaling.rs(8) |
| 0235 | WPS main wire + RBAC + CRD regen | 234,212 | — | rio-controller,infra | LOW | **ctrl/main.rs(17)** |
| 0236 | rio-cli cutoffs | 231,216 | — | rio-cli | LOW | cli/main.rs |
| 0237 | rio-cli wps | 232,236,216 | — | rio-cli | LOW | cli/main.rs |
| 0238 | BuildStatus conditions (fields DEFERRED) | — | — | rio-controller | MED | reconcilers/build.rs |
| 0239 | VM PDB + Section H WPS | 235,206,207 | r[verify] ctrl.wps.* | nix/tests | MED | lifecycle.nix(6) |
| 0240 | VM Section F+J scheduling | — | — | nix/tests | LOW | scheduling.nix |
| 0241 | VM Section G NetPol | 220 | — | nix/tests | MED | default.nix(soft) |
| 0242 | VM Section I security | — | — | nix/tests | LOW | security.nix |
| 0243 | VM FOD-proxy | — | — | nix/tests | MED | default.nix(soft) |
| 0244 | Doc-sync sweep + phase [x] | ALL | — | docs | LOW | — |

**Frontier @ t=0:** 12 plans (P0220-P0227, P0238, P0240, P0242, P0243). **Critical path:** 9 hops. **Y-join:** P0234.

---

**Files referenced:**
- `/root/src/rio-build/main/docs/src/phases/phase4c.md` — source task list
- `/root/src/rio-build/main/.claude/collisions.jsonl` — hot-file counts
- `/root/src/rio-build/main/.claude/work/plan-02{04..19}-*.md` — 4b plan docs (resolved symbolic deps)
- `/root/src/rio-build/main/rio-scheduler/src/actor/completion.rs:320-345` — existing misclassification semantics (GT1)
- `/root/src/rio-build/main/rio-store/src/cas.rs:540-569` — scopeguard self-healing comment (GT11)
- `/root/src/rio-build/main/docs/src/components/scheduler.md:157-171` — existing `sched.classify.*` family (GT12)
- `/root/src/rio-build/main/migrations/` — last is 011, P0206 adds 012, 4c uses 013
