// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use chrono::prelude::*;
use diesel::{prelude::*, upsert::excluded};
use diesel_async::{AsyncConnection, RunQueryDsl};
use gorgond_diesel::AsyncPgPool;
use miette::{Context, IntoDiagnostic, NamedSource, Result, SourceOffset, SourceSpan, bail};
use octocrab::models::events::{self, payload::EventPayload};
use scoped_futures::ScopedFutureExt;
use tokio::runtime::Runtime;
use tracing::{Span, debug, field::Empty, info, instrument};

use crate::{models, schema::gorgond_github as schema};

macro_rules! excluded {
    ($($col:expr),+$(,)?) => {
        ($($col.eq(excluded($col))),+)
    };
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum EventError {
    #[error("Invalid JSON")]
    #[diagnostic(help("This command expects a stream of JSON events on stdin."))]
    InvalidJson(#[from] serde_json::Error),

    #[error("Invalid Event")]
    FromValue(
        #[label("{2}")] SourceOffset,
        #[source_code] NamedSource<String>,
        #[source] serde_json::Error,
    ),

    #[error("Couldn't ingest event")]
    #[diagnostic(forward(2))]
    Ingest(
        #[label("{2}")] SourceSpan,
        #[source_code] NamedSource<String>,
        #[source] Box<dyn miette::Diagnostic + Send + Sync + 'static>,
    ),
}

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "ingest")]
pub struct Args {}
impl Args {
    #[instrument(name = "ingest", skip_all)]
    pub fn run(self, pool: AsyncPgPool, rt: Runtime) -> Result<()> {
        let mut it =
            serde_json::Deserializer::from_reader(std::io::BufReader::new(std::io::stdin().lock()))
                .into_iter::<serde_json::Value>()
                .map(|r| r.map_err(EventError::InvalidJson));
        let (mut events, mut failed) = (0, 0);
        while let Some(value) = it.next().transpose()? {
            events += 1;
            let event = serde_json::value::from_value(value.clone()).map_err(|err| {
                let src = serde_json::to_string_pretty(&value).unwrap_or_default();
                EventError::FromValue(
                    SourceOffset::from_location(&src, err.line(), err.column()),
                    NamedSource::new("event.json", src).with_language("JSON"),
                    err,
                )
            })?;
            if let Err(err) = rt.block_on(self.ingest_event(event, &pool)) {
                failed += 1;
                let src = serde_json::to_string_pretty(&value).unwrap_or_default();
                let err = EventError::Ingest(
                    SourceSpan::new(0.into(), src.len()),
                    NamedSource::new("event.json", src),
                    err.into(),
                );
                eprintln!("{:?}", miette::Report::new(err));
            };
        }
        if failed > 0 {
            bail!("Ingesting {failed}/{events} events failed!");
        }
        Ok(())
    }

    #[instrument(name = "event", skip_all, fields(
        event_id = Empty, kind = Empty, action = Empty,
        repo_id = Empty, actor_id = Empty, org_id = Empty, push_id = Empty, pr_id = Empty,
    ))]
    async fn ingest_event(&self, e: events::Event, pool: &AsyncPgPool) -> Result<()> {
        if let Err(kind) = models::EventKind::try_from(e.r#type.clone()) {
            debug!(?kind, "Skipping event");
            return Ok(());
        }

        let span = Span::current();
        let (now, current_at) = (Utc::now(), e.created_at);

        let event = models::Event::try_from_event(now, &e).context("Event")?;
        let repo = models::Repo::try_from_event(now, current_at, &e.repo).context("Repo")?;
        let actor = models::Actor::try_from_event(now, current_at, &e.actor).context("Actor")?;
        span.record("event_id", event.id)
            .record("repo_id", repo.id)
            .record("actor_id", actor.id)
            .record("kind", event.kind.as_ref());

        let org = e
            .org
            .as_ref()
            .map(|o| models::Org::try_from_event(now, current_at, o))
            .transpose()
            .context("Org")?
            .inspect(|org| {
                span.record("org_id", org.id);
            });

        let (mut pr, mut push) = (None, None);
        match e.payload.as_ref().and_then(|p| p.specific.as_ref()) {
            Some(EventPayload::PushEvent(p)) => {
                let push = push.insert(models::Push::try_from_event(now, p).context("Push")?);
                span.record("push_id", push.id);
            }
            Some(EventPayload::PullRequestEvent(p)) => {
                let pr = pr.insert(
                    models::PullRequest::try_from_event(
                        now,
                        current_at,
                        event.repo_id,
                        &p.pull_request,
                    )
                    .context("Pull Request")?,
                );
                span.record("pr_id", pr.id)
                    .record("action", format!("{:?}", p.action));
            }
            _ => {}
        }

        pool.get()
            .await
            .into_diagnostic()?
            .transaction(|conn| {
                async move {
                    diesel::insert_into(schema::repos::table)
                        .values(&repo)
                        .on_conflict(schema::repos::id)
                        .do_update()
                        .set(excluded!(
                            schema::repos::name,
                            schema::repos::current_at,
                            schema::repos::updated_at,
                        ))
                        .execute(conn)
                        .await?;
                    diesel::insert_into(schema::actors::table)
                        .values(&actor)
                        .on_conflict(schema::actors::id)
                        .do_update()
                        .set(excluded!(
                            schema::actors::login,
                            schema::actors::display_login,
                            schema::actors::avatar_url,
                            schema::actors::current_at,
                            schema::actors::updated_at,
                        ))
                        .execute(conn)
                        .await?;
                    if let Some(org) = org {
                        diesel::insert_into(schema::orgs::table)
                            .values(&org)
                            .on_conflict(schema::orgs::id)
                            .do_update()
                            .set(excluded!(
                                schema::orgs::login,
                                schema::orgs::avatar_url,
                                schema::orgs::current_at,
                                schema::orgs::updated_at,
                            ))
                            .execute(conn)
                            .await?;
                    }
                    if let Some(pr) = pr {
                        diesel::insert_into(schema::pull_requests::table)
                            .values(&pr)
                            .on_conflict(schema::pull_requests::id)
                            .do_update()
                            .set(excluded!(
                                schema::pull_requests::repo_id,
                                schema::pull_requests::number,
                                schema::pull_requests::ref_name,
                                schema::pull_requests::merge_request_id,
                                schema::pull_requests::current_at,
                                schema::pull_requests::updated_at,
                            ))
                            .execute(conn)
                            .await?;
                    }
                    if let Some(push) = push {
                        diesel::insert_into(schema::pushes::table)
                            .values(&push)
                            .on_conflict(schema::pushes::id)
                            .do_update()
                            .set(excluded!(schema::pushes::ref_name, schema::pushes::head))
                            .execute(conn)
                            .await?;
                    }
                    diesel::insert_into(schema::events::table)
                        .values(&event)
                        .on_conflict(schema::events::id)
                        .do_update()
                        .set(excluded!(
                            schema::events::repo_id,
                            schema::events::actor_id,
                            schema::events::org_id,
                            schema::events::pull_request_id,
                            schema::events::kind,
                            schema::events::public,
                            schema::events::payload,
                            schema::events::emitted_at,
                            schema::events::updated_at,
                        ))
                        .execute(conn)
                        .await?;
                    Ok::<_, diesel::result::Error>(())
                }
                .scope_boxed()
            })
            .await
            .into_diagnostic()?;

        info!("-> Ingested");
        Ok(())
    }
}
