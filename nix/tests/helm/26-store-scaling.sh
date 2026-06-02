# Store scaling surface (decision 5): one-replica-per-node placement +
# the KEDA ScaledObject as the SINGLE owner of the store replica count.
#
# r[verify infra.store.autoscaling] (documentary — .sh is not
# tracey-scanned; this fragment is the merge-gate render proof of the
# rule's chart half)
#
# rio-store's three load classes (substitution ingest, builder
# read-serving, builder upload ingest) all share the pod's NIC, NAR
# buffer memory and PG pool — a replica only adds capacity when it
# lands on its own node. The spread is SOFT (ScheduleAnyway): scale-out
# must never be blocked by a temporarily full node pool; under normal
# capacity each replica still gets its own NIC.
#
# The prometheus→KEDA→replica half cannot run in the k3s VM fixture
# (the airgapped image set carries no KEDA operator) — THIS render
# check is the merge-gate proof of the ScaledObject surface; the live
# loop is validated on EKS per the deployment checklist (P10).

out=$TMPDIR/store-scaling.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: the store Deployment renders at defaults.
dep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")' "$out")
test -n "$dep" || {
  echo "FAIL: rio-store Deployment did not render at chart defaults — assertions vacuous" >&2
  exit 1
}

# One-per-node spread: maxSkew 1 on the hostname topology, soft
# (ScheduleAnyway), label-selected on the store pods. Capture-then-grep
# (not yq | grep -q) — the SIGPIPE shape called out in
# 21-control-plane-readiness.sh.
tsc=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.topologySpreadConstraints' "$out")
test "$tsc" != "null" && test -n "$tsc" || {
  echo "FAIL: rio-store Deployment has no topologySpreadConstraints — replicas can stack on one node and scale-out stops adding NICs (decision 5 placement)" >&2
  exit 1
}
grep -q 'maxSkew: 1' <<<"$tsc" || {
  echo "FAIL: store topologySpreadConstraints lost maxSkew: 1" >&2
  exit 1
}
grep -q 'topologyKey: kubernetes.io/hostname' <<<"$tsc" || {
  echo "FAIL: store topologySpreadConstraints not keyed on kubernetes.io/hostname" >&2
  exit 1
}
grep -q 'whenUnsatisfiable: ScheduleAnyway' <<<"$tsc" || {
  echo "FAIL: store spread must be SOFT (ScheduleAnyway) — DoNotSchedule would block scale-out on a full pool" >&2
  exit 1
}
grep -q 'app.kubernetes.io/name: rio-store' <<<"$tsc" || {
  echo "FAIL: store spread labelSelector does not select the store pods" >&2
  exit 1
}

# ── ScaledObject: exactly one writer of .spec.replicas ───────────────

# Default render (store.autoscaling.enabled=true): the ScaledObject
# owns the count; the Deployment must NOT carry .spec.replicas or
# every helm upgrade resets it and fights the KEDA-managed HPA.
so=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store")' "$out")
test -n "$so" || {
  echo "FAIL: rio-store ScaledObject did not render at chart defaults (store.autoscaling.enabled defaults true)" >&2
  exit 1
}
target=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.scaleTargetRef.name' "$out")
test "$target" = "rio-store" || {
  echo "FAIL: store ScaledObject targets $target, expected rio-store" >&2
  exit 1
}
reps=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.replicas' "$out")
test "$reps" = "null" || {
  echo "FAIL: store Deployment renders .spec.replicas=$reps WITH autoscaling enabled — helm upgrade would reset the count and fight the HPA (the lookup-echo branch must render nothing under helm template)" >&2
  exit 1
}

# Floor/ceiling inherit the retired ComponentScaler CR's bounds (2/14
# — the 14 is the Aurora-connection note in values.yaml).
minr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.minReplicaCount' "$out")
maxr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.maxReplicaCount' "$out")
test "$minr" = "2" && test "$maxr" = "14" || {
  echo "FAIL: store ScaledObject floor/ceiling = $minr/$maxr, expected 2/14 (inherited from the retired ComponentScaler CR)" >&2
  exit 1
}

# The three triggers (KEDA takes the max): substitution backlog
# (leading, class 1), builders-per-replica (leading, classes 2-3 —
# keyed to open attempts, the busy-fleet gauge), CPU (reactive
# corrective). Both prometheus triggers are AverageValue (per-replica
# targets).
trig=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers' "$out")
grep -q 'rio_scheduler_substituting_derivations' <<<"$trig" || {
  echo "FAIL: store ScaledObject lost the substitution-backlog trigger (rio_scheduler_substituting_derivations) — the leading class-1 signal" >&2
  exit 1
}
grep -q 'rio_scheduler_open_attempts' <<<"$trig" || {
  echo "FAIL: store ScaledObject lost the builders-per-replica trigger (rio_scheduler_open_attempts) — the leading class-2/3 signal" >&2
  exit 1
}
grep -q 'type: cpu' <<<"$trig" || {
  echo "FAIL: store ScaledObject lost the cpu saturation trigger — the reactive corrective term" >&2
  exit 1
}
n_avg=$(grep -c 'metricType: AverageValue' <<<"$trig" || true)
test "$n_avg" -eq 2 || {
  echo "FAIL: expected 2 AverageValue prometheus triggers on the store ScaledObject, got $n_avg" >&2
  exit 1
}

# Scale-down damped (1800s window, -1 pod / 600s), scale-up
# unstabilized — the gateway's policy.
sd=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds' "$out")
su=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleUp.stabilizationWindowSeconds' "$out")
test "$sd" = "1800" && test "$su" = "0" || {
  echo "FAIL: store ScaledObject stabilization scaleDown/scaleUp = $sd/$su, expected 1800/0" >&2
  exit 1
}

# The store ComponentScaler CR is GONE — exactly one writer. (The CRD
# and reconciler stay for future targets; the chart just defines no
# CR.) Any ComponentScaler in the default render is a regression.
cs=$(yq -N 'select(.kind=="ComponentScaler")' "$out")
test -z "$cs" || {
  echo "FAIL: a ComponentScaler CR renders at chart defaults — two writers of the store replica count:" >&2
  echo "$cs" >&2
  exit 1
}

# ── autoscaling off (the no-KEDA overlays): static replicas ──────────
off=$TMPDIR/store-scaling-off.yaml
helm template rio . --set global.image.tag=test \
  --set store.autoscaling.enabled=false >"$off"
so_off=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store")' "$off")
test -z "$so_off" || {
  echo "FAIL: store ScaledObject renders with autoscaling disabled" >&2
  exit 1
}
reps_off=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.replicas' "$off")
test "$reps_off" = "1" || {
  echo "FAIL: with store.autoscaling.enabled=false the Deployment must render the static replica count (1), got $reps_off — the k3s VM fixtures and local dev depend on this" >&2
  exit 1
}

echo "OK"
