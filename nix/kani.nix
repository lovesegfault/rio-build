# Per-crate Kani verification: hand crateBuildKani's output to
# `kani verify-artifacts`.
#
# crateBuildKani (flake.nix) compiles every workspace member with
# kani-compiler and stashes the `*.symtab.out` + `kani-metadata.json`
# sidecars in the crate's `lib/` output. This file copies them to a
# writable scratch dir and runs `kani verify-artifacts` on it. kani-driver
# owns the entire CBMC pipeline (link, specialize, instrument-contracts,
# add-library, undefined-functions, backedges, cbmc), the verdict
# rendering, should_panic inversion, loop contracts, and parallel harness
# scheduling. We don't replicate any of it.
#
# `kani verify-artifacts` is in our pinned `lovesegfault/kani/rio-build`
# fork (kani-0.67.0 + verify-artifacts + compiler-defaults patches),
# pending upstream PRs. See nix/kani-toolchain.nix.
#
# Caching model:
#   - kani-compiler compile phase: per-crate cached via crateBuildKani.
#     Editing rio-common rebuilds rio-common's kani drv + downstream
#     dependents; editing a test file rebuilds nothing.
#   - kani-driver verify phase (this file): re-runs when the workspace
#     member's own kani drv changes, which transitively means when the
#     member or any reachable dep changes. Correct — `--reachability=harnesses`
#     produces a goto program that closes over the harness's call graph.
#
# Limitation: only lib-only workspace members. Members with [[bin]] hit a
# link error in crateBuildKani — deps are MIR-only. See
# nix/crate2nix.nix's `excludeCrateTypes` for the cdylib analogue.
#
# r[verify ...] markers go HERE — at the `kani-checks` attrset entry that
# wires a member, NOT in the harness function or the .typ spec source.
# Same wiring-point discipline as `subtests = [...]` in
# nix/tests/default.nix (P0341). .config/tracey/config.styx already lists
# this file under `test_include`.
{
  pkgs,
  kaniToolchain,
  crateBuildKani,
}:
let
  mkKaniCheck =
    {
      name,
      crate,
      # Exact harness-count tripwire: when non-null, the check fails
      # unless kani-driver's summary reports exactly this many verified
      # harnesses — so a harness silently dropping out of the artifact
      # set (a cfg/module-path regression, an accidental delete) is a
      # red check, not a quieter green one. `null` keeps the weaker
      # at-least-one guard only.
      expectedHarnesses ? null,
      # Wall-clock budget for the whole verify-artifacts batch — the
      # eternal-gate class fix, kani family (the quint family got the
      # same bound first: nix/quint.nix modelTimeoutSec). A solver
      # blowup or non-terminating symex must be a RED CHECK naming its
      # budget, never a silently-running drv that wedges gates and
      # starves daemon build locks (two relic clients burned 38h on a
      # kani drv before this bound existed). Default = generous
      # headroom over every observed green run of the largest member
      # (rio-evidence-kernel, ~21 harnesses, single-digit minutes);
      # raise per-member only with a measured justification.
      modelTimeoutSec ? 3600,
    }:
    pkgs.runCommand "kani-${name}"
      {
        nativeBuildInputs = [
          # `kani` (with cbmc, kissat, gcc on its wrapper PATH).
          kaniToolchain.kani-driver-wrapped
        ];
        # buildRustCrate's `lib` output holds `lib/{*.rlib,*.symtab.out,
        # *.kani-metadata.json}`. The `or crate` fallback supports a
        # `{ lib = <store-path>; }` shim for ad-hoc testing.
        src = crate.lib or crate;
        # Surfaced in `nix log` and error messages.
        env = {
          MEMBER = name;
          EXPECTED_HARNESSES = if expectedHarnesses == null then "" else toString expectedHarnesses;
          MODEL_TIMEOUT_SEC = toString modelTimeoutSec;
        };
      }
      ''
        set -euo pipefail

        artifacts="$src/lib"
        if [ ! -d "$artifacts" ]; then
          echo "ERROR: $artifacts does not exist (expected a crateBuildKani lib output)." >&2
          exit 1
        fi

        # The artifact dir is a read-only store path. `kani verify-artifacts`
        # writes the linked `.out` file next to each `.symtab.out`
        # (Project::try_new()), so it needs a writable copy.
        work=$(mktemp -d)
        cp "$artifacts"/* "$work/"
        chmod -R u+w "$work"

        # kani-driver exits 1 on any verification failure — runCommand's
        # `set -e` propagates that. tee into $out so `cat result` and
        # `nix log` carry the per-harness verdicts.
        #
        # `--jobs` takes an optional value (clap `[<JOBS>]`); the `=` form
        # binds it to the flag instead of being parsed as a positional
        # artifact dir. kani requires `--output-format=terse` with `--jobs`
        # (parallel harness output would otherwise interleave); terse still
        # prints the per-harness verdict + the final
        # `Complete - N successfully verified harnesses` summary line.
        # Wall-clock chokepoint (see modelTimeoutSec above): timeout's
        # 124 is split from real verification failures so the over-
        # budget red names the budget and the remedy.
        rc=0
        timeout --signal=TERM "$MODEL_TIMEOUT_SEC" \
          kani verify-artifacts "$work" \
          -Z unstable-options \
          --output-format=terse \
          --jobs="''${NIX_BUILD_CORES:-1}" \
          2>&1 | tee "$out" || rc=$?
        if [ "$rc" -eq 124 ]; then
          echo "kani-$MEMBER: exceeded the ''${MODEL_TIMEOUT_SEC}s wall-clock budget —" >&2
          echo "non-termination / solver blowup is a FAILURE, not a tail to wait on." >&2
          echo "Diagnose interactively (cargo kani --harness <name>) and raise" >&2
          echo "modelTimeoutSec at this member's nix/kani.nix entry only with a" >&2
          echo "measured justification." >&2
          exit 1
        fi
        [ "$rc" -eq 0 ] || exit "$rc"

        # Non-vacuity guard, same discipline as mbt-rio-lease's "N tests
        # run" grep and mkQuintWitnessCheck's violation-report grep
        # (nix/quint.nix): kani-driver exits 0 when the artifact set
        # contains zero proof harnesses ("No proof harnesses ... were
        # found to verify"), so the exit code alone cannot distinguish
        # "all harnesses verified" from "verified nothing". Require the
        # summary line to report at least one verified harness; kani
        # already exits 1 on any verification failure, so this only has
        # to catch the verified-nothing case (and any future exit-0 path
        # that produces no summary line at all).
        if ! grep -qE 'Complete - [1-9][0-9]* successfully verified harnesses' "$out"; then
          echo "kani-$MEMBER: no proof harnesses were verified (vacuous check)." >&2
          echo "Check that $artifacts contains a *.kani-metadata.json with a non-empty proof_harnesses list:" >&2
          echo "either the #[cfg(kani)] proofs module stopped being compiled into the kani build, or the crateBuildKani / kani-compiler-defaults plumbing changed (flake.nix, nix/kani-toolchain.nix)." >&2
          exit 1
        fi

        # Exact harness-count tripwire (per-member, opt-in): a harness
        # that silently stops being compiled or discovered must fail the
        # check, not shrink it.
        if [ -n "$EXPECTED_HARNESSES" ]; then
          if ! grep -qE "Complete - $EXPECTED_HARNESSES successfully verified harnesses" "$out"; then
            echo "kani-$MEMBER: expected exactly $EXPECTED_HARNESSES verified harnesses; summary line disagrees:" >&2
            grep -E 'Complete - [0-9]+ successfully verified harnesses' "$out" >&2 || true
            echo "If a harness was deliberately added or removed, update expectedHarnesses at this member's entry in nix/kani.nix." >&2
            exit 1
          fi
        fi
      '';
in
{
  # Expose the constructor so ad-hoc spike tests can wire their own checks.
  inherit mkKaniCheck;

  # rio-lease: lease/election state machine. One #[kani::proof_for_contract(decide_pure)]
  # harness verifies the four #[kani::ensures] iff-clauses on decide_pure() over
  # the full input domain. The contract case structure parallels the action
  # partition in docs/spec/models/leaderElection.qnt. Plus lib.rs:
  # the lease_standing_proofs quartet (fence-never-clears-held,
  # release-gate-iff-acquired-unsuperseded, the merged_bug_002
  # episode-scoped 409 deferral: Exhausted fires iff an unresolved
  # same-episode deferral precedes, and the bug_002 routing totality:
  # no believing act-failed completed read maps to a no-transition
  # action, and every routed transition clears a pending deferral)
  # and the DirtyGen mark-after-snapshot proof.
  # Harness ledger: 4 -> 5 (bughunt-5 S8 added the deferral proof)
  # -> 6 (bughunt-6 S6 added the routing-totality proof).
  # r[verify sched.lease.k8s-lease+2]
  # r[verify sched.lease.at-most-one-leader+3]
  kani-rio-lease = mkKaniCheck {
    name = "rio-lease";
    crate = crateBuildKani.members.rio-lease;
    expectedHarnesses = 6;
  };

  # rio-log-kernel: the store's log-chunk decision kernels, extracted
  # from rio-store/src/logs/kernel.rs into a dependency-free crate (the
  # rio-retry-kernel template) so the harnesses' goto model closes over
  # the kernel alone — the former kani-rio-store member re-verified on
  # EVERY rio-store or transitive-dep edit and sat one std-machinery
  # growth spurt away from the blowup class recorded below.
  # rio_store::logs::kernel re-exports the crate, so the store-side
  # call sites and the projection shims are unchanged.
  # Five harnesses:
  #   - check_visit_chunk_contract / check_accept_verdict_contract:
  #     #[kani::proof_for_contract] over the full input domain — the
  #     chunk-interval arithmetic cannot overflow under the manifest
  #     BIGINT precondition, and the accept verdict partitions its
  #     inputs exactly as docs/spec/models/logService.qnt::acceptVerdict.
  #   - check_dedup_{pair,triple}_serves_union_exactly_once: the read
  #     path's ordered-walk dedup serves each line at most once and the
  #     served set equals the union of the chunks' ranges (the model's
  #     servedSpanExact, over the full u64 domain instead of MAX_LINE=3).
  #   - check_manifest_covers_no_uncovered_point: a manifest reported as
  #     covering [0, up_to) really has no gap (the soundness direction
  #     of the completeness predicate that seals a log against appends).
  #
  # NOT wired (closed): a sixth harness for the GC chunk-list parse
  # contract (rio-store/src/gc/mod.rs try_parse_unique_chunk_hashes —
  # no panic on arbitrary input; Err exactly when Manifest::deserialize
  # rejects; on Ok an exact dedup of the entry hashes that is empty only
  # for a zero-entry manifest, so corrupt input is never reported as an
  # empty chunk set) was attempted for the former kani-rio-store member
  # with bounded arbitrary inputs of one version byte plus four, two,
  # and finally one 36-byte entry, with explicit #[kani::unwind] bounds.
  # None of those converged inside the merge-gate budget on the CI
  # builder while the five wired harnesses verify in seconds — the
  # dominant cost was the symbolic execution of the std Vec/slice/sort
  # machinery the parse and dedup use, which travels with the code into
  # the goto model (the same blowup class that kept rio-retry-kernel
  # out of the gate before its bounded-representation change), not the
  # contract assertions themselves. The deferral was closed as a
  # reasoned omission at the refcount campaign's Phase-2 close-out
  # instead of being revived: the refcount Release B deletion left
  # try_parse_unique_chunk_hashes #[cfg(test)]-only (it is the
  # differential-test oracle for the collector's server-side SQL
  # expansion, not a production decision path), so a proof of it would
  # not bind production behavior, and the collect eligibility predicate
  # itself lives in SQL rather than Rust, so the once-planned
  # decide_collect kernel proof has no production subject either. The
  # production corrupt-vs-valid arbiters keep their coverage:
  # Manifest::deserialize via the fuzz/rio-store manifest_deserialize
  # target and unit tests, and the collector's fail-closed SQL
  # validation pass via the differential pinning and abort tests in
  # gc/collect.rs. No verify marker is claimed for the parse contract.
  # Full record: docs/spec/models/refcount-invariant-map.md,
  # "Phase-2 assurance layer". If a Rust-side parse or eligibility
  # kernel ever returns to a production path, the cfg(kani) bounded
  # representation used by rio-retry-kernel is the template to bring a
  # harness for it into the gate budget — now in this kernel crate, no
  # longer coupled to rio-store's artifact context.
  # r[verify store.log.session-keyed]
  # r[verify store.log.ingest-bounds]
  # r[verify store.log.completeness-gate]
  # r[verify store.log.tail-grace-drain+2]
  # r[verify store.log.read-divergence+2]
  kani-rio-log-kernel = mkKaniCheck {
    name = "rio-log-kernel";
    crate = crateBuildKani.members.rio-log-kernel;
    # 5 + check_bounded_prefix_contract (B2: store.log.write-read-bound)
    # + check_tail_next_orphan_always_exits (bughunt2 slot 1:
    #   merged_bug_130 -- Orphaned exits unconditionally)
    # + check_final_claim_contract + check_visit_fanout_batch_contract
    #   + check_object_coverage_policy (bughunt2 slot 6: served-claim,
    #   gap-provenance, short-object coverage policy).
    expectedHarnesses = 14;
  };

  # rio-authz-kernel: the store's transport authorization decision
  # kernel (the credential-class vocabulary, the pure decide() verdict,
  # and the boot key-coherence predicate), born dependency-free per the
  # rio-log-kernel precedent so the goto model closes over the kernel
  # alone. Four harnesses (rio-authz-kernel/src/lib.rs `mod proofs`):
  #   - check_foreign_knob_independence: configs agreeing on the
  #     class's declared verifier family produce identical verdicts --
  #     the projection-dispatch pin (bughunt2 bug_237: the half-config
  #     cross-tenant-admin state was an arm reading a foreign knob).
  #   - check_key_coherence_partition: refused boot states are exactly
  #     jwt && !(service && hmac), each naming a truly-missing knob.
  #   - check_no_undeclared_admit: a keyed class with its knob ON
  #     admits only its declared accepting presentation (the dead
  #     tenant leg on Service methods stays dead).
  #   - check_decide_total: decide() is panic-free over the full
  #     domain and unkeyed knobs always admit (dual-mode doctrine).
  # r[verify store.authz.declared-verifier]
  # r[verify store.authz.key-coherence]
  kani-rio-authz-kernel = mkKaniCheck {
    name = "rio-authz-kernel";
    crate = crateBuildKani.members.rio-authz-kernel;
    expectedHarnesses = 4;
  };

  # rio-retry-kernel: the scheduler's retry/poison decision kernels
  # (decide()/classify()/placeable() and the reference fold's counter
  # arithmetic), extracted from rio-scheduler/src/retry_policy.rs into a
  # dependency-free crate so the harnesses' goto model closes over the
  # kernel alone — the extraction the retry campaign's Phase-2 deferral
  # recorded in docs/spec/models/retry-invariant-map.md as the
  # precondition for gating this check. Gated (in checks.*) since the
  # kernel's exclusion-set representation became cfg(kani)-swappable:
  # under kani every executor-id set is the kernel's fixed-capacity
  # BoundedIdSet (via the IdSet alias) instead of std's BTreeSet, the
  # ledger fold runs without an intermediate Vec, and the exemption
  # predicate's substring search is a windowed byte comparison — the
  # extraction alone had NOT been sufficient (the harnesses spent their
  # budget symbolically executing std BTreeSet/Vec/str machinery; the
  # measured history lives in the extraction follow-up's and the
  # representation change's commit messages). The swap is proof-only:
  # production keeps BTreeSet, and the two representations are pinned to
  # each other by the kernel's differential unit tests plus the
  # set-semantics harness below.
  #
  # Ten harnesses (in rio-retry-kernel/src/lib.rs `mod proofs`):
  #   - check_bounded_set_models_set_semantics: the proof-time bounded
  #     set obeys set semantics over symbolic values (insert newness,
  #     precise membership, distinct-count len, order-insensitivity,
  #     iter-yields-members) — the harness half of the representation
  #     equivalence pin.
  #   - check_decide_contract: asserts decide()'s three stated ensures
  #     clauses (through their shared predicate bodies; the
  #     contract-instrumented proof_for_contract form of a fold this
  #     size exceeds the gate budget) over bounded arbitrary attempt
  #     suffixes and scaled budgets — the
  #     verdict partition is consistent with the final counters (each
  #     terminal verdict names a budget really at its bound;
  #     fleet-exhaust is unreachable from decide()), a Requeue verdict
  #     never exceeds a budget cap, the exclusion set contains the
  #     executor of every charged threshold attempt, and (overflow
  #     checks on) the fold's counter
  #     arithmetic cannot overflow over the domain. (The legacy-seed
  #     clauses and the seed-merge harness retired with the P5 seed —
  #     migration 075 dropped the mirror columns it read.)
  #   - check_decide_deterministic: same inputs, two calls, equal
  #     Decisions.
  #   - check_classify_contract: the classification partition iff per
  #     observed-failure variant; the exemption predicate is exactly
  #     promoted-or-CONCURRENT_PUTPATH on the worker channel and
  #     promoted on the controller channel (the exempt-infra-cap
  #     definition, both channels).
  #   - check_placeable_contract: the placement partition iff — empty
  #     fleet defers, exhaustion requires every eligible worker
  #     excluded.
  #   - check_fold_fleet_exhaust_arm: the fold-side fleet-exhaust arm
  #     (E1) needs a non-empty fully-failed fleet; an empty fleet never
  #     poisons.
  #   - check_materialization_rows_invisible_to_build_decision: the kind
  #     partition (substitution-replacement design §2.5) — for any row
  #     set over both work kinds and all 15 outcome classes, the build
  #     decision over the full set equals the build decision over its
  #     build-kind-only subset (verdict, exclusion, backoff, counters).
  #   - check_materialization_never_poisons: the park-not-fail corollary
  #     — if the build-kind subset alone does not poison, no amount of
  #     interleaved materialization rows makes the full set poison.
  #   - check_sweep_suffix_equivalence: the attempt-ledger GC sweep's
  #     structural half, re-stated over the PER-LANE cut (migration
  #     084) — for every bounded history and every deletion mask
  #     confined to attempt-kind rows strictly before THEIR OWN lane's
  #     last reset row (the sweep's E1+E2 conjuncts, kinded; the age
  #     conjunct is deliberately absent, so every age implementation is
  #     a mask-shrinking special case), the LOADED VIEW
  #     (row_survives_load) is element-wise unchanged (MAX=5: no
  #     decide() call, so the structural harness carries the larger
  #     bound).
  #   - check_sweep_decide_invariant: the loader-composed end-to-end
  #     theorem — decide() and materialization_decide() over the loaded
  #     view are bit-identical before/after any structural sweep
  #     (MAX=4, ~2× the check_decide_contract fold cost; documented
  #     fallback to MAX=3 if the gate budget is ever exceeded — the
  #     bounded-exhaustive unit test keeps len<=4 covered regardless).
  #   - check_loader_cut_preserves_materialization_decide: the per-lane
  #     loader cut loses NO materialization-decision information —
  #     materialization_decide over the loaded view equals it over the
  #     full history (under the pre-084 any-kind cut, a trailing build
  #     reset emptied the view and flipped parked verdicts back to
  #     Claimable: merged_bug_011's resurrection class).
  # r[verify sched.retry.transient-budget+2]
  # r[verify sched.retry.attempts-bounded+5]
  # r[verify sched.retry.exempt-infra-cap]
  # r[verify sched.retry.per-executor-budget+4]
  # r[verify sched.dispatch.fleet-exhaust+5]
  # r[verify sched.state.poisoned-ttl]
  # r[verify sched.db.attempts-gc]
  # r[verify sched.attempt.worker-abort-bounded+2]
  kani-rio-retry-kernel = mkKaniCheck {
    name = "rio-retry-kernel";
    crate = crateBuildKani.members.rio-retry-kernel;
    # A2's 12 (kind-partition restatements + worker-abort bound) +
    # B2's check_exec_row_sweep_guards (store.log.sweep-ownership) +
    # A3's check_materialization_counters_window (the per-job budget window) +
    # B1-s2's check_store_degraded_uncharged_requeue (the pacing class
    # charges nothing — sched.retry.store-degraded-uncharged) +
    # bug_098's check_bounded_uncharged_union_composition (the
    # bounded-uncharged classes compose over the UNION trailing run:
    # membership totality + pigeonhole; signed bughunt-3 §5 Q1).
    expectedHarnesses = 18;
  };

  # rio-evidence-kernel: the scheduler's closure-evidence decision kernel
  # (the ClosureEvidence classifier, the merge/establishment
  # predicates, and the pull-admission decision
  # admit_pull()), extracted from rio-scheduler/src/dag/mod.rs +
  # actor/merge.rs + actor/pull.rs into a dependency-free crate so the
  # harnesses' goto model closes over the kernel alone — the
  # closure-evidence campaign's Phase-2 assurance deliverable, following
  # the rio-retry-kernel extraction precedent.
  # DerivationDag::closure_evidence, the merge/dispatch predicates, and
  # the actor pull shim are the projections; the quint survivors core
  # (quint-closure-survivors-*, in quintChecks) checks the surviving
  # lifecycle protocol over these predicates, these harnesses prove the
  # predicates themselves over their full bounded input domain.
  #
  # Measured at ~0.05–0.65 s CBMC time per harness (the count is
  # pinned by expectedHarnesses below) on the dev builder.
  #
  # Classifier (rio-evidence-kernel/src/lib.rs `mod proofs` — reduced
  # to the 2-input domain in T-D5.2: the walk-era hole/mark inputs and
  # the must_substitute predicate died with the evidence columns):
  #   - check_classifier_exhaustive_case_analysis: the classifier's
  #     four-cell partition (absent → Holed; empty → ChildlessLeaf;
  #     non-empty all-produced → Vouched; otherwise Pending) is exact,
  #     total, and panic-free over every bounded child set.
  #   - check_vouched_iff_nonempty_all_produced: Vouched exactly when
  #     present ∧ non-empty ∧ all children produced — the criterion the
  #     pruned-origin exemption keys on.
  #
  # Pull admission (rio-evidence-kernel/src/pull.rs `mod proofs`):
  #   - check_admit_pull_partition: the base table's exhaustive
  #     decision partition over (token, fence, status×12, attempt
  #     identity), via the public fn's kind=Build/no-job path — total
  #     and panic-free.
  #   - check_admit_pull_rejections_dominate: the load-bearing check
  #     order — a mismatched token answers RejectToken whatever else
  #     holds; an authenticated below-floor pull answers
  #     RejectStaleGeneration whatever the node state.
  #   - check_admit_pull_identity_match: DeliverExisting only for an
  #     Assigned/Running node whose open attempt is bound to the
  #     pulling identity, carrying exactly that attempt's exec id.
  #
  # Kinded admission table (rio-evidence-kernel/src/pull.rs
  # `mod kinded_proofs` — design §2.3; THE table since the
  # substitution-replacement cutover):
  #   - check_kinded_no_build_delivery_while_job_unresolved: a
  #     build pull is never delivered (new or re-delivery) while the
  #     node has an unresolved materialization job; rejections keep
  #     their dominance order.
  #   - check_kinded_one_winner_arbitration: a materialization pull is
  #     delivered only when no open attempt is held by a different
  #     identity (BC-1/AS-3): fresh claims need an unparked Pending job
  #     on a Ready node; re-deliveries carry exactly the holder's exec
  #     id, bound to the pulling identity, and never escape the
  #     identity/fence gates; a claim held by another identity never
  #     yields a delivery.
  #   - check_kinded_rejections_dominate: the token/fence dominance
  #     order over the FULL (kind × job-view) domain — every arm of
  #     the kinded table including the Claimed re-delivery arm
  #     answers RejectToken on a mis-bound token and
  #     RejectStaleGeneration on a below-floor pull; nothing else is
  #     ever rejected.
  # r[verify sched.merge.substitute-topdown+13]
  # r[verify sched.executor.pull-gone+1]
  # r[verify sched.executor.pull-not-ready+2]
  # r[verify sched.materialize.job+2]
  kani-rio-evidence-kernel = mkKaniCheck {
    name = "rio-evidence-kernel";
    crate = crateBuildKani.members.rio-evidence-kernel;
    # 10 → 11: + check_cancelled_never_charged (establish.rs — the
    # shared establishment kernel's node axis; §4.R2).
    # 11 → 12: + check_no_build_mint_inside_backoff_window (A3/282 —
    # the named (Build, None) backoff cell; the wide partition proofs
    # already cover it, the named harness pins the conjunct).
    # 13 → 12: − check_the deleted in-memory closure-vouch shim_contract (A4/bug_390 — the
    # the deleted in-memory closure-vouch shim predicate is DELETED with its sole caller, the
    # in-memory merge gate; every consumer reads the durable
    # classifier's 4-cell verdict now; recount composed over B1-s2's
    # 12 → 13 resume-token harness, second-lander reconcile).
    # 12 → 19: + the A4 step-7 battery (bughunt wave):
    #   routing.rs (over the new set-free route_from_classes core —
    #   the establish.rs discipline):
    #   - check_route_no_vacuous_complete (193/194: completion only
    #     from the clean-and-covered cell)
    #   - check_route_total_and_cells_reachable (totality + no dead
    #     arm — the 178-class catch-all regression shape)
    #   - check_childless_leaf_non_pruned_never_failfast (finding 11
    #     generalized: non-pruned never fail-fasts)
    #   outcome.rs:
    #   - check_substitute_failure_truth_table (178 table + 081 loop
    #     fold precedence Stalled > RateLimited > CleanMiss)
    #   - check_confirmed_missing_is_all_tenant_conjunction (028/Q2:
    #     bitmask sweep ≤4 tenants, stack array — no heap under CBMC)
    #   establish.rs:
    #   - check_establishment_unavailable_defers (the C1 fix's pin)
    #   - check_establishment_materialization_never_adopts_or_crash_charges
    #     (the materialization row swept over node × probe).
    #   - check_terminal_settled_never_charged (merged_bug_210: settled
    #     work is never re-litigated — CloseChargeFree over every
    #     kind × probe).
    #   - check_project_node_authority_total (merged_bug_210: the
    #     node-axis projection is total and an un-authoritative DAG
    #     never yields a disposition).
    # 21 → 23: + the settlement-law pair (bug_182/merged_bug_055,
    # settle.rs):
    #   - check_consumption_ack_iff_settled_or_fenced (the ack law's
    #     biconditional — NACK exactly on Failed)
    #   - check_companion_failed_always_releases (the companion law —
    #     the wedged-claim ghost unrepresentable through the law).
    # 23 → 24: + check_report_admission_requires_active_assignment
    # (bug_134, pull.rs): the kind-uniform report fold's full 2×2
    # table — the ProcessAdmission witness is minted exactly on
    # (assignment active ∧ not yet classified), so the materialization
    # arm's deleted hand gate (which ignored `assignment_active`)
    # cannot be reintroduced without this proof going red.
    # 24 → 27: + the substitution evidence-fold trio (merged_bug_044 /
    # merged_bug_133, outcome.rs; check_substitute_failure_truth_table
    # narrows to the classification table — its old 2-axis loop-fold
    # half is superseded by the cells form):
    #   - check_substitute_loop_cells_total (K1: record routing +
    #     Stalled > RateLimited > Errored > CleanMiss precedence vs an
    #     independent shadow over symbolic 2-record sequences)
    #   - check_substitute_cells_errored_axis_required (K1 falsify
    #     twin, should_panic: the pre-fix 2-axis projection cannot
    #     represent the (no-stall, no-429, errored) cell — if Errored
    #     ever collapses back into CleanMiss this twin stops panicking
    #     and the count check flags it)
    #   - check_fold_tenant_attempts_permutation_and_precedence (K2:
    #     charge > transient > all-clean-miss, idx lane-correctness,
    #     permutation invariance via universally-quantified
    #     transpositions).
    # 27 → 27 (substitution, bug_299): the pre-projected-boolean
    # harness check_confirmed_missing_is_all_tenant_conjunction is
    # DELETED with the per-tenant pre-projected fold (its input domain — one
    # boolean per tenant — was the hole: it computes ∀ tenant ∃ path,
    # not ∃ path ∀ tenant) and replaced one-for-one by
    # check_reprobe_quantifier_per_path (K3: the full 3×3
    # per-(tenant,path) cell space vs an independent ∃∀ recomputation
    # at every width, plus kani::cover of the complementary-coverage
    # disagreement matrix vs the old projection — 1 of 1 cover
    # satisfied, 0.6 s).
    # 27 → 28 (substitution, bug_115): +
    # check_visibility_verdict_i217_table (K4: the eight-cell I-217
    # table re-derived independently, plus the two dominance facts the
    # lazy callers rely on — owned wins, and the verdict is
    # independent of sig_trusted once owned || any_built holds, so a
    # caller skipping the trusted-set queries for those rows cannot
    # change the verdict). K5 (probe-polarity congruence) is
    # DELIBERATELY NOT a kani harness (§1.6 "if expressed in kani"):
    # the probe leg consumes the SAME classify_substitute_failure the
    # GET leg does (one truth table, already swept by
    # check_substitute_failure_truth_table), and the e2e pair
    # (HEAD-429 red + upstream_5xx green) pins both polarity halves —
    # a separate harness would re-prove the table against itself.
    # 28 → 29: + check_content_binding_axis_totality (content.rs —
    # merged_bug_114's agreement law as the kernel's ONE body; the
    # AlreadyComplete arm routes through it; agreement iff ALL three
    # axes equal over the bounded domain, so a dropped axis flips the
    # proof red. Bughunt-3 S3 formal obligation: "mismatch ⇒ never
    # Hit").
    # 29 → 37 (bughunt-4 S5a, bug_266): + the K6 generation-stamp
    # fold-refusal family — fold_guard accepts iff every verdict cell
    # carries the final tenant-set generation, drain_stale removes
    # exactly the stale cells, survivors fold iff none is newer;
    # generations <3 is the minimal domain distinguishing <, ==, >
    # final. Two ladder rungs, both measured: the original single
    # harness drove a SYMBOLIC length through Vec<(String,u64)>'s
    # realloc/retain/clone machinery (no progress at 600 s ×2); the
    # per-length combined split still ran past 600 s at N = 2 (the
    # two fold_guard calls + retain in one equation), so lengths 0-1
    # keep the combined body (check_gen_stamped_fold_refusal_len{0,1})
    # and lengths 2-3 split per arm
    # (check_gen_stamped_{guard_truth,drain_totality,post_drain_guard}_len{2,3})
    # — seconds each. kani::cover pins refusal/accept/drain-some/
    # drain-none/newer-survivor reachable. The merged_bug_046
    # ContentMismatch class extension rides the EXISTING three
    # substitute harnesses (truth table, loop cells, tenant fold),
    # which now pick through class_of_index /
    # SUBSTITUTE_FAILURE_CLASS_COUNT — the exhaustive class_index
    # inverse breaks the build on a new variant, so a class can no
    # longer be silently excluded from any sweep (the round-3
    # pick-table lesson made structural).
    # 38 → 40: + the merged_bug_011 fence-obligation pair (pull.rs
    # `mod fence_obligation_proofs`):
    #   - check_fence_obligation_partition: totality + the exact
    #     per-cell table over admission × confirm_only × lane (Gone
    #     and confirm-screened NotYetReady on Keyed ⇒ WriteAhead;
    #     DeliverNew on Keyed ⇒ ScreenRead; non-keyed lanes ⇒ None).
    #   - check_licensing_answers_oblige_fencing: dominance — no
    #     keyed answer that licenses builder exit-0 carries
    #     FenceObligation::None.
    # (C-1 delta rule: +2 over the base at this landing; whichever of
    # S4/S6a lands second re-derives the census from the driver's
    # "Complete - N" line and sets the cumulative.)
    # r[verify sched.executor.confirm-fence]
    # 40 → 44 (bughunt-5 S6a, bug_084): + the per-arm refusal-routing
    # family (routing.rs `mod proofs`, on the set-free
    # route_from_classes core — per-arm split per the K6 lesson, no
    # symbolic collections, no lengths):
    #   - check_refusal_content_settles_from_source /
    #     check_refusal_trust_and_content_settles_from_source /
    #     check_refusal_unrecognized_settles_from_source: each named
    #     Refusal variant × the full bounded domain of every other
    #     axis — refused ∧ anything missing ⟹ ResolveFromSource, with
    #     kani::cover non-vacuity pins (from-source + the refusal-moot
    #     complete/re-arm lanes reachable per harness).
    #   - check_only_unrefused_settlements_rearm_or_failfast: the
    #     architectural close as a proof — FailFast ⟹ Refusal::None
    #     over the whole domain, and an arm-3 ReArm (anything missing)
    #     ⟹ Refusal::None; covers pin both consequents reachable.
    # Count-neutral riders: check_trust_refused_settles_from_source
    # retargets Refusal::Trust; check_route_no_vacuous_complete,
    # check_route_total_and_cells_reachable, and
    # check_childless_leaf_non_pruned_never_failfast redraw their last
    # axis over any_refusal().
    # (C-1 cumulative: 38 → 40 → 44 — S4's fence-obligation pair then
    # S6a's refusal-routing four; second-lander census re-derived from
    # the kani driver's "Complete - N" summary line at this tip.)
    expectedHarnesses = 44;
  };
}
