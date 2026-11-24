//! The account expansion the notification and conversation endpoints
//! share: the stored users among a list of ids, the viewer's relationship
//! to each, and the [`Account`] entities built from both.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{
    entities::{
        account::{Account, AccountViewer},
        relationship::{Relationship, RelationshipMap},
    },
    error::AppResult,
    models::user::{self, User},
    services::account_emojis::AccountEmojis,
    state::AppState,
};

/// The stored accounts among `ids` and `viewer`'s relationships to them.
///
/// The two batched reads every endpoint that embeds [`Account`] entities
/// starts from: the users keyed by id (an id with no row is absent) and
/// the viewer's [`RelationshipMap`] over `ids`, `None` for an anonymous
/// viewer.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account and
/// relationship reads.
pub async fn accounts_with_relationships(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
) -> AppResult<(HashMap<Uuid, User>, Option<RelationshipMap>)> {
    let users = user::get_many(&state.db, ids)
        .await?
        .into_iter()
        .map(|found| (found.id, found))
        .collect();
    let relationships = Relationship::load_map(&state.db, viewer, ids).await?;
    Ok((users, relationships))
}

/// Expands account ids into [`Account`] entities as seen by `viewer`.
///
/// Every stored account appears once, in the requested order, with the
/// viewer's [`Relationship`] embedded (see [`accounts_with_relationships`]).
/// Actors that left no row (deleted mid-request) simply drop out.
///
/// # Errors
///
/// [`Database`](crate::error::AppError::Database) from the account and
/// relationship reads.
pub async fn expand_accounts(
    state: &AppState,
    viewer: AccountViewer<'_>,
    ids: &[Uuid],
) -> AppResult<Vec<Account>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (mut users, relationships) = accounts_with_relationships(state, viewer, ids).await?;
    let found: Vec<User> = ids.iter().filter_map(|id| users.remove(id)).collect();
    let emojis = AccountEmojis::load_all(&state.db, &found).await?;
    let accounts = found
        .iter()
        .map(|user| {
            let account =
                Account::from_user_for(viewer, user, &state.config, relationships.as_ref());
            emojis.fill(account, user, &state.config)
        })
        .collect();
    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        models::user::{NewUser, insert as insert_user, local_ap_id},
    };

    /// A local account named `nickname`.
    async fn local_account(state: &AppState, nickname: &str) -> User {
        insert_user(
            &state.db,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    #[tokio::test]
    async fn expand_accounts_skips_an_id_without_a_row_and_keeps_order() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = AppState::for_tests(db.pool().clone(), Config::for_tests());
        let viewer = local_account(&state, "viewer").await;
        let alice = local_account(&state, "alice").await;
        let bob = local_account(&state, "bob").await;
        let missing = Uuid::now_v7();
        let ids = [bob.id, missing, alice.id];

        let accounts = expand_accounts(&state, Some(&viewer), &ids)
            .await
            .expect("the expansion runs");
        assert_eq!(
            accounts
                .iter()
                .map(|account| account.id.clone())
                .collect::<Vec<_>>(),
            [bob.id.to_string(), alice.id.to_string()],
            "the id with no row is skipped and the rest keep the requested order"
        );

        let (users, relationships) = accounts_with_relationships(&state, Some(&viewer), &ids)
            .await
            .expect("the batched reads run");
        assert_eq!(users.len(), 2, "only the stored accounts are loaded");
        assert!(!users.contains_key(&missing), "the missing id has no row");
        assert!(
            relationships.is_some_and(|map| map.contains_key(&bob.id)),
            "a viewer gets a relationship map covering the stored ids"
        );

        let (_, anonymous) = accounts_with_relationships(&state, None, &ids)
            .await
            .expect("the anonymous reads run");
        assert!(anonymous.is_none(), "no viewer, no relationship map");
        assert!(
            expand_accounts(&state, None, &[])
                .await
                .expect("the empty ask runs")
                .is_empty(),
            "asking about nothing answers nothing"
        );
    }
}
