// Validated cross-references into generated artifacts and the source
// tree. Exported as a single `refs` dictionary so chapter prose calls
// `#(refs.metric)("rio_scheduler_...")` / `#(refs.gh)("path:line")`.
//
// `/gen/*.json` are produced by the docs build (nix/docs.nix
// `docsData`); the typst compile root fuses them alongside `/lib` and
// `/spec`. Each validator asserts membership so a renamed/removed
// referent breaks the docs build instead of silently rotting.
//
// `gh-sha` is supplied as `--input gh-sha=<rev>` by the nix build; the
// `main` default keeps bare `typst compile` working.

#let _metrics = json("/gen/metrics.json").names
#let _alerts = json("/gen/alerts.json").names
#let _alert-rules = json("/gen/alerts.json").rules
#let _migrations = json("/gen/migrations.json").stems
#let _errors = json("/gen/errors.json")
#let _ws = json("/gen/workspace.json")
// _ws.members is [{name, description}]; extract names for membership.
#let _ws-names = _ws.members.map(m => m.name)
#let _consts = json("/gen/consts.json")
#let _helm-ns = json("/gen/helm-ns.json")
// Per-component config map. Nested by component because 15 keys
// (health_addr, listen_addr, metrics_addr, ...) repeat across
// components with DIFFERENT defaults — a flat fold would silently
// overwrite. `cfg`/`cfg-default` therefore take (component, key).
#let _cfg-map = {
  let m = (:)
  for (comp, fields) in json("/gen/config.json").components {
    let by-key = (:)
    for f in fields { by-key.insert(f.key, f) }
    m.insert(comp, by-key)
  }
  m
}
#let _crds = json("/gen/crds.json")
#let _cli = json("/gen/cli.json")
// Components whose config carries a `lease_name` (or `*.lease_name`)
// key — i.e., they hold a Kubernetes Lease for leader election.
// merged_015: 3 prose sites disagreed on this; derive.
#let _leased = {
  let leased = ()
  for (comp, fields) in _cfg-map {
    if fields.keys().any(k => k.ends-with("lease_name")) {
      leased.push(comp)
    }
  }
  leased.sorted()
}
#assert(
  _leased.len() >= 2,
  message: "refs.leased-components derived <2 — config-key shape changed?",
)
#let _gh-sha = sys.inputs.at("gh-sha", default: "main")
#let _src(p) = text(
  font: "DejaVu Sans Mono",
  size: 0.85em,
  fill: rgb("#656d76"),
)[#p]

#let refs = (
  metric: name => {
    assert(name in _metrics, message: "unknown metric: " + name)
    raw(name)
  },
  alert: name => {
    assert(name in _alerts, message: "unknown alert: " + name)
    raw(name)
  },
  // The SHIPPED PromQL for an alert, rendered from gen/alerts.json
  // (merged_bug_001: the hung-node runbook restated the establishment
  // alert's expr by hand and cited a metric the rule never used; the
  // expr is now data — a rule re-key propagates here or fails
  // docs-data-fresh).
  alert-expr: name => {
    let matches = _alert-rules.filter(r => r.name == name)
    assert(matches.len() > 0, message: "unknown alert: " + name)
    // merged_bug_015: alert names may carry several severity arms
    // (RioStoreChunkUpgradeTxSlow ships warning AND critical rules);
    // find-first silently rendered an arbitrary arm. Ambiguity is a
    // hard error directing to the severity-keyed accessor.
    assert(
      matches.len() == 1,
      message: "ambiguous alert "
        + name
        + " (severities: "
        + matches.map(r => r.severity).join(", ")
        + ") — use refs.alert-expr-sev((name, severity))",
    )
    raw(block: true, lang: "promql", matches.first().expr)
  },
  // Severity-keyed twin of alert-expr for multi-arm alert names.
  alert-expr-sev: pair => {
    let (name, sev) = pair
    let matches = _alert-rules.filter(r => r.name == name and r.severity == sev)
    assert(
      matches.len() == 1,
      message: "unknown alert arm: " + name + " [" + sev + "]",
    )
    raw(block: true, lang: "promql", matches.first().expr)
  },
  // Migration reference by `NNN_slug` stem, validated against the
  // on-disk chain (merged_bug_122: prose cited bare numbers that the
  // +2 renumber silently invalidated; the slug is self-checking).
  migration: slug => {
    assert(slug in _migrations, message: "unknown migration: " + slug)
    raw(slug)
  },
  cfg: (comp, key) => {
    assert(comp in _cfg-map, message: "unknown component: " + comp)
    assert(
      key in _cfg-map.at(comp),
      message: "unknown " + comp + " config key: " + key,
    )
    raw(key)
  },
  cfg-default: (comp, key) => {
    assert(comp in _cfg-map, message: "unknown component: " + comp)
    assert(
      key in _cfg-map.at(comp),
      message: "unknown " + comp + " config key: " + key,
    )
    raw(_cfg-map.at(comp).at(key).default)
  },
  // Doc-referenced rust consts (curated allowlist, not a full scrape).
  // gen/consts.json maps NAME → integer literal; xtask panics at regen
  // time if the const isn't found at the registered file.
  const: name => {
    assert(name in _consts, message: "unknown const: " + name)
    [#_consts.at(name)]
  },
  // Per-namespace PSA level from infra/helm/rio-build/values.yaml.
  // bug_031: fetcher.typ inlined ADR-019's stale `baseline`; helm had
  // tightened to `restricted`. Deriving makes the prose track helm.
  psa: ns => {
    assert(ns in _helm-ns, message: "unknown namespace: " + ns)
    raw(_helm-ns.at(ns).psa)
  },
  // Per-variant explanation from the rust `///` doc-comment above each
  // `#[error(...)]`. merged_021: three docs restated the Wire variant's
  // semantics; two were wrong. The rust comment is the single source.
  error-doc: (enum-name, variant) => {
    let v = _errors.variants.find(e => (
      e.enum == enum-name and e.name == variant
    ))
    assert(
      v != none,
      message: "unknown error variant: " + enum-name + "::" + variant,
    )
    assert(
      v.doc != "",
      message: "error variant has no /// doc: " + enum-name + "::" + variant,
    )
    [#v.doc]
  },
  crd: kind => {
    assert(kind in _crds.kinds, message: "unknown CRD kind: " + kind)
    raw(kind)
  },
  // rio-cli top-level subcommand. Runbooks cite ~55×; two found stale
  // (R4-024, R6-011). Nested subcommands not validated this round.
  cli-sub: name => {
    assert(
      name in _cli.subcommands,
      message: "unknown rio-cli subcommand: " + name,
    )
    raw(name)
  },
  crd-field: (kind, field) => {
    assert(kind in _crds.fields, message: "unknown CRD kind: " + kind)
    assert(
      field in _crds.fields.at(kind),
      message: "unknown " + kind + " field: " + field,
    )
    raw(field)
  },
  leased-components: () => _leased.map(raw).join(" and "),
  crate: name => {
    assert(name in _ws-names, message: "unknown crate: " + name)
    raw(name)
  },
  crate-count: () => [#_ws-names.len()],
  crate-list: () => _ws-names.map(raw).join(", "),
  gh: pl => link(
    "https://github.com/lovesegfault/rio-build/blob/"
      + _gh-sha
      + "/"
      + pl.replace(":", "#L"),
    _src(pl),
  ),
)
