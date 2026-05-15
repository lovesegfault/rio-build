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
    = Architecture
    #chapter("architecture.typ")[System Architecture]
    = Spec
    == System
    #chapter("spec/system/observability.typ")[Observability]
    #chapter("spec/system/security.typ")[Security & Threat Model]
    #chapter("spec/system/tenancy.typ")[Multi-Tenancy]
    #chapter("spec/system/failure-modes.typ")[Failure Modes]
    #chapter("spec/system/verification.typ")[Verification]
    #chapter("spec/system/deployment.typ")[Deployment]
    #chapter("spec/system/crate-structure.typ")[Crate Structure]
    == Components
    #chapter("spec/components/proto.typ")[Protocol]
    #chapter("spec/components/gateway.typ")[Gateway]
    - #chapter("spec/components/scheduler.typ")[Scheduler]
      - #chapter("spec/components/sla-sizing.typ")[SLA-Driven Sizing]
    #chapter("spec/components/builder.typ")[Builder]
    #chapter("spec/components/fetcher.typ")[Fetcher]
    - #chapter("spec/components/store.typ")[Store]
      - #chapter("spec/components/lazy-store.typ")[Lazy Store Filesystem]
    #chapter("spec/components/controller.typ")[Controller]
    #chapter("spec/components/dashboard.typ")[Dashboard]
    #chapter("spec/components/cli.typ")[CLI]
    = Reference
    #chapter("ref/configuration.typ")[Configuration]
    #chapter("ref/errors.typ")[Error Taxonomy]
    #chapter("ref/metrics.typ")[Metric Reference]
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
