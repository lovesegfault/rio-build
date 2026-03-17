# Remediation 05: Coverage session drop — 15→1

**Parent finding:** [§1.5](../phase4a.md#15-coverage-session-drop-151)
**Blast radius:** dev-signal only (no runtime impact)
**Effort:** ~30 min implementation + one CI cycle to verify

---

## Ground truth (measured, not assumed)

```console
$ nix eval .#githubActions.matrix.coverage --json --apply 'builtins.attrNames' | jq length
15
```

Actual breakdown is **1 `unit` + 14 `vm-*`** — the §1.5 text "6 unit + 9 VM" is
wrong on the split (correct on the total). There is exactly one unit-lcov entry;
the fragment explosion was all on the VM side.

Also stale: `codecov.yml:3` still claims "Nine coverage flags upload per PR."

---

## 1. `codecov.yml` — add `after_n_builds`

The existing `wait_for_ci: true` waits for GitHub's commit-status to go green;
it does **not** wait for Codecov's own upload-ingest pipeline. With 15 uploads
arriving within a ~2 s window after `ci-gate` fires, Codecov's report processor
races itself and last-writer-wins. `after_n_builds` is the only knob that gates
on upload count server-side.

```diff
--- a/codecov.yml
+++ b/codecov.yml
@@ -1,17 +1,25 @@
 # Codecov configuration for rio-build.
 #
-# Nine coverage flags upload per PR (unit + vm-phase*), one per matrix
+# Fifteen coverage flags upload per PR (1 unit + 14 vm-*), one per matrix
 # entry in the `coverage` job (.github/workflows/ci.yml). Paths in the
 # lcov files are already repo-relative — nix/coverage.nix strips the
 # sandbox prefix before upload, so no `fixes:` block is needed here.

 codecov:
-  # Wait for GitHub to report CI complete before posting the comment
-  # or status checks. ci-gate fans in all coverage jobs, so "CI done"
-  # is a reliable signal that all flags have uploaded. Avoids
-  # hardcoding a flag count that would go stale as vm-phase* grows.
   notify:
+    # Block status/comment until all 15 uploads are ingested. The previous
+    # approach (wait_for_ci alone, no count) was chosen to avoid a value
+    # that goes stale as vm-* fragments grow — but at 15 parallel uploads
+    # the server-side ingest race is guaranteed: wait_for_ci gates on
+    # GitHub's commit status, not on Codecov's own ingest queue, so 14 of
+    # 15 uploads were silently dropped. after_n_builds is the only knob
+    # that gates on upload count. Drift is now caught by the
+    # codecov-matrix-sync check in flake.nix — updating the matrix without
+    # bumping this value fails `nix flake check`.
+    after_n_builds: 15
+    # Keep wait_for_ci as a floor: if a coverage job is skipped/cancelled
+    # we still want to hold until CI is done rather than wait forever.
     wait_for_ci: true
```

---

## 2. `.github/workflows/ci.yml` — harden the upload step

Current step at `ci.yml:125-129`:

```yaml
      - uses: codecov/codecov-action@v5
        with:
          files: result
          flags: ${{ matrix.name }}
          use_oidc: true
```

Two gaps:

- The action defaults to `fail_ci_if_error: false`. This is why all 15 jobs
  showed green even when the server dropped their payload — `"Process Upload
  complete"` is the **CLI** log, not a server ACK. With `true`, a non-2xx from
  the ingest endpoint fails the job.
- The action defaults to running a repo-wide `coverage.*` file discovery even
  when `files:` is set. We pass exactly one lcov; discovery is pure overhead
  and masks a broken `result` symlink by silently falling back.

```diff
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -125,6 +125,10 @@
       - uses: codecov/codecov-action@v5
         with:
           files: result
           flags: ${{ matrix.name }}
           use_oidc: true
+          # Fail the job if Codecov's ingest endpoint rejects the upload.
+          # Without this, all 15 jobs go green even when 14 are dropped.
+          fail_ci_if_error: true
+          # `files:` is exhaustive; don't fall back to repo-wide discovery.
+          disable_search: true
```

---

## 3. Matrix-derived N — investigation & decision

### Can Codecov interpolate env vars?

**No.** `codecov.yml` is fetched server-side from the commit tree and parsed as
static YAML. It never passes through GHA's `${{ }}` engine, and Codecov's own
parser has no template syntax ([codecovyml-reference] lists `after_n_builds` as
a bare `Int`, default `1`, with no mention of substitution). The §1.5 sketch of
`CODECOV_EXPECTED_UPLOADS: ${{ strategy.job-total }}` is useful for the
**monitoring** check below, but cannot feed `codecov.yml`.

[codecovyml-reference]: https://docs.codecov.com/docs/codecovyml-reference

### Can Nix generate `codecov.yml`?

Yes, but it would have to be checked in (Codecov reads from the commit tree,
not a build artifact), which means a generate-then-`diff`-against-committed
pattern. That's a lot of machinery for one integer.

### Decision: Nix drift check (simplest fix that closes the gap)

Hardcode `15`, add a flake check that fails when it drifts. The flake already
knows the matrix:

```nix
# flake.nix — add to cargoChecks or a new attrset under checks.*
codecov-matrix-sync =
  let
    expected = builtins.length (builtins.attrNames githubActions.matrix.coverage);
    # Single-line grep is fine here; if someone rewrites codecov.yml
    # to split the key across lines, the check fails closed (no match
    # → empty string → toInt error) rather than passing silently.
    declared = lib.toInt (lib.head (
      builtins.match ".*after_n_builds: ([0-9]+).*"
        (builtins.readFile ../codecov.yml)
    ));
  in
  assert lib.assertMsg (expected == declared) ''
    codecov.yml after_n_builds=${toString declared} but coverage matrix has ${toString expected} entries.
    Update codecov.yml → codecov.notify.after_n_builds to ${toString expected}.
  '';
  pkgs.emptyFile;
```

This is pure eval (no build), runs in `gen-matrix`'s `nix eval` already, and
gives a precise actionable error. Drift becomes impossible to merge.

---

## 4. Verification

After landing, push a no-op commit (or re-run CI on HEAD) and query:

```bash
sha=$(git rev-parse HEAD)
curl -sf \
  -H "Authorization: Bearer $CODECOV_TOKEN" \
  "https://api.codecov.io/api/v2/github/lovesegfault/rio-build/commits/$sha" \
  | jq '{
      sessions: (.report.sessions | length),
      coverage: .totals.coverage,
      flags:    [.report.sessions[].flags[0]] | sort
    }'
```

Pass criteria:

- `sessions == 15`
- `coverage` within ±1 % of the local `nix-build-remote -- .#coverage-full`
  number (~93.7 % as of `7c437e1`)
- `flags` matches `nix eval .#githubActions.matrix.coverage --apply builtins.attrNames`

---

## 5. Monitoring — catch future regressions of the same class

Add a final step to `ci-gate`. It already fans in on `coverage`, so by the
time it runs all uploads have been **submitted** (Codecov ingest is typically
< 30 s after submission). `strategy.job-total` isn't available outside the
matrix job, so pull the expected count from `gen-matrix`'s JSON output, which
`ci-gate` already `needs:`:

```yaml
  # in ci-gate, after alls-green
  - name: Assert Codecov session count
    env:
      EXPECTED: ${{ fromJSON(needs.gen-matrix.outputs.coverage) && toJSON(fromJSON(needs.gen-matrix.outputs.coverage)) }}
      CODECOV_TOKEN: ${{ secrets.CODECOV_TOKEN }}
    run: |
      set -euo pipefail
      want=$(jq 'length' <<<"$EXPECTED")
      for i in $(seq 1 10); do
        got=$(curl -sf -H "Authorization: Bearer $CODECOV_TOKEN" \
          "https://api.codecov.io/api/v2/github/${{ github.repository }}/commits/${{ github.sha }}" \
          | jq '.report.sessions | length')
        [[ "$got" == "$want" ]] && exit 0
        echo "codecov sessions=$got want=$want (attempt $i/10)"; sleep 6
      done
      echo "::error::codecov retained $got sessions, expected $want"; exit 1
```

This makes the drop **visible in the GHA check** rather than requiring someone
to notice a suspiciously low percentage on the Codecov dashboard.

**Defer decision:** the Nix drift check (§3) catches *config* drift at eval
time; this check catches *server-side* drops at runtime. They cover different
failure modes. Ship the drift check with the fix; evaluate whether the runtime
check is worth the extra `CODECOV_TOKEN` secret exposure after a few weeks of
green.

---

## Checklist

- [ ] `codecov.yml`: bump header comment 9→15, replace `wait_for_ci` rationale
      comment, add `after_n_builds: 15`
- [ ] `ci.yml:125`: add `fail_ci_if_error: true`, `disable_search: true`
- [ ] `flake.nix`: add `codecov-matrix-sync` assertion to `checks.*`
- [ ] Push, wait for CI, run §4 curl — assert `sessions == 15`
- [ ] Decide on §5 runtime check (follow-up, not blocking)
