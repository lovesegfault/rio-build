# Store scaling surface (decision 5): one-replica-per-node placement +
# the KEDA ScaledObject as the SINGLE owner of the store replica count.
#
# r[verify infra.store.autoscaling+2] (documentary — .sh is not
# tracey-scanned; this fragment is the merge-gate render proof of the
# rule's chart half)
#
# rio-store's three load classes (substitution ingest, builder
# read-serving, builder upload ingest) all share the pod's NIC, NAR
# buffer memory and PG pool — a replica only adds capacity when it
# lands on its own node. Placement is REQUIRED one-per-node
# podAntiAffinity (ceiling-gated, like the gateway): a Pending store
# pod on the untainted on-demand rio-general pool makes Karpenter mint
# a node in ~30-60s, so scale-out is delayed one node-mint, never
# blocked — the pool limit (karpenter.nodePools[rio-general].limits.cpu)
# is the operative scale bound; the KEDA ceiling is only the
# PG-connection safety backstop.
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

# One-per-node placement: REQUIRED podAntiAffinity on the hostname
# topology, label-selected on the store pods (the soft spread is gone —
# required aff subsumes it; Karpenter node-mints absorb the old
# blocked-scale-out concern). Capture-then-grep (not yq | grep -q) —
# the SIGPIPE shape called out in 21-control-plane-readiness.sh.
aff=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.affinity.podAntiAffinity' "$out")
test "$aff" != "null" && test -n "$aff" || {
  echo "FAIL: rio-store Deployment has no podAntiAffinity — replicas can stack on one node and scale-out stops adding NICs (decision 5 placement)" >&2
  exit 1
}
grep -q 'requiredDuringSchedulingIgnoredDuringExecution' <<<"$aff" || {
  echo "FAIL: store podAntiAffinity is not REQUIRED — placement hardening (one NIC per replica) regressed to best-effort" >&2
  exit 1
}
grep -q 'topologyKey: kubernetes.io/hostname' <<<"$aff" || {
  echo "FAIL: store podAntiAffinity not keyed on kubernetes.io/hostname" >&2
  exit 1
}
grep -q 'app.kubernetes.io/name: rio-store' <<<"$aff" || {
  echo "FAIL: store podAntiAffinity labelSelector does not select the store pods" >&2
  exit 1
}
tsc=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.topologySpreadConstraints' "$out")
test "$tsc" = "null" || {
  echo "FAIL: store still renders topologySpreadConstraints alongside required podAntiAffinity — dead config with a now-false SOFT rationale" >&2
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

# Floor 2; ceiling 173 = the PG-connection safety backstop (the
# values.yaml formula comment carries the derivation against the
# 32-ACU Aurora parameter), NOT a product cap — Karpenter binds first.
minr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.minReplicaCount' "$out")
maxr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.maxReplicaCount' "$out")
test "$minr" = "2" && test "$maxr" = "173" || {
  echo "FAIL: store ScaledObject floor/ceiling = $minr/$maxr, expected 2/173 (PG backstop: modeled 5,000 max_connections at min 1 / max 32 ACU; see values.yaml formula + infra/eks/rds.tf)" >&2
  exit 1
}

# The ceiling is values-driven, not hardcoded: a --set override must
# render through (kills any re-hardcode of the retired 14).
cfg=$TMPDIR/store-scaling-cfg.yaml
helm template rio . --set global.image.tag=test \
  --set store.autoscaling.maxReplicas=37 >"$cfg"
maxr_cfg=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.maxReplicaCount' "$cfg")
test "$maxr_cfg" = "37" || {
  echo "FAIL: --set store.autoscaling.maxReplicas=37 rendered maxReplicaCount=$maxr_cfg — the ceiling is not values-driven" >&2
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

# Scale-down damped (1800s window) but geometric once it engages:
# max(25%, 1 pod) per 600s (selectPolicy Max), so an uncapped fleet
# drains 173→2 in ~12-16 periods instead of 171 ticks of Pods-1 alone.
# Scale-up unstabilized.
sd=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds' "$out")
su=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleUp.stabilizationWindowSeconds' "$out")
test "$sd" = "1800" && test "$su" = "0" || {
  echo "FAIL: store ScaledObject stabilization scaleDown/scaleUp = $sd/$su, expected 1800/0" >&2
  exit 1
}
sdp=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown' "$out")
grep -q 'selectPolicy: Max' <<<"$sdp" || {
  echo "FAIL: store scaleDown lost selectPolicy: Max — with multiple policies the HPA default must be explicit" >&2
  exit 1
}
grep -q 'type: Percent' <<<"$sdp" && grep -q 'value: 25' <<<"$sdp" || {
  echo "FAIL: store scaleDown lost the Percent-25 policy — an uncapped fleet would drain at 1 pod / 600s (~17h from the ceiling)" >&2
  exit 1
}
grep -q 'type: Pods' <<<"$sdp" && grep -q 'value: 1' <<<"$sdp" || {
  echo "FAIL: store scaleDown lost the Pods-1 policy — small fleets need the at-least-one-pod drain floor" >&2
  exit 1
}
n_600=$(grep -c 'periodSeconds: 600' <<<"$sdp" || true)
test "$n_600" -eq 2 || {
  echo "FAIL: expected both scaleDown policies at periodSeconds 600, got $n_600" >&2
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

# ── floor asymmetry (mirrors 25-gateway-antiaffinity-autoscaling-floor):
# anti-affinity gates on the CEILING, the PDB on the FLOOR ─────────────
floor=$TMPDIR/store-scaling-floor.yaml
helm template rio . --set global.image.tag=test \
  --set podDisruptionBudget.enabled=true \
  --set store.autoscaling.minReplicas=1 >"$floor"

# Required aff still PRESENT at a 1-pod floor (KEDA can still scale to
# maxReplicas pods that would otherwise bin-pack onto one node).
aff_floor=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.affinity.podAntiAffinity' "$floor")
grep -q 'requiredDuringSchedulingIgnoredDuringExecution' <<<"$aff_floor" || {
  echo "FAIL: rio-store lost required podAntiAffinity at autoscaling minReplicas=1 — the gate must key on the CEILING" >&2
  exit 1
}

# Premise guard for the absence assertion below (the 25-gateway trick):
# the scheduler keeps chart-default replicas=2 here, so its PDB must
# render — proves pdb.yaml was evaluated.
sched_pdb=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-scheduler")' "$floor")
test -n "$sched_pdb" || {
  echo "FAIL: rio-scheduler PDB did not render at default replicas=2 — pdb.yaml is not being evaluated, the rio-store PDB-absence assertion would be vacuous" >&2
  exit 1
}

# Store PDB ABSENT at the 1-pod floor (floor-gated: a disruption budget
# against a single pod either blocks every drain or is a no-op).
pdb_floor=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-store")' "$floor")
test -z "$pdb_floor" || {
  echo "FAIL: rio-store PDB rendered at autoscaling minReplicas=1 — the PDB must gate on the FLOOR" >&2
  exit 1
}

# At chart defaults (floor 2) the store PDB IS present, in the store
# namespace, with the percentage budget (rounds UP: 1 allowed at the
# floor of 2, ~4-5 at Karpenter-bound scale — drains never blocked,
# large rotations parallelize).
pdbout=$TMPDIR/store-scaling-pdb.yaml
helm template rio . --set global.image.tag=test \
  --set podDisruptionBudget.enabled=true >"$pdbout"
pdb_def=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-store")' "$pdbout")
test -n "$pdb_def" || {
  echo "FAIL: rio-store PDB absent at chart defaults (floor 2, podDisruptionBudget.enabled=true)" >&2
  exit 1
}
grep -q 'maxUnavailable: 10%' <<<"$pdb_def" || {
  echo "FAIL: rio-store PDB lost maxUnavailable: 10% (percentage rounds UP — 1 at floor 2, parallelizes large rotations)" >&2
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

# Single-node k3s safety: with autoscaling off and replicas 1, NO
# podAntiAffinity renders (the ceiling-gate's other side).
aff_off=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.affinity.podAntiAffinity' "$off")
test "$aff_off" = "null" || {
  echo "FAIL: rio-store renders podAntiAffinity with autoscaling off + replicas 1 — single-node k3s fixtures would deadlock" >&2
  exit 1
}

echo "OK"
