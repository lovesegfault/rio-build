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
  # rio-lease / rio-store test binaries, threaded through flake.nix's
  # quintChecks binding. Only the mbt-rio-lease and mbt-rio-logservice
  # conformance checks below consume them — the model checks need
  # nothing from the Rust build.
  mkNextestRun,
  mkNextestMeta,
  rioLeaseTestBin,
  rioStoreTestBin,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # The §9.1 successor conjunction for materializationJob.qnt (shared
  # by the five per-regime holds checks below; the C-prime stage
  # record's property table is the authoritative gloss).
  matJobInvariants = [
    "boundsOK"
    "noFromSourceWhileJobUnresolved"
    "jobResolutionSound"
    "routingRequiresDurableVouchOrFailFast"
    "unresolvedJobAlwaysArmed"
    "noWrongfulTerminalFailure"
    "noWrongfulFromSourceRouting"
    "successConsumptionCoversLiveWanted"
    "interestUnionLiveOnly"
    "pinCoversIngestUntilAllInterestTerminal"
    "failoverPreservesJobs"
    "fencedJobWritesOnly"
    "kindMatchesWorker"
    "materializationNeverPoisons"
    "materializationInvisibleToBuildBudgets"
    "atMostOneUnresolvedJobPerDrv"
    "atMostOneClaimWinner"
    "wrongfulFailFastBoundedPerJob"
    # A1 fenced-write-discipline (bughunt wave): the view-settlement
    # pair — view mirrors the durable unresolved relation (133) and a
    # cancelled job's history is frozen (276). Single-owner: A1 lands
    # both; A3 extends calibration sets only (§4.F13).
    "viewMatchesDurableUnresolved"
    "chargeFreeCancellation"
    "creationLeavesTenantResolvable"
    "materializationCrashChargedOnce"
    "crossBuildWantedIsolation"
    "materializationPinHasJob"
    # A2 kind-partition (bughunt wave): the Claimed-view/open-attempt
    # lockstep with the kind conjunct (146; single owner A2 — A3
    # preserves through the re-derivation, §4.F13), the kind-scoped
    # recovery holder (266), and the kinded release edge (318).
    # Calibration pairs: quint-materialization-calib-{146,266,318}-*.
    "claimedImpliesOpenAttempt"
    "claimedByOnlyMatHolders"
    "readyImpliesDepsProducedOnRequeue"
    # A4 evidence-classification cells (bughunt wave): the five §2
    # invariants — the 193 reference-cell completion latch, the 194b
    # vacuity latch, the 178/195 charge-free transient latch, the
    # union-upsert widening pin, and the park-viability split (the
    # RBroken→ChildlessLeaf|Holed re-derivation). Calibration pairs:
    # quint-materialization-calib-a4-{refs-folded,vacuous-success,
    # transient-as-infra,union-dropped,leaf-park-forever}; reachability
    # witnesses: noTransientClose, noLeafParkReevalResolve.
    "closureCompleteResolution"
    "noVacuousCoverage"
    "transientOutcomesNeverCharge"
    "durableUnionWidensOrEqualsLive"
    "parkNeverOutlivesFromSourceViability"
    # A3 materialization-lifecycle-kernel (bughunt wave) — the
    # contract re-derivation: budget verdicts are per-job-window sound
    # on BOTH charge channels (067 — the owner-signed Q5 reversal of
    # counter-signed residual (a) — and 020's 085-window), a pending
    # unclaimed job never strands its node Running (015/307 — the
    # atomic release_claim), and the stale-reset carrier survives
    # creation-to-completion (257/055 — #[must_use]
    # RealizedPathCarrier + the completion chokepoint). Calibration
    # pairs: quint-materialization-calib-{establish-never-parks,
    # unscoped-count,split-rearm,chain-reset-carrier,
    # chain-reset-completion}.
    "budgetSoundness"
    "pendingUnclaimedImpliesClaimableNode"
    "carrierConservation"
    "completionRecordsRealizedPaths"
    # B1 bounded-await-transport (bughunt wave, merged_bug_189 / owner
    # Q3): the SIGTERM-aborted walk closes charge-free through
    # release_claim (workerAborted is regime-gated default-false; the
    # holds evidence is quint-materialization-holds-worker-abort).
    # Calibration pair: quint-materialization-calib-189-abort-charges.
    "abortNeverCharges"
  ];

  # Slot-10 walk-fold plane (threaded into materializationJob behind
  # ENABLE_WALK_FOLD; the materializationJobWalkFold regime module
  # arms it — existing regimes bind it false and stay
  # state-space-identical). The four kernel-fold laws; calibration
  # pairs: quint-materialization-calib-{299,295,133,115}-*.
  walkFoldInvariants = [
    "reprobeCongruentWithCompletability"
    "rateLimitedProbeDrawsNoBudget"
    "hitUnderAnyTenantOrderSucceeds"
    "verifiedPathsVisibleToInterest"
    # Bughunt-3 S3 (bug_139, signed Q2): stamped ⊆ verified-per-tenant,
    # derived from the StampProvenance witness. Calibration pair:
    # quint-materialization-calib-139-existsgate-forallstamp.
    "stampedOnlyVerifiedTenants"
  ];

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
  #
  # The server heap is parameterized because the conversion request for
  # the largest modules (the gateway-lifecycle calibration overrides that
  # carry both a rich connection alphabet and the in-session machinery)
  # exceeds the 4 GiB default — the server OOMs parsing the request and
  # every later attempt against the wedged JVM fails too. Only the checks
  # that need it pass a larger value: the default renders byte-identical
  # script text, so existing checks do not rehash.
  apalacheServerPreludeWithHeap = serverHeapMb: ''
    # Start the bundled Apalache server (the same distribution quint
    # would spawn) so it outlives individual quint invocations and stays
    # JIT-warm across retries. Its chatter goes to a side log, keeping
    # the transcript to quint + TLC output. Port 8822 is quint's default;
    # the sandbox's private network namespace keeps parallel checks from
    # colliding on it.
    apalache_jar=$(ls ${pkgs.quint}/share/quint/apalache-dist-*/apalache/lib/apalache.jar)
    ${pkgs.jdk21_headless}/bin/java -Xmx${toString serverHeapMb}m -XX:+UseG1GC -jar "$apalache_jar" server --port=8822 \
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
        # Hard wall-clock bound (MODEL_TIMEOUT_SEC, constructor
        # parameter): a non-terminating model MUST be a RED check with
        # a log, never an eternal gate. A state-space blowup once ran
        # 14+ hours verdict-less inside a wave gate (gcCollectState's
        # first wiring squared its map dimension); the bound turns
        # that class into a loud failure naming the budget.
        if timeout "$MODEL_TIMEOUT_SEC" \
          quint verify --server-endpoint=localhost:8822 "$@" 2>&1 | tee $out
        then
          quint_status=0
        else
          quint_status=$?
        fi
        if [ "$quint_status" -eq 124 ]; then
          echo "" >&2
          echo "model exceeded the $MODEL_TIMEOUT_SEC-second budget - non-termination is a failure (state-space blowup? see the model's parameter-ladder doc). Raise modelTimeoutSec ONLY with a measured runtime justifying it." >&2
          return 0
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

  # The default-heap prelude every existing check interpolates; renders
  # the exact text the constructors carried before the heap was
  # parameterized, so no existing derivation rehashes.

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
      # Non-default step action (an override module can expose an
      # alternative transition relation, the way the retired Stage-C
      # calibration corpus exposed its pre-fix `calibStep`). null means
      # quint's default (`step`).
      step ? null,
      # Non-default init action (a calibration module can start from a
      # MID-TRACE state when the property's prefix is too deep for the
      # checker's budget from the cold init — the controller-113
      # respawn-cancel pair). null means quint's default (`init`).
      init ? null,
      # Pin the TLC worker count to the value the wiring measurement used
      # (closure-evidence Phase-1 review finding MCI-6): an exhaustive
      # check wired from a measured budget must run at the measurement's
      # worker count or the budget is meaningless — TLC wall-clock does
      # not scale linearly in workers, so "converges in N min at 60
      # workers" says nothing about other widths. null (the default)
      # keeps the pre-Phase-1 behavior: derive from NIX_BUILD_CORES.
      workers ? null,
      # Apalache server heap (MiB) — same semantics as
      # mkQuintWitnessCheck's parameter. Exported as meta.serverHeapMb
      # (bug_383) so gen_matrix.py can isolate heavy checks into
      # singleton formal shards: ONE binding sizes the JVM and the CI
      # budget; a check whose heap is raised here moves itself out of
      # the round-robin shards automatically.
      serverHeapMb ? 4096,
      # Wall-clock budget (seconds) for ONE quint-verify attempt. A
      # model that exceeds it is a RED check naming the budget — never
      # an eternal gate: gcCollectState's first wiring squared its map
      # dimension into the state space and four wedged clients sat 14+
      # hours verdict-less, starving every gate on the host. Raise only
      # with a measured runtime documented at the check.
      modelTimeoutSec ? 1800,
      # bughunt-2 (slot 11): per-invariant-leaf vacuity exemptions for the
      # quint-policy lint — { <leaf> = { class = "boundsOK"|"scope-bound"|
      # "pre-r2-untwinned"; reason = "..."; }; }. P1 demands a live-import
      # falsify twin for every holds-invariant leaf unless exempted here.
      vacuityExempt ? { },
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # bug_383: the SAME binding that sizes the JVM below — exported
        # for gen_matrix.py's heavy-shard isolation.
        meta.serverHeapMb = serverHeapMb;
        # bughunt-2 (slot 11): wiring facts for the quint-policy lint — the
        # only channel besides the parse IR that policy enforcement reads.
        meta.quintPolicy = {
          kind = "holds";
          inherit spec main extraSpecs;
          inherit invariants;
          step = if step == null then "step" else step;
          inherit vacuityExempt;
        };
        # Only the named .qnt files. A model that imports another file
        # (a shared harness, an override module's parent model) extends
        # the fileset via extraSpecs — keeping it narrow means an
        # unrelated docs/ edit doesn't re-run every quint check.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        # Surfaced in `nix log` and error messages.
        env = {
          MODEL = spec;
          MAIN = main;
          MODEL_TIMEOUT_SEC = toString modelTimeoutSec;
        };
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
        ${
          lib.optionalString (workers != null) ''
            workers=${toString workers}
          ''
        }printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPreludeWithHeap serverHeapMb}

        # The transcript is the proof artifact: it records the
        # invariants checked, the state count, and the depth — keep it.
        # (`| tee` is pipefail-safe — tee consumes everything; never
        # replace it with `| head`, see
        # .claude/rules/ci-failure-patterns.md.)
        run_quint_verify \
          --backend=${backend} \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          ${lib.optionalString (init != null) "--init=${init}"} \
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
      # Same semantics as mkQuintCheck's step. Used by override modules
      # that select a non-default transition relation (calibStep and
      # pullStep callers).
      step ? null,
      # Non-default init action (a calibration module can start from a
      # MID-TRACE state when the property's prefix is too deep for the
      # checker's budget from the cold init — the controller-113
      # respawn-cancel pair). null means quint's default (`init`).
      init ? null,
      # Apalache server heap (MiB) for the quint->TLA+ conversion. The
      # default matches the historical hardcoded value; only the largest
      # override modules (whose conversion request OOMs a 4 GiB server)
      # need more — see apalacheServerPreludeWithHeap.
      serverHeapMb ? 4096,
      # Same semantics as mkQuintCheck's workers (MCI-6 pinning).
      workers ? null,
      # Same semantics as mkQuintCheck's modelTimeoutSec.
      modelTimeoutSec ? 1800,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # bug_383: the SAME binding that sizes the JVM below — exported
        # for gen_matrix.py's heavy-shard isolation (a check raised
        # past the 4096 default moves itself into a singleton shard).
        meta.serverHeapMb = serverHeapMb;
        # bughunt-2 (slot 11): wiring facts for the quint-policy lint — the
        # only channel besides the parse IR that policy enforcement reads.
        meta.quintPolicy = {
          kind = "witness";
          inherit spec main extraSpecs;
          inherit witness;
          step = if step == null then "step" else step;
          vacuityExempt = { };
        };
        # Same fileset narrowing as mkQuintCheck.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          WITNESS = witness;
          MODEL_TIMEOUT_SEC = toString modelTimeoutSec;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # Same worker bound as mkQuintCheck (see its comment).
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers='"auto"'
        ${
          lib.optionalString (workers != null) ''
            workers=${toString workers}
          ''
        }printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPreludeWithHeap serverHeapMb}

        # The violation is the EXPECTED outcome, so the verify call's
        # nonzero exit must not abort the script -- the retry helper
        # captures the status instead. The transcript (including the
        # counterexample trace, which is the reachability evidence) is
        # the check's output.
        run_quint_verify \
          --backend=tlc \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          ${lib.optionalString (init != null) "--init=${init}"} \
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

  # Expect-violation check via the RUST SIMULATOR (`quint run`) instead
  # of TLC. For violations that are real, confirmed, and load-bearing for
  # the campaign record, but whose BFS level sits past a gate-compatible
  # TLC budget (the closure-evidence post-0d triage record: TLC's BFS is
  # correct and finds these given enough wall-clock — the per-level state
  # growth at the affected scopes simply puts them in the
  # tens-of-minutes-to-hours class). The simulator finds the same
  # violations in seconds because a random walk pays no exhaustive
  # frontier cost.
  #
  # Semantics: bounded random search, NOT a proof — exactly right for an
  # expect-violation check, whose claim is existential ("this scenario is
  # reachable"), not universal. A found violation is a found violation
  # regardless of backend; only "no violation found" differs in strength,
  # and for these checks that outcome is a FAILURE either way.
  #
  # Flake discipline: maxSamples must be sized so the per-run miss
  # probability is negligible. With a per-trace hit rate p (measure it:
  # the introducing commit records traces-to-first-hit), P(miss) =
  # (1-p)^maxSamples ~= exp(-p*maxSamples); size maxSamples >= 25/p so
  # P(miss) <= ~1e-11. Cost note: the simulator's early exit is
  # per-worker-batch, not per-trace — high-hit-rate violations return in
  # under a second, but a low-rate violation's run costs roughly the
  # FULL sample budget's wall clock (measured: ~10s per million samples
  # per step-of-14 at the closure-evidence duo scope), so keep
  # maxSamples at the flake-math minimum rather than padding it. If a
  # previously-green sim check starts failing, the model has drifted
  # under the scenario (the same signal a TLC witness check gives) —
  # investigate, never just bump samples.
  mkQuintSimWitnessCheck =
    {
      name,
      spec,
      main ? spec,
      # The witness `val` expected to be violated.
      witness,
      # Per-run sample / step bounds (see the flake-discipline note).
      maxSamples ? 2000000,
      maxSteps ? 15,
      # Same semantics as mkQuintCheck's extraSpecs / step.
      extraSpecs ? [ ],
      step ? null,
      # Non-default init action (a calibration module can start from a
      # MID-TRACE state when the property's prefix is too deep for the
      # checker's budget from the cold init — the controller-113
      # respawn-cancel pair). null means quint's default (`init`).
      init ? null,
      # Optional fixed simulator seed (hex string): replays a recorded
      # discovery deterministically; the unseeded sample budget remains
      # the re-find backstop. null leaves the simulator's own seeding —
      # and every existing call site's derivation — bit-identical (the
      # splice below is zero-width and the env entry is absent).
      seed ? null,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # bughunt-2 (slot 11): wiring facts for the quint-policy lint — the
        # only channel besides the parse IR that policy enforcement reads.
        meta.quintPolicy = {
          kind = "witness-sim";
          inherit spec main extraSpecs;
          inherit witness;
          step = if step == null then "step" else step;
          vacuityExempt = { };
        };
        # Same fileset narrowing as mkQuintCheck.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          WITNESS = witness;
        }
        // lib.optionalAttrs (seed != null) { SEED = seed; };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # The violation is the EXPECTED outcome: `quint run` exits
        # nonzero when it finds one, so don't let that abort the script.
        status=0
        timeout 1800 quint run \
          --backend=rust \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"}${
            lib.optionalString (init != null) " --init=${init}"
          }${lib.optionalString (seed != null) " --seed=${seed}"} \
          --invariant=${witness} \
          --max-samples=${toString maxSamples} \
          --max-steps=${toString maxSteps} \
          "$src/${spec}.qnt" 2>&1 | tee $out || status=$?
        if [ "$status" -eq 124 ]; then
          echo "" >&2
          echo "${name}: simulation exceeded the 1800-second wall clock — regime-size regression (see the model's parameter-ladder doc)." >&2
          exit 1
        fi

        if grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: witness ${witness} was NOT violated in ${main} within ${toString maxSamples} samples x ${toString maxSteps} steps." >&2
          echo "The scenario it probes is no longer reachable (or its hit rate collapsed); the regime's invariants may now hold vacuously." >&2
          exit 1
        fi

        # Require the simulator's own violation report -- a crash or a
        # typecheck error must not read as a successful expect-violation.
        if ! grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: quint run failed without reporting a violation of ${witness} (tool error?)." >&2
          exit 1
        fi
      '';

  # Bounded-simulation HOLDS check: the dual of mkQuintSimWitnessCheck.
  # `quint run` over maxSamples random traces must find NO violation of
  # the named invariants. This is bounded evidence, not proof — where an
  # exhaustive TLC conjunction exists (wired or manual target) it remains
  # the proof obligation; this constructor's role is the GHA-wired
  # deliverable for FIXED properties whose exhaustive conjunctions exceed
  # every gate-compatible TLC budget (the closure-evidence Phase-1 flips:
  # owner decision 4's A17/L2 wiring).
  #
  # Vacuity discipline: a holds check proves nothing if the model can no
  # longer reach the states the property constrains. Every check built
  # from this constructor MUST name its paired expect-violation
  # calibration pin in its comment — the pin re-introduces the pre-fix
  # behavior and must keep falsifying the same property, which is the
  # machine-checked evidence that the property is still about something
  # reachable. A holds check without a falsifying pair is not wired here.
  #
  # Multiple invariants are conjoined with "and" into one expression
  # (`quint run --invariant` accepts an expression; the comma form is
  # verify-only). A violation therefore does not name the failing
  # conjunct — re-run per-invariant to identify it, same as TLC's q_inv.
  mkQuintSimHoldsCheck =
    {
      name,
      spec,
      main ? spec,
      # The invariant `val` names expected to HOLD.
      invariants,
      # Per-run sample / step bounds. Sizing: large enough that the paired
      # pin's violation class would be re-found if it re-opened (use the
      # pin's own measured budget as the floor); the introducing commit
      # records the measurement.
      maxSamples ? 2000000,
      maxSteps ? 15,
      # Same semantics as mkQuintCheck's extraSpecs / step.
      extraSpecs ? [ ],
      step ? null,
      # Non-default init action (a calibration module can start from a
      # MID-TRACE state when the property's prefix is too deep for the
      # checker's budget from the cold init — the controller-113
      # respawn-cancel pair). null means quint's default (`init`).
      init ? null,
      # Same semantics as mkQuintCheck's vacuityExempt (quint-policy P1).
      vacuityExempt ? { },
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # bughunt-2 (slot 11): wiring facts for the quint-policy lint — the
        # only channel besides the parse IR that policy enforcement reads.
        meta.quintPolicy = {
          kind = "holds-sim";
          inherit spec main extraSpecs;
          inherit invariants;
          step = if step == null then "step" else step;
          inherit vacuityExempt;
        };
        # Same fileset narrowing as mkQuintCheck.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          INVARIANTS = lib.concatStringsSep " and " invariants;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # A violation exits nonzero; capture the status so the verdict
        # grep below (not the exit code) decides the outcome.
        status=0
        timeout 1800 quint run \
          --backend=rust \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          ${lib.optionalString (init != null) "--init=${init}"} \
          --invariant='${lib.concatStringsSep " and " invariants}' \
          --max-samples=${toString maxSamples} \
          --max-steps=${toString maxSteps} \
          "$src/${spec}.qnt" 2>&1 | tee $out || status=$?
        if [ "$status" -eq 124 ]; then
          echo "" >&2
          echo "${name}: simulation exceeded the 1800-second wall clock — regime-size regression (see the model's parameter-ladder doc)." >&2
          exit 1
        fi

        if grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: a violation of (${lib.concatStringsSep " and " invariants}) was found in ${main}." >&2
          echo "A Phase-1 fixed property regressed (or its model encoding drifted): capture the trace above" >&2
          echo "and triage against the paired calibration pin before touching this check." >&2
          exit 1
        fi

        # "Holds" requires the simulator's own verdict, not just exit 0:
        # a crash that never ran any trace must not read as bounded
        # evidence.
        if [ "$status" -ne 0 ]; then
          echo "" >&2
          echo "${name}: quint run failed (status $status) without reporting a violation — tool error." >&2
          exit 1
        fi
        if ! grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: quint run exited 0 but reported no verdict (tool error?)." >&2
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
        # bughunt-2 (slot 11): wiring facts for the quint-policy lint.
        meta.quintPolicy = {
          kind = "run";
          inherit spec main extraSpecs;
          step = "step";
          vacuityExempt = { };
        };
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

        # Bounded: a named-run replay is deterministic and fast; a
        # wall-clock blowout means the regime regressed.
        timeout 1800 quint test \
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

  # bughunt-2 (slot 11): the falsify-twin-required corpus lint (P1-P6).
  # Zero quint-semantic work: wiring facts from the constructor meta
  # manifest (nix eval), structural facts from `quint parse --out` IR
  # only; an in-derivation canary pins the IR shape. See
  # nix/quint_policy.py for the rule statements and the vehicle ruling.
  mkPolicyCheck =
    corpus:
    let
      manifest = pkgs.writeText "quint-policy-manifest.json" (
        builtins.toJSON (lib.mapAttrs (_: c: c.meta.quintPolicy or null) corpus)
      );
    in
    pkgs.runCommand "quint-policy"
      {
        nativeBuildInputs = [
          pkgs.quint
          pkgs.python3
        ];
        # The FULL corpus: every live model and every calibration file —
        # the whole point is whole-corpus structural coverage.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = modelsDir;
        };
        inherit manifest;
        policyScript = ./quint_policy.py;
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"
        mkdir ir
        # IR-shape canary: parsed in-derivation; quint_policy.py asserts
        # the expected shape before trusting any verdict.
        {
          echo 'module policyCanary {'
          echo '  var x: int'
          echo "  action init = x' = 0"
          echo "  action step = x' = x + 1"
          echo '  val invX = x >= 0'
          echo '}'
        } > policyCanary.qnt
        quint parse --out ir/__canary__.json policyCanary.qnt
        i=0
        while IFS= read -r -d "" f; do
          rel="''${f#./}"
          i=$((i + 1))
          quint parse --out "ir/$i.json" "$src/$rel"
        done < <(cd "$src" && find . -name '*.qnt' -print0 | sort -z)
        # Banner (b), bughunt-3 S1: the lint proves every rule arm RED
        # on planted corpora before it may gate (bug_094's seed-flip
        # resolver and merged_bug_090's dead P6 arm both lived in this
        # lint while it certified the formal corpus).
        python3 "$policyScript" --self-test
        python3 "$policyScript" \
          --manifest "$manifest" \
          --ir-dir ir \
          --models-dir "$src" | tee "$out"
      '';

in
rec {
  # Expose the constructors so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # attrs below.
  inherit
    mkQuintCheck
    mkQuintWitnessCheck
    mkQuintSimWitnessCheck
    mkQuintSimHoldsCheck
    mkQuintRunCheck
    ;

  # [LONG] checks (Tier 2 of the closure-evidence Phase-1 three-tier
  # wiring; owner decision 3 / adjudication OQ6): check names that are
  # wired into checks.* — the LOCAL merge gate (`/nixbuild --checks`)
  # carries them under the raised 15–30-minute budget — but are EXCLUDED
  # from the GHA formal matrix (flake.nix removes them from
  # ciMatrix.formal): a 15–30-min-at-60-workers TLC check needs hours of
  # pod wall-clock on the 4–16 vCPU rio-ci spot runners, over the 45-min
  # job timeout even as a singleton shard — the same runner-class
  # rationale that keeps cov-smoke out of the GHA matrices. Every entry
  # here MUST (a) exist in `checks` below, (b) pin `workers` to its
  # qualifying measurement's count, and (c) name its Tier-1 GHA companion
  # (the holdsInSim+pin pair covering the same property) in its comment.
  #
  # Track E knobs (FORMAL_SHARD_SIZE, --max-jobs, timeout-minutes, runner
  # labels) are FROZEN for this campaign: per-check pod wall-clock is a
  # constraint no shard knob fixes (sharding divides checks among jobs,
  # it never shortens one check). Pressure to change them is
  # stop-and-report, never a unilateral CI edit.
  #
  # Currently EMPTY: the Phase-1 Wave-4 measurement campaign (invariant
  # map, Wave-4 stage record) found no exhaustive conjunction that
  # converges within the 5–30-minute Tier-2 window at 60 workers — every
  # candidate is either Tier 1 (none qualified) or a Tier-3 documented
  # manual target. The mechanism is wired so a Phase-2 conjunction that
  # does converge can be added here without touching flake.nix again.
  longChecks = [ ];

  # Per-model checks. Imported by flake.nix as the quintChecks binding,
  # which merges them into checks.* and hands the same attrset to the
  # CI matrix's `formal` kind.
  quintCorpus = {
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
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
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
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
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
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
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
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
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

    # ---- bughunt-wave F1: suspend + shutdown lease regimes -----------
    # (delimited section: leaderElection.qnt fault-gated extension.
    # All four pre-existing regimes above instantiate the new fault
    # budgets at zero and were re-measured state-count-identical at the
    # introducing commit — the new vars are constant there and multiply
    # no states.)

    # The split-renew (suspend) fault regime: a steady renew's PUT
    # commits at send while the response read parks — a host suspend
    # straddling an in-flight round-trip (bug_096). The parked node is
    # exempt from the loop-discipline tick cap (the honest statement of
    # the bug: the loop-interval assumption never held across a parked
    # await), so dual belief IS reachable here and the operative
    # property is boundedDualLeadership, not neverDual — see the module
    # comment and lease-invariant-map.md's documented deviation (a).
    # The production fix encoded: renewReadOk stamps the fence from the
    # attempt-start ANCHOR (RenewAnchor/BlindClock); the paired
    # expect-violation pin is quint-lease-calib-096-response-anchor,
    # which re-introduces response anchoring and must keep violating
    # boundedDualLeadership.
    # r[verify sched.lease.self-fence+2]
    quint-leader-election-suspend = mkQuintCheck {
      name = "leader-election-suspend";
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionSuspend";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        # The resume-bounded form: the post-read believer's
        # resume-to-first-tick gap legitimately exceeds the exact tick
        # cap (see the val's doc); the legacy regimes keep the exact
        # loopInterval.
        "loopIntervalResumeBounded"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
      ];
    };

    # The graceful-exit (shutdown) fault regime: SIGTERM breaks the
    # loop and runs the release gate, which is HOLD-gated — the
    # belief/hold split of bug_387 (LeaseStanding). gracefulHandover is
    # the headline: no node ever exits while the apiserver still names
    # it holder without releasing; neverDual carries over from the
    # asymmetric constants (an exited node is not a believer). The
    # paired expect-violation pin is quint-lease-calib-387-belief-gate
    # (the belief-gated release must keep violating gracefulHandover).
    # r[verify sched.lease.graceful-release+2]
    quint-leader-election-shutdown = mkQuintCheck {
      name = "leader-election-shutdown";
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13):
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionShutdown";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "neverDual"
        "gracefulHandover"
      ];
    };

    # Non-vacuity witnesses for the two new regimes (same discipline as
    # the witness block above: each check passes only when the checker
    # VIOLATES the witness, proving the guarded scenario reachable).
    quint-leader-election-witness-suspend-straddle = mkQuintWitnessCheck {
      name = "leader-election-witness-suspend-straddle";
      spec = "leaderElection";
      main = "leaderElectionSuspend";
      witness = "noSuspendStraddle";
    };
    quint-leader-election-witness-fence-then-exit = mkQuintWitnessCheck {
      name = "leader-election-witness-fence-then-exit";
      spec = "leaderElection";
      main = "leaderElectionShutdown";
      witness = "noFenceThenExit";
    };

    # Deterministic named-run replays for the two new regimes (the
    # executable scenario pins: the straddle-fences narrative and the
    # fence-then-SIGTERM release/skip pair).
    quint-leader-election-runs-suspend = mkQuintRunCheck {
      name = "leader-election-runs-suspend";
      spec = "leaderElection";
      main = "leaderElectionSuspend";
    };
    quint-leader-election-runs-shutdown = mkQuintRunCheck {
      name = "leader-election-runs-shutdown";
      spec = "leaderElection";
      main = "leaderElectionShutdown";
    };

    # The dropped-write (mid-band) fault regime (NEW, bughunt2-wave
    # slot 8 merged_bug_303): every renew round's fetch completes, no
    # act's response ever arrives, transmitted PUTs still COMMIT
    # (renewSendDropped), and the own-commit evidence rule
    # (fetchObservesOwnCommit) is the only thing that can re-anchor
    # the holder's belief -- consuming the UnconfirmedPut ledger and
    # stamping the blind clock at the LEDGER's anchor, never the
    # read's time. Asymmetric-TTL constants, so neverDual is checked
    # here as the SOUNDNESS ARBITER of the evidence design: stamping
    # at the anchor must never let a blind victim's belief outlive a
    # healthy thief's steal deadline. blindHolderBounded is the
    # headline liveness-as-safety claim: a fenced holder with
    # observable own-commit evidence re-believes within the forcing
    # cap's window (BLIND_HOLDER_BOUND = 8 global ticks,
    # boundary-measured: 7 violates, 8 holds -- see the module
    # comment). The paired pin is quint-lease-calib-303-blind-timeout
    # (the evidence rule removed in its three pieces must falsify).
    # Measured: exhaustive TLC ~6s / ~619k distinct states at the
    # wired constants -- the default 1800s budget is ~300x headroom.
    # r[verify sched.lease.cancelled-write+2]
    quint-leader-election-dropped-write = mkQuintCheck {
      name = "leader-election-dropped-write";
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13): the
      # dropped-write regime conjoins the base invariant set, so the
      # same untwinned leaf carries the same exemption as its six
      # sibling regimes.
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionDroppedWrite";
      step = "midBandStep";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "neverDual"
        "blindHolderBounded"
      ];
    };

    # Non-vacuity witness: a transmitted write actually drops in the
    # explored space -- the neverDual and blindHolderBounded verdicts
    # above are about a space where the fault bites.
    quint-leader-election-witness-dropped-commit = mkQuintWitnessCheck {
      name = "leader-election-witness-dropped-commit";
      spec = "leaderElection";
      main = "leaderElectionDroppedWrite";
      witness = "noDroppedCommit";
      step = "midBandStep";
    };

    # The foreign-rv-writer regime (bughunt3-wave S6b merged_bug_180):
    # the mid-band world PLUS a non-protocol Lease mutator (bumpRv —
    # annotation patches that move metadata.resourceVersion without
    # touching the holder-authored spec content) and the discarded
    # flavor of the transmitted-abandoned write (renewSendLost — the
    # client cannot distinguish committed from discarded, so the model
    # explores both). The observed record and the evidence cursor key
    # on CONTENT (production decide()/last_fetched_renew_time):
    # noFabricatedEvidence is the headline — own-commit evidence is
    # never consumed from churn the protocol did not author — and the
    # full base invariant set re-verifies with the new fault biting
    # the CAS guard (foreign writes 409 in-flight protocol PUTs).
    # The paired pin is quint-lease-calib-180-rv-keyed.
    # r[verify sched.lease.cancelled-write+2]
    # r[verify sched.lease.k8s-lease+2]
    quint-leader-election-foreign-rv = mkQuintCheck {
      name = "leader-election-foreign-rv";
      # Measured (tttt duty, gating backend): TLC-exhaustive
      # 151,134,113 generated / 35,746,806 distinct / depth 41 in
      # 2m29s at 192 workers — the 1800s default is ~12x headroom.
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13): same
      # untwinned base leaf as the six sibling regimes.
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionForeignRv";
      step = "foreignRvStep";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "neverDual"
        "noFabricatedEvidence"
      ];
    };

    # Non-vacuity witness: own-commit evidence is actually consumed in
    # a trace of the foreign-rv space — the noFabricatedEvidence
    # verdict above is about a space where the evidence machinery and
    # the fault both fire.
    quint-leader-election-witness-foreign-evidence = mkQuintWitnessCheck {
      name = "leader-election-witness-foreign-evidence";
      spec = "leaderElection";
      main = "leaderElectionForeignRv";
      witness = "noEvidenceUnderForeignBumps";
      step = "foreignRvStep";
    };

    # Deterministic named-run replay (livelockBrokenRun: two dropped
    # writes, belief sustained on own-commit evidence alone, each
    # stamp at the ledger anchor, no leaderless window, no dual).
    quint-leader-election-runs-dropped-write = mkQuintRunCheck {
      name = "leader-election-runs-dropped-write";
      spec = "leaderElection";
      main = "leaderElectionDroppedWrite";
    };

    # The holder-evidence regime (bughunt-4 S2 merged_bug_085, signed
    # Q3): the foreign-rv world PLUS the deferral law for the
    # believing renew 409 — the ambiguous CAS bounce defers ONE round
    # (keeping belief AND the unconfirmed-write ledger) instead of
    # running the lose edge blind, and the lose fires only on typed
    # holder evidence (a completed read naming another holder, or the
    # deferral exhausted by a second consecutive bounce). The deferred
    # bounce does NOT re-stamp the fence: a stamped deferral would
    # extend belief with the lease content frozen — decoupled from the
    # content writes that reset a standby's steal clock — and THIS
    # regime's checker found exactly that dual (victim defers near its
    # fence deadline, believes past the standby's steal) when the
    # first cut stamped. Production computes will_defer before the
    # blind stamp for the same reason. Headline:
    # loseRequiresHolderEvidence — no believing first 409 ever runs
    # the lose edge. neverDual re-verifies under the asymmetric
    # margin; boundedDualLeadership prices the new mid-deferral dual
    # shape (the deferral resolves or fences on the PRE-409 budget).
    # The paired pin is quint-lease-calib-085-blind-conflict-lose
    # (the immediate-lose world falsifies the headline).
    # Measured (tttt duty, gating backend): TLC-exhaustive
    # 968,119,453 generated / 220,855,702 distinct / depth 47 in
    # 15m17s at 24 workers — the 3600s budget is ~3.9x headroom at
    # that worker count; raise only with a new measured run. (At the
    # foreign-rv sibling's MAX_DROPS = 2 the space exceeded 720M
    # generated WITHOUT exhausting — the regime's module comment
    # records the scope trim and what the droppedWrite regime keeps.)
    # r[verify sched.lease.holder-evidenced-lose]
    quint-leader-election-holder-evidence = mkQuintCheck {
      name = "leader-election-holder-evidence";
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13): same
      # untwinned base leaf as the sibling regimes.
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionHolderEvidence";
      step = "holderEvidenceStep";
      modelTimeoutSec = 3600;
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "neverDual"
        "noFabricatedEvidence"
        "blindHolderBounded"
        "loseRequiresHolderEvidence"
      ];
    };

    # Non-vacuity witness: a believing 409 actually DEFERS in the
    # explored space — the loseRequiresHolderEvidence verdict above is
    # about a space where the deferral law fires, not one where no
    # conflict ever lands on a believer.
    quint-leader-election-witness-deferred-conflict = mkQuintWitnessCheck {
      name = "leader-election-witness-deferred-conflict";
      spec = "leaderElection";
      main = "leaderElectionHolderEvidence";
      witness = "noDeferredConflict";
      step = "holderEvidenceStep";
    };

    # The cooperative step-down regime (bughunt-4 S2 merged_bug_128):
    # the pg-faults fault economy (crash/recover) plus the
    # request/serve pair, instance-keyed per the production law — a
    # step-down request is stamped with the per-acquire INSTANCE that
    # filed it, the acquire/rebound edges clear any pending request,
    # and a serve demands the stamp match the CURRENT instance.
    # Headline: noStaleStepDownServed — recovery #1's demotion never
    # fires against the re-acquired tenure that superseded it (the
    # same-count ABA: a false-alarm fence + same-epoch re-acquire
    # REPEATS the transition count, which is why the count was the
    # wrong key). The paired pin is
    # quint-lease-calib-128-count-keyed-stepdown (the count-keyed +
    # no-clear world falsifies it).
    # Measured (tttt duty, gating backend): TLC-exhaustive
    # 582,741,343 generated / 94,208,853 distinct / depth 45 in 7m22s
    # at 24 workers — the 3600s budget is ~8x headroom at that worker
    # count; raise only with a new measured run.
    # r[verify sched.recovery.step-down+3]
    quint-leader-election-step-down = mkQuintCheck {
      name = "leader-election-step-down";
      modelTimeoutSec = 3600;
      # quint-policy P1 exemption (bughunt-2 slot 11; §5-Q13): same
      # untwinned base leaf as the sibling regimes.
      vacuityExempt = {
        atMostOneCASWinner = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs an apiserver-fault twin (duplicate resourceVersion admission) — a new fault axis priced in the Q13 burn-down headline list";
        };
      };
      spec = "leaderElection";
      main = "leaderElectionStepDown";
      step = "stepDownStep";
      invariants = [
        "boundsOK"
        "clockSkewBound"
        "atMostOneCASWinner"
        "loopInterval"
        "boundedDualLeadership"
        "staleLeaderHasStaleGeneration"
        "noStaleStepDownServed"
      ];
    };

    # Non-vacuity witness: step-down requests are actually FILED (and
    # therefore serveable) in the explored space — the
    # noStaleStepDownServed verdict above is about a space where the
    # request/serve machinery runs.
    quint-leader-election-witness-step-down-requested = mkQuintWitnessCheck {
      name = "leader-election-witness-step-down-requested";
      spec = "leaderElection";
      main = "leaderElectionStepDown";
      witness = "noStepDownRequested";
      step = "stepDownStep";
    };

    # The leader-marks reconciliation model (bughunt-wave F1
    # merged_bug_138; DirtyGen rework bughunt2-wave slot 8 bug_181):
    # the deletion-cost/label machinery as a level-triggered protocol —
    # edge writers that MARK a generation counter, the single-flight
    # reconcile split into its API-write and flag-clear phases, the
    # holder-aware sweep (captured-holder skip), the external falsifier
    # (a strip no edge writer sees), the rebound dirtying site
    # (merged_bug_212's hook — the "future writer" class the
    # generation arithmetic absorbs), and the bounded-cadence verify
    # pass. Headlines: marksDivergenceBounded (cause-tagged — OUR
    # pod's divergence is discovered, or strip-caused and younger than
    # the verify cadence plus the task-parking deadline; edge- and
    # stale-write-caused divergence carries NO age window because the
    # edge marked in the same transition) and notClobbered (no clear
    # erases a post-snapshot mark — the DirtyGen clear-through
    # arithmetic itself). The wrongSince/wrongCause stamps are single
    # derived helpers applied in EVERY action (no enumerated writer
    # list), which is what keeps the green verdict non-vacuous; the
    # paired pins are quint-lease-calib-138-edge-only (verify pass
    # removed must falsify the bound) and
    # quint-lease-calib-181-bool-clear (bool clear-all restored must
    # falsify notClobbered). The holder-aware sweep half is encoded
    # structurally (reconcilePatched) and pinned at the production
    # level by peer_sweep_spares_current_lease_holder — see the model
    # comment for why a model invariant there would be decorative
    # (the spawn-to-complete TOCTOU is a real, verify-bounded
    # residual). Measured: exhaustive TLC ~4s at the wired constants
    # (transcript is authoritative) — the default 1800s budget is
    # ~450x headroom.
    # r[verify sched.lease.marks-verify]
    # r[verify sched.lease.deletion-cost+3]
    quint-leader-marks = mkQuintCheck {
      name = "leader-marks";
      spec = "leaderMarks";
      main = "leaderMarksBase";
      invariants = [
        "boundsOK"
        "marksDivergenceBounded"
        "notClobbered"
      ];
    };

    # Non-vacuity witness: the external strip actually fires in the
    # explored space.
    quint-leader-marks-witness-strip = mkQuintWitnessCheck {
      name = "leader-marks-witness-strip";
      spec = "leaderMarks";
      main = "leaderMarksBase";
      witness = "noStrip";
    };

    # Non-vacuity witness: the rebound dirtying site actually fires in
    # the explored space — the notClobbered verdict covers the
    # post-212 writer set, not just the original edges.
    quint-leader-marks-witness-rebound = mkQuintWitnessCheck {
      name = "leader-marks-witness-rebound";
      spec = "leaderMarks";
      main = "leaderMarksBase";
      witness = "noRebound";
    };

    # Deterministic named-run replays (verifyConvergesRun: strip →
    # cadence-forced verify → re-discovery → re-assert;
    # clearKeepsPostSnapMarkRun: a rebound marks between the API write
    # and the clear, and the clear-through arithmetic keeps the loop
    # dirty — the DirtyGen save, end to end).
    quint-leader-marks-runs = mkQuintRunCheck {
      name = "leader-marks-runs";
      spec = "leaderMarks";
      main = "leaderMarksBase";
    };

    # ---- F1 calibration pins (expect-violation; the pre-fix
    # behaviors frozen as permanent regression evidence — the vacuity
    # guards for the three green lease checks above) ------------------

    # bug_096 pre-fix: RESPONSE-anchored blind window. The straddled
    # believer resumes with a fresh fence and a spent snapshot — no
    # discovery path armed — and boundedDualLeadership falls. The
    # violation needs the full straddle interleaving (deeper than a
    # gate-budget sim hunt), so this pin runs the exhaustive backend
    # like the regime check it guards.
    quint-lease-calib-096-response-anchor = mkQuintWitnessCheck {
      name = "lease-calib-096-response-anchor";
      spec = "calibration/lease-096-response-anchor";
      main = "leaseCalib096ResponseAnchor";
      witness = "boundedDualLeadership";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # bug_387 pre-fix: BELIEF-gated release. Fence-then-SIGTERM skips
    # the release the hold gate owes; gracefulHandover falls.
    quint-lease-calib-387-belief-gate = mkQuintWitnessCheck {
      name = "lease-calib-387-belief-gate";
      spec = "calibration/lease-387-belief-gate";
      main = "leaseCalib387BeliefGate";
      witness = "gracefulHandover";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # merged_bug_303 pre-fix (bughunt2-wave slot 8): BLIND TIMEOUT --
    # no evidence rule (no consumption, no anchor stamp, no forcing
    # cap, the pre-fix GET): a fenced holder whose committed writes
    # keep bumping the rv can never re-anchor its belief and the
    # blind-holder window stretches to the trace horizon;
    # blindHolderBounded falls. One cycle of the unbounded leaderless
    # livelock the phased renew + UnconfirmedPut ledger exist to
    # break.
    quint-lease-calib-303-blind-timeout = mkQuintWitnessCheck {
      name = "lease-calib-303-blind-timeout";
      spec = "calibration/lease-303-blind-timeout";
      main = "leaseCalib303BlindTimeout";
      witness = "blindHolderBounded";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # merged_bug_180 pre-fix (bughunt3-wave S6b): RV-KEYED identity —
    # OBS_KEY_RV restores the pre-fix keying (observed record and
    # evidence cursor on raw resourceVersion). A transmitted write is
    # LOST (renewSendLost: no protocol content moves) while a foreign
    # bumpRv moves the rv past the cursor; the rv-keyed evidence rule
    # consumes the ledger from churn the protocol never authored.
    # noFabricatedEvidence falls — the laundering the content keying
    # exists to prevent.
    quint-lease-calib-180-rv-keyed = mkQuintWitnessCheck {
      name = "lease-calib-180-rv-keyed";
      spec = "calibration/lease-180-rv-keyed";
      main = "leaseCalib180RvKeyed";
      witness = "noFabricatedEvidence";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # merged_bug_085 pre-fix (bughunt-4 S2, signed Q3): IMMEDIATE LOSE
    # on the believing renew 409 — CONFLICT_IMMEDIATE_LOSE restores
    # the lose-on-first-409 arm inside the holder-evidence world. Any
    # rv movement between the holder's GET and PUT (a foreign bumpRv,
    # or its own dropped-then-committed write surfacing) bounces the
    # CAS and the believing FIRST 409 runs the full lose edge with
    # neither holder evidence nor an exhausted deferral;
    # loseRequiresHolderEvidence falls. The blind failover (and the
    # ledger wipe that rode on it) the deferral law exists to prevent.
    quint-lease-calib-085-blind-conflict-lose = mkQuintWitnessCheck {
      name = "lease-calib-085-blind-conflict-lose";
      spec = "calibration/lease-085-blind-conflict-lose";
      main = "leaseCalib085BlindConflictLose";
      witness = "loseRequiresHolderEvidence";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # merged_bug_128 pre-fix (bughunt-4 S2): COUNT-KEYED step-down —
    # STEP_DOWN_COUNT_KEYED stamps requests with the generation-
    # derived transition count (which a false-alarm fence +
    # same-epoch re-acquire legitimately REPEATS) and skips the
    # acquire-edge clear. A request filed by tenure instance 1 serves
    # against instance 2 at the same count; noStaleStepDownServed
    # falls. The same-count ABA the per-acquire instance stamp
    # exists to prevent.
    quint-lease-calib-128-count-keyed-stepdown = mkQuintWitnessCheck {
      name = "lease-calib-128-count-keyed-stepdown";
      spec = "calibration/lease-128-count-keyed-stepdown";
      main = "leaseCalib128CountKeyedStepDown";
      witness = "noStaleStepDownServed";
      step = "calibStep";
      extraSpecs = [ "leaderElection" ];
    };

    # merged_bug_138 pre-fix: edge-only marks (verify pass removed, no
    # cadence obligation). An external strip ages unboundedly;
    # marksDivergenceBounded falls.
    quint-lease-calib-138-edge-only = mkQuintWitnessCheck {
      name = "lease-calib-138-edge-only";
      spec = "calibration/lease-138-edge-only";
      main = "leaseCalib138EdgeOnly";
      witness = "marksDivergenceBounded";
      step = "calibStep";
      extraSpecs = [ "leaderMarks" ];
    };

    # bug_181 pre-fix (bughunt2-wave slot 8): BOOL dirty flag — the
    # reconcile success path's store(false) clears EVERY dirtying
    # event regardless of when it landed. A mark between the spawn
    # snapshot and the clear (a leadership edge, the rebound hook) is
    # erased — the parked-PATCH clobber of the bug_181 red;
    # notClobbered falls. The DirtyGen clear-through arithmetic is
    # exactly what this pin guards.
    quint-lease-calib-181-bool-clear = mkQuintWitnessCheck {
      name = "lease-calib-181-bool-clear";
      spec = "calibration/lease-181-bool-clear";
      main = "leaseCalib181BoolClear";
      witness = "notClobbered";
      step = "calibStep";
      extraSpecs = [ "leaderMarks" ];
    };

    # The scheduler's cost-table leadership latch (NEW, bughunt2-wave
    # slot 8 merged_bug_212): was_leader + poller_tick_prelude + the
    # observability::LEADER_EDGES cost-latch cells as a protocol — the
    # standby store, the leading-edge reload (table catches up to the
    # world before any body runs), the body's persist, the lose cell
    # on EVERY lose-shaped transition (lost handler AND the rebound's
    # Compound delivery), and the foreign tenure that evolves the
    # world exactly when the real lease is not ours (standby, or
    # inside a rebound's unobserved lose→re-acquire gap). Headline
    # noStalePersist: no persist ever writes a previous tenure's
    # table over a foreign-evolved world — the bug_310/merged_bug_212
    # failure shape. The paired pin is
    # quint-costlatch-calib-212-acquire-only (the pre-fix acquire-only
    # rebound delivery skips the lose cell and must falsify).
    # Measured: exhaustive TLC <1s at the wired constants (transcript
    # authoritative) — the default 1800s budget is >1800x headroom.
    # r[verify sched.lease.rebound+4]
    quint-cost-latch = mkQuintCheck {
      name = "cost-latch";
      spec = "costLatch";
      main = "costLatchBase";
      invariants = [
        "boundsOK"
        "noStalePersist"
      ];
    };

    # Non-vacuity witnesses: the body's persist actually runs, and a
    # foreign tenure actually evolves the world, in the explored
    # space — the noStalePersist verdict constrains a live
    # interleaving, not an empty one.
    quint-cost-latch-witness-persist = mkQuintWitnessCheck {
      name = "cost-latch-witness-persist";
      spec = "costLatch";
      main = "costLatchBase";
      witness = "noPersist";
    };
    quint-cost-latch-witness-foreign = mkQuintWitnessCheck {
      name = "cost-latch-witness-foreign";
      spec = "costLatch";
      main = "costLatchBase";
      witness = "noForeignPersist";
    };

    # Deterministic named-run replay (reboundReloadsBeforePersistRun:
    # rebound with a foreign tenure inside the gap → lose cell →
    # reload-before-body → fresh persist).
    quint-cost-latch-runs = mkQuintRunCheck {
      name = "cost-latch-runs";
      spec = "costLatch";
      main = "costLatchBase";
    };

    # merged_bug_212 pre-fix: ACQUIRE-ONLY rebound delivery — the lose
    # cell skipped, the latch stays true across the unobserved holder
    # change, the next leading tick skips the reload and persists the
    # deposed tenure's table; noStalePersist falls.
    quint-costlatch-calib-212-acquire-only = mkQuintWitnessCheck {
      name = "costlatch-calib-212-acquire-only";
      spec = "calibration/costlatch-212-acquire-only";
      main = "costlatchCalib212AcquireOnly";
      witness = "noStalePersist";
      step = "calibStep";
      extraSpecs = [ "costLatch" ];
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
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "ingestLossCounted"
      ];
    };

    # Two executions of one derivation: the supersession scenarios. The
    # auth gate rejects the superseded execution's reopen; the
    # superseded execution's still-open session keeps writing to its own
    # (old) execution's log only; the two manifests grow concurrently
    # under disjoint exec-keyed namespaces.
    # r[verify store.log.append-auth+2]
    # r[verify obs.log.exec-keyed+2]
    quint-log-service-redispatch = mkQuintCheck {
      name = "log-service-redispatch";
      spec = "logService";
      main = "logServiceRedispatch";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "ingestLossCounted"
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
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "acceptedWithinCap"
        "ingestLossCounted"
      ];
    };

    # The TTL machinery against a live ingest pipeline, v2 ownership
    # split: the STORE pass strips chunks (age-only by design — it
    # never touches the lifecycle row) while the SCHEDULER's gcExecRow
    # reclaims the row behind the kernel eligibility (terminal AND
    # ledger-unreferenced AND aged out AND — v3, merged_bug_007 —
    # artifact-free: no surviving drv_log_chunks row, no live
    # log_ingest_sessions registry row, where registry liveness covers
    # the detached-but-undrained session whose disconnect drain is
    # still flushing); an expired-but-referenced execution's row
    # survives until the attempt-ledger GC releases it, and a row never
    # outruns its artifacts (noOrphanLogChunks).
    # r[verify store.log.sweep-ownership+1]
    quint-log-service-sweep = mkQuintCheck {
      name = "log-service-sweep";
      spec = "logService";
      main = "logServiceSweep";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "ingestLossCounted"
        "sweepOnlyTerminalUnreferenced"
        "noOrphanLogChunks"
      ];
    };

    # The PRODUCER plane (NEW, bughunt2-wave slot 8, bug_241): the
    # early uploader death (panic/abort while the build still
    # produces) stamps the channel-refusal watermark, the build keeps
    # producing into the closed channel, and the build-exit Drop must
    # disclose every refused line through the DiscardLedger
    # (UploadSink{Open,Lost} -- the only path that drops a batch is
    # the ledger method). Headline producerLossCounted: disjointness
    # from the drain-deadline counter (the ledger never counts
    # accepted lines), monotone sanity, and disclosure-complete at
    # build exit. The base lifecycle laws are co-verified -- the death
    # path's own un-acked disclosure must keep the ingest plane
    # intact. The paired pin is
    # quint-log-service-calib-producer-blind (the pre-fix silent Drop
    # must falsify). Measured: exhaustive TLC ~3s / ~53k distinct
    # states (transcript authoritative) -- the default 1800s budget
    # is ~600x headroom.
    # r[verify builder.log.loss-disclosure+4]
    quint-log-service-producer = mkQuintCheck {
      name = "log-service-producer";
      spec = "logService";
      main = "logServiceProducerLoss";
      invariants = [
        "boundsOK"
        "producerLossCounted"
        "ingestLossCounted"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Non-vacuity witness: lines are actually refused in the explored
    # space (an uploader dies and the build produces past the
    # watermark) -- the producerLossCounted verdict has work to do.
    quint-log-service-witness-refused = mkQuintWitnessCheck {
      name = "log-service-witness-refused";
      spec = "logService";
      main = "logServiceProducerLoss";
      witness = "noRefusedLines";
    };

    # Deterministic named-run replay (producerDeathDisclosedRun: die
    # mid-build, produce refused lines, exit-Drop discloses exactly
    # the refused range).
    quint-log-service-runs-producer = mkQuintRunCheck {
      name = "log-service-runs-producer";
      spec = "logService";
      main = "logServiceProducerLoss";
    };

    # bug_241 pre-fix: the producer-blind world -- the build-exit Drop
    # does not disclose; the bounced batches vanish (the production
    # red's zero counter increments); producerLossCounted falls.
    quint-log-service-calib-producer-blind = mkQuintWitnessCheck {
      name = "log-service-calib-producer-blind";
      spec = "logService";
      main = "logServiceCalibProducerBlind";
      witness = "producerLossCounted";
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
    # unassigned-writer rejection authGateExcludesUnassignedWriters is about.
    quint-log-service-witness-superseded-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-superseded-rejected";
      spec = "logService";
      main = "logServiceRedispatch";
      witness = "noSupersededOpenRejection";
    };

    # The durable cap boundary is actually reached in the resend regime
    # (acceptedTotal hits CAP via the at-least-once replay's
    # double-charge) — the cap conjuncts in the gate and the accept
    # guard are exercised, not vacuous.
    # r[verify store.log.caps-durable]
    quint-log-service-witness-cap-boundary = mkQuintWitnessCheck {
      name = "log-service-witness-cap-boundary";
      spec = "logService";
      main = "logServiceResend";
      witness = "noCapBoundary";
    };

    # CALIBRATION (expect-violation): the pre-fix per-SESSION cap
    # accounting — reconnects reset the account and the open gate has
    # no durable check — lets the durable footprint exceed the
    # per-execution cap (merged_bug_207). Pins that acceptedWithinCap
    # actually catches the bug class B2's durable-cap kernel fixed.
    quint-log-service-calib-session-caps = mkQuintWitnessCheck {
      name = "log-service-calib-session-caps";
      spec = "logService";
      main = "logServiceCalibSessionCaps";
      witness = "acceptedWithinCap";
    };

    # CALIBRATION (expect-violation): the open gate without the
    # claimed-exec assignment-row conjunct re-admits a
    # rewritten-in-place execution's writer
    # (authGateExcludesUnassignedWriters falsifies) — pins the v2
    # gate's one revocation path as load-bearing in the model.
    quint-log-service-calib-gate-row = mkQuintWitnessCheck {
      name = "log-service-calib-gate-row";
      spec = "logService";
      main = "logServiceCalibGateRow";
      witness = "authGateExcludesUnassignedWriters";
    };

    # The kind-mix regime: a materialization attempt takes the
    # latest-assignment slot mid-stream and the build execution's late
    # replay must still be admitted while the mat execution never is —
    # the displacement class merged_bug_101's claimed-exec re-keying
    # fixed, kind half (the gate's `attempt_kind` conjunct).
    # r[verify store.log.append-auth+2]
    quint-log-service-kind-mix = mkQuintCheck {
      name = "log-service-kind-mix";
      spec = "logService";
      main = "logServiceKindMix";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "ingestLossCounted"
        "buildTailAdmittable"
      ];
    };

    # A build execution's replay open is admitted while a later
    # materialization attempt holds the latest slot — the admission the
    # pre-fix latest-only gate displaced is reachable, so
    # buildTailAdmittable's verdict in the kind-mix regime is not
    # vacuous.
    quint-log-service-witness-kind-mix-replay = mkQuintWitnessCheck {
      name = "log-service-witness-kind-mix-replay";
      spec = "logService";
      main = "logServiceKindMix";
      witness = "noMatDispatchThenBuildReplayAdmitted";
    };

    # CALIBRATION (expect-violation): the pre-fix latest-assignment-only
    # open gate — a materialization mint landing after the build revokes
    # the build's replay (merged_bug_101's displacement hole) —
    # buildTailAdmittable falsifies.
    quint-log-service-calib-latest-only = mkQuintWitnessCheck {
      name = "log-service-calib-latest-only";
      spec = "logService";
      main = "logServiceCalibLatestOnly";
      witness = "buildTailAdmittable";
    };

    # An abandonment actually fires the disclosure counter — the
    # counted arm of the loss lattice is exercised.
    # r[verify builder.log.loss-disclosure+4]
    quint-log-service-witness-loss-counted = mkQuintWitnessCheck {
      name = "log-service-witness-loss-counted";
      spec = "logService";
      main = "logServiceBase";
      witness = "noLossCounterFired";
    };

    # CALIBRATION (expect-violation): the pre-fix rejected:true
    # suppression — the permanent-rejection discard skips the counter
    # and an execution with produced-but-never-stored lines abandons
    # silently (merged_bug_360) — ingestLossCounted falsifies.
    quint-log-service-calib-loss-escape = mkQuintWitnessCheck {
      name = "log-service-calib-loss-escape";
      spec = "logService";
      main = "logServiceCalibLossEscape";
      witness = "ingestLossCounted";
    };

    # CALIBRATION (expect-violation): the pre-fix age-only sweep SELECT
    # deletes a never-terminal or still-referenced execution's
    # lifecycle row (merged_bug_086) — sweepOnlyTerminalUnreferenced
    # falsifies.
    quint-log-service-calib-sweep-age-only = mkQuintWitnessCheck {
      name = "log-service-calib-sweep-age-only";
      spec = "logService";
      main = "logServiceCalibSweepAgeOnly";
      witness = "sweepOnlyTerminalUnreferenced";
    };

    # CALIBRATION (expect-violation): the execution-row GC without the
    # artifact-before-row conjuncts (merged_bug_007 pre-fix) reclaims a
    # row whose drv_log_chunks rows survive — noOrphanLogChunks
    # falsifies (the orphaned chunks are unreachable to the store's
    # sweep forever).
    quint-log-service-calib-gc-row-ignores-artifacts = mkQuintWitnessCheck {
      name = "log-service-calib-gc-row-ignores-artifacts";
      spec = "logService";
      main = "logServiceCalibGcRowIgnoresArtifacts";
      witness = "noOrphanLogChunks";
    };

    # A closer-stamped (status terminal, count NULL) execution row is
    # actually reclaimed in the sweep regime — bug_047's liveness path
    # is reachable, not merely encoded (noCloseStampedReclaim must
    # violate).
    quint-log-service-witness-close-stamped-reclaim = mkQuintWitnessCheck {
      name = "log-service-witness-close-stamped-reclaim";
      spec = "logService";
      main = "logServiceSweep";
      witness = "noCloseStampedReclaim";
    };

    # The sweep regime's composed runs: the v2 ownership-split
    # end-to-end (sweepCompleteLogRun, now through the artifact
    # conjuncts — the builder disconnect precedes the reclaim) and
    # bug_047's close-then-sweep liveness on the production red's exact
    # shape (a never-reporting MATERIALIZATION execution: close stamps,
    # row ages, ledger releases, gcExecRow reclaims).
    # r[verify store.log.sweep-ownership+1]
    # r[verify sched.db.exec-stamp-on-close]
    quint-log-service-runs-sweep = mkQuintRunCheck {
      name = "log-service-runs-sweep";
      spec = "logService";
      main = "logServiceSweep";
    };

    # CALIBRATION (composed): the pre-fix assignment close that stamps
    # nothing — after close, expiry, and ledger release the row is
    # STILL ineligible: the immortal execution row, demonstrated
    # step-by-step (closeNoStampImmortalRun).
    quint-log-service-calib-close-no-stamp = mkQuintRunCheck {
      name = "log-service-calib-close-no-stamp";
      spec = "logService";
      main = "logServiceCalibCloseNoStamp";
      match = "closeNoStampImmortalRun";
    };

    # An expired execution SURVIVES the sweep with its row intact
    # because it is still non-terminal or ledger-referenced — the v2
    # eligibility actually refuses something the age-only sweep took.
    quint-log-service-witness-live-exec-survives = mkQuintWitnessCheck {
      name = "log-service-witness-live-exec-survives";
      spec = "logService";
      main = "logServiceSweep";
      witness = "noLiveExecSurvivesSweep";
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

    # The served-stream (reader) plane: a live TailLog subscriber over
    # the capacity-1 fan-out queue — the seam where merged_bug_180/306/
    # 311 lived. The hold check pins the FIXED reader: a fan-out drop is
    # repaired by back-filling from the manifest/buffers (serve_tail's
    # recovery; the gateway reopen-once and the dashboard gap rows
    # project to the same kernel step), and a final stamp claiming
    # completeness means the subscriber rendered every recorded line.
    # The four pre-existing log-service regimes bind ENABLE_READER =
    # false and were re-measured BYTE-IDENTICAL at the introduction.
    # r[verify store.log.tail-fanout-recovery]
    # r[verify store.log.tail-grace-drain+2]
    quint-log-service-served = mkQuintCheck {
      name = "log-service-served";
      spec = "logService";
      main = "logServiceServed";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesUnassignedWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
        "ingestLossCounted"
        "servedStreamNoSilentLoss"
        "servedStreamSpanExact"
        "readerBoundsOK"
        # resumeCursorSound is deliberately NOT bound here: under
        # RECONNECT a disclosed-gap span can re-materialize below the
        # cursor via the honest replay (the disclosure already covered
        # it — the gateway's accepted_gap_floor story). The law's home
        # is the single-session worker-gap plane, where apalache holds
        # it; its twin (calib-resume-past-gap) still falsifies.
        "recoveryOnlyOnDrop"
        "batchLinesBounded"
        "noCrossExecRowInterleave"
      ];
    };

    # CALIBRATION (expect-violation): the pre-fix reader — the cursor
    # advances past a fan-out drop with no back-fill and the final
    # stamp performs no catch-up (the silent splice the wave closed) —
    # servedStreamNoSilentLoss falsifies. Paired with
    # quint-log-service-served; the deterministic counterexample replay
    # is fanoutDropSilentLossRun in the calibration module.
    quint-log-service-calib-reader-advance = mkQuintWitnessCheck {
      name = "log-service-calib-reader-advance";
      spec = "logService";
      main = "logServiceCalibReaderAdvance";
      witness = "servedStreamNoSilentLoss";
    };

    # Reader-plane non-vacuity: the queue drop, the observed jump, and
    # the back-fill repair are each REACHABLE in the served regime (the
    # plane is not vacuously green).
    quint-log-service-witness-fanout-drop = mkQuintWitnessCheck {
      name = "log-service-witness-fanout-drop";
      spec = "logService";
      main = "logServiceServed";
      witness = "noFanoutDrop";
    };

    quint-log-service-witness-reader-gap = mkQuintWitnessCheck {
      name = "log-service-witness-reader-gap";
      spec = "logService";
      main = "logServiceServed";
      witness = "noReaderGapObserved";
    };

    quint-log-service-witness-reader-recovery = mkQuintWitnessCheck {
      name = "log-service-witness-reader-recovery";
      spec = "logService";
      main = "logServiceServed";
      witness = "noReaderRecovery";
    };

    # ------------------------------------------------------------------
    # bughunt-2 slot 6: the served-stream delta planes. Three LIVE
    # regimes (worker-gap/drop-wake, exec axis, admission flood), each
    # invariant carrying its falsify twin below — a twin that stops
    # violating means the live law went vacuous. The four pre-existing
    # base regimes bind every delta flag false and MAX_BATCH_LINES =
    # MAX_LINE (the oversized arm provably dead in the bounded domain);
    # re-measured at introduction.
    # r[verify store.log.gap-provenance]
    # r[verify store.log.served-claim]
    quint-log-service-worker-gap = mkQuintCheck {
      name = "log-service-worker-gap";
      spec = "logService";
      main = "logServiceWorkerGap";
      invariants = [
        "boundsOK"
        "readerBoundsOK"
        "servedStreamSpanExact"
        "servedStreamNoSilentLoss"
        "resumeCursorSound"
        "recoveryOnlyOnDrop"
        "batchLinesBounded"
        "noCrossExecRowInterleave"
      ];
    };

    # The execution axis: a retried build's new numbering is disclosed
    # (explicit switch, cursor reset), never silently spliced or
    # swallowed (merged_bug_002 — kernel visit_chunk_keyed / the
    # dashboard's keyed mirror).
    quint-log-service-exec-axis = mkQuintCheck {
      name = "log-service-exec-axis";
      spec = "logService";
      main = "logServiceExecAxis";
      invariants = [
        "boundsOK"
        "readerBoundsOK"
        "noCrossExecRowInterleave"
      ];
    };

    # The admission bound (bug_298): MAX_BATCH_LINES shrinks to 1 so
    # the oversized arm bites — per-batch reject, stream open.
    # r[verify store.log.write-read-bound+2]
    quint-log-service-flood = mkQuintCheck {
      name = "log-service-flood";
      spec = "logService";
      main = "logServiceFlood";
      invariants = [
        "boundsOK"
        "batchLinesBounded"
      ];
    };

    # The slot-6 delta also extends the SERVED regime's bound set with
    # the four new laws (they hold there with every delta flag off —
    # the structural arms alone enforce them).

    # --- falsify twins (expect-violation) -------------------------------
    quint-log-service-calib-oversized = mkQuintWitnessCheck {
      name = "log-service-calib-oversized";
      spec = "logService";
      main = "logServiceCalibOversized";
      witness = "batchLinesBounded";
    };

    quint-log-service-calib-recovery-ungated = mkQuintWitnessCheck {
      name = "log-service-calib-recovery-ungated";
      spec = "logService";
      main = "logServiceCalibRecoveryUngated";
      witness = "recoveryOnlyOnDrop";
    };

    quint-log-service-calib-stamp-skips = mkQuintWitnessCheck {
      name = "log-service-calib-stamp-skips";
      spec = "logService";
      main = "logServiceCalibStampSkips";
      witness = "servedStreamNoSilentLoss";
    };

    quint-log-service-calib-exec-splice = mkQuintWitnessCheck {
      name = "log-service-calib-exec-splice";
      spec = "logService";
      main = "logServiceCalibExecSplice";
      witness = "noCrossExecRowInterleave";
    };

    quint-log-service-calib-drop-wake-lost = mkQuintWitnessCheck {
      name = "log-service-calib-drop-wake-lost";
      spec = "logService";
      main = "logServiceCalibDropWakeLost";
      witness = "servedStreamNoSilentLoss";
    };

    quint-log-service-calib-resume-past-gap = mkQuintWitnessCheck {
      name = "log-service-calib-resume-past-gap";
      spec = "logService";
      main = "logServiceCalibResumePastGap";
      witness = "resumeCursorSound";
    };

    # --- reachability witnesses (expect-violation) -----------------------
    quint-log-service-witness-worker-gap = mkQuintWitnessCheck {
      name = "log-service-witness-worker-gap";
      spec = "logService";
      main = "logServiceWorkerGap";
      witness = "noWorkerGapAppended";
    };

    quint-log-service-witness-oversized-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-oversized-rejected";
      spec = "logService";
      main = "logServiceFlood";
      witness = "noOversizedRejected";
    };

    quint-log-service-witness-drop-wake = mkQuintWitnessCheck {
      name = "log-service-witness-drop-wake";
      spec = "logService";
      main = "logServiceWorkerGap";
      witness = "noDropWakeRecovery";
    };

    # ------------------------------------------------------------------
    # bughunt-4 S6a: the bilateral ingest-liveness contract
    # (merged_bug_335 / #5-S Q1) and the attach-hello handshake
    # (merged_bug_067), on the self-contained logIngestLiveness plane —
    # deliberately NOT a logService import (the base const frame is
    # instantiated by every calibration module; extending it for a
    # dimension none of them explores would touch all 26 frames). The
    # small-int mirror keeps the real conformance relation
    # (period x margin < abort: 2 x 2 < 5); the REAL const pair is
    # enforced by rio-common's conformance test — the model's job is
    # the law's shape, not the numbers. Measured 2026-06-09, tlc
    # backend, full scope: live [ok] exhaustive 91 distinct/depth
    # 16/822ms; quiet-churn [violation] 143 distinct/832ms;
    # silent-attach [violation] 32 distinct/818ms — the 1800s default
    # budget is three orders of magnitude of headroom.
    # r[verify store.log.ingest-idle-abort+1]
    # r[verify store.log.attach-hello]
    quint-log-ingest-liveness = mkQuintCheck {
      name = "log-ingest-liveness";
      spec = "logService";
      main = "logIngestLivenessLive";
      invariants = [
        "conformantNeverIdleAborted"
        "attachAlwaysIdentified"
        "helloPrecedesFanout"
        "boundsOK"
      ];
    };

    # CALIBRATION (expect-violation): the pre-fix producer-less world —
    # the store's enforcement exists, no keepalive producer does, and
    # the quiet-build abort churn is reachable (merged_bug_335's
    # defect): quietSessionStaysOpen violates.
    quint-log-ingest-calib-quiet-churn = mkQuintWitnessCheck {
      name = "log-ingest-calib-quiet-churn";
      spec = "logService";
      main = "logIngestCalibQuietChurn";
      witness = "quietSessionStaysOpen";
    };

    # CALIBRATION (expect-violation): the pre-fix silent attach — no
    # hello, so a follow attach across the execution switch leaves the
    # observer holding the dead execution's stamp (merged_bug_067's
    # defect): observerNeverStale violates.
    quint-log-ingest-calib-silent-attach = mkQuintWitnessCheck {
      name = "log-ingest-calib-silent-attach";
      spec = "logService";
      main = "logIngestCalibSilentAttach";
      witness = "observerNeverStale";
    };

    # ------------------------------------------------------------------
    # rio-scheduler's retry/poison/cascade machinery: the post-collapse
    # model (retry-formal Phase 1c). retryPolicy.qnt encodes the code as
    # it exists after the Phase-1b nine-site collapse -- every entry
    # point's verdict is the reference fold (decide()) over the durable
    # attempt ledger, evaluated and persisted in the appending
    # transaction.
    #
    # Executor-campaign 1c' (T-1c'.6): the as-built-CHANNEL regimes
    # (worker / dual / crash / failover) and their named-run and witness
    # checks are retired with the stream machinery they modeled -- the
    # attempt-opening dispatch push, the disconnect channel (E5), the
    # recently_disconnected-correlated controller reports (E6/E7), the
    # correlation-TTL establishment, the backstop (E8) and the
    # dispatch-time fleet-exhaust arm (E9) no longer exist in the code.
    # The pull-mode environment regime (retryPolicyPull, wired at 1b as
    # quint-retry-policy-pull in the executor-campaign block below) is
    # the wired proof of the same fold invariants over the live
    # event-arrival environment; it now also carries the model-checked
    # markers the retired regime checks held. The retirement record --
    # including the disposition of every retired non-vacuity pin and
    # what carries it now -- is in
    # docs/spec/models/retry-invariant-map.md (the executor-campaign
    # 1c' retirement section); the witness vals stay defined in the
    # core module, and the load-bearing ones are re-pinned on the pull
    # regime here.
    #
    # The Stage-B as-built encoding (retryPolicyAsBuilt.qnt), its
    # Stage-C calibration corpus (calibration/retry-*.qnt) and the six
    # quint-retry-calib-* checks were retired in the retry campaign's
    # Phase 2 once the acceptance table consolidated their evidence --
    # see docs/spec/models/retry-invariant-map.md.

    # Non-vacuity witnesses re-pinned on the pull regime (T-1c'.6): the
    # contended states the retired regimes' witnesses pinned that are
    # still constructible -- and load-bearing for the pull check's HOLD
    # list -- are re-proven reachable in the pull regime's explored
    # space (threshold poison, the cache-hit poison clear, the
    # established-crash loop reaching a terminal, a failover landing on
    # a non-empty history). Two further re-pins (the poison-TTL expiry
    # clear and the failover-on-a-live-poisoned-row state) are verified
    # violating on this regime but are demoted to documented manual
    # targets instead of wired checks: their derivations repeatedly hit
    # the cold-server conversion failure documented above while four
    # identically-shaped siblings built green, so the demotion is a
    # tooling-budget call, recorded in the retry map, not a
    # reachability gap. The pins whose producers were deleted with the
    # stream machinery (the appending-tx fault at TX_FAULTS = 0, the
    # as-built channel-ordering shapes) are likewise recorded as
    # retired in the retry map rather than silently dropped.
    quint-retry-policy-pull-witness-threshold = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-threshold";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noThresholdPoison";
    };
    quint-retry-policy-pull-witness-cache-hit = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-cache-hit";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noCacheHitClear";
    };
    quint-retry-policy-pull-witness-crash-terminal = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-crash-terminal";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noCrashLoopTerminal";
    };
    quint-retry-policy-pull-witness-failover-history = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-failover-history";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noFailoverWithHistory";
    };

    # ------------------------------------------------------------------
    # rio-store's chunk reference-counting subsystem, AS BUILT: the exact
    # chunks.refcount counter and the claim/heartbeat/drop-guard/token/
    # reaper machinery that maintains it (chunkLiveness.qnt — the
    # refcount-formal campaign's Phase-0 Stage-B model; the invariant ↔
    # rule map is docs/spec/models/refcount-invariant-map.md). Four
    # exhaustive regimes mirror design §3.2: base (one writer, no
    # faults), crash (process death between transactions, the drain's
    # mid-commit crash), contend (two writers sharing chunks against the
    # GC pipeline and drain), corrupt (an unparseable chunk_list at
    # deletion time — the C12 skip). The corrupt regime checks the
    # CARVED forms of CR-2/CR-3 (the rules' sanctioned-deviation
    # clauses); the unconditional forms falsifying there is the
    # pre-registered as-built deviation, wired below as expect-violation
    # checks so the leak class stays demonstrated, not assumed. State
    # counts, depths and wall-clocks live in the introducing commits'
    # messages and the checks' transcripts.
    #
    # Marker scope: of the Stage-A spec-audit rules, the surviving ones
    # (no-live-collect, liveness-not-presence) keep verify markers here,
    # on the regime that makes each load-bearing; the two as-built
    # counter rules the audit added (refcount-meaning,
    # refcount-decrement) were retired with the counter writers in
    # Release B, so these checks no longer claim spec coverage for them
    # — the checks themselves stay wired against the as-built model
    # until its own retirement (Phase 2). Pre-existing mechanism rules
    # the as-built machinery still implements (upsert-inserted,
    # chunk-upload-committed, placeholder-claim, orphan-heartbeat) keep
    # the model-checked form on top of their unit-test markers; rules
    # whose text now describes the replacement collector (refcount-txn's
    # upsert+touch pairing, grace-ttl, pending-deletes,
    # bounded-garbage-retention, two-phase) carry their verify markers
    # at the chunkCollect wirings below instead. Witness checks carry no
    # markers (same policy as the other models).
    # ------------------------------------------------------------------

    # The base regime: one writer over two paths and three hashes with
    # the full chunk-list alphabet, the explicit abort/rollback paths,
    # the path sweep, the orphan-chunk sweep and the drain — no faults.
    # The counter must equal the manifest fold at every state, garbage
    # must stay reclaimable, and no referenced chunk's object may be
    # deleted.
    # r[verify store.chunk.no-live-collect]
    quint-chunk-liveness-base = mkQuintCheck {
      name = "chunk-liveness-base";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The crash regime: two writers with process death enabled at every
    # in-flight phase (C1/C2/C5/C6/C7), the drain's S3-delete-then-
    # commit-fails window (C10), and the stale-reclaim repair pair (the
    # 300 s hot-path reclaim and the 15-minute scanner). The counter
    # stays exactly equal to the fold through every crash window (the
    # abandoned rows keep their manifests, so the garbage is accounted
    # garbage), a reclaimed chunk's cleared uploaded_at forces the next
    # writer to re-PUT instead of trusting the counter, and a live
    # heartbeating owner is never reaped.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.cas.upsert-inserted+3]
    # r[verify store.cas.chunk-upload-committed]
    # r[verify store.gc.orphan-heartbeat]
    # r[verify store.put.placeholder-claim+2]
    quint-chunk-liveness-crash = mkQuintCheck {
      name = "chunk-liveness-crash";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The contend regime: two live writers sharing a chunk against the
    # GC pipeline and the drain — the G4a collect-vs-re-reference TOCTOU
    # family, the orphan-chunk sweep's select-vs-update race (C11), the
    # by-count batch sweep, and the late-cleanup no-op contention. No
    # process death: every interleaving is a healthy-process schedule.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.chunk.liveness-not-presence]
    quint-chunk-liveness-contend = mkQuintCheck {
      name = "chunk-liveness-contend";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The corrupt regime: an existing manifest_data.chunk_list can rot
    # and every deletion path that parses it at delete time skips the
    # decrement while still deleting the manifest (C12). The CARVED
    # invariant forms hold: the counter equals the fold plus exactly the
    # observably-skipped decrements, stranded garbage is exactly the
    # skipped amount, and the data-loss invariant is untouched (the skip
    # errs toward retention). The unconditional forms are the
    # pre-registered falsifications below, never invariants here.
    # r[verify store.chunk.no-live-collect]
    quint-chunk-liveness-corrupt = mkQuintCheck {
      name = "chunk-liveness-corrupt";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2CarvedCorrupt"
        "cr3CarvedCorrupt"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The deterministic reproducer runs, one check per regime: the
    # happy-path walkthrough, the own-heartbeat token no-op (C4), the
    # crash-then-hot-path-reclaim-then-re-upload shape (I-040/I-207),
    # the shared-chunk by-count batch sweep, and the corrupt-skip
    # permanent leak are replayed step by step with their expectations
    # re-asserted.
    quint-chunk-liveness-runs-base = mkQuintRunCheck {
      name = "chunk-liveness-runs-base";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
    };
    quint-chunk-liveness-runs-crash = mkQuintRunCheck {
      name = "chunk-liveness-runs-crash";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
    };
    quint-chunk-liveness-runs-contend = mkQuintRunCheck {
      name = "chunk-liveness-runs-contend";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
    };
    quint-chunk-liveness-runs-corrupt = mkQuintRunCheck {
      name = "chunk-liveness-runs-corrupt";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
    };

    # Non-vacuity witnesses for the chunkLiveness regimes. Each check
    # passes only when the checker violates its witness — machine-checked
    # evidence that the scenario a regime's invariants constrain is
    # actually reachable in that regime's explored space. Deliberately no
    # tracey markers here (same policy as the other models' witnesses).

    # The base regime's headline states: a complete chunked upload
    # exists, a backend delete actually fires, a referenced chunk whose
    # presence is not yet confirmed exists (the M_033 precondition), the
    # own-heartbeat token no-op (C4) is reachable, and the heartbeat
    # actually resets staleness.
    quint-chunk-liveness-witness-complete-upload = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-complete-upload";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noCompleteUpload";
    };
    quint-chunk-liveness-witness-backend-delete = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-backend-delete";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noBackendDelete";
    };
    quint-chunk-liveness-witness-m033-precondition = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-m033-precondition";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noUnconfirmedReferencedChunk";
    };
    quint-chunk-liveness-witness-stale-token = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-stale-token";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noStaleTokenRollbackNoop";
    };
    quint-chunk-liveness-witness-heartbeat-reset = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-heartbeat-reset";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noHeartbeatReset";
    };

    # The crash regime's fault alphabet is reachable: the C1 (claimed),
    # C2 (upgraded) and C5 (cleanup-pending — the state C3/C7 collapse
    # onto) crash windows, the two-writers-staged-then-crashed C6 shape,
    # the abandoned-but-accounted leak state those windows leave behind,
    # and both stale-reclaim repair paths (hot-path and scanner)
    # actually firing.
    quint-chunk-liveness-witness-crash-claimed = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-claimed";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashAtClaimed";
    };
    quint-chunk-liveness-witness-crash-upgraded = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-upgraded";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashAfterUpgrade";
    };
    quint-chunk-liveness-witness-crash-pending-reap = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-pending-reap";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashBeforeReap";
    };
    quint-chunk-liveness-witness-double-crash-staged = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-double-crash-staged";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noDoubleCrashStaged";
    };
    quint-chunk-liveness-witness-abandoned-accounting = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-abandoned-accounting";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noAbandonedAccounting";
    };
    quint-chunk-liveness-witness-hotpath-reclaim = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-hotpath-reclaim";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noHotpathReclaim";
    };
    quint-chunk-liveness-witness-scanner-reap = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-scanner-reap";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noScannerReap";
    };

    # The contend regime's contended states: a shared chunk decremented
    # by count in one batch transaction, the drain re-check skipping a
    # resurrected chunk, the orphan-chunk sweep's inner re-check
    # excluding a candidate resurrected after the outer SELECT (C11),
    # and an owner-side cleanup no-opping against a foreign or missing
    # row.
    quint-chunk-liveness-witness-shared-by-count = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-shared-by-count";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noSharedByCountDecrement";
    };
    quint-chunk-liveness-witness-drain-resurrect = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-drain-resurrect";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noDrainResurrectSkip";
    };
    quint-chunk-liveness-witness-orphan-recheck = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-orphan-recheck";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noOrphanRecheckSave";
    };
    quint-chunk-liveness-witness-late-cleanup-noop = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-late-cleanup-noop";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noLateCleanupNoop";
    };

    # The pre-registered as-built deviations of the corrupt regime
    # (refcount-invariant-map.md, CR-2/CR-3 rows): with a corrupt
    # chunk_list skip, the exact counter-equals-fold form and the
    # unconditional no-stranded-garbage form MUST falsify — the C12
    # permanent leak the design's replacement exists to dissolve. These
    # are reproducer checks for documented defects, not regressions: if
    # either stops falsifying, the model has stopped reaching the leak
    # (or the code stopped leaking) and the invariant map's Stage-B
    # section must be revisited. The third check pins the literal leak
    # shape (refcount above zero with no referencing manifest).
    quint-chunk-liveness-corrupt-c12-overcount = mkQuintWitnessCheck {
      name = "chunk-liveness-corrupt-c12-overcount";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "cr3CounterRefinesFold";
    };
    quint-chunk-liveness-corrupt-c12-stranded = mkQuintWitnessCheck {
      name = "chunk-liveness-corrupt-c12-stranded";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "cr2NoStrandedGarbage";
    };
    quint-chunk-liveness-witness-corrupt-leak = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-corrupt-leak";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "noCorruptLeak";
    };

    # The threshold-ordering inversion: with the hot-path reclaim
    # threshold lowered to the heartbeat deadline (production:
    # 30 s heartbeat vs 300 s hot-path vs 900 s scanner), reaping a
    # live, progressing owner becomes reachable and S5 falsifies — the
    # machine-checked evidence that the scaled clock constants preserve
    # the ordering the heartbeat/reclaim design depends on, and that S5
    # does not hold vacuously in the well-ordered regimes.
    quint-chunk-liveness-threshold-order = mkQuintWitnessCheck {
      name = "chunk-liveness-threshold-order";
      spec = "chunkLiveness";
      main = "chunkLivenessThresholdOrder";
      witness = "s5LiveOwnerNeverReaped";
    };

    # Stage-C calibration witnesses for the chunk-refcount subsystem (the
    # historical-fix corpus replayed against the as-built model — the
    # refcount-formal campaign's Phase-0 Stage C). Each check
    # instantiates the as-built chunkLiveness model, swaps ONE owner-side
    # entry point for its PRE-FIX behavior (the calibration module's
    # `calibStep`), and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-
    # checked evidence that the model would re-find that bug class if it
    # were reintroduced, and that the invariant is not vacuous for it.
    # The full per-commit calibration table (and the evidence-only
    # override modules that are not wired here) lives in
    # docs/spec/models/refcount-invariant-map.md; the wired ones are the
    # representative per-family regression guards (one per encodable
    # family, deepest consequence, cheap state space). Deliberately no
    # tracey markers (same policy as the other witness checks).
    #
    # The G1 (token rollback) and G2 (inline-only reap) guards were
    # retired at Release B of the refcount-formal campaign: the
    # PlaceholderToken and the decrement family they guarded no longer
    # exist in the code, so the regressions they watched for cannot
    # recur by construction. Their override modules
    # (calibration/refcount-g{1,2}.qnt) stay committed as evidence and
    # remain re-runnable by hand; the disposition record is in the
    # invariant map ("Release B calibration-check disposition"). The
    # surviving G3/G4a/G5 guards below watch mechanisms that outlived
    # the counter (presence keyed on uploaded_at, the drain re-check,
    # the heartbeat) and stay wired against the as-built model until
    # Phase 2 re-points them at the model of record.

    # G3 (dd5c11376 / M_033): the needs-upload verdict is keyed on the
    # liveness record instead of uploaded_at — a writer skips the PUT for
    # a chunk nobody confirmed, the 2026-04-06 data-loss precondition.
    quint-refcount-calib-g3-counter-presence = mkQuintWitnessCheck {
      name = "refcount-calib-g3-counter-presence";
      spec = "calibration/refcount-g3";
      main = "refcountCalibG3CounterAsPresence";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "cr4PresenceFromConfirmedUpload";
    };

    # G4a (aa738a5d7 / M_006): the drain loses its same-transaction
    # re-check before DeleteObject — a chunk resurrected by a re-upload
    # between soft-delete and drain loses its object while referenced
    # (the data-loss invariant's action form).
    quint-refcount-calib-g4a-drain-recheck = mkQuintWitnessCheck {
      name = "refcount-calib-g4a-drain-recheck";
      spec = "calibration/refcount-g4a";
      main = "refcountCalibG4aDrainNoRecheck";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "cr1NoLiveChunkCollected";
    };

    # G5 (a1b49b4a3): no heartbeat — a live, progressing upload outlives
    # the stale threshold and the reclaim path reaps it mid-flight.
    quint-refcount-calib-g5-no-heartbeat = mkQuintWitnessCheck {
      name = "refcount-calib-g5-no-heartbeat";
      spec = "calibration/refcount-g5";
      main = "refcountCalibG5NoHeartbeat";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "s5LiveOwnerNeverReaped";
    };

    # ------------------------------------------------------------------
    # rio-store's chunk-liveness subsystem, REPLACEMENT shape: lazy
    # mark-and-collect over the durable manifests (chunkCollect.qnt —
    # the refcount-formal campaign's Phase-1a / plan T-1a.5 model,
    # design §4/§4.6; the verdict table and witness list are in
    # docs/spec/models/refcount-invariant-map.md "Replacement model").
    # The model is the counter-free end state: no refcount, no
    # decrement/token machinery; the collector of
    # rio-store/src/gc/collect.rs (snapshot → fail-closed mark →
    # per-batch sweep → finish) modeled with its live (soft-delete +
    # enqueue) arm, the 068-era last_referenced_at touch, the
    # T-pre.1 deleted-guard on the presence commit, reap/path-sweep as
    # path-row janitors, and the deleted-only drain re-check. Four
    # exhaustive regimes mirror the as-built model's; two further
    # exhaustive regimes carry the holds-halves of the §4.6
    # falsification pairs (writer-transaction bound; late-mark guard
    # under a relaxed heartbeat contract); each pair's falsify-half is
    # an expect-violation check against an instantiation that flips
    # exactly one design guard off. State counts, depths and
    # wall-clocks live in the introducing commits' messages and the
    # checks' transcripts.
    #
    # Marker scope: the two replacement rules added by plan T-1a.5
    # (store.chunk.liveness-derived, store.gc.chunk-collect) carry
    # their verify markers here, on the regimes that make each
    # load-bearing; their implementation-side markers live with the
    # live collector arm (gc::collect). Surviving mechanism rules whose
    # text is unchanged (no-live-collect, liveness-not-presence,
    # upsert-inserted, chunk-upload-committed, placeholder-claim,
    # orphan-heartbeat) gain the replacement-model form on top of their
    # as-built chunk-liveness markers, which stay in place until the
    # as-built model's own retirement (Phase 2). Rules amended at the
    # cutover to describe collector behavior (grace-ttl,
    # bounded-garbage-retention, pending-deletes) carry their bumped
    # verify markers here only — the as-built model checks the
    # pre-cutover machinery, not the amended sentences. Witness and
    # falsification checks carry no markers (same policy as every
    # other expect-violation check).
    # ------------------------------------------------------------------

    # The base regime: one writer over two paths and three hashes with
    # the full chunk-list alphabet, the collector and the drain — no
    # faults. Liveness is recomputed from the manifests each cycle; no
    # referenced chunk's object may be deleted, garbage must stay
    # reclaimable, and presence is never inferred from liveness.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+3]
    # r[verify store.chunk.refcount-txn+2]
    quint-chunk-collect-base = mkQuintCheck {
      name = "chunk-collect-base";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
        # bughunt wave D1 (merged_bug_336): the post-pass tombstone
        # reap joined the shared alphabet — the row-retention clause of
        # bounded-garbage-retention+3, exhaustively beside CR-1/CR-2/
        # CR-4 (the resurrect-vs-reap race coverage).
        "reapSafety"
      ];
    };

    # The crash regime: two writers with process death enabled at every
    # in-flight phase, the drain's S3-delete-then-commit-fails window,
    # the path-row janitors, and the collector. A crashed upload's
    # manifest is reaped by the janitors; its chunks are then ordinary
    # unreferenced rows the next cycle collects — no counter repair
    # exists or is needed, and the cleared uploaded_at still forces the
    # next writer to re-PUT instead of trusting liveness for presence.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+3]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.cas.upsert-inserted+3]
    # r[verify store.cas.chunk-upload-committed]
    # r[verify store.gc.orphan-heartbeat]
    # r[verify store.put.placeholder-claim+2]
    quint-chunk-collect-crash = mkQuintCheck {
      name = "chunk-collect-crash";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The contend regime: two live writers sharing chunks against the
    # collector and the drain — the mark-stale race the
    # last_referenced_at touch closes (a manifest committing after the
    # cycle's mark snapshot re-references an old chunk), the
    # resurrect-vs-drain TOCTOU with the deleted-only re-check, and the
    # late-cleanup no-op contention. No process death. The touch/grace
    # term this regime exercises is the amended grace-ttl eligibility
    # predicate (zero references at the mark snapshot AND older than
    # grace from GREATEST(created_at, last_referenced_at)).
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+3]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.gc.pending-deletes+2]
    # r[verify store.chunk.grace-ttl+2]
    quint-chunk-collect-contend = mkQuintCheck {
      name = "chunk-collect-contend";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The corrupt regime: an existing manifest_data.chunk_list can rot
    # (C12). Under the replacement the polarity is fail-closed — the
    # validation pass aborts the cycle, nothing anywhere is collected
    # while the corruption persists, and the operator quarantine of
    # adjudication 7 is the sanctioned remediation. CR-2 is checked in
    # its carved form here; the unconditional form's expected
    # falsification is wired below as an expect-violation check.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+3]
    quint-chunk-collect-corrupt = mkQuintCheck {
      name = "chunk-collect-corrupt";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2CarvedCorrupt"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The writer-transaction-bound holds-half (§4.1 collect-soundness
    # condition): the upgrade transaction is split into begin/commit
    # and its duration is ceiling-bounded at the grace window by a tick
    # precondition — CR-1 holds for every interleaving and any
    # cycle/scan placement. The overrun falsify-half below relaxes the
    # ceiling by one tick. In production the bound is the monitored
    # rio_store_chunk_upgrade_tx_seconds assumption (plan T-1a.4).
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    quint-chunk-collect-writer-bounded = mkQuintCheck {
      name = "chunk-collect-writer-bounded";
      spec = "chunkCollect";
      main = "chunkCollectWriterBounded";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # ── placeholderClaim: the manifests claim ownership protocol after
    # the bughunt-wave D1 takeover rework (merged_bug_003 stall-takeover
    # predicate + merged_bug_082 validation floor). The main regime
    # holds the predicate's THREE real guarantees: the 092 phase
    # exemption (no parked/persisting steal), the 2x-heartbeat floor
    # theorem (no steady-advancer steal on stamp lag), and the liveness
    # conjunct (strikes only inside the freshness window — long-dead
    # owners always fall to the no-strike reap arm). The heartbeat is
    # modeled as the reliable claim-guarded ticker it is in cas.rs.
    # r[verify store.substitute.stale-reclaim+3]
    # r[verify store.put.placeholder-claim+2]
    quint-placeholder-claim-main = mkQuintCheck {
      name = "placeholder-claim-main";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "placeholderClaim";
      main = "placeholderClaimMain";
      invariants = [
        "boundsOK"
        "liveOwnerNeverDeposed"
        "steadyAdvancerNeverDeposed"
        "strikeOnlyWithinLivenessWindow"
      ];
    };

    # Falsification twin: the PRE-FIX predicate (no liveness conjunct)
    # strikes an owner dead past the stall window — the merged_bug_003
    # 180-300s strike window, demonstrated as a model violation.
    quint-placeholder-claim-falsify-no-liveness = mkQuintWitnessCheck {
      name = "placeholder-claim-falsify-no-liveness";
      spec = "placeholderClaim";
      main = "placeholderClaimNoLiveness";
      witness = "strikeOnlyWithinLivenessWindow";
    };

    # Falsification twin: pre-092 (no phase column) — a budget-parked
    # owner with frozen progress and a live heartbeat is deposed
    # mid-park (the backpressured-owner steal).
    quint-placeholder-claim-falsify-no-phase = mkQuintWitnessCheck {
      name = "placeholder-claim-falsify-no-phase";
      spec = "placeholderClaim";
      main = "placeholderClaimNoPhase";
      witness = "liveOwnerNeverDeposed";
    };

    # Falsification twin: a stall window below 2x the heartbeat
    # interval deposes a live owner advancing every tick, on stamp lag
    # alone — the model justification for the Config::validate() floor
    # substitute_stall >= 2 * heartbeat (merged_bug_082).
    quint-placeholder-claim-falsify-sub-heartbeat = mkQuintWitnessCheck {
      name = "placeholder-claim-falsify-sub-heartbeat";
      spec = "placeholderClaim";
      main = "placeholderClaimSubHeartbeatWindow";
      witness = "steadyAdvancerNeverDeposed";
    };

    # Reachability witnesses on the main regime: the intended takeover
    # (an alive-but-wedged downloader is stolen) and the no-strike reap
    # of a dead owner both actually happen.
    quint-placeholder-claim-witness-wedged-deposed = mkQuintWitnessCheck {
      name = "placeholder-claim-witness-wedged-deposed";
      spec = "placeholderClaim";
      main = "placeholderClaimMain";
      witness = "wedgedOwnerDeposedW";
    };
    quint-placeholder-claim-witness-dead-reaped = mkQuintWitnessCheck {
      name = "placeholder-claim-witness-dead-reaped";
      spec = "placeholderClaim";
      main = "placeholderClaimMain";
      witness = "deadOwnerReapedW";
    };

    # ── gcCollectState: the collect cycle's COMMIT BASIS law
    # (bughunt-2 slot 7, bug_226; DurableObservation/CycleCommit in
    # collect.rs). Standalone on purpose: the basis law is a per-commit
    # functional property — modeling it inside chunkCollect would fan a
    # new variable through every frame discipline for no interleaving
    # value. The committed gc_collect_state row anchors the REAL basis
    # (live/backlog under NO exclusions at the commit snapshot); the
    # simulated preview (bug_199's lane) is reporting-only.
    # r[verify store.gc.observation-basis+2]
    quint-gc-collect-state = mkQuintCheck {
      name = "gc-collect-state";
      spec = "gcCollectState";
      main = "gcCollectStateMain";
      invariants = [
        "publishedLiveEqualsRealMark"
        "backlogLeTrueEligible"
      ];
    };

    # The operator dry-run anchor run: preview simulates, commit
    # anchors real — the composed mirror of collect.rs's
    # dry_run_commit_anchors_real_basis test.
    # r[verify store.gc.observation-basis+2]
    quint-gc-collect-state-runs = mkQuintRunCheck {
      name = "gc-collect-state-runs";
      spec = "gcCollectState";
      main = "gcCollectStateMain";
    };

    # A commit whose simulated and real bases DIFFER is reachable in
    # the main regime — the calibration twin's falsification below only
    # means something if the live module explores the effective-shadow
    # arm (noEffectiveShadowCommit must violate).
    quint-gc-collect-state-witness-effective-shadow = mkQuintWitnessCheck {
      name = "gc-collect-state-witness-effective-shadow";
      spec = "gcCollectState";
      main = "gcCollectStateMain";
      witness = "noEffectiveShadowCommit";
    };

    # CALIBRATION (expect-violation): the pre-fix commit publishes the
    # SIMULATED basis — the live-count law falsifies.
    quint-gc-collect-state-calib-sim-basis-live = mkQuintWitnessCheck {
      name = "gc-collect-state-calib-sim-basis-live";
      spec = "gcCollectState";
      main = "gcCollectStateCalibSimBasis";
      witness = "publishedLiveEqualsRealMark";
    };

    # CALIBRATION (expect-violation): same twin, the backlog bound
    # falsifies (the simulated lane can only inflate backlog).
    quint-gc-collect-state-calib-sim-basis-backlog = mkQuintWitnessCheck {
      name = "gc-collect-state-calib-sim-basis-backlog";
      spec = "gcCollectState";
      main = "gcCollectStateCalibSimBasis";
      witness = "backlogLeTrueEligible";
    };

    # ── gcCadence: the backstop's ATTEMPT-CADENCE and COMMIT-GATES-OK
    # laws (bughunt-4 S4: bug_284 + merged_bug_218) plus the bughunt-5
    # S7 COMMIT-CLASSIFICATION axis (merged_bug_022: witness at the
    # durability point, three-valued attribution, own-commit
    # recognition; store.gc.collect-cadence+3). DEDICATED regime per
    # the gcDrainPass precedent: the time axis must not multiply the
    # other gc regimes.
    # Measured 2026-06-09 (TLC exhaustive, INTERVAL=4 / MAX_TIME=12,
    # workers=4, axis ON): 74,869 generated / 12,506 distinct, [ok] in
    # ~5s — the default 1800s budget is >300x the measurement.
    # Byte-identity probe of the LEGACY lane (COMMIT_CLASSIFY=false,
    # the pre-extension main bindings, F16 convention): 4,347
    # generated / 1,926 distinct — EXACTLY the pre-extension record,
    # so the two axis-off calibration regimes below are
    # state-space-identical to their pre-extension selves (the round-5
    # vars evolve as pure functions of the legacy trajectory).
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence = mkQuintCheck {
      name = "gc-cadence";
      spec = "gcCadence";
      main = "gcCadenceMain";
      invariants = [
        "attemptCadenceBounded"
        "okTicksRequireCommit"
        "attributionPartition"
        "noFalseLost"
      ];
    };

    # The bug_284 red, deterministically: two checks inside one
    # interval run exactly ONE heavy cycle (the second is throttled by
    # the attempt stamp) and the success stamp stays unwritten.
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence-runs = mkQuintRunCheck {
      name = "gc-cadence-runs";
      spec = "gcCadence";
      main = "gcCadenceMain";
    };

    # Non-vacuity witnesses (must VIOLATE in the main regime): the
    # fail-closed abort arm and the lost-commit arm are both reachable
    # — the two laws police paths the regime actually explores.
    quint-gc-cadence-witness-abort = mkQuintWitnessCheck {
      name = "gc-cadence-witness-abort";
      spec = "gcCadence";
      main = "gcCadenceMain";
      witness = "noAbortedAttempt";
    };
    quint-gc-cadence-witness-lost-commit = mkQuintWitnessCheck {
      name = "gc-cadence-witness-lost-commit";
      spec = "gcCadence";
      main = "gcCadenceMain";
      witness = "noLostCommit";
    };

    # Non-vacuity witnesses for the round-5 classification axis (must
    # VIOLATE in the main regime): the applied-but-response-lost
    # RECOGNITION trace and the proven-foreign-winner trace are both
    # reachable — noFalseLost polices paths the regime actually
    # explores. Measured: [violation] in <1s each, TLC.
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence-witness-response-lost-recognized = mkQuintWitnessCheck {
      name = "gc-cadence-witness-response-lost-recognized";
      spec = "gcCadence";
      main = "gcCadenceMain";
      witness = "noRecognizedResponseLost";
    };
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence-witness-proven-foreign = mkQuintWitnessCheck {
      name = "gc-cadence-witness-proven-foreign";
      spec = "gcCadence";
      main = "gcCadenceMain";
      witness = "noProvenForeignWinner";
    };

    # CALIBRATION (expect-violation, merged_bug_022): the as-built
    # 0-row retry collapse — every 0-row/errored retry unconditionally
    # claims "another holder committed first", so an
    # applied-but-response-lost commit (the row carries OUR OWN write
    # at expected+1) is reported lost; noFalseLost falsifies. Same
    # regime constants as the main regime. Measured: [violation] in
    # <1s, TLC.
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence-calib-zero-rows-foreign = mkQuintWitnessCheck {
      name = "gc-cadence-calib-zero-rows-foreign";
      spec = "gcCadence";
      main = "gcCadenceCalibZeroRowsClaimsForeign";
      witness = "noFalseLost";
    };

    # CALIBRATION (expect-violation, merged_bug_022): the as-built
    # post-commit release-? shape — the witness minted AFTER the
    # release instead of at the durability point, so a landed commit
    # whose lock release fails is reported lost; noFalseLost
    # falsifies. Same regime constants as the main regime. Measured:
    # [violation] in <1s, TLC.
    # r[verify store.gc.collect-cadence+3]
    quint-gc-cadence-calib-release-fails = mkQuintWitnessCheck {
      name = "gc-cadence-calib-release-fails";
      spec = "gcCadence";
      main = "gcCadenceCalibReleaseFailsCommit";
      witness = "noFalseLost";
    };

    # CALIBRATION (expect-violation): the pre-fix due predicate —
    # success-stamp staleness only, no attempt gate. A persistent
    # fail-closed abort re-runs the heavy cycle on every check tick;
    # the cadence law falsifies.
    quint-gc-cadence-calib-hourly-retry = mkQuintWitnessCheck {
      name = "gc-cadence-calib-hourly-retry";
      spec = "gcCadence";
      main = "gcCadenceCalibHourlyRetry";
      witness = "attemptCadenceBounded";
    };

    # CALIBRATION (expect-violation): the pre-fix tick placement — ok
    # ticked when the cycle drains, before the commit lands; the
    # commit-gates-ok law falsifies on the first lost commit.
    quint-gc-cadence-calib-tick-before-commit = mkQuintWitnessCheck {
      name = "gc-cadence-calib-tick-before-commit";
      spec = "gcCadence";
      main = "gcCadenceCalibTickBeforeCommit";
      witness = "okTicksRequireCommit";
    };

    # ── gcDrainPass: the collect pass's COMPLETION law (round 3,
    # bug_174 + bug_137 + merged_bug_170 — the PassDisposition type:
    # only CompleteFullScan anchors the cycle; a resumed pass keeps the
    # decremented backlog because keys at or below its resume point
    # were never scanned by it). DEDICATED regime per the S7 precedent:
    # the cursor/disposition axis must not multiply the commit-basis
    # regime's state space. Measured (TLC exhaustive, MAX_CHUNKS=4,
    # workers=4): 41,427 generated / 4,260 distinct, [ok] in <1s — the
    # default 1800s budget is ~1800x the measurement.
    # r[verify store.gc.completion-witness+2]
    quint-gc-drain-pass = mkQuintCheck {
      name = "gc-drain-pass";
      spec = "gcCollectState";
      main = "gcDrainPassMain";
      invariants = [
        "zeroAnchorTruthful"
        "anchoredByFullScanOnly"
      ];
    };

    # bug_174's counterexample choreography against the REAL wiring:
    # reap, cap, below-cursor re-eligibility, resume, drain the tail —
    # the resumed completion mints DCompleteResumed, anchors nothing,
    # and the below-cursor eligible survives for the next full pass.
    # r[verify store.gc.completion-witness+2]
    quint-gc-drain-pass-runs = mkQuintRunCheck {
      name = "gc-drain-pass-runs";
      spec = "gcCollectState";
      main = "gcDrainPassMain";
    };

    # A resumed completion sitting above below-resume-point eligibles
    # is reachable in the main regime (the twin's falsification below
    # only means something if the live regime explores that shape).
    # Measured: [violation] in <1s, TLC exhaustive.
    quint-gc-drain-pass-witness-resumed-below = mkQuintWitnessCheck {
      name = "gc-drain-pass-witness-resumed-below";
      spec = "gcCollectState";
      main = "gcDrainPassMain";
      witness = "noResumedCompletionWithBelowEligibles";
    };

    # CALIBRATION (expect-violation): the pre-fix wiring — a resumed
    # completion also anchors, from the keys IT scanned. The
    # truthfulness law falsifies (zero-backlog anchor over unseen
    # below-cursor eligibles). Measured: [violation] in <1s, TLC.
    quint-gc-drain-pass-calib-resume-anchors-zero = mkQuintWitnessCheck {
      name = "gc-drain-pass-calib-resume-anchors-zero";
      spec = "gcCollectState";
      main = "gcDrainPassCalibResumeAnchors";
      witness = "zeroAnchorTruthful";
    };

    # CALIBRATION (expect-violation): same twin, the minter law — the
    # anchor minted by a non-full scan. Measured: [violation] in <1s.
    quint-gc-drain-pass-calib-resume-anchors-minter = mkQuintWitnessCheck {
      name = "gc-drain-pass-calib-resume-anchors-minter";
      spec = "gcCollectState";
      main = "gcDrainPassCalibResumeAnchors";
      witness = "anchoredByFullScanOnly";
    };

    # ── gcCoordination: the cluster-scoped collect cadence and gauge
    # publication over the durable gc_collect_state row (bughunt wave
    # D1, bug_174 + merged_bug_211; migration 090). Two replicas, a
    # bounded DB clock, cycles as atomic stamped events under the
    # advisory lock; the backstop's due predicate reads
    # last_live_cycle_at on the DB clock (GcCycleLease::backstop_due —
    # the unlocked pre-check and the under-lock double-check collapse
    # onto one predicate); shadow commits stamp a fresh estimate
    # WITHOUT answering the cadence question; every replica publishes
    # its gauges from a 60s row read (spawn_gc_gauge_publisher).
    # r[verify store.gc.collect-cadence+3]
    quint-gc-coordination-main = mkQuintCheck {
      name = "gc-coordination-main";
      spec = "chunkCollect";
      main = "gcCoordinationMain";
      invariants = [
        "gcBoundsOK"
        "cadenceBound"
        "publishedFromDurableOnly"
      ];
    };

    # Falsification twin: the pre-fix per-replica interval_at timers —
    # each replica's due predicate consults only its own local stamp,
    # so the second replica fires immediately after the first's live
    # cycle; cadenceBound violates. bug_174's machine-checked
    # counterexample (N heavy cycles/day at KEDA scale, mutual
    # exclusion without rate limiting).
    quint-gc-coordination-falsify-local-timers = mkQuintWitnessCheck {
      name = "gc-coordination-falsify-local-timers";
      spec = "chunkCollect";
      main = "gcCoordinationLocalTimers";
      witness = "cadenceBound";
    };

    # Convergence witness on the main regime: a replica OTHER than the
    # committer of the current estimate publishes exactly that estimate
    # — merged_bug_211's convergence, unreachable pre-fix (the dry-run
    # anchor lived in the winning pod's process statics; every other
    # replica's gauge sat frozen at its pre-registered zero forever).
    quint-gc-coordination-witness-remote-publish = mkQuintWitnessCheck {
      name = "gc-coordination-witness-remote-publish";
      spec = "chunkCollect";
      main = "gcCoordinationMain";
      witness = "remoteReplicaPublishesPostCycleEstimate";
    };

    # ── shadow equivalence (bughunt wave D1, bug_199): the dry run's
    # chunk estimate must equal the live sweep-then-collect estimate at
    # the same snapshot for EVERY sweepable subset — the shadow_swept
    # anti-join IS the post-sweep simulation (modulo the per-cycle
    # victim cap, which is scheduling above the eligible-set equality).
    # r[verify store.gc.dry-run+3]
    quint-chunk-collect-shadow-equivalence = mkQuintCheck {
      name = "chunk-collect-shadow-equivalence";
      spec = "chunkCollect";
      main = "chunkCollectShadowEquivalence";
      invariants = [
        "boundsOK"
        "shadowEstimateMatchesLive"
      ];
    };

    # Falsification twin: the pre-fix estimate (no exclusion — the
    # savepoint-rolled-back sweep left the would-be-swept manifests in
    # the mark, so their chunks counted as live): a 'complete' manifest
    # exclusively referencing a past-grace chunk makes the dry run
    # report zero where the live run collects — structurally zero for
    # the dominant term of the estimate.
    quint-chunk-collect-falsify-shadow-no-exclusion = mkQuintWitnessCheck {
      name = "chunk-collect-falsify-shadow-no-exclusion";
      spec = "chunkCollect";
      main = "chunkCollectShadowNoExclusion";
      witness = "shadowEstimateMatchesLive";
    };

    # The late-mark holds-half (Phase-1 input list item 1): under a
    # relaxed heartbeat contract (a live owner may stall past the
    # reclaim thresholds), the T-pre.1 `AND deleted = FALSE` guard on
    # mark_chunks_uploaded keeps CR-1 standing. The invariant list is
    # deliberately CR-1 plus the structural/ownership set: the
    # relaxation itself makes S5 violable and opens benign bookkeeping
    # windows (orphan re-PUT, transient uploaded_at-vs-object skew)
    # with no data-loss content — the regime module's comment carries
    # the full rationale. The unguarded falsify-half is wired below.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.cas.chunk-upload-committed]
    quint-chunk-collect-latemark-guarded = mkQuintCheck {
      name = "chunk-collect-latemark-guarded";
      spec = "chunkCollect";
      main = "chunkCollectLateMarkGuarded";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "s4OwnerOnlyMutation"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The deterministic reproducer runs, one check per regime: the
    # collector happy path and the capped-cycle/backstop resume shape,
    # the crashed-upload-collected-one-cycle-later shape, the
    # mark-stale touch-protection walkthrough, and the fail-closed
    # abort + quarantine-then-resume narratives are replayed step by
    # step with their expectations re-asserted.
    quint-chunk-collect-runs-base = mkQuintRunCheck {
      name = "chunk-collect-runs-base";
      spec = "chunkCollect";
      main = "chunkCollectBase";
    };
    quint-chunk-collect-runs-crash = mkQuintRunCheck {
      name = "chunk-collect-runs-crash";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
    };
    quint-chunk-collect-runs-contend = mkQuintRunCheck {
      name = "chunk-collect-runs-contend";
      spec = "chunkCollect";
      main = "chunkCollectContend";
    };
    quint-chunk-collect-runs-corrupt = mkQuintRunCheck {
      name = "chunk-collect-runs-corrupt";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
    };

    # CALIBRATION (composed, expect-the-violation-in-run): the reap
    # DELETE without the outer-qual row-local splice (merged_bug_026
    # pre-fix). reapEpqRaceRun drives the exact two-connection race —
    # victim picked at the IN-subquery snapshot, resurrected by a
    # concurrent PutPath upsert, hard-deleted anyway because the EPQ
    # recheck finds no row-local conjunct in the outer qual — and
    # expects reapSafety violated. If a refactor makes the run fail,
    # the reapOne guard has stopped modeling the splice.
    quint-chunk-collect-calib-reap-epq-outer = mkQuintRunCheck {
      name = "chunk-collect-calib-reap-epq-outer";
      spec = "chunkCollect";
      main = "chunkCollectReapEpqOuterOnly";
      match = "reapEpqRaceRun";
    };

    # Non-vacuity witnesses for the chunkCollect regimes. Each check
    # passes only when the checker violates its witness — the contended
    # state the regime's invariants constrain is actually reachable.
    # Deliberately no tracey markers (same policy as every witness
    # check).

    # The base regime's headline states: a complete chunked upload
    # exists, a backend delete fires, a referenced-but-unconfirmed
    # chunk exists (the M_033 precondition restated over the fold), the
    # heartbeat actually resets staleness, and a collect batch actually
    # collects something.
    quint-chunk-collect-witness-complete-upload = mkQuintWitnessCheck {
      name = "chunk-collect-witness-complete-upload";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noCompleteUpload";
    };
    quint-chunk-collect-witness-backend-delete = mkQuintWitnessCheck {
      name = "chunk-collect-witness-backend-delete";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noBackendDelete";
    };
    quint-chunk-collect-witness-m033-precondition = mkQuintWitnessCheck {
      name = "chunk-collect-witness-m033-precondition";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noUnconfirmedReferencedChunk";
    };
    quint-chunk-collect-witness-heartbeat-reset = mkQuintWitnessCheck {
      name = "chunk-collect-witness-heartbeat-reset";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noHeartbeatReset";
    };
    quint-chunk-collect-witness-chunk-collected = mkQuintWitnessCheck {
      name = "chunk-collect-witness-chunk-collected";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noChunkCollected";
    };
    # The post-pass reap actually hard-deletes a tombstone (bughunt
    # wave D1, merged_bug_336's liveness witness — the pre-fix model
    # had no chunks-row delete action at all; without this state
    # reapSafety holds vacuously).
    quint-chunk-collect-witness-tombstone-reaped = mkQuintWitnessCheck {
      name = "chunk-collect-witness-tombstone-reaped";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noTombstoneReaped";
    };

    # The crash regime's fault alphabet is reachable: the C1/C2/C5
    # crash windows, the two-writers-staged-then-crashed C6 shape, the
    # abandoned-upload garbage those windows leave for the collector,
    # and both stale-reclaim repair paths firing.
    quint-chunk-collect-witness-crash-claimed = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-claimed";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashAtClaimed";
    };
    quint-chunk-collect-witness-crash-upgraded = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-upgraded";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashAfterUpgrade";
    };
    quint-chunk-collect-witness-crash-pending-reap = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-pending-reap";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashBeforeReap";
    };
    quint-chunk-collect-witness-double-crash-staged = mkQuintWitnessCheck {
      name = "chunk-collect-witness-double-crash-staged";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noDoubleCrashStaged";
    };
    quint-chunk-collect-witness-abandoned-upload = mkQuintWitnessCheck {
      name = "chunk-collect-witness-abandoned-upload";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noAbandonedUploadGarbage";
    };
    quint-chunk-collect-witness-hotpath-reclaim = mkQuintWitnessCheck {
      name = "chunk-collect-witness-hotpath-reclaim";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noHotpathReclaim";
    };
    quint-chunk-collect-witness-scanner-reap = mkQuintWitnessCheck {
      name = "chunk-collect-witness-scanner-reap";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noScannerReap";
    };

    # The contend regime's contended states: the §4.6 mark-stale
    # reachability witness (a post-snapshot upgrade re-references an
    # unmarked, past-grace chunk and only the touch retains it while a
    # sweep batch runs — the race the new column exists to close), the
    # drain re-check skipping a resurrected chunk under the
    # deleted-only re-check, and a claim-gated cleanup no-opping
    # against a foreign row.
    quint-chunk-collect-witness-mark-miss-touch-saved = mkQuintWitnessCheck {
      name = "chunk-collect-witness-mark-miss-touch-saved";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noMarkMissSavedByTouch";
    };
    quint-chunk-collect-witness-drain-resurrect = mkQuintWitnessCheck {
      name = "chunk-collect-witness-drain-resurrect";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noDrainResurrectSkip";
    };
    quint-chunk-collect-witness-late-cleanup-noop = mkQuintWitnessCheck {
      name = "chunk-collect-witness-late-cleanup-noop";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noLateCleanupNoop";
    };

    # The corrupt regime's reachability set: the fail-closed abort
    # actually fires (the alert image), the adjudication-7 quarantine
    # fires, and CR-2's unconditional structural form is — exactly as
    # the design owns in §4.4 — unsatisfiable while a corrupt manifest
    # coexists with collectable garbage (the fail-closed pause made
    # visible; the carved form is the regime's invariant above).
    quint-chunk-collect-witness-parse-abort = mkQuintWitnessCheck {
      name = "chunk-collect-witness-parse-abort";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "noParseFailureAbort";
    };
    quint-chunk-collect-witness-quarantine = mkQuintWitnessCheck {
      name = "chunk-collect-witness-quarantine";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "noQuarantine";
    };
    quint-chunk-collect-corrupt-pause-stranded = mkQuintWitnessCheck {
      name = "chunk-collect-corrupt-pause-stranded";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "cr2NoStrandedGarbage";
    };

    # The threshold-ordering inversion, carried over from the as-built
    # model: with the hot-path reclaim threshold lowered to the
    # heartbeat deadline, reaping a live owner becomes reachable and S5
    # falsifies — the path-row janitors keep the ordering dependence
    # under the replacement.
    quint-chunk-collect-threshold-order = mkQuintWitnessCheck {
      name = "chunk-collect-threshold-order";
      spec = "chunkCollect";
      main = "chunkCollectThresholdOrder";
      witness = "s5LiveOwnerNeverReaped";
    };

    # The §4.6 falsification pairs, falsify-halves: each check
    # instantiates the SAME transition relation with exactly one design
    # guard flipped off and passes only while the checker still
    # falsifies CR-1 — machine-checked evidence that each replacement
    # mechanism is load-bearing, not decorative. The matching
    # holds-halves are the wired exhaustive checks above (contend for
    # the touch, corrupt for fail-closed, writer-bounded for the
    # transaction bound, latemark-guarded for the deleted-guard).

    # Touch removed (ENABLE_TOUCH = false; design §4.6 (ii), the
    # required falsification for the one mechanism with no historical
    # fix): a manifest commits after the mark snapshot against an old
    # uploaded chunk, the writer skips the PUT, the sweep collects the
    # chunk and the drain deletes the only copy.
    quint-chunk-collect-no-touch-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-no-touch-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectNoTouch";
      witness = "cr1NoLiveChunkCollected";
    };

    # Writer-transaction bound relaxed by one tick past grace
    # (WRITER_TX_BOUND = GRACE + 1): an upgrade transaction that
    # outlives the grace window leaves its backdated touch before the
    # cycle cutoff, so the post-commit re-evaluation still collects the
    # chunk the just-committed manifest references — the §4.1
    # collect-soundness condition, exercised.
    quint-chunk-collect-writer-overrun-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-writer-overrun-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectWriterOverrun";
      witness = "cr1NoLiveChunkCollected";
    };

    # Fail-closed rule removed (ENABLE_FAIL_CLOSED = false): the mark
    # silently skips an unparseable manifest instead of aborting — the
    # forbidden C12 polarity flip — and a chunk whose only referrer is
    # the corrupt manifest is collected and drained while that manifest
    # exists (design §4.4 / adjudication 5).
    quint-chunk-collect-parse-skip-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-parse-skip-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectParseSkip";
      witness = "cr1NoLiveChunkCollected";
    };

    # Late-mark guard removed under the relaxed heartbeat contract
    # (ENABLE_MARK_DELETED_GUARD = false): a stalled owner's late
    # presence commit re-asserts uploaded_at on a collected chunk and
    # the next writer trusts it — the M_033 harm shape with no counter
    # involved (Phase-1 input list item 1 / plan T-pre.1).
    quint-chunk-collect-latemark-unguarded-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-latemark-unguarded-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectLateMarkUnguarded";
      witness = "cr1NoLiveChunkCollected";
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
    # r[verify store.log.append-auth+2]
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

    # ==================================================================
    # rio-controller's reconcile protocols (controller-formal Phase 0,
    # Stage B): two tick-level models of the two reconcile loops.
    #
    #   spawnCoherence.qnt (Model J) — the L1 Pool reconciler's
    #   same-tick coherence protocol (pool/{jobs,job}.rs): the intent
    #   poll, the 3-valued placeable gate x pool kind, the Job
    #   LIST/census, the stale/excess/orphan reap arms with their
    #   fail-closed gates, the headroom arithmetic, the 409-deduped
    #   spawn pass and the dispatched_cells-arming ack. Six
    #   configurations: base / fault-rpc / fault-lease / fault-stale on
    #   the production Builder+CRD shape, plus the crd-absent and
    #   Fetcher-pool postures (the C1/C2 adjudications in
    #   docs/spec/models/controller-invariant-map.md).
    #
    #   nodeclaimLifecycle.qnt (Model N) — the L10 NodeClaim-pool
    #   reconciler's mirror lifecycle (nodeclaim_pool/): the lease-edge
    #   polarity table (prev_idle / recorded_boot / inflight_created /
    #   the sketches reload latch), the ⊥-streak early-return and
    #   consolidate-only modes, vanish detection vs the controller's own
    #   reaps, the recency-gated Registered clears, the placeable-gate
    #   producer guarantee, and the global / per-class / per-tick cover
    #   budgets over the hw-class config mirror. Four regimes: base /
    #   fault-rpc / fault-lease / fault-karpenter.
    #
    # The Stage-A invariant map (controller-invariant-map.md) is the
    # rule <-> invariant ledger for both models; its Stage-B section
    # records the per-regime verdicts. The two pre-registered as-built
    # falsifications (the ⊥-tick early-return observation skip) are
    # wired as expect-violation checks plus a named-run check below —
    # they stay green only while the documented defect reproduces, and
    # flip to HOLD checks when the fix lands. State counts, depths and
    # wall-clocks live in the introducing commit messages and the
    # checks' transcripts.
    # ==================================================================

    # ---- Model J: exhaustive regime checks ---------------------------

    # Production shape (Builder pool, CRD present), no faults: the
    # healthy spawn/reap/ack lifecycle against an environment that moves
    # between ticks, including the informer-lagged Job.status.ready vs
    # the live pod phase and arbitrary (stale-allowed) gate publishes.
    # r[verify ctrl.pool.tick-ordering]
    # r[verify ctrl.pool.ack-spawned-soundness]
    quint-spawn-coherence-base = mkQuintCheck {
      name = "spawn-coherence-base";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # Per-RPC ⊥ faults (GetSpawnIntents / ListExecutors / the ack / the
    # Job create): the per-consumer degradation matrix — spawn fail-open,
    # stale+excess reap fail-closed, orphan reap behind its 3-arm gate,
    # nothing acked on a failed poll.
    # r[verify ctrl.pool.degraded-polarity]
    quint-spawn-coherence-fault-rpc = mkQuintCheck {
      name = "spawn-coherence-fault-rpc";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultRpc";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # Failover faults: the L10 producer unarms the gate (lease loss) and
    # the scheduler restarts (executor map + dispatched_cells empty,
    # leader young) — the re-ack of already-Pending Jobs re-arms
    # dispatched_cells and the orphan gate stays fail-closed against the
    # young leader's partial executor list.
    # r[verify ctrl.pool.ack-spawned-soundness]
    quint-spawn-coherence-fault-lease = mkQuintCheck {
      name = "spawn-coherence-fault-lease";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultLease";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # Per-read snapshot staleness: the intent poll may read the previous
    # tick's queue while the Job LIST reads the current state — the
    # I-183 incoherence direction the channels allow. The reap arms stay
    # safe under it (the grace windows and membership reaps bound the
    # damage to churn).
    # r[verify ctrl.pool.tick-ordering]
    quint-spawn-coherence-fault-stale = mkQuintCheck {
      name = "spawn-coherence-fault-stale";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultStale";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # CRD-absent configuration (static-node / k3s without Karpenter):
    # gate_armed is false every tick — Builder spawns pass through
    # unfiltered (fail-open) while the excess reap stays suppressed
    # (fail-closed), the C1 adjudication of the placeable-gate rule.
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    quint-spawn-coherence-crd-absent = mkQuintCheck {
      name = "spawn-coherence-crd-absent";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # Fetcher pool with the CRD present: never spawn-filtered, excess
    # reap keyed only on scheduler reachability — the C2 adjudication.
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    quint-spawn-coherence-fetcher = mkQuintCheck {
      name = "spawn-coherence-fetcher";
      spec = "spawnCoherence";
      main = "spawnCoherenceFetcher";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };

    # ---- Model J: non-vacuity witnesses ------------------------------
    # Each passes only when the checker violates the witness — the
    # contended scenarios the invariants constrain stay reachable.
    # Deliberately no tracey markers (same policy as the other models'
    # witnesses).

    # An excess-pending reap actually fires.
    quint-spawn-coherence-witness-excess-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-excess-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachExcessReap";
    };
    # An orphan-running reap actually fires (3-arm gate passed).
    quint-spawn-coherence-witness-orphan-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-orphan-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachOrphanReap";
    };
    # A 409 dedupe occurs (the deterministic-name collision path).
    quint-spawn-coherence-witness-409 = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-409";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReach409Dedupe";
    };
    # An unarmed gate blocks a spawn an armed gate would have made.
    quint-spawn-coherence-witness-gate-blocked = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-gate-blocked";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachGateBlockedSpawn";
    };
    # A stale-selector (fingerprint drift) reap fires.
    quint-spawn-coherence-witness-drift-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-drift-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachDriftReap";
    };
    # crd-absent: a Builder spawn proceeds ungated (fail-open half).
    quint-spawn-coherence-witness-ungated-spawn = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-ungated-spawn";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      witness = "canReachUngatedSpawn";
    };
    # crd-absent: an excess-pending surplus is left unreaped (fail-closed
    # half — the documented C1 operational cost).
    quint-spawn-coherence-witness-suppressed-excess = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-suppressed-excess";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      witness = "canReachSuppressedExcess";
    };

    # ---- Model N: exhaustive regime checks ---------------------------

    # Healthy lifecycle, no faults: create -> register -> busy/idle ->
    # idle-reap with the FFD reservation respected, the per-class clamp
    # over the config mirror, and the placeable publish.
    # r[verify ctrl.nodeclaim.budget.per-class+2]
    quint-nodeclaim-lifecycle-base = mkQuintCheck {
      name = "nodeclaim-lifecycle-base";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # Per-RPC ⊥ faults: every leader tick during an outage runs the
    # shared kube-only observation block (the 2026-06-02 ⊥-arm fix) —
    # pre-threshold ticks observe-and-return, consolidate-only mode
    # reaps + prunes; no create / republish / ack on either. Includes
    # idleReapSafety and bootSampleNotLost as HOLD invariants: their
    # pre-fix falsification was the corpus's only pre-registered
    # as-built defect, flipped per the invariant map's protocol when
    # the skip was fixed.
    # r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
    # r[verify ctrl.nodeclaim.budget.per-class+2]
    quint-nodeclaim-lifecycle-fault-rpc = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-rpc";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      invariants = [
        "boundsOK"
        "iceMarkSoundness"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
        "idleReapSafety"
        "bootSampleNotLost"
      ];
    };

    # Lease faults: lose/acquire edges, PG reload failures and the
    # controller restart — the per-field lease-edge polarity table (the
    # unconditional prev_idle clear, the edge-owned latched-buffer
    # clears with the recorded_boot re-arm staying on the Ok arm
    # [merged_bug_004], the reload latch gating persist) and the
    # producer-side gate guarantee (unarmed on loss before the
    # consumer's next tick).
    # r[verify ctrl.nodeclaim.lease-edge-polarity+4]
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    # r[verify ctrl.nodeclaim.ice-mark-clear+2]
    quint-nodeclaim-lifecycle-fault-lease = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-lease";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # Karpenter faults: launch failures, the 1s-GC-vs-10s-tick vanish
    # race and spot termination edges — NodeClaim conservation (the
    # controller's own reaps are removed from inflight_created before
    # detect_vanished, so an ICE mark is only ever emitted for a claim
    # that genuinely vanished or launch-failed).
    # r[verify ctrl.nodeclaim.inflight-conservation+3]
    # r[verify ctrl.nodeclaim.ice-mark-clear+2]
    quint-nodeclaim-lifecycle-fault-karpenter = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-karpenter";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # The deterministic ⊥-window reproducer traces, post-fix
    # (idlePruneAcrossBotRun, bootRecordedAcrossBotRun): the documented
    # conflation and boot-loss traces now HOLD end-to-end through the
    # fixed pre-threshold observation arm — kept as named runs so the
    # precise traces stay pinned alongside the regime check's BFS.
    quint-nodeclaim-runs-fault-rpc = mkQuintRunCheck {
      name = "nodeclaim-runs-fault-rpc";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
    };

    # ---- Model N: non-vacuity witnesses ------------------------------

    # An idle claim is actually reaped (past threshold, unreserved).
    quint-nodeclaim-witness-idle-reap = mkQuintWitnessCheck {
      name = "nodeclaim-witness-idle-reap";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachIdleReap";
    };
    # A deficit cover actually creates a NodeClaim.
    quint-nodeclaim-witness-create = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachCreate";
    };
    # The per-class budget binds while the global budget still has room.
    quint-nodeclaim-witness-class-budget = mkQuintWitnessCheck {
      name = "nodeclaim-witness-class-budget";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachClassBudgetBound";
    };
    # A claim vanishes between ticks (Karpenter fast-GC) and is detected
    # as an ICE mark.
    quint-nodeclaim-witness-vanish = mkQuintWitnessCheck {
      name = "nodeclaim-witness-vanish";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      witness = "canReachVanishMark";
    };
    # An ICE mark is emitted and a recency-gated Registered clear for
    # the same (still-masked) cell is emitted on a later tick.
    quint-nodeclaim-witness-clear-after-mark = mkQuintWitnessCheck {
      name = "nodeclaim-witness-clear-after-mark";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      witness = "canReachClearAfterMark";
    };
    # The ⊥-streak reaches the threshold and consolidate-only reaps
    # something.
    quint-nodeclaim-witness-consolidate-only-reap = mkQuintWitnessCheck {
      name = "nodeclaim-witness-consolidate-only-reap";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachConsolidateOnlyReap";
    };
    # A NodeClaim create fails (and consumes no budget).
    quint-nodeclaim-witness-create-failure = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create-failure";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCreateFailure";
    };
    # A tick with unplaced demand and unknown ceilings performs no
    # creates (the fail-closed half)…
    quint-nodeclaim-witness-ceilings-fail-closed = mkQuintWitnessCheck {
      name = "nodeclaim-witness-ceilings-fail-closed";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCeilingsFailClosed";
    };
    # …and creates resume once the ceilings are present (the resume
    # half, in the same regime whose init starts without them).
    quint-nodeclaim-witness-create-resumes = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create-resumes";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCreate";
    };
    # An intent is dropped solely because its cell is unconfigured.
    quint-nodeclaim-witness-unknown-cell = mkQuintWitnessCheck {
      name = "nodeclaim-witness-unknown-cell";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachUnknownCellDrop";
    };
    # A lease handoff occurs with a non-empty inflight_created.
    quint-nodeclaim-witness-handoff-inflight = mkQuintWitnessCheck {
      name = "nodeclaim-witness-handoff-inflight";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachAcquireWithInflight";
    };
    # The acquire-with-failed-reload path runs a degraded tick (persist
    # gated, latch still set).
    quint-nodeclaim-witness-degraded-reload = mkQuintWitnessCheck {
      name = "nodeclaim-witness-degraded-reload";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachDegradedReloadTick";
    };
    # After an acquire, a fresh registration edge still emits a
    # Registered clear…
    quint-nodeclaim-witness-fresh-clear-after-acquire = mkQuintWitnessCheck {
      name = "nodeclaim-witness-fresh-clear-after-acquire";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachFreshClearAfterAcquire";
    };
    # …while an old registration is recorded without one (the mass-clear
    # the recency gate prevents).
    quint-nodeclaim-witness-stale-record-only = mkQuintWitnessCheck {
      name = "nodeclaim-witness-stale-record-only";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachStaleRecordOnly";
    };

    # ---- Controller Stage-C calibration witnesses --------------------
    # The controller-formal historical-fix corpus replayed against the
    # two reconcile models (controller-invariant-map.md, the Stage-C
    # calibration table). Each check instantiates the as-built model,
    # swaps ONE tick action for its PRE-FIX behavior (the calibration
    # module's `calibStep`) and passes only while the checker still
    # falsifies the invariant the corresponding historical fix protects
    # — machine-checked evidence that the model would re-find that bug
    # class if it were reintroduced, and that the invariant is not
    # vacuous for it. One representative per falsifying family with a
    # plausible regression path; the remaining override modules under
    # docs/spec/models/calibration/ are evidence modules (not wired).
    # Deliberately no tracey markers: the spec rules are verified by the
    # HOLD regime checks above, not by these pre-fix reproductions.

    # G-A (fba9086dc): losing the live pod-phase re-check before the
    # excess-pending DELETE reaps a Job whose pod is already running.
    quint-ctrl-calib-ga-live-recheck = mkQuintWitnessCheck {
      name = "ctrl-calib-ga-live-recheck";
      spec = "calibration/controller-ga";
      main = "gaCalibNoLiveRecheck";
      extraSpecs = [ "spawnCoherence" ];
      step = "calibStep";
      witness = "reapSafety";
    };

    # G-B (cdc78f839): acking the attempted spawn slice instead of the
    # successfully-created set arms dispatched_cells with no Job behind
    # it.
    quint-ctrl-calib-gb-ack-spawned-only = mkQuintWitnessCheck {
      name = "ctrl-calib-gb-ack-spawned-only";
      spec = "calibration/controller-gb";
      main = "gbCalibAckAttempted";
      extraSpecs = [ "spawnCoherence" ];
      step = "calibStep";
      witness = "ackSoundness";
    };

    # M1 (79f86b888): clearing prev_idle only on the reload Ok arm lets
    # a stale entry survive a failed-reload acquire and over-reap a
    # freshly idle claim (the amplify-polarity row of the lease-edge
    # table).
    quint-ctrl-calib-m1-acquire-clear = mkQuintWitnessCheck {
      name = "ctrl-calib-m1-acquire-clear";
      spec = "calibration/controller-m1";
      main = "m1CalibAcquireClearOkOnly";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "idleReapSafety";
    };

    # M2 (08d49c52c): dropping the consolidate-only inflight prune makes
    # the controller's own reap read as a Karpenter vanish on the next
    # full tick — a spurious ICE mark.
    quint-ctrl-calib-m2-consolidate-prune = mkQuintWitnessCheck {
      name = "ctrl-calib-m2-consolidate-prune";
      spec = "calibration/controller-m2";
      main = "m2CalibNoConsolidatePrune";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "iceMarkSoundness";
    };

    # M3/M4 (703cbf42a): clearing the reload latch on the load attempt
    # (not on Ok) lets the same tick persist the stale standby snapshot
    # over PG.
    quint-ctrl-calib-m34-reload-latch = mkQuintWitnessCheck {
      name = "ctrl-calib-m34-reload-latch";
      spec = "calibration/controller-m34";
      main = "m34CalibLatchClearOnAttempt";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "reloadLatchRespected";
    };

    # FFD/cover (family-level): sizing cover against the global budget
    # only lets one cell exceed its per-class fleet cap.
    quint-ctrl-calib-ffd-class-clamp = mkQuintWitnessCheck {
      name = "ctrl-calib-ffd-class-clamp";
      spec = "calibration/controller-ffd";
      main = "ffdCalibNoClassClamp";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "provisioningBudget";
    };

    # ------------------------------------------------------------------
    # Executor-lifecycle campaign (#1), Phase 0 Stage B: the AS-BUILT
    # scheduler⇄executor session protocol.
    #
    # Model S (executorSession.qnt) — RE-TARGETED at 1c' to the pull
    # protocol as landed (T-1c'.5): the PullAssignment admission and
    # fenced mint, the ReportOutcome intake (fold_report + the AD5
    # abort arm), the controller's idempotent ReportAttemptOutcome
    # (second installment / synthesized verdict / no-attempt no-op),
    # the establishment sweep over the durable open-attempt view
    # (store-probe adopt arm + the C2 charge arm), the claims-floor
    # generation fence, and the pod's own bounded lifecycle around
    # them (the OA6(a) NotYetReady wait state, the charge-free idle
    # exit, death, replacement). One intent, bucketed time;
    # budgets/charges are imported from retryPolicy.qnt's pull-mode
    # regime, the spawn/Job lifecycle from spawnCoherence.qnt, the
    # lease from leaderElection.qnt (see the model header's
    # assume-guarantee checklist).
    #
    # The Stage-B AS-BUILT encoding of the stream session machinery was
    # frozen, unwired, as executorSessionAsBuilt.qnt (P14) and retired
    # on 2026-05-29 — the file is deleted, git history is the archive
    # (the invariant map's retirement record). Its retired checks and
    # the disposition of every retired witness/calibration check are
    # recorded in the invariant map's Phase-1c' re-target record.
    #
    # Model D (executorDelivery.qnt) was retired with the 1d builder
    # collapse (T-1d.4): the stream-era delivery choreography it modeled
    # (permanent sink, relay swap, half-close flush, drain gate, idle
    # exit, generation watermark) no longer exists. Its guarantees'
    # carriers are recorded in the invariant map's Model-D retirement
    # record: the pull report-retry loop and exit-code unit batteries
    # (builder.completion.exactly-once-or-death+2 verify markers), the
    # scheduler-side report idempotency tests, the chaos VM suite, and
    # — Phase 2 — the fold_report Kani contract. The model file and its
    # two f2d calibration override modules are deleted (git history
    # holds them); the f2d calibration family's acceptance verdict is
    # pre-staged in the retirement record.
    #
    # The invariant ↔ rule map, the Stage-B record for the frozen
    # as-built model, and the Phase-1c' re-target record (verdicts,
    # bounds, retirements) are in
    # docs/spec/models/executor-invariant-map.md.

    # The base regime: the full pull lifecycle with no leader faults —
    # forecast spawn before Ready, the bounded NotYetReady retry loop
    # and its charge-free idle exit (OA6(a)), delivery and idempotent
    # re-pull, the worker report (success / failure / abort), pod death
    # with and without uploaded outputs, the controller's no-attempt
    # no-op / second installment / synthesized verdict, and both
    # establishment arms. Verifies the re-targeted invariant set:
    # at-most-one-open-attempt, NotYetReady inertness, the re-based
    # repair-armed property, no fabricated completion, the
    # establishment-window discipline and the per-attempt
    # classification dedup.
    # r[verify sched.executor.one-shot+2]
    # r[verify sched.executor.pull-not-ready+2]
    # r[verify sched.attempt.establishment-window+5]
    quint-executor-session-base = mkQuintCheck {
      name = "executor-session-base";
      spec = "executorSession";
      main = "executorSessionBase";
      invariants = [ "allInvariants" ];
    };

    # The fault-leader regime: one leader failover (claim-before-serve
    # floor ratchet) plus below-floor write attempts by a deposed
    # believer. The durable open-attempt state must survive the
    # failover untouched, the post-failover authority keeps serving
    # pulls off the durable rows, and every stale-authority transaction
    # is fenced to a no-op — StaleAuthorityWritesAreInert, the checked
    # successor of the dual-belief residual the as-built model priced
    # (T-0e.2 / the 1c' lease-checklist re-derivation).
    # r[verify sched.lease.generation-fence+3]
    # r[verify sched.lease.claim-before-advertise+2]
    quint-executor-session-fault-leader = mkQuintCheck {
      name = "executor-session-fault-leader";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      invariants = [ "allInvariants" ];
    };

    # Non-vacuity witnesses for the re-targeted Model S. Each check
    # passes only when the contended state is still reachable in the
    # named regime. The first two are the OA6(a) reachability pair the
    # 1c' plan names (the warm-start delivery after NotYetReady, and
    # the never-Ready charge-free idle exit); the rest pin every
    # closer/repair path of the open-attempt row plus the orphaned
    # open attempt and the report-idempotency arms. The retired
    # as-built witnesses (phantom, drain-pending, half-dead,
    # stale-epoch, rollback, adopt, failover-inflight,
    # deposed-believer, reap-after-stall, two-channel-death,
    # establishment, race-ahead) are recorded with their dispositions
    # in the invariant map's Phase-1c' re-target record.
    quint-executor-session-witness-warm-start = mkQuintWitnessCheck {
      name = "executor-session-witness-warm-start";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachWarmStartDeliver";
    };
    quint-executor-session-witness-idle-exit = mkQuintWitnessCheck {
      name = "executor-session-witness-idle-exit";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachNeverReadyIdleExit";
    };
    quint-executor-session-witness-establishment = mkQuintWitnessCheck {
      name = "executor-session-witness-establishment";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachEstablishmentCharge";
    };
    quint-executor-session-witness-store-adopt = mkQuintWitnessCheck {
      name = "executor-session-witness-store-adopt";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachStoreProbeAdopt";
    };
    quint-executor-session-witness-synthesized-close = mkQuintWitnessCheck {
      name = "executor-session-witness-synthesized-close";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachSynthesizedClose";
    };
    quint-executor-session-witness-second-installment = mkQuintWitnessCheck {
      name = "executor-session-witness-second-installment";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachSecondInstallment";
    };
    quint-executor-session-witness-repull = mkQuintWitnessCheck {
      name = "executor-session-witness-repull";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachIdempotentRepull";
    };
    quint-executor-session-witness-orphaned-attempt = mkQuintWitnessCheck {
      name = "executor-session-witness-orphaned-attempt";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachOrphanedOpenAttempt";
    };
    quint-executor-session-witness-late-report = mkQuintWitnessCheck {
      name = "executor-session-witness-late-report";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachLateReportIgnored";
    };
    quint-executor-session-witness-no-attempt-noop = mkQuintWitnessCheck {
      name = "executor-session-witness-no-attempt-noop";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachNoAttemptNoop";
    };
    # The two fault-leader probes: a below-floor authority actually
    # attempts a write and is fenced (StaleAuthorityWritesAreInert is
    # not vacuous), and the post-failover authority serves a pull off
    # the durable rows.
    quint-executor-session-witness-stale-fenced = mkQuintWitnessCheck {
      name = "executor-session-witness-stale-fenced";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      witness = "canReachStaleFenced";
    };
    quint-executor-session-witness-post-failover-deliver = mkQuintWitnessCheck {
      name = "executor-session-witness-post-failover-deliver";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      witness = "canReachPostFailoverDeliver";
    };

    # ---- Executor-lifecycle Stage-C calibration witnesses -------------
    # The executor-lifecycle (campaign #1) historical-fix corpus replayed
    # against Models S and D (executor-invariant-map.md, the Stage-C
    # calibration section). Each check instantiates a model, swaps ONE
    # action for its PRE-FIX behavior (the calibration module's
    # `calibStep`) and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-
    # checked evidence that the model would re-find that bug class if it
    # were reintroduced, and that the invariant is not vacuous for it.
    #
    # 1c' flip (T-1c'.5): the wired Model-S calibration checks tracked
    # the AS-BUILT stream machinery, which the deletion commits removed
    # — the states they pinned are unconstructible by design on the
    # pull path, so they are retired with records (the per-family
    # "cannot recur by construction" verdicts are pre-staged in the
    # invariant map's Phase-1c' re-target record):
    #   - quint-executor-calib-f1-stale-epoch (stream-epoch attribution
    #     — no streams, no epochs; per-unary identity + the generation
    #     fence are the successors),
    #   - quint-executor-calib-f2-phantom-drain (phantom two-strike —
    #     no push channel, so a phantom binding cannot form),
    #   - quint-executor-calib-f3-stall-credit (worker-time reaper
    #     stall credit — no scheduler-side liveness reaper exists; the
    #     Job lifecycle and the establishment window own liveness),
    #   - quint-executor-calib-f5-closed-stream (closed-stream dispatch
    #     exclusion — no scheduler-side placement decision exists).
    # Their override modules stayed under calibration/ as evidence over
    # the frozen executorSessionAsBuilt.qnt until the 2026-05-29
    # as-built retirement deleted both (git history is the archive; the
    # invariant map's retirement record has the dispositions). F4
    # re-encodes against the re-targeted model below;
    # F2d (Model D, builder half) retired with Model D at the 1d builder
    # collapse — the machinery it pinned (completion_pending arming, the
    # half-close flush) no longer exists, so the state is
    # unconstructible by design; verdict pre-staged in the invariant
    # map's retirement record. Deliberately no tracey markers (the spec
    # rules are verified by the HOLD regime checks above, not by these
    # pre-fix reproductions).

    # F4 (death attribution / the I-197 double-charge precondition),
    # re-encoded against the re-targeted model: losing the
    # establishment-window gate lets the establishment sweep charge an
    # open attempt while its classifying report still has its window —
    # the pull-path analog of the correlation-entry/last_completed
    # discriminator the as-built representative pinned.
    quint-executor-calib-f4-establishment-window = mkQuintWitnessCheck {
      name = "executor-calib-f4-establishment-window";
      spec = "calibration/executor-f4-pull-establish-early";
      main = "executorCalibF4PullEstablishEarly";
      extraSpecs = [ "executorSession" ];
      step = "calibStep";
      witness = "establishmentOnlyAfterWindowCloses";
    };

    # ------------------------------------------------------------------
    # Executor-lifecycle campaign: the retryPolicy PULL-MODE environment
    # regime (the T-0e.3 re-derivation, wired at 1b as an additional
    # regime; the wired set since the 1c' retirement of the
    # as-built-channel regimes, T-1c'.6). Same fold, same invariants as
    # the retired channel regimes — what changes is the event-arrival
    # environment: the attempt opens at PullAssignment, the worker
    # classes arrive over the ReportOutcome unary, the controller's
    # pod-terminal classification is the idempotent ReportAttemptOutcome
    # row fill (with the no-attempt no-op), the establishment sweep is
    # the only time-based repair, and the exclusion/fleet-exhaust inputs
    # are re-keyed to source nodes with the AD2 small-fleet clause (the
    # spawn-intent gate's NoEligibleSource arm).
    #
    # The regime module is retryPolicyPull in retryPolicy.qnt; its
    # transition relation is `pullStep`, selected via --step.
    # ------------------------------------------------------------------

    # The exhaustive pull-mode regime check: two source nodes, the full
    # pull alphabet (pull-open, worker-report classes, pod deaths with
    # the OOM/Deadline controller fills, the no-attempt no-op, the
    # establishment, the spawn-gate exhaust, source-universe shrink, the
    # resets) plus one leader failover with the open-attempt carve-out
    # (the pull attempt and the durable budgets survive; nothing is
    # forgiven or fabricated). The HOLD list is the same invariant set
    # the as-built regimes proved — the refinement tripwires, the
    # charge-once/no-double-count discipline, the poison/clear
    # lifecycle, and the failover-budget acceptance property — now over
    # the pull-path event-arrival environment. Since the 1c' retirement
    # this check also carries the model-checked markers the retired
    # regime checks held (the rules' unit-test and kani markers are
    # unchanged).
    # r[verify sched.retry.per-executor-budget+4]
    # r[verify sched.dispatch.fleet-exhaust+5]
    # r[verify sched.retry.counters-refine-history+2]
    # r[verify sched.retry.no-double-count]
    # r[verify sched.retry.verdict-channel-invariant]
    # r[verify sched.poison.cascade-dependents]
    # r[verify sched.retry.failover-budget]
    # r[verify sched.retry.recovery-projection+3]
    quint-retry-policy-pull = mkQuintCheck {
      name = "retry-policy-pull";
      # quint-policy P1 exemptions (bughunt-2 slot 11; §5-Q13 — the
      # retryPolicy-15 burn-down headline):
      vacuityExempt = {
        countersRefineHistory = {
          class = "pre-r2-untwinned";
          reason = "refinement-map invariant; the falsifier is a history-divergence twin over the spec/live pairing — Q13 headline (retryPolicy)";
        };
        noDoubleCount = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs a double-charge injection action on the exec ledger — Q13 headline (retryPolicy)";
        };
      };
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
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
        # A2 (bug_279): the uncharged worker-abort run never exceeds
        # the kernel bound (admit_worker_abort's gate; witness
        # quint-retry-policy-pull-witness-free-close-bound keeps the
        # contended edge reachable).
        "boundedFreeRequeues"
      ];
    };

    # Non-vacuity witnesses for the pull-mode regime (same
    # expect-violation discipline as the other witness checks; no tracey
    # markers on witnesses). The four scenarios the 1b gate names plus
    # the controller-fill charge: a pull opens an attempt; a no-report
    # crash is established and charged; a pod-terminal report that finds
    # no open attempt is dropped charge-free (the
    # sched.attempt.no-attempt-no-op arm); the exhausted source universe
    # poisons FleetExhausted (the AD2 small-fleet clause / spawn-gate
    # arm); and the ReportAttemptOutcome fill actually charges (the
    # C4/C5-unified pod-terminal second installment feeds the fold).
    quint-retry-policy-pull-witness-opens-attempt = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-opens-attempt";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noPullOpensAttempt";
    };
    quint-retry-policy-pull-witness-establishment = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-establishment";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noEstablishedCrashCharge";
    };
    quint-retry-policy-pull-witness-no-attempt-noop = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-no-attempt-noop";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noNoAttemptReportNoop";
    };
    quint-retry-policy-pull-witness-fleet-exhaust = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-fleet-exhaust";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noFleetExhaustPoison";
    };
    quint-retry-policy-pull-witness-fill-charge = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-fill-charge";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noAtCapTermination";
    };
    # A2 (bug_279): the free-close run actually reaches the kernel
    # bound — boundedFreeRequeues' contended edge is reachable (three
    # consecutive uncharged worker aborts).
    quint-retry-policy-pull-witness-free-close-bound = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-free-close-bound";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "canReachFreeCloseBound";
    };

    # ------------------------------------------------------------------
    # B1 bounded-await-transport (bughunt wave, bug_408): the
    # store-degraded paced-requeue class
    # (sched.retry.store-degraded-uncharged; the attempts-bounded+5
    # pacing carve-out). The contract triple — never a poison input,
    # never a budget draw, never an exclusion key — holds exhaustively
    # with the action enabled; every other regime binds
    # ENABLE_STORE_DEGRADED = false and stays bit-identical. The
    # falsifiability pair is the retry-408-sd-as-infra calibration (the
    # pre-fix class-blind fold: the recorded red marched 11 flagged
    # reports to Poisoned); the reachability pin is
    # correlatedStoreOutageRun (the fleet-correlated outage).
    # ------------------------------------------------------------------
    # r[verify sched.retry.store-degraded-uncharged+4]
    quint-retry-policy-pull-store-degraded = mkQuintCheck {
      name = "retry-policy-pull-store-degraded";
      # quint-policy P1 exemptions (bughunt-2 slot 11; §5-Q13 — the
      # retryPolicy-15 burn-down headline):
      vacuityExempt = {
        countersRefineHistory = {
          class = "pre-r2-untwinned";
          reason = "refinement-map invariant; the falsifier is a history-divergence twin over the spec/live pairing — Q13 headline (retryPolicy)";
        };
        noDoubleCount = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs a double-charge injection action on the exec ledger — Q13 headline (retryPolicy)";
        };
      };
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      step = "pullStep";
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
        "boundedFreeRequeues"
        "storeDegradedNeverPoisons"
        "storeDegradedDrawsNoBudget"
        "storeDegradedMintsNoExclusion"
        # bug_098 (signed bughunt-3 §5 Q1): the bounded-uncharged
        # classes COMPOSE — the union mint within one trailing
        # bounded-uncharged run is bounded by the sum of the per-class
        # bounds. Falsify twin: retry-098-guard-vacuous (below);
        # reachability: the composed-runs witness (below).
        # r[verify sched.attempt.worker-abort-bounded+2]
        "unionMintBounded"
        # Bughunt-2 slot 3 (m032): the uncharged store-degraded run
        # never exceeds the kernel bound (admit_store_degraded's gate;
        # falsify twin: the retry-032-unbounded-degraded calibration;
        # witnesses keep the bound edge AND the charged fallthrough
        # reachable).
        "boundedStoreDegradedRun"
        # bug_182 (bughunt-4 S5b): a fleet-exhaust MARKER row (folded
        # to no event) still BREAKS the bounded-uncharged run for BOTH
        # consumers — the one-run law's marker clause (run_step in the
        # kernel). Falsify twin: retry-182-forked-run (below);
        # reachability: the spawn-exhaust witness (below).
        # r[verify sched.retry.store-degraded-uncharged+4]
        "markerRowBreaksRuns"
      ];
    };
    quint-retry-policy-pull-runs-store-outage = mkQuintRunCheck {
      name = "retry-policy-pull-runs-store-outage";
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      match = "correlatedStoreOutageRun|sustainedOutageChargesPastBound";
    };
    # m032: the store-degraded run actually reaches the kernel bound —
    # boundedStoreDegradedRun's contended edge is reachable (three
    # consecutive uncharged store-degraded closes).
    quint-retry-policy-sd-witness-run-bound = mkQuintWitnessCheck {
      name = "retry-policy-sd-witness-run-bound";
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      step = "pullStep";
      witness = "canReachStoreDegradedBound";
    };
    # m032: the charged fallthrough actually fires past the bound (the
    # liveness direction — sustained outage eventually charges; the
    # bound is a gate, not a dead arm).
    quint-retry-policy-sd-witness-charge-past-bound = mkQuintWitnessCheck {
      name = "retry-policy-sd-witness-charge-past-bound";
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      step = "pullStep";
      witness = "canChargePastBound";
    };
    # The pre-fix fold (TLC, first-violation; no tracey markers on
    # calibration checks).
    quint-retry-policy-calib-408-sd-as-infra = mkQuintWitnessCheck {
      name = "retry-policy-calib-408-sd-as-infra";
      spec = "calibration/retry-408-sd-as-infra";
      main = "retryCalibSdAsInfra";
      extraSpecs = [ "retryPolicy" ];
      step = "calibStep";
      witness = "storeDegradedDrawsNoBudget";
    };
    # m032 falsify twin: the pre-bound intake (no admission guard, no
    # charged fallthrough) lets the uncharged run pass the kernel bound
    # (TLC, first-violation; no tracey markers on calibration checks).
    quint-retry-policy-calib-032-unbounded-degraded = mkQuintWitnessCheck {
      name = "retry-policy-calib-032-unbounded-degraded";
      spec = "calibration/retry-032-unbounded-degraded";
      main = "retryCalibUnboundedDegraded";
      extraSpecs = [ "retryPolicy" ];
      step = "calibStep";
      witness = "boundedStoreDegradedRun";
    };

    # bug_182 falsify twin (bughunt-4 S5b): the pre-fix forked run —
    # the fold's pacing reset gated on row_to_event().is_some(), so the
    # E9 fleet-exhaust marker row (folded to no event) broke the
    # admission scan but not the pacing curve. The calibration swaps
    # the spawn-gate arm for the run-keeping pre-fix shape;
    # markerRowBreaksRuns MUST violate (TLC, first-violation; no tracey
    # markers on calibration checks).
    quint-retry-policy-calib-182-forked-run = mkQuintWitnessCheck {
      name = "retry-policy-calib-182-forked-run";
      spec = "calibration/retry-182-forked-run";
      main = "retryCalibForkedRun";
      extraSpecs = [ "retryPolicy" ];
      step = "calibStep";
      witness = "markerRowBreaksRuns";
    };
    # bug_182 non-vacuity: the spawn-gate exhaust marker is reachable
    # in the SD regime (the markerRowBreaksRuns antecedent has teeth —
    # a regime where OE9Dispatch never lands would hold the implication
    # vacuously).
    quint-retry-policy-sd-witness-spawn-exhaust = mkQuintWitnessCheck {
      name = "retry-policy-sd-witness-spawn-exhaust";
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      step = "pullStep";
      witness = "canReachSpawnGateExhaust";
    };

    # bug_098 non-vacuity: BOTH per-class counts simultaneously >= 2 is
    # reachable — the union law genuinely composes the classes (the
    # pre-fix mutual reset made this state unreachable, which is why
    # per-class checks could never see the composition hole).
    # r[verify sched.retry.store-degraded-uncharged+4]
    # r[verify sched.attempt.worker-abort-bounded+2]
    quint-retry-policy-pull-witness-composed-runs = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-composed-runs";
      spec = "retryPolicy";
      main = "retryPolicyPullStoreDegraded";
      step = "pullStep";
      witness = "canReachComposedRuns";
    };

    # bug_098 falsify twin (signed bughunt-3 §5 Q1): the guard-vacuous
    # pre-fix admissions — each class's close reset the other's run, so
    # along every composed schedule the per-class guards were
    # identically true; modeled guard-free with the runs as union
    # observers (abstraction disclosed in the calibration header; the
    # pure same-class corner is owned by retry-032 and the kernel
    # decision tables). unionMintBounded MUST violate: seven uncharged
    # closes in one bounded-uncharged run.
    # r[verify sched.retry.store-degraded-uncharged+4]
    # r[verify sched.attempt.worker-abort-bounded+2]
    quint-retry-policy-pull-calib-098-guard-vacuous = mkQuintWitnessCheck {
      name = "retry-policy-pull-calib-098-guard-vacuous";
      spec = "calibration/retry-098-guard-vacuous";
      main = "retryCalibGuardVacuous";
      extraSpecs = [ "retryPolicy" ];
      step = "calibStep";
      witness = "unionMintBounded";
    };

    # ------------------------------------------------------------------
    # Substitution-replacement Phase A (T-5.1/T-5.2): the materialization
    # attempt class — the kind partition the campaign adds to the attempt
    # ledger (design §2.5/§9.2, OQ1 amendments 1-2). Two invariants:
    # materializationNeverPoisons (no materialization charge sequence —
    # including establishment-written crash charges — ever produces a
    # Poison/Cancel verdict or touches the cascade) and
    # materializationInvisibleToBuildBudgets (materialization charges feed
    # exactly one budget, their own; every build-side budget view is
    # untouched by them). Both are encoded in the pre-state-snapshot
    # tripwire style over bounded counters (the model's section-11
    # encoding note explains why no product-state predicate can do it);
    # both calibrated by working-tree falsification before wiring (the
    # introducing commit records the transcripts).
    #
    # Why a separate regime instead of extending quint-retry-policy-pull's
    # own alphabet: the materialization counters are independent state, so
    # enabling them in a regime multiplies its reachable state space by
    # the number of reachable counter combinations — a full product; the
    # partition is the point. The build-only pull regime is the model's
    # largest check, and any product factor >= 2 on it breaks the
    # merge-gate wall-clock threshold (the Phase-A stop-condition-8
    # 2x-baseline/5-min rule). The materialization-coexistence regime
    # (retryPolicyPullMat) therefore carries the product over the build
    # channels the partition is about — worker-report charges, the
    # no-report crash and its build-side establishment (the OQ1
    # adjacency: BOTH establishment channels are reachable, and the
    # invariants pin that the materialization one never feeds the build
    # fold), dispatch, the spawn-gate exhaust and the source-universe
    # shrink. The controller-fill machinery, the resets and leader
    # failover stay the build-only regime's concern: they explore
    # build-internal lifecycles the materialization class is structurally
    # independent of (the partition invariants force exactly that), so
    # their exclusion does not weaken what this check proves about the
    # partition.
    #
    # The dormant regime (retryPolicyPull, ENABLE_MATERIALIZATION = false)
    # keeps a bit-identical reachable state space — the wired-check
    # invariance half of Phase A's dormancy criterion 5, re-proven by
    # quint-retry-policy-pull reporting the same distinct-state count and
    # depth as its pre-extension baseline.
    #
    # The HOLD list is the build-only regime's full list PLUS the two
    # partition invariants: the pre-existing invariants are re-proven
    # over materialization interleavings (a materialization action
    # between any two build events must not perturb any of them).
    # r[verify sched.materialize.routing+7]
    quint-retry-policy-pull-materialization = mkQuintCheck {
      name = "retry-policy-pull-materialization";
      # quint-policy P1 exemptions (bughunt-2 slot 11; §5-Q13 — the
      # retryPolicy-15 burn-down headline):
      vacuityExempt = {
        countersRefineHistory = {
          class = "pre-r2-untwinned";
          reason = "refinement-map invariant; the falsifier is a history-divergence twin over the spec/live pairing — Q13 headline (retryPolicy)";
        };
        noDoubleCount = {
          class = "pre-r2-untwinned";
          reason = "the falsifier needs a double-charge injection action on the exec ledger — Q13 headline (retryPolicy)";
        };
      };
      spec = "retryPolicy";
      main = "retryPolicyPullMat";
      step = "pullStep";
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
        "materializationNeverPoisons"
        "materializationInvisibleToBuildBudgets"
      ];
    };

    # Non-vacuity witness for the materialization-coexistence regime: the
    # establishment crash channel (OQ1 amendment 1) is reachable and
    # charges the materialization budget — the contended channel both
    # partition invariants protect against (a crashed store replica's
    # establishment is THE site that could plausibly write executor_crash
    # into the build ledger; this witness pins that the model exercises
    # it, so the invariants are never vacuously green).
    quint-retry-policy-pull-witness-materialization-crash = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-materialization-crash";
      spec = "retryPolicy";
      main = "retryPolicyPullMat";
      step = "pullStep";
      witness = "noMaterializationCrashCharge";
    };

    # ------------------------------------------------------------------
    # Substitution-replacement Phase C-prime: the materialization-job
    # lifecycle model (docs/spec/models/materializationJob.qnt),
    # re-targeted to the AS-BUILT Phase B system (the six handoff
    # deltas: the finding-11 mark discriminator + clear-mirror, the
    # executor channel redial, the wanted backfill, the park
    # re-evaluation split, the five-origin set, the recovery view
    # rebuild, the must_substitute claim upgrade). The §9.1 successor
    # property table (21 invariants incl. the three at-decision-time
    # re-encodes and the delta-derived invariants), the §9.3
    # calibration-transfer verdict table, and the exhaustive-budget
    # measurement record live in
    # docs/spec/models/substitution-replacement-invariant-map.md
    # (C-prime stage record).
    #
    # Check-set shape (the closure-evidence Phase-1 precedent): the
    # exhaustive TLC conjunctions do not converge inside a
    # gate-compatible budget at any modeled scope (the C-prime
    # measurement table; the reduced -Ex scopes are documented manual
    # targets with zero-violation bounded coverage), so the GHA-wired
    # deliverable per regime is the bounded-simulation HOLDS check —
    # each named pin below is its falsifiability pair — plus the
    # witness set (every §9.1 property's contended scenario stays
    # demonstrably reachable) and the §9.3 calibration pins (TLC,
    # first-violation — the permanent expect-violation record that the
    # model re-finds every transferred bug class).
    # ------------------------------------------------------------------

    # Deterministic named-run pins (the delta scenario narratives:
    # happy path, the park split, the four-conjunct fail-fast, the
    # unmarked arm-3 from-source, the marked-claim upgrade, the legacy
    # backfill, the reprobe reset; stale-reset and the failover
    # redial/view-rebuild run under the regimes that enable them).
    quint-materialization-runs-base = mkQuintRunCheck {
      name = "materialization-runs-base";
      spec = "materializationJob";
      main = "materializationJobBase";
      match = "happyPathRun|parkSplitRun|prunedFailFastRun|unmarkedArm3FromSourceRun|markedClaimUpgradeRun|legacyBackfillRun|reprobeResetRun|twoJobFreshWindowRun";
    };
    quint-materialization-runs-failover = mkQuintRunCheck {
      name = "materialization-runs-failover";
      spec = "materializationJob";
      main = "materializationJobFailover";
      match = "failoverRedialRun|failoverViewRebuildRun";
    };
    quint-materialization-runs-adversarial = mkQuintRunCheck {
      name = "materialization-runs-adversarial";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      match = "staleResetRun";
    };
    # A3 (bughunt wave): the owner-signed Q5 reversal's contract pin —
    # a pure-establishment crash-loop parks at the budget
    # (establishmentParkRun needs ENABLE_CRASH).
    quint-materialization-runs-crash-loop = mkQuintRunCheck {
      name = "materialization-runs-crash-loop";
      spec = "materializationJob";
      main = "materializationJobCrashLoop";
      match = "establishmentParkRun";
    };

    # Bounded-simulation HOLDS checks, one per design-scale regime —
    # the full 21-invariant §9.1 conjunction. Bounded evidence, not
    # proof: the exhaustive Ex-scope conjunctions are the documented
    # manual targets (commands + measured budgets in the C-prime stage
    # record). Sample sizing: the paired calibration pins' violation
    # classes are found by the simulator in well under 100 K samples at
    # the smaller Ex scopes; 2 M samples at design scale clears the
    # 25/p flake floor with two orders of headroom.
    #
    # Paired falsifiability pins (the constructor's vacuity rule —
    # every invariant in the list has at least one pin or witness
    # below): quint-materialization-calib-* (22 expect-violation pins)
    # + the marked-claim / post-failover-claim witnesses (the B1/B3
    # liveness flips).
    # r[verify sched.materialize.job+2]
    # r[verify sched.materialize.routing+7]
    quint-materialization-holds-base = mkQuintSimHoldsCheck {
      name = "materialization-holds-base";
      spec = "materializationJob";
      main = "materializationJobBase";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # r[verify store.materialize.executor+5]
    # r[verify sched.materialize.settlement]
    quint-materialization-holds-failover = mkQuintSimHoldsCheck {
      name = "materialization-holds-failover";
      spec = "materializationJob";
      main = "materializationJobFailover";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # r[verify sched.materialize.pinning]
    quint-materialization-holds-adversarial-store = mkQuintSimHoldsCheck {
      name = "materialization-holds-adversarial-store";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # The claims-floor fence's model-level verification post-D-prime:
    # fencedJobWritesOnly (in the conjunction) is the living oracle for
    # the uniform fence over job-table and wanted-relation writes — the
    # column-agnostic core of the durability rule (the evidence-column
    # subject was deleted; migration 080). Falsifiability pair:
    # quint-materialization-calib-f11-unfenced-resolve.
    # r[verify sched.evidence.durability+4]
    quint-materialization-holds-stale-tenure = mkQuintSimHoldsCheck {
      name = "materialization-holds-stale-tenure";
      spec = "materializationJob";
      main = "materializationJobStaleTenure";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    quint-materialization-holds-crash-loop = mkQuintSimHoldsCheck {
      name = "materialization-holds-crash-loop";
      spec = "materializationJob";
      main = "materializationJobCrashLoop";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # B1 (merged_bug_189 / owner Q3): the worker-abort regime — the
    # base alphabet plus the SIGTERM-aborted charge-free close; the
    # full conjunction including abortNeverCharges holds with the
    # action enabled. Falsifiability pair:
    # quint-materialization-calib-189-abort-charges.
    quint-materialization-holds-worker-abort = mkQuintSimHoldsCheck {
      name = "materialization-holds-worker-abort";
      spec = "materializationJob";
      main = "materializationJobWorkerAbort";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # Slot-10 walk-fold plane: the four kernel-fold laws hold under
    # the composed step (machine moves XOR one fold evaluates), PLUS
    # the full machine conjunction under the armed regime — composition
    # broke nothing. Falsifiability pairs:
    # quint-materialization-calib-{299,295,133,115}-* below; the
    # walk-fold reachability witness keeps the plane non-vacuous.
    # r[verify store.materialize.local-visibility]
    # r[verify store.materialize.probe-polarity]
    # r[verify store.materialize.tenant-fold+2]
    # r[verify sched.materialize.reprobe-per-path]
    quint-materialization-holds-walk-fold = mkQuintSimHoldsCheck {
      name = "materialization-holds-walk-fold";
      spec = "materializationJob";
      main = "materializationJobWalkFold";
      invariants = matJobInvariants ++ walkFoldInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # The walk-fold deterministic pin: the bug_299 defining cell
    # (complementary coverage folds Obtainable under the lawful
    # quantifier).
    quint-materialization-runs-walk-fold = mkQuintRunCheck {
      name = "materialization-runs-walk-fold";
      spec = "materializationJob";
      main = "materializationJobWalkFold";
      match = "walkFoldComplementaryCoverageRun|stampLawWalkVerifiedRun";
    };

    # Non-vacuity witnesses (rust simulator; expect-violation). Every
    # §9.1 property's contended scenario + every delta's new behavior
    # stays demonstrably reachable. No tracey markers on witnesses.
    # Sample sizing: every witness here was first found in under 300 K
    # samples (most in the first batches); 2 M is the flake floor.

    # The happy path resolves a job successfully.
    quint-materialization-witness-success = mkQuintSimWitnessCheck {
      name = "materialization-witness-success";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noSuccessResolution";
    };
    # The four-conjunct fail-fast fires (delta 1: needs the
    # pruned-origin mark — keeps the arm-3 corner non-vacuous).
    quint-materialization-witness-fail-fast = mkQuintSimWitnessCheck {
      name = "materialization-witness-fail-fast";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noFailFast";
    };
    # The unmarked arm-3 from-source disposition (delta 1, the B2 fix
    # row): if this stops violating, the 6-row table's release arm has
    # gone unreachable and the routing checks are vacuous there.
    quint-materialization-witness-unmarked-arm3 = mkQuintSimWitnessCheck {
      name = "materialization-witness-unmarked-arm3";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noUnmarkedArm3FromSource";
    };
    # A MARKED node's job is claimed (delta 6 — the B1 admission-gap
    # regression guard: pre-B1 the must_substitute refusal made this
    # unreachable; the mat-b1-claim-refuses-marked calibration module
    # is the recorded pre-fix flip).
    quint-materialization-witness-marked-claim = mkQuintSimWitnessCheck {
      name = "materialization-witness-marked-claim";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noMarkedClaim";
    };
    # A claim lands AFTER a failover staled every executor channel
    # (delta 2a — the B3 redial liveness guard: without redialChannel
    # this is unreachable; the mat-b3-no-redial calibration module is
    # the recorded pre-fix flip).
    quint-materialization-witness-post-failover-claim = mkQuintSimWitnessCheck {
      name = "materialization-witness-post-failover-claim";
      spec = "materializationJob";
      main = "materializationJobFailover";
      witness = "noPostFailoverClaim";
    };
    # A4: a transient (RetryLater) close fires — the charge-free
    # invariant constrains a reachable edge (the deferral flag is the
    # close's only footprint).
    quint-materialization-witness-transient-close = mkQuintSimWitnessCheck {
      name = "materialization-witness-transient-close";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noTransientClose";
    };
    # A4 (the RBroken split): a CHILDLESS-LEAF park is released by the
    # park re-evaluation arm. Pre-fix (Vouched|Pending-only guard) this
    # is UNREACHABLE — mat-a4-leaf-park-forever is the recorded flip
    # (the b3-no-redial dead-end pattern; manual command in its
    # header).
    quint-materialization-witness-leaf-park-reeval = mkQuintSimWitnessCheck {
      name = "materialization-witness-leaf-park-reeval";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noLeafParkReevalResolve";
    };
    # The budget park fires (delta 3: rides the InfraFailure
    # consumption).
    quint-materialization-witness-park = mkQuintSimWitnessCheck {
      name = "materialization-witness-park";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noPark";
    };
    # A Broken-evidence park (the MD-D1 stalled-gauge population) is
    # reachable (delta 3).
    quint-materialization-witness-stalled-park = mkQuintSimWitnessCheck {
      name = "materialization-witness-stalled-park";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noStalledBrokenPark";
    };
    # The park re-evaluation resolves a Vouched/Pending park from
    # source (delta 3, the PD-20 arm).
    quint-materialization-witness-park-reeval = mkQuintSimWitnessCheck {
      name = "materialization-witness-park-reeval";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noParkReevalResolve";
    };
    # A legacy (pre-relation-era) build's relation is backfilled at
    # creation (delta 2b, the B4 fix's contended state).
    quint-materialization-witness-legacy-backfill = mkQuintSimWitnessCheck {
      name = "materialization-witness-legacy-backfill";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noLegacyBackfill";
    };
    # A stale_reset job resets a Completed node (delta 4): runs in the
    # adversarial-store regime (the GC'd-outputs shape needs GC).
    quint-materialization-witness-stale-reset = mkQuintSimWitnessCheck {
      name = "materialization-witness-stale-reset";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      witness = "noStaleResetCreation";
    };
    # A reprobe-origin job (the AS-5 reset lane, delta 4).
    quint-materialization-witness-reprobe = mkQuintSimWitnessCheck {
      name = "materialization-witness-reprobe";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noReprobeCreation";
    };
    # The §5.3 pin release fires (job resolved + all interest
    # terminal) — the pinning rule's release half stays reachable.
    quint-materialization-witness-pin-release = mkQuintSimWitnessCheck {
      name = "materialization-witness-pin-release";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noPinRelease";
    };
    # A crashed materialization attempt is established (the PP-4
    # channel both partition invariants constrain).
    quint-materialization-witness-crash-establishment = mkQuintSimWitnessCheck {
      name = "materialization-witness-crash-establishment";
      spec = "materializationJob";
      main = "materializationJobCrashLoop";
      witness = "noCrashEstablishment";
    };
    # A build-kind attempt opens (kindMatchesWorker /
    # noFromSourceWhileJobUnresolved non-vacuity).
    quint-materialization-witness-build-attempt = mkQuintSimWitnessCheck {
      name = "materialization-witness-build-attempt";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noBuildAttempt";
    };

    # ---- Materialization C-prime calibration pins ---------------------
    # The §9.3 transfer corpus, executed (the go/no-go core): each pin
    # instantiates the re-targeted model at a reduced Ex scope, swaps
    # ONE action for its PRE-FIX behavior (the override's calibStep)
    # and passes only while the checker still falsifies the §9.1
    # property the as-built mechanism protects. Verdict table (every
    # transferred family dispositioned, incl. the by-construction rows
    # and the two liveness witness-flips): the C-prime stage record.
    # TLC backend, first-violation (13-27 s each at the qualifying
    # measurement). No tracey markers on calibration checks.

    # F8 + F13 (CE-33/CE-58 — THE anchor): the evidence-blind builder
    # pull delivers from source while the job is unresolved.
    quint-materialization-calib-f8-pull-ignores-job = mkQuintWitnessCheck {
      name = "materialization-calib-f8-pull-ignores-job";
      spec = "calibration/mat-f8-pull-ignores-job";
      main = "matCalibF8PullIgnoresJob";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noFromSourceWhileJobUnresolved";
    };
    # F10 L1-half (CE-48(i)): the pre-T-4.3 failover drops the view
    # without the rebuild — the pending job's armed action strands.
    quint-materialization-calib-f10-view-drop = mkQuintWitnessCheck {
      name = "materialization-calib-f10-view-drop";
      spec = "calibration/mat-f10-failover-view-drop";
      main = "matCalibF10FailoverViewDrop";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "unresolvedJobAlwaysArmed";
    };
    # R10 (bughunt-2): the row-durability hypothetical — a failover
    # drops unresolved job rows through the live failoverRowsApply
    # seat (dropsRows=true). failoverPreservesJobs' first falsifier
    # (P1: the invariant was untwinned in all seven regimes checking
    # it). Measured 3.7s TLC; the default wall-clock budget carries
    # two orders of headroom.
    quint-materialization-calib-failover-drops-rows = mkQuintWitnessCheck {
      name = "materialization-calib-failover-drops-rows";
      spec = "calibration/mat-failover-drops-rows";
      main = "matCalibFailoverDropsRows";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "failoverPreservesJobs";
    };
    # B5(a): the dedup re-feed overwrites armament state (park/holder).
    quint-materialization-calib-b5a-refeed-overwrite = mkQuintWitnessCheck {
      name = "materialization-calib-b5a-refeed-overwrite";
      spec = "calibration/mat-b5a-refeed-overwrite";
      main = "matCalibB5aRefeedOverwrite";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "unresolvedJobAlwaysArmed";
    };
    # CE-17 / F6 / F2(a): success consumption without the coverage
    # re-check (the wanted-growth race).
    quint-materialization-calib-ce17-skip-coverage = mkQuintWitnessCheck {
      name = "materialization-calib-ce17-skip-coverage";
      spec = "calibration/mat-ce17-skip-coverage";
      main = "matCalibCe17SkipCoverage";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "successConsumptionCoversLiveWanted";
    };
    # F2(b) / BC-3: the establishment adopts on output presence.
    quint-materialization-calib-f2b-establish-adopt = mkQuintWitnessCheck {
      name = "materialization-calib-f2b-establish-adopt";
      spec = "calibration/mat-f2b-establish-adopt";
      main = "matCalibF2bEstablishAdopt";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "successConsumptionCoversLiveWanted";
    };
    # F3 soundness (i): InfraFailure charged as Unobtainable corrupts
    # the one-shot's evidence.
    quint-materialization-calib-f3-infra-as-unobtainable = mkQuintWitnessCheck {
      name = "materialization-calib-f3-infra-as-unobtainable";
      spec = "calibration/mat-f3-infra-as-unobtainable";
      main = "matCalibF3InfraAsUnobtainable";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noWrongfulTerminalFailure";
    };
    # F3 soundness (ii) / E5: budget exhaustion fails instead of parks.
    quint-materialization-calib-f3-park-failfast = mkQuintWitnessCheck {
      name = "materialization-calib-f3-park-failfast";
      spec = "calibration/mat-f3-park-failfast";
      main = "matCalibF3ParkFailFast";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationNeverPoisons";
    };
    # F3 permissiveness (CE-60/CE-3): unverified missing paths route
    # from source while upstream offers them.
    quint-materialization-calib-f3p-unsound-report = mkQuintWitnessCheck {
      name = "materialization-calib-f3p-unsound-report";
      spec = "calibration/mat-f3p-unsound-report";
      main = "matCalibF3pUnsoundReport";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noWrongfulFromSourceRouting";
    };
    # F7 (CE-30/CE-28): the un-keyed resolution.
    quint-materialization-calib-f7-unkeyed-resolve = mkQuintWitnessCheck {
      name = "materialization-calib-f7-unkeyed-resolve";
      spec = "calibration/mat-f7-unkeyed-resolve";
      main = "matCalibF7UnkeyedResolve";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "jobResolutionSound";
    };
    # F9 (CE-41 re-pointed): routing trusts a divergent in-memory
    # children view over the durable relation.
    quint-materialization-calib-f9-divergent-evidence = mkQuintWitnessCheck {
      name = "materialization-calib-f9-divergent-evidence";
      spec = "calibration/mat-f9-divergent-evidence";
      main = "matCalibF9DivergentEvidence";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "routingRequiresDurableVouchOrFailFast";
    };
    # F11 (CE-50, the a17-unfenced analogue): a stale tenure's job
    # resolve applies below the claims floor.
    quint-materialization-calib-f11-unfenced-resolve = mkQuintWitnessCheck {
      name = "materialization-calib-f11-unfenced-resolve";
      spec = "calibration/mat-f11-unfenced-resolve";
      main = "matCalibF11UnfencedResolve";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "fencedJobWritesOnly";
    };
    # B4 (delta 2b): probe creation without the legacy backfill.
    quint-materialization-calib-b4-no-backfill = mkQuintWitnessCheck {
      name = "materialization-calib-b4-no-backfill";
      spec = "calibration/mat-b4-no-backfill";
      main = "matCalibB4NoBackfill";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "creationLeavesTenantResolvable";
    };
    # The partial-unique-index dedup, slot-widened.
    quint-materialization-calib-dedup-removed = mkQuintWitnessCheck {
      name = "materialization-calib-dedup-removed";
      spec = "calibration/mat-dedup-removed";
      main = "matCalibDedupRemoved";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "atMostOneUnresolvedJobPerDrv";
    };
    # PP-4 (i): the establishment writes executor_crash for the
    # materialization kind.
    quint-materialization-calib-pp4-build-charge = mkQuintWitnessCheck {
      name = "materialization-calib-pp4-build-charge";
      spec = "calibration/mat-pp4-establish-build-charge";
      main = "matCalibPp4EstablishBuildCharge";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationInvisibleToBuildBudgets";
    };
    # PP-4 (ii): the establishment closes the attempt charge-free.
    quint-materialization-calib-pp4-uncharged = mkQuintWitnessCheck {
      name = "materialization-calib-pp4-uncharged";
      spec = "calibration/mat-pp4-establish-uncharged";
      main = "matCalibPp4EstablishUncharged";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationCrashChargedOnce";
    };
    # B2-strong shape (a): ingest without the pin INSERT re-finds the
    # GC-after-vouch trace shape.
    quint-materialization-calib-b2-no-pin = mkQuintWitnessCheck {
      name = "materialization-calib-b2-no-pin";
      spec = "calibration/mat-b2-no-pin-at-ingest";
      main = "matCalibB2NoPinAtIngest";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "pinCoversIngestUntilAllInterestTerminal";
    };
    # bug_233 (bughunt wave, D1): the swallowed job_id parse → an
    # unattributed (NULL-job) materialization pin the §5.3 release rule
    # can never resolve — immortal. Production close: ClaimedJob.job_id
    # parse-don't-validate + the 093 CHECK.
    quint-materialization-calib-233-unattributed-pin = mkQuintWitnessCheck {
      name = "materialization-calib-233-unattributed-pin";
      spec = "calibration/mat-233-unattributed-pin";
      main = "matCalib233UnattributedPin";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationPinHasJob";
    };
    # F5 / PP-5 (i): the wholesale relation overwrite breaks
    # cross-build isolation.
    quint-materialization-calib-f5-wanted-overwrite = mkQuintWitnessCheck {
      name = "materialization-calib-f5-wanted-overwrite";
      spec = "calibration/mat-f5-wanted-overwrite";
      main = "matCalibF5WantedOverwrite";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "crossBuildWantedIsolation";
    };
    # A4 (merged_bug_178/195): the pre-fix transient-as-infra charge —
    # a 429/raced close moved the materialization ledger and parked at
    # the budget.
    quint-materialization-calib-a4-transient-as-infra = mkQuintWitnessCheck {
      name = "materialization-calib-a4-transient-as-infra";
      spec = "calibration/mat-a4-transient-as-infra";
      main = "matCalibTransientAsInfra";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "transientOutcomesNeverCharge";
    };
    # A4 (merged_bug_193): the pre-fix refs fold — the moot arm
    # completed a node over a confirmed reference hole.
    quint-materialization-calib-a4-refs-folded = mkQuintWitnessCheck {
      name = "materialization-calib-a4-refs-folded";
      spec = "calibration/mat-a4-refs-folded";
      main = "matCalibRefsFolded";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "closureCompleteResolution";
    };
    # A4 (merged_bug_194 store leg): the pre-fix vacuous Success — a
    # zero-wanted zero-evidence walk completed the node.
    quint-materialization-calib-a4-vacuous-success = mkQuintWitnessCheck {
      name = "materialization-calib-a4-vacuous-success";
      spec = "calibration/mat-a4-vacuous-success";
      main = "matCalibVacuousSuccess";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noVacuousCoverage";
    };
    # A4 (fork-11 commit 1): the pre-fix wanted overwrite — a re-merge
    # narrowed the durable row below an earlier contribution.
    quint-materialization-calib-a4-union-dropped = mkQuintWitnessCheck {
      name = "materialization-calib-a4-union-dropped";
      spec = "calibration/mat-a4-union-dropped";
      main = "matCalibUnionDropped";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "durableUnionWidensOrEqualsLive";
    };
    # A4 (the RBroken split): the pre-fix park-viability collapse — a
    # parked from-source-viable childless leaf in the collapsed
    # stalled population (the leaf-release dead-end is the manual half,
    # recorded in the module header).
    quint-materialization-calib-a4-leaf-park-forever = mkQuintWitnessCheck {
      name = "materialization-calib-a4-leaf-park-forever";
      spec = "calibration/mat-a4-leaf-park-forever";
      main = "matCalibLeafParkForever";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "calibParkViability";
    };
    # F4 B4-half + the C5/CE-7 closure evidence: the dead-inclusive
    # stored-union coverage read (a terminal build's stale wants drive
    # the decision) — the behavior the live-only §6 join replaced.
    quint-materialization-calib-f4-dead-union = mkQuintWitnessCheck {
      name = "materialization-calib-f4-dead-union";
      spec = "calibration/mat-f4-dead-union";
      main = "matCalibF4DeadUnion";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "interestUnionLiveOnly";
    };
    # F1 permissiveness (CE-1): the by-construction row's REACHABILITY
    # half — the covered-creation space (the state the as-built
    # presence re-check excludes) is reachable under the override, so
    # the recorded no-falsification verdict (the §9.1 conjunction holds
    # over that space — the C-prime stage record's by-construction
    # evidence) is about a real space, not vacuity.
    quint-materialization-calib-f1-covered-creation = mkQuintWitnessCheck {
      name = "materialization-calib-f1-covered-creation";
      spec = "calibration/mat-f1-no-presence-recheck";
      main = "matCalibF1NoPresenceRecheck";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noCoveredCreationJob";
    };

    # ------------------------------------------------------------------
    # A1 fenced-write-discipline (bughunt wave) — view settlement.
    #
    # The resolve-faults regime exposes the durable resolve's FAULT
    # arms (fenced/errored) as steps; the two A1 invariants
    # (viewMatchesDurableUnresolved, chargeFreeCancellation — in
    # matJobInvariants for every regime) bind the view to the durable
    # unresolved relation and freeze a cancelled job's history. The
    # paired calibration pins re-find the pre-fix view discard (133)
    # and the split-cancel attempt leak (276).
    # ------------------------------------------------------------------
    # r[verify sched.materialize.view-settlement]
    quint-materialization-holds-resolve-faults = mkQuintSimHoldsCheck {
      name = "materialization-holds-resolve-faults";
      spec = "materializationJob";
      main = "materializationJobResolveFaults";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # 133: the fenced/errored resolve still discarded the view entry.
    quint-materialization-calib-133-discarded-outcome = mkQuintWitnessCheck {
      name = "materialization-calib-133-discarded-outcome";
      spec = "calibration/mat-133-discarded-outcome";
      main = "matCalib133DiscardedOutcome";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "viewMatchesDurableUnresolved";
    };
    # 276: the split cancel leaked the open attempt to the
    # establishment charge.
    quint-materialization-calib-276-dag-absent-cancel = mkQuintWitnessCheck {
      name = "materialization-calib-276-dag-absent-cancel";
      spec = "calibration/mat-276-dag-absent-cancel";
      main = "matCalib276DagAbsentCancel";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "chargeFreeCancellation";
    };
    # A2 kind-partition calibrations (bughunt wave). 146: the kind-blind
    # controller-verdict close leaves a Claimed view with no attempt
    # (the 307 wedge mirror's controller half).
    quint-materialization-calib-146-cross-kind-close = mkQuintWitnessCheck {
      name = "materialization-calib-146-cross-kind-close";
      spec = "calibration/mat-146-controller-cross-kind-close";
      main = "matCalib146ControllerCrossKindClose";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "claimedImpliesOpenAttempt";
    };
    # 266: the kind-blind recovery claimed_by join projects a BUILD
    # attempt's builder as a pending job's holder.
    quint-materialization-calib-266-kindblind-holder = mkQuintWitnessCheck {
      name = "materialization-calib-266-kindblind-holder";
      spec = "calibration/mat-266-kindblind-recovery-holder";
      main = "matCalib266KindblindRecoveryHolder";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "claimedByOnlyMatHolders";
    };
    # 318: the forced-Ready release upgrades a Queued-origin
    # (dep-racing) claim.
    quint-materialization-calib-318-requeue-forces-ready = mkQuintWitnessCheck {
      name = "materialization-calib-318-requeue-forces-ready";
      spec = "calibration/mat-318-requeue-forces-ready";
      main = "matCalib318RequeueForcesReady";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "readyImpliesDepsProducedOnRequeue";
    };
    # A3 materialization-lifecycle-kernel calibrations (bughunt wave):
    # the re-derivation's polarity rule — the two REMOVED as-built
    # encodings preserved as expect-violation pins, the two
    # not-representable pre-fix splits INTRODUCED calibration-only.
    # never-parks: the establishment charge without the park decision
    # (bug_067 — the counter-signed residual the Q5 reversal
    # superseded; TLC first violation ~2.6s).
    quint-materialization-calib-establish-never-parks = mkQuintWitnessCheck {
      name = "materialization-calib-establish-never-parks";
      spec = "calibration/mat-establish-never-parks";
      main = "matCalibEstablishNeverParks";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "budgetSoundness";
    };
    # unscoped-count: the drv-level one-shot read spanning jobs
    # (merged_bug_020 — the flat history count; TLC ~14s).
    quint-materialization-calib-unscoped-count = mkQuintWitnessCheck {
      name = "materialization-calib-unscoped-count";
      spec = "calibration/mat-unscoped-count";
      main = "matCalibUnscopedCount";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "budgetSoundness";
    };
    # split-rearm: the claim drop without the node requeue — the
    # second-replica wedge (merged_bug_015/307; TLC ~2.2s).
    quint-materialization-calib-split-rearm = mkQuintWitnessCheck {
      name = "materialization-calib-split-rearm";
      spec = "calibration/mat-split-rearm-without-reassign";
      main = "matCalibSplitRearm";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "pendingUnclaimedImpliesClaimableNode";
    };
    # chain-reset, conservation leg: the stale-reset creation drops
    # the carrier (merged_bug_257; TLC ~3.8s at full scope).
    quint-materialization-calib-chain-reset-carrier = mkQuintWitnessCheck {
      name = "materialization-calib-chain-reset-carrier";
      spec = "calibration/mat-chain-reset-drops-carrier";
      main = "matCalibChainResetDropsCarrier";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "carrierConservation";
    };
    # chain-reset, completion leg: the dropped carrier reaches a
    # success resolution — the [""] stamp image (merged_bug_055).
    # Reduced single-drv scope per the F13 sub-regime ladder: the
    # two-drv TLC run is ~9min to first violation, the Ex scope ~3s.
    quint-materialization-calib-chain-reset-completion = mkQuintWitnessCheck {
      name = "materialization-calib-chain-reset-completion";
      spec = "calibration/mat-chain-reset-drops-carrier";
      main = "matCalibChainResetDropsCarrierEx";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "completionRecordsRealizedPaths";
    };
    # charge-on-cancel: the split cancel leaves the attempt open and
    # the establishment charges the cancelled job (the A3 leg of the
    # 276 class — extends A1's chargeFreeCancellation latch; TLC
    # ~2.7s).
    quint-materialization-calib-charge-on-cancel = mkQuintWitnessCheck {
      name = "materialization-calib-charge-on-cancel";
      spec = "calibration/mat-charge-on-cancel";
      main = "matCalibChargeOnCancel";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "chargeFreeCancellation";
    };
    # head-starvation (bug_385): a deterministic RUN pin, not an
    # invariant falsification — the LIMIT-1 oldest-first admission
    # refuses the younger claim behind a parked head while the
    # as-built free claim (the in-pass skip) proceeds from the same
    # state. The sim-witness encoding is edge-shaped and cannot flip
    # (recorded in the calibration header + the invariant map).
    quint-materialization-calib-head-starvation = mkQuintRunCheck {
      name = "materialization-calib-head-starvation";
      spec = "calibration/mat-head-starvation";
      main = "matCalibHeadStarvation";
      extraSpecs = [ "materializationJob" ];
      match = "headStarvationRun";
    };
    # f10b swallowed-rebuild (merged_bug_246): the failover rebuild
    # runs but a per-drv load failure is swallowed — partial view
    # served as hydrated. INTRODUCED calibration-only (the as-built
    # failover rebuilds faithfully; the pre-fix shape is not
    # representable in the main model — §2.A3 polarity, model twin of
    # the kani projection gap).
    quint-materialization-calib-f10b-swallowed-rebuild = mkQuintWitnessCheck {
      name = "materialization-calib-f10b-swallowed-rebuild";
      spec = "calibration/mat-f10b-swallowed-rebuild";
      main = "matCalibF10bSwallowedRebuild";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "unresolvedJobAlwaysArmed";
    };
    # B1 (merged_bug_189): the pre-fix Err->infra flatten — a
    # SIGTERM-aborted walk drew the materialization budget.
    quint-materialization-calib-189-abort-charges = mkQuintWitnessCheck {
      name = "materialization-calib-189-abort-charges";
      spec = "calibration/mat-189-abort-charges";
      main = "matCalibAbortCharges";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "abortNeverCharges";
    };
    # B1 (merged_bug_158): the pre-fix token-blind re-delivery — two
    # workers behind one identity hold the same open attempt; the
    # §2.2 arbiter's worker-class identity clause falsifies.
    quint-materialization-calib-158-colliding-identity = mkQuintWitnessCheck {
      name = "materialization-calib-158-colliding-identity";
      spec = "calibration/mat-158-colliding-identity";
      main = "matCalibCollidingIdentity";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "atMostOneClaimWinner";
    };

    # ------------------------------------------------------------------
    # Slot-10 walk-fold calibrations (the four kernel-fold laws'
    # falsifiability pairs) + the plane's reachability witness. All
    # five run the threaded model's composed step; the perturbed
    # decision routes through the SAME walkFoldApply seat (P5).
    # ------------------------------------------------------------------
    # bug_299: the whole-set per-tenant projection folds
    # ConfirmedMissing on the complementary-coverage matrix.
    quint-materialization-calib-299-wholeset-projection = mkQuintWitnessCheck {
      name = "materialization-calib-299-wholeset-projection";
      spec = "calibration/mat-299-wholeset-projection";
      main = "matCalibWholesetProjection";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "reprobeCongruentWithCompletability";
    };
    # bug_295: the probe leg's terminal 429 charged like a 5xx — the
    # park-burning harm case.
    quint-materialization-calib-295-probe-charged = mkQuintWitnessCheck {
      name = "materialization-calib-295-probe-charged";
      spec = "calibration/mat-295-probe-charged";
      main = "matCalibProbeCharged";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "rateLimitedProbeDrawsNoBudget";
    };
    # merged_bug_133: the in-loop return starves the serving tenant
    # behind a charging one.
    quint-materialization-calib-133-inloop-return = mkQuintWitnessCheck {
      name = "materialization-calib-133-inloop-return";
      spec = "calibration/mat-133-inloop-return";
      main = "matCalibInloopReturn";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "hitUnderAnyTenantOrderSucceeds";
    };
    # bug_115: raw physical presence serves a gate-hidden local row.
    quint-materialization-calib-115-tenantblind-present = mkQuintWitnessCheck {
      name = "materialization-calib-115-tenantblind-present";
      spec = "calibration/mat-115-tenantblind-present";
      main = "matCalibTenantblindPresent";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "verifiedPathsVisibleToInterest";
    };
    # The hazard-www vacuity guard: a fold IS evaluated under the
    # composed step (expect-violation of the never-folded claim), so
    # the four latches above are exercised, never vacuous.
    quint-materialization-witness-walk-fold-reachable = mkQuintWitnessCheck {
      name = "materialization-witness-walk-fold-reachable";
      spec = "materializationJob";
      main = "materializationJobWalkFold";
      witness = "noWalkFoldEvaluated";
    };
    # Bughunt-3 S3 (bug_139, signed Q2): the ∃-gate/∀-stamp cartesian
    # exceeds the witness-derived lawful pairs — the pre-fix shape,
    # re-derived through the SAME stampApply seat (P5). Measured
    # [violation] in 774ms at 400000×12 (rust backend).
    quint-materialization-calib-139-existsgate-forallstamp = mkQuintWitnessCheck {
      name = "materialization-calib-139-existsgate-forallstamp";
      spec = "calibration/mat-139-existsgate-forallstamp";
      main = "matCalibExistsgateForallstamp";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "stampedOnlyVerifiedTenants";
    };
    # The stamp sub-plane's hazard-www vacuity guard: a stamp IS
    # decided under the composed step, so stampLawAll is exercised,
    # never vacuous. Measured [violation] in 1258ms at 400000×12
    # (rust backend).
    quint-materialization-witness-stamp-fold-reachable = mkQuintWitnessCheck {
      name = "materialization-witness-stamp-fold-reachable";
      spec = "materializationJob";
      main = "materializationJobWalkFold";
      witness = "noStampFoldEvaluated";
    };

    # ------------------------------------------------------------------
    # A1 fenced-write-discipline — the fence over interleaved
    # transactions (docs/spec/models/fencedWrites.qnt): READ COMMITTED
    # modeled precisely (begin-time floor snapshot vs the in-statement
    # EvalPlanQual predicate). Tier-1 exhaustive at MAX_GEN=3 /
    # 2 replicas / 1 drv; the four calibration pins are the
    # falsifiability pairs (261 TOCTOU, 231 deposed close, 273 floor
    # regression, 393 terminal refusal). The fresh-insert-below-floor
    # residual is deliberately reachable (priced in
    # fence-invariant-map.md) — activeRowGenMonotonic holds with it.
    # ------------------------------------------------------------------
    # Bughunt-2 planes (slot 2): the same board gains (1) the
    # tenure-stamp plane — writesCarryClaimedTenure (merged_bug_338:
    # the typed ServingGeneration vs a fresh lease-atomic read), (2)
    # the outbox plane — outboxReplayNeverRegresses +
    # outboxClosesOnlyLatchedExecs (merged_bug_011: flush-time
    # re-derivation + exec-scoped close), (3) the tenure-lifecycle
    # plane — failedRecoveryNeverServes (bug_155: completion only
    # through the recovered witness; failed tenures step down). All
    # three planes are LIVE here (ENABLE_* = true in fencedWritesT1):
    # this run is the baseline-hold pair of the three plane twins
    # below; the four legacy calibrations bind the planes false and
    # keep their state space unchanged.
    # r[verify sched.evidence.durability+4]
    # r[verify sched.lease.fence-statement-guard]
    # r[verify sched.grpc.fence-retryable]
    # r[verify sched.lease.tenure-stamp-type]
    # r[verify sched.recovery.step-down+3]
    # r[verify sched.attempt.cancel-close-driven+1]
    quint-fenced-writes = mkQuintCheck {
      name = "fenced-writes";
      spec = "fencedWrites";
      main = "fencedWritesT1";
      invariants = [ "fencedWritesAll" ];
    };
    quint-fence-calib-338-atomic-reread = mkQuintWitnessCheck {
      name = "fence-calib-338-atomic-reread";
      spec = "calibration/fence-338-atomic-reread";
      main = "fenceCalib338AtomicReread";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "writesCarryClaimedTenure";
    };
    quint-fence-calib-011-absolute-replay = mkQuintWitnessCheck {
      name = "fence-calib-011-absolute-replay";
      spec = "calibration/fence-011-absolute-replay";
      main = "fenceCalib011AbsoluteReplay";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "outboxReplayNeverRegresses";
    };
    quint-fence-calib-011-foreign-close = mkQuintWitnessCheck {
      name = "fence-calib-011-foreign-close";
      spec = "calibration/fence-011-absolute-replay";
      main = "fenceCalib011AbsoluteReplay";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "outboxClosesOnlyLatchedExecs";
    };
    quint-fence-calib-155-serve-after-failed-recovery = mkQuintWitnessCheck {
      name = "fence-calib-155-serve-after-failed-recovery";
      spec = "calibration/fence-155-serve-after-failed-recovery";
      main = "fenceCalib155ServeAfterFailedRecovery";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "failedRecoveryNeverServes";
    };
    quint-fence-calib-261-unguarded-upsert = mkQuintWitnessCheck {
      name = "fence-calib-261-unguarded-upsert";
      spec = "calibration/fence-261-unguarded-upsert";
      main = "fenceCalib261UnguardedUpsert";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "activeRowGenMonotonic";
    };
    quint-fence-calib-231-unfenced-close = mkQuintWitnessCheck {
      name = "fence-calib-231-unfenced-close";
      spec = "calibration/fence-231-unfenced-close";
      main = "fenceCalib231UnfencedClose";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "openAttemptViewStableUnderDeposedClose";
    };
    quint-fence-calib-273-plain-floor-set = mkQuintWitnessCheck {
      name = "fence-calib-273-plain-floor-set";
      spec = "calibration/fence-273-plain-floor-set";
      main = "fenceCalib273PlainFloorSet";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "resourceFloorMonotonic";
    };
    quint-fence-calib-393-terminal-refusal = mkQuintWitnessCheck {
      name = "fence-calib-393-terminal-refusal";
      spec = "calibration/fence-393-terminal-refusal";
      main = "fenceCalib393TerminalRefusal";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "fenceRefusalAlwaysRetryable";
    };
    # bughunt-2 bug_358: belowFloorTxNeverMutates' FIRST falsifier — the
    # admission compare dropped entirely; the snapshotExceedsGen oracle
    # (computed in the live model's upsertApply seat, never by the
    # calibration) latches the below-floor mutation.
    quint-fence-calib-floor-blind = mkQuintWitnessCheck {
      name = "fence-calib-floor-blind";
      spec = "calibration/fence-floor-blind";
      main = "fenceCalibFloorBlind";
      extraSpecs = [ "fencedWrites" ];
      step = "calibStep";
      witness = "belowFloorTxNeverMutates";
    };

    # ------------------------------------------------------------------
    # spawnCoherence PD-7 extension (substitution-replacement Phase B,
    # design §9.2): the GetSpawnIntents job filter — served sets
    # exclude intents whose node carries an unresolved materialization
    # job. The six pre-existing regimes carry ENABLE_MAT_JOBS = false
    # (constant-false var; reachable state space unchanged — the
    # regime-split dormancy rule); this regime turns the environment
    # on. Weakened test: deleting the serve-time conjunct re-finds
    # VMatJobServed (procedure in the regime module's comment; depth in
    # the introducing commit).
    # r[verify sched.materialize.job+2]
    quint-spawn-coherence-mat-jobs = mkQuintCheck {
      name = "spawn-coherence-mat-jobs";
      spec = "spawnCoherence";
      main = "spawnCoherenceMatJobs";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
        "matJobFilteredFromIntents"
      ];
    };
    # The job filter actually filters (a queued, job-carrying intent
    # existed at a successful fresh poll) — matJobFilteredFromIntents'
    # non-vacuity.
    quint-spawn-coherence-witness-mat-filtered = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-mat-filtered";
      spec = "spawnCoherence";
      main = "spawnCoherenceMatJobs";
      witness = "canReachMatJobFiltered";
    };

    # Bughunt-wave A2 regime (bug_106): the rendered-deadline carry —
    # execGetsWork carries the spawn-rendered deadline into the binding
    # and the establishment sweep anchors to it (never a shrunk
    # mint-time re-solve). Axis-gated per §4.F16: every other regime
    # binds ENABLE_RENDERED_DEADLINE = false and is state-space
    # identical.
    quint-spawn-coherence-rendered-deadline = mkQuintCheck {
      name = "spawn-coherence-rendered-deadline";
      spec = "spawnCoherence";
      main = "spawnCoherenceRenderedDeadline";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
      ];
    };
    # The establishment actually fires under the rendered-deadline
    # regime — establishedOnlyPastRenderedDeadline's non-vacuity.
    quint-spawn-coherence-witness-established = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-established";
      spec = "spawnCoherence";
      main = "spawnCoherenceRenderedDeadline";
      witness = "canReachEstablished";
    };
    # A3/282 axis regime (bughunt wave; §4.F16 gating — every other
    # regime binds ENABLE_RETRY_BACKOFF=false and stays
    # state-space-identical, re-measured bit-exact at
    # 36,084,961/2,013,280 for spawnCoherenceBase): the spawn edge
    # refuses in-backoff intents. Exhaustive [ok] at
    # 82,701,121/4,529,880 in ~25s. Falsifiability pair:
    # quint-controller-calib-282-backoff-ignored.
    quint-spawn-coherence-retry-backoff = mkQuintCheck {
      name = "spawn-coherence-retry-backoff";
      spec = "spawnCoherence";
      main = "spawnCoherenceRetryBackoff";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "establishedOnlyPastRenderedDeadline"
        "noSpawnIntentInsideBackoff"
      ];
    };
    # The 282 pre-fix flip: intentArrives without the backoff conjunct
    # (TLC first violation ~1.1s).
    quint-controller-calib-282-backoff-ignored = mkQuintWitnessCheck {
      name = "controller-calib-282-backoff-ignored";
      spec = "calibration/controller-282-backoff-ignored";
      main = "ctrlCalib282BackoffIgnored";
      extraSpecs = [ "spawnCoherence" ];
      step = "calibStep";
      witness = "noSpawnIntentInsideBackoff";
    };

    # ------------------------------------------------------------------
    # Bughunt-wave C2 axes (areas A/C/D on Model J, areas E/F/G on the
    # NodeClaim lifecycle): each fix is wired as a falsify/hold PAIR per
    # the wave discipline — the as-built regime must keep falsifying the
    # invariant (the bug stays expressible and found), the fixed-law
    # regime must hold it, and a reachability witness keeps the hold
    # non-vacuous. All pre-C2 regimes bind the axes false and are
    # state-space identical (§4.F16). Local sampled measurements
    # (quint run, rust backend) recorded in the introducing commit.

    # C2/077 (area A): the AD2 fleet-exhaust verdict fires only at the
    # third consecutive exhausted-and-wanted tick; a placeable tick
    # resets the streak.
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-falsify-exhaust-asbuilt = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-exhaust-asbuilt";
      spec = "spawnCoherence";
      main = "spawnCoherenceExhaustAsBuilt";
      witness = "noPoisonWhilePlaceable";
    };
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-exhaust-persist = mkQuintCheck {
      name = "spawn-coherence-exhaust-persist";
      spec = "spawnCoherence";
      main = "spawnCoherenceExhaustPersist";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "noPoisonWhilePlaceable"
      ];
    };
    quint-spawn-coherence-witness-poison = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-poison";
      spec = "spawnCoherence";
      main = "spawnCoherenceExhaustPersist";
      witness = "canReachPoison";
    };

    # C2/120 (area C): the AD5 cancel arm selects on CLOSE CAUSE — a
    # CANCELLED entry in the recently_closed window with no covering
    # open attempt — never on the absence of an open row.
    # r[verify ctrl.job.cancel-close-cause+2]
    quint-spawn-coherence-falsify-cancel-asbuilt = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-cancel-asbuilt";
      spec = "spawnCoherence";
      main = "spawnCoherenceCancelAsBuilt";
      witness = "cancelArmDeletesOnlyCancelled";
    };
    # r[verify ctrl.job.cancel-close-cause+2]
    quint-spawn-coherence-cancel-cause = mkQuintCheck {
      name = "spawn-coherence-cancel-cause";
      spec = "spawnCoherence";
      main = "spawnCoherenceCancelCause";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "cancelArmDeletesOnlyCancelled"
      ];
    };
    quint-spawn-coherence-witness-cancel-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-cancel-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceCancelCause";
      witness = "canReachCancelReap";
    };

    # C2/221 (area C): the orphan reap acts on the absence of an open
    # attempt only once the serving leader is older than the grace —
    # the never-pulled cohort gets one full grace against a NEW leader.
    # The HOLD half rides the existing fault-lease regime (the gate
    # forbids exactly the falsified trace there).
    # r[verify ctrl.job.orphan-leader-age]
    quint-spawn-coherence-falsify-young-leader-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-young-leader-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceReapYoungLeader";
      witness = "noReapOfNeverPulledBeforeLeaderAged";
    };
    # r[verify ctrl.job.orphan-leader-age]
    # r[verify ctrl.ephemeral.reap-orphan-running+5]
    quint-spawn-coherence-leader-age-hold = mkQuintCheck {
      name = "spawn-coherence-leader-age-hold";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultLease";
      invariants = [ "noReapOfNeverPulledBeforeLeaderAged" ];
    };

    # ── merged_bug_117 + bug_113 axes (bughunt-2 slot 5) ──────────────
    # merged_bug_117 FALSIFY halves: the pool-SHARED streak map under a
    # second reconciling pool (poolBTick). Wipe law: B's retain resets
    # A's live streaks — the poison report is livelocked (rust-sim seed
    # 0xa108dd26025cd507). Own-count law: an overlap intent
    # double-steps to the threshold on 2 own observations (rust-sim
    # seed 0xdf6e64671329a316).
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-falsify-multipool-wipe = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-multipool-wipe";
      spec = "spawnCoherence";
      main = "spawnCoherenceMultiPoolAsBuilt";
      witness = "persistentExhaustionEventuallyReports";
    };
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-falsify-multipool-own-count = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-multipool-own-count";
      spec = "spawnCoherence";
      main = "spawnCoherenceMultiPoolAsBuilt";
      witness = "streakCountsOwnObservations";
    };
    # merged_bug_117 HOLD half: PoolStreaks pool-keying — B's tick can
    # neither wipe nor advance A's streaks; the poison verdict counts
    # the observing pool's own 3 observations. canReachPoison keeps the
    # threshold path non-vacuous under the multi-pool alphabet.
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-multipool-hold = mkQuintCheck {
      name = "spawn-coherence-multipool-hold";
      spec = "spawnCoherence";
      main = "spawnCoherenceMultiPool";
      invariants = [
        "streakCountsOwnObservations"
        "persistentExhaustionEventuallyReports"
        "noPoisonWhilePlaceable"
      ];
    };
    quint-spawn-coherence-witness-multipool-poison = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-multipool-poison";
      spec = "spawnCoherence";
      main = "spawnCoherenceMultiPool";
      witness = "canReachPoison";
    };

    # m073 cadence floor (bughunt-4 S1a): the poison verdict spans at
    # least STREAK_FLOOR wall units from the streak's first gated
    # observation — a sub-second reconcile burst can step the streak
    # to the threshold but can never fire the verdict. The regime
    # rides the exhaust-persist alphabet; the merged_bug_117
    # own-count/wipe laws keep their own exhaustive regime
    # (spawn-coherence-multipool-hold) — the MULTI_POOL x CADENCE
    # product is TLC-infeasible (2,347,503,711 gen / 108,632,544
    # distinct / queue growing at the 1800s kill), and the floor
    # predicate is per-intent over the own-pool streak either way.
    # Measured (TLC exhaustive, gating backend, yensid):
    # 250,483,073 generated / 11,627,904 distinct / 0 queue in
    # 118.6s — budget default 1800s = 15x headroom. The fold-skip
    # edge is pinned to the floor-blind world (see the model's
    # nondet comment: monotone-safe for the floor law; the twin
    # exercises it).
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-streak-cadence = mkQuintCheck {
      name = "spawn-coherence-streak-cadence";
      spec = "spawnCoherence";
      main = "spawnCoherenceStreakCadence";
      invariants = [
        "poisonRespectsWallFloor"
        "noPoisonWhilePlaceable"
      ];
    };
    # canReachPoison keeps the threshold-past-the-floor path
    # non-vacuous under the cadence alphabet (the verdict is reachable
    # once the wall has advanced STREAK_FLOOR units past the streak's
    # birth — enforcement delays it, never deletes it).
    quint-spawn-coherence-witness-cadence-poison = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-cadence-poison";
      spec = "spawnCoherence";
      main = "spawnCoherenceStreakCadence";
      witness = "canReachPoison";
    };
    # m073 FALSIFY half: the as-built floor-blind verdict (clock
    # tracked, not enforced) fires on a burst — VPoisonBurst latches
    # and poisonRespectsWallFloor is violated.
    # r[verify ctrl.pool.no-eligible-persist+4]
    quint-spawn-coherence-calib-073-burst = mkQuintWitnessCheck {
      name = "spawn-coherence-calib-073-burst";
      spec = "calibration/controller-073-burst";
      main = "controller073Burst";
      extraSpecs = [ "spawnCoherence" ];
      witness = "poisonRespectsWallFloor";
    };

    # bug_113 FALSIFY half: cancel + fast re-submit respawns the
    # deterministic Job name inside the recently_closed window; the
    # cause-only law cancel-selects the fresh Job. Live-import calib
    # with a MID-TRACE init (the cold-init prefix is ~10 ordered steps
    # — 60k samples x 20 steps found nothing; from calibInit the
    # violation is 2 ticks, rust-sim seed 0x1819714b2c47fa51).
    # r[verify ctrl.job.cancel-close-cause+2]
    quint-controller-calib-113-respawn-cancel = mkQuintWitnessCheck {
      name = "controller-calib-113-respawn-cancel";
      spec = "calibration/controller-113-respawn-cancel";
      main = "controllerCalib113RespawnCancel";
      extraSpecs = [ "spawnCoherence" ];
      init = "calibInit";
      step = "calibStep";
      witness = "cancelNeverDeletesPostCloseJob";
    };
    # bug_113 HOLD half from the SAME mid-trace init: the generation
    # conjunct makes the respawned Job structurally unselectable
    # (cancelArmDeletesOnlyCancelled retained alongside).
    # r[verify ctrl.job.cancel-close-cause+2]
    quint-controller-calib-113-respawn-cancel-hold = mkQuintCheck {
      name = "controller-calib-113-respawn-cancel-hold";
      spec = "calibration/controller-113-respawn-cancel";
      main = "controllerCalib113RespawnCancelHold";
      extraSpecs = [ "spawnCoherence" ];
      init = "calibInit";
      step = "calibStep";
      invariants = [
        "cancelNeverDeletesPostCloseJob"
        "cancelArmDeletesOnlyCancelled"
      ];
    };

    # ── wedgeCluster: the OA2 wedge-clustering verdict after the
    # bughunt-2 slot-5 rework (merged_bug_009 commensurable populations
    # + RPC-failure skip, merged_bug_176 sealed single-exit epilogue,
    # the required eviction argument) and the bughunt-3 S7 episode
    # latch (merged_bug_163). The main regime is TLC-EXHAUSTIVE at
    # these bounds with the episode ghosts FROZEN (≤3 nodes × 2 drvs:
    # 173,840,769 states generated / 1,024,082 distinct / 0 on queue,
    # 8m11s at workers=auto, no violation; re-measured at the round-4
    # m288 repair: the skip arm now RETAINS the marked set and the
    # lastSkip exemption rides the counter law) — the four round-2 laws
    # hold over the FULL bounded space, not a sampled slice. The latch
    # law (noRemarkFromLatchedEpisode) needs the ghosts LIVE, whose
    # tick-domain product does NOT converge at these bounds (>1.02B
    # generated, frontier still growing at the 1800s kill — the S7
    # landing-gate red): it runs TLC-exhaustively in
    # quint-wedge-cluster-latch below at one-notch-shrunk bounds. Every
    # law is paired with a falsify twin below (no vacuous-invariant
    # debt; boundsOK is the standard bounds-only exemption).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-main = mkQuintCheck {
      name = "wedge-cluster-main";
      spec = "wedgeCluster";
      main = "wedgeClusterMain";
      invariants = [
        "boundsOK"
        "affectedLeOf"
        "noPerNodeFromSuppressedEvidence"
        "reapedImpliesEvicted"
        "markedIncrementsOnlyOnEdges"
      ];
    };

    # The episode-latch regime: ghosts live (admission gating + the
    # latch law) at MAX_TIME=4/WINDOW=2, where the ghost product
    # converges: TLC-exhaustive 209,299,445 states generated /
    # 3,835,651 distinct / 0 on queue, 7m29s at workers=auto
    # (re-measured at the round-4 m288 repair; the skip-retain fix
    # rides this regime too) — budget 1800s ≈ 4× measured. The
    # latch-witness-systemic check below pins the systemic arm
    # reachable at these shrunk bounds, so the latch law cannot go
    # silently vacuous.
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-latch = mkQuintCheck {
      name = "wedge-cluster-latch";
      spec = "wedgeCluster";
      main = "wedgeClusterLatch";
      invariants = [
        "boundsOK"
        "affectedLeOf"
        "noPerNodeFromSuppressedEvidence"
        "reapedImpliesEvicted"
        "markedIncrementsOnlyOnEdges"
        "noRemarkFromLatchedEpisode"
      ];
      modelTimeoutSec = 1800;
    };

    # Falsify twin (live-import, calibration/): the as-built split
    # populations — retained-evidence wedged nodes over the THIS-TICK
    # fleet — emit Systemic{affected: 2, of: 1} within 2 ticks
    # (merged_bug_009; solo-verified [violation] in ~3s).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-split-population = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-split-population";
      spec = "calibration/wedge-009-split-population";
      main = "wedgeCalib009SplitPopulation";
      extraSpecs = [ "wedgeCluster" ];
      witness = "affectedLeOf";
    };

    # Falsify twin (live-import, calibration/): the as-built early
    # return past the Systemic epilogue freezes the marked set against
    # a verdict whose survivor set is empty (merged_bug_176;
    # [violation] at 172,796 generated / 3,953 distinct, ~7s —
    # re-verified at the round-5 retention restatement).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-early-return = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-early-return";
      spec = "calibration/wedge-176-early-return";
      main = "wedgeCalib176EarlyReturn";
      extraSpecs = [ "wedgeCluster" ];
      witness = "markedIncrementsOnlyOnEdges";
    };
    # Same twin, second law: the undrained episode's surviving anchors
    # later build a per-node verdict a suppression already explained
    # ([violation] at 474,362 generated / 8,939 distinct, ~10s —
    # re-verified at the round-5 retention restatement).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-suppressed-evidence = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-suppressed-evidence";
      spec = "calibration/wedge-176-early-return";
      main = "wedgeCalib176EarlyReturn";
      extraSpecs = [ "wedgeCluster" ];
      witness = "noPerNodeFromSuppressedEvidence";
    };

    # Falsify twin (live-import, calibration/): the round-2 as-built
    # episode handling — wedged-only drain, no suppression watermark —
    # lets the trailing-edge laggard re-anchor from the SAME still-open
    # attempts and a sub-threshold participant's surviving anchor pair
    # with a later blip (merged_bug_163; TLC first-violation at
    # 1,203,834 generated / 46,017 distinct). Runs with
    # TRACK_EPISODE_GHOSTS = true — the ghosts carry the violation.
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-partial-drain = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-partial-drain";
      spec = "calibration/wedge-163-partial-drain";
      main = "wedgeCalib163PartialDrain";
      extraSpecs = [ "wedgeCluster" ];
      witness = "noRemarkFromLatchedEpisode";
    };

    # Falsify twin (in-file): no eviction input — a reaped node's
    # pre-reap anchors survive the next update and keep feeding the
    # Dead arm (the REQUIRED-argument rationale; solo-verified
    # [violation] in ~5s).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-falsify-no-eviction = mkQuintWitnessCheck {
      name = "wedge-cluster-falsify-no-eviction";
      spec = "wedgeCluster";
      main = "wedgeClusterNoEviction";
      witness = "reapedImpliesEvicted";
    };

    # merged_bug_024 (round-5): the ghost-admission regime — the ghost
    # node live in the attribution universe, the admission authority's
    # fleet-absence leg ON. Both admission laws TLC-EXHAUSTIVE at
    # MAX_TIME=4/WINDOW=2: 85,729,167 states generated / 138,722
    # distinct / 0 on queue, 5m27s at workers=auto — budget 1800s
    # ≈ 5.5× measured. Ghost-free regimes above are byte-identical
    # to their pre-round-5 spaces (TRACK_GHOST_NODES=false keeps the
    # nondet domain unchanged; the admission filter is an identity over
    # registered-only pairs).
    # r[verify ctrl.nodeclaim.wedge-cluster+3]
    quint-wedge-cluster-admission = mkQuintCheck {
      name = "wedge-cluster-admission";
      spec = "wedgeCluster";
      main = "wedgeClusterAdmission";
      invariants = [
        "boundsOK"
        "affectedLeOf"
        "evidenceWithinRegistered"
        "systemicOfWithinRegisteredUniverse"
      ];
    };
    # Falsify twin (live-import, calibration/): the as-built admission
    # gate never consulted fleet membership — a ghost's first
    # post-expiry tick lands its anchors in evidence ([violation] at
    # 5,117 generated / 229 distinct, ~3s).
    # r[verify ctrl.nodeclaim.wedge-cluster+3]
    quint-wedge-cluster-calib-ghost-evidence = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-ghost-evidence";
      spec = "calibration/wedge-024-ghost-admission";
      main = "wedgeCalib024GhostAdmission";
      extraSpecs = [ "wedgeCluster" ];
      witness = "evidenceWithinRegistered";
    };
    # Same twin, second law: admitted ghosts both wedge and pad the
    # systemic denominator past the registered universe (the
    # false-Systemic shape whose drain+latch blacks out genuine
    # per-node verdicts; [violation] at 772,164 generated / 8,765
    # distinct, ~21s).
    # r[verify ctrl.nodeclaim.wedge-cluster+3]
    quint-wedge-cluster-calib-ghost-denominator = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-ghost-denominator";
      spec = "calibration/wedge-024-ghost-admission";
      main = "wedgeCalib024GhostAdmission";
      extraSpecs = [ "wedgeCluster" ];
      witness = "systemicOfWithinRegisteredUniverse";
    };

    # Reachability witnesses on the main regime: both verdict arms
    # actually fire at these bounds (anti-vacuity for the four laws).
    quint-wedge-cluster-witness-systemic = mkQuintWitnessCheck {
      name = "wedge-cluster-witness-systemic";
      spec = "wedgeCluster";
      main = "wedgeClusterMain";
      witness = "systemicReachableW";
    };
    # Same witness on the latch regime's shrunk bounds: the systemic
    # arm (and with it the latch law's guard) stays reachable at
    # MAX_TIME=4/WINDOW=2 — the bounds-shrink cannot silently vacuate
    # noRemarkFromLatchedEpisode ([violation] at 111,634 generated /
    # 4,920 distinct).
    quint-wedge-cluster-latch-witness-systemic = mkQuintWitnessCheck {
      name = "wedge-cluster-latch-witness-systemic";
      spec = "wedgeCluster";
      main = "wedgeClusterLatch";
      witness = "systemicReachableW";
    };
    quint-wedge-cluster-witness-per-node = mkQuintWitnessCheck {
      name = "wedge-cluster-witness-per-node";
      spec = "wedgeCluster";
      main = "wedgeClusterMain";
      witness = "perNodeReachableW";
    };

    # merged_bug_034 (Q2 SIGNED): the production trajectory regime —
    # fleet denominator + breadth + dwell + per-node withholding,
    # PLUS (round-5, merged_bug_016) the suppressed-retention law:
    # developing-episode ticks retain the marked transition-memory.
    # All eight laws TLC-EXHAUSTIVE at MAX_TIME=4/WINDOW=2:
    # 9,987,163 states generated / 57,968 distinct / 0 on queue, 69s
    # at workers=auto under a parallel sweep — budget 1800s ≈ 26×
    # measured (the round-5 ladder: the retention restatement grew
    # the space from 8.7M/44,111, then the release-edge close pruned
    # it slightly — drained windows collapse states; the episode
    # carriers are frozen off-trajectory, so the other regimes'
    # spaces are byte-identical to their recorded baselines). The dwell-gated systemic arm is
    # reachability-pinned below.
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-trajectory = mkQuintCheck {
      name = "wedge-cluster-trajectory";
      spec = "wedgeCluster";
      main = "wedgeClusterTrajectory";
      invariants = [
        "boundsOK"
        "affectedLeOf"
        "noPerNodeFromSuppressedEvidence"
        "reapedImpliesEvicted"
        "markedIncrementsOnlyOnEdges"
        "systemicDenominatorIsFleet"
        "noSerialReapInDevelopingEpisode"
        "suppressedTickRetainsMarked"
      ];
      modelTimeoutSec = 1800;
    };

    # merged_bug_016 falsify twin (round-5): the as-built drain on
    # suppressed ticks — the developing-episode tick fed an empty
    # survivor set into the shared marked.retain tail, so one
    # continuous wedge re-counted (and re-warned) after every
    # suppressed phase. [violation] of suppressedTickRetainsMarked at
    # the trajectory regime's exact bounds (125,827 generated / 1,990
    # distinct, ~6s).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-suppressed-drain = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-suppressed-drain";
      spec = "calibration/wedge-016-suppressed-drain";
      main = "wedgeCalib016SuppressedDrain";
      extraSpecs = [ "wedgeCluster" ];
      witness = "suppressedTickRetainsMarked";
    };
    # Anti-vacuation: the dwell-gated systemic arm actually fires at
    # the trajectory bounds (raw-trip at t, hold through DWELL_TICKS).
    quint-wedge-cluster-trajectory-witness-systemic = mkQuintWitnessCheck {
      name = "wedge-cluster-trajectory-witness-systemic";
      spec = "wedgeCluster";
      main = "wedgeClusterTrajectory";
      witness = "systemicReachableW";
    };
    # merged_bug_023 anti-vacuity: the release edge (an engaged
    # episode disengaging on an observed tick) is reachable at the
    # trajectory bounds — the close law cannot go silently vacuous
    # ([violation] at 52,248 generated / 1,143 distinct, ~9s).
    quint-wedge-cluster-trajectory-witness-release = mkQuintWitnessCheck {
      name = "wedge-cluster-trajectory-witness-release";
      spec = "wedgeCluster";
      main = "wedgeClusterTrajectory";
      witness = "releaseEdgeReachableW";
    };

    # merged_bug_023 falsify twin (round-5): the as-built third exit —
    # a breadth episode releasing silently with retained evidence,
    # which then wholly mints a per-node verdict against the
    # late-onset node ([violation] of noPerNodeFromSuppressedEvidence
    # at the trajectory bounds; the developing-tick anchors feed the
    # suppression-survivor set, so the law carries the release-edge
    # content it was previously blind to; [violation] at 193,678
    # generated / 2,584 distinct, ~18s).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-open-release = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-open-release";
      spec = "calibration/wedge-023-open-release";
      main = "wedgeCalib023OpenRelease";
      extraSpecs = [ "wedgeCluster" ];
      witness = "noPerNodeFromSuppressedEvidence";
    };

    # merged_bug_034 falsify twin: the retired instantaneous guard at
    # the trajectory regime's exact bounds — both laws [violation]
    # (the lull false-Systemic and the staggered serial reap).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-instantaneous-denominator = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-instantaneous-denominator";
      spec = "calibration/wedge-034-instantaneous";
      main = "wedgeCalib034Instantaneous";
      extraSpecs = [ "wedgeCluster" ];
      witness = "systemicDenominatorIsFleet";
    };
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-calib-instantaneous-serial-reap = mkQuintWitnessCheck {
      name = "wedge-cluster-calib-instantaneous-serial-reap";
      spec = "calibration/wedge-034-instantaneous";
      main = "wedgeCalib034Instantaneous";
      extraSpecs = [ "wedgeCluster" ];
      witness = "noSerialReapInDevelopingEpisode";
    };

    # m288: the epilogue regime (latch off, full epilogue on) — the
    # non-vacuous home of noPerNodeFromSuppressedEvidence: bystander
    # evidence survives the wedged-only drain, so the suppression-
    # survivor set is reachably non-empty ([violation] at 164,692
    # generated / 2,432 distinct, 3.0s — the witness below) and the
    # law carries content. The latch regimes hold it trivially-but-
    # truthfully (whole-episode drain leaves no survivors by design).
    # TLC-EXHAUSTIVE: 242,502,085 states generated / 1,521,281
    # distinct / 0 on queue — byte-identical across the round-5 model
    # extensions (every new var is frozen in this regime). Measured
    # 10m06s at the round-4 baseline, 14m51s solo at the round-5
    # re-measure (busier host), and a 124-kill at 95% frontier under
    # a 23-way parallel TLC batch at the old 1800s budget — the
    # budget is raised to 2700s ≈ 3× the measured solo runtime so
    # gate-level contention cannot starve a correct exhaustive run
    # (the space itself is unchanged; shrinking these bounds would
    # weaken the one regime where this law is non-vacuous).
    # r[verify ctrl.nodeclaim.wedge-two-axis+5]
    quint-wedge-cluster-epilogue = mkQuintCheck {
      name = "wedge-cluster-epilogue";
      spec = "wedgeCluster";
      main = "wedgeClusterEpilogue";
      invariants = [
        "boundsOK"
        "affectedLeOf"
        "noPerNodeFromSuppressedEvidence"
        "reapedImpliesEvicted"
        "markedIncrementsOnlyOnEdges"
      ];
      modelTimeoutSec = 2700;
    };
    # The m288 tautology guard: suppressedAnchors goes non-empty.
    quint-wedge-cluster-epilogue-witness-suppressed = mkQuintWitnessCheck {
      name = "wedge-cluster-epilogue-witness-suppressed";
      spec = "wedgeCluster";
      main = "wedgeClusterEpilogue";
      witness = "suppressedReachableW";
    };

    # C2/135 (area D): a synthesized close consumes only the attempt
    # the controller observed open at decision time — the scheduler
    # refuses an exec-pinned close whose attempt is no longer the open
    # one; newest-open-wins resolution for synthesized verdicts is the
    # falsified as-built law.
    # r[verify sched.attempt.synthesized-verdict+3]
    quint-spawn-coherence-falsify-synth-close-asbuilt = mkQuintWitnessCheck {
      name = "spawn-coherence-falsify-synth-close-asbuilt";
      spec = "spawnCoherence";
      main = "spawnCoherenceSynthCloseAsBuilt";
      witness = "closeTargetsIssuedAttempt";
    };
    # r[verify sched.attempt.synthesized-verdict+3]
    # r[verify ctrl.drain.disruption-target+4]
    quint-spawn-coherence-synth-close-pinned = mkQuintCheck {
      name = "spawn-coherence-synth-close-pinned";
      spec = "spawnCoherence";
      main = "spawnCoherenceSynthClosePinned";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "closeTargetsIssuedAttempt"
      ];
    };
    quint-spawn-coherence-witness-synth-close = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-synth-close";
      spec = "spawnCoherence";
      main = "spawnCoherenceSynthClosePinned";
      witness = "canReachSynthClose";
    };

    # C2/346 (area G): the lease-acquire edge is an epoch token — the
    # amplify-class prev_idle clear fires once per acquisition, never
    # once per reload-Err tick.
    # r[verify ctrl.nodeclaim.acquire-edge-token+1]
    quint-nodeclaim-falsify-epoch-asbuilt = mkQuintWitnessCheck {
      name = "nodeclaim-falsify-epoch-asbuilt";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleEpochAsBuilt";
      witness = "idleSpellSurvivesReloadErr";
    };
    # r[verify ctrl.nodeclaim.acquire-edge-token+1]
    # r[verify ctrl.nodeclaim.lease-edge-polarity+4]
    quint-nodeclaim-epoch = mkQuintCheck {
      name = "nodeclaim-epoch";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleEpoch";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
        "idleSpellSurvivesReloadErr"
      ];
    };
    quint-nodeclaim-witness-edge-idle-clear = mkQuintWitnessCheck {
      name = "nodeclaim-witness-edge-idle-clear";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleEpoch";
      witness = "canReachEdgeIdleClear";
    };

    # C2/007 (area F): scheduler-bound ICE-clears produced by ticks
    # that cannot deliver them are buffered, never discarded — the
    # producing Registered edge is consume-once.
    # r[verify ctrl.nodeclaim.evidence-buffered]
    quint-nodeclaim-falsify-clear-dropped-asbuilt = mkQuintWitnessCheck {
      name = "nodeclaim-falsify-clear-dropped-asbuilt";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleClearBufferAsBuilt";
      witness = "iceClearDelivered";
    };
    # r[verify ctrl.nodeclaim.evidence-buffered]
    # r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
    # r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    quint-nodeclaim-clear-buffer = mkQuintCheck {
      name = "nodeclaim-clear-buffer";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleClearBuffer";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
        "iceClearDelivered"
        # merged_bug_045 (bughunt-2 slot 5): commit-on-Ack — the
        # buffered batch survives the delivering tick's ack failure
        # (ENABLE_ACK_LATCH = true in this regime since the rework).
        "clearSurvivesAckFailure"
      ];
    };

    # merged_bug_045 FALSIFY half: the as-built mem::take before the
    # RPC loses the batch on exactly the delivering tick's ack failure
    # (the retired 007 map residual, demonstrated as a model
    # violation; rust-sim seed 0x603e2a44398a939e).
    # r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    quint-nodeclaim-falsify-ack-latch-asbuilt = mkQuintWitnessCheck {
      name = "nodeclaim-falsify-ack-latch-asbuilt";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleAckLatchAsBuilt";
      witness = "clearSurvivesAckFailure";
    };
    quint-nodeclaim-witness-buffered-clear = mkQuintWitnessCheck {
      name = "nodeclaim-witness-buffered-clear";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleClearBuffer";
      witness = "canReachBufferedClearDelivered";
    };

    # C2/285 (area E): the binding snapshot is presence-preserving —
    # every full-tick ack carries the bound set; present-and-empty
    # CLEARS the scheduler's map (scale-to-zero says so).
    # r[verify sched.snapshot.binding-presence]
    quint-nodeclaim-falsify-snapshot-asbuilt = mkQuintWitnessCheck {
      name = "nodeclaim-falsify-snapshot-asbuilt";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleSnapshotAsBuilt";
      witness = "ackCarriesSnapshot";
    };
    # r[verify sched.snapshot.binding-presence]
    quint-nodeclaim-snapshot-presence = mkQuintCheck {
      name = "nodeclaim-snapshot-presence";
      # quint-policy P1 exemption (bughunt-2 slot 11; the §5-Q13 census
      # is the burn-down artifact):
      vacuityExempt = {
        boundsOK = {
          class = "boundsOK";
          reason = "scope-ceiling tripwire: a violation means the regime misconfigured its bound consts, not a protocol defect — a falsifier would assert a misconfiguration, not a behavior";
        };
      };
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleSnapshot";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
        "ackCarriesSnapshot"
      ];
    };
    quint-nodeclaim-witness-binding-cleared = mkQuintWitnessCheck {
      name = "nodeclaim-witness-binding-cleared";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleSnapshot";
      witness = "canReachBindingCleared";
    };

    # ------------------------------------------------------------------
    # ICE evidence/ack pipeline (bughunt-5 slot 5: bug_094 +
    # merged_bug_134/008/003) — the cross-component contract
    # nodeclaimLifecycle.qnt assumes away ("Scheduler ICE ladder
    # (peer)"): controller per-cell ordered evidence buffer + epoch
    # mint, lossy ack channel (Ok / Ok-lost / refused /
    # late-duplicate), scheduler ladder + per-cell epoch gate + the
    # §13a local clear (docs/spec/models/iceEvidenceAck.qnt). The
    # holds check is EXHAUSTIVE (tlc; the reconciler tick-atomicity
    # guard keeps the space small — ~2s measured); each holds
    # invariant has its falsify twin at the matching sim budget
    # (maxSamples sized 25/p from the recorded traces-to-first-hit;
    # measurements in the introducing commit).
    # ------------------------------------------------------------------

    # r[verify sched.sla.ack-validate-then-commit]
    # r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    # r[verify ctrl.nodeclaim.ice-mark-clear+2]
    quint-ice-evidence-ack = mkQuintCheck {
      name = "ice-evidence-ack";
      spec = "iceEvidenceAck";
      main = "iceEvidenceAckBase";
      invariants = [
        "errImpliesNoMutation"
        "redeliveryIdempotent"
        "clearThenMarkRealizesReset"
        "healthyCellNeedsNewEvidence"
      ];
    };

    # merged_bug_008 headline axis FALSIFY half (+ bug_094 ordering):
    # the as-built apply-then-CostGateClosed order — planes mutate,
    # then the refusal answers; the retained-buffer redelivery
    # re-applies every tick.
    # r[verify sched.sla.ack-validate-then-commit]
    quint-ice-falsify-apply-before-refuse = mkQuintSimWitnessCheck {
      name = "ice-falsify-apply-before-refuse";
      spec = "calibration/ice-apply-before-refuse";
      main = "iceCalibApplyBeforeRefuse";
      extraSpecs = [ "iceEvidenceAck" ];
      step = "calibStep";
      witness = "errImpliesNoMutation";
      maxSamples = 1000000;
      maxSteps = 16;
    };

    # merged_bug_008 redelivery axes FALSIFY half: no epoch gate —
    # stale marks re-stamp (pinned mask), post-expiry redelivery
    # climbs (phantom failure), late duplicates re-apply.
    # r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    quint-ice-falsify-no-epoch-gate = mkQuintSimWitnessCheck {
      name = "ice-falsify-no-epoch-gate";
      spec = "calibration/ice-no-epoch-gate";
      main = "iceCalibNoEpochGate";
      extraSpecs = [ "iceEvidenceAck" ];
      step = "calibStep";
      witness = "redeliveryIdempotent";
      maxSamples = 1000000;
      maxSteps = 16;
    };

    # merged_bug_008 axis-3 FALSIFY half on the same no-gate main: a
    # redelivered retained mark after the §13a local clear re-masks
    # the proven-healthy cell (the inverted clear; P1 pairing for the
    # fourth holds invariant).
    # r[verify ctrl.nodeclaim.evidence-ack-latch+3]
    quint-ice-falsify-remask-after-clear = mkQuintSimWitnessCheck {
      name = "ice-falsify-remask-after-clear";
      spec = "calibration/ice-no-epoch-gate";
      main = "iceCalibNoEpochGate";
      extraSpecs = [ "iceEvidenceAck" ];
      step = "calibStep";
      witness = "healthyCellNeedsNewEvidence";
      maxSamples = 1000000;
      maxSteps = 16;
    };

    # merged_bug_003 FALSIFY half: latest-wins eviction — a newer
    # mark destroys the buffered clear; the mark-only request climbs
    # from the stale rung instead of reset-then-step-0.
    # r[verify ctrl.nodeclaim.ice-mark-clear+2]
    quint-ice-falsify-latest-wins-eviction = mkQuintSimWitnessCheck {
      name = "ice-falsify-latest-wins-eviction";
      spec = "calibration/ice-latest-wins-eviction";
      main = "iceCalibLatestWinsEviction";
      extraSpecs = [ "iceEvidenceAck" ];
      step = "calibStep";
      witness = "clearThenMarkRealizesReset";
      maxSamples = 1000000;
      maxSteps = 16;
    };

    # ------------------------------------------------------------------
    # Gateway connection/session lifecycle campaign (gw-session-formal,
    # round-2 Track B), Phase 0 Stage C: the rio-gateway accept → auth →
    # channel open → exec admission → protocol session → teardown
    # lifecycle as built — capacity permits and gauges, deadlines and
    # keepalives, force-close arming/enforcement, the cancel-on-disconnect
    # obligations, egress pacing and the three-stage drain — modeled
    # against an explicit russh transport environment
    # (docs/spec/models/gwConnLifecycle.qnt). The invariant ↔ spec-rule
    # map, the Stage-B B-measure, the Stage-C check-set decision and the
    # per-check verdicts live in
    # docs/spec/models/gw-session-invariant-map.md.
    #
    # Check-set shape (the §2e pre-registered fallback ladder, applied at
    # the Stage-B measurement milestone): the full-alphabet regimes do not
    # exhaust inside the per-check budget (the base regime alone is in the
    # tens of millions of distinct states, and fallback (2) shrinks it
    # only ≈2×), so the merge-gated EXHAUSTIVE checks are the per-family
    # restricted-alphabet modules (§2e fallbacks (3)/(4): every structural
    # and environment bound intact, conn B at connection level, one corpus
    # family's letters per check, asserting the full 34-property
    # `allInvariants` conjunction over that family's reachable space).
    # The full-alphabet regime modules carry the witness, expect-violation
    # and named-run checks below — those stop at the first violation, so
    # they stay cheap on the unrestricted alphabets. Which cross-family
    # interleavings are therefore NOT exhaustively explored is recorded
    # per property in the invariant map's Stage-C record.
    # ------------------------------------------------------------------

    # Pre-auth establishment / occupancy family (corpus F2/F3 pre-auth
    # half, GW-3/GW-4): the ConnStage machine, accept-time conn permits,
    # the gauge latch on the first auth callback (accept and reject
    # outcomes), silent / KEX-parked / never-auth peers, the two-phase
    # pre-auth deadline, the empty grace on idle authenticated
    # connections, decide-implies-arm at both polite-disconnect sites,
    # force-close enforcement, and every designed reap letter — at the
    # connection level (the admission interplay is the next check's).
    # r[verify gw.conn.lifecycle]
    # r[verify gw.conn.real-connection-marker]
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.keepalive+2]
    quint-gw-lifecycle-fam-preauth = mkQuintCheck {
      name = "gw-lifecycle-fam-preauth";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamPreauth";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Admission family (corpus F2 post-auth half, the GW-18 admission
    # surface): exec admission against the empty-connection grace (the
    # arm/disarm/re-arm cycle, the documented W3/P4 admit-vs-fire race),
    # the per-session handshake and opcode-idle deadlines against
    # occupancy-withholding peers, re-admission after the session count
    # touches zero, and channel opens not counting as activity — on conn
    # A's two channels with the egress/finish machinery abstracted to
    # channel-close release (famWedge / famUpstream own those paths).
    # r[verify gw.conn.lifecycle]
    # r[verify gw.conn.exit-status+3]
    # r[verify gw.conn.exec-request]
    # r[verify gw.handshake.timeout]
    quint-gw-lifecycle-fam-admission = mkQuintCheck {
      name = "gw-lifecycle-fam-admission";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamAdmission";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Connection-cap family (GW-12 surface): MAX_CONNECTIONS dialed to 1
    # (the §2e cap-regime value) so the over-cap permit-None path — the
    # connection that reaches its first auth callback permit-less, is
    # counted there, and is torn down instead of authenticating — is
    # exhaustively explored with two modeled connections.
    # r[verify gw.conn.cap]
    # r[verify gw.conn.real-connection-marker]
    quint-gw-lifecycle-fam-cap = mkQuintCheck {
      name = "gw-lifecycle-fam-cap";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamCap";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Egress-pacing / wedge family (corpus F5, GW-2/GW-8, the W5 vanish
    # reap): window-credited sends, the send and close-out budgets firing
    # only against transport-withholding peers, capacity release before
    # close-out, the close-out ordering, queue parks against non-reading
    # peers, stall → force-close arming, and the designed transport reaps
    # (keepalive / TCP_USER_TIMEOUT) for vanished peers — exhaustive for
    # one egress session on conn A (the wedge candidates are
    # single-session shapes; the multi-session content lives in
    # famUpstream/famHostile and the full-regime witnesses). Conn B is
    # the compliant control for the P5 clauses.
    # r[verify gw.conn.send-deadline]
    # r[verify gw.conn.session-cap+2]
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.exit-status+3]
    quint-gw-lifecycle-fam-wedge = mkQuintCheck {
      name = "gw-lifecycle-fam-wedge";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamWedge";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Channel-accounting / hostile-open family (corpus F7, GW-5/GW-6/GW-7):
    # forged and duplicate closes are ignored, over-bound and non-session
    # opens terminate the connection (russh residue stays bounded), burst
    # opens hit the per-connection bound, the write half is consumed by at
    # most one exec, the open/close bookkeeping never goes negative, and
    # capacity exhaustion is signaled by exec rejection only (conn B opens
    # its channel and takes the session-cap rejection — the P3 surface).
    # r[verify gw.conn.channel-limit+4]
    # r[verify gw.conn.channel-types]
    # r[verify gw.conn.per-channel-state]
    # r[verify gw.conn.exec-request]
    # r[verify gw.conn.session-cap+2]
    quint-gw-lifecycle-fam-hostile = mkQuintCheck {
      name = "gw-lifecycle-fam-hostile";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamHostile";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Upstream / teardown-obligation family (corpus F6/F9, GW-9/GW-10/
    # GW-16/GW-19; the W10/W2 surface): the rpc-wait deadline (and the
    # deliberately deadline-free build-event-wait), terminal vs non-Wire
    # build-stream outcomes against the shipped tracking policy, the
    # cancel loop on every non-panic exit, the upstream-stream release at
    # session exit, the close-out ladder, and transient accept errors.
    # Also asserts w10TriggerAbsent — the §4 W10 decision-rule trigger (a
    # build leaving the tracked set with no terminal outcome, no
    # CancelBuild attempt and the upstream stream still held after session
    # exit) is unreachable; the owner's W10 sign-off consumes this
    # together with the s16-terminal-only falsification trace below.
    # r[verify gw.conn.cancel-on-disconnect+3]
    # r[verify gw.store.transient-retry]
    # r[verify gw.conn.accept-resilience]
    # r[verify gw.conn.exit-status+3]
    quint-gw-lifecycle-fam-upstream = mkQuintCheck {
      name = "gw-lifecycle-fam-upstream";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamUpstream";
      step = "famStep";
      invariants = [
        "allInvariants"
        "w10TriggerAbsent"
      ];
    };

    # Drain family (corpus F10, GW-13): SIGTERM at any point, the
    # NOT_SERVING → accept-stop → session-drain-expiry → exit staging,
    # accept-stop terminating no established connection or session, the
    # drain-expiry shutdown-token cancel of a tracked build, and exit only
    # at full quiescence.
    # r[verify gw.drain.three-stage]
    # r[verify gw.conn.session-drain]
    quint-gw-lifecycle-fam-drain = mkQuintCheck {
      name = "gw-lifecycle-fam-drain";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamDrain";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Degraded-tier family (the §2e fault-degraded surface): the exec-gate
    # panic splitter and proto-task panics (S4's leak term, S8's
    # no-stranded-permit clause, S10's panic carve-out), the
    # TCP_USER_TIMEOUT setsockopt failure, write parks ordered against
    # force-close arming (the W7 ordering bit), and the inactivity reap
    # backstop that exists only in this regime's alphabet.
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.exec-request]
    # r[verify gw.conn.keepalive+2]
    quint-gw-lifecycle-fam-degraded = mkQuintCheck {
      name = "gw-lifecycle-fam-degraded";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamDegraded";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Non-vacuity witnesses for the gateway lifecycle model, on the
    # FULL-alphabet regime modules (they stop at the first violation, so
    # the unrestricted alphabets stay affordable). Each check passes only
    # when the checker still reaches the contended scenario; one that
    # stops violating means the regime's invariants have gone vacuous for
    # it. Deliberately no tracey markers here (same policy as the other
    # models' witnesses).
    quint-gw-lifecycle-witness-server-side-release = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-server-side-release";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachServerSideEndingReleasesEarly";
    };
    quint-gw-lifecycle-witness-grace-fires-idle = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-grace-fires-idle";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachGraceFiresOnIdleConn";
    };
    quint-gw-lifecycle-witness-exec-within-grace = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-exec-within-grace";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachExecWithinGraceSurvives";
    };
    quint-gw-lifecycle-witness-session-cap-rejected = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-session-cap-rejected";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachSessionCapExecRejected";
    };
    quint-gw-lifecycle-witness-mux-touch-zero-exec = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-mux-touch-zero-exec";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachMuxTouchZeroThenExec";
    };
    quint-gw-lifecycle-witness-mux-sibling-quiescence = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-mux-sibling-quiescence";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachMuxSiblingQuiescence";
    };
    quint-gw-lifecycle-witness-close-out-order = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-close-out-order";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachCloseOutCompletesInOrder";
    };
    quint-gw-lifecycle-witness-over-cap-auth-torn = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-over-cap-auth-torn";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleCap";
      witness = "canReachOverCapAuthTorn";
    };
    quint-gw-lifecycle-witness-kex-parked-reclaimed = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-kex-parked-reclaimed";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultOccupancy";
      witness = "canReachKexParkedReclaimed";
    };
    quint-gw-lifecycle-witness-stall-arms-force-close = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-stall-arms-force-close";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachStallArmsForceClose";
    };
    quint-gw-lifecycle-witness-forged-close-ignored = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-forged-close-ignored";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachForgedCloseIgnored";
    };
    quint-gw-lifecycle-witness-over-bound-open = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-over-bound-open";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachOverBoundOpenTerminates";
    };
    quint-gw-lifecycle-witness-burst-hits-bound = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-burst-hits-bound";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachBurstHitsBound";
    };
    quint-gw-lifecycle-witness-vanish-reclaimed = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-vanish-reclaimed";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachVanishReclaimedDesigned";
    };
    quint-gw-lifecycle-witness-nonwire-removes-tracked = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-nonwire-removes-tracked";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultUpstream";
      witness = "canReachNonWireRemovesTracked";
    };
    quint-gw-lifecycle-witness-drain-expiry-cancel = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-drain-expiry-cancel";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDrain";
      witness = "canReachDrainExpiryCancel";
    };
    quint-gw-lifecycle-witness-parked-inactivity-reclaim = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-parked-inactivity-reclaim";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "canReachParkedReclaimedByInactivity";
    };

    # Pre-registered expected as-built falsifications (design §3/§4/§6,
    # the invariant map's probe table): each strict variant is EXPECTED to
    # be violated by today's code, and the check passes only while the
    # counterexample still materializes — a probe that stops falsifying
    # after a code or model change is a finding (the documented trade-off
    # moved), not a pass. The traces these capture are the model-first
    # evidence behind the §4 W3/W7/W10/P12 dispositions. No tracey
    # markers (the spec rules are verified by the HOLD checks above).
    quint-gw-lifecycle-falsification-l5-no-carve-out = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-l5-no-carve-out";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "l5StrictNoCarveOut";
    };
    quint-gw-lifecycle-falsification-s16-terminal-only = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-s16-terminal-only";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultUpstream";
      witness = "s16StrictTerminalOnly";
    };
    quint-gw-lifecycle-falsification-s10-panic = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-s10-panic";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "s10StrictIncludingPanic";
    };
    quint-gw-lifecycle-falsification-l1-no-inactivity = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-l1-no-inactivity";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "l1StrictNoInactivity";
    };

    # The two deterministic named runs: the compliant peer's full
    # lifecycle (the P5 completion half — no budget, force-close or reap
    # ever fires against it, and every resource returns to zero) and the
    # GW-2/C14 stalled-send wedge response (release before close-out,
    # close-out under its own budget, force-close armed). Replayed step by
    # step with their expectations re-asserted; each is pinned to the
    # regime whose alphabet it needs.
    quint-gw-lifecycle-run-compliant-peer = mkQuintRunCheck {
      name = "gw-lifecycle-run-compliant-peer";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      match = "compliantLifecycleRun";
    };
    quint-gw-lifecycle-run-stall-wedge = mkQuintRunCheck {
      name = "gw-lifecycle-run-stall-wedge";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      match = "stallArmsForceCloseRun";
    };

    # ---- Gateway lifecycle Stage-C calibration witnesses ----------------
    # The historical-fix corpus replayed against the as-built model (the
    # gw-session-formal campaign's Phase-0 Stage C). Each check
    # instantiates the gwConnLifecycle core at a family-restricted scope,
    # swaps ONE action for its PRE-FIX behavior (the calibration module's
    # `calibStep`) and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-checked
    # evidence that the model would re-find that bug class if it were
    # reintroduced, and that the invariant is not vacuous for it. The full
    # per-candidate calibration table (verdict @ depth, states, the
    # trace-walk notes, and the evidence-only override modules and
    # T-direction runs that are not wired here) lives in
    # docs/spec/models/gw-session-invariant-map.md; the wired ones are the
    # representative per-family regression guards (one per falsifying
    # family, deepest consequence, cheap state space — every check stops at
    # its first violation). Deliberately no tracey markers (same policy as
    # the other models' witness/calibration checks).

    # F1 (443670d43 / GW-1): the session permit/gauge release keyed on the
    # sessions map (client action) again — a server-side ending leaves
    # capacity held with nothing armed to release it.
    quint-gw-calib-f1-server-side-release = mkQuintWitnessCheck {
      name = "gw-calib-f1-server-side-release";
      spec = "calibration/gw-f1-capacity";
      main = "gwCalibF1ServerSideKeepsGuard";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l2SendSettleArmed";
      # This module's conversion request is one of the two that OOM the
      # default 4 GiB Apalache server (occupancy letters x the full
      # close-out ladder); validated at 8 GiB.
      serverHeapMb = 8192;
    };

    # F2 (79912eda0 / GW-3): connection emptiness measured on open channels
    # again — a channel open disarms the empty grace and the
    # open-without-exec population sits with no deadline armed.
    quint-gw-calib-f2-open-disarms-grace = mkQuintWitnessCheck {
      name = "gw-calib-f2-open-disarms-grace";
      spec = "calibration/gw-f2-occupancy";
      main = "gwCalibF2OpenDisarmsGrace";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s20GraceArmedExactlyWhenEmpty";
    };

    # F3 (1c46d9781 / GW-4): a polite disconnect queued without arming the
    # force-close at the same decision point.
    quint-gw-calib-f3-decide-without-arm = mkQuintWitnessCheck {
      name = "gw-calib-f3-decide-without-arm";
      spec = "calibration/gw-f3-force-close";
      main = "gwCalibF3DecideWithoutArm";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s12DecideImpliesArmed";
    };

    # F4 (9739aca65 / GW-5): channel_close decrements unconditionally again
    # (forged/duplicate closes skew open_channels; exec honored on
    # never-accepted channels).
    quint-gw-calib-f4-forged-close-decrement = mkQuintWitnessCheck {
      name = "gw-calib-f4-forged-close-decrement";
      spec = "calibration/gw-f4-channel-accounting";
      main = "gwCalibF4ForgedCloseDecrements";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s7ChannelAccounting";
    };

    # F5 (51123b2be / GW-2): the close-out sends ordered before the
    # SessionGuard drop again — capacity release waits on the peer draining
    # the handle queue.
    quint-gw-calib-f5-release-after-close-out = mkQuintWitnessCheck {
      name = "gw-calib-f5-release-after-close-out";
      spec = "calibration/gw-f5-egress";
      main = "gwCalibF5ReleaseAfterCloseOut";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s15ReleaseBeforeCloseOut";
    };

    # F6 (0f476d6f0/a207ee15c / GW-9): a session-exit edge skips
    # cancel_active_builds again — the recurring case-completeness shape.
    quint-gw-calib-f6-exit-edge-skips-cancel = mkQuintWitnessCheck {
      name = "gw-calib-f6-exit-edge-skips-cancel";
      spec = "calibration/gw-f6-teardown";
      main = "gwCalibF6ExitEdgeSkipsCancel";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s10CancelOnSessionEnd";
      # Same conversion-size OOM as f1-server-side-release (the build
      # chain x the close-out ladder); validated at 8 GiB.
      serverHeapMb = 8192;
    };

    # F7 (765671437 / GW-13): the shutdown token only observed at the
    # opcode-read point again — drain expiry exits with an in-flight build
    # never cancelled.
    quint-gw-calib-f7-drain-expiry-no-cancel = mkQuintWitnessCheck {
      name = "gw-calib-f7-drain-expiry-no-cancel";
      spec = "calibration/gw-f7-drain";
      main = "gwCalibF7DrainExpiryNoCancel";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l4DrainObligationsArmed";
    };

    # F9 (755f49744 / GW-16): an upstream RPC await inside a session loses
    # its deadline again — the session parks in rpc-wait unbounded.
    quint-gw-calib-f9-rpc-wait-no-deadline = mkQuintWitnessCheck {
      name = "gw-calib-f9-rpc-wait-no-deadline";
      spec = "calibration/gw-f9-upstream-deadline";
      main = "gwCalibF9RpcWaitNoDeadline";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l6RpcWaitDeadlineArmed";
    };

    # F10 (9b693441f / GW-19): a transient accept error terminates the
    # listener (and the process, with every live session) again.
    quint-gw-calib-f10-accept-error-fatal = mkQuintWitnessCheck {
      name = "gw-calib-f10-accept-error-fatal";
      spec = "calibration/gw-f10-accept";
      main = "gwCalibF10AcceptErrorFatal";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s18ListenerSurvivesAcceptErrors";
    };

    # ------------------------------------------------------------------
    # Closure-evidence: the Phase D-prime survivors core + the two kept
    # calibration pins. Model: docs/spec/models/closureEvidence.qnt —
    # the PREDECESSOR system's evidence lifecycle (the walk, the
    # topdown_pruned / closure_hole columns, the Substituting status,
    # the forgiveness chain), archived IN PLACE 2026-06-02 (the A6 step,
    # post-soak precondition owner-waived; the file is the campaign
    # record at its original path — see its header banner); the
    # production machinery it verified was deleted by
    # substitution-replacement Phase D-prime (Waves D3-D6, migration
    # 080) — the store-owned materialization job (the
    # quint-materialization-* family above) is the only substitution
    # mechanism. The 27 walk-era checks that verified the deleted
    # machinery were pruned HERE in the same landing that wired the
    # survivors core (FP-1: never a window with neither the old oracle
    # nor the new); per-check dispositions (retired-with-mechanism vs
    # superseded-by-NAME) are in the pruning commit's body and the
    # Phase D-prime stage record
    # (docs/spec/models/substitution-replacement-invariant-map.md).
    # The held-back exhaustive targets, the historical verdicts and the
    # retired manual-target commands live in
    # docs/spec/models/closure-evidence-invariant-map.md (history is
    # append-only); the unwired calibration overrides under
    # docs/spec/models/calibration/closure-*.qnt carry per-file
    # retirement notes.

    # The survivors core, single-tenure two-build scope: the six
    # invariants whose subjects SURVIVE the deletion — A14 status
    # terminality, A15 dep gating, A22 condemnation co-ownership
    # scoping, B9 stale-Produced verify, B10 prune demand-set, L3
    # progress-armed liveness — over the restricted post-deletion
    # action alphabet (the walk letters are excluded from the step; see
    # the closureEvidenceSurvivors module's scope note, including the
    # recorded FailoverDuo B9 corner that fixed these constants).
    # B9/B10/A14/L3 are contended at this scope (prune, reap-survivor,
    # cross-build wanted shapes). Trace inclusion makes this a
    # regression oracle for recorded-green verdicts, not new coverage.
    # Paired falsifiability pins (the sim-holds vacuity rule):
    # quint-closure-calib-f1-stale-produced (B9) and
    # quint-closure-calib-f4-demand-drop (B10) below.
    # r[verify sched.merge.substitute-topdown+13]
    quint-closure-survivors-core = mkQuintSimHoldsCheck {
      name = "closure-survivors-core";
      spec = "closureEvidence";
      main = "closureEvidenceSurvivors";
      step = "survivorsStep";
      invariants = [
        "terminalIsTerminal"
        "readyImpliesDeclaredDepsProduced"
        "condemnRequiresLiveCoOwnedFailure"
        "staleProducedNeverUnlocksDependents"
        "demandSetSurvivesPrune"
        "liveBuildTerminalOrProgressArmed"
      ];
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # The survivors core, failover scope: the same six invariants and
    # restricted alphabet at the FailoverEx constants (single build,
    # one failover, one lost best-effort write) — A15's recovery-time
    # half and A22's condemnation scoping are contended here (their
    # full-alphabet record at this scope is the Wave-2b re-hunt).
    quint-closure-survivors-failover = mkQuintSimHoldsCheck {
      name = "closure-survivors-failover";
      spec = "closureEvidence";
      main = "closureEvidenceSurvivorsFailover";
      step = "survivorsStep";
      invariants = [
        "terminalIsTerminal"
        "readyImpliesDeclaredDepsProduced"
        "condemnRequiresLiveCoOwnedFailure"
        "staleProducedNeverUnlocksDependents"
        "demandSetSurvivesPrune"
        "liveBuildTerminalOrProgressArmed"
      ];
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # The recorded FailoverDuo B9 corner, made executable (substitution
    # map follow-up ledger row 2; Phase D-prime T-D7.1 scope finding).
    # The PREDECESSOR encoding's post-failover stored-union widening
    # violates B9 at the two-builds+failover scope: a narrow co-build
    # leaves a child Produced under width {1}; failover drops the
    # memory-only per-build wanted contributions; the effective wanted
    # set widens to the stored union (the 062-column fallback this model
    # encodes); a dependent opens from source above the now-stale
    # Produced child — a shape no holds record ever covered (FailoverDuo
    # was a C1-strict probe scope). This is HISTORY, not a product
    # oracle: production deleted the lossy fallback at Wave D2.3 (the
    # durable per-(build, derivation) relation is rebuilt at recovery);
    # the corner documents why the 062 semantics had to die. The check
    # runs the DISCOVERY configuration — the survivors-restricted
    # alphabet at the FailoverDuo constants (closureEvidenceCornerFailoverDuo;
    # the full-alphabet form's hit rate is too low for a bounded budget,
    # its status is the trace-inclusion corollary recorded in the model's
    # SCOPE NOTE) — and the pinned seed replays the recorded discovery
    # deterministically (sub-second); the 2M-sample budget is the
    # unseeded re-find backstop (T-D7.1: violated within 2 M samples at
    # this configuration). If this check goes RED, the predecessor
    # encoding in closureEvidence.qnt has drifted — route to the A6
    # archive record (substitution map ledger row 3); never silently
    # delete the check or the corner.
    quint-closure-corner-failover-duo-b9 = mkQuintSimWitnessCheck {
      name = "closure-corner-failover-duo-b9";
      spec = "closureEvidence";
      main = "closureEvidenceCornerFailoverDuo";
      step = "survivorsStep";
      witness = "staleProducedNeverUnlocksDependents";
      seed = "0xffbfc9ac0c85df5b";
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # A4 (bughunt wave): the reap-truncation corner — the standalone
    # closureEvidenceReapTruncate module (RDC-5 additions-only; the
    # FailoverDuo-corner pattern). The invariant pins that terminal-
    # build cleanup truncates the evidence the classifier reads (the
    # F9-class guard's archived-model twin): outside the bounded
    # pre-reap window, a live Vouched parent's children are Produced
    # under live interest.
    quint-closure-reap-truncate-holds = mkQuintSimHoldsCheck {
      name = "closure-reap-truncate-holds";
      spec = "closureEvidence";
      main = "closureEvidenceReapTruncate";
      invariants = [ "vouchedImpliesAllDurableChildrenProduced" ];
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # The pre-fix flip: calibReapNoTruncate clears interest without the
    # removal/hole/settlement pass — stale produced children keep
    # vouching. Pinned seed replays the recorded discovery; the budget
    # is the unseeded re-find backstop (violated in <400 K samples at
    # this scope).
    quint-closure-reap-truncate-calib = mkQuintSimWitnessCheck {
      name = "closure-reap-truncate-calib";
      spec = "closureEvidence";
      main = "closureEvidenceReapTruncate";
      step = "calibStep";
      witness = "vouchedImpliesAllDurableChildrenProduced";
      seed = "0xc214b66a0b0eb6b0";
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # ---- Kept calibration pins (the survivors' falsifiability) --------
    # Each instantiates the closure-evidence model at the named override
    # module's constants, swaps the named action(s) for their PRE-FIX
    # behavior (the override's calibStep) and passes only while the
    # checker still falsifies the property the fix protects — the
    # machine-checked record that the model's invariant set re-finds
    # that bug class. These two stay wired because the mechanisms their
    # fixes guard SURVIVE Phase D-prime (the stale-Produced verify and
    # the prune demand-set are post-deletion production behavior, B9 and
    # B10 of the survivors core above); the other closure pins and
    # witnesses guarded walk-era mechanisms and retired with them.
    # No tracey markers on calibration checks (house convention).

    # F1 soundness (CE-2, 29f0a8afa): no stale-Produced verify — a
    # resubmission's parent ends Ready above a Produced child whose
    # live-wanted outputs are absent.
    quint-closure-calib-f1-stale-produced = mkQuintWitnessCheck {
      name = "closure-calib-f1-stale-produced";
      spec = "calibration/closure-f1-stale-produced";
      main = "closureCalibF1StaleProduced";
      extraSpecs = [ "closureEvidence" ];
      step = "calibStep";
      witness = "staleProducedNeverUnlocksDependents";
      # The closureEvidence file gained the two survivors-core module
      # instantiations at T-D7.1; the quint->TLA+ conversion request now
      # OOMs the 4 GiB default server (measured: heap-space fatal at
      # 4096, converts at 8192 — the gw-f1/f6 precedent).
      serverHeapMb = 8192;
    };

    # F4 demand-set completeness (CE-66, 85213119d): the prune's demand
    # set is the structural roots only — an explicitly requested
    # non-root is silently dropped.
    quint-closure-calib-f4-demand-drop = mkQuintWitnessCheck {
      name = "closure-calib-f4-demand-drop";
      spec = "calibration/closure-f4-demand-drop";
      main = "closureCalibF4DemandDrop";
      extraSpecs = [ "closureEvidence" ];
      step = "calibStep";
      witness = "demandSetSurvivesPrune";
      # Same conversion-size OOM as the f1 pin above (the survivors-core
      # modules); validated at 8 GiB.
      serverHeapMb = 8192;
    };

    # ----- bughunt fix wave, workstream C1 (terminal-capture) -----
    # Gateway build-watch display under event loss: the single kinded
    # display map (bug_150/144), the in-stream resync signal (bug_153),
    # and the durable terminal row (merged_bug_323). Tier-1: TLC
    # exhausts the 2-drv space in seconds.
    # r[verify gw.display.single-map]
    # r[verify gw.resync.loss-signal+1]
    # r[verify sched.pull.kinded-running-surface]
    # r[verify sched.watch.terminal-from-durable-row+2]
    # r[verify gw.resync.reattach-budget+3]
    # r[verify gw.resync.snapshot-owed]
    quint-gw-build-resync = mkQuintCheck {
      name = "gw-build-resync";
      spec = "gwBuildResync";
      invariants = [
        "kindedSurfaceAgrees"
        "noStuckDisplay"
        "tailCoverage"
        "terminalVerdictNeverFabricated"
        "boundedResyncStreak"
        "snapshotOwedNoConsume"
      ];
    };

    # The tail reader loop: re-open pacing keyed on chunk VERDICTS (not
    # receipts) and the orphan exit freeze (merged_bug_054 /
    # merged_bug_130). Tier-1, exhausts instantly.
    # r[verify dash.stream.reopen-pacing]
    quint-tail-reader-loop = mkQuintCheck {
      name = "tail-reader-loop";
      spec = "tailReaderLoop";
      invariants = [
        "orphanNeverReopens"
        "pacingEscalatesAbsentProgress"
      ];
    };

    # Authorization verdicts: declared-verifier transport law, keyed
    # no-undeclared-admit, WatchBuild lifecycle-phase independence, and
    # TailLog request-string independence (bughunt-2 slot 4: bug_237 /
    # merged_bug_122 / bug_213 / merged_bug_064). Verdicts are
    # admit/deny only — the resident-phase status asymmetry is the
    # signed §5-S Q4 residual. Tier-1, exhausts instantly.
    # r[verify store.authz.declared-verifier]
    # r[verify sched.tenant.authz+3]
    # r[verify store.log.tail-ownership]
    quint-authz = mkQuintCheck {
      name = "authz";
      spec = "authz";
      invariants = [
        "enforcementFromDeclaredVerifiersOnly"
        "noUndeclaredAdmitWhenKeyed"
        "lifecyclePhaseIndependence"
        "requestStringIndependence"
      ];
    };

    # Non-vacuity witness for the two green layer invariants above:
    # "no keyed declared credential ever admits" MUST be falsified —
    # the advertised legs (claims on keyed TenantJwt, verified service
    # token on keyed Service, assignment header on keyed
    # AssignmentToken) really admit somewhere. An enforcement collapse
    # into deny-everything would hold the green invariants vacuously;
    # this turns that into a red check.
    quint-authz-witness-advertised-leg = mkQuintWitnessCheck {
      name = "authz-witness-advertised-leg";
      spec = "authz";
      main = "authz";
      witness = "advertisedLegSilent";
    };

    # Open-attempt closure under cancellation + the materialize wave's
    # claim/settlement laws (bug_347 base; §4-R2 rework: bug_357 +
    # bug_182 + merged_bug_055 + bug_251). Tier-1: 18 reachable states,
    # measured <1s exhaustive — budget 120s is ~100x headroom.
    # openAttemptHasDriver was REPLACED by notDriverLost (bug_357: the
    # old form was a propositional tautology; the new one is a
    # live-computed latch seated at the transition that must arm the
    # driver).
    # r[verify sched.attempt.cancel-close-driven+1]
    # r[verify sched.materialize.ack-law]
    # r[verify sched.materialize.claim-coherence]
    # r[verify sched.materialize.claim-resume]
    # bughunt-3 S5: +3 claim-plane laws (ledger-as-mint-authority,
    # answered-refusal disposition, confirm-only no-mint). Tier-1
    # re-measured: 22 distinct states (was 18), <1s exhaustive.
    # bughunt-4 S5a (merged_bug_074): noRefusalFiledAsLost re-scoped
    # to mint-disproving refusals; NEW authRefusalSeat +
    # claimRefusedAuthSkew (a deliberate live frame -- the auth
    # refusal keeps the credential untouched; the rotation-skew twin
    # perturbs the seat and falsifies noFaultNeverCharged through the
    # existing sweepEstablish charge path).
    # bughunt-5 S4 (merged_bug_011): the FENCE plane -- goneAnswerSeat
    # (write-ahead decision) + the mintApply mintAfterGone oracle +
    # resubmitReready (the straggler chain's middle step); NEW
    # invariant noMintAfterGoneAnswer (a Gone answer is terminal for
    # the token: write-ahead + DeliverNew screen,
    # sched.executor.confirm-fence). Tier-1 re-measured: 34 distinct
    # states (was 22), <1s exhaustive TLC -- budget 120s is ~100x
    # headroom.
    # r[verify sched.executor.confirm-fence]
    quint-open-attempts = mkQuintCheck {
      name = "open-attempts";
      spec = "openAttempts";
      modelTimeoutSec = 120;
      invariants = [
        "cancelledNeverChargedAsCrash"
        "notDriverLost"
        "ackImpliesSettledOrArmed"
        "claimedImpliesOpenAttempt"
        "noFaultNeverCharged"
        "noCredentialClobber"
        "noRefusalFiledAsLost"
        "confirmNeverMints"
        "noMintAfterGoneAnswer"
      ];
    };

    # bughunt-4 S5a (merged_bug_072): the ledger-derived mint-budget
    # law -- a SEPARATE module in openAttempts.qnt (the claim plane's
    # regimes stay byte-identical; the budget plane is two bounded
    # counters). outstanding = surviving ledger population; the
    # overmint twin gates on the pre-fix per-pass counter instead.
    # The budget module lives in its OWN file (one module per file --
    # the multi-module second-main shape proved fragile under
    # concurrent apalache servers). BOTH counters saturate at
    # POP_CAP: the first wiring left passMints unbounded and TLC ran
    # to 23M+ distinct states without a verdict (the mint->resolve
    # cycle re-enables the increment forever) -- the wedgeCluster
    # ghost-bounding lesson applied to our own plane. Measured 971ms
    # exhaustive TLC after saturation; budget 120s.
    quint-open-attempts-budget = mkQuintCheck {
      name = "open-attempts-budget";
      spec = "openAttemptsBudget";
      modelTimeoutSec = 120;
      invariants = [ "outstandingBounded" ];
    };

    # Expect-violation calibrations: each freezes one as-shipped design
    # and pins its falsification permanently.
    # Converted to live-import action-only form (bughunt-2 slot 11,
    # §4-R4): each imports the live gwBuildResync and perturbs only the
    # documented decision via calibStep; P3 baselines pair with the
    # live exhaustive check.
    quint-gwresync-calib-two-map = mkQuintWitnessCheck {
      name = "gwresync-calib-two-map";
      spec = "calibration/gwresync-two-map";
      main = "gwResyncTwoMap";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "noStuckDisplay";
    };
    quint-gwresync-calib-no-signal = mkQuintWitnessCheck {
      name = "gwresync-calib-no-signal";
      spec = "calibration/gwresync-no-signal";
      main = "gwResyncNoSignal";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "tailCoverage";
    };
    quint-gwresync-calib-kind-blind = mkQuintWitnessCheck {
      name = "gwresync-calib-kind-blind";
      spec = "calibration/gwresync-kind-blind";
      main = "gwResyncKindBlind";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "kindedSurfaceAgrees";
    };
    quint-gwresync-calib-no-pg-fallback = mkQuintWitnessCheck {
      name = "gwresync-calib-no-pg-fallback";
      spec = "calibration/gwresync-no-pg-fallback";
      main = "gwResyncNoPgFallback";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "terminalVerdictNeverFabricated";
    };
    # r[verify gw.resync.reattach-budget+3]
    quint-gwresync-calib-reset-on-snapshot = mkQuintWitnessCheck {
      name = "gwresync-calib-reset-on-snapshot";
      spec = "calibration/gwresync-reset-on-snapshot";
      main = "gwResyncCalibResetOnSnapshot";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "boundedResyncStreak";
    };
    # r[verify gw.resync.snapshot-owed]
    quint-gwresync-calib-consume-while-owed = mkQuintWitnessCheck {
      name = "gwresync-calib-consume-while-owed";
      spec = "calibration/gwresync-consume-while-owed";
      main = "gwResyncCalibConsumeWhileOwed";
      extraSpecs = [ "gwBuildResync" ];
      step = "calibStep";
      witness = "snapshotOwedNoConsume";
    };
    # Authz pre-fix laws frozen (bughunt-2 slot 4) — each falsifies
    # its paired green invariant.
    quint-authz-calib-237-foreign-knob = mkQuintWitnessCheck {
      name = "authz-calib-237-foreign-knob";
      spec = "calibration/authz-237-foreign-knob";
      main = "authzCalib237ForeignKnob";
      extraSpecs = [ "authz" ];
      step = "calibStep";
      witness = "enforcementFromDeclaredVerifiersOnly";
    };
    quint-authz-calib-122-dead-leg = mkQuintWitnessCheck {
      name = "authz-calib-122-dead-leg";
      spec = "calibration/authz-122-dead-leg";
      main = "authzCalib122DeadLeg";
      extraSpecs = [ "authz" ];
      step = "calibStep";
      witness = "noUndeclaredAdmitWhenKeyed";
    };
    quint-authz-calib-213-phase = mkQuintWitnessCheck {
      name = "authz-calib-213-phase";
      spec = "calibration/authz-213-phase";
      main = "authzCalib213Phase";
      extraSpecs = [ "authz" ];
      step = "calibStep";
      witness = "lifecyclePhaseIndependence";
    };
    quint-authz-calib-064-string = mkQuintWitnessCheck {
      name = "authz-calib-064-string";
      spec = "calibration/authz-064-string";
      main = "authzCalib064String";
      extraSpecs = [ "authz" ];
      step = "calibStep";
      witness = "requestStringIndependence";
    };
    quint-tailreader-calib-orphan-hotloop = mkQuintWitnessCheck {
      name = "tailreader-calib-orphan-hotloop";
      spec = "calibration/tailreader-orphan-hotloop";
      main = "tailReaderCalibOrphanHotloop";
      extraSpecs = [ "tailReaderLoop" ];
      step = "calibStep";
      witness = "orphanNeverReopens";
    };
    # r[verify dash.stream.reopen-pacing]
    quint-tailreader-calib-reset-on-receipt = mkQuintWitnessCheck {
      name = "tailreader-calib-reset-on-receipt";
      spec = "calibration/tailreader-reset-on-receipt";
      main = "tailReaderCalibResetOnReceipt";
      extraSpecs = [ "tailReaderLoop" ];
      step = "calibStep";
      witness = "pacingEscalatesAbsentProgress";
    };
    # openAttempts twins (§4-R2): live-import action-only, each
    # perturbing ONE decision through the model's oracle seats; every
    # invariant of quint-open-attempts has its falsifier here (P1).
    # All measured <1s; budget 120s.
    quint-openattempts-calib-charge-blind = mkQuintWitnessCheck {
      name = "openattempts-calib-charge-blind";
      spec = "calibration/openattempts-charge-blind";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsChargeBlind";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "cancelledNeverChargedAsCrash";
    };
    quint-openattempts-calib-outbox-dropped = mkQuintWitnessCheck {
      name = "openattempts-calib-outbox-dropped";
      spec = "calibration/openattempts-outbox-dropped";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsOutboxDropped";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "notDriverLost";
    };
    quint-openattempts-calib-ack-on-failed-close = mkQuintWitnessCheck {
      name = "openattempts-calib-ack-on-failed-close";
      spec = "calibration/openattempts-ack-on-failed-close";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsAckOnFailedClose";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "ackImpliesSettledOrArmed";
    };
    quint-openattempts-calib-no-fallback-release = mkQuintWitnessCheck {
      name = "openattempts-calib-no-fallback-release";
      spec = "calibration/openattempts-no-fallback-release";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsNoFallbackRelease";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "claimedImpliesOpenAttempt";
    };
    quint-openattempts-calib-nonceless-mint = mkQuintWitnessCheck {
      name = "openattempts-calib-nonceless-mint";
      spec = "calibration/openattempts-nonceless-mint";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsNoncelessMint";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "noFaultNeverCharged";
    };
    # bughunt-3 S5 twins (rule-4b client-loop repairs): every new
    # claim-plane invariant has its falsifier (P1). Measured <1.1s
    # each (34/34/30 distinct states); budget 120s.
    quint-openattempts-calib-clobbered-credential = mkQuintWitnessCheck {
      name = "openattempts-calib-clobbered-credential";
      spec = "calibration/openattempts-clobbered-credential";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsClobberedCredential";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "noCredentialClobber";
    };
    quint-openattempts-calib-refusal-as-lost = mkQuintWitnessCheck {
      name = "openattempts-calib-refusal-as-lost";
      spec = "calibration/openattempts-refusal-as-lost";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsRefusalAsLost";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "noRefusalFiledAsLost";
    };
    quint-openattempts-calib-minting-confirm = mkQuintWitnessCheck {
      name = "openattempts-calib-minting-confirm";
      spec = "calibration/openattempts-minting-confirm";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsMintingConfirm";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "confirmNeverMints";
    };
    # bughunt-4 S5a twins (the merged_bug_072/074 client-loop
    # repairs): decision-only perturbations through the new seats;
    # P3 baselines pair with the live exhaustive checks above.
    # Measured (TLC): rotation-skew [violation] 864ms / baseline [ok]
    # 851ms; overmint [violation] 790ms / baseline [ok] 803ms;
    # budget 120s.
    quint-openattempts-calib-rotation-skew = mkQuintWitnessCheck {
      name = "openattempts-calib-rotation-skew";
      spec = "calibration/openattempts-rotation-skew";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsRotationSkew";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "noFaultNeverCharged";
    };
    quint-openattempts-calib-overmint = mkQuintWitnessCheck {
      name = "openattempts-calib-overmint";
      spec = "calibration/openattempts-overmint";
      extraSpecs = [ "openAttemptsBudget" ];
      main = "openAttemptsOvermint";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "outstandingBounded";
    };
    # bughunt-5 S4 twin (merged_bug_011): the live-loop Gone answers
    # WITHOUT the fence write (the as-built pre-fix confirm_only-gated
    # arm) -- decision-only perturbation at goneAnswerSeat; the
    # mintAfterGone latch truth comes from the mint seat's own oracle.
    # Measured (TLC): [violation] 935ms / baseline [ok] 955ms;
    # budget 120s.
    # r[verify sched.executor.confirm-fence]
    quint-openattempts-calib-unfenced-gone = mkQuintWitnessCheck {
      name = "openattempts-calib-unfenced-gone";
      spec = "calibration/openattempts-unfenced-gone";
      extraSpecs = [ "openAttempts" ];
      main = "openAttemptsUnfencedGone";
      step = "calibStep";
      modelTimeoutSec = 120;
      witness = "noMintAfterGoneAnswer";
    };
  };

  # bughunt-2 (slot 11) funnel: flake.nix consumes ONLY `checks`, so a
  # wired check is inside the quint-policy lint domain BY CONSTRUCTION —
  # there is no second list to forget.
  checks = quintCorpus // {
    quint-policy = mkPolicyCheck quintCorpus;
  };
}
