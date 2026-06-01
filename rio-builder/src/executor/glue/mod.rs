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
use rio_nix::derivation::{BasicDerivation, DerivationLike as _, DerivationOutput, StructuredEnv};
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

    /// Transient I/O failure reading a `.drv` from the materialized
    /// input store (FUSE/JIT-fetch hiccup, EIO, missing materialization).
    /// Unlike [`GlueError::ExportRefsDrvUnreadable`] — which covers
    /// structural problems with the derivation itself (unparseable text,
    /// no store available to the caller) — this is a property of the
    /// worker's input materialization, so the executor classifies it as
    /// infra-transient and the build is retried instead of being
    /// rejected (the same bucket as a bind-mount materialization
    /// failure).
    #[error("exportReferencesGraph: I/O error reading derivation {path}: {reason}")]
    ExportRefsDrvIo { path: String, reason: String },

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

    /// The reference metadata reachable from an `exportReferencesGraph`
    /// target contains a reference cycle (other than a self-reference).
    /// CppNix can never face this: its local store's `registerValidPaths`
    /// topological sort makes cyclic metadata unrepresentable, so there is
    /// no defined oracle output to produce. rio-store deliberately admits
    /// cycles (for GC reclamation), which moves the rejection here — and it
    /// MUST stay a permanent input rejection: classifying it as
    /// worker-transient would convert hostile metadata into an infinite
    /// retry storm. The blame is on the registered *metadata*, not on the
    /// derivation being built (its author may be the squatter's victim).
    // r[impl builder.exec.refs-graph-acyclic]
    #[error(
        "exportReferencesGraph: the reference metadata of the requested closure is cyclic \
         (store metadata no Nix toolchain could have produced); paths involved: {}{}",
        paths.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
        if paths.len() > 8 { format!(" (+{} more)", paths.len() - 8) } else { String::new() }
    )]
    ExportRefsCyclicMetadata { paths: Vec<String> },

    /// A structured-attrs `exportReferencesGraph` value has a
    /// wrong-typed leaf. The oracle's `flatten`
    /// (derivation-options.cc:106-114) recursively accepts arrays and
    /// strings and THROWS on anything else; the pre-fix reader
    /// silently emptied nested arrays and skipped non-string leaves —
    /// both produced a wrong (empty) closure file where the oracle
    /// fails the build.
    // r[impl builder.exec.structured-attrs-typed]
    #[error("'exportReferencesGraph' value is not an array or a string (key {key}: {found})")]
    ExportRefsValueWrongType { key: String, found: String },

    /// A behavioral structured attribute the request glue reads
    /// (`passAsFile`, `impureEnvVars`) is wrong-typed or the `__json`
    /// blob is unparseable. Fail-closed per the oracle's typed getters
    /// — never "treat as absent".
    // r[impl builder.exec.structured-attrs-typed]
    #[error("structured attrs read failed: {source}")]
    StructuredAttrWrongType {
        #[from]
        source: rio_nix::derivation::StructuredAttrError,
    },

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

    /// A non-empty declared output path that does not parse as a store
    /// path. Defense-in-depth under `sec.trust.workers-untrusted`: the
    /// gateway's binding gate (`gw.reject.output-path-mismatch+2`) is the
    /// authoritative rejection, and the scheduler shape-checks
    /// `expected_output_paths` too — a malformed path reaching the worker
    /// means both were bypassed. Rejecting it here keeps tenant-controlled
    /// non-store-path strings out of every host-side filesystem join.
    #[error("output `{output}` declares path {path:?}, which is not a valid store path: {message}")]
    MalformedOutputPath {
        output: String,
        path: String,
        message: String,
    },

    /// CppNix `BasicDerivation::type()` (derivations.cc): "can't mix
    /// derivation output types". A floating content-addressed output
    /// cannot coexist with any other output kind in one derivation.
    /// Without this rule the result pipeline's CA finalization would
    /// remap a non-CA sibling's *references* to the final CA paths but
    /// never rewrite its *bytes* (only floating outputs are restored
    /// through the rewriting sink), shipping a corrupt artifact whose
    /// content still names scratch paths.
    #[error(
        "can't mix derivation output types: floating content-addressed output(s) {floating:?} \
         cannot coexist with non-floating output(s) {other:?}"
    )]
    MixedOutputTypes {
        floating: Vec<String>,
        other: Vec<String>,
    },

    #[error("builtin:fetchurl derivation has no (non-empty) `url` attribute")]
    FetchurlMissingUrl,

    /// CppNix refuses to run `builtin:fetchurl` for anything but a
    /// fixed-output derivation (enforced in its derivation build goal's
    /// builtin dispatch: "'builtin:fetchurl' must be a fixed-output
    /// derivation"). Mirroring that is also a
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

    /// A fixed-output output declares an `outputHashAlgo`/`outputHash`
    /// pair the pipeline cannot interpret (unknown algorithm, non-hex
    /// hash, wrong digest length). Same fail-closed stance as the
    /// result pipeline's `FodDeclaredHashInvalid`.
    #[error("fixed-output output `{output}` declares an unverifiable hash: {message}")]
    FixedOutputHashInvalid { output: String, message: String },

    /// A derivation declares a fixed output but is not the one shape
    /// CppNix accepts for fixed-output derivations
    /// (`BasicDerivation::type()`: "only one fixed output is allowed",
    /// "single fixed output must be named \"out\"", "can't mix
    /// derivation output types"). Rejecting the shape here keeps every
    /// downstream FOD gate — hash verification, the no-references rule,
    /// `fixed:` descriptor stamping, fetcher-pool routing — keyed on
    /// one and the same strict predicate.
    #[error("invalid fixed-output derivation: {reason}")]
    FixedOutputBadShape { reason: String },

    /// The declared store path of a fixed-output output is not the path
    /// derived from its declared hash. CppNix discards the declared
    /// path for `CAFixed` outputs and recomputes it from
    /// `(method, algo, hash)` (`makeFixedOutputPath`), so a mismatching
    /// `.drv` can never place content at the declared path there; rio
    /// keeps the declared path as the working path downstream, so the
    /// equivalent guarantee is to reject the mismatch outright before
    /// planning the build. Without this, a crafted `.drv` could pair
    /// another derivation's well-known fixed-output path with the hash
    /// of attacker-chosen bytes and have those bytes registered, signed
    /// and served at that path.
    #[error(
        "fixed-output output `{output}` declares store path {declared}, but its declared \
         outputHash derives {expected}; refusing to build content for a path its hash does \
         not bind"
    )]
    FixedOutputPathMismatch {
        output: String,
        declared: String,
        expected: String,
    },

    /// The constructed request failed `rio_exec` boundary validation.
    /// Carries only the validation message (never the executor's
    /// infrastructure error variants — those cannot occur here, and
    /// admitting the whole `ExecError` enum would contradict the
    /// "input problems only" contract above).
    #[error("constructed execution request failed validation: {0}")]
    InvalidRequest(String),
}

impl GlueError {
    /// `true` for failures that are properties of this worker's input
    /// materialization (transient I/O while reading materialized inputs)
    /// rather than of the derivation. The executor maps these to its
    /// infra-transient bucket so the build is retried elsewhere instead
    /// of being permanently rejected.
    ///
    /// This set MUST NOT be widened to cover structural metadata problems
    /// — in particular [`GlueError::ExportRefsCyclicMetadata`]: cyclic
    /// metadata is identical on every worker, so transient classification
    /// would turn one hostile registration into an unbounded retry storm
    /// (the exact failure mode the cycle rejection exists to prevent).
    pub(crate) fn is_transient_io(&self) -> bool {
        matches!(self, GlueError::ExportRefsDrvIo { .. })
    }
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
    /// Host CA bundle to expose at
    /// [`crate::builtin_fetchurl::SANDBOX_CA_BUNDLE`] for network
    /// (fixed-output) builds.
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
    /// Store basename of `path` (`{hash}-{name}`), computed exactly once
    /// from the PARSED store path at planning time. Every host-side
    /// filesystem join uses this field — never a re-derivation from the
    /// raw string — so a non-store-path declaration can never reach a
    /// `Path::join` (it is rejected before a `PlannedOutput` exists).
    pub basename: String,
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
    // CppNix-parity shape rules run before ANY planning — including the
    // builtin:fetchurl dispatch below, whose declared output path is
    // just as tenant-controlled as a sandboxed FOD's.
    validate_output_type_shape(drv)?;
    validate_fixed_output_declarations(drv_path, drv)?;

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
    let env::BuilderEnv { env, passed_files } = env::build_env(drv, &input_rewrites, &env_opts)?;

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
            target: PathBuf::from(crate::builtin_fetchurl::SANDBOX_CA_BUNDLE),
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
            identity: nix_sandbox_identity(),
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

/// CppNix `BasicDerivation::type()` shape rule (derivations.cc): output
/// types cannot be mixed in one derivation. rio enforces the floating-CA
/// half here — a derivation with any floating-CA output must be ALL
/// floating-CA. (The fixed-output half — exactly one output, named
/// `out` — is `validate_fixed_output_declarations`' shape rule, and
/// plain IA + deferred-IA mixing is resolved upstream before the worker
/// ever sees the derivation.)
///
/// Run before any planning so the sandbox, builtin, and differential
/// paths all share it. The differential corpus cannot pin this rule:
/// `nix-instantiate` refuses to even produce the mixed shape, which is
/// exactly why an honest client can never hit this error.
// r[impl builder.exec.output-types-unmixed]
fn validate_output_type_shape(drv: &BasicDerivation) -> Result<(), GlueError> {
    let is_floating =
        |o: &DerivationOutput| o.path().is_empty() && o.has_hash_algo() && o.hash().is_empty();

    let floating: Vec<String> = drv
        .outputs()
        .iter()
        .filter(|o| is_floating(o))
        .map(|o| o.name().to_owned())
        .collect();
    if floating.is_empty() {
        return Ok(());
    }
    let other: Vec<String> = drv
        .outputs()
        .iter()
        .filter(|o| !is_floating(o))
        .map(|o| o.name().to_owned())
        .collect();
    if !other.is_empty() {
        return Err(GlueError::MixedOutputTypes { floating, other });
    }
    Ok(())
}

/// CppNix-parity validation of declared fixed-output (`CAFixed`)
/// outputs, run before any sandbox or builtin planning.
///
/// **Path↔hash binding** (CppNix `Derivation::checkInvariants` /
/// `makeFixedOutputPath`): a fixed output can only ever live at the
/// store path derived from its declared `(method, algo, hash)`. CppNix
/// enforces this structurally — the parsed `CAFixed` output stores only
/// the content address and every consumer recomputes the path — so a
/// `.drv` whose declared path disagrees with its hash simply cannot
/// place content there. rio keeps the declared path as the working path
/// through planning, verification and upload, so the equivalent
/// guarantee is to reject any disagreement here, before the build
/// exists anywhere. The store-path *name* is CppNix's
/// `outputPathName(drvName, outputName)` (the derivation name for
/// `out`, `<drvName>-<outputName>` otherwise), with the derivation name
/// taken from the `.drv` store path minus its `.drv` suffix
/// (`drvPathToName`) — the same source CppNix uses for derivations
/// loaded from the store.
///
/// Outputs that declare an algo but no hash (floating-CA / impure) and
/// plain input-addressed outputs are not subject to this rule.
fn validate_fixed_output_declarations(
    drv_path: &str,
    drv: &BasicDerivation,
) -> Result<(), GlueError> {
    use rio_nix::hash::{HashAlgo, NixHash};

    let fixed_count = drv
        .outputs()
        .iter()
        .filter(|o| !o.hash_algo().is_empty() && !o.hash().is_empty())
        .count();
    if fixed_count == 0 {
        return Ok(());
    }

    // Shape rule — CppNix `BasicDerivation::type()`: a derivation with
    // a fixed output must consist of exactly that one output, and it
    // must be named `out`. After this check the strict
    // `DerivationLike::is_fixed_output()` predicate is true iff *any*
    // output declares a hash, so hash verification, the no-references
    // rule, descriptor stamping and fetcher routing all see the same
    // set of derivations — no hash-declaring shape can reach upload
    // unverified.
    if drv.outputs().len() != 1 {
        let reason = if fixed_count == drv.outputs().len() {
            "only one fixed output is allowed".to_owned()
        } else {
            "fixed-output and non-fixed outputs cannot be mixed in one derivation".to_owned()
        };
        return Err(GlueError::FixedOutputBadShape { reason });
    }
    if drv.outputs()[0].name() != "out" {
        return Err(GlueError::FixedOutputBadShape {
            reason: format!(
                "the single fixed output must be named \"out\", not \"{}\"",
                drv.outputs()[0].name()
            ),
        });
    }

    let drv_sp =
        store_path::StorePath::parse(drv_path).map_err(|e| GlueError::BadDerivationPath {
            path: drv_path.to_owned(),
            message: e.to_string(),
        })?;
    let drv_name = drv_sp
        .name()
        .strip_suffix(".drv")
        .unwrap_or_else(|| drv_sp.name());

    for o in drv
        .outputs()
        .iter()
        .filter(|o| !o.hash_algo().is_empty() && !o.hash().is_empty())
    {
        let raw_algo = o.hash_algo();
        let (recursive, algo_str) = match raw_algo.strip_prefix("r:") {
            Some(rest) => (true, rest),
            None => (false, raw_algo),
        };
        let algo: HashAlgo = algo_str
            .parse()
            .map_err(|_| GlueError::FixedOutputHashInvalid {
                output: o.name().to_owned(),
                message: format!("unsupported outputHashAlgo '{raw_algo}'"),
            })?;
        // Length-discriminated decode (base16 / nixbase32 / base64) — the
        // shared CppNix-parity parser. Defense-in-depth: the gateway gate
        // already decoded the same declaration with the same function.
        // r[impl nix.hash.fod-decode]
        let hash = NixHash::parse_nonsri_unprefixed(algo, o.hash()).map_err(|e| {
            GlueError::FixedOutputHashInvalid {
                output: o.name().to_owned(),
                message: format!(
                    "outputHash is not a valid base16, nixbase32, or base64 hash: {e}"
                ),
            }
        })?;
        let path_name = if o.name() == "out" {
            drv_name.to_owned()
        } else {
            format!("{drv_name}-{}", o.name())
        };
        let expected = store_path::StorePath::make_fixed_output(&path_name, &hash, recursive, &[])
            .map_err(|e| GlueError::FixedOutputHashInvalid {
                output: o.name().to_owned(),
                message: e.to_string(),
            })?;
        if expected.as_str() != o.path() {
            return Err(GlueError::FixedOutputPathMismatch {
                output: o.name().to_owned(),
                declared: o.path().to_owned(),
                expected: expected.as_str().to_owned(),
            });
        }
    }
    Ok(())
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
        let (path, basename, floating) = if !o.path().is_empty() {
            // Declared (input-addressed / fixed-output) path: must parse
            // as a store path. Defense-in-depth under
            // sec.trust.workers-untrusted — the gateway rejects these at
            // submission (gw.reject.output-path-mismatch+2) and the
            // scheduler shape-checks expected_output_paths; a malformed
            // declaration reaching this point means both gates were
            // bypassed. The basename it yields feeds host-side
            // filesystem joins, so it is computed from the PARSED path
            // only.
            // r[impl builder.exec.declared-path-validated]
            let sp = store_path::StorePath::parse(o.path()).map_err(|e| {
                GlueError::MalformedOutputPath {
                    output: o.name().to_owned(),
                    path: o.path().to_owned(),
                    message: e.to_string(),
                }
            })?;
            (o.path().to_owned(), sp.basename().to_owned(), false)
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
            let scratch_basename = scratch.basename().to_owned();
            (scratch.as_str().to_owned(), scratch_basename, true)
        } else {
            return Err(GlueError::MissingOutputPath {
                output: o.name().to_owned(),
            });
        };
        rewrites.insert(hash_placeholder(o.name()), path.clone());
        outputs.push(PlannedOutput {
            name: o.name().to_owned(),
            path,
            basename,
            floating_ca: floating,
        });
    }
    Ok((rewrites, outputs))
}

/// The Nix sandbox build user's login/group name, as CppNix synthesizes
/// it inside its sandbox (`linux-derivation-builder.cc`). Observable via
/// `whoami` / `id` and baked into some outputs, so it is part of the
/// de-facto Nix sandbox ABI — the differential harness byte-compares
/// outputs that embed it.
const NIX_BUILD_USER: &str = "nixbld";

/// The GECOS field CppNix uses for the build user's passwd entry.
const NIX_BUILD_GECOS: &str = "Nix build user";

/// The single construction point for the Nix sandbox identity.
///
/// rio-exec deliberately has no default identity (its boundary rule
/// bans Nix conventions); every Nix-flavoured request gets its
/// passwd/group names from HERE, so the value the differential corpus
/// pins (`build-user` / `sandbox-identity` entries) cannot drift
/// between the generic and builtin request paths.
// r[impl builder.sandbox.identity]
fn nix_sandbox_identity() -> rio_exec::SandboxIdentity {
    rio_exec::SandboxIdentity {
        user: NIX_BUILD_USER.to_owned(),
        group: NIX_BUILD_USER.to_owned(),
        gecos: NIX_BUILD_GECOS.to_owned(),
    }
}

/// 32-bit personality selection: building a 32-bit system on its 64-bit
/// host needs `PER_LINUX32` so `uname -m` inside the sandbox reports the
/// 32-bit machine.
// r[impl builder.platform.i686+2]
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

    /// The store path a fixed output of `DRV` (derivation name "demo")
    /// must declare for the given outputHashAlgo and base16 hash — i.e.
    /// the path `validate_fixed_output_declarations` derives.
    fn fod_path(algo: &str, hash_hex: &str) -> String {
        let (recursive, plain) = match algo.strip_prefix("r:") {
            Some(rest) => (true, rest),
            None => (false, algo),
        };
        let hash = rio_nix::hash::NixHash::new(
            plain.parse::<rio_nix::hash::HashAlgo>().unwrap(),
            hex::decode(hash_hex).unwrap(),
        )
        .unwrap();
        StorePath::make_fixed_output("demo", &hash, recursive, &[])
            .unwrap()
            .as_str()
            .to_owned()
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
                .any(|m| m.target == Path::new(crate::builtin_fetchurl::SANDBOX_CA_BUNDLE))
        );

        // Isolation/limits.
        assert_eq!(req.isolation.uid, 1000);
        assert_eq!(req.isolation.gid, 100);
        assert_eq!(req.isolation.identity, nix_sandbox_identity());
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
        let zeros = "00".repeat(32);
        let out = fod_path("sha256", &zeros);
        let fod = mk_drv(
            vec![DerivationOutput::new("out", out.as_str(), "sha256", zeros.as_str()).unwrap()],
            &[
                ("name", "demo"),
                ("out", out.as_str()),
                ("outputHashMode", "flat"),
            ],
        );
        let prepared = prepare(&fod);
        assert!(prepared.request.isolation.network);
        assert!(prepared.request.mounts.iter().any(|m| m.target
            == Path::new(crate::builtin_fetchurl::SANDBOX_CA_BUNDLE)
            && m.optional));
    }

    /// The Nix sandbox identity is the CppNix convention, constructed
    /// in exactly one place. The names and GECOS here are the de-facto
    /// sandbox ABI (`whoami`, perl Config.pm, "built by" banners) that
    /// the differential corpus' build-user/sandbox-identity entries
    /// byte-compare against the oracle — this test pins the values at
    /// the source so a corpus failure can only mean plumbing, never a
    /// renamed constant.
    // r[verify builder.sandbox.identity]
    #[test]
    fn sandbox_identity_is_cppnix_parity() {
        let id = nix_sandbox_identity();
        assert_eq!(id.user, "nixbld");
        assert_eq!(id.group, "nixbld");
        assert_eq!(id.gecos, "Nix build user");
    }

    /// The writer half of the CA-bundle contract: the glue's mount
    /// targets exactly the path the in-sandbox reader
    /// (`builtin_fetchurl`) opens — the shared
    /// [`crate::builtin_fetchurl::SANDBOX_CA_BUNDLE`] symbol — read-only
    /// and optional-if-missing. A drift between writer and reader can
    /// now only happen by editing the one constant both sides import.
    #[test]
    fn ca_bundle_mount_targets_the_readers_path() {
        let zeros = "00".repeat(32);
        let out = fod_path("sha256", &zeros);
        let fod = mk_drv(
            vec![DerivationOutput::new("out", out.as_str(), "sha256", zeros.as_str()).unwrap()],
            &[
                ("name", "demo"),
                ("out", out.as_str()),
                ("outputHashMode", "flat"),
            ],
        );
        let prepared = prepare(&fod);
        let ca = prepared
            .request
            .mounts
            .iter()
            .find(|m| m.target == Path::new(crate::builtin_fetchurl::SANDBOX_CA_BUNDLE))
            .expect("FOD request must mount the CA bundle at the reader's path");
        assert!(!ca.writable, "CA bundle mount must be read-only");
        assert!(
            ca.optional,
            "CA bundle mount must be optional so a bundle-less worker image still builds"
        );
    }

    #[test]
    fn fixed_output_declared_path_must_match_the_declared_hash() {
        // The attack shape: a well-formed store path that belongs to
        // some other content, declared together with the hash of
        // attacker-chosen bytes. Must be rejected before any planning,
        // for both ingestion methods.
        let zeros = "00".repeat(32);
        for algo in ["sha256", "r:sha256"] {
            let drv = mk_drv(
                vec![DerivationOutput::new("out", OUT, algo, zeros.as_str()).unwrap()],
                &[("name", "demo"), ("out", OUT)],
            );
            let (input_paths, input_meta) = closure();
            let err =
                derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
                    .unwrap_err();
            match err {
                GlueError::FixedOutputPathMismatch {
                    declared, expected, ..
                } => {
                    assert_eq!(declared, OUT);
                    assert_eq!(expected, fod_path(algo, &zeros));
                }
                other => panic!("want FixedOutputPathMismatch, got: {other}"),
            }
        }
    }

    #[test]
    fn fixed_output_with_consistent_path_is_accepted() {
        let zeros = "00".repeat(32);
        for algo in ["sha256", "r:sha256"] {
            let out = fod_path(algo, &zeros);
            let drv = mk_drv(
                vec![DerivationOutput::new("out", out.as_str(), algo, zeros.as_str()).unwrap()],
                &[("name", "demo"), ("out", out.as_str())],
            );
            let prepared = prepare(&drv);
            assert_eq!(prepared.outputs[0].path, out, "algo {algo}");
        }
    }

    /// CppNix accepts `outputHash` in nixbase32 (and base64) too, length-
    /// discriminated: the same digest in another encoding plans to the same
    /// derived output path. (The full three-encoding matrix is pinned in
    /// rio-nix's own tests; base64 is omitted here so rio-builder needs no
    /// base64 dependency.)
    // r[verify nix.hash.fod-decode]
    #[test]
    fn fixed_output_nixbase32_declaration_is_accepted() {
        let digest = vec![0u8; 32];
        let zeros_hex = "00".repeat(32);
        let out = fod_path("sha256", &zeros_hex);
        let declared = rio_nix::store_path::nixbase32::encode(&digest);
        let drv = mk_drv(
            vec![DerivationOutput::new("out", out.as_str(), "sha256", declared.as_str()).unwrap()],
            &[("name", "demo"), ("out", out.as_str())],
        );
        let prepared = prepare(&drv);
        assert_eq!(
            prepared.outputs[0].path, out,
            "declared encoding {declared:?} must plan to the canonical path"
        );
    }

    #[test]
    fn fixed_output_with_undecodable_hash_is_rejected() {
        let drv = mk_drv(
            vec![DerivationOutput::new("out", OUT, "sha256", "zz").unwrap()],
            &[("name", "demo"), ("out", OUT)],
        );
        let (input_paths, input_meta) = closure();
        let err = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap_err();
        assert!(
            matches!(err, GlueError::FixedOutputHashInvalid { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn fixed_output_with_sibling_output_is_rejected() {
        // CppNix: "can't mix derivation output types".
        let zeros = "00".repeat(32);
        let out = fod_path("sha256", &zeros);
        let drv = mk_drv(
            vec![
                DerivationOutput::new("out", out.as_str(), "sha256", zeros.as_str()).unwrap(),
                DerivationOutput::new("doc", IN_DEP, "", "").unwrap(),
            ],
            &[("name", "demo")],
        );
        let (input_paths, input_meta) = closure();
        let err = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap_err();
        match err {
            GlueError::FixedOutputBadShape { reason } => {
                assert!(reason.contains("cannot be mixed"), "{reason}")
            }
            other => panic!("want FixedOutputBadShape, got: {other}"),
        }
    }

    /// CppNix `BasicDerivation::type()`: a floating-CA output cannot
    /// coexist with any other output kind ("can't mix derivation output
    /// types"). The glue rejects the shape before any planning, on every
    /// path (sandbox, builtin, differential).
    // r[verify builder.exec.output-types-unmixed]
    #[test]
    fn derivation_into_request_rejects_mixed_output_types() {
        let (input_paths, input_meta) = closure();

        // Floating-CA + declared-path IA sibling → rejected.
        let mixed = mk_drv(
            vec![
                DerivationOutput::new("out", "", "r:sha256", "").unwrap(),
                DerivationOutput::new("lib", OUT, "", "").unwrap(),
            ],
            &[
                ("name", "demo"),
                ("out", &hash_placeholder("out")),
                ("lib", OUT),
            ],
        );
        let err =
            derivation_into_request(DRV, &mixed, &input_paths, &input_meta, &paths(), &opts())
                .unwrap_err();
        assert!(
            matches!(err, GlueError::MixedOutputTypes { .. }),
            "mixed floating-CA + IA must be rejected, got: {err}"
        );
        assert!(
            err.to_string()
                .contains("can't mix derivation output types"),
            "error carries the oracle's wording: {err}"
        );

        // All-floating still plans (two floating-CA outputs).
        let all_floating = mk_drv(
            vec![
                DerivationOutput::new("out", "", "r:sha256", "").unwrap(),
                DerivationOutput::new("doc", "", "r:sha256", "").unwrap(),
            ],
            &[
                ("name", "demo"),
                ("out", &hash_placeholder("out")),
                ("doc", &hash_placeholder("doc")),
            ],
        );
        assert!(
            derivation_into_request(
                DRV,
                &all_floating,
                &input_paths,
                &input_meta,
                &paths(),
                &opts()
            )
            .is_ok(),
            "all-floating-CA derivations keep planning"
        );

        // Single FOD still plans (fixed-output is its own legal type).
        let zeros = "00".repeat(32);
        let fod_out = fod_path("sha256", &zeros);
        let fod = mk_drv(
            vec![DerivationOutput::new("out", fod_out.as_str(), "sha256", zeros.as_str()).unwrap()],
            &[("name", "demo"), ("out", fod_out.as_str())],
        );
        assert!(
            derivation_into_request(DRV, &fod, &input_paths, &input_meta, &paths(), &opts())
                .is_ok(),
            "single fixed-output derivations keep planning"
        );
    }

    #[test]
    fn multiple_fixed_outputs_are_rejected() {
        // CppNix: "only one fixed output is allowed".
        let zeros = "00".repeat(32);
        let ones = "11".repeat(32);
        let drv = mk_drv(
            vec![
                DerivationOutput::new(
                    "out",
                    fod_path("sha256", &zeros).as_str(),
                    "sha256",
                    zeros.as_str(),
                )
                .unwrap(),
                DerivationOutput::new(
                    "lib",
                    fod_path("sha256", &ones).as_str(),
                    "sha256",
                    ones.as_str(),
                )
                .unwrap(),
            ],
            &[("name", "demo")],
        );
        let (input_paths, input_meta) = closure();
        let err = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap_err();
        match err {
            GlueError::FixedOutputBadShape { reason } => {
                assert!(reason.contains("only one fixed output"), "{reason}")
            }
            other => panic!("want FixedOutputBadShape, got: {other}"),
        }
    }

    #[test]
    fn fixed_output_not_named_out_is_rejected() {
        // CppNix: "single fixed output must be named \"out\"".
        let zeros = "00".repeat(32);
        let drv = mk_drv(
            vec![DerivationOutput::new("lib", OUT, "sha256", zeros.as_str()).unwrap()],
            &[("name", "demo")],
        );
        let (input_paths, input_meta) = closure();
        let err = derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
            .unwrap_err();
        match err {
            GlueError::FixedOutputBadShape { reason } => {
                assert!(reason.contains("named \"out\""), "{reason}")
            }
            other => panic!("want FixedOutputBadShape, got: {other}"),
        }
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

    /// Non-store-path declared output paths are rejected at planning time —
    /// these are exactly the shapes that would survive rio-exec's
    /// writable-mount validation but escape the overlay upper store via a
    /// raw-string `Path::join` (an absolute path replaces the join target).
    /// Defense-in-depth; the gateway and scheduler gates are authoritative.
    // r[verify builder.exec.declared-path-validated]
    #[test]
    fn plan_outputs_rejects_malformed_declared_path() {
        for bad in ["/build/exfil", "/nix/store", "/nix/store/zzz-evil"] {
            let drv = mk_drv(
                vec![DerivationOutput::new("out", bad, "", "").unwrap()],
                &[("name", "demo"), ("out", bad)],
            );
            let (input_paths, input_meta) = closure();
            let err =
                derivation_into_request(DRV, &drv, &input_paths, &input_meta, &paths(), &opts())
                    .unwrap_err();
            assert!(
                matches!(err, GlueError::MalformedOutputPath { .. }),
                "path {bad:?} must be rejected as malformed, got: {err}"
            );
        }
    }

    /// Every PlannedOutput's basename comes from the PARSED store path —
    /// declared (IA) and scratch (floating-CA) alike — so host-side joins
    /// never re-derive it from the raw string.
    // r[verify builder.exec.declared-path-validated]
    #[test]
    fn planned_outputs_carry_store_basenames() {
        // Input-addressed: basename of the declared path.
        let prepared = prepare(&ia_drv());
        let out = &prepared.outputs[0];
        assert_eq!(out.basename, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo");
        assert_eq!(format!("/nix/store/{}", out.basename), out.path);

        // Floating-CA: basename of the scratch path.
        let ca = mk_drv(
            vec![DerivationOutput::new("out", "", "r:sha256", "").unwrap()],
            &[("name", "demo"), ("out", &hash_placeholder("out"))],
        );
        let prepared_ca = prepare(&ca);
        let ca_out = &prepared_ca.outputs[0];
        assert_eq!(format!("/nix/store/{}", ca_out.basename), ca_out.path);
        assert!(ca_out.basename.ends_with("-demo"));

        // Fixed-output: basename of the declared (hash-derived) path.
        let zeros = "00".repeat(32);
        let fod_out = fod_path("sha256", &zeros);
        let fod = mk_drv(
            vec![DerivationOutput::new("out", fod_out.as_str(), "sha256", zeros.as_str()).unwrap()],
            &[("name", "demo"), ("out", fod_out.as_str())],
        );
        let prepared_fod = prepare(&fod);
        let f_out = &prepared_fod.outputs[0];
        assert_eq!(format!("/nix/store/{}", f_out.basename), f_out.path);
    }

    #[test]
    fn builtin_fetchurl_is_dispatched_not_sandboxed() {
        let zeros = "00".repeat(32);
        let out = fod_path("sha256", &zeros);
        let drv = BasicDerivation::new(
            vec![DerivationOutput::new("out", out.as_str(), "sha256", zeros.as_str()).unwrap()],
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

    // r[verify builder.platform.i686+2]
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
