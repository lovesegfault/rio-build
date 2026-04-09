# r[dash.auth.method-gate] fail-closed proof.
#
# Default values (enableMutatingMethods=false) MUST NOT render the
# mutating HTTPRoute. If this assert fails, a values.yaml typo (or a
# template guard regression) has fail-OPENED ClearPoison/DrainWorker/
# CreateTenant/TriggerGC to any browser that can reach the gateway.
#
# HTTPRoute (not GRPCRoute): grpc-web content-type doesn't match
# GRPCRoute matchers. The scheduler does grpc-web translation in-
# process (D3); the Gateway routes plain HTTP by exact path.
#
# `grep -x >/dev/null`, NOT `grep -qx`: stdenv runs with pipefail.
# grep -q exits on first match → closes pipe → yq's next write SIGPIPEs
# → yq exit 141 → pipeline fails → false FAIL. Go's stdout is unbuffered
# to a pipe, so each output line is a separate write() — grep can
# race-close between them. ~120 bytes fits the 64K pipe buffer normally;
# under 192-core scheduler contention it doesn't. Observed: same drv
# flapped FAIL at different yq sites on consecutive runs. Dropping -q
# makes grep drain the pipe; no SIGPIPE.

dash=$TMPDIR/httproute-dash-on.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$dash"

! yq 'select(.kind=="HTTPRoute") | .metadata.name' "$dash" |
  grep -x rio-scheduler-mutating >/dev/null || {
  echo "FAIL: rio-scheduler-mutating HTTPRoute rendered with default values (enableMutatingMethods should default false)" >&2
  exit 1
}

# Readonly route MUST render and MUST carry ClusterStatus (proves the
# route-split didn't drop the load-bearing unary-test target —
# dashboard-gateway.nix curl depends on ClusterStatus routing).
yq 'select(.kind=="HTTPRoute" and .metadata.name=="rio-scheduler-readonly")
    | .spec.rules[].matches[].path.value' "$dash" |
  grep -x /rio.admin.AdminService/ClusterStatus >/dev/null || {
  echo "FAIL: rio-scheduler-readonly missing ClusterStatus path match" >&2
  exit 1
}

# ClearPoison must NOT leak into the readonly route — a one-line yaml
# indent mistake could silently attach it.
! yq 'select(.kind=="HTTPRoute" and .metadata.name=="rio-scheduler-readonly")
      | .spec.rules[].matches[].path.value' "$dash" |
  grep ClearPoison >/dev/null || {
  echo "FAIL: ClearPoison leaked into readonly HTTPRoute" >&2
  exit 1
}

# Positive: flipping enableMutatingMethods=true DOES render the mutating
# route + ClearPoison. Proves the flag is wired (not a typo'd Values
# path that evals to nil — helm treats undefined as false, so a bad
# path silently gates forever-off).
mut=$TMPDIR/httproute-dash-mut.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set dashboard.enableMutatingMethods=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$mut"
yq 'select(.kind=="HTTPRoute" and .metadata.name=="rio-scheduler-mutating")
    | .spec.rules[].matches[].path.value' "$mut" |
  grep -x /rio.admin.AdminService/ClearPoison >/dev/null || {
  echo "FAIL: enableMutatingMethods=true did not render mutating HTTPRoute with ClearPoison" >&2
  exit 1
}
