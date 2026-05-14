// shiroa book manifest. `shiroa build` reads `<shiroa-book-meta>` from
// this file to discover the chapter list; chapter paths are resolved
// relative to this file (and absolute `/lib/...` imports inside
// chapters resolve against `--root`).
#import "@preview/shiroa:0.3.1": *

#show: book

#book-meta(
  title: "rio-build design book",
  summary: [
    = Spike
    #chapter("spec/system/_spike.typ")[Spike]
  ],
)
