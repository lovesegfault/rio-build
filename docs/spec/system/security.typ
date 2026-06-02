#import "/lib/rio.typ": *
#show: rio.with(domains: ("sec", "common", "builder"))

= Threat Model

== Trust Boundaries

#figure(
  caption: [Trust boundaries. The Cilium overlay encrypts node-to-node; SSH is the only external ingress.],
  // QA5: was 764pt (~1019px) — clipped in the 750px column. Tightened
  // horizontal spacing 16mm→8mm and shrank node text to 0.8em (≈30%
  // width cut). The enclose-node label moved top-left → bottom-left so
  // it sits on the dashed border instead of overlapping the gw box.
  {
    set text(size: 0.8em)
    diagram(
      spacing: (7mm, 11mm),
      node-stroke: 0.5pt,
      node((0, 1), [Untrusted\ (Nix clients)], name: <cl>),
      node((1, 0.5), [rio-gateway], name: <gw>, fill: accent.lighten(88%)),
      node((2, 0.5), [rio-scheduler], name: <sched>, fill: accent.lighten(88%)),
      node((3, 0.5), [rio-store], name: <store>, fill: accent.lighten(88%)),
      node((2, 1.5), [rio-builder], name: <ex>, fill: accent.lighten(88%)),
      node((4, 0), [S3 (@irsa)], name: <s3>, shape: fletcher.shapes.cylinder),
      node((4, 1), [PostgreSQL], name: <pg>, shape: fletcher.shapes.cylinder),
      node(
        (3, 1.5),
        [rio-exec sandbox\ #text(size: 0.85em)[(purity, NOT security)]],
        name: <sb>,
        stroke: (dash: "dashed"),
      ),
      edge(<cl>, <gw>, "-|>", [SSH], label-size: 0.8em),
      edge(<gw>, <sched>, "-|>", [gRPC], label-size: 0.8em),
      edge(<sched>, <store>, "-|>", [gRPC], label-size: 0.8em),
      // QA4-#4: <ex>→<store> diagonal and <ex>→<sched> vertical both
      // anchor labels at the <ex> vertex by default; label-side/pos
      // separates them.
      edge(
        <ex>,
        <store>,
        "-|>",
        [gRPC],
        label-size: 0.8em,
        label-side: right,
        label-pos: 0.6,
      ),
      edge(<ex>, <sched>, "-|>", [gRPC], label-size: 0.8em, label-side: left),
      edge(<store>, <s3>, "-|>"),
      edge(<store>, <pg>, "-|>"),
      edge(<ex>, <sb>, "-|>"),
      node(
        enclose: (<gw>, <sched>, <store>, <ex>),
        stroke: (paint: muted, dash: "dashed"),
        inset: 10pt,
        snap: false,
        align(bottom + left, text(
          size: 0.8em,
          fill: muted,
        )[Cilium overlay (WireGuard node-to-node)]),
      ),
    )
  },
)

=== Component Trust

The control plane --- gateway, scheduler, store, and controller --- is
trusted. Workers (builders and fetchers) are not: they execute arbitrary
tenant-supplied build instructions in the same pod as the code that
prepares requests and parses outputs, so a sufficiently determined tenant
must be assumed capable of compromising the worker process itself.

#r("sec.trust.workers-untrusted")[
  Workers (builders and fetchers) MUST be treated as untrusted. Any
  validation a worker performs --- request-glue shape checks, output
  policy enforcement, hash verification before upload --- is
  defense-in-depth only. Authoritative enforcement of what may be
  registered MUST live in the trusted plane: the gateway/scheduler for
  submission-shape validation, and rio-store at registration time for
  the content it accepts.
]

- *Threat*: A compromised worker uploads content that does not match what
  the derivation legitimately produces, or claims paths it was never asked
  to build.
- *Mitigations*: Upload authorization is HMAC-scoped to the exact output
  paths the scheduler assigned; the store's content-address gates
  re-derive paths from uploaded bytes rather than trusting the claim.
  Worker-side checks remain useful for failing fast, never as the
  authority.

=== Boundary 1: Nix Client → Gateway (SSH)

#r("sec.boundary.ssh-auth")[
  The gateway authenticates SSH connections via public key authentication.
  Authorized keys are loaded from an `authorized_keys`-format file at startup;
  only connections presenting a listed key are accepted. Password authentication
  is disabled.
]

- *Threat*: Malicious `.drv` files, crafted protocol messages, resource
  exhaustion
- *Mitigations*: Protocol parser fuzzing (see `fuzz/rio-nix/`), global @nar size
  limits (`MAX_NAR_SIZE`); per-tenant build-submit rate limiting
  (#rref("gw.rate.per-tenant")); global connection cap
  (#rref("gw.conn.cap")); SSH-key→tenant mapping via the server-side
  `authorized_keys` comment (#rref("gw.auth.tenant-from-key-comment")).
  Key-algorithm filtering is not provided --- the operator's `authorized_keys`
  file is the operator's trust boundary.

=== Boundary 2: Gateway/Executor → Internal Services (gRPC)

#r("sec.boundary.grpc-hmac")[
  Inter-component gRPC traffic is encrypted by Cilium WireGuard
  (`r[sec.transport.cilium-wireguard]`), reachability-restricted by
  CiliumNetworkPolicy, and --- for write-path RPCs --- authorized via
  HMAC-signed tokens.
]

- *Encryption*: Cilium WireGuard transparent encryption (node-to-node, kernel
  datapath). Components speak plaintext gRPC; the overlay encrypts.
- *Threat*: Compromised pod impersonating another component
- *Mitigations*: CiliumNetworkPolicy restricts pod-to-pod reachability by
  label-based identity (e.g., only pods labeled
  `app.kubernetes.io/name=rio-gateway` may reach `rio-store:9002`).
  Application-level HMAC tokens authorize sensitive write RPCs.
- *Authorization*: CNP gates _which pods can connect_; HMAC tokens gate _what a
  connected pod may write_:

#r("sec.transport.cilium-wireguard")[
  All pod-to-pod traffic is encrypted by Cilium's WireGuard transparent
  encryption (`encryption.type: wireguard` in the Cilium helm values).
  Encryption is at the overlay layer (Geneve-encapsulated, ChaCha20-Poly1305)
  --- rio components run plaintext gRPC servers and clients with no TLS
  configuration. There is no per-service certificate identity; component
  identity for reachability is the pod's Cilium security identity (derived from
  k8s labels), enforced by CiliumNetworkPolicy. There is no application-level
  certificate to rotate or expire.
]

#r("common.hmac.claims+1")[
  The scheduler signs *assignment tokens* (HMAC-SHA256) when dispatching work.
  Token format is
  `base64url(json(AssignmentClaims)).base64url(hmac_sha256(key, claims_json))`.
  `AssignmentClaims` carries seven fields: `executor_id` (string, audit only
  --- the store doesn't know which executor is calling), `drv_hash` (string,
  ties token to a specific build), `expected_outputs` (list of store paths, the
  authorization check), `is_ca` (bool, skips the membership check for
  floating-CA derivations whose output paths are computed post-build),
  `is_fixed_output` (bool, marks a fixed-output assignment: the store requires
  the upload to carry a `fixed:` content-address descriptor and verifies the
  uploaded bytes and the claimed path against it --- descriptor-less uploads
  under such a token are rejected), `tenant` (optional string, the attributed
  tenant for audit and per-tenant isolation; absent on legacy tokens), and
  `expiry_unix` (u64 Unix seconds, replay prevention). Optional fields use
  serde defaults so tokens minted without them still verify; the store fleet
  must carry a new field's reader before a scheduler that emits it.
  - Executors present the assignment token in the `x-rio-assignment-token` gRPC
    metadata header when calling `PutPath` on the store. The store verifies the
    token signature, checks `now < expiry_unix`, and rejects with
    `PERMISSION_DENIED` if the uploaded `store_path ∉ expected_outputs`.
  - This prevents a compromised executor from writing to store paths it was
    never assigned to build.
  - Token lifetime is scoped to the build assignment; tokens expire after a
    configurable TTL (default: 2× the build timeout).
  - The signing key is a shared HMAC secret between the scheduler and store,
    stored as a Kubernetes Secret (recommend KMS/Vault for production).
  - *Read authorization:* Executors call `GetPath` and `QueryPathInfo` on the
    store for FUSE cache fetches. Read access is gated by CiliumNetworkPolicy
    (only labeled executor pods can reach `rio-store:9002`); any reachable
    executor can read any store path. This is acceptable because: (a) store
    paths are content-addressed and immutable, (b) executors need access to
    shared paths (glibc, coreutils) regardless of tenant, (c) output isolation
    is enforced at the scheduling level (executors only build what they are
    assigned). For deployments requiring strict tenant read isolation, a future
    enhancement could add tenant-scoped read tokens.
]


#r("sec.executor.identity-token+2")[
  The scheduler signs *executor-identity tokens* (`ExecutorClaims { intent_id,
  kind, expiry_unix }`, same HMAC envelope as `AssignmentClaims`, same key) per
  `SpawnIntent`. The controller passes the token through verbatim as the
  `RIO_EXECUTOR_TOKEN` pod env var. Builders MUST present it as
  `x-rio-executor-token` metadata on `BuildExecution` open and every
  `Heartbeat`. When the HMAC key is configured, the scheduler MUST reject
  ExecutorService calls without a valid token, MUST reject a heartbeat whose
  body `intent_id` OR `kind` differs from the token's, MUST reject a heartbeat
  whose token-attested `intent_id` differs from the target executor's stored
  `auth_intent` (set at connect, immutable --- prevents a compromised pod A
  heartbeating as B with A's own intent), and MUST reject a `BuildExecution`
  reconnect whose token `intent_id` differs from the executor's stored
  `auth_intent` or whose existing stream is still live. The `BuildExecution`
  handler MUST learn the actor's accept/reject decision before spawning the
  stream-reader task (a body-supplied `executor_id` is otherwise unbound ---
  `ExecutorClaims` cannot carry it because the scheduler signs before the
  controller picks a pod name). This binds a stream to the intent AND kind its
  pod was spawned for: a compromised builder holds a token for ITS OWN
  intent+kind only and cannot hijack another executor's `stream_tx` (and
  thereby its `WorkAssignment.assignment_token`), forge `ProcessCompletion` for
  another executor's build, mutate another executor's heartbeat-driven state,
  nor self-promote `kind` to receive work routed past its CiliumNetworkPolicy
  airgap boundary.
]

#info(title: [Service-token bypass (`r[sec.authz.service-token]`)])[
  PutPath skips assignment-token verification when the caller presents a valid
  `x-rio-service-token` --- an HMAC-signed `ServiceClaims { caller, expiry_unix
  }` keyed with `RIO_SERVICE_HMAC_KEY_PATH` (a separate secret from the
  assignment key). The gateway mints one per upload with a 60s expiry. The same
  mechanism gates *every mutating RPC* on the scheduler's `AdminService` and
  the store's `StoreAdminService` (`TriggerGC`, `AddUpstream`, `GetLoad`, …):
  builders share port 9001/9002 with those services (CCNP allows the port at L4
  only), so without the gate a compromised builder could poison λ[h], drain
  arbitrary executors, set @sla overrides to bias the solver fleet-wide,
  un-poison quarantined derivations, or inject attacker-keyed upstream caches
  into another tenant's substitution path. Callers (rio-controller, rio-cli,
  rio-scheduler, rio-gateway, rio-dashboard) mint `caller="<self>"` per request
  via `ServiceTokenInterceptor`; the verifier checks against a per-RPC
  allowlist.
  The canonical mutating-RPC list is the `mutating_rpcs_require_service_token`
  test. This replaces the former certificate-CN check and is
  transport-agnostic. See `rio_auth::hmac::ensure_service_caller`,
  `rio-store/src/grpc/{put_path/,admin.rs}`, `rio-scheduler/src/admin/mod.rs`.
]

#r("sec.jwt.pubkey-mount+2")[
  When `jwt.enabled=true`, scheduler and store pods MUST have the
  `rio-jwt-pubkey` ConfigMap mounted at `/etc/rio/jwt/ed25519_pubkey` and
  `RIO_JWT__KEY_PATH` set to that path. Without the mount, `cfg.jwt.key_path`
  remains `None` and the interceptor falls through to inert mode (every RPC
  passes, no `Claims` attached) --- a silent fail-open. The gateway
  correspondingly mounts the `rio-jwt-signing` Secret at
  `/etc/rio/jwt/ed25519_seed`. Helm `_helpers.tpl` provides the
  consolidated `rio.mounts` template (parameterized by
  `form: env|mount|volume` and `want: list "jwtVerify" "jwtSign" …`),
  self-guarded on `.Values.jwt.enabled`.
]

=== Boundary 3: Executor → rio-exec Sandbox

- *Auth*: None (sandbox is a purity mechanism, NOT a security boundary)
- *Threat*: Malicious derivation escaping sandbox and accessing executor
  resources
- *Mitigations*: `CAP_SYS_ADMIN` + `seccompProfile: RuntimeDefault` (NOT
  `privileged: true`), `hostUsers: false` (user-namespace isolation), dedicated
  node pool, @networkpolicy, `automountServiceAccountToken: false`, @imdsv2 hop
  limit=1

== Executor Pod Security

#r("sec.pod.host-users-false")[
  Executor pods MUST set `hostUsers: false` to activate Kubernetes
  user-namespace isolation (K8s 1.33+). Container UIDs are remapped to
  unprivileged host UIDs; `CAP_SYS_ADMIN` applies only within the user
  namespace. A container escape gaining `CAP_SYS_ADMIN` cannot affect the host
  or other pods. See @sec-rationale-privileged. The `privileged: true` escape
  hatch (for clusters whose containerd lacks `base_runtime_spec` device
  injection) skips `hostUsers: false` --- privileged containers cannot be
  user-namespaced.
]

// rule-id is historical; mechanism is base_runtime_spec since ADR-021 §7
#r("sec.pod.fuse-device-plugin")[
  Executor pods MUST NOT obtain `/dev/fuse` via a hostPath volume --- the
  kernel rejects idmap mounts on device nodes (ADR-012 Phase 1a spike finding),
  so hostPath is incompatible with `hostUsers: false`. The device node is
  delivered by containerd's `base_runtime_spec` declaring `/dev/{fuse,kvm}` in
  OCI `linux.devices` (`nix/base-runtime-spec.nix`) --- runc `mknod`s them
  inside the container's `/dev` with container-namespace uid/gid, so no
  idmap-mount rejection. Every pod on a configured node gets `/dev/fuse`;
  `/dev/kvm` is host-conditional --- containerd's `ExecStartPre` picks the
  `withKvm` spec variant iff `test -c /dev/kvm` succeeds on the host and
  symlinks it to `/run/base-runtime-spec.json`, so non-`.metal` pods don't see
  a dead device node. No extended resource is requested and no device plugin
  runs. kvm pods route to `.metal` via per-intent `nodeAffinity`
  (`r[ctrl.pool.node-affinity-from-intent]`) plus a pool-static `rio.build/kvm`
  toleration (`r[ctrl.pool.kvm-device+2]`) --- never a pool-static
  nodeSelector, which would deadlock against the affinity on shared features.
  `privileged: true` remains an escape hatch for clusters whose containerd
  lacks `base_runtime_spec` device injection; it falls back to the hostPath
  mechanism and MUST NOT be the production default.
]

#r("sec.psa.control-plane-restricted")[
  The `rio-system` and `rio-store` namespaces MUST enforce Pod Security
  Admission #(refs.psa)("rio-system"). Control-plane pods (scheduler, gateway,
  controller, store) set `runAsNonRoot: true`, `capabilities.drop: [ALL]`,
  `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, and
  `readOnlyRootFilesystem: true`. These are gRPC servers with no FUSE, no
  mount, no raw-socket requirements --- `restricted` is the correct floor. The
  executor namespaces stay at #(refs.psa)("rio-builders") (`rio-builders`) /
  #(refs.psa)("rio-fetchers") (`rio-fetchers`) per ADR-019; they need
  `CAP_SYS_ADMIN` for FUSE.
]

#r("sec.image.control-plane-minimal")[
  Control-plane container images (scheduler, gateway, controller, store) MUST
  contain only the component binary and its direct runtime dependencies.
  Operator tooling --- rio-cli, jq, debugging utilities --- MUST NOT be
  bundled. Admin operations run rio-cli LOCALLY via `cargo xtask k8s cli`,
  which port-forwards the gRPC endpoints and fetches the service-HMAC key from
  the cluster. Bundling tooling in the scheduler image expands the attack
  surface (every transitive dependency is an execution primitive in a
  compromised pod) and couples the control-plane release cadence to CLI
  dependency updates.
]

#info(title: [seccomp])[
  Executor pods set `seccompProfile: RuntimeDefault` at the pod level (applies
  to all containers + init containers) when `privileged != true`. RuntimeDefault
  blocks \~40 syscalls including `kexec_load`, `open_by_handle_at`,
  `userfaultfd` that builds don't need. A Localhost profile additionally
  blocking `bpf`/`setns`/`process_vm_writev` under `CAP_SYS_ADMIN` is
  available --- see `r[builder.seccomp.localhost-profile]` below.
]

#r("builder.seccomp.localhost-profile+3")[
  Executor pods MAY be configured with a Localhost seccomp profile
  (`PoolSpec.seccompProfile: Localhost`) that denies `bpf`, `setns`, and the
  cross-process *write* syscall `process_vm_writev` on top of RuntimeDefault's
  \~40-syscall denylist, while keeping the read-side trace syscalls `ptrace`
  and `process_vm_readv` permitted (the builder profile only; the fetcher
  profile denies all five). The profile JSON lives at
  `nix/nixos-node/seccomp/rio-{builder,fetcher}.json`; the chart's default
  `localhostProfile` is `operator/rio-builder.json` (fetchers hardcode
  `operator/rio-fetcher.json`) --- that path is relative to
  `/var/lib/kubelet/seccomp/`, where the file must exist on every node before a
  pod referencing it schedules. All supported targets are NixOS and bake the
  profiles via `systemd.tmpfiles` so the file is present before kubelet starts
  (`nix/nixos-node/hardening.nix` on EKS, ADR-021;
  `nix/tests/fixtures/k3s-full.nix` for k3s VM tests); see
  @sec-rationale-privileged. The controller emits no wait machinery --- a
  missing profile surfaces as the executor container's `CreateContainerError`
  with the profile path in the message.
]

The read-side trace syscalls are permitted because denying them breaks every
build whose check phase traces its own processes: LeakSanitizer's at-exit
stop-the-world attaches a tracer to the leaking process (every sanitized test
suite dies with "LeakSanitizer has encountered a fatal error"), and strace- and
gdb-driven test suites fork-and-trace. The mitigating control is the Yama LSM,
active on the builder nodes via the default `lsm=landlock,yama,bpf` kernel
command line with `kernel.yama.ptrace_scope = 1` pinned in
`nix/nixos-node/hardening.nix`: a process may only trace its own descendants,
which is exactly the capability a check phase needs and close to nothing for
lateral movement. The cross-process write syscall stays denied because no test
harness needs to write another process's memory. The residual risk accepted is
kernel attack surface --- the ptrace code paths become reachable from untrusted
build code. The cluster is single-tenant today; the intent is to revisit this
allowance before onboarding untrusted tenants.

== Key Security Properties

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([Property], [Mechanism], [Status]),
    [*Build output integrity*],
    [NAR SHA-256 verified on PutPath; ed25519 signatures],
    [Designed],

    [*Chunk integrity*],
    [@blake3 verified on every read from S3/cache],
    [Designed],

    [*Signing key protection*],
    [K8s Secret (minimum); recommend KMS/Vault for production],
    [Designed],

    [*S3 credential management*],
    [IRSA (IAM Roles for Service Accounts) on EKS],
    [Recommended],

    [*Executor isolation*],
    [Per-build @overlayfs, rio-exec sandbox, NetworkPolicy],
    [Designed],

    [*Metadata service blocking*],
    [NetworkPolicy egress deny `fd00:ec2::254` / `169.254.169.254`; IMDSv2 hop
      limit=1],
    [Designed],

    [*Inter-component encryption*],
    [Cilium WireGuard transparent encryption (overlay-level)],
    [Implemented --- `encryption.type: wireguard` in Cilium helm values],

    [*Inter-component reachability*],
    [CiliumNetworkPolicy (label-based identity)],
    [Implemented --- `infra/helm/rio-build/templates/networkpolicy.yaml`],

    [*Multi-tenant data isolation*],
    [Per-tenant @narinfo visibility filtering + per-tenant signing keys; shared
      executors with per-build overlay isolation],
    [Implemented],
  ),
)

== Derivation Validation

#r("sec.drv.validate")[
  On `PutPath`, rio-store recomputes the SHA-256 digest of the uploaded NAR
  bytes and rejects the upload if the digest does not match the `nar_hash`
  declared in the accompanying `PathInfo`. This is the core integrity check: an
  executor cannot store data under a mismatched content hash. See
  `rio-store/src/validate.rs`.
]

Additional validation checks (below) are enforced at other points in the
pipeline. These are *not* covered by `r[sec.drv.validate]` --- each has its own
tracey rule or phase deferral.

#figure(
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Check], [Where], [Status], [Description]),
    [NAR SHA-256 verification],
    [Store],
    [`r[sec.drv.validate]`],
    [On `PutPath`, the store recomputes SHA-256 over the NAR bytes and rejects
      on mismatch.],

    [Eval-time path access (daemon-era `restrict-eval`)],
    [Executor],
    [Structural],
    [rio-build never evaluates Nix expressions server-side --- derivations
      arrive pre-evaluated from the client --- so there is no evaluator whose
      path access could need restricting; the daemon-era `restrict-eval`
      `nix.conf` knob has no remaining surface.],

    [Sandbox enforcement],
    [Executor],
    [Implemented],
    [Every build runs inside the rio-exec sandbox (mount/PID/IPC/UTS/cgroup
      namespaces, plus a network namespace for everything except fixed-output builds),
      constructed unconditionally by the executor itself with no unsandboxed
      fallback (#rref("builder.exec.sandbox+3")); the daemon-era
      `sandbox = true` `nix.conf` knob is gone along with the daemon.],

    [@dag size limit],
    [Gateway + Scheduler],
    [Implemented],
    [Gateway's `translate::validate_dag` checks `nodes.len() > MAX_DAG_NODES`
      before SubmitBuild (early reject); scheduler also enforces.],

    [`__noChroot` rejection],
    [Gateway],
    [Implemented],
    [`translate::validate_dag` checks derivation env for `__noChroot=1` via
      drv_cache lookup. Rejected with "sandbox escape" error.],

    [Per-tenant store quota],
    [Gateway],
    [Implemented],
    [`TenantQuota` RPC gates `SubmitBuild` against `gc_max_store_bytes`
      (30s-TTL cached, eventually-enforcing). Per-upload NAR size uses the
      global `MAX_NAR_SIZE` limit.],

    [Output path match],
    [Gateway + Scheduler + Store],
    [Implemented],
    [Gateway: declared output paths are bound to the derivation at
      submission --- input-addressed paths must equal the recomputed
      derivation-hash paths, and declared-hash (fixed-output) outputs must
      equal the path derived from their declared hash. Scheduler: every
      inline derivation (authoritative or not) is bound to its declared
      identity at `SubmitBuild` ingress --- text content-address of the
      bytes equals the declared `.drv` path, declared paths/flags equal
      values recomputed from the bytes
      (#rref("sched.merge.ingress-inline-drv-binding")); store-backed
      nodes' claims are derived at dispatch from the store's
      text-CA-verified `.drv` bytes --- never signed from submitter
      echoes (#rref("sched.dispatch.claims-derived")), and settled
      conflicts are arbitrated against those bytes
      (#rref("sched.merge.store-evidence-displacement+1")). Store:
      HMAC assignment tokens gate registration ---
      `x-rio-assignment-token` metadata on PutPath, `store_path ∈
      claims.expected_outputs` --- and content-addressed or fixed-output
      uploads are re-verified against their descriptor and claimed path.
      Gateway bypasses via `x-rio-service-token`
      (`r[sec.authz.service-token]`).],
  ),
)

== Secrets Management

rio-build requires several secrets: SSH host keys, signing keys, database
credentials, and HMAC signing keys (assignment tokens + service tokens). There
are no application-level TLS certificates --- transport encryption is at the
Cilium overlay layer.

=== Recommended Patterns (by maturity)

*Development / single-node:*
- Kubernetes Secrets with `stringData` fields. Adequate for development but not
  for production.

*Production baseline:*
- #link("https://external-secrets.io/")[External Secrets Operator] syncing from
  AWS Secrets Manager, GCP Secret Manager, or HashiCorp Vault into Kubernetes
  Secrets. Secrets are managed externally and auto-rotated.
- Mount secrets as files (not environment variables) to avoid `/proc` and `ps`
  leakage. All rio-build secret config parameters use file paths
  (`signing_key_path`, `host_key_path`, `hmac_key_path`).

#r("sec.host-key.file-mode")[
  A gateway-generated SSH host private key MUST be written with mode `0600`
  (owner-only) at creation time. The standalone NixOS module relies on the
  auto-generate path and sets neither `UMask=` nor `StateDirectoryMode=`, so a
  plain `std::fs::write` would leave the key world-readable.
]

*Production hardened:*
- HashiCorp Vault with the Vault Agent Injector sidecar. The sidecar injects
  secrets into a shared `emptyDir` volume, and rio-build reads them from file
  paths. Vault handles rotation; the sidecar re-renders secrets on change.
- For the `database_url` credential specifically: use Vault's database secrets
  engine to issue short-lived PostgreSQL credentials per pod, eliminating
  static database passwords entirely.

=== Secret Inventory

#figure(
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Secret], [Used By], [Rotation], [Status]),
    [SSH host key (`ssh_host_ed25519_key`)],
    [Gateway],
    [Rarely (causes client known_hosts warnings)],
    [Implemented],

    [Authorized SSH keys#footnote[
        The `authorized_keys` comment field carries the tenant name (e.g.,
        `ssh-ed25519 AAAA... acme`). The gateway resolves this to a tenant UUID
        via `SchedulerService.ResolveTenant` on SSH accept and mints a
        per-session JWT with `Claims.sub = tenant_id`.
      ]],
    [Gateway],
    [Per-tenant lifecycle],
    [Implemented (flat file; no tenant annotation)],

    [NAR signing key (`signing-key`)],
    [Store],
    [Annually or on compromise],
    [Implemented],

    [HMAC signing key (assignment tokens)],
    [Scheduler + Store],
    [Annually or on compromise],
    [Implemented --- `RIO_HMAC_KEY_PATH`, same key file both sides],

    // Derived from `rg 'HmacSigner::load' rio-*/src/` (Rust minters)
    // + helm `rio-service-hmac` mounts (dashboard); verifiers from
    // `rg 'ensure_service_caller|service_verifier' rio-*/src/`.
    [HMAC signing key (service tokens)],
    [Controller, CLI, Scheduler, Gateway, Dashboard (mint); Scheduler, Store
      (verify)],
    [Annually or on compromise],
    [Implemented --- `RIO_SERVICE_HMAC_KEY_PATH`; minters {controller, cli,
      scheduler, gateway, dashboard}, verifiers {scheduler `AdminService`,
      store `StoreAdminService` + `PutPath`}],

    [JWT signing key (tenant tokens)#footnote[
        The gateway mints a per-session JWT on SSH accept (`mint_session_jwt`,
        `r[gw.jwt.issue]`). Downstream services verify via
        `rio_auth::jwt_interceptor::JwtLayer` with SIGHUP-reloadable public key.
        Dual-mode fallback (`r[gw.jwt.dual-mode]`): when JWT is disabled,
        services fall back to `SubmitBuildRequest.tenant_name`.
      ]],
    [Gateway],
    [Annually; SIGHUP reload for zero-downtime],
    [Implemented --- `RIO_JWT__KEY_PATH`, gateway mints per-session JWT on SSH
      accept],

    [Database credentials (`database_url`)],
    [Scheduler, Store, Controller],
    [Via Vault database engine or External Secrets],
    [Implemented],
  ),
)

== Additional Threats

=== Signing Key Compromise/Rotation

- *Threat*: Leaked signing key allows an attacker to sign arbitrary store paths
  as trusted.
- *Mitigation*: Store signing keys in KMS/Vault (not raw K8s Secrets) for
  production deployments. See the rio-store key-rotation procedure. Keys should
  be rotated annually or immediately on suspected compromise.

=== DAG-Based Resource Exhaustion

- *Threat*: A malicious or buggy client submits a derivation DAG with millions
  of nodes, exhausting scheduler memory and CPU.
- *Mitigation*: Per-tenant limits on maximum DAG size (`max_dag_size`) and
  maximum concurrent builds (`max_concurrent_builds`). See Multi-Tenancy for
  quota configuration.
- *Implementation (Phase 3b):* `max_dag_size` is enforced at BOTH gateway
  (`translate::validate_dag`) and scheduler. Gateway-side check is early
  rejection --- saves the gRPC round-trip for obvious over-size submissions.

=== Build-Time Secrets

- *Threat*: Fixed-output derivations (FODs) needing credentials (e.g., private
  GitHub repos) require network access and authentication during build.
- *Mitigation*: FODs execute on dedicated fetcher pods with open egress; the
  @fod hash check is the integrity boundary. Per-tenant credentials are injected
  via fetcher pod env from Secrets, never via builder pods. See ADR-019.

=== FOD Network Isolation

- *Threat*: FOD builds require internet egress. A compromised build could
  exfiltrate secrets or call home; a compromised upstream could serve tampered
  content.
- *Design*: Per ADR-019 §Network isolation, builds and fetches run on separate
  executor kinds with opposite network policies:
  - *Builders* (`rio-builders` namespace) are airgapped --- egress to CoreDNS,
    rio-scheduler, rio-store only. No internet, no proxy. See
    `r[builder.netpol.airgap]`.
  - *Fetchers* (`rio-fetchers` namespace) get egress via Cilium `toEntities:
    [world]` on ports 80/443 --- address-family-agnostic, and inherently
    excludes cluster, node, and host identities. See
    `r[fetcher.netpol.egress-open]`.
  - The FOD hash check (`r[builder.fod.verify-hash]`) is the integrity
    backstop: tampered content fails `verify_fod_hashes()` before upload.
  - The scheduler NEVER routes a FOD to a builder, even under fetcher pressure
    (`r[sched.dispatch.fod-to-fetcher]`).
- *Formerly:* a Squid FOD proxy with domain allowlisting. Deleted in
  ADR-019 --- the hash check is sufficient; a domain allowlist adds operational
  friction for marginal gain.

=== Log Injection

- *Threat*: Untrusted build output is displayed in the dashboard log viewer.
  Malicious builds could inject HTML/JavaScript into logs.
- *Mitigation*: The dashboard must sanitize all log content as raw text. Never
  render log lines as HTML. Use `<pre>` elements or equivalent with proper
  escaping.

=== Cross-Tenant Chunk Probing

- *Threat*: `FindMissingChunks` can reveal whether another tenant has built a
  specific package.
- *Mitigation*: Per-tenant chunk scoping (at the cost of dedup) or accept the
  risk. See Multi-Tenancy §FindMissingChunks scoping.

== Ephemeral Builders

Builder pods are *always* one-shot Jobs (see `r[ctrl.pool.ephemeral]` in the
controller spec): one pod per build, zero shared state. Each build gets a fresh
emptyDir for the @fuse cache and overlayfs upper --- an untrusted tenant cannot
leave behind poisoned cache entries, doctored overlays, or stale mount points
for the next build, because there is no "next build" on that pod. The pod
terminates after one `CompletionReport`; K8s reaps the Job via
`ttlSecondsAfterFinished`.

*What this does NOT provide* (limitations \#1--3 below still apply): the Nix
sandbox is still a purity boundary, not a security boundary. A malicious
derivation can still attempt sandbox escape and gain `CAP_SYS_ADMIN` within the
pod. The one-shot model limits the BLAST RADIUS of such an escape --- the
attacker is confined to one pod with no persistent state to poison and no
cached inputs from other tenants to exfiltrate.

*Recommended combination* for untrusted multi-tenant:

#figure(
  table(
    columns: 3,
    align: (left, left, left),
    table.header([Layer], [Mechanism], [What it provides]),
    [Pod lifetime],
    [One-shot Job (always on)],
    [Zero cross-build state; no cache/overlay poisoning],

    [User namespace],
    [`hostUsers: false` (K8s 1.33+)],
    [`CAP_SYS_ADMIN` scoped to unprivileged host UIDs (see limitation \#2)],

    [Seccomp],
    [`PoolSpec.seccompProfile: Localhost`],
    [`bpf`/`setns`/`process_vm_writev` denied; read-side tracing
      (`ptrace`/`process_vm_readv`) Yama-confined to descendants (see
      `r[builder.seccomp.localhost-profile]`)],

    [Node isolation],
    [Dedicated tainted node pool],
    [Sandbox escape confined to builder nodes],

    [Network],
    [NetworkPolicy egress deny],
    [No exfil to arbitrary endpoints (FODs route to kind=Fetcher pools)],
  ),
)

The cost is per-build cold start (\~10--30s pod scheduling + FUSE mount +
heartbeat) plus one reconciler tick (\~10s).

== Known Limitations

+ *The rio-exec sandbox is NOT a security boundary.* It prevents builds from
  accessing undeclared inputs (purity) but does not prevent a determined
  attacker from escaping. For multi-tenant deployments, the security boundary
  is the executor pod + node isolation.

+ *Executors require `CAP_SYS_ADMIN`.* This capability enables mount namespace
  manipulation, which is powerful. `seccompProfile: RuntimeDefault` blocks \~40
  syscalls (`kexec_load`, `open_by_handle_at`, etc.), but `CAP_SYS_ADMIN` still
  grants significant host access. The Localhost @seccomp profile
  (`r[builder.seccomp.localhost-profile]`) additionally blocks
  `bpf`/`setns`/`process_vm_writev` (read-side tracing stays available to
  check phases, confined to descendants by Yama) --- production deployments
  should set `PoolSpec.seccompProfile: {type: Localhost, localhostProfile:
  operator/rio-builder.json}` (the chart default). Dedicated node pools with
  taints are essential. *Mitigation (K8s 1.33+):* Executor pods must set
  `hostUsers: false` to enable user namespace isolation. With user namespaces,
  `CAP_SYS_ADMIN` applies only within the user namespace, not on the host ---
  the attacker gains capabilities within a namespace that maps to unprivileged
  host UIDs, significantly reducing the blast radius. See
  @sec-rationale-privileged.

+ *`CAP_SYS_ADMIN` is held throughout build execution.* The executor cannot
  drop `CAP_SYS_ADMIN` between overlay setup and build completion because the
  rio-exec sandbox itself requires mount namespace manipulation. A sandbox escape
  gives the attacker `CAP_SYS_ADMIN` capabilities within the user namespace
  (see mitigation in \#2). Additional mitigations: RuntimeDefault or Localhost
  seccomp (`r[builder.seccomp.localhost-profile]`), dedicated node pools, and
  NetworkPolicy. Future work: explore splitting the executor into a privileged
  setup process and an unprivileged build supervisor.

+ *Cross-tenant chunk deduplication leaks build activity.* A tenant can probe
  `FindMissingChunks` to determine whether another tenant has built a specific
  package. Mitigation: scope `FindMissingChunks` per tenant (at the cost of
  dedup savings) or accept the risk with documentation.

+ *Fixed-output derivations (FODs) need network access.* FOD builds (fetchurl,
  fetchgit) require egress to the internet, which conflicts with the builder
  airgap. FODs route to dedicated fetcher pods with open egress; the hash check
  is the integrity boundary (see §FOD Network Isolation).

= Rationale

== Privileged builder pods // supersedes ADR-012
<sec-rationale-privileged>

Workers require Linux kernel capabilities for two operations: overlayfs mounts
(per-build isolation) and rio-exec build sandboxing (mount/PID/IPC/UTS/cgroup
namespaces, `pivot_root`). Kubernetes pod security must grant these
capabilities without opening unnecessary attack surface.

Worker pods request `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT` via the container
security context. Critically, `privileged: true` is NOT used --- it disables
seccomp profiles entirely and grants all capabilities. The recommended
deployment configuration includes dedicated node pools with taints (so worker
pods only schedule on designated nodes), a custom seccomp profile that allows
the specific syscalls needed (mount, unshare, pivot_root, clone with namespace
flags) while blocking everything else, and pod security admission at the
namespace level to enforce these constraints.

Kubernetes 1.33 (April 2025) enabled user namespace support by default. Worker
pods set `hostUsers: false` to activate user-namespace isolation: container
UIDs are remapped to unprivileged host UIDs, so even with `CAP_SYS_ADMIN` the
capability applies only within the user namespace, not on the host. This
significantly reduces the blast radius of a container escape --- an attacker
gaining `CAP_SYS_ADMIN` inside the pod cannot use it to affect the host or
other pods. `CAP_SYS_ADMIN` is still required (for FUSE mount + overlayfs + Nix
sandbox), but its scope is contained to the user namespace. rio-build requires
Kubernetes 1.33+ as the minimum version to ensure user-namespace support is
available. `hostUsers: false` does NOT eliminate the need for `CAP_SYS_ADMIN`
or the custom seccomp profile; it adds a defense-in-depth layer on top of the
existing mitigations.

*Alternatives considered.* `privileged: true` pods are the simplest
configuration but grant all Linux capabilities, disable seccomp, and give
access to host devices --- an unacceptable security posture for a multi-tenant
build service. Unprivileged builds with user namespaces only would let Nix's
sandbox work without `CAP_SYS_ADMIN` if the kernel allows unprivileged user
namespaces, but overlayfs still requires `CAP_SYS_ADMIN` in the initial user
namespace (rootless fuse-overlayfs performs significantly worse). Specialized
runtimes (sysbox, Kata Containers) provide stronger isolation but add runtime
dependencies, complicate cluster management, and may be unavailable in managed
Kubernetes services. Building inside a nested VM (Firecracker/gVisor) gives
maximum isolation but adds significant startup latency and resource overhead;
Nix builds already use namespace-based sandboxing, making VM isolation
redundant for most threat models.

*Consequences.* Workers get exactly the capabilities they need without
excessive privilege, and a custom seccomp profile limits syscall surface to
what is actually required. Dedicated node pools with taints prevent worker pods
from affecting other workloads. On the negative side: cluster-level
configuration (node pools, taints, seccomp profiles) complicates initial setup;
`CAP_SYS_ADMIN` is a broad capability and the seccomp profile is the real
security boundary; and the configuration is incompatible with
PodSecurityStandard `restricted` --- it requires `privileged` or `baseline`
with custom exceptions.

The builder-side mechanics (containerd `base_runtime_spec` device injection,
`cgroup_writable`, seccomp profile distribution) are specified in the builder
component chapter.
