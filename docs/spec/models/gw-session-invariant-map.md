# Gateway connection/session lifecycle invariant ↔ spec-rule map

Campaign artifact for `gw-session-formal` (Track B, round 2): verifying the
rio-gateway connection/channel/session/server lifecycle as built — accept,
auth, channel open, exec admission, protocol session, teardown, capacity
permits and gauges, deadlines and force-close, the three-stage drain.
The contract for this map is the approved campaign design
(`gw-session-formal-design.md`, DRAFT v2 §2–§6); the evidence base is the
corrected code inventory (`gw-session-inventory-code.md`) and the
calibration corpus (`gw-session-calibration-corpus.md`). The methodology is
the one proven on rio-lease, the log subsystem, retry, the controller, and
the executor session campaigns; the executable counterpart of this map is
the Stage-B model (`gwConnLifecycle.qnt` + its `gwTransportEnv`
environment module), which does not exist yet — Stage A is deliberately
model-free.

**Status: Stage A (spec audit) complete; Stage B (model + measurement
milestone) pending; Stage C (calibration) pending.**

Stage A added three new rules (`gw.conn.force-close`,
`gw.conn.send-deadline`, `gw.conn.accept-resilience`), amended and bumped
one (`gw.conn.cancel-on-disconnect+3`, the active_build_ids tracking
policy), closed the `gw.drain.three-stage` verify gap (marker on the
`wait_for_session_drain` drain-timeout unit test), and recorded the
operator-surface closed-world note in `gateway.typ`. The
`gw.conn.session-cap+2` window-pacing/exec-only-rejection clauses and the
`gw.conn.exit-status+3` ordering/grace clauses were audited and found
already present — no amendment needed there.

## Owner decisions binding this campaign (design checkpoint)

| Decision | Effect on this map |
|---|---|
| "Session" for capacity/autoscaling accounting = **guard-held** (permit + gauge + live-count released at pump end / finish, even while the proto task finishes its cancel loop) | S3/S4 are stated against guard-held; the W2 guard-vs-proto-task divergence is a stated bound, not a defect. |
| Conn-permit-at-accept is a **fixed fact** (probes/SYN-flood-with-completion can transiently hold conn permits with no auth-level signal) | Verified around, never prescribed against; exposure is a deployment-checklist alerting note (`errors_total{conn_cap}`). |
| SIGKILL is **outside** the verified envelope | Deployment-checklist territory (terminationGracePeriodSeconds ≥ drain budget; scheduler/store backstops); not a modeled action. |
| Exit-path STDERR writes (idle-timeout notice, version-too-old) stay **unfixed** | Below the model's abstraction; no spec rule was added that the code would violate. |

## Property ↔ rule map (scaffold — filled by Stage B)

Verdict legend (when filled): **COVERS** — the rule's normative sentence
states the invariant (or its load-bearing piece). **PARTIAL** — a piece is
stated; the missing piece is named. **GAP** — closed by a new `#r()` rule.
**CONTRADICTION** — code does not do what the rule says; recorded below,
never silently modeled around.

### Safety (S1–S20, design §3.1)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| S1 | ConnPermitConservation | — | — | — | pending Stage B |
| S2 | SessionPermitConservation | — | — | — | pending Stage B |
| S3 | GaugeAccuracy | — | — | — | pending Stage B |
| S4 | PerConnLiveCount | — | — | — | pending Stage B |
| S5 | NoSessionOutlivesConnection | — | — | — | pending Stage B |
| S6 | SingleExecPerChannel | — | — | — | pending Stage B |
| S7 | ChannelAccounting | — | — | — | pending Stage B |
| S8 | RejectReleasesCapacity | — | — | — | pending Stage B |
| S9 | SingleRelease | — | — | — | pending Stage B |
| S10 | CancelOnSessionEnd | — | — | — | pending Stage B |
| S11 | ForceCloseArmedSticky | — | — | — | pending Stage B |
| S12 | DecideImpliesArmed | — | — | — | pending Stage B |
| S13 | WindowPacedEgress | — | — | — | pending Stage B |
| S14 | RusshResidueBounded | — | — | — | pending Stage B |
| S15 | ReleaseBeforeCloseOut | — | — | — | pending Stage B |
| S16 | TrackedBuildPolicy | — | — | — | pending Stage B |
| S17 | DrainStageOrder | — | — | — | pending Stage B |
| S18 | ListenerSurvivesAcceptErrors | — | — | — | pending Stage B |
| S19 | CloseOutOrder | — | — | — | pending Stage B |
| S20 | GraceArmedExactlyWhenEmpty | — | — | — | pending Stage B |

### Permissiveness (P1–P6, design §3.2)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| P1 | MuxFanOutAdmitted | — | — | — | pending Stage B |
| P2 | SurvivesSessionCountZero | — | — | — | pending Stage B |
| P3 | CapRejectsExecOnly | — | — | — | pending Stage B |
| P4 | WithinGraceExecSurvives | — | — | — | pending Stage B |
| P5 | CompliantPeerNotForceClosed | — | — | — | pending Stage B |
| P6 | EstablishedSurviveAcceptStop | — | — | — | pending Stage B |

### Settlement (L1–L8, design §3.3 — armed-style state invariants)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| L1 | ConnReclaimArmed | — | — | — | pending Stage B |
| L2 | SendSettleArmed | — | — | — | pending Stage B |
| L3 | PreSessionOccupancyArmed | — | — | — | pending Stage B |
| L4 | DrainObligationsArmed | — | — | — | pending Stage B |
| L5 | GraceNeverKillsLiveSession | — | — | — | pending Stage B |
| L6 | RpcWaitDeadlineArmed | — | — | — | pending Stage B |
| L7 | UpstreamStreamReleasedOnExit | — | — | — | pending Stage B |
| L8 | SessionTaskQuiescence | — | — | — | pending Stage B |

## Environment assumptions

Named assumptions the model imports rather than verifies. Each is owned by
another campaign or by existing test coverage; a regression behind one of
these is NOT caught by any check this campaign adds.

| Assumption | What relies on it | Coverage / owner |
|---|---|---|
| **Scheduler orphan-watcher backstop**: a build whose gateway-side watcher is gone (skipped cancel on a handler panic, SIGKILL, or the non-wire removal heuristic guessing wrong) is auto-cancelled by the scheduler once it has no attached watcher (`r[sched.backstop.orphan-watcher]`, `rio-scheduler/src/actor/housekeeping.rs` 300 s sweep) | S10 (panic-regime weakening), S16 / the W10 disposition, the SIGKILL deployment-checklist line | Scheduler campaign; assumption named per property, never re-modeled here |
| **WatchBuild reconnect residual**: the in-opcode build-event-wait state has no deadline by design; the model assumes the WatchBuild reconnect loop is finitely bounded (MAX_RECONNECT = 10, capped backoff ≈ 111 s). A regression re-introducing an unbounded reconnect loop (the 553359e59 family, excluded from this corpus per design §8 Q6) would not be caught by this campaign's checks | The build-event-wait sub-state of the compound session node (§2a); L6 is scoped to rpc-wait only because of this | `test_build_paths_reconnect_exhausted_returns_failure` (non-vacuous since b6dc68834) and `test_reconnect_sends_first_event_sequence_not_zero`, `rio-gateway/tests/wire_opcodes/build.rs` (both `r[verify gw.reconnect.*]`) |
| **Scheduler stream silent forever with a live client**: a scheduler that accepts a build then keeps the BuildEvent stream open and silent indefinitely parks the session in build-event-wait until a terminal event, a transport-reap letter, or shutdown — an environment assumption, not a verified bound (the `process_build_events` select has no timeout arm, `rio-gateway/src/handler/build.rs`) | The §2a abstraction loss table; W5 occupancy bound | Recorded here; no test asserts the absence of a deadline (it is the design) |
| **ForceClose earliest-deadline-wins arithmetic**: the numeric "earliest wins / never moves later" half of S11 has no observable negation in a tick-free model | S11 (the model checks armed-stickiness and post-expiry gating only) | `fetch_min` implementation + the arm assertions in the stalled-send / window-starved unit tests (`rio-gateway/src/server/connection.rs`); a dedicated monotonicity unit test is an optional Stage B line item |

## Stage records

### Stage A — spec audit (this change set)

- New rules: `gw.conn.force-close` (S11/S12 normative home),
  `gw.conn.send-deadline` (S15/L2 budget half), `gw.conn.accept-resilience`
  (S18). Each landed with `r[impl]` markers at the named enforcement sites
  and `r[verify]` markers only on existing tests that genuinely exercise
  the behavior.
- Amendment: `gw.conn.cancel-on-disconnect+3` — the active_build_ids
  tracking policy (S16, the shipped non-wire-removal trade-off) is now
  normative; existing impl/verify annotations re-pointed after review; new
  impl marker on the removal-policy guard, new verify marker on the
  mid-opcode-disconnect regression test.
- `gw.drain.three-stage` verify gap closed at the `wait_for_session_drain`
  drain-timeout unit test (`rio-gateway/src/main.rs`). The S17/L4 model
  checks will carry their own markers at their `nix/quint.nix` wiring when
  they exist.
- Operator-surface closed-world note added to `gateway.typ` (non-normative).
- Audited and left unchanged: `gw.conn.session-cap+2` (window pacing +
  exec-only rejection already present), `gw.conn.exit-status+3` (close-out
  ordering + grace semantics already present).
- Contradictions found: none.

### Stage B — model + measurement milestone (pending)

Will add: the property ↔ rule verdicts above; the §3.4 candidate crosswalk
and OOM dispositions (GW-14, GW-15, GW-17, GW-21); the witness
pre-registration list (≈14–17, including the §2e cap-trigger reachability
witnesses); the pre-registered expected as-built falsifications (strict
no-inactivity L1 in `fault-degraded` — the W7 degraded tier; strict-S16 on
the W10 trace; S10-strict in the panic regime; L5 without the P4
carve-out); the B-measure milestone record (distinct states + wall clock
for `base` and the first fault regime) and the regime split decided from
it.

### Stage C — calibration (pending)

Will add: the calibration table (override module ↔ candidate ↔ falsified
property @ depth, trace-walk record), both directions for GW-2 / GW-13 /
GW-18, and the re-confirmed §4 window dispositions.
