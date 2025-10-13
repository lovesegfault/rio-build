// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::borrow::Cow;

use chrono::prelude::*;
use minicbor::{Decode, Encode};
use ownable::{IntoOwned, ToOwned};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, IntoStaticStr};
use uuid::Uuid;
use yoke::Yokeable;

use crate::models;

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned, Encode, Decode)]
pub struct BuildEvent<'a> {
    #[cbor(n(0), with = "crate::cbor::timestamp")]
    #[ownable(clone)]
    pub timestamp: DateTime<Utc>,
    #[cbor(n(1), with = "crate::cbor::uuid", has_nil)]
    #[ownable(clone)]
    pub build_id: Uuid,
    #[cbor(n(2), with = "crate::cbor::uuid", has_nil)]
    #[ownable(clone)]
    pub event_id: Uuid,
    #[b(3)]
    #[serde(borrow)]
    pub event: BuildEventKind<'a>,
}
impl<'a> From<BuildEventKind<'a>> for BuildEvent<'a> {
    fn from(event: BuildEventKind<'a>) -> Self {
        Self {
            timestamp: Utc::now(),
            build_id: Uuid::nil(),
            event_id: Uuid::now_v7(),
            event,
        }
    }
}
impl<'a> From<BuildTaskEvent<'a>> for BuildEvent<'a> {
    fn from(event: BuildTaskEvent<'a>) -> Self {
        Self {
            timestamp: Utc::now(),
            build_id: Uuid::nil(),
            event_id: Uuid::now_v7(),
            event: BuildEventKind::Task(event),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToOwned, IntoOwned, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum BuildEventKind<'a> {
    #[n(0)]
    Started,
    #[n(1)]
    Stopped(#[n(0)] models::BuildResult),
    #[n(2)]
    #[serde(borrow)]
    Task(#[b(0)] BuildTaskEvent<'a>),
    #[n(3)]
    #[serde(borrow)]
    Log(#[b(0)] Log<'a>),
}
impl BuildEventKind<'_> {
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::Task(BuildTaskEvent {
                event: BuildTaskEventKind::Updated(BuildTaskUpdate::Progress { .. }),
                ..
            })
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned, Encode, Decode)]
pub struct BuildTaskEvent<'a> {
    #[cbor(n(0), with = "crate::cbor::uuid", has_nil)]
    #[ownable(clone)]
    pub task_id: Uuid,
    #[b(1)]
    #[serde(borrow)]
    pub event: BuildTaskEventKind<'a>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToOwned, IntoOwned, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum BuildTaskEventKind<'a> {
    #[n(0)]
    Started(#[b(0)] models::BuildTask<'a>),
    #[n(1)]
    Stopped,
    #[n(2)]
    #[serde(borrow)]
    Updated(#[b(0)] BuildTaskUpdate<'a>),
    #[n(3)]
    #[serde(borrow)]
    Log(#[b(0)] Log<'a>),
}

#[derive(Debug, Clone, Serialize, Deserialize, ToOwned, IntoOwned, Encode, Decode)]
#[serde(rename_all = "snake_case")]
pub enum BuildTaskUpdate<'a> {
    #[n(0)]
    FileLinked {
        #[n(0)]
        bytes: u32,
    },
    #[n(1)]
    UntrustedPath,
    #[n(2)]
    CorruptedPath,
    #[n(3)]
    SetPhase {
        #[b(0)]
        phase: Cow<'a, str>,
    },
    #[n(4)]
    Progress {
        #[n(0)]
        done: u32,
        #[n(1)]
        expected: u32,
        #[n(2)]
        running: u32,
        #[n(3)]
        failed: u32,
    },
}

#[derive(
    Debug,
    Display,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    EnumString,
    AsRefStr,
    IntoStaticStr,
    ToOwned,
    IntoOwned,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    #[default]
    #[n(0)]
    Build,
    #[n(1)]
    PostBuild,
    #[n(2)]
    Message,
}
impl LogKind {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(
    Debug, Default, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned, Encode, Decode,
)]
pub struct Log<'a> {
    #[n(0)]
    pub level: models::BuildTaskLevel,
    #[n(1)]
    #[serde(skip_serializing_if = "LogKind::is_default", default)]
    pub kind: LogKind,
    #[b(2)]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub file: Option<Cow<'a, str>>,
    #[n(3)]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub column: Option<u32>,
    #[n(4)]
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<u32>,
    #[b(5)]
    pub text: Cow<'a, str>,
}
