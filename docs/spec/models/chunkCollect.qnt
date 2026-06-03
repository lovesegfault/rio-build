// -*- mode: Bluespec; -*-
//
// REPLACEMENT model of the rio-store chunk-liveness subsystem: lazy
// mark-and-collect over the durable manifests (refcount-formal-design.md
// §4, the counter-free end state), derived from the AS-BUILT model
// `chunkLiveness.qnt` per design §4.6 / plan T-1a.5 (P10: sibling file,
// shared invariant text; the as-built model is not re-targeted).
//
// WHAT THIS MODELS (the design's Release-A/B target shape, anchored to
// the code that already exists on this branch)
//   - The chunked write-ahead uploader state machine, kept as PATH-ROW
//     state only (inventory §3.3 item 4 / design §4.2): claim
//     (`ingest::claim_placeholder`), the upgrade transaction
//     (`metadata::chunked::upgrade_manifest_to_chunked` — manifest_data
//     INSERT + chunk-row upsert + `last_referenced_at = now()` touch on
//     conflict, one tx), the S3 PUT fan-out (`cas::do_upload`), the
//     presence commit (`metadata::chunked::mark_chunks_uploaded`, WITH
//     the `AND deleted = FALSE` late-mark guard), the claim-gated
//     completion, the claim-gated reap (`gc::orphan::reap_one`), the
//     heartbeat (`cas::heartbeat_uploading`), and the hot-path / scanner
//     stale reclaims — all WITHOUT chunk-side effects: no counter, no
//     decrement, no token. The token-gated rollback (DEC-1) does not
//     exist here (design §4.3 deletes it; an in-process failure routes
//     to the claim-gated reap).
//   - The collector of `rio-store/src/gc/collect.rs`, as three separate
//     interleavable actions per design §3.2/§4.6 (one action per SQL
//     transaction; the mark scan and each collect batch are different
//     transactions):
//       collectSnapshot — `collect_cycle`'s cutoff query
//         (`SELECT now() - make_interval(secs => grace)`): the
//         eligibility cutoff is anchored once per cycle on the DB clock.
//       collectMark — the fail-closed validation pass
//         (`mark_validation_sql`) + the server-side set-based expansion
//         (`MARK_EXPANSION_SQL`): any unparseable chunk_list aborts the
//         cycle with no verdict (CollectOutcome::ParseFailure); a clean
//         pass materializes the live set (`live_chunks`) as an explicit
//         snapshot variable, never re-evaluated at sweep time.
//       collectSweep — one live-arm collect batch (design §4.1 step 3 /
//         plan T-1a.8; the orphan-sweep skeleton with the predicate
//         swapped): soft-delete + outbox enqueue of one candidate that
//         is unmarked, not deleted, and whose
//         GREATEST(created_at, last_referenced_at) predates the cycle
//         cutoff. The shipped shadow arm (`CollectMode::Shadow`,
//         T-1a.3) is this same cycle with the sweep effects withheld;
//         modeling the live arm is what the Release-A pre-ship gate is
//         about.
//       collectFinish — the cycle ends (the `CollectReport` returns).
//         The per-cycle victim cap and the cross-cycle keyset cursor
//         (`COLLECT_CYCLE_VICTIM_CAP`, `CollectCursor` in collect.rs)
//         are cycle-scheduling below this model's granularity: actions
//         are enabled, never forced, so a cycle stopping after any
//         number of sweep batches (a capped cycle) is already a
//         reachable behavior of this encoding — design §4.6 v4 note; no
//         cap/cursor state is added.
//     Writer, path-GC, crash, and drain actions interleave between any
//     two collector actions (production holds only the GC advisory lock
//     across the phases, which excludes other collectors, never
//     writers).
//   - `chunks.last_referenced_at` (migration 070) joins the per-chunk
//     state as `refAge` — ticks since the most recent reference event
//     (row insert or upsert touch), the model image of
//     GREATEST(created_at, last_referenced_at). The upgrade action's
//     conflict arm resets it (the touch); nothing else writes it.
//   - The path-GC sweep (`gc::sweep::sweep_one_batch`) deletes manifest
//     rows ONLY — chunk GC is decoupled from path GC (design §4.3); a
//     swept path's chunks wait for the next collect cycle.
//   - The outbox drain (`gc::drain::drain_one_row`) with its re-check
//     narrowed to `deleted` only (the `refcount = 0` conjunct is
//     dropped — design §4.1 step 4); the resurrect skip is unchanged.
//   - The fault alphabet carried over from the as-built model: process
//     death between transactions (C1/C2/C5/C6/C7 by phase), the drain's
//     S3-delete-then-commit-fails window (C10), and the corrupt
//     `chunk_list` (C12) — whose polarity here is the fail-closed mark
//     abort (design §4.4), not a decrement skip. The corrupt regime
//     additionally carries the operator quarantine of adjudication 7.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - Everything the as-built model already excluded and the
//     replacement does not change: the path-level reachability GC (G4b,
//     an abstract environment choice), PG row-lock order (G6), the S3
//     PUT fan-out's per-chunk granularity, inline uploads, the outbox
//     attempts counter / parked-row alerting tail, reader paths, and
//     spurious transaction failure. See chunkLiveness.qnt's header.
//   - The collector-existence / backstop-cadence obligations (a missing
//     background loop) — carried by runtime metrics and alerts
//     (`rio_store_gc_collect_cycles_total`, the stalled alert), not by
//     this model (invariant map, Phase-1 input list).
//   - The per-cycle victim cap value and the cursor mechanics: scheduling
//     below the transition relation's granularity (capped stop ≡
//     collectFinish before the eligible set is exhausted) — §4.6 v4.
//   - The in-flight-overlap row-lock blocking (a collect batch waiting
//     on a writer's chunk-row lock and re-evaluating under READ
//     COMMITTED) — folded into the interleaving semantics exactly as
//     the as-built model folds it (the named assumption of §3.2); the
//     writer-transaction-duration term it leaves open is exercised by
//     the split-upgrade override regimes (ENABLE_SPLIT_UPGRADE) and
//     carried in production as the §4.1 monitored assumption
//     (`rio_store_chunk_upgrade_tx_seconds` + alert).
//
// ENCODING DECISIONS (beyond those inherited from chunkLiveness.qnt)
//   - The cycle cutoff is a per-cycle SET snapshot, not an absolute
//     timestamp: `eligibleAtSnap` records which rows were already past
//     grace at the snapshot instant (refAge > GRACE_TICKS), and
//     `touchedSinceSnap` records which rows a later reference event
//     re-protected (their GREATEST moved to or past the snapshot, hence
//     past the cutoff). A sweep batch may collect h iff h is in the
//     former and not in the latter (and unmarked, present, not yet
//     deleted) — exactly the SQL predicate of design §4.1 step 3
//     evaluated per batch against the fixed cutoff. Fresh inserts after
//     the snapshot are never in `eligibleAtSnap` (their created_at
//     postdates the cutoff).
//   - The fail-closed switch (ENABLE_FAIL_CLOSED), the upsert touch
//     (ENABLE_TOUCH), the late-mark deleted-guard
//     (ENABLE_MARK_DELETED_GUARD), the heartbeat contract
//     (ENABLE_HB_CONTRACT), and the split (non-atomic) upgrade
//     transaction (ENABLE_SPLIT_UPGRADE + WRITER_TX_BOUND) are module
//     constants so each §4.6 falsification pair is two thin
//     instantiation modules of THIS transition relation differing in
//     exactly one constant — the model diff is the design diff. The
//     main regimes set every guard ON (the as-designed collector); each
//     *Falsify module flips exactly one OFF.
//   - The split upgrade (ENABLE_SPLIT_UPGRADE): `upgradeBegin` captures
//     the needs-upload verdict and pins the touch instant (PG `now()`
//     is the transaction start time); `upgradeCommit` makes the
//     manifest_data row and the chunk upsert durable with the touch
//     BACKDATED by the transaction's age. The writer-transaction bound
//     (§4.1 collect-soundness condition) is an action precondition on
//     `tick` — time cannot advance an open upgrade transaction past
//     WRITER_TX_BOUND — so the bounded module proves CR-1 for every
//     interleaving under the bound and the overrun module (bound one
//     tick past grace) is the required falsification. Main regimes
//     disable the split entirely (the atomic action is the
//     production-shaped fast path); `txAge`/`sinceSnap` then stay 0 and
//     cost no states.
//   - Witness/history flags (`wit`) are model-only bookkeeping written
//     by actions and read only by invariants and witnesses, never by
//     guards.
//
// The invariants, their witnesses, and the per-regime non-vacuity
// procedures are documented at their definitions below. Measured state
// counts, depths, and wall-clocks live in the introducing commits'
// messages and the CI checks' output transcripts, never in this file.
module chunkCollect {
  // --- 1. types ---------------------------------------------------------

  /// Lifecycle of the `manifests` row for one store path (unchanged
  /// from the as-built model).
  type PStatus = | PNone | PUploading | PComplete

  /// `manifest_data.chunk_list` for one path (deduped hash set; NoMd
  /// before the upgrade wrote it). Unchanged from the as-built model.
  type MdOpt = | NoMd | Md(Set[int])

  /// One store path's durable state (unchanged from the as-built
  /// model): manifests row status, current placeholder generation,
  /// staleness clock, manifest_data chunk list, and whether that
  /// chunk_list blob is unparseable (the C12 surface — here it drives
  /// the fail-closed validation pass, not a decrement skip).
  type PathRec = {
    status: PStatus,
    gen: int,
    staleness: int,
    mdata: MdOpt,
    corrupt: bool,
  }

  /// One `chunks` row under the replacement schema: present,
  /// confirmed-backend-presence (`uploaded_at IS NOT NULL`), the
  /// soft-delete staging flag, and `refAge` — ticks since the most
  /// recent reference event (row insert or the upsert's
  /// `last_referenced_at = now()` touch), i.e. the model image of
  /// `now() - GREATEST(created_at, last_referenced_at)`, saturating
  /// just past the grace TTL. There is no refcount.
  ///
  /// `deletedAge` (bughunt wave D1, merged_bug_336): ticks since the
  /// soft-delete stamp — the model image of `now() - deleted_at`,
  /// stamped 0 by the collect batch (`deleted_at = now()` in
  /// COLLECT_BATCH_UPDATE_SQL), reset by the resurrect upsert
  /// (`deleted_at = NULL`, metadata/chunked.rs), saturating just past
  /// the reap grace. Meaningful only while `deleted`; 0 otherwise.
  type ChunkRec = {
    present: bool,
    uploadedAt: bool,
    deleted: bool,
    refAge: int,
    deletedAge: int,
  }

  /// Uploader program counter. UTxOpen exists only in the
  /// ENABLE_SPLIT_UPGRADE regimes (an upgrade transaction that has
  /// begun — verdict captured, touch instant pinned — but not yet
  /// committed). URollback does not exist: the token-gated DEC-1
  /// rollback is deleted by the replacement (design §4.3); an
  /// in-process failure routes to the claim-gated reap (UReaping).
  type UPc =
    | UIdle
    | UClaimed
    | UTxOpen
    | UUpgraded
    | UPutDone
    | UMarked
    | UReaping
    | UDone
    | UAborted
    | UCrashed

  /// One uploader process: program counter, claimed path, held claim
  /// (placeholder generation), the chunk list written (or staged) at
  /// upgrade, the needs-upload verdict (`RETURNING (uploaded_at IS
  /// NULL)`), and — split-upgrade regimes only — the open
  /// transaction's age in ticks.
  type UpRec = {
    pc: UPc,
    path: int,
    claim: int,
    list: Set[int],
    needsUpload: Set[int],
    txAge: int,
  }

  /// Collect-cycle phase: no cycle in flight / cutoff snapshotted /
  /// mark materialized (sweep batches may run).
  type CPhase = | CIdle | CSnapped | CMarked

  /// The in-flight collect cycle (`gc::collect::collect_cycle`): the
  /// phase, the mark product (`live_chunks`), the rows already past
  /// grace at the snapshot instant, the rows re-protected by a
  /// reference event since the snapshot, whether any sweep batch ran
  /// this cycle, and (split-upgrade regimes only) ticks since the
  /// snapshot.
  type CycleRec = {
    phase: CPhase,
    markSet: Set[int],
    eligibleAtSnap: Set[int],
    touchedSinceSnap: Set[int],
    swept: bool,
    sinceSnap: int,
  }

  // --- 2. constants (const = overridable per regime) --------------------

  /// Chunk hashes. Three give: a chunk shared by two manifests, a
  /// chunk private to one, and a never-referenced spare.
  const MAX_HASHES: int
  /// Store paths. Two give: sharing across manifests and the
  /// reclaim-then-re-reference races.
  const MAX_PATHS: int
  /// Uploader processes (one-shot each).
  const MAX_UPLOADERS: int
  /// Fault switches (regime knobs, design §3.2): process death + the
  /// drain's C10 window; an existing chunk_list rotting (C12).
  const ENABLE_CRASH: bool
  const ENABLE_CORRUPT: bool
  /// The chunk lists an upgrade may write (the regime's sharing
  /// alphabet).
  const LIST_CHOICES: Set[Set[int]]
  /// Scaled clock thresholds (production: 30 s heartbeat, 300 s
  /// hot-path reclaim, 900 s scanner, 300 s chunk grace). Ordering
  /// HB_DEADLINE < HOTPATH_AFTER <= SCANNER_AFTER and GRACE_TICKS >= 1;
  /// the chunkCollectThresholdOrder regime violates it deliberately.
  const HB_DEADLINE: int
  const HOTPATH_AFTER: int
  const SCANNER_AFTER: int
  const GRACE_TICKS: int
  /// The tombstone-reap grace term in ticks (bughunt wave D1,
  /// merged_bug_336; production CHUNK_GRACE_SECS re-used as the reap
  /// grace — REAP_BATCH_DELETE_SQL's `deleted_at < now() - grace`).
  /// REAP_GRACE_TICKS >= 1, like GRACE_TICKS.
  const REAP_GRACE_TICKS: int
  /// The shadow arm's simulated-sweep exclusion (bughunt wave D1,
  /// bug_199): TRUE = the shipped anti-join (`shadow_swept` temp
  /// table excluded from mark + would-collect), FALSE = the pre-fix
  /// estimate computed against UN-swept state — the
  /// chunkCollectShadowNoExclusion falsification regime. Read only by
  /// the shadow-equivalence invariant below, never by an action: the
  /// dry run has no state effects to model.
  const ENABLE_SHADOW_EXCLUSION: bool
  /// Replacement-mechanism switches (each is a load-bearing guard of
  /// the design; the main regimes set all of them to the as-designed
  /// value and each §4.6 falsification module flips exactly one):
  /// the upsert's `last_referenced_at = now()` touch (design §4.1,
  /// adjudication 6) — flipping this off is the §4.6 (ii) required
  /// falsification;
  const ENABLE_TOUCH: bool
  /// the fail-closed mark (any unparseable chunk_list aborts the
  /// cycle, design §4.4 / adjudication 5) — flipping this off is the
  /// naive skip-the-manifest polarity the design forbids;
  const ENABLE_FAIL_CLOSED: bool
  /// the `AND deleted = FALSE` late-mark guard on
  /// `mark_chunks_uploaded` (plan T-pre.1) — flipped off together with
  /// the heartbeat contract for the late-mark pair;
  const ENABLE_MARK_DELETED_GUARD: bool
  /// the production heartbeat contract (a live, guard-armed owner
  /// heartbeats before the deadline) as a `tick` precondition — the
  /// relaxed-contract regimes drop it;
  const ENABLE_HB_CONTRACT: bool
  /// the split (non-atomic) upgrade transaction and the
  /// writer-transaction duration ceiling, in ticks, enforced as a
  /// `tick` precondition — the §4.1 collect-soundness condition. The
  /// bounded regime sets WRITER_TX_BOUND = GRACE_TICKS (within the
  /// soundness condition); the overrun regime sets GRACE_TICKS + 1
  /// (one tick past it — the required falsification).
  const ENABLE_SPLIT_UPGRADE: bool
  const WRITER_TX_BOUND: int

  // --- 3. pure values / helpers ------------------------------------------

  pure val HASHES = 1.to(MAX_HASHES)
  pure val PATHS = 1.to(MAX_PATHS)
  /// Dummy key 0 (permanently a missing row) so reading
  /// `paths.get(ups.get(u).path)` is defined while an uploader holds no
  /// path (same device as the as-built model).
  pure val PATH_KEYS = 0.to(MAX_PATHS)
  pure val UPLOADERS = 1.to(MAX_UPLOADERS)
  /// Saturation caps: placeholder staleness just past the largest
  /// reclaim threshold; chunk refAge just past the grace TTL; the
  /// cycle's since-snapshot clock just past the largest backdate the
  /// split-upgrade commit can need to discriminate.
  pure val STALE_CAP = SCANNER_AFTER
  pure val AGE_CAP = GRACE_TICKS + 1
  pure val SNAP_CAP = GRACE_TICKS + WRITER_TX_BOUND + 1
  /// deletedAge saturates just past the reap grace.
  pure val REAP_AGE_CAP = REAP_GRACE_TICKS + 1

  pure def minInt(a: int, b: int): int = if (a < b) a else b

  /// An absent chunks row (canonical dead-field values).
  pure val NO_CHUNK: ChunkRec = { present: false, uploadedAt: false, deleted: false, refAge: 0, deletedAge: 0 }
  /// An absent manifests row (gen survives row deletion).
  pure def emptyPath(g: int): PathRec =
    { status: PNone, gen: g, staleness: 0, mdata: NoMd, corrupt: false }
  /// No collect cycle in flight (`collect_cycle` not running).
  pure val IDLE_CYCLE: CycleRec = {
    phase: CIdle,
    markSet: Set(),
    eligibleAtSnap: Set(),
    touchedSinceSnap: Set(),
    swept: false,
    sinceSnap: 0,
  }

  // --- 4. state variables -------------------------------------------------

  /// Per path: the manifests/manifest_data state, keyed 1..MAX_PATHS.
  var paths: int -> PathRec
  /// Per chunk hash: the chunks row (no refcount; `last_referenced_at`
  /// folded into refAge), keyed 1..MAX_HASHES.
  var chunks: int -> ChunkRec
  /// The chunk-backend object set (S3).
  var s3: Set[int]
  /// `pending_s3_deletes`, as the set of enqueued hashes.
  var outbox: Set[int]
  /// Per uploader process: the in-flight write-ahead state machine.
  var ups: int -> UpRec
  /// The in-flight collect cycle (`gc::collect::collect_cycle` runs at
  /// most one at a time — GC advisory lock).
  var cycle: CycleRec
  /// History flags for witnesses and action-form invariants
  /// (model-only; never read by guards).
  ///   - deletedWhileReferenced: a backend DeleteObject was issued for
  ///     a hash some existing manifest referenced at that instant
  ///     (CR-1's action form).
  ///   - skippedPutWithoutPresence: an upgrade's needs-upload verdict
  ///     excluded a chunk whose uploaded_at was NULL (CR-4 regression
  ///     guard; structurally impossible as encoded).
  ///   - referencedChunkSwept: a collect batch soft-deleted a chunk
  ///     that some existing manifest referenced at that instant (the
  ///     fail-closed / mark-correctness consequence sensor, one step
  ///     before CR-1's irreversible harm).
  ///   - liveOwnerReaped: a stale reclaim deleted a placeholder whose
  ///     owner was alive in a guard-armed phase (S5).
  ///   - lateCleanupNoop: a claim-gated cleanup found a foreign or
  ///     missing row and no-opped (the G1 contention arose).
  ///   - drainSkippedResurrected: the drain re-check found a
  ///     resurrected chunk and skipped the S3 delete (G4a defense).
  ///   - hotpathReaped / scannerReaped: the stale-reclaim repair paths
  ///     fired.
  ///   - heartbeatReset: a heartbeat reset a non-zero staleness.
  ///   - s3Deleted: some backend DeleteObject was issued.
  ///   - chunkCollected: some collect batch soft-deleted + enqueued a
  ///     chunk (the collector actually collects — non-vacuity).
  ///   - parseFailureAborted: a collect cycle aborted on the
  ///     fail-closed validation pass (the alert image).
  ///   - quarantined: the operator quarantine of adjudication 7 fired.
  ///   - markMissSavedByTouch: a completed cycle swept at least one
  ///     batch while an unmarked, still-referenced chunk that was
  ///     already past grace at the snapshot was retained ONLY by its
  ///     post-snapshot touch — the §4.6 mark-stale reachability state.
  ///   - tombstoneReaped: the post-pass reap hard-deleted a tombstone
  ///     row (merged_bug_336 non-vacuity — the pre-fix model had no
  ///     delete action at all; rows were permanent).
  ///   - reapedUnsafely: a reap fired against a row that was live
  ///     (`deleted = FALSE`), still pending drain (outbox row), or
  ///     still referenced by an existing manifest — reapSafety's
  ///     action-form sensor (the first two are statically excluded by
  ///     the action's own predicate, mirroring the single-statement
  ///     SQL; the referenced case is what the interleavings probe).
  var wit: {
    deletedWhileReferenced: bool,
    skippedPutWithoutPresence: bool,
    referencedChunkSwept: bool,
    liveOwnerReaped: bool,
    lateCleanupNoop: bool,
    drainSkippedResurrected: bool,
    hotpathReaped: bool,
    scannerReaped: bool,
    heartbeatReset: bool,
    s3Deleted: bool,
    chunkCollected: bool,
    parseFailureAborted: bool,
    quarantined: bool,
    markMissSavedByTouch: bool,
    tombstoneReaped: bool,
    reapedUnsafely: bool,
  }

  // --- 5. derived state (the ghost truth and the code's predicates) ------

  /// The chunk list of path `p` as the model (ghost) knows it,
  /// regardless of parseability.
  def listOf(p: int): Set[int] =
    match paths.get(p).mdata { | NoMd => Set() | Md(l) => l }

  /// `refs(h)`: the manifest fold — every existing manifest (any
  /// status) whose chunk_list references `h`. Under the replacement
  /// this is no longer mirrored by any stored counter; it is the
  /// quantity the mark phase recomputes per cycle
  /// (store.chunk.liveness-derived).
  def refs(h: int): Set[int] =
    PATHS.filter(p => paths.get(p).status != PNone and listOf(p).contains(h))

  /// The same fold restricted to 'complete' manifests — what readers
  /// can resolve (CR-1's state-form head).
  def completeRefs(h: int): Set[int] =
    PATHS.filter(p => paths.get(p).status == PComplete and listOf(p).contains(h))

  /// Does any existing manifest carry an unparseable chunk_list? This
  /// is what `mark_validation_sql()` counts; a non-zero count aborts
  /// the cycle (collect.rs::collect_cycle, the fail-closed arm).
  def corruptManifestExists: bool =
    PATHS.exists(p => paths.get(p).status != PNone and paths.get(p).corrupt)

  /// The hot-path stale-reclaim predicate (`ingest::claim_placeholder`
  /// step 3 → `reap_one(ReapBy::Stale{..})`).
  def hotpathStale(p: int): bool =
    paths.get(p).status == PUploading and paths.get(p).staleness >= HOTPATH_AFTER

  /// The orphan scanner's predicate (`gc::orphan::scan_once`).
  def scannerStale(p: int): bool =
    paths.get(p).status == PUploading and paths.get(p).staleness >= SCANNER_AFTER

  /// The claim-gated reap admission predicate
  /// (`reap_one(ReapBy::Claim(id))`). Quantified over by S4.
  def claimReapAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// The claim-gated heartbeat admission predicate
  /// (`cas::heartbeat_uploading`). Quantified over by L3.
  def heartbeatAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// The claim-gated completion admission predicate
  /// (`complete_manifest_in_conn`).
  def completeAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// Does some uploader hold the placeholder generation owning path
  /// `p`, in a phase whose heartbeat task production guarantees to be
  /// running? Used by the heartbeat-contract `tick` precondition and
  /// the S5 ghost. UTxOpen counts (the wall-clock guard task runs for
  /// the whole upgrade transaction).
  def ownerLiveForced(p: int): bool =
    UPLOADERS.exists(u =>
      val r = ups.get(u)
      and {
        r.path == p,
        r.claim == paths.get(p).gen,
        Set(UClaimed, UTxOpen, UUpgraded, UPutDone, UMarked).contains(r.pc),
      })

  /// The drain's re-check under the replacement
  /// (`gc::drain::drain_one_row` after Release A drops the
  /// `refcount = 0` conjunct, design §4.1 step 4): `deleted` FOR
  /// UPDATE, with a missing row reading as still-dead.
  def stillDead(h: int): bool =
    val c = chunks.get(h)
    if (not(c.present)) true else c.deleted

  /// The collect predicate of one sweep batch, evaluated against the
  /// cycle's fixed cutoff (design §4.1 step 3): the row exists, is not
  /// already soft-deleted, is absent from the cycle's mark product,
  /// was already past grace at the snapshot instant, and has not been
  /// re-referenced (touched or freshly inserted) since the snapshot.
  /// The last conjunct is the chunk-row lock of the §3.2/§4.1 named
  /// assumption: an open upgrade transaction's upsert statement holds
  /// the row lock, so the batch's soft-delete UPDATE waits for that
  /// writer and re-evaluates its predicate against the post-commit row
  /// — the re-evaluation outcome (protected iff the backdated touch
  /// still postdates the cutoff) is encoded at `upgradeCommit`, and
  /// while the transaction is open the row is simply not collectable.
  def sweepEligible(h: int): bool =
    val c = chunks.get(h)
    and {
      cycle.phase == CMarked,
      c.present,
      not(c.deleted),
      not(cycle.markSet.contains(h)),
      cycle.eligibleAtSnap.contains(h),
      not(cycle.touchedSinceSnap.contains(h)),
      not(UPLOADERS.exists(u => ups.get(u).pc == UTxOpen and ups.get(u).list.contains(h))),
    }

  // --- helpers for the chunk-map arithmetic (pure, used by actions) -----

  /// The chunk upsert for one manifest's deduped hash set
  /// (`upgrade_manifest_to_chunked`, metadata/chunked.rs): INSERT
  /// missing rows (refAge 0 — created_at = now()), and for existing
  /// rows `deleted = false` (the resurrect arm) plus, when the touch is
  /// enabled, `last_referenced_at = now()` (refAge 0). `uploaded_at`
  /// is NOT touched on conflict. `withTouch=false` is the §4.6 (ii)
  /// touch-removal override; `backdate` is the split-upgrade commit's
  /// transaction age (0 for the atomic action — PG `now()` is the
  /// transaction start time, so a long transaction's touch is stale by
  /// its own duration).
  pure def upsertChunks(cm: int -> ChunkRec, list: Set[int], withTouch: bool, backdate: int, ageCap: int): int -> ChunkRec =
    cm.keys().mapBy(h =>
      val c = cm.get(h)
      if (not(list.contains(h))) c
      else if (c.present)
        // The resurrect arm clears the soft-delete AND its stamp
        // (`deleted = false, deleted_at = NULL` — metadata/chunked.rs;
        // the reap-vs-resurrect race order, merged_bug_336).
        { ...c, deleted: false, deletedAge: 0, refAge: if (withTouch) minInt(c.refAge, minInt(backdate, ageCap)) else c.refAge }
      else { present: true, uploadedAt: false, deleted: false, refAge: minInt(backdate, ageCap), deletedAge: 0 })

  // --- 7. invariants ------------------------------------------------------
  //
  // CR-1, CR-2 (structural + carved), CR-4 keep the as-built model's
  // predicate text wherever the state they mention survives the
  // replacement (P10: the model diff is the design diff); CR-3 and the
  // M_023 image do not exist here (no counter — the invariant the
  // replacement makes true by construction and deletes). S4/S5/L3 are
  // carried in path-level form. Spec rules are named in each comment;
  // the r[verify ...] markers live at the check wiring points in
  // nix/quint.nix.

  /// Structural bounds / self-consistency facts the other encodings
  /// rely on (the runtime residual of TypeOK).
  val boundsOK = and {
    HASHES.forall(h =>
      val c = chunks.get(h)
      and {
        c.refAge >= 0 and c.refAge <= AGE_CAP,
        not(c.present) implies (not(c.uploadedAt) and not(c.deleted) and c.refAge == 0),
        c.deleted implies not(c.uploadedAt),
        c.deletedAge >= 0 and c.deletedAge <= REAP_AGE_CAP,
        not(c.deleted) implies c.deletedAge == 0,
      }),
    PATHS.forall(p =>
      val r = paths.get(p)
      and {
        r.gen >= 0 and r.gen <= MAX_UPLOADERS,
        r.staleness >= 0 and r.staleness <= STALE_CAP,
        r.status == PNone implies (r.mdata == NoMd and r.staleness == 0),
      }),
    UPLOADERS.forall(u =>
      val r = ups.get(u)
      and {
        r.needsUpload.subseteq(r.list),
        r.list.subseteq(HASHES),
        r.pc == UIdle implies (r.path == 0 and r.claim == 0 and r.list == Set()),
        r.txAge >= 0 and r.txAge <= WRITER_TX_BOUND,
        (r.pc != UTxOpen) implies r.txAge == 0,
      }),
    and {
      cycle.markSet.subseteq(HASHES),
      cycle.eligibleAtSnap.subseteq(HASHES),
      cycle.touchedSinceSnap.subseteq(HASHES),
      cycle.sinceSnap >= 0 and cycle.sinceSnap <= SNAP_CAP,
      cycle.phase == CIdle implies cycle == IDLE_CYCLE,
      cycle.phase == CSnapped implies (cycle.markSet == Set() and not(cycle.swept)),
    },
  }

  /// CR-1 `NoLiveChunkCollected` (store.chunk.no-live-collect; = S1) —
  /// textually identical to the as-built model. State form: a chunk
  /// referenced by any existing 'complete' manifest has its backend
  /// object present. Action form: a backend DeleteObject is issued
  /// only in states where no existing manifest of any status
  /// references the chunk — recorded by the deletedWhileReferenced
  /// ghost at the two delete sites (drainOne, drainCrashAfterS3).
  /// Under the replacement the mechanism is the mark snapshot + the
  /// last_referenced_at touch + grace + the resurrect/drain re-check
  /// pair; the falsification regimes below remove each in turn.
  val cr1NoLiveChunkCollected = and {
    HASHES.forall(h => completeRefs(h) != Set() implies s3.contains(h)),
    not(wit.deletedWhileReferenced),
  }

  /// CR-2 `BoundedGarbageRetention`
  /// (store.gc.bounded-garbage-retention; = L1+L2), replacement
  /// structural form. The wall-clock half of the rule's bound (cycle
  /// cadence, the capped-cycle backlog drain of design §4.1 step 3 v4,
  /// drain lag) is background-loop scheduling carried by runtime
  /// metrics and alerts, not model state (invariant map, Phase-1 input
  /// list); what the protocol must guarantee is that garbage never
  /// becomes UNRECLAIMABLE by the standing machinery:
  ///   (a) an existing, unreferenced, not-yet-soft-deleted chunk is
  ///       never excluded from future collection by anything durable.
  ///       Because liveness is recomputed from the manifests each
  ///       cycle, the only durable exclusion the replacement has is
  ///       the fail-closed pause while an unparseable chunk_list
  ///       exists (design §4.4) — so the unconditional clause is
  ///       "garbage implies no existing manifest is corrupt". In the
  ///       fault-free regimes the clause is discharged by construction
  ///       (no corruption is reachable); in the corrupt regime it is
  ///       the pre-registered expect-violation (the §4.4 trade made
  ///       checkable), and the carved form below is that regime's
  ///       stated invariant.
  ///   (b) a soft-deleted chunk whose backend object still exists is
  ///       enqueued in the outbox — the irreversible step remains
  ///       scheduled (the drain-lag tail of the bound). Identical to
  ///       the as-built clause.
  val cr2NoStrandedGarbage = HASHES.forall(h =>
    val c = chunks.get(h)
    and {
      (c.present and refs(h) == Set() and not(c.deleted)) implies not(corruptManifestExists),
      (c.deleted and s3.contains(h)) implies outbox.contains(h),
    })

  /// CR-2, corrupt-regime carved form (the rule's carve-out clause):
  /// while an unparseable chunk_list exists on an existing manifest,
  /// chunk collection is suspended system-wide (fail-closed), so
  /// garbage MAY persist for the duration — the carve-out is bounded
  /// by detection (the parse-failure alert; its model image is the
  /// parseFailureAborted ghost and the corrupt flag itself) and by the
  /// remediation of adjudication 7, whose resumption-within-one-cycle
  /// half is pinned by the corrupt regime's quarantineResumeRun. What
  /// must still hold unconditionally is the outbox clause: corruption
  /// never strands an already-soft-deleted object.
  val cr2CarvedCorrupt = HASHES.forall(h =>
    val c = chunks.get(h)
    (c.deleted and s3.contains(h)) implies outbox.contains(h))

  /// CR-4 `PresenceNeverInferredFromCounter`
  /// (store.chunk.liveness-not-presence, store.cas.upsert-inserted+2,
  /// store.cas.chunk-upload-committed; = S3) — textually identical to
  /// the as-built model: (a) a writer skips the backend PUT only for
  /// chunks whose uploaded_at is set; (b) uploaded_at is set only
  /// while the backend object is present. The liveness signal (the
  /// mark fold here, the counter as-built) is never consulted for
  /// presence.
  val cr4PresenceFromConfirmedUpload = and {
    not(wit.skippedPutWithoutPresence),
    HASHES.forall(h => chunks.get(h).uploadedAt implies s3.contains(h)),
  }

  /// S4 `OwnerOnlyMutation` (store.put.placeholder-claim+2,
  /// store.put.drop-cleanup+2), path-level form: the claim-gated reap
  /// — the one owner-credentialed destructive path the replacement
  /// keeps — takes effect only against the placeholder generation that
  /// issued the credential. (The token-gated rollback arm of the
  /// as-built form is gone with DEC-1.)
  val s4OwnerOnlyMutation = UPLOADERS.forall(u =>
    val r = ups.get(u)
    (r.pc == UReaping and r.path != 0 and claimReapAdmitted(r.path, r.claim)) implies
      paths.get(r.path).gen == r.claim)

  /// S5 `LiveOwnerNeverReaped` (store.gc.orphan-heartbeat,
  /// store.put.stale-reclaim / store.substitute.stale-reclaim),
  /// path-level form — textually identical to the as-built model.
  val s5LiveOwnerNeverReaped = not(wit.liveOwnerReaped)

  /// L3 support `NoForeignFreshen` (store.put.placeholder-claim+2's
  /// heartbeat clause) — textually identical to the as-built model.
  val l3NoForeignFreshen = UPLOADERS.forall(u =>
    val r = ups.get(u)
    (r.path != 0 and heartbeatAdmitted(r.path, r.claim)) implies paths.get(r.path).gen == r.claim)

  /// Replacement-specific safety sensor, one step before CR-1's
  /// irreversible harm: no collect batch ever soft-deletes a chunk
  /// that some existing manifest references at that instant (the
  /// ghost is recorded at the collectSweep site). This is the
  /// consequence-level form of "no chunk is collected in a cycle that
  /// observed a parse failure" (design §4.4) and of the
  /// quarantine-safety obligation (design §4.6 (a)): a mark that
  /// omitted a parseable manifest's chunks, a sweep that ran despite a
  /// validation failure, or a quarantine that somehow unmarked another
  /// manifest's chunks would all have to soft-delete a referenced
  /// chunk to do harm, and that is exactly what this forbids.
  val noReferencedChunkSwept = not(wit.referencedChunkSwept)

  /// Tombstone-reap safety (bughunt wave D1, merged_bug_336;
  /// store.gc.bounded-garbage-retention+3's row-retention clause): the
  /// post-pass reap never removes a live (`deleted = FALSE`) row, a
  /// row still pending drain (outbox), or a row some existing manifest
  /// references. The first two are the DELETE statement's own
  /// conjuncts (single-statement re-evaluation under the row lock —
  /// the resurrect-vs-reap race order); the referenced case is the
  /// cross-mechanism content the interleavings probe: a referenced row
  /// is only ever `deleted` transiently pre-resurrect, and the reap
  /// grace + the atomic resurrect (deleted = FALSE, deleted_at NULL in
  /// one statement) keep the reap off it. Sensor recorded at the
  /// reapOne site.
  val reapSafety = not(wit.reapedUnsafely)

  // --- shadow-equivalence (bughunt wave D1, bug_199) ----------------------
  //
  // The dry run's chunk estimate must be computed against SIMULATED
  // post-sweep state: the savepoint-rolled-back path sweep leaves the
  // would-be-swept manifests in place, so the shadow arm anti-joins
  // them out of the mark (`shadow_swept` temp table appended to both
  // MARK_EXPANSION_SQL and the validation/would-collect statements).
  // The theorem: for every subset S of currently-sweepable manifests,
  // the anti-join estimate equals the estimate computed over the
  // post-sweep state transform (the manifests in S actually emptied) —
  // i.e. the exclusion IS the simulation. Checked per reachable state,
  // quantified over the powerset; the per-cycle victim cap is
  // scheduling above this equality (both sides are the un-capped
  // eligible sets — "modulo cap"). The pre-fix estimate (no exclusion;
  // ENABLE_SHADOW_EXCLUSION = false) breaks the equality exactly when
  // some swept manifest exclusively referenced past-grace chunks — the
  // chunkCollectShadowNoExclusion falsification regime.

  /// Manifests a dry run's simulated sweep may take (pathSweepOne's
  /// own precondition: only 'complete' manifests are ever swept;
  /// unreachability is an environment choice above this model).
  def sweepableNow: Set[int] = PATHS.filter(p => paths.get(p).status == PComplete)

  /// The mark with the manifests in `excluded` anti-joined out — the
  /// shipped shadow form (collect.rs MARK_EXPANSION_SHADOW_SUFFIX).
  def markOver(excluded: Set[int]): Set[int] =
    HASHES.filter(h => PATHS.exists(p => and {
      not(excluded.contains(p)),
      paths.get(p).status != PNone,
      not(paths.get(p).corrupt),
      listOf(p).contains(h),
    }))

  /// The mark computed over the post-sweep state transform: the
  /// manifests in `swept` actually emptied (pathSweepOne's effect),
  /// then the UNMODIFIED mark formula — what a live sweep-then-collect
  /// at the same snapshot would mark.
  def markOverSimulated(swept: Set[int]): Set[int] =
    val mod = PATH_KEYS.mapBy(p =>
      if (swept.contains(p)) emptyPath(paths.get(p).gen) else paths.get(p))
    HASHES.filter(h => PATHS.exists(p => and {
      mod.get(p).status != PNone,
      not(mod.get(p).corrupt),
      (match mod.get(p).mdata { | NoMd => Set() | Md(l) => l }).contains(h),
    }))

  /// The un-capped victim set for a given mark product: present, not
  /// already soft-deleted, past grace, unmarked (the collect batch
  /// predicate minus the in-cycle scheduling conjuncts).
  def victimsGiven(mark: Set[int]): Set[int] =
    HASHES.filter(h => and {
      chunks.get(h).present,
      not(chunks.get(h).deleted),
      chunks.get(h).refAge > GRACE_TICKS,
      not(mark.contains(h)),
    })

  /// THE shadow-equivalence invariant: for every sweepable subset, the
  /// shadow estimate (anti-join when ENABLE_SHADOW_EXCLUSION, the
  /// pre-fix full mark otherwise) equals the simulated post-sweep
  /// estimate.
  val shadowEstimateMatchesLive =
    sweepableNow.powerset().forall(s =>
      victimsGiven(if (ENABLE_SHADOW_EXCLUSION) markOver(s) else markOver(Set()))
        == victimsGiven(markOverSimulated(s)))

  // --- expect-violation witnesses (non-vacuity probes) --------------------
  //
  // The vals below are NOT invariants and must never appear in a regime
  // check's invariant list. Each states "the interesting thing never
  // happens"; the quint-chunk-collect-witness-* checks verify one each
  // and PASS only when the checker reports a violation.

  /// Violated when a chunked upload completes with every chunk
  /// backend-present and presence-confirmed (CR-1's state form bites).
  val noCompleteUpload = not(PATHS.exists(p => and {
    paths.get(p).status == PComplete,
    listOf(p) != Set(),
    listOf(p).forall(h => s3.contains(h) and chunks.get(h).uploadedAt),
  }))

  /// Violated when a backend DeleteObject has actually been issued.
  val noBackendDelete = not(wit.s3Deleted)

  /// Violated when a referenced chunk exists whose backend presence is
  /// not yet confirmed (refs non-empty, uploaded_at NULL) — the M_033
  /// precondition, restated over the manifest fold instead of the
  /// retired counter.
  val noUnconfirmedReferencedChunk = not(HASHES.exists(h => and {
    chunks.get(h).present,
    refs(h) != Set(),
    not(chunks.get(h).uploadedAt),
  }))

  /// Violated when a heartbeat resets a non-zero staleness.
  val noHeartbeatReset = not(wit.heartbeatReset)

  /// Violated when a collect batch actually soft-deletes + enqueues a
  /// chunk — the collector collects; CR-2's reclamation path and the
  /// noReferencedChunkSwept sensor are non-vacuous.
  val noChunkCollected = not(wit.chunkCollected)

  /// Violated when a full cycle has run at least one sweep batch and
  /// retained an unmarked, still-referenced, past-grace-at-snapshot
  /// chunk ONLY because its referencing upgrade committed after the
  /// snapshot and touched it — the §4.6 mark-stale reachability
  /// witness (i). Without this state being reachable, "CR-1 holds"
  /// says nothing about the race `last_referenced_at` exists to close.
  val noMarkMissSavedByTouch = not(wit.markMissSavedByTouch)

  /// Violated when the post-pass reap actually hard-deletes a
  /// tombstone row (merged_bug_336's liveness witness — the pre-fix
  /// model had no chunks-row delete action at all: soft-deleted rows
  /// were permanent). Without this state being reachable, reapSafety
  /// holds vacuously.
  val noTombstoneReaped = not(wit.tombstoneReaped)

  /// Violated when an uploader has crashed while holding a claimed but
  /// not-yet-upgraded placeholder (C1).
  val noCrashAtClaimed = not(UPLOADERS.exists(u => and {
    ups.get(u).pc == UCrashed,
    ups.get(u).path != 0,
    ups.get(u).list == Set(),
    paths.get(ups.get(u).path).status == PUploading,
    paths.get(ups.get(u).path).gen == ups.get(u).claim,
  }))

  /// Violated when an uploader has crashed after its upgrade committed
  /// (write-ahead manifest durable, upload incomplete) — C2.
  val noCrashAfterUpgrade = not(UPLOADERS.exists(u => and {
    ups.get(u).pc == UCrashed,
    ups.get(u).list != Set(),
    paths.get(ups.get(u).path).status == PUploading,
    paths.get(ups.get(u).path).gen == ups.get(u).claim,
  }))

  /// Violated when an uploader has crashed with its claim-gated reap
  /// still pending (C5, the state C3/C7 collapse onto).
  val noCrashBeforeReap = not(UPLOADERS.exists(u => and {
    ups.get(u).pc == UCrashed,
    ups.get(u).path != 0,
    paths.get(ups.get(u).path).status == PUploading,
    paths.get(ups.get(u).path).gen == ups.get(u).claim,
  }))

  /// Violated when BOTH uploaders have crashed after writing their
  /// manifests (the PutPathBatch C6 shape).
  val noDoubleCrashStaged = not(
    UPLOADERS.filter(u => ups.get(u).pc == UCrashed and ups.get(u).list != Set()).size() >= 2)

  /// Violated when an abandoned chunked upload's garbage is reachable:
  /// an 'uploading' manifest whose owner crashed, its chunks present
  /// and still referenced by it. Under the replacement this garbage is
  /// not "accounted" anywhere — it is simply what the next collect
  /// cycle sees once the reapers delete the manifest row (the design's
  /// "miscount becomes collected one cycle late").
  val noAbandonedUploadGarbage = not(PATHS.exists(p => and {
    paths.get(p).status == PUploading,
    listOf(p) != Set(),
    listOf(p).forall(h => chunks.get(h).present),
    UPLOADERS.exists(u => and {
      ups.get(u).path == p,
      ups.get(u).claim == paths.get(p).gen,
      ups.get(u).pc == UCrashed,
    }),
  }))

  /// Violated when the hot-path stale reclaim fires.
  val noHotpathReclaim = not(wit.hotpathReaped)

  /// Violated when the orphan scanner reaps an abandoned placeholder.
  val noScannerReap = not(wit.scannerReaped)

  /// Violated when the drain re-check skips a resurrected chunk — the
  /// G4a TOCTOU defense at the irreversible step is exercised with the
  /// `deleted`-only re-check.
  val noDrainResurrectSkip = not(wit.drainSkippedResurrected)

  /// Violated when a claim-gated cleanup arrives late and no-ops
  /// against a missing, foreign or refreshed row — the contended state
  /// S4 is about actually arises.
  val noLateCleanupNoop = not(wit.lateCleanupNoop)

  /// Violated when a collect cycle aborts on the fail-closed
  /// validation pass — the parse-failure abort path (design §4.4) is
  /// reachable, so the corrupt regime's fail-closed claims are not
  /// vacuous. Corrupt regime only.
  val noParseFailureAbort = not(wit.parseFailureAborted)

  /// Violated when the operator quarantine of adjudication 7 fires.
  /// Corrupt regime only.
  val noQuarantine = not(wit.quarantined)

  // --- 6. actions ---------------------------------------------------------
  //
  // Each action is one production SQL transaction (or one named
  // non-transactional step: the S3 PUT fan-out, the crash injections,
  // the clock). Preconditions are the code path's guards; effects are
  // its state mutations; the function each mirrors is named in its doc
  // comment.

  /// `metadata::insert_manifest_uploading` via `ingest::claim_placeholder`
  /// (successful-insert arm): mint a fresh claim, insert the
  /// 'uploading' row, arm the drop-guard.
  action claimPath(u: int, p: int): bool = all {
    ups.get(u).pc == UIdle,
    paths.get(p).status == PNone,
    paths' = paths.set(p, {
      status: PUploading,
      gen: paths.get(p).gen + 1,
      staleness: 0,
      mdata: NoMd,
      corrupt: false,
    }),
    ups' = ups.set(u, { ...ups.get(u), pc: UClaimed, path: p, claim: paths.get(p).gen + 1 }),
    chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// `metadata::chunked::upgrade_manifest_to_chunked` via
  /// `cas::stage_chunked` step 2, as ONE atomic action (the production
  /// fast path — the transaction is far shorter than grace; the
  /// split-transaction variant below exercises the §4.1 soundness
  /// condition): FOR UPDATE on the 'uploading' row, manifest_data
  /// INSERT, chunk upsert with `RETURNING (uploaded_at IS NULL) AS
  /// needs_upload` and — when ENABLE_TOUCH — `last_referenced_at =
  /// now()` on conflict (the migration-068 touch; collect.rs's
  /// mark-snapshot race closer). While a collect cycle is in flight,
  /// every hash written here moves out of the cycle's collectable set
  /// (its GREATEST(created_at, last_referenced_at) now postdates the
  /// cycle cutoff) — recorded in touchedSinceSnap.
  action upgradeManifest(u: int, list: Set[int]): bool = {
    val p = ups.get(u).path
    val needs = list.filter(h => not(chunks.get(h).uploadedAt))
    all {
      ups.get(u).pc == UClaimed,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      paths' = paths.set(p, { ...paths.get(p), mdata: Md(list) }),
      chunks' = upsertChunks(chunks, list, ENABLE_TOUCH, 0, AGE_CAP),
      ups' = ups.set(u, { ...ups.get(u), pc: UUpgraded, list: list, needsUpload: needs }),
      cycle' = if (cycle.phase != CIdle and ENABLE_TOUCH)
        { ...cycle, touchedSinceSnap: cycle.touchedSinceSnap.union(list) }
      else cycle,
      wit' = { ...wit, skippedPutWithoutPresence: or {
        wit.skippedPutWithoutPresence,
        list.exists(h => not(needs.contains(h)) and not(chunks.get(h).uploadedAt)),
      } },
      s3' = s3, outbox' = outbox,
    }
  }

  /// Split-upgrade BEGIN (ENABLE_SPLIT_UPGRADE regimes only): the
  /// upgrade transaction opens — `pool.begin()` in
  /// `upgrade_manifest_to_chunked` — and the upsert statement executes:
  /// the needs-upload verdict reflects `uploaded_at` at this instant,
  /// the chunk-row locks are taken (a collect batch reaching these
  /// rows now waits for this transaction — the sweepEligible lock
  /// conjunct), and the touch value is pinned to PG `now()`, which is
  /// the TRANSACTION START time. Nothing is durable (visible to the
  /// mark, the drain, or other writers) until the commit; an abandoned
  /// transaction (abortToReap / crash from UTxOpen) rolls back to no
  /// effect at all.
  action upgradeBegin(u: int, list: Set[int]): bool = {
    val p = ups.get(u).path
    val needs = list.filter(h => not(chunks.get(h).uploadedAt))
    all {
      ENABLE_SPLIT_UPGRADE,
      ups.get(u).pc == UClaimed,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      ups' = ups.set(u, { ...ups.get(u), pc: UTxOpen, list: list, needsUpload: needs, txAge: 0 }),
      wit' = { ...wit, skippedPutWithoutPresence: or {
        wit.skippedPutWithoutPresence,
        list.exists(h => not(needs.contains(h)) and not(chunks.get(h).uploadedAt)),
      } },
      paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle,
    }
  }

  /// Split-upgrade COMMIT: the open transaction's effects become
  /// durable — manifest_data row, chunk rows, and the touch BACKDATED
  /// by the transaction's age (the touch was written as the
  /// transaction's start time). A collect batch that was waiting on
  /// the row lock now re-evaluates its predicate against this
  /// post-commit row (the READ-COMMITTED re-evaluation of the §3.2
  /// named assumption): the hash is re-protected against the in-flight
  /// cycle only if its backdated touch still postdates the cycle
  /// cutoff (txAge <= sinceSnap + grace); a transaction that ran
  /// longer than that leaves the chunk collectable — exactly the §4.1
  /// collect-soundness condition the WRITER_TX_BOUND tick precondition
  /// enforces in the bounded regime and the overrun regime relaxes.
  action upgradeCommit(u: int): bool = {
    val p = ups.get(u).path
    val r = ups.get(u)
    val protected = r.txAge <= cycle.sinceSnap + GRACE_TICKS
    all {
      ENABLE_SPLIT_UPGRADE,
      r.pc == UTxOpen,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      paths' = paths.set(p, { ...paths.get(p), mdata: Md(r.list) }),
      chunks' = upsertChunks(chunks, r.list, true, r.txAge, AGE_CAP),
      ups' = ups.set(u, { ...r, pc: UUpgraded, txAge: 0 }),
      cycle' = if (cycle.phase != CIdle and protected)
        { ...cycle, touchedSinceSnap: cycle.touchedSinceSnap.union(r.list) }
      else cycle,
      s3' = s3, outbox' = outbox, wit' = wit,
    }
  }

  /// `cas::do_upload`: the S3 PUT fan-out for the needs-upload set
  /// (one action; not a SQL transaction).
  action uploadChunks(u: int): bool = all {
    ups.get(u).pc == UUpgraded,
    s3' = s3.union(ups.get(u).needsUpload),
    ups' = ups.set(u, { ...ups.get(u), pc: UPutDone }),
    paths' = paths, chunks' = chunks, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// `metadata::chunked::mark_chunks_uploaded` (one statement): set
  /// uploaded_at for the chunks this uploader PUT, where still NULL
  /// AND — when ENABLE_MARK_DELETED_GUARD, the T-pre.1 late-mark guard
  /// shipped on this branch — not soft-deleted. The
  /// chunkCollectLateMark* regimes exercise the guard's load-bearing
  /// half under a relaxed heartbeat contract.
  action markUploaded(u: int): bool = all {
    ups.get(u).pc == UPutDone,
    chunks' = chunks.keys().mapBy(h =>
      val c = chunks.get(h)
      if (and {
        ups.get(u).needsUpload.contains(h),
        c.present,
        not(ENABLE_MARK_DELETED_GUARD) or not(c.deleted),
      }) { ...c, uploadedAt: true } else c),
    ups' = ups.set(u, { ...ups.get(u), pc: UMarked }),
    paths' = paths, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// `metadata::complete_manifest_chunked` → `complete_manifest_in_conn`
  /// (one transaction): claim-gated status flip to 'complete'.
  action completeManifest(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UMarked,
      completeAdmitted(p, ups.get(u).claim),
      paths' = paths.set(p, { ...paths.get(p), status: PComplete, staleness: 0 }),
      ups' = ups.set(u, { ...ups.get(u), pc: UDone }),
      chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
    }
  }

  /// The completion attempt when the claim no longer matches: no rows
  /// touched, the handler falls through to the claim-gated reap.
  action completeFailsForeign(u: int): bool = all {
    ups.get(u).pc == UMarked,
    not(completeAdmitted(ups.get(u).path, ups.get(u).claim)),
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// The owner abandons the upload (explicit abort, an upload/mark
  /// error, or the drop-guard after the handler future was dropped)
  /// and routes to the claim-gated reap. Under the replacement the
  /// in-process rollback IS this path: DEC-1 and its token are deleted
  /// (design §4.3), so every owner-side failure cleanup is the
  /// claim-gated row deletion.
  action abortToReap(u: int): bool = all {
    Set(UClaimed, UTxOpen, UUpgraded, UPutDone, UMarked).contains(ups.get(u).pc),
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping, txAge: 0 }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// `cas::heartbeat_uploading` (one statement, claim-gated): bump
  /// updated_at for the owner's own 'uploading' row.
  action heartbeat(u: int): bool = {
    val p = ups.get(u).path
    all {
      Set(UClaimed, UTxOpen, UUpgraded, UPutDone, UMarked, UReaping).contains(ups.get(u).pc),
      ups.get(u).path != 0,
      heartbeatAdmitted(p, ups.get(u).claim),
      paths' = paths.set(p, { ...paths.get(p), staleness: 0 }),
      wit' = { ...wit, heartbeatReset: wit.heartbeatReset or paths.get(p).staleness > 0 },
      chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle,
    }
  }

  /// `gc::orphan::reap_one(ReapBy::Claim(claim))` under the
  /// replacement (one transaction): if the 'uploading' row still
  /// carries this claim, DELETE narinfo (CASCADE manifests +
  /// manifest_data) — and NOTHING else: no chunk_list parse, no
  /// decrement, no soft-delete, no enqueue (design §4.3 — reaping a
  /// stale placeholder reverts to "delete the rows; the next collect
  /// cycle notices the chunks"). If the claim does not match: no-op.
  action reapByClaim(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UReaping,
      if (claimReapAdmitted(p, ups.get(u).claim)) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        wit' = { ...wit, lateCleanupNoop: true },
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle,
      },
    }
  }

  /// The shared stale-reclaim transaction body
  /// (`gc::orphan::reap_one(ReapBy::Stale{..})`) under the
  /// replacement: re-check the staleness predicate, DELETE the rows —
  /// and nothing else (the chunk-awareness of the reap paths is
  /// deleted, design §4.3). Used by the hot-path reclaim and the
  /// orphan scanner. The liveOwnerReaped ghost records a reap of a row
  /// whose owner is alive in a guard-armed phase (S5's harm).
  action reapStale(p: int, isHotpath: bool): bool = all {
    if (isHotpath) hotpathStale(p) else scannerStale(p),
    paths' = paths.set(p, emptyPath(paths.get(p).gen)),
    wit' = { ...wit,
      liveOwnerReaped: wit.liveOwnerReaped or ownerLiveForced(p),
      hotpathReaped: wit.hotpathReaped or isHotpath,
      scannerReaped: wit.scannerReaped or not(isHotpath),
    },
    chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle,
  }

  /// The hot-path stale reclaim (`ingest::claim_placeholder` step 3,
  /// I-207), path-row janitor form.
  action hotPathReclaim(p: int): bool = reapStale(p, true)

  /// The background orphan scanner (`gc::orphan::scan_once`), path-row
  /// janitor form.
  action scannerReap(p: int): bool = reapStale(p, false)

  /// One path-GC sweep batch over a single unreachable path
  /// (`gc::sweep::sweep_one_batch`, with path unreachability an
  /// abstract environment choice) under the replacement: DELETE
  /// narinfo (CASCADE manifests + manifest_data) and NOTHING else —
  /// the per-batch chunk handling (parse → decrement → zero-detect →
  /// enqueue) is deleted; chunk GC is decoupled from path GC (design
  /// §4.3, §4.5 Release A). Only 'complete' manifests are ever swept.
  action pathSweepOne(p: int): bool = all {
    paths.get(p).status == PComplete,
    paths' = paths.set(p, emptyPath(paths.get(p).gen)),
    chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle, wit' = wit,
  }

  /// Collect-cycle SNAPSHOT (`gc::collect::collect_cycle`, the cutoff
  /// query `SELECT (now() - make_interval(secs => grace))`): the
  /// eligibility cutoff is anchored once on the DB clock and never
  /// re-evaluated per batch. Model image: record which rows are
  /// already past grace at this instant (their
  /// GREATEST(created_at, last_referenced_at) predates the cutoff).
  /// One cycle at a time (GC_LOCK_ID — run_gc phase 3 and
  /// `collect_backstop_once` both take it).
  action collectSnapshot: bool = all {
    cycle.phase == CIdle,
    cycle' = {
      phase: CSnapped,
      markSet: Set(),
      eligibleAtSnap: HASHES.filter(h => chunks.get(h).present and chunks.get(h).refAge > GRACE_TICKS),
      touchedSinceSnap: Set(),
      swept: false,
      sinceSnap: 0,
    },
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, wit' = wit,
  }

  /// Collect-cycle MARK: the fail-closed validation pass
  /// (`mark_validation_sql()` — version byte, 36-byte alignment,
  /// MAX_CHUNKS, mirroring `try_parse_unique_chunk_hashes`) followed by
  /// the server-side set-based expansion (`MARK_EXPANSION_SQL` →
  /// `live_chunks`). Any unparseable chunk_list on an existing
  /// manifest aborts the cycle: no mark product, no sweep, the
  /// parse-failure counter/alert fires
  /// (`rio_store_gc_collect_parse_failures_total`) — the
  /// retention-erring polarity of design §4.4 / adjudication 5. A
  /// clean pass materializes the live set as an explicit snapshot:
  /// every chunk referenced by any manifest existing AT THIS INSTANT
  /// (any status — 'uploading' placeholders count exactly as their
  /// write-ahead upsert counts them today). With ENABLE_FAIL_CLOSED
  /// false (the forbidden skip polarity, wired only as a falsification
  /// regime) the corrupt manifest is silently skipped instead.
  action collectMark: bool = all {
    cycle.phase == CSnapped,
    if (ENABLE_FAIL_CLOSED and corruptManifestExists) all {
      // Validation failure: abort the cycle, produce no verdict.
      cycle' = IDLE_CYCLE,
      wit' = { ...wit, parseFailureAborted: true },
    } else all {
      cycle' = { ...cycle,
        phase: CMarked,
        markSet: HASHES.filter(h =>
          PATHS.exists(p => and {
            paths.get(p).status != PNone,
            not(paths.get(p).corrupt),
            listOf(p).contains(h),
          })),
      },
      wit' = wit,
    },
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups,
  }

  /// One live-arm collect batch (design §4.1 step 3 / plan T-1a.8;
  /// `sweep_orphan_batch` is the skeleton with the predicate swapped):
  /// one per-batch transaction = keyset-cursor candidate scan
  /// (deleted = FALSE, NOT EXISTS in live_chunks,
  /// GREATEST(created_at, last_referenced_at) < cutoff) + sorted
  /// soft-delete (`SET deleted = TRUE, uploaded_at = NULL`) +
  /// `enqueue_chunk_deletes` into pending_s3_deletes. Modeled at the
  /// most adversarial granularity (one candidate per batch) so writer,
  /// path-GC, drain, and crash actions interleave between batches. The
  /// per-cycle victim cap and the cross-cycle cursor
  /// (`COLLECT_CYCLE_VICTIM_CAP`, `CollectCursor`) are scheduling on
  /// top of this action — a capped stop is collectFinish below. The
  /// shipped shadow arm (`CollectMode::Shadow`, T-1a.3) is this cycle
  /// with these effects withheld.
  action collectSweep(h: int): bool = all {
    sweepEligible(h),
    // `deleted_at = now()` rides the same UPDATE
    // (COLLECT_BATCH_UPDATE_SQL) — the reap clock starts here.
    chunks' = chunks.set(h, { ...chunks.get(h), deleted: true, uploadedAt: false, deletedAge: 0 }),
    outbox' = outbox.union(Set(h)),
    cycle' = { ...cycle, swept: true },
    wit' = { ...wit,
      chunkCollected: true,
      referencedChunkSwept: wit.referencedChunkSwept or refs(h) != Set(),
    },
    paths' = paths, s3' = s3, ups' = ups,
  }

  /// Collect-cycle FINISH: the cycle returns its CollectReport and
  /// releases its session state (the temp table is dropped, the
  /// advisory lock released by the caller). A finish before the
  /// eligible set is exhausted is exactly the capped stop
  /// (`CollectCursor::store`) and a finish after it is the pass
  /// completion (`CollectCursor::reset`) — both are this one action;
  /// the model does not distinguish them (design §4.6 v4 note). The
  /// markMissSavedByTouch ghost records, at cycle end, the §4.6
  /// mark-stale reachability state: this cycle ran at least one sweep
  /// batch while some unmarked, still-referenced chunk that was
  /// already past grace at the snapshot survived ONLY because a
  /// post-snapshot upgrade touched it.
  action collectFinish: bool = all {
    cycle.phase == CMarked,
    wit' = { ...wit, markMissSavedByTouch: or {
      wit.markMissSavedByTouch,
      and {
        cycle.swept,
        HASHES.exists(h => and {
          cycle.eligibleAtSnap.contains(h),
          cycle.touchedSinceSnap.contains(h),
          not(cycle.markSet.contains(h)),
          refs(h) != Set(),
          not(chunks.get(h).deleted),
        }),
      },
    } },
    cycle' = IDLE_CYCLE,
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups,
  }

  /// Operator-confirmed quarantine of a persistently unparseable
  /// manifest (design adjudication 7; the runbook remediation the
  /// parse-failure alert points at): delete that manifest/path row so
  /// the next cycle's validation passes. Forfeits only the corrupt
  /// manifest's own chunks (its path is already unservable — GetPath's
  /// decode uses the same parser); chunks shared with parseable
  /// manifests stay referenced and are never collected (the
  /// quarantine-safety half, checked by CR-1 + noReferencedChunkSwept
  /// over this regime and pinned by quarantineResumeRun). The target
  /// is a persistently corrupt REACHABLE manifest, i.e. a 'complete'
  /// row that path GC will never remove: corrupt 'uploading' rows
  /// self-heal — the reapers delete them without parsing chunk_list
  /// (design §4.4) — and "consecutive failed cycles on the same named
  /// manifest" (the adjudication-7 trigger) cannot accumulate against
  /// a transient placeholder, so the operator action is not modeled
  /// against one. Corrupt regime only.
  action quarantineCorrupt(p: int): bool = all {
    ENABLE_CORRUPT,
    paths.get(p).status == PComplete,
    paths.get(p).corrupt,
    paths' = paths.set(p, emptyPath(paths.get(p).gen)),
    wit' = { ...wit, quarantined: true },
    chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle,
  }

  /// One drain row transaction (`gc::drain::drain_one_row`, success
  /// path) with the Release-A re-check: lock the outbox row, re-check
  /// `deleted ... FOR UPDATE` on the chunk (the `refcount = 0`
  /// conjunct is dropped — its job for the post-snapshot re-reference
  /// trace moved to the upsert touch + grace, design §4.1 step 4 /
  /// §4.2); if still dead, DeleteObject and remove the row; if
  /// resurrected, skip the delete and drop the row.
  action drainOne(h: int): bool = all {
    outbox.contains(h),
    if (stillDead(h)) all {
      s3' = s3.exclude(Set(h)),
      outbox' = outbox.exclude(Set(h)),
      wit' = { ...wit,
        s3Deleted: true,
        deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
      },
      paths' = paths, chunks' = chunks, ups' = ups, cycle' = cycle,
    } else all {
      outbox' = outbox.exclude(Set(h)),
      wit' = { ...wit, drainSkippedResurrected: true },
      paths' = paths, chunks' = chunks, s3' = s3, ups' = ups, cycle' = cycle,
    },
  }

  /// The drain's C10 window: DeleteObject succeeded, the transaction
  /// commit did not — the outbox row survives and is retried.
  /// Crash-regime only.
  action drainCrashAfterS3(h: int): bool = all {
    ENABLE_CRASH,
    outbox.contains(h),
    stillDead(h),
    s3' = s3.exclude(Set(h)),
    wit' = { ...wit,
      s3Deleted: true,
      deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
    },
    paths' = paths, chunks' = chunks, outbox' = outbox, ups' = ups, cycle' = cycle,
  }

  /// One post-pass tombstone-reap batch row (bughunt wave D1,
  /// merged_bug_336; `REAP_BATCH_DELETE_SQL`): hard-DELETE a chunks
  /// row that is soft-deleted at least the reap grace ago and fully
  /// drained (no pending outbox row). One atomic action = the one
  /// DELETE statement — its WHERE re-evaluates under the row lock, so
  /// a resurrect that lands first (deleted = FALSE, deleted_at NULL)
  /// makes the row ineligible, exactly as in PG. Modeled at the most
  /// adversarial granularity: production runs the reap only post-pass
  /// under the cycle lock; the model lets it interleave freely — if
  /// safety holds here it holds under the stricter schedule. After
  /// the reap the hash is an absent row: a later upsert of the same
  /// content is a fresh INSERT (PutPath on an absent hash).
  action reapOne(h: int): bool = all {
    val c = chunks.get(h)
    and {
      c.present,
      c.deleted,
      c.deletedAge > REAP_GRACE_TICKS,
      not(outbox.contains(h)),
    },
    chunks' = chunks.set(h, NO_CHUNK),
    wit' = { ...wit,
      tombstoneReaped: true,
      reapedUnsafely: or {
        wit.reapedUnsafely,
        not(chunks.get(h).deleted),
        outbox.contains(h),
        refs(h) != Set(),
      },
    },
    paths' = paths, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle,
  }

  /// Process death (C1/C2/C5/C6/C7 by phase; UTxOpen = death inside
  /// the upgrade transaction, which therefore never commits).
  /// Crash-regime only.
  action crash(u: int): bool = all {
    ENABLE_CRASH,
    Set(UClaimed, UTxOpen, UUpgraded, UPutDone, UMarked, UReaping).contains(ups.get(u).pc),
    ups' = ups.set(u, { ...ups.get(u), pc: UCrashed, txAge: 0 }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, cycle' = cycle, wit' = wit,
  }

  /// Data damage: an existing manifest_data.chunk_list becomes
  /// unparseable (the C12 surface — under the replacement this is what
  /// the fail-closed validation pass catches). Corrupt-regime only.
  action corruptChunkList(p: int): bool = all {
    ENABLE_CORRUPT,
    paths.get(p).mdata != NoMd,
    not(paths.get(p).corrupt),
    paths' = paths.set(p, { ...paths.get(p), corrupt: true }),
    chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups, cycle' = cycle, wit' = wit,
  }

  /// The abstract clock: one unit of time passes. Placeholder
  /// staleness and chunk refAge advance (saturating); an in-flight
  /// split-upgrade transaction and an in-flight cycle's
  /// since-snapshot clock advance with them. Preconditions encode the
  /// two production contracts as the design carries them:
  ///   - the heartbeat contract (ENABLE_HB_CONTRACT): time cannot
  ///     advance past the heartbeat deadline for a placeholder whose
  ///     owner is alive in a guard-armed phase;
  ///   - the writer-transaction bound (ENABLE_SPLIT_UPGRADE; §4.1
  ///     collect-soundness condition, monitored in production by
  ///     `rio_store_chunk_upgrade_tx_seconds` + alert): time cannot
  ///     advance an open upgrade transaction past WRITER_TX_BOUND.
  action tick: bool = all {
    not(ENABLE_HB_CONTRACT) or PATHS.forall(p =>
      (paths.get(p).status == PUploading and ownerLiveForced(p)) implies
        paths.get(p).staleness + 1 <= HB_DEADLINE),
    not(ENABLE_SPLIT_UPGRADE) or UPLOADERS.forall(u =>
      ups.get(u).pc == UTxOpen implies ups.get(u).txAge + 1 <= WRITER_TX_BOUND),
    paths' = PATH_KEYS.mapBy(p =>
      val r = paths.get(p)
      if (r.status == PUploading) { ...r, staleness: minInt(r.staleness + 1, STALE_CAP) } else r),
    chunks' = HASHES.mapBy(h =>
      val c = chunks.get(h)
      if (c.present) { ...c,
        refAge: minInt(c.refAge + 1, AGE_CAP),
        deletedAge: if (c.deleted) minInt(c.deletedAge + 1, REAP_AGE_CAP) else 0,
      } else c),
    ups' = UPLOADERS.mapBy(u =>
      val r = ups.get(u)
      if (r.pc == UTxOpen) { ...r, txAge: minInt(r.txAge + 1, WRITER_TX_BOUND) } else r),
    cycle' = if (ENABLE_SPLIT_UPGRADE and cycle.phase != CIdle)
      { ...cycle, sinceSnap: minInt(cycle.sinceSnap + 1, SNAP_CAP) }
    else cycle,
    s3' = s3, outbox' = outbox, wit' = wit,
  }

  // --- 8. init ------------------------------------------------------------

  action init = all {
    paths' = PATH_KEYS.mapBy(p => emptyPath(0)),
    chunks' = HASHES.mapBy(h => NO_CHUNK),
    s3' = Set(),
    outbox' = Set(),
    ups' = UPLOADERS.mapBy(u => {
      pc: UIdle, path: 0, claim: 0, list: Set(), needsUpload: Set(), txAge: 0,
    }),
    cycle' = IDLE_CYCLE,
    wit' = {
      deletedWhileReferenced: false,
      skippedPutWithoutPresence: false,
      referencedChunkSwept: false,
      liveOwnerReaped: false,
      lateCleanupNoop: false,
      drainSkippedResurrected: false,
      hotpathReaped: false,
      scannerReaped: false,
      heartbeatReset: false,
      s3Deleted: false,
      chunkCollected: false,
      parseFailureAborted: false,
      quarantined: false,
      markMissSavedByTouch: false,
      tombstoneReaped: false,
      reapedUnsafely: false,
    },
  }

  // --- 9. step ------------------------------------------------------------

  action step = {
    nondet u = UPLOADERS.oneOf()
    nondet p = PATHS.oneOf()
    nondet h = HASHES.oneOf()
    nondet list = LIST_CHOICES.oneOf()
    any {
      // the writers (W1-W4 collapse onto this state machine)
      claimPath(u, p),
      upgradeManifest(u, list),
      upgradeBegin(u, list),
      upgradeCommit(u),
      uploadChunks(u),
      markUploaded(u),
      completeManifest(u),
      completeFailsForeign(u),
      abortToReap(u),
      heartbeat(u),
      reapByClaim(u),
      // the path-row janitors
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      // the collector (run_gc phase 3 / the daily backstop)
      collectSnapshot,
      collectMark,
      collectSweep(h),
      collectFinish,
      // the operator remediation (adjudication 7)
      quarantineCorrupt(p),
      // the drain
      drainOne(h),
      // the post-pass tombstone reap (merged_bug_336)
      reapOne(h),
      // the fault alphabet
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      // the clock
      tick,
    }
  }
}

// =========================================================================
// THE BASE REGIME: one writer, the full collector pipeline, no faults.
// The happy path, the explicit abort path, the collect cycle and the
// drain all interleave; nothing the collector does may touch a
// referenced chunk, garbage must stay reclaimable, and presence is
// never inferred from liveness.
// =========================================================================
module chunkCollectBase {
  import chunkCollect(
    MAX_HASHES = 3,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(2), Set(3), Set(1, 2), Set(1, 3), Set(2, 3), Set(1, 2, 3)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*

  /// The happy path under the replacement: claim, upgrade
  /// (write-ahead manifest + chunk rows, no counter), upload, mark
  /// presence, complete; the path becomes unreachable and the path
  /// sweep deletes ONLY the manifest rows; the next collect cycle
  /// derives liveness from the surviving manifests (none), soft-deletes
  /// + enqueues the chunks once they are past grace, and the drain
  /// removes the objects.
  run collectorHappyPathRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .expect(all {
        paths.get(1).status == PComplete,
        s3 == Set(1, 2),
        chunks.get(1).uploadedAt and chunks.get(2).uploadedAt,
        cr1NoLiveChunkCollected,
        cr4PresenceFromConfirmedUpload,
      })
      .then(pathSweepOne(1))
      .expect(all {
        // chunk GC is decoupled from path GC: the sweep deleted the
        // manifest rows only; the chunks and their objects are intact
        // until a collect cycle runs.
        paths.get(1).status == PNone,
        chunks.get(1).present and not(chunks.get(1).deleted),
        outbox == Set(),
        s3 == Set(1, 2),
      })
      .then(tick)
      .then(tick)
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        cycle.phase == CMarked,
        cycle.markSet == Set(),
        cycle.eligibleAtSnap == Set(1, 2),
      })
      .then(collectSweep(1))
      .then(collectSweep(2))
      .then(collectFinish)
      .expect(all {
        chunks.get(1).deleted and chunks.get(2).deleted,
        not(chunks.get(1).uploadedAt),
        outbox == Set(1, 2),
        cr2NoStrandedGarbage,
        noReferencedChunkSwept,
      })
      .then(drainOne(1))
      .then(drainOne(2))
      .expect(all {
        s3 == Set(),
        outbox == Set(),
        cr1NoLiveChunkCollected,
        cr2NoStrandedGarbage,
      })

  /// A capped cycle (design §4.1 step 3 v4): the cycle stops after one
  /// batch with eligible backlog remaining (COLLECT_CYCLE_VICTIM_CAP /
  /// CollectCursor::store in collect.rs); the next cycle re-derives
  /// eligibility and drains the remainder — a backlog spans cycles,
  /// never stretches one.
  run cappedCycleResumeRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(pathSweepOne(1))
      .then(tick)
      .then(tick)
      .then(collectSnapshot)
      .then(collectMark)
      .then(collectSweep(1))
      .then(collectFinish)
      .expect(all {
        // capped stop: chunk 2 is still uncollected backlog
        chunks.get(1).deleted,
        not(chunks.get(2).deleted),
        outbox == Set(1),
      })
      .then(collectSnapshot)
      .then(collectMark)
      .then(collectSweep(2))
      .then(collectFinish)
      .expect(all {
        chunks.get(2).deleted,
        outbox == Set(1, 2),
        cr2NoStrandedGarbage,
      })
}

// =========================================================================
// THE CRASH REGIME: two writers, process death enabled at every
// in-flight phase, the drain's mid-commit crash (C10), plus the full
// collector. Crashed uploads leave manifest rows that the path-row
// janitors (hot-path reclaim, scanner) delete; their chunks are then
// ordinary unreferenced rows that the next collect cycle reclaims — no
// counter repair exists or is needed.
// =========================================================================
module chunkCollectCrash {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = true,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*

  /// The C2-class crash under the replacement: a crashed chunked
  /// upload leaves an 'uploading' manifest; the hot-path reclaim
  /// deletes the row (and nothing else); the chunk is then
  /// unreferenced, ages past grace, and the next collect cycle
  /// soft-deletes + enqueues it; a later re-upload of the same content
  /// resurrects it and re-uploads because uploaded_at was cleared at
  /// soft-delete time (CR-4), and the drain skips the resurrected row.
  run crashedUploadCollectedRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(crash(1))
      .then(tick)
      .then(tick)
      .expect(hotpathStale(1))
      .then(hotPathReclaim(1))
      .expect(all {
        paths.get(1).status == PNone,
        // the reap touched no chunk state: still present, still
        // confirmed-uploaded, not soft-deleted, object still in S3
        chunks.get(1).present and chunks.get(1).uploadedAt and not(chunks.get(1).deleted),
        s3 == Set(1),
        wit.hotpathReaped,
      })
      .then(collectSnapshot)
      .then(collectMark)
      .then(collectSweep(1))
      .then(collectFinish)
      .expect(all {
        chunks.get(1).deleted,
        not(chunks.get(1).uploadedAt),
        outbox == Set(1),
      })
      .then(claimPath(2, 1))
      .then(upgradeManifest(2, Set(1)))
      .expect(all {
        // resurrected and needing a re-upload: liveness was never a
        // presence signal (CR-4)
        not(chunks.get(1).deleted),
        ups.get(2).needsUpload == Set(1),
      })
      .then(drainOne(1))
      .expect(all {
        wit.drainSkippedResurrected,
        s3.contains(1),
        cr1NoLiveChunkCollected,
      })
}

// =========================================================================
// THE CONTEND REGIME: two live writers sharing chunks against the
// collector and the drain — the mark-stale race the
// `last_referenced_at` touch exists to close (a manifest committing
// after the cycle's mark snapshot re-references an old chunk), the
// resurrect-vs-drain TOCTOU (G4a), and the late-cleanup no-op
// contention. No process death.
// =========================================================================
module chunkCollectContend {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*

  /// The §4.6 mark-stale interleaving, pinned deterministically: an
  /// old, unreferenced, still-uploaded chunk is past grace when a
  /// cycle snapshots and marks (it is collectable); a second writer
  /// then commits a manifest referencing it (CR-4-compliant PUT skip —
  /// uploaded_at is set) and the upsert touch moves it out of the
  /// cycle's collectable set; the sweep batch that follows collects
  /// only the genuinely unreferenced chunk; the touched chunk survives
  /// this cycle and is marked by the next one. CR-1 holds throughout —
  /// and the chunkCollectNoTouch falsification regime shows it stops
  /// holding when the touch is removed.
  run markStaleTouchProtectsRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(pathSweepOne(1))
      .then(tick)
      .then(tick)
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        cycle.markSet == Set(),
        cycle.eligibleAtSnap == Set(1, 2),
      })
      .then(claimPath(2, 2))
      .then(upgradeManifest(2, Set(1)))
      .expect(all {
        // the re-referencing writer skipped the PUT (uploaded_at set)
        // and the touch re-protected chunk 1 against this cycle
        ups.get(2).needsUpload == Set(),
        cycle.touchedSinceSnap.contains(1),
      })
      .then(collectSweep(2))
      .then(collectFinish)
      .expect(all {
        // only the unreferenced chunk was collected; the re-referenced
        // one was retained by the touch alone — the §4.6 reachability
        // state, recorded by the ghost
        chunks.get(2).deleted,
        not(chunks.get(1).deleted),
        wit.markMissSavedByTouch,
        cr1NoLiveChunkCollected,
        noReferencedChunkSwept,
      })
      .then(uploadChunks(2))
      .then(markUploaded(2))
      .then(completeManifest(2))
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        // the next cycle's mark covers the new manifest: the chunk is
        // now protected by the mark, not the touch
        cycle.markSet == Set(1),
        cr1NoLiveChunkCollected,
      })
      .then(collectFinish)
      .then(drainOne(2))
      .expect(all {
        s3.contains(1),
        not(s3.contains(2)),
        cr1NoLiveChunkCollected,
        cr2NoStrandedGarbage,
      })
}

// =========================================================================
// THE CORRUPT REGIME: an existing manifest_data.chunk_list can rot
// (C12). Under the replacement the polarity is fail-closed: the
// validation pass aborts the cycle, no chunk anywhere is collected
// while the corruption persists, and the operator quarantine of
// adjudication 7 (delete the named manifest) is the sanctioned
// remediation, after which the next cycle resumes collection. CR-2's
// unconditional structural form is expectedly unsatisfiable while the
// corrupt manifest exists (the §4.4 trade) — wired below as an
// expect-violation check — and the carved form plus CR-1 and the
// referenced-chunk sensor are this regime's invariants.
// =========================================================================
module chunkCollectCorrupt {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = true,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*

  /// Fail-closed abort: a corrupt chunk_list on an existing manifest
  /// makes the validation pass abort the cycle — no mark, no sweep, no
  /// gauges, the parse-failure alert fires (its model image is the
  /// ghost), and every chunk row is left untouched.
  run failClosedAbortRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(corruptChunkList(1))
      .then(tick)
      .then(tick)
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        cycle.phase == CIdle,
        wit.parseFailureAborted,
        not(wit.chunkCollected),
        not(chunks.get(1).deleted) and not(chunks.get(2).deleted),
        outbox == Set(),
        cr1NoLiveChunkCollected,
        cr2CarvedCorrupt,
      })

  /// Adjudication 7 end-to-end: the corrupt manifest suspends
  /// collection (abort); the operator quarantines it; the NEXT cycle
  /// completes, collects the garbage the pause had stranded — including
  /// the corrupt manifest's now-unreferenced private chunk — and never
  /// touches the chunk shared with a parseable manifest (quarantine
  /// safety). This is the carved CR-2's "collection resumes within one
  /// cycle of the remediation" made executable.
  run quarantineResumeRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(claimPath(2, 2))
      .then(upgradeManifest(2, Set(1)))
      .then(uploadChunks(2))
      .then(markUploaded(2))
      .then(completeManifest(2))
      .then(corruptChunkList(1))
      .then(tick)
      .then(tick)
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        // suspended: the corrupt manifest aborted the cycle
        cycle.phase == CIdle,
        wit.parseFailureAborted,
        not(wit.chunkCollected),
      })
      .then(quarantineCorrupt(1))
      .expect(all {
        wit.quarantined,
        paths.get(1).status == PNone,
        // the shared chunk is still referenced by the parseable
        // manifest; the private chunk is now garbage
        refs(1) == Set(2),
        refs(2) == Set(),
      })
      .then(collectSnapshot)
      .then(collectMark)
      .expect(all {
        cycle.phase == CMarked,
        cycle.markSet == Set(1),
      })
      .then(collectSweep(2))
      .then(collectFinish)
      .then(drainOne(2))
      .expect(all {
        // collection resumed within one cycle of the quarantine and
        // forfeited only the corrupt manifest's own chunk
        chunks.get(2).deleted,
        not(s3.contains(2)),
        not(chunks.get(1).deleted),
        s3.contains(1),
        cr1NoLiveChunkCollected,
        cr2CarvedCorrupt,
        noReferencedChunkSwept,
      })
}

// =========================================================================
// THE THRESHOLD-ORDER REGIME: deliberately misordered clock constants
// (hot-path reclaim threshold = heartbeat deadline). Used ONLY by the
// expect-violation check on S5: the path-row janitors keep their
// heartbeat/threshold discipline under the replacement, and the scaled
// ordering is still load-bearing for it.
// =========================================================================
module chunkCollectThresholdOrder {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 1,
    SCANNER_AFTER = 2,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

// =========================================================================
// FALSIFICATION REGIME — the §4.6 (ii) required falsification: the
// `last_referenced_at` touch removed from the upgrade action
// (ENABLE_TOUCH = false; equivalently grace = 0 for re-references).
// Identical to the contend regime in every other constant, so the wired
// contend check is the restored-touch baseline ("restoring the touch
// makes CR-1 pass"). Expected counterexample: a manifest commits after
// the mark snapshot referencing an old, otherwise-unreferenced chunk
// whose uploaded_at is set; the writer skips the PUT (CR-4-compliant);
// the sweep batch collects the chunk (nothing re-protected it); the
// drain deletes the only copy — CR-1 falsifies. Wired as an
// expect-violation check; this regime must NEVER be cited as a proof.
// =========================================================================
module chunkCollectNoTouch {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = false,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

// =========================================================================
// FALSIFICATION REGIME — the fail-closed rule removed (ENABLE_FAIL_CLOSED
// = false): the mark silently SKIPS an unparseable manifest instead of
// aborting the cycle — the C12 polarity flip the design forbids
// (design §4.4 / adjudication 5). Identical to the corrupt regime in
// every other constant, so the wired corrupt check is the fail-closed
// baseline. Expected counterexample: a chunk whose ONLY referrer is the
// corrupt manifest is omitted from the mark, collected, and drained
// while that manifest still exists — CR-1 falsifies through the parser.
// Wired as an expect-violation check.
// =========================================================================
module chunkCollectParseSkip {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = true,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = false,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

// =========================================================================
// THE WRITER-TRANSACTION-BOUND PAIR (the §4.1 collect-soundness
// condition, exercised): the upgrade transaction is split into
// begin/commit (ENABLE_SPLIT_UPGRADE) so its duration is a real
// interleaving quantity, and the duration ceiling WRITER_TX_BOUND is a
// tick precondition — the model form of "no chunk-referencing write
// transaction outlives grace − slack" (carried in production as the
// monitored rio_store_chunk_upgrade_tx_seconds assumption, T-1a.4).
//
//   - chunkCollectWriterBounded: WRITER_TX_BOUND = GRACE_TICKS (the
//     bound holds). CR-1 and the rest of the safety set hold for every
//     interleaving and any scan/cycle placement — the holds-half.
//   - chunkCollectWriterOverrun: WRITER_TX_BOUND = GRACE_TICKS + 1
//     (one tick past the soundness condition). Expected
//     counterexample: the upgrade begins (verdict captured against a
//     set uploaded_at, touch pinned to the begin instant), outlives
//     grace while a cycle snapshots/marks/sweeps the chunk and the
//     drain deletes it, then commits a manifest referencing the
//     now-gone object — CR-1 falsifies. Wired as an expect-violation
//     check.
// =========================================================================
module chunkCollectWriterBounded {
  import chunkCollect(
    MAX_HASHES = 1,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = true,
    WRITER_TX_BOUND = 1
  ).*
}

module chunkCollectWriterOverrun {
  import chunkCollect(
    MAX_HASHES = 1,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = true,
    WRITER_TX_BOUND = 2
  ).*
}

// =========================================================================
// THE LATE-MARK PAIR (Phase-1 input list item 1 / plan T-pre.1): under
// a RELAXED heartbeat contract (ENABLE_HB_CONTRACT = false — a live
// owner may stall past the reclaim thresholds, the contract-free clock
// of the Stage-C calibration), the `AND deleted = FALSE` guard on
// mark_chunks_uploaded is what keeps a late presence-commit from
// re-asserting uploaded_at on a row the collector has already
// soft-deleted (and the drain may already have drained).
//
//   - chunkCollectLateMarkGuarded: the guard present (the shipped
//     T-pre.1 shape). CR-1 holds even under the relaxed contract —
//     the holds-half, exactly the claim of the Phase-1 input list.
//     The wired check's invariant list is deliberately restricted to
//     CR-1 plus the structural/ownership set (boundsOK, S4, L3,
//     noReferencedChunkSwept): the relaxation itself makes S5
//     violable (reaping a live-but-stalled owner IS the relaxed
//     contract, not a bug), and it opens benign bookkeeping windows
//     with no data-loss content that the heartbeat contract excludes
//     in every production-shaped regime — a reclaimed-then-collected
//     chunk whose still-alive owner later re-PUTs the object (an
//     unreferenced S3 object with no scheduled delete, violating
//     CR-2's outbox clause) or re-marks it after a resurrect (a
//     transient uploaded_at-set/object-absent state, violating
//     CR-4(b) until the resurrecting writer's own re-upload lands).
//     Both shapes are storage/bookkeeping noise of the deliberately
//     broken contract, not properties the late-mark guard claims to
//     restore; CR-1 is.
//   - chunkCollectLateMarkUnguarded: the guard removed
//     (ENABLE_MARK_DELETED_GUARD = false, the pre-T-pre.1 shape).
//     Expected counterexample: the stalled owner's row is reclaimed
//     between its PUTs and its mark; the collector soft-deletes the
//     chunk and the drain deletes the object; the late mark then
//     re-asserts uploaded_at on the soft-deleted row; the next writer
//     trusts uploaded_at, skips the PUT, and completes — CR-1
//     falsifies (the M_033 harm shape without any counter involved).
//     Wired as an expect-violation check.
// =========================================================================
module chunkCollectLateMarkGuarded {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = false,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

module chunkCollectLateMarkUnguarded {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = false,
    ENABLE_HB_CONTRACT = false,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

// =========================================================================
// THE SHADOW-EQUIVALENCE PAIR (bughunt wave D1, bug_199): the dry run's
// chunk estimate is computed against simulated post-sweep state — the
// shadow_swept anti-join — and must equal, for every sweepable subset,
// the estimate over the state with those manifests actually swept.
//
//   - chunkCollectShadowEquivalence: the shipped exclusion
//     (ENABLE_SHADOW_EXCLUSION = true). shadowEstimateMatchesLive holds
//     for every reachable state — the holds-half, pinning the formula
//     the shadow arm implements (r[store.gc.dry-run+3]'s model image).
//   - chunkCollectShadowNoExclusion: the pre-fix estimate (no
//     exclusion — the savepoint-rolled-back sweep left the would-be-
//     swept manifests in the mark). Expected counterexample: a
//     'complete' manifest exclusively referencing a past-grace chunk —
//     the live run would collect it, the dry run reports zero
//     (structurally, for the dominant term). Wired expect-violation.
//
// Two writers / two paths / two hashes with exclusive AND shared lists
// so subset exclusion has both polarities to get wrong.
// =========================================================================
module chunkCollectShadowEquivalence {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(2), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = true,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

module chunkCollectShadowNoExclusion {
  import chunkCollect(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(2), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1,
    REAP_GRACE_TICKS = 1,
    ENABLE_SHADOW_EXCLUSION = false,
    ENABLE_TOUCH = true,
    ENABLE_FAIL_CLOSED = true,
    ENABLE_MARK_DELETED_GUARD = true,
    ENABLE_HB_CONTRACT = true,
    ENABLE_SPLIT_UPGRADE = false,
    WRITER_TX_BOUND = 1
  ).*
}

// =========================================================================
// GC COORDINATION (bughunt wave D1, bug_174 + merged_bug_211; migration
// 090): the cluster-scoped collect cadence, cursorless here — the
// durable gc_collect_state row vs the pre-fix per-replica process
// facts. Two replicas, a bounded DB clock, cycles as atomic stamped
// events (the GC advisory lock serializes them; atomicity models the
// lock), the row's backlog estimate as distinct stamped values, and a
// per-replica published-gauge map fed by a 60s row read
// (spawn_gc_gauge_publisher).
//
//   - cadenceBound: no two LIVE cycles ever land closer than the
//     backstop interval — the durable last_live_cycle_at gate
//     (GcCycleLease::backstop_due, both the unlocked pre-check and the
//     under-lock double-check collapse onto the one due-predicate).
//     The gapViolated ghost reads the DURABLE truth in every regime,
//     so the falsification is attributable to the gate alone.
//   - publishedFromDurableOnly: every replica's published gauge value
//     is a value the durable row has actually held (bounded staleness
//     between 60s reads is allowed; a process-local anchor the row
//     never held is unrepresentable).
//   - Shadow commits stamp a fresh estimate WITHOUT touching
//     last_live_cycle_at (a dry run is not a live cycle) — the
//     211/199 compounding fix's cadence half.
//
// gcCoordinationLocalTimers (ENABLE_DURABLE_CADENCE = false) restores
// the pre-fix per-replica interval_at timers: each replica's due
// predicate consults only its own local stamp, so the second replica
// fires immediately after the first — cadenceBound falsifies; this is
// bug_174's machine-checked counterexample (N heavy cycles/day at KEDA
// scale). Wired expect-violation.
//
// remoteReplicaPublishesPostCycleEstimate (expect-violation on the
// main regime): a replica OTHER than the committer publishes the
// current row estimate — merged_bug_211's convergence witness,
// unreachable pre-fix (the anchor lived in the winner's process; every
// other replica's gauge sat frozen at its pre-registered zero).
// =========================================================================
module gcCoordination {
  /// Backstop interval in ticks (production: COLLECT_BACKSTOP_INTERVAL).
  const INTERVAL: int
  /// Bounded clock horizon.
  const MAX_TIME: int
  /// Bound on committed estimates (live + shadow stamps).
  const MAX_STAMPS: int
  /// TRUE = the due predicate reads the durable row (post-090); FALSE
  /// = each replica reads its own local timer (the pre-fix bug).
  const ENABLE_DURABLE_CADENCE: bool

  type TimeOpt = | TNone | TAt(int)
  type ValOpt = | VNone | VAt(int)
  type RepOpt = | RNone | RAt(int)

  pure val REPLICAS = Set(1, 2)

  /// The DB clock (PG now() — both cadence conjuncts evaluate on it;
  /// no cross-replica clock enters the decision).
  var clock: int
  /// gc_collect_state.last_live_cycle_at.
  var lastLive: TimeOpt
  /// The pre-fix per-replica timer view (read only when
  /// ENABLE_DURABLE_CADENCE = false; maintained in both regimes so the
  /// falsify twin differs in exactly the due predicate).
  var localLast: int -> TimeOpt
  /// gc_collect_state.backlog_estimate (NULL until first commit).
  var backlog: ValOpt
  /// Distinct-estimate generator (the cycle_epoch image: every commit,
  /// live or shadow, writes a fresh value).
  var stamps: int
  /// Every value the durable row has held (ghost; publishers must
  /// never invent one).
  var rowHistory: Set[int]
  /// Per-replica last published gauge value (VNone = the
  /// pre-registered zero / never published).
  var published: int -> ValOpt
  /// Which replica committed the CURRENT backlog value (ghost, for the
  /// convergence witness).
  var lastStamper: RepOpt
  /// Ghost: a live cycle landed closer than INTERVAL after the
  /// previous one — evaluated against the DURABLE stamp in every
  /// regime.
  var gapViolated: bool

  /// The backstop due-predicate (BACKSTOP_DUE_SQL /
  /// GcCycleLease::backstop_due under the durable gate; the per-replica
  /// interval timer pre-fix).
  def dueFor(r: int): bool =
    if (ENABLE_DURABLE_CADENCE)
      match lastLive { | TNone => true | TAt(t) => clock - t >= INTERVAL }
    else
      match localLast.get(r) { | TNone => true | TAt(t) => clock - t >= INTERVAL }

  action tick = all {
    clock < MAX_TIME,
    clock' = clock + 1,
    lastLive' = lastLive, localLast' = localLast, backlog' = backlog,
    stamps' = stamps, rowHistory' = rowHistory, published' = published,
    lastStamper' = lastStamper, gapViolated' = gapViolated,
  }

  /// One BACKSTOP live cycle as an atomic stamped event: the due
  /// check, the cycle, and commit_cycle(Live{..}) — epoch+1, fresh
  /// estimate, last_live_cycle_at = now() — under the advisory lock.
  action backstopRun(r: int): bool = all {
    stamps < MAX_STAMPS,
    dueFor(r),
    gapViolated' = (gapViolated or
      (match lastLive { | TNone => false | TAt(t) => clock - t < INTERVAL })),
    lastLive' = TAt(clock),
    localLast' = localLast.set(r, TAt(clock)),
    stamps' = stamps + 1,
    backlog' = VAt(stamps + 1),
    rowHistory' = rowHistory.union(Set(stamps + 1)),
    lastStamper' = RAt(r),
    clock' = clock, published' = published,
  }

  /// One SHADOW (dry-run) cycle commit: commit_cycle(Shadow{..})
  /// anchors a fresh durable estimate but does NOT stamp
  /// last_live_cycle_at — a dry run never answers the cadence
  /// question.
  action shadowRun(r: int): bool = all {
    stamps < MAX_STAMPS,
    stamps' = stamps + 1,
    backlog' = VAt(stamps + 1),
    rowHistory' = rowHistory.union(Set(stamps + 1)),
    lastStamper' = RAt(r),
    clock' = clock, lastLive' = lastLive, localLast' = localLast,
    published' = published, gapViolated' = gapViolated,
  }

  /// One 60s gauge-publisher read on replica `r`
  /// (spawn_gc_gauge_publisher): SELECT the row, set the gauges from
  /// it. Skips a NULL estimate (the gauge stays unset), exactly as the
  /// publisher skips NULL.
  action publishGauges(r: int): bool = all {
    backlog != VNone,
    published' = published.set(r, backlog),
    clock' = clock, lastLive' = lastLive, localLast' = localLast,
    backlog' = backlog, stamps' = stamps, rowHistory' = rowHistory,
    lastStamper' = lastStamper, gapViolated' = gapViolated,
  }

  action init = all {
    clock' = 0,
    lastLive' = TNone,
    localLast' = REPLICAS.mapBy(r => TNone),
    backlog' = VNone,
    stamps' = 0,
    rowHistory' = Set(),
    published' = REPLICAS.mapBy(r => VNone),
    lastStamper' = RNone,
    gapViolated' = false,
  }

  action step = {
    nondet r = REPLICAS.oneOf()
    any {
      tick,
      backstopRun(r),
      shadowRun(r),
      publishGauges(r),
    }
  }

  val gcBoundsOK = and {
    clock >= 0 and clock <= MAX_TIME,
    stamps >= 0 and stamps <= MAX_STAMPS,
    rowHistory == 1.to(stamps),
    backlog == (if (stamps == 0) VNone else VAt(stamps)),
    (stamps == 0) implies (lastStamper == RNone and lastLive == TNone),
  }

  /// No two live cycles closer than the backstop interval.
  val cadenceBound = not(gapViolated)

  /// Published gauge values are durable-row values only.
  val publishedFromDurableOnly = REPLICAS.forall(r =>
    match published.get(r) {
      | VNone => true
      | VAt(v) => rowHistory.contains(v)
    })

  /// Expect-violation convergence witness: violated when a replica
  /// OTHER than the committer of the current estimate has published
  /// exactly that estimate.
  val remoteReplicaPublishesPostCycleEstimate = not(REPLICAS.exists(r =>
    match lastStamper {
      | RNone => false
      | RAt(w) => and { r != w, published.get(r) == backlog, backlog != VNone }
    }))
}

module gcCoordinationMain {
  import gcCoordination(
    INTERVAL = 2,
    MAX_TIME = 5,
    MAX_STAMPS = 3,
    ENABLE_DURABLE_CADENCE = true
  ).*
}

module gcCoordinationLocalTimers {
  import gcCoordination(
    INTERVAL = 2,
    MAX_TIME = 5,
    MAX_STAMPS = 3,
    ENABLE_DURABLE_CADENCE = false
  ).*
}
