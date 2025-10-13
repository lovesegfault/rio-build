// SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
// SPDX-License-Identifier: EUPL-1.2+

use std::cmp::Ordering;
use std::fmt::{self, Display, Formatter};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EventError {
    #[error("missing id in {0}")]
    InvalidId(serde_json::Value),
    #[error("missing created_at in {0}")]
    InvalidCreatedAt(serde_json::Value),
}

pub struct Event(serde_json::Value);

impl Event {
    pub fn new(inner: serde_json::Value) -> Result<Self, EventError> {
        if inner
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            Err(EventError::InvalidId(inner))
        } else if inner
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            Err(EventError::InvalidCreatedAt(inner))
        } else {
            Ok(Self(inner))
        }
    }

    pub fn id(&self) -> &str {
        self.0.get("id").unwrap().as_str().unwrap()
    }

    pub fn created_at(&self) -> &str {
        self.0.get("created_at").unwrap().as_str().unwrap()
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Eq for Event {}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl Ord for Event {
    fn cmp(&self, other: &Self) -> Ordering {
        self.created_at().cmp(other.created_at())
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
