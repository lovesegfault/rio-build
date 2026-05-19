//! Tracing init + interactive prompt helpers.
//!
//! `step`/`poll` are thin `tracing::info_span!` wrappers — kept so the
//! ~70 callsites across `k8s/` don't churn. They emit one explicit
//! `✓ name  N.Ns` / `✗ name: err` line on completion (instead of
//! `FmtSpan::CLOSE`, whose `time.busy=… time.idle=…` fields are
//! tracing internals an operator never reads). `step_debug`/`poll_debug`
//! are the *mechanism* tier — port-forwards, SSH banners, nix copies —
//! that repeat dozens of times per QA run and only matter under `-v`.
//!
//! The previous custom span→spinner Layer (✓/✗ tree, bottom-line
//! spinner, indent-aware formatter) was cosmetic; stock `fmt::compact`
//! plus an explicit completion line is enough for a dev tool, and
//! works the same in a TTY and a `tee`'d log.

use std::fmt::Display;
use std::future::Future;
use std::io::{IsTerminal, Write as _};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Select, Text};
use tracing::level_filters::LevelFilter;
use tracing::{Instrument, Span, debug, debug_span, info_span};
use tracing_subscriber::EnvFilter;

static LEVEL: OnceLock<LevelFilter> = OnceLock::new();

/// Test override for [`is_verbose`]. `LEVEL` is a write-once
/// [`OnceLock`] set from CLI flags at [`init`]; tests never call
/// `init` and can't unset it once set, so the verbose `sh::run*`
/// branches are otherwise unreachable from a unit test. A separate
/// flag keeps the production state machine untouched.
#[cfg(test)]
static VERBOSE_TEST_OVERRIDE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `-v` or higher: child process output inherited (streams live).
///
/// clap-verbosity-flag uses WarnLevel as default, so:
///   default → Warn   (captured, xtask bumped to info via filter)
///   -v      → Info   (inherited)
///   -vv     → Debug  (+ argv logging)
///   -vvv    → Trace
pub fn is_verbose() -> bool {
    #[cfg(test)]
    if VERBOSE_TEST_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    *LEVEL.get().unwrap_or(&LevelFilter::WARN) >= LevelFilter::INFO
}

/// Force [`is_verbose`] to return `true` for the current process so
/// tests can exercise the verbose `sh::run*` paths. Setting `false`
/// clears the override (falls back to `LEVEL`, which tests never
/// initialize, i.e. non-verbose). Process-global: fine under nextest
/// (process-per-test); under `cargo test`, only call this from tests
/// that restore `false` before returning.
#[cfg(test)]
pub fn set_verbose_for_test(v: bool) {
    VERBOSE_TEST_OVERRIDE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// EnvFilter directive string for `level`. xtask is FLOORED at info
/// (not pinned): target directives win by specificity, so a literal
/// `xtask=info` would override the global `debug`/`trace` and `-vv`
/// would never show argv logging from `sh.rs`. Runtime internals stay
/// capped at info (their trace floods).
fn build_filter_directive(level: LevelFilter) -> String {
    let xtask_lvl = level.max(LevelFilter::INFO);
    format!("{level},xtask={xtask_lvl},tokio=info,runtime=info,mio=info,h2=info")
}

/// Initialize tracing. Call once from main(). Stock compact fmt to
/// stderr, env-filter (`RUST_LOG` overrides the flag). `step()`
/// boundaries are explicit `✓ name  N.Ns` lines emitted from
/// [`step_owned`] — not `FmtSpan::CLOSE` events, which carry
/// `time.busy`/`time.idle` fields and a flattened span path that an
/// operator never reads. xtask itself stays at info even at the Warn
/// default; runtime internals capped at info even at -vvv (their
/// trace floods).
pub fn init(level: LevelFilter) {
    LEVEL.set(level).ok();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(build_filter_directive(level)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();
}

/// `eprintln!` to stderr. Kept so the `clippy::print_stderr` allow
/// lives in one place instead of at every error-dump callsite.
#[allow(clippy::print_stderr)]
pub fn eprint(args: std::fmt::Arguments<'_>) {
    let _ = std::io::stderr().write_fmt(args);
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

/// Run `f` inside an `info_span!(step = name)`. Emits `✓ name  N.Ns`
/// at INFO on success and `✗ name: err` at ERROR on failure so a `?`
/// deep in `k8s up` still shows which step failed.
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
    let span = info_span!("step", %name);
    async move {
        let start = Instant::now();
        let r = fut.instrument(span).await;
        // Emit outside ANY span: the line already names the step, and
        // a `step{name=…}: ` parent prefix is redundant noise. The
        // surrounding lines (stage banner / parent step verdict) carry
        // the hierarchy.
        Span::none().in_scope(|| match &r {
            Ok(_) => tracing::info!("✓ {name:36} {:>6.1}s", start.elapsed().as_secs_f64()),
            Err(e) => tracing::error!("✗ {name}: {e:#}"),
        });
        r
    }
}

/// Like [`step`] but at DEBUG: silent at the default verbosity, shows
/// under `-v`. For *mechanism* steps — port-forwards, SSH banners,
/// `nix copy`/`nix build` legs — that repeat dozens of times per run
/// and whose verdict is owned by the caller (a scenario, the smoke
/// test, an `up` phase). Errors are also `debug!`: the layer that owns
/// the verdict surfaces failures; logging `error!` from inside the
/// mechanism would steal that verdict (and false-positives on
/// scenarios that *expect* a build to fail, e.g. i209).
pub async fn step_debug<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    step_debug_owned(name.to_string(), f()).await
}

/// Owned-name variant of [`step_debug`].
pub fn step_debug_owned<T>(
    name: String,
    fut: impl Future<Output = Result<T>>,
) -> impl Future<Output = Result<T>> {
    let span = debug_span!("step", %name);
    async move {
        let start = Instant::now();
        let r = fut.instrument(span).await;
        Span::none().in_scope(|| match &r {
            Ok(_) => debug!("✓ {name:36} {:>6.1}s", start.elapsed().as_secs_f64()),
            Err(e) => debug!("✗ {name}: {e:#}"),
        });
        r
    }
}

/// Log a skipped step (e.g. `tofu apply` when plan shows no diff).
pub fn step_skip(name: &str, reason: &str) {
    tracing::info!("⊘ {name:36} skipped — {reason}");
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

/// [`poll`] at DEBUG — see [`step_debug`] for when to use which.
pub async fn poll_debug<T, F, Fut>(name: &str, interval: Duration, max: u32, f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    step_debug_owned(name.to_string(), poll_in(interval, max, f)).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xtask_directive_floors_at_info() {
        // Default verbosity (Warn) bumps xtask to info so step() spans show.
        assert!(build_filter_directive(LevelFilter::WARN).contains("xtask=info"));
    }

    #[test]
    fn xtask_directive_scales_with_vv() {
        // -vv → Debug: xtask must follow (argv logging in sh.rs is debug!).
        // A literal `xtask=info` would pin and `-vv` would never show argv.
        let d = build_filter_directive(LevelFilter::DEBUG);
        assert!(d.contains("xtask=debug"), "{d}");
        assert!(!d.contains("xtask=info"), "{d}");
        // -vvv → Trace.
        assert!(build_filter_directive(LevelFilter::TRACE).contains("xtask=trace"));
    }
}
