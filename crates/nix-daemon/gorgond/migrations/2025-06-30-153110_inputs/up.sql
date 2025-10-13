-- SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
--
-- SPDX-License-Identifier: EUPL-1.2

CREATE TABLE repositories (
    id          UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    kind        TEXT NOT NULL CHECK (kind <> ''),
    ident       TEXT NOT NULL CHECK (kind <> ''),
    UNIQUE (kind, ident),

    storage     TEXT NOT NULL CHECK (storage <> '') UNIQUE,

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('repositories');

COMMENT ON COLUMN repositories.kind IS
    'Used by watchers to find their repositories, eg. `gorgond-github` wants "github".';
COMMENT ON COLUMN repositories.ident IS
    'Opaque identifier, used internally by watchers. Do not rely on the value/meaning of this field.';
COMMENT ON COLUMN repositories.storage IS
    'Git repository used for storage, gorgond needs write access to it.';

---

CREATE TABLE commits (
    id             UUID  PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    repository_id  UUID  NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    hash           BYTEA NOT NULL CHECK (hash <> ''),
    UNIQUE (repository_id, hash),

    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

---

CREATE TABLE repository_merge_requests (
    id             UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    repository_id  UUID NOT NULL    REFERENCES repositories (id) ON DELETE CASCADE,
    ident          TEXT NOT NULL    CHECK (ident <> ''),
    UNIQUE (repository_id, ident),

    is_open        BOOLEAN NOT NULL,

    -- Optional metadata; note the lack of `CHECK (a <> '')` constraints.
    title          TEXT    NOT NULL,
    description    TEXT    NOT NULL,

    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('repository_merge_requests');

COMMENT ON COLUMN repository_merge_requests.ident IS
    'Opaque identifier, used internally by watchers. Do not rely on the value/meaning of this field.';
COMMENT ON COLUMN repository_merge_requests.is_open IS
    'Whether the Merge Request is open, and thus considered for testing.';

COMMENT ON COLUMN repository_merge_requests.title IS 'Optional, may be shown in the UI.';
COMMENT ON COLUMN repository_merge_requests.description IS 'Optional, may be shown in the UI.';

---

CREATE TABLE repository_merge_request_versions (
    id                UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    merge_request_id  UUID NOT NULL REFERENCES repository_merge_requests (id) ON DELETE CASCADE,
    source            TEXT NOT NULL CHECK (source <> ''),
    commit_id         UUID          REFERENCES commits (id),

    obsolete_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('repository_merge_request_versions');

COMMENT ON COLUMN repository_merge_request_versions.source IS
    'Source URL, from which the commit may be fetched. These URLs are understood to not necessarily be valid forever, and versions not resolved before this happens may become permanently unavailable.';
COMMENT ON COLUMN repository_merge_request_versions.commit_id IS
    'Set by gorgond when "source" is successfully resolved to a commit.';

---

CREATE TABLE project_inputs (
    id          UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    project_id  UUID NOT NULL REFERENCES projects (id),
    key         TEXT NOT NULL CHECK (key <> ''),
    UNIQUE (project_id, key),

    -- Either "source" or "repository_id" must be set.
    source         TEXT NOT NULL,
    repository_id  UUID REFERENCES repositories (id),
    CONSTRAINT source_or CHECK ((source <> '') OR (repository_id IS NOT NULL)),

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('project_inputs');

COMMENT ON COLUMN project_inputs.key IS 'Value of "build_inputs.key".';

---

CREATE TABLE project_input_commits (
    input_id   UUID NOT NULL REFERENCES project_inputs (id) ON DELETE CASCADE,
    commit_id  UUID NOT NULL REFERENCES commits (id) ON DELETE CASCADE,
    PRIMARY KEY (input_id, commit_id),

    -- NOT NULL, unknown is represented by a matching row not being present at all.
    result      BOOLEAN NOT NULL,

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('project_input_commits');

---

CREATE TABLE rollups (
    id             UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    parent_id      UUID          REFERENCES rollups (id)        ON DELETE CASCADE,
    input_id       UUID NOT NULL REFERENCES project_inputs (id) ON DELETE CASCADE,
    commit_id      UUID NOT NULL REFERENCES commits (id)        ON DELETE CASCADE,
    build_input_id UUID          REFERENCES build_inputs (id)   ON DELETE CASCADE,

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE rollup_commits (
    rollup_id  UUID NOT NULL REFERENCES rollups (id) ON DELETE CASCADE,
    commit_id  UUID NOT NULL REFERENCES commits (id) ON DELETE CASCADE,
    PRIMARY KEY (rollup_id, commit_id)
);

SELECT diesel_manage_updated_at('rollups');

COMMENT ON COLUMN rollups.parent_id IS
    'If this rollup is part of a bisection, the parent rollup that was split to create it.';
COMMENT ON COLUMN rollups.build_input_id IS
    'Set by gorgond when "build_inputs" is created from "input_id" and this rollup. A rollup is considered pending if this is not set, and will be picked up in a future round of builds.';

---
