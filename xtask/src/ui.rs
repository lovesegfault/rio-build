//! Tracing init + interactive prompt helpers.
//!
//! `step`/`poll` are thin `tracing::info_span!` wrappers — kept so the
//! ~70 callsites across `k8s/` don't churn. The previous custom
//! span→spinner Layer (✓/✗ tree, bottom-line spinner, indent-aware
//! formatter) was cosmetic; stock `fmt::compact` with span context is
//! enough for a dev tool.

use std::fmt::Display;
use std::future::Future;
use std::io::{IsTerminal, Write as _};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Select, Text};
use tracing::level_filters::LevelFilter;
use tracing::{Instrument, debug, info, info_span};
use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

static LEVEL: OnceLock<LevelFilter> = OnceLock::new();

/// `-v` or higher: child process output inherited (streams live).
///
/// clap-verbosity-flag uses WarnLevel as default, so:
///   default → Warn   (captured, xtask bumped to info via filter)
///   -v      → Info   (inherited)
///   -vv     → Debug  (+ argv logging)
///   -vvv    → Trace
pub fn is_verbose() -> bool {
    *LEVEL.get().unwrap_or(&LevelFilter::WARN) >= LevelFilter::INFO
}

/// Initialize tracing. Call once from main(). Stock compact fmt to
/// stderr, env-filter (`RUST_LOG` overrides the flag), span-close
/// events so `step()` boundaries show up. xtask itself stays at info
/// even at the Warn default; runtime internals capped at info even at
/// -vvv (their trace floods).
pub fn init(level: LevelFilter) {
    LEVEL.set(level).ok();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "{level},xtask=info,tokio=info,runtime=info,mio=info,h2=info"
        ))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_span_events(FmtSpan::CLOSE)
        .compact()
        .init();
}

/// No-op passthrough. Kept for callsite compat — previously cleared
/// the spinner line for raw stdout writes (sh.rs, status.rs, prompts).
pub fn suspend<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// `eprintln!` to stderr. Kept for callsite compat (sh.rs error
/// dumps, push.rs failures).
#[allow(clippy::print_stderr)]
pub fn eprint(args: std::fmt::Arguments<'_>) {
    let _ = std::io::stderr().write_fmt(args);
}

/// Last-line tail / poll progress. Was the spinner message; now a
/// debug event so `-vv` shows it. Callsites: sh.rs `tail()`,
/// regen/sqlx.rs, tofu.rs.
pub fn set_message(msg: &str) {
    debug!("{msg}");
}

// -- inquire prompt helpers ---------------------------------------------

/// Force the terminal out of application cursor key mode (DECCKM) so
/// arrows send CSI (`ESC [ A`) instead of SS3 (`ESC O A`). inquire's
/// console backend can't parse SS3 — arrows would print A/B/C/D as
/// filter input instead of navigating. zsh's ZLE commonly leaves
/// DECCKM on when running external commands.
///
/// TODO: remove once console-rs/console#283 lands (adds SS3 parsing).
fn prompt<T>(f: impl FnOnce() -> T) -> T {
    // rmkx: ESC [ ? 1 l (DECCKM off) + ESC > (keypad numeric mode)
    let _ = std::io::stderr().write_all(b"\x1b[?1l\x1b>");
    f()
}

/// Treat ESC/Ctrl-C as "no" rather than bubbling an InquireError.
fn cancel_is_no(r: Result<bool, InquireError>) -> Result<bool> {
    match r {
        Ok(b) => Ok(b),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// y/N confirm. Gates on TTY (scripts can't accidentally confirm).
/// "held" naming is callsite compat — caller previously held suspend()
/// across a show-then-prompt sequence.
pub fn confirm_held(msg: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    cancel_is_no(prompt(|| Confirm::new(msg).with_default(false).prompt()))
}

/// Destructive confirm: default N. Returns false on non-TTY stdin —
/// destroying infra requires an interactive terminal.
pub fn confirm_destroy(msg: &str) -> Result<bool> {
    confirm_held(msg)
}

/// Select from a list. None if stdin isn't a TTY — caller falls back
/// to a CLI-arg error.
pub fn select<T: Display>(msg: &str, opts: Vec<T>) -> Result<Option<T>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    match prompt(|| Select::new(msg, opts).prompt()) {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("cancelled")
        }
        Err(e) => Err(e.into()),
    }
}

/// Text input with a validator. None if not a TTY.
pub fn text<V>(msg: &str, validator: V) -> Result<Option<String>>
where
    V: Fn(&str) -> Result<(), String> + Clone + Send + Sync + 'static,
{
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    match prompt(|| {
        Text::new(msg)
            .with_validator(move |s: &str| match validator(s) {
                Ok(()) => Ok(Validation::Valid),
                Err(e) => Ok(Validation::Invalid(e.into())),
            })
            .prompt()
    }) {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("cancelled")
        }
        Err(e) => Err(e.into()),
    }
}

// -- step / poll --------------------------------------------------------

/// Run `f` inside an `info_span!(step = name)`. Logs the error chain
/// on Err so a `?` deep in `k8s up` still shows which step failed.
pub async fn step<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    step_owned(name.to_string(), f()).await
}

/// Owned-name variant for spawned tasks (JoinSet, tokio::spawn).
///
/// NOT an `async fn` — the span is created synchronously at the call
/// site (while the parent span is current), not on first poll of the
/// spawned task (where there's no span context). Returns `impl Future`
/// so the span is captured before spawn.
pub fn step_owned<T>(
    name: String,
    fut: impl Future<Output = Result<T>>,
) -> impl Future<Output = Result<T>> {
    let span = info_span!("step", name);
    async move {
        let r = fut.await;
        if let Err(e) = &r {
            tracing::error!("{e:#}");
        }
        r
    }
    .instrument(span)
}

/// Log a skipped step (e.g. `tofu apply` when plan shows no diff).
pub fn step_skip(name: &str, reason: &str) {
    info!(step = name, reason, "skipped");
}

/// Poll `f` every `interval` up to `max` times inside a step span.
/// `f` returns `Ok(Some(T))` on success, `Ok(None)` to keep polling.
pub async fn poll<T, F, Fut>(name: &str, interval: Duration, max: u32, f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    step_owned(name.to_string(), poll_in(interval, max, f)).await
}

/// Poll inside the CURRENT span (caller already wrapped in `ui::step`).
pub async fn poll_in<T, F, Fut>(interval: Duration, max: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    for i in 1..=max {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        debug!("attempt {i}/{max}");
        tokio::time::sleep(interval).await;
    }
    bail!("timed out after {max} attempts")
}
