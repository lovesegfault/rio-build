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
{
  busybox,
  marker ? "caa",
  sleepSecs ? 8,
}:
let
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  mkStep =
    name: dep:
    derivation {
      inherit name;
      system = builtins.currentSystem;
      builder = sh;
      # Floating-CA: output path computed post-build from the NAR
      # hash. Three attrs required; any one missing → IA fallback.
      __contentAddressed = true;
      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
      # The marker env var goes into the ATerm (drvs differ), but
      # NOT into $out/chain (contents identical across markers →
      # nar_hash identical → cutoff fires on rebuild).
      CA_CHAIN_MARKER = marker;
      args = [
        "-c"
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
      ];
    };

  a = mkStep "rio-ca-a" null;
  b = mkStep "rio-ca-b" a;
  c = mkStep "rio-ca-c" b;
in
c
