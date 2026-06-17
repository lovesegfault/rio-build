// -*- mode: Bluespec; -*-
//
// Model of the manifests placeholder-claim ownership protocol after the
// bughunt-wave D1 takeover rework (merged_bug_003 + merged_bug_082):
// the two-clock, phase-keyed stall-takeover predicate of
// `rio-store/src/metadata/inline.rs` (`STALL_TAKEOVER_PREDICATE`) and
// the reap arm of `rio-store/src/ingest.rs` (`claim_placeholder`).
//
// WHAT THIS MODELS
//   - One substitution OWNER holding the placeholder claim, with the
//     durable row image competitors actually read: `claim_phase`
//     (migration 092: 'downloading' | 'budget_parked' | 'persisting'),
//     `fetched_bytes`, `last_progress_at`, `updated_at`. The heartbeat
//     (`cas::heartbeat_uploading_with_progress`) is a RELIABLE ticker
//     (tokio interval owned by the claim guard): it is folded into
//     `tick` and fires whenever due — only process death or release
//     stops it. It stamps liveness ALWAYS and progress ONLY when bytes
//     advanced since the previous heartbeat, so durable progress
//     visibility lags real progress by up to one heartbeat interval.
//     1 tick = 30 s in prod; HEARTBEAT_EVERY = 1 tick.
//   - One COMPETITOR with the two arms a second `claim_placeholder`
//     call takes against a held claim:
//       stallTakeover — the single-sourced predicate, every conjunct on
//         the DURABLE image and PG `now()`: phase = downloading
//         (parked/persisting exempt AS DATA), partial bytes, progress
//         stale by STALL_WINDOW, liveness FRESH within STALL_WINDOW.
//         Takeover strikes the deposed owner (strike-once).
//       reap — liveness stale by REAP_AFTER: the claim is released with
//         NO strike (the 003 fix: dead owners are reaped, not striked).
//   - The owner lifecycle producing every durable shape: advancing or
//     wedged transfers (environment choice), the budget park (phase
//     stamped BEFORE the semaphore acquire — a >STALL_WINDOW park
//     freezes progress with liveness fresh and phase exempt), the
//     persist phase, completion, death, and CANCELLATION (sh-023):
//     `ownerCancelled` is the in-process future-drop (tonic RST_STREAM
//     → handler future dropped). Distinct from `ownerDies` (kill -9):
//     a cancelled owner's PlaceholderGuard::drop runs and reaps the
//     row IFF the guard was armed (`guardArmed`). Post-sh-023 the
//     guard is pre-armed inside `claim_placeholder` BEFORE any SQL, so
//     `init` sets `guardArmed = true` and `noUnguardedOrphan` is
//     vacuously held in every main-regime trace; the
//     placeholderCalibSh023PreArmGap calibration starts unguarded
//     (the +2-era caller-side spawn) and falsifies it on
//     `init → ownerCancelled`.
//
// WHAT THE PREDICATE DOES AND DOES NOT PROMISE (the holds match the
// code's real guarantees — no more, no less):
//   - liveOwnerNeverDeposed: an alive owner in BudgetParked/Persisting
//     is never deposed (the 092 phase exemption — merged_bug_003's
//     backpressured-owner steal is unrepresentable).
//   - steadyAdvancerNeverDeposed: an owner advancing within the last
//     heartbeat interval whose progress was durably stamped within
//     2×HEARTBEAT is never deposed — TRUE exactly because
//     STALL_WINDOW >= 2×HEARTBEAT (the merged_bug_082 validation
//     floor); the placeholderClaimSubHeartbeatWindow regime drops the
//     floor and falsifies it (stamp lag deposes a live, continuously
//     advancing owner).
//   - strikeOnlyWithinLivenessWindow: a strike only ever lands while
//     the durable liveness is fresh — an owner dead LONGER than the
//     stall window always falls to the no-strike reap arm (the
//     merged_bug_003 180–300 s strike window is unrepresentable). An
//     owner dead SHORTER than one stall window is indistinguishable
//     from an alive-wedged one on the durable image — that strike is
//     the accepted residual, and the model does not pretend otherwise.
//
// REGIMES
//   placeholderClaimMain               — both conjuncts on, prod shape
//                                        (W=6, REAP=10, HB=1): all
//                                        three holds invariant;
//                                        wedged-owner takeover and
//                                        dead-owner reap witnessed
//                                        reachable.
//   placeholderClaimNoLiveness         — liveness conjunct OFF (pre-fix):
//                                        falsifies
//                                        strikeOnlyWithinLivenessWindow.
//   placeholderClaimNoPhase            — phase conjunct OFF (pre-092):
//                                        falsifies liveOwnerNeverDeposed.
//   placeholderClaimSubHeartbeatWindow — STALL_WINDOW < 2×HEARTBEAT:
//                                        falsifies
//                                        steadyAdvancerNeverDeposed
//                                        (the Config::validate() floor's
//                                        model justification).
//
// Budgets: tiny state space (one owner, one competitor, bounded
// clocks); every check converges in well under a minute at 2M/15.

module placeholderClaim {
  // --- bounds & knobs ----------------------------------------------------
  const MAX_TIME: int            // bounded clock (ticks = heartbeat intervals)
  const NAR_SIZE: int            // total bytes; bytes < NAR_SIZE = partial
  const STALL_WINDOW: int        // progress-stale threshold (ticks)
  const REAP_AFTER: int          // liveness-stale reap threshold (ticks)
  const HEARTBEAT_EVERY: int     // heartbeat cadence (ticks)
  const ENABLE_PHASE_CONJUNCT: bool     // 092 phase gate in the predicate
  const ENABLE_LIVENESS_CONJUNCT: bool  // the liveness-fresh conjunct

  type Phase = Downloading | BudgetParked | Persisting

  // --- state ---------------------------------------------------------------
  var time: int
  var ownerAlive: bool
  var claimed: bool              // the placeholder claim row is held
  var guardArmed: bool           // sh-023: the PlaceholderGuard is constructed
                                 // (drop-reap fires on ownerCancelled iff true).
                                 // Post-fix: pre-armed in init, before any SQL.
  var phase: Phase               // durable claim_phase (meaningful while claimed)
  var bytes: int                 // real fetched bytes (owner-side truth)
  var hbBytes: int               // durable fetched_bytes (as of last heartbeat)
  var lastProgressAt: int        // durable progress clock
  var updatedAt: int             // durable liveness clock
  var lastHbAt: int              // when the last heartbeat ran
  var lastAdvanceAt: int         // owner-side: when bytes last moved
  // outcome history (write-once evidence the invariants read)
  var deposedExemptPhase: bool   // takeover hit an alive parked/persisting owner
  var deposedSteadyAdvancer: bool // takeover hit a live, recently-advancing,
                                  // recently-stamped owner
  var strikedLivenessStale: bool // a strike landed while liveness was stale
  var sawTakeover: bool
  var sawDeadReap: bool

  // --- helpers ---------------------------------------------------------------
  def progressStale(now: int): bool = lastProgressAt < now - STALL_WINDOW
  def livenessFresh(now: int): bool = updatedAt >= now - STALL_WINDOW
  def livenessReapStale(now: int): bool = updatedAt < now - REAP_AFTER

  // The single-sourced predicate (inline.rs STALL_TAKEOVER_PREDICATE),
  // evaluated over the DURABLE image only.
  def takeoverEnabled(now: int): bool = and {
    claimed,
    (not(ENABLE_PHASE_CONJUNCT) or phase == Downloading),
    hbBytes < NAR_SIZE,                       // partial (fetched_bytes < $4)
    progressStale(now),
    (not(ENABLE_LIVENESS_CONJUNCT) or livenessFresh(now)),
  }

  // --- init ---------------------------------------------------------------
  action init = all {
    time' = 0,
    ownerAlive' = true,
    claimed' = true,
    // sh-023 (r[store.put.drop-cleanup+3]): the guard is pre-armed
    // INSIDE claim_placeholder, BEFORE the placeholder INSERT — the
    // post-fix invariant baked into init. The pre-fix (caller-side
    // spawn AFTER claim_placeholder returned) is the calibration's
    // calibInit, which starts guardArmed = false.
    guardArmed' = true,
    phase' = Downloading,
    bytes' = 0,
    hbBytes' = 0,
    lastProgressAt' = 0,
    updatedAt' = 0,
    lastHbAt' = 0,
    lastAdvanceAt' = 0,
    deposedExemptPhase' = false,
    deposedSteadyAdvancer' = false,
    strikedLivenessStale' = false,
    sawTakeover' = false,
    sawDeadReap' = false,
  }

  // --- time + the reliable heartbeat ticker ----------------------------------
  // The heartbeat is NOT a scheduled choice: the claim guard owns a
  // tokio interval that fires while the owner process lives and the
  // claim is held. tick advances the clock and runs the heartbeat
  // whenever it is due — one claim-guarded UPDATE stamping liveness
  // always and progress iff bytes moved since the previous heartbeat.
  action tick = all {
    time < MAX_TIME,
    time' = time + 1,
    val hbDue = ownerAlive and claimed and (time + 1) >= lastHbAt + HEARTBEAT_EVERY
    all {
      updatedAt' = if (hbDue) time + 1 else updatedAt,
      lastProgressAt' = if (hbDue and bytes > hbBytes) time + 1 else lastProgressAt,
      hbBytes' = if (hbDue) bytes else hbBytes,
      lastHbAt' = if (hbDue) time + 1 else lastHbAt,
    },
    ownerAlive' = ownerAlive, claimed' = claimed, guardArmed' = guardArmed,
    phase' = phase, bytes' = bytes, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  // The transfer advances (environment chooses; a wedged transfer just
  // never takes this action).
  action advance = all {
    ownerAlive, claimed, phase == Downloading, bytes < NAR_SIZE,
    bytes' = bytes + 1,
    lastAdvanceAt' = time,
    time' = time, ownerAlive' = ownerAlive, claimed' = claimed,
    guardArmed' = guardArmed, phase' = phase,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  // Budget park: the phase is stamped BEFORE the acquire (substitute.rs
  // stamps BudgetParked before acquire_many_owned), so the durable image
  // is exempt for the whole — possibly >STALL_WINDOW — wait.
  action park = all {
    ownerAlive, claimed, phase == Downloading,
    phase' = BudgetParked,
    time' = time, ownerAlive' = ownerAlive, claimed' = claimed,
    guardArmed' = guardArmed, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  action unpark = all {
    ownerAlive, claimed, phase == BudgetParked,
    phase' = Downloading,
    time' = time, ownerAlive' = ownerAlive, claimed' = claimed,
    guardArmed' = guardArmed, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  // The full NAR is down; persist begins (phase stamped before persist_nar).
  action startPersist = all {
    ownerAlive, claimed, phase == Downloading, bytes == NAR_SIZE,
    phase' = Persisting,
    time' = time, ownerAlive' = ownerAlive, claimed' = claimed,
    guardArmed' = guardArmed, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  action complete = all {
    ownerAlive, claimed, phase == Persisting,
    claimed' = false,
    time' = time, ownerAlive' = ownerAlive, guardArmed' = guardArmed,
    phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  action ownerDies = all {
    ownerAlive,
    ownerAlive' = false,
    time' = time, claimed' = claimed, guardArmed' = guardArmed,
    phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  // sh-023: in-process CANCELLATION (handler future dropped — gateway
  // broken-pipe → JoinSet abort → tonic RST_STREAM). Distinct from
  // ownerDies (kill -9, no Drop runs): the PlaceholderGuard's Drop
  // DOES run, and reaps the row IFF the guard was armed. Post-fix the
  // guard is pre-armed (guardArmed = true from init), so cancellation
  // ALWAYS reaps; the +2-era unguarded gap is the calibration regime.
  // Mirrors: rio-store/src/ingest.rs PlaceholderGuard::drop
  action ownerCancelled = all {
    ownerAlive,
    ownerAlive' = false,
    claimed' = if (guardArmed) false else claimed,
    time' = time, guardArmed' = guardArmed, phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale,
    sawTakeover' = sawTakeover, sawDeadReap' = sawDeadReap,
  }

  // --- competitor arms ---------------------------------------------------------
  // Stall takeover: steals the claim, STRIKES the deposed owner.
  action stallTakeover = all {
    takeoverEnabled(time),
    claimed' = false,
    sawTakeover' = true,
    deposedExemptPhase' = (deposedExemptPhase or
      (ownerAlive and (phase == BudgetParked or phase == Persisting))),
    // A live owner that advanced within the last heartbeat interval AND
    // whose progress was durably stamped within 2×HEARTBEAT — the
    // steady advancer the 082 floor protects.
    deposedSteadyAdvancer' = (deposedSteadyAdvancer or
      (ownerAlive and phase == Downloading
        and lastAdvanceAt >= time - HEARTBEAT_EVERY
        and lastProgressAt >= time - 2 * HEARTBEAT_EVERY)),
    strikedLivenessStale' = (strikedLivenessStale or not(livenessFresh(time))),
    time' = time, ownerAlive' = ownerAlive, guardArmed' = guardArmed,
    phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    sawDeadReap' = sawDeadReap,
  }

  // Reap: releases a liveness-dead claim with NO strike.
  action reap = all {
    claimed, livenessReapStale(time),
    claimed' = false,
    sawDeadReap' = (sawDeadReap or not(ownerAlive)),
    time' = time, ownerAlive' = ownerAlive, guardArmed' = guardArmed,
    phase' = phase, bytes' = bytes,
    hbBytes' = hbBytes, lastProgressAt' = lastProgressAt, updatedAt' = updatedAt,
    lastHbAt' = lastHbAt, lastAdvanceAt' = lastAdvanceAt,
    deposedExemptPhase' = deposedExemptPhase,
    deposedSteadyAdvancer' = deposedSteadyAdvancer,
    strikedLivenessStale' = strikedLivenessStale, sawTakeover' = sawTakeover,
  }

  action step = any {
    tick, advance, park, unpark, startPersist, complete,
    ownerDies, ownerCancelled, stallTakeover, reap,
  }

  // --- invariants (holds) --------------------------------------------------------
  // The 092 phase exemption: an alive parked/persisting owner is never
  // deposed (merged_bug_003's backpressured-owner steal).
  val liveOwnerNeverDeposed = not(deposedExemptPhase)
  // The 082 floor theorem: with STALL_WINDOW >= 2×HEARTBEAT, a live
  // owner advancing at heartbeat granularity whose advance was stamped
  // is never deposed; sub-2× windows falsify this on stamp lag alone.
  val steadyAdvancerNeverDeposed = not(deposedSteadyAdvancer)
  // The liveness conjunct: a strike only lands while durable liveness
  // is fresh — owners dead past the stall window always fall to the
  // no-strike reap arm.
  val strikeOnlyWithinLivenessWindow = not(strikedLivenessStale)
  val boundsOK = and {
    time >= 0, time <= MAX_TIME,
    bytes >= 0, bytes <= NAR_SIZE,
    hbBytes >= 0, hbBytes <= bytes,
  }
  // sh-023: a cancelled owner never leaves an unguarded orphan row.
  // VACUOUS in every main-regime trace (guardArmed is true from init
  // and no action clears it — the post-fix invariant baked in); the
  // calibration regime starts guardArmed = false and falsifies on
  // init → ownerCancelled. Not wired to quint-placeholder-claim-main
  // for that reason — only the calibration witness check matters.
  val noUnguardedOrphan = not(claimed and not(ownerAlive) and not(guardArmed))

  // --- witnesses (negated reachability; wired expect-violation) -------------------
  // The intended takeover IS reachable: an alive-but-wedged downloader
  // gets stolen.
  val wedgedOwnerDeposedW = not(sawTakeover)
  // The reap arm IS reachable for a dead owner.
  val deadOwnerReapedW = not(sawDeadReap)
}

// Production-shaped: 30 s heartbeat ticks, 180 s stall window (=6),
// 300 s reap (=10), both conjuncts on.
module placeholderClaimMain {
  import placeholderClaim(
    MAX_TIME = 14, NAR_SIZE = 2,
    STALL_WINDOW = 6, REAP_AFTER = 10, HEARTBEAT_EVERY = 1,
    ENABLE_PHASE_CONJUNCT = true, ENABLE_LIVENESS_CONJUNCT = true
  ).*
}

// Pre-fix predicate: no liveness conjunct — an owner dead past the
// stall window is striked instead of falling to the reap arm
// (falsifies strikeOnlyWithinLivenessWindow; the merged_bug_003
// 180–300 s strike window).
module placeholderClaimNoLiveness {
  import placeholderClaim(
    MAX_TIME = 14, NAR_SIZE = 2,
    STALL_WINDOW = 6, REAP_AFTER = 10, HEARTBEAT_EVERY = 1,
    ENABLE_PHASE_CONJUNCT = true, ENABLE_LIVENESS_CONJUNCT = false
  ).*
}

// Pre-092 predicate: no phase column — a budget-parked owner (progress
// frozen >STALL_WINDOW, heartbeat alive) is deposed mid-park (falsifies
// liveOwnerNeverDeposed; the backpressured-owner steal).
module placeholderClaimNoPhase {
  import placeholderClaim(
    MAX_TIME = 14, NAR_SIZE = 2,
    STALL_WINDOW = 6, REAP_AFTER = 10, HEARTBEAT_EVERY = 1,
    ENABLE_PHASE_CONJUNCT = false, ENABLE_LIVENESS_CONJUNCT = true
  ).*
}

// Stall window below 2× the heartbeat interval: a live owner advancing
// every tick is deposed because its durable progress stamp lags one
// heartbeat (falsifies steadyAdvancerNeverDeposed) — the model
// justification for the Config::validate() floor
// substitute_stall >= 2 * heartbeat (merged_bug_082).
module placeholderClaimSubHeartbeatWindow {
  import placeholderClaim(
    MAX_TIME = 10, NAR_SIZE = 6,
    STALL_WINDOW = 1, REAP_AFTER = 10, HEARTBEAT_EVERY = 2,
    ENABLE_PHASE_CONJUNCT = true, ENABLE_LIVENESS_CONJUNCT = true
  ).*
}
