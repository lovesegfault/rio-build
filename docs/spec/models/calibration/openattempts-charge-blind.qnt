// -*- mode: Bluespec; -*-
//
// Calibration override / regression pin (bughunt fix wave): the outbox-less charge-blind establishment (bug_347).
// PRE-FIX behavior modeled (bug_347): the failed cancel persist is dropped and the establishment sweep charges without consulting the node.
// PREDICTED falsification: cancelledNeverChargedAsCrash — the wired expect-violation
// check pins this falsification permanently: if it ever stops
// falsifying, the model has drifted away from the defect class it
// documents. Re-run:
//   quint run --invariant=cancelledNeverChargedAsCrash docs/spec/models/calibration/<this file>
//
module openAttemptsChargeBlind {
  type NodeState = NWantedLive | NCancelled | NAbsent
  type AttemptState = AOpen | AClosedCharged | AClosedFree

  var node: NodeState
  var attempt: AttemptState
  var outbox: bool
  // Ghost: the cancel reached durability (persist Ok or flush Ok).
  var cancelDurable: bool

  action init = all {
    node' = NWantedLive,
    attempt' = AOpen,
    outbox' = false,
    cancelDurable' = false,
  }

  // The cancel's terminal persist succeeds: one statement closes the
  // assignment (db/derivations.rs terminal_assignment_status).
  action cancelPersistOk = all {
    node == NWantedLive,
    attempt == AOpen,
    node' = NCancelled,
    attempt' = AClosedFree,
    outbox' = outbox,
    cancelDurable' = true,
  }

  // The cancel's terminal persist FAILS: the in-memory transition
  // stands, the owned batch latches in the outbox, the assignment
  // stays open (sched.attempt.cancel-close-driven).
  action cancelPersistErr = all {
    node == NWantedLive,
    attempt == AOpen,
    node' = NCancelled,
    attempt' = attempt,
    // PRE-FIX: the failed persist is dropped (best-effort error log) —
    // nothing latches, nothing re-drives.
    outbox' = false,
    cancelDurable' = cancelDurable,
  }

  // tick_flush_status_outbox re-drives the latched batch to Ok:
  // dropped on Ok only; the close lands.
  action flushOk = all {
    outbox,
    node' = node,
    attempt' = if (attempt == AOpen) AClosedFree else attempt,
    outbox' = false,
    cancelDurable' = true,
  }

  // The flush fails this tick: the entry is pushed back (front).
  action flushErr = all {
    outbox,
    node' = node,
    attempt' = attempt,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  // Leadership loss + successor recovery. The outbox is leader-scoped
  // (clear_persisted_state). A non-durable cancel evaporates: the
  // successor rebuilds the derivation live from PG. A durable cancel
  // either already closed the attempt or (legacy debris) leaves a
  // cancelled/absent node the sweep closes charge-free.
  action failover = all {
    node' = if (node == NCancelled and not(cancelDurable)) NWantedLive
            else if (node == NCancelled) NAbsent
            else node,
    attempt' = attempt,
    outbox' = false,
    cancelDurable' = cancelDurable,
  }

  // The establishment sweep, post-fix: routed through the total
  // kernel. Live-wanted expired attempts charge; cancelled/absent
  // close charge-free (no AttemptRow, no exclusion, no metric).
  action sweepEstablish = all {
    attempt == AOpen,
    // PRE-FIX: the sweep never consults the node — every expired open
    // attempt is charged executor_crash.
    attempt' = AClosedCharged,
    node' = node,
    outbox' = outbox,
    cancelDurable' = cancelDurable,
  }

  action step = any {
    cancelPersistOk,
    cancelPersistErr,
    flushOk,
    flushErr,
    failover,
    sweepEstablish,
  }

  // The kernel row: would the sweep close this node charge-free?
  // PRE-FIX: there is no charge-free arm.
  def sweepWouldCloseFree(n: NodeState): bool = false

  // bug_347's exclusion-pollution half: cancelled work is never
  // charged. Two forms — the node form (cancelled/absent is never the
  // charged party) and the ghost form (a durable cancel is never
  // followed by a charge).
  val cancelledNeverChargedAsCrash =
    not(attempt == AClosedCharged and node != NWantedLive)
    and not(attempt == AClosedCharged and cancelDurable)

  // bug_347's liveness half, as a state invariant: an open attempt for
  // a cancelled node always has a driver — the outbox re-drive, or the
  // sweep's charge-free arm.
  val openAttemptHasDriver =
    (attempt == AOpen and node == NCancelled) implies
      (outbox or sweepWouldCloseFree(node))

  val openAttemptsInvariants = all {
    cancelledNeverChargedAsCrash,
    openAttemptHasDriver,
  }
}
