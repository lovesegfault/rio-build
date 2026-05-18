#!/usr/bin/env bash
# Emit GHA matrix outputs for ci.yml, eliding entries already in the
# binary cache.
#
# The flake exposes `.#githubActions.{checks,fuzz,vm-test,coverage}` as
# flat name→drv attrsets. nix-eval-jobs --check-cache-status probes
# configured substituters (rio-nix-cache S3) for each and we drop
# anything already cached. coverage is filtered too: the build half
# (KVM-heavy VM runs) skips when cached; the upload half is a separate
# always-run job that substitutes each lcov and posts to Codecov, so a
# cache hit still gets per-commit reporting without spinning up a KVM
# runner.
#
# Dry-run locally:
#   GITHUB_OUTPUT=/dev/stdout bash .github/scripts/gen-matrix.sh
set -euo pipefail

: "${GITHUB_OUTPUT:?GITHUB_OUTPUT must be set}"

NEJ=$(nix build .#nix-eval-jobs --no-link --print-out-paths)/bin/nix-eval-jobs

# ARC pods are CFS-quota'd, not cpuset-pinned, so nproc reports the
# host core count (often 32+). Each nix-eval-jobs worker is a full
# evaluator (~500MB-1GB peak for NixOS module evals); uncapped that
# OOM-thrashes a 4-8GB pod. 8 is the sweet spot — enough to keep the
# 51 NixOS-config evals saturated without blowing memory.
WORKERS=${NEJ_WORKERS:-8}
echo "nix-eval-jobs: ${WORKERS} workers, ~2-5min cold, streaming attrs as they complete:" >&2

# Stream-eval all four matrices. --force-recurse: matrix.<kind> are
# plain attrsets (no recurseIntoAttrs marker). One JSONL line per leaf:
#   {"attr":"checks.clippy-rio-nix","cacheStatus":"cached"|"local"|"notBuilt",...}
# or on eval failure:
#   {"attr":"checks.broken","error":"..."}
# tee→jq to stderr makes progress visible in the GHA log; the file
# is what we actually process.
nej_out=$(mktemp)
"$NEJ" \
  --flake .#githubActions \
  --force-recurse \
  --check-cache-status \
  --workers "$WORKERS" \
  2> >(grep -Ev '^(warning:|error \(ignored\):)' >&2 || true) \
  | tee "$nej_out" | jq -rc '"  " + .attr + " " + (.cacheStatus // "ERROR")' >&2

# Fail hard on any per-attr eval error. Surfacing it here (rather than
# letting the downstream job's `nix build` rediscover it) means a single
# red gen-matrix instead of N green jobs masking one missing red one —
# an eval error on a filtered matrix would otherwise silently drop the
# attr and ci-gate would pass.
errors=$(jq -sc '[.[] | select(has("error"))]' "$nej_out")
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
' "$nej_out")

# {name: outPath} for the upload job. Passing store paths (already
# evaluated here) means coverage-upload does zero flake eval — just
# parallel `nix-store -r`. A target that failed to eval would have
# already tripped the hard-error block above; a target that eval'd
# but failed to BUILD has a valid outPath here that simply won't be
# in the cache, which the upload script handles as a per-entry miss.
coverage_paths=$(jq -sc '
  map(select(.attr | startswith("coverage."))
      | {(.attr | ltrimstr("coverage.")): .outputs.out})
  | add // {}
' "$nej_out")

# Visibility: list what was elided so reviewers can see it in the
# gen-matrix log without digging into per-job absence.
skipped=$(jq -sc '[.[] | select(.cacheStatus == "cached") | .attr]' "$nej_out")
echo "::notice::cached (skipped): $skipped"
echo "::notice::building: $filtered"

for m in checks fuzz vm-test coverage; do
  echo "$m=$(jq -c --arg m "$m" '.[$m] // []' <<<"$filtered")" >> "$GITHUB_OUTPUT"
done
echo "coverage-paths=$coverage_paths" >> "$GITHUB_OUTPUT"
