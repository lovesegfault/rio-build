# Threat Model

## Trust Boundaries

```mermaid
flowchart LR
    Clients["Untrusted<br/>(Nix clients)"] -->|SSH| GW["rio-gateway"]
    GW -->|"gRPC (mTLS)"| Sched["rio-scheduler"]
    Sched -->|"gRPC (mTLS)"| Store["rio-store"]
    Worker["rio-worker"] -->|"gRPC (mTLS)"| Store
    Worker -->|"gRPC (mTLS)"| Sched
    Store --> S3["S3 (IRSA)"]
    Store --> PG["PostgreSQL"]
    Worker --> Sandbox["nix sandbox<br/>(purity, NOT security)"]
```

### Boundary 1: Nix Client → Gateway (SSH)

r[sec.boundary.ssh-auth]
The gateway authenticates SSH connections via public key authentication. Authorized keys are loaded from an `authorized_keys`-format file at startup; only connections presenting a listed key are accepted. Password authentication is disabled.

- **Threat**: Malicious `.drv` files, crafted protocol messages, resource exhaustion
- **Mitigations**: Protocol parser fuzzing (see `rio-nix/fuzz/`), global NAR size limits (`MAX_NAR_SIZE`)

> **Phase deferral (hardening):** The following hardening measures are planned but not yet enforced: (a) ed25519-only key algorithm filter (currently any key type in `authorized_keys` is accepted), (b) per-tenant rate limiting, (c) per-tenant connection / concurrent-channel limits, (d) SSH key → tenant mapping (see [Multi-Tenancy](multi-tenancy.md)). See [Phase 5](phases/phase5.md).

### Boundary 2: Gateway/Worker → Internal Services (gRPC)

r[sec.boundary.grpc-hmac]
Inter-component gRPC traffic is authenticated with mTLS and, for write-path RPCs, authorized via HMAC-signed assignment tokens.

- **Auth**: mTLS (service mesh or cert-manager). Each component has a distinct identity.
- **Threat**: Compromised pod impersonating another component
- **Mitigations**: mTLS with per-service certificates, NetworkPolicy restricting pod-to-pod communication
- **Authorization**: mTLS authenticates component identity. Application-level authorization uses assignment-scoped tokens for sensitive RPCs:
  - The scheduler signs **assignment tokens** (HMAC-SHA256) when dispatching work. Each token contains `(worker_id, derivation_hash, expected_output_paths, expiry)`.
  - Workers present the assignment token when calling `PutPath` on the store. The store verifies the token signature and checks that the output path being uploaded matches `expected_output_paths`.
  - This prevents a compromised worker from writing to store paths it was never assigned to build.
  - Token lifetime is scoped to the build assignment; tokens expire after a configurable TTL (default: 2× the build timeout).
  - The signing key is a shared HMAC secret between the scheduler and store, stored as a Kubernetes Secret (recommend KMS/Vault for production).
  - **Read authorization:** Workers call `GetPath` and `QueryPathInfo` on the store for FUSE cache fetches. Read access is authorized by mTLS component identity --- any authenticated worker can read any store path. This is acceptable because: (a) store paths are content-addressed and immutable, (b) workers need access to shared paths (glibc, coreutils) regardless of tenant, (c) output isolation is enforced at the scheduling level (workers only build what they are assigned). For deployments requiring strict tenant read isolation, a future enhancement could add tenant-scoped read tokens.

> **Phase 3b deferral:** Neither mTLS nor HMAC assignment tokens are implemented yet. All inter-component gRPC channels currently use unauthenticated `http://` transport. There is no TLS configuration surface, no HMAC signing key, and `PutPath` performs no token verification. The security boundary is presently provided entirely by Kubernetes `NetworkPolicy` (pod-to-pod traffic restrictions). `r[sec.boundary.grpc-hmac]` is intentionally uncovered in `tracey query uncovered` until Phase 3b lands. See [Phase 3b](phases/phase3b.md).

### Boundary 3: Worker → Nix Sandbox

- **Auth**: None (sandbox is a purity mechanism, NOT a security boundary)
- **Threat**: Malicious derivation escaping sandbox and accessing worker resources
- **Mitigations**: `CAP_SYS_ADMIN` + custom seccomp profile (NOT `privileged: true`), dedicated node pool, NetworkPolicy, `automountServiceAccountToken: false`, IMDSv2 hop limit=1

> **Phase deferral (seccomp):** The custom seccomp profile is not yet shipped. Worker pods currently run with the runtime-default seccomp profile only. A rio-specific profile blocking `ptrace`, `bpf`, `kexec_load`, and other unnecessary syscalls under `CAP_SYS_ADMIN` is tracked as TODO(phase3b). See [Known Limitations](#known-limitations) item 2.

### Boundary 4: Binary Cache HTTP → External Clients

- **Auth**: Bearer token or `netrc`-compatible authentication. Nix clients use `netrc-file` or `access-tokens` settings.
- **Threat**: Unauthenticated enumeration of store paths; data exfiltration via narinfo/NAR download; resource exhaustion via large NAR downloads
- **Mitigations**:
  - Mandatory authentication (bearer token per tenant). Unauthenticated access must be an explicit opt-in for public caches.
  - Per-tenant path visibility: narinfo queries return 404 for paths outside the requesting tenant's scope.
  - Rate limiting on `/nar/` downloads (configurable per tenant).
  - NetworkPolicy: restrict access to the HTTP port from trusted CIDR ranges or ingress controller only.
- **Note**: The binary cache HTTP server runs in the same process as the gRPC StoreService. Consider separate NetworkPolicy rules for the HTTP port vs the gRPC port.

> **Phase 5 deferral:** Bearer token authentication, per-tenant path visibility, and per-tenant download rate limiting for the binary cache HTTP endpoint are not yet implemented. The narinfo/NAR endpoints currently serve any valid store path to any caller. Until Phase 5, the only access control is `NetworkPolicy` CIDR restriction on the HTTP port. See [Multi-Tenancy](multi-tenancy.md).

## Key Security Properties

| Property | Mechanism | Status |
|----------|-----------|--------|
| **Build output integrity** | NAR SHA-256 verified on PutPath; ed25519 signatures | Designed |
| **Chunk integrity** | BLAKE3 verified on every read from S3/cache | Designed |
| **Signing key protection** | K8s Secret (minimum); recommend KMS/Vault for production | Designed |
| **S3 credential management** | IRSA (IAM Roles for Service Accounts) on EKS | Recommended |
| **Worker isolation** | Per-build overlayfs, Nix sandbox, NetworkPolicy | Designed |
| **Metadata service blocking** | NetworkPolicy egress deny `169.254.169.254`; IMDSv2 hop limit=1 | Designed |
| **Inter-component auth** | mTLS between all gRPC endpoints | Phase 3b deferred (currently `http://` + NetworkPolicy) |
| **Multi-tenant data isolation** | Per-tenant data visibility (Phase 5); shared workers with per-build overlay isolation | Planned |

## Derivation Validation

r[sec.drv.validate]
On `PutPath`, rio-store recomputes the SHA-256 digest of the uploaded NAR bytes and rejects the upload if the digest does not match the `nar_hash` declared in the accompanying `PathInfo`. This is the core integrity check: a worker cannot store data under a mismatched content hash. See `rio-store/src/validate.rs`.

Additional validation checks (below) are enforced at other points in the pipeline. These are **not** covered by `r[sec.drv.validate]` — each has its own tracey rule or phase deferral.

| Check | Where | Status | Description |
|-------|-------|--------|-------------|
| NAR SHA-256 verification | Store | `r[sec.drv.validate]` | On `PutPath`, the store recomputes SHA-256 over the NAR bytes and rejects on mismatch. |
| `restrict-eval` | Worker | Implemented | The worker's `nix.conf` sets `restrict-eval = true`, preventing derivations from accessing paths outside the Nix store during evaluation. |
| Sandbox enforcement | Worker | Implemented | `sandbox = true` in `nix.conf` ensures all builds run inside the Nix sandbox (user/mount/PID/network namespaces). |
| DAG size limit | Scheduler | Partial | The scheduler enforces `max_dag_size` on `SubmitBuild`. **Not yet enforced at the gateway** as an early rejection. |
| `__noChroot` rejection | Gateway | **Not implemented** | Derivations with `__noChroot = true` must be rejected before dispatch. TODO(phase3b). |
| Per-tenant NAR size limit | Gateway | **Not implemented** | Only the global `MAX_NAR_SIZE` limit exists. Per-tenant `max_nar_upload_size` is Phase 5. |
| Output path match | Store | **Not implemented** | Blocked on HMAC assignment tokens (Phase 3b). Until then, any worker can `PutPath` any output. |

## Secrets Management

rio-build requires several secrets: SSH host keys, signing keys, database credentials, HMAC signing keys for assignment tokens, and TLS certificates (if not using a service mesh).

### Recommended Patterns (by maturity)

**Development / single-node:**
- Kubernetes Secrets with `stringData` fields. Adequate for development but not for production.

**Production baseline:**
- [External Secrets Operator](https://external-secrets.io/) syncing from AWS Secrets Manager, GCP Secret Manager, or HashiCorp Vault into Kubernetes Secrets. Secrets are managed externally and auto-rotated.
- Mount secrets as files (not environment variables) to avoid `/proc` and `ps` leakage. All rio-build secret config parameters use file paths (`signing_key_path`, `host_key_path`, `tls_key_path`).

**Production hardened:**
- HashiCorp Vault with the Vault Agent Injector sidecar. The sidecar injects secrets into a shared `emptyDir` volume, and rio-build reads them from file paths. Vault handles rotation; the sidecar re-renders secrets on change.
- For the `database_url` credential specifically: use Vault's database secrets engine to issue short-lived PostgreSQL credentials per pod, eliminating static database passwords entirely.

### Secret Inventory

| Secret | Used By | Rotation | Status |
|--------|---------|----------|--------|
| SSH host key (`ssh_host_ed25519_key`) | Gateway | Rarely (causes client known_hosts warnings) | Implemented |
| Authorized SSH keys[^authkeys] | Gateway | Per-tenant lifecycle | Implemented (flat file; no tenant annotation) |
| NAR signing key (`signing-key`) | Store | Annually or on compromise | Implemented |
| HMAC signing key (assignment tokens)[^hmac] | Scheduler + Store | Annually or on compromise | **Phase 3b** — not yet read or verified |
| JWT signing key (tenant tokens)[^jwt] | Gateway | Annually; SIGHUP reload for zero-downtime | **Phase 5** — no tenant token issuance yet |
| Database credentials (`database_url`) | Scheduler, Store, Controller | Via Vault database engine or External Secrets | Implemented |
| TLS certificates (if no service mesh)[^tls] | All gRPC components | Via cert-manager auto-renewal | **Phase 3b** — all channels currently `http://` |

[^authkeys]: Tenant annotation in the `authorized_keys` comment field (e.g., `ssh-ed25519 AAAA... tenant=acme`) is not yet parsed. All authenticated keys currently share a single implicit tenant. SSH key → tenant mapping is Phase 5.
[^hmac]: No code path currently loads, signs, or verifies an HMAC key. `PutPath` accepts any upload from any caller. See the Phase 3b deferral under [Boundary 2](#boundary-2-gatewayworker--internal-services-grpc).
[^jwt]: There is no JWT issuance or verification code. Tenant identity exists only as a string field in the scheduler's `BuildOptions` with no authentication backing.
[^tls]: No `rustls`/`tls_config` wiring exists on any tonic client or server. All gRPC connections use plaintext HTTP/2.

## Additional Threats

### Signing Key Compromise/Rotation

- **Threat**: Leaked signing key allows an attacker to sign arbitrary store paths as trusted.
- **Mitigation**: Store signing keys in KMS/Vault (not raw K8s Secrets) for production deployments. See [rio-store key rotation](components/store.md#key-rotation) for the rotation procedure. Keys should be rotated annually or immediately on suspected compromise.

### DAG-Based Resource Exhaustion

- **Threat**: A malicious or buggy client submits a derivation DAG with millions of nodes, exhausting scheduler memory and CPU.
- **Mitigation**: Per-tenant limits on maximum DAG size (`max_dag_size`) and maximum concurrent builds (`max_concurrent_builds`). See [Multi-Tenancy](multi-tenancy.md) for quota configuration.
- **Implementation note**: `max_dag_size` is currently enforced **only at the scheduler** on `SubmitBuild`. Gateway-side early rejection (before gRPC transmission) is planned but not yet implemented — TODO(phase3b): add a simple node-count check in `rio-gateway/src/handler/build.rs` before calling `SubmitBuild`.

### Build-Time Secrets

- **Threat**: Fixed-output derivations (FODs) needing credentials (e.g., private GitHub repos) require network access and authentication during build.
- **Mitigation**: Route FOD network traffic through a forward proxy (e.g., Squid) with domain allowlisting. The proxy allowlist is configurable per tenant. See [Phase 3](phases/phase3.md) for the implementation plan.

### FOD Network Egress

- **Threat**: FOD builds require internet egress, which conflicts with the worker NetworkPolicy that blocks all external traffic.
- **Design**: FOD builds are routed through a dedicated HTTP/HTTPS forward proxy (e.g., Squid) deployed as a ClusterIP service within the cluster.
  - Workers detect FOD builds (output hash is known in advance) and set `http_proxy`/`https_proxy` environment variables pointing to the proxy.
  - The worker NetworkPolicy adds an egress exception allowing traffic to the proxy service on its listening port.
  - The proxy enforces a domain allowlist (configurable per deployment; default: `cache.nixos.org`, `github.com`, `gitlab.com`, common source forges).
  - All proxied requests are logged for audit. Requests to non-allowlisted domains are rejected.
  - Non-FOD builds retain the full egress deny NetworkPolicy --- no proxy access.
- **Phase**: Phase 3b (not yet implemented). See [Phase 3b tasks](phases/phase3b.md).

### Log Injection

- **Threat**: Untrusted build output is displayed in the dashboard log viewer. Malicious builds could inject HTML/JavaScript into logs.
- **Mitigation**: The dashboard must sanitize all log content as raw text. Never render log lines as HTML. Use `<pre>` elements or equivalent with proper escaping.

### Cross-Tenant Chunk Probing

- **Threat**: `FindMissingChunks` can reveal whether another tenant has built a specific package.
- **Mitigation**: Per-tenant chunk scoping (at the cost of dedup) or accept the risk. See [Multi-Tenancy](multi-tenancy.md#findmissingchunks-scoping).

## Known Limitations

1. **The Nix sandbox is NOT a security boundary.** It prevents builds from accessing undeclared inputs (purity) but does not prevent a determined attacker from escaping. For multi-tenant deployments, the security boundary is the worker pod + node isolation.

2. **Workers require `CAP_SYS_ADMIN`.** This capability enables mount namespace manipulation, which is powerful. The custom seccomp profile blocks dangerous syscalls (`ptrace`, `bpf`, `kexec_load`), but `CAP_SYS_ADMIN` still grants significant host access. Dedicated node pools with taints are essential. **Mitigation (K8s 1.33+):** Worker pods must set `hostUsers: false` to enable user namespace isolation. With user namespaces, `CAP_SYS_ADMIN` applies only within the user namespace, not on the host --- the attacker gains capabilities within a namespace that maps to unprivileged host UIDs, significantly reducing the blast radius. See [ADR-012](./decisions/012-privileged-worker-pods.md#kubernetes-user-namespace-isolation).

3. **`CAP_SYS_ADMIN` is held throughout build execution.** The worker cannot drop `CAP_SYS_ADMIN` between overlay setup and build completion because the Nix sandbox itself requires mount namespace manipulation. A sandbox escape gives the attacker `CAP_SYS_ADMIN` capabilities within the user namespace (see mitigation in #2). Additional mitigations: custom seccomp profile, dedicated node pools, and NetworkPolicy. Future work: explore splitting the worker into a privileged setup process and an unprivileged build supervisor.

4. **Cross-tenant chunk deduplication leaks build activity.** A tenant can probe `FindMissingChunks` to determine whether another tenant has built a specific package. Mitigation: scope `FindMissingChunks` per tenant (at the cost of dedup savings) or accept the risk with documentation.

5. **Fixed-output derivations (FODs) need network access.** FOD builds (fetchurl, fetchgit) require egress to the internet, which conflicts with the worker NetworkPolicy. FOD traffic is routed through a forward proxy with domain allowlisting (see [FOD Network Egress](#fod-network-egress)).
