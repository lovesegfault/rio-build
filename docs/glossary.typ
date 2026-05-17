#import "/lib/rio.typ": *
#import "/lib/glossary.typ": glossary-entries
// This IS the glossary chapter — it owns the `<key>` anchors via
// `print-glossary` below; tell `rio()` not to emit its hidden anchor set.
#provides-glossary()

#show: rio.with(domains: none)

#print-glossary(glossary-entries, show-all: true)
