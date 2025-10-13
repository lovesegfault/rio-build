// SPDX-FileCopyrightText: 2024-2025 Alyssa Ross <hi@alyssa.is>
// SPDX-License-Identifier: EUPL-1.2+

use std::fmt::{self, Display, Formatter};
use std::io::stdin;

use clap::Parser;
use git2::{Commit, MergeOptions, Oid, Repository, Signature};
use log::{error, info};
use miette::{miette, IntoDiagnostic};

struct FormattedCommit<'a>(&'a Commit<'a>);

impl<'a> Display for FormattedCommit<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let short_id = self
            .0
            .as_object()
            .short_id()
            .inspect_err(|e| error!("Looking up short ID of commit failed: {e}"))
            .map_err(|_| fmt::Error)?;

        write!(f, "{}", short_id.as_str().unwrap())?;

        if let Some(s) = self.0.summary() {
            write!(f, " ({s:?})")?;
        }

        Ok(())
    }
}

#[derive(Parser)]
struct Args {
    /// Email address to be used in author and committer metadata of rollup commits.
    #[arg(long, env = "EMAIL")]
    email: String,

    #[command(flatten)]
    common: gorgon_util_cli::Args,
}

fn run(args: Args) -> miette::Result<()> {
    let repo = Repository::open(".").into_diagnostic()?;

    let commits = stdin()
        .lines()
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?
        .iter()
        .map(|s| repo.find_commit(Oid::from_str(s)?))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    let mut merge_options = MergeOptions::new();
    merge_options.fail_on_conflict(true);

    let sig = Signature::now("gorgon-rollup", &args.email).into_diagnostic()?;

    let mut commits = commits.into_iter();
    let Some(commit) = commits.next() else {
        return Err(miette!("No commits given"));
    };
    let message = "Rollup merge of 1 commit";
    let tree = commit.tree().into_diagnostic()?;
    let mut rollup_id = repo
        .commit(None, &sig, &sig, message, &tree, &[&commit])
        .into_diagnostic()?;

    loop {
        let rollup = repo.find_commit(rollup_id).into_diagnostic()?;
        let mut parents: Vec<_> = rollup.parents().collect();

        // Rollup commits start with one parent, and then subsequent ones have more
        // parents, so there will always be a last one.
        let last_commit = parents.last().unwrap();
        info!("Merged commit {}", FormattedCommit(&last_commit));

        let Some(commit) = commits.next() else {
            break;
        };

        let Ok(mut index) = repo.merge_commits(&rollup, &commit, Some(&merge_options)) else {
            // At the time of writing, merge conflicts aren't reported correctly by git2, so we
            // just have to assume that any error is a merge conflict.
            // https://github.com/rust-lang/git2-rs/pull/1153
            info!("Skipping conflicting commit {}", FormattedCommit(&commit));
            continue;
        };

        parents.push(commit);
        let message = format!("Rollup merge of {} commits", parents.len());
        let merge_tree_id = index.write_tree_to(&repo).into_diagnostic()?;
        let merge_tree = repo.find_tree(merge_tree_id).into_diagnostic()?;
        let borrowed_parents: Vec<_> = parents.iter().collect();
        rollup_id = repo
            .commit(None, &sig, &sig, &message, &merge_tree, &borrowed_parents)
            .into_diagnostic()?;
    }

    println!("{}", rollup_id);

    Ok(())
}

fn main() {
    let args = Args::parse();
    gorgon_util_cli::logger(args.common).init();
    gorgon_util_cli::main(args.common, || run(args));
}
