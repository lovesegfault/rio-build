# Dev overlay gateway SSH setup

```sh
just dev apply
```

That's it. The recipe reads `RIO_SSH_PUBKEY` from `.env.local`
(default `~/.ssh/id_ed25519.pub`), validates it, strips the comment,
writes it here, and runs `kubectl apply -k`.

## Why the comment is stripped

The gateway maps the authorized_keys comment → `tenant_name`
(`rio-gateway/src/server.rs:211`). The scheduler resolves that against
the `tenants` table — unknown name → `InvalidArgument` at build submit.
Your key's default comment is `user@hostname`, and there's no such
tenant. Empty comment → single-tenant mode → builds just work.

To opt into a tenant, set `RIO_SSH_TENANT` in `.env.local` and create
the tenant first:

```sh
kubectl -n rio-system exec deploy/rio-scheduler -- rio-cli create-tenant my-team
```

## Raw `kubectl apply -k` fails without the key file

Intentional. `kustomization.yaml`'s `secretGenerator` reads
`ssh/authorized_keys`; if missing, `kustomize build` errors out —
better than a pod stuck in `ContainerCreating`. The file is
`.gitignore`'d (per-user).

## Host key

The gateway auto-generates an ed25519 host key at first start and saves
it to an emptyDir volume (persists across container restarts, not pod
rescheduling). You'll get a host-key-changed warning on reschedule —
fine for dev.
