// rio design-book template.
//
// `#show: rio.with(domains: (...), paper: (title: ...))` is the
// per-chapter entrypoint. `#r("domain.area.detail")[...]` emits a
// tracey requirement marker and (when domains are declared) asserts
// the id is in-scope.
//
// One template, three render targets (selected via `--input x-target=`):
//   "pdf"       — standalone paper. Full A4 geometry + title block +
//                 outline + bibliography. The default for bare
//                 `typst compile`.
//   "book-pdf"  — chapter inside the stitched book-pdf.typ aggregate.
//                 A4 geometry, but no per-chapter front-matter.
//   web/html    — shiroa static site. Page geometry suppressed;
//                 markup-rules wires the starlight heading/link
//                 anchors. shiroa's CLI provides the page chrome.
//
// Mechanism notes:
// - `set page(...) if cond` — trailing form. `if cond { set ... }`
//   would scope the rule to the empty branch body.
// - domain assert via `state()` + `context` — not lexical capture; the
//   show-rule body that calls `r` is evaluated outside the `rio`
//   function's scope.
// - shiroa's `is-*-target` are nullary functions, not booleans.

// ─── package imports ────────────────────────────────────────────────
#import "@preview/shiroa:0.3.1": (
  is-html-target, is-pdf-target, is-web-target, templates,
)
#import templates: markup-rules
#import "@preview/tracey:0.1.0": req
#import "@preview/glossarium:0.5.10": (
  get-entry-back-references, gls, glspl, make-glossary, print-glossary,
  register-glossary,
)
#import "@preview/codly:1.3.0": codly, codly-init, codly-range
#import "@preview/codly-languages:0.1.10": codly-languages
#import "@preview/lovelace:0.3.1": pseudocode-list
#import "@preview/unify:0.7.1": num, numrange, qty, qtyrange
#import "@preview/gentle-clues:1.3.1": (
  gentle-clues, idea, info, memo, tip, warning,
)
#import "@preview/lilaq:0.6.0" as lq
#import "@preview/fletcher:0.5.8" as fletcher: diagram, edge, node
// chronos pinned to 0.2.1: 0.3.0 requires typst ≥0.14.2 but shiroa's
// embedded reflexo-typst is 0.14.0. Bump when shiroaPkg catches up.
#import "@preview/chronos:0.2.1" as chronos
#import "@preview/finite:0.5.1": automaton, layout as finite-layout
// autograph re-exports fletcher's diagram/node/edge names — keep it
// namespaced so the bare `diagram`/`node`/`edge` above stay fletcher's.
#import "@preview/autograph:0.1.0" as autograph
// pinit: page-absolute callout arrows. Pins resolve by page-scoped
// label, so pin + pinit-* call must land on the SAME rendered page —
// keep callouts inside the figure body alongside the pinned diagram.
#import "@preview/pinit:0.2.2": (
  absolute-place, pin, pinit, pinit-place, pinit-point-from, simple-arrow,
)
#import "/lib/refs.typ": refs

// ─── colors ─────────────────────────────────────────────────────────
#let accent = rgb("#1f6feb")
#let muted = rgb("#656d76")
#let rule-color = rgb("#d0d7de")

// ─── math operators ─────────────────────────────────────────────────
#let argmin = math.op("argmin", limits: true)
#let median = math.op("median", limits: true)
#let sign = math.op("sign")
#let MAD = math.op("MAD")
#let EE = math.upright("E")
#let Var = math.op("Var")

// ─── helpers ────────────────────────────────────────────────────────

// Inline reference to a source location. With url, wraps in a permalink;
// without, renders as muted mono (for refs that can't be linkified).
#let src(path, url: none) = {
  let body = text(font: "DejaVu Sans Mono", size: 0.85em, fill: muted)[#path]
  if url != none { link(url, body) } else { body }
}

// Captioned algorithm figure wrapping a lovelace pseudocode-list.
// Usage: #algorithm(caption: [...])[ + step; + *if* cond; + ... ]
#let algorithm(caption: none, body) = figure(
  kind: "algorithm",
  supplement: [Algorithm],
  caption: caption,
  block(
    width: 100%,
    stroke: (y: 0.6pt + rule-color),
    inset: (y: 0.6em),
    breakable: true,
    pseudocode-list(booktabs: false, indentation: 1.4em, body),
  ),
)

// Right-aligned italic annotation for pseudocode lines.
#let rann(body) = {
  h(1fr)
  text(size: 0.85em, style: "italic", body)
}

// Postfix multiplier "4×" without binary-operator spacing.
#let mul(n) = [#n#h(0pt)×]

// Glossarium back-reference printer: " — pp. 3, 7" in muted small text,
// deduplicated. Passed as `user-print-back-references` to print-glossary.
#let muted-backrefs(entry, deduplicate: true) = {
  let refs = get-entry-back-references(entry, deduplicate: true)
  if refs.len() > 0 {
    let lbl = if refs.len() == 1 { "p." } else { "pp." }
    text(fill: muted, size: 0.85em)[~—~#lbl~#refs.join(", ")]
  }
}

// ─── tracey markers ─────────────────────────────────────────────────
#let _domains = state("rio-domains", none)
// True when compiling the stitched book-pdf.typ aggregate. Set by
// `#book-pdf-mode()` at the top of book-pdf.typ so bare
// `typst compile docs/book-pdf.typ` works without `--input x-target=`.
// `--input x-target=book-pdf` (used by nix/docs.nix) still wins via
// the `target` check in rio() — the state is the fallback for direct CLI.
#let _book-mode = state("rio-book-mode", false)
#let book-pdf-mode() = _book-mode.update(true)

#let r(id, ..body) = context {
  let ds = _domains.get()
  assert(
    ds == none or ds.any(d => id.starts-with(d + ".")),
    message: "marker " + id + " outside declared domains " + repr(ds),
  )
  req(id, ..body)
}

// Cross-reference to a marker elsewhere (`r[id]` rendered as a link when
// the target label exists in this compilation unit, plain mono otherwise
// so standalone chapter compiles don't fail on out-of-chapter refs).
#let rref(id) = context {
  let lbl = label("r-" + id)
  let body = text(
    font: "DejaVu Sans Mono",
    size: 0.85em,
    fill: muted,
  )[r\[#id\]]
  if query(lbl).len() > 0 { link(lbl, body) } else { body }
}

// Cross-reference to a label in another chapter. Renders as a link when
// the target exists in this compilation unit (book-pdf), plain text
// otherwise so standalone chapter compiles don't fail.
#let xref(target, body) = context {
  if query(target).len() > 0 { link(target, body) } else { body }
}

// ─── the template ───────────────────────────────────────────────────
#let rio(domains: none, paper: none, body) = {
  let target = sys.inputs.at("x-target", default: "pdf")
  let is-paged = target in ("pdf", "book-pdf")
  _domains.update(domains)

  // common typography (target-neutral)
  set text(font: "Libertinus Serif", size: 10.5pt, lang: "en")
  set par(justify: true, leading: 0.7em, spacing: 1.05em)

  show raw: set text(font: "DejaVu Sans Mono", size: 0.85em)
  show raw.where(block: true): it => block(
    fill: rgb("#f6f8fa"),
    inset: (x: 1em, y: 0.8em),
    radius: 3pt,
    width: 100%,
    it,
  )

  show link: set text(fill: accent)
  show cite: set text(fill: accent)
  show bibliography: set par(justify: false)
  set heading(numbering: "1.1 ")
  show heading.where(level: 1): it => {
    v(1.2em, weak: true)
    text(size: 16pt, weight: 700, fill: accent, it)
    v(0.5em)
  }
  show heading.where(level: 2): it => {
    v(1.0em, weak: true)
    text(size: 12.5pt, weight: 700, it)
    v(0.3em)
  }
  show heading.where(level: 3): it => {
    v(0.8em, weak: true)
    text(size: 11pt, weight: 700, it)
    v(0.2em)
  }

  show math.equation.where(block: true): set block(above: 1.1em, below: 1.1em)
  show math.equation.where(block: false): box

  show figure.where(kind: "algorithm"): set align(left)
  show figure.where(kind: "algorithm"): set block(breakable: true)
  show figure.where(kind: "algorithm"): set figure.caption(position: top)
  show figure.where(kind: "listing"): set align(left)
  show figure.where(kind: "listing"): set block(breakable: true)
  show figure.where(kind: "listing"): set figure.caption(position: top)
  show figure.where(kind: table): set figure.caption(position: top)
  show heading: set block(sticky: true)
  show figure.caption: it => context {
    set text(size: 0.92em)
    [*#it.supplement #it.counter.display(it.numbering):* #it.body]
  }

  set table(stroke: none, inset: (x: 0.8em, y: 0.55em))
  show table: it => block(
    stroke: (y: 0.4pt + rule-color),
    align(center, it),
  )
  show table.cell.where(y: 0): strong

  // glossarium: intercepts @key refs for registered entries, falls
  // through to native @label otherwise.
  show: make-glossary
  show figure.where(kind: "glossarium_entry"): it => align(left, it)

  show: gentle-clues.with(breakable: false, headless: false)

  // codly: replaces the plain raw.where(block: true) styling above with
  // line-numbered, language-tagged blocks.
  show: codly-init.with()
  codly(
    languages: codly-languages,
    zebra-fill: rgb("#f6f8fa"),
    number-format: n => text(fill: muted, size: 0.8em)[#n],
    stroke: 0.4pt + rule-color,
    inset: 0.32em,
  )

  // PDF-only page geometry (trailing conditional — see header note).
  set page(
    paper: "a4",
    margin: (x: 2.6cm, y: 2.8cm),
    numbering: "1 / 1",
    header: context {
      if counter(page).get().first() > 1 and paper != none {
        grid(
          columns: (1fr, auto),
          text(
            size: 9pt,
            fill: muted,
          )[#paper.at("supertitle", default: "")],
          text(size: 9pt, fill: muted)[#paper.title],
        )
        v(-0.6em)
        line(length: 100%, stroke: 0.4pt + rule-color)
      }
    },
  ) if is-paged

  // shiroa web/html: wire starlight heading anchors + link colour.
  // markup-rules only destructures `default-theme.dash-color` from the
  // themes dict; the full theme-box-styles-from() preset machinery is
  // not needed for chapter content (the shiroa CLI supplies page
  // chrome around what this emits).
  show: if is-web-target() or is-html-target() {
    markup-rules.with(themes: (default-theme: (dash-color: accent)))
  } else { it => it }

  // standalone-paper front matter — only for direct `typst compile`,
  // not when stitched into book-pdf or rendered by shiroa.
  // `context` so `_book-mode.get()` resolves; the CLI `--input x-target`
  // and the in-doc `#book-pdf-mode()` are equivalent gates.
  let in-book = target == "book-pdf"
  let front = context if (
    paper != none and not in-book and not _book-mode.get()
  ) [
    #align(center)[
      #text(
        size: 11pt,
        fill: muted,
        tracking: 0.12em,
      )[#upper(paper.at("supertitle", default: "DESIGN"))]
      #v(0.2em)
      #text(size: 22pt, weight: 700)[#paper.title]
      #if paper.at("status", default: none) != none [
        #v(0.6em)
        #grid(
          columns: 2,
          column-gutter: 2em,
          row-gutter: 0.4em,
          text(fill: muted)[Status], [*#paper.status*],
          ..if paper.at("date", default: none) != none {
            (text(fill: muted)[Date], [#paper.date])
          } else { () },
        )
      ]
    ]
    #v(1.2em)
    #line(length: 100%, stroke: 0.6pt + rule-color)
    #v(0.5em)
    #outline(depth: 2, indent: 1.2em)
    #pagebreak()
  ] else []

  front
  body

  // Per-chapter bibliography for every target except book-pdf (the
  // stitched aggregate supplies one bibliography at the end; typst
  // forbids more than one per document). shiroa compiles each chapter
  // standalone, so omitting it there leaves @cite labels unresolved.
  context if (
    paper != none
      and not in-book
      and not _book-mode.get()
      and paper.at("bib", default: none) != none
  ) {
    if is-paged { pagebreak(weak: true) }
    bibliography(paper.bib)
  }
}
