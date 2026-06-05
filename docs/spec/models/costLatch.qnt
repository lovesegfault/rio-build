// -*- mode: Bluespec; -*-
//
// The scheduler's cost-table leadership latch (rio-scheduler
// sla/cost.rs `was_leader` + `poller_tick_prelude` + the
// observability::LEADER_EDGES cost-latch cells): the spot-price
// poller persists prices ONLY when its in-memory table was reloaded
// from PG inside the current leadership tenure. The latch's writer
// set is the LEADER_EDGES registry plus the prelude — never an
// enumerated prose list (the cost.rs doc's own rule); this model
// mirrors the registry's cost-latch cells structurally:
//
//   - prelude, standby tick  -> latch := false
//   - prelude, leading tick, latch false -> RELOAD from PG (table
//     epoch := world epoch), latch := true, skip one body
//   - prelude, leading tick, latch true  -> run the body: PERSIST
//   - LEADER_EDGES lose cell (the actor's lost handler AND the
//     rebound's Compound delivery — merged_bug_212's fix) ->
//     latch := false
//
// THE INVARIANT (noStalePersist): no persist ever writes a table
// reloaded under a PREVIOUS tenure over a world a foreign tenure has
// evolved since — `tableEpoch < worldEpoch` at a persist is the
// bug_310/merged_bug_212 failure shape (the deposed tenure's prices
// overwrite the interim leader's evolved EMA). The rebound is the
// load-bearing edge: a lose→re-acquire the loop never OBSERVED as two
// edges still runs the lose cell under Compound delivery, so the next
// leading tick reloads. The costlatch-212-acquire-only calibration
// twin restores the pre-fix acquire-only delivery (latch untouched)
// and must violate.
//
// SCOPE, deliberately narrow: one modeled replica (its belief, its
// latch, its table), the world epoch as an abstract PG version bumped
// by foreign-tenure persists, and the poller tick as three guarded
// actions. Foreign persists happen exactly when the real lease is not
// ours: while we stand by (observed), or INSIDE a rebound's
// lose→re-acquire gap (unobserved — the nondet `during` pick). A
// foreign persist while we hold the real lease is dual-leadership
// territory and is leaderElection.qnt's claim, not this model's.
//
// State counts, depth, and wall-clock: in the wired check's
// transcript and the introducing commit message, never here.
module costLatch {
  /// Leadership edges per trace (acquire + lose + rebound; bound).
  const MAX_EDGES: int
  /// Poller ticks per trace (bound).
  const MAX_TICKS: int
  /// Foreign-tenure persists per trace (the falsifier budget;
  /// 0 = disabled).
  const MAX_FOREIGN: int

  // --- state -------------------------------------------------------------
  /// Our belief: does this replica think it leads? (LeaseHooks edges.)
  var leading: bool
  /// The was_leader AtomicBool — the latch under verification.
  var latch: bool
  /// Which world version our in-memory CostTable reflects (set by the
  /// prelude's reload).
  var tableEpoch: int
  /// The abstract PG-side version: bumped by every foreign-tenure
  /// persist (the interim leader's evolved EMA).
  var worldEpoch: int
  /// Latch with a live writer (pollerTickBody computes it from the
  /// transition's own values): a persist ran with tableEpoch behind
  /// worldEpoch — the stale overwrite.
  var stalePersisted: bool
  /// Our persists so far (witness substrate).
  var persists: int
  /// Foreign persists so far (witness substrate).
  var foreignPersists: int
  /// Leadership edges so far (bound).
  var edges: int
  /// Poller ticks so far (bound).
  var ticks: int

  // --- invariants / witnesses ---------------------------------------------
  /// Bounds tripwire.
  val boundsOK = and {
    edges <= MAX_EDGES,
    ticks <= MAX_TICKS,
    foreignPersists <= MAX_FOREIGN,
    tableEpoch <= worldEpoch,
  }

  /// The headline: no persist over a foreign-evolved world from a
  /// stale-tenure table. Identically maintained by the lose cell
  /// running on EVERY lose-shaped transition (lost handler + rebound
  /// Compound delivery) — the acquire-only twin erases exactly that
  /// and falls.
  val noStalePersist = not(stalePersisted)

  /// Witness (expect-violation): our persist actually happens — the
  /// green noStalePersist verdict is about a space where the body
  /// runs.
  val noPersist = persists == 0

  /// Witness (expect-violation): a foreign tenure actually evolves
  /// the world — the verdict covers the interleaving the latch
  /// exists to survive.
  val noForeignPersist = foreignPersists == 0

  // --- actions --------------------------------------------------------
  /// Acquire edge (LeaderAcquired): belief flips up. The acquire cell
  /// does NOT latch — only the prelude's successful reload does
  /// (load-failure keeps the latch false and retries, a model no-op
  /// beyond burning a tick, deliberately not modeled).
  action acquireEdge = all {
    edges < MAX_EDGES,
    not(leading),
    leading' = true,
    latch' = latch, tableEpoch' = tableEpoch, worldEpoch' = worldEpoch,
    stalePersisted' = stalePersisted, persists' = persists,
    foreignPersists' = foreignPersists, edges' = edges + 1,
    ticks' = ticks,
  }

  /// Lose edge (LeaderLost): belief flips down AND the LEADER_EDGES
  /// cost-latch lose cell stores false.
  action loseEdge = all {
    edges < MAX_EDGES,
    leading,
    leading' = false,
    latch' = false,
    tableEpoch' = tableEpoch, worldEpoch' = worldEpoch,
    stalePersisted' = stalePersisted, persists' = persists,
    foreignPersists' = foreignPersists, edges' = edges + 1,
    ticks' = ticks,
  }

  /// The rebound (merged_bug_212): a lose→re-acquire the lease loop
  /// observed as ONE transition (the holder change happened and
  /// reverted inside an observation gap). Compound delivery runs the
  /// lose cells THEN the acquire path: the cost latch stores false;
  /// belief never flips. The nondet `during` pick is the foreign
  /// tenure that may have persisted inside the gap — the world the
  /// next reload must catch up to.
  action reboundCompound = {
    nondet during = Set(true, false).oneOf()
    all {
      edges < MAX_EDGES,
      leading,
      during implies foreignPersists < MAX_FOREIGN,
      leading' = true,
      latch' = false,
      worldEpoch' = if (during) worldEpoch + 1 else worldEpoch,
      foreignPersists' = if (during) foreignPersists + 1 else foreignPersists,
      tableEpoch' = tableEpoch,
      stalePersisted' = stalePersisted, persists' = persists,
      edges' = edges + 1, ticks' = ticks,
    }
  }

  /// A foreign tenure persists while we stand by (the observed case:
  /// between our lose and our next acquire).
  action foreignTenurePersists = all {
    foreignPersists < MAX_FOREIGN,
    not(leading),
    worldEpoch' = worldEpoch + 1,
    foreignPersists' = foreignPersists + 1,
    leading' = leading, latch' = latch, tableEpoch' = tableEpoch,
    stalePersisted' = stalePersisted, persists' = persists,
    edges' = edges, ticks' = ticks,
  }

  /// Prelude, standby arm: store(false), no body.
  action pollerTickStandby = all {
    ticks < MAX_TICKS,
    not(leading),
    latch' = false,
    leading' = leading, tableEpoch' = tableEpoch,
    worldEpoch' = worldEpoch, stalePersisted' = stalePersisted,
    persists' = persists, foreignPersists' = foreignPersists,
    edges' = edges, ticks' = ticks + 1,
  }

  /// Prelude, leading arm, latch false: reload from PG (the table
  /// catches up to the world), latch true, body SKIPPED this tick
  /// (sched.sla.cost-leader-edge-reload).
  action pollerTickReload = all {
    ticks < MAX_TICKS,
    leading,
    not(latch),
    tableEpoch' = worldEpoch,
    latch' = true,
    leading' = leading, worldEpoch' = worldEpoch,
    stalePersisted' = stalePersisted, persists' = persists,
    foreignPersists' = foreignPersists,
    edges' = edges, ticks' = ticks + 1,
  }

  /// Prelude, leading arm, latch true: the tick body runs and
  /// PERSISTS. The stale detector is written live from this
  /// transition's own values.
  action pollerTickBody = all {
    ticks < MAX_TICKS,
    leading,
    latch,
    persists' = persists + 1,
    stalePersisted' = (stalePersisted or tableEpoch < worldEpoch),
    leading' = leading, latch' = latch, tableEpoch' = tableEpoch,
    worldEpoch' = worldEpoch, foreignPersists' = foreignPersists,
    edges' = edges, ticks' = ticks + 1,
  }

  action init = all {
    leading' = false,
    latch' = false,
    tableEpoch' = 0,
    worldEpoch' = 0,
    stalePersisted' = false,
    persists' = 0,
    foreignPersists' = 0,
    edges' = 0,
    ticks' = 0,
  }

  action step = any {
    acquireEdge,
    loseEdge,
    reboundCompound,
    foreignTenurePersists,
    pollerTickStandby,
    pollerTickReload,
    pollerTickBody,
  }

  // --- deterministic protocol runs -----------------------------------
  /// The fix, end to end: lead, reload, persist fresh; a rebound with
  /// a foreign tenure inside the gap runs the lose cell; the next
  /// tick RELOADS (latch false) and only then persists — never the
  /// stale table.
  run reboundReloadsBeforePersistRun =
    init
      .then(acquireEdge)
      .then(pollerTickReload)
      .then(pollerTickBody)
      .then(reboundCompound)
      .then(pollerTickReload)
      .then(pollerTickBody)
      .expect(all {
        not(stalePersisted),
        persists == 2,
        latch,
        tableEpoch == worldEpoch,
      })
}

// =========================================================================
// The wired regime: two leadership cycles' worth of edges, one foreign
// persist, enough ticks for reload+body around each edge.
module costLatchBase {
  import costLatch(
    MAX_EDGES = 4,
    MAX_TICKS = 6,
    MAX_FOREIGN = 1
  ).*
}
