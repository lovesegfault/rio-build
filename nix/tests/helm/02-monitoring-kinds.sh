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
