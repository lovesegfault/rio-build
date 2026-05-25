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

  The comment field (`team-infra` above) is the tenant name. The gateway reads it from the server-side matched entry (NOT from the client's key — SSH auth sends raw key data only) and passes it through to the scheduler, which resolves it to a UUID via the `tenants` table. See #cross-link("/spec/components/gateway.typ")[`gw.auth.tenant-from-key-comment`] and #cross-link("/spec/components/scheduler.typ")[`sched.tenant.resolve`].

  *Dual-mode auth* is permanent — operator choice per-deployment:

  - *JWT mode* (`jwt.enabled: true` in Helm values, `RIO_JWT__KEY_PATH` set): gateway resolves the tenant name to a UUID via a scheduler `ResolveTenant` RPC at SSH-auth time, mints a short-lived ed25519-signed JWT with the UUID in `sub`, and attaches it as `x-rio-tenant-token` on every internal gRPC call. Scheduler/store verify signature+expiry. `RIO_JWT__REQUIRED=true` makes mint failure (scheduler unreachable, unknown tenant) reject SSH auth; default `false` degrades to the fallback path.
  - *Fallback mode* (default — no JWT config): tenant name passes through `SubmitBuildRequest.tenant_name` unsigned. Simpler; no cryptographic binding between the SSH key and downstream gRPC calls. Adequate for single-trust-zone deployments.

  See #cross-link("/spec/components/gateway.typ")[`gw.jwt.dual-mode`] and #cross-link("/spec/system/tenancy.typ")[Multi-Tenancy].

+ Configure the Nix client to use the key:
  ```bash
  # In ~/.config/nix/nix.conf or via NIX_SSHOPTS
  export NIX_SSHOPTS="-i ~/.ssh/rio_key"
  ```

== Client-Side `nix.conf`

For remote store usage, no special client configuration is needed beyond SSH access, with one exception: SSH connection multiplexing.

#warning[
  If your `~/.ssh/config` enables `ControlMaster auto` with a `ControlPath` (a common global setting), every Nix-spawned `ssh` multiplexes through your mux master --- and running many concurrent Nix invocations against the gateway (`nix-fast-build`, CI fan-out) will eventually fail with `protocol mismatch, got 'started...'`. When the master fails to open a session for any reason, OpenSSH silently falls back to a direct connection, and the `LocalCommand` Nix passes to every `ssh` fires there --- its `started` output lands in front of the worker-protocol handshake. Opt the gateway out of multiplexing with a Host stanza:

  ```
  Host gw.rio-build.com
      ControlMaster no
      ControlPath none
  ```

  Both lines are required: `ControlMaster no` only stops `ssh` from _creating_ a master; it will still _use_ an existing socket unless `ControlPath none` is also set. Separate connections are also better for throughput --- the gateway processes all channels on one connection serially, and the load balancer can only spread connections (not channels) across gateway replicas.
]

= Direct Use: Interactive Developer Builds

The simplest integration --- a developer runs builds directly:

```bash
# Remote store mode (full DAG visibility, optimal scheduling)
nix build --store ssh-ng://rio:2222 .#myPackage

# Remote builder mode (per-derivation delegation, works with any Nix setup)
nix build --builders 'ssh-ng://rio:2222 x86_64-linux' .#myPackage
```
