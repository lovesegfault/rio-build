# Chunk-refcount invariant ↔ spec-rule map

Working artifact for the refcount-formal campaign's Phase 0, Stage A (the
spec audit). Maps the design's invariants over the rio-store chunk
reference-counting subsystem — CR-1..CR-4 and the supporting S/L items of
`refcount-formal-design.md` §3.3 — onto the `store.chunk.*` / `store.cas.*` /
`store.gc.*` / `store.put.*` rule set in `docs/spec/components/store.typ`,
cross-referenced against the protocol's mutation and decision sites as the
protocol inventory (`refcount-inventory.md`) catalogs them. The companion
Stage-A deliverable is the consumer audit (design §5a) appended at the end of
this document.

This is the post-audit state: every committed invariant (CR-1..CR-4) maps
onto at least one rule whose normative body states it (the GAP rows below
were closed by new `#r()` rules added by this audit, with the rationale prose
after each block), every place existing spec text does not describe what the
code does is a CONTRADICTION row (recorded, not fixed — the code is not
touched in Phase 0, and the named stale paragraphs are flagged in place
rather than rewritten), and every place an existing rule covers only part of
an invariant is a PARTIAL row with the missing piece named. No existing rule
body was amended or version-bumped by this audit: the audit found no PARTIAL
whose normative meaning must change in Phase 0 (the planned Phase-1
amendments are listed at the end so they are not silently forgotten).

Subject and evidence base: `rio-store` at the `formal-sprint` branch point
this worktree was cut from; file:line evidence for every protocol claim is
the inventory (cited as "inventory §N"). The Stage-B model (`chunkLiveness.qnt`)
and the Stage-C calibration were delivered after this audit and carry their
own sections below; the audit text in this section makes no claims about
them.

## The decision sites (the columns of every row below)

The counter has one increment site, two decrement statements, and a small
fixed set of readers; everything else is the ownership/repair machinery that
makes them fire exactly once each per manifest reference. Site keys below are
used throughout the audit-finding column.

| Key | Site | What it does |
|---|---|---|
| INC | `upgrade_manifest_to_chunked` (`rio-store/src/metadata/chunked.rs`), called from `cas::stage_chunked` | `INSERT … ON CONFLICT DO UPDATE SET refcount = refcount + 1, deleted = false RETURNING (uploaded_at IS NULL)` over the manifest's deduped hashes; same transaction as the `manifest_data` INSERT, under `FOR UPDATE` of the `'uploading'` manifests row |
| DEC-1 | `delete_manifest_chunked_uploading_inner` (`metadata/chunked.rs`), called from `cas::rollback` | In-process rollback decrement, gated on the `PlaceholderToken` (`updated_at` epoch) still matching; decrement precedes the `manifest_data` DELETE in the same transaction |
| DEC-2 | `decrement_hashes_and_enqueue` (`gc/mod.rs`) | Decrement **by count** (a chunk referenced by N dying manifests loses N); same transaction as the `narinfo` DELETE (CASCADE → manifests → manifest_data) |
| ZERO | same transaction, immediately after DEC-2 (`gc/mod.rs`) | `UPDATE … SET deleted = true, uploaded_at = NULL WHERE refcount = 0 AND NOT deleted RETURNING` → `enqueue_chunk_deletes` into `pending_s3_deletes` |
| ZERO-2 | `sweep_orphan_chunks` / `sweep_orphan_batch` (`gc/sweep.rs`), hourly | Standalone reaper for chunks already at `refcount = 0`, `deleted = FALSE`, older than `CHUNK_GRACE_SECS`; re-checks the predicate at UPDATE time |
| RES | the INC statement's `deleted = false` arm + ZERO clearing `uploaded_at` | A re-upload resurrects a soft-deleted chunk; the cleared `uploaded_at` forces the re-uploader to re-PUT |
| DRN | drain task (`gc/drain.rs`), 30 s | Per outbox row: `FOR UPDATE SKIP LOCKED` on the row, re-check `(deleted AND refcount = 0) … FOR UPDATE` on the chunk, S3 DeleteObject, delete row; resurrection ⇒ skip + drop row; 10 attempts then the stuck alert |
| RDR | the only production readers of the counter | ZERO's predicate, ZERO-2's candidate SELECT + UPDATE re-check, DRN's re-check, and the `idx_chunks_gc` partial index. Nothing else in the serving path reads it (the dedup decision reads `uploaded_at` — M_033) |

Callers that drive the decrement paths (inventory §2.1): writers W1
`PutPath` (input-addressed), W2 `PutPath` (floating-CA), W3 `PutPathBatch`,
W4 `Substituter::try_upstream` — all funnel INC through the same two
functions; the matching cleanup is owned per-writer by the explicit abort,
the `put_chunked` rollback (DEC-1), the drop-guard reap, the hot-path stale
reclaim, the 15-minute orphan scanner, and the path-GC sweep (all DEC-2).
Background actors: hot-path reclaim (on claim conflict, 300 s threshold),
orphan scanner (15 min), path-GC sweep (`run_gc`, on demand), orphan-chunk
sweep (1 h), drain (30 s). The fault alphabet for the model is the 12 crash
windows of inventory §2.2 (C1–C12), taken as given.

## The invariant ↔ rule map

Verdict legend: **COVERS** — the rule's normative body states the invariant
(or the load-bearing piece of it). **PARTIAL** — the rule states a piece; the
missing piece is named. **GAP** — no rule states it; closed by a new `#r()`
rule in this audit (or recorded as deliberately left open for supporting
invariants). **CONTRADICTION** — spec text (rule or prose) does not match
what the code does; recorded in the contradiction table, not fixed here.

### CR-1 `NoLiveChunkCollected` (= inventory S1; survives the replacement unchanged)

*State form: a chunk referenced by any existing `'complete'` manifest has its
backend object present. Action form: a backend DeleteObject fires only in
states where no existing manifest of any status references the chunk.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `store.chunk.no-live-collect` *(new)* | **COVERS** | Was a GAP: every mechanism below had a rule, the property they jointly enforce had none. The new rule states both forms and the `'uploading'` exclusion of the state form (write-ahead PUTs may still be in flight; inventory §6 S1). Mechanism commentary is after the block, not in the body. |
| `store.gc.pending-deletes` | **PARTIAL** | Covers the action-form guard at the irreversible step: outbox in the same transaction as the soft-delete (ZERO), drain re-check by `blake3_hash` before DeleteObject, skip + drop row on resurrection (DRN). Does not state the end-to-end property (it is an outbox-pattern rule). The re-check is described as "re-checks the chunk state"; the load-bearing `refcount = 0` conjunct of that re-check is named only in the prose list under the rule — the conjunct the replacement drops (design §4.2/§4.3), so its job must be re-homed when that happens, not silently lost. |
| `store.chunk.refcount-txn` | **PARTIAL** | The write-ahead increment (INC in the same transaction as `manifest_data`) is what keeps an in-flight upload's chunks out of ZERO/ZERO-2/DRN eligibility today. States the transactional pairing, not the protection it buys. (Prose nit: the rule says "step 2 of PutPath"; the flow list it points at numbers the write-ahead manifest as step 3.) |
| `store.chunk.grace-ttl` | **PARTIAL** | The grace term protecting young zero-reference chunks (and, post-replacement, the mark-snapshot window). Eligibility rule only. |
| `store.cas.chunk-upload-committed` | **PARTIAL** | The soft-delete clearing `uploaded_at` (so a resurrecting writer re-uploads instead of skipping against a deleted object) — the RES half of the defense. |
| `store.gc.two-phase` | **PARTIAL** | The sweep-side chunk clause: DELETE narinfo + decrement + zero-detect + enqueue in one transaction (DEC-2/ZERO at the path-sweep site), refcounts re-read at sweep time rather than from the mark snapshot. |

§5a no-go trigger (c) checked here: the `'uploading'`-manifests-count-as-live
choice (design §4.1 step 2) does **not** contradict path-level GC root
semantics — the spec already treats `'uploading'` manifests as mark seeds
(`store.gc.two-phase`) and gives placeholders GC-protecting references from
the instant they commit (`store.put.placeholder-refs`), so counting their
chunks as live is the chunk-level image of an existing path-level commitment.
No spec decision needs escalation.

### CR-2 `BoundedGarbageRetention` (= L1+L2; survives with a new bound)

*If a chunk stays unreferenced it is eventually soft-deleted and its backend
object deleted (or the outbox row parks at the attempts cap and alerts),
within a stated bound; the corrupt-`chunk_list` regime is a carved,
observable suspension, never a silent one.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `store.gc.bounded-garbage-retention` *(new)* | **COVERS** (with the carved corrupt-regime form) | Was a GAP: no rule stated the eventual-collection obligation or any bound. The new rule states the obligation, a compositional bound (one pass of the applicable reclamation path + grace + drain lag), and the corrupt-`chunk_list` carve-out with the observability requirement. The as-built code satisfies the carve-out only in its weak form (the skip is logged) — the as-built suspension is permanent, which the rationale prose after the block records explicitly. The known as-built voids of the bound (C12 permanent leak, G2-class missed decrements) are pre-registered here as the expected Stage-B behavior of this invariant: as-built, CR-2 holds only conditionally on CR-3. |
| `store.chunk.grace-ttl` | **PARTIAL** | Defines GC eligibility ("zero manifest references AND older than grace"). Note the predicate is already phrased reference-wise while the implementation's predicate is counter-wise (`refcount = 0`); they diverge exactly when the counter is wrong — the divergence CR-3 sanctions transiently (C1–C7) and permanently (C12). The rule's rationale sentence (a rollback racing a concurrent skip-then-increment uploader) describes a pre-`RETURNING`-atomic flow; the surviving job of grace is broader (design §4.1 names three). Phase-1 amendment, not bumped here. |
| `store.gc.pending-deletes` | **PARTIAL** | The tail of the bound: 30 s drain interval, retries, attempts cap, stuck-row alerting. |
| `store.gc.two-phase` | **PARTIAL** | The promptness of the swept-path case (chunks enqueued in the same transaction as the path delete) — the promptness the replacement consciously re-prices (design §4.4). |
| `store.put.stale-reclaim` / `store.substitute.stale-reclaim` | **PARTIAL** | The crashed-upload repair cadence (hot-path 5 min); together with the orphan scanner they are the as-built repair term of the bound for C1–C7 leaks. |
| Orphan scanner + orphan-chunk sweep cadences | **GAP (prose only)** — not closed | The 15-minute scanner and the hourly orphan-chunk sweep exist only as unmarked prose (the PutPath "On crash" paragraph and the "Orphan cleanup" paragraph). Their obligations are subsumed by the new bounded-retention rule's "one full pass of the applicable reclamation path"; per-loop cadence rules are deliberately not added (policy numbers stay in code consts). |
| "A weekly full orphan scan remains as a safety net" (prose under Two-Phase GC) | **CONTRADICTION P1** | No such scan exists (see contradiction table). Load-bearing for CR-2: the spec promised a backstop for exactly the leak class (stuck `refcount > 0`) that in reality has no repair path. |

### CR-3 `CounterRefinesManifestFold` (= S2; as-built only, retired with the counter)

*At every quiescent point `refcount(h)` equals the number of existing
manifests referencing `h`, with the sanctioned transient over-counts of
C1–C7 and the sanctioned permanent over-count of C12.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `store.chunk.refcount-meaning` *(new)* | **COVERS** | Was a GAP — the inventory's headline spec gap: nothing stated what the counter means, only how to increment it. The new rule states the equality, the quiescent-point qualification, both sanctioned deviation classes, and that under-counts are never sanctioned (the M_023 direction). Flagged in the spec prose as an as-built rule slated for retirement with the counter (design §3.1, Phase 1c). |
| `store.chunk.refcount-txn` | **PARTIAL** (the increment half) | Same-transaction upsert increment, per-row conflict serialization. Phase 1 amends it (`tracey bump`) to the surviving transactional pairing without the arithmetic (design §4.5); not touched here. |
| `store.chunk.refcount-decrement` *(new)* | **COVERS** (the decrement half) | Was unmarked prose (the inventory's second named gap): no tracey-enforced link existed from "manifest deleted" to "refcounts decremented". The new rule absorbs the prose, adds the by-count clause (the `adfd303d7` C2 lesson), the per-unique-hash clause, and states which decrement paths soft-delete + enqueue in the same transaction (DEC-2/ZERO) versus leave zero-rows to the orphan-chunk sweep (DEC-1). Flagged as as-built, slated for retirement. The C12 skip (corrupt `chunk_list` ⇒ decrement skipped, permanent leak) is recorded in its rationale prose — previously the spec did not mention the corrupt-`chunk_list` behavior at all. |
| `store.chunk.lock-order` | **PARTIAL** (a maintenance precondition, not the equality) | Sorted/single-statement chunk-row locking is what lets the increment/decrement family run concurrently without 40P01 aborts; it says nothing about the counter's value. |
| M_023 `CHECK (refcount >= 0)` | **GAP (schema only)** — recorded, not closed | The only runtime enforcement of (one side of) the equality appears nowhere in the spec. Recorded in the new refcount-meaning rationale prose; no separate rule (the CHECK is dropped by the Release-B migration, design §4.5). |

### CR-4 `PresenceNeverInferredFromCounter` (= S3, the M_033 lesson; survives verbatim)

*A writer skips the backend PUT only on non-NULL `uploaded_at`; `uploaded_at`
is non-NULL only after an observed successful put since the last soft-delete;
the liveness signal is never consulted for presence.*

| Rule | Verdict | Audit finding |
|---|---|---|
| `store.cas.upsert-inserted+2` | **COVERS** (the upsert-path skip decision) | The strongest existing statement: needs-upload is `(uploaded_at IS NULL)`, atomic with the upsert, "keyed on confirmed backend presence rather than refcount", with the SIGKILL-mid-PUT rationale. |
| `store.cas.chunk-upload-committed` | **COVERS** (the `uploaded_at` truth condition) | non-NULL iff an observed successful `ChunkBackend::put`; GC clears it on soft-delete. |
| `store.chunk.liveness-not-presence` *(new)* | **COVERS** (the universal negative) | Was a GAP: the two rules above govern the chunk-upsert path specifically; no rule stated the invariant-level prohibition — the liveness signal (counter today, mark set tomorrow) is never a presence signal, for any writer, probe, or tooling. The new rule states it mechanism-neutrally so it survives the counter's replacement and forbids the regression class outright. The xtask `i201-stranded-chunks` probe currently violates the tooling clause (counter-as-presence in operator tooling) — recorded as CONTRADICTION T1 with its pre-committed fate (consumer audit / design §4.3), not fixed here. |
| `store.admin.verify-chunks` | **COVERS** (the probe-side instance) | The server-side scan keys on `deleted = FALSE AND uploaded_at IS NOT NULL` and explicitly does not filter on the counter ("refcount=0 IS verified"). Consistent with CR-4; its body's refcount mention is a Phase-1c prose touch when the column drops. |
| Chunk Storage dedup bullet, PutPath flow steps 4–5, `grpc/chunk.rs` header comment, `rio-proto` / `rio-cli` comments | **CONTRADICTION P2** (stale prose) | The pre-M_033 `refcount == 1` dedup story and the deleted `FindMissingChunks` RPC presented as current behavior — contradicting `store.cas.upsert-inserted+2` and the code. Spec instances flagged in place this commit (not rewritten); code-comment instances are Phase-1c prose sweep (consumer audit). |

### Supporting invariants (carried by the model; no new rules required)

| Invariant | Rule(s) | Verdict | Audit finding |
|---|---|---|---|
| S4 `OwnerOnlyMutation` | `store.put.placeholder-claim+2`, `store.put.drop-cleanup+2` | **COVERS** / **PARTIAL** | Claim-gating of every owner-side mutation, late-fire no-op semantics, and the foreign-clobber rationale are stated. Missing pieces, recorded not bumped: the DEC-1 `PlaceholderToken` (`updated_at`-epoch) gate exists in no rule, and "a generation's decrement happens at most once" is implied (claim filter + single transaction) rather than stated. Phase 1 deletes the token and the chunk-awareness of the reap paths, so closing this gap now would write spec for machinery the campaign intends to remove. |
| S5 `LiveOwnerNeverReaped` | `store.gc.orphan-heartbeat`, `store.put.stale-reclaim`, `store.substitute.stale-reclaim` | **COVERS** / **PARTIAL** | The heartbeat obligation (≤30 s / ≤64 chunks) and the 5-minute hot-path threshold are normative; the 15-minute scanner threshold and the ordering heartbeat ≪ hot-path ≪ scanner are prose only. The ordering is what the model's scaled-clock witnesses must preserve (design §3.2); no rule added. |
| S6 `LockOrder` | `store.chunk.lock-order`, `store.gc.serialize-lock` | **COVERS** | Sorted-array / single-statement row locking with the one-defensive-retry bound; GC-vs-GC advisory-lock exclusion. Pre-registered NOT-ENCODED for the model (below transaction granularity); the rule surface shrinks but survives the replacement. |
| L3 `PlaceholderConvergence` | `store.put.stale-reclaim`, `store.substitute.stale-reclaim` (+ prose) | **PARTIAL** | The hot-path half is normative; the 15-minute orphan-scanner half is unmarked prose; the chunk-accounting repair obligation ("every C1–C7 leak is repaired by the same event") is unstated and is exactly the as-built repair term the new bounded-retention rule's bound leans on. Deliberately not closed with its own rule: the chunk-accounting content of L3 is retired with the counter (design §3.3). |
| L4 `RepairLoopLiveness` | — | **GAP** — deliberately left open | No rule states loop progress past poison rows / bounded per-iteration memory (the G7 family). Carried by the existing tests and loop structure; the design encodes L4 only if cheap. Recorded so the omission is visible, not closed. |

## Contradiction records

Spec text says something the code does not do (or describes machinery that no
longer exists). Recorded, not fixed; none of these is a code defect — in
every row the code is the side the design treats as correct. P-rows are
prose/stale-description contradictions; T-rows are tooling-vs-new-rule
contradictions created knowingly by this audit's marker-first rules.

| # | Spec site | What the spec says | What the code does | Disposition |
|---|---|---|---|---|
| P1 | store.typ, "Orphan cleanup" paragraph under Two-Phase GC | "A weekly full orphan scan remains as a safety net for any leaked chunks not covered by manifest-based cleanup." | No weekly scan exists. Background chunk reapers are: orphan scanner (15 min, stale `'uploading'` manifests), orphan-chunk sweep (1 h, rows already at `refcount = 0`), drain (30 s). A refcount stuck above zero (C12 skip, G2-class bug) has **no** repair path — the leak the sentence claims is covered is precisely the permanent-leak class the design's replacement exists to dissolve. | Flagged in place this commit. The false safety-net claim must not survive into the Phase-1 spec rewrite; the replacement's collector genuinely provides what this sentence promised (leaked-counter chunks become "unmarked, past grace" — design §4.4). |
| P2 | store.typ Chunk Storage dedup bullet; PutPath flow steps 4–5; Key Files `chunk.rs` entry | `refcount == 1` dedup heuristic, a separate post-upsert dedup query, and a live `FindMissingChunks` RPC, presented as current behavior | Dedup is decided inside the upsert via `RETURNING (uploaded_at IS NULL)` (M_033 / `b1c7a9497`); `FindMissingChunks` was deleted (`c5bb34612`) and its `chunk_tenants` scope dropped (migration 035); `grpc/chunk.rs` serves only the test-only `GetChunk`. | Flagged in place this commit (not rewritten — the rewrite belongs to the refcount-prose retirement pass, design §3.1/§4.5). Contradicts `store.cas.upsert-inserted+2` and the new `store.chunk.liveness-not-presence`. |
| P3 | store.typ chunks-table description (Chunk Lifecycle intro) and schema appendix | The `chunks` schema is listed without `uploaded_at` (and without the M_023 CHECK or `idx_chunks_gc` being tied to their migrations) | `chunks.uploaded_at` (M_033) is the load-bearing presence column; the appendix DDL block does show `idx_chunks_gc` but not `uploaded_at`. | Recorded only (no flag added — the table is not *wrong* about what it lists, it is incomplete). Folded into the Phase-1c chunks-table prose rewrite that the design already commits to. |
| P4 | `store.chunk.refcount-txn` body; `store.chunk.grace-ttl` rationale sentence | "(step 2 of PutPath)"; a grace rationale describing a skip-then-increment race that the atomic upsert can no longer produce | The write-ahead manifest is step 3 of the documented flow; the dedup skip and the increment are one statement since the RETURNING-atomic fix, and grace's surviving jobs are the three the design names (§4.1) | Recorded as prose nits feeding the already-planned Phase-1 amendments of both rules (`tracey bump` there, not here). Normative meaning unchanged today, so no bump in Phase 0. |
| T1 | xtask `i201-stranded-chunks` QA probe vs `store.chunk.liveness-not-presence` *(new)* | (tooling, not spec) — the probe asserts "PG expects an S3 object" from `refcount > 0 AND NOT deleted`, the counter-as-presence inference the new rule forbids for probes and tooling | The serving path never does this (M_033); the probe also has a false-positive window against in-flight uploads (`refcount ≥ 1`, `uploaded_at IS NULL`, PUT not yet done) | Pre-committed fate in design §4.3: re-point the predicate at `uploaded_at IS NOT NULL AND NOT deleted` (matching the server-side VerifyChunks scan), no later than Release B, allowed earlier since the new predicate is column-independent. Recorded here because the rule is added marker-first; the xtask code is deliberately not changed in Phase 0. Consumer-audit row 1. **Resolved — landed ahead of Release B (Phase 1-pre):** the probe's sample query, fail message, and module doc now key on `uploaded_at IS NOT NULL AND NOT deleted`; T1 is closed and the in-flight false-positive window is gone. |
| T2 | xtask `i040-chunk-verify` seed-chunk pick vs the counter's retirement (no rule violated today) | (tooling) — `refcount = 1 AND NOT deleted` as the "unshared chunk" selector, plus refcount-phrased diagnostics | The selector is a liveness/sharing question, not a presence inference, so it does not violate CR-4; it simply stops compiling against the post-drop schema | Pre-committed fate in design §4.3: manifest-reference-based selector + reworded diagnostics, no later than Release B. Consumer-audit row 2. |

No code-vs-rule contradiction was found in the chunk-liveness rule set: every
existing normative MUST in the audited rules is satisfied by the current code
as far as this audit (and the inventory's file:line evidence) can establish.
The contradictions above are all spec-prose-vs-code or tooling-vs-new-rule.

## New rules added by this audit, and what happens to them

| Rule | Invariant | Fate |
|---|---|---|
| `store.chunk.refcount-decrement` | CR-3 (the decrement half of the maintenance protocol) | As-built; retired in Phase 1c with the counter (`tracey bump`/removal in the same commit that deletes the writers) |
| `store.chunk.refcount-meaning` | CR-3 | As-built; retired in Phase 1c with the counter |
| `store.chunk.no-live-collect` | CR-1 | Survives the replacement unchanged; Phase 1 adds `r[impl]`/`r[verify]` via the model checks and the surviving mechanism sites |
| `store.gc.bounded-garbage-retention` | CR-2 | Survives with the bound restated for the collector (a `tracey bump` lands with the Phase-1b cadence change); the carve-out narrows from "permanent, log-only" to "fail-closed, alerted, remediation-bounded" |
| `store.chunk.liveness-not-presence` | CR-4 | Survives verbatim; the design's replacement rules (`store.chunk.liveness-derived`, `store.gc.chunk-collect`, drafted in Phase 0, landed with the code in Phase 1) will reference it rather than restate it |

Planned Phase-1 amendments to existing rules, deliberately NOT performed in
Phase 0 (no normative meaning changes today, so no bump + re-point now):
`store.chunk.refcount-txn` (surviving transactional pairing without the
arithmetic), `store.chunk.grace-ttl` (new predicate + the three grace jobs),
`store.chunk.lock-order` (site list narrowed), `store.gc.two-phase` (path
sweep stops touching chunks), `store.gc.pending-deletes` (producer becomes
the collector; drain re-check loses the `refcount = 0` conjunct),
`store.gc.dry-run+2` and `store.admin.verify-chunks` and
`store.atomic.multi-output` (incidental refcount mentions in their bodies),
plus the prose corrections of P1–P4.

## Rules in the chunk/GC neighborhood not load-bearing for these invariants

- `store.gc.two-phase` (mark half), `store.gc.sweep-recheck+2`,
  `store.gc.sweep-referrer-order`, `store.gc.sweep-cycle-reclaim`,
  `store.gc.sweep-path-tenants`, `store.gc.tenant-retention`,
  `store.gc.tenant-quota`, `store.gc.tenant-quota-enforce`,
  `store.gc.dry-run+2`, `store.gc.shutdown-abort` — the path-level
  reachability GC. Upstream of chunk liveness (it decides which manifests
  exist), untouched by the campaign (design §8), pre-registered NOT-ENCODED
  as calibration family G4b. The chunk-level audit leans on it only through
  "manifest exists/was deleted".
- `store.put.wal-manifest`, `store.put.idempotent`,
  `store.put.placeholder-refs`, `store.put.nar-bytes-budget+3`,
  `store.atomic.multi-output` — write-path rules whose chunk-relevant content
  is either the placeholder lifecycle (covered under S4/S5/L3 above) or an
  incidental refcount mention (`store.atomic.multi-output`'s orphaned-blob
  note, consistent with ZERO-2 + grace).
- `store.manifest.format` — the `chunk_list` encoding both the counter
  maintenance and any future mark fold parse; its parser is the C12 surface
  and the planned Kani target, but the rule itself carries no liveness
  obligation.
- `store.gc.serialize-lock`, `store.db.migrate-try-lock` — advisory-lock
  hygiene (S6's GC-vs-GC half).
- `store.admin.verify-chunks`, `store.admin.service-gate` — operator probes;
  CR-4-aligned as noted above.
- The LogService rules (`store.log.*`) — a different chunk concept entirely
  (position-keyed, no refcount, TTL-swept); cited by the design only as the
  in-crate existence proof of durable-manifest + lazy-delete.

## Verify-marker status

The five new rules carry no `r[impl]` markers yet, by design: Phase 0 is
spec-audit-and-model only (no Rust is touched), and the implementing-code
annotations arrive with the Phase-1 work. Their first `r[verify]` markers
landed with Stage B: the `quint-chunk-liveness-*` regime checks in
`nix/quint.nix` carry markers for `store.chunk.no-live-collect`,
`store.gc.bounded-garbage-retention`, `store.chunk.refcount-meaning`,
`store.chunk.refcount-decrement`, and `store.chunk.liveness-not-presence`
at the wiring points (the house rule: a marker at the wiring point
structurally proves the check is built), alongside model-checked markers
for the pre-existing mechanism rules those regimes exercise
(`store.chunk.refcount-txn`, `store.chunk.grace-ttl`,
`store.cas.upsert-inserted+2`, `store.cas.chunk-upload-committed`,
`store.gc.pending-deletes`, `store.put.placeholder-claim+2`,
`store.gc.orphan-heartbeat`). The five new rules still appear in
`tracey query uncovered` (no `impl` sites) — expected, and preferable to
annotating existing code against rules whose enforcing mechanism the
campaign is about to replace. The existing unit-test markers from
inventory Appendix B are unaffected: no existing rule text was changed, so
nothing went stale. `store.chunk.lock-order` deliberately gains no model
marker (pre-registered NOT-ENCODED — below transaction granularity).

## Consumer audit (design §5a): every reader/writer of `chunks.refcount` outside `rio-store/src`

Stage-A companion deliverable. Method: repo-wide greps over the worktree at
this branch point for `refcount` (case-insensitive), `chunks.refcount`, SQL
against the `chunks` table (`FROM chunks` / `UPDATE chunks` / `INSERT INTO
chunks` / `JOIN chunks`), `idx_chunks_gc`, `chunks_refcount_nonneg`, and
`chunk_tenants`, across every crate, `xtask`, `nix/`, `infra/`, `fuzz/`,
`.sqlx/`, `docs/`, and the repo-root files. No code is changed by this audit.

Two-class disposition per design §5a: **(a)** a production decision-path
consumer of the counter outside the inventory §1.2 reader list — anything
gating behavior other than GC deletion-eligibility on the counter — is a
go/no-go finding that redraws the subject; **(b)** tooling, test seeds, or
prose that query or describe the column are not no-gos; each gets a fate
entry with a named deadline (no later than Release B for anything that
issues SQL against the column).

**Class (a) verdict: none found.** The only SQL that reads `chunks.refcount`
outside `rio-store/src` is in the two xtask k8s QA probes (operator tooling,
rows 1–2). Inside `rio-store/src` the production readers are exactly the
inventory §1.2 decision-site list reproduced in this map's site table (ZERO's
predicate, ZERO-2's candidate SELECT and UPDATE re-check, the drain re-check,
and the `idx_chunks_gc` partial index); the serving-path dedup decision reads
`uploaded_at`, and the admin `VerifyChunks` scan filters on
`deleted = FALSE AND uploaded_at IS NOT NULL`, not the counter. The design's
blast-radius claim — the counter gates GC deletion-eligibility only — holds.

### Class (b) rows

| # | Site | What it does with the counter | Fate / deadline |
|---|---|---|---|
| 1 | `xtask/src/k8s/qa/scenarios/i201_stranded_chunks.rs` (module doc; the sample query at the top of `run`; the fail message) | SQL reader: `SELECT … FROM chunks WHERE refcount > 0 AND NOT deleted ORDER BY created_at DESC LIMIT 1000`, then per-hash S3 HeadObject; "PG refcount>0 but S3 404" is the failure verdict. Counter-as-presence inference in tooling — contradiction T1 against the new `store.chunk.liveness-not-presence`; also false-positive-prone against in-flight uploads (`refcount ≥ 1`, `uploaded_at IS NULL`). | Design §4.3: predicate moves to `uploaded_at IS NOT NULL AND NOT deleted` (matching the server-side VerifyChunks scan and CR-4); fail-message text updated. **No later than Release B**; may land earlier — the new predicate is column-independent and removes the in-flight false-positive window. **Landed ahead of Release B (Phase 1-pre):** query, fail message, and module doc re-pointed; the probe no longer issues SQL against the counter (T1 row above records the closure). |
| 2 | `xtask/src/k8s/qa/scenarios/i040_chunk_verify.rs` (seed-chunk pick CTE; `diagnose_missing_chunk`; surrounding doc comments) | SQL reader: seed-chunk pick joins the seed manifest's parsed `chunk_list` to `chunks` and filters `c.refcount = 1 AND NOT c.deleted` ("unshared" selector); diagnostics and comments are refcount-phrased. (Its cleanup `DELETE FROM chunks WHERE blake3_hash = …` does not read the counter.) Not a presence inference — but the query errors once the column drops. | Design §4.3: selector re-pointed at a manifest-reference-based predicate (the hash appears in exactly one existing manifest's `chunk_list`); diagnostics reworded. **No later than Release B.** |
| 3 | `rio-cli/src/verify_chunks.rs` (module doc) | Comment only: describes the VerifyChunks audit as "PG says exists (refcount>0, deleted=false)". Doubly stale — no SQL issued, and the server-side scan has keyed on `uploaded_at` since the VerifyChunks predicate changed. | Phase-1c prose sweep (design §4.3 comment-only row). |
| 4 | `rio-proto/src/lib.rs` (store-module doc) | Comment only: "dedupes chunks via the `chunks` table refcount" — pre-M_033 phrasing. | Phase-1c prose sweep. |
| 5 | `rio-store/src/grpc/chunk.rs` (module doc; in-crate but prose-only) | Comment only: "dedup via the `refcount==1` RETURNING clause" — pre-M_033 phrasing for a module that no longer hosts the deleted RPC. | Phase-1c prose sweep (named in design §4.3). |
| 6 | `rio-store/tests/grpc/chunked.rs` (3 assertions) plus the in-module `#[cfg(test)]` suites of `metadata/chunked.rs`, `gc/{mod,sweep,orphan,drain}.rs`, `metadata/mod.rs`, `test_helpers.rs` | Test fixtures and assertions on counter values (the ~86-test corpus the inventory sizes). Outside `rio-store/src` only in the `tests/` tree; listed for completeness. | Phase-1c test disposition: each re-pointed at the collector's observable effects or retired with the falsifying-run-or-redundancy justification (design §4.6). No Release-A/B code change required. |
| 7 | `rio-migrations/migrations/` 002 (table, `idx_chunks_gc`, comments), 005/006 (comments), 023 (`chunks_refcount_nonneg` CHECK), 018 (`chunk_tenants`), 035 (drops `chunk_tenants`) | Shipped schema history; the live schema still carries the column, CHECK, and index. | Frozen — never edited (house rule). The CHECK + index drop is Release B's new migration; the column drop is the post-rollout follow-up migration; commentary lands in `migrations.rs` (design §4.5). |
| 8 | `rio-migrations/src/migrations.rs` (M_018, M_023, M_033, M_035, M_052 … doc-consts) | Historical commentary explaining why those migrations exist; mentions the counter and `chunk_tenants` throughout. | Stays as history; new M_0NN entries are added for the Release-B and follow-up migrations. No sweep. |
| 9 | `docs/gen/errors.json`, `docs/gen/metrics.json`, `docs/gen/modules.json` | Generated artifacts (`cargo xtask regen docs-data`) carrying refcount wording from code doc comments and metric help strings (orphan-chunk metrics, the i201 module doc). | Regenerate whenever the source comments change (Phase 1c); never hand-edited; `docs-data-fresh` enforces. |
| 10 | `docs/spec/components/store.typ` | The audited spec itself: the counter rules, the new invariant rules, and the flagged stale prose (P1/P2/P3). | Handled by this campaign's spec passes (this audit now; the counter-prose rewrite in the Release-B spec commit). |
| 11 | `docs/spec/components/lazy-store.typ` (write-path sketch; EROFS bootstrap note) | Describes the current chunked write path ("upsert chunk refcounts (PG)") and contrasts the EROFS bootstrap blob lifecycle ("1:1, no refcount"). Accurate as-built. | Touched by the Phase-1c prose pass only if the write-path description it quotes changes; no SQL, no deadline. |
| 12 | Checked, zero references | `.sqlx/` query cache (no prepared statement names the column — the eventual drop creates no sqlx-prepare drift), `infra/` (helm + grafana dashboards consume Prometheus metrics only), `nix/tests/` VM scenarios (no SQL against `chunks`), `rio-dashboard`, `rio-gateway`, `rio-builder`, `rio-controller`, `rio-scheduler`, `rio-common`, `rio-auth`, `rio-test-support`, `fuzz/`, shell/python scripts, README, process-compose. | Nothing to do; recorded so the negative result is reproducible. |
| 13 | Same word, different concept (excluded from the audit) | `nix/tests/fixtures/k3s-full.nix` and `nix/tests/scenarios/lifecycle/gc-sweep.nix` comments (kubernetes SecretManager reflector refcount), `rio-builder` FUSE `nlookup` refcounting, `rio-scheduler/src/state/newtypes.rs` Arc refcount comments. | Not consumers of `chunks.refcount`; listed so the grep tally reconciles. |

### `chunk_tenants` confirmation (dead table)

Confirmed dropped: `rio-migrations/migrations/035_drop_dead_rpc_tables.sql`
contains `DROP TABLE IF EXISTS chunk_tenants;` (alongside `content_index` and
`narinfo.refs_backfilled`), and the M_035 doc-const in
`rio-migrations/src/migrations.rs` records the rationale (the backing RPCs —
PutChunk/FindMissingChunks — were never wired to a production caller). The
table the design's earlier draft proposed dropping is therefore already gone;
nothing in this campaign claims that deletion.

Remaining references are stale prose only — none issues SQL, so none carries
a Release-B deadline; all are routine cleanup, not campaign deliverables:

- `rio-scheduler/src/db/tenants.rs` — `delete_tenant`'s doc comment lists
  `chunk_tenants` among the FK-CASCADE targets and cites migration 018.
- `docs/spec/components/scheduler.typ` — `sched.admin.delete-tenant`'s body
  lists `chunk_tenants` in the same CASCADE enumeration. (Inside a rule body;
  removing a dead table name does not change the rule's normative meaning, so
  the eventual cleanup is not expected to need a `tracey bump` — note left
  here so whoever sweeps it makes that call consciously.)
- `docs/spec/system/tenancy.typ` — the tenant-isolation table row, the
  "FindMissingChunks Scoping" current-state box (still presents the junction
  and the deleted RPC as the shipped, mandatory scoping mechanism), and the
  Implementation Status row. No `#r()` rules exist in that file, so these are
  prose-only.
- `docs/spec/system/security.typ` — two threat-model entries naming
  `FindMissingChunks` as a live cross-tenant probing surface.
- Historical, stays: migration 018 itself (frozen) and the M_018 commentary.

### Tally

13 class-(b) rows above (2 SQL readers, 3 comment-only code mentions, 1 test
corpus row, 2 schema/commentary rows, 1 generated-docs row, 2 spec-doc rows,
1 checked-clean row, 1 same-word-exclusion row), plus the `chunk_tenants`
stale-reference list (5 sites). 0 class-(a) findings: the §5a no-go trigger
"a production decision-path consumer outside the §1.2 reader list" is not
met, and the audit found nothing that gates anything other than GC
deletion-eligibility on the counter.

## Stage-B results (`chunkLiveness.qnt`, the as-built model)

The model is `docs/spec/models/chunkLiveness.qnt`: the write-ahead uploader
state machine (claim, the upgrade transaction's manifest_data INSERT +
refcount UPSERT + token capture, the S3 PUT fan-out, the presence commit,
the claim-gated completion), the token-gated rollback (DEC-1), the
claim-gated reap, the heartbeat, the hot-path and scanner stale reclaims,
the path-sweep batch (single-path and by-count two-path forms), the
orphan-chunk sweep split into its outer SELECT and inner UPDATE so the C11
window is a real interleaving, the outbox drain with its `FOR UPDATE`
re-check, the crash windows of inventory §2.2 as the fault alphabet, and
`refs(h)` (the manifest fold) as the recomputed ghost truth. One model
action per SQL transaction (design §3.2); scope boundaries and encoding
decisions (the path-level reachability GC abstracted to an environment
choice per the G4b pre-registration; lock order NOT-ENCODED per G6; the S3
PUT fan-out collapsed to one non-transactional action; the outbox attempts
counter and the parked-row alerting tail out of scope; the relative clock
with saturation and the heartbeat contract as a `tick` precondition) are
documented in the model header. Four exhaustive TLC regimes are wired into
`nix/quint.nix` (`quint-chunk-liveness-{base,crash,contend,corrupt}`),
plus the named-run replays, sixteen non-vacuity witness checks, the two
pre-registered corrupt-regime falsification checks, and the
threshold-ordering inversion check.

### Verdict table

Distinct-state counts are as measured at the introducing commit (also in
that commit's message and the CI transcripts): base 7,791 distinct
(735,841 generated, depth 14), crash 2,964,717 (145,054,657, depth 25),
contend 1,332,821 (53,843,425, depth 23), corrupt 3,307,725 (142,073,313,
depth 26). Every regime's HOLDS column is an exhaustive TLC result over
that regime's full reachable space.

| Design invariant (§3.3) | Model form | base | crash | contend | corrupt |
|---|---|---|---|---|---|
| CR-1 `NoLiveChunkCollected` (state + action form) | `cr1NoLiveChunkCollected` | HOLDS | HOLDS | HOLDS | HOLDS |
| CR-2 `BoundedGarbageRetention` (as-built structural form) | `cr2NoStrandedGarbage` | HOLDS | HOLDS | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — the C12 stranded-garbage shape (`quint-chunk-liveness-corrupt-c12-stranded`); the carved form below is this regime's stated form |
| CR-2, corrupt-regime carved form (the rule's carve-out clause) | `cr2CarvedCorrupt` | — (base form checked) | — | — | HOLDS |
| CR-3 `CounterRefinesManifestFold` | `cr3CounterRefinesFold` | HOLDS | HOLDS | HOLDS | **FALSIFIES-AS-PRE-REGISTERED** — the C12 sanctioned permanent over-count (`quint-chunk-liveness-corrupt-c12-overcount`) |
| CR-3, corrupt-regime carved form (counter = fold + observably-skipped decrements) | `cr3CarvedCorrupt` | — | — | — | HOLDS |
| CR-4 `PresenceNeverInferredFromCounter` | `cr4PresenceFromConfirmedUpload` | HOLDS | HOLDS | HOLDS | HOLDS |
| S4 `OwnerOnlyMutation` | `s4OwnerOnlyMutation` (admission-predicate form) | HOLDS | HOLDS | HOLDS | HOLDS |
| S5 `LiveOwnerNeverReaped` | `s5LiveOwnerNeverReaped` | HOLDS | HOLDS | HOLDS | HOLDS |
| L3 `PlaceholderConvergence` (safety support: no foreign freshen) | `l3NoForeignFreshen` | HOLDS | HOLDS | HOLDS | HOLDS |
| M_023 `CHECK (refcount >= 0)` | `m023NonNegative` | HOLDS | HOLDS | HOLDS | HOLDS |
| structural bounds / self-consistency | `boundsOK` | HOLDS | HOLDS | HOLDS | HOLDS |

Forms and qualifications, relative to design §3.3's statements:

- **CR-3 needed no C1–C7 carve-out.** The design sanctioned "transient
  over-counts after C1–C7 crashes, repaired within the scanner threshold";
  at transaction granularity those windows leave the `'uploading'`
  manifest row (and its `manifest_data`) in place, so the fold still
  counts the abandoned references and the counter never diverges — the
  crash regime HOLDS on the exact equality at every reachable state, a
  stronger verdict than the gate required. The only sanctioned deviation
  that exists as built is C12's permanent over-count, carved exactly by
  `cr3CarvedCorrupt` (counter = fold + per-chunk skipped-decrement count).
- **CR-2 is encoded structurally, not clocked.** The rule's bound mixes
  protocol obligations with background-loop cadences (15-minute scanner,
  hourly orphan-chunk sweep, 30 s drain); the cadences are scheduling
  facts about `spawn_periodic` loops, not protocol state, so the model
  checks the protocol half: garbage never becomes *unreclaimable by the
  standing machinery* — an unreferenced, not-yet-soft-deleted chunk is
  always at refcount 0 (still eligible for the zero-detect / orphan-chunk
  sweep predicates), and a soft-deleted chunk whose object still exists
  is always enqueued. As built that is exactly the conditionality on the
  counter the Stage-A row recorded: a refcount stuck above zero with no
  references is permanent garbage, which is why the unconditional form
  falsifies in the corrupt regime and only there. The wall-clock tail of
  the bound (cadence + drain lag + the attempts-cap parking) stays with
  the existing loop tests under `store.gc.pending-deletes` and the
  stale-reclaim rules.
- **CR-1's action form** is carried by the `deletedWhileReferenced` ghost
  recorded at the only two backend-delete sites (the drain transaction
  and its C10 crash variant); the state form is the `'complete'`-manifest
  presence clause. Both hold in all four regimes; the corrupt regime's
  HOLDS is the design's "C12 errs toward retention, never toward
  data loss" claim, machine-checked.
- **S5 holds against the production threshold ordering, and the ordering
  is load-bearing**: the `chunkLivenessThresholdOrder` regime lowers the
  hot-path threshold to the heartbeat deadline and S5 falsifies there
  (`quint-chunk-liveness-threshold-order` passes only while it does).
  S4/L3 are encoded over the admission predicates the actions themselves
  use (the logService authGate pattern), so dropping a claim/token/status
  conjunct from a cleanup or heartbeat guard falsifies them.
- **L4 `RepairLoopLiveness` is not encoded** (per the design's "encode
  only if cheap"): pagination, poison-row isolation and per-row
  transaction shapes are below this model's granularity; the G7 family
  stays with the existing loop tests. L3's liveness half ("every
  abandoned row is eventually reaped") is carried as the no-foreign-
  freshen safety support plus the scanner/hot-path reachability
  witnesses; the eventuality itself is a fairness property outside an
  interleaving safety model.

### Witness results

All sixteen non-vacuity witnesses are violated (the contended states are
reachable) in the regime each is wired against, and the three
expected-falsification probes reproduce; every row below is a CI check.

| Witness (model `val`) | Regime | Probes | Result |
|---|---|---|---|
| `noCompleteUpload` | base | a complete chunked upload exists (CR-1's state form is non-vacuous) | violated |
| `noBackendDelete` | base | a backend DeleteObject actually fires (CR-1's action form is non-vacuous) | violated |
| `noUnconfirmedReferencedChunk` | base | the M_033 precondition (refcount ≥ 1, `uploaded_at` NULL) | violated |
| `noStaleTokenRollbackNoop` | base | the C4 own-heartbeat token no-op | violated |
| `noHeartbeatReset` | base | a heartbeat resets non-zero staleness (the heartbeat is load-bearing) | violated |
| `noCrashAtClaimed` | crash | C1 | violated |
| `noCrashAfterUpgrade` | crash | C2 | violated |
| `noCrashBeforeReap` | crash | C5 (the state C3/C7 collapse onto) | violated |
| `noDoubleCrashStaged` | crash | C6 (both writers staged, both dead) | violated |
| `noAbandonedAccounting` | crash | the as-built leak shape: abandoned `'uploading'` manifest, chunks still counted | violated |
| `noHotpathReclaim` | crash | the 300 s hot-path reclaim fires | violated |
| `noScannerReap` | crash | the 15-minute scanner reaps an abandoned row | violated |
| `noSharedByCountDecrement` | contend | one batch decrements a shared chunk by ≥ 2 (the adfd303d7-C2 clause) | violated |
| `noDrainResurrectSkip` | contend | the drain re-check skips a resurrected chunk (G4a) | violated |
| `noOrphanRecheckSave` | contend | the orphan-sweep inner re-check excludes a resurrected candidate (C11) | violated |
| `noLateCleanupNoop` | contend | an owner-side cleanup no-ops against a foreign/missing row (the G1 contention) | violated |
| `cr3CounterRefinesFold` (pre-registered falsification) | corrupt | the C12 permanent over-count | violated, as pre-registered |
| `cr2NoStrandedGarbage` (pre-registered falsification) | corrupt | the C12 stranded garbage | violated, as pre-registered |
| `noCorruptLeak` | corrupt | the literal leak shape (refcount > 0, zero referencing manifests) | violated |
| `s5LiveOwnerNeverReaped` (threshold-ordering inversion) | threshold-order | a live, progressing owner reaped once heartbeat-deadline ≥ hot-path threshold | violated |

One §3.5 witness was corrected against the code rather than encoded as
written: the design asks for "refcount > 0 with zero manifests" to be
reachable in the **crash** regime before the reaper runs. As built that
state is unreachable outside the corrupt regime — both decrement
statements run in the same transaction as the manifest deletion that
justifies them, and the C1–C7 windows leave the manifest row in place, so
crashes leave *accounted* garbage (`noAbandonedAccounting`), not a
counter/fold divergence. The literal leak shape is reachable exactly where
a decrement is skipped against a surviving reference record's deletion —
the corrupt regime — and is pinned there by `noCorruptLeak`. This is a
design-§3.5 wording correction, not a model gap; the same observation is
why CR-3 needs no crash-regime carve-out.

### Notes for the Phase-0 exit gate (what the model run established)

- The full encoded invariant set HOLDS on the unmodified as-built model in
  every regime, in each regime's stated form (the corrupt regime's stated
  forms being the carved CR-2/CR-3), with the two unconditional-form
  falsifications in the corrupt regime exactly matching this map's
  pre-registered as-built deviations (CR-2 conditional on CR-3; C12's
  sanctioned permanent over-count). No unexpected falsification occurred.
- Encoding observations worth carrying into Stage C and Phase 1:
  `upgrade_manifest_to_chunked`'s ownership guard is the `FOR UPDATE` on
  the `'uploading'` row plus its existence — there is no claim_id filter
  in that statement; the model encodes it faithfully and no invariant
  falsifies, because a reaped-then-re-claimed path can only reach that
  state after the original owner stopped heartbeating, and a stopped
  owner never reaches its upgrade step. Calibration should keep this in
  mind when reverting heartbeat/claim mechanisms (a G1/G5-family revert
  may surface it).
- The Stage-C calibration corpus (design §3.4) is the next stage and is
  NOT part of this section's claims; its overrides will import this model
  the way `calibration/retry-g*.qnt` import `retryPolicyAsBuilt.qnt`.

## Stage-C calibration: the historical-fix corpus replayed against the model

The ~35-fix corpus (inventory §5, families G1–G7, plus the design's
pre-registered G2×G3 joint-revert row) replayed against
`chunkLiveness.qnt`: for each corpus commit the pre-fix behavior is either
expressed as an override of the as-built model and shown to falsify an
invariant (the model would re-find that bug), or its non-encodability is
dispositioned with the missing dimension named. Method per the retry
campaign's Stage C (and the design's §3.4 model-side-override correction):
each override is a module in `docs/spec/models/calibration/refcount-g*.qnt`
that instantiates the as-built model, replaces ONE owner-side entry point
with a local PRE-FIX variant, and exposes the swapped transition relation
as `calibStep` selected with `quint verify --step=calibStep`. The reference
fold `refs(h)`, the ghost sensors, and every invariant keep their as-built
definitions — they are the oracle, not part of the reverted behavior.
Where a module restricts the alphabet below a Stage-B regime's constants,
the distinguishing baseline (the as-built actions over the same
restriction — the module's imported default `step`, or an explicit
`baselineStep` where the restriction itself changes an action) was run
against the same invariant and is recorded as HOLDS; modules that reuse a
Stage-B regime's constants verbatim cite that regime's exhaustive Stage-B
verdict as their baseline. No main-model file was touched by this stage:
no invariant was added to `chunkLiveness.qnt`, the four regime checks are
bit-identical to Stage B, and the only new invariant anywhere is one
module-local ownership restatement (`completionRequiresCurrentOwner`,
below), per the retry campaign's local-invariant precedent.

Verdicts are exhaustive TLC results (violation runs stop at the first
counterexample); depth = transitions in the counterexample, states =
generated/distinct at the point TLC stopped, both from single-worker runs
so the counterexamples are the deterministic shallowest ones; wall-clocks
live in the introducing commit's message. Re-run command shape (the local
apalache-server prelude of `nix/quint.nix` applies):

```
quint verify --backend=tlc --main=<module> --step=calibStep \
  --invariant=<invariant> docs/spec/models/calibration/refcount-g<N>.qnt
```

The S4/L3 encoding caveat, recorded once: the main model's
`s4OwnerOnlyMutation` / `l3NoForeignFreshen` are admission-predicate
regression guards (they quantify over the as-built admission predicates
themselves — the logService authGate pattern), so they bind edits to the
main model's predicates and structurally cannot falsify from an additive
override module. The ownership content of a G1 revert therefore falsifies
here through its state-level consequences (CR-3 / M_023 / CR-4 / CR-1) or
through a module-local restatement over the pre-fix admission predicate —
which is where the design's §3.4 prediction ("S4, then CR-3") lands at
this model's resolution.

### Calibration table

Classification legend: **ENC** — encodable, override written and run;
**ENC-A** — encodable, covered by the named sibling override (disposition
by analogy within the family, design §3.4); **NOT-ENC** — the model
abstracts the mechanism away (the missing dimension is named); **SUBS** —
the fix's subject no longer exists in the tree. Verdict format:
invariant @ step (depth, generated/distinct).

#### G1 — a late or foreign cleanup clobbered someone else's upload (ownership/identity)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g1.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `1cd975b90` | DEC-1 rollback carries no PlaceholderToken / generation gate and takes no FOR UPDATE ownership lock — it deletes whatever 'uploading' row the path has and decrements the roller-back's own hash set against it | ENC | `refcountCalibG1RollbackPreToken` | S4-content via consequences: CR-3, then M_023 | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 8, 208,974/12,056) and m023NonNegative @ calibStep (depth 8, 208,974/12,056) — the late rollback erases the successor's placeholder and re-decrements an already-reclaimed reference below zero; the incident shape is pinned by `g1PreTokenDoubleDecrementRun`. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `937a9c928` (completion half) | completion not claim-gated — a stale uploader that resumes after its placeholder was reaped and re-claimed flips the successor's in-flight placeholder to 'complete'; reaching that state also needs the contract-free clock of the progress-driven-heartbeat era (a stalled owner missed heartbeats while alive) | ENC | `refcountCalibG1CompletionUnclaimGated` | S4-content via the local ownership form, then CR-1 | **FALSIFIES** local `completionRequiresCurrentOwner` @ calibStep (depth 9, 53,278/5,798) and cr1NoLiveChunkCollected @ calibStep (depth 10, 142,842/15,174 — the foreign flip makes the successor's still-uploading manifest readable). Baselines over the same relaxed-clock alphabet: the local invariant **HOLDS exhaustively** (38,684,545/1,608,957) — the falsification is attributable to the claim gate; CR-1 does NOT hold on that baseline (depth 13, 2,458,154/187,657) via the independent late-mark window recorded under Findings, so the CR-1 run is supporting evidence, not the attribution |
| `937a9c928` (heartbeat half) | heartbeat not claim-gated — a stale uploader keeps a foreign placeholder artificially fresh | NOT-ENC | — | — | the harm is an eventuality (the foreign freshen delays reaping; nothing is corrupted), outside this safety model; the ownership content is the same claim-gate discipline the completion half falsifies, and `l3NoForeignFreshen` guards the main model's heartbeat admission predicate. Coverage stays with the claim-gated heartbeat unit tests (store.put.placeholder-claim+2) |
| `bf7e516e4` C1 | the owner-side reap (drop-guard / abort / complete-failure cleanup) matches on the path alone, not the claim — a late drop-guard reaps the successor's in-flight manifest and chunk accounting | ENC | `refcountCalibG1ReapPathMatched` | S4-content via consequences: CR-4 | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 11, 17,722/6,084) — the foreign reap soft-deletes and enqueues the successor's chunk; the successor commits presence on the soft-deleted row and the drain removes the just-uploaded object. Baseline (as-built step over the same one-path/one-hash restriction): HOLDS (185,649/35,161) |
| `ae5f3190b` | hash/size length validation on the rollback path | NOT-ENC | — | — | input validation; pre-registered per-commit exception (design §3.4); existing unit tests |
| `31bd9c512` | orphan scanner re-checks staleness inside the reap transaction | ENC-A | covered by `refcountCalibG1ReapPathMatched` (reap acting on a stale view of the row) | — | by analogy (sibling falsified); the literal pre-fix mechanism (the re-check moved inside the transaction) is an intra-transaction read/write split below the one-action-per-SQL-transaction granularity |
| `539c2be7c` | reap re-checks status inside the transaction (reap-then-reupload race) | ENC-A | covered by `refcountCalibG1ReapPathMatched` | — | by analogy (same shape: a reap admitted against a row that changed under it) |
| `31ce52b14` | reap re-reads chunk_list inside the transaction (stale-chunk-list double decrement) | ENC-A | covered by `refcountCalibG1RollbackPreToken` (a decrement justified by a stale view, double-charging a generation) | — | by analogy (sibling falsified) |

#### G2 — a cleanup path forgot the chunks (leaked refcounts)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g2.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `e5bdbff1b` (I-040) | the owner-side reap uses the inline-only delete: manifest rows deleted, chunk accounting never touched | ENC | `refcountCalibG2ReapInlineOnly` | CR-3, then CR-2 (the leak) | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 4, 25/20) and cr2NoStrandedGarbage @ calibStep (depth 4, 25/20). Baseline (as-built step over the same one-uploader restriction): HOLDS both (1,072/398) |
| `dbb42232a` | abort_upload and the batch drop path still inline-only | ENC-A | covered by `refcountCalibG2ReapInlineOnly` — the model's writers funnel every owner-side cleanup through the same reap action, so this is the same revert at model resolution | CR-3 | by analogy (sibling falsified) |
| `adfd303d7` C2 | the path-sweep batch decrements a chunk shared by N dying manifests once, not N times | ENC | `refcountCalibG2SweepCollapsedCount` | CR-3, then CR-2 | **FALSIFIES** cr3CounterRefinesFold @ calibStep (depth 11, 3,348,835/146,119) and cr2NoStrandedGarbage @ calibStep (depth 11, 3,348,835/146,119) — the by-count clause of store.chunk.refcount-decrement, exercised end-to-end. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `d617bf3e5` | the M_023 `CHECK (refcount >= 0)` plus wiring the standalone orphan-chunk sweep | split | — | — | CHECK half: a passive schema constraint whose model image (`m023NonNegative`) is an invariant in every regime, not a mechanism an override can revert; the under-count class it detects is demonstrated by the `1cd975b90` override driving a counter to −1. Sweep-wiring half: **NOT-ENC** — the existence/cadence of a background collection loop is below the structural CR-2 encoding (the same pre-registered treatment as the 15-minute/hourly cadences); coverage stays with the sweep unit tests and the wired orphan-sweep witnesses |
| `8d93ce6c1` | chunk_tenants junction cleanup | SUBS | — | — | the table was dropped by migration 035; the subject no longer exists |

#### G3 — the counter was used as an S3-presence signal (data loss)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g3.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `dd5c11376` (M_033) | the needs-upload verdict is keyed on the liveness record (row already exists ⇒ "someone else uploaded") instead of `uploaded_at` | ENC | `refcountCalibG3CounterAsPresence` | CR-4, then CR-1 (the production data-loss trace) | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 4, 1,602/145) and cr1NoLiveChunkCollected @ calibStep (depth 7, 115,050/7,014) — two concurrent writers of the same content; the loser skips the PUT nobody confirmed and completes. Baseline: crash-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `b1c7a9497` | the dedup verdict read in a separate statement after the upsert (re-query race) | ENC-A | covered by `refcountCalibG3CounterAsPresence` — the re-query race only loses data under the counter-as-presence semantics; both shapes produce the same harm state (a PUT skipped for an unconfirmed chunk) | CR-4 | by analogy (sibling falsified); the atomicity content is what the as-built `upgradeManifest` encodes and `store.cas.upsert-inserted+2` pins |
| `127168477` | FastCDC duplicate hashes in one UNNEST batch crash the upsert | NOT-ENC | — | — | set-collapsed `chunk_list` and SQL-error granularity; pre-registered per-commit exception (design §3.4); covered by the upsert dedup unit test and the `manifest_deserialize` fuzz target |
| `00fd5b12d` | the PutChunk RPC did not set `uploaded_at` | SUBS | — | — | the RPC was deleted (`c5bb34612`); the subject no longer exists |
| G2×G3 joint revert (design §3.4 pre-registered row) | inline-only reap leaves a stale refcount behind a deleted manifest; counter-as-presence dedup then trusts it and skips the needed re-upload (the I-040 stale-skip trace) | ENC | `refcountCalibG3JointStaleSkip` | CR-4, then CR-1 | **FALSIFIES** cr4PresenceFromConfirmedUpload @ calibStep (depth 4, 1,262/121) and cr1NoLiveChunkCollected @ calibStep (depth 7, 76,630/4,969); BFS reports the two-concurrent-writers variant as the shallowest counterexample, and the documented I-040 reap-then-stale-skip shape is pinned deterministically by the module's `g3JointStaleSkipRun`. Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |

#### G4 — collect raced a concurrent re-reference (G4a chunk-level / G4b path-level)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g4a.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `aa738a5d7` (M_006) | the drain deletes the backend object with no same-transaction re-check of the chunk's current state (the resurrect arm is left as built — the minimal delta) | ENC (G4a) | `refcountCalibG4aDrainNoRecheck` | CR-1 in the contend regime | **FALSIFIES** cr1NoLiveChunkCollected @ calibStep (depth 7, 63,334/4,030) — a re-upload resurrects the enqueued chunk and the pre-fix drain deletes its object while referenced (the action-form ghost). Baseline: contend-regime constants verbatim (Stage-B exhaustive HOLDS) |
| `a2d4c6cd8` (drain re-check half) | the drain re-check ran without FOR UPDATE — its verdict could be stale at DeleteObject time | ENC-A | covered by `refcountCalibG4aDrainNoRecheck`: at one-action-per-SQL-transaction granularity the lockless re-check's staleness window collapses onto "the delete does not observe the concurrent resurrect" | CR-1 | by analogy (sibling falsified); the missing dimension for a literal encoding is an intra-transaction read/write split |
| `a2d4c6cd8` (path_tenants + cycle-reclaim halves) | sweep deletes path_tenants; cycle reclaim via temp-table anti-join | NOT-ENC (G4b) | — | — | path-level reachability GC, pre-registered NOT-ENCODED; covered by `store.gc.sweep-path-tenants`, `store.gc.sweep-cycle-reclaim` and the sweep tests |
| `2b68855c5`, `261e78c9d`, `7d5ff71dc` | the mark-vs-PutPath story (advisory lock, then placeholder-references + re-check) | NOT-ENC (G4b) | — | — | path unreachability is an abstract environment choice in `chunkLiveness.qnt` (design §3.2); covered by `store.gc.two-phase`, `store.put.placeholder-refs` and the mark/sweep tests |
| `62851c73d`, `132446e7e`, `5ba946682`, `adfd303d7` C1/C3, `bf7e516e4` C5 | sweep resurrection transitivity, referrer-first ordering, settle-before-delete, path_tenants re-check | NOT-ENC (G4b) | — | — | same disposition: `store.gc.sweep-recheck+2`, `store.gc.sweep-referrer-order`, `store.gc.sweep-cycle-reclaim`, `store.gc.tenant-retention` and their tests; the replacement leaves this layer untouched (design §4.3, §8) |

#### G5 — the repair loops reaped live uploads (heartbeat/liveness)

| Commit | Pre-fix behavior reverted | Class | Override module (calibration/refcount-g5.qnt) | Predicted | Verdict |
|---|---|---|---|---|---|
| `a1b49b4a3` | no heartbeat exists — an upload that outlives the stale threshold is reapable mid-flight | ENC | `refcountCalibG5NoHeartbeat` | S5 | **FALSIFIES** s5LiveOwnerNeverReaped @ calibStep (depth 4, 152/35) — the hot-path reclaim reaps a live, guard-armed owner. Baseline (as-built step over the same constants): HOLDS (9,705/981). Complements the Stage-B threshold-order check: that one inverts the ordering, this one removes the heartbeat itself |
| `064ceadbd` | wall-clock-driven guard heartbeat + claim plumbing for inline/slow ingests | ENC-A | heartbeat-existence content covered by `refcountCalibG5NoHeartbeat` (the model does not distinguish progress-driven from wall-clock heartbeats); the inline-ingest plumbing is outside the chunked-upload scope | S5 | by analogy (sibling falsified) |
| `2d7e4f9fd` (I-207) | no hot-path stale reclaim — a stale placeholder blocks every re-claim of its path until the 15-minute scanner | witness-form (pre-registered) | `refcountCalibG5NoHotpathReclaim` | the `noHotpathReclaim` witness becomes unviolable; no safety falsification | **AS PRE-REGISTERED**: noHotpathReclaim HOLDS under calibStep (268,869/46,623 — the repair path is gone) while boundsOK, m023NonNegative, CR-1, CR-2, CR-3, CR-4 and S5 all HOLD over the same alphabet (268,869/46,623) — the revert's harm is latency, not safety. The with-mechanism half of the pair is the wired Stage-B witness `quint-chunk-liveness-witness-hotpath-reclaim` |
| `da351aaff`, `f6bf0a546` | heartbeat/reap tasks moved to spawn_monitored | NOT-ENC | — | — | operability; pre-registered per-commit exception (the G7 treatment) |

#### G6 — lock order (4 commits)

`595b7ed9b`, `d64dbc4b0`, `5ad99b458`, `bf7e516e4` C4: **NOT-ENCODED**,
exactly as pre-registered (design §3.4) — PG row-lock acquisition order is
below the model's transaction-atomic granularity. Coverage stays with
`store.chunk.lock-order`, `with_sorted_retry`, and the existing tests; the
replacement shrinks the rule's site list but does not retire it.

#### G7 — background-loop operability (6 commits)

`bf7e516e4` C2/C3/C6/C7/C9, `adfd303d7` C4, `660825f19`, `947aaba79`,
`468fd725a`, `a97af109b`: **NOT-ENCODED**, as pre-registered — pagination,
per-row transaction isolation, gauge resets, SKIP LOCKED multi-replica
behavior and poison-row livelocks are below this model's granularity. The
design's "encode L4 only if cheap" option was evaluated and declined: a
faithful L4 would need per-row error injection and loop iteration
structure the model deliberately lacks. Coverage stays with the existing
loop tests; the replacement's collector inherits exactly this obligation
(design §7, the single-point-of-non-collection risk).

### HOLDS rows and their dispositions

No override predicted to falsify returned HOLDS: every ENC row above
falsified its predicted invariant on the first run, so none of the three
HOLDS dispositions (model gap / unstated property / redundancy candidate)
was triggered. The two rows that record HOLDS verdicts are both
by-construction: the `2d7e4f9fd` witness-form row (pre-registered by the
design as a liveness/latency property, demonstrated by the witness pair
plus the safety-intact run) and the restricted-alphabet baselines (HOLDS
is their required outcome, and all of them hold — with the one CR-1
baseline exception documented as a finding below, which is why that row's
attribution rests on the module-local ownership invariant instead). No
new invariant falsified on the unmodified model; there is no
stop-and-report event.

### Findings

- **The late-mark window (found by the `937a9c928` baseline run, walked
  against the code).** `mark_chunks_uploaded` is
  `UPDATE chunks SET uploaded_at = now() WHERE blake3_hash = ANY($1) AND
  uploaded_at IS NULL` — no `deleted` guard, no claim/generation gate
  (metadata/chunked.rs). If an owner's row is stale-reclaimed while the
  owner is still alive between its S3 PUTs and its mark (the reclaim
  decrements, soft-deletes, clears `uploaded_at`, enqueues), the owner's
  late mark re-asserts `uploaded_at` on the soft-deleted row; the drain's
  re-check (`deleted AND refcount = 0`) then passes and deletes the
  object, leaving confirmed-presence metadata with no backend object; the
  next writer of the same content trusts `uploaded_at` (CR-4-compliant)
  and skips the PUT — the M_033 harm shape without consulting the
  counter. As built this interleaving is excluded **solely by the
  heartbeat contract** (a live owner heartbeats every 30 s, so its row
  cannot reach the 300 s reclaim threshold between PUT and mark): the
  Stage-B crash regime holds CR-1 because a crashed owner never marks and
  a live owner is never reaped. The model run that exposes it relaxes
  exactly that contract (the calibration's contract-free clock), so this
  is **not** an as-built falsification and not a stop-and-report event —
  it is a documented dependency: `uploaded_at`-as-presence (CR-4(b),
  CR-1) leans on the heartbeat contract, not only S5. The dependency
  survives the replacement unchanged (mark, the drain re-check, and the
  reapers all survive — design §4.2), so it is carried into the Phase-1
  input list rather than being priced here.

### Permanent expect-violation witnesses (wired into nix/quint.nix)

Five of the ten override modules are wired as `quint-refcount-calib-*`
checks — one representative per encodable family with a plausible
regression path and a cheap state space (the retry campaign's
proportion). Each passes only while the checker still falsifies the
invariant under the module's `calibStep`.

| Check | Module | Violated invariant | Guards against |
|---|---|---|---|
| `quint-refcount-calib-g1-token-rollback` | `refcountCalibG1RollbackPreToken` | `cr3CounterRefinesFold` | losing the PlaceholderToken / generation gate on the in-process rollback (1cd975b90) |
| `quint-refcount-calib-g2-inline-reap` | `refcountCalibG2ReapInlineOnly` | `cr3CounterRefinesFold` | a cleanup path reverting to the inline-only delete (e5bdbff1b / I-040) |
| `quint-refcount-calib-g3-counter-presence` | `refcountCalibG3CounterAsPresence` | `cr4PresenceFromConfirmedUpload` | re-keying the needs-upload verdict on the liveness record (dd5c11376 / M_033) |
| `quint-refcount-calib-g4a-drain-recheck` | `refcountCalibG4aDrainNoRecheck` | `cr1NoLiveChunkCollected` | dropping the drain's same-transaction re-check before DeleteObject (aa738a5d7 / M_006) |
| `quint-refcount-calib-g5-no-heartbeat` | `refcountCalibG5NoHeartbeat` | `s5LiveOwnerNeverReaped` | losing the heartbeat that keeps live uploads below the reclaim thresholds (a1b49b4a3) |

The remaining five modules (`refcountCalibG1CompletionUnclaimGated`,
`refcountCalibG1ReapPathMatched`, `refcountCalibG2SweepCollapsedCount`,
`refcountCalibG3JointStaleSkip`, `refcountCalibG5NoHotpathReclaim`) are
evidence modules: committed, typechecked with the tree, re-runnable with
the command above, not in CI. G1/G2's wired checks guard machinery the
campaign intends to delete (the token, the decrement family); they stay
in CI until Phase 1c removes that machinery and are then retired or
re-pointed exactly as the retry campaign's were in its Phase 2; the
G3/G4a/G5 checks guard mechanisms that survive the replacement and are
re-pointed at the counter-free model of record in Phase 2 (design §3.4).

### Phase-0 exit-gate verdict (calibration criterion)

**Met for the calibration clause of design §5 / §5a.** Every encodable
family (G1, G2, G3, G4a, G5) falsifies at least one campaign invariant
through a representative override — G1: 3 overrides falsify (plus the
module-local ownership form); G2: 2; G3: 2 (including the pre-registered
joint-revert row); G4a: 1; G5: 1 plus the pre-registered witness-form
row — all as predicted, each row recording the falsified invariant,
depth, and state count, with restricted-alphabet overrides carrying their
baseline HOLDS and regime-verbatim overrides citing the Stage-B
exhaustive verdicts. Every non-encodable row (G4b, G6, G7, and the
per-commit NOT-ENC / ENC-A / SUBS rows inside G1–G5) carries its
pre-registered disposition with the missing dimension and the covering
rule/test named. No encodable-family representative failed to falsify
without an accepted explanation (§5a bullet 1 not tripped); no §3.3
invariant falsified on the unmodified as-built model (§5a bullet 4 not
tripped — the late-mark finding arises only under a relaxed clock and is
dispositioned above); the Stage-A consumer audit already established §5a
bullet (a) clean and the `'uploading'`-as-live spec check (§5a bullet 3)
closed. The calibration input to the go/no-go is therefore green. The
remaining Phase-0 gate items are outside this stage and still open: the
mark-scan cost measurement on production-scale data (and the junction
fallback decision it prices), the collect-soundness enforcement choice
(timeout vs monitored assumption), and the drafting of the replacement
`#r()` rules (`store.chunk.liveness-derived`, `store.gc.chunk-collect`).

### Phase-1 input list

What the calibration adds to the Phase-1 plan beyond the design's
existing commitments:

- **The late-mark window is a named dependency to keep or close.** The
  replacement keeps `mark_chunks_uploaded`, the drain re-check, and the
  stale reclaims verbatim, so CR-1/CR-4(b) keep leaning on the heartbeat
  contract through the window described under Findings. Phase 1a's
  replacement model (design §4.6) should add a late-mark witness pair
  alongside the mark-stale-race pair (reachability under a relaxed
  contract; excluded under the kept contract), and Phase 1 should either
  add `AND deleted = FALSE` to the mark statement (a one-conjunct change
  that closes the window structurally — the resurrect path already
  forces a re-upload after any soft-delete, so the narrowing costs
  nothing) or carry "the wall-clock heartbeat task outlives the PUT
  fan-out" as a named, monitored assumption next to the §4.1
  writer-transaction bound. Decision belongs to Phase 1a, not here (no
  Rust is touched in Phase 0).
- **The §4.6 acceptance re-run set gains one member.** Beyond the
  design's G4a/G5 re-runs, the `937a9c928` completion-clobber override
  should be re-pointed at the replacement model: the completion claim
  gate survives as a path-row janitor and its falsifiable content
  (premature visibility of an in-flight successor) is unchanged by the
  counter's removal.
- **G1/G2 acceptance rows flip to "cannot recur by construction"** once
  the counter, the token, and the decrement family are deleted (design
  §4.6); their wired calibration checks are retired or re-pointed in
  Phase 2. Until Release B ships, they stay in CI guarding the as-built
  machinery.
- **Keep the dedup verdict atomic with the upsert when the touch lands.**
  The `b1c7a9497` subsumption note: Phase 1a adds `last_referenced_at`
  to the same upsert statement; the §4.5 amendment of
  `store.cas.upsert-inserted+2` must keep the RETURNING-atomic wording so
  the pre-`b1c7a9497` re-query shape cannot reappear alongside the new
  column.
- **Loop-existence obligations stay outside the model.** The structural
  CR-2 encoding cannot see a missing background loop (the `d617bf3e5`
  sweep-wiring half), so the replacement's collector-existence and
  backstop-cadence obligations are carried by the runtime metrics and
  alerts the design already specifies plus the L4-style operability
  tests — not by the model. Do not claim model coverage for them in the
  Phase-1 exit gates.
- **The S4/L3 admission-predicate caveat carries over.** The replacement
  model should keep ownership gates falsifiable at the consequence level
  (the calibration override pattern), since its admission-predicate forms
  will have the same structural blindness to additive overrides.
- **I-207 stays latency-only.** The hot-path reclaim's chunk-awareness
  can be deleted in Phase 1c without a safety re-run (the witness-form
  row shows safety is untouched by its absence); what must be preserved
  is the path-level latency obligation (a stale placeholder yields within
  the 300 s threshold), which it keeps as a path-row janitor.

### Stage-C verify-marker status

No new tracey markers: the calibration checks are regression guards for
historical defect classes, not verifications of current spec rules, and
carry no `r[verify]` markers (the same policy as every other witness
check and as the retry campaign's calibration checks). The five Stage-A
rules' marker status is unchanged from Stage B.

## Phase 1 (in progress)

### Phase 1-pre — fallible parse landed; its Kani contract deferred

The fallible `chunk_list` parse for the future fail-closed mark phase
(`try_parse_unique_chunk_hashes` in `rio-store/src/gc/mod.rs`: corrupt
input is an `Err`, never an empty `Ok`; the legacy
`parse_unique_chunk_hashes` wrapper keeps the as-built C12 warn-and-empty
polarity at the existing callsites) landed in Phase 1-pre with unit tests
for every corrupt class, the dedup behavior, the empty-but-well-formed
manifest, and the wrapper's pinned polarity.

The design §4.6 Kani contract for that parse (no panic on arbitrary
input; `Err` exactly when `Manifest::deserialize` rejects; on `Ok` an
exact dedup of the entry hashes that is empty only for a zero-entry
manifest) was attempted as a sixth `kani-rio-store` harness over bounded
arbitrary inputs of one version byte plus four, then two, then one
36-byte entry, each with explicit unwind bounds. None of the attempts
converged inside the merge-gate budget on the CI builder, while the
member's five wired harnesses verify in seconds — the dominant cost is
the symbolic execution of the std Vec/slice/sort machinery the parse and
dedup use (the same blowup class the retry campaign recorded for
`kani-rio-retry-kernel`), not the contract assertions. The harness is
therefore NOT wired and no verify marker is claimed for it; the wired
coverage for the parse remains its unit tests plus the
`fuzz/rio-store` `manifest_deserialize` target, and the contract returns
with the Phase-2 Kani work (the `decide_collect` kernel), where a
dependency-free kernel extraction is the candidate shape. Measured
non-convergence figures are in the introducing commit message
(`feat(rio-store): add a fallible chunk_list parse...`).

### Phase 1a measurements and adjudications

#### T-1a.1 — mark-scan cost: NO-GO; the §5a junction fallback is triggered

The §5a go/no-go measurement (plan T-1a.1, sign-off item 5) was run
against the prescribed collector mark shape — one connection, keyset
pages over `manifest_data JOIN manifests`, the fallible per-manifest
parse, batched `INSERT … ON CONFLICT DO NOTHING` into
`TEMP TABLE live_chunks(blake3_hash BYTEA PRIMARY KEY)` — using the
`#[ignore]`d, env-tunable harness at
`rio-store/src/gc/mark_scan_bench.rs` (the entry-count mix and sharing
model are documented in its module doc: median a few dozen entries, a
tail at the 10 GB-NAR / ~160 k-entry class, cross-manifest dedup factor
≈ 2.4×), at three scale points — 15 k, 150 k, and the production-scale
1.5 M chunked paths — on the most production-like hardware available to
the campaign (a 192-core EPYC dev box, ephemeral PostgreSQL 18 with
fsync off and tmpfs-backed storage; faster than the production database
class on both I/O and clock, so the measurement is a lower bound on
production cost). Raw figures live in the introducing commit's message
and the run transcripts; the verdict-relevant magnitudes are below.

**Verdict: NO-GO** against the sign-off item 5 threshold (full mark scan
≤ 5 minutes at the ~1.5 M-path scale; linear-or-better growth;
temp-table-bounded memory):

- the production-scale scan takes roughly seven to eight times the
  five-minute budget (tens of minutes, not minutes);
- growth is super-linear — each 10× increase in path count costs
  roughly fourteen to fifteen times the scan wall-clock, because
  per-reference throughput degrades as the mark-set btree grows far
  past the session's local buffer pool — so the growth clause fails
  independently of the absolute-time clause;
- only the memory clause holds: the working set is bounded by the temp
  table (the client holds one page of manifests and one bounded insert
  buffer).

The dominant cost is the per-reference `ON CONFLICT` probe into a
temp-table btree that is orders of magnitude larger than PostgreSQL's
session-local `temp_buffers`. That cost is intrinsic to the prescribed
stream-parse-insert shape on a single backend — not an artifact of batch
size, page size, hardware, or the synthetic mix: at the measured
throughput, fitting the five-minute budget would require the store's
total reference volume to be several times smaller than any mix
consistent with the inventory's §3.3 shape (the known 10 GB-NAR-class
paths alone rule that out), and throughput keeps degrading as the mark
set grows.

**Consequences (design §5a; plan T-1a.1 step 4).** The junction fallback
— `chunk_refs(store_path_hash, blake3_hash)` maintained in the upgrade
transaction with `ON DELETE CASCADE`, mark as an indexed anti-join — is
triggered, NOT built: §5a requires re-deriving §4.1 and re-entering
design review to price the write-amplification and blob-vs-junction
drift obligations before any collector code exists. The collector tasks
of the Phase-1 plan (T-1a.3 onward) are void pending that re-entry.
Migration 068 / the upsert touch (T-1a.2) was also deliberately not
shipped ahead of the re-entry: whether `last_referenced_at` survives the
junction design, and in what shape, is a re-entry question, and shipped
migrations are frozen. The implied cadence/GC-lock-hold budget for the
as-designed shape — a scan of tens of minutes holding `GC_LOCK_ID` in
every `run_gc` invocation and every backstop run — is far outside what
phase-3-of-run_gc or a daily backstop can absorb, which is the same
no-go stated as an operational cost rather than a threshold breach.

The other two still-open Phase-0 items absorbed into Wave A1 — the
collect-soundness enforcement choice (T-1a.4's histogram + alert) and
the replacement `#r()` rule drafts with `chunkCollect.qnt` (T-1a.5) —
remain open: both are shaped by the mark mechanism the re-entry
chooses (the soundness condition's form and the model's mark action
both change under a junction-maintained mark), so recording a choice
now would prejudge the re-entry. The Wave A1 stop-and-report record is
`refcount-a1-blocker-T-1a.1.md` (campaign working notes, alongside the
design and plan documents).

#### T-1a.1 follow-up — set-based mark formulations (measurement spike, not a design decision)

Before the §5a re-entry prices the junction fallback, two set-based
reformulations of the SAME scan-time mark — liveness still derived
from `manifest_data` at collect time, no new write-path state, the
live set still rebuilt per cycle into a session temp table — were
measured with the same harness, fixture, scale points, and hardware as
the NO-GO record above (`gc::mark_scan_bench`, release profile, single
backend, session `work_mem = 4GB` for the set-based dedup, PostgreSQL
defaults otherwise; raw transcripts in the introducing commit
message):

- **copy + GROUP BY** (`mark_scan_bench_copy_groupby`) — the
  prescribed keyset scan and fallible client-side parse, unchanged,
  but references stream into an unindexed temp table via binary `COPY`
  and are deduplicated once at the end by a single set-based
  `GROUP BY` into `live_chunks` — no per-row `ON CONFLICT` probe, no
  btree maintained during the scan.
- **server-side expansion** (`mark_scan_bench_server_side`) — no
  client round-trip at all: a fail-closed validation pass (version
  byte, 36-byte entry alignment, `MAX_CHUNKS`; any violation aborts
  the cycle, preserving the §4.4 polarity), then one
  `CREATE TEMP TABLE … AS SELECT DISTINCT` statement that expands
  every `chunk_list` inside the server (`generate_series` +
  `substring` over a once-per-manifest detoasted copy) and
  deduplicates in the same statement.

Both formulations produce exactly the prescribed mark product (a
session temp table holding the distinct live hashes) and are pinned to
it: at every scale they reproduce the mark-set cardinality the
prescribed shape produced, and a known manifest's hashes are asserted
present in the result (a slicing/encoding-misalignment check). The
collect-phase anti-join stays outside the measured window, exactly as
in the NO-GO record.

| Formulation | 15 k paths | 150 k paths | 1.5 M paths | growth per 10× paths | spill at 1.5 M |
|---|---|---|---|---|---|
| Prescribed (per-row `ON CONFLICT`; the NO-GO above) | 11.1 s | 163.0 s | 2293.8 s (38 min 14 s) | 14.7×, 14.1× | n/a (20 GB PK btree) |
| Copy + `GROUP BY` | 1.9 s | 29.0 s | 374.3 s (6 min 14 s) | 15.0×, 12.9× | 2.5 GB |
| Server-side expansion | 1.5 s | 18.8 s | 219.7 s (3 min 40 s) | 12.4×, 11.7× | 2.5 GB |

Verdict against the sign-off item 5 threshold (full mark scan ≤ 5
minutes at the ~1.5 M-path scale; linear-or-better growth; bounded
memory):

- **Copy + GROUP BY: does not meet.** Roughly 1.25× the five-minute
  budget at 1.5 M (clause 1 fail); growth still super-linear
  (clause 2 fail); only the memory clause holds (the client holds one
  page plus one ~38 MB COPY buffer, the server side is
  `work_mem`-bounded and spills to temp files). Most of its cost is
  the single-connection round-trip itself — about half the wall-clock
  goes to pulling ~12 GB of blobs out and pushing ~12.5 GB of
  references back over one connection before dedup even starts.
- **Server-side expansion: meets the time and memory clauses with
  margin on this hardware.** 3 min 40 s is ~73 % of the budget
  (clause 1 pass); memory is bounded by `work_mem` (≈2.5 GB of
  temp-file spill observed) plus the ~9.4 GB result table (clause 3
  pass). Growth is 12.4× then 11.7× per 10× paths — ≈1.2× per decade
  against the reference volume, so not strictly linear (clause 2
  marginal), but per-reference throughput falls only 2.4 → 1.5 M
  refs/s across two decades versus 306 k → 144 k refs/s for the
  prescribed shape; the degradation is attributable to the dedup hash
  outgrowing CPU caches and to the spill at the largest point, not to
  a per-row probe that keeps getting more expensive. A further 10×
  store (15 M chunked paths) extrapolates to ~40 minutes — graceful
  degradation, not unbounded headroom.

Caveats carried from the NO-GO record: same dev-box hardware
(tmpfs-backed PostgreSQL, fsync off, EPYC clocks), so absolute times
remain a lower bound on production cost and the ~27 % margin shrinks
on a slower database host; the dominant cost is single-core CPU in the
expansion + aggregate, and `workers_planned = 0` throughout (no
parallel query was used — headroom exists there if the statement's
target is made non-temporary). Other shapes were not pursued: the
threshold was met with margin by a same-architecture formulation
(the spike's stop-early condition), and the "anti-join `chunks`
directly against the aggregated references" variant needs a populated
production-scale `chunks` table in the fixture to mean anything. An
incremental mark (only manifests changed since the last cycle) was
likewise not measured: it changes the §4.1 correctness argument (the
live set would no longer be re-derived from scratch each cycle) and is
out of scope for a measurement spike.

What this changes for the §5a re-entry: the junction fallback is no
longer the only priced option. The scan-time architecture has a
formulation that fits the stated budget at the design-point scale with
no new write-path state; the re-entry can weigh the junction's
write-amplification and blob-vs-junction drift obligations against a
server-side mark whose remaining risks are production-hardware margin,
the strictness of the linear-growth clause, and plan-shape sensitivity
across PostgreSQL versions, rather than against a 38-minute scan.

#### T-1a.1b — collect-phase anti-join (re-entry gate (c))

Both records above cover the mark phase only; design §5b gate (c)
requires the collect-phase anti-join priced on a populated
production-scale `chunks` table, recorded before any Wave A2 task
starts. The bench gained a third entry point
(`mark_scan_bench_collect_phase`) that seeds `chunks` to match the
manifest fixture and then runs the full cycle of the adopted
formulation on one connection, timing three phases separately: the
server-side mark (validation pass + set-based expansion into
`live_chunks`), a one-time prepare step on the mark product (a unique
index plus ANALYZE — what makes the per-batch anti-join an index probe
instead of a per-batch hash or sort of the whole mark set), and the
batched collect loop itself in the orphan-chunk-sweep skeleton (per
batch, one transaction: a candidate scan with the `NOT EXISTS`
anti-join and the `GREATEST(created_at, last_referenced_at)` grace
term, then a sorted `= ANY` soft-delete `RETURNING`), with a keyset
cursor on the candidate scan, mirrored into the anti-join's inner
side, so each `chunks` row and each mark-set entry is examined once
across the whole loop. Without the cursor the loop is quadratic in
batch count at the design point — every batch re-probes all marked
rows that precede its candidates (and the as-written single-statement
`IN (… LIMIT …)` form additionally seq-scans `chunks` once per batch
on the UPDATE side, which the first 15 k probe run surfaced) — so the
live arm (T-1a.8) is expected to adopt the same candidate-scan +
sorted-`ANY` + cursor shape, and the bench's EXPLAIN guard pins it.

Fixture: one `chunks` row per distinct referenced hash (refcount 1,
`uploaded_at` set, `last_referenced_at` NULL), plus a 10 %
unreferenced population — 90 % of it old and untouched (the expected
victims), 5 % younger than grace, 5 % old but freshly touched via
`last_referenced_at` — so the grace term and the migration-068 touch
column both do real filtering work and the victim volume is a
deliberately generous bound on one cycle's garbage (a store turning
over a tenth of its references between collect cycles). Same hardware,
PostgreSQL, `work_mem`, and synthetic mix as the mark records; collect
batch LIMIT 10,000. Raw figures are in the introducing commit message
and the run transcripts; the verdict-relevant magnitudes:

| paths | mark | prepare | collect (victims) | combined |
|---|---|---|---|---|
| 15 k | 1.6 s | 0.5 s | 4.1 s (136 k) | 6.3 s |
| 150 k | 18.5 s | 4.3 s | 35.3 s (1.26 M) | 58.1 s |
| 1.5 M | 3 min 24 s | 51 s | 7 min 40 s (12.4 M, 1,243 batches) | 11 min 54 s |

**Verdict against the amended budget (the five-minute-class lock-held
window of §5b, graceful-growth qualification): exceeded.** The mark
phase alone stays within the budget (3 min 24 s here, consistent with
the adopted record); adding the prepare step and the collect pass —
every existing chunk row probed once against the mark product plus the
soft-delete writes for the victim volume — brings the lock-held window
for a full cycle to ~12 minutes at the design point, ~2.4× the
five-minute-class budget, growing ~1.0–1.3× per decade on top of the
mark's own growth (combined 6.3 s → 58.1 s → 714 s across the three
points). Per the plan's T-1a.1b step 4 and the rollback-story abort
criterion, this routes to an explicit adjudication by the campaign
owner — an accepted cadence/lock-budget relaxation recorded here, or a
further design re-entry — and Wave A2 does not start until that
adjudication is recorded. Scan/collect duration does not enter the
collect-soundness condition (§4.1), so the breach is a cost/cadence
question, not a correctness one; the candidate levers the §5b record
already names (backstop-only cadence, parallel-query headroom —
`workers_planned = 0` throughout these runs too) apply to the combined
cycle as much as to the mark.

**EXPLAIN plan-shape guard (gate (b)) extended to the anti-join.** The
bench now asserts, at and above the 150 k scale point, that the
expansion plan keeps its set-based aggregate over the server-side
expansion and that the per-batch candidate scan is index-driven on
both sides (`chunks_pkey` and the `live_chunks` index; no Seq Scan of
either relation, no Sort) — the plan regression that would silently
reproduce the NO-GO cost class now fails the bench instead of
shipping. The measured candidate-scan shape at the enforced scale
points is a Merge Anti Join over the two indexes with the keyset bound
on both sides; `workers_planned = 0` throughout, so the parallel-query
headroom noted in the mark record remains unexploited here too.

Caveats carried forward: same dev-box hardware as the mark records
(tmpfs-backed PostgreSQL, fsync off, EPYC clocks), so absolute times
are a lower bound on production cost; the victim-write term scales
with the unreferenced ratio (10 % here — a generous steady-state
bound), while the scan term (every existing chunk row probed against
the mark set once) is fixed by store size; the prepare term is paid
once per cycle and could be folded into the expansion statement later
without changing the architecture.

#### T-1a.1c — capped-cycle confirmation (re-entry gate (c), v4 form)

The second re-entry redefined gate (c) to the capped cycle (design §4.1
step 3 v4, `COLLECT_CYCLE_VICTIM_CAP = 500_000`; plan sign-off item 8):
the gate now asks whether mark + prepare + collect-at-cap fits the
combined five-minute (300 s) lock-held budget at the 1.5 M-path design
point, with a backlog larger than the cap draining across cycles by
design rather than stretching one cycle. The bench's collect loop
gained the cap (default = the design value, env-tunable for smoke
runs), a clamped final batch so a cycle never overshoots the cap, a
split of the collect term into its anti-join candidate-scan and
soft-delete halves (so the sparse full-pass scan cost stays visible
separately from the victim-write cost the cap bounds), and the keyset
cursor at the stop point in the report. Mark, prepare, batch shape,
fixture, hardware, PostgreSQL, `work_mem`, and batch LIMIT are
unchanged from the T-1a.1b record; the 12.4 M-victim backlog is
exactly the case the cap must absorb.

Two release-profile runs at the design point were taken: the first
overlapped repository-scan and build activity from the executor's own
session on the shared dev box during the mark window, and is kept on
record as a contended variance data point rather than discarded (the
same identify-the-artifact-and-rerun discipline as the T-1a.1b
bring-up probes); the second ran with the executor quiescent. Raw
figures in the introducing commit message and the run transcripts;
the verdict-relevant magnitudes:

| run | mark | prepare | collect at cap (scan + soft-delete) | combined |
|---|---|---|---|---|
| r1 (executor-contended) | 228.6 s | 53.0 s | 21.0 s (18.5 s + 2.5 s) | 302.6 s |
| r2 (executor quiescent) | 207.6 s | 52.3 s | 18.4 s (16.0 s + 2.4 s) | 278.3 s |

Both runs: 50 batches, exactly 500,000 victims soft-deleted, cap
reached with the keyset cursor reported, plan-shape guard (gate (b))
green, `workers_planned = 0`, mark-set size 138,042,866 (identical to
the T-1a.1b record), and the protected populations (referenced,
younger-than-grace, freshly-touched) untouched — the capped cycle's
soundness shape is the uncapped cycle's, only the stopping rule
differs. The collect-at-cap term measured ≈37 µs/victim in the
quiescent run — the same per-victim cost as the T-1a.1b record and
well inside the 2× allowance the cap derivation budgeted (≈42 µs in
the contended run, still inside it).

**Verdict against the redefined gate (c) (combined ≤ 300 s at the
design point): PASS** — 278.3 s, ≈93 % of the budget, in the
executor-quiescent run; the contended run exceeded the budget by
0.9 % (302.6 s), which is within the run-to-run band of the mark term
on this shared box and is recorded, not adjudicated over the
quiescent run.

What the pair of runs adds to the record: mark + prepare alone is
254–282 s across the three measured cycles of this fixture (T-1a.1b
and the two runs here), i.e. 85–94 % of the budget is consumed before
any collect work happens, and the run-to-run band of those terms on
this shared dev box (~10 %) is the same order as the entire
collect-at-cap allowance. The cap bounds the term it was derived to
bound; the remaining margin is carried almost entirely by mark-phase
variance. That makes re-entry gate (a) — the production-DB-class mark
confirmation from the additive-release window, on the production
database where the cycle runs alone under the GC lock — the
load-bearing check for the budget, exactly as the v3 record already
framed it; the cycle-duration histogram (T-1a.3) is the runtime
monitor for it. Caveats carried forward otherwise unchanged from
T-1a.1b: tmpfs-backed PostgreSQL, fsync off, EPYC clocks (absolute
times are a lower bound on production cost); the sparse full-pass
scan term is bounded by store size, not by the cap, and stays
monitored (cycle-duration histogram + stalled alert), not gated.

#### T-1a.4 — collect-soundness enforcement: the monitored-assumption option (closes the second still-open Phase-0 item)

The §4.1 collect-soundness condition — no chunk-referencing write
transaction outlives `grace − clock slack` — is carried as a **named,
monitored assumption**, not as enforcement (plan P4 / sign-off
item 3):

- `rio_store_chunk_upgrade_tx_seconds` (histogram) measures every
  `upgrade_manifest_to_chunked` transaction from `begin()` to
  `commit()` — the single chunk-referencing write transaction in the
  system. Bucket boundaries are placed at the alert thresholds so the
  threshold queries are exact.
- The wired alert `RioStoreChunkUpgradeTxSlow`
  (infra/helm/rio-build/templates/prometheusrule.yaml) fires at
  warning when the p99 over a 15-minute window exceeds `grace/2`
  (150 s) and at critical above `grace − 60 s` (240 s). This is the
  runtime carrier of the assumption, in the same sense that the
  READ-COMMITTED re-evaluation assumption is carried as a named
  assumption: a firing alert means the soundness margin is eroding
  and grace (or the upload path) needs attention before the live
  collect arm is enabled or kept enabled.
- No `statement_timeout` is set in Phase 1 (the
  enforcement-by-timeout option of §4.1): a timeout would add a new
  writer-failure mode for zero observed need, and the histogram is
  exactly the data that would justify a timeout value later if one is
  wanted.
- The collector-side alternative — anchoring the collect threshold at
  `least(cycle_started_at, min(xact_start) of transactions open at
  the snapshot)` — is noted as available but not taken (it
  complicates the snapshot for a window the grace term already covers
  whenever the assumption holds).

This closes the second still-open Phase-0 item (the collect-soundness
enforcement choice; the first — the mark-scan cost measurement — was
closed by the T-1a.1 records above, and the third — the replacement
`#r()` rule drafts — lands with the replacement model). The runbook
section in docs/ops/gc-enablement.typ documents the alert's meaning
and remediation; the histogram is live from the additive release, so
the Release-A observation window (re-entry gate (a), T-1a.7) also
produces the empirical upgrade-transaction-duration distribution this
assumption is judged against.
