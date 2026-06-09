# Dashboard PromQL → metric label-set contract.
#
# Each dashboards/*.json panel `expr` references labels via `sum by (k,…)`
# clauses or `{k="v"}` selectors. This fragment extracts every (metric,
# label-key) pair and asserts it appears in the allowlist below, which is
# sourced from the `describe_*!` HELP text in each component's lib.rs and
# the tables in docs/spec/system/observability.typ.
#
# A label-key drift (e.g. `reason` vs `result`) is invisible to `helm
# template`: PromQL with an absent label collapses series to one
# empty-legend line (`sum by (absent)`) or selects zero series
# (`{absent="x"}` → "No data"). bug_141/460/483 shipped exactly this.
#
# Adding a new (metric,label) pair: extend ALLOW below AND make sure the
# emission site + observability.typ agree.

# metric:label, one per line. `le` is the implicit Prometheus histogram
# bucket label and is allowed on every *_bucket metric below.
ALLOW='
rio_controller_disruption_drains_total:result
rio_controller_scaling_decisions_total:direction
rio_controller_reconcile_duration_seconds_bucket:reconciler
rio_controller_reconcile_errors_total:reconciler
rio_controller_reconcile_errors_total:error_kind
rio_scheduler_builds_total:outcome
rio_scheduler_actor_cmd_seconds_bucket:cmd
rio_scheduler_materialization_jobs_resolved_total:outcome
rio_store_materialization_executions_total:outcome
rio_store_substitute_bytes_total:pod
rio_store_get_path_bytes_total:pod
rio_store_put_path_bytes_total:pod
kube_horizontalpodautoscaler_status_desired_replicas:namespace
kube_horizontalpodautoscaler_status_desired_replicas:horizontalpodautoscaler
kube_deployment_status_replicas_ready:namespace
kube_deployment_status_replicas_ready:deployment
container_cpu_usage_seconds_total:namespace
container_cpu_usage_seconds_total:pod
container_cpu_usage_seconds_total:container
container_network_receive_bytes_total:namespace
container_network_receive_bytes_total:pod
container_network_transmit_bytes_total:namespace
container_network_transmit_bytes_total:pod
keda_scaler_active:scaledObject
keda_scaler_detail_errors_total:scaledObject
keda_scaler_empty_upstream_responses_total:scaledResource
keda_scaler_http_requests_total:scaled_resource
'
# Provenance of the non-rio entries (Store-scaling row, decision 5):
# `pod` on rio_store_*_bytes_total is the prometheus-operator target
# label (ServiceMonitor discovery), not an emission-site label —
# per-replica balance is the thing the panels exist to show.
# kube_* come from kube-state-metrics and container_* from cadvisor,
# both shipped by the kube-prometheus-stack reference deploy
# (infra/eks/monitoring.tf); `keda-hpa-rio-store` is KEDA's managed-HPA
# naming convention for the rio-store ScaledObject.
# keda_* are the KEDA operator's self-metrics (scraped via the keda
# chart's operator ServiceMonitor, infra/eks/keda.tf). Label keys per
# keda v2.20.1 pkg/metricscollector/prommetrics.go: the shared
# metricLabels set carries camelCase `scaledObject`; the
# empty-response counter (#7062) carries `scaledResource`; the 2.20
# scaler HTTP metrics (#6600) carry snake_case `scaled_resource` —
# three spellings of the same SO-name dimension, faithful to what the
# operator emits. Values are the ScaledObject names (rio-store /
# rio-gateway), set from config.ScalableObjectName.
# (The rio_controller_component_scaler_* entries left with the
# retired ComponentScaler dashboard.)

scratch=$TMPDIR/dashlabels
rm -rf "$scratch"
mkdir -p "$scratch"

# Emit (dash<TAB>title<TAB>metric<TAB>key) for every label reference.
# Two extraction passes, both tolerant of no-match (`|| true`):
#   1. selector form: metric{k="v",k2=~"v2",…}
#   2. by-clause form: by (k,k2) (... rio_metric ...)
for dash in dashboards/*.json; do
  jq -r '.panels[]? | .title as $t | .targets[]? | [$t, .expr] | @tsv' \
    "$dash" >"$scratch/exprs"
  while IFS=$'\t' read -r title expr; do
    [ -n "$expr" ] || continue

    # ---- selector form ----------------------------------------------
    while IFS= read -r m; do
      [ -n "$m" ] || continue
      metric=${m%%\{*}
      labels=${m#*\{}; labels=${labels%\}}
      IFS=',' read -ra parts <<<"$labels"
      for p in "${parts[@]}"; do
        key=$(sed -E 's/^[[:space:]]*([a-zA-Z_][a-zA-Z0-9_]*).*/\1/' <<<"$p")
        printf '%s\t%s\t%s\t%s\n' "$dash" "$title" "$metric" "$key"
      done
    done < <(grep -Eo '[a-zA-Z_:][a-zA-Z0-9_:]*\{[^}]+\}' <<<"$expr" || true)

    # ---- by-clause form ---------------------------------------------
    rest=$expr
    while [[ $rest =~ by[[:space:]]*\(([^\)]+)\)[[:space:]]*\( ]]; do
      keys=${BASH_REMATCH[1]}
      rest=${rest#*"${BASH_REMATCH[0]}"}
      while IFS= read -r metric; do
        [ -n "$metric" ] || continue
        IFS=',' read -ra ks <<<"$keys"
        for k in "${ks[@]}"; do
          k=${k//[[:space:]]/}
          printf '%s\t%s\t%s\t%s\n' "$dash" "$title" "$metric" "$k"
        done
      done < <(grep -Eo 'rio_[a-zA-Z0-9_:]+' <<<"$rest" || true)
    done
  done <"$scratch/exprs"
done | sort -u >"$scratch/pairs"

# Assert every pair is allowed.
fail=0
while IFS=$'\t' read -r dash title metric key; do
  [ -n "$metric" ] || continue
  if [ "$key" = le ] && [[ $metric == *_bucket ]]; then
    continue
  fi
  if ! grep -qx "${metric}:${key}" <<<"$ALLOW"; then
    allowed=$(grep -E "^${metric}:" <<<"$ALLOW" \
      | sed "s/^${metric}://" | paste -sd, - || true)
    echo "FAIL: $dash panel '$title' uses label '$key' on $metric — emitted labels are: ${allowed:-<none>}" >&2
    fail=1
  fi
done <"$scratch/pairs"

exit $fail
