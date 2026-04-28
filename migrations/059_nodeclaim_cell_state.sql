-- Commentary: see rio-store/src/migrations.rs M_059
CREATE TABLE nodeclaim_cell_state (
    hw_class       text    NOT NULL,
    capacity_type  text    NOT NULL CHECK (capacity_type IN ('spot', 'od')),
    z_sketch_active   bytea,
    z_sketch_shadow   bytea,
    boot_sketch_active   bytea,
    boot_sketch_shadow   bytea,
    sketch_epoch   timestamptz NOT NULL DEFAULT now(),
    lead_time_q    double precision NOT NULL DEFAULT 0.9,
    idle_gap_events  jsonb NOT NULL DEFAULT '[]',
    PRIMARY KEY (hw_class, capacity_type)
);
