# ADR-023 §13b: 10 PriorityClasses (rio-builder-prio-{0..9}) render
# unconditionally with preemptionPolicy=Never; rio-packed scheduler
# Deployment + RBAC render under packedScheduler.enabled (default on).

out=$TMPDIR/prio.yaml
helm template rio . --set global.image.tag=test >"$out"

# Exactly 10 PriorityClasses, names rio-builder-prio-0..9, all
# preemptionPolicy: Never + globalDefault: false.
got=$(yq -N 'select(.kind=="PriorityClass") | .metadata.name' "$out" | sort)
want=$(printf 'rio-builder-prio-%d\n' 0 1 2 3 4 5 6 7 8 9 | sort)
test "$got" = "$want" || {
  echo "FAIL: PriorityClass names mismatch" >&2
  echo "  got:  $(echo "$got" | tr '\n' ' ')" >&2
  echo "  want: $(echo "$want" | tr '\n' ' ')" >&2
  exit 1
}
n=$(yq -N 'select(.kind=="PriorityClass" and .preemptionPolicy=="Never" and .globalDefault==false)' "$out" | grep -c '^kind:')
test "$n" -eq 10 || {
  echo "FAIL: expected 10 PriorityClasses with preemptionPolicy=Never+globalDefault=false, got $n" >&2
  exit 1
}

# rio-packed: Deployment with schedulerName profile + MostAllocated in
# the ConfigMap, plus the two system:* ClusterRoleBindings.
yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-packed-scheduler")' "$out" \
  | grep -q 'kube-scheduler' || {
  echo "FAIL: rio-packed-scheduler Deployment missing" >&2
  exit 1
}
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-packed-scheduler-config") | .data["config.yaml"]' "$out" \
  | grep -q 'type: MostAllocated' || {
  echo "FAIL: KubeSchedulerConfiguration missing MostAllocated scoring" >&2
  exit 1
}
for crb in rio-packed-scheduler rio-packed-volume-scheduler; do
  yq -N "select(.kind==\"ClusterRoleBinding\" and .metadata.name==\"$crb\")" "$out" \
    | grep -q 'kind: ClusterRoleBinding' || {
    echo "FAIL: ClusterRoleBinding $crb missing" >&2
    exit 1
  }
done

# vmtest-full disables packedScheduler (airgap); PriorityClasses still render.
out2=$TMPDIR/prio-vmtest.yaml
helm template rio . -f values/vmtest-full.yaml >"$out2"
yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-packed-scheduler")' "$out2" \
  | grep -q . && {
  echo "FAIL: vmtest-full should not render rio-packed-scheduler Deployment" >&2
  exit 1
}
n=$(yq -N 'select(.kind=="PriorityClass")' "$out2" | grep -c '^kind:')
test "$n" -eq 10 || {
  echo "FAIL: vmtest-full expected 10 PriorityClasses, got $n" >&2
  exit 1
}
