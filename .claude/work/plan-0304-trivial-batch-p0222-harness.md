# Plan 0304: Trivial-hardening batch — P0222 dashboard nits + harness regex

Four trivial items from the [P0222](plan-0222-grafana-dashboards.md) review sink plus one coordinator-surfaced harness regex gap. No open trivial batch existed; this is a fresh one. All four are sub-10-line edits; the configmap regen target is the "biggest" at ~20 lines of justfile.

T1 is **not** from P0222 — the coordinator surfaced it during the stage1 backfill merge when [P0075](plan-0075-cargo-deny.md)'s plan doc couldn't put `deny.toml` in its `json files` fence (regex rejected it). The original file:line ref (`state.py:389`) went stale in the onibus refactor ([`b2980679`](https://github.com/search?q=b2980679&type=commits)); re-located to [`models.py:322`](../../.claude/lib/onibus/models.py).

## Entry criteria

- [P0222](plan-0222-grafana-dashboards.md) merged (dashboard JSONs exist for T2-T4)
- [P0223](plan-0223-seccomp-localhost-profile.md) merged (seccomp CEL rules exist for T11; seccomp-rio-worker.json exists for T12)
- [P0267](plan-0267-atomic-multi-output-tx.md) merged (`put_path_batch.rs` exists with the :302 unwrap and metric gaps for T37-T38) — **DONE**

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

### T9 — ~~`refactor(nix):` lifecycle.nix extract submit_build_grpc~~ — OBE, see [P0362](plan-0362-extract-submit-build-grpc-helper.md)

**SUPERSEDED** by [P0362](plan-0362-extract-submit-build-grpc-helper.md): the copy-paste HAS
happened (4 copies at lifecycle.nix:556/707/858 + scheduling.nix:900;
P0360 will add a 5th). T9's scope was lifecycle.nix-only and bundled
nix-instantiate+nix-copy into the helper; the actual shared surface is
narrower (grpcurl+JSON-parse) and spans two fixture styles. Skip T9;
P0362 is the live plan.

### T10 — `fix(harness):` onibus flake excusable() — TCG/exit-143 pattern

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

[P0207](plan-0207-mark-cte-tenant-retention.md) will likely need a third copy (tenant-key build for the GC mark VM test — same pattern: update Secret → gateway must see the fresh key).

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

### T21 — `docs(gateway):` corpus README regen script — nix-hash pipeline captures hash not path

MODIFY [`rio-gateway/tests/golden/corpus/README.md`](../../rio-gateway/tests/golden/corpus/README.md) at `:22-23`. The regen snippet is broken:

```bash
NIX_SRC=$(nix-hash --type sha256 --sri $(ls -d /nix/store/*-source) 2>/dev/null \
  | grep -B0 'qzVtneydMSjNZXzNbxQG9VvJc490keS9RNlbUCfiQas=' | head -1)
```

`nix-hash` emits **hashes only** — `grep | head` captures a hash string into `$NIX_SRC`, not a path. The subsequent `cp $NIX_SRC/src/libstore-tests/...` fails (`cp: cannot stat 'sha256-qzVt.../src/...'`). The comment at `:24` already has the working form: `nix eval --raw .#inputs.nix.sourceInfo.outPath` — canonical, doesn't scan `/nix/store`.

Delete `:22-23` (the broken pipeline and the `# or:` comment prefix). Keep only:

```bash
NIX_SRC=$(nix eval --raw .#inputs.nix.sourceInfo.outPath)
cp $NIX_SRC/src/libstore-tests/data/worker-protocol/realisation.bin \
   ca-register-2deep.bin
# ... rest unchanged
```

### T22 — `docs:` known-flakes.jsonl header — schema comment stale after P0317 migration

MODIFY [`.claude/known-flakes.jsonl`](../../.claude/known-flakes.jsonl) at `:2-4`. [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) added `drv_name` + `mitigations` fields and changed the match semantics, but the header comments still describe the pre-P0317 schema. This is the **same three-sources-carry-false-model** problem P0317 was fixing, re-introduced in the file it migrated.

Current `:2`:
> `# Each row: {"test","symptom","root_cause","fix_owner":"P<N>","fix_description","retry"}`

Omits `drv_name` (optional, VM-tests-only match key) and `mitigations` (list). New:
> `# Each row: {"test","drv_name"?(VM only),"symptom","root_cause","fix_owner":"P<N>","fix_description","retry","mitigations":[{plan,landed_sha,note}]}`

Current `:4`:
> `# Match key is `test` (exact); `symptom` is the grep-able CI log signature for human cross-check.`

Wrong for VM tests post-P0317. New:
> `# Match key: `drv_name` for VM tests (the <N> in vm-test-run-<N>.drv per _VM_FAIL_RE); `test` for nextest. `symptom` is human-facing grep signature.`

### T23 — `refactor(harness):` extract _read_header() — header-extraction list-comp duplicated

[`cli.py:380`](../../.claude/lib/onibus/cli.py) inlines the same header-extraction pattern that [`jsonl.py:68`](../../.claude/lib/onibus/jsonl.py) has inside `remove_jsonl`:

```python
header = [ln for ln in KNOWN_FLAKES.read_text().splitlines() if ln.startswith("#")]
```

One-line DRY. MODIFY [`.claude/lib/onibus/jsonl.py`](../../.claude/lib/onibus/jsonl.py) — add after `read_jsonl` at `:44`:

```python
def read_header(path: Path) -> list[str]:
    """#-prefixed comment lines from the top of a JSONL file. Empty list
    if no file or no header. Used by remove_jsonl + cli flake-mitigation
    for header-preserving rewrites."""
    if not path.exists():
        return []
    return [ln for ln in path.read_text().splitlines() if ln.startswith("#")]
```

Then: `remove_jsonl` at `:66-68` becomes `header = read_header(path)`. And [`cli.py:380`](../../.claude/lib/onibus/cli.py) becomes `header = read_header(KNOWN_FLAKES)` (add `read_header` to the import from `onibus.jsonl`).

**Alternative considered:** `rewrite_jsonl(path, model, mutate_fn)` that encapsulates read-modify-write-with-header. More abstraction for the one extra call-site; the plain helper is simpler. If a third call-site appears, revisit.

### T24 — `docs:` dag.jsonl P992247601 → P0317 in rows 295+304

MODIFY [`.claude/dag.jsonl`](../../.claude/dag.jsonl) at `:295` and `:304`. Both rows reference `P992247601` — a stale placeholder from a prior `/plan` run ([`dcb7f1e0`](https://github.com/search?q=dcb7f1e0&type=commits)) that was never renumbered to its real target. The reference was **meant** to be P0317 (the `_VM_FAIL_RE` foundation). Any implementer following P0304's "T10 SEQUENCED AFTER P992247601" note at `:304` hits a dead ref.

```bash
sed -i 's/P992247601/P0317/g' .claude/dag.jsonl
```

Verify: `grep P992247601 .claude/dag.jsonl` → 0. Only two occurrences (`:295` note field and `:304` note field); both become `P0317`. Neither is in a `deps` array (the placeholder was never in deps — `note` field only), so no frontier-computation impact. Pure cosmetic but load-bearing for implementer navigation.

### T25 — `docs:` scheduler.md:451 spec drift — `Failed { status: TimedOut }` → `Failed` with error_summary

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) at `:451`. The `r[sched.timeout.per-build]` text says builds transition to `Failed { status: TimedOut }`, but `BuildState` at [`build.rs:16-22`](../../rio-scheduler/src/state/build.rs) is a flat enum — `Failed` has no associated data. Separately, [`errors.md:17`](../../docs/src/errors.md) (correctly, P0214-updated) says: "There is no `TimedOut` variant in the `BuildResultStatus` proto enum. Nix's `BuildStatus::TimedOut` (wire value 8) currently maps to `PermanentFailure` via the worker's fallthrough." So `TimedOut` doesn't exist at the build level OR the derivation-result level.

The actual code at [`worker.rs:598+606`](../../rio-scheduler/src/actor/worker.rs) sets `build.error_summary = Some("build_timeout {timeout}s exceeded...")` then calls `transition_build_to_failed(build_id)`. Spec should match.

[P0214](plan-0214-per-build-timeout.md)'s Files fence listed `scheduler.md` but the impl never touched it — the spec text predates the impl and was never reconciled.

Replace `:449-451`:
```markdown
has its non-terminal derivations cancelled and transitions to
`Failed`, with `error_summary` set to `"build_timeout {N}s exceeded
(wall-clock since submission)"`. This is distinct from
```

**No `tracey bump`** — the requirement didn't change (wall-clock timeout → fail the build), only the prose description of the terminal state. The `r[impl sched.timeout.per-build]` annotation at [`worker.rs:570`](../../rio-scheduler/src/actor/worker.rs) is already correct against the corrected text.

### T26 — `docs:` observability.md — add rio_scheduler_build_timeouts_total row + 4th cancel trigger

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md). Two gaps from the same P0214 miss:

**At `:114`** (after `rio_scheduler_backstop_timeouts_total`, before `rio_scheduler_worker_disconnects_total`), insert a new table row:

```markdown
| `rio_scheduler_build_timeouts_total` | Counter | Builds failed by per-build wall-clock timeout (`BuildOptions.build_timeout` seconds since submission). Distinct from `backstop_timeouts_total` (per-derivation heuristic). Emitted at [`worker.rs:593`](../../rio-scheduler/src/actor/worker.rs). |
```

**At `:116`** — `rio_scheduler_cancel_signals_total` description currently says "explicit CancelBuild, backstop timeout, or finalizer drain". There's a 4th caller: per-build timeout at [`worker.rs:605`](../../rio-scheduler/src/actor/worker.rs) goes through `cancel_build_derivations` which sends `CancelSignal`. Update the trigger list:

```markdown
| `rio_scheduler_cancel_signals_total` | Counter | CancelSignal messages sent to workers (explicit CancelBuild, backstop timeout, per-build timeout, or finalizer drain). |
```

[P0328](plan-0328-metrics-registered-bidirectional.md) T2 adds the `describe_counter!` call — that's the code half. This is the doc half. Sequence-independent.

### T27 — `docs:` scheduler.md tracey blank-line — 3 siblings of P0320's fix

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md). [P0320](plan-0320-restructure-p0253-per-adr-018.md) fixed `r[sched.ca.resolve]` at `:265` — blank line after the marker meant tracey parsed empty text for the rule. `tracey bump` silently no-oped against empty text. Three siblings from the same `f190e479` seed commit have the same defect:

- `:253` `r[sched.ca.detect]` — blank line at `:254`, text starts `:255`
- `:257` `r[sched.ca.cutoff-compare]` — blank line at `:258`, text starts `:259`
- `:261` `r[sched.ca.cutoff-propagate]` — blank line at `:262`, text starts `:263`

Compare `:265` (fixed): marker immediately followed by text at `:266`, no blank.

Three one-line deletions — remove the blank at `:254`, `:258`, `:262`. Line numbers shift as you delete; work bottom-up (`:262` → `:258` → `:254`) or use a single sed.

**Verify with `tracey query rule sched.ca.detect`** post-fix — the rule text should be non-empty (currently tracey reports it as defined but text-less).

### T28 — `fix(harness):` rio-planner.md — explicit no-leading-zero guard for json deps

MODIFY [`.claude/agents/rio-planner.md`](../../.claude/agents/rio-planner.md) near `:117`. Two prior runs (docs-928654 wrote `0318`; docs-930681 wrote `0322`/`0323`) emitted leading-zero integers in `json deps` fences — e.g. `{"deps": [0318]}`. JSON rejects leading zeros on numbers (RFC 8259 §6). The agent mechanically copied the zero-padded plan number from a `P0318` reference into the fence. Coordinator caught both at merge with a manual fix each time.

**Option A (agent guidance):** add after `:125` (after "deps are integer dep-numbers"):

```markdown
**JSON integers have NO leading zeros** — `P0318` in prose is `318`
in the `deps` array, not `0318`. Leading zeros are a JSON syntax
error (RFC 8259 §6); `json.loads` rejects on read. Twice-burned
(docs-928654, docs-930681).
```

**Option B (qa-check auto-fix):** strip leading zeros in a validation pass. Rejected — auto-fixing `0318` → `318` silently is fine, but auto-fixing `08` → `8` might mask a typo where the planner meant `80`. Reject-and-report is safer than fix-and-proceed.

**Do A.** The agent file is session-cached; this lands on the next worktree-add.

### T29 — `build(nix):` tracey-validate src — fileset.difference to exclude .claude/

MODIFY [`flake.nix:401`](../../flake.nix). The CLAUSE-4(a) fast-path premise — ".claude/-only edits are HASH-identical to the `.#ci` derivation" — is **false**. `tracey-validate` at `:398-416` uses `src = pkgs.lib.cleanSource ./.;`, which includes `.claude/` in the derivation source. OUTPUT is identical (tracey's `config.styx` doesn't scan `.claude/`), so the 21 fast-path proceeds all held — but on a BEHAVIORAL-identity basis, not the claimed hash-identity. Same tier as P0317's comment-only-nix edits.

**Option B per bughunter — make the premise TRUE instead of documenting its falsity.** `lib.fileset` is already in use at [`flake.nix:168-174`](../../flake.nix) for the crane source filter. Replace `:401`:

```nix
src = pkgs.lib.fileset.toSource {
  root = ./.;
  fileset = pkgs.lib.fileset.difference
    (pkgs.lib.fileset.fromSource (pkgs.lib.cleanSource ./.))
    ./.claude;
};
```

This excludes `.claude/` from the tracey-validate derivation's source hash. After this lands, `.claude/`-only commits ARE hash-identical to `tracey-validate` — the fast-path premise becomes true. Bonus: one fewer rebuild per `.claude/` commit (currently tracey-validate re-runs on every agent-file edit for no reason).

**Verify:** `nix eval .#checks.x86_64-linux.tracey-validate.drvPath` before/after a `.claude/`-only edit → same drv hash.

### T30 — `fix(harness):` rename-unassigned — scan batch-append targets for placeholder refs

[P0325](plan-0325-rename-unassigned-post-ff-rewrite-skip.md)'s fix at [`merge.py:344-347`](../../.claude/lib/onibus/merge.py) derives `touched` from `mapping` — one file per new plan (`plan-{placeholder}-{slug}.md`) plus `dag.jsonl`. The comment at `:334-343` says "no other file type carries placeholders (by construction — the planner only writes to `.claude/work/plan-*.md` and dag.jsonl)". **False.** The planner ALSO **appends** to open batch docs ([P0304](plan-0304-trivial-batch-p0222-harness.md), [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)) — and those appends can contain cross-references to placeholders from the SAME planner run (e.g., "soft-dep P993342102" in a Dependencies fence, or "[P993342103](plan-993342103-...)" in a Conflicts-with line).

**Manifested:** docs-933421 left stale `P993342102` / `P993342103` refs in P0304 and P0311 after merge — the placeholders were renamed in their OWN plan docs + dag.jsonl, but the batch-append cross-refs stayed. Coordinator caught post-merge via grep.

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) in `_rewrite_and_rename` — after building `touched` from `mapping` at `:344-347`, add a second pass:

```python
    # Second pass: batch-append targets. The planner appends T-tasks to
    # open batch docs (P0304, P0311, ...), and those appends can cross-
    # reference placeholders from the same run ("soft-dep P993342102").
    # docs-933421 left stale refs in TWO batch docs — the mapping-
    # derived `touched` above only covers NEW plan docs, not appends.
    # Grep every .claude/work/*.md for any placeholder substring.
    placeholders = [r.placeholder for r in mapping]
    for p in (worktree / ".claude/work").glob("plan-*.md"):
        rel = str(p.relative_to(worktree))
        if rel in touched:
            continue  # already covered (it's one of the new docs)
        text = p.read_text()
        if any(ph in text for ph in placeholders):
            touched.append(rel)
```

This is O(files × placeholders) — fine, `.claude/work/` has ~300 files and a typical run has ≤5 placeholders. The `if rel in touched` guard avoids double-processing the new docs (harmless if skipped, but wasteful).

Also MODIFY the comment at `:334-343` — the "by construction" claim is wrong, fix it:

> The planner writes placeholders to NEW plan docs (mapping-derived above), `dag.jsonl`, AND batch-append targets (second pass below, grep-derived).

**Test:** [`test_scripts.py`](../../.claude/lib/test_scripts.py) — add after wherever P0325's tests live (grep `rename_unassigned` or `rewrite_and_rename`):

```python
def test_rewrite_scans_batch_append_targets(tmp_path):
    # Scratch worktree with: one new-plan doc (mapping-covered), one
    # batch doc that REFERENCES the placeholder (not mapping-covered).
    work = tmp_path / ".claude/work"
    work.mkdir(parents=True)
    (work / "plan-993342102-new-thing.md").write_text("# Plan 993342102\n")
    (work / "plan-0304-batch.md").write_text(
        "soft-dep [P993342102](plan-993342102-new-thing.md)"
    )
    (tmp_path / ".claude/dag.jsonl").write_text('{"plan": 993342102}\n')
    mapping = [Rename(placeholder="993342102", slug="new-thing", assigned=330)]
    _rewrite_and_rename(tmp_path, mapping)
    # THE ASSERTION: batch doc was rewritten too.
    assert "P0330" in (work / "plan-0304-batch.md").read_text()
    assert "993342102" not in (work / "plan-0304-batch.md").read_text()
```

### T31 — `fix(harness):` tracey test_include — 13 invisible r[verify] in 4 crates

MODIFY [`.config/tracey/config.styx`](../../.config/tracey/config.styx) at the `test_include` block (`:50-54`).

**Current** (three globs):
```styx
test_include (
  rio-gateway/tests/**/*.rs
  rio-proto/tests/**/*.rs
  nix/tests/*.nix
)
```

**Thirteen `r[verify ...]` annotations in four unscanned crates** — tracey never sees them, `tracey query untested` falsely reports these markers as impl-only. [P0328](plan-0328-metrics-registered-bidirectional.md) added 6 of them (all the `metrics_registered.rs` duplicates); the `rio-store/tests/grpc/*.rs` 5 pre-date P0328.

| File | Marker(s) |
|---|---|
| [`rio-scheduler/tests/metrics_registered.rs:93,134`](../../rio-scheduler/tests/metrics_registered.rs) | `r[verify obs.metric.scheduler]` ×2 |
| [`rio-store/tests/grpc/chunked.rs:2,6`](../../rio-store/tests/grpc/chunked.rs) | `r[verify store.put.wal-manifest]`, `r[verify store.inline.threshold]` |
| [`rio-store/tests/grpc/core.rs:69,252`](../../rio-store/tests/grpc/core.rs) | `r[verify obs.metric.transfer-volume]`, `r[verify store.put.idempotent]` |
| [`rio-store/tests/grpc/hmac.rs:38`](../../rio-store/tests/grpc/hmac.rs) | `r[verify sec.boundary.grpc-hmac]` |
| [`rio-store/tests/metrics_registered.rs:73,110`](../../rio-store/tests/metrics_registered.rs) | `r[verify obs.metric.store]` ×2 |
| [`rio-worker/tests/metrics_registered.rs:71,103`](../../rio-worker/tests/metrics_registered.rs) | `r[verify obs.metric.worker]` ×2 |
| [`rio-controller/tests/metrics_registered.rs:60,92`](../../rio-controller/tests/metrics_registered.rs) | `r[verify obs.metric.controller]` ×2 |

**Fix** — extend the glob list:
```styx
test_include (
  rio-gateway/tests/**/*.rs
  rio-proto/tests/**/*.rs
  rio-scheduler/tests/**/*.rs
  rio-store/tests/**/*.rs
  rio-worker/tests/**/*.rs
  rio-controller/tests/**/*.rs
  nix/tests/*.nix
)
```

**Daemon cache is sticky** — after editing config.styx, kill the tracey daemon before re-querying: `ps aux | grep 'tracey daemon' | grep -v grep | awk '{print $2}' | xargs kill` (NOT `pkill -f tracey` — that kills the MCP sidecar too).

**This is a tracey-validate neutral change** — the annotations already exist and reference valid markers, they're just invisible. `.#ci`'s `tracey-validate` check greps for `0 total error(s)`; adding these to the scan can ONLY surface errors if any of the 13 has a typo'd marker ID. `tracey query validate` pre-commit to catch that.

### T32 — `refactor(gateway):` tag session.rs:32 TODO with owning plan P0335

MODIFY [`rio-gateway/src/session.rs`](../../rio-gateway/src/session.rs) at `:32`.

Bughunter found this as an "orphan TODO" but it already has an owner — [P0335](plan-0335-channelsession-drop-abort-race.md) is **exactly** this race (its dag note: "ChannelSession::Drop does proto_task.abort() — if abort wins one scheduler-yield window before EOF-arm's cancel loop, CancelBuild never fires. Fix T1+T2: CancellationToken, session.rs selects on it"). The prose at [`:32`](../../rio-gateway/src/session.rs) says "Tracked as a TODO for a select!-based fix" — it just needs the tag:

```rust
// Before:
// but TCP RST can reorder. Tracked as a TODO for a select!-based fix.

// After:
// but TCP RST can reorder. TODO(P0335): select!-based fix via
// CancellationToken — Drop fires token.cancel(), this fn selects
// on it so both the EOF-arm and the Drop path converge here.
```

This is **not** adding a TODO — it's **tagging** an existing one that was written as prose. `grep 'TODO[^(]' rio-gateway/src/session.rs` audit goes clean.

### T33 — `refactor(gateway):` handler/build.rs:176 cosmetic "TODO" → "deferred"

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) at `:176`.

The comment says: `"that's TODO — for now, matches the Started/Progress debug-only pattern"`. This is not a deferred-work TODO; it's a passing observation that the `InputsResolved` branch *could* emit a user-visible stderr line. No plan owns it because nothing needs to own it — it's a hypothetical UX nicety, not a gap.

Two options:
- **Drop the word** — `"stderr.log() is deferred; for now, matches the Started/Progress debug-only pattern"`. Keeps the observation, loses the false signal.
- **Delete the whole parenthetical** — just `"// Scheduler's store cache-check done; dispatch begins next. Matches the Started/Progress debug-only pattern."`. The "could surface via stderr.log()" suggestion doesn't earn its lines.

Either way: `grep 'TODO' rio-gateway/src/handler/build.rs` at `:176` → 0 hits.

### T34 — `test(scheduler):` emit_progress missing at 3 recovery paths

[P0270](plan-0270-buildstatus-critpath-workers.md) reviewer: `emit_progress` is called at exactly two sites — [`dispatch.rs:483`](../../rio-scheduler/src/actor/dispatch.rs) (assignment success) and [`completion.rs:528`](../../rio-scheduler/src/actor/completion.rs) (completion). Three recovery paths that change `running`/`queued`/`assigned_workers` state do NOT emit:

| Path | File:line | State change | Dashboard staleness |
|---|---|---|---|
| `handle_worker_disconnected` | [`worker.rs:56-79`](../../rio-scheduler/src/actor/worker.rs) | N derivations Running/Assigned → Ready via `reassign_derivations`; worker removed from `assigned_workers` | Until next dispatch or completion — on a quiet build (no new work, no completions), indefinite |
| assignment-send-fail rollback | [`dispatch.rs:420-459`](../../rio-scheduler/src/actor/dispatch.rs) | 1 derivation Assigned → Ready via `reset_to_ready`; `return false` at `:459` bypasses the `emit_progress` at `:483` | Until the SAME derivation re-dispatches (defers back to queue) |
| `handle_infrastructure_failure` | [`completion.rs:717-740`](../../rio-scheduler/src/actor/completion.rs) | 1 derivation Running → Ready via `reset_to_ready` + `push_ready` | Until re-dispatch |

The `BuildProgress` message ([`mod.rs:962-975`](../../rio-scheduler/src/actor/mod.rs)) carries `running`, `queued`, `assigned_workers` — all three recovery paths change at least one of those. Dashboard shows stale counts until the next unrelated event fires.

MODIFY each of the three sites — add `emit_progress` for each affected build. The pattern is the same as at `dispatch.rs:464-484`: get `interested_builds` for the drv (or for `worker.running_builds` in the disconnect case), loop, emit.

**worker_disconnected** (after `reassign_derivations` returns, before the metrics decrement at `:75`):
```rust
// Dashboard: running count dropped by to_reassign.len(); assigned_workers
// set lost this worker. Without emit_progress here, a quiet build shows
// stale state until the next unrelated dispatch/completion.
let affected: std::collections::HashSet<Uuid> = to_reassign
    .iter()
    .flat_map(|h| self.get_interested_builds(h))
    .collect();
for build_id in affected {
    self.emit_progress(build_id);
}
```

**assignment-send-fail** (after `delete_latest_assignment` at `:457`, before `return false` at `:459`):
```rust
// This derivation WAS Assigned (counted in running), now Ready (queued).
// emit_progress so the dashboard sees the rollback.
for build_id in self.get_interested_builds(drv_hash) {
    self.emit_progress(build_id);
}
```

**handle_infrastructure_failure** (after `push_ready` at `:739`):
```rust
for build_id in self.get_interested_builds(drv_hash) {
    self.emit_progress(build_id);
}
```

NEW assertion in [`actor/tests/worker.rs`](../../rio-scheduler/src/actor/tests/worker.rs) — extend the existing `WorkerDisconnected` test at `:186` (or add a sibling): assert that a `BuildProgress` event with `running: 0` arrives on the event stream after disconnect (there was 1 running before). If the test harness doesn't expose the event stream cleanly, a state-probe alternative: assert `dag.build_summary(build_id).running == 0` post-disconnect AND that the event-channel receiver has ≥1 pending message.

**Hot-file warning:** all three are high-collision — `worker.rs` count=27, `completion.rs` count=25, `dispatch.rs` count=22. Each edit is 3-6 lines of additive insertion at a specific call site; no signature changes.

### T35 — `docs(harness):` dag-run SKILL — task-notification ≠ agent-done

Coordinator self-report from P0262's merge. A background-task-completion notification (the `.#ci` subshell's stdout arriving) was misread as "merger agent done" — coordinator acted on the lock while the merger was mid-CI-retry. The 95-second lock hold was legitimate; the coordinator's assumption was not. This is a second manifestation of the lesson already at [`dag-run/SKILL.md:45`](../../.claude/skills/dag-run/SKILL.md) ("Do NOT pre-bump on inferred merges — double-bump risk when notification lags") — same root misunderstanding (notification timing ≠ agent state), different symptom.

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) at `:47` — extend the **merger lock** paragraph:

Before:
```markdown
**merger lock:** check `.claude/bin/onibus merge lock-status` before any direct-to-`$TGT` commit. `held: false` → safe. `held: true, stale: false` → merger in flight, WAIT. `held: true, stale: true` → merger died — read `ff_landed`: `false` → just `merge unlock`; `true` → ff landed, finish steps 7-8 manually then unlock.
```

After:
```markdown
**merger lock:** check `.claude/bin/onibus merge lock-status` before any direct-to-`$TGT` commit. `held: false` → safe. `held: true, stale: false` → merger in flight, WAIT. `held: true, stale: true` → merger died — read `ff_landed`: `false` → just `merge unlock`; `true` → ff landed, finish steps 7-8 manually then unlock. **A background-task notification is NOT a merger-done signal.** The merger may receive its `.#ci` subshell result and then retry (known-flake excusable, clause-4 fast-path, semantic-conflict fix loop) — the lock stays held through retries. `lock-status` is the only authoritative signal. Treating a task-notification as agent-termination is the same inference-vs-state mistake as the merge-count pre-bump at `:45`.
```

### T36 — `docs:` gateway.md:462 STDERR_ERROR → STDERR_LAST post-remediation-07

[P0302](plan-0302-gateway-remediation-07-stderr-sequencing.md) (or the phase4a remediation-07 that P0302 references) changed `validate_dag` failure handling: it no longer sends `STDERR_ERROR` (terminal frame). Instead, the failure is wrapped in `BuildResult::failure` and delivered via `STDERR_LAST` + result — the recoverable-per-operation path per [`gateway.md:584`](../../docs/src/components/gateway.md). Code at [`build.rs:651-656`](../../rio-gateway/src/handler/build.rs) confirms: "Do NOT send STDERR_ERROR here — it is a terminal frame. [...] after STDERR_LAST." The inline `__noChroot` check at [`build.rs:609`](../../rio-gateway/src/handler/build.rs) still uses `stderr_err!` (terminal) — that's `wopBuildDerivation`-only, which doesn't have the BuildResult-wrapping path.

Two doc locations drift:

**[`gateway.md:462`](../../docs/src/components/gateway.md)** — step 4 "Validation (`validate_dag`)" says both malformed and missing `.drv` cases "cause `STDERR_ERROR`". Rewrite to:

> Malformed `.drv` files and missing `.drv` files (referenced by `inputDrvs` but not in the store) are rejected via `BuildResult::failure` delivered through `STDERR_LAST` — the session stays open, subsequent opcodes are accepted. (Previously `STDERR_ERROR` terminal; changed in remediation-07 to avoid the ERROR→LAST desync when called from `wopBuildPaths`/`wopBuildPathsWithResults`, which wrap the error.)

**[`gateway.md:466`](../../docs/src/components/gateway.md)** — `r[gw.reject.nochroot]` text says "Both paths send `STDERR_ERROR`". Path (2) — the inline `wopBuildDerivation` check — still does. Path (1) — `validate_dag` — does not. Rewrite the sentence:

> Rejection happens at two points with different frame semantics: (1) `validate_dag` rejects via `BuildResult::failure` → `STDERR_LAST` (opcodes 36/46 wrap the error; session stays open); (2) `wopBuildDerivation`'s inline check sends `STDERR_ERROR` terminal (opcode 9 doesn't wrap — this is a protocol-level reject).

**This is a text change to an `r[...]`-marked paragraph.** The behavior the marker describes is MORE permissive now (session stays open for path 1), not more restrictive — existing `r[impl gw.reject.nochroot]` annotations still satisfy the constraint. Run `tracey bump` only if the implementer judges the annotation sites need review; the mechanical check is what's tested (rejection happens), not which frame type carries it. Default: no bump.

### T37 — `refactor(store):` put_path_batch :302 `.unwrap()` → `.expect()` with invariant cite

MODIFY [`rio-store/src/grpc/put_path_batch.rs:302`](../../rio-store/src/grpc/put_path_batch.rs):

```rust
// Before:
let mut created = vec![false; (*outputs.keys().last().unwrap() as usize) + 1];
// After:
let mut created = vec![false; (*outputs.keys().last().expect("non-empty: checked at :197") as usize) + 1];
```

The empty-check at [`:197`](../../rio-store/src/grpc/put_path_batch.rs) (`if outputs.is_empty() { return Err(...) }`) guarantees `.last()` is `Some`. Parity with [`:308`](../../rio-store/src/grpc/put_path_batch.rs) `.expect("validated in phase 2")` — same invariant-cite style. Bughunter mc=77 confirms all 55 workspace `.unwrap()`s are 52 cfg(test) + 3 invariant-cite; this one completes the 3→4 set (or was miscounted as cfg-test — either way, `.expect()` is the house style).

### T38 — `refactor(store):` put_path_batch metric parity with put_path.rs

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) — three gaps vs [`put_path.rs:232`/`:350`/`:604`](../../rio-store/src/grpc/put_path.rs):

1. **`already_complete` at [`:258`](../../rio-store/src/grpc/put_path_batch.rs)** — no `result=exists` counter. Add after `accum.already_complete = true;`:
   ```rust
   metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
   ```
2. **No duration histogram.** Wrap the handler body in a timer (mirror [`put_path.rs:232`](../../rio-store/src/grpc/put_path.rs) `rio_store_put_path_duration_seconds`). Record once at the end of success (before `Ok(Response::...)` at [`:335`](../../rio-store/src/grpc/put_path_batch.rs)):
   ```rust
   // At top of put_path_batch_impl, after link_parent(&request):
   let _timer = metrics::histogram!("rio_store_put_path_duration_seconds").start_timer();
   ```
   (Drop-based timers record on any exit — success OR bail. If `start_timer()` isn't the house API, use explicit `Instant::now()` + `.record(elapsed)` before `Ok` and inside `abort_batch` at [`:350`](../../rio-store/src/grpc/put_path_batch.rs).)
3. **Phase-1 `?` paths don't increment `result=error`** — actually phase-1 has no `?`, only `bail!` at `:93` etc., which calls `abort_batch` at [`:350`](../../rio-store/src/grpc/put_path_batch.rs) which already increments `result=error`. **So this finding is partially stale** — the error counter IS incremented via `abort_batch`. Verify at dispatch: `grep 'result.*error' rio-store/src/grpc/put_path_batch.rs` → expect 1 hit at `:350`. If so, this sub-item is a no-op; otherwise add.

Net: batch RPC becomes visible in the same Grafana panels as single PutPath (the [`rio_store_put_path_total`](../../docs/src/observability.md) and `_duration_seconds` rows at [`observability.md:129-130`](../../docs/src/observability.md)).

### T39 — `docs(harness):` dag-run SKILL — coordinator content-verification discipline

Coordinator self-report (mc=77-80, 3× same pattern). The coordinator fabricated specific details from attributed merger/validator notifications without reading their `<result>` content: constructed "FULL CI GREEN" from `status:merged` (when `failure_detail` said "clause-4(c), single VM fail"); constructed wrong filenames/hashes (RESTRICT/NOT VALID when actual was CASCADE, `b28a3f9c` when actual was `352d05aa`). Pattern: **inferring from summary rather than reading content**. Three retractions on the SAME P0311-closure claim.

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) — after the T35-added lock-stomp paragraph at `:47`, add a **content-verification** checklist:

```markdown
**Before acting on a merger/validator notification:** the task-tool's `<result>` is the ONLY source of truth — not the notification's `status:` field, not your expectation of what the agent probably did. Four mechanical checks before writing merge-state or re-dispatching:

1. `git merge-base --is-ancestor <hash-from-result> HEAD` → rc=0 (hash exists and is merged)
2. `cat .claude/state/merge-count.txt` matches the report's stated count
3. `grep '"plan": <N>' .claude/dag.jsonl | grep DONE` — dag status flipped
4. `grep 'clause-4\|rc=0' <result>` — `status:merged` alone does NOT distinguish full-green from clause-4(c) fast-path. If `failure_detail` contains `clause-4`, it was a fast-path merge (nextest-standalone passed, VM tier unproven).

If ANY diverges: re-read the `<result>` literal. Do NOT construct details from expectation ("FK plan → probably RESTRICT", "queued merge → probably full green"). The coordinator's history of content-fabrication (mc77 P0259 hash-wrong, mc78 P0332 filename-wrong, mc79 P0302 hash-not-in-tree) shares this root: treating `status:completed` as confirmation of expected outcome instead of cue to read what actually happened.
```

This is the 4-check discipline from the coordinator's own retraction post-mortems, codified where the coordinator will see it at dispatch time.

### T40 — `docs(harness):` bughunter mc=77 + mc=84 null-markers recorded

Bughunter mc=77 cadence run reviewed 7 merges (P0259…P0270) and found **no cross-plan pattern above threshold**. Bughunter mc=84 (`d2041017..4986ab50`) found one real bug (the `:275` `?`→`bail!` leak, owned by [P0342](plan-0342-put-path-batch-bail-on-placeholder-insert.md)) and otherwise null smell-accumulation. Record both in [`.claude/notes/bughunter-log.md`](../../.claude/notes/bughunter-log.md) (create if absent):

```markdown
- mc=77 (2026-03-19): 7-merge scan, null result. proto field-collision=none, emit_progress=single-def, path_tenants PK=point-lookup (≤1 row, JOIN can't multiply), config.styx changes=additive-only. unwrap/expect audit: 55 total = 52 cfg(test) + 3 invariant-cite. swallow=0, orphan-TODO=0.
- mc=84 (2026-03-19): 8-merge scan (d2041017..4986ab50). Found :275 ?→bail! leak → P0342. Smell-audit null: unwrap/expect=38 (non-test=3, all invariant-cite), silent-swallow=0, orphan-TODO=0 (upload.rs:164 TODO(P0263) closed this window), #[allow]=0, lock-cluster=0. grpc/mod.rs collision scan: P0266 fence speculative (ProgressUpdate handler doesn't exist at fence path); no trait-impl drift.
```

Why record null results: absence of finding IS signal. The "path_tenants PK point-lookup" observation closed a potential join-amplification bug before it became a plan. Future bughunter runs can diff against the audit counts (unwrap count should stay ~constant; a jump = review).

**If `.claude/notes/bughunter-log.md` doesn't exist:** create it with a one-line header (`# Bughunter cadence log — null results and below-threshold observations`) and both entries. [`.claude/notes/`](../../.claude/notes/) is design-provenance (not plan docs per the layout convention).

### T41 — `docs:` migration-018 stale "017" refs (4 sites)

MODIFY (post-[P0264](plan-0264-chunk-tenant-isolation.md)-merge) — the coordinator renumbered P0264's migration from 017 to 018 (P0332 also picked 017, landed first as `017_tenant_keys_fk_cascade.sql`). The header + 3 code comments still say "017":

**ALREADY FIXED** by [`76ba3999`](https://github.com/search?q=76ba3999&type=commits) (amended into P0264's merge). The coordinator renumbered P0264's migration from 017 to 018 AND swept the 4 stale "017" refs (header + 3 code comments) in the same merge-window. T41 is **OBE** unless `grep 'migration 017\|migration-017' migrations/018_chunk_tenants.sql rio-store/src/grpc/chunk.rs rio-store/src/metadata/chunked.rs rio-store/tests/grpc/chunk_service.rs` still finds hits at dispatch (it shouldn't).

**At dispatch: verify grep → 0, then skip T41 and note as OBE in the impl report.** Kept in batch for audit trail.

### T42 — `docs(store):` backend/chunk.rs:451 stale fn reference

MODIFY [`rio-store/src/backend/chunk.rs:451`](../../rio-store/src/backend/chunk.rs). Comment references `metadata::find_missing_chunks` — P0264 renames it to `find_missing_chunks_for_tenant`. Doubly stale: the fn was renamed AND PutPath now uses inline `RETURNING` (`cas.rs:do_upload`), not a separate find-missing probe. Either rewrite the parenthetical to reference `upgrade_manifest_to_chunked`'s RETURNING clause, or drop the parenthetical entirely.

**P0264 merged** ([`76ba3999`](https://github.com/search?q=76ba3999&type=commits)) — on sprint-1, [`backend/chunk.rs:451`](../../rio-store/src/backend/chunk.rs) still says "`refcounts (metadata::find_missing_chunks, cas.rs:do_upload)`". The metadata fn renamed to `find_missing_chunks_for_tenant` ([`chunked.rs:317`](../../rio-store/src/metadata/chunked.rs)); the gRPC method `find_missing_chunks` at [`grpc/chunk.rs:378`](../../rio-store/src/grpc/chunk.rs) wraps it. Comment points at the wrong layer now. 76ba3999's sweep didn't touch `backend/chunk.rs` — confirmed live at dispatch.

### T43 — `docs:` P0273 T5 streaming-trailer grep too loose

MODIFY [`.claude/work/plan-0273-envoy-sidecar-grpc-web.md:279`](plan-0273-envoy-sidecar-grpc-web.md). The T5 trailer-frame grep `` xxd | tail -5 | grep -q ' 80' `` matches ANY `0x80` byte in the last ~80 bytes — false green if the response body contains `0x80`. Tighten to the trailer-frame prefix `80 00 00 00` (type byte + 3-byte length prefix for an empty trailer, or anchor on the frame boundary). Or use `xxd -p | tail -c 50 | grep -q '^80'` to anchor on the frame start.

Plan-doc-only edit; P0273 is UNIMPL, the implementer will read the corrected grep.

### T44 — `refactor(harness):` dag.jsonl P0273 row compact spacing

MODIFY [`.claude/dag.jsonl`](../../.claude/dag.jsonl) — the P0273 row was written with `json.dumps` default separators (spaces after `:` and `,`). All sibling rows are compact. Parses fine but the next surgical-edit pass (which uses else-passthrough-raw-string per the convention) will emit a 1-row noise diff. Rewrite the row compact.

One-shot: `python3 -c "import json; [print(json.dumps(json.loads(l),separators=(',',':')) if json.loads(l).get('plan')==273 else l.rstrip()) for l in open('.claude/dag.jsonl')]" > /tmp/dag && mv /tmp/dag .claude/dag.jsonl`

Purely cosmetic; zero behavior change.

### T45 — `docs(harness):` ci-gate-fastpath-precedent.md missing count-bump step

MODIFY [`.claude/notes/ci-gate-fastpath-precedent.md:32-39`](../notes/ci-gate-fastpath-precedent.md). The "Mechanical steps" bash block has `ff-only` → `dag set-status` → `amend` but **no `onibus merge count-bump`**. The 8a1ed8cd commit that fixed the merger spec ([`rio-impl-merger.md:131-134`](../../.claude/agents/rio-impl-merger.md)) explicitly noted "Coordinator fast-path steps need the same swap (noted for future)" — that future is now. Bughunter mc=84 confirmed the merger spec itself IS correct post-8a1ed8cd (`amend` → `count-bump` ordering at [`:131-134`](../../.claude/agents/rio-impl-merger.md)); mc=82/83's dangling SHAs were either session-cached agent-def or deviation, NOT spec bug.

Add after `:36` (the `amend` line):

```bash
.claude/bin/onibus merge count-bump  # records post-amend HEAD in merge-shas.jsonl
```

**Coordinator's own mc=81 fast-path DID count-bump correctly** — the coord knows the step; the DOC is what's stale. This fixes the written recipe so future coord fast-paths copying from the note don't drift cadence.

### T46 — `feat(harness):` migration-number collision preflight

MODIFY [`.claude/lib/onibus/collisions.py`](../../.claude/lib/onibus/collisions.py). The P0332+P0264 both-picked-017 collision (resolved by coord renumbering P0264 to 018) wasn't catchable: different paths (`017_tenant_keys_fk_cascade.sql` vs `017_chunk_tenants.sql`), same number prefix. Path-index collision check sees no overlap.

**Option B (chosen over A):** special-case `migrations/*.sql` — extract `^0*(\d+)_` prefix, collide on NUMBER not path. Adds to the existing path-collision loop:

```python
_MIGRATION_NUM = re.compile(r"^migrations/0*(\d+)_")

def _migration_collision_key(path: str) -> str | None:
    if m := _MIGRATION_NUM.match(path):
        return f"migration:{m.group(1)}"
    return None
```

Collisions output gets a synthetic `migration:N` row when two plans' `json files` fences have different `migrations/NNN_*.sql` paths with the same `NNN`. Coordinator sees it in `onibus collisions top` before dispatch.

**Option A (dispatch-time, rejected):** implementer greps `ls migrations/` at worktree-add and picks next-free. Simpler but only fires at impl-time — too late for the coord to re-sequence.

### T47 — `refactor(scheduler):` add ema_proactive_updates_total to SCHEDULER_METRICS const

MODIFY [`rio-scheduler/tests/metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) at the `SCHEDULER_METRICS` const (currently [`:23`](../../rio-scheduler/tests/metrics_registered.rs)). `rio_scheduler_ema_proactive_updates_total` is in [`observability.md:103`](../../docs/src/observability.md) but NOT in the const array — `all_spec_metrics_have_describe_call()` can't catch a future `describe_counter!` removal. One-line add.

**p266 worktree ref — re-grep at dispatch.** The metric may not exist on sprint-1 until P0266 merges.

### T48 — `docs(store):` signing.rs:214 doc-comment overstates maybe_sign capability

MODIFY [`rio-store/src/signing.rs:202-206`](../../rio-store/src/signing.rs) (sprint-1 line; p338 worktree says `:214`). The comment says `maybe_sign` emits `key=tenant-foo-1` vs `key=cluster` — but [`mod.rs:342-343`](../../rio-store/src/grpc/mod.rs) (p338 ref) only emits a BRANCH label (`"tenant"` vs `"cluster"`), not the key name. The `(String, bool)` return carries no key name back. Fix the comment to match what `maybe_sign` actually does: "lets `maybe_sign` log which branch fired (tenant vs cluster)".

### T49 — `refactor(store):` maybe_sign — use cluster() instead of sign_for_tenant(None).expect()

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at the PG-failure fallback path (p338 worktree `:334-337`). Currently uses `sign_for_tenant(None, ...).await.expect("infallible")` — but `signer.cluster().sign(&fp)` does the same thing synchronously with no `Result` wrapper. `cluster()` was added in P0338 for `admin.rs:194`. Using it here encodes the infallibility at the type level instead of a comment.

**p338 worktree ref — re-grep at dispatch.** On sprint-1 today, `maybe_sign` is the pre-P0338 sync version at [`:273`](../../rio-store/src/grpc/mod.rs); the `.expect("infallible")` arrives with P0338.

### T50 — `feat(store):` rio_store_tenant_key_lookup_failed_total counter

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at the tenant-key lookup failure `warn!` (p338 worktree `:333`). The comment says ops should notice when a tenant WITH a configured key gets cluster-signed paths — a counter enables alerting instead of log-grep. [`observability.md`](../../docs/src/observability.md) doesn't mandate it (enhancement not spec-gap), so also add a row to the `r[obs.metric.store]` table:

| Metric | Type | Description |
|---|---|---|
| `rio_store_tenant_key_lookup_failed_total` | Counter | `maybe_sign` PG-query failed for a tenant with a configured key — fell back to cluster key. Alert if rate > 0: silent degradation of tenant isolation. |

**p338 worktree ref.** The `warn!` at `:333` arrives with P0338.

### T51 — `docs(store):` enqueue_chunk_deletes doc-comment — "actually enqueued" overstates

MODIFY [`rio-store/src/gc/mod.rs`](../../rio-store/src/gc/mod.rs) near `decrement_and_enqueue` (sprint-1 [`:499`](../../rio-store/src/gc/mod.rs); p339 worktree calls it `enqueue_chunk_deletes` at `:500`). Doc-comment says "Returns the number of keys actually enqueued" but returns `keys.len()` not `rows_affected()` — `ON CONFLICT DO NOTHING` silently skips duplicates, so the count is **attempted**, not inserted. Pre-existing semantics are correct; the new doc-comment overstates. Fix to "Returns the number of keys attempted (duplicates already-enqueued are no-ops; actual insert count may be lower)".

**p339 worktree ref — re-grep at dispatch** for exact function name and line.

### T52 — `refactor(store):` sweep_orphan_chunks — add #[instrument]

MODIFY [`rio-store/src/gc/sweep.rs:263`](../../rio-store/src/gc/sweep.rs). `sweep_orphan_chunks` has no `#[instrument]` span. After P0339's extraction, the `warn!` at [`mod.rs:519`](../../rio-store/src/gc/mod.rs) (p339 ref) emits an identical GC-prefixed message from both callers — pre-extraction, `sweep.rs` had its own prefix. Add:

```rust
#[instrument(skip(pool, chunk_backend))]
pub async fn sweep_orphan_chunks(
    // …
```

Span context disambiguates which caller's sweep emitted the warn.

**p339 worktree ref for `mod.rs:519` — re-grep at dispatch.**

### T53 — `refactor(nix):` security.nix:698 self-referencing TODO(P0260)

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) at `:698` (p260 worktree ref). The TODO is tagged `TODO(P0260)` but P0260 is the plan that COMMITTED this line — a plan can't leave a TODO tagged with its own number (plan is DONE at merge). The TODO says "fixture extraServiceEnv for RIO_JWT__KEY_PATH + pkgs.writeText with seed" — VM JWT-issue branch testing.

Either: **(a)** the VM JWT-issue branch testing is deliberately deferred (security.nix:697-701 says VM test ONLY covers the fallback branch; JWT-issue is unit-only) → delete the TODO line. **(b)** it's future work → re-tag with [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) (the wiring plan).

**Recommend (b)** — re-tag. The VM fixture work (extraServiceEnv for RIO_JWT__KEY_PATH) is natural follow-on to the scheduler/store wiring: once the interceptor is live, the VM test can exercise the full flow.

### T54 — `refactor(test-support):` fixtures.rs NIXBASE32 → pub use from rio_nix

MODIFY [`rio-test-support/src/fixtures.rs`](../../rio-test-support/src/fixtures.rs) at `:23` (p337 worktree ref). `NIXBASE32` const duplicates `rio_nix::store_path::nixbase32::CHARS` — both are `b"0123456789abcdfghijklmnpqrsvwxyz"`. The file already imports from `rio_nix` — replace with:

```rust
pub use rio_nix::store_path::nixbase32::CHARS as NIXBASE32;
```

Eliminates maintenance risk if `StorePath::parse` charset ever tightens. The doc comment claims rio-bench + property-tests need it, but grep finds zero external direct callers — `pub` is forward-looking.

### T55 — `refactor(controller):` .owns(Jobs) for reactive ephemeral re-spawn

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) at `:309` (p296 worktree ref). Controller currently only `.owns(StatefulSets)` — ephemeral mode is purely poll-driven at the 10s reconcile interval. Adding `.owns::<Job>()` tightens spawn latency from ~10s to <1s after each Job completion (kube-runtime watch fires on Job status change).

```rust
.owns(Api::<StatefulSet>::namespaced(client.clone(), &ns), watcher::Config::default())
.owns(Api::<Job>::namespaced(client.clone(), &ns), watcher::Config::default())  // NEW
```

Intentional-per-design at [`ephemeral.rs:76-89`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) (poll-based architecture chosen), but `.owns()` doesn't change poll semantics — it just adds reactive triggers. Non-blocking; forward-looking latency tuning. **p296 worktree ref — re-grep at dispatch.**

### T56 — `fix(nix):` lifecycle.nix ephemeral precondition assert ordering

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) at `~:1780` (p296 worktree ref; line drifts). The precondition self-assert `jobs_before >= 1` runs AFTER the `jobs_after >= jobs_before` check it guards. If `jobs_before=0`, the vacuous `>=` check passes FIRST (0>=0), THEN the precondition fires. Move the precondition assert to immediately after `jobs_before` capture — BEFORE the `build("${ephemeralDrv2}")` call — so failure messages point at the right problem.

```python
jobs_before = count_jobs()
assert jobs_before >= 1, f"PRECONDITION: expected ≥1 ephemeral Job before second build, got {jobs_before}"
build("${ephemeralDrv2}")
jobs_after = count_jobs()
assert jobs_after >= jobs_before, f"expected Job count non-decreasing, {jobs_before}→{jobs_after}"
```

### T57 — `refactor(gateway):` daemon_variant() — panic on unrecognized variant

MODIFY [`rio-gateway/tests/golden/daemon.rs`](../../rio-gateway/tests/golden/daemon.rs) at `:41` (p300 worktree ref). Current `_ => DaemonVariant::NixPinned` silently falls through when `RIO_GOLDEN_DAEMON_VARIANT` is set to an unrecognized value. A typo in `golden-matrix.nix` variant key would run the wrong skip-list against the wrong daemon with confusing failures. Split:

```rust
match std::env::var("RIO_GOLDEN_DAEMON_VARIANT") {
    Err(_) => DaemonVariant::NixPinned,  // unset → default
    Ok(s) => match s.as_str() {
        "nix-pinned" => DaemonVariant::NixPinned,
        "nix-stable" => DaemonVariant::NixStable,
        "nix-unstable" => DaemonVariant::NixUnstable,
        "lix" => DaemonVariant::Lix,
        other => panic!(
            "RIO_GOLDEN_DAEMON_VARIANT={other:?} unrecognized; \
             allowed: nix-pinned, nix-stable, nix-unstable, lix"
        ),
    },
}
```

### T58 — `test(gateway):` VARIANT_SKIP stale-name guard

MODIFY [`rio-gateway/tests/golden/daemon.rs`](../../rio-gateway/tests/golden/daemon.rs) near `:59` (p300 worktree ref). The `VARIANT_SKIP` table has no stale-name guard: if a test is renamed or a skip row has a typo in `test_name`, the skip silently never fires. Add a small unit test:

```rust
/// Every VARIANT_SKIP test_name must match a real #[tokio::test] fn.
/// Hand-maintained list of test names — compiler catches drift via
/// the #[allow(dead_code)] fn() pointers.
#[test]
fn variant_skip_names_are_real() {
    const REAL_TESTS: &[&str] = &[
        "test_wop_build_derivation",
        "test_wop_add_to_store_nar",
        // ... (grep '#\[tokio::test\]' to seed the list)
    ];
    for (_variant, test_name, _reason) in VARIANT_SKIP {
        assert!(
            REAL_TESTS.contains(&test_name),
            "VARIANT_SKIP refs unknown test {test_name:?} — renamed or typo?"
        );
    }
}
```

### T59 — `refactor(worker):` prepare_nix_state_dirs — take synth_db directly

MODIFY [`rio-worker/src/overlay.rs`](../../rio-worker/src/overlay.rs) at `:322` (p333 worktree ref). `prepare_nix_state_dirs` still hardcodes `nix/var/nix/db` — duplicates `upper_synth_db()` at `:126`. Refactor to take `synth_db: &Path` directly (caller passes `.upper_synth_db()`), symmetric with how `setup_nix_conf` was changed at `mod.rs:1021`. Then `mod.rs:442` passes `overlay_mount.upper_synth_db()` instead of `upper_dir()`. Completes the centralization P0333 started.

```rust
// Before:
pub fn prepare_nix_state_dirs(upper: &Path) -> io::Result<()> {
    let db = upper.join("nix/var/nix/db");  // duplicates :126
    // ...
}

// After:
pub fn prepare_nix_state_dirs(synth_db: &Path) -> io::Result<()> {
    // ...
}
```

Caller at `executor/mod.rs:442` (p333 ref): `prepare_nix_state_dirs(overlay_mount.upper_synth_db())`.

### T60 — `refactor(test-support):` unify seed_output + make_output_file

MODIFY [`rio-test-support/src/fixtures.rs`](../../rio-test-support/src/fixtures.rs). `seed_output` at [`inputs.rs:411`](../../rio-worker/src/executor/inputs.rs) and `make_output_file` at [`upload.rs:926`](../../rio-worker/src/upload.rs) (p333 worktree refs) are now byte-near-identical after P0333 converged both to the same `(TempDir, PathBuf)` tuple return. Both: tempdir + `nix/store` subdir + write file + return `(tmp, store_dir)`.

Lift to `rio-test-support/src/fixtures.rs` as `seed_store_output(content: &[u8]) -> (TempDir, PathBuf)`. Migrate both call sites. ~10-line net delta.

### T61 — `docs(harness):` dag-run SKILL — task-id `a` vs `b` discipline (FABRICATION #10-11)

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) after T35+T39's lock-stomp/4-check paragraphs. FABRICATION #10-11 (coordinator session, 4th+5th of the pattern): constructed "m-p337 merged @ a2006bd1" + "P0343 compile-broken" from ZERO source — only bg-task notifications had arrived. 4-check caught: `a2006bd1` not a valid object, mc=89 not 90, lock HELD.

Root cause: `<task-notification>` can come from EITHER an agent-subtask (task-id starts with `a`, e.g. `agent-a3760d09`) or a bash-bg-task (task-id starts with `b`, e.g. `bash-b5f2e881`). Bash-bg notifications are `.#ci` runs, coverage, etc. — NOT agent completions. Coordinator treated a bash-bg notification as merger completion.

Add to SKILL.md:

```markdown
**Task-id discipline:** `<task-notification>` task-ids prefix-discriminate
the source. Starts with `a` → agent subtask (merger, validator, impl). Starts
with `b` → bash bg-task (`.#ci` run, coverage, etc). A bash-bg notification
is NOT a merger-done signal — it's the merger's INTERNAL CI run completing.
Check the task-id prefix before treating as agent-completion.
```

Extends T35 (task-notification ≠ agent-done) + T39 (4-check discipline) with the **discriminator**.

### T62 — `docs(store):` chunked.rs test doc-comment line-refs stale by +6

MODIFY [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs) at `:445,:472,:497,:499,:504` — P0342's fix shifted `put_path_batch.rs` by +6 lines; P0345 extraction shifted further (net effect: re-grep at dispatch). Doc-comments in the `gt13_batch_placeholder_cleanup_on_midloop_abort` test reference `:275/:280/:284` which have drifted. Test mechanics unaffected (string-anchor based `find("--- Phase 2:")`); informational drift only. Sed the line-refs to current values, OR (preferred) anchor by concept name ("the `!inserted` bail! branch", "owned_placeholders.push inside the loop"). discovered_from=342.

### T63 — `refactor(common):` tonic-health dev-dep redundant

MODIFY [`rio-common/Cargo.toml`](../../rio-common/Cargo.toml) at `:78-80`. P0343 added `tonic-health = { workspace = true }` to normal `[dependencies]` at `:38` (for the shared `spawn_health_plaintext` helper). The dev-dep at `:80` + its comment at `:78-79` ("Health service used as a trivial gRPC service...") is now redundant — Cargo dedups silently; cruft only. Delete `:78-80`. discovered_from=343.

### T64 — `refactor(harness):` _PHANTOM_AMEND_FILES — dead merge-shas.jsonl entry

MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at `:171-174`. `_PHANTOM_AMEND_FILES` contains `.claude/state/merge-shas.jsonl` — gitignored (`.gitignore:50` `.claude/state/`), NEVER in any commit, can never appear in `git diff --name-only`. Dead entry. [`merge.py:168`](../../.claude/lib/onibus/merge.py) confirms "merge-shas.jsonl ... in state/ (gitignored)". Drop to `frozenset({".claude/dag.jsonl"})`. Also fix docstring at `:189` + [`models.py:480`](../../.claude/lib/onibus/models.py) description that propagate the misleading "dag.jsonl/merge-shas.jsonl" claim. discovered_from=346.

### T65 — `fix(harness):` phantom_amend truthiness guard + empty-tree amend test

MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at `:220`:

```python
if amend_diff and amend_diff <= _PHANTOM_AMEND_FILES:
```

The `amend_diff and` truthiness guard makes empty-set short-circuit `False`. But empty-set IS a subset. Scenario: [`merger.md:146`](../../.claude/agents/rio-impl-merger.md) — "If the row was already DONE, `dag set-status` is a no-op, `git add` stages nothing, `--amend --no-edit` rewrites with identical tree — harmless." That no-op amend STILL produces a new SHA (new committer timestamp), orphans worktrees identically. `amend_diff = diff(pre_amend, post_amend) = ∅` → detection misses. Fix: drop the `and` guard → `if amend_diff <= _PHANTOM_AMEND_FILES:` (empty-set subset always `True`; msg-check still gates). No false-positive risk: same-msg + identical-tree IS definitionally a phantom.

Also add test in [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) after `test_behind_check_phantom_amend_detection` (`~:1600`):

```python
def test_behind_check_phantom_amend_identical_tree(tmp_repo: Path):
    """merger.md:146 no-op amend: row already DONE → dag set-status no-op
    → git add stages nothing → amend --no-edit rewrites with identical
    tree. Still orphans worktrees (new SHA, new committer timestamp).
    amend_diff is ∅; with the pre-fix truthiness guard this was missed."""
    # Setup same as phantom_amend_detection but with NO file change
    # between pre_amend and post_amend (just `git commit --amend
    # --no-edit` on an unchanged tree). Assert phantom_amend=True.
    ...
```

The test documents the gap AND makes it pass after the guard fix. Coupled — land together. discovered_from=346.

### T66 — `docs(harness):` dag-run SKILL — FABRICATION #12 lock-released mid-merger stomp

MODIFY [`.claude/skills/dag-run/SKILL.md`](../../.claude/skills/dag-run/SKILL.md) after T61's task-id-prefix paragraph. FABRICATION #12 (6th same-session, same error-class as #6/7/9/10/11): constructed "m-p266-r2 merged @ d30c4d14 mc=99" from lock-released+sprint-1-advanced signal. Merger was STILL RUNNING (no report in output file, no mc=99 in merge-shas). Coordinator stomped sprint-1 mid-merger (amended 074288ae→534a00dc during its CI). Same P0262-lesson violation. Also used invalid `HELD` status (should be `RESERVED`).

Extends T39's 4-check discipline with a specific precondition:

```markdown
**Lock-released WITHOUT notification = merger still running.** The
merger's `_LEASE_SECS` lease can auto-expire mid-CI-retry without the
merger having exited. Before treating lock-released as merger-done:
(1) check the output file for a MergerReport, (2) check merge-shas.jsonl
for the expected mc bump, (3) check `ps aux | grep merger-agent-<id>`.
All three must confirm before proceeding. If lock released but no
report → WAIT for the task-notification, do NOT stomp sprint-1.
```

Also note the `json.dumps` all-349-rows noise + invalid `HELD` status pitfall in the same block (both were side-effects of the same stomp). discovered_from=coordinator (fabrication #12, filed 2× as rows 14+15, dedupe here).

### T67 — `docs(store):` put_path_batch.rs line-cite drift (3 sites)

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) at `:347`, `:373` — comment line-cites to `put_path.rs` drifted: `:347` says "put_path.rs:629-641" (actual ~648-668, content_index::insert); `:373` says "put_path.rs:645" (actual :672, bytes_total). Both point at the right concept/file; line numbers stale. **Post-P0345:** the `:160` path_not_in_claims ref was extracted to `mod.rs::validate_put_metadata` and no longer appears here.

**Low-value drift.** Per the CITATION-TIERING principle (bughunter-mc56): these are contextual-only cites (reader greps `put_path.rs` for `path_not_in_claims`/`content_index::insert`/`bytes_total`). Bare line numbers are fine to drift. **Fix OR delete the line numbers** — either update to current, or rewrite as "see `put_path.rs` `path_not_in_claims` block" (function-name anchor doesn't drift). discovered_from=344.

### T68 — `docs(nix):` fod-proxy.nix globalTimeout comment stale

MODIFY [`nix/tests/scenarios/fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) at `:78` — comment says "WorkerPool patch + STS rollout (~30s)". The kubectl-patch was removed in P0309 (DONE); no patch, no rollout wait. 900s still correct just over-budgeted. Correct to "k3s bring-up ~4min + 3 short builds". discovered_from=309.

### T69 — `refactor(common):` jwt_interceptor.rs test inlines encode_pubkey_file

MODIFY [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) at `:542-545` — `load_jwt_pubkey_from_file` test inlines base64+newline encoding; the new `encode_pubkey_file` helper at `:532` (P0349) does exactly this and is used by `sighup_swaps_pubkey`. 3-line consolidation: replace the inline encoding with a `encode_pubkey_file(&pk)` call. Post-P0349-merge. discovered_from=349.

### T70 — `refactor(gateway):` ratelimit.rs TODO(phase5) moot — remove or re-tag

MODIFY [`rio-gateway/src/ratelimit.rs`](../../rio-gateway/src/ratelimit.rs) at `:8` — `TODO(phase5)` breaks the P0NNN tag convention AND the eviction concern is moot (per threat-model: rate-key = `Claims.sub`, bounded keyspace, P0261 RESERVED/DEAD). Remove the TODO entirely OR re-tag `TODO(P0261)` with a note that P0261 is RESERVED because sub-keying obviates it. discovered_from=213.

### T71 — `fix(gateway):` ensure_permit error format — literal "max" in placeholder

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) at `:575` — `ensure_permit` error formats literal string `"max"` into `{}` placeholder, producing "connection cap reached (max concurrent SSH connections)". Either:
- Store the cap value on `ConnectionHandler` and display the real number: `"connection cap reached ({n} concurrent SSH connections)"`, OR
- Drop the `({})` entirely: `"connection cap reached"` — the operator checks `gateway.toml max_connections` for the value.

The second is simpler (no struct field add). discovered_from=213.

### T72 — `docs(store):` MetadataError::ResourceExhausted docstring — sig-cap not retriable

MODIFY [`rio-store/src/metadata/mod.rs`](../../rio-store/src/metadata/mod.rs) at `:114` — docstring says "Retriable" and bundles pool-timeout (transient, retry helps) with sig-cap (per-path permanent — UPDATE already committed, `DISTINCT` dedup means retry hits same cardinality>cap forever). Sig-cap is closer to `FAILED_PRECONDITION` than `RESOURCE_EXHAUSTED`. At minimum: the docstring should split the two cases — "Pool-timeout: retriable. Sig-cap: per-path permanent; retry hits same cardinality." discovered_from=213.

### T73 — `docs:` blank-line-after-marker sweep — 55 siblings of P0320 defect

MODIFY [`docs/src/components/store.md`](../../docs/src/components/store.md) and **5 other spec files** — 55 `r[...]` markers have blank-line-between-marker-and-text (P0320 defect class). `tracey` sees empty text → `tracey bump` is silent no-op. P0350 fixed 1 (`store.chunk.tenant-scoped` at `:89`); 12 remain in store.md, 43 in other spec files. Verified via `tracey query rule store.chunk.grace-ttl` → empty text output.

One-line removal per marker: delete the blank line AFTER `r[...]` so the text paragraph is adjacent. **Work bottom-up** within each file (deleting lines shifts numbers). P0304-T27 already fixed `r[sched.ca.*]` siblings (3); this sweep covers the remaining 55.

**Scope discovery at dispatch:** `for m in $(grep -rn '^r\[' docs/src/ | cut -d: -f3); do tracey query rule "${m:2:-1}" | head -1 | grep -q 'text: ""' && echo $m; done` — finds markers with empty text. Or: `awk 'prev~/^r\[/ && /^$/ {print FILENAME":"FNR": blank after "prev} {prev=$0}' docs/src/**/*.md` — finds blank-line-after-marker occurrences directly.

discovered_from=350. Coordinate with P0295-T31 (same fix class, different file — dashboard.md) and P0304-T27 (scheduler.md sched.ca.* — already staged in this plan).

### T74 — `fix(nix):` mutants `|| true` swallows real failures — check exit code explicitly

MODIFY [`flake.nix`](../../flake.nix) at `:807-814`. Current buildPhaseCargoCommand ends `|| true` — swallows ALL non-zero exits including exit 1 (real cargo/config/env failure), not just exit 2 (mutants survived — expected). A broken mutants.toml, missing dep, or cargo-mutants crash would produce a GREEN derivation with `caught-count=0` `missed-count=0` and the weekly diff would silently think "perfect, zero mutations". Exit 2 is the ONLY expected non-zero; everything else should fail the build.

```nix
buildPhaseCargoCommand = ''
  cargo mutants \
    --in-place --no-shuffle \
    --config .config/mutants.toml \
    --output $out \
    --timeout-multiplier 2.0 \
    || { rc=$?; [ "$rc" -eq 2 ] || exit "$rc"; }
'';
```

The `|| { ... }` block runs only on non-zero; `rc=$?` captures it immediately; exit-2 (survived mutants, expected) continues; anything else re-propagates. discovered_from=bughunter(mc112).

### T75 — `fix(tooling):` justfile mutants summary — `$()` not expanded by just

MODIFY [`justfile`](../../justfile) at `:27-29`. The summary echo lines use `$(jq ...)` — `just`'s recipe-level `$` is a just-variable reference, not shell command substitution. To pass `$` through to the shell, just requires `$$`. Current lines:

```make
@echo "Caught:   $(jq '[.outcomes[] | select(.summary == \"CaughtMutant\")] | length' mutants.out/outcomes.json)"
```

…print the LITERAL string `Caught:   $(jq ...)` (or fail, depending on just version's handling of unrecognized `$(...)`). Fix — escape the `$`:

```make
@echo "Caught:   $$(jq '[.outcomes[] | select(.summary == \"CaughtMutant\")] | length' mutants.out/outcomes.json)"
@echo "Missed:   $$(jq '[.outcomes[] | select(.summary == \"MissedMutant\")] | length' mutants.out/outcomes.json)"
```

**CHECK AT DISPATCH:** `just --dry-run mutants` and inspect the rendered shell — verify `$$(jq ...)` becomes `$(jq ...)` in the shell command. discovered_from=bughunter(mc112).

### T76 — `refactor(nix):` extract goldenTestEnv attrset — 4× RIO_GOLDEN_* duplication

MODIFY [`flake.nix`](../../flake.nix). The `RIO_GOLDEN_TEST_PATH` / `RIO_GOLDEN_CA_PATH` / `RIO_GOLDEN_FORCE_HERMETIC` / `NEXTEST_HIDE_PROGRESS_BAR` quartet appears at ≥3 sites: nextest check (~`:392-394`), nextest coverage (~`:527-528`), mutants (~`:834-837`), plus [`nix/golden-matrix.nix:63-66`](../../nix/golden-matrix.nix). 12 lines total across sites. Extract to a single attrset in the flake's `let`:

```nix
goldenTestEnv = {
  RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
  RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
  RIO_GOLDEN_FORCE_HERMETIC = "1";
  NEXTEST_HIDE_PROGRESS_BAR = "1";
};
```

…then `// goldenTestEnv` at each call site. For `golden-matrix.nix`, pass `goldenTestEnv` as an extra argument (instead of the two separate `goldenTestPath`/`goldenCaPath` — or keep both if matrix needs per-variant path overrides; check at dispatch). The dedupe also couples mutants+golden-matrix so a new golden fixture env var is added in ONE place. discovered_from=consolidator(mc110).

### T77 — `fix(scheduler):` builds.jwt_jti dead column — wire INSERT in insert_build

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) at `:374-401` (`insert_build`). Migration 016 added `builds.jwt_jti TEXT NULL` column (tested at `:2628-2643`), and [`r[gw.jwt.issue]`](../../docs/src/components/gateway.md) at `:491` says "the `SubmitBuild` handler INSERTs `jti` into `builds.jwt_jti`" — but `insert_build`'s INSERT statement at `:384-387` lists only `(build_id, tenant_id, requestor, status, priority_class, keep_going, options_json)`. **The column is dead** — nothing writes to it. Audit trail silently empty.

Add `jti: Option<&str>` parameter and include in INSERT:

```rust
pub async fn insert_build(
    &self,
    build_id: Uuid,
    tenant_id: Option<Uuid>,
    priority_class: crate::state::PriorityClass,
    keep_going: bool,
    options: &crate::state::BuildOptions,
    jti: Option<&str>,  // NEW — JWT ID for audit trail per r[gw.jwt.issue]
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO builds
            (build_id, tenant_id, requestor, status, priority_class,
             keep_going, options_json, jwt_jti)
        VALUES ($1, $2, '', 'pending', $3, $4, $5, $6)
        "#,
    )
    .bind(build_id)
    .bind(tenant_id)
    .bind(priority_class.as_str())
    .bind(keep_going)
    .bind(sqlx::types::Json(options))
    .bind(jti)  // NULL when Claims absent (dual-mode fallback)
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

Update the SubmitBuild handler caller (grep `insert_build` in [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) — single call site) to pass `jwt_claims.as_ref().map(|c| c.jti.as_str())`. The `jwt_claims` local is already computed at `:498` for the revocation check — reuse it. Other test-fixture callers (logs/flush.rs:434, admin/tests.rs, actor/tests/recovery.rs) pass `None`.

**Schema test at `:2643`** already asserts the column exists+nullable — no change there. Add an assertion in the SubmitBuild integration test (grep `insert_build\|SubmitBuild` in `rio-scheduler/src/grpc/tests.rs`) that reads back `jwt_jti` and checks it matches the Claims jti when JWT mode is active.

discovered_from=bughunter(mc112,T1). Serves `r[gw.jwt.issue]` — the spec already mandates this; the code was orphaned.

### T78 — `docs(tooling):` mutants.toml examine_globs expansion candidates

MODIFY [`.config/mutants.toml`](../../.config/mutants.toml) after `:29`. The current `examine_globs` has 7 entries (scheduler state machine, assignment, wire primitives, framed, aterm, hmac, manifest). Bughunter mc=112 noted several expansion candidates for the NEXT weekly-baseline pass — add them as commented entries so the scope expansion is staged:

```toml
# ─── Expansion candidates (uncomment after baseline stabilizes) ───
#   # Scheduler dispatch heuristics. Comparisons galore.
#   "rio-scheduler/src/actor/dispatch.rs",
#   # Store GC sweep. Batch-size boundary arithmetic.
#   "rio-store/src/gc/sweep.rs",
#   # Worker overlay mount path construction. String slicing.
#   "rio-worker/src/overlay.rs",
#   # JWT interceptor verify logic. Expiry comparison.
#   "rio-common/src/jwt_interceptor.rs",
```

These are **candidates**, not enables — uncomment one per weekly cycle, check the survived-count delta, promote if high-signal. discovered_from=bughunter(mc112).

### T79 — `fix(ci):` weekly.yml mutants job decouple from golden-matrix failure

MODIFY [`.github/workflows/weekly.yml`](../../.github/workflows/weekly.yml) at `:56`. The `mutants` job has `needs: golden-matrix` for one-runner-at-a-time sequencing (the comment at `:51-52` says "serial ... mutants is the slower job so it goes second"). Since [P0348](plan-0348-golden-matrix-input-auto-bump.md) added `--override-input nix-unstable github:NixOS/nix` to golden-matrix (`:40-42`), an upstream Nix regression breaks golden-matrix — and because GitHub Actions skips `needs:`-dependent jobs when the dependency fails, the mutants job is silently skipped. But mutants runs against LOCKED inputs (`.#mutants` doesn't override), so there's no reason for upstream breakage to skip it.

Add `if: always() && !cancelled()` to the mutants job (after `:56`):

```yaml
  mutants:
    runs-on: rio-ci
    timeout-minutes: 240
    needs: golden-matrix
    if: always() && !cancelled()
```

`needs:` stays (preserves sequencing — still want one job on the runner at a time). `if: always() && !cancelled()` makes mutants run regardless of golden-matrix outcome, but still respects workflow cancellation. The failure-coupling was accidental: `needs:` was for ordering, not gating. discovered_from=consolidator(mc115).

### T80 — `fix(infra):` device-plugin DaemonSet liveness probe

MODIFY [`infra/helm/rio-build/templates/device-plugin.yaml`](../../infra/helm/rio-build/templates/device-plugin.yaml) (NEW in [P0286](plan-0286-privileged-hardening-device-plugin.md) T1). The smarter-device-manager DaemonSet has no liveness probe — a hung registration (kubelet device-plugin gRPC socket unresponsive) leaves the pod Ready but non-functional, and worker pods with `resources.limits["smarter-devices/fuse"]` stay Pending indefinitely.

Add after the container spec's `volumeMounts` block (grep `volumeMounts` at dispatch — review finding cited `:73`):

```yaml
          livenessProbe:
            # smarter-device-manager exposes no HTTP; probe the kubelet
            # device-plugin socket directly. Presence proves registration
            # completed; staleness is caught by periodSeconds re-probe.
            exec:
              command:
                - /bin/sh
                - -c
                - test -S /var/lib/kubelet/device-plugins/smarter-fuse.sock
            initialDelaySeconds: 10
            periodSeconds: 30
```

**CHECK AT DISPATCH:** the exact socket filename (`smarter-fuse.sock` vs `smarter_fuse.sock` vs per-device naming) — grep the smarter-device-manager upstream source or `kubectl exec` into a running pod in a dev cluster to verify. The image is minimal; if `/bin/sh` isn't present, use a gRPC health check instead (smarter-device-manager's kubelet registration socket speaks the [device plugin gRPC API](https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/device-plugins/)). Post-P0286-merge. discovered_from=286(review).

### T81 — `docs(gateway):` human_bytes doc-comment — "rounds up" is false

MODIFY [`rio-gateway/src/quota.rs`](../../rio-gateway/src/quota.rs) at `:200-201`. Docstring says "one decimal place, rounds up at the decimal so '1.0 GiB' means at least 1073741824 bytes." **False.** The implementation uses `{v:.1}` which rounds to nearest (Rust's default float formatting). `1019 * 1024 * 1024` bytes = 0.995 GiB → displays as `"1.0 GiB"` but is BELOW `1073741824`. The claim "means at least" is exactly backwards for round-to-nearest.

Either fix the doc to say "rounds to nearest at one decimal place" (truthful), OR change the format to actually round up if the "at least" semantics are load-bearing for the quota message (e.g., `format!("{:.1}", (v * 10.0).ceil() / 10.0)`). The first is simpler and the UX difference (showing 1.0 vs 0.9 GiB for a value in between) is immaterial — quota is soft, the exact display doesn't affect enforcement. discovered_from=255.

### T82 — `refactor(gateway):` quota.rs tracing::warn! field convention — %status.message() → %status

MODIFY [`rio-gateway/src/quota.rs`](../../rio-gateway/src/quota.rs) at `:136-141`. Current:

```rust
tracing::warn!(
    tenant = %tenant_name,
    code = ?status.code(),
    error = %status.message(),
    "TenantQuota RPC failed; skipping quota gate (fail-open)"
);
```

Project convention (per scheduler's actor/ modules at [`completion.rs:27-78`](../../rio-scheduler/src/actor/completion.rs), [`build.rs:84-555`](../../rio-scheduler/src/actor/build.rs)) is `error = %e` — capture the whole error via Display, not split code+message fields. `tonic::Status` implements Display as `"status: <code>, message: <message>"` — one field captures both. Also drop the `tracing::` prefix (bare `warn!` — the macro is already imported at crate-level).

```rust
warn!(
    tenant = %tenant_name,
    error = %status,
    "TenantQuota RPC failed; skipping quota gate (fail-open)"
);
```

discovered_from=255.

### T83 — `test(gateway):` GATEWAY_METRICS const missing jwt_mint_degraded + quota_rejections

MODIFY [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) at `:6-15`. The `GATEWAY_METRICS` const lists 8 spec'd metric names; [`rio-gateway/src/lib.rs:66-70`](../../rio-gateway/src/lib.rs) `describe_metrics` now registers 10 (P0255 added `rio_gateway_quota_rejections_total`; P0260 added `rio_gateway_jwt_mint_degraded_total`). The `all_spec_metrics_have_describe_call` test still passes (subset is fine) but the const is stale — it's meant to mirror [`observability.md`](../../docs/src/observability.md)'s Gateway Metrics table.

Add the two missing entries:

```rust
const GATEWAY_METRICS: &[&str] = &[
    "rio_gateway_connections_total",
    "rio_gateway_connections_active",
    "rio_gateway_opcodes_total",
    "rio_gateway_opcode_duration_seconds",
    "rio_gateway_handshakes_total",
    "rio_gateway_channels_active",
    "rio_gateway_errors_total",
    "rio_gateway_bytes_total",
    "rio_gateway_jwt_mint_degraded_total",   // P0260
    "rio_gateway_quota_rejections_total",    // P0255
];
```

**Also verify** [`observability.md`](../../docs/src/observability.md)'s Gateway Metrics table has both rows — if not, add them (sibling of T26's scheduler-metric addition). The const is DOWNSTREAM of the spec table; both must be in sync. discovered_from=255.

### T84 — `refactor(store):` apply_trailer vestigial [u8; 32] return — both callers discard via `if let Err`

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at `:194-198`. [P0345](plan-0345-put-path-validate-metadata-helper.md) (DONE) landed `apply_trailer` with `-> Result<[u8; 32], Status>` and docstring "Returns the applied hash ... for callers that need it separately." But BOTH call sites use `if let Err(e) = apply_trailer(...)` — neither binds the `Ok` value:

- [`put_path.rs:495`](../../rio-store/src/grpc/put_path.rs): `if let Err(e) = apply_trailer(&mut info, &t, "PutPath") {`
- [`put_path_batch.rs:209`](../../rio-store/src/grpc/put_path_batch.rs): `if let Err(e) = apply_trailer(info, t, &format!(...)) {`

The `[u8; 32]` return is dead. Change `-> Result<[u8; 32], Status>` → `-> Result<(), Status>`; change `Ok(hash)` → `Ok(())`. Docstring: delete the "Returns the applied hash" sentence; if content-index pre-collection (P0344) ever needs the hash, callers read `info.nar_hash` after the helper (same value, no return-plumbing). discovered_from=345-review (P0345 merged during planning; retargeted from plan-doc to landed code).

### T85 — `docs(store):` M_018 "PutChunk wraps both in BEGIN..COMMIT" claim wrong — chunk.rs says autocommit

MODIFY [`rio-store/src/migrations.rs`](../../rio-store/src/migrations.rs) at `:50-59`. [P0353](plan-0353-sqlx-migration-checksum-freeze.md) (DONE) landed `M_018` with:

```rust
/// ## "No race" — true, but not for the stated reason
/// ... The "no race" property holds at *application* time
/// (`PutChunk` wraps both inserts in one `BEGIN..COMMIT`), not at
/// *migration* time.
```

But [`chunk.rs:262-263`](../../rio-store/src/grpc/chunk.rs) explicitly says: "Not in a single transaction with the chunks insert: the chunks row has already committed (autocommit)." `PutChunk` does NOT use `BEGIN..COMMIT` — two autocommit statements. The `M_018` doc-const inherited the ORIGINAL `.sql` comment's same-txn mistake (what P0295-T40 was trying to fix before P0353 moved it). Rewrite `:56-58`:

```rust
/// statements). The "no race" property holds at *application* time
/// because `PutChunk` does `INSERT chunks` (autocommit) THEN `INSERT
/// chunk_tenants` (autocommit) — two separate statements, chunks
/// commits BEFORE junction, FK satisfied sequentially. If the
/// junction insert fails, the chunk row stands unattributed; tenant's
/// next FindMissingChunks says "missing" → retry self-heals. See
/// chunk.rs:262-269 for the honest code-side note. Not one txn; not
/// atomic; sequential-commit is the actual reason no-race holds.
```

discovered_from=353-review (P0353 merged during planning; retargeted from plan-doc to landed `migrations.rs`).

### T86 — `refactor(flake):` helm-lint checkPhase /tmp → $TMPDIR consistency

MODIFY [`flake.nix:498-573`](../../flake.nix) — the helm-lint JWT mount assertions (landed with [P0357](plan-0357-helm-jwt-pubkey-mount.md)) write `jwt-on.yaml`/`jwt-off.yaml` to `/tmp/` while the earlier chart setup at `:469-471` uses `$TMPDIR/chart`. Both work in the Nix sandbox (`/tmp` is writable), but it's inconsistent — everything else in that derivation anchors on `$TMPDIR`. Change `:498`, `:503`, `:518`, `:527`, `:536`, `:548`, `:556`, `:570-573` (all `/tmp/jwt-*.yaml` refs) → `$TMPDIR/jwt-*.yaml`. No behavior change; pure convention. discovered_from=357-review.

### T87 — `refactor(test-support):` DescribedByType counters()/gauges() — zero callers

MODIFY [`rio-test-support/src/metrics.rs:93-100`](../../rio-test-support/src/metrics.rs) — `DescribedByType::counters()` and `::gauges()` have zero callers; only `::histograms()` is used (by `assert_histograms_have_buckets` at `:336`). Landed with [P0321](plan-0321-build-graph-edges-histogram-buckets.md) as speculative test-support API.

Two options: (a) delete `:93-100` (both accessors) and shrink `NamesByType` to `Vec<String>` (histogram-only); (b) keep them but add `#[allow(dead_code)]` + doc-comment "for future per-type assertions". Prefer (a) — they're trivially re-added if a `assert_counters_described`-style helper ever materializes; keeping unused pub API in test-support signals a capability that doesn't exist. If the `Recorder` impl at `:103-122` pushes to `.0`/`.1` tuple indices, simplify to `Vec<String>`. discovered_from=321-review.

### T88 — `fix(coverage):` E0583 rio-store migrations module — stale cargoArtifactsCov

The coverage build ([`flake.nix:293-300`](../../flake.nix)) hit `E0583 file not found for module migrations` after [P0353](plan-0353-migration-checksum-freeze.md) added [`rio-store/src/migrations.rs`](../../rio-store/src/migrations.rs) + `pub mod migrations;` at [`lib.rs:19`](../../rio-store/src/lib.rs). Both files exist on sprint-1; `cargo nextest run` passes. The coverage drv alone fails.

**Hypothesis:** `cargoArtifactsCov = craneLib.buildDepsOnly covArgs` at [`flake.nix:293`](../../flake.nix) was cached before P0353 landed. `buildDepsOnly` strips workspace source and builds deps against a stub `main.rs`/`lib.rs`; the cached deps drv didn't know about the new module. On rebuild, `rio-workspace-cov` unpacks fresh source (has `pub mod migrations`) but links against the stale deps artifact that compiled `rio-store` without it.

This shouldn't normally happen — crane's `buildDepsOnly` only caches `Cargo.lock`-derived deps, not workspace crates. But `covArgs` may differ from `commonArgs` in a way that changes the drv hash only on `Cargo.lock` changes, not `src` changes. Check: does `covArgs` inherit the full `commonArgs.src` fileset, or a narrower one? If `covArgs // { cargoArtifacts = ...; }` merges but `covArgs` was defined with a different `src`, that's the leak.

**Fix candidate (a):** if `covArgs` derives from `commonArgs`, this is a one-off cache-poison — `/nixbuild` with a cleared cache or a no-op `covArgs` bump invalidates it. Add a comment at `:293` noting the E0583 hazard. **Fix candidate (b):** if `covArgs` has its own `src` fileset that excludes something, align it with `commonArgs.src`. **Fix candidate (c):** if the issue is that `buildDepsOnly` isn't re-running when a new workspace `.rs` file is added (unlikely — source is in the fileset), document the constraint.

Inspect `covArgs` definition (grep `covArgs =` in flake.nix) at dispatch and pick the smallest fix. discovered_from=coverage (coord, backgrounded `.#coverage-full` run).

### T89 — `refactor(flake):` sqlx-prepare-check hook — extend files regex to migrations

MODIFY [`flake.nix`](../../flake.nix) — the `sqlx-prepare-check` pre-commit hook (arrives with [P0297](plan-0297-sqlx-query-macro-conversion.md)) has `files = "\\.rs$"`. Migration-only commits (`*.sql`) don't fire the hook, so a schema change that stales `.sqlx/*.json` slips through to CI.

The hook's internal check (`SQLX_OFFLINE=true cargo check`) validates cache↔rust, not cache↔schema — a live-PG connection would be needed for the latter, which isn't practical pre-commit. So extending the files regex to `"\\.(rs|sql)$"` and printing a reminder (`echo 'migration changed — run \`just sqlx-prepare\` if column types/names affected'`) is the pragmatic middle ground: fire the reminder on `.sql` changes, still run the cargo-check on `.rs` changes. Alternatively leave as-is and accept CI as the catch (dev-ergonomics tradeoff).

Prefer the reminder approach — adding `files = "\\.(rs|sql)$"` plus a branch in the entry script that checks `$@` for `.sql` and prints a heads-up without running `cargo check` (cheap, informative). discovered_from=297-review. **Gated on P0297 merge** — hook doesn't exist on sprint-1 yet.

### T90 — `docs(store):` grpc/mod.rs fault-line marker — put_common.rs extraction threshold

MODIFY [`rio-store/src/grpc/mod.rs:124`](../../rio-store/src/grpc/mod.rs) — add a `// NOTE(fault-line):` comment above `validate_put_metadata` at `:124`:

```rust
// NOTE(fault-line): validate_put_metadata (+81L) and apply_trailer (+29L)
// are pub(super), called ONLY from put_path.rs + put_path_batch.rs. This
// file reached 922L (collision=16) after P0345. Extraction to
// grpc/put_common.rs (~110L) makes sense IF: a 3rd PUT-variant RPC lands,
// OR mod.rs crosses 1000L, OR collision count hits 20+. NOT doing it now:
// churn-on-churn immediately post-P0345, no in-flight plans touching the
// file, RPC handlers at :471-813 remain cohesive. The fault line is
// validate_put_metadata+apply_trailer → put_common.rs; tenant_quota and
// StoreService trait impls stay.
```

This is a consolidator's **deferred** finding — not proposing the extraction now, just marking where to cut when the threshold trips. The next plan that touches `grpc/mod.rs` for a PUT-adjacent RPC sees this comment and decides. discovered_from=consolidator-mc125.

### T91 — `refactor(store):` sign_for_tenant dead-code collapse — resolve_once supersedes

MODIFY [`rio-store/src/signing.rs:224-242`](../../rio-store/src/signing.rs) — [P0352](plan-0352-putpathbatch-hoist-signer-lookup.md) introduced `resolve_once` at [`:257`](../../rio-store/src/signing.rs) + `sign_with_resolved` at [`grpc/mod.rs:392`](../../rio-store/src/grpc/mod.rs) as the batch-friendly entry. Post-P0352, `sign_for_tenant` has **zero production callers** — [`admin.rs`](../../rio-store/src/grpc/admin.rs) doesn't call it (it uses [`cluster()`](../../rio-store/src/signing.rs) for ResignPaths backfill per `:200-207`). Only the 3 unit tests at `:561`, `:605`, `:638` call `sign_for_tenant` directly.

The fn is now a 19-line convenience wrapper equivalent to `self.resolve_once(tenant_id).await.map(|(s, t)| (s.sign(fp), t))`. Two options:

- **(a)** Delete `sign_for_tenant` entirely. Migrate the 3 tests to `resolve_once(tid).await?.0.sign(fp)` — same coverage, tests the production path. The [`:209 r[impl store.tenant.sign-key]`](../../rio-store/src/signing.rs) annotation moves to `resolve_once` (same marker, same fallback semantics).
- **(b)** Keep it as a test-only convenience — add `#[cfg(test)]` to the fn, drop the doc-comment, keep `r[impl]` on `resolve_once`.

Prefer **(a)**: the tests currently exercise `sign_for_tenant` not `resolve_once` — migrating them gives `resolve_once` direct unit coverage (currently only tested transitively via PutPathBatch integration). Delete `:224-242` (fn body), update `:252` doc-comment ("Same fallback semantics as `sign_for_tenant`" → drop the reference or inline the 3-path fallback note), update the 3 tests. Net `~-15` lines. discovered_from=352-review.

### T92 — `refactor(common):` EmptyTenantName Display — wire callers or drop claim

MODIFY [`rio-common/src/tenant.rs:41-45`](../../rio-common/src/tenant.rs) — the `#[error("tenant name is empty or whitespace-only")]` Display impl has **zero callers that format the error**. Doc-comment at `:41` says "The `Display` impl's message is what callers map to `Status::invalid_argument`" — but [`rio-scheduler/src/admin/mod.rs:727`](../../rio-scheduler/src/admin/mod.rs) does `.map_err(|_| Status::invalid_argument("tenant_name is required"))` (discards, hardcodes). Same pattern in other TryFrom sites.

Two options (both fine; pick at dispatch):

- **(a)** Make callers use it: `.map_err(|e| Status::invalid_argument(e.to_string()))` — single source of truth for the message, doc-comment becomes true. Grep `EmptyTenantName` for TryFrom sites (currently 2-3).
- **(b)** Drop the `#[error(...)]` attribute and doc-comment claim. `EmptyTenantName` becomes a pure marker error (Debug only). Callers keep their hardcoded strings. Smaller diff.

Prefer **(a)**: the message in the error struct is better-worded ("empty or whitespace-only" is more precise than "is required"). Harmonizes client-facing errors. 2-3 line callsite change + drop the stale "is what callers map" phrasing. discovered_from=217-review.

### T93 — STRUCK (premise refuted by QA docs-977731)

Originally: "MockStore tenant_quota should use NormalizedName not inlined trim — rio-test-support already depends on rio-common."

**Premise FALSE** (qa-docs-977731, 2026-03-20): `rio-test-support/Cargo.toml` has NO `rio-common` entry. `metrics.rs:321-322` explicitly reads `"pass it through so this crate stays leaf (no rio-common dep)"` — HISTOGRAM_BUCKET_MAP is passed by callers, not imported. The "doesn't depend" comment at `grpc.rs:525-526` is CORRECT, not stale. rev-p217 finding was wrong.

The mock-divergence risk (mock trim differs from P0298's future tightening) is real but the proposed fix would convert a deliberate leaf-crate to non-leaf. If P0298 tightens semantics and mock-divergence bites, THEN add rio-common dep as a DESIGN CHANGE not a stale-comment fix. discovered_from=217-review (refuted), qa-docs-977731.

### T94 — `refactor(infra):` helm tls-helpers self-guard — match jwt/cov/rustLog pattern

MODIFY [`infra/helm/rio-build/templates/_helpers.tpl:45-64`](../../infra/helm/rio-build/templates/_helpers.tpl) — the three TLS helpers (`rio.tlsEnv`, `rio.tlsVolumeMount`, `rio.tlsVolume`) are **not self-guarded** on `.Values.tls.enabled`. Every caller wraps them manually: [`controller.yaml:59,67,86`](../../infra/helm/rio-build/templates/controller.yaml), [`gateway.yaml:61,73,111`](../../infra/helm/rio-build/templates/gateway.yaml), [`scheduler.yaml:97,...`](../../infra/helm/rio-build/templates/scheduler.yaml), same in store/worker.

Contrast: `rio.jwtVerify*` at `:82-103`, `rio.jwtSign*` at `:113-134`, `rio.rustLogEnv` at `:140-145`, `rio.cov*` at `:168+` are all self-guarded — callers include unconditionally with `| nindent N`.

Add `{{- if .Values.tls.enabled }}` wraps to the 3 defines (same shape as jwt helpers). Then drop the caller-side `{{- if .Values.tls.enabled }}` guards across all 5 component templates (~15 sites). Net negative. **Paired-with-T86** (same checkPhase helm-lint asserts exercise the rendered templates). discovered_from=consolidator-mc130.

**CARE: `rio.tlsVolume` takes an argument** (secret name via `{{ . }}` at `:63`). Self-guard needs `.Values.tls.enabled` but `.` inside the template is the passed secret-name string, not the root context. Pass a 2-element list `(list . "rio-controller-tls")` or use a dict with `root`+`name`; simpler: change signature to `include "rio.tlsVolume" (dict "root" . "name" "rio-controller-tls")` and access `.root.Values.tls.enabled` + `.name` inside. Check existing call pattern at dispatch — if all callers already pass the root context somewhere, use that.

### T95 — `refactor(scheduler):` RebalancerConfig missing `Deserialize` — can't load from TOML

MODIFY [`rio-scheduler/src/rebalancer.rs:44`](../../rio-scheduler/src/rebalancer.rs). The struct's doc-comment at [`:40-43`](../../rio-scheduler/src/rebalancer.rs) says "All three are config-driven via `scheduler.toml [rebalancer]`" — but `#[derive(Debug, Clone)]` has no `Deserialize`. [`spawn_task` at `:326`](../../rio-scheduler/src/rebalancer.rs) hardcodes `RebalancerConfig::default()`. The doc-comment is a lie; the config ISN'T config-driven, it's defaults-only.

Add `serde::Deserialize` to the derive list + `#[serde(default)]` on the struct (all fields have `Default::default()` via the explicit `impl Default`). Then thread it from `main.rs` config-load to `spawn_task` as a new param (or add a `rebalancer: RebalancerConfig` field to `SchedulerConfig` and read it in `actor::mod.rs:357` where `spawn_task` is called). Prefer the latter — `spawn_task`'s sig grows one param, `main.rs` passes `cfg.rebalancer` instead of `spawn_task` constructing its own default.

**Paired with [P0295-T58](plan-0295-doc-rot-batch-sweep.md)** (doc-side fix: scheduler.md:218 "config-driven" claim). That fixes the SPEC to match code; T95 fixes the CODE to match spec. Both correct. Land T95 and the spec stays accurate; skip T95 and P0295-T58 should instead rewrite the spec to say "defaults-only, not config-exposed." Prefer T95. discovered_from=230-review.

### T96 — `fix(scheduler):` `min_samples=0` latent panic — unreachable today, live after T95

MODIFY [`rio-scheduler/src/rebalancer.rs`](../../rio-scheduler/src/rebalancer.rs) at `compute_cutoffs` (`:144`). The `durations[idx.min(durations.len() - 1)]` line panics on wraparound when `durations.len() == 0`: `0usize - 1` wraps to `usize::MAX`. Guarded today by `min_samples = 500` (the `if samples.len() < cfg.min_samples { return None }` at `:102` always fires before we reach `:144` with an empty vec). [P0230's DISPATCH NOTE](plan-0230-rwlock-wire-cpu-bump-classify.md) at `:9` already called this out — "add `anyhow::ensure!(cfg.min_samples >= 1)` at config-load." But P0230 didn't wire config-load (see T95 — `spawn_task` uses `::default()`). So the ensure never landed.

Add the guard NOW in `compute_cutoffs` itself (defense-in-depth) — after `:102`'s early-return, before the `durations.len() - 1`:

```rust
// After :102's min_samples gate passes, samples.len() > 0 is
// guaranteed IFF min_samples >= 1. If a caller passes min_samples=0
// (possible once T95 exposes it to TOML), durations can be empty
// here and :144's len()-1 wraps. Guard explicitly.
if durations.is_empty() {
    return None;
}
```

Alternatively (or additionally) add `anyhow::ensure!(cfg.rebalancer.min_samples >= 1, ...)` in `main.rs` at config-load time (alongside the cutoff_secs NaN/inf check at `:295`). Both cheap; land both. discovered_from=230-review (P0230's own DISPATCH NOTE, not applied).

### T97 — `docs:` CLAUDE.md:192 same-line-trailing `r[verify]` example unparseable

MODIFY [`CLAUDE.md`](../../CLAUDE.md) at `:192`. The example block at `:190-197` shows:

```nix
subtests = [
  "gc-sweep"        # r[verify store.gc.tenant-retention]
  # r[verify worker.upload.references-scanned]
  ...
```

Line `:192` has `"gc-sweep"` followed by a same-line trailing `# r[verify ...]`. Tracey's `.nix` parser only picks up **col-0** markers (per [P0341](plan-0341-tracey-verify-reachability-markers.md)'s own convention and `config.styx`'s test_include logic). A marker after a string-literal on the same line is NOT col-0 → invisible to tracey. P0341's own documentation example would re-introduce the exact "marker exists but tracey doesn't see it" bug P0341 was FIXING.

Fix the example to put the `gc-sweep` marker on its own col-0 line (matching `:193-195`'s pattern):

```nix
subtests = [
  # r[verify store.gc.tenant-retention]
  "gc-sweep"
  # r[verify worker.upload.references-scanned]
  # r[verify worker.upload.deriver-populated]
  # r[verify store.gc.two-phase]
  "refs-end-to-end"
];
```

This matches how [`nix/tests/default.nix`](../../nix/tests/default.nix) actually structures its markers (check at dispatch — grep `r\[verify` in default.nix to confirm col-0 convention). **Ironic self-fix class** — same as T28 (rio-planner.md leading-zero guard) where a guidance doc demonstrates the pattern it forbids. discovered_from=341-review.

### T98 — `refactor(crds):` bare-string CEL rules at :137/:183/:401 — add `.message()`

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) at three sites. P0354+P0359 added `Rule::new(...).message(...)` for their new CEL rules (`:56-62` and `:82-88`), with a doc-comment at `:75-81` explaining why ("bare string emits a rule with no message → apiserver falls back to 'failed rule: {cel expr}' which is opaque to operators"). But three pre-existing CEL rules still use bare strings:

- [`:137`](../../rio-crds/src/workerpool.rs) `#[x_kube(validation = "self >= 1")]` on `max_concurrent_builds` — operator sees `failed rule: self >= 1` instead of `maxConcurrentBuilds must be >= 1 — a worker that runs 0 builds is a misconfiguration`
- [`:183`](../../rio-crds/src/workerpool.rs) `#[x_kube(validation = "size(self) > 0")]` on `systems` — operator sees `failed rule: size(self) > 0` instead of `systems must be non-empty — a worker pool with no target systems accepts no work`
- [`:401`](../../rio-crds/src/workerpool.rs) `#[x_kube(validation = "self.min <= self.max")]` on `Replicas` — operator sees `failed rule: self.min <= self.max` instead of `replicas.min must be <= replicas.max`

Convert all three to `Rule::new(...).message(...)`. Regenerate CRD YAML (`just crdgen`). Extend `cel_rules_in_schema` test (`:516+`) to assert `.message()` presence for all three (matching the existing `:548-560` check pattern for hostNetwork). **Same fix as P0304-T11** (seccomp CEL messages) — this is the pre-existing sibling batch. discovered_from=bughunt-mc133.

### T99 — `refactor(controller):` POOL_LABEL 4× literal → shared const

MODIFY [`rio-controller/src/reconcilers/workerpool/mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs) — promote the `"rio.build/pool"` literal to a `pub const POOL_LABEL: &str` at module level (or in a new `labels.rs` if there's a natural home for label constants). Four production sites currently inline the string:

| File:line | Usage |
|---|---|
| [`disruption.rs:48`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) | Already a `const POOL_LABEL` — but module-private |
| [`builders.rs:59`](../../rio-controller/src/reconcilers/workerpool/builders.rs) | `("rio.build/pool".into(), wp.name_any())` — labels map key |
| [`ephemeral.rs:126`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) | `format!("rio.build/pool={name}")` — list selector |
| [`mod.rs:394`](../../rio-controller/src/reconcilers/workerpool/mod.rs) | `format!("rio.build/pool={name}")` — list selector |

Plus two test sites ([`tests.rs:627`](../../rio-controller/src/reconcilers/workerpool/tests.rs), [`ephemeral.rs:478`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs)) that assert on the label key. Hoist the `disruption.rs:48` const to `mod.rs` as `pub(crate) const POOL_LABEL: &str = "rio.build/pool";` and reference it from all six sites. The `disruption.rs:44-47` doc-comment ("Same constant as `builders::labels()`") becomes accurate instead of aspirational. discovered_from=285-review.

### T100 — `refactor(tests):` k3s-full.nix waitReady 180s — bump for nonpriv DS bring-up

MODIFY [`nix/tests/fixtures/k3s-full.nix:475`](../../nix/tests/fixtures/k3s-full.nix). The `waitReady` block waits for `pod/default-workers-0` with `timeout=180` (inner `kubectl wait --timeout=150s`). P0360's non-privileged variant adds the smarter-device-manager DaemonSet to the bring-up path: DS must schedule + go Ready + register the `smarter-devices/fuse` extended resource **before** the worker pod (which requests the resource) can leave `Pending`. Under TCG this adds ~30-60s. 180s was already tight for the privileged fast-path (comment at `:390` says "300 (or 180) should suffice" — aspirational, not measured).

Bump `timeout=180` → `timeout=300` and `--timeout=150s` → `--timeout=270s`. Keep the privileged fast-path unchanged (P0360's `valuesFile` parameterization means the nonpriv variant can set its own timeout if preferred — but a single 300s ceiling is simpler than a conditional). Update the `:390` comment to drop the "180 should suffice" aspirational claim. discovered_from=360-review.

### T101 — `feat(controller):` rio_controller_disruption_drains_total counter

MODIFY [`rio-controller/src/reconcilers/workerpool/disruption.rs`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) — the DisruptionTarget watcher at `:82-130` fires `DrainWorker{force:true}` on every eviction-marked pod but emits zero metrics. Operator cannot distinguish "no evictions" from "watcher is dead and we're falling back to the 2h SIGTERM self-drain" ([`controller.md:282`](../../docs/src/components/controller.md)).

Add a counter:

```rust
metrics::counter!(
    "rio_controller_disruption_drains_total",
    "result" => result,  // "sent" | "rpc_error"
).increment(1);
```

Emit after the `admin.drain_worker(...)` call resolves (`~:125` — `result="sent"` on `Ok`, `result="rpc_error"` on `Err`). Add `describe_counter!` in controller's `describe_metrics` (grep `describe_counter!` in [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) or wherever the controller registers — may be `metrics.rs`). Add the row to the `r[obs.metric.controller]` table at [`observability.md:194`](../../docs/src/observability.md) (after the `gc_runs_total` row). Add the metric name to `CONTROLLER_METRICS` const in [`rio-controller/tests/metrics_registered.rs`](../../rio-controller/tests/metrics_registered.rs). Same shape as T26/T47/T50 prior metric-parity fixes in this batch. discovered_from=285-review.

### T102 — `docs(controller):` describe_metrics reconciler label — WPS-unaware

MODIFY [`rio-controller/src/lib.rs:70-81`](../../rio-controller/src/lib.rs). The `describe_histogram!` at `:70-75` says `reconciler=workerpool` and `describe_counter!` at `:76-81` says `reconciler=workerpool` — but [`workerpoolset/mod.rs:90-92`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) emits the same metric with `reconciler=workerpoolset`. The describe-string is stale (predates P0233's WPS reconciler). Operator reading `/metrics` sees `reconciler="workerpoolset"` values but `# HELP` only mentions `workerpool`.

Fix both describe-strings: `reconciler=workerpool` → `reconciler=workerpool|workerpoolset`. Two-line edit. discovered_from=233-review.

### T103 — `refactor(controller):` child_name — RFC-1123 length guard

MODIFY [`rio-controller/src/reconcilers/workerpoolset/builders.rs:43-45`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs). `child_name` returns `format!("{}-{}", wps.name_any(), class.name)` with no length or character validation. Kubernetes resource names are RFC-1123 DNS labels: ≤63 chars, lowercase alphanumeric + hyphen, no leading/trailing hyphen. A WPS name of 40 chars + a class name of 30 chars = 71 chars → apiserver 422 with a cryptic error deep in the apply path.

Add a validation:

```rust
pub(super) fn child_name(wps: &WorkerPoolSet, class: &SizeClassSpec) -> Result<String> {
    let name = format!("{}-{}", wps.name_any(), class.name);
    if name.len() > 63 {
        return Err(Error::InvalidSpec(format!(
            "child WorkerPool name '{name}' exceeds 63 chars (RFC-1123 DNS label limit). \
             Shorten WorkerPoolSet name or size-class name."
        )));
    }
    // Char validation: class.name is already CEL-validated in the CRD
    // (grep x-kubernetes-validations in workerpoolset.rs); wps.name_any()
    // is apiserver-validated. Concatenation of two valid labels with '-'
    // is valid. Only length can overflow.
    Ok(name)
}
```

Callers at [`builders.rs:140`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) and [`mod.rs:162`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) need `?`. The `Error::InvalidSpec` → reconcile error → condition on the WPS status → operator sees it in `kubectl describe wps`. discovered_from=233-review.

### T104 — `refactor(infra):` streamIdleTimeout 1h → values.yaml param

MODIFY [`infra/helm/rio-build/templates/dashboard-gateway-policy.yaml:64`](../../infra/helm/rio-build/templates/dashboard-gateway-policy.yaml). `streamIdleTimeout: 1h` is hardcoded. The `:47-49` comment says "1h is a practical ceiling (if a stream is truly quiet for an hour, the build is stuck)" — operationally reasonable default but should be values-configurable (LLVM-cold-ccache on a slow node can stall >1h; conversely a tight-budget cluster may want 15m).

Replace with `streamIdleTimeout: {{ .Values.dashboard.streamIdleTimeout | default "1h" }}`. Add to [`values.yaml:379+`](../../infra/helm/rio-build/values.yaml) `dashboard:` block:

```yaml
dashboard:
  # ... existing fields ...
  # ClientTrafficPolicy.timeout.http.streamIdleTimeout — kills a quiet
  # server-stream (GetBuildLogs, WatchBuild) after this. Envoy's default
  # is 5m which is too aggressive for slow builds.
  streamIdleTimeout: 1h
```

discovered_from=273-review.

### T105 — `docs(infra):` orphaned phase-5b deferral → TODO(P0371) tags

MODIFY [`infra/helm/rio-build/templates/dashboard-gateway.yaml:77`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml) and [`dashboard-gateway-policy.yaml:10-12`](../../infra/helm/rio-build/templates/dashboard-gateway-policy.yaml). Both files have untagged deferrals ("phase 5b adds per-method authz", "tighten allowOrigins post-MVP"). Add `TODO(P0371)` prefix to both comments so [P0371](plan-0371-dashboard-gateway-admin-method-authz.md) closes them.

**If P0371 dispatches first:** it deletes both comments entirely (the deferral is resolved). T105 becomes moot — skip if `grep 'phase 5b adds' infra/helm/rio-build/templates/dashboard-gateway.yaml` returns 0 at dispatch. discovered_from=273-review.

### T106 — `refactor(scheduler):` COALESCE($2,168) → const DEFAULT_GC_RETENTION_HOURS

MODIFY [`rio-scheduler/src/db.rs:351`](../../rio-scheduler/src/db.rs). `VALUES ($1, COALESCE($2, 168), $3, $4)` hardcodes the default GC retention (168h = 7 days) in SQL. The value appears only here but the meaning is scattered — [P0340](plan-0340-extract-seed-tenant-test-helper.md) touches the tenant-seed path and would benefit from a named const.

Add above `create_tenant` (near `:339`):

```rust
/// Default gc_retention_hours for new tenants. 168h = 7 days.
/// Used as the COALESCE fallback when CreateTenant request omits
/// retention (proto3 default 0 → None here → this value).
pub const DEFAULT_GC_RETENTION_HOURS: i32 = 168;
```

Then bind it: `sqlx::query_as(...).bind(name).bind(gc_retention_hours.or(Some(DEFAULT_GC_RETENTION_HOURS)))` and change SQL to `VALUES ($1, $2, $3, $4)` — OR keep COALESCE and reference the const in the doc-comment. Implementer's call; the const-doc-comment approach is simpler (no SQL change). discovered_from=340-review.

### T107 — `refactor(scheduler):` admin/mod.rs parse-build-id dup → shared helper

MODIFY [`rio-scheduler/src/admin/mod.rs:388`](../../rio-scheduler/src/admin/mod.rs). The line `.map_err(|e| Status::invalid_argument(format!("invalid build_id UUID: {e}")))?` duplicates [`grpc/mod.rs:176-179`](../../rio-scheduler/src/grpc/mod.rs) `parse_build_id`. The grpc version is already `pub(crate)`. Replace the admin inline with a call to the shared helper — but the admin one includes the error detail `{e}` whereas grpc's doesn't. Either:

- (a) Update `parse_build_id` to include `{e}` in the Status message (better error for CLI users), or
- (b) Keep the inline if the detail matters for admin-CLI but not SubmitBuild

Prefer (a): `Status::invalid_argument(format!("invalid build_id UUID: {e}"))` in the shared helper. Then `admin/mod.rs:388` becomes `let build_id = crate::grpc::SchedulerGrpc::parse_build_id(&req.build_id)?;`. Update call sites at [`grpc/mod.rs:605/648/670`](../../rio-scheduler/src/grpc/mod.rs) implicitly via the shared helper. discovered_from=362-review.

### T108 — `refactor(proto,store,worker):` x-rio-assignment-token → ASSIGNMENT_TOKEN_HEADER const

[`rio-proto/src/lib.rs:25`](../../rio-proto/src/lib.rs) defines `BUILD_ID_HEADER`, `:43` defines `TRACE_ID_HEADER`; [`rio-common/src/jwt_interceptor.rs:68`](../../rio-common/src/jwt_interceptor.rs) defines `TENANT_TOKEN_HEADER`. But `x-rio-assignment-token` appears as a bare string literal at 4 sites: [`rio-store/src/grpc/put_path.rs:158`](../../rio-store/src/grpc/put_path.rs) (get), `:213` (error message), [`rio-worker/src/upload.rs:337`](../../rio-worker/src/upload.rs) (set), `:834` (set).

Add to [`rio-proto/src/lib.rs`](../../rio-proto/src/lib.rs) alongside the other header consts:

```rust
/// gRPC metadata key for HMAC-signed assignment tokens.
/// Scheduler signs at dispatch; store verifies on PutPath.
/// See rio-common::hmac for the token format.
pub const ASSIGNMENT_TOKEN_HEADER: &str = "x-rio-assignment-token";
```

Migrate all 4 literal sites. Also: [`rio-gateway/src/handler/build.rs:372`](../../rio-gateway/src/handler/build.rs) uses bare `"x-rio-tenant-token"` literal — migrate to `rio_common::jwt_interceptor::TENANT_TOKEN_HEADER` (the const already exists, this site just doesn't use it). discovered_from=consol-mc145.

### T109 — `refactor(nix):` grpcurl raw_decode loop → extract helper

The `json.JSONDecoder().raw_decode` pattern appears 7 times across [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) (`:434`, `:1198`, `:1492`, `:1715`, `:1864`, `:1912`) + [`scheduling.nix:175`](../../nix/tests/scenarios/scheduling.nix). Each iterates over grpcurl's whitespace-separated JSON stream. Structure is identical:

```python
idx = 0
dec = json.JSONDecoder()
objs = []
while idx < len(out):
    obj, idx = dec.raw_decode(out, idx)
    objs.append(obj)
    while idx < len(out) and out[idx].isspace(): idx += 1
```

Extract to `lifecycle.nix` testScript preamble (alongside [P0362](plan-0362-extract-submit-build-grpc-helper.md)'s `submit_build_grpc`):

```python
def grpcurl_json_stream(out: str) -> list[dict]:
    """Parse grpcurl's concatenated-JSON output (one pretty-printed
    object per stream message, whitespace-separated). Returns list of
    dicts. Empty input → empty list."""
    idx, dec, objs = 0, json.JSONDecoder(), []
    while idx < len(out):
        while idx < len(out) and out[idx].isspace(): idx += 1
        if idx >= len(out): break
        obj, idx = dec.raw_decode(out, idx)
        objs.append(obj)
    return objs
```

Migrate the 6 lifecycle.nix sites + `scheduling.nix:175` if it uses the same fixture style (it does — `submit_build_grpc` was extracted there too by P0362-T2). Net: ~-40L across both files. discovered_from=consol-mc145.

### T110 — `docs(agents):` rio-impl-merger clause-4 codification — embed decision tree

MODIFY [`.claude/agents/rio-impl-merger.md`](../../.claude/agents/rio-impl-merger.md). The clause-4 fast-path tiers have evolved through 29+ fast-paths but the merger agent spec lacks a crisp decision tree. Mergers re-derive the tier from `.claude/notes/ci-gate-fastpath-precedent.md` every time; some mergers (P0212, P0328, P0257 mode-5) aborted correctly, others (P0227, P0274, P0268 mode-4) self-applied — the difference was whether the abort-report recipe made the tier obvious.

Add a decision-tree section (after the existing mode-5 prose, before the "Report format" section):

```markdown
## Clause-4 fast-path decision tree (codified from 29+ precedents)

On `.#ci` failure after retry:Once exhaustion, walk this tree:

1. **Plan delta touches ONLY `.claude/` or `nix/tests/` or docs?**
   → Clause-4(a). Eval `nix eval .#checks.x86_64-linux.<failing-test>.drvPath`
   at BOTH pre-merge and post-merge tips. If identical: behavioral-
   identity. Proceed to merge. (Docs-only ≠ hash-identical until
   P0304-T29 `fileset.difference` lands.)

2. **Last-green→now delta touches only `nix/tests/` AND rust-∩ = ∅?**
   → Clause-4(b). Byte-identical rust derivations by construction.
   Prove via `nix eval .#checks.x86_64-linux.nextest.drvPath` identical
   to last-green's. Proceed.

3. **Rebase-clean AND CI-proved-rust-tier fresh?**
   → Clause-4(c). Run `.#checks.x86_64-linux.nextest` STANDALONE
   (rc=0 sufficient — VM tier is where roulette lives). On rc=0 with
   `Compiling rio-<touched-crate>` in log (fresh-build proof, not
   cache-hit): proceed. Report exact test-count delta as semantic proof.

4. **Failing drv byte-identical at both tips?**
   → Mode-4. Red is definitionally baseline; rolling back is futile
   (same drv → same builder → same fleet). Proceed on
   delta-invalidated checks alone (tracey, pre-commit, clippy).

5. **None of the above AND >10 Cargo.lock delta OR new crate?**
   → Mode-5. Expect full-rebuild too slow to beat fast-fail.
   Rollback, preserve branch ff-ready, abort-report with:
   drv-hashes, expected-test-count, which-checks-green. Coordinator
   fires nextest-standalone.

Proof recipes by tier:
| Tier | Proof |
|---|---|
| hash-identical | `nix eval drvPath` at both tips, diff empty |
| behavioral-identity | drv-hash differs BUT `grep <new-attr> <failing-test>.nix`=0 + rust-tier cached (zero log presence) |
| clause-4(c) | nextest-standalone rc=0 + `Compiling` + `nar content does not exist` (fresh-upload-observed) + exact test-count-delta |
| mode-4 | failing drv identical at both tips (nix-eval proof) |
```

discovered_from=consol-mc145.

### T111 — `refactor(test-support):` TenantSeed.with_ed25519_key — tenant_keys INSERT sibling

MODIFY [`rio-test-support/src/pg.rs`](../../rio-test-support/src/pg.rs). [`TenantSeed`](../../rio-test-support/src/pg.rs) at `:420-470` covers the `tenants` table (`.with_retention_hours`, `.with_max_store_bytes`, `.with_cache_token`). Four test sites inline `INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed)` — same shape, one table down: [`signing.rs:253`](../../rio-store/tests/grpc/signing.rs), [`signing.rs:353`](../../rio-store/tests/grpc/signing.rs), plus any P0311-T23/T26/T34 additions (check at dispatch — `grep 'INSERT INTO tenant_keys' rio-*/tests/`).

Add to `TenantSeed`:

```rust
impl TenantSeed {
    /// Also seed a row in tenant_keys. Chain after .seed() consumers
    /// that need a per-tenant signing key (most signing.rs tests).
    /// Default key_name is "tenant-{name}-1" if not specified.
    pub fn with_ed25519_key(mut self, seed: [u8; 32]) -> Self {
        self.ed25519_seed = Some(seed);
        self
    }

    /// Override the default key_name ("{tenant_name}-1").
    pub fn with_key_name(mut self, name: impl Into<String>) -> Self {
        self.key_name = Some(name.into());
        self
    }
}
```

Add fields `ed25519_seed: Option<[u8; 32]>`, `key_name: Option<String>` to the struct. In `seed()`, after the tenants INSERT, if `ed25519_seed.is_some()`:

```rust
if let Some(seed) = self.ed25519_seed {
    let key_name = self.key_name.unwrap_or_else(|| format!("{}-1", &self.name));
    sqlx::query(
        "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) VALUES ($1, $2, $3)",
    )
    .bind(tid)
    .bind(key_name)
    .bind(&seed[..])
    .execute(pool)
    .await
    .expect("TenantSeed tenant_keys INSERT failed");
}
```

Migrate the 4 test sites. Net: ~-30L across callers. **COALESCE sibling-note for T106:** `pg.rs:460` has `COALESCE($2, 168)` — same magic-168 as `db.rs:351` (the bughunt-mc154 accumulation finding). T106's `DEFAULT_GC_RETENTION_HOURS` const can't be reused here (rio-test-support doesn't dep on rio-scheduler); document the duplication with a comment pointing at T106's const. discovered_from=consol-mc155.

### T112 — `docs(controller):` scale_wps_class — comment-code mismatch on fallback semantics

MODIFY [`rio-controller/src/scaling.rs`](../../rio-controller/src/scaling.rs) at the `scale_wps_class` body (p234 worktree — re-grep at dispatch). The comment at `:529-533` says "Missing class in the RPC response … Fall back to 0 → `compute_desired` returns `min`" — check that this matches the actual fallback at `:534-539`. If `compute_desired(0, target, min, max)` actually returns something OTHER than `min` (e.g. it returns 0 → would scale DOWN, not hold at min), the comment is wrong. Fix whichever side is off. discovered_from=234-review.

### T113 — `docs(controller):` builders.rs:86 — "P0234 adds GetSizeClassStatus plumbing" stale post-P0234

MODIFY [`rio-controller/src/reconcilers/workerpoolset/builders.rs:86-91`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs). "P0234 adds the per-class GetSizeClassStatus plumbing for STATUS aggregation" — future-tense, stale once P0234 lands. Replace with present-tense reference to [`scaling.rs::scale_wps_class`](../../rio-controller/src/scaling.rs) (or delete the forward-looking parenthetical entirely). discovered_from=234-review.

### T114 — `refactor(controller):` child_name format! dup — scaling.rs:508 → use builders::child_name

MODIFY [`rio-controller/src/scaling.rs:508`](../../rio-controller/src/scaling.rs) (p234 ref). `format!("{}-{}", wps.name_any(), class.name)` duplicates [`workerpoolset/builders.rs:43-45`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) `child_name()`. Lift `child_name` to `pub(crate)` (currently `pub(super)`) OR re-export from `workerpoolset/mod.rs` as `pub(crate)`. Then `scaling.rs:508` becomes `let child_name = crate::reconcilers::workerpoolset::child_name(wps, class);`. Two concerns use one naming convention. discovered_from=234-review.

### T115 — `feat(worker):` log bloom_expected_items at Cache::new — debug visibility

MODIFY [`rio-worker/src/fuse/cache.rs:268`](../../rio-worker/src/fuse/cache.rs). `let expected = bloom_expected_items.unwrap_or(BLOOM_EXPECTED_ITEMS_DEFAULT);` — add a `tracing::info!` right after: `info!(bloom_expected_items = expected, "FUSE cache bloom filter sized");`. Operator debugging a fill-ratio alert can `kubectl logs` and confirm the ceiling. ~2 lines. discovered_from=288-review.

### T116 — `refactor(dashboard):` Builds.svelte:6 — Svelte-4 export-let → Svelte-5 $props()

MODIFY [`rio-dashboard/src/pages/Builds.svelte:6`](../../rio-dashboard/src/pages/Builds.svelte). `export let id: string | undefined = undefined;` is Svelte-4 legacy prop syntax. [`Cluster.svelte`](../../rio-dashboard/src/pages/Cluster.svelte) uses Svelte-5 `$props()` (per the rev-p277 finding — check at dispatch for the exact Cluster.svelte pattern). Align to `let { id = undefined }: { id?: string } = $props();` (or the project's Svelte-5 idiom). Consistency; both files in the same pages dir. discovered_from=277-review.

### T117 — `perf(dashboard):` Cluster.svelte 5s poll — add backoff-on-error + document.hidden pause

MODIFY [`rio-dashboard/src/pages/Cluster.svelte:30-32`](../../rio-dashboard/src/pages/Cluster.svelte). `setInterval(refresh, 5000)` fires regardless of tab visibility or prior-call success. Add:

```ts
let backoff = 5000;
const id = setInterval(() => {
    if (document.hidden) return;  // skip when tab backgrounded
    refresh().then(() => { backoff = 5000; }).catch(() => { backoff = Math.min(backoff * 2, 60000); });
}, 5000);
```

Or use a recursive `setTimeout` pattern for true exponential backoff (setInterval-with-backoff-var only THROTTLES the retry decision, not the tick rate). Implementer's call; the document.hidden pause is the bigger win (backgrounded tabs stop hammering the scheduler). discovered_from=277-review.

### T118 — `refactor(nix):` docker.nix nginx — drop unused libxslt module

MODIFY [`nix/docker.nix:473`](../../nix/docker.nix) (re-grep at dispatch). nginx closure pulls `libxml2` + `libxslt` (~1.8MiB) via unused `--with-http_xslt_module`. nixpkgs nginx module exposes this — disable via override or `nginxMainline.override { withXslt = false; }` (or equivalent — check nixpkgs `nginx/generic.nix` for the exact attr). 1.8MiB closure-size win on the dashboard image. discovered_from=282-review.

### T119 — `test(infra):` device-plugin image tag lockstep-enforcement

> **SIMPLIFIED by [P0387](plan-0387-airgap-bare-tag-derive-from-destnametag.md):** the bare-tag is now DERIVED from `pulled.smarter-device-manager.destNameTag`, not hardcoded. There's no drift-to-assert. T119 collapses to: assert `devicePlugin.image` override is SET when `pulled.smarter-device-manager` appears in `extraImages` (preload-but-no-override = wasted preload; override-but-no-preload = `ImagePullBackOff`). Different axis, simpler check.

Extends [P0369](plan-0369-device-plugin-image-tag-fix.md)-T2 (helm-lint guard for digest-pin). rev-p369 found: the airgap-image-set at `nix/tests/fixtures/k3s-full.nix` ALSO hardcodes the smarter-device-manager tag. If values.yaml bumps the tag (P0369-T1) but k3s-full.nix doesn't, VM tests pull a different image than production. Add a lockstep assert in `flake.nix` helm-lint (or a separate `.#checks.*.image-tag-sync`) that extracts both tags and compares:

```nix
# Assert values.yaml devicePlugin.image tag == k3s-full.nix airgap-set tag.
# Drift here = VM tests cover a different image than production ships.
let
  valuesTag = (builtins.fromJSON (pkgs.lib.readFile ./infra/helm/rio-build/values.yaml)).devicePlugin.image;  # or yq extract
  airgapTag = ...;  # grep/extract from k3s-full.nix
in
assert pkgs.lib.assertMsg (valuesTag == airgapTag)
  "devicePlugin image tag drift: values.yaml=${valuesTag} vs k3s-full.nix=${airgapTag}";
```

Exact extraction mechanism (yq vs `builtins.fromTOML`-style parse vs grep) at implementer's discretion. discovered_from=369-review.

### T120 — `docs(notes):` kvm-pending.md +2 never-built entries

MODIFY [`.claude/notes/kvm-pending.md`](../../.claude/notes/kvm-pending.md) (P0311-T16 creates it if absent). consol-mc150 found the never-built VM backlog grew +2 this window: `vm-security-nonpriv-k3s` (P0360, already tracked by P0311-T37) + one more (check the consolidator's full description — likely a P0361 or similar scheduling.nix fragment). Add both entries with drv-hash and source-plan. discovered_from=consol-mc150.

### T121 — `refactor(controller):` SSA envelope helper — 6× `apiVersion+kind` json! pattern

The SSA-patch body pattern `serde_json::json!({"apiVersion": ar.api_version, "kind": ar.kind, "spec": {...}})` appears at ≥4 sites on sprint-1 ([`workerpool/mod.rs:371`](../../rio-controller/src/reconcilers/workerpool/mod.rs), [`ephemeral.rs:234`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs), [`scaling.rs:539`](../../rio-controller/src/scaling.rs), [`scaling.rs:780`](../../rio-controller/src/scaling.rs) `sts_replicas_patch`); P0234 adds 2 more ([`workerpoolset/mod.rs:256`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs), `scaling.rs:591` per-class patch). 6 total. Extract to [`rio-controller/src/reconcilers/mod.rs`](../../rio-controller/src/reconcilers/mod.rs) as a helper:

```rust
/// SSA patch bodies MUST include apiVersion + kind or the apiserver
/// returns 400. Centralize the envelope so no site forgets.
pub(crate) fn ssa_envelope<T: kube::Resource<DynamicType = ()>>(
    body: serde_json::Value,
) -> serde_json::Value {
    let ar = T::api_resource();
    let mut env = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
    });
    // Merge body into envelope (body is typically {"spec": {...}} or {"status": {...}})
    if let (Some(env_obj), Some(body_obj)) = (env.as_object_mut(), body.as_object()) {
        for (k, v) in body_obj {
            env_obj.insert(k.clone(), v.clone());
        }
    }
    env
}
```

Then each site becomes `let patch = ssa_envelope::<StatefulSet>(json!({"spec": {"replicas": n}}));`. `sts_replicas_patch` stays as a thin wrapper for the common case. Migrate all 6 sites; net ~-30L. discovered_from=consol-mc150.

**NOTE on T76 re-flag:** consol-mc155 re-flagged the `RIO_GOLDEN_*` triplication that T76 already covers (nextest/coverage/mutants at `flake.nix:403,666,1013`). No new task — T76's scope is correct; the consol-mc155 finding confirms the triplication is still live (T76 hasn't dispatched yet).

### T122 — `refactor(dashboard):` DrainButton busy-flicker — disabled gate collapses mid-await

rev-p281 trivial. [`DrainButton.svelte:24-26`](../../rio-dashboard/src/components/DrainButton.svelte) `disabled = $derived(busy || !target || target.status !== 'alive')`. The optimistic set at `:40` flips `target.status` to `'draining'` THEN `:41` sets `busy = true` — there's a one-tick window where `status !== 'alive'` already disables the button but `busy` hasn't kicked in. Harmless (button is still disabled via the status check) but the `busy` state is redundant once the optimistic set lands. Either: (a) set `busy = true` BEFORE the status mutation, or (b) drop `busy` entirely and let the `target.status !== 'alive'` clause cover in-flight (revert-on-error restores `'alive'` → button re-enables). Prefer (b) — one less state variable, the status already encodes in-flight. **SEQUENCE:** this lands AFTER [P0377](plan-0377-drainbutton-revert-idx-race.md) (the idx-race fix rewrites `drain()` — this polish rebases onto the fixed shape). discovered_from=281-review.

### T123 — `refactor(scheduler):` worker_service.rs:198 tokio::spawn → spawn_monitored

rev-p356 trivial. [`worker_service.rs:198`](../../rio-scheduler/src/grpc/worker_service.rs) uses bare `tokio::spawn` for the fire-and-forget EMA-proactive PG write. The comment at `:183-191` explicitly justifies the bare spawn ("fire-and-forget, dropped or failed update caught by next 10s tick"). BUT every other spawn in the file ([`worker_service.rs:75,92`](../../rio-scheduler/src/grpc/worker_service.rs)) and crate ([`main.rs:406,489,553,584`](../../rio-scheduler/src/main.rs), [`event_log.rs:65`](../../rio-scheduler/src/event_log.rs), [`rebalancer.rs:325`](../../rio-scheduler/src/rebalancer.rs)) uses `rio_common::task::spawn_monitored` — panic-in-task logged instead of silently swallowed. The EMA write shouldn't panic (it's a PG UPDATE + counter increment), but consistency + if it DOES panic the logged trace helps debug. Replace with `spawn_monitored("ema-proactive-write", async move { ... })`. Update the `:183` comment: `// spawn_monitored (fire-and-forget): the db write is ...`. Pure convention-align, no behavior change on the happy path. discovered_from=356-review.

### T124 — `refactor(dashboard):` Workers.svelte status pill — aria-label on visual-only color

rev-p278 a11y. [`Workers.svelte:86-88`](../../rio-dashboard/src/pages/Workers.svelte) (p281 worktree) renders `<span class="pill pill-{w.status}">{w.status}</span>` — the pill background color is the only visual cue differentiating alive/draining/dead, but the text content IS the status string, so screen-readers get it. Still: the `pill-*` color-coding carries semantic meaning (red=bad) that color-blind users can't distinguish. Add `aria-label="status: {w.status}"` redundantly (screen-readers already read the text, but the explicit label clarifies it's a status indicator not just a styled word) OR add a small icon/prefix char (●/○/✕) alongside the text. Prefer the icon-prefix: visible to all, no aria-only fix. **This is P0281 code** — T124 lands post-P0281-merge. discovered_from=278-review.

### T125 — `refactor(dashboard):` BuildDrawer tabs — role=tablist + aria-selected

rev-p278 a11y. [`BuildDrawer.svelte:86-97`](../../rio-dashboard/src/components/BuildDrawer.svelte) (p278 worktree) has `<nav class="tabs">` with two `<button>` elements toggling `activeTab`. Accessible tabs need `role="tablist"` on the nav, `role="tab"` + `aria-selected={activeTab === 'logs'}` on each button, and `role="tabpanel"` + `aria-labelledby` on the `<section class="tab-body">`. ~6 attribute additions, no logic change. **This is P0278 code** — T125 lands post-P0278-merge. discovered_from=278-review.

### T126 — `refactor(dashboard):` Builds.svelte tr[onclick] — keyboard-accessible row selection

rev-p278 a11y. [`Builds.svelte:172`](../../rio-dashboard/src/pages/Builds.svelte) (p278 worktree) has `<tr onclick={() => (selected = b)}>` — click-only, no keyboard affordance. Add `tabindex="0"` + `onkeydown={(e) => e.key === 'Enter' && (selected = b)}` + `role="button"` (or restructure so the first cell wraps a `<button>` instead of the `<tr>` being clickable — stricter a11y, but changes the UX). Prefer the `tabindex`+`onkeydown`+`role` add — minimal churn, screen-readers announce "button" on the row. The copy-button at `:177-182` already has `onclick` with `stopPropagation` — add `onkeydown` with the same guard. **This is P0278 code** — T126 lands post-P0278-merge. discovered_from=278-review.

### T127 — `refactor(dashboard):` extract progress(BuildInfo) — duplicated Builds.svelte + BuildDrawer.svelte

rev-p278 trivial. [`Builds.svelte:94-98`](../../rio-dashboard/src/pages/Builds.svelte) (p278 worktree) and [`BuildDrawer.svelte:21-25`](../../rio-dashboard/src/components/BuildDrawer.svelte) have identical `function progress(b: BuildInfo): number { if (b.totalDerivations === 0) return 0; const done = b.completedDerivations + b.cachedDerivations; return Math.min(100, Math.round((done / b.totalDerivations) * 100)); }`. Extract to `rio-dashboard/src/lib/build.ts` as `export function buildProgress(b: BuildInfo): number` and import from both. Also `fmtTs`/`relTime` are similar between the two files (both parse `google.protobuf.Timestamp` → ms → format string) — check at dispatch if those merit extraction too (they differ slightly: `fmtTs` returns ISO, `relTime` returns "3m ago"; may not share enough). **This is P0278 code** — T127 lands post-P0278-merge. discovered_from=278-review.

**NOTE on T115 re-flag (rev-p375):** rev-p375 independently flagged the `Cache::new()` bloom-init log omitting `expected_items` ([`cache.rs:276`](../../rio-worker/src/fuse/cache.rs)). T115 already covers this. The re-flag confirms T115 is still live (hasn't dispatched) and adds a detail: the motivation is now sharper because P0375 makes `bloom_expected_items` operator-tunable via WorkerPool CRD — operators setting the knob have no log confirmation the env propagation worked. T115 unchanged in scope; discovered_from=375-review as secondary source.

### T128 — `fix(controller):` find_wps_child — is_wps_owned → is_wps_owned_by for UID-symmetric gate

rev-p374 trivial (one-line). [`scaling.rs:1030`](../../rio-controller/src/scaling.rs) `Some(child) if is_wps_owned(child) => ChildLookup::Found(child)` — gates on kind-only `is_wps_owned` (ANY `WorkerPoolSet` controller ownerRef). But `prune_stale_children` at [`workerpoolset/mod.rs:247`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) gates on UID-specific `is_wps_owned_by(child, wps)`. The asymmetry: two WPS in the same namespace where `{a}-{b}=={c}-{d}` collide on name (e.g., WPS `prod` class `small-x` vs WPS `prod-small` class `x` → both children named `prod-small-x`) — `find_wps_child` would return the WRONG WPS's child (kind-only gate passes both), the per-class autoscaler patches the wrong STS. Prune correctly filters by UID; find doesn't.

Fix: `Some(child) if is_wps_owned_by(child, wps) => ChildLookup::Found(child)`. The `find_wps_child` signature already has `wps: &WorkerPoolSet` — the parameter exists, it's just not used by the guard. The comment at `:1009` says "two-key symmetry (name-match AND is_wps_owned)" — update to "two-key symmetry (name-match AND is_wps_owned_by UID-match) — same UID gate as prune_stale_children". Makes the "two-key symmetry" P0374 introduced ACTUALLY symmetric.

Existing tests at `:1552` (`scale_wps_class_skips_name_collision_without_ownerref`) and `:1633` (`is_wps_owned_by_matches_uid_not_just_kind`) cover the old gate and the UID predicate respectively; a new `find_wps_child_rejects_wrong_wps_uid` test after `:1630` asserts a child with a DIFFERENT WPS's UID ownerRef returns `ChildLookup::NameCollision` not `Found`. discovered_from=374-review.

### T129 — `refactor(infra):` values.yaml CORS rio-system hardcode — NOTES.txt hint on stale-placeholder

rev-p371 trivial. [`values.yaml:429`](../../infra/helm/rio-build/values.yaml) hardcodes `http://rio-dashboard.rio-system.svc.cluster.local` in `dashboard.cors.allowOrigins`. The comment at `:424-428` explains this is a deliberate fail-closed placeholder (browsers never produce this Origin; operators MUST override). But the coupling to `namespace.name` at `:31` is silent — operator who overrides `namespace.name` sees a stale `rio-system` string with no warning.

Helm doesn't template `values.yaml` so the hardcode is correct. Fix: add to `infra/helm/rio-build/templates/NOTES.txt` (create if absent) a conditional hint:

```
{{- if and .Values.dashboard.enabled (has "http://rio-dashboard.rio-system.svc.cluster.local" .Values.dashboard.cors.allowOrigins) }}
  WARNING: dashboard.cors.allowOrigins contains the placeholder in-cluster
  hostname. Browsers cannot produce this Origin — override with your
  Ingress/LoadBalancer URL in values:
    dashboard.cors.allowOrigins:
      - "https://rio.<your-domain>"
{{- end }}
```

Alternative (option-B in the followup): template the default in `dashboard-gateway-policy.yaml` via `{{ default (printf "http://rio-dashboard.%s.svc.cluster.local" .Values.namespace.name) ... }}` when `allowOrigins` is empty — more invasive (changes the policy template), but removes the coupling entirely. Prefer option-A (NOTES.txt hint) — visible at `helm install` time, zero behavior change. discovered_from=371-review.

### T130 — `refactor(infra):` dashboard-native-authz forward-refs — tag or rephrase (3 sites, no plan owns them)

rev-p371 trivial. "until dashboard-native authz lands" appears at 3 sites with no `TODO(PNNNN)` tag and no plan doc: [`dashboard-gateway.yaml:109`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml), [`values.yaml:413`](../../infra/helm/rio-build/values.yaml), [`docs/src/components/dashboard.md:30`](../../docs/src/components/dashboard.md) (inside `r[dash.auth.method-gate]` spec text). P0371 IS the method-gate (replaced the prior orphaned "phase 5b" deferral — T105 tracked that older deferral); P0371 does NOT create a plan for per-user JWT + SecurityPolicy.jwt. No plan exists.

Per CLAUDE.md deferred-work discipline: either tag with a plan number or avoid "until X lands" phrasing. No plan → rephrase. Replace "until dashboard-native authz lands" at all 3 sites with "until per-user dashboard auth is scoped (no plan yet — see r[dash.auth.method-gate] for current method-level gate)" — acknowledges the deferral without implying a scheduled landing. If a dashboard-authz plan is later written, these 3 sites get the `TODO(P0NNN)` tag at that time.

**T105 MOOT-check:** T105 was tagging the PRE-P0371 "phase 5b" comment at `:77`. P0371 rewrote dashboard-gateway.yaml entirely (the `:109` reference in this T130 is POST-P0371). Check at dispatch: `grep 'phase 5b' infra/helm/rio-build/templates/dashboard-gateway.yaml` → if 0, T105 is moot (skip); if ≥1, the old comment survived and T105 applies separately. discovered_from=371-review.

### T131 — `docs(cli):` main.rs unreachable!(Wps) — design-note doc-comment on deferred split

rev-p237 trivial. [`rio-cli/src/main.rs:513`](../../rio-cli/src/main.rs) (p237 worktree) `Cmd::Wps(_) => unreachable!("Wps handled before gRPC connect")` — the `Cmd` enum mixes gRPC-only and kube-only subcommands; `Wps` is dispatched early (before `connect_grpc()`) and the late match arm is unreachable-by-construction. The followup's verdict: sealed-trait / `KubeCmd|GrpcCmd` split is NOT YET worth it (single kube-only variant, loud-fail not silent). Document the deferral in a doc-comment above the match at ~`:380` (or wherever the `match cmd {` starts):

```rust
// Dispatch-order contract: kube-only subcommands (currently just Wps)
// are matched BEFORE gRPC connect — they don't need a scheduler client.
// The unreachable!() arm below is the tripwire if this ordering breaks.
//
// NOT splitting Cmd into KubeCmd|GrpcCmd yet: single kube-only variant,
// and the unreachable! fails loud on first invocation (not silent). When
// a SECOND kube-only subcommand lands (e.g., a WorkerPool describe that
// doesn't need admin RPC), reconsider the split.
```

discovered_from=237-review.

### T132 — `fix(nix-tests):` k3s-full.nix extraImagesBumpGiB — count dashboard image preload

rev-p283 trivial. [`k3s-full.nix:177`](../../nix/tests/fixtures/k3s-full.nix) `extraImagesBumpGiB = builtins.length extraImages + (if envoyGatewayEnabled then 1 else 0);` — counts `extraImages` + envoy bump but NOT the dashboard image preload at `:166` (conditional on `envoyGatewayEnabled && dockerImages?dashboard`). The dashboard image (~50-100MB) fits in envoy's +1GB headroom today; becomes uncounted if the bundle grows or a future VM test uses dashboard without envoy.

Fix: `extraImagesBumpGiB = builtins.length extraImages + (if envoyGatewayEnabled then 1 else 0) + (if (envoyGatewayEnabled && (dockerImages ? dashboard)) then 1 else 0);` — or simplify to `(if envoyGatewayEnabled then (if dockerImages ? dashboard then 2 else 1) else 0)`. Update the comment at `:175` to mention dashboard: "envoyGatewayEnabled adds 2 images (envoy ~200MB + dashboard ~100MB if dockerImages?dashboard — 2 bump units when both present)." discovered_from=283-review.

### T133 — `refactor(crds):` hoist child_name to rio-crds — single source of truth for CLI+controller

rev-p237 trivial (extends T103/T114). `{wps}-{class.name}` child-naming formula duplicated 3×: [`builders.rs:43`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) `pub(super) fn child_name`, [`rio-cli/src/wps.rs:114`](../../rio-cli/src/wps.rs) (p237 ref) `format!("{name}-{}", c.name)` in `list()`, [`rio-cli/src/wps.rs:147`](../../rio-cli/src/wps.rs) `format!("{}-{}", wps.name_any(), class.name)` in `describe()`. T114 already consolidates the controller-internal `scaling.rs:1022` format! to use `builders::child_name`. T103 adds RFC-1123 length guard. Neither makes it visible to rio-cli.

Hoist to `rio-crds/src/workerpoolset.rs` as `pub fn wps_child_name(wps_name: &str, class_name: &str) -> String` (takes bare strings not `&WorkerPoolSet`/`&SizeClassSpec` so CLI doesn't need to construct a full WPS just to derive the name). Controller's `builders::child_name` becomes a thin wrapper: `rio_crds::wps_child_name(&wps.name_any(), &class.name)`. CLI's two sites call it directly. T103's RFC-1123 guard moves WITH the function to rio-crds.

If the reconciler later changes naming (long-name hash suffix, namespace prefix — T103's concern), CLI gets it for free. **SEQUENCE:** after P0237 merges (wps.rs exists), after T103/T114 land (controller-side consolidation done). discovered_from=237-review.

### T134 — `docs(proto):` lib.rs build_types dual-path — document incomplete-migration as intentional

rev-p376 trivial. [`rio-proto/src/lib.rs`](../../rio-proto/src/lib.rs) (p376 worktree at `:86-100`) adds `pub mod dag` + `pub mod build_types` domain re-exports with the comment "Callers MAY use either path — both resolve to the same struct. The domain-scoped path is encouraged for new code." P0376 migrated `dag::` to 100% (0 `types::Derivation*` refs remain) but `build_types::` only ~15 sites (SubmitBuild/BuildResult/BuildResultStatus) — ~210 refs to `BuildEvent`/`SchedulerMessage`/`WorkerMessage`/`Heartbeat*`/`WorkAssignment` still use `types::` path. Exit-criterion #7 of P0376 checked only the subset; plan T3 said "same-migrate-flow-as-T2" but only migrated what EC checked.

Two options: (a) complete the migration — sed `types::` → `build_types::` for the 210 refs; (b) document the dual-path as intentional and accept the inconsistency. The followup leans (b): "zero functional impact; lib.rs doc says MAY-use-either." But "defeats the purpose of domain re-export if most callers ignore it."

**Prefer (a):** sed-migrate the 210 refs. It's mechanical (`rg -l 'types::(BuildEvent|SchedulerMessage|WorkerMessage|Heartbeat|WorkAssignment|BuildInfo)' rio-*/ | xargs sed -i 's/types::BuildEvent/build_types::BuildEvent/g'` etc.) and makes the domain split actually serve its collision-tracking purpose. If (a) is too churny at dispatch (touches ~30 files), fall back to (b): extend the lib.rs doc-comment at `:85` with "dag:: migration is complete; build_types:: migration is opportunistic — existing types:: paths are valid, new code SHOULD use build_types::" and call it done. discovered_from=376-review.

### T135 — `docs(notes):` bughunter-log.md — mc168 null-result + T5 drift pointer

bughunt-mc168 null-result + T5 pre-window drift. Extends T40's bughunter-log.md with the mc168 entry. Per the followup: 7 merges reviewed (mc161-168), no cross-plan pattern above threshold; T5 actor-alive drift pre-dates window + folded into [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md). Append to `.claude/notes/bughunter-log.md`:

```
## mc=168 (eea3fb67..6b152a26, 2026-03-20)

Null result above threshold. 7 merges (P0356/docs-890214/P0236/P0291/P0234/P0371/P0372).

T1-T4 targets: all SAFE or already-tracked (rv substring-collision SAFE;
P0374 phantom-amend rebase clean; P0371 no catch-all 11/11 explicit;
P0291 is_cutoff refs = comments not dead code).

T5 actor-alive drift: 3-string variance (grpc/admin/worker_service)
PRE-DATES window — dd530256/47d5ce40. Folded into P0383
(admin-mod-split) T1 shared-fn extraction.

Smell counts: unwrap/expect=5 (2 test, 1 test, 2 moved-not-new);
silent-swallow=0; orphan-TODO=0; allow=0; lock-cluster=1 (moved);
panic/unsafe=0/0.
```

discovered_from=bughunter-mc168.

### T136 — `test(infra):` envoy image tag lockstep-enforcement (T119 sibling)

> **SIMPLIFIED by [P0387](plan-0387-airgap-bare-tag-derive-from-destnametag.md):** the bare-tag is now DERIVED from `pulled.envoy-distroless.destNameTag`, not hardcoded. There's no drift-to-assert. T136 collapses to: assert `dashboard.envoyImage` override is SET when `pulled.envoy-distroless` appears in `extraImages` (preload-but-no-override = wasted preload; override-but-no-preload = `ImagePullBackOff`). Different axis, simpler check.

rev-p379 trivial. Extends T119's device-plugin lockstep assert pattern to envoy images. [P0379](plan-0379-envoy-image-digest-pin.md) digest-pins `envoyImage` in [`values.yaml`](../../infra/helm/rio-build/values.yaml); the airgap preload set at [`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) hardcodes the envoy image tag at `:67` (and the gateway-helm render at `:40`). Same drift hazard as T119's device-plugin case: values.yaml bump without k3s-full.nix bump means VM tests exercise a different envoy than production ships.

Add a second lockstep check adjacent to T119's devicePlugin assert in the [`flake.nix`](../../flake.nix) helm-lint block (or the separate `.#checks.*.image-tag-sync` if T119 chose that route):

```nix
# Envoy image tag lockstep — same rationale as devicePlugin (T119).
# values.yaml envoyImage digest/tag must match docker-pulled.nix:94 entry
# (which k3s-full.nix preloads via envoyGatewayRender).
envoyValues=$(yq '.envoyImage.repository + ":" + .envoyImage.tag' infra/helm/rio-build/values.yaml)
envoyAirgap=$(grep -oP 'envoyproxy/envoy[^"]+' nix/docker-pulled.nix | head -1)
[ "$envoyValues" = "$envoyAirgap" ] || {
  echo "envoy image drift: values=$envoyValues vs airgap=$envoyAirgap"; exit 1
}
```

Exact extraction mechanism follows T119's choice — if T119 used `builtins.fromJSON`/readFile, mirror that; if grep/yq, mirror that. The two checks SHOULD share the same assertion-helper shape. If [P0379-T2](plan-0379-envoy-image-digest-pin.md) already generalized the yq-loop to cover ALL third-party images, this lockstep check becomes a second axis (values-vs-airgap, not values-digest-pinned) — complementary, not redundant. discovered_from=379-review.

### T137 — `refactor(nix-tests):` common.nix mkAssertChains — `c ? last` checks presence not truthiness

bughunt-mc175 latent footgun. [`common.nix:146`](../../nix/tests/common.nix) `if c ? last then ...` uses Nix's `?` operator, which checks **key presence** in attrset `c`, not **truthiness**. A chain written as `{last=false; name="foo"; msg="bar";}` would hit the if-branch (presence → true) and assert `last == c.name`, which is nonsensical for `last=false` (compares a bool to a string). Today's convention is "omit `last` for ordering chains, include `{last=true; ...}` for must-be-tail chains" — so `{last=false}` would never be written intentionally. But the code accepts it silently, and a future author reading `:146` might assume `?` is a truthiness check (common gotcha vs other languages where `?` is null-safety).

Fix: change `:146` to check truthiness explicitly:

```nix
if (c.last or false) then
```

This makes `{last=false}` equivalent to omitting `last` (falls through to else-branch), which is the intuitive semantics. The `:136-137` comment already says "`last` is bound lazily — only forced if a {last=true} chain is present" — that reads as truthiness-intent, confirming the fix aligns with the author's mental model.

Add a one-line comment at `:145` explaining the choice:

```nix
# (c.last or false) — truthiness, not presence (? checks presence;
# {last=false} should behave like omitting last, not assert bool==string)
```

discovered_from=bughunt-mc175.

### T138 — `refactor(proto):` lib.rs — admin_types re-export module parity

consol-mc175 trivial. [`rio-proto/src/lib.rs:76`](../../rio-proto/src/lib.rs) doc-comment describes four domain proto files: `types.proto`, `dag.proto`, `build_types.proto`, `admin_types.proto`. The file has `pub mod dag` (`:89-95`) and `pub mod build_types` (`:99-109`) re-export modules but NO `pub mod admin_types`. Asymmetry: callers can write `rio_proto::dag::DerivationNode` and `rio_proto::build_types::BuildEvent` for domain-scoped imports, but admin types are only reachable via `rio_proto::types::*` (flattened namespace).

Add parity re-export after `:109`:

```rust
/// Re-export of admin-domain types from [`types`]. Sourced from
/// `proto/admin_types.proto`. Same dual-path semantics as [`dag`].
pub mod admin_types {
    pub use crate::types::{
        // Populate at dispatch: grep admin_types.proto for message/enum names.
        // Expected: SizeClassStatus, TenantInfo, WorkerInfo, BuildInfo,
        // PoisonEntry, CutoffStats, GcStats, and their *Request/*Response pairs.
    };
}
```

**CARE:** the exact type-list depends on what landed in `admin_types.proto` post-P0376. Check `proto/admin_types.proto` at dispatch and enumerate every `message`/`enum` name. If the list is long (>20), consider a bulk `pub use crate::types::*;` re-export (simplest; all types are already public via `types`, the domain-scoped path is just a collision-tracking hint). Partner to T134 (which addresses the dag/build_types migration completeness). discovered_from=consol-mc175.

### T139 — `docs(scheduler):` db.rs ON CONFLICT is_ca — safety comment (bughunt-verified)

rev-p250 + bughunt-mc175 (already VERIFIED SAFE). [`db.rs:836-841`](../../rio-scheduler/src/db.rs) `ON CONFLICT (drv_hash) DO UPDATE SET ... is_ca = EXCLUDED.is_ca` raises the question: can the same `drv_hash` arrive with a different `is_ca` value, and if so is the UPDATE overwrite safe? bughunt-mc175 verified: `drv_hash` is deterministic (input-addressed: store path; CA: modular hash per [`hash.rs:66`](../../rio-nix/src/derivation/hash.rs)). Same `drv_hash` → same `.drv` content → same `outputs[]` → same `is_fixed_output()`+`has_ca_floating_outputs()` evaluation → same `is_ca`. The UPDATE is idempotent by construction; `EXCLUDED.is_ca` always equals the existing row's `is_ca`.

Close the loop with a comment at `:840` so future readers don't re-derive:

```rust
// is_ca UPDATE is idempotent-by-construction: drv_hash is
// deterministic (input-addressed=store path; CA=modular hash per
// rio-nix hash.rs hashDerivationModulo). Same drv_hash → same
// .drv content → same outputs[] → same is_ca. The EXCLUDED value
// always equals the existing row's value. Kept in the SET-list
// for insert-columns parity (UNNEST binds $10 regardless).
```

No test — the invariant is structural (hash-function determinism), not runtime behavior to assert. discovered_from=250-review.

### T140 — `refactor(scheduler):` actor_guards.rs — ACTOR_UNAVAILABLE_MSG const

rev-p383 trivial. [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md) collapsed the three drifted actor-dead error strings into one canonical string at [`actor_guards.rs:21`](../../rio-scheduler/src/grpc/actor_guards.rs). But the SAME literal appears at [`grpc/mod.rs:142`](../../rio-scheduler/src/grpc/mod.rs) inside `actor_error_to_status`'s `ChannelSend` arm — the comment at `:139` even says "Same string as `actor_guards::check_actor_alive`". Two literals that MUST match is one const:

```rust
// actor_guards.rs, above check_actor_alive:
/// Canonical actor-dead message. Used by both [`check_actor_alive`]
/// (pre-send liveness probe) and [`SchedulerGrpc::actor_error_to_status`]
/// (post-send ChannelSend failure). Operators grep for one signature.
pub(crate) const ACTOR_UNAVAILABLE_MSG: &str =
    "scheduler actor is unavailable (panicked or exited)";
```

Then `actor_guards.rs:21` → `Status::unavailable(ACTOR_UNAVAILABLE_MSG)`, and `grpc/mod.rs:142` → `Status::unavailable(actor_guards::ACTOR_UNAVAILABLE_MSG)`. The test at [`admin/tests.rs:473`](../../rio-scheduler/src/admin/tests.rs) `cluster_status_actor_dead_returns_unavailable` can also assert `msg.contains(actor_guards::ACTOR_UNAVAILABLE_MSG)` instead of a string literal — one const, three consumers, zero drift. discovered_from=383-review.

### T141 — `refactor(scheduler):` hoist actor_error_to_status to actor_guards.rs

rev-p383 trivial. [`grpc/mod.rs:128-154`](../../rio-scheduler/src/grpc/mod.rs) `actor_error_to_status` is `pub(crate) fn` on `SchedulerGrpc` but it's called from **seven** admin submodules via the awkward path `crate::grpc::SchedulerGrpc::actor_error_to_status`:

- [`admin/gc.rs:80`](../../rio-scheduler/src/admin/gc.rs), [`admin/mod.rs:177,299,324`](../../rio-scheduler/src/admin/mod.rs), [`admin/sizeclass.rs:39`](../../rio-scheduler/src/admin/sizeclass.rs), [`admin/workers.rs:30`](../../rio-scheduler/src/admin/workers.rs), plus `grpc/worker_service.rs:72,364` and the `send_and_await` helper at `grpc/mod.rs:167,171`

The fn takes no `&self` — it's a static method that happens to live on `SchedulerGrpc`. P0383 hoisted `check_actor_alive`+`ensure_leader` to `actor_guards.rs` for the same reason (shared between `SchedulerGrpc` and `AdminServiceImpl`). Move `actor_error_to_status` to the same file:

```rust
// actor_guards.rs, after ensure_leader:
/// Map an ActorError to a gRPC Status. Shared by SchedulerGrpc +
/// AdminServiceImpl handlers that forward actor-send errors.
pub(crate) fn actor_error_to_status(err: ActorError) -> Status {
    match err {
        // ... (body moves verbatim from grpc/mod.rs:128-154)
        ActorError::ChannelSend => Status::unavailable(ACTOR_UNAVAILABLE_MSG),
        // ...
    }
}
```

Callers shorten from `crate::grpc::SchedulerGrpc::actor_error_to_status` → `crate::grpc::actor_guards::actor_error_to_status` (or `use` it). Keep a `SchedulerGrpc::actor_error_to_status` one-line wrapper that delegates, so the existing `Self::actor_error_to_status` call sites in `grpc/` don't need sed. The test at [`grpc/tests.rs:806`](../../rio-scheduler/src/grpc/tests.rs) `test_actor_error_to_status_all_arms` calls via `SchedulerGrpc::` — wrapper keeps it unchanged. discovered_from=383-review.

**T140+T141 coupling:** T140's `ACTOR_UNAVAILABLE_MSG` const and T141's `actor_error_to_status` hoist both land in `actor_guards.rs`. Apply together — T141's hoisted `ChannelSend` arm USES T140's const. Single commit, two-part `refactor(scheduler):` message.

### T142 — `refactor(controller):` fixtures.rs — test_workerpoolset + test_wps_spec (P0380 sibling)

rev-p380 trivial. [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) collapsed 4× `WorkerPoolSpec {...}` test literals → `crate::fixtures::test_workerpool_spec()`. Same pattern ONE TABLE UP — `WorkerPoolSetSpec {...}` appears at 2 sites:

- [`scaling.rs:1434`](../../rio-controller/src/scaling.rs) `test_wps(name, ns, class_names)` — builds `WorkerPoolSetSpec { classes, pool_template: PoolTemplate {..Default::default()}, cutoff_learning: None }` + WorkerPoolSet wrapper with uid/ns
- [`workerpoolset/builders.rs:200`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) `test_wps_with_classes(names)` — same shape but `PoolTemplate` fields explicit (no `..Default::default()`) + different ns

The `builders.rs:200` variant sits inside a private `#[cfg(test)] mod tests` block — **unreachable** from `scaling.rs` (cfg(test) is per-module, not per-crate). The comment at [`scaling.rs:1415`](../../rio-controller/src/scaling.rs) explicitly acknowledges this: "Mirrors `test_wps_with_classes` in reconcilers/workerpoolset/builders.rs ... because that helper is in a private #[cfg(test)] mod inside builders.rs (unreachable cross-module)."

Extract to [`fixtures.rs`](../../rio-controller/src/fixtures.rs) (same file P0380 added `test_workerpool_spec` to):

```rust
use crate::crds::workerpoolset::{PoolTemplate, SizeClassSpec, WorkerPoolSet, WorkerPoolSetSpec};

/// Minimal WorkerPoolSetSpec with N classes. Each class gets
/// monotone-increasing cutoffs + dummy resources. PoolTemplate
/// uses Default for optional fields (the builder tests that need
/// explicit fields override locally).
pub fn test_workerpoolset_spec(class_names: &[&str]) -> WorkerPoolSetSpec {
    let classes = class_names.iter().enumerate()
        .map(|(i, n)| SizeClassSpec {
            name: (*n).to_string(),
            cutoff_secs: (i as f64 + 1.0) * 60.0,
            min_replicas: Some(1),
            max_replicas: Some(10),
            target_queue_per_replica: Some(5),
            resources: k8s_openapi::api::core::v1::ResourceRequirements::default(),
        })
        .collect();
    WorkerPoolSetSpec {
        classes,
        pool_template: PoolTemplate {
            image: "rio-worker:test".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        },
        cutoff_learning: None,
    }
}

/// Wrap spec with name + UID + ns (same as test_workerpool).
pub fn test_workerpoolset(name: &str, ns: &str, class_names: &[&str]) -> WorkerPoolSet {
    let mut wps = WorkerPoolSet::new(name, test_workerpoolset_spec(class_names));
    wps.metadata.uid = Some(format!("{name}-uid"));
    wps.metadata.namespace = Some(ns.into());
    wps
}
```

Migrate both call sites. `scaling.rs:1420-1447` `test_wps` body → `crate::fixtures::test_workerpoolset(name, ns, class_names)`. `builders.rs:183-223` `test_wps_with_classes` body → spec-from-fixture + local `features: vec!["kvm".into()]` override if the test depends on it (check at dispatch — if no test reads `features`, fixture default is fine). Delete the self-referential comment at `scaling.rs:1415-1418`. discovered_from=380-review.

**SOFT-CONFLICT P0381:** `scaling.rs` split plan — `test_wps` lives in the tests mod that moves to `scaling/per_class.rs`. If P0381 lands first, T142 targets the new path; if T142 lands first, P0381 carries the 1-line delegation forward. Sequence-independent, rebase-clean.

### T143 — `docs(notes):` bughunter-log.md — mc182 null-result entry

bughunt-mc182 null-result. Extends T40/T135's bughunter-log.md cadence-entry pattern. Append to [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md):

```
## mc=182 (<range>, 2026-03-20)

Null result above threshold. 7 merges reviewed (mc175-182).

<populate at dispatch: T1-T4 scan targets + smell-counts from the
bughunt-mc182 run output; format matches mc168 entry above>
```

If the bughunter output included specific SAFE/already-tracked verdicts, include them. If it was a bare null (zero findings, zero elevated smells), record that — cadence negatives are data (absence of accumulation = 7-merge window stayed clean). discovered_from=bughunter-mc182.

### T144 — `refactor(nix):` DerivationLike — add is_content_addressed() trait default

rev-p384 trivial. The disjunction `is_fixed_output() || has_ca_floating_outputs()` appears at [`translate.rs:390`](../../rio-gateway/src/translate.rs) and [`:428`](../../rio-gateway/src/translate.rs) (two identical call sites — the rev-p384 "4 sites" counts the duplicated comment blocks too). It's the canonical "is this derivation content-addressed in ANY way" predicate. The spec text at [`scheduler.md:258`](../../docs/src/components/scheduler.md) describes exactly this: "`is_ca` flag is set from `has_ca_floating_outputs() || is_fixed_output()`".

Add to the `DerivationLike` trait at [`mod.rs:149+`](../../rio-nix/src/derivation/mod.rs) (after `has_ca_floating_outputs` at `:187`):

```rust
/// Content-addressed in ANY form: either a strict FOD (hash_algo AND
/// hash both set on the single `out` output) or floating-CA (hash_algo
/// set, hash empty). This is the spec'd `is_ca` predicate — gateway
/// translate sets proto `DerivationNode.is_content_addressed` from this.
fn is_content_addressed(&self) -> bool {
    self.is_fixed_output() || self.has_ca_floating_outputs()
}
```

Then [`translate.rs:390`](../../rio-gateway/src/translate.rs) + `:428` → `drv.is_content_addressed()`. If [P0388](plan-0388-translate-rs-generic-node-builder.md) lands first (generic `build_node<D>`), there is ONE call site instead of two. Sequence-independent: both forms compile. discovered_from=384-review.

### T145 — `refactor(nix):` DerivationLike — narrow required-methods to {outputs, env, platform}

rev-p384 trivial. The `DerivationLike` trait at [`mod.rs:149-188`](../../rio-nix/src/derivation/mod.rs) declares six required accessors: `outputs()`, `input_srcs()`, `platform()`, `builder()`, `args()`, `env()`. The default-impl'd predicates (`is_fixed_output`, `has_ca_floating_outputs`, plus T144's `is_content_addressed`) only use `outputs()`. Zero callers use `builder()`, `args()`, or `input_srcs()` THROUGH the trait — all call the inherent accessors per the [`mod.rs:143-148`](../../rio-nix/src/derivation/mod.rs) "inherent-method-resolution wins" note.

Drop `builder()`, `args()`, `input_srcs()` from the trait. The trait becomes the predicate-bearing surface (3 required accessors, 3 default predicates). Inherent accessors stay for direct-use callers. Trait impls shrink from 6 methods to 3 each (both `Derivation` and `BasicDerivation`).

**CARE — [P0388](plan-0388-translate-rs-generic-node-builder.md) constraint**: that plan's `build_node<D: DerivationLike>` calls `drv.outputs()` + `drv.env()` + `drv.platform()` through the trait. Narrow to EXACTLY those three, not fewer. If P0388 hasn't dispatched, verify via `grep 'DerivationLike' rio-gateway/src/translate.rs` — if the generic builder uses more than these three, keep what it needs. discovered_from=384-review.

### T146 — `refactor(nix):` DerivationOutput::is_fixed_output → has_hash_algo (naming collision foot-gun)

rev-p384 trivial. [`mod.rs:130`](../../rio-nix/src/derivation/mod.rs) `DerivationOutput::is_fixed_output()` and [`mod.rs:171`](../../rio-nix/src/derivation/mod.rs) `DerivationLike::is_fixed_output()` share a name but answer DIFFERENT questions. The per-output version checks `!hash_algo.is_empty()` (true for floating-CA too). The trait version is the strict FOD predicate. The doc-comment at [`:118-129`](../../rio-nix/src/derivation/mod.rs) explains the misnomer: "semantics are closer to `is_content_addressed`". [P0384](plan-0384-is-fixed-output-strict-plus-trait.md)'s plan doc at `:7` flagged the foot-gun but deferred.

**Foot-gun shape**: `drv.outputs()[0].is_fixed_output()` (per-output, loose) vs `drv.is_fixed_output()` (trait, strict) look nearly identical but diverge on floating-CA. The bug P0384 fixed at the old `translate.rs:368` was exactly this confusion.

Rename `DerivationOutput::is_fixed_output` → `has_hash_algo` (accurate: that's what it checks). Grep callers at dispatch:

```bash
grep -rn '\.is_fixed_output()' rio-*/src/ | grep -v 'drv.is_fixed_output\|basic_drv.is_fixed_output\|self.is_fixed_output'
```

Expected: zero prod callers post-P0384 (the `translate.rs:368` loose-form is gone). Rename is fn-name + doc-comment only. The `:118-129` apology doc-comment simplifies (no more "naming note — this is a misnomer" block). `DerivationLike::is_fixed_output()` stays unchanged — it's the CORRECT strict-FOD name. discovered_from=384-review.

### T147 — `docs(notes):` bughunter-log.md — mc189 null-result entry

bughunt-mc189 null-result. Same cadence-entry pattern as T40/T135/T143. Append to [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md):

```
## mc=189 (<range>, 2026-03-20)

Null result above threshold. 7 merges reviewed (mc182-189).

<populate at dispatch: scan targets + smell-counts; format matches mc182/mc168>
```

Third consecutive null cadence entry (mc175 → mc182 → mc189). The 7-merge window stayed clean through the P0376/P0381/P0384 refactor run — no fresh accumulation despite high structural-churn merges. discovered_from=bughunter-mc189.

### T148 — `refactor(scheduler):` completion.rs ca_hash_compares — add outcome=error third label

rev-p251 trivial (labeling seed). [P0251](plan-0251-ca-cutoff-compare.md)'s merged code at [`completion.rs:309-333`](../../rio-scheduler/src/actor/completion.rs) collapses RPC-error AND timeout into `matched=false` → counter labels them `outcome="miss"`. An operator seeing high `miss` rate can't distinguish "many CA builds producing novel content" (healthy) from "store RPC timing out/erroring" (infrastructure problem). The `debug!` at `:317-325` logs the distinction but doesn't surface it metrically.

Split the outcome into three:

```rust
let (matched, outcome) = match tokio::time::timeout(
    rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
    store_client.clone().content_lookup(req),
)
.await
{
    Ok(Ok(resp)) => {
        let m = !resp.into_inner().store_path.is_empty();
        (m, if m { "match" } else { "miss" })
    }
    Ok(Err(e)) => { debug!(...); (false, "error") }
    Err(_elapsed) => { debug!(...); (false, "error") }
};
all_matched &= matched;
metrics::counter!(
    "rio_scheduler_ca_hash_compares_total",
    "outcome" => outcome
).increment(1);
```

Cutoff semantics unchanged (`all_matched` still folds `false` on error — error means don't-skip, safe). Only the metric label gains resolution. Update `describe_counter!` wherever P0251-T3 registered it (three-outcome doc). discovered_from=251-review. Soft-dep [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T54 (slow-store regression test — same code region, additive). completion.rs count=26 HOT — this is a ~10L rewrite inside an existing match, low semantic conflict.

### T149 — `refactor(gateway):` metrics_registered.rs — GATEWAY_METRICS 3 omissions (STOPGAP)

rev-p367 trivial (STOPGAP — [P0394](plan-0394-spec-metrics-derive-from-obs-md.md) supersedes by deriving the const from obs.md at build time). MODIFY [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs) at `:6-15`. Add three missing entries:

```rust
const GATEWAY_METRICS: &[&str] = &[
    // ... existing 8 entries ...
    "rio_gateway_quota_rejections_total",  // obs.md:74 + lib.rs:76, pre-existing omission
    "rio_gateway_auth_degraded_total",     // P0367 added obs.md:75 + lib.rs:70
    "rio_gateway_jwt_mint_degraded_total", // lib.rs:66 only — see P0295-T91 for obs.md add
];
```

The spec→describe check at `:21` passes today because the const is a SUBSET of the describe set — missing entries are blind spots, not failures. Adding them makes the check cover these three. **SUPERSEDED BY P0394:** that plan deletes this const entirely and derives it. If P0394 dispatches first, skip T149. discovered_from=367.

### T150 — `docs(gateway):` quota.rs:91 stale from_maybe_empty → normalize_key_comment

rev-p367 trivial. MODIFY [`rio-gateway/src/quota.rs`](../../rio-gateway/src/quota.rs) at `:91`. P0367 changed the SSH auth boundary from `from_maybe_empty` to `normalize_key_comment` (calling `NormalizedName::new` directly). The doc-comment still references the old function name. Behavior equivalent (both produce `Option<NormalizedName>`); reference is stale. One-word swap. discovered_from=367.

### T151 — `docs(store):` auth.rs:101 soften unreachable claim (Unicode whitespace edge)

rev-p367 trivial. MODIFY [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs) at `:101`. The PG `CHECK` constraint uses POSIX `[[:space:]]` (6 ASCII whitespace chars); Rust `NormalizedName::new` uses `char::is_whitespace` (Unicode `White_Space` property, a superset including NBSP U+00A0, EN SPACE U+2002, etc.). A `tenant_name` with only non-ASCII whitespace between words would PASS the PG CHECK but FAIL Rust normalization.

The `:101` comment claims the 500-fail-loud branch is "provably unreachable for post-migration rows" — overclaimed for this Unicode edge. Behavior is CORRECT (500 fires, doesn't silently pass); the claim is wrong. Either soften to "unreachable for ASCII-only tenant names (migration CHECK uses POSIX `[[:space:]]` — Unicode whitespace is a gap)" OR tighten the PG regex to `[^[:graph:]]` or similar Unicode-aware pattern. Prefer the comment-soften; tightening the CHECK is a migration (out of trivial-batch scope). discovered_from=367.

### T152 — `refactor(controller):` event-reason pub(crate) consts + body_substr param rename

rev-p365 trivial. MODIFY [`rio-controller/src/reconcilers/workerpool/mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs) + `tests.rs`. [P0365](plan-0365-warn-on-spec-degrades-helper.md) landed `warn_on_spec_degrades` with event reason strings inlined at 3 sites: `mod.rs:255` (emit) + `tests.rs:1648` + `tests.rs:1709` (asserts). The crate has `pub const MANAGER`/`FINALIZER` precedent at `workerpoolset/mod.rs:70-85`; event reasons aren't const-ified anywhere yet (`scaling/standalone.rs:360-362` also inlines `ScaledUp`/`ScaledDown`).

Add `pub(crate) const REASON_SPEC_DEGRADED: &str = "SpecDegraded";` (re-check exact string at dispatch) near the existing consts. Migrate the 3 sites. A future 3rd degrade-reason (plan anticipates at `mod.rs:296-298`) would add a 4th inline site without the const.

While here: `event_post_scenario` test helper at `tests.rs:1081` has parameter named `reason` but accepts any body substring (e.g., `:1682` passes note-text, not a reason string). Rename to `body_substr` for clarity. Pure rename, zero behavior change. discovered_from=365. **Soft-conflict [P0396](plan-0396-workerpool-tests-rs-split-submodule-seams.md):** T152's `tests.rs:1648/:1709` edits move to `tests/disruption_tests.rs` after that split; sequence T152 FIRST or coordinate.

### T153 — `refactor(worker):` RIO_TEST_PREFETCH_DELAY_MS — TODO-tag or remove

rev-p299 trivial. MODIFY [`rio-worker/src/runtime.rs`](../../rio-worker/src/runtime.rs) at `:814-819`. [P0299](plan-0299-staggered-scheduling-cold-start.md)'s env-hook `RIO_TEST_PREFETCH_DELAY_MS` is implemented but **UNUSED** — no VM test sets it. Plan EC `:161` said "VM test asserts ordering on the stream" which would use this hook; the actual VM scenario at [`scheduling.nix:1234`](../../nix/tests/scenarios/scheduling.nix) is passive metrics-only.

Two options: (a) remove the ~8-line hook as dead code (clippy won't flag it — env-var reads aren't dead-code), OR (b) tag with `// TODO(P0311-T58): ordering-proof VM scenario uses this`. Prefer **(b)** — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T58 tracks the `on_worker_registered` test-gap; the hook becomes live when that lands. Tagged TODO is better than silent dead code. discovered_from=299.

### T154 — `refactor(dashboard):` logStream:42 untagged deferral → TODO(P0NNN)

rev-p279 trivial. MODIFY [`rio-dashboard/src/lib/logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts) at `:42`. Comment says "reconnect-on-transient-error story is a later plan" — untagged deferral. `sinceLine: 0n` at `:45` is hardcoded; resume-from-offset would use `chunk.firstLineNumber + chunk.lines.length` but no plan owns that yet.

No existing plan covers reconnect-on-transient-error. Write a followup via `onibus state followup` at dispatch (or this T creates a stub `TODO(P0NNN): reconnect-on-transient-error using sinceLine resume` where NNN is allocated at dispatch via `onibus state followup`). Alternatively, tag `// TODO(P0392): virtualization plan — reconnect-on-transient-error may ride along` if [P0392](plan-0392-logstream-quadratic-virtualize.md) picks up reconnect as a T5. Check at dispatch. discovered_from=279. **Post-P0279-merge** (file doesn't exist pre-merge).

### T155 — `refactor(dashboard):` LogViewer drvPath prop — TODO(P0280) tag

rev-p279 trivial. MODIFY [`rio-dashboard/src/components/LogViewer.svelte`](../../rio-dashboard/src/components/LogViewer.svelte) at `:12-13` + [`rio-dashboard/src/lib/logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts) at `:31,:45`. The `drvPath` prop/param is **dead in practice** — [`BuildDrawer.svelte:108`](../../rio-dashboard/src/components/BuildDrawer.svelte) passes only `buildId`. Test at `logStream.test.ts:170-180` exercises a path never invoked by UI.

Likely forward-looking for [P0280](plan-0280-getbuildgraph-node-click-drv-drawer.md) node-click → drvPath. Tag both sites with `// TODO(P0280): drvPath live once node-click wires it`. Don't delete — P0280 will use it. discovered_from=279. **Post-P0279-merge.**

### T156 — `refactor(scheduler):` gc_tests.rs:7 redundant StreamExt import

rev-p386 trivial. MODIFY [`rio-scheduler/src/admin/tests/gc_tests.rs`](../../rio-scheduler/src/admin/tests/gc_tests.rs) at `:7`. `use tokio_stream::StreamExt;` is redundant — already pulled in via `use super::*;` (mod.rs `:14` imports it, `:74` uses it in `collect_stream`). Clippy silent because the trait IS used at `:57` (`store_stream.next()`). Cosmetic. Remove the explicit re-import. Other 4 submodules don't re-import anything from mod.rs namespace. discovered_from=386. **Post-P0386-merge** (file exists only after the split — which is now DONE per dag).

### T157 — `fix(scheduler):` completion.rs malformed-output-hash → counter increment

rev-p251 trivial (coord-flagged; partners with **T148** — T148 adds `outcome=error` for RPC-fail/timeout, T157 adds `outcome=malformed` for the 32-byte guard, different edge). MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) at `:295-304`. The 32-byte guard logs `debug!` + sets `all_matched = false` + `continue` but doesn't increment `rio_scheduler_ca_hash_compares_total` with any label. A malformed output silently drops from the metric — the counter under-counts total CA derivations compared.

Add a `"malformed"` outcome label:

```rust
if output.output_hash.len() != 32 {
    debug!(/* ... */);
    metrics::counter!("rio_scheduler_ca_hash_compares_total",
                      "outcome" => "malformed").increment(1);
    all_matched = false;
    continue;
}
```

Also update [`docs/src/observability.md`](../../docs/src/observability.md) Scheduler Metrics table — the `rio_scheduler_ca_hash_compares_total` row (if present after [P0295](plan-0295-doc-rot-batch-sweep.md)-T91 lands) should list `outcome=match|miss|error|malformed` labels (T148's `error` + T157's `malformed` + [P0393](plan-0393-ca-contentlookup-serial-timeout.md)'s `skipped_after_miss`). Consolidate all five labels in one table edit. discovered_from=251.

### T158 — `refactor(scheduler):` resolve.rs serialize_resolved — dedupe against rio-nix aterm (fixes doc-lie)

rev-p253 trivial + doc-bug (coupled). MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) at `:448-579` (`serialize_resolved` + `write_aterm_string`). The ~110-line hand-rolled serializer duplicates [`rio-nix/src/derivation/aterm.rs`](../../rio-nix/src/derivation/aterm.rs) `to_aterm` (`:288-335`) + `write_aterm_string` (`:12-25`, crate-private). Comment at `:562-565` says "12 lines cheaper than making pub" — dup is ~110L not 12. Doc-comment at `:444-447` says "Uses Derivation::to_aterm on a re-parsed struct" — the body hand-rolls it instead.

Two dedup routes:
- **(a)** Add `pub fn to_aterm_resolved(&self, drop_input_drvs: &BTreeSet<&str>, extra_input_srcs: impl Iterator<Item=&str>) -> String` to `rio-nix/src/derivation/aterm.rs`. Reuses existing `write_aterm_string` + outputs/platform/builder/args/env serialization. ~30L add to rio-nix, ~110L delete from rio-scheduler.
- **(b)** Add `Derivation` setters (`set_input_drvs`, `set_input_srcs`) so caller can mutate then call `to_aterm()`. Less invasive API; slightly more allocation (clone-mutate-serialize vs direct-serialize).

Prefer **(a)**. `serialize_resolved`'s signature simplifies to a thin wrapper or deletes entirely. The doc-comment at `:444-447` becomes ACCURATE. **Soft-conflict [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md):** that plan changes the inputDrvs-drop semantics (ALWAYS empty, not filtered). If P0398 lands first, `to_aterm_resolved` takes just `extra_input_srcs` (no `drop_input_drvs` param). Coordinate at dispatch. discovered_from=253.

### T159 — `refactor(scheduler):` downstream_placeholder — Result<String> → String

rev-p253 trivial. MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) at `:150-178`. Body has zero fallible ops: `strip_suffix`/`format!`/`Sha256::digest`/`nixbase32::encode` all infallible. Every call site wastes a `?` or `.unwrap()`. Change signature to `-> String`. Update callers (grep `downstream_placeholder(` — one at `:264` inside `resolve_ca_inputs`, plus tests). discovered_from=253.

### T160 — `refactor(scheduler):` ca/mod.rs — re-export insert_realisation_deps + CaResolveInput

rev-p253 trivial. MODIFY [`rio-scheduler/src/ca/mod.rs`](../../rio-scheduler/src/ca/mod.rs) at `:18-21`. `insert_realisation_deps` is `pub async fn` in `resolve.rs` but NOT in the re-export list. [P0254-T7](plan-0254-ca-metrics-vm-demo.md) wires it into `handle_success_completion` expecting `ca::insert_realisation_deps` — add to `pub use`.

While here: `downstream_placeholder` is a pure Nix-protocol concept — arguably belongs in `rio-nix` alongside `StorePath`/`Derivation`. Not moving it (cross-crate churn > value), but add a doc-comment pointing at the Nix source `libstore/downstream-placeholder.cc` for anyone wondering why it's in rio-scheduler. **If T158 route-(a) lands**, consider hoisting `downstream_placeholder` to rio-nix alongside `to_aterm_resolved` — same commit, zero extra churn. discovered_from=253.

### T161 — `perf(scheduler):` resolve_ca_inputs — batch SELECT via UNNEST

rev-p253 perf. MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) at `:247-275`. `query_realisation` is called inside a for-loop per `(input × output_name)`, each `.await` blocking the next — N×M serial DB round-trips. Contrast [`insert_realisation_deps:395-408`](../../rio-scheduler/src/ca/resolve.rs) which correctly UNNEST-batches the INSERT.

Batch the SELECT symmetrically: single query `WHERE (drv_hash, output_name) IN (SELECT * FROM UNNEST($1::bytea[], $2::text[]))`. Build the bind arrays upfront (all `modular_hash` + all `output_name` pairs), fire one round-trip, partition results back into the per-input lookups map.

**Low urgency** — resolve only fires for floating-CA-depends-on-CA chains (rare), and [`collect_ca_inputs`](../../rio-scheduler/src/actor/dispatch.rs) is stubbed (returns `[]`) until [P0254-T6](plan-0254-ca-metrics-vm-demo.md) wires `ca_modular_hash`. This is ahead-of-time optimization. But it's the same pattern already in the file at `:395-408`, so consistency argument holds. discovered_from=253.

### T162 — `docs(notes):` bughunter-log.md — mc196 null-result entry (ContentLookup self-match was the finding)

bughunt-mc196 null-result marker. MODIFY [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md) — add entry for `mc=196`. The bughunt WAS a finding (ContentLookup self-match — routed to [P0397](plan-0397-ca-contentlookup-self-match-exclude.md)), so the log entry documents that + the audit sweep counts. Sibling to T143's mc182 and T147's mc189 entries. Format: `## mc=196 (<range>, 2026-03-20)` + smell counts + "FINDING: ContentLookup self-match → P0397" line. discovered_from=bughunter-mc196.

### T163 — `docs(notes):` bughunter-log.md — mc203 null-result entry (all 5 verdicts CLEAN)

bughunt-mc203 null-result marker. MODIFY [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md) — add entry for `mc=203` (range e5feb62c..8155e004, 7 merges P0279/docs-267443/P0244/docs-758618/P0253/P0347/P0373). All 5 coord-seeded targets CLEAN:

```markdown
## mc=203 (e5feb62c..8155e004, 2026-03-20)

7 merges. Smell counts: unwrap/expect=17 all-test-code, non-test=0.
silent-swallow=0 (one `let _=parse(text)?` has ?-propagation, discards value not error).
orphan-TODO=0 (8 TODOs all tagged P0254). allow=1 (unused_mut tagged P0254). lock-cluster=0.

Coord targets verdicts:
(1) P0253 ATerm dup — COVERED by P0304-T158 + P0398 soft-conflict. Escape-tables byte-identical (arm-order cosmetic).
(2) P0347 deadline mid-build — NOT A BUG: terminationGracePeriodSeconds=7200 inherited; worker run_drain waits in-flight; ephemeral.rs:286-292 documents heartbeat-timeout handling.
(3) P0373 const{} consistency — CONSISTENT: 3 uses all `const{assert!()}` form; no mix; no contract.rs (coord mention inaccurate).
(4) P0244×P0295 conflict — CLEAN: git merge-tree --write-tree sprint-1 p295 → c66f4b29 no-markers; p295 fork 91ccad72=mc~198 post-P0244.
(5) T-collision 2nd — NONE: docs-875101 writes T158+/T62+ correctly; phantom-commit e9b4b46b byte-identical auto-drops.

ERROR-PATH: 0 new production Result fns. RESOURCE: none suspicious.
```

Sibling to T143/T147/T162. discovered_from=bughunter-mc203.

### T164 — `refactor(config):` mutants.toml — delete pad-guard excludes (drift-brittle line anchors)

rev-p373 trivial. MODIFY [`.config/mutants.toml`](../../.config/mutants.toml) at `:59-77` + MODIFY [`rio-nix/src/protocol/wire/mod.rs`](../../rio-nix/src/protocol/wire/mod.rs) at `:87` + `:158`. The `exclude_re` entries use `:87:12`, `:158:16`, `:229:41`, `:230:18` line:col anchors — brittle to any insert above. Silent un-exclude → spurious MISSED in weekly sweep.

For `:87`/`:158` pad-guard entries: per the toml comment itself, `if pad > 0` is a perf no-op (`read_exact`/`write_all` on 0-byte slice is no-op). DELETE the guard at both sites in `wire/mod.rs` — eliminates both exclusions + the drift problem. Zero behavior change (nextest proves).

For `:229`/`:230` (1 GiB total cap): no code-level delete route. Add a note in the toml comment: "re-verify after any wire/mod.rs edit above :229". Optionally add a `just mutants-check-excludes` tripwire that runs `cargo mutants --list` and greps each exclude_re, failing if any match 0 times. Defer the tripwire unless drift hits in practice. discovered_from=373.

### T165 — `refactor(nix):` proptest-regressions — commit policy consistency

rev-p373 trivial. Worktree p373 had uncommitted `rio-nix/proptest-regressions/derivation/aterm.txt` (4 seeds) + `rio-nix/proptest-regressions/protocol/wire/mod.txt` (3 seeds). NOT gitignored. Precedent [`661e5395`](https://github.com/search?q=661e5395&type=commits) commits these (`rio-nix/proptest-regressions/protocol/build.txt` tracked).

Check [`aterm.rs:720-724`](../../rio-nix/src/derivation/aterm.rs) doc-comment — it mentions a crane-fileset concern (seeds in src → drv hash changes → cache-miss). Grep `proptest-regressions` in `flake.nix` fileset filters at dispatch. If excluded: commit the seeds (route **a**, matches build.txt precedent). If included in crane src: add `proptest-regressions/` to `.gitignore` + the crane fileset exclusion (route **b**, avoids cache-miss). discovered_from=373.

### T166 — `perf(nix):` wire boundary tests — drop() between heavy allocations

rev-p373 small-perf. MODIFY [`rio-nix/src/protocol/wire/mod.rs`](../../rio-nix/src/protocol/wire/mod.rs) at `:762-785`, `:906-920`. Three boundary tests allocate heavy buffers without inter-allocation drop:

- `write_bytes_max_string_len_boundary` (`:906-920`): `at_max` (64MiB) + `over` (64MiB+1) coexist — ~128MiB peak. Add `drop(at_max);` before constructing `over`.
- `write_strings_max_boundary_error_carries_count` (`:762-785`): writes 1M u64s to growable `Vec<u8>`. Switch to `tokio::io::sink()` (consistent with pairs variant at `:884`; sink avoids 8MiB allocation).
- `write_string_pairs_max_boundary` (`:882-898`): already uses sink — no change.

All under 30s slow-timeout today (0.718s reported). Preemptive — cargo-mutants runs these ~100× (once per wire-mutant). `drop()` halves peak; sink-over-Vec saves 8MiB. discovered_from=373.

### T167 — `refactor(dashboard):` logStream.test + App.test — migrate to adminMock

rev-p389 trivial. MODIFY [`rio-dashboard/src/lib/__tests__/logStream.test.ts`](../../rio-dashboard/src/lib/__tests__/logStream.test.ts) + `rio-dashboard/src/__tests__/App.test.ts`. Both still carry the pre-P0389 `vi.hoisted`/inline-mock pattern — out of P0389's 6-file scope but same consol-mc185 duplication class. [`logStream.test.ts:21-23`](../../rio-dashboard/src/lib/__tests__/logStream.test.ts) comment already anticipates the shared helper.

Migrate both to `adminMock` from [`test-support/admin-mock.ts`](../../rio-dashboard/src/test-support/admin-mock.ts). `logStream.test.ts` may need a `flushMicrotask(rounds)` helper for generator-drain — shared with [`GC.test.ts:50-53`](../../rio-dashboard/src/pages/__tests__/GC.test.ts)'s tick+Promise.resolve loop. Distinct from `flushSvelte`'s fake-timer advance. Place it in `test-support/flush.ts` (NEW or extend existing). **Post-P0389-merge.** discovered_from=389.

### T168 — `refactor(dashboard):` adminMock — getBuildGraph empty-default

rev-p389 trivial (advisory for P0280 dispatch). MODIFY [`rio-dashboard/src/test-support/admin-mock.ts`](../../rio-dashboard/src/test-support/admin-mock.ts) at `:54`. `adminMock.getBuildGraph` is bare `vi.fn()` (returns `undefined`). When [P0280](plan-0280-dashboard-dag-viz-xyflow.md) lands DagView/Graph calling `await admin.getBuildGraph({buildId})` at mount, any page embedding it needs `.mockResolvedValue({nodes:[],edges:[]})` or crashes on `undefined.nodes`.

```typescript
getBuildGraph: vi.fn().mockResolvedValue({ nodes: [], edges: [], truncated: false, totalNodes: 0 }),
```

Per-test overrides via `.mockResolvedValueOnce(...)` still work. Consider same empty-default pattern for `listWorkers`/`listBuilds`/`clusterStatus` — hardens against render-time undefined.X in embeds-component-that-fetches tests. **Post-P0389-merge.** discovered_from=389. Soft-dep [P0280](plan-0280-dashboard-dag-viz-xyflow.md) (getBuildGraph caller arrives with it).

### T169 — `refactor(scheduler):` MAX_CASCADE_DEPTH → MAX_CASCADE_NODES (naming)

rev-p252 trivial. MODIFY [`rio-scheduler/src/dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) at `:14-21`. The const caps NODE-COUNT (iterations = pops from frontier stack), not BFS tree-depth. Comment at `:14-18` correctly says "each iteration is one `find_cutoff_eligible` call" but the const name + warn-log field `depth=` both imply tree-depth. For wide DAGs (fanout >1), 1000 nodes hit before depth>3.

Rename `MAX_CASCADE_DEPTH` → `MAX_CASCADE_NODES`. Update the warn-log field name (grep `depth,` near `tracing::warn!` in the cascade loop). Keep metric name `rio_scheduler_ca_cutoff_depth_cap_hits_total` (non-breaking); clarify semantics in its `describe_counter!` doc-string ("node-count cap, not tree-depth — operator should review if non-zero"). discovered_from=252. **Soft-conflict [P0399](plan-0399-all-deps-completed-skipped-hang.md)-T1:** both touch `dag/mod.rs` — T169 at `:14-21`, that plan at `:411-421`; non-overlapping.

> **SCOPE +2 sites (rev-p405):** [P0405](plan-0405-bfs-walker-speculative-cascade-dedup.md)'s BFS-walker extraction added two more const references: [`completion.rs:84`](../../rio-scheduler/src/actor/completion.rs) (`speculative_cascade_reachable` call in `verify_cutoff_candidates`) and [`dag/mod.rs:646`](../../rio-scheduler/src/dag/mod.rs) (same helper called in `cascade_cutoff`). The rename now touches BOTH files. Doc-comment refs at [`completion.rs:68`](../../rio-scheduler/src/actor/completion.rs) and [`dag/mod.rs:635`](../../rio-scheduler/src/dag/mod.rs) also mention the const — update prose to "node-count cap" there too. Full grep at dispatch: `grep -n MAX_CASCADE rio-scheduler/src/` → expect 16 hits pre-rename (incl 9 in dag/tests.rs), 0 `_DEPTH` / 7 `_NODES` post-rename.

### T170 — `docs(dashboard):` graphLayout.ts sourcePosition comment — delete or rewrite

rev-p280 doc-bug. MODIFY [`rio-dashboard/src/lib/graphLayout.ts`](../../rio-dashboard/src/lib/graphLayout.ts) at `:118-122`. Comment says `sourcePosition`/`targetPosition` use string literals (`"bottom"`/`"top"`) but code at `:128-139` does NOT set them. Edge routing works via [`DrvNode.svelte`](../../rio-dashboard/src/components/DrvNode.svelte) `<Handle>` components (`Position.Top`/`Position.Bottom`). Comment describes a mechanism that was removed or never implemented. Delete, or rewrite to explain Handles-drive-routing. **Post-P0280-merge.** discovered_from=280. **Soft-conflict [P0400](plan-0400-graph-page-skipped-worker-race.md)-T1:** same file, non-overlapping (`:43-83` vs `:118-122`).

### T171 — `refactor(dashboard):` Graph.svelte poll — stop once all nodes terminal

**SUPERSEDED BY [P0400](plan-0400-graph-page-skipped-worker-race.md)-T3.** This T-item tracked the rev-p280 trivial finding at [`Graph.svelte:186`](../../rio-dashboard/src/pages/Graph.svelte) (5s poll runs unconditionally after build terminal). That plan absorbs it as T3 since it's the same edit region as the Worker-race fix (T2 there). If P0400 dispatches, skip T171. If P0400 is deferred/rejected, T171 stands alone: `r.nodes.every(n => TERMINAL.has(n.status)) → clearInterval`. **Post-P0280-merge.** discovered_from=280.

### T172 — `docs(harness):` batch-target stale placeholder refs — sed map

consolidator-mc205 trivial. 13 live stale 9-digit placeholder refs in batch-target docs — the [P0304-T30 `rename-unassigned` grep-pass](plan-0304-trivial-batch-p0222-harness.md) tooling fix isn't landed yet, so 3 prior docs-merges (`ed32f1a4`, `25499380`, `b7b4f67f`) left cross-refs pointing at placeholder numbers. Implementer navigation broken for P0311-T33/T41 + P0295-T23-adjacent.

Sed mapping (verified against git rename history at the cited commits):

| Placeholder | Real | Plan | Via commit |
|---|---|---|---|
| `996394101` | `0353` | [P0353](plan-0353-sqlx-migration-checksum-freeze.md) | [`94cf5d41`](https://github.com/search?q=94cf5d41&type=commits) rename |
| `996394104` | `0356` | [P0356](plan-0356-split-scheduler-grpc-service-impls.md) | [`94cf5d41`](https://github.com/search?q=94cf5d41&type=commits) rename |
| `997105501` | `0361` | [P0361](plan-0361-sigint-graceful-load50drv-ordering.md) | [`057f6bf9`](https://github.com/search?q=057f6bf9&type=commits) rename |
| `997105502` | `0362` | [P0362](plan-0362-extract-submit-build-grpc-helper.md) | [`057f6bf9`](https://github.com/search?q=057f6bf9&type=commits) rename |
| `997443701` | `0363` | [P0363](plan-0363-upload-references-count-buckets.md) | [`ed32f1a4`](https://github.com/search?q=ed32f1a4&type=commits) rename |
| `998902141` | `0374` | [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) | [`25499380`](https://github.com/search?q=25499380&type=commits) direct-add |
| `998902142` | `0375` | [P0375](plan-0375-bloom-expected-items-crd-knob.md) | [`25499380`](https://github.com/search?q=25499380&type=commits) direct-add |

Affected files + line-refs from consolidator scan + QA sweep:

- [`plan-0287-trace-linkage-submitbuild-metadata.md:234`](plan-0287-trace-linkage-submitbuild-metadata.md) — `996394104` in soft_deps
- [`plan-0295-doc-rot-batch-sweep.md:500`](plan-0295-doc-rot-batch-sweep.md), `:700`, soft_deps fence — `997443701`, `998902142`, `996394104` in cross-links + table cell + soft_deps
- [`plan-0311-test-gap-batch-cli-recovery-dash.md`](plan-0311-test-gap-batch-cli-recovery-dash.md) `:1175,:1196,:1198,:1453,:2128,:2196,:2248` + soft_deps fence — `997443701`, `998902141`, `997105501` in cross-links + Tracey refs + soft_deps
- **[`plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md)** (this doc) soft_deps fence + note body — `996394101`, `997105502`, `998902141`, `998902142` (QA-caught self-referential stale refs)

Also rewrite `plan-<placeholder>-*.md` link targets to the real filenames (the plan docs themselves were renamed by `rename-unassigned`; only the REFERENCES to them are stale). Note-body prose mentions (e.g., `P0353` / `P0374-T3`) in deps-fence `note` strings also need rewrite — grep hits include raw integers in JSON soft_deps AND P-prefixed prose. STRUCTURAL: fires every docs-merge that batch-appends cross-refs until T30 lands. discovered_from=consolidator-mc205.

### T173 — `docs(agents):` git diff 2-dot → 3-dot in agent markdowns

consolidator-mc205 trivial. 10 instances of `git diff $TGT..HEAD` / `git diff $TGT..<branch>` (2-dot) across 5 agent markdowns should use `$TGT...` (3-dot, merge-base-relative). [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md)-T1 fixed the `onibus behind-check` 2-dot; agent markdowns didn't follow. For rebased branches 2-dot==3-dot so rarely bites — but on behind-branches, 2-dot includes sprint-1's own diffs → false-positives for reviewer/validator. Same inherited-idiom class as P0306's onibus fix.

Instances:
- [`.claude/agents/rio-impl-reviewer.md`](../agents/rio-impl-reviewer.md) `:25`, `:73`
- [`.claude/agents/rio-impl-validator.md`](../agents/rio-impl-validator.md) `:63`, `:64`
- [`.claude/agents/rio-ci-flake-validator.md`](../agents/rio-ci-flake-validator.md) `:15`, `:29`, `:30`, `:40`, `:41`
- [`.claude/agents/rio-plan-reviewer.md`](../agents/rio-plan-reviewer.md) `:25`

**NOT:** [`.claude/agents/rio-impl-merger.md:58`](../agents/rio-impl-merger.md) — that's a `git log` range for convco-check, 2-dot is correct there (range, not diff). Mechanical sed, low risk. discovered_from=consolidator-mc205.

### T174 — `refactor(nix):` ATerm T158 addendum — include to_aterm_modulo (3rd copy)

consolidator-mc205 trivial (scope addendum to T158). T158 tracked [`serialize_resolved`](../../rio-scheduler/src/ca/resolve.rs) (`:448-579`) vs [`to_aterm`](../../rio-nix/src/derivation/aterm.rs) (`:288-335`) as 2-way dup. [`to_aterm_modulo`](../../rio-nix/src/derivation/aterm.rs) (`:349-412`) is a **third** ~60L near-copy: outputs-loop `:358-378` byte-identical to `:293-309`; inputDrvs-loop differs only in rewrite-map-sort. The shared `write_aterm_tail` already extracts inputSrcs/platform/builder/args/env tail; remaining duplication is outputs-loop (3× identical) + inputDrvs-loop (3× structurally identical, differ in filter/rewrite).

T158 route-(a)'s `to_aterm_resolved` generalizes to `to_aterm_with(outputs_mask, input_drvs_transform, extra_input_srcs)` collapsing all three. Net -150L, one escape-table to maintain (currently 2 copies of `write_aterm_string` diverged — `resolve.rs:566` vs `aterm.rs:12`). If [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) lands first, `to_aterm_resolved` variant simplifies (no `drop_input_drvs` param — always-empty). **ADDENDUM to T158 scope, not a new dedup.** discovered_from=consolidator-mc205.

### T175 — `refactor(scheduler):` cursor decode — version-check before length-check

rev-p271 trivial. MODIFY [`rio-scheduler/src/admin/builds.rs`](../../rio-scheduler/src/admin/builds.rs) at `:67-71` (p271 worktree). `decode_cursor` checks `buf.len() != CURSOR_V1_LEN` before `buf[0] != CURSOR_V1`. A future v2 cursor with different length gets "bad cursor length" not "bad cursor version" — misleading error, defeats version dispatch. Swap order: version-byte first (enables multi-version accept), then length-check (which becomes version-specific).

```rust
if buf.is_empty() || buf[0] != CURSOR_V1 {
    return Err(Status::invalid_argument("bad cursor version"));
}
if buf.len() != CURSOR_V1_LEN {
    return Err(Status::invalid_argument("bad cursor length"));
}
```

Note: the `is_empty()` guard makes the version-byte index safe without relying on the length check. **Post-P0271-merge.** discovered_from=271.

### T176 — `docs(proto):` admin_types cursor comment — strip encoding leak

rev-p271 trivial. MODIFY [`rio-proto/proto/admin_types.proto`](../../rio-proto/proto/admin_types.proto) at `:60` (p271 worktree — re-grep at dispatch; [P0376](plan-0376-proto-split-admin-types-out.md) moved `ListBuildsRequest` from types.proto). Comment leaks encoding ("base64(version_byte || submitted_at_micros_i64_be || build_id_uuid_16b)") while [`builds.rs:11-14`](../../rio-scheduler/src/admin/builds.rs) docstring says clients treat cursors as opaque. Proto comment invites client-side decode — defeats the "version byte lets us change encoding later without wire break" design note.

Replace with:

```protobuf
// Opaque keyset cursor — pass the prior page's next_cursor verbatim.
// When set, offset is ignored (cursor wins). Server encodes; clients
// treat as an opaque token.
```

**Post-P0271-merge.** discovered_from=271.

### T177 — `docs(nix):` default.nix :539-541 — rewrap "doesn't / buffer"

rev-p295 trivial. MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) at `:539-541` (p295 worktree — P0295-T65 edit). Comment mid-sentence wrap "proves the grpc_web filter\n#   doesn't\n#   buffer server-streams" reads awkward across `#` lines after T65's frame-prefix-grep tighten. Rewrap so "doesn't buffer" stays on one line:

```nix
#   ... The frame-prefix grep proves the grpc_web filter doesn't buffer
#   server-streams — load-bearing for WatchBuild / live log tail. Also ...
```

**Post-P0295-merge** (T65 produces the awkward wrap). discovered_from=295.

### T178 — `docs(infra):` networkpolicy.yaml :202 comment — template namespace

rev-p295 trivial. MODIFY [`infra/helm/rio-build/templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml) at `:202` (p295 worktree). P0295-T69-A templated `:216` (`kubernetes.io/metadata.name: {{ .Values.dashboard.envoyGatewayNamespace }}`) but left the `:202` CoreDNS-resolution comment hardcoded ("rio-dashboard-envoy.envoy-gateway-system.svc"). Comment describes default; drifts if operator overrides `envoyGatewayNamespace`.

Rewrite as:

```yaml
# Can't resolve rio-dashboard-envoy.{{ .Values.dashboard.envoyGatewayNamespace }}.svc
# without CoreDNS.
```

Helm templates comments too, so `{{ }}` in `#` lines renders at `helm template` time. **Post-P0295-T69-merge** (the `:216` templating + `values.yaml` key exist). discovered_from=295.

### T179 — `refactor(scheduler):` handle.rs spawn_with_leader — DeployConfig bundle newtype

rev-p307 trivial at [`rio-scheduler/src/actor/handle.rs:94-103`](../../rio-scheduler/src/actor/handle.rs). P0307 adds `poison_config` + `retry_policy` to `spawn_with_leader`, growing it 8→10 args. The pre-existing `#[allow(clippy::too_many_arguments)]` at `:93` now covers 10. Three of the args (`size_classes`, `poison_config`, `retry_policy`) are TOML-loaded-no-CLI deploy config — they match the grouping at [`main.rs:383`](../../rio-scheduler/src/main.rs) comment ("structural deploy config, not a knob").

Bundle them into a `DeployConfig` newtype (or reuse an existing `ActorConfig` if one exists — grep `struct.*Config.*{` in `actor/`):

```rust
#[derive(Debug, Clone, Default)]
pub struct DeployConfig {
    pub size_classes: Vec<crate::assignment::SizeClassConfig>,
    pub poison: crate::state::derivation::PoisonConfig,
    pub retry: crate::state::worker::RetryPolicy,
}
```

`spawn_with_leader(..., deploy: DeployConfig, ...)` drops to 8 args; the `#[allow]` can stay for headroom or be removed. **Next sub-config add (e.g., [P0304-T180](plan-0304-trivial-batch-p0222-harness.md) POISON_TTL) would've been 11 — the bundle absorbs it.** Update the single prod caller at `main.rs` and the `spawn()` delegator at `:68`. Cosmetic; no behavior change. discovered_from=307. Post-P0307-merge.

### T180 — `refactor(scheduler):` POISON_TTL — absorb into PoisonConfig

rev-p307 feature (routed trivial — low priority, natural follow-on) at [`rio-scheduler/src/state/derivation.rs:628-634`](../../rio-scheduler/src/state/derivation.rs). `POISON_TTL` is a `cfg(test)`-shadowed const (`24h` prod / `100ms` test — [`scheduler.md:108`](../../docs/src/components/scheduler.md) describes it as fixed). P0307 wired `PoisonConfig.threshold` + `require_distinct_workers` to TOML but left `POISON_TTL` as a const because the spec at `:108` describes it as fixed-24h.

IF an operator ever wants tune-without-rebuild, this is the gap. Absorb into `PoisonConfig.ttl: Duration` (default `24h`, serde-default). The cfg(test) 100ms override moves to a test-fixture `PoisonConfig { ttl: 100ms, ..Default::default() }` at the test callsites that need poison-expiry observation. **T179's DeployConfig bundle makes this a 1-field add, not an 11th arg.** discovered_from=307. Post-P0307-merge. Low priority — skip at dispatch if the TOML knob isn't operationally needed.

### T181 — `fix(dashboard):` Graph.svelte — truncated-check before allTerminal poll-stop

rev-p400 trivial at [`rio-dashboard/src/pages/Graph.svelte:177-182`](../../rio-dashboard/src/pages/Graph.svelte) (p400 worktree line-refs; re-grep `allTerminal` at dispatch). P0400-T3 adds a poll-stop when all visible nodes are terminal. The comment at `:177-179` claims "truncated first-5000-terminal implies tail-also-terminal because scheduler walks DAG forward" — but insertion-order truncation means EARLIER-inserted (roots) being terminal does NOT imply LATER-inserted (sinks) are too; roots settling first is normal DAG progress.

For >5000-node builds, poll-stop can fire while the tail is still running. Low impact (degraded-table already shown; truncation is communicated to the user). Fix: **either** guard `allTerminal` computation with `!r.truncated` (keep polling when truncated), **or** just soften the comment ("heuristic — truncated response means visible-terminal, not all-terminal; keeps polling anyway in degraded mode"). Prefer the guard — one-line conditional. discovered_from=400. Post-P0400-merge.

### T182 — `refactor(controller):` workerpool/tests — pub(super)→pub(crate) + builders_tests further split

rev-p396 trivial at [`rio-controller/src/reconcilers/workerpool/tests/mod.rs:33,40,60`](../../rio-controller/src/reconcilers/workerpool/tests/mod.rs) and [`tests/builders_tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs) (1013L post-P0396). Two sub-items:

**(a) Visibility:** `test_wp()` / `test_sts()` / `test_ctx()` are `pub(super)` — correct for the `tests/` submodules but too narrow if a sibling reconciler (e.g., `workerpoolset/tests/`) wants the same fixtures. `pub(crate)` is the conventional test-fixture visibility (matches [`rio-controller/src/fixtures.rs`](../../rio-controller/src/fixtures.rs) if P0380 landed that). 3-site `pub(super)` → `pub(crate)` at `:33,:40,:60`.

**(b) builders_tests 1013L:** P0396 split the 2200L monolith into 4 files; `builders_tests.rs` still holds 1013L (the StatefulSet/PDB spec rendering + quantity parsing tests). Candidate split seam: `builders_tests.rs` (StatefulSet spec + env propagation, ~500L) + `quantity_tests.rs` (GB/memory-parsing table tests, ~500L). Re-grep at dispatch for the section boundary (comment `// --- quantity parsing ---` or first `#[test]` with `parse_` prefix). Same pattern as P0386/P0395's per-domain split. discovered_from=396. Post-P0396-merge.

### T183 — `refactor(test-support):` metrics_grep — controller floor margin + cargo:warning threshold

rev-p394 trivial at [`rio-controller/tests/metrics_registered.rs:18`](../../rio-controller/tests/metrics_registered.rs) and [`rio-test-support/src/metrics_grep.rs:113`](../../rio-test-support/src/metrics_grep.rs) (p394 worktree refs; post-P0394-merge). Two sub-items:

**(a) Controller floor zero-margin:** P0394's floor-check is `spec_metrics.len() >= 5` but `grep -c 'rio_controller_' observability.md` → 6. Floor = actual-1 means ONE row-delete from the obs table silently passes. Other crates have wider margins (scheduler `>= 20` vs ~30 actual, worker `>= 10` vs ~14). Either (i) raise floor to `>= 6` (tight, intentional), or (ii) keep `>= 5` but add a comment "floor = current_count - 1; bump on next metric add". Prefer (i) — tight floor catches accidental row-delete, and adding metrics bumps the floor intentionally.

**(b) cargo:warning noise:** [`metrics_grep.rs:113`](../../rio-test-support/src/metrics_grep.rs) emits `cargo:warning=spec-metrics grep: {path} not found` on every build where `docs/src/observability.md` is absent (e.g., crane fuzz builds whose fileset excludes `docs/`). The warning is CORRECT-but-noisy — it fires on every fuzz build. Gate it on a `std::env::var("SPEC_METRICS_STRICT").is_ok()` check, OR demote to a `cargo:rerun-if-env-changed` info line (not visible by default). Prefer the env-gate: `if std::env::var("CI").is_ok() || strict { println!("cargo:warning=…") }` — silent in fuzz/local, loud in CI where it matters. discovered_from=394. Post-P0394-merge.

### T184 — `fix(infra):` nonpriv profile smarter-device override consistency

rev-p387 correctness (routed trivial — verify-at-dispatch) at [`infra/helm/rio-build/values/vmtest-full-nonpriv.yaml`](../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml) vs [`nix/tests/default.nix:335-350`](../../nix/tests/default.nix). The reviewer reported "Two nonpriv profiles DIFFER on smarter-device override. One is right — make consistent." P0387 unifies via `pulled.smarter-device-manager.destNameTag` derivation (T2+T3: moves the image override from YAML-hardcode to Nix `extraValues`).

**Verify at dispatch** whether the reported inconsistency survives P0387 or was a finding about p387's intermediate state. Candidates to check: (a) [`k3s-full.nix:548-549`](../../nix/tests/fixtures/k3s-full.nix) "vmtest-full-nonpriv.yaml exercises the production ADR-012 path" comment vs the post-P0387 YAML that no longer contains `image:`; (b) [`docker-pulled.nix:53-59`](../../nix/docker-pulled.nix) "the nonpriv values file overrides to the…" — stale after P0387-T3 removes the override from the YAML; (c) a second nonpriv registration in `default.nix` that wasn't migrated by P0387-T2 (grep `vmtest-full-nonpriv\|devicePlugin` in `nix/tests/`). If (c) exists and still has the YAML hardcode, that's the inconsistency. If only (a)/(b), it's comment-staleness → route to P0295 instead. discovered_from=387. Post-P0387-merge.

### T185 — `fix(nix):` envoy-gateway operator tag lockstep — nixhelm chart-bump drift window

rev-p387 trivial at [`nix/docker-pulled.nix:71-82`](../../nix/docker-pulled.nix) vs [`nix/helm-charts.nix:48`](../../nix/helm-charts.nix). `docker-pulled.nix` pins `envoy-gateway` operator at `finalImageTag = "v1.7.1"` + `imageDigest = "sha256:8bb273…"`. This tag MUST match what the `gateway-helm` chart (via `nixhelm` at `helm-charts.nix:48`) renders into the operator Deployment — [`envoy-gateway-render.nix:18-19`](../../nix/envoy-gateway-render.nix) notes "the chart's certgen Job and Deployment both pull `docker.io/envoyproxy/gateway:v1.7.1`". When nixhelm bumps `gateway-helm` to v1.8.x, the chart's image reference changes but `docker-pulled.nix`'s pin stays `v1.7.1` → airgap preload has the WRONG tag → `ImagePullBackOff` in the VM test.

Add a lockstep assert at [`flake.nix`](../../flake.nix) near T136's envoy-distroless tag-sync assert (grep `envoyValues.*envoyAirgap` — T136's existing pattern):

```nix
# Envoy Gateway operator: chart's appVersion must match the preloaded
# image tag. When nixhelm bumps gateway-helm, this fires at eval time.
envoyGatewayChartVer = (builtins.fromJSON
  (builtins.readFile "${subcharts.gateway-helm}/Chart.yaml")).appVersion
  or (throw "gateway-helm Chart.yaml has no appVersion");
assertMsg (envoyGatewayChartVer == pulled.envoy-gateway.finalImageTag)
  "envoy-gateway operator tag drift: chart appVersion=${envoyGatewayChartVer} but docker-pulled.nix finalImageTag=${pulled.envoy-gateway.finalImageTag}. Bump docker-pulled.nix:80 + imageDigest when nixhelm bumps gateway-helm.";
```

**Care — Chart.yaml is YAML not JSON:** `builtins.fromJSON` won't parse it. Options: (a) add a `runCommand` that `yq -o json Chart.yaml > chart.json` and read that (adds IFD — per [`deps-hygiene.md`](../../CLAUDE.md) "IFD categorically bad"); (b) grep `appVersion:` line with `builtins.match` on `readFile`; (c) add the version as a plain `envoyGatewayAppVersion = "v1.7.1";` let-binding next to the chart import with a comment "bump alongside nixhelm gateway-helm". Prefer (c) — one MORE duplication but the lockstep assert catches when either side drifts, and no IFD. discovered_from=387. Post-P0387-merge.

### T186 — `fix(nix):` helm-lint digest assert — extend to envoy-gateway operator image

rev-p387 trivial at [`nix/docker-pulled.nix:90`](../../nix/docker-pulled.nix). The comment says "values.yaml envoyImage (same digest). Bump BOTH — helm-lint assert" for the `envoy-distroless` data-plane image. The `envoy-gateway` OPERATOR image at `:71-82` has no such helm-lint assert — the `imageDigest` at `:79` can drift from whatever the chart references with no eval-time check.

Extend the helm-lint yq loop at [`flake.nix`](../../flake.nix) (grep `helm-lint.*yq\|yq.*digest` — [P0379](plan-0379-envoy-image-digest-pin.md) added it for `envoyImage`) to ALSO check the operator's `image:` field in the rendered chart:

```nix
# Extract the rendered operator Deployment's container image digest
# from 03-envoy-gateway.yaml; must match docker-pulled.nix.envoy-gateway.imageDigest
operatorImageRef=$(yq '.spec.template.spec.containers[0].image' \
  ${envoyGatewayRender}/03-envoy-gateway.yaml | head -1)
# Chart default is bare-tag (no @sha256:), so this checks TAG equality
# against finalImageTag. For digest-pin, would need the chart to emit
# @sha256: refs (it doesn't — the chart uses bare tags).
```

Actually the simpler check: the chart doesn't digest-pin (gateway-helm uses bare tags). So the `imageDigest` at `docker-pulled.nix:79` is ONLY used by `dockerTools.pullImage` to fetch the exact layer — it's never cross-checked against anything the chart renders. This is FINE (pullImage verifies the digest matches what docker.io serves for that tag). The "no-check" finding is about: when someone bumps `finalImageTag` to `v1.8.0` but forgets to bump `imageDigest`, pullImage fetches the NEW tag's layers but hashes them against the OLD digest → FOD hash mismatch → eval-time failure. That's already caught.

**Actual gap:** the envoy-distroless `helm-lint assert` at `:90` cross-checks `values.yaml`'s `envoyImage` digest vs `docker-pulled.nix`'s `imageDigest` — ensures the Helm chart's digest-pin and the airgap-preload's digest match. The operator has NO equivalent `values.yaml` digest-pin (gateway-helm doesn't expose one). So there's nothing to assert against. The finding likely meant: add a comment at `:79-82` explaining WHY there's no helm-lint assert for this image (no chart-side digest to compare against) — the asymmetry with the `:90` envoy-distroless comment is confusing without the note. Add ~3-line comment. discovered_from=387. Post-P0387-merge.

### T187 — `fix(scheduler):` P0408 recovery-resolve fetch-fail degrade counter

rev-p408 trivial at [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) post-P0408. [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md)-T1's store-fetch fallback at `:34` (plan-doc line ref; re-grep at dispatch) does `warn!("recovered CA dispatch: drv_content empty + store fetch failed; dispatching unresolved")` then early-returns with empty `drv_content`. No metric. The equivalent warn at `:751` (resolve-error swallow-to-worker-retry) ALSO has no metric — both are "CA resolve degraded to worker-fail-retry" paths.

Add `rio_scheduler_ca_resolve_degraded_total` counter with a `reason` label:

```rust
metrics::counter!("rio_scheduler_ca_resolve_degraded_total",
    "reason" => "store_fetch_failed").increment(1);
// … and at :751 …
metrics::counter!("rio_scheduler_ca_resolve_degraded_total",
    "reason" => "resolve_error").increment(1);
// … and at :790 collect_ca_inputs skip (ca_modular_hash unset) …
metrics::counter!("rio_scheduler_ca_resolve_degraded_total",
    "reason" => "modular_hash_unset").increment(1);
```

Three degrade paths, one counter, three label values. Operator sees `sum by (reason) (rate(…))` and knows WHICH CA-resolve-degrade is spiking. Also add a `describe_counter!` in [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) + a row in [`docs/src/observability.md`](../../docs/src/observability.md). The existing `r[obs.metric.scheduler]` marker covers it — no spec addition. discovered_from=408. Post-P0408-merge.

### T188 — `docs(dashboard):` Workers.test.ts stale ageMs comment — post-P0406 extraction

rev-p406 trivial at [`rio-dashboard/src/pages/__tests__/Workers.test.ts:23`](../../rio-dashboard/src/pages/__tests__/Workers.test.ts). Comment says "so ageMs is deterministic against the heartbeat fixture timestamps" — `ageMs` is the inline function at [`Workers.svelte:43`](../../rio-dashboard/src/pages/Workers.svelte). [P0406](plan-0406-dashboard-buildinfo-ts-extraction.md) extracts timestamp helpers to [`lib/buildInfo.ts`](../../rio-dashboard/src/lib/buildInfo.ts) and migrates `Workers.svelte` to use `fmtTsRel`/`tsToMs` instead. Post-P0406, the test comment references a function that no longer exists by that name.

Update `:23` to reference whichever helper `Workers.svelte` imports post-P0406 (`tsToMs` or `fmtTsRel` — check at dispatch what P0406's `Workers.svelte` migration uses for the age-column). If P0406 keeps `ageMs` as a thin wrapper in `Workers.svelte` that calls `tsToMs`, comment stays correct. If P0406 inlines the `tsToMs` call, comment should say "so tsToMs is deterministic". Verify-at-dispatch. discovered_from=406. Post-P0406-merge.

### T189 — `refactor(scheduler):` size_classes cutoff ensure → validate_config()

rev-p409 trivial. MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs). The `size_classes` cutoff_secs validation at `:347-358` fires inline in `main()` AFTER PG connect (`:277`) + migrations + S3 log-flusher spawn (`:330`). Move the `ensure!` into `validate_config()` at `:178` — join the existing 5 ensures. Matches the doc-comment at `:172` ("When P0307 or a later plan wires a new field … add its bounds check here").

In `validate_config()` after the `poison.threshold` ensure at `:215-221`, add:

```rust
// size_classes cutoff_secs: TOML supports `nan`/`inf` literals. A typo
// like `cutoff_secs = nan` crashes the sort at dispatch. Same is_finite
// + > 0.0 guard as before — just moved from :347 inline → here so it
// fires BEFORE PG connect/migrations/S3-flusher, and is unit-testable
// alongside config_rejects_*.
for class in &cfg.size_classes {
    anyhow::ensure!(
        class.cutoff_secs.is_finite() && class.cutoff_secs > 0.0,
        "size_classes[{}].cutoff_secs must be finite and positive, got {}",
        class.name,
        class.cutoff_secs
    );
}
```

At `:347-358`, DELETE the `ensure!` block (it's now in `validate_config`). **KEEP** the `:359-360` gauge-emit (`metrics::gauge!(...).set(class.cutoff_secs)`) and `:362-367` `info!` inline — `validate_config` should stay pure (no side effects). The `:347` `for class in &cfg.size_classes {` loop body shrinks to just the gauge-set line.

Optionally: add `config_rejects_nan_cutoff()` + `config_rejects_negative_cutoff()` rejection tests next to P0409's at `:893+` (test_valid_config() helper already handles the other required fields). discovered_from=409. Post-P0409-merge (DONE).

### T190 — `docs(notes):` bughunter-log.md — mc224 null-result entry

bughunt-mc224 null-result. Same cadence-entry pattern as T40/T135/T143/T147/T162/T163. Append to [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md):

```
## mc=224 (dd7f370d..3d2f7f20, 2026-03-20)

Null result above threshold. 7 merges reviewed (mc217-224: P0307/docs-988499/P0403/P0394/P0401/P0400/P0408).

Smell counts: unwrap/expect=9 (5× build.rs OUT_DIR idiomatic + 4× test code, ZERO in production src paths); silent-swallow=0; orphan-TODO=0; #[allow]=4 (2× dead_code metrics_grep.rs defensive + 2× result_large_err figment API-fixed 208B — all documented, test/support only); #[ignore]=1 (EXPLAIN test, dev-only documented); lock-cluster=0. Error-path: 2 new Result fns, both test-only.

Target-specific verdicts (all CLEAN):
- P0394 multi-line describe_counter — NON-ISSUE: detected via runtime recorder hook (with_local_recorder at rio-test-support/src/metrics.rs:240), not grep; emitted-grep handles multi-line via \s* (metrics_grep.rs:28 documents \s matches \n).
- P0403 NOT VALID/VALIDATE race — NON-ISSUE: premise wrong, migration 022 = CREATE INDEX CONCURRENTLY IF NOT EXISTS (single stmt, idempotent, INVALID-index recovery documented :21-23); no FK NOT VALID/VALIDATE path.
- P0408 1MiB .drv cap — NOT silent truncation: collect_nar_stream (client/mod.rs:202) → NarCollectError::SizeExceeded → Err→None→warn-level fallback; worker fetches with 4GiB MAX_NAR_SIZE (inputs.rs:235), scheduler-side optimization degrades gracefully.
- P0400 STATUS_CLASS vs P0406 progress — NON-ISSUE: statusClass ??gray fallback (graphLayout.ts:58, tested :120); P0406 progress() is build-level not per-derivation; P0410 closes cross-language gap.
- P0307 Jail vs P0409 validate_config — NOT overlap: Jail tests WIRING (roundtrip proves [poison].threshold=7 reaches cfg.poison.threshold); validate_config tests BOUNDS (config_rejects_*). Complementary.

P0307 f64-unvalidated gap already covered by rev-p409 followup (2026-03-20T16:05:49.867924) — NOT re-reported.
```

discovered_from=bughunter-mc224. No hard-dep (bughunter-log.md created by T40).

### T191 — `fix(harness):` dag_flip update-ref fallback — loud not silent

rev-p414 trivial. MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `:217-223`. The `tgt_sha != head_sha` belt-and-suspenders fallback runs `git update-ref` to force the integration-branch ref to post-amend HEAD — but prints nothing. If it fires, something about git's amend-under-worktree-layout is surprising (the `cur_branch != INTEGRATION_BRANCH` precondition at `:180` should make it unreachable). Silent recovery hides the signal.

Add stderr log + `DagFlipResult.ref_forced: bool` field:

```python
if tgt_sha != head_sha:
    # Belt-and-suspenders: force-update the ref. The cur_branch
    # precondition above should make this unreachable. If it fires,
    # something about git's amend semantics under this worktree
    # layout is surprising — the update-ref recovers regardless.
    print(
        f"dag_flip: UNEXPECTED — {INTEGRATION_BRANCH!r} at {tgt_sha[:8]} "
        f"but HEAD at {head_sha[:8]} post-amend. Force-updating ref. "
        f"The cur_branch precondition should prevent this; investigate "
        f"worktree layout.",
        file=sys.stderr,
    )
    git("update-ref", f"refs/heads/{INTEGRATION_BRANCH}", head_sha,
        cwd=REPO_ROOT)
    ref_forced = True
```

And add `ref_forced: bool = False` to `DagFlipResult` at [`models.py:488-509`](../../.claude/lib/onibus/models.py) so mergers can report it in `MergerReport`. Initialize `ref_forced = False` before the `if` at `:217`, pass to the final `DagFlipResult(...)` at `:230`. discovered_from=414. **Soft-conflict [P0417](plan-0417-dag-flip-already-done-double-bump.md):** both touch `merge.py:201-229` — T191 at `:217-223` (fallback block), P0417-T3 at `:201-206` (already-done branch); non-overlapping hunks. Both also touch `models.py` `DagFlipResult` — T191 adds `ref_forced` field, P0417-T5 edits `amend_sha` docstring; additive field-add + docstring-edit, rebase-clean.

### T192 — `refactor(scheduler):` find_cutoff_eligible DEAD — delete thin wrapper

rev-p405 trivial. MODIFY [`rio-scheduler/src/dag/mod.rs`](../../rio-scheduler/src/dag/mod.rs) at `:513-515`. `find_cutoff_eligible` is a thin wrapper (`find_cutoff_eligible_speculative(completed, &HashSet::new())`) with zero call-sites post-[P0405](plan-0405-bfs-walker-speculative-cascade-dedup.md): both `cascade_cutoff` and `verify_cutoff_candidates` now call `speculative_cascade_reachable` directly, which closes over `find_cutoff_eligible_speculative`. The non-speculative wrapper is dead.

Delete the fn (`:513-515`). Update 4 doc-comment mentions that reference it by name:
- [`completion.rs:77`](../../rio-scheduler/src/actor/completion.rs) — "walking find_cutoff_eligible with a provisional-skipped" → "walking find_cutoff_eligible_speculative with a provisional-skipped"
- [`completion.rs:363`](../../rio-scheduler/src/actor/completion.rs) — "→ P0252's find_cutoff_eligible() will skip downstream" → "→ P0252's cutoff-propagate cascade (`cascade_cutoff`) will skip downstream"
- [`dag/mod.rs:17`](../../rio-scheduler/src/dag/mod.rs) — "Each iteration is one `find_cutoff_eligible` call" → "Each iteration is one `find_cutoff_eligible_speculative` call"
- [`state/derivation.rs:242`](../../rio-scheduler/src/state/derivation.rs) — "consumed by `find_cutoff_eligible`" → "consumed by `cascade_cutoff` via `find_cutoff_eligible_speculative`"

The doc-comment at `:517` (`/// Variant of [`Self::find_cutoff_eligible`] that treats nodes`) becomes orphaned — rewrite as standalone description: `/// Returns nodes that become cutoff-eligible when `completed` finishes, treating nodes in `provisional_skipped` as-if-terminal. Used by `speculative_cascade_reachable` (batch-verification prewalk + cascade transition walk).` discovered_from=405. Post-P0405-merge. **Soft-conflict T169** (MAX_CASCADE rename at `:14-21`) — same file non-overlapping; **T169's SCOPE-+2 blockquote** (this batch's own inline edit) mentions `:646` which stays.

### T193 — `docs(notes):` kvm-pending.md — backfill T16 core-3 + T32/T37 + vm-ca-cutoff-standalone

rev-p311 doc-bug at [`.claude/notes/kvm-pending.md:36`](../../.claude/notes/kvm-pending.md). P0311's T16 exit criterion says "`≥3 entries vm-fod-proxy-k3s / vm-scheduling-disrupt-standalone / vm-netpol-k3s`" — but the current manifest has **only** the T53/T56+T70 entries (vm-lifecycle-wps-k3s + vm-observability-standalone). The T52-T70 dispatch batch created a **partial** manifest; the T16 core-3 + T32 (jwt-mount-present at [`default.nix:445`](../../nix/tests/default.nix)) + T37 (vm-security-nonpriv-k3s at [`default.nix:357`](../../nix/tests/default.nix)) never landed. **Plus** vm-ca-cutoff-standalone at [`default.nix:208`](../../nix/tests/default.nix) (added [`c1be4a36`](https://github.com/search?q=c1be4a36&type=commits) with `r[verify sched.ca.cutoff-propagate]` at [`default.nix:198`](../../nix/tests/default.nix)) — [P0405](plan-0405-bfs-walker-speculative-cascade-dedup.md) just refactored the cascade mechanism it tests, making it a high-priority observation target.

Append 6 entries (after the existing vm-observability-standalone entry at `:36`):

```markdown
- **vm-fod-proxy-k3s** — (P0311-T16 core). `no tracey marker — FOD proxy mitm e2e` at
  `fod-proxy.nix`. FOD builds through the CONNECT proxy, mitm-proof. Never
  KVM-built if P0308 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-fod-proxy-k3s`

- **vm-scheduling-disrupt-standalone** — (P0311-T16 core). `r[verify
  actual markers per nix/tests/default.nix:263. Worker drain → reassign +
  cordon. Never KVM-built if scheduling-fragment fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-scheduling-disrupt-standalone`

- **vm-netpol-k3s** — (P0311-T16 core). `no tracey marker — egress-deny NetPol e2e`
  at `netpol.nix`. k8s NetworkPolicy egress-deny holds for worker pods.
  Never KVM-built if P0241 fast-pathed (and per tooling-gotchas, it did —
  merger mode-4 drv-identical; new vm-netpol drv NEVER built).
  Run: `/nixbuild .#checks.x86_64-linux.vm-netpol-k3s`

- **vm-security-standalone** (jwt-mount-present subtest, P0311-T32) —
  scheduler+store have rio-jwt-pubkey ConfigMap mounted at expected path.
  Subtest at `default.nix:445`. Never KVM-built if P0357 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-security-standalone`
  → check `jwt-mount-present` subtest output.

- **vm-security-nonpriv-k3s** — (P0311-T37). Non-privileged worker container
  variant (devicePlugin + hostUsers:false). Entry at `default.nix:357`.
  Never KVM-built if P0360 fast-pathed.
  Run: `/nixbuild .#checks.x86_64-linux.vm-security-nonpriv-k3s`

- **vm-ca-cutoff-standalone** — `r[verify sched.ca.cutoff-propagate]` at
  `default.nix:198`. CA cascade end-to-end (compare → propagate → resolve).
  Added c1be4a36. [P0405](plan-0405-bfs-walker-speculative-cascade-dedup.md)
  refactored `speculative_cascade_reachable` — this test is the integration
  proof the refactor didn't change visible behavior. HIGH PRIORITY next
  KVM-slot.
  Run: `/nixbuild .#checks.x86_64-linux.vm-ca-cutoff-standalone`
```

**Supersedes T120** (consol-mc150's "+2 entries" — T120 was partial scope; this backfills the full set per T16's criterion plus the post-P0405 high-priority entry). If T120 already dispatched, dedupe at merge. discovered_from=311. Post-P0311-merge (DONE).

### T194 — `docs(scheduler):` completion.rs test doc-comments — fn-name cites not line-numbers

rev-p311 trivial at [`rio-scheduler/src/actor/tests/completion.rs:319`](../../rio-scheduler/src/actor/tests/completion.rs). Six doc-comments cite `completion.rs:422`, `:470`, `:476`, `:397`, `:399`, `:429` — these drift post-[P0405](plan-0405-bfs-walker-speculative-cascade-dedup.md)-rebase ([`b317ff7d`](https://github.com/search?q=b317ff7d&type=commits)+[`2e0806e3`](https://github.com/search?q=2e0806e3&type=commits) net +24/-20 above these lines). Contextual only — behavior unchanged, tests compile/pass.

Per citation-tiering principle: grep-stable fn-name > bare-line for contextual pointers in hot files (completion.rs count=31, 59 commits/30d-equivalent hot). Replace line-refs with the **thing being referenced**:

- `:422` → "the `tokio::time::timeout(grpc_timeout, …)` wrapper in `handle_completed`'s content_lookup call" (after [P0419](plan-0419-ca-compare-slow-store-test-timeout.md), it's `grpc_timeout` not `DEFAULT_GRPC_TIMEOUT`)
- `:470`, `:476` → "the `all_matched &= matched` accumulator in `handle_completed`"
- `:397`, `:399` → "the `!result.built_outputs.is_empty()` early-false at the top of the CA-compare block"
- `:429` → "the `all_matched &= matched` loop body" (same as :470 — these are the same loop, different commits referenced it at different lines)

Grep at dispatch for exact doc-comment locations (`grep ':[0-9][0-9][0-9]' rio-scheduler/src/actor/tests/completion.rs | grep -v 'r\[\|TODO'`). discovered_from=311. Post-P0311+P0405-merge (both DONE).

### T195 — `fix(nextest):` postgres group filter — match actor::tests:: paths

rev-p311 config-fix at [`.config/nextest.toml:73`](../../.config/nextest.toml). The filter `test(/^tests::/)` anchored-matches only **top-level** `tests::` modules; `actor::tests::completion::*` falls through to the DEFAULT group (30s×3 slow-timeout) instead of the `postgres` group (60s×2).

The new CA-compare tests at [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) **do** use `TestDb + MIGRATOR` (postgres-backed) but get misgrouped. Not a correctness bug — DEFAULT's 30s×3 = 90s ceiling is generous enough — but group semantics are mismatched (postgres group also serializes to `max-threads=8` which prevents thundering-initdb).

Widen the filter:

```toml
[[profile.default.overrides]]
filter = 'package(rio-scheduler) and test(/tests::/)'
test-group = 'postgres'
slow-timeout = { period = "60s", terminate-after = 2 }
```

`^tests::` → `tests::` (substring, not anchor). Matches both `tests::foo` and `actor::tests::foo`. Verify at dispatch with `cargo nextest list --package rio-scheduler 2>&1 | grep 'tests::' | grep -v '^tests::'` → should show the `actor::tests::` paths that will now match. discovered_from=311.

### T196 — `docs(workspace):` banner line-cites 1012-1100 → all_subconfigs_roundtrip_toml fn-name

rev-p412 doc-bug at [`rio-store/src/main.rs:722`](../../rio-store/src/main.rs), [`rio-gateway/src/main.rs:483`](../../rio-gateway/src/main.rs), [`rio-controller/src/main.rs:519`](../../rio-controller/src/main.rs), [`rio-worker/src/config.rs:378`](../../rio-worker/src/config.rs), [`rio-scheduler/src/main.rs:1021`](../../rio-scheduler/src/main.rs). [P0412](plan-0412-standing-guard-config-tests-four-crates.md)'s 4 banner comments cite `rio-scheduler/src/main.rs:1012-1100` — but scheduler main.rs is HOT (59 commits/30d; [P0412](plan-0412-standing-guard-config-tests-four-crates.md) plan-doc already observed 137-line drift `:875→:1012` between plan-write and dispatch). The scheduler's own banner at `:1021` also cites store `:640+` (46 commits/30d, same drift risk).

Same citation-tiering as T194: grep-stable name > bare-line for contextual pointers in hot files. Replace `rio-scheduler/src/main.rs:1012-1100` with `rio-scheduler/src/main.rs all_subconfigs_roundtrip_toml + all_subconfigs_default_when_absent` (the fn names P0412 introduces). The scheduler's own `:1021` cite of store becomes `rio-store/src/main.rs tests module (figment::Jail per-field roundtrips)`.

discovered_from=412. Post-P0412-merge. Four-crate sweep (store/gateway/controller/worker + scheduler's self-cite).

### T197 — `refactor(workspace):` standing-guard banner — hoist rationale to rio-common/src/config.rs

rev-p412 trivial at [`rio-store/src/main.rs:716-727`](../../rio-store/src/main.rs), [`rio-gateway/src/main.rs:476-485`](../../rio-gateway/src/main.rs), [`rio-controller/src/main.rs:512-521`](../../rio-controller/src/main.rs), [`rio-worker/src/config.rs:370-379`](../../rio-worker/src/config.rs). [P0412](plan-0412-standing-guard-config-tests-four-crates.md)'s 5th copy of the standing-guard banner (~10L each, 4 new + scheduler's original). Near-identical across all 5; test scaffolding (`#[test]` + `#[allow(clippy::result_large_err)]` + `figment::Jail::expect_with` wrapper) identical.

**Don't** macro-extract the scaffolding — that would obscure the `ADD IT HERE` edit-point which is the entire purpose. Instead: put the ~10L rationale paragraph **once** in [`rio-common/src/config.rs`](../../rio-common/src/config.rs) module-doc (after the existing `//! # How binaries wire this up` section at `~:7`), have each crate's banner be a **1-line pointer**:

```rust
// Standing-guard config tests — see rio-common/src/config.rs module-doc
// "Standing-guard tests" section for rationale + P0219 failure mode.
```

Saves ~40L net across 5 crates without hiding the edit-point. The test bodies (which vary per-crate Config fields) stay visible. Low-priority — copy-paste is intentional-for-visibility; this is a marginal cleanup. discovered_from=412. Post-P0412-merge.

### T198 — `docs(common):` config.rs — ADD-IT-HERE known-limitation paragraph

rev-p412 doc-note at [`rio-worker/src/config.rs:386-389`](../../rio-worker/src/config.rs) + [`rio-scheduler/src/main.rs:1032-1035`](../../rio-scheduler/src/main.rs). The standing-guard pair relies on human discipline: `ADD IT HERE when you add Config.newfield`. No enforcement — if someone adds a 6th sub-config table (e.g., `[circuit_breaker]`) without updating the crate test, the test passes (it only asserts what's present, not what's absent).

A build.rs that greps `struct Config` for non-primitive fields and cross-checks against the TOML literal would enforce this — but that's overkill for 5 crates and would itself drift. Instead: **document the limitation** in the T197 rationale-paragraph (rio-common/src/config.rs module-doc). Add a `## Known limitation` sub-section:

```rust
//! ## Known limitation: ADD-IT-HERE is advisory, not enforced
//!
//! The standing-guard pair asserts that KNOWN sub-config tables
//! roundtrip; it does NOT catch a NEW sub-config you forgot to add to
//! the test. Worker's test already omits scalar fields (fuse_cache_size_gb,
//! log_rate_limit, daemon_timeout_secs) setting the precedent.
//! A build.rs grep-vs-TOML-literal cross-check was considered and
//! rejected (overkill for 5 crates; would itself drift).
//!
//! When adding a `Config.newfield` of non-primitive type (sub-table),
//! you MUST update `all_subconfigs_roundtrip_toml` + `_default_when_absent`
//! in that crate's main.rs. The doc-comment is the enforcement.
```

Pairs with T197 (hoist to module-doc). If T197 skipped, add this paragraph to each crate's banner individually (5 copies). discovered_from=412. Post-P0412-merge.

### T199 — `fix(dashboard):` eslint gen/*_pb glob — widen for subdirectory depth

rev-p407 trivial at [`rio-dashboard/eslint.config.js:39`](../../rio-dashboard/eslint.config.js). The `no-restricted-imports` pattern `**/gen/*_pb` only matches DIRECT children of `gen/`. If `buf.gen.yaml` ever adds package-subdirectory output (e.g., `out: src/gen`, `paths: source_relative` → `src/gen/rio/admin_types_pb`), imports like `../gen/rio/admin_types_pb` would bypass the lint. Widen to `**/gen/**/*_pb` for depth-inside-gen coverage.

Currently `gen/` is flat ([`buf.gen.yaml:32`](../../rio-dashboard/buf.gen.yaml) has `out: src/gen`, no `paths:` opt) — this is speculative insurance. 1-char change (`*` → `**`). Update both patterns at `:39` (`**/gen/*_pb` and `**/gen/*_pb.js`). discovered_from=407.

### T200 — `docs(scheduler):` stale db.rs string cites — fn-name anchors post-P0411

rev-p411 trivial. Two stale string references to the pre-split `db.rs` monolith:

- [`actor/tests/build.rs:277`](../../rio-scheduler/src/actor/tests/build.rs): `"read_event_log in db.rs"` — now at `db/recovery.rs` (but re-exported via `db::read_event_log` at [`db/mod.rs:35`](../../rio-scheduler/src/db/mod.rs)). Change to `"read_event_log in db/recovery.rs (re-exported as db::read_event_log)"`.
- [`actor/completion.rs:995`](../../rio-scheduler/src/actor/completion.rs): `"db.rs:537 terminal filter"` — line was stale pre-split too (`TERMINAL_STATUS_SQL` was at `:22`, now [`db/mod.rs:56`](../../rio-scheduler/src/db/mod.rs)). Change to grep-stable `"TERMINAL_STATUS_SQL const"` (no line number). Matches T194's citation-tiering lesson.

Both are `cfg(test)` doc-comments — zero behavior change. discovered_from=411.

### T201 — `docs(components):` scheduler.md db.rs → db/ post-P0411

rev-p411 doc-bug at [`docs/src/components/scheduler.md:664`](../../docs/src/components/scheduler.md). Lists `"rio-scheduler/src/db.rs — PostgreSQL persistence"` in the crate-module list. Post-P0411 this path doesn't exist; should be:

```markdown
- `rio-scheduler/src/db/` — PostgreSQL persistence (derivations, assignments, build_history EMA; split into 9 domain modules per P0411)
```

Single-line sed fix. `remediations/phase4a/01-poison-orphan-recovery.md` has many `db.rs:NNN` cites too — those are historical archaeology, leave unless a future batch sweeps them. discovered_from=411.

### T202 — `refactor(scheduler):` connect_worker_no_ack — promote to helpers.rs, delegate

consol-mc235 trivial at [`rio-scheduler/src/actor/tests/worker.rs:819`](../../rio-scheduler/src/actor/tests/worker.rs). `connect_worker_no_ack` is a near-exact copy of `connect_worker` at [`helpers.rs:86`](../../rio-scheduler/src/actor/tests/helpers.rs) minus the trailing `PrefetchComplete` ACK — ~25L duplicated (`WorkerConnected` + `Heartbeat` construction with all 9 fields). P0311 added the `_no_ack` variant for warm-gate tests at [`:917`, `:963`](../../rio-scheduler/src/actor/tests/worker.rs).

Extraction: move `connect_worker_no_ack` to `helpers.rs` as `pub(crate)`, then have `connect_worker` call it + append the ACK send:

```rust
// helpers.rs
pub(crate) async fn connect_worker_no_ack(
    handle: &ActorHandle, worker_id: &str, system: &str, max_builds: u32,
) -> anyhow::Result<tokio::sync::mpsc::Receiver<Assignment>> {
    // ... existing body from worker.rs:819-845 ...
}

pub(crate) async fn connect_worker(
    handle: &ActorHandle, worker_id: &str, system: &str, max_builds: u32,
) -> anyhow::Result<tokio::sync::mpsc::Receiver<Assignment>> {
    let rx = connect_worker_no_ack(handle, worker_id, system, max_builds).await?;
    handle.send_unchecked(ActorCommand::PrefetchComplete {
        worker_id: worker_id.into(),
    })?;
    Ok(rx)
}
```

Net: -20L, single source of truth for the `Heartbeat` field list. Next time `ActorCommand::Heartbeat` grows a field (it has 9 today), one edit not two. discovered_from=consolidator.

### T203 — `refactor(harness):` merge.py raw subprocess.run → git()/git_try()

consol-mc235 + rev-p417 trivial at [`.claude/lib/onibus/merge.py:76,:114,:147`](../../.claude/lib/onibus/merge.py). Three raw `subprocess.run(["git", "rev-parse", ...])` calls with identical semantics (cwd=REPO_ROOT, capture_output, text, strip stdout) that could use the existing `git()` helper from [`onibus.git_ops`](../../.claude/lib/onibus/git_ops.py). The 4th at [`:551`](../../.claude/lib/onibus/merge.py) (`merge-base --is-ancestor`) is returncode-only — `git()` doesn't fit (it `check=True`s).

**Semantic divergence (rev-p417):** the current raw sites have NO `check=True` — they tolerate empty-on-fail (`stdout.strip()` → `""` → `if tip:` guard skips). `git()` has `check=True` (raise on non-zero). A naive fold would change behavior:

| Site | Current tolerate-empty | Post-`git()` behavior | Correct? |
|---|---|---|---|
| [`:76`](../../.claude/lib/onibus/merge.py) `lock` rev-parse | yes | raise | arguably CORRECT — rev-parse failing in REPO_ROOT = catastrophic, should raise not silent-skip |
| [`:114`](../../.claude/lib/onibus/merge.py) `lock_status` rev-parse | yes | raise | same — CORRECT |
| [`:147`](../../.claude/lib/onibus/merge.py) `count_bump` rev-parse | yes (`:152 if tip:`) | raise → `:152` guard unreachable | DEBATABLE — comment `:143-146` explicitly names `tip==""` as reachable (merger bash-cwd outside repo). But cwd=REPO_ROOT is explicit → rev-parse shouldn't fail unless REPO_ROOT itself is broken → raise is CORRECT. |

**Recommendation:** fold all three to `git()` (deliberate fail-loud conversion). Update `:143-146` comment to say "cwd=REPO_ROOT explicit, rev-parse can't silently fail — raises instead, cadence-gap impossible." If implementer wants conservative: add `git_try()` to git_ops.py that returns `None` on non-zero, use it at `:147` only.

**NOTE:** [P0417](plan-0417-dag-flip-already-done-double-bump.md) + [P0418](plan-0418-onibus-rename-canonicalization-hardening.md) + [P0420](plan-0420-count-bump-toctou-write-order.md) + [P0421](plan-0421-merge-py-rename-cluster-extract.md) all touch merge.py — fold this into whichever lands last, or land standalone after they drain. discovered_from=consolidator (mc235 + rev-p417 row 15 semantic-divergence note combined).

### T204 — `fix(harness):` merge.py scan break — last-match not first

rev-p417 trivial at [`.claude/lib/onibus/merge.py:224`](../../.claude/lib/onibus/merge.py) (becomes different line post-P0417; grep `break` in the already-done scan loop). The scan `break`s on FIRST-found `MergeSha` row with `plan == plan_num`. After a `--set-to` rewind + replay-through-same-plan, multiple rows with `plan=N` exist (chronological append-order); first = oldest, latest = correct.

Edge case (requires manual rewind discipline). Fix: iterate all, take last match:

```python
# BEFORE
for row in read_jsonl(sha_file, MergeSha):
    if row.plan == plan_num:
        prior_mc = row.mc
        break

# AFTER
for row in read_jsonl(sha_file, MergeSha):
    if row.plan == plan_num:
        prior_mc = row.mc
        # no break — last-row-per-plan wins (post-rewind may have
        # multiple plan=N rows; latest is correct per _cadence_range's
        # last-row-per-mc semantics at :262)
```

Or `reversed(list(read_jsonl(...)))` + break-on-first (same result, reads fewer rows on the common case). Post-P0417-merge. discovered_from=417.

### T205 — `fix(controller):` autoscaler _secs — ensure >0 on scale_up/scale_down/min_interval

rev-p416 trivial at [`rio-controller/src/main.rs:141`](../../rio-controller/src/main.rs). `validate_config` checks ONLY `autoscaler_poll_secs > 0`; `autoscaler_scale_up_window_secs` / `autoscaler_scale_down_window_secs` / `autoscaler_min_interval_secs` (at [`:53-58`](../../rio-controller/src/main.rs)) unvalidated.

Verified: these 3 feed `Duration::from_secs` ([`main.rs:375-377`](../../rio-controller/src/main.rs)) but NOT `tokio::interval` — 0 value is DEGRADED (thrash / no-cooldown at [`scaling/mod.rs:305-316`](../../rio-controller/src/scaling/mod.rs)) not PANIC. Below the docstring "specific crash" bar. Optional hardening: `ensure >0` on all 3 to prevent operator-foot-shooting.

```rust
// After the existing autoscaler_poll_secs ensure at :141
anyhow::ensure!(
    cfg.autoscaler_scale_up_window_secs > 0,
    "autoscaler_scale_up_window_secs must be > 0 (got {}); 0 → no cooldown → thrash",
    cfg.autoscaler_scale_up_window_secs
);
// ... same for scale_down_window_secs, min_interval_secs
```

Lower-severity than poll_secs (degraded not panic). rev-p415 flagged; seed(d) delegated here. discovered_from=416. Soft-conflict [P0425](plan-0425-ensure-required-helper-ten-site.md) (same validate_config body, different ensures — additive).

### T206 — `refactor(scheduler):` db/mod.rs row-struct domain-move (LOW-priority)

consol-mc236 trivial at [`rio-scheduler/src/db/mod.rs:101`](../../rio-scheduler/src/db/mod.rs). P0411 split cleanly (2859L → 317L mod.rs + 8 domain files). The 317L residual is ~170L of 9 `FromRow`/input structs + 2 SQL consts + 1 enum that each have ONE primary `db/` consumer. Map:

| Struct/const | → Move to | External callers |
|---|---|---|
| `TenantRow` | `tenants.rs` | 0 |
| `BuildListRow` + `LIST_BUILDS_SELECT` | `builds.rs` | `admin/*` (1) |
| `AssignmentStatus` | `assignments.rs` | 0 |
| `BuildHistoryRow` + `EMA_ALPHA` | `history.rs` | `estimator.rs` (1) |
| `DerivationRow` | `batch.rs` | 0 |
| `{RecoveryBuildRow, RecoveryDerivationRow, GraphNodeRow, GraphEdgeRow}` | `recovery.rs` | `state/derivation.rs` (1) |
| `PoisonedDerivationRow` | `derivations.rs` | 0 |

All have 1 `db/` consumer + 0-1 external (6 external callers total via `use crate::db::X`). Move structs next to their query fn, `pub use` from mod.rs — external callers unchanged. mod.rs drops to ~145L (`SchedulerDb` struct + `TERMINAL_STATUS_SQL` const + `encode_pg_text_array` + mod decls + re-exports).

**LOW-value:** ~170L move, zero behavior change, completes the P0411 fault-line story. Worth it if: a 10th row struct appears OR 2+ row structs get touched at once. Pre-split collision count was 43 (highest); post-split any residual mod.rs churn is these row structs. discovered_from=consolidator. Orthogonal to T207 (recovery.rs cohesion angle — different finding, same post-P0411 cleanup class).

### T208 — `docs(nix):` check-mutants-marker — version-pin comment anchor

rev-p402 trivial at [`flake.nix:1256`](../../flake.nix) (grep `check-mutants-marker` for exact line post-P0402). The pre-commit hook greps for `/* ~ changed by cargo-mutants ~ */` — no version-pin comment anchoring to cargo-mutants 26.2.0. If cargo-mutants ever changes `MUTATION_MARKER_COMMENT` format (unlikely but possible on major bump), hook silently passes on dirty mutant source.

Add comment adjacent to the grep pattern:

```nix
# marker verified for cargo-mutants 26.2.0 — MUTATION_MARKER_COMMENT
# const at src/mutate.rs (grep `MUTATION_MARKER_COMMENT` upstream).
# If a cargo-mutants bump changes the marker string, this hook
# SILENTLY passes — re-verify on major-version bumps. Same
# anchor-discipline as P0329's SHA-anchored ssh-store.cc citation.
```

Same citation-tiering principle: load-bearing→SHA/version+test, contextual→bare-line. discovered_from=402. Post-P0402-merge.

### T209 — `docs(notes):` bughunter-log mc235-mc238 — verdict archive entries

Archive-note batch (rows 10, 19, row 5 plan-0411-doc-accuracy, coordinator P0414-FIXED acknowledgment). MODIFY [`.claude/notes/bughunter-log.md`](../notes/bughunter-log.md). Add entries after mc224 (T190's entry):

```markdown
## mc=235-238 (<range>, 2026-03-20)

**consol-mc235 P0412 proc-macro verdict:** NOT WORTH. 5 figment::Jail
standing-guard test copies = FULL universe (all 5 binary crates). ~50L
shared-wrapper vs ~300L proc-macro crate for per-crate TOML-literal
derivation. Net negative. [tls] assertion block (~5L×4 = 20L identical)
also not worth a helper. Leave as-is; each pair cross-refs scheduler
all_subconfigs_roundtrip_toml for rationale (T196 makes this grep-stable).

**consol-mc236 5-target verdict:** T1 P0412+P0416 combine = answered above.
T2 merge.py split = [P0421](../work/plan-0421-merge-py-rename-cluster-extract.md).
T3 P0311 batch-sweep-shape = not duplication (14 test commits clean-merge,
workflow-correct). T4 T-collision = P0401 works at spec 9-digit; writer-side
11-digit format-divergence + 3 gaps covered by [P0418](../work/plan-0418-onibus-rename-canonicalization-hardening.md).
T5 db/mod.rs residual = T206 above (LOW-priority row-struct move).

**bughunt-mc238 finding:** SizeClassConfig.cpu_limit_cores Option<f64>
no validate_config check (NaN → bump-disabled, neg → always-bump).
Same P0415 failure class but missed by the wave. →
[P0424](../work/plan-0424-sizeclassconfig-cpu-limit-validation.md).

**coord P0414-FIXED archive-note:** merger-amend-branch-ref (step 7.5
detached-HEAD window) fixed by P0414 dag_flip compound. T191's
update-ref loud-fallback adds belt-and-suspenders. P0417 closes the
already-done double-bump; P0420 closes the count_bump write-order
TOCTOU. Three-plan chain (P0414→P0417→P0420) covers the full
merger state-machine failure surface.

**rev-p411 plan-doc-accuracy note (non-actionable):** plan-0411 Files
fence called for db/tests/recovery.rs (T9, not created — zero db-level
recovery tests existed; tests live at actor/tests/recovery.rs
integration-tier) and omitted db/tests/assignments.rs from tree-diagram
(impl correctly created it — 2 tests from pre-split :1856,:1880).
Plan doc over/under-specified. Only matters if plan doc becomes an
audit reference. Archived here, not fixed.
```

discovered_from=consolidator (rows 10,19) + bughunter-mc238 + coordinator + rev-p411-row-5.

### T207 — `docs(scheduler):` db/recovery.rs — cohesion NOTE for admin_reads candidates

rev-p411 trivial at [`rio-scheduler/src/db/recovery.rs:173`](../../rio-scheduler/src/db/recovery.rs). `load_build_graph` (dashboard viz query for `AdminService.GetBuildGraph`) and `read_event_log` (grpc bridge replay) live in `db/recovery.rs` but NEITHER is a recovery-on-LeaderAcquired query. They were physically inside the pre-split `:1123` section-banner range; P0411 faithfully preserved placement per its T8.

Add a `NOTE(fault-line)` comment at the top of these two fns (same pattern as T90's `put_path_batch.rs` NOTE):

```rust
// NOTE(fault-line): load_build_graph + read_event_log live here because
// they were physically inside the pre-split :1123 "recovery" banner.
// Neither is actually a recovery-on-LeaderAcquired query — load_build_graph
// serves AdminService.GetBuildGraph (dashboard viz), read_event_log serves
// grpc bridge replay. If a future plan touches both, consider extracting
// to db/admin_reads.rs. r[impl dash.graph.degrade-threshold] at the
// load_build_graph 5000-node cap IS correctly placed (scheduler implements
// server-side cap regardless of which db/ file hosts it).
```

LOW-priority rename/extract deferred to future plan; this task just documents the known-misplacement. discovered_from=411. Orthogonal to T206 (row-struct placement — different cohesion angle).

### T210 — `feat(harness):` preemptive-cadence at N-5 — fire cadence agents BEFORE mod-5/7 boundary

coord harness feature (rix §3). Cadence agents (consolidator %5, bughunter %7) currently fire AT the boundary — by then the window's merges are already landed and the coordinator has moved on. Preemptive fire at N-5 (e.g., at mc=65 fire consolidator for window mc=66-70 speculatively) lets findings reach the coordinator BEFORE those plans merge, so a "these 5 plans will duplicate X" finding can redirect the in-flight impls.

MODIFY [`.claude/lib/onibus/merge.py`](../../.claude/lib/onibus/merge.py) at `_cadence_range` / the `CadenceReport` builder. Add a `preemptive: bool` flag to `CadenceWindow` and compute a second window: `if (mc + 5) % cadence == 0: due_preemptive = True` with range `(current_sha, None)` — bughunter/consolidator read `git log <start>..HEAD` at scan time, so an open-ended range self-updates as merges land during the cadence agent's run. Coordinator's `/dag-run` step 6 spawns with `--preemptive` flag; agent output tagged `origin: "consolidator-preemptive"` so it's distinguishable from the retrospective scan. discovered_from=coord. No hard-dep.

### T211 — `fix(harness):` lock-FP-discriminator — `ff_landed` check uses plan's branch-tip, not just main_at_acquire

coord harness fix (rix §6). `lock_status()` at [`.claude/lib/onibus/merge.py:103-120`](../../.claude/lib/onibus/merge.py) sets `ff_landed` by comparing `INTEGRATION_BRANCH` tip vs `main_at_acquire`. False-positive when ANOTHER merger's ff moved the tip while THIS merger was still pre-ff (lock is exclusive so this shouldn't happen — but a coord fast-path commit, or a manual `git commit` to `$TGT`, moves the tip without an ff). The discriminator should check whether THIS plan's branch tip is an ancestor of `$TGT` (the ff DID land the plan's commits) vs just "$TGT moved" (something else landed).

Change `:112-119` to:

```python
if stale:
    # ff_landed = did THIS lock's plan-branch land? The prior check
    # (tip != main_at_acquire) is a false-positive when some OTHER
    # commit moved $TGT. Ancestor-check the held-plan's branch tip.
    plan_branch = content.get("plan", "").lstrip("p")
    if plan_branch:
        rc = subprocess.run(
            ["git", "merge-base", "--is-ancestor",
             f"p{plan_branch}", INTEGRATION_BRANCH],
            cwd=REPO_ROOT,
        ).returncode
        ff_landed = (rc == 0)
    else:
        ff_landed = None  # can't discriminate — chain-merger lock has plan="chain-<id>"
```

discovered_from=coord. Soft-dep [P427](plan-427-chain-merger-fifo-loop.md) (chain-merger's `lock_renew` writes the CURRENT plan to the lock — this discriminator reads it).

### T212 — `feat(dashboard):` logStream droppedLines count — show how many lines truncated

rev-p392 feature at [`rio-dashboard/src/lib/logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts). P0392's `truncated: boolean` flag is a one-bit signal. Replace (or supplement) with `droppedLines: number` — each splice bumps it by `excess`. Viewer renders "— 12,340 earlier lines truncated —" instead of the generic banner. Useful when a user scrolls up expecting the full log and wonders HOW MUCH is missing.

```ts
let droppedLines = $state(0);
// ... in the cap block ...
droppedLines += excess;
```

Expose via getter; LogViewer renders `{stream.droppedLines.toLocaleString()}` in the truncated banner. Keep `truncated` as a convenience bool (`droppedLines > 0`) for existing callers. discovered_from=392. Post-P0392-merge. Soft-conflict [P426](plan-426-logviewer-line-h-cap-spread-limits.md)-T3 (same cap block — both additive, T3 computes `excess`, T212 consumes it).

### T213 — `feat(dashboard):` LogViewer title={line} — hover to see full line under ellipsis

rev-p392 trivial at [`rio-dashboard/src/components/LogViewer.svelte:139`](../../rio-dashboard/src/components/LogViewer.svelte). P0392-T2's `.line { white-space: pre; overflow: hidden; text-overflow: ellipsis }` clips long lines. Adding `title={line}` to the `<pre>` shows the full text on hover — zero-cost recovery of the pre-virtualization `pre-wrap` content without the variable-height problem.

```svelte
<pre class="line" title={line}>{line}</pre>
```

Browser-native tooltip; no new dep. For very long lines (>~1000 chars) the tooltip itself truncates — acceptable. discovered_from=392. Post-P0392-merge.

### T214 — `refactor(dashboard):` viewportOverride — dev-only prop via `$props()` destructure default

rev-p392 trivial at [`rio-dashboard/src/components/LogViewer.svelte:26-31`](../../rio-dashboard/src/components/LogViewer.svelte). `viewportOverride` is test-only (comment at `:23-25` says so) but typed as `{ start: number; end: number } | undefined` in the public props shape. A production caller sees it in autocomplete. Svelte 5 doesn't have a first-class "test prop" escape hatch; the simplest guard is a type-level `@internal` JSDoc annotation OR renaming to `_viewportOverride` with the underscore convention:

```ts
/** @internal test-only — jsdom layout is all-zeros; tests stub the viewport directly. */
_viewportOverride = undefined,
```

Low priority — it's a type-hygiene nit, not a correctness issue. discovered_from=392. Post-P0392-merge.

### T215 — `fix(dashboard):` LogViewer {#each} key — key on content hash or stable index, not viewport.start+i

rev-p392 trivial at [`rio-dashboard/src/components/LogViewer.svelte:138`](../../rio-dashboard/src/components/LogViewer.svelte). The `{#each}` is keyed on `(viewport.start + i)` — stable for a FIXED-position window, but when the cap splices the head (10K lines dropped) and the user is scrolled to an absolute position, `viewport.start` stays fixed while the underlying `stream.lines` shifts — every keyed node's content changes but its key doesn't, so Svelte reuses the DOM nodes with new content instead of animating the shift. Mostly invisible (no `animate:` directive in use) but would break if a future PR adds transitions.

**Option A (simplest):** key on `line` itself — `{#each ... as line (line)}`. Duplicate lines (common in build logs: "compiling...") collide. **Option B:** key on `(stream.firstLineNumber ?? 0n) + BigInt(viewport.start + i)` — use the proto's `firstLineNumber` field (bigint, monotonic) as the stable offset. Requires plumbing `firstLineNumber` through the stream type. **Option C (defer):** leave as-is, add a comment "keys are viewport-relative, NOT log-absolute — don't add animate: without rekeying" and defer to the plan that adds transitions.

Prefer **Option C** — it's a latent footgun, not a live bug. 3-line comment at `:138`. discovered_from=392. Post-P0392-merge.

### T216 — `refactor(scheduler):` MAX_PREFETCH_PATHS — hoist to one module, delete dup

rev-p391 trivial at [`rio-scheduler/src/actor/worker.rs:12`](../../rio-scheduler/src/actor/worker.rs) + [`rio-scheduler/src/actor/dispatch.rs:597`](../../rio-scheduler/src/actor/dispatch.rs). P0391 introduced `const MAX_PREFETCH_PATHS: usize = 100` in `worker.rs`; `dispatch.rs` already had its own copy at `:597` (different prefetch code path — per-dispatch hint vs initial-warm hint). Both say `100`. A future tune-one-forget-other is the risk.

Hoist to a shared spot (e.g., `actor/mod.rs` or a `prefetch.rs` consts module) and `pub(crate) use` from both. Or: keep both but cross-comment them ("sibling at dispatch.rs:597 — bump BOTH"). Prefer hoist — 2-line extraction + 2-line import. discovered_from=391. Post-P0391-merge.

### T217 — `refactor(scheduler):` GaugeValues recorder — promote to rio-test-support alongside CountingRecorder

rev-p366 trivial at [`rio-scheduler/src/rebalancer/tests.rs:294`](../../rio-scheduler/src/rebalancer/tests.rs) (p366 worktree ref). P0366 introduces `GaugeValues` — a `metrics::Recorder` impl that captures `gauge.set()` values for test assertions (`f64::to_bits`/`from_bits` roundtrip through an `AtomicU64`-backed `DashMap`). Same pattern as `CountingRecorder` at [`rio-test-support/src/metrics.rs:139`](../../rio-test-support/src/metrics.rs) (which captures counter increments). If another test wants gauge-value assertions, it'll reimplement. Hoist to rio-test-support next to `CountingRecorder`:

```rust
// rio-test-support/src/metrics.rs — add after CountingRecorder
/// Captures `gauge!().set()` values for test assertions. Partner to
/// CountingRecorder (counters). f64 values roundtrip via
/// AtomicU64::to_bits/from_bits — no precision loss for exact sets.
#[derive(Default)]
pub struct GaugeValues { ... }
```

Update P0366's `rebalancer/tests.rs` to `use rio_test_support::metrics::GaugeValues`. Check at dispatch whether [P0311-T36](plan-0311-test-gap-batch-cli-recovery-dash.md) (rebalancer spawn_task test) independently wants gauge readback — if so, it benefits immediately. discovered_from=366. Post-P0366-merge. Soft-conflict P0423 (MockStore FaultMode refactor — also touches rio-test-support, different file `grpc.rs` vs `metrics.rs`, non-overlapping).

### T218 — `test(vm):` security.nix+fod-proxy.nix — restore debug-context in assertion msgs

rev-p314 finding at [`nix/tests/scenarios/security.nix:1232`](../../nix/tests/scenarios/security.nix). P0314 cleanup replaced `out[-500:]` (full build tail) with `out_path!r` (only stripped last line) in the assertion message. On `startswith`-failure, CI log no longer shows what `nix-build` actually printed. Same pattern at [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) `:216` + `:352` — old had `print(...output...)` before strip, both dropped. Restore the debug-tail: `f"...got {out_path!r}; tail: {out[-500:]}"` or re-add the `print(out)` before the assert. discovered_from=314.

### T219 — `refactor(vm):` lifecycle.nix — use attr= kwarg instead of shell-injected -A

rev-p314 finding at [`nix/tests/scenarios/lifecycle.nix:1804,1806`](../../nix/tests/scenarios/lifecycle.nix). Calls pass `'-A dep'` / `'-A consumer'` inside the `drv_file` positional (shell-injected). Pre-existing, but `mkBuildHelperV2` now has a proper `attr=` kwarg. Convert to: `build('${refsDrvFile}', attr='dep', capture_stderr=False)`. Works as-is via shell expansion; cleanup prevents future attr-inject surprises and matches the helper contract. discovered_from=314.

### T220 — `refactor(vm):` ca-cutoff.nix build_ca_chain — absorb into mkBuildHelperV2

rev-p314 finding at [`nix/tests/scenarios/ca-cutoff.nix:51-66`](../../nix/tests/scenarios/ca-cutoff.nix). `build_ca_chain` is the 6th divergent `build()` helper — exact try/except/dump/raise skeleton P0314 consolidated in 5 other scenario files. Missed because `ca-cutoff.nix` wasn't in P0314's 5-file table. The plan tagline said "a 6th scenario file would grow a 6th copy" — ca-cutoff IS that 6th, it pre-existed. Absorb via `mkBuildHelperV2` + `extra_args=f'--impure --argstr marker {marker!r}'`. ~15L net removed. discovered_from=314.

### T221 — `docs(scheduler):` assignment.rs+main.rs — `:128` cite → `:131` (self-shifted)

rev-p424 finding at [`rio-scheduler/src/assignment.rs:68`](../../rio-scheduler/src/assignment.rs) + [`main.rs:263`](../../rio-scheduler/src/main.rs). Both comments cite `c > limit` at `:128`; actual location is `:131` (self-inflicted — P0424-T3 added 3 doc-comment lines, shifting the ref). Plan-doc [plan-0424](plan-0424-sizeclassconfig-cpu-limit-validation.md) also says `:128`. Update both code comments to `:131` (or re-grep at dispatch — may have shifted again). discovered_from=424.

### T222 — `refactor(scheduler):` completion.rs — drop stale grpc_timeout(3s), use setup_ca_fixture

coord finding from chain-merger-2. [P0393](plan-0393-ca-compare-short-circuit-on-miss.md) left stale `grpc_timeout(3s)` at [`completion.rs:363-366`](../../rio-scheduler/src/actor/tests/completion.rs) — now harmless-redundant (`CONTENT_LOOKUP_TIMEOUT=2s` module const wins). Fix: `setup_ca_fixture("ca-slow")` (from [P0422](plan-0422-ca-compare-test-fixture-extract.md)) + drop the stale comment. discovered_from=393. Soft-dep P0422 (fixture helper must exist).

### T223 — `fix(crds):` SeccompProfileKind — Rule::new().message() not bare-string validation

rev-p304 finding at [`rio-crds/src/workerpool.rs:436-437`](../../rio-crds/src/workerpool.rs). `SeccompProfileKind` uses bare-string `validation=` instead of `Rule::new().message()` — missed in the sweep that converted all other `#[x_kube]` validations. Operators get opaque CEL-expr error instead of actionable message. Same mechanism as the rest, not intentionally different. Apply the `Rule::new(<expr>).message("<actionable text>")` pattern. discovered_from=304.

### T224 — `fix(vm):` k3s-full.nix:489 — untagged TODO timeout=600

rev-p304 finding at [`nix/tests/fixtures/k3s-full.nix:489`](../../nix/tests/fixtures/k3s-full.nix). Untagged `TODO: timeout=600` remains after plan cleaned up the sibling TODO above it (same containerd-tmpfs rationale). Either reduce to 300 matching the sibling fix, or tag with a plan number per CLAUDE.md untagged-TODO audit. discovered_from=304.

### T225 — `refactor(store):` gc/mod.rs — drop dead ON CONFLICT DO NOTHING on pending_s3_deletes

sprint-1 cleanup finding at [`rio-store/src/gc/mod.rs:573`](../../rio-store/src/gc/mod.rs). The `INSERT INTO pending_s3_deletes ... ON CONFLICT DO NOTHING` clause is dead code: `pending_s3_deletes` has no unique constraint on `s3_key` or `blake3_hash` (only `id BIGSERIAL PK`, per [`migrations/005_gc.sql`](../../migrations/005_gc.sql)), so ON CONFLICT never fires. The adjacent test-comment at `:962` already documents this — the old `idempotent_enqueue_on_conflict` test was replaced by `double_decrement_rejected_by_check` precisely because the ON CONFLICT was unreachable.

**Route-(a) drop clause:** Remove `ON CONFLICT DO NOTHING` from the INSERT. Duplicate enqueues are ALREADY prevented by the `deleted = false` filter in RETURNING + the `refcount = 0` precondition — the ON CONFLICT was belt-and-suspenders that never engaged. Update the `:962` comment to past-tense ("was dead code, removed").

**Route-(b) add constraint:** Add `UNIQUE(blake3_hash)` to `pending_s3_deletes` via a NEW migration, making the ON CONFLICT live. Only if there's a real double-enqueue scenario the current filters miss — check at dispatch whether the `:559` comment's TOCTOU-with-drain case can actually produce a duplicate enqueue.

Prefer route-(a): the `:962` analysis already concluded the scenario "can't happen in practice" (orphan scanner vs GC sweep are mutually exclusive on manifest status). discovered_from=sprint-1-cleanup.

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
- ~~`grep 'def submit_build_grpc\|submit_build_grpc(' nix/tests/` → ≥2 hits (T9: helper defined + called)~~ — OBE via P0362
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
- `grep 'nix-hash\|grep -B0' rio-gateway/tests/golden/corpus/README.md` → 0 (T21: broken pipeline deleted)
- `grep 'nix eval --raw' rio-gateway/tests/golden/corpus/README.md` → ≥1 (T21: working form remains)
- `grep 'drv_name\|mitigations' .claude/known-flakes.jsonl | head -1 | grep '^#'` → match (T22: header line 2 mentions both new fields)
- `grep 'Match key is .test. (exact)' .claude/known-flakes.jsonl` → 0 (T22: stale match-semantics line replaced)
- `grep 'def read_header' .claude/lib/onibus/jsonl.py` → 1 hit (T23: helper defined)
- `grep -c 'read_header' .claude/lib/onibus/cli.py .claude/lib/onibus/jsonl.py` ≥ 3 (T23: def + two call-sites)
- `grep 'P992247601' .claude/dag.jsonl` → 0 (T24: stale placeholder ref fixed)
- `grep -c 'P0317' .claude/dag.jsonl` ≥ current+2 (T24: both rows now reference the real plan)
- `grep 'Failed { status: TimedOut }' docs/src/components/scheduler.md` → 0 (T25: spec drift fixed)
- `grep 'error_summary' docs/src/components/scheduler.md` → ≥1 hit in the `:449-451` region (T25: corrected text present)
- `grep 'rio_scheduler_build_timeouts_total' docs/src/observability.md` → ≥1 hit (T26: table row added)
- `grep 'per-build timeout' docs/src/observability.md` → ≥1 hit in cancel_signals_total description (T26: 4th trigger added)
- `nix develop -c tracey query rule sched.ca.detect` → non-empty rule text (T27: blank-line fix makes tracey see the paragraph)
- Same check for `sched.ca.cutoff-compare` and `sched.ca.cutoff-propagate` (T27)
- `grep 'NO leading zeros\|leading zeros are a JSON' .claude/agents/rio-planner.md` → ≥1 hit (T28: guidance added)
- `grep 'fileset.difference' flake.nix` → ≥1 hit in tracey-validate block (T29)
- `nix eval .#checks.x86_64-linux.tracey-validate.drvPath` before/after a `.claude/`-only edit → identical drv path (T29: hash-identity now holds)
- `grep 'by construction' .claude/lib/onibus/merge.py` → 0 hits in the `_rewrite_and_rename` docstring (T30: false claim removed)
- `grep 'batch-append targets\|glob.*plan-' .claude/lib/onibus/merge.py` → ≥1 hit in `_rewrite_and_rename` (T30: second-pass grep loop present)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'batch_append_targets'` → 1 passed (T30: regression test for docs-933421 class)
- `grep -c 'rio-scheduler/tests\|rio-store/tests\|rio-worker/tests\|rio-controller/tests' .config/tracey/config.styx` → 4 (T31: all four crate test dirs in test_include)
- `nix develop -c tracey query rule store.put.idempotent` shows ≥1 `verify` site at `rio-store/tests/grpc/core.rs:252` (T31: previously-invisible annotation now scanned — kill tracey daemon first)
- `nix develop -c tracey query rule sec.boundary.grpc-hmac` shows ≥1 `verify` site (T31: `rio-store/tests/grpc/hmac.rs:38` — pre-P0328 annotation, proves the fix covers more than just metrics_registered)
- `nix develop -c tracey query validate` → `0 total error(s)` (T31: no typo'd marker IDs in the 13 newly-scanned annotations)
- `grep 'TODO(P0335)' rio-gateway/src/session.rs` → 1 hit (T32: tag added)
- `grep -E 'TODO[^(]' rio-gateway/src/session.rs` → 0 hits (T32: no untagged TODOs remain in that file — the `:32` one was the only one)
- `sed -n '176p' rio-gateway/src/handler/build.rs | grep -c TODO` → 0 (T33: word dropped)
- `grep -c 'emit_progress' rio-scheduler/src/actor/worker.rs rio-scheduler/src/actor/completion.rs` → ≥2 total (T34: new call sites; completion.rs already had 1 at :528, now ≥2; worker.rs was 0, now ≥1)
- `grep -B2 'return false' rio-scheduler/src/actor/dispatch.rs | grep emit_progress` → ≥1 hit (T34: assignment-send-fail rollback emits before returning)
- `cargo nextest run -p rio-scheduler actor::tests::worker` — disconnect test asserts BuildProgress event or `build_summary().running == 0` post-disconnect (T34)
- `grep 'background-task notification is NOT a merger-done' .claude/skills/dag-run/SKILL.md` → 1 hit (T35: lock-stomp lesson added)
- `grep 'STDERR_ERROR' docs/src/components/gateway.md | grep -c 'validate_dag'` → 0 (T36: validate_dag no longer described as STDERR_ERROR; the inline-check mention at :466 stays for path 2)
- `grep 'BuildResult::failure.*STDERR_LAST' docs/src/components/gateway.md` → ≥1 hit in the :462 region (T36: corrected text present)
- `grep 'expect("non-empty: checked at :197")' rio-store/src/grpc/put_path_batch.rs` → 1 hit (T37: invariant-cite parity with :308)
- `grep -c 'unwrap()' rio-store/src/grpc/put_path_batch.rs` → 0 (T37: no bare unwraps in prod path)
- `grep -c 'rio_store_put_path_total.*exists\|rio_store_put_path_duration' rio-store/src/grpc/put_path_batch.rs` → ≥2 (T38: exists counter + duration histogram added)
- `grep 'git merge-base --is-ancestor\|grep .clause-4' .claude/skills/dag-run/SKILL.md` → ≥2 hits (T39: 4-check discipline added)
- `grep 'mc=77.*null result' .claude/notes/bughunter-log.md` → 1 hit (T40: mc77 cadence null recorded)
- `grep 'mc=84' .claude/notes/bughunter-log.md` → 1 hit (T40: mc84 cadence entry recorded)
- `grep 'migration 017' migrations/018_chunk_tenants.sql rio-store/src/grpc/chunk.rs rio-store/src/metadata/chunked.rs rio-store/tests/grpc/chunk_service.rs` → 0 hits (T41: OBE — already fixed by 76ba3999; skip if 0)
- `grep 'find_missing_chunks\b' rio-store/src/backend/chunk.rs` → 0 OR grep rewritten to reference RETURNING (T42: post-P0264-merge)
- `grep "80 00 00 00\|grep -q '\^80'" .claude/work/plan-0273-*.md` → ≥1 hit (T43: tightened grep pattern)
- `python3 -c "import json; l=[ln for ln in open('.claude/dag.jsonl') if '273' in ln and ': ' in ln]; assert not l, l"` → no AssertionError (T44: compact spacing)
- `grep 'merge count-bump' .claude/notes/ci-gate-fastpath-precedent.md` → ≥1 hit (T45: step added to mechanical recipe)
- `grep 'migration:\|_MIGRATION_NUM\|migration_collision' .claude/lib/onibus/collisions.py` → ≥2 hits (T46: special-case present)
- `grep 'ema_proactive_updates_total' rio-scheduler/tests/metrics_registered.rs` → ≥1 hit (T47: in SCHEDULER_METRICS const; post-P0266-merge)
- `grep 'which branch fired\|branch label' rio-store/src/signing.rs` → ≥1 hit (T48: comment corrected)
- `grep 'key=tenant-foo-1' rio-store/src/signing.rs` → 0 hits (T48: overstated capability removed)
- `grep 'cluster().sign\|\.cluster()\.' rio-store/src/grpc/mod.rs | grep -v 'fn cluster'` → ≥1 hit in maybe_sign fallback (T49: post-P0338-merge)
- `grep 'expect("infallible")' rio-store/src/grpc/mod.rs` → 0 hits (T49)
- `grep 'tenant_key_lookup_failed_total' rio-store/src/grpc/mod.rs docs/src/observability.md` → ≥2 hits (T50: counter in code + spec table; post-P0338-merge)
- `grep 'attempted\|may be lower' rio-store/src/gc/mod.rs | grep -i 'enqueue\|keys'` → ≥1 hit (T51: corrected comment; post-P0339-merge)
- `grep -B1 'pub async fn sweep_orphan_chunks' rio-store/src/gc/sweep.rs | grep instrument` → match (T52)
- `grep 'TODO(P0260)' nix/tests/scenarios/security.nix` → 0 hits (T53: re-tagged or deleted)
- `grep 'pub use rio_nix::store_path::nixbase32' rio-test-support/src/fixtures.rs` → 1 hit (T54)
- `grep 'b"0123456789abcdfghijklmnpqrsvwxyz"' rio-test-support/src/fixtures.rs` → 0 hits (T54: literal removed)
- `grep 'owns.*Job\|Api::<Job>' rio-controller/src/main.rs` → ≥1 hit (T55; post-P0296-merge)
- lifecycle.nix ephemeral subtest: precondition `jobs_before >= 1` appears textually BEFORE the `build(` call and BEFORE `jobs_after >= jobs_before` (T56; grep line-order post-P0296-merge)
- `grep 'panic!.*unrecognized\|allowed: nix-pinned' rio-gateway/tests/golden/daemon.rs` → ≥1 hit (T57; post-P0300-merge)
- `cargo nextest run -p rio-gateway variant_skip_names_are_real` → pass (T58; post-P0300-merge)
- `grep 'synth_db: &Path\|upper_synth_db()' rio-worker/src/overlay.rs rio-worker/src/executor/mod.rs` → ≥2 hits (T59: signature + caller; post-P0333-merge)
- `grep 'nix/var/nix/db' rio-worker/src/overlay.rs | grep -v 'upper_synth_db\|fn upper_synth'` → ≤1 hit (T59: only the :126 accessor def remains)
- `grep 'seed_store_output\|fn seed_store_output' rio-test-support/src/fixtures.rs` → ≥1 hit (T60)
- `grep -c 'seed_store_output' rio-worker/src/executor/inputs.rs rio-worker/src/upload.rs` → ≥2 (T60: both callers migrated)
- `grep 'task-id.*prefix\|Starts with .a.\|Starts with .b.' .claude/skills/dag-run/SKILL.md` → ≥1 hit (T61)
- `grep ':277\|:280\|:284' rio-store/tests/grpc/chunked.rs | grep -v 'r\[\|^//'` → 0 hits OR refs match current `put_path_batch.rs` lines (T62: stale refs updated)
- `grep -c 'tonic-health' rio-common/Cargo.toml` → 1 (T63: dev-dep removed, normal dep at :38 stays)
- `grep 'merge-shas.jsonl' .claude/lib/onibus/git_ops.py` → 0 hits in `_PHANTOM_AMEND_FILES` (T64: dead entry removed)
- `python3 -c "from onibus.git_ops import _PHANTOM_AMEND_FILES; assert _PHANTOM_AMEND_FILES == frozenset({'.claude/dag.jsonl'})"` → no AssertionError (T64)
- `grep 'amend_diff and amend_diff' .claude/lib/onibus/git_ops.py` → 0 hits (T65: truthiness guard dropped)
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'phantom_amend_identical_tree'` → 1 passed (T65)
- **T65 mutation:** re-add `amend_diff and` guard → identical-tree test fails with `phantom_amend is False`
- `grep 'Lock-released WITHOUT notification\|merger still running' .claude/skills/dag-run/SKILL.md` → ≥1 hit (T66)
- T67: `grep ':316-331\|:629-641\|:645' rio-store/src/grpc/put_path_batch.rs` → 0 hits OR line numbers match current `put_path.rs` (drift fixed or fn-name-anchored)
- T68: `grep 'WorkerPool patch\|STS rollout' nix/tests/scenarios/fod-proxy.nix` → 0 hits (stale comment fixed)
- T69: `grep -c 'encode_pubkey_file' rio-common/src/jwt_interceptor.rs` → ≥2 (T69: helper used by both tests; post-P0349-merge)
- T70: `grep 'TODO(phase5)' rio-gateway/src/ratelimit.rs` → 0 hits (removed or re-tagged)
- T71: `grep 'format!.*"max"' rio-gateway/src/server.rs` → 0 hits (literal placeholder removed)
- T72: `grep 'per-path permanent\|Sig-cap.*cardinality' rio-store/src/metadata/mod.rs` → ≥1 hit (docstring split; post-P0213-merge)
- T73: `for f in docs/src/components/*.md docs/src/observability.md docs/src/security.md; do awk 'prev~/^r\[/ && /^$/ {c++} {prev=$0} END{print FILENAME,c+0}' $f; done` → all-zero (no blank-after-marker remaining)
- T73: `nix develop -c tracey query rule store.chunk.grace-ttl` → non-empty text (verification: the P0350-reviewer-cited broken marker now parses)
- T74: `grep '|| true' flake.nix` → 0 hits in the mutants `buildPhaseCargoCommand` block (exit-code check present instead)
- T74: `grep 'rc=\$?' flake.nix` → ≥1 hit (captured exit code check)
- T75: `just --dry-run mutants 2>&1 | grep -c '\$(jq'` → ≥2 (shell sees real command substitution, not literal)
- T76: `grep -c 'goldenTestEnv' flake.nix` → ≥4 (1 definition + ≥3 merge sites)
- T76: `grep 'RIO_GOLDEN_TEST_PATH\|RIO_GOLDEN_CA_PATH' flake.nix | grep -v goldenTestEnv | wc -l` → ≤2 (dedupe leaves ≤2 special-case direct refs, or 0 if fully consolidated)
- T77: `grep 'jwt_jti' rio-scheduler/src/db.rs | grep -c 'INSERT\|VALUES\|\$6\|bind(jti'` → ≥2 (column in INSERT + binding)
- T77: `grep 'jti: Option' rio-scheduler/src/db.rs` → ≥1 hit (new parameter)
- T77: `cargo nextest run -p rio-scheduler grpc::tests` — SubmitBuild test asserts `jwt_jti` readback when Claims present
- T78: `grep '# Expansion candidates\|# "rio-scheduler/src/actor/dispatch.rs"' .config/mutants.toml` → ≥1 hit (commented candidates staged)
- T79: `grep -A1 'needs: golden-matrix' .github/workflows/weekly.yml | grep 'always() && !cancelled()'` → ≥1 hit (mutants job decoupled)
- T80: `grep 'livenessProbe' infra/helm/rio-build/templates/device-plugin.yaml` → ≥1 hit (post-P0286-merge)
- T81: `grep 'rounds up' rio-gateway/src/quota.rs` → 0 hits (false claim removed)
- T81: `grep 'rounds to nearest\|one decimal place' rio-gateway/src/quota.rs` → ≥1 hit (corrected doc)
- T82: `grep 'code = ?status.code()' rio-gateway/src/quota.rs` → 0 hits (split code+message collapsed to error=%status)
- T82: `grep 'tracing::warn!' rio-gateway/src/quota.rs` → 0 hits (bare warn! — consistent with crate-level import)
- T83: `grep -c 'rio_gateway_' rio-gateway/tests/metrics_registered.rs` → ≥10 (both new metrics added to GATEWAY_METRICS const)
- T83: `cargo nextest run -p rio-gateway metrics_registered` → all pass (spec_metrics_described subset check still green with the fuller const)
- T84: `grep 'Result<\[u8; 32\], Status>' rio-store/src/grpc/mod.rs` → 0 hits (apply_trailer returns unit)
- T84: `grep 'Returns the applied hash' rio-store/src/grpc/mod.rs` → 0 hits (docstring rationale removed)
- T85: `grep 'BEGIN..COMMIT\|wraps both.*BEGIN' rio-store/src/migrations.rs` → 0 hits (wrong claim removed)
- T85: `grep 'two separate statements\|sequential-commit' rio-store/src/migrations.rs` → ≥1 hit (honest autocommit note)
- T86: `grep '/tmp/jwt-' flake.nix` → 0 hits (all → `$TMPDIR/jwt-`)
- T86: `grep '$TMPDIR/jwt-' flake.nix` → ≥8 hits (helm-lint yq/grep sites all consistent)
- T87: `grep -c 'fn counters\|fn gauges' rio-test-support/src/metrics.rs` → 0 (dead accessors removed) OR ≥2 with `#[allow(dead_code)]` adjacent
- T88: `/nixbuild .#coverage-full` → no E0583 (cov build compiles rio-store with migrations mod; may need cache-invalidate one-off)
- T89: `grep 'sql\|migration' flake.nix | grep -i 'sqlx-prepare'` → ≥1 hit (files regex OR reminder branch added; post-P0297-merge)
- T90: `grep 'NOTE(fault-line)' rio-store/src/grpc/mod.rs` → 1 hit at `:124` region
- T91: `grep -c 'pub async fn sign_for_tenant' rio-store/src/signing.rs` → 0 (fn deleted; tests use resolve_once)
- T91: `grep 'r\[impl store.tenant.sign-key\]' rio-store/src/signing.rs` → 1 hit on `resolve_once` (annotation migrated)
- T91: `cargo nextest run -p rio-store signing` → all 3 tests still pass (now via resolve_once)
- T92: `grep 'e.to_string()\|{e}' rio-scheduler/src/admin/mod.rs | grep -i tenant` → ≥1 hit (caller formats EmptyTenantName) OR `grep '#\[error' rio-common/src/tenant.rs` → 0 (Display dropped)
- T94: `grep -c 'if .Values.tls.enabled' infra/helm/rio-build/templates/_helpers.tpl` → ≥3 (three tls defines self-guarded)
- T94: `grep -B1 'include "rio.tlsEnv"' infra/helm/rio-build/templates/*.yaml | grep 'if .Values.tls.enabled'` → 0 hits (caller guards removed — helper self-guards)
- T94: `nix build .#checks.x86_64-linux.helm-lint` → green (template renders clean with tls.enabled=true and tls.enabled=false)
- T95: `grep 'Deserialize' rio-scheduler/src/rebalancer.rs | grep RebalancerConfig` → ≥1 hit (derive added)
- T95: `grep 'RebalancerConfig::default()' rio-scheduler/src/rebalancer.rs` → 0 hits in `spawn_task` body (config threaded from main.rs, not constructed inline — `impl Default` block keeps one `::default` for the trait itself)
- T96: `grep 'durations.is_empty()\|min_samples >= 1' rio-scheduler/src/rebalancer.rs rio-scheduler/src/main.rs` → ≥1 hit (defense-in-depth guard)
- T96: `cargo nextest run -p rio-scheduler rebalancer` → no new panics (guard prevents the wraparound, not just hides it)
- T97: `grep '"gc-sweep".*r\[verify' CLAUDE.md` → 0 hits (same-line trailing marker removed from example)
- T97: `sed -n '190,200p' CLAUDE.md | grep -c '^  # r\[verify'` → ≥4 (all markers col-0 in the example block)
- T98: `grep -c 'Rule::new\|\.message(' rio-crds/src/workerpool.rs` → ≥6 each (three pre-existing + two P0354/P0359 = five rules, each uses Rule::new + .message)
- T98: `grep 'validation = "self' rio-crds/src/workerpool.rs` → 0 hits (no bare-string validations remain)
- T98: `cargo nextest run -p rio-crds cel_rules_in_schema` → pass with message-presence asserts for all rules
- T99: `grep '"rio.build/pool"' rio-controller/src/reconcilers/workerpool/{builders,disruption,ephemeral,mod}.rs` → ≤1 hit (the const definition only)
- T99: `grep 'POOL_LABEL' rio-controller/src/reconcilers/workerpool/mod.rs` → ≥1 hit (`pub(crate) const` at module level)
- T100: `grep 'timeout=300\|--timeout=270s' nix/tests/fixtures/k3s-full.nix` → ≥2 hits (both bumped)
- T100: `grep '180.*should suffice' nix/tests/fixtures/k3s-full.nix` → 0 hits (aspirational claim removed)
- T101: `grep 'rio_controller_disruption_drains_total' rio-controller/src/reconcilers/workerpool/disruption.rs docs/src/observability.md rio-controller/tests/metrics_registered.rs` → ≥3 hits (emit + spec-table + registered-test)
- T101: `grep 'describe_counter!.*disruption_drains' rio-controller/src/` → ≥1 hit (described)
- T102: `grep 'workerpool|workerpoolset\|workerpool\\|workerpoolset' rio-controller/src/lib.rs | grep reconciler` → ≥2 hits (both describe-strings updated)
- T103: `grep '63\|RFC-1123\|RFC 1123' rio-controller/src/reconcilers/workerpoolset/builders.rs` → ≥1 hit (length guard)
- T103: `grep 'Result<String>' rio-controller/src/reconcilers/workerpoolset/builders.rs | grep child_name` → ≥1 hit (signature change)
- T104: `grep 'streamIdleTimeout.*Values\|dashboard\.streamIdleTimeout' infra/helm/rio-build/templates/dashboard-gateway-policy.yaml infra/helm/rio-build/values.yaml` → ≥2 hits (template + values)
- T105: `grep 'TODO(P0371)' infra/helm/rio-build/templates/dashboard-gateway*.yaml` → ≥2 hits (both comments tagged) — OR 0 if P0371 already deleted them
- T106: `grep 'DEFAULT_GC_RETENTION_HOURS' rio-scheduler/src/db.rs` → ≥2 hits (const decl + reference)
- T107: `grep 'parse_build_id' rio-scheduler/src/admin/mod.rs` → ≥1 hit (shared helper used) — OR `grep 'invalid build_id UUID:' rio-scheduler/src/admin/mod.rs` → 0 (inline gone)
- T108: `grep 'ASSIGNMENT_TOKEN_HEADER' rio-proto/src/lib.rs rio-store/src/grpc/put_path.rs rio-worker/src/upload.rs` → ≥4 hits (const + 3 use-sites)
- T108: `grep '"x-rio-assignment-token"' rio-store/src/grpc/put_path.rs rio-worker/src/upload.rs` → 0 hits (literals migrated)
- T108: `grep 'TENANT_TOKEN_HEADER' rio-gateway/src/handler/build.rs` → ≥1 hit (gateway literal migrated)
- T109: `grep 'def grpcurl_json_stream' nix/tests/scenarios/lifecycle.nix` → ≥1 hit (helper defined)
- T109: `grep -c 'raw_decode' nix/tests/scenarios/lifecycle.nix` → ≤2 (migrated; ≤2 allows for the helper body itself)
- T110: `grep 'Clause-4 fast-path decision tree\|hash-identical > behavioral-identity' .claude/agents/rio-impl-merger.md` → ≥1 hit (section added)
- T111: `grep 'with_ed25519_key\|ed25519_seed' rio-test-support/src/pg.rs` → ≥3 hits (field + method + INSERT)
- T111: `grep 'INSERT INTO tenant_keys' rio-store/tests/grpc/signing.rs` → ≤1 hit (inline INSERTs migrated to builder — ≤1 allows for one non-migratable edge case)
- T111: `grep 'pg.rs:460.*T106\|DEFAULT_GC_RETENTION' rio-test-support/src/pg.rs` → ≥1 hit (COALESCE sibling-note pointing at T106 const)
- T112: `grep 'returns.*min\|compute_desired.*0' rio-controller/src/scaling.rs | grep -v TODO` → verified-consistent (comment matches code at dispatch OR fix applied)
- T113: `grep 'P0234 adds' rio-controller/src/reconcilers/workerpoolset/builders.rs` → 0 hits (future-tense removed; post-P0234-merge)
- T114: `grep 'child_name(wps, class)' rio-controller/src/scaling.rs` → ≥1 hit (format! replaced with shared fn)
- T114: `grep 'format!.*"{}-{}".*wps.name_any' rio-controller/src/scaling.rs` → 0 hits in scale_wps_class body (dup eliminated)
- T115: `grep 'bloom_expected_items.*=.*expected\|bloom.*filter sized' rio-worker/src/fuse/cache.rs` → ≥1 hit (info! log added)
- T116: `grep 'export let' rio-dashboard/src/pages/Builds.svelte` → 0 hits (Svelte-4 syntax migrated)
- T116: `grep '\$props()' rio-dashboard/src/pages/Builds.svelte` → ≥1 hit (Svelte-5 syntax)
- T117: `grep 'document.hidden\|visibilitychange' rio-dashboard/src/pages/Cluster.svelte` → ≥1 hit (tab-pause added)
- T118: `nix path-info -S .#dockerImages.dashboard | awk '{print $2}'` → smaller than pre-T118 baseline (libxslt removed) — OR `grep 'withXslt.*false\|xslt.*false' nix/docker.nix` → ≥1 hit
- T119: `grep 'devicePlugin.*tag.*sync\|image-tag-lockstep' flake.nix nix/tests/fixtures/k3s-full.nix` → ≥1 hit (lockstep assert present)
- T120: `wc -l .claude/notes/kvm-pending.md` → ≥2 lines more than pre-T120 (entries added)
- T121: `grep 'ssa_envelope\|fn ssa_envelope' rio-controller/src/reconcilers/mod.rs` → ≥1 hit (helper defined)
- T121: `grep -c 'ssa_envelope::<' rio-controller/src/scaling.rs rio-controller/src/reconcilers/workerpool/mod.rs rio-controller/src/reconcilers/workerpool/ephemeral.rs rio-controller/src/reconcilers/workerpoolset/mod.rs` → ≥4 (sites migrated)
- T122: `grep 'let busy\|busy = \$state' rio-dashboard/src/components/DrainButton.svelte` → 0 hits IF option-(b) taken (busy state removed, status check covers in-flight)
- T123: `grep 'tokio::spawn' rio-scheduler/src/grpc/worker_service.rs` → 0 hits (migrated to spawn_monitored)
- T123: `grep 'spawn_monitored.*ema-proactive' rio-scheduler/src/grpc/worker_service.rs` → ≥1 hit
- T124: `grep 'aria-label.*status\|●\|○\|✕' rio-dashboard/src/pages/Workers.svelte` → ≥1 hit (icon-prefix or aria-label on pill)
- T125: `grep 'role="tablist"\|aria-selected' rio-dashboard/src/components/BuildDrawer.svelte` → ≥2 hits (tablist + per-tab aria-selected)
- T126: `grep 'tabindex="0".*onkeydown\|onkeydown.*Enter' rio-dashboard/src/pages/Builds.svelte` → ≥1 hit (keyboard row-select)
- T127: `grep 'export function buildProgress\|from.*lib/build' rio-dashboard/src/lib/build.ts rio-dashboard/src/pages/Builds.svelte rio-dashboard/src/components/BuildDrawer.svelte` → ≥3 hits (extracted + imported ×2)
- T128: `grep 'is_wps_owned_by(child, wps)' rio-controller/src/scaling.rs` → ≥1 hit (find_wps_child uses UID-gate; post-P0374)
- T128: `grep 'is_wps_owned(child)' rio-controller/src/scaling.rs` → 0 hits in find_wps_child body (kind-only gate removed; the tick() skip at :273 stays — it's "is A wps child", not "is THIS wps's child")
- T128: `cargo nextest run -p rio-controller find_wps_child_rejects_wrong_wps_uid` → 1 passed
- T129: `grep 'allowOrigins contains the placeholder\|override with your' infra/helm/rio-build/templates/NOTES.txt` → ≥1 hit (hint rendered)
- T129: `helm template rio infra/helm/rio-build --set dashboard.enabled=true 2>&1 | grep 'WARNING.*allowOrigins'` → ≥1 hit (NOTES.txt conditional fires with default values)
- T130: `grep 'until dashboard-native authz lands' infra/helm/rio-build/templates/dashboard-gateway.yaml infra/helm/rio-build/values.yaml docs/src/components/dashboard.md` → 0 hits (3 sites rephrased)
- T130: `grep 'per-user dashboard auth is scoped\|no plan yet' infra/helm/rio-build/templates/dashboard-gateway.yaml infra/helm/rio-build/values.yaml` → ≥2 hits (replacement text present at yaml sites; dashboard.md is spec text so wording may differ)
- T131: `grep 'NOT splitting Cmd into KubeCmd' rio-cli/src/main.rs` → ≥1 hit (design-note doc-comment added; post-P0237)
- T132: `grep 'dashboard' nix/tests/fixtures/k3s-full.nix | grep -i 'bumpGiB\|bump unit'` → ≥1 hit (dashboard counted in tmpfs sizing formula or comment)
- T133: `grep 'pub fn wps_child_name' rio-crds/src/workerpoolset.rs` → 1 hit (hoisted to rio-crds; post-P0237 + T103/T114)
- T133: `grep 'wps_child_name\|rio_crds::wps_child_name' rio-cli/src/wps.rs rio-controller/src/reconcilers/workerpoolset/builders.rs` → ≥3 hits (CLI 2× + controller wrapper)
- T133: `grep 'format!.*"{}-{}"' rio-cli/src/wps.rs` → 0 hits (inline format! migrated)
- T134: `grep 'types::BuildEvent\|types::SchedulerMessage\|types::WorkerMessage\|types::Heartbeat\|types::WorkAssignment' rio-scheduler/src/ rio-worker/src/ rio-gateway/src/ -r` → ≤5 hits IF option-(a) taken (210→~5 residual edge cases); OR `grep 'build_types:: migration is opportunistic' rio-proto/src/lib.rs` → ≥1 hit IF option-(b) (doc-only)
- T135: `grep 'mc=168\|mc168' .claude/notes/bughunter-log.md` → ≥1 hit (cadence entry recorded)
- T136: `grep 'envoy.*lockstep\|envoyValues.*envoyAirgap' flake.nix` → ≥1 hit (envoy tag-sync assert present, adjacent to T119's devicePlugin assert)
- T137: `grep 'c.last or false' nix/tests/common.nix` → 1 hit (truthiness check; `grep 'c ? last' nix/tests/common.nix` → 0 hits)
- T138: `grep 'pub mod admin_types' rio-proto/src/lib.rs` → 1 hit (parity re-export; post-P0376)
- T139: `grep 'is_ca UPDATE is idempotent' rio-scheduler/src/db.rs` → 1 hit (safety comment at :840 — bughunt-verified invariant documented)
- T140: `grep 'ACTOR_UNAVAILABLE_MSG' rio-scheduler/src/grpc/actor_guards.rs` → ≥1 hit (const defined); `grep -c 'scheduler actor is unavailable' rio-scheduler/src/grpc/` → 1 (only the const literal; :21+:142 duplicates collapsed)
- T141: `grep 'pub(crate) fn actor_error_to_status' rio-scheduler/src/grpc/actor_guards.rs` → 1 hit (hoisted); `grep 'SchedulerGrpc::actor_error_to_status' rio-scheduler/src/admin/` → 0 hits (admin callers use actor_guards:: path); post-P0383
- T142: `grep 'test_workerpoolset\b\|test_workerpoolset_spec' rio-controller/src/fixtures.rs` → ≥2 hits (both fns defined); `grep 'WorkerPoolSetSpec {' rio-controller/src/scaling.rs rio-controller/src/reconcilers/workerpoolset/builders.rs` → 0 hits (test literals migrated); post-P0380
- T143: `grep 'mc=182\|mc182' .claude/notes/bughunter-log.md` → ≥1 hit (cadence entry recorded)
- T144: `grep 'fn is_content_addressed' rio-nix/src/derivation/mod.rs` → 1 hit (trait default added); `grep 'is_fixed_output() || .*has_ca_floating' rio-gateway/src/translate.rs` → 0 (disjunction-sites migrated to `.is_content_addressed()`); post-P0384
- T145: `grep -c 'fn builder\|fn args\|fn input_srcs' rio-nix/src/derivation/mod.rs | head -1` — count inherent-accessor defs only (trait required-methods removed; `grep 'fn builder(&self)' rio-nix/src/derivation/mod.rs` inside `trait DerivationLike` block → 0); post-P0384
- T146: `grep 'fn has_hash_algo' rio-nix/src/derivation/mod.rs` → 1 hit (renamed); `grep 'DerivationOutput.*is_fixed_output' rio-*/src/` → 0 (old name gone except doc-comment historical refs); post-P0384
- T147: `grep 'mc=189\|mc189' .claude/notes/bughunter-log.md` → ≥1 hit (cadence entry recorded)
- T148: `grep '"error"' rio-scheduler/src/actor/completion.rs` → ≥1 hit in the ca_hash_compares match (third outcome label); `grep 'match.*miss.*error' rio-scheduler/src/lib.rs` → ≥1 hit OR `grep 'three outcomes' rio-scheduler/src/lib.rs` → ≥1 hit (describe_counter updated); post-P0251
- T149: `grep -c 'rio_gateway_' rio-gateway/tests/metrics_registered.rs` → ≥11 (three new entries; STOPGAP — skip if P0394 dispatched)
- T150: `grep 'from_maybe_empty' rio-gateway/src/quota.rs` → 0 hits (stale ref replaced)
- T150: `grep 'normalize_key_comment' rio-gateway/src/quota.rs` → ≥1 hit (correct ref present)
- T151: `grep 'provably unreachable' rio-store/src/cache_server/auth.rs` → 0 hits (overclaim softened)
- T151: `grep 'Unicode whitespace\|POSIX.*space' rio-store/src/cache_server/auth.rs` → ≥1 hit (gap documented)
- T152: `grep 'REASON_SPEC_DEGRADED\|pub(crate) const REASON_' rio-controller/src/reconcilers/workerpool/mod.rs` → ≥1 hit (const defined)
- T152: `grep 'body_substr' rio-controller/src/reconcilers/workerpool/tests.rs rio-controller/src/reconcilers/workerpool/tests/disruption_tests.rs` → ≥1 hit (param renamed; post-split path if P0396 landed)
- T153: `grep 'TODO(P0311' rio-worker/src/runtime.rs` → ≥1 hit near `:814` (hook tagged) — OR `grep 'RIO_TEST_PREFETCH_DELAY_MS' rio-worker/src/runtime.rs` → 0 hits if option-(a) delete taken
- T154: `grep 'TODO(P' rio-dashboard/src/lib/logStream.svelte.ts` → ≥1 hit near `:42` (untagged deferral tagged; post-P0279-merge)
- T155: `grep 'TODO(P0280)' rio-dashboard/src/components/LogViewer.svelte rio-dashboard/src/lib/logStream.svelte.ts` → ≥2 hits (drvPath sites tagged; post-P0279-merge)
- T156: `sed -n '7p' rio-scheduler/src/admin/tests/gc_tests.rs | grep StreamExt` → 0 hits (redundant import removed; post-P0386-merge)
- T157: `grep '"malformed"' rio-scheduler/src/actor/completion.rs` → ≥1 hit (counter label added — partners with T148's error label)
- T157: `grep 'outcome=.*malformed\|malformed.*outcome' docs/src/observability.md` → ≥1 hit (label in spec table; coordinate with P0295-T91 + T148)
- T163: `grep 'mc=203\|e5feb62c..8155e004' .claude/notes/bughunter-log.md` → ≥1 hit (cadence entry added)
- T164: `grep 'if pad > 0' rio-nix/src/protocol/wire/mod.rs` → 0 hits at `:87`/`:158` (guards deleted — unconditional read/write); `grep ':87:12\|:158:16' .config/mutants.toml` → 0 hits (exclusions removed)
- T164: `cargo nextest run -p rio-nix protocol::wire::` → all pass (0-byte read_exact/write_all no-op confirmed)
- T165: `ls rio-nix/proptest-regressions/derivation/aterm.txt rio-nix/proptest-regressions/protocol/wire/mod.txt 2>/dev/null` → both tracked OR both in `.gitignore` (policy consistent — check route at dispatch)
- T166: `grep 'drop(at_max)\|tokio::io::sink()' rio-nix/src/protocol/wire/mod.rs` → ≥2 hits (drop + sink both applied)
- T167: `grep 'vi.hoisted\|vi.mock.*admin' rio-dashboard/src/lib/__tests__/logStream.test.ts rio-dashboard/src/__tests__/App.test.ts` → 0 hits (both migrated to adminMock; post-P0389-merge)
- T168: `grep 'mockResolvedValue.*nodes.*\[\]' rio-dashboard/src/test-support/admin-mock.ts` → ≥1 hit (getBuildGraph empty-default; post-P0389-merge)
- T169: `grep -rn 'MAX_CASCADE_DEPTH' rio-scheduler/src/` → 0 hits (renamed everywhere — SCOPE-+2 post-P0405: completion.rs:68,84,511 + dag/mod.rs:21,635,646 + lib.rs:272)
- T169: `grep -rn 'MAX_CASCADE_NODES' rio-scheduler/src/` → ≥6 hits (all 7 sites renamed; :17 comment may rephrase not mention const)
- T169: `grep 'node-count cap' rio-scheduler/src/lib.rs` → ≥1 hit in depth_cap_hits_total describe_counter (semantics clarified)
- T170: `grep 'sourcePosition.*string literals\|targetPosition.*bottom' rio-dashboard/src/lib/graphLayout.ts` → 0 hits (stale comment deleted/rewritten; post-P0280-merge)
- T171: SKIP if P0400 dispatched (superseded); otherwise `grep 'TERMINAL\|clearInterval' rio-dashboard/src/pages/Graph.svelte` → ≥2 hits
- T172: `grep -E '99639410[0-9]|99710550[0-9]|99744370[0-9]|99890214[0-9]' .claude/work/plan-0287-*.md .claude/work/plan-0295-*.md .claude/work/plan-0311-*.md .claude/work/plan-0304-*.md` → 0 hits (all stale placeholders rewritten — includes P0304 self-referential)
- T172: `grep 'P0356\|plan-0356' .claude/work/plan-0287-*.md` → ≥1 hit (996394104→0356 rewrite applied)
- T172: `grep 'P0353\|P0362\|P0374\|P0375' .claude/work/plan-0304-*.md | grep -v 'T172\|sed map\|T30'` → ≥4 hits (self-referential 353/997105502/998902141/998902142 rewritten in soft_deps + note body, excluding T172's own description of the mapping)
- T173: `grep -c '\$TGT\.\.[^.]' .claude/agents/rio-impl-reviewer.md .claude/agents/rio-impl-validator.md .claude/agents/rio-ci-flake-validator.md .claude/agents/rio-plan-reviewer.md` → 0 (all 2-dot diffs → 3-dot; merger's git-log 2-dot preserved)
- T173: `grep -c '\$TGT\.\.\.' .claude/agents/rio-impl-reviewer.md .claude/agents/rio-impl-validator.md .claude/agents/rio-ci-flake-validator.md .claude/agents/rio-plan-reviewer.md` → ≥10 (3-dot applied at all sites)
- T174: (addendum to T158's criteria) `grep 'to_aterm_modulo\|to_aterm_with' rio-nix/src/derivation/aterm.rs` — if T158 route-(a) taken with 3-way dedup, `to_aterm_with` present; `to_aterm_modulo` body delegates or is removed
- T175: `sed -n '63,72p' rio-scheduler/src/admin/builds.rs | grep -c 'bad cursor version'` before `grep -c 'bad cursor length'` — version-check precedes length-check (check by line-number order; post-P0271-merge)
- T176: `grep 'version_byte.*submitted_at_micros' rio-proto/proto/admin_types.proto` → 0 hits (encoding leak removed); `grep 'Opaque keyset cursor' rio-proto/proto/admin_types.proto` → ≥1 hit
- T177: `sed -n '539,543p' nix/tests/default.nix | grep "doesn't$"` → 0 hits (no orphaned "doesn't" line-end; post-P0295-T65-merge)
- T178: `grep 'envoy-gateway-system.svc' infra/helm/rio-build/templates/networkpolicy.yaml` → 0 hits (comment templated); `grep '{{ .Values.dashboard.envoyGatewayNamespace }}.svc' infra/helm/rio-build/templates/networkpolicy.yaml` → ≥1 hit (post-P0295-T69)
- T179: `grep 'DeployConfig\|ActorConfig' rio-scheduler/src/actor/handle.rs` → ≥1 hit (bundle type defined); `grep -c 'pub fn spawn_with_leader' rio-scheduler/src/actor/handle.rs` → arg-count ≤8 (bundle reduced params; post-P0307-merge)
- T180: `grep 'ttl' rio-scheduler/src/state/derivation.rs | grep PoisonConfig` → ≥1 hit (POISON_TTL absorbed into struct) OR explicitly skipped at dispatch with rationale (low-priority — only if TOML knob operationally needed)
- T181: `grep 'allTerminal' rio-dashboard/src/pages/Graph.svelte | grep -v 'truncated'` → 0 hits (guard added) OR comment softened with "heuristic" (post-P0400-merge)
- T182: `grep 'pub(super)' rio-controller/src/reconcilers/workerpool/tests/mod.rs` → 0 hits (all `pub(crate)`); `wc -l rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs` → <700 (split into builders + quantity; post-P0396-merge)
- T183: `grep '>= 5' rio-controller/tests/metrics_registered.rs` → 0 hits (floor bumped to 6); `grep 'cargo:warning' rio-test-support/src/metrics_grep.rs | grep -c 'CI\|STRICT'` → ≥1 (env-gated; post-P0394-merge)
- T184: `grep -c 'devicePlugin.image' infra/helm/rio-build/values/vmtest-full-nonpriv.yaml` → 0 (no hardcode; consistent with Nix-derived override) AND `grep 'smarter-device-manager.destNameTag' nix/tests/default.nix` → ≥1 (post-P0387; verify-at-dispatch whether a second profile exists that also needs migration)
- T185: `grep 'envoyGatewayAppVersion\|gateway-helm.*appVersion\|operator tag drift' flake.nix nix/docker-pulled.nix` → ≥2 hits (let-binding + assertMsg OR comment). `nix eval .#checks.x86_64-linux.helm-lint` → no IFD (route-c taken, plain let-binding not readFile+fromJSON on Chart.yaml). Post-P0387-merge
- T186: `grep 'no chart-side digest\|gateway-helm uses bare tags\|why no helm-lint assert' nix/docker-pulled.nix` → ≥1 hit near `:79-82` (asymmetry-explaining comment). Post-P0387-merge
- T187: `grep -c 'ca_resolve_degraded_total' rio-scheduler/src/actor/dispatch.rs rio-scheduler/src/lib.rs docs/src/observability.md` → ≥5 (3× emit in dispatch.rs + 1× describe in lib.rs + 1× table row in obs.md). Post-P0408-merge
- T187: `grep '"reason"' rio-scheduler/src/actor/dispatch.rs | grep ca_resolve_degraded` → ≥3 (three reason labels: store_fetch_failed / resolve_error / modular_hash_unset)
- T188: `grep 'ageMs' rio-dashboard/src/pages/__tests__/Workers.test.ts` → 0 hits at `:23` comment IF P0406 renamed; OR ≥1 hit + `grep 'function ageMs' rio-dashboard/src/pages/Workers.svelte` → ≥1 (P0406 kept thin wrapper — comment still correct). Verify-at-dispatch. Post-P0406-merge
- T189: `grep 'cutoff_secs.is_finite' rio-scheduler/src/main.rs` → ≥1 hit INSIDE `validate_config` body (moved from :347 inline); `grep -A5 'for class in &cfg.size_classes' rio-scheduler/src/main.rs | grep -c 'anyhow::ensure'` → 1 (only the validate_config copy, not a duplicate left at :347)
- T189: `cargo nextest run -p rio-scheduler config_` — no regressions (all P0409 rejection tests still pass; optionally +config_rejects_nan_cutoff passes)
- T190: `grep 'mc=224' .claude/notes/bughunter-log.md` → ≥1 hit (entry added)
- T191: `grep 'file=sys.stderr' .claude/lib/onibus/merge.py | grep -c 'dag_flip\|UNEXPECTED'` → ≥1 (stderr log in update-ref fallback); `grep 'ref_forced' .claude/lib/onibus/models.py` → ≥1 hit in DagFlipResult (field added)
- T192: `grep 'pub fn find_cutoff_eligible\b' rio-scheduler/src/dag/mod.rs` → 0 hits (thin wrapper deleted — `find_cutoff_eligible_speculative` stays); `cargo clippy -p rio-scheduler --all-targets -- --deny warnings` — no unused-fn (confirms deletion, not just private)
- T192: `grep 'find_cutoff_eligible\b' rio-scheduler/src/ -r | grep -v '_speculative'` → 0 non-`_speculative` hits in code (all 4 doc-comment mentions rewritten to reference `_speculative` or `cascade_cutoff`)
- T193: `grep -c 'vm-fod-proxy-k3s\|vm-scheduling-disrupt-standalone\|vm-netpol-k3s\|vm-ca-cutoff-standalone\|jwt-mount-present\|vm-security-nonpriv-k3s' .claude/notes/kvm-pending.md` → ≥6 (all entries backfilled; SUPERSEDES T120's partial "+2")
- T193: `grep 'P0405\|speculative_cascade_reachable\|HIGH PRIORITY' .claude/notes/kvm-pending.md` → ≥1 hit (vm-ca-cutoff-standalone entry references the P0405 refactor)
- T194: `grep ':[0-9][0-9][0-9]' rio-scheduler/src/actor/tests/completion.rs | grep -v 'r\[\|TODO\|#\[' | grep -cE ':(397|399|422|429|470|476)\b'` → 0 (stale line-cites replaced with fn-name/code-shape cites)
- T195: `grep "test(/tests::/)" .config/nextest.toml` → ≥1 hit (anchor dropped); `grep "test(/\^tests::/)" .config/nextest.toml` → 0 hits at the rio-scheduler postgres override (old anchored form removed from :73)
- T195: `cargo nextest list --package rio-scheduler 2>&1 | grep 'actor::tests::' | head -1` — actor::tests:: paths exist (verify they now match postgres group at dispatch via `cargo nextest show-config test-groups`)
- T196: `grep 'main.rs:1012\|main.rs:1021\|main.rs:875' rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/config.rs rio-scheduler/src/main.rs | grep -v '^Binary'` → 0 hits (all bare-line cites → fn-name); `grep 'all_subconfigs_roundtrip_toml' rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/config.rs | grep -c '//'` → ≥4 (banner comments reference fn-name)
- T197: `grep 'Standing-guard tests\|P0219 failure mode' rio-common/src/config.rs` → ≥2 hits (rationale paragraph in module-doc); `wc -l rio-store/src/main.rs rio-gateway/src/main.rs rio-controller/src/main.rs rio-worker/src/config.rs | tail -1` → ~40 lines lower than pre-T197 (banners shrunk to 1-line pointers)
- T198: `grep 'ADD-IT-HERE is advisory\|Known limitation' rio-common/src/config.rs` → ≥1 hit (limitation documented in module-doc)
- T199: `grep '\*\*/gen/\*\*/\*_pb' rio-dashboard/eslint.config.js` → ≥2 hits (both patterns widened); `grep "'\*\*/gen/\*_pb'" rio-dashboard/eslint.config.js` → 0 hits at :39 (single-star form gone)
- T200: `grep 'db\.rs:537\|db\.rs will decode' rio-scheduler/src/actor/tests/build.rs rio-scheduler/src/actor/completion.rs` → 0 hits; `grep 'TERMINAL_STATUS_SQL const\|db/recovery.rs' rio-scheduler/src/actor/tests/build.rs rio-scheduler/src/actor/completion.rs | grep -c '//'` → ≥2 (grep-stable cites)
- T201: `grep 'rio-scheduler/src/db\.rs' docs/src/components/scheduler.md` → 0 hits; `grep 'rio-scheduler/src/db/' docs/src/components/scheduler.md` → ≥1 hit (:664 updated)
- T202: `grep 'pub(crate) async fn connect_worker_no_ack' rio-scheduler/src/actor/tests/helpers.rs` → 1 hit (promoted); `grep 'async fn connect_worker_no_ack' rio-scheduler/src/actor/tests/worker.rs` → 0 hits (moved out); `cargo nextest run -p rio-scheduler 'actor::tests::worker'` → all pass (delegation preserves behavior)
- T203: `grep 'subprocess\.run.*rev-parse' .claude/lib/onibus/merge.py | grep -v '^\s*#'` → ≤1 hit (the :551 merge-base --is-ancestor stays raw; 3 rev-parse folded to git() OR added git_try variant); `nix develop -c pytest .claude/lib/test_scripts.py -k 'dag_flip or count_bump'` → all pass
- T204: `grep -A3 'if row.plan == plan_num' .claude/lib/onibus/merge.py | grep -c 'break'` → 0 (loop runs through, last match wins) OR `grep 'reversed(.*read_jsonl' .claude/lib/onibus/merge.py` → ≥1 hit (reversed iter + break-on-first)
- T205: `grep 'scale_up_window_secs must be\|scale_down_window_secs must be\|min_interval_secs must be' rio-controller/src/main.rs` → ≥3 hits (all three ensures added); `cargo nextest run -p rio-controller validate_config` → all pass
- T206: `wc -l rio-scheduler/src/db/mod.rs` → ≤180 (row structs moved to domain files); `python3 -c "import subprocess; out=subprocess.check_output(['cargo','check','-p','rio-scheduler'], stderr=subprocess.STDOUT, text=True); assert 'error' not in out.lower()"` — compiles (pub-use re-exports keep external callers working)
- T208: `grep 'cargo-mutants 26\|MUTATION_MARKER_COMMENT' flake.nix` → ≥1 hit near check-mutants-marker hook (version-anchor comment)
- T209: `grep 'mc=235\|mc235\|mc=236\|mc236\|mc=238\|mc238' .claude/notes/bughunter-log.md` → ≥1 hit (archive entries added)
- T207: `grep 'NOTE(fault-line)\|db/admin_reads' rio-scheduler/src/db/recovery.rs` → ≥1 hit (cohesion comment at load_build_graph/read_event_log)
- T210: `grep 'preemptive\|due_preemptive' .claude/lib/onibus/merge.py .claude/lib/onibus/models.py` → ≥2 hits (flag in CadenceWindow + compute logic)
- T211: `grep 'merge-base --is-ancestor' .claude/lib/onibus/merge.py | grep lock_status` → ≥1 hit near lock_status body (ancestor-check discriminator)
- T212: `grep 'droppedLines' rio-dashboard/src/lib/logStream.svelte.ts rio-dashboard/src/components/LogViewer.svelte` → ≥3 hits (state + getter + banner render)
- T213: `grep 'title={line}' rio-dashboard/src/components/LogViewer.svelte` → 1 hit
- T214: `grep '@internal\|_viewportOverride' rio-dashboard/src/components/LogViewer.svelte` → ≥1 hit (test-only annotation)
- T215: `grep 'viewport-relative\|NOT log-absolute' rio-dashboard/src/components/LogViewer.svelte` → ≥1 hit (Option-C footgun comment at :138)
- T216: `grep -c 'const MAX_PREFETCH_PATHS' rio-scheduler/src/actor/*.rs` → 1 (hoisted to one spot, dup removed); `grep 'use.*MAX_PREFETCH_PATHS\|pub(crate) const MAX_PREFETCH_PATHS' rio-scheduler/src/actor/` → ≥2 hits (def + ≥1 import)
- T217: `grep 'pub struct GaugeValues\|impl Recorder for GaugeValues' rio-test-support/src/metrics.rs` → ≥2 hits (promoted); `grep 'use rio_test_support::metrics::GaugeValues' rio-scheduler/src/rebalancer/tests.rs` → 1 hit (consumer migrated)
- T218: `grep 'out\[-500:\]\|tail:' nix/tests/scenarios/security.nix` → ≥1 hit (debug-context restored); same for fod-proxy.nix
- T219: `grep "'-A " nix/tests/scenarios/lifecycle.nix` → 0 hits in build() calls (attr= kwarg used)
- T220: `grep 'def build_ca_chain\|mkBuildHelperV2' nix/tests/scenarios/ca-cutoff.nix` — helper absorbed, ~15L net removed
- T221: `grep ':128' rio-scheduler/src/assignment.rs rio-scheduler/src/main.rs` → 0 hits citing `c > limit` (updated to current line)
- T222: `grep 'grpc_timeout(3\|grpc_timeout(Duration::from_secs(3))' rio-scheduler/src/actor/tests/completion.rs` → 0 hits at ~:363 region
- T223: `grep 'Rule::new' rio-crds/src/workerpool.rs` — SeccompProfileKind now uses Rule::new().message() like siblings
- T224: `grep 'TODO[^(]' nix/tests/fixtures/k3s-full.nix` → 0 hits (tagged or resolved)
- T225: `grep 'ON CONFLICT DO NOTHING' rio-store/src/gc/mod.rs` → route-(a): 0 hits in decrement_and_enqueue INSERT (clause dropped); OR route-(b): `grep 'UNIQUE.*blake3_hash' migrations/` → ≥1 hit (constraint added via new migration)

## Tracey

No new markers. T2 implicitly serves `r[obs.metric.scheduler]` (the queries reference spec'd metrics) but adds no `r[impl]`/`r[verify]` annotations — dashboard JSON is not annotated. T11 serves `r[worker.seccomp.localhost-profile]` (the CEL rules guard the spec'd Localhost coupling) but the fix is UX (error messages), not behavior — no annotation change. T25 corrects spec drift under `r[sched.timeout.per-build]` — code already matches corrected text, no annotation change. T26 extends the `r[obs.metric.scheduler]` table — doc-side, no annotation. T27 is a tracey mechanical fix: the three `r[sched.ca.*]` markers ALREADY EXIST at [`scheduler.md:253,257,261`](../../docs/src/components/scheduler.md); the blank-line deletions make tracey parse their text correctly. No bump — text content didn't change, tracey's view of it did. T38 serves `r[obs.metric.store]` (the `rio_store_put_path_total` and `_duration_seconds` rows at [`observability.md:129-130`](../../docs/src/observability.md) now cover batch RPC too) — no annotation change, metric is already spec'd. T50 extends `r[obs.metric.store]` table with `tenant_key_lookup_failed_total` — doc-side addition, enhancement not spec-mandated. T52 serves `r[obs.tracing.span-structure]` implicitly (span-context disambiguation) — no annotation change.

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
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T9: OBE — superseded by P0362 (4 copies exist + scheduling.nix variant; see P0362)"},
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
  {"path": "rio-gateway/tests/functional/mod.rs", "action": "MODIFY", "note": "T20: comment at :127 documenting JoinHandle-detach-on-? edge (not fixing — test-only, rare, process-exit reaps)"},
  {"path": "rio-gateway/tests/golden/corpus/README.md", "action": "MODIFY", "note": "T21: delete broken nix-hash|grep pipeline :22-23, keep only nix-eval form"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T22: header :2-4 — add drv_name+mitigations to schema comment, fix match-key semantics (drv_name for VM, test for nextest)"},
  {"path": ".claude/lib/onibus/jsonl.py", "action": "MODIFY", "note": "T23: +read_header(path) helper after :44; remove_jsonl :66-68 uses it"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T23: :380 header list-comp → read_header(KNOWN_FLAKES); add import"},
  {"path": ".claude/dag.jsonl", "action": "MODIFY", "note": "T24: s/P992247601/P0317/g in note fields at :295 and :304 (2 occurrences)"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T25: :449-451 Failed{status:TimedOut} -> Failed with error_summary; T27: delete blank lines at :254 :258 :262 (tracey parse fix, work bottom-up)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T26: insert rio_scheduler_build_timeouts_total row after :114; update cancel_signals_total trigger list at :116 (+per-build timeout)"},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T28: add no-leading-zero JSON guidance after :125 (deps are bare ints: 318 not 0318)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T29: tracey-validate src :401 cleanSource -> fileset.difference excluding ./.claude; makes CLAUSE-4(a) hash-identity premise TRUE"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T30: _rewrite_and_rename second-pass — glob .claude/work/*.md for placeholder refs in batch-append targets; fix :334-343 'by construction' claim"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T30: test_rewrite_scans_batch_append_targets — regression for docs-933421"},
  {"path": ".config/tracey/config.styx", "action": "MODIFY", "note": "T31: test_include +4 globs (scheduler/store/worker/controller tests) — 13 invisible r[verify] annotations"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "T32: tag :32 TODO prose with (P0335) — P0335 owns the select!-based fix"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T33: :176 cosmetic TODO → deferred (no owner, no gap, just UX hypothetical)"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T34: emit_progress loop after reassign_derivations in handle_worker_disconnected :72-74 (HOT count=27, additive insert)"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T34: emit_progress before return false at assignment-send-fail :459 (HOT count=22, additive insert)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T34: emit_progress after push_ready in handle_infrastructure_failure :739 (HOT count=25, additive insert)"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T34: extend WorkerDisconnected test :186 — assert BuildProgress event post-disconnect"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T35: extend merger-lock paragraph :47 — task-notification != agent-done; lock-status is authoritative"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T36: :462 validate_dag STDERR_ERROR→STDERR_LAST (remediation-07); :466 r[gw.reject.nochroot] split path semantics (HOT count=27, pure text)"},
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T37: :302 .unwrap() -> .expect('non-empty: checked at :197'); T38: +result=exists counter at :258, +duration histogram timer at handler entry"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T39: content-verification 4-check discipline after T35's lock-stomp paragraph (~:50)"},
  {"path": ".claude/notes/bughunter-log.md", "action": "NEW", "note": "T40: mc=77 + mc=84 null-result entries (audit counts: mc77 55 unwrap, mc84 38 unwrap/3 non-test)"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "T42: :451 stale find_missing_chunks ref → find_missing_chunks_for_tenant OR RETURNING clause OR drop. T41 OBE (fixed by 76ba3999)"},
  {"path": ".claude/work/plan-0273-envoy-sidecar-grpc-web.md", "action": "MODIFY", "note": "T43: :279 tighten 0x80 trailer grep to '80 00 00 00' frame prefix"},
  {"path": ".claude/dag.jsonl", "action": "MODIFY", "note": "T44: P0273 row compact separators (cosmetic)"},
  {"path": ".claude/notes/ci-gate-fastpath-precedent.md", "action": "MODIFY", "note": "T45: add 'onibus merge count-bump' after amend in mechanical-steps block :32-39"},
  {"path": ".claude/lib/onibus/collisions.py", "action": "MODIFY", "note": "T46: +_MIGRATION_NUM regex, migration-number collision key (same-NNN different-name catch)"},
  {"path": "rio-scheduler/tests/metrics_registered.rs", "action": "MODIFY", "note": "T47: +ema_proactive_updates_total to SCHEDULER_METRICS const :23 (p266 ref)"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T48: :202-206 doc-comment — 'key=tenant-foo-1' → 'which branch fired' (bool doesn't carry key name)"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T49: maybe_sign PG-fallback .expect('infallible') → signer.cluster().sign() (p338 ref); T50: +tenant_key_lookup_failed_total counter at warn! (p338 ref)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T50: +rio_store_tenant_key_lookup_failed_total row in r[obs.metric.store] table"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T51: enqueue doc-comment 'actually enqueued' → 'attempted' (p339 ref)"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T52: +#[instrument(skip(pool, chunk_backend))] on sweep_orphan_chunks :263"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T53: :698 TODO(P0260) self-ref → re-tag P0349 or delete (p260 ref)"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "T54: NIXBASE32 const → pub use rio_nix::store_path::nixbase32::CHARS as NIXBASE32 (p337 ref :23); T60: +seed_store_output helper (unify inputs.rs:411 + upload.rs:926)"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T55: +.owns::<Job>() at :309 for reactive ephemeral re-spawn (p296 ref)"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T56: ephemeral precondition assert reorder ~:1780 — jobs_before>=1 BEFORE build() (p296 ref)"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "T57: :41 split Err(_)→default from Ok(other)→panic! with allowed-values hint (p300 ref); T58: +variant_skip_names_are_real unit test :59"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "T59: prepare_nix_state_dirs(upper)→(synth_db: &Path) at :322 — caller passes .upper_synth_db() (p333 ref)"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "T59: :442 caller passes overlay_mount.upper_synth_db() (p333 ref)"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "T60: seed_output :411 → use rio_test_support::seed_store_output (p333 ref)"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T60: make_output_file :926 → use rio_test_support::seed_store_output (p333 ref)"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T61: task-id a-vs-b discipline paragraph after T35+T39's lock-stomp/4-check (~:50)"},
  {"path": "rio-store/tests/grpc/chunked.rs", "action": "MODIFY", "note": "T62: :412,437-445,466 stale put_path_batch.rs line-refs +6 drift post-P0342"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "T63: delete :78-80 redundant tonic-health dev-dep (normal dep at :38 covers)"},
  {"path": ".claude/lib/onibus/git_ops.py", "action": "MODIFY", "note": "T64: _PHANTOM_AMEND_FILES drop dead merge-shas.jsonl :173 + docstring :189; T65: drop truthiness guard :220"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T64: phantom_amend description :480 — drop misleading merge-shas.jsonl mention"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T65: +test_behind_check_phantom_amend_identical_tree after :1600 (no-op amend empty-diff detection)"},
  {"path": ".claude/skills/dag-run/SKILL.md", "action": "MODIFY", "note": "T66: lock-released-without-notification = merger-still-running paragraph (extends T39 4-check)"},
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T67: :160/:363/:389 line-cite drift — update OR fn-name-anchor (post-P0344-merge)"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T68: :78 globalTimeout comment — 'WorkerPool patch+STS rollout'→'k3s bring-up+3 builds' (post-P0309-merge)"},
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "MODIFY", "note": "T69: :542-545 inline base64+newline → encode_pubkey_file call (post-P0349-merge)"},
  {"path": "rio-gateway/src/ratelimit.rs", "action": "MODIFY", "note": "T70: :8 TODO(phase5) → remove (eviction moot, sub-keying bounded) or re-tag TODO(P0261) (post-P0213-merge)"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T71: :575 ensure_permit format — drop literal 'max' placeholder OR store+display real cap (post-P0213-merge)"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T72: :114 ResourceExhausted docstring — split pool-timeout (retriable) from sig-cap (per-path permanent) (post-P0213-merge)"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T73: 12 blank-line-after-marker deletions (P0320 defect class — e.g. store.chunk.put-standalone, store.chunk.grace-ttl)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55 — awk scan at dispatch)"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55)"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55)"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T73: blank-line-after-marker sweep (subset of 55)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T74: :807-814 mutants || true → || { rc=$?; [ $rc -eq 2 ] || exit $rc; }; T76: extract goldenTestEnv attrset, // at nextest/coverage/mutants call sites"},
  {"path": "justfile", "action": "MODIFY", "note": "T75: :27-28 $() → $$() for just-shell-escape"},
  {"path": "nix/golden-matrix.nix", "action": "MODIFY", "note": "T76: accept goldenTestEnv arg, // at mkMatrixRun :63-66"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T77: insert_build +jti param, INSERT +jwt_jti column, bind(jti) at :382-398 (HOT count=40 — 7-line additive)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T77: pass jwt_claims.as_ref().map(|c| c.jti.as_str()) to insert_build caller (HOT count=34 — single arg add)"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T77: SubmitBuild integration test asserts jwt_jti readback"},
  {"path": ".config/mutants.toml", "action": "MODIFY", "note": "T78: +commented expansion candidates after :29"},
  {"path": "infra/helm/rio-build/templates/device-plugin.yaml", "action": "MODIFY", "note": "T80: +livenessProbe exec socket-check (post-P0286-merge; review cited :73)"},
  {"path": "rio-gateway/src/quota.rs", "action": "MODIFY", "note": "T81: :200-201 'rounds up' → 'rounds to nearest' doc fix; T82: :136-141 warn! code+message → error=%status convention"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T83: GATEWAY_METRICS const +jwt_mint_degraded_total +quota_rejections_total (:6-15)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T83: +rio_gateway_jwt_mint_degraded_total + _quota_rejections_total rows to Gateway Metrics table if missing"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T84: apply_trailer :198 Result<[u8;32],Status>→Result<(),Status>; drop 'Returns the applied hash' docstring (P0345 landed, both callers if-let-Err)"},
  {"path": "rio-store/src/migrations.rs", "action": "MODIFY", "note": "T85: M_018 :56-58 'PutChunk wraps in BEGIN..COMMIT'→autocommit sequential-commit honest note (chunk.rs:262 contradicts; P0353 inherited .sql's original error)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T86: helm-lint :498-573 /tmp/jwt-*.yaml → $TMPDIR/jwt-*.yaml (convention consistency, P0357's yq asserts); T88: covArgs src fileset investigate + align with commonArgs.src if divergent (E0583 migrations mod); T89: sqlx-prepare-check files regex extend to .sql + reminder branch (post-P0297)"},
  {"path": "rio-test-support/src/metrics.rs", "action": "MODIFY", "note": "T87: DescribedByType :93-100 counters()/gauges() — delete dead accessors (0 callers) + shrink NamesByType to Vec<String> if Recorder impl simplifies"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T90: +NOTE(fault-line) comment above validate_put_metadata :124 — extraction threshold (3rd PUT RPC OR 1000L OR collision=20+)"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T91: delete sign_for_tenant :224-242; migrate r[impl store.tenant.sign-key] to resolve_once; migrate 3 tests at :561,:605,:638 to resolve_once().sign()"},
  {"path": "rio-common/src/tenant.rs", "action": "MODIFY", "note": "T92: fix :41 doc-comment OR drop #[error] attr — EmptyTenantName Display zero formatters"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T92: :727 .map_err(|_|...) → .map_err(|e| ...e.to_string()) if option-(a)"},
  {"path": "infra/helm/rio-build/templates/_helpers.tpl", "action": "MODIFY", "note": "T94: :45-64 wrap rio.tls{Env,VolumeMount,Volume} in if .Values.tls.enabled (match jwt/cov pattern)"},
  {"path": "infra/helm/rio-build/templates/controller.yaml", "action": "MODIFY", "note": "T94: drop caller-side if .Values.tls.enabled guards (helper self-guards now)"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "T94: drop caller-side tls.enabled guards"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "T94: drop caller-side tls.enabled guards"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "T94: drop caller-side tls.enabled guards"},
  {"path": "infra/helm/rio-build/templates/worker.yaml", "action": "MODIFY", "note": "T94: drop caller-side tls.enabled guards (check at dispatch — may not have tls section)"},
  {"path": "rio-scheduler/src/rebalancer.rs", "action": "MODIFY", "note": "T95: +Deserialize on RebalancerConfig :44; spawn_task sig +cfg param (drop ::default() at :326); T96: +durations.is_empty() guard before :144"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T95: thread cfg.rebalancer to actor spawn; T96: anyhow::ensure!(cfg.rebalancer.min_samples >= 1) alongside :295 cutoff-NaN check"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T95: spawn_task callsite :357 — pass cfg.rebalancer instead of inline default"},
  {"path": "rio-crds/src/workerpool.rs", "action": "MODIFY", "note": "T98: :137 max_concurrent_builds + :183 systems + :401 Replicas — bare-string CEL → Rule::new().message(); extend cel_rules_in_schema test :516+"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "T98: regen — three new x-kubernetes-validations message fields"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T99: +pub(crate) const POOL_LABEL; :394 format! → POOL_LABEL"},
  {"path": "rio-controller/src/reconcilers/workerpool/disruption.rs", "action": "MODIFY", "note": "T99: :48 const → use super::POOL_LABEL; T101: +disruption_drains_total counter after drain_worker call ~:125"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T99: :59 literal → POOL_LABEL"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T99: :126 format! literal → POOL_LABEL; :478 test assert key → POOL_LABEL"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T99: :627 selector.get literal → POOL_LABEL"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T100: :475 timeout=180→300, :474 --timeout=150s→270s, :390 drop '180 should suffice' aspirational"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T101: CONTROLLER_METRICS const +rio_controller_disruption_drains_total"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T101: r[obs.metric.controller] table +disruption_drains_total row after :194 gc_runs_total"},
  {"path": "rio-controller/src/lib.rs", "action": "MODIFY", "note": "T102: describe_histogram/counter reconciler=workerpool → workerpool|workerpoolset at :72,:78"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T103: child_name +RFC-1123 63-char guard, return Result<String>; caller at :140 gets ?"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "MODIFY", "note": "T103: child_name(&wps,class) → child_name(&wps,class)? at :162"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway-policy.yaml", "action": "MODIFY", "note": "T104: streamIdleTimeout: 1h → {{ .Values.dashboard.streamIdleTimeout | default 1h }}; T105: :10-12 +TODO(P0371)"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T104: +dashboard.streamIdleTimeout: 1h in dashboard: block :379+"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway.yaml", "action": "MODIFY", "note": "T105: :77 +TODO(P0371) on phase-5b-authz comment"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T106: +DEFAULT_GC_RETENTION_HOURS: i32 = 168 const near :339; reference in :351 COALESCE or .or(Some(..))"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T107: parse_build_id :176-179 — include error detail {e} in Status msg"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T107: :388 inline parse → crate::grpc::SchedulerGrpc::parse_build_id()"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T108: +ASSIGNMENT_TOKEN_HEADER const after :43 alongside TRACE_ID_HEADER"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T108: :158 .get literal → ASSIGNMENT_TOKEN_HEADER; :213 error-msg literal → const interpolation"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T108: :337,:834 insert literal → ASSIGNMENT_TOKEN_HEADER"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T108: :372 x-rio-tenant-token literal → rio_common::jwt_interceptor::TENANT_TOKEN_HEADER"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T109: +grpcurl_json_stream helper after store_grpc :379; migrate 6 raw_decode sites :434/:1198/:1492/:1715/:1864/:1912"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T109: migrate :175 raw_decode site → grpcurl_json_stream (inline helper copy or import pattern)"},
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T110: +Clause-4 fast-path decision tree section with 5-tier ladder + proof-recipes table"},
  {"path": "rio-test-support/src/pg.rs", "action": "MODIFY", "note": "T111: +with_ed25519_key/+with_key_name builder methods; ed25519_seed/key_name fields; tenant_keys INSERT in seed(); COALESCE sibling-note pointing at T106 const"},
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T111: migrate 2+ INSERT INTO tenant_keys inline → TenantSeed.with_ed25519_key (post-P0340)"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T112: comment-code mismatch fix at scale_wps_class fallback :529-539 (p234 ref); T114: child_name format! dup → shared fn at :508 (p234 ref); T121: ssa_envelope migration sites"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T113: :86-91 'P0234 adds' future-tense → present/delete; T114: child_name pub(super)→pub(crate) or re-export"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "T115: +info! log after :268 bloom_expected_items unwrap"},
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T116: :6 export let → $props() Svelte-5 syntax"},
  {"path": "rio-dashboard/src/pages/Cluster.svelte", "action": "MODIFY", "note": "T117: :30-32 setInterval +document.hidden pause +backoff-on-error"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T118: :473 nginx override withXslt=false (drop libxml2+libxslt ~1.8MiB)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T119: devicePlugin image-tag lockstep assert (values.yaml vs k3s-full.nix airgap-set)"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T120: +2 never-built VM entries (consol-mc150 backlog-grew finding)"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T121: +ssa_envelope<T> helper — centralize apiVersion+kind SSA body"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T121: :371 ssa json! → ssa_envelope"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T121: :234 ssa json! → ssa_envelope"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "MODIFY", "note": "T121: :256 (p234 ref) ssa json! → ssa_envelope"},
  {"path": "rio-dashboard/src/components/DrainButton.svelte", "action": "MODIFY", "note": "T122: drop busy state OR set before optimistic mutation (post-P0281 + P0377)"},
  {"path": "rio-scheduler/src/grpc/worker_service.rs", "action": "MODIFY", "note": "T123: :198 tokio::spawn → spawn_monitored('ema-proactive-write',...); :183 comment update"},
  {"path": "rio-dashboard/src/pages/Workers.svelte", "action": "MODIFY", "note": "T124: :86-88 status pill +icon-prefix or aria-label (a11y, post-P0281)"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "MODIFY", "note": "T125: :86-97 nav.tabs +role=tablist, buttons +role=tab aria-selected, section +role=tabpanel (a11y, post-P0278)"},
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T126: :172 tr[onclick] +tabindex=0 +onkeydown Enter +role=button (a11y, post-P0278)"},
  {"path": "rio-dashboard/src/lib/build.ts", "action": "NEW", "note": "T127: buildProgress(BuildInfo) extraction from Builds.svelte:94-98 + BuildDrawer.svelte:21-25 dup (post-P0278)"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T128: :1030 find_wps_child guard is_wps_owned→is_wps_owned_by(child,wps); :1009 comment update; +find_wps_child_rejects_wrong_wps_uid test after :1630 (post-P0374)"},
  {"path": "infra/helm/rio-build/templates/NOTES.txt", "action": "MODIFY", "note": "T129: +CORS placeholder warning conditional (create file if absent)"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway.yaml", "action": "MODIFY", "note": "T130: :109 'until dashboard-native authz lands'→'until per-user dashboard auth is scoped (no plan yet)' (post-P0371)"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T130: :413 same rephrase as dashboard-gateway.yaml:109"},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T130: :30 r[dash.auth.method-gate] spec text — same rephrase (spec-text change but not behavior, no tracey bump)"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T131: +design-note doc-comment above main match re: KubeCmd split deferral (post-P0237)"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T132: :177 extraImagesBumpGiB +dashboard image conditional; :175 comment update"},
  {"path": "rio-crds/src/workerpoolset.rs", "action": "MODIFY", "note": "T133: +pub fn wps_child_name(wps_name,class_name)→String (hoisted from builders.rs:43; RFC-1123 guard from T103 moves WITH it; post-P0237+T103)"},
  {"path": "rio-cli/src/wps.rs", "action": "MODIFY", "note": "T133: :114+:147 inline format!→rio_crds::wps_child_name (post-P0237)"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T134: option-(a) preferred = no change here (migration is at caller sites); option-(b) = extend :85 doc-comment re: opportunistic migration (post-P0376)"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T135: +mc168 null-result entry; T5 drift folded into P0383"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T136: envoy image tag lockstep assert adjacent to T119's devicePlugin assert in helm-lint block (post-P0379)"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "T137: :146 'c ? last' → '(c.last or false)' truthiness-not-presence; +comment :145"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T138: +pub mod admin_types re-export after :109 (parity with dag/build_types; post-P0376)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T139: +idempotency-by-construction comment at :840 ON CONFLICT is_ca (bughunt-verified; HOT count~40, comment-only)"},
  {"path": "rio-scheduler/src/grpc/actor_guards.rs", "action": "MODIFY", "note": "T140+T141: +ACTOR_UNAVAILABLE_MSG const above check_actor_alive; +actor_error_to_status fn hoisted from grpc/mod.rs:128-154 (post-P0383)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T140+T141: :142 ChannelSend literal→const ref; :128-154 actor_error_to_status body→delegate to actor_guards:: (one-line wrapper kept for Self:: callers; HOT count=38)"},
  {"path": "rio-scheduler/src/admin/gc.rs", "action": "MODIFY", "note": "T141: :80 SchedulerGrpc::actor_error_to_status → actor_guards:: (path shortening; post-P0383)"},
  {"path": "rio-scheduler/src/admin/sizeclass.rs", "action": "MODIFY", "note": "T141: :39 SchedulerGrpc::actor_error_to_status → actor_guards:: (post-P0383)"},
  {"path": "rio-scheduler/src/admin/workers.rs", "action": "MODIFY", "note": "T141: :30 SchedulerGrpc::actor_error_to_status → actor_guards:: (post-P0383)"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T141: :177,:299,:324 SchedulerGrpc::actor_error_to_status → actor_guards:: (count=21; post-P0383)"},
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "T142: +test_workerpoolset_spec + test_workerpoolset (P0380 sibling one-table-up; post-P0380)"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T142: :1420-1447 test_wps body→fixtures delegation; delete :1415-1418 unreachable-comment (HOT count=16; post-P0380)"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T142: :183-223 test_wps_with_classes body→fixtures delegation + optional features override (count=22; post-P0380)"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T143: +mc182 null-result cadence entry (format matches T135's mc168)"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "T144: +is_content_addressed() trait default after :187. T145: drop builder()/args()/input_srcs() from trait required-methods (narrow to outputs+env+platform). T146: DerivationOutput::is_fixed_output→has_hash_algo rename + simplify :118-129 doc-comment (post-P0384)"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T144: :390+:428 disjunction → drv.is_content_addressed() (or single site if P0388 landed first; HOT count=24)"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T147: +mc189 null-result cadence entry (third consecutive null: mc175→mc182→mc189)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T148: :309-333 ca_hash_compares match — split outcome into match/miss/error (was match/miss collapsing errors; HOT count=26; post-P0251)"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T148: describe_counter! for rio_scheduler_ca_hash_compares_total — three-outcome doc update"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T149: GATEWAY_METRICS +3 omitted entries (quota_rejections/auth_degraded/jwt_mint_degraded). STOPGAP — P0394 supersedes"},
  {"path": "rio-gateway/src/quota.rs", "action": "MODIFY", "note": "T150: :91 from_maybe_empty → normalize_key_comment (stale doc-comment ref post-P0367)"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T151: :101 soften 'provably unreachable' (Unicode whitespace vs POSIX [[:space:]] gap)"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T152: +pub(crate) const REASON_SPEC_DEGRADED near existing MANAGER/FINALIZER consts; :255 inline → const ref"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T152: :1648,:1709 assert → REASON_SPEC_DEGRADED const; :1081 event_post_scenario reason→body_substr rename. Soft-conflict P0396 (split moves to tests/disruption_tests.rs)"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T153: :814-819 RIO_TEST_PREFETCH_DELAY_MS → +TODO(P0311-T58) tag (or remove if option-a)"},
  {"path": "rio-dashboard/src/lib/logStream.svelte.ts", "action": "MODIFY", "note": "T154: :42 reconnect-on-transient-error comment → +TODO(P0NNN) tag; T155: :31,:45 drvPath param → +TODO(P0280) (post-P0279-merge)"},
  {"path": "rio-dashboard/src/components/LogViewer.svelte", "action": "MODIFY", "note": "T155: :12-13 drvPath prop → +TODO(P0280) tag (post-P0279-merge)"},
  {"path": "rio-scheduler/src/admin/tests/gc_tests.rs", "action": "MODIFY", "note": "T156: :7 redundant use tokio_stream::StreamExt — delete (post-P0386-merge — DONE)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T157: :295-304 32-byte guard → +counter increment outcome=malformed (partners with T148's outcome=error)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T157: Scheduler Metrics table ca_hash_compares_total row +malformed label (coordinate with P0295-T91 + T148 + P0393)"},
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "MODIFY", "note": "T158: :448-579 serialize_resolved+write_aterm_string dedup→rio-nix to_aterm_resolved (~110L delete). T159: :150-178 Result<String>→String. T161: :247-275 query_realisation loop→UNNEST batch SELECT (perf; ahead-of-time until P0254-T6). discovered_from=253"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "T158: +pub fn to_aterm_resolved(drop_input_drvs,extra_input_srcs) reusing write_aterm_string+to_aterm sections (~30L add). Soft-conflict P0398 (if that lands first, no drop_input_drvs param — always-empty)"},
  {"path": "rio-scheduler/src/ca/mod.rs", "action": "MODIFY", "note": "T160: :18-21 +insert_realisation_deps to pub-use list. discovered_from=253"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T162: +mc196 entry — self-match FINDING (→P0397) + smell counts. T163: +mc203 entry — all 5 coord-target verdicts CLEAN"},
  {"path": ".config/mutants.toml", "action": "MODIFY", "note": "T164: :59-77 delete :87/:158 pad-guard exclude_re (guards deleted from source); note on :229/:230 drift-toil. discovered_from=373"},
  {"path": "rio-nix/src/protocol/wire/mod.rs", "action": "MODIFY", "note": "T164: :87 + :158 delete `if pad > 0` guard (0-byte slice no-op). T166: :906 +drop(at_max); :762 Vec→tokio::io::sink(). discovered_from=373"},
  {"path": "rio-nix/proptest-regressions/derivation/aterm.txt", "action": "NEW", "note": "T165: commit 4 seeds (route-a) OR delete + gitignore (route-b — check crane fileset at dispatch). discovered_from=373"},
  {"path": "rio-nix/proptest-regressions/protocol/wire/mod.txt", "action": "NEW", "note": "T165: commit 3 seeds (route-a) OR delete + gitignore (route-b). discovered_from=373"},
  {"path": "rio-dashboard/src/lib/__tests__/logStream.test.ts", "action": "MODIFY", "note": "T167: migrate vi.hoisted/inline-mock → adminMock + flushMicrotask helper (post-P0389-merge). discovered_from=389"},
  {"path": "rio-dashboard/src/__tests__/App.test.ts", "action": "MODIFY", "note": "T167: migrate to adminMock (one-depth path variant; post-P0389-merge). discovered_from=389"},
  {"path": "rio-dashboard/src/test-support/admin-mock.ts", "action": "MODIFY", "note": "T168: :54 getBuildGraph → vi.fn().mockResolvedValue({nodes:[],edges:[],truncated:false,totalNodes:0}) + consider same pattern for listWorkers/listBuilds/clusterStatus (post-P0389-merge). discovered_from=389"},
  {"path": "rio-dashboard/src/test-support/flush.ts", "action": "NEW", "note": "T167: flushMicrotask(rounds) helper for generator-drain (shared w/ GC.test.ts:50-53 tick+Promise.resolve loop; distinct from flushSvelte). discovered_from=389"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T169: :14-21 MAX_CASCADE_DEPTH→MAX_CASCADE_NODES rename + SCOPE-+2 post-P0405 :635,:646 refs. discovered_from=252+405. Soft-conflict P0399-T1 at :411 (non-overlapping)"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T169: describe_counter depth_cap_hits_total → clarify 'node-count cap, not tree-depth' in doc-string. discovered_from=252"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T169 SCOPE-+2: :68,:84,:511 MAX_CASCADE_DEPTH→NODES refs added by P0405. discovered_from=405. HOT count=31 — const-rename only"},
  {"path": "rio-dashboard/src/lib/graphLayout.ts", "action": "MODIFY", "note": "T170: :118-122 delete/rewrite stale sourcePosition comment (post-P0280-merge). discovered_from=280. Soft-conflict P0400-T1 at :43-83 (non-overlapping)"},
  {"path": ".claude/work/plan-0287-trace-linkage-submitbuild-metadata.md", "action": "MODIFY", "note": "T172: :234 soft_deps 356→356 (stale placeholder ref — P0356 grpc-mod-split). discovered_from=consolidator-mc205"},
  {"path": ".claude/work/plan-0295-doc-rot-batch-sweep.md", "action": "MODIFY", "note": "T172: :500 P0363→P0363 cross-link; :700 plan-0375→plan-0375 table cell; soft_deps fence 356→356. discovered_from=consolidator-mc205"},
  {"path": ".claude/work/plan-0311-test-gap-batch-cli-recovery-dash.md", "action": "MODIFY", "note": "T172: :1175,:1196,:1198,:1453,:2128,:2196,:2248 + soft_deps fence — 363→0363, 374→0374, 361→0361 placeholder rewrites. discovered_from=consolidator-mc205"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T172: THIS DOC soft_deps fence + note body — 353→0353, 362→0362, 374→0374, 375→0375 (QA-caught self-referential). discovered_from=consolidator-mc205"},
  {"path": ".claude/agents/rio-impl-reviewer.md", "action": "MODIFY", "note": "T173: :25,:73 git diff $TGT..→$TGT... (2-dot→3-dot). discovered_from=consolidator-mc205"},
  {"path": ".claude/agents/rio-impl-validator.md", "action": "MODIFY", "note": "T173: :63,:64 git diff $TGT..→$TGT... (2-dot→3-dot). discovered_from=consolidator-mc205"},
  {"path": ".claude/agents/rio-ci-flake-validator.md", "action": "MODIFY", "note": "T173: :15,:29,:30,:40,:41 git diff $TGT..→$TGT... (2-dot→3-dot). discovered_from=consolidator-mc205"},
  {"path": ".claude/agents/rio-plan-reviewer.md", "action": "MODIFY", "note": "T173: :25 git diff $TGT..→$TGT... (2-dot→3-dot). discovered_from=consolidator-mc205"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "T174: T158-ADDENDUM — to_aterm_modulo :349-412 is 3rd copy; T158's to_aterm_resolved generalizes to to_aterm_with(outputs_mask,input_drvs_transform,extra_input_srcs) collapsing all 3. discovered_from=consolidator-mc205"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "T175: :67-71 decode_cursor — version-check BEFORE length-check (enables multi-version dispatch; post-P0271). discovered_from=271"},
  {"path": "rio-proto/proto/admin_types.proto", "action": "MODIFY", "note": "T176: :60 strip cursor encoding leak from comment (clients-treat-opaque; post-P0271+P0376). discovered_from=271"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T177: :539-541 rewrap 'doesn't / buffer' comment (P0295-T65 edit artifact; post-P0295). discovered_from=295"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T178: :202 CoreDNS comment — template envoyGatewayNamespace to match :216 (P0295-T69 partial extraction; post-P0295). discovered_from=295"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "T179: +DeployConfig bundle newtype (size_classes+poison+retry); spawn_with_leader 10→8 args. discovered_from=307. Post-P0307-merge"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T180: POISON_TTL const → PoisonConfig.ttl field; cfg(test) 100ms moves to test-fixture. discovered_from=307. Post-P0307-merge. Low-priority — may skip"},
  {"path": "rio-dashboard/src/pages/Graph.svelte", "action": "MODIFY", "note": "T181: :177-182 allTerminal guard — add !r.truncated check OR soften comment. discovered_from=400. Post-P0400-merge"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/mod.rs", "action": "MODIFY", "note": "T182a: :33,:40,:60 pub(super)→pub(crate). discovered_from=396. Post-P0396-merge"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs", "action": "MODIFY", "note": "T182b: 1013L→~500L (split quantity-parsing out). discovered_from=396. Post-P0396-merge"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/quantity_tests.rs", "action": "NEW", "note": "T182b: extracted quantity/GB-parsing table tests (~500L). discovered_from=396"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T183a: :18 floor >=5→>=6 (zero-margin tighten). discovered_from=394. Post-P0394-merge"},
  {"path": "rio-test-support/src/metrics_grep.rs", "action": "MODIFY", "note": "T183b: :113 cargo:warning gate on CI||SPEC_METRICS_STRICT env. discovered_from=394. Post-P0394-merge"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T184: verify second nonpriv profile (if any) uses derived devicePlugin.image. discovered_from=387. Post-P0387-merge. Verify-at-dispatch"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T184: :53-59 'nonpriv values file overrides to' comment stale post-P0387-T3 — may route to P0295 if comment-only. discovered_from=387"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T185: +envoyGatewayAppVersion let-binding + assertMsg lockstep (route-c no-IFD) near T136's envoy-distroless assert. discovered_from=387"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T186: :79-82 envoy-gateway operator — add comment explaining no-helm-lint-assert asymmetry (gateway-helm uses bare tags, no chart-side digest to compare). discovered_from=387"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T187: +rio_scheduler_ca_resolve_degraded_total counter at 3 warn! degrade paths (P0408's store-fetch-failed, :751 resolve-error, :790 modular-hash-unset). discovered_from=408"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T187: +describe_counter! for ca_resolve_degraded_total"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T187: +Scheduler Metrics table row for ca_resolve_degraded_total (reason label)"},
  {"path": "rio-dashboard/src/pages/__tests__/Workers.test.ts", "action": "MODIFY", "note": "T188: :23 ageMs comment — verify-and-update post-P0406 extraction (fmtTsRel/tsToMs or keep if thin wrapper survives). discovered_from=406"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T189: move size_classes cutoff_secs ensure from :347-358 inline → into validate_config() after :221. Keep :359-360 gauge-emit + :362-367 info! inline (pure fn). Optional +config_rejects_nan_cutoff test. discovered_from=409"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T190: +mc224 null-result cadence entry (dd7f370d..3d2f7f20, mc217-224, 5 target-verdicts all CLEAN). discovered_from=bughunter-mc224"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T191: :217-223 update-ref fallback +stderr print + ref_forced tracking. discovered_from=414. Soft-conflict P0417-T3 at :201-206 (non-overlapping)"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T191: DagFlipResult +ref_forced bool field at :488-509. discovered_from=414. Soft-conflict P0417-T5 (amend_sha docstring edit, rebase-clean)"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "T192: :513-515 delete find_cutoff_eligible thin wrapper (0 callers post-P0405); :17 + :517 doc-comment rewrites. discovered_from=405. T169 SCOPE-+2: :635,:646 MAX_CASCADE rename targets added by P0405. Soft-conflict T169 (:14-21), P0405 (:567-646 — all DONE)"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T192: :77,:363 doc-comment rewrite (find_cutoff_eligible → _speculative/cascade_cutoff). T169 SCOPE-+2: :68,:84,:511 MAX_CASCADE rename targets added by P0405. discovered_from=405. HOT count=31 — comment-only, zero behavior"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T192: :242 doc-comment rewrite (consumed-by find_cutoff_eligible → cascade_cutoff via _speculative). discovered_from=405. count=20 — comment-only"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T193: +6 entries (T16 core-3 + T32 jwt-mount-present + T37 vm-security-nonpriv + vm-ca-cutoff-standalone P0405-refactor-target). SUPERSEDES T120. discovered_from=311"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T194: 6× line-cite → fn-name/code-shape cite (citation-tiering; P0405 rebase drift). Soft-conflict P0419 (:332-337 doc-comment + :384-389 outer guard — different doc-comments, non-overlapping). discovered_from=311"},
  {"path": ".config/nextest.toml", "action": "MODIFY", "note": "T195: :73 test(/^tests::/) → test(/tests::/) (drop anchor, match actor::tests::). discovered_from=311"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T196+T197: banner line-cite → fn-name cite (post-P0412); banner ~10L → 1-line pointer to rio-common module-doc. discovered_from=412"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T196+T197: same as rio-store (post-P0412). discovered_from=412"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T196+T197: same as rio-store (post-P0412). discovered_from=412"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "T196+T197: same as rio-store (post-P0412). discovered_from=412"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T196: :1021 store-cite → fn-name. discovered_from=412. HOT count=38 — comment-only"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "T197+T198: module-doc +Standing-guard rationale +Known-limitation (ADD-IT-HERE advisory) paragraphs. discovered_from=412. Low-traffic"},
  {"path": "rio-dashboard/eslint.config.js", "action": "MODIFY", "note": "T199: :39 **/gen/*_pb → **/gen/**/*_pb (both patterns). discovered_from=407. Low-traffic"},
  {"path": "rio-scheduler/src/actor/tests/build.rs", "action": "MODIFY", "note": "T200: :277 stale db.rs cite → db/recovery.rs fn-name anchor. discovered_from=411"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T200: :995 stale db.rs:537 → TERMINAL_STATUS_SQL const cite. discovered_from=411. HOT count=32 — comment-only"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T201: :664 db.rs → db/ post-P0411. discovered_from=411. count=24 — single-line doc fix"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "T202: +pub(crate) connect_worker_no_ack (moved from worker.rs:819); connect_worker delegates. discovered_from=consolidator. Soft-conflict P0422 (CaFixture helper adds after :26; T202 adds after :86 — non-overlapping)"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T202: delete connect_worker_no_ack :819-845 (moved to helpers.rs). discovered_from=consolidator"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T203: :76,:114,:154 raw subprocess.run→git() OR git_try(). T204: already-done scan loop — drop break OR reversed-iter (last-match). discovered_from=consolidator+rev-p417. Soft-conflict P0417(DONE)+P0418+P0420+P0421 — all merge.py, non-overlapping hunks"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T205: validate_config +3 ensures (scale_up/scale_down/min_interval _secs > 0). discovered_from=416. HOT count=26 — additive ensures"},
  {"path": "rio-scheduler/src/db/mod.rs", "action": "MODIFY", "note": "T206: LOW-PRIORITY row-struct domain-move (9 structs → tenants.rs/builds.rs/assignments.rs/history.rs/batch.rs/recovery.rs/derivations.rs + pub-use re-exports). discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/tenants.rs", "action": "MODIFY", "note": "T206: +TenantRow struct (moved from mod.rs). discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/builds.rs", "action": "MODIFY", "note": "T206: +BuildListRow + LIST_BUILDS_SELECT. discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/assignments.rs", "action": "MODIFY", "note": "T206: +AssignmentStatus enum. discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/history.rs", "action": "MODIFY", "note": "T206: +BuildHistoryRow + EMA_ALPHA const. discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/batch.rs", "action": "MODIFY", "note": "T206: +DerivationRow. discovered_from=consolidator"},
  {"path": "rio-scheduler/src/db/recovery.rs", "action": "MODIFY", "note": "T206: +RecoveryBuildRow/RecoveryDerivationRow/GraphNodeRow/GraphEdgeRow (moved). T207: NOTE(fault-line) comment at load_build_graph + read_event_log (cohesion — not-really-recovery fns). discovered_from=consolidator+rev-p411"},
  {"path": "rio-scheduler/src/db/derivations.rs", "action": "MODIFY", "note": "T206: +PoisonedDerivationRow. discovered_from=consolidator"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T208: check-mutants-marker hook — +version-pin comment (cargo-mutants 26.2.0 MUTATION_MARKER_COMMENT anchor). discovered_from=402. HOT count=38 — comment-only adjacent to hook"},
  {"path": ".claude/notes/bughunter-log.md", "action": "MODIFY", "note": "T209: +mc235-238 verdict archive (P0412 proc-macro NOT WORTH; consol-mc236 5-target verdict; bughunt-mc238 cpu_limit_cores finding→P0424; P0414-chain complete; rev-p411 plan-doc-accuracy non-actionable). discovered_from=consolidator+bughunter+coordinator+rev-p411"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T210: _cadence_range + CadenceReport builder — preemptive flag + open-ended range at N-5. T211: lock_status :112-119 — ff_landed via merge-base --is-ancestor plan-branch. discovered_from=coord. Soft-conflict P427 (chain-merger adds lock_renew/queue_next — different fns) + P0418/P0420/P0421 (merge.py, non-overlapping)"},
  {"path": ".claude/lib/onibus/models.py", "action": "MODIFY", "note": "T210: CadenceWindow +preemptive:bool field. discovered_from=coord"},
  {"path": "rio-dashboard/src/lib/logStream.svelte.ts", "action": "MODIFY", "note": "T212: +droppedLines $state + accumulator + getter. discovered_from=392. Post-P0392. Soft-conflict P426-T3 (same cap block — excess var shared)"},
  {"path": "rio-dashboard/src/components/LogViewer.svelte", "action": "MODIFY", "note": "T212: banner renders droppedLines.toLocaleString(). T213: :139 +title={line}. T214: :26-31 viewportOverride @internal or underscore-rename. T215: :138 +viewport-relative-key comment (Option-C). discovered_from=392. Post-P0392. Soft-conflict P426-T1 (LINE_H→lineH — non-overlapping hunks :64/:91 vs :138/:139)"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T216: delete const MAX_PREFETCH_PATHS :12, import from hoisted spot. discovered_from=391. Post-P0391"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T216: delete const MAX_PREFETCH_PATHS :597, import from hoisted spot. discovered_from=391. Post-P0391"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T216: +pub(crate) const MAX_PREFETCH_PATHS = 100 (hoisted, single source). discovered_from=391"},
  {"path": "rio-test-support/src/metrics.rs", "action": "MODIFY", "note": "T217: +GaugeValues struct + Recorder impl (gauge-set capture, f64::to_bits roundtrip). discovered_from=366. Post-P0366"},
  {"path": "rio-scheduler/src/rebalancer/tests.rs", "action": "MODIFY", "note": "T217: delete local GaugeValues :289-330, use rio_test_support::metrics::GaugeValues. discovered_from=366. Post-P0366"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T218: :1232 assert msg — restore out[-500:] debug-tail. discovered_from=314"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T218: :216+:352 — restore print(output) or tail in assert msg. discovered_from=314"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T219: :1804,1806 — '-A dep' shell-inject → attr='dep' kwarg. discovered_from=314"},
  {"path": "nix/tests/scenarios/ca-cutoff.nix", "action": "MODIFY", "note": "T220: :51-66 build_ca_chain → mkBuildHelperV2 + extra_args (6th scenario absorb). discovered_from=314"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T221: :68 comment :128→:131 cite fix. discovered_from=424"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T221: :263 comment :128→:131 cite fix. discovered_from=424. HOT count=41"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T222: :363-366 drop stale grpc_timeout(3s), use setup_ca_fixture. discovered_from=393. Soft-dep P0422. HOT count=32"},
  {"path": "rio-crds/src/workerpool.rs", "action": "MODIFY", "note": "T223: :436-437 SeccompProfileKind bare-string→Rule::new().message(). discovered_from=304"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T224: :489 untagged TODO timeout=600 — tag or reduce to 300. discovered_from=304"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "T225: :573 drop dead ON CONFLICT DO NOTHING (pending_s3_deletes has no unique constraint). Update :962 comment to past-tense. discovered_from=sprint-1-cleanup"}
]
```

**Outside Files-fence validator pattern (until T1's regex extension lands):** `clippy.toml` MODIFY at repo root — T6 adds OsStr/Path to_string_lossy entries. `.github/workflows/weekly.yml` MODIFY — T79 adds `if: always() && !cancelled()` after `:56` (mutants job decoupled from golden-matrix upstream-nix failures). `CLAUDE.md` MODIFY at repo root — T97 fixes the `:192` same-line-trailing `r[verify]` example (col-0 only). T1's regex fix makes these fence-compatible — re-check at dispatch.

```
.claude/lib/
├── onibus/models.py            # T1: regex extension at :322
└── plan-doc-skeleton.md        # T1: prose sync
infra/helm/grafana/
├── scheduler.json              # T2: sum(rate(...)) wraps
├── worker-utilization.json     # T4: drop no-op matchers
└── configmap.yaml              # T3: regen (GENERATED header)
justfile                        # T3: grafana-configmap target
docs/src/
├── components/scheduler.md     # T25 + T27: spec drift + tracey blank-lines
└── observability.md            # T26: metric row + cancel trigger
.claude/agents/rio-planner.md   # T28: leading-zero guard
flake.nix                       # T29: tracey-validate fileset
```

## Dependencies

```json deps
{"deps": [222, 223, 290, 294, 315, 305, 306, 247, 317, 214, 320, 325, 328, 270, 302, 267, 264, 266, 338, 339, 260, 296, 300, 333, 337, 342, 343, 346, 344, 309, 213, 349, 350, 301, 348, 286, 255, 345, 353, 357, 321, 297, 352, 217, 230, 341, 354, 359, 285, 360, 233, 273, 362, 340, 234, 288, 277, 282, 369, 278, 281, 356, 374, 371, 237, 283, 376, 379, 250, 383, 380, 384, 251, 367, 365, 299, 279, 386, 253, 373, 252, 389, 271, 295, 307, 400, 396, 394, 387, 408, 406, 409, 414, 405, 412, 311, 407, 411, 416, 417, 402], "soft_deps": [303, 216, 289, 313, 206, 207, 316, 318, 322, 335, 326, 332, 353, 358, 362, 298, 366, 370, 287, 374, 375, 377, 381, 378, 388, 393, 280, 398, 254, 399, 403, 410, 415, 418, 419, 420, 421, 422, 425, 392, 391, 426, 427, 423], "note": "T2-T4 depend on P0222 (dashboard files exist — merged). T6 depends on P0290 (clippy.toml exists). T8/T9 depend on P0294 (Build CRD rip — dead variant becomes dead, lifecycle.nix rewritten). T11/T12 depend on P0223 (seccomp CEL rules + profile JSON exist). T14 soft-dep P0206 (lifecycle.nix :1204 copy exists post-P0206; if T14 lands first, extract in k3s-full.nix only and P0206/P0207 use the helper). T15 depends on P0315 (DONE — kvmCheck CREATE_VM probe exists; T15 drops the vestigial GET_API_VERSION it left behind; discovered_from=315). T10 SEQUENCED AFTER P0317 (_VM_FAIL_RE foundation — without it T10's early-return masks co-occurring real failures; re-scoped to supplementary grant per P0317 T7 forward-reference). T16 soft-dep P0316 (DONE — ConnectionResetError is downstream of its -machine accel=kvm gate) + P0317 T4 (if mitigations list migration landed, add as Mitigation entry instead of symptom string). T17 depends on P0306 (DONE — _LEASE_SECS lives in merge.py which P0306 refactored; discovered_from=306). T18 depends on P0305 (DONE — functional/references.rs discard-read sites exist; discovered_from=305). T19 depends on P0305 (same). T20 discovered_from=bughunter mc28 + soft-dep P0318 (same function, comment lands on whichever name is live). T21 depends on P0247 (DONE — corpus README.md exists; discovered_from=247). T22 depends on P0317 (DONE — drv_name+mitigations fields exist; discovered_from=317). T23 depends on P0317 (DONE — cli.py:380 duplication site exists; discovered_from=317); soft-conflict P0322 T2 (also touches cli.py :371-375 — T23 touches :380, non-overlapping but same function). T24 no dep (pure dag.jsonl sed; discovered_from=317, the ref SHOULD have pointed there). T25+T26 depend on P0214 (DONE — worker.rs:570-609 + :593 metric exist; spec/doc never reconciled). T27 depends on P0320 (DONE — same defect class, P0320 fixed :265, T27 fixes :253/:257/:261 siblings from f190e479 seed). T28 no dep (agent-file guidance; session-cached, lands next worktree-add). T29 no dep (flake.nix fileset pattern already established :168-174). Soft-dep P0328 T2: adds describe_counter! for the same metric T26 documents — sequence-independent, both serve one gap. IRONIC SELF-FIX: this fence's soft_deps had 322 and 328 (leading zeros) from prior appends — fixed to 322/328 here per T28's own rule. T30 depends on P0325 (DONE — _rewrite_and_rename mapping-derived touched exists; T30 extends it with batch-append grep pass; discovered_from=coordinator, docs-933421 stale-ref manifestation). Soft: T5 references p216 worktree line numbers (P0216). T9 must land BEFORE P0289 dispatches. T10's KVM-DENIED-BUILDER marker is emitted by P0313 — match BOTH pre/post markers for transition. T1/T13 independent. T31 depends on P0328 (DONE — discovered_from=328): P0328 added metrics_registered.rs to 4 crates with r[verify] annotations that test_include doesn't scan. 7 of the 13 invisible annotations pre-date P0328 (rio-store/tests/grpc/* — landed with P0305 store.put.idempotent verify and earlier). config.styx is low-conflict (single file, append to a glob list). T32 soft-dep P0335 (UNIMPL — the tag POINTS AT IT, not BLOCKED BY IT; P0335 does NOT need to land first. If P0335 lands before T32 it rewrites session.rs:32 entirely and T32 becomes moot — check at dispatch, skip T32 if P0335 already closed it). T33 no dep (cosmetic word removal). T32 SEMANTIC NOTE: P0335 plans to add r[gw.conn.cancel-on-disconnect] and rewrite the cancel path — T32's tag is a BREADCRUMB so anyone reading session.rs before P0335 lands knows where to look. T34 depends on P0270 (DONE — emit_progress and BuildProgress exist; reviewer discovered_from=270). T34 HOT FILES: worker.rs=27 completion.rs=25 dispatch.rs=22 — each edit is 3-6 lines additive at a specific call site, no signature change, low semantic-conflict risk. T35 no dep (SKILL.md session-cached; lands next worktree-add; coordinator self-report discovered_from=262). T36 depends on P0302 (remediation-7 changed validate_dag to STDERR_LAST; doc never reconciled; discovered_from=302). T36 text edit to r[gw.reject.nochroot] paragraph — default NO tracey bump (rejection-happens unchanged, frame-type differs). discovered_from: T14=206, T15=315, T16=316, T17=306, T18=305, T19=305, T21=247, T22=317, T23=317, T24=317, T30=325, T31=328, T32=bughunter, T33=bughunter, T34=270, T35=coordinator(262), T36=302, T37=267, T38=267, T39=coordinator(mc77-80), T40=bughunter(mc77+mc84). T37+T38 depend on P0267 (DONE — put_path_batch.rs exists with the :302 unwrap and the metric gaps). T37+T38 soft-conflict P0342 (fixes :275 ?→bail! same file, non-overlapping :268-275 vs :302+:258+handler-entry); sequence-independent, rebase clean. T39 extends T35's SKILL.md block (same file, adjacent paragraph). T40 creates .claude/notes/bughunter-log.md — path is .claude/notes/, not .claude/work/ per layout convention. T41+T42 depend on P0264 (UNIMPL — migration 18 + renamed fn comments arrive with it; discovered_from=264). T43+T44 soft-dep P0326 (DONE — the P0273 scope change + dag row edit happened there; discovered_from=326). T45 no dep (ci-gate-fastpath-precedent.md exists since P0313 fast-path); bughunter-mc84 REFINED coord's earlier row-6 followup (spec correct, note stale; discovered_from=bughunter). T46 soft-dep P0332 (the 17-collision incident; discovered_from=bughunter-mc84). T47 depends on P0266 (UNIMPL — ema_proactive_updates_total metric arrives with it; discovered_from=266). T48+T49+T50 depend on P0338 (UNIMPL — maybe_sign PG-lookup fallback + cluster() + warn! arrive with it; discovered_from=338). T51+T52 depend on P0339 (UNIMPL — enqueue_chunk_deletes extraction + sweep_orphan_chunks double-caller arrive with it; discovered_from=339). Soft-dep P0346: T46 touches collisions.py, P0346 touches git_ops.py+models.py — same onibus package, non-overlapping files. T53 depends on P0260 (TODO arrives with it; discovered_from=260); soft-dep P0349 (the re-tag TARGET). T54 depends on P0337 (NIXBASE32 const arrives; discovered_from=337). T55+T56 depend on P0296 (ephemeral reconciler + lifecycle subtest arrive; discovered_from=296); T55 soft-conflicts P0347 (both touch ephemeral — T55 is main.rs watcher, P0347 is CEL+deadline, non-overlapping). T57+T58 depend on P0300 (daemon_variant() + VARIANT_SKIP arrive; discovered_from=300). T59+T60 depend on P0333 (DONE — accessors + converged make_output_file exist; discovered_from=333). T61 no dep (SKILL.md session-cached; coord self-report discovered_from=coordinator fabrication #10-11). T62 depends on P0342 (DONE — line-shift +6 happened; discovered_from=342). T63 depends on P0343 (DONE — tonic-health normal dep at :38 exists; discovered_from=343). T64+T65 depend on P0346 (DONE — _PHANTOM_AMEND_FILES + :220 guard + phantom tests exist; discovered_from=346). T66 no dep (SKILL.md session-cached; coord self-report fabrication #12 — 6th same-session, dedupe of followup rows 14+15). P0304-T1 ALREADY COVERS the .github/ regex extension (coord's row-5 followup is a reminder that T1 needs dispatch, not new scope). T65 coupled fix+test — the empty-set subset is guarded by the truthiness check at :220; dropping it makes no-op-amend detected, test proves it. T67 depends on P0344 (DONE — put_path_batch.rs line-cites arrived with it; discovered_from=344). T68 depends on P0309 (DONE — kubectl-patch removed; comment stale; discovered_from=309). T69 depends on P0349 (UNIMPL — encode_pubkey_file helper at :532 arrives with it; discovered_from=349). T70+T71+T72 depend on P0213 (UNIMPL — ratelimit.rs, ensure_permit, ResourceExhausted variant all arrive with it; discovered_from=213). T73 depends on P0350 (UNIMPL — fixed 1 marker; 55 siblings remain; discovered_from=350). T73 soft-dep P0353 (migration-freeze — if P0353-T2 strips 18.sql commentary and a marker was in that commentary, resequence; unlikely — markers are in docs/src not migrations). T73 same fix-class as P0304-T27 (sched.ca.* 3-sibling fix, already in this plan) and P0295-T31 (dashboard.md 3-sibling fix, different plan, non-overlapping files). The awk scan at dispatch finds exact set; the followup cited 12 in store.md + 44 elsewhere = 56 total with 1 fixed by P0350 → 55. T74+T76+T78 depend on P0301 (DONE — mutants derivation + .config/mutants.toml exist; discovered_from=bughunter-mc112/consolidator-mc110). T75 depends on P0301 (justfile mutants recipe). T76 soft-conflicts P0300 (golden-matrix.nix arrives with it; T76 touches same file adding goldenTestEnv arg). T77 no new hard dep (migration 16 shipped phase-4c; r[gw.jwt.issue] spec exists; jwt_claims local at grpc/mod.rs:498 exists from P0260/P0349 chain — discovered_from=bughunter-mc112-T1). T77 serves r[gw.jwt.issue] — spec mandates INSERT, code orphaned it. T74-T78 all additive/localized — T77 is the only HOT-file toucher (db.rs=40, grpc/mod.rs=34) but is single-param-add + single-bind-add, low conflict surface. Soft-dep P0358 (phantom-amend stacked): T64+T65 edit same if-block as P0358-T1 — T64 at :173, T65 at :220, P0358-T1 at :208 — all different conditions, rebase-clean either order. T79 depends on P0348 (DONE — override-input upstream-nix bump creates the failure-coupling hazard; discovered_from=consolidator-mc115). T80 depends on P0286 (UNIMPL — device-plugin.yaml DaemonSet arrives with it; discovered_from=286-review). T79 is 1-line yaml add at :57 (.github/ — P0304-T1's regex extension covers this path). T80 is ~8-line livenessProbe block add to a file that doesn't exist pre-P0286. T81+T82+T83 depend on P0255 (DONE — quota.rs:200 human_bytes + :136 warn! + metrics_registered.rs:6 GATEWAY_METRICS const exist; discovered_from=255). T81 is pure doc-comment fix; T82 is convention-align (code+message → error=%status); T83 is test-const sync with observability.md spec table. T84+T85 depend on P0345+P0353 (DONE — merged DURING planning; retargeted from plan-doc-errata to landed-code fixes). T84 discovered_from=345-review (apply_trailer [u8;32] return — both callers if-let-Err, never bind Ok). T85 discovered_from=353-review (M_018:57 'PutChunk wraps in BEGIN..COMMIT' contradicts chunk.rs:262 'Not in a single transaction ... autocommit' — inherited the ORIGINAL .sql comment's same-txn mistake that P0295-T40 was fixing; P0353 moved it to Rust without correcting it). T84 touches rio-store/src/grpc/mod.rs (count=15 — same file as T49/T50 maybe_sign rewrite and P0352; additive signature change, no conflict with behavior). T85 touches rio-store/src/migrations.rs (NEW from P0353, count=1 — no conflicts). Soft-dep P0362: T9 here is SUPERSEDED by P0362 (extract-submit-build-grpc-helper) — the copy-paste happened (4 copies + 5th in P0360); P0362 owns the extraction. Skip T9 if P0362 dispatched. T86 depends on P0357 (DONE — helm-lint yq asserts at flake.nix:498-573 exist; discovered_from=357). T87 depends on P0321 (DONE — DescribedByType at metrics.rs:86 exists; discovered_from=321). T88 depends on P0353 (DONE — migrations.rs + lib.rs:19 mod decl exist; coverage-tier failure, nextest passes; discovered_from=coverage-backgrounded). T89 depends on P0297 (UNIMPL — sqlx-prepare-check hook arrives with it; discovered_from=297-review). T90 no hard dep (consolidator-mc125 fault-line marker; validate_put_metadata at :124 landed with P0345 DONE). T86 is pure /tmp→$TMPDIR sed in one derivation's checkPhase — zero behavior change in Nix sandbox. T87 removes dead pub API (0 callers; clippy doesn't flag dead pub in a lib crate). T88 may be a one-off cache-poison — check if a fresh .#coverage-full passes before touching code. T90 is comment-only, no logic change. T91+T92+T93 depend on P0352+P0217 (DONE — resolve_once exists post-P0352; NormalizedName/EmptyTenantName exist post-P0217; discovered_from=352-review,217-review). T91 touches signing.rs (count=8, low traffic); tests migration is cfg(test)-only. T92 is 2-site conditional: if option-(a), also touches admin/mod.rs:727. T93 soft-dep P0298 (NormalizedName tighten — T93 makes the mock track whatever P0298 ships; sequence-independent, works pre or post). T94 no hard-dep (consolidator-mc130; _helpers.tpl tls defines landed pre-sprint-1). T94 is NET-NEGATIVE across 5 component yamls + 1 helper file; helm-lint at .#checks covers render-correctness. T94 CARE: rio.tlsVolume takes arg (secret-name) — self-guard needs root context, not the arg; dict-signature change or 2-list at dispatch. T95+T96 depend on P0230 (DONE — RebalancerConfig + spawn_task + compute_cutoffs exist; discovered_from=230-review). T95: struct doc-comment :40-43 says 'config-driven via scheduler.toml [rebalancer]' but no Deserialize derive + spawn_task hardcodes ::default() at :326 — doc-comment LIES. Paired with P0295-T58 (doc-side fix). T96: compute_cutoffs :144 durations[len()-1] wraps on empty — guarded by min_samples=500 default today, becomes reachable once T95 exposes to TOML. P0230's own DISPATCH NOTE at :9 called this out, never applied. T95 soft-conflict P366 (touches spawn_task :350+ for gauge re-emit; T95 touches :326 ::default() + sig change — non-overlapping, both additive to same fn). T97 depends on P0341 (DONE — the CLAUDE.md example block at :190-197 arrived with it; discovered_from=341-review). T97 IRONIC SELF-FIX: P0341's own convention doc demonstrates the same-line-trailing pattern it forbids (tracey parses col-0 only). T98 depends on P0354+P0359 (DONE — Rule::new().message() pattern + :75-81 rationale comment exist; discovered_from=bughunt-mc133). T98 is the pre-existing sibling batch to T11's seccomp CEL messages — same fix shape, three bare-string :137/:183/:401 sites. T99 depends on P0285 (DONE — disruption.rs watcher + builders.rs:59 label literal exist; discovered_from=285-review). T99 soft-conflicts P0370 (spawn_periodic — touches disruption.rs for option-c inline-biased annotation; T99 touches :48 const-hoist; non-overlapping). T100 depends on P0360 (UNIMPL — nonpriv variant + DS bring-up path arrive with it; the 180s timeout is pre-existing but only becomes tight under P0360's variant; discovered_from=360-review). T101 depends on P0285 (DONE — drain_worker call at disruption.rs:~125 exists; discovered_from=285-review). T101 serves r[obs.metric.controller] (adds counter row to spec table + emit + register-test — same parity shape as T26/T47/T50). T101 soft-conflicts P0311-T33 (adds all_histograms_have_bucket_config to controller tests — both additive to metrics_registered.rs, non-overlapping test-fns). T102+T103 depend on P0233 (DONE — WPS reconciler + child_name exist; discovered_from=233-review). T102 is 2-word edit in describe-strings; T103 signature change Result<String> with 2 caller updates. T104+T105 depend on P0273 (DONE — dashboard-gateway templates exist; discovered_from=273-review). T105 soft-dep P0371 (adds the plan the TODO tags point at — if P0371 dispatches first, it deletes both comments and T105 is moot). T106 depends on P0340 (UNIMPL — seed_tenant helper touches create_tenant path; const useful for both; discovered_from=340-review). T107 depends on P0362 (DONE — the P0362 review found the admin/mod.rs:388 dup; discovered_from=362-review). T108 no hard-dep (consts exist at rio-proto:25,43 + jwt_interceptor:68; gateway build.rs:372 literal predates TENANT_TOKEN_HEADER const; discovered_from=consol-mc145). T108 soft-dep P0287 (TRACE_ID_HEADER landed there — same consts-section parity). T109 depends on P0362 (DONE — submit_build_grpc helper sits in same preamble; discovered_from=consol-mc145). T109 touches lifecycle.nix+scheduling.nix (both HOT — lifecycle=40+ plans, scheduling=20+). Helper extraction is additive at preamble; 6 migrations are subtractive at scattered lines; ~-40L net. T110 no hard-dep (rio-impl-merger.md session-cached; lands next worktree-add; discovered_from=consol-mc145). T110 distills ci-gate-fastpath-precedent.md into merger-agent-embedded decision tree — mergers stop re-deriving. T111 depends on P0340 (DONE — TenantSeed builder exists at pg.rs:420; discovered_from=consol-mc155). T111 migrates 4 INSERT INTO tenant_keys test sites to a new .with_ed25519_key builder method (same shape one table down from .with_retention_hours). T111 also adds COALESCE sibling-note: pg.rs:460 has COALESCE($2,168) — same magic-168 as T106's db.rs:351 target (bughunt-mc154 accumulation finding — 'already filed' pointed at T106). rio-test-support can't dep on rio-scheduler so comment-document the dup. T112-T114 depend on P0234 (UNIMPL — scale_wps_class + per-class loop arrive with it; discovered_from=234-review). T112-T114 are p234-worktree line-refs — re-grep at dispatch. T112 is comment-vs-code audit (may find no divergence — in which case no-op with rationale). T113 is post-P0234 future-tense cleanup (same class as T33/T34 post-P0260). T114 is a format!-dup → shared-fn (same shape as T107 parse_build_id). T112-T114 soft-conflict P0374 (the asymmetric-key flap fix also touches scale_wps_class — T114's child_name refactor is orthogonal to P0374-T1's is_wps_owned gate, both additive to the same fn body, rebase-clean). T115 depends on P0288 (DONE — bloom_expected_items config exists; discovered_from=288-review). T115 is a 2-line info! add — pure observability trivia. T116+T117 depend on P0277 (DONE — Builds.svelte + Cluster.svelte exist; discovered_from=277-review). T116 is Svelte-4→5 prop-syntax consistency; T117 is a poll-pattern improvement (document.hidden pause is the main win — background tabs stop polling). T118 depends on P0282 (DONE — nginx docker image exists at docker.nix:473; discovered_from=282-review). T118 is a closure-size trim (1.8MiB). T119 depends on P0369 (UNIMPL — devicePlugin.image tag fix; T119 extends P0369-T2's helm-lint guard to also check airgap-set sync; discovered_from=369-review). T120 no hard-dep (kvm-pending.md from P0311-T16; discovered_from=consol-mc150). T121 depends on P0234 (UNIMPL — adds 2 of the 6 SSA-envelope sites; discovered_from=consol-mc150). T121 soft-conflicts T112/T114/P0374 (all touch scaling.rs — T121 migrates json! calls, others touch scale_wps_class body; non-overlapping). T121's ssa_envelope<T> helper lands in reconcilers/mod.rs; migrates 4 sprint-1 sites + 2 p234 sites. consol-mc155 re-flagged T76 (testRunnerEnv = same triplication T76 already covers — confirms still live, no new scope). consol-mc155 'flake.nix size trajectory' is a tracking note, not a T-task — land in .claude/notes/flake-size-tracking.md if/when the trajectory becomes actionable. T122 depends on P0281 (DrainButton.svelte arrives) + soft-dep P0377 (idx-race fix rewrites drain() — T122 polish rebases onto fixed shape; sequence P0377 FIRST). T123 depends on P0356 (DONE — worker_service.rs :198 tokio::spawn moved there with the split; discovered_from=356-review). T124 depends on P0281 (Workers.svelte pill code; discovered_from=278-review; rev-p278 flagged code that P0281 introduces). T125+T126+T127 depend on P0278 (BuildDrawer.svelte + Builds.svelte arrive; discovered_from=278-review). T124-T127 are all additive attr-adds or small extractions, no conflict with P0279/P0280 placeholders filling in. T128 depends on P0374 (DONE or merging — is_wps_owned_by + find_wps_child exist; discovered_from=374-review). T128 soft-conflict P0381 (scaling.rs split — T128 is one-line guard change at :1030, moves to scaling/mod.rs post-split; sequence-independent, rebase-clean). T128 touches scaling.rs (HOT, collision≈16) — additive single-line guard swap + one test fn, low conflict. T129 depends on P0371 (DONE — dashboard.cors.allowOrigins at values.yaml:429 exists; discovered_from=371-review). T129 creates/touches NOTES.txt (rare, low-conflict). T130 depends on P0371 (DONE — the 3 'until authz lands' sites arrived with P0371's rewrite of dashboard-gateway.yaml; discovered_from=371-review). T130 soft-dep T105 (T105 tagged the PRE-P0371 phase-5b comment; T130 rephrases the POST-P0371 comment; T105 may be moot — check at T130 dispatch). T130 edits r[dash.auth.method-gate] spec text at dashboard.md:30 — NO tracey bump (the 'until X lands' clause is informational not normative; behavior unchanged). T131 depends on P0237 (UNIMPL — main.rs:513 unreachable! arm arrives with wps subcommand; discovered_from=237-review). T131 is pure doc-comment add, zero behavior change. T132 depends on P0283 (UNIMPL — dashboard image preload at k3s-full.nix:166 conditional arrives with it; discovered_from=283-review). T132 is nix-arithmetic fix + comment — check nix-eval for tmpfs-size at dispatch. T133 depends on P0237 (UNIMPL — wps.rs:114+147 format! sites exist only post-P0237) + T103/T114 (controller-side child_name consolidation — T133 hoists to rio-crds AFTER that's landed; discovered_from=237-review). T133 touches rio-crds/src/workerpoolset.rs (low traffic) + wps.rs (P0237-new) + builders.rs (T103 target — rebase-clean, T103 changes fn body to add RFC-1123 guard, T133 changes fn body to delegate to rio-crds). Sequence: T103 FIRST (adds guard inline) → T133 (hoists guard+formula to rio-crds; controller becomes wrapper). T134 depends on P0376 (UNIMPL — lib.rs dag/build_types re-exports arrive with it; discovered_from=376-review). T134 option-(a) is a mechanical ~30-file sed; option-(b) is 1-line doc-comment. Prefer (a) but defer to (b) if 30-file churn is too high at dispatch (check git diff --stat). T135 no hard-dep (.claude/notes/bughunter-log.md exists from T40; discovered_from=bughunter-mc168). T135 soft-dep P0383 (the mc168-T5 actor-alive drift is FOLDED INTO P0383-T1 — T135's log entry references that). T115 re-flag confirmed by rev-p375 (same cache.rs:276 log gap; P0375 made it sharper via CRD knob — T115 scope unchanged). T136 depends on P0379 (UNIMPL — envoyImage digest-pin + docker-pulled.nix entry arrive with it; discovered_from=379-review). T136 is T119's envoy sibling — same lockstep-assert shape, adjacent helm-lint block, complementary to P0379-T2's generalized yq-loop (different axis: values-vs-airgap tag sync, not values-digest-pinned). T136 soft-dep P0378 (mkFragmentTest extraction touches common.nix — different file but both in nix/tests/ tier; sequence-independent). T137 no hard-dep (common.nix mkAssertChains exists since P0378 predecessor; discovered_from=bughunt-mc175). T137 is 1-line Nix expr change + comment — common.nix count=17, all prior plans touch different sections (kvmCheck, busybox seeding, helpers). T138 depends on P0376 (DONE — dag+build_types re-export modules + admin_types.proto split exist; admin_types re-export was omitted; discovered_from=consol-mc175). T138 is T134's parity partner — T134 addresses migration-completeness, T138 addresses re-export-module-presence. Same file (rio-proto/src/lib.rs), non-overlapping additive edits (T134 at :85 doc or caller-sites; T138 adds new block after :109). T139 depends on P0250 (DONE — is_ca column in ON CONFLICT SET exists at db.rs:841; discovered_from=250-review+bughunt-mc175 VERIFIED-SAFE). T139 is comment-only at :840, db.rs count~40 HOT but comment-add inside existing SQL block, zero behavior change. T140+T141 depend on P0383 (DONE — actor_guards.rs exists; the :21 string + grpc/mod.rs:142 duplicate are live post-split; discovered_from=383-review). T140+T141 COUPLED: T141's hoisted ChannelSend arm uses T140's const; single commit. grpc/mod.rs HOT count=38 — T141 shrinks a 26-line match-body to a 1-line delegate, T140 changes a string literal to a const ref; both low-semantic-conflict. T141 touches admin/mod.rs (count=21), admin/{gc,sizeclass,workers}.rs (low-traffic post-P0383) — path-shortening sed, no logic change. T141 soft-conflicts P0386 (admin/tests.rs split — T141's actor_error_to_status test at grpc/tests.rs:806 stays in grpc/tests, unaffected; the admin tests don't directly test actor_error_to_status). T142 depends on P0380 (UNIMPL — fixtures.rs test_workerpool_spec must exist first; discovered_from=380-review). T142 is P0380's ONE-TABLE-UP sibling: same extract-to-fixtures pattern for WorkerPoolSetSpec (2 sites vs P0380's 4). T142 soft-conflicts P0381 (scaling.rs split — test_wps moves to scaling/per_class.rs; sequence-independent). scaling.rs count=16, builders.rs count=22 — both edits collapse a 20-30L literal to a 1-line delegation inside a #[cfg(test)] mod. T143 no hard-dep (bughunter-log.md exists from T40; discovered_from=bughunter-mc182). T143 is the mc182 sibling to T135's mc168 entry. Soft-dep P0387 (T119/T136 SIMPLIFIED blockquotes from that plan's T5 land here; sequence-independent — the blockquote is advisory prose, T119/T136 can dispatch with either assert shape). T144+T145+T146 depend on P0384 (DONE — DerivationLike trait at mod.rs:149 + dual translate.rs disjunction at :390/:428 exist; discovered_from=384-review). T144 adds is_content_addressed() trait default; T145 narrows trait to outputs/env/platform; T146 renames DerivationOutput::is_fixed_output→has_hash_algo. T144+T145 soft-conflict P0388 (generic build_node<D: DerivationLike> — T144-first makes P0388's struct-field cleaner; T145 MUST keep outputs/env/platform for P0388's builder to compile; sequence-independent, both orders compile). T144+T148 touch HOT files (translate.rs=24, completion.rs=26) but both are small-scope: T144 swaps a disjunction-expr at 2 sites, T148 rewrites one match-arm. T146 is rio-nix-only (mod.rs low-traffic post-P0384). T147 no hard-dep (bughunter-log.md exists from T40; third consecutive null cadence mc175→mc182→mc189; discovered_from=bughunter-mc189). T148 depends on P0251 (DONE — ca_hash_compares counter + match/miss labeling exist at completion.rs:329; discovered_from=251-review). T148 soft-dep P0311-T54 (slow-store regression test — same code region, additive test-fn, sequence-independent). T149 depends on P0367 (DONE — the 3 omissions were surfaced by rev-p367; discovered_from=367). T149 STOPGAP-SUPERSEDED-BY P0394 (derives SPEC_METRICS from obs.md — T149 is the manual fix, P0394 makes manual sync unnecessary; skip T149 if P0394 dispatched first). T150 depends on P0367 (DONE — normalize_key_comment landed; discovered_from=367). T151 depends on P0367 (DONE — the PG CHECK and auth.rs:101 branch exist; discovered_from=367). T152 depends on P0365 (DONE — warn_on_spec_degrades + event reason strings at mod.rs:255 + tests.rs:1648/1709 exist; discovered_from=365). T152 soft-conflict P0396 (workerpool/tests.rs split — T152's tests.rs edits move to tests/disruption_tests.rs post-split; sequence T152 FIRST or coordinate). T153 depends on P0299 (DONE — RIO_TEST_PREFETCH_DELAY_MS env-hook at runtime.rs:814 exists; discovered_from=299). T153 soft-dep P0311-T58 (the ordering-proof test that would use the hook). T154+T155 depend on P0279 (UNIMPL — logStream.svelte.ts + LogViewer.svelte arrive with it; discovered_from=279). T155 soft-dep P0280 (node-click → drvPath live). T156 depends on P0386 (DONE — gc_tests.rs exists post-split; discovered_from=386). T157 depends on P0251 (DONE — completion.rs 32-byte guard at :295-304 exists; discovered_from=251). T157 PARTNERS WITH T148 (T148=error label for RPC-fail/timeout, T157=malformed label for 32-byte guard — DIFFERENT edges of same counter). T157 soft-dep P0393 (adds skipped_after_miss label to same counter — consolidate all five labels in one obs.md table edit). T158-T161 depend on P0253 (DONE — ca/resolve.rs exists; discovered_from=253). T158 dedupes serialize_resolved ~110L against rio-nix aterm.rs; fixes :444 doc-comment that describes a rejected approach. T158 soft-conflict P0398 (tryResolve semantics — that plan changes inputDrvs-drop to always-empty; if P0398 lands first, T158's to_aterm_resolved takes no drop_input_drvs param). T159 is pure signature simplification (zero fallible ops in body). T160 is 1-line pub-use add (P0254-T7 expects ca::insert_realisation_deps). T161 is ahead-of-time perf (resolve only fires for floating-CA-on-CA chains + collect_ca_inputs stubbed until P0254-T6; same UNNEST-batch pattern already at :395-408, consistency argument). T162 no hard-dep (bughunter-log.md exists from T40; mc196 was FINDING not null — self-match→P0397; entry documents finding + audit counts; fourth entry after T135/T143/T147). FIXED soft_deps leading-zero: 394→394, 396→396, 393→393 per T28's own rule (fifth occurrence). T172-T178 from consolidator-mc205 + rev-p271 + rev-p295. T172 is THIS BATCH's own T30 gap (rename-unassigned batch-target cross-ref scan) biting live 13× across 3 docs-merges — structural, fires every cross-ref batch-append until T30 tooling fix lands. Sed mapping (7 entries, verified via git rename history): 353→353, 356→356, 361→361, 362→362, 363→363, 374→374, 375→375. T173 same inherited-idiom class as P0306-T1 (onibus 2-dot→3-dot); agent-md session-cached, lands next worktree-add. T174 is T158-addendum (3rd ATerm copy, not new dedup — scope extension only). T175+T176 depend on P0271 (cursor decode + proto comment both p271-worktree refs; post-P0271-merge); T175 is version-dispatch-enable refactor, T176 is pure doc-string. T177+T178 depend on P0295 (both are p295-worktree artifacts — T177=T65-edit-wrap, T178=T69-partial-extraction followup); post-P0295-merge. T175 soft-dep P403 (both touch admin/builds.rs post-P0271; P403-T3 edits same file different fn body; non-overlapping). T179-T184 from rev-p307/rev-p400/rev-p396/rev-p394/rev-p387. T179 depends on P0307 (spawn_with_leader grows 8→10 with poison_config+retry_policy; discovered_from=307); DeployConfig bundle absorbs future sub-config adds (T180's POISON_TTL becomes a 1-field-add not 11th-arg). T180 depends on P0307 (PoisonConfig TOML surface exists; discovered_from=307); LOW PRIORITY — scheduler.md:108 describes TTL as fixed-24h, skip if no operational need. T181 depends on P0400 (allTerminal poll-stop arrives with T3; discovered_from=400); the 'scheduler walks forward' comment is a wrong-model heuristic — truncated response means visible-terminal not all-terminal. T181 soft-conflict P0400-T2/T3 (same Graph.svelte region :89-186; T181 at :177-182; sequence AFTER P0400). T182 depends on P0396 (tests/mod.rs + builders_tests.rs exist post-split; discovered_from=396); pub(super)→pub(crate) makes fixtures reusable by workerpoolset/tests/; builders_tests 1013L is the P0386/P0395 split-pattern applied one more level. T183 depends on P0394 (metrics_grep.rs + per-crate floor-checks exist; discovered_from=394); controller floor >=5 vs actual=6 means ONE row-delete silently passes — tight floors catch drift by design. cargo:warning fires every fuzz build (docs/ excluded from fileset) — gate on CI env. T184 depends on P0387 (destNameTag derivation exists; discovered_from=387); VERIFY-AT-DISPATCH whether the reported two-profile inconsistency survives P0387 or was a p387-intermediate-state finding (candidates: docker-pulled.nix:53-59 stale comment OR unmigrated second nonpriv registration). May route to P0295 if comment-only. T127 SUPERSEDED by P0406 (buildInfo.ts extraction — broader scope incl timestamp helpers + Workers.svelte; T127 was progress()-only to lib/build.ts). T179 soft-dep P0409 (config validation — same main.rs config-load region, non-overlapping: validation at :192, DeployConfig bundling at actor-spawn ~:380). T189 depends on P0409 (DONE — validate_config() fn at :178 exists; T189 moves size_classes ensure INTO it; discovered_from=409). T189 soft-conflict P0415 (RetryPolicy backoff f64 bounds — same validate_config body, additive different subconfig, sequence-independent) + T95/T96 (RebalancerConfig — same body, additive). T190 no hard-dep (bughunter-log.md created by T40; discovered_from=bughunter-mc224). T191 depends on P0414 (DONE — dag_flip update-ref fallback at merge.py:217-223 exists; discovered_from=414). T191 soft-conflict P0417 (also touches merge.py dag_flip + models.py DagFlipResult — non-overlapping hunks, rebase-clean either order). T192 depends on P0405 (DONE — speculative_cascade_reachable extraction made find_cutoff_eligible thin-wrapper dead; discovered_from=405). T192 soft-conflict T169 (both touch dag/mod.rs — T169 at :14-21+:635+:646 rename, T192 at :513-515 delete + :17+:517 doc-rewrite; non-overlapping). T169 SCOPE-+2 note depends on P0405 (DONE — the two extra MAX_CASCADE sites at completion.rs:84 + dag/mod.rs:646 arrived with the BFS extraction). T193 depends on P0311 (DONE — kvm-pending.md T16 criterion exists; discovered_from=311); SUPERSEDES T120 (consol-mc150's partial +2 — this backfills full set). T194 depends on P0311+P0405 (both DONE — test doc-comments + P0405 rebase drift exist; discovered_from=311); soft-conflict P0419 (same file completion.rs, different doc-comments :332-337 vs the :397/:422/:470 stale cites — non-overlapping). T195 depends on P0311 (DONE — actor::tests::completion:: tests exist; discovered_from=311); nextest.toml low-traffic. T196+T197+T198 depend on P0412 (UNIMPL — banner comments arrive with it; discovered_from=412-review); T196 is T194's sibling (same citation-tiering principle, different files). T197+T198 COUPLED (hoist rationale + known-limitation to same rio-common/src/config.rs module-doc section). consol-mc230 ALSO flagged find_cutoff_eligible DEAD → already T192, NOT duplicated. FIXED soft_deps leading-zero (6th occurrence per T28 rule): 418→418, 419→419. T199 depends on P0407 (DONE — eslint.config.js no-restricted-imports exists; discovered_from=407). T200+T201+T207 depend on P0411 (DONE — db/ split landed; stale db.rs cites are post-split artifacts; discovered_from=411). T202 no hard-dep (connect_worker_no_ack at worker.rs:819 exists since P0311 warm-gate tests; discovered_from=consolidator). T202 soft-conflict P0422 (CaFixture extraction — both add to helpers.rs, non-overlapping). T203+T204 depend on P0417 (DONE — already-done scan + plan kwarg exist; discovered_from=consolidator+rev-p417). T203+T204 soft-conflict P0418 (merge.py canonicalization — non-overlapping hunks) + P0420 (count_bump write-order — T203 rewrites :76/:114/:154 rev-parse calls, P0420 rewrites :123-163 count_bump body; non-overlapping) + P0421 (rename-cluster extraction — removes :368-604, T203+T204 touch :76-224; non-overlapping). T205 depends on P0416 (DONE — validate_config() exists in controller; discovered_from=416). T205 soft-conflict P0425 (ensure_required helper — same validate_config body, additive different ensures). T206 depends on P0411 (DONE — db/ split; 317L mod.rs residual is the subject; discovered_from=consolidator). LOW-PRIORITY mechanical. T208 depends on P0402 (check-mutants-marker hook arrives with it; discovered_from=402 — P0402 may be merging at mc240 per d31f80dc commit msg; verify at dispatch). T209 no hard-dep (bughunter-log.md exists from T40; archives consol-mc235/236 + bughunt-mc238 verdicts; discovered_from=consolidator+bughunter+coordinator). T207 depends on P0411 (DONE — db/recovery.rs exists post-split; cohesion note only, no extraction; discovered_from=411). T210+T211 no hard-dep (coord rix §3/§6; merge.py _cadence_range + lock_status exist). T210 soft-dep P427 (chain-merger adds lock_renew — T211's discriminator reads the renewed plan field). T212-T215 depend on P0392 (UNIMPL — virtualization + cap + viewportOverride arrive with it; discovered_from=392). T212+T213+T214+T215 soft-conflict P426 (same two files — P426 edits LINE_H/push/cap; T212 uses T3's excess var; T213-T215 touch :138/:139/:26 non-overlapping). T216 depends on P0391 (UNIMPL — worker.rs MAX_PREFETCH_PATHS const arrives with it; discovered_from=391). T217 depends on P0366 (UNIMPL — GaugeValues at rebalancer/tests.rs:294 arrives with it; discovered_from=366). T217 soft-conflict P0423 (MockStore FaultMode refactor — rio-test-support, different file metrics.rs vs grpc.rs)."}
```

**Depends on:** [P0222](plan-0222-grafana-dashboards.md) — merged at [`6b723def`](https://github.com/search?q=6b723def&type=commits). T1 (harness regex) has no dep.

**Conflicts with:** `infra/helm/grafana/*.json` freshly created by P0222; no other UNIMPL plan touches them. `justfile` is low-traffic. `.claude/lib/onibus/models.py` — [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) T2 touches it too (LockStatus cleanup), different section. `.claude/lib/onibus/build.py` — T10+T13 both touch it; [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) does not. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T6 adds asserts to `cel_rules_in_schema` (test fn), T11 here adds messages to the derive attrs (struct) — different sections, same file. [`scheduler.md`](../../docs/src/components/scheduler.md) count=20 — T25 edits `:449-451`, T27 edits `:254/:258/:262`; non-overlapping. [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T2-path-B may ALSO edit the `r[sched.timeout.per-build]` text — if it dispatches, sequence T25 BEFORE it (T25 is a definite correction; 0329-T2-B is conditional on probe outcome). [`observability.md`](../../docs/src/observability.md) count=21 — T26 inserts one row at `:114` and edits `:116`; additive. [`flake.nix`](../../flake.nix) count=31 — T29 is 4 lines in the `tracey-validate` block at `:398-416`; no other batch task touches that block. Low conflict risk across the board.
