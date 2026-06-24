-- Count of derivations resolved by an actual build (executor reported
-- BuildResultStatus::Built), as opposed to substituted/cached. Feeds
-- BuildProgress.built (nix progress-bar semantics) and cache-effectiveness
-- dashboards.
ALTER TABLE builds ADD COLUMN built_drvs INTEGER NOT NULL DEFAULT 0;
