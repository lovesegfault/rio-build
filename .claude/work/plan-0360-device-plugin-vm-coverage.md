# Plan 0360: Device-plugin production path VM coverage — non-privileged k3s fixture variant

[P0286](plan-0286-privileged-hardening-device-plugin.md) post-PASS review — two related test-gap findings:

1. **cgroup rw-remount never tested.** [`rio-worker/src/cgroup.rs:305-318`](../../rio-worker/src/cgroup.rs) claims the `MS_REMOUNT | MS_BIND` to make `/sys/fs/cgroup` writable is load-bearing under `privileged: false` (containerd mounts it RO for non-privileged pods). The `TODO(P0286)` at `:301-304` says "exercise it in VM tests once non-privileged + device-plugin is wired." P0286 T6 deletes the TODO without adding the test. Zero tests exercise the remount path — all VM fixtures use `privileged: true` ([`vmtest-full.yaml:142`](../../infra/helm/rio-build/values/vmtest-full.yaml)), which makes containerd mount rw already so the remount is a no-op.

2. **Device-plugin mechanism never exercised.** P0286 T5 adds a `privileged-hardening` fragment to [`security.nix`](../../nix/tests/scenarios/security.nix) that asserts the pod spec SHAPE (`hostUsers is False`, `seccompProfile.type == Localhost`). But with `vmtest-full.yaml:142` still `privileged: true`, the worker pod gets the escape hatch. The fragment proves the controller RENDERS the right spec when `privileged: false` — it does NOT prove the spec WORKS (FUSE mount succeeds via device-plugin injection, cgroup remount succeeds, build completes). P0286 T5's note "k3s fixture caveat: smarter-device-manager needs to be in the airgap image set" flags the blocker but doesn't close it.

**Root cause shared:** no VM fixture runs the non-privileged + device-plugin path. [`phase4c.md:63`](../../docs/src/phases-archive/phase4c.md) scheduled this as "unit test or defer to device-plugin VM test" — P0286 closed the device-plugin deployment without closing the test.

**This plan's scope:** add a second values file (`vmtest-full-nonpriv.yaml`) + a security.nix fragment that exercises the full production path: smarter-device-manager DaemonSet Ready → worker pod requests `smarter-devices/fuse` via resources.limits → `hostUsers: false` admitted → cgroup rw-remount succeeds → FUSE mount works → build completes. The existing `privileged: true` fixture remains (fast-path for scenarios that don't care about the security posture).

## Entry criteria

- [P0286](plan-0286-privileged-hardening-device-plugin.md) merged — `infra/helm/rio-build/templates/device-plugin.yaml` exists; builders.rs device-plugin branch exists; `r[sec.pod.fuse-device-plugin]` + `r[sec.pod.host-users-false]` markers exist in [`security.md`](../../docs/src/security.md)

## Tasks

### T1 — `feat(nix):` smarter-device-manager in docker-pulled.nix airgap set

MODIFY [`nix/docker-pulled.nix`](../../nix/docker-pulled.nix). Add a pinned smarter-device-manager image to the pulled set (same pattern as the existing entries — `dockerTools.pullImage` with pinned digest + sha256):

```nix
smarter-device-manager = pkgs.dockerTools.pullImage {
  imageName = "registry.gitlab.com/arm-research/smarter/smarter-device-manager";
  imageDigest = "sha256:<pin-at-dispatch>";  # v1.20.x — check gitlab tags
  sha256 = "<nix-prefetch-docker output>";
  finalImageName = "smarter-device-manager";
  finalImageTag = "v1.20";
};
```

The image must be added to whatever aggregate set `extraImages` pulls from in [`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix). Grep `extraImages` + `rioImages` at dispatch — P0286's T5 note said "add to `nix/tests/common.nix` image bundle" but the airgap set is actually in `docker-pulled.nix` + `k3s-full.nix:210,292` (`images = [...] ++ rioImages`).

**Prefetch recipe** (run at dispatch to fill digest+sha256):
```bash
nix develop -c nix-prefetch-docker \
  registry.gitlab.com/arm-research/smarter/smarter-device-manager v1.20
```

### T2 — `feat(infra):` values/vmtest-full-nonpriv.yaml — device-plugin path variant

NEW [`infra/helm/rio-build/values/vmtest-full-nonpriv.yaml`](../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml). Layers on `vmtest-full.yaml` via Helm's `-f file1 -f file2` semantics (last wins). Minimal override:

```yaml
# Non-privileged + device-plugin VM fixture variant. Used ONLY by
# security.nix privileged-hardening-e2e fragment. vmtest-full.yaml
# stays privileged:true (fast path — most scenarios don't care about
# pod security posture and the escape hatch avoids the device-plugin
# DaemonSet bring-up tax ~30-60s).

workerPool:
  # Flip the escape hatch OFF. P0286 makes false the production
  # default in values.yaml; vmtest-full.yaml keeps true. This
  # overrides BACK to false for the one scenario that proves the
  # production path works.
  privileged: false
  # seccomp: RuntimeDefault suffices for k3s (Localhost needs the
  # profile installed on nodes, which vmtest doesn't do). P0286's
  # Localhost profile is production-only.
  seccompProfile:
    type: RuntimeDefault

devicePlugin:
  # P0286 T1 added this gate, default true. Redundant here but
  # explicit for clarity.
  enabled: true
```

MODIFY [`nix/tests/fixtures/k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) — parameterize `valuesFile`. Current `:65` hardcodes `vmtest-full.yaml`. Add a function argument:

```nix
{
  extraValues ? { },
  extraImages ? [ ],
  # NEW: which values file to render. Default = privileged fast-path.
  # security.nix privileged-hardening-e2e passes vmtest-full-nonpriv.yaml.
  valuesFile ? ../../../infra/helm/rio-build/values/vmtest-full.yaml,
}:
```

And at `:65`: `valuesFile = valuesFile;` (shadowing the default). **Alternative:** if `helm-render.nix` supports `valuesFiles = [ f1 f2 ]` layering, pass both `[vmtest-full.yaml vmtest-full-nonpriv.yaml]` — cleaner because the nonpriv file stays minimal (only the overrides). Check at dispatch.

### T3 — `test(vm):` security.nix privileged-hardening-e2e fragment

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix). P0286 T5 adds a `privileged-hardening` fragment that asserts pod spec SHAPE. This task adds a SECOND fragment `privileged-hardening-e2e` that proves MECHANISM. It uses the T2 `vmtest-full-nonpriv.yaml` fixture:

```nix
privileged-hardening-e2e = {
  # Uses the nonpriv values variant + smarter-device-manager image.
  fixture = k3sFull {
    valuesFile = ../../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml;
    extraImages = [ pulled.smarter-device-manager ];
  };
  testScript = ''
    # ── Device plugin DaemonSet Ready ───────────────────────────────
    # P0286 T1 deploys smarter-device-manager as a DaemonSet. It
    # needs to register with kubelet BEFORE the worker pod schedules
    # (otherwise resources.limits["smarter-devices/fuse"] is
    # unschedulable — "insufficient smarter-devices/fuse").
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} rollout status ds/smarter-device-manager --timeout=90s",
        timeout=120,
    )

    # Verify kubelet sees the extended resource.
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node k3s-agent -o jsonpath="
        "'{.status.allocatable.smarter-devices/fuse}' | grep -qE '^[1-9]'",
        timeout=60,
    )

    # ── Worker pod Ready (non-privileged path) ──────────────────────
    # r[verify sec.pod.fuse-device-plugin]
    # r[verify sec.pod.host-users-false]
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} wait --for=condition=Ready "
        "pod/default-workers-0 --timeout=180s",
        timeout=210,
    )

    # Assert hostUsers:false was admitted (NOT suppressed).
    pod_json = k3s_server.succeed(
        "k3s kubectl -n ${ns} get pod default-workers-0 -o json"
    )
    import json
    pod = json.loads(pod_json)
    assert pod["spec"].get("hostUsers") is False, (
        f"expected hostUsers:false, got {pod['spec'].get('hostUsers')!r}"
    )
    # And privileged is absent/false.
    sc = pod["spec"]["containers"][0].get("securityContext", {})
    assert not sc.get("privileged", False), f"still privileged: {sc}"

    # ── cgroup rw-remount succeeded ─────────────────────────────────
    # r[verify worker.cgroup.ns-root-remount]
    # The worker's delegated_root() remounts /sys/fs/cgroup rw and
    # creates /leaf/. Under privileged:true, containerd mounts rw
    # already → the remount is a no-op. Under privileged:false,
    # containerd mounts RO → the remount is load-bearing.
    # Proof: /sys/fs/cgroup/leaf/ exists inside the worker container.
    k3s_server.succeed(
        "k3s kubectl -n ${ns} exec default-workers-0 -- "
        "test -d /sys/fs/cgroup/leaf"
    )
    # And subtree_control has controllers enabled (proves the worker
    # wrote to it — only possible if rw-remount worked).
    k3s_server.succeed(
        "k3s kubectl -n ${ns} exec default-workers-0 -- "
        "grep -q memory /sys/fs/cgroup/cgroup.subtree_control"
    )

    # ── Build completes (FUSE works via device-plugin) ──────────────
    # The FUSE mount is the overlay lower layer. If device-plugin
    # injection failed, the mount fails, the worker never registers,
    # and this times out. If it succeeds, a build through the full
    # path proves the MECHANISM end-to-end.
    build_hello(k3s_server, ns)  # existing helper — grep at dispatch
  '';
};
```

Wire the fragment into [`nix/tests/default.nix`](../../nix/tests/default.nix) — add to the `subtests` list for `vm-security-k3s` (or whatever the security.nix scenario target is named; grep `security` in `default.nix`). **CRITICAL** (P0289 r2 lesson): the `r[verify]` markers are only valid if the fragment is actually in the `subtests` list. Verify via `grep 'privileged-hardening-e2e' nix/tests/default.nix` ≥1 hit before claiming tracey coverage.

**Use the submit_build_grpc helper** (landed in [P0362](plan-0362-extract-submit-build-grpc-helper.md)
— lifecycle.nix testScript preamble) instead of copy-pasting the
port-forward+grpcurl+JSON-parse block a 5th time. If P0362 hasn't
merged yet, this fragment is the 5th copy — note it for later cleanup.
If the fragment uses an ssh-ng build helper (as sketched above with
`build_hello`), the gRPC SubmitBuild path may not be needed — check at
dispatch.

### T4 — `docs(worker):` r[worker.cgroup.ns-root-remount] marker — rw-remount load-bearing under non-privileged

MODIFY [`docs/src/components/worker.md`](../../docs/src/components/worker.md). Add new marker after `r[worker.cgroup.sibling-layout]` at `:286` (standalone paragraph, blank line before, col 0):

See `## Spec additions` for text.

MODIFY [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs) at the remount call (`:305`) — add `// r[impl worker.cgroup.ns-root-remount]` above the `nix::mount::mount(` call. P0286 T6 deletes the `TODO(P0286)` at `:301`; this task adds the tracey annotation so the remount path is spec-traceable.

## Exit criteria

- `/nbr .#ci` green — **includes** the new `privileged-hardening-e2e` fragment proving non-privileged FUSE + cgroup remount work end-to-end
- `grep 'privileged: false' infra/helm/rio-build/values/vmtest-full-nonpriv.yaml` → 1 hit (T2: override present)
- `grep 'smarter-device-manager' nix/docker-pulled.nix` → ≥1 hit (T1: image in airgap set)
- `grep 'privileged-hardening-e2e' nix/tests/scenarios/security.nix` → ≥1 hit (T3: fragment defined)
- `grep 'privileged-hardening-e2e' nix/tests/default.nix` → ≥1 hit (T3: fragment **wired** — P0289 r2 lesson; markers lie if unwired)
- `grep 'test -d /sys/fs/cgroup/leaf' nix/tests/scenarios/security.nix` → ≥1 hit (T3: cgroup remount proof-probe present)
- `grep 'r\[impl worker.cgroup.ns-root-remount\]' rio-worker/src/cgroup.rs` → 1 hit (T4: annotation added)
- `nix develop -c tracey query rule worker.cgroup.ns-root-remount` → shows spec text + 1 impl + 1 verify (T4+T3)
- `nix develop -c tracey query rule sec.pod.fuse-device-plugin` → shows ≥1 verify in `security.nix` (T3 — second verify site; P0286 T5's shape-check fragment is the first)

## Tracey

References existing markers:
- `r[sec.pod.fuse-device-plugin]` — T3 adds second `r[verify]` (mechanism proof; P0286 T5 adds first, shape proof)
- `r[sec.pod.host-users-false]` — T3 adds second `r[verify]` (hostUsers admitted under device-plugin path)
- `r[worker.cgroup.sibling-layout]` — T3's `/leaf/` probe is this marker's "move itself into a `/leaf/` subgroup" clause under the RO-cgroup condition

Adds new markers to component specs:
- `r[worker.cgroup.ns-root-remount]` → [`docs/src/components/worker.md`](../../docs/src/components/worker.md) (see ## Spec additions)

## Spec additions

New marker in [`docs/src/components/worker.md`](../../docs/src/components/worker.md), inserted after `r[worker.cgroup.sibling-layout]` at `:286`, standalone paragraph, blank line before, col 0:

```
r[worker.cgroup.ns-root-remount]
When running in a cgroup-namespace root (`/proc/self/cgroup` shows `0::/`) under a non-privileged security context, the worker MUST remount `/sys/fs/cgroup` read-write before creating the `/leaf/` subgroup. Containerd mounts `/sys/fs/cgroup` read-only for non-privileged pods even with `CAP_SYS_ADMIN`; the `MS_REMOUNT | MS_BIND` call clears the per-mount-point RO flag (preserving superblock `nosuid`/`nodev`/`noexec`). Under `privileged: true` containerd mounts rw already and the remount is a no-op — this path is load-bearing only in the production `privileged: false` + device-plugin configuration (ADR-012).
```

## Files

```json files
[
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T1: +smarter-device-manager pullImage pinned digest"},
  {"path": "infra/helm/rio-build/values/vmtest-full-nonpriv.yaml", "action": "NEW", "note": "T2: privileged:false + devicePlugin.enabled override layered on vmtest-full.yaml"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T2: parameterize valuesFile arg (default stays vmtest-full.yaml)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T3: +privileged-hardening-e2e fragment — DS Ready, extended-resource allocatable, worker Ready, hostUsers admitted, /leaf exists, subtree_control writable, build completes"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: wire privileged-hardening-e2e into vm-security subtests list"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "T4: +r[impl worker.cgroup.ns-root-remount] annotation at :305 mount call"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "T4 / Spec additions: +r[worker.cgroup.ns-root-remount] marker after :286"}
]
```

```
nix/
├── docker-pulled.nix                          # T1: +smarter-device-manager
└── tests/
    ├── fixtures/k3s-full.nix                  # T2: valuesFile param
    ├── scenarios/security.nix                 # T3: e2e fragment
    └── default.nix                            # T3: wire fragment
infra/helm/rio-build/values/
└── vmtest-full-nonpriv.yaml                   # T2: NEW override
rio-worker/src/
└── cgroup.rs                                  # T4: annotation
docs/src/components/
└── worker.md                                  # T4: marker
```

## Dependencies

```json deps
{"deps": [286], "soft_deps": [0359], "note": "rev-p286 test-gap — 2 findings combined: (1) cgroup.rs:305 rw-remount load-bearing under privileged:false, zero tests; (2) vmtest-full privileged:true → device-plugin path proves SHAPE not MECHANISM. Root: no VM fixture runs non-privileged + device-plugin. phase4c.md:63 scheduled this, P0286 closed deployment without closing test. discovered_from=286. security.nix count=14 — T3 adds new fragment key (append-only, no overlap with P0286's privileged-hardening fragment). k3s-full.nix — T2 adds one function arg (low conflict). cgroup.rs count=17 — T4 is 1-line annotation add at :305; P0286 T6 deletes :301-304 TODO same region. Sequence: P0286 first (hard-dep), then T4 lands above the post-P0286 mount call. docker-pulled.nix low-traffic. Soft-dep P0359 (hostUsers+hostNetwork guardrail) — if both dispatch together, T3 fragment can include the CEL-reject kubectl-apply probe from P0359 T3-optional; else sequence-independent."}
```

**Depends on:** [P0286](plan-0286-privileged-hardening-device-plugin.md) — `device-plugin.yaml` DaemonSet template, builders.rs device-plugin branch, `r[sec.pod.*]` markers, `devicePlugin.enabled` values flag.

**Soft-dep:** [P0359](plan-0359-hostusers-hostnetwork-guardrail.md) — shares security.nix touch; optional CEL-reject fragment piggyback.

**Conflicts with:** [`security.nix`](../../nix/tests/scenarios/security.nix) count=14 — P0286 T5 adds `privileged-hardening` (shape-check); T3 here adds `privileged-hardening-e2e` (mechanism-check). Different fragment keys, both append-only at the scenario attrset level — rebase-clean. [P0304-T53](plan-0304-trivial-batch-p0222-harness.md) re-tags a `TODO(P0260)` in the same file (different line, pure-text). [`cgroup.rs`](../../rio-worker/src/cgroup.rs) count=17 — P0286 T6 deletes `:301-304`; T4 adds annotation at `:305`. Adjacent lines; if P0286 lands cleanly first (hard-dep), T4's line target is unambiguous. [`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) — [P0304-T14](plan-0304-trivial-batch-p0222-harness.md) extracts `bounceGatewayForSecret` helper at `:486-524`; T2 here adds a function arg at the top-level `{ ... }:` signature `:43-52`. Non-overlapping hunks.
