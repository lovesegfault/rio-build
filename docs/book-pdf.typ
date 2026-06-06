// Single-file PDF aggregate. The shiroa manifest (book.typ) drives the
// HTML site; this file is the `typst compile` entrypoint for the PDF
// check / artifact and stitches chapters via `#include`.
//
// `book-pdf-mode()` sets state so each chapter's `rio()` suppresses its
// per-chapter title-block and bibliography — the aggregate emits one
// `#bibliography()` at the end. This makes bare `typst compile --root
// docs docs/book-pdf.typ` work without `--input x-target=book-pdf`
// (which the nix derivation still passes; both gates are equivalent).
//
// Scope: PDF = intro + architecture + spec/ + ref/. The HTML site
// (book.typ) additionally has guide/, ops/, glossary, contributing —
// operational/contributor content that doesn't belong in the design
// reference PDF. A new SPEC or REF chapter goes in BOTH files; a new
// guide/ops chapter is HTML-only. cross-link()s from PDF chapters to
// HTML-only ones degrade to plain text in the PDF (acceptable).
#import "/lib/rio.typ": book-pdf-mode
#book-pdf-mode()
#set document(title: "rio-build design book")
// Chapter stitch, generated from one array (merged_bug_227): every
// chapter is joined with a weak pagebreak BY CONSTRUCTION — the
// hand-stitched form shipped a missed seam (metrics.typ → alerts.typ
// had no pagebreak), and a missed seam is now unrepresentable. The
// pdf⊆html docs-lint extraction parses this array: one quoted path
// per line, two-space indent, keep that shape.
#let chapters = (
  "intro.typ",
  "architecture.typ",
  "spec/system/observability.typ",
  "spec/system/security.typ",
  "spec/system/tenancy.typ",
  "spec/system/failure-modes.typ",
  "spec/system/verification.typ",
  "spec/system/deployment.typ",
  "spec/system/crate-structure.typ",
  "spec/components/proto.typ",
  "spec/components/gateway.typ",
  "spec/components/scheduler.typ",
  "spec/components/sla-sizing.typ",
  "spec/components/builder.typ",
  "spec/components/fetcher.typ",
  "spec/components/store.typ",
  "spec/components/lazy-store.typ",
  "spec/components/controller.typ",
  "spec/components/dashboard.typ",
  "spec/components/cli.typ",
  "ref/configuration.typ",
  "ref/errors.typ",
  "ref/metrics.typ",
  "ref/alerts.typ",
)
#for c in chapters {
  include c
  pagebreak(weak: true)
}
#bibliography("/lib/bib.yml")
