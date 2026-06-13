// -*- mode: Bluespec; -*-
//
// Fence calibration, TB-4 (bughunt-13: the model's own former close
// keying, re-encoded as the falsifiability pair of the repair).
//
// PRE-FIX behavior modeled: the direct fenced close was
// OWNER-gated and DERIVATION-keyed — an admitted owner's close
// landed on WHATEVER exec the derivation's active row carried, the
// caller's actual subject exec ignored (no `exec_id = $1` scope).
// The live trace it fabricates: the owner re-mints (its first exec
// goes stale in its report pipeline), then settles the STALE report
// — and the derivation-keyed close kills the successor exec, a
// destructive close production's stale-exec selector turns into
// zero rows.
//
// DECISION-ONLY perturbation (bughunt-2 bug_358): the admission and
// the fence are the LIVE guards, kept; only the named-exec match is
// dropped — the close routes through the same closeApply seat with
// named = 0 (a derivation-keyed statement binds no exec id) and the
// closeScopeViolated oracle observes the mismatch live.
//
// PREDICTED: directCloseHitsOnlyNamedExec falsifies
// (closeScopeViolated latch).
//
// Run (falsification):
//   quint verify --backend=tlc --main=fenceCalibTb4DerivationKeyedClose \
//     --step=calibStep --invariant=directCloseHitsOnlyNamedExec \
//     docs/spec/models/calibration/fence-tb4-derivation-keyed-close.qnt
// Baseline (as-built step, must HOLD):
//   quint verify --backend=tlc --main=fenceCalibTb4DerivationKeyedClose \
//     --invariant=directCloseHitsOnlyNamedExec \
//     docs/spec/models/calibration/fence-tb4-derivation-keyed-close.qnt
module fenceCalibTb4DerivationKeyedClose {
  import fencedWrites(
    MAX_GEN = 3,
    MAX_EXECS = 4,
    REPLICAS = Set("r1", "r2"),
    // bughunt-2 planes DORMANT: the perturbed surface is the legacy
    // direct close; the plane actions stay disabled and the plane
    // vars are framed by planesUnchanged in the override below.
    ENABLE_ATOMIC_REREAD = false,
    ENABLE_OUTBOX = false,
    ENABLE_TENURE_LIFECYCLE = false
  ).* from "../fencedWrites"

  /// PRE-FIX: the owner-keyed, exec-blind close (the TB-4 shape this
  /// wave repaired). Admission and fence are LIVE; the exec scope is
  /// the dropped decision.
  // Environment: PRE-FIX strawman — the model's former owner-keyed
  // close; no production transition implements it (the repair's
  // falsifiability pair).
  action calibOwnerKeyedClose(r: str): bool = all {
    match txPhase.get(r) {
      | TxIdle => false
      | TxFloorRead(v) => v <= replicaGen.get(r)
    },
    activeGen > 0,
    activeOwner == r,
    txPhase' = txPhase.set(r, TxIdle),
    val snap = match txPhase.get(r) { | TxIdle => 0 | TxFloorRead(v) => v }
    closeApply(r, snap, 0),
    pendingRefusal' = pendingRefusal,
    claimsUnchanged, floorUnchanged,
    planesUnchanged,
  }

  action calibStep = {
    nondet r = REPLICAS.oneOf()
    any {
      step,
      calibOwnerKeyedClose(r),
    }
  }
}
