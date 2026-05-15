#import "/lib/rio.typ": *
#show: rio.with(domains: none)

= Authentication Setup

== SSH Key Configuration

+ Generate an ed25519 key pair for each user/team:
  ```bash
  ssh-keygen -t ed25519 -f ~/.ssh/rio_key -N ""
  ```

+ Add the public key to the gateway's `authorized_keys`:
  ```
  ssh-ed25519 AAAA... team-infra
  ```

  The comment field (`team-infra` above) is the tenant name. The gateway reads it from the server-side matched entry (NOT from the client's key — SSH auth sends raw key data only) and passes it through to the scheduler, which resolves it to a UUID via the `tenants` table. See #link("./components/gateway.md")[`gw.auth.tenant-from-key-comment`] and #link("./components/scheduler.md")[`sched.tenant.resolve`].

  *Dual-mode auth* is permanent — operator choice per-deployment:

  - *JWT mode* (`jwt.enabled: true` in Helm values, `RIO_JWT__KEY_PATH` set): gateway resolves the tenant name to a UUID via a scheduler `ResolveTenant` RPC at SSH-auth time, mints a short-lived ed25519-signed JWT with the UUID in `sub`, and attaches it as `x-rio-tenant-token` on every internal gRPC call. Scheduler/store verify signature+expiry. `RIO_JWT__REQUIRED=true` makes mint failure (scheduler unreachable, unknown tenant) reject SSH auth; default `false` degrades to the fallback path.
  - *Fallback mode* (default — no JWT config): tenant name passes through `SubmitBuildRequest.tenant_name` unsigned. Simpler; no cryptographic binding between the SSH key and downstream gRPC calls. Adequate for single-trust-zone deployments.

  See #link("./components/gateway.md")[`gw.jwt.dual-mode`] and #link("./multi-tenancy.md")[Multi-Tenancy].

+ Configure the Nix client to use the key:
  ```bash
  # In ~/.config/nix/nix.conf or via NIX_SSHOPTS
  export NIX_SSHOPTS="-i ~/.ssh/rio_key"
  ```

== Client-Side `nix.conf`

For remote store usage, no special client configuration is needed beyond SSH access.

= Direct Use: Interactive Developer Builds

The simplest integration --- a developer runs builds directly:

```bash
# Remote store mode (full DAG visibility, optimal scheduling)
nix build --store ssh-ng://rio:2222 .#myPackage

# Remote builder mode (per-derivation delegation, works with any Nix setup)
nix build --builders 'ssh-ng://rio:2222 x86_64-linux' .#myPackage
```
