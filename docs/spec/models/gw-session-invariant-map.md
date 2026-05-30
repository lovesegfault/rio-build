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

**Status: Stage A (spec audit) complete; Stage B (model + measurement
milestone) complete — all 34 properties encoded, witnesses and
pre-registered falsification probes confirmed reachable/falsifying, the
B-measure recorded below with the regime-split recommendation; Stage C
check-set selection + CI wiring + witnesses/probes + the §4 model-first
evidence complete (this change set — 31 permanent `nix/quint.nix` checks,
verdicts in the Stage-C record below); Stage-C calibration (override
modules, both directions for GW-2/GW-13/GW-18) pending.**

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
Stage-C record's coverage table below. Calibration verdicts land with the
override modules (next change set).

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
pre-registered disposition. Stage C builds the override modules and records
the falsified-property @ depth verdicts; this table is the Stage-B
pre-registration of what each override must reproduce.

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
| GW-14 | fac58554b | — (wire-position integrity, below the data-plane abstraction) | OOM: `wire_opcodes` + golden-conformance upload tests |
| GW-15 | cb64e2913 (+d2e84cf31) | — (per-entry upload bounds) | OOM: store campaign (store half) + golden/integration multi-entry upload tests (gateway half) |
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

### Stage B — model + measurement milestone (this change set)

**Artifact.** `docs/spec/models/gwConnLifecycle.qnt` — one core module
(2.2k lines) + seven §2e regime instantiation modules (`Base`, `Cap`,
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
stale references to the deleted dispatch helpers"), the same HEAD the
inventory and calibration corpus were mined against. `git log cccb4d778..HEAD
-- rio-gateway/` contains only this campaign's own Stage-A commits
(66901f86d, af9f7c482, 805c8ab4a, c088d7af2), and their rio-gateway diffs are
comment-only (`r[impl]`/`r[verify]` markers and doc comments — 31 insertions,
5 deletions, no executable line touched). rio-gateway has not changed since
the corpus/inventory were mined.

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

### Stage C (continued) — calibration (pending, next change set)

Will add: the calibration table (override module ↔ candidate ↔ falsified
property @ depth, trace-walk record), both directions for GW-2 / GW-13 /
GW-18, and the re-confirmed §4 window dispositions. The override modules
land under `docs/spec/models/calibration/gw-*.qnt` and instantiate the
full-alphabet regime modules (not the restricted family modules), per
design §5.
