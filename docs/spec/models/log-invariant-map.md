# Build-log invariant ↔ spec-rule map

Working artifact for the log-formal campaign's Phase 1 (spec audit). Maps the
ten invariants of the build-log verification design (§3.4: five model-A
invariants over the single-replica entry lifecycle, five model-B invariants
over cross-replica finalization) onto the `obs.log.*` / `sched.log.*` /
`sched.merge.*` / `sched.recovery.*` rule set. Phase 2 moves the model-A half
of this table into `logBufferLifecycle.qnt`'s header comment.

This is the post-audit state: every invariant now maps onto a rule whose
normative MUST sentence states it (the COVERS column is total). The "audit
finding" column records what the pre-audit rule set was missing and which
Phase-1 task closed it, so the model phases can cite why each rule has the
shape it has.

## Model A — single-replica entry lifecycle

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

## Verify-marker status (Phase-2 wiring targets)

`tracey query untested` filtered to `obs.log.*` / `sched.log.*` /
`sched.merge.exec-correlation` / `sched.recovery.log-buffer-sweep` after the
Phase-1 commits lists exactly two rows:

- `obs.log.line-conservation` — verified by model A's `NoSilentLineLoss`
  check (Task 2.10/2.11).
- `obs.log.entry-justified` — verified by model A's
  `EveryRetainedEntryIsJustified` check (Task 2.10/2.11).

Both are deliberate: their verification is the Phase-2 model checks (the
`r[verify]` markers land at the check wiring points in `nix/quint.nix`), not
a unit test. Every other rule in the inventory already carries at least one
`r[verify]` site. `tracey query uncovered` shows no log-domain rows.
