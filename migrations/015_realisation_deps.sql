-- migration 015: realisation_deps — CA-depends-on-CA junction table.
--
-- USER Q3: junction table, NOT a JSONB column on realisations.
--
-- Populated at scheduler resolve time (NOT by parsing the wire
-- `dependentRealisations` field — that field was removed in Nix
-- realisation JSON schema v2 and is always {} on the wire; see
-- ADR-018 / P0247). P0253 consumes.
--
-- realisations has NO surrogate id — PK is composite (drv_hash,
-- output_name) per migrations/002_store.sql:142. Composite FK on both
-- sides. ON DELETE RESTRICT (not CASCADE): a realisation delete that
-- would orphan deps is a scheduler/GC bug to surface loudly, not
-- silently cascade through.

-- r[sched.ca.resolve] — impl lands in P0253
CREATE TABLE realisation_deps (
    drv_hash         BYTEA NOT NULL,
    output_name      TEXT  NOT NULL,
    dep_drv_hash     BYTEA NOT NULL,
    dep_output_name  TEXT  NOT NULL,
    PRIMARY KEY (drv_hash, output_name, dep_drv_hash, dep_output_name),
    FOREIGN KEY (drv_hash, output_name)
        REFERENCES realisations(drv_hash, output_name) ON DELETE RESTRICT,
    FOREIGN KEY (dep_drv_hash, dep_output_name)
        REFERENCES realisations(drv_hash, output_name) ON DELETE RESTRICT
);

-- Reverse lookup: "who depends on this realisation?" — for cutoff
-- cascade. PK covers forward lookup (drv_hash, output_name prefix).
CREATE INDEX realisation_deps_reverse_idx
    ON realisation_deps(dep_drv_hash, dep_output_name);
