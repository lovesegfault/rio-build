// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::{
    cmp::max,
    collections::BTreeMap,
    fmt::Debug,
    io::{IsTerminal, Write},
};

use bytesize::ByteSize;
use chrono::prelude::*;
use comfy_table::{Cell, ColumnConstraint, Table};
use gorgond_client::{
    events::{
        self, BuildEvent, BuildEventKind, BuildTaskEvent, BuildTaskEventKind, BuildTaskUpdate,
    },
    models::{self, BuildTask, BuildTaskLevel},
    Uuid,
};
use miette::{IntoDiagnostic, NamedSource, Result};
use owo_colors::{OwoColorize, Stream, Style};
use serde::Serialize;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Default,
    Json,
}

#[derive(Debug, Clone, clap::Parser)]
#[command(next_help_heading = "Output")]
pub struct OutputArgs {
    /// Output format.
    ///
    /// By default, output will attempt to be more human-friendly if stdout is a TTY.
    #[arg(long, short, global = true)]
    format: Option<Format>,

    /// Colour output, or explicitly disable it with --color=false.
    ///
    /// By default, output will be coloured if stdout is a TTY.
    #[arg(long, short, default_missing_value = "true", require_equals = true, num_args=0..=1, global=true)]
    color: Option<bool>,
}
impl OutputArgs {
    pub fn stdout(&self) -> Format {
        self.format
            .unwrap_or_else(|| match std::io::stdout().is_terminal() {
                true => Format::Default,
                false => Format::Json,
            })
    }
    pub fn stdout_color(&self) -> bool {
        self.color
            .unwrap_or_else(|| std::io::stdout().is_terminal())
    }

    pub fn with<V: Debug + Serialize, F: FnOnce(&V)>(&self, f: F, v: &V) -> Result<()> {
        match self.stdout() {
            Format::Default => {
                f(v);
                Ok(())
            }
            Format::Json => self.dump(v),
        }
    }

    pub fn dump<V: Debug + Serialize>(&self, v: &V) -> Result<()> {
        #[derive(Debug, thiserror::Error, miette::Diagnostic)]
        #[error("Serializing output failed")]
        struct DumpJsonError(
            #[source] serde_json::Error,
            #[source_code] NamedSource<String>,
        );

        let mode = match self.stdout_color() {
            true => colored_json::ColorMode::On,
            false => colored_json::ColorMode::Off,
        };
        let mut stdout = std::io::stdout();
        colored_json::write_colored_json_with_mode(&v, &mut stdout, mode)
            .map_err(|err| DumpJsonError(err, NamedSource::new("", format!("{v:#?}"))))?;
        stdout.write_all(b"\n").into_diagnostic()
    }
}

macro_rules! to_cell {
    ($t:ty) => {
        impl ToCell for $t {
            fn cell(&self) -> Cell {
                Cell::new(self)
            }
        }
    };
}

trait ToCell {
    fn cell(&self) -> Cell;
}
impl<T: ToCell> ToCell for Option<T> {
    fn cell(&self) -> Cell {
        match self.as_ref() {
            Some(v) => v.cell(),
            None => Cell::new("-").add_attribute(comfy_table::Attribute::Dim),
        }
    }
}
impl ToCell for Uuid {
    fn cell(&self) -> Cell {
        Cell::new(self.to_string())
            .set_delimiter('-')
            .add_attribute(comfy_table::Attribute::Dim)
    }
}
impl ToCell for DateTime<Utc> {
    fn cell(&self) -> Cell {
        Cell::new(self.format("%Y-%m-%d %H:%M:%S")).set_delimiter(' ')
    }
}
to_cell!(std::borrow::Cow<'_, str>);
to_cell!(models::BuildResult);
to_cell!(models::BuildTaskLevel);
to_cell!(models::BuildTaskKind);

fn cell<V: ToCell>(v: &V) -> Cell {
    ToCell::cell(v)
}
fn cell_or<B: ToCell, A, F: FnOnce(&A) -> Cell>(b: &B, o: &Option<A>, f: F) -> Cell {
    match o {
        Some(a) => f(a),
        None => b.cell(),
    }
}

fn table() -> Table {
    let mut table = comfy_table::Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    table
}

pub fn build_table<'a>(
    builds: impl IntoIterator<
        Item = (
            &'a models::Build<'a>,
            Option<&'a models::Project<'a>>,
            Option<&'a models::Worker<'a>>,
        ),
    >,
) {
    let mut table = table();
    table
        .set_header([
            "Build ID", "Project", "Kind", "Worker", "Result", "Started", "Ended",
        ])
        .set_constraints([ColumnConstraint::ContentWidth])
        .add_rows(builds.into_iter().map(|(b, p, w)| {
            [
                cell(&b.id),
                cell_or(&b.project_id, &p, |p| p.spec.slug.cell()),
                cell(&b.spec.kind),
                cell_or(&b.worker_id, &w, |w| w.spec.slug.cell()),
                cell(&b.result),
                cell(&b.started_at),
                cell(&b.ended_at),
            ]
        }));
    println!("{table}");
}

pub fn repo_table<'a>(repos: impl IntoIterator<Item = &'a models::Repository<'a>>) {
    let mut table = table();
    table
        .set_header(["Repo ID", "Kind", "Ident"])
        .set_constraints([ColumnConstraint::ContentWidth])
        .add_rows(
            repos
                .into_iter()
                .map(|r| [cell(&r.id), cell(&r.spec.kind), cell(&r.spec.ident)]),
        );
    println!("{table}");
}

pub fn event_tree<'a>(events: impl IntoIterator<Item = &'a BuildEvent<'a>>) {
    #[derive(Debug)]
    enum TimelineEvent<'a> {
        Task(&'a BuildTask<'a>),
        Event(&'a BuildEvent<'a>),
    }
    let mut timelines = BTreeMap::<Uuid, Vec<TimelineEvent<'a>>>::new();
    for event in events {
        match &event.event {
            BuildEventKind::Task(BuildTaskEvent { task_id, event: e }) => {
                if let BuildTaskEventKind::Started(task) = e {
                    timelines
                        .entry(task.parent_id.unwrap_or_default())
                        .or_default()
                        .push(TimelineEvent::Task(task));
                }
                timelines
                    .entry(*task_id)
                    .or_default()
                    .push(TimelineEvent::Event(event));
            }
            _ => timelines
                .entry(Uuid::nil())
                .or_default()
                .push(TimelineEvent::Event(event)),
        }
    }

    fn log_log(w: &mut impl Write, log: &events::Log) -> std::io::Result<()> {
        let events::Log {
            level,
            kind: _,
            file: _,
            column: _,
            line: _,
            text,
        } = log;
        let (level, style) = match level {
            BuildTaskLevel::Error => ("ERROR ", Style::new().red().bold()),
            BuildTaskLevel::Warn => ("WARN  ", Style::new().yellow().bold()),
            BuildTaskLevel::Notice => ("NOTICE", Style::new().bold()),
            BuildTaskLevel::Info => ("INFO  ", Style::new()),
            BuildTaskLevel::Talkative => ("TALK  ", Style::new().dimmed().cyan()),
            BuildTaskLevel::Chatty => ("CHAT  ", Style::new().dimmed().blue()),
            BuildTaskLevel::Debug => ("DEBUG ", Style::new().dimmed()),
            BuildTaskLevel::Vomit => ("VOMIT ", Style::new().dimmed().italic()),
        };
        let level = level.if_supports_color(Stream::Stdout, |v| v.style(style));
        let text = text.strip_suffix('\n').unwrap_or(text);
        write!(w, " {level} {text}")
    }
    fn walk<'a>(
        w: &mut impl Write,
        timelines: &BTreeMap<Uuid, Vec<TimelineEvent<'a>>>,
        task: Option<&'a BuildTask<'a>>,
        depth: usize,
    ) -> std::io::Result<()> {
        let task_id = |task: &Option<&BuildTask>| task.map(|t| t.id).unwrap_or_default();
        let entries: &[TimelineEvent] = timelines
            .get(&task_id(&task))
            .map(|v| v.as_slice())
            .unwrap_or_default();

        for entry in entries {
            match entry {
                TimelineEvent::Task(task) => walk(w, timelines, Some(task), depth + 1)?,
                TimelineEvent::Event(BuildEvent {
                    timestamp, event, ..
                }) => {
                    let ts = timestamp.to_rfc3339_opts(SecondsFormat::Secs, true);
                    let ts = ts.if_supports_color(Stream::Stdout, |v| v.dimmed());
                    write!(w, "{ts} ")?;
                    for _ in 0..depth.checked_sub(1).unwrap_or(depth) {
                        write!(w, "│")?;
                    }

                    // Nix sometimes logs by starting and immediately ending a task.
                    if entries.iter().all(|e| {
                        matches!(
                            e,
                            TimelineEvent::Event(BuildEvent {
                                event: BuildEventKind::Task(BuildTaskEvent {
                                    event: BuildTaskEventKind::Started(_)
                                        | BuildTaskEventKind::Stopped,
                                    ..
                                }),
                                ..
                            })
                        )
                    }) {
                        if let Some(task) = task {
                            let style = match task.level {
                                BuildTaskLevel::Error
                                | BuildTaskLevel::Warn
                                | BuildTaskLevel::Notice
                                | BuildTaskLevel::Info => Style::new(),
                                BuildTaskLevel::Talkative
                                | BuildTaskLevel::Chatty
                                | BuildTaskLevel::Debug
                                | BuildTaskLevel::Vomit => Style::new().dimmed(),
                            };
                            write!(w, "├─╴")?;
                            if task.kind.is_some() {
                                write!(w, "{}╶╴", task_icon(task))?;
                            }
                            let name = task_name(task);
                            let name = name.if_supports_color(Stream::Stdout, |v| v.style(style));
                            writeln!(w, "{name}",)?;
                        }
                        return Ok(());
                    }

                    match event {
                        BuildEventKind::Started => {
                            write!(w, "┌╴🏗️ Build Started")?;
                        }
                        BuildEventKind::Log(log) => {
                            write!(w, "│ ")?;
                            log_log(w, log)?;
                        }
                        BuildEventKind::Stopped(result) => {
                            write!(w, "└╴")?;
                            match result {
                                models::BuildResult::Unknown => write!(w, "⁉️ Unknown")?,
                                models::BuildResult::Success => write!(w, "✅ Build Succeeded")?,
                                models::BuildResult::Failure => write!(w, "❌ Build Failed")?,
                            }
                        }
                        BuildEventKind::Task(BuildTaskEvent { event, .. }) => match event {
                            BuildTaskEventKind::Started(task) => {
                                write!(w, "├┬╴{}╶╴{}", task_icon(task), task_name(task))?;
                            }
                            BuildTaskEventKind::Log(log) => {
                                write!(w, "││ ")?;
                                log_log(w, log)?;
                            }
                            BuildTaskEventKind::Updated(BuildTaskUpdate::FileLinked { bytes }) => {
                                let size = ByteSize::b(*bytes as _);
                                write!(w, "│├╴File Linked: {size}")?;
                            }
                            BuildTaskEventKind::Updated(BuildTaskUpdate::UntrustedPath) => {
                                write!(w, "│├╴Untrusted Path")?;
                            }
                            BuildTaskEventKind::Updated(BuildTaskUpdate::CorruptedPath) => {
                                write!(w, "│├╴Corrupted Path")?;
                            }
                            BuildTaskEventKind::Updated(BuildTaskUpdate::SetPhase { phase }) => {
                                write!(w, "│├─╴{phase}")?;
                            }
                            BuildTaskEventKind::Updated(BuildTaskUpdate::Progress {
                                done,
                                expected: exp,
                                running: run,
                                failed: fail,
                            }) => {
                                const WIDTH: usize = 20;
                                let frac = |v| match v {
                                    0 => 0,
                                    v => max(1, ((v as f32 / *exp as f32) * WIDTH as f32) as _),
                                };
                                let (done, done_s) = (frac(*done), Style::new().green());
                                let (run, run_s) = (frac(*run), Style::new().green().dimmed());
                                let (fail, fail_s) = (frac(*fail), Style::new().red());
                                let (rest, rest_s) =
                                    (WIDTH - done - run - fail, Style::new().dimmed());

                                write!(w, "││ ")?;
                                for (num, style, chr) in [
                                    (done, done_s, "█"),
                                    (run, run_s, "▒"),
                                    (fail, fail_s, "╳"),
                                    (rest, rest_s, "░"),
                                ] {
                                    if num > 0 {
                                        write!(w, "{}", style.prefix_formatter())?;
                                        for _ in 0..num {
                                            write!(w, "{chr}")?;
                                        }
                                        write!(w, "{}", style.suffix_formatter())?;
                                    }
                                }
                                write!(w, " {done}/{exp}")?;
                                if run > 0 {
                                    write!(w, "{}", format_args!(" {run} running").style(run_s))?;
                                }
                                if fail > 0 {
                                    write!(w, "{}", format_args!(" {fail} failed").style(fail_s))?;
                                }
                            }
                            BuildTaskEventKind::Stopped => {
                                write!(w, "├┘")?;
                            }
                        },
                    };
                    writeln!(w)?;
                }
            }
        }
        Ok(())
    }
    walk(&mut std::io::stdout(), &timelines, None, 0).unwrap();
}

fn task_name<'a>(task: &'a BuildTask<'a>) -> &'a str {
    match task.name.as_ref() {
        "" => match task.kind {
            None => "???",
            Some(kind) => match kind {
                models::BuildTaskKind::Helper => "Invoking Helper...",
                models::BuildTaskKind::Realise => "Realising...",
                models::BuildTaskKind::Builds => "Running builds...",
                models::BuildTaskKind::Build => "Building...",
                models::BuildTaskKind::PostBuildHook => "Post-Build Hook...",
                models::BuildTaskKind::BuildWaiting => "Build Waiting...",
                models::BuildTaskKind::Substitute => "Substituting...",
                models::BuildTaskKind::CopyPaths => "Copying paths...",
                models::BuildTaskKind::CopyPath => "Copying path...",
                models::BuildTaskKind::FileTransfer => "Transferring file...",
                models::BuildTaskKind::OptimiseStore => "Optimising store...",
                models::BuildTaskKind::VerifyPaths => "Verifying paths...",
                models::BuildTaskKind::QueryPathInfo => "Querying path info...",
            },
        },
        name => name,
    }
}
fn task_icon(task: &BuildTask) -> &'static str {
    match task.kind {
        None => "❔",
        Some(kind) => match kind {
            models::BuildTaskKind::Helper => "📞",
            models::BuildTaskKind::Realise => "🐶",
            models::BuildTaskKind::Builds => "🛠️",
            models::BuildTaskKind::Build => "🔨",
            models::BuildTaskKind::PostBuildHook => "🪝",
            models::BuildTaskKind::BuildWaiting => "💤",
            models::BuildTaskKind::Substitute => "📦",
            models::BuildTaskKind::CopyPaths => "📡",
            models::BuildTaskKind::CopyPath => "🛰️",
            models::BuildTaskKind::FileTransfer => "📂",
            models::BuildTaskKind::OptimiseStore => "🧹",
            models::BuildTaskKind::VerifyPaths => "🔍",
            models::BuildTaskKind::QueryPathInfo => "🔮",
        },
    }
}
