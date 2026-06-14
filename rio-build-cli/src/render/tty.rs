//! Interactive (TTY) renderer.
//!
//! Superconsole model: finished-build verdicts and failure extracts go
//! to normal terminal scrollback (permanent lines), while a region at
//! the bottom is redrawn in place showing the running builds. `f`
//! opens a log browser over failed, running and substituting builds;
//! logs open in `$PAGER` (a snapshot — events queue while paging) or
//! are dumped to scrollback.
//!
//! The terminal is only put into cbreak mode (no raw mode, no
//! alternate screen), so output stays in scrollback and Ctrl-C keeps
//! working.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rio_proto::types::DerivationEventKind;
use tokio::sync::Notify;

use super::term::{clip_ansi, subseq_match, trunc_middle};
use super::{Clock, DrvEdge, DrvRow, RenderEvent, fmt_duration, ingest_log, route, wall_clock};

const CSI: &str = "\x1b[";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

/// Failure extract printed to scrollback.
const EXTRACT_LINES: usize = 5;
const BUFFER_LINES: usize = 10_000;
/// Max browser rows per page (each row is 2 lines).
const PAGE: usize = 6;
/// Redraw interval.
pub const TICK: Duration = Duration::from_millis(250);

/// CSI final bytes for the cursor keys, mapped to browser navigation.
fn arrow_key(b: u8) -> Option<u8> {
    match b {
        b'A' => Some(b'k'),
        b'B' => Some(b'j'),
        b'C' => Some(b'n'),
        b'D' => Some(b'p'),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    List,
}

/// Owns the terminal: permanent scrollback lines + ephemeral region.
///
/// Flicker avoidance: every update is composed into one buffer and
/// emitted with a single write, wrapped in the synchronized-output
/// escape (DEC 2026) so capable terminals paint it atomically.
/// Ephemeral lines are overwritten in place with clear-to-EOL instead
/// of erasing the whole region first.
pub struct Display<W: Write> {
    out: W,
    pub ephemeral_lines: usize,
    last_ephemeral: Vec<String>,
    suspended: bool,
    /// Permanent lines queued while suspended (a pager owns the screen).
    pending: Vec<String>,
    /// Test override; `None` reads `console::Term::stderr().size()`.
    pub term_size: Option<(u16, u16)>,
}

impl<W: Write> Display<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            ephemeral_lines: 0,
            last_ephemeral: Vec::new(),
            suspended: false,
            pending: Vec::new(),
            term_size: None,
        }
    }

    fn size(&self) -> (u16, u16) {
        self.term_size
            .unwrap_or_else(|| console::Term::stderr().size())
    }

    fn emit(&mut self, buf: &str) {
        let _ = write!(self.out, "{CSI}?2026h{buf}{CSI}?2026l");
        let _ = self.out.flush();
    }

    /// Cursor sits below the old region; overwrite it line by line.
    fn compose_ephemeral(&mut self, lines: Vec<String>) -> String {
        let (_, width) = self.size();
        let mut buf = if self.ephemeral_lines > 0 {
            format!("{CSI}{}F", self.ephemeral_lines)
        } else {
            String::new()
        };
        for line in &lines {
            // Cell-aware clip (CJK/emoji = 2): an overwide line would
            // wrap and break the cursor-up math for the whole region.
            // RESET re-applied because the clip may drop a trailing reset.
            buf.push_str(&clip_ansi(line, width as usize));
            buf.push_str(RESET);
            buf.push_str(CSI);
            buf.push_str("K\n");
        }
        if lines.len() < self.ephemeral_lines {
            // Old region was taller: drop leftover lines.
            buf.push_str(CSI);
            buf.push('J');
        }
        self.ephemeral_lines = lines.len();
        self.last_ephemeral = lines;
        buf
    }

    pub fn permanent(&mut self, lines: &[String]) {
        if self.suspended {
            // Another program (pager) owns the terminal: queue for later.
            self.pending.extend_from_slice(lines);
            return;
        }
        // Erase ephemeral, print permanent lines, repaint ephemeral: one write.
        let mut buf = if self.ephemeral_lines > 0 {
            format!("{CSI}{}F{CSI}J", self.ephemeral_lines)
        } else {
            String::new()
        };
        self.ephemeral_lines = 0;
        for line in lines {
            buf.push_str(line);
            buf.push('\n');
        }
        let last = std::mem::take(&mut self.last_ephemeral);
        buf.push_str(&self.compose_ephemeral(last));
        self.emit(&buf);
    }

    pub fn ephemeral(&mut self, lines: Vec<String>) {
        if self.suspended {
            return;
        }
        let buf = self.compose_ephemeral(lines);
        self.emit(&buf);
    }

    /// Clear our region and stop touching the terminal.
    pub fn suspend(&mut self) {
        self.ephemeral(vec![]);
        self.last_ephemeral.clear();
        self.suspended = true;
    }

    /// Re-take the terminal; flush events that happened meanwhile.
    /// Returns the count so the caller can tell the user what was missed.
    pub fn resume(&mut self) -> usize {
        self.suspended = false;
        // The pager left the cursor anywhere; start fresh on a new line.
        self.ephemeral_lines = 0;
        let pending = std::mem::take(&mut self.pending);
        let n = pending.len();
        if n > 0 {
            self.permanent(&pending);
        }
        n
    }

    fn write_ctl(&mut self, seq: &str) {
        let _ = self.out.write_all(seq.as_bytes());
        let _ = self.out.flush();
    }
}

/// What category of row a `DrvRow` is in (display + browsable scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowState {
    Running,
    Substituting,
    Failed,
    Succeeded,
}

pub struct TtyRenderer<W: Write> {
    pub display: Display<W>,
    clock: Clock,
    started_at: Instant,
    mode: Mode,
    /// drv_path → row.
    rows: HashMap<String, DrvRow>,
    /// drv_path → state. Failed rows keep their `DrvRow` for the
    /// browser; succeeded rows drop theirs.
    state: HashMap<String, RowState>,
    spin: usize,
    /// `false` = pager, `true` = dump to scrollback.
    dump_action: bool,
    page: usize,
    filter: String,
    filter_input: bool,
    /// LIST mode: order pinned on entry so rows don't shift under the
    /// user's finger; later arrivals are appended and tagged "new".
    pinned: Vec<String>,
    new_rows: Vec<String>,
    cursor: usize,
    last_viewed: Option<String>,
    /// Builds finished, only the browser keeps us up.
    all_done: bool,
    flash_text: String,
    flash_until: Instant,
    /// Set while a pager owns the terminal.
    pub pager_gate: Arc<AtomicBool>,
    /// Idle signal for [`want_idle_wait`](Self::want_idle_wait).
    idle: Arc<Notify>,
    stopping: bool,
    /// Request to spawn a pager (handled by the drive loop, not here —
    /// the renderer state machine stays sync).
    pager_request: Option<String>,
}

impl<W: Write> TtyRenderer<W> {
    pub fn new(out: W, clock: Clock) -> Self {
        let now = clock();
        Self {
            display: Display::new(out),
            clock,
            started_at: now,
            mode: Mode::Normal,
            rows: HashMap::new(),
            state: HashMap::new(),
            spin: 0,
            dump_action: false,
            page: 0,
            filter: String::new(),
            filter_input: false,
            pinned: Vec::new(),
            new_rows: Vec::new(),
            cursor: 0,
            last_viewed: None,
            all_done: false,
            flash_text: String::new(),
            flash_until: now,
            pager_gate: Arc::new(AtomicBool::new(false)),
            idle: Arc::new(Notify::new()),
            stopping: false,
            pager_request: None,
        }
    }

    pub fn idle_notify(&self) -> Arc<Notify> {
        self.idle.clone()
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    // ── event ingestion ────────────────────────────────────────────

    pub fn on_event(&mut self, ev: &RenderEvent) {
        let RenderEvent::Build(ev) = ev;
        let now = self.now();
        match route(ev) {
            DrvEdge::Open(d) => {
                let kind = DerivationEventKind::try_from(d.kind).ok();
                if let Some(row) = self.rows.get_mut(&d.derivation_path) {
                    // Substituting → Queued/Started: reset, not no-op.
                    row.started_at = now;
                    row.last_output_at = now;
                    row.phase = None;
                    row.lines.clear();
                    if kind != Some(DerivationEventKind::Substituting) {
                        self.state
                            .insert(d.derivation_path.clone(), RowState::Running);
                    }
                    return;
                }
                if kind == Some(DerivationEventKind::Queued) {
                    return;
                }
                let mut row = DrvRow::new(&d.derivation_path, build_id, now, BUFFER_LINES);
                let st = if kind == Some(DerivationEventKind::Substituting) {
                    row.phase = Some("fetching".into());
                    RowState::Substituting
                } else {
                    RowState::Running
                };
                self.state.insert(d.derivation_path.clone(), st);
                self.rows.insert(d.derivation_path.clone(), row);
            }
            DrvEdge::Log(batch) => {
                if let Some(row) = self.rows.get_mut(&batch.derivation_path) {
                    ingest_log(row, batch, now);
                }
            }
            DrvEdge::Phase(p) => {
                if let Some(row) = self.rows.get_mut(&p.derivation_path) {
                    row.phase = Some(p.phase.clone());
                    row.push_line(format!("@ phase {}", p.phase), now);
                }
            }
            DrvEdge::Substitute(p) => {
                if let Some(row) = self.rows.get_mut(&p.derivation_path) {
                    row.last_output_at = now;
                    let line = format!(
                        "{:.1}/{:.1} MiB from {}",
                        p.bytes_done as f64 / (1 << 20) as f64,
                        p.bytes_expected as f64 / (1 << 20) as f64,
                        p.upstream_uri
                    );
                    // Replace, don't append: a substituting row carries
                    // one synthetic progress line.
                    row.lines.clear();
                    row.lines.push_back(line);
                }
            }
            DrvEdge::Close { kind, drv } => {
                self.finish_drv(&drv.derivation_path, kind, &drv.error_message);
            }
            DrvEdge::Drain { build_id } => {
                // This build was cancelled/failed: drop its still-open
                // rows. Other roots keep theirs.
                let gone: Vec<String> = self
                    .rows
                    .iter()
                    .filter(|(k, r)| {
                        r.build_id == build_id
                            && matches!(
                                self.state.get(*k),
                                Some(RowState::Running | RowState::Substituting)
                            )
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                for drv in gone {
                    self.state.remove(&drv);
                    self.rows.remove(&drv);
                }
            }
            DrvEdge::Ignore => {}
        }
    }

    fn finish_drv(&mut self, drv: &str, kind: DerivationEventKind, error: &str) {
        let Some(row) = self.rows.get(drv) else {
            return;
        };
        let stamp = format!("{DIM}{}{RESET}", local_hms());
        let dur = fmt_duration(row.elapsed(self.now()));
        let label = row.label.clone();
        let build_id = row.build_id.clone();
        match kind {
            DerivationEventKind::Failed => {
                let mut lines = vec![format!(
                    "{stamp} {RED}✘  {label}{RESET}  {dur}{}",
                    if error.is_empty() {
                        String::new()
                    } else {
                        format!(": {error}")
                    }
                )];
                lines.push(format!(
                    "{DIM}── error extract (full log: [f] or rio build --attach {build_id}) ──{RESET}"
                ));
                let tail: Vec<_> = row.lines.iter().rev().take(EXTRACT_LINES).collect();
                for line in tail.into_iter().rev() {
                    lines.push(format!("{DIM}{label}>{RESET} {line}"));
                }
                self.display.permanent(&lines);
                self.state.insert(drv.to_string(), RowState::Failed);
            }
            DerivationEventKind::Cached => {
                self.display.permanent(&[format!(
                    "{stamp} {GREEN}✔  {label}{RESET}  {DIM}(cached){RESET}"
                )]);
                self.state.insert(drv.to_string(), RowState::Succeeded);
                self.rows.remove(drv);
            }
            _ => {
                self.display
                    .permanent(&[format!("{stamp} {GREEN}✔  {label}{RESET}  {dur}")]);
                self.state.insert(drv.to_string(), RowState::Succeeded);
                self.rows.remove(drv);
            }
        }
    }

    // ── lifecycle ──────────────────────────────────────────────────

    fn engaged(&self) -> bool {
        self.mode == Mode::List || self.pager_gate.load(Ordering::Relaxed)
    }

    fn signal_if_idle(&mut self) {
        if self.all_done && !self.engaged() {
            self.idle.notify_one();
        }
    }

    /// Builds are done: if the user is in the log browser or a pager,
    /// keep the UI alive until they leave instead of yanking it away.
    /// Returns whether to wait (caller awaits the notify).
    pub fn want_idle_wait(&mut self) -> bool {
        if !self.engaged() {
            return false;
        }
        self.all_done = true;
        self.flash("builds finished — leave browser ([q/Esc]) to exit");
        true
    }

    pub fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.display.ephemeral(vec![]);
        self.display.write_ctl(SHOW_CURSOR);
    }

    pub fn on_winch(&mut self) {
        // Terminal resized: old region may have rewrapped, making the
        // cursor-up anchor wrong. Abandon it and let the next tick
        // draw a fresh region.
        self.display.ephemeral_lines = 0;
    }

    /// Extract a pending pager request (drive loop handles it).
    pub fn take_pager_request(&mut self) -> Option<DrvRow> {
        let drv = self.pager_request.take()?;
        self.rows.get(&drv).cloned()
    }

    pub fn pager_finished(&mut self, missed: usize) {
        if missed > 0 {
            self.flash(&format!("while paging: {missed} new lines in scrollback ↑"));
        }
        self.signal_if_idle();
    }

    // ── rendering ──────────────────────────────────────────────────

    pub fn tick(&mut self) {
        self.spin += 1;
        if self.pager_gate.load(Ordering::Relaxed) {
            return;
        }
        let lines = match self.mode {
            Mode::List => self.render_list(),
            Mode::Normal => self.render_normal(),
        };
        self.display.ephemeral(lines);
    }

    fn count(&self, st: RowState) -> usize {
        self.state.values().filter(|s| **s == st).count()
    }

    fn open_rows_sorted(&self) -> Vec<&DrvRow> {
        // Running + substituting, longest-elapsed first; ties keep
        // start order (no jitter).
        let mut v: Vec<&DrvRow> = self
            .state
            .iter()
            .filter(|(_, s)| matches!(s, RowState::Running | RowState::Substituting))
            .filter_map(|(k, _)| self.rows.get(k))
            .collect();
        v.sort_by_key(|r| r.started_at);
        v
    }

    fn header(&self) -> String {
        let elapsed = fmt_duration(self.now().duration_since(self.started_at));
        let done = if self.all_done {
            format!(" · {BOLD}finished{RESET}")
        } else {
            String::new()
        };
        let running = self.count(RowState::Running) + self.count(RowState::Substituting);
        format!(
            " {BOLD}BUILD{RESET} {GREEN}✔{}{RESET} {RED}✘{}{RESET} ⏵{running}   {elapsed}{done}",
            self.count(RowState::Succeeded),
            self.count(RowState::Failed)
        )
    }

    pub fn render_normal(&self) -> Vec<String> {
        let mut lines = vec![self.header()];
        let spin = SPINNER[self.spin % SPINNER.len()];
        // A region taller than the terminal breaks the cursor-up anchor,
        // so clamp the rows. Reserve 3 lines: header, footer, off-by-one.
        let (height, _) = self.display.size();
        let mut budget = (height as i32 - 3).max(2) as usize;
        let open = self.open_rows_sorted();
        let now = self.now();
        for (shown, row) in open.iter().enumerate() {
            let nrows = if row.lines.is_empty() { 1 } else { 2 };
            if budget < nrows + usize::from(shown < open.len() - 1) {
                lines.push(format!("     {DIM}… +{} more{RESET}", open.len() - shown));
                break;
            }
            budget -= nrows;
            let phase = row.phase.as_deref().unwrap_or("build");
            lines.push(format!(
                " {YELLOW}{spin}{RESET} {:<40} {phase:<12} {:>7}",
                trunc_middle(&row.label, 40),
                fmt_duration(row.elapsed(now))
            ));
            if let Some(last) = row.lines.back() {
                lines.push(format!("     {DIM}{last}{RESET}"));
            }
        }
        let nfailed = self.count(RowState::Failed);
        let badge = if nfailed > 0 {
            format!(" ({RED}✘{nfailed}{RESET}{DIM})")
        } else {
            String::new()
        };
        let reopen = if self.last_viewed.is_some() {
            " · [o] last log"
        } else {
            ""
        };
        let footer = format!("{DIM} [f] logs{badge}{reopen} · [Ctrl-C] abort{RESET}");
        lines.push(self.flash_line().unwrap_or(footer));
        lines
    }

    /// Failures first (triage priority), then running and substituting.
    fn browsable(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .state
            .iter()
            .filter(|(_, s)| **s == RowState::Failed)
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v.extend(self.open_rows_sorted().iter().map(|r| r.drv_path.clone()));
        v
    }

    fn enter_list(&mut self) {
        self.mode = Mode::List;
        self.pinned = self.browsable();
        self.new_rows.clear();
        self.cursor = 0;
        self.page = 0;
    }

    fn refresh_pinned(&mut self) {
        // Append rows that appeared after the list was opened. Existing
        // rows never move; finished rows stay selectable.
        let known: std::collections::HashSet<_> = self.pinned.iter().cloned().collect();
        for k in self.browsable() {
            if !known.contains(&k) {
                self.pinned.push(k.clone());
                self.new_rows.push(k);
            }
        }
    }

    fn filtered(&mut self) -> Vec<String> {
        self.refresh_pinned();
        if self.filter.is_empty() {
            return self.pinned.clone();
        }
        self.pinned
            .iter()
            .filter(|k| {
                self.rows
                    .get(*k)
                    .is_some_and(|r| subseq_match(&self.filter, &r.label))
            })
            .cloned()
            .collect()
    }

    fn page_size(&self) -> usize {
        // Adapt to terminal height: header/separator/footer need 4
        // lines, each row takes 2.
        let (h, _) = self.display.size();
        ((h as usize).saturating_sub(4) / 2).clamp(1, PAGE)
    }

    fn pages(&mut self) -> usize {
        self.filtered().len().div_ceil(self.page_size()).max(1)
    }

    pub fn flash(&mut self, msg: &str) {
        self.flash_text = msg.to_string();
        self.flash_until = self.now() + Duration::from_millis(1500);
    }

    fn flash_line(&self) -> Option<String> {
        (self.now() < self.flash_until).then(|| format!(" {YELLOW}{}{RESET}", self.flash_text))
    }

    fn status_label(&self, drv: &str) -> String {
        match self.state.get(drv) {
            Some(RowState::Running | RowState::Substituting) => {
                let phase = self
                    .rows
                    .get(drv)
                    .and_then(|r| r.phase.clone())
                    .unwrap_or_else(|| "build".into());
                format!("{YELLOW}⏵ {phase}{RESET}")
            }
            Some(RowState::Succeeded) => format!("{GREEN}✔ done{RESET}"),
            _ => format!("{RED}✘ failed{RESET}"),
        }
    }

    pub fn render_list(&mut self) -> Vec<String> {
        let filtered = self.filtered();
        let size = self.page_size();
        self.cursor = self.cursor.min(filtered.len().saturating_sub(1));
        if !filtered.is_empty() {
            self.page = self.cursor / size;
        }
        let pages = self.pages();
        let page_info = if pages > 1 {
            format!(" page {}/{pages}", self.page + 1)
        } else {
            String::new()
        };
        let match_info = if self.filter.is_empty() {
            String::new()
        } else {
            format!(" · {}/{} match", filtered.len(), self.pinned.len())
        };
        let mut lines = vec![
            self.header(),
            format!(" {DIM}── logs{page_info}{match_info} ──────────────────{RESET}"),
        ];
        self.page = self.page.min(pages - 1);
        let visible: Vec<String> = filtered
            .iter()
            .skip(self.page * size)
            .take(size)
            .cloned()
            .collect();
        let now = self.now();
        for (i, drv) in visible.iter().enumerate() {
            let selected = self.page * size + i == self.cursor;
            let marker = if selected {
                format!("{BOLD}▸{RESET}")
            } else {
                " ".into()
            };
            let new = if self.new_rows.contains(drv) {
                format!(" {YELLOW}new{RESET}")
            } else {
                String::new()
            };
            let row = self.rows.get(drv);
            let label = row.map(|r| r.label.clone()).unwrap_or_default();
            let dur = row
                .map(|r| fmt_duration(r.elapsed(now)))
                .unwrap_or_default();
            lines.push(format!(
                " {marker}{BOLD}{}{RESET}  {:<40} {dur:>7}  {}{new}",
                i + 1,
                trunc_middle(&label, 40),
                self.status_label(drv)
            ));
            let gist = row
                .and_then(|r| r.lines.back().cloned())
                .unwrap_or_else(|| "(no output yet)".into());
            lines.push(format!("      {DIM}{gist}{RESET}"));
        }
        if visible.is_empty() {
            let msg = if self.filter.is_empty() {
                "no failed or running builds"
            } else {
                "(no matches)"
            };
            lines.push(format!("  {DIM}{msg}{RESET}"));
        }
        if self.filter_input {
            lines.push(format!(
                " {YELLOW}/{}█{RESET}  {DIM}[Enter] apply · [Esc] clear{RESET}",
                self.filter
            ));
            return lines;
        }
        if let Some(fl) = self.flash_line() {
            lines.push(fl);
            return lines;
        }
        let action = if self.dump_action {
            "dump to scrollback"
        } else {
            "$PAGER"
        };
        let paging = if pages > 1 { " · [n/p] page" } else { "" };
        let flt = if self.filter.is_empty() {
            String::new()
        } else {
            format!(" · filter:{YELLOW}/{}{RESET}{DIM}", self.filter)
        };
        lines.push(format!(
            " {DIM}[Enter/1-{}] {action} · [j/k] move · [d]→{}{paging} · [/] filter{flt} · \
             [f/Esc] back{RESET}",
            visible.len().max(1),
            if self.dump_action { "pager" } else { "dump" }
        ));
        lines
    }

    // ── log viewing ────────────────────────────────────────────────

    fn dump_log(&mut self, drv: &str) {
        let Some(row) = self.rows.get(drv) else {
            return;
        };
        let running = matches!(
            self.state.get(drv),
            Some(RowState::Running | RowState::Substituting)
        );
        let state = if running {
            format!("{}, running", row.phase.as_deref().unwrap_or("build"))
        } else {
            "failed".into()
        };
        let mut lines = vec![format!(
            "{DIM}────── log: {} ({state}, {}) ──────{RESET}",
            row.label,
            fmt_duration(row.elapsed(self.now()))
        )];
        for line in &row.lines {
            lines.push(format!("{DIM}{}>{RESET} {line}", row.label));
        }
        lines.push(format!(
            "{DIM}────── end ({} lines{}) · re-fetch: rio build --attach {} ──────{RESET}",
            row.lines.len(),
            if running { " so far" } else { "" },
            row.build_id
        ));
        let n = row.lines.len();
        self.display.permanent(&lines);
        self.flash(&format!("dumped {n} lines ↑"));
    }

    fn open(&mut self, drv: &str) {
        self.last_viewed = Some(drv.to_string());
        if self.dump_action {
            self.dump_log(drv);
            return;
        }
        if self.pager_gate.load(Ordering::Relaxed) {
            // Two opens can race within one stdin batch: never spawn
            // two pagers.
            return;
        }
        self.pager_request = Some(drv.to_string());
    }

    // ── key handling ───────────────────────────────────────────────

    pub fn on_key(&mut self, key: u8) {
        match self.mode {
            Mode::Normal => self.key_normal(key),
            Mode::List if self.filter_input => self.key_filter(key),
            Mode::List => self.key_list(key),
        }
    }

    fn key_normal(&mut self, key: u8) {
        match key {
            b'f' => self.enter_list(),
            b'o' => {
                if let Some(drv) = self.last_viewed.clone() {
                    self.open(&drv);
                }
            }
            _ => {}
        }
    }

    fn key_filter(&mut self, key: u8) {
        match key {
            b'\r' | b'\n' => {
                self.filter_input = false;
                let m = self.filtered();
                if m.len() == 1 {
                    // Single match: open it directly, fzf-style.
                    self.open(&m[0].clone());
                }
            }
            0x1b => {
                self.filter.clear();
                self.filter_input = false;
            }
            0x7f | 0x08 => {
                self.filter.pop();
            }
            b if (b as char).is_ascii_graphic() || b == b' ' => {
                self.filter.push(b as char);
                self.cursor = 0;
            }
            _ => {}
        }
    }

    fn key_list(&mut self, key: u8) {
        match key {
            0x1b if !self.filter.is_empty() => self.filter.clear(),
            b'f' | 0x1b | b'q' => {
                self.mode = Mode::Normal;
                self.filter.clear();
                self.filter_input = false;
                self.signal_if_idle();
            }
            b'/' => self.filter_input = true,
            b'd' => self.dump_action = !self.dump_action,
            b'j' => self.move_cursor(1),
            b'k' => self.move_cursor(-1),
            b'n' => self.move_page(1),
            b'p' => self.move_page(-1),
            b'\r' | b'\n' => {
                let f = self.filtered();
                if let Some(drv) = f.get(self.cursor.min(f.len().saturating_sub(1))).cloned() {
                    self.open(&drv);
                } else {
                    self.flash("nothing to open");
                }
            }
            d @ b'1'..=b'9' => {
                let size = self.page_size();
                let f = self.filtered();
                let visible: Vec<_> = f.iter().skip(self.page * size).take(size).collect();
                let idx = (d - b'1') as usize;
                if let Some(drv) = visible.get(idx) {
                    let drv = (*drv).clone();
                    self.open(&drv);
                } else {
                    self.flash(&format!("no entry {} on this page", d as char));
                }
            }
            b if (b as char).is_ascii_graphic() => {
                self.flash(&format!("unknown key {:?} — [/] to filter", b as char));
            }
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let n = self.filtered().len() as i32;
        let target = self.cursor as i32 + delta;
        if (0..n).contains(&target) {
            self.cursor = target as usize;
        } else {
            self.flash(if delta > 0 {
                "already at bottom"
            } else {
                "already at top"
            });
        }
    }

    fn move_page(&mut self, delta: i32) {
        let pages = self.pages() as i32;
        let target = self.page as i32 + delta;
        if (0..pages).contains(&target) {
            self.page = target as usize;
            self.cursor = self.page * self.page_size();
        } else {
            self.flash(if delta > 0 {
                "already at last page"
            } else {
                "already at first page"
            });
        }
    }

    pub fn feed_bytes(&mut self, data: &[u8]) {
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b != 0x1b {
                self.on_key(if b < 0x80 { b } else { b'?' });
                i += 1;
                continue;
            }
            if i + 1 == data.len() {
                // Bare ESC (no sequence tail in this batch): the Esc key.
                self.on_key(0x1b);
                break;
            }
            if data[i + 1] == b'[' {
                // CSI sequence: consume through its final byte
                // (0x40–0x7e) so the tail isn't misread as commands.
                // Arrows navigate; other sequences are ignored — they
                // must NOT act as Esc, or cursor keys would close the
                // browser.
                let mut j = i + 2;
                while j < data.len() && !(0x40..=0x7e).contains(&data[j]) {
                    j += 1;
                }
                if let Some(k) = data.get(j).and_then(|b| arrow_key(*b)) {
                    self.on_key(k);
                }
                i = j + 1;
            } else {
                // Alt-modified key or other two-byte escape: swallow both.
                i += 2;
            }
        }
    }
}

/// Local-time `HH:MM:SS` (no `chrono` dep — `libc::localtime_r`).
fn local_hms() -> String {
    // SAFETY: time(NULL) and localtime_r are async-signal-safe; the tm
    // buffer is stack-local.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

// ── terminal ownership (cbreak, cursor, stdin reader) ──────────────

/// Restores termios + cursor on drop so panic/SIGINT can't leave the
/// terminal broken. Construct on the main stack (not inside a spawned
/// task) so `Drop` runs on `?`-unwind.
pub struct TermGuard {
    saved: libc::termios,
    armed: bool,
}

impl TermGuard {
    /// Enter cbreak mode: clear `ICANON|ECHO`, keep `ISIG` set (Ctrl-C
    /// still delivers SIGINT). Hide the cursor (it strobes during
    /// redraws). Returns `None` if stdin isn't a tty.
    pub fn cbreak() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr on a tty fd; the buffer is local.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw) != 0 {
                return None;
            }
            let _ = std::io::stderr().write_all(HIDE_CURSOR.as_bytes());
            Some(Self { saved, armed: true })
        }
    }

    /// Temporarily restore cooked mode (for the pager); the guard's
    /// `Drop` is disarmed and re-armed by [`recbreak`](Self::recbreak).
    pub fn cook(&mut self) {
        // SAFETY: same as cbreak.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.saved);
        }
        let _ = std::io::stderr().write_all(SHOW_CURSOR.as_bytes());
        self.armed = false;
    }

    pub fn recbreak(&mut self) {
        // SAFETY: same as cbreak.
        unsafe {
            let mut raw = self.saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw);
        }
        let _ = std::io::stderr().write_all(HIDE_CURSOR.as_bytes());
        self.armed = true;
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: tcsetattr on stdin restoring saved attrs.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.saved);
            }
        }
        let _ = std::io::stderr().write_all(SHOW_CURSOR.as_bytes());
    }
}

/// Readiness-based stdin reader. `AsyncFd` over the raw fd (not
/// `tokio::io::stdin()`, which spawns a blocking thread that can't be
/// cleanly detached for the pager).
pub struct StdinKeys {
    fd: tokio::io::unix::AsyncFd<std::os::fd::RawFd>,
    saved_flags: libc::c_int,
}

impl StdinKeys {
    pub fn new() -> std::io::Result<Self> {
        // Non-blocking so a spurious readable wakeup returns EAGAIN
        // instead of stalling the render task. Restored on Drop —
        // O_NONBLOCK is a description flag, the pager would inherit it.
        // SAFETY: fcntl on a known-valid fd.
        let saved_flags = unsafe {
            let flags = libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL);
            libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            flags
        };
        Ok(Self {
            fd: tokio::io::unix::AsyncFd::with_interest(
                libc::STDIN_FILENO,
                tokio::io::Interest::READABLE,
            )?,
            saved_flags,
        })
    }

    /// Next batch of key bytes; `None` on EOF (terminal hangup).
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        loop {
            let mut guard = self.fd.readable_mut().await.ok()?;
            let mut buf = [0u8; 1024];
            // SAFETY: read into a stack buffer of known length.
            let n =
                unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut _, buf.len()) };
            match n {
                0 => return None,
                n if n > 0 => return Some(buf[..n as usize].to_vec()),
                _ => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        continue;
                    }
                    return None;
                }
            }
        }
    }
}

impl Drop for StdinKeys {
    fn drop(&mut self) {
        // SAFETY: fcntl on a known-valid fd.
        unsafe {
            libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, self.saved_flags);
        }
    }
}

// ── pager ──────────────────────────────────────────────────────────

pub fn pager_cmd(row: &DrvRow, state: &str) -> Vec<String> {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    // Shell-style split (no full shlex; PAGER is rarely quoted).
    let mut cmd: Vec<String> = pager.split_whitespace().map(String::from).collect();
    if cmd.is_empty() {
        cmd.push("less".into());
    }
    if std::path::Path::new(&cmd[0])
        .file_name()
        .is_some_and(|n| n == "less")
    {
        // Title prompt so the user knows what they're reading; the error
        // is almost always at the end, so open at +G. The tmpfile is a
        // snapshot — events queue while the pager owns the terminal —
        // so live follow (+F) would track a static file. Reopen ([o])
        // for a fresh snapshot.
        cmd.push(format!("-Ps{} ({state}) ?pB(%pB\\%).", row.label));
        cmd.push("+G".into());
    }
    cmd
}

pub fn write_log_tmpfile(row: &DrvRow, state: &str, dur: &str) -> std::io::Result<PathBuf> {
    let mut tf = tempfile::Builder::new()
        .suffix(&format!("-{}.log", row.label.replace('/', "_")))
        .tempfile()?;
    writeln!(tf, "# log: {} ({state}, {dur})", row.label)?;
    for line in &row.lines {
        writeln!(tf, "{}> {line}", row.label)?;
    }
    let (_, path) = tf.keep()?;
    Ok(path)
}

// ── drive loop ─────────────────────────────────────────────────────

/// Drive the TTY renderer: drain `rx`, redraw on tick, feed key bytes,
/// hand off to a pager on request.
pub async fn drive(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RenderEvent>,
    pager_gate: Arc<AtomicBool>,
) {
    let mut r = TtyRenderer::new(std::io::stderr(), wall_clock());
    r.pager_gate = pager_gate;
    let mut guard = TermGuard::cbreak();
    let mut keys = guard.as_ref().and_then(|_| StdinKeys::new().ok());
    let mut tick = tokio::time::interval(TICK);
    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    loop {
        let key_fut = async {
            match &mut keys {
                Some(k) => k.read().await,
                None => std::future::pending().await,
            }
        };
        let winch_fut = async {
            match &mut winch {
                Some(w) => w.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => r.on_event(&ev),
                None => {
                    // Channel closed: builds done. Wait for the user
                    // to leave the browser if engaged.
                    if r.want_idle_wait() {
                        let idle = r.idle_notify();
                        loop {
                            tokio::select! {
                                _ = idle.notified() => break,
                                _ = tick.tick() => r.tick(),
                                k = async {
                                    match &mut keys {
                                        Some(k) => k.read().await,
                                        None => std::future::pending().await,
                                    }
                                } => match k {
                                    Some(b) => { r.feed_bytes(&b); r.tick(); }
                                    None => break,
                                },
                            }
                        }
                    }
                    break;
                }
            },
            _ = tick.tick() => r.tick(),
            k = key_fut => match k {
                Some(b) => { r.feed_bytes(&b); r.tick(); }
                None => { keys = None; }
            },
            _ = winch_fut => r.on_winch(),
        }
        // Pager requested: hand the terminal over.
        if let Some(row) = r.take_pager_request() {
            r.pager_gate.store(true, Ordering::Relaxed);
            r.display.suspend();
            // Detach the key reader so we don't steal the pager's
            // keystrokes (drop deregisters the AsyncFd and clears
            // O_NONBLOCK).
            drop(keys.take());
            if let Some(g) = &mut guard {
                g.cook();
            }
            run_pager(&row).await;
            if let Some(g) = &mut guard {
                g.recbreak();
            }
            keys = guard.as_ref().and_then(|_| StdinKeys::new().ok());
            let missed = r.display.resume();
            r.pager_gate.store(false, Ordering::Relaxed);
            r.pager_finished(missed);
        }
    }
    r.stop();
    drop(guard);
}

async fn run_pager(row: &DrvRow) {
    let state = row.phase.clone().unwrap_or_else(|| "log snapshot".into());
    let dur = fmt_duration(row.elapsed(Instant::now()));
    let Ok(path) = write_log_tmpfile(row, &state, &dur) else {
        return;
    };
    let cmd = pager_cmd(row, &state);
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.entry("LESS".into()).or_insert_with(|| "RXi".into());
    let mut child = match tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(&path)
        .envs(&env)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let _ = child.wait().await;
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rio_proto::types::{
        BuildEvent, BuildLogBatch, DerivationEvent, SubstituteProgress, build_event::Event,
    };

    use super::*;

    const DRV: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";

    type Buf = Arc<Mutex<Vec<u8>>>;
    struct BufWrite(Buf);
    impl Write for BufWrite {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    type FakeClock = Arc<Mutex<Instant>>;

    fn make() -> (TtyRenderer<BufWrite>, Buf, FakeClock) {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let now: FakeClock = Arc::new(Mutex::new(Instant::now()));
        let now2 = now.clone();
        let mut r = TtyRenderer::new(
            BufWrite(buf.clone()),
            Box::new(move || *now2.lock().unwrap()),
        );
        r.display.term_size = Some((24, 120));
        (r, buf, now)
    }

    fn advance(now: &FakeClock, secs: u64) {
        *now.lock().unwrap() += Duration::from_secs(secs);
    }

    fn text(buf: &Buf) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn plain(lines: &[String]) -> String {
        regex::Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]")
            .unwrap()
            .replace_all(&lines.join("\n"), "")
            .into_owned()
    }

    fn ev(event: Event) -> RenderEvent {
        RenderEvent::Build(BuildEvent {
            build_id: "build-abc".into(),
            timestamp: None,
            event: Some(event),
        })
    }

    fn drv_ev(path: &str, kind: DerivationEventKind) -> RenderEvent {
        ev(Event::Derivation(DerivationEvent {
            derivation_path: path.into(),
            kind: kind as i32,
            ..Default::default()
        }))
    }

    fn drv_n(n: usize) -> String {
        format!("/nix/store/{:0<32}-pkg-{n:02}.drv", n)
    }

    fn log(path: &str, first: u64, lines: &[&str]) -> RenderEvent {
        RenderEvent::Log(BuildLogBatch {
            derivation_path: path.into(),
            lines: lines.iter().map(|s| s.as_bytes().to_vec()).collect(),
            first_line_number: first,
            executor_id: String::new(),
        })
    }

    fn fail_n(r: &mut TtyRenderer<BufWrite>, n: usize) -> Vec<String> {
        let mut paths = vec![];
        for i in 0..n {
            let p = drv_n(i);
            r.on_event(&drv_ev(&p, DerivationEventKind::Started));
            r.on_event(&log(&p, 0, &[&format!("log of {i}")]));
            r.on_event(&drv_ev(&p, DerivationEventKind::Failed));
            paths.push(p);
        }
        paths
    }

    // ── Display ────────────────────────────────────────────────────

    #[test]
    fn display_ephemeral_overwrites_in_place() {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let mut d = Display::new(BufWrite(buf.clone()));
        d.term_size = Some((24, 80));
        d.ephemeral(vec!["a".into(), "b".into()]);
        d.ephemeral(vec!["c".into()]);
        let t = text(&buf);
        assert!(t.contains(&format!("{CSI}2F")), "{t}");
        assert!(t.contains(&format!("{CSI}J")), "{t}");
        assert_eq!(d.ephemeral_lines, 1);
    }

    #[test]
    fn display_permanent_above_ephemeral() {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let mut d = Display::new(BufWrite(buf.clone()));
        d.term_size = Some((24, 80));
        d.ephemeral(vec!["status".into()]);
        d.permanent(&["event".into()]);
        let t = text(&buf);
        assert!(t.find("event").unwrap() < t.rfind("status").unwrap());
        assert_eq!(d.ephemeral_lines, 1);
    }

    #[test]
    fn display_suspend_queues_permanent() {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let mut d = Display::new(BufWrite(buf.clone()));
        d.term_size = Some((24, 80));
        d.ephemeral(vec!["status".into()]);
        d.suspend();
        let before = text(&buf);
        d.permanent(&["while paging".into()]);
        assert_eq!(text(&buf), before);
        assert_eq!(d.resume(), 1);
        assert!(text(&buf).contains("while paging"));
    }

    #[test]
    fn display_sync_markers() {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let mut d = Display::new(BufWrite(buf.clone()));
        d.term_size = Some((24, 80));
        d.ephemeral(vec!["x".into()]);
        let t = text(&buf);
        assert!(t.starts_with(&format!("{CSI}?2026h")));
        assert!(t.ends_with(&format!("{CSI}?2026l")));
    }

    // ── lifecycle ──────────────────────────────────────────────────

    #[test]
    fn lifecycle_and_failure_extract() {
        let (mut r, buf, now) = make();
        let drv_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bad.drv";
        r.on_event(&drv_ev(DRV, DerivationEventKind::Started));
        r.on_event(&drv_ev(drv_b, DerivationEventKind::Started));
        let lines: Vec<String> = (0..10).map(|i| format!("l{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        r.on_event(&log(drv_b, 0, &refs));
        advance(&now, 65);
        r.on_event(&drv_ev(DRV, DerivationEventKind::Completed));
        r.on_event(&drv_ev(drv_b, DerivationEventKind::Failed));
        assert_eq!(r.count(RowState::Succeeded), 1);
        assert_eq!(r.count(RowState::Failed), 1);
        assert_eq!(r.count(RowState::Running), 0);
        let t = plain(&[text(&buf)]);
        assert!(t.contains("✔  x.drv  1m05s"), "{t}");
        assert!(t.contains("✘  bad.drv  1m05s"), "{t}");
        assert!(t.contains("bad.drv> l5"), "{t}");
        assert!(!t.contains("bad.drv> l4"), "{t}");
        assert!(t.contains("rio build --attach build-abc"), "{t}");
    }

    #[test]
    fn render_normal_rows() {
        let (mut r, _buf, now) = make();
        r.on_event(&drv_ev(DRV, DerivationEventKind::Started));
        advance(&now, 5);
        let drv_b = drv_n(1);
        r.on_event(&drv_ev(&drv_b, DerivationEventKind::Started));
        r.on_event(&log(DRV, 0, &["compiling foo.c"]));
        let t = plain(&r.render_normal());
        assert!(t.contains("x.drv"), "{t}");
        assert!(t.contains("compiling foo.c"), "{t}");
        assert!(t.contains("pkg-01.drv"), "{t}");
        assert!(t.contains("[f] logs"), "{t}");
    }

    #[test]
    fn render_normal_clamps_to_height() {
        let (mut r, _buf, _now) = make();
        for i in 0..30 {
            let p = drv_n(i);
            r.on_event(&drv_ev(&p, DerivationEventKind::Started));
            r.on_event(&log(&p, 0, &["output"]));
        }
        let lines = r.render_normal();
        assert!(lines.len() <= 23, "{}", lines.len());
        assert!(lines.iter().any(|l| l.contains("more")));
    }

    // ── browser ────────────────────────────────────────────────────

    #[test]
    fn browser_pinned_order_and_new_tag() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 2);
        r.on_key(b'f');
        assert_eq!(r.mode, Mode::List);
        assert_eq!(r.pinned.len(), 2);
        let late = drv_n(99);
        r.on_event(&drv_ev(&late, DerivationEventKind::Started));
        r.on_event(&drv_ev(&late, DerivationEventKind::Failed));
        let t = plain(&r.render_list());
        assert_eq!(r.pinned.len(), 3);
        assert!(t.contains("new"), "{t}");
    }

    #[test]
    fn browser_clamp_and_flash_expires() {
        let (mut r, _buf, now) = make();
        fail_n(&mut r, 2);
        r.on_key(b'f');
        r.on_key(b'k');
        assert!(r.flash_text.contains("top"));
        r.on_key(b'j');
        r.on_key(b'j');
        assert_eq!(r.cursor, 1);
        assert!(r.flash_text.contains("bottom"));
        advance(&now, 2);
        assert!(r.flash_line().is_none());
    }

    #[test]
    fn browser_paging() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, PAGE + 2);
        r.on_key(b'f');
        let _ = r.render_list();
        assert_eq!(r.pages(), 2);
        r.on_key(b'n');
        assert_eq!(r.page, 1);
        assert_eq!(r.cursor, PAGE);
        r.on_key(b'n');
        assert!(r.flash_text.contains("last page"));
        r.on_key(b'p');
        assert_eq!(r.page, 0);
    }

    #[test]
    fn browser_filter_subsequence_and_layered_esc() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 3);
        let extra = "/nix/store/cccccccccccccccccccccccccccccccc-checks.deadnix.drv";
        r.on_event(&drv_ev(extra, DerivationEventKind::Started));
        r.on_event(&drv_ev(extra, DerivationEventKind::Failed));
        r.on_key(b'f');
        r.on_key(b'/');
        assert!(r.filter_input);
        for ch in b"ddnx" {
            r.on_key(*ch);
        }
        assert_eq!(r.filtered(), vec![extra.to_string()]);
        r.on_key(0x1b);
        assert!(!r.filter_input);
        assert_eq!(r.filter, "");
        assert_eq!(r.mode, Mode::List);
    }

    #[test]
    fn browser_digit_and_dump() {
        let (mut r, buf, _now) = make();
        fail_n(&mut r, 2);
        r.on_key(b'f');
        r.on_key(b'd');
        assert!(r.dump_action);
        r.on_key(b'9');
        assert!(r.flash_text.contains("no entry 9"));
        r.on_key(b'1');
        assert!(r.last_viewed.is_some());
        assert!(r.flash_text.contains("dumped"));
        let t = plain(&[text(&buf)]);
        assert!(t.contains("pkg-00.drv> log of 0"), "{t}");
    }

    #[test]
    fn browser_includes_substituting_and_reset_on_started() {
        let (mut r, _buf, now) = make();
        r.on_event(&drv_ev(DRV, DerivationEventKind::Substituting));
        r.on_event(&ev(Event::SubstituteProgress(SubstituteProgress {
            derivation_path: DRV.into(),
            bytes_done: 1 << 20,
            bytes_expected: 2 << 20,
            upstream_uri: "https://cache".into(),
        })));
        r.on_key(b'f');
        // Q3=C: substituting rows are browsable.
        assert_eq!(r.pinned, vec![DRV.to_string()]);
        let t = plain(&r.render_list());
        assert!(t.contains("⏵ fetching"), "{t}");
        r.on_key(b'q');
        // RC-A: Substituting → Started resets the row.
        advance(&now, 30);
        r.on_event(&drv_ev(DRV, DerivationEventKind::Started));
        assert_eq!(r.state[DRV], RowState::Running);
        assert!(r.rows[DRV].lines.is_empty());
        assert_eq!(r.rows[DRV].phase, None);
    }

    #[test]
    fn exit_browser_keys() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 1);
        for k in [b'f', b'q', 0x1b] {
            r.on_key(b'f');
            assert_eq!(r.mode, Mode::List);
            r.on_key(k);
            assert_eq!(r.mode, Mode::Normal);
        }
    }

    #[test]
    fn unknown_key_flashes() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 1);
        r.on_key(b'f');
        r.on_key(b'z');
        assert!(r.flash_text.contains("unknown key"));
    }

    #[test]
    fn browser_succeeded_label() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 1);
        let slow = drv_n(50);
        r.on_event(&drv_ev(&slow, DerivationEventKind::Started));
        r.on_key(b'f');
        r.on_event(&drv_ev(&slow, DerivationEventKind::Completed));
        let t = plain(&r.render_list());
        assert!(t.contains("✔ done"), "{t}");
        assert_eq!(t.matches("✘ failed").count(), 1);
    }

    #[test]
    fn render_list_adapts_to_height() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 10);
        r.on_key(b'f');
        r.display.term_size = Some((10, 80));
        let lines = r.render_list();
        assert!(lines.len() <= 9, "{}", lines.len());
        assert!(r.pages() * r.page_size() >= 10);
    }

    #[test]
    fn arrow_keys_navigate_not_escape() {
        let (mut r, _buf, _now) = make();
        fail_n(&mut r, 3);
        r.on_key(b'f');
        // Down arrow moves; must NOT act as Esc.
        r.feed_bytes(b"\x1b[B");
        assert_eq!(r.mode, Mode::List);
        assert_eq!(r.cursor, 1);
        r.feed_bytes(b"\x1b[A");
        assert_eq!(r.cursor, 0);
        // Unknown CSI (Home) swallowed entirely.
        r.feed_bytes(b"\x1b[1~");
        assert_eq!(r.mode, Mode::List);
        assert_eq!(r.cursor, 0);
        // Two arrows in one batch both processed.
        r.feed_bytes(b"\x1b[B\x1b[B");
        assert_eq!(r.cursor, 2);
        // Bare ESC still exits.
        r.feed_bytes(b"\x1b");
        assert_eq!(r.mode, Mode::Normal);
    }

    #[test]
    fn pager_cmd_less_opens_at_end() {
        let (mut r, _buf, _now) = make();
        let p = fail_n(&mut r, 1).pop().unwrap();
        let row = r.rows.get(&p).unwrap();
        let cmd = pager_cmd(row, "failed");
        assert_eq!(cmd[0], "less");
        assert_eq!(cmd.last().unwrap(), "+G");
    }

    #[test]
    fn drain_on_build_terminal_is_per_build() {
        let (mut r, _buf, _now) = make();
        r.on_event(&drv_ev(DRV, DerivationEventKind::Started));
        r.on_event(&drv_ev(&drv_n(1), DerivationEventKind::Substituting));
        // A second root's row must survive the first root's cancel.
        r.on_event(&RenderEvent::Build(BuildEvent {
            build_id: "build-other".into(),
            timestamp: None,
            event: Some(Event::Derivation(DerivationEvent {
                derivation_path: drv_n(2),
                kind: DerivationEventKind::Started as i32,
                ..Default::default()
            })),
        }));
        r.on_event(&ev(Event::Cancelled(rio_proto::types::BuildCancelled {
            reason: "user".into(),
        })));
        assert_eq!(r.count(RowState::Running), 1);
        assert_eq!(r.count(RowState::Substituting), 0);
        assert!(r.rows.contains_key(&drv_n(2)));
    }

    #[test]
    fn local_hms_shape() {
        let s = local_hms();
        assert_eq!(s.len(), 8);
        assert_eq!(&s[2..3], ":");
        assert_eq!(&s[5..6], ":");
    }
}
