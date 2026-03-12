use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use rio_push::nix;
use rio_push::upload;

// Two-struct config split — see rio-common/src/config.rs for rationale.

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// rio-store gRPC address (host:port).
    store_addr: String,
    /// Maximum concurrent uploads.
    concurrency: usize,
    /// OIDC token for authentication. Typically set via env var
    /// `RIO_OIDC_TOKEN` (e.g., from GitHub Actions' OIDC provider).
    oidc_token: Option<String>,
    /// mTLS configuration for connecting to rio-store.
    tls: rio_common::tls::TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store_addr: "localhost:9002".into(),
            concurrency: 8,
            oidc_token: None,
            tls: rio_common::tls::TlsConfig::default(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(name = "rio-push", about = "Push Nix store path closures to rio-store")]
struct CliArgs {
    /// Nix store paths to push (with their full closures).
    #[arg(required = true)]
    #[serde(skip)]
    store_paths: Vec<String>,

    /// rio-store gRPC address (host:port).
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// Maximum concurrent uploads.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<usize>,

    /// OIDC token for authentication.
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    oidc_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Err = a provider is already installed (benign in tests / multi-init).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let store_paths = cli.store_paths.clone();

    let cfg: Config = rio_common::config::load("push", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("push")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        store_addr = %cfg.store_addr,
        concurrency = cfg.concurrency,
        paths = store_paths.len(),
        "starting rio-push"
    );

    // Set up client TLS (if configured) and connect to the store.
    let client_tls = rio_common::tls::load_client_tls(&cfg.tls)
        .map_err(|e| anyhow::anyhow!("client TLS config: {e}"))?;
    rio_proto::client::init_client_tls(client_tls);

    let mut client = rio_proto::client::connect_store(&cfg.store_addr).await?;
    info!(store_addr = %cfg.store_addr, "connected to rio-store");

    // Discover closure.
    info!("discovering closure of {} store paths", store_paths.len());
    let closure = nix::discover_closure(&store_paths).await?;
    let closure_nar_bytes: u64 = closure.iter().map(|p| p.nar_size).sum();
    info!(
        paths = closure.len(),
        nar_bytes = closure_nar_bytes,
        "closure discovered"
    );

    if closure.is_empty() {
        info!("nothing to push");
        return Ok(());
    }

    // Find which paths the store is missing.
    let all_paths: Vec<String> = closure.iter().map(|p| p.path.clone()).collect();
    let missing = upload::find_missing(&mut client, all_paths).await?;
    let total = closure.len();
    let skipped = total - missing.len();

    // Build a lookup for the NixPathInfo by store path.
    let closure_map: std::collections::HashMap<&str, &nix::NixPathInfo> =
        closure.iter().map(|p| (p.path.as_str(), p)).collect();

    if missing.is_empty() {
        info!(total, "all paths already present in store");
        return Ok(());
    }

    let upload_nar_bytes: u64 = missing
        .iter()
        .filter_map(|p| closure_map.get(p.as_str()))
        .map(|npi| npi.nar_size)
        .sum();
    info!(
        total,
        missing = missing.len(),
        skipped,
        upload_nar_bytes,
        "uploading missing paths"
    );

    // Upload missing paths concurrently.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(cfg.concurrency));
    let oidc_token: Option<Arc<str>> = cfg.oidc_token.map(|t| Arc::from(t.as_str()));

    let mut handles = Vec::with_capacity(missing.len());
    let mut errors = 0u64;
    for path in &missing {
        let Some(npi) = closure_map.get(path.as_str()) else {
            error!(path, "missing path not found in closure (skipping)");
            errors += 1;
            continue;
        };

        let validated = match npi.to_validated() {
            Ok(v) => v,
            Err(e) => {
                error!(path, error = %e, "failed to validate path info");
                errors += 1;
                continue;
            }
        };

        let store_addr = cfg.store_addr.clone();
        let token = oidc_token.clone();
        let permit = semaphore.clone().acquire_owned().await?;
        let path = path.clone();

        handles.push(tokio::spawn(async move {
            let _permit = permit;

            // Each task gets its own client connection for simplicity.
            let mut task_client = match rio_proto::client::connect_store(&store_addr).await {
                Ok(c) => c,
                Err(e) => {
                    error!(path, error = %e, "failed to connect to store");
                    return (path, Err(e));
                }
            };

            // Dump NAR.
            let nar = match nix::dump_nar(&path).await {
                Ok(n) => n,
                Err(e) => {
                    error!(path, error = %e, "failed to dump NAR");
                    return (path, Err(e));
                }
            };

            let nar_len = nar.len() as u64;
            info!(path, nar_size = nar_len, "uploading");

            let result =
                upload::push_path(&mut task_client, validated, nar, token.as_deref()).await;
            (path, result.map(|created| (created, nar_len)))
        }));
    }

    let mut created = 0u64;
    let mut existed = 0u64;
    // errors was initialized before the loop to catch pre-spawn failures.
    let mut uploaded_bytes = 0u64;

    for handle in handles {
        let (path, result) = handle.await?;
        match result {
            Ok((true, nar_len)) => {
                debug!(path, "pushed");
                created += 1;
                uploaded_bytes += nar_len;
            }
            Ok((false, _)) => {
                debug!(path, "already existed");
                existed += 1;
            }
            Err(e) => {
                error!(path, error = %e, "upload failed");
                errors += 1;
            }
        }
    }

    info!(
        created,
        existed, errors, skipped, total, uploaded_bytes, "push complete"
    );

    if errors > 0 {
        anyhow::bail!("{errors} path(s) failed to upload");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.store_addr, "localhost:9002");
        assert_eq!(d.concurrency, 8);
        assert!(d.oidc_token.is_none());
        assert!(!d.tls.is_configured());
    }

    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
