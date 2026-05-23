# Build-log invariant ↔ spec-rule map

Working artifact for the log-formal campaign's Phase 1 (spec audit). Maps the
ten invariants of the build-log verification design (§3.4: five model-A
invariants over the single-replica entry lifecycle, five model-B invariants
over cross-replica finalization) onto the existing `obs.log.*` /
`sched.log.*` / `sched.merge.*` / `sched.recovery.*` rule set, and records
what each rule does and does not normatively cover. Phase 2 moves the
model-A half of this table into `logBufferLifecycle.qnt`'s header comment.

Verdict legend:

- **COVERS** — the rule's normative MUST sentence states the invariant (or a
  statement the invariant is a direct restatement of in interval algebra).
- **PARTIAL** — a rule exists and covers part of the invariant; the missing
  piece is named.
- **GAP** — no rule states the invariant.
- **CONTRADICTION** — the merged code does not do what a normative rule says
  it MUST (none found; one documented residual is noted under
  `AtMostOneFinalizingWrite`).

## Model A — single-replica entry lifecycle

| Invariant | Rule(s) | Verdict | Notes |
|---|---|---|---|
| `NoCrossExecContamination` | `obs.log.exec-keyed`, `sched.log.batch-binding` | **PARTIAL** → resolved by the Task-1.5 extension of `obs.log.exec-keyed` | `exec-keyed` normatively requires one blob and one PG row per execution (so exec E2's stored artifacts are keyed apart from E1's); `batch-binding` normatively drops batches from executors that do not hold the assignment. Neither states the in-memory half: a ring entry restamped to a different execution must not carry the prior execution's lines, seal, or final-pending mark into the new execution's flush. That clearing is what `LogBuffers::set_exec`'s cross-exec arm implements and what keeps E1's lines out of E2's blob; it is documented only in rustdoc. |
| `LineSpanExact` | `obs.log.gap-span` | **PARTIAL** → resolved by the Task-1.5 extension of `obs.log.gap-span` | The rule's write-path MUST is conditioned on "a row whose blob folds a recovered pre-failover prefix". The code keeps `first_line + line_count` one past the highest true worker line for *every* recorded row, including a hole-carrying payload with no folded prefix (a re-acquired ex-leader's retained ring whose interior hole was delivered only to an interim leader that never flushed) — `upload_and_record` records the ring's line-number span, not its physical count, unconditionally. The read-path MUST (physical-vs-claimed divergence ⇒ full re-serve) is already unconditional. |
| `BindingGateExcludesForeignExecutors` | `sched.log.batch-binding` | **COVERS** | "MUST drop batches whose `derivation_path` does not match an active assignment held by the calling executor's stream" is exactly the gate; `push_for` evaluates it under the entry's shard write-lock so the check and the append are atomic. `sched.log.phase-binding` is the sibling for `BuildPhase` (not part of the log-buffer model). `sched.log.path-length` and `sched.executor.input-bounds+2` bound the same ingestion surface (path length, monotone line numbering, overflow) — adjacent, modelled as part of the accept/reject predicate in Task 2.3. |
| `EveryRetainedEntryIsJustified` | `obs.log.entry-justified` (new, Task 1.4); `sched.recovery.log-buffer-sweep+2` covers one restoration point | **GAP** → resolved by Task 1.4 | Before Task 1.4 the four flusher/epilogue reaps (tenure-orphan, refused-UPSERT, sealed-empty, enqueue-failure) had no unifying rule; only the acquisition-time sweep (`sched.recovery.log-buffer-sweep+2`) and the poison-TTL backstop were normative. The new rule states the justification set (live in an authoritative DAG ∨ unresolved final ∨ DAG not authoritative) and the obligation that an entry that has lost every justification is reaped by the next path that observes it. The model's check is that the reap set is *complete* (no fifth orphan shape). |
| `NoSilentLineLoss` (per-replica) | `obs.log.line-conservation` (new, Task 1.3); pieces in `obs.log.{gap-span, ring-byte-cap, periodic-flush, incomplete-surfaced, stored-coverage-preserved}` | **PARTIAL** → resolved by Task 1.3 | The per-piece rules each bound or surface one loss channel (head eviction, the ≤30s failover window, the incomplete indicator, the prior-tenure fold). No rule states the conjunction: a delivered line is in the ring, in the stored coverage, counted in the recorded span as a gap, or revealed as lost by the record itself (`first_line` above the true start / `is_complete = false`). The model checks the conjunction over the ghost `delivered` interval. |

## Model B — cross-replica finalization

| Invariant | Rule(s) | Verdict | Notes |
|---|---|---|---|
| `TenurePinnedFinalization` | `obs.log.deferred-final-retry+4` | **PARTIAL at +3** → resolved by the Task-1.2 bump | At `+3` the normative drop condition was "the replica no longer holds the lease or its generation has moved on" — the generation alone cannot identify the tenure once the recovery PG-floor seed has saturated it past `leaseTransitions + 1` (an A→B→A holder change is then a generation no-op), which is exactly the `req_in_tenure` bug the rebase fixed. `+4` makes the recorded acquire-epoch the normative discriminator; the generation stays as rationale (a conservative additional break on the mid-tenure floor seed). |
| `IsCompleteMonotone` | `obs.log.finalize-immutable` | **COVERS** | "Row content MUST NOT be overwritten or regressed by a later flush" includes the `is_complete` column itself; `upsert_drv_log`'s `WHERE NOT drv_logs.is_complete` clause refuses every write to a finalized row. |
| `AtMostOneFinalizingWrite` | `obs.log.finalize-immutable` | **COVERS** (one documented residual, below) | "A flusher holding retained lines for an already-finalized execution MUST discard them instead of re-finalizing" is the at-most-once obligation; the row consult before any S3 work plus the frozen-row UPSERT are the two enforcement layers. **Residual for the Phase-4 model to adjudicate:** the guard SELECT and the S3 PUT are not atomic across replicas — a concurrent finalization landing between another replica's guard read (`is_complete = false`) and its PUT overwrites the finalized blob in place while the frozen row keeps describing the replaced content (`upsert_drv_log`'s refused-write comment). The row-level invariant holds; the blob-level "MUST NOT be overwritten" has this race window. Not recorded as a CONTRADICTION because the obligation's enforcement exists and the window is documented as accepted in the code; model B decides whether the window is reachable under the lease layer's NeverDual guarantee and whether the rule needs a carve-out or the code a fix. |
| `StoredCoverageNeverRegresses` | `obs.log.stored-coverage-preserved` | **COVERS** | "Content recorded by a prior tenure and not contiguously covered by the current ring MUST NOT be overwritten; the flusher MUST fold the stored blob in" is the no-regression statement; the carve-outs (same-tenure ring eviction, never-flushed interim-leader lines) are in the trailing prose and are the same carve-outs the model's `stored` ghost variable encodes. |
| `NoSilentLineLoss` (cross-replica) | `obs.log.line-conservation` (new, Task 1.3) | **PARTIAL** → resolved by Task 1.3 | Same statement as the per-replica form; the rule is written replica-agnostically (the ring entry, the shared stored coverage, the recorded span) so the cross-replica union form is the same rule read over both replicas' rings plus the shared store. |

## Rules in the log-domain inventory not load-bearing for any §3.4 invariant

- `obs.log.batch-64-100ms`, `obs.log.worker-header` — worker-side framing and
  display headers; conventional tests cover them.
- `obs.log.incomplete-surfaced` — the read-path surfacing of
  `is_complete = false`; referenced by `line-conservation`'s rationale as the
  channel that discloses unaccounted tail loss, but the CLI/dashboard
  rendering itself is outside the model.
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

## Verify-marker status (Phase-2 wiring targets)

`tracey query untested` filtered to `obs.log.*` / `sched.log.*` /
`sched.merge.exec-correlation` / `sched.recovery.log-buffer-sweep` at the
time of this audit: **no rows**. Every pre-existing rule in the inventory
already carries at least one `r[verify]` site, so none of them is a Phase-2
wiring target on verify-coverage grounds (Phase 2 still adds model-backed
`r[verify]` markers at the `nix/quint.nix` wiring points for the invariants
each check enables).

The two rules added by this phase are deliberately left without `r[verify]`
markers — their verification is the Phase-2 model checks, not a unit test:

- `obs.log.line-conservation` — verified by model A's `NoSilentLineLoss`
  check (Task 2.10/2.11).
- `obs.log.entry-justified` — verified by model A's
  `EveryRetainedEntryIsJustified` check (Task 2.10/2.11).

Until those checks land, `tracey query untested` is expected to list exactly
these two rules for the log domain.
