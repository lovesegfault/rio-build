# r37 bug_020 (§process-strike): every multi-replica control-plane
# Deployment in the system namespace must have podAntiAffinity AND a
# PodDisruptionBudget. rio-scheduler and rio-gateway shipped with both;
# kube-build-scheduler shipped without either. This check enumerates
# every Deployment with replicas>1 in the rendered chart and asserts
# the pair so the NEXT new control-plane component fails CI on day 1.
#
# r38 merged_021: extended to assert `strategy.rollingUpdate.maxUnavailable
# >= 1` for required-anti-affinity Deployments — the apps/v1 default
# 25% rounds DOWN to 0 for N<4, deadlocking rollouts on a fixed-size
# cluster.
#
# r38 merged_013: count guard so the test fails loudly (not vacuously)
# if the yq filter rots and selects nothing.

out=$TMPDIR/cpr.yaml
helm template rio . --set global.image.tag=test \
  --set podDisruptionBudget.enabled=true \
  --set buildScheduler.enabled=true \
  --set scheduler.replicas=2 --set gateway.replicas=2 >"$out"

# Multi-replica Deployments: static `replicas: N` ∪ KEDA-ScaledObject-
# targeted with `minReplicaCount > 1`. r38 (AMEND of r37
# verifier-note): a KEDA-managed Deployment omits `.spec.replicas` so
# helm upgrade doesn't fight the autoscaler, but KEDA holds it at
# minReplicaCount — it is still a multi-replica control-plane
# Deployment and still needs podAntiAffinity + a PDB. rio-gateway and
# rio-store both take this shape (the store's ComponentScaler-era CR
# is gone; its ScaledObject inherited the min:2 floor).
multi_replica=$(
  {
    yq -N 'select(.kind=="Deployment" and (.spec.replicas // 1) > 1) | .metadata.name' "$out"
    yq -N 'select(.kind=="ScaledObject" and (.spec.minReplicaCount // 1) > 1) | .spec.scaleTargetRef.name' "$out"
  } | sort -u
)
n=$(echo "$multi_replica" | grep -c . || true)
# r38 merged_013 (§Stability-tests "nothing → no change"): with the
# explicit --set values above the chart MUST render ≥4 multi-replica
# control-plane Deployments (rio-scheduler, rio-gateway,
# kube-build-scheduler, rio-store via its ScaledObject). 0 means the
# yq filter rotted — fail loudly, don't pass vacuously.
test "$n" -ge 4 || {
  echo "FAIL: expected ≥4 multi-replica Deployments with the explicit --set values, got $n — assertion vacuous" >&2
  echo "$multi_replica" >&2
  exit 1
}

# Documented exemptions: a Deployment listed here is EXPECTED to fail
# the readiness checks; the exemption is visible and tracked.
# - rio-store: KEDA-ScaledObject-managed (min:2..max:14) with a SOFT
#   one-per-node topologySpreadConstraint (26-store-scaling.sh) but no
#   required podAntiAffinity / PDB. Hardening to required rules is a
#   deliberate change requiring the I-064-style trade-off analysis
#   (no sticky sessions, S3+PG-backed, surge-first may not apply, and
#   a required spread would block scale-out on a full pool).
#   TODO(rio-store anti-affinity).
exempt_aff_pdb="rio-store"
# - rio-gateway: required podAntiAffinity + maxUnavailable: 0 is a
#   DELIBERATE I-064 trade-off (NLB drain race > rollout cost). See
#   gateway.yaml's strategy comment.
exempt_strategy="rio-gateway"

# Process substitution (NOT `cmd | while`) so an `exit 1` inside the
# loop body unconditionally terminates the script — `cmd | while` only
# propagates the exit because `while` is the last pipeline component
# under `set -e`, which is one runner-config change from silently
# becoming a no-op.
while read -r dep; do
  case " $exempt_aff_pdb " in *" $dep "*) continue ;; esac
  # podAntiAffinity (required)
  # r39: capture yq output before grep -q. Same SIGPIPE shape as the
  # yq | grep -q pipes swept in 12-priorityclass.sh — `grep -q` exits
  # at first match → yq SIGPIPE (141) → pipefail flags the pipeline →
  # false-positive FAIL. The here-string avoids the pipe.
  aff=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\") | .spec.template.spec.affinity.podAntiAffinity" "$out")
  grep -q 'requiredDuringSchedulingIgnoredDuringExecution' <<<"$aff" || {
    echo "FAIL: Deployment $dep has replicas>1 but no required podAntiAffinity" >&2
    exit 1
  }
  # PDB
  pdb=$(yq -N "select(.kind==\"PodDisruptionBudget\" and (.metadata.name | test(\"$dep|rio-${dep#rio-}\")))" "$out")
  test -n "$pdb" || {
    echo "FAIL: Deployment $dep has replicas>1 but no PodDisruptionBudget" >&2
    exit 1
  }
done < <(echo "$multi_replica")

# r38 merged_021: every Deployment with REQUIRED anti-affinity must
# have `strategy.rollingUpdate.maxUnavailable >= 1` (or a documented
# exemption above). apps/v1 default is 25% → ⌊N×0.25⌋ which rounds
# DOWN to 0 for N<4 → rollout deadlock when no 3rd node is available.
while read -r dep; do
  case " $exempt_aff_pdb $exempt_strategy " in *" $dep "*) continue ;; esac
  has_aff=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\") | .spec.template.spec.affinity.podAntiAffinity.requiredDuringSchedulingIgnoredDuringExecution | length" "$out")
  test "$has_aff" -gt 0 || continue
  mu=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\") | .spec.strategy.rollingUpdate.maxUnavailable // 0" "$out")
  test "$mu" -ge 1 || {
    echo "FAIL: Deployment $dep has required podAntiAffinity but maxUnavailable=$mu (default rounds DOWN to 0) — rollout deadlocks on a fixed-size cluster (r38 merged_021)" >&2
    exit 1
  }
done < <(echo "$multi_replica")

echo "OK"
