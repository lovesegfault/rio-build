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
