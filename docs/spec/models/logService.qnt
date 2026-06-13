// -*- mode: Bluespec; -*-
//
// Model C of the rio-store build-log subsystem: the LogService
// session/chunk/dedup protocol (`rio-store/src/logs/{sessions,chunks,
// ingest,gate,tail,sweep,service}.rs` plus the builder-side at-least-once
// uploader `rio-builder/src/log_upload.rs`), driven by an adversarial
// builder that re-sends, fabricates, overlaps, or abandons.
//
// This is the successor to the retired `logBufferLifecycle.qnt` (model A
// of the deleted in-scheduler log subsystem). The architecture it models
// has no lease composition (LogService never reads the scheduler's leader
// election), no reaps, no tenure pins, and no finalizing write — the
// PREDECESSOR CAMPAIGN TRANSFER block below records which of the
// predecessor's nine invariants transfer, which are vacuous, and why.
// The acceptance test for this model is the predecessor's 22-bug
// calibration table re-run against the new architecture — the MODEL-C
// ACCEPTANCE TABLE below, satisfied.
//
// WHAT IS MODELED:
//   - the per-execution build + uploader: monotone line production, the
//     ack-trimmed retransmit buffer, reconnect-and-replay, the drain
//     abandonment, the permanent-rejection discard
//     (`log_upload.rs::UploadTask`);
//   - the open-time binding + completeness gate (`gate.rs::
//     check_append_open`): latest-assignment admission, the complete-log
//     seal, the final-line-count handoff to the session;
//   - the per-stream ingest session (`ingest.rs::IngestSession`): the
//     accept predicate (monotone floor, the per-append completeness
//     ceiling, the straddling-batch truncation), the buffer, the
//     contiguous-prefix chunk cut, the abort that drops the buffer;
//   - the chunk manifest: INSERT-only `(exec, session, first_line,
//     line_count)` rows, written only by a cut of the named session;
//   - the ack: `durable_through_line` deliverable to the builder only
//     after the chunk commit, only while the builder is still connected
//     to the cutting session;
//   - the read path (`tail.rs::{read_manifest_range, read_chunk,
//     LineCursor}`): the `(first_line, session_id)`-ordered walk with the
//     watermark dedup, encoded as the code's fold (not as the set union
//     it is supposed to compute) so `servedSpanExact` checks the fold;
//   - the completeness predicate (`gate.rs::log_is_complete`): terminal
//     status, known count, the contiguous-coverage fold;
//   - the scheduler's terminal stamp (`completion.rs`): the 0 -> NULL
//     final-line-count mapping;
//   - the TTL sweep (`sweep.rs::sweep_expired_logs`): the v2
//     eligibility (`rio_retry_kernel::exec_row_sweep_eligible` —
//     terminal AND attempt-ledger-unreferenced AND aged out; the
//     fourth kernel conjunct, no-active-assignment, is scheduler-side
//     state outside this model and its omission only WIDENS the
//     modeled sweep, so every safety verdict transfers) and the
//     row left to the SCHEDULER's `gcExecRow` reclaim — the bughunt
//     wave's ownership split (`store.log.sweep-ownership`);
//   - the execution-kind population (bughunt-wave B2): a
//     materialization attempt of the same derivation can take the
//     latest-assignment slot, and the open gate's kind conjunct
//     (`gate.rs`'s `attempt_kind` read) admits build executions only;
//   - the uploader's loss-disclosure accounting (`log_upload.rs::
//     disclose`, the single chokepoint every abandoned line routes
//     through): an abandonment either counted its real loss or had
//     none to count.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced):
//   - Line content and byte sizes. Lines are integers (worker-assigned
//     line numbers); every gate and every fold in the code is interval
//     algebra over those numbers. The price: the content/provenance harm
//     of a builder fabricating DIFFERENT BYTES at a line number its build
//     legitimately produced is invisible (the served line number is
//     "covered" either way). The blast radius of that harm is the
//     builder's own execution's log, whose content it controls anyway.
//     Same price the predecessor model paid and documented.
//   - The byte/line/chunk-count resource bounds (`store.log.
//     ingest-bounds`), the per-line truncation, the stream-count and
//     byte-budget semaphores, and the line-number-overflow rejection.
//     Resource bounds, not protocol; the overflow arm is unrepresentable
//     in the bounded line domain (a Kani/fuzz obligation).
//   - The S3 object behind a manifest row as separate state. The
//     object-before-manifest write order and the loud
//     missing-object read error are unit-tested code branches; modeling
//     the split adds a state dimension whose only constraint is a
//     restatement of that ordering. A manifest row IS its chunk here.
//   - The ingest-session lease's heartbeat clock (`sessions.rs`). The
//     lease is documented as an admission/routing mechanism, not a
//     mutual-exclusion guarantee: the protocol is correct under session
//     overlap, and the model reaches the overlap directly (a detached
//     session still cutting while its successor also cuts) without the
//     30s staleness clock that bounds how long the overlap lasts.
//   - Token expiry. `openSession` requires the execution to be
//     unexpired: the assignment token's lifetime is far shorter than the
//     30-day log-retention TTL, so a fresh stream open for an expired
//     execution is unreachable in production. Without this assumption
//     the model reaches a state where a swept execution accumulates new
//     manifest rows that no later sweep pass re-finds (the sweep's
//     SELECT is over `drv_executions`, whose row is already gone) — a
//     bounded resource leak, not a safety violation. The narrower
//     instance that survives the assumption — a session opened BEFORE
//     expiry that cuts a chunk after the sweep's manifest DELETE but
//     whose execution row the same pass then deletes — is reachable
//     here and violates nothing; it is the same bounded leak.
//   - The cross-replica TailLog proxy. Read-side plumbing over the
//     same manifest + buffer state the model does constrain; the
//     history-then-live seam invariant is pinned by
//     `subscribe_during_in_flight_cut_sees_drained_lines` and the
//     `log-service` VM scenario. The live-tail subscription fan-out
//     and the reconnect cursor — EXCLUDED here until the bughunt
//     wave — are now the served-stream (reader) plane: `ENABLE_READER`
//     consts/vars/actions below, regime `logServiceServed`, pre-fix
//     calibration `logServiceCalibReaderAdvance`. The reader plane is
//     deliberately OUTSIDE the mbt_tests projection: `serve_tail` is
//     one continuous async loop, not a steppable surface — its
//     conformance is carried by the store unit tests
//     (`follow_recovers_fanout_drops_in_stream` and siblings) and the
//     dashboard's verbatim case-table mirror; the projection replays
//     the ingest planes only (documented no-op disposition).
//   - The HMAC layer, the token-vs-header derivation match, and the
//     executor-identity comparison. The gate's checks collapse to "is
//     the claimed execution the derivation's latest assignment" for
//     tokens the scheduler actually mints (one token per (exec,
//     executor) pair, minted together); forgery is below the model's
//     trust boundary. The writer identity here IS the execution index.
//
// ENCODING DECISIONS:
//   - `delivered(e)` (the conservation-law ghost) is the contiguous
//     prefix `[0, producedEnd(e))` of lines the build has emitted into
//     its uploader. The builder's retransmit buffer is the derived
//     sub-range `[ackedBelow(e), producedEnd(e))` (empty once the
//     uploader abandons) — the code's buffer is batch-granular and can
//     retain already-durable lines slightly longer; the model's
//     line-granular form is a subset, which only makes the conservation
//     law harder to satisfy.
//   - A chunk is its manifest row `{sess, first, count}`; the lines it
//     physically holds are `[first, first+count)` by construction
//     because the cut drains a contiguous run and records its length —
//     the per-chunk half of span exactness is structural in the code
//     (`ingest.rs::contiguous_prefix_len` + `lines.len()`) and therefore
//     also in the model. The non-structural half — the READ path's
//     ordered-walk-with-watermark over overlapping chunks — is encoded
//     as the code's fold and checked against the set union it is
//     supposed to equal (`servedSpanExact`).
//   - The session's "lines accepted at or after the completeness ceiling
//     was learned" set is derived from `hwAtLearn` (the session's
//     high-water mark at the moment the ceiling was learned) rather than
//     carried as a ghost: accepted line numbers are strictly increasing
//     within a session (the monotone floor), so the post-learn accepts
//     are exactly the accepted lines >= hwAtLearn.
//   - One derivation. Entries for distinct derivations share no state;
//     the cross-execution scenarios (supersession, the foreign-token
//     rejection) need a second EXECUTION of the same derivation, not a
//     second derivation.
//
// The invariants, their witnesses, and the per-regime non-vacuity
// procedures are documented at their definitions below. Measured state
// counts, depths, and wall-clocks live in the introducing commits'
// messages and the CI checks' output transcripts, never in this file.
//
// ===========================================================================
// PREDECESSOR CAMPAIGN TRANSFER (relocated from the retired build-log
// invariant map's campaign close-out; the full original is in git
// history). The in-scheduler subsystem the predecessor campaign modeled
// (rio-scheduler/src/logs/: the per-derivation ring buffer, push_for,
// the gap-merge fold, the four reaps, the lease-gated flusher, the
// stored-coverage reconciliation, the drv_logs row) was deleted and
// replaced by this model's subject. The predecessor campaign found,
// confirmed, fixed and proved ONE new bug in the old subsystem
// (calibration entry #0: the gap-merge fold double-counts a re-delivered
// overlap once the stored-coverage reconcile empties the ring; the
// accounted_below accept floor was the fix). Its calibration against the
// 22 historical bugs: 14 re-findable by the invariant set, 2 proven
// redundant for safety, the rest not-encoded or model-B; model B
// (cross-replica tenure-pinned finalization) was cancelled outright —
// its subject does not exist here — and the Phase-6 simplification
// ledger closed with "the architecture change was the simplification".
//
// THE NINE PREDECESSOR INVARIANTS AGAINST THE LOGSERVICE (the rebase
// audit's transfer table, verbatim):
// | Invariant | Disposition | Why |
// |---|---|---|
// | `noCrossExecContamination` | **Structural** | Storage is keyed by `exec_id` end-to-end (chunk key, manifest PK, the stream bound to one execution at open); a re-dispatch mints a new `exec_id` and a new key namespace. The in-memory carrier a restamp could contaminate does not exist — the ingest session is created per stream and dropped with it. |
// | `lineSpanExact` | **Gap → fixed** | The per-chunk half is structural (a chunk's recorded count is the physical length of a contiguous run; no gap arithmetic exists to overshoot). The log-level half moved into the completeness gate, whose post-terminal MUST (drop accepted lines at or past `final_line_count`) was specified but unimplemented — the one real protocol finding of the audit, fixed during integration by enforcing the gate on every append rather than only at stream open. |
// | `bindingGateExcludesForeignExecutors` | **Structural** | Relocated and strengthened: `store.log.append-auth` (HMAC token → derivation match → latest-assignment + builder binding) runs at stream open, and the stream is then bound to one `(exec_id, session_id)` whose chunk keys cannot name any other execution. |
// | `everyRetainedEntryIsJustified` | **Vacuous** | The four orphan shapes and their three differently-gated reapers are gone. The only retained in-memory state is the per-stream ingest buffer, whose lifetime is the gRPC stream's; the persistent analog (a manifest row) is justified by definition and reaped only by the TTL sweep. |
// | `noSilentLineLoss` | **Structural** | A line leaves the builder's retransmit buffer only when an ack covers it; an ack is sent only after the manifest INSERT commits; the manifest row is written only after the object PUT. Every remaining loss channel is disclosed (the computed `is_complete = false`, the abandoned-drain counter, the missing-object read error). |
// | `noStaleSealOnLiveCarrier` | **Vacuous** | No seal latch and no carrier. Completeness is a monotone predicate recomputed from the DB on every open and every read; a seal that outlives the state that justified it is unrepresentable. |
// | `finalizedRowFrozen` | **Vacuous** | There is no finalized row to freeze: chunks are write-once, the manifest is INSERT-only, `final_line_count` is stamped once at terminal, and no finalizing write consolidates other writers' data. |
// | `storedCoverageNeverRegresses` | **Vacuous** | Coverage is the union of INSERT-only manifest rows; there is no reconcile that folds a stored row into a new write, so nothing can replace a larger span with a smaller one. Overlapping sessions union at read time. |
// | `boundsOK` | **Vacuous** | The model's own state-ceiling tripwire (a TLC finite-state-space artifact) has no subject without the model. The implementation bounds it stood beside (the ring byte cap, the periodic flush) are replaced by `store.log.ingest-bounds`. |
//
// **Successor.** The continuing campaign re-targets onto the LogService
//
// MODEL-C ACCEPTANCE TABLE (the predecessor's 22-bug corpus against
// THIS model, verbatim from the retired map — the acceptance test the
// transfer demanded, executed; quint.nix's log-service section points
// here):
// `docs/spec/models/logService.qnt` is the successor model: the builder's
// at-least-once ack-trimmed uploader, the open-time binding + completeness
// gate, the per-stream ingest session and its accept predicate, the
// INSERT-only chunk manifest, the read path's ordered-walk dedup over
// overlapping sessions, the read-time completeness fold, and the TTL sweep.
// Four exhaustive TLC regimes (`quint-log-service-{base,redispatch,resend,
// sweep}` in `nix/quint.nix`) assert its invariants over their full
// reachable state spaces; twelve expect-violation witness checks pin the
// contended states as reachable. Measured state counts and wall-clocks are
// in the introducing commit's message and the checks' transcripts.
//
// ### The model-C invariants
//
// | Invariant | Predecessor | What it checks |
// |---|---|---|
// | `noCrossExecContamination` | `noCrossExecContamination` | Execution E's manifest holds only lines E's own bound builder sent. The defense is structural (the chunk key embeds the exec_id the stream was bound to at open); the invariant is the regression guard that the cut keys its chunk by the cutting session's execution. |
// | `authGateExcludesUnassignedWriters` | `authGateExcludesForeignWriters` (renamed at the bughunt-wave B2 re-keying) | The stream-open predicate admits a claimed execution only while its OWN assignment row exists (the v2 claimed-exec gate, `store.log.append-auth+2`): supersession alone no longer revokes write authority — only the scheduler's in-place row rewrite does. `logServiceCalibGateRow` pins that dropping the row conjunct falsifies it. |
// | `acceptedWithinCap` | — (new; bughunt-wave B2, merged_bug_207) | The durable accepted footprint (committed chunks ∪ session buffers ∪ fabricated lines) never exceeds the per-execution cap, across reconnects. Measured against durable state, not the accounting ghost, so the pre-fix per-session encoding (`logServiceCalibSessionCaps`: reconnects reset the account and the open gate has no durable check) falsifies it. The boundary is pinned reachable by `quint-log-service-witness-cap-boundary`. |
// | `noSilentLineLoss` | `noSilentLineLoss` | The four-disjunct disclosed-loss form: every line the build emitted is in the builder's retransmit buffer, in a store-side ingest buffer, covered by a manifest chunk, or the loss is disclosed (the log does not read as complete / the uploader recorded its abandonment). |
// | `ackImpliesDurable` | — (new) | The honest uploader's load-bearing refinement of the conservation law: every line below the builder's acked watermark is manifest-covered until the TTL sweep removes it. Asserted only in the regimes without the fabricating client (see the val's doc for why a client that mis-numbers its own lines forfeits the per-ack durability claim). |
// | `servedSpanExact` | `lineSpanExact` | The read path's ordered-walk-with-watermark over the manifest — including overlapping chunks from two sessions — yields exactly the union of the chunks' line ranges, each line exactly once. The fold is encoded as the code's walk, not as the union it is supposed to compute. |
// | `completeLogServesAllProduced` | — (the conservation law's bite, restated) | A log that reads complete serves every line the build produced. |
// | `completenessGate` | — (new; the audit's defect #1) | Once a session has learned the execution's recorded `final_line_count`, every line it subsequently accepts is below it. Lines accepted before the mid-stream refresh observed the seal are the disclosed bounded residual. |
//
// ### The 22-bug corpus against model C
//
// Verdict legend: **CONSTRUCTION** = the state or mechanism the bug lived
// in does not exist in the new architecture; the model's transition
// relation cannot represent the harmful write. **CHECKED(inv, regime)** =
// the equivalent hazard exists at the named site and the named invariant
// holds over the named regime's full reachable state space, with the
// contended scenario pinned reachable by the named witness check.
// **OUTSIDE** = no model footprint then, no model footprint now;
// conventional tests own it.
//
// | Original row | Old fix | Model-C verdict | Where the hazard went |
// |---|---|---|---|
// | `3a55474ac` | accounted-floor accept gate | **CHECKED**(`servedSpanExact`, resend) | The re-delivered batch after an ambiguous disconnect lands as a *manifest overlap* (a second session's chunk covering the same line range), not a span overstatement — there is no gap-merge fold to double-count it. The hazard moved to the read path's `(first_line, session_id)` ordered walk and `LineCursor` watermark (`tail.rs`), which must serve each overlapped line exactly once; `quint-log-service-witness-overlap` and `-dedup` pin the overlap and the suppressed duplicate as reachable. The within-one-session re-delivery is rejected by `high_water_line` (the monotone floor — the `accounted_below` fix built in from the start, never reset by the buffer emptying). |
// | `496e6fb14` | recv-task (executor, drv) binding gate | **CHECKED**(`authGateExcludesUnassignedWriters`, redispatch) | Relocated to `gate.rs::check_append_open`: HMAC token, token-vs-header derivation match, claimed-exec row + builder + kind binding (v2 at the bughunt-wave: the latest-assignment race was merged_bug_101's displacement hole — a materialization mint or re-dispatch silently revoked a terminal-but-incomplete build's late replay; authority now keys on the claimed exec's own row and the rejection moved to the in-place-rewrite arm, pinned by `quint-log-service-witness-superseded-rejected` over the rewritten row). The identity comparisons are pure equality on signed token fields. The content/provenance harm is still erased by the line-as-integer encoding — the same documented price the predecessor paid. |
// | `496e6fb14` (residual) | legacy `push()`'s `or_default()` unstamped-entry allocation | **CONSTRUCTION** | There is no entry to allocate: a stream that fails the gate never creates a session, and the per-replica stream-count and byte-budget semaphores bound the resource the unstamped entry leaked. Resource bounds, outside the model. |
// | `7beb1ca00` #1 | gateway `ForwardLogBatch` gated on the accept verdict | **CONSTRUCTION** | The live-tail fan-out happens inside `accept()` after every gate (`ingest.rs`), so a rejected batch is never fanned out. The fan-out sink itself is read-side plumbing outside the model. |
// | `7beb1ca00` #2 | `FlushRequest.exec_id` stale-drain pin | **CONSTRUCTION** | No finalizing drain exists. A cut drains the cutting session's own buffer into the cutting session's own `(exec, session)` keyspace; there is no request that names an execution other than the one the stream was bound to at open. `noCrossExecContamination` is the regression guard. |
// | `7beb1ca00` #3 | `LogFlusher` periodic arms leader-gated | **CONSTRUCTION** | No leader, no flusher. The periodic cut is per-stream and INSERT-only; two replicas concurrently cutting for one execution are two sessions whose chunk sets union at read time instead of two writers racing an UPSERT. |
// | `7ffbf1415` | `BuildPhase` actor-side executor gate | **OUTSIDE** | A sibling message type with no log-subsystem footprint in either architecture. |
// | `6c26e85f8` | cross-exec restamp clears the retained lines | **CONSTRUCTION** + **CHECKED**(`noCrossExecContamination`, redispatch) | There is no restamp and no in-memory carrier that survives a re-dispatch: the new execution gets a new exec_id, a new session, a new buffer, and a new chunk-key namespace. The contended state — both executions' logs growing concurrently while the superseded session still holds lines — is reachable (`quint-log-service-witness-concurrent-execs`) and the invariant holds over it. |
// | `699ea1692` | cross-exec restamp clears the seal | **CONSTRUCTION** | No seal latch. Completeness is a predicate computed per read from the execution's own lifecycle row and manifest; a prior execution's completeness cannot mute a live execution's stream because the gate consults the *claimed* execution's state, not a shared per-derivation tombstone. The muting state (`noStaleSealOnLiveCarrier`'s subject) is unrepresentable. |
// | `2f9a747d0` | same-exec restamp clears the seal + final-pending mark | **CONSTRUCTION** | Same as above for the seal half. The final-pending mark has no analog (no deferred final). A reconnecting builder opens a fresh session whose admission is recomputed from the DB. |
// | `6aab70b47` | enqueue-failure reap | **CONSTRUCTION** | No flush queue, no enqueue failure, no orphaned entry. The per-stream buffer's lifetime is the gRPC stream's (a panic-safe scopeguard deregisters it on every exit path). |
// | `463090eb7` | one-shot finalized-elsewhere consult | **CONSTRUCTION** | No finalized-elsewhere state to consult and no retained entry to reap. |
// | `f8ce10b8e` | recurring refused-UPSERT reap | **CONSTRUCTION** | No UPSERT to refuse. The manifest is INSERT-only with `ON CONFLICT DO NOTHING` on a per-attempt key. |
// | `81824cfbb` | periodic sealed-empty reap | **CONSTRUCTION** | No sealed entry. |
// | `84ac79b84` | stale-request guard atomic with the drain | **CONSTRUCTION** | No `flush_final` and no drain of state another actor can restamp. The cut's drain and commit operate on the cutting session's own buffer under its own mutex; the await-granularity window between them is the seam invariant (`buffer`/`in_flight`/manifest), pinned by the `subscribe_during_in_flight_cut_sees_drained_lines` unit test rather than the model (the same whole-action-atomicity boundary the predecessor drew). |
// | `3ce5d03e4` | deferred-final tenure pin | **CONSTRUCTION** | The harm — a stale writer overwriting the row a fresher writer extended — requires a writer that overwrites. Chunks are write-once, the manifest is INSERT-only, and there is no deferred request that outlives the authority that enqueued it. |
// | `646022e37` | tenure pin hoisted before the destructive arms | **CONSTRUCTION** | No destructive arms and no tenure to pin against. |
// | `825cdd478` | atomic tenure-drop reap + post-await re-validation | **CONSTRUCTION** | No tenure-drop reap. |
// | `2c301438d` | disconnect-time discard gated on an authoritative DAG | **CHECKED**(`noSilentLineLoss`, all four regimes) | The one undisclosed-loss bug of the old design. The new design's disconnect paths either *drain* the buffer (the clean half-close cuts every remaining run) or *drop* it (the abort path) — and every dropped line is still in the builder's ack-trimmed retransmit buffer, because an ack is sent only after the manifest INSERT commits. The four-disjunct conservation law holds over every regime's full state space, including the abort-then-replay and abandon-with-unacked paths (`quint-log-service-witness-abandoned` pins the disclosure channel as exercised). `ackImpliesDurable` is the no-early-ack refinement that keeps disjunct 1 honest. |
// | `e6c18add2` | acquisition-time stale-buffer sweep | **CONSTRUCTION** | No acquisition and no stale retained buffer to sweep. |
// | `8e97c2220` | sweep spares only non-terminal DAG entries | **CHECKED**(`noSilentLineLoss` + `ackImpliesDurable`, sweep) | The old sweep's hazard was deleting an entry whose data was still needed. The new TTL sweep's only guard is wall-clock age; the model checks that only an expired execution's chunks are ever deleted, that the deletion is disclosed (the log stops reading complete the moment its manifest or lifecycle row goes), and that the chunks-before-execution-row deletion order's crash window violates nothing. `quint-log-service-witness-swept` pins the deletion as reachable. |
// | `abfade4d5` | periodic frozen-row UPSERT latch | **CONSTRUCTION** | `is_complete` is not stored; it is computed per read from monotone inputs (a terminal status never un-sets, the manifest only grows until the TTL sweep). A downgrade is unrepresentable in the transition relation — no action removes a chunk or a count except the sweep, whose deletion is the retention policy, not a regression. |
// | `44905298b` | `flush_final` already-finalized consult | **CHECKED**(`completenessGate`, base) | No finalizing write to race; the one decide-once datum (`final_line_count`) is stamped by a single-threaded actor and a duplicate stamp writes the same value. The closest surviving analog is the open-time seal — a complete log rejects further `AppendLog` opens — checked by `openAdmitted`'s `not(logIsComplete)` conjunct and pinned reachable by `quint-log-service-witness-complete-open-rejected`. |
// | `d8944727e` | gap-merge fold counts the gap, not the marker | **CHECKED**(`completeLogServesAllProduced`, base) | No gap-merge fold. A forward gap splits the cut into two chunks and is *visible in the manifest* as non-contiguous rows; the completeness fold (`gate.rs::manifest_covers_contiguously`) reports it as incomplete instead of counting it into a span. The gapped manifest is reachable (`quint-log-service-witness-gapped-manifest`) and a log never reads complete across a gap. |
// | `effefb0a1` | flush span = line-number span, not physical count | **CHECKED**(`servedSpanExact`, base + resend) | A chunk's recorded `line_count` is the physical length of the contiguous run it was cut from, so its line-number span and its physical count are equal by construction — the two quantities the old fix had to reconcile cannot diverge. `servedSpanExact` checks the read path serves exactly the recorded intervals. |
// | `e1fd179b9` | stored-coverage reconcile folds any non-subsumed row | **CONSTRUCTION** | No reconcile and no fold of a stored row into a new write. Coverage is the union of INSERT-only manifest rows; the interim-leader-extended-row scenario maps to "session B committed chunks while session A was partitioned", which the read path unions instead of overwriting. |
// | `e10deb9f6` | `req_in_tenure` keyed on the acquire-epoch | **CONSTRUCTION** | No tenure and no deferred request to validate against one. (Model B's calibration entry #1; model B is cancelled.) |
//
// **Summary.** Of the 27 calibration rows covering the 22 historical bugs:
// **18 are closed by construction** — the state they lived in (the shared
// ring entry, the seal latch, the finalizing write, the four orphan shapes,
// the lease-coupled flusher) does not exist, and the model's transition
// relation cannot express the harmful write; **7 are checked by an
// invariant** over an exhaustive regime whose contended scenario a witness
// check pins as reachable (the re-delivery overlap, the foreign-writer
// rejection, the disconnect-time loss, the sweep's deletion scope, the
// finalize-once seal, the gap accounting, and the span arithmetic); **2
// are outside the model** in both architectures (display-stream and
// sibling-message-type gates owned by conventional tests). No row is
// exposed without a check.
//
//
// BUGHUNT-WAVE B2 ROSTER (all 10 findings, close-out modes, verbatim):
// | Finding | Mode | Where it closed |
// |---|---|---|
// | merged_bug_207 (session-scoped caps) | S+R | `GateOk`-only construction + durable accounts (commit 1); displaced-driver cancel rider; `acceptedWithinCap` + `logServiceCalibSessionCaps` |
// | merged_bug_101 (latest-assignment displacement) | S | claimed-exec own-row gate v2 + the kind-mix plane above; `logServiceCalibGateRow` + `logServiceCalibLatestOnly` |
// | merged_bug_111 (unpinned TailLog → latest exec) | S | `latest_build_exec` view (the view half); the query-filter direction is the `log-no-raw-latest-exec` policy check + DB test — none-sensible for the model (a SQL projection, no protocol content) |
// | merged_bug_360 (loss-counter holes) | S | `AbandonReason` lattice + single `disclose()` chokepoint + `LossGuard`; the loss-conservation plane above |
// | bug_248 (mid-stream permanent rejections) | S | `SessionEnd::PermanentReject` classification (rides the disclosure lattice) |
// | bug_098 (write/read bound asymmetry) | S | kernel-owned `MAX_CHUNK_PAYLOAD_BYTES` + `bounded_contiguous_prefix_len` (kani contract + fuzz round-trip; byte domain — none-sensible for the line-integer model, priced in the header) |
// | merged_bug_086 (unguarded TTL delete) | S | kernel `exec_row_sweep_eligible` + scheduler-owned `gc_exec_rows`; the sweep-ownership plane above |
// | bug_290 (TailLog unauthenticated) | S+R | per-method `CredentialClass` table + tenant-JWT + ownership (A6); authz topology is tests + golden + VM — none-sensible for this model (trust boundary sits below the modeled writer identity, see the header) |
// | bug_255 (parent-dir fsync) | R | `durable_put` sequence-spy test; POSIX crash semantics — none-sensible (recorded per directive 2) |
// | bug_131 (bitmask-as-clamp) | R | `semaphore_permits` saturating helper; pure arithmetic with a unit table — none-sensible |
//
//
// THE UNTIMED AXIS — B1'S OWNING RATIONALE (§4.R8, merged_bug_119,
// verbatim; this model stays deliberately untimed):
// The wave-wide ruling (`§4.R8`) and its owning record: **no workstream
// models the timed axis of the log service in this wave**, and
// `logService.qnt` stays deliberately UNTIMED — cuts are atomic, there
// is no drain-deadline clock and no heartbeat-staleness clock in the
// model's alphabet. Three reasons, priced once here. (1) The defects the
// timed axis would catch were closed STRUCTURALLY, below the timer:
// merged_bug_119's StaleBuffer mis-fire was a predicate bug (multi-run
// buffers conflated; the backstop now gates on `failures > 0`), not a
// timing bug — its pin is the deterministic test pair
// `parked_cut_does_not_starve_abort` (a BlockingPutStore park must not
// starve the abort past ~3-4 cut intervals, heartbeat observed running)
// + `paced_multirun_drain_never_trips_stale_buffer` (the recorded red:
// fired on tick 2-3 pre-fix). (2) A timed model re-derives tokio/timer
// semantics rather than protocol content — the same restatement trap as
// modeling `bounded()` (B1's transport none-sensible record, retry map).
// (3) The B3 fork's gate-caught snapshot-ordering race sits BELOW even
// the cut-atomicity floor — the honest net for sub-atomic interleavings
// is the unit suite, recorded there. Re-open trigger: if a future
// defect's mechanism is irreducibly a clock interleaving (not a
// predicate over observed state), the timed axis graduates from
// none-sensible to a funded model extension; until then every timer
// behavior is pinned by deterministic tests with paused time.
//
//
// B3 NONE-SENSIBLE RECORDS (roster legs with no model plane, verbatim):
// - **merged_bug_051 (peer-URL template)** — deployment topology, not
//   protocol state: the fail-closed empty default, the `{pod}`
//   placeholder validation, and the IPv6 bracket rule are pinned by the
//   config validate unit table and helm fragment 30 (byte-equality
//   against values.yaml's sole default + null→empty env). A model
//   dimension for "which URL template the operator rendered" would
//   restate the fragment.
// - **merged_bug_306 / bug_311 (dashboard + CLI rendering)** — the
//   rendering layer is pinned by the vitest battery (the verbatim
//   case-table mirror in `lineCursor.test.ts`, the follow battery, the
//   gap-row/banner component tests) and the `vm-dashboard-k3s` live-tail
//   subtest; the server-side disclosure those renderers consume is what
//   `logServiceServed` verifies. The model's claim boundary is the wire:
//   what arrives; the pixels are the frontend suites' jurisdiction.
//
// DELIBERATE NON-DELTA (gw-resync workstream order, relocated):
// logService.qnt gains no builder-vanish / non-reading-client actions —
// hung awaits are invisible to untimed models; the streaming-open-ban
// lint, the keepalive chokepoint, and the timed conformance tests are
// the binding artifacts there (this also keeps the file collision-free
// with log-serve's extension).
// ===========================================================================
module logService {
  // --- 1. types ---------------------------------------------------------

  /// `drv_executions.final_line_count` (and, by the column's write
  /// contract, the terminal status that accompanies it): `NoCount`
  /// until the scheduler stamps the lifecycle row terminal with a
  /// known builder-reported count. SQL NULL-status and "no terminal
  /// status yet" collapse into `NoCount`; `StampedNoCount` is the
  /// assignment-close stamp (sched.db.exec-stamp-on-close): status
  /// terminal, count NULL — the no-report path's verdict. Terminal for
  /// the sweep (`!= NoCount`), countless for the completeness gate
  /// (no ceiling, never reads complete) — exactly the SQL row
  /// `(status='failed', final_line_count=NULL)`.
  type CountOpt = | NoCount | StampedNoCount | Count(int)

  /// The completeness ceiling as one ingest session knows it
  /// (`IngestSession::final_line_count`). `NoCeiling` until the session
  /// learns the execution's recorded end (at open for an
  /// already-terminal execution, at a heartbeat refresh for a seal that
  /// lands mid-stream). `hwAtLearn` is the session's high-water mark at
  /// the moment it learned the value: accepted lines >= hwAtLearn were
  /// accepted under the ceiling and `completenessGate` requires them to
  /// be below it; accepted lines < hwAtLearn are the disclosed
  /// pre-refresh residual.
  type CeilingState = | NoCeiling | Ceiling({ value: int, hwAtLearn: int })

  /// One ingest session's stream phase.
  ///   - `Open`: the builder's gRPC stream is connected; batches are
  ///     accepted, chunks are cut, acks are deliverable.
  ///   - `Detached`: the builder is gone (clean half-close, transport
  ///     error, or a server-side abort). No more accepts, no more acks.
  ///     A detached session with a non-empty buffer is the driver's
  ///     final drain still cutting (`AppendDriver::drain`) — or a
  ///     deposed same-execution predecessor still committing chunks
  ///     that overlap its successor's. A detached session whose buffer
  ///     was dropped (the abort path) is inert.
  type Phase = | SessOpen | SessDetached

  /// One `AppendLog` ingest session (`ingest.rs::IngestSession` +
  /// `IngestShared`, with `buffer` and `in_flight` collapsed into one
  /// set — a failed cut restores the staged run exactly, so the
  /// staged-but-uncommitted state is observationally the buffer).
  /// `NoSession` = never opened. Fields:
  ///   - `highWater` — `IngestSession::high_water_line`: the lowest
  ///     acceptable `first_line_number` for the next batch. Raised to
  ///     every accepted batch's (post-truncation) end; never lowered;
  ///     not reset by a cut or by the buffer emptying.
  ///   - `ceiling` — `IngestSession::final_line_count` plus the
  ///     learn-time high-water mark.
  ///   - `buf` — `IngestShared::{buffer, in_flight}`: accepted lines
  ///     not yet in a committed chunk.
  type SessionSlot =
    | NoSession
    | Session({
        phase: Phase,
        highWater: int,
        ceiling: CeilingState,
        buf: Set[int],
      })

  /// One committed log chunk's manifest row (`drv_log_chunks`): the
  /// cutting session, and the contiguous run `[first, first + count)`
  /// the object holds. Write-once; the set of chunks per execution only
  /// grows (ingest) or is emptied wholesale (the TTL sweep).
  type Chunk = { sess: int, first: int, count: int }

  /// The execution's `drv_executions.attempt_kind` (bughunt-wave B2):
  /// the open gate admits `KBuild` only — a materialization execution
  /// never holds an AppendLog stream, and its taking the
  /// latest-assignment slot must not revoke a build execution's
  /// replay (merged_bug_101's displacement class, the kind half).
  type Kind = | KBuild | KMat

  /// The per-build-attempt uploader's terminal state
  /// (`log_upload.rs::DrainStatus` x the `disclose()` chokepoint).
  /// `Uploading` covers the whole live lifecycle (streaming,
  /// reconnecting, draining). `Abandoned({ counted })` covers the
  /// drain-deadline expiry, the permanent store rejection, and the
  /// panic guard — every arm routes through `disclose()`, which
  /// increments `rio_builder_log_drain_abandoned_total` iff real lines
  /// were lost; `counted: false` therefore CLAIMS the abandonment had
  /// no un-stored lines (`ingestLossCounted` checks that claim; the
  /// pre-fix rejected:true suppression that skipped the counter is
  /// `CALIB_LOSS_ESCAPE`).
  type Uploader = | Uploading | Abandoned({ counted: bool })

  // --- 2. constants (const = overridable per regime) --------------------

  /// Executions minted for the one modeled derivation. Two are required
  /// for the supersession scenarios: the auth gate's
  /// not-the-latest-assignment rejection, the superseded-but-still-open
  /// session writing to its own (old) execution's log, and the
  /// cross-execution contamination invariant's contended state.
  const MAX_EXECS: int
  /// Ingest sessions per execution. Two are required for the
  /// re-sent-batch-after-ambiguous-disconnect case: a chunk is
  /// committed, the ack is lost, the builder replays the same lines to
  /// a fresh session, and the read path must dedup the overlap.
  const MAX_SESSIONS: int
  /// Worker line numbers range over 0..MAX_LINE-1. Three are required:
  /// an overlap that is not total ([0,2) vs [0,3)), a forward gap
  /// ([0,1) + [2,1)), and a batch that straddles the completeness
  /// ceiling are the shapes the dedup fold, the completeness fold, and
  /// the per-append ceiling are about.
  const MAX_LINE: int
  /// Fault/adversary switches. Each gates one branch of `step`; the
  /// regimes enable them in named combinations.
  ///
  /// The bound builder sends batches that are not its uploader's honest
  /// in-order replay: arbitrary forward gaps, re-sends of already-cut
  /// ranges, and lines at or past the recorded final_line_count. This
  /// is what makes the past-final rejection, the straddling-batch
  /// truncation, and the gap-fragmented manifest reachable.
  const ENABLE_FABRICATION: bool
  /// The builder can disconnect and reopen a fresh session (the
  /// at-least-once re-send shape). Off in the base regime so its state
  /// space stays the single-writer core.
  const ENABLE_RECONNECT: bool

  /// The per-execution accepted-line cap — the model-side collapse of
  /// the durable byte + chunk caps (`store.log.caps-durable`; one line
  /// = one accounted unit). Bound it ABOVE the reachable accept volume
  /// to leave a regime's exploration untouched, or AT `MAX_LINE` to
  /// exercise the boundary (the resend regime does — that is where
  /// reconnects could historically reset the account).
  const CAP: int

  /// CALIBRATION ONLY (`logServiceCalibSessionCaps`): re-encode the
  /// pre-fix per-SESSION cap accounting — every `openSession` resets
  /// the accepted account to zero, exactly merged_bug_207's reconnect
  /// hole. Production regimes bind false; the calibration check pins
  /// that `acceptedWithinCap` falsifies under it.
  const CALIB_SESSION_SCOPED_CAPS: bool

  /// CALIBRATION ONLY (`logServiceCalibUnclampedAck`): re-encode the
  /// pre-merged_bug_005 ack producer — `deliverAck` sends the committed
  /// chunk's own end, never clamped to the contiguous durable frontier
  /// (the hole-spanning wire value the clamp removed). Production
  /// regimes bind false; the calibration check pins that
  /// `ackImpliesDurable` falsifies under it (the bughunt-13 F9 twin).
  const CALIB_UNCLAMPED_ACK: bool

  /// CALIBRATION ONLY (`logServiceCalibGateRow`): drop the
  /// assignment-row conjunct from the open gate — the pre-fix shape
  /// where admission never consulted the claimed exec's own row.
  /// Production regimes bind false; the calibration check pins that
  /// `authGateExcludesUnassignedWriters` falsifies under it.
  const CALIB_GATE_IGNORES_ROW: bool
  /// Executions can expire and the TTL sweep can run.
  const ENABLE_SWEEP: bool

  /// A materialization attempt can be dispatched into the
  /// latest-assignment slot (`dispatchMaterialization`). Off in the
  /// regimes that predate the kind axis — their state spaces carry
  /// `execKind` as a constant-`KBuild` map and are unchanged.
  const ENABLE_MAT_DISPATCH: bool
  /// Upper bound on BUILD-kind dispatches (`dispatch`). The regimes
  /// that predate the kind axis bind it to MAX_EXECS (no behavior
  /// change); the kind-mix regime binds 1 so its second slot is
  /// exclusively the materialization attempt — without the bound the
  /// regime explores the two-build-execs x two-sessions cross product
  /// (the redispatch and resend subjects multiplied; measured past
  /// 300M distinct states), and the displacement scenario needs
  /// exactly one build.
  const MAX_BUILD_EXECS: int

  /// CALIBRATION ONLY (`logServiceCalibLatestOnly`): re-encode the
  /// pre-fix latest-assignment-only open gate — admission requires
  /// `e == dispatched`, so any later dispatch (a re-attempt or a
  /// materialization mint) silently revokes a terminal-but-incomplete
  /// build's late replay (merged_bug_101's displacement hole).
  /// Production regimes bind false; the calibration check pins that
  /// `buildTailAdmittable` falsifies under it.
  const CALIB_GATE_LATEST_ONLY: bool

  /// CALIBRATION ONLY (`logServiceCalibLossEscape`): the pre-fix
  /// rejected:true suppression — the permanent-rejection abandonment
  /// discards the retransmit buffer WITHOUT routing through the
  /// disclosure counter (merged_bug_360's PERMISSION_DENIED hole).
  /// Production regimes bind false; the calibration check pins that
  /// `ingestLossCounted` falsifies under it.
  const CALIB_LOSS_ESCAPE: bool

  /// CALIBRATION ONLY (`logServiceCalibGcRowIgnoresArtifacts`): the
  /// execution-row GC WITHOUT the artifact-before-row conjuncts
  /// (merged_bug_007 pre-fix; store.log.sweep-ownership+2) — the row
  /// deletes while `drv_log_chunks` rows or a live ingest session
  /// still key on it, orphaning the artifacts forever
  /// (`noOrphanLogChunks` falsifies).
  const CALIB_GC_ROW_IGNORE_ARTIFACTS: bool

  /// CALIBRATION ONLY (`logServiceCalibCloseNoStamp`): the pre-fix
  /// assignment close that stamps NOTHING (bug_047) — `closeExecStamp`
  /// degrades to a no-op, the no-report execution's row keeps
  /// `NoCount` forever, `execRowSweepEligible` never satisfies, and
  /// the composed `closeNoStampImmortalRun` demonstrates the immortal
  /// row.
  const CALIB_CLOSE_NO_STAMP: bool

  /// CALIBRATION ONLY (`logServiceCalibSweepAgeOnly`): the pre-fix
  /// age-only sweep SELECT — eligibility ignores the terminal and
  /// ledger-reference conjuncts (merged_bug_086). Production regimes
  /// bind false; the calibration check pins that
  /// `sweepOnlyTerminalUnreferenced` falsifies under it.
  const CALIB_SWEEP_AGE_ONLY: bool

  /// The served-stream (reader) plane: a live TailLog subscriber over
  /// execution 1's log — the seam where merged_bug_180/306/311 lived
  /// (fan-out drops rendered as seamless splices). Disabled (false) in
  /// every pre-existing regime: the reader actions are guard-disabled
  /// and the reader vars stay at their init values, so those state
  /// spaces are unchanged (re-measured byte-identical at the
  /// introduction).
  const ENABLE_READER: bool

  /// The live fan-out queue's capacity (`with_tail_subscriber_queue`).
  /// The adversarial branch: a delivery arriving at a full queue is
  /// DROPPED (`try_send` Full) — the reader plane exists to prove the
  /// drop is repaired, never silently spliced. 0 when the reader is
  /// disabled.
  const QUEUE_CAP: int

  /// CALIBRATION ONLY (`logServiceCalibReaderAdvance`): the pre-fix
  /// reader — a delivered line that jumps past the cursor advances it
  /// WITHOUT back-filling from the manifest/buffers and the final
  /// stamp performs no catch-up (the silent-splice encoding of
  /// merged_bug_180's gateway/store seam and merged_bug_306/311's
  /// frozen one-shot consumers). Production regimes bind false; the
  /// calibration check pins that `servedStreamNoSilentLoss` falsifies
  /// under it.
  const CALIB_READER_ADVANCE_PAST: bool

  // --- bughunt-2 slot-6 served-stream delta consts ----------------------
  // (bug_298 / merged_bug_187 / merged_bug_150 / merged_bug_002 /
  // merged_bug_063's commit-mid-serve seam.) Const-flag pattern: the
  // four pre-existing base regimes bind every flag false and
  // MAX_BATCH_LINES = MAX_LINE (the bound can never bite in the
  // bounded domain), so their state spaces are unchanged — re-measured
  // byte-identical at introduction.

  /// Enables `appendWorkerGap`: the HONEST uploader's stream skips a
  /// span (suppression, rate-drop) — the admitted-hole source for the
  /// served plane, distinct from hostile fabrication.
  const ENABLE_WORKER_GAP: bool
  /// Enables `readerWakeOnDrop`: the drop-epoch latch wake — a fan-out
  /// drop arms `droppedPending`, and the wake back-fills from held
  /// state (d02563663's watch::Sender latch; "latch, don't pulse").
  const ENABLE_DROP_WAKE: bool
  /// Enables the execution axis: `execSwitchDelivery` retags the live
  /// stream with a new execution (a retry on another worker), and
  /// `readerDrain` must take the switch arm (merged_bug_002).
  const ENABLE_EXEC_AXIS: bool
  /// Per-batch admission bound (`rio_log_kernel::MAX_BATCH_LINES`,
  /// bug_298): `acceptVerdict` rejects a batch of more lines than
  /// this, per-batch, stream open. Base regimes bind it to MAX_LINE
  /// (unreachable); the flood regime binds 1 so the arm bites.
  const MAX_BATCH_LINES: int
  /// CALIBRATION: the oversized arm is skipped — an over-bound batch
  /// is admitted whole (`batchLinesBounded` twin).
  const CALIB_OVERSIZED_ADMITTED: bool
  /// CALIBRATION: the drop wake fires WITHOUT the latch — recovery
  /// runs on admitted holes too, the merged_bug_187 worker
  /// amplification (`recoveryOnlyOnDrop` twin).
  const CALIB_RECOVERY_UNGATED: bool
  /// CALIBRATION: `readerStamp` claims the store's completeness
  /// verbatim without checking the catch-up actually covered — the
  /// cursor-blind stamp (merged_bug_063;
  /// `servedStreamNoSilentLoss` twin over the commit-mid-serve seam).
  const CALIB_STAMP_SKIPS_CATCHUP: bool
  /// CALIBRATION: a foreign-execution delivery is drained through the
  /// OLD cursor — the new numbering's low lines vanish as
  /// "duplicates" (`noCrossExecRowInterleave` twin, merged_bug_002).
  const CALIB_EXEC_SPLICE: bool
  /// CALIBRATION: the drop wake is LOST (the pulse-not-latch failure
  /// mode) — `readerWakeOnDrop` never fires even armed
  /// (`servedStreamNoSilentLoss` twin leg two, with the stamp skip).
  const CALIB_DROP_WAKE_LOST: bool
  /// CALIBRATION: `readerCatchup` advances the cursor to the mark
  /// WITHOUT serving the held back-fill (`resumeCursorSound` twin,
  /// merged_bug_205's finalize-at-gap inverse).
  const CALIB_RESUME_PAST_GAP: bool
  /// Enables the PRODUCER plane (bughunt2-wave slot 8, bug_241): the
  /// uploader can die EARLY (uploaderDies -- panic/abort while the
  /// build still produces; the channel closes, subsequent sends
  /// bounce), the channel-refusal watermark (refusedFrom) is stamped,
  /// and the build-exit Drop discloses every refused line through the
  /// DiscardLedger (producerCountedBelow advances at buildFinishes).
  /// Mirrors rio-builder/src/log_upload.rs UploadSink{Open,Lost}:
  /// the ONLY path that drops a batch is the ledger method, and Drop
  /// routes through disclose() as uploader_dead. False in every
  /// pre-existing regime: all three new coordinates freeze and the
  /// state spaces stay byte-identical (re-measured at the introducing
  /// commit).
  const ENABLE_PRODUCER_PLANE: bool
  /// Calibration: the pre-fix producer -- the build-exit Drop does
  /// NOT disclose (no DiscardLedger; the bounced batches simply
  /// vanish). producerLossCounted falsifies under it -- the bug_241
  /// red (zero counter increments on post-death discards) as a model
  /// violation. House-pattern note (lease campaign rationale (e),
  /// relocated): the successor brief sketched a separate
  /// calibration/log-241-producer-blind.qnt override file; this
  /// model's calibration convention is CALIB_* consts with in-file
  /// regime modules (22 prior instances), which also avoids
  /// re-framing 30+ actions in an override file —
  /// logServiceCalibProducerBlind follows the house pattern.
  const CALIB_PRODUCER_BLIND: bool

  // --- 3. pure helpers --------------------------------------------------

  pure def maxInt(a: int, b: int): int = if (a > b) a else b
  pure def minInt(a: int, b: int): int = if (a < b) a else b

  /// The lowest element of a non-empty set (MAX_LINE for the empty set —
  /// callers guard on non-emptiness).
  pure def setMin(s: Set[int]): int = s.fold(MAX_LINE, (a, b) => minInt(a, b))

  /// The longest contiguous run starting at `from` within `s`
  /// (`ingest.rs::contiguous_prefix_len` applied to the buffer's sorted
  /// order): every line `from..from+k-1` is in `s` and `from+k` is not.
  /// Encoded as a left fold over the ORDERED bounded line domain rather
  /// than a recursion (Quint has no general recursion, and `Set.fold`'s
  /// iteration order is unspecified — every ordered walk in this model
  /// folds over a `range(...)` list).
  pure def runFrom(s: Set[int], from: int): Set[int] =
    range(from, MAX_LINE).foldl(Set(), (acc, n) =>
      if (n == from and s.contains(n)) acc.union(Set(n))
      else if (acc.contains(n - 1) and s.contains(n)) acc.union(Set(n))
      else acc)

  /// The half-open interval `[lo, hi)` as a set ([] when lo >= hi).
  pure def interval(lo: int, hi: int): Set[int] =
    if (lo >= hi) Set() else lo.to(hi - 1)

  /// The lines a chunk's manifest row claims (and, by the cut's
  /// construction, physically holds).
  pure def chunkLines(c: Chunk): Set[int] = interval(c.first, c.first + c.count)

  // --- 4. state variables -----------------------------------------------

  /// Executions minted so far (1-based; execution N is the N-th
  /// assignment attempt for the derivation, and the LATEST attempt is
  /// execution `dispatched`). `assignments` rows are never deleted, so
  /// the auth gate's latest-assignment lookup reads this directly.
  var dispatched: int

  /// GHOST, per execution: one past the highest line the build has
  /// emitted into its uploader — `delivered(e)` is `[0, producedEnd(e))`.
  /// The conservation-law subject. The build emits lines contiguously
  /// from 0 (`log_stream.rs` numbers lines monotonically per execution).
  var producedEnd: int -> int
  /// Per execution: the build process has exited and its completion
  /// report (carrying `final_line_count = producedEnd`) is ready. No
  /// more lines are produced; the uploader's drain deadline is armed.
  var buildFinished: int -> bool
  /// Per execution: `drv_executions.{status, final_line_count}` — `Count`
  /// iff the scheduler has stamped the row terminal with a known count.
  /// A terminal row with an unknown count (a cancellation, the 0->NULL
  /// mapping, a lost completion report) is observationally identical to
  /// a non-terminal row for every guard and every invariant in this
  /// subsystem (`gate.rs::sealed_final_line_count` returns None for
  /// both), so the two are not distinguished.
  var dbFinalCount: int -> CountOpt
  /// Per execution: the `drv_executions` row exists (INSERTed at
  /// dispatch, DELETEd by the TTL sweep's last statement).
  var execRowExists: int -> bool
  /// Per execution: `started_at < now() - retention` — the TTL sweep's
  /// guard. Set by the environment, monotone.
  var expired: int -> bool

  /// Has the scheduler's ON-CONFLICT in-place rewrite erased this
  /// (non-latest) execution's assignment row? The v2 write-authority
  /// gate keys on the claimed exec's OWN row — supersession by a newer
  /// attempt no longer revokes it (merged_bug_101), but a rewritten
  /// row does, permanently.
  var assignmentRowLost: int -> bool

  /// Ghost: lines accepted for the execution over its LIFETIME (all
  /// sessions; never reset on reconnect — that is the
  /// `store.log.caps-durable` fix). The accept guard charges it; the
  /// open gate refuses an at-cap execution.
  var acceptedTotal: int -> int
  /// Per execution: the store's TTL pass has stripped this execution
  /// (chunks + session registry). Mirrors the SELECT's EXISTS
  /// disjunction keeping a stripped execution out of later passes.
  var sweepChunksDone: int -> bool

  /// Per execution: `drv_executions.attempt_kind`, stamped at dispatch
  /// and immutable (the dispatch INSERT writes it once).
  var execKind: int -> Kind
  /// Per execution: the scheduler's attempt ledger still references
  /// this execution (`drv_attempts` rows whose exec joins here — the
  /// retry kernel's decision suffix). Set at dispatch (the attempt
  /// INSERT), released by the environment when the ledger GC's own
  /// per-lane cut moves past it (`ledgerReleases`); the v2 sweep
  /// refuses to delete a referenced row. Frozen once the row is gone
  /// (a post-deletion release changes nothing this subsystem reads),
  /// which is what makes `sweepOnlyTerminalUnreferenced` checkable
  /// from post-states.
  var ledgerReferenced: int -> bool

  /// Per execution: the uploader's lifecycle.
  var uploader: int -> Uploader
  /// Per execution: the channel-refusal watermark -- one past the
  /// highest line the uploader ever accepted; -1 while the uploader
  /// lives (or died never having refused). Stamped by uploaderDies at
  /// the death instant: every line produced past it bounces off the
  /// closed channel into the DiscardLedger. Frozen -1 outside the
  /// producer plane.
  var refusedFrom: int -> int
  /// Per execution: one past the highest produced line the producer
  /// ledger has DISCLOSED as refused (the Drop's disclose(uploader_dead)
  /// at build exit); 0 = nothing disclosed. Covers [refusedFrom, .)
  /// when a refusal exists -- disjoint by construction from the
  /// drain-deadline counter, which owns [ackedBelow, refusedFrom).
  /// Frozen 0 outside the producer plane.
  var producerCountedBelow: int -> int
  /// Per execution: one past the highest line covered by an ack the
  /// builder has received (`UploadTask::last_acked_line + 1`). Monotone.
  /// The retransmit buffer is `[ackedBelow, producedEnd)` while
  /// `Uploading`.
  var ackedBelow: int -> int
  /// Per execution: one past the highest line the uploader has
  /// transmitted on its CURRENT session (`UploadTask::drive`'s `sent`
  /// index, projected onto line numbers). Reset to `ackedBelow` on every
  /// reconnect — that reset IS the at-least-once retransmit.
  var sentBelow: int -> int

  /// Per (execution, session index): the ingest sessions. Session
  /// indices are mint-ordered (UUIDv7), which is the read path's
  /// same-first-line tiebreak order.
  var sessions: int -> (int -> SessionSlot)
  /// Per execution: how many sessions have ever been opened (the next
  /// session index is `sessionsOpened + 1`).
  var sessionsOpened: int -> int
  /// Per execution: the committed chunk manifest (`drv_log_chunks`).
  var chunks: int -> Set[Chunk]
  /// GHOST, per execution: lines accepted into one of the execution's
  /// sessions from a batch that was NOT the uploader's honest in-order
  /// replay of the build's real output. `noCrossExecContamination`
  /// bounds every chunk's lines by `delivered union fabricated` — the
  /// set of lines the execution's own bound builder ever sent.
  var fabricated: int -> Set[int]

  /// History flags (model-only bookkeeping, never read by an action
  /// guard; each is the discriminator for one expect-violation witness).
  ///   - `supersededOpenRejected`: the auth gate rejected a stream open
  ///     whose claimed execution is no longer the latest assignment.
  ///   - `completeOpenRejected`: the gate rejected a stream open because
  ///     the execution's log is already complete.
  ///   - `pastFinalRejected`: the per-append completeness ceiling
  ///     dropped a whole batch or truncated a straddling one.
  ///   - `nonMonotoneRejected`: the monotone floor dropped a batch
  ///     numbered below the session's high-water mark.
  ///   - `sweptChunks`: the TTL sweep deleted a non-empty manifest.
  ///   - `matThenBuildReplayAdmitted`: a build execution's reopen was
  ///     admitted while a LATER materialization attempt held the
  ///     latest-assignment slot — the replay the pre-fix latest-only
  ///     gate displaced.
  var wit: {
    supersededOpenRejected: bool,
    completeOpenRejected: bool,
    pastFinalRejected: bool,
    nonMonotoneRejected: bool,
    sweptChunks: bool,
    matThenBuildReplayAdmitted: bool,
    // slot-6 served-stream delta: workerGapAppended (an honest
    // worker-gap batch landed — the admitted hole is reachable),
    // oversizedRejected (the bound bit), oversizedAdmitted (only the
    // CALIB_OVERSIZED_ADMITTED arm can set it; batchLinesBounded is
    // its negation).
    workerGapAppended: bool,
    oversizedRejected: bool,
    oversizedAdmitted: bool,
    // A closer-stamped (`StampedNoCount`) row was reclaimed by
    // `gcExecRow` — the close-then-sweep liveness path actually
    // completed (bug_047's witness; `noCloseStampedReclaim` must
    // violate in the sweep regime).
    closeStampedReclaimed: bool,
  }

  /// The reader's terminal observation: the serving session's final
  /// message, carrying the cursor it closed at and the store's own
  /// completeness claim (`is_complete`).
  type ReaderFinal = | NoFinal | Final({ cursor: int, complete: bool })

  // --- 4b. served-stream (reader) plane state ---------------------------
  // One live TailLog subscriber over execution 1 (the served regimes
  // bind MAX_EXECS = 1). All vars init-frozen unless ENABLE_READER.

  /// A subscriber is attached (between `readerSubscribe` and
  /// `readerFinalize`).
  var readerAttached: bool
  /// The subscriber's watermark: the next line number it has not yet
  /// rendered (the `since_line` it would reconnect with).
  var readerCursor: int
  /// The set of line numbers actually rendered to the subscriber.
  var readerServed: Set[int]
  /// Ghost: the COUNT of render events. `count == served.size()` is
  /// the at-most-once half of span exactness — a double-render shows
  /// up as count > size because the set deduplicates.
  var readerServedCount: int
  /// The live fan-out queue: accepted lines awaiting delivery to the
  /// subscriber. Bounded by QUEUE_CAP at JOIN time — the overflow is
  /// dropped, not queued (`try_send` Full).
  var pendingFan: Set[int]
  /// The final message, once sent.
  var readerFinal: ReaderFinal
  /// Ghost: lines the full queue dropped (the witness `noFanoutDrop`
  /// pins the drop reachable).
  var fanDropped: Set[int]
  /// Ghost: a delivery jumped past the cursor (the reader OBSERVED a
  /// gap; witness `noReaderGapObserved` pins it reachable).
  var readerJumpObserved: bool
  /// Ghost: a jump was repaired by back-filling from the manifest and
  /// store buffers (witness `noReaderRecovery` pins the repair
  /// reachable).
  var readerRecovered: bool

  // --- slot-6 served-stream delta vars ----------------------------------

  /// The catch-up mark: one past the highest held line at the last
  /// `readerCatchup`. The stamp's completeness honesty is measured
  /// against what landed AFTER this mark (commit-mid-serve).
  var readerManifestMark: int
  /// Between `readerCatchup` and `readerStamp` (the finalize split's
  /// intermediate state — core actions interleave here).
  var readerPendingStamp: bool
  /// COUNT of recovery (back-fill) events — the m187 "counted
  /// backfill". `recoveryOnlyOnDrop` pins it to drop provenance.
  var readerRecoveryEntries: int
  /// The drop-epoch LATCH (d02563663's watch::Sender): armed by a
  /// fan-out drop, consumed by `readerWakeOnDrop`. A pulse would be
  /// lost while the serve loop is parked; the latch survives.
  var droppedPending: bool
  /// The execution whose numbering the reader's cursor lives in
  /// (exec-axis regimes; an abstract 0/1 tag).
  var readerExec: int
  /// The execution tag on the live stream's next deliveries.
  var deliveryExec: int
  /// Ghost: a foreign-execution row was drained through the old
  /// cursor and swallowed (only the CALIB_EXEC_SPLICE arm can set it;
  /// `noCrossExecRowInterleave` = its negation).
  var execSpliced: bool

  // --- 5. derived state -------------------------------------------------

  pure val EXECS = 1.to(MAX_EXECS)
  pure val SESSION_IDS = 1.to(MAX_SESSIONS)

  /// The conservation-law subject: the lines execution `e`'s build has
  /// emitted into its uploader.
  def delivered(e: int): Set[int] = interval(0, producedEnd.get(e))

  /// The uploader has reached a terminal state (any abandonment arm).
  def uploaderAbandoned(e: int): bool = uploader.get(e) != Uploading

  /// The uploader's abandonment routed through the disclosure counter
  /// (`disclose()` with `lost > 0`). False while uploading.
  def uploaderLossCounted(e: int): bool =
    match uploader.get(e) { | Uploading => false | Abandoned(a) => a.counted }

  /// The builder's retransmit buffer: every emitted line not yet covered
  /// by a received ack. Emptied by abandonment (the deadline expiry or
  /// the permanent rejection — `UploadTask::reject_permanently` clears
  /// the deque, and an exited task's heap is gone either way).
  def builderBuf(e: int): Set[int] =
    if (uploaderAbandoned(e)) Set()
    else interval(ackedBelow.get(e), producedEnd.get(e))

  // Session accessors. Quint's effect system requires every branch of a
  // match used in action position to assign the same variables, so the
  // actions read sessions through these (which return safe defaults for
  // `NoSession`) and guard on `hasSession` exactly where the code guards
  // on the registry lookup.
  def hasSession(e: int, s: int): bool = sessions.get(e).get(s) != NoSession
  def sessPhase(e: int, s: int): Phase =
    match sessions.get(e).get(s) { | NoSession => SessDetached | Session(x) => x.phase }
  def sessHw(e: int, s: int): int =
    match sessions.get(e).get(s) { | NoSession => 0 | Session(x) => x.highWater }
  def sessCeiling(e: int, s: int): CeilingState =
    match sessions.get(e).get(s) { | NoSession => NoCeiling | Session(x) => x.ceiling }
  def sessBuf(e: int, s: int): Set[int] =
    match sessions.get(e).get(s) { | NoSession => Set() | Session(x) => x.buf }
  def sessIsOpen(e: int, s: int): bool = hasSession(e, s) and sessPhase(e, s) == SessOpen

  /// The `log_ingest_sessions` row's liveness, as the execution-row
  /// GC's sixth conjunct sees it (F10 — the doc previously named the
  /// WRONG mechanism: the in-memory deregistration scopeguard, which
  /// governs the TailLog-routing registry entry and runs BEFORE the
  /// drain, not this DB row). The TRUE mechanism the code denominates
  /// in: the row exists from `acquire` until `sessions::release`,
  /// which runs AFTER the disconnect drain commits the buffered tail;
  /// the conjunct reads heartbeat STALENESS, and the heartbeat spans
  /// the drain on a dedicated task — respawned for the drain if the
  /// original died (the panic face) — whose cadence the sessions.rs
  /// margin certificate compile-asserts strictly under the staleness
  /// bound. The drain therefore HOLDS the row for as long as it
  /// flushes, which is exactly this predicate's abstraction:
  /// an OPEN session holds its row, and a DETACHED session holds it
  /// while its drain is still flushing (non-empty buffer). The abort
  /// arm wipes the buffer at detach, so an aborted session's row dies
  /// immediately (release runs promptly there) — both arms mirror
  /// service.rs's exit order.
  def registryLive(e: int, s: int): bool = and {
    hasSession(e, s),
    or { sessPhase(e, s) == SessOpen, sessBuf(e, s) != Set() },
  }

  /// The execution's unique Open session index, or 0 if none. The
  /// builder holds one `AppendLog` stream at a time, so at most one
  /// session per execution is ever Open (asserted by `boundsOK`).
  def openSessionOf(e: int): int =
    SESSION_IDS.fold(0, (acc, s) => if (sessIsOpen(e, s)) s else acc)

  /// The chunks cut by session `s` of execution `e`.
  def chunksOf(e: int, s: int): Set[Chunk] = chunks.get(e).filter(c => c.sess == s)

  /// Every line accepted into session `s` of `e` that is still
  /// observable: buffered or committed. A line accepted and then dropped
  /// by an abort before it was cut is gone from this set — and from
  /// every harm an accepted line can do.
  def sessAccepted(e: int, s: int): Set[int] =
    sessBuf(e, s).union(chunksOf(e, s).fold(Set(), (acc, c) => acc.union(chunkLines(c))))

  /// Is line `n` covered by some manifest chunk of `e`?
  def manifestHolds(e: int, n: int): bool =
    chunks.get(e).exists(c => c.first <= n and n < c.first + c.count)

  /// Is line `n` in some session's not-yet-durable ingest buffer for `e`?
  def storeBufHolds(e: int, n: int): bool =
    SESSION_IDS.exists(s => sessBuf(e, s).contains(n))

  /// `rio_retry_kernel::exec_row_sweep_eligible`, projected onto this
  /// model's state (bughunt-wave B2, merged_bug_086): the sweep pass
  /// may target an execution only if its lifecycle row is TERMINAL
  /// (the model's collapsed encoding: a stamped count — see the
  /// `dbFinalCount` doc; a terminal row with an unknown count is
  /// indistinguishable from a non-terminal one here, which only makes
  /// the modeled guard stricter), the attempt ledger no longer
  /// references it, and the retention clock has aged it out. The
  /// kernel's fourth conjunct (no active assignment) is scheduler-side
  /// state this model does not carry; omitting it only widens the
  /// modeled reclaim. Guards `gcExecRow` (the scheduler's row delete);
  /// the store's chunk/session pass carries its own structural
  /// exclusions (`sweepChunks`: terminal + no live registry row,
  /// merged_bug_071) and never touches the row.
  def execRowSweepEligible(e: int): bool = and {
    expired.get(e),
    // CALIBRATION: the pre-fix SELECT was age-only.
    CALIB_SWEEP_AGE_ONLY or and {
      dbFinalCount.get(e) != NoCount,
      not(ledgerReferenced.get(e)),
    },
    // v3 (store.log.sweep-ownership+2, merged_bug_007): artifact-
    // before-row is data-structural — the row outlives every log
    // artifact keyed on it (conjuncts 5/6: NOT EXISTS drv_log_chunks /
    // log_ingest_sessions), under ANY retention configuration.
    // The registry conjunct keys on `registryLive`: open sessions AND
    // detached-but-undrained ones still hold their
    // `log_ingest_sessions` row (release runs after the disconnect
    // drain, and the dedicated drain heartbeat — respawned on beat
    // death, F10 — keeps the staleness conjunct holding it while the
    // drain flushes) — found by this model: keying on open-only
    // admitted a reclaim under an in-flight drain, whose late chunk
    // INSERT (no FK) would orphan.
    CALIB_GC_ROW_IGNORE_ARTIFACTS or and {
      chunks.get(e) == Set(),
      not(SESSION_IDS.exists(s => registryLive(e, s))),
    },
  }

  // -- The completeness predicate (`gate.rs::log_is_complete`) ---------

  /// `gate.rs::manifest_covers_contiguously`: does the manifest, walked
  /// in ascending `first_line` order, cover a contiguous `[0, up_to)`?
  /// Overlap extends coverage (`covered = max(covered, first + count)`);
  /// a chunk starting past the covered-through point is a gap. The walk
  /// order is materialized by folding over the bounded `first_line`
  /// domain; same-first chunks fold through `max` and are
  /// order-insensitive, exactly as the SQL `ORDER BY first_line` (no
  /// session tiebreak) leaves them.
  def manifestCoversUpTo(e: int, upTo: int): bool =
    val covered = range(0, MAX_LINE).foldl(0, (cov, f) =>
      chunks.get(e).filter(c => c.first == f).fold(cov, (cov2, c) =>
        // The early-return-on-gap is encoded by freezing `cov` once a
        // gap is found: a negative sentinel never recovers. The inner
        // fold over same-first chunks is a set fold in arbitrary order;
        // it is order-insensitive because every same-first chunk either
        // trips the same gap test or feeds the same max.
        if (cov2 < 0 or c.first > cov2) -1
        else maxInt(cov2, c.first + c.count)))
    covered >= upTo

  /// `CoverageMap::contiguous_prefix_end` (bug_032): one past the
  /// highest line of the contiguous-from-zero covered prefix of the
  /// committed manifest — the open-time coverage watermark the server
  /// advertises on the open ack and the session's durable floor. Zero
  /// when line 0 is uncovered. Same fold as `manifestCoversUpTo`, but
  /// a gap FREEZES the prefix instead of poisoning it.
  def coveredPrefixEnd(e: int): int =
    val folded = range(0, MAX_LINE).foldl({ cov: 0, stop: false }, (st, f) =>
      chunks.get(e).filter(c => c.first == f).fold(st, (st2, c) =>
        if (st2.stop or c.first > st2.cov) { cov: st2.cov, stop: true }
        else { cov: maxInt(st2.cov, c.first + c.count), stop: st2.stop }))
    folded.cov

  /// Is line `l` inside some committed chunk's interval? The covered
  /// consult's pointwise form — equivalent to the implementation's
  /// one-span containment because `CoverageMap` merges overlapping and
  /// adjacent spans.
  def lineCovered(e: int, l: int): bool =
    chunks.get(e).exists(c => c.first <= l and l < c.first + c.count)

  /// Is the whole half-open `[lo, hi)` durably covered
  /// (`CoverageMap::covers_range`)? Empty ranges are not covered.
  def rangeCovered(e: int, lo: int, hi: int): bool = and {
    lo < hi,
    interval(lo, hi).forall(l => lineCovered(e, l)),
  }

  /// The session after a COVERED-REPLAY drop (merged_bug_002's
  /// write-path arm / the bug_032 floor): the batch is dropped
  /// uncharged and un-buffered — it can mint no chunk and no charge —
  /// but the high water still advances past it (the lines are
  /// accounted for: durable) and the driver acks it from the manifest
  /// truth.
  pure def sessAfterCoveredDrop(slot: SessionSlot, end: int): SessionSlot =
    match slot {
      | NoSession => NoSession
      | Session(x) => Session({
          phase: x.phase,
          highWater: maxInt(x.highWater, end),
          ceiling: x.ceiling,
          buf: x.buf,
        })
    }

  /// `gate.rs::log_is_complete`: terminal with a known count, and the
  /// manifest contiguously covers `[0, count)`. A missing
  /// `drv_executions` row (never dispatched, or swept) means the
  /// execution cannot be judged complete. Computed at read time from
  /// these inputs on every call — never latched.
  def logIsComplete(e: int): bool = and {
    execRowExists.get(e),
    match dbFinalCount.get(e) {
      | NoCount => false
      // Terminal-by-close with no recorded count: completeness is
      // unjudgeable, the log never reads complete (disclosed).
      | StampedNoCount => false
      | Count(n) => manifestCoversUpTo(e, n)
    },
  }

  // -- The read path (`tail.rs`) ---------------------------------------

  /// One step of the `LineCursor` walk: visit chunk `c` with the
  /// watermark at `cursor`, returning the post-visit watermark and the
  /// lines this chunk contributed (`tail.rs::read_chunk`). A chunk
  /// whose last line is below the watermark is skipped without
  /// advancing it; otherwise the chunk yields `[max(first, cursor),
  /// first + count)` and the watermark lands one past its last line.
  pure def visitChunk(cursor: int, c: Chunk): { cursor: int, yielded: Set[int] } =
    if (c.count == 0 or c.first + c.count - 1 < cursor)
      { cursor: cursor, yielded: Set() }
    else
      { cursor: c.first + c.count, yielded: interval(maxInt(c.first, cursor), c.first + c.count) }

  /// The full `TailLog` history read from `since_line = 0`: every chunk
  /// of `e` visited in `(first_line, session_id)` order
  /// (`tail.rs::read_manifest_range`'s ORDER BY), each contributing its
  /// lines above the shared watermark. `count` accumulates the total
  /// number of (line, chunk) yield events so `servedSpanExact` can
  /// assert each line was yielded at most once — the half of the dedup
  /// a set-valued result cannot express.
  def readTail(e: int): { served: Set[int], count: int } =
    val r = range(0, MAX_LINE).foldl({ cursor: 0, served: Set(), count: 0 }, (acc, f) =>
      range(1, MAX_SESSIONS + 1).foldl(acc, (acc2, s) =>
        // At most one chunk per (first_line, session): a session's
        // chunks are disjoint runs, so two of them cannot share a
        // first_line and the inner set fold visits at most one element.
        chunks.get(e).filter(c => c.first == f and c.sess == s).fold(acc2, (acc3, c) =>
          val v = visitChunk(acc3.cursor, c)
          { cursor: v.cursor,
            served: acc3.served.union(v.yielded),
            count: acc3.count + v.yielded.size() })))
    { served: r.served, count: r.count }

  /// What the read path is SUPPOSED to serve: the union of every
  /// manifest chunk's line range (each line once). The independent
  /// truth `servedSpanExact` compares the fold against.
  def manifestUnion(e: int): Set[int] =
    chunks.get(e).fold(Set(), (acc, c) => acc.union(chunkLines(c)))

  // -- The accept predicate (`ingest.rs::IngestSession::accept`) -------

  /// The verdict for a non-empty batch `[lo, hi)` against session
  /// state `(hw, ceiling)`, in the code's check order:
  ///   1. `first_line_number < high_water_line` -> RejectedNonMonotone
  ///      (the whole batch, even the lines above the high water);
  ///   2. ceiling known and `first_line_number >= ceiling` ->
  ///      RejectedPastFinal (the whole batch);
  ///   3. ceiling known and `end > ceiling` -> accepted truncated to
  ///      `[lo, ceiling)`;
  ///   4. otherwise accepted whole.
  /// The overflow arm (check 0 in the code) is unrepresentable in the
  /// bounded line domain. The accepted batch raises the high water to
  /// its (post-truncation) end.
  /// The oversized arm (bug_298, check 2 in the code after the
  /// unrepresentable overflow): a batch of more than MAX_BATCH_LINES
  /// lines is rejected whole BEFORE the monotone/ceiling checks —
  /// per-batch, stream open. `CALIB_OVERSIZED_ADMITTED` skips it (the
  /// pre-fix admission, `batchLinesBounded`'s twin).
  type Verdict = | RejectOversized | RejectNonMonotone | RejectPastFinal | Accept(int)
  pure def acceptVerdictWith(oversizedSkipped: bool, maxBatch: int,
      hw: int, ceiling: CeilingState, lo: int, hi: int): Verdict =
    if (hi - lo > maxBatch and not(oversizedSkipped)) RejectOversized
    else if (lo < hw) RejectNonMonotone
    else
      match ceiling {
        | NoCeiling => Accept(hi)
        | Ceiling(c) =>
          if (lo >= c.value) RejectPastFinal
          else Accept(minInt(hi, c.value))
      }
  def acceptVerdict(hw: int, ceiling: CeilingState, lo: int, hi: int): Verdict =
    acceptVerdictWith(CALIB_OVERSIZED_ADMITTED, MAX_BATCH_LINES, hw, ceiling, lo, hi)

  // --- 7. invariants ----------------------------------------------------
  //
  // The three surviving invariants of the predecessor model
  // (`noCrossExecContamination`, the binding/auth gate, `noSilentLineLoss`)
  // restated over the new state, `lineSpanExact` restated over the
  // manifest + dedup read path as `servedSpanExact`, the completeness
  // gate, and the load-bearing ack-ordering refinement of the
  // conservation law. Each carries the spec rule it verifies; the
  // r[verify ...] markers live at the check wiring points in
  // nix/quint.nix. Defined before the actions (Quint definitions are
  // order-independent).

  /// The runtime residual of TypeOK: the integer ranges the ceiling
  /// preconditions are supposed to enforce, plus the structural facts
  /// the other invariants' encodings rely on (at most one Open session
  /// per execution; session buffers and high-water marks within the
  /// line domain). The acked watermark is deliberately NOT asserted to
  /// trail the transmit cursor: an ack for a fabricated high-numbered
  /// chunk legitimately overtakes the honest replay's cursor (see
  /// `ackImpliesDurable`).
  val boundsOK = and {
    dispatched >= 0 and dispatched <= MAX_EXECS,
    EXECS.forall(e => producedEnd.get(e) >= 0 and producedEnd.get(e) <= MAX_LINE),
    EXECS.forall(e => ackedBelow.get(e) >= 0 and ackedBelow.get(e) <= MAX_LINE),
    EXECS.forall(e => sessionsOpened.get(e) >= 0 and sessionsOpened.get(e) <= MAX_SESSIONS),
    EXECS.forall(e => SESSION_IDS.forall(s =>
      hasSession(e, s) implies and {
        s <= sessionsOpened.get(e),
        sessHw(e, s) >= 0 and sessHw(e, s) <= MAX_LINE,
        sessBuf(e, s).forall(n => n >= 0 and n < MAX_LINE),
      })),
    // At most one Open session per execution: the builder holds one
    // AppendLog stream at a time, and `openSession` requires the
    // previous session to be detached first.
    EXECS.forall(e => SESSION_IDS.forall(s1 => SESSION_IDS.forall(s2 =>
      (sessIsOpen(e, s1) and sessIsOpen(e, s2)) implies s1 == s2))),
    // An execution's lifecycle row exists iff it was dispatched and the
    // sweep has not deleted it.
    EXECS.forall(e => execRowExists.get(e) implies e <= dispatched),
  }

  /// `NoCrossExecContamination` (obs.log.exec-keyed+2,
  /// store.log.append-auth's downstream half): execution E's manifest
  /// holds only lines E's own bound builder sent — its build's real
  /// output plus its own fabrications — never another execution's. The
  /// defense is structural (the chunk key embeds the exec_id the stream
  /// was bound to at open, and a re-dispatch mints a new exec_id and a
  /// new key namespace); this invariant is the model-side regression
  /// guard that the cut action keys its chunk by the cutting session's
  /// execution and nothing else. Its calibration value is the
  /// non-vacuity of the quantifier — the contended state (two
  /// executions of one derivation with concurrently growing logs, the
  /// superseded one still being written by its not-yet-rejected
  /// session) is pinned reachable by `noConcurrentExecWriters`.
  val noCrossExecContamination = EXECS.forall(e =>
    chunks.get(e).forall(c =>
      chunkLines(c).subseteq(delivered(e).union(fabricated.get(e)))))

  /// `BindingGateExcludesUnassignedWriters` (store.log.append-auth+2,
  /// the v2 claimed-exec re-keying): the stream-open predicate admits
  /// a claimed execution only while its OWN assignment row exists — a
  /// rewritten-in-place attempt is permanently excluded. Supersession
  /// alone no longer excludes (merged_bug_101: the builder's own
  /// superseded execution stays writable; containment is exec-keyed
  /// chunks + the durable caps + the ceiling — see
  /// noCrossExecContamination). The invariant falsifies if the
  /// row conjunct is dropped from `openAdmitted`
  /// (`logServiceCalibGateRow` pins exactly that). The
  /// presenter-identity, derivation-match, and kind halves of the gate
  /// are equality checks on signed/scheduler-stamped fields, outside
  /// the model (see the header's scope notes).
  val authGateExcludesUnassignedWriters = EXECS.forall(e =>
    openAdmitted(e) implies not(assignmentRowLost.get(e)))

  /// `store.log.caps-durable`: the durable accepted footprint — every
  /// line the store is holding or has committed for the execution,
  /// across ALL its sessions — never exceeds the per-execution cap.
  /// Measured against the durable state (chunks + buffers), not the
  /// accounting ghost, so the pre-fix per-session encoding (which
  /// resets the GHOST on reconnect; `logServiceCalibSessionCaps`)
  /// falsifies it: a reconnect-reset account admits and accepts past
  /// the cap, and the over-cap lines land in chunks.
  val acceptedWithinCap = EXECS.forall(e =>
    size(
      chunks.get(e).fold(Set(), (acc, c) => acc.union(chunkLines(c)))
        .union(SESSION_IDS.fold(Set(), (acc, s) => acc.union(sessBuf(e, s))))
        .union(fabricated.get(e))
    ) <= CAP)

  /// Witness (negated): some execution actually REACHES the cap
  /// boundary — the cap conjuncts are exercised, not vacuous.
  val noCapBoundary = EXECS.forall(e => acceptedTotal.get(e) < CAP)

  /// `store.log.append-auth+2`'s kind half + the displacement fix
  /// (bughunt-wave B2 item 8, merged_bug_101's kind leg): a
  /// materialization execution is NEVER admitted to AppendLog, and a
  /// build execution whose own row is intact, whose log is incomplete
  /// and unexpired, and whose durable account has headroom IS admitted
  /// — even while a later attempt (in particular a materialization
  /// mint) holds the latest-assignment slot. In the production gate
  /// the second conjunct is the gate's own definition (the calibration
  /// gives it teeth: `CALIB_GATE_LATEST_ONLY` re-adds the pre-fix
  /// `e == dispatched` conjunct and a mat dispatch then falsifies it).
  val buildTailAdmittable = EXECS.forall(e => and {
    execKind.get(e) == KMat implies not(openAdmitted(e)),
    (and {
      e <= dispatched,
      execKind.get(e) == KBuild,
      not(assignmentRowLost.get(e)),
      not(logIsComplete(e)),
      not(expired.get(e)),
      acceptedTotal.get(e) < CAP,
    }) implies openAdmitted(e),
  })

  /// `builder.log.loss-disclosure` at the ingest plane (bughunt-wave
  /// B2 item 8, merged_bug_360): an abandonment that did NOT route a
  /// loss through the disclosure counter must have had nothing real to
  /// lose — every line its build produced is store-held (buffered or
  /// committed). Deliberately WITHOUT `noSilentLineLoss`'s
  /// not-complete and abandoned escapes: the pre-fix rejected:true
  /// suppression (`CALIB_LOSS_ESCAPE`) satisfied those and still
  /// vanished real lines silently — exactly the hole this plane pins.
  val ingestLossCounted = EXECS.forall(e =>
    match uploader.get(e) {
      | Uploading => true
      | Abandoned(a) =>
        not(a.counted) implies
          delivered(e).forall(n => storeBufHolds(e, n) or manifestHolds(e, n))
    })

  /// `builder.log.loss-disclosure` at the PRODUCER plane
  /// (bughunt2-wave slot 8, bug_241): every line the build produced
  /// past the channel-refusal watermark is disclosed by the time the
  /// build exits -- the UploadSink Drop's disclose(uploader_dead) over
  /// the DiscardLedger. The first conjunct is the disjointness half
  /// (the ledger never counts lines the uploader accepted; those are
  /// the drain-deadline counter's), the second is monotone sanity,
  /// the third is the headline. CALIB_PRODUCER_BLIND (the pre-fix
  /// Drop, no disclosure) falsifies the third.
  val producerLossCounted = EXECS.forall(e => and {
    refusedFrom.get(e) == -1 implies producerCountedBelow.get(e) == 0,
    producerCountedBelow.get(e) <= producedEnd.get(e),
    (buildFinished.get(e) and refusedFrom.get(e) != -1) implies
      producerCountedBelow.get(e) == producedEnd.get(e),
  })

  /// Witness (expect-violation in the producer regime): lines are
  /// actually REFUSED in the explored space -- an uploader dies and
  /// the build produces past the watermark; the producerLossCounted
  /// verdict is about a space where the ledger has work to do.
  val noRefusedLines = EXECS.forall(e => or {
    refusedFrom.get(e) == -1,
    producedEnd.get(e) <= refusedFrom.get(e),
  })

  /// `store.log.sweep-ownership` (bughunt-wave B2 item 8,
  /// merged_bug_086): a deleted lifecycle row implies the execution
  /// was sweep-eligible — terminal, attempt-ledger-unreferenced, and
  /// aged out. Sound from post-states because all three facts are
  /// frozen or monotone once the row is gone (`recordFinalLineCount`
  /// and `ledgerReleases` both require the row; `expired` never
  /// clears). The age-only calibration (`CALIB_SWEEP_AGE_ONLY`)
  /// falsifies it by sweeping a never-terminal or still-referenced
  /// execution.
  val sweepOnlyTerminalUnreferenced = EXECS.forall(e =>
    (e <= dispatched and not(execRowExists.get(e))) implies and {
      dbFinalCount.get(e) != NoCount,
      not(ledgerReferenced.get(e)),
      expired.get(e),
    })

  /// Artifact-before-row (store.log.sweep-ownership+2,
  /// merged_bug_007): a reclaimed lifecycle row never strands
  /// `drv_log_chunks` rows — the execution-row GC refuses rows whose
  /// artifacts survive, so a missing row implies an empty manifest.
  val noOrphanLogChunks = EXECS.forall(e =>
    (e <= dispatched and not(execRowExists.get(e))) implies chunks.get(e) == Set())

  /// Non-vacuity witness for the close-then-sweep liveness path
  /// (bug_047): MUST VIOLATE in the sweep regime — a closer-stamped
  /// row actually gets reclaimed (the immortality is gone).
  val noCloseStampedReclaim = not(wit.closeStampedReclaimed)

  /// `NoSilentLineLoss` (the conservation law), the four-disjunct
  /// disclosed-loss form (the predecessor campaign's conservation law):
  /// every line the build emitted into its uploader is, at all times,
  ///   (1) in the builder's ack-trimmed retransmit buffer (it will be
  ///       replayed to the next session), or
  ///   (2) in a store-side ingest buffer (accepted, awaiting a cut), or
  ///   (3) covered by a manifest chunk (durable and servable), or
  ///   (4) the loss is disclosed: the log does not read as complete
  ///       (`is_complete = false` is the reader-facing incomplete
  ///       indicator, and a swept execution's missing lifecycle row
  ///       reads as incomplete too), or the uploader recorded its
  ///       abandonment (the `rio_builder_log_drain_abandoned_total`
  ///       counter / the permanent-rejection warn).
  /// The disjuncts collapse: completeness REQUIRES the manifest to
  /// contiguously cover `[0, final_line_count)` and the count is the
  /// build's own line total, so the bite is concentrated at "a log that
  /// reads complete while its uploader has not given up" — where every
  /// produced line must already be durable. The load-bearing refinement
  /// that does not route through the completeness definition is
  /// `ackImpliesDurable` below.
  val noSilentLineLoss = EXECS.forall(e =>
    delivered(e).forall(n => or {
      builderBuf(e).contains(n),
      storeBufHolds(e, n),
      manifestHolds(e, n),
      not(logIsComplete(e)),
      uploaderAbandoned(e),
    }))

  /// The conservation law's load-bearing refinement: a line leaves the
  /// retransmit buffer only when an ack covers it, an ack is sent only
  /// after the chunk's manifest INSERT commits, and every ack VALUE is
  /// clamped to the contiguous durable frontier (merged_bug_005:
  /// `clamped_durable_ack` = min(natural end, frontier); no ack at all
  /// when nothing new is contiguously durable from line 0) — so every
  /// line below the acked watermark is durably in the manifest until
  /// the TTL sweep (the one sanctioned deleter) removes it. This is
  /// the invariant that falsifies if an ack is ever emitted before the
  /// manifest commit, if anything other than the sweep deletes a
  /// manifest row, or if any producer re-grows a hole-spanning ack
  /// value (an early or unclamped ack lets the builder prefix-pop a
  /// line that was never durably stored, with no replay and no
  /// disclosure beyond the log never completing).
  ///
  /// Asserted in EVERY regime, fabrication included (bughunt-13 F9):
  /// the pre-merged_bug_005 producer acked a committed chunk's own end
  /// — a max over chunk ENDS — so a client violating the LogBatcher's
  /// monotone-numbering contract (a high-numbered fabricated batch
  /// before its real low-numbered lines) was acked past the manifest
  /// hole and `discharge_through` prefix-popped the real lines' only
  /// copy unstored. The clamp removed that producer from the code;
  /// `deliverAck` carries the same clamp, so the analogous state
  /// (ackedBelow past a manifest hole) is unreachable in every live
  /// regime and a model-side clamp regression falsifies HERE, not
  /// nowhere. The pre-fix producer survives only as the
  /// `logServiceCalibUnclampedAck` calibration regime, whose wired
  /// check pins that this invariant falsifies under it (fabricate
  /// [k, k+1), cut, unclamped ack -> ackedBelow = k+1 with [0, k)
  /// never durable). The fabricated tail a clamped ack never covers
  /// stays in the un-acked buffer and is disclosed by the log never
  /// reading complete (`noSilentLineLoss`'s fourth disjunct holds over
  /// the same trace in every regime, unchanged).
  val ackImpliesDurable = EXECS.forall(e =>
    not(expired.get(e)) implies
      interval(0, ackedBelow.get(e)).forall(n => manifestHolds(e, n)))

  /// `ServedSpanExact` (store.log.session-keyed's read half; the
  /// successor of the predecessor's `lineSpanExact`): the read path's
  /// ordered-walk-with-watermark over the manifest — including
  /// overlapping chunks from concurrent or successive sessions — yields
  /// exactly the union of the chunks' line ranges, each line exactly
  /// once. `served == union` is the no-loss half (a wrong visit order
  /// strands a low-numbered chunk behind an already-advanced watermark
  /// and silently drops its lines); `count == size` is the no-duplicate
  /// half (the same line yielded from two overlapping chunks would
  /// double-print in the client's stream and corrupt its reconnect
  /// cursor).
  val servedSpanExact = EXECS.forall(e =>
    val r = readTail(e)
    and {
      r.served == manifestUnion(e),
      r.count == r.served.size(),
    })

  /// The user-facing restatement of the conservation law's bite: a log
  /// that reads COMPLETE serves every line the build produced. Implied
  /// by `servedSpanExact` + the completeness fold's definition + the
  /// honest terminal stamp; stated independently so an encoding bug in
  /// any one of those three is caught by the others' conjunction.
  val completeLogServesAllProduced = EXECS.forall(e =>
    logIsComplete(e) implies delivered(e).subseteq(readTail(e).served))

  /// `NoSilentLineLoss` lifted to the SERVED plane (the reader's view,
  /// not the manifest's): a final stamp claiming completeness means
  /// the subscriber actually rendered every line of the recorded
  /// final count. Falsifies immediately under the pre-fix
  /// advance-past-jump encoding (`CALIB_READER_ADVANCE_PAST`): a
  /// fan-out drop becomes a hole the cursor silently crossed, and the
  /// completeness claim lies to the one consumer who can no longer
  /// tell.
  val servedStreamNoSilentLoss = match readerFinal {
    | NoFinal => true
    | Final(f) => f.complete implies match dbFinalCount.get(1) {
        | NoCount => true
        | StampedNoCount => true
        | Count(c) => interval(0, c).subseteq(readerServed)
      }
  }

  /// Span exactness across the live seam: every rendered line is a
  /// line the build actually produced (the reader never invents), and
  /// the render count equals the served set's size (at-most-once — a
  /// double render of a resent line would push count past size).
  val servedStreamSpanExact = and {
    readerServedCount == readerServed.size(),
    readerServed.forall(n => n < producedEnd.get(1)),
  }

  /// Reader-plane bounds (the served regimes' boundsOK companion).
  val readerBoundsOK = and {
    readerCursor >= 0 and readerCursor <= MAX_LINE,
    readerServed.forall(n => n >= 0 and n < MAX_LINE),
    pendingFan.forall(n => n >= 0 and n < MAX_LINE),
    pendingFan.size() <= QUEUE_CAP,
    readerServedCount >= readerServed.size(),
    readerManifestMark >= 0 and readerManifestMark <= MAX_LINE,
    readerRecoveryEntries >= 0,
    readerExec >= 0 and readerExec <= 1,
    deliveryExec >= 0 and deliveryExec <= 1,
  }

  // Reader-plane witnesses (expect-violation: each scenario must be
  // REACHABLE or the plane is vacuous).
  /// Violated when the full queue drops a delivery.
  val noFanoutDrop = fanDropped == Set()
  /// Violated when a delivery jumps past the reader's cursor.
  val noReaderGapObserved = not(readerJumpObserved)
  /// Violated when a jump (or the final catch-up) is repaired by
  /// back-filling from the manifest/buffers.
  val noReaderRecovery = not(readerRecovered)

  // --- slot-6 served-stream delta invariants -----------------------------

  /// merged_bug_205's resume law: every HELD line below the reader's
  /// cursor is served — the cursor never silently crosses a line the
  /// store could have given it (an un-held below-cursor line is an
  /// admitted hole, disclosed by the jump/gap row). The
  /// CALIB_RESUME_PAST_GAP catch-up advances the cursor to the mark
  /// without serving and falsifies this.
  val resumeCursorSound = interval(0, readerCursor)
    .forall(n => or { readerServed.contains(n), not(readerHeld(n)) })

  /// merged_bug_187's provenance gate: counted recovery entries exist
  /// only when the fan-out actually DROPPED something — admitted
  /// holes (worker gaps, out-of-order fan) serve across without
  /// feeding the recovery lane. CALIB_RECOVERY_UNGATED counts every
  /// back-fill and falsifies this.
  val recoveryOnlyOnDrop = readerRecoveryEntries > 0 implies fanDropped != Set()

  /// bug_298's admission bound: an over-bound batch is never admitted
  /// (the ghost's only writer is the CALIB_OVERSIZED_ADMITTED arm —
  /// the LIVE oversized check makes admission unrepresentable).
  val batchLinesBounded = not(wit.oversizedAdmitted)

  /// merged_bug_002: a foreign-execution row is never drained through
  /// the old cursor's numbering (the silent splice/swallow). Only the
  /// CALIB_EXEC_SPLICE arm can set the ghost; the LIVE switch arm
  /// resets the cursor first, by construction.
  val noCrossExecRowInterleave = not(execSpliced)

  // Delta witnesses (expect-violation: reachability pins).
  /// Violated when an honest worker-gap batch lands (the admitted
  /// hole is reachable, not vacuously absent).
  val noWorkerGapAppended = not(wit.workerGapAppended)
  /// Violated when the oversized arm actually rejects a batch (the
  /// bound bites in the flood regime).
  val noOversizedRejected = not(wit.oversizedRejected)
  /// Violated when the drop wake repairs a dropped span (the latch
  /// path is reachable; entries imply the wake or the drain counted).
  val noDropWakeRecovery = not(readerRecoveryEntries > 0)

  /// `CompletenessGate` (store.log.completeness-gate's per-append half):
  /// once an ingest session has learned the execution's recorded
  /// `final_line_count`, every line it subsequently accepts is below
  /// that count — a batch starting at or past the ceiling is dropped
  /// whole, a straddling batch is truncated. Lines accepted BEFORE the
  /// session learned the ceiling (the seal landed mid-stream and the
  /// heartbeat refresh had not yet observed it) are the disclosed
  /// bounded residual: they are over-coverage the completeness fold
  /// already tolerates, not silent loss, and they are excluded here by
  /// the `>= hwAtLearn` filter (accepted line numbers are strictly
  /// increasing within a session, so "accepted after the learn" is
  /// exactly "numbered at or above the learn-time high water"). The
  /// open-time half of the rule (a COMPLETE log rejects the open
  /// outright) is `openAdmitted`'s `not(logIsComplete)` conjunct,
  /// witnessed by `noCompleteOpenRejection`.
  val completenessGate = EXECS.forall(e =>
    SESSION_IDS.forall(s =>
      match sessCeiling(e, s) {
        | NoCeiling => true
        | Ceiling(c) =>
          sessAccepted(e, s).forall(n => n >= c.hwAtLearn implies n < c.value)
      }))

  // --- expect-violation witnesses (non-vacuity probes) ------------------
  //
  // The vals below are NOT invariants and must never be added to a
  // regime check's invariant list. Each states "the interesting thing
  // never happens"; the quint-log-service-witness-* checks in
  // nix/quint.nix verify one each and PASS only when the checker reports
  // a violation — machine-checked evidence that the scenario a regime's
  // headline invariant constrains is actually reachable in that regime's
  // explored space. A witness that stops being violated means the model
  // (or a regime's constants) no longer reaches the behavior the code
  // can exhibit — a model bug, not a reason to delete the witness.

  /// Violated when an execution's log reads complete. The headline
  /// non-vacuity probe: `noSilentLineLoss` and
  /// `completeLogServesAllProduced` only bite for a complete log, and
  /// the open-time seal only fires against one.
  val noCompletedLog = not(EXECS.exists(e => logIsComplete(e)))

  /// Violated when two chunks of one execution from DIFFERENT sessions
  /// overlap in line range — the re-sent-batch-after-ambiguous-
  /// disconnect shape the read path's dedup exists for. A regime that
  /// never reaches it proves `servedSpanExact` only over disjoint
  /// manifests.
  val noOverlappingChunks = not(EXECS.exists(e =>
    chunks.get(e).exists(c1 => chunks.get(e).exists(c2 => and {
      c1.sess != c2.sess,
      chunkLines(c1).intersect(chunkLines(c2)) != Set(),
    }))))

  /// Violated when the read path's watermark actually suppresses a
  /// duplicate (the total yield count over all chunks falls short of
  /// the sum of the chunks' claimed counts) — the dedup is exercised,
  /// not merely encoded.
  val noDuplicateSuppressed = not(EXECS.exists(e =>
    readTail(e).count < chunks.get(e).fold(0, (acc, c) => acc + c.count)))

  /// Violated when a manifest contains a forward gap (two chunks with
  /// uncovered line numbers between them) — the shape the completeness
  /// fold's gap arm and the cut's split-at-gaps behavior exist for.
  val noGappedManifest = not(EXECS.exists(e =>
    chunks.get(e).exists(c1 => chunks.get(e).exists(c2 => and {
      c1.first + c1.count < c2.first,
      not(manifestHolds(e, c1.first + c1.count)),
    }))))

  /// Violated when the auth gate rejects a stream open for a superseded
  /// execution — the foreign-token rejection (a token for an assignment
  /// that is no longer the derivation's latest).
  val noSupersededOpenRejection = not(wit.supersededOpenRejected)

  /// Violated when the open-time seal rejects a stream open for an
  /// already-complete log.
  val noCompleteOpenRejection = not(wit.completeOpenRejected)

  /// Violated when the per-append completeness ceiling drops or
  /// truncates a batch — the post-terminal injection the gate exists to
  /// stop is actually attempted.
  val noPastFinalRejection = not(wit.pastFinalRejected)

  /// Violated when the monotone floor rejects a batch numbered below
  /// the session's high-water mark.
  val noNonMonotoneRejection = not(wit.nonMonotoneRejected)

  /// Violated when a session learns its completeness ceiling AFTER it
  /// has already accepted lines (the seal lands mid-stream and the
  /// heartbeat refresh picks it up) — the case that distinguishes the
  /// per-append gate from the open-time gate, and the source of the
  /// disclosed pre-refresh residual.
  val noMidStreamCeiling = not(EXECS.exists(e => SESSION_IDS.exists(s =>
    match sessCeiling(e, s) { | NoCeiling => false | Ceiling(c) => c.hwAtLearn > 0 })))

  /// Violated when an uploader abandons its drain with un-acked lines —
  /// the disclosed-loss channel (`rio_builder_log_drain_abandoned_total`
  /// / the permanent-rejection discard) actually fires.
  val noAbandonedWithUnacked = not(EXECS.exists(e => and {
    uploaderAbandoned(e),
    ackedBelow.get(e) < producedEnd.get(e),
  }))

  /// Violated when two executions of the derivation hold committed
  /// chunks while neither's sessions are all closed — the concurrent-
  /// writers state `noCrossExecContamination` is about (the superseded
  /// execution's session still committing while the successor's
  /// session also commits).
  val noConcurrentExecWriters = not(EXECS.exists(e1 => EXECS.exists(e2 => and {
    e1 < e2,
    chunks.get(e1) != Set(),
    chunks.get(e2) != Set(),
    SESSION_IDS.exists(s => hasSession(e1, s) and sessBuf(e1, s) != Set()),
  })))

  /// Violated when the TTL sweep deletes a non-empty manifest — the
  /// sweep's deletion arm is reachable, so `ackImpliesDurable`'s
  /// expired carve-out and the conservation law's disclosed-loss
  /// disjunct are exercised against a real deletion.
  val noSweptChunks = not(wit.sweptChunks)

  /// Violated when a build execution's REPLAY open is admitted while a
  /// later materialization attempt holds the latest-assignment slot —
  /// the exact displacement scenario merged_bug_101's fix exists for.
  /// `buildTailAdmittable`'s verdict in the kind-mix regime is about a
  /// state space that actually contains this admission.
  val noMatDispatchThenBuildReplayAdmitted = not(wit.matThenBuildReplayAdmitted)

  /// Violated when some abandonment actually fires the disclosure
  /// counter — `ingestLossCounted`'s counted arm is exercised against
  /// a real counted loss, not merely encoded.
  val noLossCounterFired = not(EXECS.exists(e => uploaderLossCounted(e)))

  /// Violated when an expired execution SURVIVES with its lifecycle
  /// row intact because it is still ineligible (non-terminal or
  /// ledger-referenced) — the v2 guard actually refuses something the
  /// age-only sweep would have taken.
  val noLiveExecSurvivesSweep = not(EXECS.exists(e => and {
    expired.get(e),
    execRowExists.get(e),
    or { dbFinalCount.get(e) == NoCount, ledgerReferenced.get(e) },
  }))

  // --- 6. actions -------------------------------------------------------
  //
  // Each action's precondition is the corresponding code path's guard
  // and its effect is that path's state mutation; the function it
  // mirrors is named in its doc comment. Actions are GUARDED (disabled
  // when their preconditions fail), not thin always-enabled no-ops.
  // Ceilings (MAX_EXECS, MAX_SESSIONS, MAX_LINE) are action
  // preconditions so the TLC backend's state space is finite.

  /// Frame helper: the variables almost every action leaves unchanged.
  /// Quint has no frame conditions; each action lists its unchanged
  /// variables explicitly. (Kept as individual assignments at each
  /// action rather than a helper action — a helper that assigns a
  /// variable the action also assigns would be a double-assignment
  /// error.)

  // --- 6a. dispatch and the build --------------------------------------

  /// The scheduler dispatches the derivation: a new assignment attempt
  /// with a fresh exec_id becomes the LATEST assignment, and its
  /// `drv_executions` lifecycle row is INSERTed.
  // Mirrors: rio-scheduler assign_to_worker -> assignments INSERT +
  //          rio-scheduler/src/db/executions.rs (the dispatch INSERT).
  action dispatch = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    dispatched < MAX_EXECS,
    EXECS.filter(e => e <= dispatched and execKind.get(e) == KBuild).size() < MAX_BUILD_EXECS,
    dispatched' = dispatched + 1,
    execRowExists' = execRowExists.put(dispatched + 1, true),
    execKind' = execKind.put(dispatched + 1, KBuild),
    ledgerReferenced' = ledgerReferenced.put(dispatched + 1, true),
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// A materialization attempt of the same derivation takes the
  /// latest-assignment slot (the zeroth-executor mint): a fresh
  /// exec_id with `attempt_kind = 'materialization'` becomes the
  /// LATEST assignment and its lifecycle row is INSERTed. It has no
  /// uploader, produces no log lines, and the open gate's kind
  /// conjunct never admits it — what it CAN do, pre-fix, is displace
  /// a build execution's late replay (the latest-only gate;
  /// `CALIB_GATE_LATEST_ONLY`).
  // Mirrors: rio-scheduler mint_and_deliver (kind = materialization)
  //          observed by gate.rs as a newer assignments row.
  action dispatchMaterialization = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_MAT_DISPATCH,
    dispatched < MAX_EXECS,
    dispatched' = dispatched + 1,
    execRowExists' = execRowExists.put(dispatched + 1, true),
    execKind' = execKind.put(dispatched + 1, KMat),
    ledgerReferenced' = ledgerReferenced.put(dispatched + 1, true),
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// ENVIRONMENT: the scheduler's attempt-ledger GC moves its per-lane
  /// suffix cut past this execution's rows — the ledger no longer
  /// references it and the v2 sweep may reclaim the lifecycle row.
  /// Guarded on the row still existing so the reference state is
  /// frozen after deletion (see the var's doc).
  // Mirrors: the drv_attempts per-lane GC cut observed by
  //          sweep.rs's eligibility SELECT as zero referencing rows.
  action ledgerReleases(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_SWEEP,
    e <= dispatched,
    ledgerReferenced.get(e),
    execRowExists.get(e),
    ledgerReferenced' = ledgerReferenced.put(e, false),
    execKind' = execKind,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
  }

  /// The build emits one more stderr line into its uploader's input
  /// channel (and so into the retransmit buffer).
  // Mirrors: rio-builder/src/log_stream.rs (the per-line counter) +
  //          log_upload.rs::UploadTask::accept (the buffer push).
  action produceLine(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    execKind.get(e) == KBuild,
    not(buildFinished.get(e)),
    producedEnd.get(e) < MAX_LINE,
    producedEnd' = producedEnd.put(e, producedEnd.get(e) + 1),
    dispatched' = dispatched,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The build exits: no more lines, the completion report (carrying
  /// the build's own line total) is ready for the scheduler, and the
  /// uploader's input channel closes (arming the drain deadline).
  // Mirrors: rio-builder/src/runtime/mod.rs (the post-footer
  //          completion report) + log_upload.rs::accept(None).
  action buildFinishes(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    e <= dispatched,
    execKind.get(e) == KBuild,
    not(buildFinished.get(e)),
    buildFinished' = buildFinished.put(e, true),
    // The build-exit Drop (UploadSink::Lost): disclose every refused
    // line through the DiscardLedger -- unless the calibration
    // restores the pre-fix silent Drop.
    producerCountedBelow' = producerCountedBelow.put(e,
      if (and {
        ENABLE_PRODUCER_PLANE,
        not(CALIB_PRODUCER_BLIND),
        refusedFrom.get(e) != -1,
      }) producedEnd.get(e)
      else producerCountedBelow.get(e)),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The scheduler stamps the execution's lifecycle row terminal with
  /// the builder-reported line count. A zero-line build maps to NULL
  /// (the proto's not-reported sentinel), so its log can never read
  /// complete — disclosed, permanent, and harmless (the banner header
  /// guarantees real builds are never zero-line). The stamp is honest
  /// (the count is the build's own total): a scheduler that stamps a
  /// count below the true total would seal real lines out of the log —
  /// that is a trusted-component failure outside this model.
  /// Precondition `not(expired)`: the terminal stamp follows the
  /// completion report by seconds, the TTL by days.
  // Mirrors: rio-scheduler/src/actor/completion.rs
  //          (terminal_log_epilogue's `try_from(...).filter(> 0)`).
  action recordFinalLineCount(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    buildFinished.get(e),
    execRowExists.get(e),
    not(expired.get(e)),
    // The epilogue commutes with the assignment-close stamp on the
    // equal verdict: a closer-stamped (StampedNoCount) row still
    // receives its count (COALESCE final_line_count — the qual
    // `status IS NULL OR status = $2`).
    or {
      dbFinalCount.get(e) == NoCount,
      dbFinalCount.get(e) == StampedNoCount,
    },
    dbFinalCount' = dbFinalCount.put(e,
      if (producedEnd.get(e) > 0) Count(producedEnd.get(e)) else NoCount),
    // Avoid a self-loop when the count maps to NULL: the row is
    // terminal either way, and a terminal row with an unknown count is
    // observationally identical to a non-terminal one (see the
    // dbFinalCount doc), so re-enabling the action would only add a
    // stuttering step.
    producedEnd.get(e) > 0,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6b. the open gate -----------------------------------------------

  /// `gate.rs::check_append_open`'s admission predicate, restricted to
  /// the checks with model content (v2, the bughunt-wave re-keying):
  /// the claimed execution's OWN assignment row still exists (check 3 —
  /// supersession by a newer attempt no longer revokes write authority
  /// over the old execution; only the in-place rewrite does), its log
  /// is not already complete (check 4), and its durable accepted
  /// account is under the per-execution cap (check 5,
  /// `store.log.caps-durable`). Checks 1-2 (HMAC, the token-vs-header
  /// derivation match) are pure equality on signed fields; the
  /// single-live-session lease is the `openSessionOf(e) == 0`
  /// admission conjunct on the action itself. v3 (bughunt-wave B2
  /// item 8): the kind axis is modeled — the gate's `attempt_kind`
  /// read admits build executions only, and token expiry is part of
  /// the per-exec predicate (the gate's view of an expired row).
  def openAdmitted(e: int): bool = and {
    e <= dispatched,
    execKind.get(e) == KBuild,
    // CALIBRATION: the pre-fix latest-assignment-only gate — any later
    // dispatch (including a materialization mint) revoked the replay.
    not(CALIB_GATE_LATEST_ONLY) or e == dispatched,
    CALIB_GATE_IGNORES_ROW or not(assignmentRowLost.get(e)),
    not(logIsComplete(e)),
    not(expired.get(e)),
    // CALIBRATION: pre-fix there WAS no open-time cap check (check 5
    // did not exist) — the per-session encoding drops this conjunct
    // along with resetting the account.
    CALIB_SESSION_SCOPED_CAPS or acceptedTotal.get(e) < CAP,
  }

  /// The builder opens (or re-opens) an `AppendLog` stream for its
  /// execution and the gate admits it. The new session is born with the
  /// completeness ceiling iff the execution is already terminal with a
  /// known count (the late-replay case); the uploader's transmit cursor
  /// rewinds to its acked watermark so everything un-acked is
  /// retransmitted (the at-least-once replay).
  // Mirrors: service.rs::append_log (steps 3-6) + gate.rs::
  //          check_append_open + sessions::acquire +
  //          log_upload.rs::UploadTask::open / drive's `sent = 0`.
  action openSession(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    openAdmitted(e),
    uploader.get(e) == Uploading,
    // One AppendLog stream per uploader: the previous session must have
    // been detached (the builder cannot hold two streams). The lease's
    // Busy rejection against a still-fresh foreign-replica session is a
    // bounded retry delay, not a distinct safety state.
    openSessionOf(e) == 0,
    sessionsOpened.get(e) < MAX_SESSIONS,
    // A second open is the uploader's reconnect-and-replay; the regimes
    // that do not explore the at-least-once shape disable it (the first
    // open is always allowed).
    sessionsOpened.get(e) == 0 or ENABLE_RECONNECT,
    // Token expiry << the 30-day TTL (see the header's scope notes);
    // the conjunct lives in `openAdmitted` (v3).
    sessions' = sessions.put(e, sessions.get(e).put(sessionsOpened.get(e) + 1, Session({
      phase: SessOpen,
      highWater: 0,
      ceiling: match dbFinalCount.get(e) {
        | NoCount => NoCeiling
        | StampedNoCount => NoCeiling
        | Count(n) => Ceiling({ value: n, hwAtLearn: 0 })
      },
      buf: Set(),
    }))),
    sessionsOpened' = sessionsOpened.put(e, sessionsOpened.get(e) + 1),
    // The open-time coverage ack (bug_032, the wire-1 letter): when
    // the execution has durable contiguous coverage, the FIRST message
    // on the new ack stream advertises its watermark and the uploader
    // trims its retransmit buffer below it BEFORE replaying
    // (`open_coverage_next_line`; absent/zero = legacy no-trim). The
    // open is atomic here, so the letter rides it: ackedBelow advances
    // to the watermark and the rewound transmit cursor follows.
    ackedBelow' = ackedBelow.put(e, maxInt(ackedBelow.get(e), coveredPrefixEnd(e))),
    sentBelow' = sentBelow.put(e, maxInt(ackedBelow.get(e), coveredPrefixEnd(e))),
    // CALIBRATION: the pre-fix per-session account zeroed on every
    // open; production keeps the lifetime account.
    acceptedTotal' = if (CALIB_SESSION_SCOPED_CAPS) acceptedTotal.put(e, 0)
                     else acceptedTotal,
    // Witness ghost: this open is a REPLAY (not the first session) and
    // a later materialization attempt holds the latest slot — the
    // admission the pre-fix latest-only gate displaced.
    wit' = if (sessionsOpened.get(e) > 0 and
               EXECS.exists(m => m <= dispatched and m > e and execKind.get(m) == KMat))
             { ...wit, matThenBuildReplayAdmitted: true }
           else wit,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The gate rejects a stream open whose claimed execution's
  /// assignment row was rewritten in place (FAILED_PRECONDITION, the
  /// permanent `superseded` class). v2: mere supersession by a newer
  /// attempt no longer rejects — only the erased row does. The
  /// uploader treats it as a permanent rejection: the retransmit
  /// buffer is discarded and the remaining input is drained to
  /// nowhere — the un-stored tail is gone, disclosed by the
  /// loss-disclosure counter and by its log never reading complete.
  // Mirrors: gate.rs::check_append_open (the no-matching-row arm) +
  //          log_upload.rs::reject_permanently.
  action openRejectedSuperseded(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    execKind.get(e) == KBuild,
    assignmentRowLost.get(e),
    uploader.get(e) == Uploading,
    openSessionOf(e) == 0,
    // v2 (2f975de9a): the permanent-rejection discard routes through
    // the `disclose()` chokepoint like every other abandonment arm.
    // CALIBRATION: pre-fix, rejected:true SUPPRESSED the counter — the
    // buffer vanished silently (merged_bug_360).
    uploader' = uploader.put(e, Abandoned({ counted: not(CALIB_LOSS_ESCAPE) })),
    wit' = { ...wit, supersededOpenRejected: true },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// ENVIRONMENT: the scheduler's ON-CONFLICT upsert rewrites a
  /// superseded execution's assignment row in place — the one path
  /// that permanently revokes the old executor's write authority
  /// under the v2 claimed-exec gate. Only non-latest executions are
  /// rewritable (the live attempt's row is what the rewrite WRITES).
  // Mirrors: the scheduler's assignment upsert observed by
  //          gate.rs::check_append_open as fetch_optional == None.
  action rewriteAssignment(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    e != dispatched,
    not(assignmentRowLost.get(e)),
    assignmentRowLost' = assignmentRowLost.put(e, true),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
  }

  /// The gate rejects a stream open because the execution's log is
  /// already complete (FAILED_PRECONDITION). Same uploader reaction as
  /// the superseded rejection, but nothing of value is lost: every line
  /// the buffer still holds is already covered by the (complete)
  /// manifest.
  // Mirrors: gate.rs::check_append_open (check 4) +
  //          log_upload.rs::reject_permanently.
  action openRejectedComplete(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    e == dispatched,
    execKind.get(e) == KBuild,
    logIsComplete(e),
    uploader.get(e) == Uploading,
    openSessionOf(e) == 0,
    uploader' = uploader.put(e, Abandoned({ counted: true })),
    wit' = { ...wit, completeOpenRejected: true },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6c. batch ingestion ---------------------------------------------

  /// Rebuild session `s` of `e` after accepting the (post-truncation)
  /// batch `[lo, keptHi)`: the lines join the buffer, the high water
  /// rises to the kept end. Only meaningful under a `hasSession` guard.
  pure def sessAfterAccept(slot: SessionSlot, lo: int, keptHi: int): SessionSlot =
    match slot {
      | NoSession => NoSession
      | Session(x) => Session({
          phase: x.phase,
          highWater: maxInt(x.highWater, keptHi),
          ceiling: x.ceiling,
          buf: x.buf.union(interval(lo, keptHi)),
        })
    }

  /// The uploader transmits the next un-sent run of its retransmit
  /// buffer — `[sentBelow, producedEnd)`, the whole remainder — to the
  /// execution's Open session. The accept verdict is a pure function of
  /// the session's state: the batch is accepted (possibly truncated at
  /// the completeness ceiling), rejected as non-monotone (only
  /// reachable after a fabricated batch raised the high water past the
  /// honest cursor), or rejected as past-final (only reachable after a
  /// truncation left the cursor at the ceiling). The transmit cursor
  /// advances regardless — the uploader gets no per-batch verdict, only
  /// acks — and a rejected batch's lines stay in the retransmit buffer
  /// until an ack covers them or the drain is abandoned.
  // Mirrors: log_upload.rs::drive (the send arm) ->
  //          service.rs::drive (the Batch arm) ->
  //          ingest.rs::IngestSession::accept.
  action appendHonest(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    uploader.get(e) == Uploading,
    val s = openSessionOf(e)
    val lo = sentBelow.get(e)
    val hi = producedEnd.get(e)
    // The durable floor (bug_032): the session trims a replay's
    // below-floor prefix before the verdict — with the open-time
    // coverage ack the honest uploader rarely sends one (the letter
    // already trimmed), but a session opened against gapped coverage
    // has floor 0 and replays blind, and the trim still protects the
    // account. The covered consult (merged_bug_002's write-path arm)
    // then drops a batch FULLY inside durable coverage: un-buffered,
    // un-charged, high-water advanced, acked from the manifest truth.
    val floor = coveredPrefixEnd(e)
    val lo2 = maxInt(lo, floor)
    all {
      s != 0,
      lo < hi,
      // The mid-stream cap guard (`store.log.caps-durable+2`): the
      // kept lines must fit in the execution's lifetime account. The
      // over-cap batch is the stream-fatal FAILED_PRECONDITION arm —
      // modeled by this action being disabled (the abort itself is
      // `sessionAborts`' nondeterminism). Covered drops charge
      // nothing, so they never disable the action.
      or {
        hi <= floor,
        match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo2, hi) {
          | Accept(keptHi) => or {
              rangeCovered(e, lo2, keptHi),
              acceptedTotal.get(e) + (keptHi - lo2) <= CAP,
            }
          | RejectOversized => true
          | RejectNonMonotone => true
          | RejectPastFinal => true
        },
      },
      sentBelow' = sentBelow.put(e, hi),
      if (hi <= floor) all {
        // Fully below the floor: a committed-but-unacked replay —
        // dropped, acked, never re-charged (`AcceptOutcome::
        // CoveredReplay`).
        sessions' = sessions.put(e,
          sessions.get(e).put(s, sessAfterCoveredDrop(sessions.get(e).get(s), hi))),
        acceptedTotal' = acceptedTotal,
        ackedBelow' = ackedBelow.put(e, maxInt(ackedBelow.get(e), hi)),
        wit' = wit,
      } else
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo2, hi) {
        | Accept(keptHi) =>
          if (rangeCovered(e, lo2, keptHi)) all {
            // The whole admitted range is already durable: the
            // covered-replay drop (gapped-coverage shapes; the
            // contiguous prefix went through the floor arm above).
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterCoveredDrop(sessions.get(e).get(s), keptHi))),
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow.put(e, maxInt(ackedBelow.get(e), keptHi)),
            wit' = wit,
          } else all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterAccept(sessions.get(e).get(s), lo2, keptHi))),
            acceptedTotal' = acceptedTotal.put(e, acceptedTotal.get(e) + (keptHi - lo2)),
            ackedBelow' = ackedBelow,
            wit' = if (hi - lo2 > MAX_BATCH_LINES)
                // Only reachable with CALIB_OVERSIZED_ADMITTED (the
                // live arm rejects first): the admitted over-bound
                // batch — batchLinesBounded's violation source.
                { ...wit, oversizedAdmitted: true }
              else if (keptHi < hi) { ...wit, pastFinalRejected: true }
              else wit,
          }
        | RejectOversized => all {
            // Per-batch rejection, stream open, nothing buffered —
            // unreachable for the honest uploader unless the flood
            // regime shrinks MAX_BATCH_LINES below the line domain.
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = { ...wit, oversizedRejected: true },
          }
        | RejectNonMonotone => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = { ...wit, nonMonotoneRejected: true },
          }
        | RejectPastFinal => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = { ...wit, pastFinalRejected: true },
          }
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The bound builder sends a batch that is NOT its uploader's honest
  /// in-order replay: an arbitrary `[lo, hi)` at or above the session's
  /// high water. Accepted lines (post-truncation at the completeness
  /// ceiling) enter the buffer and the `fabricated` ghost; lines at or
  /// past the ceiling are dropped. This is the adversary the per-append
  /// completeness gate exists for ("a builder holding a still-current
  /// token for a terminal-but-incomplete execution can append arbitrary
  /// lines past the build's true end") and the source of forward-gapped
  /// manifests. The uploader's cursors are untouched — the fabrication
  /// is the builder process misbehaving alongside its own uploader, and
  /// The HONEST uploader's stream skips a span (ENABLE_WORKER_GAP):
  /// rate-limit suppression or batcher drop — the worker-side
  /// admitted-hole source (merged_bug_187's provenance axis; the
  /// suppression marker disclosure is out of model). The skipped span
  /// `[sentBelow, lo)` is never retransmitted; the batch `[lo, hi)`
  /// lands as a legal forward gap. Distinct from fabrication: these
  /// are genuine produced lines, so the reader-plane span laws keep
  /// counting them — only the conservation totality (which suppression
  /// breaks BY DESIGN) is regime-excluded.
  action appendWorkerGap(e: int): bool = all {
    ENABLE_WORKER_GAP,
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    uploader.get(e) == Uploading,
    val s = openSessionOf(e)
    val skipTo = sentBelow.get(e) + 1
    val hi = producedEnd.get(e)
    all {
      s != 0,
      skipTo < hi,
      sessHw(e, s) <= skipTo,
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), skipTo, hi) {
        | Accept(keptHi) => acceptedTotal.get(e) + (keptHi - skipTo) <= CAP
        | RejectOversized => true
        | RejectNonMonotone => true
        | RejectPastFinal => true
      },
      sentBelow' = sentBelow.put(e, hi),
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), skipTo, hi) {
        | Accept(keptHi) => all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterAccept(sessions.get(e).get(s), skipTo, keptHi))),
            acceptedTotal' = acceptedTotal.put(e, acceptedTotal.get(e) + (keptHi - skipTo)),
            wit' = { ...wit, workerGapAppended: true },
          }
        | RejectOversized => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            wit' = { ...wit, oversizedRejected: true },
          }
        | RejectNonMonotone => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            wit' = wit,
          }
        | RejectPastFinal => all {
            sessions' = sessions,
            acceptedTotal' = acceptedTotal,
            wit' = { ...wit, pastFinalRejected: true },
          }
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// its high-water side effect can permanently strand the honest
  /// replay behind the monotone floor (the builder shooting its own
  /// log; the strand is disclosed by the log never completing).
  // Mirrors: ingest.rs::IngestSession::accept (the accept and
  //          truncate-at-ceiling arms) driven by an untrusted
  //          LogBatcher.
  action appendFabricated(e: int, lo: int, hi: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_FABRICATION,
    e <= dispatched,
    lo < hi,
    hi <= MAX_LINE,
    val s = openSessionOf(e)
    // The fabricator rides the same accept path as the honest uploader
    // (one chokepoint): the below-floor prefix is trimmed and a batch
    // fully inside durable coverage is dropped un-charged with the
    // manifest-truth ack — a fabricated re-send of committed content
    // can mint no chunk, no charge, and no fabricated ghost.
    val floor = coveredPrefixEnd(e)
    val lo2 = maxInt(lo, floor)
    all {
      s != 0,
      lo >= sessHw(e, s),
      // Fabricated lines are accepted (and charged) like honest ones —
      // the cap is an accounting bound, not an honesty check. Covered
      // drops charge nothing.
      or {
        hi <= floor,
        match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo2, hi) {
          | Accept(keptHi) => or {
              rangeCovered(e, lo2, keptHi),
              acceptedTotal.get(e) + (keptHi - lo2) <= CAP,
            }
          | RejectOversized => true
          | RejectNonMonotone => true
          | RejectPastFinal => true
        },
      },
      if (hi <= floor) all {
        sessions' = sessions.put(e,
          sessions.get(e).put(s, sessAfterCoveredDrop(sessions.get(e).get(s), hi))),
        fabricated' = fabricated,
        acceptedTotal' = acceptedTotal,
        ackedBelow' = ackedBelow.put(e, maxInt(ackedBelow.get(e), hi)),
        wit' = wit,
      } else
      match acceptVerdict(sessHw(e, s), sessCeiling(e, s), lo2, hi) {
        | Accept(keptHi) =>
          if (rangeCovered(e, lo2, keptHi)) all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterCoveredDrop(sessions.get(e).get(s), keptHi))),
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow.put(e, maxInt(ackedBelow.get(e), keptHi)),
            wit' = wit,
          } else all {
            sessions' = sessions.put(e,
              sessions.get(e).put(s, sessAfterAccept(sessions.get(e).get(s), lo2, keptHi))),
            fabricated' = fabricated.put(e, fabricated.get(e).union(interval(lo2, keptHi))),
            acceptedTotal' = acceptedTotal.put(e, acceptedTotal.get(e) + (keptHi - lo2)),
            ackedBelow' = ackedBelow,
            wit' = if (keptHi < hi) { ...wit, pastFinalRejected: true } else wit,
          }
        | RejectOversized => all {
            // Reachable only when the flood regime shrinks the bound;
            // the fabricated over-bound batch is dropped whole.
            sessions' = sessions,
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = { ...wit, oversizedRejected: true },
          }
        | RejectNonMonotone => all {
            // Unreachable under the `lo >= hw` precondition; the arm
            // must still assign every variable.
            sessions' = sessions,
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = wit,
          }
        | RejectPastFinal => all {
            sessions' = sessions,
            fabricated' = fabricated,
            acceptedTotal' = acceptedTotal,
            ackedBelow' = ackedBelow,
            wit' = { ...wit, pastFinalRejected: true },
          }
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// A session learns the execution's recorded `final_line_count`
  /// mid-stream: the scheduler stamped the lifecycle row terminal while
  /// the builder was still streaming, and the handler's heartbeat-tick
  /// refresh observed it. The learn-time high-water mark is recorded so
  /// `completenessGate` can separate the pre-refresh residual from the
  /// post-refresh obligation. Only an Open session refreshes (the
  /// heartbeat loop exits with the driver's select loop); the first
  /// observed value wins (the count is stamped once and never changes).
  // Mirrors: service.rs::drive (the heartbeat arm's
  //          sealed_final_line_count refresh) +
  //          ingest.rs::set_final_line_count.
  action refreshCeiling(e: int, s: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    sessIsOpen(e, s),
    sessCeiling(e, s) == NoCeiling,
    execRowExists.get(e),
    val n = dbFinalCount.get(e)
    all {
      match n { | Count(_) => true | _ => false },
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({
              phase: x.phase,
              highWater: x.highWater,
              ceiling: match n {
                | NoCount => NoCeiling
                | StampedNoCount => NoCeiling
                | Count(v) => Ceiling({ value: v, hwAtLearn: x.highWater })
              },
              buf: x.buf,
            })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6d. the chunk cut and the ack -----------------------------------

  /// Cut the longest contiguous prefix of session `s`'s buffer into one
  /// immutable chunk: the run leaves the buffer and a manifest row
  /// describing exactly that run is committed under the cutting
  /// session's `(exec, session)` key. The commit and the manifest
  /// INSERT are one atomic step here — the failure path restores the
  /// staged run exactly (a no-op at this granularity) and burns a
  /// sequence number the model does not track; the abort that three
  /// consecutive failures trigger is `sessionAborts`. Both Open and
  /// Detached sessions cut (the periodic/size-triggered cut and the
  /// final drain respectively) — a detached predecessor session
  /// committing its drain concurrently with its successor's cuts is
  /// the manifest-overlap source.
  // Mirrors: ingest.rs::IngestSession::cut + commit_chunk;
  //          service.rs::do_cut / drain.
  action cutChunk(e: int, s: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    hasSession(e, s),
    sessBuf(e, s) != Set(),
    val cutRun = runFrom(sessBuf(e, s), setMin(sessBuf(e, s)))
    all {
      chunks' = chunks.put(e, chunks.get(e).union(Set({
        sess: s,
        first: setMin(cutRun),
        count: cutRun.size(),
      }))),
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({ ...x, buf: x.buf.exclude(cutRun) })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The builder receives the ack for one committed chunk of its
  /// CURRENT Open session and prefix-pops every retransmit-buffer frame
  /// whose last line the ack covers (`discharge_through`). The ack
  /// exists only because the chunk's manifest INSERT committed
  /// (`do_cut` sends it after `cut` returns the durable-through value),
  /// and it is deliverable only while the builder still holds the
  /// stream that cut the chunk — a detached session's drain acks go to
  /// a closed response stream. The ack VALUE is a contiguous-prefix
  /// claim, never the chunk's own end (merged_bug_005, modeled at
  /// bughunt-13 F9): every producer routes through
  /// `clamped_durable_ack` = min(natural end, frontier), where the
  /// frontier is `CoverageMap::contiguous_prefix_end` over the
  /// execution's committed chunks — `coveredPrefixEnd(e)` here — and
  /// NO ack is sent when the clamp yields nothing new (the code's
  /// `None`/no-send arm is this action's enabling guard). Outside
  /// holey states the clamp is the identity, so honest contiguous
  /// traffic acks exactly what it always did; the prefix-pop is a max,
  /// so delivering any single chunk's clamped ack reaches the same
  /// watermark set as delivering the ordered prefix up to it.
  // Mirrors: rio-store/src/logs/service.rs:do_cut
  // Mirrors: rio-store/src/logs/ingest.rs:clamped_durable_ack
  // Mirrors: rio-log-kernel/src/lib.rs:contiguous_prefix_end
  // Mirrors: rio-builder/src/log_upload.rs:discharge_through
  action deliverAck(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    uploader.get(e) == Uploading,
    val s = openSessionOf(e)
    all {
      s != 0,
      nondet c = chunks.get(e).filter(c => c.sess == s).oneOf()
      // CALIB_UNCLAMPED_ACK re-encodes the pre-merged_bug_005 producer
      // (the chunk's own end, hole-spanning) for the falsify twin.
      val ackEnd = if (CALIB_UNCLAMPED_ACK) c.first + c.count
                   else minInt(c.first + c.count, coveredPrefixEnd(e))
      all {
        ackEnd > ackedBelow.get(e),
        ackedBelow' = ackedBelow.put(e, ackEnd),
      },
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6e. disconnects, aborts, abandonment ----------------------------

  /// The builder's stream to its Open session ends without a
  /// server-side abort: the clean half-close at the end of every build,
  /// a transport error, or the builder reconnecting elsewhere. The
  /// driver runs its final drain — the session keeps its buffer and
  /// keeps cutting (as a Detached session) until the buffer is empty.
  /// Un-acked lines stay in the builder's retransmit buffer for the
  /// next session; the drain's chunks for those same lines are the
  /// manifest overlap the read path dedups. Enabled in every regime
  /// (every build's stream ends); whether a SECOND session can then
  /// open is `openSession`'s ENABLE_RECONNECT gate.
  // Mirrors: service.rs::drive (Ok(None) / Err -> ClientFinished) +
  //          AppendDriver::run (the drain arm).
  action builderDisconnects(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    val s = openSessionOf(e)
    all {
      s != 0,
      sessions' = sessions.put(e, sessions.get(e).put(s,
        match sessions.get(e).get(s) {
          | NoSession => NoSession
          | Session(x) => Session({ ...x, phase: SessDetached })
        })),
    },
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The server tears the stream down and DROPS the in-memory buffer
  /// without draining it: the gray-failure abort (three consecutive
  /// failed cuts, a stale buffer), the per-execution byte/chunk caps,
  /// or the ingest lease observed as lost. Safe by design: every
  /// dropped line is still in the builder's retransmit buffer (it was
  /// never acked) and will be replayed to the next session.
  // Mirrors: service.rs::AppendDriver::run (the LoopExit::Abort /
  //          LeaseLost arms — the drain is skipped and the
  //          deregistration scopeguard drops IngestShared).
  action sessionAborts(e: int, s: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    hasSession(e, s),
    // An inert detached session (empty buffer) aborting is a no-op
    // self-loop; require something observable to change.
    sessIsOpen(e, s) or sessBuf(e, s) != Set(),
    sessions' = sessions.put(e, sessions.get(e).put(s,
      match sessions.get(e).get(s) {
        | NoSession => NoSession
        | Session(x) => Session({ ...x, phase: SessDetached, buf: Set() })
      })),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The uploader gives up with lines still un-acked: the post-build
  /// drain deadline expires while the store is unreachable (or keeps
  /// rejecting the stream). The retransmit buffer is dropped; the loss
  /// is disclosed by `rio_builder_log_drain_abandoned_total` and the
  /// error log, and the execution's log never reads complete (the
  /// manifest cannot cover the abandoned tail).
  // Mirrors: log_upload.rs::run_inner (DeadlineExpired -> abandoned()).
  /// The EARLY uploader death (bughunt2-wave slot 8, bug_241): the
  /// upload task panics/aborts while the build still produces. The
  /// channel closes -- the refusal watermark is stamped at the
  /// current produced end, and every line produced from here on
  /// bounces into the DiscardLedger (disclosed at the build-exit
  /// Drop; see buildFinishes). The un-acked range the uploader had
  /// already accepted, [ackedBelow, refusedFrom), is the death
  /// path's own disclosure -- counted: true, exactly the drain arm's
  /// accounting (production: the SendError-bounced batch seeds the
  /// ledger and the LossGuard discloses the buffered range).
  /// Producer-plane only.
  // Mirrors: rio-builder/src/log_upload.rs (UploadSink::Lost +
  //          DiscardLedger seed on SendError).
  action uploaderDies(e: int): bool = all {
    readerUnchanged,
    ENABLE_PRODUCER_PLANE,
    e <= dispatched,
    execKind.get(e) == KBuild,
    uploader.get(e) == Uploading,
    not(buildFinished.get(e)),
    uploader' = uploader.put(e, Abandoned({ counted: true })),
    refusedFrom' = refusedFrom.put(e, producedEnd.get(e)),
    producerCountedBelow' = producerCountedBelow,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  action uploaderAbandons(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    execKind.get(e) == KBuild,
    uploader.get(e) == Uploading,
    buildFinished.get(e),
    ackedBelow.get(e) < producedEnd.get(e),
    // The drain-deadline arm always routes through `disclose()`; its
    // un-acked lines are real loss, so the counter fires.
    uploader' = uploader.put(e, Abandoned({ counted: true })),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  // --- 6f. expiry and the TTL sweep ------------------------------------

  /// The retention clock passes the execution's `started_at +
  /// log_retention_days`. Environment action; monotone.
  action executionExpires(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_SWEEP,
    e <= dispatched,
    not(expired.get(e)),
    expired' = expired.put(e, true),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    acceptedTotal' = acceptedTotal,
  }

  /// The store's TTL sweep pass for one expired execution
  /// (`sweep.rs::sweep_expired_logs`): every `drv_log_chunks` manifest
  /// row (and transitively the objects). The pass also clears the
  /// `log_ingest_sessions` REGISTRY rows — hygiene this model does not
  /// carry: the `sessions` variable mirrors the IN-MEMORY ingest
  /// sessions (phase, high water, buffer), which the pass never
  /// touches — a still-open session keeps cutting after the pass, and
  /// the SELECT's EXISTS disjunction re-finds the new chunks next
  /// pass. v2 (`store.log.sweep-ownership`): the pass NEVER deletes
  /// the `drv_executions` row — lifecycle state is scheduler-owned
  /// (`gcExecRow`). Age is the pass's only eligibility: safe precisely
  /// BECAUSE the row survives. The guard requires something to strip
  /// (the EXISTS disjunction keeps a stripped execution out of every
  /// later pass).
  // Mirrors: sweep.rs::sweep_expired_logs (the drv_log_chunks DELETE +
  //          delete_batch; the registry DELETE is unmodeled hygiene).
  action sweepChunks(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_SWEEP,
    e <= dispatched,
    expired.get(e),
    execRowExists.get(e),
    // The structural exclusions (merged_bug_071): age alone is not a
    // liveness proof. A victim must be TERMINAL — a non-terminal row
    // is an open attempt that may legally stream to the scheduler's
    // deadline cap (config validation refuses retention at or under
    // it) — and free of any LIVE ingest-session registry row
    // (`registryLive`, the same notion the scheduler's reclaim keys
    // on: open sessions and detached-but-undrained ones still hold
    // their row; the implementation's reap-grace window bounds the
    // row's afterlife).
    dbFinalCount.get(e) != NoCount,
    not(SESSION_IDS.exists(s => registryLive(e, s))),
    not(sweepChunksDone.get(e)),
    or {
      chunks.get(e) != Set(),
      SESSION_IDS.exists(s => hasSession(e, s)),
    },
    chunks' = chunks.put(e, Set()),
    sessions' = sessions,
    sweepChunksDone' = sweepChunksDone.put(e, true),
    wit' = if (chunks.get(e) != Set()) { ...wit, sweptChunks: true } else wit,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessionsOpened' = sessionsOpened,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The SCHEDULER's execution-row reclaim
  /// (`SchedulerDb::gc_exec_rows`, the housekeeping tick arm): deletes
  /// the `drv_executions` row behind the kernel's eligibility —
  /// `execRowSweepEligible` (terminal AND ledger-unreferenced AND aged
  /// out; see the def's doc for the priced fourth conjunct). Runs in a
  /// different service from the store's sweep pass: no ordering with
  /// `sweepChunks` — a row reclaimed before its chunks are stripped
  /// leaves the chunks unreachable to the store SELECT (bounded by the
  /// S3 lifecycle backstop, priced in the header), and a row that
  /// outlives its stripped chunks reads incomplete forever (disclosed).
  // Mirrors: rio-scheduler/src/db/attempts.rs::gc_exec_rows behind
  //          rio_retry_kernel::exec_row_sweep_eligible.
  action gcExecRow(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    ENABLE_SWEEP,
    e <= dispatched,
    execRowSweepEligible(e),
    execRowExists.get(e),
    execRowExists' = execRowExists.put(e, false),
    wit' = if (dbFinalCount.get(e) == StampedNoCount)
      { ...wit, closeStampedReclaimed: true }
    else wit,
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  /// The assignment close stamps the execution row terminal in the
  /// SAME statement (sched.db.exec-stamp-on-close;
  /// `close_assignments_sql`'s stamped CTE): the no-report path's
  /// verdict — a crashed or never-reporting build's assignment closes
  /// (fenced close, sweep_stale_assignments, the materialization
  /// cancel) and the row becomes terminal with a NULL count
  /// (`StampedNoCount`). Guarded `status IS NULL`-first-verdict-wins:
  /// a row the epilogue already stamped keeps its count. Pre-fix
  /// (CALIB_CLOSE_NO_STAMP) the close stamped NOTHING — the row kept
  /// `NoCount` forever and was immortal to `gcExecRow` (bug_047).
  // Mirrors: rio-scheduler/src/db/mod.rs::close_assignments_sql
  //          (the stamped CTE arm; status IS NULL guard).
  action closeExecStamp(e: int): bool = all {
    readerUnchanged,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    e <= dispatched,
    execRowExists.get(e),
    dbFinalCount.get(e) == NoCount,
    dbFinalCount' = dbFinalCount.put(e,
      if (CALIB_CLOSE_NO_STAMP) NoCount else StampedNoCount),
    dispatched' = dispatched,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    execRowExists' = execRowExists,
    expired' = expired,
    sweepChunksDone' = sweepChunksDone,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
  }

  // --- 7b. served-stream (reader) plane ----------------------------------
  // The reader consumes execution 1's log through the SAME decision
  // kernel the consumers adopted (rio-log-kernel::visit_chunk +
  // tail_next): a delivery at the cursor is contiguous; a delivery past
  // it names the gap, and the FIXED behavior back-fills the missing
  // span from the manifest and store buffers before advancing
  // (rio-store serve_tail's recovery, the gateway's reopen-once, the
  // dashboard's gap rows all project to this one step). The
  // CALIB_READER_ADVANCE_PAST encoding is the pre-fix silent splice.

  /// Frame: the reader plane untouched. One line in every
  /// non-reader action keeps the QNT000 fan-out mechanical.
  action readerUnchanged = all {
    readerAttached' = readerAttached,
    readerCursor' = readerCursor,
    readerServed' = readerServed,
    readerServedCount' = readerServedCount,
    pendingFan' = pendingFan,
    readerFinal' = readerFinal,
    fanDropped' = fanDropped,
    readerJumpObserved' = readerJumpObserved,
    readerRecovered' = readerRecovered,
    readerManifestMark' = readerManifestMark,
    readerPendingStamp' = readerPendingStamp,
    readerRecoveryEntries' = readerRecoveryEntries,
    droppedPending' = droppedPending,
    readerExec' = readerExec,
    deliveryExec' = deliveryExec,
    execSpliced' = execSpliced,
  }

  /// Frame: the core (non-reader) state untouched — the reader's
  /// actions are read-only over the log state.
  action coreUnchanged = all {
    dispatched' = dispatched,
    refusedFrom' = refusedFrom,
    producerCountedBelow' = producerCountedBelow,
    producedEnd' = producedEnd,
    buildFinished' = buildFinished,
    dbFinalCount' = dbFinalCount,
    execRowExists' = execRowExists,
    expired' = expired,
    assignmentRowLost' = assignmentRowLost,
    acceptedTotal' = acceptedTotal,
    sweepChunksDone' = sweepChunksDone,
    execKind' = execKind,
    ledgerReferenced' = ledgerReferenced,
    uploader' = uploader,
    ackedBelow' = ackedBelow,
    sentBelow' = sentBelow,
    sessions' = sessions,
    sessionsOpened' = sessionsOpened,
    chunks' = chunks,
    fabricated' = fabricated,
    wit' = wit,
  }

  /// What the store can serve the reader for execution 1 right now:
  /// a manifest-covered line or a line sitting in an ingest buffer
  /// (the open-session snapshot phase reads both).
  def readerHeld(n: int): bool = or {
    manifestHolds(1, n),
    storeBufHolds(1, n),
  }

  /// Subscribe: the manifest walk + open-session snapshot (serve_tail
  /// phases 1-2) delivers everything currently held; the live
  /// registration starts with an empty queue. The cursor lands one
  /// past the highest snapshot line — interior snapshot holes are the
  /// (already-disclosed) storage-hole story, not this plane's seam.
  action readerSubscribe = all {
    ENABLE_READER,
    not(readerAttached),
    readerFinal == NoFinal,
    val held = interval(0, MAX_LINE).filter(n => readerHeld(n))
    all {
      readerAttached' = true,
      readerServed' = held,
      readerServedCount' = readerServedCount + held.size(),
      readerCursor' = if (held == Set()) 0 else held.fold(0, (a, b) => maxInt(a, b)) + 1,
      pendingFan' = Set(),
      readerFinal' = readerFinal,
      fanDropped' = fanDropped,
      readerJumpObserved' = readerJumpObserved,
      readerRecovered' = readerRecovered,
      readerManifestMark' = readerManifestMark,
      readerPendingStamp' = readerPendingStamp,
      readerRecoveryEntries' = readerRecoveryEntries,
      droppedPending' = droppedPending,
      readerExec' = readerExec,
      deliveryExec' = deliveryExec,
      execSpliced' = execSpliced,
    },
    coreUnchanged,
  }

  /// One accepted line joins the live fan-out — or is DROPPED at a
  /// full queue (`try_send` Full, the adversarial branch). Each line
  /// fans at most once (one broadcast per accept).
  action fanoutDeliver(n: int): bool = all {
    ENABLE_READER,
    readerAttached,
    readerHeld(n),
    n >= readerCursor,
    not(readerServed.contains(n)),
    not(pendingFan.contains(n)),
    not(fanDropped.contains(n)),
    if (pendingFan.size() < QUEUE_CAP) all {
      pendingFan' = pendingFan.union(Set(n)),
      fanDropped' = fanDropped,
      droppedPending' = droppedPending,
    } else all {
      // Full queue: the delivery is dropped — and the drop-epoch
      // LATCH arms (d02563663's watch::Sender; a pulse here would be
      // lost exactly when the serve loop is parked mid-send). The
      // reader learns of this line through the wake's back-fill, a
      // later jump, or the final catch-up.
      pendingFan' = pendingFan,
      fanDropped' = fanDropped.union(Set(n)),
      droppedPending' = true,
    },
    readerAttached' = readerAttached,
    readerCursor' = readerCursor,
    readerServed' = readerServed,
    readerServedCount' = readerServedCount,
    readerFinal' = readerFinal,
    readerJumpObserved' = readerJumpObserved,
    readerRecovered' = readerRecovered,
    readerManifestMark' = readerManifestMark,
    readerPendingStamp' = readerPendingStamp,
    readerRecoveryEntries' = readerRecoveryEntries,
    readerExec' = readerExec,
    deliveryExec' = deliveryExec,
    execSpliced' = execSpliced,
    coreUnchanged,
  }

  action fanoutDeliverAny = {
    nondet n = interval(0, MAX_LINE).oneOf()
    fanoutDeliver(n)
  }

  /// The reader drains the oldest queued delivery through the kernel
  /// step: at-cursor serves contiguously; below-cursor is a resent
  /// duplicate and is skipped (never re-rendered); past-cursor is an
  /// OBSERVED gap — the fixed reader back-fills the missing span from
  /// the manifest and buffers before advancing, the calibration reader
  /// advances past it (the pre-fix splice).
  action readerDrain = all {
    ENABLE_READER,
    readerAttached,
    pendingFan != Set(),
    val n = setMin(pendingFan)
    val execMismatch = ENABLE_EXEC_AXIS and deliveryExec != readerExec
    all {
      pendingFan' = pendingFan.exclude(Set(n)),
      if (execMismatch and not(CALIB_EXEC_SPLICE)) all {
        // merged_bug_002, the FIXED switch arm: a delivery from a new
        // execution resets the cursor to the new numbering plane and
        // discloses the switch (the execSwitch row); the delivery
        // itself then serves against the fresh floor (n == 0 serves,
        // n > 0 is the new plane's own jump).
        readerExec' = deliveryExec,
        readerCursor' = n + 1,
        readerServed' = Set(n),
        readerServedCount' = readerServedCount + 1,
        readerJumpObserved' = (readerJumpObserved or n > 0),
        readerRecovered' = readerRecovered,
        readerRecoveryEntries' = readerRecoveryEntries,
        execSpliced' = execSpliced,
      } else if (execMismatch and CALIB_EXEC_SPLICE) all {
        // CALIBRATION (pre-fix splice): the foreign delivery drains
        // through the OLD cursor. Its low line numbers look like
        // resent duplicates and are SWALLOWED — the silent splice.
        readerExec' = readerExec,
        if (n < readerCursor) all {
          readerCursor' = readerCursor,
          readerServed' = readerServed,
          readerServedCount' = readerServedCount,
          execSpliced' = true,
        } else all {
          readerCursor' = n + 1,
          readerServed' = readerServed.union(Set(n)),
          readerServedCount' = readerServedCount + 1,
          execSpliced' = execSpliced,
        },
        readerJumpObserved' = readerJumpObserved,
        readerRecovered' = readerRecovered,
        readerRecoveryEntries' = readerRecoveryEntries,
      } else if (n < readerCursor) all {
        // Resent duplicate: the watermark already covers it.
        readerExec' = readerExec,
        readerCursor' = readerCursor,
        readerServed' = readerServed,
        readerServedCount' = readerServedCount,
        readerJumpObserved' = readerJumpObserved,
        readerRecovered' = readerRecovered,
        readerRecoveryEntries' = readerRecoveryEntries,
        execSpliced' = execSpliced,
      } else if (n == readerCursor) all {
        readerExec' = readerExec,
        readerCursor' = n + 1,
        readerServed' = readerServed.union(Set(n)),
        readerServedCount' = readerServedCount + 1,
        readerJumpObserved' = readerJumpObserved,
        readerRecovered' = readerRecovered,
        readerRecoveryEntries' = readerRecoveryEntries,
        execSpliced' = execSpliced,
      } else all {
        // Jump past the cursor: [readerCursor, n) is missing from the
        // live stream.
        readerExec' = readerExec,
        readerJumpObserved' = true,
        execSpliced' = execSpliced,
        if (CALIB_READER_ADVANCE_PAST) all {
          // Pre-fix: advance past, serve only the jumped-to line.
          readerCursor' = n + 1,
          readerServed' = readerServed.union(Set(n)),
          readerServedCount' = readerServedCount + 1,
          readerRecovered' = readerRecovered,
          readerRecoveryEntries' = readerRecoveryEntries,
        } else all {
          // Fixed: back-fill the held part of the hole first. The
          // un-held remainder is provenance-admitted (a worker gap or
          // a swept span has nothing to back-fill from) — served
          // across, no entry. The COUNTED recovery (m187) keys on
          // DROP provenance: only a back-fill that re-serves a line
          // the fan-out actually dropped increments the counter — a
          // jump produced by out-of-order fan delivery back-fills
          // without counting (the code's per-accept broadcast cannot
          // even produce that order; the model is more adversarial).
          // CALIB_RECOVERY_UNGATED counts EVERY back-fill — the
          // pre-fix amplification where admitted holes fed the
          // recovery lane.
          // Already-served lines are excluded (the code's sealed
          // visits dedup by construction): once the drop wake can
          // serve ABOVE the cursor, the served set is no longer a
          // prefix, and an unfiltered back-fill would re-count a
          // wake-served line — the double-render the at-most-once
          // half of servedStreamSpanExact exists to catch (and did,
          // in the gate's exhaustive search).
          val backfill = interval(readerCursor, n).filter(m => and {
            readerHeld(m),
            not(readerServed.contains(m)),
          })
          val counted = if (CALIB_RECOVERY_UNGATED) backfill != Set()
            else backfill.intersect(fanDropped) != Set()
          all {
            readerCursor' = n + 1,
            readerServed' = readerServed.union(backfill).union(Set(n)),
            readerServedCount' = readerServedCount + backfill.size() + 1,
            readerRecovered' = (readerRecovered or backfill != Set()),
            readerRecoveryEntries' = readerRecoveryEntries
              + (if (counted) 1 else 0),
          }
        },
      },
      readerAttached' = readerAttached,
      readerFinal' = readerFinal,
      fanDropped' = fanDropped,
      readerManifestMark' = readerManifestMark,
      readerPendingStamp' = readerPendingStamp,
      droppedPending' = droppedPending,
      deliveryExec' = deliveryExec,
    },
    coreUnchanged,
  }

  /// FIRST HALF of the finalize split (merged_bug_063's
  /// commit-mid-serve seam): the fixed reader catches up from the
  /// manifest and buffers (serve_tail's pre-`send_final` catch-up) and
  /// records the mark it caught up THROUGH. The stamp is a separate
  /// action — core actions (a cut committing, a line landing) can
  /// interleave between the two, exactly the window where the
  /// cursor-blind stamp lied. The calibration readers catch up
  /// nothing (`CALIB_READER_ADVANCE_PAST`) or advance the cursor
  /// without serving (`CALIB_RESUME_PAST_GAP`).
  action readerCatchup = all {
    ENABLE_READER,
    readerAttached,
    not(readerPendingStamp),
    val held = interval(0, MAX_LINE).filter(m => readerHeld(m))
    val catchup = if (CALIB_READER_ADVANCE_PAST or CALIB_RESUME_PAST_GAP) Set()
      else held.filter(m => not(readerServed.contains(m)))
    val mark = if (held == Set()) 0 else held.fold(0, (a, b) => maxInt(a, b)) + 1
    val cursor2 = if (CALIB_RESUME_PAST_GAP) maxInt(readerCursor, mark)
      else if (readerServed.union(catchup) == Set()) readerCursor
      else maxInt(readerCursor,
        readerServed.union(catchup).fold(0, (a, b) => maxInt(a, b)) + 1)
    val counted = if (CALIB_RECOVERY_UNGATED) catchup != Set()
      else catchup.intersect(fanDropped) != Set()
    all {
      readerServed' = readerServed.union(catchup),
      readerServedCount' = readerServedCount + catchup.size(),
      readerCursor' = cursor2,
      readerRecovered' = (readerRecovered or catchup != Set()),
      readerRecoveryEntries' = readerRecoveryEntries + (if (counted) 1 else 0),
      readerManifestMark' = mark,
      readerPendingStamp' = true,
      readerAttached' = readerAttached,
      readerFinal' = readerFinal,
      pendingFan' = pendingFan,
      fanDropped' = fanDropped,
      readerJumpObserved' = readerJumpObserved,
      droppedPending' = droppedPending,
      readerExec' = readerExec,
      deliveryExec' = deliveryExec,
      execSpliced' = execSpliced,
    },
    coreUnchanged,
  }

  /// SECOND HALF: the final message stamps. The FIXED stamp's
  /// completeness claim is the store's completeness fold AND the
  /// catch-up coverage re-check (everything held at stamp time is
  /// served — a line that landed after the mark un-claims
  /// completeness; the FinalClaim cursor-bind). The
  /// CALIB_STAMP_SKIPS_CATCHUP stamp claims the store's fold verbatim
  /// — the pre-fix cursor-blind predicate, honest only by luck.
  action readerStamp = all {
    ENABLE_READER,
    readerAttached,
    readerPendingStamp,
    val covered = interval(0, MAX_LINE)
      .forall(m => readerHeld(m) implies readerServed.contains(m))
    val complete = if (CALIB_STAMP_SKIPS_CATCHUP) logIsComplete(1)
      else logIsComplete(1) and covered
    all {
      readerAttached' = false,
      readerPendingStamp' = false,
      readerFinal' = Final({ cursor: readerCursor, complete: complete }),
      pendingFan' = Set(),
      readerServed' = readerServed,
      readerServedCount' = readerServedCount,
      readerCursor' = readerCursor,
      readerRecovered' = readerRecovered,
      readerRecoveryEntries' = readerRecoveryEntries,
      readerManifestMark' = readerManifestMark,
      fanDropped' = fanDropped,
      readerJumpObserved' = readerJumpObserved,
      droppedPending' = droppedPending,
      readerExec' = readerExec,
      deliveryExec' = deliveryExec,
      execSpliced' = execSpliced,
    },
    coreUnchanged,
  }

  /// The drop-epoch wake (ENABLE_DROP_WAKE; d02563663's latch): armed
  /// `droppedPending`, the serve loop back-fills every held dropped
  /// line at its next select — the burst-end drop serves WITHOUT new
  /// fan-out output. CALIB_DROP_WAKE_LOST models the lost pulse: the
  /// action never fires (the latch the code uses cannot lose the
  /// wake; a Notify could and DID — the flaked predecessor run).
  action readerWakeOnDrop = all {
    ENABLE_READER,
    ENABLE_DROP_WAKE,
    not(CALIB_DROP_WAKE_LOST),
    readerAttached,
    or { droppedPending, CALIB_RECOVERY_UNGATED },
    val recoverable = fanDropped.filter(m => and {
      readerHeld(m),
      not(readerServed.contains(m)),
    })
    val ungated = if (CALIB_RECOVERY_UNGATED)
      interval(0, MAX_LINE).filter(m => and {
        readerHeld(m), not(readerServed.contains(m)),
      })
      else recoverable
    val served2 = readerServed.union(ungated)
    all {
      readerServed' = served2,
      readerServedCount' = readerServedCount + ungated.size(),
      // SEALED advance (m205's CursorAdvance law, the same rule the
      // code's LineCursor enforces): the cursor moves only through
      // the CONTIGUOUS served prefix — a max-based advance would
      // cross a held line whose fan-out simply hasn't run yet, the
      // exact resume-unsoundness resumeCursorSound forbids.
      readerCursor' = interval(readerCursor, MAX_LINE)
        .filter(m => not(served2.contains(m)))
        .fold(MAX_LINE, (a, b) => minInt(a, b)),
      readerRecovered' = (readerRecovered or ungated != Set()),
      readerRecoveryEntries' = readerRecoveryEntries
        + (if (ungated != Set()) 1 else 0),
      droppedPending' = false,
      readerAttached' = readerAttached,
      readerFinal' = readerFinal,
      pendingFan' = pendingFan,
      fanDropped' = fanDropped,
      readerJumpObserved' = readerJumpObserved,
      readerManifestMark' = readerManifestMark,
      readerPendingStamp' = readerPendingStamp,
      readerExec' = readerExec,
      deliveryExec' = deliveryExec,
      execSpliced' = execSpliced,
    },
    coreUnchanged,
  }

  /// The live stream retags to a new execution (ENABLE_EXEC_AXIS): a
  /// retry on another worker whose numbering restarts. Abstract 0/1
  /// tag toggle; the reader's NEXT drain must take the switch arm.
  action execSwitchDelivery = all {
    ENABLE_READER,
    ENABLE_EXEC_AXIS,
    readerAttached,
    deliveryExec == readerExec,
    deliveryExec' = 1 - deliveryExec,
    readerAttached' = readerAttached,
    readerCursor' = readerCursor,
    readerServed' = readerServed,
    readerServedCount' = readerServedCount,
    pendingFan' = pendingFan,
    readerFinal' = readerFinal,
    fanDropped' = fanDropped,
    readerJumpObserved' = readerJumpObserved,
    readerRecovered' = readerRecovered,
    readerManifestMark' = readerManifestMark,
    readerPendingStamp' = readerPendingStamp,
    readerRecoveryEntries' = readerRecoveryEntries,
    droppedPending' = droppedPending,
    readerExec' = readerExec,
    execSpliced' = execSpliced,
    coreUnchanged,
  }

  // --- 8. init ----------------------------------------------------------

  action init = all {
    dispatched' = 0,
    producedEnd' = EXECS.mapBy(e => 0),
    buildFinished' = EXECS.mapBy(e => false),
    dbFinalCount' = EXECS.mapBy(e => NoCount),
    execRowExists' = EXECS.mapBy(e => false),
    expired' = EXECS.mapBy(e => false),
    assignmentRowLost' = EXECS.mapBy(e => false),
    acceptedTotal' = EXECS.mapBy(e => 0),
    sweepChunksDone' = EXECS.mapBy(e => false),
    execKind' = EXECS.mapBy(e => KBuild),
    ledgerReferenced' = EXECS.mapBy(e => true),
    uploader' = EXECS.mapBy(e => Uploading),
    refusedFrom' = EXECS.mapBy(e => -1),
    producerCountedBelow' = EXECS.mapBy(e => 0),
    ackedBelow' = EXECS.mapBy(e => 0),
    sentBelow' = EXECS.mapBy(e => 0),
    sessions' = EXECS.mapBy(e => SESSION_IDS.mapBy(s => NoSession)),
    sessionsOpened' = EXECS.mapBy(e => 0),
    chunks' = EXECS.mapBy(e => Set()),
    fabricated' = EXECS.mapBy(e => Set()),
    wit' = {
      supersededOpenRejected: false,
      completeOpenRejected: false,
      pastFinalRejected: false,
      nonMonotoneRejected: false,
      sweptChunks: false,
      matThenBuildReplayAdmitted: false,
      workerGapAppended: false,
      oversizedRejected: false,
      oversizedAdmitted: false,
      closeStampedReclaimed: false,
    },
    readerAttached' = false,
    readerCursor' = 0,
    readerServed' = Set(),
    readerServedCount' = 0,
    pendingFan' = Set(),
    readerFinal' = NoFinal,
    fanDropped' = Set(),
    readerJumpObserved' = false,
    readerRecovered' = false,
    readerManifestMark' = 0,
    readerPendingStamp' = false,
    readerRecoveryEntries' = 0,
    droppedPending' = false,
    readerExec' = 0,
    deliveryExec' = 0,
    execSpliced' = false,
  }

  // --- 9. step ----------------------------------------------------------
  // Every disjunct of `step` is a NAMED action with its nondet choices
  // bound directly inside it (the MBT tracker records the innermost
  // disjunct's name; an anonymous block makes the whole step anonymous
  // and undrivable).

  action produceLineAny = { nondet e = EXECS.oneOf() produceLine(e) }
  action buildFinishesAny = { nondet e = EXECS.oneOf() buildFinishes(e) }
  action recordFinalLineCountAny = { nondet e = EXECS.oneOf() recordFinalLineCount(e) }
  action openSessionAny = { nondet e = EXECS.oneOf() openSession(e) }
  action openRejectedSupersededAny = { nondet e = EXECS.oneOf() openRejectedSuperseded(e) }
  action rewriteAssignmentAny = { nondet e = EXECS.oneOf() rewriteAssignment(e) }
  action openRejectedCompleteAny = { nondet e = EXECS.oneOf() openRejectedComplete(e) }
  action appendHonestAny = { nondet e = EXECS.oneOf() appendHonest(e) }
  action appendWorkerGapAny = { nondet e = EXECS.oneOf() appendWorkerGap(e) }
  action appendFabricatedAny = {
    nondet e = EXECS.oneOf()
    nondet lo = 0.to(MAX_LINE - 1).oneOf()
    nondet hi = 1.to(MAX_LINE).oneOf()
    appendFabricated(e, lo, hi)
  }
  action refreshCeilingAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    refreshCeiling(e, s)
  }
  action cutChunkAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    cutChunk(e, s)
  }
  action deliverAckAny = { nondet e = EXECS.oneOf() deliverAck(e) }
  action builderDisconnectsAny = { nondet e = EXECS.oneOf() builderDisconnects(e) }
  action sessionAbortsAny = {
    nondet e = EXECS.oneOf()
    nondet s = SESSION_IDS.oneOf()
    sessionAborts(e, s)
  }
  action uploaderAbandonsAny = { nondet e = EXECS.oneOf() uploaderAbandons(e) }
  action uploaderDiesAny = { nondet e = EXECS.oneOf() uploaderDies(e) }
  action executionExpiresAny = { nondet e = EXECS.oneOf() executionExpires(e) }
  action sweepChunksAny = { nondet e = EXECS.oneOf() sweepChunks(e) }
  action gcExecRowAny = { nondet e = EXECS.oneOf() gcExecRow(e) }
  action closeExecStampAny = { nondet e = EXECS.oneOf() closeExecStamp(e) }
  action ledgerReleasesAny = { nondet e = EXECS.oneOf() ledgerReleases(e) }

  action step = any {
    dispatch,
    dispatchMaterialization,
    produceLineAny,
    buildFinishesAny,
    recordFinalLineCountAny,
    openSessionAny,
    openRejectedSupersededAny,
    rewriteAssignmentAny,
    openRejectedCompleteAny,
    appendHonestAny,
    appendFabricatedAny,
    refreshCeilingAny,
    cutChunkAny,
    deliverAckAny,
    builderDisconnectsAny,
    sessionAbortsAny,
    uploaderAbandonsAny,
    // Producer-plane (disabled at ENABLE_PRODUCER_PLANE = false; the
    // pre-existing regimes' state spaces re-measured byte-identical
    // at the introducing commit).
    uploaderDiesAny,
    executionExpiresAny,
    sweepChunksAny,
    gcExecRowAny,
    closeExecStampAny,
    ledgerReleasesAny,
    readerSubscribe,
    fanoutDeliverAny,
    readerDrain,
    readerCatchup,
    readerStamp,
    readerWakeOnDrop,
    execSwitchDelivery,
    appendWorkerGapAny,
  }
}

// =========================================================================
// THE BASE REGIME: one execution, one session, no reconnects, no sweep.
// The adversaries are the builder's own batch stream (fabricated forward
// gaps, re-sends below the high water, lines at or past the recorded
// final count) and the unbiased interleaving of production, transmission,
// cutting, acking, the terminal stamp, and the mid-stream ceiling
// refresh. A regression in the core ingest/cut/read/complete lifecycle —
// the accept predicate, the per-append completeness ceiling, the
// contiguous-prefix cut, the read fold, the completeness fold — surfaces
// in this small fast check instead of buried in the multi-session
// regimes.
//
// Constants are chosen for COVERAGE — the smallest state space that
// still reaches every interleaving class the invariants are about:
//   - 1 execution: the supersession scenarios need a second execution
//     and live in the redispatch regime.
//   - 1 session: the overlap/dedup scenarios need a second session and
//     live in the resend regime.
//   - 3 line numbers: a forward gap ({0,2}), a batch straddling the
//     completeness ceiling, and a partial ack are the smallest shapes
//     that exercise the cut's gap split, the ceiling truncation, and
//     the conservation law's buffer accounting.
//
// NON-VACUITY at these constants (machine-checked by the
// quint-log-service-witness-* checks in nix/quint.nix): a log reads
// complete, a past-final batch is rejected, a non-monotone batch is
// rejected, a session learns its ceiling mid-stream, a manifest carries
// a forward gap, and an uploader abandons un-acked lines.
// =========================================================================
module logServiceBase {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = true,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  // --- deterministic protocol runs --------------------------------------
  // Executable sanity checks of the happy path and the known-critical
  // scenarios; `quint test --main=logServiceBase --match=<run>` replays
  // them. They do not change the verified state space (runs are not
  // part of `step`).

  /// The happy path: dispatch, produce two lines, stream them, cut,
  /// ack, finish, stamp, and the log reads complete with every produced
  /// line served.
  run happyPathRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(deliverAck(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .expect(all {
        ackedBelow.get(1) == 2,
        chunks.get(1) == Set({ sess: 1, first: 0, count: 2 }),
        dbFinalCount.get(1) == Count(2),
        logIsComplete(1),
        readTail(1).served == Set(0, 1),
        builderBuf(1) == Set(),
      })

  /// The per-append completeness ceiling: the execution goes terminal
  /// at 1 line while the session is open; the refresh observes it; a
  /// fabricated batch at the recorded end is rejected whole and a
  /// straddling batch is truncated, so the manifest never grows past
  /// the recorded count.
  run pastFinalRejectionRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .then(refreshCeiling(1, 1))
      .expect(sessCeiling(1, 1) == Ceiling({ value: 1, hwAtLearn: 1 }))
      .then(appendFabricated(1, 1, 3))
      .expect(all {
        wit.pastFinalRejected,
        sessBuf(1, 1) == Set(0),
        sessAccepted(1, 1).forall(n => n < 1),
      })

  /// The mid-stream seal residual: lines accepted before the session
  /// learns the ceiling are kept (over-coverage the completeness fold
  /// tolerates), lines at or past it offered afterwards are dropped.
  run midStreamCeilingResidualRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendFabricated(1, 0, 3))
      .expect(sessBuf(1, 1) == Set(0, 1, 2))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .then(refreshCeiling(1, 1))
      // The residual (lines 1 and 2, accepted before the learn) stays;
      // completenessGate excludes it via hwAtLearn = 3.
      .expect(all {
        sessCeiling(1, 1) == Ceiling({ value: 1, hwAtLearn: 3 }),
        completenessGate,
      })
}

// =========================================================================
// THE REDISPATCH REGIME: two executions of one derivation, one session
// each. Execution 1 is superseded by execution 2 mid-stream: its
// already-open session keeps writing to its OWN (old) execution's log,
// its builder's next reconnect is rejected by the auth gate and discards
// its un-stored tail, and the two executions' manifests grow
// concurrently under disjoint key namespaces. This is the regime where
// `authGateExcludesForeignWriters` and `noCrossExecContamination` are
// load-bearing; the witness checks pin the superseded-open rejection and
// the concurrent-writers state as reachable.
//
// Fabrication is DISABLED: the adversarial batch stream is the base
// regime's subject, and none of the supersession scenarios depend on it
// — a fabricated line lands in the fabricating execution's own keyspace
// regardless of whether that execution is the latest assignment, which
// the base regime already proves. Enabling it here multiplies two
// already-independent per-execution state machines by the fabrication
// branching factor squared and pushes the check past the per-check
// budget without adding an interleaving class any invariant observes.
// =========================================================================
module logServiceRedispatch {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The superseded writer, v2: execution 1's session is open and
  /// holding lines when execution 2 is dispatched. Supersession alone
  /// no longer revokes execution 1's write authority (it remains
  /// admitted — its late replay is the merged_bug_101 fix); only the
  /// scheduler's in-place row rewrite does, after which its builder is
  /// rejected at the next open and abandons. Execution 2's log is
  /// untouched by any of it.
  run supersededWriterRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(dispatch)
      // v2: BOTH stay admitted — the old exec's own row still exists.
      .expect(openAdmitted(1) and openAdmitted(2))
      .then(cutChunk(1, 1))
      .expect(all {
        chunks.get(1) == Set({ sess: 1, first: 0, count: 1 }),
        chunks.get(2) == Set(),
        noCrossExecContamination,
      })
      .then(builderDisconnects(1))
      // The one revocation path: the assignment row is rewritten in
      // place; only then is the old executor excluded.
      .then(rewriteAssignment(1))
      .expect(not(openAdmitted(1)) and openAdmitted(2))
      .then(openRejectedSuperseded(1))
      .expect(all {
        wit.supersededOpenRejected,
        uploaderAbandoned(1),
        // v2: the permanent-rejection discard routes through
        // disclose() — the loss is counted, never silent.
        uploaderLossCounted(1),
      })
}

// =========================================================================
// THE RESEND REGIME: one execution, two sessions, reconnects enabled.
// The at-least-once shape: a chunk is committed but its ack never
// reaches the builder (the stream dies first), the builder reopens a
// fresh session and retransmits its whole un-acked buffer, and the
// detached predecessor's final drain commits chunks that overlap the
// successor's. This is the regime where `servedSpanExact`'s dedup and
// `ackImpliesDurable` are load-bearing; the witness checks pin the
// overlapping-chunks state and the suppressed duplicate as reachable.
// Fabrication is disabled — the adversary here is the retransmission
// protocol itself, and the fabricated-batch branching multiplies a
// state space that the two-session product already dominates.
// =========================================================================
module logServiceResend {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    // 4, not MAX_LINE: the at-least-once replay re-charges the
    // retransmitted lines (the durable account is seeded from committed
    // chunks and the new session re-accepts the un-acked tail — the
    // code's exact double-charge on overlap), so the canonical
    // 2-line-cut + reconnect + 2-line-replay trace costs 4. The cap
    // boundary is reached by exactly that trace (noCapBoundary's
    // witness), making the cap conjuncts non-vacuous here.
    CAP = 4,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The ambiguous-disconnect overlap: session 1 commits a chunk for
  /// lines [0, 2) but the builder disconnects before the ack arrives;
  /// the builder replays the same lines to session 2, which commits an
  /// overlapping chunk; the read path serves each line exactly once.
  run ambiguousAckOverlapRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .expect(chunks.get(1) == Set({ sess: 1, first: 0, count: 2 }))
      // The ack is never delivered: the builder disconnects first.
      // Pre-letter this minted the ambiguous overlap — the reconnect
      // replayed [0,2) blind and a second identical chunk landed; the
      // open-time coverage ack (bug_032) now answers the ambiguity AT
      // OPEN: the uploader trims to the durable watermark before
      // replaying, and a third produced line streams alone.
      .then(builderDisconnects(1))
      .then(openSession(1))
      // The letter consumed: the transmit cursor starts AT the durable
      // coverage watermark, not at the stale acked floor.
      .expect(all { sentBelow.get(1) == 2, ackedBelow.get(1) == 2 })
      .then(produceLine(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 2))
      .expect(all {
        // ONE chunk per distinct span — the committed-but-unacked
        // replay minted nothing (no overlap, no double-charge), and
        // the fresh tail cut normally.
        chunks.get(1) == Set(
          { sess: 1, first: 0, count: 2 },
          { sess: 2, first: 2, count: 1 }
        ),
        acceptedTotal.get(1) == 3,
        readTail(1).served == Set(0, 1, 2),
        readTail(1).count == 3,
        servedSpanExact,
      })
}

// =========================================================================
// THE SWEEP REGIME: the resend regime plus expiry and the TTL sweep.
// The sweep's expired-only guard, the chunks-before-execution-row
// deletion order (with a live session's cut interleavable between the
// two DELETEs), and the disclosed-loss accounting for swept lines are
// the load-bearing pieces; the witness check pins the non-empty-manifest
// deletion as reachable. Sessions are capped at one — the sweep's
// interaction is with the manifest and the lifecycle row, not with the
// session overlap, and the two-session product is already covered by the
// resend regime.
// =========================================================================
module logServiceSweep {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    // Mat dispatch ON since the close-stamp work: the immortal-row
    // production red was an aged closed MATERIALIZATION row; the
    // close-then-sweep witness drives that exact kind.
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The TTL sweep deletes a complete log: the manifest and the
  /// lifecycle row go, the log stops reading complete (the disclosure),
  /// and the conservation law's disclosed-loss disjunct absorbs the
  /// deleted lines.
  run sweepCompleteLogRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(deliverAck(1))
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .expect(logIsComplete(1))
      .then(executionExpires(1))
      // The builder disconnects FIRST (merged_bug_071): a live ingest
      // session structurally shields its execution from the sweep —
      // the clean close releases the registry row, and only then is
      // the expired execution a victim.
      .then(builderDisconnects(1))
      .then(sweepChunks(1))
      // v2 ownership split: the store pass strips the manifest and the
      // session registry but the lifecycle row SURVIVES it.
      .expect(all {
        chunks.get(1) == Set(),
        not(logIsComplete(1)),
        execRowExists.get(1),
        wit.sweptChunks,
        noSilentLineLoss,
      })
      // The scheduler's reclaim needs the attempt ledger released
      // AND the artifact conjuncts clear (the registry row died at
      // the disconnect — store.log.sweep-ownership+2's sixth
      // conjunct), then the row goes.
      .then(ledgerReleases(1))
      .then(gcExecRow(1))
      .expect(not(execRowExists.get(1)))

  /// bug_047's liveness path, composed end-to-end on the production
  /// red's exact shape: a MATERIALIZATION execution that never reports
  /// — its assignment closes (the close statement stamps the row
  /// terminal, count NULL), it ages out, the ledger releases, and the
  /// execution-row GC reclaims it. Pre-fix the close stamped nothing
  /// and this row was immortal (see logServiceCalibCloseNoStamp).
  run materializationCloseThenSweptRun =
    init
      .then(dispatchMaterialization)
      .then(closeExecStamp(1))
      .expect(dbFinalCount.get(1) == StampedNoCount)
      .then(executionExpires(1))
      .then(ledgerReleases(1))
      .expect(execRowSweepEligible(1))
      .then(gcExecRow(1))
      .expect(all {
        not(execRowExists.get(1)),
        wit.closeStampedReclaimed,
        noOrphanLogChunks,
      })
}

// =========================================================================
// CALIBRATION: the pre-fix per-SESSION cap accounting (merged_bug_207).
// Every reconnect zeroes the accepted account, so the durable footprint
// grows one cap per session — `acceptedWithinCap` (measured against the
// durable chunks + buffers, not the resettable ghost) falsifies. The
// expect-violation check pins that the invariant actually catches the
// bug class this wave fixed; if a refactor makes it pass here, the
// invariant has gone vacuous and the check fails loudly.
// =========================================================================
module logServiceCalibSessionCaps {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    // 2 < MAX_LINE: the durable footprint is a SET of line numbers
    // (≤ MAX_LINE = 3), so the cap must sit BELOW the line domain for
    // an over-cap footprint to be representable. Pre-fix: session 1
    // accepts {0,1} (= CAP) and cuts; the reconnect resets the
    // account; session 2 accepts line 2 — footprint {0,1,2} = 3 > 2.
    // Post-fix the lifetime account blocks session 2's accept and the
    // reopen itself.
    CAP = 2,
    CALIB_SESSION_SCOPED_CAPS = true,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// CALIBRATION: the open gate without the assignment-row conjunct
// (merged_bug_101's sibling regression class — admission that consults
// nothing about the claimed exec's own row). With the conjunct ignored,
// a rewritten-in-place execution's builder is re-admitted —
// `authGateExcludesUnassignedWriters` falsifies.
// =========================================================================
module logServiceCalibGateRow {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = true,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// THE KIND-MIX REGIME (bughunt-wave B2 item 8): one build execution and
// one materialization attempt of the same derivation. The mat dispatch
// takes the latest-assignment slot mid-stream; the build's late replay
// (a reconnect after its first session detaches) must still be admitted
// — the displacement merged_bug_101's v2 gate fixed — and the mat
// execution must never be admitted at all. `buildTailAdmittable` is
// load-bearing here; the witness check pins the
// replay-after-mat-dispatch admission as reachable. Fabrication is
// disabled (the adversary is the dispatch interleaving, not the batch
// stream); two sessions are required for the replay open.
// =========================================================================
module logServiceKindMix {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The displacement scenario, fixed: the build streams and cuts a
  /// chunk, disconnects, a materialization mint takes the latest slot,
  /// and the build's replay open is STILL admitted (v2: its own row is
  /// intact) while the mat execution never is.
  run matDispatchThenBuildReplayRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(builderDisconnects(1))
      .then(dispatchMaterialization)
      .expect(all {
        execKind.get(2) == KMat,
        not(openAdmitted(2)),
        openAdmitted(1),
      })
      .then(openSession(1))
      .expect(all {
        wit.matThenBuildReplayAdmitted,
        buildTailAdmittable,
      })
}

// =========================================================================
// CALIBRATION: the pre-fix latest-assignment-only open gate
// (merged_bug_101's displacement hole, the kind-mix face). With
// `e == dispatched` re-added to admission, the materialization mint
// landing after the build revokes the build's replay —
// `buildTailAdmittable` falsifies. Pins that the invariant catches the
// displacement class B2's claimed-exec re-keying fixed.
// =========================================================================
module logServiceCalibLatestOnly {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = true,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// CALIBRATION: the pre-fix rejected:true suppression (merged_bug_360's
// PERMISSION_DENIED hole). The permanent-rejection abandonment discards
// the retransmit buffer without the disclosure counter; an execution
// with produced-but-never-stored lines abandons silently —
// `ingestLossCounted` falsifies. Two executions so the in-place row
// rewrite (the rejection's trigger) is reachable.
// =========================================================================
module logServiceCalibLossEscape {
  import logService(
    MAX_EXECS = 2,
    MAX_BUILD_EXECS = 2,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = true,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// CALIBRATION: the pre-fix age-only sweep SELECT (merged_bug_086). With
// the terminal and ledger-reference conjuncts ignored, the pass deletes
// a never-terminal (or still-referenced) execution's lifecycle row —
// `sweepOnlyTerminalUnreferenced` falsifies. Pins that the invariant
// catches the unguarded-delete class the kernel's
// `exec_row_sweep_eligible` guard fixed.
// =========================================================================
module logServiceCalibSweepAgeOnly {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = true,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// THE SERVED-STREAM REGIME: one execution, two sessions, a live reader
// over the fan-out seam with a capacity-1 queue — the smallest shape
// where a drop, the jump it causes, and the back-fill that repairs it
// are all reachable (merged_bug_180's fan-out hole, merged_bug_076's
// premature exit surface, merged_bug_306/311's consumer splice all
// project onto this plane). Fabrication off: the reader's contract is
// over honest logs; the fabricated-batch adversary belongs to the
// ingest planes above. Reconnect on (two sessions): the resend overlap
// is what exercises the reader's below-cursor skip.
module logServiceServed {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The repaired-drop happy path, replayable with
  /// `quint test --main=logServiceServed --match=fanoutDropRecoveryRun`:
  /// two lines land durably, the subscriber attaches on the empty log,
  /// the queue (capacity 1) takes line 0 and DROPS line 1, line 2's
  /// later delivery jumps the cursor — and the fixed reader back-fills
  /// the dropped line from the manifest before advancing, so the final
  /// stamp's completeness claim is honest.
  run fanoutDropRecoveryRun =
    init
      .then(dispatch)
      .then(readerSubscribe)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(fanoutDeliver(0)) // joins (queue 0/1)
      .then(fanoutDeliver(1)) // queue full -> DROPPED
      .then(readerDrain) //       serves 0, cursor 1
      .then(fanoutDeliver(2)) // joins
      .then(readerDrain) //       2 jumps cursor 1: back-fill 1, serve 2
      .expect(all {
        fanDropped == Set(1),
        readerJumpObserved,
        readerRecovered,
        readerServed == Set(0, 1, 2),
        readerServedCount == 3,
        readerCursor == 3,
      })
}

// CALIBRATION (expect-violation): the pre-fix reader — the cursor
// advances past a fan-out drop with no back-fill and the final stamp
// performs no catch-up; `servedStreamNoSilentLoss` falsifies (the
// completeness claim covers a line the subscriber never rendered).
// The paired hold check is `quint-log-service-served`.
module logServiceCalibReaderAdvance {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = true,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = true,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The recorded pre-fix counterexample, replayable with
  /// `quint test --main=logServiceCalibReaderAdvance
  /// --match=fanoutDropSilentLossRun`: same drop as the recovery run,
  /// but the calibration reader splices — line 1 is never rendered,
  /// no recovery happens, and the final stamp still claims complete.
  run fanoutDropSilentLossRun =
    init
      .then(dispatch)
      .then(readerSubscribe)
      .then(produceLine(1))
      .then(produceLine(1))
      .then(produceLine(1))
      .then(openSession(1))
      .then(appendHonest(1))
      .then(cutChunk(1, 1))
      .then(fanoutDeliver(0)) // joins (queue 0/1)
      .then(fanoutDeliver(1)) // queue full -> DROPPED
      .then(readerDrain) //       serves 0, cursor 1
      .then(fanoutDeliver(2)) // joins
      .then(readerDrain) //       PRE-FIX: cursor jumps to 3, line 1 spliced
      .then(buildFinishes(1))
      .then(recordFinalLineCount(1))
      .then(readerCatchup) //    calib: catches up nothing
      .then(readerStamp) //       blind stamp: complete over the hole
      .expect(all {
        fanDropped == Set(1),
        readerJumpObserved,
        not(readerRecovered),
        readerServed == Set(0, 2),
        readerFinal == Final({ cursor: 3, complete: true }),
      })
}


// =========================================================================
// SLOT-6 SERVED-STREAM DELTA REGIMES (bughunt-2). Each LIVE regime has
// its falsify twin (same bindings, one CALIB flag flipped) wired as a
// quint-log-service-calib-* expect-violation check in nix/quint.nix —
// a twin that stops violating means the live invariant went vacuous.
// =========================================================================

// LIVE: the honest worker-gap + drop-wake plane. The uploader can skip
// spans (suppression — the admitted-hole source), the capacity-1 queue
// drops under burst, and the drop-epoch LATCH wakes the serve loop to
// back-fill. The conservation-totality invariants are deliberately
// absent: suppression breaks produced→served totality BY DESIGN (the
// disclosure is the suppression marker, out of model); the reader-plane
// laws are what hold. RECONNECT IS OFF in this regime: the model's
// replay resets sentBelow to ackedBelow, which would resurrect a
// suppressed line the code's batcher dropped from its retransmit
// buffer permanently — suppression permanence is the rio-builder
// proptest's pin (suppression_never_skips_numbers), not this plane's;
// the reconnect x served-stream interleavings stay verified in
// logServiceServed.
module logServiceWorkerGap {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = true,
    ENABLE_DROP_WAKE = true,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN of logServiceWorkerGap: the wake is LOST (pulse-not-latch) and
// the stamp is blind — a burst-end drop becomes a hole a complete
// stamp crosses. servedStreamNoSilentLoss MUST falsify.
module logServiceCalibDropWakeLost {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = true,
    ENABLE_DROP_WAKE = true,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = true,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = true,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN: the finalize split's commit-mid-serve seam alone — catch-up
// runs honestly, a cut commits BETWEEN catch-up and stamp, and the
// blind stamp claims the store's fold without the coverage re-check.
// servedStreamNoSilentLoss MUST falsify (merged_bug_063's shape).
module logServiceCalibStampSkips {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = true,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN: recovery counted without drop provenance — out-of-order fan
// delivery makes the drain back-fill held-but-never-dropped lines, and
// the ungated counter bills them to the recovery lane (merged_bug_187's
// worker amplification). recoveryOnlyOnDrop MUST falsify. QUEUE_CAP=2
// so the jump can happen with NO drop at all.
module logServiceCalibRecoveryUngated {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 2,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = true,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN: the resume cursor crosses a held-unserved line — the catch-up
// advances to the mark without serving the back-fill (merged_bug_205's
// finalize-at-gap inverse). resumeCursorSound MUST falsify.
module logServiceCalibResumePastGap {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 1,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = true,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// LIVE: the execution axis. The live stream can retag to a new
// execution (a retry on another worker, numbering restarted); the
// reader's drain takes the switch arm — explicit disclosure, cursor
// reset, never a silent swallow.
module logServiceExecAxis {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 2,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = true,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN of logServiceExecAxis: the splice — a foreign-execution row
// drains through the OLD cursor and its low numbering vanishes as a
// "duplicate" (merged_bug_002's exact swallow).
// noCrossExecRowInterleave MUST falsify.
module logServiceCalibExecSplice {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 2,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = true,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = true,
    QUEUE_CAP = 2,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = true,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = true,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// LIVE: the admission-bound (flood) regime. MAX_BATCH_LINES shrinks to
// 1 so a two-line honest batch hits the oversized arm — per-batch
// reject, stream open, nothing buffered. The reader plane is off (an
// ingest-plane probe); the conservation totality is deliberately
// absent (a rejected batch's lines are wire-dropped by construction —
// the real builder's flush bound sits orders below the cap, pinned by
// the rio-builder const-assert).
module logServiceFlood {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 1,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// TWIN of logServiceFlood: the oversized arm is skipped — the
// over-bound batch is ADMITTED whole. batchLinesBounded MUST falsify.
module logServiceCalibOversized {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 1,
    CALIB_OVERSIZED_ADMITTED = true,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// CALIBRATION (expect-violation): the execution-row GC without the
// artifact-before-row conjuncts (merged_bug_007 pre-fix). The reclaim
// fires while drv_log_chunks rows still key on the execution —
// noOrphanLogChunks falsifies (the chunks are orphaned forever; the
// store's TTL sweep can no longer find them through the row).
// =========================================================================
module logServiceCalibGcRowIgnoresArtifacts {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    MAX_BATCH_LINES = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = true,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// CALIBRATION (composed): the pre-fix assignment close that stamps
// nothing (bug_047). The same prefix as the sweep regime's
// close-then-sweep witness, but the close leaves NoCount — after
// expiry and ledger release the row is STILL ineligible: the immortal
// execution row, demonstrated deterministically.
// =========================================================================
module logServiceCalibCloseNoStamp {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    MAX_BATCH_LINES = 3,
    ENABLE_FABRICATION = false,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = true,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = true,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = true,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The immortality, step by step: closed (no stamp), expired,
  /// released — and STILL not eligible; the row survives. Mirrors the
  /// pre-fix red: gc_exec_rows returned 0 for the aged closed row.
  run closeNoStampImmortalRun =
    init
      .then(dispatchMaterialization)
      .then(closeExecStamp(1))
      // The pre-fix close stamped nothing.
      .expect(dbFinalCount.get(1) == NoCount)
      .then(executionExpires(1))
      .then(ledgerReleases(1))
      .expect(all {
        execRowExists.get(1),
        not(execRowSweepEligible(1)),
      })
}

// =========================================================================
// THE PRODUCER REGIME (bughunt2-wave slot 8, bug_241): the base
// lifecycle plus the early uploader death. The adversarial schedule is
// the death racing production: lines produced past the refusal
// watermark exist only in the DiscardLedger's accounting, and the
// build-exit Drop must disclose every one of them
// (producerLossCounted). ingestLossCounted is co-verified: the death
// path's own un-acked disclosure (counted: true) must keep the ingest
// plane's law intact. Constants are the base regime's; only the plane
// toggles.
// =========================================================================
module logServiceProducerLoss {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = true,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = true,
    CALIB_PRODUCER_BLIND = false,
  ).*

  /// The bug_241 narrative, end to end: the uploader dies mid-build,
  /// the build keeps producing (the refused lines), and the exit Drop
  /// discloses exactly the refused range while the death's own
  /// disclosure covered the accepted-but-un-acked range.
  run producerDeathDisclosedRun =
    init
      .then(dispatch)
      .then(produceLine(1))
      .then(uploaderDies(1))
      .then(produceLine(1))
      .then(produceLine(1))
      .then(buildFinishes(1))
      .expect(all {
        refusedFrom.get(1) == 1,
        producedEnd.get(1) == 3,
        producerCountedBelow.get(1) == 3,
        uploader.get(1) == Abandoned({ counted: true }),
      })
}

// =========================================================================
// bug_241 pre-fix calibration: the producer-blind world -- the
// build-exit Drop does NOT disclose (no DiscardLedger; the bounced
// batches vanish, the production red's zero counter increments).
// producerLossCounted falsifies. This pin is the non-vacuity evidence
// for logServiceProducerLoss: if it ever stops falsifying, the model
// has drifted away from the silent-discard class UploadSink::Lost
// exists to make unrepresentable.
// =========================================================================
module logServiceCalibProducerBlind {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = true,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = false,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = true,
    CALIB_PRODUCER_BLIND = true,
  ).*
}

// =========================================================================
// CALIBRATION: the pre-merged_bug_005 unclamped ack producer (bughunt-13
// F9) — the ack value is the committed chunk's own end, never clamped to
// the contiguous durable frontier. Under fabrication, a high-numbered
// chunk past a manifest hole is acked at its end and the builder's
// prefix-pop trim destroys the real low-numbered lines' only copy:
// `ackImpliesDurable` falsifies. The live regimes assert the invariant
// everywhere (fabrication included) against the clamped `deliverAck`;
// if this pin ever stops falsifying, the model has drifted away from
// the hole-spanning-ack class the clamp exists to make unrepresentable.
// =========================================================================
module logServiceCalibUnclampedAck {
  import logService(
    MAX_EXECS = 1,
    MAX_BUILD_EXECS = 1,
    MAX_SESSIONS = 1,
    MAX_LINE = 3,
    ENABLE_FABRICATION = true,
    ENABLE_RECONNECT = false,
    ENABLE_SWEEP = false,
    CAP = 9,
    CALIB_SESSION_SCOPED_CAPS = false,
    CALIB_UNCLAMPED_ACK = true,
    CALIB_GATE_IGNORES_ROW = false,
    ENABLE_MAT_DISPATCH = false,
    CALIB_GATE_LATEST_ONLY = false,
    CALIB_LOSS_ESCAPE = false,
    CALIB_SWEEP_AGE_ONLY = false,
    CALIB_GC_ROW_IGNORE_ARTIFACTS = false,
    CALIB_CLOSE_NO_STAMP = false,
    ENABLE_READER = false,
    QUEUE_CAP = 0,
    CALIB_READER_ADVANCE_PAST = false,
    ENABLE_WORKER_GAP = false,
    ENABLE_DROP_WAKE = false,
    ENABLE_EXEC_AXIS = false,
    MAX_BATCH_LINES = 3,
    CALIB_OVERSIZED_ADMITTED = false,
    CALIB_RECOVERY_UNGATED = false,
    CALIB_STAMP_SKIPS_CATCHUP = false,
    CALIB_EXEC_SPLICE = false,
    CALIB_DROP_WAKE_LOST = false,
    CALIB_RESUME_PAST_GAP = false,
    ENABLE_PRODUCER_PLANE = false,
    CALIB_PRODUCER_BLIND = false,
  ).*
}

// =========================================================================
// bughunt-4 S6a: the bilateral ingest-liveness contract (merged_bug_335 /
// #5-S Q1) and the attach-hello handshake (merged_bug_067).
//
// Self-contained plane — deliberately NOT an import of `logService`: the
// base module's const frame is instantiated by every calibration module
// above, so extending it would touch all of them for a dimension none of
// them explores (the override-frame hazard). The two laws here compose
// with the base model only through the session-open/aborted booleans,
// which this plane owns outright.
//
// Small-int mirror of rio_common::liveness: period 2 x margin 2 < abort
// 5 — the same strict relation the const pair's conformance test pins
// (period x margin < abort), at model scale.
// =========================================================================
module logIngestLiveness {
  // Whether the builder-side keepalive producer exists. true = the
  // post-fix uploader (rio-builder log_upload.rs drive()'s keepalive
  // arm); false = the pre-fix world the quiet-churn twin witnesses.
  const PRODUCER_ENABLED: bool
  // Whether the follow-attach emits the exec-stamped hello. false =
  // the pre-fix silent attach the second twin witnesses.
  const HELLO_ENABLED: bool

  pure val UPLOADER_KEEPALIVE_PERIOD: int = 2
  pure val KEEPALIVE_SAFETY_MARGIN: int = 2
  pure val INBOUND_IDLE_ABORT: int = 5
  pure val MAX_TICKS: int = 12

  // --- the parked-session plane (Q1)
  var inboundSilence: int   // store's view: ticks since last inbound frame
  var sinceKeepalive: int   // producer's own period timer
  var sessionOpen: bool
  var idleAborted: bool
  var ticks: int

  // --- the attach plane (m067)
  var servingExec: int      // 1, then 2 after the execution switch
  var attached: bool
  var helloExec: int        // exec stamp the attached observer holds; 0 = none
  var fanoutSeen: bool

  // The conformance relation the rio-common test enforces, restated at
  // model scale so a future re-tuning of the mirror constants cannot
  // silently break the regime this plane explores.
  pure val mirrorConforms: bool =
    UPLOADER_KEEPALIVE_PERIOD * KEEPALIVE_SAFETY_MARGIN < INBOUND_IDLE_ABORT

  pure def keepaliveFires(sk: int): bool =
    PRODUCER_ENABLED and (sk + 1 >= UPLOADER_KEEPALIVE_PERIOD)

  action init = all {
    inboundSilence' = 0,
    sinceKeepalive' = 0,
    sessionOpen' = true,
    idleAborted' = false,
    ticks' = 0,
    servingExec' = 1,
    attached' = false,
    helloExec' = 0,
    fanoutSeen' = false,
  }

  // One quiet tick on an open, empty-buffer session: time advances; the
  // producer (when present) emits its empty keepalive batch when its
  // period elapses — which IS inbound traffic at the store.
  action quietTick = all {
    ticks < MAX_TICKS,
    sessionOpen,
    not(idleAborted),
    ticks' = ticks + 1,
    sinceKeepalive' = if (keepaliveFires(sinceKeepalive)) 0 else sinceKeepalive + 1,
    inboundSilence' = if (keepaliveFires(sinceKeepalive)) 0 else inboundSilence + 1,
    sessionOpen' = sessionOpen,
    idleAborted' = idleAborted,
    servingExec' = servingExec,
    attached' = attached,
    helloExec' = helloExec,
    fanoutSeen' = fanoutSeen,
  }

  // The store's enforcement arm: empty buffer + inbound silence at the
  // bound => abort (the heartbeat-tick arm in service.rs).
  action idleAbortFire = all {
    sessionOpen,
    not(idleAborted),
    inboundSilence >= INBOUND_IDLE_ABORT,
    idleAborted' = true,
    sessionOpen' = false,
    inboundSilence' = inboundSilence,
    sinceKeepalive' = sinceKeepalive,
    ticks' = ticks,
    servingExec' = servingExec,
    attached' = attached,
    helloExec' = helloExec,
    fanoutSeen' = fanoutSeen,
  }

  // The execution switch: a retry replaces the serving execution; any
  // existing attachment is severed (its follow stream ended).
  action execSwitch = all {
    servingExec == 1,
    servingExec' = 2,
    attached' = false,
    fanoutSeen' = false,
    helloExec' = helloExec,
    inboundSilence' = inboundSilence,
    sinceKeepalive' = sinceKeepalive,
    sessionOpen' = sessionOpen,
    idleAborted' = idleAborted,
    ticks' = ticks,
  }

  // A follow attach: with the hello, the observer learns the serving
  // execution's identity AT attach time; without it (pre-fix), the
  // observer keeps whatever stamp it last held — a reconnect across an
  // exec switch shows the dead execution as streaming.
  action followAttach = all {
    not(attached),
    attached' = true,
    fanoutSeen' = false,
    helloExec' = if (HELLO_ENABLED) servingExec else helloExec,
    inboundSilence' = inboundSilence,
    sinceKeepalive' = sinceKeepalive,
    sessionOpen' = sessionOpen,
    idleAborted' = idleAborted,
    ticks' = ticks,
    servingExec' = servingExec,
  }

  // Live fan-out delivery to an attached observer.
  action fanoutDeliver = all {
    attached,
    fanoutSeen' = true,
    attached' = attached,
    helloExec' = helloExec,
    inboundSilence' = inboundSilence,
    sinceKeepalive' = sinceKeepalive,
    sessionOpen' = sessionOpen,
    idleAborted' = idleAborted,
    ticks' = ticks,
    servingExec' = servingExec,
  }

  action step = any {
    quietTick,
    idleAbortFire,
    execSwitch,
    followAttach,
    fanoutDeliver,
  }

  // --- the laws ---

  // Q1: a conformant producer is never idle-aborted — the conformance
  // relation makes inboundSilence unable to reach the bound.
  val conformantNeverIdleAborted = PRODUCER_ENABLED implies not(idleAborted)

  // m067: an attached observer always holds the SERVING execution's
  // stamp (the hello is what delivers it at attach time).
  val attachAlwaysIdentified = attached implies (helloExec == servingExec)

  // Fan-out is only ever observed by an observer the hello identified.
  val helloPrecedesFanout = fanoutSeen implies (helloExec == servingExec)

  // Exploration bounds.
  val boundsOK = and {
    ticks <= MAX_TICKS,
    inboundSilence <= MAX_TICKS,
    sinceKeepalive <= MAX_TICKS,
    servingExec >= 1,
    servingExec <= 2,
  }

  // --- twin witnesses (the "interesting thing never happens" vals the
  // expect-violation checks run; a [violation] verdict is the proof the
  // regime still reaches the defect) ---

  // Pre-fix churn (PRODUCER_ENABLED = false): the quiet session is
  // idle-aborted — this val is violated.
  val quietSessionStaysOpen = not(idleAborted)

  // Pre-fix silent attach (HELLO_ENABLED = false): an attach across the
  // exec switch leaves the observer holding the OLD stamp — violated.
  val observerNeverStale = attached implies (helloExec == servingExec)
}

// The live (post-fix) instance: both sides of the bilateral contract
// present, the hello emitted. All four laws hold exhaustively.
module logIngestLivenessLive {
  import logIngestLiveness(
    PRODUCER_ENABLED = true,
    HELLO_ENABLED = true,
  ).*
}

// CALIBRATION: the pre-fix producer-less world (merged_bug_335). The
// store's enforcement exists but no keepalive producer does — the
// quiet-build abort churn is reachable: quietSessionStaysOpen [violation].
module logIngestCalibQuietChurn {
  import logIngestLiveness(
    PRODUCER_ENABLED = false,
    HELLO_ENABLED = true,
  ).*
}

// CALIBRATION: the pre-fix silent attach (merged_bug_067). No hello —
// a follow attach across the execution switch leaves the observer
// holding the dead execution's stamp: observerNeverStale [violation].
module logIngestCalibSilentAttach {
  import logIngestLiveness(
    PRODUCER_ENABLED = true,
    HELLO_ENABLED = false,
  ).*
}
