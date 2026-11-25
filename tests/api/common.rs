//! Reusable helpers for the HTTP integration tests (decision 13).
//!
//! Every endpoint test follows the same shape: [`test_db`] checks out a
//! private, fully migrated, empty database from the slot pool in
//! [`wired_be::test_db`] and hands it over as a `PgPool`; [`test_app`] builds
//! the real router on top of that pool; [`TestRequest`] drives the router
//! in-process through `tower::ServiceExt::oneshot` — no sockets, no ports —
//! and returns a [`TestResponse`] to assert against. The fixture replaces
//! `#[sqlx::test]`, which paid a `CREATE DATABASE`, a full migration run and
//! a `DROP DATABASE` per test and serialized all of it behind one global
//! advisory lock.
//!
//! The `#![cfg(test)]` gate is always active in this crate (Cargo compiles
//! the `tests/api` crate with `--test`). It is what lets Clippy's
//! `allow-unwrap-in-tests` family apply below: a test helper may abort on
//! programmer error (invalid header, unparsable body) exactly like a
//! `#[test]` function does.
//!
//! Later tickets add database fixture helpers here (`create_user`,
//! `create_token`, ...); keep them in this module instead of duplicating
//! them per test file.

#![cfg(test)]

mod clients;
mod config;
mod credentials;
mod federation;
mod fixtures;
pub mod images;
mod jobs;
mod json;
mod request;
mod response;

pub use self::{
    clients::{
        TEST_REDIRECT_URI, TestClient, access_token_of, api_delete, api_get, api_patch, api_post,
        api_put, client_credentials_token, entity_id, login_password, register_client,
        register_via_api,
    },
    config::{
        MEDIA_PROXY_TEST_SECRET, TRANSLATION_TEST_KEY, test_app, test_app_state,
        test_app_with_config, test_app_with_emoji_dir, test_app_with_instance_docs_dir,
        test_app_with_restricted_areas, test_app_with_upload_limits, test_app_with_uploads_dir,
        test_config, test_config_with_emoji_dir, test_config_with_instance_docs_dir,
        test_config_with_rate_limits, test_config_with_restricted_areas,
        test_config_with_uploads_dir, test_db, test_media_proxy_app, test_media_proxy_config,
        test_translation_app, test_translation_config, test_uploader,
    },
    credentials::{
        create_admin_with_password, create_app, create_app_and_token, create_app_named,
        create_app_only_token, create_deactivated_user_with_password,
        create_mfa_user_with_password, create_moderator_with_password, create_token, expire_token,
    },
    federation::{deliver, delivering_state, delivery_jobs, incoming_args},
    fixtures::{
        create_local_user, create_local_user_with, create_notification, create_reaction,
        create_remote_user, create_status, create_status_attachment, create_user_with_password,
        reload_user,
    },
    jobs::run_all_jobs,
    json::{id_of, ids_of, sorted_keys},
    request::{MultipartPart, TestRequest},
    response::{
        TestResponse, activity_json, assert_all_answered, assert_all_errors, assert_all_refused,
        assert_answer, assert_error, expected_link,
    },
};
