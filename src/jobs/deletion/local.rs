//! The local account sweep: the job that tombstones a departed account's
//! posts, clears its relationships and blanks its row.

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::{DELETE_JOB, DeleteArgs, PASS_JOB};
#[cfg(doc)]
use super::{KIND_DELETE, KIND_DELETE_REMOTE};
use super::{SweepCounts, clear_shared};
use crate::{
    jobs::{
        AttemptInfo, HandlerFuture, JobError, backups::sweep_archive_file, load_user, parse_args,
        retry, scheduled_statuses,
    },
    models::{backup, job, list, media_attachment, scheduled_status, user, web_push_subscription},
    state::AppState,
};

/// The [`JobRegistry`](crate::jobs::JobRegistry) handler for [`KIND_DELETE`]:
/// clear the account's statuses, relationships, notifications, lists and
/// media, then blank the personal data of the row itself.
///
/// A vanished user row or an unparsable payload is discarded — no retry
/// can fix either; everything else that fails is a database failure over
/// work that is idempotent, and retries. That includes queueing the actor
/// `Delete`: it happens before the follow rows go, so a failed attempt
/// leaves the recipients in place for the next one.
pub fn handle_delete(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move {
        let user_id = parse_args::<DeleteArgs>(&args, DELETE_JOB)?.user_id;
        let account = load_account(&state, user_id).await?;
        // A failure here retries before `clear_data` deletes the follow rows
        // the recipient list is built from.
        crate::services::federation_out::after_account_deleted(&state, &account)
            .await
            .map_err(|err| retry(PASS_JOB, &err))?;
        clear_data(&state, &account).await?;
        user::purge_personal_data(&state.db, account.id)
            .await
            .map_err(|err| retry(PASS_JOB, &err))?;
        debug!(account = %account.nickname, "account deletion finished");
        Ok(())
    })
}

/// The account the job clears. A vanished row is permanent (discard): the
/// endpoint only queues this job for a row it just marked, and if it is
/// gone by a later attempt, there is nothing left to clear. The account is
/// already deactivated by then, so the load does not gate on activity.
///
/// # Errors
///
/// [`JobError::Discard`] when the user row is gone, [`JobError::Retry`]
/// on a database failure.
pub(super) async fn load_account(state: &AppState, user_id: Uuid) -> Result<user::User, JobError> {
    load_user(state, user_id, "delete", false).await
}

/// Steps 1 to 5: the account's posts tombstone, its relationships, likes,
/// marks, notifications, lists and media rows go, then its scheduled
/// statuses, backups and push subscriptions. Each statement is a
/// whole step on its own — a retry after a mid-way failure re-runs the
/// earlier ones as no-ops and picks up where they stopped.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`retry`]).
async fn clear_data(state: &AppState, account: &user::User) -> Result<(), JobError> {
    let db = &state.db;
    let id = account.id;
    let SweepCounts {
        statuses,
        chat_messages,
        follows,
        favourites,
        reactions,
        bookmarks,
        notifications,
    } = clear_shared(state, account).await?;
    let lists = list::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let media = media_attachment::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let scheduled = clear_scheduled(state, id).await?;
    let backups = clear_backups(state, id).await?;
    let push_subscriptions = web_push_subscription::delete_all_for_user(db, id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    debug!(
        account = %account.nickname,
        statuses, chat_messages, follows, favourites, reactions, bookmarks, notifications,
        lists, media,
        scheduled, backups, push_subscriptions,
        "account deletion cleared"
    );
    Ok(())
}

/// Step 3: the account's scheduled statuses go and each one's pending
/// `scheduled_statuses.publish` job is cancelled, so nothing fires as the
/// deleted actor. Answers how many schedules went.
///
/// # Errors
///
/// [`JobError::Retry`] on any database failure (see [`retry`]).
async fn clear_scheduled(state: &AppState, user_id: Uuid) -> Result<usize, JobError> {
    let ids = scheduled_status::delete_all_for_user(&state.db, user_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    for id in &ids {
        let _cancelled = job::cancel_pending(&state.db, &scheduled_statuses::unique_key(*id))
            .await
            .map_err(|err| retry(PASS_JOB, &err))?;
    }
    Ok(ids.len())
}

/// Step 4: the account's backup archives leave storage, then their rows.
/// A file that is already gone counts as removed; any other storage
/// failure retries the pass with the row kept, so no archive is left
/// without a database pointer. Answers how many rows went.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure or a failed file delete.
async fn clear_backups(state: &AppState, user_id: Uuid) -> Result<usize, JobError> {
    let rows = backup::list_for_user(&state.db, user_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    for row in &rows {
        if let Some(key) = row.storage_key.as_deref()
            && !sweep_archive_file(state, row, key).await
        {
            return Err(JobError::Retry(format!(
                "account delete pass failed: backup file {key} could not be deleted"
            )));
        }
        let _swept = backup::delete(&state.db, row.id)
            .await
            .map_err(|err| retry(PASS_JOB, &err))?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod announce_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_support::{
        BATCH_OVERSHOOT, accepted_follow, available_jobs_keyed, count_where, follows, fresh_user,
        has_follow, is_blank_tombstone, likes, list_of, listed_messages, live_statuses, local_user,
        locked_user, marks, media_row, notify, post, post_many, processed_backup,
        push_subscription, reacts, run, scheduled_with_job, send_chat, status_deleted_at,
        uploads_state,
    };
    use super::super::{KIND_DELETE, job_args};
    use super::*;
    use crate::test_support::{app_state, broken_state};
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

    /// After the job, the purged row renders as an `Account` with no
    /// profile data beyond its handle: empty note, no fields or tags, the
    /// default avatar and header, no background, and no roles.
    #[tokio::test]
    async fn the_handler_leaves_an_account_that_renders_no_profile() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let carol = insert(
            &pool,
            &NewUser {
                email: Some("carol@example.org".to_owned()),
                password_hash: Some("hash-of-a-secret".to_owned()),
                name: "Carol".to_owned(),
                bio: "<p>carol's bio</p>".to_owned(),
                raw_bio: "carol's bio".to_owned(),
                fields: json!([{"name": "site", "value": "https://carol.example"}]),
                raw_fields: json!([{"name": "site", "value": "https://carol.example"}]),
                avatar_url: Some("https://cdn.example/carol.png".to_owned()),
                banner_url: Some("https://cdn.example/carol-banner.png".to_owned()),
                background_image_url: Some("https://cdn.example/carol-bg.png".to_owned()),
                tags: vec!["force_unlisted".to_owned()],
                is_admin: Flag::new(true),
                is_moderator: Flag::new(true),
                discoverable: Flag::new(true),
                ..NewUser::local("carol", &local_ap_id(TEST_BASE_URL, "carol"))
            },
        )
        .await
        .expect("the populated fixture inserts");
        let state = app_state(&pool);

        run(&state, carol.id).await.expect("the delete job runs");

        let reloaded = user::get_by_id(&pool, carol.id)
            .await
            .unwrap()
            .expect("the row is kept for referential integrity");
        let account = crate::entities::account::Account::from_user(&reloaded, &state.config);
        assert_eq!(account.note, "", "the bio is gone");
        assert!(account.fields.is_empty(), "the fields are gone");
        assert_eq!(account.display_name, "", "the display name is gone");
        assert_eq!(
            account.avatar,
            format!(
                "{}{}",
                state.config.media_base_url,
                crate::entities::account::DEFAULT_AVATAR_PATH
            ),
            "the avatar is the default"
        );
        assert_eq!(
            account.header,
            format!(
                "{}{}",
                state.config.media_base_url,
                crate::entities::account::DEFAULT_BANNER_PATH
            ),
            "the header is the default"
        );
        assert!(reloaded.tags.is_empty(), "the tags are gone");
        assert!(
            account.pleroma.background_image.is_none(),
            "the background is gone"
        );
        assert!(!account.discoverable.0, "the account is not discoverable");
        assert!(!account.pleroma.is_admin.0, "the admin role is gone");
        assert!(
            !account.pleroma.is_moderator.0,
            "the moderator role is gone"
        );
        assert_eq!(account.acct, "carol", "the handle stays");
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

    /// The peers of a deleted local account are live: the follower of it
    /// stops counting a followee and the account it followed stops counting
    /// a follower, while the deleted account's own counters drop to zero.
    #[tokio::test]
    async fn the_handler_steps_the_peers_follow_counters_down() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = local_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let erin = local_user(&pool, "erin").await;
        follows(&pool, &carol, &alice).await;
        follows(&pool, &alice, &dave).await;
        follows(&pool, &carol, &erin).await;
        assert_eq!(fresh_user(&pool, carol.id).await.following_count, 2);
        assert_eq!(fresh_user(&pool, dave.id).await.followers_count, 1);

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert_eq!(
            fresh_user(&pool, carol.id).await.following_count,
            1,
            "carol no longer counts alice but keeps erin"
        );
        assert_eq!(
            fresh_user(&pool, dave.id).await.followers_count,
            0,
            "dave no longer counts alice as a follower"
        );
        assert_eq!(
            fresh_user(&pool, erin.id).await.followers_count,
            1,
            "erin's follower is nobody the sweep touches"
        );
        let departed = fresh_user(&pool, alice.id).await;
        assert_eq!(
            (departed.followers_count, departed.following_count),
            (0, 0),
            "the deleted account's own counters are zero"
        );
    }

    /// The deleted local account's emoji reactions go — its row is kept, so
    /// no cascade removes them — while another account's reaction on the
    /// deleted account's status stays, and a second pass changes nothing.
    #[tokio::test]
    async fn the_handler_clears_the_accounts_emoji_reactions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let alices_post = post(&pool, &alice, "alice posts").await;
        let bobs_post = post(&pool, &bob, "bob posts").await;
        reacts(&pool, &alice, bobs_post.id, "\u{1f44d}").await;
        reacts(&pool, &bob, alices_post.id, "\u{1f389}").await;
        assert_eq!(
            count_where(&pool, "emoji_reactions", "status_id", bobs_post.id).await,
            1,
            "alice's reaction renders on bob's status"
        );

        let state = app_state(&pool);
        run(&state, alice.id).await.expect("the first pass runs");
        run(&state, alice.id).await.expect("the second pass runs");

        assert_eq!(
            count_where(&pool, "emoji_reactions", "user_id", alice.id).await,
            0,
            "alice's reaction on bob's status is gone"
        );
        assert_eq!(
            count_where(&pool, "emoji_reactions", "status_id", bobs_post.id).await,
            0,
            "so bob's status no longer shows it"
        );
        assert_eq!(
            count_where(&pool, "emoji_reactions", "user_id", bob.id).await,
            1,
            "bob's own reaction is not alice's to clear"
        );
    }

    /// A pending request from the deleted account to a locked account
    /// goes without moving any counter.
    #[tokio::test]
    async fn a_pending_request_moves_no_counter() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let gated = locked_user(&pool, "gated").await;
        follows(&pool, &alice, &gated).await;
        assert!(has_follow(&pool, &alice, &gated).await, "the request waits");

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert!(
            !has_follow(&pool, &alice, &gated).await,
            "the request went with the account"
        );
        let locked = fresh_user(&pool, gated.id).await;
        assert_eq!(
            (locked.followers_count, locked.following_count),
            (0, 0),
            "a request that never counted steps nothing"
        );
    }

    /// A second pass over the cleared account steps no counter again.
    #[tokio::test]
    async fn a_second_pass_leaves_the_peers_counters_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let carol = local_user(&pool, "carol").await;
        let dave = local_user(&pool, "dave").await;
        let erin = local_user(&pool, "erin").await;
        follows(&pool, &carol, &alice).await;
        follows(&pool, &alice, &dave).await;
        follows(&pool, &carol, &erin).await;
        follows(&pool, &dave, &erin).await;

        let state = app_state(&pool);
        run(&state, alice.id).await.expect("the first pass runs");
        run(&state, alice.id).await.expect("the second pass runs");

        assert_eq!(
            fresh_user(&pool, carol.id).await.following_count,
            1,
            "carol's remaining follow of erin still counts"
        );
        assert_eq!(fresh_user(&pool, dave.id).await.followers_count, 0);
        assert_eq!(
            fresh_user(&pool, dave.id).await.following_count,
            1,
            "dave's follow of erin is untouched by the rerun"
        );
        assert_eq!(fresh_user(&pool, erin.id).await.followers_count, 2);
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
                detail.starts_with("delete user read failed"),
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
        let registry = crate::job_registry(&crate::config::Config::for_tests());
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

    /// The sweep takes the account's scheduled statuses and cancels each
    /// one's pending publish job, leaving another account's schedule and
    /// job alone.
    #[tokio::test]
    async fn the_handler_deletes_schedules_and_cancels_their_jobs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let state = app_state(&pool);
        let first = scheduled_with_job(&state, &alice).await;
        let second = scheduled_with_job(&state, &alice).await;
        let bobs = scheduled_with_job(&state, &bob).await;
        let key = scheduled_statuses::unique_key;
        assert_eq!(available_jobs_keyed(&pool, &key(first.id)).await, 1);

        run(&state, alice.id).await.expect("the delete job runs");

        assert_eq!(
            count_where(&pool, "scheduled_statuses", "user_id", alice.id).await,
            0,
            "alice has no schedule left"
        );
        for gone in [first.id, second.id] {
            assert_eq!(
                available_jobs_keyed(&pool, &key(gone)).await,
                0,
                "the publish job of schedule {gone} is cancelled"
            );
        }
        assert_eq!(
            count_where(&pool, "scheduled_statuses", "id", bobs.id).await,
            1,
            "bob's schedule stays"
        );
        assert_eq!(
            available_jobs_keyed(&pool, &key(bobs.id)).await,
            1,
            "and so does its job"
        );
    }

    /// A processed backup's file leaves the uploader and its row goes; a
    /// backup whose file is already missing, and one that never got a file,
    /// lose their rows too; another account's backup and file stay.
    #[tokio::test]
    async fn the_handler_deletes_backups_rows_and_files() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (dir, state) = uploads_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let (stored, stored_key) = processed_backup(&state, &alice, b"alice's dms").await;
        let (lapsed, lapsed_key) = processed_backup(&state, &alice, b"already gone").await;
        state
            .uploader
            .delete(&lapsed_key)
            .await
            .expect("the fixture file goes first");
        let requested = backup::create(&pool, &alice)
            .await
            .expect("the unprocessed fixture creates");
        let (bobs, bobs_key) = processed_backup(&state, &bob, b"bob's dms").await;

        run(&state, alice.id).await.expect("the delete job runs");

        for gone in [stored.id, lapsed.id, requested.id] {
            assert_eq!(
                count_where(&pool, "backups", "id", gone).await,
                0,
                "backup row {gone} is gone"
            );
        }
        assert!(
            !dir.path().join(&stored_key).exists(),
            "the archive's file is gone from the uploader"
        );
        assert_eq!(count_where(&pool, "backups", "id", bobs.id).await, 1);
        assert!(
            dir.path().join(&bobs_key).is_file(),
            "bob's archive is not alice's to delete"
        );
    }

    /// A file delete that fails for any reason but "already gone" retries
    /// the pass and keeps the row, so the archive is never orphaned.
    #[tokio::test]
    async fn a_failing_archive_delete_retries_and_keeps_the_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (dir, state) = uploads_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let (stuck, key) = processed_backup(&state, &alice, b"stuck").await;
        state
            .uploader
            .delete(&key)
            .await
            .expect("the fixture file goes first");
        std::fs::create_dir_all(dir.path().join(&key)).expect("a directory takes the file's path");

        match run(&state, alice.id).await {
            Err(JobError::Retry(detail)) => assert!(
                detail.contains(&key),
                "the retry names the file that stuck: {detail}"
            ),
            other => panic!("a failed file delete must retry, not {other:?}"),
        }
        assert_eq!(
            count_where(&pool, "backups", "id", stuck.id).await,
            1,
            "the row stays for the next pass"
        );
        assert_eq!(
            user::get_by_id(&pool, alice.id)
                .await
                .unwrap()
                .expect("the row is there")
                .email
                .as_deref(),
            Some("alice@example.org"),
            "and the personal data is not purged before the retry"
        );
    }

    /// The account's push subscriptions go; another account's stay.
    #[tokio::test]
    async fn the_handler_deletes_push_subscriptions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        push_subscription(&pool, &alice, "https://push.example/alice-1").await;
        push_subscription(&pool, &alice, "https://push.example/alice-2").await;
        push_subscription(&pool, &bob, "https://push.example/bob").await;

        run(&app_state(&pool), alice.id)
            .await
            .expect("the delete job runs");

        assert_eq!(
            count_where(&pool, "web_push_subscriptions", "user_id", alice.id).await,
            0,
            "alice has no subscription left"
        );
        assert_eq!(
            count_where(&pool, "web_push_subscriptions", "user_id", bob.id).await,
            1,
            "bob's subscription stays"
        );
    }

    /// The other participant of a chat no longer reads what the deleted
    /// account sent: its messages are blank tombstones that drop off the
    /// page, and the other participant's own messages stay. A second pass
    /// changes nothing.
    #[tokio::test]
    async fn the_handler_withdraws_the_chat_messages_the_account_sent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let first = send_chat(&pool, &alice, &bob, "alice says one").await;
        let second = send_chat(&pool, &alice, &bob, "alice says two").await;
        let reply = send_chat(&pool, &bob, &alice, "bob answers").await;
        let bobs_chat = first.chat_id;
        assert_eq!(
            listed_messages(&pool, bobs_chat).await.len(),
            3,
            "bob reads all three messages before the sweep"
        );

        let state = app_state(&pool);
        run(&state, alice.id).await.expect("the delete job runs");

        assert_eq!(
            listed_messages(&pool, bobs_chat).await,
            vec![reply.message_id],
            "bob's page keeps only bob's own message"
        );
        for gone in [first.message_id, second.message_id] {
            assert!(
                is_blank_tombstone(&pool, gone).await,
                "alice's message {gone} is a blank tombstone"
            );
        }
        assert!(
            !is_blank_tombstone(&pool, reply.message_id).await,
            "bob's reply is not alice's to withdraw"
        );

        run(&state, alice.id).await.expect("the second pass runs");
        assert_eq!(
            listed_messages(&pool, bobs_chat).await,
            vec![reply.message_id],
            "the rerun changes nothing"
        );
    }

    /// A second pass over an account whose schedules, backups and
    /// subscriptions are gone succeeds and changes nothing.
    #[tokio::test]
    async fn a_second_pass_over_the_new_steps_is_a_no_op() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (_dir, state) = uploads_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _row = scheduled_with_job(&state, &alice).await;
        let _backup = processed_backup(&state, &alice, b"dms").await;
        push_subscription(&pool, &alice, "https://push.example/alice").await;
        let _bobs = processed_backup(&state, &bob, b"bob").await;

        run(&state, alice.id).await.expect("the first pass runs");
        run(&state, alice.id).await.expect("the second pass runs");

        for table in ["scheduled_statuses", "backups", "web_push_subscriptions"] {
            assert_eq!(
                count_where(&pool, table, "user_id", alice.id).await,
                0,
                "{table} stays empty for alice"
            );
        }
        assert_eq!(
            count_where(&pool, "backups", "user_id", bob.id).await,
            1,
            "the rerun touched nobody else"
        );
    }
}
