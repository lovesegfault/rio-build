# TLC checks for formal protocol models in docs/spec/models/.
#
# Each model is a TLA+ specification of a distributed protocol that the
# prose spec in docs/spec/components/ describes normatively. TLC explores
# the bounded state space and asserts the invariants in the .cfg file. A
# counterexample (deadlock / invariant violation) fails the check and
# prints a step-by-step trace to the build log.
#
# Why TLA+ next to Kani: nix/kani.nix proves *Rust-code* properties
# ("no panic, no overflow, postcondition holds") over rio-lease's
# functions. TLC proves *protocol* properties ("no two nodes ever both
# think they hold the lease") over the abstract distributed algorithm —
# the thing the Rust implements but cannot itself observe across nodes.
# Same crate, two complementary proof obligations.
#
# Caching: the .tla model and .cfg config are eval-time inputs. Editing
# either rehashes the derivation and re-runs TLC. State space is bounded
# via .cfg constants. A check around ~1min needs no further optimization;
# there is no hard ceiling, but a model over ~5min should be tuned if
# that's possible without losing the interleavings that make its
# invariants non-vacuous. A correct slow check beats a fast faulty one —
# never shrink constants past the point where a deliberately-weakened
# test stops producing its counterexample. A genuinely unbounded model
# belongs in a manual `packages.*` target, not here.
#
# r[verify ...] markers live HERE at the wiring point, not in the .tla
# files — same discipline as nix/kani.nix's `kani-checks` attrset and
# nix/tests/default.nix's `subtests` entries: a marker at the wiring
# point structurally proves the check is built; a marker in the .tla
# header would claim "verified" even if this file's attr were deleted.
# .config/tracey/config.styx must list nix/tla.nix under `test_include`
# for tracey to scan the markers — that change lands with the first
# model so the markers and the file they guard are in the same commit.
#
# Gating: each model entry below is wrapped in `lib.optionalAttrs
# (builtins.pathExists …)` so the check only appears in checks.* once
# the .tla file lands. Trade-off vs leaving it red: a red check would
# make the wiring commit unmergeable under the "gate green before push"
# policy; gating lets the wiring + markers land first and the model
# commit turn the check on. The structural enforcement is preserved —
# a model that exists in docs/spec/models/ AND is wired here gets
# checked at the same commit; an unwired model is caught by `tracey
# query untested` (the spec rules stay listed until the markers are
# scanned). Drop the gate and let it fail loud if the project ever
# wants strict red-first sequencing instead.
{
  pkgs,
  lib,
  unfilteredRoot,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # One TLC run per model. The .cfg bounds the state space (CONSTANT
  # values, INVARIANT/PROPERTY names). TLC exits 0 iff every invariant
  # holds across the explored state graph; a violation prints a
  # counterexample trace and exits nonzero.
  mkTlcCheck =
    { name, spec }:
    pkgs.runCommand "tla-${name}"
      {
        nativeBuildInputs = [ pkgs.tlaplus ];
        # Only the .tla + .cfg pair. Models can EXTEND each other (e.g.
        # a shared `MC.tla` harness) — when that lands, extend the
        # fileset; keeping it narrow means an unrelated docs/ edit
        # doesn't re-run every TLC check.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions [
            (modelsDir + "/${spec}.tla")
            (modelsDir + "/${spec}.cfg")
          ];
        };
        # Surfaced in `nix log` and error messages.
        env.MODEL = spec;
      }
      ''
        set -euo pipefail
        cd $src
        # -workers: bound state-space exploration to the cores Nix
        # actually allotted this build. TLC's `auto` calls
        # Runtime.availableProcessors() — every visible host core —
        # which oversubscribes badly under nix-fast-build's parallel
        # checks gate (each TLC instance thinks it owns the box).
        # NIX_BUILD_CORES=0 means "all"; TLC's `auto` does the same,
        # so pass it through. Same shape as nix/fuzz.nix's -fork cap.
        # -metadir to a writable dir — tlc writes checkpoint/state
        # files under ./states/ by default and $src is RO. The
        # transcript is the proof artifact: it records the invariants
        # checked, the state count, and the depth — keep it.
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers="auto"
        tlc \
          -workers "$workers" \
          -metadir "$TMPDIR/tlc-states" \
          -config ${spec}.cfg \
          ${spec}.tla 2>&1 | tee $out
      '';
in
{
  # Expose the constructor so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # gated attrs below.
  inherit mkTlcCheck;

  # Per-model checks, gated on the .tla file existing. Spliced into
  # checks.* via misc-checks.nix — `lib.optionalAttrs` makes the attr
  # absent (not present-and-failing) until the model lands, so the gate
  # stays green between the wiring commit and the model commit.
  checks = lib.optionalAttrs (builtins.pathExists (modelsDir + "/LeaderElection.tla")) {
    # rio-lease's leader-election protocol over a Kubernetes Lease
    # object. The Phase-1 model has per-node clocks (bounded skew), the
    # observed-record staleness clock, local self-fencing, and
    # crash/recovery. It verifies AtMostOneCASWinner (the apiserver
    # admits at most one writer per resourceVersion),
    # BoundedDualLeadership (every dual-belief state has a discovery
    # mechanism armed), and StaleLeaderHasStaleGeneration (concurrent
    # believers always have distinct generations -- the bridge to the
    # executor-side generation fence). The third invariant is what the
    # pre-fix protocol falsified at depth 12; it holds now that Steal
    # derives the generation from the lease's transition count
    # (lease.gen+2) and claims it in PG at acquisition time (genHW
    # advances inside Steal, not in a separate dispatch-time Persist).
    # The fetch-max-seed marker covers that seeding-and-claiming
    # encoding. Lease-object deletion is outside this model's fault set;
    # the DeleteLease extension and the generation-claim verify marker
    # land with it.
    # r[verify sched.lease.at-most-one-leader+2]
    # r[verify sched.lease.k8s-lease]
    # r[verify sched.recovery.fetch-max-seed+2]
    tla-leader-election = mkTlcCheck {
      name = "leader-election";
      spec = "LeaderElection";
    };
  };
}
