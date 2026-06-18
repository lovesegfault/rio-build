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
//   html        — native typst 0.15 `target() == "html"`. Page chrome
//                 (head/CSS/sidebar/nav) is supplied by lib/html/
//                 page-shell; this file only handles content
//                 transforms (equation/figure framing, callouts).
//
// Mechanism notes:
// - `set page(...) if cond` — trailing form. `if cond { set ... }`
//   would scope the rule to the empty branch body.
// - domain assert via `state()` + `context` — not lexical capture; the
//   show-rule body that calls `r` is evaluated outside the `rio`
//   function's scope.
// - `is-html()`/`is-paged()` wrap typst's native contextual `target()`.

// ─── package imports ────────────────────────────────────────────────
#import "/lib/html/meta.typ": chapters, flatten-chapters, label-for, route-for

#let is-html() = target() == "html"
#let is-paged() = target() == "paged"
// Compile-global (NOT contextual) html predicate for call sites that
// can't be wrapped in `context` — the shiroa-era `is-html-target()` /
// `is-web-target()` names some chapters still use. Gated on the
// `--input x-target=` CLI input rather than typst's `target()`, so it
// evaluates the same inside `html.frame()`'s paged sub-context as
// outside it.
#let is-html-target() = (
  sys.inputs.at("x-target", default: "pdf") not in ("pdf", "book-pdf")
)
#let is-web-target = is-html-target

// Replacement for shiroa's cross-link: resolve a docs-relative .typ
// path to either a PDF-internal label or an HTML href. Call sites in
// chapter prose pass paths with a leading "/" (`"/spec/.../foo.typ"`);
// lib/html/meta.typ's `route-for`/`label-for` expect tree-relative
// paths without it, so strip. The PDF branch degrades to plain text
// when the target chapter isn't in this compilation unit (book-pdf
// scope excludes guide/ops/glossary/contributing).
#let cross-link(path, body) = context {
  let p = if path.starts-with("/") { path.slice(1) } else { path }
  if is-html() {
    html.elem("a", attrs: (href: "/" + route-for(p) + ".html"))[#body]
  } else {
    let lbl = label-for(p)
    if query(lbl).len() > 0 { link(lbl, body) } else { body }
  }
}
// shiroa's plain-text was a content→str flattener; std repr() is the
// closest stdlib analogue but callers in this file only used it for
// title strings, which are already str in lib/html/meta.typ.

// tracey's `req()` rendering helper is no longer used — `#r()` below
// renders via showybox (paged) / html.elem (html) directly. The
// `@preview/tracey` package stays in nix/docs.nix typstDeps for
// consistency with `tracey query` config.styx, but nothing imports
// from it at compile time. tracey's scanner reads .typ SOURCE for
// `#r("…")` calls, so the import was never load-bearing for
// traceability.
#import "@preview/glossarium:0.5.10": (
  get-entry-back-references, gls, glspl, make-glossary, print-glossary,
  register-glossary,
)
#import "@preview/codly:1.3.0": codly, codly-init, codly-range
#import "@preview/codly-languages:0.1.10": codly-languages
#import "@preview/lovelace:0.3.1": pseudocode-list
#import "@preview/unify:0.8.0": num, numrange, qty, qtyrange
#import "@preview/gentle-clues:1.3.1": (
  gentle-clues, idea as _gc-idea, info as _gc-info, memo as _gc-memo,
  tip as _gc-tip, warning as _gc-warning,
)
#import "@preview/showybox:2.0.4": showybox
#import "@preview/lilaq:0.6.0" as lq
#import "@preview/fletcher:0.5.8" as fletcher: diagram, edge, node
#import "@preview/chronos:0.3.0" as chronos
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
#import "/lib/glossary.typ": glossary-entries

// ─── colors ─────────────────────────────────────────────────────────
#let accent = rgb("#1f6feb")
#let muted = rgb("#656d76")
#let rule-color = rgb("#d0d7de")

// ─── html-frame theming ─────────────────────────────────────────────
// Figures and equations both render as inline SVG via html.frame().
// Neither dual-renders anymore — figures recolor via CSS
// `filter: invert()` (.rio-frame svg) and equations via
// `currentColor`: they emit at sentinel fill/stroke="#000000" and the
// CSS attribute selectors at `.inline-equation svg [fill="#000000"]`
// override them to `currentColor` so `.inline-equation svg
// { color: var(--fg) }` themes it. NO post-process rewrite — a
// page-wide replace would also hit .rio-frame diagram SVGs, which
// double-applies (currentColor → light --fg → invert → dark-on-dark);
// the CSS scoping is load-bearing. Works in serve and build alike.

// Per-page route, set by lib/html/page-shell (Task 7). Until that
// lands, stays `none` so `_chapter-nav()` returns the empty stub and
// PDF compiles unaffected.
#let _current-route = state("rio-current-route", none)

// Locate the current route in the flattened chapter manifest. Returns
// (title: str, prev: (title,path,depth)|none, next: …|none). QA #6 +
// #9 share this — title for <title>/<h1>, prev/next for the nav
// wrapper. The chapter title in lib/html/meta.typ is the single source
// of truth. Must be called from `context`.
#let _chapter-nav() = {
  let flat = flatten-chapters(chapters).filter(c => c.path != none)
  let cur = _current-route.get()
  if cur == none { return (title: "", prev: none, next: none) }
  let idx = flat.position(c => route-for(c.path) == cur)
  if idx == none { return (title: "", prev: none, next: none) }
  (
    title: flat.at(idx).title,
    prev: if idx > 0 { flat.at(idx - 1) } else { none },
    next: if idx + 1 < flat.len() { flat.at(idx + 1) } else { none },
  )
}

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
  // The block(width: 100%) wrapper is paged-only — inside
  // html.frame()'s paged sub-context there's no container width, so
  // 100% → 0pt → zero-width SVG (QA #1). frame-figure supplies a
  // fixed-width box for kind:"algorithm" instead.
  context if is-html() {
    pseudocode-list(booktabs: false, indentation: 1.4em, body)
  } else {
    block(
      width: 100%,
      stroke: (y: 0.6pt + rule-color),
      inset: (y: 0.6em),
      breakable: true,
      pseudocode-list(booktabs: false, indentation: 1.4em, body),
    )
  },
)

// Right-aligned italic annotation for pseudocode lines. Right-aligned
// in PDF (h(1fr) needs container width); below-line in HTML
// (html.frame() has no width to push against, and below-line reflows
// at any viewport — width-independent, so the 560pt box stays). QA2-C.
#let rann(body) = context if is-html() {
  linebreak()
  h(2em)
  text(size: 0.85em, style: "italic", fill: muted, body)
} else {
  h(1fr)
  text(size: 0.85em, style: "italic", body)
}

// Postfix multiplier "4×" without binary-operator spacing.
#let mul(n) = [#n#h(0pt)×]

// gentle-clues callouts: in html mode the package's icon+title grid()
// warns, so emit a plain <aside> instead (selectable text; styled by
// rio-css extra-assets below). Paged targets get the real gentle-clues render.
#let _clue(gc-fn, kind, ..args) = context if is-html() {
  let title = args.named().at("title", default: none)
  html.elem("aside", attrs: (class: "rio-clue rio-clue-" + kind), {
    if title != none {
      html.elem("p", attrs: (class: "rio-clue-title"), strong(title))
    }
    args.pos().join()
  })
} else { gc-fn(..args) }
#let info = _clue.with(_gc-info, "info")
#let warning = _clue.with(_gc-warning, "warning")
#let memo = _clue.with(_gc-memo, "memo")
#let tip = _clue.with(_gc-tip, "tip")
#let idea = _clue.with(_gc-idea, "idea")

// Glossarium back-reference printer: " — pp. 3, 7" in muted small text,
// deduplicated. Passed as `user-print-back-references` to print-glossary.
#let muted-backrefs(entry, deduplicate: true) = context {
  // Page backrefs are meaningless in HTML (every chapter is "page 1").
  // QA2-D.
  if not is-html() {
    let refs = get-entry-back-references(entry, deduplicate: true)
    if refs.len() > 0 {
      let lbl = if refs.len() == 1 { "p." } else { "pp." }
      text(fill: muted, size: 0.85em)[~—~#lbl~#refs.join(", ")]
    }
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
// True when compiling the multi-document HTML bundle (book.typ). In
// bundle mode all `document()` calls share ONE label/state space, so
// per-chapter `_gloss-anchors` would collide with glossary.typ's
// `print-glossary` — book.typ sets this and glossary.typ becomes the
// sole `<key>` emitter (cross-document `@key` refs resolve there).
#let _bundle-mode = state("rio-bundle-mode", false)
#let bundle-mode() = _bundle-mode.update(true)

// glossarium per-chapter wiring. `@key`/`#gls("key")` resolve via
// `link(label(key), …)` to a `figure(kind: "glossarium_entry")<key>` —
// `make-glossary`'s `ref` show rule checks `r.element.kind` and only
// intercepts when such a figure exists. `register-glossary` alone is NOT
// enough (populates entry state, emits no labels); `print-glossary`
// creates the labelled figures but a chapter doesn't want a visible
// glossary section. `_gloss-anchors` emits one zero-size labelled figure
// per key so refs resolve without rendering anything.
//
// Both registration (panics on duplicate keys) and anchors (duplicate
// `<key>` labels error) must run exactly ONCE per document. `_gloss-done`
// gates the in-`rio()` placement so book-pdf's many `rio()` calls don't
// re-fire: typst's `state.get()` is positional, so the second `rio()`'s
// `context` read sees the first call's `_gloss-done.update(true)`.
#let _gloss-done = state("rio-gloss-done", false)
// Chapters that `provides-glossary()` (sla-sizing, glossary.typ) own
// their `<key>` anchors via `print-glossary`; `_gloss-own` lets the
// html `@key` cross-link intercept in `rio()` fall through to the
// intra-chapter glossarium link there.
#let _gloss-own = state("rio-gloss-own", false)
// Registered key set, for the `show link:` membership check.
#let _gloss-keys = glossary-entries.map(e => e.key)
// Bare empty-body figures: rio()'s `show figure.where(kind:
// "glossarium_entry")` rule renders them as `it.body` (=[]) in html
// mode and `align(left, it)` in paged (also visually empty), so no box
// wrapper needed. Placement is just before `body` in `rio()`, after
// all show rules, so the figures are inside the html structure.
#let _gloss-anchors = {
  for e in glossary-entries [
    #figure(kind: "glossarium_entry", supplement: "", [])#label(e.key)
  ]
}
// For chapters that print a visible glossary themselves (sla-sizing,
// glossary.typ): call BEFORE `#show: rio.with(…)`. Marks `_gloss-done`
// so `rio()` skips `_gloss-anchors` (which would otherwise duplicate
// the `<key>` labels their `print-glossary` emits), and registers if
// nothing has yet (standalone compile of the chapter; under book-pdf an
// earlier chapter's `rio()` already did). `_gloss-own` is the separate
// gate for the html cross-link intercept (it stays false in chapters
// that rely on rio()'s anchors and true here).
#let provides-glossary() = {
  context if not _gloss-done.get() {
    register-glossary(glossary-entries)
  }
  _gloss-done.update(true)
  _gloss-own.update(true)
}

// Label/anchor id with the tracey `+N` revision suffix stripped.
// Spec authors write `#r("foo+2")` (the `+N` is tracey's bump grammar)
// but reference as `#rref("foo")`; the label must be version-agnostic
// so a `tracey bump` doesn't silently kill inbound links (bug_025).
#let _rid(id) = "r-" + id.replace(regex("\+\d+$"), "")

#let r(id, ..body) = context {
  let ds = _domains.get()
  assert(
    ds == none or ds.any(d => id.starts-with(d + ".")),
    message: "marker " + id + " outside declared domains " + repr(ds),
  )
  if is-html() {
    // Bypass tracey's req() — its block/box/v() layout primitives warn
    // under typst's html target. tracey's scanner reads .typ source
    // (regex for `r[...]`/`#r("...")`), not compiled output, so this
    // doesn't affect `tracey query`.
    //
    // The trailing typst `#label(_rid(id))` is what makes `rref()`'s
    // `query(label())` find the target in static-HTML mode — the html
    // `id:` attr alone is invisible to query(). typst's `link(label,…)`
    // resolves the href to the labelled element's html `id` attribute,
    // so the two stay in sync (verified empirically).
    [#html.elem("div", attrs: (class: "rio-req", id: _rid(id)), {
        html.elem("code", attrs: (class: "rio-req-id"), "r[" + id + "]")
        [ ]
        body.pos().join()
      }) #label(_rid(id))]
  } else {
    // PDF/dyn-paged: showybox mirroring the .rio-req CSS above
    // (3px left border #d0d7de, badge #f6f8fa, body inset 1em left).
    // tracey's bundled req() is pure rendering (block+box, no
    // metadata or state) — its scanner reads .typ SOURCE for
    // `#r("…")` calls — so replacing the render keeps `tracey query`
    // intact. The badge keeps the full `+N` id; only the #label is
    // version-agnostic via _rid so `tracey bump` doesn't rot rrefs.
    [#showybox(
        frame: (
          border-color: rgb("#d0d7de"),
          body-color: white,
          thickness: (left: 3pt),
          radius: 0pt,
          inset: (left: 1em, top: 0.5em, bottom: 0.5em, right: 0em),
        ),
        breakable: true,
        spacing: 1em,
        {
          box(
            fill: rgb("#f6f8fa"),
            radius: 3pt,
            inset: (x: 0.5em, y: 0.15em),
            text(font: "DejaVu Sans Mono", size: 0.85em)[r\[#id\]],
          )
          [ ]
          body.pos().join()
        },
      ) #label(_rid(id))]
  }
}

// Cross-reference to a marker elsewhere (`r[id]` rendered as a link when
// the target label exists in this compilation unit, plain mono otherwise
// so standalone chapter compiles don't fail on out-of-chapter refs).
#let rref(id) = context {
  let lbl = label(_rid(id))
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
  // `x-target` (NOT `target` — that would shadow typst's builtin
  // `target()` which the show-rules below need to detect
  // `html.frame()`'s paged sub-context).
  let x-target = sys.inputs.at("x-target", default: "pdf")
  // is-pdf — direct `typst compile` (pdf / book-pdf). The html/paged
  // split is handled per-site via the contextual `is-html()`/
  // `is-paged()` predicates above; gate html.elem/html.frame on
  // `is-html()` ONLY.
  let is-pdf = x-target in ("pdf", "book-pdf")
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
  // Heading geometry is paged-only — in html mode the page-shell CSS
  // supplies heading styling and an outer v() would warn.
  show heading.where(level: 1): it => context if is-paged() {
    v(1.2em, weak: true)
    text(size: 16pt, weight: 700, fill: accent, it)
    v(0.5em)
  } else { it }
  show heading.where(level: 2): it => context if is-paged() {
    v(1.0em, weak: true)
    text(size: 12.5pt, weight: 700, it)
    v(0.3em)
  } else { it }
  show heading.where(level: 3): it => context if is-paged() {
    v(0.8em, weak: true)
    text(size: 11pt, weight: 700, it)
    v(0.2em)
  } else { it }

  show math.equation.where(block: true): set block(above: 1.1em, below: 1.1em)
  // box() is paged-layout — in html mode the equation show-rule below
  // wraps the equation in html.frame() and an outer box() would
  // re-trigger the "layout ignored" warning.
  show math.equation.where(block: false): it => context if is-paged() {
    box(it)
  } else {
    it
  }

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
  show table: it => context if is-paged() {
    block(stroke: (y: 0.4pt + rule-color), align(center, it))
  } else { it }
  show table.cell.where(y: 0): strong

  // glossarium: intercepts @key refs for registered entries, falls
  // through to native @label otherwise. Registration is gated on
  // `_gloss-done` so book-pdf's many `rio()` calls fire exactly once.
  // The `<key>` anchor figures (`_gloss-anchors`) are placed just
  // before `body` below — after all show rules — and the
  // `_gloss-done.update(true)` is there too so both placements share
  // one gate.
  context if not _gloss-done.get() {
    register-glossary(glossary-entries)
  }
  show: make-glossary
  show figure.where(kind: "glossarium_entry"): it => align(left, it)

  show: gentle-clues.with(breakable: false, headless: false)

  // codly: replaces the plain raw.where(block: true) styling above with
  // line-numbered, language-tagged blocks. Paged-only — its grid()
  // layout warns under the html target; let raw fall through to typst's
  // native <pre><code class="language-X"> there (selectable, CSS-
  // styleable). Gated on the compile-level `is-pdf` (sys.inputs) rather
  // than contextual `is-paged()` so the gate can be evaluated outside
  // `context` (set/show rules can't sit inside `context`).
  show: if is-pdf { codly-init.with() } else { it => it }
  if is-pdf {
    codly(
      languages: codly-languages,
      zebra-fill: rgb("#f6f8fa"),
      number-format: n => text(fill: muted, size: 0.8em)[#n],
      stroke: 0.4pt + rule-color,
      inset: 0.32em,
    )
  }

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
  ) if is-pdf

  // html: emit content transforms. Page chrome (head/CSS/sidebar/nav)
  // is supplied by lib/html/page-shell; this block only handles
  // equation/figure/table framing and footnote/nav fragments.
  //
  // Whole block is gated on is-html() ONLY — every branch below emits
  // html.elem/html.frame, which don't exist when typst's target is
  // paged.
  //
  // figure.where(kind: image) catches every #figure(diagram(...)),
  // chronos.diagram, automaton, autograph, lq.diagram — typst defaults
  // unrecognised figure bodies to kind: image. Algorithm/listing/table/
  // glossarium figures keep their explicit kinds and render as HTML.
  // Custom CSS for our html-mode element bypasses (#r → div.rio-req,
  // gentle-clues → aside.rio-clue, figure → .rio-figure, footnote →
  // .rio-footnote) lives in lib/html/page-shell.
  let rio-css = ```css
  /* Equations: single-render, themed via attribute-selector override.
     typst emits fill/stroke="#000000"; the [fill="#000000"] selectors
     below override to currentColor so `.inline-equation svg
     { color: var(--fg) }` themes them. Works identically in serve and
     build (no post-process needed). NOT applied to .rio-frame svg —
     those use the `filter: invert()` dark-theme path; recolouring
     would double-apply (currentColor → light --fg → invert →
     dark-on-dark). QA2-R1. */
  .inline-equation { display: inline-block; width: fit-content; }
  .block-equation { display: grid; place-items: center; overflow-x: auto; }
  .inline-equation svg, .block-equation svg { color: var(--fg, #1b1f24); }
  .inline-equation svg [fill="#000000"],
  .block-equation svg [fill="#000000"] { fill: currentColor; }
  .inline-equation svg [stroke="#000000"],
  .block-equation svg [stroke="#000000"] { stroke: currentColor; }
  .rio-figure { display: block; text-align: center; overflow-x: auto; margin: 1.2em 0; }
  /* typst's html.frame emits inline `style="width: Nem"` (em at the
     typst font-size, ~10.5pt) alongside `width="Npt"`. At the page's
     16px/em that overshoots ~14%. Clamp to column width; height:auto
     keeps aspect ratio. (The earlier `max-width: none` was for the
     2051px crate-structure graph, since redrawn to 455px.) */
  .rio-frame > svg { max-width: 100%; height: auto; }
  .rio-figure figcaption { font-size: 0.92em; margin-top: 0.6em; }
  .rio-table { overflow-x: auto; max-width: 100%; }   /* QA #5 */
  /* QA4-#5: scroll affordance for wide figures/tables. macOS auto-hides
     scrollbars; without a hint, 2350px-wide diagrams look clipped. */
  .rio-figure, .rio-table { scrollbar-width: thin; }
  .rio-figure::-webkit-scrollbar, .rio-table::-webkit-scrollbar { height: 8px; }
  .rio-figure::-webkit-scrollbar-thumb,
  .rio-table::-webkit-scrollbar-thumb { background: var(--scrollbar, #c0c0c0);
                                        border-radius: 4px; }
  .rio-req { border-left: 3px solid #d0d7de; padding: 0.5em 0 0.5em 1em; margin: 1em 0; }
  .rio-req-id { background: #f6f8fa; border-radius: 3px; padding: 0.1em 0.5em; font-size: 0.85em; }
  .rio-clue { border-left: 4px solid; border-radius: 4px; padding: 0.6em 1em; margin: 1em 0; }
  .rio-clue-title { margin: 0 0 0.4em 0; }
  .rio-clue-info { border-color: #1f6feb; background: #ddf4ff; }
  .rio-clue-warning { border-color: #d1242f; background: #ffebe9; }
  .rio-clue-memo { border-color: #9a6700; background: #fff8c5; }
  .rio-clue-tip { border-color: #1a7f37; background: #dafbe1; }
  .rio-clue-idea { border-color: #8250df; background: #fbefff; }
  .rio-footnote { color: #656d76; font-size: 0.9em; }
  .rio-chapter-title { margin: 0 0 1em 0; font-size: 2em; }
  /* QA #7: dark-theme diagram contrast. cetz output carries explicit
     fill/stroke; can't recolor via typst show-rules. CSS filter
     approximates a dark variant. Imperfect (hue-rotate shifts blues),
     but far better than black-on-dark. Option (b) #themed-figure(builder)
     deferred. */
  .ayu .rio-frame svg, .navy .rio-frame svg, .coal .rio-frame svg {
    filter: invert(0.87) hue-rotate(180deg);
  }
  /* QA #8: dark-theme clue/req-id contrast. */
  .ayu .rio-clue, .navy .rio-clue, .coal .rio-clue { color: var(--fg); }
  .ayu .rio-clue-info, .navy .rio-clue-info, .coal .rio-clue-info { background: #0d2847; }
  .ayu .rio-clue-warning, .navy .rio-clue-warning, .coal .rio-clue-warning { background: #3d0f12; }
  .ayu .rio-clue-memo, .navy .rio-clue-memo, .coal .rio-clue-memo { background: #3a2e05; }
  .ayu .rio-clue-tip, .navy .rio-clue-tip, .coal .rio-clue-tip { background: #0a2e1a; }
  .ayu .rio-clue-idea, .navy .rio-clue-idea, .coal .rio-clue-idea { background: #2d1b47; }
  .ayu .rio-req-id, .navy .rio-req-id, .coal .rio-req-id { background: #161b22; }
  /* Anchor scroll-offset for sticky header. */
  [id^="r-"], [id^="label-"], h2[id], h3[id], h4[id] { scroll-margin-top: 3.5em; }
  /* Code-block background (codly's HTML output is plain <pre><code>). */
  main pre { background: var(--quote-bg, #f6f8fa); padding: 0.8em 1em;
             border-radius: 4px; overflow-x: auto; }
  /* Sidebar 1080 breakpoint: upstream index.js collapses at width<800;
     mdbook proper uses ~1080. CSS-only override — below 1080,
     default-hide unless explicitly .sidebar-visible (which the toggle
     button sets). */
  @media (max-width: 1080px) {
    html:not(.sidebar-visible) .sidebar { transform: translateX(calc(0px - var(--sidebar-width))); }
    html:not(.sidebar-visible) .page-wrapper { margin-left: 0; transform: none; }
  }
  /* mdbook chrome.css has `.previous { float: left }` (no-op on
     position:fixed) instead of `left: var(--page-padding)`. Without an
     explicit left anchor the arrow lands at <main>'s left edge and
     intercepts clicks across a 90px strip. */
  .nav-wide-wrapper .previous { left: var(--page-padding); }
  /* The .previous fixed arrow at left:15px sits in the same x-range
     as the open sidebar; without an explicit z-index the later-in-DOM
     .nav-chapters paints above. */
  .sidebar { z-index: 10; }
  /* Copy-to-clipboard button (rio-js below adds it to each <pre>). */
  .rio-copy { position: absolute; top: .5em; right: .5em; padding: .2em .5em;
              border: 1px solid var(--icons); border-radius: 3px;
              background: var(--quote-bg, #f6f8fa); color: var(--icons);
              cursor: pointer; font-size: .9em; opacity: 0; transition: opacity .15s; }
  pre:hover .rio-copy, .rio-copy:focus { opacity: 1; }
  ```
  // ←/→ nav keys + copy-button for <pre>. inline-assets emits as a
  // data:application/javascript URI in <head>, so wrap in
  // DOMContentLoaded. Self-contained — no upstream gate exists for
  // either (checked themes/mdbook/mod.typ).
  let rio-js = ```js
  document.addEventListener('DOMContentLoaded', () => {
    document.addEventListener('keydown', e => {
      if (e.target.closest('input,textarea') || e.altKey || e.ctrlKey || e.metaKey) return;
      const go = sel => { const a = document.querySelector(sel); if (a) location.href = a.href; };
      if (e.key === 'ArrowLeft') go('a.nav-chapters.previous');
      if (e.key === 'ArrowRight') go('a.nav-chapters.next');
    });
    document.querySelectorAll('main pre').forEach(pre => {
      const b = document.createElement('button');
      b.className = 'rio-copy'; b.textContent = '⧉'; b.title = 'Copy';
      // Read from <code>, not <pre> — the button is appended INSIDE
      // <pre>, and position:absolute/opacity don't exclude an element
      // from innerText (only display:none/visibility:hidden do).
      b.onclick = () => navigator.clipboard
        .writeText((pre.querySelector('code') || pre).innerText)
        .then(() => { b.textContent='✓'; setTimeout(()=>b.textContent='⧉', 1200); });
      pre.style.position = 'relative'; pre.appendChild(b);
    });
  });
  ```
  // Favicon: monospace "r" on accent-colour circle, data-URI so it's
  // self-contained (no extra file to deploy). inline-assets has no
  // raw-html branch so emit via a tiny lang:"js" createElement append
  // (compile-time constant href; no user input). URL-encoded SVG (typst
  // has no built-in base64).
  let _favicon-uri = (
    "data:image/svg+xml,"
      + "%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E"
      + "%3Ccircle cx='16' cy='16' r='16' fill='"
      + accent.to-hex().replace("#", "%23")
      + "'/%3E"
      + "%3Ctext x='16' y='24' text-anchor='middle' font-family='monospace' "
      + "font-weight='bold' font-size='22' fill='%23fff'%3Er%3C/text%3E%3C/svg%3E"
  )
  let rio-favicon = raw(
    lang: "js",
    "(l=>{l.rel='icon';l.href=\""
      + _favicon-uri
      + "\";document.head.appendChild(l)})(document.createElement('link'));",
  )
  show: if not is-pdf {
    it => {
      // Single-render equations: theme via CSS `currentColor`, not
      // dual-SVG. Emit ONE html.frame() at fill=black; the
      // `[fill="#000000"]` attribute selectors override to
      // currentColor so `.inline-equation svg { color: var(--fg) }`
      // themes them — works in serve and build.
      show math.equation: set text(weight: 400)
      show math.equation.where(block: false): it => context (
        if target() == "html" {
          html.elem(
            "span",
            attrs: (class: "inline-equation", role: "math"),
            html.frame({
              set text(fill: black)
              it
            }),
          )
        } else { it }
      )
      show math.equation.where(block: true): it => context (
        if target() == "html" {
          html.elem(
            "p",
            attrs: (class: "block-equation", role: "math"),
            html.frame({
              set text(fill: black)
              it
            }),
          )
        } else { it }
      )
      // html.frame() figures: diagrams (kind: image — typst's default
      // for unrecognised bodies, so catches fletcher/chronos/lilaq/
      // autograph/finite) and lovelace pseudocode (kind: "algorithm",
      // whose grid() would otherwise warn). Selectable text matters
      // less for these than for code/callouts.
      let frame-figure = fig => context if target() == "html" {
        // 560pt ≈ 746px (mdbook --content-max-width is 750px). Only
        // algorithms get a fixed-width box — they never exceed the
        // column and benefit from fill width. Diagrams (kind: image)
        // stay intrinsic so wide autograph/fletcher (1500pt+) aren't
        // clipped; .rio-figure CSS handles overflow scroll. QA #1/#4.
        let body = if fig.kind == "algorithm" {
          box(width: 560pt, fig.body)
        } else { fig.body }
        // Single-variant render — dark themes recolor via CSS filter
        // (.ayu/.navy/.coal .rio-frame svg below). .rio-frame scopes
        // the invert filter to the diagram SVG only — figcaption
        // inline-eq SVGs and figure(kind: table) body cells are
        // .rio-figure children but NOT .rio-frame; they're
        // currentColor-recoloured and must not be inverted. QA4-#1.
        html.elem("figure", attrs: (class: "rio-figure"), {
          html.elem("div", attrs: (class: "rio-frame"), html.frame(body))
          if fig.caption != none {
            html.elem("figcaption", fig.caption)
          }
        })
      } else { fig }
      show figure.where(kind: image): frame-figure
      show figure.where(kind: "algorithm"): frame-figure
      // Tables wider than the content column need horizontal scroll.
      // QA #5. target() returns "paged" inside html.frame(), so this
      // no-ops there. Wraps ALL tables (acceptable — narrow tables get
      // a benign extra div).
      show table: it => context if target() == "html" {
        html.elem("div", attrs: (class: "rio-table"), it)
      } else { it }
      // figure(kind: table) isn't routed through frame-figure (tables
      // stay HTML, not SVG); give it the same .rio-figure margin/
      // caption styling.
      show figure.where(kind: table): fig => context if (
        target() == "html"
      ) {
        html.elem("figure", attrs: (class: "rio-figure"), {
          html.elem("div", attrs: (class: "rio-table"), fig.body)
          if fig.caption != none { html.elem("figcaption", fig.caption) }
        })
      } else { fig }
      // typst html refuses #footnote when a custom <html> element is
      // present (mdbook emits one). Render the note body inline as a
      // muted parenthetical instead — close enough for web reading.
      show footnote: it => html.elem(
        "span",
        attrs: (class: "rio-footnote"),
        [ (#it.body)],
      )
      // Page <h1> from the chapter manifest title. QA2-h1.
      context {
        let t = _chapter-nav().title
        if t != "" {
          html.elem("h1", attrs: (class: "rio-chapter-title"), t)
        }
      }
      // Chapters do NOT carry a leading `= <Title>` heading — the
      // synthetic <h1> above renders the manifest title; chapter body
      // starts at level-1 sections (`= Responsibilities`, …). QA4-B:
      // the QA3 show-rule suppression had three failure modes
      // (false-positive prefix match ate `= Deployment Order`; H1→H3
      // skip; §-starts-at-2); the 14 title-dup chapters are now
      // source-migrated (range-limited promote) and docs-lint catches
      // re-introduction.
      it
      // QA #9/QA2-R3: prev/next chapter nav. mdbook's chrome.css
      // expects TWO blocks with `.previous`/`.next` children:
      //   .nav-wrapper      — mobile bottom buttons (display:none on
      //                        desktop, @media ≤1080px shows it)
      //   .nav-wide-wrapper — desktop floating side arrows
      // Emit <a> directly (cross-link doesn't pass attrs) so the
      // chrome.css class + rel + aria-label land on the link itself.
      context {
        let nav = _chapter-nav()
        if nav.prev != none or nav.next != none {
          let nav-a(ch, cls, rel, body) = if ch != none {
            html.elem(
              "a",
              attrs: (
                class: cls,
                rel: rel,
                href: "/" + route-for(ch.path) + ".html",
                aria-label: rel + ": " + ch.title,
              ),
              body,
            )
          }
          html.elem(
            "nav",
            attrs: (class: "nav-wrapper", aria-label: "Page navigation"),
            {
              nav-a(nav.prev, "mobile-nav-chapters previous", "prev", [←])
              nav-a(nav.next, "mobile-nav-chapters next", "next", [→])
            },
          )
          html.elem(
            "nav",
            attrs: (class: "nav-wide-wrapper", aria-label: "Page navigation"),
            {
              nav-a(nav.prev, "nav-chapters previous", "prev", [←])
              nav-a(nav.next, "nav-chapters next", "next", [→])
            },
          )
        }
      }
    }
  } else { it => it }

  // standalone-paper front matter — only for direct `typst compile`,
  // not when stitched into book-pdf or rendered to html.
  // `context` so `_book-mode.get()` resolves; the CLI `--input x-target`
  // and the in-doc `#book-pdf-mode()` are equivalent gates.
  let in-book = x-target == "book-pdf"
  let front = context if (
    paper != none and is-pdf and not in-book and not _book-mode.get()
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
  // Hidden glossary `<key>` anchors (see `_gloss-anchors` above): placed
  // here so they land after all show rules. Gated on `_gloss-done` (one
  // set per document) and `_book-mode` (book-pdf's included
  // sla-sizing.typ prints the real glossary, supplying the labels).
  context if (
    not _gloss-done.get() and not _book-mode.get() and not _bundle-mode.get()
  ) {
    _gloss-anchors
  }
  _gloss-done.update(true)
  body

  // Per-chapter bibliography for every target except book-pdf (the
  // stitched aggregate supplies one bibliography at the end; typst
  // forbids more than one per document). The html build compiles each
  // chapter standalone, so omitting it there leaves @cite labels
  // unresolved.
  context if (
    paper != none
      and not in-book
      and not _book-mode.get()
      and paper.at("bib", default: none) != none
  ) {
    if is-pdf { pagebreak(weak: true) }
    bibliography(paper.bib)
  }
}
