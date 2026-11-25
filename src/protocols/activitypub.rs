//! `ActivityPub`'s adapter: the façade the protocol layer sees it through.
//!
//! This file holds no `ActivityPub` logic. The implementation stays in
//! `src/federation/` (actors, signatures, delivery, inbox processing) and in
//! `src/api/activitypub*` (the HTTP routes and handlers); every hook of
//! [`ProtocolAdapter`] implemented here only delegates to those modules.
//!
//! # Job kinds
//!
//! A job kind belongs to `ActivityPub` when its handler lives under
//! `src/federation/` or `src/services/federation_out/`, or it is enqueued on
//! [`QUEUE_FEDERATOR_INCOMING`] or [`QUEUE_FEDERATOR_OUTGOING`]. These are the
//! kinds `ACTIVITYPUB_JOB_HANDLERS` registers:
//!
//! - `federator.deliver` ([`federation::delivery::KIND_DELIVER`])
//! - `federator.incoming` ([`federation::dispatch::KIND_INCOMING`])
//! - `federation.cleanup_received` ([`federation::dispatch::KIND_CLEANUP_RECEIVED`])
//! - `federator.publish` ([`federation::publisher::KIND_PUBLISH`])
//! - `polls.publish_update` ([`services::federation_out::KIND_PUBLISH_UPDATE`])
//! - `federator.backfill` ([`federation::fetch_object::KIND_BACKFILL`])
//! - `federator.quote_backfill` ([`federation::fetch_object::KIND_QUOTE_BACKFILL`])
//!
//! It is one of the two worked examples a new protocol copies: this one wraps
//! existing code, `protocols::fixture` is the minimal one.

use axum::Router;
use futures::future::BoxFuture;
use sqlx::PgConnection;

use crate::{
    api::well_known::{self, FingerLink, WebfingerContribution},
    config::Config,
    error::AppResult,
    federation,
    jobs::{HandlerFn, QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING},
    models::{
        activitypub_actor::{self, ActorStatus},
        user::{
            User, local_ap_id, nickname_taken,
            register::{NICKNAME_FIELD, validated_nickname},
        },
    },
    protocols::{
        ProtocolId,
        adapter::ProtocolAdapter,
        identity::{Identity, IdentityStatus},
    },
    services,
    state::AppState,
};

/// The `ActivityPub` adapter.
#[derive(Debug, Clone, Copy)]
pub struct ActivityPub;

/// Every job kind `ActivityPub` owns, bound to its handler in `src/federation/`
/// or `src/services/federation_out/`.
const ACTIVITYPUB_JOB_HANDLERS: &[(&str, HandlerFn)] = &[
    (
        federation::delivery::KIND_DELIVER,
        federation::delivery::handle,
    ),
    (
        federation::dispatch::KIND_INCOMING,
        federation::dispatch::handle,
    ),
    (
        federation::publisher::KIND_PUBLISH,
        federation::publisher::handle,
    ),
    (
        services::federation_out::KIND_PUBLISH_UPDATE,
        services::federation_out::handle_publish_update,
    ),
    (
        federation::dispatch::KIND_CLEANUP_RECEIVED,
        federation::dispatch::handle_cleanup_received,
    ),
    (
        federation::fetch_object::KIND_BACKFILL,
        federation::fetch_object::handle,
    ),
    (
        federation::fetch_object::KIND_QUOTE_BACKFILL,
        federation::fetch_object::handle_quote,
    ),
];

/// The queues `ActivityPub` enqueues its inbound and outbound work onto.
const ACTIVITYPUB_JOB_QUEUES: &[&str] = &[QUEUE_FEDERATOR_INCOMING, QUEUE_FEDERATOR_OUTGOING];

impl ProtocolAdapter for ActivityPub {
    /// `ActivityPub`'s id.
    fn id(&self) -> ProtocolId {
        ProtocolId::ActivityPub
    }

    /// The routes of [`crate::api::activitypub`]: actors, objects, inboxes, outboxes.
    fn router(&self, state: &AppState) -> Router<AppState> {
        crate::api::activitypub::router(&state.config)
    }

    /// The federation job kinds listed in the module docs.
    fn job_handlers(&self) -> &'static [(&'static str, HandlerFn)] {
        ACTIVITYPUB_JOB_HANDLERS
    }

    /// The incoming and outgoing federator queues.
    fn job_queues(&self) -> &'static [&'static str] {
        ACTIVITYPUB_JOB_QUEUES
    }

    /// The actor id and profile aliases, and the profile-page, self and subscribe links.
    fn webfinger(&self, config: &Config, user: &User) -> WebfingerContribution {
        well_known::activitypub_contribution(user, config)
    }

    /// The `lrdd` link naming the `WebFinger` template.
    fn host_meta_links(&self, config: &Config) -> Vec<FingerLink> {
        well_known::activitypub_host_meta_links(config)
    }

    /// `NodeInfo` names it `activitypub`.
    fn nodeinfo_protocol(&self) -> Option<&'static str> {
        Some("activitypub")
    }

    /// `nickname`, the key registration errors already use.
    fn identifier_field(&self) -> &'static str {
        NICKNAME_FIELD
    }

    /// Today's registration rules for a nickname.
    fn validate_identifier(
        &self,
        requested: &str,
        _config: &Config,
    ) -> Result<String, Vec<String>> {
        validated_nickname(requested)
    }

    /// The nickname itself: every valid nickname is a valid `ActivityPub` ID.
    fn derive_identifier(&self, nickname: &str, _config: &Config) -> String {
        nickname.to_owned()
    }

    /// Whether any account already has the nickname, in any case.
    fn identifier_taken<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        identifier: &'a str,
    ) -> BoxFuture<'a, AppResult<bool>> {
        Box::pin(nickname_taken(conn, identifier))
    }

    /// The actor URL of the local account named `identifier`.
    fn identifier_uri(&self, identifier: &str, config: &Config) -> String {
        local_ap_id(&config.base_url, identifier)
    }

    /// The account's `activitypub_actors` row as an identity, `None` without one.
    fn identity<'a>(
        &'a self,
        state: &'a AppState,
        user: &'a User,
    ) -> BoxFuture<'a, AppResult<Option<Identity>>> {
        Box::pin(async move {
            let row = activitypub_actor::get(&state.db, user.id).await?;
            Ok(row.map(|actor| Identity {
                identifier: user.nickname.clone(),
                uri: user.ap_id.clone(),
                status: match actor.status {
                    ActorStatus::Pending => IdentityStatus::Pending,
                    ActorStatus::Active => IdentityStatus::Active,
                },
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixtures::{local_user, remote_user},
        test_support::app_state,
    };

    /// Inputs spanning valid, too-long, bad-character, blank and reserved nicknames.
    const NICKNAMES: [&str; 8] = [
        "alice",
        "Alice_Doe-1",
        "  padded  ",
        "",
        "a-nickname-that-is-way-too-long-to-pass",
        "bad.char",
        "admin",
        "Admin",
    ];

    /// The adapter's validator gives the verdict and messages `validated_nickname` gives.
    #[test]
    fn validation_agrees_with_the_nickname_rules() {
        let config = Config::for_tests();
        for requested in NICKNAMES {
            assert_eq!(
                ActivityPub.validate_identifier(requested, &config),
                validated_nickname(requested),
                "for {requested:?}"
            );
        }
    }

    /// Spot checks of the verdicts themselves, so agreement cannot hide a broken rule.
    #[test]
    fn validation_verdicts() {
        let config = Config::for_tests();
        let verdict = |requested: &str| ActivityPub.validate_identifier(requested, &config);
        assert_eq!(
            verdict("  alice "),
            Ok("alice".to_owned()),
            "valid, trimmed"
        );
        assert!(verdict(&"a".repeat(31)).is_err(), "too long");
        assert!(verdict("bad.char").is_err(), "bad character");
        assert!(verdict("admin").is_err(), "reserved");
        assert!(verdict("").is_err(), "blank");
    }

    /// The field key and derivation keep today's behaviour.
    #[test]
    fn field_and_derivation_keep_the_nickname() {
        let config = Config::for_tests();
        assert_eq!(ActivityPub.identifier_field(), "nickname", "field");
        assert_eq!(
            ActivityPub.derive_identifier("Cool_Guy", &config),
            "Cool_Guy",
            "derived ID"
        );
    }

    /// The URI is the actor URL.
    #[test]
    fn uri_is_the_actor_url() {
        let config = Config::for_tests();
        assert_eq!(
            ActivityPub.identifier_uri("alice", &config),
            local_ap_id(&config.base_url, "alice"),
            "uri"
        );
    }

    /// Taken is the case-insensitive nickname check.
    #[tokio::test]
    async fn taken_means_a_nickname_in_any_case() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let _user = local_user(db.pool(), "Taken_Name").await;
        let mut conn = db.pool().acquire().await.expect("a connection");

        let same = ActivityPub.identifier_taken(&mut conn, "Taken_Name").await;
        let other_case = ActivityPub.identifier_taken(&mut conn, "taken_name").await;
        let free = ActivityPub.identifier_taken(&mut conn, "free_name").await;

        assert!(same.expect("lookup"), "same spelling");
        assert!(other_case.expect("lookup"), "other case");
        assert!(!free.expect("lookup"), "unused nickname");
    }

    /// A registered local user is an active identity at its actor URL.
    #[tokio::test]
    async fn local_user_identity_is_active_at_the_actor_url() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = local_user(db.pool(), "identity_user").await;

        let identity = ActivityPub.identity(&state, &user).await;

        assert_eq!(
            identity.expect("lookup"),
            Some(Identity {
                identifier: "identity_user".to_owned(),
                uri: local_ap_id(&state.config.base_url, "identity_user"),
                status: IdentityStatus::Active,
            }),
            "identity"
        );
    }

    /// A pending actor row is a pending identity.
    #[tokio::test]
    async fn pending_actor_is_a_pending_identity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = local_user(db.pool(), "pending_user").await;
        let updated =
            sqlx::query("UPDATE activitypub_actors SET status = 'pending' WHERE user_id = $1")
                .bind(user.id)
                .execute(db.pool())
                .await
                .expect("the status updates")
                .rows_affected();
        assert_eq!(updated, 1, "one actor row is pending");

        let identity = ActivityPub.identity(&state, &user).await;

        let identity = identity.expect("lookup").expect("enrolled");
        assert_eq!(identity.status, IdentityStatus::Pending, "status");
    }

    /// A user without an actor row is not enrolled.
    #[tokio::test]
    async fn user_without_an_actor_row_has_no_identity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = local_user(db.pool(), "unenrolled_user").await;
        let deleted = sqlx::query("DELETE FROM activitypub_actors WHERE user_id = $1")
            .bind(user.id)
            .execute(db.pool())
            .await
            .expect("the row is removed")
            .rows_affected();
        assert_eq!(deleted, 1, "one actor row is removed");

        let identity = ActivityPub.identity(&state, &user).await;

        assert_eq!(identity.expect("lookup"), None, "not enrolled");
    }

    /// A remote mirror's identity is its own actor id.
    #[tokio::test]
    async fn remote_user_identity_uses_its_actor_id() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());
        let user = remote_user(db.pool(), "far_away").await;

        let identity = ActivityPub.identity(&state, &user).await;

        let identity = identity.expect("lookup").expect("enrolled");
        assert_eq!(identity.uri, user.ap_id, "uri");
    }
}
