// Single-file PDF aggregate. The shiroa manifest (book.typ) drives the
// HTML site; this file is the `typst compile` entrypoint for the PDF
// check / artifact and stitches chapters via `#include`.
#set document(title: "rio-build design book")
#include "spec/system/_spike.typ"
