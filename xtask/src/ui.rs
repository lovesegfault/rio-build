//! Span-native progress + verbosity.
//!
//! Every unit of work is a `tracing::Span`. A single bottom-line
//! spinner shows the innermost active step; completed steps print as a
//! depth-indented ✓/✗ tree above it. `info!` indents to match.

use std::cell::Cell;
use std::fmt::Display;
use std::future::Future;
use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use console::style;
use inquire::ui::{Attributes, Color, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Select, Text};

use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing::{Instrument, Span, info_span, span};
use tracing_subscriber::fmt::{FormatEvent, FormatFields, format};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Clear the spinner line for the duration of `f`, then let it resume.
///
/// Under the old multi-progress renderer, raw stdout writes scrolled
/// past its tracked terminal bottom and froze a copy of the bar in
/// scrollback — hence the `clippy::print_{stdout,stderr}` deny in
/// main.rs. With a single `\r…\x1b[K` spinner the failure mode is
/// milder (one half-overwritten line), but the discipline stays:
/// anything that writes to the terminal directly wraps in `suspend`.
pub fn suspend<R>(f: impl FnOnce() -> R) -> R {
    // Signature matches the old re-export so callers (sh.rs, tofu.rs,
    // status.rs, stress.rs, prompt helpers below) compile unchanged.
    let _g = SPINNER.pause();
    f()
}

// -- inquire theme + prompt helpers -------------------------------------

/// Suspend the spinner AND force the terminal out of application
/// cursor key mode (DECCKM) so arrows send CSI (`ESC [ A`) instead
/// of SS3 (`ESC O A`). inquire's console backend can't parse SS3 —
/// arrows would print A/B/C/D as filter input instead of navigating.
/// zsh's ZLE commonly leaves DECCKM on when running external commands.
///
/// TODO: remove once console-rs/console#283 lands (adds SS3 parsing).
fn prompt<T>(f: impl FnOnce() -> T) -> T {
    suspend(|| {
        // rmkx: ESC [ ? 1 l (DECCKM off) + ESC > (keypad numeric mode)
        let _ = std::io::stderr().write_all(b"\x1b[?1l\x1b>");
        f()
    })
}

/// Theme matching the cyan/blue palette and ▸/✓/✗ glyphs.
/// `Color::Rgb` is unsupported on the console backend — named ANSI only.
fn render_config() -> RenderConfig<'static> {
    RenderConfig::empty()
        .with_prompt_prefix(Styled::new("▸").with_fg(Color::LightCyan))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(Color::LightGreen))
        .with_highlighted_option_prefix(Styled::new("▸ ").with_fg(Color::LightCyan))
        .with_scroll_up_prefix(Styled::new("▴").with_fg(Color::DarkGrey))
        .with_scroll_down_prefix(Styled::new("▾").with_fg(Color::DarkGrey))
        .with_selected_checkbox(Styled::new("◉").with_fg(Color::LightCyan))
        .with_unselected_checkbox(Styled::new("○").with_fg(Color::DarkGrey))
        .with_answer(StyleSheet::new().with_fg(Color::LightCyan))
        .with_help_message(StyleSheet::new().with_fg(Color::DarkGrey))
        .with_default_value(StyleSheet::new().with_fg(Color::DarkGrey))
        .with_canceled_prompt_indicator(Styled::new("✗ cancelled").with_fg(Color::LightRed))
        .with_error_message(
            ErrorMessageRenderConfig::empty()
                .with_prefix(Styled::new("✗").with_fg(Color::LightRed))
                .with_message(StyleSheet::new().with_fg(Color::LightRed)),
        )
}

/// Treat ESC/Ctrl-C as "no" rather than bubbling an InquireError.
fn cancel_is_no(r: Result<bool, InquireError>) -> Result<bool> {
    match r {
        Ok(b) => Ok(b),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// y/N confirm — caller holds suspend() across a show-then-prompt
/// sequence. Releasing suspend between the output and the prompt lets
/// the spinner repaint over both. Gates on TTY (scripts can't
/// accidentally confirm) and resets DECCKM.
///
/// For a standalone confirm with no preceding output, wrap in suspend:
/// `ui::suspend(|| ui::confirm_held(msg))`.
pub fn confirm_held(msg: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let _ = std::io::stderr().write_all(b"\x1b[?1l\x1b>");
    cancel_is_no(Confirm::new(msg).with_default(false).prompt())
}

/// Destructive confirm: red ⚠ prefix, bold, default N. Returns false
/// on non-TTY stdin — destroying infra requires an interactive terminal.
pub fn confirm_destroy(msg: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    cancel_is_no(prompt(|| {
        Confirm::new(msg)
            .with_default(false)
            .with_render_config(
                render_config().with_prompt_prefix(
                    Styled::new("⚠")
                        .with_fg(Color::LightRed)
                        .with_attr(Attributes::BOLD),
                ),
            )
            .prompt()
    }))
}

/// Select from a list. Returns None if stdin isn't a TTY — caller
/// should fall back to a CLI-arg error.
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

// -- depth layer --------------------------------------------------------
//
// Depth = number of `_` spans currently entered on this thread. Tracked
// via a Layer's on_enter/on_exit so it's correct under tokio::join! —
// when one branch yields, its spans exit; when the other branch polls,
// its spans enter. A global Mutex<Vec> can't do this (pushes from
// concurrent branches interleave, pops remove the wrong entry).

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Per-span state stored in span extensions. Concurrency-safe since
/// each span owns its own extension.
struct StepState {
    name: String,
    depth: usize,
    header_printed: bool,
}

/// Span ID of the step whose child last printed a ✓/· line. When the
/// next print comes from a DIFFERENT subtree (concurrent join branch),
/// we re-emit that subtree's `▸` header so children don't visually
/// group under the previous subtree's completion line.
static LAST_PARENT: Mutex<Option<u64>> = Mutex::new(None);

struct DepthLayer;

impl<S> Layer<S> for DepthLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, _attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span not in registry");
        if span.name() != "_" {
            return;
        }
        // Depth = count of `_` ancestors (walk the parent chain).
        let depth = span.scope().skip(1).filter(|s| s.name() == "_").count();
        span.extensions_mut().insert(StepState {
            name: String::new(), // filled by init_step_state
            depth,
            header_printed: false,
        });
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        if ctx.span(id).is_some_and(|s| s.name() == "_") {
            DEPTH.set(DEPTH.get() + 1);
        }
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        if ctx.span(id).is_some_and(|s| s.name() == "_") {
            DEPTH.set(DEPTH.get() - 1);
        }
    }
}

/// Record name into the span's extension.
fn init_step_state(span: &Span, name: &str) {
    span.with_subscriber(|(id, sub)| {
        if let Some(reg) = sub.downcast_ref::<tracing_subscriber::Registry>()
            && let Some(s) = reg.span(id)
            && let Some(st) = s.extensions_mut().get_mut::<StepState>()
        {
            st.name = name.to_string();
        }
    });
}

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

/// Initialize tracing. Call once from main() with the LevelFilter
/// from clap-verbosity-flag.
///
/// Two modes:
///   default/`-q` → fancy: bottom-line spinner, ✓/✗ tree, depth-indented info!()
///   `-v`+        → plain: stock tracing fmt with timestamps + targets,
///                  no spinner, no tree. For debugging — the fancy output
///                  can obscure what's actually happening.
pub fn init(level: LevelFilter) {
    inquire::set_global_render_config(render_config());

    LEVEL.set(level).ok();

    // step() spans are named "_" and exist only for the depth tree.
    // Hide them from fmt in both modes — they'd show as "_:_:"
    // context prefixes.
    let hide_ui_spans =
        tracing_subscriber::filter::filter_fn(|meta| !(meta.is_span() && meta.name() == "_"));

    if is_verbose() {
        // Plain mode. RUST_LOG overrides the flag.
        // Runtime internals capped at info even at -vvv — their trace
        // spans flood output and become parents of our debug! events.
        const RUNTIME_CAP: &str = "tokio=info,runtime=info,mio=info,h2=info";
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(format!("{level},{RUNTIME_CAP}")));

        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_filter(hide_ui_spans),
            )
            .init();
        return;
    }

    // Fancy mode. At default (Warn), xtask itself still logs at info —
    // the flag controls dep noise, not our own narration.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},xtask=info")));

    SPINNER.start();

    tracing_subscriber::registry()
        .with(filter)
        .with(DepthLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(IndentedFormat)
                .with_writer(SpinnerWriter)
                .with_filter(hide_ui_spans),
        )
        .init();
}

/// Event formatter for fancy mode: `{indent}{glyph} {message}`.
///
/// Single-glyph level marker so log lines are structurally parallel
/// to the ✓/✗ step lines — `· tofu: no changes` lands at the same
/// column as `✓ tofu plan`, not one off because compact's `INFO`
/// label has a leading space.
///
/// Also emits lazy ancestor headers before the event (same as
/// `finish()`), so an `info!()` that fires before any step completes
/// doesn't appear orphaned above `▸ k8s up`.
struct IndentedFormat;

impl<S, N> FormatEvent<S, N> for IndentedFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut w: format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        // Headers for unprinted ancestors. Emitted inline — the
        // SpinnerWriter wrapper handles clearing the spinner line.
        for (d, name) in emit_ancestor_headers(false) {
            for _ in 0..d {
                w.write_str("  ")?;
            }
            writeln!(w, "{} {name}", style("▸").blue())?;
        }

        let glyph = match *event.metadata().level() {
            Level::ERROR => style("!").red().bold(),
            Level::WARN => style("!").yellow(),
            _ => style("·").dim(),
        };
        for _ in 0..DEPTH.get() {
            w.write_str("  ")?;
        }
        write!(w, "{glyph} ")?;
        ctx.field_format().format_fields(w.by_ref(), event)?;
        writeln!(w)
    }
}

/// Walk the current span's ancestry, collect unprinted `▸` headers,
/// mark them printed, return `(depth, name)` pairs root→leaf.
///
/// `skip_self`: true from `finish()` (the current step is about to
/// print its own ✓/✗, don't header it); false from the event
/// formatter (the event is AT the current step's depth, so that
/// step needs a header too).
fn emit_ancestor_headers(skip_self: bool) -> Vec<(usize, String)> {
    let mut headers: Vec<(usize, String)> = vec![];
    Span::current().with_subscriber(|(id, sub)| {
        let Some(reg) = sub.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        let Some(span) = reg.span(id) else { return };

        // Detect a subtree switch: if the last thing printed belonged
        // to a different parent step, we need to re-emit our own
        // parent's header (even if header_printed=true) so this line
        // visually groups under the right ▸. Happens during
        // tokio::join! when one branch's output interleaves with
        // another's.
        let my_parent = span
            .scope()
            .skip(skip_self as usize + 1)
            .find(|s| s.name() == "_")
            .map(|s| s.id().into_u64());
        let mut last = LAST_PARENT.lock().unwrap();
        let switched = my_parent.is_some() && *last != my_parent;
        *last = my_parent;
        drop(last);

        for (i, ancestor) in span.scope().skip(skip_self as usize).enumerate() {
            let mut ext = ancestor.extensions_mut();
            let Some(st) = ext.get_mut::<StepState>() else {
                continue;
            };
            // i==0 is the immediate parent — re-emit if we switched.
            let force = switched && i == 0;
            if !st.header_printed || force {
                headers.push((st.depth, st.name.clone()));
                st.header_printed = true;
            }
        }
    });
    headers.reverse();
    headers
}

// -- spinner ------------------------------------------------------------
//
// One bottom-line spinner showing the innermost active step. A
// dedicated thread repaints `\r⠋ {msg} {elapsed}\x1b[K` every 80ms.
// Permanent output (✓/✗ lines, info!() events) goes through eprint()
// or SpinnerWriter, both of which wipe the spinner line with `\r\x1b[K`
// before writing so the repaint slots back in cleanly underneath.
//
// Stack-shaped: step() entry pushes (name, started); exit pops. The
// thread always shows top-of-stack. Concurrent join! branches are
// best-effort here (last-enter wins) — the persistent ✓/✗ tree above
// is the authoritative record.

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);

#[derive(Default)]
struct Spinner {
    stack: Mutex<Vec<(String, Instant)>>,
    /// Set while the painter thread is alive (TTY + fancy mode).
    running: AtomicBool,
    /// Incremented by suspend(); painter skips repaint while >0.
    paused: Mutex<u32>,
}

static SPINNER: Spinner = Spinner {
    stack: Mutex::new(Vec::new()),
    running: AtomicBool::new(false),
    paused: Mutex::new(0),
};

impl Spinner {
    /// Spawn the painter thread. No-op if stderr isn't a TTY (CI logs
    /// don't want `\r` carriage returns) or it's already running.
    fn start(&'static self) {
        if !std::io::stderr().is_terminal() || self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::spawn(move || {
            let mut i = 0usize;
            loop {
                std::thread::sleep(TICK);
                if *self.paused.lock().unwrap() > 0 {
                    continue;
                }
                let stack = self.stack.lock().unwrap();
                let Some((msg, started)) = stack.last() else {
                    continue;
                };
                let frame = FRAMES[i % FRAMES.len()];
                i += 1;
                // Truncate to terminal width so `\r` overwrites cleanly
                // (a wrapped spinner line leaves garbage on repaint).
                // Elapsed is "Ns" — second granularity is plenty for
                // operations that warrant a spinner at all.
                let cols = console::Term::stderr().size().1 as usize;
                let elapsed = started.elapsed().as_secs();
                let line = format!(
                    "{} {msg} {}",
                    style(frame).cyan(),
                    style(format!("{elapsed}s")).dim()
                );
                let line = console::truncate_str(&line, cols, "…");
                let _ = write!(std::io::stderr(), "\r{line}\x1b[K");
            }
        });
    }

    fn push(&self, name: &str) {
        self.stack
            .lock()
            .unwrap()
            .push((name.to_string(), Instant::now()));
    }

    fn pop(&self) {
        self.stack.lock().unwrap().pop();
    }

    /// Replace the top-of-stack message (preserves its start time).
    /// Used by [`set_message`] for last-line tail / poll progress.
    fn set_message(&self, msg: &str) {
        if let Some(top) = self.stack.lock().unwrap().last_mut() {
            top.0 = msg.to_string();
        }
    }

    /// Pause repaints and clear the spinner line. Returns a guard that
    /// resumes on drop. Reentrant (counted) so suspend-within-suspend
    /// (sh::run_interactive inside tofu's confirm block) doesn't resume
    /// early.
    fn pause(&'static self) -> impl Drop {
        if self.running.load(Ordering::SeqCst) {
            *self.paused.lock().unwrap() += 1;
            let _ = write!(std::io::stderr(), "\r\x1b[K");
        }
        scopeguard::guard((), |()| {
            if self.running.load(Ordering::SeqCst) {
                *self.paused.lock().unwrap() -= 1;
            }
        })
    }

    /// Write a permanent line above the spinner: clear the spinner
    /// line, emit `s`, let the next tick repaint underneath.
    fn eprint(&self, s: std::fmt::Arguments<'_>) {
        let mut e = std::io::stderr().lock();
        if self.running.load(Ordering::SeqCst) {
            let _ = e.write_all(b"\r\x1b[K");
        }
        let _ = e.write_fmt(s);
    }
}

/// `MakeWriter` for the fancy-mode fmt layer. Each event clears the
/// spinner line first so `info!()` output doesn't land on top of it.
struct SpinnerWriter;

impl std::io::Write for SpinnerWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SPINNER.eprint(format_args!(
            "{}",
            std::str::from_utf8(buf).unwrap_or_default()
        ));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SpinnerWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        SpinnerWriter
    }
}

/// `eprintln!` that clears the spinner line first. For permanent
/// output that isn't a `tracing` event (sh.rs error dumps, push
/// failures). Safe in non-TTY/verbose mode — just writes through.
#[allow(clippy::print_stderr)]
pub fn eprint(args: std::fmt::Arguments<'_>) {
    SPINNER.eprint(args);
}

// -- step ---------------------------------------------------------------

/// Run `f` inside a progress span. Shows a spinner while running,
/// `✓ name` on Ok, `✗ name: err` on Err. Nested step() calls indent.
pub async fn step<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    step_owned(name.to_string(), f()).await
}

/// Mark a step as skipped: prints a dimmed `· name (reason)` line.
/// Use when a step is conditionally bypassed — e.g. `tofu apply` when
/// the plan shows no diff.
pub fn step_skip(name: &str, reason: &str) {
    if is_verbose() {
        tracing::info!(step = name, reason, "skipped");
        return;
    }
    let indent = "  ".repeat(DEPTH.get());
    SPINNER.eprint(format_args!(
        "{indent}{} {} {}\n",
        style("·").dim(),
        name,
        style(format!("({reason})")).dim()
    ));
}

/// Owned-name variant for spawned tasks (JoinSet, tokio::spawn) where
/// the future outlives the &str.
///
/// NOT an `async fn` — the span must be created synchronously at the
/// call site (while the parent span is current), not on first poll of
/// the spawned task (where there's no span context). Returns `impl
/// Future` so the span is captured before spawn, then entered on each
/// poll of the spawned task.
pub fn step_owned<T>(
    name: String,
    fut: impl Future<Output = Result<T>>,
) -> impl Future<Output = Result<T>> {
    let span = info_span!("_");
    init_step_state(&span, &name);
    async move {
        SPINNER.push(&name);
        let r = fut.await;
        SPINNER.pop();
        finish(&name, &r);
        r
    }
    .instrument(span)
}

/// Update the spinner message (for last-line tail, poll progress).
pub fn set_message(msg: &str) {
    SPINNER.set_message(msg);
}

/// Print a ✓/✗ line. First emits `▸ name` headers for any unprinted
/// ancestors so nesting reads correctly (children finish before
/// parents, so without headers the indentation groups children under
/// the PREVIOUS step visually).
///
/// Depth + ancestor state come from span extensions (DepthLayer), not
/// a global stack — so tokio::join! branches each see their own
/// ancestry instead of a corrupted interleaved Vec.
fn finish<T>(name: &str, r: &Result<T>) {
    if *LEVEL.get().unwrap_or(&LevelFilter::WARN) < LevelFilter::WARN {
        return; // -q: silent
    }
    if is_verbose() {
        // Plain mode: step completion as a regular log event.
        match r {
            Ok(_) => tracing::info!(step = name, "done"),
            Err(e) => tracing::error!(step = name, error = %e, "failed"),
        }
        return;
    }

    for (d, n) in emit_ancestor_headers(true) {
        let indent = "  ".repeat(d);
        SPINNER.eprint(format_args!("{indent}{} {n}\n", style("▸").blue()));
    }

    // Prefer the span-recorded depth (thread-local DEPTH can be stale
    // if the span was created on another thread).
    let depth = Span::current()
        .with_subscriber(|(id, sub)| {
            sub.downcast_ref::<tracing_subscriber::Registry>()?
                .span(id)?
                .extensions()
                .get::<StepState>()
                .map(|st| st.depth)
        })
        .flatten()
        .unwrap_or_else(|| DEPTH.get().saturating_sub(1));

    let indent = "  ".repeat(depth);
    match r {
        Ok(_) => SPINNER.eprint(format_args!("{indent}{} {name}\n", style("✓").green())),
        Err(e) => SPINNER.eprint(format_args!("{indent}{} {name}: {e}\n", style("✗").red())),
    }
}

// -- poll ---------------------------------------------------------------

/// Poll inside the CURRENT step span — no new `step()` wrapper.
/// Use this when the caller already wrapped you in `ui::step`;
/// otherwise use [`poll`] which creates its own span.
pub async fn poll_in<T, F, Fut>(interval: Duration, max: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let base = Span::current()
        .with_subscriber(|(id, sub)| {
            sub.downcast_ref::<tracing_subscriber::Registry>()?
                .span(id)?
                .extensions()
                .get::<StepState>()
                .map(|st| st.name.clone())
        })
        .flatten()
        .unwrap_or_default();
    for i in 1..=max {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        set_message(&format!("{base} (attempt {i}/{max})"));
        tokio::time::sleep(interval).await;
    }
    bail!("timed out after {max} attempts")
}

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
            set_message(&format!("{name} (attempt {i}/{max})"));
            tokio::time::sleep(interval).await;
        }
        bail!("timed out after {max} attempts")
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    /// Capture fmt output to a buffer so we can assert on indentation.
    #[derive(Clone, Default)]
    struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    #[test]
    fn info_indents_by_step_depth() {
        let buf = Buf::default();
        let sub = tracing_subscriber::registry().with(DepthLayer).with(
            tracing_subscriber::fmt::layer()
                .event_format(IndentedFormat)
                .with_writer(buf.clone())
                .with_ansi(false),
        );
        let _g = tracing::subscriber::set_default(sub);

        tracing::info!("depth0");
        let outer = info_span!("_");
        let _o = outer.enter();
        tracing::info!("depth1");
        let inner = info_span!("_");
        let _i = inner.enter();
        tracing::info!("depth2");

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        // Glyph format: "· message" with 2-space indent per depth.
        // Ancestor headers (▸ _) are emitted before nested events but
        // the `_` name is empty in this test — we didn't call
        // init_step_state. Filter to just the event lines.
        let events: Vec<_> = out.lines().filter(|l| l.contains("depth")).collect();
        assert_eq!(events.len(), 3, "got: {out:?}");
        assert_eq!(events[0], "· depth0");
        assert_eq!(events[1], "  · depth1");
        assert_eq!(events[2], "    · depth2");
    }

    /// step_owned must create its span synchronously at the call site
    /// so parent-chain depth is captured BEFORE the future is spawned.
    /// If it were `async fn`, info_span! would run on first poll —
    /// on a spawned task that's a worker thread with no span context,
    /// so depth would be 0.
    #[tokio::test]
    async fn step_owned_captures_parent_depth_at_call_site() {
        let sub = tracing_subscriber::registry().with(DepthLayer);
        let _g = tracing::subscriber::set_default(sub);

        // Simulate step("outer") entered at depth 0.
        let outer = info_span!("_");
        init_step_state(&outer, "outer");
        let depth = async {
            // step_owned creates its span NOW with outer as parent.
            // The inner async reads StepState.depth (same path as
            // finish()) — should be 1 regardless of which thread
            // eventually polls it, because DepthLayer::on_new_span
            // recorded the depth at creation time.
            step_owned("inner".into(), async {
                Ok::<_, anyhow::Error>(
                    Span::current()
                        .with_subscriber(|(id, sub)| {
                            sub.downcast_ref::<tracing_subscriber::Registry>()?
                                .span(id)?
                                .extensions()
                                .get::<StepState>()
                                .map(|st| st.depth)
                        })
                        .flatten()
                        .expect("StepState missing"),
                )
            })
            .await
        }
        .instrument(outer)
        .await
        .unwrap();

        assert_eq!(depth, 1);
    }
}
