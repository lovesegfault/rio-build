-- Commentary: see rio-migrations/src/migrations.rs M_097
CREATE TABLE executor_confirm_fences (
    executor_token_sha256 CHAR(64)    PRIMARY KEY,
    intent_id             TEXT        NOT NULL,
    confirmed_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX executor_confirm_fences_confirmed_at
    ON executor_confirm_fences (confirmed_at);
