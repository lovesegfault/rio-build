#import "/lib/rio.typ": *

#show: rio.with(domains: none)


Full per-component metric inventory. Naming convention, label rules,
leader-gating, and histogram bucket policy are specified normatively in
#xref(label("r-obs.metric.gateway"), [Observability §Metrics]).

// Tables derive from gen/metrics.json (each row = one describe_*!
// macro's name+kind+help). bug_002: the previous hand-written tables
// duplicated the help text, drifted, and missed two new gauges.
// json→table coverage is now structural by construction.
#let _by-comp = json("/gen/metrics.json").by_component
#let _kind-label = (counter: "Counter", gauge: "Gauge", histogram: "Histogram")
#let _metric-table(comp) = table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Metric], [Type], [Description]),
  .._by-comp
    .at(comp)
    .map(m => ((refs.metric)(m.name), [#_kind-label.at(m.kind)], [#m.help]))
    .flatten(),
)

= Gateway <tbl-metrics-gateway>
#_metric-table("gateway")

= Scheduler <tbl-metrics-scheduler>
#_metric-table("scheduler")

= Store <tbl-metrics-store>
#_metric-table("store")

= Builder <tbl-metrics-builder>

#info[
  Per ADR-019 §Observability, the former `rio_worker_*` metrics are now
  `rio_builder_*`. (The scheduler-side per-kind queue-depth and
  utilization `{kind}` gauges that used to track the
  builder/fetcher split retired with the placement layer; the
  per-system backlog split is `queued_by_system` on
  `ClusterStatus`/`GetSpawnIntents`.)
]

#_metric-table("builder")

= Controller <tbl-metrics-controller>
#_metric-table("controller")

= Retired and renamed

Historical notes the source `describe_*!` help cannot carry (the metric
in question no longer exists to annotate). New code should reference the
replacement directly.

- `_sla_resize_retry_total` was never emitted; the under-provisioning
  signal is #(refs.metric)("rio_scheduler_resource_floor_bumps_total")
  (`_prediction_ratio` is blind to censored samples).
- `_sla_als_cap_hit_total` →
  #(refs.metric)("rio_scheduler_sla_als_round_cap_hit_total")
  (pre-production rename, no alias).
- `_ice_backoff_total` →
  #(refs.metric)("rio_scheduler_sla_hw_ladder_exhausted_total").
- `rio_worker_*` → `rio_builder_*` (ADR-019; see §Builder above).
