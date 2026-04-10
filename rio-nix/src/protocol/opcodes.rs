//! Nix worker protocol opcode definitions.

wire_enum! {
    /// Worker protocol opcodes.
    ///
    /// These values are the u64 opcode numbers sent by the Nix client
    /// after the handshake completes.
    pub enum WorkerOp {
        IsValidPath = 1 => "wopIsValidPath",
        AddToStore = 7 => "wopAddToStore",
        AddTextToStore = 8 => "wopAddTextToStore",
        BuildPaths = 9 => "wopBuildPaths",
        EnsurePath = 10 => "wopEnsurePath",
        AddTempRoot = 11 => "wopAddTempRoot",
        SetOptions = 19 => "wopSetOptions",
        QueryPathInfo = 26 => "wopQueryPathInfo",
        QueryPathFromHashPart = 29 => "wopQueryPathFromHashPart",
        QueryValidPaths = 31 => "wopQueryValidPaths",
        BuildDerivation = 36 => "wopBuildDerivation",
        AddSignatures = 37 => "wopAddSignatures",
        NarFromPath = 38 => "wopNarFromPath",
        AddToStoreNar = 39 => "wopAddToStoreNar",
        QueryMissing = 40 => "wopQueryMissing",
        QueryDerivationOutputMap = 41 => "wopQueryDerivationOutputMap",
        RegisterDrvOutput = 42 => "wopRegisterDrvOutput",
        QueryRealisation = 43 => "wopQueryRealisation",
        AddMultipleToStore = 44 => "wopAddMultipleToStore",
        BuildPathsWithResults = 46 => "wopBuildPathsWithResults",
    }
}

impl WorkerOp {
    /// Try to parse a u64 opcode number into a known `WorkerOp`.
    pub fn from_u64(v: u64) -> Option<Self> {
        Self::try_from(v).ok()
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
            WorkerOp::AddToStore,
            WorkerOp::AddTextToStore,
            WorkerOp::BuildPaths,
            WorkerOp::EnsurePath,
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
}
