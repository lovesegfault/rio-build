// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::{collections::BTreeMap, fmt::Display};

use chrono::prelude::*;
use derive_more::{Constructor, Deref, DerefMut, Display};
use miette::Diagnostic;
use ownable::{
    traits::{IntoOwned, ToOwned},
    IntoOwned, ToOwned,
};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display as EnumDisplay, IntoStaticStr};
use url::Url;
use uuid::Uuid;
use yoke::Yokeable;

use crate::{models, Client, Request, Result, Yoke};

/// Percent encodes characters that are not safe for use in a URL segment.
fn percent_encode(s: &str) -> percent_encoding::PercentEncode {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_');
    percent_encoding::utf8_percent_encode(s, SET)
}

#[derive(Debug, EnumDisplay, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Model {
    Project,
    ProjectInput,
    Worker,
    Build,
    BuildInput,
    BuildTask,
    BuildEvent,
    BuildLog,
    Repository,
    Commit,
    RepositoryMergeRequest,
    RepositoryMergeRequestVersion,
    #[serde(untagged)]
    Unknown(String),
}

/// Wraps an ApiError in RFC 9457 ("Problem Details for HTTP APIs") format:
/// https://www.rfc-editor.org/rfc/rfc9457.html
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiErrorWrapper {
    pub status: u16,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub inner: ApiError,
}
impl From<ApiError> for ApiErrorWrapper {
    fn from(err: ApiError) -> Self {
        Self {
            status: err.status().as_u16(),
            title: err.to_string(),
            detail: err.help().map(|help| help.to_string()),
            inner: err,
        }
    }
}
impl From<ApiErrorWrapper> for ApiError {
    fn from(wrap: ApiErrorWrapper) -> Self {
        wrap.inner
    }
}

/// An error returned from the API (wrapped in an ApiErrorWrapper).
#[derive(Debug, Serialize, Deserialize, AsRefStr, IntoStaticStr, thiserror::Error, Diagnostic)]
#[serde(tag = "type")]
#[strum(serialize_all = "title_case")]
pub enum ApiError {
    #[serde(rename = "tag:IO")]
    #[diagnostic(code("gorgond:IO"))]
    #[error("IO Error")]
    IO {
        #[source]
        error: ApiErrorSource,
    },

    #[serde(rename = "tag:NoSuchRoute")]
    #[diagnostic(code("gorgond:NoSuchRoute"))]
    #[error("No such route: {route}")]
    NoSuchRoute { method: String, route: String },

    #[serde(rename = "tag:MethodNotAllowed")]
    #[diagnostic(code("gorgond:MethodNotAllowed"))]
    #[error("Method not allowed: {method} {route}")]
    MethodNotAllowed { method: String, route: String },

    #[serde(rename = "tag:BadRequestBody")]
    #[diagnostic(code("gorgond::BadRequestBody"))]
    #[error("Bad request body")]
    BadRequestBody {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:BadRequestJson")]
    #[diagnostic(code("gorgond::BadRequestJson"))]
    #[error("Bad request JSON")]
    BadRequestJson {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:BadUrl")]
    #[diagnostic(code("gorgond::BadUrl"))]
    #[error("Bad URL")]
    BadUrl {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:BadUuid")]
    #[diagnostic(code("gorgond::BadUuid"))]
    #[error("Bad UUID")]
    BadUuid {
        #[source]
        error: ApiErrorSource,
    },

    #[serde(rename = "tag:DatabaseNotFound")]
    #[diagnostic(code("gorgond::DatabaseNotFound"))]
    #[error("Database returned no rows")]
    DatabaseNotFound {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:DatabaseDuplicate")]
    #[diagnostic(code("gorgond::DatabaseDuplicate"))]
    #[error("Attempted to insert duplicate record")]
    DatabaseDuplicate {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:DatabaseConstraints")]
    #[diagnostic(code("gorgond::DatabaseConstraints"))]
    #[error("Database constraints violated")]
    DatabaseConstraints {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:DatabaseConnection")]
    #[diagnostic(code("gorgond::DatabaseConnection"))]
    #[error("Database connection error")]
    DatabaseConnection {
        #[source]
        error: ApiErrorSource,
    },
    #[serde(rename = "tag:DatabaseQuery")]
    #[diagnostic(code("gorgond::DatabaseQueryError"))]
    #[error("Database query failed")]
    DatabaseQueryError {
        #[source]
        error: ApiErrorSource,
    },

    #[serde(rename = "tag:ModelNotFound")]
    #[diagnostic(code("gorgond::ModelNotFound"))]
    #[error(fmt=Self::format_model_not_found)]
    ModelNotFound {
        model: Model,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        key: Option<String>,
    },
    #[serde(rename = "tag:NoWorkAvailable")]
    #[diagnostic(code("gorgond::NoWorkAvailable"))]
    #[error("No work available")]
    #[diagnostic(
        severity = "Warning",
        help = "Until automatic build creation is implemented, you have to create builds manually with `gorgonctl builds <project_slug> create`."
    )]
    NoWorkAvailable,

    #[serde(untagged)]
    #[diagnostic(code("gorgond::Unknown"))]
    #[error("unknown error type: {0}")]
    Unknown(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::IO { .. } | Self::BadUrl { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::NoSuchRoute { .. } => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            Self::BadRequestBody { .. } | Self::BadRequestJson { .. } | Self::BadUuid { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::DatabaseNotFound { .. } => StatusCode::NOT_FOUND,
            Self::DatabaseDuplicate { .. } | Self::DatabaseConstraints { .. } => {
                StatusCode::BAD_REQUEST
            }
            Self::DatabaseConnection { .. } | Self::DatabaseQueryError { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::ModelNotFound { .. } => StatusCode::NOT_FOUND,
            Self::NoWorkAvailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn io(err: std::io::Error) -> Self {
        Self::IO {
            error: Self::wrap_err(&err),
        }
    }

    pub fn wrap_err<E: std::error::Error + ?Sized>(err: &E) -> ApiErrorSource {
        ApiErrorSource {
            error: err.to_string(),
            source: err.source().map(|err| Box::new(Self::wrap_err(err))),
            ..Default::default()
        }
    }
    pub fn wrap<E: Diagnostic + ?Sized>(err: &E) -> ApiErrorSource {
        ApiErrorSource {
            error: err.to_string(),
            source: err
                .diagnostic_source()
                .map(|err| Box::new(Self::wrap(err)))
                .or_else(|| err.source().map(|err| Box::new(Self::wrap_err(err)))),
            code: err.code().map(|s| s.to_string()),
            severity: err.severity(),
            help: err.help().map(|s| s.to_string()),
            url: err.url().map(|s| s.to_string()),
            source_code: err.source_code().and_then(|src| {
                src.read_span(&miette::SourceSpan::new(0.into(), 0), 0, usize::MAX)
                    .map(|sc| String::from_utf8_lossy(sc.data()).to_string())
                    .ok()
            }),
            labels: err.labels().map(|it| it.collect()),
            related: err.related().map(|it| it.map(Self::wrap).collect()),
        }
    }

    fn format_model_not_found(
        model: &Model,
        key: &Option<String>,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        write!(f, "{model} not found")?;
        if let Some(key) = key {
            write!(f, ": {key}")?;
        }
        Ok(())
    }
}

impl From<url::ParseError> for ApiError {
    fn from(err: url::ParseError) -> Self {
        Self::BadUrl {
            error: Self::wrap_err(&err),
        }
    }
}
impl From<uuid::Error> for ApiError {
    fn from(err: uuid::Error) -> Self {
        Self::BadUuid {
            error: Self::wrap_err(&err),
        }
    }
}

#[cfg(feature = "diesel")]
impl From<diesel::result::Error> for ApiError {
    fn from(err: diesel::result::Error) -> Self {
        match err {
            diesel::result::Error::NotFound => Self::DatabaseNotFound {
                error: Self::wrap_err(&err),
            },
            diesel::result::Error::DatabaseError(kind, _) => match kind {
                diesel::result::DatabaseErrorKind::UniqueViolation => Self::DatabaseDuplicate {
                    error: Self::wrap_err(&err),
                },
                diesel::result::DatabaseErrorKind::ForeignKeyViolation
                | diesel::result::DatabaseErrorKind::NotNullViolation
                | diesel::result::DatabaseErrorKind::CheckViolation => Self::DatabaseConstraints {
                    error: Self::wrap_err(&err),
                },
                _ => Self::DatabaseQueryError {
                    error: Self::wrap_err(&err),
                },
            },
            err => Self::DatabaseConnection {
                error: Self::wrap_err(&err),
            },
        }
    }
}
#[cfg(feature = "diesel")]
impl From<diesel_async::pooled_connection::PoolError> for ApiError {
    fn from(err: diesel_async::pooled_connection::PoolError) -> Self {
        use diesel_async::pooled_connection::PoolError;
        match err {
            PoolError::ConnectionError(err) => Self::DatabaseConnection {
                error: Self::wrap_err(&err),
            },
            PoolError::QueryError(err) => match err {
                diesel::result::Error::NotFound => Self::DatabaseNotFound {
                    error: Self::wrap_err(&err),
                },
                _ => Self::DatabaseConnection {
                    error: Self::wrap_err(&err),
                },
            },
        }
    }
}
#[cfg(feature = "diesel")]
impl From<diesel_async::pooled_connection::deadpool::PoolError> for ApiError {
    fn from(err: diesel_async::pooled_connection::deadpool::PoolError) -> Self {
        use diesel_async::pooled_connection::deadpool::PoolError;
        match err {
            PoolError::Backend(err) => Self::from(err),
            err => Self::DatabaseConnection {
                error: Self::wrap_err(&err),
            },
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{error}")]
pub struct ApiErrorSource {
    pub error: String,

    #[source]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Box<ApiErrorSource>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<miette::Severity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<miette::LabeledSpan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related: Option<Vec<ApiErrorSource>>,
}

impl Diagnostic for ApiErrorSource {
    fn code<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.code
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.source.as_ref().and_then(|src| src.code()))
    }
    fn severity(&self) -> Option<miette::Severity> {
        self.severity
            .or_else(|| self.source.as_ref().and_then(|src| src.severity()))
    }
    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.help
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.source.as_ref().and_then(|src| src.help()))
    }
    fn url<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        self.url
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.source.as_ref().and_then(|src| src.url()))
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.source_code
            .as_ref()
            .map(|s| s as _)
            .or_else(|| self.source.as_ref().and_then(|src| src.source_code()))
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        let own_labels = self
            .labels
            .as_ref()
            .map(|v| Box::new(v.iter().cloned()) as _);
        let src_labels = self.source.as_ref().and_then(|src| src.labels());

        match (own_labels, src_labels) {
            (Some(a), Some(b)) => Some(Box::new(a.chain(b))),
            (Some(it), None) | (None, Some(it)) => Some(it),
            (None, None) => None,
        }
    }

    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn Diagnostic> + 'a>> {
        let own_related = self
            .related
            .as_ref()
            .map(|v| Box::new(v.iter().map(|err| err as &dyn Diagnostic)) as _);
        let src_related = self.source.as_ref().and_then(|src| src.related());

        match (own_related, src_related) {
            (Some(a), Some(b)) => Some(Box::new(a.chain(b))),
            (Some(it), None) | (None, Some(it)) => Some(it),
            (None, None) => None,
        }
    }
    fn diagnostic_source(&self) -> Option<&dyn Diagnostic> {
        None
    }
}

/// Workaround for BTreeMap<Uuid, V> not being ToOwned/IntoOwned.
#[derive(Debug, Clone, Serialize, Deserialize, Deref, DerefMut)]
pub struct OwnableMap<K: Copy + Ord, V: Clone>(BTreeMap<K, V>);

impl<K: Copy + Ord, V: Clone> Default for OwnableMap<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Copy + Ord, V: Clone> Extend<(K, V)> for OwnableMap<K, V> {
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        self.0.extend(iter)
    }
}

impl<K: Copy + Ord, V: Clone + ToOwned> ToOwned for OwnableMap<K, V>
where
    V::Owned: Clone,
{
    type Owned = OwnableMap<K, <V as ToOwned>::Owned>;
    #[inline]
    fn to_owned(&self) -> Self::Owned {
        OwnableMap::<K, <V as ToOwned>::Owned>(
            self.0.iter().map(|(k, v)| (*k, v.to_owned())).collect(),
        )
    }
}
impl<K: Copy + Ord, V: Clone + IntoOwned> IntoOwned for OwnableMap<K, V>
where
    V::Owned: Clone,
{
    type Owned = OwnableMap<K, <V as IntoOwned>::Owned>;
    #[inline]
    fn into_owned(self) -> Self::Owned {
        OwnableMap::<K, <V as IntoOwned>::Owned>(
            self.0
                .into_iter()
                .map(|(k, v)| (k, v.into_owned()))
                .collect(),
        )
    }
}

// -- Macros -- //

macro_rules! mkrequest {
    ($rsp:ident$(<$($rspgen:tt),*>)?, $req:ident$(<$($reqgen:tt),*>)? { $($body:tt)* }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
        pub struct $req$(<$($reqgen),*>)? { $($body)* }
        impl$(<$($reqgen),*>)? $crate::Request for $req$(<$($reqgen),*>)? {
            type Response = $rsp$(<$($rspgen),*>)?;
        }
    };
}
macro_rules! mkresponse {
    ($rsp:ident$(<$($rspgen:tt),*>)? { $($body:tt)* }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
        pub struct $rsp$(<$($rspgen),*>)? { $($body)* }
    };
}

macro_rules! mkmodelrequest_pair_old {
    ($field:ident, $req:ident, $rsp:ident<$rspmodel:ident>) => {
        mkrequest!($rsp<'static>, $req {});
        mkmodelresponse_pair_old!($rsp<$rspmodel>, $field);
    };
    ($field:ident, $req:ident<$reqmodel:ident>, $rsp:ident<$rspmodel:ident>) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
        pub struct $req<'a> {
            #[serde(borrow)]
            pub $field: models::$reqmodel<'a>,
        }
        impl<'a> $crate::Request for $req<'a> {
            type Response = $rsp<'static>;
        }
        mkmodelresponse_pair_old!($rsp<$rspmodel>, $field);
    };
}
macro_rules! mkmodelresponse_pair_old {
    ($rsp:ident<$rspmodel:ident>, $field:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
        pub struct $rsp<'a> {
            #[serde(borrow)]
            pub $field: models::$rspmodel<'a>,
        }
    };
}

macro_rules! gen_endpoints {
    ($path:expr, ($($arg:ident),*), {
        $(fn $ident:ident($req:ty) -> ($method:ident, $endpoint:literal);)*
    }) => {
        fn _path(&self, endpoint: &str) -> String {
            match endpoint {
                "" => format!($path, $($arg = self.$arg),*),
                _ => format!("{}/{endpoint}", format_args!($path, $($arg = self.$arg),*)),
            }
        }

        $(pub async fn $ident(&self, req: $req) -> Result<Yoke<<$req as Request>::Response>> {
            self.client.call(Method::$method, self._path($endpoint), req).await
        })*
    };
}

// -- Root -- //

mkresponse!(RootResponse {});
mkrequest!(RootResponse, RootRequest {});

// -- Repositories -- //

mkresponse!(RepositoryResponse<'a> {
    #[serde(borrow)]
    pub repository: models::Repository<'a>,
});
mkrequest!(RepositoryResponse<'static>, RepositoryGetRequest {});
mkrequest!(RepositoryResponse<'static>, RepositoryCreateRequest<'a> {
    #[serde(borrow)]
    pub repository: models::RepositorySpec<'a>,
});

mkresponse!(RepositoriesResponse<'a> {
    #[serde(borrow)]
    pub repositories: Vec<models::Repository<'a>>,
});
mkrequest!(RepositoriesResponse<'static>, RepositoriesListRequest {});

#[derive(Debug, Constructor)]
pub struct RepositoriesClient<'a> {
    client: &'a Client,
}
impl<'a> RepositoriesClient<'a> {
    pub fn id(&self, id: Uuid) -> RepositoryClient<'a> {
        RepositoryClient::new(self.client, RepositoryPath::Id(id))
    }
    pub fn kind(&self, kind: &'a str) -> RepositoriesKindClient<'a> {
        RepositoriesKindClient::new(self.client, kind)
    }
    gen_endpoints!("repos", (), {
        fn list(RepositoriesListRequest) -> (GET, "");
        fn create(RepositoryCreateRequest<'_>) -> (POST, "");
    });
}

#[derive(Debug, Constructor)]
pub struct RepositoriesKindClient<'a> {
    client: &'a Client,
    repo_kind: &'a str,
}
impl<'a> RepositoriesKindClient<'a> {
    pub fn ident(&self, ident: &'a str) -> RepositoryClient<'a> {
        RepositoryClient::new(self.client, RepositoryPath::Ident(self.repo_kind, ident))
    }
    gen_endpoints!("repos/{repo_kind}", (repo_kind), {
        fn list(RepositoriesListRequest) -> (GET, "");
    });
}

#[derive(Debug, Display)]
pub enum RepositoryPath<'a> {
    #[display("-/{_0}")]
    Id(Uuid),
    #[display("{_0}/{}", percent_encode(_1))]
    Ident(&'a str, &'a str),
}
#[derive(Debug, Constructor)]
pub struct RepositoryClient<'a> {
    client: &'a Client,
    repo_path: RepositoryPath<'a>,
}
impl RepositoryClient<'_> {
    gen_endpoints!("repos/{repo_path}", (repo_path), {
        fn get(RepositoryGetRequest) -> (GET, "");
    });
}

// -- Projects -- //

mkmodelrequest_pair_old!(project, ProjectGetRequest, ProjectGetResponse<Project>);
mkmodelrequest_pair_old!(
    project,
    ProjectCreateRequest<ProjectSpec>,
    ProjectCreateResponse<Project>
);

#[derive(Debug, Constructor)]
pub struct ProjectsClient<'a> {
    client: &'a Client,
}
impl<'a> ProjectsClient<'a> {
    pub fn slug(&self, slug: &'a str) -> ProjectClient<'a> {
        ProjectClient::new(self.client, slug)
    }
    gen_endpoints!("projects", (), {
        fn create(ProjectCreateRequest<'_>) -> (POST, "");
    });
}

#[derive(Debug, Constructor)]
pub struct ProjectClient<'a> {
    client: &'a Client,
    project_slug: &'a str,
}
impl<'a> ProjectClient<'a> {
    pub fn builds(&self) -> ProjectsBuildsClient<'a> {
        ProjectsBuildsClient::new(self.client, self.project_slug)
    }
    gen_endpoints!("projects/{project_slug}", (project_slug), {
        fn get(ProjectGetRequest) -> (GET, "");
    });
}

mkmodelrequest_pair_old!(
    build,
    ProjectBuildCreateRequest<BuildSpec>,
    ProjectBuildCreateResponse<Build>
);

mkrequest!(ProjectBuildsResponse<'static>, ProjectBuildsRequest {});
mkresponse!(ProjectBuildsResponse<'a> {
    #[serde(borrow)]
    pub project: models::Project<'a>,
    #[serde(borrow)]
    pub builds: Vec<models::Build<'a>>,
});

#[derive(Debug, Constructor)]
pub struct ProjectsBuildsClient<'a> {
    client: &'a Client,
    project_slug: &'a str,
}
impl ProjectsBuildsClient<'_> {
    gen_endpoints!("projects/{project_slug}/builds", (project_slug), {
        fn list(ProjectBuildsRequest) -> (GET, "");
        fn create(ProjectBuildCreateRequest<'_>) -> (POST, "");
    });
}

// -- Builds -- //

mkrequest!(BuildsResponse<'static>, BuildsRequest {});
mkresponse!(BuildsResponse<'a> {
    #[serde(borrow)]
    pub workers: OwnableMap<Uuid, models::Worker<'a>>,
    #[serde(borrow)]
    pub projects: OwnableMap<Uuid, models::Project<'a>>,
    #[serde(borrow)]
    pub builds: Vec<models::Build<'a>>,
});

#[derive(Debug, Constructor)]
pub struct BuildsClient<'a> {
    client: &'a Client,
}
impl<'a> BuildsClient<'a> {
    pub fn id(&self, id: Uuid) -> BuildClient<'a> {
        BuildClient::new(self.client, id)
    }
    gen_endpoints!("builds", (), {
        fn list(BuildsRequest) -> (GET, "");
    });
}

mkrequest!(BuildResponse<'static>, BuildRequest {});
mkresponse!(BuildResponse<'a> {
    #[serde(borrow)]
    pub worker: Option<models::Worker<'a>>,
    #[serde(borrow)]
    pub project: models::Project<'a>,
    #[serde(borrow)]
    pub build: models::Build<'a>,
});

mkrequest!(BuildEventsResponse, BuildEventsRequest {});
mkresponse!(BuildEventsResponse {
    #[ownable(clone)]
    pub url: Url,
});

#[derive(Debug, Constructor)]
pub struct BuildClient<'a> {
    client: &'a Client,
    build_id: Uuid,
}
impl<'a> BuildClient<'a> {
    pub fn internal(&self) -> BuildInternalClient<'a> {
        BuildInternalClient::new(self.client, self.build_id)
    }
    gen_endpoints!("builds/{build_id}", (build_id), {
        fn get(BuildRequest) -> (GET, "");
        fn events(BuildEventsRequest) -> (GET, "events");
    });
}

mkrequest!(BuildInternalHeartbeatResponse, BuildInternalHeartbeatRequest {
    #[ownable(clone)]
    pub started_at: DateTime<Utc>,
    #[ownable(clone)]
    pub heartbeat_at: DateTime<Utc>,
});
mkresponse!(BuildInternalHeartbeatResponse {});

mkrequest!(BuildInternalEndResponse, BuildInternalEndRequest {
    #[ownable(clone)]
    pub started_at: DateTime<Utc>,
    #[ownable(clone)]
    pub ended_at: DateTime<Utc>,
    pub result: models::BuildResult,
});
mkresponse!(BuildInternalEndResponse {});

#[derive(Debug, Constructor)]
pub struct BuildInternalClient<'a> {
    client: &'a Client,
    build_id: Uuid,
}
impl BuildInternalClient<'_> {
    gen_endpoints!("builds/{build_id}/internal", (build_id), {
        fn end(BuildInternalEndRequest) -> (PUT, "end");
        fn heartbeat(BuildInternalHeartbeatRequest) -> (PUT, "heartbeat");
    });
}

// -- Workers -- //

#[derive(Debug, Constructor)]
pub struct WorkersClient<'a> {
    client: &'a Client,
}
impl<'a> WorkersClient<'a> {
    pub fn slug(&self, slug: &'a str) -> WorkerClient<'a> {
        WorkerClient::new(self.client, slug)
    }
}

#[derive(Debug, Constructor)]
pub struct WorkerClient<'a> {
    client: &'a Client,
    worker_slug: &'a str,
}
impl WorkerClient<'_> {
    pub fn internal(&self) -> WorkerInternalClient<'_> {
        WorkerInternalClient::new(self.client, self.worker_slug)
    }
}

mkrequest!(
    WorkerInternalAllocateBuildResponse<'static>,
    WorkerInternalAllocateBuildRequest {}
);
mkresponse!(WorkerInternalAllocateBuildResponse<'a> {
    #[serde(borrow)]
    pub build: models::Build<'a>,
});

#[derive(Debug, Constructor)]
pub struct WorkerInternalClient<'a> {
    client: &'a Client,
    worker_slug: &'a str,
}
impl WorkerInternalClient<'_> {
    gen_endpoints!("workers/{worker_slug}/internal", (worker_slug), {
        fn allocate_build(WorkerInternalAllocateBuildRequest) -> (PUT, "allocate-build");
    });
}
