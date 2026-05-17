// rio design-book template.
//
// `#show: rio.with(domains: (...), paper: (title: ...))` is the
// per-chapter entrypoint. `#r("domain.area.detail")[...]` emits a
// tracey requirement marker and (when domains are declared) asserts
// the id is in-scope.
//
// One template, four render targets (selected via `--input x-target=`):
//   "pdf"       — standalone paper. Full A4 geometry + title block +
//                 outline + bibliography. The default for bare
//                 `typst compile`.
//   "book-pdf"  — chapter inside the stitched book-pdf.typ aggregate.
//                 A4 geometry, but no per-chapter front-matter.
//   html        — shiroa --mode static-html. typst target = html;
//                 template-rules → mdbook() emits the <head>/CSS/
//                 sidebar chrome; markup-rules wires heading anchors.
//                 ALSO the html-wrapper pass of dyn-paged (see below).
//   web         — shiroa --mode dyn-paged, content pass. typst target
//                 = PAGED (the wasm renderer rasterizes the canvas);
//                 html.* does NOT exist here — render like PDF on one
//                 tall auto-height page at shiroa's `page-width`.
//                 dyn-paged compiles each chapter TWICE: once with
//                 x-target=web-* (this branch → .sir.in artifact) and
//                 once with x-target=html-wrapper (the `html` branch
//                 above → mdbook chrome + wasm trampoline in place of
//                 the body).
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
  cross-link, is-html-target, is-pdf-target, is-web-target, shiroa-sys-target,
  templates, x-current, x-url-base,
)
#import templates: markup-rules, template-rules
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
#import "@preview/unify:0.7.1": num, numrange, qty, qtyrange
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

// ─── shiroa html-frame theming ──────────────────────────────────────
// Figures and equations both render as inline SVG via html.frame().
// Neither dual-renders anymore — figures recolor via CSS
// `filter: invert()` (.rio-figure svg) and equations via
// `currentColor`: they emit at sentinel fill/stroke="#000000" and the
// CSS attribute selectors at `.inline-equation svg [fill="#000000"]`
// override them to `currentColor` so `.inline-equation svg
// { color: var(--fg) }` themes it. nix/docs-svg-dedup.py also rewrites
// the literal attrs (size-only optimisation; the CSS is what makes
// `shiroa serve` correct since it has no post-process). shiroa's
// theme-box (dark+light copies, class-toggled) is no longer used.

// Flatten book-meta.summary into [(path, title), ...] and locate
// x-current. Returns (title: str, prev: (path,title)|none,
// next: (path,title)|none). QA #6 + #9 share this — title for
// <title>/<h1>, prev/next for the nav-wrapper. The chapter title in
// book.typ is the single source of truth; chapters' first `=` is a
// section heading, not the page title. Must be called from `context`.
#let _chapter-nav() = {
  let bm = query(<shiroa-book-meta>).at(0, default: none)
  if bm == none or x-current == none {
    return (title: "", prev: none, next: none)
  }
  // Recursively flatten the summary tree (parts/nested chapters).
  // Chapter nodes have {kind:"chapter", link, title, sub?}; part
  // nodes have {kind:"part", title}.
  let flatten(nodes) = {
    let acc = ()
    for n in nodes {
      if n.kind == "chapter" and n.at("link", default: none) != none {
        // n.title is shiroa's _store-content wrapper:
        // (kind: "plain-text", content: <str>).
        acc.push((path: n.link, title: n.title.content))
      }
      if "sub" in n { acc += flatten(n.sub) }
    }
    acc
  }
  let flat = flatten(bm.value.summary)
  // x-current is the bare relative path ("intro.typ"); summary links
  // match. Normalize leading-slash variant defensively.
  let i = flat.position(c => (
    c.path == x-current or "/" + c.path == x-current
  ))
  if i == none { return (title: "", prev: none, next: none) }
  (
    title: flat.at(i).title,
    prev: if i > 0 { flat.at(i - 1) } else { none },
    next: if i + 1 < flat.len() { flat.at(i + 1) } else { none },
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
  // 100% → 0pt → zero-width SVG (QA #1). is-html-target() is
  // compile-global (NOT context-lazy shiroa-sys-target which would
  // evaluate to "paged" inside html.frame); frame-figure supplies a
  // fixed-width box for kind:"algorithm" instead.
  if is-html-target() {
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

// Right-aligned italic annotation for pseudocode lines.
#let rann(body) = {
  h(1fr)
  text(size: 0.85em, style: "italic", body)
}

// Postfix multiplier "4×" without binary-operator spacing.
#let mul(n) = [#n#h(0pt)×]

// gentle-clues callouts: in html mode the package's icon+title grid()
// warns, so emit a plain <aside> instead (selectable text; styled by
// rio-css extra-assets below). Paged targets get the real gentle-clues render.
#let _clue(gc-fn, kind, ..args) = if is-html-target() {
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
// shiroa `@key` cross-link intercept in `rio()` fall through to the
// intra-chapter glossarium link there.
#let _gloss-own = state("rio-gloss-own", false)
// Registered key set, for the `show link:` membership check.
#let _gloss-keys = glossary-entries.map(e => e.key)
// Bare empty-body figures: rio()'s `show figure.where(kind:
// "glossarium_entry")` rule renders them as `it.body` (=[]) in html
// mode and `align(left, it)` in paged (also visually empty), so no box
// wrapper needed — and a box() at top-level would land OUTSIDE shiroa's
// `<html>` element. Placement is just before `body` in `rio()`, after
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
// gate for the shiroa cross-link intercept (it stays false in chapters
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
  if is-html-target() {
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
  // `target()` which the glossarium show-rules below need to detect
  // `html.frame()`'s paged sub-context).
  let x-target = sys.inputs.at("x-target", default: "pdf")
  // Three-way split (is-html / is-dyn-web mutually exclusive):
  //   is-html      — static-html. typst target=html → html.elem exists.
  //   is-dyn-web   — dyn-paged. typst target=paged → html.* MISSING.
  //   is-pdf       — direct `typst compile` (pdf / book-pdf).
  // is-paged-out = typst's layout engine renders = everything but html.
  // Gate html.elem/html.frame/extra-assets on is-html ONLY.
  let is-html = is-html-target()
  let is-dyn-web = is-web-target()
  let is-paged-out = not is-html
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
  // Heading geometry is paged-only — in html mode markup-rules supplies
  // the theme's heading wrapper and an outer v() would warn.
  show heading.where(level: 1): it => if is-paged-out {
    v(1.2em, weak: true)
    text(size: 16pt, weight: 700, fill: accent, it)
    v(0.5em)
  } else { it }
  show heading.where(level: 2): it => if is-paged-out {
    v(1.0em, weak: true)
    text(size: 12.5pt, weight: 700, it)
    v(0.3em)
  } else { it }
  show heading.where(level: 3): it => if is-paged-out {
    v(0.8em, weak: true)
    text(size: 11pt, weight: 700, it)
    v(0.2em)
  } else { it }

  show math.equation.where(block: true): set block(above: 1.1em, below: 1.1em)
  // box() is paged-layout — in html mode the equation show-rule below
  // wraps the equation in html.frame() and an outer box() would
  // re-trigger the "layout ignored" warning.
  show math.equation.where(block: false): it => if is-paged-out {
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
  show table: it => if is-paged-out {
    block(stroke: (y: 0.4pt + rule-color), align(center, it))
  } else { it }
  show table.cell.where(y: 0): strong

  // glossarium: intercepts @key refs for registered entries, falls
  // through to native @label otherwise. Registration is gated on
  // `_gloss-done` so book-pdf's many `rio()` calls fire exactly once.
  // The `<key>` anchor figures (`_gloss-anchors`) are placed just
  // before `body` below — after all show rules so they land inside
  // shiroa's `<html>` element — and the `_gloss-done.update(true)` is
  // there too so both placements share one gate. In html mode, return
  // `it.body` to consume the figure before glossarium's default theme
  // wraps it in `align(start, ...)` (which warns under the html target).
  context if not _gloss-done.get() {
    register-glossary(glossary-entries)
  }
  show: make-glossary
  // The two glossarium show-rules below emit `html.elem`, which warns
  // ("elem was ignored during paged export") inside `html.frame()`'s
  // paged sub-context — `is-html` (captured from `sys.inputs`) stays
  // true there, but typst's native `target()` flips to `"paged"`. So:
  // outer `if is-html` keeps the rules out of pure-PDF compiles where
  // `target` is undefined; inner `target() == "html"` keeps the
  // `html.elem` arm out of framed-SVG sub-exports.
  show figure.where(kind: "glossarium_entry"): if is-html {
    it => context if target() != "html" {
      // html.frame() paged sub-context — render plainly so the SVG
      // doesn't drop it.
      align(left, it)
    } else if _gloss-own.get() and it.has("label") {
      // The `print-glossary` figures in chapters that own their glossary
      // (glossary.typ, sla-sizing): wrap with `id="label-<key>"` so the
      // cross-chapter `<a href="…#label-<key>">` below has a fragment
      // target. Non-own chapters' `_gloss-anchors` figures (empty body,
      // `_gloss-own` false) fall through to `it.body` — present so
      // glossarium's `link(label(key), …)` resolves, but the link is
      // rewritten to cross-chapter by the `show link:` rule.
      html.elem("span", attrs: (id: "label-" + str(it.label)), it.body)
    } else { it.body }
  } else { it => align(left, it) }
  // shiroa static-html: rewrite glossarium's `link(label(key), …)`
  // (the common tail of `@key`, `#gls(key)`, `#glspl(key)`) to a
  // cross-chapter `<a href="<base>glossary.html#label-<key>">`. Can't
  // use cross-link(reference:) — it queries locally first, finds the
  // hidden anchor, and short-circuits to a same-page link. Chapters
  // that `provides-glossary()` keep the intra-chapter link.
  show link: if is-html {
    it => context {
      if (
        target() == "html"
          and not _gloss-own.get()
          and type(it.dest) == label
          and str(it.dest) in _gloss-keys
      ) {
        html.elem(
          "a",
          attrs: (
            class: "typst-content-link",
            href: x-url-base + "glossary.html#label-" + str(it.dest),
          ),
          it.body,
        )
      } else { it }
    }
  } else { it => it }

  show: gentle-clues.with(breakable: false, headless: false)

  // codly: replaces the plain raw.where(block: true) styling above with
  // line-numbered, language-tagged blocks. Paged-only — its grid()
  // layout warns under the html target; let raw fall through to typst's
  // native <pre><code class="language-X"> there (selectable, CSS-styleable).
  show: if is-paged-out { codly-init.with() } else { it => it }
  if is-paged-out {
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

  // dyn-paged: shiroa CLI provides theme chrome around the wasm-rendered
  // canvas and supplies its own page geometry (serve mode wraps the
  // chapter in a container before #show fires, so `set page` here
  // errors with "not allowed inside of containers"). Content renders
  // like PDF — codly/gentle-clues/fletcher on, wasm rasterizes them.
  // No html.elem emission, no template-rules.

  // shiroa static-html: emit the page chrome + content transforms.
  //
  // template-rules is the outer wrapper — it dispatches to shiroa-
  // mdbook's `mdbook()`, which emits the full <html><head> (CSS,
  // <title>, dyn-svg-support wasm bootstrap) and <body> (sidebar nav,
  // theme picker, getTypstTheme/svg_utils.js wiring) and drops the
  // show-body into the main-content slot. The `book-meta` arg is
  // `include "/book.typ"` so the sidebar reflects the book manifest;
  // book.typ doesn't import rio.typ so there's no cycle.
  //
  // Whole block is gated on is-html ONLY — every branch below emits
  // html.elem/html.frame/add-styles, which don't exist when typst's
  // target is paged. dyn-paged (is-web-target) must NOT enter here.
  //
  // markup-rules + the equation/figure/clue bypasses go inside
  // template-rules so they transform the content that lands in mdbook's
  // main-content slot.
  //
  // markup-rules only destructures `default-theme.dash-color`.
  // figure.where(kind: image) catches every #figure(diagram(...)),
  // chronos.diagram, automaton, autograph, lq.diagram — typst defaults
  // unrecognised figure bodies to kind: image. Algorithm/listing/table/
  // glossarium figures keep their explicit kinds and render as HTML.
  // Custom CSS for our html-mode element bypasses (#r → div.rio-req,
  // gentle-clues → aside.rio-clue, figure → .rio-figure, footnote →
  // .rio-footnote). Passed via `extra-assets` (mdbook reads it; mdbook
  // does NOT read the `shiroa-assets` state that `add-styles()` writes
  // to — that's a starlight-only path).
  let rio-css = ```css
  /* Equations: single-render, themed via currentColor (typst emits
     fill="#000000"; nix/docs-svg-dedup.py rewrites to currentColor). */
  .inline-equation { display: inline-block; width: fit-content; }
  .block-equation { display: grid; place-items: center; overflow-x: auto; }
  .inline-equation svg, .block-equation svg { color: var(--fg, #1b1f24); }
  /* serve mode has no post-process; the inline fill/stroke="#000000"
     stays literal. Attribute-selector override makes equation glyphs
     and strokes (fraction bars, radicals) track `color:` in BOTH serve
     and build. NOT applied to .rio-figure svg — those use the
     `filter: invert()` dark-theme path; recolouring would double-apply.
     The post-process is now size-only, not load-bearing. QA2-R1. */
  .inline-equation svg [fill="#000000"],
  .block-equation svg [fill="#000000"] { fill: currentColor; }
  .inline-equation svg [stroke="#000000"],
  .block-equation svg [stroke="#000000"] { stroke: currentColor; }
  .rio-figure { display: block; text-align: center; overflow-x: auto; margin: 1.2em 0; }
  .rio-figure svg { max-width: none; }   /* QA #4: don't shrink wide diagrams; let the wrapper scroll */
  .rio-figure figcaption { font-size: 0.92em; margin-top: 0.6em; }
  .rio-table { overflow-x: auto; max-width: 100%; }   /* QA #5 */
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
  .nav-wrapper { display: flex; justify-content: space-between; margin-top: 2em;
                 padding-top: 1em; border-top: 1px solid var(--quote-border, #d0d7de); }
  /* QA #7: dark-theme diagram contrast. cetz output carries explicit
     fill/stroke; can't recolor via typst show-rules. CSS filter
     approximates a dark variant. Imperfect (hue-rotate shifts blues),
     but far better than black-on-dark. Option (b) #themed-figure(builder)
     deferred. */
  .ayu .rio-figure svg, .navy .rio-figure svg, .coal .rio-figure svg {
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
  ```
  show: if is-html {
    it => {
      show: template-rules.with(
        book-meta: include "/book.typ",
        // QA #6: chapter title from book.typ's #chapter()[Title]
        // (single source of truth). Wrapped in `context` since
        // _chapter-nav() queries the document; mdbook's meta-title
        // callback compares `title != ""` — content is never == "" so
        // the `[#title -- #site-title]` branch always fires, which is
        // what we want.
        title: if paper != none { paper.title } else {
          context _chapter-nav().title
        },
        plain-body: body,
        web-theme: "mdbook",
        extra-assets: (rio-css,),
      )
      show: markup-rules.with(
        web-theme: "mdbook",
        themes: (default-theme: (dash-color: accent)),
      )
      // Single-render equations: theme via CSS `currentColor`, not
      // dual-SVG. shiroa's equation-rules wraps each eq in theme-box
      // → dark+light copies (byte-identical except fill); on
      // sla-sizing.html that's ~2000 eqs × 2 copies × ~7KB glyph
      // paths. Emit ONE html.frame() at fill=black; nix/docs-svg-
      // dedup.py rewrites #000000 → currentColor and rio-css sets
      // `.inline-equation svg { color: var(--fg) }`. (equation-rules'
      // add-styles() is starlight-only anyway, so the CSS lives in
      // rio-css.)
      show math.equation: set text(weight: 400)
      show math.equation.where(block: false): it => context (
        if shiroa-sys-target() == "html" {
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
        if shiroa-sys-target() == "html" {
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
      let frame-figure = fig => context if shiroa-sys-target() == "html" {
        // 560pt ≈ 746px (mdbook --content-max-width is 750px). Only
        // algorithms get a fixed-width box — they never exceed the
        // column and benefit from fill width. Diagrams (kind: image)
        // stay intrinsic so wide autograph/fletcher (1500pt+) aren't
        // clipped; .rio-figure CSS handles overflow scroll. QA #1/#4.
        let body = if fig.kind == "algorithm" {
          box(width: 560pt, fig.body)
        } else { fig.body }
        // Single-variant render — dark themes recolor via CSS filter
        // (.ayu/.navy/.coal .rio-figure svg below).
        html.elem("figure", attrs: (class: "rio-figure"), {
          html.elem("div", html.frame(body))
          if fig.caption != none {
            html.elem("figcaption", fig.caption)
          }
        })
      } else { fig }
      show figure.where(kind: image): frame-figure
      show figure.where(kind: "algorithm"): frame-figure
      // Tables wider than the content column need horizontal scroll.
      // QA #5. shiroa-sys-target() returns "paged" inside html.frame()
      // (it aliases std.target), so this no-ops there. Wraps ALL
      // tables (acceptable — narrow tables get a benign extra div).
      show table: it => context if shiroa-sys-target() == "html" {
        html.elem("div", attrs: (class: "rio-table"), it)
      } else { it }
      // figure(kind: table) isn't routed through frame-figure (tables
      // stay HTML, not SVG); give it the same .rio-figure margin/
      // caption styling.
      show figure.where(kind: table): fig => context if (
        shiroa-sys-target() == "html"
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
      it
      // QA #9: prev/next chapter nav (mdbook-style). Same
      // _chapter-nav() traversal that feeds <title> above.
      context {
        let nav = _chapter-nav()
        if nav.prev != none or nav.next != none {
          html.elem(
            "nav",
            attrs: (class: "nav-wrapper", aria-label: "Page navigation"),
            {
              if nav.prev != none {
                cross-link("/" + nav.prev.path, html.elem(
                  "span",
                  attrs: (class: "nav-prev"),
                  [← #nav.prev.title],
                ))
              }
              [ ]
              if nav.next != none {
                cross-link("/" + nav.next.path, html.elem(
                  "span",
                  attrs: (class: "nav-next"),
                  [#nav.next.title →],
                ))
              }
            },
          )
        }
      }
    }
  } else { it => it }

  // standalone-paper front matter — only for direct `typst compile`,
  // not when stitched into book-pdf or rendered by shiroa.
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
  // here so they land INSIDE the shiroa `<html>` wrapper (template-rules
  // is already applied). Gated on `_gloss-done` (one set per document)
  // and `_book-mode` (book-pdf's included sla-sizing.typ prints the
  // real glossary, supplying the labels). In static-html they're still
  // emitted so glossarium's `link(label(key))` resolves; the `show
  // link:` intercept above rewrites the link to cross-chapter.
  context if not _gloss-done.get() and not _book-mode.get() {
    _gloss-anchors
  }
  _gloss-done.update(true)
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
    if is-pdf { pagebreak(weak: true) }
    bibliography(paper.bib)
  }
}
