//! Span-native progress + verbosity.
//!
//! Every unit of work is a `tracing::Span`. tracing-indicatif renders
//! spans as nested progress bars; child spans auto-indent via
//! `{span_child_prefix}`. `info!` prints above active bars via the
//! layer's suspending writer.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::io::IsTerminal;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
use console::style;
use indicatif::ProgressStyle;
use inquire::ui::{Attributes, Color, ErrorMessageRenderConfig, RenderConfig, StyleSheet, Styled};
use inquire::validator::Validation;
use inquire::{Confirm, InquireError, Select, Text};

use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing::{Instrument, Span, info_span, span};
use tracing_indicatif::IndicatifLayer;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_subscriber::fmt::{FormatEvent, FormatFields, format};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Suspend progress bars (if any) while running `f`. In plain mode
/// (`-v`+) there are no bars — `suspend_tracing_indicatif` handles
/// the no-subscriber case gracefully (just calls `f`), so this is
/// safe to call unconditionally.
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
    /// Phase span id, for lookup in PHASES. Storing the Span handle
    /// directly here created a reference cycle (span → extension →
    /// clone of span) that prevented on_close from firing.
    phase_id: Option<u64>,
}

/// Phase Span handles keyed by `Id::into_u64()`. Phases register on
/// start and deregister via a drop-guard in their async block —
/// outside the extension, so no self-reference cycle.
static PHASES: Mutex<Option<HashMap<u64, Span>>> = Mutex::new(None);

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
            phase_id: None, // set by phase() via init_step_state
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

/// Record name + (optionally) phase id into the span's extension.
fn init_step_state(span: &Span, name: &str, phase_id: Option<u64>) {
    span.with_subscriber(|(id, sub)| {
        if let Some(reg) = sub.downcast_ref::<tracing_subscriber::Registry>()
            && let Some(s) = reg.span(id)
            && let Some(st) = s.extensions_mut().get_mut::<StepState>()
        {
            st.name = name.to_string();
            st.phase_id = phase_id;
        }
    });
}

/// Walk `span`'s ancestry and return a handle to the nearest phase.
/// The handle comes from PHASES (keyed by id), not from the span
/// extension — storing it in the extension created a cycle.
fn nearest_phase(span: &Span) -> Option<Span> {
    let id = span.with_subscriber(|(id, sub)| {
        let reg = sub.downcast_ref::<tracing_subscriber::Registry>()?;
        reg.span(id)?
            .scope()
            .skip(1)
            .find_map(|s| s.extensions().get::<StepState>().and_then(|st| st.phase_id))
    })??;
    PHASES.lock().unwrap().as_ref()?.get(&id).cloned()
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
///   default/`-q` → fancy: indicatif bars, ✓/✗ tree, depth-indented info!()
///   `-v`+        → plain: stock tracing fmt with timestamps + targets,
///                  no bars, no tree. For debugging — the fancy output
///                  can obscure what's actually happening.
pub fn init(level: LevelFilter) {
    inquire::set_global_render_config(render_config());

    LEVEL.set(level).ok();

    // step()/phase() spans are named "_" and exist only for indicatif.
    // Hide them from fmt in both modes — in plain mode they'd show as
    // "_:_:" context prefixes, in fancy mode same problem.
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

    let indicatif = IndicatifLayer::new()
        .with_span_child_prefix_indent("  ")
        .with_span_child_prefix_symbol("▸ ")
        .with_progress_style(spinner_style());

    tracing_subscriber::registry()
        .with(filter)
        .with(DepthLayer)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(IndentedFormat)
                .with_writer(indicatif.get_stderr_writer())
                .with_filter(hide_ui_spans),
        )
        .with(indicatif)
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
        // Headers for unprinted ancestors. Emitted inline (not
        // indicatif_eprintln!) — we're already inside the layer's
        // write path, MultiProgress suspend wraps this whole call.
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
        for ancestor in span.scope().skip(skip_self as usize) {
            let mut ext = ancestor.extensions_mut();
            let Some(st) = ext.get_mut::<StepState>() else {
                continue;
            };
            if !st.header_printed {
                headers.push((st.depth, st.name.clone()));
                st.header_printed = true;
            }
        }
    });
    headers.reverse();
    headers
}

// -- styles -------------------------------------------------------------

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{span_child_prefix}{spinner:.cyan} {wide_msg} {elapsed:.dim}")
        .unwrap()
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "])
}

fn bar_style() -> ProgressStyle {
    // No `/{len}` — the bar auto-grows as steps are discovered, so
    // `6/8` would read as "6 of 8 total" when 8 is just "discovered
    // so far". Show only `{pos}` (granular step count); the bar's
    // fill ratio (done/started) still gives a visual sense of whether
    // the currently-running work is caught up.
    ProgressStyle::with_template(
        "{span_child_prefix}{msg:.bold} {wide_bar:.cyan/blue} {pos:>3} steps {elapsed:.dim}",
    )
    .unwrap()
}

// -- step/phase ---------------------------------------------------------

/// Run `f` inside a progress span. Shows a spinner while running,
/// `✓ name` on Ok, `✗ name: err` on Err. Nested step() calls indent.
///
/// Auto-grows + increments the nearest phase ancestor's bar: starting
/// a step adds 1 to the phase's length, finishing adds 1 to position.
/// The bar tracks every nested step without manual `inc()` calls.
pub async fn step<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    step_owned(name.to_string(), f()).await
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
    let span = info_span!("_", indicatif.pb_show = tracing::field::Empty);
    span.pb_set_style(&spinner_style());
    span.pb_set_message(&name);
    init_step_state(&span, &name, None);
    // Grow the phase bar at step start; captured handle is moved into
    // the future so finish can increment it regardless of poll thread.
    let phase = nearest_phase(&span);
    if let Some(p) = &phase {
        p.pb_inc_length(1);
    }
    async move {
        let r = fut.await;
        if let Some(p) = &phase {
            p.pb_inc(1);
        }
        finish(&name, &r);
        r
    }
    .instrument(span)
}

/// Run `f` inside an auto-growing progress bar span. Every nested
/// `step()` (at any depth) grows the length by 1 on start and
/// increments position by 1 on finish — the bar tracks total
/// granular progress without callers needing to know the count.
pub async fn phase<F, Fut, T>(name: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let span = info_span!("_", indicatif.pb_show = tracing::field::Empty);
    span.pb_set_style(&bar_style());
    span.pb_set_message(name);
    span.pb_set_length(0);
    // Register in PHASES (not in the extension — that would be a
    // self-reference cycle preventing on_close).
    let id = span.id().map(|i| i.into_u64()).unwrap_or(0);
    init_step_state(&span, name, Some(id));
    PHASES
        .lock()
        .unwrap()
        .get_or_insert_default()
        .insert(id, span.clone());
    let name = name.to_string();
    async move {
        // Deregister on exit so the clone in PHASES drops before the
        // instrumented wrapper drops the primary handle — otherwise
        // refcount stays ≥1 and on_close never fires.
        let _g = scopeguard::guard(id, |id| {
            if let Some(m) = PHASES.lock().unwrap().as_mut() {
                m.remove(&id);
            }
        });
        let r = f().await;
        finish(&name, &r);
        r
    }
    .instrument(span)
    .await
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
        tracing_indicatif::indicatif_eprintln!("{indent}{} {n}", style("▸").blue());
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
        init_step_state(&outer, "outer", None);
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
