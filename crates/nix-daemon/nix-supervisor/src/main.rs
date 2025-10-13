// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

mod api;
mod entity;
mod migration;

use anyhow::{anyhow, Context, Result};
use async_tempfile::TempFile;
use chrono::prelude::*;
use clap::Parser;
use nix_daemon::{
    nix::{DaemonProtocolAdapter, DaemonStore},
    BuildMode, BuildResult, ClientSettings, Missing, NixError, PathInfo, Progress, Stderr,
    StderrActivityType, StderrResult, StderrResultType, StderrStartActivity, Store,
};
use sea_orm::{prelude::*, Database, Set};
use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};
use std::{fmt::Debug, ops::Deref};
use tap::{Tap, TapFallible};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{
    net::{UnixListener, UnixStream},
    select,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Increase log level.
    #[arg(long, short, action=clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease log level.
    #[arg(long, short, action=clap::ArgAction::Count)]
    quiet: u8,

    /// Path to the database. [default: $XDG_STATE_HOME/nix-supervisor/db.sqlite3]
    #[arg(long)]
    db_path: Option<PathBuf>,

    /// Path to the real nix daemon socket.
    #[arg(long, short = 'S', default_value = "/nix/var/nix/daemon-socket/socket")]
    nix_daemon_sock: PathBuf,

    /// Path to our listening socket.
    #[arg(long, short = 's', default_value = "nix-supervisor.sock")]
    sock: PathBuf,

    /// API listen address.
    #[arg(long, short = 'L', default_value = "[::]:9649")]
    http_listen: std::net::SocketAddr,
}

#[instrument(skip_all)]
fn init_logging(args: &Args) {
    use tracing_subscriber::prelude::*;

    tracing_subscriber::Registry::default()
        .with(match args.verbose as i8 - args.quiet as i8 {
            ..=-2 => tracing_subscriber::filter::LevelFilter::ERROR,
            -1 => tracing_subscriber::filter::LevelFilter::WARN,
            0 => tracing_subscriber::filter::LevelFilter::INFO,
            1 => tracing_subscriber::filter::LevelFilter::DEBUG,
            2.. => tracing_subscriber::filter::LevelFilter::TRACE,
        })
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

#[instrument(skip_all)]
async fn init_db(args: &Args, xdg_dirs: &xdg::BaseDirectories) -> Result<Arc<DatabaseConnection>> {
    use sea_orm_migration::prelude::*;

    let db_path = args
        .db_path
        .clone()
        .map(|p| Ok(p))
        .unwrap_or_else(|| xdg_dirs.place_state_file("db.sqlite3"))
        .context("Creating database directory")?
        .into_os_string()
        .into_string()
        .expect("Database path must be valid UTF-8");

    debug!(?db_path, "Connecting to database...");
    let mut opt = sea_orm::ConnectOptions::new(format!("sqlite:{}?mode=rwc", &db_path));
    opt.sqlx_logging(true)
        .sqlx_logging_level(tracing::log::LevelFilter::Debug);
    let db = Database::connect(opt)
        .await
        .context("Couldn't connect to database")?;

    debug!("Running database migrations...");
    migration::Migrator::up(&db, None)
        .await
        .context("Running database migrations")?;

    Ok(Arc::new(db))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(&args);

    let xdg_dirs = xdg::BaseDirectories::with_prefix("nix-supervisor")?;
    let db = init_db(&args, &xdg_dirs)
        .await
        .context("Connecting to database")?;

    let mut listen_fds = listenfd::ListenFd::from_env();
    if listen_fds.len() > 0 {
        info!("We're socket-activated, using external FDs...");
    }

    // Listen for Ctrl+C or a shutdown signal, then start a graceful termination.
    let shutdown_root = CancellationToken::new();
    let shutdown_child = shutdown_root.child_token();
    tokio::spawn(
        async move {
            debug!("Waiting for shutdown signal...");
            tokio::signal::ctrl_c()
                .await
                .expect("Couldn't register Ctrl+C handler");
            debug!("Shutdown signal received");
            shutdown_root.cancel();
        }
        .instrument(info_span!("ctrl_c")),
    );
    // Outstanding tasks should hold a clone of the sender, but never send anything.
    // When shutting down, we'll wait until all senders are dropped before exiting.
    let (shutdown_send, mut shutdown_recv) = tokio::sync::mpsc::channel::<()>(1);

    // Spawn the API.
    {
        let api_srv = if listen_fds.len() > 1 {
            axum::Server::from_tcp(
                listen_fds
                    .take_tcp_listener(1)
                    .context("Couldn't take API socket; wrong order?")?
                    .ok_or_else(|| anyhow!("FD 1 already consumed; this should be impossible???"))?
                    .tap(|listener| {
                        info!(
                            addr = ?listener.local_addr().unwrap(),
                            "-> Using external FD 1 for API"
                        )
                    }),
            )
            .context("Couldn't wrap external API socket")?
        } else {
            axum::Server::try_bind(&args.http_listen).context("Couldn't bind to API socket")?
        }
        .serve(api::new(db.clone()).into_make_service());

        let shutdown = shutdown_child.child_token();
        let shutdown_send_ = shutdown_send.clone();
        tokio::spawn(
            async move {
                info!(addr = ?api_srv.local_addr(), "API Running!");
                api_srv
                    .with_graceful_shutdown(shutdown.cancelled())
                    .await
                    .expect("API shut down unexpectedly");
                drop(shutdown_send_);
            }
            .instrument(info_span!("api")),
        );
    }

    // Take an external FD if we were socket activated, else listen normally.
    let listener = if listen_fds.len() > 0 {
        UnixListener::from_std(
            listen_fds
                .take_unix_listener(0)
                .context("Couldn't take proxy socket; wrong order?")?
                .ok_or_else(|| anyhow!("FD 0 already consumed; this should be impossible???"))?
                .tap(|listener| {
                    info!(
                        addr = ?listener.local_addr().unwrap(),
                        "-> Using external FD 0 for proxy...",
                    )
                }),
        )
        .context("Couldn't turn std::net::UnixListener into tokio::net::UnixListener")?
    } else {
        UnixListener::bind(&args.sock)
            .with_context(|| format!("Couldn't bind to socket: {}", args.sock.to_string_lossy()))?
    };
    let listener_path = listener
        .local_addr()?
        .as_pathname()
        .ok_or_else(|| anyhow!("socket is unnamed; this should be impossible"))?
        .canonicalize()?;
    info!(path = ?listener_path.to_string_lossy(), "Running!");

    loop {
        debug!("Waiting for a connection...");
        match select! {
            _ = shutdown_child.cancelled() => None,
            r = listener.accept() => Some(r),
        } {
            Some(Ok((cstream, _))) => {
                let cred = cstream
                    .peer_cred()
                    .tap_err(|err| warn!(?err, "Couldn't get client's SO_PEERCRED"))
                    .ok();

                let args_ = args.clone();
                let shutdown_ = shutdown_child.child_token();
                let db_ = db.clone();
                let shutdown_send_ = shutdown_send.clone();
                tokio::spawn(
                    async move {
                        info!("Client connected");
                        if let Err(err) = run_session(args_, shutdown_, db_, cstream).await {
                            error!("{:?}", err);
                        }
                        drop(shutdown_send_);
                    }
                    .instrument(info_span!(
                        "conn",
                        pid = cred.map(|c| c.pid()),
                        uid = cred.map(|c| c.uid())
                    )),
                );
            }
            Some(Err(err)) => error!(?err, "Couldn't accept connection"),
            None => {
                debug!("Accept loop shutting down");
                break;
            }
        }
    }

    // Clean up the dang socket file.
    debug!(path = ?listener_path.to_string_lossy(), "Shutting down socket...");
    drop(listener);
    tokio::fs::remove_file(listener_path)
        .await
        .context("Removing socket")?;

    // Try to receive on the shutdown channel. Nothing should be sending on it, so it'll wait
    // until no more senders exist, and fail - this is what we want, so we discard the error.
    debug!("Waiting for outstanding connections...");
    drop(shutdown_send);
    let _ = shutdown_recv.recv().await;

    // If we can't do Arc::into_inner() here, we've got an outstanding clone of the db Arc.
    // Make sure anything that takes a db.clone() also takes a cancellation token, and holds
    // a sender on the shutdown channel until they've properly terminated.
    debug!("Closing database connection...");
    Arc::into_inner(db)
        .ok_or_else(|| anyhow!("Database has outstanding references; this is a bug"))?
        .close()
        .await
        .context("Shutting down database")?;

    info!("Bye!");
    Ok(())
}

async fn run_session(
    args: Args,
    _cancel: CancellationToken,
    db: Arc<DatabaseConnection>,
    cstream: UnixStream,
) -> Result<()> {
    // Connect to the real nix-daemon.
    debug!(?args.nix_daemon_sock, "Connecting to nix daemon on client's behalf...");
    let store = DaemonStore::builder()
        .connect_unix(&args.nix_daemon_sock)
        .await
        .with_context(|| {
            format!(
                "Couldn't connect to nix daemon socket: {}",
                args.nix_daemon_sock.to_string_lossy()
            )
        })?;

    // Wrap the store in our proxy.
    let mut proxy = ProxyStore { store, db };

    // Run a daemon protocol adapter.
    let (cr, cw) = cstream.into_split();
    let mut adapter = DaemonProtocolAdapter::builder(&mut proxy)
        .adopt(cr, cw)
        .await?;
    Ok(adapter.run().await?)
}

struct BuildState {
    build: entity::build::Model,
    log: Option<TempFile>,
}

/// Tracking wrapper for a Progress.
struct TrackingProgress<'db, T: Send, E, P: Progress<T = T, Error = E>, HF: FnOnce(&T) + Send>
where
    anyhow::Error: From<E>,
{
    db: &'db Arc<DatabaseConnection>,
    prog: P,
    result_hook: HF,

    // Currently active builds.
    builds: HashMap<u64, BuildState>,
}

impl<'db, T: Send, E, P: Progress<T = T, Error = E>, HF: FnOnce(&T) + Send> Progress
    for TrackingProgress<'db, T, E, P, HF>
where
    anyhow::Error: From<E>,
{
    type T = T;
    type Error = anyhow::Error;

    async fn next(&mut self) -> Result<Option<Stderr>, Self::Error> {
        let v = self.prog.next().await.map_err(Into::into)?;
        match &v {
            None => trace!("End of queued stderr"),
            Some(Stderr::Next(msg)) => trace!(msg, "Message"),
            Some(Stderr::Error(NixError { level, msg, traces })) => {
                trace!(?level, msg, ?traces, "Exception Thrown")
            }
            Some(Stderr::StartActivity(StderrStartActivity {
                act_id,
                level,
                kind,
                s,
                fields,
                parent_id,
            })) => match kind {
                StderrActivityType::Build => {
                    // cur_rounds and nr_rounds aren't used anymore, and the daemon will error
                    // unless they're both 1. (They were for running a build multiple times.)
                    let (name, machine_name, cur_rounds, nr_rounds) = (
                        fields.get(0).and_then(|v| v.as_string()),
                        fields.get(1).and_then(|v| v.as_string()),
                        fields.get(2).and_then(|v| v.as_int()),
                        fields.get(3).and_then(|v| v.as_int()),
                    );
                    trace!(
                        act_id,
                        ?level,
                        s,
                        name,
                        machine_name,
                        cur_rounds,
                        nr_rounds,
                        parent_id,
                        "Start Activity: Build"
                    );

                    // Start tracking build progress.
                    let build = entity::build::ActiveModel {
                        name: Set(name.cloned().unwrap_or_default()),
                        machine_name: Set(machine_name.cloned().unwrap_or_default()),
                        start: Set(Utc::now()),
                        ..Default::default()
                    };
                    debug!(
                        ?build.name,
                        ?build.machine_name, "[{}] Build Started", act_id
                    );
                    self.builds.insert(
                        *act_id,
                        BuildState {
                            build: build
                                .insert(self.db.deref())
                                .await
                                .context("Saving build")?,
                            log: None,
                        },
                    );
                }
                _ => trace!(
                    act_id,
                    ?level,
                    ?kind,
                    s,
                    ?fields,
                    parent_id,
                    "Start Activity"
                ),
            },
            Some(Stderr::StopActivity { act_id }) => {
                trace!(act_id, "Stop Activity");

                // Is this the end of a build we're tracking?
                if let Some(bs) = self.builds.remove(&act_id) {
                    // Set the end time.
                    let now = Utc::now();
                    let build = {
                        let mut build_: entity::build::ActiveModel = bs.build.into();
                        build_.end = Set(Some(now));
                        build_.temp_log_path = Set(None);
                        build_
                            .update(self.db.deref())
                            .await
                            .context("Saving build")?
                    };

                    // Log the final phase too.
                    if let Some(entity::build::Phase(last_phase, started)) =
                        build.start_phase.0.last()
                    {
                        debug!(t=?(now-started),"[{}] -> {} finished", act_id, last_phase);
                    }
                    debug!(?build.name,?build.machine_name,t=?(now-build.start), "[{}] Build Finished", act_id);
                }
            }
            Some(Stderr::Result(StderrResult {
                act_id,
                kind,
                fields,
            })) => {
                match kind {
                    StderrResultType::BuildLogLine => {
                        let msg = fields
                            .get(0)
                            .and_then(|v| v.as_string())
                            .ok_or_else(|| anyhow!("{:?} has no 'msg'", kind))?;
                        trace!(act_id, msg, "Result: BuildLogLine");

                        // Write it to the temporary log file.
                        if let Some(bs) = self.builds.get_mut(&act_id) {
                            // If we don't have a TempFile open for this build, open one.
                            if bs.log.is_none() {
                                let log = TempFile::new_with_name(format!(
                                    "nix_supervisor_build_{}.tmp.log",
                                    bs.build.id
                                ))
                                .await
                                .with_context(|| {
                                    format!(
                                        "Creating temporary log file for build #{} ({})",
                                        bs.build.id, bs.build.name
                                    )
                                })?;
                                debug!(act_id, path = log.file_path().to_str(), "Opened log file");

                                bs.build = {
                                    let mut build_: entity::build::ActiveModel =
                                        bs.build.clone().into();
                                    build_.temp_log_path = Set(Some(log.file_path().to_str().ok_or_else(|| anyhow!(
                                                 "Temp log path for build #{} ({}) contains invalid UTF-8: {}", bs.build.id,
                                                 bs.build.name,
                                                 log.file_path().to_string_lossy()))?.into(),
                                             ));
                                    build_
                                        .update(self.db.deref())
                                        .await
                                        .context("Saving build")?
                                };
                                bs.log = Some(log);
                            };

                            // Write the log line + a trailing newline.
                            let log = bs.log.as_mut().unwrap();
                            log.write_all(msg.as_bytes())
                                .await
                                .with_context(|| format!("Writing log line: {}", msg))?;
                            log.write_all(&[b'\n'])
                                .await
                                .with_context(|| format!("Writing newline after: {}", msg))?;
                        } else {
                            warn!(act_id, msg, "BuildLogLine on unknown build");
                        };
                    }
                    StderrResultType::SetPhase => {
                        let phase = fields
                            .get(0)
                            .and_then(|v| v.as_string())
                            .ok_or_else(|| anyhow!("{:?} has no 'phase'", kind))?;
                        trace!(act_id, phase, "Result: SetPhase");

                        if let Some(bs) = self.builds.get_mut(&act_id) {
                            let now = Utc::now();

                            // If we just finished a phase, log that.
                            if let Some(entity::build::Phase(phase, started)) =
                                bs.build.start_phase.0.last()
                            {
                                debug!(t=?(now-started),"[{}] -> {} finished", act_id, phase);
                            }

                            // Push it into the state list.
                            bs.build = {
                                let mut build_: entity::build::ActiveModel =
                                    bs.build.clone().into();
                                build_.start_phase = Set(entity::build::PhaseList({
                                    let mut list = bs.build.start_phase.0.clone();
                                    list.push(entity::build::Phase(phase.clone(), now));
                                    list
                                }));
                                build_
                                    .update(self.db.deref())
                                    .await
                                    .context("Saving build")?
                            }
                        } else {
                            warn!(act_id, phase, "SetPhase on unknown build");
                        };
                    }
                    StderrResultType::Progress => {
                        let done = fields
                            .get(0)
                            .and_then(|v| v.as_int())
                            .ok_or_else(|| anyhow!("{:?} has no 'done'", kind))?;
                        let expected = fields
                            .get(1)
                            .and_then(|v| v.as_int())
                            .ok_or_else(|| anyhow!("{:?} has no 'expected'", kind))?;
                        let running = fields
                            .get(2)
                            .and_then(|v| v.as_int())
                            .ok_or_else(|| anyhow!("{:?} has no 'running'", kind))?;
                        let failed = fields
                            .get(3)
                            .and_then(|v| v.as_int())
                            .ok_or_else(|| anyhow!("{:?} has no 'failed'", kind))?;
                        trace!(act_id, done, expected, running, failed, "Result: Progress");
                    }
                    StderrResultType::SetExpected => {
                        let state = fields
                            .get(0)
                            .and_then(|v| v.as_activity_type())
                            .ok_or_else(|| anyhow!("{:?} has no 'state'", kind))?;
                        let expected = fields
                            .get(1)
                            .and_then(|v| v.as_int())
                            .ok_or_else(|| anyhow!("{:?} has no 'expected'", kind))?;
                        trace!(act_id, ?state, expected, "Result: SetExpected");
                    }
                    _ => trace!(act_id, ?kind, ?fields, "Result"),
                }
            }
        }
        Ok(v)
    }

    async fn result(mut self) -> Result<Self::T, Self::Error> {
        while self.next().await?.is_some() {}
        self.prog
            .result()
            .await
            .tap_ok(|v| (self.result_hook)(v))
            .map_err(Into::into)
    }
}

struct ProxyStore<ST: Store + Send + Sync>
where
    ST::Error: 'static + std::error::Error,
{
    store: ST,
    db: Arc<DatabaseConnection>,
}

impl<ST: Store + Send + Sync> ProxyStore<ST>
where
    ST::Error: 'static + std::error::Error,
{
    fn track<'db, T: Send, P: Progress<T = T, Error = ST::Error>, HF: FnOnce(&T) + Send>(
        db: &'db Arc<DatabaseConnection>,
        prog: P,
        result_hook: HF,
    ) -> TrackingProgress<T, ST::Error, P, HF> {
        TrackingProgress {
            db,
            prog,
            result_hook,
            builds: HashMap::<_, _>::new(),
        }
    }
}

impl<ST: Store + Send + Sync> Store for ProxyStore<ST>
where
    ST::Error: 'static + std::error::Error,
{
    type Error = anyhow::Error;

    /// Returns whether a store path is valid.
    #[instrument(skip(self))]
    fn is_valid_path<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = bool, Error = Self::Error> {
        debug!("-> IsValidPath");
        Self::track(&self.db, self.store.is_valid_path(path), |valid| {
            debug!(valid, "<- IsValidPath");
        })
    }

    /// Adds a file to the store.
    #[instrument(skip(self))]
    fn add_to_store<
        SN: AsRef<str> + Send + Sync + Debug,
        SC: AsRef<str> + Send + Sync + Debug,
        Refs,
        R,
    >(
        &mut self,
        name: SN,
        cam_str: SC,
        refs: Refs,
        repair: bool,
        source: R,
    ) -> impl Progress<T = (String, PathInfo), Error = Self::Error>
    where
        Refs: IntoIterator + Send + Debug,
        Refs::IntoIter: ExactSizeIterator + Send,
        Refs::Item: AsRef<str> + Send + Sync,
        R: AsyncReadExt + Unpin + Send + Debug,
    {
        debug!("-> AddToStore");
        Self::track(
            &self.db,
            self.store.add_to_store(name, cam_str, refs, repair, source),
            |(name, pi)| debug!(name, ?pi, "<- AddToStore"),
        )
    }

    /// Returns whether QuerySubstitutablePathInfos would return anything.
    #[instrument(skip(self))]
    fn has_substitutes<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = bool, Error = Self::Error> {
        debug!(?path, "-> HasSubstitute");
        Self::track(&self.db, self.store.has_substitutes(path), |has| {
            debug!(has, "<- HasSubstitute")
        })
    }

    /// Builds the specified paths.
    #[instrument(skip(self))]
    fn build_paths<Paths>(
        &mut self,
        paths: Paths,
        mode: BuildMode,
    ) -> impl Progress<T = (), Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!(?paths, ?mode, "-> BuildPaths");
        Self::track(&self.db, self.store.build_paths(paths, mode), |_| {})
    }

    /// Ensure the specified store path exists.
    #[instrument(skip(self))]
    fn ensure_path<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        debug!(?path, "-> EnsurePath");
        Self::track(&self.db, self.store.ensure_path(path), |_| {})
    }

    /// Creates a temporary GC root, which persists until the daemon restarts.
    #[instrument(skip(self))]
    fn add_temp_root<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        debug!(?path, "-> AddTempRoot");
        Self::track(&self.db, self.store.add_temp_root(path), |_| {})
    }

    /// Creates a persistent GC root. This is what's normally meant by a GC root.
    #[instrument(skip(self))]
    fn add_indirect_root<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        debug!(?path, "-> AddIndirectRoot");
        Self::track(&self.db, self.store.add_indirect_root(path), |_| {})
    }

    /// Returns the (link, target) of all known GC roots.
    #[instrument(skip(self))]
    fn find_roots(&mut self) -> impl Progress<T = HashMap<String, String>, Error = Self::Error> {
        debug!("-> FindRoots");
        Self::track(&self.db, self.store.find_roots(), |roots| {
            debug!("<- FindRoots");
            for (link, target) in roots {
                debug!(link, target, "  <- FindRoots.root[]");
            }
        })
    }

    /// Applies client options. This changes the behaviour of future commands.
    #[instrument(skip(self))]
    fn set_options(&mut self, opts: ClientSettings) -> impl Progress<T = (), Error = Self::Error> {
        debug!(
            opts.keep_failed,
            opts.keep_going,
            opts.try_fallback,
            ?opts.verbosity,
            opts.max_build_jobs,
            opts.max_silent_time,
            opts.verbose_build,
            opts.build_cores,
            opts.use_substitutes,
            ?opts.overrides,
            "-> SetOptions"
        );
        Self::track(&self.db, self.store.set_options(opts), |_| {})
    }

    /// Returns a PathInfo struct for the given path.
    #[instrument(skip(self))]
    fn query_pathinfo<S: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: S,
    ) -> impl Progress<T = Option<PathInfo>, Error = Self::Error> {
        debug!(?path, "-> QueryPathInfo");
        Self::track(&self.db, self.store.query_pathinfo(path), |pi| {
            debug!(?pi, "<- QueryPathInfo")
        })
    }

    /// Returns which of the passed paths are valid.
    #[instrument(skip(self))]
    fn query_valid_paths<Paths>(
        &mut self,
        paths: Paths,
        use_substituters: bool,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!(?paths, use_substituters, "-> QueryValidPaths");
        Self::track(
            &self.db,
            self.store.query_valid_paths(paths, use_substituters),
            |valids| {
                debug!("<- QueryValidPaths");
                for valid in valids {
                    debug!(valid, "  <- QueryValidPaths.valids[]");
                }
            },
        )
    }

    /// Returns paths which can be substituted.
    #[instrument(skip(self))]
    fn query_substitutable_paths<Paths>(
        &mut self,
        paths: Paths,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!(?paths, "-> QuerySubstitutablePaths");
        Self::track(
            &self.db,
            self.store.query_substitutable_paths(paths),
            |subs| {
                debug!("<- QuerySubstitutablePaths");
                for sub in subs {
                    debug!(sub, "  <- QuerySubstitutablePaths.subs[]");
                }
            },
        )
    }

    /// Returns a list of valid derivers for a path.
    /// This is sort of like PathInfo.deriver, but it doesn't lie to you.
    #[instrument(skip(self))]
    fn query_valid_derivers<S: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: S,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error> {
        debug!(?path, "-> QueryValidDerivers");
        Self::track(&self.db, self.store.query_valid_derivers(path), |drvs| {
            debug!("<- QueryValidDerivers");
            for drv in drvs {
                debug!(drv, "  <- QueryValidDerivers.drvs[]");
            }
        })
    }

    /// Takes a list of paths and queries which would be built, substituted or unknown,
    /// along with an estimate of the cumulative download and NAR sizes.
    #[instrument(skip(self))]
    fn query_missing<Ps>(&mut self, paths: Ps) -> impl Progress<T = Missing, Error = Self::Error>
    where
        Ps: IntoIterator + Send + Debug,
        Ps::IntoIter: ExactSizeIterator + Send,
        Ps::Item: AsRef<str> + Send + Sync,
    {
        debug!(?paths, "-> QueryMissing");
        Self::track(&self.db, self.store.query_missing(paths), |missing| {
            debug!(?missing, "<- QueryMissing");
        })
    }

    /// Returns a map of (output, store path) for the given derivation.
    #[instrument(skip(self))]
    fn query_derivation_output_map<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = HashMap<String, String>, Error = Self::Error> {
        debug!(?path, "-> QueryDerivationOutputMap");
        Self::track(
            &self.db,
            self.store.query_derivation_output_map(path),
            |outputs| {
                debug!("<- QueryDerivationOutputMap");
                for (name, path) in outputs {
                    debug!(name, path, "  <- QueryDerivationOutputMap.outputs[]");
                }
            },
        )
    }

    #[instrument(skip(self))]
    fn build_paths_with_results<Ps>(
        &mut self,
        paths: Ps,
        mode: BuildMode,
    ) -> impl Progress<T = HashMap<String, BuildResult>, Error = Self::Error>
    where
        Ps: IntoIterator + Send + Debug,
        Ps::IntoIter: ExactSizeIterator + Send,
        Ps::Item: AsRef<str> + Send + Sync,
    {
        debug!(?paths, ?mode, "-> BuildPathsWithResults");
        Self::track(
            &self.db,
            self.store.build_paths_with_results(paths, mode),
            |results| {
                debug!("<- BuildPathsWithResults");
                for (path, result) in results {
                    debug!(
                        path,
                        ?result.status,
                        result.error_msg,
                        result.times_built,
                        result.is_non_deterministic,
                        ?result.start_time,
                        ?result.stop_time,
                         "  <- BuildPathsWithResults.results[]");
                    for (name, path) in &result.built_outputs {
                        debug!(
                            name,
                            path, "    <- BuildPathsWithResults.results[].outputs[]"
                        );
                    }
                }
            },
        )
    }
}
