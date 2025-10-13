-- SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
--
-- SPDX-License-Identifier: EUPL-1.2

CREATE TABLE projects (
    id          UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    slug        TEXT UNIQUE NOT NULL CHECK (slug <> ''),
    name        TEXT NOT NULL CHECK (name <> ''),

    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('projects');

---

CREATE TABLE workers (
    id          UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    slug        TEXT UNIQUE NOT NULL CHECK (slug <> ''),
    name        TEXT NOT NULL CHECK (name <> ''),

    seen_at     TIMESTAMPTZ,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('workers');

---

CREATE TYPE build_result AS ENUM ('unknown', 'success', 'failure');

CREATE TABLE builds (
    id              UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    project_id      UUID NOT NULL REFERENCES projects (id),
    kind            TEXT NOT NULL CHECK (kind <> ''),
    expr            TEXT NOT NULL CHECK (expr <> ''),

    worker_id       UUID REFERENCES workers (id),
    result          build_result,

    allocated_at    TIMESTAMPTZ,
    heartbeat_at    TIMESTAMPTZ,
    started_at      TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

SELECT diesel_manage_updated_at('builds');

---

CREATE TABLE build_inputs (
    id          UUID PRIMARY KEY CHECK (id <> '00000000-0000-0000-0000-000000000000'),
    build_id    UUID NOT NULL REFERENCES builds (id) ON DELETE CASCADE,
    key         TEXT NOT NULL CHECK (key <> ''),
    source      TEXT NOT NULL CHECK (source <> ''),

    UNIQUE (build_id, key)
);
