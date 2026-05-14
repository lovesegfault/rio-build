// Common imports for compile-time Monte-Carlo / numerical modules
// under `/lib/mc/`. MC modules that only need RNG + native typst
// (figure/table/calc) can stay self-contained; pull from here when
// they need plotting or units.

#import "/lib/rio.typ": lq, num, qty, qtyrange
// suiji float API: `-f` variants thread the RNG state explicitly
// (return `(rng', sample)`), which is what compile-time MC wants.
#import "@preview/suiji:0.5.1": gen-rng-f, normal-f, random-f, uniform-f
