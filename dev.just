# Dev overlay recipes (local k3s/kind). Invoked as `just dev <recipe>`.
#
# Settings do NOT inherit from the root justfile — `set shell` here
# is load-bearing (without it: no pipefail, silent pipe failures).
set shell := ["bash", "-euo", "pipefail", "-c"]

# Materialize authorized_keys from RIO_SSH_PUBKEY (.env.local).
# Shebang recipe: `source` + call must share a shell (just runs
# each indented line in a fresh shell otherwise).
[private]
_ssh-key:
    #!/usr/bin/env bash
    set -euo pipefail
    source infra/lib/ssh-key.sh
    rio_authorized_keys > infra/k8s/overlays/dev/ssh/authorized_keys

# Install SSH key + kubectl apply the dev overlay
apply: _ssh-key
    kubectl apply -k infra/k8s/overlays/dev/

# kubectl delete the dev overlay
delete:
    kubectl delete -k infra/k8s/overlays/dev/ --ignore-not-found
