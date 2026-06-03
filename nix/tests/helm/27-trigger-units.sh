# KEDA trigger unit discipline (bug_299): every prometheus trigger
# renders through rio.promTrigger (templates/_helpers.tpl), which joins
# the metric and its threshold knob via files/metric-units.json and
# FAILS the render on a missing name or a unit mismatch. The original
# defect: a threshold seeded in PATH units (600) divided a
# DERIVATION-count gauge — replicas under-asked ~7× with no error
# anywhere. Red-first evidence: this fragment fails against the
# pre-change chart (knob still named targetBacklogPathsPerReplica,
# raw trigger blocks).

out=$TMPDIR/trigger-units.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard.
so=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store")' "$out")
test -n "$so" || {
  echo "FAIL: rio-store ScaledObject did not render — trigger assertions vacuous" >&2
  exit 1
}

# Positive: the backlog trigger renders with the JOB-unit knob's seed.
backlog=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers[] | select(.metadata.query=="sum(rio_scheduler_substituting_derivations)") | .metadata.threshold' "$out" | tr -d '"')
test "$backlog" = "85" || {
  echo "FAIL: store backlog trigger threshold = '$backlog', expected 85 (targetBacklogJobsPerReplica — job units; see files/metric-units.json)" >&2
  exit 1
}
builders=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers[] | select(.metadata.query=="sum(rio_scheduler_open_attempts)") | .metadata.threshold' "$out" | tr -d '"')
test "$builders" = "50" || {
  echo "FAIL: store builders trigger threshold = '$builders', expected 50" >&2
  exit 1
}
gw=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-gateway") | .spec.triggers[] | select(.metadata.query=="sum(rio_gateway_channels_active)") | .metadata.threshold' "$out" | tr -d '"')
test -n "$gw" && test "$gw" != "null" || {
  echo "FAIL: gateway sessions trigger did not render through rio.promTrigger" >&2
  exit 1
}

# The retired paths-unit knob name must be gone from the chart.
if grep -rn "targetBacklogPathsPerReplica" templates/ values.yaml >/dev/null 2>&1; then
  echo "FAIL: retired knob name targetBacklogPathsPerReplica still present in the chart" >&2
  exit 1
fi

# Negative self-test: a mismatched (metric, knob) pair must FAIL the
# render — proves the gate has teeth, not just that today's pairs
# happen to agree. The fragment sandbox chart copy is writable
# ($TMPDIR/chart, see misc-checks.nix) — drop a throwaway template,
# expect failure, remove it.
cat > templates/zz-unit-selftest.yaml <<'TPL'
{{- include "rio.promTrigger" (list $ "http://selftest" "sum(rio_scheduler_open_attempts)" "rio_scheduler_open_attempts" "targetSessionsPerPod" 1) }}
TPL
if helm template rio . --set global.image.tag=test >/dev/null 2>"$TMPDIR/unit-err"; then
  rm templates/zz-unit-selftest.yaml
  echo "FAIL: mismatched trigger (attempts metric × sessions knob) RENDERED — rio.promTrigger's unit check is not firing" >&2
  exit 1
fi
grep -q "unit mismatch" "$TMPDIR/unit-err" || {
  rm templates/zz-unit-selftest.yaml
  echo "FAIL: render failed but not with the unit-mismatch error:" >&2
  cat "$TMPDIR/unit-err" >&2
  exit 1
}
rm templates/zz-unit-selftest.yaml

echo "OK: triggers unit-checked (backlog=85 jobs, builders=50 attempts, gateway=$gw sessions); mismatch fails render"
