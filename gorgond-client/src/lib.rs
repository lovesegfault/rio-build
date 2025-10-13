// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
// SPDX-License-Identifier: EUPL-1.2

// Clippy really hates crate::Error, and will warn for *everything that returns one*. Jeez, chill.
#![allow(clippy::result_large_err)]

use derive_more::{Deref, Unwrap};
use miette::{Diagnostic, NamedSource, SourceOffset, SourceSpan};
use reqwest::{header, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, future::Future};

pub use url::Url;
pub use uuid::Uuid;
pub use yoke::Yokeable;

pub mod api;
pub mod cbor;
pub mod events;
pub mod models;

pub type Result<T, E = Error> = std::result::Result<T, E>;
pub type Yoke<T, C = String> = yoke::Yoke<T, C>;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, thiserror::Error, Diagnostic, Unwrap)]
pub enum Error {
    #[error("Invalid URL")]
    ParseUrl(#[from] url::ParseError),
    #[error("Failed to build HTTP client")]
    BuildClient(#[source] reqwest::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Request(RequestError),
}

#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("{method} {url}")]
#[diagnostic(forward(err))]
pub struct RequestError {
    pub method: Method,
    pub url: Url,
    #[source]
    pub err: RequestErrorKind,
}

#[derive(Debug, thiserror::Error, Diagnostic, Unwrap)]
pub enum RequestErrorKind {
    #[error("Request Failed")]
    Reqwest(#[from] reqwest::Error),
    #[error("HTTP {}", .0.status())]
    #[diagnostic(transparent)]
    Api(#[from] api::ApiError),
    #[error("JSON Error")]
    Json(
        #[label] SourceSpan,
        #[source_code] NamedSource<String>,
        #[source] serde_json::Error,
    ),
    #[error("Error serializing query string")]
    SerializeQuery(#[from] serde_urlencoded::ser::Error),
}

pub trait Request: Serialize + Debug {
    type Response: for<'y> Yokeable<'y, Output: Deserialize<'y>>;
}

fn call<R: Request>(
    client: &reqwest::Client,
    method: Method,
    mut url: Url,
    req: R,
) -> impl Future<Output = Result<Yoke<R::Response>, Error>> + Send {
    // reqwest::Client already has an internal Arc.
    let client = client.clone();

    // Serialize outside the future, avoiding the need to send references across threads.
    // For GET requests, we serialize req into a query string, else a JSON body.
    let body_r = match method {
        Method::GET => serde_urlencoded::to_string(&req).map_err(RequestErrorKind::SerializeQuery),
        _ => serde_json::to_string_pretty(&req).map_err(|err| {
            let src = format!("{req:#?}");
            RequestErrorKind::Json(
                SourceSpan::new(0.into(), src.len()),
                NamedSource::new(std::any::type_name_of_val(&req), src),
                err,
            )
        }),
    };

    // Then go async.
    async move {
        // Inner closure returns a RequestErrorKind, outer makes it a proper RequestError.
        async {
            let req = match method {
                Method::GET => {
                    url.set_query(Some(&body_r?));
                    client.request(method.clone(), url.clone())
                }
                _ => client
                    .request(method.clone(), url.clone())
                    .body(body_r?)
                    .header(header::CONTENT_TYPE, "application/json"),
            }
            .build()?;
            let rsp = client.execute(req).await?;
            let status = rsp.status();
            let body = rsp.text().await?;

            fn parse<'de, T: Deserialize<'de>>(
                url: &Url,
                body: &'de str,
            ) -> Result<T, RequestErrorKind> {
                serde_json::from_str(body).map_err(|err| {
                    RequestErrorKind::Json(
                        SourceSpan::new(
                            // Convert line + column to position, rest of line is length.
                            // This works specifically because the API returns pretty JSON.
                            SourceOffset::from_location(body, err.line(), err.column()),
                            body.lines()
                                .nth(err.line() - 1)
                                .map(|line| line.len() - err.column())
                                .unwrap_or(0),
                        ),
                        NamedSource::new(url, body.to_string()).with_language("JSON"),
                        err,
                    )
                })
            }
            match status {
                StatusCode::OK => Yoke::try_attach_to_cart(body, |body| parse(&url, body)),
                _ => Err(RequestErrorKind::Api(parse(&url, &body)?)),
            }
        }
        .await
        .map_err(|err| Error::Request(RequestError { method, url, err }))
    }
}

#[derive(Debug, Clone, Deref)]
pub struct Client {
    #[deref]
    client: reqwest::Client,
    base_url: Url,
}

impl Client {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, Error> {
        Self::new_with(
            reqwest::Client::builder()
                .connection_verbose(true)
                .build()
                .map_err(Error::BuildClient)?,
            base_url,
        )
    }

    pub fn new_with(client: reqwest::Client, base_url: impl AsRef<str>) -> Result<Self, Error> {
        Ok(Self {
            client,
            base_url: Url::parse(base_url.as_ref())?,
        })
    }

    async fn call<R: Request>(
        &self,
        method: Method,
        path: impl AsRef<str>,
        req: R,
    ) -> Result<Yoke<R::Response>, Error> {
        call(
            &self.client,
            method,
            self.base_url.join(path.as_ref())?,
            req,
        )
        .await
    }

    pub async fn root(&self, req: api::RootRequest) -> Result<Yoke<api::RootResponse>> {
        call(&self.client, Method::GET, self.base_url.clone(), req).await
    }

    pub fn repositories(&self) -> api::RepositoriesClient {
        api::RepositoriesClient::new(self)
    }
    pub fn projects(&self) -> api::ProjectsClient {
        api::ProjectsClient::new(self)
    }
    pub fn builds(&self) -> api::BuildsClient {
        api::BuildsClient::new(self)
    }
    pub fn workers(&self) -> api::WorkersClient {
        api::WorkersClient::new(self)
    }
}
