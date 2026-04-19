# Dashboard PromQL → metric label-set contract.
#
# Each dashboards/*.json panel `expr` references labels via `sum by (k,…)`
# clauses or `{k="v"}` selectors. This fragment extracts every (metric,
# label-key) pair and asserts it appears in the allowlist below, which is
# sourced from the `describe_*!` HELP text in each component's lib.rs and
# the tables in docs/src/observability.md.
#
# A label-key drift (e.g. `reason` vs `result`) is invisible to `helm
# template`: PromQL with an absent label collapses series to one
# empty-legend line (`sum by (absent)`) or selects zero series
# (`{absent="x"}` → "No data"). bug_141/460/483 shipped exactly this.
#
# Adding a new (metric,label) pair: extend ALLOW below AND make sure the
# emission site + observability.md agree.

# metric:label, one per line. `le` is the implicit Prometheus histogram
# bucket label and is allowed on every *_bucket metric below.
ALLOW='
rio_controller_disruption_drains_total:result
rio_controller_scaling_decisions_total:direction
rio_controller_reconcile_duration_seconds_bucket:reconciler
rio_controller_reconcile_errors_total:reconciler
rio_controller_reconcile_errors_total:error_kind
rio_controller_component_scaler_desired_replicas:cs
rio_controller_component_scaler_learned_ratio:cs
rio_controller_component_scaler_observed_load:cs
rio_scheduler_builds_total:outcome
rio_scheduler_actor_cmd_seconds_bucket:cmd
'

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
