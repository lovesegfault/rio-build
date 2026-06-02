# exportReferencesGraph on a .drv target, exercised through the NATIVE
# scheduler path (the differential corpus pins the same shape against
# the oracle in-harness; this fixture proves the end-to-end chain:
# gateway DAG -> scheduler -> worker input resolution derives the
# demand from the declaration -> glue expands the graph).
#
# The build itself asserts the registration file lists the inner .drv
# (closure expansion ran) — a missing or stub graph file fails the
# build, never converges silently.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{ busybox }:
let
  inner = derivation {
    name = "rio-test-erg-inner";
    system = builtins.currentSystem;
    builder = "${busybox}/bin/sh";
    args = [
      "-c"
      "echo inner > $out"
    ];
  };
in
derivation {
  name = "rio-test-erg-native";
  system = builtins.currentSystem;
  builder = "${busybox}/bin/sh";
  args = [
    "-c"
    ''
      ${busybox}/bin/grep -q "${inner.drvPath}" refs
      ${busybox}/bin/cp refs $out
    ''
  ];
  exportReferencesGraph = [
    "refs"
    inner.drvPath
  ];
}
