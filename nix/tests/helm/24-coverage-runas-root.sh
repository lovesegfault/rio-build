# Coverage mode MUST render explicit `runAsUser: 0` on all four
# control-plane Deployments. Guards against the silent zero-coverage
# regression where someone "simplifies" rio.podSecurityContext's
# coverage branch back to rendering nothing: omitting securityContext
# does NOT yield root — the runtime falls back to the image's
# config.User (65532, baked in nix/docker.nix nonrootUser), so the
# pod runs as 65532, profraw atexit flush hits EACCES on the
# root-owned 0755 hostPath, and coverage silently goes to zero.
#
# Paired runtime guard: nix/tests/common.nix collectCoverage hard-
# fails on zero pod profraws on the k3s server.

out=$TMPDIR/cov-on.yaml
helm template rio . \
  --set coverage.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

# Four control-plane Deployments. rio-builder is a Job-spawning
# controller (no Deployment), and its image has no config.User anyway
# (root for FUSE).
deps="rio-gateway rio-scheduler rio-store rio-controller"

n=0
for dep in $deps; do
  uid=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
        | .spec.template.spec.securityContext.runAsUser" "$out")
  test "$uid" = "0" || {
    echo "FAIL: $dep coverage-mode runAsUser=$uid (want 0) — image config.User=65532 will apply, profraw flush EACCES" >&2
    exit 1
  }
  n=$((n + 1))
done

# r38-style count guard (§Stability-tests "nothing → no change"): if
# the for-loop body never ran (deps list empty / yq filter rotted),
# the test would pass vacuously.
test "$n" -eq 4 || {
  echo "FAIL: asserted $n Deployments, expected 4 — assertion vacuous" >&2
  exit 1
}

# Negative: production (coverage.enabled=false) MUST keep PSA-
# restricted runAsUser: 65532. Catches an over-broad edit that drops
# the else-branch.
prod=$TMPDIR/cov-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$prod"
for dep in $deps; do
  uid=$(yq -N "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
        | .spec.template.spec.securityContext.runAsUser" "$prod")
  test "$uid" = "65532" || {
    echo "FAIL: $dep production runAsUser=$uid (want 65532) — PSA-restricted regressed" >&2
    exit 1
  }
done

echo "OK"
