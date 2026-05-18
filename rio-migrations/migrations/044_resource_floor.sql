ALTER TABLE derivations
  ADD COLUMN floor_mem_bytes bigint NOT NULL DEFAULT 0,
  ADD COLUMN floor_disk_bytes bigint NOT NULL DEFAULT 0,
  ADD COLUMN floor_deadline_secs bigint NOT NULL DEFAULT 0;
