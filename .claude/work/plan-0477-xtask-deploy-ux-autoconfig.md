# Plan 477: xtask k8s deploy — auto-configure JWT, SSH tenant, force-rollout

Manual rsb testing on kind identified three UX papercuts that make "deploy and use" require manual post-steps. Each is small; together they're the difference between `cargo xtask k8s deploy kind && nix build --store ssh-ng://...` working first-try versus requiring a runbook.

**The three gaps** (from [`xtask/src/k8s/`](../../xtask/src/k8s/) + [`ssh.rs:30`](../../rio-gateway/src/ssh.rs)):

1. **JWT disabled by default.** `values.yaml` has `jwt.enabled: false`. Deploy works, but authenticated features (tenant-scoped substitution, quota) silently degrade. For kind/k3s dev clusters, xtask should auto-generate an ed25519 keypair, write the secret, and set `--set jwt.enabled=true` in the helm install.

2. **SSH authorized_keys lacks tenant comment.** The gateway parses the ssh key comment field to resolve tenant-id. A bare key (`ssh-ed25519 AAAA...`) with no comment means the connection is anonymous. xtask should write the dev key with a `default` (or configurable) tenant comment.

3. **Same-tag `:dev` push doesn't restart pods.** `skopeo copy` to `:dev` succeeds, but the Deployment spec is unchanged (same image tag), so kube doesn't roll the pods. xtask should `kubectl rollout restart deployment/...` after a successful push when the tag is unchanged.

Discovered during P0471-P0473 rsb iteration; each required a manual workaround.

## Entry criteria

- P0471 merged (`0163cc3a` — JWT threading to FindMissingPaths). Already on `sprint-1`.

## Tasks

### T1 — `feat(xtask):` auto-generate JWT keypair for kind/k3s deploy

MODIFY [`xtask/src/k8s/kind/`](../../xtask/src/k8s/kind/) and [`xtask/src/k8s/k3s/`](../../xtask/src/k8s/k3s/) deploy flows (likely `shared.rs` if the logic is common). Before `helm install`:

```rust
// Auto-enable JWT for dev clusters. Generate keypair, write as
// k8s Secret, pass --set jwt.enabled=true to helm.
if !jwt_secret_exists(&client, ns).await? {
    let (sk, pk) = ed25519_dalek::SigningKey::generate(&mut OsRng);
    write_jwt_secret(&client, ns, &sk, &pk).await?;
    ui::step_ok("Generated JWT signing keypair");
}
helm_args.push("--set".into());
helm_args.push("jwt.enabled=true".into());
```

Skip on `eks` provider — production clusters manage JWT keys via external secret stores, not xtask.

### T2 — `feat(xtask):` write SSH authorized_keys with default tenant comment

MODIFY the ssh-key provisioning step in [`xtask/src/k8s/shared.rs`](../../xtask/src/k8s/shared.rs) (or wherever `authorized_keys` is assembled — grep for `authorized_keys` near [`mod.rs:292`](../../xtask/src/k8s/mod.rs)). Append the tenant-id as the key comment:

```rust
// Gateway's auth_publickey parses the comment for tenant-id.
// "default" matches the default tenant created by migrations.
let entry = format!("{} {} default", key_type, key_b64);
```

Add a `--tenant <name>` flag to override `default` if the user has created a named tenant.

### T3 — `feat(xtask):` force rollout restart after same-tag push

MODIFY the image-push step (grep for `skopeo copy` in [`xtask/src/k8s/`](../../xtask/src/k8s/)). After a successful push to an existing tag:

```rust
if tag_unchanged {
    // Same tag → Deployment spec unchanged → pods keep old image.
    // Force rollout so the new image is pulled.
    run("kubectl", &["rollout", "restart", "deployment", "-n", ns,
                     "-l", "app.kubernetes.io/part-of=rio-build"])?;
    ui::step_ok("Forced rollout restart (same-tag push)");
}
```

The `-l app.kubernetes.io/part-of=rio-build` selector catches all rio deployments without hardcoding names.

### T4 — `test(xtask):` smoke test for auto-config outputs

NEW test in [`xtask/tests/`](../../xtask/tests/) (or extend an existing k8s smoke). Assert:
- JWT secret exists post-deploy
- `authorized_keys` configmap contains `default` (or `--tenant` value) in comment position
- Deploy command exit-0 includes `rollout restart` in output when tag matches

## Exit criteria

- `cargo xtask k8s deploy kind` followed immediately by `nix build --store ssh-ng://rio@localhost:2222 nixpkgs#hello` succeeds with zero manual steps
- `kubectl get secret rio-jwt -n rio` → exists post-deploy (T1)
- `kubectl get configmap rio-ssh-keys -o jsonpath='{.data.authorized_keys}'` → contains `default` comment (T2)
- Second `cargo xtask k8s deploy kind` (no changes) triggers rollout restart (T3 — verify via `kubectl rollout status`)
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[gw.jwt.issue]` — T1 makes JWT auto-enabled for dev clusters so this path is exercised by default
- `r[sched.tenant.resolve]` — T2 makes the tenant-comment resolve path work out-of-box

## Files

```json files
[
  {"path": "xtask/src/k8s/shared.rs", "action": "MODIFY", "note": "T1: JWT keypair gen; T2: tenant comment; T3: rollout restart"},
  {"path": "xtask/src/k8s/kind/mod.rs", "action": "MODIFY", "note": "T1: wire JWT auto-gen into kind deploy"},
  {"path": "xtask/src/k8s/k3s/mod.rs", "action": "MODIFY", "note": "T1: wire JWT auto-gen into k3s deploy"},
  {"path": "xtask/src/k8s/mod.rs", "action": "MODIFY", "note": "T2: --tenant flag"},
  {"path": "xtask/tests/k8s_deploy.rs", "action": "MODIFY", "note": "T4: smoke asserts (NEW if no existing k8s test file)"}
]
```

```
xtask/src/k8s/
├── shared.rs       # T1-T3: core logic
├── kind/mod.rs     # T1: wire
├── k3s/mod.rs      # T1: wire
└── mod.rs          # T2: flag
xtask/tests/
└── k8s_deploy.rs   # T4: smoke
```

## Dependencies

```json deps
{"deps": [471], "soft_deps": [474, 476], "note": "Makes rsb Just Work. Soft-deps on P0474/P0476 only for the end-to-end smoke — xtask changes are independent."}
```

**Depends on:** P0471 (`0163cc3a`, already merged) — JWT threading makes `jwt.enabled=true` meaningful for substitution.

**Conflicts with:** None — xtask is isolated from the rio-* crates. Recent xtask commits (`f729b5d7`, `9f79b677`, `42a15c87`) touch different files (eks ssm, kind provision, tofu steps) — no overlap.
