# KVM-pending VM tests

VM test attrs that carry load-bearing `r[verify]` annotations but have
**never built on a KVM-capable builder** (merged via clause-4 fast-path,
or fast-fail killed before dispatch). The tracey annotations are present
and satisfy `tracey query untested`, but the assertions have never
executed. Coordinator consults this list at KVM-slot-open to burn down
NEVER-BUILT coverage.

Each entry: the VM attr to run, the fragments/subtests that never
executed, and the `r[verify]` markers they carry. Entries are
**confirmatory-not-gating** — CI green doesn't block on these; the
markers will fire RED on first KVM allocation if the committed
assertions are wrong.

---

- **vm-lifecycle-wps-k3s** — `pdb-ownerref` + `wps-lifecycle` fragments (P0239):
  `r[verify ctrl.wps.reconcile]` + `r[verify ctrl.wps.autoscale]` at col-0.
  PDB ownerRef cascade (WorkerPool delete → PDB GC'd) and WPS 3-child
  create→delete→children-gone lifecycle. Fragments at lifecycle.nix
  `fragments = {` block. Never KVM-built if P0239 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-lifecycle-wps-k3s`
  → check `pdb-ownerref` + `wps-lifecycle` subtest output.

- **vm-observability-standalone** — observability.nix:328-368 PARENTING observe-block.
  P0295-T63 committed spec text + assert at :356-368 based on
  mechanism-analysis (set_parent before enter → lazy OTel span alloc
  sees stored parent context). Zero KVM observations in 1278 rix-dev
  logs. If mechanism-analysis is wrong, the assert at :363 fires
  wrong-direction AND spec text at `r[sched.trace.assignment-traceparent]`
  (observability.md:279) is wrong.
  Run: `/nixbuild .#checks.x86_64-linux.vm-observability-standalone`
  → grep log for `CONFIRMED: span_from_traceparent →` line. If
  `PARENTING`, spec commit stands. If `LINK only`, spec text needs
  "produces parent-child" → "produces a link" correction.
