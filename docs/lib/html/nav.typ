// docs/lib/html/nav.typ
// Sidebar chapter tree. Emits a bare `<ul>` — the `.rio-nav` wrapper
// (and the button/search siblings) live in page.typ so the CSS
// `.rio-nav > ul` selector and the body grid both see the right
// element.
//
// Nodes with children render as `<details>` accordions; the `open`
// attribute is set on the path from root to the current page so the
// active leaf is visible on load without JS. `trail` is the
// `crumb-trail` ancestor chain page.typ already computes — its
// path-membership is exactly the "current is somewhere under this
// node" predicate, so a second recursive walk over the chapter tree
// isn't needed.
#import "meta.typ": chapters, href-for, route-for

#let _li(current, on-trail, title, path, children) = {
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
      _li(current, on-trail, t, p, c)
    })
    let attrs = (class: "rio-nav-group")
    // A node is on the open path iff it (or its section heading) is in
    // the crumb trail. `(title, path)` is the trail's pair shape.
    if (title, path) in on-trail { attrs.insert("open", "open") }
    html.elem("li", html.elem("details", attrs: attrs)[
      #html.elem("summary", label)
      #kids
    ])
  }
}

#let nav-tree(current, trail) = html.elem(
  "ul",
  for (t, p, c) in chapters { _li(current, trail, t, p, c) },
)
