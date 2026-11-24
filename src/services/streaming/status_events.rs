//! The status emitters: Mastodon's `update`, `delete` and `status.update`
//! for one stored row. Each resolves the row's answer through
//! [`targets`](crate::services::streaming::targets) and hands the finished
//! [`StreamEvent`] to the shared body in [`publish`]; the writing hook that
//! calls one never fails its request over a push.

use sqlx::PgPool;

use crate::{
    error::AppResult,
    models::status::{self, Status},
    state::AppState,
    streaming::hub::RenderSpec,
};

use self::publish::{Emission, publish_status, publish_withdrawal};

mod publish;

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
/// [never-published cases]) or when
/// a read behind the computation fails, and every such refusal is a log
/// line. A remote row ingested through the same create path publishes from
/// here too — its `public:remote*` streams come from [`targets`].
///
/// [`targets`]: crate::services::streaming::targets
/// [never-published cases]: crate::services::streaming#when-a-status-is-never-published
pub async fn status_created(state: &AppState, row: &Status) {
    let emission = Emission {
        event: "update",
        payload: None,
        render: Some(RenderSpec::Status(row.id)),
    };
    publish_status(state, row, emission).await;
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
/// the same refusal cases as [`status_created`], except that a reblog row
/// whose original is a tombstone still publishes: that is the state every
/// reblog tombstoned along with its original is in, and the `delete` is what
/// removes the boost from the boosters' followers' columns.
///
/// [`after_status_deleted`]: crate::services::delete_status
/// [`targets`]: crate::services::streaming::targets
pub async fn status_deleted(state: &AppState, row: &Status) {
    let emission = Emission {
        event: "delete",
        payload: Some(row.id.to_string()),
        render: None,
    };
    publish_withdrawal(state, row, emission).await;
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
///
/// [`targets`]: crate::services::streaming::targets
pub async fn status_edited(state: &AppState, row: &Status) {
    let emission = Emission {
        event: "status.update",
        payload: None,
        render: Some(RenderSpec::Status(row.id)),
    };
    publish_status(state, row, emission).await;
}

/// Whether `row` is a reblog whose original is gone: [`status::get_by_id`]
/// hides tombstones, so its `None` is both the deleted and the never-there
/// answer, and each of them leaves a boost with nothing to announce.
pub(super) async fn reblog_without_original(pool: &PgPool, row: &Status) -> AppResult<bool> {
    let Some(original_id) = row.reblog_of_id else {
        return Ok(false);
    };
    Ok(status::get_by_id(pool, original_id).await?.is_none())
}

#[cfg(test)]
mod tests {
    use crate::{
        models::status::Visibility,
        services::streaming::test_support::{
            app_state, assert_empty, boost, canonical, direct_stream, emitted, follow_user,
            inactive_user, list_of, list_stream, local_user, post, post_vis, remote_post,
            remote_user, sorted_ids, user_stream,
        },
        streaming::hub::Audience,
    };

    use super::*;

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
}
