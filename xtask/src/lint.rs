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
    /// Every letter of every DB-CHECK alphabet (the `db_str_enum!`
    /// invocation surface) has at least one EXPRESSION-position
    /// constructor in production source, and every label value a
    /// metric HELP string describes has a same-crate string literal
    /// outside its own registration. Pattern-position arms
    /// (`Variant =>`, `| Variant`, `matches!` payloads, let-patterns)
    /// do not count — they consume the letter, they never produce it.
    ///
    /// The historical exemplar (live_061): `JobState::Obsolete` sat in
    /// the 078 CHECK alphabet from birth with exactly the enum
    /// declaration and a display match arm — zero writers for the
    /// system's whole life. The by-other-means resolutions laundered
    /// into `cancelled`, `resolved_total{outcome="obsolete"}` was
    /// zero-forever, and the unresolvable pending rows pinned the
    /// claimable listing as permanently-refusing heads. A
    /// pattern-position-blind census would have shipped that letter
    /// green; this lint reds it at the first gate.
    DeadAlphabetLetter,
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
            Lint::DeadAlphabetLetter,
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
        Lint::DeadAlphabetLetter => dead_alphabet_letter(),
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
/// `Lint` variant doc. KeepForever rows are a CHECKED NEGATIVE CLAIM
/// since bug_095 (no cascading FK survives the migration corpus; the
/// workspace `DELETE FROM` census matches the declared deleter) —
/// the prior `Ok(())` exemption let `gc_holds` ship "released, never
/// deleted" beside an `ON DELETE CASCADE` FK, CI-green.
fn retention_truth() -> Result<()> {
    use rio_migrations::retention::{RETENTION_REGISTRY, RetentionPolicy};
    let root = repo_root();
    let corpus = retention_corpus(root)?;
    let migrations = migration_corpus(root)?;
    let mut violations = Vec::new();
    for (table, policy) in RETENTION_REGISTRY {
        let res = match policy {
            RetentionPolicy::SweptBy { symbol, .. } => check_swept_by(table, symbol, &corpus),
            RetentionPolicy::CascadeFrom {
                parent, migration, ..
            } => check_cascade(table, parent, migration, root),
            RetentionPolicy::KeepForever(_, deleter) => {
                check_keep_forever(table, *deleter, &corpus, &migrations)
            }
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
        // merged_bug_086 arm 2: parent-cfg(test)-gated files are test
        // code regardless of their name — the mod graph decides.
        let gated = cfg_test_gated_files(&dir)?;
        walk_rs(&dir, &mut |path| {
            if path.components().any(|c| c.as_os_str() == "tests") {
                return Ok(());
            }
            if gated.contains(path) {
                skipped += 1;
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

/// Files whose `mod` declaration chain passes through a
/// cfg-test-gated declaration (merged_bug_086 arm 2: the old
/// exclusion was the `tests.rs`/`*_tests.rs` NAMING convention, so
/// parent-gated files like `test_helpers.rs`, `fixtures.rs`,
/// `actor/debug.rs`, `gc/mark_scan_bench.rs` entered the corpus whole
/// as fail-open deletion-evidence channels — from inside such a file
/// the gate in the parent is invisible). The mod GRAPH, not the
/// filename, decides: every file's `mod x;` declarations are
/// syn-parsed (including declarations nested in inline mods, with
/// gating inherited from enclosing inline mods), resolved per Rust
/// 2018 module layout (`dir/x.rs` | `dir/x/mod.rs`, `#[path]`
/// honored), and a file is gated iff EVERY declaration edge reaching
/// it is gated (transitively). Undeclared files keep today's
/// behavior: scanned.
fn cfg_test_gated_files(src_dir: &Path) -> Result<std::collections::HashSet<std::path::PathBuf>> {
    use std::collections::{HashMap, HashSet};
    // child file -> [(edge_gated, declaring file)]
    let mut edges: HashMap<std::path::PathBuf, Vec<(bool, std::path::PathBuf)>> = HashMap::new();
    let mut all_files: Vec<std::path::PathBuf> = Vec::new();
    walk_rs(src_dir, &mut |path| {
        all_files.push(path.to_owned());
        Ok(())
    })?;
    for path in &all_files {
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let Ok(file) = syn::parse_file(&raw) else {
            continue; // unparseable files cannot contribute edges
        };
        // Effective child dir: mod.rs/lib.rs/main.rs resolve siblings
        // in their own dir; any other file resolves in dir/<stem>/.
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let dir = path.parent().unwrap_or(src_dir).to_owned();
        let eff_dir = if matches!(name, "mod.rs" | "lib.rs" | "main.rs") {
            dir.clone()
        } else {
            dir.join(path.file_stem().and_then(|s| s.to_str()).unwrap_or(""))
        };
        fn collect(
            items: &[syn::Item],
            gated_ancestor: bool,
            eff_dir: &Path,
            file_dir: &Path,
            declaring: &Path,
            edges: &mut std::collections::HashMap<
                std::path::PathBuf,
                Vec<(bool, std::path::PathBuf)>,
            >,
        ) {
            for item in items {
                let syn::Item::Mod(m) = item else { continue };
                let gated = gated_ancestor || is_cfg_test_attrs(&m.attrs);
                match &m.content {
                    Some((_, inner)) => {
                        // inline mod: nested declarations resolve under
                        // eff_dir/<mod name>/
                        collect(
                            inner,
                            gated,
                            &eff_dir.join(m.ident.to_string()),
                            file_dir,
                            declaring,
                            edges,
                        );
                    }
                    None => {
                        // `mod x;` — #[path] overrides, relative to the
                        // declaring file's directory.
                        let path_attr = m.attrs.iter().find_map(|a| {
                            if !a.path().is_ident("path") {
                                return None;
                            }
                            match &a.meta {
                                syn::Meta::NameValue(nv) => match &nv.value {
                                    syn::Expr::Lit(syn::ExprLit {
                                        lit: syn::Lit::Str(s),
                                        ..
                                    }) => Some(s.value()),
                                    _ => None,
                                },
                                _ => None,
                            }
                        });
                        let candidates: Vec<std::path::PathBuf> = if let Some(p) = path_attr {
                            vec![file_dir.join(p)]
                        } else {
                            let n = m.ident.to_string();
                            vec![
                                eff_dir.join(format!("{n}.rs")),
                                eff_dir.join(&n).join("mod.rs"),
                            ]
                        };
                        for c in candidates {
                            if c.exists() {
                                edges
                                    .entry(c)
                                    .or_default()
                                    .push((gated, declaring.to_owned()));
                                break;
                            }
                        }
                    }
                }
            }
        }
        collect(&file.items, false, &eff_dir, &dir, path, &mut edges);
    }
    // A file is gated iff it has >=1 edge and EVERY edge is gated OR
    // leads from a gated declaring file (transitive; cycles resolve
    // un-gated — conservative toward scanning).
    fn gated(
        f: &Path,
        edges: &std::collections::HashMap<std::path::PathBuf, Vec<(bool, std::path::PathBuf)>>,
        memo: &mut std::collections::HashMap<std::path::PathBuf, bool>,
        visiting: &mut HashSet<std::path::PathBuf>,
    ) -> bool {
        if let Some(&g) = memo.get(f) {
            return g;
        }
        if !visiting.insert(f.to_owned()) {
            return false;
        }
        let g = match edges.get(f) {
            None => false,
            Some(es) => {
                !es.is_empty()
                    && es
                        .iter()
                        .all(|(eg, parent)| *eg || gated(parent, edges, memo, visiting))
            }
        };
        visiting.remove(f);
        memo.insert(f.to_owned(), g);
        g
    }
    let mut memo = HashMap::new();
    let mut out = HashSet::new();
    for f in &all_files {
        if gated(f, &edges, &mut memo, &mut HashSet::new()) {
            out.insert(f.clone());
        }
    }
    Ok(out)
}

fn is_cfg_test_attrs(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<proc_macro2::TokenStream>()
                .map(cfg_pred_gates_test)
                .unwrap_or(false)
    })
}

/// Drop every `#[doc = …]` attribute group (recursively, so impl/mod
/// bodies are covered) — doc text is commentary, never production
/// deletion evidence.
fn strip_doc_attrs(ts: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    let tts: Vec<proc_macro2::TokenTree> = ts.into_iter().collect();
    let mut out: Vec<proc_macro2::TokenTree> = Vec::new();
    let mut i = 0;
    while i < tts.len() {
        if let proc_macro2::TokenTree::Punct(p) = &tts[i]
            && p.as_char() == '#'
        {
            // merged_bug_086 arm 3: inner attributes are `#![doc = …]`
            // — peek past the optional `!` so `//!` module prose is
            // stripped exactly like `///` item prose.
            let mut j = i + 1;
            if let Some(proc_macro2::TokenTree::Punct(q)) = tts.get(j)
                && q.as_char() == '!'
            {
                j += 1;
            }
            if let Some(proc_macro2::TokenTree::Group(g)) = tts.get(j)
                && g.delimiter() == proc_macro2::Delimiter::Bracket
                && matches!(
                    g.stream().into_iter().next(),
                    Some(proc_macro2::TokenTree::Ident(id)) if id == "doc"
                )
            {
                i = j + 1; // drop `#`, the optional `!`, and the [doc = …] group
                continue;
            }
        }
        out.push(match tts[i].clone() {
            proc_macro2::TokenTree::Group(g) => proc_macro2::TokenTree::Group(
                proc_macro2::Group::new(g.delimiter(), strip_doc_attrs(g.stream())),
            ),
            other => other,
        });
        i += 1;
    }
    out.into_iter().collect()
}

/// Evaluate a `cfg(...)` predicate for test-gating by its HEAD, not by
/// token mention (merged_bug_086 arm 1: blind recursion classified
/// `#[cfg(not(test))]` — production-ONLY code — as test-gated and
/// pruned the live reap at gc/collect.rs from the corpus). Elements at
/// one level are comma-separated; per element: a bare `test` ident
/// gates; `not(..)` NEVER gates (its body compiles in production);
/// `any(..)`/`all(..)` gate iff any inner element gates (recursing
/// with the same head rules — `all(not(test), x)` is kept);
/// `feature = "..."` and every other key never gate.
fn cfg_pred_gates_test(ts: proc_macro2::TokenStream) -> bool {
    let mut elems: Vec<Vec<proc_macro2::TokenTree>> = vec![Vec::new()];
    for tt in ts {
        if matches!(&tt, proc_macro2::TokenTree::Punct(p) if p.as_char() == ',') {
            elems.push(Vec::new());
        } else {
            elems.last_mut().expect("non-empty").push(tt);
        }
    }
    elems.into_iter().any(|e| {
        let mut it = e.into_iter();
        match it.next() {
            Some(proc_macro2::TokenTree::Ident(id)) if id == "test" => true,
            Some(proc_macro2::TokenTree::Ident(id)) if id == "not" => false,
            Some(proc_macro2::TokenTree::Ident(id)) if id == "any" || id == "all" => {
                match it.next() {
                    Some(proc_macro2::TokenTree::Group(g)) => cfg_pred_gates_test(g.stream()),
                    _ => false,
                }
            }
            _ => false,
        }
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

/// The migration corpus for KeepForever FK resolution: every
/// `rio-migrations/migrations/*.sql`, numeric order, as
/// `(basename, text)`.
fn migration_corpus(root: &Path) -> Result<Vec<(String, String)>> {
    let dir = root.join("rio-migrations").join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("non-UTF-8 migration filename: {}", f.display()))?
            .to_string();
        let text = std::fs::read_to_string(&f)?;
        out.push((name, text));
    }
    Ok(out)
}

/// Final FK state of constraints ON `table` (the child side — the
/// rows a parent-DELETE could take), folded over the migration corpus
/// in order: inline `REFERENCES … ON DELETE <action>` clauses in the
/// table's CREATE block (constraint name synthesized as the PG
/// default `<table>_<col>_fkey`) + `ALTER TABLE <table>
/// DROP/ADD CONSTRAINT`. Returns `constraint name → action text`
/// (uppercased; absent ON DELETE = "NO ACTION").
fn keep_forever_fk_state(
    table: &str,
    migrations: &[(String, String)],
) -> std::collections::BTreeMap<String, String> {
    let mut state: std::collections::BTreeMap<String, String> = Default::default();
    for (_name, text) in migrations {
        let upper = text.to_uppercase();
        // --- inline REFERENCES inside CREATE TABLE <table> ( … ) ---
        let create_pat = format!("CREATE TABLE {} (", table.to_uppercase());
        if let Some(start) = upper.find(&create_pat) {
            let body_start = start + create_pat.len();
            let bytes = upper.as_bytes();
            let mut depth = 1i32;
            let mut k = body_start;
            while k < upper.len() && depth > 0 {
                match bytes[k] as char {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                k += 1;
            }
            let block = &text[body_start..k.saturating_sub(1)];
            for line in block.lines() {
                let u = line.to_uppercase();
                if !u.contains("REFERENCES") {
                    continue;
                }
                let col = line
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_matches(',')
                    .to_lowercase();
                if col.is_empty() || col == "constraint" || col == "foreign" {
                    // explicit named constraints handled below via
                    // their CONSTRAINT name if present in-line
                    continue;
                }
                let action = if u.contains("ON DELETE CASCADE") {
                    "CASCADE"
                } else if u.contains("ON DELETE SET NULL") {
                    "SET NULL"
                } else if u.contains("ON DELETE RESTRICT") {
                    "RESTRICT"
                } else {
                    "NO ACTION"
                };
                state.insert(format!("{table}_{col}_fkey"), action.to_string());
            }
        }
        // --- ALTER TABLE <table> DROP/ADD CONSTRAINT ---
        let alter_pat = format!("ALTER TABLE {}", table.to_uppercase());
        let mut from = 0;
        while let Some(i) = upper[from..].find(&alter_pat) {
            let at = from + i;
            // statement = up to the next ';'
            let end = upper[at..].find(';').map(|e| at + e).unwrap_or(upper.len());
            let stmt_u = &upper[at..end];
            let stmt = &text[at..end];
            if let Some(d) = stmt_u.find("DROP CONSTRAINT") {
                let name = stmt[d + "DROP CONSTRAINT".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                state.remove(&name);
            }
            if let Some(a) = stmt_u.find("ADD CONSTRAINT") {
                let name = stmt[a + "ADD CONSTRAINT".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if stmt_u.contains("FOREIGN KEY") {
                    let action = if stmt_u.contains("ON DELETE CASCADE") {
                        "CASCADE"
                    } else if stmt_u.contains("ON DELETE SET NULL") {
                        "SET NULL"
                    } else if stmt_u.contains("ON DELETE RESTRICT") {
                        "RESTRICT"
                    } else {
                        "NO ACTION"
                    };
                    state.insert(name, action.to_string());
                }
            }
            from = end;
        }
    }
    state
}

/// bug_095: the KeepForever CHECKED NEGATIVE CLAIM. (a) No cascading
/// FK on the table survives the migration corpus — an `ON DELETE
/// CASCADE` reaching a KeepForever table is a live deletion vector no
/// registry prose can talk away (gc_holds shipped exactly this,
/// CI-green, with the live `delete_tenant` path silently erasing
/// active litigation-class holds). (b) The production `DELETE FROM`
/// census matches the declared deleter: `None` ⇒ zero hits;
/// `AdminRpc(symbols)` ⇒ every hit sits inside a listed fn's body
/// (brace-matched span, the `defines_fn` discipline).
fn check_keep_forever(
    table: &str,
    deleter: rio_migrations::retention::KeepForeverDeleter,
    corpus: &[(std::path::PathBuf, String)],
    migrations: &[(String, String)],
) -> Result<()> {
    use rio_migrations::retention::KeepForeverDeleter as D;
    // (a) the cascade-FK face — the schema layer.
    let fk_state = keep_forever_fk_state(table, migrations);
    for (name, action) in &fk_state {
        ensure!(
            action != "CASCADE",
            "KeepForever table carries a live deletion vector: FK `{name}` \
             is ON DELETE CASCADE in the final migration-corpus state — a \
             parent DELETE silently erases rows the registry declares \
             never-deleted (repair = a NEW migration re-declaring RESTRICT; \
             shipped migrations are frozen)"
        );
    }
    // (b) the workspace DELETE FROM face — the code layer.
    let mut hits: Vec<(std::path::PathBuf, usize)> = Vec::new();
    for (path, norm) in corpus {
        let mut from = 0;
        while let Some(i) = find_stmt(norm, "DELETE FROM", table, from) {
            hits.push((path.clone(), i));
            from = i + 1;
        }
    }
    match deleter {
        D::None => {
            ensure!(
                hits.is_empty(),
                "KeepForever(None) table has production DELETE FROM hit(s) \
                 in {:?} — either the rows are NOT keep-forever (fix the \
                 registry) or the delete is a doctrine violation",
                hits.iter()
                    .map(|(p, _)| p.display().to_string())
                    .collect::<Vec<_>>()
            );
        }
        D::AdminRpc(symbols) => {
            ensure!(
                !hits.is_empty(),
                "KeepForever(AdminRpc{symbols:?}) declares sanctioned \
                 delete fns but no production DELETE FROM {table} exists — \
                 stale registry row (reclassify as None)"
            );
            for (path, pos) in &hits {
                let norm = &corpus.iter().find(|(p, _)| p == path).unwrap().1;
                let inside = symbols.iter().any(|sym| {
                    fn_body_spans(norm, sym)
                        .iter()
                        .any(|(s, e)| pos >= s && pos < e)
                });
                ensure!(
                    inside,
                    "DELETE FROM {table} at {}:{pos} (byte offset) is OUTSIDE \
                     every sanctioned deleter fn {symbols:?} — an unsanctioned \
                     deletion vector on a KeepForever table",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// Byte offsets of `fn <symbol>` body spans (brace-matched) in `norm`.
fn fn_body_spans(norm: &str, symbol: &str) -> Vec<(usize, usize)> {
    let pat = format!("fn {symbol}");
    let bytes = norm.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(i) = norm[from..].find(&pat) {
        let at = from + i;
        let after = at + pat.len();
        // word boundary: next char is ws, '(' or '<'
        let ok = norm[after..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '(' || c == '<');
        if !ok {
            from = after;
            continue;
        }
        // body start: first '{' at paren-depth 0 after the signature
        let mut depth_paren = 0i32;
        let mut j = after;
        let mut body_start = None;
        while j < norm.len() {
            match bytes[j] as char {
                '(' => depth_paren += 1,
                ')' => depth_paren -= 1,
                ';' if depth_paren == 0 => break,
                '{' if depth_paren == 0 => {
                    body_start = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        if let Some(bs) = body_start {
            let mut depth = 0i32;
            let mut k = bs;
            while k < norm.len() {
                match bytes[k] as char {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            spans.push((bs, k.min(norm.len())));
            from = k.min(norm.len());
        } else {
            from = j.max(after) + 1;
        }
    }
    spans
}

/// Byte offset of the next `DELETE FROM <table>` statement hit at or
/// after `from` (word-bounded on the table name), or None.
fn find_stmt(norm: &str, verb: &str, table: &str, from: usize) -> Option<usize> {
    let pat = format!("{verb} {table}");
    let mut f = from;
    while let Some(i) = norm[f..].find(&pat) {
        let at = f + i;
        let after = at + pat.len();
        let boundary = norm[after..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if boundary {
            return Some(at);
        }
        f = after;
    }
    None
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

// ======================= dead-alphabet-letter ==========================

/// The dead-alphabet-letter jurisdiction: the crates whose `.rs`
/// sources the `xtask-lint` flake check stages (the
/// nix/misc-checks.nix fileset union) — the declared list and the
/// staging move together (the local `xtask lint` vs nix parity rule
/// documented at that fileset). Widening = add the crate here AND to
/// the fileset union. Every entry is existence-asserted (an absent
/// root is an error, never an empty scan).
const DEAD_LETTER_CRATES: &[&str] = &["rio-scheduler", "rio-store", "rio-controller"];

/// CHECK-alphabet letters intentionally shipped without a production
/// constructor. Each entry MUST carry a rationale naming the
/// constructor-to-be (the schema-liveness ALLOW_DEAD discipline); an
/// entry whose letter gains a constructor goes stale and must be
/// removed (the lint reds on unused allowances).
const ALLOW_UNCONSTRUCTED: &[(&str, &str, &str)] = &[];

/// Alphabets whose production constructor IS the macro-generated
/// `FromStr` over client-originated strings (the wire or a PG column
/// written from the wire): no system arm ever spells the variant, the
/// PARSE mints it. The exemption is enum-wide and carries the data
/// source as its rationale — a letter of a parse-constructed alphabet
/// can still die (no client ever sends it), but that is a traffic
/// fact, not a static one; the lint documents the tier instead of
/// pretending to decide it.
const PARSE_CONSTRUCTED: &[(&str, &str)] = &[(
    "PriorityClass",
    "client-originated: the gateway submits it as a wire string, merge stores it, \
     recovery re-parses it (actor/recovery.rs `.parse()`); Default covers the \
     fallback arm",
)];

/// One parsed `db_str_enum!` alphabet declaration.
struct AlphabetDecl {
    file: std::path::PathBuf,
    enum_name: String,
    /// (variant, PG literal) pairs in declaration order.
    variants: Vec<(String, String)>,
}

fn dead_alphabet_letter() -> Result<()> {
    dead_alphabet_letter_at(
        repo_root(),
        DEAD_LETTER_CRATES,
        ALLOW_UNCONSTRUCTED,
        PARSE_CONSTRUCTED,
    )
}

/// The testable core: scan `root`'s `crates` and red on (a) a
/// db-CHECK alphabet letter with zero expression-position
/// constructors, (b) a HELP-described label value with zero
/// same-crate literals outside its registration, (c) a stale
/// ALLOW_UNCONSTRUCTED row. Pattern positions never count: syn's
/// typed AST separates `Expr` from `Pat` structurally, and inside
/// macro token streams (opaque to the typed AST) an occurrence
/// followed by `=>` or flanked by `|` is a match-arm pattern while
/// `matches!` bodies are skipped wholesale (a pattern argument by
/// definition; the conservative direction). Aliased enum imports
/// (`use JobState as J`) earn no credit — the rename-tightness
/// refusal the standing censuses use.
fn dead_alphabet_letter_at(
    root: &Path,
    crates: &[&str],
    allow_unconstructed: &[(&str, &str, &str)],
    parse_constructed: &[(&str, &str)],
) -> Result<()> {
    let mut violations: Vec<String> = Vec::new();

    // ---- per-crate corpora (production-only ASTs) ----
    let mut corpora: Vec<(String, Vec<(std::path::PathBuf, syn::File)>)> = Vec::new();
    for krate in crates {
        let dir = root.join(krate).join("src");
        ensure!(
            dir.is_dir(),
            "dead-alphabet-letter: declared jurisdiction root missing: {} \
             (the lint never scans an empty default — fix the path or the staging)",
            dir.display()
        );
        let gated = cfg_test_gated_files(&dir)?;
        let mut files = Vec::new();
        walk_rs(&dir, &mut |path| {
            if path.components().any(|c| c.as_os_str() == "tests") || gated.contains(path) {
                return Ok(());
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                return Ok(());
            }
            let raw =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let mut file: syn::File =
                syn::parse_file(&raw).with_context(|| format!("syn parse {}", path.display()))?;
            prune_cfg_test_items(&mut file.items);
            {
                use syn::visit_mut::VisitMut;
                let mut pruner = CfgTestStmtPrune;
                for item in &mut file.items {
                    pruner.visit_item_mut(item);
                }
            }
            files.push((path.to_owned(), file));
            Ok(())
        })?;
        ensure!(
            !files.is_empty(),
            "dead-alphabet-letter: zero production files under {} — population floor",
            dir.display()
        );
        corpora.push(((*krate).to_string(), files));
    }

    // ---- half 1: the db-CHECK alphabets (db_str_enum! lives in
    //      rio-scheduler; the decl census floor pins the surface) ----
    let mut decls: Vec<AlphabetDecl> = Vec::new();
    let mut constructed: std::collections::BTreeSet<(String, String)> = Default::default();
    for (krate, files) in &corpora {
        if krate != "rio-scheduler" {
            continue;
        }
        for (path, file) in files {
            collect_db_str_enum_decls(path, file, &mut decls);
        }
        // The constructor census runs over the same crate (the
        // alphabets are crate-internal types).
        for (_path, file) in files {
            let mut v = ConstructorCensus {
                impl_self: Vec::new(),
                in_pattern: 0,
                hits: &mut constructed,
            };
            syn::visit::Visit::visit_file(&mut v, file);
        }
    }
    ensure!(
        decls.len() >= 6,
        "dead-alphabet-letter: only {} db_str_enum! declarations found (floor 6) — \
         the decl scan or the staging rotted",
        decls.len()
    );
    // SQL-literal production: a value the crate's own SQL text inlines
    // (`status = 'pending'`) is produced without any Rust constructor —
    // the single-quote form is the credit key (a bare "pending" string,
    // e.g. a sibling enum's display arm, earns nothing: that is exactly
    // the laundering surface the live_061 'obsolete' letter hid behind).
    let mut sql_quoted: std::collections::BTreeSet<String> = Default::default();
    for (krate, files) in &corpora {
        if krate != "rio-scheduler" {
            continue;
        }
        let mut lits: std::collections::BTreeSet<String> = Default::default();
        for (_path, file) in files {
            collect_literals_outside_describes(file, &mut lits);
        }
        for lit in lits {
            let bytes = lit.as_bytes();
            let mut i = 0;
            while let Some(open) = lit[i..].find('\'') {
                let s = i + open + 1;
                if let Some(close) = lit[s..].find('\'') {
                    let val = &lit[s..s + close];
                    if !val.is_empty()
                        && val
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        sql_quoted.insert(val.to_string());
                    }
                    i = s + close + 1;
                } else {
                    break;
                }
            }
            let _ = bytes;
        }
    }
    let mut allowance_used: std::collections::BTreeSet<(String, String)> = Default::default();
    let mut parse_constructed_used: std::collections::BTreeSet<String> = Default::default();
    for decl in &decls {
        if let Some((e, _why)) = PARSE_CONSTRUCTED.iter().find(|(e, _)| e == &decl.enum_name) {
            parse_constructed_used.insert((*e).to_string());
            continue;
        }
        for (variant, literal) in &decl.variants {
            let key = (decl.enum_name.clone(), variant.clone());
            if constructed.contains(&key) || sql_quoted.contains(literal) {
                continue;
            }
            if let Some((e, v, _why)) = allow_unconstructed
                .iter()
                .find(|(e, v, _)| e == &decl.enum_name && v == variant)
            {
                allowance_used.insert(((*e).to_string(), (*v).to_string()));
                continue;
            }
            violations.push(format!(
                "{}: CHECK letter `{}::{}` (= '{}') has no expression-position \
                 constructor in production source — the alphabet admits a value \
                 nothing can produce (the live_061 'obsolete' shape: decl + display \
                 arm only, zero writers). Construct it, or enroll it in \
                 ALLOW_UNCONSTRUCTED with the constructor-to-be named",
                decl.file.display(),
                decl.enum_name,
                variant,
                literal,
            ));
        }
    }
    for (e, _why) in parse_constructed {
        if !parse_constructed_used.contains(*e) {
            violations.push(format!(
                "PARSE_CONSTRUCTED row `{e}` names no known db_str_enum alphabet — \
                 remove or fix the row"
            ));
        }
    }
    for (e, v, _why) in allow_unconstructed {
        let key = ((*e).to_string(), (*v).to_string());
        if constructed.contains(&key) {
            violations.push(format!(
                "ALLOW_UNCONSTRUCTED row `{e}::{v}` is stale — the letter has a \
                 production constructor now; remove the allowance"
            ));
        } else if !allowance_used.contains(&key) {
            violations.push(format!(
                "ALLOW_UNCONSTRUCTED row `{e}::{v}` names no known alphabet letter — \
                 remove or fix the row"
            ));
        }
    }

    // ---- half 2: metric HELP vocabularies vs same-crate literals ----
    let mut describe_count = 0usize;
    for (krate, files) in &corpora {
        // (metric, label, value) -> declaring file
        let mut vocab: Vec<(String, String, String, std::path::PathBuf)> = Vec::new();
        let mut literals: std::collections::BTreeSet<String> = Default::default();
        for (path, file) in files {
            let mut helps: Vec<(String, String)> = Vec::new();
            collect_describe_helps(file, &mut helps);
            describe_count += helps.len();
            for (metric, help) in &helps {
                for (label, value) in parse_help_vocab(help) {
                    vocab.push((metric.clone(), label, value, path.clone()));
                }
            }
            collect_literals_outside_describes(file, &mut literals);
        }
        for (metric, label, value, path) in vocab {
            if !literals.contains(&value) {
                violations.push(format!(
                    "{}: HELP for `{metric}` describes {label}={value} but the value \
                     has no string literal anywhere in {krate}'s production source \
                     outside metric registrations — the vocabulary narrates a cell no \
                     emission site (or label-producer arm) can mint; fix the HELP or \
                     the emitter",
                    path.display(),
                ));
            }
        }
    }
    ensure!(
        describe_count >= 30,
        "dead-alphabet-letter: only {describe_count} describe_* registrations found \
         (floor 30) — the registration scan or the staging rotted"
    );

    ensure!(
        violations.is_empty(),
        "dead-alphabet-letter: {} violation(s) —\n  {}",
        violations.len(),
        violations.join("\n  ")
    );
    tracing::info!(
        alphabets = decls.len(),
        letters = decls.iter().map(|d| d.variants.len()).sum::<usize>(),
        describes = describe_count,
        "dead-alphabet-letter: every CHECK letter constructed (or enrolled), every \
         HELP vocabulary value minted"
    );
    Ok(())
}

/// Extract `db_str_enum! { ... enum Name { Variant = "lit", ... } }`
/// alphabets from a file's macro invocations (the macro_rules!
/// DEFINITION parses as path `macro_rules` and is skipped — only
/// invocations carry alphabets).
fn collect_db_str_enum_decls(path: &Path, file: &syn::File, out: &mut Vec<AlphabetDecl>) {
    fn from_tokens(path: &Path, tokens: proc_macro2::TokenStream, out: &mut Vec<AlphabetDecl>) {
        let toks: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
        // find `enum <Name> { ... }`
        for i in 0..toks.len() {
            let proc_macro2::TokenTree::Ident(kw) = &toks[i] else {
                continue;
            };
            if kw != "enum" {
                continue;
            }
            let Some(proc_macro2::TokenTree::Ident(name)) = toks.get(i + 1) else {
                continue;
            };
            let Some(proc_macro2::TokenTree::Group(body)) = toks.get(i + 2) else {
                continue;
            };
            let inner: Vec<proc_macro2::TokenTree> = body.stream().into_iter().collect();
            let mut variants = Vec::new();
            for j in 0..inner.len() {
                let proc_macro2::TokenTree::Ident(v) = &inner[j] else {
                    continue;
                };
                let vs = v.to_string();
                if !vs.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    continue;
                }
                let Some(proc_macro2::TokenTree::Punct(eq)) = inner.get(j + 1) else {
                    continue;
                };
                if eq.as_char() != '=' {
                    continue;
                }
                let Some(proc_macro2::TokenTree::Literal(lit)) = inner.get(j + 2) else {
                    continue;
                };
                let raw = lit.to_string();
                let Some(pg) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
                    continue;
                };
                variants.push((vs, pg.to_string()));
            }
            if !variants.is_empty() {
                out.push(AlphabetDecl {
                    file: path.to_owned(),
                    enum_name: name.to_string(),
                    variants,
                });
            }
        }
    }
    fn walk_items(path: &Path, items: &[syn::Item], out: &mut Vec<AlphabetDecl>) {
        for item in items {
            match item {
                syn::Item::Macro(m)
                    if m.mac
                        .path
                        .segments
                        .last()
                        .is_some_and(|s| s.ident == "db_str_enum") =>
                {
                    from_tokens(path, m.mac.tokens.clone(), out);
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content {
                        walk_items(path, inner, out);
                    }
                }
                _ => {}
            }
        }
    }
    walk_items(path, &file.items, out);
}

/// The expression-position constructor census. Typed-AST positions
/// are exact (an `ExprPath` is an expression; a `Pat` is not); macro
/// token streams get the documented token-tier classification.
struct ConstructorCensus<'a> {
    /// Enum-name stack of enclosing `impl <Enum>` blocks, for
    /// `Self::Variant` constructor credit.
    impl_self: Vec<String>,
    /// Pattern-nesting depth: syn 2 stores a unit-variant PATTERN as
    /// the same `ExprPath` node an expression uses (`Pat::Path`
    /// carries `ExprPath`), so without this guard a match arm's
    /// pattern would earn constructor credit — the exact laundering
    /// the lint exists to refuse.
    in_pattern: usize,
    hits: &'a mut std::collections::BTreeSet<(String, String)>,
}

impl ConstructorCensus<'_> {
    fn credit_path(&mut self, segs: &[String]) {
        let n = segs.len();
        if n < 2 {
            return;
        }
        let (owner, variant) = (&segs[n - 2], &segs[n - 1]);
        if !variant
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            return;
        }
        let owner = if owner == "Self" {
            match self.impl_self.last() {
                Some(t) => t.clone(),
                None => return,
            }
        } else {
            owner.clone()
        };
        if owner.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            self.hits.insert((owner, variant.clone()));
        }
    }

    fn scan_macro_tokens(&mut self, mac: &syn::Macro) {
        let last = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        // matches! bodies are pattern arguments by definition; the
        // alphabet decls and metric registrations are their own
        // scan surfaces, never constructor credit.
        if last == "matches" || last == "db_str_enum" || last.starts_with("describe_") {
            return;
        }
        let toks: Vec<proc_macro2::TokenTree> = mac.tokens.clone().into_iter().collect();
        self.scan_token_slice(&toks);
    }

    fn scan_token_slice(&mut self, toks: &[proc_macro2::TokenTree]) {
        use proc_macro2::TokenTree as Tt;
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tt::Group(g) => {
                    let inner: Vec<Tt> = g.stream().into_iter().collect();
                    self.scan_token_slice(&inner);
                    i += 1;
                }
                Tt::Ident(a) => {
                    // Ident :: Ident — the path tail.
                    let is_path = matches!(
                        (toks.get(i + 1), toks.get(i + 2)),
                        (Some(Tt::Punct(p1)), Some(Tt::Punct(p2)))
                            if p1.as_char() == ':' && p2.as_char() == ':'
                    );
                    if is_path && let Some(Tt::Ident(b)) = toks.get(i + 3) {
                        // pattern-position screens (the brief's two
                        // forms): `... => ` after, or `|` adjacency.
                        let followed_by_arrow = matches!(
                            (toks.get(i + 4), toks.get(i + 5)),
                            (Some(Tt::Punct(p1)), Some(Tt::Punct(p2)))
                                if p1.as_char() == '=' && p2.as_char() == '>'
                        );
                        let pipe_adjacent = matches!(toks.get(i + 4), Some(Tt::Punct(p)) if p.as_char() == '|')
                            || (i > 0
                                && matches!(&toks[i - 1], Tt::Punct(p) if p.as_char() == '|'));
                        if !followed_by_arrow && !pipe_adjacent {
                            self.credit_path(&[a.to_string(), b.to_string()]);
                        }
                        i += 4;
                    } else {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for ConstructorCensus<'_> {
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let pushed = if let syn::Type::Path(tp) = &*node.self_ty {
            tp.path.segments.last().map(|s| {
                self.impl_self.push(s.ident.to_string());
            })
        } else {
            None
        };
        syn::visit::visit_item_impl(self, node);
        if pushed.is_some() {
            self.impl_self.pop();
        }
    }

    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        self.in_pattern += 1;
        syn::visit::visit_pat(self, node);
        self.in_pattern -= 1;
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if self.in_pattern == 0 {
            let segs: Vec<String> = node
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            self.credit_path(&segs);
        }
        syn::visit::visit_expr_path(self, node);
    }

    // Struct-variant and call-variant constructors arrive as
    // ExprStruct / ExprCall(ExprPath) — ExprCall's callee is an
    // ExprPath (covered above); ExprStruct carries the path here.
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if self.in_pattern == 0 {
            let segs: Vec<String> = node
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            self.credit_path(&segs);
        }
        syn::visit::visit_expr_struct(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.scan_macro_tokens(node);
        syn::visit::visit_macro(self, node);
    }
}

/// Collect `(metric name, HELP text)` from describe_counter! /
/// describe_gauge! / describe_histogram! invocations (the first two
/// string literals of each call).
fn collect_describe_helps(file: &syn::File, out: &mut Vec<(String, String)>) {
    struct V<'a> {
        out: &'a mut Vec<(String, String)>,
    }
    impl<'ast> syn::visit::Visit<'ast> for V<'_> {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            let last = node
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            if last.starts_with("describe_") {
                let lits: Vec<String> = node
                    .tokens
                    .clone()
                    .into_iter()
                    .filter_map(|t| match t {
                        proc_macro2::TokenTree::Literal(l) => {
                            let s = l.to_string();
                            syn::parse_str::<syn::LitStr>(&s).ok().map(|ls| ls.value())
                        }
                        _ => None,
                    })
                    .collect();
                if lits.len() >= 2 {
                    self.out.push((lits[0].clone(), lits[1..].join("")));
                }
            }
            syn::visit::visit_macro(self, node);
        }
    }
    syn::visit::Visit::visit_file(&mut V { out }, file);
}

/// Parse the two HELP vocabulary grammars:
///   A: `labeled by <label> (<v1>|<v2>|...)` — the closed pipe-list;
///   B: `<label>=<value>:` — the per-value definition form (the
///      colon separates it from `{label=value}` cross-references,
///      which are excluded by the brace screen).
fn parse_help_vocab(help: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = help.as_bytes();
    // Form A.
    let mut idx = 0;
    while let Some(at) = help[idx..].find("labeled by ") {
        let start = idx + at + "labeled by ".len();
        let rest = &help[start..];
        let label: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let after = &rest[label.len()..];
        // only the immediately-following parenthesis group
        if let Some(open) = after.find('(')
            && after[..open].trim().is_empty()
            && let Some(close) = after[open..].find(')')
        {
            let body = &after[open + 1..open + close];
            if body.contains('|')
                && body
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
            {
                for v in body.split('|').filter(|v| !v.is_empty()) {
                    out.push((label.clone(), v.to_string()));
                }
            }
        }
        idx = start;
    }
    // Form B: label=value: (brace-screened).
    let re_like = help.char_indices().collect::<Vec<_>>();
    let mut i = 0;
    while i < re_like.len() {
        let (pos, c) = re_like[i];
        if c == '=' {
            // value forward to ':'
            let label_end = pos;
            let mut ls = label_end;
            while ls > 0 && {
                let pc = bytes[ls - 1] as char;
                pc.is_ascii_alphanumeric() || pc == '_'
            } {
                ls -= 1;
            }
            let label = &help[ls..label_end];
            let vstart = pos + 1;
            let mut ve = vstart;
            while ve < bytes.len() && {
                let vc = bytes[ve] as char;
                vc.is_ascii_alphanumeric() || vc == '_'
            } {
                ve += 1;
            }
            let value = &help[vstart..ve];
            let braced = ls > 0 && bytes[ls - 1] as char == '{';
            let colon = ve < bytes.len() && bytes[ve] as char == ':';
            if !label.is_empty() && !value.is_empty() && colon && !braced {
                out.push((label.to_string(), value.to_string()));
            }
        }
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}

/// Every string literal in the file OUTSIDE describe_* macro bodies —
/// the population a HELP vocabulary value must intersect (an emission
/// site's label value, or its label-producer's match arm).
fn collect_literals_outside_describes(
    file: &syn::File,
    out: &mut std::collections::BTreeSet<String>,
) {
    struct V<'a> {
        out: &'a mut std::collections::BTreeSet<String>,
    }
    impl<'ast> syn::visit::Visit<'ast> for V<'_> {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            let last = node
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            if last.starts_with("describe_") {
                return; // the registration is never its own emission
            }
            fn walk(ts: proc_macro2::TokenStream, out: &mut std::collections::BTreeSet<String>) {
                for t in ts {
                    match t {
                        proc_macro2::TokenTree::Literal(l) => {
                            if let Ok(ls) = syn::parse_str::<syn::LitStr>(&l.to_string()) {
                                out.insert(ls.value());
                            }
                        }
                        proc_macro2::TokenTree::Group(g) => walk(g.stream(), out),
                        _ => {}
                    }
                }
            }
            walk(node.tokens.clone(), self.out);
            syn::visit::visit_macro(self, node);
        }
        fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
            self.out.insert(node.value());
            syn::visit::visit_lit_str(self, node);
        }
    }
    syn::visit::Visit::visit_file(&mut V { out }, file);
}

// `lint.rs` is in `CORPUS_EXCLUDE`, so the synthetic table names below
// can't leak into the schema-liveness corpus and mask a real dead table.
#[cfg(test)]
mod tests {
    use super::*;

    /// merged_bug_086 arm 1: `#[cfg(not(test))]` is PRODUCTION-only
    /// code — the corpus must keep it (item- and statement-level),
    /// while `#[cfg(test)]` / `#[cfg(any(test, …))]` still prune and
    /// `#[cfg(all(not(test), …))]` is kept (production under the same
    /// builds the sweeper runs in).
    #[test]
    fn cfg_not_test_survives_the_prune() {
        let src = r##"
            #[cfg(not(test))]
            pub fn reap() { q("DELETE FROM widgets WHERE old"); }
            pub fn body() {
                #[cfg(not(test))]
                let _x = q("DELETE FROM gizmos WHERE old");
            }
            #[cfg(test)]
            mod t { pub fn f() { q("DELETE FROM hidden_t WHERE 1=1"); } }
            #[cfg(any(test, feature = "test-utils"))]
            pub fn g() { q("DELETE FROM hidden_any WHERE 1=1"); }
            #[cfg(all(not(test), feature = "server"))]
            pub fn h() { q("DELETE FROM kept_all WHERE 1=1"); }
        "##;
        let norm = corpus_text(std::path::Path::new("x.rs"), src)
            .unwrap()
            .unwrap();
        assert!(contains_stmt(&norm, "DELETE FROM", "widgets"), "{norm}");
        assert!(contains_stmt(&norm, "DELETE FROM", "gizmos"), "{norm}");
        assert!(contains_stmt(&norm, "DELETE FROM", "kept_all"), "{norm}");
        assert!(!contains_stmt(&norm, "DELETE FROM", "hidden_t"), "{norm}");
        assert!(!contains_stmt(&norm, "DELETE FROM", "hidden_any"), "{norm}");
    }

    /// merged_bug_086 arm 3: inner `#![doc]` (`//!` prose) is
    /// commentary, never production deletion evidence.
    #[test]
    fn inner_doc_attrs_are_stripped() {
        let src = "pub mod m { //! DELETE FROM doc_table\n pub fn f() {} }";
        let norm = corpus_text(std::path::Path::new("x.rs"), src)
            .unwrap()
            .unwrap();
        assert!(!contains_stmt(&norm, "DELETE FROM", "doc_table"), "{norm}");
    }

    /// merged_bug_086 arm 2: a file included via a `#[cfg(test)] mod`
    /// declaration (any gating shape, transitively) is test code even
    /// though nothing inside the file says so — the mod graph, not the
    /// filename, decides.
    #[test]
    fn parent_gated_mod_files_are_excluded() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path();
        std::fs::write(
            src.join("lib.rs"),
            "#[cfg(test)]\nmod helpers;\n#[cfg(any(test, feature = \"test-utils\"))]\npub mod fixtures;\nmod live;\n",
        )
        .unwrap();
        std::fs::write(
            src.join("helpers.rs"),
            "pub fn f() { q(\"DELETE FROM h1\"); }",
        )
        .unwrap();
        std::fs::write(
            src.join("fixtures.rs"),
            "pub fn f() { q(\"DELETE FROM h2\"); }",
        )
        .unwrap();
        std::fs::create_dir(src.join("live")).unwrap();
        std::fs::write(
            src.join("live").join("mod.rs"),
            "#[cfg(test)]\nmod bench;\nmod real;\n",
        )
        .unwrap();
        std::fs::write(src.join("live").join("bench.rs"), "pub fn f() {}").unwrap();
        std::fs::write(src.join("live").join("real.rs"), "pub fn f() {}").unwrap();
        let gated = cfg_test_gated_files(src).unwrap();
        assert!(gated.contains(&src.join("helpers.rs")), "{gated:?}");
        assert!(gated.contains(&src.join("fixtures.rs")), "{gated:?}");
        assert!(
            gated.contains(&src.join("live").join("bench.rs")),
            "{gated:?}"
        );
        assert!(
            !gated.contains(&src.join("live").join("real.rs")),
            "{gated:?}"
        );
        assert!(
            !gated.contains(&src.join("live").join("mod.rs")),
            "{gated:?}"
        );
    }

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

    /// W10-H (bug_095, R22′ planted red at the OUTERMOST scan layer):
    /// a fixture migration declaring a KeepForever table WITH a
    /// cascade FK goes RED — the exact evasion axis the prior
    /// `Ok(())` exemption shipped (gc_holds + 103's inline CASCADE,
    /// CI-green for a full wave). The raw .sql text enters the scan;
    /// no post-extraction fixture.
    #[test]
    fn keep_forever_flags_planted_cascade_fixture() {
        use rio_migrations::retention::KeepForeverDeleter;
        let planted = vec![(
            "strawman_cascade.sql".to_string(),
            "CREATE TABLE evidence_rows (\n\
                 id UUID PRIMARY KEY,\n\
                 tenant_id UUID REFERENCES tenants (tenant_id) ON DELETE CASCADE\n\
             );\n"
                .to_string(),
        )];
        let err = check_keep_forever("evidence_rows", KeepForeverDeleter::None, &[], &planted)
            .expect_err("a KeepForever table with a cascade FK MUST go red");
        assert!(
            err.to_string().contains("live deletion vector"),
            "the red names the violation, got: {err:#}"
        );
    }

    /// The 103→104 supersession shape resolves: DROP + ADD CONSTRAINT
    /// RESTRICT in a LATER migration clears the inline CASCADE (the
    /// frozen-migration repair form) — and the lint reads the FINAL
    /// corpus state, not any single file.
    #[test]
    fn keep_forever_accepts_restrict_supersession() {
        use rio_migrations::retention::KeepForeverDeleter;
        let corpus = vec![
            (
                "a_strawman_create.sql".to_string(),
                "CREATE TABLE evidence_rows (\n\
                     tenant_id UUID REFERENCES tenants (tenant_id) ON DELETE CASCADE\n\
                 );\n"
                    .to_string(),
            ),
            (
                "b_strawman_repair.sql".to_string(),
                "ALTER TABLE evidence_rows\n\
                     DROP CONSTRAINT evidence_rows_tenant_id_fkey;\n\
                 ALTER TABLE evidence_rows\n\
                     ADD CONSTRAINT evidence_rows_tenant_id_fkey\n\
                     FOREIGN KEY (tenant_id) REFERENCES tenants (tenant_id)\n\
                     ON DELETE RESTRICT;\n"
                    .to_string(),
            ),
        ];
        check_keep_forever("evidence_rows", KeepForeverDeleter::None, &[], &corpus)
            .expect("RESTRICT supersession is the lawful repair form");
    }

    /// The DELETE FROM census arms: None refuses a production hit;
    /// AdminRpc accepts a hit INSIDE the sanctioned fn body and
    /// refuses one outside it (brace-matched spans, not file-level).
    #[test]
    fn keep_forever_delete_census_polarity() {
        use rio_migrations::retention::KeepForeverDeleter;
        let inside = "fn delete_thing(pool: &PgPool) {\n    sqlx::query(\"DELETE FROM evidence_rows WHERE id = $1\");\n}\n";
        let outside = "fn delete_thing(pool: &PgPool) {}\nfn rogue(pool: &PgPool) {\n    sqlx::query(\"DELETE FROM evidence_rows WHERE id = $1\");\n}\n";
        let corpus_inside = vec![(std::path::PathBuf::from("a.rs"), inside.to_string())];
        let corpus_outside = vec![(std::path::PathBuf::from("b.rs"), outside.to_string())];

        check_keep_forever(
            "evidence_rows",
            KeepForeverDeleter::AdminRpc(&["delete_thing"]),
            &corpus_inside,
            &[],
        )
        .expect("a hit inside the sanctioned fn is lawful");

        let err = check_keep_forever(
            "evidence_rows",
            KeepForeverDeleter::AdminRpc(&["delete_thing"]),
            &corpus_outside,
            &[],
        )
        .expect_err("a hit OUTSIDE the sanctioned fn must go red");
        assert!(err.to_string().contains("OUTSIDE"), "got: {err:#}");

        let err = check_keep_forever(
            "evidence_rows",
            KeepForeverDeleter::None,
            &corpus_inside,
            &[],
        )
        .expect_err("None with any production hit must go red");
        assert!(err.to_string().contains("production DELETE FROM"));
    }

    // ---- dead-alphabet-letter planted reds -------------------------------

    /// Fixture crate skeleton for [`dead_alphabet_letter_at`]: the
    /// floors demand 6 alphabets and 30 registrations, so the builder
    /// plants a filler population and each case adds its specimen on
    /// top. The crate must be named `rio-scheduler` (the db half's
    /// declared home).
    fn dead_letter_fixture(extra: &str) -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let src_dir = td.path().join("rio-scheduler/src");
        fs::create_dir_all(&src_dir).unwrap();
        let mut filler = String::from(
            "macro_rules! db_str_enum { ($($t:tt)*) => {} }\n\
             macro_rules! describe_counter { ($($t:tt)*) => {} }\n",
        );
        for i in 0..6 {
            filler.push_str(&format!(
                "db_str_enum! {{ pub enum Filler{i} {{ AlwaysOn = \"always_on_{i}\" }} }}\n\
                 pub fn mint_filler_{i}() {{ let _ = Filler{i}::AlwaysOn; }}\n"
            ));
        }
        for i in 0..30 {
            filler.push_str(&format!(
                "pub fn reg_{i}() {{ describe_counter!(\"rio_x_{i}_total\", \"plain help\"); }}\n"
            ));
        }
        filler.push_str(extra);
        fs::write(src_dir.join("lib.rs"), filler).unwrap();
        td
    }

    /// The live_061 exemplar shape, synthetic: a CHECK letter with
    /// exactly the declaration and a display match arm — the lint must
    /// red (pattern positions never count), and the sibling letter
    /// constructed in expression position must not mask it.
    #[test]
    fn dead_letter_reds_on_decl_plus_display_arm_only() {
        let td = dead_letter_fixture(
            "db_str_enum! { pub enum SynthState { Live = \"live\", Obsolete = \"obsolete\" } }\n\
             pub fn writer() -> &'static str {\n\
                 let s = SynthState::Live;\n\
                 match s { SynthState::Live => \"live\", SynthState::Obsolete => \"obsolete\" }\n\
             }\n",
        );
        let err = dead_alphabet_letter_at(td.path(), &["rio-scheduler"], &[], &[])
            .expect_err("the display-arm-only letter must red");
        let msg = format!("{err:#}");
        assert!(msg.contains("SynthState::Obsolete"), "got: {msg}");
        assert!(
            !msg.contains("SynthState::Live"),
            "the constructed sibling is green: {msg}"
        );
    }

    /// Every pattern position the brief names earns NOTHING: match
    /// arms, alternations, `matches!` payloads, and let-patterns. Only
    /// the expression-position constructor flips the letter green.
    #[test]
    fn dead_letter_pattern_positions_never_count() {
        let red = dead_letter_fixture(
            "db_str_enum! { pub enum P { A = \"a\" } }\n\
             pub fn consume(p: P) -> bool {\n\
                 if let P::A = p { return true; }\n\
                 matches!(p, P::A) || match p { P::A => true }\n\
             }\n",
        );
        let err = dead_alphabet_letter_at(red.path(), &["rio-scheduler"], &[], &[])
            .expect_err("pattern-only occurrences must red");
        assert!(format!("{err:#}").contains("P::A"));

        let green = dead_letter_fixture(
            "db_str_enum! { pub enum P { A = \"a\" } }\n\
             pub fn produce() -> P { P::A }\n",
        );
        dead_alphabet_letter_at(green.path(), &["rio-scheduler"], &[], &[])
            .expect("the expression-position constructor is the credit");
    }

    /// SQL-text production credits through the single-quote form only:
    /// an inline `'pending'` in a query string mints the value; a BARE
    /// "pending" string (a sibling display arm — the laundering
    /// surface the live_061 'obsolete' letter hid behind) does not.
    #[test]
    fn dead_letter_sql_quote_credits_bare_string_does_not() {
        let green = dead_letter_fixture(
            "db_str_enum! { pub enum S { Pending = \"pending\" } }\n\
             pub const SQL: &str = \"INSERT INTO t (s) VALUES ('pending')\";\n",
        );
        dead_alphabet_letter_at(green.path(), &["rio-scheduler"], &[], &[])
            .expect("the SQL inline literal is a production site");

        let red = dead_letter_fixture(
            "db_str_enum! { pub enum S { Pending = \"pending\" } }\n\
             pub fn label() -> &'static str { \"pending\" }\n",
        );
        let err = dead_alphabet_letter_at(red.path(), &["rio-scheduler"], &[], &[])
            .expect_err("a bare string is never a constructor");
        assert!(format!("{err:#}").contains("S::Pending"));
    }

    /// The metrics half: a HELP-described label value with no
    /// same-crate literal outside registrations reds (both vocabulary
    /// grammars), an emission-site literal greens it, and the
    /// `{label=value}` cross-reference form is screened out.
    #[test]
    fn dead_letter_metric_vocabulary_reds_and_greens() {
        let red = dead_letter_fixture(
            "pub fn reg_v() { describe_counter!(\"rio_v_total\", \
             \"Verdicts, labeled by verdict (kept|dropped). verdict=kept: retained.\"); }\n\
             pub fn emit() { let _ = \"kept\"; }\n",
        );
        let err = dead_alphabet_letter_at(red.path(), &["rio-scheduler"], &[], &[])
            .expect_err("the dropped cell has no literal");
        let msg = format!("{err:#}");
        assert!(msg.contains("verdict=dropped"), "got: {msg}");
        assert!(!msg.contains("verdict=kept"), "kept is minted: {msg}");

        let green = dead_letter_fixture(
            "pub fn reg_v() { describe_counter!(\"rio_v_total\", \
             \"Verdicts, labeled by verdict (kept|dropped); pairs with \
             sibling_total{verdict=ghost}.\"); }\n\
             pub fn emit() { let _ = (\"kept\", \"dropped\"); }\n",
        );
        dead_alphabet_letter_at(green.path(), &["rio-scheduler"], &[], &[])
            .expect("both cells minted; the braced cross-reference is screened");
    }

    /// cfg(test) constructors are pruned from the corpus: a letter
    /// whose only constructor is test code is still dead in
    /// production (exactly how 'obsolete' stayed invisible — its
    /// round-trip tests exercised the letter while no production arm
    /// ever minted it).
    #[test]
    fn dead_letter_test_constructors_earn_nothing() {
        let td = dead_letter_fixture(
            "db_str_enum! { pub enum T { OnlyTests = \"only_tests\" } }\n\
             #[cfg(test)]\nmod tests { use super::*;\n\
                 pub fn mk() -> T { T::OnlyTests } }\n",
        );
        let err = dead_alphabet_letter_at(td.path(), &["rio-scheduler"], &[], &[])
            .expect_err("test-only constructors are not production writers");
        assert!(format!("{err:#}").contains("T::OnlyTests"));
    }

    /// Allowlist hygiene: a stale ALLOW_UNCONSTRUCTED row (the letter
    /// gained a constructor) and a PARSE_CONSTRUCTED row naming no
    /// declared alphabet both red — allowances may only shrink-track
    /// reality, never outlive it.
    #[test]
    fn dead_letter_allowlist_hygiene_reds() {
        let td = dead_letter_fixture(
            "db_str_enum! { pub enum H { Minted = \"minted\" } }\n\
             pub fn produce() -> H { H::Minted }\n",
        );
        let err = dead_alphabet_letter_at(
            td.path(),
            &["rio-scheduler"],
            &[("H", "Minted", "planted stale allowance")],
            &[("GhostEnum", "planted ghost parse row")],
        )
        .expect_err("both hygiene arms must red");
        let msg = format!("{err:#}");
        assert!(msg.contains("`H::Minted` is stale"), "got: {msg}");
        assert!(msg.contains("`GhostEnum` names no known"), "got: {msg}");
    }
}
