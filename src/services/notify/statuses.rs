//! The status-action tellings of [`crate::services::notify`]: the mention
//! round of a fresh post, the boost and the like, each with its withdrawal
//! arm. The shared creation plumbing these calls live in the parent module.

use tracing::warn;

use crate::{
    error::AppResult,
    models::{
        favourite::Favourite,
        mention,
        notification::{TYPE_FAVOURITE, TYPE_MENTION, TYPE_REBLOG},
        status::{self, Status as StatusRow},
        status_visibility,
        user::{self, User},
    },
    state::AppState,
};

use super::{forget, tell, tell_local};

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
/// already name them — but only when the reply is visible to that author
/// ([`status_visibility::can_view`]): a `private` or `direct` reply that
/// leaves them out of its audience must not push its text to them. A stored
/// mention link is the addressing, so those recipients are told as written.
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
        && status_visibility::can_view(&state.db, row, Some(&parent_author)).await?
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
/// [`notification::delete_for_status`](crate::models::notification::delete_for_status)
/// already took with it — nothing left to tell or take back either way.
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
    use chrono::Utc;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::{
        models::{
            favourite,
            status::{NewStatus, Visibility},
        },
        services::notify::test_support::{BASE, app_state, facts, local, post, remote, rows},
    };

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

    /// A reply by the remote `author` to `parent` (by `parent_author`) with
    /// the given `visibility`, storing mention links to `linked`.
    async fn remote_reply(
        pool: &PgPool,
        author: &User,
        visibility: Visibility,
        linked: &[&User],
        parent: (&User, &StatusRow),
    ) -> StatusRow {
        status::insert(
            pool,
            BASE,
            &NewStatus {
                uri: Some(format!("https://remote.example/objects/{}", Uuid::now_v7())),
                visibility,
                local: false,
                mentioned_user_ids: linked.iter().map(|found| found.id).collect(),
                in_reply_to_id: Some(parent.1.id),
                in_reply_to_account_id: Some(parent.0.id),
                ..NewStatus::local(author, "a remote answer")
            },
        )
        .await
        .expect("the remote reply inserts")
    }

    #[tokio::test]
    async fn a_public_reply_tells_the_parent_author_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "pv-alice").await;
        let bob = local(pool, "pv-bob").await;

        let parent = post(pool, &alice, "a root").await;
        let row = mentioned(pool, &bob, "an answer", &[], Some((&alice, &parent))).await;
        on_mention(&state, &row).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_MENTION.to_owned(), bob.id, Some(row.id))],
            "a public reply is visible to the parent author, who is told once"
        );
    }

    #[tokio::test]
    async fn a_reply_the_parent_author_cannot_see_tells_them_nothing() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "iv-alice").await;
        let mallory = remote(pool, "iv-mallory", "remote.example").await;

        let parent = post(pool, &alice, "a root").await;
        let private_reply =
            remote_reply(pool, &mallory, Visibility::Private, &[], (&alice, &parent)).await;
        on_mention(&state, &private_reply).await;
        let direct_reply =
            remote_reply(pool, &mallory, Visibility::Direct, &[], (&alice, &parent)).await;
        on_mention(&state, &direct_reply).await;
        assert!(
            rows(pool, alice.id).await.is_empty(),
            "a private or direct reply that does not address her tells her nothing"
        );
    }

    #[tokio::test]
    async fn a_direct_reply_that_mentions_the_parent_author_tells_them_once() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool();
        let state = app_state(pool);
        let alice = local(pool, "dm-alice").await;
        let mallory = remote(pool, "dm-mallory", "remote.example").await;

        let parent = post(pool, &alice, "a root").await;
        let row = remote_reply(
            pool,
            &mallory,
            Visibility::Direct,
            &[&alice],
            (&alice, &parent),
        )
        .await;
        on_mention(&state, &row).await;
        assert_eq!(
            facts(pool, alice.id).await,
            vec![(TYPE_MENTION.to_owned(), mallory.id, Some(row.id))],
            "the mention link addresses her: told once, not duplicated"
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
}
