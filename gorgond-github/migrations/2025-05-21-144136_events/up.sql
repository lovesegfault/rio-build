-- SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
--
-- SPDX-License-Identifier: EUPL-1.2

CREATE TABLE repos (
    id             BIGINT PRIMARY KEY,
    name           TEXT   NOT NULL,
    repository_id  UUID       NULL REFERENCES gorgond.repositories ON DELETE SET NULL,

    polled_at      TIMESTAMPTZ,
    current_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('repos');

COMMENT ON COLUMN repos.repository_id IS
    'Set only if the repo corresponds to a repository monitored by gorgond. '
    'We still need to track eg. forks, to be able to follow pull request pushes.';
COMMENT ON COLUMN repos.polled_at IS
    'If this is a repo monitored by gorgond, when we last polled for events.';

---

CREATE TABLE actors (
    id             BIGINT PRIMARY KEY,
    login          TEXT   NOT NULL,
    display_login  TEXT   NOT NULL,
    avatar_url     TEXT   NOT NULL,

    current_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('actors');

---

CREATE TABLE orgs (
    id             BIGINT PRIMARY KEY,
    login          TEXT   NOT NULL,
    avatar_url     TEXT   NOT NULL,

    current_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('orgs');

---

CREATE TABLE pull_requests (
    id                BIGINT PRIMARY KEY,
    repo_id           BIGINT NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
    number            INT    NOT NULL,
    ref_name          TEXT   NOT NULL,
    merge_request_id  UUID       NULL REFERENCES gorgond.repository_merge_requests ON DELETE SET NULL,

    current_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('pull_requests');

COMMENT ON COLUMN pull_requests.merge_request_id IS
    'MergeRequests on the gorgond side are created asynchronously, then filled in here.';

---

CREATE TABLE pushes (
    id             BIGINT PRIMARY KEY,
    ref_name       TEXT   NOT NULL CHECK (ref_name <> ''),
    head           BYTEA  NOT NULL CHECK (head <> ''),

    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

---

-- https://docs.github.com/en/rest/using-the-rest-api/github-event-types
CREATE TYPE event_kind AS ENUM ('pull_request', 'push');

CREATE TABLE events (
    id               BIGINT      PRIMARY KEY,
    repo_id          BIGINT      NOT NULL REFERENCES repos (id)         ON DELETE CASCADE,
    actor_id         BIGINT      NOT NULL REFERENCES actors (id)        ON DELETE CASCADE,
    org_id           BIGINT          NULL REFERENCES orgs (id)          ON DELETE SET NULL,
    pull_request_id  BIGINT          NULL REFERENCES pull_requests (id) ON DELETE SET NULL,
    kind             event_kind  NOT NULL,
    public           BOOLEAN     NOT NULL,
    payload          JSONB       NOT NULL,

    emitted_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('events');
