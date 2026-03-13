//! Wire format types for build operations: `BasicDerivation`, `BuildResult`.
//!
//! `BasicDerivation` is the inline derivation sent by clients in `wopBuildDerivation` (opcode 36).
//! `BuildResult` is returned by `wopBuildDerivation` and `wopBuildPathsWithResults` (opcode 46).

use super::wire::{self, Result};
use crate::derivation::{BasicDerivation, DerivationOutput};
use tokio::io::{AsyncRead, AsyncWrite};

/// Build mode for derivation builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum BuildMode {
    Normal = 0,
    Repair = 1,
    Check = 2,
}

impl TryFrom<u64> for BuildMode {
    type Error = u64;
    fn try_from(v: u64) -> std::result::Result<Self, u64> {
        match v {
            0 => Ok(BuildMode::Normal),
            1 => Ok(BuildMode::Repair),
            2 => Ok(BuildMode::Check),
            other => Err(other),
        }
    }
}

/// Build result status codes.
///
/// Wire values match the Nix protocol. Logically u8 range but serialized as u64 on the wire (all Nix wire integers are u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum BuildStatus {
    Built = 0,
    Substituted = 1,
    AlreadyValid = 2,
    PermanentFailure = 3,
    InputRejected = 4,
    OutputRejected = 5,
    TransientFailure = 6,
    CachedFailure = 7,
    TimedOut = 8,
    MiscFailure = 9,
    DependencyFailed = 10,
    LogLimitExceeded = 11,
    NotDeterministic = 12,
    ResolvesToAlreadyValid = 13,
    NoSubstituters = 14,
}

impl TryFrom<u64> for BuildStatus {
    type Error = u64;
    fn try_from(v: u64) -> std::result::Result<Self, u64> {
        match v {
            0 => Ok(BuildStatus::Built),
            1 => Ok(BuildStatus::Substituted),
            2 => Ok(BuildStatus::AlreadyValid),
            3 => Ok(BuildStatus::PermanentFailure),
            4 => Ok(BuildStatus::InputRejected),
            5 => Ok(BuildStatus::OutputRejected),
            6 => Ok(BuildStatus::TransientFailure),
            7 => Ok(BuildStatus::CachedFailure),
            8 => Ok(BuildStatus::TimedOut),
            9 => Ok(BuildStatus::MiscFailure),
            10 => Ok(BuildStatus::DependencyFailed),
            11 => Ok(BuildStatus::LogLimitExceeded),
            12 => Ok(BuildStatus::NotDeterministic),
            13 => Ok(BuildStatus::ResolvesToAlreadyValid),
            14 => Ok(BuildStatus::NoSubstituters),
            other => Err(other),
        }
    }
}

impl BuildStatus {
    /// Whether this status represents a successful build.
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            BuildStatus::Built
                | BuildStatus::Substituted
                | BuildStatus::AlreadyValid
                | BuildStatus::ResolvesToAlreadyValid
        )
    }
}

/// A built output entry in a `BuildResult`.
///
/// On the wire, each entry is a `DrvOutput` key + `Realisation` JSON.
/// Only the `outPath` field is preserved; `signatures` and
/// `dependentRealisations` are discarded on read and synthesized
/// empty on write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltOutput {
    /// DrvOutput ID string (e.g., "sha256:abcdef...!out").
    pub drv_output_id: String,
    /// Realized output store path (e.g., "/nix/store/...").
    pub out_path: String,
}

/// Result of a derivation build, returned by `wopBuildDerivation` and
/// `wopBuildPathsWithResults`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    /// Build status.
    pub status: BuildStatus,
    /// Error message (empty on success).
    pub error_msg: String,
    /// Number of times this derivation was built.
    pub times_built: u64,
    /// Whether non-deterministic output was detected.
    pub is_non_deterministic: bool,
    /// Build start time (Unix epoch).
    pub start_time: u64,
    /// Build stop time (Unix epoch).
    pub stop_time: u64,
    /// CPU user time in microseconds (protocol >= 1.37).
    pub cpu_user: Option<i64>,
    /// CPU system time in microseconds (protocol >= 1.37).
    pub cpu_system: Option<i64>,
    /// Built output entries (DrvOutput → Realisation).
    pub built_outputs: Vec<BuiltOutput>,
}

impl Default for BuildResult {
    fn default() -> Self {
        Self {
            status: BuildStatus::Built,
            error_msg: String::new(),
            times_built: 0,
            is_non_deterministic: false,
            start_time: 0,
            stop_time: 0,
            cpu_user: None,
            cpu_system: None,
            built_outputs: Vec::new(),
        }
    }
}

impl BuildResult {
    /// Create a simple success result with no outputs.
    pub fn success() -> Self {
        Self {
            times_built: 1,
            ..Default::default()
        }
    }

    /// Create a failure result.
    pub fn failure(status: BuildStatus, error_msg: impl Into<String>) -> Self {
        Self {
            status,
            error_msg: error_msg.into(),
            ..Default::default()
        }
    }

    /// Populate built_outputs from derivation output definitions and a
    /// pre-computed modular derivation hash.
    ///
    /// Used when the local daemon returns a success status (e.g., AlreadyValid)
    /// but with empty builtOutputs — the remote client needs the output paths.
    ///
    /// `drv_hash_hex` is the hex-encoded result of `hash_derivation_modulo`.
    pub fn with_outputs_from_drv(
        mut self,
        drv: &crate::derivation::Derivation,
        drv_hash_hex: &str,
    ) -> Self {
        self.built_outputs = drv
            .outputs()
            .iter()
            .map(|output| {
                let drv_output_id = format!("sha256:{drv_hash_hex}!{}", output.name());
                BuiltOutput {
                    drv_output_id,
                    out_path: output.path().to_string(),
                }
            })
            .collect();

        self
    }
}

// ---------------------------------------------------------------------------
// Wire format: reading BasicDerivation (client → server)
// ---------------------------------------------------------------------------

/// Read a `BasicDerivation` from the wire (opcode 36 payload).
///
/// Note: the `drvPath` string is read by the calling opcode handler
/// before this function is invoked. This reads only the BasicDerivation fields.
///
/// Wire format for protocol 1.37+:
/// - outputs: count + per-output (name, path, hashAlgo, hash)
/// - inputSrcs: string collection
/// - platform: string
/// - builder: string
/// - args: string collection
/// - env: string-pair collection
pub async fn read_basic_derivation<R: AsyncRead + Unpin>(r: &mut R) -> Result<BasicDerivation> {
    // outputs
    let output_count = wire::read_u64(r).await?;
    if output_count > wire::MAX_COLLECTION_COUNT {
        return Err(wire::WireError::CollectionTooLarge(output_count));
    }
    let mut outputs = Vec::with_capacity(output_count.min(64) as usize);
    for _ in 0..output_count {
        let name = wire::read_string(r).await?;
        let path = wire::read_string(r).await?;
        let hash_algo = wire::read_string(r).await?;
        let hash = wire::read_string(r).await?;
        outputs.push(
            DerivationOutput::new(name, path, hash_algo, hash).map_err(|e| {
                wire::WireError::Io(std::io::Error::other(format!(
                    "invalid derivation output: {e}"
                )))
            })?,
        );
    }

    let input_srcs = wire::read_strings(r).await?;
    let platform = wire::read_string(r).await?;
    let builder = wire::read_string(r).await?;
    let args = wire::read_strings(r).await?;
    let env = wire::read_string_pairs(r).await?;

    BasicDerivation::new(
        outputs,
        input_srcs.into_iter().collect(),
        platform,
        builder,
        args,
        env.into_iter().collect(),
    )
    .map_err(|e| {
        wire::WireError::Io(std::io::Error::other(format!(
            "invalid BasicDerivation: {e}"
        )))
    })
}

/// Write a `BasicDerivation` to the wire.
pub async fn write_basic_derivation<W: AsyncWrite + Unpin>(
    w: &mut W,
    drv: &BasicDerivation,
) -> Result<()> {
    wire::write_u64(w, drv.outputs().len() as u64).await?;
    for output in drv.outputs() {
        wire::write_string(w, output.name()).await?;
        wire::write_string(w, output.path()).await?;
        wire::write_string(w, output.hash_algo()).await?;
        wire::write_string(w, output.hash()).await?;
    }

    let input_srcs: Vec<&str> = drv.input_srcs().iter().map(String::as_str).collect();
    wire::write_strings(w, &input_srcs).await?;
    wire::write_string(w, drv.platform()).await?;
    wire::write_string(w, drv.builder()).await?;
    wire::write_strings(w, drv.args()).await?;
    let env_pairs: Vec<(&str, &str)> = drv
        .env()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    wire::write_string_pairs(w, &env_pairs).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Wire format: reading/writing BuildResult
// ---------------------------------------------------------------------------

/// Read an optional i64 from the wire (u64 tag + u64 value).
///
/// All Nix wire integers are u64, even for u8/i64 logical types.
async fn read_optional_i64<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<i64>> {
    let tag = wire::read_u64(r).await?;
    match tag {
        0 => Ok(None),
        1 => Ok(Some(wire::read_u64(r).await? as i64)),
        _ => Err(wire::WireError::Io(std::io::Error::other(format!(
            "invalid optional tag: {tag}"
        )))),
    }
}

/// Write an optional i64 to the wire (u64 tag + u64 value).
async fn write_optional_i64<W: AsyncWrite + Unpin>(w: &mut W, val: Option<i64>) -> Result<()> {
    match val {
        None => wire::write_u64(w, 0).await?,
        Some(v) => {
            wire::write_u64(w, 1).await?;
            wire::write_u64(w, v as u64).await?;
        }
    }
    Ok(())
}

/// Read a `BuildResult` from the wire (server → client, protocol >= 1.37).
pub async fn read_build_result<R: AsyncRead + Unpin>(r: &mut R) -> Result<BuildResult> {
    // Status is logically u8 but serialized as u64 (all Nix wire ints are u64)
    let status_val = wire::read_u64(r).await?;
    let status = match BuildStatus::try_from(status_val) {
        Ok(s) => s,
        Err(v) => {
            tracing::warn!(
                status_val = v,
                "unknown BuildStatus from daemon, treating as MiscFailure"
            );
            BuildStatus::MiscFailure
        }
    };

    let error_msg = wire::read_string(r).await?;

    // Protocol 1.29+ (always present since we target 1.37+)
    let times_built = wire::read_u64(r).await?;
    let is_non_deterministic = wire::read_bool(r).await?;
    let start_time = wire::read_u64(r).await?;
    let stop_time = wire::read_u64(r).await?;

    // Protocol 1.37+ (always present since we target 1.37+): CPU time
    let cpu_user = read_optional_i64(r).await?;
    let cpu_system = read_optional_i64(r).await?;

    // Protocol 1.28+ (always present since we target 1.37+): builtOutputs (DrvOutputs map)
    let output_count = wire::read_u64(r).await?;
    if output_count > wire::MAX_COLLECTION_COUNT {
        return Err(wire::WireError::CollectionTooLarge(output_count));
    }
    let mut built_outputs = Vec::with_capacity(output_count.min(64) as usize);
    for _ in 0..output_count {
        // Key: DrvOutput as string ("sha256:<hex>!<outputname>")
        let drv_output_id = wire::read_string(r).await?;
        // Value: Realisation as JSON string
        let json_str = wire::read_string(r).await?;
        // Wire outPath is the BASENAME (<hashpart>-<name>) per Nix's
        // StorePath::to_string(). Prepend /nix/store/ so internal
        // repr is always the full path.
        let out_path = match serde_json::from_str::<serde_json::Value>(&json_str) {
            Ok(v) => match v.get("outPath").and_then(|p| p.as_str()) {
                Some(p) if p.starts_with(crate::store_path::STORE_PREFIX) => p.to_string(),
                Some(p) => format!("{}{p}", crate::store_path::STORE_PREFIX),
                None => {
                    tracing::warn!(json = %json_str, "Realisation JSON missing 'outPath' field");
                    return Err(wire::WireError::Io(std::io::Error::other(format!(
                        "Realisation JSON missing 'outPath': {json_str}"
                    ))));
                }
            },
            Err(e) => {
                tracing::warn!(json = %json_str, error = %e, "failed to parse Realisation JSON");
                return Err(wire::WireError::Io(std::io::Error::other(format!(
                    "invalid Realisation JSON: {e}"
                ))));
            }
        };
        built_outputs.push(BuiltOutput {
            drv_output_id,
            out_path,
        });
    }

    Ok(BuildResult {
        status,
        error_msg,
        times_built,
        is_non_deterministic,
        start_time,
        stop_time,
        cpu_user,
        cpu_system,
        built_outputs,
    })
}

/// Write a `BuildResult` to the wire (server → client, protocol >= 1.37).
pub async fn write_build_result<W: AsyncWrite + Unpin>(
    w: &mut W,
    result: &BuildResult,
) -> Result<()> {
    // Status is logically u8 but serialized as u64 (all Nix wire ints are u64)
    wire::write_u64(w, result.status as u64).await?;
    wire::write_string(w, &result.error_msg).await?;

    // Protocol 1.29+ (always present since we target 1.37+)
    wire::write_u64(w, result.times_built).await?;
    wire::write_bool(w, result.is_non_deterministic).await?;
    wire::write_u64(w, result.start_time).await?;
    wire::write_u64(w, result.stop_time).await?;

    // Protocol 1.37+ (always present since we target 1.37+): CPU time
    write_optional_i64(w, result.cpu_user).await?;
    write_optional_i64(w, result.cpu_system).await?;

    // Protocol 1.28+ (always present since we target 1.37+): builtOutputs (DrvOutputs map)
    wire::write_u64(w, result.built_outputs.len() as u64).await?;
    for output in &result.built_outputs {
        // Key: DrvOutput string
        wire::write_string(w, &output.drv_output_id).await?;
        // Value: Realisation as JSON. outPath is the BASENAME
        // (<hashpart>-<name>), NOT the full /nix/store/ path — Nix's
        // StorePath::to_string() doesn't include the prefix, and
        // Realisation::fromJSON parses via that path. Sending the
        // full path fails client-side with "illegal base-32
        // character '/'".
        let out_path_basename = output
            .out_path
            .strip_prefix(crate::store_path::STORE_PREFIX)
            .unwrap_or(&output.out_path);
        let json = serde_json::json!({
            "id": output.drv_output_id,
            "outPath": out_path_basename,
            "signatures": [],
            "dependentRealisations": {}
        });
        wire::write_string(w, &json.to_string()).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn build_mode_roundtrip() {
        for mode in [BuildMode::Normal, BuildMode::Repair, BuildMode::Check] {
            assert_eq!(BuildMode::try_from(mode as u64), Ok(mode));
        }
        assert_eq!(BuildMode::try_from(99), Err(99));
    }

    #[test]
    fn build_status_roundtrip() {
        for val in 0..=14u64 {
            assert!(BuildStatus::try_from(val).is_ok());
        }
        assert_eq!(BuildStatus::try_from(99u64), Err(99));
    }

    #[test]
    fn build_status_is_success() {
        assert!(BuildStatus::Built.is_success());
        assert!(BuildStatus::Substituted.is_success());
        assert!(BuildStatus::AlreadyValid.is_success());
        assert!(BuildStatus::ResolvesToAlreadyValid.is_success());
        assert!(!BuildStatus::PermanentFailure.is_success());
        assert!(!BuildStatus::TimedOut.is_success());
    }

    #[tokio::test]
    async fn build_result_roundtrip() -> anyhow::Result<()> {
        let result = BuildResult {
            status: BuildStatus::Built,
            times_built: 1,
            start_time: 1700000000,
            stop_time: 1700000060,
            cpu_user: Some(12345),
            cpu_system: Some(6789),
            built_outputs: vec![BuiltOutput {
                drv_output_id: "sha256:abcdef0123456789!out".to_string(),
                out_path: "/nix/store/abc-hello".to_string(),
            }],
            ..Default::default()
        };

        let mut buf = Vec::new();
        write_build_result(&mut buf, &result).await?;

        let mut reader = Cursor::new(buf);
        let parsed = read_build_result(&mut reader).await?;
        assert_eq!(parsed, result);
        Ok(())
    }

    #[tokio::test]
    async fn build_result_failure_roundtrip() -> anyhow::Result<()> {
        let result = BuildResult::failure(BuildStatus::PermanentFailure, "build failed: exit 1");

        let mut buf = Vec::new();
        write_build_result(&mut buf, &result).await?;

        let mut reader = Cursor::new(buf);
        let parsed = read_build_result(&mut reader).await?;
        assert_eq!(parsed.status, BuildStatus::PermanentFailure);
        assert_eq!(parsed.error_msg, "build failed: exit 1");
        assert!(parsed.built_outputs.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn basic_derivation_roundtrip() -> anyhow::Result<()> {
        let outputs = vec![
            DerivationOutput::new("out", "/nix/store/abc-hello", "", "")?,
            DerivationOutput::new("dev", "/nix/store/def-hello-dev", "", "")?,
        ];
        let input_srcs: std::collections::BTreeSet<String> =
            ["/nix/store/ghi-source.sh".to_string()]
                .into_iter()
                .collect();
        let platform = "x86_64-linux".to_string();
        let builder = "/nix/store/jkl-bash/bin/bash".to_string();
        let args = vec!["-e".to_string(), "script.sh".to_string()];
        let env_map: std::collections::BTreeMap<String, String> = [
            ("name".to_string(), "hello".to_string()),
            ("system".to_string(), "x86_64-linux".to_string()),
        ]
        .into_iter()
        .collect();

        let drv = BasicDerivation::new(
            outputs.clone(),
            input_srcs.clone(),
            platform.clone(),
            builder.clone(),
            args.clone(),
            env_map.clone(),
        )?;

        let mut buf = Vec::new();
        write_basic_derivation(&mut buf, &drv).await?;

        let mut reader = Cursor::new(buf);
        let r_drv = read_basic_derivation(&mut reader).await?;

        assert_eq!(r_drv.outputs(), outputs);
        assert_eq!(*r_drv.input_srcs(), input_srcs);
        assert_eq!(r_drv.platform(), platform);
        assert_eq!(r_drv.builder(), builder);
        assert_eq!(r_drv.args(), args);
        assert_eq!(*r_drv.env(), env_map);
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::io::Cursor;

        fn arb_build_status() -> impl Strategy<Value = BuildStatus> {
            (0u64..=14).prop_map(|v| BuildStatus::try_from(v).unwrap())
        }

        fn arb_built_output() -> impl Strategy<Value = BuiltOutput> {
            (
                "[0-9a-f]{64}",
                "[a-z]{1,8}",
                "/nix/store/[a-z0-9]{32}-[a-z]{1,10}",
            )
                .prop_map(|(hash, name, path)| BuiltOutput {
                    drv_output_id: format!("sha256:{hash}!{name}"),
                    out_path: path,
                })
        }

        fn arb_build_result() -> impl Strategy<Value = BuildResult> {
            (
                arb_build_status(),
                "[a-zA-Z0-9 :]{0,50}",
                0u64..100,
                any::<bool>(),
                any::<u64>(),
                any::<u64>(),
                proptest::option::of(any::<i64>()),
                proptest::option::of(any::<i64>()),
                proptest::collection::vec(arb_built_output(), 0..4),
            )
                .prop_map(
                    |(
                        status,
                        error_msg,
                        times_built,
                        is_non_deterministic,
                        start_time,
                        stop_time,
                        cpu_user,
                        cpu_system,
                        built_outputs,
                    )| BuildResult {
                        status,
                        error_msg,
                        times_built,
                        is_non_deterministic,
                        start_time,
                        stop_time,
                        cpu_user,
                        cpu_system,
                        built_outputs,
                    },
                )
        }

        fn arb_derivation_output() -> impl Strategy<Value = DerivationOutput> {
            (
                "[a-z]{1,8}",
                "/nix/store/[a-z0-9]{32}-[a-z]{1,10}",
                prop_oneof![Just(String::new()), Just("sha256".to_string())],
                prop_oneof![Just(String::new()), "[0-9a-f]{64}"],
            )
                .prop_map(|(name, path, hash_algo, hash)| {
                    DerivationOutput::new(name, path, hash_algo, hash)
                        .expect("generated to be valid")
                })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn build_result_roundtrip(result in arb_build_result()) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_build_result(&mut buf, &result).await?;
                    let mut reader = Cursor::new(buf);
                    let parsed = read_build_result(&mut reader).await?;
                    prop_assert_eq!(parsed, result);
                    Ok(())
                })?;
            }

            #[test]
            fn basic_derivation_roundtrip(
                outputs in proptest::collection::vec(arb_derivation_output(), 1..5),
                input_srcs in proptest::collection::btree_set("/nix/store/[a-z0-9]{32}-[a-z]{1,8}", 0..3),
                platform in "(x86_64|aarch64)-linux",
                builder in "/nix/store/[a-z0-9]{32}-bash/bin/bash",
                args in proptest::collection::vec("[a-zA-Z0-9 -]{0,10}", 0..4),
                env_map in proptest::collection::btree_map(
                    "[a-zA-Z_]{1,10}", "[a-zA-Z0-9 /_.-]{0,20}",
                    0..5
                ),
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let drv = BasicDerivation::new(outputs, input_srcs, platform, builder, args, env_map)?;
                    let mut buf = Vec::new();
                    write_basic_derivation(&mut buf, &drv).await?;
                    let mut reader = Cursor::new(buf);
                    let r_drv = read_basic_derivation(&mut reader).await?;
                    prop_assert_eq!(r_drv.outputs(), drv.outputs());
                    prop_assert_eq!(r_drv.input_srcs(), drv.input_srcs());
                    prop_assert_eq!(r_drv.platform(), drv.platform());
                    prop_assert_eq!(r_drv.builder(), drv.builder());
                    prop_assert_eq!(r_drv.args(), drv.args());
                    prop_assert_eq!(r_drv.env(), drv.env());
                    Ok(())
                })?;
            }
        }
    }
}
