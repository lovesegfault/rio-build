# Version pins moved to ./pins.toml — TOML so both Nix (this shim, via
# builtins.fromTOML) and `cargo xtask regen tfvars` (the toml crate) read
# the same file natively, and regenerating the EKS tfvars no longer needs
# a nix build. Every existing `import ./pins.nix` site keeps working
# through this shim. Edit ./pins.toml, not this file.
builtins.fromTOML (builtins.readFile ./pins.toml)
