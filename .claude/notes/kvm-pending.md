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

- **vm-fod-proxy-k3s** — (P0311-T16 core). `no tracey marker — FOD proxy mitm e2e` at
  `fod-proxy.nix`. FOD builds through the CONNECT proxy, mitm-proof. Never
  KVM-built if P0308 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-fod-proxy-k3s`

- **vm-scheduling-disrupt-standalone** — (P0311-T16 core). Worker drain → reassign +
  cordon; markers per nix/tests/default.nix registry entry. Never KVM-built
  if scheduling-fragment fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-scheduling-disrupt-standalone`

- **vm-netpol-k3s** — (P0311-T16 core). `no tracey marker — egress-deny NetPol e2e`
  at `netpol.nix`. k8s NetworkPolicy egress-deny holds for worker pods.
  Never KVM-built if P0241 fast-pathed (and per tooling-gotchas, it did —
  merger mode-4 drv-identical; new vm-netpol drv NEVER built).
  Run: `/nixbuild .#checks.x86_64-linux.vm-netpol-k3s`

- **vm-security-standalone** (jwt-mount-present subtest, P0311-T32) —
  scheduler+store have rio-jwt-pubkey ConfigMap mounted at expected path.
  Never KVM-built if P0357 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-security-standalone`
  → check `jwt-mount-present` subtest output.

- **vm-security-nonpriv-k3s** — (P0311-T37). Non-privileged worker container
  variant (devicePlugin + hostUsers:false). Never KVM-built if P0360
  fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-security-nonpriv-k3s`

- **vm-ca-cutoff-standalone** — `r[verify sched.ca.cutoff-propagate]`.
  CA cascade end-to-end (compare → propagate → resolve). Added c1be4a36.
  P0405 refactored `speculative_cascade_reachable` — this test is the
  integration proof the refactor didn't change visible behavior. **HIGH
  PRIORITY** next KVM-slot.
  Run: `/nixbuild .#checks.x86_64-linux.vm-ca-cutoff-standalone`
