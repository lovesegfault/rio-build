// Single-file PDF aggregate. The shiroa manifest (book.typ) drives the
// HTML site; this file is the `typst compile` entrypoint for the PDF
// check / artifact and stitches chapters via `#include`.
//
// `book-pdf-mode()` sets state so each chapter's `rio()` suppresses its
// per-chapter title-block and bibliography — the aggregate emits one
// `#bibliography()` at the end. This makes bare `typst compile --root
// docs docs/book-pdf.typ` work without `--input x-target=book-pdf`
// (which the nix derivation still passes; both gates are equivalent).
#import "/lib/rio.typ": book-pdf-mode
#book-pdf-mode()
#set document(title: "rio-build design book")
#include "intro.typ"
#pagebreak(weak: true)
#include "architecture.typ"
#pagebreak(weak: true)
#include "spec/system/observability.typ"
#pagebreak(weak: true)
#include "spec/system/security.typ"
#pagebreak(weak: true)
#include "spec/system/tenancy.typ"
#pagebreak(weak: true)
#include "spec/system/failure-modes.typ"
#pagebreak(weak: true)
#include "spec/system/verification.typ"
#pagebreak(weak: true)
#include "spec/system/deployment.typ"
#pagebreak(weak: true)
#include "spec/system/crate-structure.typ"
#pagebreak(weak: true)
#include "spec/components/proto.typ"
#pagebreak(weak: true)
#include "spec/components/gateway.typ"
#pagebreak(weak: true)
#include "spec/components/scheduler.typ"
#pagebreak(weak: true)
#include "spec/components/sla-sizing.typ"
#pagebreak(weak: true)
#include "spec/components/builder.typ"
#pagebreak(weak: true)
#include "spec/components/fetcher.typ"
#pagebreak(weak: true)
#include "spec/components/store.typ"
#pagebreak(weak: true)
#include "spec/components/lazy-store.typ"
#pagebreak(weak: true)
#include "spec/components/controller.typ"
#pagebreak(weak: true)
#include "spec/components/dashboard.typ"
#pagebreak(weak: true)
#include "spec/components/cli.typ"
#pagebreak(weak: true)
#include "ref/configuration.typ"
#pagebreak(weak: true)
#include "ref/errors.typ"
#pagebreak(weak: true)
#include "ref/metrics.typ"
#pagebreak(weak: true)
#bibliography("/lib/bib.yml")
