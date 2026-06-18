// docs/book.typ — native typst bundle entry point.
// `typst compile --features bundle,html --format bundle` walks the
// chapter manifest and emits one `document()` per page plus static
// assets. Each chapter `.typ` applies `#show: rio.with(...)` itself,
// so this file only routes + wraps in the page shell. PDF stitching
// is `book-pdf.typ`.
#import "/lib/html/meta.typ": chapters, flatten-chapters, route-for
#import "/lib/html/page.typ": page-shell, site-url
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
    {
      // Bundle mode shares one counter space across `document()` calls;
      // reset so each page's first heading is §1, not §N (QA H2).
      // Heading-slug ids are text-derived (lib/rio.typ _slug), so this
      // doesn't change anchor hrefs.
      counter(heading).update(0)
      page-shell(route, c.title, c.path)[#include "/" + c.path]
    },
  )
}

#asset("style.css", read("/assets/style.css", encoding: none))
#asset("theme.js", read("/assets/theme.js", encoding: none))

// Crawler hints. Only emitted when site-url is known (the deployed
// `packages.docs` build); a relative-only sitemap is invalid per the
// sitemaps.org schema.
#if site-url != "" {
  let routes = flatten-chapters(chapters)
    .filter(c => (
      c.path != none
    ))
    .map(c => route-for(c.path))
  asset("robots.txt", bytes(
    "User-agent: *\nAllow: /\nSitemap: " + site-url + "/sitemap.xml\n",
  ))
  asset("sitemap.xml", bytes(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
      + "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n"
      + routes
        .map(r => "  <url><loc>" + site-url + "/" + r + ".html</loc></url>\n")
        .join("")
      + "</urlset>\n",
  ))
}

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
