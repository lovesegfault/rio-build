// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use crate::{Error, Result};
use std::ffi::OsStr;
use tokio::process::Command;
use tracing::{debug, debug_span, instrument, warn};

/// Path to the nix binary. Default set in: .cargo/config.toml
pub const NIX_CLI_PATH: &str = env!("NIX_WEB_BUILD_NIX_CLI_PATH");

#[instrument(name = "nix", skip_all)]
async fn exec(cmd: &mut Command) -> Result<Vec<u8>> {
    let cmd = cmd.stdin(std::process::Stdio::null());
    debug!(?cmd, "Calling nix...");

    let out = cmd.output().await?;
    if !out.status.success() {
        let last_line = debug_span!("stderr").in_scope(|| {
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .inspect(|line| warn!("{}", line))
                .last()
                .map(|s| s.strip_prefix("error:").unwrap_or(s).trim().to_string())
        });
        return Err(Error::NixCommandError(
            out.status.code().unwrap_or(0),
            last_line.unwrap_or("".into()),
        ));
    }
    Ok(out.stdout)
}

/// Returns logs for a derivation.
pub async fn log<P: AsRef<OsStr>>(path: P) -> Result<String> {
    Ok(String::from_utf8_lossy(
        &exec(
            Command::new(NIX_CLI_PATH)
                .arg("--experimental-features")
                .arg("nix-command")
                .arg("log")
                .arg(path),
        )
        .await?[..],
    )
    .into())
}
