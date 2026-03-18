# FINAL Implementation Plan: Phase 5 Partition-Notes Document

**Deliverable:** `/root/src/rio-build/main/.claude/notes/phase5-partition.md` (~750 lines, matching §0-§8 format of `71de962:.claude/notes/phase4c-partition.md`)

**Lens:** MVP-first structure with risk-gating spikes. Phase-staged document production so the ground-truth section (§0-§1) ships first and is independently useful; spikes resolve the two highest-blast-radius unknowns before implementation lanes open; MVP-tier markers let the coordinator cut at plan boundaries under schedule pressure.

---

# USER DECISIONS (2026-03-18) — OVERRIDE DEFAULTS BELOW

| Item | Partition default | **USER DECISION** | Impact on plans |
|---|---|---|---|
| **Q2** | `CompletedViaCutoff` enum variant | **`Skipped`** terminal variant | P0252: same architecture, variant named `Skipped`. Metric stays `rio_scheduler_ca_cutoff_saves_total` (counts `→Skipped` transitions). |
| **Q3** | JSONB column | **Junction table** `realisation_deps(realisation_id, dep_realisation_id)` | P0249 migration: 2 tables not 1. Spike P0247 still captures wire shape but storage is decided. |
| **A10** | T1 (cuttable to Phase 6) | **T0 — NOT cuttable** | P0253 (CA resolution) moves to minimum-viable cut. Milestone demo (P0254 VM test) MUST include a CA-depends-on-CA chain. P0247 spike still samples nixpkgs frequency (informational, not decision-gating). |
| **A5** | Dual-mode ≥1 deploy cycle | **Dual-mode PERMANENT** | P0260: SSH-comment auth branch NEVER deleted. Two auth paths maintained. `r[gw.auth.tenant-from-key-comment]` unbumped. Operator chooses per-deployment via `gateway.toml auth_mode`. |
| **A6** | Rate-key = `tenant_id` UUID (bounded) | **Rate-key = per-session `jti`** (unbounded) | **P0261 is now MANDATORY**: governor key eviction via LRU (each SSH connect mints fresh jti → dashmap grows unbounded). Finer-grained rate-limiting (per-token, not per-tenant). |
| **Q4** | In-memory `HashSet<jti>` | **PG table `migration 016 jwt_revoked(jti, revoked_at)`** | P0259 adds migration. **TENSION:** gateway is PG-free (stateless HA). Resolution: revocation check routes via **scheduler** (already has PG + already does tenant-resolve — it can check `jti NOT IN jwt_revoked` in the same codepath). Gateway just forwards `jti` claim in `SubmitBuildRequest.jwt_jti`. Keeps gateway PG-free. |
| **Q5** | Gateway `handler/build.rs` | **Gateway** (default confirmed) | P0255 unchanged. Serializes with P0213 (4b) on `handler/build.rs`. |
| **Q1** | Sibling marker `r[sched.preempt.oom-migrate]` (3 plans) | **Proactive ema only** (2 plans) — see reframe below | Preempt lane shrinks. Spec untouched. No running-build-kill ever. |
| **A8** | One tracking plan P0273 | **Sub-partition HERE** | Add ~8-10 TypeScript plans (Wave-4, P0273-P0282 range). Total goes ~38-40 plans. Non-Rust: different validator criteria, separate `just ts-check` gate. |
| **Q6** | 2 faults (latency + reset) | **All 4 faults** | P0268 scope: latency 500ms + reset mid-PutPath + partition 30s + bandwidth 1Mbps. One plan, 4 subtests. |

## Q1 Reframe — "live preemption" is a non-feature

**User insight:** the problem phase5.md:27 describes shouldn't exist if size-classification works. Existing architecture already handles OOM:

| phase5.md wants | Already exists |
|---|---|
| Track memory like CPU | `build_samples.peak_memory_bytes` (P0227), `ema_peak_cpu_cores` (db.rs:931), `r[sched.classify.mem-bump]` |
| OOM → reschedule bigger | `r[sched.classify.penalty-overwrite]` @ completion.rs:336 — OOM records actual peak; next attempt bumped |
| Node never OOMs | k8s resource requests from `ema_peak_memory_bytes` → kube-scheduler packs correctly |

**What phase5.md:27 is actually reaching for:** proactive ema update. Mid-build `ResourceUsage` streaming via `ProgressUpdate` lets the scheduler update `ema_peak_memory_bytes` BEFORE completion — so the NEXT submit of that drv is right-sized immediately, not after a full OOM→retry cycle.

**Preempt lane becomes 2 plans (was 3):**
- **P0265**: proto `ProgressUpdate.resource_usage { peak_mem_bytes, peak_cpu_cores }` + worker emits from cgroup stats every N seconds
- **P0266**: scheduler consumes → `build_samples` ema update while build still running

NO kill, NO CRIU, NO `r[sched.preempt.oom-migrate]` sibling marker. `r[sched.preempt.never-running]` stands unbumped.

## CA cutoff: running-downstream question — spec stands

**User asked:** if A→B, A completes with cutoff-match, B already running — kill B?

**Answer: NO. Spec chose correctly.** `r[sched.preempt.never-running]` explicitly: "including for CA early cutoff." CA cutoff means "output identical" — letting B finish wastes CPU but produces the right output. Killing B adds a cancel-mid-build cleanup path (partial uploads, cgroup teardown) for marginal wall-clock savings in a rare scenario (B running AND A re-submitted while B still in-flight).

Cutoff propagates to `Queued` derivations only. Running ones finish.

## Revised plan count

| Lane | Original | Adjusted |
|---|---|---|
| Wave 0 spikes | 2 (P0245 GT, P0246 preempt-ADR, P0247 CA-spike) — wait, 3 | **P0246 dropped** (preempt reframe eliminates the ADR need) → 2 spikes |
| CA spine | P0250-P0254 (5) | Unchanged, P0253 promoted T1→T0 |
| JWT spine | P0259-P0261 | P0261 mandatory; +migration 016 in P0259 |
| Preempt lane | 3 | **2** (P0265-P0266, proactive ema only) |
| Dashboard | 1 tracking | **~8-10 plans** (P0273-P0282, sub-partition TBD) |
| Chaos | 1 plan 2 faults | 1 plan **4 faults** |

**Revised total: ~36-38 plans** (was 30). Dashboard sub-partition to be written separately before feeding to `/plan`.

---


# Assumptions & Open Questions — USER CONFIRMS BEFORE PROCEEDING

## Assumptions carried from scope (updated against disk @ `6b5d4f4`)

| # | Assumption | Basis | Falsification signal |
|---|---|---|---|
| **A1** | 4c implementation (P0220-P0244) merges before any Phase 5 plan dispatches; deps chain through concrete 4c numbers | 4c partition merged at 71de962 as notes-only; plan docs being written. Frontier blocked until P0244 merges. | `state.py dag-render` shows P0244 UNIMPL at P0245 dispatch → `/dag-run` idles correctly |
| **A2** | Migration numbering: 4b P0206=012, 4c P0227=013, **Phase 5 starts at 014**. Single batched migration plan (P0249) claims 014 + conditionally 015 based on P0247 spike outcome. | `ls migrations/` @ 6b5d4f4 ends at 011. sqlx checksum-lock means no post-merge renumbering. | `ls migrations/` at P0249 dispatch shows ≠013 last → renumber in plan doc |
| **A3** | "per-edge cutoff logic" (`phase5.md:10`) does NOT exist. Phase 5 adds CA-aware branch to `dag/mod.rs find_newly_ready()` dependency-unblocking. Verified: `rg cutoff rio-scheduler/src/` = 20 matches, all size-class duration routing. | GT1 — verified fact, not assumption | — |
| **A4** | **[CORRECTED from scope]** 4 `TODO(phase5)` comments ARE in tree @ 6b5d4f4: `opcodes_read.rs:493`, `chunk.rs:62`, `fuse/ops.rs:361`, `cgroup.rs:301`. Scope said "projected, land with 4b/4c" — stale. Additional TODOs from P0207/P0213 land as code comments; P0245 re-greps at dispatch. | `rg 'TODO\(phase5\)'` verified | — |
| **A5** | JWT rollout is **dual-mode**: SSH-key-comment auth remains valid alongside JWT for ≥1 deployment cycle. Gateway auth becomes two-branched (header present → JWT verify; absent → SSH-comment fallback). | Hard cutover = flag day for every CI integration. `multi-tenancy.md:19` deferral block describes JWT as additive, not replacing. `r[gw.auth.tenant-from-key-comment]` at `gateway.md:481` is a current normative requirement — bumping it is a separate decision from adding JWT. | User wants hard cutover → P0260 simplifies (delete SSH branch), but needs a deployment runbook plan |
| **A6** | JWT `sub` claim = **`tenant_id` UUID** per spec (`multi-tenancy.md:24`: "sub: the tenant_id"), NOT `tenant_name`. This is **server-resolved** (gateway looks up UUID from SSH-comment name via `r[sched.tenant.resolve]` before minting), therefore **bounded keyspace** → governor key-eviction (4b P0213 deferral) likely stays deferrable. | Spec authority + `r[sched.tenant.resolve]` already does name→UUID resolution scheduler-side; JWT moves resolution to gateway-at-mint-time but result is same bounded UUID set | User wants per-session `jti` as rate-limit key → unbounded keyspace → P0261 becomes mandatory LRU eviction |
| **A7** | Live preemption "checkpoint" = **kill + redispatch on larger size-class**, NOT CRIU process checkpoint. nix-daemon builds are not checkpointable (nested namespaces, FUSE mounts, no CRIU story). "Migration" = lose progress, restart on bigger node. | `r[sched.preempt.never-running]` at `scheduler.md:241` normative: "Nix builds cannot be paused or resumed." types.proto:151 comment uses same "checkpoint" language — it's a design-book misnomer for "redispatch marker." | P0246 spike finds trivial CRIU integration path → preemption lane balloons, separate roadmap needed |
| **A8** | Web dashboard = **one tracking plan** (P0273) deferring to separate sub-roadmap `.claude/notes/dashboard-partition.md`. TypeScript/gRPC-Web stack disjoint from rio-* crates; interleaving would pollute dag with non-Rust build deps. | Different toolchain (pnpm/vite), zero hot-file overlap, 10+ sub-plans if expanded here would push total to ~40. | User wants dashboard sub-partitioned in THIS doc → add §3b Wave-4 with ~10 TS plans |
| **A9** | `cgroup.rs:301` TODO(phase5) is **ADR-012 device-plugin track** per comment context — separate roadmap. NOT absorbed. P0274 retags to `TODO(adr-012)`. | 4c A5 already established this is doc-only; `cgroup.rs:304` comment references ADR-012 | User wants the `#[ignore]` test stub → 30min addition to P0274 |
| **A10** | CA resolution (P0253) serializes AFTER cutoff (P0250→P0252) on the same spine but is **individually cuttable** to Phase 6 if the common case (CA-output derivations with non-CA inputs) provides sufficient cutoff value. This satisfies the "COUPLED — must serialize" constraint via spine ordering while preserving a cut boundary. | Cutoff for non-CA-input CA drvs needs only `content_index` nar_hash comparison. Resolution (inputDrvs rewrite) is needed only when CA-depends-on-CA. Usage frequency of CA-on-CA in real nixpkgs TBD (P0247 spike samples this). | P0247 spike finds CA-on-CA is >30% of real CA usage → resolution is NOT cuttable, move P0253 to T0 tier, milestone demo (P0254 VM) must include a CA chain |
| **A11** | Atomic multi-output bug (`phase5.md:28` "partial registration possible") is **UNVERIFIED**. `rg 'atomic\|transaction\|BEGIN' rio-store/src/cas.rs` = zero tx wrapper found, but absence of wrapper ≠ presence of bug (may be single-statement INSERT that's already atomic). P0245 includes a GT-verification task constructing the failure case. | `put_path` handler structure unknown; may already batch in one statement | GT5 shows single-statement → P0267 becomes tracey-annotate-only (4c P0226 pattern) |

## Open Questions — resolved by spikes or user input before dispatch

| # | Question | Default if unanswered | Decides |
|---|---|---|---|
| **Q1** | **`r[sched.preempt.never-running]` semantic conflict** — spec at `scheduler.md:241-244` says "running builds are never preempted... including for CA early cutoff... Exception: the only case where a running build is killed is worker pod termination." phase5.md:27 wants OOM-migration to kill a running build. Three resolutions: **(a) Spec wins** — "migration" means queue-level reassignment of NOT-YET-DISPATCHED work when worker mem pressure rises; no kill. Lane E = 2 plans. **(b) Bump spec** — `r[sched.preempt.never-running+2]` adds second exception: "or imminent OOM detected." Lane E = 3 plans. **(c) Sibling marker** — OOM-migration is a third category (neither CA-cutoff-preempt nor queue-preempt); new `r[sched.preempt.oom-migrate]`, no bump. Lane E = 3 plans. | **(c) sibling marker** — preserves existing `never-running` contract for CA-cutoff case (which phase5.md:12 also cares about: "propagate cutoff" should NOT kill in-flight downstream), adds OOM as separately-tracked exception. P0245 seeds the sibling; P0246 spike writes ADR confirming. | P0245 marker-seed list; P0265-P0266 scope; whether `tracey bump` needed |
| **Q2** | CA cutoff state-machine: `dag/mod.rs find_newly_ready` requires `status == Queued`. Cutoff = `Queued → terminal` without `Ready→Assigned→Running`. `derivation.rs validate_transition` forbids this. New enum variant `CompletedViaCutoff` OR relax `Queued→Completed`? | **New terminal variant `CompletedViaCutoff`** — distinct for metrics (`rio_scheduler_ca_cutoff_saves_total`) + audit trail + recovery. Relaxing `Queued→Completed` loses the "why didn't this run?" signal and makes leader-failover recovery ambiguous. | P0252 state-machine implementation |
| **Q3** | `dependentRealisations` storage — `opcodes_read.rs:418` discards it, `:577` returns empty, `:586` hardcodes `{}`. Comment says "phase 5's early cutoff uses it." P0247 spike captures real wire shape. If it's a small `{drvout_hash: store_path}` map → `realisations.dependent_realisations JSONB` column. If large/queryable → junction table `realisation_deps`. | **JSONB column** (migration 015) — matches Nix's own SQLite `Realisations.dependentRealisations` TEXT blob; never queried by value; hot path is read-by-realisation-id. Defer normalization unless P0247 shows >50-entry maps. | P0249 migration content; P0253 impl shape |
| **Q4** | JWT `jti` revocation storage — spec at `multi-tenancy.md:27` says "unique token ID for revocation tracking." Needs persistence: migration 016 `jwt_revoked` table? Redis? In-memory (scheduler restart = all tokens invalid)? | **In-memory until evidence of need.** JWTs are short-lived (SSH session + grace per spec `:26`). Restart-invalidates-all is acceptable for MVP. `TODO(phase6)` comment for persistent revocation. | P0259 scope — whether to add table |
| **Q5** | Quota enforcement location: reject at **gateway** `handler/build.rs` (near P0213's rate-limit hook) or at **scheduler** `grpc/mod.rs SubmitBuild`? | **Gateway** — user sees failure in `nix build` output immediately via `STDERR_ERROR`, before SSH round-trip to scheduler completes. Matches 4b P0213 rate-limit pattern. Requires gateway→store RPC to query `tenant_store_bytes()`. | P0255 primary file (`handler/build.rs` vs `grpc/mod.rs`); hot-file serialization chain |
| **Q6** | Chaos harness MVP scenarios — toxiproxy supports latency/bandwidth/slow-close/timeout/reset/slicer. phase5.md:29 says only "network fault injection." Which 2-3 faults for MVP? | **(a)** scheduler↔store latency 500ms — exercises retry/timeout paths. **(b)** worker↔store connection reset mid-PutPath — exercises upload retry + the P0267 atomic-multi-output fix. Others → followups. | P0268 testScript scope |

---

# Phase 1 — Write §0 + §1 Ground-Truth (ship FIRST, independently useful)

These sections constrain every downstream plan's file list and scope. Write them before any plan rows. If nothing else ships, §1 is still useful for reviewers of `phase5.md`.

## Step 1.1 — §1 Ground-Truth table (16 rows, all disk-verified @ `6b5d4f4`)

| GT# | phase5.md claim | Reality @ `6b5d4f4` | Resolution |
|---|---|---|---|
| **GT1** | L10: "connected to the scheduler's per-edge cutoff logic" | **NO SUCH THING.** `rg cutoff rio-scheduler/src/` = 20 matches, ALL size-class duration routing (`assignment.rs:59 cutoff_secs`, `completion.rs:312-334` 2×-cutoff misclassification penalty, `dispatch.rs:184-205`). The "per-edge" infra is `dag/mod.rs find_newly_ready()` — dependency-completion unblocking, zero CA awareness. | P0245 corrects phase5.md:10 → "connects to `find_newly_ready` dependency-unblocking; Phase 5 ADDS a hash-comparison branch." P0252 implements the branch. |
| **GT2** | L18: "`wopQueryRealisation` (43) and `wopRegisterDrvOutput` (42) are already implemented as working read/write" | **OPCODES WORK, BUT `dependentRealisations` IS DISCARDED.** `opcodes_read.rs:418` doc: "ignored — phase 5's early cutoff uses it"; `:577`: "always empty (phase 5 populates it)"; `:586`: hardcoded `"dependentRealisations": {}`. `realisations.rs:52 insert()` + `:80 query()` work end-to-end for the OTHER fields. | NOT "just connect" — **populate the discarded field first.** P0247 spike captures real wire shape. P0253 stops discarding on write + populates on read. Migration 015 adds storage (per Q3 default). |
| **GT3** | (implicit) scheduler has CA derivation state | **ZERO.** `rg 'is_ca\|content_addressed\|ca_mode' rio-scheduler/src/` = only `is_cancelled` false-positives. `DerivationState` struct has no CA field. | P0250 introduces `is_ca: bool` on proto + `DerivationState` from scratch. |
| **GT4** | (implicit) CA detection needs building | **DETECTION EXISTS.** `rio-nix/src/derivation/mod.rs:222 has_ca_floating_outputs()` parses `hash_algo set + hash empty`. Tested at `hash.rs:164,291`. **Not plumbed to scheduler.** | P0250 scope **SHRINKS ~30 lines**: call existing fn in `translate.rs`, set proto field. No ATerm parser work. |
| **GT5** | (implicit from L18) realisation data sufficient for cutoff | `opcodes_read.rs:493 TODO(phase5)` — `output_hash` stored as **zeros** in realisations table. Comment: "Zeros are fine for cache-hit purposes — QueryRealisation only uses (drv_hash, output_name)." | **Cutoff does NOT read `realisations.output_hash`** — it compares completion's `nar_hash` vs `content_index` lookup (`content_index.rs:12` "content_hash = nar_hash"). The `:493` TODO is a **SIGNING concern** (signing a realisation needs the real hash), NOT on CA-cutoff critical path. Absorbed by **P0256** (signing plan) as T-line-item, NOT by CA spine. |
| **GT6** | L25: "PutChunk RPC" is new work | **PROTO EXISTS.** `types.proto:585 PutChunkRequest/Metadata/Response` + `store.proto:67 rpc PutChunk(stream)`. Server stub at `chunk.rs:50-73` returns `Status::unimplemented` with `TODO(phase5)` at `:62`. `FindMissingChunks` fully implemented. | P0262 scope shrinks: **zero proto work**, fill server stub only. |
| **GT7** | L27: ResourceUsage streaming via ProgressUpdate is new | **PROTO DONE.** `types.proto:401 ResourceUsage` + `:338 ProgressUpdate` with `resources` field at `:340`+`:352`. Proto comment at `:149-152` literally describes the Phase 5 use case: "mid-build ResourceUsage to detect 'about to OOM, migrate'. Needs worker-side checkpoint support first." ResourceUsage already plumbed through Heartbeat (`cgroup.rs:573 to_proto()`). | **Zero proto touch for preemption lane.** P0265 = worker-side emission cadence only. P0266 = scheduler consumption + threshold. |
| **GT8** | L22: JWT is greenfield | **SPEC FULLY WRITTEN** at `multi-tenancy.md:19-31` (Phase-5-deferral block): `sub`=tenant_id/`iat`/`exp`/`jti` claims, ed25519 K8s Secret, `x-rio-tenant-token` header, ConfigMap pubkey distribution, SIGHUP reload. `rio-common/src/hmac.rs` has a Claims struct for assignment-token HMAC — reusable **pattern** (different key, same shape). | JWT plans cite `multi-tenancy.md:19-31` as spec authority. P0257 mirrors `hmac.rs` module shape. NOTE per A6: `sub` = tenant_id UUID (server-resolved), bounded. |
| **GT9** | **[NEW — spec conflict]** L27 "Live preemption / migration... scheduler detects 'about to OOM, move it'" vs existing `r[sched.preempt.never-running]` | `scheduler.md:241-244` normative: "running builds are **never preempted or cancelled** — including for CA early cutoff... **Exception**: the only case where a running build is killed is worker pod termination." phase5.md:27 adds a **SECOND exception** (OOM-migration kill+redispatch) without acknowledging the first. | **P0246 SPIKE** writes `docs/src/decisions/017-oom-migration.md` resolving Q1. P0245 seeds `r[sched.preempt.oom-migrate]` as SIBLING (per Q1 default (c)), does NOT bump `never-running`. **IF spike concludes (a) queue-only** → P0265-P0266 scope shrinks, no spec touch. **IF (b) bump** → P0245's seed becomes a `tracey bump` instead. |
| **GT10** | L21: "Phase 4 signs all narinfo with a single cluster key" | **CONFIRMED.** `signing.rs` single `SigningKey`, no tenant lookup. `multi-tenancy.md:64` deferral block agrees. | P0256 adds `tenant_keys` table (via P0249 migration 014) + lookup by `tenant_id`. Cluster key stays as fallback when tenant has no key. |
| **GT11** | (4b P0213 deferral) governor rate-limiter exists | **DOES NOT EXIST @ 6b5d4f4** — `rg governor rio-gateway/` = empty. Lands with P0213. P0213 plan doc says `TODO(phase5): eviction if keys ever become client-controlled`. BUT `r[gw.rate.per-tenant]` already exists at `gateway.md:657` — verify at dispatch whether it describes the pre-P0213 or post-P0213 state. | P0261 deps on `file:[P0213]` (ratelimit.rs must exist). Per A6: `sub`=tenant UUID is server-issued → bounded → **P0261 likely = assess-then-document** (4c A7 scopeguard pattern). Plan doc includes the decision tree. |
| **GT12** | L29: chaos harness | **ZERO** toxiproxy/chaos anywhere in `nix/tests/` or `rio-*/src/`. Clean slate. | P0268 pure greenfield. Max parallel. |
| **GT13** | L28: "currently partial registration is possible on upload failure" | **UNVERIFIED BUG.** `rg 'atomic\|transaction\|BEGIN' rio-store/src/cas.rs` = zero tx wrapper. BUT absence of wrapper ≠ bug (single-statement insert = atomic). | P0245 includes GT-verify task: construct 2-output drv, inject fault between output-1 and output-2 PutPath, check for orphan. **IF single-statement** → P0267 = tracey-annotate-only. **IF real** → P0267 = tx wrap. |
| **GT14** | (existing markers — namespace collision check) | `r[sched.preempt.never-running]` @ scheduler.md:241 — **CONFLICTS** (see GT9). `r[store.chunk.refcount-txn]` @ store.md:78 — **ADJACENT** (grace-TTL joins this family). `r[gw.auth.tenant-from-key-comment]` @ gateway.md:481 — **ADJACENT** (JWT dual-mode must not contradict). `r[gw.rate.per-tenant]` @ gateway.md:657 — **check at P0261 dispatch** (may already describe governor). `r[store.gc.tenant-quota]` @ store.md:207 — **ADJACENT** (enforcement vs accounting distinction). `r[sched.tenant.resolve]` @ scheduler.md:91 — **ADJACENT** (JWT moves resolution point). ZERO existing `sched.ca.*`, `gw.jwt.*`, `store.atomic.*`. | P0245 marker-seed list joins existing families where they exist: `store.chunk.grace-ttl` sibling to `refcount-txn`; `store.gc.tenant-quota-enforce` sibling to `tenant-quota` (distinguishes enforcement from accounting). P0245 does NOT bump `gw.auth.tenant-from-key-comment` (dual-mode preserves it). |
| **GT15** | **[stale-forward]** `proto.md:252` says "The tenant's JWT is propagated via gRPC metadata (`x-rio-tenant-token`)" in **present tense** | `multi-tenancy.md:19` says "completely unimplemented... `tenant_id` is an empty string in all gRPC metadata; no issuance, signing, propagation, or verification exists." Docs contradict each other. | P0245 adds `> **Phase 5 deferral:**` caveat to `proto.md:252`. P0259 (verify middleware) removes the caveat when it makes the claim true. |
| **GT16** | **[milestone exit gate]** phase5.md:47 milestone: "CA early cutoff skips downstream... tenant isolation enforced" | `introduction.md:50` has a **removable warning**: "Multi-tenant deployments with untrusted tenants are **unsafe before Phase 5**. Prior to Phase 5, resource quotas are not enforced, per-tenant signing keys are not available, and data isolation relies on incomplete query-level filtering." | **THIS is the phase exit gate.** P0274 closeout removes `introduction.md:50` warning IFF: P0255 (quota-reject) + P0256 (per-tenant keys) + P0272 (narinfo filter) all merged. CA cutoff is the headline feature but NOT the safety gate — it's an optimization. MVP tier weighting reflects this. |

**Re-verify at dispatch** (when P0244 merged):
```bash
cd /root/src/rio-build/main
rg -n 'TODO\(phase5\)' -- '**/*.rs'                              # GT-refresh — expect ≥4, capture new from P0207/P0213
rg -n 'governor|DefaultKeyedRateLimiter' rio-gateway/src/        # GT11 — should NOW exist post-P0213
ls migrations/ | sort -V | tail -1                               # GT-migration — expect 013, use 014
rg -n 'r\[gw.rate' docs/src/components/gateway.md                # GT14 — check if P0213 touched the marker
git log --oneline -5 -- rio-scheduler/src/actor/completion.rs    # find P0228's anchor for CA spine
```

## Step 1.2 — §0 tables

Copy the Assumptions (A1-A11) and Open Questions (Q1-Q6) tables from above **verbatim** into the partition doc §0. They are already in 4c format.

---

# Phase 2 — Spike Plans (Wave 0 decision gates, BEFORE implementation lanes open)

The two spikes de-risk the highest-blast-radius unknowns. They produce ADR documents, not code. They run in parallel alongside P0245 (docs-only, no file collision). Lanes cannot dispatch until spike outcomes are recorded.

## Step 2.1 — Wave-0 plan rows (3 plans)

### P0245 — Prologue: phase5.md corrections + 15 marker seeds + GT-verify tasks + spec caveats

| | |
|---|---|
| **MVP Tier** | **T0 — FRONTIER ROOT** |
| **Tasks** | (T1) Correct `phase5.md:10` per GT1: "connects to scheduler's `find_newly_ready` dependency-unblocking; Phase 5 ADDS the hash-comparison branch (no pre-existing CA cutoff infrastructure)." Add GT4 note: "`has_ca_floating_outputs()` exists at `rio-nix/src/derivation/mod.rs:222` — plumb it." (T2) Seed **15 markers** (zero `r[impl]`/`r[verify]` — definitions only), joining existing families where present: `sched.ca.detect`, `sched.ca.cutoff-compare`, `sched.ca.cutoff-propagate`, `sched.ca.resolve` → `scheduler.md`; `sched.preempt.oom-migrate` → `scheduler.md` sibling to `never-running` (per Q1 default (c)); `store.gc.tenant-quota-enforce` → `store.md` sibling to existing `tenant-quota`; `store.tenant.sign-key`, `store.tenant.narinfo-filter`, `store.chunk.put-standalone`, `store.chunk.grace-ttl` (sibling to `refcount-txn`), `store.atomic.multi-output` → `store.md`; `gw.jwt.issue`, `gw.jwt.verify`, `gw.jwt.claims`, `gw.jwt.dual-mode` → `gateway.md`. (T3) GT15 fix: add `> **Phase 5 deferral:**` caveat to `proto.md:252` (present-tense JWT claim is aspirational). (T4) GT13 verify task: construct 2-output drv, inject mid-upload fault, check for orphan — write outcome to a `<!-- GT13-OUTCOME: {real|false-alarm} -->` comment in this partition doc §1. (T5) `rg 'TODO\(phase5\)'` audit — confirm 4 known (`opcodes_read.rs:493`, `chunk.rs:62`, `fuse/ops.rs:361`, `cgroup.rs:301`) + capture new from P0207/P0213. (T6) `tracey query validate` = 0 errors (15 new uncovered expected). |
| **Primary files** | MODIFY `docs/src/phases/phase5.md`; MODIFY `docs/src/components/{scheduler,store,gateway,proto}.md`; MODIFY `docs/src/multi-tenancy.md` (optional: marker ¶s); MODIFY `.claude/notes/phase5-partition.md` (GT13 outcome) |
| **Deps** | `P0244` (4c closeout — ensures `admin/builds.rs:22` retagged to phase5, all 4c deferral blocks closed) |
| **Tracey** | Seeds 15 markers. `tracey query uncovered` +15. `tracey query validate` = 0. |
| **Serialization** | Docs-only. No code-file collision. |
| **Exit** | `/nbr .#ci` green. `rg 'per-edge cutoff' docs/src/phases/phase5.md` = 0. `tracey query uncovered | wc -l` ≥15 over baseline. |

### P0246 — SPIKE: Preemption semantics ADR (DECISION GATE for Lane E)

| | |
|---|---|
| **MVP Tier** | **T0 — DECISION** (like 4c P0220 NetPol pre-verify) |
| **Tasks** | 4-hour timebox. NOT implementation. (T1) Read full `r[sched.preempt.never-running]` context + the "Exception: worker pod termination" escape hatch — OOM-migration looks structurally identical (forced kill outside the build's control). (T2) Survey: does Nix's own Hydra/nix-daemon do anything like OOM-migration? (likely no — informs how novel this is). (T3) Decide Q1 resolution: (a) queue-only, (b) bump spec, (c) sibling marker. (T4) Write `docs/src/decisions/017-oom-migration.md` (~2 pages): define "checkpoint" = kill+redispatch (NOT CRIU, per A7), define OOM-detection heuristic (proposed: `peak_mem > 0.9 × worker_mem_limit` for 2 consecutive ProgressUpdates 10s apart), define redispatch policy (same build_id, next-larger size class, max 1 migration per build to prevent thrash). (T5) IF outcome = (b), stage `tracey bump r[sched.preempt.never-running]` with rewritten spec text. IF (c), no bump — P0245's sibling seed stands. (T6) Update this partition doc §0 Q1 with outcome + §3 P0265/P0266 scope notes. |
| **Primary files** | NEW `docs/src/decisions/017-oom-migration.md`; POSSIBLY `docs/src/components/scheduler.md` (if (b) — bump+rewrite); MODIFY `.claude/notes/phase5-partition.md` (Q1 outcome) |
| **Deps** | `P0245` (marker seed exists to reference) |
| **Tracey** | IF (b): bumps `r[sched.preempt.never-running]`. IF (c): none. |
| **Serialization** | Docs-only. Parallel with P0247. |
| **Exit** | ADR merged. Q1 has definitive answer. Followup written: `{"origin":"P0246","decision":"(a|b|c)","preempt_lane_plans":[265,266] or [265]}`. |

### P0247 — SPIKE: CA wire-capture + dependentRealisations schema ADR (DECISION GATE for Lane A + migration)

| | |
|---|---|
| **MVP Tier** | **T0 — DECISION** |
| **Tasks** | 8-hour timebox. NOT implementation. (T1) In a VM test shell (`nix-build-remote -- .#checks.x86_64-linux.vm-phase3a.driverInteractive`): build a 2-deep CA derivation chain via real `nix build --rebuild` with `__contentAddressed = true;`. (T2) Capture `wopRegisterDrvOutput` JSON payloads (strace `write(daemon_socket)` or golden-capture existing test infra) — record actual `dependentRealisations` shape + cardinality in real Nix. (T3) Diff resolved vs unresolved ATerm (what does `inputDrvs` look like before/after resolution?). (T4) Sample: how common is CA-depends-on-CA in real nixpkgs? (`rg '__contentAddressed' $(nix-instantiate '<nixpkgs>' -A stdenv) | wc -l` heuristic). (T5) Decide Q3: JSONB column vs junction table. (T6) Decide A10: is resolution cuttable (CA-on-CA rare) or T0-mandatory (CA-on-CA common)? (T7) Write `docs/src/decisions/018-ca-resolution.md` with captured wire samples as appendix. (T8) Save captured bytes to `rio-gateway/tests/golden/corpus/ca-*` for future conformance tests. |
| **Primary files** | NEW `docs/src/decisions/018-ca-resolution.md`; NEW `rio-gateway/tests/golden/corpus/ca-{register,query}-*.bin` (captured wire); MODIFY `.claude/notes/phase5-partition.md` (Q3+A10 outcomes) |
| **Deps** | `P0245` |
| **Tracey** | none |
| **Serialization** | Docs + corpus. Parallel with P0246. |
| **Exit** | ADR merged. Q3 + A10 definitive. Followup: `{"origin":"P0247","dependent_storage":"jsonb|junction","resolution_tier":"T0|T2-cuttable","ca_on_ca_frequency":"<pct>"}`. |

---

# Phase 3 — MVP Plan Table (§3 core — T0 tier, the 6-plan minimum cut)

## Step 3.1 — Define MVP shipping threshold (goes in new §2 of partition doc)

The `phase5.md:47` milestone is two clauses. Per GT16, the **production safety gate** (`introduction.md:50` warning removal) is tenant isolation, NOT CA cutoff. MVP weighting:

| Clause | MVP plans | Safety-gate? |
|---|---|---|
| "tenant isolation enforced" | P0255 (quota-reject) + P0256 (per-tenant signing) + P0272 (narinfo filter) | **YES** — these three close `introduction.md:50`'s three named gaps |
| "CA early cutoff skips downstream" | P0250 → P0251 → P0252 → P0254 (detect → compare → propagate → VM demo) | NO — headline feature, pure optimization |

**Minimum-viable Phase 5 = 7 implementation plans + 3 wave-0 = 10 plans.** Coordinator can cut here under extreme schedule pressure and still remove the safety warning + demonstrate the headline feature.

## Step 3.2 — T0 plan rows

### Hot-file prologues (2 plans, serial, gated on spikes)

**P0248 — types.proto: `is_ca` field (SINGLE phase-5 proto touch)**

| | |
|---|---|
| **MVP Tier** | **T0 — proto prologue** |
| **Tasks** | (T1) `types.proto`: `bool is_content_addressed` on `DerivationInfo` (EOF-append). Per GT6+GT7+GT8: PutChunk proto exists, ResourceUsage exists, JWT uses string metadata header — **this is the ONLY required Phase 5 types.proto touch.** (T2) Proto roundtrip test (so syntax errors fail fast). NOTE: P0270 (BuildStatus fields) + P0271 (cursor) touch `admin.proto` not `types.proto` — lower collision, don't batch here. |
| **Primary files** | MODIFY `rio-proto/proto/types.proto` (EOF-append, ~3 lines) |
| **Deps** | `P0245` + `file:[P0231]` (4c HUB — last types.proto toucher) |
| **Tracey** | none (plumbing) |
| **Serialization** | **types.proto count=27 HOTTEST. Serial after P0231. This is the ONLY Phase 5 types.proto touch — pressure LOWER than 4c (which had 3 touches).** |
| **Exit** | `/nbr .#ci` green. Proto regen clean. |

**P0249 — Migration batch: 014_tenant_keys + conditional 015_realisations_deps**

| | |
|---|---|
| **MVP Tier** | **T0 — migration prologue** |
| **Tasks** | (T1) `014_tenant_keys.sql`: `CREATE TABLE tenant_keys (key_id SERIAL PRIMARY KEY, tenant_id UUID NOT NULL REFERENCES tenants, key_name TEXT NOT NULL, ed25519_seed BYTEA NOT NULL, created_at TIMESTAMPTZ DEFAULT now(), revoked_at TIMESTAMPTZ NULL); CREATE INDEX ON tenant_keys(tenant_id) WHERE revoked_at IS NULL;`. (T2) **IF P0247 outcome = "storage needed"**: `015_realisations_deps.sql`: `ALTER TABLE realisations ADD COLUMN dependent_realisations JSONB DEFAULT '{}'::jsonb;` (per Q3 default). IF P0247 says junction: `CREATE TABLE realisation_deps (...)` instead. IF P0247 says "not needed for cutoff, only resolution, resolution=T2-cuttable" → defer 015 to phase 6. (T3) `cargo sqlx prepare --workspace` → commit `.sqlx/`. (T4) `ls migrations/` verify 013 is last — renumber if not. |
| **Primary files** | NEW `migrations/014_tenant_keys.sql`; CONDITIONAL NEW `migrations/015_realisations_deps.sql`; MODIFY `.sqlx/` |
| **Deps** | `decision:[P0247]` (015 content gated) + `table:[P0227]` (4c migration 013 exists) |
| **Tracey** | none |
| **Serialization** | sqlx checksum-lock. Single migration commit. `.sqlx/` regen MUST be in exit criteria (4c R13). |
| **Exit** | `/nbr .#ci` green. `sqlx migrate run` clean on fresh DB. `.sqlx/` committed. |

### CA spine (4 plans, linear)

**P0250 — CA detect: plumb `is_ca` flag end-to-end**

| | |
|---|---|
| **MVP Tier** | **T0 — CA spine hop 1** |
| **Tasks** | (T1) `rio-gateway/src/translate.rs`: call `drv.has_ca_floating_outputs()` (EXISTS per GT4), set proto `is_content_addressed`. (T2) `rio-scheduler/src/state/derivation.rs`: add `pub is_ca: bool` on `DerivationState`. (T3) `rio-scheduler/src/actor/merge.rs`: populate from proto on DAG merge. (T4) `rio-scheduler/src/db.rs`: `is_ca BOOLEAN DEFAULT FALSE` column — inline migration hunk `ALTER TABLE derivations ADD COLUMN IF NOT EXISTS is_ca ...` OR batch into P0249 (DECISION: batch — single migration plan, this becomes a `.sqlx/` regen only). (T5) Unit: submit CA drv fixture → assert `state.is_ca`. |
| **Primary files** | MODIFY `rio-gateway/src/translate.rs`; MODIFY `rio-scheduler/src/state/derivation.rs`; MODIFY `rio-scheduler/src/actor/merge.rs`; MODIFY `rio-scheduler/src/db.rs` (query only); MODIFY `.sqlx/` |
| **Deps** | `P0248` (proto field) + `P0249` (column exists) + `file:[P0229]` (4c last db.rs touch) |
| **Tracey** | `r[impl sched.ca.detect]` at `translate.rs` + `merge.rs`. `r[verify]` unit test. |
| **Serialization** | **db.rs count=29 — serial after P0229.** `derivation.rs` + `merge.rs` low collision. |
| **Exit** | `/nbr .#ci` green. `tracey query rule sched.ca.detect` = impl+verify. |

**P0251 — CA cutoff COMPARE: completion.rs hash-check hook**

| | |
|---|---|
| **MVP Tier** | **T0 — CA spine hop 2** |
| **Tasks** | (T1) `completion.rs` success path (anchor near P0228's EMA update — **`git log -p completion.rs` at dispatch, do NOT trust :298**): IF `state.is_ca` → fetch completion's `nar_hash` per output → gRPC `QueryContentIndex(content_hash = nar_hash)` (`content_index.rs:12` semantics) → compare. (T2) Store result `ca_output_unchanged: bool` (single bool MVP — multi-output CA rare; per-output map = followup). (T3) Counter `rio_scheduler_ca_hash_compares_total{outcome=match|miss}`. **NO propagation yet** — that's P0252. (T4) Unit: CA drv completes with hash matching content_index fixture → counter `{outcome=match}` +1. |
| **Primary files** | MODIFY `rio-scheduler/src/actor/completion.rs` (~30 lines after P0228's anchor); MODIFY `rio-scheduler/src/lib.rs` (counter register); NEW test in `rio-scheduler/src/actor/tests/completion.rs` |
| **Deps** | `P0250` + `file:[P0228]` (4c — ONLY 4c completion.rs touch) |
| **Tracey** | `r[impl sched.ca.cutoff-compare]` at completion.rs. `r[verify]` at tests/completion.rs. |
| **Serialization** | **completion.rs count=20+ — serial after P0228. ONLY phase5 completion.rs touch before P0253 (resolution).** Use 4c R12 pattern: `git log -p` for anchor. |
| **Exit** | `/nbr .#ci` green. |

**P0252 — CA cutoff PROPAGATE: state-machine variant + dag skip**

| | |
|---|---|
| **MVP Tier** | **T0 — CA spine hop 3** |
| **Tasks** | (T1) Per Q2 default: `derivation.rs` add `DerivationStatus::CompletedViaCutoff` terminal variant; extend `is_terminal()` + `validate_transition()` to allow `Queued → CompletedViaCutoff`. **AUDIT: every `match status` site — state-machine change ripples** (R1). (T2) `dag/mod.rs` NEW fn `find_cutoff_eligible(completed_ca_drv) -> Vec<DrvHash>`: walks downstream like `find_newly_ready()` but returns derivations where (a) ONLY remaining incomplete dep was the CA drv, (b) that dep is `ca_output_unchanged`. (T3) `completion.rs`: after P0251's compare, IF unchanged → call `find_cutoff_eligible` → transition returned drvs to `CompletedViaCutoff` → **recurse** (cutoff cascades). (T4) Guard: recursion depth cap 1000 (pathological DAG). (T5) Recovery consideration: `CompletedViaCutoff` persists to db; leader failover must NOT re-queue these (add to `db.rs` recovery query WHERE clause). |
| **Primary files** | MODIFY `rio-scheduler/src/state/derivation.rs` (enum + transition + **every match site**); MODIFY `rio-scheduler/src/dag/mod.rs` (new fn); MODIFY `rio-scheduler/src/actor/completion.rs` (propagate call); MODIFY `rio-scheduler/src/db.rs` (recovery WHERE); MODIFY `rio-scheduler/src/dag/tests.rs` |
| **Deps** | `P0251` |
| **Tracey** | `r[impl sched.ca.cutoff-propagate]` at dag/mod.rs + completion.rs. `r[verify]` at dag/tests.rs (unit). |
| **Serialization** | `completion.rs` serial after P0251 (dep enforces). `dag/mod.rs` low collision. `derivation.rs` low BUT state-machine ripple — **DISPATCH SOLO, no concurrent actor/ plans** (4c R4 pattern). `db.rs` serial after P0250 (dep enforces). |
| **Exit** | `/nbr .#ci` green. Unit: 3-node chain A→B→C, A CA-unchanged → B+C transition to `CompletedViaCutoff` without running. `rg 'DerivationStatus::' rio-scheduler/src/ | wc -l` = same count before+after (no missed match arms — exhaustiveness check). |

**P0254 — CA cutoff metrics + VM scenario (milestone demo)**

| | |
|---|---|
| **MVP Tier** | **T0 — CA spine closeout (MILESTONE CLAUSE 1)** |
| **Tasks** | (T1) Metric `rio_scheduler_ca_cutoff_saves_total` (derivations skipped) + gauge `rio_scheduler_ca_cutoff_seconds_saved` (sum of `ema_duration_secs` of skipped). Register in `lib.rs`. (T2) NEW `nix/tests/scenarios/ca-cutoff.nix`: submit CA derivation chain (pattern from `nix/tests/lib/derivations/chain.nix` + `__contentAddressed = true;`), complete once → resubmit identical → assert `ca_cutoff_saves_total > 0` AND second submit completes in <5s (vs ~30s normal). (T3) Register in `nix/tests/default.nix`. (T4) `observability.md` + `scheduler.md` doc updates. (T5) Close `data-flows.md:58` deferral block. |
| **Primary files** | MODIFY `rio-scheduler/src/lib.rs`; NEW `nix/tests/scenarios/ca-cutoff.nix`; MODIFY `nix/tests/default.nix`; MODIFY `docs/src/{observability,components/scheduler,data-flows}.md` |
| **Deps** | `P0252` |
| **Tracey** | `r[verify sched.ca.cutoff-propagate]` at **col-0 header** of `ca-cutoff.nix` (per tracey-adoption: NOT in testScript literal). Closes cross-plan verify from P0252. |
| **Serialization** | `default.nix` SOFT-conflict with P0268 (chaos) — coordinator serializes dispatch, not dag dep (4c A9 pattern). |
| **Exit** | `/nbr .#ci` green including new `vm-ca-cutoff-*`. **MILESTONE CLAUSE 1 DEMONSTRATED.** |

### Tenant enforcement (3 plans — the safety gate, per GT16)

**P0255 — Quota enforcement: reject SubmitBuild over quota (absorbs 4b P0207 T5)**

| | |
|---|---|
| **MVP Tier** | **T0 — SAFETY GATE 1 of 3** |
| **Tasks** | Per Q5 default: **gateway-side** reject. (T1) `rio-gateway/src/handler/build.rs` before submit (near P0213's rate-limit hook): store RPC `tenant_store_bytes(tenant_id)` → compare to `tenants.gc_max_store_bytes` → IF over: `STDERR_ERROR "tenant '{name}' over quota: {used}/{limit} — run GC or request increase"` + early return (DON'T close connection — 4b P0213 pattern). (T2) `estimated_new_bytes = 0` for MVP (reject on CURRENT overflow only, not predictive). (T3) Metric `rio_gateway_quota_rejections_total{tenant}`. (T4) Unit: mock store RPC returning over-quota → assert STDERR_ERROR. VM fragment in `security.nix` TAIL. (T5) Close `multi-tenancy.md:82` deferral block. |
| **Primary files** | MODIFY `rio-gateway/src/handler/build.rs` (~20 lines near P0213's hook); MODIFY `docs/src/multi-tenancy.md:82`; MODIFY `nix/tests/scenarios/security.nix` (TAIL fragment) |
| **Deps** | `P0245` (marker) + `table:[P0206]` (path_tenants exists) + `P0207` (accounting populated — else SUM=0 always passes) + `file:[P0213]` (hook location + STDERR_ERROR pattern) |
| **Tracey** | `r[impl store.gc.tenant-quota-enforce]` at handler. `r[verify]` unit + VM fragment (.nix col-0 header). |
| **Serialization** | **handler/build.rs count=19+ — serial after P0213 (and P0214 if it also touches).** `security.nix` TAIL-append, serial after 4c P0242. |
| **Exit** | `/nbr .#ci` green. `tracey query rule store.gc.tenant-quota-enforce` = impl+verify. |

**P0256 — Per-tenant signing keys + output_hash fix (absorbs opcodes_read.rs:493 TODO per GT5)**

| | |
|---|---|
| **MVP Tier** | **T0 — SAFETY GATE 2 of 3** |
| **Tasks** | (T1) `rio-store/src/signing.rs`: `sign_narinfo(tenant_id: Option<Uuid>, ...)` — lookup active key from `tenant_keys` table (P0249 migration 014), fall back to cluster key if `None` or no tenant key. (T2) NEW `rio-store/src/metadata/tenant_keys.rs` (avoid `db.rs` — HOTTEST): `get_active_signing_key(tenant_id) -> Option<SigningKey>`. (T3) `rio-gateway/src/handler/opcodes_read.rs:493` (absorbed TODO per GT5): fetch real `output_hash` via QueryPathInfo before RegisterRealisation (needed for SIGNING realisations, NOT on cutoff path). (T4) Close `multi-tenancy.md:64` deferral. (T5) Unit: tenant with key → narinfo signed with tenant key; tenant without → cluster key; `nix store verify --trusted-public-keys tenant:<pk>` passes only for that tenant's paths. |
| **Primary files** | MODIFY `rio-store/src/signing.rs`; NEW `rio-store/src/metadata/tenant_keys.rs`; MODIFY `rio-gateway/src/handler/opcodes_read.rs:493`; MODIFY `docs/src/multi-tenancy.md:64` |
| **Deps** | `P0249` (tenant_keys table) + `P0245` |
| **Tracey** | `r[impl store.tenant.sign-key]` at signing.rs. `r[verify]` unit. |
| **Serialization** | `signing.rs` no 4b/4c touch — low collision. `opcodes_read.rs` low collision. **Avoids db.rs entirely** via new file. |
| **Exit** | `/nbr .#ci` green. `rg 'TODO\(phase5\)' rio-gateway/src/handler/opcodes_read.rs` = 0. |

**P0272 — Per-tenant narinfo filtering (absorbs 4b P0207 deferral)**

| | |
|---|---|
| **MVP Tier** | **T0 — SAFETY GATE 3 of 3** |
| **Tasks** | (T1) `rio-store/src/cache_server/` narinfo endpoint: when auth present, `JOIN path_tenants WHERE tenant_id = $auth.tenant_id`. Anonymous → unfiltered (backward compat). (T2) Close `security.md:69` deferral ("per-tenant path visibility"). (T3) Closes 4b P0207's `TODO(phase5)` at `cache_server/auth.rs` (landed with P0207 — verify at dispatch). (T4) VM fragment: tenant-A cannot narinfo tenant-B's path. |
| **Primary files** | MODIFY `rio-store/src/cache_server/auth.rs` (or handlers); MODIFY `docs/src/security.md:69` |
| **Deps** | `P0245` + `P0207` (path_tenants populated + AuthenticatedTenant type) + `table:[P0206]` |
| **Tracey** | `r[impl store.tenant.narinfo-filter]` + `r[verify]`. |
| **Serialization** | `cache_server/` no collision entry. Parallel with P0255/P0256. |
| **Exit** | `/nbr .#ci` green. **WITH P0255+P0256 merged: `introduction.md:50` safety-warning removal unblocked** (P0274 closeout does the removal). |

---

# Phase 4 — Complete Plan Table (remaining 18 plans, tiered T1-T3)

## Step 4.1 — Tier-1 plans (independent lanes, dispatch Wave-1 parallel with CA spine)

**P0253 — CA resolution + dependentRealisations populate (gated on P0247 spike, tier set by A10)**

| | |
|---|---|
| **MVP Tier** | **T0 IF P0247 says CA-on-CA common; T2-CUTTABLE IF rare** (A10 gate) |
| **Tasks** | (T1) `opcodes_read.rs:418` stop discarding `dependentRealisations` on write; `:577/:586` populate on read from storage (migration 015 column per Q3). (T2) NEW `rio-scheduler/src/ca/resolve.rs`: after all CA-input derivations complete, query realisations for input outputs → rewrite `inputDrvs` placeholder paths → produce "resolved" derivation. (T3) `rio-scheduler/src/actor/dispatch.rs`: call resolve before worker assignment. (T4) Unit with mock realisations. |
| **Primary files** | MODIFY `rio-gateway/src/handler/opcodes_read.rs` (3 sites); NEW `rio-scheduler/src/ca/resolve.rs`; MODIFY `rio-scheduler/src/actor/dispatch.rs` |
| **Deps** | `decision:[P0247]` + `P0250` (is_ca flag) + `P0249` (migration 015 IF needed) + `file:[P0230]` (4c dispatch.rs RwLock callsite) + `file:[P0251]` (completion.rs serial — resolution may need to hook near cutoff) |
| **Tracey** | `r[impl sched.ca.resolve]` + `r[verify]`. |
| **Serialization** | `dispatch.rs` serial after 4c P0230. `opcodes_read.rs` low. |
| **Cut** | IF P0247 → T2-cuttable AND schedule slips: `docs/src/components/scheduler.md` gets `> **Phase 6 deferral:** sched.ca.resolve ...` block, marker stays uncovered, P0274 closes with this noted. |

**P0257 — JWT lib: claims + sign/verify (rio-common, mirrors hmac.rs)**

| | |
|---|---|
| **MVP Tier** | **T1 — JWT spine head** |
| **Tasks** | (T1) NEW `rio-common/src/jwt.rs` (pattern from `hmac.rs`): `Claims { sub: Uuid, iat: i64, exp: i64, jti: String }` per `multi-tenancy.md:24-27`. `sign(claims, ed25519_key) -> String`, `verify(token, ed25519_pubkey) -> Result<Claims>`. (T2) `Cargo.toml` add `jsonwebtoken` (only new dep for JWT lane). (T3) Proptest roundtrip. |
| **Primary files** | NEW `rio-common/src/jwt.rs`; MODIFY `rio-common/src/lib.rs` (mod decl); MODIFY `Cargo.toml` |
| **Deps** | `P0245` only — no code-file collision with any 4b/4c plan |
| **Tracey** | `r[impl gw.jwt.claims]` + `r[verify]` proptest. |
| **Serialization** | None. **MAX PARALLEL.** |

**P0258 — JWT issuance at gateway SSH auth**

| | |
|---|---|
| **MVP Tier** | **T1 — JWT spine hop 2** |
| **Tasks** | (T1) `rio-gateway/src/server.rs` (after tenant resolution from SSH-comment, near `:354`): `jwt::sign(Claims { sub: tenant_id_uuid, ... })` using ed25519 K8s Secret (per spec `multi-tenancy.md:28`). Store on SessionContext. (T2) `handler/mod.rs`: inject `x-rio-tenant-token` metadata header on all outbound gRPC calls. |
| **Primary files** | MODIFY `rio-gateway/src/server.rs`; MODIFY `rio-gateway/src/handler/mod.rs` |
| **Deps** | `P0257` |
| **Tracey** | `r[impl gw.jwt.issue]`. |
| **Serialization** | `server.rs` moderate collision — check at dispatch; no known 4c touch. |

**P0259 — JWT verify middleware (scheduler + store + controller)**

| | |
|---|---|
| **MVP Tier** | **T1 — JWT spine hop 3** |
| **Tasks** | (T1) NEW `rio-common/src/jwt_interceptor.rs` (tonic interceptor pattern — see `rio-proto/src/interceptor.rs` for shape): extract `x-rio-tenant-token`, `jwt::verify()`, attach `Claims` to request extensions. Invalid/expired → `Status::unauthenticated`. (T2) Wire in `rio-{scheduler,store,controller}/src/main.rs` server builders. (T3) Per Q4 default: NO `jti` revocation table — `TODO(phase6)` comment. (T4) Close `multi-tenancy.md:19` deferral block + remove `proto.md:252` caveat added by P0245 (GT15). |
| **Primary files** | NEW `rio-common/src/jwt_interceptor.rs`; MODIFY `rio-{scheduler,store,controller}/src/main.rs`; MODIFY `docs/src/{multi-tenancy,components/proto}.md` |
| **Deps** | `P0258` |
| **Tracey** | `r[impl gw.jwt.verify]`. |
| **Serialization** | `scheduler/main.rs` + `store/main.rs` moderate — no 4c touch after P0235 (controller). |

**P0260 — JWT dual-mode config + K8s Secret/ConfigMap + SIGHUP reload**

| | |
|---|---|
| **MVP Tier** | **T1 — JWT spine closeout** |
| **Tasks** | (T1) NEW helm templates `infra/helm/rio-build/templates/jwt-{signing-secret,pubkey-cm}.yaml`. (T2) `rio-common/src/config.rs`: `jwt_required: bool` (false = dual-mode per A5). (T3) Extend `rio-common/src/signal.rs` for SIGHUP pubkey reload (per spec `multi-tenancy.md:31`). (T4) VM fragment in `security.nix`: both SSH-comment and JWT auth resolve to same tenant. Close `integration.md:19` deferral. |
| **Primary files** | NEW `infra/helm/rio-build/templates/jwt-*.yaml`; MODIFY `rio-common/src/{config,signal}.rs`; MODIFY `docs/src/integration.md:19`; MODIFY `nix/tests/scenarios/security.nix` (TAIL) |
| **Deps** | `P0259` |
| **Tracey** | `r[impl gw.jwt.dual-mode]` + `r[verify]` VM fragment (.nix col-0 header). |
| **Serialization** | `security.nix` TAIL-append serial after P0255. |

**P0261 — Governor eviction: assess-then-close OR implement (absorbs 4b P0213 deferral)**

| | |
|---|---|
| **MVP Tier** | **T1 — likely ~30min assess** |
| **Tasks** | Per A6: JWT `sub` = tenant_id UUID = server-resolved = bounded keyspace (cardinality ≤ `SELECT count(*) FROM tenants`). (T1) **Assess**: read P0213's `ratelimit.rs`, confirm rate-limit key = tenant_id not `jti`. (T2a) IF tenant_id (expected): document bounded keyspace in code comment, delete the `TODO(phase5)` P0213 left, close. ~30min. (T2b) IF `jti` or other unbounded key: implement LRU eviction — wrap governor's `DefaultKeyedRateLimiter` in `moka::sync::Cache` with TTL, OR periodic `retain()` sweep dropping keys idle > 10min. ~4h. |
| **Primary files** | MODIFY `rio-gateway/src/ratelimit.rs` (DOES NOT EXIST @ 6b5d4f4 — lands with P0213) |
| **Deps** | `P0260` (JWT claims structure finalized) + `file:[P0213]` (file must exist) |
| **Tracey** | none — either deletes a TODO or adds internal resilience |
| **Serialization** | `ratelimit.rs` is NEW from P0213, zero prior collision. |

**P0262 — PutChunk impl + grace-TTL refcount (closes chunk.rs:62 TODO)**

| | |
|---|---|
| **MVP Tier** | **T1 — Chunk lane head** |
| **Tasks** | (T1) `rio-store/src/grpc/chunk.rs:62`: replace `Status::unimplemented` with real stream handler → `ChunkBackend::put`. Zero proto work (GT6). (T2) `rio-store/src/gc/sweep.rs`: grace-TTL policy — chunk with no manifest reference AND `created_at < now() - grace_seconds` → GC-eligible. `chunks.created_at` exists? Verify at dispatch; add column hunk to P0249 if absent. (T3) Unit: PutChunk → chunk persisted; GC sweep respects grace. |
| **Primary files** | MODIFY `rio-store/src/grpc/chunk.rs`; MODIFY `rio-store/src/gc/sweep.rs` |
| **Deps** | `P0245` only |
| **Tracey** | `r[impl store.chunk.put-standalone]` + `r[impl store.chunk.grace-ttl]` + `r[verify]` unit. |
| **Serialization** | `chunk.rs` no 4b/4c touch. `sweep.rs` serial after 4b P0207 (6th UNION arm) — verify at dispatch. |

**P0263 — Worker client-side chunker**

| | |
|---|---|
| **MVP Tier** | **T1 — Chunk lane hop 2** |
| **Tasks** | (T1) `rio-worker/src/upload.rs` (or NEW `chunker.rs`): before NAR stream → FastCDC chunk locally (same params as `rio-store/src/chunker.rs` — share via `rio-common` OR copy constants) → `FindMissingChunks` → `PutChunk` only missing → PutPath with manifest-only mode. (T2) Metric `rio_worker_chunks_skipped_total`. (T3) VM: upload same NAR twice → second PutChunk count = 0. |
| **Primary files** | MODIFY `rio-worker/src/upload.rs`; POSSIBLY NEW `rio-worker/src/chunker.rs` |
| **Deps** | `P0262` |
| **Tracey** | `r[verify store.chunk.put-standalone]` VM scenario. |
| **Serialization** | `upload.rs` — last major touch was 4a NAR scanner, no 4b/4c. |

**P0267 — Atomic multi-output tx wrap (scope per P0245 GT13 outcome)**

| | |
|---|---|
| **MVP Tier** | **T1 — correctness, small** |
| **Tasks** | **Scope gated on P0245's GT13-verify outcome.** (T1a) IF bug real: `rio-store/src/cas.rs` or `grpc/put_path.rs` — wrap multi-output registration loop in `sqlx::Transaction`. NOTE: tx covers DB rows only; blob-store writes are NOT rolled back by this (R10) — orphaned blobs are GC-eligible, document this. Fault-inject test: mid-loop failure → zero rows committed. (T1b) IF single-statement atomic: tracey-annotate existing code (4c P0226 GT2 pattern). |
| **Primary files** | MODIFY `rio-store/src/cas.rs` OR `grpc/put_path.rs` |
| **Deps** | `gt:[P0245.GT13]` + `file:[P0208]` (4b cas.rs xmax refactor) |
| **Tracey** | `r[impl store.atomic.multi-output]` + `r[verify]` (fault-inject IF (a), annotation IF (b)). |
| **Serialization** | `cas.rs` count=10 — serial after P0208. |

**P0268 — Chaos harness: toxiproxy VM fixture + 2 scenarios (per Q6 default)**

| | |
|---|---|
| **MVP Tier** | **T1 — disjoint, CUTTABLE** |
| **Tasks** | (T1) NEW `nix/tests/fixtures/toxiproxy.nix` (pull `shopify/toxiproxy` image in `nix/docker-pulled.nix`). (T2) NEW `nix/tests/scenarios/chaos.nix`: scenario (a) scheduler↔store latency 500ms → builds complete (retry paths work); scenario (b) worker↔store reset mid-PutPath → upload retries succeed (exercises P0267 fix). (T3) Register in `default.nix`. |
| **Primary files** | NEW `nix/tests/fixtures/toxiproxy.nix`; NEW `nix/tests/scenarios/chaos.nix`; MODIFY `nix/tests/default.nix`; MODIFY `nix/docker-pulled.nix` |
| **Deps** | `P0245` only — **MAX PARALLEL, zero code-file collision** |
| **Tracey** | none (test infra) |
| **Serialization** | `default.nix` SOFT-conflict with P0254 — coordinator serializes dispatch. |

**P0269 — fuse/ops.rs is_file guard (trivial TODO close)**

| | |
|---|---|
| **MVP Tier** | **T1 — trivial, 10 lines** |
| **Tasks** | `rio-worker/src/fuse/ops.rs:361`: add `file.metadata()?.is_file()` guard before `open_backing`, skip passthrough for directories. Noise reduction. |
| **Primary files** | MODIFY `rio-worker/src/fuse/ops.rs:361` |
| **Deps** | `P0245` only |
| **Tracey** | none |
| **Serialization** | None. |

## Step 4.2 — Tier-2 plans (spine tails + absorbed 4c deferrals)

**P0270 — BuildStatus criticalPathRemaining+workers fields (absorbs 4c A3/P0238 deferral)**
- Files: MODIFY `rio-proto/proto/admin.proto` (2 fields EOF-append — `admin.proto` count=2, LOW); MODIFY `rio-controller/src/reconcilers/build.rs` (populate from BuildEvent — 4c A3 says "if BuildEvent carries worker IDs, trivial"; verify `grep worker_id rio-proto/proto/types.proto` at dispatch)
- Deps: `P0245` + `P0238` (4c conditions impl exists)
- Serialization: `reconcilers/build.rs` serial after P0238. `admin.proto` low — parallel with P0271.
- MVP Tier: **T2 — absorbed deferral, small**

**P0271 — Cursor pagination at admin/builds.rs:22 (absorbs 4c A4/GT14)**
- Files: MODIFY `rio-proto/proto/admin.proto` (`optional string cursor` + `repeated` truncation); MODIFY `rio-scheduler/src/admin/builds.rs:22` (keyset `WHERE submitted_at < $cursor ORDER BY submitted_at DESC LIMIT $n`)
- Deps: `P0245` + `P0244` (retag landed — verify `grep 'TODO(phase5)' rio-scheduler/src/admin/builds.rs` shows retag)
- Serialization: `admin.proto` low — coordinate with P0270 (both EOF-append, merge clean).
- MVP Tier: **T2**

**P0264 — FindMissingChunks per-tenant scoping (CUTTABLE per phase5.md:23)**
- Files: NEW `migrations/016_chunks_tenant_id.sql` (renumber at dispatch); MODIFY `rio-store/src/grpc/chunk.rs` (`WHERE tenant_id`); MODIFY `rio-store/src/metadata/chunked.rs`
- Deps: `P0263` + `P0259` (need `Claims.sub` from JWT interceptor to scope by)
- **ZERO downstream deps. CUTTABLE** — phase5.md:23 "optional, at the cost of dedup savings." Coordinator cuts on schedule slip.
- MVP Tier: **T3**

## Step 4.3 — Tier-3 plans (design-gated on P0246 spike)

**P0265 — Worker ResourceUsage mid-build emission (GATED on P0246 decision)**
- Files: MODIFY `rio-worker/src/runtime.rs` (emit `ProgressUpdate{ resources: Some(cgroup_sample) }` every 10s during build — proto ALREADY carries the field per GT7)
- Deps: `decision:[P0246]`
- Tracey: `r[impl sched.preempt.oom-migrate]` (partial — worker half)
- **IF P0246 → (a) queue-only: THIS PLAN'S SCOPE CHANGES** — worker emits Heartbeat with aggregate mem, not ProgressUpdate per-build. Simpler.

**P0266 — Scheduler OOM-detect + redispatch (GATED on P0246)**
- Files: MODIFY `rio-scheduler/src/actor/mod.rs` (consume ProgressUpdate.resources, threshold per P0246 ADR, CancelBuild + re-dispatch on larger size class, max 1 migration per build)
- Deps: `P0265`
- Tracey: `r[verify sched.preempt.oom-migrate]` VM scenario
- Serialization: **actor/mod.rs count=31 #2 HOTTEST — DISPATCH SOLO, serial after 4c P0230+P0231**
- **IF P0246 → (a): becomes queue-reprioritize (no kill) — moves work from `actor/mod.rs` to `assignment.rs` — MUCH smaller, lower collision**

## Step 4.4 — Tracking + Closeout

**P0273 — Dashboard tracking plan (defers to sub-roadmap)**
- Files: NEW `.claude/notes/dashboard-partition.md` (skeleton: tonic-web vs Envoy decision, TS stack, 4 view tasks from `phase5.md:31-34`); OPTIONAL: enable `tonic-web` layer on scheduler admin server (small, ~10 lines in `main.rs`, unblocks sub-roadmap)
- Deps: `P0270, P0271` (dashboard consumes these fields)
- MVP Tier: **T3 — TRACKING, CUTTABLE**

**P0274 — Closeout: doc-sync sweep + introduction.md:50 removal + TODO retags + phase [x]**

| | |
|---|---|
| **Tasks** | (T1) **Remove `introduction.md:50` warning** — THE production-gate milestone (per GT16). Precondition: P0255+P0256+P0272 merged. (T2) Sweep residual `> Phase 5 deferral:` blocks — grep content-string NOT line number (4c R pattern). Expected closures: `data-flows.md:58` (P0254), `integration.md:19` (P0260), `multi-tenancy.md:19,64,82` (P0259,P0256,P0255), `security.md:69` (P0272). (T3) `cgroup.rs:301` retag `TODO(phase5)` → `TODO(adr-012)` per A9. (T4) phase5.md all `[ ]` → `[x]`. (T5) IF P0253/P0264/P0265/P0266 cut: `scheduler.md` gets `> Phase 6 deferral:` blocks for uncovered markers. |
| **Primary files** | MODIFY `docs/src/introduction.md:50`; ~8 docs files; MODIFY `rio-worker/src/cgroup.rs:301`; MODIFY `docs/src/phases/phase5.md` |
| **Deps** | ALL leaf-tips (P0254, P0255, P0256, P0260, P0261, P0263, P0267, P0268, P0269, P0270, P0271, P0272; + P0253/P0264/P0266 if not cut; + P0273) |
| **Exit** | `/nbr .#ci` green. `tracey query validate` = 0. `rg 'TODO\(phase5\)' rio-*/src/` = 0 (or only `adr-012` retag). `rg 'unsafe before Phase 5' docs/` = 0. |

---

# Phase 5 — §4-§8 Supplementary Sections

## Step 5.1 — §4 Dependency Graph

```
WAVE 0 (serial + parallel spikes):
  P0244 → P0245 ──┬──→ P0246 (preempt ADR) ─decision─→ Lane E
         (root)   └──→ P0247 (CA ADR)      ─decision─→ P0249 015-content + P0253 tier

WAVE 1 (hot-file prologue, serial):
  P0245 → P0248 (proto, serial:P0231) → P0250 (CA detect, serial:P0229)
  P0247 → P0249 (migrations, serial:P0227) ─┘

WAVE 2 (max-parallel lanes, after Wave-1 merges):
  CA:      P0250 → P0251(serial:P0228) → P0252(SOLO) → P0254    [4 hops]
  Tenant:  P0255(serial:P0213) ‖ P0256 ‖ P0272                   [1 hop each, parallel]
  JWT:     P0257 → P0258 → P0259 → P0260 → P0261(serial:P0213)   [5 hops]
  Chunk:   P0262 → P0263 → P0264(CUT?)                            [3 hops]
  Preempt: P0265(gate:P0246) → P0266(SOLO:actor/mod,serial:P0231) [2 hops]
  Single:  P0267(serial:P0208) ‖ P0268 ‖ P0269 ‖ P0270 ‖ P0271

  ALL → P0274 (closeout)
```

**Critical path:** `P0245 → P0248 → P0250 → P0251 → P0252 → P0254 → P0274` = **7 hops** (~14 days @ 2 days/hop). But the **safety-gate path** is shorter: `P0245 → P0249 → P0256 ‖ (P0255, P0272) → P0274` = **4 hops** — warning can be removed before CA ships.

**Widest tier:** Wave-2 frontier after P0249+P0250 merge = P0251, P0255, P0256, P0257, P0262, P0267, P0268, P0269, P0270, P0271, P0272 + P0246/P0247 still in flight = **~11 plans**. Exceeds ≤10 cap — coordinator prioritizes by unblock-count: P0251 (unblocks 3), P0257 (unblocks 4), P0255/P0256/P0272 (safety gate).

## Step 5.2 — §5 Tracey Marker Distribution (15 markers, all seeded by P0245)

| Marker | Spec ¶ location | `r[impl]` plan | `r[verify]` plan | Cross-plan? |
|---|---|---|---|---|
| `sched.ca.detect` | scheduler.md (P0245) | P0250 | P0250 | No |
| `sched.ca.cutoff-compare` | scheduler.md | P0251 | P0251 | No |
| `sched.ca.cutoff-propagate` | scheduler.md | P0252 | P0252 unit + **P0254 VM** | **YES — transient `tracey untested` between P0252/P0254 merges. EXPECTED** (4c §5 pattern). P0252 plan doc notes this. |
| `sched.ca.resolve` | scheduler.md | P0253 | P0253 | No — **IF P0253 CUT per A10: `> Phase 6 deferral:` block in scheduler.md, marker stays uncovered** |
| `sched.preempt.oom-migrate` | scheduler.md sibling to `never-running` | P0265+P0266 | P0266 | **IF P0246 → (a) queue-only: marker may be deleted or renamed `sched.preempt.mem-reprioritize`** |
| `store.gc.tenant-quota-enforce` | store.md sibling to existing `tenant-quota` | P0255 | P0255 | No |
| `store.tenant.sign-key` | store.md | P0256 | P0256 | No |
| `store.tenant.narinfo-filter` | store.md | P0272 | P0272 | No |
| `store.chunk.put-standalone` | store.md sibling to `refcount-txn` | P0262 | P0262 unit + **P0263 VM** | YES — transient |
| `store.chunk.grace-ttl` | store.md sibling to `refcount-txn` | P0262 | P0262 | No |
| `store.atomic.multi-output` | store.md | P0267 | P0267 | No |
| `gw.jwt.claims` | gateway.md | P0257 | P0257 | No |
| `gw.jwt.issue` | gateway.md | P0258 | P0258 | No |
| `gw.jwt.verify` | gateway.md | P0259 | P0259 | No |
| `gw.jwt.dual-mode` | gateway.md (does NOT bump `gw.auth.tenant-from-key-comment`) | P0260 | P0260 VM | No |

**.nix annotation:** P0254 `ca-cutoff.nix`, P0255/P0260 `security.nix`, P0263 chunk VM — `r[verify]` at **col-0 BEFORE `{`**, NOT in testScript literal (tracey-adoption memory).

## Step 5.3 — §6 Risks & Mitigations

| # | Risk | Plan(s) | Mitigation |
|---|---|---|---|
| **R1** | **State-machine ripple** — `CompletedViaCutoff` new enum variant requires updating EVERY `match DerivationStatus` site (exhaustiveness). Missing one = silent logic bug. | P0252 | Exit criterion: `rg 'DerivationStatus::' rio-scheduler/src/ | wc -l` before/after = same (compiler caught all). PLUS recovery query review (T5). **DISPATCH SOLO.** |
| **R2** | **Preemption violates spec silently** — if P0246 spike is skipped and P0266 implements OOM-kill without noticing `r[sched.preempt.never-running]`, tracey-validate PASSES (no broken ref) but semantic contract is violated. | P0246, P0266 | P0246 is **NOT OPTIONAL** — it's a dep on P0265/P0266. Coordinator must NOT dispatch Lane E until P0246 followup written. |
| **R3** | **dependentRealisations schema wrong** — JSONB vs junction committed before P0247 wire-capture → sqlx checksum-lock → permanent migration cruft to fix. | P0249, P0253 | P0249 migration 015 gated on `decision:[P0247]`. If spike inconclusive, ship 014 only; 015 lands with P0253. |
| **R4** | **Migration numbering collision** — 4b P0206=012, 4c P0227=013 are NOT YET MERGED @ 6b5d4f4 (tree shows 011 last). If 4b/4c renumber, Phase 5 renumbers too. | P0249 | `ls migrations/ | sort -V | tail -1` in exit criteria. Plan doc says "use next-free, document actual number in commit." |
| **R5** | **CA coupling gamble wrong** — A10 decouples resolution (P0253) as T2-cuttable, but if CA-on-CA is common (stdenv bootstrapping uses it), cutoff without resolution produces zero saves → milestone demo (P0254) shows `ca_cutoff_saves_total = 0` → misleading "feature shipped." | P0247, P0253, P0254 | P0247 spike T4 samples CA-on-CA frequency. IF >30% → A10 overridden, P0253 becomes T0, P0254 VM scenario uses a CA chain. P0254 plan doc includes: "IF `ca_cutoff_saves_total = 0` in VM, do NOT mark milestone clause 1 complete — investigate resolution dependency." |
| **R6** | **has_ca_floating_outputs covers floating NOT fixed** — `mod.rs:222` detects `hash_algo set + hash empty`. Fixed-output CA (`hash` also set) is the OTHER CA kind. Cutoff may need both. | P0250 | Plan doc cites `mod.rs:218-226` (both `is_fixed_output` and `has_ca_floating_outputs`) — `is_ca = is_fixed_output() || has_ca_floating_outputs()`. Verify in P0247 spike which actually appear in real nixpkgs. |
| **R7** | **Gateway quota check adds store RPC to hot path** (per Q5 default) — every SubmitBuild now round-trips to store before proceeding. Latency. | P0255 | Cache `tenant_store_bytes` result in gateway with short TTL (30s). Quota is soft — a few MB of race-window overflow is fine. Document as "eventually-enforcing." |
| **R8** | **types.proto conflict with 4c P0231** — HUB plan, may still be in flight when P0248 wants to dispatch. | P0248 | Hard `file:[P0231]` dep. EOF-append only. Single 3-line change — smallest possible proto touch. |
| **R9** | **completion.rs anchor moved** — 4b P0206+P0219 + 4c P0228 all touch it. Line :298 is stale. | P0251 | Plan doc: "`git log -p --since='2 months' -- rio-scheduler/src/actor/completion.rs` at dispatch to find current EMA call site" (4c R12 pattern). |
| **R10** | **Atomic tx doesn't cover blob store** — `sqlx::Transaction` rolls back DB rows but PutPath also writes NAR blobs to object store. Blob orphans possible on rollback. | P0267 | Plan doc documents this explicitly: "tx = DB atomicity only. Orphaned blobs are refcount-zero, GC-eligible next sweep. Document bound: ≤ 1 NAR-size per failure." Good enough for correctness (no inconsistent `nix-store -q --outputs`); blob cleanup is best-effort. |
| **R11** | **JWT interceptor wiring in 3 main.rs files** — scheduler+store+controller all get the same change. Easy to miss one; unauth'd backdoor. | P0259 | Exit criterion includes: `rg 'jwt_interceptor' rio-{scheduler,store,controller}/src/main.rs | wc -l` = 3. VM test in P0260 hits all three services. |
| **R12** | **actor/mod.rs 31-collision P0266 conflict** — hottest scheduler file, P0252 also touches adjacent (`completion.rs` calls into actor). | P0266 | **DISPATCH SOLO.** Serial after 4c P0230+P0231. IF P0246 → (a) queue-only, work moves to `assignment.rs` (count=7) — MUCH safer. |
| **R13** | **sqlx compile fail** if P0249 doesn't commit `.sqlx/` — P0250+P0256 queries fail. | P0249 | `.sqlx/` commit in exit criteria (4c R13 pattern). |
| **R14** | **CA cutoff recursion unbounded** — cascade through deep DAG (nixpkgs stdenv bootstrap = 100s deep). | P0252 | Depth cap 1000 (T4). IF cap hit: log WARN + counter, stop cascading, remaining stay Queued (safe — just suboptimal). |
| **R15** | **Governor doesn't exist yet** — P0261 against file P0213 creates. If P0213 dropped or restructured, P0261 targets nothing. | P0261 | `file:[P0213]` is a hard dep — frontier computation won't surface P0261 until ratelimit.rs exists. IF P0213 dropped entirely → P0261 becomes "add governor WITH bounded key from day 1," deps shift to P0257. |
| **R16** | **introduction.md:50 removed prematurely** — closeout deletes warning before all three safety-gate plans actually work end-to-end. | P0274 | Closeout T1 precondition is explicit: "P0255+P0256+P0272 merged AND their VM tests green in the last `.#ci` run." Not just "dag says DONE." |

## Step 5.4 — §7 Coordinator Dispatch + Exit Gates

```bash
cd /root/src/rio-build/main

# After P0244 (4c closeout) merged — Wave 0
python3 .claude/lib/state.py dag-append '{"plan":245,"title":"Prologue: phase5.md corrections + 15 markers + GT-verify","deps":[244],"crate":"docs","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":246,"title":"SPIKE: preemption ADR (DECISION for Lane E)","deps":[245],"crate":"docs","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":247,"title":"SPIKE: CA wire-capture + schema ADR (DECISION)","deps":[245],"crate":"docs","status":"UNIMPL","complexity":"MED"}'

# Wave 1 — hot-file prologue (gated on spikes where noted)
python3 .claude/lib/state.py dag-append '{"plan":248,"title":"types.proto: is_ca field (ONLY phase5 types.proto touch)","deps":[245,231],"crate":"rio-proto","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":249,"title":"Migration 014 tenant_keys + cond. 015 realisations_deps","deps":[247,227],"crate":"migrations","status":"UNIMPL","complexity":"LOW"}'

# CA spine (T0)
python3 .claude/lib/state.py dag-append '{"plan":250,"title":"CA detect: plumb is_ca flag (GT4 shrink)","deps":[248,249,229],"crate":"rio-gateway,rio-scheduler","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":251,"title":"CA cutoff COMPARE: completion.rs hash-check","deps":[250,228],"crate":"rio-scheduler","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":252,"title":"CA cutoff PROPAGATE: state variant + dag skip (SOLO)","deps":[251],"crate":"rio-scheduler","status":"UNIMPL","complexity":"HIGH"}'
python3 .claude/lib/state.py dag-append '{"plan":253,"title":"CA resolution + dependentRealisations populate (tier=A10)","deps":[247,250,249,230,251],"crate":"rio-gateway,rio-scheduler","status":"UNIMPL","complexity":"HIGH"}'
python3 .claude/lib/state.py dag-append '{"plan":254,"title":"CA metrics + VM scenario (MILESTONE demo)","deps":[252],"crate":"rio-scheduler,nix/tests","status":"UNIMPL","complexity":"MED"}'

# Tenant enforcement (T0 — safety gate)
python3 .claude/lib/state.py dag-append '{"plan":255,"title":"Quota reject SubmitBuild (absorbs P0207 T5)","deps":[245,206,207,213],"crate":"rio-gateway","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":256,"title":"Per-tenant signing keys + output_hash fix (absorbs :493)","deps":[249,245],"crate":"rio-store,rio-gateway","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":272,"title":"Per-tenant narinfo filter (absorbs P0207)","deps":[245,207,206],"crate":"rio-store","status":"UNIMPL","complexity":"LOW"}'

# JWT spine (T1)
python3 .claude/lib/state.py dag-append '{"plan":257,"title":"JWT lib: claims + sign/verify","deps":[245],"crate":"rio-common","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":258,"title":"JWT issuance at gateway","deps":[257],"crate":"rio-gateway","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":259,"title":"JWT verify middleware (3 services)","deps":[258],"crate":"rio-common,rio-scheduler,rio-store,rio-controller","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":260,"title":"JWT dual-mode config + K8s plumbing + SIGHUP","deps":[259],"crate":"rio-common,infra","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":261,"title":"Governor eviction: assess-then-close (absorbs P0213)","deps":[260,213],"crate":"rio-gateway","status":"UNIMPL","complexity":"LOW"}'

# Chunk lane (T1)
python3 .claude/lib/state.py dag-append '{"plan":262,"title":"PutChunk impl + grace-TTL (closes chunk.rs:62)","deps":[245],"crate":"rio-store","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":263,"title":"Worker client-side chunker","deps":[262],"crate":"rio-worker","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":264,"title":"FindMissingChunks tenant scope (CUTTABLE)","deps":[263,259],"crate":"rio-store,migrations","status":"UNIMPL","complexity":"LOW"}'

# Preempt lane (T3 — gated on P0246)
python3 .claude/lib/state.py dag-append '{"plan":265,"title":"Worker ResourceUsage mid-build emit (gate:P0246)","deps":[246],"crate":"rio-worker","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":266,"title":"Scheduler OOM detect + redispatch (SOLO actor/mod)","deps":[265,230,231],"crate":"rio-scheduler","status":"UNIMPL","complexity":"HIGH"}'

# Singletons (T1)
python3 .claude/lib/state.py dag-append '{"plan":267,"title":"Atomic multi-output tx (scope per GT13)","deps":[245,208],"crate":"rio-store","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":268,"title":"Chaos harness: toxiproxy + 2 scenarios","deps":[245],"crate":"nix/tests","status":"UNIMPL","complexity":"MED"}'
python3 .claude/lib/state.py dag-append '{"plan":269,"title":"fuse/ops.rs is_file guard (trivial)","deps":[245],"crate":"rio-worker","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":270,"title":"BuildStatus critPath+workers (absorbs 4c A3)","deps":[245,238],"crate":"rio-proto,rio-controller","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":271,"title":"Cursor pagination (absorbs 4c A4)","deps":[245,244],"crate":"rio-proto,rio-scheduler","status":"UNIMPL","complexity":"LOW"}'

# Tracking + Closeout
python3 .claude/lib/state.py dag-append '{"plan":273,"title":"Dashboard tracking + sub-roadmap","deps":[270,271],"crate":"docs","status":"UNIMPL","complexity":"LOW"}'
python3 .claude/lib/state.py dag-append '{"plan":274,"title":"Closeout: intro.md:50 removal + retags + phase [x]","deps":[254,255,256,260,261,263,267,268,269,270,271,272,273],"crate":"docs","status":"UNIMPL","complexity":"LOW"}'

python3 .claude/lib/state.py collisions-regen
python3 .claude/lib/state.py dag-render | grep 'UNIMPL.*deps-met'
# Expect: P0245 only (frontier root until it merges)
```

**Phase exit gate (P0274's final check):**
```bash
NIXBUILDNET_REUSE_BUILD_FAILURES=false nix-build-remote --no-nom -- -L .#ci
nix develop -c tracey query validate    # 0 errors, 15 new markers all resolved (or Phase-6-deferred)
nix develop -c tracey query status      # sched.ca.cutoff-propagate may show transient untested between P0252/P0254 — fine
rg 'TODO\(phase5' rio-*/src/            # empty (only adr-012 retag survives)
rg 'Phase 5 deferral' docs/src/         # empty (or only Phase-6-deferred for cut plans)
rg 'unsafe before Phase 5' docs/src/    # EMPTY — THE milestone
```

## Step 5.5 — §8 Hidden Dependencies (implementer checks at dispatch, NOT in dag)

| Plan | Check | Command |
|---|---|---|
| P0248 | types.proto EOF clean after 4c P0231 | `git log -p --since='2 months' -- rio-proto/proto/types.proto \| head -50` |
| P0249 | Migration 013 is last | `ls migrations/ \| sort -V \| tail -1` → expect 013, use 014 |
| P0249 | P0247 spike outcome recorded | `jq 'select(.origin=="P0247")' .claude/state/followups-pending.jsonl` |
| P0250 | db.rs anchor after 4c P0229 | `git log -p --since='2 months' -- rio-scheduler/src/db.rs \| head -80` |
| P0251 | completion.rs EMA anchor moved by P0228 | `git log -p -- rio-scheduler/src/actor/completion.rs \| head -100` — do NOT trust :298 |
| P0252 | Every DerivationStatus match site | `rg -n 'DerivationStatus::' rio-scheduler/src/ \| wc -l` — record count, compiler enforces exhaustiveness but check for `_ =>` wildcards |
| P0253 | P0247 says `dependentRealisations` storage needed | `grep 'dependent_storage' .claude/state/followups-pending.jsonl` |
| P0255 | P0213's rate-limit hook location | `grep -n 'limiter.check\|STDERR_ERROR' rio-gateway/src/handler/build.rs` → quota check adjacent |
| P0255 | `tenant_store_bytes` fn from P0207 | `grep -rn tenant_store_bytes rio-store/src/` — if absent, write own query |
| P0256 | `tenant_keys` table exists | `psql -c '\d tenant_keys'` in test DB OR `grep tenant_keys migrations/014*.sql` |
| P0261 | `ratelimit.rs` exists + has TODO | `test -f rio-gateway/src/ratelimit.rs && grep 'TODO(phase5)' $_` |
| P0262 | `chunks.created_at` column exists | `grep 'created_at' migrations/*.sql \| grep chunks` — add column hunk to P0249 if absent |
| P0265 | P0246 decision recorded | `jq 'select(.origin=="P0246")' .claude/state/followups-pending.jsonl` — scope depends on (a)/(b)/(c) |
| P0266 | actor/mod.rs changed by 4c P0230/P0231 | `git log -p --since='2 months' -- rio-scheduler/src/actor/mod.rs` |
| P0267 | P0245 GT13-verify outcome | `grep 'GT13-OUTCOME' .claude/notes/phase5-partition.md` |
| P0270 | BuildEvent carries worker_id (4c A3) | `grep -A3 'worker_id\|worker_name' rio-proto/proto/types.proto` |
| P0271 | admin/builds.rs:22 retagged by P0244 | `grep 'TODO(phase5)' rio-scheduler/src/admin/builds.rs` → expect present |
| P0272 | P0207's cache_server/auth.rs TODO landed | `grep 'TODO(phase5)' rio-store/src/cache_server/` |
| P0274 | All three safety-gate plans' VM tests green | Check last `.#ci` artifact for `vm-*security*` pass — not just dag DONE |

---

# Phase 6 — Document Assembly + Self-Review + Commit

## Step 6.1 — Assemble at `/root/src/rio-build/main/.claude/notes/phase5-partition.md`

Section order (matches 4c at `71de962` exactly, with §2 as the MVP-first addition):

```
# Phase 5 Partition — FINAL (P0245–P0274, 30 plans)
## §0 — Assumptions & Open Questions (11 A-rows, 6 Q-rows)
## §1 — Ground-Truth Reconciliation (16 GT rows, verified @ 6b5d4f4)
## §2 — MVP Shipping Thresholds (NEW — T0/T1/T2/T3 tier table + safety-gate vs headline-feature distinction)
## §3 — Partition Table (30 plan blocks, grouped by wave)
  ### Wave 0 — Prologue + Spikes (P0245-P0247)
  ### Wave 1 — Hot-file prologue (P0248-P0249)
  ### Wave 2a — CA spine (P0250-P0254)
  ### Wave 2b — Tenant safety gate (P0255, P0256, P0272)
  ### Wave 2c — JWT spine (P0257-P0261)
  ### Wave 2d — Chunk lane (P0262-P0264)
  ### Wave 2e — Preempt lane (P0265-P0266, gated)
  ### Wave 2f — Singletons (P0267-P0271)
  ### Wave 3 — Tracking + Closeout (P0273-P0274)
## §4 — Dependency Graph (ASCII + critical path + topo tiers + dispatch priority)
## §5 — Tracey Marker Distribution (15 markers)
## §6 — Risks & Mitigations (16 rows)
## §7 — Coordinator Dispatch + Exit Gates (dag-append commands)
## §8 — Hidden Dependencies (implementer checks at dispatch)
## Summary table (for rio-planner expansion)
## Files referenced
```

Target length: ~750 lines (4c was 661L for 25 plans; 30 plans + larger §0/§1 → ~750).

## Step 6.2 — Self-review checklist (run BEFORE commit)

```bash
# Plan count
grep -c '^#### P02' .claude/notes/phase5-partition.md     # expect 30

# All 5 absorbed deferrals present with back-refs
grep -E 'P0207|P0213|4c A3|4c A4|4c GT14' .claude/notes/phase5-partition.md | wc -l  # expect ≥5

# All 4 in-tree TODO(phase5) addressed
grep -E 'opcodes_read.rs:493|chunk.rs:62|fuse/ops.rs:361|cgroup.rs:301' .claude/notes/phase5-partition.md  # each appears ≥1

# Every hot-file touch has serialization note
grep -B2 'types.proto\|completion.rs\|db.rs\|handler/build.rs\|actor/mod.rs\|cas.rs' .claude/notes/phase5-partition.md | grep -i 'serial'
# visual check — every hot-file mention has adjacent serial:[Pxxxx] note

# All deps are concrete plan numbers (no phase labels)
grep -oE '"deps":\[[^]]*\]' .claude/notes/phase5-partition.md | grep -vE '[0-9]+'  # expect empty (all numeric)

# No Future Work items leaked in
grep -iE 'work.stealing|speculative|staggered|multi.version|cargo.mutants' .claude/notes/phase5-partition.md  # expect empty

# P0264 has zero downstream (cuttable)
grep -E '"deps":\[.*264' .claude/notes/phase5-partition.md  # expect empty (nothing deps on 264)

# Tracey markers domain-indexed
grep -oE 'r\[[a-z.]+-?[a-z]*\]' .claude/notes/phase5-partition.md | sort -u  # visual — all sched.*/store.*/gw.*

# Spec conflict (GT9) explicitly addressed
grep 'never-running' .claude/notes/phase5-partition.md | wc -l  # expect ≥3 (GT9, Q1, P0246, R2)
```

## Step 6.3 — Validate against goals checklist

- [ ] 30 plans, within 20-30 ceiling ✓
- [ ] P0245 is wave-0 frontier root, seeds 15 markers, zero r[impl]/r[verify] ✓
- [ ] Two spike plans (P0246, P0247) de-risk highest-blast-radius unknowns ✓
- [ ] ≥4 parallel lanes after Wave-1: CA, Tenant, JWT, Chunk, Preempt, Singletons = 6 lanes ✓
- [ ] Every hot-file touch has serialization predecessor note ✓
- [ ] All 5 absorbed deferrals: P0213→P0261, P0207-filter→P0272, P0207-quota→P0255, 4c-A3→P0270, 4c-A4→P0271 ✓
- [ ] All 4 in-tree TODOs: `:493`→P0256, `chunk:62`→P0262, `fuse:361`→P0269, `cgroup:301`→P0274 retag ✓
- [ ] GT1/GT2/GT3 answered + GT9 (spec conflict) + GT2-expansion (dependentRealisations discard) ✓
- [ ] P0264 (FindMissingChunks scoping) standalone, zero downstream, CUTTABLE ✓
- [ ] Deps column: concrete numbers only (206,207,208,213,227,228,229,230,231,238,244 from 4b/4c) ✓
- [ ] Future Work (L38-43) NOT in partition ✓
- [ ] Migration sequence: 014 tenant_keys (P0249) → 015 realisations_deps (conditional) → 016 chunks_tenant_id (IF P0264 ships) — NO collision ✓
- [ ] MVP cut = T0 tier = 10 plans (3 wave-0 + 2 prologue + 4 CA + 3 tenant — wait, that's 12; safety-gate alone = P0245+P0249+P0255+P0256+P0272 = 5 plans) — document BOTH cut boundaries ✓
- [ ] `introduction.md:50` removal is explicit closeout precondition ✓

## Step 6.4 — Commit (notes-only)

```bash
cd /root/src/rio-build/main
git add .claude/notes/phase5-partition.md
git commit -m 'docs(dag): phase5 partition P0245-P0274 (30 plans, spike-gated)'
```

---

# MVP-First Summary (for partition doc §2)

| Tier | Plans | Ships | Cuttable? |
|---|---|---|---|
| **T0-Spikes** | P0245, P0246, P0247 | Ground truth + 2 ADRs | NO — de-risks everything downstream |
| **T0-Safety** | P0249, P0255, P0256, P0272 | `introduction.md:50` warning removal | NO — the production gate |
| **T0-CA** | P0248, P0250, P0251, P0252, P0254 | Milestone clause 1 demo | NO — headline feature |
| **T0/T2-gated** | P0253 (resolution) | CA-on-CA cutoff | Tier set by P0247 spike (A10) |
| **T1-JWT** | P0257-P0261 | Full JWT flow per `multi-tenancy.md:19-31` spec | P0261 likely 30-min assess |
| **T1-Chunk** | P0262, P0263 | Worker-side dedup | No (absorbs chunk.rs:62 TODO) |
| **T1-Single** | P0267, P0268, P0269, P0270, P0271 | Correctness + absorbed deferrals + chaos | P0268 cuttable |
| **T3-Gated** | P0264, P0265, P0266 | Per-tenant chunks + preempt | P0264 explicitly optional; P0265-266 design-gated |
| **T3-Track** | P0273 | Dashboard roadmap | Cuttable |
| **Closeout** | P0274 | Doc sweep + warning removal | NO |

**Minimum safety-gate Phase 5 = 8 plans** (P0245-P0247 + P0249 + P0255 + P0256 + P0272 + P0274).
**Minimum milestone Phase 5 = 13 plans** (safety-gate + CA spine P0248,P0250-P0252,P0254).
**Full Phase 5 = 30 plans. 5 explicitly cuttable (P0253-if-T2, P0264, P0265, P0266, P0268, P0273).**

---

**Files referenced:**
- `/root/src/rio-build/main/docs/src/phases/phase5.md` — 48-line source task list
- `/root/src/rio-build/main/docs/src/components/scheduler.md:241-244` — `r[sched.preempt.never-running]` spec conflict
- `/root/src/rio-build/main/docs/src/multi-tenancy.md:19-31` — JWT spec authority (Phase 5 deferral block)
- `/root/src/rio-build/main/docs/src/introduction.md:50` — the production-gate warning (closeout target)
- `/root/src/rio-build/main/rio-gateway/src/handler/opcodes_read.rs:418,493,577,586` — dependentRealisations discard + output_hash zeros
- `/root/src/rio-build/main/rio-nix/src/derivation/mod.rs:222` — `has_ca_floating_outputs()` (GT4 scope-shrink)
- `/root/src/rio-build/main/rio-proto/proto/types.proto:149-152,338,401,585` — existing proto (GT6-GT7 scope-shrink)
- `/root/src/rio-build/main/migrations/` — ends at 011; 012=4b, 013=4c, Phase5 starts 014
- `git show 71de962:.claude/notes/phase4c-partition.md` — 661-line format reference (§0-§8 structure)
