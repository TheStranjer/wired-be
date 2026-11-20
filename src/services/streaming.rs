//! The streaming target computation: which hub streams carry a status,
//! whose audience it is, and whether it is local-only.
//!
//! [`crate::streaming::hub`] delivers every [`StreamEvent`] by stream NAME
//! and gates it by [`Audience`], so a producer must first answer, from the
//! stored row and its author: which stream names the event belongs to
//! (`"public"`, `"user:<uuid>"`, `"list:<uuid>"`, `"hashtag:<tag>"`, …),
//! who may receive it, and whether it concerns local viewers only. This
//! module is that answer — the same question Pleroma's
//! `Pleroma.Activity.Ir.Topics` (`lib/pleroma/activity/ir/topics.ex`)
//! answers when it turns an activity into `PubSub` topics, restated over
//! decision 6's relational statuses and the five `statuses.visibility`
//! values. [`targets`] computes and reads; [`status_created`],
//! [`status_deleted`] and [`status_edited`] are its publish side: each
//! resolves the row's answer and hands the finished [`StreamEvent`] to
//! [`Hub::publish`](crate::streaming::hub::Hub::publish) from the action
//! hook that calls it, best-effort — a status already written never fails
//! its request over a push. A created or edited status publishes a
//! [`RenderSpec::Status`] hint rather than an entity, so every socket
//! renders the entity for its own reader (the streaming endpoint of
//! [`crate::api::streaming`] resolves the hint); a delete, whose row is a
//! tombstone nothing can re-render, carries the deleted id itself.
//! [`notification_created`] is the module's other producer: Mastodon's
//! `notification` for a freshly created notification row, riding the
//! recipient's own `user:{id}` and `user:notification:{id}` streams with a
//! [`RenderSpec::Notification`] hint — and never published when the
//! recipient mutes the actor, the same mute the rendered entity reports
//! as `pleroma.is_muted`. [`chat_updated`] is the third: Pleroma's
//! `pleroma:chat_update` for one participant's chat row, riding only their
//! `user:pleroma_chat:{id}` stream with a [`RenderSpec::Chat`] hint, fired
//! from the chat hooks of [`crate::api::chats`]. [`conversation_updated`] is
//! the fourth: Mastodon's `conversation` for one local participant's side of
//! a direct thread, riding only their `direct:{id}` stream with a
//! [`RenderSpec::Conversation`] hint, fired from the create hook's
//! [`crate::services::create_status`] conversation arm once a `direct` status
//! lands.
//!
//! The rules, stated once here and split into one helper per component:
//!
//! - **`public`**: the public timeline set — `public`, plus `public:local`
//!   for a local status or `public:remote` and `public:remote:{author
//!   domain}` for a remote one; with attachments the same shape again on
//!   `public:media`, narrowed to `public:local:media` /
//!   `public:remote:media`; and per stored tag `hashtag:{tag}`, plus
//!   `hashtag:local:{tag}` when the status is local (a remote post
//!   belongs in no local timeline — the same local/remote split as every
//!   `public:local*` name) — all of it with `Audience::Public`, plus the
//!   [home](#the-home-rule) and [list](#the-list-rule) streams, which is
//!   how a public post reaches the home and list timelines of the users
//!   who follow its author.
//! - **`unlisted`**: hashtag, [list](#the-list-rule) and
//!   [home](#the-home-rule) streams only — no `public*` timeline ever —
//!   gated by `Audience::Users` over the home ids plus the list owners:
//!   the post stays out of the public view, and the event's audience is
//!   the circle its streams name.
//! - **`private`**: the author's and every accepted local follower's
//!   `user:{id}` stream, `Audience::Users` over exactly those ids — the
//!   home rule without the mentioned users, since a mention grants nothing
//!   on a followers-only post (see [`crate::models::status_visibility`]).
//! - **`direct`**: the author's and each mentioned LOCAL user's `user:{id}`
//!   and `direct:{id}` streams, `Audience::Users` over those ids — never
//!   a public, hashtag or list stream.
//! - **`local`** (Pleroma's never-federated visibility): the local half of
//!   the public set — `public:local`, its `public:local:media` variant
//!   with attachments, and `hashtag:local:{tag}` per tag — with
//!   `Audience::Everyone` and `local_only = true`: every authenticated
//!   local user may see the post, and no socket without a reader may.
//!
//! ## The home rule
//!
//! A post reaches the home timelines of its author, of every follower this
//! instance owns whose follow is accepted and active, and of the local
//! users it @-mentions. For a reblog row the followers are narrowed to
//! those who kept [`show_reblogs`] on — a hidden boost must not land in
//! the hider's home stream (Mastodon's per-follow `reblogs` toggle).
//!
//! ## The list rule
//!
//! A post reaches every list whose owner follows the author and holds the
//! author as a member — [`list::lists_following_member`] is that join,
//! [`list::lists_containing`] its per-owner view. A root post every policy
//! carries; a reply only where the list's `replies_policy` admits the
//! account being replied to: `none` never carries replies, `followed`
//! carries a reply only when its owner follows or is the replied-to
//! account, `list` only when that account is the owner or a member of the
//! same list (Mastodon's `List#includes_reply?`).
//!
//! ## When a status is never published
//!
//! A deactivated author publishes nothing, and neither does a reblog row
//! whose original is deleted or missing — the boost of a gone post has no
//! body to render into an event. A tombstoned row *itself* still answers a
//! target set: the `delete` push runs off it, which is why the row's own
//! `deleted_at` is not one of the refusals.
//!
//! The module is a tree: `targets` stands here with the rules above, and
//! the parts are children. `status_events` holds the three status emitters
//! and the never-publish reblog test, with their shared push body in
//! `status_events/publish`; `other_events` the notification, chat,
//! conversation, filter and announcement emitters; `audience` the five
//! per-visibility answers, with the stream names and id rosters they read
//! in `audience/streams`; `lists` the list rule; `facts` the stored-row
//! lookups and the audience and name folds over their answers; and
//! `test_support` the fixtures the children's unit tests share. The
//! emitters are re-exported here, so `crate::services::streaming::*` is
//! their outside path.
//!
//! [`Audience`]: crate::streaming::hub::Audience
//! [`StreamEvent`]: crate::streaming::hub::StreamEvent
//! [`show_reblogs`]: crate::models::follow::Follow::show_reblogs
//! [`list::lists_containing`]: crate::models::list::lists_containing
//! [`list::lists_following_member`]: crate::models::list::lists_following_member
//! [`RenderSpec::Status`]: crate::streaming::hub::RenderSpec::Status
//! [`RenderSpec::Notification`]: crate::streaming::hub::RenderSpec::Notification
//! [`RenderSpec::Chat`]: crate::streaming::hub::RenderSpec::Chat
//! [`RenderSpec::Conversation`]: crate::streaming::hub::RenderSpec::Conversation

use crate::{
    error::AppResult,
    models::{
        status::{Status, Visibility},
        user::User,
    },
    state::AppState,
    streaming::hub::Audience,
};

use self::{
    audience::{direct_targets, local_targets, private_targets, public_targets, unlisted_targets},
    status_events::reblog_without_original,
};

mod audience;
mod facts;
mod lists;
mod other_events;
mod status_events;

#[cfg(test)]
mod test_support;

pub use self::{
    other_events::{
        announcement_published, chat_updated, conversation_updated, filters_changed,
        notification_created,
    },
    status_events::{status_created, status_deleted, status_edited},
};

/// The stream, audience and locality a stored status would publish with,
/// or `None` when it must never be published.
///
/// The full answer is the tuple `(streams, audience, local_only)`:
///
/// - `streams` — the hub stream names the event belongs to, sorted and
///   deduplicated (the [module docs](self) state the per-visibility set);
/// - `audience` — the [`Audience`] gating delivery at the receiving
///   sockets: `Public` for a public status, `Users` over every user whose
///   own stream carries the event for `unlisted` / `private` / `direct`,
///   `Everyone` for the `local` visibility;
/// - `local_only` — the [`StreamEvent::local_only`] flag: `true` only for
///   the never-federated `local` visibility, whose every viewer is a local
///   user.
///
/// `None` — never publish — is a deactivated author and a reblog row whose
/// original is deleted or missing (see the
/// [never-published cases](self#when-a-status-is-never-published)).
///
/// Pure computation over database reads: it publishes nothing and fires no
/// hook. The emitters ([`status_created`], [`status_deleted`],
/// [`status_edited`]) are its consumers, and the action hooks call those.
///
/// [`Audience`]: crate::streaming::hub::Audience
/// [`StreamEvent::local_only`]: crate::streaming::hub::StreamEvent::local_only
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<Option<(Vec<String>, Audience, bool)>> {
    if !author.is_active.get() {
        return Ok(None);
    }
    if reblog_without_original(&state.db, row).await? {
        return Ok(None);
    }
    let outcome = match row.visibility {
        Visibility::Public => public_targets(state, row, author).await?,
        Visibility::Unlisted => unlisted_targets(state, row, author).await?,
        Visibility::Private => private_targets(state, row, author).await?,
        Visibility::Direct => direct_targets(state, row, author).await?,
        Visibility::Local => local_targets(state, row).await?,
    };
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use crate::{
        models::status,
        services::streaming::test_support::{app_state, boost, inactive_user, local_user, post},
    };

    use super::*;

    #[tokio::test]
    async fn a_deactivated_author_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let ghost = inactive_user(&pool, "ghost").await;
        let row = post(&pool, &ghost, "heard from the grave").await;

        let answer = targets(&state, &row, &ghost)
            .await
            .expect("the computation reads");

        assert!(
            answer.is_none(),
            "a deactivated author never publishes, whatever the row says"
        );
    }

    #[tokio::test]
    async fn a_reblog_of_a_deleted_original_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dave = local_user(&pool, "dave").await;
        let original = post(&pool, &alice, "doomed").await;
        let row = boost(&pool, &dave, &original).await;
        let _tombstone = status::soft_delete(&pool, original.id)
            .await
            .expect("the delete fixture runs");

        let answer = targets(&state, &row, &dave)
            .await
            .expect("the computation reads");

        assert!(
            answer.is_none(),
            "a boost whose original is tombstoned has nothing left to announce — \
             the same live read answers a never-stored original"
        );
    }
}
