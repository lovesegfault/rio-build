# r37 bug_020 (§process-strike): every multi-replica control-plane
# Deployment in the system namespace must have podAntiAffinity AND a
# PodDisruptionBudget. rio-scheduler and rio-gateway shipped with both;
# kube-build-scheduler shipped without either. This check enumerates
# every Deployment with replicas>1 in the rendered chart and asserts
# the pair so the NEXT new control-plane component fails CI on day 1.

out=$TMPDIR/cpr.yaml
helm template rio . --set global.image.tag=test \
  --set podDisruptionBudget.enabled=true \
  --set buildScheduler.enabled=true \
  --set scheduler.replicas=2 --set gateway.replicas=2 >"$out"

# Every Deployment with replicas > 1 must have podAntiAffinity.
yq -N 'select(.kind=="Deployment" and (.spec.replicas // 1) > 1) | .metadata.name' "$out" \
  | while read -r dep; do
  yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\") | .spec.template.spec.affinity.podAntiAffinity" "$out" \
    | grep -q 'requiredDuringSchedulingIgnoredDuringExecution' || {
    echo "FAIL: Deployment $dep has replicas>1 but no required podAntiAffinity" >&2
    exit 1
  }
done

# Every Deployment with replicas > 1 must have a PodDisruptionBudget.
yq -N 'select(.kind=="Deployment" and (.spec.replicas // 1) > 1) | .metadata.name' "$out" \
  | while read -r dep; do
  pdb=$(yq -N "select(.kind==\"PodDisruptionBudget\" and (.metadata.name | test(\"$dep|rio-${dep#rio-}\")))" "$out")
  test -n "$pdb" || {
    echo "FAIL: Deployment $dep has replicas>1 but no PodDisruptionBudget" >&2
    exit 1
  }
done
