# Store scaling surface (decision 5): one-replica-per-node placement +
# the KEDA ScaledObject as the SINGLE owner of the store replica count.
#
# r[verify infra.store.autoscaling+4] (documentary — .sh is not
# tracey-scanned; this fragment is the merge-gate render proof of the
# rule's chart half; the scaler-outage fallback half is fragment
# 38-gateway-fallback.sh)
#
# rio-store's three load classes (substitution ingest, builder
# read-serving, builder upload ingest) all share the pod's NIC, NAR
# buffer memory and PG pool — a replica only adds capacity when it
# lands on its own node. Placement is REQUIRED one-per-node
# podAntiAffinity (ceiling-gated, like the gateway): a Pending store
# pod on the on-demand rio-store pool (D1: the store fleet's own
# NodePool) makes Karpenter mint a node in ~30-60s, so scale-out is
# delayed one node-mint, never blocked — the pool limit
# (karpenter.nodePools[rio-store].limits.cpu) is the operative scale
# bound; the KEDA ceiling is only the PG-connection safety backstop.
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
test "$n_avg" -eq 3 || {
  echo "FAIL: expected 3 AverageValue prometheus triggers on the store ScaledObject (backlog, builders, D2 demand inhibitor), got $n_avg" >&2
  exit 1
}

# Scale-down (D2, bughunt-9): the abort-aware fast collapse — 300s
# stabilization (the N=10-scrape debounce) + Percent-100/60s. The
# guards moved from the blanket 1800s damping to the TRIGGERS (HPA
# max-over-triggers: backlog, builders, CPU, and the live_056-c
# demand-side retry inhibitor — see 45-store-scaledown-inhibitor.sh);
# post-collapse claim stranding self-heals via the zero-progress
# reclaim (WO-S5-4, co-derived). Scale-up unstabilized.
sd=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds' "$out")
su=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleUp.stabilizationWindowSeconds' "$out")
test "$sd" = "300" && test "$su" = "0" || {
  echo "FAIL: store ScaledObject stabilization scaleDown/scaleUp = $sd/$su, expected 300/0 (D2: the floor SLO needs the short window; the inhibitor trigger carries the protection)" >&2
  exit 1
}
# D-052-2: scale-up keeps the 0s window (a post-wipe wave must scale
# out the moment the leading signal fires) but the per-period
# COMMITMENT is bounded: Pods 16 / 30s. live_052's raw-backlog signal
# asked 4→173 replicas in 75s against a ~46-node hostable ceiling —
# 133 pods structurally unschedulable. One Pods policy, exactly.
sup=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleUp.policies' "$out")
grep -q 'type: Pods' <<<"$sup" && grep -q 'value: 16' <<<"$sup" && grep -q 'periodSeconds: 30' <<<"$sup" || {
  echo "FAIL: store scaleUp lost the Pods-16/30s policy — an unbounded scale-up commits the whole ceiling in one HPA pass (live_052)" >&2
  exit 1
}
n_sup=$(grep -c 'type: ' <<<"$sup" || true)
test "$n_sup" -eq 1 || {
  echo "FAIL: expected exactly 1 scaleUp policy (Pods 16/30s), got $n_sup — a second policy under the default selectPolicy Max would defeat the bound" >&2
  exit 1
}
sdp=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown' "$out")
grep -q 'selectPolicy: Max' <<<"$sdp" || {
  echo "FAIL: store scaleDown lost selectPolicy: Max — with multiple policies the HPA default must be explicit" >&2
  exit 1
}
grep -q 'type: Percent' <<<"$sdp" && grep -q 'value: 100' <<<"$sdp" || {
  echo "FAIL: store scaleDown lost the Percent-100 fast-collapse policy (D2: floor within T<=5min once the quiet consensus holds)" >&2
  exit 1
}
n_60=$(grep -c 'periodSeconds: 60' <<<"$sdp" || true)
test "$n_60" -eq 1 || {
  echo "FAIL: expected the one scaleDown policy at periodSeconds 60, got $n_60" >&2
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

# ── disruption render-property matrix (merged_bug_378): every rule on
# its NAMED axis. PDB present in ALL store-enabled configs (percentage
# rounds up — harmless at 1); strategy iff FLOOR>1
# (rio.alwaysRunsMultiple); required anti-affinity iff CEILING>1
# (rio.mayRunMultiple). Red-first evidence: against the pre-change
# chart the (floor=1, ceiling=173) row FAILS — strategy rendered on
# the ceiling axis, marking a 1-replica store Available at zero ready
# pods; and the same row's PDB row FAILS — the floor gate left
# KEDA-scaled pods unprotected.
matrix_check() {
  # args: name, extra --set args..., want_strategy(y/n), want_aff(y/n), want_pdb(y/n)
  local name="$1"; shift
  local want_strategy="$1"; shift
  local want_aff="$1"; shift
  local want_pdb="$1"; shift
  local f=$TMPDIR/matrix-$name.yaml
  helm template rio . --set global.image.tag=test \
    --set podDisruptionBudget.enabled=true "$@" >"$f"
  local strat aff pdb
  strat=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.strategy' "$f")
  aff=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.affinity.podAntiAffinity' "$f")
  pdb=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-store")' "$f")
  local got_strategy=n got_aff=n got_pdb=n
  test "$strat" != "null" && test -n "$strat" && got_strategy=y
  test "$aff" != "null" && test -n "$aff" && got_aff=y
  test -n "$pdb" && got_pdb=y
  test "$got_strategy" = "$want_strategy" || {
    echo "FAIL[matrix:$name]: strategy rendered=$got_strategy want=$want_strategy (strategy must key on the FLOOR — rio.alwaysRunsMultiple)" >&2
    exit 1
  }
  test "$got_aff" = "$want_aff" || {
    echo "FAIL[matrix:$name]: required anti-affinity rendered=$got_aff want=$want_aff (aff must key on the CEILING — rio.mayRunMultiple)" >&2
    exit 1
  }
  test "$got_pdb" = "$want_pdb" || {
    echo "FAIL[matrix:$name]: store PDB rendered=$got_pdb want=$want_pdb (PDB renders unconditionally on the scale axes — percentage rounds up)" >&2
    exit 1
  }
}
#            name                 strategy aff pdb   overrides
matrix_check defaults             y        y   y
matrix_check floor1-ceiling-many  n        y   y     --set store.autoscaling.minReplicas=1
matrix_check floor1-ceiling1      n        n   y     --set store.autoscaling.minReplicas=1 --set store.autoscaling.maxReplicas=1
matrix_check off-replicas1        n        n   y     --set store.autoscaling.enabled=false
matrix_check off-replicas3        y        y   y     --set store.autoscaling.enabled=false --set store.replicas=3

# Premise guard for the matrix's PDB assertions: the scheduler PDB
# renders in the same pass (proves pdb.yaml was evaluated, the
# 25-gateway trick).
sched_pdb=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-scheduler")' "$TMPDIR/matrix-defaults.yaml")
test -n "$sched_pdb" || {
  echo "FAIL: rio-scheduler PDB did not render — pdb.yaml not evaluated, matrix PDB assertions vacuous" >&2
  exit 1
}
pdb_def=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-store")' "$TMPDIR/matrix-defaults.yaml")
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
# podAntiAffinity renders (the ceiling-gate's other side; the matrix
# above pins the strategy/PDB cells for this config too).
aff_off=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.affinity.podAntiAffinity' "$off")
test "$aff_off" = "null" || {
  echo "FAIL: rio-store renders podAntiAffinity with autoscaling off + replicas 1 — single-node k3s fixtures would deadlock" >&2
  exit 1
}

echo "OK"
