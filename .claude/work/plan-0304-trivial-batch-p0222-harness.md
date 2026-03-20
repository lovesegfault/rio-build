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

MODIFY [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs) at `:412,:437-439,:444-445,:466` — P0342's fix shifted `put_path_batch.rs` by +6 lines. Doc-comments in the `gt13_batch_placeholder_cleanup_on_midloop_abort` test reference `:275/:277/:280/:284` which are now `:281/:283/:286/:290` (re-grep at dispatch — may have drifted further). Test mechanics unaffected (string-anchor based `find("--- Phase 2:")` at `:419`); informational drift only. Sed the 4 line-refs to current values. discovered_from=342.

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

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) at `:160`, `:363`, `:389` — comment line-cites to `put_path.rs` drifted: `:160` says "put_path.rs:316-331" (actual ~338-358, the path_not_in_claims block); `:363` says "put_path.rs:629-641" (actual ~648-668, content_index::insert); `:389` says "put_path.rs:645" (actual :672, bytes_total). All three point at the right concept/file; line numbers stale.

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
  {"path": ".claude/agents/rio-impl-merger.md", "action": "MODIFY", "note": "T110: +Clause-4 fast-path decision tree section with 5-tier ladder + proof-recipes table"}
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
{"deps": [222, 223, 290, 294, 315, 305, 306, 247, 317, 214, 320, 325, 328, 270, 302, 267, 264, 266, 338, 339, 260, 296, 300, 333, 337, 342, 343, 346, 344, 309, 213, 349, 350, 301, 348, 286, 255, 345, 353, 357, 321, 297, 352, 217, 230, 341, 354, 359, 285, 360, 233, 273, 362, 340], "soft_deps": [303, 216, 289, 313, 206, 207, 316, 318, 322, 335, 326, 332, 996394101, 358, 997105502, 298, 366, 370, 371, 287], "note":"T2-T4 depend on P0222 (dashboard files exist — merged). T6 depends on P0290 (clippy.toml exists). T8/T9 depend on P0294 (Build CRD rip — dead variant becomes dead, lifecycle.nix rewritten). T11/T12 depend on P0223 (seccomp CEL rules + profile JSON exist). T14 soft-dep P0206 (lifecycle.nix :1204 copy exists post-P0206; if T14 lands first, extract in k3s-full.nix only and P0206/P0207 use the helper). T15 depends on P0315 (DONE — kvmCheck CREATE_VM probe exists; T15 drops the vestigial GET_API_VERSION it left behind; discovered_from=315). T10 SEQUENCED AFTER P0317 (_VM_FAIL_RE foundation — without it T10's early-return masks co-occurring real failures; re-scoped to supplementary grant per P0317 T7 forward-reference). T16 soft-dep P0316 (DONE — ConnectionResetError is downstream of its -machine accel=kvm gate) + P0317 T4 (if mitigations list migration landed, add as Mitigation entry instead of symptom string). T17 depends on P0306 (DONE — _LEASE_SECS lives in merge.py which P0306 refactored; discovered_from=306). T18 depends on P0305 (DONE — functional/references.rs discard-read sites exist; discovered_from=305). T19 depends on P0305 (same). T20 discovered_from=bughunter mc28 + soft-dep P0318 (same function, comment lands on whichever name is live). T21 depends on P0247 (DONE — corpus README.md exists; discovered_from=247). T22 depends on P0317 (DONE — drv_name+mitigations fields exist; discovered_from=317). T23 depends on P0317 (DONE — cli.py:380 duplication site exists; discovered_from=317); soft-conflict P0322 T2 (also touches cli.py :371-375 — T23 touches :380, non-overlapping but same function). T24 no dep (pure dag.jsonl sed; discovered_from=317, the ref SHOULD have pointed there). T25+T26 depend on P0214 (DONE — worker.rs:570-609 + :593 metric exist; spec/doc never reconciled). T27 depends on P0320 (DONE — same defect class, P0320 fixed :265, T27 fixes :253/:257/:261 siblings from f190e479 seed). T28 no dep (agent-file guidance; session-cached, lands next worktree-add). T29 no dep (flake.nix fileset pattern already established :168-174). Soft-dep P0328 T2: adds describe_counter! for the same metric T26 documents — sequence-independent, both serve one gap. IRONIC SELF-FIX: this fence's soft_deps had 0322 and 0328 (leading zeros) from prior appends — fixed to 322/328 here per T28's own rule. T30 depends on P0325 (DONE — _rewrite_and_rename mapping-derived touched exists; T30 extends it with batch-append grep pass; discovered_from=coordinator, docs-933421 stale-ref manifestation). Soft: T5 references p216 worktree line numbers (P0216). T9 must land BEFORE P0289 dispatches. T10's KVM-DENIED-BUILDER marker is emitted by P0313 — match BOTH pre/post markers for transition. T1/T13 independent. T31 depends on P0328 (DONE — discovered_from=328): P0328 added metrics_registered.rs to 4 crates with r[verify] annotations that test_include doesn't scan. 7 of the 13 invisible annotations pre-date P0328 (rio-store/tests/grpc/* — landed with P0305 store.put.idempotent verify and earlier). config.styx is low-conflict (single file, append to a glob list). T32 soft-dep P0335 (UNIMPL — the tag POINTS AT IT, not BLOCKED BY IT; P0335 does NOT need to land first. If P0335 lands before T32 it rewrites session.rs:32 entirely and T32 becomes moot — check at dispatch, skip T32 if P0335 already closed it). T33 no dep (cosmetic word removal). T32 SEMANTIC NOTE: P0335 plans to add r[gw.conn.cancel-on-disconnect] and rewrite the cancel path — T32's tag is a BREADCRUMB so anyone reading session.rs before P0335 lands knows where to look. T34 depends on P0270 (DONE — emit_progress and BuildProgress exist; reviewer discovered_from=270). T34 HOT FILES: worker.rs=27 completion.rs=25 dispatch.rs=22 — each edit is 3-6 lines additive at a specific call site, no signature change, low semantic-conflict risk. T35 no dep (SKILL.md session-cached; lands next worktree-add; coordinator self-report discovered_from=262). T36 depends on P0302 (remediation-07 changed validate_dag to STDERR_LAST; doc never reconciled; discovered_from=302). T36 text edit to r[gw.reject.nochroot] paragraph — default NO tracey bump (rejection-happens unchanged, frame-type differs). discovered_from: T14=206, T15=315, T16=316, T17=306, T18=305, T19=305, T21=247, T22=317, T23=317, T24=317, T30=325, T31=328, T32=bughunter, T33=bughunter, T34=270, T35=coordinator(262), T36=302, T37=267, T38=267, T39=coordinator(mc77-80), T40=bughunter(mc77+mc84). T37+T38 depend on P0267 (DONE — put_path_batch.rs exists with the :302 unwrap and the metric gaps). T37+T38 soft-conflict P0342 (fixes :275 ?→bail! same file, non-overlapping :268-275 vs :302+:258+handler-entry); sequence-independent, rebase clean. T39 extends T35's SKILL.md block (same file, adjacent paragraph). T40 creates .claude/notes/bughunter-log.md — path is .claude/notes/, not .claude/work/ per layout convention. T41+T42 depend on P0264 (UNIMPL — migration 018 + renamed fn comments arrive with it; discovered_from=264). T43+T44 soft-dep P0326 (DONE — the P0273 scope change + dag row edit happened there; discovered_from=326). T45 no dep (ci-gate-fastpath-precedent.md exists since P0313 fast-path); bughunter-mc84 REFINED coord's earlier row-6 followup (spec correct, note stale; discovered_from=bughunter). T46 soft-dep P0332 (the 017-collision incident; discovered_from=bughunter-mc84). T47 depends on P0266 (UNIMPL — ema_proactive_updates_total metric arrives with it; discovered_from=266). T48+T49+T50 depend on P0338 (UNIMPL — maybe_sign PG-lookup fallback + cluster() + warn! arrive with it; discovered_from=338). T51+T52 depend on P0339 (UNIMPL — enqueue_chunk_deletes extraction + sweep_orphan_chunks double-caller arrive with it; discovered_from=339). Soft-dep P0346: T46 touches collisions.py, P0346 touches git_ops.py+models.py — same onibus package, non-overlapping files. T53 depends on P0260 (TODO arrives with it; discovered_from=260); soft-dep P0349 (the re-tag TARGET). T54 depends on P0337 (NIXBASE32 const arrives; discovered_from=337). T55+T56 depend on P0296 (ephemeral reconciler + lifecycle subtest arrive; discovered_from=296); T55 soft-conflicts P0347 (both touch ephemeral — T55 is main.rs watcher, P0347 is CEL+deadline, non-overlapping). T57+T58 depend on P0300 (daemon_variant() + VARIANT_SKIP arrive; discovered_from=300). T59+T60 depend on P0333 (DONE — accessors + converged make_output_file exist; discovered_from=333). T61 no dep (SKILL.md session-cached; coord self-report discovered_from=coordinator fabrication #10-11). T62 depends on P0342 (DONE — line-shift +6 happened; discovered_from=342). T63 depends on P0343 (DONE — tonic-health normal dep at :38 exists; discovered_from=343). T64+T65 depend on P0346 (DONE — _PHANTOM_AMEND_FILES + :220 guard + phantom tests exist; discovered_from=346). T66 no dep (SKILL.md session-cached; coord self-report fabrication #12 — 6th same-session, dedupe of followup rows 14+15). P0304-T1 ALREADY COVERS the .github/ regex extension (coord's row-5 followup is a reminder that T1 needs dispatch, not new scope). T65 coupled fix+test — the empty-set subset is guarded by the truthiness check at :220; dropping it makes no-op-amend detected, test proves it. T67 depends on P0344 (DONE — put_path_batch.rs line-cites arrived with it; discovered_from=344). T68 depends on P0309 (DONE — kubectl-patch removed; comment stale; discovered_from=309). T69 depends on P0349 (UNIMPL — encode_pubkey_file helper at :532 arrives with it; discovered_from=349). T70+T71+T72 depend on P0213 (UNIMPL — ratelimit.rs, ensure_permit, ResourceExhausted variant all arrive with it; discovered_from=213). T73 depends on P0350 (UNIMPL — fixed 1 marker; 55 siblings remain; discovered_from=350). T73 soft-dep P996394101 (migration-freeze — if P996394101-T2 strips 018.sql commentary and a marker was in that commentary, resequence; unlikely — markers are in docs/src not migrations). T73 same fix-class as P0304-T27 (sched.ca.* 3-sibling fix, already in this plan) and P0295-T31 (dashboard.md 3-sibling fix, different plan, non-overlapping files). The awk scan at dispatch finds exact set; the followup cited 12 in store.md + 44 elsewhere = 56 total with 1 fixed by P0350 → 55. T74+T76+T78 depend on P0301 (DONE — mutants derivation + .config/mutants.toml exist; discovered_from=bughunter-mc112/consolidator-mc110). T75 depends on P0301 (justfile mutants recipe). T76 soft-conflicts P0300 (golden-matrix.nix arrives with it; T76 touches same file adding goldenTestEnv arg). T77 no new hard dep (migration 016 shipped phase-4c; r[gw.jwt.issue] spec exists; jwt_claims local at grpc/mod.rs:498 exists from P0260/P0349 chain — discovered_from=bughunter-mc112-T1). T77 serves r[gw.jwt.issue] — spec mandates INSERT, code orphaned it. T74-T78 all additive/localized — T77 is the only HOT-file toucher (db.rs=40, grpc/mod.rs=34) but is single-param-add + single-bind-add, low conflict surface. Soft-dep P0358 (phantom-amend stacked): T64+T65 edit same if-block as P0358-T1 — T64 at :173, T65 at :220, P0358-T1 at :208 — all different conditions, rebase-clean either order. T79 depends on P0348 (DONE — override-input upstream-nix bump creates the failure-coupling hazard; discovered_from=consolidator-mc115). T80 depends on P0286 (UNIMPL — device-plugin.yaml DaemonSet arrives with it; discovered_from=286-review). T79 is 1-line yaml add at :57 (.github/ — P0304-T1's regex extension covers this path). T80 is ~8-line livenessProbe block add to a file that doesn't exist pre-P0286. T81+T82+T83 depend on P0255 (DONE — quota.rs:200 human_bytes + :136 warn! + metrics_registered.rs:6 GATEWAY_METRICS const exist; discovered_from=255). T81 is pure doc-comment fix; T82 is convention-align (code+message → error=%status); T83 is test-const sync with observability.md spec table. T84+T85 depend on P0345+P0353 (DONE — merged DURING planning; retargeted from plan-doc-errata to landed-code fixes). T84 discovered_from=345-review (apply_trailer [u8;32] return — both callers if-let-Err, never bind Ok). T85 discovered_from=353-review (M_018:57 'PutChunk wraps in BEGIN..COMMIT' contradicts chunk.rs:262 'Not in a single transaction ... autocommit' — inherited the ORIGINAL .sql comment's same-txn mistake that P0295-T40 was fixing; P0353 moved it to Rust without correcting it). T84 touches rio-store/src/grpc/mod.rs (count=15 — same file as T49/T50 maybe_sign rewrite and P0352; additive signature change, no conflict with behavior). T85 touches rio-store/src/migrations.rs (NEW from P0353, count=1 — no conflicts). Soft-dep P997105502: T9 here is SUPERSEDED by P997105502 (extract-submit-build-grpc-helper) — the copy-paste happened (4 copies + 5th in P0360); P997105502 owns the extraction. Skip T9 if P997105502 dispatched. T86 depends on P0357 (DONE — helm-lint yq asserts at flake.nix:498-573 exist; discovered_from=357). T87 depends on P0321 (DONE — DescribedByType at metrics.rs:86 exists; discovered_from=321). T88 depends on P0353 (DONE — migrations.rs + lib.rs:19 mod decl exist; coverage-tier failure, nextest passes; discovered_from=coverage-backgrounded). T89 depends on P0297 (UNIMPL — sqlx-prepare-check hook arrives with it; discovered_from=297-review). T90 no hard dep (consolidator-mc125 fault-line marker; validate_put_metadata at :124 landed with P0345 DONE). T86 is pure /tmp→$TMPDIR sed in one derivation's checkPhase — zero behavior change in Nix sandbox. T87 removes dead pub API (0 callers; clippy doesn't flag dead pub in a lib crate). T88 may be a one-off cache-poison — check if a fresh .#coverage-full passes before touching code. T90 is comment-only, no logic change. T91+T92+T93 depend on P0352+P0217 (DONE — resolve_once exists post-P0352; NormalizedName/EmptyTenantName exist post-P0217; discovered_from=352-review,217-review). T91 touches signing.rs (count=8, low traffic); tests migration is cfg(test)-only. T92 is 2-site conditional: if option-(a), also touches admin/mod.rs:727. T93 soft-dep P0298 (NormalizedName tighten — T93 makes the mock track whatever P0298 ships; sequence-independent, works pre or post). T94 no hard-dep (consolidator-mc130; _helpers.tpl tls defines landed pre-sprint-1). T94 is NET-NEGATIVE across 5 component yamls + 1 helper file; helm-lint at .#checks covers render-correctness. T94 CARE: rio.tlsVolume takes arg (secret-name) — self-guard needs root context, not the arg; dict-signature change or 2-list at dispatch. T95+T96 depend on P0230 (DONE — RebalancerConfig + spawn_task + compute_cutoffs exist; discovered_from=230-review). T95: struct doc-comment :40-43 says 'config-driven via scheduler.toml [rebalancer]' but no Deserialize derive + spawn_task hardcodes ::default() at :326 — doc-comment LIES. Paired with P0295-T58 (doc-side fix). T96: compute_cutoffs :144 durations[len()-1] wraps on empty — guarded by min_samples=500 default today, becomes reachable once T95 exposes to TOML. P0230's own DISPATCH NOTE at :9 called this out, never applied. T95 soft-conflict P366 (touches spawn_task :350+ for gauge re-emit; T95 touches :326 ::default() + sig change — non-overlapping, both additive to same fn). T97 depends on P0341 (DONE — the CLAUDE.md example block at :190-197 arrived with it; discovered_from=341-review). T97 IRONIC SELF-FIX: P0341's own convention doc demonstrates the same-line-trailing pattern it forbids (tracey parses col-0 only). T98 depends on P0354+P0359 (DONE — Rule::new().message() pattern + :75-81 rationale comment exist; discovered_from=bughunt-mc133). T98 is the pre-existing sibling batch to T11's seccomp CEL messages — same fix shape, three bare-string :137/:183/:401 sites. T99 depends on P0285 (DONE — disruption.rs watcher + builders.rs:59 label literal exist; discovered_from=285-review). T99 soft-conflicts P0370 (spawn_periodic — touches disruption.rs for option-c inline-biased annotation; T99 touches :48 const-hoist; non-overlapping). T100 depends on P0360 (UNIMPL — nonpriv variant + DS bring-up path arrive with it; the 180s timeout is pre-existing but only becomes tight under P0360's variant; discovered_from=360-review). T101 depends on P0285 (DONE — drain_worker call at disruption.rs:~125 exists; discovered_from=285-review). T101 serves r[obs.metric.controller] (adds counter row to spec table + emit + register-test — same parity shape as T26/T47/T50). T101 soft-conflicts P0311-T33 (adds all_histograms_have_bucket_config to controller tests — both additive to metrics_registered.rs, non-overlapping test-fns). T102+T103 depend on P0233 (DONE — WPS reconciler + child_name exist; discovered_from=233-review). T102 is 2-word edit in describe-strings; T103 signature change Result<String> with 2 caller updates. T104+T105 depend on P0273 (DONE — dashboard-gateway templates exist; discovered_from=273-review). T105 soft-dep P0371 (adds the plan the TODO tags point at — if P0371 dispatches first, it deletes both comments and T105 is moot). T106 depends on P0340 (UNIMPL — seed_tenant helper touches create_tenant path; const useful for both; discovered_from=340-review). T107 depends on P0362 (DONE — the P0362 review found the admin/mod.rs:388 dup; discovered_from=362-review). T108 no hard-dep (consts exist at rio-proto:25,43 + jwt_interceptor:68; gateway build.rs:372 literal predates TENANT_TOKEN_HEADER const; discovered_from=consol-mc145). T108 soft-dep P0287 (TRACE_ID_HEADER landed there — same consts-section parity). T109 depends on P0362 (DONE — submit_build_grpc helper sits in same preamble; discovered_from=consol-mc145). T109 touches lifecycle.nix+scheduling.nix (both HOT — lifecycle=40+ plans, scheduling=20+). Helper extraction is additive at preamble; 6 migrations are subtractive at scattered lines; ~-40L net. T110 no hard-dep (rio-impl-merger.md session-cached; lands next worktree-add; discovered_from=consol-mc145). T110 distills ci-gate-fastpath-precedent.md into merger-agent-embedded decision tree — mergers stop re-deriving."}
```

**Depends on:** [P0222](plan-0222-grafana-dashboards.md) — merged at [`6b723def`](https://github.com/search?q=6b723def&type=commits). T1 (harness regex) has no dep.

**Conflicts with:** `infra/helm/grafana/*.json` freshly created by P0222; no other UNIMPL plan touches them. `justfile` is low-traffic. `.claude/lib/onibus/models.py` — [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) T2 touches it too (LockStatus cleanup), different section. `.claude/lib/onibus/build.py` — T10+T13 both touch it; [P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) does not. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T6 adds asserts to `cel_rules_in_schema` (test fn), T11 here adds messages to the derive attrs (struct) — different sections, same file. [`scheduler.md`](../../docs/src/components/scheduler.md) count=20 — T25 edits `:449-451`, T27 edits `:254/:258/:262`; non-overlapping. [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T2-path-B may ALSO edit the `r[sched.timeout.per-build]` text — if it dispatches, sequence T25 BEFORE it (T25 is a definite correction; 0329-T2-B is conditional on probe outcome). [`observability.md`](../../docs/src/observability.md) count=21 — T26 inserts one row at `:114` and edits `:116`; additive. [`flake.nix`](../../flake.nix) count=31 — T29 is 4 lines in the `tracey-validate` block at `:398-416`; no other batch task touches that block. Low conflict risk across the board.
