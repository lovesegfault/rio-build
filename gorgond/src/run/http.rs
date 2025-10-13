// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::fmt::Debug;

use axum::{
    extract::{FromRequest, Path, State},
    response::IntoResponse,
    routing::{get, post, put},
    Router,
};
use axum_extra::response::ErasedJson;
use chrono::prelude::*;
use diesel::prelude::*;
use diesel_async::{
    pooled_connection::deadpool::Pool, AsyncConnection, AsyncPgConnection, RunQueryDsl,
};
use futures::stream::TryStreamExt;
use gorgond_client::{
    api::{self, ApiError},
    Url, Uuid, Yoke, Yokeable,
};
use gorgond_diesel::{models, schema};
use miette::{Context, Diagnostic, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

pub type Result<T, E = ApiErrorAdapter> = std::result::Result<T, E>;

#[derive(Debug, Clone, clap::Parser)]
#[clap(next_help_heading = "HTTP Server")]
pub struct HttpArgs {
    /// Listen address for the HTTP server.
    #[arg(long, short = 'H', default_value = "localhost:9999")]
    http_addr: String,
}

#[derive(Clone)]
pub struct S {
    pub pool: Pool<AsyncPgConnection>,
    pub events_base: Url,
}

#[instrument(name = "http", level = "ERROR", skip_all)]
pub async fn run(token: CancellationToken, args: HttpArgs, state: S) -> miette::Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/repos", get(repos).post(repo_create))
        .route("/repos/{kind}", get(repos_by_kind))
        .route("/repos/{kind}/{ident}", get(repo))
        .route("/projects", post(project_create))
        .route("/projects/{project}", get(project))
        .route(
            "/projects/{project}/builds",
            get(project_builds).post(project_build_create),
        )
        .route("/builds", get(builds))
        .route("/builds/{build_id}", get(build))
        .route("/builds/{build_id}/events", get(build_events))
        .route(
            "/builds/{build_id}/internal/heartbeat",
            put(build_internal_heartbeat),
        )
        .route("/builds/{build_id}/internal/end", put(build_internal_end))
        .route(
            "/workers/{worker}/internal/allocate-build",
            put(worker_internal_allocate_build),
        )
        .fallback(error_not_found)
        .method_not_allowed_fallback(error_method_not_allowed)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    debug!(http_addr = args.http_addr, "Binding HTTP listener...");
    let listener = TcpListener::bind(&args.http_addr)
        .await
        .into_diagnostic()
        .context("Couldn't bind socket")?;
    let addr = listener
        .local_addr()
        .into_diagnostic()
        .context("Couldn't get local address")?;
    info!("Running on: http://{}/", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(token.cancelled_owned())
        .await
        .into_diagnostic()
}

#[derive(Debug, thiserror::Error, Diagnostic)]
#[error(transparent)]
#[diagnostic(transparent)]
pub struct ApiErrorAdapter(ApiError);
impl IntoResponse for ApiErrorAdapter {
    fn into_response(self) -> axum::response::Response {
        (
            self.0.status(),
            ErasedJson::pretty(api::ApiErrorWrapper::from(self.0)),
        )
            .into_response()
    }
}
impl<E: Into<ApiError>> From<E> for ApiErrorAdapter {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[derive(Debug)]
pub struct JsonRequest<T: for<'y> Yokeable<'y, Output: Debug + Deserialize<'y>>>(pub Yoke<T>);

impl<T: for<'y> Yokeable<'y, Output: Debug + Deserialize<'y>>, S: Send + Sync> FromRequest<S>
    for JsonRequest<T>
{
    type Rejection = ApiErrorAdapter;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(Yoke::try_attach_to_cart(
            String::from_request(req, state)
                .await
                .map_err(|err| ApiError::BadRequestBody {
                    error: ApiError::wrap_err(&err),
                })?,
            |src| {
                serde_json::from_str(src).map_err(|err| ApiError::BadRequestJson {
                    error: ApiError::wrap_err(&err),
                })
            },
        )?))
    }
}

#[derive(Debug)]
pub struct JsonResponse<T: Debug + Serialize>(pub T);
impl<T: Debug + Serialize> IntoResponse for JsonResponse<T> {
    fn into_response(self) -> axum::response::Response {
        ErasedJson::pretty(self.0).into_response()
    }
}

macro_rules! transaction {
    ($state:ident, $block:expr) => {
        $state
            .pool
            .get()
            .await?
            .transaction(move |conn| Box::pin(async move { ($block)(conn).await }))
    };
}

async fn error_not_found(req: axum::extract::Request) -> Result<()> {
    Err(ApiErrorAdapter::from(ApiError::NoSuchRoute {
        method: req.method().to_string(),
        route: req.uri().path().to_string(),
    }))
}

async fn error_method_not_allowed(req: axum::extract::Request) -> Result<()> {
    Err(ApiErrorAdapter::from(ApiError::MethodNotAllowed {
        method: req.method().to_string(),
        route: req.uri().path().to_string(),
    }))
}

async fn root() -> JsonResponse<api::RootResponse> {
    JsonResponse(api::RootResponse {})
}

async fn repos(State(state): State<S>) -> Result<JsonResponse<api::RepositoriesResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        Ok(JsonResponse(api::RepositoriesResponse {
            repositories: models::Repository::all()
                .load_stream(conn)
                .await?
                .map_ok(|v| v.to_client())
                .try_collect()
                .await?,
        }))
    })
    .await
}

async fn repos_by_kind(
    State(state): State<S>,
    Path(repo_kind): Path<String>,
) -> Result<JsonResponse<api::RepositoriesResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        Ok(JsonResponse(api::RepositoriesResponse {
            repositories: models::Repository::all()
                .filter(schema::gorgond::repositories::kind.eq(repo_kind))
                .load_stream(conn)
                .await?
                .map_ok(|v| v.to_client())
                .try_collect()
                .await?,
        }))
    })
    .await
}

async fn repo(
    State(state): State<S>,
    Path((repo_kind, repo_ident)): Path<(String, String)>,
) -> Result<JsonResponse<api::RepositoryResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        Ok(JsonResponse(api::RepositoryResponse {
            repository: models::Repository::get_ident(conn, &repo_kind, &repo_ident)
                .await?
                .to_client(),
        }))
    })
    .await
}

async fn repo_create(
    State(state): State<S>,
    JsonRequest(req): JsonRequest<api::RepositoryCreateRequest<'static>>,
) -> Result<JsonResponse<api::RepositoryResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let now = Utc::now();
        let repo =
            models::Repository::from_spec(req.get().repository.to_owned(), Uuid::now_v7(), now);
        let repo = {
            diesel::insert_into(schema::gorgond::repositories::table)
                .values(&repo)
                .get_result::<models::Repository>(conn)
                .await?
        };
        Ok(JsonResponse(api::RepositoryResponse {
            repository: repo.to_client(),
        }))
    })
    .await
}

async fn project_create(
    State(state): State<S>,
    JsonRequest(req): JsonRequest<api::ProjectCreateRequest<'static>>,
) -> Result<JsonResponse<api::ProjectCreateResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let now = Utc::now();
        let project = models::Project::from_spec(req.get().project.to_owned(), Uuid::now_v7(), now);
        let project = {
            diesel::insert_into(schema::gorgond::projects::table)
                .values(&project)
                .get_result::<models::Project>(conn)
                .await?
        };
        Ok(JsonResponse(api::ProjectCreateResponse {
            project: project.to_client(),
        }))
    })
    .await
}

async fn project(
    State(state): State<S>,
    Path(project): Path<String>,
) -> Result<JsonResponse<api::ProjectGetResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        Ok(JsonResponse(api::ProjectGetResponse {
            project: models::Project::get_ident(conn, &project)
                .await?
                .to_client(),
        }))
    })
    .await
}

async fn project_builds(
    State(state): State<S>,
    Path(project): Path<String>,
) -> Result<JsonResponse<api::ProjectBuildsResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let project = models::Project::get_ident(conn, &project).await?;
        let builds = models::Build::belonging_to(&project)
            .load::<models::Build>(conn)
            .await?;
        let build_inputs = models::BuildInput::belonging_to(&builds)
            .load::<models::BuildInput>(conn)
            .await?
            .grouped_by(&builds);
        Ok(JsonResponse(api::ProjectBuildsResponse {
            project: project.to_client(),
            builds: builds
                .into_iter()
                .zip(build_inputs)
                .map(|(b, bi)| b.to_client(bi))
                .collect(),
        }))
    })
    .await
}

async fn project_build_create(
    State(state): State<S>,
    Path(project): Path<String>,
    JsonRequest(req): JsonRequest<api::ProjectBuildCreateRequest<'static>>,
) -> Result<JsonResponse<api::ProjectBuildCreateResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let now = Utc::now();
        let project_id = models::Project::get_id(conn, &project).await?;

        let (build, inputs) =
            models::Build::from_spec(req.get().build.to_owned(), Uuid::now_v7(), project_id, now);
        let build = {
            use schema::gorgond::builds;
            diesel::insert_into(builds::table)
                .values(&build)
                .get_result::<models::Build>(conn)
                .await?
        };
        let inputs = {
            use schema::gorgond::build_inputs;
            diesel::insert_into(build_inputs::table)
                .values(inputs.collect::<Vec<_>>())
                .get_results::<models::BuildInput>(conn)
                .await?
        };
        Ok(JsonResponse(api::ProjectBuildCreateResponse {
            build: build.to_client(inputs),
        }))
    })
    .await
}

async fn builds(State(state): State<S>) -> Result<JsonResponse<api::BuildsResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let builds = models::Build::all().get_results(conn).await?;
        let build_inputs = models::BuildInput::belonging_to(&builds)
            .load::<models::BuildInput>(conn)
            .await?
            .grouped_by(&builds);

        Ok(JsonResponse(api::BuildsResponse {
            workers: models::Worker::ids(builds.iter().filter_map(|b| b.worker_id))
                .load_stream(conn)
                .await?
                .map_ok(|w| (w.id, w.to_client()))
                .try_collect()
                .await?,
            projects: models::Project::ids(builds.iter().map(|b| b.project_id))
                .load_stream(conn)
                .await?
                .map_ok(|p| (p.id, p.to_client()))
                .try_collect()
                .await?,
            builds: builds
                .into_iter()
                .zip(build_inputs)
                .map(|(b, bi)| b.to_client(bi))
                .collect(),
        }))
    })
    .await
}

async fn build(
    State(state): State<S>,
    Path(build_id): Path<Uuid>,
) -> Result<JsonResponse<api::BuildResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        let build = models::Build::get(conn, build_id).await?;
        let build_inputs = models::BuildInput::belonging_to(&build)
            .load::<models::BuildInput>(conn)
            .await?;

        Ok(JsonResponse(api::BuildResponse {
            worker: match build.worker_id {
                Some(worker_id) => Some(models::Worker::get(conn, worker_id).await?.to_client()),
                None => None,
            },
            project: models::Project::get(conn, build.project_id)
                .await?
                .to_client(),
            build: build.to_client(build_inputs),
        }))
    })
    .await
}

async fn build_events(
    State(state): State<S>,
    Path(build_id): Path<Uuid>,
) -> Result<JsonResponse<api::BuildEventsResponse>> {
    Ok(JsonResponse(api::BuildEventsResponse {
        url: state.events_base.join(&format!("{build_id}.cbor.zst"))?,
    }))
}

async fn build_internal_heartbeat(
    State(state): State<S>,
    Path(build_id): Path<Uuid>,
    JsonRequest(req): JsonRequest<api::BuildInternalHeartbeatRequest>,
) -> Result<JsonResponse<api::BuildInternalHeartbeatResponse>> {
    let req = req.get();
    transaction!(state, async |conn: &mut _| {
        diesel::update(schema::gorgond::builds::table)
            .filter(schema::gorgond::builds::id.eq(build_id))
            .set((
                schema::gorgond::builds::started_at.eq(req.started_at),
                schema::gorgond::builds::heartbeat_at.eq(req.heartbeat_at),
            ))
            .execute(conn)
            .await?;

        Ok(JsonResponse(api::BuildInternalHeartbeatResponse {}))
    })
    .await
}
async fn build_internal_end(
    State(state): State<S>,
    Path(build_id): Path<Uuid>,
    JsonRequest(req): JsonRequest<api::BuildInternalEndRequest>,
) -> Result<JsonResponse<api::BuildInternalEndResponse>> {
    let req = req.get();
    transaction!(state, async |conn: &mut _| {
        diesel::update(schema::gorgond::builds::table)
            .filter(schema::gorgond::builds::id.eq(build_id))
            .set((
                schema::gorgond::builds::started_at.eq(req.started_at),
                schema::gorgond::builds::ended_at.eq(req.ended_at),
                schema::gorgond::builds::result.eq(models::BuildResult::from(req.result)),
            ))
            .execute(conn)
            .await?;

        Ok(JsonResponse(api::BuildInternalEndResponse {}))
    })
    .await
}

async fn worker_internal_allocate_build(
    State(state): State<S>,
    Path(worker): Path<String>,
    JsonRequest(_req): JsonRequest<api::WorkerInternalAllocateBuildRequest>,
) -> Result<JsonResponse<api::WorkerInternalAllocateBuildResponse<'static>>> {
    transaction!(state, async |conn: &mut _| {
        use schema::gorgond::{build_inputs, builds, workers};
        let now = Utc::now();

        // Resolve the worker slug to an ID, or create it if it doesn't exist.
        let worker_id = {
            let default_worker = models::Worker {
                id: Uuid::now_v7(),
                slug: worker.clone(),
                name: worker.clone(),
                created_at: now,
                updated_at: now,
                seen_at: Some(now),
            };
            // Without *a lot* of persuading, Postgres returns nothing to a query with
            // ON CONFLICT DO NOTHING and no changes, so we bump seen_at here instead.
            let worker_id = diesel::insert_into(workers::table)
                .values(&default_worker)
                .on_conflict(workers::slug)
                .do_update()
                .set(workers::seen_at.eq(now))
                .returning(workers::id)
                .get_result::<Uuid>(conn)
                .await?;
            if worker_id == default_worker.id {
                info!("New worker node registered!");
            }
            worker_id
        };

        // If worker has an unfinished task, remind it to get back on it.
        // This is also our idempotency mechanism for this endpoint.
        let build = if let Some(build) = models::Build::all()
            .filter(builds::result.is_null())
            .filter(builds::worker_id.eq(worker_id))
            .first::<models::Build>(conn)
            .await
            .optional()?
        {
            info!("Worker has an unfinished build, reminding it");
            build
        } else {
            // If not, try to find an unallocated task...
            let build_id = if let Some(build_id) = builds::table
                .filter(builds::worker_id.is_null())
                .select(builds::id)
                .limit(1)
                .order_by(builds::created_at.asc())
                .for_no_key_update() // No race conditions pls.
                .get_result::<Uuid>(conn)
                .await
                .optional()?
            {
                info!("Found an unallocated build, allocating it");
                build_id
            } else {
                warn!("No work available!");
                return Err(ApiError::NoWorkAvailable.into());
            };

            // ...and allocate it to this worker.
            debug!("Allocating build...");
            diesel::update(builds::table)
                .filter(builds::id.eq(build_id))
                .set(builds::worker_id.eq(worker_id))
                .returning(models::Build::as_returning())
                .get_result(conn)
                .await?
        };

        let inputs = build_inputs::table
            .filter(build_inputs::build_id.eq(build.id))
            .get_results::<models::BuildInput>(conn)
            .await?;
        Ok(JsonResponse(api::WorkerInternalAllocateBuildResponse {
            build: build.to_client(inputs),
        }))
    })
    .await
}
