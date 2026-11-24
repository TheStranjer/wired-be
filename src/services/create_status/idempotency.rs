//! The `Idempotency-Key` arm of `POST /api/v1/statuses`.
//!
//! [`create_status_idempotent`] restates the status a claimed key already
//! created, or claims the key inside the create's own transaction — losing
//! a race rolls the write back with `discard` and answers the winner
//! through `replayed_entity`.
//!
//! [`create_status`]: crate::services::create_status::create_status

use tracing::{debug, warn};

use super::{
    CreateStatusParams,
    build::{Prepared, prepared_new_status},
    create::{created_entity, store_poll},
};
use crate::{
    entities::status::Status,
    error::{AppError, AppResult},
    models::{idempotency_key, oauth_app::OauthApp, status, user::User},
    services::render_status::{ViewerMode, render_status},
    state::AppState,
};

/// Creates the status `params` describes under the request's
/// `Idempotency-Key`, or restates the one that key already created.
///
/// When the pair `(user, key)` already names a live status, that status's
/// entity is the whole answer (200 for the endpoint as everywhere here):
/// nothing is checked beyond the key, nothing is written, and no hook fires
/// a second time. Otherwise the create runs exactly as
/// [`crate::services::create_status::create_status`]
/// does, with one more statement on its transaction: the key is claimed
/// around the freshly written status with `ON CONFLICT DO NOTHING` (see
/// [`idempotency_key::claim`]), so two requests racing under one key
/// serialize on that insert. The winner commits its own status — and its
/// poll, stored on the same transaction by `store_poll`, with it; the
/// loser finds the pair taken, rolls its write back without a trace (poll
/// and options included: a replayed or raced key leaves exactly one poll
/// row behind) and answers
/// the winner's entity. The third arm — the pair is taken but its status has
/// been deleted since — moves the key onto the fresh status
/// ([`idempotency_key::reassign`]), because a dead post is not a
/// replay-able answer. A body naming a `quote_id` is a create like any
/// other through this arm: the one committed write counts the original
/// exactly once, and a replay or a lost race never counts it again.
///
/// # Errors
///
/// Those of [`crate::services::create_status::create_status`], plus [`AppError::NotFound`] for the one row
/// a replayed render cannot answer (a status deleted in the window between
/// the key read and the render).
pub async fn create_status_idempotent(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    params: CreateStatusParams,
    key: &str,
) -> AppResult<Status> {
    if let Some(row) = idempotency_key::live_status(&state.db, user.id, key).await? {
        debug!(
            status = %row.id,
            user = %user.id,
            "status reposted from its idempotency key"
        );
        return replayed_entity(state, user, &row).await;
    }
    let Prepared { new, poll, .. } = prepared_new_status(state, user, app, &params).await?;
    let mut tx = state.db.begin().await?;
    let row = status::insert_conn(&mut tx, &state.config.base_url, &new).await?;
    store_poll(&mut tx, row.id, poll.as_ref()).await?;
    if idempotency_key::claim(&mut tx, user.id, key, row.id).await? {
        tx.commit().await?;
        return created_entity(state, user, row).await;
    }
    // The pair is taken — by a create that raced this one, or by a status
    // since deleted. Whichever, the re-read on this open transaction says
    // which, because a skipped `DO NOTHING` only ever names committed rows.
    if let Some(winner) = idempotency_key::live_status_conn(&mut tx, user.id, key).await? {
        // Ours never happened: the racing create's row is the key's answer,
        // and our uncommitted write goes back with this rollback.
        discard(tx).await;
        debug!(
            status = %winner.id,
            user = %user.id,
            "status reposted from a raced idempotency key"
        );
        return replayed_entity(state, user, &winner).await;
    }
    idempotency_key::reassign(&mut tx, user.id, key, row.id).await?;
    tx.commit().await?;
    created_entity(state, user, row).await
}

/// Rolls back the transaction of a create that lost its key race, logging a
/// rollback that failed: the answer the caller is about to return needs
/// nothing of ours, and a connection that dies before its `ROLLBACK` takes
/// the uncommitted write with it anyway.
async fn discard(tx: sqlx::Transaction<'_, sqlx::Postgres>) {
    if let Err(err) = tx.rollback().await {
        warn!(
            %err,
            "idempotent status create could not roll its losing write back"
        );
    }
}

/// Renders an already-stored status — the key's answer — as `user`, its
/// author, sees it: the create's render with no write and no hook behind it,
/// and a status that vanished between the key read and here is the 404.
///
/// # Errors
///
/// [`AppError::NotFound`] for a row the renderer cannot answer; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn replayed_entity(state: &AppState, user: &User, row: &status::Status) -> AppResult<Status> {
    render_status(state, ViewerMode::user(user), row)
        .await?
        .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::{poll, status::NewStatus},
        services::create_status::test_support::{
            BASE, attachment_ids, key_rows, live_ids, media_row, parent_row, plain_post, poll_rows,
            polled, posted, replying, state, upload, user, with_media,
        },
    };
    /// A post through the idempotency arm, as `author` under `key`.
    async fn keyed(state: &AppState, author: &User, text: &str, key: &str) -> Status {
        create_status_idempotent(state, author, None, posted(text), key)
            .await
            .expect("the keyed post creates")
    }

    /// Claim `key` for `status_id` outside any create — the row a racing
    /// request would have committed before this test's create looks.
    async fn claimed_for(pool: &PgPool, author: &User, key: &str, status_id: Uuid) {
        let mut tx = pool.begin().await.expect("a transaction opens");
        let claimed = idempotency_key::claim(&mut tx, author.id, key, status_id)
            .await
            .expect("the claim runs");
        assert!(claimed, "the fixture pair was free");
        tx.commit().await.expect("the claim commits");
    }

    /// A plain stored post, written straight through the model: the test's
    /// stand-in for a create whose key was claimed in an earlier life.
    async fn stored_post(pool: &PgPool, author: &User, text: &str) -> status::Status {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("the fixture post inserts")
    }

    #[tokio::test]
    async fn the_same_key_replays_the_status_it_created() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyed-once").await;

        let first = keyed(&app_state, &alice, "keyed once", "k1").await;
        let second = keyed(&app_state, &alice, "a different second body", "k1").await;

        assert_eq!(
            first.id, second.id,
            "one key, one status, the same id answered twice"
        );
        assert_eq!(
            second.content, "<p>keyed once</p>",
            "the replay restates the status the key created — the second \
             body is not even looked at"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![Uuid::parse_str(&first.id).expect("the entity carries its id")],
            "and exactly one row was ever written"
        );
        assert_eq!(key_rows(pool).await, 1, "one claim stands");
        let counted = crate::models::user::get_by_id(pool, alice.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(counted.statuses_count, 1, "counted once");
    }

    #[tokio::test]
    async fn different_keys_create_their_own_statuses() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyed-twice").await;

        let first = keyed(&app_state, &alice, "one", "k1").await;
        let second = keyed(&app_state, &alice, "two", "k2").await;

        assert_ne!(first.id, second.id, "two keys are two posts");
        assert_eq!(live_ids(pool).await.len(), 2, "and two rows stand");
        assert_eq!(key_rows(pool).await, 2, "each with its own claim");
    }

    #[tokio::test]
    async fn one_key_belongs_to_its_author_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "sharp").await;
        let bob = user(pool, "echo").await;

        let mine = keyed(&app_state, &alice, "alice's", "shared").await;
        let theirs = keyed(&app_state, &bob, "bob's", "shared").await;

        assert_ne!(
            mine.id, theirs.id,
            "the same key text is one key per account"
        );
        let again = keyed(&app_state, &alice, "ignored", "shared").await;
        assert_eq!(again.id, mine.id, "and each account replays its own status");
        assert_eq!(key_rows(pool).await, 2, "two claims, one per pair");
    }

    #[tokio::test]
    async fn a_key_claimed_by_another_writer_answers_its_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "late").await;
        let winner = stored_post(pool, &alice, "the winner").await;
        claimed_for(pool, &alice, "k1", winner.id).await;

        let replay = keyed(&app_state, &alice, "the loser's body", "k1").await;

        assert_eq!(
            replay.id,
            winner.id.to_string(),
            "the committed claim is the key's answer"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![winner.id],
            "and the losing body created nothing of its own"
        );
        assert_eq!(key_rows(pool).await, 1, "the claim stands alone");
    }

    /// Two creates under one fresh key, driven concurrently from one task:
    /// whichever arm the second request takes — pre-read replay or the
    /// mid-transaction `ON CONFLICT` loss — exactly one status stands and
    /// both callers answer it with the same id. Both bodies carry a poll,
    /// so the single surviving `polls` row proves the loser's poll rolled
    /// back with its status.
    #[tokio::test]
    async fn two_creates_racing_under_one_key_leave_one_status() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let alice = user(pool, "racer").await;
        let fixture = Arc::new((state(pool), alice));

        let (first, second) = tokio::join!(
            create_status_idempotent(&fixture.0, &fixture.1, None, polled("race one"), "k1"),
            create_status_idempotent(&fixture.0, &fixture.1, None, polled("race two"), "k1"),
        );
        let one = first.expect("the first racer creates or replays");
        let two = second.expect("the second racer creates or replays");

        assert_eq!(
            one.id, two.id,
            "both callers are answered by the one status the key made"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![Uuid::parse_str(&one.id).expect("an id")]
        );
        assert_eq!(key_rows(pool).await, 1, "one claim");
        assert_eq!(
            poll_rows(pool).await,
            1,
            "one poll — the loser's rolled back with its status"
        );
        let winner_id = Uuid::parse_str(&one.id).expect("an id");
        let stand = poll::get_for_status(pool, winner_id)
            .await
            .expect("the poll reads")
            .expect("the winner's poll stands");
        assert_eq!(
            poll::options(pool, stand.id)
                .await
                .expect("the options read")
                .len(),
            2,
            "and its options stand with it"
        );
        for entity in [&one, &two] {
            let value = serde_json::to_value(entity).expect("the entity serializes");
            assert_eq!(
                value["poll"]["id"],
                stand.id.to_string(),
                "both answers carry the one surviving poll"
            );
        }
        let counted = crate::models::user::get_by_id(pool, fixture.1.id)
            .await
            .unwrap()
            .expect("the author exists");
        assert_eq!(
            counted.statuses_count, 1,
            "the loser's rolled-back write counted nothing"
        );
    }

    #[tokio::test]
    async fn a_key_whose_status_was_deleted_creates_afresh() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "repeater").await;
        let doomed = keyed(&app_state, &alice, "post and delete me", "k1").await;
        let doomed_id = Uuid::parse_str(&doomed.id).expect("the entity carries its id");
        let tombstone = status::soft_delete(pool, doomed_id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        assert!(
            tombstone.deleted_at.is_some(),
            "the mapped row is gone from the API"
        );

        let fresh = keyed(&app_state, &alice, "the next one", "k1").await;
        let fresh_id = Uuid::parse_str(&fresh.id).expect("the entity carries its id");

        assert_ne!(
            fresh.id, doomed.id,
            "a deleted status is no replay-able answer"
        );
        assert_eq!(
            live_ids(pool).await,
            vec![fresh_id],
            "the fresh post is the only live row — and it moved the stale \
             claim instead of colliding with it"
        );
        assert_eq!(key_rows(pool).await, 1, "still one mapping for the pair");
        let restated = keyed(&app_state, &alice, "ignored", "k1").await;
        assert_eq!(
            restated.id, fresh.id,
            "and the moved key now replays afresh"
        );
    }

    /// The keyed arm shares the create's whole pre-write half, so a reply
    /// posts under an `Idempotency-Key` exactly as it would without one —
    /// and a replay restates it without counting the parent twice.
    #[tokio::test]
    async fn a_keyed_reply_replies_and_replays() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "keyer").await;
        let bob = user(pool, "bob").await;
        let parent = plain_post(pool, &bob, "keyable").await;

        let first =
            create_status_idempotent(&app_state, &alice, None, replying("keyed", parent.id), "k1")
                .await
                .expect("the keyed reply creates");
        assert_eq!(
            first.in_reply_to_id,
            Some(parent.id.to_string()),
            "the entity answers its parent"
        );
        let row = status::get_by_id(pool, Uuid::parse_str(&first.id).expect("an id"))
            .await
            .unwrap()
            .expect("the reply is stored");
        assert_eq!(row.in_reply_to_account_id, Some(bob.id));
        assert_eq!(row.conversation_id, parent.conversation_id);
        assert_eq!(parent_row(pool, parent.id).await.replies_count, 1);

        let again = create_status_idempotent(
            &app_state,
            &alice,
            None,
            replying("ignored", parent.id),
            "k1",
        )
        .await
        .expect("the replay answers");
        assert_eq!(again.id, first.id, "one key, one reply");
        assert_eq!(
            parent_row(pool, parent.id).await.replies_count,
            1,
            "and the parent counted it once"
        );
    }

    #[tokio::test]
    async fn a_replayed_create_restates_the_status_and_its_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let app_state = state(pool);
        let alice = user(pool, "alice").await;
        let mine = upload(pool, &alice).await;

        let first = create_status_idempotent(
            &app_state,
            &alice,
            None,
            with_media("once", &[&mine.id.to_string()]),
            "media-key",
        )
        .await
        .expect("the keyed create posts");
        let again = create_status_idempotent(
            &app_state,
            &alice,
            None,
            with_media("ignored", &[]),
            "media-key",
        )
        .await
        .expect("the replay answers");

        assert_eq!(again.id, first.id, "one key, one status");
        assert_eq!(
            attachment_ids(&again),
            attachment_ids(&first),
            "and the replay restates its attachments"
        );
        assert_eq!(live_ids(pool).await.len(), 1, "no second row");
        assert_eq!(media_row(pool, mine.id).await.position, 0, "claimed once");
    }
}
