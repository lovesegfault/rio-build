// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::{
    borrow::Cow,
    cell::LazyCell,
    ffi::OsStr,
    fmt,
    io::BufRead,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread::JoinHandle,
};

use derive_more::{Deref, DerefMut, From};
use gorgond_client::{
    events::{BuildEvent, BuildEventKind, BuildTaskEvent, BuildTaskEventKind},
    models::{BuildTask, BuildTaskKind},
    Uuid,
};
use miette::{NamedSource, SourceOffset};
use owo_colors::OwoColorize;
use tracing::{debug, enabled, error, info, info_span, trace, warn};

use crate::{event_handler, Event, SerializedError};

pub type Result<T, E = HelperError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum HelperError {
    #[error(fmt = Self::fmt_find_binary)]
    FindBinary(String, Vec<String>, #[source] which::Error),

    #[error("Failed to start helper process")]
    Start(#[source] std::io::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Exit(HelperExitError),
}

impl HelperError {
    fn fmt_find_binary(
        name: &String,
        paths: &Vec<String>,
        _: &which::Error,
        f: &mut fmt::Formatter,
    ) -> fmt::Result {
        write!(f, "Looking for `{name}`, in:")?;
        for path in paths {
            write!(f, "\n  - {path}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub struct HelperExitError {
    pub name: String,
    pub status: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

impl fmt::Display for HelperExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = &self.name;
        match (self.stdout.as_ref(), self.stderr.as_ref()) {
            (Some(s), None) | (None, Some(s)) if !s.contains("\n") => {
                write!(f, "{name}: {s}")?;
            }
            (Some(s), None) => write!(f, "STDOUT:\n\n{s}\n\n")?,
            (None, Some(s)) => write!(f, "STDERR:\n\n{s}\n\n")?,
            (Some(s1), Some(s2)) => {
                write!(f, "STDOUT:\n\n{s1}\n\n")?;
                write!(f, "STDERR:\n\n{s2}\n\n")?;
            }
            (None, None) => match self.status {
                Some(v) => write!(f, "{name} exited with status code: {v}")?,
                None => write!(f, "{name} exited with unknown status")?,
            },
        };
        Ok(())
    }
}

impl miette::Diagnostic for HelperExitError {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.status
            .map(|status| Box::new(format!("{} failed with code: {status}", self.name)) as _)
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum TaskError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Helper(#[from] HelperError),
    #[error("Couldn't deserialize task error")]
    #[diagnostic(transparent)]
    ErrorJson(#[from] JsonError),
    #[error("Couldn't process stderr")]
    #[diagnostic(transparent)]
    Stderr(#[from] StderrError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Task(#[from] TaskExitError),
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("Task failed: `{}`", .1.name)]
pub struct TaskExitError(#[related] Vec<TaskExitErrorCause>, BuildTask<'static>);

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("Error from task: `{}`", .1)]
#[diagnostic(forward(0))]
struct TaskExitErrorCause(#[source] SerializedError, Cow<'static, str>);

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum StderrError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Couldn't deserialize event")]
    #[diagnostic(transparent)]
    EventJson(#[from] JsonError),
    #[error("Couldn't send deserialized error back to main thread")]
    SendError(#[from] crossbeam::channel::TrySendError<SerializedError>),
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("{err}")]
pub struct JsonError {
    pub err: serde_json::Error,
    #[source_code]
    pub src: NamedSource<String>,
    #[label("{err}")]
    pub pos: SourceOffset,
}

impl JsonError {
    pub fn new(src: NamedSource<String>, err: serde_json::Error) -> Self {
        Self {
            pos: SourceOffset::from_location(src.inner(), err.line(), err.column()),
            src,
            err,
        }
    }
}

pub trait HelperContext: Clone {
    fn apply(&self, helper: Helper) -> Helper;
}
impl HelperContext for () {
    fn apply(&self, helper: Helper) -> Helper {
        helper
    }
}

pub trait HelperCaller: Sized + Clone {
    fn helper(&self, name: impl AsRef<OsStr>) -> Result<Helper>;
    fn with<Ctx: HelperContext>(self, ctx: Ctx) -> impl HelperCaller {
        HelperCallerWith(ctx, self)
    }
}

#[derive(Debug, Clone)]
pub struct HelperFinder<'a> {
    pub current_dir: Cow<'a, Path>,
    pub path: Cow<'a, OsStr>,
    pub dev: bool,
    pub dev_repo: Option<Cow<'a, Path>>,
}

impl<'a> HelperFinder<'a> {
    pub fn new(current_dir: Cow<'a, Path>, path: Cow<'a, OsStr>) -> Self {
        Self {
            current_dir,
            path,
            dev: false,
            dev_repo: None,
        }
    }
    pub fn with_dev(&mut self, dev_repo: Option<Cow<'a, Path>>) -> &mut Self {
        self.dev = true;
        self.dev_repo = dev_repo;
        self
    }
}
impl HelperCaller for HelperFinder<'_> {
    fn helper(&self, name: impl AsRef<OsStr>) -> Result<Helper> {
        if self.dev {
            let repo_path = self.dev_repo.as_ref().unwrap_or(&self.current_dir);
            let src_path = repo_path.join(Path::new(&name));
            match src_path.try_exists() {
                Ok(true) => {
                    info!(
                        name = &*name.as_ref().to_string_lossy(),
                        "🚧 Building helper from source..."
                    );
                    return Helper::build_dev(&name, repo_path, &self.current_dir);
                }
                Ok(false) => trace!(?src_path, "Path does not exist"),
                Err(err) => {
                    debug!(?src_path, %err, "Couldn't check if path exists")
                }
            }
        }

        Helper::find(name, &self.path, &self.current_dir)
    }
}

#[derive(Debug, Clone)]
pub struct HelperCallerWith<Ctx: HelperContext, C: HelperCaller>(Ctx, C);

impl<Ctx: HelperContext, C: HelperCaller> HelperCaller for HelperCallerWith<Ctx, C> {
    fn helper(&self, name: impl AsRef<OsStr>) -> Result<Helper> {
        self.1.helper(name).map(|h| h.with(&self.0))
    }
}

#[derive(Debug, Deref, DerefMut, From)]
pub struct Helper(Command);

#[derive(Debug)]
pub struct HelperChild<'a> {
    pub helper: &'a Helper,
    pub child: Child,
}

impl fmt::Display for Helper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (key, value) in self.get_envs() {
            let key = key.to_string_lossy();
            let value = value.map(|s| s.to_string_lossy()).unwrap_or_default();
            writeln!(f, "{key}={value}")?;
        }
        let cwd = self
            .get_current_dir()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default();
        writeln!(f, "[{cwd}]")?;
        write!(f, "$ {}", self.get_program().to_string_lossy())?;
        for arg in self.get_args() {
            write!(f, " {}", arg.to_string_lossy())?;
        }
        Ok(())
    }
}

impl Helper {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self::from(Command::new(program))
    }

    pub fn name(&self) -> Cow<'_, str> {
        let path = Path::new(self.get_program());
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
    }

    pub fn run(mut self) -> Result<Output> {
        self.start().and_then(|child| child.wait_checked())
    }

    pub fn start(&mut self) -> Result<HelperChild> {
        if enabled!(tracing::Level::DEBUG) {
            debug!(%self, "Invoking helper...");
        }
        self.spawn()
            .map(|child| HelperChild {
                helper: self,
                child,
            })
            .map_err(HelperError::Start)
    }

    pub fn build_dev(
        name: impl AsRef<OsStr>,
        repo_path: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::from(Command::new("cargo"))
            .with_args(["build", "-p"])
            .with_arg(name.as_ref())
            .with_cwd(repo_path.as_ref())
            .run()?;
        Self::find(name, repo_path.as_ref().join("target/debug/"), cwd)
    }

    pub fn find(
        name: impl AsRef<OsStr>,
        paths: impl AsRef<OsStr>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self> {
        Ok(Self::from(Command::new(
            which::which_in(name.as_ref(), Some(paths.as_ref()), cwd).map_err(|err| {
                HelperError::FindBinary(
                    name.as_ref().to_string_lossy().into(),
                    std::env::split_paths(&paths)
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    err,
                )
            })?,
        )))
    }

    pub fn with<C: HelperContext>(self, ctx: &C) -> Self {
        ctx.apply(self)
    }
    pub fn with_env(mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> Self {
        self.env(key, val);
        self
    }
    pub fn with_arg(mut self, p: impl AsRef<OsStr>) -> Self {
        self.arg(p);
        self
    }
    pub fn with_arg_opt(mut self, p: Option<impl AsRef<OsStr>>) -> Self {
        if let Some(p) = p {
            self.arg(p);
        }
        self
    }
    pub fn with_args(mut self, it: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.args(it);
        self
    }
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.current_dir(cwd);
        self
    }
    pub fn with_stdin(mut self, stdin: impl Into<std::process::Stdio>) -> Self {
        self.stdin(stdin);
        self
    }

    pub fn into_task(self, task: Option<BuildTask<'static>>) -> TaskHelper {
        let task = task.unwrap_or_else(|| BuildTask {
            id: Uuid::now_v7(),
            name: self.name().into_owned().into(),
            kind: Some(BuildTaskKind::Helper),
            ..Default::default()
        });
        TaskHelper {
            span: info_span!("task", name = &*task.name),
            helper: self,
            task,
        }
    }
}

impl HelperChild<'_> {
    pub fn wait_checked(self) -> Result<Output> {
        let output = self.child.wait_with_output().map_err(HelperError::Start)?;
        match output.status {
            status if !status.success() => Err(HelperError::Exit(HelperExitError {
                name: self.helper.name().to_string(),
                status: status.code(),
                stdout: Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
                    .filter(|s| !s.is_empty()),
                stderr: Some(String::from_utf8_lossy(&output.stderr).trim().to_string())
                    .filter(|s| !s.is_empty()),
            })),
            _ => Ok(output),
        }
    }
}

#[derive(Debug)]
pub struct TaskHelper {
    task: BuildTask<'static>,
    helper: Helper,
    span: tracing::Span,
}

#[derive(Debug)]
pub struct TaskChild<'a> {
    pub helper: &'a TaskHelper,
    pub child: Child,
    join_stdout: JoinHandle<Result<(), StderrError>>,
    recv_err: crossbeam::channel::Receiver<SerializedError>,
    _stop_on_drop: StopTaskOnDrop,
}

impl TaskHelper {
    pub fn start(&mut self, level: tracing::Level) -> Result<TaskChild, TaskError> {
        let _enter = self.span.enter();
        event_handler().handle(Event::Build(
            BuildTaskEvent {
                task_id: self.task.id,
                event: BuildTaskEventKind::Started(self.task.borrowed()),
            }
            .into(),
        ));

        self.helper
            .env(
                "GORGON_VERBOSE",
                match level {
                    tracing::Level::DEBUG => "1",
                    tracing::Level::TRACE => "2",
                    _ => "0",
                },
            )
            .env(
                "GORGON_QUIET",
                match level {
                    tracing::Level::WARN => "1",
                    tracing::Level::ERROR => "2",
                    _ => "0",
                },
            )
            .env("GORGON_EVENTS", "true")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let HelperChild {
            helper: _,
            mut child,
        } = self.helper.start()?;

        let (send_err, recv_err) = crossbeam::channel::unbounded::<SerializedError>();
        let join_stdout = std::thread::spawn({
            let span = self.span.clone();
            let task_id = self.task.id;
            let mut stdout = std::io::BufReader::new(child.stdout.take().expect("No child stdout"));
            move || -> Result<(), StderrError> {
                let _enter = span.entered();
                let _enter = info_span!("stdout").entered();
                let handler = event_handler();
                let mut buf = String::new();
                loop {
                    stdout.read_line(&mut buf)?;
                    if buf.is_empty() {
                        return Ok(());
                    }
                    if buf.starts_with('{') {
                        let mut event: Event = match serde_json::from_str(&buf) {
                            Err(err) if err.is_eof() => {
                                // Keep reading lines until we've got a complete JSON payload.
                                continue;
                            }
                            Err(err) => Err(StderrError::from(JsonError::new(
                                NamedSource::new("stdout", buf.clone()).with_language("JSON"),
                                err,
                            ))),
                            Ok(event) => Ok(event),
                        }?;

                        // Assign top-level child tasks the correct parent.
                        if let Event::Build(BuildEvent {
                            event:
                                BuildEventKind::Task(BuildTaskEvent {
                                    event:
                                        BuildTaskEventKind::Started(BuildTask {
                                            ref mut parent_id, ..
                                        }),
                                    ..
                                }),
                            ..
                        }) = &mut event
                        {
                            parent_id.get_or_insert(task_id);
                        }

                        // Send errors back through the error channel.
                        if let Event::Error(err) = &event {
                            send_err.try_send(err.clone())?;
                        }

                        handler.handle(event);
                    } else {
                        warn!("{}", buf.trim_end());
                    }

                    // Now we can clear the buffer.
                    buf.clear();
                }
            }
        });
        Ok(TaskChild {
            helper: self,
            child,
            join_stdout,
            recv_err,
            _stop_on_drop: StopTaskOnDrop {
                task_id: self.task.id,
            },
        })
    }
}

impl TaskChild<'_> {
    pub fn wait_checked(self) -> Result<Output, TaskError> {
        let _enter = self.helper.span.enter();
        let output = self.child.wait_with_output().map_err(HelperError::Start)?;
        self.join_stdout.join().expect("stdout thread panicked!?")?;
        match output.status {
            status if !status.success() => {
                let stdout_str = LazyCell::new(|| String::from_utf8_lossy(&output.stdout));

                // If debug logging is enabled, log stdout.
                if enabled!(tracing::Level::DEBUG) {
                    let termwidth = terminal_size::terminal_size_of(std::io::stdout())
                        .map(|(w, _)| w.0 as usize)
                        .unwrap_or(80);
                    eprintln!(
                        "{}",
                        format!(
                            "{:-^termwidth$}\n{}{:-^termwidth$}",
                            " DEBUG --- failed task stdout --- DEBUG ",
                            stdout_str.as_ref(),
                            ""
                        )
                        .dimmed()
                    );
                }

                // Try to parse the forwarded error from the child.
                Err(TaskError::Task(TaskExitError(
                    self.recv_err
                        .into_iter()
                        .map(|r| TaskExitErrorCause(r, self.helper.task.name.clone()))
                        .collect(),
                    self.helper.task.clone(),
                )))
            }
            _ => {
                if let Some(err) = self.recv_err.into_iter().next() {
                    unimplemented!("non-fatal error from child: {err}");
                }
                Ok(output)
            }
        }
    }
}

/// Dropping this value will emit a "task stopped" event. TaskChild holds an instance.
/// (We're not implementing Drop directly on TaskChild, as then we can't move out of it.)
#[derive(Debug)]
pub struct StopTaskOnDrop {
    task_id: Uuid,
}
impl Drop for StopTaskOnDrop {
    fn drop(&mut self) {
        event_handler().handle(Event::Build(
            BuildTaskEvent {
                task_id: self.task_id,
                event: BuildTaskEventKind::Stopped,
            }
            .into(),
        ))
    }
}
