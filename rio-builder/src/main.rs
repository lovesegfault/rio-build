//! rio-builder binary entry point.
//!
//! Thin wrapper: bootstrap (config + telemetry + signal), then hand off
//! to [`rio_builder::runtime::setup`] / [`rio_builder::runtime::run`].

use clap::Parser;

use rio_builder::config::{CliArgs, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        serve_shutdown: _,
        otel_guard: _otel_guard,
        root_span: _root_span,
    } = rio_common::server::bootstrap("builder", cli, rio_builder::describe_metrics)?;

    match rio_builder::runtime::setup(cfg, shutdown).await? {
        Some(rt) => rio_builder::runtime::run(rt).await,
        // Shutdown fired during cold-start connect. Clean exit —
        // nothing to drain (never connected), no FUSE mounted.
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the worker.
    // -----------------------------------------------------------------------

    /// Both required fields filled with placeholders. Rejection
    /// tests patch ONE field to prove that specific check fires.
    fn test_valid_config() -> Config {
        let mut cfg = Config::default();
        cfg.scheduler.addr = "http://localhost:9000".into();
        cfg.store.addr = "http://localhost:9001".into();
        cfg
    }

    #[test]
    fn config_rejects_empty_addrs() {
        type Patch = fn(&mut Config);
        let cases: &[(&str, Patch)] = &[
            ("scheduler.addr", |c| c.scheduler.addr = String::new()),
            ("store.addr", |c| c.store.addr = String::new()),
        ];
        for (field, patch) in cases {
            let mut cfg = test_valid_config();
            patch(&mut cfg);
            let err = cfg
                .validate()
                .expect_err("cleared required field must be rejected")
                .to_string();
            assert!(
                err.contains(field),
                "error for cleared {field} must name it: {err}"
            );
        }
    }

    /// Baseline: `test_valid_config()` itself passes — proves
    /// rejection tests test ONLY their mutation.
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
    }
}
