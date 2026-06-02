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
    /// Every call site of a `Builds` terminal-inclusive accessor
    /// (`*_including_terminal*`, rio-scheduler/src/state/build.rs)
    /// carries an adjacent `// Bookkeeping lookup: <reason>`
    /// justification, and every such marker annotates a real call
    /// site. Catches a policy site silently misclassified onto the
    /// terminal-inclusive side — the choice compiles either way, and
    /// a misclassified site lets a finished build keep steering
    /// scheduling until the delayed cleanup.
    BookkeepingMarker,
    /// Every xtask flow that creates a rio-replay engine Job
    /// (`replay::jobs::{create_job,try_create_job}` call sites) also
    /// runs the CiliumNetworkPolicy admission read-back
    /// (`preflight::verify_cnp_admissions`), or carries an explicit
    /// exemption with a reason. Catches a Job-creating command shipped
    /// without the check — Cilium silently DROPS an unadmitted
    /// engine's scheduler/store gRPC, hanging the run instead of
    /// failing it.
    ReplayCnpPreflight,
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
            Lint::BookkeepingMarker,
            Lint::ReplayCnpPreflight,
        ]
    }
}

/// Run one lint.
pub fn run(lint: &Lint) -> Result<()> {
    match lint {
        Lint::SchemaLiveness => schema_liveness(),
        Lint::HelmSla => helm_sla(),
        Lint::SeccompAllowlist => seccomp_allowlist(),
        Lint::BookkeepingMarker => bookkeeping_marker(),
        Lint::ReplayCnpPreflight => replay_cnp_preflight(),
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

/// The justification-comment token required at every terminal-inclusive
/// builds-map call site (and forbidden from going stale — see
/// [`bookkeeping_marker`]).
const BOOKKEEPING_MARKER: &str = "Bookkeeping lookup:";

/// Justification-marker guard for terminal-inclusive builds-map reads.
///
/// `Builds` (rio-scheduler/src/state/build.rs) splits build lookups by
/// liveness: `get()` returns LIVE builds only — the policy default —
/// while the `*_including_terminal*` accessors expose lingering
/// terminal entries for bookkeeping (cleanup, transition validation,
/// count updates, terminal-event re-send, tenant attribution, sweeps
/// that filter on `state()` explicitly). Choosing the terminal-
/// inclusive side is a per-site classification that compiles silently
/// either way, and a policy site misclassified onto it keeps a
/// finished build steering scheduling for up to the cleanup delay
/// (or until restart if the cleanup command is dropped). The dispatch
/// build-options fold shipped exactly that misclassification: the
/// liveness-split conversion enumerated the policy consumers from
/// recall, missed the fifth one, and nothing made the unjustified
/// terminal-inclusive read visible.
///
/// This lint turns the convention into a gate, in both directions:
///
/// - every `*_including_terminal*` call site under rio-scheduler/src
///   must carry a `// Bookkeeping lookup: <reason>` comment adjacent
///   to the statement containing the call (same line, or above it
///   with only comment/attribute/statement-continuation lines in
///   between — a `;`, `{`, or `}` boundary cuts the blessing, so one
///   marker cannot vouch for a neighboring statement);
/// - every marker must annotate such a call site, so a site later
///   flipped to the live-only accessor cannot leave a stale
///   justification behind.
///
/// The accessor-name set is derived from the wrapper definition at
/// scan time (any `fn *_including_terminal*` in state/build.rs), so a
/// new family member joins the gate without editing this lint; floor
/// guards on both the accessor count and the call-site count keep the
/// scan from passing vacuously if the wrapper moves or the call-site
/// shape changes.
fn bookkeeping_marker() -> Result<()> {
    let root = repo_root();
    let def_rel = "rio-scheduler/src/state/build.rs";
    let def_path = root.join(def_rel);
    let def_src =
        fs::read_to_string(&def_path).with_context(|| format!("reading {}", def_path.display()))?;
    let accessors = extract_terminal_accessors(&def_src);
    // Floor guard: the wrapper defines 7 family members today. An
    // empty/shrunken set means the wrapper moved or the family was
    // renamed — fail loud instead of scanning for nothing.
    ensure!(
        accessors.len() >= 4,
        "only {} `fn *_including_terminal*` accessor(s) found in {def_rel} — \
         the Builds wrapper moved or the family was renamed; update \
         `bookkeeping_marker` in xtask/src/lint.rs",
        accessors.len(),
    );

    let call_re = call_site_regex(&accessors);
    let scan_root = root.join("rio-scheduler/src");
    ensure!(
        scan_root.is_dir(),
        "bookkeeping-marker scan root {} not found",
        scan_root.display()
    );
    let mut sites = 0usize;
    let mut violations: Vec<String> = Vec::new();
    walk_rs(&scan_root, &mut |p| {
        let src = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
        sites += check_bookkeeping_markers(&src, &rel, &call_re, &mut violations);
        Ok(())
    })?;
    // Floor guard: ~29 call sites today. Near-zero means the call-site
    // regex or the scan root regressed, not that the code went clean.
    ensure!(
        sites >= 10,
        "bookkeeping-marker scan found only {sites} `*_including_terminal*` \
         call site(s) under rio-scheduler/src — suspiciously few; the \
         call-site detection or scan root has regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} bookkeeping-marker violation(s):\n    {}\n  every \
             `*_including_terminal*` call site needs an adjacent \
             `// {BOOKKEEPING_MARKER} <why terminal entries are wanted>` comment \
             (or the live-only accessor, if the read feeds a policy decision), \
             and every `{BOOKKEEPING_MARKER}` marker must annotate such a call",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(
        accessors = accessors.len(),
        call_sites = sites,
        "bookkeeping-marker ok"
    );
    Ok(())
}

/// Accessor-name set for [`bookkeeping_marker`], derived from the
/// `Builds` wrapper source: every `fn` whose name contains
/// `_including_terminal`. Doc-comment cross-references don't match
/// (no `fn` keyword); `BTreeSet` for deterministic regex alternation.
fn extract_terminal_accessors(src: &str) -> BTreeSet<String> {
    let re = regex::Regex::new(r"\bfn\s+(\w*_including_terminal\w*)\s*\(").unwrap();
    re.captures_iter(src).map(|c| c[1].to_owned()).collect()
}

/// Method-call regex for the accessor set: `.name(` with optional
/// whitespace. rustfmt never splits between `.` and the method name,
/// so a per-line match is reliable.
fn call_site_regex(accessors: &BTreeSet<String>) -> regex::Regex {
    let alt: Vec<String> = accessors.iter().map(|a| regex::escape(a)).collect();
    regex::Regex::new(&format!(r"\.\s*(?:{})\s*\(", alt.join("|"))).unwrap()
}

/// Scan one file for [`bookkeeping_marker`]. Returns the number of
/// call sites found; pushes a violation for every unjustified call
/// site and every stale marker.
fn check_bookkeeping_markers(
    src: &str,
    rel: &str,
    call_re: &regex::Regex,
    violations: &mut Vec<String>,
) -> usize {
    let lines: Vec<&str> = src.lines().collect();
    let mut sites = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("//") && call_re.is_match(strip_line_comment(line)) {
            sites += 1;
            if !marker_blesses_call(&lines, i) {
                violations.push(format!(
                    "{rel}:{}: `*_including_terminal*` call without an adjacent \
                     `{BOOKKEEPING_MARKER}` justification",
                    i + 1,
                ));
            }
        }
        // Only plain `//` comments are markers — whole-line or trailing
        // a code/attribute line. `///`/`//!` doc text mentioning the
        // convention is prose: it documents items, it cannot bless a
        // statement, and it must not be flagged stale. Every shape the
        // blessing walk honors is stale-checked here, so a site flipped
        // to the live-only accessor cannot keep a leftover
        // justification in any shape.
        if (is_marker_comment(trimmed) || has_trailing_marker(line))
            && !marker_annotates_call(&lines, i, call_re)
        {
            violations.push(format!(
                "{rel}:{}: stale `{BOOKKEEPING_MARKER}` marker — no \
                 `*_including_terminal*` call in the statement it annotates",
                i + 1,
            ));
        }
    }
    sites
}

/// Is this trimmed line a plain `//` comment carrying the marker?
/// Doc comments (`///`, `//!`) are prose about the convention, not
/// per-site justifications.
fn is_marker_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        && !trimmed.starts_with("///")
        && !trimmed.starts_with("//!")
        && trimmed.contains(BOOKKEEPING_MARKER)
}

/// Does this code or attribute line carry the marker in a TRAILING `//`
/// comment? Whole-line comments (including doc comments) are
/// [`is_marker_comment`]'s domain, not this one's; a marker token before
/// the `//` (e.g. inside a string literal) is code, not a justification.
/// Naive about `//` inside string literals, like [`strip_line_comment`].
fn has_trailing_marker(line: &str) -> bool {
    !line.trim_start().starts_with("//")
        && line
            .split_once("//")
            .is_some_and(|(_, comment)| comment.contains(BOOKKEEPING_MARKER))
}

/// Code part of a line: everything before a `//` comment. Naive about
/// `//` inside string literals — fine for a lint over call sites that
/// never embed one.
fn strip_line_comment(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

/// Upward adjacency: does a `Bookkeeping lookup:` marker bless the
/// call at `call_idx`? The marker may trail the call line itself, or
/// sit above it separated only by comment lines, attributes, and
/// statement-continuation lines (trailing any of those intervening
/// lines, or as a whole-line comment). Any line with a `;`, `{`, or
/// `}` in its code part bounds the statement — a marker beyond it,
/// including one trailing the bounding line itself, belongs to a
/// different statement and does NOT bless this call. The boundary is
/// therefore checked on the comment-stripped code part BEFORE a
/// trailing marker is honored; the reverse order would let one
/// statement's justification bless its neighbor.
fn marker_blesses_call(lines: &[&str], call_idx: usize) -> bool {
    if lines[call_idx].contains(BOOKKEEPING_MARKER) {
        return true;
    }
    let mut budget = 30usize;
    for line in lines[..call_idx].iter().rev() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            if is_marker_comment(trimmed) {
                return true;
            }
            continue;
        }
        if trimmed.starts_with("#[") {
            // Attributes belong to the statement below them, so a
            // marker trailing one blesses that statement (and never
            // bounds it — attributes carry no statement terminator).
            if has_trailing_marker(line) {
                return true;
            }
            continue;
        }
        let code = strip_line_comment(line);
        if code.contains(';') || code.contains('{') || code.contains('}') {
            return false;
        }
        if has_trailing_marker(line) {
            // Trailing marker on a continuation line of this statement.
            return true;
        }
    }
    false
}

/// Downward adjacency for the stale-marker check: does the marker at
/// `marker_idx` annotate a terminal-inclusive call? Mirror of
/// [`marker_blesses_call`]: the call must appear on the marker's own
/// line or in the first statement below it (comment/attribute lines
/// skipped; the statement ends at the first `;`/`{`/`}` code line,
/// which is itself still checked — `for … in x.iter_including_terminal() {`
/// carries call and boundary on one line). A TRAILING marker whose own
/// code part bounds the statement annotates that statement only —
/// nothing below it can keep it fresh, exactly as nothing below it can
/// be blessed by it. (Whole-line comment markers have no code part, so
/// the own-line boundary check never fires for them.)
fn marker_annotates_call(lines: &[&str], marker_idx: usize, call_re: &regex::Regex) -> bool {
    let own_code = strip_line_comment(lines[marker_idx]);
    if call_re.is_match(own_code) {
        return true;
    }
    // Attribute lines never bound the statement (mirrors the upward
    // walk, where they are skipped before the boundary check), even when
    // their arguments carry braces in strings.
    if !lines[marker_idx].trim_start().starts_with("#[")
        && (own_code.contains(';') || own_code.contains('{') || own_code.contains('}'))
    {
        return false;
    }
    let mut budget = 30usize;
    for line in lines.iter().skip(marker_idx + 1) {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("#[") {
            continue;
        }
        let code = strip_line_comment(line);
        if call_re.is_match(code) {
            return true;
        }
        if code.contains(';') || code.contains('{') || code.contains('}') {
            return false;
        }
    }
    false
}

/// CNP-admission pre-flight guard for rio-replay engine-Job creation.
///
/// The chart's CiliumNetworkPolicies admit the campaign engine's
/// scheduler/store gRPC by namespace + pod label, and xtask creates
/// every engine Job through exactly two helpers —
/// `replay::jobs::{create_job,try_create_job}`, both hard-pinned to
/// that namespace — so the helpers' call sites ARE the complete set of
/// engine-Job creations. The admissions themselves are deployed chart
/// state that can drift after setup verified them (a raw-helm
/// `replay.namespace` override, an out-of-band CNP edit, a divergent
/// older deployment), and an unadmitted engine does not fail: Cilium
/// silently drops its gRPC and the run hangs. Whether a Job-creating
/// flow re-reads the admissions first
/// (`preflight::verify_cnp_admissions`) is a per-flow classification
/// that compiles either way — launch's pre-flight gained the read-back
/// while repro's create path shipped without it, because repro's
/// skip-the-pre-flight rationale predated the check.
///
/// This lint turns the classification into a gate, in both directions:
///
/// - every file with a creator call site must also call
///   `verify_cnp_admissions` in code (file scope, not line order:
///   launch reaches it through a pre-flight helper defined after its
///   create call), or be listed in `CNP_EXEMPT` with a reason;
/// - every `CNP_EXEMPT` entry must still name a file that creates
///   engine Jobs and still lacks the read-back — an entry gone stale
///   either way fails, so the exemption list cannot accrete.
///
/// The creator-fn set is derived from the jobs.rs definitions at scan
/// time, so a renamed or added creator joins the gate without editing
/// this lint; floor guards on the creator count and the call-site
/// count keep the scan from passing vacuously.
fn replay_cnp_preflight() -> Result<()> {
    // Files allowed to create engine Jobs WITHOUT the CNP read-back.
    // Each entry MUST carry a rationale naming why the Job never needs
    // the admissions.
    const CNP_EXEMPT: &[(&str, &str)] = &[(
        "xtask/src/replay/eval.rs",
        "recorder Job: the eval engine talks only to Hydra, the nixpkgs tarball host, \
         cache.nixos.org, and S3 — it never dials the scheduler/store gRPC ports the \
         CNP admissions cover",
    )];

    let root = repo_root();
    let def_rel = "xtask/src/replay/jobs.rs";
    let def_path = root.join(def_rel);
    let def_src =
        fs::read_to_string(&def_path).with_context(|| format!("reading {}", def_path.display()))?;
    let creators = extract_job_creators(&def_src);
    // Floor guard: the helpers are `create_job` + `try_create_job`
    // today. A shrunken set means they moved or were renamed — fail
    // loud instead of scanning for nothing.
    ensure!(
        creators.len() >= 2,
        "only {} `fn *create_job*` helper(s) found in {def_rel} — the engine-Job \
         creators moved or were renamed; update `replay_cnp_preflight` in \
         xtask/src/lint.rs",
        creators.len(),
    );
    let call_re = creator_call_regex(&creators);
    let verify_re = regex::Regex::new(r"\bverify_cnp_admissions\s*\(").unwrap();

    let scan_root = root.join("xtask/src");
    ensure!(
        scan_root.is_dir(),
        "replay-cnp-preflight scan root {} not found",
        scan_root.display()
    );
    let mut sites = 0usize;
    let mut violations: Vec<String> = Vec::new();
    let mut exempt_seen: BTreeSet<&str> = BTreeSet::new();
    walk_rs(&scan_root, &mut |p| {
        let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
        // Skipped, not exempt:
        // - jobs.rs is the definition site: its `create_job` →
        //   `try_create_job` delegation is the helpers' own internals,
        //   not a campaign flow;
        // - this file (basename match, same caveat as CORPUS_EXCLUDE):
        //   the creator names appear in its regex literals and test
        //   fixtures, which would self-trip the scan.
        if rel == def_rel
            || p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "lint.rs")
        {
            return Ok(());
        }
        let src = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let exempt = CNP_EXEMPT.iter().find(|(f, _)| *f == rel).map(|(f, _)| *f);
        if let Some(f) = exempt {
            exempt_seen.insert(f);
        }
        sites += check_cnp_preflight_file(
            &src,
            &rel,
            &call_re,
            &verify_re,
            exempt.is_some(),
            &mut violations,
        );
        Ok(())
    })?;
    for (f, _) in CNP_EXEMPT {
        ensure!(
            exempt_seen.contains(f),
            "CNP_EXEMPT lists `{f}` but no such file was scanned — remove the entry \
             (or fix the path)",
        );
    }
    // Floor guard: 3 call sites today (eval, launch, repro). Near-zero
    // means the call-site detection or the scan root regressed, not
    // that xtask stopped creating Jobs.
    ensure!(
        sites >= 2,
        "replay-cnp-preflight found only {sites} engine-Job-creating call site(s) \
         under xtask/src — suspiciously few; the call-site detection or scan root \
         has regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} replay-cnp-preflight violation(s):\n    {}\n  every flow that \
             creates a rio-replay engine Job must first read the deployed \
             CiliumNetworkPolicy admissions back (preflight::verify_cnp_admissions) \
             — Cilium silently DROPS an unadmitted engine's scheduler/store gRPC, \
             so a drifted admission hangs the run instead of failing it. Run the \
             read-back before creating the Job (see `replay launch`'s pre-flight or \
             `replay repro`), or — only for a Job that never dials scheduler/store \
             — add a CNP_EXEMPT entry with a rationale in xtask/src/lint.rs",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(
        creators = creators.len(),
        call_sites = sites,
        exemptions = CNP_EXEMPT.len(),
        "replay-cnp-preflight ok"
    );
    Ok(())
}

/// Creator-fn set for [`replay_cnp_preflight`], derived from the
/// `replay::jobs` source: every `fn` whose name contains `create_job`.
/// Doc-comment cross-references don't match (no `fn` keyword);
/// `BTreeSet` for deterministic regex alternation.
fn extract_job_creators(src: &str) -> BTreeSet<String> {
    let re = regex::Regex::new(r"\bfn\s+(\w*create_job\w*)\s*\(").unwrap();
    re.captures_iter(src).map(|c| c[1].to_owned()).collect()
}

/// Free-function call regex for the creator set: `name(` with optional
/// whitespace. Matches `jobs::create_job(…)` and a bare imported call
/// alike; the leading `\b` keeps `create_job` from matching inside
/// `try_create_job`.
fn creator_call_regex(creators: &BTreeSet<String>) -> regex::Regex {
    let alt: Vec<String> = creators.iter().map(|a| regex::escape(a)).collect();
    regex::Regex::new(&format!(r"\b(?:{})\s*\(", alt.join("|"))).unwrap()
}

/// Scan one file for [`replay_cnp_preflight`]. Returns the number of
/// creator call sites found; pushes a violation for every call site in
/// an unexempt file with no code-level `verify_cnp_admissions` call,
/// and for an exemption gone stale (no call site left, or the file now
/// runs the read-back anyway).
fn check_cnp_preflight_file(
    src: &str,
    rel: &str,
    call_re: &regex::Regex,
    verify_re: &regex::Regex,
    exempt: bool,
    violations: &mut Vec<String>,
) -> usize {
    let mut site_lines: Vec<usize> = Vec::new();
    let mut has_verify = false;
    for (i, line) in src.lines().enumerate() {
        // Comment-only lines (and the comment tail of code lines) never
        // count — neither as a call site nor as the read-back. A
        // commented-out `verify_cnp_admissions(…)` is exactly the shape
        // this lint exists to refuse.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let code = strip_line_comment(line);
        if call_re.is_match(code) {
            site_lines.push(i + 1);
        }
        if verify_re.is_match(code) {
            has_verify = true;
        }
    }
    if exempt {
        if site_lines.is_empty() {
            violations.push(format!(
                "{rel}: CNP_EXEMPT lists this file but it has no engine-Job-creating \
                 call site — remove the entry",
            ));
        } else if has_verify {
            violations.push(format!(
                "{rel}: CNP_EXEMPT lists this file but it DOES call \
                 verify_cnp_admissions — the exemption is moot; remove the entry",
            ));
        }
    } else if !has_verify {
        for l in &site_lines {
            violations.push(format!(
                "{rel}:{l}: engine-Job-creating call with no verify_cnp_admissions \
                 read-back anywhere in this flow",
            ));
        }
    }
    site_lines.len()
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

    // ── bookkeeping-marker ─────────────────────────────────────────

    /// Run [`check_bookkeeping_markers`] over a synthetic source with
    /// the real accessor family; returns (call_sites, violations).
    fn bk(src: &str) -> (usize, Vec<String>) {
        let accessors = BTreeSet::from([
            "get_including_terminal_for_bookkeeping".to_owned(),
            "iter_including_terminal".to_owned(),
        ]);
        let re = call_site_regex(&accessors);
        let mut violations = Vec::new();
        let sites = check_bookkeeping_markers(src, "synthetic.rs", &re, &mut violations);
        (sites, violations)
    }

    #[test]
    fn accessor_extraction_from_wrapper_source() {
        let src = "impl Builds {\n\
                   \x20   /// [`get_including_terminal_for_bookkeeping`] xref only.\n\
                   \x20   pub fn get(&self, id: &Uuid) -> Option<&BuildInfo> { todo!() }\n\
                   \x20   pub fn get_including_terminal_for_bookkeeping(&self, id: &Uuid) {}\n\
                   \x20   pub fn iter_mut_including_terminal(&mut self) {}\n\
                   }\n";
        assert_eq!(
            extract_terminal_accessors(src),
            BTreeSet::from([
                "get_including_terminal_for_bookkeeping".to_owned(),
                "iter_mut_including_terminal".to_owned(),
            ]),
            "fn definitions extracted; live-only get and doc xrefs ignored"
        );
    }

    #[test]
    fn marked_call_sites_pass() {
        // Single line + trailing marker.
        let (sites, v) =
            bk("let b = m.get_including_terminal_for_bookkeeping(id); // Bookkeeping lookup: x\n");
        assert_eq!((sites, v.len()), (1, 0), "trailing marker blesses: {v:?}");

        // Marker block above a rustfmt-split chain.
        let (sites, v) = bk(
            "// Bookkeeping lookup: terminal entries wanted because reasons\n\
             // (second comment line).\n\
             let build = self\n\
             \x20   .builds\n\
             \x20   .get_including_terminal_for_bookkeeping(&build_id)\n\
             \x20   .ok_or(Error::NotFound)?;\n",
        );
        assert_eq!((sites, v.len()), (1, 0), "block marker blesses: {v:?}");

        // Marker above a for-header (call and `{` share the line).
        let (sites, v) = bk("// Bookkeeping lookup: sweep filters state explicitly\n\
             for (id, b) in self.builds.iter_including_terminal() {\n\
             }\n");
        assert_eq!((sites, v.len()), (1, 0), "for-header blessed: {v:?}");
    }

    #[test]
    fn unmarked_call_site_fails() {
        let (sites, v) = bk("let b = m.get_including_terminal_for_bookkeeping(id);\n");
        assert_eq!(sites, 1);
        assert_eq!(v.len(), 1, "unmarked call must be flagged");
        assert!(v[0].contains("synthetic.rs:1"), "names file:line: {v:?}");
    }

    #[test]
    fn marker_does_not_bless_across_statement_boundary() {
        // A `;` between marker and call cuts the blessing — one marker
        // cannot vouch for the next statement.
        let (sites, v) = bk("// Bookkeeping lookup: for the FIRST statement only\n\
             let a = m.get_including_terminal_for_bookkeeping(x);\n\
             let b = m.get_including_terminal_for_bookkeeping(y);\n");
        assert_eq!(sites, 2);
        assert_eq!(v.len(), 1, "second call unblessed: {v:?}");
        assert!(v[0].contains("synthetic.rs:3"));
    }

    #[test]
    fn trailing_marker_does_not_bless_across_statement_boundary() {
        // Same boundary rule for the TRAILING marker shape: a marker
        // trailing statement A's own (bounded) line justifies A only.
        // The `;` in the line's code part ends the statement, so the
        // next statement's call cannot ride on it.
        let (sites, v) = bk(
            "let a = m.get_including_terminal_for_bookkeeping(x); // Bookkeeping lookup: for a\n\
             let b = m.get_including_terminal_for_bookkeeping(y);\n",
        );
        assert_eq!(sites, 2);
        assert_eq!(v.len(), 1, "second call unblessed: {v:?}");
        assert!(v[0].contains("synthetic.rs:2"), "{v:?}");
    }

    #[test]
    fn trailing_marker_on_continuation_line_blesses_and_is_not_stale() {
        // The must-admit direction of the boundary rule: a marker
        // trailing an UNBOUNDED continuation line of the same statement
        // still blesses the call below it, and the downward stale walk
        // finds that call.
        let (sites, v) = bk("let b = m // Bookkeeping lookup: terminal entry wanted\n\
             \x20   .get_including_terminal_for_bookkeeping(y);\n");
        assert_eq!((sites, v.len()), (1, 0), "{v:?}");
    }

    #[test]
    fn trailing_marker_on_attribute_blesses_and_is_not_stale() {
        // Attributes belong to the statement below them, so a marker
        // trailing one justifies that statement — the same adjacency a
        // whole-line marker above the attribute has.
        let (sites, v) = bk("#[allow(unused)] // Bookkeeping lookup: reason\n\
             let b = m.get_including_terminal_for_bookkeeping(y);\n");
        assert_eq!((sites, v.len()), (1, 0), "{v:?}");

        // Attribute arguments may carry braces in strings; attribute
        // lines never bound the statement, in either walk direction.
        let (sites, v) = bk(
            "#[doc = \"{see} the convention\"] // Bookkeeping lookup: reason\n\
             let b = m.get_including_terminal_for_bookkeeping(y);\n",
        );
        assert_eq!((sites, v.len()), (1, 0), "{v:?}");
    }

    #[test]
    fn comment_mention_is_not_a_call_site() {
        let (sites, v) = bk(
            "// routes through .get_including_terminal_for_bookkeeping(id)\n\
             // Bookkeeping lookup: prose only, no call\n\
             let x = 1;\n",
        );
        assert_eq!(sites, 0, "comment text is not a call site");
        // ... and the marker with no call below it is stale.
        assert_eq!(v.len(), 1, "stale marker flagged: {v:?}");
        assert!(v[0].contains("stale"));
    }

    #[test]
    fn stale_marker_after_accessor_flip_fails() {
        // The shape left behind when a site is converted to the
        // live-only accessor but the justification comment is kept.
        let (sites, v) = bk("// Bookkeeping lookup: outdated rationale\n\
             let b = m.get(id);\n");
        assert_eq!(sites, 0);
        assert_eq!(v.len(), 1, "stale marker flagged: {v:?}");
        assert!(v[0].contains("stale"), "{v:?}");
    }

    #[test]
    fn stale_trailing_marker_fails() {
        // The same accessor-flip leftover in the TRAILING shape: the
        // call was converted to the live-only accessor and the trailing
        // justification kept. Honored markers must be stale-checkable in
        // every shape they are honored in, or a flipped site keeps a
        // justification that no longer justifies anything.
        let (sites, v) = bk("let b = m.get(id); // Bookkeeping lookup: outdated rationale\n");
        assert_eq!(sites, 0);
        assert_eq!(v.len(), 1, "stale trailing marker flagged: {v:?}");
        assert!(v[0].contains("stale"), "{v:?}");
    }

    #[test]
    fn bounded_trailing_marker_does_not_annotate_the_next_statement() {
        // The downward mirror of the boundary rule: a trailing marker on
        // a bounded line annotates that statement only, so a justified
        // call in the NEXT statement cannot keep it fresh. The call
        // itself is fine (its own whole-line marker blesses it); only
        // the leftover trailing marker is flagged.
        let (sites, v) = bk("let a = m.get(x); // Bookkeeping lookup: leftover\n\
             // Bookkeeping lookup: real reason\n\
             let b = m.get_including_terminal_for_bookkeeping(y);\n");
        assert_eq!(sites, 1);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(
            v[0].contains("synthetic.rs:1") && v[0].contains("stale"),
            "{v:?}"
        );
    }

    #[test]
    fn doc_comment_mention_is_prose_not_marker() {
        // `///` doc text describing the convention is neither a stale
        // marker nor a blessing for a following call.
        let (sites, v) = bk("/// Call sites carry a `// Bookkeeping lookup: <reason>`\n\
             /// comment, enforced by xtask lint.\n\
             pub struct Builds;\n");
        assert_eq!((sites, v.len()), (0, 0), "doc prose ignored: {v:?}");

        // ... and a doc comment cannot bless a call site.
        let (sites, v) = bk("/// Bookkeeping lookup: doc text, not a justification\n\
             fn f(m: &M) { m.get_including_terminal_for_bookkeeping(id); }\n");
        assert_eq!(sites, 1);
        assert_eq!(v.len(), 1, "doc comment must not bless: {v:?}");
    }

    // No on-tree test for `bookkeeping_marker` itself: like `helm_sla`
    // and `seccomp_allowlist` it reads sibling-crate files at runtime,
    // which the per-member nextest sandbox doesn't stage (manifests +
    // stub targets only). The `xtask-lint` flake check runs it against
    // the real tree.

    // ── replay-cnp-preflight ───────────────────────────────────────

    /// Run [`check_cnp_preflight_file`] over a synthetic source with
    /// the real creator set; returns (call_sites, violations).
    fn cnp(src: &str, exempt: bool) -> (usize, Vec<String>) {
        let creators = BTreeSet::from(["create_job".to_owned(), "try_create_job".to_owned()]);
        let call_re = creator_call_regex(&creators);
        let verify_re = regex::Regex::new(r"\bverify_cnp_admissions\s*\(").unwrap();
        let mut violations = Vec::new();
        let sites = check_cnp_preflight_file(
            src,
            "synthetic.rs",
            &call_re,
            &verify_re,
            exempt,
            &mut violations,
        );
        (sites, violations)
    }

    #[test]
    fn job_creator_extraction_from_jobs_source() {
        let src = "/// Create the Job in [`NS_REPLAY`] via [`try_create_job`].\n\
                   pub async fn try_create_job(client: &Client, job: &Job) -> Result<Outcome> {\n\
                   pub async fn create_job(client: &Client, job: &Job) -> Result<()> {\n\
                   pub fn created_jobs_report() {}\n";
        assert_eq!(
            extract_job_creators(src),
            BTreeSet::from(["create_job".to_owned(), "try_create_job".to_owned()]),
            "fn definitions extracted; doc xrefs and near-miss names ignored"
        );
    }

    #[test]
    fn cnp_guarded_flow_passes_unguarded_fails() {
        // The launch shape: creator call and read-back in the same
        // file, but the read-back sits inside a pre-flight helper
        // defined AFTER the create line — file scope, not line order,
        // is what the gate demands.
        let (sites, v) = cnp(
            "ui::step(&format!(\"apply campaign Job {id}\"), || {\n\
             \x20   jobs::create_job(&client, &job)\n\
             })\n\
             .await?;\n\
             async fn preflight_checks(client: &Client) -> Result<()> {\n\
             \x20   preflight::verify_cnp_admissions(client).await\n\
             }\n",
            false,
        );
        assert_eq!((sites, v.len()), (1, 0), "{v:?}");

        // No read-back anywhere: every creator call flagged, file:line.
        let (sites, v) = cnp(
            "jobs::create_job(&client, &job).await?;\n\
             jobs::try_create_job(&client, &job).await?;\n",
            false,
        );
        assert_eq!(sites, 2);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(
            v[0].contains("synthetic.rs:1") && v[1].contains("synthetic.rs:2"),
            "{v:?}"
        );
    }

    #[test]
    fn cnp_commented_out_read_back_does_not_count() {
        // The exact shape of removing the check: the call survives only
        // as comments (whole-line and trailing) — the gate must refuse.
        let (sites, v) = cnp(
            "// preflight::verify_cnp_admissions(&client)\n\
             jobs::create_job(&client, &job).await?; // verify_cnp_admissions(…) someday\n",
            false,
        );
        assert_eq!((sites, v.len()), (1, 1), "{v:?}");

        // …and a comment naming a creator is not a call site.
        let (sites, v) = cnp(
            "// launch applies the Job via jobs::create_job(&client, &job)\n\
             let x = 1;\n",
            false,
        );
        assert_eq!((sites, v.len()), (0, 0), "{v:?}");
    }

    #[test]
    fn cnp_exemption_covers_and_goes_stale() {
        // The exempt recorder shape: creator call, no read-back — clean.
        let (sites, v) = cnp("jobs::try_create_job(&client, &job).await?;\n", true);
        assert_eq!((sites, v.len()), (1, 0), "{v:?}");

        // Stale: the exempt file no longer creates engine Jobs.
        let (_, v) = cnp("let x = 1;\n", true);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("remove the entry"), "{v:?}");

        // Moot: the exempt file now runs the read-back anyway.
        let (_, v) = cnp(
            "preflight::verify_cnp_admissions(&client).await?;\n\
             jobs::try_create_job(&client, &job).await?;\n",
            true,
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("moot"), "{v:?}");
    }

    // Same story as above for `replay_cnp_preflight` itself: the file
    // walk needs the real repo layout, which the nextest sandbox
    // doesn't guarantee. The pure pieces are tested here; the
    // `xtask-lint` flake check runs the scan against the real tree.
}
