# Plan 0204: phase4b doc-sync + 12 spec-marker seeding

Wave-0 serial prologue. This plan is the **frontier root for all of phase 4b** — every downstream plan references marker IDs that don't exist until this merges. Doc-only (~minutes of work) plus two one-line TODO retags; zero `r[impl]`/`r[verify]` annotations.

Three jobs: (1) correct [`phase4b.md`](../../docs/src/phases/phase4b.md) stale claims against ground truth (NAR scanner landed in 4a remediation, Karpenter done, rio-cli partial not missing); (2) retag two `TODO(phase4b)` comments to `TODO(phase4c)` where work is gated/blocked; (3) seed **12** new domain markers into `docs/src/components/*.md` so downstream `r[impl]`/`r[verify]` annotations have targets.

The partition's §1 ground-truth table drives the corrections. The critical-path shift: `tenants.gc_retention_hours` (migration [`009_phase4.sql:16`](../../migrations/009_phase4.sql)) is dead data today — `rio-cli create-tenant --gc-retention-hours 720` sets it but [`mark.rs`](../../rio-store/src/gc/mark.rs) never reads it. This replaces NAR-scanner as the correctness front for 4b.

**Assumption A1 accepted:** migration goes in new file `012_path_tenants.sql`, NOT appended to 009. Migrations 010+011 already exist; `sqlx::migrate!` at [`rio-store/src/lib.rs:34`](../../rio-store/src/lib.rs) is checksum-checked. Even a header-comment change to 009 would break every persistent DB. This plan does NOT touch 009 at all — it documents the deviation in `phase4b.md` only.

**Assumption A2 verified:** `tracey query validate` CI check greps for `0 total error(s)`. Uncovered markers are NOT errors — they appear in `tracey query uncovered` (different query). Adding 12 uncovered markers → 167/179 covered, still 0 errors, CI stays green.

## Entry criteria

- [P0203](plan-0203-niks3-upload-pipeline.md) merged (backfill terminal — `phase-4a` tag landed)

## Tasks

### T1 — `docs(phase4b):` correct stale phase4b.md claims against ground truth

In [`docs/src/phases/phase4b.md`](../../docs/src/phases/phase4b.md):

- **Line 13** (NAR scanner): mark `[x]`, ref commit `9165dc2`. Reality: `r[impl worker.upload.references-scanned]` at [`rio-worker/src/upload.rs:150`](../../rio-worker/src/upload.rs) (RefScanSink, Boyer-Moore NOT aho-corasick); `r[verify]` at `:799`; VM assertion at [`lifecycle.nix:1800-1811`](../../nix/tests/scenarios/lifecycle.nix); migrations 010+011 backfill.
- **Line 39** (Karpenter): mark `[x]`. Reality: [`infra/eks/karpenter.tf`](../../infra/eks/karpenter.tf) (79L) + `templates/karpenter.yaml` + `templates/workerpool.yaml` all exist.
- **Line 40** (rio-cli packaging): mark `[x]` — scheduler-image only, strike standalone. Reality: [`nix/docker.nix:177-181`](../../nix/docker.nix) already bundles rio-cli.
- **Line 64** (proposed marker `worker.refs.nar-scan`): strike → note actual marker is `worker.upload.references-scanned` (already impl+verify). Do NOT rename code.
- **Line 22** ("Migration 009 Part C"): update → "Migration 012 (was 009 Part C — 010/011 now exist, sqlx checksum-locked, see A1)".
- **Line 70** (Carried-forward table, `upload.rs:223 Vec::new()`): update → resolved 4a remediation 02 ([P0181](plan-0181-rem02-nar-scanner-gc-gate.md)).

### T2 — `docs(phase4b):` retag two deferred TODOs to phase4c

Two `TODO(phase4b)` comments whose work is gated/blocked — retag to `TODO(phase4c)`:

- [`rio-worker/src/upload.rs:166`](../../rio-worker/src/upload.rs) — trailer-refs. TODO text self-gates: "if pre-scan cost becomes measurable." No measurement exists. One-line edit: `TODO(phase4b)` → `TODO(phase4c)`.
- [`rio-scheduler/src/db.rs:9`](../../rio-scheduler/src/db.rs) — `query!()` macro migration. Blocked on `.sqlx/` in Crane source filter + `cargo sqlx prepare` tooling. Touches every query in a 2017-line file; would serialize with P0206. Zero user-visible benefit. One-line edit: `TODO(phase4b)` → `TODO(phase4c)`.

### T3 — `docs(spec):` seed 12 domain markers in component specs

Add each as a standalone paragraph (col 0, blank line before) in the target file. These are **definitions** — downstream plans annotate code with `r[impl ...]`/`r[verify ...]` pointing here. See `## Spec additions` below for the full marker texts to add.

| Marker | Target file | Near | Implemented by |
|---|---|---|---|
| `r[store.gc.tenant-retention]` | [`store.md`](../../docs/src/components/store.md) | line 194 (expand one-liner to section) | P0207 |
| `r[store.gc.tenant-quota]` | [`store.md`](../../docs/src/components/store.md) | same section | P0207 |
| `r[store.cas.xmax-inserted]` | [`store.md`](../../docs/src/components/store.md) | near `:202` pending-deletes | P0208 |
| `r[sched.gc.path-tenants-upsert]` | [`scheduler.md`](../../docs/src/components/scheduler.md) | near `:88` | P0206 |
| `r[sched.timeout.per-build]` | [`scheduler.md`](../../docs/src/components/scheduler.md) | near `:376` | P0214 |
| `r[sched.retry.per-worker-budget]` | [`scheduler.md`](../../docs/src/components/scheduler.md) | near poison section | P0219 |
| `r[worker.fuse.circuit-breaker]` | [`worker.md`](../../docs/src/components/worker.md) | FUSE section | P0209 |
| `r[worker.heartbeat.store-degraded]` | [`worker.md`](../../docs/src/components/worker.md) | FUSE section | P0210/P0211 |
| `r[worker.silence.timeout-kill]` | [`worker.md`](../../docs/src/components/worker.md) | near `:152` | P0215 |
| `r[gw.rate.per-tenant]` | [`gateway.md`](../../docs/src/components/gateway.md) | new section | P0213 |
| `r[gw.conn.cap]` | [`gateway.md`](../../docs/src/components/gateway.md) | same section | P0213 |
| `r[ctrl.gc.cron-schedule]` | [`controller.md`](../../docs/src/components/controller.md) | new section | P0212 |

### T4 — `docs(spec):` update deferral prose to forward-ref new markers

At [`docs/src/challenges.md:100`](../../docs/src/challenges.md) and [`docs/src/failure-modes.md:52`](../../docs/src/failure-modes.md): change "Phase 4 deferral" → "Phase 4b: see `r[worker.fuse.circuit-breaker]`". This is a **forward-ref, NOT removal** — P0210 deletes the blockquotes entirely once the behavior is implemented.

## Exit criteria

- `/nbr .#ci` green
- `tracey query validate` shows `0 total error(s)` (uncovered ≠ error per A2)
- `tracey query uncovered` shows exactly the 12 new rules listed above (and only those 12 new ones)
- `grep -rc 'TODO(phase4b)' rio-*/src/` totals 5 (down from 7 — two retagged to 4c)
- Zero `r[impl]`/`r[verify]` annotations added by this plan (definitions only)

## Tracey

This plan **defines** 12 new domain markers. It does not **implement** or **verify** any — those are downstream plans.

Adds new markers to component specs:
- `r[store.gc.tenant-retention]` → `docs/src/components/store.md`
- `r[store.gc.tenant-quota]` → `docs/src/components/store.md`
- `r[store.cas.xmax-inserted]` → `docs/src/components/store.md`
- `r[sched.gc.path-tenants-upsert]` → `docs/src/components/scheduler.md`
- `r[sched.timeout.per-build]` → `docs/src/components/scheduler.md`
- `r[sched.retry.per-worker-budget]` → `docs/src/components/scheduler.md`
- `r[worker.fuse.circuit-breaker]` → `docs/src/components/worker.md`
- `r[worker.heartbeat.store-degraded]` → `docs/src/components/worker.md`
- `r[worker.silence.timeout-kill]` → `docs/src/components/worker.md`
- `r[gw.rate.per-tenant]` → `docs/src/components/gateway.md`
- `r[gw.conn.cap]` → `docs/src/components/gateway.md`
- `r[ctrl.gc.cron-schedule]` → `docs/src/components/controller.md`

## Spec additions

Full marker texts to add to the component spec files (standalone paragraph, blank line before, col 0):

---

**`docs/src/components/store.md`** (near line 194, expand the tenant-retention one-liner into a section):

```
r[store.gc.tenant-retention]

A store path survives GC if *any* tenant that has referenced it still
has the path inside its retention window. The mark phase CTE joins
`path_tenants` against `tenants.gc_retention_hours`: `WHERE
pt.first_referenced_at > now() - make_interval(hours =>
t.gc_retention_hours)`. This is union-of-retention semantics — the
most generous tenant wins. The global grace period (`narinfo.created_at`
window) is a floor; tenant retention extends it but never shortens it.

r[store.gc.tenant-quota]

Per-tenant store accounting sums `narinfo.nar_size` over all paths
the tenant has referenced (`JOIN path_tenants USING (store_path_hash)
WHERE tenant_id = $1`). Phase 4b is accounting-only — the query exists
and returns bytes, but no quota enforcement is implemented. Phase 5
adds enforcement (reject PutPath above quota, or trigger tenant-scoped
GC).
```

**`docs/src/components/store.md`** (near line 202, after `store.gc.pending-deletes`):

```
r[store.cas.xmax-inserted]

The chunk-upsert batch INSERT returns per-row `(xmax = 0) AS inserted`
so the caller knows which blake3 hashes are genuinely new (and need
upload to backend) vs which were already present (skip upload). This
replaces the racy re-query: previously `do_upload` re-SELECTed
`refcount` after the upsert, but a concurrent PutPath could bump
refcount between upsert and re-SELECT, causing a chunk to be marked
already-present when it was actually our insert. `xmax = 0` is atomic
with the INSERT itself.
```

---

**`docs/src/components/scheduler.md`** (near line 88):

```
r[sched.gc.path-tenants-upsert]

On build completion, the scheduler upserts `(store_path_hash,
tenant_id)` rows into `path_tenants` for every output path × every
tenant whose build was interested in that derivation (dedup via
`interested_builds`). This is best-effort: upsert failure warns but
does not fail completion — GC may under-retain a path if the upsert
fails, but the build still succeeds. The upsert is `ON CONFLICT DO
NOTHING` (composite PK on `(store_path_hash, tenant_id)`); repeated
builds of the same path by the same tenant are idempotent.
```

**`docs/src/components/scheduler.md`** (near line 376):

```
r[sched.timeout.per-build]

`BuildOptions.build_timeout` (proto field, seconds) is a wall-clock
limit on the *entire* build from submission to completion. In
`handle_tick`, any build with `submitted_at.elapsed() > build_timeout`
has its non-terminal derivations cancelled and transitions to
`Failed { status: TimedOut }`. This is distinct from
`r[sched.backstop.timeout]` (per-derivation heuristic: est×3) and
distinct from the worker-side daemon floor (which also receives
`build_timeout` as a per-derivation `min_nonzero` — defense-in-depth,
NOT the primary semantics). Zero means no overall timeout.
```

**`docs/src/components/scheduler.md`** (near the poison section):

```
r[sched.retry.per-worker-budget]

`DerivationState.per_worker_failures: HashMap<WorkerId, u32>` tracks
retry attempts per worker, separate from `failed_workers: HashSet`
(distinct-worker poison count). `best_worker` excludes any worker with
≥ `MAX_SAME_WORKER_RETRIES` (default 2) failures on the current
derivation. The poison threshold STAYS on `failed_workers.len()`
(distinct workers) — a derivation that fails 3× on the same worker
is NOT poisoned; it's just not retried on *that* worker. This map is
in-memory only (not persisted) — reset on scheduler restart is
acceptable (matches pre-4a poison TTL behavior).
```

---

**`docs/src/components/worker.md`** (near FUSE section):

```
r[worker.fuse.circuit-breaker]

The FUSE fetch path has a circuit breaker: `threshold` (default 5)
consecutive `ensure_cached` failures open the circuit → subsequent
`check()` returns `EIO` immediately (fail-fast, don't stall builds on
a dead store). After `auto_close_after` (default 30s) the circuit goes
half-open: the next `check()` probes — success closes the circuit,
failure re-opens it. **CRITICAL: this is std::sync ONLY** — FUSE
callbacks run on fuser's thread pool, NOT in a tokio context.
`AtomicU32` + `parking_lot::Mutex`; zero `tokio::sync`, zero `.await`.

r[worker.heartbeat.store-degraded]

`HeartbeatRequest.store_degraded` (proto bool, field 9) reflects
`CircuitBreaker::is_open()`. Scheduler treats it like `draining`:
`has_capacity()` returns false, worker is excluded from assignment.
Wire-compatible: old workers don't send it, scheduler reads default
`false`. Cleared when the breaker closes or half-opens.
```

**`docs/src/components/worker.md`** (near line 152):

```
r[worker.silence.timeout-kill]

`maxSilentTime` (seconds, forwarded from client `--option
max-silent-time`) is enforced rio-side in the stderr read loop: on
each STDERR_NEXT/READ/WRITE, reset `last_output`; a `select!` arm
fires at `last_output + max_silent_time` → `SilenceTimeout` outcome →
`cgroup.kill()` → `BuildResult { status: TimedOut, error_msg: "no
output for Ns (maxSilentTime)" }`. The local nix-daemon MAY also
enforce it (forwarded via `client_set_options`) — rio-side is the
authoritative backstop ensuring the correct `TimedOut` status.
```

---

**`docs/src/components/gateway.md`** (new section):

```
r[gw.rate.per-tenant]

Per-tenant build-submit rate limiting via `governor`
`DefaultKeyedRateLimiter<String>` keyed on `tenant_name` (from
authorized_keys comment — operator-controlled, cannot be forged by
client; `None` → key `"__anon__"`). Default quota: 10/min with burst
30, configurable via `gateway.toml`. On rate-limit violation:
`STDERR_ERROR` with wait-hint, early return — do NOT close the
connection. No key-eviction (operator-controlled keyspace is bounded);
`TODO(phase5)`: eviction if keys ever become client-controlled.

r[gw.conn.cap]

Global connection cap via `Arc<Semaphore>` (default 1000, configurable
via `gateway.toml max_connections`). `try_acquire_owned()` in the
accept loop before spawning the session task; permit is held by the
task and dropped on disconnect. At cap: russh
`Disconnect::TooManyConnections` before session spawn.
```

---

**`docs/src/components/controller.md`** (new section):

```
r[ctrl.gc.cron-schedule]

Controller runs a GC cron reconciler: `tokio::select!` on
`shutdown.cancelled()` vs `interval.tick()` (default 24h, configurable
via `controller.toml gc_interval_hours`; 0 = disabled). Each tick:
connect to store-admin with `tokio::time::timeout(30s, ...)` — on
connect failure, `warn!` + increment
`rio_controller_gc_runs_total{result="connect_failure"}` + `continue`
(NEVER `?`-propagate out of the loop — tonic has no default connect
timeout and a stale IP hangs on SYN). On success: `TriggerGC`, drain
the `GcProgress` stream, increment `{result="success"}`.
```

## Files

```json files
[
  {"path": "docs/src/phases/phase4b.md", "action": "MODIFY", "note": "T1: mark [x] NAR-scanner/Karpenter/cli-packaging; strike worker.refs.nar-scan; update 009→012 note"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T3: add r[store.gc.tenant-retention], r[store.gc.tenant-quota], r[store.cas.xmax-inserted]"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T3: add r[sched.gc.path-tenants-upsert], r[sched.timeout.per-build], r[sched.retry.per-worker-budget]"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "T3: add r[worker.fuse.circuit-breaker], r[worker.heartbeat.store-degraded], r[worker.silence.timeout-kill]"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T3: add r[gw.rate.per-tenant], r[gw.conn.cap]"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T3: add r[ctrl.gc.cron-schedule]"},
  {"path": "docs/src/challenges.md", "action": "MODIFY", "note": "T4: line 100 deferral → forward-ref r[worker.fuse.circuit-breaker]"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "T4: line 52 deferral → forward-ref r[worker.fuse.circuit-breaker]"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T2: line 166 TODO(phase4b)→TODO(phase4c) (trailer-refs, gated on measurement)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T2: line 9 TODO(phase4b)→TODO(phase4c) (query! macro, blocked on .sqlx/ build)"}
]
```

```
docs/src/
├── phases/phase4b.md              # T1: ground-truth corrections
├── components/
│   ├── store.md                   # T3: +3 markers
│   ├── scheduler.md               # T3: +3 markers
│   ├── worker.md                  # T3: +3 markers
│   ├── gateway.md                 # T3: +2 markers
│   └── controller.md              # T3: +1 marker
├── challenges.md                  # T4: forward-ref
└── failure-modes.md               # T4: forward-ref
rio-worker/src/upload.rs           # T2: 1-line retag
rio-scheduler/src/db.rs            # T2: 1-line retag
```

## Dependencies

```json deps
{"deps": [203], "soft_deps": [], "note": "frontier root for all of phase 4b — MUST merge before Wave 1"}
```

**Depends on:** [P0203](plan-0203-niks3-upload-pipeline.md) — backfill terminal; `phase-4a` tag lands after `644d74e`. P0203 is DONE, so this is immediately in the frontier.

**Conflicts with:** none. This is the **only** plan in 4b touching `docs/src/components/*.md` — all 12 markers seeded upfront means zero downstream doc collisions. The two `.rs` files have 1-line retags at `:166`/`:9`; P0206 touches `db.rs` elsewhere (line `:591+`, distant).

**Serialization: HARD.** MUST merge before Wave 1 (all downstream plans reference these marker IDs in their `r[impl]`/`r[verify]` annotations).
