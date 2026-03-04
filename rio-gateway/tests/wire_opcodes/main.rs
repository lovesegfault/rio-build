//! Byte-level opcode tests for the rio-gateway Nix worker protocol handler.
//!
//! These tests construct raw wire bytes for each opcode, feed them through
//! `run_protocol` via a DuplexStream, and assert the response bytes match the
//! Nix worker protocol spec. This catches framing and encoding bugs that
//! high-level integration tests hide.
//!
//! Test structure:
//!   - GatewaySession::new_with_handshake: wraps duplex stream + mock gRPC
//!     servers + spawned protocol task, with handshake + setOptions done
//!   - drain_stderr_until_last: consumes STDERR messages until STDERR_LAST
//!   - drain_stderr_expecting_error: consumes STDERR messages expecting STDERR_ERROR
//!   - Per-opcode tests: happy path + error path for each

// tests/common/mod.rs is a sibling directory to tests/wire_opcodes/.
// #[path] reaches it from this binary's main module.
#[path = "../common/mod.rs"]
mod common;
use common::GatewaySession;

use rio_nix::protocol::wire;
use rio_test_support::fixtures::{make_nar, make_path_info};
use rio_test_support::grpc::MockSchedulerOutcome;
use rio_test_support::wire::{drain_stderr_expecting_error, drain_stderr_until_last};
use rio_test_support::{wire_bytes, wire_send};

/// A valid-looking Nix store path (32-char nixbase32 hash + name).
const TEST_PATH_A: &str = "/nix/store/00000000000000000000000000000000-test-a";
const TEST_PATH_MISSING: &str = "/nix/store/11111111111111111111111111111111-missing";

mod build;
mod misc;
mod opcodes_read;
mod opcodes_write;
