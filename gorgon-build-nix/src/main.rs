// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use chrono::prelude::*;
use clap::Parser;
use clap_stdin::MaybeStdin;
use derive_more::{Deref, DerefMut};
use gorgon_build_helper::{event_handler, helper::Helper, Event};
use gorgond_client::{
    events::BuildTaskUpdate,
    models::{Build, BuildTask, BuildTaskKind},
};
use gorgond_client::{
    events::{BuildEvent, BuildEventKind, BuildTaskEvent, BuildTaskEventKind, Log, LogKind},
    models::BuildTaskLevel,
    Uuid,
};
use miette::{miette, Context, IntoDiagnostic, Result};
use nix_daemon::{
    nix::internal_json::{
        Action, ActionMessage, ActionResult, ActionResultData, ActionStart, ActionStop,
        BuildLogLine, FileLinked, Progress, SetExpected, SetPhase,
    },
    StderrActivityType, Verbosity,
};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    ops::Deref,
    path::{Path, PathBuf},
    process::Stdio,
};
use tracing::{info, trace, warn};
use url::{Position, Url};

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Path to `nix-instantiate`.
    #[arg(long, short = 'I', default_value = "nix-instantiate")]
    nix_instantiate: PathBuf,

    /// Path to `nix-store`.
    #[arg(long, short = 'S', default_value = "nix-store")]
    nix_store: PathBuf,

    /// Build spec in JSON format, or "-" to read from stdin.
    build_json: MaybeStdin<String>,

    #[command(flatten)]
    common: gorgon_util_cli::Args,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum FetchExprError {
    #[error("{1}")]
    #[diagnostic(code = "FetchExpr::url::parse")]
    ParseUrl(String, #[source] url::ParseError),
    #[error("{1}")]
    #[diagnostic(forward(1))]
    Url(Url, #[source] FetchExprUrlError),
}
#[derive(Debug, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum FetchExprUrlError {
    #[error("Unsupported fetcher: {0}")]
    #[diagnostic(
        code = "FetchExpr::url::scheme",
        help = "Fetchers are specified by the URL scheme, eg. 'git://' or 'git+http://'.\nSupported fetchers: 'git'."
    )]
    Fetcher(String),
    #[error("Unknown query parameter: ?{0}")]
    #[diagnostic(
        code = "FetchExpr::url::query::unknown",
        help = "Valid query parameters are: ?rev"
    )]
    UnknownQueryParameter(String),
    #[error("Missing required query parameter: ?{0}")]
    #[diagnostic(
        code = "FetchExpr::url::query::required",
        help = "Required query parameters are: ?rev"
    )]
    MissingQueryParameter(&'static str),
}

/// A nix expression for a fetcher, eg. `builtins.fetchGit`.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchExpr {
    FetchGit { url: String, rev: String },
}
impl std::fmt::Display for FetchExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FetchGit { url, rev } => write!(
                f,
                r#"builtins.fetchGit {{ url = "{url}"; rev = "{rev}"; }}"#,
            ),
        }
    }
}
impl FetchExpr {
    /// Parses a Fetcher URL, eg. `git+https://example.com/?rev=abc123`.
    pub fn parse_url(url_s: &str) -> Result<Self, FetchExprError> {
        let url = Url::parse(url_s).map_err(|err| FetchExprError::ParseUrl(url_s.into(), err))?;
        let url_err = |err| FetchExprError::Url(url.clone(), err);

        // The URL crate won't let us do `set_scheme()` involving "special" schemes like
        // https://, which... is unfortunate for doing git+https -> https, so if we need
        // to do that, we need to rebuild the whole string ourselves.
        // https://github.com/servo/rust-url/issues/577
        let scheme_ = url.scheme();
        let (fetcher, scheme) = scheme_.split_once("+").unwrap_or((scheme_, scheme_));
        let url_s = [scheme, &url[Position::AfterScheme..Position::AfterPath]].join("");

        match fetcher {
            "git" => {
                let mut rev = None;
                for (k, v) in url.query_pairs() {
                    match k.deref() {
                        "rev" => rev = Some(v.to_string()),
                        _ => {
                            return Err(url_err(FetchExprUrlError::UnknownQueryParameter(k.into())))
                        }
                    }
                }
                Ok(Self::FetchGit {
                    url: url_s,
                    rev: rev
                        .ok_or_else(|| url_err(FetchExprUrlError::MissingQueryParameter("rev")))?,
                })
            }
            _ => Err(url_err(FetchExprUrlError::Fetcher(fetcher.into()))),
        }
    }
}

#[derive(Debug, Deref, DerefMut)]
struct NixCommand(Helper);

impl NixCommand {
    fn new(path: impl AsRef<Path>) -> Self {
        let mut helper = Helper::new(path.as_ref());
        helper
            .args(["--log-format", "internal-json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self(helper)
    }

    fn input(&mut self, name: &str, expr: &FetchExpr) {
        self.args(["--arg", name, &expr.to_string()]);
    }
    fn input_url(&mut self, name: &str, url_s: &str) -> Result<(), FetchExprError> {
        self.input(name, &FetchExpr::parse_url(url_s)?);
        Ok(())
    }

    fn call(mut self) -> Result<String> {
        let mut child = self.start()?;

        let join_stderr = std::thread::spawn({
            let stderr = child.child.stderr.take().expect("child has no stderr");
            move || -> Result<()> {
                let mut lines = BufReader::new(stderr).lines();
                let mut tasks = BTreeMap::<u64, BuildTask>::new();
                let mut progress_dedup = BTreeMap::<u64, (u64, u64, u64, u64)>::new();

                fn get_task<'a>(
                    tasks: &'a mut BTreeMap<u64, BuildTask>,
                    id: u64,
                ) -> &'a BuildTask<'a> {
                    tasks.entry(id).or_default()
                }

                while let Some(line) = lines.next().transpose().into_diagnostic()? {
                    if let Some(json) = line.strip_prefix("@nix ") {
                        let action = match serde_json::from_str::<Action>(json).into_diagnostic() {
                            Ok(action) => action,
                            Err(err) => {
                                warn!("{:?}", err.context("Couldn't decode log line"));
                                continue;
                            }
                        };
                        let event = match action {
                            Action::Start(ActionStart {
                                id,
                                level,
                                parent,
                                text,
                                type_,
                            }) => {
                                let task = BuildTask {
                                    id: Uuid::now_v7(),
                                    parent_id: Some(parent)
                                        .filter(|id| *id != 0)
                                        .and_then(|id| tasks.get(&id).map(|t| t.id)),
                                    level: task_level(level),
                                    kind: task_kind(type_),
                                    name: text.into(),
                                    started_at: Utc::now(),
                                    ..Default::default()
                                };
                                let event = BuildEventKind::Task(BuildTaskEvent {
                                    task_id: task.id,
                                    event: BuildTaskEventKind::Started(task.clone()),
                                });

                                tasks.insert(id, task);
                                event
                            }
                            Action::Stop(ActionStop { id }) => {
                                BuildEventKind::Task(BuildTaskEvent {
                                    task_id: get_task(&mut tasks, id).id,
                                    event: BuildTaskEventKind::Stopped,
                                })
                            }
                            Action::Result(ActionResult { id, data }) => {
                                let task = get_task(&mut tasks, id);
                                BuildEventKind::Task(BuildTaskEvent {
                                    task_id: task.id,
                                    event: match data {
                                        ActionResultData::FileLinked(FileLinked {
                                            bytes_linked,
                                        }) => BuildTaskEventKind::Updated(
                                            BuildTaskUpdate::FileLinked {
                                                bytes: bytes_linked
                                                    .try_into()
                                                    .expect("bytes_linked"),
                                            },
                                        ),
                                        ActionResultData::BuildLogLine(BuildLogLine { line }) => {
                                            BuildTaskEventKind::Log(Log {
                                                kind: LogKind::Build,
                                                text: line.into(),
                                                ..Default::default()
                                            })
                                        }
                                        ActionResultData::PostBuildLogLine(BuildLogLine {
                                            line,
                                        }) => BuildTaskEventKind::Log(Log {
                                            level: task.level,
                                            kind: LogKind::PostBuild,
                                            text: line.into(),
                                            ..Default::default()
                                        }),
                                        ActionResultData::UntrustedPath => {
                                            BuildTaskEventKind::Updated(
                                                BuildTaskUpdate::UntrustedPath,
                                            )
                                        }
                                        ActionResultData::CorruptedPath => {
                                            BuildTaskEventKind::Updated(
                                                BuildTaskUpdate::CorruptedPath,
                                            )
                                        }
                                        ActionResultData::SetPhase(SetPhase { phase }) => {
                                            BuildTaskEventKind::Updated(BuildTaskUpdate::SetPhase {
                                                phase: phase.into(),
                                            })
                                        }
                                        ActionResultData::Progress(Progress {
                                            done,
                                            expected,
                                            running,
                                            failed,
                                        }) => {
                                            let dedup = (done, expected, running, failed);
                                            if progress_dedup.insert(id, dedup) == Some(dedup) {
                                                trace!(
                                                    id,
                                                    done,
                                                    expected,
                                                    running,
                                                    failed,
                                                    "Skipping duplicate Progress"
                                                );
                                            }
                                            BuildTaskEventKind::Updated(BuildTaskUpdate::Progress {
                                                done: done.try_into().expect("done"),
                                                expected: expected.try_into().expect("expected"),
                                                running: running.try_into().expect("running"),
                                                failed: failed.try_into().expect("failed"),
                                            })
                                        }
                                        ActionResultData::SetExpected(SetExpected { .. }) => {
                                            continue; // Not terribly interesting.
                                        }
                                    },
                                })
                            }
                            Action::Message(ActionMessage {
                                file,
                                column,
                                line,
                                level,
                                msg,
                            }) => BuildEventKind::Log(Log {
                                level: task_level(level),
                                kind: LogKind::Message,
                                file: file.map(|s| s.into()),
                                column: column.map(|v| v.try_into().expect("column")),
                                line: line.map(|v| v.try_into().expect("line")),
                                text: msg.into(),
                            }),
                        };
                        event_handler().handle(Event::Build(BuildEvent {
                            timestamp: Utc::now(),
                            build_id: Uuid::nil(),
                            event_id: Uuid::now_v7(),
                            event,
                        }));
                    }
                }
                Ok(())
            }
        });

        let out = {
            let out_r = child.wait_checked();
            join_stderr.join().expect("stderr thread panicked!?")?;
            out_r
        }?;
        if !out.status.success() {
            return match out.status.code() {
                Some(code) => Err(miette!("Process exited with status: {code}")),
                None => Err(miette!("Process exited with unknown status")),
            };
        }
        let mut stdout = String::from_utf8(out.stdout)
            .into_diagnostic()
            .context("stdout contains invalid UTF-8")?;
        stdout.truncate(stdout.trim_end().len());
        Ok(stdout)
    }
}

fn run(args: Args) -> Result<()> {
    let build: Build = serde_json::from_str(&args.build_json)
        .into_diagnostic()
        .context("Couldn't parse Build spec")?;

    // First, instantiate the derivation...
    let mut inst = NixCommand::new(args.nix_instantiate);
    for (i, (name, s)) in build.spec.inputs.iter().enumerate() {
        inst.input_url(name, s)
            .with_context(|| format!("build.inputs[{i}]"))?;
    }
    inst.args(["--expr", &build.spec.expr]);
    info!(?inst, "Instantiating derivation...");
    let drv = inst.call().context("nix-instantiate failed")?;

    // ...then realise (build) it.
    let mut build = NixCommand::new(args.nix_store);
    build.args(["--realise", &drv]);
    info!(?build, "Building...");
    let out = build.call().context("nix-instantiate failed")?;

    println!("{out}");
    Ok(())
}

fn main() {
    let args = Args::parse();
    gorgon_util_cli::logger(args.common).init();
    gorgon_util_cli::main(args.common, || run(args));
}

pub fn task_level(verbosity: Verbosity) -> BuildTaskLevel {
    match verbosity {
        Verbosity::Error => BuildTaskLevel::Error,
        Verbosity::Warn => BuildTaskLevel::Warn,
        Verbosity::Notice => BuildTaskLevel::Notice,
        Verbosity::Info => BuildTaskLevel::Info,
        Verbosity::Talkative => BuildTaskLevel::Talkative,
        Verbosity::Chatty => BuildTaskLevel::Chatty,
        Verbosity::Debug => BuildTaskLevel::Debug,
        Verbosity::Vomit => BuildTaskLevel::Vomit,
    }
}

pub fn task_kind(type_: StderrActivityType) -> Option<BuildTaskKind> {
    match type_ {
        StderrActivityType::Unknown => None,
        StderrActivityType::CopyPath => Some(BuildTaskKind::CopyPath),
        StderrActivityType::FileTransfer => Some(BuildTaskKind::FileTransfer),
        StderrActivityType::Realise => Some(BuildTaskKind::Realise),
        StderrActivityType::CopyPaths => Some(BuildTaskKind::CopyPaths),
        StderrActivityType::Builds => Some(BuildTaskKind::Builds),
        StderrActivityType::Build => Some(BuildTaskKind::Build),
        StderrActivityType::OptimiseStore => Some(BuildTaskKind::OptimiseStore),
        StderrActivityType::VerifyPaths => Some(BuildTaskKind::VerifyPaths),
        StderrActivityType::Substitute => Some(BuildTaskKind::Substitute),
        StderrActivityType::QueryPathInfo => Some(BuildTaskKind::QueryPathInfo),
        StderrActivityType::PostBuildHook => Some(BuildTaskKind::PostBuildHook),
        StderrActivityType::BuildWaiting => Some(BuildTaskKind::BuildWaiting),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_fetchexpr_parse_url_bad_url() {
        assert_eq!(
            FetchExprError::ParseUrl("bad".into(), url::ParseError::RelativeUrlWithoutBase),
            FetchExpr::parse_url("bad").unwrap_err(),
        );
    }
    #[test]
    fn test_fetchexpr_parse_url_bad_scheme() {
        let url = "bad://";
        assert_eq!(
            FetchExprError::Url(
                Url::parse(url).unwrap(),
                FetchExprUrlError::Fetcher("bad".into()),
            ),
            FetchExpr::parse_url(url).unwrap_err(),
        );
    }
    #[test]
    fn test_fetchexpr_parse_url_unknown_query() {
        let url = "git+https://example.com/?bad=yes";
        assert_eq!(
            FetchExprError::Url(
                Url::parse(url).unwrap(),
                FetchExprUrlError::UnknownQueryParameter("bad".into()),
            ),
            FetchExpr::parse_url(url).unwrap_err(),
        );
    }
    #[test]
    fn test_fetchexpr_parse_url_missing_rev() {
        let url = "git+https://example.com/";
        assert_eq!(
            FetchExprError::Url(
                Url::parse(url).unwrap(),
                FetchExprUrlError::MissingQueryParameter("rev"),
            ),
            FetchExpr::parse_url(url).unwrap_err(),
        );
    }
    #[test]
    fn test_fetchexpr_parse_url_git() {
        assert_eq!(
            FetchExpr::FetchGit {
                url: "git://example.com/".into(),
                rev: "abc123".into(),
            },
            FetchExpr::parse_url("git://example.com/?rev=abc123").unwrap(),
        );
    }
    #[test]
    fn test_fetchexpr_parse_url_git_https() {
        assert_eq!(
            FetchExpr::FetchGit {
                url: "https://example.com/".into(),
                rev: "abc123".into(),
            },
            FetchExpr::parse_url("git+https://example.com/?rev=abc123").unwrap(),
        );
    }
}
