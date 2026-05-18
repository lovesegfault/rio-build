-- Persistent build event log for since_sequence resumption.
--
-- The actor's in-memory broadcast channel (cap 1024) handles
-- late subscribers within a single scheduler instance's lifetime.
-- This table handles gateway reconnect to a NEW leader after
-- failover: the new leader doesn't have the old one's broadcast
-- buffer, but it can replay from here.
--
-- Written by EventPersister (a bounded mpsc + single drain task,
-- same pattern as LogFlusher). emit_build_event try_sends to it
-- AFTER the broadcast — if the persister is backed up, the try_send
-- drops (the broadcast already carried the event; only a mid-
-- backlog reconnect loses it).
--
-- Event::Log is FILTERED OUT — those go to S3 via LogFlusher,
-- not here. A chatty rustc with 2 active builds sends ~20 log
-- events/sec; persisting those would flood PG. The gateway
-- reconnect cares about Started/DerivationEvent/Completed/Failed
-- (state machine events), not log lines.
--
-- GC: handle_cleanup_terminal_build deletes rows for completed
-- builds. Fire-and-forget (same as other terminal cleanup).

CREATE TABLE build_event_log (
    build_id    UUID    NOT NULL,
    -- Monotonic per-build, assigned by emit_build_event. The
    -- gateway's WatchBuild passes since_sequence; we replay
    -- WHERE sequence > $since.
    sequence    BIGINT  NOT NULL,
    -- Prost-encoded BuildEvent. BYTEA not JSONB: the gateway
    -- decodes back to the proto type; we don't query into it.
    -- JSONB would be nice for psql debugging but adds an encode/
    -- decode round-trip for no functional benefit.
    event_bytes BYTEA   NOT NULL,
    -- For GC time-based fallback (not the primary GC — that's
    -- per-build on cleanup). An orphaned event (build crashed
    -- before cleanup) eventually ages out. TODO(phase3b): add
    -- a periodic "DELETE WHERE created_at < now() - interval
    -- '7 days'" on Tick.
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (build_id, sequence)
);

-- Time-based index for the eventual GC sweep. Not used by the
-- replay query (that uses the PK).
CREATE INDEX idx_build_event_log_created ON build_event_log (created_at);
