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
(each asserts all five invariants plus the `boundsOK` ceiling tripwire over
its regime's full reachable state space); non-vacuity pinned by the eleven
`quint-log-witness-*` expect-violation checks plus
`quint-log-witness-gap-span-ungated`.

| Invariant | Rule(s) | Verdict | Audit finding |
|---|---|---|---|
| `NoCrossExecContamination` | `obs.log.exec-keyed+2`, `sched.log.batch-binding` | **COVERS** | Was PARTIAL at `exec-keyed+1`: the rule required one blob and one PG row per execution but said nothing about the in-memory ring entry that feeds them — a cross-exec restamp carrying the prior execution's lines, seal, pending-final mark, or cached prefix into the new execution's flush would defeat the storage keying. `+2` makes the cross-exec clearing normative; `batch-binding` covers the ingress half (a foreign executor's batch never enters the entry). |
| `LineSpanExact` | `obs.log.gap-span+2` | **COVERS** | Was PARTIAL at `gap-span+1`: the write-path MUST was conditioned on "a row whose blob folds a recovered pre-failover prefix", leaving the hole-carrying-payload-without-a-prefix shape (an interior hole of lines delivered only to an interim leader) outside the normative scope even though the code records the true span unconditionally and the rule's own read-path half depends on it. `+2` broadens the subject to any flush payload that does not physically contain every line of the range it covers. |
| `BindingGateExcludesForeignExecutors` | `sched.log.batch-binding` | **COVERS** | "MUST drop batches whose `derivation_path` does not match an active assignment held by the calling executor's stream" is exactly the gate; `push_for` evaluates it under the entry's shard write-lock so the check and the append are atomic. `sched.log.path-length` and `sched.executor.input-bounds+2` bound the same ingestion surface (path length, monotone line numbering, overflow) — modelled as part of the accept/reject predicate in Task 2.3. |
| `EveryRetainedEntryIsJustified` | `obs.log.entry-justified` | **COVERS** | Was a GAP: the four flusher/epilogue reaps (tenure-orphan, refused-UPSERT, sealed-empty, enqueue-failure) had no unifying rule; only the acquisition-time sweep (`sched.recovery.log-buffer-sweep+2`) and the poison-TTL backstop were normative. The new rule states the justification set (non-terminal in an authoritative DAG ∨ unresolved pending final ∨ DAG not authoritative) and the obligation that an entry that has lost every justification is reaped by the next path that observes it. The model's check is that the reap set is *complete* (no fifth orphan shape). |
| `NoSilentLineLoss` (per-replica) | `obs.log.line-conservation` | **COVERS** | Was PARTIAL: the per-piece rules (`gap-span`, `ring-byte-cap`, `periodic-flush`, `incomplete-surfaced`, `stored-coverage-preserved`) each bound or surface one loss channel; no rule stated the conjunction. The new rule states it: a delivered line is in the ring, in the stored coverage, or counted in the recorded span as a gap, and may only leave all three through a loss channel the stored record itself discloses (a `first_line` above the true start, or a record that never claims completeness). |

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
