# Plan 0314: mkBuildHelper v2 — consolidate 5 divergent scenario build() helpers

[`mkBuildHelper` at common.nix:540](../../nix/tests/common.nix) has **zero callers** since the scenario refactor ([`ab276f69`](https://github.com/search?q=ab276f69&type=commits) + [`8bd55ddd`](https://github.com/search?q=8bd55ddd&type=commits)). Scenarios outgrew it in two ways: (1) it bakes `testDrvFile` at Nix-eval time (one drv per test), but scenarios need `drv_file` as a Python runtime param (multiple drvs per test); (2) it uses raw `journalctl` dump, scenarios use `dump_all_logs()` from [`assertions.py`](../../nix/tests/lib/assertions.py). The doc-comment example at [`common.nix:26`](../../nix/tests/common.nix) still shows `${common.mkBuildHelper {...}}` — dead reference.

Five divergent `build()` re-implementations have accumulated, each sharing the core `nix-build --store ssh-ng:// --arg busybox` try/except/dump/raise skeleton:

| File:line | Name | Params | Dump target | Notable delta |
|---|---|---|---|---|
| [`lifecycle.nix:339`](../../nix/tests/scenarios/lifecycle.nix) | `build` | `drv_file, capture_stderr` | `dump_all_logs([], kube_node=...)` | k3s-full: kubectl-based dump |
| [`scheduling.nix:101`](../../nix/tests/scenarios/scheduling.nix) | `build` | `drv_file, attr, capture_stderr` | `dump_all_logs([host] + workers)` | standalone: `-A attr` support |
| [`observability.nix:75`](../../nix/tests/scenarios/observability.nix) | `build_chain` | (none — fixed drv) | `dump_all_logs([host] + workers)` | single fixed drv, simplest |
| [`security.nix:281`](../../nix/tests/scenarios/security.nix) | `build_drv` | `identity_file, drv_path, expect_fail` | (own variant) | ssh-key querystring in store URL |
| [`fod-proxy.nix:207`](../../nix/tests/scenarios/fod-proxy.nix) (p243) | `build` | `drv_file, extra_args, expect_fail` | `dump_all_logs([], kube_node=...)` | `timeout 90` wrapper + `--timeout 60 --max-silent-time 60` |

Surfaced by [P0294](plan-0294-build-crd-full-rip.md) shrinking lifecycle.nix from ~1600 to ~1000 LOC — the file got small enough to see the duplication. The consolidator noted: "4th divergent copy already landed, 5th will land with next scenario file (p243 adds fod-proxy.nix — check if it grows a build() too)." **Confirmed:** p243's fod-proxy.nix has `build()` at :207 with an `expect_fail` + `timeout 90` variant. A 6th scenario file would grow a 6th copy.

## Entry criteria

- [P0206](plan-0206-path-tenants-migration-upsert-completion.md) merged (touches lifecycle.nix)
- [P0215](plan-0215-worker-max-silent-time.md) merged (touches lifecycle.nix + scheduling.nix)
- [P0216](plan-0216-rio-cli-subcommands.md) merged (touches cli.nix + lifecycle.nix)
- [P0243](plan-0243-vm-fod-proxy-scenario.md) merged (fod-proxy.nix exists with the 5th `build()`)

All four are UNIMPL with active worktrees as of this writing — this plan waits until they merge to avoid 4-way rebase pain on the hottest scenario file.

## Tasks

### T1 — `refactor(nix):` delete dead mkBuildHelper

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix):

- Delete `mkBuildHelper` at `:540-561` (22 lines, zero callers)
- Delete the doc-comment usage example at `:26`:
  ```nix
  #       ${common.mkBuildHelper { gatewayHost = "control"; inherit testDrvFile; }}
  ```
  Replace with a forward reference to the v2 pattern (T2).

### T2 — `feat(nix):` mkBuildHelper v2 — Nix-eval config, Python-runtime params

NEW attr in [`nix/tests/common.nix`](../../nix/tests/common.nix) where `mkBuildHelper` was:

```nix
  # Scenario build() helper v2. Supersedes the old mkBuildHelper which
  # baked drv_file at Nix-eval time (one drv per test). Scenarios need
  # multiple drvs per test, so drv_file is a PYTHON-runtime param now.
  #
  # Nix-eval config (varies by fixture, not by call):
  #   gatewayHost — ssh-ng://<this> store URL
  #   dumpLogsExpr — Python expression called in except: (differs for
  #                  k3s-full vs standalone — see usage below)
  #
  # Python-runtime params (vary per call):
  #   drv_file — path to .nix file (or .drv)
  #   attr — -A attribute name (default: build the file's main expr)
  #   extra_args — arbitrary --arg/--argstr for FOD scenarios
  #   capture_stderr — 2>&1 (default True; False for stderr-separate tests)
  #   expect_fail — use client.fail instead of client.succeed
  #   timeout_wrap — `timeout N` outer wrapper (fod-proxy uses 90s)
  #
  # Does NOT cover security.nix's identity_file variant (ssh-key
  # querystring in store URL) — that's a different store-URL shape
  # entirely, keep it separate.
  mkBuildHelperV2 =
    { gatewayHost, dumpLogsExpr }:
    ''
      def build(drv_file, attr="", extra_args="", capture_stderr=True,
                expect_fail=False, timeout_wrap=None):
          cmd = (
              f"nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
              f"--arg busybox '(builtins.storePath ${busybox})' "
              f"{extra_args} {drv_file}"
          )
          if attr:
              cmd += f" -A {attr}"
          if capture_stderr:
              cmd += " 2>&1"
          if timeout_wrap is not None:
              cmd = f"timeout {timeout_wrap} {cmd}"
          try:
              if expect_fail:
                  return client.fail(cmd)
              return client.succeed(cmd)
          except Exception:
              ${dumpLogsExpr}
              raise
    '';
```

**Usage pattern (document in the doc-comment at `:26`):**

```nix
# k3s-full scenarios:
${common.mkBuildHelperV2 {
  gatewayHost = "k3s-server";
  dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
}}

# standalone scenarios:
${common.mkBuildHelperV2 {
  gatewayHost = gatewayHost;  # usually "control"
  dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
}}
```

### T3 — `refactor(nix):` migrate lifecycle + scheduling + observability + fod-proxy

Replace each inline `def build()` with `${common.mkBuildHelperV2 {...}}`:

**[`lifecycle.nix:335-351`](../../nix/tests/scenarios/lifecycle.nix)** (k3s-full, kube_node dump):
```nix
# before: 17-line inline def build(drv_file, capture_stderr=True)
# after:
${common.mkBuildHelperV2 {
  gatewayHost = "k3s-server";
  dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
}}
```
All existing `build(...)` callsites unchanged — same signature, just more optional params.

**[`scheduling.nix:101-115`](../../nix/tests/scenarios/scheduling.nix)** (standalone, `-A attr` already used):
```nix
${common.mkBuildHelperV2 {
  gatewayHost = gatewayHost;
  dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
}}
```
Callsites unchanged — `attr=` param already matches.

**[`observability.nix:75-87`](../../nix/tests/scenarios/observability.nix)** — `build_chain` has no params (single fixed drv). Rewrite the one callsite:
```nix
${common.mkBuildHelperV2 {
  gatewayHost = gatewayHost;
  dumpLogsExpr = "dump_all_logs([${gatewayHost}] + workers)";
}}
# Then at :100:
output = build("${drvs.chain}")  # was build_chain()
```

**[`fod-proxy.nix:207-221`](../../nix/tests/scenarios/fod-proxy.nix)** (p243 — exists post-P0243-merge) — the `timeout 90` + `--timeout 60 --max-silent-time 60` variant. The v2 helper's `timeout_wrap=90` covers the outer bound; the inner `--timeout 60 --max-silent-time 60` becomes `extra_args`:
```nix
${common.mkBuildHelperV2 {
  gatewayHost = "k3s-server";
  dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
}}
# Callsites change:
# before: build("${drvs.fodFetch}", extra_args="--argstr url ...")
# after:  build("${drvs.fodFetch}", extra_args="--timeout 60 --max-silent-time 60 --argstr url ...", timeout_wrap=90)
```
**OR** — if fod-proxy's inner nix-build timeouts should be the default for ALL builds (they bound rio-gateway/scheduler dispatch hangs), bake them into the v2 helper's cmd string. Decide at dispatch based on whether lifecycle/scheduling would benefit.

### T4 — `refactor(nix):` security.nix kept separate — add comment

MODIFY [`nix/tests/scenarios/security.nix:281`](../../nix/tests/scenarios/security.nix) — `build_drv` stays. Its `identity_file` param changes the store URL itself (`ssh-ng://<host>?ssh-key=<identity_file>`), not just args. Add a comment above `:281`:

```python
# NOT mkBuildHelperV2 — identity_file goes in the store URL
# querystring, not --arg. Different shape. See common.nix
# mkBuildHelperV2 doc-comment.
def build_drv(identity_file, drv_path, expect_fail=False):
```

This prevents a future consolidator from flagging it as "missed during P0314."

## Exit criteria

- `/nbr .#ci` green — all four migrated scenarios pass with the v2 helper
- `grep -c 'mkBuildHelper\b' nix/tests/common.nix` → 0 (v1 deleted; v2 is `mkBuildHelperV2`)
- `grep -c 'mkBuildHelperV2' nix/tests/common.nix` → ≥1 (definition present)
- `grep -c 'mkBuildHelperV2' nix/tests/scenarios/*.nix` → ≥4 (lifecycle, scheduling, observability, fod-proxy all use it)
- `grep -c 'def build\b\|def build_chain' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/scheduling.nix nix/tests/scenarios/observability.nix` → 0 (inline defs gone)
- `grep 'NOT mkBuildHelperV2' nix/tests/scenarios/security.nix` → ≥1 (T4 comment present)
- `grep -c 'def build\b' nix/tests/scenarios/fod-proxy.nix` → 0 (5th copy migrated)

## Tracey

No marker changes. VM test helper consolidation is not spec-covered — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:16`](../../.claude/lib/onibus/tracey.py).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: delete dead mkBuildHelper :540-561 + doc-comment :26; T2: add mkBuildHelperV2"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T3: replace def build :335-351 with mkBuildHelperV2 splice"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T3: replace def build :101-115 with mkBuildHelperV2 splice"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T3: replace build_chain :75-87 + callsite :100 with mkBuildHelperV2"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T3: replace def build :207-221 with mkBuildHelperV2 + timeout_wrap=90 (exists post-P0243)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T4: add NOT-mkBuildHelperV2 comment above build_drv :281 (intentionally separate)"}
]
```

```
nix/tests/
├── common.nix                    # T1: -mkBuildHelper  T2: +mkBuildHelperV2
└── scenarios/
    ├── lifecycle.nix             # T3: def build → ${common.mkBuildHelperV2 {...}}
    ├── scheduling.nix            # T3: same
    ├── observability.nix         # T3: build_chain → build("${drvs.chain}")
    ├── fod-proxy.nix             # T3: same (post-P0243)
    └── security.nix              # T4: comment only (kept separate)
```

## Dependencies

```json deps
{"deps": [206, 215, 216, 243], "soft_deps": [0313, 304], "note": "deps=[206,215,216,243] are REBASE-PAIN AVOIDANCE, not semantic — all four have active worktrees touching scenario files (p206:lifecycle, p215:lifecycle+scheduling, p216:cli+lifecycle, p243:adds fod-proxy.nix with the 5th build()). Scheduling after their merge means one clean refactor instead of four rebases. Soft: P0313 (kvmCheck) also touches scenario preludes (one-line prepend, non-overlapping section). P0304 T9 extracts submit_build_grpc in lifecycle.nix — also non-overlapping."}
```

**Depends on:** [P0206](plan-0206-path-tenants-migration-upsert-completion.md), [P0215](plan-0215-worker-max-silent-time.md), [P0216](plan-0216-rio-cli-subcommands.md), [P0243](plan-0243-vm-fod-proxy-scenario.md) — all UNIMPL with active worktrees. This plan is a leaf behind all four.

**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=14 (hottest scenario file). The deps list IS the conflict-avoidance: by the time this dispatches, p206/p215/p216 have merged and their line-refs are stable. [P0313](plan-0313-kvm-fast-fail-preamble.md) touches the **prelude top** (before `start_all()`); this plan touches **inside the prelude** (`build()` helper at lifecycle:335+). Different sections, trivial merge.

**Payoff:** 5 divergent copies → 1 definition + 4 splices + 1 documented exception. The 6th scenario file doesn't grow a 6th copy.
