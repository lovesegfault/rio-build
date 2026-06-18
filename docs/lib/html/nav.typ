// docs/lib/html/nav.typ
// Sidebar chapter tree. Emits a bare `<ul>` — the `.rio-nav` wrapper
// (and the button/search siblings) live in page.typ so the CSS
// `.rio-nav > ul` selector and the body grid both see the right
// element.
#import "meta.typ": chapters, route-for

#let _li(current, title, path, children) = {
  let kids = if children.len() > 0 {
    html.elem("ul", for (t, p, c) in children { _li(current, t, p, c) })
  } else { [] }
  if path == none {
    html.elem("li")[#html.elem("span", attrs: (class: "section"))[#title] #kids]
  } else {
    let href = "/" + route-for(path) + ".html"
    let attrs = (href: href)
    if route-for(path) == current { attrs.insert("aria-current", "page") }
    html.elem("li")[#html.elem("a", attrs: attrs)[#title] #kids]
  }
}

#let nav-tree(current) = html.elem(
  "ul",
  for (t, p, c) in chapters { _li(current, t, p, c) },
)
