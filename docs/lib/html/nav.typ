// docs/lib/html/nav.typ
// Sidebar chapter tree. Emits a bare `<ul>` — the `.rio-nav` wrapper
// (and the button/search siblings) live in page.typ so the CSS
// `.rio-nav > ul` selector and the body grid both see the right
// element.
//
// Nodes with children render as `<details>` accordions; the `open`
// attribute is set on the path from root to the current page so the
// active leaf is visible on load without JS.
#import "meta.typ": chapters, href-for, route-for

// True if `current` is the route of any descendant of `children`.
#let _contains(current, children) = children.any(
  ((_, p, c)) => (
    (p != none and route-for(p) == current) or _contains(current, c)
  ),
)

#let _li(current, title, path, children) = {
  let is-current = path != none and route-for(path) == current
  let label = if path == none {
    html.elem("span", attrs: (class: "section"))[#title]
  } else {
    let attrs = (href: href-for(path))
    if is-current { attrs.insert("aria-current", "page") }
    html.elem("a", attrs: attrs)[#title]
  }
  if children.len() == 0 {
    html.elem("li", label)
  } else {
    let kids = html.elem("ul", for (t, p, c) in children {
      _li(current, t, p, c)
    })
    let attrs = (class: "rio-nav-group")
    if is-current or _contains(current, children) {
      attrs.insert("open", "open")
    }
    html.elem("li", html.elem("details", attrs: attrs)[
      #html.elem("summary", label)
      #kids
    ])
  }
}

#let nav-tree(current) = html.elem(
  "ul",
  for (t, p, c) in chapters { _li(current, t, p, c) },
)
