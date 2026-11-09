//! Notification generation: the write side of telling an account what
//! another account just did (`docs/ARCHITECTURE.md` decision 3).
//!
//! The reference is Pleroma's `Pleroma.Notification.create_notifications`
//! (`lib/pleroma/notification.ex`): every action that ends in the database
//! ends in one telling of the affected local account, with the same rules
//! Pleroma's `skip?` list applies. This module is the single place those
//! tellings are decided, and the action hooks in [`crate::api`] and
//! [`crate::services`] are thin landings that call exactly one of its five
//! functions:
//!
//! - [`on_follow`] — the follow endpoint's telling: `follow` when the
//!   follow is accepted, `follow_request` when a locked account let it
//!   wait; an unfollow takes nothing back, and a pair the endpoint re-found
//!   open is told only once (Pleroma's `recently_followed` skip, through
//!   [`notification::already_notified`]).
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
//!
//! Every creation runs through [`notification::maybe_notify`], so the
//! recipient's `block_from_strangers` setting withholds what it withholds
//! and the exempt [`TYPE_FOLLOW_REQUEST`] alone reaches a gated account
//! through it. Before that, this module's own rules: a notification goes
//! only to a **local** user (the remote one's instance is told by
//! federation, not by a row), and nobody is notified of their own action.
//! Blocked and muted actors are not filtered here — the reads of
//! [`crate::models::notification`] hide their rows (Pleroma's
//! `for_user_query`), which also retroactively suppresses tellings made
//! before the relationship was drawn.
//!
//! The tellings are best-effort, exactly like the delete service's
//! [`after_status_deleted`](crate::services::delete_status): the action
//! they report is already committed, so a failing write is logged with
//! `warn!` and the causing endpoint answers as if the telling had landed.

use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        favourite::Favourite,
        follow::{Follow, FollowState},
        mention,
        notification::{
            self, NewNotification, TYPE_FAVOURITE, TYPE_FOLLOW, TYPE_FOLLOW_REQUEST, TYPE_MENTION,
            TYPE_REBLOG,
        },
        status::{self, Status as StatusRow},
        user::{self, User},
    },
    state::AppState,
};

/// Tell `recipient` (a [`User`] row already in hand) that `actor_id` did
/// `event_type` about `status_id`.
///
/// The shared create arm of every rule above: a recipient who is the actor
/// (self-action) or who is not local is nobody to tell from here, and
/// everyone else's row is decided by [`notification::maybe_notify`]'s own
/// skip rules.
async fn tell(
    state: &AppState,
    recipient: &User,
    actor_id: Uuid,
    event_type: &str,
    status_id: Option<Uuid>,
) -> AppResult<()> {
    if recipient.id == actor_id || !recipient.local.get() {
        return Ok(());
    }
    notification::maybe_notify(
        &state.db,
        &NewNotification {
            user_id: recipient.id,
            from_account_id: actor_id,
            notification_type: event_type,
            status_id,
            emoji: None,
            report_id: None,
        },
    )
    .await
    .map(drop)?;
    Ok(())
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
/// the next telling through — see [`notification::already_notified`]).
async fn tell_once(
    state: &AppState,
    recipient: &User,
    actor_id: Uuid,
    event_type: &str,
) -> AppResult<()> {
    if recipient.local.get()
        && recipient.id != actor_id
        && notification::already_notified(&state.db, recipient.id, actor_id, event_type).await?
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
    notification::delete_matching(&state.db, recipient_id, actor_id, event_type, status_id)
        .await
        .map(drop)?;
    Ok(())
}

/// The follow endpoint's notification, landed from `after_follow_changed`
/// in [`crate::api::accounts`].
///
/// `following` and `follow` say what the write did, and only a new follow
/// tells: a withdrawal takes no telling back, Pleroma's
/// `Notification.create_notifications` never seeing an `Undo` of a
/// `Follow`. A pair the endpoint re-found open is told only once
/// (Pleroma's `recently_followed` skip).
pub async fn on_follow(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) {
    if let Err(err) = follow_telling(state, follower, target, follow, following).await {
        warn!(
            followee = %target.id,
            follower = %follower.id,
            %err,
            "could not tell the followee about a follow"
        );
    }
}

/// [`on_follow`]'s work: the type is the relationship's state — an
/// accepted follow tells `follow`, a locked account's waiting request
/// tells `follow_request`, and a `rejected` row (only a mirrored state,
/// never one these endpoints leave behind) tells nothing.
async fn follow_telling(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) -> AppResult<()> {
    if !following {
        return Ok(());
    }
    let Some(row) = follow else {
        return Ok(());
    };
    let event_type = match row.state {
        FollowState::Accepted => TYPE_FOLLOW,
        FollowState::Pending => TYPE_FOLLOW_REQUEST,
        FollowState::Rejected => return Ok(()),
    };
    tell_once(state, target, follower.id, event_type).await
}

/// The follow-request answer's notification, landed from
/// `after_follow_request_resolved` in [`crate::api::follow_requests`].
///
/// An **accepted** request replaces its own telling — the
/// [`TYPE_FOLLOW_REQUEST`] row the request created is deleted and a
/// `follow` row created in its place (Pleroma's grant runs the accepted
/// `Follow` through its pipeline exactly this way); a rejection deletes
/// nothing and tells nothing, and the request's row stands until its
/// recipient clears it.
pub async fn on_follow_request_resolved(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) {
    if let Err(err) = resolution_telling(state, resolver, request, granted).await {
        warn!(
            user = %resolver.id,
            follower = %request.follower_id,
            %err,
            "could not tell a followee about an answered follow request"
        );
    }
}

/// [`on_follow_request_resolved`]'s work.
async fn resolution_telling(
    state: &AppState,
    resolver: &User,
    request: &Follow,
    granted: bool,
) -> AppResult<()> {
    if !granted {
        return Ok(());
    }
    forget(
        state,
        resolver.id,
        request.follower_id,
        TYPE_FOLLOW_REQUEST,
        None,
    )
    .await?;
    tell_once(state, resolver, request.follower_id, TYPE_FOLLOW).await
}

/// The create service's mention notifications, landed from
/// `after_status_created` in [`crate::services::create_status`].
///
/// Every **local** user the fresh status mentions — the @-names in its
/// text *and* the author of the status it replies to — receives a
/// `mention` naming the new status, each once however many ways they were
/// reached (Pleroma's `maybe_notify_to_recipients` merged into the
/// mentioned list). A reblog row notifies no mentions: its author's telling
/// is the `reblog` from [`on_reblog`]. `expires` and the delivery schedule
/// are nobody's business here — an expiring or scheduled post notifies
/// exactly like a permanent one, at the moment it is created.
pub async fn on_mention(state: &AppState, row: &StatusRow) {
    if let Err(err) = mention_telling(state, row).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell the mentioned accounts about a status"
        );
    }
}

/// [`on_mention`]'s work: the recipients come from the stored
/// `status_mentions` links ([`mention::for_statuses`], the same rows the
/// create linked in its transaction), so what is told matches what was
/// written, and the replied-to author joins by id when the text did not
/// already name them.
async fn mention_telling(state: &AppState, row: &StatusRow) -> AppResult<()> {
    if row.reblog_of_id.is_some() {
        return Ok(());
    }
    let mut recipients: Vec<User> = mention::for_statuses(&state.db, std::slice::from_ref(&row.id))
        .await?
        .remove(&row.id)
        .unwrap_or_default();
    if let Some(parent_author_id) = row.in_reply_to_account_id
        && !recipients.iter().any(|found| found.id == parent_author_id)
        && let Some(parent_author) = user::get_by_id(&state.db, parent_author_id).await?
    {
        recipients.push(parent_author);
    }
    for recipient in &recipients {
        tell(state, recipient, row.user_id, TYPE_MENTION, Some(row.id)).await?;
    }
    Ok(())
}

/// The reblog endpoint's notification, landed from `after_reblog_changed`
/// in [`crate::api::statuses`].
///
/// A boost tells the original's author `reblog`, and the withdrawal of a
/// boost takes exactly that telling back. The notification names the
/// **original** as its status — Pleroma's group key and its rendered
/// notification are both the boosted post, not the boost row — and the
/// `reblog` row's own [`user_id`](StatusRow::user_id) is the actor.
pub async fn on_reblog(state: &AppState, reblog: &StatusRow, reblogging: bool) {
    if let Err(err) = reblog_telling(state, reblog, reblogging).await {
        warn!(
            reblog = %reblog.id,
            %err,
            "could not tell a boosted post's author about the boost"
        );
    }
}

/// [`on_reblog`]'s work: the original is read for its author, and a row
/// that cannot be read back is a deleted original whose notifications
/// [`notification::delete_for_status`] already took with it — nothing
/// left to tell or take back either way.
async fn reblog_telling(state: &AppState, reblog: &StatusRow, reblogging: bool) -> AppResult<()> {
    let Some(original_id) = reblog.reblog_of_id else {
        return Ok(());
    };
    let Some(original) = status::get_by_id(&state.db, original_id).await? else {
        return Ok(());
    };
    if reblogging {
        tell_local(
            state,
            original.user_id,
            reblog.user_id,
            TYPE_REBLOG,
            Some(original_id),
        )
        .await
    } else {
        forget(
            state,
            original.user_id,
            reblog.user_id,
            TYPE_REBLOG,
            Some(original_id),
        )
        .await
    }
}

/// The favourite endpoint's notification, landed from
/// `after_favourite_changed` in [`crate::api::statuses`].
///
/// A like tells the status's author `favourite`, and the withdrawal takes
/// exactly that telling back. The favourite row's
/// [`status_id`](Favourite::status_id) is already the original (both
/// endpoints map a reblog's id to it before the write), and a repeat
/// favourite never reaches the hook, so no deduplication is owed here.
pub async fn on_favourite(state: &AppState, favourite: &Favourite, favouriting: bool) {
    if let Err(err) = favourite_telling(state, favourite, favouriting).await {
        warn!(
            status = %favourite.status_id,
            %err,
            "could not tell a favourited post's author about the like"
        );
    }
}

/// [`on_favourite`]'s work, reading the liked status for its author; a
/// status that cannot be read back is a deleted post whose notifications
/// are already gone (see [`reblog_telling`] for the same arm).
async fn favourite_telling(
    state: &AppState,
    favourite: &Favourite,
    favouriting: bool,
) -> AppResult<()> {
    let Some(liked) = status::get_by_id(&state.db, favourite.status_id).await? else {
        return Ok(());
    };
    if favouriting {
        tell_local(
            state,
            liked.user_id,
            favourite.user_id,
            TYPE_FAVOURITE,
            Some(favourite.status_id),
        )
        .await
    } else {
        forget(
            state,
            liked.user_id,
            favourite.user_id,
            TYPE_FAVOURITE,
            Some(favourite.status_id),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::{favourite, follow, status::NewStatus, user::NewUser},
        streaming::hub::Hub,
        uploads::UploaderBackend,
    };

    /// The fixed test config's instance origin.
    const BASE: &str = crate::config::TEST_BASE_URL;

    /// An [`AppState`] over `pool`: the one the job handlers are tested with.
    fn app_state(pool: &PgPool) -> AppState {
        let config = Arc::new(Config::for_tests());
        AppState {
            db: pool.clone(),
            hub: Hub::new(pool),
            config: Arc::clone(&config),
            http_client: HttpClient::from(config.as_ref()),
            uploader: UploaderBackend::from(config.as_ref()),
            instance_stats_cache: Arc::default(),
            translation_languages_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// A local user named `nickname`.
    async fn local(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser::local(nickname, &user::local_ap_id(BASE, nickname)),
        )
        .await
        .expect("local user fixture")
    }

    /// A local user named `nickname` who answers follows with requests.
    async fn locked(pool: &PgPool, nickname: &str) -> User {
        user::insert(
            pool,
            &NewUser {
                locked: user::Flag::new(true),
                ..NewUser::local(nickname, &user::local_ap_id(BASE, nickname))
            },
        )
        .await
        .expect("locked user fixture")
    }

    /// The mirror of the remote actor `nickname` on `host`.
    async fn remote(pool: &PgPool, nickname: &str, host: &str) -> User {
        let ap_id = format!("https://{host}/users/{nickname}");
        user::insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@{host}"),
                ap_id,
                domain: Some(host.to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("remote user fixture")
    }

    /// A public post by `author`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> StatusRow {
        status::insert(pool, BASE, &NewStatus::local(author, text))
            .await
            .expect("status fixture")
    }

    /// The recipient's stored notification rows, newest first.
    async fn rows(pool: &PgPool, recipient: Uuid) -> Vec<notification::Notification> {
        sqlx::query_as::<_, notification::Notification>(
            "SELECT * FROM notifications WHERE user_id = $1 ORDER BY id DESC",
        )
        .bind(recipient)
        .fetch_all(pool)
        .await
        .expect("the rows read")
    }

    /// The stored notifications of `recipient`, as `(type, from, status)`
    /// triples: what every assertion below compares.
    async fn facts(pool: &PgPool, recipient: Uuid) -> Vec<(String, Uuid, Option<Uuid>)> {
        rows(pool, recipient)
            .await
            .into_iter()
            .map(|row| (row.event_type, row.from_account_id, row.status_id))
            .collect()
    }

    /// The relationship the follow endpoint leaves behind: `follower`
    /// following `target`, straight through the model.
    async fn followed(pool: &PgPool, follower: &User, target: &User) -> Follow {
        follow::follow(pool, BASE, follower, target)
            .await
            .expect("the follow inserts")
    }

    #[tokio::test]
    async fn an_accepted_follow_notifies_the_local_followee_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "nf-follower").await;
        let target = local(pool, "nf-target").await;

        let created = followed(pool, &follower, &target).await;
        assert_eq!(
            created.state,
            FollowState::Accepted,
            "the fixture is granted"
        );
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await,
            vec![(TYPE_FOLLOW.to_owned(), follower.id, None)],
            "the follow lands as one status-less `follow`"
        );

        // The endpoint re-fires the hook for a pair already open; the
        // deduplication keeps the second call silent.
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await.len(),
            1,
            "a re-follow of an open pair tells once"
        );

        // The withdrawal tells nothing new and takes nothing back.
        on_follow(&state, &follower, &target, Some(&created), false).await;
        assert_eq!(
            facts(pool, target.id).await.len(),
            1,
            "an unfollow leaves the telling alone"
        );
    }

    #[tokio::test]
    async fn a_pending_follow_notifies_a_locked_account_with_a_request() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "rq-follower").await;
        let target = locked(pool, "rq-locked").await;

        let created = followed(pool, &follower, &target).await;
        assert_eq!(
            created.state,
            FollowState::Pending,
            "the fixture is a waiting request"
        );
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert_eq!(
            facts(pool, target.id).await,
            vec![(TYPE_FOLLOW_REQUEST.to_owned(), follower.id, None)],
            "the waiting pair is told as a follow request"
        );
    }

    #[tokio::test]
    async fn a_remote_followee_is_told_by_nobody_from_here() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "rf-follower").await;
        let target = remote(pool, "rf-remote", "remote.example").await;

        let created = followed(pool, &follower, &target).await;
        on_follow(&state, &follower, &target, Some(&created), true).await;
        assert!(
            rows(pool, target.id).await.is_empty(),
            "a remote target is the remote instance's business, not ours"
        );
    }

    #[tokio::test]
    async fn an_accepted_request_becomes_a_follow_telling() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let follower = local(pool, "ar-follower").await;
        let resolver = locked(pool, "ar-locked").await;

        let request = followed(pool, &follower, &resolver).await;
        on_follow(&state, &follower, &resolver, Some(&request), true).await;
        let granted = follow::accept(pool, request.id)
            .await
            .expect("the accept runs");
        on_follow_request_resolved(&state, &resolver, &granted, true).await;
        assert_eq!(
            facts(pool, resolver.id).await,
            vec![(TYPE_FOLLOW.to_owned(), follower.id, None)],
            "the request telling is replaced by the follow one"
        );

        // A rejection replaces nothing: the request row stands.
        let other = local(pool, "ar-other").await;
        let second = followed(pool, &other, &resolver).await;
        on_follow(&state, &other, &resolver, Some(&second), true).await;
        on_follow_request_resolved(&state, &resolver, &second, false).await;
        assert_eq!(
            facts(pool, resolver.id).await,
            vec![
                (TYPE_FOLLOW_REQUEST.to_owned(), other.id, None),
                (TYPE_FOLLOW.to_owned(), follower.id, None),
            ],
            "rejecting leaves the request telling where it was"
        );
    }

    /// The mention fixture: `alice` posting `text` straight through the
    /// model with `linked` users stored as the status's mention links, and
    /// answering `reply_to` when one is given.
    async fn mentioned(
        pool: &PgPool,
        alice: &User,
        text: &str,
        linked: &[&User],
        reply_to: Option<(&User, &StatusRow)>,
    ) -> StatusRow {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                mentioned_user_ids: linked.iter().map(|found| found.id).collect(),
                in_reply_to_id: reply_to.map(|(_, parent)| parent.id),
                in_reply_to_account_id: reply_to.map(|(author, _)| author.id),
                ..NewStatus::local(alice, text)
            },
        )
        .await
        .expect("the mention status inserts")
    }

    #[tokio::test]
    async fn a_status_tells_its_local_mentions_once_each() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "mn-alice").await;
        let bob = local(pool, "mn-bob").await;
        let carol = local(pool, "mn-carol").await;
        let mallory = remote(pool, "mn-mallory", "remote.example").await;

        let row = mentioned(
            pool,
            &alice,
            "hi @bob @carol @mallory",
            &[&bob, &carol, &mallory],
            None,
        )
        .await;
        on_mention(&state, &row).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_MENTION.to_owned(), alice.id, Some(row.id))],
            "the first local mention is told"
        );
        assert_eq!(
            facts(pool, carol.id).await,
            vec![(TYPE_MENTION.to_owned(), alice.id, Some(row.id))],
            "and so is the second"
        );
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote mention is not told from here"
        );
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "the author tells themselves nothing"
        );
    }

    #[tokio::test]
    async fn a_reply_tells_its_parent_author_whoever_they_mention() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rp-alice").await;
        let bob = local(pool, "rp-bob").await;
        let carol = local(pool, "rp-carol").await;

        let parent = post(pool, &bob, "a root").await;
        // The reply mentions carol and answers bob: both are told, and the
        // same user reached two ways would be told once.
        let row = mentioned(
            pool,
            &alice,
            "re: @carol and @bob",
            &[&carol, &bob],
            Some((&bob, &parent)),
        )
        .await;
        on_mention(&state, &row).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_MENTION.to_owned(), alice.id, Some(row.id))],
            "the replied-to author is told once although named twice"
        );
        assert_eq!(
            facts(pool, carol.id).await,
            vec![(TYPE_MENTION.to_owned(), alice.id, Some(row.id))],
            "and the named mention beside them"
        );

        // An un-@-mentioned parent author still hears about the reply.
        let plain_parent = post(pool, &carol, "another root").await;
        let quiet_reply = mentioned(
            pool,
            &alice,
            "an answer",
            &[],
            Some((&carol, &plain_parent)),
        )
        .await;
        on_mention(&state, &quiet_reply).await;
        assert_eq!(
            facts(pool, carol.id).await,
            vec![
                (TYPE_MENTION.to_owned(), alice.id, Some(quiet_reply.id)),
                (TYPE_MENTION.to_owned(), alice.id, Some(row.id)),
            ],
            "the replied-to author is told even un-@-mentioned"
        );

        // Mentioning and answering yourself tells yourself nothing.
        let own = post(pool, &alice, "my own root").await;
        let self_reply = mentioned(
            pool,
            &alice,
            "talking to me?",
            &[&alice],
            Some((&alice, &own)),
        )
        .await;
        on_mention(&state, &self_reply).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "mentioning and answering yourself tells yourself nothing"
        );
    }

    #[tokio::test]
    async fn a_reblog_row_notifies_no_mentions() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rb-alice").await;
        let bob = local(pool, "rb-bob").await;

        let original = post(pool, &alice, "boost me").await;
        let reblog = status::insert(
            pool,
            BASE,
            &NewStatus {
                reblog_of_id: Some(original.id),
                ..NewStatus::local(&bob, "")
            },
        )
        .await
        .expect("the reblog fixture inserts");
        on_mention(&state, &reblog).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "the boost's telling is the reblog arm's, never a mention"
        );
    }

    #[tokio::test]
    async fn a_reblog_tells_the_original_author_and_undo_retires_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rr-alice").await;
        let bob = local(pool, "rr-bob").await;

        let original = post(pool, &alice, "boost me").await;
        let (reblog, created) =
            status::create_reblog(pool, BASE, &bob, &original, status::Visibility::Public)
                .await
                .expect("the reblog writes");
        assert!(created, "the fixture is the first boost");
        on_reblog(&state, &reblog, true).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_REBLOG.to_owned(), bob.id, Some(original.id))],
            "the boost is told about the original, not the boost row"
        );

        let tombstone = status::remove_reblog(pool, bob.id, original.id)
            .await
            .expect("the withdrawal runs")
            .expect("the reblog row was there to remove");
        on_reblog(&state, &tombstone, false).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "the undo takes the telling back"
        );
    }

    #[tokio::test]
    async fn a_self_reblog_and_a_remote_author_tell_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "sr-alice").await;
        let mallory = remote(pool, "sr-mallory", "remote.example").await;

        let own = post(pool, &alice, "my post").await;
        let (self_boost, _) =
            status::create_reblog(pool, BASE, &alice, &own, status::Visibility::Public)
                .await
                .expect("the self boost writes");
        on_reblog(&state, &self_boost, true).await;

        let mirror = post(pool, &mallory, "their post").await;
        let (remote_boost, _) =
            status::create_reblog(pool, BASE, &alice, &mirror, status::Visibility::Public)
                .await
                .expect("the boost of the mirror writes");
        on_reblog(&state, &remote_boost, true).await;

        assert!(
            rows(pool, alice.id).await.is_empty() && rows(pool, mallory.id).await.is_empty(),
            "boosting your own post tells nobody, and a remote author is not ours to tell"
        );
    }

    #[tokio::test]
    async fn a_favourite_tells_the_author_and_undo_retires_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "fa-alice").await;
        let bob = local(pool, "fa-bob").await;

        let original = post(pool, &alice, "like me").await;
        let created = favourite::favourite(pool, BASE, bob.id, original.id)
            .await
            .expect("the favourite writes")
            .expect("the pair was free");
        on_favourite(&state, &created, true).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_FAVOURITE.to_owned(), bob.id, Some(original.id))],
            "the like is told to the author"
        );

        let removed = favourite::unfavourite(pool, bob.id, original.id)
            .await
            .expect("the withdrawal runs")
            .expect("the like was there to remove");
        on_favourite(&state, &removed, false).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "the undo takes the telling back"
        );

        // A self-like tells nothing, and the hook never re-fires for a
        // pair already liked — the model answers `None` — so no dedup is
        // owed here.
        let own = favourite::favourite(pool, BASE, alice.id, original.id)
            .await
            .expect("the self favourite writes")
            .expect("the pair was free");
        on_favourite(&state, &own, true).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "liking your own post tells nobody"
        );
    }

    #[tokio::test]
    async fn a_deleted_target_leaves_both_arms_silent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "dt-alice").await;
        let bob = local(pool, "dt-bob").await;

        let doomed = post(pool, &alice, "doomed").await;
        let _tombstone = status::soft_delete(pool, doomed.id)
            .await
            .expect("the delete runs")
            .expect("the row was live");
        let gone_like = Favourite {
            id: Uuid::now_v7(),
            user_id: bob.id,
            status_id: doomed.id,
            ap_id: None,
            inserted_at: Utc::now(),
        };
        on_favourite(&state, &gone_like, true).await;
        on_favourite(&state, &gone_like, false).await;
        let gone_reblog = StatusRow {
            reblog_of_id: Some(doomed.id),
            ..post(pool, &bob, "a boost of the doomed").await
        };
        on_reblog(&state, &gone_reblog, true).await;
        on_reblog(&state, &gone_reblog, false).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a status the reads cannot answer tells nobody and errors nowhere"
        );
    }
}
