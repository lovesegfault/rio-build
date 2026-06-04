# merged_bug_014 (+ §4-R-promtool): the alert inventory is scraped by
# a hand regex (docs_data.rs parse_alerts) whose old header claimed
# "exprs are plain PromQL — verified" without any verifier. This
# fragment IS the verifier, over the RENDERED chart:
#
# 1. promtool check rules — every rendered rule parses as PromQL
#    (templated exprs are rendered by helm first, so this also covers
#    the `templated: true` class end-to-end).
# 2. promtool test rules — alert-contract unit tests. Every rendered
#    `for: 0m` alert must have at least one FIRING case here
#    (non-vacuity: a 0m alert has no soak window, so a wrong polarity
#    pages instantly in prod; the firing case proves the expr can fire
#    at all). Adding a for:0m alert without a contract case fails the
#    coverage assert below by name.
#
# Harness contract: runner cd's into $TMPDIR/chart, bash -euo pipefail.

mon=$TMPDIR/34-mon.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=true >"$mon"

rules=$TMPDIR/34-rules.yaml
yq -N 'select(.kind == "PrometheusRule") | {"groups": .spec.groups}' \
  "$mon" >"$rules"
[ -s "$rules" ] || { echo "FAIL: no PrometheusRule rendered" >&2; exit 1; }

promtool check rules "$rules"

# Contract tests assert FIRING semantics (expr + for + labels), not
# annotation prose — promtool exp_alerts compares annotations exactly,
# so strip them for the test replay (they stay in the check above).
bare=$TMPDIR/34-rules-bare.yaml
yq -N 'del(.groups[].rules[].annotations)' "$rules" >"$bare"

tests=$TMPDIR/34-tests.yaml
cat >"$tests" <<EOF
rule_files:
  - $bare
evaluation_interval: 1m
tests:
  # RioControllerRestarts (for:0m): one restart in the window fires.
  - interval: 1m
    input_series:
      - series: 'kube_pod_container_status_restarts_total{container="controller"}'
        values: '0 0 0 1 1 1 1'
    alert_rule_test:
      - eval_time: 4m
        alertname: RioControllerRestarts
        exp_alerts:
          - exp_labels:
              severity: warning
              container: controller
  # RioLogReadDataLoss (for:0m): any loss increment fires.
  - interval: 1m
    input_series:
      - series: 'rio_store_log_read_data_loss_total'
        values: '0 0 0 2 2 2 2'
    alert_rule_test:
      - eval_time: 4m
        alertname: RioLogReadDataLoss
        exp_alerts:
          - exp_labels:
              severity: critical
  # RioSchedulerAttemptEstablishmentCluster (for:0m): >=2 in 30m.
  - interval: 1m
    input_series:
      - series: 'rio_scheduler_pull_establishments_total'
        values: '0 0 1 2 2 2 2'
    alert_rule_test:
      - eval_time: 5m
        alertname: RioSchedulerAttemptEstablishmentCluster
        exp_alerts:
          - exp_labels:
              severity: warning
  # RioStoreChunkUpgradeTxSlow critical arm (for:0m): an upgrade tx
  # past the 240s bucket fires.
  - interval: 1m
    input_series:
      - series: 'rio_store_chunk_upgrade_tx_seconds_bucket{le="+Inf"}'
        values: '0 0 0 3 3 3 3'
      - series: 'rio_store_chunk_upgrade_tx_seconds_bucket{le="240"}'
        values: '0 0 0 2 2 2 2'
    alert_rule_test:
      - eval_time: 4m
        alertname: RioStoreChunkUpgradeTxSlow
        exp_alerts:
          - exp_labels:
              severity: critical
  # RioStoreGcCollectParseFailure (for:0m): any parse failure fires.
  - interval: 1m
    input_series:
      - series: 'rio_store_gc_collect_parse_failures_total'
        values: '0 0 0 1 1 1 1'
    alert_rule_test:
      - eval_time: 4m
        alertname: RioStoreGcCollectParseFailure
        exp_alerts:
          - exp_labels:
              severity: critical
  # merged_bug_235 contract pair: RioSlaHwCostStale must NOT fire when
  # ANY replica is fresh (the standby's stale-seconds gauge climbs
  # forever by design — observability.typ blesses it; the alert must
  # aggregate). It MUST fire when every replica is stale.
  - interval: 1m
    input_series:
      - series: 'rio_scheduler_sla_hw_cost_stale_seconds{pod="leader"}'
        values: '300x45'
      - series: 'rio_scheduler_sla_hw_cost_stale_seconds{pod="standby"}'
        values: '0+60x45'
    alert_rule_test:
      - eval_time: 40m
        alertname: RioSlaHwCostStale
        exp_alerts: []
  - interval: 1m
    input_series:
      - series: 'rio_scheduler_sla_hw_cost_stale_seconds{pod="a"}'
        values: '0+120x35'
      - series: 'rio_scheduler_sla_hw_cost_stale_seconds{pod="b"}'
        values: '0+120x35'
    alert_rule_test:
      - eval_time: 30m
        alertname: RioSlaHwCostStale
        exp_alerts:
          - exp_labels:
              severity: warning
EOF

promtool test rules "$tests"

# for:0m coverage assert — every rendered 0m/0s alert has a contract
# case above. New 0m alerts fail HERE by name until they get one.
zero_for=$(yq -N \
  '.groups[].rules[] | select((.for // "0m") == "0m" or .for == "0s") | .alert' \
  "$rules" | sort -u)
covered=$(grep -E '^\s+alertname: ' "$tests" | awk '{print $2}' | sort -u)
fail=0
while IFS= read -r a; do
  [ -z "$a" ] && continue
  if ! printf '%s\n' "$covered" | grep -qx "$a"; then
    echo "FAIL: for:0m alert $a has no firing contract case in" >&2
    echo "      nix/tests/helm/34-alert-contracts.sh (non-vacuity," >&2
    echo "      §4-R-promtool) — add an input_series + alert_rule_test." >&2
    fail=1
  fi
done <<<"$zero_for"
[ "$fail" -eq 0 ]
