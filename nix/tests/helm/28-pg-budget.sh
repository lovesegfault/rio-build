# PG connection budget (merged_bug_080): the chart-default store
# ceiling must satisfy its own documented derivation against the
# MODELED Aurora max_connections. The model lives in infra/eks/rds.tf
# (the AWS PG table at the provisioned capacity: min_capacity=1 lifts
# the PG min-capacity cap, max_capacity=32 ⇒ 5,000 — owner decision Q1
# 2026-06-03); `xtask deploy`'s pg preflight enforces the same formula
# against the MEASURED value at deploy time (derive_store_ceiling —
# one formula, two enforcement points; values.yaml documents it).
# This fragment keeps the chart-only fallback honest: if someone bumps
# maxReplicas or pgMaxConnections past the modeled budget, helm-lint
# fails at merge time instead of Aurora failing at 2am.
#
# Worst case for reference (NOT asserted here — the deploy preflight
# derives it live): min_capacity ≤ 0.5 re-caps max_connections at
# 2,000 ⇒ the same formula gives 68.

MODELED_MAX_CONNECTIONS=5000
NON_STORE_BUDGET=34 # scheduler 2×10 + controller 4 + psql headroom 10

out=$TMPDIR/pg-budget.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guards: both surfaces render at defaults.
so=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store")' "$out")
test -n "$so" || {
  echo "FAIL: rio-store ScaledObject did not render at chart defaults — budget assertion vacuous" >&2
  exit 1
}
ceiling=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.maxReplicaCount' "$out")
pgmax=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.containers[0].env[] | select(.name=="RIO_PG_MAX_CONNECTIONS") | .value' "$out" | tr -d '"')
case "$ceiling$pgmax" in
*null* | "")
  echo "FAIL: could not read maxReplicaCount ($ceiling) / RIO_PG_MAX_CONNECTIONS ($pgmax) from the render" >&2
  exit 1
  ;;
esac

# floor(0.70 × modeled) in integer arithmetic: 70 × modeled / 100.
budget=$((70 * MODELED_MAX_CONNECTIONS / 100))
worst=$((ceiling * pgmax + NON_STORE_BUDGET))
test "$worst" -le "$budget" || {
  echo "FAIL: store ceiling ${ceiling}×${pgmax}+${NON_STORE_BUDGET}=${worst} exceeds 70% of the modeled max_connections (${budget}/${MODELED_MAX_CONNECTIONS}) — re-derive values.yaml store.autoscaling.maxReplicas (see infra/eks/rds.tf)" >&2
  exit 1
}

# Configurability: the ceiling is a values knob, not a constant — the
# deploy preflight --sets it from the live measurement.
ovr=$(helm template rio . --set global.image.tag=test --set store.autoscaling.maxReplicas=37 |
  yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.maxReplicaCount')
test "$ovr" = "37" || {
  echo "FAIL: --set store.autoscaling.maxReplicas=37 rendered $ovr" >&2
  exit 1
}

echo "OK: store ceiling ${ceiling}×${pgmax}+${NON_STORE_BUDGET}=${worst} ≤ ${budget} (70% of modeled ${MODELED_MAX_CONNECTIONS}); ceiling configurable"
