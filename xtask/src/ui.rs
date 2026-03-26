//! Span-native progress + verbosity.
//!
//! Every unit of work is a `tracing::Span`. tracing-indicatif renders
//! spans as nested progress bars; child spans auto-indent via
//! `{span_child_prefix}`. `info!` prints above active bars via the
//! layer's suspending writer.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use console::style;
use indicatif::ProgressStyle;
use std::sync::Mutex;

use tracing::level_filters::LevelFilter;
use tracing::{Instrument, Span, info_span};
use tracing_indicatif::IndicatifLayer;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

pub use tracing_indicatif::suspend_tracing_indicatif as suspend;

/// y/N confirmation prompt. Suspends progress bars so the prompt
/// doesn't get clobbered. Returns false on EOF or anything but "y".
pub fn confirm(question: &str) -> bool {
    suspend(|| {
        use std::io::Write;
        eprint!("{} [y/N] ", style(question).bold());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("y")
    })
}

/// Stack of active steps: (name, header_printed). Children printing
/// their ✓ first emit headers for any unprinted ancestors, so nesting
/// reads correctly even though children finish before parents.
static STACK: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

static LEVEL: OnceLock<LevelFilter> = OnceLock::new();

/// `-v` or higher: child process output inherited (streams live).
///
/// clap-verbosity-flag uses WarnLevel as default, so:
///   -q      → Error  (captured, errors only)
///   default → Warn   (captured, xtask bumped to info via filter)
///   -v      → Info   (inherited)
///   -vv     → Debug  (+ argv logging)
///   -vvv    → Trace  (+ wire-level)
pub fn is_verbose() -> bool {
    *LEVEL.get().unwrap_or(&LevelFilter::WARN) >= LevelFilter::INFO
}

/// Initialize tracing + indicatif. Call once from main() with the
/// LevelFilter from clap-verbosity-flag.
pub fn init(level: LevelFilter) {
    LEVEL.set(level).ok();

    // RUST_LOG overrides the flag. At default (Warn), xtask itself
    // still logs at info — the flag controls dep noise, not our
    // own narration.
    //
    // Runtime internals (tokio/mio/hyper's h2) are capped at info
    // even at -vvv. Their trace spans become parents of our debug!
    // events and IndicatifLayer swallows events inside unexpected
    // parent spans — so without this cap, -vvv shows LESS than -vv.
    const RUNTIME_CAP: &str = "tokio=info,runtime=info,mio=info,h2=info";
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if level <= LevelFilter::WARN {
            EnvFilter::new(format!("{level},xtask=info"))
        } else {
            EnvFilter::new(format!("{level},{RUNTIME_CAP}"))
        }
    });

    let indicatif = IndicatifLayer::new()
        .with_span_child_prefix_indent("  ")
        .with_span_child_prefix_symbol("▸ ")
        .with_progress_style(spinner_style());

    // step()/phase() spans are named "_" and exist only so indicatif
    // can hang a progress bar off them. Hide them from the fmt layer,
    // otherwise every info!() inside a step gets a "_:_:" context
    // prefix. The filter is per-layer: indicatif still sees the spans.
    let hide_ui_spans =
        tracing_subscriber::filter::filter_fn(|meta| !(meta.is_span() && meta.name() == "_"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_target(false)
                .with_writer(indicatif.get_stderr_writer())
                .with_filter(hide_ui_spans),
        )
        .with(indicatif)
        .init();
}

// -- styles -------------------------------------------------------------

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{span_child_prefix}{spinner:.cyan} {wide_msg} {elapsed:.dim}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "])
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{span_child_prefix}{bar:20.cyan/blue} {pos}/{len} {wide_msg} {elapsed:.dim}",
    )
    .unwrap()
}

// -- step/phase ---------------------------------------------------------

struct DepthGuard;
impl DepthGuard {
    fn new(name: &str) -> Self {
        STACK.lock().unwrap().push((name.to_string(), false));
        Self
    }
}
impl Drop for DepthGuard {
    fn drop(&mut self) {
        STACK.lock().unwrap().pop();
    }
}

/// Run `f` inside a progress span. Shows a spinner while running,
/// `✓ name` on Ok, `✗ name: err` on Err. Nested step() calls indent.
pub async fn step<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let span = info_span!("_", indicatif.pb_show = tracing::field::Empty);
    span.pb_set_style(&spinner_style());
    span.pb_set_message(name);
    let name = name.to_string();
    async move {
        let _d = DepthGuard::new(&name);
        let r = f().await;
        finish(&name, &r);
        r
    }
    .instrument(span)
    .await
}

/// Owned-name variant for spawned tasks (JoinSet, tokio::spawn) where
/// the future outlives the &str.
pub async fn step_owned<T>(name: String, fut: impl Future<Output = Result<T>>) -> Result<T> {
    let span = info_span!("_", indicatif.pb_show = tracing::field::Empty);
    span.pb_set_style(&spinner_style());
    span.pb_set_message(&name);
    async move {
        let _d = DepthGuard::new(&name);
        let r = fut.await;
        finish(&name, &r);
        r
    }
    .instrument(span)
    .await
}

/// Run `f` inside a known-count progress span (bar instead of spinner).
/// Call `inc()` inside `f` to advance.
pub async fn phase<F, Fut, T>(name: &str, len: u64, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let span = info_span!("_", indicatif.pb_show = tracing::field::Empty);
    span.pb_set_style(&bar_style());
    span.pb_set_message(name);
    span.pb_set_length(len);
    let name = name.to_string();
    async move {
        let _d = DepthGuard::new(&name);
        let r = f().await;
        finish(&name, &r);
        r
    }
    .instrument(span)
    .await
}

/// Advance the current phase's bar by 1.
pub fn inc() {
    Span::current().pb_inc(1);
}

/// Update the current span's message (for last-line tail, poll progress).
pub fn set_message(msg: &str) {
    Span::current().pb_set_message(msg);
}

/// Print a ✓/✗ line. First emits `▸ name` headers for any unprinted
/// ancestors so nesting reads correctly (children finish before
/// parents, so without headers the indentation groups children under
/// the PREVIOUS step visually).
///
/// Not via pb_set_finish_message — that freezes bars in LIFO stack
/// order. indicatif_eprintln! writes via MultiProgress::println which
/// inserts above active bars without a clear/redraw cycle.
fn finish<T>(name: &str, r: &Result<T>) {
    if *LEVEL.get().unwrap_or(&LevelFilter::WARN) < LevelFilter::WARN {
        return; // -q: silent
    }
    let mut stack = STACK.lock().unwrap();
    // Emit headers for ancestors that haven't printed yet (everything
    // except the current step, which is at the top of the stack).
    let depth = stack.len().saturating_sub(1);
    for (i, (ancestor, printed)) in stack.iter_mut().take(depth).enumerate() {
        if !*printed {
            let indent = "  ".repeat(i);
            tracing_indicatif::indicatif_eprintln!("{indent}{} {ancestor}", style("▸").blue());
            *printed = true;
        }
    }
    drop(stack);

    let indent = "  ".repeat(depth);
    match r {
        Ok(_) => {
            tracing_indicatif::indicatif_eprintln!("{indent}{} {name}", style("✓").green())
        }
        Err(e) => {
            tracing_indicatif::indicatif_eprintln!("{indent}{} {name}: {e}", style("✗").red())
        }
    }
}

// -- poll ---------------------------------------------------------------

/// Poll `f` every `interval` up to `max` times inside a step span.
/// `f` returns `Ok(Some(T))` on success, `Ok(None)` to keep polling.
pub async fn poll<T, F, Fut>(name: &str, interval: Duration, max: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    step(name, || async move {
        for i in 1..=max {
            if let Some(v) = f().await? {
                return Ok(v);
            }
            set_message(&format!("{name} ({i}/{max})"));
            tokio::time::sleep(interval).await;
        }
        bail!("timed out after {max} attempts")
    })
    .await
}
