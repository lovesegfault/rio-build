//! Nix request glue: `Derivation` → [`rio_exec::ExecutionRequest`].
//!
//! This module is the Nix-specific layer ABOVE the build-system-agnostic
//! sandbox executor. It owns everything the per-build `nix-daemon`
//! subprocess used to do *before* forking the builder:
//!
//! - the builder environment ([`env`]), including `passAsFile`,
//!   placeholder (`inputRewrites`) substitution and the FOD impure-env
//!   policy;
//! - `__structuredAttrs` materialization ([`attrs`]);
//! - `exportReferencesGraph` materialization ([`refs_graph`]);
//! - the sandbox filesystem view (mounts of the overlay-merged store,
//!   the input closure, `/bin/sh`, operator extra paths, the CA bundle
//!   for network builds);
//! - isolation/limit parameters (uid/gid, network-for-FODs, 32-bit
//!   personality, timeouts, the per-build cgroup);
//! - and the `@nix` log side-channel filter ([`log`]) that replaces the
//!   daemon's `STDERR_RESULT{SetPhase}` frames.
//!
//! Boundary discipline (DESIGN.md §4.1): nothing below
//! [`rio_exec::ExecutionRequest`] knows what a store path or a
//! derivation is; everything Nix-specific is resolved HERE into plain
//! mounts, files, argv and env.
//!
//! This is the live request path: `run_native_lifecycle` calls
//! [`derivation_into_request`] for every build and hands the resulting
//! request (or the builtin:fetchurl re-exec plan) to
//! `rio_exec::execute()`.

pub(crate) mod attrs;
pub(crate) mod builtin;
pub(crate) mod env;
pub(crate) mod log;
pub(crate) mod refs_graph;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rio_exec::{
    ExecutionRequest, InlineFile, Isolation, Limits, Mount, OutputCapture, Personality,
};
use rio_nix::derivation::{BasicDerivation, DerivationLike as _, StructuredEnv};
use rio_nix::store_path::{self, hash_placeholder};
use rio_proto::validated::ValidatedPathInfo;

use env::{SANDBOX_BUILD_DIR, SANDBOX_STORE_DIR};
use refs_graph::ClosureIndex;

/// Errors from translating a derivation into an execution request.
///
/// All of these are *input* problems (the derivation asks for something
/// unsupported or inconsistent with its input closure) or internal
/// inconsistencies — none are transient.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GlueError {
    #[error("unsupported builtin builder `{builder}` (only builtin:fetchurl is supported)")]
    UnsupportedBuiltin { builder: String },

    #[error("output `{output}` has no store path and is not content-addressed")]
    MissingOutputPath { output: String },

    #[error("__structuredAttrs is set but the __json attribute is missing or malformed")]
    StructuredAttrsMissingJson,

    #[error("__structuredAttrs __json is not a JSON object")]
    StructuredAttrsNotObject,

    #[error("serializing .attrs.json: {0}")]
    AttrsJsonSerialize(#[source] serde_json::Error),

    #[error("exportReferencesGraph path {path} is outside the build's input closure")]
    ExportRefsOutsideClosure { path: String },

    /// CppNix runs every graph target through `toStorePath()`, which
    /// requires the path to live under the store dir; anything else is
    /// rejected with "path '…' is not in the Nix store".
    #[error("exportReferencesGraph path {path} is not in the Nix store")]
    ExportRefsNotAStorePath { path: String },

    #[error("exportReferencesGraph: no metadata for closure path {path}")]
    ExportRefsMissingMetadata { path: String },

    #[error("exportReferencesGraph: cannot read derivation {path} for closure expansion: {reason}")]
    ExportRefsDrvUnreadable { path: String, reason: String },

    #[error(
        "exportReferencesGraph: cannot expand {drv}: output `{output}` has no statically-known \
         store path (content-addressed derivations are not supported here, matching Nix)"
    )]
    ExportRefsDrvFloatingOutput { drv: String, output: String },

    #[error(
        "exportReferencesGraph: expanding {drv} requires path info for {path}, which is not in \
         the build's input metadata — reference the derivation via its `drvPath` (or otherwise \
         add its outputs to the build's inputs) so their metadata travels with the input set"
    )]
    ExportRefsDrvOutputMissing { drv: String, path: String },

    #[error("exportReferencesGraph value is malformed (expected `name path` pairs): {value}")]
    ExportRefsMalformed { value: String },

    /// The graph *name* is tenant-controlled and becomes a file name
    /// under `/build` (flat form) — reject anything that is not a safe
    /// identifier at the source instead of relying on downstream path
    /// validation. Mirrors CppNix's `[A-Za-z_][A-Za-z0-9_.-]*` check,
    /// so a derivation Nix accepts is accepted here and vice versa.
    #[error(
        "exportReferencesGraph name `{name}` is not a valid graph name \
         (must match [A-Za-z_][A-Za-z0-9_.-]*)"
    )]
    ExportRefsInvalidName { name: String },

    #[error("derivation builder path is empty")]
    EmptyBuilder,

    #[error("derivation path {path} is not a valid store path: {message}")]
    BadDerivationPath { path: String, message: String },

    #[error("builtin:fetchurl derivation has no (non-empty) `url` attribute")]
    FetchurlMissingUrl,

    /// CppNix refuses to run `builtin:fetchurl` for anything but a
    /// fixed-output derivation (`builtins/fetchurl.cc`: "'builtin:fetchurl'
    /// must be a fixed-output derivation"). Mirroring that is also a
    /// security boundary: the fetchurl request is the only build path that
    /// sets `Isolation { network: true }`, and that grant must stay tied to
    /// fixed-output-ness — otherwise a tenant could submit a hash-less
    /// `builtin:fetchurl` derivation and use the Builder pod's network
    /// identity for SSRF/exfiltration, with no FOD hash gate over what it
    /// downloads.
    #[error(
        "builtin:fetchurl requires a fixed-output derivation \
         (a single `out` output with outputHash/outputHashAlgo set)"
    )]
    BuiltinFetchurlNotFixedOutput,

    #[error(
        "builtin:fetchurl requires the rio-builder binary path (SandboxOptions::builder_binary) \
         to re-exec inside the sandbox"
    )]
    FetchurlBuilderBinaryUnknown,

    /// The constructed request failed `rio_exec` boundary validation.
    /// Carries only the validation message (never the executor's
    /// infrastructure error variants — those cannot occur here, and
    /// admitting the whole `ExecError` enum would contradict the
    /// "input problems only" contract above).
    #[error("constructed execution request failed validation: {0}")]
    InvalidRequest(String),
}

/// Host-side directories backing the sandbox's writable mounts.
pub(crate) struct SandboxPaths {
    /// Host directory that becomes `/build` inside the sandbox (the
    /// per-build scratch/tmp dir).
    pub build_dir: PathBuf,
    /// Host directory that becomes `/nix/store` inside the sandbox —
    /// the overlay *merged* view (FUSE-backed inputs ∪ upper layer).
    /// Outputs written through it land in the upper layer where the
    /// existing collection/upload code reads them.
    pub merged_store: PathBuf,
}

/// Per-build options resolved by the caller (config + assignment).
pub(crate) struct SandboxOptions {
    /// Effective core count (already clamped ≥ 1).
    pub build_cores: u32,
    /// uid/gid the builder runs as inside the sandbox.
    pub uid: u32,
    pub gid: u32,
    /// Host path of the static shell to provide as `/bin/sh`
    /// (a store path of the builder image, e.g. busybox-static's `sh`).
    /// `None` omits the mount (some minimal fetcher images may not
    /// carry one; nixpkgs builds effectively require it).
    pub sandbox_shell: Option<PathBuf>,
    /// Operator-configured extra read-only bind mounts (same path on
    /// both sides), the successor of nix.conf `extra-sandbox-paths`.
    pub extra_sandbox_paths: Vec<PathBuf>,
    /// Operator-configured impure environment for FOD `impureEnvVars`.
    pub impure_env: BTreeMap<String, String>,
    /// Host CA bundle to expose at `/etc/ssl/certs/ca-certificates.crt`
    /// for network (fixed-output) builds.
    pub ca_bundle: Option<PathBuf>,
    /// Extra character devices to expose (e.g. `/dev/kvm` when the
    /// worker advertises the `kvm` feature).
    pub extra_devices: Vec<PathBuf>,
    /// The worker's own system (e.g. `x86_64-linux`), for the 32-bit
    /// personality decision.
    pub host_system: String,
    /// Wall-clock timeout for the build (None = unbounded).
    pub timeout: Option<Duration>,
    /// Max-silent timeout (None = disabled).
    pub max_silent: Option<Duration>,
    /// Max captured log bytes (None = unlimited).
    pub max_log_bytes: Option<u64>,
    /// Per-build cgroup directory (created by the caller).
    pub cgroup: Option<PathBuf>,
    /// Hashed-mirror base URLs tried before the origin URL by
    /// `builtin:fetchurl` (successor of nix.conf `hashed-mirrors`).
    /// Only passed to flat-mode FODs.
    pub hashed_mirrors: Vec<String>,
    /// Host path of the rio-builder binary itself, bind-mounted into
    /// builtin sandboxes for the `__builtin-fetchurl` re-exec. The
    /// activation wires this from `std::env::current_exe()`; `None`
    /// makes builtin derivations fail with a typed error.
    pub builder_binary: Option<PathBuf>,
    /// Operator-provided netrc contents for authenticated fetchurl
    /// sources, written into the sandbox as an inline file.
    pub netrc: Option<Vec<u8>>,
}

/// One planned derivation output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedOutput {
    pub name: String,
    /// In-sandbox store path the build will produce this output at:
    /// the declared path for input-addressed / fixed-output outputs,
    /// the deterministic scratch path for floating-CA outputs (which
    /// the result glue rewrites to the final content-addressed path).
    pub path: String,
    pub floating_ca: bool,
}

/// A fully-translated sandbox build.
#[derive(Debug)]
pub(crate) struct PreparedBuild {
    pub request: ExecutionRequest,
    /// Outputs in derivation declaration order.
    pub outputs: Vec<PlannedOutput>,
}

/// What the executor should do with a derivation.
#[derive(Debug)]
pub(crate) enum GluePlan {
    /// Run the builder inside the sandbox.
    Sandbox(Box<PreparedBuild>),
    /// `builtin:fetchurl` — the rio-builder binary re-execs itself
    /// inside the sandbox as `rio-builder __builtin-fetchurl`; the
    /// carried request is that re-exec (see [`builtin`]).
    BuiltinFetchurl(Box<PreparedBuild>),
}

/// Translate a resolved derivation into an execution plan.
///
/// `drv` is the *resolved* `BasicDerivation` (inputDrvs collapsed into
/// inputSrcs, deferred output paths filled) — the same value that was
/// previously sent to the per-build daemon. `input_paths` /
/// `input_metadata` describe the full transitive input closure.
pub(crate) fn derivation_into_request(
    drv_path: &str,
    drv: &BasicDerivation,
    input_paths: &[String],
    input_metadata: &[ValidatedPathInfo],
    paths: &SandboxPaths,
    opts: &SandboxOptions,
) -> Result<GluePlan, GlueError> {
    // builtin: builders never get a generic sandbox request — they
    // re-exec this binary's __builtin-fetchurl subcommand instead.
    if let Some(rest) = drv.builder().strip_prefix("builtin:") {
        return match rest {
            "fetchurl" => builtin::prepare_fetchurl(drv_path, drv, paths, opts)
                .map(|pb| GluePlan::BuiltinFetchurl(Box::new(pb))),
            _ => Err(GlueError::UnsupportedBuiltin {
                builder: drv.builder().to_owned(),
            }),
        };
    }
    if drv.builder().is_empty() {
        return Err(GlueError::EmptyBuilder);
    }

    let is_fod = drv.is_fixed_output();
    let senv = StructuredEnv::new(drv.env());
    let structured = senv.is_structured_attrs();

    // ---- outputs, placeholders, rewrites -------------------------------
    let (input_rewrites, outputs) = plan_outputs(drv_path, drv)?;

    // ---- environment + passAsFile --------------------------------------
    let env_opts = env::EnvOptions {
        build_cores: opts.build_cores,
        impure_env: &opts.impure_env,
    };
    let env::BuilderEnv { env, passed_files } = env::build_env(drv, &input_rewrites, &env_opts);

    // ---- inline files ---------------------------------------------------
    let closure =
        ClosureIndex::new(input_metadata, input_paths).with_store_dir(&paths.merged_store);
    let mut inline_files: Vec<InlineFile> = Vec::new();

    for pf in passed_files {
        inline_files.push(InlineFile {
            path: PathBuf::from(format!("{SANDBOX_BUILD_DIR}/{}", pf.file_name)),
            contents: pf.contents,
            mode: 0o644,
        });
    }

    if structured {
        let json = senv
            .json()
            .ok_or(GlueError::StructuredAttrsMissingJson)?
            .clone();
        let output_names: Vec<String> = drv.outputs().iter().map(|o| o.name().to_owned()).collect();
        let files =
            attrs::prepare_structured_attrs(&json, &output_names, &closure, &input_rewrites)?;
        inline_files.push(InlineFile {
            path: PathBuf::from(format!("{SANDBOX_BUILD_DIR}/.attrs.json")),
            contents: files.attrs_json,
            mode: 0o644,
        });
        inline_files.push(InlineFile {
            path: PathBuf::from(format!("{SANDBOX_BUILD_DIR}/.attrs.sh")),
            contents: files.attrs_sh,
            mode: 0o644,
        });
    } else if let Some(flat) = drv.env().get("exportReferencesGraph") {
        // Flat (non-structured) form: `name path name path …`; each
        // graph file is written into the build dir under its name.
        for (name, target) in refs_graph::parse_flat_export_refs(flat)? {
            let text = closure.registration_text(&[target])?;
            inline_files.push(InlineFile {
                path: PathBuf::from(format!("{SANDBOX_BUILD_DIR}/{name}")),
                contents: text,
                mode: 0o644,
            });
        }
    }

    // ---- mounts ----------------------------------------------------------
    let mut mounts: Vec<Mount> = vec![
        Mount {
            source: paths.build_dir.clone(),
            target: PathBuf::from(SANDBOX_BUILD_DIR),
            writable: true,
            optional: false,
        },
        Mount {
            source: paths.merged_store.clone(),
            target: PathBuf::from(SANDBOX_STORE_DIR),
            writable: true,
            optional: false,
        },
    ];
    // r[impl builder.exec.input-closure-binds]
    // Input closure: read-only binds nested inside the writable store
    // mount, so a build cannot modify (its view of) its inputs.
    for p in input_paths {
        let Some(basename) = store_path::basename(p) else {
            continue;
        };
        mounts.push(Mount {
            source: paths.merged_store.join(basename),
            target: PathBuf::from(p),
            writable: false,
            optional: false,
        });
    }
    if let Some(shell) = &opts.sandbox_shell {
        mounts.push(Mount {
            source: shell.clone(),
            target: PathBuf::from("/bin/sh"),
            writable: false,
            optional: false,
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
    if is_fod && let Some(ca) = &opts.ca_bundle {
        mounts.push(Mount {
            source: ca.clone(),
            target: PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
            writable: false,
            optional: true,
        });
    }

    // ---- argv / program --------------------------------------------------
    // exec path = the full builder path, NOT rewritten; argv[0] = its
    // basename; remaining argv = drv.args with placeholder rewrites.
    let program = PathBuf::from(drv.builder());
    let argv0 = Path::new(drv.builder())
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| OsString::from(drv.builder()));
    let mut args: Vec<OsString> = vec![argv0];
    args.extend(
        drv.args()
            .iter()
            .map(|a| OsString::from(env::rewrite(a, &input_rewrites))),
    );

    // ---- request ---------------------------------------------------------
    let request = ExecutionRequest {
        program,
        args,
        env: env
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect(),
        cwd: PathBuf::from(SANDBOX_BUILD_DIR),
        mounts,
        extra_devices: opts.extra_devices.clone(),
        inline_files,
        declared_outputs: outputs.iter().map(|o| PathBuf::from(&o.path)).collect(),
        capture: OutputCapture::MergedPty,
        isolation: Isolation {
            network: is_fod,
            uid: opts.uid,
            gid: opts.gid,
            personality: personality_for(drv.platform(), &opts.host_system),
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

    Ok(GluePlan::Sandbox(Box::new(PreparedBuild {
        request,
        outputs,
    })))
}

/// Plan the derivation's outputs: where each one will be produced inside
/// the sandbox, and the placeholder rewrites that point builders at
/// those locations.
fn plan_outputs(
    drv_path: &str,
    drv: &BasicDerivation,
) -> Result<(BTreeMap<String, String>, Vec<PlannedOutput>), GlueError> {
    // Parsed lazily: only derivations with floating-CA outputs need the
    // structured form (for the scratch-path computation).
    let mut parsed_drv_path: Option<store_path::StorePath> = None;

    let mut rewrites = BTreeMap::new();
    let mut outputs = Vec::with_capacity(drv.outputs().len());
    for o in drv.outputs() {
        let (path, floating) = if !o.path().is_empty() {
            (o.path().to_owned(), false)
        } else if o.has_hash_algo() {
            let drv_sp = match &parsed_drv_path {
                Some(p) => p,
                None => {
                    let p = store_path::StorePath::parse(drv_path).map_err(|e| {
                        GlueError::BadDerivationPath {
                            path: drv_path.to_owned(),
                            message: e.to_string(),
                        }
                    })?;
                    parsed_drv_path.insert(p)
                }
            };
            // The shared rio-nix implementation of CppNix's
            // `makeFallbackPath` — the result glue's CA finalization
            // recomputes paths with the same crate, so the recipe can
            // never silently diverge between the two sides.
            let scratch = store_path::StorePath::make_scratch_output_path(drv_sp, o.name())
                .map_err(|e| GlueError::BadDerivationPath {
                    path: drv_path.to_owned(),
                    message: e.to_string(),
                })?;
            (scratch.as_str().to_owned(), true)
        } else {
            return Err(GlueError::MissingOutputPath {
                output: o.name().to_owned(),
            });
        };
        rewrites.insert(hash_placeholder(o.name()), path.clone());
        outputs.push(PlannedOutput {
            name: o.name().to_owned(),
            path,
            floating_ca: floating,
        });
    }
    Ok((rewrites, outputs))
}

/// 32-bit personality selection: building a 32-bit system on its 64-bit
/// host needs `PER_LINUX32` so `uname -m` inside the sandbox reports the
/// 32-bit machine.
fn personality_for(drv_system: &str, host_system: &str) -> Personality {
    let linux32 = matches!(
        (host_system, drv_system),
        (h, "i686-linux") if h.starts_with("x86_64-")
    ) || matches!(
        (host_system, drv_system),
        (h, "armv5tel-linux" | "armv6l-linux" | "armv7l-linux") if h.starts_with("aarch64-")
    );
    if linux32 {
        Personality::Linux32
    } else {
        Personality::Native
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use rio_nix::derivation::DerivationOutput;
    use rio_nix::store_path::StorePath;

    const OUT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo";
    const IN_BASH: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bash";
    const IN_DEP: &str = "/nix/store/cccccccccccccccccccccccccccccccc-dep";
    const DRV: &str = "/nix/store/dddddddddddddddddddddddddddddddd-demo.drv";

    fn mk_drv(outputs: Vec<DerivationOutput>, env: &[(&str, &str)]) -> BasicDerivation {
        BasicDerivation::new(
            outputs,
            BTreeSet::from([IN_BASH.to_string(), IN_DEP.to_string()]),
            "x86_64-linux".into(),
            format!("{IN_BASH}/bin/bash"),
            vec!["-e".into(), "builder.sh".into()],
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn ia_drv() -> BasicDerivation {
        mk_drv(
            vec![DerivationOutput::new("out", OUT, "", "").unwrap()],
            &[("name", "demo"), ("out", OUT)],
        )
    }

    fn meta(path: &str) -> ValidatedPathInfo {
        ValidatedPathInfo {
            store_path: StorePath::parse(path).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        }
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
            sandbox_shell: Some(PathBuf::from(
                "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-busybox/bin/sh",
            )),
            extra_sandbox_paths: vec![],
            impure_env: BTreeMap::new(),
            ca_bundle: Some(PathBuf::from("/etc/ssl/certs/ca-bundle.crt")),
            extra_devices: vec![],
            host_system: "x86_64-linux".to_string(),
            timeout: Some(Duration::from_secs(3600)),
            max_silent: Some(Duration::from_secs(600)),
            max_log_bytes: Some(64 * 1024 * 1024),
            cgroup: Some(PathBuf::from("/sys/fs/cgroup/rio/builds/b1")),
            hashed_mirrors: vec![],
            builder_binary: Some(PathBuf::from("/host/bin/rio-builder")),
            netrc: None,
        }
    }

    fn closure() -> (Vec<String>, Vec<ValidatedPathInfo>) {
        (
            vec![IN_BASH.to_string(), IN_DEP.to_string()],
            vec![meta(IN_BASH), meta(IN_DEP)],
        )
    }

    fn prepare(drv: &BasicDerivation) -> PreparedBuild {
        let (input_paths, input_meta) = closure();
        match derivation_into_request(DRV, drv, &input_paths, &input_meta, &paths(), &opts())
            .expect("glue should succeed")
        {
            GluePlan::Sandbox(p) => *p,
            GluePlan::BuiltinFetchurl(_) => panic!("not a builtin"),
        }
    }

    #[test]
    fn sandbox_request_shape() {
        let prepared = prepare(&ia_drv());
        let req = &prepared.request;

        // Program/argv: full path exec, basename argv[0], args preserved.
        assert_eq!(req.program, PathBuf::from(format!("{IN_BASH}/bin/bash")));
        assert_eq!(req.args[0], OsString::from("bash"));
        assert_eq!(req.args[1], OsString::from("-e"));
        assert_eq!(req.cwd, PathBuf::from("/build"));
        assert_eq!(req.capture, OutputCapture::MergedPty);

        // Mounts: /build + /nix/store writable, inputs ro nested, /bin/sh.
        let find = |target: &str| {
            req.mounts
                .iter()
                .find(|m| m.target == Path::new(target))
                .unwrap_or_else(|| panic!("missing mount {target}"))
        };
        assert!(find("/build").writable);
        assert!(find("/nix/store").writable);
        assert_eq!(
            find("/nix/store").source,
            PathBuf::from("/host/builds/b1/merged")
        );
        let dep = find(IN_DEP);
        assert!(!dep.writable);
        assert_eq!(
            dep.source,
            PathBuf::from("/host/builds/b1/merged/cccccccccccccccccccccccccccccccc-dep")
        );
        assert!(!find("/bin/sh").writable);
        // Airgapped (non-FOD) build: no network, no CA bundle mount.
        assert!(!req.isolation.network);
        assert!(
            !req.mounts
                .iter()
                .any(|m| m.target == Path::new("/etc/ssl/certs/ca-certificates.crt"))
        );

        // Isolation/limits.
        assert_eq!(req.isolation.uid, 1000);
        assert_eq!(req.isolation.gid, 100);
        assert_eq!(req.isolation.hostname, "localhost");
        assert!(req.isolation.deny_setuid_and_xattrs);
        assert_eq!(req.isolation.personality, Personality::Native);
        assert_eq!(
            req.limits.cgroup,
            Some(PathBuf::from("/sys/fs/cgroup/rio/builds/b1"))
        );

        // Outputs: declared + planned agree.
        assert_eq!(req.declared_outputs, vec![PathBuf::from(OUT)]);
        assert_eq!(prepared.outputs.len(), 1);
        assert_eq!(prepared.outputs[0].path, OUT);
        assert!(!prepared.outputs[0].floating_ca);

        // Env spot checks (full coverage in env.rs).
        let env_get = |k: &str| {
            req.env
                .iter()
                .find(|(key, _)| key == &OsString::from(k))
                .map(|(_, v)| v.clone())
        };
        assert_eq!(env_get("NIX_BUILD_CORES"), Some(OsString::from("2")));
        assert_eq!(env_get("out"), Some(OsString::from(OUT)));
        assert_eq!(env_get("TMPDIR"), Some(OsString::from("/build")));
    }

    #[test]
    fn fod_gets_network_and_ca_bundle() {
        let fod = mk_drv(
            vec![
                DerivationOutput::new(
                    "out",
                    OUT,
                    "sha256",
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
            ],
            &[("name", "demo"), ("out", OUT), ("outputHashMode", "flat")],
        );
        let prepared = prepare(&fod);
        assert!(prepared.request.isolation.network);
        assert!(
            prepared
                .request
                .mounts
                .iter()
                .any(|m| m.target == Path::new("/etc/ssl/certs/ca-certificates.crt") && m.optional)
        );
    }

    #[test]
    fn floating_ca_outputs_get_scratch_paths() {
        let ca = mk_drv(
            vec![DerivationOutput::new("out", "", "r:sha256", "").unwrap()],
            &[("name", "demo"), ("out", &hash_placeholder("out"))],
        );
        let prepared = prepare(&ca);
        let out = &prepared.outputs[0];
        assert!(out.floating_ca);
        assert!(out.path.starts_with("/nix/store/"));
        assert!(
            out.path.ends_with("-demo"),
            "scratch path keeps the name: {}",
            out.path
        );
        assert_ne!(out.path, OUT);
        // The placeholder in the env is rewritten to the scratch path.
        let env_out = prepared
            .request
            .env
            .iter()
            .find(|(k, _)| k == &OsString::from("out"))
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(env_out, OsString::from(out.path.clone()));
        // Deterministic.
        let again = prepare(&ca);
        assert_eq!(again.outputs[0].path, out.path);
    }

    #[test]
    fn builtin_fetchurl_is_dispatched_not_sandboxed() {
        let drv = BasicDerivation::new(
            vec![
                DerivationOutput::new(
                    "out",
                    OUT,
                    "sha256",
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
            ],
            BTreeSet::new(),
            "builtin".into(),
            "builtin:fetchurl".into(),
            vec![],
            [("url", "https://example.org/x")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap();
        let (input_paths, input_meta) = closure();
        let plan = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap();
        let GluePlan::BuiltinFetchurl(prepared) = plan else {
            panic!("builtin:fetchurl must dispatch to the re-exec path");
        };
        // The re-exec request targets this binary's subcommand, not a
        // builder script.
        assert_eq!(
            prepared.request.args,
            vec![
                OsString::from("rio-builder"),
                OsString::from("__builtin-fetchurl")
            ]
        );

        let other = BasicDerivation::new(
            drv.outputs().to_vec(),
            BTreeSet::new(),
            "builtin".into(),
            "builtin:buildenv".into(),
            vec![],
            BTreeMap::new(),
        )
        .unwrap();
        let err =
            derivation_into_request(DRV, &other, &input_paths, &input_meta, &paths(), &opts())
                .unwrap_err();
        assert!(matches!(err, GlueError::UnsupportedBuiltin { .. }));
    }

    /// A `builtin:fetchurl` derivation WITHOUT an output hash must be
    /// rejected before any network-enabled request is constructed
    /// (CppNix: "'builtin:fetchurl' must be a fixed-output derivation").
    /// The network grant stays tied to fixed-output-ness.
    #[test]
    fn non_fixed_output_builtin_fetchurl_is_rejected() {
        let drv = BasicDerivation::new(
            vec![DerivationOutput::new("out", OUT, "", "").unwrap()],
            BTreeSet::new(),
            "builtin".into(),
            "builtin:fetchurl".into(),
            vec![],
            [("url", "https://internal.metadata.host/latest")]
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap();
        let (input_paths, input_meta) = closure();
        let err = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap_err();
        assert!(
            matches!(err, GlueError::BuiltinFetchurlNotFixedOutput),
            "expected the fixed-output gate, got: {err}"
        );
    }

    #[test]
    fn export_references_graph_flat_form_writes_inline_file() {
        let drv = mk_drv(
            vec![DerivationOutput::new("out", OUT, "", "").unwrap()],
            &[
                ("name", "demo"),
                ("out", OUT),
                ("exportReferencesGraph", &format!("closure {IN_DEP}")),
            ],
        );
        let prepared = prepare(&drv);
        let file = prepared
            .request
            .inline_files
            .iter()
            .find(|f| f.path == Path::new("/build/closure"))
            .expect("graph file present");
        let text = String::from_utf8(file.contents.clone()).unwrap();
        assert!(text.starts_with(&format!("{IN_DEP}\n")));
    }

    #[test]
    fn structured_attrs_files_are_planned() {
        let drv = mk_drv(
            vec![DerivationOutput::new("out", OUT, "", "").unwrap()],
            &[
                ("__structuredAttrs", "1"),
                (
                    "__json",
                    r#"{"name":"demo","mesonFlags":["-Da=1","-Db=2"]}"#,
                ),
                ("out", OUT),
            ],
        );
        let prepared = prepare(&drv);
        let json_file = prepared
            .request
            .inline_files
            .iter()
            .find(|f| f.path == Path::new("/build/.attrs.json"))
            .expect(".attrs.json present");
        let parsed: serde_json::Value = serde_json::from_slice(&json_file.contents).unwrap();
        assert_eq!(parsed["outputs"]["out"], serde_json::json!(OUT));
        assert!(
            prepared
                .request
                .inline_files
                .iter()
                .any(|f| f.path == Path::new("/build/.attrs.sh"))
        );
        // Structured-attrs env is minimal.
        assert!(
            !prepared
                .request
                .env
                .iter()
                .any(|(k, _)| k == &OsString::from("mesonFlags"))
        );
    }

    #[test]
    fn personality_selection() {
        assert_eq!(
            personality_for("i686-linux", "x86_64-linux"),
            Personality::Linux32
        );
        assert_eq!(
            personality_for("x86_64-linux", "x86_64-linux"),
            Personality::Native
        );
        assert_eq!(
            personality_for("armv7l-linux", "aarch64-linux"),
            Personality::Linux32
        );
        assert_eq!(
            personality_for("aarch64-linux", "aarch64-linux"),
            Personality::Native
        );
        assert_eq!(
            personality_for("i686-linux", "aarch64-linux"),
            Personality::Native,
            "cross-ISA routing is the scheduler's problem, not personality's"
        );
    }

    #[test]
    fn scratch_path_matches_known_shape() {
        // The shared rio-nix `make_scratch_output_path` (CppNix
        // `makeFallbackPath`) is what plan_outputs uses; its exact value
        // is pinned in rio-nix — here we only assert the shape the rest
        // of the glue relies on (store dir prefix, outputPathName
        // naming, 32-char hash part).
        let drv = StorePath::parse("/nix/store/dddddddddddddddddddddddddddddddd-demo.drv").unwrap();
        let p = StorePath::make_scratch_output_path(&drv, "dev").unwrap();
        let p = p.as_str();
        assert!(p.starts_with("/nix/store/"));
        assert!(p.ends_with("-demo-dev"));
        // 32-char nixbase32 hash part.
        let base = p.strip_prefix("/nix/store/").unwrap();
        assert_eq!(base.split('-').next().unwrap().len(), 32);
    }
}
