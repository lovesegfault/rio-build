// docs/lib/html/page.typ
// Per-page HTML shell: <html>/<head>/<body> chrome around a chapter
// body. typst 0.15's `html.html`/`html.head`/`html.body` take a single
// content block and no `attrs:`, so the document root goes through
// `html.elem("html", attrs: ...)` instead.
#import "meta.typ": (
  chapters, crumb-trail, descriptions, repo-edit-base, route-for,
)
#import "nav.typ": nav-tree
#import "/lib/rio.typ": _current-route, _page-toc

// Ayu accent — keeps the mobile-chrome tint in step with style.css.
#let _theme-color = "#f29718"
// Deploy base for canonical/OG URLs. nix/docs.nix passes this
// unconditionally; only out-of-band `typst compile` hits the empty
// default (which then omits canonical/OG meta).
#let site-url = sys.inputs.at("site-url", default: "")

#let page-shell(route, title, src-path, body) = {
  _current-route.update(route)
  // src-path is none for synthesized pages (404) — no description,
  // breadcrumb, or edit link in that case.
  let desc = if src-path != none { descriptions.at(src-path, default: none) }
  // QA N3: always emit description/og:description; fall back to a
  // title-derived blurb when meta.typ has no per-page summary.
  let meta-desc = if desc != none { desc } else {
    "rio-build design book — " + title
  }
  let trail = if src-path != none { crumb-trail(chapters, src-path) } else {
    ()
  }
  let page-url = if site-url != "" { site-url + "/" + route + ".html" }
  html.elem("html", attrs: (lang: "en"))[
    #html.head[
      #html.elem("meta", attrs: (charset: "utf-8"))
      #html.elem(
        "meta",
        attrs: (
          name: "viewport",
          content: "width=device-width,initial-scale=1",
        ),
      )
      #html.elem("meta", attrs: (name: "description", content: meta-desc))
      #if page-url != none {
        html.elem("link", attrs: (rel: "canonical", href: page-url))
        html.elem("meta", attrs: (property: "og:title", content: title))
        html.elem("meta", attrs: (property: "og:type", content: "article"))
        html.elem("meta", attrs: (property: "og:url", content: page-url))
        html.elem("meta", attrs: (
          property: "og:description",
          content: meta-desc,
        ))
        html.elem("meta", attrs: (
          property: "og:image",
          content: site-url + "/og-image.svg",
        ))
      }
      #html.elem("meta", attrs: (name: "theme-color", content: _theme-color))
      #html.title[#title — rio-build design book]
      // Inline SVG favicon (Ayu accent disc + "r" glyph). data: URI so
      // it ships with every page — no /favicon.ico round-trip.
      #html.elem("link", attrs: (
        rel: "icon",
        href: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><circle cx='16' cy='16' r='14' fill='%23f29718'/><text x='16' y='22' text-anchor='middle' font-size='18' font-weight='bold' fill='%2310141c'>r</text></svg>",
      ))
      // Preload the three above-the-fold faces so font-display:swap's
      // fallback window closes before first paint (FOUT layout-shift).
      #for f in (
        "NewCMSans10-Regular",
        "NewCMSans10-Bold",
        "NewCMMono10-Regular",
      ) {
        html.elem("link", attrs: (
          rel: "preload",
          // `as` is a typst keyword — string-key the pair.
          "as": "font",
          type: "font/woff2",
          crossorigin: "anonymous",
          href: "/assets/fonts/" + f + ".woff2",
        ))
      }
      #html.elem("link", attrs: (rel: "stylesheet", href: "/style.css"))
      #html.elem("link", attrs: (
        rel: "stylesheet",
        href: "/pagefind/pagefind-ui.css",
      ))
      #html.elem("script", attrs: (
        src: "/pagefind/pagefind-ui.js",
        defer: "defer",
      ))[]
      #html.elem("script", attrs: (src: "/theme.js"))[]
    ]
    #html.body[
      #html.elem("a", attrs: (
        class: "rio-skip",
        href: "#main",
      ))[Skip to content]
      #html.elem("dialog", attrs: (
        class: "rio-shortcuts",
        aria-label: "Keyboard shortcuts",
      ))[
        #html.elem("p")[#html.elem("strong")[Keyboard shortcuts]]
        #html.elem("table")[
          #html.elem("tr")[#html.elem("td")[#html.elem("kbd")[s]] #html.elem(
              "td",
            )[Focus search]]
          #html.elem("tr")[#html.elem(
              "td",
            )[#html.elem("kbd")[←] / #html.elem("kbd")[→]] #html.elem(
              "td",
            )[Previous / next chapter]]
          #html.elem("tr")[#html.elem("td")[#html.elem("kbd")[Esc]] #html.elem(
              "td",
            )[Close drawer / clear search]]
          #html.elem("tr")[#html.elem("td")[#html.elem("kbd")[?]] #html.elem(
              "td",
            )[Toggle this help]]
        ]
      ]
      #html.elem("button", attrs: (
        class: "rio-nav-toggle",
        type: "button",
        aria-label: "Toggle navigation",
        aria-controls: "rio-nav",
        aria-expanded: "false",
      ))[☰]
      #html.elem("div", attrs: (class: "rio-page"))[
        #html.elem("nav", attrs: (class: "rio-nav", id: "rio-nav"))[
          #html.elem("a", attrs: (
            class: "rio-brand",
            href: "/",
          ))[rio-build]
          #html.elem("button", attrs: (
            class: "rio-theme-toggle",
            type: "button",
            aria-label: "Toggle color theme",
            title: "Toggle theme",
          ))[◐]
          #html.elem("div", attrs: (id: "search"))[]
          #nav-tree(route)
        ]
        #html.elem(
          "main",
          // QA S1: scope pagefind indexing to the chapter body only —
          // sidebar/TOC/dialog chrome outside <main> are excluded.
          // Synthesized pages (404, src-path: none) are NOT indexed.
          attrs: (class: "rio-main", id: "main")
            + if src-path != none { (data-pagefind-body: "") } else { (:) },
        )[
          #if trail.len() > 1 {
            // Breadcrumbs: ancestor chain root→leaf. Leaf (= this page,
            // already the <h1> below) is rendered unlinked; section
            // headings (path: none) are also unlinked. Ignored by
            // pagefind so ancestor titles don't pollute every nested
            // page's index entry.
            html.elem(
              "nav",
              attrs: (
                class: "rio-crumbs",
                aria-label: "Breadcrumb",
                data-pagefind-ignore: "",
              ),
              html.elem("ol", for (i, (t, p)) in trail.enumerate() {
                let leaf = i + 1 == trail.len()
                html.elem(
                  "li",
                  attrs: if leaf { (aria-current: "page") } else { (:) },
                  if p == none or leaf {
                    [#t]
                  } else {
                    html.elem("a", attrs: (
                      href: "/" + route-for(p) + ".html",
                    ))[#t]
                  },
                )
              }),
            )
          }
          #html.elem("h1")[#title]
          #body
          #if src-path != none {
            html.elem("footer", attrs: (
              class: "rio-edit",
              data-pagefind-ignore: "",
            ))[
              #html.elem("a", attrs: (
                href: repo-edit-base + src-path,
                target: "_blank",
                rel: "noopener",
              ))[Edit this page on GitHub]
            ]
          }
        ]
        // On-this-page right-rail TOC. Headings are collected by
        // lib/rio.typ's html-mode `show heading:` rule into the
        // route-keyed `_page-toc` state; `.final()` is read so the aside
        // can sit lexically after <main> while still seeing every heading
        // in `body`. Only h2/h3 (typst level 1/2) are listed.
        #context {
          let toc = _page-toc
            .final()
            .at(route, default: ())
            .filter(h => (
              h.level <= 2
            ))
          // Always emit the <aside> so the .rio-page grid's 3rd column has
          // an element regardless of heading count; populate only when >1.
          html.elem(
            "aside",
            attrs: (
              class: "rio-toc",
              aria-label: "On this page",
            ),
            if toc.len() > 1 [
              #html.elem("p", attrs: (class: "rio-toc-title"))[On this page]
              #html.elem("ul", for h in toc {
                html.elem(
                  "li",
                  attrs: (class: "rio-toc-l" + str(h.level)),
                  html.elem("a", attrs: (href: "#" + h.id))[#h.text],
                )
              })
            ],
          )
        }
      ]
    ]
  ]
}
