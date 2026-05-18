#!/usr/bin/env bash
# Emit GHA matrix outputs for ci.yml, eliding entries already in the
# binary cache.
#
# The flake exposes `.#githubActions.matrix.{checks,fuzz,vm-test,coverage}`
# as flat name→drv attrsets. We deep-evaluate the first three with
# nix-eval-jobs --check-cache-status (probes configured substituters,
# i.e. rio-nix-cache S3) and drop anything already cached. coverage is
# never filtered: those jobs upload lcov to Codecov keyed on commit SHA,
# so a cache hit still needs a runner.
#
# Dry-run locally:
#   GHA=githubActions GITHUB_OUTPUT=/dev/stdout bash .github/scripts/gen-matrix.sh
set -euo pipefail

: "${GHA:?GHA env (flake attr prefix) must be set}"
: "${GITHUB_OUTPUT:?GITHUB_OUTPUT must be set}"

NEJ=$(nix build ".#${GHA}.nix-eval-jobs" --no-link --print-out-paths)/bin/nix-eval-jobs

# Stream-eval checks/fuzz/vm-test. --force-recurse: matrix.<kind> are
# plain attrsets (no recurseIntoAttrs marker). One JSONL line per leaf:
#   {"attr":"checks.clippy-rio-nix","cacheStatus":"cached"|"local"|"notBuilt",...}
# or on eval failure:
#   {"attr":"checks.broken","error":"..."}
nej_out=$("$NEJ" \
  --flake ".#${GHA}.matrix" \
  --force-recurse \
  --check-cache-status \
  --workers "$(nproc)" \
  --select 'm: builtins.removeAttrs m ["coverage"]' \
  2> >(grep -Ev '^(warning:|error \(ignored\):)' >&2 || true))

# Fail hard on any per-attr eval error. Surfacing it here (rather than
# letting the downstream job's `nix build` rediscover it) means a single
# red gen-matrix instead of N green jobs masking one missing red one —
# an eval error on a filtered matrix would otherwise silently drop the
# attr and ci-gate would pass.
errors=$(jq -sc '[.[] | select(has("error"))]' <<<"$nej_out")
if [[ "$errors" != "[]" ]]; then
  echo "::error::nix-eval-jobs reported eval failures:"
  jq -r '.[] | "  \(.attr): \(.error)"' <<<"$errors" >&2
  exit 1
fi

# Keep everything not already in a substituter. "local" is kept on
# purpose: CI runners have an empty store so it never appears there,
# and keeping it makes local dry-runs reflect what a never-pushed
# branch would build. attr is "kind.name"; split on first dot only.
filtered=$(jq -sc '
  map(select(.cacheStatus != "cached")
      | .attr | split(".") | {kind: .[0], name: (.[1:] | join("."))})
  | group_by(.kind)
  | map({(.[0].kind): map(.name)})
  | add // {}
' <<<"$nej_out")

# coverage: always all entries. Cheap eval (attrNames doesn't force
# the NixOS-module-heavy values).
coverage=$(nix eval ".#${GHA}.matrix.coverage" --json --apply builtins.attrNames)

# Visibility: list what was elided so reviewers can see it in the
# gen-matrix log without digging into per-job absence.
skipped=$(jq -sc '[.[] | select(.cacheStatus == "cached") | .attr]' <<<"$nej_out")
echo "::notice::cached (skipped): $skipped"
echo "::notice::building: $filtered"
echo "::notice::coverage (always): $coverage"

for m in checks fuzz vm-test; do
  echo "$m=$(jq -c --arg m "$m" '.[$m] // []' <<<"$filtered")" >> "$GITHUB_OUTPUT"
done
echo "coverage=$coverage" >> "$GITHUB_OUTPUT"
