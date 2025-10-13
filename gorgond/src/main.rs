// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

// Clippy doesn't like gorgond_client::Error.
#![allow(clippy::result_large_err)]

use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Parser, Subcommand};
use miette::Result;

pub mod events;
pub mod run;

#[derive(Debug, Clone, Parser)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub logging: LoggingArgs,
}

#[derive(Debug, Clone, Parser)]
#[command(next_help_heading = "Logging")]
pub struct LoggingArgs {
    /// Increase log level.
    #[arg(long, short, global=true, action=clap::ArgAction::Count)]
    pub verbose: u8,
    /// Decrease log level.
    #[arg(long, short, global=true, action=clap::ArgAction::Count)]
    pub quiet: u8,
}
impl LoggingArgs {
    pub fn init(&self) {
        use tracing::Level;
        use tracing_subscriber::prelude::*;

        let level = match self.verbose as i8 - self.quiet as i8 {
            ..=-2 => Level::ERROR,
            -1 => Level::WARN,
            0 => Level::INFO,
            1 => Level::DEBUG,
            2.. => Level::TRACE,
        };
        tracing_subscriber::Registry::default()
            .with(
                tracing_subscriber::filter::Targets::new()
                    .with_target("axum::rejections", Level::TRACE)
                    // These log way too much internal information. Comment out if debugging them.
                    .with_target("tokio_postgres", Level::INFO)
                    .with_default(level),
            )
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}

fn init_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name_fn(|| {
            static THREAD_ID: AtomicUsize = AtomicUsize::new(0);
            let id = THREAD_ID.fetch_add(1, Ordering::SeqCst);
            format!("gorgond-tokio-{id}")
        })
        .build()
        .expect("Couldn't build tokio runtime")
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run the Gorgon daemon.
    Run(run::Args),
    /// Manage event log files.
    Events(events::Args),
}

pub fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Run(args) => init_runtime().block_on(run::run(args)),
        Command::Events(args) => events::run(args),
    }
}

fn main() {
    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .with_syntax_highlighting(miette::highlighters::SyntectHighlighter::default())
                .context_lines(6)
                .build(),
        )
    }))
    .expect("Couldn't install miette hook");

    let args = Args::parse();
    args.logging.init();

    if let Err(err) = run(args) {
        eprintln!("\n{err:?}");
        std::process::exit(1);
    }
}
