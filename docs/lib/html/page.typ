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

#let page-shell(route, title, src-path, body) = {
  _current-route.update(route)
  let desc = descriptions.at(src-path, default: none)
  let trail = crumb-trail(chapters, src-path)
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
      #if desc != none {
        html.elem("meta", attrs: (name: "description", content: desc))
      }
      #html.elem("meta", attrs: (name: "theme-color", content: _theme-color))
      #html.title[#title — rio-build design book]
      // Empty data-URI favicon — suppresses the /favicon.ico 404 every
      // page load otherwise triggers under `nix run .#docs`.
      #html.elem("link", attrs: (rel: "icon", href: "data:,"))
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
      #html.elem("button", attrs: (
        class: "rio-nav-toggle",
        type: "button",
        aria-label: "Toggle navigation",
        aria-controls: "rio-nav",
        aria-expanded: "false",
      ))[☰]
      #html.elem("nav", attrs: (class: "rio-nav", id: "rio-nav"))[
        #html.elem("button", attrs: (
          class: "rio-theme-toggle",
          type: "button",
        ))[◐]
        #html.elem("div", attrs: (id: "search"))[]
        #nav-tree(route)
      ]
      #html.elem("main", attrs: (class: "rio-main"))[
        #if trail.len() > 1 {
          // Breadcrumbs: ancestor chain root→leaf. Leaf (= this page,
          // already the <h1> below) is rendered unlinked; section
          // headings (path: none) are also unlinked.
          html.elem(
            "nav",
            attrs: (class: "rio-crumbs", aria-label: "Breadcrumb"),
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
        #html.elem("footer", attrs: (class: "rio-edit"))[
          #html.elem("a", attrs: (
            href: repo-edit-base + src-path,
          ))[Edit this page on GitHub]
        ]
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
        // Always emit the <aside> so the body grid's 3rd column has an
        // element regardless of heading count; populate only when >1.
        html.elem("aside", attrs: (class: "rio-toc"), if toc.len() > 1 [
          #html.elem("p", attrs: (class: "rio-toc-title"))[On this page]
          #html.elem("ul", for h in toc {
            html.elem(
              "li",
              attrs: (class: "rio-toc-l" + str(h.level)),
              html.elem("a", attrs: (href: "#" + h.id))[#h.text],
            )
          })
        ])
      }
    ]
  ]
}
