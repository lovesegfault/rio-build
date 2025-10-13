// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use derive_more::{Constructor, Unwrap};
use miette::{Diagnostic, NamedSource, SourceOffset, SourceSpan};
use reqwest::{header, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display};
use std::future::Future;
use url::Url;
use yoke::Yokeable;

use crate::{api::ApiError, Client, Yoke};

#[macro_export]
macro_rules! mkrequest {
    ($path:literal, $req:ty, $rsp:ty) => {
        impl $crate::Request for $req {
            type Response = $rsp;
            fn endpoint(&self, base: &Url) -> Result<(Method, Url), url::ParseError> {
                Ok((Method::PUT, base.join($path)?))
            }
        }
    };
}

#[derive(Debug, thiserror::Error, Diagnostic, Constructor)]
#[diagnostic(forward(kind))]
pub struct RequestError {
    pub endpoint: Option<(Method, Url)>,
    #[source]
    pub kind: RequestErrorKind,
}
impl Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.endpoint {
            Some((method, url)) => write!(f, "{} {}", method, url),
            None => write!(f, "Invalid endpoint"),
        }
    }
}

#[derive(Debug, thiserror::Error, Diagnostic, Unwrap)]
pub enum RequestErrorKind {
    #[error("Bad URL")]
    Url(#[from] url::ParseError),
    #[error("Request Failed")]
    Request(#[from] reqwest::Error),
    #[error("HTTP {}: {}", .0.status(), .0.status().canonical_reason().unwrap_or("???") )]
    #[diagnostic(transparent)]
    Api(#[from] ApiError),
    #[error("JSON Error")]
    Json(
        #[label] SourceSpan,
        #[source_code] NamedSource<String>,
        #[source] serde_json::Error,
    ),
}

pub trait Request: Serialize + Debug {
    type Response: for<'y> Yokeable<'y>;

    fn endpoint(&self, base: &Url) -> Result<(Method, Url), url::ParseError>;

    fn send(
        &self,
        client: &Client,
    ) -> impl Future<Output = Result<Yoke<Self::Response>, RequestError>> + Send
    where
        for<'y> <Self::Response as Yokeable<'y>>::Output: Deserialize<'y>,
    {
        // Serialize self outside the future, this avoids needing to send &Self between threads.
        let endpoint_r = self.endpoint(&client.base_url);
        let body_r = serde_json::to_string_pretty(&self).map_err(|err| {
            let src = format!("{self:#?}");
            RequestErrorKind::Json(
                SourceSpan::new(0.into(), src.len()),
                NamedSource::new(std::any::type_name_of_val(self), src),
                err,
            )
        });

        // Then go async.
        async move {
            let (method, url) = endpoint_r.map_err(|err| RequestError::new(None, err.into()))?;

            // Shut up, clippy. This closure exists so that the inner one can return a
            // RequestErrorKind, and the outer one can draw the rest of the RequestError.
            #[allow(clippy::redundant_closure_call)]
            (async |method: &Method, url: &Url| -> Result<Yoke<Self::Response>, RequestErrorKind> {
                let req = client
                    .client
                    .request(method.clone(), url.clone())
                    .body(body_r?)
                    .header(header::CONTENT_TYPE, "application/json")
                    .build()?;
                let rsp = client.client.execute(req).await?;
                let status = rsp.status();
                let body = rsp.text().await?;

                fn parse<'de, T: Deserialize<'de>>(
                    url: &Url,
                    body: &'de str,
                ) -> Result<T, RequestErrorKind> {
                    serde_json::from_str(body).map_err(|err| {
                        RequestErrorKind::Json(
                            SourceSpan::new(
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
                    StatusCode::OK => Yoke::try_attach_to_cart(body, |body| parse(url, body)),
                    _ => Err(RequestErrorKind::Api(parse(url, &body)?)),
                }
            })(&method, &url)
            .await
            .map_err(|err| RequestError::new(Some((method, url)), err))
        }
    }
}
