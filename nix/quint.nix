# Quint checks for formal protocol models in docs/spec/models/.
#
# Each model is a Quint specification (https://quint-lang.org — TLA+
# semantics behind a typed, effect-checked, programmer-facing syntax) of
# a distributed protocol that the prose spec in docs/spec/components/
# describes normatively. `quint verify` explores the state space and
# asserts the named invariants; a violation prints a step-by-step
# counterexample trace to the build log and fails the check.
#
# Why Quint next to Kani: nix/kani.nix proves *Rust-code* properties
# ("no panic, no overflow, postcondition holds") over rio-lease's
# functions. Quint proves *protocol* properties ("no two nodes ever both
# think they hold the lease") over the abstract distributed algorithm —
# the thing the Rust implements but cannot itself observe across nodes.
# Same crate, two complementary proof obligations. (The models here
# replaced a hand-written TLA+ toolchain; the port was validated against
# every documented result of its predecessor before the TLA+ artifacts
# were removed — the migration commits carry the evidence.)
#
# Backends — why the default is `tlc`, not quint's own default:
#   - `tlc`: transpiles the spec to TLA+ and runs TLC's parallel BFS over
#     the FULL reachable state space — an exhaustive check of every
#     reachable state, parallel across every core the build is allotted.
#     This is the CI backend.
#   - `apalache` (quint's own default if no --backend is given — which is
#     exactly why this constructor must default to `tlc`): bounded
#     symbolic model checking via one Z3 context, incrementally unrolled.
#     Single-threaded, and its cost is superlinear in the step bound —
#     measured at adoption time as an order of magnitude slower than the
#     tlc backend at its default 10-step bound and unable to complete a
#     20-step bound on a spec the tlc backend exhausts in under a second
#     (the figures are in the evaluation record and the introducing
#     commits). It proves "no violation within maxSteps steps", not "no
#     violation". Use it only for a property that is shallow by
#     construction, or in the dev loop where finding a shallow
#     counterexample in seconds beats waiting for the exhaustive run.
#
# The tlc backend requires a FINITE state space: every variable that can
# grow without bound (a counter, a resourceVersion, a clock) needs a
# ceiling as an action PRECONDITION in the .qnt itself (see the MAX_RV /
# MAX_TIME / MAX_GEN preconditions in leaderElection.qnt). Without one
# TLC never terminates. Apalache does not need the ceiling (it bounds by
# step count) but the ceiling must be present anyway so the same spec is
# checkable under both backends.
#
# Where the tools come from: pkgs.quint's bin/quint is a wrapper that puts
# its own JRE on PATH and sets QUINT_HOME to the package's share/quint,
# which carries the bundled Apalache distribution as a store path. The
# tlc backend runs `java -cp <QUINT_HOME>/apalache-dist-*/apalache/lib/
# apalache.jar tlc2.TLC` — TLC ships inside the Apalache jar, so neither
# backend needs pkgs.tlaplus, a network connection, or a writable HOME.
# The apalache backend starts a gRPC server on a fixed port (8822); the
# nix sandbox's private network namespace means parallel apalache-backend
# checks on one builder cannot collide on it.
#
# Caching: the .qnt model is the only eval-time input. Editing it
# rehashes the derivation and re-runs the checker. State space is
# bounded via the ceilings in the .qnt. A check around ~1min needs no
# further optimization; there is no hard ceiling, but a model over ~5min
# should be tuned if that's possible without losing the interleavings
# that make its invariants non-vacuous. A correct slow check beats a
# fast faulty one — never shrink constants past the point where a
# deliberately-weakened test stops producing its counterexample. A
# genuinely unbounded model belongs in a manual `packages.*` target,
# not here.
#
# r[verify ...] markers live HERE at the wiring point, not in the .qnt
# files — same discipline as nix/kani.nix's `kani-checks` attrset and
# nix/tests/default.nix's `subtests` entries: a marker at
# the wiring point structurally proves the check is built; a marker in
# the .qnt header would claim "verified" even if this file's attr were
# deleted. .config/tracey/config.styx must list nix/quint.nix under
# `test_include` for tracey to scan the markers.
#
# The checks reference their .qnt files unconditionally: a missing or
# renamed model file is a loud eval/build failure, never a silently
# absent check. (The bootstrap-era `lib.optionalAttrs (pathExists …)`
# gating that let the wiring land before the model was dropped once
# every wired model existed — a gated check whose model vanishes
# disappears from checks.* instead of failing, which converts "the
# proof is gone" into a silent coverage loss.)
{
  pkgs,
  lib,
  unfilteredRoot,
  # nix/checks.nix's nextest reuse-build helpers and the prebuilt
  # rio-lease / rio-store test binaries, threaded through
  # misc-checks.nix. Only the mbt-rio-lease and mbt-rio-logservice
  # conformance checks below consume them — the model checks need
  # nothing from the Rust build.
  mkNextestRun,
  mkNextestMeta,
  rioLeaseTestBin,
  rioStoreTestBin,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # The leader-election model as its own single-file store path (the
  # same narrowing mkQuintCheck's src uses): the conformance check
  # depends on the model's content, not on whatever tree unfilteredRoot
  # points at — an unrelated docs/ edit must not re-run it, a model edit
  # must.
  leaseModel = lib.fileset.toSource {
    root = modelsDir;
    fileset = modelsDir + "/leaderElection.qnt";
  };

  # Same narrowing for the LogService model, consumed by the
  # mbt-rio-logservice conformance check below.
  logServiceModel = lib.fileset.toSource {
    root = modelsDir;
    fileset = modelsDir + "/logService.qnt";
  };

  # The TLC backend's quint->TLA+ conversion goes through the bundled
  # Apalache acting as a gRPC server. When `quint verify` has to spawn
  # that server itself, two failure modes bite:
  #   - the first conversion against a cold (un-JITed) server can die
  #     with an empty-details gRPC INTERNAL error for larger models
  #     (retryPolicy.qnt reproduces it deterministically; the same
  #     request against a warmed server succeeds in ~1 s), and
  #   - quint's own spawn path installs an uncaughtException handler
  #     that swallows exactly that error and exits 0 without ever
  #     running TLC — which a naive "exit 0 == proved" check would
  #     happily report as a green exhaustive proof.
  # Both constructors below therefore start the bundled Apalache server
  # themselves (so it persists across attempts and quint never installs
  # its swallow-and-exit handler), retry the verify against the
  # progressively warmed server, and accept a result only when quint's
  # own verdict line ("[ok] No violation found" / "[violation] Found an
  # issue") is present in the transcript — exit codes alone are not
  # trusted in either direction.
  apalacheServerPrelude = ''
    # Start the bundled Apalache server (the same distribution quint
    # would spawn) so it outlives individual quint invocations and stays
    # JIT-warm across retries. Its chatter goes to a side log, keeping
    # the transcript to quint + TLC output. Port 8822 is quint's default;
    # the sandbox's private network namespace keeps parallel checks from
    # colliding on it.
    apalache_jar=$(ls ${pkgs.quint}/share/quint/apalache-dist-*/apalache/lib/apalache.jar)
    ${pkgs.jdk21_headless}/bin/java -Xmx4096m -XX:+UseG1GC -jar "$apalache_jar" server --port=8822 \
      > apalache-server.log 2>&1 &
    apalache_pid=$!
    trap 'kill $apalache_pid 2>/dev/null || true' EXIT
    for _ in $(seq 1 90); do
      if (exec 3<>/dev/tcp/127.0.0.1/8822) 2>/dev/null; then
        exec 3>&- 3<&-
        break
      fi
      sleep 1
    done

    # The gRPC port opens well before the server can serve a LARGE
    # quint->TLA+ conversion: for roughly the first 30-60 s after startup
    # the conversion of a regime-sized module returns an empty-details
    # INTERNAL error while small modules convert fine, and the window is
    # long enough that three back-to-back verify attempts can all land
    # inside it (measured against retryPolicy.qnt's regime modules:
    # requests at +8/+14/+21 s after port-open fail, +58 s succeeds; the
    # server log shows no error — the request is simply not served yet).
    # Warm the server by running THE model conversion this check is about
    # to need, retrying until it succeeds, so quint verify below starts
    # against a server that has already proven it can convert this exact
    # module. $MODEL/$MAIN are the env attrs every check sets; $src is
    # the staged model fileset.
    for _ in $(seq 1 30); do
      if quint compile --target tlaplus --main=$MAIN \
          --server-endpoint=localhost:8822 "$src/$MODEL.qnt" > /dev/null 2>&1; then
        break
      fi
      sleep 5
    done

    # Run `quint verify "$@"`, retrying while the transcript carries no
    # verdict line (the residual cold-conversion failure shape, should
    # the warm-up above have raced the server). Sets quint_status; the
    # transcript of the accepted attempt is in $out.
    run_quint_verify() {
      quint_status=0
      for attempt in 1 2 3; do
        if quint verify --server-endpoint=localhost:8822 "$@" 2>&1 | tee $out; then
          quint_status=0
        else
          quint_status=$?
        fi
        if grep -qE '\[ok\] No violation found|\[violation\] Found an issue' $out; then
          return 0
        fi
        echo "attempt $attempt produced no TLC verdict (cold-server conversion failure?); retrying" >&2
        sleep 10
      done
      return 0
    }
  '';

  # One `quint verify` run per (model, main-module, invariant-set,
  # backend) tuple. `main` selects the module within the file — a model
  # with several fault regimes declares one core module plus one thin
  # instantiation module per regime, and each regime is its own check so
  # a regression in the core protocol surfaces in the small fast check
  # instead of buried in the largest one (the role a separate .cfg per
  # regime played for the hand-written-TLA+ predecessor of this file).
  mkQuintCheck =
    {
      name,
      spec,
      main ? spec,
      invariants,
      backend ? "tlc",
      # apalache backend only: the step bound. Not passed to the tlc
      # backend — TLC's exhaustive BFS has no step bound and feeding it
      # one would silently weaken the check's guarantee.
      maxSteps ? 10,
      # Additional .qnt files (relative to docs/spec/models, without
      # the extension) that `spec` imports. A module that overrides or
      # instantiates another model file needs that file staged alongside
      # it at the same relative position — quint resolves `from "../x"`
      # against the importing file's directory.
      extraSpecs ? [ ],
      # Non-default step action (the Stage-C calibration modules expose
      # their pre-fix transition relation as `calibStep` next to the
      # imported as-built `step`). null means quint's default (`step`).
      step ? null,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # Only the named .qnt files. A model that imports another file
        # (a shared harness, an override module's parent model) extends
        # the fileset via extraSpecs — keeping it narrow means an
        # unrelated docs/ edit doesn't re-run every quint check.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        # Surfaced in `nix log` and error messages.
        env.MODEL = spec;
        env.MAIN = main;
      }
      ''
        set -euo pipefail
        # Both backends write working files under the cwd (apalache:
        # `_apalache-out/`; tlc: a generated .tla/.cfg temp tree) and
        # $src is read-only — run from the writable scratch dir and
        # reference the spec by absolute path.
        cd "$TMPDIR"

        # Bound the checker to the cores Nix actually allotted this
        # build. The tlc backend's default is every visible host core,
        # which oversubscribes badly under nix-fast-build's parallel
        # checks gate (each TLC instance thinks it owns the box).
        # NIX_BUILD_CORES=0 means "all"; TLC's `auto` does the same, so
        # pass it through.
        # --tlc-config accepts only JVM/runtime knobs ({workers,
        # maxHeap, stackSize}) — it is NOT a .cfg-directive escape
        # hatch; everything else lives in the .qnt.
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers='"auto"'
        printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPrelude}

        # The transcript is the proof artifact: it records the
        # invariants checked, the state count, and the depth — keep it.
        # (`| tee` is pipefail-safe — tee consumes everything; never
        # replace it with `| head`, see
        # .claude/rules/ci-failure-patterns.md.)
        run_quint_verify \
          --backend=${backend} \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant=${lib.concatStringsSep "," invariants} \
          ${
            if backend == "tlc" then "--tlc-config=tlc-config.json" else "--max-steps=${toString maxSteps}"
          } \
          "$src/${spec}.qnt"

        # "Proved" requires the checker's own verdict, not just a zero
        # exit: a quint crash that never reached TLC must never read as
        # a green exhaustive proof.
        if [ "$quint_status" -ne 0 ]; then
          echo "" >&2
          echo "${name}: quint verify failed (status $quint_status) — see the transcript above." >&2
          exit 1
        fi
        if ! grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: quint verify exited 0 but reported no verdict (tool error / conversion failure?)." >&2
          exit 1
        fi
      '';

  # The dual of mkQuintCheck: an expect-violation (non-vacuity) check.
  # `quint verify` runs against a single witness predicate of the form
  # "the interesting thing never happens", and the check PASSES only
  # when the checker reports a violation -- machine-checked evidence
  # that the scenario is still reachable in that regime's explored
  # space. A witness that stops being violated means the regime's
  # headline invariant has gone vacuous (the contention it constrains
  # can no longer arise), which previously surfaced only as
  # "re-verify by hand after a constant change" notes in the model's
  # module comments.
  #
  # One witness per check: a conjunction of witnesses is violated as
  # soon as ANY conjunct is, so a reachable witness would mask an
  # unreachable one.
  #
  # Failure modes are distinguished: "no violation found" (the vacuity
  # signal) and tool errors (typecheck failure, checker crash) both
  # fail, with different messages -- success requires the checker's own
  # violation report in the transcript, not just a nonzero exit.
  #
  # tlc backend only: TLC stops at the first violation, so these runs
  # are far cheaper than the exhaustive proofs (timings live in each
  # check's transcript and the introducing commit's message).
  mkQuintWitnessCheck =
    {
      name,
      spec,
      main ? spec,
      # The witness `val` expected to be violated.
      witness,
      # Same semantics as mkQuintCheck's extraSpecs.
      extraSpecs ? [ ],
      # Same semantics as mkQuintCheck's step (used by the Stage-C
      # calibration checks to select the pre-fix transition relation).
      step ? null,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # Same fileset narrowing as mkQuintCheck.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          WITNESS = witness;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # Same worker bound as mkQuintCheck (see its comment).
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers='"auto"'
        printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPrelude}

        # The violation is the EXPECTED outcome, so the verify call's
        # nonzero exit must not abort the script -- the retry helper
        # captures the status instead. The transcript (including the
        # counterexample trace, which is the reachability evidence) is
        # the check's output.
        run_quint_verify \
          --backend=tlc \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant=${witness} \
          --tlc-config=tlc-config.json \
          "$src/${spec}.qnt"

        if [ "$quint_status" -eq 0 ] && grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: witness ${witness} was NOT violated in ${main}." >&2
          echo "The scenario it probes is no longer reachable; the regime's invariants may now hold vacuously." >&2
          exit 1
        fi

        # A nonzero exit alone is not enough: a typecheck error or a
        # checker crash also exits nonzero. Require quint's own
        # violation report.
        if ! grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: quint verify failed without reporting a violation of ${witness} (tool error?)." >&2
          exit 1
        fi
      '';

  # Deterministic named-run replay: `quint test` over a regime module's
  # `run` definitions. The runs are the model's executable scenario pins
  # (the documented-divergence reproducers and the happy-path narratives);
  # this check keeps them green as the model evolves, the same way a unit
  # test pins a hand-computed history. Cheap (no state-space exploration),
  # so one check per regime module is fine.
  #
  # The match pattern is anchored to the `...Run` naming convention so a
  # regime with no runs fails loudly (zero matches) instead of silently
  # passing -- a renamed run must not drop out of CI unnoticed.
  mkQuintRunCheck =
    {
      name,
      spec,
      main ? spec,
      match ? ".*Run$",
      extraSpecs ? [ ],
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          MATCH = match;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        quint test \
          --main=${main} \
          --match='${match}' \
          "$src/${spec}.qnt" 2>&1 | tee $out

        # `quint test` exits nonzero on a failing run; additionally require
        # that at least one run actually matched and passed, so a rename
        # that empties the match cannot turn into a silent green check.
        if ! grep -qE ' [1-9][0-9]* passing' $out; then
          echo "" >&2
          echo "${name}: no runs matched '${match}' in ${main}." >&2
          exit 1
        fi
      '';
in
{
  # Expose the constructors so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # attrs below.
  inherit mkQuintCheck mkQuintWitnessCheck mkQuintRunCheck;

  # Per-model checks. Spliced into checks.* via misc-checks.nix.
  checks = {
    # rio-lease's leader-election protocol over a Kubernetes Lease
    # object: per-node clocks (bounded skew), the observed-record
    # staleness clock, local self-fencing, crash/recovery, and the
    # write-ahead generation claim. Ported from (and replacing) the
    # hand-written TLA+ model; its r[verify ...] markers moved here
    # with it.
    # The base regime disables every operator/infrastructure fault so a
    # regression in the core protocol surfaces in this check rather than
    # buried in the larger fault-injection regimes.
    #
    # It verifies atMostOneCASWinner (the apiserver admits at most one
    # writer per resourceVersion), boundedDualLeadership (every
    # dual-belief state has a discovery mechanism armed),
    # staleLeaderHasStaleGeneration (concurrent believers always have
    # distinct generations -- the bridge to the executor-side generation
    # fence; the invariant the pre-fix protocol falsified), plus the
    # loopInterval and clockSkewBound precondition tripwires and the
    # boundsOK ceiling tripwire. The fetch-max-seed marker covers the
    # seeding-and-claiming encoding inside steal (the generation derives
    # from the lease's transition count and the PG floor advances at
    # acquisition time). The model collapses acquire/seed/claim into one
    # atomic step; the unobserved-holder-change window that collapse
    # hides is discharged at the implementation level by
    # sched.recovery.bump-confirm (a claim target the durable floor
    # cannot vouch for requires a post-claim Leading round) and by
    # sched.lease.rebound (a still-leading round that observes a moved
    # transition count re-runs recovery) -- see the steal action doc.
    #
    # The state count, depth, and wall-clock are in this check's output
    # transcript and in the commit that introduced (or last re-measured)
    # the model -- never here. The durable claim: the port's state count
    # is exactly |NODES|! times the symmetry-reduced TLA+ predecessor's,
    # minus the self-symmetric states -- the structural-identity
    # evidence that the two state spaces are the same space.
    # r[verify sched.lease.at-most-one-leader+3]
    # r[verify sched.lease.k8s-lease+2]
    # r[verify sched.recovery.fetch-max-seed+4]
    quint-leader-election = mkQuintCheck {
      name = "leader-election";
      spec = "leaderElection";
      main = "leaderElectionBase";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
      ];
    };

    # The same model with the Lease-object-deletion fault enabled
    # (MAX_DELETES = 1): `kubectl delete lease` resets the
    # leaseTransitions counter the generation derives from — the one
    # fault that re-arms the generation-collision failure mode the
    # transition-count derivation closed — while PG survives. The
    # write-ahead claim (the generation is durably recorded in PG's
    # claims ledger inside the acquisition step, before dispatch is
    # ungated) is what survives it: the checker proves
    # staleLeaderHasStaleGeneration still holds with the fault
    # injected, and the deliberately-weakened run (the claim reverted
    # to the pre-claim protocol) is falsified with a shallow
    # steal-delete-steal trace — two believers at the same generation.
    # See leaderElectionDeletion's module comment for the red-first
    # procedure and the rest of the regime's non-vacuity evidence.
    # r[verify sched.lease.generation-claim+2]
    quint-leader-election-deletion = mkQuintCheck {
      name = "leader-election-deletion";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
      ];
    };

    # The same model under the asymmetric-TTL constants — THE HEALTHY
    # REGIME. The base and deletion regimes keep zero fence/steal
    # separation and model the degraded case, where dual belief is
    # reachable and boundedDualLeadership (every dual-belief state has
    # a discovery mechanism armed) is the operative property. This
    # regime gives the model the separation the asymmetric-TTL change
    # gives production (fence at LEASE_TTL - FENCE_MARGIN, steal at
    # LEASE_TTL + FENCE_MARGIN), and the operative property upgrades to
    # neverDual: no two replicas ever simultaneously believe they lead,
    # over the full state space. neverDual holds iff
    # STEAL_AFTER - FENCE_AFTER >= RENEW + 2*MAX_SKEW (production:
    # 2*FENCE_MARGIN >= RENEW_INTERVAL + 2*clock_skew — an 8s
    # separation against a 5s renew interval leaves a 1.5s skew
    # budget, which is rio-lease's compile-time assertion). The
    # boundary is measured from both sides — one tick less separation
    # and neverDual is violated — and the second-acquisition
    # reachability probe proves the contention it constrains actually
    # happens. See leaderElectionAsymmetric's module comment for the
    # boundary procedure and the non-vacuity evidence; the measured
    # depths are in the introducing commit. neverDual is the
    # verification of both the self-fence ordering claim (the victim
    # has provably stopped believing before any thief steals) and the
    # at-most-one-leader soft half (the dual-belief window is empty
    # under bounded skew).
    # r[verify sched.lease.at-most-one-leader+3]
    # r[verify sched.lease.self-fence+2]
    quint-leader-election-asymmetric = mkQuintCheck {
      name = "leader-election-asymmetric";
      spec = "leaderElection";
      main = "leaderElectionAsymmetric";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "neverDual"
      ];
    };

    # The same model with both PostgreSQL-side faults enabled and the
    # Lease-deletion fault disabled (MAX_CLAIM_FAILURES = 1,
    # MAX_RESTORES = 1, MAX_DELETES = 0): a claim INSERT that fails
    # between the seed read and the claim write while recovery proceeds
    # anyway (the production proceed-on-failure path), and a
    # point-in-time restore that regresses the floor to zero. Every
    # invariant still holds: the Lease object's transition count is an
    # independent epoch source, so each PG fault alone is survivable —
    # the Lease and the PG ledger are REDUNDANT epoch sources and a
    # generation collision requires destroying both. The boundary is
    # measured: re-enabling the deletion fault alongside either PG
    # fault violates staleLeaderHasStaleGeneration with a shallow
    # steal-delete-steal trace (see leaderElectionPgFaults's module
    # comment for the conjunction-evidence procedure; the trace
    # summaries and depths are in the introducing commit). Those
    # conjunctions are the documented, accepted residuals of the
    # proceed-on-failure choice and of relying on PG as the
    # post-deletion backstop; this check pins the claim that they are
    # the ONLY ways a PG-side fault reaches a collision.
    # r[verify sched.lease.generation-claim+2]
    quint-leader-election-pg-faults = mkQuintCheck {
      name = "leader-election-pg-faults";
      spec = "leaderElection";
      main = "leaderElectionPgFaults";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
      ];
    };

    # Non-vacuity witnesses. Each regime's headline invariant constrains
    # a scenario that must itself be REACHABLE for the green check to
    # mean anything; each check below passes only when the checker
    # violates its witness -- the machine-checked replacement for the
    # "re-verify by hand after a constant change" notes in the model's
    # module comments. Deliberately no tracey markers here: the spec
    # rules are verified by the regime checks above; these guard those
    # checks against going vacuous.

    # Dual belief is reachable in the base regime: the antecedent of
    # boundedDualLeadership and staleLeaderHasStaleGeneration actually
    # arises (a deposed leader that has not yet noticed its loss).
    # neverDual doubles as the witness -- it is the asymmetric regime's
    # headline invariant and the base regime's reachability probe.
    quint-leader-election-witness-dual-belief = mkQuintWitnessCheck {
      name = "leader-election-witness-dual-belief";
      spec = "leaderElection";
      main = "leaderElectionBase";
      witness = "neverDual";
    };

    # A second acquisition (a steal of a previously-held lease) is
    # reachable in the asymmetric regime: neverDual holds there because
    # of the fence/steal separation, not because the widened threshold
    # made contention unreachable within the clock ceiling.
    quint-leader-election-witness-second-acquisition = mkQuintWitnessCheck {
      name = "leader-election-witness-second-acquisition";
      spec = "leaderElection";
      main = "leaderElectionAsymmetric";
      witness = "atMostOneAcquisition";
    };

    # The steal-delete-steal sequence is explored in the deletion
    # regime: its staleLeaderHasStaleGeneration verdict is about a state
    # space that actually contains the post-deletion re-acquisition the
    # write-ahead claim defends against.
    quint-leader-election-witness-deletion-resteal = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-resteal";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noReacquisitionAfterDeletion";
    };

    # The retain-on-own-claim re-acquisition is explored in the deletion
    # regime: a deposed-by-deletion holder re-steals its recreated lease
    # and retains its entry generation because the claims-ledger row at
    # the floor is its own -- the fetch-max-seed / generation-claim
    # retain clause the regime checks claim to verify, exercised rather
    # than merely expressible.
    quint-leader-election-witness-deletion-retain = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-retain";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noRetainAfterDeletion";
    };

    # The foreign-tie bump is explored in the deletion regime: after a
    # deleteLease the surviving claims-ledger floor (owned by the deletion
    # victim) ties a foreign stealer's restarted entry generation and must
    # be exceeded -- the bump that keeps staleLeaderHasStaleGeneration
    # true past the deletion. A ceiling or guard change that silently
    # stranded that arm of seedFor would leave the regime check green
    # while the tie case went unexplored; this check pins it.
    quint-leader-election-witness-deletion-foreign-tie = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-foreign-tie";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noForeignTieBumpAfterDeletion";
    };

    # The floor-above-entry bump is explored in the deletion regime: a
    # holder deposed by deleteLease that crashes and recovers re-acquires
    # on the renew edge, and the surviving claims-ledger floor exceeds
    # what the recreated lease's transition count can vouch for, so the
    # claim path must bump past it -- the restore the deletion-regime
    # header calls LOAD-BEARING for staleLeaderHasStaleGeneration. A
    # ceiling or guard change that silently stranded that arm would leave
    # the deletion regime check green while the proof claim quietly
    # narrowed; this check pins it. A red here means the renew-edge
    # instance of the arm is no longer reachable: either a deliberate
    # regime-constant/guard change (adjust the constants or retire this
    # check with the same deliberation) or the regression this check
    # exists to catch.
    quint-leader-election-witness-deletion-floor-bump = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-floor-bump";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noFloorBumpAfterDeletion";
    };

    # The claim-INSERT failure is explored in the pg-faults regime: its
    # "each PG fault alone is survivable" verdict is about a state space
    # in which the proceed-on-failure path actually fires, not one where
    # the fault never bites.
    quint-leader-election-witness-claim-failure = mkQuintWitnessCheck {
      name = "leader-election-witness-claim-failure";
      spec = "leaderElection";
      main = "leaderElectionPgFaults";
      witness = "noClaimFailure";
    };

    # The restore-then-reclaim sequence is explored in the pg-faults
    # regime: a point-in-time restore zeroes the floor and a later
    # acquisition's claim re-raises it, so surviving the floor
    # regression is a property of the explored space rather than a
    # vacuous one.
    quint-leader-election-witness-restore-reclaim = mkQuintWitnessCheck {
      name = "leader-election-witness-restore-reclaim";
      spec = "leaderElection";
      main = "leaderElectionPgFaults";
      witness = "noReclaimAfterRestore";
    };

    # ------------------------------------------------------------------
    # rio-store's LogService: the build-log session/chunk/dedup protocol
    # (model C of the log-formal campaign — the successor to the retired
    # in-scheduler logBufferLifecycle model). The model covers the
    # builder's at-least-once ack-trimmed uploader, the open-time
    # binding + completeness gate, the per-stream ingest session and its
    # accept predicate, the immutable chunk manifest, the read path's
    # ordered-walk dedup over overlapping sessions, the completeness
    # fold, and the TTL sweep. The acceptance test for the model is the
    # 22-bug calibration table in docs/spec/models/log-invariant-map.md
    # re-run against this architecture. State counts, depths, and
    # wall-clocks are in the introducing commit's message and each
    # check's transcript.
    #
    # Marker scope: append-auth, completeness-gate, session-keyed, and
    # exec-keyed are marked on the regime that makes each load-bearing.
    # store.log.chunk-immutable is NOT marked (the model has no
    # overwrite action, so immutability is by construction rather than
    # checked; the seq-burning and object-before-manifest halves are
    # unit-tested code branches). store.log.ingest-bounds and
    # store.log.tail-reconnect are NOT marked (resource bounds and the
    # since_line reconnect cursor are outside the model's scope — see
    # the model header's priced omissions).
    # ------------------------------------------------------------------

    # The single-writer core: one execution, one session, the
    # adversarial batch stream (fabricated gaps, re-sends, lines past
    # the recorded final count) against the accept predicate, the
    # per-append completeness ceiling, the contiguous-prefix cut, the
    # read fold, and the completeness fold. ackImpliesDurable is NOT
    # asserted here: the fabricating client can poison its own ack
    # watermark (see the val's doc in the model) and the per-ack
    # durability claim is only made for the honest uploader.
    # r[verify store.log.completeness-gate]
    quint-log-service-base = mkQuintCheck {
      name = "log-service-base";
      spec = "logService";
      main = "logServiceBase";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Two executions of one derivation: the supersession scenarios. The
    # auth gate rejects the superseded execution's reopen; the
    # superseded execution's still-open session keeps writing to its own
    # (old) execution's log only; the two manifests grow concurrently
    # under disjoint exec-keyed namespaces.
    # r[verify store.log.append-auth]
    # r[verify obs.log.exec-keyed+2]
    quint-log-service-redispatch = mkQuintCheck {
      name = "log-service-redispatch";
      spec = "logService";
      main = "logServiceRedispatch";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Two sessions for one execution: the at-least-once re-send shape.
    # A chunk commits but its ack is lost, the builder replays the same
    # lines to a fresh session, the detached predecessor's drain commits
    # overlapping chunks, and the read path's (first_line, session_id)
    # ordered walk serves each line exactly once. The honest uploader's
    # ack-implies-durable refinement is asserted here (no fabrication).
    # r[verify store.log.session-keyed]
    quint-log-service-resend = mkQuintCheck {
      name = "log-service-resend";
      spec = "logService";
      main = "logServiceResend";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # The TTL sweep against a live ingest pipeline: the expired-only
    # guard, the chunks-before-execution-row deletion order with a cut
    # interleavable between the two DELETEs, and the disclosed-loss
    # accounting for swept lines.
    quint-log-service-sweep = mkQuintCheck {
      name = "log-service-sweep";
      spec = "logService";
      main = "logServiceSweep";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Non-vacuity witnesses for the LogService regimes. Each check
    # passes only when the checker violates its witness — machine-checked
    # evidence that the scenario a regime's headline invariant constrains
    # is actually reachable in that regime's explored space. Deliberately
    # no tracey markers here: the spec rules are verified by the regime
    # checks above; these guard those checks against going vacuous.

    # A log reads complete in the base regime: the conservation law and
    # the open-time seal only bite against a complete log.
    quint-log-service-witness-completed-log = mkQuintWitnessCheck {
      name = "log-service-witness-completed-log";
      spec = "logService";
      main = "logServiceBase";
      witness = "noCompletedLog";
    };

    # The per-append completeness ceiling actually drops or truncates a
    # batch — the post-terminal injection the gate exists for is
    # attempted, not merely encodable.
    quint-log-service-witness-past-final = mkQuintWitnessCheck {
      name = "log-service-witness-past-final";
      spec = "logService";
      main = "logServiceBase";
      witness = "noPastFinalRejection";
    };

    # The monotone floor actually rejects a batch numbered below the
    # session's high-water mark.
    quint-log-service-witness-non-monotone = mkQuintWitnessCheck {
      name = "log-service-witness-non-monotone";
      spec = "logService";
      main = "logServiceBase";
      witness = "noNonMonotoneRejection";
    };

    # A session learns its completeness ceiling AFTER accepting lines
    # (the seal lands mid-stream): the case that distinguishes the
    # per-append gate from the open-time gate and produces the disclosed
    # pre-refresh residual completenessGate carves out.
    quint-log-service-witness-mid-stream-ceiling = mkQuintWitnessCheck {
      name = "log-service-witness-mid-stream-ceiling";
      spec = "logService";
      main = "logServiceBase";
      witness = "noMidStreamCeiling";
    };

    # An uploader abandons its drain with un-acked lines: the
    # disclosed-loss channel the conservation law's fourth disjunct
    # routes through actually fires.
    quint-log-service-witness-abandoned = mkQuintWitnessCheck {
      name = "log-service-witness-abandoned";
      spec = "logService";
      main = "logServiceBase";
      witness = "noAbandonedWithUnacked";
    };

    # A manifest carries a forward gap: the completeness fold's gap arm
    # and the cut's split-at-gaps behavior are exercised.
    quint-log-service-witness-gapped-manifest = mkQuintWitnessCheck {
      name = "log-service-witness-gapped-manifest";
      spec = "logService";
      main = "logServiceBase";
      witness = "noGappedManifest";
    };

    # The open-time seal rejects a stream open for an already-complete
    # log.
    quint-log-service-witness-complete-open-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-complete-open-rejected";
      spec = "logService";
      main = "logServiceBase";
      witness = "noCompleteOpenRejection";
    };

    # Two chunks of one execution from different sessions overlap in
    # line range: the re-sent-batch-after-ambiguous-disconnect shape the
    # read path's dedup exists for. servedSpanExact's verdict in the
    # resend regime is about a state space that actually contains the
    # overlap.
    quint-log-service-witness-overlap = mkQuintWitnessCheck {
      name = "log-service-witness-overlap";
      spec = "logService";
      main = "logServiceResend";
      witness = "noOverlappingChunks";
    };

    # The read path's watermark actually suppresses a duplicate (the
    # total yield count falls short of the sum of the chunks' claimed
    # counts): the dedup is exercised, not merely encoded.
    quint-log-service-witness-dedup = mkQuintWitnessCheck {
      name = "log-service-witness-dedup";
      spec = "logService";
      main = "logServiceResend";
      witness = "noDuplicateSuppressed";
    };

    # The auth gate rejects a superseded execution's reopen — the
    # foreign-token rejection authGateExcludesForeignWriters is about.
    quint-log-service-witness-superseded-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-superseded-rejected";
      spec = "logService";
      main = "logServiceRedispatch";
      witness = "noSupersededOpenRejection";
    };

    # Two executions' logs grow concurrently (the superseded execution's
    # session still holds lines while both manifests are non-empty): the
    # contended state noCrossExecContamination constrains.
    quint-log-service-witness-concurrent-execs = mkQuintWitnessCheck {
      name = "log-service-witness-concurrent-execs";
      spec = "logService";
      main = "logServiceRedispatch";
      witness = "noConcurrentExecWriters";
    };

    # The TTL sweep deletes a non-empty manifest: the sweep's deletion
    # arm is reachable and the disclosed-loss accounting for swept lines
    # is exercised against a real deletion.
    quint-log-service-witness-swept = mkQuintWitnessCheck {
      name = "log-service-witness-swept";
      spec = "logService";
      main = "logServiceSweep";
      witness = "noSweptChunks";
    };

    # ------------------------------------------------------------------
    # rio-scheduler's retry/poison/cascade machinery: the post-collapse
    # model (retry-formal Phase 1c). retryPolicy.qnt encodes the code as
    # it exists after the Phase-1b nine-site collapse -- every entry
    # point's verdict is the reference fold (decide()) over the durable
    # attempt ledger, evaluated and persisted in the appending
    # transaction -- and the Stage-B as-built encoding is frozen at
    # retryPolicyAsBuilt.qnt, imported only by the calibration corpus
    # below. Checks come in three flavours:
    #   - the per-regime HOLD checks below (exhaustive TLC), now
    #     including the invariants whose as-built falsifications were
    #     pre-registered at Stage B and fixed by Phase 1b: the D1
    #     verdict-channel divergence, the D2/D3 controller-channel
    #     counter divergences, the D4 unmirrored backstop charge and the
    #     C2 unbounded no-report crash loop (their former
    #     expect-violation checks are retired -- the flip is the Phase-1
    #     acceptance evidence, not a silent edit);
    #   - the witness checks (non-vacuity: every cap, reset, channel
    #     race, establishment and fault the invariants quantify over is
    #     reachable);
    #   - the calibration checks (the historical-fix corpus replayed
    #     against the FROZEN as-built encoding).
    # The named-run checks replay the deterministic reproducer runs (one
    # per formerly-documented divergence, now ending in the adjudicated
    # outcomes) so the precise documented shape stays pinned even though
    # TLC's BFS may report a different counterexample first. State
    # counts, depths and wall-clocks live in the introducing commits'
    # messages and the checks' transcripts.
    #
    # Marker scope: the sched.retry.* rules whose verification the
    # invariant map defers to this model are marked on the regime that
    # makes each load-bearing; counters-refine-history / attempts-bounded
    # / transient-budget gain the model-checked form here on top of the
    # reference fold's unit-test markers. sched.retry.verdict-channel-
    # invariant is marked on the dual regime, the regime whose
    # mixed-channel alphabet exercises it (it moved here from the retired
    # expect-violation D1 check when Phase 1b landed the adjudicated fix
    # and Phase 1c flipped the check).
    # ------------------------------------------------------------------

    # The worker-channel regime: every failure is worker-reported, two
    # executor slots, resets enabled. This is the exhaustive re-proof of
    # the fold-backed entry points over the full worker alphabet
    # (fencepost conventions, the I-127 window reset and its exempt
    # fall-through, the backoff arming, the threshold/fleet/cap
    # ordering), of the refinement tripwires (the cached view, the
    # durable ledger fold and the reference fold advance together), and
    # of the placement/threshold scenarios that need a second slot.
    # r[verify sched.retry.counters-refine-history+2]
    # r[verify sched.retry.transient-budget]
    # r[verify sched.retry.attempts-bounded+2]
    quint-retry-policy-worker = mkQuintCheck {
      name = "retry-policy-worker";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      invariants = [
        "boundsOK"
        "countersRefineHistory"
        "verdictMatchesFold"
        "attemptsChargedOnce"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "recoveryNeverFabricatesFailures"
        "durableMirrorsCharges"
        "placementSound"
        "clearedPoisonClearsDurably"
        "clearedPoisonScrubsExclusions"
      ];
    };

    # The dual-channel regime: pod deaths, the controller report channel
    # (race-ahead / late-installment / loss), the establishment of
    # never-classified deaths, the wedge backstop and the no-report crash
    # on one slot. The HOLD set here is the dedup and terminal-state
    # discipline (one physical death counts at most once across every
    # observation subset and order, poison stays terminal, the cascade
    # reaches exactly the dependent, nothing fabricates failures) PLUS
    # the refinement invariants whose falsification on this regime was
    # pre-registered against the as-built code and fixed by Phase 1b:
    # verdictMatchesFold (D1 -- the controller-observed timeout cap now
    # ends Cancelled), countersRefineHistory (D2/D3 -- the at-cap anchor
    # stamp and the promoted exempt charge on the controller channel) and
    # durableMirrorsCharges (D4 -- the backstop charge is a durable
    # ledger row). Their former expect-violation checks
    # (quint-retry-policy-divergence-*) are retired by this flip.
    # r[verify sched.retry.no-double-count]
    # r[verify sched.poison.cascade-dependents]
    # r[verify sched.retry.verdict-channel-invariant]
    quint-retry-policy-dual = mkQuintCheck {
      name = "retry-policy-dual";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      invariants = [
        "boundsOK"
        "attemptsChargedOnce"
        "countersRefineHistory"
        "verdictMatchesFold"
        "durableMirrorsCharges"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "recoveryNeverFabricatesFailures"
        "placementSound"
        "clearedPoisonClearsDurably"
        "clearedPoisonScrubsExclusions"
      ];
    };

    # The crash regime: the C2 no-report hard-crash loop in isolation,
    # now with the establishment charge (Phase 1b T-1b.11). Every crash
    # is established and charged, so the loop terminates and the
    # boundedness clause HOLDS: attemptsBoundedGlobal joins this regime's
    # invariant list (the former expect-violation
    # quint-retry-policy-crash-unbounded check is retired by the flip;
    # the establishment-charge and crash-terminal witnesses below keep
    # the regime's reachability evidence).
    quint-retry-policy-crash = mkQuintCheck {
      name = "retry-policy-crash";
      spec = "retryPolicy";
      main = "retryPolicyCrash";
      invariants = [
        "boundsOK"
        "attemptsChargedOnce"
        "attemptsBoundedGlobal"
        "countersRefineHistory"
        "verdictMatchesFold"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "recoveryNeverFabricatesFailures"
        "durableMirrorsCharges"
        "placementSound"
      ];
    };

    # The fault / failover regime: the dual-channel alphabet plus one
    # leader failover and one appending-transaction failure. The
    # post-collapse recovery contract is load-bearing here: the recovered
    # retry view is the fold over the durable attempt ledger -- budgets,
    # the window anchor, the exclusion set and the poison set all survive
    # a leader change (failoverPreservesHistory, the Phase-1 acceptance
    # property of sched.retry.failover-budget), nothing is forgiven and
    # nothing is fabricated, the recovered view never diverges from the
    # durable fold (the seeded-fold recovery contract of
    # sched.retry.recovery-projection+2; the transitional pre-066 legacy
    # seed is code-level and covered by the rio-scheduler recovery
    # tests), and a failed appending transaction charges nothing at all
    # (the event is re-delivered) instead of leaving the durable view
    # behind the in-memory one. Reset events stay enabled in this regime
    # and the failover-with-history witness below keeps the contended
    # state's reachability machine-checked.
    # r[verify sched.retry.failover-budget]
    # r[verify sched.retry.recovery-projection+2]
    quint-retry-policy-failover = mkQuintCheck {
      name = "retry-policy-failover";
      spec = "retryPolicy";
      main = "retryPolicyFailover";
      invariants = [
        "boundsOK"
        "attemptsChargedOnce"
        "countersRefineHistory"
        "verdictMatchesFold"
        "durableMirrorsCharges"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "failoverPreservesHistory"
        "recoveryNeverFabricatesFailures"
        "placementSound"
        "clearedPoisonClearsDurably"
        "clearedPoisonScrubsExclusions"
        "recoveryPreservesPoisonStatus"
      ];
    };

    # The deterministic reproducer runs, one check per regime: the
    # documented-divergence shapes (D1/D2/D3/D4/C2), the dedup scenarios,
    # the selective-forgiveness failover and the budget walkthroughs are
    # replayed step by step and their expectations re-asserted.
    quint-retry-policy-runs-worker = mkQuintRunCheck {
      name = "retry-policy-runs-worker";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
    };
    quint-retry-policy-runs-dual = mkQuintRunCheck {
      name = "retry-policy-runs-dual";
      spec = "retryPolicy";
      main = "retryPolicyDual";
    };
    quint-retry-policy-runs-crash = mkQuintRunCheck {
      name = "retry-policy-runs-crash";
      spec = "retryPolicy";
      main = "retryPolicyCrash";
    };
    quint-retry-policy-runs-failover = mkQuintRunCheck {
      name = "retry-policy-runs-failover";
      spec = "retryPolicy";
      main = "retryPolicyFailover";
    };

    # The pre-registered Stage-B falsification checks
    # (quint-retry-policy-divergence-{verdict,counters,durable} and
    # quint-retry-policy-crash-unbounded) are retired: Phase 1b fixed the
    # documented defects they reproduced (D1/D2/D3/D4/C2) and the
    # corresponding invariants joined the dual and crash regime HOLD
    # lists above -- the flip the invariant map's Stage-B section
    # pre-registered as the Phase-1 acceptance criterion. The
    # deterministic reproducer runs survive in the named-run checks with
    # their post-collapse (adjudicated) outcomes, and the establishment /
    # crash-terminal / tx-failure witnesses below replace the retired
    # checks' reachability evidence.

    # Non-vacuity witnesses for the retryPolicy regimes. Each check
    # passes only when the checker violates its witness -- machine-checked
    # evidence that the scenario a regime's invariants constrain is
    # actually reachable in that regime's explored space. Deliberately no
    # tracey markers here (same policy as the other models' witnesses).

    # Every budget terminal is reachable in the worker regime: the
    # distinct-worker threshold, the non-exempt infra cap, the exempt
    # infra cap, and the worker-reported timeout cap's Cancelled. (The
    # per-cycle transient cap is deliberately NOT witnessed: under the
    # production distinct-workers threshold and hard_filter's exclusion
    # the same worker never fails the same derivation twice in a cycle,
    # so the threshold always fires first -- recorded in the invariant
    # map's Stage-B section.)
    quint-retry-policy-witness-threshold = mkQuintWitnessCheck {
      name = "retry-policy-witness-threshold";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noThresholdPoison";
    };
    quint-retry-policy-witness-infra-cap = mkQuintWitnessCheck {
      name = "retry-policy-witness-infra-cap";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noInfraCapPoison";
    };
    quint-retry-policy-witness-exempt-cap = mkQuintWitnessCheck {
      name = "retry-policy-witness-exempt-cap";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noExemptCapPoison";
    };
    quint-retry-policy-witness-timeout-cancel = mkQuintWitnessCheck {
      name = "retry-policy-witness-timeout-cancel";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noTimeoutCapCancel";
    };

    # The I-127 window reset fires, and fires on an exempt under-cap
    # event (the as-built fall-through the corrected fold reproduces).
    quint-retry-policy-witness-window-reset = mkQuintWitnessCheck {
      name = "retry-policy-witness-window-reset";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noWindowReset";
    };
    quint-retry-policy-witness-exempt-fallthrough = mkQuintWitnessCheck {
      name = "retry-policy-witness-exempt-fallthrough";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noExemptFallthroughReset";
    };

    # The poison lifecycle's clears are reachable: the TTL expiry and the
    # cache-hit clear (the resubmit reset is pinned by the named runs).
    quint-retry-policy-witness-ttl-expiry = mkQuintWitnessCheck {
      name = "retry-policy-witness-ttl-expiry";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noTtlExpiry";
    };
    quint-retry-policy-witness-cache-hit = mkQuintWitnessCheck {
      name = "retry-policy-witness-cache-hit";
      spec = "retryPolicy";
      main = "retryPolicyWorker";
      witness = "noCacheHitClear";
    };

    # The controller channel's interesting interleavings are reachable in
    # the dual regime: the controller-observed timeout cap ending
    # terminal Cancelled (the post-collapse D1 shape -- the as-built
    # poison witness has no producer any more, so this witness was
    # re-pointed at the Cancelled terminal with the Phase-1c flip), the
    # promoted and at-cap controller terminations (the D3 / D2 arms), the
    # late installment (a report correlated through recently_disconnected
    # after the disconnect already requeued the derivation), the
    # race-ahead report, and the dispatch-time fleet-exhaust poison.
    quint-retry-policy-witness-controller-cap = mkQuintWitnessCheck {
      name = "retry-policy-witness-controller-cap";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noControllerCapCancelled";
    };
    quint-retry-policy-witness-promoted-termination = mkQuintWitnessCheck {
      name = "retry-policy-witness-promoted-termination";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noPromotedTermination";
    };
    quint-retry-policy-witness-atcap-termination = mkQuintWitnessCheck {
      name = "retry-policy-witness-atcap-termination";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noAtCapTermination";
    };
    quint-retry-policy-witness-late-installment = mkQuintWitnessCheck {
      name = "retry-policy-witness-late-installment";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noLateInstallment";
    };
    quint-retry-policy-witness-race-ahead = mkQuintWitnessCheck {
      name = "retry-policy-witness-race-ahead";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noRaceAheadReport";
    };
    quint-retry-policy-witness-fleet-exhaust = mkQuintWitnessCheck {
      name = "retry-policy-witness-fleet-exhaust";
      spec = "retryPolicy";
      main = "retryPolicyDual";
      witness = "noFleetExhaustPoison";
    };

    # The establishment charge is reachable and the crash loop now
    # terminates: the C2 fix's two non-vacuity probes in the crash
    # regime, replacing the retired quint-retry-policy-crash-unbounded
    # expect-violation check's reachability role (that check's
    # boundedness claim itself is now the attemptsBoundedGlobal HOLD in
    # the crash regime above).
    quint-retry-policy-witness-crash-charge = mkQuintWitnessCheck {
      name = "retry-policy-witness-crash-charge";
      spec = "retryPolicy";
      main = "retryPolicyCrash";
      witness = "noEstablishedCrashCharge";
    };
    quint-retry-policy-witness-crash-terminal = mkQuintWitnessCheck {
      name = "retry-policy-witness-crash-terminal";
      spec = "retryPolicy";
      main = "retryPolicyCrash";
      witness = "noCrashLoopTerminal";
    };

    # The failover / fault regime's contended states are reachable: a
    # failover lands on a non-empty under-budget history, and an
    # appending transaction actually fails (the bounded re-delivery
    # path). The latter replaces the as-built lost-mirror-write witness
    # (quint-retry-policy-witness-pg-write-lost, retired): post-066 the
    # charge, the verdict and the status persist commit or fail as one
    # transaction, so the independently-lost mirror write it probed is
    # structurally impossible.
    quint-retry-policy-witness-failover-history = mkQuintWitnessCheck {
      name = "retry-policy-witness-failover-history";
      spec = "retryPolicy";
      main = "retryPolicyFailover";
      witness = "noFailoverWithHistory";
    };
    quint-retry-policy-witness-tx-failure = mkQuintWitnessCheck {
      name = "retry-policy-witness-tx-failure";
      spec = "retryPolicy";
      main = "retryPolicyFailover";
      witness = "noAttemptTxFailure";
    };

    # Stage-C calibration witnesses (the historical-fix corpus replayed
    # against the model). Each check instantiates the as-built model,
    # swaps ONE entry point for its PRE-FIX behavior (the calibration
    # module's `calibStep`), and passes only while the checker still
    # falsifies the invariant the corresponding historical fix protects --
    # machine-checked evidence that the model would re-find that bug class
    # if it were reintroduced, and that the invariant is not vacuous for
    # it. The full 45-commit calibration table (and the evidence-only
    # override modules that are not wired here) lives in
    # docs/spec/models/retry-invariant-map.md; these six are the
    # representative per-family regression guards (cheap, deepest
    # consequence). Deliberately no tracey markers (same policy as the
    # other witness checks).

    # G1 (8283d4362): the controller-reported at-cap OOM path loses its
    # cap check -> infra_count exceeds the budget and nothing poisons.
    quint-retry-calib-g1-controller-cap = mkQuintWitnessCheck {
      name = "retry-calib-g1-controller-cap";
      spec = "calibration/retry-g1";
      main = "retryCalibG1ControllerOomUncapped";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "boundsOK";
    };

    # G2 (a4bcb5623): the resubmit reset stops splitting the per-cycle
    # and cross-cycle counters -> the live counters diverge from the fold
    # at the first reset.
    quint-retry-calib-g2-resubmit-split = mkQuintWitnessCheck {
      name = "retry-calib-g2-resubmit-split";
      spec = "calibration/retry-g2";
      main = "retryCalibG2ResubmitSharedCounter";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "countersRefineHistory";
    };

    # G3 (af0eb62c6): poison without the dependent cascade -> a
    # failure-terminal derivation with a still-Ready dependent.
    quint-retry-calib-g3-cascade = mkQuintWitnessCheck {
      name = "retry-calib-g3-cascade";
      spec = "calibration/retry-g3";
      main = "retryCalibG3PoisonWithoutCascade";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "cascadeReachesExactlyTheDependents";
    };

    # G4 (b874e5120): the admin clear runs in-memory first with a
    # best-effort PG clear -> a clear observation behind a still-poisoned
    # durable row (also the non-vacuity guard for the
    # clearedPoisonClearsDurably invariant added with the calibration).
    quint-retry-calib-g4-clear-ordering = mkQuintWitnessCheck {
      name = "retry-calib-g4-clear-ordering";
      spec = "calibration/retry-g4";
      main = "retryCalibG4ClearInMemFirst";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "clearedPoisonClearsDurably";
    };

    # G5 (ee9302b86): the race-ahead termination report stops marking the
    # completion -> the same pod death is counted twice through the
    # disconnect-then-re-report path.
    quint-retry-calib-g5-race-ahead = mkQuintWitnessCheck {
      name = "retry-calib-g5-race-ahead";
      spec = "calibration/retry-g5";
      main = "retryCalibG5RaceAheadKeepsPending";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "noDoubleCount";
    };

    # G8: the poison set fails to survive failover -> a durable,
    # unexpired Poisoned row behind a derivation recovered as anything
    # but Poisoned (the recoveryPreservesPoisonStatus non-vacuity guard;
    # anchored on 891a6520d's poison-set half -- the poisoned_at-IS-NULL
    # load filter and the remove_build reap, abstracted at model
    # resolution as "not reloaded").
    quint-retry-calib-g8-poison-reload = mkQuintWitnessCheck {
      name = "retry-calib-g8-poison-reload";
      spec = "calibration/retry-g8";
      main = "retryCalibG8PoisonedRowNotReloaded";
      extraSpecs = [ "retryPolicyAsBuilt" ];
      step = "calibStep";
      witness = "recoveryPreservesPoisonStatus";
    };

    # Implementation conformance (model-based testing). The regime checks
    # above prove the PROTOCOL; this one proves rio-lease implements
    # that protocol: rio-lease/src/mbt_tests.rs replays traces generated
    # from the leaderElectionBase regime against the real election
    # machinery — the #[quint_run] simulation walks `step` with quint's
    # --mbt action tracking under a seed pinned in the test attribute,
    # the four mbt_run_* tests replay the model's named runs from their
    # ITF traces — and diffs the projected state (lease, leading, gen)
    # after every step. Model↔implementation drift therefore surfaces
    # here as a red check, not as a review-time judgment call.
    #
    # Wiring: the same prebuilt rio-lease test binary nextest-rio-lease
    # runs (a rio-lease source edit rebuilds both), with quint on PATH
    # and the model file staged into the remapped nextest workspace (a
    # model edit re-runs this check — that coupling is the point). The
    # staged copy serves both spec lookups in mbt_tests.rs: the
    # #[quint_run] attribute's workspace-relative path resolves against
    # the test CWD, and the named-run replays read the RIO_MBT_SPEC_PATH
    # override (their compile-time fallback path names the sandbox that
    # compiled the binary, which does not exist here). The mbt_* tests
    # are #[ignore]d, so nextest-rio-lease stays quint-free and this
    # check is the one place they run in CI.
    #
    # The markers below cover what the replayed traces genuinely
    # exercise end-to-end: Lease-object CAS acquisition/renewal/conflict
    # (k8s-lease), the single-writer-per-resourceVersion race and the
    # belief/lease agreement diffed at every step (at-most-one-leader),
    # and the blind-leader self-fence flip (self-fence). The PG-side
    # rules (generation-claim, fetch-max-seed) are NOT marked: the
    # phase-1 projection omits genHW and the mock has no claims ledger.
    # r[verify sched.lease.k8s-lease+2]
    # r[verify sched.lease.at-most-one-leader+3]
    # r[verify sched.lease.self-fence+2]
    mbt-rio-lease = mkNextestRun {
      name = "mbt-rio-lease";
      member = "rio-lease";
      meta = mkNextestMeta { rio-lease = rioLeaseTestBin; };
      extraRuntimeInputs = [ pkgs.quint ];
      extraArgs = [
        "-E"
        "package(rio-lease) and test(/mbt_/)"
        "--run-ignored"
        "all"
      ];
      preRun = ''
        export RIO_MBT_SPEC_PATH=$TMPDIR/ws/docs/spec/models/leaderElection.qnt
      '';
      postWsSetup = ''
        mkdir -p $ws/docs/spec/models
        cp ${leaseModel}/leaderElection.qnt $ws/docs/spec/models/
      '';
      postRun = ''
        # The module-level nextest args pass --no-tests=warn, so a
        # filter that matches nothing would otherwise yield a green
        # check that ran no conformance test at all (e.g. after a test
        # module rename). Assert at least one test actually ran; the
        # exit code already covers failures.
        grep -E ' [1-9][0-9]* tests? run:' $out/log > /dev/null || {
          echo "mbt-rio-lease: the mbt_* filter matched no tests" >&2
          exit 1
        }
      '';
    };

    # Implementation conformance for the LogService model: rio-store/
    # src/logs/mbt_tests.rs replays traces from logService.qnt against
    # the real open-gate / ingest-session / chunk-manifest / read-path /
    # TTL-sweep code, diffing the projected state (the manifest, the
    # session high-water/ceiling/buffer, the lifecycle row's terminal
    # stamp) after every step and re-running the real TailLog dedup walk
    # over the manifest after every step (the servedSpanExact invariant
    # checked against the implementation's fold instead of the model's).
    # Six named runs span all four regime modules (base, redispatch,
    # resend, sweep); the #[quint_run] simulation random-walks the base
    # regime under a pinned seed. Builder-side uploader actions and the
    # in-place buffer drop of the abort path are driver bookkeeping —
    # the scoping rationale lives in the test module's header.
    #
    # Wiring: the same prebuilt rio-store test binary nextest-rio-store
    # runs, with quint on PATH, postgres on PATH (the `member`
    # indirection pulls in rio-store's runtimeTestInputs — the ephemeral
    # PG the projection reads real manifest rows from), and the model
    # staged into the remapped nextest workspace so a model edit re-runs
    # this check.
    #
    # The markers cover what the replayed traces exercise end-to-end:
    # the latest-assignment + already-complete open rejections
    # (append-auth), the open-time seal and the per-append ceiling with
    # its mid-stream refresh (completeness-gate), and the
    # overlapping-session manifest dedup on the read path
    # (session-keyed).
    # r[verify store.log.append-auth]
    # r[verify store.log.completeness-gate]
    # r[verify store.log.session-keyed]
    mbt-rio-logservice = mkNextestRun {
      name = "mbt-rio-logservice";
      member = "rio-store";
      meta = mkNextestMeta { rio-store = rioStoreTestBin; };
      extraRuntimeInputs = [ pkgs.quint ];
      extraArgs = [
        "-E"
        "package(rio-store) and test(/mbt_/)"
        "--run-ignored"
        "all"
      ];
      preRun = ''
        export RIO_MBT_SPEC_PATH=$TMPDIR/ws/docs/spec/models/logService.qnt
      '';
      postWsSetup = ''
        mkdir -p $ws/docs/spec/models
        cp ${logServiceModel}/logService.qnt $ws/docs/spec/models/
      '';
      postRun = ''
        # Same no-tests guard as mbt-rio-lease: --no-tests=warn would
        # otherwise turn a filter that matches nothing into a green
        # check that proved nothing.
        grep -E ' [1-9][0-9]* tests? run:' $out/log > /dev/null || {
          echo "mbt-rio-logservice: the mbt_* filter matched no tests" >&2
          exit 1
        }
      '';
    };
  };
}
