# monitoring-on: ServiceMonitor/PodMonitor/PrometheusRule templates are
# gated and otherwise never rendered by CI.

out=$TMPDIR/monitoring.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true >"$out"

for k in ServiceMonitor PodMonitor PrometheusRule; do
  grep -qx "kind: $k" "$out" || {
    echo "FAIL: monitoring.enabled=true did not render kind: $k" >&2
    exit 1
  }
done

# r33 bug_001: RioNodeclaimPoolStuckPending's threshold MUST be derived
# from the per-cell lead-time gauge (a hardcoded `> 90` false-fires for
# the entire 7-15min metal boot window). Tripwire against a future
# "simplification" that drops the join and silently re-globalizes the
# threshold — the §SCC sweep miss this alert was last bitten by.
grep -A4 'alert: RioNodeclaimPoolStuckPending' "$out" \
  | grep -q 'rio_controller_nodeclaim_lead_time_seconds' || {
  echo "FAIL: RioNodeclaimPoolStuckPending expr does not join on" \
    "rio_controller_nodeclaim_lead_time_seconds — per-cell threshold" \
    "lost (r33 bug_001)" >&2
  exit 1
}

# r34 bug_019 (§Verifier-one-step-removed inverse of r33 bug_001):
# the StuckPending threshold sits above the reaper's 2×seed; a
# successfully-reaped boot-timeout loop is silent to it. The sibling
# alert keys on the reap rate, covering the false-negative arm.
grep -q 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" || {
  echo "FAIL: RioNodeclaimPoolBootTimeoutLoop alert missing —"   \
    "the StuckPending threshold (3×lead, cap from maxLeadTime)"   \
    "sits above the reaper's 2×seed boot-timeout reap; a sustained" \
    "mint→reap loop is silent without this sibling. (r34 bug_019)" >&2
  exit 1
}
grep -A4 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" \
  | grep -q 'reason="boot-timeout"' || {
  echo "FAIL: BootTimeoutLoop expr does not key on reason=boot-timeout" >&2
  exit 1
}

# r34 bug_017 (§Partition-single-source): the StuckPending clamp cap
# must derive from `maxLeadTime`, not a hardcoded literal — the
# invariant `cap >= 2×maxLeadTime` is load-bearing (the alert must
# fire AFTER the reaper, which acts at 2×seed<=2×maxLeadTime). The
# BootTimeoutLoop window also derives from the same constant.
expected_cap=$(( 3 * $(yq '.scheduler.sla.maxLeadTime' values.yaml) ))
# Mirror the {{ max 90 ... }} floor in the template: a maxLeadTime < 30
# would render a window/cap < 90, which is degenerate (Prometheus rejects
# `[0s]` ranges; clamp(v, min, max) returns empty when min > max).
[ "$expected_cap" -lt 90 ] && expected_cap=90
grep -A4 'alert: RioNodeclaimPoolStuckPending' "$out" \
  | grep -q "clamp(.*, 90, ${expected_cap})" || {
  echo "FAIL: RioNodeclaimPoolStuckPending clamp cap != 3×maxLeadTime"  \
    "(expected ${expected_cap}; cap must derive from maxLeadTime so"     \
    "raising it never disarms the reaper-failed signal)" >&2
  exit 1
}
grep -A4 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" \
  | grep -q "\[${expected_cap}s\]" || {
  echo "FAIL: BootTimeoutLoop increase() window != 3×maxLeadTime"       \
    "(expected [${expected_cap}s]; window must span >= 1 reap cycle"     \
    "= 2×seed <= 2×maxLeadTime)" >&2
  exit 1
}
