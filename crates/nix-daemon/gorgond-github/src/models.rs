// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use chrono::prelude::*;
use diesel::prelude::*;
use diesel_derive_enum::DbEnum;
use gorgond_client::Uuid;
use miette::{Context, IntoDiagnostic, Result, miette};
use octocrab::models::events;
use strum::AsRefStr;

fn sign<T: TryFrom<u64>>(id: &u64) -> Result<T>
where
    <T as TryFrom<u64>>::Error: std::error::Error + Send + Sync + 'static,
{
    (*id).try_into().into_diagnostic()
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::repos, check_for_backend(diesel::pg::Pg))]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub repository_id: Option<Uuid>,

    pub polled_at: Option<DateTime<Utc>>,
    pub current_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Repo {
    pub fn try_from_event(
        now: DateTime<Utc>,
        current_at: DateTime<Utc>,
        r: &events::Repository,
    ) -> Result<Self> {
        Ok(Self {
            id: sign(&r.id).context("Invalid ID")?,
            name: r.name.clone(),
            repository_id: None,
            polled_at: None,
            current_at: Some(current_at),
            updated_at: now,
            created_at: now,
        })
    }
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::actors, check_for_backend(diesel::pg::Pg))]
pub struct Actor {
    pub id: i64,
    pub login: String,
    pub display_login: String,
    pub avatar_url: String,

    pub current_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Actor {
    pub fn try_from_event(
        now: DateTime<Utc>,
        current_at: DateTime<Utc>,
        a: &events::Actor,
    ) -> Result<Self> {
        Ok(Self {
            id: sign(&a.id).context("Invalid ID")?,
            login: a.login.clone(),
            display_login: a.display_login.clone(),
            avatar_url: a.avatar_url.to_string(),
            current_at: Some(current_at),
            updated_at: now,
            created_at: now,
        })
    }
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::orgs, check_for_backend(diesel::pg::Pg))]
pub struct Org {
    pub id: i64,
    pub login: String,
    pub avatar_url: String,

    pub current_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Org {
    pub fn try_from_event(
        now: DateTime<Utc>,
        current_at: DateTime<Utc>,
        o: &events::Org,
    ) -> Result<Self> {
        Ok(Self {
            id: sign(&o.id).context("Invalid ID")?,
            login: o.login.clone(),
            avatar_url: o.avatar_url.to_string(),
            current_at: Some(current_at),
            updated_at: now,
            created_at: now,
        })
    }
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::pull_requests, check_for_backend(diesel::pg::Pg))]
pub struct PullRequest {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub ref_name: String,
    pub merge_request_id: Option<Uuid>,

    pub current_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl PullRequest {
    pub fn try_from_event(
        now: DateTime<Utc>,
        current_at: DateTime<Utc>,
        repo_id: i64,
        pr: &octocrab::models::pulls::PullRequest,
    ) -> Result<Self> {
        Ok(Self {
            id: sign(&pr.id).context("Invalid ID")?,
            repo_id,
            number: sign(&pr.number).context("Invalid PR Number")?,
            ref_name: pr.head.ref_field.clone(),
            merge_request_id: None,
            current_at: Some(current_at),
            updated_at: now,
            created_at: now,
        })
    }
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::pushes, check_for_backend(diesel::pg::Pg))]
pub struct Push {
    pub id: i64,
    pub ref_name: String,
    pub head: Vec<u8>,

    pub created_at: DateTime<Utc>,
}
impl Push {
    pub fn try_from_event(
        now: DateTime<Utc>,
        p: &octocrab::models::events::payload::PushEventPayload,
    ) -> Result<Self> {
        Ok(Self {
            id: sign(&p.push_id).context("Invalid ID")?,
            ref_name: p.r#ref.clone(),
            head: hex::decode(&p.head).into_diagnostic().context("head")?,
            created_at: now,
        })
    }
}

#[derive(Debug, AsRefStr, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, DbEnum)]
#[db_enum(existing_type_path = "crate::schema::gorgond_github::sql_types::EventKind")]
pub enum EventKind {
    PullRequest,
    Push,
}
impl TryFrom<events::EventType> for EventKind {
    type Error = events::EventType;
    fn try_from(t: events::EventType) -> std::result::Result<Self, Self::Error> {
        match t {
            events::EventType::PullRequestEvent => Ok(Self::PullRequest),
            events::EventType::PushEvent => Ok(Self::Push),
            _ => Err(t),
        }
    }
}

#[derive(Debug, Queryable, Selectable, Insertable, Identifiable)]
#[diesel(table_name = crate::schema::gorgond_github::events, check_for_backend(diesel::pg::Pg))]
pub struct Event {
    pub id: i64,
    pub repo_id: i64,
    pub actor_id: i64,
    pub org_id: Option<i64>,
    pub pull_request_id: Option<i64>,
    pub kind: EventKind,
    pub public: bool,
    pub payload: serde_json::Value,

    pub emitted_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}
impl Event {
    pub fn try_from_event(now: DateTime<Utc>, e: &events::Event) -> Result<Self> {
        Ok(Self {
            id: e.id.parse().into_diagnostic().context("Invalid ID")?,
            repo_id: sign(&e.repo.id).context("Invalid Repo ID")?,
            actor_id: sign(&e.actor.id).context("Invalid Actor ID")?,
            org_id: e
                .org
                .as_ref()
                .map(|o| sign(&o.id))
                .transpose()
                .context("Invalid Org ID")?,
            pull_request_id: e
                .payload
                .as_ref()
                .and_then(|p| {
                    p.specific.as_ref().and_then(|p| match p {
                        events::payload::EventPayload::PullRequestEvent(p) => {
                            Some(sign(&p.pull_request.id))
                        }
                        _ => None,
                    })
                })
                .transpose()?,
            kind: e
                .r#type
                .clone()
                .try_into()
                .map_err(|t| miette!("Invalid Event Type: {t:?}"))?,
            public: e.public,
            payload: serde_json::to_value(&e.payload).into_diagnostic()?,
            emitted_at: e.created_at,
            updated_at: now,
            created_at: now,
        })
    }
}
