//! Regenerate `.sqlx/` offline query cache.
//!
//! Spins up an ephemeral postgres (reusing `rio-test-support::pg::PgServer`
//! — the same initdb+spawn bootstrap the integration tests use), runs
//! migrations, then `cargo sqlx prepare --workspace`.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

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
    // `cargo sqlx prepare` sets RUSTFLAGS before its internal `cargo
    // check`, which poisons the main target/ fingerprint — the next
    // `cargo run` sees different flags and rebuilds everything. Isolate
    // into a sub-target so the main cache stays warm.
    let _env3 = sh.push_env(
        "CARGO_TARGET_DIR",
        sh.current_dir().join("target/sqlx-prepare"),
    );

    // `cargo sqlx prepare` bumps src/{lib,main}.rs mtimes on every
    // workspace crate to force proc-macro re-expansion (stable can't
    // track env vars). Snapshot mtimes now, restore after, so the
    // main target/ fingerprint stays valid.
    let snapshot = snapshot_mtimes()?;
    let _restore = scopeguard::guard(snapshot, |s| {
        for (p, t) in s {
            let _ = filetime::set_file_mtime(&p, filetime::FileTime::from(t));
        }
    });

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

/// Snapshot mtimes of all files sqlx-cli touches for its "minimal
/// recompile setup": build.rs, src/lib.rs, src/main.rs, src/bin/*.rs.
fn snapshot_mtimes() -> Result<Vec<(PathBuf, SystemTime)>> {
    let mut out = Vec::new();
    let mut record = |p: PathBuf| {
        if let Ok(m) = std::fs::metadata(&p)
            && let Ok(t) = m.modified()
        {
            out.push((p, t));
        }
    };
    for entry in std::fs::read_dir(repo_root())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let root = entry.path();
        record(root.join("build.rs"));
        let src = root.join("src");
        record(src.join("lib.rs"));
        record(src.join("main.rs"));
        if let Ok(bins) = std::fs::read_dir(src.join("bin")) {
            for b in bins.flatten() {
                if b.path().extension().is_some_and(|e| e == "rs") {
                    record(b.path());
                }
            }
        }
    }
    Ok(out)
}
