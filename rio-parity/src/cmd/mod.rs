//! Subcommand implementations. Each submodule owns its clap `Args`
//! struct and `run()` fn so `main.rs` stays a thin dispatcher.

pub mod eval;
