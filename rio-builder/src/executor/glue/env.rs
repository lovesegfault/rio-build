//! Builder environment materialization and placeholder rewriting.
//!
//! Replicates the env block CppNix's `DerivationBuilderImpl::initEnv` +
//! `derivation-env-desugar.cc` construct for the builder process. The
//! daemon used to do this inside its sandbox setup; the native executor
//! receives the COMPLETE environment in the `ExecutionRequest` and adds
//! nothing, so every rule lives here where it is unit-testable.
//!
//! Ordering/precedence (later wins on key collision):
//! 1. base env (`PATH`, `HOME`, `NIX_STORE`, `NIX_BUILD_CORES`, plus
//!    `NIX_OUTPUT_CHECKED` for fixed-output derivations);
//! 2. the derivation's own env — unless `__structuredAttrs`, in which
//!    case only `NIX_ATTRS_JSON_FILE` / `NIX_ATTRS_SH_FILE`;
//! 3. forced values that always win over derivation attrs:
//!    `NIX_BUILD_TOP`, `TMPDIR`, `TEMPDIR`, `TMP`, `TEMP`, `PWD`,
//!    `NIX_LOG_FD=2`, `TERM=xterm-256color`;
//! 4. fixed-output only: each name in `impureEnvVars`, copied **from the
//!    operator-configured impure-env map only** — never from the worker
//!    process's environment. (CppNix falls back to the daemon's own
//!    environment; rio deliberately does not, because the builder pod's
//!    environment carries credentials such as `RIO_EXECUTOR_TOKEN` that
//!    a tenant-supplied `impureEnvVars` list must not be able to read.
//!    Documented divergence — DESIGN.md §4.3.)
//!
//! Every derivation-supplied value (env values, argv, `passAsFile`
//! contents) is passed through [`rewrite`] with the build's
//! `inputRewrites` map so `placeholder "out"`-style strings become real
//! (or scratch) output paths.

use std::collections::BTreeMap;

use rio_nix::derivation::{BasicDerivation, DerivationLike as _, StructuredEnv};

/// In-sandbox build directory. Matches Nix's `sandbox-build-dir`
/// default; nixpkgs builds observe `/build` in `$TMPDIR`/`$NIX_BUILD_TOP`
/// and some bake the value into outputs, so this is part of the
/// de-facto sandbox ABI.
pub(crate) const SANDBOX_BUILD_DIR: &str = "/build";

/// In-sandbox store directory.
pub(crate) const SANDBOX_STORE_DIR: &str = "/nix/store";

/// Apply an `inputRewrites` map to a string: every occurrence of every
/// key is replaced by its value.
///
/// Keys are hash placeholders (`/<52 nixbase32 chars>`) or scratch-path
/// hash parts; they cannot overlap each other, so a single left-to-right
/// pass per key is sufficient and ordering between keys does not matter.
pub(crate) fn rewrite(s: &str, rewrites: &BTreeMap<String, String>) -> String {
    let mut out = s.to_owned();
    for (from, to) in rewrites {
        if out.contains(from.as_str()) {
            out = out.replace(from.as_str(), to);
        }
    }
    out
}

/// A `passAsFile` materialization: the attr's (rewritten) value goes to
/// `/build/<file_name>` and the env gets `<attr>Path` instead of
/// `<attr>`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PassedFile {
    /// File name inside the build directory: `.attr-<nixbase32(sha256(name))>`.
    pub file_name: String,
    /// Rewritten attribute value (the file contents).
    pub contents: Vec<u8>,
}

/// The materialized environment plus the files it requires.
pub(crate) struct BuilderEnv {
    /// Final `KEY=VALUE` map, sorted by key (CppNix's env is a
    /// `std::map`, so the sandboxed process sees sorted environ too).
    pub env: BTreeMap<String, String>,
    /// `passAsFile` files to write into the build directory.
    pub passed_files: Vec<PassedFile>,
}

/// Inputs that influence env construction beyond the derivation itself.
pub(crate) struct EnvOptions<'a> {
    /// Effective core count (already clamped ≥ 1 by the caller).
    pub build_cores: u32,
    /// Operator-configured impure environment (the only source for
    /// `impureEnvVars` values).
    pub impure_env: &'a BTreeMap<String, String>,
}

/// Build the complete builder environment for `drv`.
///
/// `structured` callers (`__structuredAttrs`) get the minimal env; the
/// `.attrs.json` / `.attrs.sh` files themselves are produced by
/// [`super::attrs`].
pub(crate) fn build_env(
    drv: &BasicDerivation,
    rewrites: &BTreeMap<String, String>,
    opts: &EnvOptions<'_>,
) -> BuilderEnv {
    let senv = StructuredEnv::new(drv.env());
    let structured = senv.is_structured_attrs();
    let is_fod = drv.is_fixed_output();

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut passed_files = Vec::new();

    // 1. Base environment.
    env.insert("PATH".into(), "/path-not-set".into());
    env.insert("HOME".into(), "/homeless-shelter".into());
    env.insert("NIX_STORE".into(), SANDBOX_STORE_DIR.into());
    env.insert("NIX_BUILD_CORES".into(), opts.build_cores.to_string());
    if is_fod {
        // Tells nixpkgs' fetcher framework the output hash is checked by
        // the builder (so it can skip its own re-hash).
        env.insert("NIX_OUTPUT_CHECKED".into(), "1".into());
    }

    // 2. The derivation env (or the structured-attrs minimal env).
    if structured {
        env.insert(
            "NIX_ATTRS_JSON_FILE".into(),
            format!("{SANDBOX_BUILD_DIR}/.attrs.json"),
        );
        env.insert(
            "NIX_ATTRS_SH_FILE".into(),
            format!("{SANDBOX_BUILD_DIR}/.attrs.sh"),
        );
    } else {
        // passAsFile: the listed attrs become files instead of env vars.
        // (Ignored under structuredAttrs, matching CppNix's desugaring.)
        let pass_as_file: std::collections::BTreeSet<String> = senv
            .strings("passAsFile")
            .unwrap_or_default()
            .into_iter()
            .collect();

        for (k, v) in drv.env() {
            if pass_as_file.contains(k) {
                let file_name = format!(".attr-{}", attr_file_hash(k));
                env.insert(
                    format!("{k}Path"),
                    format!("{SANDBOX_BUILD_DIR}/{file_name}"),
                );
                passed_files.push(PassedFile {
                    file_name,
                    contents: rewrite(v, rewrites).into_bytes(),
                });
            } else {
                env.insert(k.clone(), rewrite(v, rewrites));
            }
        }
    }

    // 3. Forced values — always win over derivation attrs.
    for k in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
        env.insert(k.into(), SANDBOX_BUILD_DIR.into());
    }
    env.insert("NIX_LOG_FD".into(), "2".into());
    env.insert("TERM".into(), "xterm-256color".into());

    // 4. Fixed-output: impureEnvVars from the operator map ONLY.
    if is_fod {
        for name in senv.strings("impureEnvVars").unwrap_or_default() {
            let value = opts.impure_env.get(&name).cloned().unwrap_or_default();
            env.insert(name, value);
        }
    }

    BuilderEnv { env, passed_files }
}

/// `nixbase32(sha256(attr_name))` — the suffix Nix uses for
/// `passAsFile` file names (`.attr-<this>`).
fn attr_file_hash(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_bytes());
    rio_nix::store_path::nixbase32::encode(&digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use rio_nix::derivation::DerivationOutput;
    use rio_nix::store_path::hash_placeholder;

    fn drv_with_env(env: &[(&str, &str)]) -> BasicDerivation {
        BasicDerivation::new(
            vec![
                DerivationOutput::new(
                    "out",
                    "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
                    "",
                    "",
                )
                .unwrap(),
            ],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash/bin/bash".into(),
            vec![],
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn fod_with_env(env: &[(&str, &str)]) -> BasicDerivation {
        BasicDerivation::new(
            vec![
                DerivationOutput::new(
                    "out",
                    "/nix/store/cccccccccccccccccccccccccccccccc-src.tar.gz",
                    "sha256",
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
            ],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash/bin/bash".into(),
            vec![],
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn no_rewrites() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn default_opts(impure: &BTreeMap<String, String>) -> EnvOptions<'_> {
        EnvOptions {
            build_cores: 4,
            impure_env: impure,
        }
    }

    #[test]
    fn base_env_and_drv_env() {
        let impure = BTreeMap::new();
        let drv = drv_with_env(&[
            ("out", "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"),
            ("name", "x"),
        ]);
        let BuilderEnv { env, passed_files } =
            build_env(&drv, &no_rewrites(), &default_opts(&impure));
        assert!(passed_files.is_empty());
        assert_eq!(env["PATH"], "/path-not-set");
        assert_eq!(env["HOME"], "/homeless-shelter");
        assert_eq!(env["NIX_STORE"], "/nix/store");
        assert_eq!(env["NIX_BUILD_CORES"], "4");
        assert_eq!(env["name"], "x");
        assert_eq!(env["out"], "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x");
        assert_eq!(env["NIX_LOG_FD"], "2");
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["PWD"], "/build");
        assert!(!env.contains_key("NIX_OUTPUT_CHECKED"), "not a FOD");
    }

    #[test]
    fn forced_vars_override_drv_env() {
        let impure = BTreeMap::new();
        // A drv that tries to set TMPDIR/TERM/NIX_LOG_FD gets overridden.
        let drv = drv_with_env(&[
            ("TMPDIR", "/somewhere-else"),
            ("TERM", "dumb"),
            ("NIX_LOG_FD", "7"),
            ("TEMP", "/tmp"),
        ]);
        let env = build_env(&drv, &no_rewrites(), &default_opts(&impure)).env;
        assert_eq!(env["TMPDIR"], "/build");
        assert_eq!(env["TEMP"], "/build");
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["NIX_LOG_FD"], "2");
    }

    #[test]
    fn drv_env_overrides_base_path() {
        let impure = BTreeMap::new();
        // stdenv sets PATH in the drv env; it must win over the
        // /path-not-set base value (base → drv → forced ordering).
        let drv = drv_with_env(&[("PATH", "/nix/store/dddddddddddddddddddddddddddddddd-sd/bin")]);
        let env = build_env(&drv, &no_rewrites(), &default_opts(&impure)).env;
        assert_eq!(
            env["PATH"],
            "/nix/store/dddddddddddddddddddddddddddddddd-sd/bin"
        );
    }

    #[test]
    fn placeholder_rewriting_reaches_env_values() {
        let impure = BTreeMap::new();
        let ph = hash_placeholder("out");
        let drv = drv_with_env(&[("configureFlags", &format!("--prefix={ph}"))]);
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            ph.clone(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x".to_string(),
        );
        let env = build_env(&drv, &rewrites, &default_opts(&impure)).env;
        assert_eq!(
            env["configureFlags"],
            "--prefix=/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"
        );
    }

    #[test]
    fn pass_as_file_materializes_and_rewrites() {
        let impure = BTreeMap::new();
        let ph = hash_placeholder("out");
        let drv = drv_with_env(&[
            ("passAsFile", "buildScript"),
            ("buildScript", &format!("install -D x {ph}/bin/x")),
        ]);
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            ph,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x".to_string(),
        );
        let BuilderEnv { env, passed_files } = build_env(&drv, &rewrites, &default_opts(&impure));

        // The attr itself is NOT in the env; <attr>Path is.
        assert!(!env.contains_key("buildScript"));
        // Golden file name: Nix writes .attr-<nixbase32(sha256(name))>.
        // The literal below is
        //   nix-hash --type sha256 --to-base32 \
        //     $(echo -n buildScript | sha256sum | cut -d' ' -f1)
        // = nixbase32(sha256("buildScript")) — pinned so an accidental
        // change to the hashing or encoding fails loudly, not just a
        // length check.
        assert_eq!(passed_files.len(), 1);
        let pf = &passed_files[0];
        assert_eq!(env["buildScriptPath"], format!("/build/{}", pf.file_name));
        assert_eq!(
            pf.file_name,
            ".attr-1hh0pq9k4wbl9mj5xlw82k9dw6jijbh6a82qwxyb5mlam7l6j2s9"
        );
        // Contents are rewritten.
        assert_eq!(
            pf.contents,
            b"install -D x /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x/bin/x".to_vec()
        );
        // The passAsFile attr itself stays in the env (it is an ordinary
        // attr from the drv's perspective).
        assert_eq!(env["passAsFile"], "buildScript");
    }

    #[test]
    fn structured_attrs_gets_minimal_env() {
        let impure = BTreeMap::new();
        let drv = drv_with_env(&[
            ("__structuredAttrs", "1"),
            ("__json", r#"{"name":"x","configurePhase":"true"}"#),
            ("out", "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"),
        ]);
        let env = build_env(&drv, &no_rewrites(), &default_opts(&impure)).env;
        assert_eq!(env["NIX_ATTRS_JSON_FILE"], "/build/.attrs.json");
        assert_eq!(env["NIX_ATTRS_SH_FILE"], "/build/.attrs.sh");
        // The drv env (including $out and __json) is NOT copied in.
        assert!(!env.contains_key("out"));
        assert!(!env.contains_key("__json"));
        assert!(!env.contains_key("name"));
        // Base + forced still present.
        assert_eq!(env["PATH"], "/path-not-set");
        assert_eq!(env["TMPDIR"], "/build");
    }

    #[test]
    fn fod_impure_env_comes_from_operator_map_only() {
        // The worker process env must never leak in: set a var in the
        // process env and confirm it is NOT picked up.
        // (Static value: build_env never consults std::env at all.)
        let mut impure = BTreeMap::new();
        impure.insert("https_proxy".to_string(), "http://proxy:3128".to_string());
        let drv = fod_with_env(&[
            ("impureEnvVars", "https_proxy NO_SUCH_VAR"),
            ("urls", "https://example.org/src.tar.gz"),
            ("outputHash", "sha256-aaaa"),
        ]);
        let env = build_env(&drv, &no_rewrites(), &default_opts(&impure)).env;
        assert_eq!(env["NIX_OUTPUT_CHECKED"], "1");
        assert_eq!(env["https_proxy"], "http://proxy:3128");
        // Listed but not in the operator map → empty string (CppNix
        // behavior for unset impure vars), NOT the worker's value.
        assert_eq!(env["NO_SUCH_VAR"], "");
    }

    #[test]
    fn rewrite_handles_multiple_occurrences_and_keys() {
        let mut rw = BTreeMap::new();
        rw.insert("/AAA".to_string(), "/nix/store/x-a".to_string());
        rw.insert("/BBB".to_string(), "/nix/store/y-b".to_string());
        assert_eq!(
            rewrite("/AAA:/BBB:/AAA", &rw),
            "/nix/store/x-a:/nix/store/y-b:/nix/store/x-a"
        );
        assert_eq!(rewrite("untouched", &rw), "untouched");
    }
}
