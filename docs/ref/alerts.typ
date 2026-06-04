#import "/lib/rio.typ": *

#show: rio.with(domains: none)


Full alert-rule inventory. Every row derives from `gen/alerts.json`,
which `xtask regen docs-data` scrapes from the shipped PrometheusRule
template — name, severity, `for`, the PromQL expression, and the
`rio_*` metrics the expression reads (histogram `_bucket`/`_sum`/
`_count` series resolve to their base metric). Operator runbooks cite
alerts with `refs.alert` and render the live expression with
`refs.alert-expr`; a rule re-key propagates into every citation or
fails `docs-data-fresh` (merged_bug_001: the hung-node runbook
restated the establishment tripwire by hand against a metric the
shipped rule never used).

// One section per rule. The expression is rendered verbatim; each
// referenced metric goes through refs.metric, so an alert expression
// reading a retired metric fails the docs build by construction (the
// docs-side twin of the obs-surface-lint over the chart itself).
#let _rules = json("/gen/alerts.json").rules

#table(
  columns: (auto, auto, auto),
  align: (left, left, left),
  table.header([Alert], [Severity], [For]),
  .._rules
    .map(r => ((refs.alert)(r.name), raw(r.severity), raw(r.at("for"))))
    .flatten(),
)

#for r in _rules [
  // Severity-suffixed heading: alert names may legally carry two
  // arms (e.g. a warning and a critical threshold); the suffix keeps
  // the section anchors unique (merged_bug_015).
  == #raw(r.name) (#raw(r.severity))

  #if r.at("templated", default: false) [
    _Templated expression — helm renders `{{ }}` values at deploy
    time; the text below is the template source, not final PromQL
    (merged_bug_014)._
  ]

  #raw(block: true, lang: "promql", r.expr)

  #if r.metrics.len() > 0 [
    Reads: #r.metrics.map(m => (refs.metric)(m)).join(", ").
  ] else [
    Reads no `rio_*` metrics (kube/external series only).
  ]
]
