# Executor-lifecycle / session-protocol invariant ↔ spec-rule map

Campaign #1 of the simplification arc: verify the as-built
scheduler⇄executor session machinery (registration, heartbeat,
assignment push, completion delivery, disconnect/reap/reconcile,
draining, the controller's Job/pod lifecycle and termination
reporting), then — only on a 0e "go" — replace it with the
"assignment IS the pod" pull protocol. The contract for this map is
`executor-formal-design.md` (DRAFT v2) §3, §5, §6; the evidence base
is `executor-inventory.md` (snapshot `e650f23a4`, 2026-05-25),
re-pinned below. The methodology is the one proven on rio-lease, the
log subsystem, retry-formal, refcount, and the controller campaign.

This file is the campaign artifact. Stage 0a (this commit set) adds
the churn pin, the inventory re-pin, the corpus pin with its
pre-registered partition, the encodability pre-registration, and the
open-adjudication tracking. Stage 0b adds the invariant ↔ rule map
proper (one subsection per F1–F8 invariant), the new spec rules, the
contradiction records, and the witness / as-built-falsification
pre-registrations. Stage 0c adds the Stage-B model verdicts
(`executorSession.qnt`, `executorDelivery.qnt`). Stage 0d adds the
Stage-C calibration table against the corpus pinned here. Stage 0e
adds the frozen replacement contract and the go/no-go record.

## Phase 0a — churn pin and re-pin protocol

Pin date: 2026-05-26. Base: the `formal-sprint` lineage at
`277618342` ("test(rio-retry-kernel): bound the classify harness and
record the post-extraction CBMC findings"). This tip is **after** the
retry campaign's Phase 1b–2 and close-out (the durable attempt
ledger, the establishment vehicle, the legacy counter retirement),
after the third harden-subst rebase, after the in-scheduler build-log
deletion / LogService cutover (`f1c758bb5`, `73b727732`), and after
the controller campaign's Stage C — i.e. all the churn the design
flagged as having moved past the inventory snapshot is included in
the pin.

### What is pinned

The in-scope file set (the churn set; the corpus query below uses the
narrower nine-path set):

| In-scope path | Last commit touching it (at the pin) | Last `fix(…)` commit |
|---|---|---|
| `rio-scheduler/src/state/executor.rs` | `001cf0eeb` 2026-05-26 (retry Stage-A self-review corrections) | `001cf0eeb` 2026-05-26 |
| `rio-scheduler/src/actor/executor.rs` | `bcfa87ef8` 2026-05-26 (legacy retry counter retirement) | `7d5646105` 2026-05-26 |
| `rio-scheduler/src/actor/housekeeping.rs` | `bcfa87ef8` 2026-05-26 | `125feb450` 2026-05-26 |
| `rio-scheduler/src/grpc/executor_service.rs` | `f1c758bb5` 2026-05-26 (build-log subsystem deletion) | `8f6190df7` 2026-05-26 |
| `rio-scheduler/src/assignment.rs` | `cde21963a` 2026-05-26 (fleet-exhaust onto placeable()) | `001cf0eeb` 2026-05-26 |
| `rio-builder/src/runtime/` | `73b727732` 2026-05-26 (LogService cutover) | `0ea9bd701` 2026-05-26 |
| `rio-builder/src/main.rs` | `fa3bc53d5` 2026-04-08 | `96056b318` 2026-04-07 |
| `rio-builder/src/health.rs` | `fdbe38517` 2026-04-08 | none (no fix commit in its history) |
| `rio-controller/src/reconcilers/pool/` | `d7cce02ae` 2026-05-26 (controller Stage-A markers) | `be2f50e9e` 2026-05-21 (docs-only); last behavior fix `f97644a53` 2026-05-11 |

Shared actor files in scope for this churn table and the
repair-mechanism audit only (NOT part of the corpus query — see the
corpus pin):

| Shared path (session-relevant slices) | Last commit | Last `fix(…)` |
|---|---|---|
| `rio-scheduler/src/actor/mod.rs` | `66e73569a` 2026-05-26 | `4f12a7ffa` 2026-05-26 |
| `rio-scheduler/src/actor/dispatch.rs` | `ea3b1c078` 2026-05-26 | `44d4235b8` 2026-05-26 |
| `rio-scheduler/src/actor/completion.rs` | `ea3b1c078` 2026-05-26 | `7d5646105` 2026-05-26 |
| `rio-scheduler/src/actor/recovery.rs` | `f512516f9` 2026-05-26 | `44d4235b8` 2026-05-26 |
| `rio-scheduler/src/actor/snapshot.rs` | `7f7a19b8a` 2026-05-23 | `421d674b5` 2026-05-11 |

Peer files whose interfaces the models will treat as environment
(same role as the controller map's peer table):

| Peer path | Why it is a peer | Last commit at the pin |
|---|---|---|
| `rio-scheduler/src/retry_policy.rs` + `rio-retry-kernel/` | the fold/decide()/placeable() kernels Model S imports as guarantees (campaign #4, closed) | `277618342` 2026-05-26 |
| `rio-scheduler/src/db/attempts.rs` | the durable attempt ledger (two-installment rows, `fill_termination`) the termination/establishment path now lands on | `bcfa87ef8` 2026-05-26 |
| `rio-scheduler/src/lease_hooks.rs`, `rio-lease/` | leader environment (imported abstract action; hook ordering) | `125feb450` 2026-05-26 |
| `rio-scheduler/src/sla/cost.rs` | ICE backoff ladder (controller-campaign peer state) | `8026d5f2b` 2026-05-15 |
| `rio-controller/src/reconcilers/nodeclaim_pool/` | Model N's subject (controller campaign); consumes the `dead_nodes` signal this subsystem produces | `782b6155b` 2026-05-24 |
| `rio-proto/proto/{builder,build_types,admin}.proto` | the wire surface (§1.1) | `17856466d` 2026-05-26 |

### Design-hash → on-lineage cross-walk

Every design-named hash (the §3.5 / T-0d.2 representatives and the
cross-campaign exemplars) was resolved against the pin. All are
HEAD-reachable identity rows except the two below (subject-line
match, same content):

| Design / inventory hash | On-lineage hash at the pin | Subject |
|---|---|---|
| `5c47af5ad` | `0ea9bd701` | fix(rio-scheduler): advertise only the post-recovery generation in heartbeat replies |
| `f1902fe63` (inventory G6 "lease hooks in order") | `125feb450` | fix(rio-scheduler/rio-lease): deliver lease hooks to the actor in invocation order |

Identity rows (verified ancestors of the pin, no substitution
needed): `db457374f`, `a6697c6b0`, `0127cf854`, `be3ad068e`,
`6b6cfcf10`, `8201db59b`, `1353d3224`, `29222884e`, `5971778f8`,
`1757790f2`, `96d8092b8`, `a62631c90`, `20afe5154`, `c5c5ccd17`,
`ee9302b86`, `8283d4362`, `172776b1b`, `c13f6a277`, `8d38cb999`,
`e872b2b49`, `dc094dd0c`, `6a9ba0ef0`, `7f04c9d88`, `fba9086dc`,
`9123e72d4`, `4f8f68ff8`, `3082598a3`, `451f2dc80`, `9a2dbc873`,
`83e0b338f`, `ea10e1d74`. The T-0d.2 representative table and the
partition below are written against on-lineage hashes from the
start; T-0d.1 only re-checks for drift after 0a.

### Inventory re-pin: anchor re-verification

The inventory cites `e650f23a4`, which is off-lineage after the third
harden-subst rebase (merge base `d79d633685`); 302 commits separate
it from the pin (cross-lineage superset), 29 of them touching the
churn set above. The scheduler-side files absorbed the retry
campaign's Phase 1b–2 and the build-log deletion since the snapshot,
so the §2/§3 anchors were re-verified one by one rather than carried
over. Verdict: **every §2 state-inventory anchor and §3.1/3.2/3.3
mechanism anchor re-confirms at the pin** — no mechanism was removed
or absorbed — with the line shifts and the four content deltas
recorded below. Inventory references should be read against this
section from here on.

Content deltas (mechanism behavior that moved since the snapshot —
the design anticipated all four):

1. **`recently_disconnected` carries the released execution
   identity.** The map is now `HashMap<ExecutorId,
   DisconnectedAttempt>` (`actor/mod.rs:292-300`, map at `:364`),
   carrying `drv_hash`, `derivation_id`, the released `exec_id`, and
   the observation instant — not the snapshot's `(DrvHash, Instant)`
   pair. Inventory §2.2's purpose/mutator/failover columns otherwise
   stand (insert on mid-build disconnect, first-classifier-wins
   consumption, cleared on leader transition).
2. **The 60 s TTL sweep is now the establishment vehicle.** The sweep
   (`actor/executor.rs:1143-1290`, TTL constant
   `TERMINATION_REPORT_TTL` = 60 s at `:31`) no longer just drops
   expired entries: an entry whose classifying report never arrived
   is established as a durable `executor_crash`/unreported attempt
   fill on the released `(derivation_id, exec_id)` row and charged
   through `decide()` (retry campaign T-1b.11). Mechanism #14's
   detection role is unchanged; its repair action gained a durable
   write.
3. **The termination/establishment path lands on durable attempt
   rows.** `handle_executor_termination` (`actor/executor.rs:676`)
   and the disconnect path append/fill `drv_attempts` rows (migration
   066; `db/attempts.rs::fill_termination` at `:386`, idempotent via
   `WHERE termination_reason IS NULL`); the first-report-wins dedup
   of mechanism #15 is now also a schema property
   (`drv_attempts_exec_id_uniq`). The in-memory dedup
   (`recently_disconnected.remove()`, race-ahead `last_completed`)
   still exists and is what Model S owns (F4's lifecycle).
4. **The legacy retry counter mutations are retired and the
   dispatch-time fleet-exhaust verdict is `placeable()`.**
   (`bcfa87ef8`, `cde21963a`.) The backstop (#17), disconnect (#3),
   and termination (#15/#16) charging paths now flow through the
   retry kernels; `dispatch_fleet_exhausted`
   (`actor/dispatch.rs:1789`) consults the kernel. This does not
   change which repair mechanisms exist; it changes what they write.

Anchor re-verification (new locations at the pin; "≈" means the
function/region begins at the cited line):

§2 state inventory:

| Inventory anchor | At the pin |
|---|---|
| §2.1 `ExecutorState`, 18 fields (`state/executor.rs:28-186`) | re-confirmed — struct at `state/executor.rs:28`, same 18 fields; `new()` ≈ `:192`, `is_registered` `:229`, `has_capacity` `:240` (incl. the I-095 `is_closed` check), `is_draining` `:255` |
| §2.2 actor maps (`actor/mod.rs`) | `executors` `:315`; `hung_nodes` `:327`; `authoritative_binding` `:353`; `recently_disconnected` `:364` (type delta 1 above); `dispatched_cells` `:456`; pacing fields `dispatch_dirty`/`probe_generation`/`became_idle_inline_this_tick` `:642-662` |
| §2.3 DAG-side per-drv session fields | re-confirmed; `transition_to_assigned` ≈ `dispatch.rs:1921`, exec_id mint inside `assign_to_worker` ≈ `:1851`, rollback ≈ `:2077` |
| §2.4 builder runtime state | re-confirmed; `BuilderRuntime` fields ≈ `runtime/mod.rs:779-805` (`relay_target_tx`, `completion_pending`, `latest_generation`, `idle_timeout`), `BuildSlot` `runtime/slot.rs` (`try_claim` `:63`, cgroup path `:28-32`) |
| §2.5 controller cross-tick state | re-confirmed (per-tick recomputation; Jobs/pods + annotations are the durable state) |
| §2.6 durable PG state | re-confirmed for `assignments` / `drv_executions` / status mirror; **new**: `drv_attempts` (066) — the scheduler-owned attempt ledger (delta 3); the executor fleet still has no PG table |
| §2.7 what failover loses | re-confirmed — `clear_persisted_state` ≈ `actor/mod.rs:872`; clears `recently_disconnected`, `dispatched_cells`, `hung_nodes`, `authoritative_binding`; retains `executors` (the `executors: _` bind); standby drops `ProcessCompletion` (`actor/mod.rs:1190`) |

§3.1 scheduler-side mechanisms (1–22):

| # | At the pin |
|---|---|
| 1 heartbeat-timeout reaper | ≈ `housekeeping.rs:291-312` (`HEARTBEAT_TIMEOUT_SECS`) |
| 2 stall credit | `credit_heartbeats_for_stall` `housekeeping.rs:240` (call sites unchanged) |
| 3 stream-close → disconnect → reassign | `executor_service.rs:507-516`, `actor/executor.rs:347`, `reassign_derivations` ≈ `:565` |
| 4 stream-epoch stale-disconnect filter | `actor/executor.rs:363-371` (unchanged) |
| 5 reconnect stale-flag clear | inside `handle_worker_connected` ≈ `:116-247` (region unchanged) |
| 6 reconnect hijack + intent-mismatch rejection | `actor/executor.rs:148-176`; accept-gate `executor_service.rs:226-239` |
| 7 unknown-executor heartbeat drop | inside `handle_heartbeat` ≈ `:1444` (I-048b arm) |
| 8 heartbeat running-build TOCTOU keep | inside `handle_heartbeat` / `reconcile_running_build` ≈ `:1680` |
| 9 heartbeat adopt | `adopt_heartbeat_build` ≈ `:1860` |
| 10 phantom two-strike + drain_phantoms | `drain_phantoms` ≈ `:1803`; suspect marking in `reconcile_running_build` |
| 11 closed-stream exclusion + WARN | `assignment.rs:53`, `state/executor.rs:240-249` |
| 12 completion capacity-free hoist + stale-report guard | `completion.rs:898` (handler), hoist + `last_completed` `:1001-1007`, stale guard `:1096-1106` |
| 13 one-shot draining-on-completion + last_completed | `completion.rs:1009-1019` (I-188/I-197 comments intact) |
| 14 `recently_disconnected` correlation map + TTL sweep | map `actor/mod.rs:364`; sweep ≈ `actor/executor.rs:1143-1290` (deltas 1–2) |
| 15 termination-report dedup | `handle_executor_termination` ≈ `:676`; `fill_termination` `db/attempts.rs:386` (delta 3) |
| 16 DeadlineExceeded job-name prefix-match | second half of `handle_executor_termination` (info line "DeadlineExceeded backstop fired" ≈ `:1132`) |
| 17 backstop timeout | `tick_scan_dag` ≈ `housekeeping.rs:314` + `tick_process_backstop_timeouts` (charging now via decide(), delta 4) |
| 18 dispatch rollback | `rollback_assignment` ≈ `dispatch.rs:2077` |
| 19 post-failover reconcile (45 s) | `handle_reconcile_assignments` ≈ `recovery.rs:1779`; `RECONCILE_DELAY` = 45 s `recovery.rs:2153` |
| 20 hung-node detector | `snapshot.rs::detect_hung_nodes` `:51`; `tick_hung_nodes` `housekeeping.rs:276-288` |
| 21 leader-transition hygiene | `clear_persisted_state` ≈ `actor/mod.rs:872`; leader gates at command dispatch (`actor/mod.rs:1190` and the per-handler gates) |
| 22 dispatched_cells sweep + ICE heartbeat-edge clear | `tick_sweep_dispatched_cells` `housekeeping.rs:741`; registration-edge clear in `handle_heartbeat` |

§3.2 builder-side (B1–B9): B1 `'reconnect` loop ≈ `runtime/mod.rs:814/:840`;
B2 swap-after-Ok `:907-912`; B3 graceful half-close ≈ `:1054-1061`;
B4 drain gate `runtime/drain.rs:37-120` + `completion_pending`
set-before-sink `runtime/result.rs:227-244`; B5 generation fence
`runtime/mod.rs:75` + heartbeat `fetch_max` `runtime/heartbeat.rs:207`;
B6 slot-busy / draining rejection in `handle_assignment` ≈
`runtime/mod.rs:1186`; B7 idle-timeout exit ≈ `:985`; B8 heartbeat RPC
timeout `runtime/heartbeat.rs:24-36`; B9 panic-catcher completion
`runtime/result.rs:132` + heartbeat-task liveness `runtime/mod.rs:915`
+ teardown abort ≈ `:1133-1139`. All re-confirmed.

§3.3 controller-side (C1–C6): C1 `reap_stale_for_intents`
`pool/jobs.rs:785`; C2 `reap_excess_pending` `pool/job.rs:373`
(`REAP_PENDING_GRACE` = 10 s `:270`, `is_pending_job` `:223`); C3
`select_orphan_running`/orphan reap `pool/job.rs:496`
(`ORPHAN_REAP_GRACE` = 300 s `:256`, leader-age fail-closed arm ≈
`:539-562`); C4 `report_terminated_pods` ≈ `pool/job.rs:990-1050`; C5
`report_deadline_exceeded_jobs` ≈ `:1090-1120`; C6
`pool/disruption.rs` (whole file). K8s-native backstops unchanged
(`JOB_TTL_SECS` = 600 `pool/job.rs:50`, `JOB_REQUEUE` = 10 s `:40`,
`activeDeadlineSeconds` floor in `pool/jobs.rs`). All re-confirmed.

§4 decision-inventory anchors follow the same shifts as their host
mechanisms above (the predicate chain `rejection_reason`
`assignment.rs:25`, `statically_eligible` `:136`, `best_executor`
`:175`, dispatch pacing `dispatch.rs:113/268/354`); no decision was
added or removed by the churn except the fleet-exhaust collapse
(delta 4), which replaces the open-coded exhaust check with the
kernel's `placeable()` — same decision, one implementation.

### OA6 bookkeeping (design §5.2, recorded at the 0a re-pin)

Inventory §1.9 and §4.4 omit the §13b placeable-gate's
no-ready-filter behavior: the gate (`pool/jobs.rs:353-397`,
`ctrl.nodeclaim.placeable-gate+5`) publishes every FFD-placed
Builder intent with **no ready filter**, so forecast (ready=false)
intents reach the Job spawner and a pod's first pull under the
replacement can land before its drv is Ready. The neighbouring
comment "`queued` counts only Ready intents" (`pool/jobs.rs:458`) is
stale in exactly this sense. Recorded here so the OA6 adjudication
(0e) and the 0b contradiction row (sla-sizing.typ `@alg-pool` vs the
gate) start from the corrected description, not the inventory's.

### Repair-mechanism completeness audit

Audit inputs: `git log e650f23a4..277618342` over the churn set (29
commits) plus a grep of the in-scope production files for incident
markers (`I-[0-9]{3}[a-z]?`, `bug_[0-9]{3}`), diffed against the §3
tables. Result: **no repair mechanism is missing from the §3
tables.** The 29 commits are: the retry campaign's Phase 1b–2 ledger
work and close-out fixes (deltas 1–4 above — they extend mechanisms
#3/#14/#15/#17, none new), the build-log deletion / LogService
cutover (removes scheduler-side log relay code; no session mechanism
touched), the lease hook-ordering fix (`125feb450`, ordering
infrastructure for #21), the controller Stage-A marker commit and
fetcher-budget split (no mechanism change), and doc/comment sweeps.
Incident markers present in the in-scope files all map to mechanisms
already in the table; no marker names a repair behavior outside it.
The table is therefore NOT extended and no new F-family is needed at
0b beyond the design's F1–F8.

### Cross-campaign sequencing and standing directives

- **Retry campaign (#4): closed.** `retry-invariant-map.md` carries
  the campaign close-out ("Campaign close-out — retry/poison/cascade
  (campaign #4)"). The hand-off items this campaign now owns (from
  its "What transfers to the executor-lifecycle campaign (#1)"
  list): the stream-epoch / heartbeat-binding halves of `db457374f`
  and the late-disconnect-vs-reconnect race; heterogeneous static
  eligibility (`a62631c90` — the eligibility computation feeding
  `placeable()`); the correlation/dedup-state lifecycle
  (`recently_disconnected` with the released `exec_id`,
  `last_completed`, the establishment TTL sweep); the
  `ExecRec`/slot identity-freshness encodings in `retryPolicy.qnt`
  as a starting point for Model S's slot state; and the
  file-size-expectation lesson for any durability-adding collapse.
- **Controller campaign: Phase 0 complete, Phase 1 not started.**
  Verified at the pin: `controller-invariant-map.md` carries its
  Stage-C corpus pin (95 commits at `746164c4f`), the calibration
  table, the Stage-C run record, six wired `quint-ctrl-calib-*`
  witnesses, and a "Phase-0 exit-gate verdict: Met" — and no Phase-1
  record or close-out. The design's 0a ordering decision is
  therefore recorded as: **controller Stage C closed — option (a)'s
  ordering precondition (Stage C before this campaign's 1b touches
  `pool/{jobs,job}.rs`) is met.** This is NOT a discharge of that
  map's own constraints; the named-owner-and-ordering entry with the
  start/green-light obligations lives in
  `controller-invariant-map.md`'s in-flight-work section (added in
  this same commit set), and its obligations — the affected-section
  re-audit (J11, the orphan-reap rows, F1/F3, the I12 out-of-model
  entry) at this campaign's 1b/1d, the F1/F3 prerequisite review at
  0e if that campaign is still mid-campaign, and the Stage-C
  calibration-table delta pass — are carried into this campaign's 0e
  and Phase-1 gates (T-0e.6, T-0e.8). G7 cross-references at 0d are
  unblocked (the controller table exists).
- **Standing rebase directive (harden-subst follow-ups).** The
  formal-sprint lineage is periodically rebased onto harden-subst
  follow-ups; the third such rebase already rewrote one
  representative hash (`5c47af5ad` → `0ea9bd701`). A rebase that
  rewrites the pinned base or any hash named in this map triggers
  the re-pin protocol below — re-locate by subject, record old→new
  rows, re-run the corpus query — never a silent re-anchor.

### Re-pin protocol

Run immediately before Stage B (0c) starts and again immediately
before Stage C (0d) freezes the calibration corpus:

```
git log --oneline 277618342..HEAD -- \
  rio-scheduler/src/state/executor.rs rio-scheduler/src/actor/executor.rs \
  rio-scheduler/src/actor/housekeeping.rs rio-scheduler/src/grpc/executor_service.rs \
  rio-scheduler/src/assignment.rs rio-builder/src/runtime rio-builder/src/main.rs \
  rio-builder/src/health.rs rio-controller/src/reconcilers/pool
```

Stage A/B artifacts MUST be re-validated (re-run the affected slice
of this audit, update the affected rows, re-pin this section) if any
of:

1. any commit in that range changes an in-scope file beyond
   comments, doc-comments, or tracey markers;
2. the formal-sprint lineage is rebased so that the pinned base or
   any hash recorded in this map is rewritten (the standing
   directive above);
3. a behavior-relevant change lands on a peer file that an
   assume-guarantee checklist names (retry kernels, lease hooks,
   nodeclaim_pool's dead-nodes consumer);
4. the corpus query below returns new `fix(` commits at the 0d
   freeze (those are bucketed the same way and the delta recorded —
   the 0d gate then audits against the updated counts).

Changes that do NOT trigger re-validation: comment-only or
marker-only commits, test-only commits, spec prose outside the rules
this campaign adds at 0b, and peer-file changes outside the named
checklists (they move the peer table at the next re-pin but not the
audit).

## Stage-C corpus pin: the calibration denominators (pre-registered at 0a)

Pinned 2026-05-26 at `277618342`, before any model or override
exists, per design §3.5 ("partitioned commit-by-commit, with counts,
before any reverting"). These counts are the denominators the 0d
gate is audited against.

### The corpus query

The corpus file set is exactly inventory §5's nine paths — scheduler
`state/executor.rs`, `actor/executor.rs`, `actor/housekeeping.rs`,
`grpc/executor_service.rs`, `assignment.rs`; builder `runtime/`,
`main.rs`, `health.rs`; controller `reconcilers/pool/` — and the
query is, verbatim (one row per distinct commit; a multi-file commit
gets one row):

```
git log --format='%h %s' 277618342 -- \
  rio-scheduler/src/state/executor.rs rio-scheduler/src/actor/executor.rs \
  rio-scheduler/src/actor/housekeeping.rs rio-scheduler/src/grpc/executor_service.rs \
  rio-scheduler/src/assignment.rs rio-builder/src/runtime rio-builder/src/main.rs \
  rio-builder/src/health.rs rio-controller/src/reconcilers/pool \
  | awk '$2 ~ /^fix[(:]/'
```

Query-shape verification: the same query evaluated at the
inventory's snapshot commit (`git log e650f23a4 -- …`) reproduces
the inventory §5 headline figures exactly (334 commits touching the
set, 168 `fix`, 64 `feat`), so the denominators below are continuous
with the figures the design's §3.5 partition was sized against. Two
recorded properties of this query shape, kept deliberately:

- **No `--follow`.** The five shared actor files are excluded by
  design (below), and pre-rename history (`rio-worker/`,
  `reconcilers/{worker,builder,fetcher}pool/`, `worker.rs`-era
  scheduler files, all before 2026-04-06) is NOT part of the corpus
  — the inventory's 334/168 basis did not include it either (the
  `--follow` phrase in inventory §5's method line applied to its
  per-file deep-dives, not the headline count, as the reproduction
  above shows). Widening to the renamed-path union would add ~130
  worker-era fix commits that the design's denominators were never
  built on; if a later stage wants that history it is a recorded
  corpus change, not a silent widening.
- **The five shared actor files** (`actor/{mod,dispatch,completion,
  recovery,snapshot}.rs`) are NOT in the corpus query —
  "session-relevant slices" is not a git-expressible filter, and
  including the whole files would roughly double the corpus with
  retry-/log-/SLA-owned fixes. Lifecycle-owned fixes that live only
  in those files (the establishment-window and dedup-lifecycle
  halves the retry close-out hands over) enter the 0d calibration
  table as the explicitly-listed hand-off rows (T-0d.4), never by
  widening the query.

### The denominator

At the pin the query returns **170 fix commits** (the design 0a
row's "re-pinned fix count"; the snapshot's was 168). Every one of
the 170 is assigned exactly one bucket below; the bucket counts sum
to 170.

| Bucket | Count |
|---|---|
| in-family (owned by this campaign, G1–G8) | **50** |
| cross-campaign-owned — retry table / retry campaign artifacts | **21** |
| cross-campaign-owned — controller Stage-C table | **43** |
| out-of-scope (adjacent subsystems on shared files) | **56** |
| **Total** | **170** |

Per-family in-family counts (the 0d denominators per family):

| Family | n | Notes |
|---|---|---|
| G1 session identity (→ F1) | 8 | |
| G2 outcome delivery (→ F2, scheduler + builder halves) | 11 | |
| G3 liveness calibration (→ F3) | 12 | 6 per-executor + 6 hung-node/node-aggregation sub-family |
| G4 death attribution (→ F4) | 0 | no standalone in-family commit: the charging halves are all retry-owned (cross-campaign rows below); F4's in-family content enters 0d as the hand-off halves (the `db457374f` heartbeat-binding/stream-epoch halves counted under G1, the dedup-entry lifecycle, the late-disconnect-vs-reconnect race) per T-0d.4 |
| G5 eligibility coherence (→ F5) | 7 | |
| G6 failover convergence (→ F6) | 3 | |
| G7 fleet-supply scheduler-side obligations (→ F7) | 3 | the controller half of G7 is cross-campaign (43 rows) |
| G8 input hardening (→ F8) | 6 | |
| **In-family total** | **50** | |

### In-family rows (50)

| Hash | Family | Note |
|---|---|---|
| `db457374f` | G1 | design F1 representative (stream_epoch atomicity + heartbeat auth_intent binding); split row: the deadline/backstop accounting halves are retry-table G1 rows (cross-referenced at 0d) |
| `a6697c6b0` | G1 | design F1 representative (reconnect hijack accept-gate, ExecutorClaims kind binding); split: also a controller G-F row (auth/identity halves) and log-relay halves |
| `ea10e1d74` | G1 | identity chain (ExecutorClaims HMAC) + per-stream caps; split: also a controller G-F row; build-events-bridge half is log-relay |
| `4f8f68ff8` | G1 | adopt-conflict + stream_epoch (I-056 family) |
| `3082598a3` | G1 | clear draining/degraded on reconnect (I-056a) |
| `451f2dc80` | G1 | I-048 zombie guards |
| `9a2dbc873` | G1 | skip re-register on SIGTERM-reconnect when slot idle |
| `83e0b338f` | G1 | dup-Register handling; split: LogBuffers bound + step_down halves are log/lease content |
| `0127cf854` | G2 | design F2 representative (phantom two-strike drain) |
| `be3ad068e` | G2 | design F2 representative (heartbeat adopt of reconnecting worker's build) |
| `6b6cfcf10` | G2 | design F2(D) representative (relay swap-after-Ok) |
| `8201db59b` | G2 | design F2(D) representative (completion_pending before first await + graceful half-close) |
| `1353d3224` | G2 | design F2(D) representative (drain gated on completion-delivered) |
| `29222884e` | G2 | design F2(D) representative (relay watches target change) |
| `aaa08721d` | G2 | decouple dispatch from heartbeat (lost-assignment prevention); split: FUSE-warm bound + build_id sanitize halves |
| `41bc8dd97` | G2 | unsolicited-Cancelled completion left drv stuck after slot freed (C3 half); split: batch-persist/event-emission halves out of model |
| `cc1ca02a7` | G2 | BuildSlot state under one mutex (single-build occupancy coherence); split: upload-scan half store-owned |
| `d653222cf` | G2 | early graceful-drain (builder half); split: gateway drain + FUSE xattr halves |
| `e4ed7b6a9` | G2 | builder DrainExecutor retry on not-leader (exit choreography; mechanism since deleted by `fb3ea232d`) |
| `5971778f8` | G3 | design F3 representative (reap at 30 s + handle_tick leader gate) |
| `1757790f2` | G3 | design F3 representative (stall credit at all 8 FMP sites) |
| `44a55a224` | G3 | stall-credit early-return removal |
| `e7b8ee91a` | G3 | heartbeat RPC timeout < interval (bug_044) |
| `d12b31027` | G3 | abort heartbeat + DrainExecutor timeout on ephemeral exit (I-142) |
| `f9c89bb92` | G3 | ephemeral idle-timeout exit (I-116) |
| `99a17cd2f` | G3 (hung-node) | authoritative_binding map for detect_hung_nodes; controller table carries it as a Remainder (out-of-its-model) row |
| `468900350` | G3 (hung-node) | tenant_of keyed on auth_intent |
| `9699ac8b2` | G3 (hung-node) | key on auth_intent, floor 2, TTL-only retain |
| `6b152ee22` | G3 (hung-node) | repeats across ticks; clear_persisted_state exhaustive half noted |
| `b9a131ded` | G3 (hung-node) | group by controller-authoritative node binding |
| `b6d26c001` | G3 (hung-node) | computed before stale-reap in handle_tick |
| `a62631c90` | G5 | design F5 representative (fleet-exhaust system/feature-aware); split: retry-table G7 row records it NOT-ENC there and hands the eligibility half to this campaign |
| `20afe5154` | G5 | design F5 representative (intent-matched pod resource-fit self-rejection); split: IceBackoff/solve_full halves are SLA/ICE content |
| `96d8092b8` | G5 | design F5 representative (skip closed-stream executors in dispatch); inventory listed it under G3 — re-grouped to match the design's F5 row |
| `9ce1bcf1b` | G5 | PrefetchComplete routed through became_idle inline cap |
| `c9382fd63` | G5 | became_idle inline dispatch cap per tick |
| `a52c3ec80` | G5 | builder-side features derivation from executor_kind (eligibility input) |
| `6fb244337` | G5 | PrefetchHint contents (inputSrcs) — warm-path correctness |
| `c5c5ccd17` | G6 | design F6 representative (leader-gate reassign_derivations); retry-table G5 row records it NOT-ENC there |
| `0ea9bd701` | G6 | design F6 representative (advertise only post-recovery generation); old hash `5c47af5ad` |
| `374280877` | G6 | leader gates on ProcessCompletion/ReportExecutorTermination/Tick + clear_persisted_state per-generation maps; split: breaker/gc-roots/log-seq halves out of scope |
| `445928288` | G7 | scheduler-side ICE/ack arming (arm-on-ack + dag sweep + single edge-reload owner) |
| `461f6c661` | G7 | scheduler-side ICE clear semantics (registered_cells/heartbeat, not pending ack) |
| `2c8abc9b6` | G7 | scheduler-side ICE-attempt orphan reap keyed on dag state |
| `9917c384d` | G8 | bound every worker-supplied string at the boundary |
| `2143845d6` | G8 | bound derivation_path length at ingestion |
| `d40b3ee86` | G8 | worker-supplied float validation |
| `6b0de6e4e` | G8 | worker log line-number ordering + span totality |
| `7ffbf1415` | G8 | BuildPhase ingestion gated on (executor, drv) binding |
| `496e6fb14` | G8 | (executor, drv) binding check in recv task |

### Cross-campaign-owned rows: retry (21)

Each row links the owning retry artifact it will be cross-referenced
against at 0d (no re-run here).

| Hash | Owning retry row / artifact |
|---|---|
| `ee9302b86` | retry calibration table G5 (race-ahead report keeps pending entry; permanent witness `quint-retry-calib-g5-race-ahead`) |
| `e872b2b49` | retry calibration table G5 (non-promoting report preserves the correlation entry) |
| `dc094dd0c` | retry calibration table G1 (assigned-only disconnects; covered by `retryCalibG1DisconnectCharges`) |
| `8d38cb999` | retry calibration table G1 (I-213 disconnect-path exemption) |
| `c13f6a277` | retry calibration table G1 (I-213 max_retries exemption; NOT-ENC there, P4 vehicle) |
| `8283d4362` | retry calibration table G1 (window-reset gate + controller-OOM cap-check, two falsifying rows) |
| `172776b1b` | retry divergence catalog C1/C4 + controller Stage-C G-E row (deadline-exceeded ownership) |
| `2acd1b327` | retry calibration table G6 (floor-ladder family, NOT-ENC) + controller Stage-C G-G row |
| `c55467cbc` | retry calibration table G6 (floor-ladder family) |
| `37c21bb7b` | retry calibration table G6 (floor-ladder family) |
| `1184d1bb8` | retry calibration table G6 (floor-ladder family) |
| `12b86c285` | retry calibration table G6 (deadline alignment) + controller Stage-C G-G row |
| `a76589e37` | retry calibration table G6 (configuration plumbing) |
| `8a016a393` | retry calibration table G1 (at-cap OOM single-count) |
| `a60d58a32` | retry calibration table G1 (PutPath exemption / window) |
| `699ad52e1` | retry calibration table G1 (exempt-cap) + G7 (draining-exclusion); the output-membership half stays with completion-intake unit tests |
| `d91df7e9f` | retry calibration table G3 (NOT-ENC build-level keep_going) |
| `3973a4f54` | retry calibration table G3 (recovery re-cascade half) |
| `5b4543c3a` | retry calibration table G3/G8 (recovery halves); non-retry halves are observability bookkeeping |
| `7d5646105` | retry campaign Phase-1b/2 implementation fix (floor outcome on controller-classified attempt rows) — owned by the retry close-out, post-dates its calibration corpus |
| `001cf0eeb` | retry campaign Stage-A self-review corrections — owned by the retry Stage-A record |

### Cross-campaign-owned rows: controller Stage-C table (43)

All 43 are members of the controller campaign's pinned 95-commit
corpus; each links its family row there (the G7 fleet-reconciliation
family of inventory §5 is owned by that table, per design §3.5).

| Controller family row | Hashes (this corpus ∩ that table) |
|---|---|
| G-A spawn↔reap↔queued coherence | `7f04c9d88`, `6a9ba0ef0`, `fb0953870`, `fba9086dc`, `6c4f4983d`, `9123e72d4`, `fd5d7c988`, `5e01a9ff1`, `8b0128f5a`, `004956eeb` |
| G-B ack/ICE protocol | `cdc78f839`, `5815a7544`, `485e736a2`, `af1383c0e`, `e8bd76451`, `d6bc376d3`, `408a48bcb` |
| G-C resource-accounting parity | `a415a9a8b`, `286566a57`, `d5602b3aa`, `073170dfb`, `5250a4b9a`, `b25836ef1`, `5c2a83761`, `bcfdc2262` |
| G-D placement derivation | `80cfcd65c`, `039861b56`, `3f416e02e`, `2f9a3769c`, `9fd4b6e59`, `b570cdd8d`, `015667efa`, `f97644a53` |
| G-E deadline coupling | `f73b98b1f` (and `172776b1b`, `12b86c285`, `2acd1b327` listed under retry above — one row each, the retry link is primary, the controller link noted) |
| G-F identity/security plumbing | `acf6d476b` (`a6697c6b0`, `ea10e1d74` are in-family G1 rows above with the G-F cross-reference noted) |
| FFD/cover ⇄ scheduler-config parity | `f333ebed5`, `c5320b40e`, `e013b2044` |
| Remainder | `2ad753db9`, `416895e3e`, `3c3062760`, `c8ca42a91`, `dbc7f7cb2` |

### Out-of-scope rows (56)

Fixes on the corpus files whose repaired behavior belongs to an
adjacent subsystem; each names the subsystem (the 0d table carries
these as OUTSIDE rows, not dispositioned here).

| Subsystem | n | Hashes |
|---|---|---|
| log relay / banner / LogService data plane (incl. the since-deleted in-scheduler log subsystem) | 13 | `44d4235b8`, `8f6190df7`, `849fce331`, `2c301438d`, `c04b5e2a4`, `77a08ec14`, `649b89b81`, `32cd79bec`, `7868d46f2`, `c638fe449`, `7beb1ca00`, `1d51bc845`, `5be205ebc` (gateway log rendering); log halves of split in-family rows are noted in-row above |
| SLA solver / hw-class sampling / estimator | 14 | `bce30573b`, `82f0e9fde`, `20bfb3bee`, `054e8083c`, `90fbf5b52`, `bd41e23ea`, `93ce060f0`, `c6163485a`, `13acff94f`, `b81da271f`, `a9a2e6fc1`, `827b56255`, `077854387`, `c967d75d6` |
| FUSE / overlay / store / FMP probe | 7 | `d5b99450d`, `9c85bcfe5`, `bf7e516e4`, `96056b318`, `8f917db2c`, `77f628ddb`, `702b9ea00` |
| controller pod-spec construction (pool/pod.rs and friends; excluded from the controller corpus by its definition, G-D-disposition coverage there) | 4 | `cda4ad612`, `5b4db724d`, `54ec6d079`, `3ec9120af` |
| auth / service-token gating | 3 | `e36a645cc`, `a92c03ddf`, `fb3ea232d` |
| build/DAG bookkeeping (build-level transitions, client-orphan sweep, cancel bookkeeping) | 3 | `71a7c8a9b`, `a54ac4650`, `1dd32cc10` |
| builder execute-loop / cgroup resources | 2 | `34a4c40be`, `a6b72bf94` |
| controller pool status/census + ComponentScaler (outside both pinned corpora) | 2 | `b19164959`, `e89b89110` |
| observability / tracing | 2 | `475b79eee`, `81963379a` |
| test / spec-annotation hygiene | 3 | `785288a3b`, `0dbd5f2af`, `f005fa55c` |
| CA-derivation dispatch chain | 1 | `6434a2f45` |
| retry-policy configuration validation | 1 | `002effbab` |
| lease / leader-election machinery (rio-lease campaign) | 1 | `125feb450` |
| **Total** | **56** | |

Three rows in this partition re-disposition an
inventory §5 grouping, recorded here so the deviation is explicit:
`849fce331`/`2c301438d` (inventory G6) are out-of-scope log-relay
content — the gated object is log-buffer state owned by the log
campaign and since deleted from the scheduler; `96d8092b8`
(inventory G3) is in-family G5 to match the design's F5
representative row; `f1902fe63` → `125feb450` (inventory G6) is
out-of-scope lease machinery — the hook-ordering forwarder belongs
to the rio-lease campaign's model, not Model S.

### Per-family encodability pre-registration (design §3.5, carried into the partition)

Pre-registered now so every 0d verdict is a checked prediction; an
encodability prediction that fails at 0d is a stop-and-report, never
a silent re-disposition.

| Family (in-family bucket) | Pre-registered encodability |
|---|---|
| F1 (G1) | Model S |
| F2 scheduler half (G2: phantom drain, adopt, dispatch-decouple, unsolicited-Cancelled, slot coherence) | Model S |
| F2 builder half (G2: swap-after-Ok `6b6cfcf10`, half-close `8201db59b`/bug_117, drain-on-delivery `1353d3224`, relay-watch `29222884e`, exit choreography rows) | Model D at await-point granularity |
| F3 per-executor liveness (G3: reap bound, stall credit, RPC timeout, idle/ephemeral exit) | Model S |
| F3 hung-node / node-aggregation sub-family (G3: the six hung-node rows) | node-regime cfg only (2 slots, 1 node, 2 tenants); else NOT-ENCODED with `chaos.nix` and `lifecycle/recovery.nix` named as the coverage |
| F4 lifecycle half (no standalone commits; the hand-off halves of T-0d.4) | Model S (needs the exec_id-carrying `recently_disconnected` map and the establishment action) |
| F5 (G5) | Model S; the static-eligibility *content* (kind/system/features arithmetic, warm/prefetch internals) is expected NOT-ENCODED with the dispatch/assignment unit tests named |
| F6 (G6) | Model S (fault-leader regime; deposed-believer window kept reachable) |
| F7 (G7 scheduler-side rows) | NOT re-modeled — covered by `spawnCoherence.qnt` (controller campaign) plus Model S's stated guarantees (busy-accuracy, ack arming, report idempotency); expected NOT-ENCODED rows naming that coverage |
| F8 (G8) | NOT-ENCODED by design (bounds checks + existing unit tests at the gRPC boundary) |
| G7 controller half (cross-campaign bucket) | controller campaign's table (already calibrated there) |

## Open adjudications (0a tracking)

Owner for every entry: B. Meurer (campaign owner; also the
controller-campaign owner, so the cross-campaign asks below are
recorded as self-issued and tracked here rather than negotiated).
Status values: open / data-pending / decided-at-0e.

### OA1 — establishment-window instrument (decision recorded at 0a)

Decision needed at 0e: the pull-mode establishment deadline + slack
and the no-report degradation mode, signed against an as-built
baseline of (i) Job/pod-terminal → `ReportExecutorTermination`
acked, and (ii) terminal/death observation → derivation back to
Ready, per cause (worker-report / pod-terminal / establishment).

**Instrument decision at 0a: option (b) — the documented log/DB join
— was audited first per the plan's default; the audit (below) shows
interval (i) cannot be reconstructed from existing sources at
production retention, so the option-(a) authorization request
(additive histogram pair, the design's single sanctioned Phase-0
production change) is escalated to the campaign owner now, at 0a.**
Until that authorization is granted or refused, option (b)'s partial
query (below) is the standing instrument and starts accumulating
what it can measure; the controller-outage arm is exercised in the
VM suite either way. If authorization is refused and (b) cannot be
extended, no-go condition 5 ("the OA1 baseline cannot be obtained")
is the live risk to record at 0e — not a reason to soften the gate.

Bounded source audit (the option-(b) feasibility evidence, verified
at the pin):

- Interval (i) endpoint A — Job/pod terminal: exists only as k8s
  object state (pod `containerStatuses[].state.terminated`
  / Job `Failed/DeadlineExceeded` conditions). Jobs and their
  terminal conditions are TTL-reaped 600 s after finish
  (`JOB_TTL_SECS`, `pool/job.rs:50`); pods can be deleted earlier by
  the Job controller. The controller does not log the observation
  per se; `report_terminated_pods` / `report_deadline_exceeded_jobs`
  log at `debug!` for skips and at `warn!` for RPC errors.
- Interval (i) endpoint B — report acked: the controller logs
  `info!` ONLY when the scheduler reply says the floor was promoted
  ("reported pod termination → scheduler bumped resource_floor",
  `pool/job.rs` ≈ `:1034`, and the DeadlineExceeded twin ≈ `:1106`);
  non-promoting acks are silent at info. On the scheduler side the
  second-installment classification is persisted by
  `fill_termination` (`db/attempts.rs:386`), which updates
  `termination_reason`/`outcome_class`/floor flags and **writes no
  timestamp** — the attempt row's `occurred_at`/`recorded_at` are
  set when the row is appended (at disconnect/dispatch time), so
  report/ack time is not recoverable from the DB.
- Interval (ii) endpoint A — death/terminal observation: the
  disconnect is countable (`rio_scheduler_worker_disconnects_total`,
  `actor/executor.rs:452`) but not timestamped per-executor in any
  durable store; the pod-terminal cause's true start (the pod's
  death) is the same k8s-side timestamp as interval (i) endpoint A.
- Interval (ii) endpoint B — derivation back to Ready: recoverable.
  The requeue happens in the same actor turn as the terminal
  observation's attempt-row append, so `drv_attempts.recorded_at`
  per `outcome_class ∈ {disconnected, executor_crash, backstop,
  infra, timeout, …}` (cause label = `outcome_class` ×
  `reporting_party`) is a faithful end-point; `derivations.updated_at`
  (001) is overwritten by later transitions and is only a weak
  cross-check. Establishment-cause rows are exactly the
  `fill_termination` calls made by the TTL sweep
  (`actor/executor.rs:1143-1290`).
- `build_event_log` (003) is prost-encoded BYTEA, filtered to
  state-machine events, and GC'd on terminal cleanup
  (`actor/build.rs:730`) plus a 24 h sweep
  (`housekeeping.rs:767`) — not a usable latency source.

Conclusion: interval (ii) is measurable end-to-end only for the
worker-report cause (observation and requeue are the same actor
turn) and measurable as "scheduler-side processing time" for the
other causes; interval (i) — the number OA1 actually sizes the
establishment slack against — has neither endpoint durably
timestamped on the rio side, and the k8s-side endpoint ages out in
≤600 s. A log join could only work in an environment that retains
debug-level controller and scheduler logs, which the named target
environment does not guarantee.

Committed option-(b) query (what (b) can measure today; per-cause
requeue/processing latency, NOT interval (i)):

```sql
-- per-cause attempt-terminal events at the scheduler (end-points of
-- interval (ii)); join key for any log-side start-point is
-- (executor_id, exec_id).
SELECT outcome_class,
       reporting_party,
       date_trunc('hour', recorded_at)            AS bucket,
       count(*)                                   AS n,
       percentile_cont(0.5) WITHIN GROUP (ORDER BY recorded_at - occurred_at)  AS p50_record_lag,
       percentile_cont(0.99) WITHIN GROUP (ORDER BY recorded_at - occurred_at) AS p99_record_lag
FROM drv_attempts
WHERE event_kind = 'attempt'
  AND outcome_class IN ('disconnected','executor_crash','backstop','infra','exempt_infra','timeout')
GROUP BY 1, 2, 3
ORDER BY 3, 1, 2;
```

Environment / population the baseline accumulates from (named so AD5
and no-go conditions 4/5 are evaluated against a stated population):
the EKS deployment described by `infra/eks` + `infra/helm/rio-build`
(the only standing non-VM environment), all Builder/Fetcher pools,
window = from instrument availability to the 0e cut; the
controller-outage arm is exercised at least once in the VM suite
(`nix/tests` lifecycle/chaos scenarios) and recorded alongside, per
the plan. A change of population at 0e is a recorded deviation.

Status: **escalated** (option-(a) authorization request open with
the campaign owner; decision due before 0b closes so the histograms
— if authorized — accumulate through 0c/0d). 0e-blocking via no-go
condition 5.

### OA2 — hung-node aggregation owner and shape (0e-blocking)

The ask to the controller-campaign owner is issued as part of this
0a engagement (same owner; the ordering entry added to
`controller-invariant-map.md` in this commit set is the venue): pick
the replacement signal shape for the multi-tenant stale-node signal
— L10 health reap, node conditions / NotReady-age, per-node
Job-deadline/pull-latency clustering, or an interim scheduler-side
ledger sweep over open attempts + spawn-ack node binding — and
either commit a landing slot no later than 1c or sign the accepted
1b→1d coverage gap with its bound and named compensating controls
(per design §5.2). Requested decision-by: 0e (target 2026-06-06), so
0e records a decision rather than opening the negotiation.
Status: open, 0e-blocking.

### OA3 — fetcher pull cardinality (data request)

Data needed: fetcher pool churn/cost (pod creations per FOD fetch,
fetch duration distribution vs pod cold-start, I-116 idle-exit rate
for fetchers). Source: existing pool/Job metrics + `drv_attempts`
fetcher-kind rows in the OA1 environment. Default absent data:
one-pull. Owner: campaign owner; due: before 0e (target 2026-06-06).
Status: data-pending.

### OA4 — BuildPhase fate (dashboard owner ping)

Ask issued to the dashboard owner (same person at present): drop
BuildPhase or keep it as a fire-and-forget unary in the replacement.
Inventory §1.11 records it as cosmetic (dashboard phase column
only). Due: 0e (target 2026-06-06). Status: open.

### OA5 — operator-facing fleet view and controls

Inventory of the surfaces that go blind for pull-mode pods at 1b:
`ListExecutors` / `DebugListExecutors` (admin/executors.rs, CLI),
the `workers_active` gauge, the dashboard fleet view, and the
operator controls `DrainExecutor` (per-executor drain / force-evict)
and the fleet-wide stop. 0e must record the open-attempts +
Job-census successor surface, what the dashboard loses (per-pod
heartbeat age), the sign-off owner, and the O1–O3 control
successors; the sign-off itself happens against the running
replacement at 1b. Owner: campaign owner + dashboard/operator owner.
Due: surface + owner recorded by 0e (target 2026-06-06). Status:
open.

### OA6 — forecast-spawn data query (0e-blocking, jointly owned with the controller campaign)

The 0e choice (a third `NotYetReady` pull outcome vs a ready filter
at the placeable gate / spawn pass) is data-driven. Data query
issued at 0a, due before 0d closes (target 2026-06-03), shared with
the controller-campaign owner:

- fraction of spawned Builder Jobs whose intent was ready=false at
  spawn (the §13b no-ready-filter path recorded above);
- how often such a pod registers before its drv becomes Ready, and
  the registration→Ready latency distribution;
- the I-116 idle-exit rate for those pods;
- the cold-start side: what a forecast-warmed, already-registered
  pod saves vs spawn+register (pod creation→registration latency).

Sources: `rio_scheduler_sla_forecast_dropped_total`,
`rio_controller_nodeclaim_forecast_hit_ewma` and the §13b SLI set
where they suffice; otherwise a documented log/DB join defined the
same way as the OA1 option-(b) instrument and committed alongside
this map before 0d closes. Owner: campaign owner (joint sign-off
with the controller campaign at 0e). Status: data-pending,
0e-blocking.
