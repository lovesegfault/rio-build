// shiroa book manifest. `shiroa build` reads `<shiroa-book-meta>` from
// this file to discover the chapter list; chapter paths are resolved
// relative to this file (and absolute `/lib/...` imports inside
// chapters resolve against `--root`).
#import "@preview/shiroa:0.3.1": *

#show: book

#book-meta(
  title: "rio-build design book",
  summary: [
    #chapter("intro.typ")[Introduction]
    = Guide
    #chapter("guide/setup.typ")[Setup]
    #chapter("guide/ci.typ")[CI Integration]
    #chapter("guide/programmatic.typ")[Programmatic Access]
    = Spec
    #chapter("spec/components/sla-sizing.typ")[SLA-Driven Sizing]
    #chapter("spec/system/_spike.typ")[(spike)]
    = Ops
    #chapter("ops/capacity-planning.typ")[Capacity Planning]
    #chapter("ops/gc-enablement.typ")[GC Enablement]
    #chapter("ops/eks-smoke.typ")[EKS Smoke Test]
    #chapter("ops/sla-model.typ")[SLA Model Runbook]
    = Appendix
    #chapter("glossary.typ")[Glossary]
    #chapter("contributing.typ")[Contributing]
  ],
)
