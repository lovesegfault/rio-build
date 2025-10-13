// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

pub mod aterm;
pub mod nix_cli;
pub mod supervisor_api;

use askama::Template;
use aterm::{Derivation, DerivationInputDerivation};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderName, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json,
};
use clap::Parser;
use hyper::header;
use listenfd::ListenFd;
use nix_daemon::{nix::DaemonStore, Progress, Store};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, Row};
use std::{collections::BTreeMap, path::PathBuf};
use std::{collections::BTreeSet, sync::Arc};
use tracing::{debug, error, info, instrument, warn};

pub type Result<T, E = Error> = std::result::Result<T, E>;

// TODO: Is there a way to make this a Path or PathBuf?
const NIX_STORE_PATH: &str = "/nix/store";

/// Error wrapper. Can be returned from a handler and turned into a 500 Internal Server Error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("nix command exited with code {0}: {1}")]
    NixCommandError(i32, String),

    #[error(transparent)]
    Askama(#[from] askama::Error),
    #[error(transparent)]
    JSON(#[from] serde_json::Error),
    #[error(transparent)]
    Hyper(#[from] hyper::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    URL(#[from] url::ParseError),
    #[error(transparent)]
    IO(#[from] std::io::Error),
    #[error(transparent)]
    NixDaemon(#[from] nix_daemon::Error),
    #[error(transparent)]
    ATerm(#[from] aterm::Error),

    #[error("request handler panicked - this is a bug: {0}")]
    Panic(String),
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    error: Error,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(ErrorTemplate { error: self }.render().unwrap())
            .unwrap()
            .into_response()
    }
}

fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    Error::Panic(if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "[UNKNOWN PANIC MESSAGE]".to_string()
    })
    .into_response()
}

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Path to the nix database.
    #[arg(long, default_value = "/nix/var/nix/db/db.sqlite")]
    nix_db_path: PathBuf,

    /// Path to the nix store.
    #[arg(long, default_value = "/nix/store")]
    nix_store_path: PathBuf,

    /// Path to the nix daemon socket.
    #[arg(long, default_value = "/nix/var/nix/daemon-socket/socket")]
    nix_daemon_sock: PathBuf,

    /// Increase log level.
    #[arg(long, short, action=clap::ArgAction::Count)]
    verbose: u8,

    /// Decrease log level.
    #[arg(long, short, action=clap::ArgAction::Count)]
    quiet: u8,

    /// Listen address.
    #[arg(long, short = 'L', default_value = "[::]:8649")]
    http_listen: std::net::SocketAddr,

    /// Use nix-supervisor.
    #[arg(long, short = 's')]
    nix_supervisor: bool,

    /// Address of the nix-supervisor API.
    #[arg(long, short = 'S', default_value = "http://[::1]:9649")]
    nix_supervisor_addr: url::Url,
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

#[derive(Debug, Clone)]
struct AppState {
    args: Args,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(&args);
    let state = Arc::new(AppState { args: args.clone() });

    let app = axum::Router::new()
        .route("/", get(get_index))
        .route("/static/*path", get(get_static))
        .route("/goto", get(get_goto))
        .route("/nix/store/:path", get(get_derivation))
        .route("/nix/store/:path/*_", get(get_derivation_trailing))
        .route("/api/nix/store/:drv/poll", get(get_api_derivation_poll))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            handle_panic,
        ));

    // Take an external FD if we were socket activated, else listen normally.
    let mut listen_fds = ListenFd::from_env();
    let srv = if listen_fds.len() > 0 {
        // This unwrap() can only fail if the FD has already been used.
        info!("We're socket-activated, using external FD 0...");
        axum::Server::from_tcp(listen_fds.take_tcp_listener(0)?.unwrap())?
    } else {
        axum::Server::try_bind(&args.http_listen)?
    }
    .serve(app.into_make_service());
    info!(addr = ?srv.local_addr(), "Running!");
    srv.await?;
    Ok(())
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    last_paths: Vec<String>,
}

async fn get_index(State(state): State<Arc<AppState>>) -> Result<IndexTemplate> {
    // Find the last registered paths in the store, sorted by most-recent-first.
    let last_path_paths: Vec<String> = {
        // Open the database as immutable, or we start fighting the daemon over the WAL.
        // This may cause problems if the database changes while the query is running.
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&state.args.nix_db_path)
                .immutable(true),
        )
        .await?;
        sqlx::query("SELECT path FROM ValidPaths ORDER BY registrationTime DESC LIMIT 10")
            .map(|row: sqlx::sqlite::SqliteRow| row.get(0))
            .fetch_all(&mut conn)
            .await?
    };

    Ok(IndexTemplate {
        last_paths: last_path_paths,
    })
}

async fn get_static(
    Path(path): Path<String>,
) -> (StatusCode, [(HeaderName, &'static str); 1], &'static str) {
    match path.as_str() {
        "style.css" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            include_str!("../static/style.css"),
        ),
        "bootstrap.css" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            include_str!("../../vendor/bootstrap/css/bootstrap.css"),
        ),
        "bootstrap.css.map" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            include_str!("../../vendor/bootstrap/css/bootstrap.css.map"),
        ),
        "bootstrap.js" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            include_str!("../../vendor/bootstrap/js/bootstrap.js"),
        ),
        "bootstrap.js.map" => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            include_str!("../../vendor/bootstrap/js/bootstrap.js.map"),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found",
        ),
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
pub struct GotoQuery {
    path: String,
}

async fn get_goto(query: Query<GotoQuery>) -> Response {
    Redirect::to(&query.path).into_response()
}

#[derive(Template)]
#[template(path = "derivation.html")]
struct DerivationTemplate<'a> {
    in_path: &'a str,
    roots: Vec<DerivationRootTemplate>,
}

#[derive(Template)]
#[template(path = "_derivation_root.html")]
struct DerivationRootTemplate {
    path: String,
    drv: Derivation,
    is_built: bool,
    outputs: Vec<(String, String, bool)>,
    input_tmpls: Vec<DerivationInputTemplate>,
    env_html: Vec<(String, String)>,

    builds: Vec<supervisor_api::Build>,
}

#[derive(Template)]
#[template(path = "_derivation_input.html")]
struct DerivationInputTemplate {
    path: String,
    is_valid: bool,
    is_seen_before: bool,
    input_tmpls: Vec<DerivationInputTemplate>,
}

impl DerivationInputTemplate {
    async fn build_batch<S: Store>(
        store: &mut S,
        drvs: &mut BTreeMap<String, Derivation>,
        paths_valid: &mut BTreeMap<String, bool>,
        drvs_built: &mut BTreeMap<String, bool>,
        seen: &mut BTreeSet<String>,
        inputs: &BTreeMap<String, DerivationInputDerivation>,
    ) -> Result<Vec<Self>>
    where
        Error: From<S::Error>,
    {
        let mut tmpls = Vec::new();
        for (drv_path, input) in inputs.iter() {
            if let Some(drv) = read_derivation(drvs, &drv_path)? {
                for out_name in input.outputs.iter() {
                    if let Some(out) = drv.outputs.get(out_name) {
                        let is_seen_before = seen.contains(&out.path);
                        if !is_seen_before {
                            seen.insert(out.path.clone());
                        }

                        let is_valid = check_path_valid(store, paths_valid, &out.path).await?;
                        let input_tmpls = if !is_seen_before {
                            Box::pin(Self::build_batch(
                                store,
                                drvs,
                                paths_valid,
                                drvs_built,
                                seen,
                                &drv.input_drvs,
                            ))
                            .await?
                        } else {
                            vec![]
                        };
                        tmpls.push(Self {
                            path: out.path.clone(),
                            is_valid,
                            is_seen_before,
                            input_tmpls,
                        });
                    } else {
                        // This should never happen.
                        error!(drv_path, out_name, "Input references bad output");
                    }
                }
            } else {
                error!(drv_path, "Input references bad derivation");
            }
        }
        Ok(tmpls)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
pub enum GetDerivationPage {
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "log")]
    Log,
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
pub struct GetDerivationQuery {
    #[serde(rename = "p")]
    page: Option<GetDerivationPage>,
}

// Swallows a trailing path, eg. /nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26/bin/bash
// should be /nix/store/ffffffffffffffffffffffffffffffff-bash-5.2p26.
async fn get_derivation_trailing(
    State(state): State<Arc<AppState>>,
    Path((store_path, _)): Path<(String, String)>,
    Query(query): Query<GetDerivationQuery>,
) -> Result<Response> {
    get_derivation(State(state), Path(store_path), Query(query)).await
}

async fn get_derivation(
    State(state): State<Arc<AppState>>,
    Path(store_path): Path<String>,
    Query(query): Query<GetDerivationQuery>,
) -> Result<Response> {
    match query.page.unwrap_or(GetDerivationPage::Info) {
        GetDerivationPage::Info => Ok(get_derivation_info(State(state), Path(store_path))
            .await?
            .into_response()),
        GetDerivationPage::Log => Ok(get_derivation_log(State(state), Path(store_path))
            .await?
            .into_response()),
    }
}

async fn get_derivation_info(
    State(state): State<Arc<AppState>>,
    Path(store_path): Path<String>,
) -> Result<Html<String>> {
    let in_path = PathBuf::from(NIX_STORE_PATH).join(&store_path);
    let in_path_str = in_path.to_str().expect("in_path contains invalid UTF-8");

    let mut store = DaemonStore::builder()
        .connect_unix(state.args.nix_daemon_sock.clone())
        .await?;

    // If this isn't a derivation (ends with ".drv"), resolve its deriver(s).
    let derivers = if in_path_str.ends_with(".drv") {
        vec![in_path_str.to_string()]
    } else {
        store.query_valid_derivers(in_path_str).result().await?
    };

    // Cache for seen derivations, build statuses and path validity.
    let mut drvs = BTreeMap::new();
    let mut drvs_built = BTreeMap::<String, bool>::new();
    let mut paths_valid = BTreeMap::<String, bool>::new();

    let mut roots: Vec<DerivationRootTemplate> = Vec::with_capacity(drvs.len());
    for path in derivers.iter() {
        if let Some(drv) = read_derivation(&mut drvs, path)? {
            let is_built =
                check_drv_built(&mut store, &mut paths_valid, &mut drvs_built, &path, &drv).await?;

            let mut outputs = Vec::with_capacity(drv.outputs.len());
            for (name, out) in drv.outputs.iter() {
                let valid = check_path_valid(&mut store, &mut paths_valid, &out.path).await?;
                outputs.push((name.clone(), out.path.clone(), valid))
            }

            let mut inputs_seen = BTreeSet::<String>::new();
            let input_tmpls = DerivationInputTemplate::build_batch(
                &mut store,
                &mut drvs,
                &mut paths_valid,
                &mut drvs_built,
                &mut inputs_seen,
                &drv.input_drvs,
            )
            .await?;

            let env_html = drv
                .env
                .iter()
                .map(|(key, value)| (key.clone(), env_value_to_html(&value.to_string())))
                .collect();

            // If nix-supervisor integration is enabled, ask if it knows about this derivation.
            let builds = if state.args.nix_supervisor {
                supervisor_api::Build::list(&state.args.nix_supervisor_addr, &path)
                    .await?
                    .builds
            } else {
                vec![]
            };

            roots.push(DerivationRootTemplate {
                path: path.clone(),
                drv: drv.clone(),
                is_built,
                outputs,
                input_tmpls,
                env_html,
                builds,
            })
        }
    }

    Ok(Html(
        DerivationTemplate {
            in_path: in_path_str,
            roots,
        }
        .render()?,
    ))
}

fn env_value_to_html(unescaped_value: &String) -> String {
    // HTML escape the value.
    let mut escaped = {
        use askama_escape::Escaper;
        let mut s = String::new();
        (askama_escape::Html {})
            .write_escaped(&mut s, &unescaped_value)
            .unwrap();
        s
    };

    // Link nix store paths.
    lazy_static::lazy_static! {
        static ref LINK_RE: regex::Regex = regex::Regex::new(
            r"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[0-9a-zA-Z+._?=-]+").unwrap();
    };
    escaped = LINK_RE
        .replace_all(&escaped, "<a href=\"$0\">$0</a>")
        .into();

    // Wrap multi-line values in <pre> tags.
    if escaped.contains("\n") {
        format!("<pre style=\"white-space: pre-wrap;\">{}</pre>", escaped)
    } else {
        escaped
    }
}

/// Reads a derivation from disk. Returns Ok(None) if the path doesn't exist or is invalid.
#[instrument(skip(drvs))]
pub fn read_derivation<'a>(
    drvs: &mut BTreeMap<String, Derivation>,
    path: &str,
) -> Result<Option<Derivation>> {
    if let Some(drv) = drvs.get(path) {
        Ok(Some(drv.clone()))
    } else {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                let drv = Derivation::parse(&data)?;
                drvs.insert(path.to_string(), drv.clone());
                Ok(Some(drv))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                warn!(path, "Attempted to read invalid path");
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }
}

#[instrument(skip(store, paths_valid))]
pub async fn check_path_valid<S: Store>(
    store: &mut S,
    paths_valid: &mut BTreeMap<String, bool>,
    path: &str,
) -> Result<bool>
where
    Error: From<S::Error>,
{
    if let Some(valid) = paths_valid.get(path) {
        Ok(*valid)
    } else {
        let valid = store.is_valid_path(path).result().await?;
        paths_valid.insert(path.to_string(), valid);
        Ok(valid)
    }
}

#[instrument(skip(store, paths_valid, drvs_built, drv))]
pub async fn check_drv_built<S: Store>(
    store: &mut S,
    paths_valid: &mut BTreeMap<String, bool>,
    drvs_built: &mut BTreeMap<String, bool>,
    path: &str,
    drv: &Derivation,
) -> Result<bool>
where
    Error: From<S::Error>,
{
    if let Some(status) = drvs_built.get(path) {
        Ok(*status)
    } else {
        let mut built = false;
        for (_, out) in drv.outputs.iter() {
            if check_path_valid(store, paths_valid, &out.path).await? {
                built = true;
                break;
            }
        }
        drvs_built.insert(path.to_string(), built);
        Ok(built)
    }
}

#[derive(Template)]
#[template(path = "derivation_log.html")]
struct DerivationLogTemplate {
    root_path: String,
    is_final: bool,
    log: String,
}

async fn get_derivation_log(
    State(state): State<Arc<AppState>>,
    Path(store_path): Path<String>,
) -> Result<DerivationLogTemplate> {
    let in_path = PathBuf::from(NIX_STORE_PATH).join(&store_path);
    let in_path_str = in_path.to_str().unwrap();

    let mut store = DaemonStore::builder()
        .connect_unix(state.args.nix_daemon_sock.clone())
        .await?;

    // If this isn't a derivation (ends with ".drv"), resolve its deriver(s).
    let root_path = if in_path_str.ends_with(".drv") {
        in_path_str.to_string()
    } else {
        store
            .query_valid_derivers(in_path_str)
            .result()
            .await?
            .drain(..)
            .next()
            .expect("No derivers??")
    };
    let (is_final, log) = read_derivation_log(&state.args, &root_path).await?;

    Ok(DerivationLogTemplate {
        root_path,
        is_final,
        log,
    })
}

#[derive(Debug, Serialize)]
struct GetAPIDerivationPollResponse {
    pub is_final: bool,
    pub log: String,
}
async fn get_api_derivation_poll(
    State(state): State<Arc<AppState>>,
    Path(drv): Path<String>,
) -> Result<Json<GetAPIDerivationPollResponse>> {
    let path_buf = PathBuf::from(NIX_STORE_PATH).join(&drv);
    let path = path_buf.to_str().unwrap();

    let (is_final, log) = read_derivation_log(&state.args, path).await?;
    Ok(Json(GetAPIDerivationPollResponse { is_final, log }))
}

#[instrument(skip(args))]
async fn read_derivation_log(args: &Args, path: &str) -> Result<(bool, String)> {
    // Arrested for Rust crimes.
    Ok(match nix_cli::log(path).await {
        Ok(log) if log.len() > 0 => {
            debug!("Got final log from nix");
            (true, log)
        }
        _ if args.nix_supervisor => {
            debug!("Querying nix-supervisor...");
            let log = supervisor_api::derivation_log(&args.nix_supervisor_addr, path).await?;
            (false, log)
        }
        _ => {
            debug!("No log, returning non-final blank");
            (false, String::new())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_value_to_html_nop() {
        assert_eq!(env_value_to_html(&"hello world".into()), "hello world");
    }

    #[test]
    fn test_env_value_to_html_sneaky() {
        assert_eq!(
            env_value_to_html(&"<blink>hello world</blink>".into()),
            "&lt;blink&gt;hello world&lt;/blink&gt;"
        );
    }

    #[test]
    fn test_env_value_to_html_multiline() {
        assert_eq!(
            env_value_to_html(&"hello\nworld".into()),
            "<pre style=\"white-space: pre-wrap;\">hello\nworld</pre>"
        );
    }

    #[test]
    fn test_env_value_to_html_store_path() {
        assert_eq!(
            env_value_to_html(&"/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin".into()),
            "<a href=\"/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin\">/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin</a>"
        );
    }

    #[test]
    fn test_env_value_to_html_store_path_subpath() {
        assert_eq!(
            env_value_to_html(&"/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin/bin/sqlite3".into()),
            "<a href=\"/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin\">/nix/store/ffffffffffffffffffffffffffffffff-sqlite-3.41.2-bin</a>/bin/sqlite3"
        );
    }
}
