# Scaler-outage posture (KEDA spec.fallback) on the two ScaledObjects.
#
# r[verify infra.store.autoscaling+4] (documentary — .sh is not
# tracey-scanned; this fragment is the merge-gate render proof of the
# rule's fallback half)
#
# The gateway ScaledObject has exactly ONE trigger — the prometheus
# sessions-per-pod query. Without spec.fallback a prometheus outage
# leaves KEDA with zero healthy scalers and the HPA frozen at whatever
# replica count the outage caught (fine at steady state, refused
# sessions if the outage coincides with a surge). fallback feeds the
# HPA `replicas × threshold` after failureThreshold consecutive scaler
# failures, pinning a deliberate serving capacity instead.
#
# The store ScaledObject is the OTHER side of the same posture: KEDA
# fallback never applies to cpu/memory triggers, so during a prometheus
# outage the store's cpu trigger keeps feeding the HPA real utilization
# — that is the designed degraded mode. A fallback stanza there would
# instead pin the two prometheus triggers at a load-blind replica floor
# the cpu trigger could never correct below (HPA takes the max across
# triggers), at 16-CPU/8-Gi per pod. Absence is asserted, not assumed.

out=$TMPDIR/gateway-fallback.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: both ScaledObjects render at chart defaults.
gw=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway")' "$out")
test -n "$gw" || {
  echo "FAIL: rio-gateway ScaledObject did not render at chart defaults — fallback assertions vacuous" >&2
  exit 1
}
so=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store")' "$out")
test -n "$so" || {
  echo "FAIL: rio-store ScaledObject did not render at chart defaults — fallback-absence assertion vacuous" >&2
  exit 1
}

# Gateway: spec.fallback present, threshold 3 (≈ a minute-plus of
# consecutive scaler failures at the HPA's ~15s external-metrics
# cadence — rides out a prometheus pod restart, catches an outage).
ft=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.fallback.failureThreshold' "$out")
test "$ft" = "3" || {
  echo "FAIL: gateway ScaledObject fallback.failureThreshold = '$ft', expected 3 — a prometheus outage would freeze gateway scaling at the caught count" >&2
  exit 1
}

# Blind-at-ceiling: the fallback replica count equals the autoscaling
# ceiling. While KEDA cannot see demand it must assume the worst case
# the fleet is sized for — gateway pods are cheap (250m/512Mi
# requests), refused build sessions are not. Equality is asserted
# against the RENDERED ceiling so a future maxReplicas change must
# consciously revisit the outage posture too.
fr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.fallback.replicas' "$out")
maxr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.maxReplicaCount' "$out")
test "$fr" = "8" || {
  echo "FAIL: gateway ScaledObject fallback.replicas = '$fr', expected 8 (the autoscaling ceiling — blind-at-ceiling posture)" >&2
  exit 1
}
test "$fr" = "$maxr" || {
  echo "FAIL: gateway fallback.replicas ($fr) != maxReplicaCount ($maxr) — the default posture is blind-at-ceiling; change both knobs together or justify the gap in values.yaml" >&2
  exit 1
}

# The posture is values-driven, not hardcoded.
cfg=$TMPDIR/gateway-fallback-cfg.yaml
helm template rio . --set global.image.tag=test \
  --set gateway.autoscaling.fallback.failureThreshold=5 \
  --set gateway.autoscaling.fallback.replicas=4 >"$cfg"
ft_cfg=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.fallback.failureThreshold' "$cfg")
fr_cfg=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.fallback.replicas' "$cfg")
test "$ft_cfg" = "5" && test "$fr_cfg" = "4" || {
  echo "FAIL: --set gateway.autoscaling.fallback.{failureThreshold=5,replicas=4} rendered $ft_cfg/$fr_cfg — the fallback posture is not values-driven" >&2
  exit 1
}

# Store: NO fallback, deliberately (cpu trigger is the degraded-mode
# corrective; see header).
sf=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.fallback' "$out")
test "$sf" = "null" || {
  echo "FAIL: store ScaledObject renders spec.fallback — fallback never applies to the cpu trigger and would pin a load-blind floor the cpu corrective cannot undercut:" >&2
  echo "$sf" >&2
  exit 1
}

echo "OK: gateway fallback 3/8 (= ceiling), values-driven; store carries none"
