// docs/lib/html/meta.typ
// Single source of truth for the design-book chapter tree.
// Consumed by book.typ (HTML routing), book-pdf.typ (include order),
// lib/html/nav.typ (sidebar), and lib/rio.typ (cross-link resolution).

#let repo-edit-base = "https://github.com/lovesegfault/rio-build/edit/main/docs/"

// (title, path, children) — path is relative to docs/, children is an
// array of the same shape. A `path: none` node is a section heading
// (renders in nav, no page of its own).
#let chapters = (
  ("Introduction", "intro.typ", ()),
  (
    "Guide",
    none,
    (
      ("Setup", "guide/setup.typ", ()),
      ("CI Integration", "guide/ci.typ", ()),
      ("Programmatic Access", "guide/programmatic.typ", ()),
    ),
  ),
  (
    "Architecture",
    none,
    (
      ("System Architecture", "architecture.typ", ()),
    ),
  ),
  (
    "Spec",
    none,
    (
      (
        "System",
        none,
        (
          ("Observability", "spec/system/observability.typ", ()),
          ("Security & Threat Model", "spec/system/security.typ", ()),
          ("Multi-Tenancy", "spec/system/tenancy.typ", ()),
          ("Failure Modes", "spec/system/failure-modes.typ", ()),
          ("Verification", "spec/system/verification.typ", ()),
          ("Deployment", "spec/system/deployment.typ", ()),
          ("Crate Structure", "spec/system/crate-structure.typ", ()),
        ),
      ),
      (
        "Components",
        none,
        (
          ("Protocol", "spec/components/proto.typ", ()),
          ("Gateway", "spec/components/gateway.typ", ()),
          (
            "Scheduler",
            "spec/components/scheduler.typ",
            (
              ("SLA-Driven Sizing", "spec/components/sla-sizing.typ", ()),
            ),
          ),
          ("Builder", "spec/components/builder.typ", ()),
          ("Fetcher", "spec/components/fetcher.typ", ()),
          (
            "Store",
            "spec/components/store.typ",
            (
              ("Lazy Store Filesystem", "spec/components/lazy-store.typ", ()),
            ),
          ),
          ("Controller", "spec/components/controller.typ", ()),
          ("Dashboard", "spec/components/dashboard.typ", ()),
          ("CLI", "spec/components/cli.typ", ()),
        ),
      ),
    ),
  ),
  (
    "Reference",
    none,
    (
      ("Configuration", "ref/configuration.typ", ()),
      ("Error Taxonomy", "ref/errors.typ", ()),
      ("Metric Reference", "ref/metrics.typ", ()),
      ("Alert Rules", "ref/alerts.typ", ()),
    ),
  ),
  (
    "Ops",
    none,
    (
      ("Capacity Planning", "ops/capacity-planning.typ", ()),
      ("GC Enablement", "ops/gc-enablement.typ", ()),
      ("EKS Smoke Test", "ops/eks-smoke.typ", ()),
      ("SLA Model Runbook", "ops/sla-model.typ", ()),
      ("Hung-Node Manual Reap", "ops/hung-node-manual-reap.typ", ()),
      ("Pull Rollout Checklist", "ops/pull-rollout-checklist.typ", ()),
      (
        "Gateway Deployment Checklist",
        "ops/gateway-deployment-checklist.typ",
        (),
      ),
      (
        "Materialization Deployment Checklist",
        "ops/materialization-deployment-checklist.typ",
        (),
      ),
    ),
  ),
  (
    "Appendix",
    none,
    (
      ("Glossary", "glossary.typ", ()),
      ("Contributing", "contributing.typ", ()),
    ),
  ),
)

#let flatten-chapters(tree, depth: 0) = {
  let out = ()
  for (title, path, children) in tree {
    out.push((title: title, path: path, depth: depth))
    out += flatten-chapters(children, depth: depth + 1)
  }
  out
}

#let route-for(path) = {
  assert(path != none, message: "route-for: section headings have no route")
  if path == "intro.typ" { "index" } else { path.slice(0, path.len() - 4) }
}

#let label-for(path) = label("chapter:" + route-for(path))
