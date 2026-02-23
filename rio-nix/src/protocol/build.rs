//! Wire format types for build operations: `BasicDerivation`, `BuildResult`.
//!
//! `BasicDerivation` is the inline derivation sent by clients in `wopBuildDerivation` (opcode 36).
//! `BuildResult` is returned by `wopBuildDerivation` and `wopBuildPathsWithResults` (opcode 46).

use super::wire::{self, Result};
use crate::derivation::DerivationOutput;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum BuildStatus {
    Built = 0,
    Substituted = 1,
    AlreadyValid = 2,
    PermanentFailure = 3,
    TransientFailure = 4,
    CachedFailure = 5,
    TimedOut = 6,
    MiscFailure = 7,
    DependencyFailed = 8,
    LogLimitExceeded = 9,
    NotDeterministic = 10,
    ResolveFailed = 11,
    NoSubstituters = 12,
}

impl TryFrom<u64> for BuildStatus {
    type Error = u64;
    fn try_from(v: u64) -> std::result::Result<Self, u64> {
        match v {
            0 => Ok(BuildStatus::Built),
            1 => Ok(BuildStatus::Substituted),
            2 => Ok(BuildStatus::AlreadyValid),
            3 => Ok(BuildStatus::PermanentFailure),
            4 => Ok(BuildStatus::TransientFailure),
            5 => Ok(BuildStatus::CachedFailure),
            6 => Ok(BuildStatus::TimedOut),
            7 => Ok(BuildStatus::MiscFailure),
            8 => Ok(BuildStatus::DependencyFailed),
            9 => Ok(BuildStatus::LogLimitExceeded),
            10 => Ok(BuildStatus::NotDeterministic),
            11 => Ok(BuildStatus::ResolveFailed),
            12 => Ok(BuildStatus::NoSubstituters),
            other => Err(other),
        }
    }
}

impl BuildStatus {
    /// Whether this status represents a successful build.
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            BuildStatus::Built | BuildStatus::Substituted | BuildStatus::AlreadyValid
        )
    }
}

/// A built output entry in a `BuildResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltOutput {
    /// Output name (e.g., "out", "dev").
    pub name: String,
    /// Realized output store path.
    pub path: String,
    /// Hash algorithm (for CA outputs, empty for input-addressed).
    pub hash_algo: String,
    /// Output content hash (for CA outputs, empty for input-addressed).
    pub hash: String,
}

/// Result of a derivation build, returned by `wopBuildDerivation` and
/// `wopBuildPathsWithResults`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    /// Build status.
    status: BuildStatus,
    /// Error message (empty on success).
    error_msg: String,
    /// Number of times this derivation was built.
    times_built: u64,
    /// Whether non-deterministic output was detected.
    is_non_deterministic: bool,
    /// Build start time (Unix epoch).
    start_time: u64,
    /// Build stop time (Unix epoch).
    stop_time: u64,
    /// Built output entries.
    built_outputs: Vec<BuiltOutput>,
}

impl BuildResult {
    /// Create a new build result.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        status: BuildStatus,
        error_msg: String,
        times_built: u64,
        is_non_deterministic: bool,
        start_time: u64,
        stop_time: u64,
        built_outputs: Vec<BuiltOutput>,
    ) -> Self {
        BuildResult {
            status,
            error_msg,
            times_built,
            is_non_deterministic,
            start_time,
            stop_time,
            built_outputs,
        }
    }

    /// Create a simple success result with no outputs.
    pub fn success() -> Self {
        Self::new(
            BuildStatus::Built,
            String::new(),
            1,
            false,
            0,
            0,
            Vec::new(),
        )
    }

    /// Create a failure result.
    pub fn failure(status: BuildStatus, error_msg: impl Into<String>) -> Self {
        Self::new(status, error_msg.into(), 0, false, 0, 0, Vec::new())
    }

    pub fn status(&self) -> BuildStatus {
        self.status
    }
    pub fn error_msg(&self) -> &str {
        &self.error_msg
    }
    pub fn times_built(&self) -> u64 {
        self.times_built
    }
    pub fn is_non_deterministic(&self) -> bool {
        self.is_non_deterministic
    }
    pub fn start_time(&self) -> u64 {
        self.start_time
    }
    pub fn stop_time(&self) -> u64 {
        self.stop_time
    }
    pub fn built_outputs(&self) -> &[BuiltOutput] {
        &self.built_outputs
    }
}

// ---------------------------------------------------------------------------
// Wire format: reading BasicDerivation (client → server)
// ---------------------------------------------------------------------------

/// Read a `BasicDerivation` from the wire (opcode 36 payload).
///
/// Wire format for protocol 1.37+:
/// - outputs: count + per-output (name, path, hashAlgo, hash)
/// - inputSrcs: string collection
/// - platform: string
/// - builder: string
/// - args: string collection
/// - env: string-pair collection
pub async fn read_basic_derivation<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(
    Vec<DerivationOutput>,
    Vec<String>,
    String,
    String,
    Vec<String>,
    Vec<(String, String)>,
)> {
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
        outputs.push(DerivationOutput::new(name, path, hash_algo, hash));
    }

    let input_srcs = wire::read_strings(r).await?;
    let platform = wire::read_string(r).await?;
    let builder = wire::read_string(r).await?;
    let args = wire::read_strings(r).await?;
    let env = wire::read_string_pairs(r).await?;

    Ok((outputs, input_srcs, platform, builder, args, env))
}

/// Write a `BasicDerivation` to the wire (client → server for local daemon).
pub async fn write_basic_derivation<W: AsyncWrite + Unpin>(
    w: &mut W,
    outputs: &[DerivationOutput],
    input_srcs: &[String],
    platform: &str,
    builder: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<()> {
    wire::write_u64(w, outputs.len() as u64).await?;
    for output in outputs {
        wire::write_string(w, output.name()).await?;
        wire::write_string(w, output.path()).await?;
        wire::write_string(w, output.hash_algo()).await?;
        wire::write_string(w, output.hash()).await?;
    }

    wire::write_strings(w, input_srcs).await?;
    wire::write_string(w, platform).await?;
    wire::write_string(w, builder).await?;
    wire::write_strings(w, args).await?;
    wire::write_string_pairs(w, env).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Wire format: reading/writing BuildResult
// ---------------------------------------------------------------------------

/// Read a `BuildResult` from the wire (server → client).
pub async fn read_build_result<R: AsyncRead + Unpin>(r: &mut R) -> Result<BuildResult> {
    let status_val = wire::read_u64(r).await?;
    let status = BuildStatus::try_from(status_val).unwrap_or(BuildStatus::MiscFailure);

    let error_msg = wire::read_string(r).await?;

    // Protocol 1.29+
    let times_built = wire::read_u64(r).await?;
    let is_non_deterministic = wire::read_bool(r).await?;
    let start_time = wire::read_u64(r).await?;
    let stop_time = wire::read_u64(r).await?;

    // Protocol 1.28+: builtOutputs
    let output_count = wire::read_u64(r).await?;
    if output_count > wire::MAX_COLLECTION_COUNT {
        return Err(wire::WireError::CollectionTooLarge(output_count));
    }
    let mut built_outputs = Vec::with_capacity(output_count.min(64) as usize);
    for _ in 0..output_count {
        let name = wire::read_string(r).await?;
        let path = wire::read_string(r).await?;
        let hash_algo = wire::read_string(r).await?;
        let hash = wire::read_string(r).await?;
        built_outputs.push(BuiltOutput {
            name,
            path,
            hash_algo,
            hash,
        });
    }

    Ok(BuildResult::new(
        status,
        error_msg,
        times_built,
        is_non_deterministic,
        start_time,
        stop_time,
        built_outputs,
    ))
}

/// Write a `BuildResult` to the wire (server → client).
pub async fn write_build_result<W: AsyncWrite + Unpin>(
    w: &mut W,
    result: &BuildResult,
) -> Result<()> {
    wire::write_u64(w, result.status as u64).await?;
    wire::write_string(w, &result.error_msg).await?;

    // Protocol 1.29+
    wire::write_u64(w, result.times_built).await?;
    wire::write_bool(w, result.is_non_deterministic).await?;
    wire::write_u64(w, result.start_time).await?;
    wire::write_u64(w, result.stop_time).await?;

    // Protocol 1.28+: builtOutputs
    wire::write_u64(w, result.built_outputs.len() as u64).await?;
    for output in &result.built_outputs {
        wire::write_string(w, &output.name).await?;
        wire::write_string(w, &output.path).await?;
        wire::write_string(w, &output.hash_algo).await?;
        wire::write_string(w, &output.hash).await?;
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
        for val in 0..=12u64 {
            assert!(BuildStatus::try_from(val).is_ok());
        }
        assert_eq!(BuildStatus::try_from(99), Err(99));
    }

    #[test]
    fn build_status_is_success() {
        assert!(BuildStatus::Built.is_success());
        assert!(BuildStatus::Substituted.is_success());
        assert!(BuildStatus::AlreadyValid.is_success());
        assert!(!BuildStatus::PermanentFailure.is_success());
        assert!(!BuildStatus::TimedOut.is_success());
    }

    #[tokio::test]
    async fn build_result_roundtrip() {
        let result = BuildResult::new(
            BuildStatus::Built,
            String::new(),
            1,
            false,
            1700000000,
            1700000060,
            vec![BuiltOutput {
                name: "out".to_string(),
                path: "/nix/store/abc-hello".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
        );

        let mut buf = Vec::new();
        write_build_result(&mut buf, &result).await.unwrap();

        let mut reader = Cursor::new(buf);
        let parsed = read_build_result(&mut reader).await.unwrap();
        assert_eq!(parsed, result);
    }

    #[tokio::test]
    async fn build_result_failure_roundtrip() {
        let result = BuildResult::failure(BuildStatus::PermanentFailure, "build failed: exit 1");

        let mut buf = Vec::new();
        write_build_result(&mut buf, &result).await.unwrap();

        let mut reader = Cursor::new(buf);
        let parsed = read_build_result(&mut reader).await.unwrap();
        assert_eq!(parsed.status(), BuildStatus::PermanentFailure);
        assert_eq!(parsed.error_msg(), "build failed: exit 1");
        assert!(parsed.built_outputs().is_empty());
    }

    #[tokio::test]
    async fn basic_derivation_roundtrip() {
        let outputs = vec![
            DerivationOutput::new("out", "/nix/store/abc-hello", "", ""),
            DerivationOutput::new("dev", "/nix/store/def-hello-dev", "", ""),
        ];
        let input_srcs = vec!["/nix/store/ghi-source.sh".to_string()];
        let platform = "x86_64-linux";
        let builder = "/nix/store/jkl-bash/bin/bash";
        let args = vec!["-e".to_string(), "script.sh".to_string()];
        let env = vec![
            ("name".to_string(), "hello".to_string()),
            ("system".to_string(), "x86_64-linux".to_string()),
        ];

        let mut buf = Vec::new();
        write_basic_derivation(
            &mut buf,
            &outputs,
            &input_srcs,
            platform,
            builder,
            &args,
            &env,
        )
        .await
        .unwrap();

        let mut reader = Cursor::new(buf);
        let (r_outputs, r_input_srcs, r_platform, r_builder, r_args, r_env) =
            read_basic_derivation(&mut reader).await.unwrap();

        assert_eq!(r_outputs, outputs);
        assert_eq!(r_input_srcs, input_srcs);
        assert_eq!(r_platform, platform);
        assert_eq!(r_builder, builder);
        assert_eq!(r_args, args);
        assert_eq!(r_env, env);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::io::Cursor;

        fn arb_build_status() -> impl Strategy<Value = BuildStatus> {
            (0u64..=12).prop_map(|v| BuildStatus::try_from(v).unwrap())
        }

        fn arb_built_output() -> impl Strategy<Value = BuiltOutput> {
            (
                "[a-z]{1,8}",
                "/nix/store/[a-z0-9]{32}-[a-z]{1,10}",
                prop_oneof![Just(String::new()), Just("sha256".to_string())],
                prop_oneof![Just(String::new()), "[0-9a-f]{64}"],
            )
                .prop_map(|(name, path, hash_algo, hash)| BuiltOutput {
                    name,
                    path,
                    hash_algo,
                    hash,
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
                proptest::collection::vec(arb_built_output(), 0..4),
            )
                .prop_map(
                    |(
                        status,
                        error_msg,
                        times_built,
                        is_non_deterministic,
                        start,
                        stop,
                        outputs,
                    )| {
                        BuildResult::new(
                            status,
                            error_msg,
                            times_built,
                            is_non_deterministic,
                            start,
                            stop,
                            outputs,
                        )
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
                })
        }

        proptest! {
            #[test]
            fn build_result_roundtrip(result in arb_build_result()) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_build_result(&mut buf, &result).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let parsed = read_build_result(&mut reader).await.unwrap();
                    prop_assert_eq!(parsed, result);
                    Ok(())
                })?;
            }

            #[test]
            fn basic_derivation_roundtrip(
                outputs in proptest::collection::vec(arb_derivation_output(), 1..5),
                input_srcs in proptest::collection::vec("/nix/store/[a-z0-9]{32}-[a-z]{1,8}", 0..3),
                platform in "(x86_64|aarch64)-linux",
                builder in "/nix/store/[a-z0-9]{32}-bash/bin/bash",
                args in proptest::collection::vec("[a-zA-Z0-9 -]{0,10}", 0..4),
                env in proptest::collection::vec(
                    ("[a-zA-Z_]{1,10}", "[a-zA-Z0-9 /_.-]{0,20}"),
                    0..5
                ),
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_basic_derivation(&mut buf, &outputs, &input_srcs, &platform, &builder, &args, &env).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let (r_outputs, r_input_srcs, r_platform, r_builder, r_args, r_env) =
                        read_basic_derivation(&mut reader).await.unwrap();
                    prop_assert_eq!(r_outputs, outputs);
                    prop_assert_eq!(r_input_srcs, input_srcs);
                    prop_assert_eq!(r_platform, platform);
                    prop_assert_eq!(r_builder, builder);
                    prop_assert_eq!(r_args, args);
                    prop_assert_eq!(r_env, env);
                    Ok(())
                })?;
            }
        }
    }
}
