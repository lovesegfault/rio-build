// shiroa book manifest. `shiroa build` reads `<shiroa-book-meta>` from
// this file to discover the chapter list; chapter paths are resolved
// relative to this file (and absolute `/lib/...` imports inside
// chapters resolve against `--root`).
//
// print.html: intentionally not generated — `nix build .#docs-pdf`
// (the stitched book-pdf.typ aggregate) is the print equivalent.
// shiroa-mdbook hardcodes `print-enable = false` anyway.
//
// Known console warning: shiroa.js:4262 "deprecated parameters for
// initSync()" — wasm-bindgen API churn in the typst.ts renderer
// bundle (rio-pin's vendored assets/artifacts/shiroa.js). Benign;
// patching the generated bundle is fragile. Tracked at
// Myriad-Dreamin/typst.ts for the next renderer release.
#import "@preview/shiroa:0.3.1": *

#show: book

// Default dest-dir is `./dist` (= docs/dist/, gitignored). shiroa's
// watcher is typst-dependency-based, not directory-recursive, so
// writing here does NOT trigger spurious rebuilds. nix/docs.nix
// passes `-d $out` explicitly so this only matters for local builds.
#build-meta(dest-dir: "./dist")

#book-meta(
  title: "rio-build design book",
  repository: "https://github.com/lovesegfault/rio-build",
  repository-edit: "https://github.com/lovesegfault/rio-build/edit/main/docs/{path}",
  summary: [
    #chapter("intro.typ")[Introduction]
    = Guide
    #chapter("guide/setup.typ")[Setup]
    #chapter("guide/ci.typ")[CI Integration]
    #chapter("guide/programmatic.typ")[Programmatic Access]
    = Architecture
    #chapter("architecture.typ")[System Architecture]
    // Nested parts work since shiroa-mdbook is built from our fork
    // (rio-pin → PR #239: items.sum(default: [])); upstream 0.3.1
    // crashes on a `=` part with no direct chapters.
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
    #chapter("spec/components/replay.typ")[Replay Engine]
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
