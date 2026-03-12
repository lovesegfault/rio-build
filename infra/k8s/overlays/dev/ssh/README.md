# Dev overlay gateway SSH setup

The gateway requires an `authorized_keys` file listing SSH public keys
allowed to connect via `ssh-ng://`. This is **per-user** — do NOT commit
your key.

## Quick setup

```sh
cp ~/.ssh/id_ed25519.pub infra/k8s/overlays/dev/ssh/authorized_keys
kubectl apply -k infra/k8s/overlays/dev/
```

## How it works

`kustomization.yaml`'s `secretGenerator` reads `ssh/authorized_keys` and
creates the `rio-gateway-ssh` Secret at build time. If the file is missing,
`kustomize build` fails loudly (better than a pod stuck in
`ContainerCreating` on a missing Secret).

`ssh/authorized_keys` is `.gitignore`'d — everyone uses their own key.

## Host key

The gateway auto-generates an ed25519 host key at first start and saves it
to an emptyDir volume (persists across container restarts, not pod
rescheduling). You'll get a host-key-changed warning if the pod is
rescheduled — fine for dev.
