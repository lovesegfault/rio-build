-- Scheduler-managed live-build input pins (X9 fix).
--
-- Problem: GcRoots (scheduler actor command) collects only OUTPUT
-- paths from non-terminal derivations — these aren't in narinfo yet,
-- so the mark CTE can't walk their references. But the INPUTS of an
-- in-flight build (already in narinfo, past grace period, no other
-- root) can be GC'd mid-build → worker's FUSE read fails.
--
-- Fix: scheduler writes input-closure paths to scheduler_live_pins
-- at dispatch time, deletes on terminal status. The mark CTE seeds
-- from this table (see gc/mark.rs seed (e)). Best-effort on the
-- scheduler side — if PG is down during dispatch, the pin is lost
-- but the 24h grace period is the fallback safety net.
--
-- No FK to narinfo: the scheduler writes BEFORE the path may exist
-- in narinfo (input of a build-being-dispatched might itself still
-- be uploading from a prior build). mark.rs joins this to narinfo,
-- so pins for paths-not-in-narinfo are harmless (JOIN produces 0
-- rows; the pin just doesn't protect anything).
--
-- PK (store_path_hash, drv_hash): one pin per (path, build). The
-- same input path shared by two concurrent builds gets two rows —
-- unpinning one doesn't expose the other's input.

CREATE TABLE scheduler_live_pins (
    store_path_hash BYTEA NOT NULL,
    drv_hash TEXT NOT NULL,
    pinned_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store_path_hash, drv_hash)
);

-- For sweep_stale_live_pins: delete all pins for a drv_hash when it
-- goes terminal. Also for the stale-cleanup query (WHERE drv_hash
-- NOT IN (SELECT drv_hash FROM derivations WHERE ...)).
CREATE INDEX scheduler_live_pins_drv_hash_idx
    ON scheduler_live_pins (drv_hash);
