//! The follow handshake this instance starts and answers: a local follower's
//! `Follow` of a REMOTE account (and the `Undo` of a withdrawal) under the
//! row's own stored `ap_id`, and the `Accept` / `Reject` a local followee
//! sends back for a delivered request.
//!
//! The endpoint arms that answer a follow *request* and refuse a forced
//! removal are [`super::follow_requests`]', and the one-recipient publisher
//! hand is the root's.

use serde_json::{Value, json};

use super::publish_only_to;
use crate::{
    error::AppResult,
    federation::{
        activities::{AnsweredActivity, accept_json, follow_json, reject_json, undo_json},
        publisher::{self, PublishOpts},
    },
    models::{follow::Follow, user::User},
    state::AppState,
    util::id::new_id,
};

/// The delivered `Follow` a local followee answers: the activity's own id,
/// which the answer embeds, and the follower's actor id, which is the
/// answer's sole recipient.
#[derive(Debug)]
pub struct FollowAnswer<'a> {
    /// The `Follow` activity's `ActivityPub` id — embedded verbatim in the
    /// answer, because Mastodon matches an `Accept` by the activity it
    /// carries, not by a reference it would have to fetch.
    pub follow_id: &'a str,
    /// The follower's actor id — the delivered `Follow`'s own `actor`.
    pub follower_ap_id: &'a str,
}

/// Answer a remote account's follow of `local_followee` with an `Accept`.
///
/// The document is [`accept_json`]'s: a fresh `{BASE_URL}/activities/{uuid}`
/// id (decision 6), `actor` the followee, `to` the follower, and the
/// delivered `Follow` embedded as `{id, type, actor, object}`. The fan-out
/// runs [`publisher::publish`] right here — this caller is the incoming
/// dispatcher's own job, and `only_to` is the follower, so the answer lands
/// in their personal inbox alone (the followee's other followers have no
/// business with who follows them). Returns how many deliveries were
/// queued: `0` for a follower nothing can be delivered to, and for an
/// instance with federation disabled.
///
/// # Errors
///
/// Any database failure behind the fan-out's recipient reads, or a failed
/// delivery-job insert, arrives as
/// [`AppError::Database`](crate::error::AppError) — the dispatcher's retry
/// runs the idempotent fold again.
pub async fn accept_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
) -> AppResult<usize> {
    answer_follow(state, local_followee, follow, true).await
}

/// Refuse a remote account's follow of `local_followee` with a `Reject`.
///
/// [`accept_follow`]'s refusal twin: same document shape under
/// [`reject_json`]'s type and the same one-recipient fan-out. The inbound
/// handler runs it for a pair blocked either way, before anything of the
/// follow is stored.
///
/// # Errors
///
/// Those of [`accept_follow`].
pub async fn reject_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
) -> AppResult<usize> {
    answer_follow(state, local_followee, follow, false).await
}

/// The shared body of [`accept_follow`] and [`reject_follow`]: the embedded
/// `Follow`, the answer document, the one-recipient publish.
async fn answer_follow(
    state: &AppState,
    local_followee: &User,
    follow: &FollowAnswer<'_>,
    accepted: bool,
) -> AppResult<usize> {
    let embedded = json!({
        "id": follow.follow_id,
        "type": "Follow",
        "actor": follow.follower_ap_id,
        "object": local_followee.ap_id,
    });
    let published = chrono::Utc::now();
    let to = [follow.follower_ap_id];
    let answer = AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &local_followee.ap_id,
        to: &to,
        cc: &[],
        answered: &embedded,
        published: &published,
    };
    let document = if accepted {
        accept_json(&answer)
    } else {
        reject_json(&answer)
    };
    let opts = PublishOpts {
        extra_inboxes: Vec::new(),
        only_to: Some(vec![follow.follower_ap_id.to_owned()]),
    };
    publisher::publish(state, local_followee, &document, opts).await
}

/// Announce the follow or unfollow the caller just performed.
///
/// Only a REMOTE followee is worth announcing (a local one is settled in
/// our own database) and only a local follower's activity may be signed.
/// `following` selects the direction: the row's own `Follow` —
/// [`follow_json`] under the stored `ap_id`, the follower as `actor`, the
/// followee as `object` and as the only `to` address — or, for the
/// withdrawal, the `Undo` wrapping that very document under a fresh
/// `{BASE_URL}/activities/{uuid}` id (decision 6), Pleroma's
/// `ActivityPub.follow`/`unfollow` pair. [`publish`] is then run with
/// `only_to` naming the followee, so the activity lands in their personal
/// inbox alone and the fan-out never touches the follower's own remote
/// followers.
///
/// A restated follow — the endpoint's idempotent repeat of an open pair, an
/// import re-run — re-sends the same activity id, and the per-activity,
/// per-inbox deduplication of the delivery queue keeps the restatement a
/// single sending. A pair with no row (a withdrawal that found nothing to
/// take back) and a row with no `ap_id` announce nothing.
///
/// [`publish`]: crate::federation::publisher::publish
pub async fn after_follow_changed(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) {
    if !state.config.federation_enabled.get() {
        return;
    }
    if target.local.get() || !follower.local.get() {
        return; // a local pair is settled in the database; only our actors sign
    }
    let Some(document) = follow_announcement(state, follower, target, follow, following) else {
        return; // no row, or an id-less one: there is nothing to announce
    };
    publish_only_to(state, follower, &target.ap_id, &document, "follow").await;
}

/// The document [`after_follow_changed`] announces: the row's own `Follow`
/// when the write opened (or restated) the relationship, and the `Undo`
/// wrapping that document — a fresh id, the withdrawal's own moment — when
/// it took the follow back. `None` when the write left no row, or the row
/// carries no id to stand behind the document.
fn follow_announcement(
    state: &AppState,
    follower: &User,
    target: &User,
    follow: Option<&Follow>,
    following: bool,
) -> Option<Value> {
    let row = follow?;
    let document = follow_document(state, row, follower, target)?;
    if following {
        return Some(document);
    }
    let now = chrono::Utc::now();
    let to = [target.ap_id.as_str()];
    Some(undo_json(&AnsweredActivity {
        config: &state.config,
        id: new_id(),
        actor_ap_id: &follower.ap_id,
        to: &to,
        cc: &[],
        answered: &document,
        published: &now,
    }))
}

/// The `Follow` behind the relationship row `row`: [`follow_json`] with its
/// `id` replaced by the stored string — the builder mints ids from a fresh
/// uuid and the row's `ap_id` is its own (decision 6), so only the stored
/// text names the activity the peer was sent and must hear withdrawn.
/// `None` when the row carries no id at all (a mirrored follow, which this
/// instance does not own).
fn follow_document(
    state: &AppState,
    row: &Follow,
    follower: &User,
    target: &User,
) -> Option<Value> {
    let stored_id = row.ap_id.as_deref()?;
    let mut document = follow_json(
        &state.config,
        row.id,
        &follower.ap_id,
        &target.ap_id,
        &row.inserted_at,
    );
    if let Some(fields) = document.as_object_mut() {
        let _previous = fields.insert("id".to_owned(), json!(stored_id));
    }
    Some(document)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    use crate::{
        config::TEST_BASE_URL,
        models::follow,
        services::federation_out::test_support::{
            app_state, assert_fresh_activity_id, delivery_jobs, job_count, local, publish_jobs,
            remote,
        },
        util::time::mastodon_time,
    };

    // ───────────────  the follow-write hooks  ───────────────

    /// `follower`'s follow of `target` recorded through the model, asserted
    /// to carry what a local follower's row always does: a freshly minted
    /// `ap_id` (decision 6) — the id the announcement must reuse.
    async fn recorded_follow(pool: &PgPool, follower: &User, target: &User) -> Follow {
        let row = follow::follow(pool, TEST_BASE_URL, follower, target)
            .await
            .expect("the follow fixture writes");
        assert!(
            row.ap_id.is_some(),
            "a local follower's row carries a minted activity id"
        );
        row
    }

    /// Following a REMOTE account queues the row's `Follow` straight to the
    /// followee's personal inbox: the stored `ap_id` as the activity id,
    /// the follower as actor, the followee as `object` and as the only
    /// address — nobody else, not even the follower's own followers.
    #[tokio::test]
    async fn following_a_remote_account_delivers_the_stored_follow() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the request is announced once");
        assert!(
            publish_jobs(&pool).await.is_empty(),
            "a one-recipient follow ships without a fan-out job"
        );
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/fan/inbox",
            "the followee's personal inbox is the sole target"
        );
        assert_eq!(
            jobs[0]["actor_id"],
            alice.id.to_string(),
            "signed as the follower"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Follow");
        assert_eq!(
            body["id"],
            row.ap_id.expect("a local follower's row carries its id"),
            "the activity id is the stored follows.ap_id"
        );
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["object"], fan.ap_id);
        assert_eq!(body["to"], json!([fan.ap_id]));
        assert_eq!(body["cc"], json!([]));
        assert_eq!(body["published"], mastodon_time(&row.inserted_at));
    }

    /// Unfollowing delivers the `Undo` that wraps the very `Follow` the row
    /// stands for: the wrapper's own fresh id outside, the stored id inside,
    /// and the same single addressee.
    #[tokio::test]
    async fn unfollowing_a_remote_account_delivers_the_undo() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let stored_id = row
            .ap_id
            .clone()
            .expect("a local follower's row carries its id");
        let _withdrawn = follow::unfollow(&pool, &alice, &fan)
            .await
            .expect("the withdrawal runs");
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), false).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(jobs.len(), 1, "the withdrawal is announced once");
        assert_eq!(
            jobs[0]["inbox"], "https://remote.example/users/fan/inbox",
            "the withdrawn follow's addressee hears the taking-back"
        );
        let body = &jobs[0]["body"];
        assert_eq!(body["type"], "Undo");
        assert_eq!(body["actor"], alice.ap_id);
        assert_eq!(body["to"], json!([fan.ap_id]));
        assert_eq!(body["cc"], json!([]));
        let id = body["id"].as_str().expect("the undo has an id");
        assert_fresh_activity_id(id);
        assert_ne!(
            id,
            stored_id.as_str(),
            "the wrapper stands under a fresh id"
        );
        assert_eq!(body["object"]["type"], "Follow");
        assert_eq!(
            body["object"]["id"], stored_id,
            "the embedded follow keeps the row's stored id"
        );
        assert_eq!(body["object"]["actor"], alice.ap_id);
        assert_eq!(body["object"]["object"], fan.ap_id);
    }

    /// A local followee, a mirrored follower and a write that left no row
    /// behind are the documented silences: no delivery, no fan-out, nothing.
    #[tokio::test]
    async fn follow_writes_that_are_not_ours_to_announce_publish_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let neighbour = local(&pool, "neighbour").await;
        let fan = remote(&pool, "fan").await;
        let other = remote(&pool, "other").await;
        let open_pair = recorded_follow(&pool, &alice, &neighbour).await;
        let mirrored = follow::follow(&pool, TEST_BASE_URL, &fan, &other)
            .await
            .expect("the mirror's follow writes");
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &neighbour, Some(&open_pair), true).await;
        after_follow_changed(&state, &alice, &neighbour, Some(&open_pair), false).await;
        after_follow_changed(&state, &fan, &other, Some(&mirrored), true).await;
        after_follow_changed(&state, &alice, &fan, None, false).await;

        assert_eq!(
            job_count(&pool).await,
            0,
            "a local pair, a mirror's follow and an empty withdrawal queue nothing"
        );
    }

    /// A follow row with no `ap_id` has no document to stand behind — both
    /// arms fall silent, exactly as the block hook's twin rule does.
    #[tokio::test]
    async fn a_follow_without_a_stored_id_publishes_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let mut row = recorded_follow(&pool, &alice, &fan).await;
        row.ap_id = None;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;
        after_follow_changed(&state, &alice, &fan, Some(&row), false).await;

        assert_eq!(job_count(&pool).await, 0, "an id-less row has no document");
    }

    /// The endpoint's idempotent repeat — a second follow of the open pair —
    /// restates the same activity, and the per-inbox delivery dedup is what
    /// keeps the restated request a single sending.
    #[tokio::test]
    async fn a_repeated_follow_restates_one_delivery() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let fan = remote(&pool, "fan").await;
        let row = recorded_follow(&pool, &alice, &fan).await;
        let state = app_state(&pool);

        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;
        after_follow_changed(&state, &alice, &fan, Some(&row), true).await;

        let jobs = delivery_jobs(&pool).await;
        assert_eq!(
            jobs.len(),
            1,
            "the re-send collapses into the live delivery"
        );
        assert_eq!(
            jobs[0]["body"]["id"],
            row.ap_id.expect("a local follower's row carries its id"),
            "and it is the row's own activity, not a re-mint"
        );
    }
}
