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
