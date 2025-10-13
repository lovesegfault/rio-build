// SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
// SPDX-License-Identifier: EUPL-1.2+

mod client;
mod event;
mod http;

use std::collections::BTreeSet;
use std::env::var_os;
use std::ffi::OsString;
use std::fs::read_to_string;
use std::io::{self, stdin, ErrorKind, Write};
use std::os::unix::prelude::*;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context};
use clap::Parser;
use thiserror::Error;
use tracing::warn;
use url::Url;

use client::Client;
use event::{Event, EventError};
use http::get_link;

fn read_github_token() -> io::Result<Option<String>> {
    var_os("CREDENTIALS_DIR")
        .and_then(
            |dir| match read_to_string(Path::new(&dir).join("GITHUB_TOKEN")) {
                Ok(token) => Some(Ok(token)),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                e => Some(e),
            },
        )
        .transpose()
}

#[derive(Debug, Error)]
#[error("should contain exactly one '/'")]
struct RepoValidationError;

fn validate_repo(repo: &str) -> Result<String, RepoValidationError> {
    let repo = repo.to_string();

    if repo.chars().filter(|c| *c == '/').count() != 1 {
        return Err(RepoValidationError);
    }

    Ok(repo)
}

#[derive(Parser)]
struct Args {
    #[arg(long, short, action=clap::ArgAction::Count)]
    verbose: u8,

    #[arg(long, short, action=clap::ArgAction::Count)]
    quiet: u8,

    #[arg(short, long, default_value = "api.github.com")]
    domain: String,

    #[arg(value_name = "OWNER/REPO", value_parser = validate_repo)]
    repo: String,

    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<OsString>,
}

fn init_logging(args: &Args) {
    use tracing_subscriber::prelude::*;

    tracing_subscriber::Registry::default()
        .with(match args.verbose as i8 - args.quiet as i8 {
            ..=-2 => tracing_subscriber::filter::LevelFilter::ERROR,
            -1 => tracing_subscriber::filter::LevelFilter::WARN,
            0 => tracing_subscriber::filter::LevelFilter::INFO,
            1 => tracing_subscriber::filter::LevelFilter::DEBUG,
            2.. => tracing_subscriber::filter::LevelFilter::TRACE,
        })
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging(&args);

    let mut fs_safe_repo = args.repo.replace('/', "_");
    fs_safe_repo.make_ascii_lowercase();
    let last_ids: BTreeSet<_> = stdin().lines().collect::<Result<_, _>>().unwrap();

    let token = read_github_token().context("reading GITHUB_TOKEN credential")?;
    if token.is_none() {
        warn!("No GitHub access token supplied!");
    }
    let mut client = Client::new(&token).context("creating client")?;

    let page1_url = format!(
        "https://{}/networks/{}/events?per_page=100",
        args.domain, args.repo
    );
    let mut next_rel = Some(Url::parse(&page1_url).context("parsing URL")?);
    let mut events = BTreeSet::new();

    while let Some(url) = next_rel {
        let mut response = client.get(&url, b"next").context("GET {url}")?;
        events.append(&mut response.events);
        next_rel = response.next_rel;
    }

    if !last_ids.is_empty() {
        let last_ids: BTreeSet<_> = last_ids.iter().map(String::as_str).collect();
        let new_ids = events.iter().map(|e| e.id()).collect();
        if last_ids.intersection(&new_ids).next().is_none() {
            warn!(
                "WARNING: None of the events seen last run were encountered again this time.\n\
                 This means that events are likely to have been dropped.\n\
                 You should adjust gorgon-github-events@.timer to run more frequently."
            );
        }
    }

    let mut child = None;

    for event in events.iter() {
        if last_ids.is_empty() || !last_ids.contains(event.id()) {
            if child.is_none() {
                child = Some(
                    Command::new(&args.command[0])
                        .args(&args.command[1..])
                        .stdin(Stdio::piped())
                        .spawn()
                        .context("spawning child")?,
                );
            }

            writeln!(child.as_ref().unwrap().stdin.as_ref().unwrap(), "{event}")
                .context("writing to child stdin")?;
        }

        println!("{}", event.id());
    }

    if let Some(mut child) = child {
        // This could use exit_ok if it stabilizes:
        // https://github.com/rust-lang/rust/issues/84908
        let status = child.wait().context("waiting for child")?;
        match status.code() {
            Some(0) => (),
            Some(code) => return Err(anyhow!("child exited with status {code}")),
            None => {
                let signal = status.signal().unwrap();
                return Err(anyhow!("child killed by signal {signal}"));
            }
        }
    }

    Ok(())
}
