//! String-backed newtypes for scheduler identifiers.

rio_common::string_newtype! {
    /// Derivation hash newtype. The 32-char nixbase32 hash part of a .drv store path.
    ///
    /// Distinct from `drv_path` (full `/nix/store/HASH-name.drv` string). Prevents
    /// accidental swaps — see the post-2a `drv_key` rename where `handle_completion`
    /// took a `drv_hash: String` that was sometimes actually a path.
    ///
    /// Implements `Borrow<str>` so `HashMap<DrvHash, _>::get(&str)` works.
    pub struct DrvHash
}

rio_common::string_newtype! {
    /// Worker identifier newtype (e.g., `"worker-0"` or a UUID).
    pub struct WorkerId
}
