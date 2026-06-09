// -*- mode: Bluespec; -*-
//
// merged_bug_072 (bughunt-4 S5a): the ledger-derived mint-budget law,
// a SEPARATE state machine in its OWN file: quint verify wedges on
// the SECOND module of a multi-module .qnt (observed 8min30s+ on a
// 7-state machine, via --main AND via import; the first module
// verifies in <1s) -- one module per file is the only proven shape.
// The openAttempts claim plane stays byte-identical either way. The fresh-claim gate's
// budget input is the DECISION: the live client derives
// `outstanding` from the surviving ledger population (entries are
// created at mint and leave on every authoritative answer); the
// pre-fix client derived it from a per-pass counter that RESET at
// pass end, making prior-pass unanswered mints invisible -- a
// list-ok/pull-lost brownout minted one nonce per pass to the
// ledger cap (32x a 1-slot worker's capacity).
//
//   openattempts-overmint -> outstandingBounded  (the twin gates on
//   the per-pass counter; one pass boundary later the population
//   exceeds the slot budget)
//
// Tier-1 budget: ledgerPop in 0..POP_CAP x passMints in 0..POP_CAP
// -- single-digit distinct states; TLC exhausts in <1s.
module openAttemptsBudget {
  /// The worker's slot budget (1-slot worker: the brownout shape).
  pure val SLOTS = 1
  /// State bound for the counters (2 = enough to exhibit one full
  /// pass boundary past the budget).
  pure val POP_CAP = 2

  /// Surviving ledger population: unanswered nonces (created at
  /// mint, removed on every authoritative answer).
  var ledgerPop: int
  /// The per-pass mint counter the PRE-FIX gate consulted (resets at
  /// every pass boundary).
  var passMints: int

  action init = all {
    ledgerPop' = 0,
    passMints' = 0,
  }

  /// Mint-budget seat: `gateOpen` is the decision -- the live client
  /// computes it from the LEDGER population; the overmint twin
  /// computes it from the per-pass counter. The seat applies the
  /// mint iff the gate opened. BOTH counters are saturated at
  /// POP_CAP: ledgerPop by the refuse-never-evict backstop, and
  /// passMints because an unbounded ghost counter blows up the
  /// exhaustive state space (first wiring attempt: 23M+ distinct
  /// states, no verdict inside the budget -- the mint->resolve cycle
  /// re-enables the increment forever; the wedgeCluster
  /// TRACK_EPISODE_GHOSTS lesson, hazard (tttt) discipline). The
  /// saturation preserves the twin's counterexample exactly: it
  /// needs passMints < SLOTS after one pass boundary, far below the
  /// cap.
  action mintBudgetSeat(gateOpen: bool): bool = all {
    ledgerPop' = if (gateOpen and ledgerPop < POP_CAP) ledgerPop + 1 else ledgerPop,
    passMints' = if (gateOpen and ledgerPop < POP_CAP and passMints < POP_CAP)
                   passMints + 1
                 else passMints,
  }

  /// LIVE: the gate derives from the surviving population -- mint
  /// only while outstanding < SLOTS.
  action budgetMintGated = mintBudgetSeat(ledgerPop < SLOTS)

  /// A pass boundary: the per-pass counter resets; the POPULATION
  /// survives (unanswered nonces are existing obligations).
  action budgetPassEnd = all {
    ledgerPop' = ledgerPop,
    passMints' = 0,
  }

  /// An authoritative answer drains one entry.
  action budgetEntryResolved = all {
    ledgerPop > 0,
    ledgerPop' = ledgerPop - 1,
    passMints' = passMints,
  }

  action step = any {
    budgetMintGated,
    budgetPassEnd,
    budgetEntryResolved,
  }

  // merged_bug_072: outstanding mints never exceed the slot budget --
  // the ledger population IS the budget input, across passes as well
  // as within one.
  val outstandingBounded = ledgerPop <= SLOTS

  // Reachability witness (non-vacuity): the budget plane actually
  // mints (a run where ledgerPop reaches SLOTS exists).
  val canReachFullBudget = ledgerPop < SLOTS
}
