# Plan 0314: mkBuildHelper v2 — consolidate 5 divergent scenario build() helpers

[`mkBuildHelper` at common.nix:540](../../nix/tests/common.nix) has **zero callers** since the scenario refactor ([`ab276f69`](https://github.com/search?q=ab276f69&type=commits) + [`8bd55ddd`](https://github.com/search?q=8bd55ddd&type=commits)). Scenarios outgrew it in two ways: (1) it bakes `testDrvFile` at Nix-eval time (one drv per test), but scenarios need `drv_file` as a Python runtime param (multiple drvs per test); (2) it uses raw `journalctl` dump, scenarios use `dump_all_logs()` from [`assertions.py`](../../nix/tests/lib/assertions.py). The doc-comment example at [`common.nix:26`](../../nix/tests/common.nix) still shows `${common.mkBuildHelper {...}}` — dead reference.

Five divergent `build()` re-implementations have accumulated, each sharing the core `nix-build --store ssh-ng:// --arg busybox` try/except/dump/raise skeleton:

> **AMENDMENT (docs-926870, consolidator):** The T4 "security.nix kept separate" carve-out is **dropped**. When this plan was written, [`security.nix:282`](../../nix/tests/scenarios/security.nix) `build_drv` was the ONLY custom-store-URL case. [P0206](plan-0206-path-tenants-migration-upsert.md) added a **second** at [`lifecycle.nix:1266-1276`](../../nix/tests/scenarios/lifecycle.nix) — inline `nix-build --store 'ssh-ng://k3s-server-tenant'` with the comment "Can't use build() — it hardcodes 'ssh-ng://k3s-server'". [P0207](plan-0207-mark-cte-tenant-retention.md) T5 ("Build a path with a tenant SSH key") will add a **third** via the same alias. Three custom-URL cases = `store_url` is a helper param, not a carve-out reason.
>
> **Fix:** T2's v2 helper takes `store_url` as a Python-runtime param (default `ssh-ng://${gatewayHost}` — unchanged for existing callers). Tenant calls pass `store_url="ssh-ng://k3s-server-tenant"`. security.nix's `build_drv` becomes a thin wrapper: `build(drv_path, store_url=f"ssh-ng://root@{host}?ssh-key={identity_file}", expect_fail=expect_fail)`.
>
> **Bonus fold:** `security.nix:299-300` AND `lifecycle.nix:1275-1276` both do the same last-line-extract (`[l.strip() for l in out.split("\n") if l.strip()][-1]`) to skip SSH known_hosts warnings under `2>&1`. T2's v2 as currently written returns raw `client.succeed` output — these two sites post-process. Add `strip_to_store_path: bool` param (default `True` when `capture_stderr=True`): last non-empty line, assert-startswith `/nix/store/`. Absorbs both.
>
> **Why amend BEFORE dispatch:** P0314 already has `deps=[206]`, so the implementer rebases onto `lifecycle.nix:1266` and SEES the inline call. Plan-as-written tells them "leave security.nix alone" → by analogy they'll leave `:1266` alone too. Without this amendment, a 4th consolidator pass. See T2 AMENDED and T4 AMENDED below.


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
  #   store_url — override --store (default ssh-ng://${gatewayHost});
  #               tenant/identity-file cases pass a different URL
  #   strip_to_store_path — return last non-empty line (skips SSH
  #                         warnings under 2>&1); default True when
  #                         capture_stderr=True
  #
  # AMENDED docs-926870: store_url as a runtime param folds in
  # security.nix build_drv (identity_file → ?ssh-key= querystring)
  # AND lifecycle.nix:1266 (tenant Host alias) AND P0207 T5. The
  # v1 plan's "security.nix is a different shape" carve-out was
  # sound at 1 outlier; 3 outliers = it's a param.
  mkBuildHelperV2 =
    { gatewayHost, dumpLogsExpr }:
    ''
      def build(drv_file, attr="", extra_args="", capture_stderr=True,
                expect_fail=False, timeout_wrap=None,
                store_url="ssh-ng://${gatewayHost}",
                strip_to_store_path=None):
          # Default strip_to_store_path = capture_stderr: SSH warnings
          # only appear under 2>&1; if stderr is separate, output is
          # already clean. Caller can override either way.
          if strip_to_store_path is None:
              strip_to_store_path = capture_stderr
          cmd = (
              f"nix-build --no-out-link --store '{store_url}' "
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
              out = client.succeed(cmd)
              if strip_to_store_path:
                  # Last non-empty line is the store path. Earlier
                  # lines: SSH known_hosts warning + build progress.
                  # security.nix:299 + lifecycle.nix:1275 both did
                  # this inline — absorbed here.
                  lines = [l.strip() for l in out.strip().split("\n")
                           if l.strip()]
                  return lines[-1] if lines else ""
              return out
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

**[`fod-proxy.nix:207-221`](../../nix/tests/scenarios/fod-proxy.nix)** (p243 — exists post-P0243-merge) — the `timeout 90` + `--timeout 60 --max-silent-time 60` variant. The v2 helper's `timeout_wrap=90` covers the outer bound.

**DROP `--timeout 60 --max-silent-time 60` entirely** — they are documented no-ops. [P0215](plan-0215-max-silent-time.md) proved ssh-ng clients never send `wopSetOptions`; the comment at [`fod-proxy.nix:200-207`](../../nix/tests/scenarios/fod-proxy.nix) already says "NO-OPS over ssh-ng — the client never sends wopSetOptions (P0215 empirical). Kept as harmless." They are not harmless — they're cargo-cult that the next reader has to reason about. Live bounds are `wget -T 15` (inside the FOD builder) + `timeout 90` (shell wrapper) only.

```nix
${common.mkBuildHelperV2 {
  gatewayHost = "k3s-server";
  dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
}}
# Callsites change:
# before: build("${drvs.fodFetch}", extra_args="--argstr url ...")
# after:  build("${drvs.fodFetch}", extra_args="--argstr url ...", timeout_wrap=90)
# (--timeout 60 --max-silent-time 60 DROPPED — ssh-ng no-ops per P0215)
```

**[`scheduling.nix:720-725`](../../nix/tests/scenarios/scheduling.nix)** — P0215's max-silent-time subtest has an inline `client.fail("nix-build ... ${silenceDrv} 2>&1")` that bypasses the `build()` helper because the scheduling `build()` lacked `expect_fail` at the time. P0314 was authored pre-P0215-merge so this site is not in the original T3 list. After the v2 migration it becomes:

```python
# before (scheduling.nix:720-725):
t0 = _time.monotonic()
out = client.fail(
    "nix-build --no-out-link --store 'ssh-ng://${gatewayHost}' "
    "--arg busybox '(builtins.storePath ${common.busybox})' "
    "${silenceDrv} 2>&1"
)
elapsed = _time.monotonic() - t0

# after:
t0 = _time.monotonic()
out = build("${silenceDrv}", expect_fail=True)
elapsed = _time.monotonic() - t0
```

The `t0`/`elapsed` timing wrap stays at the callsite — it's orthogonal to the helper (measuring end-to-end including ssh setup, not just the build).

**[`lifecycle.nix:1261-1276`](../../nix/tests/scenarios/lifecycle.nix)** (AMENDED — P0206's tenant-key inline build) — the "Can't use build()" comment goes away. After the v2 migration:

```python
# before (lifecycle.nix:1266-1276):
try:
    out_tenant = client.succeed(
        "nix-build --no-out-link "
        "--store 'ssh-ng://k3s-server-tenant' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${tenantDrv} 2>&1"
    )
except Exception:
    dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")
    raise
out_tenant = [l.strip() for l in out_tenant.strip().split("\n")
              if l.strip()][-1]

# after:
out_tenant = build("${tenantDrv}", store_url="ssh-ng://k3s-server-tenant")
# strip_to_store_path=True is the default — last-line-extract absorbed.
```

The `assert out_tenant.startswith("/nix/store/")` at `:1277` stays — it's the test assertion, not post-processing.

### T4 — `refactor(nix):` security.nix build_drv → thin wrapper (AMENDED)

**AMENDED from "kept separate" to "thin wrapper".** `build_drv`'s `identity_file` param is just a `store_url` variant: `ssh-ng://root@${gatewayHost}?ssh-key={identity_file}`. With T2's `store_url` param, `build_drv` collapses to a 3-line wrapper.

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) at `:282-303`. Current body is 22 lines (cmd construction + try/except/dump/raise + last-line-extract). After:

```python
${common.mkBuildHelperV2 {
  gatewayHost = gatewayHost;  # default store_url; build_drv overrides
  dumpLogsExpr = "dump_all_logs([${gatewayHost}, worker])";
}}

def build_drv(identity_file, drv_path, expect_fail=False):
    """Build via ssh-ng using the given identity file (selects the
    matching authorized_keys entry → tenant). Thin wrapper over
    build() — identity_file becomes ?ssh-key= in the store URL."""
    return build(
        drv_path,
        store_url=f"ssh-ng://root@${gatewayHost}?ssh-key={identity_file}",
        expect_fail=expect_fail,
    )
```

The try/except/dump/raise is the helper's. The `lines[-1]` at `:299-300` is the helper's `strip_to_store_path=True` default. `build_drv` keeps its name (12+ callsites in security.nix call it) but is now 4 lines instead of 22.

**Check at dispatch:** `security.nix` uses `worker` (singular) in its dump — [`security.nix:302`](../../nix/tests/scenarios/security.nix) `dump_all_logs([${gatewayHost}, worker])`. Confirm the fixture scope has `worker` bound; if it's `all_workers` or similar, adjust the `dumpLogsExpr` interpolation.

## Exit criteria

- `/nbr .#ci` green — all four migrated scenarios pass with the v2 helper
- `grep -c 'mkBuildHelper\b' nix/tests/common.nix` → 0 (v1 deleted; v2 is `mkBuildHelperV2`)
- `grep -c 'mkBuildHelperV2' nix/tests/common.nix` → ≥1 (definition present)
- `grep -c 'mkBuildHelperV2' nix/tests/scenarios/*.nix` → ≥4 (lifecycle, scheduling, observability, fod-proxy all use it)
- `grep -c 'def build\b\|def build_chain' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/scheduling.nix nix/tests/scenarios/observability.nix` → 0 (inline defs gone)
- `grep -c 'def build\b' nix/tests/scenarios/fod-proxy.nix` → 0 (5th copy migrated)
- `grep -- '--max-silent-time\|--timeout 60' nix/tests/scenarios/fod-proxy.nix` → 0 hits (T3: ssh-ng no-op flags dropped, not preserved in extra_args)
- `grep 'client.fail.*nix-build.*silenceDrv' nix/tests/scenarios/scheduling.nix` → 0 hits (T3: scheduling.nix:720 inline migrated to `build(..., expect_fail=True)`)
- `grep 'store_url' nix/tests/common.nix` → ≥2 hits (T2 AMENDED: param + default)
- `grep 'strip_to_store_path' nix/tests/common.nix` → ≥1 hit (T2 AMENDED: last-line-extract param)
- `grep "Can't use build()\|hardcodes.*k3s-server" nix/tests/scenarios/lifecycle.nix` → 0 hits (T3 AMENDED: :1263 comment obsolete — tenant call uses store_url=)
- `grep 'store_url="ssh-ng://k3s-server-tenant"' nix/tests/scenarios/lifecycle.nix` → ≥1 hit (T3 AMENDED: P0206's inline call migrated)
- `wc -l nix/tests/scenarios/security.nix | awk '{print $1}'` — decreases by ~15 (T4 AMENDED: 22-line build_drv body → 4-line wrapper; splice adds back ~4)
- `grep -c 'mkBuildHelperV2' nix/tests/scenarios/security.nix` → ≥1 (T4 AMENDED: helper splice present, build_drv wraps it)
- `grep 'l.strip.*for l in.*split' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/security.nix | wc -l` → 0 (T2+T3+T4 AMENDED: both last-line-extract sites absorbed into helper's strip_to_store_path)

## Tracey

No marker changes. VM test helper consolidation is not spec-covered — no `harness.*` domain in `TRACEY_DOMAINS` at [`tracey.py:16`](../../.claude/lib/onibus/tracey.py).

## Files

```json files
[
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T1: delete dead mkBuildHelper :540-561 + doc-comment :26; T2: add mkBuildHelperV2"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T3: replace def build :335-351 with mkBuildHelperV2 splice; AMENDED: migrate P0206's inline tenant build :1261-1276 to build(store_url='ssh-ng://k3s-server-tenant')"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T3: replace def build :101-115 with mkBuildHelperV2 splice; ALSO migrate inline client.fail at :720-725 (P0215 max-silent-time subtest) to build(..., expect_fail=True)"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T3: replace build_chain :75-87 + callsite :100 with mkBuildHelperV2"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T3: replace def build :207-221 with mkBuildHelperV2 + timeout_wrap=90; DROP --timeout 60 --max-silent-time 60 (ssh-ng no-ops per P0215, fod-proxy.nix:200 comment)"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T4 AMENDED: build_drv :282-303 → thin wrapper over build(store_url=f'...?ssh-key={identity_file}'); splice mkBuildHelperV2 above; -18 lines"}
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
    └── security.nix              # T4 AMENDED: build_drv → thin wrapper
```

## Dependencies

```json deps
{"deps": [206, 215, 216, 243], "soft_deps": [313, 304, 207], "note": "deps=[206,215,216,243] are REBASE-PAIN AVOIDANCE, not semantic — all four have active worktrees touching scenario files (p206:lifecycle, p215:lifecycle+scheduling, p216:cli+lifecycle, p243:adds fod-proxy.nix with the 5th build()). Scheduling after their merge means one clean refactor instead of four rebases. AMENDED docs-926870: P0206 dep is now SEMANTIC too — T3 migrates lifecycle.nix:1266 (P0206's tenant-key inline call). soft_dep P0207: its T5 adds a 3rd tenant-alias build — if P0207 lands first, it copies lifecycle.nix:1266; if this lands first, P0207 T5 uses build(store_url=). Ship-order preference: THIS before P0207 (P0207 is UNIMPL; coordinator can sequence). Soft: P0313 (kvmCheck) also touches scenario preludes (one-line prepend, non-overlapping section). P0304 T9 extracts submit_build_grpc in lifecycle.nix — also non-overlapping."}
```

**Depends on:** [P0206](plan-0206-path-tenants-migration-upsert-completion.md), [P0215](plan-0215-worker-max-silent-time.md), [P0216](plan-0216-rio-cli-subcommands.md), [P0243](plan-0243-vm-fod-proxy-scenario.md) — all UNIMPL with active worktrees. This plan is a leaf behind all four.

**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=14 (hottest scenario file). The deps list IS the conflict-avoidance: by the time this dispatches, p206/p215/p216 have merged and their line-refs are stable. [P0313](plan-0313-kvm-fast-fail-preamble.md) touches the **prelude top** (before `start_all()`); this plan touches **inside the prelude** (`build()` helper at lifecycle:335+). Different sections, trivial merge.

**Payoff (AMENDED):** 5 divergent copies + 2 inline duplicates + 1 exception → 1 definition + 5 splices + 1 thin wrapper. The 6th scenario file doesn't grow a 6th copy. P0207 T5 doesn't grow a 3rd tenant-alias inline.
