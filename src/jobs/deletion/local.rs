//! The local account sweep: the job that tombstones a departed account's
//! posts, clears its relationships and blanks its row.

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

#[cfg(doc)]
use super::{KIND_DELETE, KIND_DELETE_REMOTE};
use super::{parse_args, retry};
use crate::{
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::{bookmark, favourite, follow, list, media_attachment, notification, status, user},
    state::AppState,
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_DELETE`]:
/// clear the account's statuses, relationships, notifications, lists and
/// media, then blank the personal data of the row itself.
///
/// A vanished user row or an unparsable payload is discarded — no retry
/// can fix either; everything else that fails is a database failure over
/// work that is idempotent, and retries.
pub fn handle_delete(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let user_id = parse_args(&args)?;
        let account = load_account(&state, user_id).await?;
        crate::services::federation_out::after_account_deleted(&state, &account).await;
        clear_data(&state, &account).await?;
        user::purge_personal_data(&state.db, account.id)
            .await
            .map_err(|err| retry(&err))?;
        debug!(account = %account.nickname, "account deletion finished");
        Ok(())
    })
}

/// The account the job clears. A vanished row is permanent (discard): the
/// endpoint only queues this job for a row it just marked, and if it is
/// gone by a later attempt, there is nothing left to clear.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone, [`JobError::Retry`]
/// on a database failure.
pub(super) async fn load_account(state: &AppState, user_id: Uuid) -> Result<user::User, JobError> {
    match user::get_by_id(&state.db, user_id).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(JobError::Discard(format!(
            "the delete job's user {user_id} no longer exists"
        ))),
        Err(err) => Err(retry(&err)),
    }
}

/// Steps 1 and 2: the account's posts tombstone, its relationships, likes,
/// marks, notifications, lists and media rows go. Each statement is a
/// whole step on its own — a retry after a mid-way failure re-runs the
/// earlier ones as no-ops and picks up where they stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`retry`]).
async fn clear_data(state: &AppState, account: &user::User) -> Result<(), JobError> {
    let db = &state.db;
    let id = account.id;
    let statuses = status::soft_delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let follows = follow::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let favourites = favourite::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let bookmarks = bookmark::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let notifications = notification::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let lists = list::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    let media = media_attachment::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(&err))?;
    debug!(
        account = %account.nickname,
        statuses, follows, favourites, bookmarks, notifications, lists, media,
        "account deletion cleared"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{
        BATCH_OVERSHOOT, accepted_follow, app_state, broken_state, count_where, follows,
        has_follow, likes, list_of, live_statuses, local_user, marks, media_row, notify, post,
        post_many, run, status_deleted_at,
    };
    use super::super::{KIND_DELETE, job_args};
    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{Flag, NewUser, insert, local_ap_id},
    };

    /// The sweep's status and media half: past a batch boundary every
    /// authored post is a tombstone, the account's uploads and the media
    /// of its posts go, and the kept row has no personal data left.
    #[tokio::test]
    async fn the_handler_tombstones_statuses_and_purges_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        post_many(&pool, &alice, BATCH_OVERSHOOT).await;
        let alices_post = post(&pool, &alice, "alice posts").await;
        let bobs_post = post(&pool, &bob, "bob keeps this").await;
        let alices_upload = media_row(&pool, &alice, None).await;
        let on_alices_status = media_row(&pool, &alice, Some(alices_post.id)).await;
        let bobs_on_alices_status = media_row(&pool, &bob, Some(alices_post.id)).await;
        let bobs_own_media = media_row(&pool, &bob, Some(bobs_post.id)).await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "all 121 of alice's statuses tombstoned, across the 100-row batch"
        );
        assert!(
            status_deleted_at(&pool, alices_post.id).await.is_some(),
            "the single post tombstoned too"
        );
        assert!(
            status_deleted_at(&pool, bobs_post.id).await.is_none(),
            "bob's post is not alice's to tombstone"
        );

        for gone in [alices_upload, on_alices_status, bobs_on_alices_status] {
            assert_eq!(
                count_where(&pool, "media_attachments", "id", gone).await,
                0,
                "media row {gone} is gone"
            );
        }
        assert_eq!(
            count_where(&pool, "media_attachments", "id", bobs_own_media).await,
            1,
            "bob's own file on bob's own post stays"
        );

        let purged = user::get_by_id(&pool, alice.id)
            .await
            .unwrap()
            .expect("the row is kept for referential integrity");
        assert!(purged.email.is_none(), "the email is gone");
        assert!(
            purged.password_hash.is_none(),
            "and the password hash with it"
        );
        assert_eq!(purged.name, "", "and the display name is blanked");
        assert_eq!(purged.nickname, "alice", "the handle stays for the refs");
    }

    /// The sweep's relationship half: both directions of every follow,
    /// the account's own likes and marks, its notifications in both
    /// roles, its lists and its memberships — while every row of other
    /// accounts about the same peers stays.
    #[tokio::test]
    async fn the_handler_clears_relationships_notifications_and_lists() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = local_user(&pool, "carol").await;
        let alices_post = post(&pool, &alice, "alice posts").await;
        let bobs_post = post(&pool, &bob, "bob posts").await;
        follows(&pool, &alice, &bob).await;
        follows(&pool, &bob, &alice).await;
        follows(&pool, &bob, &carol).await;
        let _alices_like = likes(&pool, &alice, bobs_post.id).await;
        marks(&pool, &alice, bobs_post.id).await;
        let bobs_like = likes(&pool, &bob, alices_post.id).await;
        let _to_alice = notify(&pool, &alice, &bob).await;
        let _from_alice = notify(&pool, &bob, &alice).await;
        let bobs_other = notify(&pool, &bob, &carol).await;
        let alices_list = list_of(&pool, &alice, &bob).await;
        let bobs_list = list_of(&pool, &bob, &alice).await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert!(
            !has_follow(&pool, &alice, &bob).await,
            "alice's follow of bob is gone"
        );
        assert!(
            !has_follow(&pool, &bob, &alice).await,
            "bob's follow of alice is gone"
        );
        assert!(
            has_follow(&pool, &bob, &carol).await,
            "bob and carol keep their relationship"
        );
        assert_eq!(
            count_where(&pool, "favourites", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "bookmarks", "user_id", alice.id).await,
            0
        );
        assert_eq!(
            count_where(&pool, "favourites", "id", bobs_like.id).await,
            1,
            "bob's like of alice's (tombstoned) status survives"
        );
        assert_eq!(
            count_where(&pool, "notifications", "user_id", alice.id).await,
            0,
            "alice's notification inbox is gone"
        );
        assert_eq!(
            count_where(&pool, "notifications", "from_account_id", alice.id).await,
            0,
            "and the notifications alice caused"
        );
        assert_eq!(
            count_where(&pool, "notifications", "id", bobs_other.id).await,
            1,
            "bob's notification about carol is nobody else's data"
        );
        assert_eq!(count_where(&pool, "lists", "id", alices_list.id).await, 0);
        assert_eq!(count_where(&pool, "lists", "id", bobs_list.id).await, 1);
        assert_eq!(
            count_where(&pool, "list_members", "user_id", alice.id).await,
            0,
            "alice is out of every list"
        );
        assert_eq!(
            count_where(&pool, "list_members", "user_id", bob.id).await,
            0,
            "bob's membership of alice's list went down with her list"
        );
    }

    /// The sweep announces the account while the graph still exists: a
    /// local account with a remote follower leaves exactly one
    /// `federator.publish` job — the deterministic `Delete`, its snapshot
    /// naming the follower's shared inbox — even though the same pass
    /// deletes every follow row and purges the row's personal data. The
    /// keypair stays behind on purpose: the queued deliveries sign with it
    /// after this returns.
    #[tokio::test]
    async fn the_handler_publishes_the_delete_before_clearing_the_graph() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(
            &pool,
            &NewUser {
                private_key: Some("PEM-PRIV".to_owned()),
                public_key: Some("PEM-PUB".to_owned()),
                ..NewUser::local("alice", &local_ap_id(TEST_BASE_URL, "alice"))
            },
        )
        .await
        .expect("the keyed local fixture inserts");
        let fan = insert(
            &pool,
            &NewUser {
                nickname: "fan@peer.example".to_owned(),
                ap_id: "https://peer.example/users/fan".to_owned(),
                local: Flag::new(false),
                domain: Some("peer.example".to_owned()),
                inbox: Some("https://peer.example/users/fan/inbox".to_owned()),
                shared_inbox: Some("https://peer.example/inbox".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");
        accepted_follow(&pool, &fan, &alice).await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        let rows =
            sqlx::query_as::<_, (Value,)>("SELECT args FROM jobs WHERE kind = 'federator.publish'")
                .fetch_all(&pool)
                .await
                .expect("the publish job reads");
        assert_eq!(rows.len(), 1, "the sweep announces the account once");
        let args = &rows[0].0;
        assert_eq!(args["actor_id"], alice.id.to_string());
        assert_eq!(args["activity"]["type"], "Delete");
        assert_eq!(
            args["activity"]["id"],
            format!("{TEST_BASE_URL}/activities/delete-user-{}", alice.id),
            "the id is the deterministic one the ticket pins"
        );
        assert_eq!(
            args["extra_inboxes"],
            json!(["https://peer.example/inbox"]),
            "the follower's shared inbox was snapshotted before the clear"
        );
        assert_eq!(
            count_where(&pool, "follows", "followee_id", alice.id).await,
            0,
            "and the relationship rows are gone all the same"
        );
    }

    /// The sweep is idempotent: a second pass over the cleared account
    /// succeeds and changes nothing — which is what makes the retry on a
    /// mid-way database failure safe.
    #[tokio::test]
    async fn a_second_pass_clears_nothing_more() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _posted = post(&pool, &alice, "one post").await;
        follows(&pool, &alice, &bob).await;

        let state = app_state(&pool);
        run(&state, alice.id).await.expect("the first pass runs");
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "first pass cleared"
        );

        run(&state, alice.id).await.expect("the second pass runs");
        assert_eq!(
            count_where(&pool, "statuses", "user_id", alice.id).await,
            1,
            "the rerun wrote no new row and removed no tombstone"
        );
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "and the account stays fully tombstoned"
        );
    }

    /// A payload that is not `{user_id}` can never run: discarded, not
    /// retried. So does a user row that no longer exists.
    #[tokio::test]
    async fn unusable_arguments_and_a_vanished_user_discard() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let state = app_state(db.pool());

        let bad = handle_delete(
            state.clone(),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            json!({ "id": "not-a-user" }),
        )
        .await;
        match bad {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("unusable"),
                "the discard says why: {detail}"
            ),
            other => panic!("unusable arguments must discard, not {other:?}"),
        }

        let missing = run(&state, Uuid::now_v7()).await;
        match missing {
            Err(JobError::Discard(detail)) => assert!(
                detail.contains("no longer exists"),
                "the discard names the missing row: {detail}"
            ),
            other => panic!("a vanished user must discard, not {other:?}"),
        }
    }

    /// A database that cannot answer is a retryable failure: every step is
    /// idempotent, so the queue's next attempt picks the sweep up.
    #[tokio::test]
    async fn an_unreachable_database_retries_the_sweep() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let state = broken_state().await;

        let outcome = run(&state, alice.id).await;
        match outcome {
            Err(JobError::Retry(detail)) => assert!(
                detail.starts_with("account delete pass failed"),
                "the retry states what failed: {detail}"
            ),
            other => panic!("a closed pool must retry, not {other:?}"),
        }
        assert!(
            user::get_by_id(&pool, alice.id)
                .await
                .unwrap()
                .expect("the healthy pool still has the row")
                .email
                .is_some(),
            "the broken pass purged nothing"
        );
    }

    /// The kind is wired into the worker's registry, and the registered
    /// handler is this module's sweep.
    #[tokio::test]
    async fn the_kind_is_registered_to_this_handler() {
        let registry = crate::job_registry();
        let handler = registry
            .get(KIND_DELETE)
            .expect("the account delete kind is registered");
        assert_eq!(KIND_DELETE, "users.delete", "the ticket's kind name");

        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let _posted = post(&pool, &alice, "one post").await;
        handler(
            app_state(&pool),
            AttemptInfo {
                attempts: 1,
                max_attempts: 5,
            },
            job_args(alice.id),
        )
        .await
        .expect("the registered handler runs");
        assert_eq!(
            live_statuses(&pool, alice.id).await,
            0,
            "and it is this module's sweep: the post is a tombstone"
        );
    }
}
