-- Drop the explicit-pin table. It was created in 005_gc.sql as a
-- "reserved extension point" but never gained a production writer
-- (no AddGcRoot RPC, no rio-cli subcommand, no controller reconciler);
-- mark/sweep paid a JOIN + per-path EXISTS against a permanently-empty
-- table on every GC. Operator pinning via 'extra_roots' + grace window
-- is the supported path. See M_036 in rio-store/src/migrations.rs.
DROP TABLE IF EXISTS gc_roots;
