//! The viewer-relative arms of the timeline statement: the `reply_visibility`
//! filter's arms and the moderation exclusions (blocks, mutes, domain
//! blocks, thread mutes) the viewer lives by.

use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::models::user::User;

use super::{ACCEPTED_FOLLOW_STATE, MUTE_LIVE_ARMS, REBLOG_AUTHOR_SQL, ReplyVisibility};

/// Appends the `reply_visibility` group for `visibility`, judged by the
/// reply's stored target (`in_reply_to_account_id`) and @-mentions;
/// non-replies and the viewer's own replies always survive. With no viewer
/// there is nothing to reply *to* or *for*, so — as Pleroma's clause that
/// requires a `%User{}` — the filter appends nothing.
pub(super) fn push_reply_visibility(
    builder: &mut QueryBuilder<Postgres>,
    visibility: ReplyVisibility,
    viewer: Option<&User>,
) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    let _open: &mut _ = builder
        .push(" AND (s.in_reply_to_id IS NULL OR s.user_id = ")
        .push_bind(viewer_id);
    match visibility {
        ReplyVisibility::SelfOnly => push_self_arms(builder, viewer_id),
        ReplyVisibility::Following => push_following_arms(builder, viewer_id),
    }
    let _close: &mut _ = builder.push(")");
}

/// The `self` arms: the reply's author-side target is the viewer, or the
/// reply @-mentions them.
fn push_self_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _target: &mut _ = builder
        .push(" OR s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(
            " OR EXISTS (SELECT 1 FROM status_mentions sm \
               WHERE sm.status_id = s.id AND sm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(")");
}

/// The `following` arms: the target is the viewer or one of their accepted
/// follows, and a @-mention of the viewer or of an accepted follow counts as
/// being addressed to friends (Pleroma's recipients ∩ (self ∪ friends)).
fn push_following_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _target: &mut _ = builder
        .push(" OR s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.in_reply_to_account_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(") OR EXISTS (SELECT 1 FROM status_mentions sm LEFT JOIN follows mf ON mf.follower_id = ")
        .push_bind(viewer_id)
        .push(
            " AND mf.followee_id = sm.user_id AND mf.state = 'accepted' \
               WHERE sm.status_id = s.id AND (sm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" OR mf.id IS NOT NULL))");
}

/// Appends the exclusions the viewer lives by — statuses authored by
/// accounts the viewer blocked or still mutes (an unexpired mute), reblogs
/// **of** those accounts' statuses, statuses by authors on a domain-blocked
/// domain, and statuses inside a thread-muted conversation (the viewer's
/// own posts in a muted thread excepted). All four read the viewer, so an
/// anonymous request gains no condition; `with_muted` skips the whole
/// group, while [`push_visibility_clause`] — the author's side of a block —
/// keeps applying either way.
///
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
pub(super) fn push_moderation_exclusions(
    builder: &mut QueryBuilder<Postgres>,
    viewer: Option<&User>,
) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    push_author_edge_exclusion(builder, viewer_id);
    push_muted_edge_exclusion(builder, viewer_id);
    let _domain: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM user_domain_blocks udb \
               JOIN users d ON d.id = s.user_id \
               WHERE udb.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND udb.domain = d.domain)");
    let _thread: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM conversation_mutes cm \
               WHERE cm.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND cm.conversation_id = s.conversation_id AND s.user_id <> ")
        .push_bind(viewer_id)
        .push(")");
}

/// The block arm: neither the row's author nor the author of the original
/// it reblogs may be an account the viewer blocks.
fn push_author_edge_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _blocked: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.blocker_id = ")
        .push_bind(viewer_id)
        .push(" AND (b.blocked_id = s.user_id OR b.blocked_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push("))");
}

/// The mute arm, [`push_author_edge_exclusion`]'s shape with the
/// live-mutes test ([`MUTE_LIVE_ARMS`]) on: an expired mute hides nothing,
/// the same way it counts for nothing in every other read of
/// [`crate::models::mute`].
fn push_muted_edge_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _muted: &mut _ = builder
        .push(" AND NOT EXISTS (SELECT 1 FROM mutes m WHERE m.muter_id = ")
        .push_bind(viewer_id)
        .push(" AND (m.muted_id = s.user_id OR m.muted_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push(")")
        .push(MUTE_LIVE_ARMS)
        .push(")");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            block, conversation_mute, domain_block, follow,
            mute::{self, MuteOptions},
            status::{Status, Visibility},
            user::Flag,
        },
        services::timeline::{
            TimelineFilters, TimelineQuery, TimelineSource,
            test_support::{
                BASE, as_viewer, filtered, local, newest_first, page_ids, post, reblog, remote,
                remote_post, reply, timeline,
            },
        },
    };

    /// The ids of `statuses`, newest first, minus the ids in `without`:
    /// the expected page written as a difference from the seeded set.
    fn minus(statuses: &[&Status], without: &[&Status]) -> Vec<Uuid> {
        let skipped: Vec<Uuid> = without.iter().map(|status| status.id).collect();
        let mut ids: Vec<Uuid> = statuses
            .iter()
            .map(|status| status.id)
            .filter(|id| !skipped.contains(id))
            .collect();
        ids.sort_by(|a, b| b.cmp(a));
        ids
    }

    /// The `Public` timeline as `viewer` with only a `reply_visibility`
    /// filter, one full page.
    fn reply_page(viewer: Option<User>, reply_visibility: ReplyVisibility) -> TimelineQuery {
        filtered(
            viewer,
            TimelineSource::Public,
            TimelineFilters {
                reply_visibility: Some(reply_visibility),
                ..TimelineFilters::default()
            },
        )
    }

    /// Lapses `muter`'s mute of `muted` an hour into the past, the state
    /// every read in this repository already treats as no mute at all.
    async fn lapse_mute(pool: &sqlx::PgPool, muter: &User, target: &User) {
        let lapsed = sqlx::query(
            "UPDATE mutes SET expires_at = now() - interval '1 hour'
              WHERE muter_id = $1 AND muted_id = $2",
        )
        .bind(muter.id)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the mute lapses");
        assert_eq!(lapsed.rows_affected(), 1, "the fixture mute was there");
    }

    /// `reply_visibility` judges each reply by its stored target and
    /// @-mentions: `self` keeps only replies addressed to the viewer
    /// (answering them or mentioning them), `following` keeps replies whose
    /// target or mention is the viewer or an account they follow; roots and
    /// the viewer's own replies always survive, and without a viewer the
    /// filter asks nothing of anyone.
    #[tokio::test]
    async fn reply_visibility_following_and_self_filter_replies() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let _friend = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        let bob_root = post(&pool, &bob, Visibility::Public, "bob root").await;
        let alice_root = post(&pool, &alice, Visibility::Public, "alice root").await;
        let carol_root = post(&pool, &carol, Visibility::Public, "carol root").await;
        let to_followed = reply(&pool, &carol, &bob_root, &[], "carol to bob").await;
        let to_stranger = reply(&pool, &bob, &carol_root, &[], "bob to carol").await;
        let mention_alice = reply(&pool, &bob, &carol_root, &[&alice], "cc alice").await;
        let mention_bob = reply(&pool, &dave, &carol_root, &[&bob], "dave cc bob").await;
        let to_alice = reply(&pool, &dave, &alice_root, &[], "dave to alice").await;
        let alice_own = reply(&pool, &alice, &carol_root, &[], "alice to carol").await;
        let all = [
            &bob_root,
            &alice_root,
            &carol_root,
            &to_followed,
            &to_stranger,
            &mention_alice,
            &mention_bob,
            &to_alice,
            &alice_own,
        ];

        let following = page_ids(
            &pool,
            &reply_page(
                Some(as_viewer(&pool, &alice).await),
                ReplyVisibility::Following,
            ),
        )
        .await;
        assert_eq!(
            following,
            minus(&all, &[&to_stranger]),
            "`following` keeps every reply except the one to an unfollowed stranger — \
             replies to the viewer and mentions of them or of the followed bob all stay"
        );
        let selfed = page_ids(&pool, &reply_page(Some(alice), ReplyVisibility::SelfOnly)).await;
        assert_eq!(
            selfed,
            minus(&all, &[&to_followed, &to_stranger, &mention_bob]),
            "`self` keeps only replies answering or mentioning the viewer"
        );
        let anonymous = page_ids(&pool, &reply_page(None, ReplyVisibility::SelfOnly)).await;
        assert_eq!(
            anonymous,
            newest_first(&all),
            "with no viewer there is no one to reply to or for, so the filter appends nothing"
        );
    }

    /// Blocks, unexpired mutes and domain blocks pull an author's rows out
    /// of every timeline the viewer reads — and so do reblogs **of** those
    /// rows — while a lapsed mute counts as absent and `with_muted` waves
    /// the whole group back through. None of it touches an anonymous page.
    #[tokio::test]
    async fn blocked_muted_and_domain_blocked_authors_leave_the_page() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let eve = local(&pool, "eve").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let carol_block = block::block(&pool, BASE, &alice, &carol)
            .await
            .expect("alice blocks carol");
        assert_eq!(
            carol_block.blocked_id, carol.id,
            "the fixture block is on carol"
        );
        let dave_mute = mute::mute(&pool, &alice, &dave, MuteOptions::default())
            .await
            .expect("alice mutes dave");
        assert_eq!(dave_mute.muted_id, dave.id, "the fixture mute is on dave");
        let eve_mute = mute::mute(&pool, &alice, &eve, MuteOptions::default())
            .await
            .expect("alice mutes eve");
        assert_eq!(eve_mute.muted_id, eve.id, "the second mute is on eve");
        lapse_mute(&pool, &alice, &eve).await;
        domain_block::block_domain(&pool, alice.id, "remote.example")
            .await
            .expect("alice domain-blocks remote.example");
        let carol_post = post(&pool, &carol, Visibility::Public, "blocked carol").await;
        let dave_post = post(&pool, &dave, Visibility::Public, "muted dave").await;
        let eve_post = post(&pool, &eve, Visibility::Public, "lapsed eve").await;
        let mallory_post = remote_post(&pool, &mallory, "mal-public", &[]).await;
        let bob_post = post(&pool, &bob, Visibility::Public, "plain bob").await;
        let bob_reb_carol = reblog(&pool, &bob, &carol_post).await;
        let bob_reb_dave = reblog(&pool, &bob, &dave_post).await;

        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&bob_post, &eve_post]),
            "blocked and muted authors are out, boosts of their rows with them, a domain \
             block takes its remote rows, and a lapsed mute counts as absent"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert_eq!(
            anonymous,
            newest_first(&[
                &carol_post,
                &dave_post,
                &eve_post,
                &mallory_post,
                &bob_post,
                &bob_reb_carol,
                &bob_reb_dave,
            ]),
            "none of this is an anonymous request's problem"
        );
        let with_muted = page_ids(
            &pool,
            &filtered(
                Some(alice),
                TimelineSource::Public,
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            with_muted, anonymous,
            "with_muted brings the whole group back, exactly the unfiltered page"
        );
    }

    /// A thread mute takes the muted conversation's foreign rows out of the
    /// page — the root and every reply in it, boosts of its rows included —
    /// but never the viewer's own posts in that thread; another account
    /// reads unaffected, and `with_muted` overrides.
    #[tokio::test]
    async fn thread_mutes_hide_the_thread_except_the_viewers_own_posts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let root = post(&pool, &bob, Visibility::Public, "muted thread root").await;
        let alice_in_thread = reply(&pool, &alice, &root, &[], "alice answers in it").await;
        let carol_in_thread = reply(&pool, &carol, &root, &[], "carol answers in it").await;
        let dave_elsewhere = post(&pool, &dave, Visibility::Public, "another thread").await;
        let boost_of_thread = reblog(&pool, &dave, &alice_in_thread).await;
        conversation_mute::mute(&pool, alice.id, root.conversation_id)
            .await
            .expect("alice mutes the thread");

        let whole_thread = newest_first(&[
            &root,
            &alice_in_thread,
            &carol_in_thread,
            &dave_elsewhere,
            &boost_of_thread,
        ]);
        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&alice_in_thread, &dave_elsewhere]),
            "the foreign root, the foreign reply and even a boost of the viewer's own row \
             are all out of the muted thread"
        );
        let for_bob = page_ids(&pool, &timeline(Some(bob), TimelineSource::Public)).await;
        assert_eq!(
            for_bob, whole_thread,
            "the mute is alice's, nobody else reads through it"
        );
        let for_alice_unmuted = page_ids(
            &pool,
            &filtered(
                Some(alice),
                TimelineSource::Public,
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            for_alice_unmuted, whole_thread,
            "with_muted restores the muted thread too"
        );
        assert!(
            !for_alice.contains(&carol_in_thread.id) && !for_alice.contains(&boost_of_thread.id),
            "the muted thread keeps nothing of carol's or dave's for alice"
        );
    }
}
