-- Dwell-clock carrier for the Item T conversion-strictness knob
-- (substitution-replacement follow-up ledger row 7, second half; spec
-- rule sched.materialize.conversion-strictness). The PD-20 park
-- re-evaluation may require a minimum dwell since the job's MOST
-- RECENT park began before converting a parked Vouched/Pending job
-- from-source; no park-begin instant existed anywhere (park_until is
-- the expiry, and recovery rebuilt only the remaining backoff), so the
-- dwell clock would have restarted at every failover. Written by the
-- park UPDATE (re-park overwrites -- the clock restarts by design);
-- read by the recovery view rebuild for failover-exact dwell. NULL =
-- parked before this migration (or never parked): the dwell gate
-- treats it as unmet and the next park cycle stamps it -- conservative
-- and self-healing, never a crash.
ALTER TABLE materialization_jobs
    ADD COLUMN park_began_at TIMESTAMPTZ;
