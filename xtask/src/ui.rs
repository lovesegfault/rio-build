//! Span-native progress + verbosity.
//!
//! Every unit of work is a `tracing::Span`. tracing-indicatif renders
//! spans as nested progress bars; child spans auto-indent via
//! `{span_child_prefix}`. `info!` prints above active bars via the
//! layer's suspending writer.

use std::cell::Cell;
use std::fmt::Display;
use std::future::Future;
use std::io::IsTerminal;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, bail};
use console::style;
use indicatif::ProgressStyle;
use inquire::ui::{Attributes, Color, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Select, Text};

use tracing::level_filters::LevelFilter;
use tracing::{Instrument, Span, info_span, span};
use tracing_indicatif::IndicatifLayer;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_subscriber::fmt::{FormatEvent, FormatFields, format};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

pub use tracing_indicatif::suspend_tracing_indicatif as suspend;

// -- inquire theme + prompt helpers -------------------------------------

/// Suspend progress bars AND force the terminal out of application
/// cursor key mode (DECCKM) so arrows send CSI (`ESC [ A`) instead
/// of SS3 (`ESC O A`). inquire's console backend can't parse SS3 —
/// arrows would print A/B/C/D as filter input instead of navigating.
/// zsh's ZLE commonly leaves DECCKM on when running external commands.
///
/// TODO: remove once console-rs/console#283 lands (adds SS3 parsing).
fn prompt<T>(f: impl FnOnce() -> T) -> T {
    suspend(|| {
        use std::io::Write;
        // rmkx: ESC [ ? 1 l (DECCKM off) + ESC > (keypad numeric mode)
        let _ = std::io::stderr().write_all(b"\x1b[?1l\x1b>");
        f()
    })
}

/// Theme matching our indicatif cyan/blue palette and ▸/✓/✗ glyphs.
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

/// y/N confirm. Suspended so indicatif doesn't redraw over the input.
/// Returns false on non-TTY stdin — scripts can't accidentally confirm.
pub fn confirm(msg: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    cancel_is_no(prompt(|| Confirm::new(msg).with_default(false).prompt()))
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
            name: String::new(), // filled by set_step_name
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

/// Record the step name into the current span's extension. Called
/// right after span creation (the name isn't known at macro time
/// since info_span! needs a static string).
fn set_step_name(span: &Span, name: &str) {
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

/// Initialize tracing + indicatif. Call once from main() with the
/// LevelFilter from clap-verbosity-flag.
pub fn init(level: LevelFilter) {
    inquire::set_global_render_config(render_config());

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
        .with(DepthLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(IndentedFormat::new())
                .with_writer(indicatif.get_stderr_writer())
                .with_filter(hide_ui_spans),
        )
        .with(indicatif)
        .init();
}

/// Event formatter that prefixes each log line with step-depth indent
/// so `info!()` inside `provision → tofu plan` lands at the same
/// column as `✓ tofu plan`, not at column 0.
///
/// Depth comes from the `DEPTH` thread-local (maintained by
/// `DepthLayer::on_enter/on_exit`), so it's correct per-poll under
/// `tokio::join!`.
struct IndentedFormat(format::Format<format::Compact, ()>);

impl IndentedFormat {
    fn new() -> Self {
        Self(
            format::Format::default()
                .compact()
                .without_time()
                .with_target(false),
        )
    }
}

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
        for _ in 0..DEPTH.get() {
            w.write_str("  ")?;
        }
        self.0.format_event(ctx, w, event)
    }
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
    set_step_name(&span, name);
    let name = name.to_string();
    async move {
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
    set_step_name(&span, &name);
    async move {
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
    set_step_name(&span, name);
    let name = name.to_string();
    async move {
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
/// Depth + ancestor state come from span extensions (DepthLayer), not
/// a global stack — so tokio::join! branches each see their own
/// ancestry instead of a corrupted interleaved Vec.
///
/// Not via pb_set_finish_message — that freezes bars in LIFO stack
/// order. indicatif_eprintln! writes via MultiProgress::println which
/// inserts above active bars without a clear/redraw cycle.
fn finish<T>(name: &str, r: &Result<T>) {
    if *LEVEL.get().unwrap_or(&LevelFilter::WARN) < LevelFilter::WARN {
        return; // -q: silent
    }

    // Walk the span tree: collect unprinted ancestor headers + our depth.
    let mut headers: Vec<(usize, String)> = vec![];
    let mut depth = DEPTH.get().saturating_sub(1);
    Span::current().with_subscriber(|(id, sub)| {
        let Some(reg) = sub.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        let Some(span) = reg.span(id) else { return };
        // scope() is leaf→root; skip self (the current step).
        for ancestor in span.scope().skip(1) {
            let mut ext = ancestor.extensions_mut();
            let Some(st) = ext.get_mut::<StepState>() else {
                continue;
            };
            if !st.header_printed {
                headers.push((st.depth, st.name.clone()));
                st.header_printed = true;
            }
        }
        // Prefer the span-recorded depth (thread-local DEPTH can be
        // stale if the span was created on another thread).
        if let Some(st) = span.extensions().get::<StepState>() {
            depth = st.depth;
        }
    });

    // Print root→leaf.
    for (d, n) in headers.into_iter().rev() {
        let indent = "  ".repeat(d);
        tracing_indicatif::indicatif_eprintln!("{indent}{} {n}", style("▸").blue());
    }

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
                .event_format(IndentedFormat::new())
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
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3, "got: {out:?}");
        // Compact format: " INFO message" (leading space before level).
        assert!(lines[0].starts_with(" INFO"), "depth0: {:?}", lines[0]);
        assert!(lines[1].starts_with("   INFO"), "depth1: {:?}", lines[1]);
        assert!(lines[2].starts_with("     INFO"), "depth2: {:?}", lines[2]);
    }
}
