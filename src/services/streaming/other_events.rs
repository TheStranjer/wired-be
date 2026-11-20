//! The streaming bus's other producers: Mastodon's `notification`,
//! Pleroma's `pleroma:chat_update`, Mastodon's `conversation`,
//! `filters_changed` and `announcement`. Each hands a finished
//! [`StreamEvent`] to [`Hub::publish`](crate::streaming::hub::Hub::publish)
//! from the action hook that calls it, best-effort: the write behind the
//! event already committed, so a lost push costs a socket a redraw, never
//! a failure.

use tracing::warn;
use uuid::Uuid;

use crate::{
    models::{mute, notification::Notification, user},
    state::AppState,
    streaming::hub::{Audience, RenderSpec, StreamEvent},
};

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
///
/// [`status_created`]: crate::services::streaming::status_created
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
/// [`crate::api::admin::content`]'s `POST /announcements` is the caller: a
/// notice the admin makes is published the moment its row stands, so a
/// client's socket learns of it without a re-read. Updates and deletes are
/// not published — Mastodon sends the `announcement` event for a new
/// notice alone, and its clients re-read the list on the next poll.
///
/// Best-effort like [`status_created`]: a failed roster read is a log line
/// and nothing published. With no active local users there is nobody to name
/// in either gate, so the roster's emptiness is the other silence.
///
/// [`status_created`]: crate::services::streaming::status_created
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use crate::{
        models::{
            chat,
            notification::{self, NewNotification, TYPE_FAVOURITE},
            user::User,
        },
        services::streaming::test_support::{
            app_state, assert_empty, canonical, direct_stream, emitted, inactive_user, local_user,
            remote_user, sorted_ids, user_stream,
        },
    };

    use super::*;

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
