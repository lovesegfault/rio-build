// SPDX-FileCopyrightText: 2023, 2025 Alyssa Ross <hi@alyssa.is>
// SPDX-License-Identifier: EUPL-1.2+

use std::collections::BTreeSet;
use std::io::{stderr, Write};

use curl::easy::{Easy, List};
use thiserror::Error;
use tracing::debug;
use url::Url;

use crate::{get_link, Event, EventError};

const USER_AGENT: &str = "gorgon-github-events";

pub struct Response {
    pub events: BTreeSet<Event>,
    pub next_rel: Option<Url>,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("curl error: {0}")]
    Curl(#[from] curl::Error),
    #[error("error reading event: {0}")]
    InvalidEvent(#[from] EventError),
    #[error("got HTTP {0} response code")]
    HttpUnsuccessful(u32),
}

pub struct Client {
    buf: Vec<u8>,
    easy: Easy,
    next_rel: Option<Url>,
}

impl Client {
    pub fn new(token: &Option<String>) -> Result<Self, curl::Error> {
        let mut easy = Easy::new();
        easy.useragent(USER_AGENT)?;

        if let Some(token) = token {
            let mut headers = List::new();
            headers.append(&format!("Authorization: Bearer {token}"))?;
            easy.http_headers(headers)?;
        }

        Ok(Self {
            buf: vec![],
            easy,
            next_rel: None,
        })
    }

    pub fn get(&mut self, url: &Url, next_rel: &[u8]) -> Result<Response, ClientError> {
        self.buf.clear();

        self.easy.url(url.as_str())?;

        let mut transfer = self.easy.transfer();

        transfer.header_function(|header| {
            let Some(colon_index) = header.iter().position(|&byte| byte == b':') else {
                return true;
            };

            let (name, mut value) = header.split_at(colon_index + 1);
            if name.eq_ignore_ascii_case(b"link:") {
                if value.ends_with(b"\r\n") {
                    value = &value[..value.len() - 2]
                }

                self.next_rel = get_link(next_rel, &mut value, url);
            }
            true
        })?;

        transfer.write_function(|data| {
            self.buf.write_all(data).unwrap();
            Ok(data.len())
        })?;

        debug!("GET {url}");

        transfer.perform()?;
        drop(transfer);

        let response_code = self.easy.response_code()?;
        debug!("{response_code} {url}");
        if !matches!(response_code, 200..=299) {
            let _ = stderr().write_all(&self.buf);
            return Err(ClientError::HttpUnsuccessful(response_code));
        }

        Ok(Response {
            events: serde_json::from_slice::<Vec<_>>(&self.buf)
                .unwrap()
                .into_iter()
                .map(Event::new)
                .collect::<Result<_, _>>()?,
            next_rel: self.next_rel.take(),
        })
    }
}
