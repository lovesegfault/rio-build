// docs/lib/html/page.typ
// Per-page HTML shell: <html>/<head>/<body> chrome around a chapter
// body. typst 0.15's `html.html`/`html.head`/`html.body` take a single
// content block and no `attrs:`, so the document root goes through
// `html.elem("html", attrs: ...)` instead.
#import "meta.typ": (
  accent, canonical-url, chapters, crumb-trail, descriptions, href-for,
  repo-edit-base, site-url,
)
#import "nav.typ": nav-tree
#import "/lib/rio.typ": _current-route, _page-toc

// Ayu accent — keeps the mobile-chrome tint in step with style.css.
// Light/dark pair from meta.typ `accent`; the dark entry carries
// `media` so a dark-OS user gets the dark tint regardless of the
// in-page data-theme toggle (browsers only read the meta, not the
// stylesheet).
#let _theme-color = (
  (media: none, content: accent.light),
  (media: "(prefers-color-scheme: dark)", content: accent.dark),
)

// `<meta {kind}="k" content="v">` for each pair. The og:* block was
// ~40 lines of triple-nested `html.elem` copy-paste; one helper, one
// tuple per tag.
#let _meta-tags(kind, pairs) = for (k, v) in pairs {
  html.elem("meta", attrs: ((kind): k, content: v))
}

#let page-shell(route, title, src-path, body) = {
  _current-route.update(route)
  // Synthesized pages (404) have no source file — book.typ passes
  // `src-path: none`. No description lookup, no breadcrumb trail, no
  // canonical/OG meta, no edit link, not pagefind-indexed. Named once
  // so the half-dozen gates below read as "is-error-page", not
  // "src-path is none".
  let is-error-page = src-path == none
  let desc = if not is-error-page { descriptions.at(src-path, default: none) }
  // QA N3: always emit description/og:description; fall back to a
  // title-derived blurb when meta.typ has no per-page summary.
  let meta-desc = if desc != none { desc } else {
    "rio-build design book — " + title
  }
  let trail = if is-error-page { () } else { crumb-trail(chapters, src-path) }
  // The 404 shell → no canonical/OG: a not-found page has no canonical
  // URL and isn't an `og:type=article`. Mirrors the data-pagefind-body
  // gate further down. URL shape is single-sourced in meta.typ
  // `canonical-url` (book.typ's sitemap reads the same helper).
  let page-url = if site-url != "" and not is-error-page {
    canonical-url(route)
  }
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
      #if is-error-page {
        // 404: tell crawlers not to index. GH Pages serves 404.html
        // with status 200 for soft-404 detection, so the meta tag is
        // the only signal.
        html.elem("meta", attrs: (name: "robots", content: "noindex"))
      }
      #if page-url != none {
        html.elem("link", attrs: (rel: "canonical", href: page-url))
        _meta-tags("property", (
          ("og:title", title),
          ("og:type", "article"),
          ("og:url", page-url),
          ("og:description", meta-desc),
          // PNG, not SVG: OG scrapers (Slack/LinkedIn/Twitter/
          // Facebook) require raster. nix/docs.nix rasterizes
          // book.typ's og-image.svg via resvg.
          ("og:image", site-url + "/og-image.png"),
          ("og:image:type", "image/png"),
          ("og:image:width", "1200"),
          ("og:image:height", "630"),
        ))
      }
      #for tc in _theme-color {
        let attrs = (name: "theme-color", content: tc.content)
        if tc.media != none { attrs.insert("media", tc.media) }
        html.elem("meta", attrs: attrs)
      }
      #html.title[#title — rio-build design book]
      // Inline SVG favicon (Ayu accent disc + "r" glyph). data: URI so
      // it ships with every page — no /favicon.ico round-trip.
      // `accent.light` minus its leading `#` (URL-encoded as `%23`).
      #html.elem("link", attrs: (
        rel: "icon",
        href: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><circle cx='16' cy='16' r='14' fill='%23"
          + accent.light.slice(1)
          + "'/><text x='16' y='22' text-anchor='middle' font-size='18' font-weight='bold' fill='%2310141c'>r</text></svg>",
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
          // Synthesized pages (404) are NOT indexed.
          attrs: (class: "rio-main", id: "main")
            + if is-error-page { (:) } else { (data-pagefind-body: "") },
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
                    html.elem("a", attrs: (href: href-for(p)))[#t]
                  },
                )
              }),
            )
          }
          #html.elem("h1")[#title]
          #body
          #if not is-error-page {
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
