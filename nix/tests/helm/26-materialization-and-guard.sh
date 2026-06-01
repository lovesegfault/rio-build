# Substitution-replacement Phase B (design §8-B / AS-6 / PD-B4): the
# materialization cutover rendering and its deployment-ordering guard.
#
# 1. Chart default (the Phase B cutover): BOTH deployments render
#    RIO_MATERIALIZATION__ENABLED=true — scheduler-side job creation
#    and the store executor come on together.
# 2. The AND-guard: the persistent hazardous mixed state (creation on,
#    executor off) is unrenderable — scheduler.materialization.enabled
#    =true with store.materialization.enabled=false renders the
#    SCHEDULER env as "false". The transient rollout race is bounded
#    and proven non-stranding by the mixed-flag VM scenario.
# 3. Rollback: both flags false renders both deployments off (the
#    flag-off walk path keeps serving — design §4 revertability; see
#    NOTES.txt for the operator procedure).

env_value() {
  # $1 = rendered yaml, $2 = deployment name → RIO_MATERIALIZATION__ENABLED value
  yq "select(.kind==\"Deployment\" and .metadata.name==\"$2\")
      | .spec.template.spec.containers[0].env[]
      | select(.name==\"RIO_MATERIALIZATION__ENABLED\") | .value" "$1"
}

# ── 1. Default render: the cutover is ON for both components ─────────
d=$TMPDIR/mat-default.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$d"
test "$(env_value "$d" rio-scheduler)" = "true" || {
  echo "FAIL: chart default must render scheduler RIO_MATERIALIZATION__ENABLED=true (Phase B cutover)" >&2
  exit 1
}
test "$(env_value "$d" rio-store)" = "true" || {
  echo "FAIL: chart default must render store RIO_MATERIALIZATION__ENABLED=true (Phase B cutover)" >&2
  exit 1
}

# ── 2. AS-6 AND-guard: mixed state is unrenderable ───────────────────
m=$TMPDIR/mat-mixed.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set scheduler.materialization.enabled=true \
  --set store.materialization.enabled=false \
  >"$m"
test "$(env_value "$m" rio-scheduler)" = "false" || {
  echo "FAIL: AND-guard broken — scheduler renders enabled although the store executor is off (AS-6 hazard)" >&2
  exit 1
}
test "$(env_value "$m" rio-store)" = "false" || {
  echo "FAIL: store.materialization.enabled=false must render the store executor off" >&2
  exit 1
}

# ── 3. Rollback render: both off ─────────────────────────────────────
r=$TMPDIR/mat-rollback.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set scheduler.materialization.enabled=false \
  --set store.materialization.enabled=false \
  >"$r"
test "$(env_value "$r" rio-scheduler)" = "false" || {
  echo "FAIL: rollback must render the scheduler flag off" >&2
  exit 1
}
test "$(env_value "$r" rio-store)" = "false" || {
  echo "FAIL: rollback must render the store flag off" >&2
  exit 1
}
