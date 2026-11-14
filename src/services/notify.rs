//! Notification generation: the write side of telling an account what
//! another account just did (`docs/ARCHITECTURE.md` decision 3).
//!
//! The reference is Pleroma's `Pleroma.Notification.create_notifications`
//! (`lib/pleroma/notification.ex`): every action that ends in the database
//! ends in one telling of the affected local account, with the same rules
//! Pleroma's `skip?` list applies. This module is the single place those
//! tellings are decided, and the action hooks in [`crate::api`] and
//! [`crate::services`] are thin landings that call exactly one of its eleven
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
//! - [`on_poll_vote`] — the vote endpoint's telling: the poll's author is
//!   told `poll` of every ballot, `from` the voter and about the poll's
//!   status, deduplicated per voter and poll so a re-fired hook cannot
//!   stack tellings.
//! - [`on_poll_ended`] — the [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY)
//!   sweep's telling: the poll's author *and* every local voter are told
//!   `poll` with the author as actor (Pleroma's `create_poll_notifications`
//!   walks the Question's actor-and-voters list with the self skip waived
//!   for the poll type).
//! - [`on_status_edited`] — the edit route's telling: every local account
//!   boosting the post is told `update` naming it, the author as actor
//!   (Pleroma's `Update` arm: the object's local repeaters minus the
//!   actor).
//! - [`on_subscribed_status`] — the create service's subscription telling:
//!   every local user *subscribed* to the author (an accepted follow with
//!   its `notify` toggle on) is told `status` of the fresh post, whenever
//!   its visibility can reach them — never of a `direct` post, and never
//!   of a reblog row.
//! - [`on_reaction_changed`] — the reaction routes' telling: the status's
//!   author is told `pleroma:emoji_reaction` naming the emoji, and the
//!   withdrawal takes exactly that telling back (one emoji's row, not the
//!   other emoji's on the same status).
//! - [`on_move`] — the inbound `Move` handler's telling: every local
//!   follower whose relationship a departing remote account's move was
//!   applied to is told `move`, the *old* account as actor (Mastodon's
//!   `move` notification: the account they followed is now elsewhere).
//!
//! Every creation runs through [`notification::maybe_notify`], so the
//! recipient's `block_from_strangers` setting withholds what it withholds
//! and the exempt [`TYPE_FOLLOW_REQUEST`] alone reaches a gated account
//! through it. Before that, this module's own rules: a notification goes
//! only to a **local** user (the remote one's instance is told by
//! federation, not by a row), and nobody is notified of their own action —
//! the one exemption is [`on_poll_ended`]'s author, whose own poll closing
//! is news *to them* (Pleroma's `:self` skip answering `false` for the
//! `poll` type). Beside the row, every telling of a local recipient queues
//! its Web Push deliveries — one [`push.deliver`](crate::jobs::web_push::KIND_DELIVER)
//! job per registered subscription, [`web_push::enqueue_deliveries`] —
//! best-effort, since the inbox row stands whatever the push service does.
//! Blocked and muted actors are not filtered here — the reads of
//! [`crate::models::notification`] hide their rows (Pleroma's
//! `for_user_query`), which also retroactively suppresses tellings made
//! before the relationship was drawn.
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
        emoji_reaction::EmojiReaction as ReactionRow,
        favourite::Favourite,
        follow::{self, Follow, FollowState},
        mention,
        notification::{
            self, NewNotification, TYPE_EMOJI_REACTION, TYPE_FAVOURITE, TYPE_FOLLOW,
            TYPE_FOLLOW_REQUEST, TYPE_MENTION, TYPE_MOVE, TYPE_POLL, TYPE_REBLOG, TYPE_STATUS,
            TYPE_UPDATE, Telling,
        },
        poll::{self, Poll as PollRow},
        status::{self, Status as StatusRow, Visibility},
        user::{self, User},
    },
    services::web_push,
    state::AppState,
};

/// The shared creation arm behind every telling: [`notification::maybe_notify`]
/// for `recipient` — who must be **local**, or there is nobody to tell from
/// here — and, when the row lands, the queueing of its Web Push deliveries.
///
/// Split out from [`tell`] for the two arms that pair with their own extra
/// checks: [`tell`] adds the self-action skip, and [`on_poll_ended`]'s
/// author arm deliberately skips *without* it (the poll closing on one's
/// own post is one's own news). The push queueing stays best-effort here —
/// a notification that exists but fails to wake a browser is logged, never
/// an error back to the endpoint.
async fn announce(state: &AppState, recipient: &User, note: &NewNotification<'_>) -> AppResult<()> {
    if !recipient.local.get() {
        return Ok(());
    }
    let created = notification::maybe_notify(&state.db, note).await?;
    if let Some(row) = created
        && let Err(err) = web_push::enqueue_deliveries(&state.db, row.id, recipient.id).await
    {
        warn!(
            notification = %row.id,
            %err,
            "could not queue web push deliveries"
        );
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

/// The poll vote endpoint's notification, landed from `after_vote_cast` in
/// [`crate::api::polls`].
///
/// A ballot tells the poll's author `poll`, `from` the voter and about the
/// poll's status (Pleroma's `CommonAPI.vote` runs each `Answer` through its
/// pipeline, which lands the same telling). Deduplicated per voter and
/// poll: the endpoint already refuses a second ballot, so only a re-fired
/// hook can offer the same fact twice, and [`notification::already_told`]
/// keeps that one telling standing.
pub async fn on_poll_vote(state: &AppState, poll: &PollRow, voter: &User) {
    if let Err(err) = poll_vote_telling(state, poll, voter).await {
        warn!(
            poll = %poll.id,
            voter = %voter.id,
            %err,
            "could not tell a poll's author about a ballot"
        );
    }
}

/// [`on_poll_vote`]'s work: the author read off the poll's status, the
/// deduplication on the full telling key, and [`tell`] for the local and
/// self decisions (an author who somehow marked their own poll tells
/// themselves nothing).
async fn poll_vote_telling(state: &AppState, poll: &PollRow, voter: &User) -> AppResult<()> {
    let Some(posted) = status::get_by_id(&state.db, poll.status_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: posted.user_id,
        from_account_id: voter.id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    let Some(author) = user::get_by_id(&state.db, posted.user_id).await? else {
        return Ok(());
    };
    tell(state, &author, voter.id, TYPE_POLL, Some(poll.status_id)).await
}

/// The [`polls.end_notify`](crate::jobs::polls::KIND_END_NOTIFY) sweep's
/// telling: the poll's author and every local voter are told `poll`, with
/// the *author* as the actor, that the question closed.
///
/// Pleroma's `create_poll_notifications` (`lib/pleroma/notification.ex`,
/// run by `lib/pleroma/workers/poll_worker.ex` when a Question's `closed`
/// moment arrives) walks `[actor | voters]` and waives the self skip for
/// the `poll` type — the author is told of their own poll's ending because
/// the *closing*, not the acting, is what the row reports. Here the same
/// pair: the author goes through this module's shared creation arm
/// directly (the local check and the skip rules intact, the self exemption
/// owed to them), and each voter through its self-skipping teller.
///
/// Unlike the endpoint landings this one **returns** its failure: the sweep
/// sets the poll's `ended_notified` flag only after it answers `Ok`, so a
/// retried pass re-runs the tellings — where [`notification::already_told`]
/// keeps each recipient's single row a single row.
///
/// [`polls.end_notify`]: crate::jobs::polls::KIND_END_NOTIFY
///
/// # Errors
///
/// Any database failure behind the status, user, deduplication or insert
/// reads arrives as [`AppError::Database`](crate::error::AppError); the
/// caller owns the retry (see [`handle_end_notify`](crate::jobs::polls::handle_end_notify)).
pub async fn on_poll_ended(state: &AppState, poll: &PollRow) -> AppResult<()> {
    let Some(posted) = status::get_by_id(&state.db, poll.status_id).await? else {
        return Ok(());
    };
    let Some(author) = user::get_by_id(&state.db, posted.user_id).await? else {
        return Ok(());
    };
    let ending = Telling {
        user_id: author.id,
        from_account_id: author.id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if !notification::already_told(&state.db, &ending).await? {
        announce(state, &author, &told_row(&ending)).await?;
    }
    for voter_id in poll::voter_ids(&state.db, poll.id).await? {
        poll_end_voter_telling(state, author.id, poll, voter_id).await?;
    }
    Ok(())
}

/// The [`NewNotification`] a [`Telling`] key names, emoji-free — the shape
/// the `poll` tellings are told in.
const fn told_row<'a>(telling: &Telling<'a>) -> NewNotification<'a> {
    NewNotification {
        user_id: telling.user_id,
        from_account_id: telling.from_account_id,
        notification_type: telling.notification_type,
        status_id: telling.status_id,
        emoji: None,
        report_id: None,
        chat_message_id: None,
    }
}

/// The voter arm of [`on_poll_ended`]: each *other* local voter is told
/// their ballot's question closed, once however many sweep reruns pass
/// (the author cannot mark their own poll, and is told by the arm above
/// whatever this one decides).
async fn poll_end_voter_telling(
    state: &AppState,
    actor_id: Uuid,
    poll: &PollRow,
    voter_id: Uuid,
) -> AppResult<()> {
    if voter_id == actor_id {
        return Ok(());
    }
    let Some(voter) = user::get_by_id(&state.db, voter_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: voter.id,
        from_account_id: actor_id,
        notification_type: TYPE_POLL,
        status_id: Some(poll.status_id),
        emoji: None,
    };
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    tell(state, &voter, actor_id, TYPE_POLL, Some(poll.status_id)).await
}

/// The edit route's notification, landed from `after_status_edited` in
/// [`crate::api::statuses`].
///
/// An edit tells every **local** account still boosting the post an
/// `update` naming it, with the author as the actor (Pleroma's `Update`
/// arm of `get_potential_receiver_ap_ids`: the object's `Announce` actors,
/// `where u.local`, minus the actor — `lib/pleroma/notification.ex`). A
/// withdrawn boost names nobody ([`status::local_reblogger_ids`] reads live
/// rows only), a remote booster is federation's to tell, and the author
/// boosting their own post is no one's news.
pub async fn on_status_edited(state: &AppState, row: &StatusRow) {
    if let Err(err) = edit_telling(state, row).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell a post's boosters about its edit"
        );
    }
}

/// [`on_status_edited`]'s work, one [`tell`] per local booster.
async fn edit_telling(state: &AppState, row: &StatusRow) -> AppResult<()> {
    for booster_id in status::local_reblogger_ids(&state.db, row.id).await? {
        if let Some(booster) = user::get_by_id(&state.db, booster_id).await? {
            tell(state, &booster, row.user_id, TYPE_UPDATE, Some(row.id)).await?;
        }
    }
    Ok(())
}

/// The create service's subscription notifications, landed from
/// `after_status_created` in [`crate::services::create_status`] beside
/// [`on_mention`].
///
/// Every local user **subscribed** to the author — an accepted follow with
/// its `notify` toggle on ([`follow::subscribers`], Pleroma's
/// `User.subscriber_users` behind `get_notified_subscribers`) — is told
/// `status` naming the fresh post. The audience rules, per the ticket:
/// never a reblog row (its telling is the boost's [`on_reblog`], and a
/// boosted post was already the original author's news), never a `direct`
/// post (an address is not a subscription's subject), and every other
/// visibility as its own audience allows — a `local` post reaches the
/// local subscribers this read returns, and remote subscribers, which it
/// never does, are federation's to tell.
pub async fn on_subscribed_status(state: &AppState, row: &StatusRow) {
    if let Err(err) = subscription_telling(state, row).await {
        warn!(
            status = %row.id,
            %err,
            "could not tell an author's subscribers about their status"
        );
    }
}

/// [`on_subscribed_status`]'s work: the two never-arms answered before any
/// read, then one [`tell`] per subscriber (whose own mention telling, from
/// [`on_mention`], is a separate fact and stands beside this one).
async fn subscription_telling(state: &AppState, row: &StatusRow) -> AppResult<()> {
    if row.reblog_of_id.is_some() || row.visibility == Visibility::Direct {
        return Ok(());
    }
    for subscriber in follow::subscribers(&state.db, row.user_id).await? {
        tell(state, &subscriber, row.user_id, TYPE_STATUS, Some(row.id)).await?;
    }
    Ok(())
}

/// The reaction routes' notification, landed from `after_reaction_changed`
/// in [`crate::api::emoji_reactions`].
///
/// A reaction tells the status's author `pleroma:emoji_reaction` with the
/// emoji stored on the row (Pleroma's `EmojiReact` arm: the potential
/// receiver is the object's actor alone), and the withdrawal takes exactly
/// that telling back — the one emoji's row, so a second reaction with a
/// different emoji on the same status stands. The repeat `PUT` an
/// idempotent write answers again is deduplicated on the full key
/// ([`notification::already_told`]); a `DELETE` that found no reaction
/// never reaches the hook, and a deleted status has lost its
/// notifications to [`notification::delete_for_status`] anyway.
pub async fn on_reaction_changed(state: &AppState, reaction: &ReactionRow, reacting: bool) {
    if let Err(err) = reaction_telling(state, reaction, reacting).await {
        warn!(
            status = %reaction.status_id,
            emoji = %reaction.emoji,
            %err,
            "could not tell a reacted post's author about the reaction"
        );
    }
}

/// [`on_reaction_changed`]'s work, create and withdraw arms.
async fn reaction_telling(
    state: &AppState,
    reaction: &ReactionRow,
    reacting: bool,
) -> AppResult<()> {
    let Some(reacted) = status::get_by_id(&state.db, reaction.status_id).await? else {
        return Ok(());
    };
    let telling = Telling {
        user_id: reacted.user_id,
        from_account_id: reaction.user_id,
        notification_type: TYPE_EMOJI_REACTION,
        status_id: Some(reaction.status_id),
        emoji: Some(reaction.emoji.as_str()),
    };
    if !reacting {
        return notification::delete_telling(&state.db, &telling)
            .await
            .map(drop);
    }
    if notification::already_told(&state.db, &telling).await? {
        return Ok(());
    }
    let Some(author) = user::get_by_id(&state.db, reacted.user_id).await? else {
        return Ok(());
    };
    if author.id == reaction.user_id {
        return Ok(());
    }
    announce(
        state,
        &author,
        &NewNotification {
            user_id: author.id,
            from_account_id: reaction.user_id,
            notification_type: TYPE_EMOJI_REACTION,
            status_id: Some(reaction.status_id),
            emoji: Some(reaction.emoji.as_str()),
            report_id: None,
            chat_message_id: None,
        },
    )
    .await
}

/// The inbound `Move` handler's telling, run by
/// [`crate::federation::handlers::misc`] for every local follower whose
/// relationship the migration was applied to.
///
/// The follower is told `move` with the **old** account as actor (Mastodon's
/// `move` notification: the account they followed declared a new one, and
/// this instance moved them). The migration's new follow tells no `follow`:
/// the endpoint's [`on_follow`] audience is the client API's follows, and a
/// migrated relationship is the move itself, which this one row reports.
/// Deduplicated on [`notification::already_notified`] through this module's
/// `tell_once`: a `Move` redelivered after the ledger's fortnight expires
/// tells an already-told follower nothing new.
pub async fn on_move(state: &AppState, follower: &User, moved: &User) {
    if let Err(err) = tell_once(state, follower, moved.id, TYPE_MOVE).await {
        warn!(
            follower = %follower.id,
            moved = %moved.id,
            %err,
            "could not tell a follower about an account's move"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::{
        config::Config,
        http_client::HttpClient,
        models::{
            emoji_reaction::{self, NewReaction},
            favourite, follow,
            oauth_app::{self, NewOauthApp},
            oauth_token::{self, NewToken},
            status::NewStatus,
            user::NewUser,
            web_push_subscription::{self, NewSubscription},
        },
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
            mrf_config_cache: Arc::default(),
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
            status::create_reblog(pool, BASE, &bob, &original, Visibility::Public)
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
        let (self_boost, _) = status::create_reblog(pool, BASE, &alice, &own, Visibility::Public)
            .await
            .expect("the self boost writes");
        on_reblog(&state, &self_boost, true).await;

        let mirror = post(pool, &mallory, "their post").await;
        let (remote_boost, _) =
            status::create_reblog(pool, BASE, &alice, &mirror, Visibility::Public)
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

    /// A fresh `push`-scoped access token of `owner_id`, as a subscription
    /// is keyed to one.
    async fn push_token(pool: &PgPool, owner_id: Uuid, name: &str) -> Uuid {
        let app = oauth_app::create(
            pool,
            &NewOauthApp {
                client_name: name.to_owned(),
                redirect_uris: "https://client.example/callback".to_owned(),
                scopes: "push".to_owned(),
                website: None,
                user_id: Some(owner_id),
            },
        )
        .await
        .expect("the push app fixture inserts");
        oauth_token::create_token(
            pool,
            &NewToken {
                user_id: Some(owner_id),
                app_id: app.id,
                scopes: "push".to_owned(),
                ttl: None,
            },
        )
        .await
        .expect("the push token fixture inserts")
        .id
    }

    /// A registered subscription of `owner`'s `token_id`, follow alert on.
    async fn register_push(pool: &PgPool, owner: &User, token_id: Uuid) -> Uuid {
        web_push_subscription::create(
            pool,
            &NewSubscription {
                user_id: owner.id,
                token_id,
                endpoint: format!("https://push.example/{token_id}"),
                key_p256dh: "BFpush-key".to_owned(),
                key_auth: "push-auth".to_owned(),
                alerts: json!({"follow": true}),
                policy: "all".to_owned(),
            },
        )
        .await
        .expect("the push subscription registers")
        .id
    }

    /// The queued `push.deliver` jobs as `(queue, args, max_attempts)`,
    /// ordered by the subscription each names.
    async fn push_jobs(pool: &PgPool) -> Vec<(String, serde_json::Value, i32)> {
        sqlx::query_as::<_, (String, serde_json::Value, i32)>(
            "SELECT queue, args, max_attempts FROM jobs WHERE kind = $1 \
              ORDER BY args->>'subscription_id'",
        )
        .bind(crate::jobs::web_push::KIND_DELIVER)
        .fetch_all(pool)
        .await
        .expect("the jobs read")
    }

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

    /// The poll fixture: a two-option multiple-choice poll on `posted`,
    /// closing `mins` minutes from now (negative: already closed).
    async fn poll_on(pool: &PgPool, posted: &StatusRow, mins: i64) -> PollRow {
        poll::create(
            &mut pool.acquire().await.expect("a slot connection"),
            posted.id,
            &poll::PollParams {
                options: vec!["tea".to_owned(), "coffee".to_owned()],
                expires_at: Some(Utc::now() + chrono::TimeDelta::minutes(mins)),
                multiple: true,
                ..poll::PollParams::default()
            },
        )
        .await
        .expect("the poll fixture creates")
    }

    /// `voter` marks the first option of `stand`, straight through the
    /// vote write the endpoint uses.
    async fn ballot(pool: &PgPool, stand: &PollRow, voter: &User) {
        let first = poll::options(pool, stand.id)
            .await
            .expect("the options read")
            .into_iter()
            .next()
            .expect("the fixture poll has an option")
            .id;
        assert!(
            poll::cast_vote(pool, stand.id, voter.id, &[first])
                .await
                .expect("the ballot writes"),
            "the fixture pair had not voted yet"
        );
    }

    /// A post of `author` at `visibility`.
    async fn post_vis(
        pool: &PgPool,
        author: &User,
        text: &str,
        visibility: Visibility,
    ) -> StatusRow {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility,
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the visibility status fixture inserts")
    }

    /// `subscriber` holds an accepted, notified follow of `author`.
    async fn subscribed(pool: &PgPool, subscriber: &User, author: &User) {
        let _open = followed(pool, subscriber, author).await;
        assert!(
            follow::set_notify(pool, subscriber.id, author.id, true)
                .await
                .expect("the toggle runs")
                .is_some(),
            "the accepted pair takes the flag"
        );
    }

    /// `reactor` reacts to `target` with `emoji` through the reaction
    /// write, and the hook lands: the pair the `PUT` route runs.
    async fn reacted(
        state: &AppState,
        pool: &PgPool,
        reactor: &User,
        target: &StatusRow,
        emoji: &str,
    ) {
        let stored = emoji_reaction::react(
            pool,
            BASE,
            &NewReaction {
                user_id: reactor.id,
                status_id: target.id,
                emoji,
                url: None,
            },
        )
        .await
        .expect("the reaction writes");
        on_reaction_changed(state, &stored, true).await;
    }

    #[tokio::test]
    async fn a_poll_vote_tells_the_author_once_per_voter_and_poll() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pv-alice").await;
        let bob = local(pool, "pv-bob").await;

        let posted = post(pool, &alice, "vote on me").await;
        let stand = poll_on(pool, &posted, 30).await;
        ballot(pool, &stand, &bob).await;
        on_poll_vote(&state, &stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_POLL.to_owned(), bob.id, Some(posted.id))],
            "the ballot is told to the author, about the poll's status"
        );

        // The same fact re-fired (the endpoint refuses a second ballot, so
        // only a re-run of the hook can offer it again) stands deduped.
        on_poll_vote(&state, &stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await.len(),
            1,
            "one voter and one poll tell once"
        );

        // The same voter on a second poll is a second fact.
        let second = post(pool, &alice, "another question").await;
        let second_stand = poll_on(pool, &second, 30).await;
        ballot(pool, &second_stand, &bob).await;
        on_poll_vote(&state, &second_stand, &bob).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![
                (TYPE_POLL.to_owned(), bob.id, Some(second.id)),
                (TYPE_POLL.to_owned(), bob.id, Some(posted.id)),
            ],
            "the deduplication is per voter *and* poll"
        );

        // The author's own ballot (unreachable through the endpoint's
        // refusal, reachable through a re-fired hook) tells nobody.
        on_poll_vote(&state, &stand, &alice).await;
        assert_eq!(
            facts(pool, alice.id).await.len(),
            2,
            "nobody is told of their own action"
        );
    }

    #[tokio::test]
    async fn a_vote_on_a_remote_or_gone_poll_stays_silent() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "pg-bob").await;

        // A mirrored remote author is told by federation, not by a row.
        let mallory = remote(pool, "pg-mallory", "remote.example").await;
        let theirs = post(pool, &mallory, "their question").await;
        let stand = poll_on(pool, &theirs, 30).await;
        ballot(pool, &stand, &bob).await;
        on_poll_vote(&state, &stand, &bob).await;
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote author is nobody's telling target here"
        );

        // A status deleted after the vote read: the tell finds no author
        // and errors nowhere.
        let alice = local(pool, "pg-alice").await;
        let doomed = post(pool, &alice, "doomed question").await;
        let doomed_stand = poll_on(pool, &doomed, 30).await;
        let _tombstone = status::soft_delete(pool, doomed.id)
            .await
            .expect("the delete runs");
        on_poll_vote(&state, &doomed_stand, &bob).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a poll status the reads cannot answer tells nobody"
        );
    }

    #[tokio::test]
    async fn the_poll_end_tells_the_author_and_the_local_voters_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pe-alice").await;
        let bob = local(pool, "pe-bob").await;
        let carol = local(pool, "pe-carol").await;
        let mallory = remote(pool, "pe-mallory", "remote.example").await;

        let posted = post(pool, &alice, "tea or coffee?").await;
        let stand = poll_on(pool, &posted, -1).await;
        for voter in [&bob, &carol] {
            ballot(pool, &stand, voter).await;
            on_poll_vote(&state, &stand, voter).await;
        }
        ballot(pool, &stand, &mallory).await;

        on_poll_ended(&state, &stand)
            .await
            .expect("the closing telling runs");
        let closing = (TYPE_POLL.to_owned(), alice.id, Some(posted.id));
        assert_eq!(
            facts(pool, alice.id).await,
            vec![
                closing.clone(),
                (TYPE_POLL.to_owned(), carol.id, Some(posted.id)),
                (TYPE_POLL.to_owned(), bob.id, Some(posted.id)),
            ],
            "the author keeps the two ballot tellings and gains their own \
             closing — the self skip's one exemption"
        );
        for voter in [&bob, &carol] {
            assert_eq!(
                facts(pool, voter.id).await,
                vec![closing.clone()],
                "each local voter is told the closing, from the author"
            );
        }
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "a remote voter is not ours to tell"
        );

        // The sweep's rerun (a retried job pass) retells nobody.
        on_poll_ended(&state, &stand).await.expect("the rerun runs");
        assert_eq!(
            facts(pool, alice.id).await.len(),
            3,
            "the author is not told twice"
        );
        assert_eq!(
            facts(pool, bob.id).await,
            vec![closing],
            "and neither is the voter"
        );
    }

    #[tokio::test]
    async fn a_closing_on_a_gone_poll_status_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pd-alice").await;
        let posted = post(pool, &alice, "doomed closing").await;
        let stand = poll_on(pool, &posted, -1).await;
        let _tombstone = status::soft_delete(pool, posted.id)
            .await
            .expect("the delete runs");

        on_poll_ended(&state, &stand)
            .await
            .expect("the gone status is done work, not an error");
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "nobody is told about a deleted question"
        );
    }

    #[tokio::test]
    async fn an_edit_tells_the_boosters_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "ed-alice").await;
        let bob = local(pool, "ed-bob").await;
        let carol = local(pool, "ed-carol").await;
        let mallory = remote(pool, "ed-mallory", "remote.example").await;

        let posted = post(pool, &alice, "worth boosting").await;
        let _bobs_boost = status::create_reblog(pool, BASE, &bob, &posted, Visibility::Public)
            .await
            .expect("bob's boost writes");
        let _carols_boost = status::create_reblog(pool, BASE, &carol, &posted, Visibility::Public)
            .await
            .expect("carol's boost starts");
        // Carol withdraws hers: a tombstoned boost names nobody.
        let _tombstone = status::remove_reblog(pool, carol.id, posted.id)
            .await
            .expect("the withdrawal runs");
        // A remote account's boost is federation's to tell.
        let _mirrored_boost =
            status::create_reblog(pool, BASE, &mallory, &posted, Visibility::Public)
                .await
                .expect("the mirrored boost writes");
        // And the author boosting her own post is her own news.
        let _own_boost = status::create_reblog(pool, BASE, &alice, &posted, Visibility::Public)
            .await
            .expect("the self boost writes");

        on_status_edited(&state, &posted).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_UPDATE.to_owned(), alice.id, Some(posted.id))],
            "the one live local booster is told, naming the edited original"
        );
        for quiet in [&alice, &carol] {
            assert!(
                rows(pool, quiet.id).await.is_empty(),
                "{} (author or withdrawn booster) hears nothing",
                quiet.nickname
            );
        }
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "the remote booster is not told from here"
        );

        // A second edit — and a booster who took the boost back since —
        // reaches exactly the audience that stands.
        let _gone = status::remove_reblog(pool, bob.id, posted.id)
            .await
            .expect("bob withdraws after the first edit");
        on_status_edited(&state, &posted).await;
        assert_eq!(
            facts(pool, bob.id).await,
            vec![(TYPE_UPDATE.to_owned(), alice.id, Some(posted.id))],
            "the second edit finds no booster and grows no row"
        );
    }

    #[tokio::test]
    async fn subscribers_are_told_of_every_visible_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "su-alice").await;
        let bob = local(pool, "su-bob").await;
        let carol = local(pool, "su-carol").await;
        subscribed(pool, &bob, &alice).await;
        // Carol follows without the notify toggle: a plain follower.
        let _quiet = followed(pool, &carol, &alice).await;

        for (label, visibility) in [
            ("public", Visibility::Public),
            ("unlisted", Visibility::Unlisted),
            ("private", Visibility::Private),
            ("local", Visibility::Local),
        ] {
            let row = post_vis(pool, &alice, &format!("{label} news"), visibility).await;
            on_subscribed_status(&state, &row).await;
        }
        let told: Vec<String> = facts(pool, bob.id)
            .await
            .into_iter()
            .map(|found| found.0)
            .collect();
        assert_eq!(
            told,
            vec![TYPE_STATUS.to_owned(); 4],
            "the subscriber is told of every visibility but direct, newest first"
        );
        assert!(
            rows(pool, carol.id).await.is_empty(),
            "the unsubscribed follower is told nothing"
        );

        // A `direct` post is never a subscriber's news.
        let dm = post_vis(pool, &alice, "for addressees only", Visibility::Direct).await;
        on_subscribed_status(&state, &dm).await;
        assert_eq!(
            facts(pool, bob.id).await.len(),
            4,
            "the direct post grew no fifth telling"
        );
    }

    #[tokio::test]
    async fn a_reblog_row_and_a_pending_request_tell_no_subscribers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "sr2-alice").await;
        let bob = local(pool, "sr2-bob").await;
        subscribed(pool, &bob, &alice).await;

        // Alice boosts a mirrored remote post: the reblog row is not a
        // subscriber's `status` news (the boost's own arm tells the
        // original's author instead).
        let mallory = remote(pool, "sr2-mallory", "remote.example").await;
        let theirs = post(pool, &mallory, "their post").await;
        let (boost, _) = status::create_reblog(pool, BASE, &alice, &theirs, Visibility::Public)
            .await
            .expect("the boost writes");
        on_subscribed_status(&state, &boost).await;
        assert!(
            rows(pool, bob.id).await.is_empty(),
            "a reblog row notifies no subscriptions"
        );

        // A pending request with its toggle on has subscribed nobody yet.
        let lara = locked(pool, "sr2-lara").await;
        let fred = local(pool, "sr2-fred").await;
        let request = followed(pool, &fred, &lara).await;
        assert_eq!(request.state, FollowState::Pending, "the fixture waits");
        let _toggled = follow::set_toggles(
            pool,
            fred.id,
            lara.id,
            follow::FollowToggles {
                show_reblogs: None,
                notify: Some(true),
            },
        )
        .await
        .expect("the toggles run");
        let news = post(pool, &lara, "locked news").await;
        on_subscribed_status(&state, &news).await;
        assert!(
            rows(pool, fred.id).await.is_empty(),
            "the waiting request carries no subscription yet"
        );
    }

    #[tokio::test]
    async fn a_reaction_tells_the_author_per_emoji_and_the_undo_retires_it() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "rc-alice").await;
        let bob = local(pool, "rc-bob").await;

        let posted = post(pool, &alice, "react to me").await;
        reacted(&state, pool, &bob, &posted, "👍").await;
        let notes = rows(pool, alice.id).await;
        assert_eq!(notes.len(), 1, "the reaction is told to the author");
        let note = notes.first().expect("the fixture telling");
        assert_eq!(note.event_type, TYPE_EMOJI_REACTION);
        assert_eq!(note.from_account_id, bob.id);
        assert_eq!(note.status_id, Some(posted.id));
        assert_eq!(
            note.emoji.as_deref(),
            Some("👍"),
            "the emoji rides in the row"
        );

        // The idempotent `PUT` re-fires the hook on the same fact: the
        // deduplication keeps it one row.
        let again = emoji_reaction::react(
            pool,
            BASE,
            &NewReaction {
                user_id: bob.id,
                status_id: posted.id,
                emoji: "👍",
                url: None,
            },
        )
        .await
        .expect("the repeat reaction restates the row");
        on_reaction_changed(&state, &again, true).await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            1,
            "a restated reaction tells once"
        );

        // A second emoji is a second fact.
        reacted(&state, pool, &bob, &posted, "🎉").await;
        assert_eq!(
            rows(pool, alice.id).await.len(),
            2,
            "two emojis on one status are two tellings"
        );

        // And the withdrawal takes exactly its own emoji back.
        let removed = emoji_reaction::unreact(pool, bob.id, posted.id, "👍")
            .await
            .expect("the withdrawal runs")
            .expect("the reaction was there");
        on_reaction_changed(&state, &removed, false).await;
        let left = rows(pool, alice.id).await;
        assert_eq!(left.len(), 1, "the other emoji's telling stands");
        assert_eq!(
            left.first().expect("the standing telling").emoji.as_deref(),
            Some("🎉"),
            "and it is the one that was not withdrawn"
        );
    }

    #[tokio::test]
    async fn a_self_or_remote_reaction_tells_nobody() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let bob = local(pool, "rs-bob").await;
        let mallory = remote(pool, "rs-mallory", "remote.example").await;

        let own = post(pool, &bob, "my own post").await;
        reacted(&state, pool, &bob, &own, "👍").await;
        assert!(
            rows(pool, bob.id).await.is_empty(),
            "reacting to your own post tells nobody"
        );

        let theirs = post(pool, &mallory, "their post").await;
        reacted(&state, pool, &bob, &theirs, "👍").await;
        assert!(
            rows(pool, mallory.id).await.is_empty(),
            "a remote author is not ours to tell"
        );

        // A withdrawal of a reaction that never existed leaves the hook
        // nothing to retire and no error to report.
        let untouched = emoji_reaction::unreact(pool, bob.id, own.id, "🎉")
            .await
            .expect("the withdrawal runs");
        assert!(untouched.is_none(), "there was no row to remove");
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
        let stand = poll_on(pool, &posted, 30).await;
        ballot(pool, &stand, &bob).await;
        on_poll_vote(&state, &stand, &bob).await;
        assert!(
            rows(pool, gated.id).await.is_empty(),
            "the stranger's ballot is withheld by the setting"
        );

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
}
