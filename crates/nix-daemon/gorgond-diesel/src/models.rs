// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::collections::BTreeMap;

use chrono::prelude::*;
use diesel::{
    Selectable,
    dsl::{self, auto_type},
    pg::Pg,
    prelude::*,
};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use diesel_derive_enum::DbEnum;
use gorgond_client::{api, models as client};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yoke::Yokeable;

use crate::schema::gorgond as schema;

macro_rules! gen_model {
    ($schema:ident, $model:ident) => {
        pub fn not_found(key: impl Into<String>) -> api::ApiError {
            api::ApiError::ModelNotFound {
                model: api::Model::$model,
                key: Some(key.into()),
            }
        }

        pub fn all() -> dsl::Select<schema::$schema::table, dsl::AsSelect<Self, Pg>> {
            schema::$schema::table.select(Self::as_select())
        }

        #[auto_type(no_type_alias)]
        pub fn ids<I: IntoIterator<Item = Uuid>>(ids: I) -> _ {
            let q: dsl::Select<schema::$schema::table, dsl::AsSelect<Self, Pg>> = Self::all();
            let it: I::IntoIter = ids.into_iter();
            q.filter(schema::$schema::id.eq_any(it))
        }

        pub async fn get(conn: &mut AsyncPgConnection, id: Uuid) -> Result<Self, api::ApiError> {
            Self::all()
                .find(id)
                .first(conn)
                .await
                .optional()?
                .ok_or_else(|| Self::not_found(id))
        }
    };
}
macro_rules! gen_model_with_ident_slug {
    ($schema:ident) => {
        #[auto_type(no_type_alias)]
        pub fn with_ident(ident: &str) -> _ {
            let id: Uuid = Uuid::parse_str(&ident).unwrap_or_default();
            let q: dsl::Select<schema::$schema::table, dsl::AsSelect<Self, Pg>> = Self::all();
            q.find(id).or_filter(schema::$schema::slug.eq(ident))
        }

        pub fn not_found_ident(ident: &str) -> api::ApiError {
            Self::not_found(ident)
        }

        gen_model_with_ident_helpers!($schema, slug as &str);
    };
}
macro_rules! gen_model_with_ident_helpers {
    ($schema:ident, $($arg:ident as $t:ty),*) => {
            pub async fn get_ident(
                conn: &mut AsyncPgConnection,
                $($arg: $t),*
            ) -> Result<Self, api::ApiError> {
                Self::with_ident($($arg),*)
                    .first(conn)
                    .await
                    .optional()?
                    .ok_or_else(|| Self::not_found_ident($($arg),*))
            }
            pub async fn get_id(
                conn: &mut AsyncPgConnection,
                $($arg: $t),*
            ) -> Result<Uuid, api::ApiError> {
                Self::with_ident($($arg),*)
                    .select(schema::$schema::id)
                    .first(conn)
                    .await
                    .optional()?
                    .ok_or_else(|| Self::not_found_ident($($arg),*))
            }
    }
}

#[derive(
    Debug, Serialize, Deserialize, Yokeable, Queryable, Selectable, Insertable, Identifiable,
)]
#[diesel(table_name = crate::schema::gorgond::projects, check_for_backend(diesel::pg::Pg))]
pub struct Project {
    pub id: Uuid,
    pub slug: String,
    pub name: String,

    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Project {
    gen_model!(projects, Project);
    gen_model_with_ident_slug!(projects);

    pub fn from_spec(v: client::ProjectSpec<'static>, id: Uuid, now: DateTime<Utc>) -> Self {
        Self {
            id,
            slug: v.slug.into(),
            name: v.name.into(),
            created_at: now,
            updated_at: now,
        }
    }
    pub fn to_client(self) -> client::Project<'static> {
        client::Project {
            id: self.id,
            spec: client::ProjectSpec {
                slug: self.slug.into(),
                name: self.name.into(),
            },
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::project_inputs, check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(Project))]
pub struct ProjectInput {
    pub id: Uuid,
    pub project_id: Uuid,
    pub key: String,

    pub source: String,
    pub repository_id: Option<Uuid>,

    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl ProjectInput {
    gen_model!(project_inputs, ProjectInput);
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::project_input_commits)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(primary_key(input_id, commit_id))]
#[diesel(belongs_to(ProjectInput, foreign_key = input_id))]
pub struct ProjectInputCommit {
    pub input_id: Uuid,
    pub commit_id: Uuid,
    pub result: bool,

    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    QueryableByName,
    Selectable,
    Insertable,
    Identifiable,
    AsChangeset,
)]
#[diesel(table_name = crate::schema::gorgond::workers, check_for_backend(diesel::pg::Pg))]
pub struct Worker {
    pub id: Uuid,
    pub slug: String,
    pub name: String,

    pub seen_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Worker {
    gen_model!(workers, Worker);
    gen_model_with_ident_slug!(workers);

    pub fn to_client(self) -> client::Worker<'static> {
        client::Worker {
            id: self.id,
            spec: client::WorkerSpec {
                slug: self.slug.into(),
                name: self.name.into(),
            },
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, DbEnum)]
#[db_enum(existing_type_path = "crate::schema::gorgond::sql_types::BuildResult")]
pub enum BuildResult {
    Unknown,
    Success,
    Failure,
}
impl From<client::BuildResult> for BuildResult {
    fn from(v: client::BuildResult) -> Self {
        match v {
            client::BuildResult::Unknown => Self::Unknown,
            client::BuildResult::Success => Self::Success,
            client::BuildResult::Failure => Self::Failure,
        }
    }
}
impl From<BuildResult> for client::BuildResult {
    fn from(v: BuildResult) -> Self {
        match v {
            BuildResult::Unknown => Self::Unknown,
            BuildResult::Success => Self::Success,
            BuildResult::Failure => Self::Failure,
        }
    }
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::build_inputs, check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(Build))]
pub struct BuildInput {
    pub id: Uuid,
    pub build_id: Uuid,
    pub key: String,
    pub source: String,
}
impl BuildInput {
    gen_model!(build_inputs, BuildInput);
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::builds, check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(Project), belongs_to(Worker))]
pub struct Build {
    pub id: Uuid,
    pub project_id: Uuid,
    pub kind: String,
    pub expr: String,

    pub worker_id: Option<Uuid>,
    pub result: Option<BuildResult>,

    pub allocated_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Build {
    gen_model!(builds, Build);

    pub fn from_spec(
        v: client::BuildSpec<'static>,
        id: Uuid,
        project_id: Uuid,
        now: DateTime<Utc>,
    ) -> (Self, impl Iterator<Item = BuildInput>) {
        (
            Self {
                id,
                project_id,
                kind: v.kind.into(),
                expr: v.expr.into(),
                worker_id: None,
                result: None,
                created_at: now,
                updated_at: now,
                allocated_at: None,
                heartbeat_at: None,
                started_at: None,
                ended_at: None,
            },
            v.inputs.into_iter().map(move |(k, v)| BuildInput {
                id: Uuid::now_v7(),
                build_id: id,
                key: k.into(),
                source: v.into(),
            }),
        )
    }
    pub fn to_client(self, inputs: impl IntoIterator<Item = BuildInput>) -> client::Build<'static> {
        client::Build {
            id: self.id,
            project_id: self.project_id,
            spec: client::BuildSpec {
                kind: self.kind.into(),
                inputs: BTreeMap::from_iter(
                    inputs
                        .into_iter()
                        .map(|input| (input.key.into(), input.source.into())),
                ),
                expr: self.expr.into(),
            },
            worker_id: self.worker_id,
            result: self.result.map(|v| v.into()),
            created_at: self.created_at,
            updated_at: self.updated_at,
            allocated_at: self.allocated_at,
            heartbeat_at: self.heartbeat_at,
            started_at: self.started_at,
            ended_at: self.ended_at,
        }
    }
}

#[derive(
    Debug, Serialize, Deserialize, Yokeable, Queryable, Selectable, Insertable, Identifiable,
)]
#[diesel(table_name = crate::schema::gorgond::repositories, check_for_backend(diesel::pg::Pg))]
pub struct Repository {
    pub id: Uuid,
    pub kind: String,
    pub ident: String,

    pub storage: String,

    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Repository {
    gen_model!(repositories, Repository);
    gen_model_with_ident_helpers!(repositories, kind as &str, ident as &str);

    #[auto_type(no_type_alias)]
    pub fn with_ident<'a>(kind: &'a str, ident: &'a str) -> _ {
        let id: Uuid = (kind == "-")
            .then(|| Uuid::parse_str(ident).unwrap_or_default())
            .unwrap_or_default();
        let q: dsl::Select<schema::repositories::table, dsl::AsSelect<Self, Pg>> = Self::all();
        q.find(id).or_filter(
            (schema::repositories::kind.eq(kind)).and(schema::repositories::ident.eq(ident)),
        )
    }
    pub fn not_found_ident(kind: &str, ident: &str) -> api::ApiError {
        Self::not_found(format!("{kind}:{ident}"))
    }

    pub fn from_spec(v: client::RepositorySpec<'static>, id: Uuid, now: DateTime<Utc>) -> Self {
        Self {
            id,
            kind: v.kind.into(),
            ident: v.ident.into(),
            storage: v.storage.into(),
            created_at: now,
            updated_at: now,
        }
    }
    pub fn to_client(self) -> client::Repository<'static> {
        client::Repository {
            id: self.id,
            spec: client::RepositorySpec {
                kind: self.kind.into(),
                ident: self.ident.into(),
                storage: self.storage.into(),
            },
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::commits, check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(Repository))]
pub struct Commit {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub hash: Vec<u8>,

    pub created_at: DateTime<Utc>,
}
impl Commit {
    gen_model!(commits, Commit);
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::repository_merge_requests)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(Repository))]
pub struct RepositoryMergeRequest {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub ident: String,

    pub is_open: bool,
    pub title: String,
    pub description: String,

    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl RepositoryMergeRequest {
    gen_model!(repository_merge_requests, RepositoryMergeRequest);
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Yokeable,
    Queryable,
    Selectable,
    Insertable,
    Identifiable,
    Associations,
)]
#[diesel(table_name = crate::schema::gorgond::repository_merge_request_versions)]
#[diesel(check_for_backend(diesel::pg::Pg))]
#[diesel(belongs_to(RepositoryMergeRequest, foreign_key = merge_request_id))]
pub struct RepositoryMergeRequestVersion {
    pub id: Uuid,
    pub merge_request_id: Uuid,
    pub source: String,
    pub commit_id: Option<Uuid>,

    pub obsolete_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl RepositoryMergeRequestVersion {
    gen_model!(
        repository_merge_request_versions,
        RepositoryMergeRequestVersion
    );
}
