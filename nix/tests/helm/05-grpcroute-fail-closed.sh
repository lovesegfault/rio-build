# r[dash.auth.method-gate] fail-closed proof.
#
# Default values (enableMutatingMethods=false) MUST NOT render the
# mutating GRPCRoute. If this assert fails, a values.yaml typo (or a
# template guard regression) has fail-OPENED ClearPoison/DrainWorker/
# CreateTenant/TriggerGC to any browser that can reach the gateway.
#
# `grep -x >/dev/null`, NOT `grep -qx`: stdenv runs with pipefail.
# grep -q exits on first match → closes pipe → yq's next write SIGPIPEs
# → yq exit 141 → pipeline fails → false FAIL. Go's stdout is unbuffered
# to a pipe, so each output line is a separate write() — grep can
# race-close between them. ~120 bytes fits the 64K pipe buffer normally;
# under 192-core scheduler contention it doesn't. Observed: same drv
# flapped FAIL at different yq sites on consecutive runs. Dropping -q
# makes grep drain the pipe; no SIGPIPE.

dash=$TMPDIR/grpcroute-dash-on.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$dash"

! yq 'select(.kind=="GRPCRoute") | .metadata.name' "$dash" |
  grep -x rio-scheduler-mutating >/dev/null || {
  echo "FAIL: rio-scheduler-mutating GRPCRoute rendered with default values (enableMutatingMethods should default false)" >&2
  exit 1
}

# Readonly route MUST render and MUST carry ClusterStatus (proves the
# route-split didn't drop the load-bearing unary-test target —
# dashboard-gateway.nix curl depends on ClusterStatus routing).
yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
    | .spec.rules[].matches[].method.method' "$dash" |
  grep -x ClusterStatus >/dev/null || {
  echo "FAIL: rio-scheduler-readonly missing ClusterStatus match" >&2
  exit 1
}

# ClearPoison must NOT leak into the readonly route — a one-line yaml
# indent mistake could silently attach it.
! yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
      | .spec.rules[].matches[].method.method' "$dash" |
  grep -x ClearPoison >/dev/null || {
  echo "FAIL: ClearPoison leaked into readonly GRPCRoute" >&2
  exit 1
}

# CORS allowOrigins MUST NOT be wildcard by default. The earlier MVP
# had "*" — regression guard. yq-go `select()` doesn't short-circuit
# like jq; pipe to grep -x instead.
! yq 'select(.kind=="SecurityPolicy" and .metadata.name=="rio-dashboard-cors")
      | .spec.cors.allowOrigins[]' "$dash" |
  grep -x '\*' >/dev/null || {
  echo "FAIL: SecurityPolicy rio-dashboard-cors allowOrigins contains wildcard" >&2
  exit 1
}

# Positive: flipping enableMutatingMethods=true DOES render the mutating
# route + ClearPoison. Proves the flag is wired (not a typo'd Values
# path that evals to nil — helm treats undefined as false, so a bad
# path silently gates forever-off).
mut=$TMPDIR/grpcroute-dash-mut.yaml
helm template rio . \
  --set dashboard.enabled=true \
  --set dashboard.enableMutatingMethods=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$mut"
yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")
    | .spec.rules[].matches[].method.method' "$mut" |
  grep -x ClearPoison >/dev/null || {
  echo "FAIL: enableMutatingMethods=true did not render mutating GRPCRoute with ClearPoison" >&2
  exit 1
}
