// docs/book.typ — native typst bundle entry point.
// `typst compile --features bundle,html --format bundle` walks the
// chapter manifest and emits one `document()` per page plus static
// assets. Each chapter `.typ` applies `#show: rio.with(...)` itself,
// so this file only routes + wraps in the page shell. PDF stitching
// is `book-pdf.typ`.
#import "/lib/html/meta.typ": chapters, flatten-chapters, route-for
#import "/lib/html/page.typ": page-shell
#import "/lib/rio.typ": bundle-mode

// Bundle mode shares one label/state space across all `document()`
// calls; tell rio() to skip per-chapter `_gloss-anchors` so
// glossary.typ's `print-glossary` is the sole `<key>` emitter.
#bundle-mode()

#for c in flatten-chapters(chapters).filter(c => c.path != none) {
  let route = route-for(c.path)
  document(
    route + ".html",
    title: c.title,
    page-shell(route, c.title, c.path)[#include "/" + c.path],
  )
}

#asset("style.css", read("/assets/style.css", encoding: none))
#asset("theme.js", read("/assets/theme.js", encoding: none))

// 404: minimal shell, no chapter body.
#document(
  "404.html",
  title: "Not Found",
  page-shell("404", "Not Found", "intro.typ")[
    The page you are looking for does not exist.
    #linebreak()
    #html.elem("a", attrs: (href: "/"))[← rio-build design book]
  ],
)
