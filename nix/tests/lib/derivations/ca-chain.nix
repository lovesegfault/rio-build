# Sequential CA-on-CA chain A → B → C for CA early-cutoff validation.
#
# Unlike the IA chain (chain.nix), every node is floating-CA
# (`__contentAddressed = true` + `outputHashMode = "recursive"` +
# `outputHashAlgo = "sha256"`). Each step's output path is computed
# post-build from the NAR hash, and each step references its dep by
# a downstream placeholder (`/1rlll...`) that the scheduler's
# maybe_resolve_ca rewrites to the realized path at dispatch time.
#
# The `marker` arg distinguishes build-1 from build-2: a different
# `marker` → different ATerm for A → different drv hash → scheduler
# treats them as distinct submissions. But the build OUTPUT
# (the chain file contents) is marker-independent — both builds of A
# write the same `"root"` string, so the nar_hash matches, so
# B+C can be Skipped on build-2. That's the whole point: same
# content, different eval-time input.
#
# `sleepSecs` makes each step observable for timing assertions. With
# sleepSecs=8 and 3 steps, build-1 takes ~24s serial. Build-2 with
# cutoff working: A rebuilds (~8s), B+C skip → <15s total.
#
# The `iaLevels` arg stacks N input-addressed steps ABOVE the CA chain
# (A and B always stay floating-CA). This is the GAP-2 / ia.deferred
# case from ADR-018 Appendix B: an IA derivation depending on a CA
# input has that input's placeholder embedded in env/args, so the
# scheduler must resolve it at dispatch.
#
#   iaLevels=0 → A→B→C all floating-CA (default; cutoff test).
#   iaLevels=1 → A→B→C with C input-addressed (1-level ia.deferred).
#   iaLevels=2 → A→B→C→D with C and D input-addressed. CppNix's
#     derivationStrict makes BOTH C and D deferred-IA (empty output
#     path); D's child C is NOT floating-CA, so the gateway's
#     needs_resolve detection must check has_unknown_output_paths
#     (any empty path), not has_ca_floating_outputs (algo set + hash
#     empty), or D dispatches with an unresolved /1<hash> placeholder.
#
# `iaFinal` is a deprecated alias for `iaLevels=1` (kept until callers
# migrate).
{
  busybox,
  marker ? "caa",
  sleepSecs ? 8,
  iaFinal ? false,
  iaLevels ? (if iaFinal then 1 else 0),
}:
let
  inherit (import ./_busybox.nix { inherit busybox; }) bb mkDrv;

  mkStep =
    name: dep: ca:
    mkDrv name
      ''
        set -e
        ${bb} echo "CA-CHAIN building ${name} marker=${marker}" >&2
        ${bb} sleep ${toString sleepSecs}
        ${bb} mkdir -p $out
        ${
          if dep == null then
            # Root: content is "root" regardless of marker. Second
            # build of A with a different marker produces the SAME
            # output bytes → same nar_hash → cutoff-compare matches.
            ''${bb} echo "root" > $out/chain''
          else
            # Downstream: `${dep}` is a placeholder
            # (`/1rlll<hash>-...`) in the ATerm that the scheduler
            # rewrites to the realized store path before dispatch.
            # After rewrite, cat reads the prior step's actual
            # output. Content is the concatenated chain; also
            # marker-independent.
            ''
              ${bb} cat ${dep}/chain > $out/chain
              ${bb} echo "${name}" >> $out/chain
            ''
        }
      ''
      (
        # The marker env var goes into the ATerm (drvs differ), but
        # NOT into $out/chain (contents identical across markers →
        # nar_hash identical → cutoff fires on rebuild).
        {
          CA_CHAIN_MARKER = marker;
        }
        // (
          if ca then
            {
              # Floating-CA: output path computed post-build from the NAR
              # hash. Three attrs required; any one missing → IA fallback.
              __contentAddressed = true;
              outputHashMode = "recursive";
              outputHashAlgo = "sha256";
            }
          else
            { }
        )
      );

  a = mkStep "rio-ca-a" null true;
  b = mkStep "rio-ca-b" a true;
  # iaLevels>=1 → C is input-addressed but depends on floating-CA B.
  # B's output path is a placeholder in C's env until the scheduler's
  # maybe_resolve_ca rewrites it (gated on needs_resolve, set by the
  # gateway when any inputDrv has an unknown output path). Without that
  # rewrite, C's `cat ${b}/chain` references a nonexistent `/1rlll...`
  # path.
  c = mkStep (if iaLevels >= 1 then "rio-ia-c" else "rio-ca-c") b (iaLevels < 1);
  # iaLevels>=2 → D is input-addressed depending on input-addressed
  # deferred C. D's child is NOT floating-CA — regression target for
  # populate_needs_resolve's has_unknown_output_paths predicate.
  d = mkStep "rio-ia-d" c false;
in
if iaLevels >= 2 then d else c
