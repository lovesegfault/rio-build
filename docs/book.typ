// docs/book.typ — native typst bundle entry point.
// `typst compile --features bundle,html --format bundle` walks the
// chapter manifest and emits one `document()` per page plus static
// assets. Each chapter `.typ` applies `#show: rio.with(...)` itself,
// so this file only routes + wraps in the page shell. PDF stitching
// is `book-pdf.typ`.
#import "/lib/html/meta.typ": (
  accent, canonical-url, flat-chapters, route-for, site-url,
)
#import "/lib/html/page.typ": page-shell
#import "/lib/rio.typ": bundle-mode

// Bundle mode shares one label/state space across all `document()`
// calls; tell rio() to skip per-chapter `_gloss-anchors` so
// glossary.typ's `print-glossary` is the sole `<key>` emitter.
#bundle-mode()

#for c in flat-chapters {
  let route = route-for(c.path)
  document(
    route + ".html",
    title: c.title,
    {
      // Bundle mode shares one counter space across `document()` calls;
      // reset so each page's first heading is §1, not §N (QA H2), and
      // each page's first Figure/Table/Algorithm/Listing/equation/
      // footnote is N=1 — not a running total across the whole book
      // (architecture.typ's 5 captioned figures otherwise leave the
      // next chapter at "Figure 6"). math.equation and footnote are
      // unused in HTML today (no `set math.equation(numbering:)`;
      // rio.typ re-implements footnotes via _footnotes state) but
      // reset defensively so a chapter that adds either doesn't
      // inherit a prior chapter's count — docs-html-smoke tripwires
      // check headings/figures only. Heading-slug ids are text-derived
      // (lib/rio.typ _slug), so this doesn't change anchor hrefs.
      counter(heading).update(0)
      counter(figure.where(kind: image)).update(0)
      counter(figure.where(kind: table)).update(0)
      counter(figure.where(kind: "algorithm")).update(0)
      counter(figure.where(kind: "listing")).update(0)
      counter(math.equation).update(0)
      counter(footnote).update(0)
      page-shell(route, c.title, c.path)[#include "/" + c.path]
    },
  )
}

#asset("style.css", read("/assets/style.css", encoding: none))
#asset("theme.js", read("/assets/theme.js", encoding: none))

// Social-preview card source. 1200×630 SVG, Ayu accent fill, brand
// wordmark in NCM Sans. Inline literal — no external asset file.
// nix/docs.nix rasterizes this to og-image.png (resvg, with the NCM
// font dir wired in) because OG scrapers (Slack/LinkedIn/Twitter/
// Facebook) do not render SVG; page.typ points og:image at the PNG.
#asset("og-image.svg", bytes(
  "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1200 630'>\n"
    + "  <rect width='1200' height='630' fill='"
    + accent.light
    + "'/>\n"
    + "  <text x='600' y='360' text-anchor='middle'\n"
    + "    font-family='NewComputerModernSans10, sans-serif'\n"
    + "    font-size='140' font-weight='700' fill='#ffffff'>rio-build</text>\n"
    + "</svg>\n",
))

// Crawler hints. Only emitted when site-url is known (the deployed
// `packages.docs` build); a relative-only sitemap is invalid per the
// sitemaps.org schema.
#if site-url != "" {
  let routes = flat-chapters.map(c => route-for(c.path))
  asset("robots.txt", bytes(
    "User-agent: *\nAllow: /\nSitemap: " + site-url + "/sitemap.xml\n",
  ))
  // <loc> via meta.typ `canonical-url` — same helper page.typ uses for
  // `<link rel=canonical>`, so sitemap and per-page canonical can't
  // disagree on URL shape (root → `/`, not `/index.html`).
  asset("sitemap.xml", bytes(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
      + "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n"
      + routes
        .map(r => "  <url><loc>" + canonical-url(r) + "</loc></url>\n")
        .join("")
      + "</urlset>\n",
  ))
}

// 404: minimal shell, no chapter body. src-path: none — no edit link,
// no breadcrumb, no description.
#document(
  "404.html",
  title: "Not Found",
  page-shell("404", "Not Found", none)[
    The page you are looking for does not exist.
    #linebreak()
    #html.elem("a", attrs: (href: "/"))[← rio-build design book]
  ],
)
