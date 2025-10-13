// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use std::path::PathBuf;

use crate::helper::{Helper, HelperContext};

#[derive(Debug, Clone, clap::Parser)]
#[group(id = "fetcher")]
#[command(next_help_heading = "Fetcher")]
pub struct FetcherArgs {
    /// Destination path.
    #[arg(long, short, env = "GORGON_FETCHER_OUT")]
    pub out: PathBuf,
}

impl HelperContext for &FetcherArgs {
    fn apply(&self, helper: Helper) -> Helper {
        helper.with_env("GORGON_FETCHER_OUT", &self.out)
    }
}
