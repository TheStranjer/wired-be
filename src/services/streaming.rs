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
//! [`Audience`]: crate::streaming::hub::Audience
//! [`StreamEvent`]: crate::streaming::hub::StreamEvent
//! [`show_reblogs`]: crate::models::follow::Follow::show_reblogs
//! [`list::lists_containing`]: crate::models::list::lists_containing

use sqlx::PgPool;
use tracing::warn;
use uuid::Uuid;

use crate::{
    error::AppResult,
    models::{
        follow, hashtag, list, media_attachment, mention, mute,
        notification::Notification,
        status::{self, Status, Visibility},
        user::{self, User},
    },
    state::AppState,
    streaming::hub::{Audience, RenderSpec, StreamEvent},
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
/// [module docs](self#when-a-status-is-never-published)).
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

/// Publish Mastodon's `update` for the freshly stored status `row`.
///
/// The event rides the target set [`targets`] answers for the row, and it
/// carries a [`RenderSpec::Status`] hint rather than a payload: each
/// receiving socket renders the entity for its own reader (see
/// [`crate::api::streaming`], the reader that resolves the hint).
///
/// Best-effort like every action-hook tail: the row is already committed,
/// so a lost push costs a socket a redraw, not the client a failure.
/// Nothing publishes when [`targets`] answers `None` (the
/// [never-published cases](self#when-a-status-is-never-published)) or when
/// a read behind the computation fails, and every such refusal is a log
/// line. A remote row ingested through the same create path publishes from
/// here too — its `public:remote*` streams come from [`targets`].
pub async fn status_created(state: &AppState, row: &Status) {
    publish_status(state, row, "update", None, Some(RenderSpec::Status(row.id))).await;
}

/// Publish Mastodon's `delete` for the removed status.
///
/// `row` is the snapshot **as it was before the delete** (what
/// [`after_status_deleted`] is handed), so the same [`targets`] answer
/// names exactly the streams the `update` of this post rode: the delete
/// clears the row's own text but leaves its mention, hashtag and
/// attachment links, its visibility and its followers.
///
/// The payload is the deleted id as plain text (Mastodon's `delete`
/// shape) and no render hint rides along: the row is a tombstone, so no
/// socket can re-render an entity out of it. Best-effort and silent in
/// the same refusal cases as [`status_created`].
///
/// [`after_status_deleted`]: crate::services::delete_status
pub async fn status_deleted(state: &AppState, row: &Status) {
    publish_status(state, row, "delete", Some(row.id.to_string()), None).await;
}

/// Publish Mastodon's `status.update` for the edited status `row`, so the
/// sockets that carried the post redraw it.
///
/// `row` is the stored row as the edit route's gate read it, before the
/// write: an edit moves none of the fields [`targets`] reads off the row
/// itself — the author, the visibility, the thread, the locality and the
/// reblog link all stand (a reblog is not editable at that route at all)
/// — while the mentions, hashtags and attachments the computation looks up
/// come from the live tables the edit's transaction replaced. Either
/// version of the row therefore answers the same target set. The event
/// carries the same [`RenderSpec::Status`] hint as [`status_created`] and
/// no payload, and the same best-effort refusals apply.
pub async fn status_edited(state: &AppState, row: &Status) {
    publish_status(
        state,
        row,
        "status.update",
        None,
        Some(RenderSpec::Status(row.id)),
    )
    .await;
}

/// The shared body of the three emitters: read `row`'s author, ask
/// [`targets`], and hand the caller-shaped [`StreamEvent`] to
/// [`Hub::publish`](crate::streaming::hub::Hub::publish). Every refusal
/// is a return without publishing: a missing or unreadable author, a
/// failed target read (a log line each, since the write behind the event
/// already committed) and the target rules' own `None` (no log — the
/// [rules](self#when-a-status-is-never-published) already say why).
async fn publish_status(
    state: &AppState,
    row: &Status,
    event: &str,
    payload: Option<String>,
    render: Option<RenderSpec>,
) {
    let author = match user::get_by_id(&state.db, row.user_id).await {
        Ok(Some(author)) => author,
        Ok(None) => {
            warn!(
                status = %row.id,
                user = %row.user_id,
                "status stream event skipped: the row has no author to gate it by"
            );
            return;
        }
        Err(err) => {
            warn!(
                status = %row.id,
                %err,
                "status stream event skipped: reading its author failed"
            );
            return;
        }
    };
    let found = match targets(state, row, &author).await {
        Ok(found) => found,
        Err(err) => {
            warn!(
                status = %row.id,
                %err,
                "status stream event skipped: the target computation failed"
            );
            return;
        }
    };
    let Some((streams, audience, local_only)) = found else {
        return;
    };
    state.hub.publish(StreamEvent {
        streams,
        event: event.to_owned(),
        payload,
        audience,
        render,
        local_only,
    });
}

/// Publish Mastodon's `notification` for the freshly created notification `row`.
///
/// The event rides the recipient's two private streams — `user:{recipient}`
/// and `user:notification:{recipient}`, the hub names a client's `user` and
/// `user:notification` subscriptions expand to (see
/// [`crate::api::streaming`]) — with the recipient as the whole
/// [`Audience::Users`] and a [`RenderSpec::Notification`] hint and no
/// payload, so each socket renders the entity for its own reader as the
/// status emitters do.
///
/// A notification whose recipient mutes its actor publishes nothing: that
/// mute is exactly what the rendered entity reports as `pleroma.is_muted`
/// (the relationship's `muting` flag, [`mute::is_muting`] being its
/// one-pair spelling), and a row hidden from the recipient's listing is
/// hidden from their socket too.
///
/// Best-effort like [`status_created`]: the row is already committed, so a
/// failed mute read is a log line and nothing published, never an error
/// back to the caller.
pub async fn notification_created(state: &AppState, row: &Notification) {
    let recipient = row.user_id;
    match mute::is_muting(&state.db, recipient, row.from_account_id).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            warn!(
                notification = %row.id,
                %err,
                "notification stream event skipped: reading the recipient's mute failed"
            );
            return;
        }
    }
    state.hub.publish(StreamEvent {
        streams: vec![
            format!("user:{recipient}"),
            format!("user:notification:{recipient}"),
        ],
        event: "notification".to_owned(),
        payload: None,
        audience: Audience::Users(vec![recipient]),
        render: Some(RenderSpec::Notification(row.id)),
        local_only: false,
    });
}

/// Publish Pleroma's `pleroma:chat_update` for `owner_id`'s chat `chat_id`.
///
/// The event rides the owner's one chat stream — `user:pleroma_chat:{owner}`,
/// the hub name a client's `user:pleroma_chat` subscription expands to (see
/// [`crate::api::streaming`]) — with the owner as the whole
/// [`Audience::Users`] and a [`RenderSpec::Chat`] hint rather than a payload,
/// so each socket renders the entity for its own reader as the status and
/// notification emitters do. The hint must name the reader's OWN chat row:
/// every participant of a conversation owns their own row ([`chat::get_or_create`]
/// and [`chat::get_owned`]) and one participant's id means nothing to
/// another, so each side of a chat is published to separately, with its own
/// id. The chat hooks of [`crate::api::chats`] are the callers: a created or
/// deleted message tells both LOCAL sides, a read marker only the reader.
///
/// Best-effort like the other emitters — the rows behind the change are
/// already committed — and since nothing here reads the database, this call
/// cannot fail and never refuses. It is synchronous, like
/// [`Hub::publish`](crate::streaming::hub::Hub::publish) itself: handing
/// the event to the hub is the whole emission, and the cross-instance
/// bridge is the hub's spawned task.
///
/// [`chat::get_or_create`]: crate::models::chat::get_or_create
/// [`chat::get_owned`]: crate::models::chat::get_owned
pub fn chat_updated(state: &AppState, owner_id: Uuid, chat_id: Uuid) {
    state.hub.publish(StreamEvent {
        streams: vec![format!("user:pleroma_chat:{owner_id}")],
        event: "pleroma:chat_update".to_owned(),
        payload: None,
        audience: Audience::Users(vec![owner_id]),
        render: Some(RenderSpec::Chat(chat_id)),
        local_only: false,
    });
}

/// Publish Mastodon's `conversation` for `user_id`'s side of a direct thread.
///
/// The event rides the reader's one direct stream — `direct:{user_id}`, the
/// hub name a client's `direct` subscription expands to (see
/// [`crate::api::streaming`]) — with the reader as the whole
/// [`Audience::Users`] and a [`RenderSpec::Conversation`] hint rather than a
/// payload, so each socket renders the conversation entity for its own reader
/// as the status, notification and chat emitters do. The hint must name the
/// reader's OWN participation row: every local side of a thread owns its own
/// row ([`conversation::participate`]) and one side's id means nothing to
/// another, so each participant of a DM is published to separately, with their
/// own id. [`conversation::participations_for_status`] is the roster that
/// read answers, and [`create_status`]'s `participate_in_conversation` the
/// caller: a fresh `direct` status tells every LOCAL participant of its thread,
/// each with their own participation id — a remote account holds no
/// participation row and so hears nothing.
///
/// Best-effort like the other emitters — the participation rows are already
/// committed — and since nothing here reads the database, this call cannot
/// fail and never refuses. It is synchronous, like [`chat_updated`] and
/// [`Hub::publish`](crate::streaming::hub::Hub::publish) itself: handing the
/// event to the hub is the whole emission, and the cross-instance bridge is
/// the hub's spawned task.
///
/// [`conversation::participate`]: crate::models::conversation::participate
/// [`conversation::participations_for_status`]: crate::models::conversation::participations_for_status
/// [`create_status`]: crate::services::create_status
pub fn conversation_updated(state: &AppState, user_id: Uuid, participation_id: Uuid) {
    state.hub.publish(StreamEvent {
        streams: vec![format!("direct:{user_id}")],
        event: "conversation".to_owned(),
        payload: None,
        audience: Audience::Users(vec![user_id]),
        render: Some(RenderSpec::Conversation(participation_id)),
        local_only: false,
    });
}

/// Publish Mastodon's `filters_changed` after `user_id`'s filter write.
///
/// The event rides the owner's one user stream — `user:{user_id}`, the hub
/// name a client's `user` subscription expands to (see
/// [`crate::api::streaming`]) — with the owner as the whole
/// [`Audience::Users`]. It carries neither payload nor render hint: Mastodon
/// sends `filters_changed` bare, and a client's whole reaction to it is to
/// re-read its filters, so there is no entity to ship or to render. The
/// socket path forwards such a bare event as it stands — the envelope
/// simply has no `payload` key.
///
/// [`crate::api::filters`]' three write handlers are the callers: a created,
/// replaced or deleted filter tells its owner, and only its owner.
/// Best-effort like the other emitters — the filter row is already committed
/// — and since nothing here reads the database, this call cannot fail and
/// never refuses. It is synchronous, like [`chat_updated`] and
/// [`Hub::publish`](crate::streaming::hub::Hub::publish) itself: handing the
/// event to the hub is the whole emission, and the cross-instance bridge is
/// the hub's spawned task.
pub fn filters_changed(state: &AppState, user_id: Uuid) {
    state.hub.publish(StreamEvent {
        streams: vec![format!("user:{user_id}")],
        event: "filters_changed".to_owned(),
        payload: None,
        audience: Audience::Users(vec![user_id]),
        render: None,
        local_only: false,
    });
}

/// Publish Mastodon's `announcement` for the entity `announcement` to every
/// active LOCAL user.
///
/// There is no shared hub name an announcement could ride: a client's bare
/// `user` subscription only ever matches `user:{reader uuid}` at the socket
/// (see [`crate::api::streaming`]), so a bare `user` stream would reach
/// nobody. This therefore reads the roster of active local ids once
/// ([`user::active_local_ids`]) and publishes a single [`StreamEvent`] over
/// all of their `user:{id}` streams with [`Audience::Users`] set to exactly
/// those ids — remote mirrors and deactivated accounts stand outside both
/// gates by construction. The payload is the announcement entity as its JSON
/// text: Mastodon ships the rendered entity here, not a render hint, so the
/// socket writes the frame unchanged.
///
/// Nothing calls this yet: the admin announcements ticket (WBE-204) will
/// publish through here when its routes make and pull announcements.
///
/// Best-effort like [`status_created`]: a failed roster read is a log line
/// and nothing published. With no active local users there is nobody to name
/// in either gate, so the roster's emptiness is the other silence.
pub async fn announcement_published(state: &AppState, announcement: &serde_json::Value) {
    let ids = match user::active_local_ids(&state.db).await {
        Ok(ids) => ids,
        Err(err) => {
            warn!(
                %err,
                "announcement stream event skipped: reading the active local users failed"
            );
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    state.hub.publish(StreamEvent {
        streams: ids.iter().map(|id| format!("user:{id}")).collect(),
        event: "announcement".to_owned(),
        payload: Some(announcement.to_string()),
        audience: Audience::Users(ids),
        render: None,
        local_only: false,
    });
}

/// Whether `row` is a reblog whose original is gone: [`status::get_by_id`]
/// hides tombstones, so its `None` is both the deleted and the never-there
/// answer, and each of them leaves a boost with nothing to announce.
async fn reblog_without_original(pool: &PgPool, row: &Status) -> AppResult<bool> {
    let Some(original_id) = row.reblog_of_id else {
        return Ok(false);
    };
    Ok(status::get_by_id(pool, original_id).await?.is_none())
}

/// The `public` visibility's answer: the public timeline set, the hashtag
/// streams, and the home and list deliveries, gated by nobody.
async fn public_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let has_media = has_attachments(&state.db, row.id).await?;
    let mut streams = public_timeline_streams(row, author, has_media);
    let tags = status_tag_names(&state.db, row.id).await?;
    streams.extend(hashtag_streams(&tags, row.local));
    let home = home_ids(&state.db, row, author).await?;
    streams.extend(user_streams(&home));
    let (list_streams, _owners_gated_by_public_anyway) =
        list_targets(&state.db, row, author).await?;
    streams.extend(list_streams);
    Ok((sorted_names(streams), Audience::Public, false))
}

/// The `unlisted` visibility's answer: hashtag, list and home streams —
/// no `public*` timeline ever — gated by `Audience::Users` over the home
/// circle plus the list owners, the event's only viewers besides itself.
async fn unlisted_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let tags = status_tag_names(&state.db, row.id).await?;
    let mut streams = hashtag_streams(&tags, row.local);
    let home = home_ids(&state.db, row, author).await?;
    streams.extend(user_streams(&home));
    let (list_streams, owners) = list_targets(&state.db, row, author).await?;
    streams.extend(list_streams);
    Ok((sorted_names(streams), users_audience(home, &owners), false))
}

/// The `private` visibility's answer: the author's and the accepted local
/// followers' `user:{id}` streams — the home rule without the mentioned
/// users, because a mention grants no visibility on a followers-only post.
async fn private_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut ids = vec![author.id];
    ids.extend(followers_of(&state.db, row, author).await?);
    ids.sort_unstable();
    ids.dedup();
    let streams = user_streams(&ids);
    Ok((streams, Audience::Users(ids), false))
}

/// The `direct` visibility's answer: the author and every mentioned LOCAL
/// user, each on their own `user:{id}` and `direct:{id}` streams, and no
/// public, hashtag or list stream anywhere.
async fn direct_targets(
    state: &AppState,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut recipients = vec![author.id];
    recipients.extend(mentioned_local_ids(&state.db, row.id).await?);
    recipients.sort_unstable();
    recipients.dedup();
    let mut streams = Vec::with_capacity(recipients.len() * 2);
    for id in &recipients {
        streams.push(format!("direct:{id}"));
        streams.push(format!("user:{id}"));
    }
    Ok((sorted_names(streams), Audience::Users(recipients), false))
}

/// The `local` visibility's answer: the local half of the public set —
/// `public:local`, its media variant, and the local hashtag streams —
/// with nobody specific to gate: every authenticated local user may see
/// the post, so the audience is `Everyone` and `local_only` is set, which
/// is what drops it on every socket without a reader.
async fn local_targets(state: &AppState, row: &Status) -> AppResult<(Vec<String>, Audience, bool)> {
    let mut streams = vec!["public:local".to_owned()];
    if has_attachments(&state.db, row.id).await? {
        streams.push("public:local:media".to_owned());
    }
    let tags = status_tag_names(&state.db, row.id).await?;
    streams.extend(local_hashtag_streams(&tags));
    Ok((sorted_names(streams), Audience::Everyone, true))
}

/// The public timeline stream names of `row`: the global `public` stream
/// and its local or remote variant, and — once the status carries
/// attachments — the same pair again on the `public:media` timelines.
fn public_timeline_streams(row: &Status, author: &User, has_media: bool) -> Vec<String> {
    let mut streams = vec!["public".to_owned()];
    if row.local {
        streams.push("public:local".to_owned());
    } else {
        streams.push("public:remote".to_owned());
        if let Some(domain) = &author.domain {
            streams.push(format!("public:remote:{domain}"));
        }
    }
    if has_media {
        streams.push("public:media".to_owned());
        if row.local {
            streams.push("public:local:media".to_owned());
        } else {
            streams.push("public:remote:media".to_owned());
        }
    }
    streams
}

/// The hashtag streams of a federated visibility (`public`, `unlisted`):
/// every tag's global `hashtag:{tag}` timeline, and — for a local status,
/// which is all this instance's tag timelines ever carry — its
/// `hashtag:local:{tag}` beside it.
fn hashtag_streams(tags: &[String], local_status: bool) -> Vec<String> {
    let mut streams = Vec::new();
    for tag in tags {
        streams.push(format!("hashtag:{tag}"));
        if local_status {
            streams.push(format!("hashtag:local:{tag}"));
        }
    }
    streams
}

/// The `local` visibility's hashtag streams: each tag's
/// `hashtag:local:{tag}` and nothing global — the post never leaves this
/// instance, so it belongs in no global tag timeline.
fn local_hashtag_streams(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| format!("hashtag:local:{tag}"))
        .collect()
}

/// The `user:{id}` stream names of `ids`, in the order given.
fn user_streams(ids: &[Uuid]) -> Vec<String> {
    ids.iter().map(|id| format!("user:{id}")).collect()
}

/// The home rule's ids: the author, the followers whose home timeline
/// still sees this post (the [boost-narrowed](self#the-home-rule) set for
/// a reblog row), and the local users it mentions — sorted and dedup,
/// since one account is often both follower and mention.
async fn home_ids(pool: &PgPool, row: &Status, author: &User) -> AppResult<Vec<Uuid>> {
    let mut ids = vec![author.id];
    ids.extend(followers_of(pool, row, author).await?);
    ids.extend(mentioned_local_ids(pool, row.id).await?);
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The follower half of the home and private rules: the ids of the local,
/// active, accepted follows of the author, narrowed to the ones still
/// seeing boosts when `row` is itself a reblog.
async fn followers_of(pool: &PgPool, row: &Status, author: &User) -> AppResult<Vec<Uuid>> {
    follow::accepted_local_follower_ids(pool, author.id, row.reblog_of_id.is_some()).await
}

/// The `status_mentions` accounts of `row` that this instance owns: the
/// local half of a direct audience and of the home rule's mention side (a
/// remote mention holds no socket here).
async fn mentioned_local_ids(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<Uuid>> {
    let mut grouped = mention::for_statuses(pool, &[status_id]).await?;
    let some_users = grouped.remove(&status_id).unwrap_or_default();
    Ok(some_users
        .into_iter()
        .filter(|user| user.local.get())
        .map(|user| user.id)
        .collect())
}

/// The list rule's answer: the `list:{uuid}` stream of every list whose
/// owner follows the author and holds them as a member, and the owners'
/// ids to fold into the audience — each one a reply already checked
/// against its list's [`replies_policy`](list::List::replies_policy).
async fn list_targets(
    pool: &PgPool,
    row: &Status,
    author: &User,
) -> AppResult<(Vec<String>, Vec<Uuid>)> {
    let mut streams = Vec::new();
    let mut owners = Vec::new();
    for list_row in list::lists_following_member(pool, author.id).await? {
        if reply_admitted(pool, row, &list_row).await? {
            streams.push(format!("list:{}", list_row.id));
            owners.push(list_row.user_id);
        }
    }
    Ok((streams, owners))
}

/// Whether `list_row` carries `row`: every list carries a root status, a
/// reply only where its policy admits the account being replied to.
async fn reply_admitted(pool: &PgPool, row: &Status, list_row: &list::List) -> AppResult<bool> {
    if row.in_reply_to_id.is_none() {
        return Ok(true);
    }
    match list_row.replies_policy.as_str() {
        "list" => replied_to_in_list(pool, row, list_row).await,
        "followed" => replied_to_is_followed(pool, row, list_row).await,
        // `"none"` carries no replies at all; anything else the column's
        // check constraint does not allow admits nothing either.
        _ => Ok(false),
    }
}

/// The `list` policy's answer: the replied-to account is the owner's own,
/// or a member of this same list. A reply to an account this instance does
/// not know the author of is neither.
async fn replied_to_in_list(pool: &PgPool, row: &Status, list_row: &list::List) -> AppResult<bool> {
    let Some(replied_to) = row.in_reply_to_account_id else {
        return Ok(false);
    };
    if replied_to == list_row.user_id {
        return Ok(true);
    }
    Ok(list::member_ids(pool, list_row.id)
        .await?
        .contains(&replied_to))
}

/// The `followed` policy's answer: the replied-to account is the owner's
/// own, or one the owner holds an accepted follow of. A reply to an
/// account this instance does not know the author of is neither.
async fn replied_to_is_followed(
    pool: &PgPool,
    row: &Status,
    list_row: &list::List,
) -> AppResult<bool> {
    let Some(replied_to) = row.in_reply_to_account_id else {
        return Ok(false);
    };
    if replied_to == list_row.user_id {
        return Ok(true);
    }
    follow::has_accepted_follow(pool, list_row.user_id, replied_to).await
}

/// The stored tag names of `row`, name-ordered as
/// [`hashtag::for_statuses`] groups them, empty when the status has none.
async fn status_tag_names(pool: &PgPool, status_id: Uuid) -> AppResult<Vec<String>> {
    let mut grouped = hashtag::for_statuses(pool, &[status_id]).await?;
    let some_tags = grouped.remove(&status_id).unwrap_or_default();
    Ok(some_tags.into_iter().map(|tag| tag.name).collect())
}

/// Whether `row` carries at least one media attachment — the gate on the
/// `public*:media` streams.
async fn has_attachments(pool: &PgPool, status_id: Uuid) -> AppResult<bool> {
    let grouped = media_attachment::for_statuses(pool, &[status_id]).await?;
    Ok(grouped.contains_key(&status_id))
}

/// The `Audience::Users` over `ids` extended with the list `owners` — the
/// list rule's "with the owner in `Audience::Users`" — sorted and dedup,
/// so the audience names each user once.
fn users_audience(mut ids: Vec<Uuid>, owners: &[Uuid]) -> Audience {
    ids.extend(owners.iter().copied());
    ids.sort_unstable();
    ids.dedup();
    Audience::Users(ids)
}

/// `names` sorted and deduplicated: the canonical order the computed
/// stream list is handed over in, so a later publisher (and every test)
/// compares stream sets by comparing vectors.
fn sorted_names(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;
    use tokio::sync::broadcast;

    use super::*;
    use crate::{
        config::{Config, TEST_BASE_URL},
        http_client::HttpClient,
        models::{
            chat,
            follow::FollowToggles,
            list::NewList,
            media_attachment::NewMediaAttachment,
            notification::{self, NewNotification, TYPE_FAVOURITE},
            status::NewStatus,
            user::{Flag, NewUser, insert, local_ap_id},
        },
        streaming::hub::Hub,
        uploads::UploaderBackend,
        util::id::new_id,
    };

    /// Test state over `pool` with the fixed test config.
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
            mrf_config_cache: Arc::default(),
            rate_limiter: Arc::new(crate::rate_limit::RateLimiter::from_config(config.as_ref())),
            mailer: Arc::new(crate::mailer::MemoryTransport::new()),
        }
    }

    /// Inserts a local user named `nickname`.
    async fn local_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname)),
        )
        .await
        .expect("the local fixture inserts")
    }

    /// Inserts a local user named `nickname` whose account is deactivated.
    async fn inactive_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                is_active: Flag::new(false),
                ..NewUser::local(nickname, &local_ap_id(TEST_BASE_URL, nickname))
            },
        )
        .await
        .expect("the deactivated fixture inserts")
    }

    /// Inserts a mirror of `nickname` on `remote.example`.
    async fn remote_user(pool: &PgPool, nickname: &str) -> User {
        insert(
            pool,
            &NewUser {
                nickname: format!("{nickname}@remote.example"),
                ap_id: format!("https://remote.example/users/{nickname}"),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts")
    }

    /// A public local post by `author` of `text`.
    async fn post(pool: &PgPool, author: &User, text: &str) -> Status {
        status::insert(pool, TEST_BASE_URL, &NewStatus::local(author, text))
            .await
            .expect("the status fixture inserts")
    }

    /// A local post by `author` of `text` at `visibility`, @-mentioning
    /// every user in `mentions`.
    async fn post_vis(
        pool: &PgPool,
        author: &User,
        text: &str,
        visibility: Visibility,
        mentions: &[Uuid],
    ) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                visibility,
                mentioned_user_ids: mentions.to_vec(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the status fixture inserts")
    }

    /// A public remote status by `author` of `text`, tagged with `tags`.
    async fn remote_post(pool: &PgPool, author: &User, text: &str, tags: &[&str]) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                user_id: author.id,
                content: text.to_owned(),
                raw_text: text.to_owned(),
                hashtags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                uri: Some(format!("https://remote.example/posts/{}", new_id())),
                ..NewStatus::default()
            },
        )
        .await
        .expect("the remote status fixture inserts")
    }

    /// A public local post by `author` answering `parent`, recording its
    /// author as the replied-to account the way the create path does.
    async fn reply_to(pool: &PgPool, author: &User, parent: &Status) -> Status {
        status::insert(
            pool,
            TEST_BASE_URL,
            &NewStatus {
                in_reply_to_id: Some(parent.id),
                in_reply_to_account_id: Some(parent.user_id),
                ..NewStatus::local(author, "in reply")
            },
        )
        .await
        .expect("the reply fixture inserts")
    }

    /// `follower` follows `target`, the fixture asserting the unlocked
    /// local target granted it on the spot.
    async fn follow_user(pool: &PgPool, follower: &User, target: &User) {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture writes");
        assert_eq!(
            row.state,
            follow::FollowState::Accepted,
            "the unlocked local target grants it"
        );
    }

    /// Turns `follower`'s follow of `target` into a boosts-hidden one.
    async fn hide_boosts(pool: &PgPool, follower: &User, target: &User) {
        let toggled = follow::set_toggles(
            pool,
            follower.id,
            target.id,
            FollowToggles {
                show_reblogs: Some(false),
                notify: None,
            },
        )
        .await
        .expect("the toggle fixture writes")
        .expect("the relationship exists");
        assert!(!toggled.show_reblogs, "the stored row hides boosts");
    }

    /// A list of `owner` holding `member`, titled for its reply policy.
    async fn list_of(pool: &PgPool, owner: &User, member: &User, policy: &str) -> list::List {
        let created = list::create(
            pool,
            owner.id,
            &NewList {
                title: format!("{policy} list"),
                replies_policy: policy.to_owned(),
                ..NewList::default()
            },
        )
        .await
        .expect("the list fixture creates");
        let added = list::add_members(pool, created.id, &[member.id])
            .await
            .expect("the member fixture adds");
        assert_eq!(added, 1, "the membership is new");
        created
    }

    /// The three reply policies, owned by `dana` and holding `alice`.
    async fn all_three_lists(pool: &PgPool, dana: &User, alice: &User) -> Vec<list::List> {
        let mut made = Vec::new();
        for policy in ["none", "list", "followed"] {
            made.push(list_of(pool, dana, alice, policy).await);
        }
        made
    }

    /// The id of the entry of `lists` carrying this reply policy.
    fn list_id_of(lists: &[list::List], policy: &str) -> Uuid {
        lists
            .iter()
            .find(|list_row| list_row.replies_policy == policy)
            .expect("the fixture made all three policies")
            .id
    }

    /// Attaches one image to `row`.
    async fn attach_media(pool: &PgPool, author: &User, row: &Status) {
        let _fixture = media_attachment::insert(
            pool,
            &NewMediaAttachment {
                user_id: Some(author.id),
                status_id: Some(row.id),
                url: "https://cdn.example/pic.png".to_owned(),
                content_type: "image/png".to_owned(),
                ..NewMediaAttachment::default()
            },
        )
        .await
        .expect("the media fixture attaches");
    }

    /// `reblogger` boosts `original` as a public status.
    async fn boost(pool: &PgPool, reblogger: &User, original: &Status) -> Status {
        let (row, created) =
            status::create_reblog(pool, TEST_BASE_URL, reblogger, original, Visibility::Public)
                .await
                .expect("the reblog fixture writes");
        assert!(created, "the boost row is new");
        row
    }

    /// [`targets`]' unwrapped answer for a status the test expects to
    /// publish.
    async fn published(
        state: &AppState,
        row: &Status,
        author: &User,
    ) -> (Vec<String>, Audience, bool) {
        targets(state, row, author)
            .await
            .expect("the computation reads")
            .expect("the fixture publishes")
    }

    /// The `user:<id>` hub name.
    fn user_stream(id: Uuid) -> String {
        format!("user:{id}")
    }

    /// The `direct:<id>` hub name.
    fn direct_stream(id: Uuid) -> String {
        format!("direct:{id}")
    }

    /// The `list:<id>` hub name.
    fn list_stream(id: Uuid) -> String {
        format!("list:{id}")
    }

    /// An expected stream set, spelled in the canonical order [`targets`]
    /// answers in.
    fn canonical(mut names: Vec<String>) -> Vec<String> {
        names.sort();
        names
    }

    /// The ids of a set, sorted, for order-insensitive comparison.
    fn sorted_ids(mut ids: Vec<Uuid>) -> Vec<Uuid> {
        ids.sort_unstable();
        ids
    }

    /// The `Audience::Users` ids of a computed audience, which the test
    /// asserts are users-gated.
    fn users_ids(audience: &Audience) -> Vec<Uuid> {
        match audience {
            Audience::Users(ids) => ids.clone(),
            Audience::Public | Audience::Everyone => {
                panic!("a users-gated event was the audience {audience:?}")
            }
        }
    }

    /// The single event the emitter under test handed to `rx`, which the
    /// test subscribed to before making the call: the hub delivers every
    /// publish to its local subscribers at once, so one awaited emitter
    /// call is already buffered here by the time this reads.
    async fn emitted(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) -> StreamEvent {
        Arc::unwrap_or_clone(rx.recv().await.expect("the emitter published an event"))
    }

    /// Asserts `rx` holds no event: the emitter published nothing.
    fn assert_empty(rx: &mut broadcast::Receiver<Arc<StreamEvent>>) {
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "the emitter published an event it owed nothing"
        );
    }

    #[tokio::test]
    async fn public_local_post_streams_the_public_timeline_and_home() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let erin = local_user(&pool, "erin").await;
        let row = post(&pool, &alice, "hello world").await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
            ]),
            "the public timeline plus the author's and the follower's home streams"
        );
        assert!(
            !streams.contains(&user_stream(erin.id)),
            "and not an unrelated account's home"
        );
        assert_eq!(audience, Audience::Public, "a public post gates nobody");
        assert!(!local_only, "a public post is not a local-only event");
    }

    #[tokio::test]
    async fn public_remote_post_streams_the_remote_timelines_alone() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let carol = remote_user(&pool, "carol").await;

        let tagged = remote_post(&pool, &carol, "over there", &["Rust"]).await;
        let (streams, audience, local_only) = published(&state, &tagged, &carol).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:remote".to_owned(),
                "public:remote:remote.example".to_owned(),
                "hashtag:rust".to_owned(),
                user_stream(carol.id),
            ]),
            "the remote variants stand in for the local ones, and a remote post rides \
             the global tag timeline but no local one"
        );
        assert_eq!(audience, Audience::Public, "a public post gates nobody");
        assert!(!local_only, "a remote post is not local-only either");

        let with_media = remote_post(&pool, &carol, "over there again", &[]).await;
        attach_media(&pool, &carol, &with_media).await;
        let (media_streams, _, _) = published(&state, &with_media, &carol).await;
        assert_eq!(
            media_streams,
            canonical(vec![
                "public".to_owned(),
                "public:remote".to_owned(),
                "public:remote:remote.example".to_owned(),
                "public:media".to_owned(),
                "public:remote:media".to_owned(),
                user_stream(carol.id),
            ]),
            "attachments ride the media timelines, remote-narrowed"
        );
    }

    #[tokio::test]
    async fn public_tagged_and_media_posts_add_their_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;

        let tagged = post(&pool, &alice, "hello #Rust world").await;
        let (tag_streams, _, _) = published(&state, &tagged, &alice).await;
        assert_eq!(
            tag_streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
            ]),
            "a local tagged post rides both hashtag timelines, the stored name lowercased"
        );

        let media = post(&pool, &alice, "look at this").await;
        attach_media(&pool, &alice, &media).await;
        let (media_streams, _, _) = published(&state, &media, &alice).await;
        assert_eq!(
            media_streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                "public:media".to_owned(),
                "public:local:media".to_owned(),
                user_stream(alice.id),
            ]),
            "a local media post rides both media timelines"
        );
    }

    #[tokio::test]
    async fn unlisted_post_streams_hashtags_lists_and_home_but_no_public_timeline() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let alice_list = list_of(&pool, &dana, &alice, "followed").await;
        let erin = local_user(&pool, "erin").await;

        let row = post_vis(
            &pool,
            &alice,
            "quietly #Rust",
            Visibility::Unlisted,
            &[erin.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
                user_stream(dana.id),
                user_stream(erin.id),
                list_stream(alice_list.id),
            ]),
            "hashtag, home (author, followers, the mentioned non-follower) and list — \
             and no public timeline stream at all"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id, erin.id, dana.id]),
            "the audience is the home circle plus the list owner"
        );
        assert!(!local_only, "unlisted is still a federated post");
    }

    #[tokio::test]
    async fn private_post_streams_the_author_and_accepted_local_followers_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let carol = remote_user(&pool, "carol").await;
        follow_user(&pool, &carol, &alice).await;
        let dave = inactive_user(&pool, "dave").await;
        follow_user(&pool, &dave, &alice).await;
        let erin = local_user(&pool, "erin").await;

        let row = post_vis(
            &pool,
            &alice,
            "followers only",
            Visibility::Private,
            &[erin.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![user_stream(alice.id), user_stream(bob.id)]),
            "the author and the one local active accepted follower — not the remote \
             mirror, not the deactivated account, not the mentioned non-follower"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id]),
            "and the audience is exactly those two"
        );
        assert!(!local_only, "private is still a federated post");
    }

    #[tokio::test]
    async fn direct_post_streams_recipients_user_and_direct_names() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = remote_user(&pool, "carol").await;

        let row = post_vis(
            &pool,
            &alice,
            "for a few eyes only",
            Visibility::Direct,
            &[bob.id, carol.id],
        )
        .await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                user_stream(alice.id),
                direct_stream(alice.id),
                user_stream(bob.id),
                direct_stream(bob.id),
            ]),
            "author and every mentioned LOCAL user on both their streams — the remote \
             mention gets nothing, and no public or hashtag stream ever does"
        );
        assert_eq!(
            users_ids(&audience),
            sorted_ids(vec![alice.id, bob.id]),
            "the audience is exactly the recipients"
        );
        assert!(!local_only, "a direct post is not the local visibility");
    }

    #[tokio::test]
    async fn local_visibility_post_is_everyone_local_only_on_the_local_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;

        let row = post_vis(&pool, &alice, "instance only #Rust", Visibility::Local, &[]).await;
        attach_media(&pool, &alice, &row).await;

        let (streams, audience, local_only) = published(&state, &row, &alice).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public:local".to_owned(),
                "public:local:media".to_owned(),
                "hashtag:local:rust".to_owned(),
            ]),
            "the local half of the public set — no global streams, no home or list delivery"
        );
        assert_eq!(
            audience,
            Audience::Everyone,
            "every authenticated local user may see it"
        );
        assert!(
            local_only,
            "and no socket without a reader may see it at all"
        );
    }

    #[tokio::test]
    async fn a_reblog_skips_the_followers_who_hidden_boosts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dave = local_user(&pool, "dave").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &dave).await;
        hide_boosts(&pool, &bob, &dave).await;
        let carol = local_user(&pool, "carol").await;
        follow_user(&pool, &carol, &dave).await;

        let own_post = post(&pool, &dave, "a plain post").await;
        let (own_streams, _, _) = published(&state, &own_post, &dave).await;
        assert!(
            own_streams.contains(&user_stream(bob.id)),
            "dave's own post reaches the follower who hides boosts"
        );

        let original = post(&pool, &alice, "worth boosting").await;
        let row = boost(&pool, &dave, &original).await;

        let (streams, audience, local_only) = published(&state, &row, &dave).await;

        assert_eq!(
            streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                user_stream(dave.id),
                user_stream(carol.id),
            ]),
            "the boost lands everywhere dave's own posts do, minus the home of the \
             follower who turned reblogs off"
        );
        assert_eq!(
            audience,
            Audience::Public,
            "a boost of a public post keeps the public audience"
        );
        assert!(!local_only, "a boost of a public post is not local-only");
    }

    #[tokio::test]
    async fn lists_carry_root_statuses_under_every_policy() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let lists = all_three_lists(&pool, &dana, &alice).await;

        let row = post(&pool, &alice, "a root post").await;

        let (streams, audience, _) = published(&state, &row, &alice).await;
        for list_row in &lists {
            assert!(
                streams.contains(&list_stream(list_row.id)),
                "the {} policy carries a root post",
                list_row.replies_policy
            );
        }
        assert_eq!(
            audience,
            Audience::Public,
            "the lists ride along with the public audience"
        );
    }

    #[tokio::test]
    async fn list_reply_policies_admit_by_the_replied_to_account() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let lists = all_three_lists(&pool, &dana, &alice).await;
        let xan = local_user(&pool, "xan").await;
        let _member = list::add_members(&pool, list_id_of(&lists, "list"), &[xan.id])
            .await
            .expect("the list membership fixture adds");
        follow_user(&pool, &dana, &xan).await;
        let yanni = local_user(&pool, "yanni").await;

        let xan_row = post(&pool, &xan, "a listed account").await;
        let to_member = reply_to(&pool, &alice, &xan_row).await;
        let (member_streams, _, _) = published(&state, &to_member, &alice).await;
        assert!(
            member_streams.contains(&list_stream(list_id_of(&lists, "list"))),
            "the list policy carries a reply to one of its own members"
        );
        assert!(
            member_streams.contains(&list_stream(list_id_of(&lists, "followed"))),
            "the followed policy carries a reply to an account the owner follows"
        );
        assert!(
            !member_streams.contains(&list_stream(list_id_of(&lists, "none"))),
            "the none policy carries no reply at all"
        );

        let yanni_row = post(&pool, &yanni, "nobody to dana").await;
        let to_outsider = reply_to(&pool, &alice, &yanni_row).await;
        let (outsider_streams, _, _) = published(&state, &to_outsider, &alice).await;
        for list_row in &lists {
            assert!(
                !outsider_streams.contains(&list_stream(list_row.id)),
                "the {} policy drops a reply to an account outside it",
                list_row.replies_policy
            );
        }
    }

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

    #[tokio::test]
    async fn status_created_pushes_update_on_the_public_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let alice_list = list_of(&pool, &dana, &alice, "followed").await;
        let row = post(&pool, &alice, "hello #Rust world").await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "update", "Mastodon's arrival event");
        assert_eq!(
            event.streams,
            canonical(vec![
                "public".to_owned(),
                "public:local".to_owned(),
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
                user_stream(dana.id),
                list_stream(alice_list.id),
            ]),
            "the event rides exactly the computed set — the public timelines, both tag \
             streams, the home circle and the list"
        );
        assert_eq!(event.audience, Audience::Public, "the computed audience");
        assert!(!event.local_only, "a public post is not local-only");
        assert_eq!(
            event.render,
            Some(RenderSpec::Status(row.id)),
            "the update carries the status render hint"
        );
        assert!(
            event.payload.is_none(),
            "and no body: the socket renders it"
        );
    }

    #[tokio::test]
    async fn status_created_pushes_update_on_the_unlisted_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let dana = local_user(&pool, "dana").await;
        follow_user(&pool, &dana, &alice).await;
        let alice_list = list_of(&pool, &dana, &alice, "followed").await;
        let erin = local_user(&pool, "erin").await;
        let row = post_vis(
            &pool,
            &alice,
            "quietly #Rust",
            Visibility::Unlisted,
            &[erin.id],
        )
        .await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "update", "Mastodon's arrival event");
        assert_eq!(
            event.streams,
            canonical(vec![
                "hashtag:rust".to_owned(),
                "hashtag:local:rust".to_owned(),
                user_stream(alice.id),
                user_stream(bob.id),
                user_stream(dana.id),
                user_stream(erin.id),
                list_stream(alice_list.id),
            ]),
            "the unlisted set — hashtag, home (with the mentioned non-follower) and \
             list — and no public stream at all"
        );
        assert_eq!(
            event.audience,
            Audience::Users(sorted_ids(vec![alice.id, bob.id, dana.id, erin.id])),
            "the home circle plus the list owner gate delivery"
        );
        assert!(!event.local_only, "unlisted is still federated");
        assert_eq!(event.render, Some(RenderSpec::Status(row.id)), "the hint");
        assert!(event.payload.is_none(), "and no body");
    }

    #[tokio::test]
    async fn status_created_pushes_update_on_the_private_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let erin = local_user(&pool, "erin").await;
        let row = post_vis(
            &pool,
            &alice,
            "followers only",
            Visibility::Private,
            &[erin.id],
        )
        .await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "update", "Mastodon's arrival event");
        assert_eq!(
            event.streams,
            canonical(vec![user_stream(alice.id), user_stream(bob.id)]),
            "the author's and the accepted follower's home — not the mentioned \
             non-follower, no public stream"
        );
        assert_eq!(
            event.audience,
            Audience::Users(sorted_ids(vec![alice.id, bob.id])),
            "exactly those two"
        );
        assert_eq!(event.render, Some(RenderSpec::Status(row.id)), "the hint");
        assert!(event.payload.is_none(), "and no body");
    }

    #[tokio::test]
    async fn status_created_pushes_update_on_the_direct_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let carol = remote_user(&pool, "carol").await;
        let row = post_vis(
            &pool,
            &alice,
            "for a few eyes only",
            Visibility::Direct,
            &[bob.id, carol.id],
        )
        .await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "update", "Mastodon's arrival event");
        assert_eq!(
            event.streams,
            canonical(vec![
                user_stream(alice.id),
                direct_stream(alice.id),
                user_stream(bob.id),
                direct_stream(bob.id),
            ]),
            "author and every mentioned LOCAL user on both their streams — the remote \
             mention gets nothing"
        );
        assert_eq!(
            event.audience,
            Audience::Users(sorted_ids(vec![alice.id, bob.id])),
            "the recipients alone"
        );
        assert_eq!(event.render, Some(RenderSpec::Status(row.id)), "the hint");
        assert!(event.payload.is_none(), "and no body");
    }

    #[tokio::test]
    async fn status_created_pushes_the_local_visibility_as_local_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let row = post_vis(&pool, &alice, "instance only #Rust", Visibility::Local, &[]).await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "update", "Mastodon's arrival event");
        assert_eq!(
            event.streams,
            canonical(vec![
                "public:local".to_owned(),
                "hashtag:local:rust".to_owned(),
            ]),
            "the local half of the public set and no global stream"
        );
        assert_eq!(
            event.audience,
            Audience::Everyone,
            "every authenticated local user"
        );
        assert!(event.local_only, "the local-only flag rides the event");
        assert_eq!(event.render, Some(RenderSpec::Status(row.id)), "the hint");
        assert!(event.payload.is_none(), "and no body");
    }

    #[tokio::test]
    async fn status_created_pushes_the_remote_statuses_domain_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let carol = remote_user(&pool, "carol").await;
        let row = remote_post(&pool, &carol, "over here #Rust", &["Rust"]).await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(
            event.streams,
            canonical(vec![
                "public".to_owned(),
                "public:remote".to_owned(),
                "public:remote:remote.example".to_owned(),
                "hashtag:rust".to_owned(),
                user_stream(carol.id),
            ]),
            "a remote status ingested through the create path rides the remote-domain \
             streams straight off the target computation"
        );
        assert_eq!(event.audience, Audience::Public, "as the computation says");
        assert!(!event.local_only, "a remote post is not local-only");
        assert_eq!(event.event, "update", "the same arrival event");
        assert_eq!(event.render, Some(RenderSpec::Status(row.id)), "the hint");
    }

    #[tokio::test]
    async fn status_edited_pushes_status_update_on_the_same_streams() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let row = post(&pool, &alice, "before the edit").await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;
        let created = emitted(&mut rx).await;
        status_edited(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "status.update", "Mastodon's redraw event");
        assert_eq!(
            event.streams, created.streams,
            "the redraw rides exactly the streams the post arrived on"
        );
        assert_eq!(event.audience, created.audience, "with the same audience");
        assert_eq!(
            event.local_only, created.local_only,
            "and the same locality"
        );
        assert_eq!(
            event.render,
            Some(RenderSpec::Status(row.id)),
            "the render hint lets each socket redraw the entity"
        );
        assert!(event.payload.is_none(), "and no body rides the bus");
    }

    #[tokio::test]
    async fn status_deleted_mirrors_the_created_streams_with_the_id_payload() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        follow_user(&pool, &bob, &alice).await;
        let row = post(&pool, &alice, "doomed #Rust").await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;
        let created = emitted(&mut rx).await;
        let _tombstone = status::soft_delete(&pool, row.id)
            .await
            .expect("the delete fixture runs");
        status_deleted(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "delete", "Mastodon's withdrawal event");
        assert_eq!(
            event.streams, created.streams,
            "the withdrawal rides exactly the streams the post arrived on"
        );
        assert_eq!(event.audience, created.audience, "with the same audience");
        assert_eq!(
            event.local_only, created.local_only,
            "and the same locality"
        );
        assert_eq!(
            event.payload.as_deref(),
            Some(row.id.to_string().as_str()),
            "the payload is the deleted id as plain text"
        );
        assert_eq!(event.render, None, "no render hint: the row is a tombstone");
    }

    #[tokio::test]
    async fn a_deactivated_author_publishes_nothing_on_any_push() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let ghost = inactive_user(&pool, "ghost").await;
        let row = post(&pool, &ghost, "heard from the grave").await;
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;
        assert_empty(&mut rx);
        status_edited(&state, &row).await;
        assert_empty(&mut rx);
        status_deleted(&state, &row).await;
        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn the_emitters_publish_nothing_for_a_reblog_of_a_deleted_original() {
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
        let mut rx = state.hub.subscribe();

        status_created(&state, &row).await;

        assert_empty(&mut rx);
    }

    /// A stored `favourite` telling: `actor` favourited, `recipient` is told.
    async fn favourite_note(pool: &PgPool, recipient: &User, actor: &User) -> Notification {
        notification::create(
            pool,
            &NewNotification {
                user_id: recipient.id,
                from_account_id: actor.id,
                notification_type: TYPE_FAVOURITE,
                status_id: None,
                emoji: None,
                report_id: None,
                chat_message_id: None,
            },
        )
        .await
        .expect("the notification fixture inserts")
    }

    #[tokio::test]
    async fn notification_created_pushes_the_recipient_streams_with_the_hint() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = favourite_note(&pool, &alice, &bob).await;
        let mut rx = state.hub.subscribe();

        notification_created(&state, &row).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "notification", "Mastodon's notification event");
        assert_eq!(
            event.streams,
            vec![
                user_stream(alice.id),
                format!("user:notification:{}", alice.id),
            ],
            "the event rides exactly the recipient's user and notification streams"
        );
        assert_eq!(
            event.audience,
            Audience::Users(vec![alice.id]),
            "the recipient is the whole audience"
        );
        assert_eq!(
            event.render,
            Some(RenderSpec::Notification(row.id)),
            "the event carries the notification render hint"
        );
        assert!(event.payload.is_none(), "no body: the socket renders it");
        assert!(
            !event.local_only,
            "a notification is not flagged local-only"
        );
    }

    #[tokio::test]
    async fn notification_created_publishes_nothing_the_recipient_mutes() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _mute = mute::mute(&pool, &alice, &bob, mute::MuteOptions::default())
            .await
            .expect("the mute fixture writes");
        let row = favourite_note(&pool, &alice, &bob).await;
        let mut rx = state.hub.subscribe();

        notification_created(&state, &row).await;

        assert_empty(&mut rx);
    }

    #[tokio::test]
    async fn chat_updated_pushes_the_owners_chat_stream_with_the_hint() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let row = chat::get_or_create(&pool, &alice, &bob)
            .await
            .expect("the chat fixture creates");
        let mut rx = state.hub.subscribe();

        chat_updated(&state, alice.id, row.id);

        let event = emitted(&mut rx).await;
        assert_eq!(
            event.event, "pleroma:chat_update",
            "Pleroma's chat update event"
        );
        assert_eq!(
            event.streams,
            vec![format!("user:pleroma_chat:{}", alice.id)],
            "the event rides exactly the owner's pleroma_chat stream"
        );
        assert_eq!(
            event.audience,
            Audience::Users(vec![alice.id]),
            "the owner is the whole audience"
        );
        assert_eq!(
            event.render,
            Some(RenderSpec::Chat(row.id)),
            "the event carries the chat render hint"
        );
        assert!(event.payload.is_none(), "no body: the socket renders it");
        assert!(!event.local_only, "a chat update is not flagged local-only");
    }

    #[tokio::test]
    async fn conversation_updated_pushes_the_readers_direct_stream_with_the_hint() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let participation_id = Uuid::now_v7();
        let mut rx = state.hub.subscribe();

        conversation_updated(&state, alice.id, participation_id);

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "conversation", "Mastodon's conversation event");
        assert_eq!(
            event.streams,
            vec![direct_stream(alice.id)],
            "the event rides exactly the reader's direct stream"
        );
        assert_eq!(
            event.audience,
            Audience::Users(vec![alice.id]),
            "the reader is the whole audience"
        );
        assert_eq!(
            event.render,
            Some(RenderSpec::Conversation(participation_id)),
            "the event carries the reader's own participation render hint"
        );
        assert!(event.payload.is_none(), "no body: the socket renders it");
        assert!(
            !event.local_only,
            "a conversation update is not flagged local-only"
        );
    }

    #[tokio::test]
    async fn filters_changed_pushes_the_bare_event_on_the_owners_user_stream() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let mut rx = state.hub.subscribe();

        filters_changed(&state, alice.id);

        let event = emitted(&mut rx).await;
        assert_eq!(
            event.event, "filters_changed",
            "Mastodon's filter-changed event"
        );
        assert_eq!(
            event.streams,
            vec![user_stream(alice.id)],
            "the event rides exactly the owner's user stream"
        );
        assert_eq!(
            event.audience,
            Audience::Users(vec![alice.id]),
            "the owner is the whole audience"
        );
        assert!(
            event.payload.is_none(),
            "Mastodon sends filters_changed with no payload"
        );
        assert!(event.render.is_none(), "and with nothing to render");
        assert!(
            !event.local_only,
            "a filter change is not flagged local-only"
        );
    }

    #[tokio::test]
    async fn announcement_published_pushes_the_entity_to_every_active_local() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let alice = local_user(&pool, "alice").await;
        let bob = local_user(&pool, "bob").await;
        let _inactive = inactive_user(&pool, "sleepy").await;
        let _remote = remote_user(&pool, "carol").await;
        let announcement = serde_json::json!({"id": "1", "content": "<p>Up!</p>"});
        let mut rx = state.hub.subscribe();

        announcement_published(&state, &announcement).await;

        let event = emitted(&mut rx).await;
        assert_eq!(event.event, "announcement", "Mastodon's announcement event");
        assert_eq!(
            event.streams,
            canonical(vec![user_stream(alice.id), user_stream(bob.id)]),
            "one user stream per active local user, none for the deactivated or the mirror"
        );
        assert_eq!(
            event.audience,
            Audience::Users(sorted_ids(vec![alice.id, bob.id])),
            "the same roster is the whole audience"
        );
        assert_eq!(
            event.payload.as_deref(),
            Some(announcement.to_string().as_str()),
            "the entity ships as Mastodon's string-in-envelope"
        );
        assert!(
            event.render.is_none(),
            "the payload is the entity: no render hint rides"
        );
        assert!(
            !event.local_only,
            "the roster, not the flag, is what keeps it to the locals"
        );
    }

    #[tokio::test]
    async fn announcement_published_publishes_nothing_without_active_local_users() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let state = app_state(&pool);
        let _inactive = inactive_user(&pool, "sleepy").await;
        let _remote = remote_user(&pool, "carol").await;
        let mut rx = state.hub.subscribe();

        announcement_published(&state, &serde_json::json!({"id": "1"})).await;

        assert_empty(&mut rx);
    }
}
