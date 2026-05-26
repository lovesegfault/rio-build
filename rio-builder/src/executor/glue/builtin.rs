//! Request construction for `builtin:fetchurl` derivations.
//!
//! A `builtin:` derivation has no builder script to execute; under the
//! daemon it ran as C++ inside the daemon's sandbox child. The native
//! executor keeps the same isolation shape by re-exec'ing the
//! rio-builder binary itself inside the rio-exec sandbox as
//! `rio-builder __builtin-fetchurl` (see [`crate::builtin_fetchurl`]
//! for the in-sandbox half). This module builds that
//! [`ExecutionRequest`]: the binary mount, the `RIO_FETCHURL_*`
//! parameter env, the network-enabled isolation, and the writable
//! store scratch the output lands in.
//!
//! The result is an ordinary [`PreparedBuild`] — the result glue
//! processes a fetchurl output exactly like any other FOD output
//! (existence, canonicalisation, reference scan, **hash verification**).

use std::ffi::OsString;
use std::path::PathBuf;

use rio_exec::{
    ExecutionRequest, InlineFile, Isolation, Limits, Mount, OutputCapture, Personality,
};
use rio_nix::derivation::BasicDerivation;

use crate::builtin_fetchurl::env_vars;

use super::{
    GlueError, PreparedBuild, SANDBOX_BUILD_DIR, SANDBOX_STORE_DIR, SandboxOptions, SandboxPaths,
    plan_outputs,
};

/// In-sandbox path the rio-builder binary is bind-mounted at for
/// builtin builds. Outside `/nix/store` and `/build` so it can never
/// collide with (or be confused for) an input or an output.
pub(crate) const SANDBOX_BUILDER_BIN: &str = "/rio/rio-builder";

/// In-sandbox directory the fetched output is written into for builtin
/// builds. The host `/nix/store` is mounted read-only at `/nix/store`
/// (see `prepare_fetchurl`), so the output cannot be written there;
/// this directory's mount source is the per-build merged store, which
/// is exactly where the host-side collection and the FOD hash gate
/// already look for produced outputs.
pub(crate) const SANDBOX_BUILTIN_OUT_DIR: &str = "/rio/out";

/// In-sandbox path of the netrc file when the operator configured one.
const SANDBOX_NETRC: &str = "/build/.netrc";

/// Translate a `builtin:fetchurl` derivation into a sandbox request
/// that re-execs this binary's `__builtin-fetchurl` subcommand.
pub(crate) fn prepare_fetchurl(
    drv_path: &str,
    drv: &BasicDerivation,
    paths: &SandboxPaths,
    opts: &SandboxOptions,
) -> Result<PreparedBuild, GlueError> {
    let url = drv
        .env()
        .get("url")
        .filter(|u| !u.is_empty())
        .ok_or(GlueError::FetchurlMissingUrl)?;

    let (input_rewrites, outputs) = plan_outputs(drv_path, drv)?;
    // builtin:fetchurl always has exactly one output; tolerate anything
    // else by fetching into the first declared output (parity with the
    // daemon, which only ever looked at "out").
    let output = outputs.first().ok_or(GlueError::FetchurlMissingUrl)?;
    // The subcommand writes the fetched bytes into the dedicated writable
    // mount, NOT the in-store path: the sandbox's `/nix/store` is the host
    // store mounted read-only (required so the dynamically linked
    // re-exec'd binary can resolve its interpreter and libraries). The
    // file lands in the overlay upper under the output's basename, which
    // is where collection and the FOD hash gate already look.
    let out_basename = std::path::Path::new(&output.path)
        .file_name()
        .expect("planned output paths always carry a store basename");
    let sandbox_out = PathBuf::from(SANDBOX_BUILTIN_OUT_DIR).join(out_basename);

    let builder_binary = opts
        .builder_binary
        .clone()
        .ok_or(GlueError::FetchurlBuilderBinaryUnknown)?;

    // ---- parameter env ---------------------------------------------------
    let drv_flag = |name: &str| drv.env().get(name).map(String::as_str) == Some("1");
    let unpack = drv_flag("unpack");
    let executable = drv_flag("executable");

    // The output's declared hash drives hashed-mirror URL construction.
    // Mirrors only make sense for flat-mode FODs (`<mirror>/<algo>/<b16>`
    // is the flat content hash); recursive ("r:"-prefixed) outputs skip
    // them, matching Nix.
    let drv_output = drv
        .outputs()
        .iter()
        .find(|o| o.name() == output.name)
        .expect("planned output exists in derivation");
    let algo = drv_output.hash_algo();
    let flat = !algo.starts_with("r:") && !algo.is_empty();

    let mut env: Vec<(OsString, OsString)> = vec![
        (env_vars::URL.into(), url.into()),
        (env_vars::OUTPUT.into(), sandbox_out.as_os_str().to_owned()),
        (
            env_vars::UNPACK.into(),
            if unpack { "1" } else { "0" }.into(),
        ),
        (
            env_vars::EXECUTABLE.into(),
            if executable { "1" } else { "0" }.into(),
        ),
        // Hygiene parity with normal builds; the subcommand itself
        // writes only next to the output path.
        ("HOME".into(), "/homeless-shelter".into()),
        ("TMPDIR".into(), SANDBOX_BUILD_DIR.into()),
    ];
    if flat && !opts.hashed_mirrors.is_empty() {
        env.push((
            env_vars::MIRRORS.into(),
            opts.hashed_mirrors.join(" ").into(),
        ));
        env.push((env_vars::HASH_ALGO.into(), algo.into()));
        env.push((env_vars::HASH_B16.into(), drv_output.hash().into()));
    }

    // ---- mounts + inline files --------------------------------------------
    let mut mounts = vec![
        Mount {
            source: paths.build_dir.clone(),
            target: PathBuf::from(SANDBOX_BUILD_DIR),
            writable: true,
            optional: false,
        },
        // The HOST store, read-only: the re-exec'd rio-builder binary is
        // dynamically linked, so its ELF interpreter and libraries (host
        // store paths) must resolve inside the sandbox. Builtin builds run
        // no tenant code, and the FOD hash gate — not input invisibility —
        // is their integrity boundary, so exposing the store read-only is
        // acceptable (the daemon-era equivalent ran builtins in a child of
        // the daemon with the real store visible).
        Mount {
            source: PathBuf::from("/nix/store"),
            target: PathBuf::from(SANDBOX_STORE_DIR),
            writable: false,
            optional: false,
        },
        // Where the fetched output is actually written (see SANDBOX_BUILTIN_OUT_DIR).
        Mount {
            source: paths.merged_store.clone(),
            target: PathBuf::from(SANDBOX_BUILTIN_OUT_DIR),
            writable: true,
            optional: false,
        },
        Mount {
            source: builder_binary,
            target: PathBuf::from(SANDBOX_BUILDER_BIN),
            writable: false,
            optional: false,
        },
    ];
    // TLS roots: a fetchurl build is network-enabled by definition, so
    // the CA bundle is mounted unconditionally (not gated on is_fod the
    // way the generic path gates it).
    if let Some(ca) = &opts.ca_bundle {
        mounts.push(Mount {
            source: ca.clone(),
            target: PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
            writable: false,
            optional: true,
        });
    }
    for p in &opts.extra_sandbox_paths {
        mounts.push(Mount {
            source: p.clone(),
            target: p.clone(),
            writable: false,
            optional: false,
        });
    }

    let mut inline_files = Vec::new();
    if let Some(netrc) = &opts.netrc {
        inline_files.push(InlineFile {
            path: PathBuf::from(SANDBOX_NETRC),
            contents: netrc.clone(),
            mode: 0o600,
        });
        env.push((env_vars::NETRC.into(), SANDBOX_NETRC.into()));
    }

    // ---- request -----------------------------------------------------------
    let request = ExecutionRequest {
        program: PathBuf::from(SANDBOX_BUILDER_BIN),
        args: vec![
            OsString::from("rio-builder"),
            OsString::from("__builtin-fetchurl"),
        ],
        env,
        cwd: PathBuf::from(SANDBOX_BUILD_DIR),
        mounts,
        extra_devices: Vec::new(),
        inline_files,
        declared_outputs: vec![sandbox_out.clone()],
        capture: OutputCapture::MergedPty,
        isolation: Isolation {
            network: true,
            uid: opts.uid,
            gid: opts.gid,
            personality: Personality::Native,
            hostname: "localhost".to_owned(),
            deny_setuid_and_xattrs: true,
        },
        limits: Limits {
            timeout: opts.timeout,
            max_silent: opts.max_silent,
            max_log_bytes: opts.max_log_bytes,
            cgroup: opts.cgroup.clone(),
        },
    };
    request
        .validate()
        .map_err(|e| GlueError::InvalidRequest(e.to_string()))?;

    Ok(PreparedBuild {
        request,
        input_rewrites,
        outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::derivation::DerivationOutput;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::time::Duration;

    const OUT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src";

    /// Self-contained fixtures (deliberately NOT shared with the parent
    /// module's test helpers, whose `mk_drv` hardcodes a bash builder).
    fn fetchurl_drv(hash_algo: &str, extra_env: &[(&str, &str)]) -> BasicDerivation {
        let mut env: Vec<(&str, &str)> =
            vec![("url", "https://example.org/src.tar.xz"), ("out", OUT)];
        env.extend_from_slice(extra_env);
        BasicDerivation::new(
            vec![DerivationOutput::new("out", OUT, hash_algo, "00".repeat(32)).unwrap()],
            BTreeSet::new(),
            "builtin".into(),
            "builtin:fetchurl".into(),
            vec![],
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn paths() -> SandboxPaths {
        SandboxPaths {
            build_dir: PathBuf::from("/host/builds/b1/build"),
            merged_store: PathBuf::from("/host/builds/b1/merged"),
        }
    }

    fn opts() -> SandboxOptions {
        SandboxOptions {
            build_cores: 2,
            uid: 1000,
            gid: 100,
            sandbox_shell: None,
            extra_sandbox_paths: vec![],
            impure_env: BTreeMap::new(),
            ca_bundle: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
            extra_devices: vec![],
            host_system: "x86_64-linux".to_string(),
            timeout: Some(Duration::from_secs(3600)),
            max_silent: Some(Duration::from_secs(600)),
            max_log_bytes: Some(64 * 1024 * 1024),
            cgroup: None,
            hashed_mirrors: vec![],
            builder_binary: Some(PathBuf::from("/host/bin/rio-builder")),
            netrc: None,
        }
    }

    #[test]
    fn request_shape() {
        let drv = fetchurl_drv("sha256", &[("unpack", "1"), ("executable", "1")]);
        let mut o = opts();
        o.hashed_mirrors = vec!["http://mirror/".into()];
        o.netrc = Some(b"machine example.org login a password b\n".to_vec());
        let pb = prepare_fetchurl("/nix/store/x.drv", &drv, &paths(), &o).expect("prepare");

        assert_eq!(pb.request.program, PathBuf::from(SANDBOX_BUILDER_BIN));
        assert_eq!(
            pb.request.args,
            vec![
                OsString::from("rio-builder"),
                OsString::from("__builtin-fetchurl")
            ]
        );
        let env: BTreeMap<String, String> = pb
            .request
            .env
            .iter()
            .map(|(k, v)| {
                // Test fixtures are known UTF-8; to_str (not the
                // workspace-disallowed to_string_lossy) keeps that explicit.
                (
                    k.to_str().expect("utf-8 env key").to_owned(),
                    v.to_str().expect("utf-8 env value").to_owned(),
                )
            })
            .collect();
        assert_eq!(env[env_vars::URL], "https://example.org/src.tar.xz");
        // The fetch target lives in the dedicated writable mount, keyed by
        // the output's store basename — NOT the in-store path, which is
        // read-only inside a builtin sandbox.
        assert_eq!(
            env[env_vars::OUTPUT],
            "/rio/out/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src"
        );
        assert_eq!(env[env_vars::UNPACK], "1");
        assert_eq!(env[env_vars::EXECUTABLE], "1");
        assert_eq!(env[env_vars::MIRRORS], "http://mirror/");
        assert_eq!(env[env_vars::HASH_ALGO], "sha256");
        assert_eq!(env[env_vars::NETRC], "/build/.netrc");

        // The binary is mounted read-only at the program path; the host
        // store is read-only at /nix/store (so the dynamically linked
        // re-exec can resolve its interpreter); the merged store backs the
        // writable output dir; network is on; CA bundle present.
        let bin_mount = pb
            .request
            .mounts
            .iter()
            .find(|m| m.target.as_path() == Path::new(SANDBOX_BUILDER_BIN))
            .expect("builder binary mount");
        assert!(!bin_mount.writable);
        assert_eq!(bin_mount.source, PathBuf::from("/host/bin/rio-builder"));
        let store_mount = pb
            .request
            .mounts
            .iter()
            .find(|m| m.target.as_path() == Path::new(SANDBOX_STORE_DIR))
            .expect("host store mount");
        assert!(!store_mount.writable, "host store must be read-only");
        assert_eq!(store_mount.source, PathBuf::from("/nix/store"));
        let out_mount = pb
            .request
            .mounts
            .iter()
            .find(|m| m.target.as_path() == Path::new(SANDBOX_BUILTIN_OUT_DIR))
            .expect("writable output mount");
        assert!(out_mount.writable);
        assert_eq!(out_mount.source, PathBuf::from("/host/builds/b1/merged"));
        assert_eq!(
            pb.request.declared_outputs,
            vec![PathBuf::from(
                "/rio/out/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src"
            )]
        );
        assert!(
            pb.request
                .mounts
                .iter()
                .any(|m| m.target.as_path() == Path::new("/etc/ssl/certs/ca-certificates.crt"))
        );
        assert!(pb.request.isolation.network);
        // The netrc rides as an inline file with tight permissions.
        assert_eq!(pb.request.inline_files.len(), 1);
        assert_eq!(pb.request.inline_files[0].mode, 0o600);
        assert_eq!(pb.outputs.len(), 1);
    }

    #[test]
    fn recursive_hash_gets_no_mirrors() {
        let drv = fetchurl_drv("r:sha256", &[]);
        let mut o = opts();
        o.hashed_mirrors = vec!["http://mirror/".into()];
        let pb = prepare_fetchurl("/nix/store/x.drv", &drv, &paths(), &o).expect("prepare");
        assert!(
            !pb.request
                .env
                .iter()
                .any(|(k, _)| k == &OsString::from(env_vars::MIRRORS)),
            "recursive-mode FODs must not receive hashed mirrors"
        );
    }

    #[test]
    fn missing_url_is_rejected() {
        // Construct directly so the `url` attribute is genuinely absent.
        let drv = BasicDerivation::new(
            vec![DerivationOutput::new("out", OUT, "sha256", "00".repeat(32)).unwrap()],
            BTreeSet::new(),
            "builtin".into(),
            "builtin:fetchurl".into(),
            vec![],
            [("out", OUT)]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap();
        let err = prepare_fetchurl("/nix/store/x.drv", &drv, &paths(), &opts()).unwrap_err();
        assert!(matches!(err, GlueError::FetchurlMissingUrl));
    }

    #[test]
    fn missing_builder_binary_is_rejected() {
        let drv = fetchurl_drv("sha256", &[]);
        let mut o = opts();
        o.builder_binary = None;
        let err = prepare_fetchurl("/nix/store/x.drv", &drv, &paths(), &o).unwrap_err();
        assert!(matches!(err, GlueError::FetchurlBuilderBinaryUnknown));
    }
}
