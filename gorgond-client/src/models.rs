// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use chrono::prelude::*;
use minicbor::{Decode, Encode};
use ownable::{IntoOwned, ToOwned};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeMap, fmt::Debug};
use strum::{AsRefStr, Display, EnumString, IntoStaticStr};
use uuid::Uuid;
use yoke::Yokeable;

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct ProjectSpec<'a> {
    #[serde(borrow)]
    pub slug: Cow<'a, str>,
    #[serde(borrow)]
    pub name: Cow<'a, str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct Project<'a> {
    #[ownable(clone)]
    pub id: Uuid,
    #[serde(flatten, borrow)]
    pub spec: ProjectSpec<'a>,

    #[ownable(clone)]
    pub created_at: DateTime<Utc>,
    #[ownable(clone)]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct WorkerSpec<'a> {
    #[serde(borrow)]
    pub slug: Cow<'a, str>,
    #[serde(borrow)]
    pub name: Cow<'a, str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct Worker<'a> {
    #[ownable(clone)]
    pub id: Uuid,
    #[serde(flatten, borrow)]
    pub spec: WorkerSpec<'a>,

    #[ownable(clone)]
    pub created_at: DateTime<Utc>,
    #[ownable(clone)]
    pub updated_at: DateTime<Utc>,
}

#[derive(
    Debug, Display, Clone, Copy, Serialize, Deserialize, ToOwned, IntoOwned, Encode, Decode,
)]
#[serde(rename_all = "snake_case")]
pub enum BuildResult {
    #[n(0)]
    Unknown,
    #[n(1)]
    Success,
    #[n(2)]
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct BuildSpec<'a> {
    #[serde(borrow)]
    pub kind: Cow<'a, str>,
    #[serde(borrow)]
    pub inputs: BTreeMap<Cow<'a, str>, Cow<'a, str>>,
    #[serde(borrow)]
    pub expr: Cow<'a, str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct Build<'a> {
    #[ownable(clone)]
    pub id: Uuid,
    #[ownable(clone)]
    pub project_id: Uuid,
    #[serde(flatten, borrow)]
    pub spec: BuildSpec<'a>,

    #[ownable(clone)]
    pub worker_id: Option<Uuid>,
    pub result: Option<BuildResult>,

    #[ownable(clone)]
    pub created_at: DateTime<Utc>,
    #[ownable(clone)]
    pub updated_at: DateTime<Utc>,
    #[ownable(clone)]
    pub allocated_at: Option<DateTime<Utc>>,
    #[ownable(clone)]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[ownable(clone)]
    pub started_at: Option<DateTime<Utc>>,
    #[ownable(clone)]
    pub ended_at: Option<DateTime<Utc>>,
}

/// Verbosity levels for a Task, including all of Nix' levels.
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
pub enum BuildTaskLevel {
    #[n(0)]
    #[serde(rename = "E")]
    #[strum(serialize = "E")]
    Error,
    #[n(1)]
    #[serde(rename = "W")]
    #[strum(serialize = "W")]
    Warn,
    #[n(2)]
    #[serde(rename = "N")]
    #[strum(serialize = "N")]
    Notice,
    #[n(3)]
    #[serde(rename = "I")]
    #[strum(serialize = "I")]
    #[default]
    Info,
    #[n(4)]
    #[serde(rename = "T")]
    #[strum(serialize = "T")]
    Talkative,
    #[n(5)]
    #[serde(rename = "C")]
    #[strum(serialize = "C")]
    Chatty,
    #[n(6)]
    #[serde(rename = "D")]
    #[strum(serialize = "D")]
    Debug,
    #[n(7)]
    #[serde(rename = "V")]
    #[strum(serialize = "V")]
    Vomit,
}
impl From<BuildTaskLevel> for tracing::Level {
    fn from(level: BuildTaskLevel) -> Self {
        match level {
            BuildTaskLevel::Error => tracing::Level::ERROR,
            BuildTaskLevel::Warn => tracing::Level::WARN,
            BuildTaskLevel::Notice | BuildTaskLevel::Info => tracing::Level::INFO,
            BuildTaskLevel::Talkative | BuildTaskLevel::Chatty | BuildTaskLevel::Debug => {
                tracing::Level::DEBUG
            }
            BuildTaskLevel::Vomit => tracing::Level::TRACE,
        }
    }
}

/// Every possible kind of task.
#[derive(
    Debug,
    Display,
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
pub enum BuildTaskKind {
    #[n(0)]
    Helper,
    #[n(1)]
    Realise,
    #[n(2)]
    Builds,
    #[n(3)]
    Build,
    #[n(4)]
    PostBuildHook,
    #[n(5)]
    BuildWaiting,
    #[n(6)]
    Substitute,
    #[n(7)]
    CopyPaths,
    #[n(8)]
    CopyPath,
    #[n(9)]
    FileTransfer,
    #[n(10)]
    OptimiseStore,
    #[n(11)]
    VerifyPaths,
    #[n(12)]
    QueryPathInfo,
}

/// A task performed as part of a Build.
#[derive(
    Debug, Default, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned, Encode, Decode,
)]
pub struct BuildTask<'a> {
    #[ownable(clone)]
    #[cbor(n(0), with = "crate::cbor::uuid", has_nil)]
    pub id: Uuid,
    #[ownable(clone)]
    #[cbor(n(1), with = "crate::cbor::uuid::opt", has_nil)]
    pub parent_id: Option<Uuid>,
    #[ownable(clone)]
    #[cbor(n(2), with = "crate::cbor::uuid", has_nil)]
    pub build_id: Uuid,

    #[b(3)]
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    #[n(4)]
    pub kind: Option<BuildTaskKind>,
    #[n(5)]
    pub level: BuildTaskLevel,

    #[ownable(clone)]
    #[cbor(n(6), with = "crate::cbor::timestamp", has_nil)]
    pub started_at: DateTime<Utc>,
    #[ownable(clone)]
    #[cbor(n(7), with = "crate::cbor::timestamp::opt", has_nil)]
    pub ended_at: Option<DateTime<Utc>>,
    #[ownable(clone)]
    #[cbor(n(8), with = "crate::cbor::timestamp::opt", has_nil)]
    pub created_at: Option<DateTime<Utc>>,
    #[ownable(clone)]
    #[cbor(n(9), with = "crate::cbor::timestamp::opt", has_nil)]
    pub updated_at: Option<DateTime<Utc>>,
}
impl<'a> BuildTask<'a> {
    pub fn borrowed(&'a self) -> Self {
        Self {
            id: self.id,
            parent_id: self.parent_id,
            build_id: self.build_id,
            name: Cow::Borrowed(&self.name),
            kind: self.kind,
            level: self.level,
            created_at: self.created_at,
            updated_at: self.updated_at,
            started_at: self.started_at,
            ended_at: self.ended_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct RepositorySpec<'a> {
    #[serde(borrow)]
    pub kind: Cow<'a, str>,
    #[serde(borrow)]
    pub ident: Cow<'a, str>,
    #[serde(borrow)]
    pub storage: Cow<'a, str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Yokeable, ToOwned, IntoOwned)]
pub struct Repository<'a> {
    #[ownable(clone)]
    pub id: Uuid,
    #[serde(flatten, borrow)]
    pub spec: RepositorySpec<'a>,

    #[ownable(clone)]
    pub created_at: DateTime<Utc>,
    #[ownable(clone)]
    pub updated_at: DateTime<Utc>,
}
