//! The delivery itself: the gates a notification passes, the payload built
//! from it and the signed send to the push service.

use sqlx::PgPool;
use tracing::debug;
use uuid::Uuid;

use super::{PASS_JOB, text::body_text};
use crate::{
    entities::account::{DEFAULT_AVATAR_PATH, plain_display_name, profile_image},
    jobs::{JobError, JobErrorExt as _, retry},
    models::{
        follow,
        notification::{self, Notification},
        oauth_token, user,
        web_push_subscription::{self, WebPushSubscription},
    },
    services::{vapid, web_push},
    state::AppState,
};

/// The job's body: load the subscription, then run [`deliver_for`]. A
/// subscription the queue still holds a job for but the database no longer
/// has is already retired — complete quietly.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure, and whatever [`deliver_for`]
/// reports.
pub(super) async fn deliver(
    state: &AppState,
    notification_id: Uuid,
    subscription_id: Uuid,
) -> Result<(), JobError> {
    let found = web_push_subscription::find_by_id(&state.db, subscription_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let Some(subscription) = found else {
        debug!(subscription = %subscription_id, "push subscription is already gone");
        return Ok(());
    };
    deliver_for(state, &subscription, notification_id).await
}

/// The gates: the subscription's owner, its notification, its alert flag
/// and its policy. Each way of having nothing to deliver is a silent
/// completion, not an error.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure, and whatever [`send_note`]
/// reports.
async fn deliver_for(
    state: &AppState,
    subscription: &WebPushSubscription,
    notification_id: Uuid,
) -> Result<(), JobError> {
    let Some(recipient_id) = subscription.user_id else {
        debug!(subscription = %subscription.id, "app-only subscription has nobody to wake");
        return Ok(());
    };
    let found = notification::get_visible(&state.db, recipient_id, notification_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let Some(note) = found else {
        debug!(
            notification = %notification_id,
            "notification is gone, dismissed, blocked or muted before delivery"
        );
        return Ok(());
    };
    if !web_push::alert_enabled(&subscription.alerts, &note.event_type) {
        debug!(
            subscription = %subscription.id,
            notification_type = %note.event_type,
            "the subscription's alert flag is off"
        );
        return Ok(());
    }
    if !policy_permits(&state.db, subscription, recipient_id, note.from_account_id).await? {
        debug!(
            subscription = %subscription.id,
            policy = %subscription.policy,
            "the subscription's policy keeps this actor out"
        );
        return Ok(());
    }
    send_note(state, subscription, recipient_id, &note).await
}

/// Whether `subscription`'s policy lets `actor_id` wake `recipient_id`:
/// the ticket's reading of Mastodon's four — `followed` asks whether the
/// recipient follows the actor, `follower` whether the actor follows the
/// recipient, and any other stored spelling (`none` included) stays dark.
///
/// # Errors
///
/// [`JobError::Retry`] on database failure.
async fn policy_permits(
    pool: &PgPool,
    subscription: &WebPushSubscription,
    recipient_id: Uuid,
    actor_id: Uuid,
) -> Result<bool, JobError> {
    match subscription.policy.as_str() {
        "all" => Ok(true),
        "followed" => follow::has_accepted_follow(pool, recipient_id, actor_id)
            .await
            .map_err(|err| retry(PASS_JOB, &err)),
        "follower" => follow::has_accepted_follow(pool, actor_id, recipient_id)
            .await
            .map_err(|err| retry(PASS_JOB, &err)),
        _ => Ok(false),
    }
}

/// The delivery itself: assemble the payload from the actor, the
/// recipient's settings and the status text, sign the
/// VAPID header, and send. A `Gone` answer retires the subscription and
/// completes; a `Retryable` one becomes the queue's retry.
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure or a retryable push failure,
/// [`JobError::Discard`] for VAPID material that cannot sign at all.
async fn send_note(
    state: &AppState,
    subscription: &WebPushSubscription,
    recipient_id: Uuid,
    note: &Notification,
) -> Result<(), JobError> {
    let pool = &state.db;
    let Some(actor) = user::get_by_id(pool, note.from_account_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?
    else {
        debug!(account = %note.from_account_id, "the notification's actor is gone");
        return Ok(());
    };
    let Some(recipient) = user::get_by_id(pool, recipient_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?
    else {
        return Ok(());
    };
    let found = oauth_token::get_by_id(pool, subscription.token_id)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    if found.is_none() {
        debug!(token = %subscription.token_id, "the subscription's token is gone");
        return Ok(());
    }
    let body = body_text(pool, &recipient, note).await?;
    let icon = profile_image(
        &actor,
        &state.config,
        actor.avatar_url.as_deref(),
        DEFAULT_AVATAR_PATH,
    );
    let title_name = plain_display_name(&actor);
    let payload = web_push::payload_json(&web_push::PushPayload {
        notification_id: note.id,
        notification_type: &note.event_type,
        icon: &icon,
        actor_display_name: &title_name,
        body: &body,
    });
    let authorization = vapid_authorization(state, &subscription.endpoint).await?;
    match web_push::send(&state.http_client, subscription, &payload, &authorization).await {
        Ok(()) => {
            debug!(
                subscription = %subscription.id,
                notification = %note.id,
                "web push delivered"
            );
            Ok(())
        }
        Err(web_push::PushError::Gone) => {
            let deleted = web_push_subscription::delete_for_token(pool, subscription.token_id)
                .await
                .map_err(|err| retry(PASS_JOB, &err))?;
            debug!(
                subscription = %subscription.id,
                deleted,
                "push subscription retired by the push service"
            );
            Ok(())
        }
        Err(web_push::PushError::Retryable(detail)) => Err(JobError::Retry(detail)),
    }
}

/// The `Authorization` header for one send: the stored VAPID pair (seeded
/// on first use, see [`vapid::vapid_public_key`]) signed for `endpoint`
/// with the instance's [`web_push::vapid_subject`].
///
/// # Errors
///
/// [`JobError::Retry`] on a database failure reading the pair,
/// [`JobError::Discard`] when the stored material cannot sign — a broken
/// key or endpoint no retry fixes.
async fn vapid_authorization(state: &AppState, endpoint: &str) -> Result<String, JobError> {
    use crate::models::instance_secret;

    let public_key = vapid::vapid_public_key(&state.db)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let stored = instance_secret::get(&state.db, vapid::VAPID_PRIVATE_NAME)
        .await
        .map_err(|err| retry(PASS_JOB, &err))?;
    let private_pem = stored.ok_or_else(|| {
        JobError::Discard("the instance has no VAPID private key to sign with".to_owned())
    })?;
    web_push::vapid_headers(
        endpoint,
        &private_pem,
        &public_key,
        &web_push::vapid_subject(&state.config),
    )
    .map(|(authorization, _expires_at)| authorization)
    .job_discard("the VAPID JWT could not be signed")
}

#[cfg(test)]
mod send_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::task::JoinHandle;

    use super::super::test_support::{
        Pref, Seen, account, client_keys, local, mock_push_service, note_about, open_pref, run,
        served, setup, subscribe,
    };
    use super::*;
    use crate::{config::TEST_BASE_URL, test_support::app_state};

    #[tokio::test]
    async fn a_missing_subscription_or_notification_completes_quietly() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        assert_eq!(
            run(&state, Uuid::now_v7(), Uuid::now_v7()).await,
            Ok(()),
            "rows that are gone retire the delivery"
        );
        let acct = account(&pool, "wp-dead-bob").await;
        let subscription = subscribe(
            &pool,
            &acct,
            "https://push.invalid/x",
            &client_keys(),
            &open_pref(),
        )
        .await;
        assert_eq!(
            run(&state, Uuid::now_v7(), subscription.id).await,
            Ok(()),
            "and so does a notification that never existed for the owner"
        );
    }

    #[tokio::test]
    async fn a_disabled_alert_sends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        // Re-register the token's subscription with the follow alert off.
        let off = Pref {
            alerts: json!({"follow": false}),
            policy: "all",
        };
        let off_sub = subscribe(&pool, &s.recipient, &s.url, &s.keys, &off).await;

        let outcome = run(&s.state, s.note.id, off_sub.id).await;

        assert_eq!(outcome, Ok(()), "an off flag is a clean skip");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    #[tokio::test]
    async fn an_actor_blocked_after_enqueueing_sends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        let _edge = crate::models::block::block(&pool, TEST_BASE_URL, &s.recipient.user, &s.actor)
            .await
            .expect("the block lands after the job was queued");

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(outcome, Ok(()), "a blocked actor retires the job quietly");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    #[tokio::test]
    async fn an_actor_muted_after_enqueueing_sends_nothing() {
        use crate::models::mute::{self, MuteOptions};

        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        let _mute = mute::mute(&pool, &s.recipient.user, &s.actor, MuteOptions::default())
            .await
            .expect("the mute lands after the job was queued");

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(outcome, Ok(()), "a muted actor retires the job quietly");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    #[tokio::test]
    async fn a_dismissed_notification_sends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        let _dismissed = notification::dismiss(&pool, s.recipient.user.id, s.note.id)
            .await
            .expect("the dismissal runs");

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(outcome, Ok(()), "a dismissed row retires the job quietly");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    #[tokio::test]
    async fn a_deactivated_actor_sends_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let s = setup(&pool, 201).await;
        let _inactive = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(s.actor.id)
            .execute(&pool)
            .await
            .expect("the deactivation lands");

        let outcome = run(&s.state, s.note.id, s.subscription.id).await;

        assert_eq!(outcome, Ok(()), "a deactivated actor retires the job");
        assert!(!served(&s.seen), "nothing reached the push service");
        s.server.abort();
    }

    /// The follow edges a policy case draws between actor and recipient.
    #[derive(Clone, Copy)]
    struct Edges {
        /// Whether the recipient follows the actor.
        recipient_follows_actor: bool,
        /// Whether the actor follows the recipient.
        actor_follows_recipient: bool,
    }

    /// Nobody follows anybody: the both-edges-off corner of the matrix.
    const NO_EDGES: Edges = Edges {
        recipient_follows_actor: false,
        actor_follows_recipient: false,
    };
    /// Only the recipient follows the actor.
    const RECIPIENT_FOLLOWS: Edges = Edges {
        recipient_follows_actor: true,
        actor_follows_recipient: false,
    };
    /// Only the actor follows the recipient.
    const ACTOR_FOLLOWS: Edges = Edges {
        recipient_follows_actor: false,
        actor_follows_recipient: true,
    };
    /// A mutual follow.
    const BOTH_WAYS: Edges = Edges {
        recipient_follows_actor: true,
        actor_follows_recipient: true,
    };

    /// The policy fixture: a `follow` notification from a named actor to
    /// a subscription with `policy`, over the `edges`' follow relationships,
    /// returning the job's outcome beside the mock's capture.
    async fn policy_case(
        pool: &PgPool,
        policy: &'static str,
        nickname: &str,
        edges: Edges,
    ) -> (Result<(), JobError>, Seen, JoinHandle<()>) {
        let actor = local(pool, &format!("{nickname}-actor"), "Actor").await;
        let recipient = account(pool, &format!("{nickname}-target")).await;
        let (url, seen, server) = mock_push_service(201).await;
        let subscription = subscribe(
            pool,
            &recipient,
            &url,
            &client_keys(),
            &Pref {
                alerts: json!({}),
                policy,
            },
        )
        .await;
        if edges.recipient_follows_actor {
            let _edge = follow::follow(pool, TEST_BASE_URL, &recipient.user, &actor)
                .await
                .expect("the recipient's follow writes");
        }
        if edges.actor_follows_recipient {
            let _edge = follow::follow(pool, TEST_BASE_URL, &actor, &recipient.user)
                .await
                .expect("the actor's follow writes");
        }
        let note = note_about(
            pool,
            &recipient.user,
            &actor,
            notification::TYPE_FOLLOW,
            None,
        )
        .await;
        let outcome = run(&app_state(pool), note.id, subscription.id).await;
        (outcome, seen, server)
    }

    #[tokio::test]
    async fn policies_gate_the_wake_by_the_follow_edges() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();

        let (all_out, all_seen, all_server) = policy_case(&pool, "all", "plall", NO_EDGES).await;
        assert_eq!(all_out, Ok(()), "`all` answers for anyone");
        assert!(served(&all_seen), "and the stranger got through");
        all_server.abort();

        let (none_out, none_seen, none_server) =
            policy_case(&pool, "none", "plnon", BOTH_WAYS).await;
        assert_eq!(none_out, Ok(()), "`none` answers for nobody");
        assert!(!served(&none_seen), "not even a mutual follow wakes it");
        none_server.abort();

        let (_f1_out, f1_seen, f1_server) =
            policy_case(&pool, "followed", "plf1", RECIPIENT_FOLLOWS).await;
        assert!(
            served(&f1_seen),
            "`followed` wakes when the recipient follows"
        );
        f1_server.abort();
        let (_f2_out, f2_seen, f2_server) =
            policy_case(&pool, "followed", "plf2", ACTOR_FOLLOWS).await;
        assert!(
            !served(&f2_seen),
            "`followed` stays dark when only the actor follows"
        );
        f2_server.abort();

        let (_r1_out, r1_seen, r1_server) =
            policy_case(&pool, "follower", "plr1", ACTOR_FOLLOWS).await;
        assert!(
            served(&r1_seen),
            "`follower` wakes for the recipient's followers"
        );
        r1_server.abort();
        let (_r2_out, r2_seen, r2_server) =
            policy_case(&pool, "follower", "plr2", RECIPIENT_FOLLOWS).await;
        assert!(
            !served(&r2_seen),
            "`follower` stays dark when only the recipient follows"
        );
        r2_server.abort();
    }
}
