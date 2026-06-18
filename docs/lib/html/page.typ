// docs/lib/html/page.typ
// Per-page HTML shell: <html>/<head>/<body> chrome around a chapter
// body. typst 0.15's `html.html`/`html.head`/`html.body` take a single
// content block and no `attrs:`, so the document root goes through
// `html.elem("html", attrs: ...)` instead.
#import "meta.typ": repo-edit-base
#import "nav.typ": nav-tree
#import "/lib/rio.typ": _current-route

#let page-shell(route, title, src-path, body) = {
  _current-route.update(route)
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
      #html.title[#title — rio-build design book]
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
      #html.elem("nav", attrs: (class: "rio-nav"))[
        #html.elem("button", attrs: (
          class: "rio-theme-toggle",
          type: "button",
        ))[◐]
        #html.elem("div", attrs: (id: "search"))[]
        #nav-tree(route)
      ]
      #html.elem("main", attrs: (class: "rio-main"))[
        #html.elem("h1")[#title]
        #body
        #html.elem("footer", attrs: (class: "rio-edit"))[
          #html.elem("a", attrs: (
            href: repo-edit-base + src-path,
          ))[Edit this page on GitHub]
        ]
      ]
    ]
  ]
}
