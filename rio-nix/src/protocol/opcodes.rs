//! Nix worker protocol opcode definitions.

/// Worker protocol opcodes.
///
/// These values are the u64 opcode numbers sent by the Nix client
/// after the handshake completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum WorkerOp {
    IsValidPath = 1,
    BuildPaths = 9,
    AddTempRoot = 11,
    SetOptions = 19,
    QueryPathInfo = 26,
    QueryPathFromHashPart = 29,
    QueryValidPaths = 31,
    BuildDerivation = 36,
    AddSignatures = 37,
    NarFromPath = 38,
    AddToStoreNar = 39,
    QueryMissing = 40,
    QueryDerivationOutputMap = 41,
    RegisterDrvOutput = 42,
    QueryRealisation = 43,
    AddMultipleToStore = 44,
    BuildPathsWithResults = 46,
}

impl WorkerOp {
    /// Try to parse a u64 opcode number into a known `WorkerOp`.
    pub fn from_u64(v: u64) -> Option<Self> {
        match v {
            1 => Some(WorkerOp::IsValidPath),
            9 => Some(WorkerOp::BuildPaths),
            11 => Some(WorkerOp::AddTempRoot),
            19 => Some(WorkerOp::SetOptions),
            26 => Some(WorkerOp::QueryPathInfo),
            29 => Some(WorkerOp::QueryPathFromHashPart),
            31 => Some(WorkerOp::QueryValidPaths),
            36 => Some(WorkerOp::BuildDerivation),
            37 => Some(WorkerOp::AddSignatures),
            38 => Some(WorkerOp::NarFromPath),
            39 => Some(WorkerOp::AddToStoreNar),
            40 => Some(WorkerOp::QueryMissing),
            41 => Some(WorkerOp::QueryDerivationOutputMap),
            42 => Some(WorkerOp::RegisterDrvOutput),
            43 => Some(WorkerOp::QueryRealisation),
            44 => Some(WorkerOp::AddMultipleToStore),
            46 => Some(WorkerOp::BuildPathsWithResults),
            _ => None,
        }
    }

    /// Return the human-readable name of this opcode.
    pub fn name(&self) -> &'static str {
        match self {
            WorkerOp::IsValidPath => "wopIsValidPath",
            WorkerOp::BuildPaths => "wopBuildPaths",
            WorkerOp::AddTempRoot => "wopAddTempRoot",
            WorkerOp::SetOptions => "wopSetOptions",
            WorkerOp::QueryPathInfo => "wopQueryPathInfo",
            WorkerOp::QueryPathFromHashPart => "wopQueryPathFromHashPart",
            WorkerOp::QueryValidPaths => "wopQueryValidPaths",
            WorkerOp::BuildDerivation => "wopBuildDerivation",
            WorkerOp::AddSignatures => "wopAddSignatures",
            WorkerOp::NarFromPath => "wopNarFromPath",
            WorkerOp::AddToStoreNar => "wopAddToStoreNar",
            WorkerOp::QueryMissing => "wopQueryMissing",
            WorkerOp::QueryDerivationOutputMap => "wopQueryDerivationOutputMap",
            WorkerOp::RegisterDrvOutput => "wopRegisterDrvOutput",
            WorkerOp::QueryRealisation => "wopQueryRealisation",
            WorkerOp::AddMultipleToStore => "wopAddMultipleToStore",
            WorkerOp::BuildPathsWithResults => "wopBuildPathsWithResults",
        }
    }

    /// Whether this opcode is implemented in Phase 1a (read-only + stubs).
    pub fn is_phase1a(&self) -> bool {
        matches!(
            self,
            WorkerOp::IsValidPath
                | WorkerOp::AddTempRoot
                | WorkerOp::SetOptions
                | WorkerOp::QueryPathInfo
                | WorkerOp::QueryPathFromHashPart
                | WorkerOp::QueryValidPaths
                | WorkerOp::AddSignatures
                | WorkerOp::NarFromPath
                | WorkerOp::QueryMissing
        )
    }
}

impl std::fmt::Display for WorkerOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name(), *self as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_roundtrip() {
        let ops = [
            WorkerOp::IsValidPath,
            WorkerOp::BuildPaths,
            WorkerOp::AddTempRoot,
            WorkerOp::SetOptions,
            WorkerOp::QueryPathInfo,
            WorkerOp::QueryPathFromHashPart,
            WorkerOp::QueryValidPaths,
            WorkerOp::BuildDerivation,
            WorkerOp::AddSignatures,
            WorkerOp::NarFromPath,
            WorkerOp::AddToStoreNar,
            WorkerOp::QueryMissing,
            WorkerOp::QueryDerivationOutputMap,
            WorkerOp::RegisterDrvOutput,
            WorkerOp::QueryRealisation,
            WorkerOp::AddMultipleToStore,
            WorkerOp::BuildPathsWithResults,
        ];

        for op in ops {
            let val = op as u64;
            assert_eq!(WorkerOp::from_u64(val), Some(op));
        }
    }

    #[test]
    fn test_unknown_opcode() {
        assert_eq!(WorkerOp::from_u64(0), None);
        assert_eq!(WorkerOp::from_u64(999), None);
        assert_eq!(WorkerOp::from_u64(2), None); // obsolete HasPathHash
    }

    #[test]
    fn test_phase1a_opcodes() {
        assert!(WorkerOp::IsValidPath.is_phase1a());
        assert!(WorkerOp::SetOptions.is_phase1a());
        assert!(WorkerOp::QueryPathInfo.is_phase1a());
        assert!(WorkerOp::QueryValidPaths.is_phase1a());
        assert!(WorkerOp::AddTempRoot.is_phase1a());
        assert!(WorkerOp::NarFromPath.is_phase1a());

        assert!(!WorkerOp::BuildDerivation.is_phase1a());
        assert!(!WorkerOp::BuildPathsWithResults.is_phase1a());
        assert!(!WorkerOp::AddToStoreNar.is_phase1a());
    }
}
