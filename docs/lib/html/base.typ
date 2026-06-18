// docs/lib/html/base.typ
// Tiny html-attr helpers adapted from typst.app/docs
// (typst/typst docs/components/base.typ). Pure string builders — no
// `target()`/`context` dependency.

// classnames("a", "b", c: cond, d: other) → "a b c" when cond, other falsy.
// Positional args are unconditional; named args include the KEY when the
// value is truthy. `none` positionals are dropped.
#let classnames(..args) = {
  let cs = (
    args.pos().filter(c => c != none)
      + args.named().pairs().filter(p => p.at(1)).map(p => p.at(0))
  )
  // .join() on an empty array yields none; html attrs want a string.
  if cs.len() == 0 { "" } else { cs.join(" ") }
}

// inline-style(color: "red", margin-top: "1em") → "color: red; margin-top: 1em".
// `none` values are dropped so call sites can pass conditional props.
#let inline-style(..props) = {
  let pairs = props.named().pairs().filter(p => p.at(1) != none)
  pairs.map(p => p.at(0) + ": " + p.at(1)).join("; ")
}
