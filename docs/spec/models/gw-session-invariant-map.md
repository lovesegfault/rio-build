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
the Stage-B model `gwConnLifecycle.qnt` (one module carrying the russh
transport-environment variables of design §2b inline, plus seven regime
instantiation modules, a fallback measurement module, and the eight
Stage-C per-family restricted-alphabet modules the wired exhaustive
checks run on).

**Status: Phase 0 complete — campaign CLOSED (verify-only; no Phase 1
needed).** Stage A (spec audit) complete; Stage B (model + measurement
milestone) complete — all 34 properties encoded, witnesses and
pre-registered falsification probes confirmed reachable/falsifying, the
B-measure recorded below with the regime-split recommendation; Stage C
check-set selection + CI wiring + witnesses/probes + the §4 model-first
evidence complete (31 permanent `nix/quint.nix` checks, verdicts in the
Stage-C record below); Stage-C calibration complete (override modules
under `calibration/gw-f*.qnt`, all 17 encodable candidates falsified in
the violation direction, the three T halves falsified for
GW-2/GW-13/GW-18, 9 permanent `quint-gw-calib-*` checks, verdicts in the
calibration table below). Phase-0 acceptance verdict and go/no-go
recommendation: the calibration stage record. The verify-only closure
decision, the counter-signatures, the corrections they forced, the
accepted residuals, and the deployment-checklist deltas: the campaign
close-out at the end of this document.

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

## Property ↔ rule map (Stage B: encoded; Stage C: checks wired)

Verdict legend: **COVERS** — the rule's normative sentence states the
invariant (or its load-bearing piece). **PARTIAL** — a piece is stated; the
missing piece is named. **GAP** — closed by a new `#r()` rule.
**CONTRADICTION** — code does not do what the rule says; recorded below,
never silently modeled around. Stage-C status for every row below:
**encoded — holds (Stage-C check set)** — the predicate is asserted (as part
of `allInvariants`) by every wired per-family exhaustive check and holds in
all of them; which family check carries each property's *content* (and which
full-regime interleavings are therefore not exhaustively explored) is the
Stage-C record's coverage table below. Calibration verdicts (which
properties fall when each historical guard is reverted) are in the
calibration table of the Stage-C calibration record.

Model predicate names are the lowercased property ids (e.g.
`s1ConnPermitConservation`, `l1ConnReclaimArmed`); all 34 are conjoined as
`allInvariants`, which every regime check asserts. "Regimes" lists where
the property's content is actually exercised (it is asserted everywhere).

### Safety (S1–S20, design §3.1)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| S1 | ConnPermitConservation | `gw.conn.cap`, `gw.conn.real-connection-marker` | all; over-cap clause exercised in cap | `canReachOverCapAuthTorn` (cap regime) | encoded — holds (Stage-C check set) |
| S2 | SessionPermitConservation | `gw.conn.session-cap+2` | all | `canReachSessionCapExecRejected` | encoded — holds (Stage-C check set) |
| S3 | GaugeAccuracy | `gw.conn.real-connection-marker`, `gw.conn.session-cap+2` | all | `canReachServerSideEndingReleasesEarly` | encoded — holds (Stage-C check set) |
| S4 | PerConnLiveCount | `gw.conn.exit-status+3` (grace/live-count pairing) | all; panic term exercised in fault-degraded | `canReachMuxTouchZeroThenExec` | encoded — holds (Stage-C check set) |
| S5 | NoSessionOutlivesConnection | `gw.conn.cancel-on-disconnect+3`, `gw.conn.lifecycle` | all | `canReachVanishReclaimedDesigned` (teardown cascade) | encoded — holds (Stage-C check set) |
| S6 | SingleExecPerChannel | `gw.conn.exec-request`, `gw.conn.per-channel-state` | all | `canReachMuxTouchZeroThenExec` (re-exec on a fresh channel) | encoded — holds (Stage-C check set) |
| S7 | ChannelAccounting | `gw.conn.channel-limit+4`, `gw.conn.per-channel-state` | all; bound exercised in fault-transport | `canReachBurstHitsBound`, `canReachForgedCloseIgnored` | encoded — holds (Stage-C check set) |
| S8 | RejectReleasesCapacity | `gw.conn.exec-request` | content falsifiable only in fault-degraded (panic splitter) | panic-letter run (Stage C) | encoded — holds (Stage-C check set) |
| S9 | SingleRelease | `gw.conn.session-cap+2`, `gw.conn.cap` | all | shares S2/S3 witnesses | encoded — holds (Stage-C check set) |
| S10 | CancelOnSessionEnd | `gw.conn.cancel-on-disconnect+3` | all; carve-out exercised in fault-degraded | `canReachDrainExpiryCancel`, `canReachVanishReclaimedDesigned` | encoded — holds (Stage-C check set) |
| S11 | ForceCloseArmedSticky | `gw.conn.force-close` | all (latches); fetch_min arithmetic stays with the unit tests | `canReachStallArmsForceClose` | encoded — holds (Stage-C check set) |
| S12 | DecideImpliesArmed | `gw.conn.force-close` | all | `canReachGraceFiresOnIdleConn` | encoded — holds (Stage-C check set) |
| S13 | WindowPacedEgress | `gw.conn.session-cap+2` (window-pacing clause) | all; contended in fault-transport | `canReachStallArmsForceClose` | encoded — holds (Stage-C check set) |
| S14 | RusshResidueBounded | `gw.conn.channel-limit+4`, `gw.conn.channel-types` | fault-transport | `canReachOverBoundOpenTerminates` | encoded — holds (Stage-C check set) |
| S15 | ReleaseBeforeCloseOut | `gw.conn.send-deadline`, `gw.conn.exit-status+3` | all | `canReachServerSideEndingReleasesEarly` | encoded — holds (Stage-C check set) |
| S16 | TrackedBuildPolicy | `gw.conn.cancel-on-disconnect+3` (tracking-policy clause) | fault-upstream | `canReachNonWireRemovesTracked`; strict probe `s16StrictTerminalOnly` | encoded — holds (Stage-C check set) |
| S17 | DrainStageOrder | `gw.drain.three-stage`, `gw.conn.session-drain` | fault-drain | `canReachDrainExpiryCancel` | encoded — holds (Stage-C check set) |
| S18 | ListenerSurvivesAcceptErrors | `gw.conn.accept-resilience` | fault-upstream (EMFILE letter) | latch-only (no dedicated reach flag; the letter is a no-op by construction) | encoded — holds (Stage-C check set) |
| S19 | CloseOutOrder | `gw.conn.exit-status+3` | all | `canReachCloseOutCompletesInOrder` | encoded — holds (Stage-C check set) |
| S20 | GraceArmedExactlyWhenEmpty | `gw.conn.exit-status+3` (grace clause) | all | `canReachGraceFiresOnIdleConn`, `canReachExecWithinGraceSurvives` | encoded — holds (Stage-C check set) |

### Permissiveness (P1–P6, design §3.2)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| P1 | MuxFanOutAdmitted | `gw.conn.session-cap+2` (exec-only rejection) | all (latch) | `canReachMuxTouchZeroThenExec` | encoded — holds (Stage-C check set) |
| P2 | SurvivesSessionCountZero | `gw.conn.exit-status+3` (grace clause) | all (latch) | `canReachGraceFiresOnIdleConn` | encoded — holds (Stage-C check set) |
| P3 | CapRejectsExecOnly | `gw.conn.session-cap+2` | all (latch) | `canReachSessionCapExecRejected` | encoded — holds (Stage-C check set) |
| P4 | WithinGraceExecSurvives | `gw.conn.exit-status+3` (grace clause) | all (latch) | `canReachExecWithinGraceSurvives` | encoded — holds (Stage-C check set) |
| P5 | CompliantPeerNotForceClosed | `gw.conn.force-close`, `gw.conn.send-deadline`, `gw.conn.keepalive+2` | fault-peer-occupancy / fault-peer-transport (conn B is the compliant control) | base-regime compliant named run (Stage C); GW-2 T-override is the falsifier | encoded — holds (Stage-C check set) |
| P6 | EstablishedSurviveAcceptStop | `gw.drain.three-stage` | fault-drain (latch) | `canReachDrainExpiryCancel` | encoded — holds (Stage-C check set) |

### Settlement (L1–L8, design §3.3 — armed-style state invariants)

| # | Property | Rule(s) | Regime(s) | Witness / named run | Verdict |
|---|---|---|---|---|---|
| L1 | ConnReclaimArmed | `gw.conn.force-close`, `gw.conn.keepalive+2`, `gw.conn.exit-status+3` | all; degraded tier in fault-degraded | `canReachKexParkedReclaimed`, `canReachVanishReclaimedDesigned`, `canReachParkedReclaimedByInactivity`; strict probe `l1StrictNoInactivity` | encoded — holds (Stage-C check set) |
| L2 | SendSettleArmed | `gw.conn.send-deadline` | all; contended in fault-transport | `canReachStallArmsForceClose` | encoded — holds (Stage-C check set) |
| L3 | PreSessionOccupancyArmed | `gw.handshake.timeout`, `gw.conn.lifecycle` | all; firing exercised in fault-peer-occupancy | GW-3 calibration (Stage C); occupancy-regime reclamation runs | encoded — holds (Stage-C check set) |
| L4 | DrainObligationsArmed | `gw.drain.three-stage`, `gw.conn.session-drain` | fault-drain | `canReachDrainExpiryCancel`; full drain named run (Stage C) | encoded — holds (Stage-C check set) |
| L5 | GraceNeverKillsLiveSession | `gw.conn.exit-status+3` (grace clause) | all (latch) | strict probe `l5StrictNoCarveOut` shows the documented race is reachable | encoded — holds (Stage-C check set) |
| L6 | RpcWaitDeadlineArmed | `gw.conn.cancel-on-disconnect+3` (timeout-wrapped upstream calls), `gw.store.transient-retry` | all; deadline fire in fault-upstream | reachability via `canReachNonWireRemovesTracked` (passes through rpc-wait) | encoded — holds (Stage-C check set) |
| L7 | UpstreamStreamReleasedOnExit | `gw.conn.cancel-on-disconnect+3` | all | `canReachNonWireRemovesTracked` (stream held then released) | encoded — holds (Stage-C check set) |
| L8 | SessionTaskQuiescence | `gw.conn.lifecycle`, `gw.conn.per-channel-state` | all | `canReachMuxSiblingQuiescence` | encoded — holds (Stage-C check set) |

## Calibration-candidate crosswalk (design §3.4) and OOM dispositions

Direction: V = violation-on-guard-removal calibration; T = trace-admission
(permissiveness) calibration; OOM = out-of-model in Phase 0 with a
pre-registered disposition. This table is the Stage-B pre-registration of
what each override must reproduce; the override modules and the measured
falsified-property @ depth verdicts are in the Stage-C calibration record's
calibration table below.

| Candidate | Repr. commit | Property(ies) targeted | Direction / disposition |
|---|---|---|---|
| GW-1 | 443670d43 | S2, S3, L2 | V |
| GW-2 | 51123b2be / 861888876 | S15, L2 + T half: P5 (override removes the §2d peer-class condition from the send/close-out budget) | V + T |
| GW-3 | 79912eda0 (+0101be9c6, aec2521a3, a207ee15c) | L1, L3, S20 | V |
| GW-4 | 1c46d9781 (+2f11bb8f5, 5a50b3f70) | S12, S11, L1 | V |
| GW-5 | 9739aca65 | S7 | V |
| GW-6 | 719f99809 | S7 | V |
| GW-7 | 2cd23d940 / eef1fbd8d | S14 | V |
| GW-8 | deea04bbc | S13 | V |
| GW-9 | bbf30cc7f (+0f476d6f0, a207ee15c, 765671437, ebe9faaa2) | S10, S16 | V |
| GW-10 | 2e54649c3 | S10 | V |
| GW-11 | 1cca275b4 (+9a18f756a) | L8, S3 | V |
| GW-12 | a2fcb8aa0 (+8e7c6e724) | S1 | V |
| GW-13 | 765671437 / d653222cf / a6a5a77b9 | S17, L4, P6 | V + T |
| GW-14 | fac58554b | — (wire-position integrity, below the data-plane abstraction) | OOM: the `wire_opcodes` upload tests (`opcodes_write.rs`, incl. the fac58554b regression test) + `functional/nar_roundtrip.rs` (corrected at close-out: previously attributed to "golden-conformance upload tests" — that suite has no upload tests) |
| GW-15 | cb64e2913 (+d2e84cf31) | — (per-entry upload bounds) | OOM: store campaign (store half) + the `wire_opcodes` multi-entry upload tests and `functional/nar_roundtrip.rs` (gateway half; corrected at close-out: previously attributed to "golden/integration multi-entry upload tests") |
| GW-16 | 755f49744 (+0f5408e11, d617bf3e5) | L6 | V |
| GW-17 | 09807689f (+a8b5c1974) | — (credential freshness; needs a clock) | OOM: `session_jwt_token_refreshes_per_access` + I-129 regression tests |
| GW-18 | 9d80038cb | S2 + P1, P2, P3 | V + T |
| GW-19 | 9b693441f | S18 | V |
| GW-20 | 24bd41581 | S19 | V (no T half — the client-hang consequence is environment) |
| GW-21 | ab83a6a10 (+ddc043d3b, 5c9ebf72e) | — (per-session ingress memory budget, below the data-plane abstraction) | OOM: drv_cache cap unit tests; named Phase-2 Kani candidate |

Mechanisms in the model with no historical fix (original verification —
a failed Stage-C calibration there is not misread as a model gap): H10
exec-after-rejected-exec, H11 data-on-half-closed (header-documented
no-op), H12 non-exec channel requests (folded into the open-without-exec
letter), the W3 race bound (P4/L5 carve-out), the panic regime, S20's
arm/disarm pairing.

## Witness pre-registration (design §5; encoded as `canReach*` predicates)

Seventeen expect-violation witnesses are encoded in the model (§6b of the
.qnt); Stage C wired each as a `mkQuintWitnessCheck`
(`quint-gw-lifecycle-witness-*`, `nix/quint.nix`) against the FULL-alphabet
regime module listed — witness checks stop at the first violation, so the
unrestricted alphabets stay affordable there. All seventeen were measured
violating (reachable) at wiring time; per-check times are in the Stage-C
record. A witness that stops violating means the regime's invariants have
gone vacuous for that scenario.

| Witness predicate | Regime | Pins |
|---|---|---|
| `canReachStallArmsForceClose` | fault-peer-transport | a stalled send arms the force-close (C14/GW-2) |
| `canReachServerSideEndingReleasesEarly` | base | server-side ending releases capacity with the channel still open (W2/guard-held) |
| `canReachGraceFiresOnIdleConn` | base | the grace fires on an idle authenticated connection |
| `canReachExecWithinGraceSurvives` | base | an exec admitted while the grace is armed disarms it (P4) |
| `canReachKexParkedReclaimed` | fault-peer-occupancy | a KEX-parked pre-auth connection is reclaimed |
| `canReachForgedCloseIgnored` | fault-peer-transport | a forged close is ignored (GW-5) |
| `canReachOverBoundOpenTerminates` | fault-peer-transport | an over-bound open terminates the connection |
| `canReachBurstHitsBound` | fault-peer-transport | a burst of opens reaches the per-connection bound (GW-6) |
| `canReachSessionCapExecRejected` | base | a session-cap exec rejection with the channel open succeeded (P3) |
| `canReachOverCapAuthTorn` | cap | an over-cap connection reaches its first auth callback permit-less and is torn down (S1/GW-12) |
| `canReachDrainExpiryCancel` | fault-drain | a drain-expiry shutdown-token cancel fires |
| `canReachMuxTouchZeroThenExec` | base | a mux connection touches zero sessions and execs again |
| `canReachMuxSiblingQuiescence` | base | a session reaches tasks==0 with a sibling session live (L8/GW-11) |
| `canReachVanishReclaimedDesigned` | fault-peer-transport | a half-open vanish is reclaimed by a designed transport letter (W5) |
| `canReachParkedReclaimedByInactivity` | fault-degraded | a parked-write connection is reclaimed via the inactivity letter (W7 degraded tier) |
| `canReachNonWireRemovesTracked` | fault-upstream | a non-Wire stream error removes a tracked build (S16 shipped policy) |
| `canReachCloseOutCompletesInOrder` | base | a close-out completes in order (S19) |

## Pre-registered expected as-built falsifications (design §3/§4/§6)

Encoded as named predicates outside `allInvariants`; Stage C wired each as
an expect-violation check (`quint-gw-lifecycle-falsification-*`,
`nix/quint.nix`) on the full regime listed. All four falsify exactly as
predicted; the captured counterexample traces are summarized in the Stage-C
record (they are the model-first evidence behind the §4 dispositions). A
probe that stops falsifying after a code or model change is a finding, not
a pass.

| Probe | Regime | Expected falsifying trace | Why it is acceptable |
|---|---|---|---|
| `l1StrictNoInactivity` | fault-degraded | parked write whose force-close was armed only after the park, with TCP_USER_TIMEOUT setsockopt failed: only the inactivity backstop remains | the W7 degraded tier (~1 h); accepted-with-rationale per design §4, with the deployment-checklist observable on the setsockopt warn line |
| `s16StrictTerminalOnly` | fault-upstream | non-Wire stream error removes a tracked build with no processed terminal outcome | the W10 orphan-watcher-bounded leak (≤ ~300 s); the owner sign-off the design requests |
| `s10StrictIncludingPanic` | fault-degraded | a handler panic skips the cancel loop for a tracked build | P12; scheduler orphan-watcher backstop (named environment assumption) |
| `l5StrictNoCarveOut` | base | an exec admitted between the grace re-check and its disconnect is disconnected by that cycle | the documented W3/P4 race window; bounded by one grace period |

## Environment assumptions

Named assumptions the model imports rather than verifies. Each is owned by
another campaign or by existing test coverage; a regression behind one of
these is NOT caught by any check this campaign adds.

| Assumption | What relies on it | Coverage / owner |
|---|---|---|
| **Scheduler orphan-watcher backstop**: a build whose gateway-side watcher is gone (skipped cancel on a handler panic, SIGKILL, or the non-wire removal heuristic guessing wrong) is auto-cancelled by the scheduler once it has no attached watcher (`r[sched.backstop.orphan-watcher]`, `rio-scheduler/src/actor/housekeeping.rs` 300 s sweep) | S10 (panic-regime weakening), S16 / the W10 disposition, the SIGKILL deployment-checklist line | Scheduler campaign; assumption named per property, never re-modeled here |
| **WatchBuild reconnect residual**: the in-opcode build-event-wait state has no deadline by design; the model assumes the WatchBuild reconnect loop is finitely bounded (MAX_RECONNECT = 10, capped backoff ≈ 111 s). A regression re-introducing an unbounded reconnect loop (the 553359e59 family, excluded from this corpus per design §8 Q6) would not be caught by this campaign's checks. *C4 update (2026-05-31): the resumability layer behind this residual (since_sequence cursor, event-log replay, dedup) was deleted in favor of snapshot-first WatchBuild attach — the unverified surface shrank to the reconnect loop + snapshot consumption. The boundedness assumption itself is unchanged (the loop and its MAX_RECONNECT/backoff bounds survive the deletion)* | The build-event-wait sub-state of the compound session node (§2a); L6 is scoped to rpc-wait only because of this | `test_build_paths_reconnect_exhausted_returns_failure` (non-vacuous since b6dc68834; survives C4 unchanged) and the snapshot-resync tests `test_build_paths_reconnect_snapshot_resumes_state` + `test_build_paths_reconnect_terminal_snapshot_short_circuits`, `rio-gateway/tests/wire_opcodes/build.rs` (`r[verify gw.reconnect.backoff+2]` / `r[verify gw.reconnect.snapshot-resync]`). *`test_reconnect_sends_first_event_sequence_not_zero` was deleted with the since_sequence machinery it verified (C4)* |
| **Scheduler stream silent forever with a live client**: a scheduler that accepts a build then keeps the BuildEvent stream open and silent indefinitely parks the session in build-event-wait until a terminal event, a transport-reap letter, or shutdown — an environment assumption, not a verified bound (the `process_build_events` select has no timeout arm, `rio-gateway/src/handler/build.rs`) | The §2a abstraction loss table; W5 occupancy bound | Recorded here; no test asserts the absence of a deadline (it is the design) |
| **ForceClose earliest-deadline-wins arithmetic**: the numeric "earliest wins / never moves later" half of S11 has no observable negation in a tick-free model | S11 (the model checks armed-stickiness and post-expiry gating only) | `fetch_min` implementation (`ForceClose::arm_within`, `rio-gateway/src/server/mod.rs`) + the arm assertions in the stalled-send / window-starved unit tests (`rio-gateway/src/server/connection.rs`); a dedicated monotonicity unit test is an optional Stage B line item (citation corrected at close-out: the implementation lives in `mod.rs`, the tests in `connection.rs`) |

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

### Stage B — model + measurement milestone (this change set)

**Artifact.** `docs/spec/models/gwConnLifecycle.qnt` — one core module
(≈2.0k lines at the Stage-B commit; corrected at close-out from "2.2k")
+ seven §2e regime instantiation modules (`Base`, `Cap`,
`FaultOccupancy`, `FaultTransport`, `FaultUpstream`, `FaultDrain`,
`FaultDegraded`) + a `BaseSlim` measurement module for the §2e fallback
(2). Five state variables (`conns`, `chans`, `srv`, `viol`, `reach`)
carrying ~30 connection fields (ConnStage, permit/gauge latches, the eight
§2b russh-environment variables, peer-class latches, explicit bookkeeping
counters), ~7 channel fields and ~21 compound-session fields per channel
slot, 66 named transition actions plus `init`/`step`, 57 named predicates
(34 §3 properties + `boundsOK` + `allInvariants` + 17 witnesses + 4 strict
probes), and 2 named runs (the P5 compliant lifecycle, the GW-2 stall
wedge). One-line property statements live in the model's doc comments and
design §3; the property ↔ rule rows above carry the rule and witness
linkage. `quint typecheck` green.

**B-measure (state count / wall clock vs the §2e budget).**

| Run | Result |
|---|---|
| Random-simulation smoke, all 7 regimes, `allInvariants`, 400–500 traces × 80 steps | no violation in any regime |
| Witness reachability | 17/17 `canReach*` witnesses confirmed reachable (15 by random search, the stalled-send witness by the deterministic `stallArmsForceCloseRun`, the W7 inactivity witness by 30k-trace random search in `fault-degraded`) |
| Pre-registered falsification probes | all four falsify as predicted: `l1StrictNoInactivity` (fault-degraded), `s16StrictTerminalOnly` (fault-upstream), `s10StrictIncludingPanic` (fault-degraded), `l5StrictNoCarveOut` (base) |
| Named runs | `compliantLifecycleRun` (base) and `stallArmsForceCloseRun` (fault-transport) pass under `quint test` |
| **TLC exhaustive, `base`, full §2e bounds** (2 conns × 2 channels each) | **did not complete within the §2e budget**: stopped at 629 s wall on a 192-worker host with ≈59.7 M distinct states found (1.65 G generated) at BFS depth 24 and ≈34.7 M states still queued (frontier still growing), no invariant violation in the explored prefix. For scale, the executor-session base (the §2e re-baselining anchor) was 1.39 M distinct / 172 s at 32 workers. |
| **TLC exhaustive, `baseSlim`** (fallback (2): conn B limited to 1 channel slot) | also did not complete: stopped at ≈13.5 min on the same host with ≈62.3 M distinct states (2.0 G generated) at BFS depth 27 and ≈30.2 M queued, no violation in the explored prefix. Fallback (2) alone shrinks the space by only ≈2× at equal depth — not enough to bring a regime check inside the budget. (One earlier `baseSlim` attempt died ≈41 s in with no TLC diagnostic — a worker-JVM infrastructure failure on the shared host, not a model error; the rerun reproduced nothing.) |

**Regime-split decision (per the §2e pre-committed fallback order).** The
state space at the §2e structural bounds is in the high tens-to-hundreds
of millions of states per regime — an order of magnitude past what the
5 min soft / 15 min hard per-check budget can absorb even on a large
builder, and fallback (2) alone does not close the gap. Stage C must
therefore decide the wiring with the owner before adding `nix/quint.nix`
checks, choosing among (in the §2e pre-committed order, then the §5
escape hatch):

1. Apply fallback (2) (`B_CHANNEL_SLOTS = 1`, already a model constant)
   plus fallback (3)/(4): per-regime restricted alphabets or per-family
   override modules as the *wired* checks, keeping the full-alphabet
   regimes as manual `packages.*` targets (the `nix/quint.nix` header's
   own guidance for genuinely large models). Witness and
   expect-violation checks stay on the full regimes — they stop at first
   violation, and every §6b/§6c probe was found well inside depth ≈20.
2. With owner sign-off (they touch §2e environment bounds, not just
   policy caps): reduce `WINDOW_CREDITS` and/or `QUEUE_DEPTH` from 2 to 1
   and/or collapse the exit-reason classes — each is a real fidelity loss
   and must be checked against the §5 calibration requirements before
   adoption.
3. Accept one long-running exhaustive check per regime outside the merge
   gate (a `packages.gw-lifecycle-exhaustive` aggregate) and wire only
   the witness/probe/named-run checks plus a bounded-depth Apalache check
   into `checks.*`. This keeps the merge gate fast but weakens the
   "exhaustive in CI" guarantee the round-1 campaigns set as precedent —
   an owner decision, not a default.

   The mux witnesses that need two channels on one connection
   (`canReachMuxTouchZeroThenExec`, `canReachMuxSiblingQuiescence`) are
   exercised on conn A, so they stay reachable under fallback (2).

**Encoding decisions where the design is under-specified (and deviations).**

- **TCP_USER_TIMEOUT enabling class.** §2d's table says the kernel timeout
  never fires "against an ACKing zero-window peer", but §4 W7's own
  designed ~305 s tier is TCP_USER_TIMEOUT reclaiming a zero-window
  (ACKing) wedge. Encoding the literal ACK exclusion makes the §4 tier
  unreachable and leaves the fault-peer-transport wedge state with no
  designed reclamation (an unlisted L1 falsification). The model therefore
  keys the reap on the peer-class form: it fires against vanished peers
  and against transport-withholding peers with an undeliverable write
  outstanding, never against the compliant class. Recorded as a deviation
  from the table's letter, faithful to its permissiveness intent.
- **Force-close enforcement vs a parked write.** §2a treats enforcement as
  available once armed; taken literally that makes the W7 degraded tier
  (the pre-registered `l1StrictNoInactivity` falsification) unreachable.
  The model keeps one bit of park ordering (`parkWake`: was the force-close
  armed when the inline write parked) — enforcement reaches a parked write
  only if the wake was registered. The leftover-wake sliver itself stays
  out of model exactly as §2a scopes it.
- **Stage-deadline fire guards** (pre-auth, handshake, opcode-idle) are
  keyed on "peer not in the compliant class" rather than the §2d row's
  occupancy-letter enumeration, so transport-withholding-only peers (e.g.
  a pre-auth vanish) are still reapable by their stage deadline; the
  compliant-class exclusion — the table's load-bearing half — is exact.
- **Empty-grace fire** carries no compliant-class exclusion (per the §2d
  table): an idle compliant connection is grace-disconnected exactly as in
  production. P5 is therefore encoded over the budget/reap/cap latches and
  the budget-armed force-close sites, not over the grace.
- **L1 armed-path semantics:** stage deadlines and graces count when
  *armed* (their fire guards protect permissiveness); reap letters and
  force-close enforcement count when *enabled*. The empty grace re-arms at
  guard release (S20), which is what keeps L1 designed-only outside the
  degraded regime.
- **Process exit** is modeled as enabled only at full quiescence (no
  counted connections, no live proto tasks); the real CANCEL_GRACE
  hard-exit with stragglers is wall-clock behavior below the tick-free
  abstraction (and SIGKILL is outside the envelope per the owner
  decision).
- **L4** is encoded as the exit-quiescence clause; the per-stage
  "shutdown path enabled" obligations hold by construction in this
  encoding (the fired token enables every proto's observe action) and are
  pinned by the drain named run in Stage C.
- **Egress decoupling:** window credits pace data sends (GW-8 surface);
  the handle queue carries close-out/disconnect deliveries and the
  write-park mechanics (GW-2/W7 surface). Their cross-coupling adds state
  without adding property content.
- **Channel slots are use-once** (a closed slot is not reused); over-bound
  and non-session open attempts need no free slot, so the §2e attempt
  letters stay enabled at the structural ceiling.
- **Exec-gate panic splitter** lands after `session_admitted` (live-count
  incremented, grace disarmed, write half taken) and before the gauge
  bump/guard creation — the only fallible/awaitable site in the gate per
  the W1/W2 review evidence — so S3 stays exact and S4 carries the
  documented panic term.
- **S18** has no dedicated reachability witness (the EMFILE letter is a
  no-op by construction); its content is the latch plus the GW-19
  calibration in Stage C.
- **`B_CHANNEL_SLOTS`** was added as a model constant (not in the design's
  §2e constants table) purely to make fallback (2) a per-regime
  instantiation choice instead of a model edit.

**Stage-B exit-gate status.** All §3 properties hold in every regime under
random search and in the explored exhaustive prefix of `base`; no unlisted
falsification surfaced; the four pre-registered falsifications all
materialize; every witness is reachable. The outstanding gate item is
exhaustive per-regime runs inside the budget, which is a Stage-C wiring
concern under the regime-split decision above.

### Stage C — check-set selection, CI wiring, witnesses and probes (this change set)

**Verification subject / churn pin.** The verification subject is this
worktree's base — `formal-sprint` at cccb4d778 ("docs(rio-scheduler): drop
stale references to the deleted dispatch helpers"), the same HEAD the code
inventory was mined against. The calibration corpus was mined earlier, from
`origin/main` at dfe9a5569 (the corpus's own provenance line); between
dfe9a5569 and cccb4d778 rio-gateway changed by 1,313 insertions /
112 deletions across 14 files — the four round-1 LogService cutover commits
(808db8976, f226a12f7, 1a1bcef29, 82060a2a2), which touched the modeled
lifecycle files (handler/build.rs, main.rs, server/connection.rs,
server/mod.rs, session.rs) and added handler/log_tail.rs. None of the four
is a lifecycle FIX/HARDENING commit, so no corpus family, candidate, or
acceptance verdict rests on the difference. *(Corrected at close-out: this
paragraph originally claimed cccb4d778 was "the same HEAD the inventory and
calibration corpus were mined against" and that "rio-gateway has not changed
since the corpus/inventory were mined" — true for the inventory, wrong for
the corpus; the impact is confined to this provenance statement.)*
`git log cccb4d778..HEAD -- rio-gateway/` contains only this campaign's own
Stage-A commits — recorded at wiring time under their pre-rebase hashes
(66901f86d, af9f7c482, 805c8ab4a, c088d7af2); on the integrated branch the
same four commits are fd92fb873, a9f3ee2bc, a99314b20, 56fc66ea4 (corrected
at close-out: a reader replaying the recorded command sees the post-rebase
hashes) — and their rio-gateway diffs are comment-only (`r[impl]`/`r[verify]`
markers and doc comments — 31 insertions, 5 deletions, no executable line
touched).

**Check-set decision (the §2e fallback ladder, applied).** The Stage-B
B-measure stands: the full-alphabet `base` regime is ≈59.7 M distinct states
at depth 24 and still growing at 629 s on a 192-worker host, and fallback (2)
(`BaseSlim`) shrinks it only ≈2×. A Stage-C re-measurement confirmed the same
holds for any restriction that keeps a rich connection-level alphabet
multiplied by two concurrent full session lifecycles (first-cut per-family
modules that kept conn B's session and both conn-A sessions were still
heading past 20 M distinct at 8+ minutes). Per the §2e pre-committed order
the wired exhaustive set is therefore **fallbacks (3)/(4): eight per-family
restricted-alphabet modules** (`gwConnLifecycleFam*` in the model, wired as
`quint-gw-lifecycle-fam-*`), each keeping every §2e structural/environment
bound (2 conns, 2 channel slots on conn A, queue depth 2, window credits 2,
residue ceiling 2, ≤1 tracked build) and its regime's policy-cap values,
applying fallback (2) (`B_CHANNEL_SLOTS = 1`), and restricting only the
alphabet: conn B plays the connection-level legitimate letters (accept,
banner, auth, grace, reaps — so cross-connection permit/gauge arithmetic is
asserted everywhere), conn A plays the family's letters, and famWedge /
famDegraded additionally confine the egress/panic session to conn A channel
0 (their corpus shapes are single-session). **No environment bound was
reduced below the design's registered floors and nothing moved out of the
merge gate** — the full-alphabet regimes carry the witness, falsification
and named-run checks (TLC stops at the first violation there), and the
calibration overrides (next change set) instantiate the full regimes as
before. The witnesses and probes were *not* moved onto restricted modules.

**What the restricted exhaustive set does NOT cover** (deferred full-regime
exhaustive coverage; the named restricted check that carries each
property's content instead is in the table below): cross-family
interleavings (e.g. occupancy withholding × window stalls × builds in one
trace); conn B running its own full session lifecycle concurrently with
conn A's (B-side sessions appear only in famHostile's cap-rejection arm);
two concurrent *egress/panic* sessions on one connection (famWedge /
famDegraded are single-session; multi-session interleavings are exhausted
in famAdmission / famUpstream / famDrain / famHostile, which carry the
mux/sibling content); and the "everything legitimate at once" base-regime
proof, which remains witness-and-named-run-pinned only. A future
full-regime exhaustive run stays available as a manual `packages.*`-style
target if the owner wants to spend builder-hours on it; it is not in the
merge gate.

**Wired checks, scopes, measured cost vs budget.** Budget per §2e: ≈5 min
soft target, ≤15 min hard cap per check. Measurements below are wall-clock
of the same `quint verify --backend=tlc` invocation the harness runs, at 32
TLC workers on a shared 192-core builder (concurrent with 3–5 other TLC
instances, i.e. conservative), including ≈20–30 s of conversion/JVM
overhead; states = TLC distinct states at completion (exhaustive) or at
first violation (witnesses/probes).

| Check (`checks.x86_64-linux.*`) | Module / scope | Verdict | Distinct states | Wall |
|---|---|---|---|---|
| `quint-gw-lifecycle-fam-preauth` | FamPreauth (conn-level establishment/occupancy/grace/reaps) | HOLDS exhaustive @ scope | 1,183 | 31 s |
| `quint-gw-lifecycle-fam-admission` | FamAdmission (admission vs grace, stage deadlines, conn-A sessions ×2) | HOLDS exhaustive @ scope | 136,103 | 34 s |
| `quint-gw-lifecycle-fam-cap` | FamCap (MAX_CONNECTIONS=1, over-cap auth teardown) | HOLDS exhaustive @ scope | 260,061 | 39 s |
| `quint-gw-lifecycle-fam-wedge` | FamWedge (egress pacing/budgets/parks/vanish, single egress session) | HOLDS exhaustive @ scope | 66,059 | 34 s |
| `quint-gw-lifecycle-fam-hostile` | FamHostile (hostile opens/closes, channel accounting, P3 rejection) | HOLDS exhaustive @ scope | 968,151 | 69 s |
| `quint-gw-lifecycle-fam-upstream` | FamUpstream (rpc-wait/build tracking/cancel/close-out; also asserts `w10TriggerAbsent`) | HOLDS exhaustive @ scope | 3,253,999 | 246 s |
| `quint-gw-lifecycle-fam-drain` | FamDrain (three-stage drain, drain-expiry cancel, exit quiescence) | HOLDS exhaustive @ scope | 3,337,595 | 216 s |
| `quint-gw-lifecycle-fam-degraded` | FamDegraded (panic splitter, sockopt-fail, parks, inactivity tier; single session) | HOLDS exhaustive @ scope | 225,953 | 26 s |
| `quint-gw-lifecycle-witness-*` (17) | full regimes per the witness table above | all 17 violate (reachable) | 87–130,940 each | 21–38 s each |
| `quint-gw-lifecycle-falsification-l5-no-carve-out` | base | expect-violation confirmed (depth 11) | 168 | 30 s |
| `quint-gw-lifecycle-falsification-s16-terminal-only` | fault-upstream | expect-violation confirmed (depth 11) | 1,078 | 30 s |
| `quint-gw-lifecycle-falsification-s10-panic` | fault-degraded | expect-violation confirmed (depth 11) | 3,732 | 24 s |
| `quint-gw-lifecycle-falsification-l1-no-inactivity` | fault-degraded | expect-violation confirmed (depth 15) | 430,271 | 46 s |
| `quint-gw-lifecycle-run-compliant-peer` | base, `compliantLifecycleRun` | run passes (`quint test`) | — | <1 s + harness overhead |
| `quint-gw-lifecycle-run-stall-wedge` | fault-transport, `stallArmsForceCloseRun` | run passes (`quint test`) | — | <1 s + harness overhead |

Every wired check is comfortably inside the ≤15 min hard cap with headroom
(worst exhaustive check ≈4 min measured under contention); the seventeen
witnesses and four probes are each well under a minute. The first-cut
family modules that did NOT fit (a combined establishment family and a
two-session wedge family, both >20 M states and still growing at 8–10 min)
were replaced by the famPreauth/famAdmission split and the single-session
famWedge/famDegraded pin before wiring — that re-shaping is an alphabet
decision, not a bound change.

**Per-property coverage in the restricted set** (which family check carries
each property's content; every check asserts all 34 via `allInvariants`):
S1 famPreauth + famCap (over-cap clause); S2/S3/S9 all (global arithmetic),
content-deepest in famAdmission/famUpstream; S4 famAdmission (+ panic term
famDegraded); S5 all teardown families (famWedge/famUpstream/famDrain);
S6 famAdmission/famHostile; S7/S14 famHostile; S8 famDegraded;
S10 famUpstream/famDrain (panic carve-out famDegraded); S11/S12 famPreauth
(grace/pre-auth arms) + famWedge (stall arm) + famDegraded (park ordering);
S13 famWedge; S15 famWedge/famUpstream; S16 famUpstream; S17 famDrain;
S18 famUpstream; S19 famWedge/famUpstream; S20 famPreauth/famAdmission;
P1/P2/P4 famAdmission; P3 famHostile; P5 famPreauth/famWedge (+ the
compliant named run); P6 famDrain; L1 famPreauth (occupancy) + famWedge
(transport) + famDegraded (inactivity tier); L2 famWedge; L3 famAdmission;
L4 famDrain; L5 famAdmission; L6/L7 famUpstream; L8 famUpstream/famWedge.

**Pre-registered falsification probes — trace summaries (the §4
model-first evidence).**

- `l5StrictNoCarveOut` (base, depth 11): accept → auth (grace arms) → open
  → grace re-check passes → **exec admitted after the re-check** → grace
  fires and disconnects with `live_sessions = 1`. The documented W3/P4
  post-check admit race, bounded by one grace cycle; the carved L5 and P4
  hold everywhere.
- `s16StrictTerminalOnly` (fault-upstream, depth 11): exec → handshake →
  opcode → rpc-wait → SubmitBuild accepted (tracked, BuildEvent stream
  held) → **non-Wire stream error removes the build with no processed
  terminal outcome** (the shipped policy; S16-as-shipped holds). The
  W10 evidence paragraph below carries the decision-rule answer.
- `s10StrictIncludingPanic` (fault-degraded, depth 11): same prefix to a
  tracked build → **handler panic unwinds the proto task**: exit reason
  panic, no CancelBuild attempt, stream dropped with the unwound frame.
  The scheduler orphan-watcher backstop is the named environment
  assumption; non-panic S10 holds everywhere.
- `l1StrictNoInactivity` (fault-degraded, depth 15): auth → stop-draining
  peer → TCP_USER_TIMEOUT setsockopt fails → exec → handshake timeout →
  guard released, close-out queued → channel closed (close-out budget
  disarmed) → **queue write parks with the force-close not yet armed**
  (no enforcement wake) → grace fires (force-close armed only after the
  park). Reachable reclamation paths in that state: none designed — only
  the inactivity backstop (INACTIVITY_AVAILABLE) re-arms L1. That is the
  W7 degraded tier (~1 h bound) exactly as pre-registered; full L1 holds
  in fault-degraded, and in every designed regime L1 holds without the
  inactivity disjunct.

**W10 evidence (the §4 decision rule, answered).** The trigger condition —
a build leaving the tracked set with no processed terminal outcome, no
CancelBuild attempt, and the upstream-stream-held bit still set after
session exit — does **not** appear in any reachable trace. Evidence:
(a) the falsifying strict-S16 trace above shows the stream-held bit
dropping in the same transition as the removal (the BuildEvent stream is a
handler-local; the handler frame that removes the build is the frame that
drops the stream), with the session still live; (b) the new
`w10TriggerAbsent` invariant (the trigger stated as "never reachable") is
asserted by the exhaustive `quint-gw-lifecycle-fam-upstream` check and
holds over that family's full space (3.25 M states), alongside L7 holding
in every check; (c) no witness or probe run produced a state matching the
trigger. Per the design's decision rule the Phase-1 gateway-side
removal-predicate change is therefore **not** triggered; what remains for
the owner to sign off is the orphan-watcher-bounded leak itself (the
strict-S16 trace: a non-Wire stream failure with a dead or absent client
orphans a running build for up to ~300 s until
`rio-scheduler/src/actor/housekeeping.rs`'s sweep cancels it) — the
recommendation stays accepted-with-rationale, and the two W10
pinning/assumption-validation tests stay on the Phase-1 list regardless.

**W5 evidence (occupancy bound, confirmed).** The
`canReachVanishReclaimedDesigned` witness violates in `fault-transport`
(depth 8): a half-open vanish (no FIN/RST, keepalives unanswered) is
reclaimed by a designed transport-reap letter — keepalive-exhaustion /
TCP_USER_TIMEOUT — in a regime whose alphabet contains no inactivity
letter, and L1/L2 (armed form) hold across that regime's witness searches
and across famWedge exhaustively. The finding stays an occupancy bound,
not a violation: until the ≈300–330 s designed reap fires, the vanished
peer pins one conn permit (of 1000), one session permit (of 4096), the
gauge slots, the per-connection duplex buffers (≈512 KiB) and possibly a
NAR assembly buffer, plus the in-flight scheduler/builder work — per
occurrence, exactly the §4 W5 figure. No code change; the W8 memory
amplification stays a deployment-checklist headroom note.

**W2 evidence (divergence visible and bounded).** The
`canReachServerSideEndingReleasesEarly` witness violates in `base`
(depth 11): a server-side session ending releases the permit/gauge/
live-count (guard drop) while the channel is still open and the connection
live — the guard-held accounting definition doing exactly what the owner
decision says it should. The divergence window (proto task still in its
cancel loop after the guard released) is structurally bounded: the only
state a diverged proto task can occupy is `cancelling`, whose exit action
(`cancelLoopDone`, the bounded-by-DEFAULT_GRPC_TIMEOUT cancel loop) is
enabled unconditionally — no peer letter, no upstream reply and no timer
is required for it to retire — and S5/L8 hold in every check, so the
divergence cannot survive connection end or session-task quiescence. The
~30 s worst-case figure remains the deployment-checklist note about
`channels_active` momentarily under-counting during mass disconnects.

**Validation.** `quint typecheck` green on the extended model;
`tracey query validate` clean (the new `r[verify]` markers at the
`nix/quint.nix` wiring points resolve against the Stage-A rules);
`treefmt` clean; `nix eval` lists all 31 new checks under
`checks.x86_64-linux`; the wired checks were each built once via the
harness (remote builder) before commit — exhaustive checks pass, witness
and falsification checks report their expected violations, run checks
pass.

### Stage C (continued) — calibration and the Phase-0 acceptance verdict (this change set)

**Artifacts.** Nine override files under `docs/spec/models/calibration/`
(`gw-f1-capacity.qnt`, `gw-f2-occupancy.qnt`, `gw-f3-force-close.qnt`,
`gw-f4-channel-accounting.qnt`, `gw-f5-egress.qnt`, `gw-f6-teardown.qnt`,
`gw-f7-drain.qnt`, `gw-f9-upstream-deadline.qnt`, `gw-f10-accept.qnt`),
one per falsifying corpus family, carrying 25 override modules (the 17
encodable candidates, with separate modules where a candidate's
constituent commits revert distinct mechanisms, plus the three
trace-admission halves) and 12 as-built baseline modules. Each override
module instantiates the `gwConnLifecycle` core (the same
import-and-override pattern as the round-1 calibration corpus), defines
the PRE-FIX variant of one action (the behavior the named historical fix
removed), and exposes it through `calibStep`; the as-built violation and
reachability latches are never reverted, so a falsification means the
as-built invariant set re-finds that bug class.

**Scope and baselines (a documented deviation from the §5 letter).** §5
planned the overrides on the full-alphabet regime modules; the Stage-B
B-measure showed those do not exhaust inside the per-check budget, so the
falsification direction would have been fine (TLC stops at the first
violation) but the matching baseline ("with the guard restored the
violation disappears at the same scope") would not have been provable.
Each override's `calibStep` therefore restricts the alphabet to its
family's letters — the same single-rich-dimension principle as the wired
`gwConnLifecycleFam*` checks — and the as-built baseline is an explicit
`baselineStep` in the same file (same constants, same alphabet shape, the
as-built action(s) restored) run to exhaustion. No §2e structural or
environment bound is reduced, and the §2e policy-cap table's
pre-registered override values are used where required (GW-6/GW-7 at
`MAX_CHANNELS_PER_CONN = 1`, GW-12 at `MAX_CONNECTIONS = 1`). Verdict
format: property @ depth (states generated / distinct); wall-clocks are
from the same serial `quint verify --backend=tlc` protocol as the Stage-C
check measurements (24–32 workers on the shared builder, ≈20–30 s of
JVM/conversion overhead included in every figure).

**Calibration table — violation (V) direction.** Every encodable
candidate falsifies a predicted property; every baseline holds.

| Candidate | Override module (file) | Constants dialed | Predicted | Verdict |
|---|---|---|---|---|
| GW-1 | `gwCalibF1ServerSideKeepsGuard` (f1) | — | S2, S3, L2 | **FALSIFIES L2** @ 14 (757,893/24,405); S15 falls on the same trace; S2/S3 do not fall (note 1) |
| GW-2 (release ordering) | `gwCalibF5ReleaseAfterCloseOut` (f5) | — | S15, L2 | **FALSIFIES S15** @ 9 (5,228/408) |
| GW-2 (send budget) | `gwCalibF5NoSendBudget` (f5) | — | S15, L2 | **FALSIFIES L2** @ 10 (6,600/479) |
| GW-3 (representative) | `gwCalibF2OpenDisarmsGrace` (f2) | — | L1, L3, S20 | **FALSIFIES S20** @ 6 (506/57) and **L1** @ 6 (465/51) |
| GW-3 (a207ee15c constituent) | `gwCalibF2NoHandshakeDeadline` (f2) | — | L3 | **FALSIFIES L3** @ 8 (663/67) |
| GW-4 | `gwCalibF3DecideWithoutArm` (f3) | — | S12, S11, L1 | **FALSIFIES S12** @ 5 (443/80); the 2f11bb8f5 S11/L1 constituent: note 2 |
| GW-5 | `gwCalibF4ForgedCloseDecrements` (f4) | — | S7 | **FALSIFIES S7** @ 3 (97/27) |
| GW-6 | `gwCalibF4BoundOnSessionsMap` (f4) | `MAX_CHANNELS_PER_CONN = 1` | S7 | **FALSIFIES S7** @ 6 (1,469/110) |
| GW-7 | `gwCalibF4RefusePolitely` (f4) | `MAX_CHANNELS_PER_CONN = 1` | S14 | **FALSIFIES S14** @ 4 (733/68) |
| GW-8 | `gwCalibF5UnwindowedSend` (f5) | — | S13 | **FALSIFIES S13** @ 7 (922/117) |
| GW-9 (tracking cleared early) | `gwCalibF6WireErrorRemovesTracked` (f6) | — | S10, S16 | **FALSIFIES S16** @ 11 (6,966/403) |
| GW-9 (exit edge skips cancel) | `gwCalibF6ExitEdgeSkipsCancel` (f6) | — | S10 | **FALSIFIES S10** @ 12 (14,698/723) |
| GW-10 | `gwCalibF6AbortWinsOverCancel` (f6) | — | S10 | **FALSIFIES S10** @ 11 (4,599/277) |
| GW-11 (task quiescence) | `gwCalibF6FinishSkipsPumpReap` (f6) | — | L8 | **FALSIFIES L8** @ 14 (77,880/3,536) |
| GW-11 (gauge, 9a18f756a) | `gwCalibF6ConnDropLeaksGauge` (f6) | — | S3 | **FALSIFIES S3** @ 7 (965/100) |
| GW-12 | `gwCalibF1AuthRejectSkipsGate` (f1) | `MAX_CONNECTIONS = 1` | S1 | **FALSIFIES S1** @ 4 (297/56) |
| GW-13 (drain-expiry cancel) | `gwCalibF7DrainExpiryNoCancel` (f7) | — | S17, L4, P6 | **FALSIFIES L4** @ 14 (408,403/11,417) |
| GW-13 (stage collapse) | `gwCalibF7SigtermSkipsStages` (f7) | — | S17 | **FALSIFIES S17** @ 7 (811/58) |
| GW-16 | `gwCalibF9RpcWaitNoDeadline` (f9) | — | L6 | **FALSIFIES L6** @ 10 (1,592/81) |
| GW-18 (global cap, V) | `gwCalibF1NoGlobalSessionCap` (f1) | — | S2 | **FALSIFIES S2** @ 13 (159,819/8,164) |
| GW-19 | `gwCalibF10AcceptErrorFatal` (f10) | — | S18 | **FALSIFIES S18** @ 3 (82/24) (note 3) |
| GW-20 | `gwCalibF6CloseOutSkipsExitStatus` (f6) | — | S19 | **FALSIFIES S19** @ 10 (3,467/222) |

**Calibration table — trace-admission (T) direction** (the over-tight
pre-fix guards re-introduced per §2d/§5; the named permissiveness
property must fall).

| Candidate | Override module | Predicted | Verdict |
|---|---|---|---|
| GW-2 | `gwCalibF5BudgetIgnoresPeerClass` (f5) | P5 | **FALSIFIES P5** @ 10 (32,253/1,567) |
| GW-13 | `gwCalibF7AcceptStopKillsEstablished` (f7) | P6 | **FALSIFIES P6** @ 8 (1,327/85) |
| GW-18 | `gwCalibF1PerConnCapRefusals` (f1) | P1, P2, P3 | **FALSIFIES P1** @ 8 (6,538/556), **P2** @ 7 (471/56), **P3** @ 7 (466/53) |

**Baselines (as-built `baselineStep`, exhaustive, every one HOLDS
`allInvariants` at its override's scope).**

| Baseline module | Baselines | Distinct states | Wall |
|---|---|---|---|
| `gwCalibF1AsBuilt` | GW-1 | 12,107,117 | 480 s |
| `gwCalibF1AsBuiltSpread` | GW-18 (V and T) | 199,002 | 34 s |
| `gwCalibF1AsBuiltCap1` | GW-12 | 1,119 | 30 s |
| `gwCalibF2AsBuilt` | GW-3 (both arms) | 1,350,115 | 99 s |
| `gwCalibF3AsBuilt` | GW-4 | 1,183 | 38 s |
| `gwCalibF4AsBuilt` | GW-5 | 52,870 | 44 s |
| `gwCalibF4AsBuiltChanCap1` | GW-6, GW-7 | 18,445 | 45 s |
| `gwCalibF5AsBuilt` | GW-2 (V and T), GW-8 | 66,059 | 44 s |
| `gwCalibF6AsBuilt` | GW-9, GW-10, GW-11, GW-20 | 3,253,999 | 224 s |
| `gwCalibF7AsBuilt` | GW-13 (V and T) | 3,337,595 | 228 s |
| `gwCalibF9AsBuilt` | GW-16 | 1,204,287 | 120 s |
| `gwCalibF10AsBuilt` | GW-19 | 49,315 | 44 s |

The baseline state counts double as a structural cross-check: where a
baseline's alphabet is reachability-equivalent to a wired family check's
(`gwCalibF3AsBuilt` ↔ famPreauth at 1,183, `gwCalibF5AsBuilt` ↔ famWedge
at 66,059, `gwCalibF6AsBuilt` ↔ famUpstream at 3,253,999) the distinct
counts match exactly.

**Trace walks and dispositions** (every falsifying trace was read
step-by-step against the pre-fix code path before being recorded as a
reproduction; the notes the tables reference).

- GW-1: accept → auth → exec → handshake timeout (server-side ending) →
  pre-fix pump exit keeps the guard → close-out completes → capacity
  still held with no budget, no enforceable force-close and no pump exit
  left — the 443670d43 exec-then-silent shape (permit/gauge keyed on the
  sessions map, which only client action removes). S15 falls on the same
  trace (the release-before-close-out ordering rule post-dates the
  pre-fix world). **Note 1 — predicted S2/S3 do not fall:** the pre-fix
  accounting is internally consistent (the gauge still counts the held
  guard, the permit is still held by a live guard), so the conservation
  forms cannot distinguish it; the defect is a settlement failure, which
  is exactly the predicted-and-falsified L2. The S2/S3 half of the
  prediction is carried within F1 by the representatives that do break
  conservation (GW-12 → S1, GW-18 → S2). Not a crosswalk contradiction —
  the mapped property set falls through L2.
- GW-2 (release ordering): client EOF → proto exits → pre-fix pump exit
  enters the close-out with the guard still held — S15 at the first
  close-out state (51123b2be: release waited on the peer-parkable handle
  queue). (budget): a withhold-window peer parks the send with no
  HANDLE_SEND_TIMEOUT armed — L2 falls with capacity held toward a
  transport-withholding peer and nothing armed (861888876's unbounded
  handle.data() await). (T): a compliant peer's window simply runs out
  and the budget — stripped of the §2d peer-class condition — fires the
  wedge response against it; P5 falls via budgetFiredOnCompliant /
  fcByBudget (the structural form of "budget too small for normal
  congestion", the 5 s close-out budget 861888876 replaced).
- GW-3 (representative): an authenticated connection opens a channel and
  never execs; the pre-fix open disarms the empty grace, so the
  connection sits with no stage deadline, no grace, no budget and no
  designed reap enabled against a keepalive-answering peer — S20's
  must-be-armed clause and L1's armed-path disjunction fall in the same
  state (79912eda0). The 0101be9c6 constituent (grace armed only at
  channel_close) is the same wrong-emptiness-signal mechanism at model
  resolution, and aec2521a3 (no pre-banner bound) is the same L1
  armed-path loss one stage earlier — both covered by this module's
  falsification rather than separate overrides. (a207ee15c constituent):
  an exec admitted with no handshake deadline armed — L3 falls at the
  admission.
- GW-4: reject-only auth attempts → the pre-auth deadline's phase-2
  decision queues the polite disconnect WITHOUT arming the force-close —
  S12 falls at the decision point (1c46d9781). **Note 2 — the 2f11bb8f5
  constituent (enforcement restricted to the read path; TCP_USER_TIMEOUT
  added as the kernel backstop):** at the model's armed-style resolution
  this revert is the loss of the parked-write enforcement wake
  (`parkWake`) plus the kernel reap letter, and its violating state is
  exactly the one the wired
  `quint-gw-lifecycle-falsification-l1-no-inactivity` probe already
  exhibits (L1-strict falls in fault-degraded when both are absent and
  only the inactivity backstop remains). Recorded as a
  structural-argument disposition — the bug class is expressible and
  permanently pinned by that probe — rather than duplicated as a third
  F3 override. The 5a50b3f70 constituent's enforcement half is the same
  decide-implies-arm mechanism this module reverts; its occupancy half
  is GW-3's surface (famPreauth carries the as-built KEX-parked
  reclamation witness).
- GW-5: a forged/duplicate CHANNEL_CLOSE on a connection with no
  accepted channel decrements `open_channels` below the accepted-set
  size (9739aca65's unguarded decrement); the same module carries the
  exec-on-never-accepted-channel arm of that fix.
- GW-6: at the override's per-connection cap of 1, two opens both pass
  the pre-fix bound check (it reads the exec'd-sessions map, which is
  empty before any exec) — the accepted-open count exceeds the bound
  (719f99809's burst-open-then-exec, caught at the open transition).
- GW-7: two politely-refused opens leave residue 2 > f = 1 — the pre-fix
  `Ok(false)` path retains russh channel-table state and the connection
  survives (2cd23d940 / eef1fbd8d). The pre-auth position of the opens
  in the shortest counterexample is the as-built attempt letter's own
  enabling condition, not something the override loosened.
- GW-8: the first response send goes through the pre-fix non-window path
  (`Handle::data`) — russh-side pending occupancy above zero with no
  credit consumed; S13 falls immediately (deea04bbc). L2 deliberately
  does not fall here (pre-fix sends complete instantly into the
  unbounded buffer), exactly as the §3.4 row predicts.
- GW-9 (tracking): a tracked build's stream fails with a Wire-class
  error and the pre-fix handler removes it from `active_build_ids`
  anyway — S16 falls (bbf30cc7f: the disconnect cleanup loop later finds
  an empty set). (exit edge): a non-panic exit edge skips the cancel
  loop with a build still tracked — S10 falls (the 0f476d6f0 /
  a207ee15c case-completeness shape).
- GW-10: channel close hard-aborts the protocol task (the pre-fix
  ChannelSession::Drop abort) — the cancel obligation is destroyed with
  it; S10 falls. GW-9's exit-edge arm and GW-10 falsify the same
  property through different reverted mechanisms (a missing call site
  vs an abort racing the cancel loop); this is not the §1 simplification
  trigger (which needs the same property at the same depth across all
  regimes plus a revert that falsifies nothing).
- GW-11 (quiescence): the close-out completes but the pre-fix finish
  awaits the client pump instead of reaping it — L8 falls with the pump
  still live after the finish (1cca275b4). (gauge): a connection drop
  tears down a still-held SessionGuard but the pre-fix path never
  decrements `channels_active` (the scattered explicit decrement sites
  missed the abnormal exits) — S3 falls; S2 stays true on the same trace
  (the permit release is correct), so the falsification is attributable
  to the gauge alone (9a18f756a).
- GW-12: a reject-only auth callback skips mark_real_connection /
  ensure_permit — a live connection past the auth layer that is neither
  counted nor being torn down (a2fcb8aa0). The found counterexample is
  the uncounted half; the permit-less-not-torn-down half violates the
  same invariant's other conjunct deeper in the same module (the §2e
  cap-1 constants keep it reachable).
- GW-13 (expiry cancel): SIGTERM → accept-stop → drain expiry with a
  build parked in the build-event wait; the pre-fix token observation
  (opcode-read points only) never reaches it and the CANCEL_GRACE hard
  exit fires with the build still tracked and never cancelled — L4
  falls at the exit (765671437). (stage collapse): SIGTERM with a live
  session takes the process straight down — S17 falls via the
  stage-order latch and the same state exits with live proto tasks
  (d653222cf / a6a5a77b9). (T): accept-stop tears down an established
  connection with a live session — P6 falls (the pre-drain rollout
  behavior).
- GW-16: an upstream RPC await issued without a deadline — L6 falls at
  the first un-deadlined rpc-wait entry (755f49744).
- GW-18 (V): with no global session semaphore, sessions spread across
  both connections exceed `MAX_SESSIONS` — S2 falls at the third
  concurrent guard (9d80038cb's safety half). (T): the
  per-connection-cap world refuses a second exec below the global cap
  (P1), refuses the channel open itself (P3), and disconnects the
  connection the instant its session count touches zero (P2) — all
  three permissiveness latches fall (9d80038cb's permissiveness half).
- GW-19: a transient accept error treated as fatal — S18 falls via the
  listener-death latch. **Note 3:** S18's content is latch-shaped (as
  the witness table already records — the as-built letter is a no-op by
  construction), so the override is necessarily a latch-setter; the
  pinned counterexample exits the process with a live connection up, so
  the structural consequence (the corpus's "all live sessions aborted",
  the L4 form) is visible in the same trace (9b693441f).
- GW-20: the close-out ladder starts at eof — S19's exit-status-first
  ordering falls immediately (24bd41581). V-only per the §3.4 row (the
  client-side hang is environment, not modeled).

**Permanent CI artifacts (this change set).** Nine expect-violation
calibration checks (`quint-gw-calib-*`, `nix/quint.nix`), one per
falsifying corpus family — the round-1 proportion (the representative
with the most plausible regression path and a cheap state space; every
check stops at its first violation, so each is well under a minute of
checker time at the falsification-table costs above):
`f1-server-side-release` (GW-1 → L2), `f2-open-disarms-grace` (GW-3 →
S20), `f3-decide-without-arm` (GW-4 → S12), `f4-forged-close-decrement`
(GW-5 → S7), `f5-release-after-close-out` (GW-2 → S15),
`f6-exit-edge-skips-cancel` (GW-9 → S10), `f7-drain-expiry-no-cancel`
(GW-13 → L4), `f9-rpc-wait-no-deadline` (GW-16 → L6),
`f10-accept-error-fatal` (GW-19 → S18). Aggregate addition to a fully
uncached gate run ≈ 5–7 minutes of builder time, inside the §5 estimate.
Each was built once via the harness before commit and reports its
expected violation. Harness finding: the conversion request for the two
largest override modules (`gwCalibF1ServerSideKeepsGuard`,
`gwCalibF6ExitEdgeSkipsCancel` — a rich connection alphabet multiplied
by the in-session machinery) OOMs the harness's default 4 GiB Apalache
server, so `mkQuintWitnessCheck` gained a `serverHeapMb` parameter
(default unchanged and hash-stable for every existing check; the two
heavy checks pass 8 GiB, validated locally). The remaining override
modules, the baseline modules and the three T-direction runs stay as
documented manual targets, re-runnable with the command in each file's
header.

**Acceptance verdict (the §5 criterion, per corpus family).**
"Calibrated" = the family falsifies its predicted property through at
least one trace-walked representative, both directions where a T half
exists, with the as-built baseline holding at the same scope.

| Family | Verdict | Evidence |
|---|---|---|
| F1 capacity conservation | **calibrated** | GW-1 (V: L2), GW-12 (V: S1), GW-18 (V: S2; T: P1/P2/P3); GW-2's release-ordering member also lands in F1 via S15 |
| F2 progress-bounded occupancy | **calibrated** | GW-3 (V: S20 + L1; constituent arm L3) |
| F3 decision-to-enforcement | **calibrated** | GW-4 (V: S12); the 2f11bb8f5 constituent dispositioned by structural argument onto the wired l1-no-inactivity probe (note 2) |
| F4 channel/session bookkeeping integrity | **calibrated** | GW-5 (V: S7), GW-6 (V: S7), GW-7 (V: S14) |
| F5 egress flow control / bounded queues | **calibrated** | GW-2 (V: S15, L2; T: P5), GW-8 (V: S13) |
| F6 teardown obligations | **calibrated** | GW-9 (V: S16, S10), GW-10 (V: S10), GW-11 (V: L8, S3), GW-20 (V: S19) |
| F7 drain/shutdown ordering | **calibrated** | GW-13 (V: L4, S17; T: P6) |
| F8 upload bounding & wire-position integrity | **pre-registered out-of-model (test-only)** | GW-14: the `wire_opcodes` upload tests (`opcodes_write.rs` — `test_add_multiple_streaming_early_ok_preserves_wire_position` is the fac58554b regression test, plus the multi-entry batch/mixed permissiveness tests) and `functional/nar_roundtrip.rs`; GW-15: store-side half owned by the store campaign, gateway permissiveness half by the same `wire_opcodes` multi-entry tests + `functional/nar_roundtrip.rs` (owner decision §8 Q5). Corrected at close-out: both halves were previously attributed to "golden-conformance" / "golden/integration" upload tests (wording inherited from design §3.4) — the golden-conformance suite has no upload tests; the dispositions themselves are unchanged |
| F9 anti-hang deadlines on upstream waits | **calibrated** | GW-16 (V: L6); the budget-sizing members stay non-candidates per the corpus |
| F10 accept-path availability | **calibrated** | GW-19 (V: S18) |
| F11 session credential freshness | **pre-registered out-of-model (test-only)** | GW-17: `session_jwt_token_refreshes_per_access` + the I-129 regression tests (no clock in the model; owner decision §8 Q5) |
| F12 result-integrity vs store | **out of corpus (pre-registered exclusion)** | not a lifecycle-machine family (corpus §3, design §1 out-of-scope table); coverage: the 85118ecdf / cb3f6bfbb output-verification paths and their build-result tests; a separate data-integrity model would own it if commissioned |
| F13 adjacent state machines (scheduler-watch reconnect, STDERR framing, startup/readiness) | **out of corpus (pre-registered exclusion, §8 Q6)** | reconnect boundedness: `test_build_paths_reconnect_exhausted_returns_failure` + the C4 snapshot-resync tests (`test_build_paths_reconnect_snapshot_resumes_state`, `test_build_paths_reconnect_terminal_snapshot_short_circuits`) — see the WatchBuild environment-assumption row above for the C4 deletion note; STDERR framing: wire/golden conformance tests; startup/readiness: the vm-lifecycle scenarios |
| F14 per-session ingress memory budget | **pre-registered out-of-model (test-only; named Phase-2 Kani candidate)** | GW-21: drv_cache cap unit tests (`MAX_TRANSITIVE_INPUTS`, `insert_drv_bounded`, the occupancy-aware gate tests) |

No family is NOT MET: every encodable family (F1–F7, F9, F10) falsifies
its predicted property through at least one trace-walked representative
with the baseline holding at the same scope; the three T halves
(GW-2/GW-13/GW-18) falsify their named permissiveness properties; the
four OOM candidates (GW-14/15/17/21) carry their pre-registered
dispositions. No falsification contradicted the §3.4 crosswalk (the one
partial-prediction case, GW-1's S2/S3, falls through the predicted L2
and is documented in note 1); no unlisted falsification of the as-built
model surfaced (every baseline holds `allInvariants`); there is no
stop-and-report item.

**§4 window dispositions.** Unchanged by calibration: the W2 / W5 / W7 /
W10 model-first evidence recorded in the Stage-C wiring record above
stands (the calibration overrides revert pre-fix variants, they do not
touch the as-built dispositions), and no calibration trace contradicted
any of the four recommendations.

**Phase-0 go/no-go (the §7 gate, stated honestly).** The Stage-A and
Stage-B exit gates are met and recorded above; the Stage-C exit gate is
met by this record (every encodable family falsifies through a
trace-walked representative or carries its pre-registered disposition;
the §4 verdicts re-confirmed). The calibration table shows the model
expresses the bug classes of F1–F7, F9 and F10 — the §7 go condition.
What Phase 0 does NOT establish, recorded as the explicit residue of a
GO: (a) the deferred full-regime exhaustive coverage listed in the
Stage-C check-set record (cross-family interleavings, conn-B concurrent
full sessions, two concurrent egress/panic sessions, the base-regime
"everything legitimate at once" proof) remains witness/named-run-pinned
only; (b) the four OOM families stay on test-only coverage; (c) the
environment-assumption rows above (scheduler orphan watcher, WatchBuild
reconnect boundedness, the silent-stream assumption, fetch_min
monotonicity) are imported, not verified; (d) the §4 dispositions still
need the owner's sign-off — they are recommendations with model
evidence, not decisions this campaign can make *(collected: the
2026-05-30 Phase-0 checkpoint ratified the W2/W5/W7/W10 dispositions
with their recorded bounds — see the campaign close-out below)*.
**Recommendation: GO** —
proceed to Phase 1 with the small scope §7 already names (the two W10
pinning/assumption-validation tests; the optional W7 hardening and
`errors_total{setsockopt}` label only if the owner asks; new finds
budgeted at 1–3 small fixes), conditional on that owner sign-off. If the
owner instead stops after Phase 0, the campaign is still independently
valuable per the design: the spec rules, the model, the 31 + 9 permanent
checks and this calibration record stand on their own.

**Validation.** `quint typecheck` green on all nine override files;
every falsification and baseline run above executed via the same
`quint verify --backend=tlc` invocation the harness uses (the only
non-serial exception — two F1 baseline attempts that ran concurrently
against one Apalache server — produced tool errors, not verdicts, and
was redone serially; concurrent conversions against one server are not
trustworthy, which is also why the harness keeps one server per
sandbox); `tracey query validate` clean (the calibration checks
deliberately carry no markers, same policy as the witness checks);
`treefmt` clean; `nix eval` lists the nine `quint-gw-calib-*` checks
under `checks.x86_64-linux`; each wired check was built once via the
harness before commit (seven on the remote builder at the default
server heap, the two heavy ones locally at 8 GiB after the
`serverHeapMb` fix) and reports its expected violation.

## Campaign close-out — gateway connection/session lifecycle (verify-only)

The gw-session-formal campaign (Track B, round 2) is complete and
CLOSED at Phase 0, on the design's "stop after Phase 0" arm: a
verify-only campaign with no Phase 1. This section is the
campaign-level record, in the same shape as the round-1 close-outs
(retry, executor, log); the per-stage evidence lives in the stage
records above, the introducing commits, and the CI transcripts. The
counter-signatures below are the round-2 close-out discipline's
independent verification of this map's own claims; the corrections they
forced have been applied in place above, each with a "(corrected at
close-out: ...)" note.

**The verdict.** The as-built gateway connection/channel/session/server
lifecycle is correct at the model's resolution — **zero as-built
defects, zero code changes needed**. Every §3 property (20 safety, 6
permissiveness, 8 settlement) holds exhaustively over every wired
per-family scope; the only reachable violations are the four
pre-registered documented windows (W3/P4 admit race, W10 orphan bound,
P12 panic carve-out, W7 degraded tier), each falsifying exactly as
predicted and each accepted with a recorded bound; no unlisted
falsification surfaced in any exhaustive run, witness search, or
calibration baseline; and the calibration record proves the invariant
set re-finds all 17 encodable historical bug classes (plus the three
permissiveness halves) when their fixes are reverted. Where round-1
campaigns found and fixed as-built defects (retry's D1–D4, the
executor's `1bbad1ee7`, closure-evidence's C3/L3), this campaign's
Phase-0 product is a clean bill of health plus the permanent
verification stack that keeps it honest: the Stage-A spec rules, the
model, the 40 wired CI checks, this map, and the deployment-checklist
deltas below.

**Owner decisions closing the campaign.** The 2026-05-29 design
checkpoint bound the campaign to seven Track-B decisions (B1 guard-held
session accounting, B2 panics-in-envelope/SIGKILL-out, B3
scheduler-side cancel as a named assumption, B4 conn-permit-at-accept
as a fixed fact, B5 GW-14/15/17/21 test-only, B6 corpus exclusions
ratified, B7 STDERR exit-path writes left unfixed); the owner-decision
table near the top of this map carries the four that shape properties
directly, and the other three are carried in the panic-regime
properties, the environment-assumption rows, and the acceptance table's
§8 Q5/Q6 citations. The 2026-05-30 Phase-0 checkpoint ratified the
W2/W5/W7/W10 window dispositions with their recorded bounds (closing
the go/no-go's clause-(d) sign-off above) and closed the campaign
verify-only: the W10 decision rule did not trigger (`w10TriggerAbsent`
holds over fam-upstream's 3.25 M states), the optional W7 hardening and
`errors_total{setsockopt}` label were not commissioned, the 1–3
small-fix budget for new finds went unspent, and the two W10
pinning/assumption-validation tests that were parked on the Phase-1
list land with no Phase 1 — i.e. they do not exist; W10's coverage is
the wired falsification probe plus the scheduler orphan-watcher
environment assumption, exactly as the residuals below record.

### Final check inventory and CI cost

40 permanent `nix/quint.nix` checks, all evaluating under
`checks.x86_64-linux.*`, all in the merge gate:

| Kind | Count | Checks | Verdict class |
|---|---|---|---|
| Per-family exhaustive (restricted alphabet, full §2e bounds) | 8 | `quint-gw-lifecycle-fam-{preauth,admission,cap,wedge,hostile,upstream,drain,degraded}` | HOLDS `allInvariants` (34 properties) exhaustively at scope |
| Reachability witnesses (full regimes) | 17 | `quint-gw-lifecycle-witness-*` | expect-violation (reachable) |
| Pre-registered falsification probes (full regimes) | 4 | `quint-gw-lifecycle-falsification-{l5-no-carve-out,s16-terminal-only,s10-panic,l1-no-inactivity}` | expect-violation (the documented W3/W10/P12/W7 windows) |
| Named runs | 2 | `quint-gw-lifecycle-run-{compliant-peer,stall-wedge}` | pass (`quint test`) |
| Calibration guards (one per falsifying corpus family) | 9 | `quint-gw-calib-f{1,2,3,4,5,6,7,9,10}-*` | expect-violation (pre-fix revert re-finds the bug class) |

Measured aggregate cost at wiring time (32 TLC workers, shared builder,
serial protocol; figures from the Stage-C tables above): ≈12 min for
the 8 exhaustive checks (worst single check ≈4 min), ≈6–11 min for the
17 witnesses, ≈2 min for the 4 probes, ≈5–7 min for the 9 calibration
guards, seconds for the 2 runs — roughly 25–30 minutes of checker time
across 40 derivations on a fully uncached gate run, parallelized by
nix-fast-build and off the VM-test critical path; inside the design §5
estimate. Every check is individually inside the §2e ≤15 min hard cap.
Unwired manual targets (re-runnable with the command in each file's
header): the 16 non-wired calibration override modules, the 12 as-built
baseline modules, and any future full-regime exhaustive run.

### Counter-signatures (close-out verification, workflow run wf_96638bed-4fb, 2026-05-30)

Three independent verification slices were run over the integrated
campaign records — this map, the model, the calibration modules, the
`nix/quint.nix` wiring, the spec markers, and the
corpus/inventory/decision-log context documents — per the round-2
close-out discipline (orchestration stage B5). Each slice re-derived
the record's claims from the artifacts (rebuilding checks, reading
transcripts, reading every cited test and marker site) rather than
trusting the record.

| Slice | Scope | Rows checked | Verdict | Discrepancies |
|---|---|---|---|---|
| checks-vs-records | every wired check vs the Stage-C/calibration tables: inventory, module/invariant/step/heap parameters, verdicts, state counts, costs; spot force-rebuilds | 41 | **counter-signed** | 3 (2 minor, 1 cosmetic) — measurement-snapshot variance and a rounding overstatement; no false claims |
| acceptance-vs-corpus | all 14 acceptance-table family rows + the owner-decision rows vs the corpus, all 9 override files, the named test coverage, and the decision log | 18 | **counter-signed-with-corrections** | 4 (1 material, 1 minor, 2 cosmetic) |
| spec-markers-vs-reality | the 7 campaign tracey rules (≈40 marker sites read against the cited code/tests) + the 4 environment-assumption rows + wiring/validation claims | 11 | **counter-signed-with-corrections** | 3 (1 minor, 2 cosmetic) |

Aggregate: 70 rows, 10 discrepancies — 1 material (corrected in place),
4 minor (2 corrected in place; the 2 measurement-variance findings are
noted below, combined into one note), 5 cosmetic (2 corrected in place,
3 noted below). No discrepancy affects any verdict: every HOLD, every
violation, every calibration falsification, and every acceptance
disposition stands as recorded.

Spot-rebuild evidence from the checks-vs-records slice: 10 of the 40
checks force-rebuilt (`nix build --rebuild`) on a 192-core host,
spanning all 5 kinds. The rebuilt exhaustive checks reproduce their
recorded distinct-state counts EXACTLY (fam-preauth 1,183 / fam-hostile
968,151 / fam-upstream 3,253,999 / fam-wedge 66,059 — exhaustive counts
are deterministic); witnesses, falsification probes, and calibration
guards violate as recorded; the runs pass. All 40 stored transcripts
were read and agree with their recorded verdicts. The unwired
`gwCalibF3AsBuilt` baseline was additionally re-run manually and HOLDS
at exactly the recorded 1,183 distinct states, confirming the
structural cross-check row (`gwCalibF3AsBuilt` ↔ famPreauth).

### Corrections applied at this close-out

The record text above was corrected in place; reality (code, model,
checks) needed no changes anywhere.

1. **Material — churn-pin provenance (Stage-C record).** The record
   claimed the corpus and inventory were both mined at cccb4d778 and
   that "rio-gateway has not changed since the corpus/inventory were
   mined". True only for the inventory: the corpus was mined from
   origin/main dfe9a5569, and the four round-1 LogService cutover
   commits (1,313 insertions / 112 deletions across 14 rio-gateway
   files) sit between the two HEADs. None of the four is a lifecycle
   FIX/HARDENING commit, so no corpus family, candidate, or acceptance
   verdict is affected — the error was confined to the provenance
   sentence. Corrected in place.
2. **Minor — F8 test attribution (crosswalk + acceptance tables).** The
   GW-14/GW-15 out-of-model coverage was attributed to
   "golden-conformance" / "golden/integration" upload tests (wording
   inherited from design §3.4); the golden-conformance suite has no
   upload tests. The real coverage — the `wire_opcodes` upload tests in
   `opcodes_write.rs` (including the exact fac58554b regression test)
   and `functional/nar_roundtrip.rs` — exists and is load-bearing.
   Corrected in place; the test-only dispositions themselves are
   unchanged.
3. **Minor — pre-rebase commit hashes (churn-pin paragraph).** The four
   Stage-A commit hashes were recorded pre-rebase; the integrated
   branch carries them as fd92fb873 / a9f3ee2bc / a99314b20 /
   56fc66ea4. Corrected in place (both hash sets recorded).
4. **Cosmetic, corrected in place:** the Stage-B core-module line count
   ("2.2k" → ≈2.0k at the Stage-B commit); the fetch_min implementation
   citation (`ForceClose::arm_within` lives in `server/mod.rs`; the
   arm-assertion tests live in `connection.rs`).

Noted but not corrected (the record is accurate as a snapshot, or the
inaccuracy lives outside this document):

- **State-count / depth variance (minor).** Witness and falsification
  states-at-first-violation, and 4 of the 13 measured
  falsification/calibration depths (l5-no-carve-out, calib f2/f5/f9),
  are TLC-worker-count and parallel-BFS-race dependent: re-runs find
  equally valid counterexamples at 2–5× different state counts and
  slightly different depths (one witness reached 344,631 distinct
  states at 192 workers vs the recorded 87–130,940 range at 32
  workers). The recorded figures are correct as wiring-time snapshots
  under their stated conditions; violation/HOLD verdicts and exhaustive
  distinct-state counts are run-invariant and reproduce exactly. Reruns
  should compare verdicts and exhaustive counts, not first-violation
  statistics.
- **Owner-decision table is a subset (cosmetic).** It carries 4 of the
  design checkpoint's 7 Track-B decisions; the other three (B2
  panics-in-envelope, B3 scheduler-side cancel, B6 corpus exclusions)
  are carried in other sections of this map. A correct subset, no
  contradiction; the close-out paragraph above now lists all seven.
- **GW-16 crosswalk constituents (cosmetic).** The crosswalk row
  follows design §3.4's constituent swap (d617bf3e5 in, 5bc482e52 out)
  rather than corpus §4; the crosswalk header cites the design, so the
  row is internally consistent — representative, family, predicted
  property (L6), and verdict are identical either way.
- **Tracey impl-list overcount (cosmetic).** `tracey query rule
  gw.conn.cancel-on-disconnect+3` lists a verb-less cross-reference
  comment in `xtask/src/k8s/qa/scenarios/i183_pending_reaped.rs` as an
  impl site (it pre-existed the campaign and was only version-bumped).
  The rule's genuine enforcement sites (the session.rs cancel-loop
  select, the handler/build.rs removal-policy guard) carry the real
  impl markers; coverage is not overstated by this map.

### Accepted residuals (the GO's residue, ratified 2026-05-30)

What this campaign verified is bounded by the following — all
deliberate, all recorded at the stage that introduced them, and all
ratified at the Phase-0 checkpoint:

- **The four windows, accepted with their recorded bounds:**
  - **W2 — guard-vs-proto-task divergence** (owner decision B1,
    guard-held accounting): the `channels_active` gauge under-counts
    winding-down sessions for ≤ ~30 s per session; structurally bounded
    (the diverged proto task can only occupy `cancelling`, whose exit
    needs no peer, upstream, or timer cooperation). Pinned by
    `quint-gw-lifecycle-witness-server-side-release`; deployment note
    GW-D2 below.
  - **W5 — half-open vanish occupancy:** a vanished peer pins one conn
    permit (of 1000), one session permit (of 4096), the gauge slots,
    ≈512 KiB of duplex buffers, possibly a NAR assembly buffer (≤4 GiB,
    `MAX_NAR_SIZE`), and the in-flight scheduler/builder work for
    ≈300–330 s until the designed transport reap fires. An occupancy
    bound, not a violation. Pinned by
    `quint-gw-lifecycle-witness-vanish-reclaimed`; deployment note
    GW-D3 below.
  - **W7 — degraded-tier reclamation:** with the TCP_USER_TIMEOUT
    setsockopt failed, reclamation of a parked-write connection
    degrades from the designed ~305 s to the ~1 h inactivity backstop;
    the narrower in-process-unbounded sliver (arming after both russh
    wakes are spent) is out of model. The gating premise — a kernel
    setsockopt failure on the Linux/EKS target — is the deployment
    observable GW-D1 below. Pinned by
    `quint-gw-lifecycle-falsification-l1-no-inactivity`.
  - **W10 — non-Wire removal orphan:** a non-Wire stream failure with a
    dead or absent client orphans a running build for up to ~300 s
    until the scheduler orphan watcher cancels it. The Phase-1
    removal-predicate fix was NOT triggered (the trigger condition is
    unreachable; `w10TriggerAbsent` holds exhaustively); the leak is
    bounded by a named environment assumption, not by this campaign's
    checks. Pinned by
    `quint-gw-lifecycle-falsification-s16-terminal-only`.
  - Plus the two pre-registered carve-outs that are windows by design:
    the W3/P4 admit-vs-grace race (bounded by one grace cycle; pinned
    by `l5-no-carve-out`) and the P12 handler-panic carve-out (pinned
    by `s10-panic`; backstopped by the same orphan watcher).
- **Deferred full-regime exhaustive coverage** (the Stage-C check-set
  record's list): cross-family interleavings, conn-B concurrent full
  session lifecycles, two concurrent egress/panic sessions on one
  connection, and the base-regime "everything legitimate at once" proof
  remain witness/named-run-pinned only. A future full-regime exhaustive
  run stays available as a manual target; it is not in the merge gate.
- **Out-of-model families on test-only coverage** (owner decisions
  §8 Q5/Q6 = B5/B6, ratified): F8 (GW-14/GW-15 upload bounding — the
  `wire_opcodes` upload tests + `functional/nar_roundtrip.rs`; store
  half owned by the store campaign), F11 (GW-17 credential freshness —
  the session_jwt refresh + I-129 regression tests), F14 (GW-21 ingress
  memory budget — the drv_cache cap tests; named Phase-2 Kani candidate
  if ever commissioned), F12/F13 (out of corpus: result-integrity vs
  store, and the adjacent state machines — scheduler-watch reconnect,
  STDERR framing, startup/readiness).
- **Environment assumptions** (imported, not verified; a regression
  behind one is NOT caught by any check this campaign added): the
  scheduler orphan-watcher backstop; WatchBuild reconnect boundedness;
  the silent-stream-with-live-client design; ForceClose fetch_min
  monotonicity (unit-test-covered only). The environment-assumptions
  table above names the owner and coverage of each.

### Deployment-checklist deltas (the operator handoff)

This campaign adds no deployment gates (round-2 no-deploy discipline);
these are the operational consequences of the accepted residuals and
the design-checkpoint decisions, finalized here as checklist items for
whoever first deploys the gateway. Sources: design §4 dispositions as
ratified, owner decisions B1/B2/B4.

| # | Item | What to do at deployment time | Source |
|---|---|---|---|
| GW-D1 | **TCP_USER_TIMEOUT setsockopt-failure alert** | Alert on the `set TCP_USER_TIMEOUT failed` warn line (`rio-gateway/src/server/mod.rs:511`). The W7 acceptance rests on the premise that this setsockopt effectively cannot fail on the Linux/EKS target — the alert is the check on that load-bearing premise. There is no `errors_total` label for it (the optional Phase-1 label was not commissioned); the warn line is the only signal. If it fires: parked-write reclamation degrades to the ~1 h inactivity backstop, and the out-of-model sliver (slow-output + zero-window-ACKing peer) is unbounded in-process until restart. | §4 W7 |
| GW-D2 | **`channels_active` autoscaling caveat** | `rio_gateway_channels_active` (the autoscaling signal) momentarily under-counts winding-down sessions during mass disconnects with an unresponsive scheduler — the W2 guard-held divergence, ≤ ~30 s per session (the cancel-loop bound). Do not tune autoscaler reactions tighter than that window; treat dips during mass-disconnect events as expected. | §4 W2 / owner decision B1 |
| GW-D3 | **Memory headroom for NAR buffering** | Per-connection worst-case memory is NOT bounded by the lifecycle caps: a half-open vanish (W5) or a slow client can pin ≈512 KiB of duplex buffers plus a NAR assembly buffer (up to `MAX_NAR_SIZE` = 4 GiB) for the full ~300–330 s reap window, per occurrence. Size pod memory limits for expected concurrent uploads × realistic NAR sizes on top of the steady-state footprint (the W8 amplification has no in-process cap; the existing 4 GiB pod limit was sized for drv_cache, not concurrent NAR uploads). | §4 W5/W8 |
| GW-D4 | **Conn-permit occupancy alert** | Alert on sustained `rio_gateway_errors_total{type="conn_cap"}` growth. Conn-permit-at-accept is a fixed fact (owner decision B4): probes and SYN-flood-with-completion can transiently hold conn permits with no auth-level signal, indistinguishable in-process from legitimate load. | Owner decision B4 |
| GW-D5 | **SIGKILL / terminationGracePeriodSeconds** | SIGKILL is outside the verified envelope (owner decision B2). Set the gateway pod's `terminationGracePeriodSeconds` ≥ the drain budget (accept-stop + session-drain timeout + 5 s CANCEL_GRACE) so the verified three-stage drain is what actually runs on eviction; the scheduler orphan watcher and store backstops are the named assumptions behind anything a SIGKILL interrupts. | Owner decision B2 |

### What the campaign does NOT claim

No deployment, soak, or canary validation exists (no cluster was ever
involved); every figure above is a model, test, or CI measurement. The
exhaustive verdicts hold at the §2e bounds (2 connections, 2 channel
slots on conn A, queue depth 2, window credits 2, residue ceiling 2,
≤1 tracked build) under the restricted per-family alphabets; the
full-alphabet regimes are witness/probe-pinned, not exhausted. Recorded
wall-clocks and first-violation state counts are snapshots under their
stated conditions (TLC worker count and builder contention move them;
verdicts and exhaustive counts do not). Nothing is machine-proved at
the code level (no Kani harness exists for any gateway path; F14's
drv_cache budget is the named candidate if one is ever commissioned);
the coupling between model and code is the calibration corpus plus the
tracey marker discipline, not refinement. The four environment
assumptions above are imported, not verified. SIGKILL, the data plane
(upload framing, NAR streaming, STDERR), credential freshness, and the
adjacent state machines are outside the verified envelope, on the
test-only coverage the acceptance table names.
