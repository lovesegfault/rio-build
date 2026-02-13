//! Nix derivation (`.drv`) ATerm format parser.
//!
//! Handles parsing of `.drv` files, which use a Nix-specific variant of the ATerm format.
//! Needed in Phase 1b for DAG reconstruction from uploaded `.drv` files.

// TODO(Phase 1b): Implement ATerm parser for `.drv` files.
// Key types: Derivation (with inputDrvs, inputSrcs, outputs, env, builder, args, platform).
// See: Nix C++ `BasicDerivation` and `Derivation` in libstore/derivations.{hh,cc}.
