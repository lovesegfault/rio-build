//! k8s/nix interop helpers shared across the workspace.

/// Map a single nix `system` (e.g. `"x86_64-linux"`) to its
/// `kubernetes.io/arch` label value. `None` for empty/`builtin`/
/// unknown — caller treats an unmappable system as undroppable (no node
/// can host it). Same arch table as the per-Pool
/// `nix_systems_to_k8s_arch` (I-098); single-string here because
/// `SpawnIntent.system` is scalar. Shared by the controller's FFD
/// `agnostic_arch` path and the scheduler's bypass-path `--capacity`
/// arch-match.
pub fn system_to_k8s_arch(system: &str) -> Option<&'static str> {
    match system.split_once('-').map_or(system, |(a, _)| a) {
        "x86_64" | "i686" => Some("amd64"),
        "aarch64" | "armv7l" | "armv6l" => Some("arm64"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_to_arch_mapping() {
        assert_eq!(system_to_k8s_arch("x86_64-linux"), Some("amd64"));
        assert_eq!(system_to_k8s_arch("i686-linux"), Some("amd64"));
        assert_eq!(system_to_k8s_arch("aarch64-linux"), Some("arm64"));
        assert_eq!(system_to_k8s_arch("armv7l-linux"), Some("arm64"));
        assert_eq!(system_to_k8s_arch("builtin"), None);
        assert_eq!(system_to_k8s_arch(""), None);
        assert_eq!(system_to_k8s_arch("riscv64-linux"), None);
    }
}
