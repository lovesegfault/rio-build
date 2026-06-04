//! Execution-lifecycle CRUD — `drv_executions` table.
//!
//! One row per execution attempt (UUIDv7 `exec_id`), created at
//! dispatch and stamped once at terminal. This is the log subsystem's
//! per-execution anchor: rio-store's latest-exec resolution
//! (`ORDER BY exec_id DESC`) and completeness predicate
//! (`status` ∈ terminal ∧ `final_line_count` covered by the chunk
//! manifest) read it. It deliberately duplicates `exec_id` /
//! `builder_id` / timestamps that also live on `assignments` —
//! `assignments` keeps one row per attempt with its own audit
//! semantics and a *different* status vocabulary (see
//! `rio_migrations::schema::EXEC_STATUS_SUCCEEDED`).
//!
//! The terminal UPDATE does not live here: `terminal_log_epilogue` is
//! a sync chokepoint that fires the write through `spawn_monitored`
//! (the `record_exec_correlation` pattern), so the SQL sits next to
//! that call in `actor/event.rs`.
//!
//! No CRUD lives here anymore (merged_bug_284 dead-code sweep): the
//! pull mint creates the row inside its fenced statement
//! (`open_attempts.rs::mint_pull_attempt_fenced`) — the stream-era
//! `insert_drv_execution` dispatch writer it replaced was deleted
//! when the module-level dead-code shields came off.
