//! Regenerate `.sqlx/` offline query cache.
//!
//! Spins up an ephemeral postgres (reusing `rio-test-support::pg::PgServer`
//! — the same initdb+spawn bootstrap the integration tests use), runs
//! migrations, then `cargo sqlx prepare --workspace`.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

pub async fn run() -> Result<()> {
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
    // into a sub-target so the main cache stays warm (build-dir defaults
    // to the target dir, so this covers intermediates too).
    let isolated = sh.current_dir().join("target/sqlx-prepare");
    let _env3 = sh.push_env("CARGO_TARGET_DIR", &isolated);

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

    ui::step("cargo sqlx migrate run", || {
        sh::run(cmd!(
            sh,
            "cargo sqlx migrate run --source rio-migrations/migrations"
        ))
    })
    .await?;

    // The full prepare runs UNCONDITIONALLY (bughunt-2 merged_bug_293
    // residual): `--check` verifies only the source→cache direction —
    // measured passing in 101s with a planted orphan .sqlx entry
    // present — so a --check fast-path silently preserves orphaned
    // cache files. The full workspace prepare rewrites the EXACT live
    // query set (sweeping orphans), making this regenerator the strict
    // both-directions set-equality vehicle by construction; a
    // no-change run rewrites identical bytes, so umbrella idempotence
    // is unaffected. Text-matching orphan detection stays banned as
    // unsound (Rust adjacent-string-literal concatenation defeats any
    // normalization short of literal parsing — 8/10 false positives
    // measured).
    //
    // `cargo sqlx prepare --workspace` internally does `cargo rustc -p
    // <crate>` per member — per-package feature resolution, unavoidable;
    // the isolated CARGO_TARGET_DIR keeps repeat runs warm.
    //
    // `-- --all-targets`: forwarded to the inner `cargo rustc` so
    // `#[cfg(test)]` queries are cached too. The cross-service
    // `LivePin` contract anchor (rio-store gc tests) lives under
    // cfg(test) — without this, regen succeeds but `cargo test`
    // fails on "no cached data for this query".
    ui::step("cargo sqlx prepare --workspace", || {
        sh::run(cmd!(sh, "cargo sqlx prepare --workspace -- --all-targets"))
    })
    .await?;

    let count = std::fs::read_dir(sh.current_dir().join(".sqlx"))
        .map(|d| d.count())
        .unwrap_or(0);
    tracing::debug!("{count} queries cached");
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
