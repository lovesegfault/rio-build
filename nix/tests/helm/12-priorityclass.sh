# ADR-023 §13b: 10 PriorityClasses (rio-builder-prio-{0..9}) render
# unconditionally with preemptionPolicy=Never; kube-build-scheduler
# Deployment + RBAC render under buildScheduler.enabled (default on).

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

# kube-build-scheduler: Deployment with schedulerName profile + MostAllocated in
# the ConfigMap, plus the two system:* ClusterRoleBindings.
#
# r38: `grep -q` exits at first match while yq is still writing its ~3KB
# Deployment doc → yq SIGPIPE (141) → pipefail flags the pipeline as
# failed → false-positive FAIL ~16% of runs (see ci-failure-patterns.md
# "stdenv pipefail SIGPIPE"). Capture yq output first; the here-string
# avoids the pipe entirely.
ksdep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="kube-build-scheduler")' "$out")
grep -q 'kube-scheduler' <<<"$ksdep" || {
  echo "FAIL: kube-build-scheduler Deployment missing" >&2
  exit 1
}
# r38/r39: capture yq output before grep -q. `grep -q` exits at first
# match while yq is still writing → SIGPIPE (141) → pipefail flags the
# pipeline as failed. `||`-polarity → false-positive FAIL ~16% of runs;
# `&&`-polarity → the FAIL block is silently skipped, masking a real
# regression (worse). Capture into a var; the here-string avoids the
# pipe entirely. (See ci-failure-patterns.md "stdenv pipefail SIGPIPE".)
kscfg=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="kube-build-scheduler-config") | .data["config.yaml"]' "$out")
grep -q 'type: MostAllocated' <<<"$kscfg" || {
  echo "FAIL: KubeSchedulerConfiguration missing MostAllocated scoring" >&2
  exit 1
}
for crb in kube-build-scheduler kube-build-volume-scheduler; do
  crb_doc=$(yq -N "select(.kind==\"ClusterRoleBinding\" and .metadata.name==\"$crb\")" "$out")
  grep -q 'kind: ClusterRoleBinding' <<<"$crb_doc" || {
    echo "FAIL: ClusterRoleBinding $crb missing" >&2
    exit 1
  }
done

# vmtest-full disables buildScheduler (airgap); PriorityClasses still render.
out2=$TMPDIR/prio-vmtest.yaml
helm template rio . -f values/vmtest-full.yaml >"$out2"
ksdep2=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="kube-build-scheduler")' "$out2")
test -z "$ksdep2" || {
  echo "FAIL: vmtest-full should not render kube-build-scheduler Deployment" >&2
  exit 1
}
n=$(yq -N 'select(.kind=="PriorityClass")' "$out2" | grep -c '^kind:')
test "$n" -eq 10 || {
  echo "FAIL: vmtest-full expected 10 PriorityClasses, got $n" >&2
  exit 1
}

# r37 bug_017: --authorization-always-allow-paths is a StringSlice that
# REPLACES the upstream default /healthz,/readyz,/livez. The probe
# paths must be re-listed explicitly or the kubelet's probe round-trips
# a SubjectAccessReview to the apiserver → crash-loop on brownout.
ksargs=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="kube-build-scheduler") | .spec.template.spec.containers[0].command[]' "$out")
aap=$(grep -- '--authorization-always-allow-paths=' <<<"$ksargs" || true)
grep -q '/healthz' <<<"$aap" || {
  echo "FAIL: kube-build-scheduler --authorization-always-allow-paths must" \
       "include /healthz (StringSlice replaces upstream default — r37 bug_017)" >&2
  exit 1
}

# r37 bug_019: KubeBuildSchedulerPendingStuck must NOT key on
# queue="active" — un-placeable pods sit in unschedulable/backoff.
mon=$TMPDIR/prio-mon.yaml
helm template rio . --set global.image.tag=test --set monitoring.enabled=true >"$mon"
# Use yq to extract the actual `expr` field so the assertion is
# immune to comment-line count drift (the comment block contains
# the literal `queue="active"` as historical reference).
expr=$(yq -N '.spec.groups[].rules[] | select(.alert=="KubeBuildSchedulerPendingStuck") | .expr' "$mon")
# r38 merged_013: the negative-only check passes vacuously if the
# alert is deleted/renamed (`yq` emits nothing → `grep` on "" → 1 →
# `&&` short-circuits). Mirror the `KubeBuildSchedulerDown` positive
# existence shape below.
test -n "$expr" || {
  echo "FAIL: KubeBuildSchedulerPendingStuck alert missing from PrometheusRule" >&2
  exit 1
}
echo "$expr" | grep -q 'queue="active"' && {
  echo "FAIL: KubeBuildSchedulerPendingStuck still keys on queue=\"active\"" \
       "— it spends ~ms there; the alert structurally cannot fire (r37 bug_019)" >&2
  exit 1
}
grep -q 'alert: KubeBuildSchedulerDown' "$mon" || {
  echo "FAIL: KubeBuildSchedulerDown absent() alert missing (r37 bug_019)" >&2
  exit 1
}
