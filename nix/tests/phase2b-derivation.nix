# Sequential A→B→C chain for phase 2b validation.
#
# Unlike phase2a's fan-out (4 parallel leaves), this chain forces strict
# ordering — B can't start until A completes, C can't start until B completes.
# With 3 workers and 3 sequential derivations, each worker builds exactly one
# step (scheduler dispatch assigns the next ready derivation to the next idle
# worker).
#
# Each step echoes a distinctive marker to stderr. The VM test greps the
# CLIENT's `nix-build` output for this marker, validating the full log
# pipeline end-to-end: worker LogBatcher → scheduler ring buffer →
# ForwardLogBatch → BuildEvent::Log → gateway STDERR_NEXT → SSH → client.
{ busybox }:
let
  sh = "${busybox}/bin/sh";
  bb = "${busybox}/bin/busybox";

  mkStep =
    name: dep:
    derivation {
      inherit name;
      system = builtins.currentSystem;
      builder = sh;
      args = [
        "-c"
        ''
          set -e
          # stderr (>&2) is what nix-daemon captures as STDERR_NEXT and
          # the worker forwards as LogBatch. stdout would be swallowed.
          ${bb} echo "PHASE2B-LOG-MARKER: building ${name}" >&2
          ${bb} mkdir -p $out
          ${
            if dep == null then
              ''${bb} echo "root" > $out/chain''
            else
              ''
                ${bb} cat ${dep}/chain > $out/chain
                ${bb} echo "${name}" >> $out/chain
              ''
          }
        ''
      ];
    };

  a = mkStep "rio-chain-a" null;
  b = mkStep "rio-chain-b" a;
  c = mkStep "rio-chain-c" b;
in
c
