# Plan 494: xtask k8s cli-tunnel — run rio-cli locally, not in-pod

The current admin-CLI path is `kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`
(documented at [`deployment.md:102`](../../docs/src/deployment.md); wired at
[`kube.rs:155`](../../xtask/src/kube.rs) `run_in_scheduler`). This forces
the scheduler container image to bundle rio-cli AND every runtime dependency
an operator might pipe it through — `jq` for `--json`, `column` for table
rendering, whatever's next. Each addition widens the control-plane attack
surface in direct contradiction to `r[sec.psa.control-plane-restricted]`'s
spirit: the scheduler is a gRPC server, full stop.

[P0489](plan-0489-flake-vm-lifecycle-recovery-pod-cycle.md) hit the mirror
image — a test VM lacked `jq` and the in-pod invocation failed. Both
directions of this problem stem from the same coupling.

This plan adds `cargo xtask k8s cli` (alias `cli-tunnel`) as a sibling of
`rsb`/`cpt` ([`mod.rs:177`](../../xtask/src/k8s/mod.rs)): open gRPC tunnels
to scheduler + store, fetch the mTLS client cert into a tempdir, exec the
LOCAL rio-cli binary with `RIO_SCHEDULER_ADDR`/`RIO_STORE_ADDR`/`RIO_TLS__*`
set. The scheduler image can then drop rio-cli and its transitive deps.

**Key enabler:** the `rio-scheduler-tls` Secret already has `localhost` in
its SANs ([`cert-manager.yaml:118`](../../infra/helm/rio-build/templates/cert-manager.yaml))
— put there precisely so in-pod rio-cli could talk to `localhost:9001`.
That same SAN makes a port-forwarded `localhost:9001` pass tonic's
`:authority` check when the cert is fetched client-side. No new Certificate
resource needed.

## Tasks

### T1 — `feat(xtask):` add Provider::tunnel_grpc for scheduler+store port-forward

Extend [`provider.rs:93`](../../xtask/src/k8s/provider.rs) with a second
tunnel method. The existing `tunnel()` stays SSH-specific (gateway:22,
waits for banner); `tunnel_grpc()` forwards scheduler:9001 + store:9002
and waits for TCP accept.

```rust
/// Open port-forwards to scheduler:9001 and store:9002, waiting until
/// both accept TCP. Drop the guards to tear down. Returns
/// (scheduler_guard, store_guard, sched_port, store_port).
async fn tunnel_grpc(&self, sched_port: u16, store_port: u16)
    -> Result<(ProcessGuard, ProcessGuard)>;
```

Implementations:
- **k3s/kind:** two `kubectl port-forward svc/rio-scheduler <local>:9001`
  + `svc/rio-store <local>:9002` children, each wrapped in `ProcessGuard`.
  Readiness poll: `TcpStream::connect` succeeds (no banner — gRPC has no
  greeting). Factor the spawn+poll into a `port_forward(svc, ns, local,
  remote)` helper in [`k3s/smoke.rs`](../../xtask/src/k8s/k3s/smoke.rs)
  so the existing gateway `tunnel()` can reuse it.
- **eks:** same `kubectl port-forward` (NOT SSM — the scheduler/store
  aren't behind the NLB; kubectl reaches them via the apiserver proxy,
  which `aws eks update-kubeconfig` already set up).

Namespace note: scheduler is in `rio-system`, store is in `rio-store`
(four-namespace split — [`main.rs:73`](../../rio-cli/src/main.rs) doc).
The port-forward commands need `-n <ns>` per service.

### T2 — `feat(xtask):` fetch_tls_to_tempdir — pull Secret keys to disk

New helper in [`kube.rs`](../../xtask/src/kube.rs), built on the existing
`get_secret_key` (line 127):

```rust
/// Fetch tls.crt, tls.key, ca.crt from a cert-manager Secret into a
/// tempdir. Returns (TempDir, cert_path, key_path, ca_path). TempDir
/// drop cleans up — hold it for the lifetime of the child process.
pub async fn fetch_tls_to_tempdir(
    client: &Client, ns: &str, secret: &str,
) -> Result<(TempDir, PathBuf, PathBuf, PathBuf)>;
```

cert-manager Secrets always populate `tls.crt`, `tls.key`, `ca.crt` —
fail fast if any key is absent (means cert-manager hasn't issued yet;
actionable error: "wait for Certificate {secret} Ready").

File perms: 0600 on key file (tonic doesn't check, but it's a private key
on the operator's laptop).

### T3 — `feat(xtask):` CliTunnel subcommand + with_cli_tunnel helper

New `K8sCmd::CliTunnel` variant + dispatch arm in
[`mod.rs`](../../xtask/src/k8s/mod.rs), mirroring `RemoteStoreBuild`:

```rust
#[command(visible_alias = "cli")]
CliTunnel {
    #[arg(long, default_value_t = 19001)]
    sched_port: u16,
    #[arg(long, default_value_t = 19002)]
    store_port: u16,
    /// Passed through to rio-cli.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    args: Vec<String>,
},
```

`with_cli_tunnel` helper (analog of `with_remote_store` at line 305):

```rust
async fn with_cli_tunnel<F>(p: &dyn Provider, sched: u16, store: u16, f: F)
    -> Result<()>
where F: FnOnce(&xshell::Shell) -> Result<()>,
{
    let client = kube::client().await?;
    let _guards = ui::step("tunnel scheduler+store", ||
        p.tunnel_grpc(sched, store)).await?;
    let (_dir, cert, key, ca) = ui::step("fetch mTLS cert", ||
        kube::fetch_tls_to_tempdir(&client, NS, "rio-scheduler-tls")).await?;

    let sh = sh::shell()?;
    let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}"));
    let _e2 = sh.push_env("RIO_STORE_ADDR",     format!("localhost:{store}"));
    let _e3 = sh.push_env("RIO_TLS__CERT_PATH", cert.to_string_lossy());
    let _e4 = sh.push_env("RIO_TLS__KEY_PATH",  key.to_string_lossy());
    let _e5 = sh.push_env("RIO_TLS__CA_PATH",   ca.to_string_lossy());
    f(&sh)
}
```

Dispatch runs `cargo run -p rio-cli --` (dev) or `nix run .#rio-cli --`
(depends on what's in PATH — prefer `rio-cli` if on PATH, fall back to
`cargo run -p rio-cli`). Use `sh::run_interactive` so streaming subcommands
(`logs`, `gc`) work.

### T4 — `refactor(xtask):` migrate smoke/status callers off run_in_scheduler

Three call sites to convert from in-pod exec to local-via-tunnel:

1. [`eks/smoke.rs:89`](../../xtask/src/k8s/eks/smoke.rs) `step_tenant` —
   `rio-cli create-tenant`
2. [`eks/smoke.rs:387`](../../xtask/src/k8s/eks/smoke.rs) `step_status` —
   `rio-cli status`
3. [`status.rs:123`](../../xtask/src/k8s/status.rs) — `rio-cli status`
   for the `xtask k8s status` display

Smoke-test consideration: the smoke phase macro already holds a gateway
tunnel (`_tunnel` at [`k3s/smoke.rs:27`](../../xtask/src/k8s/k3s/smoke.rs)).
Add a second `let _grpc = "grpc tunnel" => p.tunnel_grpc(...)` step before
`step_tenant`, pass the ports through. Cert fetch is per-invocation (cheap
— one Secret GET) so each step that needs rio-cli can re-fetch, OR fetch
once into a struct threaded through the phase. Prefer once-up-front for
clarity.

`run_in_scheduler` stays for now (NOT deleted) — there may be legitimate
in-pod-exec uses beyond rio-cli (debugging, `ps`, etc). Add a doc-comment
steering new callers toward cli-tunnel.

### T5 — `docs:` flip deployment.md primary path + rio-cli doc-comment

[`deployment.md:99-103`](../../docs/src/deployment.md): replace the
`kubectl exec` block with `cargo xtask k8s cli -- create-tenant my-team`
etc. Keep the in-pod form as a "without xtask" fallback in a collapsible.

[`rio-cli/src/main.rs:1-9`](../../rio-cli/src/main.rs): invert the
primary/secondary framing. Current: "intended for kubectl exec; standalone
also works." New: "intended for local use via port-forward (`xtask k8s
cli`); in-pod exec also works but bloats the scheduler image."

### T6 — `docs(security):` add sec.image.control-plane-minimal marker

New spec paragraph in [`security.md`](../../docs/src/security.md) after
`r[sec.psa.control-plane-restricted]` (line 89). See `## Spec additions`.

## Exit criteria

- `cargo xtask k8s cli -p kind -- status` prints cluster status from a
  locally-run rio-cli against a port-forwarded scheduler
- `cargo xtask k8s cli -p kind -- upstream list` reaches the store
  (proves store port-forward + `RIO_STORE_ADDR` override work)
- `cargo xtask k8s smoke -p kind` passes with step_tenant/step_status
  using the local path (no `run_in_scheduler` for rio-cli invocations)
- `xtask k8s status` renders rio-cli output without in-pod exec
- `deployment.md` verification section shows `xtask k8s cli` as primary

## Tracey

References existing markers:
- `r[sec.psa.control-plane-restricted]` — T6 adds a neighboring marker
  that extends the same principle from pod-security to image contents

Adds new markers to component specs:
- `r[sec.image.control-plane-minimal]` → `docs/src/security.md`
  (see `## Spec additions` below)

## Spec additions

Add to [`docs/src/security.md`](../../docs/src/security.md) after
`r[sec.psa.control-plane-restricted]` (line 89, before the seccomp note):

```markdown
r[sec.image.control-plane-minimal]

Control-plane container images (scheduler, gateway, controller, store)
MUST contain only the component binary and its direct runtime
dependencies. Operator tooling — rio-cli, jq, debugging utilities —
MUST NOT be bundled. Admin operations run rio-cli LOCALLY via
`cargo xtask k8s cli`, which port-forwards the gRPC endpoints and
fetches the mTLS client cert from the cluster. Bundling tooling in the
scheduler image expands the attack surface (every transitive dependency
is an execution primitive in a compromised pod) and couples the
control-plane release cadence to CLI dependency updates.
```

## Files

```json files
[
  {"path": "xtask/src/k8s/provider.rs", "action": "MODIFY", "note": "T1: add tunnel_grpc trait method"},
  {"path": "xtask/src/k8s/k3s/smoke.rs", "action": "MODIFY", "note": "T1: port_forward helper + tunnel_grpc impl"},
  {"path": "xtask/src/k8s/kind/mod.rs", "action": "MODIFY", "note": "T1: delegate tunnel_grpc to k3s"},
  {"path": "xtask/src/k8s/eks/mod.rs", "action": "MODIFY", "note": "T1: eks tunnel_grpc (kubectl port-forward, not SSM)"},
  {"path": "xtask/src/kube.rs", "action": "MODIFY", "note": "T2: fetch_tls_to_tempdir helper"},
  {"path": "xtask/src/k8s/mod.rs", "action": "MODIFY", "note": "T3: CliTunnel variant + with_cli_tunnel helper"},
  {"path": "xtask/src/k8s/eks/smoke.rs", "action": "MODIFY", "note": "T4: migrate step_tenant/step_status to local rio-cli"},
  {"path": "xtask/src/k8s/status.rs", "action": "MODIFY", "note": "T4: migrate status rio-cli call"},
  {"path": "docs/src/deployment.md", "action": "MODIFY", "note": "T5: xtask k8s cli as primary verification path"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T5: invert doc-comment primary/secondary framing"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T6: add sec.image.control-plane-minimal marker"}
]
```

```
xtask/src/
├── kube.rs                  # T2: fetch_tls_to_tempdir
└── k8s/
    ├── mod.rs               # T3: CliTunnel + with_cli_tunnel
    ├── provider.rs          # T1: tunnel_grpc trait
    ├── status.rs            # T4: migrate run_in_scheduler call
    ├── k3s/smoke.rs         # T1: port_forward helper
    ├── kind/mod.rs          # T1: delegate
    └── eks/
        ├── mod.rs           # T1: impl
        └── smoke.rs         # T4: migrate step_tenant/status
rio-cli/src/main.rs          # T5: doc-comment
docs/src/
├── deployment.md            # T5: verification section
└── security.md              # T6: new marker
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [489, 492], "note": "standalone; P0489 is context-only (jq-missing incident), P0492 touches k3s/kind deploy() but not smoke.rs"}
```

**Depends on:** none — all plumbing exists (port-forward pattern in
`k3s/smoke.rs`, Secret-read in `kube.rs`, `with_remote_store` analog in
`mod.rs`, `localhost` SAN already in cert-manager).

**Conflicts with:** [P0492](plan-0492-xtask-deploy-local-dev-shared.md)
touches `xtask/src/k8s/{kind,k3s}/mod.rs` (deploy() extraction) — T1 here
touches `kind/mod.rs` and `k3s/smoke.rs`. Overlap is minimal (different
functions in the same files); whichever lands second rebases trivially.
No serialization needed. Not in collisions-top-30.
