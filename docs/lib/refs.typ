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
    [#v.doc]
  },
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
