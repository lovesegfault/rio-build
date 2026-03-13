#!/usr/bin/env bash
# Split multi-doc CRD YAML into one file per CRD, named by metadata.name.
# The chart's crds/ gets them (Helm installs that dir on `helm install`,
# never upgrades — so the Argo rio-crds Application syncs infra/helm/crds/
# separately with ServerSideApply).
#
# Usage: nix build .#crds && ./scripts/split-crds.sh result
set -euo pipefail

src="${1:?usage: $0 <crds-yaml>}"
out="$(git rev-parse --show-toplevel)/infra/helm/crds"

rm -f "$out/"*.yaml

python3 - "$src" "$out" <<'PY'
import sys, yaml, pathlib
src, out = sys.argv[1], pathlib.Path(sys.argv[2])
with open(src) as f:
    for doc in yaml.safe_load_all(f):
        if doc is None:
            continue
        name = doc["metadata"]["name"]
        (out / f"{name}.yaml").write_text(yaml.dump(doc, sort_keys=False))
        print(f"  {name}.yaml")
PY

# No copy under rio-build/crds/ — Helm's crds/ dir semantics are wrong
# for a dev-phase project (install-only, never upgraded). The Argo
# rio-crds Application syncs infra/helm/crds/ with ServerSideApply so
# schema changes land. For local `just dev apply`, kubectl apply these
# before helm install.

echo "wrote CRDs to $out/"
