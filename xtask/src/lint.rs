//! Workspace-level invariant checks ("lints that can't be lints").
//!
//! Each lint reads files from multiple workspace crates at runtime —
//! cross-cutting checks that don't fit in any single crate's tests.
//! Surfaced as a flake check via `nix/misc-checks.nix` (`xtask-lint`).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::sh::repo_root;

// New variant? Add an arm in `run()` (compiler-enforced) AND an entry in
// `all()` (test-enforced — see `all_returns_every_variant`). Nothing
// else: `xtask lint` and the `xtask-lint` flake check run `all()`.
#[derive(Debug, clap::Subcommand)]
pub enum Lint {
    /// Every table created by `rio-migrations/migrations/*.sql` is
    /// referenced by name in ≥1 workspace `.rs` file. Catches dead
    /// schema before the checksum freeze makes it permanent.
    SchemaLiveness,
    /// Every `HELM_RENDERED_SLA_KEYS` entry appears in the rendered
    /// scheduler helm template. Catches a `[sla]` field helm forgot to
    /// surface to operators.
    HelmSla,
    /// `nix/nixos-node/seccomp/{rio-builder,rio-fetcher}.json` are
    /// allowlists (`defaultAction: SCMP_ACT_ERRNO`), the denied
    /// syscalls are absent from every ALLOW block, the
    /// worker-critical syscalls (mount/unshare/chroot/clone/umount2)
    /// are present (plus, builder-only, the read-side trace syscalls
    /// sanitizer/debugger check phases need), and the fetcher profile
    /// keeps its explicit SCMP_ACT_ERRNO block (ADR-019). Catches a
    /// profile edit that flips to a denylist or strands the Nix
    /// sandbox.
    SeccompAllowlist,
    /// Every `RETENTION_REGISTRY` policy claim resolves against
    /// reality: `SweptBy` symbols define in non-test workspace source
    /// AND a defining file carries the deleting statement
    /// (`DELETE FROM {table}` / a `TRUNCATE` list naming it,
    /// line-continuation normalized); `CascadeFrom` rows resolve a
    /// `REFERENCES {parent} … ON DELETE CASCADE` clause in the named
    /// migration. Catches phantom retention attributions
    /// (merged_bug_001/142): a registry row crediting a sweeper that
    /// doesn't exist or doesn't delete the table, or a "CASCADE" that
    /// is actually RESTRICT — rows growing forever behind a row that
    /// greps to nothing.
    RetentionTruth,
}

impl Lint {
    /// Every lint, in run order, for the `xtask lint` umbrella.
    ///
    /// Hand-maintained because clap subcommand enums don't expose a
    /// value iterator (and `strum::EnumIter` would mean a new direct
    /// dep just for this). Drift is impossible to ship: the
    /// `all_returns_every_variant` test compares this against the
    /// subcommand list clap derives from the enum, so a variant added
    /// to the enum but not here fails `cargo test -p xtask`.
    fn all() -> Vec<Lint> {
        vec![
            Lint::SchemaLiveness,
            Lint::HelmSla,
            Lint::SeccompAllowlist,
            Lint::RetentionTruth,
        ]
    }
}

/// Run one lint.
pub fn run(lint: &Lint) -> Result<()> {
    match lint {
        Lint::SchemaLiveness => schema_liveness(),
        Lint::HelmSla => helm_sla(),
        Lint::SeccompAllowlist => seccomp_allowlist(),
        Lint::RetentionTruth => retention_truth(),
    }
}

/// Run every lint. The `xtask lint` no-subcommand umbrella —
/// `nix/misc-checks.nix`'s `xtask-lint` derivation calls this so a new
/// `Lint` variant joins the flake check without editing the `.nix`.
///
/// Collect-all, not fail-fast: each lint runs even if an earlier one
/// failed, so a single local `xtask lint` surfaces every violation
/// instead of one per fix-rerun cycle. CI doesn't care (any failure is
/// red either way); the choice is for the developer loop.
pub fn run_all() -> Result<()> {
    let mut failed = Vec::new();
    for lint in Lint::all() {
        if let Err(e) = run(&lint) {
            tracing::error!(lint = ?lint, error = format_args!("{e:#}"), "lint failed");
            failed.push(lint);
        }
    }
    ensure!(
        failed.is_empty(),
        "{} of {} lint(s) failed: {failed:?}",
        failed.len(),
        Lint::all().len(),
    );
    Ok(())
}

/// Schema-liveness guard: every table that exists after applying all
/// `rio-migrations/migrations/*.sql` is referenced by name in ≥1
/// workspace `.rs` file.
///
/// `migration_checksums_frozen` only catches *edits* to shipped
/// migrations — it does not catch a NEW migration that creates or
/// extends schema nothing reads. Migration 055 added EMA-state columns
/// to `hw_cost_factors`; the actual EMA persist shipped via
/// `sla_ema_state` instead, so 055 (and 042's `hw_cost_factors` itself)
/// were dead on arrival, and the `M_055` doc-const actively misled.
/// Once a dead migration ships, the checksum freeze means dropping the
/// schema needs a SECOND migration. This lint fails the first one
/// before it freezes.
///
/// The corpus is workspace `.rs` source from the PG-querying crates
/// (rio-store, rio-scheduler, rio-controller, xtask). NOT
/// `.sqlx/query-*.json` — that only covers `query!`/`query_as!` macro
/// callsites; the ≈200 non-macro `sqlx::query()` / `QueryBuilder` sites
/// leave no `.sqlx/` entry. `migrations.rs` is excluded so the
/// per-migration doc-const prose (which names every table as
/// commentary) can't mask a dead table.
///
/// **A new table this lint flags as dead:** either it IS dead (delete
/// the migration before it ships), or add it to `ALLOW_DEAD` below
/// with a one-line rationale naming the consumer-to-be.
fn schema_liveness() -> Result<()> {
    let root = repo_root();

    // Tables intentionally present in the schema with zero `.rs`
    // references in the corpus. Each entry MUST carry a rationale.
    const ALLOW_DEAD: &[(&str, &str)] = &[(
        "hw_cost_factors",
        "ADR-023 chose sla_ema_state instead; DROP TABLE deferred to a \
         follow-up migration (042 is frozen)",
    )];

    // ── Live-table set: CREATE/ALTER add, DROP removes, in version
    // order. Migration filenames are zero-padded `NNN_*.sql`, so a
    // lexicographic sort matches `MIGRATOR.iter()`'s ascending-version
    // order without parsing the prefix. Per-file extraction (regex +
    // comment-stripping + fail-loud guard) lives in `scan_migration_ddl`.
    let mig_dir = root.join("rio-migrations/migrations");
    ensure!(
        mig_dir.is_dir(),
        "migration dir not found: {}",
        mig_dir.display()
    );
    let mut sql_paths: Vec<_> = fs::read_dir(&mig_dir)
        .with_context(|| format!("reading {}", mig_dir.display()))?
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension().is_some_and(|e| e == "sql")).then_some(p)
        })
        .collect();
    sql_paths.sort();

    let mut live: BTreeSet<String> = BTreeSet::new();
    for path in &sql_paths {
        let sql =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        scan_migration_ddl(&sql, path, &mut live)?;
    }
    // Floor guard: a regex regression that matches nothing would make
    // the loop below vacuously pass.
    ensure!(live.len() > 20, "parsed only {} live tables", live.len());

    // ── Corpus: concat every `.rs` file under the PG-querying crates'
    // `src/` trees.
    //
    // Files excluded from the walk. Matched by **basename only** — any
    // file with one of these names anywhere under a corpus root is
    // excluded, not just the one path the exclusion was written for.
    // Currently safe: the only basename collisions are this file
    // (`xtask/src/lint.rs`) and the doc-const file
    // (`rio-migrations/src/migrations.rs`, which is outside the corpus
    // roots anyway). If you add a `lint.rs` or `migrations.rs` to
    // rio-store, rio-scheduler, rio-controller, or xtask that DOES
    // legitimately reference table names, switch this to a path-relative
    // match.
    //
    // Why each entry is excluded:
    // - `migrations.rs`: the per-migration doc-const file names every
    //   table as prose, which would defeat the liveness check. (Today
    //   no corpus dir contains one — it moved to
    //   `rio-migrations/src/migrations.rs` — but the exclusion stays so
    //   re-introducing it doesn't silently mask dead tables.)
    // - `lint.rs`: this file. ALLOW_DEAD entries, the doc-comments
    //   above, and the `#[cfg(test)]` synthetic SQL all name tables as
    //   commentary; including the lint in its own corpus makes every
    //   allowlisted table look "referenced" and breaks the reverse
    //   stale-allowlist check below. The original test lived in
    //   `rio-store/tests/migrations.rs` — outside the `src/`-only walk
    //   — so it never had this problem.
    const CORPUS_EXCLUDE: &[&str] = &["migrations.rs", "lint.rs"];
    let corpus_roots = [
        "rio-store/src",
        "rio-scheduler/src",
        "rio-controller/src",
        "xtask/src",
    ];
    let mut corpus = String::new();
    let mut total = 0usize;
    for rel in corpus_roots {
        let dir = root.join(rel);
        ensure!(
            dir.is_dir(),
            "schema-liveness corpus root {} not found",
            dir.display()
        );
        walk_rs(&dir, &mut |p| {
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| CORPUS_EXCLUDE.contains(&n))
            {
                return Ok(());
            }
            corpus.push_str(
                &fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
            );
            corpus.push('\n');
            total += 1;
            Ok(())
        })?;
    }
    // Non-empty sanity: a silently-empty corpus would make the loop
    // below vacuously fail every table — better to fail with a clear
    // message.
    ensure!(
        total > 50,
        "schema-liveness corpus suspiciously small ({total} files) — \
         expected ≥50 across rio-store + rio-scheduler + rio-controller + xtask"
    );

    let mut dead = Vec::new();
    for t in &live {
        if ALLOW_DEAD.iter().any(|(n, _)| n == t) {
            continue;
        }
        // Word-boundary match so `foo` doesn't satisfy on `foo_bar` /
        // `foobar`. Table names are `\w+` so `\b` is the right anchor.
        let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(t))).unwrap();
        if !re.is_match(&corpus) {
            dead.push(t.clone());
        }
    }
    if !dead.is_empty() {
        bail!(
            "table(s) declared in migrations/ but never referenced in \
             rio-store/rio-scheduler/rio-controller/xtask Rust source:\n    {dead:?}\n  \
             dead schema — delete the migration before it ships, or add to \
             ALLOW_DEAD in xtask/src/lint.rs with rationale",
        );
    }

    // Reverse check: ALLOW_DEAD entries that ARE now referenced (or no
    // longer exist) are stale — drop them so the allowlist doesn't
    // accrete.
    for &(t, _) in ALLOW_DEAD {
        ensure!(
            live.contains(t),
            "ALLOW_DEAD lists `{t}` but no live migration creates it — remove the entry",
        );
        let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(t))).unwrap();
        ensure!(
            !re.is_match(&corpus),
            "ALLOW_DEAD lists `{t}` but it IS referenced in Rust source — remove the entry",
        );
    }

    tracing::info!(
        live_tables = live.len(),
        allow_dead = ALLOW_DEAD.len(),
        corpus_files = total,
        "schema-liveness ok"
    );
    Ok(())
}

/// Parse one migration's SQL and apply its table-level DDL to `live`.
///
/// SQL `--` comment lines are stripped first (017 has the prose
/// "CREATE TABLE inline REFERENCES" in a comment, which would otherwise
/// extract a phantom `inline` table). No block comments exist in
/// `migrations/` today; if one appears the regex simply misses tables
/// inside it, which is a false negative, not a false positive —
/// acceptable.
///
/// The extraction regex recognizes (uppercase only, matching house
/// style):
///   - `CREATE TABLE [IF NOT EXISTS] <name>` — inserts `<name>`
///   - `ALTER TABLE <name>`                  — inserts `<name>`
///   - `DROP TABLE [IF EXISTS] <name>`       — removes `<name>`
///
/// It does **not** handle:
///   - `ALTER TABLE old RENAME TO new` — captures `old`, never tracks
///     `new`. The line still matches, so this is a *silent* gap; the
///     fail-loud guard below cannot catch it.
///   - schema-qualified names (`public.foo`) — captures `public`, not
///     `foo`. Also a silent gap (line still matches).
///   - `CREATE UNLOGGED/TEMP[ORARY] TABLE` — modifier between `CREATE`
///     and `TABLE` defeats the regex. Caught by the guard.
///   - lowercase DDL — regex is case-sensitive. Caught by the guard.
///
/// The fail-loud guard bails on any non-comment line that *looks* like
/// table DDL (case-insensitive `CREATE/ALTER/DROP … TABLE` prefix) but
/// the extraction regex doesn't match — so a novel DDL shape errors
/// instead of silently bypassing the liveness check. Non-table DDL
/// (`CREATE INDEX`/`VIEW`/`EXTENSION`/`FUNCTION`, `DROP CONSTRAINT`,
/// `ALTER COLUMN`/`INDEX` continuation lines, …) does not trip it.
fn scan_migration_ddl(sql: &str, path: &Path, live: &mut BTreeSet<String>) -> Result<()> {
    let ddl = regex::Regex::new(
        r"(?x)
          \b CREATE \s+ TABLE \s+ (?: IF \s+ NOT \s+ EXISTS \s+ )? (?<create> \w+ )
        | \b ALTER  \s+ TABLE \s+                                  (?<alter>  \w+ )
        | \b DROP   \s+ TABLE \s+ (?: IF \s+ EXISTS \s+ )?         (?<drop>   \w+ )
        ",
    )
    .unwrap();
    // Prefixes that mark a line as table DDL for the fail-loud guard.
    // Deliberately broader than the regex (UNLOGGED/TEMP[ORARY]) so the
    // shapes the regex CAN'T capture are the ones that bail.
    const TABLE_DDL_PREFIXES: &[&str] = &[
        "CREATE TABLE",
        "CREATE UNLOGGED TABLE",
        "CREATE TEMPORARY TABLE",
        "CREATE TEMP TABLE",
        "ALTER TABLE",
        "DROP TABLE",
    ];
    let mut stripped = String::new();
    for (i, line) in sql.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        if TABLE_DDL_PREFIXES.iter().any(|kw| upper.starts_with(kw)) && !ddl.is_match(line) {
            bail!(
                "{}: unrecognized table DDL at line {}: `{trimmed}` — the \
                 schema-liveness extraction regex doesn't handle this shape; \
                 update `scan_migration_ddl` in xtask/src/lint.rs (regex + \
                 doc-comment), or rephrase the migration to a recognized shape",
                path.display(),
                i + 1,
            );
        }
        stripped.push_str(line);
        stripped.push('\n');
    }
    for c in ddl.captures_iter(&stripped) {
        if let Some(t) = c.name("create").or_else(|| c.name("alter")) {
            live.insert(t.as_str().to_owned());
        } else if let Some(t) = c.name("drop") {
            live.remove(t.as_str());
        }
    }
    Ok(())
}

/// Helm `[sla]` chart-coverage guard: every entry in
/// `HELM_RENDERED_SLA_KEYS` appears as a substring of the scheduler
/// helm template.
///
/// Class-level guard for merged_bug_056 (helm forgot `hw_cost_source`
/// → §13a unreachable in production). The companion check — every
/// `SlaConfig` serde field is classified as RENDERED or NOT_RENDERED —
/// stays as a unit test (`helm_keys_complete` in
/// `rio-scheduler/src/sla/config.rs`) because it's a pure serde
/// round-trip with no file read. Splitting the chart-coverage half here
/// removes the only `include_str!` reaching from `rio-scheduler/src/`
/// into `infra/helm/`, which forced a cross-directory fileset symlink
/// in `nix/crate2nix.nix`.
fn helm_sla() -> Result<()> {
    use rio_scheduler::sla::config::HELM_RENDERED_SLA_KEYS;

    let tpl_path = repo_root().join("infra/helm/rio-build/templates/scheduler.yaml");
    let tpl =
        fs::read_to_string(&tpl_path).with_context(|| format!("reading {}", tpl_path.display()))?;
    for k in HELM_RENDERED_SLA_KEYS {
        ensure!(
            tpl.contains(k),
            "[sla] key `{k}` not rendered by {} — add a `{{{{- with .X }}}}` block, \
             or move it to HELM_NOT_RENDERED_SLA_KEYS in \
             rio-scheduler/src/sla/config.rs with a rationale",
            tpl_path.display()
        );
    }
    tracing::info!(
        rendered_keys = HELM_RENDERED_SLA_KEYS.len(),
        template = %tpl_path.display(),
        "helm-sla ok"
    );
    Ok(())
}

/// Seccomp profile structure guard for the builder and fetcher
/// `Localhost` profiles.
///
/// Both profiles are `type: Localhost` k8s seccomp profiles baked into
/// the NixOS node AMI (ADR-021) and force-applied to every
/// builder/fetcher pod by `pool/pod.rs`. This lint asserts each stays
/// an ALLOWLIST and that the deny/allow sets don't regress. The
/// fetcher profile is the higher-risk one (fetchers face the open
/// internet) and adds `keyctl`/`add_key` to the deny set per ADR-019;
/// it must also carry the explicit `SCMP_ACT_ERRNO` block its
/// `//provenance` header documents — that block is what makes the
/// extra denies grep-able and robust against allowlist widening.
///
/// Lives here (a runtime file read) rather than as a unit test
/// (compile-time `include_str!`) so the per-crate nix sandbox doesn't
/// need a cross-directory symlink hack to resolve a path 5 levels
/// outside `rio-controller/`. Same rationale as `helm_sla` above. The
/// profile *files* stay at `nix/nixos-node/seccomp/`; they're
/// deployment config consumed by `hardening.nix`, `k3s-full.nix`, and
/// `xtask regen seccomp`.
// r[verify builder.seccomp.localhost-profile+3]
// r[verify fetcher.sandbox.strict-seccomp]
fn seccomp_allowlist() -> Result<()> {
    // Worker-critical syscalls — must be present in an ALLOW block. If
    // any of these regress the worker can't mount overlayfs / set up
    // the Nix sandbox. Same set for both profiles: the fetcher runs
    // the FOD build script inside the same sandbox.
    const NEEDED: &[&str] = &["mount", "unshare", "chroot", "clone", "umount2"];

    // Builder-only: the read-side trace syscalls must STAY in an ALLOW
    // block. Sanitizer/debugger check phases (LeakSanitizer's at-exit
    // stop-the-world, strace/gdb test suites) trace their own
    // descendants; Yama ptrace_scope=1 confines the capability to
    // exactly that. A profile edit that drops these re-breaks every
    // such build. NOT folded into NEEDED: the fetcher profile denies
    // them (see FETCHER_EXTRA_DENIED).
    const BUILDER_NEEDED_TRACE: &[&str] = &["ptrace", "process_vm_readv"];

    // Fetcher-only ADR-019 denies, on top of the builder DENIED set.
    // `ptrace`/`process_vm_readv` are allowed in the BUILDER profile
    // (read-side tracing for check phases — see regen::seccomp::DENIED)
    // but stay denied here: FOD fetch scripts have no check phase, and
    // the fetcher faces the open internet.
    const FETCHER_EXTRA_DENIED: &[&str] = &["keyctl", "add_key", "ptrace", "process_vm_readv"];

    let fetcher_denied: Vec<&str> = crate::regen::seccomp::DENIED
        .iter()
        .chain(FETCHER_EXTRA_DENIED)
        .copied()
        .collect();

    let builder_needed: Vec<&str> = NEEDED.iter().chain(BUILDER_NEEDED_TRACE).copied().collect();

    check_seccomp_profile(
        "nix/nixos-node/seccomp/rio-builder.json",
        crate::regen::seccomp::DENIED,
        &builder_needed,
        // Builder profile relies on defaultAction ERRNO alone; no
        // explicit ERRNO block required.
        &[],
    )?;
    check_seccomp_profile(
        "nix/nixos-node/seccomp/rio-fetcher.json",
        &fetcher_denied,
        NEEDED,
        // The fetcher profile MUST keep its explicit ERRNO block — see
        // its `//provenance` header. The block is redundant with
        // defaultAction (none of these are in an ALLOW block) but it
        // is the grep-able guard against allowlist widening.
        &fetcher_denied,
    )?;
    Ok(())
}

/// Validate one seccomp `Localhost` profile.
///
/// `must_explicit_errno`: syscalls that MUST appear in an explicit
/// `SCMP_ACT_ERRNO` block, in addition to being absent from ALLOW.
/// Pass `&[]` when defaultAction-ERRNO alone is sufficient.
fn check_seccomp_profile(
    rel_path: &str,
    denied: &[&str],
    needed: &[&str],
    must_explicit_errno: &[&str],
) -> Result<()> {
    let path = repo_root().join(rel_path);
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let profile: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    // The profile must be an ALLOWLIST (defaultAction ERRNO). A
    // denylist (defaultAction ALLOW + explicit ERRNO for the deny
    // targets) would be a security REGRESSION vs RuntimeDefault — it
    // would re-enable the ~40 syscalls RuntimeDefault blocks
    // (kexec_load, open_by_handle_at, userfaultfd etc). K8s
    // type: Localhost REPLACES RuntimeDefault; it doesn't stack.
    ensure!(
        profile["defaultAction"] == "SCMP_ACT_ERRNO",
        "{}: defaultAction is {}, want \"SCMP_ACT_ERRNO\" — profile must be an \
         allowlist; a denylist regresses vs RuntimeDefault (Audit B1 #12)",
        path.display(),
        profile["defaultAction"],
    );
    ensure!(
        profile["defaultErrnoRet"] == 1,
        "{}: defaultErrnoRet is {}, want 1 (EPERM — the standard \
         'operation not permitted' errno)",
        path.display(),
        profile["defaultErrnoRet"],
    );

    let blocks = profile["syscalls"]
        .as_array()
        .with_context(|| format!("{}: `syscalls` is not an array", path.display()))?;

    // Collect every syscall that appears in any ALLOW block. The
    // denied syscalls must be absent from ALL of them — defaultAction
    // ERRNO is what denies them.
    let allowed: BTreeSet<&str> = blocks
        .iter()
        .filter(|b| b["action"] == "SCMP_ACT_ALLOW")
        .flat_map(|b| b["names"].as_array().into_iter().flatten())
        .filter_map(|n| n.as_str())
        .collect();
    let explicit_errno: BTreeSet<&str> = blocks
        .iter()
        .filter(|b| b["action"] == "SCMP_ACT_ERRNO")
        .flat_map(|b| b["names"].as_array().into_iter().flatten())
        .filter_map(|n| n.as_str())
        .collect();

    for d in denied {
        ensure!(
            !allowed.contains(d),
            "{}: `{d}` appears in an ALLOW block — must be ABSENT \
             (denied via defaultAction ERRNO)",
            path.display(),
        );
    }
    for d in must_explicit_errno {
        ensure!(
            explicit_errno.contains(d),
            "{}: `{d}` missing from the explicit SCMP_ACT_ERRNO block — \
             the profile's provenance header documents it as required \
             (grep-able guard against allowlist widening)",
            path.display(),
        );
    }
    for n in needed {
        ensure!(
            allowed.contains(n),
            "{}: `{n}` missing from ALLOW blocks — the executor needs it \
             (overlayfs/Nix-sandbox setup, or descendant tracing for \
             sanitizer/debugger check phases — see NEEDED / \
             BUILDER_NEEDED_TRACE in xtask/src/lint.rs)",
            path.display(),
        );
    }

    tracing::info!(
        allowed_syscalls = allowed.len(),
        denied = denied.len(),
        explicit_errno = must_explicit_errno.len(),
        needed = needed.len(),
        profile = %path.display(),
        "seccomp-allowlist ok"
    );
    Ok(())
}

// r[verify sched.db.table-retention+1]
/// `RetentionTruth`: every registry policy claim resolves — see the
/// `Lint` variant doc. KeepForever rows are exempt by construction
/// (their rationale IS the decision; nothing to resolve).
fn retention_truth() -> Result<()> {
    use rio_migrations::retention::{RETENTION_REGISTRY, RetentionPolicy};
    let root = repo_root();
    let corpus = retention_corpus(root)?;
    let mut violations = Vec::new();
    for (table, policy) in RETENTION_REGISTRY {
        let res = match policy {
            RetentionPolicy::SweptBy { symbol, .. } => check_swept_by(table, symbol, &corpus),
            RetentionPolicy::CascadeFrom {
                parent, migration, ..
            } => check_cascade(table, parent, migration, root),
            RetentionPolicy::KeepForever(_) => Ok(()),
        };
        if let Err(e) = res {
            violations.push(format!("  {table}: {e:#}"));
        }
    }
    ensure!(
        violations.is_empty(),
        "retention registry misattributions ({}):
{}",
        violations.len(),
        violations.join(
            "
"
        )
    );
    tracing::info!(tables = RETENTION_REGISTRY.len(), "retention-truth ok");
    Ok(())
}

/// The non-test corpus for sweeper-symbol resolution: production `.rs`
/// of the PG-touching crates, structurally cut (merged_bug_021 — the
/// old corpus split on the literal `#[cfg(test)]\nmod tests` marker
/// and kept comments, so a cfg(test)-gated `mbt_tests.rs` entered
/// whole and a `// DELETE FROM …` comment counted as production
/// deletion evidence). Per file: [`corpus_text`].
fn retention_corpus(root: &Path) -> Result<Vec<(std::path::PathBuf, String)>> {
    const CRATES: &[&str] = &[
        "rio-store",
        "rio-scheduler",
        "rio-controller",
        "rio-gateway",
        "rio-auth",
        "rio-builder",
        "rio-common",
    ];
    let mut corpus = Vec::new();
    let mut files = 0usize;
    let mut skipped = 0usize;
    for krate in CRATES {
        let dir = root.join(krate).join("src");
        if !dir.exists() {
            continue;
        }
        walk_rs(&dir, &mut |path| {
            if path.components().any(|c| c.as_os_str() == "tests") {
                return Ok(());
            }
            files += 1;
            let raw =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            match corpus_text(path, &raw)
                .with_context(|| format!("retention corpus: {}", path.display()))?
            {
                Some(norm) => corpus.push((path.to_owned(), norm)),
                None => skipped += 1,
            }
            Ok(())
        })?;
    }
    tracing::info!(
        files,
        skipped_test_files = skipped,
        corpus_files = corpus.len(),
        "retention corpus (AST-cut, comment-free)"
    );
    Ok(corpus)
}

/// One file's corpus contribution: `None` for cfg(test)-included
/// module files (the `tests.rs` / `*_tests.rs` naming convention —
/// from inside such a file the gate in the parent is invisible);
/// otherwise the syn-parsed item tree with every `#[cfg(test)]`-gated
/// item pruned (recursively: inline mods and impl items too), rendered
/// back to tokens — comments cannot survive a token render — and
/// normalized for statement grepping.
fn corpus_text(path: &Path, raw: &str) -> Result<Option<String>> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name == "tests.rs" || name.ends_with("_tests.rs") {
        return Ok(None);
    }
    let file: syn::File = syn::parse_file(raw).context("syn parse")?;
    let mut items = file.items;
    prune_cfg_test_items(&mut items);
    // Statement-level `#[cfg(test)]` (a test-only call inside a
    // production fn body — gc/mark_scan_bench.rs, logs/chunks.rs) is
    // pruned by a full visit_mut walk over every block; the corpus
    // floor below remains the tripwire for any shape this misses.
    {
        use syn::visit_mut::VisitMut;
        let mut pruner = CfgTestStmtPrune;
        for item in &mut items {
            pruner.visit_item_mut(item);
        }
    }
    // Doc comments are `#[doc = "…"]` ATTRIBUTES in the token stream —
    // they survive a token render, so without this strip `/// DELETE
    // FROM x` would still count as deletion evidence and prose
    // mentioning cfg(test) would trip the corpus floor (the gate's
    // first run caught exactly that on gc/mark_scan_bench.rs).
    let rendered = items
        .iter()
        .map(|i| strip_doc_attrs(quote::quote!(#i)).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    // Floor: nothing cfg(test)-gated may remain in the corpus — if the
    // prune logic ever rots, fail the lint loudly instead of silently
    // re-admitting test statements as production evidence.
    if let Some(at) = rendered
        .find("cfg (test")
        .or_else(|| rendered.find("cfg(test"))
    {
        let lo = at.saturating_sub(80);
        let hi = rendered.len().min(at + 80);
        anyhow::bail!(
            "cfg(test) item survived the corpus prune near: …{}…",
            &rendered[lo..hi]
        );
    }
    Ok(Some(normalize_decl_text(&rendered)))
}

/// Recursively drop items gated behind `#[cfg(test)]` (or any `cfg`
/// whose argument tokens mention the bare `test` ident — `cfg(any(test,
/// …))` prunes too; `cfg(feature = "test-utils")` does not, since
/// `"test-utils"` is a literal, not an ident).
fn prune_cfg_test_items(items: &mut Vec<syn::Item>) {
    items.retain(|i| !item_attrs(i).is_some_and(is_cfg_test_attrs));
    for item in items {
        match item {
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &mut m.content {
                    prune_cfg_test_items(inner);
                }
            }
            syn::Item::Impl(im) => {
                im.items
                    .retain(|ii| !impl_item_attrs(ii).is_some_and(is_cfg_test_attrs));
            }
            _ => {}
        }
    }
}

fn item_attrs(i: &syn::Item) -> Option<&[syn::Attribute]> {
    use syn::Item::*;
    Some(match i {
        Const(x) => &x.attrs,
        Enum(x) => &x.attrs,
        ExternCrate(x) => &x.attrs,
        Fn(x) => &x.attrs,
        ForeignMod(x) => &x.attrs,
        Impl(x) => &x.attrs,
        Macro(x) => &x.attrs,
        Mod(x) => &x.attrs,
        Static(x) => &x.attrs,
        Struct(x) => &x.attrs,
        Trait(x) => &x.attrs,
        TraitAlias(x) => &x.attrs,
        Type(x) => &x.attrs,
        Union(x) => &x.attrs,
        Use(x) => &x.attrs,
        _ => return None,
    })
}

fn impl_item_attrs(i: &syn::ImplItem) -> Option<&[syn::Attribute]> {
    use syn::ImplItem::*;
    Some(match i {
        Const(x) => &x.attrs,
        Fn(x) => &x.attrs,
        Type(x) => &x.attrs,
        Macro(x) => &x.attrs,
        _ => return None,
    })
}

struct CfgTestStmtPrune;

impl syn::visit_mut::VisitMut for CfgTestStmtPrune {
    fn visit_block_mut(&mut self, block: &mut syn::Block) {
        block.stmts.retain(|s| !stmt_is_cfg_test(s));
        syn::visit_mut::visit_block_mut(self, block);
    }

    fn visit_item_enum_mut(&mut self, e: &mut syn::ItemEnum) {
        e.variants = std::mem::take(&mut e.variants)
            .into_iter()
            .filter(|v| !is_cfg_test_attrs(&v.attrs))
            .collect();
        syn::visit_mut::visit_item_enum_mut(self, e);
    }

    fn visit_expr_match_mut(&mut self, m: &mut syn::ExprMatch) {
        m.arms.retain(|a| !is_cfg_test_attrs(&a.attrs));
        syn::visit_mut::visit_expr_match_mut(self, m);
    }

    fn visit_expr_struct_mut(&mut self, e: &mut syn::ExprStruct) {
        e.fields = std::mem::take(&mut e.fields)
            .into_iter()
            .filter(|fv| !is_cfg_test_attrs(&fv.attrs))
            .collect();
        syn::visit_mut::visit_expr_struct_mut(self, e);
    }

    fn visit_pat_struct_mut(&mut self, e: &mut syn::PatStruct) {
        e.fields = std::mem::take(&mut e.fields)
            .into_iter()
            .filter(|fp| !is_cfg_test_attrs(&fp.attrs))
            .collect();
        syn::visit_mut::visit_pat_struct_mut(self, e);
    }

    fn visit_fields_named_mut(&mut self, f: &mut syn::FieldsNamed) {
        f.named = std::mem::take(&mut f.named)
            .into_iter()
            .filter(|fl| !is_cfg_test_attrs(&fl.attrs))
            .collect();
        syn::visit_mut::visit_fields_named_mut(self, f);
    }

    fn visit_item_trait_mut(&mut self, t: &mut syn::ItemTrait) {
        t.items
            .retain(|ti| !trait_item_attrs(ti).is_some_and(is_cfg_test_attrs));
        syn::visit_mut::visit_item_trait_mut(self, t);
    }
}

fn trait_item_attrs(i: &syn::TraitItem) -> Option<&[syn::Attribute]> {
    use syn::TraitItem::*;
    Some(match i {
        Const(x) => &x.attrs,
        Fn(x) => &x.attrs,
        Type(x) => &x.attrs,
        Macro(x) => &x.attrs,
        _ => return None,
    })
}

fn stmt_is_cfg_test(s: &syn::Stmt) -> bool {
    match s {
        syn::Stmt::Local(l) => is_cfg_test_attrs(&l.attrs),
        syn::Stmt::Item(i) => item_attrs(i).is_some_and(is_cfg_test_attrs),
        syn::Stmt::Expr(e, _) => expr_attrs(e).is_some_and(is_cfg_test_attrs),
        syn::Stmt::Macro(m) => is_cfg_test_attrs(&m.attrs),
    }
}

fn expr_attrs(e: &syn::Expr) -> Option<&[syn::Attribute]> {
    use syn::Expr::*;
    Some(match e {
        Array(x) => &x.attrs,
        Assign(x) => &x.attrs,
        Async(x) => &x.attrs,
        Await(x) => &x.attrs,
        Binary(x) => &x.attrs,
        Block(x) => &x.attrs,
        Call(x) => &x.attrs,
        Cast(x) => &x.attrs,
        Field(x) => &x.attrs,
        ForLoop(x) => &x.attrs,
        If(x) => &x.attrs,
        Index(x) => &x.attrs,
        Let(x) => &x.attrs,
        Loop(x) => &x.attrs,
        Macro(x) => &x.attrs,
        Match(x) => &x.attrs,
        MethodCall(x) => &x.attrs,
        Paren(x) => &x.attrs,
        Path(x) => &x.attrs,
        Range(x) => &x.attrs,
        Reference(x) => &x.attrs,
        Return(x) => &x.attrs,
        Try(x) => &x.attrs,
        TryBlock(x) => &x.attrs,
        Tuple(x) => &x.attrs,
        Unary(x) => &x.attrs,
        Unsafe(x) => &x.attrs,
        While(x) => &x.attrs,
        _ => return None,
    })
}

fn is_cfg_test_attrs(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<proc_macro2::TokenStream>()
                .map(tokens_mention_test_ident)
                .unwrap_or(false)
    })
}

/// Drop every `#[doc = …]` attribute group (recursively, so impl/mod
/// bodies are covered) — doc text is commentary, never production
/// deletion evidence.
fn strip_doc_attrs(ts: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let mut out: Vec<proc_macro2::TokenTree> = Vec::new();
    let mut iter = ts.into_iter().peekable();
    while let Some(tt) = iter.next() {
        if let proc_macro2::TokenTree::Punct(p) = &tt
            && p.as_char() == '#'
            && let Some(proc_macro2::TokenTree::Group(g)) = iter.peek()
            && g.delimiter() == proc_macro2::Delimiter::Bracket
            && matches!(
                g.stream().into_iter().next(),
                Some(proc_macro2::TokenTree::Ident(id)) if id == "doc"
            )
        {
            iter.next(); // drop the [doc = …] group with its `#`
            continue;
        }
        out.push(match tt {
            proc_macro2::TokenTree::Group(g) => proc_macro2::TokenTree::Group(
                proc_macro2::Group::new(g.delimiter(), strip_doc_attrs(g.stream())),
            ),
            other => other,
        });
    }
    out.into_iter().collect()
}

fn tokens_mention_test_ident(ts: proc_macro2::TokenStream) -> bool {
    ts.into_iter().any(|tt| match tt {
        proc_macro2::TokenTree::Ident(id) => id == "test",
        proc_macro2::TokenTree::Group(g) => tokens_mention_test_ident(g.stream()),
        _ => false,
    })
}

/// Collapse Rust string-literal line continuations (`\` at EOL) and
/// all whitespace runs to single spaces, so
/// `"DELETE FROM \`↵`     drv_log_chunks …"` and multi-line `r#"…"#`
/// SQL both normalize to greppable single-spaced text.
fn normalize_decl_text(s: &str) -> String {
    s.replace("\\\n", "\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `"{prefix} {table}"` occurs with a word boundary after the table
/// name (so `chunks` cannot satisfy a `chunks_archive` statement).
fn contains_stmt(norm: &str, prefix: &str, table: &str) -> bool {
    let pat = format!("{prefix} {table}");
    let mut from = 0;
    while let Some(i) = norm[from..].find(&pat) {
        let end = from + i + pat.len();
        let bounded = norm
            .as_bytes()
            .get(end)
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'));
        if bounded {
            return true;
        }
        from = end;
    }
    false
}

/// A `TRUNCATE` statement whose (comma-separated, possibly
/// `TABLE`-prefixed) list names `table` — `TRUNCATE build_samples,
/// hw_perf_samples` must satisfy BOTH tables.
fn truncate_list_names(norm: &str, table: &str) -> bool {
    let mut from = 0;
    while let Some(i) = norm[from..].find("TRUNCATE ") {
        let start = from + i + "TRUNCATE ".len();
        let window = &norm[start..norm.len().min(start + 200)];
        let end = window.find(['"', ';', '\'']).unwrap_or(window.len());
        if window[..end]
            .split([',', ' '])
            .any(|tok| tok.trim() == table)
        {
            return true;
        }
        from = start;
    }
    false
}

/// One `SweptBy` claim: the symbol defines somewhere in the corpus,
/// and a defining file carries the deleting statement for `table`.
fn check_swept_by(
    table: &str,
    symbol: &str,
    corpus: &[(std::path::PathBuf, String)],
) -> Result<()> {
    let defs: Vec<_> = corpus
        .iter()
        .filter(|(_, norm)| defines_fn(norm, symbol))
        .collect();
    ensure!(
        !defs.is_empty(),
        "sweeper symbol `{symbol}` defines nowhere in non-test workspace source"
    );
    let hit = defs.iter().any(|(_, norm)| {
        contains_stmt(norm, "DELETE FROM", table) || truncate_list_names(norm, table)
    });
    ensure!(
        hit,
        "`{symbol}` defines ({} file(s)) but no defining file deletes `{table}` (no `DELETE FROM {table}` and no TRUNCATE naming it)",
        defs.len()
    );
    Ok(())
}

/// `fn {symbol}` followed (over optional whitespace) by `(` or `<` —
/// tolerant of token-render spacing (`fn retention_sweep (pool …`),
/// strict on the symbol's word boundary.
fn defines_fn(norm: &str, symbol: &str) -> bool {
    let pat = format!("fn {symbol}");
    let mut from = 0;
    while let Some(i) = norm[from..].find(&pat) {
        let end = from + i + pat.len();
        let rest = norm[end..].trim_start();
        if rest.starts_with('(') || rest.starts_with('<') {
            return true;
        }
        from = end;
    }
    false
}

/// The CHILD table's own `CREATE/ALTER TABLE {table}` statement must
/// carry `REFERENCES {parent}` whose FK clause (up to the next
/// top-level `,` / end of statement) says `ON DELETE CASCADE`
/// (merged_bug_021 — the old scan never bound the child table at all:
/// a flat 220-char window from ANY `REFERENCES {parent}` in the file
/// let a NEIGHBORING table's CASCADE satisfy a RESTRICT parent ref).
fn cascade_clause_ok(norm: &str, table: &str, parent: &str) -> bool {
    for prefix in [
        format!("CREATE TABLE {table}"),
        format!("CREATE TABLE IF NOT EXISTS {table}"),
        format!("ALTER TABLE {table}"),
        format!("ALTER TABLE ONLY {table}"),
    ] {
        let mut from = 0;
        while let Some(i) = norm[from..].find(&prefix) {
            let at = from + i;
            let bend = at + prefix.len();
            let bounded = norm
                .as_bytes()
                .get(bend)
                .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'));
            let stmt_end = norm[at..].find(';').map_or(norm.len(), |e| at + e);
            if bounded && stmt_fk_cascade(&norm[at..stmt_end], parent) {
                return true;
            }
            from = bend;
        }
    }
    false
}

/// Within ONE statement: `REFERENCES {parent}` whose FK clause — the
/// span from the match to the next comma at paren depth 0 (or the
/// closing paren of the column list / end of statement) — contains
/// `ON DELETE CASCADE`. `ON DELETE RESTRICT` does NOT satisfy — the
/// phantom class.
fn stmt_fk_cascade(stmt: &str, parent: &str) -> bool {
    let pat = format!("REFERENCES {parent}");
    let mut from = 0;
    while let Some(i) = stmt[from..].find(&pat) {
        let at = from + i;
        let end = at + pat.len();
        let bounded = stmt
            .as_bytes()
            .get(end)
            .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'));
        if bounded {
            let bytes = stmt.as_bytes();
            let mut depth = 0i32;
            let mut j = end;
            while j < bytes.len() {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    b',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            if stmt[end..j].contains("ON DELETE CASCADE") {
                return true;
            }
        }
        from = end;
    }
    false
}

/// One `CascadeFrom` claim against the named migration file.
fn check_cascade(table: &str, parent: &str, migration: &str, root: &Path) -> Result<()> {
    let path = root.join("rio-migrations/migrations").join(migration);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("CascadeFrom migration for `{table}`: {}", path.display()))?;
    ensure!(
        cascade_clause_ok(&normalize_decl_text(&text), table, parent),
        "{migration}: `{table}`'s own CREATE/ALTER statement carries no `REFERENCES {parent} … ON DELETE CASCADE` FK clause (missing or RESTRICT — the phantom-cascade class)"
    );
    Ok(())
}

/// Recursive `.rs` walk via `std` (no `walkdir` dep). Follows symlinks
/// — under the nix flake check, the corpus dirs are staged into a
/// store-path source tree and may be symlinked.
fn walk_rs(dir: &Path, f: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // `metadata()` (not `file_type()`) so symlinked dirs recurse.
        let md = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if md.is_dir() {
            walk_rs(&path, f)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            f(&path)?;
        }
    }
    Ok(())
}

// `lint.rs` is in `CORPUS_EXCLUDE`, so the synthetic table names below
// can't leak into the schema-liveness corpus and mask a real dead table.
#[cfg(test)]
mod tests {
    use super::*;

    /// The normalizer collapses Rust string-literal line continuations
    /// and whitespace runs — a table name split across a continuation
    /// still resolves.
    #[test]
    fn normalizer_handles_line_continuations() {
        let src = "let q = \"DELETE FROM \\\n         drv_log_chunks WHERE exec_id = ANY($1)\";";
        let norm = normalize_decl_text(src);
        assert!(
            contains_stmt(&norm, "DELETE FROM", "drv_log_chunks"),
            "{norm}"
        );
        let raw = "sqlx::query(r#\"\n    DELETE FROM realisations\n    WHERE x = 1\n\"#)";
        assert!(contains_stmt(
            &normalize_decl_text(raw),
            "DELETE FROM",
            "realisations"
        ));
    }

    /// Word boundary: `chunks` must not satisfy `chunks_archive`.
    #[test]
    fn stmt_match_is_word_bounded() {
        let norm = normalize_decl_text("\"DELETE FROM chunks_archive WHERE 1=1\"");
        assert!(!contains_stmt(&norm, "DELETE FROM", "chunks"));
        assert!(contains_stmt(&norm, "DELETE FROM", "chunks_archive"));
    }

    /// `TRUNCATE a, b` satisfies BOTH tables; `TRUNCATE TABLE x` works;
    /// an absent table does not.
    #[test]
    fn truncate_comma_list_resolves_members() {
        let norm = normalize_decl_text("query(\"TRUNCATE build_samples, hw_perf_samples\")");
        assert!(truncate_list_names(&norm, "build_samples"));
        assert!(truncate_list_names(&norm, "hw_perf_samples"));
        assert!(!truncate_list_names(&norm, "interrupt_samples"));
        assert!(truncate_list_names(
            &normalize_decl_text("\"TRUNCATE TABLE jwt_revoked\""),
            "jwt_revoked"
        ));
    }

    /// The planted-phantom battery: a symbol that defines nowhere, a
    /// symbol whose file does not delete the table, and the good case.
    #[test]
    fn swept_by_rejects_phantom_attributions() {
        let corpus = vec![
            (
                std::path::PathBuf::from("fake/sweeper.rs"),
                normalize_decl_text(
                    "pub async fn real_sweep() { query(\"DELETE FROM widgets WHERE old\") }",
                ),
            ),
            (
                std::path::PathBuf::from("fake/other.rs"),
                normalize_decl_text("pub fn idle_helper() { /* no SQL */ }"),
            ),
        ];
        // Good: symbol defines and its file deletes the table.
        check_swept_by("widgets", "real_sweep", &corpus).expect("good claim resolves");
        // Phantom symbol: defines nowhere.
        let e = check_swept_by("widgets", "ghost_sweep", &corpus).unwrap_err();
        assert!(e.to_string().contains("defines nowhere"), "{e:#}");
        // Misattributed: symbol exists, file deletes nothing.
        let e = check_swept_by("widgets", "idle_helper", &corpus).unwrap_err();
        assert!(e.to_string().contains("no defining file deletes"), "{e:#}");
    }

    /// CASCADE resolves; RESTRICT (merged_bug_142's phantom) does not;
    /// the parent name is word-bounded.
    #[test]
    fn cascade_requires_actual_cascade() {
        let cascade = normalize_decl_text(
            "ALTER TABLE tenant_keys\n  ADD CONSTRAINT k FOREIGN KEY (tenant_id)\n  \
             REFERENCES tenants(tenant_id)\n  ON DELETE CASCADE;",
        );
        assert!(cascade_clause_ok(&cascade, "tenant_keys", "tenants"));
        let restrict = normalize_decl_text(
            "CREATE TABLE realisation_refs (\n  drv_hash bytea,\n  \
             FOREIGN KEY (drv_hash) REFERENCES \
             realisations(drv_hash) ON DELETE RESTRICT\n);",
        );
        assert!(!cascade_clause_ok(
            &restrict,
            "realisation_refs",
            "realisations"
        ));
        assert!(!cascade_clause_ok(&cascade, "tenant_keys", "tenant"));
    }

    /// merged_bug_021 red (statement scoping): a NEIGHBORING table's
    /// CASCADE on the same parent must not satisfy the child's claim —
    /// pre-fix, a flat 220-char window from ANY `REFERENCES parent` in
    /// the file accepted exactly this; the child table name was never
    /// consulted at all.
    #[test]
    fn cascade_neighbor_fk_cannot_satisfy_child_claim() {
        let two_tables = normalize_decl_text(
            "CREATE TABLE other_refs (x uuid REFERENCES parent_t(id) ON DELETE CASCADE);\n\
             CREATE TABLE child_refs (y uuid REFERENCES parent_t(id) ON DELETE RESTRICT);",
        );
        assert!(cascade_clause_ok(&two_tables, "other_refs", "parent_t"));
        assert!(!cascade_clause_ok(&two_tables, "child_refs", "parent_t"));
        // Two FKs in ONE statement: the sibling column's CASCADE on a
        // different parent must not leak into this parent's clause.
        let two_fks = normalize_decl_text(
            "CREATE TABLE child2 (a uuid REFERENCES p1(id) ON DELETE RESTRICT, \
             b uuid REFERENCES p2(id) ON DELETE CASCADE);",
        );
        assert!(!cascade_clause_ok(&two_fks, "child2", "p1"));
        assert!(cascade_clause_ok(&two_fks, "child2", "p2"));
    }

    /// merged_bug_021 reds (corpus cut): comments and cfg(test) items —
    /// under ANY mod name, at item or impl-item level — cannot count as
    /// production deletion evidence; cfg(test)-included module FILES
    /// are skipped whole; string literals survive the token render.
    #[test]
    fn corpus_drops_comments_and_test_items() {
        let p = std::path::Path::new("fake/prod.rs");
        let norm = corpus_text(
            p,
            "// fn reap_widgets() runs DELETE FROM widgets nightly\npub fn live() {}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        let norm = corpus_text(
            p,
            "pub fn live() {}\n#[cfg(test)]\nmod harness {\n    const Q: &str = \"DELETE FROM widgets\";\n}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        let norm = corpus_text(
            p,
            "struct S;\nimpl S {\n    #[cfg(test)]\n    fn t(&self) { let _ = \"DELETE FROM widgets\"; }\n    fn live(&self) {}\n}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        let norm = corpus_text(p, "pub fn sweep() { let _ = \"DELETE FROM widgets\"; }\n")
            .unwrap()
            .unwrap();
        assert!(norm.contains("DELETE FROM widgets"), "{norm}");
        assert!(defines_fn(&norm, "sweep"));
        assert!(
            corpus_text(std::path::Path::new("fake/mbt_tests.rs"), "pub fn x() {}")
                .unwrap()
                .is_none()
        );
        assert!(
            corpus_text(std::path::Path::new("fake/tests.rs"), "pub fn x() {}")
                .unwrap()
                .is_none()
        );
    }

    /// Gate-red shapes (first full-gate run caught all five): doc
    /// comments are #[doc] ATTRIBUTES and survive a token render, and
    /// cfg(test) gates appear on statements, enum variants, match
    /// arms, struct-literal fields, and destructuring-pattern fields —
    /// every one must be pruned (or doc-stripped), and the corpus
    /// floor is the tripwire for any shape still missing.
    #[test]
    fn corpus_prunes_every_cfg_test_position_and_doc_text() {
        let p = std::path::Path::new("fake/prod.rs");
        // Doc text is never evidence (and prose mentioning cfg(test)
        // must not trip the floor).
        let norm = corpus_text(
            p,
            "/// uses the cfg(test) override; runs DELETE FROM widgets\npub fn live() {}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        // Statement-level cfg(test) inside a production fn body.
        let norm = corpus_text(
            p,
            "pub fn live() {\n    #[cfg(test)]\n    recorder(\"DELETE FROM widgets\");\n    real();\n}\nfn real() {}\nfn recorder(_s: &str) {}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        // Enum variant + match arm.
        let norm = corpus_text(
            p,
            "pub enum E {\n    Live,\n    #[cfg(test)]\n    T(&'static str),\n}\npub fn f(e: E) {\n    match e {\n        E::Live => {}\n        #[cfg(test)]\n        E::T(q) => { let _ = (q, \"DELETE FROM widgets\"); }\n    }\n}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
        // Struct-literal field value + destructuring-pattern field.
        let norm = corpus_text(
            p,
            "pub struct S {\n    a: u8,\n    #[cfg(test)]\n    q: &'static str,\n}\npub fn f() -> S {\n    S { a: 1, #[cfg(test)] q: \"DELETE FROM widgets\" }\n}\npub fn g(s: S) {\n    let S { a: _, #[cfg(test)] q: _ } = s;\n}\n",
        )
        .unwrap()
        .unwrap();
        assert!(!norm.contains("DELETE FROM widgets"), "{norm}");
    }

    /// `Lint::all()` must list every enum variant, or `xtask lint`
    /// (and the `xtask-lint` flake check) silently skip the new lint.
    /// The compiler enforces the `run()` match arm; this test enforces
    /// `all()`. Ground truth is clap's own subcommand registry —
    /// `augment_subcommands` registers one subcommand per variant — so
    /// the test self-updates when a variant is added.
    #[test]
    fn all_returns_every_variant() {
        let cmd = <Lint as clap::Subcommand>::augment_subcommands(clap::Command::new("lint"));
        let clap_variants: Vec<_> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        assert_eq!(
            Lint::all().len(),
            clap_variants.len(),
            "Lint::all() lists {} lint(s) but the Lint enum has {} variant(s) \
             ({clap_variants:?}). Add the missing variant to `Lint::all()` so \
             `xtask lint` runs it.",
            Lint::all().len(),
            clap_variants.len(),
        );
    }

    fn scan(sql: &str) -> Result<BTreeSet<String>> {
        let mut live = BTreeSet::new();
        scan_migration_ddl(sql, Path::new("synthetic.sql"), &mut live)?;
        Ok(live)
    }

    #[test]
    fn extracts_create_alter_drop() {
        let live = scan(
            "CREATE TABLE foo (id INT);\n\
             ALTER TABLE bar ADD COLUMN x INT;\n\
             CREATE TABLE IF NOT EXISTS baz (id INT);\n\
             DROP TABLE foo;\n\
             DROP TABLE IF EXISTS gone;",
        )
        .unwrap();
        assert_eq!(
            live,
            BTreeSet::from(["bar".to_owned(), "baz".to_owned()]),
            "CREATE then DROP cancels; ALTER and IF NOT EXISTS recognized"
        );
    }

    #[test]
    fn comment_lines_and_non_table_ddl_pass_through() {
        // None of these should bail OR extract a table.
        let live = scan(
            "-- CREATE TABLE phantom (commented out)\n\
             CREATE INDEX foo_idx ON foo (x);\n\
             CREATE UNIQUE INDEX bar_idx ON bar (y);\n\
             CREATE VIEW v AS SELECT 1;\n\
             CREATE EXTENSION IF NOT EXISTS pgcrypto;\n\
             -- continuation lines from a multi-line ALTER TABLE\n\
                 ALTER COLUMN x TYPE BIGINT,\n\
                 DROP CONSTRAINT foo_pkey;\n\
             DROP INDEX foo_idx;\n\
             DROP VIEW v;",
        )
        .unwrap();
        assert!(live.is_empty(), "no table DDL extracted, got {live:?}");
    }

    #[test]
    fn fail_loud_guard_rejects_unrecognized_table_ddl() {
        // Shapes the regex can't match at all → guard must bail.
        for bad in [
            "CREATE UNLOGGED TABLE foo (id INT);",
            "CREATE TEMPORARY TABLE foo (id INT);",
            "CREATE TEMP TABLE foo (id INT);",
            "create table foo (id INT);", // lowercase
            "Drop Table foo;",            // mixed case
        ] {
            let err = scan(bad).unwrap_err();
            assert!(
                err.to_string().contains("unrecognized table DDL"),
                "expected guard to fire on `{bad}`, got: {err}"
            );
        }
    }

    #[test]
    fn known_silent_gaps_do_not_trip_the_guard() {
        // These ARE wrong (see scan_migration_ddl's doc-comment) but the
        // regex partially matches, so the guard can't catch them — pin
        // the behavior so a future regex tweak that changes it is
        // noticed.
        let live = scan("ALTER TABLE old RENAME TO new;").unwrap();
        assert_eq!(live, BTreeSet::from(["old".to_owned()]), "new not tracked");

        let live = scan("CREATE TABLE public.foo (id INT);").unwrap();
        assert_eq!(
            live,
            BTreeSet::from(["public".to_owned()]),
            "schema captured, not table"
        );
    }
}
