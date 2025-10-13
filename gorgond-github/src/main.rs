// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use clap::{Parser, Subcommand};
use diesel_migrations::{EmbeddedMigrations, embed_migrations};
use gorgond_diesel::AsyncPgPool;
use miette::{Context, IntoDiagnostic, Result};
use tokio::runtime::Runtime;

pub mod ingest;
pub mod models;
pub mod schema;
pub mod sync;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[derive(Debug, Clone, Parser)]
pub struct Args {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    db: gorgond_diesel::Args,
    #[command(flatten)]
    common: gorgon_util_cli::Args,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    Sync(sync::Args),
    Ingest(ingest::Args),
}

fn init_runtime(threads: Option<usize>) -> Result<Runtime> {
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    if let Some(threads) = threads {
        rt.worker_threads(threads);
    }
    rt.enable_all()
        .thread_name_fn({
            static THREAD_ID: AtomicUsize = AtomicUsize::new(1);
            move || format!("tokio-{:02}", THREAD_ID.fetch_add(1, Ordering::Relaxed))
        })
        .build()
        .into_diagnostic()
        .context("Couldn't init Tokio runtime")
}
fn with_runtime(threads: Option<usize>, f: impl FnOnce(Runtime) -> Result<()>) -> Result<()> {
    f(init_runtime(threads)?)
}

async fn init_db(args: &gorgond_diesel::Args) -> Result<AsyncPgPool> {
    args.init(MIGRATIONS, "gorgond_github").await
}
pub fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Sync(cmd) => with_runtime(None, |rt| {
            cmd.run(rt.block_on(init_db(&args.db))?, Arc::new(rt))
        }),
        Command::Ingest(cmd) => {
            // Don't spawn `nproc` worker threads for a basically single-threaded task.
            with_runtime(Some(2), |rt| cmd.run(rt.block_on(init_db(&args.db))?, rt))
        }
    }
}

fn main() {
    let args = Args::parse();
    gorgon_util_cli::logger(args.common).init();
    gorgon_util_cli::main(args.common, || run(args));
}
