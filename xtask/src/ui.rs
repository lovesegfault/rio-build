//! Progress bars + tracing integration.
//!
//! `tracing::info!()` prints above active progress bars via
//! tracing-indicatif's IndicatifLayer, so logs and spinners don't
//! clobber each other.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

static MULTI: OnceLock<MultiProgress> = OnceLock::new();

pub fn init_tracing() {
    let indicatif = IndicatifLayer::new();
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("xtask=info,warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_target(false)
                .with_writer(indicatif.get_stderr_writer()),
        )
        .with(indicatif)
        .init();
}

pub fn multi() -> &'static MultiProgress {
    MULTI.get_or_init(MultiProgress::new)
}

/// Spinner for open-ended waits. Finishes with a ✓/✗ on
/// `finish_ok`/`finish_err`.
pub fn spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = multi().add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
    );
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

pub fn finish_ok(pb: &ProgressBar, msg: impl Into<String>) {
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(msg.into());
}

pub fn finish_err(pb: &ProgressBar, msg: impl Into<String>) {
    pb.set_style(ProgressStyle::with_template("{prefix:.red} {msg}").unwrap());
    pb.set_prefix("✗");
    pb.finish_with_message(msg.into());
}

/// Known-count progress bar.
pub fn bar(len: u64, msg: impl Into<String>) -> ProgressBar {
    let pb = multi().add(ProgressBar::new(len));
    pb.set_style(ProgressStyle::with_template("{bar:30.cyan/blue} {pos}/{len} {msg}").unwrap());
    pb.set_message(msg.into());
    pb
}

/// Poll `f` every `interval` up to `max` times, with a spinner.
/// Replaces the bash `retry N SLEEP cmd` pattern.
///
/// `f` returns `Ok(Some(T))` on success, `Ok(None)` to keep polling,
/// `Err` to abort immediately.
pub async fn poll_with_spinner<T, F, Fut>(
    msg: &str,
    interval: Duration,
    max: u32,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let pb = spinner(msg.to_string());
    for i in 1..=max {
        match f().await {
            Ok(Some(v)) => {
                finish_ok(&pb, msg.to_string());
                return Ok(v);
            }
            Ok(None) => {
                pb.set_message(format!("{msg} ({i}/{max})"));
                tokio::time::sleep(interval).await;
            }
            Err(e) => {
                finish_err(&pb, format!("{msg}: {e}"));
                return Err(e);
            }
        }
    }
    finish_err(&pb, format!("{msg}: timed out after {max} attempts"));
    bail!("{msg}: timed out after {max} attempts")
}
