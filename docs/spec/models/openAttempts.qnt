// -*- mode: Bluespec; -*-
//
// Open-attempt closure under cancellation (bughunt fix wave, bug_347;
// spec rule sched.attempt.cancel-close-driven).
//
// WHAT IS MODELED
//   One derivation's open pull-mode attempt and the two mechanisms
//   that may close it after its build is cancelled:
//     - the terminal status persist (which closes the assignment row
//       in the same statement), re-driven by the leader-scoped status
//       outbox when a persist fails ("latch on Ok only"), and
//     - the establishment sweep, routed through the total kernel
//       (rio-evidence-kernel/src/establish.rs): a cancelled or absent
//       node closes CHARGE-FREE; only live-wanted work may be charged.
//   Failover is modeled honestly: leadership loss clears the outbox.
//   If the cancel was never durable, the new leader recovers the
//   derivation as live (the cancel evaporated — the client re-cancels)
//   and a charge is then legitimate; if the cancel WAS durable, the
//   node recovers Cancelled (or was already closed).
//
// THE PROPERTIES
//   cancelledNeverChargedAsCrash — an attempt is never ClosedCharged
//     while its node is cancelled/absent, and never after the cancel
//     became durable (the ghost).
//   openAttemptHasDriver — an open attempt for a cancelled node always
//     has a driver: the outbox holds the close, or the sweep's
//     charge-free arm would close it (the kernel row).
//
// CALIBRATION (expect-violation): openattempts-charge-blind freezes
//   the pre-fix world — the failed persist is dropped (no outbox) and
//   the sweep charges without consulting the node — and both
//   invariants falsify on the shallow trace cancel-fail/sweep.
//
// Tier-1 budget: |node| * |attempt| * 2 (outbox) * 2 (ghost) = 36
// states reachable upper bound; TLC exhausts in well under a second.

module openAttempts {
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
    outbox' = true,
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
    attempt' = if (node == NWantedLive) AClosedCharged else AClosedFree,
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
  def sweepWouldCloseFree(n: NodeState): bool = n != NWantedLive

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
