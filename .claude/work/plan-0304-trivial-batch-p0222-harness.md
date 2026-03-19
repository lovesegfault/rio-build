# Plan 0304: Trivial-hardening batch — P0222 dashboard nits + harness regex

Four trivial items from the [P0222](plan-0222-grafana-dashboards.md) review sink plus one coordinator-surfaced harness regex gap. No open trivial batch existed; this is a fresh one. All four are sub-10-line edits; the configmap regen target is the "biggest" at ~20 lines of justfile.

T1 is **not** from P0222 — the coordinator surfaced it during the stage1 backfill merge when [P0075](plan-0075-cargo-deny.md)'s plan doc couldn't put `deny.toml` in its `json files` fence (regex rejected it). The original file:line ref (`state.py:389`) went stale in the onibus refactor ([`b2980679`](https://github.com/search?q=b2980679&type=commits)); re-located to [`models.py:322`](../../.claude/lib/onibus/models.py).

## Entry criteria

- [P0222](plan-0222-grafana-dashboards.md) merged (dashboard JSONs exist for T2-T4)
- [P0223](plan-0223-seccomp-localhost-profile.md) merged (seccomp CEL rules exist for T11; seccomp-rio-worker.json exists for T12)

## Tasks

### T1 — `fix(harness):` extend PlanFile.path regex for root-level files

MODIFY [`.claude/lib/onibus/models.py`](../../.claude/lib/onibus/models.py) at `:322`.

Current pattern:
```python
pattern=r"^(rio-[a-z-]+/|nix/|docs/|infra/|migrations/|scripts/|flake\.nix|\.claude/|Cargo|justfile|\.config/|codecov\.yml)"
```

Missing (10 file entries dropped during stage1 backfill merge):
- `.github/` — workflow files (e.g., dependabot config)
- `deny.toml` — P0075's cargo-deny config couldn't be referenced
- `flake.lock` — flake input bumps are plan-doc-worthy
- `.envrc` — direnv hook edits
- root `*.md` — `README.md`, `CLAUDE.md`, `CONTRIBUTING.md`. [P0295](plan-0295-doc-rot-batch-sweep.md) T1 works around this TODAY with a "Root-level file (outside Files-fence validator pattern)" prose escape hatch at line 104

Extended pattern:
```python
pattern=r"^(rio-[a-z-]+/|nix/|docs/|infra/|migrations/|scripts/|flake\.nix|flake\.lock|\.claude/|Cargo|justfile|\.config/|\.github/|\.envrc|codecov\.yml|deny\.toml|[A-Z]+\.md$)"
```

The `[A-Z]+\.md$` anchor catches `README.md`, `CLAUDE.md`, `CONTRIBUTING.md`, `LICENSE.md` — root-level all-caps-stem markdown. Anchored with `$` so it doesn't match `README.md.backup` or accidentally permit `/README.md` paths.

Also update the comment at [`models.py:320`](../../.claude/lib/onibus/models.py) — the "`- deny.toml`" in the rix-delta note is now wrong.

**Also** update [`.claude/lib/plan-doc-skeleton.md:69-70`](../lib/plan-doc-skeleton.md) — the prose list of valid prefixes is documentation for plan authors and should match. (Currently says "or `codecov.yml`"; extend with the new entries.)

**Post-fix:** [P0295](plan-0295-doc-rot-batch-sweep.md) can move its README.md entry into the fence. Leave a `TODO(P0295)` comment in P0295's escape-hatch prose pointing here — or just fix it inline if P0295 is still UNIMPL when this lands (it is as of this writing).

### T2 — `fix(infra):` wrap scheduler counter rates in sum()

MODIFY [`infra/helm/grafana/scheduler.json`](../../infra/helm/grafana/scheduler.json) at `:96`, `:124`, `:151`.

With `scheduler.replicas: 2`, both pods export the counter. The standby's dispatch loop no-ops (`r[sched.lease.k8s-lease]`) so its counter stays at zero — but Prometheus still scrapes it. A bare `rate(...)` query returns two series. The panel renders two lines with identical `legendFormat`: one real, one flat-zero. Wrap in `sum(rate(...))`.

| Line | Before | After |
|---|---|---|
| 96 | `rate(rio_scheduler_backstop_timeouts_total[5m])` | `sum(rate(rio_scheduler_backstop_timeouts_total[5m]))` |
| 124 | `rate(rio_scheduler_cancel_signals_total[5m])` | `sum(rate(rio_scheduler_cancel_signals_total[5m]))` |
| 151 | `rate(rio_scheduler_misclassifications_total[5m])` | `sum(rate(rio_scheduler_misclassifications_total[5m]))` |

Note `:178` already does `sum by (assigned_class, actual_class) (rate(...))` correctly — no change there. `:32`, `:37` are `histogram_quantile(..., sum by (le) (rate(...)))` — also already correct.

**After editing the JSON, regen the configmap** — `just grafana-configmap` (from T3) or manual re-inline. If T3 doesn't land in the same commit, re-inline manually.

### T3 — `feat(infra):` justfile target for configmap regen

[`configmap.yaml`](../../infra/helm/grafana/configmap.yaml) is 1241 lines of hand-inlined JSON duplicating the four source `.json` files. Parity verified at review time (semantic diff clean). No regen mechanism exists — first dashboard edit will drift unless someone remembers to re-inline.

MODIFY [`justfile`](../../justfile) — add a regen target:

```makefile
# Regenerate the Grafana dashboard ConfigMap from source JSONs.
# The configmap is a kubectl-applyable bundle for the Grafana sidecar
# (grafana_dashboard: "1" label). Source JSONs are the edit surface;
# configmap.yaml is derived. Run this after any dashboard edit.
grafana-configmap:
    #!/usr/bin/env bash
    set -euo pipefail
    cd infra/helm/grafana
    {
      echo "# GENERATED by 'just grafana-configmap' — edit the .json files, not this"
      echo "apiVersion: v1"
      echo "kind: ConfigMap"
      echo "metadata:"
      echo "  name: rio-grafana-dashboards"
      echo "  labels:"
      echo "    grafana_dashboard: \"1\""
      echo "data:"
      for f in build-overview.json worker-utilization.json store-health.json scheduler.json; do
        echo "  $f: |"
        sed 's/^/    /' "$f"
      done
    } > configmap.yaml
    echo "Regenerated configmap.yaml ($(wc -l < configmap.yaml) lines)"
```

MODIFY [`infra/helm/grafana/configmap.yaml`](../../infra/helm/grafana/configmap.yaml) — regen it once with the new target so the `# GENERATED` header is present and the file matches byte-for-byte what the target produces. This also picks up T2+T4's edits.

**Alternative considered and rejected:** kustomize `configMapGenerator` with `files:` — cleaner in theory, but the repo has no kustomization.yaml today and adding kustomize to the dependency closure for one configmap is scope creep. The justfile target is zero new deps.

### T4 — `refactor(infra):` drop no-op label matchers

MODIFY [`infra/helm/grafana/worker-utilization.json`](../../infra/helm/grafana/worker-utilization.json) at `:32`, `:59`.

`{pool=~".+"}` at line 32 and `{class=~".+"}` at line 59 are no-ops:
- [`rio-controller/src/scaling.rs:248,251`](../../rio-controller/src/scaling.rs) always sets `pool` (both `kind=actual` and `kind=desired` paths)
- [`rio-scheduler/src/actor/dispatch.rs:150,154`](../../rio-scheduler/src/actor/dispatch.rs) always sets `class` (both the zero-out loop and the count loop)

The matcher's only effect is to exclude series where the label is the empty string — but neither metric is ever emitted with an empty label. Drop the matcher; keep `legendFormat: "{{pool}} ({{kind}})"` and `"{{class}}"`.

| Line | Before | After |
|---|---|---|
| 32 | `rio_controller_workerpool_replicas{pool=~".+"}` | `rio_controller_workerpool_replicas` |
| 59 | `rio_scheduler_class_queue_depth{class=~".+"}` | `rio_scheduler_class_queue_depth` |

**After editing, regen the configmap** (T3).

### T5 — `refactor(cli):` smoke.rs stale RIO_CONFIG_PATH comment

MODIFY [`rio-cli/tests/smoke.rs`](../../rio-cli/tests/smoke.rs) at `:44-47` (p216 worktree — verify post-P0216-merge). Comment says "RIO_CONFIG_PATH pointed at /dev/null: `config::load` tries to..." but the code never sets `RIO_CONFIG_PATH`. Either the env-set was removed and the comment wasn't, or it was never added. Delete the comment block (4 lines) OR add the missing `.env("RIO_CONFIG_PATH", "/dev/null")` — check which the test actually needs by reading `config::load` behavior.

### T6 — `refactor(workspace):` clippy.toml OsStr/Path to_string_lossy siblings

MODIFY [`clippy.toml`](../../clippy.toml) (will exist post-P0290-merge — p290 worktree has it at root). P0290 disallowed `String::from_utf8_lossy` ([`clippy.toml:11`](../../clippy.toml), p290). The `OsStr::to_string_lossy` / `Path::to_string_lossy` siblings have the same silent-U+FFFD problem on lookup paths. Two production uses found:

- [`rio-worker/src/fuse/ops.rs:162`](../../rio-worker/src/fuse/ops.rs) — `let name_str = name.to_string_lossy()` on a FUSE lookup name (OsStr). Non-UTF-8 filenames inside NARs are legal; lossy here produces wrong paths.
- [`rio-worker/src/upload.rs:91`](../../rio-worker/src/upload.rs) — `entry.file_name().to_string_lossy()` on a DirEntry. Store paths should be UTF-8 (nix enforces this) but lossy would hide a violation.

Add two entries to `disallowed-methods`:
```toml
{ path = "std::ffi::OsStr::to_string_lossy", reason = "silently produces U+FFFD; use .to_str().ok_or(...) for parse paths (P0290)" },
{ path = "std::path::Path::to_string_lossy", reason = "silently produces U+FFFD; use .to_str().ok_or(...) for parse paths (P0290)" },
```

**Then fix the 2 production callsites.** `fuse/ops.rs:162` probably wants `name.to_str().ok_or(libc::EINVAL)?` (FUSE error code for invalid-name). `upload.rs:91` wants `entry.file_name().to_str().ok_or(UploadError::NonUtf8Path)?` — may need a new error variant. Any `#[allow(clippy::disallowed_methods)]` in test code / log-display paths stays.

**Check `ops.rs:203`** — same pattern, second callsite in the same file. Fix or allow per the same logic.

### T7 — `docs:` P0279/P0280 "seeded by P0284" → P0245

MODIFY [`.claude/work/plan-0279-dashboard-streaming-log-viewer.md`](plan-0279-dashboard-streaming-log-viewer.md) at `:112` and [`.claude/work/plan-0280-dashboard-dag-viz-xyflow.md`](plan-0280-dashboard-dag-viz-xyflow.md) at `:132` — both say "(seeded by P0284)". The `r[dash.*]` markers were seeded by **P0245** (confirmed: commit [`ea818938`](https://github.com/search?q=ea818938&type=commits) "docs(spec): seed 4 r[dash.*] markers + register dash domain" is part of P0245's work, and [`tracey.py:5`](../../.claude/lib/onibus/tracey.py) docstring says "`dash` seeded by P0245 (pulled forward from P0284...)"). P0284 was the original plan; the work was pulled forward. Change both to "(seeded by P0245)".

### T8 — `refactor(controller):` delete dead SchedulerUnavailable variant

MODIFY [`rio-controller/src/error.rs`](../../rio-controller/src/error.rs) at `:37` and `:55`. `Error::SchedulerUnavailable(#[from] tonic::Status)` is dead — P0294 removes all controller→scheduler gRPC calls (the Build reconciler was the only caller). Clippy misses it because `#[from]` generates a `From<tonic::Status>` impl that keeps the variant "in use" even if nothing ever constructs it.

Delete both lines. If a `tonic::Status` import is only used by this variant, delete that too. `cargo clippy --all-targets -- --deny warnings` will catch any remaining callers (there should be none post-P0294).

### T9 — `refactor(nix):` lifecycle.nix extract submit_build_grpc

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) at `:502-531` (sprint-1 line numbers — this is the Build-CRD `kubectl apply` block that P0294 deletes/rewrites). **P0294 rewrites this entire subtest** to use gRPC SubmitBuild directly. When P0294 rewrites, it'll inline ~30 lines of grpcurl + assertion boilerplate. [P0289](plan-0289-port-specd-unlanded-test-trio.md) will need the same boilerplate for its ported tests.

**Extract before the copy-paste happens:** Define a `submit_build_grpc(drv_path, priority=50)` Python helper in the fixture's common module (find where `common.busybox` comes from — likely `nix/tests/common.nix` or similar). The helper does: `nix-instantiate` → `nix copy --derivation` → `grpcurl SubmitBuild` → return `build_id`. Both P0294's rewrite and P0289's ports call it.

**Timing:** This MUST land before P0289 dispatches (so P0289 uses the helper). If P0294 has already rewritten the subtest with inline grpcurl, extract from P0294's version instead.

### T10 — `fix(harness):` onibus build excusable() — TCG/exit-143 pattern

MODIFY [`.claude/lib/onibus/build.py`](../../.claude/lib/onibus/build.py) at `:129-155`.

Current `excusable()` only matches nextest `FAIL [Ns]` lines via `_NEXTEST_FAIL_RE` at `:132`. When nixbuild.net allocates a TCG builder, the VM test fails with `failed to initialize kvm` and exit 143 — **no nextest FAIL line exists** (VM tests aren't nextest). The function reports `"no FAIL lines in log (not a test failure?)"` at `:145` and returns `ok=False`. fix-p209 hit this three times.

> **Sequenced after [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md):** P0317 T1 adds `_VM_FAIL_RE` which populates `failing` with VM drv names. Without that, this T10's early-return design masks co-occurring real failures (nextest FAIL + VM TCG in same `.#ci` run → 2 failures, but early-return grants excusability on marker alone). **Re-scope `_TCG_MARKERS` as a supplementary grant, not an early-return:** check it AFTER `failing` is computed, inside the `elif not matched:` branch — if `len(failing) == 1 and failing[0] in vm_fails and any(m in text for m in _TCG_MARKERS)`, override to `ok=True` with reason `"TCG marker present — builder-side infra, retry"`. This preserves the 1-failure-exactly discipline while granting the infra-always-excusable property.

Add a pre-check for the TCG signature before the nextest-FAIL path:

```python
# Add after _NEXTEST_FAIL_RE at :132 — TCG signature patterns.
# These are BUILDER-SIDE infra failures, not test-code bugs. When
# P0313 lands, the marker changes to 'KVM-DENIED-BUILDER'
# (exit 77, ~10s) — match BOTH for transition period.
_TCG_MARKERS = (
    # TODO(P0316): this marker now covers TWO failure modes with the
    # same string. Pre-P0313: qemu tried kvm in `-machine accel=kvm:tcg`,
    # printed this as a WARNING, fell back to TCG silently (slow death →
    # exit-143 timeout). Post-P0316 (`-accel kvm` in every node's
    # virtualisation.qemu.options, common.nix:196): qemu tries kvm ONLY,
    # prints this as a FATAL error, exits non-zero immediately — per-VM
    # hard-fail for the concurrent-VM race (kvmCheck single-shot probe
    # passes, then 2/3 QEMU children race-lose on their own CREATE_VM).
    # QEMU format string (verified in qemu-10.2.1 binary): "failed to
    # initialize %s: %s" → lowercase "kvm" + strerror. Matching just the
    # prefix is correct for both modes.
    "failed to initialize kvm",    # qemu: TCG-fallback warning (pre-P0313) OR -accel kvm hard-fail (post-P0316)
    "KVM-DENIED-BUILDER",          # kvmCheck fast-fail exit-77 (P0313/P0315 preamble)
)

def excusable(log_path: Path) -> ExcusableVerdict:
    text = log_path.read_text()

    # TCG allocation: builder-side infra, always excusable (retry
    # hits a different builder). Check BEFORE nextest-FAIL parse —
    # TCG failures have no FAIL line.
    for marker in _TCG_MARKERS:
        if marker in text:
            return ExcusableVerdict(
                excusable=True, failing_tests=[],
                matched_flakes=["<builder-kvm-denied>"],
                reason=f"TCG builder allocation ({marker!r} in log) — retry on different builder",
            )

    failing = sorted(set(_NEXTEST_FAIL_RE.findall(text)))
    # ... rest unchanged
```

**Verify at dispatch** whether `ExcusableVerdict.matched_flakes` expects `KnownFlake` instances or strings — adjust the `"<builder-kvm-denied>"` sentinel accordingly. Check [`models.py`](../../.claude/lib/onibus/models.py) `ExcusableVerdict` definition.

### T11 — `fix(controller):` CEL validation .message() for seccomp rules

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — P0223 line refs (p223 worktree; re-grep post-P0223-merge).

The five `#[x_kube(validation)]` rules have no `message:` field. K8s default error is `failed rule: {Rule}` — the raw CEL expression echoed back. For simple rules like `self >= 1` at `:75` that's readable. For the seccomp coupling ternary at `:292`:

```
failed rule: self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)
```

A user who sets `{type: RuntimeDefault, localhostProfile: foo}` gets that raw CEL back — opaque. [`kube-core`](https://docs.rs/kube-core) supports `message` in the validation attr. Sweep all five for consistency; the two seccomp rules at p223 `:291-292` are the motivating cases:

```rust
// Before (p223 :291-292):
#[x_kube(
    validation = "self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']",
    validation = "self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"
)]

// After:
#[x_kube(
    validation = Rule::new("self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']")
        .message("seccompProfile.type must be one of: RuntimeDefault, Localhost, Unconfined"),
    validation = Rule::new("self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)")
        .message("seccompProfile.localhostProfile is required when type=Localhost, forbidden otherwise"),
)]
```

**Check at dispatch:** the `#[x_kube(validation = ...)]` attr may use string-literal syntax, not `Rule::new()` — grep the `kube-derive` docs or existing usages. If `Rule::new()` isn't the right shape, the message may be a separate attr key: `#[x_kube(validation = "...", message = "...")]`. The `kube-core` `Rule` struct at [docs.rs/kube-core](https://docs.rs/kube-core/latest/kube_core/validation/struct.Rule.html) has a `message` field; the derive-macro syntax to set it varies.

**The three pre-existing rules** at `:75` (`self >= 1`), `:121` (`size(self) > 0`), `:331` (`self.min <= self.max`) — simple enough that the echoed CEL is readable. Add messages anyway for consistency (low cost, better UX), OR leave them (less churn). Decide at impl; the two seccomp rules are the load-bearing fix.

**After editing:** regen the CRD YAML (`.#crds` — see T13 for the `--link` mechanism).

### T12 — `feat(infra):` seccomp regen-diff script for moby upstream drift

NEW [`scripts/seccomp-regen-diff.sh`](../../scripts/seccomp-regen-diff.sh). [`seccomp-rio-worker.json`](../../infra/helm/rio-build/files/seccomp-rio-worker.json) was hand-derived from [moby `default.json` v27.5.1](https://github.com/moby/moby/blob/v27.5.1/profiles/seccomp/default.json). Provenance is in `//`-keys + commit msg ([`c94c93ff`](https://github.com/search?q=c94c93ff&type=commits)). When moby v28 adds new syscalls, the allowlist silently lags.

The existing `seccomp_profile_json_is_valid` test (P0223) guards **invariants** (allowlist structure, the 5 denied syscalls absent, worker-required syscalls present) but not **upstream drift** (moby added a new safe syscall; our profile doesn't have it; builds that need it fail with `EPERM`).

```bash
#!/usr/bin/env bash
# Regenerate seccomp-rio-worker.json from moby default.json and diff
# against the checked-in version. Run manually when bumping moby tag.
#
# Derivation: moby's default.json has conditional blocks keyed on caps
# (CAP_SYS_ADMIN gets mount/umount2/setns, CAP_SYS_CHROOT gets chroot).
# Flatten for the caps the worker HAS, then remove the 5 we explicitly
# deny (ptrace, bpf, setns, process_vm_readv, process_vm_writev per
# security.md r[worker.seccomp.localhost-profile]).
set -euo pipefail
MOBY_TAG="${1:-v27.5.1}"
OURS=infra/helm/rio-build/files/seccomp-rio-worker.json
DENIED=(ptrace bpf setns process_vm_readv process_vm_writev)

tmp=$(mktemp)
curl -sfL "https://raw.githubusercontent.com/moby/moby/${MOBY_TAG}/profiles/seccomp/default.json" |
  jq --argjson caps '["CAP_SYS_ADMIN","CAP_SYS_CHROOT"]' '
    # Flatten: default block + blocks whose .includes.caps ⊆ our caps.
    # Moby format: .syscalls is an array of {names:[], action:, includes:{caps:[]}}.
    .syscalls |= map(select(
      (.includes.caps // []) | all(. as $c | $caps | index($c))
    ))
  ' |
  jq --argjson denied "$(printf '%s\n' "${DENIED[@]}" | jq -R . | jq -s .)" '
    # Remove denied syscalls from every .names array.
    .syscalls |= map(.names -= $denied)
  ' > "$tmp"

diff -u "$OURS" "$tmp" || {
  echo "DRIFT: moby ${MOBY_TAG} default.json differs from checked-in profile"
  echo "Review the diff. If moby added safe syscalls, update $OURS."
  echo "If moby removed syscalls, check whether worker builds need them."
  exit 1
}
echo "No drift vs moby ${MOBY_TAG}"
```

**NOT in CI** — network-dependent, and moby bumps are rare. Add a note in the profile's `//`-comment header pointing at this script. Run manually on moby tag bumps (dependabot or periodic check).

**The jq flattening is approximate** — moby's format is complex (architecture-specific blocks, minKernel conditionals). The script produces a **diff for human review**, not a mechanical overwrite. If the diff is non-trivial, a human reads both files.

### T13 — `feat(harness):` onibus build --link for non-check outputs

MODIFY [`.claude/lib/onibus/build.py`](../../.claude/lib/onibus/build.py) at `:38-115` (the `run()` function).

**Stale file:line in the followup:** `.claude/bin/nix-build-remote` does not exist — superseded per [`build.py:73`](../../.claude/lib/onibus/build.py) ("Supersedes the nix-build-remote wrapper"). The real gap: `onibus build --copy` does `nix copy --from ssh-ng://nxb-dev` at `:99-102`, putting the output in the **local /nix/store**, but creates no `result/` symlink (`--no-link` at `:77` is deliberate — a symlink would dangle while the output is remote-only). The `.#crds` regen workflow expects `result/workerpool.yaml` for subsequent `cp result/*.yaml infra/helm/...`.

Add `link: bool = False` param:

```python
def run(
    target: str, *, role: BuildRole = "impl", copy: bool = False,
    link: bool = False, loud: bool = False,
) -> BuildReport:
    # ... existing build logic ...

    store_path = None
    if rc == 0 and copy and out_paths:
        cp = subprocess.run(
            ["nix", "copy", "--no-check-sigs", "--from", "ssh-ng://nxb-dev", *out_paths],
            cwd=toplevel, capture_output=True, text=True,
        )
        if cp.returncode != 0:
            rc = cp.returncode
            # ... existing error handling ...
        else:
            store_path = out_paths[0]
            # --link: create ./result → /nix/store/<hash>-<name>
            # after --copy so the target exists locally. This is what
            # plain `nix build` does by default; we suppress it with
            # --no-link because the output is USUALLY remote-only.
            # Needed for .#crds, .#coverage-html — outputs the caller
            # cp's or opens, not just checks for existence.
            if link:
                result = Path(toplevel) / "result"
                result.unlink(missing_ok=True)
                result.symlink_to(store_path)
```

MODIFY [`.claude/skills/nixbuild/SKILL.md`](../../.claude/skills/nixbuild/SKILL.md) — add `--link` to the invocation table at `:8-14`:

```
.claude/bin/onibus build <target> --copy --link      # pull output + ./result symlink (for .#crds, .#coverage-html)
```

And a note after `:36`: "With `--link` (implies `--copy`), also creates `./result` → local store path. Use for `.#crds` regen (`cp result/*.yaml infra/helm/...`) and `.#coverage-html` (`open result/index.html`). Without `--link`, `store_path` in the JSON is still the handle — `ln -sf $(jq -r .store_path) result` is the manual equivalent."

**CLI plumbing:** find where `onibus build` argparse lives (likely `onibus/__main__.py` or `cli.py`) and add `--link` flag. Validate: `--link` without `--copy` should error (can't symlink to a remote-only path) — or make `--link` imply `--copy`.

### T14 — `refactor(nix):` extract bounceGatewayForSecret — scale-bounce duplication

The scale-0 → wait-deleted → scale-1 → rollout-status sequence is duplicated:

- [`nix/tests/fixtures/k3s-full.nix:486-524`](../../nix/tests/fixtures/k3s-full.nix) — original, inside the monolithic `sshKeySetup` block (`:451-516`). Has the long explanatory comment about kubelet's SecretManager watch-mode cache + reflector refcount.
- [`nix/tests/scenarios/lifecycle.nix:1204-1227`](../../nix/tests/scenarios/lifecycle.nix) **(p206 worktree — post-P0206-merge line ref)** — byte-for-byte copy, ~25 lines.

[P0207](plan-0207-tenant-key-build-gc-mark.md) will likely need a third copy (tenant-key build for the GC mark VM test — same pattern: update Secret → gateway must see the fresh key).

**Extract as `bounceGatewayForSecret` helper in k3s-full.nix:** a Python-string attr alongside `waitReady`/`sshKeySetup` that scenarios interpolate via `${fixture.bounceGatewayForSecret}`. The long SecretManager comment stays with the helper definition (single source of truth).

MODIFY [`nix/tests/fixtures/k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix):

```nix
  # Scale gateway to 0 → wait for full pod deletion → scale to 1.
  # Necessary after updating the rio-gateway-ssh Secret: gateway reads
  # authorized_keys once at startup (Arc<Vec<PublicKey>>, no hot-reload),
  # and `rollout restart` is NOT sufficient — see the SecretManager
  # watch-cache explanation below. The scale-to-zero forces the reflector
  # refcount to hit 0 → cache evict → fresh LIST on the new pod.
  bounceGatewayForSecret = ''
    # [move :486-507 comment here, then the 4 k3s_server calls :508-524]
    k3s_server.succeed("k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=0")
    k3s_server.wait_until_succeeds(
        "! k3s kubectl -n ${ns} get pods "
        "-l app.kubernetes.io/name=rio-gateway --no-headers 2>/dev/null | grep -q .",
        timeout=90,
    )
    k3s_server.succeed("k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=1")
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} rollout status deploy/rio-gateway --timeout=60s",
        timeout=90,
    )
  '';
```

Then: `sshKeySetup` at `:486-524` becomes `${self.bounceGatewayForSecret}` (or equivalent fixture-internal reference); lifecycle.nix at `:1204-1227` (post-P0206) becomes `${fixture.bounceGatewayForSecret}`.

**Timing:** depends on P0206 merge (the lifecycle.nix copy doesn't exist on sprint-1 yet). If this lands before P0206: extract in k3s-full.nix + use in sshKeySetup; leave a `TODO(P0207)` pointer; P0206's reviewer will point at the helper instead of copy-pasting.

### T15 — `refactor(nix):` drop vestigial GET_API_VERSION from kvmCheck

MODIFY [`nix/tests/common.nix`](../../nix/tests/common.nix) at `:144-175` (`kvmCheck` body).

The `KVM_GET_API_VERSION` ioctl at `:159` is vestigial — [`common.nix:156-158`](../../nix/tests/common.nix) already says so ("empirically passes on the 3/7 ioctl-gap builders — the real gate is CREATE_VM"). The P0315 pivot itself proved GET_API_VERSION gates nothing (kernel handler is `case KVM_GET_API_VERSION: r = KVM_API_VERSION;` — no perm check; [`common.nix:121-123`](../../nix/tests/common.nix)).

The `locals().get('_kvm_ver', '?')` hack at `:167` defends against GET_API_VERSION raising before assigning `_kvm_ver` — but the shared `except OSError` block at `:163-172` unconditionally says `"ioctl(KVM_CREATE_VM) failed"` even when GET_API_VERSION was the raise site. The discriminator carries correct errno inside wrong framing.

**Collapse to open→CREATE_VM only:**

```nix
kvmCheck = ''
  import os, sys, fcntl
  _KVM_CREATE_VM = 0xAE01  # _IO(KVMIO, 0x01); KVMIO=0xAE. Returns VM fd.
  try:
      _kvm_fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
  except OSError as _kvm_err:
      print(
          f"KVM-DENIED-BUILDER: cannot open /dev/kvm O_RDWR ({_kvm_err.strerror}) — "
          "builder allocated without KVM; TCG would 10-20x the runtime",
          file=sys.stderr, flush=True,
      )
      sys.exit(77)
  try:
      # KVM_CREATE_VM: the actual permission gate (kvm_dev_ioctl_create_vm
      # is where the LSM check lives; GET_API_VERSION has none — returns
      # a constant). arg=0 → default VM type. Returns a VM fd; close()
      # triggers kvm_put_kvm() teardown. Microseconds of lifetime, same
      # probe QEMU runs seconds later anyway.
      _vm_fd = fcntl.ioctl(_kvm_fd, _KVM_CREATE_VM, 0)
  except OSError as _kvm_err:
      os.close(_kvm_fd)
      print(
          f"KVM-DENIED-BUILDER: /dev/kvm opened O_RDWR but "
          f"ioctl(KVM_CREATE_VM) failed ({_kvm_err.strerror}) — "
          "LSM/userns gate on VM creation",
          file=sys.stderr, flush=True,
      )
      sys.exit(77)
  os.close(_vm_fd)
  os.close(_kvm_fd)
'';
```

**Deletes:** `_KVM_GET_API_VERSION` const, the GET_API_VERSION call, the `locals().get()` hack, `_kvm_ver` interpolation. **Moves:** the 4/7 and 3/7 empirical ratios from the f-string at `:169` into the comment block above `kvmCheck` (around `:114-126` where the CREATE_VM rationale already lives) — runtime stderr will outlive the merge-37/38 snapshot the ratios reference. The comment block is the archaeology surface; the stderr message is the operational surface.

**[P0317](plan-317-excusable-vm-regex-knownflake-schema.md) T7 touches `:130-132`** (the false `pattern-match` claim, ~10 lines above) — non-overlapping with `:144-175` here, but same file. Either order works.

### T16 — `fix(harness):` QMP ConnectionResetError = P0316 downstream — alt marker signature

MODIFY [`.claude/known-flakes.jsonl`](../../.claude/known-flakes.jsonl) at `:11` (the `<tcg-builder-allocation>` / `vm-lifecycle-recovery-k3s` TCG row — check post-P0317-T3 name).

When QEMU hard-fails via `-machine accel=kvm` ([P0316](plan-0316-qemu-force-accel-kvm.md)), QEMU **exits** → the QMP socket closes → the NixOS test driver gets `ConnectionResetError: [Errno 104] Connection reset by peer`. Observed in 7 logs (`merge-52/56/58/60/64`). This is **not a new failure mode** — it's the P0316 gate's output viewed from the Python-test-driver side instead of the QEMU-serial-log side. Same root cause, different observation point.

The `symptom` field already has `failed to initialize kvm: Permission denied` (QEMU's message). Add the QMP-side signature as an alternate in the same field — `symptom` is human-facing per [`known-flakes.jsonl:4`](../../.claude/known-flakes.jsonl) header, so a pipe-separated alternation is fine for grep-by-human:

```
"symptom": "falling back to tcg OR failed to initialize kvm: Permission denied OR ConnectionResetError: [Errno 104] (QMP socket closed — QEMU exited on accel=kvm)"
```

**IF P0317 T4 has migrated this row to `mitigations: list[Mitigation]`** — add a `Mitigation` entry for P0316 that mentions both symptom strings in its `note`. Check at dispatch which schema is live.

### T17 — `fix(harness):` _LEASE_SECS 1800→2700 — TCG cold-cache headroom

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `:57`.

Current `_LEASE_SECS = 30 * 60`. The comment at `:52-56` says "merge itself is ~10min". P0216's merger ran ~30min under TCG (cold-cache rebase-then-CI-revalidate — every VM drv invalidated, every builder TCG). That's exactly the lease. Failure mode is **safe** — `stale=True, ff_landed=False` → coordinator pings human, doesn't auto-steal — but 45min is cheap insurance against a spurious ping.

```python
# Staleness threshold. The merge itself is ~10min typically (rebase + ff
# + .#ci cache-hit re-validate); .#coverage-full is backgrounded so
# doesn't count against lease. 30min was the old value — P0216 under
# TCG (cold-cache, every VM drv rebuilt on broken builders) ran ~30min
# end-to-end. 45min gives headroom. Failure mode is safe (stale=True →
# coordinator pings human, not auto-steal) but spurious pings waste time.
# PID-liveness was the wrong mechanism: the `onibus merge lock`
# subprocess exits immediately after writing (fire-and-forget CLI), so
# os.kill(pid, 0) was always ProcessLookupError → stale=True → POISONED
# on every merge.
_LEASE_SECS = 45 * 60
```

### T18 — `refactor(test-support):` read_path_info wire helper — 6 discard-read sites

NEW function in [`rio-test-support/src/wire.rs`](../../rio-test-support/src/wire.rs) (after `do_handshake`/`send_set_options` at end of file, ~`:175`).

The `wopQueryPathInfo` (opcode 26) response wire format after `valid: bool` is a 8-field sequence. Every test that queries PathInfo reads-and-discards 7 of them to get at the one it cares about. Six current sites (17 new lines from P0305 + 10 pre-existing):

| File:line | Kept field | Discarded |
|---|---|---|
| [`functional/references.rs:53-60`](../../rio-gateway/tests/functional/references.rs) | `refs` (line 55) | deriver, nar_hash, regtime, nar_size, ultimate, sigs, ca |
| [`functional/references.rs:149-156`](../../rio-gateway/tests/functional/references.rs) | `refs` (line 151) | same 7 |
| [`wire_opcodes/opcodes_write.rs:509-516`](../../rio-gateway/tests/wire_opcodes/opcodes_write.rs) | varies | 5+ fields |
| [`wire_opcodes/opcodes_write.rs:558-565`](../../rio-gateway/tests/wire_opcodes/opcodes_write.rs) | varies | 6+ fields |

Plus 2 more in `wire_opcodes/` (grep `let _deriver`). Tranche-2 adds ~22 scenarios, many query PathInfo — would grow to 20+ sites.

Add a struct + reader:

```rust
/// `wopQueryPathInfo` (opcode 26) response fields, wire-order.
/// `r[gw.opcode.query-path-info]` at docs/src/components/gateway.md:251.
///
/// Returned AFTER `valid: bool` — caller reads `valid` first, then
/// calls this if `valid == true`. Tests usually want one field
/// (refs, nar_hash, ca) and discard the rest; this reads them all
/// so callsites don't repeat the 8-field discard sequence.
#[derive(Debug)]
pub struct PathInfoWire {
    pub deriver: String,
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: String,
}

/// Read the `wopQueryPathInfo` response body (everything after `valid: bool`).
/// Caller MUST read `valid` first — this reads deriver onwards.
pub async fn read_path_info(s: &mut DuplexStream) -> anyhow::Result<PathInfoWire> {
    Ok(PathInfoWire {
        deriver: wire::read_string(s).await?,
        nar_hash: wire::read_string(s).await?,
        references: wire::read_strings(s).await?,
        registration_time: wire::read_u64(s).await?,
        nar_size: wire::read_u64(s).await?,
        ultimate: wire::read_bool(s).await?,
        sigs: wire::read_strings(s).await?,
        ca: wire::read_string(s).await?,
    })
}
```

Then migrate the 6 sites. `references.rs:53-60` becomes:

```rust
let info = read_path_info(&mut stack.stream).await?;
assert_eq!(
    info.references,
    vec![path_a.clone()],
    "B's references should be [A] — round-tripped through PG narinfo.\"references\" TEXT[]"
);
```

Also add `pub use wire::{read_path_info, PathInfoWire};` to the `rio-test-support` prelude (find where `do_handshake` is re-exported).

### T19 — `fix(gateway):` tracing_subscriber init in functional + wire_opcodes main.rs

MODIFY [`rio-gateway/tests/functional/main.rs`](../../rio-gateway/tests/functional/main.rs) and [`rio-gateway/tests/wire_opcodes/main.rs`](../../rio-gateway/tests/wire_opcodes/main.rs).

`tracing::debug!` at [`functional/mod.rs:152`](../../rio-gateway/tests/functional/mod.rs) (`run_protocol` error log) and [`common/mod.rs:66`](../../rio-gateway/tests/common/mod.rs) (same message, wire_opcodes side) go to the void — no subscriber. When a functional test fails unexpectedly, zero server-side diagnostics.

`init_test_logging()` exists at [`common/mod.rs:129-134`](../../rio-gateway/tests/common/mod.rs) — **defined, never called**. Uses `try_init()` (idempotent) + `with_test_writer()` (captured per-test, only shown on failure). Correct shape, dead code.

**wire_opcodes/main.rs** — already pulls in `common/mod.rs` via `#[path]`. Add at module scope (after imports):

```rust
// init_test_logging is idempotent (try_init); with_test_writer means
// output is captured per-test, only shown on failure. Without this,
// tracing::debug! at common/mod.rs:66 (run_protocol error log) is void.
#[ctor::ctor]
fn _init_logging() { common::init_test_logging(); }
```

**functional/main.rs** — does NOT pull in `common/mod.rs` (it uses its own `#[path = "mod.rs"] mod fixture` at [`main.rs:26`](../../rio-gateway/tests/functional/main.rs)). Either: (a) also `#[path]`-include `common/mod.rs` and call its `init_test_logging`; (b) inline a 3-line `tracing_subscriber::fmt().with_env_filter(...).with_test_writer().try_init()` in a `#[ctor::ctor]` fn. Option (b) avoids the cross-module include — 3 lines, zero coupling.

**Check `ctor` dep** — if `rio-gateway` dev-deps doesn't have [`ctor`](https://docs.rs/ctor), add it. Alternative: call `init_test_logging()` at the top of each `#[tokio::test]` fn (idempotent, so safe to call N times). `ctor` is cleaner (one call, module-level); per-test calls are zero-new-deps. Implementer's call.

### T20 — `refactor(gateway):` note-only — RioStack `?` paths detach spawned tasks

**Bughunter finding, documented-not-fixed.** At [`functional/mod.rs:125-130`](../../rio-gateway/tests/functional/mod.rs):

```rust
let (store_addr, store_handle) = spawn_grpc_server(router).await;     // :125
let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;  // :127 — ? here
let store_client = rio_proto::client::connect_store(...).await?;      // :129 — ? here
let sched_client = rio_proto::client::connect_scheduler(...).await?;  // :130 — ? here
```

If `:127` or `:129` or `:130` return `Err`, `store_handle` / `sched_handle` **drop** — but `JoinHandle::drop` **detaches**, not aborts. The task keeps running, holding the TCP listener + `db.pool` clone. `TestDb::Drop` at [`pg.rs:393`](../../rio-test-support/src/pg.rs) then tries `DROP DATABASE` while the detached task holds a pool connection. Best-effort per `:399` — likely succeeds with `FORCE`, but the zombie runs until process exit.

**Same shape at [`grpc.rs:898-899`](../../rio-test-support/src/grpc.rs)** — `spawn_mock_store_with_client` does spawn→connect?, handle drops on Err. Pre-existing pattern, not P0305-specific.

**Impact: test-only, rare.** connect-to-just-spawned-127.0.0.1 rarely fails. Process exit reaps. Only bites if mass connect failures exhaust ephemeral ports across many tests (thousands-of-tests scale, not dozens).

**Not fixing now.** If it ever matters: [`scopeguard::guard`](https://docs.rs/scopeguard) around the handle, or an `AbortOnDrop` wrapper (`struct AbortOnDrop(JoinHandle<()>); impl Drop { fn drop(&mut self) { self.0.abort(); } }`). Add a comment at `:127` documenting the known edge + the fix-when-it-matters pattern:

```rust
// `?` on :127-130 detaches store_handle (JoinHandle::drop doesn't abort).
// Test-only + connect-to-127.0.0.1-just-spawned rarely fails + process-exit
// reaps. If mass-connect-failures ever exhaust ephemeral ports:
// scopeguard::guard or AbortOnDrop wrapper. Same pattern at
// rio-test-support/src/grpc.rs:898 (spawn_mock_store_with_client).
let (scheduler, sched_addr, sched_handle) = spawn_mock_scheduler().await?;
```

**[P0318](plan-0318-riostackbuilder-tranche2-axis.md) T1** renames `build` → `build_inner` in this same function — apply this comment to whichever name is live at dispatch.

## Exit criteria

- `/nbr .#ci` green
- `python3 -c "from onibus.models import PlanFile; PlanFile(path='deny.toml'); PlanFile(path='README.md'); PlanFile(path='.github/workflows/ci.yml'); PlanFile(path='flake.lock'); PlanFile(path='.envrc')"` — no ValidationError (all five new patterns accepted)
- `python3 -c "from onibus.models import PlanFile; PlanFile(path='random.md')"` — raises ValidationError (lowercase stem `.md` still rejected)
- `grep 'sum(rate(' infra/helm/grafana/scheduler.json | wc -l` ≥ 3 (T2's three wraps plus the existing `sum by (le)` ones)
- `just grafana-configmap && git diff --exit-code infra/helm/grafana/configmap.yaml` — regen is idempotent (second run produces no diff)
- `grep '=~".+"' infra/helm/grafana/worker-utilization.json` → 0 hits
- `head -1 infra/helm/grafana/configmap.yaml | grep GENERATED` → match
- `grep 'RIO_CONFIG_PATH' rio-cli/tests/smoke.rs` — either 0 hits (comment deleted) or ≥2 hits (comment + `.env()` call both present); NOT 1 hit (T5: comment-only = stale)
- `grep 'OsStr::to_string_lossy\|Path::to_string_lossy' clippy.toml` → ≥2 hits (T6: both disallowed)
- `cargo clippy --all-targets -- --deny warnings` passes — meaning T6's two prod callsites are fixed (clippy now catches them)
- `grep 'seeded by P0284' .claude/work/plan-0279*.md .claude/work/plan-0280*.md` → 0 hits (T7)
- `grep 'SchedulerUnavailable' rio-controller/src/error.rs` → 0 hits (T8)
- `grep 'def submit_build_grpc\|submit_build_grpc(' nix/tests/` → ≥2 hits (T9: helper defined + called)
- `grep 'KVM-DENIED-BUILDER\|failed to initialize kvm' .claude/lib/onibus/build.py` → ≥2 hits (T10: both TCG markers matched)
- `grep -c 'message' rio-controller/src/crds/workerpool.rs` ≥ 2 (T11: at minimum the two seccomp rules have messages; post-P0223-merge)
- `test -x scripts/seccomp-regen-diff.sh` (T12: executable)
- `scripts/seccomp-regen-diff.sh v27.5.1` — exit 0 (T12: no drift vs current pinned tag; proves the flattening logic matches the hand-derivation)
- `grep 'link: bool\|--link' .claude/lib/onibus/build.py .claude/skills/nixbuild/SKILL.md` → ≥3 hits (T13: param + CLI + doc)
- `.claude/bin/onibus build .#crds --copy --link && test -L result && cp result/*.yaml /tmp/` — roundtrip works (T13; manual validation, not CI-gated since .#crds isn't in .#ci)
- `grep -c 'bounceGatewayForSecret' nix/tests/fixtures/k3s-full.nix` → ≥2 (T14: defined + used in sshKeySetup)
- `grep 'SecretManager\|reflector refcount' nix/tests/fixtures/k3s-full.nix` → ≥1 hit (T14: explanatory comment lives with the helper, not duplicated)
- `grep 'GET_API_VERSION\|_kvm_ver\|locals().get' nix/tests/common.nix` → 0 hits in kvmCheck body (T15: vestigial probe + locals hack removed; mentions allowed in comment-block archaeology above)
- `grep -c 'sys.exit(77)' nix/tests/common.nix` → 2 (T15: both exit paths preserved — open-fail + ioctl-fail)
- `grep '4/7\|3/7' nix/tests/common.nix | grep -v '^\s*#'` → 0 hits (T15: empirical ratios moved from f-string to comment)
- `grep 'ConnectionResetError\|Errno 104' .claude/known-flakes.jsonl` → ≥1 hit (T16: QMP-side alt marker in symptom OR mitigation note)
- `grep '_LEASE_SECS = 45' .claude/lib/onibus/merge.py` → 1 hit (T17: bumped)
- `grep 'TCG\|P0216' .claude/lib/onibus/merge.py | head -3` → ≥1 hit in the _LEASE_SECS comment (T17: rationale references the data point)
- `grep 'read_path_info\|PathInfoWire' rio-test-support/src/wire.rs` → ≥2 hits (T18: struct + fn)
- `grep -c 'let _deriver' rio-gateway/tests/functional/references.rs rio-gateway/tests/wire_opcodes/opcodes_write.rs` → 0 (T18: discard-read sites migrated)
- `grep -c 'read_path_info' rio-gateway/tests/functional/references.rs` → ≥2 (T18: both sites use the helper)
- `grep 'init_test_logging\|tracing_subscriber.*try_init' rio-gateway/tests/functional/main.rs rio-gateway/tests/wire_opcodes/main.rs` → ≥2 hits (T19: both test binaries init a subscriber)
- `grep 'detach\|AbortOnDrop\|scopeguard' rio-gateway/tests/functional/mod.rs` → ≥1 hit (T20: comment documenting the known edge)

## Tracey

No new markers. T2 implicitly serves `r[obs.metric.scheduler]` (the queries reference spec'd metrics) but adds no `r[impl]`/`r[verify]` annotations — dashboard JSON is not annotated. T11 serves `r[worker.seccomp.localhost-profile]` (the CEL rules guard the spec'd Localhost coupling) but the fix is UX (error messages), not behavior — no annotation change.

## Files

```json files
[
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T1: extend PlanFile.path regex :322 for .github/ deny.toml flake.lock .envrc root-*.md"},
  {"path": ".claude/lib/plan-doc-skeleton.md", "action": "MODIFY", "note": "T1: sync valid-prefix prose list :69-70 with new regex"},
  {"path": "infra/helm/grafana/scheduler.json", "action": "MODIFY", "note": "T2: wrap rate() in sum() at :96 :124 :151"},
  {"path": "justfile", "action": "MODIFY", "note": "T3: new grafana-configmap regen target"},
  {"path": "infra/helm/grafana/configmap.yaml", "action": "MODIFY", "note": "T3: regen once with GENERATED header (picks up T2+T4 edits)"},
  {"path": "infra/helm/grafana/worker-utilization.json", "action": "MODIFY", "note": "T4: drop no-op {pool=~.+} :32 and {class=~.+} :59"},
  {"path": "rio-cli/tests/smoke.rs", "action": "MODIFY", "note": "T5: delete stale RIO_CONFIG_PATH comment :44-47 OR add missing .env() (p216 worktree ref)"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "T6: to_string_lossy → to_str().ok_or at :162 :203"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T6: to_string_lossy → to_str().ok_or at :91"},
  {"path": ".claude/work/plan-0279-dashboard-streaming-log-viewer.md", "action": "MODIFY", "note": "T7: seeded by P0284 → P0245 at :112"},
  {"path": ".claude/work/plan-0280-dashboard-dag-viz-xyflow.md", "action": "MODIFY", "note": "T7: seeded by P0284 → P0245 at :132"},
  {"path": "rio-controller/src/error.rs", "action": "MODIFY", "note": "T8: delete dead SchedulerUnavailable at :37 :55"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T9: extract submit_build_grpc helper (coordinate with P0294 rewrite)"},
  {"path": ".claude/lib/onibus/build.py", "action": "MODIFY", "note": "T10: _TCG_MARKERS + early-return in excusable() :129-155; T13: link= param in run() :38-115 + result symlink after --copy"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T11: .message() on seccomp CEL rules (p223 :291-292; sweep all 5 for consistency) — post-P0223-merge"},
  {"path": "scripts/seccomp-regen-diff.sh", "action": "NEW", "note": "T12: moby default.json flatten+diff script; manual run on moby tag bump"},
  {"path": "infra/helm/rio-build/files/seccomp-rio-worker.json", "action": "MODIFY", "note": "T12: header //-comment pointing at regen script — post-P0223-merge"},
  {"path": ".claude/skills/nixbuild/SKILL.md", "action": "MODIFY", "note": "T13: --link in invocation table :8-14 + note after :36"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T14: extract bounceGatewayForSecret helper from sshKeySetup :486-524; keep SecretManager comment with helper"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T15: kvmCheck — drop GET_API_VERSION :159 + locals().get hack :167; collapse to open→CREATE_VM; move 4/7,3/7 ratios from f-string :169 to comment block :114-126"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T16: ConnectionResetError [Errno 104] as alt marker in TCG row :11 (QMP-side view of P0316 gate)"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T17: _LEASE_SECS 30*60 → 45*60 at :57 + comment referencing P0216 TCG data point"},
  {"path": "rio-test-support/src/wire.rs", "action": "MODIFY", "note": "T18: +PathInfoWire struct + read_path_info() helper at end of file ~:175"},
  {"path": "rio-gateway/tests/functional/references.rs", "action": "MODIFY", "note": "T18: migrate 2× 8-field discard-read :53-60 :149-156 → read_path_info()"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "T18: migrate discard-read sites :509-516 :558-565 → read_path_info()"},
  {"path": "rio-gateway/tests/functional/main.rs", "action": "MODIFY", "note": "T19: tracing_subscriber init (inline or via ctor) — mod.rs:152 debug! currently void"},
  {"path": "rio-gateway/tests/wire_opcodes/main.rs", "action": "MODIFY", "note": "T19: call common::init_test_logging() (ctor or per-test) — common/mod.rs:66 debug! currently void"},
  {"path": "rio-gateway/tests/functional/mod.rs", "action": "MODIFY", "note": "T20: comment at :127 documenting JoinHandle-detach-on-? edge (not fixing — test-only, rare, process-exit reaps)"}
]
```

**clippy.toml (created by P0290):** MODIFY at repo root — T6 adds OsStr/Path to_string_lossy entries. Same root-level-file pattern as P0295's README escape hatch; T1's regex fix here may make this fence-compatible — check the new pattern at dispatch.

```
.claude/lib/
├── onibus/models.py            # T1: regex extension at :322
└── plan-doc-skeleton.md        # T1: prose sync
infra/helm/grafana/
├── scheduler.json              # T2: sum(rate(...)) wraps
├── worker-utilization.json     # T4: drop no-op matchers
└── configmap.yaml              # T3: regen (GENERATED header)
justfile                        # T3: grafana-configmap target
```

## Dependencies

```json deps
{"deps": [222, 223, 290, 294, 315, 305, 306], "soft_deps": [303, 216, 289, 313, 206, 207, 317, 316, 0318], "note": "T2-T4 depend on P0222 (dashboard files exist — merged). T6 depends on P0290 (clippy.toml exists). T8/T9 depend on P0294 (Build CRD rip — dead variant becomes dead, lifecycle.nix rewritten). T11/T12 depend on P0223 (seccomp CEL rules + profile JSON exist). T14 soft-dep P0206 (lifecycle.nix :1204 copy exists post-P0206; if T14 lands first, extract in k3s-full.nix only and P0206/P0207 use the helper). T15 depends on P0315 (DONE — kvmCheck CREATE_VM probe exists; T15 drops the vestigial GET_API_VERSION it left behind; discovered_from=315). T10 SEQUENCED AFTER P0317 (_VM_FAIL_RE foundation — without it T10's early-return masks co-occurring real failures; re-scoped to supplementary grant per P0317 T7 forward-reference). T16 soft-dep P0316 (DONE — ConnectionResetError is downstream of its -machine accel=kvm gate) + P0317 T4 (if mitigations list migration landed, add as Mitigation entry instead of symptom string). T17 depends on P0306 (DONE — _LEASE_SECS lives in merge.py which P0306 refactored; discovered_from=306). T18 depends on P0305 (DONE — functional/references.rs discard-read sites exist; discovered_from=305). T19 depends on P0305 (same). T20 discovered_from=bughunter mc28 + soft-dep P0318 (same function, comment lands on whichever name is live). Soft: T5 references p216 worktree line numbers (P0216). T9 must land BEFORE P0289 dispatches. T10's KVM-DENIED-BUILDER marker is emitted by P0313 — match BOTH pre/post markers for transition. T1/T13 independent. discovered_from: T14=206, T15=315, T16=316, T17=306, T18=305, T19=305."}
```

**Depends on:** [P0222](plan-0222-grafana-dashboards.md) — merged at [`6b723def`](https://github.com/search?q=6b723def&type=commits). T1 (harness regex) has no dep.

**Conflicts with:** `infra/helm/grafana/*.json` freshly created by P0222; no other UNIMPL plan touches them. `justfile` is low-traffic. `.claude/lib/onibus/models.py` — [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) T2 touches it too (LockStatus cleanup), different section. `.claude/lib/onibus/build.py` — T10+T13 both touch it; [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) does not. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T6 adds asserts to `cel_rules_in_schema` (test fn), T11 here adds messages to the derive attrs (struct) — different sections, same file. Low conflict risk across the board.
