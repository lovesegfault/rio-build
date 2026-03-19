# Non-FOD sentinel: dumps the builder's env to $out.
#
# fod-proxy scenario asserts `http_proxy` is ABSENT here. executor/mod.rs's
# is_fod gate means the daemon spawned for THIS build gets no proxy env at
# all (not just stripped by the sandbox — never set in the first place).
# If the gate were missing, the daemon would always get http_proxy and
# the non-FOD sandbox might or might not strip it → flaky. The gate makes
# the assertion deterministic.
#
# Evaluated IN THE VM via nix-build — same `{ busybox }:` pattern as the
# other path-literal fixtures.
{ busybox }:
derivation {
  name = "rio-env-dump";
  builder = "${busybox}/bin/sh";
  # `env` applet → $out. No FOD attrs (no outputHash) → non-FOD build.
  args = [
    "-c"
    "${busybox}/bin/busybox env > $out"
  ];
  system = builtins.currentSystem;
}
