//! The post-write hook of [`create_status`]: `after_status_created` lets a
//! fresh local status out of the database.
//!
//! The hook runs the mention and subscription notifications, the streaming
//! `update`, the rich-media card job, the conversation participation
//! (`participate_in_conversation`) and the federation fan-out — and
//! [`crate::services::ingest`] runs a mirrored remote status through
//! exactly the same arms.
//!
//! [`create_status`]: crate::services::create_status::create_status

use tracing::warn;

use crate::{
    models::{conversation, status},
    services::{federation_out, notify, streaming},
    state::AppState,
};

/// The post-write hook of [`create_status`]: the tickets that let a fresh
/// local status out of the database it was just written to — the mention
/// notifications for the mentioned accounts and the replied-to author
/// (landed from [`notify::on_mention`], Pleroma's `Create` notification
/// arm), the subscription `status` notifications for the local users who
/// subscribed to the author (landed from [`notify::on_subscribed_status`],
/// Pleroma's subscriber arm of the same create), the streaming `update` to
/// every timeline the new post belongs to (landed from
/// [`streaming::status_created`], Pleroma's `stream_out`), the
/// [`rich_media.fetch`](crate::jobs::rich_media) card job the post's first
/// link schedules for itself (see
/// [`schedule_fetch`](crate::jobs::rich_media::schedule_fetch)), and the
/// federation of the `Create` activity through the job queue (landed from
/// [`federation_out::after_status_created`], decision 11's queued fan-out)
/// — wire themselves in here, and only here.
///
/// `status` is the stored row: the id the tellings name, the author they
/// do not tell, the `visibility` that keeps a `direct` post out of the
/// subscriber arm, and the `reblog_of_id` that keeps a boost row out of
/// both. The stream push names the row by render hint rather than shipping
/// the rendered entity, so the streaming consumer needed no growth of this
/// one call's signature; the federation arm renders its own document from
/// the row, through [`crate::federation::note`].
///
/// [`notify::on_mention`]: crate::services::notify::on_mention
/// [`streaming::status_created`]: crate::services::streaming::status_created
/// [`federation_out::after_status_created`]: crate::services::federation_out::after_status_created
///
/// `pub(crate)`, not private: the federation ingest path
/// ([`crate::services::ingest`] step 9) runs a freshly mirrored remote
/// status through exactly this hook once its write commits — a new post is
/// a new post for the notifications, cards and conversation arms whoever
/// authored it, and every arm here already decides on the *recipient's*
/// locality, never the author's.
pub(crate) async fn after_status_created(state: &AppState, status: &status::Status) {
    notify::on_mention(state, status).await;
    notify::on_quote(state, status).await;
    notify::on_subscribed_status(state, status).await;
    streaming::status_created(state, status).await;
    crate::jobs::rich_media::schedule_fetch(state, status).await;
    participate_in_conversation(state, status).await;
    federation_out::after_status_created(state, status).await;
}

/// The conversation arm of [`after_status_created`]: a `direct` status is
/// recorded in its DM thread — the thread row found-or-created, the
/// status linked to it, every local participant's side bumped and every
/// addressee (remote included) stored on the recipient lists — through
/// [`conversation::participate_for_status`], which reads the roster off
/// the status's stored mention links. A status of any other visibility
/// does nothing.
///
/// Once that write commits, the thread's local participants are told: each
/// is pushed a [`streaming::conversation_updated`] on their own `direct`
/// stream, carrying their OWN participation id as the render hint — the
/// roster [`conversation::participations_for_status`] reads back. A status
/// of any other visibility threads nothing and so publishes nothing; a
/// remote participant owns no participation row and is simply absent.
///
/// Best-effort beside the rest of this hook: the post itself is already
/// committed, so a participation that fails to land, a roster that fails to
/// read, or a publish that has nothing to name are all just log lines — the
/// status stands, and its read view simply lacks the thread or the push.
async fn participate_in_conversation(state: &AppState, status: &status::Status) {
    if let Err(err) = conversation::participate_for_status(&state.db, status).await {
        warn!(
            status = %status.id,
            %err,
            "could not record the conversation participations of a status"
        );
        return;
    }
    let participants = match conversation::participations_for_status(&state.db, status.id).await {
        Ok(participants) => participants,
        Err(err) => {
            warn!(
                status = %status.id,
                %err,
                "could not read the conversation participants to publish"
            );
            return;
        }
    };
    for participation in participants {
        streaming::conversation_updated(state, participation.user_id, participation.id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::{
            status::{NewStatus, Visibility},
            user::User,
        },
        services::create_status::test_support::{BASE, created, posted, state, user},
        test_fixtures::remote_user,
    };
    /// The `after_status_created` hook pushes Mastodon's `update` with the
    /// render hint onto the hub the moment the row is written — the create
    /// path a remote ingestion later shares — and the preview arm, which
    /// writes nothing, fires nothing.
    #[tokio::test]
    async fn the_create_hook_pushes_the_update_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "hooking-author").await;
        let mut rx = app_state.hub.subscribe();

        let entity = created(&app_state, &alice, posted("streamed from the hook")).await;

        let event = rx.recv().await.expect("the create hook published an event");
        assert_eq!(event.event, "update", "the fresh post arrives");
        let hinted = Uuid::parse_str(&entity.id).expect("the entity names its status");
        assert_eq!(
            event.render,
            Some(crate::streaming::hub::RenderSpec::Status(hinted)),
            "the push carries the created status's render hint"
        );
        assert!(
            event.streams.contains(&"public".to_owned()),
            "a public post rides the public timeline: {:?}",
            event.streams
        );
        assert!(event.payload.is_none(), "the entity is rendered per socket");
    }

    /// Drains every event already buffered on `rx`: the hub hands a publish to
    /// its local subscribers synchronously, so an awaited emitter call is all
    /// here by the time this reads.
    fn drained(
        rx: &mut tokio::sync::broadcast::Receiver<Arc<crate::streaming::hub::StreamEvent>>,
    ) -> Vec<crate::streaming::hub::StreamEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(Arc::unwrap_or_clone(event));
        }
        events
    }

    /// `author`'s `direct` status addressing everyone in `mentioned`, written
    /// through the model — the unthreaded row the create hook's conversation
    /// arm then records and announces.
    async fn direct_status(
        pool: &PgPool,
        author: &User,
        mentioned: &[&User],
        text: &str,
    ) -> status::Status {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Direct,
                mentioned_user_ids: mentioned.iter().map(|who| who.id).collect(),
                ..NewStatus::local(author, text)
            },
        )
        .await
        .expect("the direct fixture inserts")
    }

    /// `reader`'s OWN participation id in the thread `status_id` was recorded
    /// into — the id a conversation hint must name for that reader.
    async fn participation_id(pool: &PgPool, status_id: Uuid, reader: Uuid) -> Uuid {
        let thread = status::get_by_id(pool, status_id)
            .await
            .expect("the row reads")
            .expect("the fixture is live")
            .direct_conversation_id
            .expect("the conversation arm threaded the fixture");
        conversation::participation_ids(pool, reader, &[thread])
            .await
            .expect("the participation reads")
            .get(&thread)
            .copied()
            .expect("the reader is a local participant of the fixture thread")
    }

    /// A `direct` status by a local author to a local addressee tells both
    /// local sides on their own `direct` stream, each with their OWN
    /// participation id as the render hint — the shape `conversation` clients
    /// render their conversation list from.
    #[tokio::test]
    async fn a_direct_status_publishes_a_conversation_event_per_local_participant() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "dm-author").await;
        let bob = user(&pool, "dm-recipient").await;
        let dm = direct_status(&pool, &alice, &[&bob], "@dm-recipient psst").await;
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &dm).await;

        let events = drained(&mut rx);
        assert_eq!(events.len(), 2, "one event per local side of the thread");
        for event in &events {
            assert_eq!(event.event, "conversation", "Mastodon's conversation event");
            assert!(event.payload.is_none(), "no body: the socket renders it");
            assert!(
                !event.local_only,
                "a conversation update is not flagged local-only"
            );
        }
        let alice_participation = participation_id(&pool, dm.id, alice.id).await;
        let bob_participation = participation_id(&pool, dm.id, bob.id).await;
        assert_ne!(
            alice_participation, bob_participation,
            "each side owns its own participation row"
        );
        let alice_event = events
            .iter()
            .find(|event| event.streams == vec![format!("direct:{}", alice.id)])
            .expect("the author is told on her own direct stream");
        assert_eq!(
            alice_event.audience,
            crate::streaming::hub::Audience::Users(vec![alice.id]),
            "the reader is the whole audience"
        );
        assert_eq!(
            alice_event.render,
            Some(crate::streaming::hub::RenderSpec::Conversation(
                alice_participation
            )),
            "alice's hint names her OWN participation"
        );
        let bob_event = events
            .iter()
            .find(|event| event.streams == vec![format!("direct:{}", bob.id)])
            .expect("the local addressee is told on his own direct stream");
        assert_eq!(
            bob_event.audience,
            crate::streaming::hub::Audience::Users(vec![bob.id]),
            "the reader is the whole audience"
        );
        assert_eq!(
            bob_event.render,
            Some(crate::streaming::hub::RenderSpec::Conversation(
                bob_participation
            )),
            "bob's hint names his OWN participation"
        );
    }

    /// A status of any visibility but `direct` records no thread and so
    /// announces no conversation: the arm is a no-op beside the write.
    #[tokio::test]
    async fn a_non_direct_status_publishes_no_conversation_event() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "private-author").await;
        let bob = user(&pool, "private-tagged").await;
        let post = status::insert(
            &pool,
            BASE,
            &NewStatus {
                visibility: Visibility::Private,
                mentioned_user_ids: vec![bob.id],
                ..NewStatus::local(&alice, "@private-tagged hello")
            },
        )
        .await
        .expect("the private fixture inserts");
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &post).await;

        assert!(
            drained(&mut rx).is_empty(),
            "a followers-only post threads nothing and tells no conversation"
        );
    }

    /// A `direct` status addressed to a remote account as well as a local one
    /// tells every LOCAL participant and nothing for the remote: a remote
    /// account owns no participation row, so it is absent from the roster the
    /// arm publishes to.
    #[tokio::test]
    async fn a_direct_status_publishes_nothing_for_a_remote_participant() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let app_state = state(&pool);
        let alice = user(&pool, "federation-author").await;
        let bob = user(&pool, "federation-recipient").await;
        let carol = remote_user(&pool, "federation-carol").await;
        let dm = direct_status(&pool, &alice, &[&bob, &carol], "a mixed DM").await;
        let mut rx = app_state.hub.subscribe();

        participate_in_conversation(&app_state, &dm).await;

        let events = drained(&mut rx);
        assert_eq!(events.len(), 2, "only the two local sides are told");
        assert!(
            events.iter().all(|event| event
                .streams
                .iter()
                .all(|stream| *stream != format!("direct:{}", carol.id))),
            "the remote addressee owns no participation and is published to nothing"
        );
    }
}
