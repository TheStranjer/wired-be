//! Notification generation: the write side of telling an account what
//! another account just did (`docs/ARCHITECTURE.md` decision 3).
//!
//! The reference is Pleroma's `Pleroma.Notification.create_notifications`
//! (`lib/pleroma/notification.ex`): every action that ends in the database
//! ends in one telling of the affected local account, with the same rules
//! Pleroma's `skip?` list applies. This module is the single place those
//! tellings are decided, and the action hooks in [`crate::api`] and
//! [`crate::services`] are thin landings that call one of its functions:
//!
//! - [`on_follow`] — the follow endpoint's telling: `follow` when the
//!   follow is accepted, `follow_request` when a locked account let it
//!   wait; an unfollow takes nothing back, and a pair the endpoint re-found
//!   open is told only once (Pleroma's `recently_followed` skip, through
//!   [`notification::already_told`]).
//! - [`on_follow_request_resolved`] — the answer route's replacement: an
//!   accepted request's `follow_request` telling is taken back and a
//!   `follow` one created in its place; a rejection says nothing new.
//! - [`on_mention`] — the create service's telling: every mentioned local
//!   user *and* the replied-to author receive a `mention` naming the fresh
//!   status; a reblog row notifies no mentions (its telling is the
//!   `reblog`), and an expiring or scheduled post is told like any other.
//! - [`on_reblog`] and [`on_favourite`] — the boost and like tellings of
//!   the original's author, naming the *original* as the notification's
//!   status (the group key Pleroma groups boosts and likes on is the
//!   object id, and the entity renders the post that was acted on). The
//!   withdrawal of either takes that one telling back.
//! - [`on_poll_ended`] — the [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY)
//!   sweep's telling: the poll's author *and* every local voter are told
//!   `poll` with the author as actor (Pleroma's `create_poll_notifications`
//!   walks the Question's actor-and-voters list with the self skip waived
//!   for the poll type).
//! - [`on_poll_restarted`] — the edit route's telling of a poll whose
//!   options changed: every local voter whose ballot was deleted is told
//!   `update` naming the post, the author as actor (Mastodon parity).
//! - [`on_status_edited`] — the edit route's telling: every local account
//!   boosting the post is told `update` naming it, the author as actor
//!   (Pleroma's `Update` arm: the object's local repeaters minus the
//!   actor).
//! - [`on_subscribed_status`] — the create service's subscription telling:
//!   every local user *subscribed* to the author (an accepted follow with
//!   its `notify` toggle on) is told `status` of the fresh post, whenever
//!   its visibility can reach them — never of a `direct` post, and never
//!   of a reblog row.
//! - [`on_quote`], [`on_quote_linked`] and [`on_quoted_update`] — the
//!   quote tellings (Mastodon 4.5): the local author of a quoted post is
//!   told `quote` of the quoting status, once, when the quote is created or
//!   linked after the fact, and `quoted_update` whenever that status is
//!   edited; never of a self-quote, a refused quote or one they cannot see.
//! - [`on_reaction_changed`] — the reaction routes' telling: the status's
//!   author is told `pleroma:emoji_reaction` naming the emoji, and the
//!   withdrawal takes exactly that telling back (one emoji's row, not the
//!   other emoji's on the same status).
//! - [`on_move`] — the inbound `Move` handler's telling: every local
//!   follower whose relationship a departing remote account's move was
//!   applied to is told `move`, the *old* account as actor (Mastodon's
//!   `move` notification: the account they followed is now elsewhere).
//!
//! The tellings themselves live one child module per audience: `social`
//! (follows, follow-request answers and moves), `statuses` (mentions,
//! boosts and likes), `edits`, `subscriptions`, `reactions` and `polls`.
//! This root file keeps the module's shared creation plumbing — the
//! tellers and the withdrawal arm every child calls.
//!
//! Every creation runs through [`notification::maybe_notify`], so what the
//! recipient refuses never becomes a row — an actor they blocked, an actor
//! of a domain they blocked, an actor under a live notification mute, a
//! status in a thread they muted ([`notification::recipient_refuses`]) — and
//! their `block_from_strangers` setting withholds what it withholds, the
//! exempt [`TYPE_FOLLOW_REQUEST`](crate::models::notification::TYPE_FOLLOW_REQUEST)
//! alone reaching a gated account through it. Before that, this module's own
//! rules: a notification goes only to a **local** user (the remote one's
//! instance is told by federation, not by a row), and nobody is notified of
//! their own action — the one exemption is [`on_poll_ended`]'s author, whose
//! own poll closing is news *to them* (Pleroma's `:self` skip answering
//! `false` for the `poll` type). Beside the row, every telling of a local
//! recipient queues its Web Push deliveries — one
//! [`push.deliver`](crate::jobs::web_push::KIND_DELIVER) job per registered
//! subscription, [`web_push::enqueue_deliveries`] — best-effort, since the
//! inbox row stands whatever the push service does. A refused telling
//! creates no row, so `announce` publishes no stream event and queues no
//! push job for it (Pleroma's `get_notified_from_activity` drops these
//! receivers first). The reads of [`crate::models::notification`] still
//! hide the same causes (Pleroma's `for_user_query`), which retroactively
//! suppresses tellings made before the relationship was drawn.
//!
//! The tellings are best-effort, exactly like the delete service's
//! [`after_status_deleted`](crate::services::delete_status): the action
//! they report is already committed, so a failing write is logged with
//! `warn!` and the causing endpoint answers as if the telling had landed.
//! The one caller that is *itself* background work — the poll-end sweep —
//! takes the error instead of a log line, so its retry and its
//! [`ended_notified`](crate::models::poll::Poll::ended_notified) flag keep
//! the tellings exactly-once.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        notification::{self, NewNotification, Telling},
        user::{self, User},
    },
    services::{streaming, web_push},
    state::AppState,
};

mod edits;
mod polls;
mod quotes;
mod reactions;
mod social;
mod statuses;
mod subscriptions;

#[cfg(test)]
mod test_support;

pub use edits::on_status_edited;
pub use polls::{on_poll_ended, on_poll_restarted};
pub use quotes::{on_quote, on_quote_linked, on_quoted_update};
pub use reactions::on_reaction_changed;
pub use social::{on_follow, on_follow_request_resolved, on_move};
pub use statuses::{on_favourite, on_mention, on_reblog};
pub use subscriptions::on_subscribed_status;

/// The shared creation arm behind every telling: [`notification::maybe_notify`]
/// for `recipient` — who must be **local**, or there is nobody to tell from
/// here — and, when the row lands, the streaming publish and the queueing
/// of its Web Push deliveries.
///
/// Split out from [`tell`] for the two arms that pair with their own extra
/// checks: [`tell`] adds the self-action skip, and [`on_poll_ended`]'s
/// author arm deliberately skips *without* it (the poll closing on one's
/// own post is one's own news). Both tails stay best-effort here — the
/// streaming emitter logs its own refusals, and a notification that exists
/// but fails to wake a browser is logged, never an error back to the
/// endpoint.
async fn announce(state: &AppState, recipient: &User, note: &NewNotification<'_>) -> AppResult<()> {
    if !recipient.local.get() {
        return Ok(());
    }
    let created = notification::maybe_notify(&state.db, note).await?;
    if let Some(row) = created {
        streaming::notification_created(state, &row).await;
        if let Err(err) = web_push::enqueue_deliveries(&state.db, row.id, recipient.id).await {
            warn!(
                notification = %row.id,
                %err,
                "could not queue web push deliveries"
            );
        }
    }
    Ok(())
}

/// Tell `recipient` (a [`User`] row already in hand) that `actor_id` did
/// `event_type` about `status_id`.
///
/// The shared create arm of the endpoint rules above: a recipient who is
/// the actor (self-action) or who is not local is nobody to tell from here,
/// and everyone else's row is decided by [`notification::maybe_notify`]'s
/// own skip rules.
async fn tell(
    state: &AppState,
    recipient: &User,
    actor_id: Uuid,
    event_type: &str,
    status_id: Option<Uuid>,
) -> AppResult<()> {
    if recipient.id == actor_id {
        return Ok(());
    }
    announce(
        state,
        recipient,
        &NewNotification {
            user_id: recipient.id,
            from_account_id: actor_id,
            notification_type: event_type,
            status_id,
            emoji: None,
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
}

/// [`tell`] for a recipient named by id: the row is read first — an id
/// with no user is nobody to tell — and [`tell`] makes the local and self
/// decisions.
async fn tell_local(
    state: &AppState,
    recipient_id: Uuid,
    actor_id: Uuid,
    event_type: &str,
    status_id: Option<Uuid>,
) -> AppResult<()> {
    let Some(recipient) = user::get_by_id(&state.db, recipient_id).await? else {
        return Ok(());
    };
    tell(state, &recipient, actor_id, event_type, status_id).await
}

/// [`tell`] for the status-less follow pair, told at most once: a live
/// row of `event_type` from `actor_id` already says it, so the fresh one
/// is skipped (Pleroma's `recently_followed` skip; a dismissed one lets
/// the next telling through — see [`notification::already_told`]).
async fn tell_once(
    state: &AppState,
    recipient: &User,
    actor_id: Uuid,
    event_type: &str,
) -> AppResult<()> {
    let telling = Telling {
        user_id: recipient.id,
        from_account_id: actor_id,
        notification_type: event_type,
        status_id: None,
        emoji: None,
    };
    if recipient.local.get()
        && recipient.id != actor_id
        && notification::already_told(&state.db, &telling).await?
    {
        return Ok(());
    }
    tell(state, recipient, actor_id, event_type, None).await
}

/// Take one telling back: the withdrawal arm paired with [`tell`], deleting
/// exactly the rows [`tell`] would have created for this recipient, actor,
/// type and status.
async fn forget(
    state: &AppState,
    recipient_id: Uuid,
    actor_id: Uuid,
    event_type: &str,
    status_id: Option<Uuid>,
) -> AppResult<()> {
    let telling = Telling {
        user_id: recipient_id,
        from_account_id: actor_id,
        notification_type: event_type,
        status_id,
        emoji: None,
    };
    notification::delete_telling(&state.db, &telling)
        .await
        .map(drop)?;
    Ok(())
}

/// The [`NewNotification`] a [`Telling`] key names, carrying the key's
/// emoji — the shape the `poll` and reaction tellings are told in.
const fn told_row<'a>(telling: &Telling<'a>) -> NewNotification<'a> {
    NewNotification {
        user_id: telling.user_id,
        from_account_id: telling.from_account_id,
        notification_type: telling.notification_type,
        status_id: telling.status_id,
        emoji: telling.emoji,
        report_id: None,
        chat_message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        models::user::{self, NewUser},
        services::notify::test_support::{
            BASE, app_state, followed, local, poll_on, post, push_jobs, push_token, reacted,
            register_push, remote, rows,
        },
    };

    #[tokio::test]
    async fn a_told_local_user_gets_one_push_job_per_subscription() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "pp-follower").await;
        let target = local(pool, "pp-target").await;
        let one = push_token(pool, target.id, "pp-app-one").await;
        let two = push_token(pool, target.id, "pp-app-two").await;
        let sub_one = register_push(pool, &target, one).await;
        let sub_two = register_push(pool, &target, two).await;

        let created = followed(pool, &follower, &target).await;
        on_follow(&state, &follower, &target, Some(&created), true).await;

        let notes = rows(pool, target.id).await;
        assert_eq!(notes.len(), 1, "the telling landed");
        let jobs = push_jobs(pool).await;
        assert_eq!(jobs.len(), 2, "one delivery job per subscription");
        let want = [sub_one, sub_two];
        let mut got: Vec<String> = jobs
            .iter()
            .map(|(_queue, args, _attempts)| args["subscription_id"].to_string())
            .collect();
        got.sort();
        let mut want_text: Vec<String> = want.iter().map(|id| format!("\"{id}\"")).collect();
        want_text.sort();
        assert_eq!(got, want_text, "the jobs name both subscriptions");
        for (queue, args, max_attempts) in &jobs {
            assert_eq!(queue, crate::jobs::QUEUE_DEFAULT, "on the default queue");
            assert_eq!(*max_attempts, 3, "the ticket's three attempts");
            assert_eq!(
                args["notification_id"],
                serde_json::Value::String(notes[0].id.to_string()),
                "and the notification the telling created"
            );
        }
    }

    #[tokio::test]
    async fn a_withheld_telling_queues_no_push_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "pw-follower").await;
        // A gated account that follows nobody: maybe_notify withholds the
        // stranger's follow, and the hook queues nothing for it.
        let gated = user::insert(
            pool,
            &NewUser {
                notification_settings: json!({
                    "block_from_strangers": true, "hide_notification_contents": false,
                }),
                ..NewUser::local("pw-gated", &user::local_ap_id(BASE, "pw-gated"))
            },
        )
        .await
        .expect("the gated fixture inserts");
        let token_id = push_token(pool, gated.id, "pw-app").await;
        let _sub = register_push(pool, &gated, token_id).await;

        let created = followed(pool, &follower, &gated).await;
        on_follow(&state, &follower, &gated, Some(&created), true).await;

        assert!(
            rows(pool, gated.id).await.is_empty(),
            "the telling was withheld"
        );
        assert!(
            push_jobs(pool).await.is_empty(),
            "and no delivery job was queued for a notification that never landed"
        );
    }

    #[tokio::test]
    async fn the_skip_rules_still_hold_for_the_new_tellings() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "sk-bob").await;
        // A gated author who follows nobody: every stranger's new telling
        // is withheld exactly like the old ones.
        let gated = user::insert(
            pool,
            &NewUser {
                notification_settings: json!({
                    "block_from_strangers": true, "hide_notification_contents": false,
                }),
                ..NewUser::local("sk-gated", &user::local_ap_id(BASE, "sk-gated"))
            },
        )
        .await
        .expect("the gated fixture inserts");

        let posted = post(pool, &gated, "gate me").await;
        reacted(&state, pool, &bob, &posted, "👍").await;
        assert!(
            rows(pool, gated.id).await.is_empty(),
            "and so is the stranger's reaction"
        );

        let second = post(pool, &gated, "second question").await;
        let second_stand = poll_on(pool, &second, -1).await;
        on_poll_ended(&state, &second_stand)
            .await
            .expect("the closing runs");
        assert!(
            rows(pool, gated.id).await.is_empty(),
            "even the self-exempt closing reaches a gated author only as \
             the setting allows, which for an unfollowed actor it does not"
        );
    }

    /// `author` posting a status that mentions `target`, as the create
    /// service stores it.
    async fn mention_of(
        pool: &sqlx::PgPool,
        author: &User,
        target: &User,
    ) -> crate::models::status::Status {
        crate::models::status::insert(
            pool,
            BASE,
            &crate::models::status::NewStatus {
                mentioned_user_ids: vec![target.id],
                ..crate::models::status::NewStatus::local(author, "hello there")
            },
        )
        .await
        .expect("the mention status inserts")
    }

    /// A recipient with one push subscription, for the refusal tests.
    async fn subscribed(pool: &sqlx::PgPool, nickname: &str) -> User {
        let recipient = local(pool, nickname).await;
        let token_id = push_token(pool, recipient.id, &format!("{nickname}-app")).await;
        let _sub = register_push(pool, &recipient, token_id).await;
        recipient
    }

    #[tokio::test]
    async fn a_blocked_actors_mention_makes_no_row_stream_event_or_push_job() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = subscribed(pool, "rf-alice").await;
        let mallory = local(pool, "rf-mallory").await;
        let _block = crate::models::block::block(pool, BASE, &alice, &mallory)
            .await
            .expect("the block lands");
        let mut rx = state.hub.subscribe();

        let status = mention_of(pool, &mallory, &alice).await;
        on_mention(&state, &status).await;

        assert!(rows(pool, alice.id).await.is_empty(), "no notification row");
        assert!(push_jobs(pool).await.is_empty(), "no push.deliver job");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "no stream event was published"
        );
    }

    #[tokio::test]
    async fn a_domain_blocked_actors_mention_makes_no_row() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = subscribed(pool, "rd-alice").await;
        let mallory = remote(pool, "rd-mallory", "evil.example").await;
        crate::models::domain_block::block_domain(pool, alice.id, "evil.example")
            .await
            .expect("the domain block lands");

        let status = mention_of(pool, &mallory, &alice).await;
        on_mention(&state, &status).await;

        assert!(rows(pool, alice.id).await.is_empty(), "no notification row");
        assert!(push_jobs(pool).await.is_empty(), "and no push job");
    }

    #[tokio::test]
    async fn only_a_live_notification_mute_refuses_the_telling() {
        use crate::models::mute::{self, MuteOptions};
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rm-alice").await;
        let bob = local(pool, "rm-bob").await;

        let _hiding = mute::mute(pool, &alice, &bob, MuteOptions::default())
            .await
            .expect("the hiding mute lands");
        let status = mention_of(pool, &bob, &alice).await;
        on_mention(&state, &status).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a mute that hides notifications refuses the mention"
        );

        let _timeline = mute::mute(
            pool,
            &alice,
            &bob,
            MuteOptions {
                notifications: false,
                expires_in: None,
            },
        )
        .await
        .expect("the timeline-only mute replaces it");
        on_mention(&state, &status).await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            1,
            "a timeline-only mute lets the mention through"
        );

        let _dismissed = notification::delete_all_for_user(pool, alice.id)
            .await
            .expect("the rows clear");
        let _hiding_again = mute::mute(pool, &alice, &bob, MuteOptions::default())
            .await
            .expect("the hiding mute returns");
        let _expired = sqlx::query("UPDATE mutes SET expires_at = now() - interval '1 hour'")
            .execute(pool)
            .await
            .expect("the mute is back-dated");
        on_mention(&state, &status).await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            1,
            "an expired mute refuses nothing"
        );
    }

    #[tokio::test]
    async fn a_muted_thread_refuses_the_favourite() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rt-alice").await;
        let bob = local(pool, "rt-bob").await;
        let original = post(pool, &alice, "mute this thread").await;
        crate::models::conversation_mute::mute(pool, alice.id, original.conversation_id)
            .await
            .expect("the thread mute lands");

        let liked = crate::models::favourite::favourite(pool, BASE, bob.id, original.id)
            .await
            .expect("the favourite writes")
            .expect("the pair was free");
        on_favourite(&state, &liked, true).await;

        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a favourite in a muted thread writes no row"
        );
    }

    #[tokio::test]
    async fn an_unrelated_pair_is_still_told() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = subscribed(pool, "ru-alice").await;
        let bob = local(pool, "ru-bob").await;
        let carol = local(pool, "ru-carol").await;
        let _block = crate::models::block::block(pool, BASE, &alice, &carol)
            .await
            .expect("an unrelated block lands");

        let status = mention_of(pool, &bob, &alice).await;
        on_mention(&state, &status).await;

        assert_eq!(rows(pool, alice.id).await.len(), 1, "the mention lands");
        assert_eq!(push_jobs(pool).await.len(), 1, "and queues its push job");
    }
}
