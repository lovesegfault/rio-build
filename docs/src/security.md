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
  - The scheduler signs **assignment tokens** (HMAC-SHA256) when dispatching work. Token format is `base64url(json(Claims)).base64url(hmac_sha256(key, claims_json))`. The `Claims` struct has exactly four fields: `worker_id` (string, audit only), `drv_hash` (string, ties token to a specific build), `expected_outputs` (list of store paths, the authorization check), `expiry_unix` (u64 Unix seconds, replay prevention).
  - Workers present the assignment token in the `x-rio-assignment-token` gRPC metadata header when calling `PutPath` on the store. The store verifies the token signature, checks `now < expiry_unix`, and rejects with `PERMISSION_DENIED` if the uploaded `store_path ∉ expected_outputs`.
  - This prevents a compromised worker from writing to store paths it was never assigned to build.
  - Token lifetime is scoped to the build assignment; tokens expire after a configurable TTL (default: 2× the build timeout).
  - The signing key is a shared HMAC secret between the scheduler and store, stored as a Kubernetes Secret (recommend KMS/Vault for production).
  - **Read authorization:** Workers call `GetPath` and `QueryPathInfo` on the store for FUSE cache fetches. Read access is authorized by mTLS component identity --- any authenticated worker can read any store path. This is acceptable because: (a) store paths are content-addressed and immutable, (b) workers need access to shared paths (glibc, coreutils) regardless of tenant, (c) output isolation is enforced at the scheduling level (workers only build what they are assigned). For deployments requiring strict tenant read isolation, a future enhancement could add tenant-scoped read tokens.

> **Implemented (Phase 3b):** mTLS + HMAC assignment tokens are live. When `RIO_TLS__CERT_PATH`/`KEY_PATH`/`CA_PATH` are set, all gRPC channels use TLS with client cert verification (`ServerTlsConfig::client_ca_root`). When `RIO_HMAC_KEY_PATH` is set on scheduler + store, assignment tokens are HMAC-SHA256-signed at dispatch and verified on `PutPath` (rejected if the uploaded path isn't in `claims.expected_outputs`). **mTLS CN bypass:** PutPath skips HMAC verification when the caller's client certificate has `CN=rio-gateway` (the gateway handles `nix copy --to` and has no assignment token). This check only applies when an `HmacVerifier` is configured --- without one, PutPath accepts all callers (dev mode). See `rio-common/src/tls.rs`, `rio-common/src/hmac.rs`, `rio-store/src/grpc/put_path.rs`.

> **TLS SNI:** `load_client_tls` does **not** set a fixed `domain_name` on the `ClientTlsConfig`. tonic derives the SNI server name from the connect URL's host per-connection. A global `domain_name` override would break multi-service clients (gateway/worker connect to both scheduler and store, each with a different cert SAN).

#### OIDC Authentication for External Push

`rio-push` enables external CI environments (GitHub Actions, GitLab CI) to push pre-built store path closures to rio-store without mTLS certificates or HMAC assignment tokens. Authentication uses OIDC JWT tokens:

- The CI environment obtains an OIDC token from its identity provider (e.g., `$ACTIONS_ID_TOKEN_REQUEST_URL` in GitHub Actions).
- `rio-push` sends the token in the `x-rio-oidc-token` gRPC metadata header with each `PutPath` call.
- rio-store validates the token: signature (via JWKS), expiry, audience, issuer, and bound claims.
- OIDC-authenticated pushes have **no path restriction** (unlike HMAC tokens with `expected_outputs`), since the caller controls what to push. Access is scoped by the provider's bound claims (e.g., `repository_owner`).

**PutPath auth priority:** (1) OIDC token present + valid -> accept, (2) HMAC assignment token -> verify + path-check, (3) mTLS CN=rio-gateway -> accept, (4) no verifier -> accept (dev mode), (5) otherwise -> reject.

**Threat mitigations:**
- **Token theft:** OIDC tokens are short-lived (typically 5--15 min). Bound claims (`repository_owner`, `repository`) limit blast radius.
- **Key rotation:** On signature verification failure, rio-store force-refreshes the JWKS cache once before rejecting (handles key rotation without downtime).
- **Issuer spoofing:** Only explicitly configured issuers are accepted. Unknown issuers are rejected before JWKS fetch.

See `rio-common/src/oidc.rs` for the verifier, `rio-store/src/grpc/put_path.rs` for the auth flow.

### Boundary 3: Worker → Nix Sandbox

- **Auth**: None (sandbox is a purity mechanism, NOT a security boundary)
- **Threat**: Malicious derivation escaping sandbox and accessing worker resources
- **Mitigations**: `CAP_SYS_ADMIN` + `seccompProfile: RuntimeDefault` (NOT `privileged: true`), dedicated node pool, NetworkPolicy, `automountServiceAccountToken: false`, IMDSv2 hop limit=1

> **seccomp (Phase 3b):** Worker pods set `seccompProfile: RuntimeDefault` at the pod level (applies to all containers + init containers) when `privileged != true`. RuntimeDefault blocks ~40 syscalls including `kexec_load`, `open_by_handle_at`, `userfaultfd` that builds don't need. A CUSTOM profile (blocking `ptrace`, `bpf` etc under `CAP_SYS_ADMIN`) is tracked for Phase 4 — RuntimeDefault is sufficient for Phase 3b's threat model (it's what production containers use by default; we just make it explicit). See [Known Limitations](#known-limitations) item 2.

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
| **Inter-component auth** | mTLS between all gRPC endpoints | Implemented (Phase 3b) — configure via `RIO_TLS__*` env |
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
| DAG size limit | Gateway + Scheduler | Implemented | Gateway's `translate::validate_dag` checks `nodes.len() > MAX_DAG_NODES` before SubmitBuild (early reject); scheduler also enforces. |
| `__noChroot` rejection | Gateway | Implemented | `translate::validate_dag` checks derivation env for `__noChroot=1` via drv_cache lookup. Rejected with "sandbox escape" error. |
| Per-tenant NAR size limit | Gateway | **Not implemented** | Only the global `MAX_NAR_SIZE` limit exists. Per-tenant `max_nar_upload_size` is Phase 5. |
| Output path match | Store | Implemented | HMAC assignment tokens: store verifies `x-rio-assignment-token` metadata on PutPath, checks `store_path ∈ claims.expected_outputs`. mTLS bypass for gateway. |

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
| HMAC signing key (assignment tokens) | Scheduler + Store | Annually or on compromise | Implemented — `RIO_HMAC_KEY_PATH`, same key file both sides |
| JWT signing key (tenant tokens)[^jwt] | Gateway | Annually; SIGHUP reload for zero-downtime | **Phase 5** — no tenant token issuance yet |
| Database credentials (`database_url`) | Scheduler, Store, Controller | Via Vault database engine or External Secrets | Implemented |
| TLS certificates | All gRPC components | Via cert-manager auto-renewal (90d certs, renew at 30d) | Implemented — see `deploy/overlays/prod/cert-manager.yaml` |

[^authkeys]: Tenant annotation in the `authorized_keys` comment field (e.g., `ssh-ed25519 AAAA... tenant=acme`) is not yet parsed. All authenticated keys currently share a single implicit tenant. SSH key → tenant mapping is Phase 5.
[^jwt]: There is no JWT issuance or verification code. Tenant identity exists only as a string field in the scheduler's `BuildOptions` with no authentication backing.

## Additional Threats

### Signing Key Compromise/Rotation

- **Threat**: Leaked signing key allows an attacker to sign arbitrary store paths as trusted.
- **Mitigation**: Store signing keys in KMS/Vault (not raw K8s Secrets) for production deployments. See [rio-store key rotation](components/store.md#key-rotation) for the rotation procedure. Keys should be rotated annually or immediately on suspected compromise.

### DAG-Based Resource Exhaustion

- **Threat**: A malicious or buggy client submits a derivation DAG with millions of nodes, exhausting scheduler memory and CPU.
- **Mitigation**: Per-tenant limits on maximum DAG size (`max_dag_size`) and maximum concurrent builds (`max_concurrent_builds`). See [Multi-Tenancy](multi-tenancy.md) for quota configuration.
- **Implementation (Phase 3b):** `max_dag_size` is enforced at BOTH gateway (`translate::validate_dag`) and scheduler. Gateway-side check is early rejection — saves the gRPC round-trip for obvious over-size submissions.

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
- **Phase**: Implemented (Phase 3b). See `deploy/base/fod-proxy.yaml` (Squid + allowlist) and the worker's `spawn_daemon_in_namespace` (`fod_proxy` param, injects env only when `is_fixed_output`).

### Log Injection

- **Threat**: Untrusted build output is displayed in the dashboard log viewer. Malicious builds could inject HTML/JavaScript into logs.
- **Mitigation**: The dashboard must sanitize all log content as raw text. Never render log lines as HTML. Use `<pre>` elements or equivalent with proper escaping.

### Cross-Tenant Chunk Probing

- **Threat**: `FindMissingChunks` can reveal whether another tenant has built a specific package.
- **Mitigation**: Per-tenant chunk scoping (at the cost of dedup) or accept the risk. See [Multi-Tenancy](multi-tenancy.md#findmissingchunks-scoping).

## Known Limitations

1. **The Nix sandbox is NOT a security boundary.** It prevents builds from accessing undeclared inputs (purity) but does not prevent a determined attacker from escaping. For multi-tenant deployments, the security boundary is the worker pod + node isolation.

2. **Workers require `CAP_SYS_ADMIN`.** This capability enables mount namespace manipulation, which is powerful. `seccompProfile: RuntimeDefault` blocks ~40 syscalls (`kexec_load`, `open_by_handle_at`, etc.), but `CAP_SYS_ADMIN` still grants significant host access. A custom seccomp profile blocking `ptrace`/`bpf`/`setns` is a Phase 4 enhancement. Dedicated node pools with taints are essential. **Mitigation (K8s 1.33+):** Worker pods must set `hostUsers: false` to enable user namespace isolation. With user namespaces, `CAP_SYS_ADMIN` applies only within the user namespace, not on the host --- the attacker gains capabilities within a namespace that maps to unprivileged host UIDs, significantly reducing the blast radius. See [ADR-012](./decisions/012-privileged-worker-pods.md#kubernetes-user-namespace-isolation).

3. **`CAP_SYS_ADMIN` is held throughout build execution.** The worker cannot drop `CAP_SYS_ADMIN` between overlay setup and build completion because the Nix sandbox itself requires mount namespace manipulation. A sandbox escape gives the attacker `CAP_SYS_ADMIN` capabilities within the user namespace (see mitigation in #2). Additional mitigations: RuntimeDefault seccomp (custom profile tracked for Phase 4), dedicated node pools, and NetworkPolicy. Future work: explore splitting the worker into a privileged setup process and an unprivileged build supervisor.

4. **Cross-tenant chunk deduplication leaks build activity.** A tenant can probe `FindMissingChunks` to determine whether another tenant has built a specific package. Mitigation: scope `FindMissingChunks` per tenant (at the cost of dedup savings) or accept the risk with documentation.

5. **Fixed-output derivations (FODs) need network access.** FOD builds (fetchurl, fetchgit) require egress to the internet, which conflicts with the worker NetworkPolicy. FOD traffic is routed through a forward proxy with domain allowlisting (see [FOD Network Egress](#fod-network-egress)).
