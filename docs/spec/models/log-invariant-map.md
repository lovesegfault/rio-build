# Build-log invariant ↔ spec-rule map

Working artifact for the log-formal campaign's Phase 1 (spec audit) and
Phase 2 (model A). Maps the ten invariants of the build-log verification
design (§3.4: five model-A invariants over the single-replica entry
lifecycle, five model-B invariants over cross-replica finalization) onto the
`obs.log.*` / `sched.log.*` / `sched.merge.*` / `sched.recovery.*` rule set.

This is the post-Phase-2 state: every invariant maps onto a rule whose
normative MUST sentence states it (the COVERS column is total), and every
model-A invariant is verified exhaustively by the `quint-log-*` checks in
`nix/quint.nix` (TLC backend, full reachable state space, six regimes: `base`,
`flap`, `fault-local`, `fault-recovery`, `fault-persist`, `fault-guard`). The "audit
finding" column records what the pre-audit rule set was missing and which
Phase-1 task closed it, so the model phases can cite why each rule has the
shape it has. The "Findings" section at the end records what the Phase-2
model checking itself found.

## Model A — single-replica entry lifecycle

Verified by: `quint-log-base`, `quint-log-flap`, `quint-log-fault-local`,
`quint-log-fault-recovery`, `quint-log-fault-persist`, `quint-log-fault-guard`
(each asserts all eight invariants — the five §3.4 model-A invariants plus the
three Phase-3 calibration invariants — and the `boundsOK` ceiling tripwire
over its regime's full reachable state space); non-vacuity pinned by the
twelve `quint-log-witness-*` expect-violation checks,
`quint-log-witness-gap-span-ungated`, and the four `quint-log-calib-*`
permanent calibration witnesses.

| Invariant | Rule(s) | Verdict | Audit finding |
|---|---|---|---|
| `NoCrossExecContamination` | `obs.log.exec-keyed+2`, `sched.log.batch-binding` | **COVERS** | Was PARTIAL at `exec-keyed+1`: the rule required one blob and one PG row per execution but said nothing about the in-memory ring entry that feeds them — a cross-exec restamp carrying the prior execution's lines, seal, pending-final mark, or cached prefix into the new execution's flush would defeat the storage keying. `+2` makes the cross-exec clearing normative; `batch-binding` covers the ingress half (a foreign executor's batch never enters the entry). |
| `LineSpanExact` | `obs.log.gap-span+2` | **COVERS** | Was PARTIAL at `gap-span+1`: the write-path MUST was conditioned on "a row whose blob folds a recovered pre-failover prefix", leaving the hole-carrying-payload-without-a-prefix shape (an interior hole of lines delivered only to an interim leader) outside the normative scope even though the code records the true span unconditionally and the rule's own read-path half depends on it. `+2` broadens the subject to any flush payload that does not physically contain every line of the range it covers. |
| `BindingGateExcludesForeignExecutors` | `sched.log.batch-binding` | **COVERS** | "MUST drop batches whose `derivation_path` does not match an active assignment held by the calling executor's stream" is exactly the gate; `push_for` evaluates it under the entry's shard write-lock so the check and the append are atomic. `sched.log.path-length` and `sched.executor.input-bounds+2` bound the same ingestion surface (path length, monotone line numbering, overflow) — modelled as part of the accept/reject predicate in Task 2.3. |
| `EveryRetainedEntryIsJustified` | `obs.log.entry-justified` | **COVERS** | Was a GAP: the four flusher/epilogue reaps (tenure-orphan, refused-UPSERT, sealed-empty, enqueue-failure) had no unifying rule; only the acquisition-time sweep (`sched.recovery.log-buffer-sweep+2`) and the poison-TTL backstop were normative. The new rule states the justification set (non-terminal in an authoritative DAG ∨ unresolved pending final ∨ DAG not authoritative) and the obligation that an entry that has lost every justification is reaped by the next path that observes it. The model's check is that the reap set is *complete* (no fifth orphan shape). |
| `NoSilentLineLoss` (per-replica) | `obs.log.line-conservation` | **COVERS** | Was PARTIAL: the per-piece rules (`gap-span`, `ring-byte-cap`, `periodic-flush`, `incomplete-surfaced`, `stored-coverage-preserved`) each bound or surface one loss channel; no rule stated the conjunction. The new rule states it: a delivered line is in the ring, in the stored coverage, or counted in the recorded span as a gap, and may only leave all three through a loss channel the stored record itself discloses (a `first_line` above the true start, or a record that never claims completeness). |
| `NoStaleSealOnLiveCarrier` *(Phase 3)* | `obs.log.exec-keyed+2` (the restamp-unseal half) | **COVERS** | Added by the Phase-3 calibration: the seal-clearing fixes (699ea1692, 2f9a747d0) protect a property the original five cannot state — an entry sealed while its derivation is DAG-live mutes a live execution, and the muting is invisible to the conservation law because a gate-rejected line never enters `delivered`. One safety invariant over existing state (`sealed × hasEntry × dag`), not a new state dimension. |
| `FinalizedRowFrozen` *(Phase 3)* | `obs.log.finalize-immutable` (the model-A half) | **COVERS** | Added by the Phase-3 calibration: once a row is recorded complete, no later write changes any of its columns. Backed by the `storedHist.frozen` ghost (the first finalizing write's snapshot). The single-replica + abstract-interim half of `IsCompleteMonotone` / `AtMostOneFinalizingWrite`; the cross-replica blob/row split stays model B's. |
| `StoredCoverageNeverRegresses` (single-writer) *(Phase 3)* | `obs.log.stored-coverage-preserved` | **COVERS** | Added by the Phase-3 calibration: a row's recorded span end never decreases across writes (the `storedHist.everEnd` ghost high-water). The model-A-observable half of the model-B invariant of the same name; the cross-replica conservation form (the interim's *delivered lines* survive into the finalized record) still needs model B. This is the invariant that re-finds both the stored-coverage reconcile fix (e1fd179b9) and — unexpectedly — the deferred-final tenure pin (3ce5d03e4). |

## Model B — cross-replica finalization

| Invariant | Rule(s) | Verdict | Audit finding |
|---|---|---|---|
| `TenurePinnedFinalization` | `obs.log.deferred-final-retry+4` | **COVERS** | Was PARTIAL at `+3`: the normative drop condition named only the lease and the generation, and the generation cannot identify the tenure once the recovery PG-floor seed has saturated it past `leaseTransitions + 1` (an A→B→A holder change is then a generation no-op) — exactly the `req_in_tenure` bug the rebase fixed in code. `+4` makes the recorded acquire-epoch the normative discriminator; the generation stays as rationale (a conservative additional break on the mid-tenure floor seed). |
| `IsCompleteMonotone` | `obs.log.finalize-immutable` | **COVERS** | "Row content MUST NOT be overwritten or regressed by a later flush" includes the `is_complete` column itself; `upsert_drv_log`'s `WHERE NOT drv_logs.is_complete` clause refuses every write to a finalized row. |
| `AtMostOneFinalizingWrite` | `obs.log.finalize-immutable` | **COVERS** (one documented residual, below) | "A flusher holding retained lines for an already-finalized execution MUST discard them instead of re-finalizing" is the at-most-once obligation; the row consult before any S3 work plus the frozen-row UPSERT are the two enforcement layers. **Residual for the Phase-4 model to adjudicate:** the guard SELECT and the S3 PUT are not atomic across replicas — a concurrent finalization landing between another replica's guard read (`is_complete = false`) and its PUT overwrites the finalized blob in place while the frozen row keeps describing the replaced content (`upsert_drv_log`'s refused-write comment). The row-level invariant holds; the blob-level "MUST NOT be overwritten" has this race window. Not recorded as a CONTRADICTION because the obligation's enforcement exists and the window is documented as accepted in the code; model B decides whether the window is reachable under the lease layer's NeverDual guarantee and whether the rule needs a carve-out or the code a fix. |
| `StoredCoverageNeverRegresses` | `obs.log.stored-coverage-preserved` | **COVERS** | "Content recorded by a prior tenure and not contiguously covered by the current ring MUST NOT be overwritten; the flusher MUST fold the stored blob in" is the no-regression statement; the carve-outs (same-tenure ring eviction, never-flushed interim-leader lines) are in the trailing prose and are the same carve-outs the model's `stored` ghost variable encodes. |
| `NoSilentLineLoss` (cross-replica) | `obs.log.line-conservation` | **COVERS** | Same rule as the per-replica form, written replica-agnostically (the ring entry, the shared stored coverage, the recorded span) so the cross-replica union form is the same rule read over both replicas' rings plus the shared store. |

## Rules in the log-domain inventory not load-bearing for any §3.4 invariant

- `obs.log.batch-64-100ms`, `obs.log.worker-header` — worker-side framing and
  display headers; conventional tests cover them.
- `obs.log.incomplete-surfaced` — the read-path surfacing of
  `is_complete = false`; referenced by `line-conservation` as the channel that
  discloses unaccounted tail loss, but the CLI/dashboard rendering itself is
  outside the model.
- `obs.log.periodic-flush`, `obs.log.ring-byte-cap` — the loss *bounds* (≤30s,
  16 MiB / 100k lines); the model abstracts both to "an eviction may fire" and
  "a flush may run" and checks accounting, not sizing.
- `obs.log.required-fields` — structured JSON telemetry of the components
  themselves, not build logs; in the `obs.log.*` namespace but entirely
  outside this campaign's subject.
- `sched.log.path-length`, `sched.log.phase-binding` — ingestion bounds on
  sibling message types; adjacent to the binding gate but not part of the
  entry lifecycle.
- `sched.merge.exec-correlation+7` — the `build_derivations.exec_id`
  write-once correlation; P2's read-side half. The model covers the exec_id
  *minting and stamping* (dispatch, restamp) but not the per-build
  observation row.
- `sched.recovery.log-buffer-sweep+2` — one of the reap paths unified under
  `obs.log.entry-justified`; stays as the specific normative statement of the
  acquisition-time sweep's predicate (unsealed ∧ not non-terminal in the
  rebuilt DAG).

## Verify-marker status

The two rows Phase 1 left in `tracey query untested` are now closed:

- `obs.log.line-conservation` — verified by model A's `noSilentLineLoss`
  invariant in all six `quint-log-*` regime checks (the markers live at the
  check wiring points in `nix/quint.nix`).
- `obs.log.entry-justified` — verified by model A's
  `everyRetainedEntryIsJustified` invariant in the same checks.

Every other rule in the inventory already carries at least one `r[verify]`
site. `tracey query uncovered` shows no log-domain rows.

## Findings (Phase-2 model checking)

### Calibration entry #0: the gap-merge fold's accept-gate precondition (`obs.log.gap-span+2`)

The first falsification of the campaign, and the template for the Phase-3
calibration table (each entry is "revert `<fix>`, run `<check>`, watch
`<invariant>` go red").

- **Falsification:** `lineSpanExact` violated. `push_for`'s line-numbering
  gate compared a batch only against the ring's current tail, so it reset
  whenever the ring emptied — and the stored-coverage reconcile empties the
  ring on exactly the path where the comparison matters (it truncates every
  retained line below a prior tenure's stored row end and caches that row as
  the recovered prefix). A batch numbered below the cached prefix's end then
  landed in the empty ring and the gap-merge fold double-counted it: the
  row's `first_line + line_count` overshot the execution's true end by the
  overlap, the blob held the overlapping lines twice, and the read path's
  physical-vs-claimed divergence check stayed blind (duplication and
  overstatement cancel). The original machine-found counterexample reached
  the precondition through the model's then-free-standing eviction action — a
  state the real push-coupled eviction cannot produce; the adversarial review
  re-derived the violation through the reachable path (an interim leader
  extending the stored row past this replica's retained ring).
- **Fix:** the entry-lifetime accept floor `RingBuf::accounted_below` — one
  past the highest line the entry has ever accounted for, raised by every
  accepted push, by the stored-coverage truncation, and by the prefix cache;
  never lowered; reset only by a cross-exec restamp. `push_for` rejects any
  batch starting below it (`reason="below_stored_prefix"` when the ring holds
  nothing at or above the batch).
- **Witness that the fix is load-bearing:** `quint-log-witness-gap-span-ungated`
  — the flap regime with the floor disabled (`ENABLE_ACCOUNTED_FLOOR = false`)
  MUST still violate `lineSpanExact`. A green exhaustive `quint-log-flap`
  plus a red `lineSpanExact` in the ungated module is the machine-checked
  statement that the gate is necessary and sufficient at the model's bounds.

### The sealed-orphan reaps are gated on `may_flush()`, which a rebound suspends for a full recovery cycle (`obs.log.entry-justified`)

Found by the first exhaustive run of the flap regime (the violation is 13
actions deep; 20 000 random 24-step traces never reached it). A sealed
non-empty orphan left by `flush_final`'s out-of-tenure drop arm, whose ring
the stored-coverage reconcile then empties, has exactly one reaper left: the
periodic sealed-empty reap (its sibling, the refused-UPSERT reap, covers the
non-empty-with-a-finalized-row shape). Both periodic reaps are gated on
`may_flush() = is_leader && recovery_complete()`, and `on_rebound` clears the
recovery-completion stamp without clearing `is_leader`, the DAG, or
`dag_authoritative` — so a rebound suspends the orphan's only reaper for a
full recovery cycle while the replica still looks like an authoritative
leader. The acquisition-time sweep does not cover the entry (it skips sealed
keys). Adjudicated as an invariant-encoding gap, not a code bug: the
re-fired LeaderAcquired's recovery re-opens the gate and the next periodic
tick reaps the entry, so the window is bounded; the invariant's
pre-recovery-window disjunct is now keyed on the reap's own gate.

**Architectural note for Phase 6:** this is direct evidence for the
"four reaps → one ungated general reap" simplification candidate. The four
orphan shapes (tenure-drop, refused-UPSERT, sealed-empty, enqueue-failure)
are reaped by four different code paths with three different gates
(`req_in_tenure`, `may_flush()`, none), and the justification argument for
"no fifth orphan shape" has to thread the union of those gates through every
lease transition. A single reap pass that runs unconditionally on every
periodic tick (or at every acquisition) and discards any entry that has lost
every justification would collapse the case analysis and remove the
rebound-window sensitivity entirely.

## Findings (Phase-3 calibration against the historical-fix corpus)

Phase 3 reverted each substantive protocol fix in the build-log subsystem's
history *in the model* — one calibration switch per fix in
`logBufferLifecycle.qnt`'s const block, one override module per reverted
behavior in `docs/spec/models/calibration/` — and asked whether an invariant
re-finds the bug the fix closed. Three new model-A invariants came out of it
(each added because an override produced a harmful state none of the original
five observed), one candidate was rejected as inexpressible, and the
`reapEnabledFor` justification was tightened to scheduler-initiated reaps
only. Four overrides are wired into CI as permanent expect-violation checks
(`quint-log-calib-*`); the rest stay in `calibration/` as re-runnable
evidence. Measured state counts, trace depths, and wall-clocks are in the
introducing commits' messages and the check transcripts.

### The invariant additions

- **`noStaleSealOnLiveCarrier`** — the model sees *misattribution* (one
  execution's data under another's key: `noCrossExecContamination`) but was
  structurally blind to *muting* (a live execution's data refused at the
  gate), because the conservation law deliberately quantifies over accepted
  lines only and a rejected line never enters `delivered`. Two of the three
  restamp fixes exist to prevent muting; both calibration overrides hold on
  every original invariant over their full reverted state spaces and falsify
  this one at 7 actions.
- **`finalizedRowFrozen`** — `obs.log.finalize-immutable` had no model-A
  invariant; both finalize-once overrides previously falsified only
  `noSilentLineLoss`, two steps downstream of the overwrite, through a
  two-tenure `delivered`-accumulation argument. The frozen-row ghost catches
  the overwrite at the step it happens. Adding it required completing the
  post-fix-interim environment assumption (`interimFinalizes` now refuses to
  rewrite an already-finalized row, as the real interim's
  `WHERE NOT drv_logs.is_complete` UPSERT does — without that guard the
  abstract interim falsifies the invariant by doing something no real
  replica can).
- **`storedCoverageNeverRegresses`** (single-writer) — the recorded span end
  of a `drv_logs` row never decreases. The lines a regression destroys were
  delivered only to the interim leader, so they are outside the per-replica
  conservation ghost by design; the ghost high-water is the cheap repair (no
  new state dimension — the ghost is a function of the live row in every
  shipped regime and adds zero distinct states).
- **`liveDerivationHasCarrier` — REJECTED.** The tenure-pins dossier's
  candidate ("a DAG-live derivation always has an unsealed carrier") is
  reachably false on the post-fix model: an in-tenure final flush
  legitimately drains the carrier of a derivation whose DAG node is live
  only because the terminal's PG persist failed and recovery rebuilt the
  node from the stale assignment row. The harmful instance (an
  out-of-tenure or wrong-exec request did the draining) is state-identical
  to that benign one — the distinguishing fact is the draining request's
  tenure stamps, which the drain consumes. The carrier-destruction half of
  the tenure-pins harm class is therefore a **transition property** model A
  cannot state; its automated coverage is the rio-scheduler deferred-final
  unit tests (the persist×guard correlated scenario the regime split
  already pins on them). Falsification trace and the failed refinement
  attempts: `phase3-falsification-liveDerivationHasCarrier.md` in the
  campaign workspace and the comment at the val's would-be definition site.
- **The `reapEnabledFor` scheduler/environment split** — the
  "one step from reaped" justification disjunct no longer counts the
  disconnect-time discard: that reap is environment-initiated (it fires only
  if the owning worker's stream happens to close, which may never happen for
  a long-lived connected worker), so its enabledness is not "the scheduler
  will reap this". The tightening is what makes the acquisition-sweep
  override falsifiable; all six shipped regimes stay green under the
  stricter invariant, confirming the disjunct was dead in the post-fix state
  space.

### The calibration table

Verdict legend: **F(inv, n)** = the override falsifies `inv` with an
`n`-state counterexample (n includes the initial state; actions = n − 1);
**H** = holds over the override's *full* reachable state space (the
machine-checked statement that no listed invariant observes the reverted
behavior). Class: **E** = encodable (an override module exists), **NE** =
not encoded (the pre-fix/post-fix delta is not expressible at the model's
granularity), **B** = out of scope for model A (a two-replica / shared-row
concern).

| Commit | Fix | Class | Module (`logBufferLifecycle…`) | Verdict | Disposition |
|---|---|---|---|---|---|
| `3a55474ac` | accounted-floor accept gate | E | `…Ungated` | **F**(lineSpanExact) | Calibration entry #0 — see its own findings section above. CI: `quint-log-witness-gap-span-ungated`. |
| `496e6fb14` | recv-task (executor, drv) binding gate | E | `…Unbound` | **F**(bindingGateExcludesForeignExecutors, 2); the other eight **H** over the full space, which is *bit-for-bit the base regime's* — ungating adds zero reachable states because the executor parameter is guard-only | The only invariant that can falsify is the gate's own definition: the `Set[int]` line encoding erases the content/provenance harm (another worker's bytes under this execution's blob and banner), and an accepted foreign batch is state-identical to the bound executor pushing the same interval. Recorded decision: the price of the line encoding is worth paying; the gate's un-modeled consumers are display-only. Not wired into CI — the invariant is asserted in every regime check, so weakening `batchAccepted` already turns `quint-log-base` red. |
| `496e6fb14` (residual) | legacy `push()`'s `or_default()` unstamped-entry allocation | NE | — | — | Missing dimension: the unstamped-entry state, excluded by design. Not worth adding — `assign_to_worker`'s discard clears a pre-staged entry at dispatch; the residual harm is a memory bound, not a lifecycle property. |
| `7beb1ca00` #1 | gateway `ForwardLogBatch` gated on `push_for`'s verdict | NE | — | — | Missing dimension: the live fan-out sink (a second consumer of the accept decision). Content provenance on a write-only display stream — a turmoil/MBT candidate, not a model-A candidate. |
| `7beb1ca00` #2 | `FlushRequest.exec_id` stale-drain pin | E (already encoded: every `flushFinal*` arm conjoins `entryExec == req.exec`) | not separately reverted | — | Its reversion is the same transition-property blindness as the tenure-pins family (a mis-drained entry's post-state is identical to a legitimately-drained one's); subsumed by that family's verdict and by the `flushFinalStaleOrMissing` arm's existence. |
| `7beb1ca00` #3 | `LogFlusher` periodic arms leader-gated | B | — | — | Model-B handoff: ungating the periodic UPSERT should falsify the cross-replica `StoredCoverageNeverRegresses` (an ex-leader's stale `.partial` racing the new leader's fresher row needs two writers). |
| `7ffbf1415` | `BuildPhase` actor-side executor gate | NE | — | — | No model footprint (no Phase message, no display sink, no actor-side `assigned_executor`); already listed above as not load-bearing for any §3.4 invariant. Conventional tests own it. |
| `6c26e85f8` | cross-exec restamp clears the retained lines | E | `…CrossExecCarriesLines` | **F**(noCrossExecContamination, 8) | The one corpus row that falsifies an original model-A invariant outright. CI: `quint-log-calib-cross-exec-carries-lines`. |
| `699ea1692` | cross-exec restamp clears the seal | E | `…CrossExecKeepsSeal` | **F**(noStaleSealOnLiveCarrier, 8); original five + the other two new **H** over the full 21.3M-state space | Resolved by the Phase-3 invariant. The full-space HOLDS is the machine-checked statement that the original invariant set cannot see the muting. |
| `2f9a747d0` | same-exec restamp clears the seal + final-pending mark | E | `…SameExecKeepsSeal` | **F**(noStaleSealOnLiveCarrier, 8); original five + the other two new **H** over the full 24.8M-state space | Seal half: resolved by the Phase-3 invariant. Mark half: at the model's granularity genuinely redundant with justification disjunct (4) — the periodic flush keeps snapshotting the marked orphan; the leak it causes is a process-lifetime bound the model does not measure. |
| `6aab70b47` | enqueue-failure reap | E | `…NoEnqueueFailReap` | **H** (all nine, full 36.0M-state fault-local exploration) | Redundant-with {terminal-cleanup discard, periodic sealed-empty reap} *as observed by the safety invariant*. NOT deletable from the code: its real justifications are the dead-flusher mode (in which the periodic reaps never run again — persistent component death is not a representable mode) and the `GetDerivationLogs` shadow-window latency, both NOT-ENCODED. Phase-6 ledger entry. |
| `463090eb7` | one-shot finalized-elsewhere consult at the drop arm | E | shares `…NoRefusedUpsertReap`; its mechanism is restorable via `…OneShotConsult` | **F**(everyRetainedEntryIsJustified, 10) | The no-reap-at-all bug this commit's message describes. Phase-6 ledger: superseded the same day by f8ce10b8e; no code remains. |
| `f8ce10b8e` | recurring refused-UPSERT reap | E | `…NoRefusedUpsertReap`, `…OneShotConsult` | **F**(everyRetainedEntryIsJustified, 10) in BOTH variants — with no reaper at all, and with 463090eb7's one-shot consult restored as the only reaper (the consult runs at request-drop time, before the interim finalizes the execution, and never re-runs) | Individually load-bearing: the reconcile never empties a finalized row's ring, terminal cleanup is blocked by the final-pending mark, the sweep and the disconnect discard skip sealed keys, and the drop arm already had its one chance. The OneShotConsult red is the machine-checked statement that moving the reap to the recurring periodic chokepoint was a correctness fix, not a performance refactor. CI: `quint-log-calib-no-refused-upsert-reap`. |
| `81824cfbb` | periodic sealed-empty reap | E | `…NoSealedEmptyReap` | **F**(everyRetainedEntryIsJustified, 12) | Individually load-bearing: the only reaper of a sealed entry whose ring the stored-coverage reconcile empties after its pending final is consumed (TLC's counterexample needs no interim leader — a holey ring's own `.partial` covers past its physical content, so the next tenure's reconcile truncates the whole ring away). The deepest counterexample in the corpus; the Phase-2 finding's adjacent state was unreachable by 20k random 24-step traces. CI: `quint-log-calib-no-sealed-empty-reap`. |
| `84ac79b84` | flush_final stale-request guard atomic with the drain | NE | — | — | Missing dimension: lock-granularity interleaving between two synchronous DashMap calls (the `exec_id()` read and the unconditional `drain()`); the model's per-arm action atomicity collapses it. Even with the split, the harm is the carrier-destruction transition property. §3.6 evidence. |
| `3ce5d03e4` | deferred-final tenure pin | E | `…TenureUnpinned` | **F**(storedCoverageNeverRegresses, 9) — *the dossier predicted HOLDS-everything*; original five **H** over the full 9.6M-state space; noStaleSealOnLiveCarrier and finalizedRowFrozen also **H** | **The campaign's headline deviation.** The un-pinned final fires while deposed with a prefix latch settled before the flap and overwrites the row the interim leader extended past this replica's ring — the exact shape the dossier predicted but assigned to model B because "the property is a history property over `stored` that model A does not state". The Phase-3 ghost high-water states it, so the tenure pin is now justified by a model-A invariant. The *other* harms of an unpinned final (freezing a row another replica is extending; destroying the live restamped carrier) remain model B's and the transition-property gap respectively. |
| `646022e37` | tenure pin hoisted before the destructive arms | E | `…PinAfterGuard` (the persist×guard product regime the shipped split deliberately excludes) | **H** (all nine, full 12.1M-state exploration) | The harm — an out-of-tenure request's guard-error arm reaps the live restamped carrier — produces a post-state identical to the legitimate in-tenure reap's: the rejected `liveDerivationHasCarrier` transition property. The rio-scheduler deferred-final unit tests are the only automated check standing between this fix and a regression. The third destructive arm it gated (the deferred-queue cap-overflow drain) is unmodeled. |
| `825cdd478` | atomic tenure-drop reap + post-await tenure re-validation | NE | — | — | Missing dimensions: lock-granularity interleaving (the `is_sealed()` read vs the removal) and await-granularity interleaving (the entry-time pin vs the post-guard-SELECT destructive arms — the model header pre-registers exactly this gap). The commit's own regression tests (a held `LOCK TABLE` parking the guard SELECT while the lease flips) are the deterministic-simulation form. §3.6 evidence. |
| `2c301438d` | disconnect-time discard gated on an authoritative DAG | E | `…DisconnectUngated` | **F**(noSilentLineLoss, 11) | The discarded lines were never stored, are not in the finalized record's claimed span, and are not disclosed by an incomplete indicator. `lineSpanExact` stays green on the same trace (the row is exact about the span it claims; it claims too little) — the clean separation between the span-arithmetic invariant and the conservation law. |
| `e6c18add2` | acquisition-time stale-buffer sweep | E | `…Sweepless` | **F**(everyRetainedEntryIsJustified, 7) | Falsifiable **only after** the `reapEnabledFor` scheduler/environment split — the dossier's predicted HOLDS-as-written was an invariant-encoding artifact (the un-swept orphan was declared "one step from reaped" forever by the environment-initiated disconnect discard), not a property of the sweep. |
| `8e97c2220` | sweep spares only non-terminal DAG entries (poisoned drvs) | NE | — | — | Missing dimension: a persisted terminal status that recovery loads back into the DAG as a non-live, reap-exempt node (`PgPoisoned`, a `DagTerminal` recovery outcome, and the poison-TTL backstop reap). Not worth the cost: the delta is a one-line filter pinned by the poisoned-under-interim-leader unit test; the failure class is a data property of `load_dag_from_rows` for conformance/MBT. |
| `abfade4d5` | periodic frozen-row UPSERT latch | E (the latch); NE (the mid-sweep `is_leader()` staleness that motivated it — N1); B (the `IsCompleteMonotone` headline) | `…NoPeriodicLatch` | **F**(finalizedRowFrozen, 12) at the downgrade step; **F**(noSilentLineLoss, 14) two steps later (the downgrade un-freezes the row for a later legitimate final that re-finalizes with a smaller span) | Resolved by the Phase-3 invariant at the overwrite itself. The model reaches the unsafe write through the A→B→A retained-entry route, not the commit's mid-sweep race. |
| `44905298b` | flush_final already-finalized consult (+ frozen-row tightening + empty-drain guard) | L1: E. L2's independence from L1 + the blob/row split: NE (N2). L3: NE (N3). Headline: B | `…NoFinalizeRefusal` | **F**(finalizedRowFrozen, 13); **F**(noSilentLineLoss, 13) | The model reaches the guard via the single-replica self-re-finalization route (a failed terminal persist leaves the assignment live across this replica's own finalization) — the commit's A→B→A reset-out-of-terminal route needs a reset-to-ready transition the model lacks (N4). The wired non-vacuity probe is `quint-log-witness-already-finalized-refusal` (the post-fix refusal arm is reachable). Model-B handoff: `AtMostOneFinalizingWrite` (the SELECT→PUT window under two real writers). |
| `d8944727e` | gap-merge fold counts the gap, not the marker | E | `…MarkerCountedSpan` (flap constants with `MAX_LINE = 4`) | **F**(lineSpanExact, 10); **F**(noSilentLineLoss, 11) | **Bound finding:** the shipped `MAX_LINE = 3` caps the reachable prefix→ring gap at 1, where `marker(1) == gap(1)` and the pre-fix arithmetic is accidentally exact — the shipped flap regime's green `lineSpanExact` verdict over the gap-merge fold covers only the cases where this fix is invisible. The undershoot dual of calibration entry #0's overshoot. Table-only (the `MAX_LINE = 4` space is a one-shot cost). |
| `effefb0a1` | flush span = line-number span, not physical count | E | `…PhysicalCountSpan` | **F**(lineSpanExact, 7); **F**(noSilentLineLoss, 7) | The cheapest falsification in the corpus (base regime, one holey ring, no failover) and the most plausible regression. CI: `quint-log-calib-physical-count-span`. |
| `e1fd179b9` | stored-coverage reconcile folds any non-subsumed row | E | `…DisjointFoldOnly` | **F**(storedCoverageNeverRegresses, 10); original five + noStaleSealOnLiveCarrier + finalizedRowFrozen **H** over the full 5.3M-state space | The destroyed lines were delivered only to the interim leader — outside the per-replica `delivered` ghost by design — and the overwriting row is internally consistent, so neither conservation nor span-exactness can see the regression. Resolved by the Phase-3 ghost high-water; the cross-replica conservation form stays model B's. |
| `e10deb9f6` | `req_in_tenure` keyed on the acquire-epoch | B | — | deferred | Phase 4's calibration entry #1 (model B's `TenurePinnedFinalization`). |

### Phase-6 simplification ledger

Entries the calibration *adds* to the simplification candidate list (the
"four reaps → one ungated general reap" candidate above already stands and
is strengthened by the two CI-wired reap witnesses, which define exactly
what the unification must preserve):

1. **The enqueue-failure reap is redundant for safety.** `…NoEnqueueFailReap`
   holds all nine invariants over the full fault-local space. Deleting the
   reap costs one periodic tick (≤30 s) of `GetDerivationLogs` shadowing per
   orphan and loses the only reap that survives flusher death — both
   properties the model does not encode. Flag for the reap unification;
   do not delete on the model's evidence alone.
2. **The tenure-drop reap (the drop arm's sealed-empty discard) is the same
   shape.** It is the event-coupled twin of the periodic sealed-empty reap
   (every state it fires in, the periodic reap's `reapEnabledFor` disjunct
   also covers once recovery completes); the calibration's
   `…PinAfterGuard` module disables it (along with the rest of the
   pin-first ordering) and nothing falsifies. Same disposition as #1.
3. **The binding-gate invariant is the gate's own definition.** Its
   calibration value is non-vacuity of the quantifier, not a deeper
   property; any future strengthening of the binding gate should be
   accompanied by a *content/provenance* test at the turmoil/MBT layer, not
   another model-A invariant.
4. **463090eb7's one-shot consult is confirmed dead.** The `…OneShotConsult`
   module proves the intermediate mechanism was insufficient (the orphan it
   targets is still unjustified at 10 states with the consult as the only
   reaper); no code remains to remove, but the history is worth keeping —
   it is the corpus's one example of a fix that was superseded for
   correctness rather than refactored for performance.

### NOT-ENCODED dimensions (the §3.6 deterministic-simulation evidence)

The corpus's six NOT-ENCODED rows group into two missing dimensions; the
recommendation for the design's §3.6 turmoil decision follows from how
little a model-A encoding of either would buy.

**Await/lock-granularity interleaving** (`84ac79b84`, `825cdd478`,
`abfade4d5`'s motivating race, `44905298b`'s L2-independence): the
check-…-act-on-stale class inside one flusher attempt. Encoding it requires
a per-derivation flusher program counter (`Idle | PinChecked |
GuardReturned | …`) with every other action interleavable between its
stages — multiplying the per-derivation state count by the stage count
times the latched-request space on regimes already at the per-check budget.
The decisive datum: for **every** row in this class, adding the
interleaving split alone would *still not produce a falsification* — the
reap/drain harms also need the inexpressible carrier-liveness transition
property, and the success-path freeze harms also need model B. The fixes in
this class are single `remove_if` calls whose regression tests already
exist as deterministic mid-await fault-injection unit tests (a held
`LOCK TABLE` parking the guard SELECT; `pg_terminate_backend` on the
blocked SELECT). **Recommendation: do not add await granularity to model A.
Cover the class with loom/deterministic-simulation over `LogBuffers` +
`flush_final` against a scripted `LeaderState` — which the codebase already
does by hand for the two known instances — and let model B's two-replica
whole-action interleavings subsume the cross-replica half for free.**

**Content/provenance and secondary sinks** (`496e6fb14`'s residual,
`7beb1ca00` #1, `7ffbf1415`, `8e97c2220`): the live fan-out display stream,
the unstamped-entry allocation, the phase message type, and the
poisoned-DAG-resident state. Each would add a state variable whose only
constraint is a restatement of an existing gate over a sink nothing in the
protocol reads back. **Recommendation: conventional/MBT tests own these.**

### Model-B handoff list

Rows deferred to Phase 4, with the invariant each should falsify there:

| Row | Model-B invariant | The two-replica ingredient model A lacks |
|---|---|---|
| `3ce5d03e4` (the freeze half) | `TenurePinnedFinalization` (definitionally), cross-replica `NoSilentLineLoss` | the lines only the live leader received, excluded from the frozen span when the deposed replica's stale final freezes the row |
| `7beb1ca00` #3 | cross-replica `StoredCoverageNeverRegresses` | an ex-leader's periodic `.partial` UPSERT racing the new leader's fresher partial row for the same execution |
| `44905298b` / `abfade4d5` (the headline) | `AtMostOneFinalizingWrite`, `IsCompleteMonotone` | two real writers whose guard SELECT → S3 PUT → row UPSERT windows interleave; a blob ghost distinct from the row |
| `e1fd179b9` (the conservation half) | cross-replica `NoSilentLineLoss` | the union `delivered` over both replicas' rings |
| `e10deb9f6` | `TenurePinnedFinalization` | already dispositioned as Phase 4's calibration entry #1 |

The single-replica halves of the first and fourth rows are now covered by
model A's `storedCoverageNeverRegresses`; model B's job for them narrows to
the *delivered-lines* (rather than recorded-span) form of the conservation
law.

## Campaign close-out — the subsystem this document verifies has been deleted

Everything above is a historical record. The in-scheduler build-log subsystem
this campaign modeled — `rio-scheduler/src/logs/`, the per-derivation ring
buffer, `push_for`, the gap-merge fold, the four reaps, the lease-gated
flusher, the stored-coverage reconciliation, the `drv_logs` row — was deleted
and replaced by a lease-decoupled `LogService` in rio-store (authenticated
chunked ingest, immutable session-keyed S3 chunks, a PostgreSQL line-range
manifest, read-time set-union dedup, a TTL sweep, and a read-time
completeness predicate). `logBufferLifecycle.qnt`, `calibration/`, and the
23 `quint-log-*` checks were retired with their subject; this map, the
invariant definitions, and the calibration table above are what remains. The
rule names, file paths, check attrs, and present-tense claims above describe
the deleted design and its CI wiring as they existed at the close of Phase 3.

**Outcome.** The campaign found, confirmed, fixed, and proved one new bug in
the old subsystem (calibration entry #0: the gap-merge fold double-counts a
re-delivered overlap once the stored-coverage reconcile empties the ring; the
`accounted_below` accept floor is the fix, and the ungated witness plus the
green flap regime are the machine-checked statement that the fix is necessary
and sufficient at the model's bounds). It calibrated the model against the
subsystem's 22 historical bugs: 14 are re-findable by the invariant set
(reverting the fix falsifies an invariant over the regime's full state
space), 2 are proven redundant for safety (the enqueue-failure reap and the
pin-before-guard hoist hold every invariant with the fix reverted), and the
rest are dispositioned as not-encoded or model-B in the table above. That
table — six of the 22 bugs are orphaned-entry leaks, four are finalize-once
races, and almost all of the rest are lease-coupling scar tissue — is the
documented evidence for the architecture change that obsoleted the model: the
replacement removes the state those bug classes live in rather than adding a
fifth reap or a sixth gate. The rebase audit (`harden-logs-audit.md` in the
campaign workspace) walked every bug class against the LogService and found
no exposure to the dominant ones: re-delivery lands as a manifest overlap
that the read path dedups instead of a span overstatement, and there is no
retained entry to orphan and no finalizing write to race.

**The nine invariants against the LogService**, per the rebase audit:

| Invariant | Disposition | Why |
|---|---|---|
| `noCrossExecContamination` | **Structural** | Storage is keyed by `exec_id` end-to-end (chunk key, manifest PK, the stream bound to one execution at open); a re-dispatch mints a new `exec_id` and a new key namespace. The in-memory carrier a restamp could contaminate does not exist — the ingest session is created per stream and dropped with it. |
| `lineSpanExact` | **Gap → fixed** | The per-chunk half is structural (a chunk's recorded count is the physical length of a contiguous run; no gap arithmetic exists to overshoot). The log-level half moved into the completeness gate, whose post-terminal MUST (drop accepted lines at or past `final_line_count`) was specified but unimplemented — the one real protocol finding of the audit, fixed during integration by enforcing the gate on every append rather than only at stream open. |
| `bindingGateExcludesForeignExecutors` | **Structural** | Relocated and strengthened: `store.log.append-auth` (HMAC token → derivation match → latest-assignment + builder binding) runs at stream open, and the stream is then bound to one `(exec_id, session_id)` whose chunk keys cannot name any other execution. |
| `everyRetainedEntryIsJustified` | **Vacuous** | The four orphan shapes and their three differently-gated reapers are gone. The only retained in-memory state is the per-stream ingest buffer, whose lifetime is the gRPC stream's; the persistent analog (a manifest row) is justified by definition and reaped only by the TTL sweep. |
| `noSilentLineLoss` | **Structural** | A line leaves the builder's retransmit buffer only when an ack covers it; an ack is sent only after the manifest INSERT commits; the manifest row is written only after the object PUT. Every remaining loss channel is disclosed (the computed `is_complete = false`, the abandoned-drain counter, the missing-object read error). |
| `noStaleSealOnLiveCarrier` | **Vacuous** | No seal latch and no carrier. Completeness is a monotone predicate recomputed from the DB on every open and every read; a seal that outlives the state that justified it is unrepresentable. |
| `finalizedRowFrozen` | **Vacuous** | There is no finalized row to freeze: chunks are write-once, the manifest is INSERT-only, `final_line_count` is stamped once at terminal, and no finalizing write consolidates other writers' data. |
| `storedCoverageNeverRegresses` | **Vacuous** | Coverage is the union of INSERT-only manifest rows; there is no reconcile that folds a stored row into a new write, so nothing can replace a larger span with a smaller one. Overlapping sessions union at read time. |
| `boundsOK` | **Vacuous** | The model's own state-ceiling tripwire (a TLC finite-state-space artifact) has no subject without the model. The implementation bounds it stood beside (the ring byte cap, the periodic flush) are replaced by `store.log.ingest-bounds`. |

**Successor.** The continuing campaign re-targets onto the LogService
session/chunk protocol (model C: ingest sessions, chunk manifests, the
read-time dedup, the TTL sweep, and the completeness gate) — a much smaller
model with no lease composition, no reaps, and no tenure pins. Its plan is
`log-formal-reshape-plan.md` in the campaign workspace; its acceptance test
is this document's calibration table re-run against the new architecture,
each of the 22 rows becoming either "no exposure by construction" with the
structural reason or "the equivalent hazard exists at this site and this
invariant checks it". Model B is cancelled outright — its subject
(cross-replica tenure-pinned finalization of a shared `drv_logs` row) does
not exist in the new architecture — and the Phase-6 simplification ledger
closes with "the architecture change was the simplification": the cutover
deleted every mechanism the calibration proved redundant, and the rest.

## Model C — the LogService acceptance table

`docs/spec/models/logService.qnt` is the successor model: the builder's
at-least-once ack-trimmed uploader, the open-time binding + completeness
gate, the per-stream ingest session and its accept predicate, the
INSERT-only chunk manifest, the read path's ordered-walk dedup over
overlapping sessions, the read-time completeness fold, and the TTL sweep.
Four exhaustive TLC regimes (`quint-log-service-{base,redispatch,resend,
sweep}` in `nix/quint.nix`) assert its invariants over their full
reachable state spaces; twelve expect-violation witness checks pin the
contended states as reachable. Measured state counts and wall-clocks are
in the introducing commit's message and the checks' transcripts.

### The model-C invariants

| Invariant | Predecessor | What it checks |
|---|---|---|
| `noCrossExecContamination` | `noCrossExecContamination` | Execution E's manifest holds only lines E's own bound builder sent. The defense is structural (the chunk key embeds the exec_id the stream was bound to at open); the invariant is the regression guard that the cut keys its chunk by the cutting session's execution. |
| `authGateExcludesForeignWriters` | `bindingGateExcludesForeignExecutors` | The stream-open predicate admits a claimed execution only when it is the derivation's latest assignment attempt. |
| `noSilentLineLoss` | `noSilentLineLoss` | The four-disjunct disclosed-loss form: every line the build emitted is in the builder's retransmit buffer, in a store-side ingest buffer, covered by a manifest chunk, or the loss is disclosed (the log does not read as complete / the uploader recorded its abandonment). |
| `ackImpliesDurable` | — (new) | The honest uploader's load-bearing refinement of the conservation law: every line below the builder's acked watermark is manifest-covered until the TTL sweep removes it. Asserted only in the regimes without the fabricating client (see the val's doc for why a client that mis-numbers its own lines forfeits the per-ack durability claim). |
| `servedSpanExact` | `lineSpanExact` | The read path's ordered-walk-with-watermark over the manifest — including overlapping chunks from two sessions — yields exactly the union of the chunks' line ranges, each line exactly once. The fold is encoded as the code's walk, not as the union it is supposed to compute. |
| `completeLogServesAllProduced` | — (the conservation law's bite, restated) | A log that reads complete serves every line the build produced. |
| `completenessGate` | — (new; the audit's defect #1) | Once a session has learned the execution's recorded `final_line_count`, every line it subsequently accepts is below it. Lines accepted before the mid-stream refresh observed the seal are the disclosed bounded residual. |

### The 22-bug corpus against model C

Verdict legend: **CONSTRUCTION** = the state or mechanism the bug lived
in does not exist in the new architecture; the model's transition
relation cannot represent the harmful write. **CHECKED(inv, regime)** =
the equivalent hazard exists at the named site and the named invariant
holds over the named regime's full reachable state space, with the
contended scenario pinned reachable by the named witness check.
**OUTSIDE** = no model footprint then, no model footprint now;
conventional tests own it.

| Original row | Old fix | Model-C verdict | Where the hazard went |
|---|---|---|---|
| `3a55474ac` | accounted-floor accept gate | **CHECKED**(`servedSpanExact`, resend) | The re-delivered batch after an ambiguous disconnect lands as a *manifest overlap* (a second session's chunk covering the same line range), not a span overstatement — there is no gap-merge fold to double-count it. The hazard moved to the read path's `(first_line, session_id)` ordered walk and `LineCursor` watermark (`tail.rs`), which must serve each overlapped line exactly once; `quint-log-service-witness-overlap` and `-dedup` pin the overlap and the suppressed duplicate as reachable. The within-one-session re-delivery is rejected by `high_water_line` (the monotone floor — the `accounted_below` fix built in from the start, never reset by the buffer emptying). |
| `496e6fb14` | recv-task (executor, drv) binding gate | **CHECKED**(`authGateExcludesForeignWriters`, redispatch) | Relocated to `gate.rs::check_append_open`: HMAC token, token-vs-header derivation match, latest-assignment + builder binding. The model checks the latest-assignment half (the superseded-token rejection, pinned by `quint-log-service-witness-superseded-rejected`); the identity comparisons are pure equality on signed token fields. The content/provenance harm is still erased by the line-as-integer encoding — the same documented price the predecessor paid. |
| `496e6fb14` (residual) | legacy `push()`'s `or_default()` unstamped-entry allocation | **CONSTRUCTION** | There is no entry to allocate: a stream that fails the gate never creates a session, and the per-replica stream-count and byte-budget semaphores bound the resource the unstamped entry leaked. Resource bounds, outside the model. |
| `7beb1ca00` #1 | gateway `ForwardLogBatch` gated on the accept verdict | **CONSTRUCTION** | The live-tail fan-out happens inside `accept()` after every gate (`ingest.rs`), so a rejected batch is never fanned out. The fan-out sink itself is read-side plumbing outside the model. |
| `7beb1ca00` #2 | `FlushRequest.exec_id` stale-drain pin | **CONSTRUCTION** | No finalizing drain exists. A cut drains the cutting session's own buffer into the cutting session's own `(exec, session)` keyspace; there is no request that names an execution other than the one the stream was bound to at open. `noCrossExecContamination` is the regression guard. |
| `7beb1ca00` #3 | `LogFlusher` periodic arms leader-gated | **CONSTRUCTION** | No leader, no flusher. The periodic cut is per-stream and INSERT-only; two replicas concurrently cutting for one execution are two sessions whose chunk sets union at read time instead of two writers racing an UPSERT. |
| `7ffbf1415` | `BuildPhase` actor-side executor gate | **OUTSIDE** | A sibling message type with no log-subsystem footprint in either architecture. |
| `6c26e85f8` | cross-exec restamp clears the retained lines | **CONSTRUCTION** + **CHECKED**(`noCrossExecContamination`, redispatch) | There is no restamp and no in-memory carrier that survives a re-dispatch: the new execution gets a new exec_id, a new session, a new buffer, and a new chunk-key namespace. The contended state — both executions' logs growing concurrently while the superseded session still holds lines — is reachable (`quint-log-service-witness-concurrent-execs`) and the invariant holds over it. |
| `699ea1692` | cross-exec restamp clears the seal | **CONSTRUCTION** | No seal latch. Completeness is a predicate computed per read from the execution's own lifecycle row and manifest; a prior execution's completeness cannot mute a live execution's stream because the gate consults the *claimed* execution's state, not a shared per-derivation tombstone. The muting state (`noStaleSealOnLiveCarrier`'s subject) is unrepresentable. |
| `2f9a747d0` | same-exec restamp clears the seal + final-pending mark | **CONSTRUCTION** | Same as above for the seal half. The final-pending mark has no analog (no deferred final). A reconnecting builder opens a fresh session whose admission is recomputed from the DB. |
| `6aab70b47` | enqueue-failure reap | **CONSTRUCTION** | No flush queue, no enqueue failure, no orphaned entry. The per-stream buffer's lifetime is the gRPC stream's (a panic-safe scopeguard deregisters it on every exit path). |
| `463090eb7` | one-shot finalized-elsewhere consult | **CONSTRUCTION** | No finalized-elsewhere state to consult and no retained entry to reap. |
| `f8ce10b8e` | recurring refused-UPSERT reap | **CONSTRUCTION** | No UPSERT to refuse. The manifest is INSERT-only with `ON CONFLICT DO NOTHING` on a per-attempt key. |
| `81824cfbb` | periodic sealed-empty reap | **CONSTRUCTION** | No sealed entry. |
| `84ac79b84` | stale-request guard atomic with the drain | **CONSTRUCTION** | No `flush_final` and no drain of state another actor can restamp. The cut's drain and commit operate on the cutting session's own buffer under its own mutex; the await-granularity window between them is the seam invariant (`buffer`/`in_flight`/manifest), pinned by the `subscribe_during_in_flight_cut_sees_drained_lines` unit test rather than the model (the same whole-action-atomicity boundary the predecessor drew). |
| `3ce5d03e4` | deferred-final tenure pin | **CONSTRUCTION** | The harm — a stale writer overwriting the row a fresher writer extended — requires a writer that overwrites. Chunks are write-once, the manifest is INSERT-only, and there is no deferred request that outlives the authority that enqueued it. |
| `646022e37` | tenure pin hoisted before the destructive arms | **CONSTRUCTION** | No destructive arms and no tenure to pin against. |
| `825cdd478` | atomic tenure-drop reap + post-await re-validation | **CONSTRUCTION** | No tenure-drop reap. |
| `2c301438d` | disconnect-time discard gated on an authoritative DAG | **CHECKED**(`noSilentLineLoss`, all four regimes) | The one undisclosed-loss bug of the old design. The new design's disconnect paths either *drain* the buffer (the clean half-close cuts every remaining run) or *drop* it (the abort path) — and every dropped line is still in the builder's ack-trimmed retransmit buffer, because an ack is sent only after the manifest INSERT commits. The four-disjunct conservation law holds over every regime's full state space, including the abort-then-replay and abandon-with-unacked paths (`quint-log-service-witness-abandoned` pins the disclosure channel as exercised). `ackImpliesDurable` is the no-early-ack refinement that keeps disjunct 1 honest. |
| `e6c18add2` | acquisition-time stale-buffer sweep | **CONSTRUCTION** | No acquisition and no stale retained buffer to sweep. |
| `8e97c2220` | sweep spares only non-terminal DAG entries | **CHECKED**(`noSilentLineLoss` + `ackImpliesDurable`, sweep) | The old sweep's hazard was deleting an entry whose data was still needed. The new TTL sweep's only guard is wall-clock age; the model checks that only an expired execution's chunks are ever deleted, that the deletion is disclosed (the log stops reading complete the moment its manifest or lifecycle row goes), and that the chunks-before-execution-row deletion order's crash window violates nothing. `quint-log-service-witness-swept` pins the deletion as reachable. |
| `abfade4d5` | periodic frozen-row UPSERT latch | **CONSTRUCTION** | `is_complete` is not stored; it is computed per read from monotone inputs (a terminal status never un-sets, the manifest only grows until the TTL sweep). A downgrade is unrepresentable in the transition relation — no action removes a chunk or a count except the sweep, whose deletion is the retention policy, not a regression. |
| `44905298b` | `flush_final` already-finalized consult | **CHECKED**(`completenessGate`, base) | No finalizing write to race; the one decide-once datum (`final_line_count`) is stamped by a single-threaded actor and a duplicate stamp writes the same value. The closest surviving analog is the open-time seal — a complete log rejects further `AppendLog` opens — checked by `openAdmitted`'s `not(logIsComplete)` conjunct and pinned reachable by `quint-log-service-witness-complete-open-rejected`. |
| `d8944727e` | gap-merge fold counts the gap, not the marker | **CHECKED**(`completeLogServesAllProduced`, base) | No gap-merge fold. A forward gap splits the cut into two chunks and is *visible in the manifest* as non-contiguous rows; the completeness fold (`gate.rs::manifest_covers_contiguously`) reports it as incomplete instead of counting it into a span. The gapped manifest is reachable (`quint-log-service-witness-gapped-manifest`) and a log never reads complete across a gap. |
| `effefb0a1` | flush span = line-number span, not physical count | **CHECKED**(`servedSpanExact`, base + resend) | A chunk's recorded `line_count` is the physical length of the contiguous run it was cut from, so its line-number span and its physical count are equal by construction — the two quantities the old fix had to reconcile cannot diverge. `servedSpanExact` checks the read path serves exactly the recorded intervals. |
| `e1fd179b9` | stored-coverage reconcile folds any non-subsumed row | **CONSTRUCTION** | No reconcile and no fold of a stored row into a new write. Coverage is the union of INSERT-only manifest rows; the interim-leader-extended-row scenario maps to "session B committed chunks while session A was partitioned", which the read path unions instead of overwriting. |
| `e10deb9f6` | `req_in_tenure` keyed on the acquire-epoch | **CONSTRUCTION** | No tenure and no deferred request to validate against one. (Model B's calibration entry #1; model B is cancelled.) |

**Summary.** Of the 27 calibration rows covering the 22 historical bugs:
**18 are closed by construction** — the state they lived in (the shared
ring entry, the seal latch, the finalizing write, the four orphan shapes,
the lease-coupled flusher) does not exist, and the model's transition
relation cannot express the harmful write; **7 are checked by an
invariant** over an exhaustive regime whose contended scenario a witness
check pins as reachable (the re-delivery overlap, the foreign-writer
rejection, the disconnect-time loss, the sweep's deletion scope, the
finalize-once seal, the gap accounting, and the span arithmetic); **2
are outside the model** in both architectures (display-stream and
sibling-message-type gates owned by conventional tests). No row is
exposed without a check.
