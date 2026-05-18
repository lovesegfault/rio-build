ALTER TABLE interrupt_samples ADD COLUMN event_uid TEXT;
CREATE UNIQUE INDEX interrupt_samples_event_uid_uniq
  ON interrupt_samples (event_uid) WHERE event_uid IS NOT NULL;
