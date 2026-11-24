//! The `Delete` fold: a sender's own post withdrawn, or its tombstone
//! reserved for a post never seen.

use tracing::debug;

#[cfg(doc)]
use super::delete_note;
use crate::{
    federation::{
        dispatch::{ActivityView, FederationError, Outcome},
        handlers::{actor, chat::withdraw_chat_message},
        json,
    },
    models::{
        chat,
        status::{self, Status},
        user::User,
    },
    services::delete_status,
    state::AppState,
};

/// The body of [`delete_note`]: the sender's own id back to the actor
/// handler, a stored chat message's id (Pleroma's bare-id or an embedded
/// `Tombstone` `Delete`) to the chat withdrawal, everything else folded as a
/// status withdrawal.
pub(super) async fn apply_delete(
    state: AppState,
    view: ActivityView,
    signer: User,
) -> Result<Outcome, FederationError> {
    let Some(object_id) = view.object.id() else {
        return Ok(Outcome::Ignored("a Delete whose object names no id"));
    };
    if object_id == view.actor {
        return actor::delete_actor(state, view, signer).await;
    }
    let chat_message = chat::message_by_ap_id(&state.db, object_id)
        .await
        .map_err(|err| FederationError::transient("the chat message read failed", &err))?;
    if let Some(message) = chat_message {
        return withdraw_chat_message(&state, &message, &signer).await;
    }
    let stored = status::get_by_uri(&state.db, object_id)
        .await
        .map_err(|err| FederationError::transient("the status uri read failed", &err))?;
    withdraw(&state, &signer, object_id, stored).await
}

/// The fold of [`apply_delete`]'s read: the author's live post deleted
/// through the shared service, an already withdrawn one restated, somebody
/// else's post nobody may withdraw, and an unseen object reserved as a
/// tombstone so its late `Create` is swallowed, provided the object is on
/// the signer's own host (a foreign host's object is ignored, writing nothing).
async fn withdraw(
    state: &AppState,
    signer: &User,
    uri: &str,
    stored: Option<Status>,
) -> Result<Outcome, FederationError> {
    match stored {
        Some(row) if row.user_id != signer.id => {
            debug!(
                status = %row.id,
                account = %signer.ap_id,
                "a Delete by an account other than the post's author; ignoring"
            );
            Ok(Outcome::Ignored(
                "a Delete by someone other than the author",
            ))
        }
        Some(row) if row.deleted_at.is_some() => Ok(Outcome::Ignored("an already deleted status")),
        Some(row) => {
            let deleted = delete_status::delete_status(state, &row)
                .await
                .map_err(|err| FederationError::transient("the status delete failed", &err))?;
            if deleted {
                debug!(status = %row.id, "a Delete folded a remote status withdrawal");
                Ok(Outcome::Handled)
            } else {
                Ok(Outcome::Ignored("an already deleted status"))
            }
        }
        None => {
            let uri_host = json::http_host(uri);
            if uri_host.is_none() || uri_host != json::http_host(&signer.ap_id) {
                debug!(
                    account = %signer.ap_id,
                    uri, "a Delete of an unstored object on another host; reserving nothing"
                );
                return Ok(Outcome::Ignored(
                    "a Delete of a foreign object this instance never stored",
                ));
            }
            let reserved = status::insert_tombstone(&state.db, uri, signer.id)
                .await
                .map_err(|err| {
                    FederationError::transient("the tombstone reservation failed", &err)
                })?;
            match reserved {
                Some(row) => {
                    debug!(
                        status = %row.id,
                        uri, "a Delete of an unstored status reserved its tombstone"
                    );
                    Ok(Outcome::Handled)
                }
                None => Ok(Outcome::Ignored("the object's tombstone was already there")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{
        BOB, app_state, author, author_again, live_content, mirrored, note, other, rows_for_uri,
        view_of,
    };
    use super::super::{delete_note, update_note};
    use super::*;
    use crate::models::user;

    #[tokio::test]
    async fn the_delete_soft_deletes_the_authors_post() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/gone-1";
        drop(mirrored(&pool, &author, uri, "doomed").await);

        let outcome = delete_note(
            state.clone(),
            view_of("Delete", json!(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the withdrawal is folded");
        assert!(
            live_content(&pool, uri).await.is_none(),
            "the post is no longer live"
        );
        let tombstone = status::get_by_uri(&pool, uri)
            .await
            .expect("the uri read")
            .expect("the row stays");
        assert!(tombstone.deleted_at.is_some(), "as a tombstone");

        let again = delete_note(state, view_of("Delete", json!(uri), None), author)
            .await
            .expect("the second fold completes");
        assert_eq!(
            again,
            Outcome::Ignored("an already deleted status"),
            "and the second sight moves nothing"
        );
    }

    #[tokio::test]
    async fn a_delete_of_an_unstored_same_host_object_reserves_its_tombstone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/deleted-before-arrival";

        let outcome = delete_note(
            state.clone(),
            view_of("Delete", json!(uri), None),
            author_again(&pool).await,
        )
        .await
        .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the tombstone is reserved");
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "exactly one row carries the uri"
        );

        // A `Create` of the same object now meets the reserved tombstone in
        // the ingest's dedupe read and is swallowed; the update path routes
        // an unknown-to-edit object through that same ingest, so it answers
        // identically here.
        let swallowed = update_note(state, view_of("Create", note(uri, json!({})), None), author)
            .await
            .expect("the late activity completes");
        assert_eq!(
            swallowed,
            Outcome::Ignored("tombstoned"),
            "the ingest's tombstone check ignores the post that arrives after its delete"
        );
        assert_eq!(
            rows_for_uri(&pool, uri).await,
            1,
            "and no live status re-appears"
        );
    }

    /// A stored remote account `nick` on `host`.
    async fn remote_on(pool: &sqlx::PgPool, host: &str, nick: &str) -> User {
        user::insert(
            pool,
            &user::NewUser {
                nickname: format!("{nick}@{host}"),
                ap_id: format!("https://{host}/users/{nick}"),
                local: user::Flag::new(false),
                domain: Some(host.to_owned()),
                ..user::NewUser::default()
            },
        )
        .await
        .expect("the remote account fixture inserts")
    }

    #[tokio::test]
    async fn a_delete_of_an_unstored_foreign_object_reserves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let signer = remote_on(&pool, "peer.example", "m").await;
        let uri = "https://other.example/notes/1";
        let actor = signer.ap_id.clone();

        let outcome = delete_note(
            state,
            view_of("Delete", json!(uri), Some(actor.as_str())),
            signer,
        )
        .await
        .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("a Delete of a foreign object this instance never stored"),
            "a host cannot reserve another host's object"
        );
        assert_eq!(rows_for_uri(&pool, uri).await, 0, "and nothing is written");
    }

    #[tokio::test]
    async fn a_delete_of_an_unstored_object_with_no_host_reserves_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let signer = author(&pool).await;
        let uri = "urn:example:not-a-url";

        let outcome = delete_note(state, view_of("Delete", json!(uri), None), signer)
            .await
            .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("a Delete of a foreign object this instance never stored"),
            "an id with no http host matches no signer"
        );
        assert_eq!(rows_for_uri(&pool, uri).await, 0, "and nothing is written");
    }

    #[tokio::test]
    async fn a_foreign_delete_does_not_block_a_later_create() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let evil = remote_on(&pool, "peer.example", "m").await;
        let real = remote_on(&pool, "other.example", "alice").await;
        let uri = "https://other.example/notes/1";
        let evil_actor = evil.ap_id.clone();

        let refused = delete_note(
            state.clone(),
            view_of("Delete", json!(uri), Some(evil_actor.as_str())),
            evil,
        )
        .await
        .expect("the foreign delete completes");
        assert_eq!(
            refused,
            Outcome::Ignored("a Delete of a foreign object this instance never stored"),
            "the foreign delete is refused"
        );

        let real_actor = real.ap_id.clone();
        let created = update_note(
            state,
            view_of(
                "Create",
                note(uri, json!({ "attributedTo": real_actor })),
                Some(real_actor.as_str()),
            ),
            real,
        )
        .await
        .expect("the create completes");

        assert_eq!(
            created,
            Outcome::Handled,
            "the real author's post still arrives"
        );
        assert_eq!(rows_for_uri(&pool, uri).await, 1, "as one live row");
    }

    #[tokio::test]
    async fn a_delete_by_somebody_else_is_ignored() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let author = author(&pool).await;
        let erin = other(&pool).await;
        let uri = "https://remote.example/users/bob/statuses/not-yours";
        drop(mirrored(&pool, &author, uri, "still here").await);

        let outcome = delete_note(
            state,
            view_of("Delete", json!(uri), Some(erin.ap_id.as_str())),
            erin,
        )
        .await
        .expect("the fold completes");

        assert_eq!(
            outcome,
            Outcome::Ignored("a Delete by someone other than the author"),
            "one account does not withdraw another's post"
        );
        assert_eq!(
            live_content(&pool, uri).await.as_deref(),
            Some("<p>still here</p>"),
            "and the post stands"
        );
    }

    #[tokio::test]
    async fn the_fallback_hands_the_actors_own_id_to_the_actor_handler() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let signer = author(&pool).await;
        let signer_id = signer.id;

        let outcome = delete_note(state, view_of("Delete", json!(BOB), None), signer)
            .await
            .expect("the fold completes");

        assert_eq!(outcome, Outcome::Handled, "the self-delete was folded");
        let retired = user::get_by_id(&pool, signer_id)
            .await
            .expect("the account read")
            .expect("the account is still stored");
        assert!(
            !retired.is_active.get(),
            "and the actor handler retired it, not this fold"
        );
        assert_eq!(
            rows_for_uri(&pool, BOB).await,
            0,
            "and no tombstone status shadows the actor id"
        );
    }
}
