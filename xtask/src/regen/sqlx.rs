//! Regenerate `.sqlx/` offline query cache.
//!
//! Spins up an ephemeral postgres (reusing `rio-test-support::pg::PgServer`
//! — the same initdb+spawn bootstrap the integration tests use), runs
//! migrations, then `cargo sqlx prepare --workspace`.

use anyhow::Result;
use tracing::info;

use crate::sh::{cmd, shell};

pub fn run() -> Result<()> {
    let sh = shell()?;

    // rio-test-support bootstraps a process-global postgres (initdb +
    // spawn, PR_SET_PDEATHSIG cleanup). Unix-socket-only; sqlx-cli
    // handles the `?host=/path` URL form.
    let pg = rio_test_support::pg::PgServer::get();
    let url = pg.admin_url();

    // Devshell sets SQLX_OFFLINE=true globally so cargo build works
    // without PG. Unset so prepare actually hits the DB.
    let _env = sh.push_env("DATABASE_URL", url);
    let _env2 = sh.push_env("SQLX_OFFLINE", "false");

    info!("migrating");
    cmd!(sh, "cargo sqlx migrate run --source migrations").run()?;

    // --check first: exits 0 if cache is current. Non-zero → regenerate.
    //
    // NOTE: `cargo sqlx prepare --workspace` internally does `cargo rustc
    // -p <crate>` per workspace member. That per-package resolution
    // differs from the workspace-unified feature set, so this WILL
    // rebuild sqlx/kube/etc. with narrower features than the main build
    // cache has. Unavoidable — sqlx-cli has no flag for unified
    // resolution. The --check fast-path avoids this in the common case.
    info!("checking .sqlx/ cache");
    if cmd!(sh, "cargo sqlx prepare --workspace --check")
        .quiet()
        .run()
        .is_ok()
    {
        info!("sqlx cache already current");
        return Ok(());
    }

    info!("regenerating .sqlx/ (rebuilds with per-package features — expect recompile)");
    cmd!(sh, "cargo sqlx prepare --workspace").run()?;

    let count = std::fs::read_dir(sh.current_dir().join(".sqlx"))
        .map(|d| d.count())
        .unwrap_or(0);
    info!("sqlx cache: {count} queries");
    Ok(())
}
