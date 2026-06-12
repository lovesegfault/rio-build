//! `rio build` client configuration.
//!
//! Two-struct split per `rio_common::config` module docs:
//!   - [`Config`]: merged result, all fields concrete, `Default` =
//!     compiled-in defaults.
//!   - [`ConfigOverlay`]: clap-parsed flags shared by the `build`
//!     subcommand, all fields `Option`, no `env=` (the `RIO_` env
//!     layer handles that), no `default_value`.
//!
//! Component name is `build` — TOML at `/etc/rio/build.toml` /
//! `./build.toml`, env prefix `RIO_` (e.g. `RIO_SCHEDULER_ADDR`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Scheduler gRPC endpoint as `host:port` (no scheme —
    /// `SubmitBuild`/`WatchBuild`/`CancelBuild`). Required. Env:
    /// `RIO_SCHEDULER_ADDR`.
    pub scheduler_addr: String,
    /// Store castore-door gRPC endpoint as `host:port` (`Has*`
    /// negotiation, drv blob puts, chunked source upload, output
    /// fetch). Required. Env: `RIO_STORE_ADDR`.
    pub store_addr: String,
    /// File containing the tenant JWT attached as `x-rio-tenant-token`
    /// on every RPC. Unset = no token (single-tenant/dev clusters
    /// only; a production door rejects anonymous callers).
    pub tenant_token_path: Option<PathBuf>,
    /// Client CAS root (pack store, fingerprint index, cluster-ack
    /// table, fetched outputs). Unset = `$XDG_CACHE_HOME/rio/evalstore`
    /// (falling back to `~/.cache/rio/evalstore`).
    pub cas_root: Option<PathBuf>,
    /// Path to the eval-parent binary (C++ libexpr embedding — ADR-024
    /// P3b). `rio build` spawns it with the worker channel on fd 3.
    /// Required for `rio build <installable>` until P3b ships a
    /// default; `--attach`/`--cancel` work without it.
    pub eval_parent: Option<PathBuf>,
    /// Cluster-ack record TTL in seconds. MUST be ≤ the cluster's
    /// minimum unpinned-blob lifetime (ADR-024) — a longer TTL turns
    /// every cluster GC into a stale-ack recovery cycle. Default 6h,
    /// matching the store's in-flight grace window.
    pub ack_ttl_secs: u64,
    /// Nodes per `SubmitBuild` page. Submissions above this paginate
    /// with a shared `submission_id` (the skeleton is ~334B/node; the
    /// 16MB budget is exceeded around ~50k raw nodes).
    pub page_max_nodes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scheduler_addr: String::new(),
            store_addr: String::new(),
            tenant_token_path: None,
            cas_root: None,
            eval_parent: None,
            ack_ttl_secs: 6 * 3600,
            page_max_nodes: 50_000,
        }
    }
}

impl Config {
    /// Resolved CAS root (explicit, or the XDG cache location).
    pub fn cas_root(&self) -> PathBuf {
        if let Some(p) = &self.cas_root {
            return p.clone();
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("rio").join("evalstore")
    }

    /// Ack-table scope: cluster endpoints + tenant identity
    /// (`r[bc.negotiate.ack-short-circuit]`). The tenant component is
    /// the JWT `sub` claim when the token parses as one — decoded
    /// WITHOUT signature verification, which is fine here because the
    /// scope only partitions a local cache file, never authorizes
    /// anything — so a renewed/rotated token for the same tenant keeps
    /// its acks instead of forcing a full re-negotiation. An opaque
    /// token falls back to a fingerprint of its bytes.
    pub fn ack_scope(&self, token: Option<&str>) -> String {
        let tenant = match token {
            None => "anon".to_string(),
            Some(t) => jwt_sub_unverified(t)
                .map(|sub| format!("tenant:{sub}"))
                .unwrap_or_else(|| {
                    let fp = blake3::hash(t.as_bytes()).to_hex();
                    format!("tok:{}", &fp.as_str()[..16])
                }),
        };
        format!("{}|{}|{}", self.scheduler_addr, self.store_addr, tenant)
    }

    /// Read the tenant token (trimmed), if configured.
    pub fn tenant_token(&self) -> anyhow::Result<Option<String>> {
        match &self.tenant_token_path {
            None => Ok(None),
            Some(p) => {
                let raw = std::fs::read_to_string(p).map_err(|e| {
                    anyhow::anyhow!("reading tenant_token_path {}: {e}", p.display())
                })?;
                Ok(Some(raw.trim().to_string()))
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        rio_common::config::ensure_required(&self.scheduler_addr, "scheduler_addr", "build")?;
        rio_common::config::ensure_required(&self.store_addr, "store_addr", "build")?;
        Ok(())
    }
}

/// Extract the `sub` claim from a JWS-shaped token without verifying
/// the signature (header.payload.signature, base64url payload). Used
/// ONLY to partition the local ack cache by tenant — never as an
/// authorization input (the server verifies the token on every RPC).
fn jwt_sub_unverified(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    Some(v.get("sub")?.as_str()?.to_string())
}

/// CLI overlay flags shared by the `rio build` subcommand. `None`
/// fields are skipped at serialization so they don't overwrite lower
/// config layers.
#[derive(Debug, Default, clap::Args, Serialize)]
pub struct ConfigOverlay {
    /// Scheduler gRPC endpoint.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler_addr: Option<String>,
    /// Store castore-door gRPC endpoint.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_addr: Option<String>,
    /// File containing the tenant JWT.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_token_path: Option<PathBuf>,
    /// Client CAS root directory.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cas_root: Option<PathBuf>,
    /// Eval-parent binary to spawn for evaluation.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_parent: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The jail macros resolve `CliArgs` at the call site; this
    /// crate's CLI overlay struct plays that role.
    type CliArgs = ConfigOverlay;

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn overlay_parses() {
        use clap::CommandFactory;
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            overlay: ConfigOverlay,
        }
        T::command().debug_assert();
    }

    /// A minimal unsigned JWS shape: `{}` header, the given payload,
    /// dummy signature. Only the payload is read by `ack_scope`.
    fn fake_jwt(payload: serde_json::Value) -> String {
        use base64::Engine as _;
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.{}",
            enc(b"{}"),
            enc(payload.to_string().as_bytes()),
            enc(b"sig")
        )
    }

    #[test]
    fn ack_scope_distinguishes_cluster_and_tenant() {
        let mut c = Config {
            scheduler_addr: "http://s:1".into(),
            store_addr: "http://st:2".into(),
            ..Config::default()
        };
        // Opaque (non-JWT) tokens scope by token fingerprint.
        let a = c.ack_scope(Some("token-a"));
        let b = c.ack_scope(Some("token-b"));
        assert_ne!(a, b, "different tenants must not share acks");
        c.store_addr = "http://other:2".into();
        assert_ne!(
            a,
            c.ack_scope(Some("token-a")),
            "different clusters must not share acks"
        );
    }

    /// The scope keys on TENANT identity, not token bytes: a rotated
    /// JWT for the same `sub` keeps its acks; a different `sub` never
    /// shares them (`r[bc.negotiate.ack-short-circuit]`).
    #[test]
    fn ack_scope_survives_token_rotation_for_same_tenant() {
        let c = Config {
            scheduler_addr: "http://s:1".into(),
            store_addr: "http://st:2".into(),
            ..Config::default()
        };
        let t1 = fake_jwt(serde_json::json!({"sub": "tenant-x", "exp": 1}));
        let t2 = fake_jwt(serde_json::json!({"sub": "tenant-x", "exp": 2}));
        assert_eq!(
            c.ack_scope(Some(&t1)),
            c.ack_scope(Some(&t2)),
            "rotation must not invalidate the ack table"
        );
        let other = fake_jwt(serde_json::json!({"sub": "tenant-y", "exp": 1}));
        assert_ne!(c.ack_scope(Some(&t1)), c.ack_scope(Some(&other)));
    }

    // Jailed standing-guard tests — see rio-test-support/src/config.rs.
    rio_test_support::jail_roundtrip!(
        "build",
        r#"
        scheduler_addr = "http://sched:50051"
        store_addr = "http://store:50052"
        ack_ttl_secs = 1234
        page_max_nodes = 777
        "#,
        |cfg: Config| {
            assert_eq!(cfg.scheduler_addr, "http://sched:50051");
            assert_eq!(cfg.store_addr, "http://store:50052");
            assert_eq!(cfg.ack_ttl_secs, 1234);
            assert_eq!(cfg.page_max_nodes, 777);
            assert!(cfg.tenant_token_path.is_none());
        }
    );

    rio_test_support::jail_defaults!("build", "ack_ttl_secs = 21600", |cfg: Config| {
        assert_eq!(cfg.ack_ttl_secs, 6 * 3600);
        assert_eq!(cfg.page_max_nodes, 50_000);
        assert!(cfg.scheduler_addr.is_empty());
        assert!(cfg.eval_parent.is_none());
    });
}
