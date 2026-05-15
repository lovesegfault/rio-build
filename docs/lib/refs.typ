// Validated cross-references into generated artifacts and the source
// tree. Exported as a single `refs` dictionary so chapter prose calls
// `#(refs.metric)("rio_scheduler_...")` / `#(refs.gh)("path:line")`.
//
// `/gen/*.json` are produced by the docs build (nix/docs.nix
// `docsData`); the typst compile root fuses them alongside `/lib` and
// `/spec`. metric/alert assert membership so a renamed metric breaks
// the docs build instead of silently rotting.
//
// `gh-sha` is supplied as `--input gh-sha=<rev>` by the nix build; the
// `main` default keeps bare `typst compile` working.

#let _metrics = json("/gen/metrics.json").names
#let _alerts = json("/gen/alerts.json").names
#let _cfg-keys = json("/gen/config.json")
.components
.values()
.map(c => c.map(f => f.key))
.flatten()
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
  cfg: key => {
    assert(key in _cfg-keys, message: "unknown config key: " + key)
    raw(key)
  },
  gh: pl => link(
    "https://github.com/lovesegfault/rio-build/blob/"
      + _gh-sha
      + "/"
      + pl.replace(":", "#L"),
    _src(pl),
  ),
)
