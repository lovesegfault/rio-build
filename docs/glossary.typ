#import "/lib/rio.typ": *
#import "/lib/glossary.typ": glossary-entries
// This IS the glossary chapter — it owns the `<key>` anchors via
// `print-glossary` below; tell `rio()` not to emit its hidden anchor set.
#provides-glossary()

#show: rio.with(domains: none)

#print-glossary(
  glossary-entries,
  show-all: true,
  // Page backrefs are meaningless in HTML (every chapter is "page 1");
  // PDF keeps them. QA2-D. The 3 sla-sizing.typ print-glossary calls
  // pass `user-print-back-references: muted-backrefs` which gates the
  // same way (lib/rio.typ).
  disable-back-references: is-html-target(),
)
