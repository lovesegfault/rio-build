#!/usr/bin/env bash
# Regenerate nix/tests/helm/rendered-key-population.txt — the
# [GEN-SET] baseline the helm-lint driver diffs against the canonical
# default + karpenter profile renders (merged_bug_004's
# adjacency-drift census; see strict_decode.py).
#
# Run from the repo root, with nix available (the dev shell has it).
# The chart's postgresql subchart and a python3+pyyaml interpreter are
# built via the flake's own pinned inputs, so this script and the
# helm-lint driver always agree on tool versions.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

pg_chart=$(nix build --impure --no-link --print-out-paths --expr "
  let f = builtins.getFlake \"$repo_root\";
  in (import $repo_root/nix/helm-charts.nix {
    nixhelm = f.inputs.nixhelm;
    system = builtins.currentSystem;
  }).postgresql")
pyenv=$(nix build --impure --no-link --print-out-paths --expr "
  let f = builtins.getFlake \"$repo_root\";
  in (import f.inputs.nixpkgs { system = builtins.currentSystem; })
    .python3.withPackages (ps: [ ps.pyyaml ])")

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
cp -r infra/helm/rio-build "$workdir/chart"
chmod -R +w "$workdir/chart"
mkdir -p "$workdir/chart/charts"
ln -sfn "$pg_chart" "$workdir/chart/charts/postgresql"

render() {
  helm template rio "$workdir/chart" "$@"
}

render --set global.image.tag=test > "$workdir/kp-default.yaml"
render \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test > "$workdir/kp-karpenter.yaml"

out=nix/tests/helm/rendered-key-population.txt
{
  "$pyenv/bin/python3" nix/tests/helm/strict_decode.py keys \
    "$workdir/kp-default.yaml" | sed 's/^/default\t/'
  "$pyenv/bin/python3" nix/tests/helm/strict_decode.py keys \
    "$workdir/kp-karpenter.yaml" | sed 's/^/karpenter\t/'
} > "$out"
echo "wrote $out ($(wc -l < "$out") lines)" >&2
