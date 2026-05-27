# ADR-022: Mount-admission credentials — scheduler-signed Ed25519 tokens, node-scoped, no minting material on builder nodes

Status: **Accepted** (owner decisions recorded 2026-05-27).

**Scope:** the credential a builder pod presents to `rio-mountd` to be admitted on the node-local UDS (`/run/rio-mountd/mountd.sock`). Supersedes the per-cluster symmetric `rio/mountd-hmac` scheme that landed under the §P0559 socket-access row of the [implementation plan](./022-implementation-plan.md) — wired in code/helm/bootstrap but **never provisioned in any cluster**. Sequencing is [implementation plan §P0590](./022-implementation-plan.md). Three credential designs (and a red-team review of each) plus a three-way evaluation of node-scoping mechanisms were developed off-repo; this document records the decisions, the chosen architecture, and the rejected alternatives in condensed form.

---

## 1. Problem and owner constraint

Builder nodes host only untrusted workloads; node compromise and malicious build content are the normal case. The shipped k8s admission path puts a cluster-wide **symmetric** key (`rio/mountd-hmac`) on every builder node: each node's verifier *is* a signer, so rooting one node yields the ability to mint Mount-admission tokens for any build_id/tenant that every other node accepts, recoverable only by a flag-day rotation.

Owner constraint: **compromising one node must not yield credential-minting ability anywhere else — ideally no minting ability at all.** Properties that must not regress: per-build isolation, key separation from the store-facing assignment key (`builder.mountd.token-key-separate`), the fail-closed keyless default, the gid-990 standalone path, rollout/skew safety, and rio-mountd's offline posture (no kube API, no PG, no network egress; root + `CAP_SYS_ADMIN` but not `privileged`). The owner's initial sketch — "mint a key for every builder and record it in PG" — was evaluated as candidate D2 (§4) and is not the adopted mechanism; the instinct behind it (one node's material must not be valid anywhere else) is delivered in a strictly stronger form.

A second residual from §P0559 is pulled into scope by decision 4: a per-build token stolen on node A is otherwise presentable to node B's mountd for its remaining TTL (cross-node build_id squat).

## 2. Decision record (2026-05-27)

1. **mountd tokens: Ed25519 only, greenfield** — the never-provisioned symmetric mountd-HMAC path is not kept; the symmetric arm is deleted in the final phase.
2. **Trust-root rotation: restart-based file reads** (rolling DaemonSet restart), no SIGHUP hot-reload.
3. **The PG table ships**: `assignments.node_name` (per-token issuance audit) + `builder_nodes(node_name PK, first_seen, last_seen, retired_at)`, scheduler-upserted from controller acknowledgements, retired via the existing dead-node sweep; **never consulted at mint or verify time**.
4. **Node-scoped claims are in scope now** (not deferred): `MountdClaims` gains a `node` claim resolved at the existing dispatch mint site from controller-attested placement (pod-name-keyed binding; executor register-time `RIO_NODE_NAME` as fallback), and mountd verifies it against its own node name from the downward API; a `require | prefer` knob defaults to `require` (defer the drv one dispatch pass if placement is unresolvable). The owner's original proposal (mountd self-minted identity + node-up handshake with the scheduler) was evaluated and is recorded as the rejected alternative (§4), with per-node identity (a `node_fp` claim) noted as the future upgrade path if sealed per-node secrets/attestation ever arrive.

Operational context: there are currently no long-lived clusters — only wipeable dev deploys — so a greenfield cutover (provision the new keypair, never enable the symmetric path) is acceptable.

## 3. Architecture

### 3.1 Trust chain

```text
PROVISIONING (per cluster; bootstrap Job, idempotent)
  Secrets Manager: rio/mountd-signing-key (PRIVATE)    rio/mountd-signing-pub (PUBLIC, one line per active key)
       └─ ESO ─▶ Secret rio-mountd-signing-key → rio-system only   → scheduler Deployment
       └─ ESO ─▶ Secret rio-mountd-signing-pub → rio-builders only → every mountd DS pod  (transport only; not secret)

PER BUILD
  scheduler dispatch: resolve target node from controller-attested placement, then sign
      MountdClaims{aud, build_id, tenant, node, issued, expiry} with the PRIVATE key
        → WorkAssignment.mountd_token (opaque string on the executor-token-gated stream)
  builder pod: retains the token, never parses it; sends Mount{build_id, token} (+ /dev/fuse dup via SCM_RIGHTS); re-sends on re-Mount
  rio-mountd: verify signature against PUBLIC trust roots → expiry → aud → claims.build_id == requested → claims.node == own node name
        → admit, or one opaque Unauthorized + close (reasons only in logs/metrics)

STANDALONE/systemd: unchanged — no keys, gid-990 SO_PEERCRED admits, socket 0660.
```

The whole model in one sentence: the scheduler signs a small per-build claims blob with a key only the control plane has; every mountd checks it against a public-key file plus its own node name; gid-990 peers skip tokens; no keys configured ⇒ gid-only 0660.

### 3.2 Claims and envelope

- Claims: today's `MountdClaims` (`aud:"rio-mountd"`, `build_id` = sanitized drv basename, `tenant` audit-only, issued/expiry = the assignment-token TTL, `deny_unknown_fields`) **plus `node`** (the kube `spec.nodeName` of the target node) in the first shipped Ed25519 claims version, **plus a reserved, unenforced `node_fp: Option<String>`** so identity-keyed binding can be added later without a claims-format break. Because the claims are `deny_unknown_fields`, the node field must ship in the first asymmetric claims version — this is what makes "node scoping now" cheap and "node scoping later" a breaking change, and is part of why decision 4 pulls it in.
- Envelope: `rmt2.<b64url(claims_json)>.<b64url(ed25519_sig_64B)>`; the signature covers the transmitted bytes before the last dot, so the version tag is downgrade-bound. Verification order preserves "no serde_json on unauthenticated bytes": split → decode signature → try each trust root → only then parse claims → expiry → aud → build_id → node. Two-segment legacy HMAC tokens are accepted only while an HMAC key is configured (production never configures one; the arm is deleted in Phase 5).
- Trust roots: the verifier tries all loaded roots (≤3 during rotation, hard cap 8, duplicate names rejected); the verifying key *name* (from its own list, never attacker input) is logged on success. Signatures must decode to exactly 64 bytes, public keys to exactly 32. Key names must carry the `rio-mountd-` prefix; loaders hard-fail otherwise (prevents cross-wiring with the narinfo keypair, which uses the same `name:base64` encoding).
- New reject reasons: `node-mismatch`, `node-missing`. When `--node-name` is set (helm always sets it via the downward API), an `rmt2` token without a node claim is rejected; when unset (standalone/gid-990), the node check is skipped and nothing else changes.

### 3.3 Node resolution at mint, verification at Mount

- **Scheduler (mint):** the token is minted inside WorkAssignment construction for an already kube-bound executor, so placement precedes signing — "node unknown at dispatch" is a knowledge problem, not an ordering problem. The target node resolves as: controller-attested binding keyed by pod name (`BoundIntent.pod_name`, new proto field populated by the controller's spawned-intent acks) → the existing intent-keyed `authoritative_binding` → the executor's own register-time `RIO_NODE_NAME` report (`ExecutorRegister.node_name`, new field; used only to scope that executor's own token, never written into `authoritative_binding`, never read by hung-node detection) → none ⇒ config `mountd_node_binding = require | prefer` (default `require`: defer the drv to the next dispatch pass; `prefer` mints unbound, which strict mountds reject). A `rio_scheduler_node_binding_mismatch_total` metric counts source disagreement (controller wins); the resolved node is logged next to the signing-key name.
- **mountd (verify):** new `--node-name` / `RIO_MOUNTD_NODE_NAME` flag (helm: `fieldRef: spec.nodeName`); compares `claims.node` for equality. No other changes — still no network/kube/PG in the daemon.
- A lying executor can only mis-scope *its own* token to a node it is not on, which then fails at Mount time — self-defeating, so the fallback needs no extra authentication.
- The node binding survives mountd restart and builder reconnect automatically (it is a name, not an instance key), so the retained-token re-Mount path is unaffected.

### 3.4 Key material and lifecycle

| Material | Format | Holder | Role |
|---|---|---|---|
| `rio/mountd-signing-key` | `rio-mountd-<n>:base64(64 B Ed25519 secret)` (32 B seed accepted) | scheduler only (rio-system) | signs Mount-admission tokens |
| `rio/mountd-signing-pub` | one `rio-mountd-<n>:base64(32 B pubkey)` line per active key | every mountd DS pod (rio-builders) | verifies; public, mints nothing |
| `rio/mountd-hmac` (shipped, never provisioned) | raw 32 B | — | retired by this design; never deployed |

- **Generation:** the bootstrap Job gains a keypair block cloned from the existing `rio/signing-key{,-pub}` dual-guard pattern; `spike_mountd_client keygen` produces the same files for VM tests/standalone.
- **Distribution:** ESO → Secrets → file mounts, read once at process start (`RIO_MOUNTD_SIGNING_KEY_PATH` on the scheduler, `RIO_MOUNTD_PUBKEY_PATH` on the DS). Configured-but-unreadable/empty is a startup error; nothing configured stays gid-only.
- **Rotation (zero-downtime, restart-based per decision 2):** (1) append the key-2 public line in Secrets Manager; (2) confirm ESO refresh, roll the DS, and verify via the mandatory `rio_mountd_trust_root{key_name}` gauge that every mountd reports key-2 (DS restarts are non-disruptive: clients hold the fuse fd and re-Mount with retained tokens); (3) swap the private key and restart the scheduler (rollback = unset the signer only; never remove trust roots while v2 tokens are in flight); (4) after one TTL-cap window, drop the key-1 line and roll the DS. In-flight builds never invalidate.
- **Compromise:** builder node — nothing to rotate; fail/re-dispatch its builds if desired. Scheduler private key — control-plane compromise; run the rotation with step 4 immediate.

### 3.5 What PG records (decision 3)

One migration, scheduler-written, never read at mint or verify time:

- `ALTER TABLE assignments ADD COLUMN node_name TEXT` — populated by `record_assignment` with the node the token was scoped to (NULL if minted unbound under `prefer`). Per-dispatch issuance audit.
- `CREATE TABLE builder_nodes(node_name TEXT PRIMARY KEY, first_seen TIMESTAMPTZ NOT NULL, last_seen TIMESTAMPTZ NOT NULL, retired_at TIMESTAMPTZ)` — upserted by the scheduler from controller acknowledgements; `retired_at` set when the node appears in the existing dead-node sweep (plus a last_seen TTL). Operator/dashboard visibility of node lineage; structurally shaped like `cluster_key_history` (public facts), not like `tenant_keys` (private seeds). Standard migration-freeze rules apply.
- The optional `mountd_signing_keys` audit table stays **skipped**: issuance is already auditable via the `assignments` row + the scheduler log line carrying the signing-key name, and key state is auditable via Secrets Manager + the mounted files + the trust-root gauge.

### 3.6 Config and observability surface

| Surface | Addition |
|---|---|
| scheduler config | `mountd_signing_key_path` / `RIO_MOUNTD_SIGNING_KEY_PATH` (signer; replaces `mountd_hmac_key_path` after Phase 5); `mountd_node_binding = "require" \| "prefer"` (default `require`) |
| mountd flags | `--token-pubkey-path` / `RIO_MOUNTD_PUBKEY_PATH` (trust roots); `--node-name` / `RIO_MOUNTD_NODE_NAME` (downward API). Token mode (and the 0666 socket rule) = any verifier configured — rule unchanged |
| helm values | `mountdSigning.{privateKeySecretName,publicKeySecretName}` (defaults empty ⇒ keyless ⇒ gid-only); DS gets the node-name fieldRef whenever signing is configured |
| metrics | `rio_mountd_trust_root{key_name}` gauge (mandatory rotation precondition); `rio_scheduler_node_binding_mismatch_total`; existing mountd reject counter gains `node-mismatch` / `node-missing` reason labels |
| logs | scheduler dispatch logs the signing-key name + resolved node; mountd logs the verifying key name on success; rejects stay opaque on the wire, detailed in logs/metrics only |

### 3.7 Blast radius (end state)

Root on a builder node obtains: the Ed25519 public trust roots (worthless for minting), the per-build expiring tokens of builds already placed on **that node** (replayable only for those exact build_ids on that node — the node claim removes the cross-node squat), and what node root always had locally (gid-0 bypass of its own broker, its own node's cache). It does **not** obtain the ability to mint Mount admission for any other build/tenant/node, nor any store/scheduler/tenant credential. Recovery from node compromise requires no key rotation.

## 4. Considered alternatives

Full evaluations (criteria matrices, red-team reviews) were performed off-repo; summarised:

- **Per-builder keys in a PG registry (D2; the owner's original sketch).** Per-node keys with seeds in PG and a controller-maintained keyring relocate the cluster-wide minting secret into a table reachable through the shared `rio-postgres` DSN that the store and controller already consume — a new minting capability for the store and a spirit-level regression of key separation unless a dedicated role/DSN is built first — and add key-provisioning/rotation races that fail builds, node-churn key hygiene, and a dispatch↔controller liveness coupling. The verifier is the node and the node is deliberately offline, so PG can never sit in the verification path without ending that posture. Rejected; D1+node-claim achieves a strictly stronger property (no minting material on nodes at all) with fewer moving parts.
- **Attested builder identity / pod proof-of-possession (D3 Layer B).** A per-pod ephemeral key and proof-of-possession on Mount would make a grant captured off-node unusable, but adds a proto field, a third Mount wire shape, client-side signing, and freshness handling for a bearer-leak channel that does not exist today (tokens are neither logged nor persisted). Deferred until there is a real second consumer of pod identity or an actual leak channel; it layers on top of this design without rework.
- **mountd self-minted identity + node-up handshake with the scheduler (M1; the owner's proposed node-scoping mechanism).** Registering a per-node identity at startup requires a new mountd→scheduler connection through a CNP that is deny-all today, a registration endpoint reachable by untrusted builder pods, and — to avoid re-opening the very cross-node replay it exists to close — a projected SA token + TokenReview inside the CAP_SYS_ADMIN pod plus scheduler RBAC, a registry with GC/conflict arbitration, and a persisted key on the node. It still needs the same dispatch-time node resolution as the adopted design, so it is that work *plus* a registration subsystem. Rejected. Its genuine advantage — a cryptographic node identity that survives node-name reuse — is preserved as the upgrade path: the reserved `node_fp` claim, fillable later either by M1-with-TokenReview or by a cheaper "identity echo" (mountd persists a keypair, publishes the fingerprint beside its UDS, the builder relays it on its existing stream), neither requiring rework of what ships here.
- **mountd as per-node token authority (M2: voucher relay / countersign / on-node minting).** Every variant either re-introduces an admission signer on hostile nodes or needs a control-plane channel into mountd, plus a second credential lifecycle, for zero marginal security over the node claim. Dominated; not carried further.

## 5. Residual risks

- **Same-name reincarnation window:** an unexpired token stolen from node A is presentable on a *future* node that reuses A's exact node name within the TTL — strictly narrower than today's any-node squat; closable later via `node_fp` if ever justified.
- **Scheduler private-key compromise** mints tokens every node accepts — already control-plane compromise; rotation runbook applies, and there is no per-node secret to also chase.
- **Same-node replay:** tokens of builds already placed on a compromised node remain replayable for those build_ids on that node until expiry (the bounded nuisance accepted under §P0559 too).
- **`prefer` mode mints unbound tokens** that strict mountds reject; the default is `require` and the knob exists only as an operational escape hatch.

## 6. Explicit non-goals

- Per-node or per-pod **signing** keys, a PG key registry, or any PG/kube/network dependency in mountd (keeps the offline posture).
- Pod proof-of-possession (D3 Layer B) — deferred as above.
- Revocation faster than expiry; terminal-state revocation remains a store/scheduler concern.
- Changing the assignment/executor/service token schemes (their verifiers are control-plane services; the rule is "no symmetric verifier on untrusted nodes", not "no HMAC anywhere").
- uid-floor (`--min-peer-uid`) and pre-auth connection caps: orthogonal hardening, separate follow-up.
- Any change to the gid-990 standalone path, the socket-mode derivation, or the per-build isolation invariants.

## 7. Phased delivery

Sequenced as [implementation plan §P0590](./022-implementation-plan.md); each phase independently shippable and gated by the full checks gate.

1. **Phase 1 — rio-auth credential module** (no behavior change): `MountdSigningKey`, `MountdTrustRoots`, `MountdVerifier`, `MountdClaims` + `node`/reserved `node_fp`, `verify_for_build(expected_node)`; round-trip/tamper/multi-root/cross-scheme-rejection tests.
2. **Phase 2 — daemon + scheduler wiring, proto, controller, spec, vm-mountd**: pubkey + node-name flags on mountd, signer + node-resolution chain + `require|prefer` knob on the scheduler, `ExecutorRegister.node_name` + `BoundIntent.pod_name`, spec-rule updates (existing admission rule amended; new no-node-mint and node-scoped rules), full vm-mountd admit/reject matrix incl. rotation overlap, wrong-node, missing-claim, node-name-unset, and restart-re-Mount cases.
3. **Phase 2b — PG audit migration** (independent micro-phase): `assignments.node_name` + `builder_nodes` + scheduler upsert/retire wiring.
4. **Phase 3 — helm/ESO/bootstrap/xtask cutover** (greenfield): `mountdSigning.{privateKeySecretName,publicKeySecretName}` + node-name fieldRef, render guards (private key never on rio-builders objects, pub never on the scheduler, pair-or-nothing, mountdSigning and mountdHmac mutually exclusive, keyless default renders nothing), bootstrap keypair block, `xtask eks deploy` switches to `mountdSigning.*` and stops setting `mountdHmac.secretName`.
5. **Phase 4 — EKS smoke** (runbook checklist, not a merge-gate derivation): bootstrap + ESO sync to the right namespaces, a real `hostUsers:false` executor completes the Mount round-trip with an `rmt2` token (closes §P0559 residuals 1–2 for the new scheme), a rotation drill watched via the trust-root gauge, and a cross-node negative probe (token scoped to node A presented on node B ⇒ opaque Unauthorized + `node-mismatch` metric) plus the PG audit rows appearing.
6. **Phase 5 — delete the symmetric arm**: remove the HMAC verifier variant, `--token-key-path`, `mountd_hmac_key_path`, the `mountdHmac` helm family, its ESO and bootstrap blocks; after this a builder node cannot even be configured to hold a mountd signing secret.

## 8. Cross-references

- [Implementation plan §P0590](./022-implementation-plan.md) — file-level sequencing, exit criteria; §P0559's socket-access row records the superseded mountd-HMAC scheme and now points here.
- [Design Overview §11 — Privilege boundary](./022-design-overview.md#11-privilege-boundary) — the UDS protocol, gid-990/token two-mode admission, and the existing `builder.mountd.token-admission` / `token-key-separate` rules this design amends (spec text changes land with Phase 2, not with this ADR).
- [ADR-022 §2.5](./022-lazy-store-fs-erofs-vs-riofs.md) — the Mount/fd-handoff sequence the credential rides on.
- [Closure-scoped castore reads](./022-closure-scoped-castore-reads.md) — the companion decision for the *store-facing* assignment token; the two credentials remain separate key families by design.
