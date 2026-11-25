//! The identity claims of a registration: one identity per enabled protocol,
//! written on the transaction that inserts the user.
//!
//! The claim rules are in [`crate::protocols::identity`]. This module is the
//! registration's side of them: it picks each protocol's ID and collects
//! every failure, so the registrant sees all of them at once.

use std::collections::BTreeMap;

use sqlx::PgConnection;

use super::{FieldErrors, RegisterError, error::add_error, validate::HAS_ALREADY_BEEN_TAKEN};
use crate::{
    config::Config,
    models::user::User,
    protocols::{ProtocolId, adapter::ProtocolAdapter, identity::ClaimOutcome, registry},
    state::AppState,
};

/// Claims `user`'s identity on every enabled protocol, in registry order.
///
/// A protocol's ID is the one in `requested`, else the one derived from the
/// nickname; either way it is validated first. Every invalid or taken ID is
/// recorded under its protocol's
/// [`identifier_field`](ProtocolAdapter::identifier_field), and the claims
/// after a failure still run so the answer lists every failure.
///
/// # Errors
///
/// [`RegisterError::Invalid`] with every failure when any claim is invalid or
/// taken, in which case the caller must roll `conn`'s transaction back;
/// [`RegisterError::Failed`] for a database failure.
pub(super) async fn claim_identities(
    conn: &mut PgConnection,
    state: &AppState,
    user: &User,
    requested: &BTreeMap<ProtocolId, String>,
) -> Result<(), RegisterError> {
    let mut errors = FieldErrors::new();
    for adapter in registry::enabled(&state.config) {
        let field = adapter.identifier_field();
        let wanted = requested.get(&adapter.id()).map(String::as_str);
        match chosen_identifier(adapter, &state.config, &user.nickname, wanted) {
            Err(messages) => {
                for message in &messages {
                    add_error(&mut errors, field, message);
                }
            }
            Ok(identifier) => {
                if adapter.claim(conn, user, &identifier, state).await? == ClaimOutcome::Taken {
                    add_error(&mut errors, field, HAS_ALREADY_BEEN_TAKEN);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(RegisterError::Invalid(errors))
    }
}

/// The validated ID to claim on `adapter`'s protocol: `requested`, else derived from `nickname`.
fn chosen_identifier(
    adapter: &dyn ProtocolAdapter,
    config: &Config,
    nickname: &str,
    requested: Option<&str>,
) -> Result<String, Vec<String>> {
    let wanted = requested.map_or_else(
        || adapter.derive_identifier(nickname, config),
        str::to_owned,
    );
    adapter.validate_identifier(&wanted, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            invite_token::{self, InviteParams},
            user::register::{
                RegisterParams, create_local_user,
                test_support::{
                    actor_count, config_with_fixture, params_for, test_state_with, user_count,
                },
            },
        },
        protocols::fixture::{Fixture, mark_taken},
        test_support::app_state,
    };

    /// The message the fixture gives an ID with a character outside `a-z`, `0-9`, `-`.
    const FIXTURE_BAD_CHARACTER: &str = "may only contain a-z, 0-9 and -";

    /// Registration params for `nickname` that ask the fixture for `requested`.
    fn asking_fixture(nickname: &str, requested: &str) -> RegisterParams {
        RegisterParams {
            identifiers: BTreeMap::from([(ProtocolId::Fixture, requested.to_owned())]),
            ..params_for(nickname)
        }
    }

    /// The failures of a registration that must have been refused as invalid.
    fn invalid_errors(outcome: Result<User, RegisterError>) -> FieldErrors {
        match outcome {
            Err(RegisterError::Invalid(errors)) => errors,
            other => panic!("expected an Invalid registration, got {other:?}"),
        }
    }

    /// Whether the fixture holds `identifier`.
    async fn fixture_holds(pool: &sqlx::PgPool, identifier: &str) -> bool {
        let mut conn = pool.acquire().await.expect("a connection");
        Fixture
            .identifier_taken(&mut conn, identifier)
            .await
            .expect("lookup")
    }

    /// With the default config a registration leaves a user row and an active actor row.
    #[tokio::test]
    async fn default_config_claims_the_activitypub_identity() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let user = create_local_user(&state, params_for("plain"), None)
            .await
            .expect("the registration lands");

        assert_eq!(user_count(db.pool()).await, 1, "one users row");
        let actor = crate::models::activitypub_actor::get(db.pool(), user.id)
            .await
            .expect("the actor row reads")
            .expect("the account has an actor row");
        assert_eq!(
            actor.status,
            crate::models::activitypub_actor::ActorStatus::Active,
            "the identity is active"
        );
        assert_eq!(
            actor.inbox,
            Some(crate::models::user::inbox(&user.ap_id)),
            "inbox is the account's"
        );
        assert!(actor.private_key.is_some(), "and the claim made the keys");
    }

    /// With the fixture enabled a registration claims both identities, the fixture's derived.
    #[tokio::test]
    async fn fixture_identity_is_derived_from_the_nickname() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state_with(db.pool(), config_with_fixture());

        let user = create_local_user(&state, params_for("Claim_Guy"), None)
            .await
            .expect("the registration lands");

        assert_eq!(actor_count(db.pool()).await, 1, "the actor row exists");
        assert!(user.local.get(), "the user row exists");
        assert!(fixture_holds(db.pool(), "claim-guy").await, "fixture ID");
    }

    /// A requested fixture ID is used instead of the derived one, after normalising.
    #[tokio::test]
    async fn requested_identifier_wins_over_the_derived_one() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state_with(db.pool(), config_with_fixture());

        let outcome =
            create_local_user(&state, asking_fixture("requester", " Chosen-Id "), None).await;

        let user = outcome.expect("the registration lands");
        assert_eq!(user.nickname, "requester", "the account keeps its nickname");
        assert!(fixture_holds(db.pool(), "chosen-id").await, "requested ID");
        assert!(
            !fixture_holds(db.pool(), "requester").await,
            "the derived ID was not claimed"
        );
    }

    /// A taken fixture ID refuses the registration and leaves no row of either identity.
    #[tokio::test]
    async fn taken_fixture_identifier_rolls_everything_back() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state_with(db.pool(), config_with_fixture());
        mark_taken("taken-guy");

        let outcome = create_local_user(&state, params_for("Taken_Guy"), None).await;

        let errors = invalid_errors(outcome);
        assert_eq!(
            errors.keys().copied().collect::<Vec<_>>(),
            vec!["fixture_identifier"],
            "only the fixture failed"
        );
        assert_eq!(
            errors.get("fixture_identifier"),
            Some(&vec![HAS_ALREADY_BEEN_TAKEN.to_owned()]),
            "the message"
        );
        assert_eq!(user_count(db.pool()).await, 0, "no users row remains");
        assert_eq!(actor_count(db.pool()).await, 0, "no actor row remains");
    }

    /// A requested fixture ID that fails validation is refused with its message.
    #[tokio::test]
    async fn invalid_requested_identifier_is_refused_with_its_message() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state_with(db.pool(), config_with_fixture());

        let outcome = create_local_user(&state, asking_fixture("badasker", "bad_id"), None).await;

        let errors = invalid_errors(outcome);
        assert_eq!(
            errors.get("fixture_identifier"),
            Some(&vec![FIXTURE_BAD_CHARACTER.to_owned()]),
            "the fixture's own message"
        );
        assert_eq!(user_count(db.pool()).await, 0, "no users row remains");
        assert_eq!(actor_count(db.pool()).await, 0, "no actor row remains");
    }

    /// A requested ID for a protocol that is not enabled is never looked at.
    #[tokio::test]
    async fn requested_identifier_of_a_disabled_protocol_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let outcome = create_local_user(&state, asking_fixture("ignorer", "bad_id"), None).await;

        let user = outcome.expect("a protocol that is not enabled is not claimed");
        assert_eq!(user.nickname, "ignorer", "the registration landed");
        assert!(!fixture_holds(db.pool(), "bad_id").await, "nothing claimed");
    }

    /// An invited registration that fails on a claim spends no invite.
    #[tokio::test]
    async fn failed_claim_spends_no_invite() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = test_state_with(db.pool(), config_with_fixture());
        let invite = invite_token::create(db.pool(), &InviteParams::one_time())
            .await
            .expect("the invite mints");
        mark_taken("invited-guy");

        let outcome = create_local_user(&state, params_for("Invited_Guy"), Some(&invite)).await;

        assert!(
            invalid_errors(outcome).contains_key("fixture_identifier"),
            "the claim failed"
        );
        let stored = invite_token::list(db.pool())
            .await
            .expect("the list runs")
            .into_iter()
            .find(|candidate| candidate.id == invite.id)
            .expect("the invite exists");
        assert_eq!(stored.uses, 0, "no use was counted");
        assert!(!stored.used, "the one-time invite is not spent");
        assert_eq!(user_count(db.pool()).await, 0, "no users row remains");
    }
}
