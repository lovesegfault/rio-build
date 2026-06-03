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
    /// No first-party code reads a structured-attrs string-list user
    /// attr (`rio_nix::derivation::structured_attrs::
    /// STRING_LIST_USER_ATTRS`) straight off an env map with a literal
    /// key — every read routes through the canonical
    /// `string_list_attr` precedence rule. Catches a new open-coded
    /// sibling of the structured-attrs-blind flat read (the shape that
    /// silently dropped `requiredSystemFeatures` for `__structuredAttrs`
    /// derivations).
    StructuredAttrReads,
    /// Every declared replay-trust contract row (capability gates,
    /// content digests, the gate's coverage witness, provenance
    /// classification, closure completeness) resolves to a NAMED
    /// consumer-side contract test that exists and exercises the row's
    /// artifact, or carries an explicit waiver with a reason. The row
    /// universe is parsed from the schema source (Capabilities /
    /// ContentDigests fields), so a new capability flag or digest field
    /// fails this lint until its enforcement test (or waiver) is
    /// registered. Catches write-only witnesses: contracts that are
    /// produced and documented but never demanded by any consumer.
    ContractRegistry,
    /// Every external-IO call shape (`.send()` dispatch, awaited
    /// `.collect()`/`.text()` body buffering, `.read_to_end(`,
    /// `.read_to_vec(`) in rio-replay's enrolled IO modules (supply,
    /// substituter, archive, artifact, recorder s3) carries an adjacent
    /// `// bounded-io: <bound>` marker stating its time/size bound or
    /// deliberate waiver, every marker annotates such a call, and every
    /// rio-replay module holding a raw `aws_sdk_s3::Client` is enrolled.
    /// External IO here means any byte source the peer or input artifact
    /// controls — network responses AND local decompression surfaces
    /// (the DwarFS image backend's `read_to_vec` is how an under-cap
    /// image's decompression bomb reached an unbounded buffer unmarked).
    /// Catches a new unbounded arm at introduction — the per-arm-guard
    /// pattern (deadline added where a problem was noticed, sibling arms
    /// left exposed) that produced the relay header-wait wedge and the
    /// unbounded narinfo collect.
    BoundedIo,
    /// Every production load of the supply journal (`StateFile::Supply`
    /// via `load_jsonl`) in rio-replay carries an adjacent
    /// `// supply-fold: <projection>` marker naming the
    /// `model::SupplyFold` projection the loaded rows fold through (or
    /// `exempt — <reason>` for non-fold reads), the named projection
    /// exists on the owner, and every marker annotates a real load.
    /// The projection vocabulary is parsed from the owner impl at scan
    /// time, so it can't drift. Catches a new journal consumer
    /// hand-rolling latest-row-wins over raw rows — the shape that let
    /// bookkeeping `skipped` rows displace the inline-resume gate's
    /// deferral evidence while the settlement-aware sibling folds read
    /// the same journal correctly.
    SupplyFoldOwner,
    /// Every revision-pinned spec-rule citation in docs/dev prose
    /// (`` `domain.area.detail+N` `` in backticks) exists in docs/spec
    /// at exactly that revision (some `#r("…+N")` declares it). Catches
    /// the prose half of a `tracey bump`: tracey re-validates code-side
    /// `r[impl]`/`r[verify]` markers against the bumped id, but
    /// markdown citations are invisible to it, so a bump silently
    /// orphans them.
    SpecRuleCitations,
    /// The replay engine's job scheduling-state transition ops are
    /// called only from their owner chokepoints: the watchdog's per-job
    /// mutations (`observe_job` / `confirm_queued_requeue` /
    /// `remove_job` / `grant_stall_grace`) only from the `JobLedger`,
    /// and in-flight reservation mutations only from the ledger's
    /// transitions and the owner-keyed settlement release in
    /// `submit.rs`. The chokepoints carry the staleness re-checks
    /// (owner/phase verified before any journal append or reservation
    /// strip), so a new direct caller would silently re-open the
    /// stale-watchdog-verdict class — phantom journal entries, stripped
    /// live reservations — that the chokepoints close. Catches the new
    /// consumer at introduction instead of at the next audit.
    ReplayTransitionOps,
    /// The replay engine's `BuildPathsWithResults` issuances are
    /// enumerable: every call of the transport's
    /// `build_paths_with_results[_observed]` lives in a sanctioned file
    /// — the submitter chokepoint, which derives the realized-closure
    /// workload estimate that keys the op's stderr drain budget, or the
    /// supply prefetch arm, whose `closure_nodes = 0` opt-out (the
    /// roots-scaled budget floor) is deliberate and shape-checked here.
    /// A new direct caller would silently choose its own budget keying;
    /// under-keying re-opens the healthy-log-heavy-batch cap trip
    /// (Wire error → channel abandonment → every in-flight build in the
    /// DAG cancelled) that workload keying closed. Catches the new
    /// caller — and any widening of the prefetch opt-out — at
    /// introduction instead of at the next audit.
    ReplayBuildOpCallers,
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
            Lint::StructuredAttrReads,
            Lint::ContractRegistry,
            Lint::BoundedIo,
            Lint::SupplyFoldOwner,
            Lint::SpecRuleCitations,
            Lint::ReplayTransitionOps,
            Lint::ReplayBuildOpCallers,
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
        Lint::StructuredAttrReads => structured_attr_reads(),
        Lint::ContractRegistry => contract_registry(),
        Lint::BoundedIo => bounded_io(),
        Lint::SupplyFoldOwner => supply_fold_owner(),
        Lint::SpecRuleCitations => spec_rule_citations(),
        Lint::ReplayTransitionOps => replay_transition_ops(),
        Lint::ReplayBuildOpCallers => replay_build_op_callers(),
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
/// the comment (e.g. inside a string literal) is code, not a
/// justification — comment detection is string-aware ([`line_comment`]).
fn has_trailing_marker(line: &str) -> bool {
    !line.trim_start().starts_with("//")
        && line_comment(line).is_some_and(|comment| comment.contains(BOOKKEEPING_MARKER))
}

/// Scan one line of Rust source: where its `//` LINE COMMENT starts (a
/// `//` inside a string or char literal — `"s3://…"` — is code, never a
/// comment start), plus the comment-stripped code with every literal
/// INTERIOR blanked to spaces (same byte length, quotes kept), so
/// `;`/`{`/`}` and call-shape tokens inside string literals are never
/// read as statement structure.
///
/// Lexed per line, honestly scoped: regular and byte strings honor
/// backslash escapes, raw strings (`r"…"`, `br#"…"#`) honor their hash
/// fences, and a `'` is consumed as a char literal only when a closing
/// quote follows (escape-aware), so lifetimes never open a phantom
/// literal. A literal SPANNING lines is out of model: the opening line
/// blanks to its end and reports no comment, but a continuation line is
/// scanned from column 0 with no carried state, so its interior may be
/// misread as code. That failure direction is loud, not silent — a stray
/// `;`/`{`/`}` on such an interior can only BOUND a marker walk early
/// (an unblessed-needle or stale-marker violation), never extend a
/// blessing across a real statement boundary.
fn scan_line(line: &str) -> (Option<usize>, String) {
    let bytes = line.as_bytes();
    let mut blanked = bytes.to_vec();
    let blank = |blanked: &mut Vec<u8>, from: usize, to: usize| {
        for b in &mut blanked[from..to] {
            *b = b' ';
        }
    };
    let ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                blanked.truncate(i);
                let code = String::from_utf8(blanked).expect("blanking preserves UTF-8");
                return (Some(i), code);
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                let end = loop {
                    match bytes.get(j) {
                        None => break None,
                        Some(b'\\') => j += 2,
                        Some(b'"') => break Some(j),
                        Some(_) => j += 1,
                    }
                };
                match end {
                    // Unterminated: the literal spans lines (or the line is
                    // a fragment); the rest of the line is string interior.
                    None => {
                        blank(&mut blanked, start.min(bytes.len()), bytes.len());
                        break;
                    }
                    Some(j) => {
                        blank(&mut blanked, start, j);
                        i = j + 1;
                    }
                }
            }
            b'r' | b'b' if !(i > 0 && ident(bytes[i - 1])) => {
                // Possible literal prefix: `b"…"`, `b'…'`, `r"…"`,
                // `br#…#"…"#…#`. Anything else starting with r/b is a
                // plain identifier.
                let mut j = i;
                if bytes[j] == b'b' {
                    j += 1;
                }
                let mut hashes = 0usize;
                let raw = bytes.get(j) == Some(&b'r');
                if raw {
                    j += 1;
                    while bytes.get(j) == Some(&b'#') {
                        hashes += 1;
                        j += 1;
                    }
                }
                match bytes.get(j) {
                    Some(b'"') if raw => {
                        // Raw string: closes at `"` + the same hash fence;
                        // backslashes are literal.
                        let start = j + 1;
                        let mut k = start;
                        let end = loop {
                            match bytes.get(k) {
                                None => break None,
                                Some(b'"')
                                    if bytes.len() >= k + 1 + hashes
                                        && bytes[k + 1..k + 1 + hashes]
                                            .iter()
                                            .all(|b| *b == b'#') =>
                                {
                                    break Some(k);
                                }
                                Some(_) => k += 1,
                            }
                        };
                        match end {
                            None => {
                                blank(&mut blanked, start.min(bytes.len()), bytes.len());
                                break;
                            }
                            Some(k) => {
                                blank(&mut blanked, start, k);
                                i = k + 1 + hashes;
                            }
                        }
                    }
                    // `b"…"` / `b'…'`: re-scan from the quote — the plain
                    // string/char arms handle the body.
                    Some(b'"' | b'\'') if !raw => i = j,
                    _ => i += 1,
                }
            }
            b'\'' => {
                // Char literal vs lifetime: a literal closes with a `'`
                // within its longest form (`'\u{10FFFF}'`); a lifetime
                // (`'a`, `'static`, `'_`) never does.
                let start = i + 1;
                let end = match bytes.get(start) {
                    Some(b'\\') => {
                        let mut j = start + 2;
                        loop {
                            match bytes.get(j) {
                                Some(b'\'') => break Some(j),
                                Some(_) if j - start <= 10 => j += 1,
                                _ => break None,
                            }
                        }
                    }
                    Some(_) if bytes.get(start + 1) == Some(&b'\'') => Some(start + 1),
                    _ => None,
                };
                match end {
                    Some(j) => {
                        blank(&mut blanked, start, j);
                        i = j + 1;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    (
        None,
        String::from_utf8(blanked).expect("blanking preserves UTF-8"),
    )
}

/// The `//` line comment of `line` (comment markers included), if one
/// exists outside every string/char literal. See [`scan_line`].
fn line_comment(line: &str) -> Option<&str> {
    scan_line(line).0.map(|i| &line[i..])
}

/// Code part of a line: everything before its real `//` comment, string
/// contents kept VERBATIM (a `//` inside a string literal is code). For
/// statement-structure questions — boundaries, call shapes — use
/// [`code_blanked`] instead, which also blanks literal interiors.
fn strip_line_comment(line: &str) -> &str {
    match scan_line(line).0 {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Structural view of a line's code: comment stripped AND every
/// string/char-literal interior blanked, so `contains(';')`-style
/// boundary tests and call-site regexes only ever see real code tokens.
fn code_blanked(line: &str) -> String {
    scan_line(line).1
}

/// Upward adjacency: does a `Bookkeeping lookup:` marker bless the
/// call at `call_idx`? The marker may trail the call line itself, or
/// sit above it separated only by comment lines, attributes, and
/// statement-continuation lines (trailing any of those intervening
/// lines, or as a whole-line comment). Any line with a `;`, `{`, or
/// `}` in its code part bounds the statement — a marker beyond it,
/// including one trailing the bounding line itself, belongs to a
/// different statement and does NOT bless this call. The boundary is
/// therefore checked on the structural code view ([`code_blanked`]:
/// comments stripped, string interiors blanked) BEFORE a trailing
/// marker is honored; the reverse order would let one statement's
/// justification bless its neighbor.
fn marker_blesses_call(lines: &[&str], call_idx: usize) -> bool {
    if has_trailing_marker(lines[call_idx]) {
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
        let code = code_blanked(line);
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
    let own_code = code_blanked(lines[marker_idx]);
    if call_re.is_match(&own_code) {
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
        let code = code_blanked(line);
        if call_re.is_match(&code) {
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

/// Structured-attrs read guard: no first-party code reads a
/// `STRING_LIST_USER_ATTRS` key straight off an env map — with the key
/// written literally OR named via the class's own `*_ATTR` consts —
/// every read routes through the canonical
/// `rio_nix::derivation::structured_attrs::string_list_attr` precedence
/// rule (or a carrier adapter feeding it).
///
/// Why: when `__structuredAttrs = true`, Nix serializes the user attrs
/// into `env["__json"]` ONLY, so a flat `.get("requiredSystemFeatures")`
/// silently returns None for exactly the derivations that declare
/// features. The precedence rule was open-coded per consumer (gateway
/// wire envs, recorder show JSON, replay-engine archive ATerms) and one
/// copy was structured-attrs-blind — under a prose comment claiming
/// parity with another site. The shared function ends the divergence;
/// this lint keeps a new open-coded sibling from re-introducing it. The
/// needle alphabet covers BOTH ways such a read gets written: the
/// historical literal-keyed shape, and the const-keyed shape
/// (`env.get(REQUIRED_SYSTEM_FEATURES_ATTR)`) that the module's own
/// `pub const` names — house style at every conforming call site —
/// make the natural way to write the next raw read. The canonical rule
/// and its adapters take the key as a parameter, so no conforming line
/// matches either shape; a read through a freshly-bound local alias is
/// outside the alphabet (a textual tripwire, not a type system).
///
/// Quantification domain: every `.rs` file under `src/`, `tests/`, and
/// `fuzz_targets/` of every root-manifest workspace member AND every
/// `[workspace] exclude` fuzz workspace — derived from the root
/// Cargo.toml, not hand-listed, so a new crate joins the domain when it
/// joins the workspace. A member with none of those directories staged
/// is a hard error (the nix check stages a fileset; a partial tree must
/// fail loud, not pass vacuously). Literal needles derive from
/// `STRING_LIST_USER_ATTRS` itself; const-name needles derive from
/// [`STRING_LIST_ATTR_CONSTS`], whose totality against the class is
/// enforced at scan time — an attr added to the class without its const
/// name registered here fails the lint loudly.
fn structured_attr_reads() -> Result<()> {
    let needles = structured_attr_needles()?;

    #[derive(serde::Deserialize)]
    struct RootManifest {
        workspace: RootWorkspace,
    }
    #[derive(serde::Deserialize)]
    struct RootWorkspace {
        members: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    }

    let root = repo_root();
    let manifest_path = root.join("Cargo.toml");
    let manifest: RootManifest = toml::from_str(
        &fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let crates: Vec<String> = manifest
        .workspace
        .members
        .into_iter()
        .chain(manifest.workspace.exclude)
        .collect();
    ensure!(
        crates.len() >= 10,
        "only {} workspace member/exclude dirs found in the root Cargo.toml — the manifest \
         shape changed; update `structured_attr_reads` in xtask/src/lint.rs",
        crates.len(),
    );

    let mut files = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for crate_dir in &crates {
        let crate_root = root.join(crate_dir);
        let mut scanned_any = false;
        for sub in ["src", "tests", "fuzz_targets"] {
            let dir = crate_root.join(sub);
            if !dir.is_dir() {
                continue;
            }
            scanned_any = true;
            walk_rs(&dir, &mut |p| {
                files += 1;
                let src =
                    fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
                check_structured_attr_file(&src, &rel, &needles, &mut violations);
                Ok(())
            })?;
        }
        ensure!(
            scanned_any,
            "workspace member {crate_dir} has no src/, tests/, or fuzz_targets/ directory in \
             the scanned tree — under the nix check this means the xtask-lint fileset \
             (nix/misc-checks.nix) no longer stages the lint's domain; a partial tree must \
             fail, not pass vacuously",
        );
    }
    // Floor guard: ~700 first-party .rs files today. Near-zero means the
    // walk or the member list regressed, not that the code went clean.
    ensure!(
        files >= 100,
        "structured-attr-reads scanned only {files} .rs file(s) — suspiciously few; the \
         workspace-member walk has regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} structured-attrs raw read(s):\n    {}\n  these attrs live in env[\"__json\"] \
             for __structuredAttrs derivations, so a flat read (literal-keyed or via the \
             *_ATTR consts) silently returns nothing for them; route the read through \
             rio_nix::derivation::structured_attrs::string_list_attr (via AtermEnv or a \
             carrier adapter)",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(
        attrs = rio_nix::derivation::structured_attrs::STRING_LIST_USER_ATTRS.len(),
        needles = needles.len(),
        crates = crates.len(),
        files,
        "structured-attr-reads ok"
    );
    Ok(())
}

/// `(const name, value)` for every `STRING_LIST_USER_ATTRS` member —
/// the const-keyed half of the [`structured_attr_reads`] needle
/// alphabet. The VALUES are the real consts (a rename refuses to
/// compile), and [`structured_attr_needles`] enforces totality against
/// the class const at scan time, so an attr added to
/// `STRING_LIST_USER_ATTRS` without its const name registered here
/// fails the lint loudly instead of silently scanning a smaller
/// alphabet.
const STRING_LIST_ATTR_CONSTS: [(&str, &str); 2] = [
    (
        "REQUIRED_SYSTEM_FEATURES_ATTR",
        rio_nix::derivation::structured_attrs::REQUIRED_SYSTEM_FEATURES_ATTR,
    ),
    (
        "IMPURE_ENV_VARS_ATTR",
        rio_nix::derivation::structured_attrs::IMPURE_ENV_VARS_ATTR,
    ),
];

/// The [`structured_attr_reads`] needle alphabet: per attr, the two
/// literal-keyed raw-read shapes — a map lookup (`env.get("attr")`) and
/// a JSON index (`payload["attr"]`) — plus the same two shapes keyed by
/// the attr's `pub const` name, bare and `structured_attrs::`-qualified
/// (the two spellings `use` conventions produce; a deeper path still
/// ends in one of them only when it ends in the bare const, which the
/// bare needle's `(`/`[` anchor deliberately does NOT match — the
/// alphabet is exact shapes, never substrings of identifiers).
fn structured_attr_needles() -> Result<Vec<String>> {
    use rio_nix::derivation::structured_attrs::STRING_LIST_USER_ATTRS;

    // Floor guard: the class currently lists 2 attrs. An empty/shrunken
    // const means the class definition moved — fail loud instead of
    // scanning for nothing.
    ensure!(
        STRING_LIST_USER_ATTRS.len() >= 2,
        "rio_nix::derivation::structured_attrs::STRING_LIST_USER_ATTRS has only {} entries \
         — the class definition moved or shrank; update `structured_attr_reads` in \
         xtask/src/lint.rs",
        STRING_LIST_USER_ATTRS.len(),
    );
    // Totality: every class member has exactly one registered const name.
    for key in STRING_LIST_USER_ATTRS {
        ensure!(
            STRING_LIST_ATTR_CONSTS
                .iter()
                .filter(|(_, value)| *value == key)
                .count()
                == 1,
            "STRING_LIST_USER_ATTRS entry {key:?} has no (or several) const-name registrations \
             in STRING_LIST_ATTR_CONSTS (xtask/src/lint.rs) — the const-keyed needle alphabet \
             would silently not cover it",
        );
    }
    ensure!(
        STRING_LIST_ATTR_CONSTS.len() == STRING_LIST_USER_ATTRS.len(),
        "STRING_LIST_ATTR_CONSTS registers {} const names but STRING_LIST_USER_ATTRS has {} \
         members — remove the stale registration",
        STRING_LIST_ATTR_CONSTS.len(),
        STRING_LIST_USER_ATTRS.len(),
    );

    let mut needles = Vec::new();
    for key in STRING_LIST_USER_ATTRS {
        needles.push(format!(".get(\"{key}\")"));
        needles.push(format!("[\"{key}\"]"));
    }
    for (name, _) in STRING_LIST_ATTR_CONSTS {
        needles.push(format!(".get({name})"));
        needles.push(format!(".get(structured_attrs::{name})"));
        needles.push(format!("[{name}]"));
        needles.push(format!("[structured_attrs::{name}]"));
    }
    Ok(needles)
}

/// Scan one file's lines for [`structured_attr_reads`] needles. Comment
/// lines may cite the hazardous shape when explaining it; only code
/// lines count.
fn check_structured_attr_file(
    src: &str,
    rel: &str,
    needles: &[String],
    violations: &mut Vec<String>,
) {
    for (idx, line) in src.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for needle in needles {
            if line.contains(needle.as_str()) {
                violations.push(format!("{rel}:{}: `{needle}`", idx + 1));
            }
        }
    }
}

/// How one declared-contract row is enforced.
#[derive(Debug, Clone, Copy)]
enum Enforcement {
    /// A consumer-side contract test: `test_fn` must exist in `file`'s
    /// TEST REGION (everything from the first `#[cfg(test)]` line on;
    /// the whole file for integration-test files without one), and that
    /// region's CODE — comment lines and trailing comments excluded —
    /// must contain every `artifact_needles` entry: strings from the
    /// test's canned hostile artifact or its artifact-driving calls (the
    /// flipped digest, the `checked: 0` gate document, the
    /// withdrawn-claim manifest edit). Production code is deliberately
    /// outside the search scope, so a needle that happens to name a
    /// production identifier cannot be satisfied by the production code
    /// itself surviving while the test decays — needles must live in the
    /// test, or the row fails. A call site is deliberately NOT
    /// acceptable enforcement: a consumer that parses-and-ignores
    /// resolves names just fine.
    Test {
        file: &'static str,
        test_fn: &'static str,
        artifact_needles: &'static [&'static str],
    },
    /// Deliberately unenforced, with the reason on record. The declared
    /// contract text must still exist (a waiver may not outlive its
    /// contract). Currently unused by the live registry — every row has a
    /// consumer-side test today, which is the desired steady state — but
    /// the variant (validated in this module's tests) is the only
    /// sanctioned way to register a row whose enforcement is consciously
    /// deferred, instead of leaving the row off the registry entirely.
    #[allow(dead_code)]
    Waived { reason: &'static str },
}

/// One row of the declared-contract registry: a trust contract the replay
/// design publishes (a capability gate, a content digest, a witness
/// field, a provenance classification), where its normative text lives,
/// and the consumer-side test that demands it.
#[derive(Debug, Clone, Copy)]
struct ContractRow {
    /// Stable row key. `capability.<flag>` and `content-digest.<field>`
    /// rows are REQUIRED by the parsed schema vocabulary; other keys are
    /// free-form.
    key: &'static str,
    /// `(file, needle)`: the file that declares the contract and a
    /// substring of its normative text. The lint fails when the needle
    /// disappears — a registry row may not outlive the contract it
    /// enforces.
    declared: (&'static str, &'static str),
    enforcement: Enforcement,
}

/// The declared-contract registry. Quantification domain: every
/// `Capabilities` flag and every `ContentDigests` field parsed from
/// `rio-replay/src/archive/schema.rs` (the schema structs ARE the
/// vocabulary — `Capability::enabled_in`'s exhaustive destructuring
/// couples the enum to the struct, and `ContentDigests::verify_at_open`
/// destructures every digest field), plus the named cross-crate witness
/// rows below. [`contract_registry`] fails when a vocabulary item has no
/// row, when a `capability.*`/`content-digest.*` row names a vocabulary
/// item that no longer exists, or when a row's named test/needles/
/// declaration cannot be resolved.
const CONTRACT_REGISTRY: &[ContractRow] = &[
    // ── Capability gates: one row per flag, all resolved by the
    // Capability::ALL behavioral flip test (its exhaustive match refuses
    // to compile for a new variant, and its artifact is the
    // withdrawn-claim manifest edit `set_capability_false`). ──
    ContractRow {
        key: "capability.timed",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "The timed scheduling mode",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "ensure_timed_capability"],
        },
    },
    ContractRow {
        key: "capability.expected_outcomes",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "Verdict comparison (§7 Comparison model)",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "expected_outcomes_for_units(&gated"],
        },
    },
    ContractRow {
        key: "capability.output_hashes",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "Output-divergence verdicts (§7 Comparison model)",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "unclaimed hashes are withheld"],
        },
    },
    ContractRow {
        key: "capability.embedded_store_paths",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "The archive rung of the supply ladder",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "!gated.has_embedded(SRC_PATH)"],
        },
    },
    ContractRow {
        key: "capability.impure_env",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "Impure demotion (§7, §8)",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "demoted_impure"],
        },
    },
    ContractRow {
        key: "capability.dependency_closures",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "Plan-time closure computation",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "capability_flags_gate_their_documented_engine_behavior",
            artifact_needles: &["set_capability_false", "SENTINEL_SRC"],
        },
    },
    // ── Content digests: one row per ContentDigests field, each with a
    // canned corrupted-artifact test at its designated verification
    // site (open-time recomputation, or the per-path dump check the
    // open-time waiver names). ──
    ContractRow {
        key: "content-digest.drvs",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "digest = SHA-256 of the ATerm bytes",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "drv_listing_digest_mismatch_is_detected",
            artifact_needles: &["cp -r $src $oux"],
        },
    },
    ContractRow {
        key: "content-digest.narinfo",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "SHA-256 of the sidecar file's bytes",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "narinfo_listing_digest_mismatch_is_detected",
            artifact_needles: &["narinfo listing digest mismatch"],
        },
    },
    ContractRow {
        key: "content-digest.embedded_store_paths",
        declared: (
            "rio-replay/src/archive/schema.rs",
            "SHA-256 of the uncompressed NAR serialization",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/archive/reader.rs",
            test_fn: "dump_nar_detects_sidecar_disagreement",
            artifact_needles: &["tampered after finalize"],
        },
    },
    // ── The regression gate's coverage witness: demanded at the design-
    // named CI consumption point (consumer side, across the JSON
    // boundary) AND derived on an evidence axis (producer side). ──
    ContractRow {
        key: "gate.checked-consumed",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "the single CI consumption point",
        ),
        enforcement: Enforcement::Test {
            file: "xtask/src/replay/report.rs",
            test_fn: "check_gate_demands_the_coverage_witness_across_the_wire",
            artifact_needles: &["\"tripped\":false,\"checked\":0"],
        },
    },
    ContractRow {
        key: "gate.checked-axis",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "is the gate's coverage witness",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/report.rs",
            test_fn: "gate_coverage_counts_evidence_not_classification_totality",
            artifact_needles: &["backfill must not mint coverage"],
        },
    },
    // ── …and the witness's sibling conjunct, the trip CONDITION: a pass
    // is meaningful only when the gate could have tripped at all. Under
    // fail_on "none" the trip predicate is the constant false, so the
    // consumption point must refuse to read the structural untripped
    // pass as verification. ──
    ContractRow {
        key: "gate.trippable-consumed",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "could have tripped at all",
        ),
        enforcement: Enforcement::Test {
            file: "xtask/src/replay/report.rs",
            test_fn: "check_gate_demands_a_trippable_gate_across_the_wire",
            artifact_needles: &["\"fail_on\":\"none\",\"tripped\":false,\"checked\":120"],
        },
    },
    ContractRow {
        key: "gate.trip-table",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "| `regression` | `unexpected-failure`",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/report.rs",
            test_fn: "gate_trip_sets_match_the_design_doc_table",
            artifact_needles: &["row_classes"],
        },
    },
    ContractRow {
        key: "gate.supply-failed-confidence",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "Counted against run confidence via the `supply-failed-units` low-confidence flag",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/report.rs",
            test_fn: "supply_failed_units_flag_low_confidence",
            artifact_needles: &["FLAG_SUPPLY_FAILED_UNITS"],
        },
    },
    // ── Provenance classification of campaign-spec fields (relaunch
    // guard) and its era-constant class. ──
    ContractRow {
        key: "spec.provenance-classification",
        declared: (
            "xtask/src/replay/launch.rs",
            "The strip list answers one question",
        ),
        enforcement: Enforcement::Test {
            file: "xtask/src/replay/launch.rs",
            test_fn: "spec_identity_strips_every_cluster_observed_field",
            artifact_needles: &["ERA_CONSTANTS", "IDENTITY_BEARING"],
        },
    },
    ContractRow {
        key: "spec.era-constants",
        declared: (
            "xtask/src/replay/launch.rs",
            "One more class is stripped: ERA CONSTANTS",
        ),
        enforcement: Enforcement::Test {
            file: "xtask/src/replay/launch.rs",
            test_fn: "guard_admits_old_era_records_and_still_blocks_intent_drift",
            artifact_needles: &["[\"tenants\"][\"upstreams_verified\"]"],
        },
    },
    // ── Drv-closure completeness: consumer-side membership cross-check
    // at plan time, plus the import-skip surfacing belt. ──
    ContractRow {
        key: "drv-closure.completeness",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "MUST be embedded",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/archive_input.rs",
            test_fn: "adjacency_closure_with_unembedded_member_is_a_plan_time_error",
            artifact_needles: &["not embedded in the archive"],
        },
    },
    ContractRow {
        key: "drv-closure.import-skip",
        declared: (
            "rio-replay/src/run/drv_import.rs",
            "so the submitter surfaces it on the",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/drv_import.rs",
            test_fn: "closure_skips_inputs_the_archive_does_not_embed",
            artifact_needles: &[
                "reported, not swallowed",
                "attributed to the root whose text closure reaches it",
            ],
        },
    },
    // ── …and the breadcrumb's CONSUMER: recording a skip is half the
    // contract — the batch-settle retirement is what makes a starved
    // root's failure archive-attributed instead of a regression charge.
    // (The skip field spent a round write-only: producers landed with
    // the doc-promise, no reader existed, and failed roots classified
    // Genuine — this row demands the reader stays wired.) ──
    ContractRow {
        key: "drv-closure.import-skip-consumed",
        declared: (
            "rio-replay/src/run/closure_gap.rs",
            "retires a failed root whose own text closure carried a gap",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/mod.rs",
            test_fn: "import_gap_starved_failures_retire_as_supply_failed",
            artifact_needles: &["only the transport member is re-offered", "import-gap"],
        },
    },
    // ── Plan-time demotion pin (resume classification stability). ──
    ContractRow {
        key: "plan.demotion-pin",
        declared: (
            "docs/dev/2026-05-28-build-replay-design.md",
            "Plan-time dispositions are pinned by the first plan",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/mod.rs",
            test_fn: "demotion_membership_pins_at_first_plan",
            artifact_needles: &["resolve_demoted_impure(Some(&pinned)"],
        },
    },
    // ── DAG-fallback blanket text-shape contract: the scheduler's
    // build-level first-failure summary (the shared rio-proto formatter
    // over the gateway's store-path DAG key) demanded by the replay
    // engine's blanket detector. The declared needle is the
    // fixture-discipline sentence on the formatter; the consumer test's
    // needles prove its fixtures still go through the formatter rather
    // than a hand-written string (the dead-detector shape this row
    // exists to prevent). ──
    ContractRow {
        key: "blanket.dag-first-failure-summary",
        declared: (
            "rio-proto/src/lib.rs",
            "Detector fixtures MUST be built through this function",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/collect.rs",
            test_fn: "dag_fallback_blanket_detector_is_producer_exact",
            artifact_needles: &[
                "rio_proto::dag_first_failure_summary",
                "is_dag_fallback_blanket",
            ],
        },
    },
    // ── Per-root status provenance: the one BuildStatus value the
    // gateway may mint with neither per-root scheduler evidence nor
    // store presence evidence (the lost-terminal evidence-loss row),
    // and what the measurement consumer must do with it (auto-retry,
    // then infra-indeterminate — never a substitution event, never a
    // genuine failure). The consumer test builds the row via the
    // producer's own constructor + the production wire codec, so the
    // contract holds across the crate boundary. ──
    ContractRow {
        key: "wire.lost-terminal-unverified",
        declared: (
            "rio-nix/src/protocol/build.rs",
            "The honest non-presence verdict",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/collect.rs",
            test_fn: "lost_terminal_unverified_row_is_evidence_loss_not_a_substitution_or_failure",
            artifact_needles: &[
                "lost_terminal_unverified()",
                "LOST_TERMINAL_UNVERIFIED_PREFIX",
            ],
        },
    },
    // ── The lost-terminal cell's other half: when store presence IS
    // confirmed, the wire stays Substituted (stock-client compat) and
    // the gateway disambiguates over the stderr side channel — the relay
    // marker line, formatted by the shared rio-nix producer pair. The
    // measurement consumer must route a Substituted row carrying the
    // marker to evidence-loss classification (auto-retry, then
    // infra-indeterminate), never to the target-substituted disposition
    // that force-build measurement tenants make definitionally
    // impossible — while an UNMARKED Substituted row keeps the
    // substitution-event leg. The consumer test builds both channels via
    // the producer's own chain (the wire codec for the row; the shared
    // formatter through the engine's capture for the marker). ──
    ContractRow {
        key: "wire.lost-terminal-relay-marker",
        declared: (
            "rio-nix/src/protocol/build.rs",
            "Leading bytes of the gateway's lost-terminal stderr RELAY line",
        ),
        enforcement: Enforcement::Test {
            file: "rio-replay/src/run/collect.rs",
            test_fn: "lost_terminal_marker_substituted_row_is_evidence_loss_not_target_substituted",
            artifact_needles: &["lost_terminal_relay_line(", "lost_terminals"],
        },
    },
];

/// Field names of one struct in `src`: the `pub <name>: <..>` lines
/// between `pub struct <name> {` and its closing brace (attribute and
/// comment lines skipped). Brace-counting is naive on purpose — the
/// schema structs are flat field lists.
fn struct_field_names(src: &str, struct_name: &str) -> Result<BTreeSet<String>> {
    let header = format!("pub struct {struct_name} {{");
    let start = src
        .find(&header)
        .with_context(|| format!("struct {struct_name} not found"))?;
    let body = &src[start + header.len()..];
    let end = body
        .find("\n}")
        .with_context(|| format!("struct {struct_name} has no closing brace"))?;
    let mut fields = BTreeSet::new();
    for line in body[..end].lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pub ")
            && let Some((name, _)) = rest.split_once(':')
        {
            fields.insert(name.trim().to_string());
        }
    }
    Ok(fields)
}

/// Validate the registry against the parsed vocabulary and a file reader.
/// Split from [`contract_registry`] so tests can feed synthetic sources.
fn check_contract_registry(
    rows: &[ContractRow],
    capability_flags: &BTreeSet<String>,
    digest_fields: &BTreeSet<String>,
    read: &dyn Fn(&str) -> Result<String>,
) -> Result<()> {
    let mut violations: Vec<String> = Vec::new();
    let keys: BTreeSet<&str> = rows.iter().map(|row| row.key).collect();
    ensure!(
        keys.len() == rows.len(),
        "contract registry has duplicate row keys"
    );

    // Vocabulary totality: every schema item has a row; every prefixed
    // row names a live schema item.
    for flag in capability_flags {
        let key = format!("capability.{flag}");
        if !keys.contains(key.as_str()) {
            violations.push(format!(
                "Capabilities field `{flag}` has no `{key}` contract-registry row — register \
                 its gate's consumer-side test (or a waiver with the reason)"
            ));
        }
    }
    for field in digest_fields {
        let key = format!("content-digest.{field}");
        if !keys.contains(key.as_str()) {
            violations.push(format!(
                "ContentDigests field `{field}` has no `{key}` contract-registry row — register \
                 its verifier's test (or a waiver with the reason)"
            ));
        }
    }
    for row in rows {
        if let Some(flag) = row.key.strip_prefix("capability.")
            && !capability_flags.contains(flag)
        {
            violations.push(format!(
                "registry row `{}` names a Capabilities field that no longer exists",
                row.key
            ));
        }
        if let Some(field) = row.key.strip_prefix("content-digest.")
            && !digest_fields.contains(field)
        {
            violations.push(format!(
                "registry row `{}` names a ContentDigests field that no longer exists",
                row.key
            ));
        }
    }

    // Per-row resolution: the declaration text and the named test (with
    // its canned-artifact needles) must exist.
    for row in rows {
        let (declared_file, declared_needle) = row.declared;
        match read(declared_file) {
            Err(e) => violations.push(format!(
                "row `{}`: declaration file {declared_file} unreadable: {e:#}",
                row.key
            )),
            Ok(text) if !text.contains(declared_needle) => violations.push(format!(
                "row `{}`: {declared_file} no longer contains the declared contract text \
                 {declared_needle:?} — update the row alongside the contract",
                row.key
            )),
            Ok(_) => {}
        }
        match row.enforcement {
            Enforcement::Waived { reason } => {
                if reason.trim().is_empty() {
                    violations.push(format!("row `{}`: waiver has no reason", row.key));
                }
            }
            Enforcement::Test {
                file,
                test_fn,
                artifact_needles,
            } => match read(file) {
                Err(e) => violations.push(format!(
                    "row `{}`: test file {file} unreadable: {e:#}",
                    row.key
                )),
                Ok(text) => {
                    let searchable = test_region_code(&text);
                    if !searchable.contains(&format!("fn {test_fn}(")) {
                        violations.push(format!(
                            "row `{}`: named contract test `{test_fn}` does not exist in \
                             {file}'s test region",
                            row.key
                        ));
                    }
                    for needle in artifact_needles {
                        if !searchable.contains(needle) {
                            violations.push(format!(
                                "row `{}`: {file}'s test region no longer contains the \
                                 artifact needle {needle:?} in code — the named test stopped \
                                 exercising the row's artifact (or the needle moved into \
                                 production/comments, where it proves nothing about the test)",
                                row.key
                            ));
                        }
                    }
                }
            },
        }
    }

    ensure!(
        violations.is_empty(),
        "contract-registry violations:\n  {}",
        violations.join("\n  ")
    );
    Ok(())
}

/// The searchable CODE of a file's test region, for [`Enforcement::Test`]
/// resolution: lines from the first `#[cfg(test)]` line on (the whole
/// file when none exists — integration-test files are all test), with
/// whole-line comments dropped and trailing comments stripped
/// (string-aware: an artifact needle inside a string literal — the
/// normal place for one — survives; a needle that only ever appeared in
/// prose does not). Joined with `\n` so needles can span a line's full
/// code but never cross lines.
///
/// Files that interleave `#[cfg(test)]` test-support items with
/// production code (e.g. archive_input.rs) get a region wider than
/// their tests module — the line-oriented scan cannot tell where a
/// cfg-gated item ends. The registry's needle discipline carries the
/// rest of the guarantee there: needles are strings unique to the named
/// test's body (canned-artifact text, test-local bindings), never bare
/// production identifiers, so surviving production code between
/// test-support items cannot satisfy a row whose test decayed.
fn test_region_code(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim() == "#[cfg(test)]")
        .unwrap_or(0);
    let mut code = String::new();
    for line in &lines[start..] {
        if line.trim_start().starts_with("//") {
            continue;
        }
        code.push_str(strip_line_comment(line));
        code.push('\n');
    }
    code
}

/// Declared-contract registry guard — see [`CONTRACT_REGISTRY`] and
/// [`Lint::ContractRegistry`].
fn contract_registry() -> Result<()> {
    let root = repo_root();
    let schema = fs::read_to_string(root.join("rio-replay/src/archive/schema.rs"))
        .context("read rio-replay/src/archive/schema.rs")?;
    let capability_flags = struct_field_names(&schema, "Capabilities")?;
    let digest_fields = struct_field_names(&schema, "ContentDigests")?;
    ensure!(
        !capability_flags.is_empty() && !digest_fields.is_empty(),
        "schema parse produced an empty vocabulary (parser drift?)"
    );
    let read = |rel: &str| -> Result<String> {
        fs::read_to_string(root.join(rel)).with_context(|| format!("read {rel}"))
    };
    check_contract_registry(CONTRACT_REGISTRY, &capability_flags, &digest_fields, &read)?;
    tracing::info!(
        rows = CONTRACT_REGISTRY.len(),
        capability_flags = capability_flags.len(),
        digest_fields = digest_fields.len(),
        "contract-registry ok"
    );
    Ok(())
}

/// Marker comment tag for [`bounded_io`]: `// bounded-io: <bound>`.
const BOUNDED_IO_MARKER: &str = "bounded-io:";

/// Bounded-IO marker gate over the replay engine's external-IO modules.
///
/// Quantification domain (stated, per the lint contract): production
/// code — everything before a file's first `#[cfg(test)]` line — under
/// EXACTLY these roots:
///
/// - `rio-replay/src/run/supply/` (the upload/prefetch execution arms),
/// - `rio-replay/src/substituter.rs` (the binary-cache client),
/// - `rio-replay/src/nixcache.rs` (the operator binary-cache client the
///   upstream-coverage probe rides),
/// - `rio-replay/src/archive/` (the S3 layout and image backends),
/// - `rio-replay/src/run/artifact.rs` (the campaign artifact store — the
///   module performing the largest single download in the system onto the
///   most disk-constrained pod),
/// - `rio-replay/src/s3.rs` (the recorder's archive-publication adapter
///   and by-recipe pointer PUT/GET);
///
/// The selection criterion for roots is a PROPERTY, not incident history:
/// every rio-replay module whose production code holds a raw
/// `aws_sdk_s3::Client` is enrolled, and the sweep at the end of the scan
/// enforces that as a standing check — a new raw-S3 module fails the lint
/// until it is enrolled and its IO sites carry markers. (The crate's
/// `reqwest` holders are only partially enrolled: `substituter.rs` is a
/// root; `hydra.rs` and `nixcache.rs` carry their own round-3 deadline+cap
/// treatment but are not yet swept — extending the property sweep to the
/// reqwest family rides with the encapsulation TODO below.)
///
/// and EXACTLY this needle alphabet ([`bounded_io_needle`]): `.send()`
/// (empty-parens HTTP/SDK request dispatch — channel/`mpsc` sends take an
/// argument and do not match), `.collect()`/`.text()` immediately awaited
/// (wholesale body buffering; iterator collects are never awaited),
/// `.chunk()` (reqwest's per-chunk body streaming — the accumulation
/// loop must state its cap; iterator `.chunks(n)` takes an argument and
/// never matches), and `.read_to_end(` (reader draining). These are the
/// modules where the resource-bound defect class clustered and the call
/// shapes it wore: a deadline or size cap added at the arm where a
/// problem was noticed while sibling arms stayed exposed (the narinfo
/// collect outliving its send-only timeout, the relay header wait with
/// no deadline at all).
///
/// Needle detection and the statement-boundary walks all read the
/// structural code view ([`code_blanked`]): the scanned modules are
/// saturated with `s3://`/`https://` string literals, and a naive
/// `split("//")` comment strip would eat a real `;`/`{`/`}` after such a
/// literal — letting one statement's marker bless its neighbor — while a
/// one-line `client.get("https://…").send()` would lose its needle
/// entirely and land unmarked.
///
/// The gate is two-directional:
///
/// - every needle must carry an adjacent `// bounded-io: <bound>` marker
///   — on the needle's line, or above it within the same statement — so
///   new IO in these modules cannot land without DECIDING and STATING
///   its time/size bound (or explicitly waiving one with the rationale);
/// - every marker must annotate a needle in its own statement, so a
///   justification cannot outlive the call it vouches for.
///
/// HONESTY CLAUSE: this is a textual tripwire, not a by-construction
/// guarantee. IO routed through helpers outside the alphabet, or living
/// outside the listed roots, is invisible to it. The by-construction
/// version is transport-handle encapsulation — bounded combinators that
/// OWN the reqwest/aws clients so a raw `.send()` is unreachable outside
/// them, making an unbounded arm unwritable rather than unblessed.
// TODO: transport-handle encapsulation for the replay engine's external
// IO: move the reqwest client and the aws-sdk clients behind combinators
// that demand a (deadline, size-bound) at construction or per call
// (`fetch_nar`'s take-discipline and `narinfo`'s deadline+cap are the
// shapes to generalize), then narrow this lint to "no raw client handles
// outside the combinator module" — at which point the by-construction
// claim above becomes real instead of aspirational.
fn bounded_io() -> Result<()> {
    let root = repo_root();
    // Module allowlist — see the doc comment for why exactly these.
    let file_roots = [
        "rio-replay/src/substituter.rs",
        "rio-replay/src/nixcache.rs",
        "rio-replay/src/run/artifact.rs",
        "rio-replay/src/s3.rs",
    ];
    let dir_roots = ["rio-replay/src/run/supply", "rio-replay/src/archive"];

    let mut needles = 0usize;
    let mut files = 0usize;
    let mut violations: Vec<String> = Vec::new();
    let mut scan = |path: &Path| -> Result<()> {
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        files += 1;
        needles += check_bounded_io_file(&src, &rel, &mut violations);
        Ok(())
    };
    for rel in file_roots {
        let path = root.join(rel);
        ensure!(path.is_file(), "bounded-io scan root {rel} not found");
        scan(&path)?;
    }
    for rel in dir_roots {
        let path = root.join(rel);
        ensure!(path.is_dir(), "bounded-io scan root {rel} not found");
        walk_rs(&path, &mut scan)?;
    }

    // Standing root-selection sweep: every rio-replay module whose
    // PRODUCTION code holds a raw `aws_sdk_s3::Client` must be one of the
    // scan roots above — the property the doc states, enforced as a check
    // so a new raw-S3 module cannot ship IO the marker gate never sees
    // (the escape that left run/artifact.rs, and then s3.rs, invisible).
    // Doc/comment mentions don't count: only the production region is
    // matched, with comment lines stripped.
    let enrolled = |rel: &str| -> bool {
        file_roots.contains(&rel)
            || dir_roots
                .iter()
                .any(|dir| rel.starts_with(&format!("{dir}/")))
    };
    let mut sweep = |path: &Path| -> Result<()> {
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let holds_raw_client = src
            .lines()
            .take_while(|line| line.trim() != "#[cfg(test)]")
            .any(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//") && trimmed.contains("aws_sdk_s3::Client")
            });
        if holds_raw_client && !enrolled(&rel) {
            violations.push(format!(
                "{rel}: production code holds a raw `aws_sdk_s3::Client` but the module is not \
                 a bounded-io scan root — enroll it (and mark its IO sites) so its calls cannot \
                 land unbounded"
            ));
        }
        Ok(())
    };
    walk_rs(&root.join("rio-replay/src"), &mut sweep)?;

    // Floor guards: ~29 production needles across ~13 files today. A
    // collapse means the needle detection or the roots regressed, not
    // that the modules stopped doing IO.
    ensure!(
        files >= 7,
        "bounded-io scan visited only {files} file(s) — a scan root has regressed",
    );
    ensure!(
        needles >= 19,
        "bounded-io scan found only {needles} IO call site(s) — suspiciously few; the needle \
         detection or the scan roots have regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} bounded-io violation(s):\n    {}\n  every external-IO call in the enrolled \
             rio-replay IO modules needs an adjacent `// {BOUNDED_IO_MARKER} <time/size bound, \
             or the deliberate waiver>` comment, every marker must annotate such a call, and \
             every module holding a raw S3 client must be enrolled",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(files, needles, "bounded-io ok");
    Ok(())
}

/// Scan one file for [`bounded_io`]: returns the number of needles found,
/// pushing a violation for every unblessed needle and every stale marker.
/// Only the production region is scanned — everything from the first
/// `#[cfg(test)]` line onward is test code (loopback fixtures, mock
/// clients), where bounding IO is meaningless.
fn check_bounded_io_file(src: &str, rel: &str, violations: &mut Vec<String>) -> usize {
    let lines: Vec<&str> = src.lines().collect();
    let prod_end = lines
        .iter()
        .position(|line| line.trim() == "#[cfg(test)]")
        .unwrap_or(lines.len());
    let lines = &lines[..prod_end];
    let mut needles = 0usize;
    for i in 0..lines.len() {
        let trimmed = lines[i].trim_start();
        if !trimmed.starts_with("//") && bounded_io_needle(lines, i).is_some() {
            needles += 1;
            if !bounded_io_marker_blesses(lines, i) {
                violations.push(format!(
                    "{rel}:{}: external-IO call (`{}`) without an adjacent \
                     `{BOUNDED_IO_MARKER}` bound statement",
                    i + 1,
                    bounded_io_needle(lines, i).expect("needle was just matched"),
                ));
            }
        }
        // Staleness covers BOTH marker placements — pure comment lines
        // AND markers trailing code — so a refactor cannot leave either
        // shape vouching for nothing. The marker token must sit in a real
        // comment ([`line_comment`]); a string literal mentioning the tag
        // is code, neither a blessing nor a staleness candidate.
        if (is_bounded_io_marker_comment(trimmed) || has_trailing_bounded_io_marker(lines[i]))
            && !bounded_io_marker_annotates(lines, i)
        {
            violations.push(format!(
                "{rel}:{}: stale `{BOUNDED_IO_MARKER}` marker — no external-IO call in the \
                 statement it annotates",
                i + 1,
            ));
        }
    }
    needles
}

/// Needle test for [`bounded_io`] at line `idx`: the structural code
/// view's external-IO call shape, if any. Reads [`code_blanked`], so a
/// needle token inside a string literal (a log message quoting
/// `.send()`, an inline `https://…` URL hiding the rest of the line)
/// neither matches nor masks.
///
/// `.collect()`/`.text()` count only when immediately awaited (same line,
/// or the next non-empty/non-comment line starts with `.await`) — that is
/// the body-buffering shape; iterator collects are never awaited, so the
/// adjacency requirement is what keeps them out of the alphabet.
fn bounded_io_needle(lines: &[&str], idx: usize) -> Option<&'static str> {
    let code = code_blanked(lines[idx]);
    if code.contains(".send()") {
        return Some(".send()");
    }
    if code.contains(".read_to_end(") {
        return Some(".read_to_end(");
    }
    // The dwarfs crate's whole-member buffering read: decompresses an
    // image-declared (attacker-sized) byte count into one Vec, so it is
    // exactly `.read_to_end(` with the network swapped for a local
    // decompression surface. Out of the alphabet, it landed unbounded in
    // the image backend while the module sat enrolled as a scan root.
    if code.contains(".read_to_vec(") {
        return Some(".read_to_vec(");
    }
    if code.contains(".chunk()") {
        return Some(".chunk()");
    }
    for (token, name) in [
        (".collect()", ".collect().await"),
        (".text()", ".text().await"),
    ] {
        if !code.contains(token) {
            continue;
        }
        if code.contains(".await") {
            return Some(name);
        }
        for line in lines.iter().skip(idx + 1) {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with(".await") {
                return Some(name);
            }
            break;
        }
    }
    None
}

/// Is this trimmed line a plain `//` comment carrying the bounded-io
/// marker? Doc comments (`///`, `//!`) describing the convention are
/// prose, not per-site bound statements.
fn is_bounded_io_marker_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        && !trimmed.starts_with("///")
        && !trimmed.starts_with("//!")
        && trimmed.contains(BOUNDED_IO_MARKER)
}

/// Does this code line carry the bounded-io marker in a TRAILING `//`
/// comment? String-aware like [`has_trailing_marker`]: a marker token
/// inside a string literal is code, not a bound statement.
fn has_trailing_bounded_io_marker(line: &str) -> bool {
    !line.trim_start().starts_with("//")
        && line_comment(line).is_some_and(|comment| comment.contains(BOUNDED_IO_MARKER))
}

/// Upward adjacency: does a `bounded-io:` marker bless the needle at
/// `needle_idx`? The marker may trail the needle line itself, or sit
/// above it separated only by comment/attribute/statement-continuation
/// lines. The statement boundary is checked on the COMMENT-STRIPPED code
/// FIRST: a line whose code part carries `;`, `{`, or `}` belongs to a
/// different statement, so a marker trailing IT cannot bless this needle
/// (honoring the trailing marker before the boundary would let one
/// statement's justification leak onto its neighbor).
fn bounded_io_marker_blesses(lines: &[&str], needle_idx: usize) -> bool {
    if has_trailing_bounded_io_marker(lines[needle_idx]) {
        return true;
    }
    let mut budget = 30usize;
    for line in lines[..needle_idx].iter().rev() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            if is_bounded_io_marker_comment(trimmed) {
                return true;
            }
            continue;
        }
        if trimmed.starts_with("#[") {
            continue;
        }
        let code = code_blanked(line);
        if code.contains(';') || code.contains('{') || code.contains('}') {
            return false;
        }
        if has_trailing_bounded_io_marker(line) {
            // Trailing marker on a continuation line of this statement.
            return true;
        }
    }
    false
}

/// Staleness check, mirroring [`bounded_io_marker_blesses`] exactly: the
/// marker at `marker_idx` (a pure comment line, or trailing a code line)
/// must share a statement with a needle. Pure comment lines look DOWN
/// into the first statement below; trailing markers look at their own
/// line and UP through the statement they terminate. Per code line the
/// needle test runs before the boundary test, so `…send().await?;` —
/// needle and terminator on one line — still counts.
fn bounded_io_marker_annotates(lines: &[&str], marker_idx: usize) -> bool {
    if bounded_io_needle(lines, marker_idx).is_some() {
        return true;
    }
    let mut budget = 30usize;
    if lines[marker_idx].trim_start().starts_with("//") {
        // Pure comment: scan down into the first statement.
        for (j, line) in lines.iter().enumerate().skip(marker_idx + 1) {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            if bounded_io_needle(lines, j).is_some() {
                return true;
            }
            let code = code_blanked(line);
            if code.contains(';') || code.contains('{') || code.contains('}') {
                return false;
            }
        }
    } else {
        // Trailing marker: scan up through its own statement.
        for j in (0..marker_idx).rev() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let trimmed = lines[j].trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            if bounded_io_needle(lines, j).is_some() {
                return true;
            }
            let code = code_blanked(lines[j]);
            if code.contains(';') || code.contains('{') || code.contains('}') {
                return false;
            }
        }
    }
    false
}

/// Marker prefix for [`supply_fold_owner`] call-site annotations.
const SUPPLY_FOLD_MARKER: &str = "supply-fold:";

/// Supply-journal fold-owner discipline: every PRODUCTION read of
/// supply.jsonl in rio-replay routes through a named `model::SupplyFold`
/// projection.
///
/// The journal's settlement contract (bookkeeping rows never displace a
/// settled truth, deferral evidence is redeemed by settlements only) is
/// enforced INSIDE the fold owner — but only for consumers that use it. A
/// new `load_jsonl(StateFile::Supply)` that hand-rolls latest-row-wins
/// over the raw rows silently re-derives weaker semantics; that exact
/// shape shipped the fail-open inline-resume gate (an unfiltered fold let
/// breaker `skipped` rows erase the gate's deferral evidence) while two
/// settlement-aware sibling folds read the same journal correctly.
///
/// Mechanics, two-directional like [`bookkeeping_marker`]:
///
/// - every `StateFile::Supply` use in rio-replay production code (the
///   region before the file's first `#[cfg(test)]`) is classified by its
///   call: `append_jsonl` = writer (the producer side, no marker),
///   `load_jsonl` = consumer — which must carry an adjacent
///   `// supply-fold: <projection>` marker within the preceding
///   [`SUPPLY_FOLD_WINDOW`] lines, naming the owner projection the rows
///   fold through, or `exempt — <reason>` for reads that fold no per-path
///   truth (e.g. the coverage probe's "which paths have ANY row"
///   presence set). Anything else fails loud: a new access shape must be
///   classified here before it ships.
/// - every marker must have a `StateFile::Supply` load within the
///   following [`SUPPLY_FOLD_WINDOW`] lines, so a justification cannot
///   outlive the load it vouches for.
///
/// The projection vocabulary is parsed from the `impl<'a> SupplyFold<'a>`
/// block in rio-replay's model.rs at scan time (its `pub fn` names), so a
/// renamed or removed projection invalidates the markers that name it
/// instead of letting them rot. `state.rs` is skipped: it owns the
/// `StateFile` enum (file-name table, sync manifest), not journal
/// semantics.
fn supply_fold_owner() -> Result<()> {
    let root = repo_root();
    let model_rel = "rio-replay/src/run/model.rs";
    let model_src =
        fs::read_to_string(root.join(model_rel)).with_context(|| format!("reading {model_rel}"))?;
    let projections = extract_supply_fold_projections(&model_src);
    ensure!(
        projections.len() >= 4,
        "only {} `pub fn` projection(s) found in {model_rel}'s SupplyFold impl — the owner \
         moved or the extraction regressed: {projections:?}",
        projections.len(),
    );

    let scan_root = root.join("rio-replay/src");
    ensure!(scan_root.is_dir(), "supply-fold scan root not found");
    let mut loads = 0usize;
    let mut writes = 0usize;
    let mut violations: Vec<String> = Vec::new();
    walk_rs(&scan_root, &mut |path: &Path| -> Result<()> {
        if path.ends_with("run/state.rs") {
            return Ok(());
        }
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let (file_loads, file_writes) =
            check_supply_fold_file(&src, &rel, &projections, &mut violations);
        loads += file_loads;
        writes += file_writes;
        Ok(())
    })?;

    // Floor guards: 4 production loads (gate, two rollup call sites, the
    // report refresh) + the coverage probe's exempt read, and at least
    // the upload-arm + probe + deferral writers, exist today. A collapse
    // means the detection regressed, not that the journal went unread.
    ensure!(
        loads >= 4,
        "supply-fold scan found only {loads} production load(s) of StateFile::Supply — \
         suspiciously few; the detection has regressed",
    );
    ensure!(
        writes >= 3,
        "supply-fold scan found only {writes} production append(s) of StateFile::Supply — \
         suspiciously few; the detection has regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} supply-fold violation(s):\n    {}\n  every production load of supply.jsonl \
             must carry an adjacent `// {SUPPLY_FOLD_MARKER} <projection>` naming the \
             model::SupplyFold projection it folds through (one of {projections:?}, or \
             `exempt — <reason>` for non-fold reads), and every marker must annotate a load",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(loads, writes, ?projections, "supply-fold-owner ok");
    Ok(())
}

/// `pub fn` names inside the `impl<'a> SupplyFold<'a>` block — the legal
/// marker vocabulary for [`supply_fold_owner`].
fn extract_supply_fold_projections(model_src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut in_impl = false;
    for line in model_src.lines() {
        if line.starts_with("impl<'a> SupplyFold<'a>") {
            in_impl = true;
            continue;
        }
        if in_impl {
            // The impl block closes at the first column-0 brace.
            if line.starts_with('}') {
                break;
            }
            if let Some(rest) = line.trim_start().strip_prefix("pub fn ")
                && let Some(name) = rest.split('(').next()
            {
                names.insert(name.trim().to_string());
            }
        }
    }
    names
}

/// How far (in lines) a `// supply-fold:` marker may sit above its load,
/// and a load below its marker — wide enough for a rationale paragraph
/// between them, narrow enough that a marker cannot vouch across
/// unrelated statements.
const SUPPLY_FOLD_WINDOW: usize = 12;

/// Scan one file for [`supply_fold_owner`]; returns `(loads, writes)`
/// found in the production region, pushing violations for unmarked or
/// misnamed loads, unclassifiable uses, and stale markers.
fn check_supply_fold_file(
    src: &str,
    rel: &str,
    projections: &BTreeSet<String>,
    violations: &mut Vec<String>,
) -> (usize, usize) {
    let lines: Vec<&str> = src.lines().collect();
    let prod_end = lines
        .iter()
        .position(|line| line.trim() == "#[cfg(test)]")
        .unwrap_or(lines.len());
    let lines = &lines[..prod_end];
    let mut loads = 0usize;
    let mut writes = 0usize;
    for i in 0..lines.len() {
        let trimmed = lines[i].trim_start();
        if lines[i].contains("StateFile::Supply") && !trimmed.starts_with("//") {
            // Classify by the owning call, which may sit up to two lines
            // above (`state.append_jsonl(\n StateFile::Supply, ...`).
            let window_start = i.saturating_sub(2);
            let call_window = lines[window_start..=i].join("\n");
            if call_window.contains("append_jsonl") {
                writes += 1;
            } else if call_window.contains("load_jsonl") {
                loads += 1;
                match supply_fold_marker_above(lines, i) {
                    None => violations.push(format!(
                        "{rel}:{}: supply.jsonl load without an adjacent \
                         `// {SUPPLY_FOLD_MARKER} <projection>` marker",
                        i + 1,
                    )),
                    Some(SupplyFoldMarker { name, has_reason })
                        if name == "exempt" && !has_reason =>
                    {
                        violations.push(format!(
                            "{rel}:{}: `{SUPPLY_FOLD_MARKER} exempt` needs a reason \
                             (`exempt — <why no per-path truth is folded>`)",
                            i + 1,
                        ));
                    }
                    Some(SupplyFoldMarker { name, .. })
                        if name != "exempt" && !projections.contains(&name) =>
                    {
                        violations.push(format!(
                            "{rel}:{}: supply-fold marker names `{name}`, which is not a \
                             SupplyFold projection ({projections:?}) or `exempt`",
                            i + 1,
                        ));
                    }
                    Some(_) => {}
                }
            } else {
                violations.push(format!(
                    "{rel}:{}: unclassifiable StateFile::Supply use (neither append_jsonl \
                     nor load_jsonl in reach) — classify the new access shape in this lint",
                    i + 1,
                ));
            }
        }
        // Staleness: a non-doc marker must annotate a load below it.
        if lines[i].contains(SUPPLY_FOLD_MARKER)
            && trimmed.starts_with("//")
            && !trimmed.starts_with("///")
            && !trimmed.starts_with("//!")
        {
            let below_end = (i + 1 + SUPPLY_FOLD_WINDOW).min(lines.len());
            let annotates = lines[i + 1..below_end]
                .iter()
                .any(|line| line.contains("StateFile::Supply"));
            if !annotates {
                violations.push(format!(
                    "{rel}:{}: stale `// {SUPPLY_FOLD_MARKER}` marker — no StateFile::Supply \
                     load within the next {SUPPLY_FOLD_WINDOW} lines",
                    i + 1,
                ));
            }
        }
    }
    (loads, writes)
}

/// A parsed `// supply-fold: <name> [— reason]` marker.
struct SupplyFoldMarker {
    /// The named projection (or `exempt`).
    name: String,
    /// Whether an em-dash reason follows the name.
    has_reason: bool,
}

/// The nearest `// supply-fold: <name> [— reason]` marker within
/// [`SUPPLY_FOLD_WINDOW`] lines above `load_idx`, if any. The scan walks
/// through the load's own statement lines and rationale comments — the
/// window bound and the marker-staleness direction keep it from
/// over-matching across unrelated statements.
fn supply_fold_marker_above(lines: &[&str], load_idx: usize) -> Option<SupplyFoldMarker> {
    let start = load_idx.saturating_sub(SUPPLY_FOLD_WINDOW);
    for i in (start..load_idx).rev() {
        let trimmed = lines[i].trim_start();
        if !trimmed.starts_with("//") {
            continue;
        }
        if let Some(rest) = trimmed
            .trim_start_matches('/')
            .trim_start()
            .strip_prefix(SUPPLY_FOLD_MARKER)
        {
            let rest = rest.trim_start();
            let name = rest
                .split([' ', '\u{2014}'])
                .next()
                .unwrap_or_default()
                .trim_end_matches(['.', ','])
                .to_string();
            return (!name.is_empty()).then_some(SupplyFoldMarker {
                name,
                has_reason: rest.contains('\u{2014}'),
            });
        }
    }
    None
}

/// Transition-ops chokepoint gate over the replay engine — see
/// [`Lint::ReplayTransitionOps`].
///
/// Quantification domain (stated): production code — everything before a
/// file's first `#[cfg(test)]` line, the same cut [`bounded_io`] uses —
/// in every `.rs` file under `rio-replay/src/`, against two needle sets:
///
/// - watchdog per-job mutation calls
///   (`.observe_job(` / `.confirm_queued_requeue(` / `.remove_job(` /
///   `.grant_stall_grace(`): allowed only in `run/ledger.rs`. The
///   definitions in `run/watchdog.rs` don't match (no receiver dot), so
///   the definition file needs no carve-out.
/// - in-flight reservation mutations (an `in_flight` binding or field
///   followed by `insert`/`remove`/`clear`/`retain`/`drain`/`entry`/
///   `get_mut` on one line): allowed in `run/ledger.rs` (commit /
///   stall-requeue / retire) and `run/submit.rs` (the owner-keyed
///   `release_after_settle`). Reads (`get`, `contains_key`, `len`,
///   iteration) are unrestricted.
///
/// HONESTY CLAUSE: textual tripwire, not a by-construction guarantee — a
/// mutation through a renamed binding (`let m = &mut *tracker.in_flight
/// .lock().await;`) or a helper would evade it. The by-construction
/// version is field privacy on `SubmitTracker.in_flight` with mutation
/// methods only; that refactor touches every wave-assembly read site and
/// is deliberately deferred — this lint is the standing consumer
/// enumeration until then.
fn replay_transition_ops() -> Result<()> {
    // (needle regex, sanctioned files, label) per op family. Each
    // sanctioned entry carries the rationale for WHY that file may
    // perform the op.
    let phase_re = regex::Regex::new(
        r"\.(observe_job|confirm_queued_requeue|remove_job|grant_stall_grace)\s*\(",
    )
    .unwrap();
    let in_flight_re = regex::Regex::new(
        r"\bin_flight\b[^;]*\.(insert|remove|clear|retain|drain|entry|get_mut)\s*\(",
    )
    .unwrap();
    const PHASE_SANCTIONED: &[(&str, &str)] = &[(
        "rio-replay/src/run/ledger.rs",
        "the JobLedger is the single owner of job-state transitions; every phase op rides a \
         journaled, staleness-checked transition",
    )];
    const IN_FLIGHT_SANCTIONED: &[(&str, &str)] = &[
        (
            "rio-replay/src/run/ledger.rs",
            "commit_batch reserves, requeue_stalled releases owner-keyed under the lock it \
             re-checks, retire releases on terminal authority",
        ),
        (
            "rio-replay/src/run/submit.rs",
            "SubmitTracker owns the map; release_after_settle is the owner-keyed settlement \
             release",
        ),
    ];

    let root = repo_root();
    let scan_root = root.join("rio-replay/src");
    ensure!(
        scan_root.is_dir(),
        "replay-transition-ops scan root {} not found",
        scan_root.display()
    );
    let mut phase_sites = 0usize;
    let mut in_flight_sites = 0usize;
    let mut violations: Vec<String> = Vec::new();
    walk_rs(&scan_root, &mut |path| {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let lines: Vec<&str> = src.lines().collect();
        let prod_end = lines
            .iter()
            .position(|line| line.trim() == "#[cfg(test)]")
            .unwrap_or(lines.len());
        let phase_ok = PHASE_SANCTIONED.iter().any(|(f, _)| *f == rel);
        let in_flight_ok = IN_FLIGHT_SANCTIONED.iter().any(|(f, _)| *f == rel);
        for (i, line) in lines[..prod_end].iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let code = strip_line_comment(line);
            for found in phase_re.find_iter(code) {
                // The JobLedger's pass-through API shares one method
                // name with the raw watchdog op (`grant_stall_grace`);
                // a call through a `*ledger` receiver IS the
                // chokepoint, not a bypass of it.
                if code[..found.start()].trim_end().ends_with("ledger") {
                    continue;
                }
                phase_sites += 1;
                if !phase_ok {
                    violations.push(format!(
                        "{rel}:{}: watchdog phase op outside the JobLedger chokepoint",
                        i + 1
                    ));
                }
            }
            if in_flight_re.is_match(code) {
                in_flight_sites += 1;
                if !in_flight_ok {
                    violations.push(format!(
                        "{rel}:{}: in-flight reservation mutation outside the sanctioned \
                         owners",
                        i + 1
                    ));
                }
            }
        }
        Ok(())
    })?;
    // Floor guards: 8 phase ops and 4 reservation mutations live in the
    // sanctioned files today. Near-zero means the needle detection or
    // the scan root regressed, not that the engine stopped scheduling.
    ensure!(
        phase_sites >= 5,
        "replay-transition-ops found only {phase_sites} watchdog phase-op call site(s) — \
         suspiciously few; the needle detection or scan root has regressed",
    );
    ensure!(
        in_flight_sites >= 3,
        "replay-transition-ops found only {in_flight_sites} in-flight mutation site(s) — \
         suspiciously few; the needle detection or scan root has regressed",
    );
    if !violations.is_empty() {
        bail!(
            "{} replay-transition-ops violation(s):\n    {}\n  job scheduling-state \
             transitions go through their owner chokepoints — JobLedger methods for phase \
             changes (journal + staleness re-check + observation together), \
             SubmitTracker::release_after_settle for settlement releases. A direct call \
             re-opens the stale-watchdog-verdict class (phantom journal entries, stripped \
             live reservations). Route through the ledger, or — for a genuinely new owner — \
             extend the sanctioned table in xtask/src/lint.rs with the rationale",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(phase_sites, in_flight_sites, "replay-transition-ops ok");
    Ok(())
}

/// Standing enumeration of the replay engine's `BuildPathsWithResults`
/// callers (see [`Lint::ReplayBuildOpCallers`]). The budget-keying rule
/// this enforces lives at `DaemonChannel::build_paths_with_results`
/// (rio-replay/src/run/transport.rs): `closure_nodes` keys the op's
/// stderr drain budget to its workload, and the submitter chokepoint is
/// the one place that derives a real estimate (the realized import
/// closure: order + skipped). The accepted divergence this pins: the
/// prefetch arm calls the transport DIRECTLY with `closure_nodes = 0`
/// (no closure-union estimate exists for prefetch plans, and prefetch
/// resolves via target-side substitution whose activity traffic sits far
/// below build-log volume — the roots-scaled floor is deliberate). That
/// opt-out stays acceptable only while it is the lone direct caller and
/// keeps its `0`; widening either axis must re-derive the budget keying
/// here, consciously, instead of inheriting the floor by accident.
fn replay_build_op_callers() -> Result<()> {
    // A call through a receiver (`channel.build_paths_with_results(`)
    // or a UFCS forward (`DaemonChannel::build_paths_with_results_observed(`)
    // — `fn` definitions and the `client_`-prefixed protocol functions
    // do not match.
    let call_re = regex::Regex::new(r"(\.|::)build_paths_with_results(_observed)?\s*\(").unwrap();
    // The sanctioned shape of the prefetch opt-out: the literal `0`
    // estimate in argument position. Spread over two lines by rustfmt it
    // still matches — the needle is applied to the joined statement
    // window below.
    let opt_out_re = regex::Regex::new(r"\.build_paths_with_results\s*\([^)]*,\s*0\s*\)").unwrap();
    const SANCTIONED: &[(&str, &str)] = &[
        (
            "rio-replay/src/run/submitter.rs",
            "the submission chokepoint: the SubmitChannel seam whose production impl forwards \
             to the transport with the chokepoint-derived workload estimate",
        ),
        (
            "rio-replay/src/run/supply/exec.rs",
            "the prefetch arm's documented closure_nodes=0 opt-out (roots-scaled budget floor)",
        ),
    ];

    let root = repo_root();
    let scan_root = root.join("rio-replay/src");
    ensure!(
        scan_root.is_dir(),
        "replay-build-op-callers scan root {} not found",
        scan_root.display()
    );
    let mut sites: Vec<(String, usize, String)> = Vec::new();
    let mut violations: Vec<String> = Vec::new();
    walk_rs(&scan_root, &mut |path| {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        // The transport module OWNS the methods; its `fn` definitions and
        // intra-doc links are not call sites, and the `(\.|::)` anchor
        // plus comment stripping below already exclude them — but keep
        // the owner scanned so a self-call would surface too.
        let src =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let lines: Vec<&str> = src.lines().collect();
        let prod_end = lines
            .iter()
            .position(|line| line.trim() == "#[cfg(test)]")
            .unwrap_or(lines.len());
        let sanctioned = SANCTIONED.iter().any(|(f, _)| *f == rel);
        for (i, line) in lines[..prod_end].iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let code = strip_line_comment(line);
            if !call_re.is_match(code) {
                continue;
            }
            // Join a small forward window so an argument list rustfmt
            // wrapped across lines is still shape-checkable.
            let window = lines[i..(i + 4).min(prod_end)].join(" ");
            sites.push((rel.clone(), i + 1, window));
            if !sanctioned {
                violations.push(format!(
                    "{rel}:{}: direct BuildPathsWithResults caller outside the sanctioned \
                     files — every issuance must key its stderr drain budget: route the \
                     submission through the submitter chokepoint (which derives the \
                     realized-closure estimate), or — for a genuinely new op family — extend \
                     the sanctioned table in xtask/src/lint.rs with the rationale AND the \
                     budget derivation",
                    i + 1
                ));
            }
        }
        Ok(())
    })?;
    // Exact-shape checks on the sanctioned universe. Counts are exact,
    // not floors: this universe is two sites today, and a third site in
    // a sanctioned FILE must still be looked at (the file sanction names
    // one call's rationale, not a blanket).
    let in_file = |file: &str| sites.iter().filter(|(rel, _, _)| rel == file).count();
    ensure!(
        in_file("rio-replay/src/run/submitter.rs") == 2,
        "replay-build-op-callers: expected exactly 2 sites in submitter.rs (the SubmitChannel \
         seam's production forward + the chokepoint's one issuance with the derived workload \
         estimate), found {} — a new call site in the chokepoint file still needs its budget \
         keying re-derived and this count re-pinned",
        in_file("rio-replay/src/run/submitter.rs"),
    );
    ensure!(
        in_file("rio-replay/src/run/supply/exec.rs") == 1,
        "replay-build-op-callers: expected exactly 1 prefetch call in supply/exec.rs, found \
         {} — the closure_nodes=0 opt-out is sanctioned for the lone prefetch arm only",
        in_file("rio-replay/src/run/supply/exec.rs"),
    );
    let prefetch_window = sites
        .iter()
        .find(|(rel, _, _)| rel == "rio-replay/src/run/supply/exec.rs")
        .map(|(_, _, window)| window.as_str())
        .unwrap_or_default();
    ensure!(
        opt_out_re.is_match(prefetch_window),
        "replay-build-op-callers: the prefetch arm's call no longer passes the literal \
         closure_nodes=0 opt-out — if it now derives a real estimate, move it behind the \
         submitter chokepoint's derivation (or update this lint's sanctioned shape with the \
         new keying's rationale); a non-zero ad-hoc estimate must not bypass the chokepoint",
    );
    if !violations.is_empty() {
        bail!(
            "{} replay-build-op-callers violation(s):\n    {}",
            violations.len(),
            violations.join("\n    "),
        );
    }
    tracing::info!(sites = sites.len(), "replay-build-op-callers ok");
    Ok(())
}

/// Recursive `.rs` walk via `std` (no `walkdir` dep). Follows symlinks
/// — under the nix flake check, the corpus dirs are staged into a
/// store-path source tree and may be symlinked.
fn walk_rs(dir: &Path, f: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    walk_ext(dir, "rs", f)
}

fn walk_ext(dir: &Path, ext: &str, f: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // `metadata()` (not `file_type()`) so symlinked dirs recurse.
        let md = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if md.is_dir() {
            walk_ext(&path, ext, f)?;
        } else if path.extension().is_some_and(|e| e == ext) {
            f(&path)?;
        }
    }
    Ok(())
}

/// Backticked revision-pinned rule-id pattern for [`spec_rule_citations`]:
/// a dotted lowercase id with an explicit `+N` revision suffix, e.g.
/// `` `sched.dispatch.fod-substitute+4` ``. Bare ids (no `+N`) and glob
/// citations (`sched.merge.substitute-*`) deliberately do not match: a
/// bare id tracks its rule across bumps and cannot dangle the same way.
fn versioned_citation_regex() -> regex::Regex {
    regex::Regex::new(r"`([a-z][a-z0-9-]*(?:\.[a-z0-9-]+)+\+[0-9]+)`").expect("static regex")
}

/// `#r(...)` declaration matcher for [`spec_rule_citations`], anchored at
/// a `#r(` token. Tolerates BOTH spec styles in use: the inline
/// `#r("id")` and crate-structure.typ's multi-line
/// `#r(\n  "id",\n)` — `\s` spans newlines and the trailing comma is
/// optional. Anchored (`\A`) so it can be applied AT each found token:
/// the universe scan is parity-checked token by token, not trusted to a
/// free-floating search.
fn rule_declaration_regex() -> regex::Regex {
    regex::Regex::new(r#"\A#r\(\s*"([a-z0-9.+-]+)"\s*,?\s*\)"#).expect("static regex")
}

/// Byte ranges of the comment regions in one Typst source, mirroring the
/// Typst lexer's precedence just far enough for the universe scan: raw
/// spans are opaque (a backtick run opens raw text until a matching run,
/// so the spec's `path/*.glob` and `nix run .#x` spellings cannot open
/// phantom comments), `//` opens a line comment to end-of-line UNLESS
/// immediately preceded by `:` (the URL shape — Typst's lexer consumes
/// `https://…` as a link before comment lexing can see its slashes), and
/// `/* … */` opens a block comment with Typst's nesting rule. An
/// unterminated block or raw span runs to end-of-file, conservatively
/// matching the lexer. A stray `*/` with no open block is plain text.
///
/// Deliberately NOT modeled: Typst string literals (only meaningful in
/// code mode, while spec prose is markup where `"` is plain text — naive
/// quote tracking would phase-flip on prose quotation marks and mis-hide
/// real comments). A code-mode string containing `//` or `/*` before a
/// declaration on the same line would mis-screen it; the failure is LOUD
/// (the declaration leaves the universe and its citations false-red),
/// and no spec file has that shape.
fn typ_comment_spans(src: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = src.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            // Raw span: a run of N backticks is closed by the next run of
            // >= N backticks. Comments never start inside raw text.
            b'`' => {
                let open_len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                i += open_len;
                // N == 2 is an EMPTY inline raw (`` ``) per the lexer; the
                // two backticks already consumed are the whole span.
                if open_len == 2 {
                    continue;
                }
                loop {
                    match bytes[i..].iter().position(|&b| b == b'`') {
                        None => {
                            i = bytes.len();
                            break;
                        }
                        Some(offset) => {
                            i += offset;
                            let close_len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                            i += close_len;
                            if close_len >= open_len {
                                break;
                            }
                        }
                    }
                }
            }
            // Line comment: `//` to end-of-line, with the `:`-guard for
            // URLs (`https://…`, `ssh-ng://…`).
            b'/' if bytes.get(i + 1) == Some(&b'/') && (i == 0 || bytes[i - 1] != b':') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                spans.push(start..i);
            }
            // Block comment, nested per the Typst lexer; unterminated
            // runs to end-of-file.
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                spans.push(start..i);
            }
            _ => i += 1,
        }
    }
    spans
}

/// Collect every `#r("…")` rule declaration from one spec file into
/// `universe`, with an inventory-parity guarantee: EVERY `#r(` token in
/// the file must either sit inside a comment ([`typ_comment_spans`]: a
/// `//` line comment or a `/* */` block comment — a prose mention or
/// dead text) or parse as a declaration ([`rule_declaration_regex`]
/// anchored at the token). Anything else is a violation — a declaration
/// style this matcher does not parse — so a third style fails the lint
/// loudly instead of silently shrinking the universe (the original
/// inline-only matcher missed all 19 multi-line declarations and would
/// have false-redded citations of perfectly live rules).
///
/// The comment screen runs BEFORE the parse attempt, in that order
/// deliberately: a commented-out declaration is dead text however
/// cleanly it parses, and admitting it would keep its id alive in the
/// universe — prose citations of a rule the spec no longer declares
/// would silently stay green (the lint's one job is catching exactly
/// that dangle). Returns the number of declaration tokens parsed, so
/// callers can assert the full accounting: parsed + commented +
/// violations == `#r(` tokens.
fn collect_spec_rules(
    src: &str,
    rel: &str,
    universe: &mut BTreeSet<String>,
    violations: &mut Vec<String>,
) -> usize {
    let decl_re = rule_declaration_regex();
    let comment_spans = typ_comment_spans(src);
    let mut parsed = 0usize;
    for (pos, _) in src.match_indices("#r(") {
        // Commented (line or block): a tolerated mention, never a
        // declaration — classification first, interpretation second.
        if comment_spans.iter().any(|span| span.contains(&pos)) {
            continue;
        }
        if let Some(cap) = decl_re.captures(&src[pos..]) {
            universe.insert(cap[1].to_string());
            parsed += 1;
            continue;
        }
        let line_no = src[..pos].matches('\n').count() + 1;
        violations.push(format!(
            "{rel}:{line_no}: `#r(` token is neither a comment-screened mention nor a \
             declaration this matcher parses — if a new declaration style was introduced, \
             extend `rule_declaration_regex` (the citation universe would otherwise silently \
             shrink and live citations would false-red)",
        ));
    }
    parsed
}

/// Scan one prose file for versioned citations and record each one that
/// the spec universe does not declare at that exact revision. Returns the
/// number of citations seen (matching or not).
fn check_spec_rule_citations(
    universe: &BTreeSet<String>,
    rel: &str,
    src: &str,
    violations: &mut Vec<String>,
) -> usize {
    let cite_re = versioned_citation_regex();
    let mut citations = 0usize;
    for (idx, line) in src.lines().enumerate() {
        for cap in cite_re.captures_iter(line) {
            citations += 1;
            let id = &cap[1];
            if !universe.contains(id) {
                violations.push(format!(
                    "{rel}:{}: cites `{id}`, but the scanned docs/spec universe ({} rules) does \
                     not declare it. Either the rule was bumped (re-point the citation at the \
                     live revision, or drop the `+N` suffix to track the rule across bumps), \
                     the citation is a typo, or — if the rule IS live in docs/spec — the \
                     declaration matcher missed its style: extend `rule_declaration_regex`.",
                    idx + 1,
                    universe.len(),
                ));
            }
        }
    }
    citations
}

/// Versioned spec-rule citation gate over docs/dev prose.
///
/// Quantification domain (stated, per the lint contract):
///
/// - citations: every backticked `` `<id>+<N>` `` span in
///   `docs/dev/**/*.md` matching [`versioned_citation_regex`];
/// - universe: every `#r("<id>")` declaration in `docs/spec/**/*.typ`,
///   inline or multi-line ([`rule_declaration_regex`]), with
///   inventory parity enforced per `#r(` token
///   ([`collect_spec_rules`]: comment-screened mention (line or block,
///   [`typ_comment_spans`]) or parsed declaration, or the lint is red —
///   commented-out declarations are dead text, never universe entries).
///
/// Each citation must name an id+revision the spec declares verbatim.
/// One-directional by design (prose → spec): the spec owes prose
/// nothing. Bare and glob citations are out of domain (see the regex
/// doc); so is prose outside docs/dev (READMEs, code comments — code
/// markers are tracey's domain). HONESTY CLAUSE: this validates that a
/// pinned citation EXISTS, not that the surrounding sentence is true of
/// that revision's text — semantic drift still needs a human read.
///
/// Why this exists: `tracey bump` re-validates spec `#r()` declarations
/// and code-side `r[impl]`/`r[verify]` markers, and tracey-validate
/// fails CI on dangling code markers — but a markdown citation is
/// invisible to both, so every bump silently orphans prose pins (a
/// `+3` citation survived the `sched.dispatch.fod-substitute` +3→+4
/// bump as the tree's only `+3` reference).
fn spec_rule_citations() -> Result<()> {
    let root = repo_root();
    let mut universe: BTreeSet<String> = BTreeSet::new();
    let mut decl_violations = Vec::new();
    let mut declarations = 0usize;
    walk_ext(&root.join("docs/spec"), "typ", &mut |path| {
        let src = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        declarations += collect_spec_rules(&src, &rel, &mut universe, &mut decl_violations);
        Ok(())
    })?;
    ensure!(
        decl_violations.is_empty(),
        "{} unparsed `#r(` declaration token(s) — the citation universe is incomplete:\n{}",
        decl_violations.len(),
        decl_violations.join("\n")
    );
    ensure!(
        !universe.is_empty(),
        "docs/spec scan produced an empty rule universe (parser drift?)"
    );

    let mut violations = Vec::new();
    let mut citations = 0usize;
    walk_ext(&root.join("docs/dev"), "md", &mut |path| {
        let src = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        citations += check_spec_rule_citations(&universe, &rel, &src, &mut violations);
        Ok(())
    })?;
    ensure!(
        violations.is_empty(),
        "{} dangling spec-rule citation(s):\n{}",
        violations.len(),
        violations.join("\n")
    );
    tracing::info!(
        citations,
        declarations,
        rules = universe.len(),
        "spec-rule-citations ok"
    );
    Ok(())
}

// `lint.rs` is in `CORPUS_EXCLUDE`, so the synthetic table names below
// can't leak into the schema-liveness corpus and mask a real dead table.
#[cfg(test)]
mod tests {
    use super::*;

    /// One synthetic registry row for the negative checks below.
    fn synthetic_row(enforcement: Enforcement) -> ContractRow {
        ContractRow {
            key: "capability.alpha",
            declared: ("schema.rs", "alpha gates the frobnicator"),
            enforcement,
        }
    }

    /// File reader over an in-memory map.
    fn reader<'a>(
        files: &'a std::collections::BTreeMap<&'static str, &'static str>,
    ) -> impl Fn(&str) -> Result<String> + 'a {
        move |rel: &str| {
            files
                .get(rel)
                .map(|text| (*text).to_string())
                .with_context(|| format!("no such file {rel}"))
        }
    }

    #[test]
    fn contract_registry_resolves_rows_to_real_tests() {
        let vocab: BTreeSet<String> = BTreeSet::from(["alpha".to_string()]);
        let digests: BTreeSet<String> = BTreeSet::new();
        let files = std::collections::BTreeMap::from([
            ("schema.rs", "alpha gates the frobnicator"),
            (
                "tests.rs",
                "#[test]\nfn alpha_gate_flip() { let artifact = \"flipped-alpha\"; }",
            ),
        ]);

        // A complete row resolves: declaration + test fn + artifact needle.
        let good = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["flipped-alpha"],
        });
        check_contract_registry(&[good], &vocab, &digests, &reader(&files)).unwrap();

        // A vocabulary item with no row fails naming the missing key.
        let err = check_contract_registry(&[], &vocab, &digests, &reader(&files))
            .unwrap_err()
            .to_string();
        assert!(err.contains("capability.alpha"), "{err}");

        // A row whose named test does not exist fails — name resolution
        // to a CALL SITE (or to nothing) is not enforcement.
        let missing_test = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "nonexistent_test",
            artifact_needles: &[],
        });
        let err = check_contract_registry(&[missing_test], &vocab, &digests, &reader(&files))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonexistent_test"), "{err}");

        // A row whose artifact needle vanished fails: the named test may
        // have stopped exercising the row's artifact.
        let stale_needle = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["needle-that-was-removed"],
        });
        let err = check_contract_registry(&[stale_needle], &vocab, &digests, &reader(&files))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needle-that-was-removed"), "{err}");

        // The decayed-test bypass: the row's needle surviving in
        // PRODUCTION code (or in comments) proves nothing about the
        // test, so resolution is scoped to the test region's code. A
        // gutted test body fails the row even though the file as a
        // whole still contains both the fn name shape and the needle.
        let files_gutted = std::collections::BTreeMap::from([
            ("schema.rs", "alpha gates the frobnicator"),
            (
                "tests.rs",
                "pub fn flip_helper() { let artifact = \"flipped-alpha\"; }\n\
                 // prose mention: flipped-alpha\n\
                 #[cfg(test)]\n\
                 mod tests {\n\
                 \x20   #[test]\n\
                 \x20   fn alpha_gate_flip() { assert!(true); }\n\
                 }\n",
            ),
        ]);
        let gutted = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["flipped-alpha"],
        });
        let err = check_contract_registry(&[gutted], &vocab, &digests, &reader(&files_gutted))
            .unwrap_err()
            .to_string();
        assert!(err.contains("flipped-alpha"), "{err}");
        assert!(err.contains("test region"), "{err}");

        // …and a needle INSIDE the test region resolves even when the
        // file has a production region above it; one in a test-region
        // comment still does not (prose is not exercise).
        let files_split = std::collections::BTreeMap::from([
            ("schema.rs", "alpha gates the frobnicator"),
            (
                "tests.rs",
                "pub fn production() {}\n\
                 #[cfg(test)]\n\
                 mod tests {\n\
                 \x20   // commentary: decoy-needle\n\
                 \x20   #[test]\n\
                 \x20   fn alpha_gate_flip() { let artifact = \"flipped-alpha\"; }\n\
                 }\n",
            ),
        ]);
        let good_split = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["flipped-alpha"],
        });
        check_contract_registry(&[good_split], &vocab, &digests, &reader(&files_split)).unwrap();
        let comment_only = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["decoy-needle"],
        });
        let err = check_contract_registry(&[comment_only], &vocab, &digests, &reader(&files_split))
            .unwrap_err()
            .to_string();
        assert!(err.contains("decoy-needle"), "{err}");

        // A row may not outlive its contract text.
        let files_without_decl = std::collections::BTreeMap::from([
            ("schema.rs", "the contract moved away"),
            ("tests.rs", "fn alpha_gate_flip() { \"flipped-alpha\"; }"),
        ]);
        let good = synthetic_row(Enforcement::Test {
            file: "tests.rs",
            test_fn: "alpha_gate_flip",
            artifact_needles: &["flipped-alpha"],
        });
        let err = check_contract_registry(&[good], &vocab, &digests, &reader(&files_without_decl))
            .unwrap_err()
            .to_string();
        assert!(err.contains("declared contract text"), "{err}");

        // A stale row naming a dead vocabulary item fails.
        let empty_vocab: BTreeSet<String> = BTreeSet::new();
        let err = check_contract_registry(
            &[synthetic_row(Enforcement::Waived { reason: "n/a" })],
            &empty_vocab,
            &digests,
            &reader(&files),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no longer exists"), "{err}");

        // Waivers carry a reason; an empty one fails.
        let err = check_contract_registry(
            &[synthetic_row(Enforcement::Waived { reason: "  " })],
            &vocab,
            &digests,
            &reader(&files),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("waiver has no reason"), "{err}");
        check_contract_registry(
            &[synthetic_row(Enforcement::Waived {
                reason: "verified per path at dump time instead",
            })],
            &vocab,
            &digests,
            &reader(&files),
        )
        .unwrap();
    }

    #[test]
    fn struct_field_names_parses_flat_schema_structs() {
        let src = "/// doc\npub struct Capabilities {\n    #[serde(default)]\n    pub timed: bool,\n    // comment\n    #[serde(default)]\n    pub impure_env: bool,\n}\n\npub struct Other {\n    pub x: u64,\n}\n";
        assert_eq!(
            struct_field_names(src, "Capabilities").unwrap(),
            BTreeSet::from(["timed".to_string(), "impure_env".to_string()])
        );
        assert_eq!(
            struct_field_names(src, "Other").unwrap(),
            BTreeSet::from(["x".to_string()])
        );
        assert!(struct_field_names(src, "Absent").is_err());
    }

    /// The real registry against the real tree: every published
    /// capability flag and digest field is registered, every named test
    /// exists, every needle resolves. Skipped (with a note) where the
    /// sibling crates are not in the staged source tree — the
    /// `xtask-lint` flake check runs against the full workspace and is
    /// the enforcement point.
    #[test]
    fn contract_registry_passes_on_the_real_tree() {
        match contract_registry() {
            Ok(()) => {}
            Err(e) => {
                let message = format!("{e:#}");
                if message.contains("read rio-replay/src/archive/schema.rs") {
                    eprintln!(
                        "sibling crate sources not present in this build's tree; the \
                         xtask-lint flake check enforces the registry"
                    );
                } else {
                    panic!("{message}");
                }
            }
        }
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

    /// Run [`check_supply_fold_file`] over a synthetic source against a
    /// fixed projection vocabulary; returns `(loads, writes, violations)`.
    fn sf(src: &str) -> (usize, usize, Vec<String>) {
        let projections: BTreeSet<String> = ["latest_settlements", "report_outcomes"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut violations = Vec::new();
        let (loads, writes) = check_supply_fold_file(src, "f.rs", &projections, &mut violations);
        (loads, writes, violations)
    }

    /// The supply-fold scanner's classification table: appends pass with
    /// no marker (writers are the producer side), a load with a
    /// projection-naming marker passes, an exempt-with-reason passes,
    /// and the violation classes — unmarked load, unknown projection
    /// name, reasonless exempt, unclassifiable access shape, stale
    /// marker — each trip. Test code after `#[cfg(test)]` is invisible.
    #[test]
    fn supply_fold_scanner_classifies_loads_writes_and_markers() {
        // Writers: no marker needed, counted as writes.
        let (loads, writes, violations) =
            sf("    state.append_jsonl(\n        StateFile::Supply,\n        &row,\n    )?;\n");
        assert_eq!((loads, writes), (0, 1));
        assert!(violations.is_empty(), "{violations:?}");

        // Marked load with a legal projection: clean.
        let (loads, _, violations) = sf(
            "    // supply-fold: latest_settlements — rollup input.\n    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n",
        );
        assert_eq!(loads, 1);
        assert!(violations.is_empty(), "{violations:?}");

        // Exempt with a reason: clean. Without one: violation.
        let (_, _, violations) = sf(
            "    // supply-fold: exempt — presence set only.\n    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n",
        );
        assert!(violations.is_empty(), "{violations:?}");
        let (_, _, violations) = sf(
            "    // supply-fold: exempt\n    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n",
        );
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("needs a reason"), "{violations:?}");

        // Unmarked load: violation.
        let (_, _, violations) =
            sf("    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("without an adjacent"),
            "{violations:?}"
        );

        // Marker naming a non-projection: violation.
        let (_, _, violations) = sf(
            "    // supply-fold: hand_rolled_fold\n    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n",
        );
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("hand_rolled_fold"), "{violations:?}");

        // Unclassifiable access shape: violation demanding lint extension.
        let (_, _, violations) = sf("    let f = StateFile::Supply;\n");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("unclassifiable"), "{violations:?}");

        // Stale marker (no load below): violation.
        let (_, _, violations) = sf("    // supply-fold: latest_settlements\n    let x = 1;\n");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("stale"), "{violations:?}");

        // Test region is invisible: an unmarked load after #[cfg(test)]
        // neither counts nor violates.
        let (loads, writes, violations) = sf(
            "#[cfg(test)]\nmod tests {\n    let rows = state.load_jsonl::<SupplyEntry>(StateFile::Supply)?;\n}\n",
        );
        assert_eq!((loads, writes), (0, 0));
        assert!(violations.is_empty(), "{violations:?}");
    }

    /// The projection vocabulary comes from the owner impl in model.rs
    /// source — `pub fn` names inside `impl<'a> SupplyFold<'a>`, nothing
    /// outside it.
    #[test]
    fn supply_fold_projection_extraction_reads_the_owner_impl() {
        let src = "\
pub struct SupplyFold;\n\
impl<'a> SupplyFold<'a> {\n\
    pub fn collapse(entries: &[SupplyEntry]) -> Self { todo!() }\n\
    fn fold(&self) {}\n\
    pub fn latest_settlements(&self) -> usize { 0 }\n\
}\n\
impl Other {\n\
    pub fn not_a_projection(&self) {}\n\
}\n";
        let names = extract_supply_fold_projections(src);
        assert_eq!(
            names,
            ["collapse", "latest_settlements"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<String>>()
        );
    }

    /// The real tree passes the supply-fold lint: every production load
    /// is marked with a live projection (or a reasoned exemption), and
    /// the floor guards see the expected population.
    #[test]
    fn supply_fold_owner_passes_on_the_real_tree() {
        supply_fold_owner().unwrap();
    }

    /// The declaration matcher against the REAL spec grammar: both
    /// styles in use today — inline `#r("id")` and crate-structure.typ's
    /// multi-line `#r(\n  "id",\n)` (with and without the trailing
    /// comma) — land in the universe; a `//`-commented `#r()` mention is
    /// tolerated without parsing; and an `#r(` token in a style the
    /// matcher does NOT parse is a loud violation rather than a silent
    /// universe shrink. The original inline-only matcher missed all 19
    /// multi-line declarations (universe 529 of 548) and false-redded a
    /// citation of the live `ts.mock.scheduler-outcome+2`.
    #[test]
    fn rule_declarations_parsed_in_both_styles_and_parity_enforced() {
        let src = "\
#r(\"sched.dispatch.fod-substitute+4\")[inline body]
#r(
  \"ts.mock.scheduler-outcome+2\",
)[multi-line body, trailing comma]
#r(
  \"common.bootstrap\"
)[multi-line body, no trailing comma]
// the dict below holds ALL trailing per-crate content (#r() markers,
";
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(src, "docs/spec/x.typ", &mut universe, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(parsed, 3, "all three declaration styles parsed");
        let expected: BTreeSet<String> = [
            "sched.dispatch.fod-substitute+4",
            "ts.mock.scheduler-outcome+2",
            "common.bootstrap",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(universe, expected);

        // A third style the matcher does not parse (a comment between
        // the paren and the id) must red the lint, not shrink the
        // universe.
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(
            "#r(\n  // why\n  \"sched.unparsed.style\",\n)[body]\n",
            "docs/spec/y.typ",
            &mut universe,
            &mut violations,
        );
        assert_eq!(parsed, 0);
        assert!(universe.is_empty(), "{universe:?}");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].starts_with("docs/spec/y.typ:1:")
                && violations[0].contains("extend `rule_declaration_regex`"),
            "{violations:?}"
        );
    }

    /// The token-parity accounting is exact, not just violation-free:
    /// over a corpus mixing every tolerated shape, parsed declarations
    /// plus commented mentions account for EVERY `#r(` token, so the
    /// universe equals the grep-derived declaration inventory by
    /// construction. (The same accounting runs inside the lint over the
    /// REAL docs/spec tree on every `xtask lint` / xtask-lint CI run —
    /// 549 declarations + 1 commented mention today — which is where
    /// the real-tree parity is enforced; the nextest sandbox stages no
    /// docs/spec, so a unit test cannot see the real tree without going
    /// vacuous.)
    #[test]
    fn token_parity_accounts_for_every_declaration_token() {
        let src = "\
#r(\"a.b+1\")[x]
// prose mention: #r() markers
#r(
  \"c.d\",
)[y]
#r(\"a.b+1\")[duplicate id, second token]
/* a block-commented mention: #r() */
";
        let tokens = src.matches("#r(").count();
        let commented = 2usize;
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(src, "docs/spec/z.typ", &mut universe, &mut violations);
        assert_eq!(tokens, 5);
        // The full accounting: every token is parsed, commented, or a
        // violation — nothing falls through silently.
        assert_eq!(parsed + commented + violations.len(), tokens);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(parsed, 3, "3 declaration tokens (one id declared twice)");
        assert_eq!(universe.len(), 2);
    }

    /// Commented-out declarations stay OUT of the universe, in every
    /// comment style Typst has — `//` line comments, `/* */` block
    /// comments (same-line and spanning the declaration), and nested
    /// blocks. A dead declaration that still parsed would keep its id
    /// alive in the universe and silently green-light prose citations of
    /// a rule the spec no longer declares — so the comment screen runs
    /// BEFORE the parse attempt, and parseability cannot resurrect dead
    /// text. Both directions are pinned: the dead ids are absent (and a
    /// citation of one is flagged), while a live declaration in the same
    /// file — including one followed by a trailing comment, and one on a
    /// line after a closed block comment — is still admitted and
    /// satisfies its citation.
    #[test]
    fn commented_out_declarations_never_enter_the_universe() {
        let src = "\
// #r(\"dead.line+3\")[stale copy kept after a bump]
/* #r(\"dead.block+1\")[same-line block comment] */
/*
#r(\"dead.spanned+2\")[multi-line block comment]
/* #r(\"dead.nested+4\")[nested block comment] */
still inside the outer block
*/
#r(\"live.rule+1\")[a live declaration] // trailing comment
/* closed */ #r(\"live.after-block+1\")[after a closed block]
";
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(src, "docs/spec/c.typ", &mut universe, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");
        let expected: BTreeSet<String> = ["live.rule+1", "live.after-block+1"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(universe, expected, "only the live declarations are rules");
        assert_eq!(parsed, 2);
        // Accounting: 6 tokens = 2 parsed + 4 commented + 0 violations.
        assert_eq!(src.matches("#r(").count(), 6);

        // Citation direction: a prose pin of a commented-out rule is now
        // a dangling citation; pins of the live rules pass.
        let prose = "\
dead: `dead.line+3` and `dead.block+1` and `dead.spanned+2`.
live: `live.rule+1` and `live.after-block+1`.";
        let mut cite_violations = Vec::new();
        let citations =
            check_spec_rule_citations(&universe, "docs/dev/c.md", prose, &mut cite_violations);
        assert_eq!(citations, 5);
        assert_eq!(cite_violations.len(), 3, "{cite_violations:?}");
        for (violation, dead_id) in
            cite_violations
                .iter()
                .zip(["dead.line+3", "dead.block+1", "dead.spanned+2"])
        {
            assert!(violation.contains(dead_id), "{violation}");
        }
    }

    /// The comment screen mirrors the Typst lexer's precedence, so prose
    /// shapes that LOOK like comment introducers do not screen (or
    /// swallow) live declarations: a `//` that is part of a URL (always
    /// preceded by `:`) is not a line comment, and a `/*` inside a
    /// backtick raw span (the spec's `path/*.glob` spellings) does not
    /// open a phantom block comment. The URL case is also pinned in the
    /// violation direction: an unparseable token after a URL is a LOUD
    /// violation, not a silently tolerated mention — the whole-line
    /// `contains(\"//\")` screen this replaces swallowed it.
    #[test]
    fn url_and_raw_span_slashes_are_not_comment_introducers() {
        // Live declarations after URL `//` and after a raw-span glob.
        let src = "\
see https://example.org for context #r(\"live.after-url+1\")[x]
generated from `proto/*.proto` files
#r(\"live.after-glob+1\")[the glob's `/*` must not swallow this]
";
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(src, "docs/spec/u.typ", &mut universe, &mut violations);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(parsed, 2);
        let expected: BTreeSet<String> = ["live.after-url+1", "live.after-glob+1"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(universe, expected);

        // Violation direction: a URL on the line must not silently
        // excuse an unparseable token.
        let mut universe = BTreeSet::new();
        let mut violations = Vec::new();
        let parsed = collect_spec_rules(
            "see https://example.org #r(unparsed mention)\n",
            "docs/spec/v.typ",
            &mut universe,
            &mut violations,
        );
        assert_eq!(parsed, 0);
        assert!(universe.is_empty(), "{universe:?}");
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].starts_with("docs/spec/v.typ:1:")
                && violations[0].contains("extend `rule_declaration_regex`"),
            "{violations:?}"
        );
    }

    /// Both directions of the citation check on synthetic prose: a
    /// pinned citation the universe declares passes, one it does not
    /// declare is flagged with file:line, and out-of-domain spans (bare
    /// ids, globs, dotless tokens, unbackticked ids) are not citations
    /// at all.
    #[test]
    fn versioned_citations_checked_against_the_universe() {
        let universe: BTreeSet<String> = ["sched.dispatch.fod-substitute+4"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let src = "\
pinned and live: `sched.dispatch.fod-substitute+4` is fine.
pinned and dangling: `gw.build.per-tenant-policy+2` was bumped away.
out of domain: bare `sched.merge.force-build-roots`, glob
`sched.merge.substitute-*`, dotless `foo+1`, and an unbackticked
sched.dispatch.fod-substitute+3 are not citations.
two on one line: `sched.dispatch.fod-substitute+4` `sched.dispatch.fod-substitute+9`.";
        let mut violations = Vec::new();
        let citations = check_spec_rule_citations(&universe, "docs/dev/x.md", src, &mut violations);
        assert_eq!(citations, 4, "exactly the four pinned spans are in domain");
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(
            violations[0].starts_with("docs/dev/x.md:2:")
                && violations[0].contains("gw.build.per-tenant-policy+2"),
            "{violations:?}"
        );
        assert!(
            violations[1].starts_with("docs/dev/x.md:6:")
                && violations[1].contains("sched.dispatch.fod-substitute+9"),
            "{violations:?}"
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

    // ── bounded-io ─────────────────────────────────────────────────

    /// Run [`check_bounded_io_file`] over a synthetic source; returns
    /// (needles, violations).
    fn bio(src: &str) -> (usize, Vec<String>) {
        let mut violations = Vec::new();
        let needles = check_bounded_io_file(src, "synthetic.rs", &mut violations);
        (needles, violations)
    }

    #[test]
    fn bounded_io_unmarked_send_fails_marked_passes() {
        // Bare dispatch: flagged with file:line and the needle shape.
        let (needles, v) = bio("let resp = client.get(url).send().await?;\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");
        assert!(
            v[0].contains("synthetic.rs:1") && v[0].contains(".send()"),
            "{v:?}"
        );

        // Trailing marker on the needle line blesses it.
        let (needles, v) = bio(
            "let resp = client.get(url).send().await?; // bounded-io: request timeout spans body\n",
        );
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");

        // Marker comment above the statement blesses a multi-line chain.
        let (needles, v) = bio("let resp = client\n\
             \x20   .get(url)\n\
             \x20   // bounded-io: request timeout spans body\n\
             \x20   .send()\n\
             \x20   .await?;\n");
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");

        // A channel-style send (argument present) is not in the alphabet.
        let (needles, v) = bio("tx.send(value).await?;\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
    }

    /// `.read_to_vec(` — the dwarfs whole-member buffering read — is in
    /// the needle alphabet: unmarked it is flagged like any other
    /// external-IO call (the negative control reproduces the exact shape
    /// the image backend shipped unbounded), marked it passes, and prose
    /// mentions in comments neither match nor mask.
    #[test]
    fn bounded_io_read_to_vec_is_a_needle() {
        // The historical unbounded shape, verbatim: flagged.
        let (needles, v) = bio("let bytes = file\n\
             \x20   .read_to_vec(&mut *archive)\n\
             \x20   .with_context(|| format!(\"read {rel}\"))?;\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");
        assert!(
            v[0].contains("synthetic.rs:2") && v[0].contains(".read_to_vec("),
            "{v:?}"
        );

        // Marked: blessed.
        let (needles, v) = bio("let bytes = file\n\
             \x20   // bounded-io: declared-size pre-check + take(cap + 1) belt\n\
             \x20   .read_to_vec(&mut *archive)?;\n");
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");

        // A comment mentioning the call is prose, not a needle.
        let (needles, v) = bio("// read_to_vec( is unbounded; see the backend\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
    }

    #[test]
    fn bounded_io_collect_needs_await_adjacency() {
        // ByteStream collect (awaited): a needle.
        let (needles, v) = bio("let bytes = resp\n\
             \x20   .body\n\
             \x20   .collect()\n\
             \x20   .await?;\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");

        // Iterator collect (never awaited): not a needle, and a marker
        // on it is stale.
        let (needles, v) = bio("let xs: Vec<_> = ys.iter().map(f).collect();\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
        let (_, v) = bio("// bounded-io: stale — annotates an iterator collect\n\
             let xs: Vec<_> = ys.iter().map(f).collect();\n");
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("stale"), "{v:?}");
    }

    #[test]
    fn bounded_io_statement_boundary_cuts_a_trailing_marker() {
        // A marker trailing the PREVIOUS statement (its code part ends
        // with `;`) must not bless the next statement's needle — the
        // boundary is checked on the comment-stripped code BEFORE the
        // trailing marker is honored.
        let (needles, v) = bio(
            "let a = setup(); // bounded-io: belongs to this statement only\n\
             let resp = client\n\
             \x20   .get(url)\n\
             \x20   .send()\n\
             \x20   .await?;\n",
        );
        assert_eq!(needles, 1);
        assert_eq!(v.len(), 2, "unblessed needle AND stale marker: {v:?}");
        assert!(v.iter().any(|m| m.contains("without an adjacent")), "{v:?}");
        assert!(v.iter().any(|m| m.contains("stale")), "{v:?}");

        // The same trailing marker IS valid when its own statement has
        // the needle.
        let (needles, v) = bio("let r = c.send().await?; // bounded-io: deadline upstream\n");
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");
    }

    #[test]
    fn bounded_io_test_region_is_skipped() {
        // Needles after `#[cfg(test)]` are loopback fixtures, not
        // production IO — neither flagged nor counted.
        let (needles, v) = bio("fn prod() {} \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   async fn t() { client.get(u).send().await; }\n\
             }\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
    }

    #[test]
    fn bounded_io_doc_comment_mention_is_prose() {
        // `///` prose describing the convention is neither a marker nor
        // stale.
        let (needles, v) = bio(
            "/// Sites carry `// bounded-io: <bound>` markers, enforced\n\
             /// by xtask lint.\n\
             pub struct Client;\n",
        );
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
    }

    /// The realistic evasion the scanned modules are saturated with: the
    /// `//` of a URL **string literal**. A naive `split("//")` strip read
    /// everything after `"https:` as a comment, so an inline-URL one-liner
    /// lost its `.send()` needle (new unbounded IO landed unmarked) and a
    /// URL-bearing line lost its `;` terminator (one statement's marker
    /// blessed its neighbor). Both directions must hold against the
    /// string-aware lexer.
    #[test]
    fn bounded_io_url_literals_neither_mask_needles_nor_eat_boundaries() {
        // Inline-URL dispatch: the needle must still be seen and flagged.
        let (needles, v) =
            bio("let resp = client.get(\"https://cache.example.org/x\").send().await?;\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");
        assert!(v[0].contains(".send()"), "{v:?}");

        // A URL-bearing terminated statement between a marker and the
        // next statement's needle is a BOUNDARY: the marker must not
        // bless across it (and goes stale, having no needle of its own).
        // This is the `.with_context(|| format!("s3://…"))?;` shape the
        // archive backends use on almost every call.
        let (needles, v) = bio("// bounded-io: belongs to the statement below only\n\
             let base = parse(\"s3://{}/{key}\", bucket);\n\
             let resp = client.get(base).send().await?;\n");
        assert_eq!(needles, 1);
        assert_eq!(v.len(), 2, "unblessed needle AND stale marker: {v:?}");
        assert!(v.iter().any(|m| m.contains("without an adjacent")), "{v:?}");
        assert!(v.iter().any(|m| m.contains("stale")), "{v:?}");

        // Conversely: a URL inside a CONTINUATION line of one statement
        // (no real terminator outside the string) does not bound the
        // walk — the marker above still blesses its own needle, and
        // braces inside the literal are not statement structure.
        let (needles, v) = bio(
            "// bounded-io: dispatch only; body streamed under its own cap\n\
             let resp = client\n\
             \x20   .get_object(format!(\"s3://{}/{key};{}\", bucket, extra))\n\
             \x20   .send()\n\
             \x20   .await?;\n",
        );
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");

        // A needle token inside a string literal is a log message, not an
        // IO call: no marker demanded, and a marker annotating only it is
        // stale.
        let (needles, v) = bio("tracing::warn!(\"retrying .send() after backoff\");\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
        let (_, v) = bio(
            "// bounded-io: stale — the needle below is inside a string\n\
             tracing::warn!(\"retrying .send() after backoff\");\n",
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("stale"), "{v:?}");

        // A marker token inside a string literal is code, not a blessing:
        // the needle next to it still demands a real marker.
        let (needles, v) = bio("let tag = \"// bounded-io: not a real marker\";\n\
             let resp = client.get(url).send().await?;\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");
    }

    /// `.chunk()` is in the alphabet: the per-chunk accumulation loop is
    /// the body-buffering shape one step removed (the narinfo cap rider's
    /// loop), so it must state its bound like any other body read.
    #[test]
    fn bounded_io_chunk_loop_is_a_needle() {
        let (needles, v) = bio("while let Some(chunk) = resp.chunk().await? {\n\
             \x20   bytes.extend_from_slice(&chunk);\n\
             }\n");
        assert_eq!((needles, v.len()), (1, 1), "{v:?}");
        assert!(v[0].contains(".chunk()"), "{v:?}");

        // The real nixcache shape: marker above the loop statement.
        let (needles, v) = bio(
            "// bounded-io: size-capped by the running MAX_NARINFO_BYTES check\n\
             // below; time-bounded by the client-wide request timeout.\n\
             while let Some(chunk) = resp\n\
             \x20   .chunk()\n\
             \x20   .await?\n\
             {\n\
             \x20   bytes.extend_from_slice(&chunk);\n\
             }\n",
        );
        assert_eq!((needles, v.len()), (1, 0), "{v:?}");

        // Iterator `.chunks(n)` takes an argument and never matches.
        let (needles, v) = bio("for chunk in xs.chunks(500) { upload(chunk); }\n");
        assert_eq!((needles, v.len()), (0, 0), "{v:?}");
    }

    // ── shared line lexer ──────────────────────────────────────────

    /// The string-aware lexer behind every marker lint: `//` inside
    /// string/char literals is code; real comments are found behind any
    /// literal shape; literal interiors are blanked out of the structural
    /// view.
    #[test]
    fn line_lexer_distinguishes_strings_chars_and_comments() {
        // Plain comment.
        assert_eq!(strip_line_comment("let x = 1; // c"), "let x = 1; ");
        // URL literal: no comment on the line at all.
        let url = ".with_context(|| format!(\"s3://{}/{key}\", self.bucket))?;";
        assert_eq!(strip_line_comment(url), url);
        assert_eq!(line_comment(url), None);
        // The structural view keeps the real `;` but blanks the braces
        // inside the literal.
        let blanked = code_blanked(url);
        assert!(blanked.ends_with("?;"), "{blanked:?}");
        assert!(!blanked.contains('{'), "{blanked:?}");
        // Real comment after a URL literal.
        assert_eq!(
            strip_line_comment("let u = \"https://x\"; // note"),
            "let u = \"https://x\"; "
        );
        // Escaped quote inside a string does not end it.
        assert_eq!(
            strip_line_comment("let s = \"a\\\"b//c\"; // d"),
            "let s = \"a\\\"b//c\"; "
        );
        // A quote CHAR literal does not open a string.
        assert_eq!(
            strip_line_comment("if c == '\"' { x(); } // e"),
            "if c == '\"' { x(); } "
        );
        // …and a brace char literal is not statement structure.
        assert!(!code_blanked("let open = '{';").contains('{'));
        // Raw strings honor their hash fences.
        assert_eq!(
            strip_line_comment("let r = r#\"// not a comment\"#; // f"),
            "let r = r#\"// not a comment\"#; "
        );
        // Byte strings too.
        assert_eq!(
            strip_line_comment("let b = b\"//\"; // h"),
            "let b = b\"//\"; "
        );
        // Lifetimes are not char literals: the comment is still found.
        assert_eq!(
            strip_line_comment("fn f<'a>(x: &'a str) {} // g"),
            "fn f<'a>(x: &'a str) {} "
        );
        // An unterminated string (a literal spanning lines) swallows the
        // rest of the line: no comment, interior blanked.
        assert_eq!(line_comment("let s = \"abc // not"), None);
        assert!(!code_blanked("let s = \"abc ; {").contains(';'));
        // Identifiers starting with r/b are not literal prefixes.
        assert_eq!(
            strip_line_comment("let radius = r * 2; // c"),
            "let radius = r * 2; "
        );
    }

    /// The bookkeeping lint rides the same lexer: a marker token inside a
    /// string is code (neither blesses nor goes stale), and a URL literal
    /// before a real trailing marker does not hide it.
    #[test]
    fn bookkeeping_marker_detection_is_string_aware() {
        assert!(has_trailing_marker(
            "let url = \"http://x\"; // Bookkeeping lookup: audit view"
        ));
        assert!(!has_trailing_marker(
            "let s = \"// Bookkeeping lookup: not a marker\";"
        ));
    }

    // ── structured-attr reads ──────────────────────────────────────

    /// Run [`check_structured_attr_file`] over a synthetic source.
    /// Fixture lines are assembled by CONCATENATION so this file never
    /// contains a contiguous needle — the real lint scans xtask/src too.
    fn sar(src: &str) -> Vec<String> {
        let needles = structured_attr_needles().unwrap();
        let mut violations = Vec::new();
        check_structured_attr_file(src, "synthetic.rs", &needles, &mut violations);
        violations
    }

    /// Every raw-read shape in the alphabet trips: the historical
    /// literal-keyed reads AND the const-keyed style the canonical
    /// module's own `pub const` names made the house standard — the
    /// realistic way the next bypassing read gets written in a crate
    /// that already imports rio-nix.
    #[test]
    fn structured_attr_alphabet_catches_literal_and_const_keyed_reads() {
        let attr = "requiredSystemFeatures";
        let attr2 = "impureEnvVars";
        let konst = "REQUIRED_SYSTEM_FEATURES_ATTR";
        let konst2 = "IMPURE_ENV_VARS_ATTR";
        let cases = [
            format!("let f = env.get({:?});\n", attr),
            format!("let v = payload[{:?}].clone();\n", attr2),
            format!("let f = env.get({konst});\n"),
            format!("let f = env.get(structured_attrs::{konst2});\n"),
            format!("let v = payload[{konst}].as_array();\n"),
            format!("let v = payload[structured_attrs::{konst2}];\n"),
        ];
        for case in &cases {
            let v = sar(case);
            assert_eq!(v.len(), 1, "must trip: {case:?} -> {v:?}");
        }

        // Conforming shapes stay clean: the canonical rule and adapters
        // take the key as a parameter, comments may cite the hazard, and
        // the class const's own definition list is not a read.
        let clean = format!(
            "let names = string_list_attr(&view, structured_attrs::{konst});\n\
             // explaining the hazard: env.get({:?}) would be blind\n\
             pub const STRING_LIST_USER_ATTRS: [&str; 2] = [{konst}, {konst2}];\n",
            attr
        );
        assert_eq!(sar(&clean), Vec::<String>::new());
    }

    /// The needle alphabet's totality guard: the const-name registry must
    /// cover the class exactly (values are the real consts, so a rename
    /// refuses to compile; this pins the count side).
    #[test]
    fn structured_attr_const_registry_is_total() {
        use rio_nix::derivation::structured_attrs::STRING_LIST_USER_ATTRS;
        assert_eq!(STRING_LIST_ATTR_CONSTS.len(), STRING_LIST_USER_ATTRS.len());
        for key in STRING_LIST_USER_ATTRS {
            assert_eq!(
                STRING_LIST_ATTR_CONSTS
                    .iter()
                    .filter(|(_, value)| *value == key)
                    .count(),
                1,
                "{key} must have exactly one const-name registration"
            );
        }
        // 2 literal + 4 const-keyed shapes per attr.
        assert_eq!(
            structured_attr_needles().unwrap().len(),
            STRING_LIST_USER_ATTRS.len() * 6
        );
    }

    // As with the other file-walking lints, `bounded_io` itself (root
    // existence, floor guards, the real-tree scan) runs in the
    // `xtask-lint` flake check; the per-file scanner and its adjacency
    // semantics are fully covered here.
}
