// Single-file PDF aggregate. The HTML site is driven from the same
// chapter tree (lib/html/meta.typ); this file is the `typst compile`
// entrypoint for the PDF check / artifact and stitches chapters via
// `#include`.
//
// `book-pdf-mode()` sets state so each chapter's `rio()` suppresses its
// per-chapter title-block and bibliography — the aggregate emits one
// `#bibliography()` at the end. This makes bare `typst compile --root
// docs docs/book-pdf.typ` work without `--input x-target=book-pdf`
// (which the nix derivation still passes; both gates are equivalent).
//
// Scope: PDF = intro + architecture + spec/ + ref/. The HTML site
// (lib/html/meta.typ's full tree) additionally has guide/, ops/,
// glossary, contributing — operational/contributor content that
// doesn't belong in the design reference PDF. The `pdf-scope` filter
// below derives the PDF chapter set from the tree by path prefix, so a
// new SPEC or REF chapter is picked up automatically; a new guide/ops
// chapter stays HTML-only. cross-link()s from PDF chapters to HTML-
// only ones degrade to plain text in the PDF (cross-link's query()
// guard).
#import "/lib/html/meta.typ": chapters, flatten-chapters, label-for
#import "/lib/rio.typ": book-pdf-mode
#book-pdf-mode()
#set document(title: "rio-build design book")
// Chapter stitch derived from the canonical tree. Each leaf in PDF
// scope gets a weak pagebreak seam BY CONSTRUCTION (merged_bug_227 —
// the hand-stitched form shipped a missed seam, now unrepresentable)
// and an invisible `chapter:<route>` label so `cross-link()`'s
// `link(label-for(path), …)` resolves inside the stitched document.
#let pdf-scope(c) = (
  c.path != none
    and (
      c.path in ("intro.typ", "architecture.typ")
        or c.path.starts-with("spec/")
        or c.path.starts-with("ref/")
    )
)
#for c in flatten-chapters(chapters).filter(pdf-scope) {
  pagebreak(weak: true)
  [#metadata(c.title)#label-for(c.path)]
  include "/" + c.path
}
#pagebreak(weak: true)
#bibliography("/lib/bib.yml")
