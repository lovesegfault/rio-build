// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use clap::Parser;
use colored_json::write_colored_json_with_mode;
use gorgon_build_helper::{
    helper::{Helper, HelperCaller, HelperContext, HelperFinder},
    Event, EventHandler, SerializedError,
};
use gorgond_client::{
    events::{
        BuildEvent, BuildEventKind, BuildTaskEvent, BuildTaskEventKind, BuildTaskUpdate, Log,
    },
    models::{BuildTask, BuildTaskLevel},
    Uuid,
};
use miette::{miette, Context, IntoDiagnostic, Result};
use parking_lot::RwLock;
use strum::{AsRefStr, Display, EnumString, IntoStaticStr};
use tracing::{debug, error, info, info_span, trace, warn, Span};

#[derive(Clone)]
struct EventLoggerTask {
    span: Span,
}

pub struct EventLogger {
    span: Span,
    tasks: RwLock<BTreeMap<Uuid, EventLoggerTask>>,
}
impl Default for EventLogger {
    fn default() -> Self {
        Self {
            span: info_span!("event"),
            tasks: Default::default(),
        }
    }
}
impl EventLogger {
    fn log_log(
        &self,
        Log {
            level,
            kind,
            file: _,
            column: _,
            line: _,
            text,
        }: Log,
    ) {
        match level {
            BuildTaskLevel::Error => error!(%kind, "{text}"),
            BuildTaskLevel::Warn => warn!(%kind, "{text}"),
            BuildTaskLevel::Notice | BuildTaskLevel::Info => info!("{text}",),
            BuildTaskLevel::Talkative | BuildTaskLevel::Chatty | BuildTaskLevel::Debug => {
                debug!(%kind, "{text}")
            }
            BuildTaskLevel::Vomit => trace!(%kind, "{text}"),
        }
    }
}
impl EventHandler for EventLogger {
    fn handle(&self, event: Event<'_>) {
        let _enter = self.span.enter();
        match event {
            Event::Error(err) => {
                error!("{:?}", miette::Report::new(err))
            }
            Event::Build(BuildEvent { event, .. }) => {
                let build_span = info_span!("build");
                let _enter = build_span.enter();
                match event {
                    BuildEventKind::Started => info!("Build Started"),
                    BuildEventKind::Stopped(result) => info!(%result, "Build Stopped"),
                    BuildEventKind::Log(log) => self.log_log(log),
                    BuildEventKind::Task(BuildTaskEvent { task_id, event }) => {
                        if let BuildTaskEventKind::Started(BuildTask {
                            level,
                            parent_id,
                            ref name,
                            ..
                        }) = event
                        {
                            let mut tasks = self.tasks.write();
                            let task = EventLoggerTask {
                                span: parent_id
                                    .and_then(|id| tasks.get(&id).map(|e| &e.span))
                                    .unwrap_or(&build_span)
                                    .in_scope(|| match level {
                                        BuildTaskLevel::Error => {
                                            tracing::error_span!("task", %name, %level)
                                        }
                                        BuildTaskLevel::Warn => {
                                            tracing::warn_span!("task", %name, %level)
                                        }
                                        BuildTaskLevel::Notice | BuildTaskLevel::Info => {
                                            tracing::info_span!("task", %name, %level)
                                        }
                                        BuildTaskLevel::Talkative
                                        | BuildTaskLevel::Chatty
                                        | BuildTaskLevel::Debug
                                        | BuildTaskLevel::Vomit => {
                                            tracing::debug_span!("task", %name, %level)
                                        }
                                    }),
                            };
                            tasks.insert(task_id, task);
                        } else if let BuildTaskEventKind::Stopped = event {
                            self.tasks.write().remove(&task_id);
                            return;
                        };
                        let tasks = self.tasks.read();
                        let Some(task) = tasks.get(&task_id) else {
                            error!(%task_id, "Event received for unknown task!");
                            return;
                        };
                        let _enter = task.span.enter();

                        match event {
                            BuildTaskEventKind::Started(..) | BuildTaskEventKind::Stopped => {}
                            BuildTaskEventKind::Updated(update) => match update {
                                BuildTaskUpdate::FileLinked { bytes } => {
                                    info!(bytes, "  -> File Linked")
                                }
                                BuildTaskUpdate::UntrustedPath => {
                                    warn!("  -> Untrusted Path")
                                }
                                BuildTaskUpdate::CorruptedPath => {
                                    warn!("  -> Corrupted Path")
                                }
                                BuildTaskUpdate::SetPhase { phase } => {
                                    info!(phase = &*phase, "  -> Phase Change")
                                }
                                BuildTaskUpdate::Progress {
                                    done,
                                    expected,
                                    running,
                                    failed,
                                } => {
                                    trace!(done, expected, running, failed, "  -> Progress")
                                }
                            },
                            BuildTaskEventKind::Log(log) => self.log_log(log),
                        };
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct EventSerializer;
impl EventHandler for EventSerializer {
    fn handle(&self, event: Event) {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &event).expect("Couldn't serialize event!?");
        let _ = writeln!(&mut stdout);
    }
}

#[derive(
    Debug,
    Display,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    EnumString,
    AsRefStr,
    IntoStaticStr,
    clap::ValueEnum,
)]
#[strum(serialize_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Default,
    Json,
}

#[derive(Debug, Clone, Copy, Parser)]
#[group(id = "common")]
#[command(next_help_heading = "Logging")]
pub struct Args {
    /// Increase log level.
    #[arg(long, short, env = "GORGON_VERBOSE", action=clap::ArgAction::Count)]
    pub verbose: u8,

    /// Decrease log level.
    #[arg(long, short, env = "GORGON_QUIET", action=clap::ArgAction::Count)]
    pub quiet: u8,

    /// Write events to stdout.
    #[arg(long, env = "GORGON_EVENTS", action=clap::ArgAction::SetTrue)]
    pub events: bool,
}

impl Args {
    pub fn level(&self) -> tracing::Level {
        match self.verbose as i8 - self.quiet as i8 {
            ..=-2 => tracing::Level::ERROR,
            -1 => tracing::Level::WARN,
            0 => tracing::Level::INFO,
            1 => tracing::Level::DEBUG,
            2.. => tracing::Level::TRACE,
        }
    }
}

impl HelperContext for &Args {
    fn apply(&self, mut helper: Helper) -> Helper {
        if self.verbose > 0 {
            helper = helper.with_env("GORGON_VERBOSE", self.verbose.to_string());
        }
        if self.quiet > 0 {
            helper = helper.with_env("GORGON_QUIET", self.quiet.to_string());
        }
        if self.events {
            helper = helper.with_env("GORGON_EVENTS", "true");
        }
        helper
    }
}

#[derive(Debug, Clone, Parser)]
#[group(id = "helpers")]
#[command(next_help_heading = "Helpers")]
pub struct HelperArgs {
    #[cfg(not(windows))]
    /// Path(s) searched for helpers. Default: $PATH
    #[arg(long, env = "GORGON_HELPER_PATH", value_delimiter = ':')]
    helper_path: Vec<PathBuf>,

    #[cfg(windows)]
    /// Path(s) searched for helpers. Default: $PATH
    #[arg(long, env = "GORGON_HELPER_PATH", value_delimiter = ';')]
    helper_path: Vec<PathBuf>,

    /// Run helpers from source. Requires `cargo`.
    #[arg(long, env = "GORGON_HELPER_DEV")]
    helper_dev: bool,

    /// With --helper-dev, build from this repository.
    #[arg(long, env = "GORGON_HELPER_DEV_REPO")]
    helper_dev_repo: Option<PathBuf>,
}

impl HelperContext for HelperArgs {
    fn apply(&self, mut helper: Helper) -> Helper {
        if !self.helper_path.is_empty() {
            helper = helper.with_env(
                "GORGON_HELPER_PATH",
                std::env::join_paths(&self.helper_path).expect("Couldn't join GORGON_HELPER_PATH"),
            );
        }
        if self.helper_dev {
            helper = helper.with_env("GORGON_HELPER_DEV", "true");
        }
        if let Some(path) = self.helper_dev_repo.as_deref() {
            helper = helper.with_env("GORGON_HELPER_DEV_REPO", path);
        }
        helper
    }
}

impl HelperArgs {
    pub fn caller(&self) -> Result<impl HelperCaller + use<'_>> {
        let current_dir = std::env::current_dir()
            .into_diagnostic()
            .context("Couldn't get current working directory")?;

        let path = match &self.helper_path[..] {
            [] => Cow::Owned(
                std::env::var_os("PATH")
                    .ok_or_else(|| miette!("Neither $PATH nor --search-path is set"))?,
            ),
            [path] => Cow::Borrowed(path.as_ref()),
            paths => Cow::Owned(
                std::env::join_paths(paths)
                    .into_diagnostic()
                    .context("Couldn't join --search-path")?,
            ),
        };

        let mut caller = HelperFinder::new(current_dir.into(), path);
        if self.helper_dev {
            caller.with_dev(
                self.helper_dev_repo
                    .as_ref()
                    .map(|p| Cow::Borrowed(p.as_ref())),
            );
        }
        Ok(caller)
    }
}

#[must_use]
pub struct LoggerBuilder {
    level: tracing::Level,
    target_levels: BTreeMap<Cow<'static, str>, tracing::Level>,
}

impl LoggerBuilder {
    pub fn with_target_level(
        mut self,
        target: impl Into<Cow<'static, str>>,
        level: tracing::Level,
    ) -> Self {
        self.target_levels.insert(target.into(), level);
        self
    }

    pub fn init(self) {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::Registry::default()
            .with(
                tracing_subscriber::filter::Targets::new()
                    .with_targets(self.target_levels)
                    .with_default(self.level),
            )
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}

pub fn logger(args: Args) -> LoggerBuilder {
    LoggerBuilder {
        level: args.level(),
        target_levels: BTreeMap::new(),
    }
}

pub fn main<E: Into<miette::Report>, F: FnOnce() -> Result<(), E>>(args: Args, f: F) {
    static MAIN_CALLED: AtomicBool = AtomicBool::new(false);
    assert!(
        !MAIN_CALLED.swap(true, Ordering::SeqCst),
        "Please don't call main() more than once."
    );

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .with_syntax_highlighting(miette::highlighters::SyntectHighlighter::default())
                .context_lines(6)
                .build(),
        )
    }))
    .expect("Couldn't install miette hook");

    if !args.events {
        // SAFETY: See MAIN_CALLED.
        unsafe {
            gorgon_build_helper::set_event_handler(Box::leak(Box::new(EventLogger::default())))
        };
    } else {
        // SAFETY: See MAIN_CALLED.
        unsafe { gorgon_build_helper::set_event_handler(Box::leak(Box::new(EventSerializer))) };
    }

    if let Err(err) = f().map_err(|err| err.into()) {
        if !args.events {
            eprintln!("\n{err:?}");
        } else {
            write_colored_json_with_mode(
                &Event::Error(SerializedError::new(
                    AsRef::<dyn miette::Diagnostic>::as_ref(&err),
                )),
                &mut std::io::stdout(),
                colored_json::ColorMode::Auto(colored_json::Output::StdOut),
            )
            .expect("Couldn't serialize error");
            println!();
        }

        if err.severity().unwrap_or(miette::Severity::Error) >= miette::Severity::Error {
            std::process::exit(1);
        } else {
            std::process::exit(0);
        }
    }
}
