//! Helpers both providers (k3s/eks) use.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rand::{Rng, RngExt, distr::Alphanumeric};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::client as kube;
use crate::k8s::provider::BuiltImages;
use crate::sh::{self, cmd, repo_root, shell};
use crate::{git, ui};

/// `NIX_SSHOPTS` for every nix invocation that talks to the gateway
/// over an SSM tunnel (rsb/cpt, stress, smoke). Consolidated after
/// I-161 — the I-149 ServerAlive fix landed in `with_remote_store`
/// only; stress/smoke kept the bare `StrictHostKeyChecking` literal
/// and re-broke at the same idle window during cold-eval.
///
/// - `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null`:
///   gateway host key is ephemeral (regenerated per pod).
/// - `ServerAliveInterval=30` + `ServerAliveCountMax=6` (I-149/I-161):
///   the SSM port-forward path doesn't reliably round-trip
///   server-originated SSH keepalives when there's zero client→server
///   data (idle during eval, idle mid-build). Client-side keepalive
///   forces a write every 30s, which flushes the websocket in both
///   directions and resets the gateway's russh `alive_timeouts`.
/// - `ControlMaster=no` + `ControlPath=none`: a user-level
///   `ControlMaster auto` (common in `~/.ssh/config`) would mux all
///   nix-spawned ssh processes through one master. A killed nix run
///   leaves a stale master that subsequent xtask runs hang on.
///   Tunnels are per-run; multiplexing buys nothing.
/// - `IdentityAgent=none` + `IdentitiesOnly=yes` (I-161 root cause): a
///   forwarded ssh-agent that is dead/unresponsive (e.g. an Eternal
///   Terminal forwarded `SSH_AUTH_SOCK` whose remote end disconnected)
///   makes ssh hang indefinitely on the agent unix socket BEFORE
///   sending KEXINIT — the gateway sees TCP-accept then 120s of
///   silence then keepalive timeout. `IdentitiesOnly=yes` alone is
///   insufficient: ssh still queries the agent for the `-i` key (to
///   check if the agent holds the decrypted private half).
///   `IdentityAgent=none` disables agent communication entirely. This
///   was the actual I-161 mechanism; ServerAlive + keepalive_max are
///   defense-in-depth for the genuine SSM-idle case.
pub const NIX_SSHOPTS_BASE: &str = "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ServerAliveInterval=30 -o ServerAliveCountMax=6 \
     -o ControlMaster=no -o ControlPath=none \
     -o IdentityAgent=none -o IdentitiesOnly=yes";

/// I-161: warm the eval cache so a subsequent ssh-ng build's
/// connection doesn't sit idle during cold `--impure` eval. nix opens
/// the connection on first remote query, then evaluates locally; over
/// SSM port-forward, server-originated keepalive replies don't
/// reliably round-trip when there's zero client→server data, so the
/// gateway drops the session at 120s while nix is still evaluating.
/// Pre-evaluating shrinks the connect→submit window to <5s.
///
/// `envs` is set on the nix process — fsbench threads `FSBENCH_SEED`
/// through both this pre-eval and the build itself so both instantiate
/// the same drv (a mismatch would cold-eval on the build's connection,
/// re-opening the I-161 window).
///
/// Returns the instantiated .drv path on success: fsbench matches it
/// against `DebugExecutorState.running_build` for build→node
/// attribution. Failure is non-fatal (warn + `None`) — the build can
/// still succeed on a cold eval, it just risks the idle-window drop.
pub fn pre_eval_installable(installable: &str, envs: &[(&str, &str)]) -> Option<String> {
    tracing::info!("pre-evaluating {installable} (cold-eval can take ~2min)");
    let mut cmd = std::process::Command::new("nix");
    cmd.args(["path-info", "--derivation", "--impure", installable]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let pre_eval = match cmd.output() {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("pre-eval spawn failed (continuing): {e}");
            return None;
        }
    };
    if !pre_eval.status.success() {
        tracing::warn!(
            "pre-eval failed (continuing): {}",
            std::str::from_utf8(&pre_eval.stderr)
                .unwrap_or("<non-utf8 stderr>")
                .trim()
        );
        return None;
    }
    std::str::from_utf8(&pre_eval.stdout)
        .ok()?
        .lines()
        .find(|l| l.trim_end().ends_with(".drv"))
        .map(|l| l.trim().to_owned())
}

/// One remote ssh-ng build riding its own SSM tunnel. Both halves are
/// drop-guarded: the tunnel is a [`ProcessGuard`] (killpg on drop), the
/// nix child is `kill_on_drop` — holding this struct is what keeps the
/// build alive.
pub struct RemoteBuild {
    /// Local port the tunnel actually bound (differs from the request
    /// when 0/ephemeral was passed).
    pub port: u16,
    pub tunnel: ProcessGuard,
    pub child: tokio::process::Child,
}

/// Establish a gateway tunnel and spawn `nix build --store ssh-ng://…`
/// for `installable`, logging to `log_path`. Extracted from the stress
/// harness so fsbench submits through the identical path (I-149/I-161
/// SSH options, eval-store auto, --max-jobs 0). `port_req` 0 = the
/// tunnel binds an ephemeral port; a fixed port gets its stale
/// listeners reaped first.
pub async fn spawn_remote_nix_build(
    p: &dyn super::provider::Provider,
    port_req: u16,
    cfg: &XtaskConfig,
    installable: &str,
    log_path: &std::path::Path,
    envs: &[(&str, &str)],
) -> Result<RemoteBuild> {
    use std::process::Stdio;

    let key = crate::ssh::privkey_path(cfg)?;
    if port_req != 0 {
        kill_port_listeners(port_req);
    }
    let (port, tunnel) = p.tunnel(port_req).await?;

    let log_file = std::fs::File::create(log_path)?;
    let log_err = log_file.try_clone()?;
    let store = format!("ssh-ng://rio@localhost:{port}?ssh-key={}", key.display());

    let mut cmd = tokio::process::Command::new("nix");
    cmd.args(["build", "--store", &store, "--eval-store", "auto"])
        .arg(installable)
        // -L: stream build logs to stderr. Without it, redirected
        // stderr stays empty until the first `copying path` line —
        // ~2.5min of silence on cold-cache eval (I-051).
        .args(["--impure", "--no-link", "-L", "--max-jobs", "0"])
        // I-149/I-161: see [`NIX_SSHOPTS_BASE`].
        .env("NIX_SSHOPTS", NIX_SSHOPTS_BASE)
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn nix build (installable: {installable})"))?;
    tracing::info!(
        "build[{port}]: pid={} → {}",
        child.id().unwrap_or(0),
        log_path.display()
    );
    Ok(RemoteBuild {
        port,
        tunnel,
        child,
    })
}

/// Subcharts listed in Chart.yaml's `dependencies:`. Helm validates
/// charts/ against Chart.yaml BEFORE evaluating `condition: *.enabled`,
/// so every entry must be symlinked even when disabled for a given
/// provider (eks uses Aurora+S3, k3s uses Rook).
const SUBCHARTS: &[&str] = &["postgresql"];

/// Symlink all subcharts from their nix-store derivations into
/// `infra/helm/rio-build/charts/`. Gitignored.
pub async fn chart_deps() -> Result<()> {
    let charts = repo_root().join("infra/helm/rio-build/charts");
    // Reap before repopulate. Helm renders every entry in charts/ as a
    // subchart regardless of Chart.yaml — a stale symlink left by a
    // removed dependency renders with that chart's own defaults (no
    // `condition:` gate once the Chart.yaml entry is gone).
    if let Ok(entries) = std::fs::read_dir(&charts) {
        for entry in entries {
            let _ = std::fs::remove_file(entry?.path());
        }
    }
    std::fs::create_dir_all(&charts)?;
    for name in SUBCHARTS {
        let attr = format!(".#helm.{name}");
        // I-198: was sync `sh::read` — `nix build` is multi-second on a
        // cold cache; this runs inside the deploy phase (a spawned tokio
        // task). `run_read` yields. Shell scoped per call so `&Shell`
        // (`!Sync`) isn't held across the await.
        let path = {
            let sh = shell()?;
            sh::run_read(cmd!(sh, "nix build --no-link --print-out-paths {attr}"))
        }
        .await?;
        let link = charts.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(path.trim(), &link)?;
    }
    Ok(())
}

/// Build the dockerImages linkFarm for the host arch only.
/// Used by k3s (`ctr import`) — runs on the local machine so only
/// needs the host arch.
pub async fn build_host_arch(_cfg: &XtaskConfig) -> Result<BuiltImages> {
    let repo = git::open()?;
    let tag = git::image_tag(&repo)?;

    let sys = match std::env::consts::ARCH {
        "x86_64" => "x86_64-linux",
        "aarch64" => "aarch64-linux",
        other => bail!("unsupported host arch: {other}"),
    };

    let dir = tempfile::tempdir()?;
    let link = dir.path().join("images");
    let link_s = link.to_str().unwrap();
    let attr = format!(".#packages.{sys}.dockerImages");

    // Shell scoped so `&Shell` (`!Sync`) drops before the await — keeps
    // this future `Send` for the per-phase `tokio::spawn` (I-198).
    let build = {
        let sh = shell()?;
        sh::run(cmd!(sh, "nix build {attr} -L --out-link {link_s}"))
    };
    ui::step(&format!("nix build {attr}"), || build).await?;
    Ok(BuiltImages { dir, tag })
}

/// Namespaces that get a `rio-postgres` URL secret. `ensure_pg_secrets`
/// creates one in each; `K3s::destroy` deletes from each. Shared so
/// create/destroy can't drift (ADR-019 added NS_STORE to create only).
pub const PG_SECRET_NAMESPACES: [&str; 2] = [NS, super::NS_STORE];

/// Create the two postgres Secrets for the in-cluster bitnami path
/// (k3s). Generates a random password on first deploy; reuses
/// the existing one on upgrades so the DB doesn't get locked out.
///
/// - `rio-postgres-auth` key `password` — raw password, what bitnami
///   reads via `auth.existingSecret`
/// - `rio-postgres` key `url` — full connection URL, what store/
///   scheduler read via `RIO_DATABASE_URL` secretKeyRef
///
/// Keeps the password out of helm values (so `helm get values`
/// doesn't leak it) and out of git. EKS uses ESO for the same
/// contract; VM tests keep the hardcoded `rio` password via
/// `postgres-secret.yaml` (airgapped, xtask doesn't run there).
/// Write the `rio-gateway-ssh` Secret. When `tenant` is `None` and
/// `RIO_SSH_TENANT` is unset, preserves the existing Secret's comment
/// instead of clobbering to `default` (I-100: a bare `xtask deploy`
/// after smoke wrote `smoke-test` would otherwise silently re-route
/// the user's key to a different tenant). Falls through to
/// [`crate::ssh::DEFAULT_TENANT`] only on first deploy (Secret absent);
/// EKS deploy creates that tenant in the scheduler so `rsb` works
/// out of the box.
pub async fn ensure_gateway_ssh_secret(
    client: &kube::Client,
    cfg: &XtaskConfig,
    tenant: Option<&str>,
) -> Result<()> {
    let tenant = match tenant.or(cfg.ssh_tenant.as_deref()) {
        explicit @ Some(_) => explicit.map(str::to_owned),
        None => {
            let key = crate::ssh::read_pubkey(cfg)?;
            kube::get_secret_key(client, NS, "rio-gateway-ssh", "authorized_keys")
                .await?
                .as_deref()
                .and_then(|ak| crate::ssh::tenant_for_key(ak, &key))
        }
    };
    let authorized = crate::ssh::authorized_keys(cfg, tenant.as_deref())?;
    // MERGE, don't replace: a redeploy must not wipe keys added via
    // `xtask k8s grant`. The operator's key is upserted (same
    // fingerprint → comment updated; new fingerprint → appended).
    merge_authorized_key(client, &authorized).await
}

/// Upsert one `authorized_keys` line into the `rio-gateway-ssh`
/// Secret. Dedups on (key-type, base64) — comment may change (re-
/// granting the same key under a different tenant replaces its line).
/// All other existing lines are preserved IN ORDER.
pub async fn merge_authorized_key(client: &kube::Client, key_line: &str) -> Result<()> {
    merge_authorized_keys_batch(client, &[key_line]).await
}

/// Batch [`merge_authorized_key`]: ONE Secret read + ONE write for N
/// keys. The qa tenant pool installs ~8 keys at once; the single-key
/// path would be 8 round-trips with last-write-wins races.
pub async fn merge_authorized_keys_batch(client: &kube::Client, lines: &[&str]) -> Result<()> {
    let existing = kube::get_secret_key(client, NS, "rio-gateway-ssh", "authorized_keys")
        .await?
        .unwrap_or_default();
    let merged = lines
        .iter()
        .fold(existing, |acc, l| merge_authorized_key_lines(&acc, l));
    kube::apply_secret(
        client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), merged)]),
    )
    .await
}

/// Remove every `authorized_keys` line whose comment (third field)
/// starts with `prefix`. ONE Secret read + ONE write. qa cleanup uses
/// this with `qa-{nonce}-` so only this run's keys are stripped.
pub async fn remove_authorized_keys_by_comment_prefix(
    client: &kube::Client,
    prefix: &str,
) -> Result<()> {
    let existing = kube::get_secret_key(client, NS, "rio-gateway-ssh", "authorized_keys")
        .await?
        .unwrap_or_default();
    let kept: String = existing
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .nth(2)
                .is_none_or(|c| !c.starts_with(prefix))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    kube::apply_secret(
        client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), kept)]),
    )
    .await
}

/// Pure half of [`merge_authorized_key`]. Map-in-place: walk
/// `existing`, replace the line whose (type, base64) matches `key_line`
/// (comment may differ), append if no match. Preserves order — the
/// previous filter-then-append moved the upserted key to the end,
/// which combined with positional tenant lookup (now fixed) silently
/// re-tagged the operator's key after `grant`.
fn merge_authorized_key_lines(existing: &str, key_line: &str) -> String {
    let key_id = |l: &str| {
        let mut it = l.split_whitespace();
        Some((it.next()?.to_owned(), it.next()?.to_owned()))
    };
    let new_id = key_id(key_line);
    let key_line = key_line.trim_end_matches('\n');
    let mut replaced = false;
    let mut merged = String::new();
    for l in existing.lines().filter(|l| !l.trim().is_empty()) {
        if key_id(l) == new_id {
            merged.push_str(key_line);
            replaced = true;
        } else {
            merged.push_str(l);
        }
        merged.push('\n');
    }
    if !replaced {
        merged.push_str(key_line);
        merged.push('\n');
    }
    merged
}

/// Grant build access to one SSH public key under its own tenant.
///
/// 1. Validate the pubkey, force its comment to `tenant` (the gateway
///    maps comment → `SubmitBuild.tenant_name`).
/// 2. `rio-cli create-tenant <tenant>` via the scheduler tunnel
///    (idempotent — `AlreadyExists` is fine).
/// 3. Merge the key into the `rio-gateway-ssh` Secret, deduping by
///    (type, base64) so re-runs don't grow the file.
/// 4. Optionally rollout-restart the gateway. The gateway hot-reloads
///    `authorized_keys` (I-109, `r[gw.keys.hot-reload]`) within ~70s
///    of the Secret changing, so restart is only for "need it NOW".
///
/// Admin access is NOT granted: the key only authenticates the ssh-ng
/// build path. `AdminService` lives on ClusterIP:9001 behind the
/// `scheduler-ingress` CiliumNetworkPolicy and needs `kubectl
/// port-forward` (i.e. k8s credentials), which `grant` does not hand
/// out.
pub async fn grant(pubkey: &str, tenant: &str, restart: bool) -> Result<()> {
    use super::eks::smoke::{CliCtx, step_restart_gateway, step_tenant, step_upstream};

    // Inline key first (`ssh-ed25519 AAAA...`); if that doesn't parse,
    // treat the value as a file path. A path string never parses as a
    // key (no algorithm token), so the fallback only fires for non-key
    // input.
    let mut key = match ssh_key::PublicKey::from_openssh(pubkey) {
        Ok(k) => k,
        Err(_) => ssh_key::PublicKey::read_openssh_file(std::path::Path::new(pubkey))
            .with_context(|| {
                format!(
                    "'{pubkey}' is neither an inline OpenSSH public key nor a readable .pub file"
                )
            })?,
    };
    key.set_comment(tenant);
    let key_line = key.to_openssh()? + "\n";

    let client = kube::client().await?;

    ui::step("create tenant", || async {
        let cli = CliCtx::open(&client, 0, 0).await?;
        step_tenant(&cli, tenant).await?;
        // Upstreams are per-tenant. Without this, a fresh tenant
        // substitutes from nothing — every build compiles stdenv from
        // source. Idempotent (checks `upstream list` first).
        step_upstream(&cli, tenant).await
    })
    .await?;

    ui::step("merge key into rio-gateway-ssh", || {
        merge_authorized_key(&client, &key_line)
    })
    .await?;

    if restart {
        ui::step("rollout restart rio-gateway", || {
            step_restart_gateway(&client)
        })
        .await?;
    } else {
        ui::step_skip(
            "rollout restart rio-gateway",
            "gateway hot-reloads authorized_keys within ~70s (I-109); pass --restart for immediate effect",
        );
    }

    let host = kube::gateway_lb_hostname(&client, NS)
        .await
        .unwrap_or_else(|_| "<gateway-lb-hostname>".into());
    let when = if restart { "now" } else { "in ~70s" };
    tracing::info!(
        "granted: tenant '{tenant}' ({}) — active {when}\n  \
         nix build --store 'ssh-ng://rio@{host}?ssh-key=<their-private-key>' ...",
        key.fingerprint(Default::default()),
    );
    Ok(())
}

pub async fn ensure_pg_secrets(client: &kube::Client) -> Result<()> {
    let pass = match kube::get_secret_key(client, NS, "rio-postgres-auth", "password").await? {
        Some(p) => p,
        None => rand::rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect(),
    };
    kube::apply_secret(
        client,
        NS,
        "rio-postgres-auth",
        BTreeMap::from([("password".into(), pass.clone())]),
    )
    .await?;
    // Service name: bitnami's default is <release>-postgresql, release
    // is "rio". Subchart renders in the helm release namespace (rio-
    // system) — spelled out so the store's copy resolves across the ns
    // boundary. Password is alphanumeric-only so no urlencoding needed.
    let url = format!("postgres://rio:{pass}@rio-postgresql.{NS}:5432/rio");
    // ADR-019: store moved to rio-store. Secrets are ns-scoped, so the
    // store's Deployment can't read rio-system/rio-postgres. Duplicate
    // the url secret; scheduler reads the rio-system copy, store reads
    // the rio-store copy. Same connection string (postgresql Service
    // lives in rio-system either way).
    for ns in PG_SECRET_NAMESPACES {
        kube::apply_secret(
            client,
            ns,
            "rio-postgres",
            BTreeMap::from([("url".into(), url.clone())]),
        )
        .await?;
    }
    Ok(())
}

/// JWT keypair as base64'd helm `--set` values. Both 32 raw bytes → b64
/// string. `seed` goes in `jwt.signingSeed` (helm's `b64enc` double-
/// wraps for Secret.data; gateway decodes both layers — see
/// `jwt-signing-secret.yaml`). `pubkey` goes in `jwt.publicKey` →
/// ConfigMap data (single b64; verify-side decodes once).
pub struct JwtKeypair {
    pub seed: String,
    pub pubkey: String,
}

// r[impl gw.jwt.issue]
/// Generate or reuse the JWT ed25519 keypair. Idempotent: first deploy
/// generates a fresh 32-byte seed; subsequent deploys read it back from
/// the helm-rendered `rio-jwt-signing` Secret so tokens stay valid
/// across `xtask k8s deploy` reruns. Rotation = `kubectl delete secret
/// rio-jwt-signing` then redeploy.
///
/// Returns the b64 seed + derived b64 pubkey for passing to helm:
/// `--set jwt.enabled=true --set jwt.signingSeed=<seed> --set jwt.publicKey=<pubkey>`.
/// Both go through helm values (visible in `helm get values`) — fine for
/// dev clusters; the seed is ephemeral, the cluster is local. Contrast
/// with `ensure_pg_secrets` which avoids helm values because that DB
/// password can unlock persisted data.
pub async fn ensure_jwt_keypair(client: &kube::Client) -> Result<JwtKeypair> {
    let seed = match kube::get_secret_key(client, NS, "rio-jwt-signing", "ed25519_seed").await? {
        // helm-rendered Secret stores our b64 seed (k8s decodes the
        // outer Secret.data b64; what we read back is the operator's
        // b64 string — same one we passed in via --set).
        Some(s) => s,
        None => {
            let mut raw = [0u8; 32];
            rand::rng().fill_bytes(&mut raw);
            B64.encode(raw)
        }
    };
    // Derive pubkey from seed. `SigningKey::from_bytes` takes the raw
    // 32-byte seed; `verifying_key().to_bytes()` gives the 32-byte pub.
    // Both match what gateway/scheduler expect per `_helpers.tpl`.
    let sk = decode_jwt_seed(&seed)?;
    let pubkey = B64.encode(sk.verifying_key().to_bytes());
    Ok(JwtKeypair { seed, pubkey })
}

/// Decode the b64 `ed25519_seed` value from the `rio-jwt-signing`
/// Secret into a signing key.
fn decode_jwt_seed(seed_b64: &str) -> Result<ed25519_dalek::SigningKey> {
    let raw: [u8; 32] = B64
        .decode(seed_b64)
        .context("rio-jwt-signing secret ed25519_seed is not valid base64")?
        .try_into()
        .map_err(|v: Vec<u8>| {
            anyhow::anyhow!(
                "rio-jwt-signing ed25519_seed decodes to {} bytes, expected 32",
                v.len()
            )
        })?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&raw))
}

/// Read the gateway's JWT signing key from the live `rio-jwt-signing`
/// Secret. Unlike [`ensure_jwt_keypair`], never generates one — a
/// token minted under a key the cluster doesn't verify with would just
/// fail every RPC with `UNAUTHENTICATED`, so a missing Secret is the
/// caller's problem to surface, not paper over.
pub async fn jwt_signing_key(client: &kube::Client) -> Result<ed25519_dalek::SigningKey> {
    let seed = kube::get_secret_key(client, NS, "rio-jwt-signing", "ed25519_seed")
        .await?
        .context(
            "Secret rio-jwt-signing not found — the cluster has no JWT signing key \
             (deploy with `cargo xtask k8s up --deploy` first)",
        )?;
    decode_jwt_seed(&seed)
}

/// Label selector matching every rio Deployment. Set by the chart's
/// `rio.labels` helper on every rendered resource; also set on
/// namespaces by `kube::ensure_namespace` (NetworkPolicy namespaceSelector
/// rules match by it).
pub const RIO_LABEL_SELECTOR: &str = "app.kubernetes.io/part-of=rio-build";

/// Rollout-restart every rio Deployment across all rio namespaces.
/// Called by k3s after a same-tag `:dev` push: the Deployment spec
/// is unchanged (same image tag), so kube won't re-pull the image on its
/// own. EKS skips this — git-SHA tags change on every push, so helm
/// upgrade already triggers a rollout.
///
/// Restarts are fire-and-forget: no `wait_rollout` here. If the caller
/// wants to block until pods are healthy, `helm --wait` on the upgrade
/// already covers it, or call `wait_rollout` per deployment after.
pub async fn rollout_restart_rio(client: &kube::Client) -> Result<()> {
    let mut all = Vec::new();
    for &(ns, _) in super::NAMESPACES {
        let names = kube::rollout_restart_all(client, ns, RIO_LABEL_SELECTOR).await?;
        all.extend(names.into_iter().map(|n| format!("{ns}/{n}")));
    }
    tracing::info!(deployments = ?all, "rollout-restarted (same-tag push)");
    Ok(())
}

/// Kill any process listening on `port` (TCP, localhost). Best-effort:
/// `lsof` missing or no listener → no-op. Logs each PID killed.
///
/// Reaps stale `session-manager-plugin` / `kubectl port-forward`
/// children that a SIGKILL'd or panicked xtask left bound. ProcessGuard
/// only fires on clean drop; the stress harness's `setsid nohup` path
/// leaks tunnels by design (I-128 QA sessions). A new tunnel on an
/// occupied port either fails to bind or — worse — the old listener
/// accepts and forwards to a stale target, surfacing as the
/// "unexpected packet type 80" SSH error.
pub fn kill_port_listeners(port: u16) {
    use ::nix::sys::signal::{Signal, kill};
    use ::nix::unistd::Pid;

    // -t: terse (PID only). -i: TCP on this port. -sTCP:LISTEN: only
    // the listener, not clients connected TO it. lsof exits 1 when
    // nothing matches — that's the no-stale-tunnel case, not an error.
    let port_spec = format!("-iTCP:{port}");
    let Ok(out) = std::process::Command::new("lsof")
        .args(["-t", &port_spec, "-sTCP:LISTEN"])
        .output()
    else {
        tracing::debug!("lsof not on PATH; skipping stale-tunnel reap");
        return;
    };
    for line in std::str::from_utf8(&out.stdout).unwrap_or("").lines() {
        let Ok(pid) = line.trim().parse::<i32>() else {
            continue;
        };
        // Best-effort name for the log line. /proc/PID/comm is the
        // 16-char task name (e.g. "session-manager", "kubectl").
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into());
        match kill(Pid::from_raw(pid), Signal::SIGTERM) {
            Ok(()) => tracing::info!("killed stale listener on :{port} — pid={pid} ({comm})"),
            Err(e) => tracing::warn!("kill pid={pid} on :{port} failed: {e}"),
        }
    }
}

/// Guard that kills a child *process group* on drop. Used for
/// port-forward and SSM tunnel processes in smoke tests.
///
/// I-158: `aws ssm start-session` spawns `session-manager-plugin` as a
/// grandchild; SIGTERM on the direct child doesn't reliably propagate
/// (python wrapper), so the plugin orphans (ppid→1) and keeps the
/// port bound — same "type 80" failure as [`kill_port_listeners`].
/// [`spawn`](Self::spawn) puts the child in its own group; Drop kills
/// the whole group.
pub struct ProcessGuard {
    pub child: tokio::process::Child,
    /// pgid == child's pid (process_group(0) makes the child a group
    /// leader). Captured at spawn — `Child::id()` returns `None` after
    /// the child is reaped, so reading it in Drop would be too late.
    pgid: ::nix::unistd::Pid,
}

impl ProcessGuard {
    /// Spawn `cmd` in a fresh process group and wrap it. The ONLY way
    /// to construct a guard — direct construction would let Drop's
    /// killpg target whatever group the child inherited (xtask's own).
    pub fn spawn(mut cmd: tokio::process::Command) -> std::io::Result<Self> {
        // Linux-only: process_group(0) → setpgid(0,0) post-fork pre-exec.
        // Child's pgid becomes its own pid; descendants inherit it.
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pgid = ::nix::unistd::Pid::from_raw(
            child
                .id()
                .expect("just spawned; not yet waited")
                .try_into()
                .expect("pid_t fits i32"),
        );
        Ok(Self { child, pgid })
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        use ::nix::sys::signal::{Signal, killpg};
        let _ = killpg(self.pgid, Signal::SIGTERM);
    }
}

/// Drop-guard over a supervisor task that owns a [`ProcessGuard`] and
/// (optionally) respawns it when the child exits. sh-011: a `kubectl
/// port-forward` death mid-build (observed iter3 T+~1800s) drops the
/// ssh-ng connection; the [`GatewayEndpoint::Tunnel`] fallback wraps
/// its port-forward in a respawn loop so a transient apiserver-proxy
/// hiccup doesn't kill an hour-long `nix build --store ssh-ng://…`.
///
/// Drop aborts the supervisor task at its next await point; the
/// task's local `ProcessGuard` then drops → `killpg`. NOT synchronous
/// (same best-effort contract as [`ProcessGuard`] itself), but
/// `with_remote_store` returning is the only caller and a few-ms tail
/// is harmless there.
///
/// [`GatewayEndpoint::Tunnel`]: crate::k8s::provider::GatewayEndpoint::Tunnel
pub struct SupervisedTunnel(tokio::task::JoinHandle<()>);

impl SupervisedTunnel {
    /// Hold `guard` until dropped — no respawn. Used by the default
    /// `Provider::gateway_endpoint` impl (k3s, the `phases.rs` mock):
    /// same semantics as holding the bare `ProcessGuard` today.
    pub fn hold(guard: ProcessGuard) -> Self {
        Self(tokio::spawn(async move {
            // Park forever; on abort the future drops, `_g` drops,
            // killpg fires.
            let _g = guard;
            std::future::pending::<()>().await;
        }))
    }

    /// Own `first` and, when it exits, `kill_port_listeners(port)` +
    /// `respawn(port)` up to `max` times. The respawn closure rebinds
    /// the same `port` (the first bind may have been ephemeral; the
    /// concrete port is what we re-use) so the caller's
    /// `ssh-ng://rio@localhost:{port}` URL stays valid across a
    /// kubectl death.
    pub fn supervise<F, Fut>(mut guard: ProcessGuard, port: u16, max: u32, respawn: F) -> Self
    where
        F: Fn(u16) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(u16, ProcessGuard)>> + Send,
    {
        Self(tokio::spawn(async move {
            for n in 1..=max {
                // `wait()` is cancel-safe; on abort `guard` drops here.
                let status = guard.child.wait().await;
                tracing::warn!(
                    ?status,
                    "gateway port-forward died; reaping :{port} and respawning ({n}/{max})"
                );
                kill_port_listeners(port);
                match respawn(port).await {
                    Ok((_, g)) => guard = g,
                    Err(e) => {
                        tracing::error!("gateway port-forward respawn {n} failed: {e:#}");
                        return;
                    }
                }
            }
            tracing::error!("gateway port-forward exceeded {max} respawns; holding last child");
            let _ = guard.child.wait().await;
        }))
    }
}

impl Drop for SupervisedTunnel {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn `kubectl port-forward <target> <local>:<remote>` in `ns` and
/// return `(bound_local_port, drop-guard)`. `target` is the full
/// kubectl resource ref (`svc/rio-gateway`, `pod/rio-scheduler-abc`).
/// Long-lived gateway tunnels use [`super::ssm`] instead.
///
/// Pass `local = 0` for an ephemeral port: kubectl binds `:0`, the OS
/// picks a free port, and we parse it from the `Forwarding from
/// 127.0.0.1:NNNNN -> REMOTE` stdout line. I-101: fixed local ports
/// made concurrent `xtask k8s cli` invocations race on bind — second
/// one's kubectl failed `address already in use`, surfacing later as a
/// bare `transport error` from rio-cli.
///
/// Fixed ports (`local != 0`) parse the same bind line, so a failed
/// bind (port already taken) errors here instead of leaving the
/// caller's readiness probe talking to whatever foreign listener owns
/// the port.
pub async fn port_forward(
    ns: &str,
    target: &str,
    local: u16,
    remote: u16,
) -> Result<(u16, ProcessGuard)> {
    let mut cmd = tokio::process::Command::new("kubectl");
    cmd.args(["-n", ns, "port-forward", target])
        .arg(format!("{local}:{remote}"))
        .stderr(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    let mut guard = ProcessGuard::spawn(cmd)?;
    let stdout = guard.child.stdout.take().expect("piped above");
    let mut lines = BufReader::new(stdout).lines();
    // kubectl can hang indefinitely without producing a bind line
    // (apiserver auth stall, target pod gone, exec creds wedged) — and
    // `next_line()` blocks until then. A bind-or-bail deadline keeps a
    // wedged kubectl from silently stalling the caller forever. This
    // was the only leg of `gateway_build` with no deadline at all
    // (SSH-banner is `ui::poll`-bounded, nix-* eventually time out on
    // their own); a 2026-05-19 full-QA run lost an i048c warmup build
    // somewhere in that chain with no evidence of which leg stalled.
    let bind_loop = async {
        loop {
            let Some(line) = lines.next_line().await? else {
                anyhow::bail!("kubectl port-forward {target} exited before binding");
            };
            // First line: `Forwarding from 127.0.0.1:NNNNN -> REMOTE`.
            // (Second is `[::1]:NNNNN`; per-conn `Handling connection` follows.)
            if let Some(rest) = line.strip_prefix("Forwarding from 127.0.0.1:") {
                break rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .with_context(|| format!("unparseable port-forward line: {line}"));
            }
        }
    };
    let bound = tokio::time::timeout(Duration::from_secs(30), bind_loop)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "kubectl port-forward {target} did not bind within 30s — \
                 apiserver slow, exec credential wedged, or target unschedulable"
            )
        })??;
    // Drain the rest so kubectl never blocks on a full pipe.
    tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });
    Ok((bound, guard))
}

/// Fetch a Secret's data key from the rio-system namespace as raw
/// bytes (the HMAC key is `openssl rand 32`, NOT UTF-8). `Ok(None)`
/// only if the Secret itself is absent (404); a Secret that exists
/// but lacks `key` hard-errors so a malformed Secret surfaces as
/// "missing data key X" instead of a downstream `PermissionDenied`.
pub async fn secret_bytes(name: &str, key: &str) -> Result<Option<Vec<u8>>> {
    use ::kube::Api;
    use k8s_openapi::api::core::v1::Secret;
    let client = kube::client().await?;
    let api: Api<Secret> = Api::namespaced(client, NS);
    let Some(secret) = api.get_opt(name).await? else {
        return Ok(None);
    };
    let bytes = secret
        .data
        .and_then(|d| d.get(key).map(|v| v.0.clone()))
        .with_context(|| format!("Secret {name} present but missing data key {key:?}"))?;
    Ok(Some(bytes))
}

/// Write bytes to an anonymous memfd and return the held-open File.
/// memfd not tempfile: secrets never hit disk. NO `MFD_CLOEXEC` — the
/// rio-cli child must inherit the fd to open `/dev/fd/N`.
pub fn bytes_to_memfd(b: &[u8]) -> Result<std::fs::File> {
    use ::nix::sys::memfd::{MFdFlags, memfd_create};
    use std::io::Write;
    let fd = memfd_create(c"rio-secret", MFdFlags::empty())?;
    let mut f = std::fs::File::from(fd);
    f.write_all(b)?;
    Ok(f)
}

#[cfg(test)]
#[test]
fn bytes_to_memfd_round_trips_via_dev_fd() {
    // Recurrence guard for bug_022: with_cli_tunnel hands rio-cli a
    // /dev/fd/N path to this memfd; assert the path reads back the
    // bytes AND that FD_CLOEXEC is unset (so the fd survives exec()
    // into rio-cli). The /dev/fd/N read alone is same-process —
    // FD_CLOEXEC only bites across exec(), so without the F_GETFD
    // assert this would still pass with `MFdFlags::MFD_CLOEXEC`.
    use ::nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use std::os::fd::{AsFd, AsRawFd};
    let f = bytes_to_memfd(b"k").unwrap();
    let flags = FdFlag::from_bits_retain(fcntl(f.as_fd(), FcntlArg::F_GETFD).unwrap());
    assert!(
        !flags.contains(FdFlag::FD_CLOEXEC),
        "memfd has FD_CLOEXEC; rio-cli child won't inherit it"
    );
    let path = format!("/dev/fd/{}", f.as_raw_fd());
    assert_eq!(std::fs::read(&path).unwrap(), b"k");
}

/// Poll for a live scheduler-leader pod. helm --wait returns when the
/// NEW pods are Ready, but the Lease may still name the OLD pod
/// (release is on the shutdown path); tunnelling to a Terminating pod
/// passes the TCP-accept probe then dies mid-RPC. `scheduler_leader`
/// rejects non-live holders; this polls until the new leader has
/// acquired. Bails immediately when no scheduler pods exist
/// (post-`--wipe`, pre-deploy) so callers' Err arm engages instead of
/// 30×2s of dead polling.
pub async fn wait_scheduler_leader(client: &kube::Client) -> Result<String> {
    ui::poll_debug("scheduler lease holder", Duration::from_secs(2), 30, || {
        let c = client.clone();
        async move {
            if kube::count_pods(&c, NS, "app.kubernetes.io/name=rio-scheduler").await? == 0 {
                bail!("no rio-scheduler pods exist; skipping leader wait");
            }
            match kube::scheduler_leader(&c, NS).await {
                Ok(h) => Ok(Some(h)),
                Err(e) => {
                    tracing::info!("{e:#}");
                    Ok(None)
                }
            }
        }
    })
    .await
}

/// Tunnel scheduler:9001 + store:9002, wait for TCP accept on both.
/// ADR-019: scheduler is in rio-system, store in rio-store. Scheduler
/// targets the leader pod (standbys reject admin writes). EKS uses SSM
/// per-pod-node ([`super::ssm::tunnel_pod`]); k3s falls back to
/// kubectl. Returns the BOUND ports (differ from inputs when
/// `0`/ephemeral was passed).
pub async fn tunnel_grpc(
    sched_port: u16,
    store_port: u16,
) -> Result<((u16, ProcessGuard), (u16, ProcessGuard))> {
    let client = kube::client().await?;
    let leader = wait_scheduler_leader(&client).await?;
    let (sched, store) = if super::ssm::relay().await.is_some() {
        let store_pod =
            kube::one_running_pod(&client, super::NS_STORE, "app.kubernetes.io/name=rio-store")
                .await?;
        (
            super::ssm::tunnel_pod(&client, NS, &leader, 9001, sched_port).await?,
            super::ssm::tunnel_pod(&client, super::NS_STORE, &store_pod, 9002, store_port).await?,
        )
    } else {
        (
            port_forward(NS, &format!("pod/{leader}"), sched_port, 9001).await?,
            port_forward(super::NS_STORE, "svc/rio-store", store_port, 9002).await?,
        )
    };
    let (sp, tp) = (sched.0, store.0);
    ui::poll_debug(
        "scheduler+store TCP accept",
        Duration::from_secs(2),
        10,
        || async {
            // gRPC has no greeting — bare connect is the only signal.
            let s = tokio::net::TcpStream::connect(("127.0.0.1", sp)).await;
            let t = tokio::net::TcpStream::connect(("127.0.0.1", tp)).await;
            Ok((s.is_ok() && t.is_ok()).then_some(()))
        },
    )
    .await?;
    Ok((sched, store))
}

/// Side-task that prints "helm waiting: rio-scheduler 0/2, …" every
/// 15s while a `helm upgrade --wait` runs. helm's own `--wait` is
/// silent until done; on a post-`--wipe` cold start that's 3-4min of
/// nothing, which reads as a hang. Polls Deployments in `rio-system` +
/// `rio-store` (the two namespaces with chart Deployments) and lists
/// the not-yet-Ready ones. Aborted by the caller when helm exits.
pub fn spawn_helm_wait_progress(client: &kube::Client) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        // First tick fires immediately; skip it so the line only shows
        // when helm has actually been waiting a beat.
        tick.tick().await;
        loop {
            tick.tick().await;
            let mut pending = Vec::new();
            for ns in [NS, super::NS_STORE] {
                for d in kube::list_deployment_status(&client, ns)
                    .await
                    .unwrap_or_default()
                {
                    if !d.ok {
                        pending.push(format!("{} {}/{}", d.name, d.ready, d.want));
                    }
                }
            }
            if pending.is_empty() {
                tracing::info!(
                    "helm waiting: all Deployments Ready (hooks/Jobs may still be running)"
                );
            } else {
                tracing::info!("helm waiting: {}", pending.join(", "));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify gw.jwt.issue]
    #[test]
    fn jwt_keypair_roundtrip() {
        // Fresh seed → b64 → decode → derive pubkey. Mirrors
        // ensure_jwt_keypair's derivation path without a kube client.
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let seed_b64 = B64.encode(raw);

        let decoded: [u8; 32] = B64.decode(&seed_b64).unwrap().try_into().unwrap();
        assert_eq!(decoded, raw);

        let sk = ed25519_dalek::SigningKey::from_bytes(&decoded);
        let pubkey_b64 = B64.encode(sk.verifying_key().to_bytes());
        // Pubkey decodes to 32 bytes (VerifyingKey::from_bytes contract).
        assert_eq!(B64.decode(&pubkey_b64).unwrap().len(), 32);

        // Deterministic: same seed → same pubkey.
        let sk2 = ed25519_dalek::SigningKey::from_bytes(&raw);
        assert_eq!(
            pubkey_b64,
            B64.encode(sk2.verifying_key().to_bytes()),
            "pubkey derivation must be deterministic for idempotent redeploys"
        );
    }

    #[test]
    fn jwt_seed_b64_length() {
        // 32 raw bytes → 44-char b64 string (no padding surprises).
        // Matches what operators pass via `openssl rand -base64 32`.
        let seed = B64.encode([0u8; 32]);
        assert_eq!(seed.len(), 44);
    }

    #[test]
    fn decode_jwt_seed_validates_length() {
        // A truncated/corrupt Secret value must error with the byte
        // count, not panic or silently produce a wrong key.
        let err = decode_jwt_seed(&B64.encode([0u8; 16])).unwrap_err();
        assert!(err.to_string().contains("16 bytes"), "{err}");
        // Round-trip: same seed bytes → same pubkey as direct from_bytes.
        let raw = [7u8; 32];
        let sk = decode_jwt_seed(&B64.encode(raw)).unwrap();
        assert_eq!(
            sk.verifying_key(),
            ed25519_dalek::SigningKey::from_bytes(&raw).verifying_key()
        );
    }

    /// I-158 regression: ProcessGuard must kill GRANDCHILDREN. The
    /// `aws ssm start-session` → `session-manager-plugin` shape: we
    /// model it as `bash -c '... & wait'` → `sleep`. Pre-fix (single-
    /// pid kill) the sleep orphaned; post-fix (killpg) it dies with
    /// the group.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn process_guard_kills_grandchildren() {
        use ::nix::sys::signal::kill;
        use ::nix::unistd::Pid;
        use std::process::Stdio;
        use tokio::io::AsyncBufReadExt;

        // Shell prints the grandchild's pid then waits — same parent-
        // stays-alive-until-child-exits shape as the aws wrapper.
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut guard = ProcessGuard::spawn(cmd).expect("spawn bash");
        let stdout = guard.child.stdout.take().expect("piped");
        let line = tokio::io::BufReader::new(stdout)
            .lines()
            .next_line()
            .await
            .expect("read line")
            .expect("got pid line");
        let grandchild = Pid::from_raw(line.trim().parse::<i32>().expect("parse pid"));

        // Signal-0 probe: alive before drop.
        assert!(
            kill(grandchild, None).is_ok(),
            "grandchild should be alive pre-drop"
        );

        drop(guard);

        // SIGTERM delivery + reap isn't synchronous with killpg(). Poll
        // for ESRCH; same shape as stress.rs::kill_graceful's reap-wait.
        for _ in 0..20 {
            if kill(grandchild, None).is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("grandchild {grandchild} still alive 2s after ProcessGuard drop");
    }

    #[test]
    fn merge_authorized_key_preserves_order() {
        let existing = "ssh-ed25519 AAAA ops\nssh-ed25519 BBBB alice\n";
        // Upsert ops with a new comment → replaces in place, NOT appended.
        let merged = merge_authorized_key_lines(existing, "ssh-ed25519 AAAA newops\n");
        assert_eq!(merged, "ssh-ed25519 AAAA newops\nssh-ed25519 BBBB alice\n");
        // New key → appended.
        let merged = merge_authorized_key_lines(existing, "ssh-ed25519 CCCC bob");
        assert_eq!(
            merged,
            "ssh-ed25519 AAAA ops\nssh-ed25519 BBBB alice\nssh-ed25519 CCCC bob\n"
        );
        // Empty existing → just the new line.
        assert_eq!(
            merge_authorized_key_lines("", "ssh-ed25519 AAAA ops"),
            "ssh-ed25519 AAAA ops\n"
        );
    }

    /// sh-011: `with_remote_store` returning MUST tear the tunnel
    /// down. `SupervisedTunnel` holds the guard on a spawned task;
    /// Drop aborts it → the task's local `ProcessGuard` drops →
    /// killpg. RED at base: `SupervisedTunnel` does not exist.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn supervised_tunnel_drop_kills_held_child() {
        use ::nix::sys::signal::kill;
        use ::nix::unistd::Pid;
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60");
        let guard = ProcessGuard::spawn(cmd).expect("spawn sleep");
        let pid = Pid::from_raw(i32::try_from(guard.child.id().unwrap()).unwrap());
        let st = SupervisedTunnel::hold(guard);
        assert!(kill(pid, None).is_ok(), "child alive pre-drop");
        drop(st);
        for _ in 0..40 {
            if kill(pid, None).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("held child still alive 2s after SupervisedTunnel drop");
    }

    #[test]
    fn pg_secret_namespaces_covers_store() {
        // ADR-019 deploy/destroy symmetry: ensure_pg_secrets writes
        // into NS_STORE; K3s::destroy iterates the same const.
        assert!(PG_SECRET_NAMESPACES.contains(&crate::k8s::NS_STORE));
        assert!(PG_SECRET_NAMESPACES.contains(&NS));
    }

    /// I-161 regression guard: `NIX_SSHOPTS_BASE` carries the I-149
    /// ServerAlive options, ControlMaster suppression, IdentitiesOnly,
    /// and IdentityAgent=none. The last is the actual I-161 root cause
    /// (dead forwarded agent → ssh hangs pre-KEXINIT); IdentitiesOnly
    /// alone is insufficient — see the const's doc.
    #[test]
    fn nix_sshopts_have_keepalive_and_no_mux() {
        for needle in [
            "ServerAliveInterval=30",
            "ServerAliveCountMax=6",
            "ControlMaster=no",
            "ControlPath=none",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
        ] {
            assert!(
                NIX_SSHOPTS_BASE.contains(needle),
                "NIX_SSHOPTS_BASE missing {needle:?}"
            );
        }
    }
}
