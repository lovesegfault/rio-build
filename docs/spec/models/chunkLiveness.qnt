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

}
