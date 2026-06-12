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
// campaign archive (linkage, Stage-B/C records, acceptance) is
// docs/spec/models/refcount-records.md.
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

  // --- 7. invariants ------------------------------------------------------
  //
  // CR-1..CR-4 are the campaign's committed invariants
  // (refcount-formal-design.md §3.3; archive: refcount-records.md); the
  // S/L items are the supporting set carried for calibration. Spec
  // rules are named in each comment; the r[verify ...] markers live at
  // the check wiring points in nix/quint.nix.

  /// The runtime residual of TypeOK plus the self-consistency facts the
  /// other encodings rely on: refcounts within the bound the one-shot
  /// uploaders imply, clocks within their caps, a soft-deleted row is
  /// at refcount 0 (the upsert's resurrect arm clears `deleted` in the
  /// same statement as any increment), needs-upload sets within the
  /// written list, and at most one placeholder generation per path at a
  /// time (the ON CONFLICT DO NOTHING idempotency lock).
  val boundsOK = and {
    HASHES.forall(h =>
      val c = chunks.get(h)
      and {
        c.rc >= 0,
        c.rc <= MAX_UPLOADERS + MAX_PATHS,
        c.age >= 0 and c.age <= AGE_CAP,
        not(c.present) implies (c.rc == 0 and not(c.uploadedAt) and not(c.deleted) and c.age == 0),
        c.deleted implies c.rc == 0,
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
      }),
  }

  /// M_023's `CHECK (refcount >= 0)`: under-counts are never sanctioned
  /// (store.chunk.refcount-meaning's last clause). A double decrement
  /// for the same manifest generation would push a counter below zero
  /// or below the fold; this is the direct schema-level image.
  val m023NonNegative = HASHES.forall(h => chunks.get(h).rc >= 0)

  /// CR-1 `NoLiveChunkCollected` (store.chunk.no-live-collect; = S1).
  /// State form: a chunk referenced by any existing 'complete' manifest
  /// has its backend object present ('uploading' manifests are excluded
  /// — their PUTs may still be in flight; the write-ahead upsert
  /// deliberately precedes the upload). Action form: a backend
  /// DeleteObject is issued only in states where no existing manifest
  /// of any status references the chunk — recorded by the
  /// deletedWhileReferenced ghost at the two delete sites (drainOk,
  /// drainCrashAfterS3). This is the data-loss invariant.
  val cr1NoLiveChunkCollected = and {
    HASHES.forall(h => completeRefs(h) != Set() implies s3.contains(h)),
    not(wit.deletedWhileReferenced),
  }

  /// CR-2 `BoundedGarbageRetention` (store.gc.bounded-garbage-retention;
  /// = L1+L2), as-built structural form. The cadence/wall-clock half of
  /// the rule's bound (15-minute scanner, hourly sweep, 30 s drain) is
  /// background-loop scheduling, not protocol, and is not modeled; what
  /// the protocol must guarantee is that garbage never becomes
  /// UNRECLAIMABLE by the standing machinery:
  ///   (a) an existing, not-yet-soft-deleted chunk with no manifest
  ///       references is at refcount 0 — i.e. it remains eligible for
  ///       the zero-detect / orphan-chunk-sweep predicates. As built
  ///       this is exactly the conditionality on the counter: a
  ///       refcount stuck above zero with no references is permanent
  ///       garbage (nothing ever decrements it again).
  ///   (b) a soft-deleted chunk whose backend object still exists is
  ///       enqueued in the outbox — the irreversible step remains
  ///       scheduled (the drain-lag tail of the bound).
  /// The corrupt regime checks the carved form below instead; the
  /// unconditional form falsifying there (C12) is the pre-registered
  /// as-built deviation (the rule's carve-out clause).
  val cr2NoStrandedGarbage = HASHES.forall(h =>
    val c = chunks.get(h)
    and {
      (c.present and refs(h) == Set() and not(c.deleted)) implies c.rc == 0,
      (c.deleted and s3.contains(h)) implies outbox.contains(h),
    })

  /// CR-2, corrupt-regime carved form (the rule's carve-out clause +
  /// observability requirement): stranded garbage may exist only for
  /// chunks whose references-on-record were destroyed by a corrupt
  /// chunk_list skip, the skip is observable (the leakedRefs ghost is
  /// the model image of the warn + the absent decrement), and the
  /// stranded amount is exactly the skipped amount. The outbox clause
  /// is unconditional — corruption never strands an already-enqueued
  /// object.
  val cr2CarvedCorrupt = HASHES.forall(h =>
    val c = chunks.get(h)
    and {
      (c.present and refs(h) == Set() and not(c.deleted) and c.rc > 0) implies
        (leakedRefs.get(h) > 0 and c.rc <= leakedRefs.get(h)),
      (c.deleted and s3.contains(h)) implies outbox.contains(h),
    })

  /// CR-3 `CounterRefinesManifestFold` (store.chunk.refcount-meaning;
  /// = S2): the counter equals the manifest fold (and a hash with no
  /// chunks row is referenced by no manifest — the upsert and the
  /// manifest_data INSERT are one transaction). Every model action is
  /// a whole SQL transaction, so every reachable state is a quiescent
  /// point and the equality is checked everywhere. The design allows
  /// "sanctioned transient over-counts after C1-C7 crashes"; at this
  /// granularity those windows leave the manifest row in place, so the
  /// fold still counts it and no carve-out is needed — a crash-regime
  /// HOLDS verdict on this exact form is a stronger result than the
  /// design asked for, not a weaker encoding. In the corrupt regime
  /// this exact form is the pre-registered expected falsification (the
  /// C12 sanctioned permanent over-count); the carved form below is
  /// that regime's stated-form invariant.
  val cr3CounterRefinesFold = HASHES.forall(h =>
    (chunks.get(h).present implies chunks.get(h).rc == refs(h).size()) and
    (not(chunks.get(h).present) implies refs(h) == Set()))

  /// CR-3, corrupt-regime carved form (the rule's sanctioned permanent
  /// over-count): the counter equals the fold plus exactly the
  /// decrements that were skipped against unparseable chunk_lists.
  /// Under-counts remain unsanctioned in every regime.
  val cr3CarvedCorrupt = HASHES.forall(h =>
    (chunks.get(h).present implies chunks.get(h).rc == refs(h).size() + leakedRefs.get(h)) and
    (not(chunks.get(h).present) implies refs(h) == Set()))

  /// CR-4 `PresenceNeverInferredFromCounter`
  /// (store.chunk.liveness-not-presence, store.cas.upsert-inserted+2,
  /// store.cas.chunk-upload-committed; = S3, the M_033 lesson):
  ///   (a) a writer skips the backend PUT only for chunks whose
  ///       uploaded_at is set (the needs-upload verdict is keyed on
  ///       confirmed presence, never the counter) — recorded by the
  ///       skippedPutWithoutPresence ghost at the upgrade action;
  ///   (b) uploaded_at is set only while the backend object is present
  ///       (set after an observed successful put; cleared by the
  ///       soft-delete before any drain can remove the object).
  val cr4PresenceFromConfirmedUpload = and {
    not(wit.skippedPutWithoutPresence),
    HASHES.forall(h => chunks.get(h).uploadedAt implies s3.contains(h)),
  }

  /// S4 `OwnerOnlyMutation` (store.put.placeholder-claim+2,
  /// store.put.drop-cleanup+2): an owner-credentialed cleanup takes
  /// effect only against the placeholder generation that issued the
  /// credential. Quantified over the admission predicates the actions
  /// themselves use (the logService authGate pattern): if the claim
  /// reap or the token rollback is admitted for an uploader, the row it
  /// would mutate is that uploader's own generation. Dropping the
  /// claim/token conjunct from the predicates falsifies this.
  val s4OwnerOnlyMutation = UPLOADERS.forall(u =>
    val r = ups.get(u)
    and {
      (r.pc == UReaping and r.path != 0 and claimReapAdmitted(r.path, r.claim)) implies
        paths.get(r.path).gen == r.claim,
      (r.pc == URollback and r.path != 0 and rollbackAdmitted(r.path, r.claim, r.tokenStale)) implies
        paths.get(r.path).gen == r.claim,
    })

  /// S5 `LiveOwnerNeverReaped` (store.gc.orphan-heartbeat,
  /// store.put.stale-reclaim / store.substitute.stale-reclaim): a
  /// stale-path reclaim never deletes a placeholder whose owner is
  /// alive in a guard-armed phase. Holds because the heartbeat deadline
  /// is strictly below both reclaim thresholds and time cannot advance
  /// past the deadline for a live-owned row (the production heartbeat
  /// contract); the chunkLivenessThresholdOrder regime inverts the
  /// ordering and must falsify exactly this.
  val s5LiveOwnerNeverReaped = not(wit.liveOwnerReaped)

  /// L3 support `NoForeignFreshen` (store.put.placeholder-claim+2's
  /// heartbeat clause; the 937a9c928 lesson): the heartbeat freshens
  /// only the heartbeater's own placeholder generation — a stale
  /// uploader can never keep a successor's (or any foreign) placeholder
  /// artificially fresh, which is what makes every abandoned
  /// 'uploading' row eventually reapable (PlaceholderConvergence's
  /// safety content at this granularity).
  val l3NoForeignFreshen = UPLOADERS.forall(u =>
    val r = ups.get(u)
    (r.path != 0 and heartbeatAdmitted(r.path, r.claim)) implies paths.get(r.path).gen == r.claim)

  // --- expect-violation witnesses (non-vacuity probes) --------------------
  //
  // The vals below are NOT invariants and must never appear in a regime
  // check's invariant list. Each states "the interesting thing never
  // happens"; the quint-chunk-liveness-witness-* checks verify one each
  // and PASS only when the checker reports a violation — machine-checked
  // evidence that the contended state the headline invariants constrain
  // is actually reachable in that regime's explored space.

  /// Violated when a chunked upload completes: a 'complete' manifest
  /// whose every chunk is backend-present and presence-confirmed. The
  /// headline non-vacuity probe — CR-1's state form only bites for
  /// complete manifests.
  val noCompleteUpload = not(PATHS.exists(p => and {
    paths.get(p).status == PComplete,
    listOf(p) != Set(),
    listOf(p).forall(h => s3.contains(h) and chunks.get(h).uploadedAt),
  }))

  /// Violated when a backend DeleteObject has actually been issued —
  /// CR-1's action form is exercised against real deletes, not an
  /// empty delete set.
  val noBackendDelete = not(wit.s3Deleted)

  /// Violated when a referenced chunk exists whose backend presence is
  /// not yet confirmed (refcount >= 1, uploaded_at NULL) — the M_033
  /// precondition: the state in which trusting the counter for
  /// presence loses data.
  val noUnconfirmedReferencedChunk = not(HASHES.exists(h => and {
    chunks.get(h).present,
    chunks.get(h).rc >= 1,
    not(chunks.get(h).uploadedAt),
  }))

  /// Violated when the in-process rollback no-ops because the
  /// uploader's own heartbeat advanced updated_at after the upgrade
  /// (C4) — the deliberate token false-negative is reachable, so the
  /// orphan scanner's repair obligation for it is not vacuous.
  val noStaleTokenRollbackNoop = not(wit.rollbackNoopStaleToken)

  /// Violated when a heartbeat resets a non-zero staleness — the
  /// heartbeat is load-bearing for keeping live uploads below the
  /// reclaim thresholds (S5's mechanism actually does work).
  val noHeartbeatReset = not(wit.heartbeatReset)

  /// Violated when one sweep batch decrements a chunk by 2 or more — a
  /// chunk shared by several manifests dying in one batch is
  /// decremented by count, not collapsed to one (the adfd303d7-C2
  /// clause of store.chunk.refcount-decrement).
  val noSharedByCountDecrement = not(wit.sweptSharedByCount)

  /// Violated when the drain re-check skips a resurrected chunk — the
  /// G4a TOCTOU defense at the irreversible step is exercised.
  val noDrainResurrectSkip = not(wit.drainSkippedResurrected)

  /// Violated when the orphan-chunk sweep's inner UPDATE re-check
  /// excludes a candidate the outer SELECT had already chosen (C11).
  val noOrphanRecheckSave = not(wit.orphanRecheckSaved)

  /// Violated when an owner-side cleanup (claim reap or token
  /// rollback) arrives late and no-ops against a missing, foreign or
  /// refreshed row — the contended state S4 is about actually arises.
  val noLateCleanupNoop = not(wit.lateCleanupNoop)

  /// Violated when the hot-path stale reclaim fires (a claim attempt
  /// against an abandoned placeholder reclaims it at the 300 s
  /// threshold, before the 15-minute scanner would).
  val noHotpathReclaim = not(wit.hotpathReaped)

  /// Violated when the orphan scanner reaps an abandoned placeholder —
  /// the 15-minute backstop repair path runs (L3's repair term).
  val noScannerReap = not(wit.scannerReaped)

  /// Violated when an abandoned upload's accounting is reachable: an
  /// 'uploading' manifest whose owner crashed, its chunks still
  /// counted (refcount >= 1). This is the as-built C2 leak shape — the
  /// manifest row survives the crash, so the counter still agrees with
  /// the fold and the garbage is *accounted*, awaiting the reaper.
  /// (The literal "refcount > 0 with zero manifests" shape of design
  /// §3.5 is NOT reachable as built outside the corrupt regime —
  /// decrements are atomic with manifest deletion; see the corrupt
  /// regime's c12 falsification checks for the only true leak shape.)
  val noAbandonedAccounting = not(PATHS.exists(p => and {
    paths.get(p).status == PUploading,
    listOf(p) != Set(),
    listOf(p).forall(h => chunks.get(h).rc >= 1),
    UPLOADERS.exists(u => and {
      ups.get(u).path == p,
      ups.get(u).claim == paths.get(p).gen,
      ups.get(u).pc == UCrashed,
    }),
  }))

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
  /// (write-ahead state durable, upload incomplete) — C2.
  val noCrashAfterUpgrade = not(UPLOADERS.exists(u => and {
    ups.get(u).pc == UCrashed,
    ups.get(u).list != Set(),
    paths.get(ups.get(u).path).status == PUploading,
    paths.get(ups.get(u).path).gen == ups.get(u).claim,
  }))

  /// Violated when an uploader has crashed with its claim-gated reap
  /// still pending (the drop-guard / abort cleanup never ran) — C5,
  /// and the observable state C3/C7 collapse onto.
  val noCrashBeforeReap = not(UPLOADERS.exists(u => and {
    ups.get(u).pc == UCrashed,
    ups.get(u).path != 0,
    paths.get(ups.get(u).path).status == PUploading,
    paths.get(ups.get(u).path).gen == ups.get(u).claim,
  }))
  // (noCrashBeforeReap is intentionally weaker than the two above: any
  // crashed owner with a surviving placeholder is a pending-repair
  // state; the two above pin the specific C1/C2 phases.)

  /// Violated when BOTH uploaders have crashed after staging
  /// (upgrading) their outputs — the PutPathBatch C6 shape: k of N
  /// outputs staged, none completed, every guard dead.
  val noDoubleCrashStaged = not(
    UPLOADERS.filter(u => ups.get(u).pc == UCrashed and ups.get(u).list != Set()).size() >= 2)

  /// Violated when the corrupt-skip leak shape is reached: a chunk with
  /// no manifest references whose refcount is stuck above zero (only
  /// reachable through the C12 skip — the corrupt regime's
  /// pre-registered CR-3/CR-2 deviation made concrete).
  val noCorruptLeak = not(HASHES.exists(h => and {
    chunks.get(h).present,
    refs(h) == Set(),
    chunks.get(h).rc > 0,
  }))

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
  /// where still NULL and not soft-deleted (`AND deleted = FALSE` — the
  /// late-mark guard: a row stale-reclaimed between the PUT fan-out and
  /// this mark keeps uploaded_at NULL; presence on a soft-deleted row is
  /// re-established only by the resurrecting upsert + a fresh PUT). Under
  /// the heartbeat contract that interleaving is unreachable here, so the
  /// guard does not change this model's reachable states; the witness
  /// pair that shows it load-bearing under a relaxed contract belongs to
  /// the replacement (collector) model. The presence commit point
  /// (store.cas.chunk-upload-committed).
  action markUploaded(u: int): bool = all {
    ups.get(u).pc == UPutDone,
    chunks' = chunks.keys().mapBy(h =>
      val c = chunks.get(h)
      if (ups.get(u).needsUpload.contains(h) and c.present and not(c.deleted)) { ...c, uploadedAt: true } else c),
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

// =========================================================================
// THE BASE REGIME: one writer, the full background machinery, no faults.
// The happy path, the explicit abort/rollback paths, the orphan-chunk
// sweep and the drain all interleave; the counter must refine the
// manifest fold at every state and nothing the GC pipeline does may
// touch a referenced chunk's object. This is the regime where a core
// protocol regression surfaces fastest.
// =========================================================================
module chunkLivenessBase {
  import chunkLiveness(
    MAX_HASHES = 3,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(2), Set(3), Set(1, 2), Set(1, 3), Set(2, 3), Set(1, 2, 3)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).*

  /// The happy path: claim, upgrade (write-ahead increment), upload,
  /// mark presence, complete; then the path becomes unreachable, the
  /// sweep soft-deletes its chunks, and the drain removes the objects.
  run happyPathRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .expect(all {
        paths.get(1).status == PComplete,
        chunks.get(1).rc == 1 and chunks.get(2).rc == 1,
        s3 == Set(1, 2),
        chunks.get(1).uploadedAt and chunks.get(2).uploadedAt,
        cr3CounterRefinesFold,
        cr1NoLiveChunkCollected,
      })
      .then(pathSweepOne(1))
      .expect(all {
        paths.get(1).status == PNone,
        chunks.get(1).rc == 0 and chunks.get(1).deleted,
        outbox == Set(1, 2),
        not(chunks.get(1).uploadedAt),
      })
      .then(drainOne(1))
      .then(drainOne(2))
      .expect(all {
        s3 == Set(),
        outbox == Set(),
        cr2NoStrandedGarbage,
        cr3CounterRefinesFold,
      })

  /// The C4 shape without any fault: a tick forces a heartbeat (token
  /// goes stale), the upload then fails, and the rollback no-ops —
  /// the leaked accounting is left for the scanner, exactly as
  /// documented at PlaceholderToken.
  run staleTokenRollbackRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1)))
      .then(tick)
      .then(heartbeat(1))
      .then(uploadFailsToRollback(1))
      .then(rollbackTx(1))
      .expect(all {
        wit.rollbackNoopStaleToken,
        paths.get(1).status == PUploading,
        chunks.get(1).rc == 1,
        cr3CounterRefinesFold,
      })
}

// =========================================================================
// THE CRASH REGIME: two writers, process death enabled at every
// in-flight phase (C1, C2, C5, C6, C7), the drain's mid-transaction
// crash (C10), plus everything the base regime has. The reapers are the
// repair path: every crash window's leaked accounting must stay exactly
// accounted (CR-3 still exact at transaction granularity) and must
// remain reclaimable (CR-2) once the placeholder goes stale.
// =========================================================================
module chunkLivenessCrash {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = true,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).*

  /// The I-040/I-207 shape: a crashed chunked upload leaves an
  /// accounted placeholder; the hot-path reclaim reaps it at the 300 s
  /// threshold (decrementing and enqueueing), and a fresh upload of
  /// the same path re-uploads the chunk because uploaded_at was
  /// cleared — no stale-presence skip.
  run staleReclaimRerunRun =
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
        chunks.get(1).rc == 0 and chunks.get(1).deleted,
        not(chunks.get(1).uploadedAt),
        outbox == Set(1),
        wit.hotpathReaped,
      })
      .then(claimPath(2, 1))
      .then(upgradeManifest(2, Set(1)))
      .expect(all {
        // resurrected and needing a re-upload: the liveness signal was
        // never consulted for presence (CR-4)
        not(chunks.get(1).deleted),
        chunks.get(1).rc == 1,
        ups.get(2).needsUpload == Set(1),
        cr3CounterRefinesFold,
      })
      .then(drainOne(1))
      .expect(all {
        // drain re-check sees the resurrection and skips the delete
        wit.drainSkippedResurrected,
        s3.contains(1),
        cr1NoLiveChunkCollected,
      })
}

// =========================================================================
// THE CONTEND REGIME: two live writers sharing chunks against the GC
// pipeline and the drain — the G4a TOCTOU family (collect vs
// re-reference), the C11 select-vs-update race, the by-count batch
// sweep, and the late-cleanup no-op contention. No process death: every
// interleaving here is one production schedules on healthy processes.
// =========================================================================
module chunkLivenessContend {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).*

  /// Two complete manifests share chunk 1; one batch sweep deletes
  /// both and decrements the shared chunk by two in a single
  /// transaction (the adfd303d7-C2 by-count discipline), zero-detects
  /// it and enqueues it once. The second writer's upgrade also skips
  /// the shared chunk's PUT (uploaded_at already set — the CR-4
  /// compliant dedup).
  run sharedChunkBatchSweepRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1, 2)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(claimPath(2, 2))
      .then(upgradeManifest(2, Set(1)))
      .expect(ups.get(2).needsUpload == Set())
      .then(uploadChunks(2))
      .then(markUploaded(2))
      .then(completeManifest(2))
      .expect(all {
        chunks.get(1).rc == 2,
        cr3CounterRefinesFold,
      })
      .then(pathSweepBatch)
      .expect(all {
        wit.sweptSharedByCount,
        chunks.get(1).rc == 0 and chunks.get(1).deleted,
        outbox == Set(1, 2),
        cr3CounterRefinesFold,
        cr2NoStrandedGarbage,
      })
}

// =========================================================================
// THE CORRUPT REGIME: two writers with the C12 surface enabled — an
// existing manifest_data.chunk_list can become
// unparseable, and every deletion path that parses it at delete time
// (the claim reap, the stale reclaims, the path sweep) skips the
// decrement for that manifest while still deleting it. The carved
// CR-2/CR-3 forms hold (the leak is exactly the observably-skipped
// decrements); the unconditional forms are expected to falsify here and
// are wired as expect-violation checks (the pre-registered as-built
// deviation of the invariant map), never as invariants of this regime.
// =========================================================================
module chunkLivenessCorrupt {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 2,
    MAX_UPLOADERS = 2,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = true,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 2,
    SCANNER_AFTER = 3,
    GRACE_TICKS = 1
  ).*

  /// The C12 permanent leak: a complete manifest's chunk_list rots,
  /// the path sweep deletes the manifest but cannot parse the list, the
  /// decrement is skipped, and the chunk is stranded (refcount above
  /// zero, no references, never collectable) — the storage-only leak
  /// the as-built protocol accepts and the carved invariant forms
  /// sanction.
  run corruptSkipLeakRun =
    init
      .then(claimPath(1, 1))
      .then(upgradeManifest(1, Set(1)))
      .then(uploadChunks(1))
      .then(markUploaded(1))
      .then(completeManifest(1))
      .then(corruptChunkList(1))
      .then(pathSweepOne(1))
      .expect(all {
        paths.get(1).status == PNone,
        chunks.get(1).rc == 1,
        refs(1) == Set(),
        leakedRefs.get(1) == 1,
        not(chunks.get(1).deleted),
        cr3CarvedCorrupt,
        cr2CarvedCorrupt,
        not(cr3CounterRefinesFold),
        s3.contains(1),
      })
}

// =========================================================================
// THE THRESHOLD-ORDER REGIME: deliberately misordered clock constants
// (the hot-path reclaim threshold equals the heartbeat deadline). Used
// ONLY by the expect-violation check on S5: with the production
// ordering heartbeat << hot-path <= scanner broken, a live, progressing
// owner's placeholder becomes reclaimable between two of its own
// heartbeats and the reap of a live owner is reachable. This is the
// machine-checked form of "the scaled thresholds preserve the ordering
// and the ordering is load-bearing" (design §3.5).
// =========================================================================
module chunkLivenessThresholdOrder {
  import chunkLiveness(
    MAX_HASHES = 2,
    MAX_PATHS = 1,
    MAX_UPLOADERS = 1,
    ENABLE_CRASH = false,
    ENABLE_CORRUPT = false,
    LIST_CHOICES = Set(Set(1), Set(1, 2)),
    HB_DEADLINE = 1,
    HOTPATH_AFTER = 1,
    SCANNER_AFTER = 2,
    GRACE_TICKS = 1
  ).*
}
