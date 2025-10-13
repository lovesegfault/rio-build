// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

// @generated automatically by Diesel CLI.

pub mod gorgond_github {
    pub mod sql_types {
        #[derive(diesel::query_builder::QueryId, Clone, diesel::sql_types::SqlType)]
        #[diesel(postgres_type(name = "event_kind", schema = "gorgond_github"))]
        pub struct EventKind;
    }

    diesel::table! {
        gorgond_github.actors (id) {
            id -> Int8,
            login -> Text,
            display_login -> Text,
            avatar_url -> Text,
            current_at -> Nullable<Timestamptz>,
            updated_at -> Timestamptz,
            created_at -> Timestamptz,
        }
    }

    diesel::table! {
        use diesel::sql_types::*;
        use super::sql_types::EventKind;

        gorgond_github.events (id) {
            id -> Int8,
            repo_id -> Int8,
            actor_id -> Int8,
            org_id -> Nullable<Int8>,
            pull_request_id -> Nullable<Int8>,
            kind -> EventKind,
            public -> Bool,
            payload -> Jsonb,
            emitted_at -> Timestamptz,
            updated_at -> Timestamptz,
            created_at -> Timestamptz,
        }
    }

    diesel::table! {
        gorgond_github.orgs (id) {
            id -> Int8,
            login -> Text,
            avatar_url -> Text,
            current_at -> Nullable<Timestamptz>,
            updated_at -> Timestamptz,
            created_at -> Timestamptz,
        }
    }

    diesel::table! {
        gorgond_github.pull_requests (id) {
            id -> Int8,
            repo_id -> Int8,
            number -> Int4,
            ref_name -> Text,
            merge_request_id -> Nullable<Uuid>,
            current_at -> Nullable<Timestamptz>,
            updated_at -> Timestamptz,
            created_at -> Timestamptz,
        }
    }

    diesel::table! {
        gorgond_github.pushes (id) {
            id -> Int8,
            ref_name -> Text,
            head -> Bytea,
            created_at -> Timestamptz,
        }
    }

    diesel::table! {
        gorgond_github.repos (id) {
            id -> Int8,
            name -> Text,
            repository_id -> Nullable<Uuid>,
            polled_at -> Nullable<Timestamptz>,
            current_at -> Nullable<Timestamptz>,
            updated_at -> Timestamptz,
            created_at -> Timestamptz,
        }
    }

    diesel::joinable!(events -> actors (actor_id));
    diesel::joinable!(events -> orgs (org_id));
    diesel::joinable!(events -> pull_requests (pull_request_id));
    diesel::joinable!(events -> repos (repo_id));
    diesel::joinable!(pull_requests -> repos (repo_id));

    diesel::allow_tables_to_appear_in_same_query!(
        actors,
        events,
        orgs,
        pull_requests,
        pushes,
        repos,
    );
}
