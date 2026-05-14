// rio design-book template — minimal Phase-A scaffold.
//
// `#show: rio.with(domains: (...))` is the per-chapter entrypoint.
// `#r("domain.area.detail")[...]` emits a tracey requirement marker
// and (when domains are declared) asserts the id is in-scope.
//
// Mechanism notes:
// - `set page(...) if cond` — trailing form. `if cond { set ... }`
//   would scope the rule to the empty branch body.
// - domain assert via `state()` + `context` — not lexical capture; the
//   show-rule body that calls `r` is evaluated outside the `rio`
//   function's scope.
// - shiroa's `is-*-target` are nullary functions, not booleans.

#import "@preview/shiroa:0.3.1": is-web-target, is-html-target, is-pdf-target, templates
#import "@preview/tracey:0.1.0": req

#let _domains = state("rio-domains", none)

#let r(id, ..body) = context {
  let ds = _domains.get()
  assert(
    ds == none or ds.any(d => id.starts-with(d + ".")),
    message: "marker " + id + " outside declared domains " + repr(ds),
  )
  req(id, ..body)
}

#let rio(domains: none, paper: none, body) = {
  let target = sys.inputs.at("x-target", default: "pdf")
  _domains.update(domains)
  set page(paper: "a4", margin: (x: 2.6cm, y: 2.8cm)) if target in ("pdf", "book-pdf")
  // Phase B wires shiroa's `templates.*-rules` here for the HTML
  // target. For the spike the chapter body passes through unchanged
  // and shiroa renders it via its own pipeline.
  body
}
