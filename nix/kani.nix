# Per-crate Kani verification: read each #[kani::proof] harness from
# crateBuildKani's kani-metadata.json and run the CBMC pipeline.
#
# Why this file exists at all: `cargo kani` / `kani-driver` insist on
# driving the *compile* phase themselves (cargo subcommand → rustc
# wrapper). They cannot consume pre-built goto-C artifacts, so they
# would re-compile the whole workspace inside one derivation and lose
# crate2nix's per-crate caching. Instead, crateBuildKani (flake.nix)
# compiles every workspace member with kani-compiler and stashes the
# `*.symtab.out` + `kani-metadata.json` sidecars in the crate's `lib/`
# output. This file replicates kani-driver's *verify* phase
# (call_goto_instrument.rs + call_cbmc.rs) over those sidecars.
#
# Caching model:
#   - kani-compiler compile phase: per-crate cached via crateBuildKani.
#     Editing rio-common rebuilds rio-common's kani drv + downstream
#     dependents; editing a test file rebuilds nothing.
#   - CBMC verify phase (this file): re-runs when the workspace member's
#     own kani drv changes, which transitively means when the member or
#     any reachable dep changes. Correct — `--reachability=harnesses`
#     produces a goto program that *closes over* the harness's call
#     graph, so a behavior-relevant edit anywhere in that closure changes
#     the symtab and therefore this drv's input hash.
#
# Limitation: only lib-only workspace members. Members with [[bin]] hit
# a link error in crateBuildKani — deps are MIR-only (no codegen,
# `--reachability=none`) so the bin step has no object code to link.
# See nix/crate2nix.nix's `excludeCrateTypes` for the cdylib analogue
# (same root cause, different output type). rio-lease is lib-only, so
# this is fine; rio-{gateway,builder,...} need a `[lib]` split or a
# crateBuildKani change before they can be wired here.
#
# Pipeline shape (replicates kani-driver, verified against spike #2;
# kani 0.67.0, cbmc 6.8.0):
#   1. goto-cc symtab + kani_lib.c → goto binary  (link_goto_binary)
#   2. goto-cc --function <harness>               (specialize_to_proof_harness)
#   3. goto-instrument --add-library              (CBMC C library models)
#   4. goto-instrument --generate-function-body   (assert-false on undefined fns)
#   5. goto-instrument --ensure-one-backedge-per-target
#   6. cbmc                                       (the actual verification)
#
# Caveat: kani-compiler is invoked WITHOUT `--assertion-reach-checks`
# (see kaniBaseFlags in flake.nix). With reach checks enabled, raw cbmc
# reports FAILED on every harness even when the property holds —
# kani-driver's JSON post-processor flips the semantics, and we don't
# have it. Without reach checks the cbmc verdict is direct.
#
# r[verify ...] markers: when rio-lease (or any other member) gains
# #[kani::proof] harnesses, the markers go HERE — at the `kani-checks`
# attrset entry that wires the member, NOT in the harness function or
# the .typ spec source. Same wiring-point discipline as the
# `subtests = [...]` entries in nix/tests/default.nix (P0341): a marker
# at the wiring point structurally proves the harness is built; a marker
# in election.rs would claim "verified" even if the kani-checks entry
# were deleted. .config/tracey/config.styx already lists this file under
# `test_include`.
{
  pkgs,
  kaniToolchain,
  crateBuildKani,
}:
let
  # cbmc/kissat are version-asserted in nix/kani-toolchain.nix (kani
  # 0.67.0 pins cbmc 6.8.0, kissat >= 4.0.1). Reference them via the
  # `kani` derivation's passthru so the assertion fires when any
  # `mkKaniCheck` derivation is forced (e.g. `nix build
  # .#kani-toolchain.kani-checks.kani-rio-lease`) — a `nix flake update`
  # that drifts cbmc fails there with a named pin instead of weeks later
  # with a goto-cc segfault.
  inherit (kaniToolchain.kani) cbmc kissat;
  inherit (pkgs) jq;

  # Verify every #[kani::proof] harness in one workspace member's
  # crateBuildKani output.
  #
  #   name:  short member name, used in the derivation name and log lines
  #   crate: crateBuildKani.members.<name> (a buildRustCrate drv whose
  #          `lib` output holds `lib/*.kani-metadata.json` + symtabs);
  #          OR a `{ lib = <path>; }` shim for ad-hoc testing.
  mkKaniCheck =
    { name, crate }:
    pkgs.runCommand "kani-${name}"
      {
        nativeBuildInputs = [
          cbmc # goto-cc, goto-instrument, cbmc
          kissat # external SAT solver (per-harness opt-in via attributes.solver)
          jq # metadata parsing
          pkgs.gcc # goto-cc execvp's `cc -E` to preprocess kani_lib.c;
          # nixpkgs cbmc does NOT propagate a compiler. A devshell
          # masks this gap (host PATH has cc); the sandbox does not.
        ];
        # buildRustCrate's `lib` output holds `lib/{*.rlib,*.symtab.out,
        # *.kani-metadata.json}`. The `or crate` fallback supports the
        # `{ lib = <store-path>; }` shim used for testing the pipeline
        # against ad-hoc spike artifacts.
        src = crate.lib or crate;
        # The ~125-LoC C runtime stubs goto-cc links into every harness
        # (kani-driver ships these as $sysroot/library/kani/kani_lib.c).
        kaniLib = "${kaniToolchain.kani}/library/kani/kani_lib.c";
        # Spike #2: cbmc/console emit ANSI escapes if stdout is a PTY.
        # The sandbox isn't one, but belt-and-suspenders — we grep cbmc
        # output and ANSI codes would defeat the match.
        env.NO_COLOR = "1";
        # Surfaced in `nix log` and error messages.
        env.MEMBER = name;
      }
      ''
        set -euo pipefail

        artifacts="$src/lib"
        if [ ! -d "$artifacts" ]; then
          echo "ERROR: $artifacts does not exist." >&2
          echo "  Expected a crateBuildKani lib output (lib/*.kani-metadata.json + symtabs)." >&2
          exit 1
        fi

        # 1. Locate the per-crate metadata. crateBuildKani emits exactly
        # one *.kani-metadata.json per workspace member; the filename
        # carries a metadata hash so we glob.
        shopt -s nullglob
        metas=("$artifacts"/*.kani-metadata.json)
        shopt -u nullglob
        if [ "''${#metas[@]}" -ne 1 ]; then
          echo "ERROR: expected exactly one *.kani-metadata.json in $artifacts, found ''${#metas[@]}." >&2
          ls -la "$artifacts" >&2
          echo "  This usually means crateBuildKani didn't pass --reachability=harnesses" >&2
          echo "  for $MEMBER (check localExtraRustcOpts in flake.nix)." >&2
          exit 1
        fi
        meta="''${metas[0]}"
        # kani-driver also iterates .test_harnesses (project.rs:55), but
        # crateBuildKani is lib-only so it is always [] — a future
        # --test-mode wiring would need to walk both arrays.
        n_harnesses=$(jq '.proof_harnesses | length' "$meta")

        echo "kani verify: $MEMBER ($n_harnesses harness(es)) — $meta"

        # 2. Vacuous pass on zero harnesses. NOT an error: a workspace
        # member can be wired into kani-checks before its #[kani::proof]
        # contracts land (the rio-lease FV plan does exactly that). The
        # vacuous result is recorded so a `cat result` is unambiguous —
        # silence here would look like a pipeline that never ran.
        if [ "$n_harnesses" -eq 0 ]; then
          {
            echo "kani verify: $MEMBER"
            echo "0 #[kani::proof] harnesses found — vacuous pass."
            echo
            echo "This is expected until the rio-lease FV plan adds harnesses."
            echo "Once they land, promote kani-$MEMBER from packages.* to checks.*"
            echo "and add r[verify ...] markers at the kani-checks attr in nix/kani.nix."
            echo
            echo "metadata: $meta"
          } > "$out"
          cat "$out"
          exit 0
        fi

        # 3+. Verify each harness. Write a one-line verdict per harness
        # to $out; the full per-step + cbmc logs go to stderr (nix log).
        : > "$out"
        scratch=$(mktemp -d)

        for idx in $(seq 0 $(( n_harnesses - 1 ))); do
          h=$(jq -c ".proof_harnesses[$idx]" "$meta")
          pretty=$(jq -r '.pretty_name'             <<<"$h")
          mangled=$(jq -r '.mangled_name'           <<<"$h")
          goto_file=$(jq -r '.goto_file'            <<<"$h")
          unwind=$(jq -r '.attributes.unwind_value' <<<"$h")
          solver=$(jq -r '.attributes.solver'       <<<"$h")
          contract=$(jq -r '.contract'              <<<"$h")
          loop_contracts=$(jq -r '.has_loop_contracts' <<<"$h")
          should_panic=$(jq -r '.attributes.should_panic' <<<"$h")

          echo "=== [$((idx + 1))/$n_harnesses] $pretty ($mangled) ===" >&2

          # Hard-fail on harness features this pipeline can't replicate
          # yet. A silent skip would let an unverified contract claim
          # "verified" — strictly worse than no pipeline at all.
          if [ "$contract" != "null" ]; then
            echo "ERROR: harness $pretty declares a function contract." >&2
            echo "  Contract harnesses need an extra goto-instrument pass" >&2
            echo "  (--apply-loop-contracts / --enforce-contract); see" >&2
            echo "  kani-driver call_goto_instrument.rs:173. Not yet" >&2
            echo "  supported in nix/kani.nix — extend the pipeline before" >&2
            echo "  adding contract harnesses to $MEMBER." >&2
            exit 1
          fi
          if [ "$loop_contracts" = "true" ]; then
            echo "ERROR: harness $pretty has loop contracts." >&2
            echo "  Loop-contract harnesses need --apply-loop-contracts and a" >&2
            echo "  decreases-clause check; see kani-driver" >&2
            echo "  call_goto_instrument.rs::instrument_contracts. Not yet" >&2
            echo "  supported in nix/kani.nix." >&2
            exit 1
          fi
          if [ "$should_panic" = "true" ]; then
            echo "ERROR: harness $pretty has #[kani::should_panic] which inverts the verdict." >&2
            echo "  This pipeline does not implement the inversion (kani-driver" >&2
            echo "  call_cbmc.rs::verification_outcome_from_properties); a should_panic" >&2
            echo "  harness that fails to panic would SILENTLY PASS here." >&2
            echo "  Either drop should_panic from the harness or implement the inversion." >&2
            exit 1
          fi

          # Per-harness conditional cbmc args. unwind_value → bound the
          # symbolic execution.
          #
          # Unwinding assertions: CBMC 6.0+ enables `--unwinding-assertions`
          # by default, and we do not pass `--no-unwinding-assertions`. An
          # under-unwound harness (`#[kani::unwind(N)]` smaller than the
          # actual loop bound) fails loudly: cbmc rc=10 + `unwinding
          # assertion loop 0: FAILURE` + `VERIFICATION FAILED` — caught by
          # the verdict gate below. No extra flag needed. kani-driver
          # itself doesn't pass `--unwinding-assertions` either; it passes
          # `--no-self-loops-to-assumptions` (the unwinding_on() arm of
          # cbmc_check_flags()), which we already include below.
          unwind_args=()
          if [ "$unwind" != "null" ]; then
            unwind_args=(--unwind "$unwind")
          fi

          # Solver selection. kani-driver defaults to cadical (built into
          # cbmc); harnesses can opt into kissat via
          # #[kani::solver(kissat)]. The metadata serializes the enum
          # variant — match case-insensitively in case kani changes the
          # serde rename policy.
          solver_args=()
          case "''${solver,,}" in
            null | cadical) solver_args=(--sat-solver cadical) ;;
            kissat)         solver_args=(--external-sat-solver kissat) ;;
            *)
              echo "ERROR: harness $pretty requests SAT solver '$solver'." >&2
              echo "  nix/kani.nix supports cadical (default) and kissat only." >&2
              echo "  Add the solver to nativeBuildInputs and this case arm." >&2
              exit 1
              ;;
          esac

          # goto_file is a relative path under crateBuildKani's
          # OUT_DIR-equivalent (e.g. `target/lib/<crate>-<hash>__<mangled>.symtab.out`).
          # Only the basename is portable — resolve it against the
          # artifact dir we were handed.
          symtab="$artifacts/$(basename "$goto_file")"
          if [ ! -e "$symtab" ]; then
            echo "ERROR: symtab $symtab not found (goto_file=$goto_file)." >&2
            ls -la "$artifacts" >&2
            exit 1
          fi

          gb="$scratch/$idx.goto"
          cbmc_log="$scratch/$idx.cbmc.log"

          # The 6-step kani-driver verify pipeline, in order. Each
          # goto-instrument call is idempotent (rewrites $gb in place).
          echo "  1/6 link_goto_binary" >&2
          goto-cc "$symtab" "$kaniLib" -o "$gb"

          echo "  2/6 specialize_to_proof_harness" >&2
          goto-cc "$gb" --function "$mangled" -o "$gb"

          echo "  3/6 add_library" >&2
          goto-instrument --add-library --no-malloc-may-fail "$gb" "$gb"

          echo "  4/6 undefined_functions" >&2
          goto-instrument \
            --generate-function-body-options assert-false-assume-false \
            --generate-function-body '.*' \
            --drop-unused-functions \
            "$gb" "$gb"

          echo "  5/6 rewrite_back_edges" >&2
          goto-instrument --ensure-one-backedge-per-target "$gb" "$gb"

          echo "  6/6 cbmc" >&2
          # Capture cbmc output to a file rather than gating on exit
          # code alone: cbmc returns 0=SUCCESSFUL, 10=FAILED, but a
          # crash mid-encode can also exit 0 with no verdict line. We
          # require BOTH exit 0 AND the literal "VERIFICATION
          # SUCCESSFUL" string. set -e is suspended around the cbmc
          # call so a non-zero exit still dumps the log.
          rc=0
          cbmc \
            --no-malloc-may-fail \
            --no-undefined-shift-check \
            --no-signed-overflow-check \
            --nan-check \
            --no-self-loops-to-assumptions \
            --no-pointer-primitive-check \
            --object-bits 16 \
            --slice-formula \
            "''${solver_args[@]}" \
            "''${unwind_args[@]}" \
            "$gb" \
            --verbosity 9 \
            > "$cbmc_log" 2>&1 || rc=$?

          # Echo the full cbmc transcript into the build log so
          # `nix log` carries the per-property breakdown.
          cat "$cbmc_log" >&2

          if [ "$rc" -ne 0 ]; then
            echo "ERROR: cbmc exited $rc on harness $pretty (10 = VERIFICATION FAILED, 6 = parse/conversion error)." >&2
            tail -n 40 "$cbmc_log" >&2
            exit 1
          fi
          if ! grep -q 'VERIFICATION SUCCESSFUL' "$cbmc_log"; then
            echo "ERROR: cbmc exited 0 but did not print 'VERIFICATION SUCCESSFUL' for $pretty." >&2
            echo "  (NO_COLOR=1 is set, so this is not an ANSI-escape mismatch.)" >&2
            tail -n 40 "$cbmc_log" >&2
            exit 1
          fi

          echo "$pretty: VERIFICATION SUCCESSFUL" >> "$out"
          # cbmc's per-run summary line, e.g. "** 0 of 12 failed (1 iterations)".
          grep -E '^\*\* [0-9]+ of [0-9]+ failed' "$cbmc_log" >> "$out" || true
        done

        echo >> "$out"
        echo "Complete - $n_harnesses successfully verified harnesses, 0 failures, $n_harnesses total." >> "$out"
        cat "$out"
      '';
in
{
  # Expose the constructor so ad-hoc spike tests (and a future
  # cross-member aggregate) can build their own checks without going
  # through the named attrs below.
  inherit mkKaniCheck;

  # rio-lease is the first kani-instrumented member (lease/election
  # state machine — small, self-contained, and an actual correctness
  # nightmare to hand-test). Currently 0 harnesses (vacuous pass);
  # the rio-lease FV plan adds #[kani::proof] contracts and promotes
  # this to checks.*. Promotion target: the `// {` block at the tail of
  # flake.nix's `checks =` definition where `cov-smoke` and
  # `mutants-smoke` live — those are the precedent for manual-target →
  # gated-check promotion.
  kani-rio-lease = mkKaniCheck {
    name = "rio-lease";
    crate = crateBuildKani.members.rio-lease;
  };
}
