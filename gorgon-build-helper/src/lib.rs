// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::fmt;

use gorgond_client::events;
use miette::Diagnostic;
use serde::{Deserialize, Serialize};

pub mod fetcher;
pub mod helper;

static mut EVENT_HANDLER: &dyn EventHandler = &();

/// # SAFETY
/// This is safe to call up until events start being produced.
pub unsafe fn set_event_handler(handler: &'static dyn EventHandler) {
    EVENT_HANDLER = handler
}
pub fn event_handler() -> &'static dyn EventHandler {
    // SAFETY: This is only ever assigned by the set_event_handler() function.
    unsafe { EVENT_HANDLER }
}

pub trait EventHandler {
    fn handle(&self, event: Event);
}
impl EventHandler for () {
    fn handle(&self, _: Event) {}
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event<'a> {
    #[serde(borrow)]
    Build(events::BuildEvent<'a>),
    Error(SerializedError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Diagnostic, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SerializedErrorSource {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Error(SerializedError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Diagnostic(SerializedError),
}

#[derive(Debug, Default, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{error}")]
pub struct SerializedError {
    pub error: String,

    #[source]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Box<SerializedErrorSource>>,

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
    pub related: Option<Vec<SerializedError>>,
}

impl SerializedError {
    pub fn new_error<E: std::error::Error + ?Sized>(err: &E) -> Self {
        Self {
            error: err.to_string(),
            source: err
                .source()
                .map(|err| Box::new(SerializedErrorSource::Error(Self::new_error(err)))),
            ..Default::default()
        }
    }

    pub fn new<E: Diagnostic + ?Sized>(err: &E) -> Self {
        Self {
            error: err.to_string(),
            source: err
                .diagnostic_source()
                .map(|err| Box::new(SerializedErrorSource::Diagnostic(Self::new(err))))
                .or_else(|| {
                    err.source()
                        .map(|err| Box::new(SerializedErrorSource::Error(Self::new_error(err))))
                }),
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
            related: err.related().map(|it| it.map(Self::new).collect()),
        }
    }

    fn transparent(&self) -> Option<&Self> {
        self.source.as_ref().map(|src| match &**src {
            SerializedErrorSource::Error(err) | SerializedErrorSource::Diagnostic(err) => err,
        })
    }
}

impl Diagnostic for SerializedError {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.code
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.transparent().as_ref().and_then(|src| src.code()))
    }
    fn severity(&self) -> Option<miette::Severity> {
        self.severity
            .or_else(|| self.transparent().as_ref().and_then(|src| src.severity()))
    }
    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.help
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.transparent().as_ref().and_then(|src| src.help()))
    }
    fn url<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.url
            .as_ref()
            .map(|s| Box::new(s) as _)
            .or_else(|| self.transparent().as_ref().and_then(|src| src.url()))
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        self.source_code.as_ref().map(|s| s as _).or_else(|| {
            self.transparent()
                .as_ref()
                .and_then(|src| src.source_code())
        })
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        let own_labels = self
            .labels
            .as_ref()
            .map(|v| Box::new(v.iter().cloned()) as _);
        let src_labels = self.transparent().and_then(|src| src.labels());

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
        let src_related = self.transparent().and_then(|src| src.related());

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
