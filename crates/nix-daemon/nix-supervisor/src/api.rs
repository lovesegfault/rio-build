// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use crate::entity;
use anyhow::{anyhow, Context};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use sea_orm::{prelude::*, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct JsonError {
            pub error: String,
            pub cause: Vec<String>,
        }

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(JsonError {
                error: format!("{}", self.0),
                cause: self.0.chain().map(|err| format!("{}", err)).collect(),
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    db: Arc<DatabaseConnection>,
}

pub(crate) fn new(db: Arc<DatabaseConnection>) -> Router<()> {
    Router::new()
        .route("/nix/store/:drv/builds", get(get_drv_builds))
        .route("/nix/store/:drv/log", get(get_drv_log))
        .route("/builds/:build_id", get(get_build))
        .with_state(Arc::new(AppState { db }))
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

#[derive(Debug, Serialize, Deserialize)]
struct GetDrvBuildsResponse {
    pub builds: Vec<entity::build::Model>,
}

async fn get_drv_builds(
    State(state): State<Arc<AppState>>,
    Path(drv): Path<String>,
) -> Result<Json<GetDrvBuildsResponse>> {
    let builds = entity::build::Entity::find()
        .filter(entity::build::Column::Name.eq(format!("/nix/store/{}", drv)))
        .order_by_desc(entity::build::Column::Start)
        .all(&*state.db)
        .await
        .with_context(|| format!("Querying Builds for: /nix/store/{}", drv))?;
    Ok(Json(GetDrvBuildsResponse { builds }))
}

#[derive(Debug, Serialize, Deserialize)]
struct GetBuildResponse {
    pub build: entity::build::Model,
}

async fn get_build(
    State(state): State<Arc<AppState>>,
    Path(build_id): Path<i32>,
) -> Result<Json<GetBuildResponse>> {
    Ok(Json(GetBuildResponse {
        build: entity::build::Entity::find_by_id(build_id)
            .one(&*state.db)
            .await
            .with_context(|| format!("Getting build #{}", build_id))?
            .ok_or_else(|| anyhow!("Build not found: #{}", build_id))?,
    }))
}

async fn get_drv_log(
    State(state): State<Arc<AppState>>,
    Path(drv): Path<String>,
) -> Result<impl IntoResponse> {
    let build = entity::build::Entity::find()
        .filter(entity::build::Column::Name.eq(format!("/nix/store/{}", drv)))
        .order_by_desc(entity::build::Column::Start)
        .limit(1)
        .one(&*state.db)
        .await
        .with_context(|| format!("Getting last build for: {}", drv))?
        .ok_or_else(|| anyhow!("No builds for: {}", drv))?;
    if let Some(path) = build.temp_log_path {
        let f = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("Couldn't open log for build #{}: {}", build.id, &path))?;
        Ok(axum::body::StreamBody::new(
            tokio_util::io::ReaderStream::new(f),
        ))
    } else {
        // The in-progress builds are deleted at the end of a build, which means requesting logs
        // for a build that's already finished will error. This is a bit surprising, but I'm not
        // sure there's a better solution here, unless we want a duplicate, per-build log store.
        Err(anyhow!("No logs for build #{}", build.id).into())
    }
}
