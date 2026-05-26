// -*- mode: Bluespec; -*-
//
// AS-BUILT model of the rio-store chunk reference-counting subsystem:
// the exact `chunks.refcount` counter and the claim / heartbeat /
// drop-guard / token / reaper machinery that keeps it honest across
// writers, cleanup paths and background loops
// (`rio-store/src/{ingest,cas}.rs`, `rio-store/src/metadata/{inline,
// chunked,mod}.rs`, `rio-store/src/gc/{orphan,sweep,drain,mod}.rs`).
//
// This is the refcount-formal campaign's Phase-0 Stage-B deliverable
// (refcount-formal-design.md §3.2-§3.5): the transition relation encodes
// THE CODE AS BUILT at the formal-sprint branch point, not the design's
// intended replacement. The protocol inventory with file:line evidence
// for every claim mirrored here is refcount-inventory.md; the
// invariant-to-spec-rule map is docs/spec/models/refcount-invariant-map.md.
//
// WHAT IS MODELED
//   - The chunked write-ahead state machine per uploader (inventory
//     §1.4): claim_placeholder (idempotency lock + claim_id), the
//     upgrade transaction (manifest_data INSERT + refcount UPSERT +
//     PlaceholderToken capture, one tx), the S3 PUT fan-out, the
//     mark_chunks_uploaded presence commit, the claim-gated completion,
//     the token-gated in-process rollback (DEC-1), the claim-gated
//     abort/drop-guard reap, and the wall-clock/progress heartbeat.
//     All four production writers (PutPath input-addressed and
//     floating-CA, PutPathBatch per-output staging, Substituter) funnel
//     the increment through the same two functions and differ only in
//     which cleanup path owns the matching decrement (inventory §2.1),
//     so the model's uploaders are writer-shape-agnostic.
//   - The background accountants as interleavable actions, one model
//     action per SQL transaction: the hot-path stale reclaim (300 s),
//     the orphan scanner (15 min), the path-GC sweep batch (driven by
//     an abstract "this complete path became unreachable" environment
//     choice), the standalone orphan-chunk sweep (outer SELECT and
//     inner UPDATE as two separate actions so the C11 stale-read window
//     is representable), and the outbox drain with its FOR UPDATE
//     re-check before the irreversible backend delete.
//   - The fault alphabet of inventory §2.2: process death between
//     transactions (C1, C2, C5, C6, C7 — by phase), the
//     own-heartbeat-invalidated rollback token (C4), the drain crash
//     between the S3 DeleteObject and its transaction commit (C10), the
//     orphan-sweep select-vs-update race (C11), and the corrupt
//     `chunk_list` decrement skip (C12). C3 (complete fails and the
//     in-process reap also fails) and C7 (reap errors) leave exactly
//     the C5 state (an abandoned 'uploading' row whose pending reap
//     never runs) and are dispositioned onto it. C8 is benign by
//     construction (single tx) and needs no action; C9 is the ordinary
//     state between enqueue and drain.
//   - The abstract clock: per-placeholder staleness and per-chunk age
//     advance on an explicit `tick` action; the heartbeat deadline,
//     hot-path threshold, scanner threshold and grace TTL are scaled
//     model constants with the production ordering preserved
//     (heartbeat deadline < hot-path < scanner; see the
//     chunkLivenessThresholdOrder regime for the inversion check).
//   - The ghost truth: `refs(h)` (the manifest fold over
//     `manifest_data.chunk_list` of existing manifests, any status) is
//     a derived definition, never stored — exactly the quantity the
//     counter is supposed to mirror (CR-3) and the quantity the
//     planned replacement would compute at collect time.
//
// WHAT IS NOT MODELED (deliberate scope boundaries, each priced)
//   - The path-level reachability GC (mark CTE, sweep re-check,
//     referrer ordering, cycle reclaim, tenant retention). Path
//     unreachability is an abstract environment choice: any 'complete'
//     manifest may be swept at any time. Pre-registered NOT-ENCODED as
//     calibration family G4b (design §3.4); covered by the
//     store.gc.two-phase / sweep-recheck rules and their tests. The
//     consequence used here (and justified in the action comment): the
//     path sweep only ever deletes 'complete' manifests, because
//     'uploading' manifests are mark roots.
//   - PG row-lock acquisition order (40P01) and the sorted-bind
//     discipline: below the model's transaction-atomic granularity.
//     Pre-registered NOT-ENCODED (G6); covered by
//     store.chunk.lock-order, with_sorted_retry and the existing tests.
//   - The S3 PUT fan-out's per-chunk granularity: `uploadChunks`
//     uploads an uploader's whole needs-upload set in one action. The
//     S3 PUTs are not SQL transactions, so the per-transaction
//     granularity rule does not bind them; the load-bearing
//     distinction (upgraded but unconfirmed — the object may be
//     missing while the row is referenced) is preserved, and no
//     invariant or witness distinguishes partial-PUT states. The
//     heartbeat that production fires every 64 chunks/30 s during the
//     fan-out is the separate `heartbeat` action.
//   - Inline (< 256 KiB) uploads, narinfo contents, signatures, and
//     PutPathBatch's atomic multi-output completion: they never touch
//     the chunks table (inline) or add no chunk-level accounting beyond
//     per-output staging (batch), whose chunk effects are exactly two
//     independent uploaders here.
//   - The outbox attempts counter, the 10-attempt cap and the stuck
//     alert: drain failures are an S3-availability fault class, not a
//     refcount-protocol class. CR-2's "or parked at the attempts cap
//     and alerted" disjunct is therefore not exercised here; the
//     parked tail stays covered by the drain unit tests under
//     store.gc.pending-deletes.
//   - Reader paths (GetPath chunk reassembly): readers resolve only
//     'complete' manifests, so CR-1's state form is the reader-facing
//     property without a reader in the state vector (design §3.3).
//   - PG transaction failure as a nondeterministic fault on arbitrary
//     transactions. The error paths the code actually branches on are
//     modeled (rollback, abort-to-reap, the drain mid-crash); a
//     spuriously failing reap/sweep transaction is observationally a
//     reap/sweep that has not run yet.
//
// ENCODING DECISIONS
//   - One atomic action per SQL transaction (design §3.2). The named
//     READ-COMMITTED assumption ("the later-committing action observes
//     the earlier one's effects") is inherent in the interleaving
//     semantics: actions never observe stale snapshots, except where
//     the code itself materializes one (the orphan-chunk sweep's outer
//     SELECT, modeled as the separate `orphanSelect` action).
//   - The clock is relative, not absolute: the model never needs
//     "now()" — the code consults time only as `now() - updated_at`
//     (placeholder staleness) and `now() - created_at` (chunk age), so
//     the state carries those two distances directly, advanced by
//     `tick` and saturating just past the largest threshold that reads
//     them. Saturation is sound here because no property compares two
//     entities' clock readings for equality or distinctness — every
//     guard is a one-sided threshold comparison.
//   - The production guarantee "a live owner heartbeats at least every
//     30 s" (the wall-clock guard task plus the per-64-chunks progress
//     heartbeat) is encoded as a precondition on `tick`: time cannot
//     advance past the heartbeat deadline for a placeholder whose
//     owner is alive and in a guard-armed phase. Owners whose handler
//     already entered the rollback/reap path are NOT forced to
//     heartbeat (their heartbeat task may already be aborted — the
//     drop-guard path); they may still heartbeat. This
//     over-approximates the set of schedules (more behaviors), which
//     is sound for the safety properties checked here.
//   - The PlaceholderToken (`EXTRACT(EPOCH FROM updated_at)` captured
//     at upgrade, compared by equality at rollback) is encoded
//     semantically: the rollback matches iff the row is still the
//     uploader's own generation AND no heartbeat of its own has
//     advanced `updated_at` since the upgrade (`tokenStale`). Nothing
//     else writes `updated_at` without also changing status or
//     generation, so the equality test and this encoding coincide.
//   - claim_id (UUIDv4 per placeholder) is encoded as the per-path
//     placeholder generation counter: fresh per insert, carried by the
//     row, held by the owning uploader, compared by equality.
//   - Witness/history flags (`wit`) are model-only bookkeeping written
//     by actions and read only by invariants and witnesses, never by
//     guards.
//   - `leakedRefs(h)` is the corrupt-regime ghost: how many decrements
//     were skipped because the deleted manifest's chunk_list was
//     unparseable (gc/mod.rs::parse_unique_chunk_hashes's
//     warn-and-return-empty arm). It is what carves CR-2/CR-3 into
//     their sanctioned-deviation forms in the corrupt regime.
//
// The invariants, their witnesses, and the per-regime non-vacuity
// procedures are documented at their definitions below. Measured state
// counts, depths, and wall-clocks live in the introducing commits'
// messages and the CI checks' output transcripts, never in this file.
module chunkLiveness {
  // --- 1. types ---------------------------------------------------------

  /// Lifecycle of the `manifests` row for one store path.
  ///   - `PNone`: no row (never claimed, or reaped/swept away).
  ///   - `PUploading`: the write-ahead placeholder / idempotency lock
  ///     (`insert_manifest_uploading`); GC mark roots these, so the
  ///     path sweep never deletes them.
  ///   - `PComplete`: `complete_manifest_in_conn` flipped the status;
  ///     readers resolve only these.
  type PStatus = | PNone | PUploading | PComplete

  /// `manifest_data.chunk_list` for one path: `NoMd` when the row does
  /// not exist (placeholder not yet upgraded, or deleted), `Md(set)`
  /// once `upgrade_manifest_to_chunked` wrote it. The set is the
  /// DEDUPED hash set — the real list is ordered with possible repeats,
  /// but every consumer modeled here (the refcount arithmetic, the
  /// liveness fold) consumes `parse_unique_chunk_hashes`' deduped view
  /// (chunked.rs dedups before the UNNEST upsert; gc/mod.rs dedups at
  /// parse time).
  type MdOpt = | NoMd | Md(Set[int])

  /// One store path's durable state: the `manifests` row status, the
  /// placeholder generation minted last (`claim_id` of the current row
  /// when one exists — see the header's claim_id encoding note), the
  /// staleness clock (`now() - updated_at`, only meaningful while
  /// `PUploading`), the `manifest_data` chunk list, and whether that
  /// chunk_list blob is unparseable (the C12 surface).
  type PathRec = {
    status: PStatus,
    gen: int,
    staleness: int,
    mdata: MdOpt,
    corrupt: bool,
  }

  /// One `chunks` row: present (the row exists at all), the refcount,
  /// confirmed-backend-presence (`uploaded_at IS NOT NULL`), the
  /// soft-delete staging flag, and the age clock (`now() - created_at`,
  /// saturating just past the grace TTL). Rows are never hard-deleted
  /// by the as-built protocol.
  type ChunkRec = {
    present: bool,
    rc: int,
    uploadedAt: bool,
    deleted: bool,
    age: int,
  }

  /// Uploader program counter over the write-ahead state machine
  /// (inventory §1.4). Guard-armed phases (heartbeat task running,
  /// drop-guard armed): UClaimed, UUpgraded, UPutDone, UMarked.
  ///   - `URollback`: an upload/mark error was hit; the token-gated
  ///     DEC-1 rollback is pending (`cas::stage_chunked` error arm).
  ///   - `UReaping`: the claim-gated reap is pending (explicit abort,
  ///     complete-failure cleanup, or the drop-guard's spawned
  ///     `reap_one` after the handler future was dropped — the
  ///     heartbeat task is no longer guaranteed to run here).
  ///   - `UDone`: completed; guard defused.
  ///   - `UAborted`: the failure path finished (reap/rollback ran or
  ///     no-opped); inert.
  ///   - `UCrashed`: process death — no further actions ever (the
  ///     fault alphabet's C1/C2/C5/C6/C7, by the phase it struck).
  type UPc =
    | UIdle
    | UClaimed
    | UUpgraded
    | UPutDone
    | UMarked
    | URollback
    | UReaping
    | UDone
    | UAborted
    | UCrashed

  /// One uploader process: program counter, the path it claimed, the
  /// claim (placeholder generation) it holds, the manifest chunk set it
  /// wrote at upgrade, the subset of that set it must PUT (the upsert's
  /// RETURNING needs-upload verdict), and whether its own heartbeat has
  /// advanced `updated_at` since its upgrade (the PlaceholderToken
  /// false-negative arm, C4).
  type UpRec = {
    pc: UPc,
    path: int,
    claim: int,
    list: Set[int],
    needsUpload: Set[int],
    tokenStale: bool,
  }

  // --- 2. constants (const = overridable per regime) --------------------

  /// Chunk hashes. Three give: a chunk shared by two manifests, a
  /// chunk private to one, and a never-referenced spare — the shapes
  /// the by-count decrement, the resurrect race and the orphan-chunk
  /// sweep are about.
  const MAX_HASHES: int
  /// Store paths. Two give: two manifests sharing a chunk (the
  /// by-count decrement when both die in one sweep batch) and the
  /// reclaim-then-re-reference races.
  const MAX_PATHS: int
  /// Uploader processes (one-shot each — a process performs at most
  /// one claim). Two give: the ownership/identity races (a late
  /// cleanup against a successor's fresh placeholder) and concurrent
  /// writers sharing chunks.
  const MAX_UPLOADERS: int
  /// Fault switches. Each gates one branch of `step`; the regimes
  /// enable them in named combinations (design §3.2).
  /// Process death between transactions (C1-C7) and the drain's
  /// S3-delete-then-commit-fails window (C10).
  const ENABLE_CRASH: bool
  /// An existing manifest_data.chunk_list becomes unparseable (C12).
  const ENABLE_CORRUPT: bool
  /// The chunk lists an upgrade may write (the FastCDC output shapes
  /// the regime explores). Restricting this per regime is the main
  /// state-space lever: the two-writer regimes only need one shared
  /// and one private hash to exercise every sharing-sensitive
  /// mechanism (by-count decrement, resurrect race, dedup skip), while
  /// the single-writer base regime explores every non-empty subset.
  const LIST_CHOICES: Set[Set[int]]
  /// Scaled clock thresholds (production: 30 s heartbeat, 300 s
  /// hot-path stale reclaim, 900 s orphan scanner, 300 s chunk grace).
  /// The model only needs the ordering HB_DEADLINE < HOTPATH_AFTER <=
  /// SCANNER_AFTER and GRACE_TICKS >= 1; the chunkLivenessThresholdOrder
  /// regime deliberately violates it to show S5 depends on it.
  const HB_DEADLINE: int
  const HOTPATH_AFTER: int
  const SCANNER_AFTER: int
  const GRACE_TICKS: int

  // --- 3. pure values / helpers ------------------------------------------

  pure val HASHES = 1.to(MAX_HASHES)
  pure val PATHS = 1.to(MAX_PATHS)
  /// The paths map's key domain includes a dummy key 0 (permanently a
  /// missing row) so reading `paths.get(ups.get(u).path)` is defined
  /// even while an uploader holds no path — Quint's `get` on a missing
  /// key is undefined behavior, and conjunctions are not guaranteed to
  /// short-circuit. Quantifications and nondet picks use PATHS only.
  pure val PATH_KEYS = 0.to(MAX_PATHS)
  pure val UPLOADERS = 1.to(MAX_UPLOADERS)
  /// Staleness saturates just past the largest threshold that reads it;
  /// chunk age just past the grace TTL (header: saturation soundness).
  pure val STALE_CAP = SCANNER_AFTER
  pure val AGE_CAP = GRACE_TICKS + 1

  pure def minInt(a: int, b: int): int = if (a < b) a else b

  /// An absent chunks row, used to pre-populate the map and to keep
  /// non-present rows in a single canonical state (no spurious state
  /// blow-up from dead fields).
  pure val NO_CHUNK: ChunkRec = { present: false, rc: 0, uploadedAt: false, deleted: false, age: 0 }
  /// An absent manifests row (canonical dead-field values; `gen` is
  /// carried separately below because it survives row deletion).
  pure def emptyPath(g: int): PathRec =
    { status: PNone, gen: g, staleness: 0, mdata: NoMd, corrupt: false }

  // --- 4. state variables -------------------------------------------------

  /// Per path: the manifests/manifest_data state (schema 002 +
  /// claim_id from M_052), keyed 1..MAX_PATHS.
  var paths: int -> PathRec
  /// Per chunk hash: the chunks row (schema 002 + uploaded_at from
  /// M_033 + the M_023 CHECK), keyed 1..MAX_HASHES.
  var chunks: int -> ChunkRec
  /// The chunk-backend object set (S3): which hashes currently have a
  /// stored object.
  var s3: Set[int]
  /// `pending_s3_deletes`, as the set of enqueued hashes (the attempts
  /// counter is out of scope — header).
  var outbox: Set[int]
  /// The orphan-chunk sweep's outer-SELECT snapshot (`sweep_orphan_chunks`
  /// candidates), carried between the select and the batch UPDATE so
  /// the C11 stale-read-then-recheck window is a real interleaving.
  var orphanCand: Set[int]
  /// Per uploader process: the in-flight write-ahead state machine.
  var ups: int -> UpRec
  /// GHOST, per chunk: decrements skipped because the deleted
  /// manifest's chunk_list was unparseable (C12). Only ever non-zero
  /// when ENABLE_CORRUPT.
  var leakedRefs: int -> int
  /// History flags for witnesses and action-form invariants
  /// (model-only; never read by guards).
  ///   - deletedWhileReferenced: a backend DeleteObject was issued for
  ///     a hash that some existing manifest referenced at that instant
  ///     (CR-1's action form).
  ///   - skippedPutWithoutPresence: an upgrade's needs-upload verdict
  ///     excluded a chunk whose uploaded_at was NULL (CR-4's "the
  ///     liveness signal is never a presence signal" — structurally
  ///     impossible as built, the regression guard).
  ///   - liveOwnerReaped: a stale-path reap (hot-path or scanner)
  ///     deleted a placeholder whose owner was alive in a guard-armed
  ///     phase (S5).
  ///   - lateCleanupNoop: an owner-side cleanup (claim reap / token
  ///     rollback) found a foreign or fresh row and no-opped (the G1
  ///     contention actually arose).
  ///   - rollbackNoopStaleToken: the no-op was specifically the C4
  ///     own-heartbeat token mismatch.
  ///   - sweptSharedByCount: one sweep batch decremented a chunk by
  ///     more than one (the adfd303d7-C2 by-count clause exercised).
  ///   - drainSkippedResurrected: the drain re-check found a
  ///     resurrected chunk and skipped the S3 delete.
  ///   - orphanRecheckSaved: the orphan-chunk sweep's inner UPDATE
  ///     re-check excluded a candidate that was resurrected after the
  ///     outer SELECT (C11).
  ///   - hotpathReaped / scannerReaped: the two stale-reclaim paths
  ///     actually fired.
  ///   - heartbeatReset: a heartbeat reset a non-zero staleness (the
  ///     heartbeat is load-bearing, not decorative).
  ///   - s3Deleted: some backend DeleteObject was actually issued.
  var wit: {
    deletedWhileReferenced: bool,
    skippedPutWithoutPresence: bool,
    liveOwnerReaped: bool,
    lateCleanupNoop: bool,
    rollbackNoopStaleToken: bool,
    sweptSharedByCount: bool,
    drainSkippedResurrected: bool,
    orphanRecheckSaved: bool,
    hotpathReaped: bool,
    scannerReaped: bool,
    heartbeatReset: bool,
    s3Deleted: bool,
  }

  // --- 5. derived state (the ghost truth and the code's predicates) ------

  /// The chunk list of path `p` as the model (ghost) knows it,
  /// regardless of parseability.
  def listOf(p: int): Set[int] =
    match paths.get(p).mdata { | NoMd => Set() | Md(l) => l }

  /// `refs(h)`: the manifest fold — every existing manifest (any
  /// status) whose chunk_list references `h`. This is the quantity the
  /// counter mirrors (store.chunk.refcount-meaning) and the ground
  /// truth for liveness (inventory §3.2). Never stored; recomputed.
  def refs(h: int): Set[int] =
    PATHS.filter(p => paths.get(p).status != PNone and listOf(p).contains(h))

  /// `complete_refs(h)`: the same fold restricted to 'complete'
  /// manifests — what readers can resolve (CR-1's state-form head).
  def completeRefs(h: int): Set[int] =
    PATHS.filter(p => paths.get(p).status == PComplete and listOf(p).contains(h))

  /// What `parse_unique_chunk_hashes` returns for path `p`'s chunk_list
  /// at deletion time (gc/mod.rs): the deduped hash set, or EMPTY with
  /// a warning when the blob is unparseable — the C12 skip polarity.
  def parsedListOf(p: int): Set[int] =
    if (paths.get(p).corrupt) Set() else listOf(p)

  /// Does some uploader currently hold the placeholder generation that
  /// owns path `p`, in a phase whose heartbeat task production
  /// guarantees to be running (guard armed, not crashed)? Used by the
  /// `tick` precondition (the 30 s heartbeat contract) and by the S5
  /// ghost. Phases past the guard-armed window (URollback/UReaping)
  /// are deliberately excluded — see the header's clock note.
  def ownerLiveForced(p: int): bool =
    UPLOADERS.exists(u =>
      val r = ups.get(u)
      and {
        r.path == p,
        r.claim == paths.get(p).gen,
        Set(UClaimed, UUpgraded, UPutDone, UMarked).contains(r.pc),
      })

  /// The hot-path stale-reclaim predicate (`ingest::claim_placeholder`
  /// step 3 → `reap_one(ReapBy::Stale{SUBSTITUTE_STALE_THRESHOLD})`):
  /// an 'uploading' row whose updated_at is at least the hot-path
  /// threshold old.
  def hotpathStale(p: int): bool =
    paths.get(p).status == PUploading and paths.get(p).staleness >= HOTPATH_AFTER

  /// The orphan scanner's predicate (`gc/orphan.rs::scan_once` /
  /// `reap_one(ReapBy::Stale{STALE_THRESHOLD})`).
  def scannerStale(p: int): bool =
    paths.get(p).status == PUploading and paths.get(p).staleness >= SCANNER_AFTER

  /// The claim-gated reap admission predicate
  /// (`reap_one(ReapBy::Claim(id))`'s WHERE: status='uploading' AND
  /// claim_id = id). Quantified over by S4 below, exactly the way the
  /// action uses it.
  def claimReapAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// The token-gated rollback admission predicate
  /// (`delete_manifest_chunked_uploading`'s WHERE: status='uploading'
  /// AND updated_at-token equality — see the header's token encoding
  /// note).
  def rollbackAdmitted(p: int, claim: int, tokenStale: bool): bool =
    and {
      paths.get(p).status == PUploading,
      paths.get(p).gen == claim,
      not(tokenStale),
    }

  /// The claim-gated heartbeat admission predicate
  /// (`cas::heartbeat_uploading`'s WHERE: status='uploading' AND
  /// claim_id = $2). Quantified over by the L3-supporting invariant.
  def heartbeatAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// The claim-gated completion admission predicate
  /// (`complete_manifest_in_conn`'s WHERE: claim_id = $claim; the row
  /// is necessarily 'uploading' when the generation still matches and
  /// has not completed — completion is the only status flip and it
  /// keeps the claim).
  def completeAdmitted(p: int, claim: int): bool =
    paths.get(p).status == PUploading and paths.get(p).gen == claim

  /// The orphan-chunk sweep's candidate predicate
  /// (`sweep_orphan_chunks`' outer SELECT: refcount = 0 AND deleted =
  /// FALSE AND created_at < now() - grace).
  def orphanCandidate(h: int): bool =
    val c = chunks.get(h)
    and { c.present, c.rc == 0, not(c.deleted), c.age > GRACE_TICKS }

  /// The drain's re-check (`drain_one_row`: SELECT (deleted AND
  /// refcount = 0) ... FOR UPDATE, with a missing row reading as
  /// still-dead).
  def stillDead(h: int): bool =
    val c = chunks.get(h)
    if (not(c.present)) true else (c.deleted and c.rc == 0)

  // --- helpers for the chunk-map arithmetic (pure, used by actions) -----

  /// The refcount UPSERT for one manifest's deduped hash set
  /// (`upgrade_manifest_to_chunked`, metadata/chunked.rs): INSERT at
  /// refcount 1 for missing rows, `refcount + 1, deleted = false` for
  /// existing rows. `uploaded_at` and `created_at` are NOT touched on
  /// conflict.
  pure def upsertChunks(cm: int -> ChunkRec, list: Set[int]): int -> ChunkRec =
    cm.keys().mapBy(h =>
      val c = cm.get(h)
      if (not(list.contains(h))) c
      else if (c.present) { ...c, rc: c.rc + 1, deleted: false }
      else { present: true, rc: 1, uploadedAt: false, deleted: false, age: 0 })

  /// The decrement-by-count + zero-detect statement pair
  /// (`decrement_hashes_and_enqueue`, gc/mod.rs): refcount -= count,
  /// then rows that are now at 0 and not yet deleted get
  /// `deleted = true, uploaded_at = NULL`. Returns the updated map and
  /// the zeroed set (the RETURNING used for the outbox enqueue).
  pure def decrementAndZero(cm: int -> ChunkRec, dec: int -> int): { cm: int -> ChunkRec, zeroed: Set[int] } =
    val decd = cm.keys().mapBy(h =>
      val c = cm.get(h)
      if (dec.get(h) == 0 or not(c.present)) c else { ...c, rc: c.rc - dec.get(h) })
    val zeroed = decd.keys().filter(h => and {
      dec.get(h) > 0,
      decd.get(h).present,
      decd.get(h).rc == 0,
      not(decd.get(h).deleted),
    })
    val marked = decd.keys().mapBy(h =>
      val c = decd.get(h)
      if (zeroed.contains(h)) { ...c, deleted: true, uploadedAt: false } else c)
    { cm: marked, zeroed: zeroed }

  /// Decrement counts for a single deleted manifest (counts = 1 per
  /// unique parsed hash — `decrement_and_enqueue`).
  pure def countsForOne(parsed: Set[int]): int -> int =
    HASHES.mapBy(h => if (parsed.contains(h)) 1 else 0)

  // --- 6. actions ---------------------------------------------------------
  //
  // Each action is one production SQL transaction (or one named
  // non-transactional step: the S3 PUT fan-out, the crash injections,
  // the clock). Preconditions are the code path's guards; effects are
  // its state mutations; the function each mirrors is named in its doc
  // comment. Actions are guarded (disabled when their preconditions
  // fail), not thin always-enabled no-ops.

  /// `metadata::insert_manifest_uploading` via `ingest::claim_placeholder`
  /// (the successful-insert arm): mint a fresh claim (placeholder
  /// generation), insert the 'uploading' row with references protecting
  /// the closure, arm the drop-guard. The AlreadyComplete and
  /// Concurrent outcomes write nothing and are not modeled as actions.
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
    chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// `metadata::upgrade_manifest_to_chunked` via `cas::stage_chunked`
  /// step 2 (one transaction): FOR UPDATE token capture on the
  /// 'uploading' row, manifest_data INSERT, refcount UPSERT with
  /// `RETURNING (uploaded_at IS NULL) AS needs_upload`. The chunk list
  /// is the adversary's choice (any non-empty subset — FastCDC output
  /// content is irrelevant to liveness). The needs-upload verdict is
  /// keyed on uploaded_at (M_033 / store.cas.upsert-inserted+2); the
  /// skippedPutWithoutPresence ghost records any verdict that skips an
  /// unconfirmed chunk (impossible as encoded — the CR-4 regression
  /// guard). The code's WHERE has no claim filter — only
  /// status='uploading' — and the model mirrors that faithfully: the
  /// guard is the row's existence, not its generation.
  action upgradeManifest(u: int, list: Set[int]): bool = {
    val p = ups.get(u).path
    val needs = list.filter(h => not(chunks.get(h).uploadedAt))
    all {
      ups.get(u).pc == UClaimed,
      paths.get(p).status == PUploading,
      paths.get(p).mdata == NoMd,
      paths' = paths.set(p, { ...paths.get(p), mdata: Md(list) }),
      chunks' = upsertChunks(chunks, list),
      ups' = ups.set(u, { ...ups.get(u), pc: UUpgraded, list: list, needsUpload: needs, tokenStale: false }),
      wit' = { ...wit, skippedPutWithoutPresence: or {
        wit.skippedPutWithoutPresence,
        list.exists(h => not(needs.contains(h)) and not(chunks.get(h).uploadedAt)),
      } },
      s3' = s3, outbox' = outbox, orphanCand' = orphanCand, leakedRefs' = leakedRefs,
    }
  }

  /// `cas::do_upload`: the bounded-concurrency S3 PUT fan-out for the
  /// needs-upload set (collapsed to one action — see the header's
  /// granularity note; not a SQL transaction).
  action uploadChunks(u: int): bool = all {
    ups.get(u).pc == UUpgraded,
    s3' = s3.union(ups.get(u).needsUpload),
    ups' = ups.set(u, { ...ups.get(u), pc: UPutDone }),
    paths' = paths, chunks' = chunks, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// `metadata::mark_chunks_uploaded` (cas::stage_chunked step 4b, one
  /// statement): set uploaded_at for the chunks this uploader PUT,
  /// where still NULL. The presence commit point
  /// (store.cas.chunk-upload-committed).
  action markUploaded(u: int): bool = all {
    ups.get(u).pc == UPutDone,
    chunks' = chunks.keys().mapBy(h =>
      val c = chunks.get(h)
      if (ups.get(u).needsUpload.contains(h) and c.present) { ...c, uploadedAt: true } else c),
    ups' = ups.set(u, { ...ups.get(u), pc: UMarked }),
    paths' = paths, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// `metadata::complete_manifest_chunked` → `complete_manifest_in_conn`
  /// (one transaction): claim-gated status flip to 'complete' +
  /// narinfo fill; updated_at = now(). The guard defuses on success.
  action completeManifest(u: int): bool = {
    val p = ups.get(u).path
    all {
      ups.get(u).pc == UMarked,
      completeAdmitted(p, ups.get(u).claim),
      paths' = paths.set(p, { ...paths.get(p), status: PComplete, staleness: 0 }),
      ups' = ups.set(u, { ...ups.get(u), pc: UDone }),
      chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
      leakedRefs' = leakedRefs, wit' = wit,
    }
  }

  /// The completion attempt when the claim no longer matches (the row
  /// was reaped, possibly re-claimed by a successor):
  /// `complete_manifest_in_conn` touches zero rows and returns
  /// PlaceholderMissing; `cas::put_chunked` then falls through to the
  /// claim-gated reap (which will also no-op). Modeled as the pc move
  /// onto the reap path; no durable state changes.
  action completeFailsForeign(u: int): bool = all {
    ups.get(u).pc == UMarked,
    not(completeAdmitted(ups.get(u).path, ups.get(u).claim)),
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// The owner decides to abandon the upload and clean up via the
  /// claim-gated reap: the explicit `abort_upload` / `abort_placeholder`
  /// on an ingest error, the complete-failure cleanup arm of
  /// `cas::put_chunked`, or the drop-guard's spawned `reap_one` after
  /// the handler future was dropped (`ingest::PlaceholderGuard::drop`).
  /// No durable change yet — the reap is the next transaction.
  action abortToReap(u: int): bool = all {
    Set(UClaimed, UUpgraded, UPutDone, UMarked).contains(ups.get(u).pc),
    ups' = ups.set(u, { ...ups.get(u), pc: UReaping }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// An upload or mark failure sends the uploader to the in-process
  /// rollback (`cas::stage_chunked`'s error arms call `rollback()`).
  /// No durable change yet.
  action uploadFailsToRollback(u: int): bool = all {
    Set(UUpgraded, UPutDone).contains(ups.get(u).pc),
    ups' = ups.set(u, { ...ups.get(u), pc: URollback }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// `cas::heartbeat_uploading` (one statement, claim-gated): bump
  /// updated_at for the owner's own 'uploading' row. Fired by the
  /// guard's 30 s wall-clock task and the per-64-chunks progress hook.
  /// A post-upgrade heartbeat invalidates the captured
  /// PlaceholderToken (the C4 deliberate false-negative).
  action heartbeat(u: int): bool = {
    val p = ups.get(u).path
    all {
      Set(UClaimed, UUpgraded, UPutDone, UMarked, URollback, UReaping).contains(ups.get(u).pc),
      ups.get(u).path != 0,
      heartbeatAdmitted(p, ups.get(u).claim),
      paths' = paths.set(p, { ...paths.get(p), staleness: 0 }),
      ups' = ups.set(u, { ...ups.get(u),
        tokenStale: or {
          ups.get(u).tokenStale,
          Set(UUpgraded, UPutDone, UMarked, URollback).contains(ups.get(u).pc),
        } }),
      wit' = { ...wit, heartbeatReset: wit.heartbeatReset or paths.get(p).staleness > 0 },
      chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
      leakedRefs' = leakedRefs,
    }
  }

  /// `metadata::delete_manifest_chunked_uploading` (DEC-1, one
  /// transaction): the token-gated in-process rollback. If the
  /// 'uploading' row is still this uploader's generation and its own
  /// heartbeat has not advanced updated_at since the upgrade, decrement
  /// each of its written hashes by one and delete the
  /// manifest_data/manifests/narinfo placeholder rows — NO zero-detect
  /// and NO outbox enqueue (rows left at zero are the orphan-chunk
  /// sweep's job after the grace TTL). Otherwise a no-op (left for the
  /// orphan scanner).
  action rollbackTx(u: int): bool = {
    val p = ups.get(u).path
    // The specifically-C4 no-op shape: still my row, but my own
    // heartbeat advanced updated_at after the upgrade. Every other
    // no-op cause (row gone, foreign generation) is the late-cleanup
    // contention the claim/token machinery exists for.
    val ownRowStaleToken = and {
      paths.get(p).status == PUploading,
      paths.get(p).gen == ups.get(u).claim,
      ups.get(u).tokenStale,
    }
    all {
      ups.get(u).pc == URollback,
      if (rollbackAdmitted(p, ups.get(u).claim, ups.get(u).tokenStale)) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        chunks' = chunks.keys().mapBy(h =>
          val c = chunks.get(h)
          if (ups.get(u).list.contains(h) and c.present) { ...c, rc: c.rc - 1 } else c),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        s3' = s3, outbox' = outbox, orphanCand' = orphanCand, leakedRefs' = leakedRefs, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        wit' = { ...wit,
          lateCleanupNoop: wit.lateCleanupNoop or not(ownRowStaleToken),
          rollbackNoopStaleToken: wit.rollbackNoopStaleToken or ownRowStaleToken,
        },
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
        leakedRefs' = leakedRefs,
      },
    }
  }

  /// `gc::orphan::reap_one(ReapBy::Claim(claim))` (one transaction):
  /// the owner-side claim-gated reap used by abort_placeholder, the
  /// drop-guard, and put_chunked's complete-failure cleanup. If the
  /// 'uploading' row still carries this claim: read its chunk_list
  /// inside the transaction, DELETE narinfo (CASCADE manifests +
  /// manifest_data), decrement the parsed unique hashes, soft-delete
  /// the ones that hit zero and enqueue them (decrement_and_enqueue).
  /// A corrupt chunk_list parses to empty — decrement skipped, leak
  /// recorded (C12). If the claim does not match (reaped + re-claimed,
  /// or already completed): no-op.
  action reapByClaim(u: int): bool = {
    val p = ups.get(u).path
    val parsed = parsedListOf(p)
    val res = decrementAndZero(chunks, countsForOne(parsed))
    all {
      ups.get(u).pc == UReaping,
      if (claimReapAdmitted(p, ups.get(u).claim)) all {
        paths' = paths.set(p, emptyPath(paths.get(p).gen)),
        chunks' = res.cm,
        outbox' = outbox.union(res.zeroed),
        leakedRefs' = leakedRefs.keys().mapBy(h =>
          if (listOf(p).contains(h) and not(parsed.contains(h)))
            leakedRefs.get(h) + 1
          else leakedRefs.get(h)),
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        s3' = s3, orphanCand' = orphanCand, wit' = wit,
      } else all {
        ups' = ups.set(u, { ...ups.get(u), pc: UAborted }),
        wit' = { ...wit, lateCleanupNoop: true },
        paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
        leakedRefs' = leakedRefs,
      },
    }
  }

  /// The shared stale-reclaim transaction body
  /// (`gc::orphan::reap_one(ReapBy::Stale{..})`): re-check the
  /// staleness predicate inside the transaction (FOR UPDATE), DELETE
  /// narinfo (CASCADE), decrement the parsed unique hashes, soft-delete
  /// + enqueue the zeroed ones. Used by the hot-path reclaim (300 s,
  /// fired from a conflicting claim attempt) and the orphan scanner
  /// (15 min). The liveOwnerReaped ghost records a reap of a row whose
  /// owner is alive in a guard-armed phase (S5's harm).
  action reapStale(p: int, isHotpath: bool): bool = {
    val parsed = parsedListOf(p)
    val res = decrementAndZero(chunks, countsForOne(parsed))
    all {
      if (isHotpath) hotpathStale(p) else scannerStale(p),
      paths' = paths.set(p, emptyPath(paths.get(p).gen)),
      chunks' = res.cm,
      outbox' = outbox.union(res.zeroed),
      leakedRefs' = leakedRefs.keys().mapBy(h =>
        if (listOf(p).contains(h) and not(parsed.contains(h)))
          leakedRefs.get(h) + 1
        else leakedRefs.get(h)),
      wit' = { ...wit,
        liveOwnerReaped: wit.liveOwnerReaped or ownerLiveForced(p),
        hotpathReaped: wit.hotpathReaped or isHotpath,
        scannerReaped: wit.scannerReaped or not(isHotpath),
      },
      ups' = ups, s3' = s3, orphanCand' = orphanCand,
    }
  }

  /// The hot-path stale reclaim (`ingest::claim_placeholder` step 3,
  /// I-207): a conflicting claim attempt finds a stale 'uploading'
  /// placeholder (>= 300 s without progress) and reaps it before
  /// retrying its own insert. Modeled as a standalone action — the
  /// environment always has a writer able to attempt the path, so the
  /// only load-bearing part of "who triggers it" is the threshold.
  action hotPathReclaim(p: int): bool = reapStale(p, true)

  /// The background orphan scanner (`gc::orphan::scan_once`, 15 min
  /// cadence): reaps stale 'uploading' placeholders one transaction
  /// per row.
  action scannerReap(p: int): bool = reapStale(p, false)

  /// One path-GC sweep batch over a single unreachable path
  /// (`gc::sweep::sweep_one_batch` with the path-level mark and
  /// re-check abstracted into the environment's choice — see the
  /// header's G4b scope note): DELETE narinfo (CASCADE manifests +
  /// manifest_data), decrement the parsed unique hashes by 1, zero-
  /// detect, enqueue. Only 'complete' manifests are ever swept
  /// ('uploading' rows are mark roots).
  action pathSweepOne(p: int): bool = {
    val parsed = parsedListOf(p)
    val res = decrementAndZero(chunks, countsForOne(parsed))
    all {
      paths.get(p).status == PComplete,
      paths' = paths.set(p, emptyPath(paths.get(p).gen)),
      chunks' = res.cm,
      outbox' = outbox.union(res.zeroed),
      leakedRefs' = leakedRefs.keys().mapBy(h =>
        if (listOf(p).contains(h) and not(parsed.contains(h)))
          leakedRefs.get(h) + 1
        else leakedRefs.get(h)),
      ups' = ups, s3' = s3, orphanCand' = orphanCand, wit' = wit,
    }
  }

  /// One path-GC sweep batch covering BOTH paths in the same
  /// transaction (`sweep_one_batch` collects the whole batch's hashes
  /// into one UNNEST decrement, by count): a chunk referenced by both
  /// dying manifests is decremented by 2 in one statement — the
  /// adfd303d7-C2 by-count discipline (store.chunk.refcount-decrement).
  action pathSweepBatch: bool = {
    val counts = HASHES.mapBy(h =>
      PATHS.filter(p => parsedListOf(p).contains(h)).size())
    val res = decrementAndZero(chunks, counts)
    all {
      PATHS.forall(p => paths.get(p).status == PComplete),
      PATHS.size() >= 2,
      paths' = PATH_KEYS.mapBy(p => emptyPath(paths.get(p).gen)),
      chunks' = res.cm,
      outbox' = outbox.union(res.zeroed),
      leakedRefs' = leakedRefs.keys().mapBy(h =>
        leakedRefs.get(h) +
          PATHS.filter(p => listOf(p).contains(h) and not(parsedListOf(p).contains(h))).size()),
      wit' = { ...wit, sweptSharedByCount: or {
        wit.sweptSharedByCount,
        HASHES.exists(h => counts.get(h) >= 2),
      } },
      ups' = ups, s3' = s3, orphanCand' = orphanCand,
    }
  }

  /// The orphan-chunk sweep's outer SELECT (`sweep_orphan_chunks`):
  /// snapshot the candidate set (refcount = 0, not deleted, past the
  /// grace TTL) outside any transaction. The snapshot can go stale —
  /// that staleness is C11, and the inner UPDATE's re-check is the
  /// guard.
  action orphanSelect: bool = all {
    orphanCand == Set(),
    HASHES.exists(h => orphanCandidate(h)),
    orphanCand' = HASHES.filter(h => orphanCandidate(h)),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, ups' = ups,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// The orphan-chunk sweep's batch transaction (`sweep_orphan_batch`):
  /// UPDATE ... WHERE refcount = 0 AND deleted = FALSE re-checks each
  /// candidate at execution time; rows that still match are
  /// soft-deleted (uploaded_at cleared) and enqueued. A candidate
  /// resurrected since the SELECT is excluded by the re-check
  /// (orphanRecheckSaved).
  action orphanSweepBatch: bool = {
    val zeroed = orphanCand.filter(h => and {
      chunks.get(h).present,
      chunks.get(h).rc == 0,
      not(chunks.get(h).deleted),
    })
    all {
      orphanCand != Set(),
      chunks' = chunks.keys().mapBy(h =>
        val c = chunks.get(h)
        if (zeroed.contains(h)) { ...c, deleted: true, uploadedAt: false } else c),
      outbox' = outbox.union(zeroed),
      orphanCand' = Set(),
      wit' = { ...wit, orphanRecheckSaved: or {
        wit.orphanRecheckSaved,
        orphanCand.exists(h => not(zeroed.contains(h))),
      } },
      paths' = paths, s3' = s3, ups' = ups, leakedRefs' = leakedRefs,
    }
  }

  /// One drain row transaction (`gc::drain::drain_one_row`, success
  /// path): lock the outbox row, re-check `(deleted AND refcount = 0)`
  /// on the chunk FOR UPDATE; if still dead, issue the backend
  /// DeleteObject and remove the outbox row; if resurrected, skip the
  /// delete and drop the row. The deletedWhileReferenced ghost records
  /// CR-1's action form at the only irreversible step.
  action drainOne(h: int): bool = all {
    outbox.contains(h),
    if (stillDead(h)) all {
      s3' = s3.exclude(Set(h)),
      outbox' = outbox.exclude(Set(h)),
      wit' = { ...wit,
        s3Deleted: true,
        deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
      },
      paths' = paths, chunks' = chunks, orphanCand' = orphanCand, ups' = ups,
      leakedRefs' = leakedRefs,
    } else all {
      outbox' = outbox.exclude(Set(h)),
      wit' = { ...wit, drainSkippedResurrected: true },
      paths' = paths, chunks' = chunks, s3' = s3, orphanCand' = orphanCand, ups' = ups,
      leakedRefs' = leakedRefs,
    },
  }

  /// The drain's C10 window: the backend DeleteObject succeeded but the
  /// transaction commit did not — the outbox row survives and the next
  /// pass re-processes it (the S3 delete is idempotent). Crash-regime
  /// only.
  action drainCrashAfterS3(h: int): bool = all {
    ENABLE_CRASH,
    outbox.contains(h),
    stillDead(h),
    s3' = s3.exclude(Set(h)),
    wit' = { ...wit,
      s3Deleted: true,
      deletedWhileReferenced: wit.deletedWhileReferenced or refs(h) != Set(),
    },
    paths' = paths, chunks' = chunks, outbox' = outbox, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs,
  }

  /// Process death (the C1/C2/C5/C6/C7 windows, by phase): the
  /// uploader stops forever; its guard, heartbeat task and pending
  /// cleanup never run. Crash-regime only.
  action crash(u: int): bool = all {
    ENABLE_CRASH,
    Set(UClaimed, UUpgraded, UPutDone, UMarked, URollback, UReaping).contains(ups.get(u).pc),
    ups' = ups.set(u, { ...ups.get(u), pc: UCrashed }),
    paths' = paths, chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// Data damage: an existing manifest_data.chunk_list becomes
  /// unparseable (the C12 surface — `Manifest::deserialize` fails at
  /// reap/sweep time). Corrupt-regime only.
  action corruptChunkList(p: int): bool = all {
    ENABLE_CORRUPT,
    paths.get(p).mdata != NoMd,
    not(paths.get(p).corrupt),
    paths' = paths.set(p, { ...paths.get(p), corrupt: true }),
    chunks' = chunks, s3' = s3, outbox' = outbox, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  /// The abstract clock: one unit of time passes. Placeholder
  /// staleness and chunk age advance (saturating at their caps — see
  /// the header's saturation note). The precondition encodes the
  /// production heartbeat contract: time cannot advance past the
  /// heartbeat deadline for a placeholder whose owner is alive in a
  /// guard-armed phase (the 30 s wall-clock guard task), which is what
  /// keeps live uploads strictly below both reclaim thresholds.
  action tick: bool = all {
    PATHS.forall(p =>
      (paths.get(p).status == PUploading and ownerLiveForced(p)) implies
        paths.get(p).staleness + 1 <= HB_DEADLINE),
    paths' = PATH_KEYS.mapBy(p =>
      val r = paths.get(p)
      if (r.status == PUploading) { ...r, staleness: minInt(r.staleness + 1, STALE_CAP) } else r),
    chunks' = HASHES.mapBy(h =>
      val c = chunks.get(h)
      if (c.present) { ...c, age: minInt(c.age + 1, AGE_CAP) } else c),
    s3' = s3, outbox' = outbox, orphanCand' = orphanCand, ups' = ups,
    leakedRefs' = leakedRefs, wit' = wit,
  }

  // --- 8. init ------------------------------------------------------------

  action init = all {
    paths' = PATH_KEYS.mapBy(p => emptyPath(0)),
    chunks' = HASHES.mapBy(h => NO_CHUNK),
    s3' = Set(),
    outbox' = Set(),
    orphanCand' = Set(),
    ups' = UPLOADERS.mapBy(u => {
      pc: UIdle, path: 0, claim: 0, list: Set(), needsUpload: Set(), tokenStale: false,
    }),
    leakedRefs' = HASHES.mapBy(h => 0),
    wit' = {
      deletedWhileReferenced: false,
      skippedPutWithoutPresence: false,
      liveOwnerReaped: false,
      lateCleanupNoop: false,
      rollbackNoopStaleToken: false,
      sweptSharedByCount: false,
      drainSkippedResurrected: false,
      orphanRecheckSaved: false,
      hotpathReaped: false,
      scannerReaped: false,
      heartbeatReset: false,
      s3Deleted: false,
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
      uploadChunks(u),
      markUploaded(u),
      completeManifest(u),
      completeFailsForeign(u),
      abortToReap(u),
      uploadFailsToRollback(u),
      heartbeat(u),
      rollbackTx(u),
      reapByClaim(u),
      // the background accountants
      hotPathReclaim(p),
      scannerReap(p),
      pathSweepOne(p),
      pathSweepBatch,
      orphanSelect,
      orphanSweepBatch,
      drainOne(h),
      // the fault alphabet
      crash(u),
      drainCrashAfterS3(h),
      corruptChunkList(p),
      // the clock
      tick,
    }
  }
}
