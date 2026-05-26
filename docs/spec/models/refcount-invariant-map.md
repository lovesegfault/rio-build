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
and the calibration table are later Phase-0 stages and are NOT part of this
document's claims.

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
| T1 | xtask `i201-stranded-chunks` QA probe vs `store.chunk.liveness-not-presence` *(new)* | (tooling, not spec) — the probe asserts "PG expects an S3 object" from `refcount > 0 AND NOT deleted`, the counter-as-presence inference the new rule forbids for probes and tooling | The serving path never does this (M_033); the probe also has a false-positive window against in-flight uploads (`refcount ≥ 1`, `uploaded_at IS NULL`, PUT not yet done) | Pre-committed fate in design §4.3: re-point the predicate at `uploaded_at IS NOT NULL AND NOT deleted` (matching the server-side VerifyChunks scan), no later than Release B, allowed earlier since the new predicate is column-independent. Recorded here because the rule is added marker-first; the xtask code is deliberately not changed in Phase 0. Consumer-audit row 1. |
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
| 1 | `xtask/src/k8s/qa/scenarios/i201_stranded_chunks.rs` (module doc; the sample query at the top of `run`; the fail message) | SQL reader: `SELECT … FROM chunks WHERE refcount > 0 AND NOT deleted ORDER BY created_at DESC LIMIT 1000`, then per-hash S3 HeadObject; "PG refcount>0 but S3 404" is the failure verdict. Counter-as-presence inference in tooling — contradiction T1 against the new `store.chunk.liveness-not-presence`; also false-positive-prone against in-flight uploads (`refcount ≥ 1`, `uploaded_at IS NULL`). | Design §4.3: predicate moves to `uploaded_at IS NOT NULL AND NOT deleted` (matching the server-side VerifyChunks scan and CR-4); fail-message text updated. **No later than Release B**; may land earlier — the new predicate is column-independent and removes the in-flight false-positive window. |
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
