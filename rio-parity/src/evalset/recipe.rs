//! Hydra-faithful eval recipe.
//!
//! Reconstructs the evaluator invocation Hydra used for an evaluation,
//! so a local nix-eval-jobs run reproduces Hydra's drvPaths bit-for-bit
//! (verified against the recorded Hydra evaluation 1824219): recover
//! revCount/shortRev from a sampled build's release name, render the
//! `nixpkgs` argument attrset, map the jobset's other declared inputs
//! onto `--arg` pairs, derive the pinned-source tarball URL, and build
//! the nix-eval-jobs argv for full or scoped (selection-expression)
//! runs.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::hydra::HydraJobset;

/// revCount/shortRev recovered from a versionSuffix-carrying build's
/// `nixname`/`releasename` (e.g. `nixos-26.05pre975402.68d8aa3d661f`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSuffix {
    pub rev_count: u64,
    pub short_rev: String,
}

/// Recover revCount/shortRev from a build's `nixname`/`releasename` by
/// matching the nixpkgs versionSuffix convention `pre<revCount>.<shortRev>`
/// — the regex `pre(\d+)\.([0-9a-f]{7,40})`. Names without a version
/// suffix (plain packages, VM tests) yield `None`.
pub fn recover_version_suffix(name: &str) -> Option<VersionSuffix> {
    use std::sync::LazyLock;

    static VERSION_SUFFIX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"pre(\d+)\.([0-9a-f]{7,40})").expect("static regex"));

    let caps = VERSION_SUFFIX.captures(name)?;
    Some(VersionSuffix {
        rev_count: caps[1].parse().ok()?,
        short_rev: caps[2].to_string(),
    })
}

/// The `--arg nixpkgs` attrset passed to the jobset entry point, in the
/// exact form whose evaluation reproduces Hydra's drvPaths bit-for-bit
/// (including jobs such as nixos.channel and nixpkgs.tarball that embed
/// rev/shortRev/revCount in their outputs).
#[derive(Debug, Clone)]
pub struct NixpkgsArg {
    /// Store path of the unpacked tarball added as `--name source`.
    pub source_store_path: String,
    /// Full 40-char revision (required: `system.nixos.revision`,
    /// `.git-revision` files consume it).
    pub rev: String,
    pub short_rev: String,
    pub rev_count: u64,
}

impl NixpkgsArg {
    /// Render the attrset as a Nix expression.
    ///
    /// The fields are interpolated verbatim (this function cannot
    /// fail), so the caller must guarantee they are inert in a Nix
    /// string/expression context: `rev` and `short_rev` must be bare
    /// lowercase hex (as Hydra's revision field and
    /// [`recover_version_suffix`] produce), and `source_store_path`
    /// must be a plain `/nix/store/...` path with no quotes, spaces,
    /// `\`, or `${`. [`selection_expr`] re-checks the hex shape before
    /// embedding the rendering into a generated file.
    pub fn to_nix_expr(&self) -> String {
        format!(
            "{{ outPath = builtins.storePath {}; rev = \"{}\"; shortRev = \"{}\"; revCount = {}; }}",
            self.source_store_path, self.rev, self.short_rev, self.rev_count
        )
    }
}

/// Map the jobset's declared inputs (minus the nixexprinput itself)
/// onto `--arg <name> <nix literal>` pairs. Only the input types the
/// release expressions actually consume are supported; anything else
/// (a second git input, path inputs, …) is an error so we never
/// silently evaluate with a missing argument. String values are escaped
/// for Nix string syntax (`\`, `"`, and the `${` interpolation opener);
/// `nix` values are already Nix expressions and pass through verbatim.
pub fn jobset_extra_args(jobset: &HydraJobset) -> anyhow::Result<Vec<(String, String)>> {
    let expr_input = jobset.nixexprinput.as_deref().unwrap_or_default();
    let mut out = Vec::new();
    for (name, input) in &jobset.inputs {
        if name == expr_input {
            continue;
        }
        let value = input.value.clone().unwrap_or_default();
        let literal = match input.input_type.as_str() {
            "boolean" => match value.as_str() {
                "true" | "false" => value,
                other => anyhow::bail!("jobset input {name}: boolean with value {other:?}"),
            },
            "string" => format!(
                "\"{}\"",
                value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace("${", "\\${")
            ),
            "nix" => value,
            other => anyhow::bail!(
                "jobset input {name} has unsupported type {other:?}; \
                 only the nixexprinput plus boolean/string/nix inputs are supported"
            ),
        };
        out.push((name.clone(), literal));
    }
    Ok(out)
}

/// GitHub archive tarball URL for the pinned revision. The unpacked
/// archive is NAR-identical to Hydra's git export of the same revision
/// (verified on eval 1824219), so no git checkout is needed.
pub fn tarball_url(git_uri: &str, rev: &str) -> anyhow::Result<String> {
    let base = git_uri.trim_end_matches('/').trim_end_matches(".git");
    anyhow::ensure!(
        base.starts_with("https://github.com/"),
        "only github.com nixpkgs inputs are supported (got {git_uri}); \
         pass --source-tarball-url to override"
    );
    Ok(format!("{base}/archive/{rev}.tar.gz"))
}

/// nix-eval-jobs argv (after the program name) for a FULL-scope eval.
#[allow(clippy::too_many_arguments)]
pub fn evaluator_argv_full(
    entry_point: &Path,
    source_tree: &Path,
    nixpkgs: &NixpkgsArg,
    extra_args: &[(String, String)],
    gc_roots_dir: &Path,
    workers: u32,
    max_memory_mb: u64,
) -> Vec<String> {
    let mut argv = vec![
        entry_point.display().to_string(),
        "-I".into(),
        format!("nixpkgs={}", source_tree.display()),
        "--arg".into(),
        "nixpkgs".into(),
        nixpkgs.to_nix_expr(),
    ];
    for (name, literal) in extra_args {
        argv.push("--arg".into());
        argv.push(name.clone());
        argv.push(literal.clone());
    }
    argv.extend([
        "--gc-roots-dir".into(),
        gc_roots_dir.display().to_string(),
        "--workers".into(),
        workers.to_string(),
        "--max-memory-size".into(),
        max_memory_mb.to_string(),
        "--meta".into(),
        "--constituents".into(),
        "--force-recurse".into(),
    ]);
    argv
}

/// nix-eval-jobs argv (after the program name) for a SCOPED eval over
/// a generated selection expression (the expression already binds the
/// jobset arguments, so no `--arg` here).
pub fn evaluator_argv_scoped(
    selection_file: &Path,
    source_tree: &Path,
    gc_roots_dir: &Path,
    workers: u32,
    max_memory_mb: u64,
) -> Vec<String> {
    vec![
        selection_file.display().to_string(),
        "-I".into(),
        format!("nixpkgs={}", source_tree.display()),
        "--gc-roots-dir".into(),
        gc_roots_dir.display().to_string(),
        "--workers".into(),
        workers.to_string(),
        "--max-memory-size".into(),
        max_memory_mb.to_string(),
        "--meta".into(),
    ]
}

/// Generate the scoped selection expression: one shared evaluation of
/// the jobset entry point, exposing exactly the requested jobs as a
/// flat attrset keyed by Hydra job name.
///
/// Job names and the nixpkgs rev/shortRev are interpolated into the
/// generated Nix source, so they are validated first: each `.`-separated
/// job component must stay within `[A-Za-z0-9_+-]` (every Hydra job name
/// observed in the nixos jobsets does) and the revisions must be bare
/// lowercase hex, otherwise the generated expression could mean
/// something other than the requested selection.
pub fn selection_expr(
    entry_point: &Path,
    nixpkgs: &NixpkgsArg,
    extra_args: &[(String, String)],
    jobs: &[String],
) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let is_bare_hex = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    };
    anyhow::ensure!(
        is_bare_hex(&nixpkgs.rev),
        "nixpkgs rev {:?} is not bare lowercase hex",
        nixpkgs.rev
    );
    anyhow::ensure!(
        is_bare_hex(&nixpkgs.short_rev),
        "nixpkgs shortRev {:?} is not bare lowercase hex",
        nixpkgs.short_rev
    );

    let mut expr = String::new();
    writeln!(
        expr,
        "# Generated by rio-parity eval: scoped selection over the Hydra jobset entry point."
    )?;
    writeln!(expr, "let")?;
    writeln!(expr, "  release = import {} {{", entry_point.display())?;
    writeln!(expr, "    nixpkgs = {};", nixpkgs.to_nix_expr())?;
    for (name, literal) in extra_args {
        writeln!(expr, "    {name} = {literal};")?;
    }
    writeln!(expr, "  }};")?;
    writeln!(
        expr,
        "  getPath = path: set: builtins.foldl' (s: a: builtins.getAttr a s) set path;"
    )?;
    writeln!(expr, "in")?;
    writeln!(expr, "{{")?;
    for job in jobs {
        let components: Vec<&str> = job.split('.').collect();
        anyhow::ensure!(
            !components.is_empty() && components.iter().all(|c| !c.is_empty()),
            "job name {job:?} has an empty attr-path component"
        );
        for component in &components {
            anyhow::ensure!(
                component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')),
                "job name {job:?} component {component:?} contains characters outside \
                 [A-Za-z0-9_+-]; such attribute names cannot be embedded in the generated \
                 selection expression"
            );
        }
        let path_list = components
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(expr, "  \"{job}\" = getPath [ {path_list} ] release;")?;
    }
    writeln!(expr, "}}")?;
    Ok(expr)
}

/// `(short_rev, full_rev)` consistency check used after recovery: it
/// rejects a `pre<digits>.<hex>` match that came from a package's own
/// version string instead of the nixpkgs versionSuffix, since such a
/// false positive recovers hex that is not a prefix of the evaluation's
/// real nixpkgs revision.
pub fn short_rev_matches(short_rev: &str, full_rev: &str) -> bool {
    !short_rev.is_empty() && full_rev.starts_with(short_rev)
}

// ── Source-prep subprocess helpers ───────────────────────────────────
//
// Network/nix-dependent: the offline unit suite never calls these; they
// are exercised end-to-end when an eval set is actually built (and by
// the #[ignore]d eval_e2e integration test).

/// Download the pinned-revision source tarball to `dest`. The whole
/// body is buffered in memory (a nixpkgs GitHub archive is ~50 MB);
/// non-2xx responses are errors carrying a clipped body snippet.
pub async fn download_tarball(url: &str, user_agent: &str, dest: &Path) -> anyhow::Result<()> {
    tracing::info!(%url, "downloading nixpkgs tarball");
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("build tarball HTTP client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "GET {url}: HTTP {status}: {}",
            crate::body_snippet(&resp.text().await.unwrap_or_default())
        );
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("download {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    tracing::info!(bytes = bytes.len(), dest = %dest.display(), "tarball downloaded");
    Ok(())
}

/// Unpack the tarball into `unpack_dir` and return the single
/// top-level directory it contains (`nixpkgs-<rev>` for a GitHub
/// archive tarball). More than one top-level directory — e.g. a reused
/// scratch dir holding a previous revision's unpack — is an error
/// rather than a guess.
pub async fn unpack_tarball(tarball: &Path, unpack_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(unpack_dir)
        .with_context(|| format!("create {}", unpack_dir.display()))?;
    let out = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(unpack_dir)
        // Cancelling the caller (e.g. a failure elsewhere in the build)
        // drops this future; kill_on_drop keeps that from orphaning a
        // still-running tar.
        .kill_on_drop(true)
        .output()
        .await
        .context("spawn tar -xzf")?;
    anyhow::ensure!(
        out.status.success(),
        "tar -xzf {} exited with {}: {}",
        tarball.display(),
        out.status,
        crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
    );
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(unpack_dir)
        .with_context(|| format!("read {}", unpack_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    anyhow::ensure!(
        dirs.len() == 1,
        "expected exactly one unpacked top-level directory in {}, found {}",
        unpack_dir.display(),
        dirs.len()
    );
    Ok(dirs.remove(0))
}

/// `nix store add-path --name source <tree>` — adds the unpacked tree
/// to the local store under the SAME store-path name Hydra's git-input
/// export uses (`source`), which is what lets jobs that embed the
/// source path in their outputs (nixos.channel, nixpkgs.tarball)
/// reproduce Hydra's drvPaths bit-for-bit. Returns the store path.
pub async fn store_add_source(nix_bin: &str, tree: &Path) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(nix_bin)
        .args([
            "--extra-experimental-features",
            "nix-command",
            "store",
            "add-path",
            "--name",
            "source",
        ])
        .arg(tree)
        // Cancelling the caller drops this future; kill_on_drop keeps
        // that from orphaning a still-running nix process.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn {nix_bin} store add-path"))?;
    anyhow::ensure!(
        out.status.success(),
        "nix store add-path failed ({}): {}",
        out.status,
        crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
    );
    let path = String::from_utf8(out.stdout).context("nix store add-path output is not UTF-8")?;
    let path = path.trim().to_string();
    anyhow::ensure!(
        path.starts_with("/nix/store/") && path.ends_with("-source"),
        "unexpected nix store add-path output: {path:?}"
    );
    Ok(path)
}

/// `<bin> --version` first line (e.g. `nix (Nix) 2.34.7`), recorded in
/// the eval-set key so a tooling upgrade forks the key digest instead
/// of silently landing on an existing prefix.
pub async fn tool_version(bin: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(bin)
        .arg("--version")
        // kill_on_drop so a cancelled caller cannot orphan the child.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn {bin} --version"))?;
    anyhow::ensure!(
        out.status.success(),
        "{bin} --version exited with {}: {}",
        out.status,
        crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
    );
    let stdout = std::str::from_utf8(&out.stdout).context("--version output is not UTF-8")?;
    Ok(stdout.lines().next().unwrap_or_default().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real values from the recorded Hydra eval 1824219: REV is the
    // nixpkgs input revision; SOURCE_STORE_PATH is what
    // `nix store add-path --name source` produced for that revision's
    // unpacked tarball when the recipe was verified against Hydra.
    const REV: &str = "68d8aa3d661f0e6bd5862291b5bb263b2a6595c9";
    const SOURCE_STORE_PATH: &str = "/nix/store/gay80fqbpm2wakbsyd4in44gx0cwx3h5-source";

    #[test]
    fn recovers_revcount_and_shortrev_from_release_names() {
        // releasename of nixos.channel in the recorded eval 1824219.
        let v = recover_version_suffix("nixos-26.05pre975402.68d8aa3d661f").unwrap();
        assert_eq!(v.rev_count, 975402);
        assert_eq!(v.short_rev, "68d8aa3d661f");
        // nixname form works too.
        let v = recover_version_suffix("nixos-channel-26.05pre975402.68d8aa3d661f").unwrap();
        assert_eq!(v.rev_count, 975402);
        // Plain packages carry no version suffix.
        assert!(recover_version_suffix("hello-2.12.3").is_none());
        assert!(recover_version_suffix("vm-test-run-openssh").is_none());
        // The recovered shortRev must be a prefix of the full revision.
        assert!(REV.starts_with(&v.short_rev));
    }

    #[test]
    fn nixpkgs_arg_renders_the_hydra_faithful_attrset() {
        let arg = NixpkgsArg {
            source_store_path: SOURCE_STORE_PATH.to_string(),
            rev: REV.to_string(),
            short_rev: "68d8aa3d661f".to_string(),
            rev_count: 975402,
        };
        assert_eq!(
            arg.to_nix_expr(),
            "{ outPath = builtins.storePath /nix/store/gay80fqbpm2wakbsyd4in44gx0cwx3h5-source; \
             rev = \"68d8aa3d661f0e6bd5862291b5bb263b2a6595c9\"; \
             shortRev = \"68d8aa3d661f\"; revCount = 975402; }"
        );
    }

    #[test]
    fn jobset_inputs_become_extra_args() {
        let js: crate::hydra::HydraJobset = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/hydra/jobset-nixos-unstable.json"),
            )
            .unwrap(),
        )
        .unwrap();
        // nixpkgs is the nixexprinput → excluded; stableBranch boolean → bare literal.
        assert_eq!(
            jobset_extra_args(&js).unwrap(),
            vec![("stableBranch".to_string(), "false".to_string())]
        );
    }

    #[test]
    fn unsupported_jobset_input_types_are_rejected() {
        let js: crate::hydra::HydraJobset = serde_json::from_str(
            r#"{"project":"p","name":"j","nixexprinput":"src","nixexprpath":"release.nix",
                "inputs":{"src":{"type":"git","value":"https://example.com/x.git"},
                          "other":{"type":"git","value":"https://example.com/y.git"}}}"#,
        )
        .unwrap();
        let err = jobset_extra_args(&js).unwrap_err();
        assert!(err.to_string().contains("other"), "got: {err:#}");
    }

    #[test]
    fn jobset_string_and_nix_inputs_render_as_nix_literals() {
        // The recorded nixos-unstable jobset only exercises the boolean
        // branch, so cover the string-escaping and nix-passthrough
        // branches with a synthetic jobset: the string value carries a
        // quote, a backslash, and a `${` that must all come out inert.
        let js: crate::hydra::HydraJobset = serde_json::from_str(
            r#"{"project":"p","name":"j","nixexprinput":"src","nixexprpath":"release.nix",
                "inputs":{"src":{"type":"git","value":"https://example.com/x.git"},
                          "label":{"type":"string","value":"say \"hi\" \\ ${notInterpolated}"},
                          "overrides":{"type":"nix","value":"{ allowUnfree = true; }"}}}"#,
        )
        .unwrap();
        assert_eq!(
            jobset_extra_args(&js).unwrap(),
            vec![
                (
                    "label".to_string(),
                    "\"say \\\"hi\\\" \\\\ \\${notInterpolated}\"".to_string()
                ),
                (
                    "overrides".to_string(),
                    "{ allowUnfree = true; }".to_string()
                ),
            ]
        );
    }

    #[test]
    fn tarball_url_from_git_uri() {
        assert_eq!(
            tarball_url("https://github.com/nixos/nixpkgs.git", REV).unwrap(),
            format!("https://github.com/nixos/nixpkgs/archive/{REV}.tar.gz")
        );
        assert_eq!(
            tarball_url("https://github.com/NixOS/nixpkgs", REV).unwrap(),
            format!("https://github.com/NixOS/nixpkgs/archive/{REV}.tar.gz")
        );
        assert!(tarball_url("git://example.com/repo.git", REV).is_err());
    }

    fn sample_arg() -> NixpkgsArg {
        NixpkgsArg {
            source_store_path: SOURCE_STORE_PATH.to_string(),
            rev: REV.to_string(),
            short_rev: "68d8aa3d661f".to_string(),
            rev_count: 975402,
        }
    }

    #[test]
    fn full_scope_argv_is_hydra_faithful() {
        let argv = evaluator_argv_full(
            std::path::Path::new("/src/tree/nixos/release-combined.nix"),
            std::path::Path::new("/src/tree"),
            &sample_arg(),
            &[("stableBranch".to_string(), "false".to_string())],
            std::path::Path::new("/work/gcroots"),
            4,
            4096,
        );
        assert_eq!(
            argv,
            vec![
                "/src/tree/nixos/release-combined.nix",
                "-I",
                "nixpkgs=/src/tree",
                "--arg",
                "nixpkgs",
                "{ outPath = builtins.storePath /nix/store/gay80fqbpm2wakbsyd4in44gx0cwx3h5-source; rev = \"68d8aa3d661f0e6bd5862291b5bb263b2a6595c9\"; shortRev = \"68d8aa3d661f\"; revCount = 975402; }",
                "--arg",
                "stableBranch",
                "false",
                "--gc-roots-dir",
                "/work/gcroots",
                "--workers",
                "4",
                "--max-memory-size",
                "4096",
                "--meta",
                "--constituents",
                "--force-recurse",
            ]
        );
    }

    #[test]
    fn scoped_argv_points_at_generated_expression() {
        let argv = evaluator_argv_scoped(
            std::path::Path::new("/work/selection.nix"),
            std::path::Path::new("/src/tree"),
            std::path::Path::new("/work/gcroots"),
            4,
            4096,
        );
        assert_eq!(
            argv,
            vec![
                "/work/selection.nix",
                "-I",
                "nixpkgs=/src/tree",
                "--gc-roots-dir",
                "/work/gcroots",
                "--workers",
                "4",
                "--max-memory-size",
                "4096",
                "--meta",
            ]
        );
    }

    #[test]
    fn selection_expression_golden() {
        let expr = selection_expr(
            std::path::Path::new("/src/tree/nixos/release-combined.nix"),
            &sample_arg(),
            &[("stableBranch".to_string(), "false".to_string())],
            &[
                "nixpkgs.hello.x86_64-linux".to_string(),
                "nixos.tests.login.x86_64-linux".to_string(),
            ],
        )
        .unwrap();
        let expected = r#"# Generated by rio-parity eval: scoped selection over the Hydra jobset entry point.
let
  release = import /src/tree/nixos/release-combined.nix {
    nixpkgs = { outPath = builtins.storePath /nix/store/gay80fqbpm2wakbsyd4in44gx0cwx3h5-source; rev = "68d8aa3d661f0e6bd5862291b5bb263b2a6595c9"; shortRev = "68d8aa3d661f"; revCount = 975402; };
    stableBranch = false;
  };
  getPath = path: set: builtins.foldl' (s: a: builtins.getAttr a s) set path;
in
{
  "nixpkgs.hello.x86_64-linux" = getPath [ "nixpkgs" "hello" "x86_64-linux" ] release;
  "nixos.tests.login.x86_64-linux" = getPath [ "nixos" "tests" "login" "x86_64-linux" ] release;
}
"#;
        assert_eq!(expr, expected);
    }

    #[test]
    fn selection_expression_rejects_empty_attr_component() {
        let err = selection_expr(
            std::path::Path::new("/e.nix"),
            &sample_arg(),
            &[],
            &["nixpkgs..hello".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("nixpkgs..hello"));
    }

    #[test]
    fn selection_expression_rejects_unsupported_component_characters() {
        // A component outside [A-Za-z0-9_+-] (here a quote) could break
        // out of the generated attribute-path strings.
        let err = selection_expr(
            std::path::Path::new("/e.nix"),
            &sample_arg(),
            &[],
            &["nixpkgs.\"weird attr\".x86_64-linux".to_string()],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("characters outside"),
            "got: {err:#}"
        );

        // Non-hex rev/shortRev never come from a real Hydra eval; refuse
        // to embed them rather than emit a corrupted expression.
        let bad_rev = NixpkgsArg {
            rev: "68d8aa3d\" ++ injected".to_string(),
            ..sample_arg()
        };
        let err = selection_expr(
            std::path::Path::new("/e.nix"),
            &bad_rev,
            &[],
            &["nixpkgs.hello.x86_64-linux".to_string()],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not bare lowercase hex"),
            "got: {err:#}"
        );
    }

    #[test]
    fn short_rev_match_guards_against_foreign_version_suffixes() {
        // Recovered from the real nixpkgs versionSuffix → prefix of the
        // evaluation's full revision.
        assert!(short_rev_matches("68d8aa3d661f", REV));
        // Recovered from some package's own pre<digits>.<hex> version →
        // unrelated hex, must be rejected by the caller.
        assert!(!short_rev_matches("0f33ab1aaaaa", REV));
        assert!(!short_rev_matches("", REV));
    }
}
