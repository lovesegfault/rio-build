//! Snapshot guard for `schema_for!(Config)` — see CLAUDE.md "Config
//! schemas are committed snapshots".
rio_test_support::config_schema_frozen!(rio_build_cli::config::Config);
