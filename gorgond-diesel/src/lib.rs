// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

pub mod models;
pub mod schema;

use std::error::Error as StdError;

use diesel::{define_sql_function, migration::MigrationSource, pg::Pg, sql_types};
use diesel_async::{
    pg::AsyncPgConnection,
    pooled_connection::{AsyncDieselConnectionManager, deadpool::Pool},
};
use diesel_migrations::MigrationHarness;
use futures::future::TryFutureExt;
use miette::{Context, IntoDiagnostic, Result};
use tracing::{debug, debug_span, info, instrument};

pub type AsyncPgPool = Pool<AsyncPgConnection>;

define_sql_function! {
    /// https://www.postgresql.org/docs/current/functions-admin.html#FUNCTIONS-ADMIN-SET
    fn set_config(
        setting_name: sql_types::Text,
        new_value: sql_types::Text,
        is_local: sql_types::Bool,
    ) -> sql_types::Text;
}

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "db")]
#[command(next_help_heading = "Database Connection")]
pub struct Args {
    /// Database URL.
    #[arg(long, short = 'D', env = "GORGOND_DATABASE_URL")]
    db_url: String,
}
impl Args {
    #[instrument(name = "db", level = "DEBUG", skip_all)]
    pub async fn init(
        &self,
        migrations: impl MigrationSource<Pg> + Send + 'static,
        schema: &'static str,
    ) -> Result<Pool<AsyncPgConnection>> {
        debug!(db_url = self.db_url, "Connecting to database...");
        let cfg = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&self.db_url);
        let pool = Pool::builder(cfg)
            .build()
            .into_diagnostic()
            .context("Couldn't build database connection pool")?;
        pool.get()
            .map_err(miette::Report::from_err)
            .and_then(|conn| migrate(conn, migrations, schema))
            .await
            .context("Couldn't run migrations")?;
        Ok(pool)
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum MigrationError {
    #[error(transparent)]
    Diesel(#[from] diesel::result::Error),
    #[error("Couldn't get pending migrations")]
    PendingMigrations(#[source] Box<dyn StdError + Send + Sync>),
    #[error("Couldn't run migration: {1}")]
    RunMigration(#[source] Box<dyn StdError + Send + Sync>, String),
}

#[instrument(name = "migrate", level = "DEBUG", skip_all)]
pub async fn migrate<
    C: diesel_async::AsyncConnection<Backend = Pg> + 'static,
    M: MigrationSource<Pg> + Send + 'static,
>(
    conn: C,
    migrations: M,
    schema: &'static str,
) -> Result<()> {
    // Bridge the async connection back into a sync one for diesel-migrations.
    tokio::task::spawn_blocking(move || {
        debug!("Running migrations...");
        diesel::Connection::transaction::<(), MigrationError, _>(
            &mut diesel_async::async_connection_wrapper::AsyncConnectionWrapper::<C>::from(conn),
            |conn| {
                // Stupid hack to work around __diesel_schema_migrations always being created
                // in the default schema (public): override search_path for this transaction.
                {
                    use diesel::prelude::*;
                    diesel::sql_query(format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                        .execute(conn)?;
                    diesel::select(set_config("search_path", schema, true)).execute(conn)?;
                }
                match conn
                    .pending_migrations(migrations)
                    .map_err(MigrationError::PendingMigrations)?
                {
                    pending if pending.is_empty() => debug!("▶️ Migrations already up to date"),
                    pending => {
                        info!(num = pending.len(), "⏸️ Database has pending migrations");
                        for migration in pending {
                            let name = migration.name();
                            let _enter = debug_span!("migration", %name).entered();
                            info!(%name, "⏩ Applying migration...");

                            conn.run_migration(&migration).map_err(|err| {
                                MigrationError::RunMigration(err, name.to_string())
                            })?;
                        }
                        info!("▶️ Database successfully migrated!");
                    }
                }
                Ok(())
            },
        )
    })
    .await
    .expect("gorgond-diesel::migrate panicked!")?;

    Ok(())
}
