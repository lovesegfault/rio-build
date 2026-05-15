// Single-file PDF aggregate. The shiroa manifest (book.typ) drives the
// HTML site; this file is the `typst compile` entrypoint for the PDF
// check / artifact and stitches chapters via `#include`.
#set document(title: "rio-build design book")
#include "intro.typ"
#pagebreak(weak: true)
#include "spec/system/_spike.typ"
#pagebreak(weak: true)
#include "spec/components/sla-sizing.typ"
#pagebreak(weak: true)
#bibliography("/lib/bib.yml")
