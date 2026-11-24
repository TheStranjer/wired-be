//! The viewer-relative arms of the timeline statement: the `reply_visibility`
//! filter's arms, the block exclusions (blocks, domain blocks) and the mute
//! exclusions (mutes, thread mutes) the viewer lives by.

use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::models::{block::push_not_blocked, mute::push_not_muted, user::User};

use super::{ACCEPTED_FOLLOW_STATE, REBLOG_AUTHOR_SQL, ReplyVisibility, TimelineSource};

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

/// Appends the block exclusions — statuses authored by accounts the viewer
/// blocked, reblogs **of** those accounts' statuses, and statuses by authors
/// on a domain the viewer blocked (a reblog of such an author's status
/// included). They read the viewer, so an anonymous
/// request gains no condition. Applied on every timeline, `with_muted` or
/// not (Pleroma's `restrict_blocked`); [`push_visibility_clause`] — the
/// author's side of a block — applies alongside.
///
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
pub(super) fn push_block_exclusions(
    builder: &mut QueryBuilder<Postgres>,
    source: &TimelineSource,
    viewer: Option<&User>,
) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    push_author_edge_exclusion(builder, viewer_id);
    let own_account =
        matches!(source, TimelineSource::Account { user_id } if *user_id == viewer_id);
    if !own_account {
        push_addressee_exclusion(builder, viewer_id);
    }
    let _domain: &mut _ = builder
        .push(
            " AND NOT EXISTS (SELECT 1 FROM user_domain_blocks udb \
               JOIN users d ON d.id IN (s.user_id, ",
        )
        .push(REBLOG_AUTHOR_SQL)
        .push(") WHERE udb.user_id = ")
        .push_bind(viewer_id)
        .push(" AND udb.domain = d.domain)");
}

/// Appends the mute exclusions — statuses authored by accounts the viewer
/// still mutes (an unexpired mute), reblogs **of** those accounts' statuses,
/// and statuses inside a thread-muted conversation (the viewer's own posts in
/// a muted thread excepted). They read the viewer, so an anonymous request
/// gains no condition; `with_muted` skips this group (Pleroma's
/// `restrict_muted`) but never [`push_block_exclusions`].
pub(super) fn push_mute_exclusions(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        return;
    };
    push_muted_edge_exclusion(builder, viewer_id);
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
    push_not_blocked(builder, viewer_id, &["s.user_id", REBLOG_AUTHOR_SQL]);
}

/// The addressee arm: unless the viewer wrote the row, it may not reply to
/// an account the viewer blocks, nor (for a reblog, the original it boosts)
/// @-mention one — Mastodon's mention/reply-target filter and Pleroma's
/// blocked-recipient drop.
fn push_addressee_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _addressed: &mut _ = builder
        .push(" AND (s.user_id = ")
        .push_bind(viewer_id)
        .push(" OR (NOT EXISTS (SELECT 1 FROM blocks b2 WHERE b2.blocker_id = ")
        .push_bind(viewer_id)
        .push(
            " AND b2.blocked_id = s.in_reply_to_account_id) \
               AND NOT EXISTS (SELECT 1 FROM status_mentions smb \
               JOIN blocks b3 ON b3.blocked_id = smb.user_id AND b3.blocker_id = ",
        )
        .push_bind(viewer_id)
        .push(" WHERE smb.status_id = COALESCE(s.reblog_of_id, s.id))))");
}

/// The mute arm, [`push_author_edge_exclusion`]'s shape with the
/// live-mutes test on: an expired mute hides nothing, the same way it counts
/// for nothing in every other read of [`crate::models::mute`].
fn push_muted_edge_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    push_not_muted(builder, viewer_id, &["s.user_id", REBLOG_AUTHOR_SQL]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            block, conversation_mute, domain_block, follow,
            mute::{self, MuteOptions},
            status::{self, NewStatus, Status, Visibility},
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
    /// rows — while a lapsed mute counts as absent. None of it touches an
    /// anonymous page; `with_muted` is covered by
    /// `with_muted_never_lifts_blocks_or_domain_blocks`.
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
    }

    /// A domain block also hides a followed local friend's boost of a post
    /// from the blocked domain, on Home and Public, while the friend's own
    /// post and a boost of a post from another domain stay.
    #[tokio::test]
    async fn domain_block_hides_boosts_of_blocked_domain_posts() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = remote(&pool, "carol", "remote.example").await;
        let erin = remote(&pool, "erin", "other.example").await;
        let _follow = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        let _accepted = sqlx::query("UPDATE follows SET state = 'accepted'")
            .execute(&pool)
            .await
            .expect("the follow is accepted");
        domain_block::block_domain(&pool, alice.id, "remote.example")
            .await
            .expect("alice domain-blocks remote.example");
        let carol_post = remote_post(&pool, &carol, "carol-public", &[]).await;
        let erin_post = remote_post(&pool, &erin, "erin-public", &[]).await;
        let bob_post = post(&pool, &bob, Visibility::Public, "plain bob").await;
        let boost_blocked = reblog(&pool, &bob, &carol_post).await;
        let boost_other = reblog(&pool, &bob, &erin_post).await;

        let home = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Home),
        )
        .await;
        assert_eq!(
            home,
            newest_first(&[&bob_post, &boost_other]),
            "bob's boost of the blocked domain's post is out of Home"
        );
        let public = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            public,
            newest_first(&[&erin_post, &bob_post, &boost_other]),
            "and out of Public, where the other domain's post and boost stay"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert!(
            anonymous.contains(&boost_blocked.id),
            "an anonymous request has no domain blocks"
        );
    }

    /// What [`addressing_world`] seeds: alice follows bob and blocks mallory.
    struct AddressingWorld {
        /// The viewer, who follows `bob` and blocks mallory.
        alice: User,
        /// The followed author of every row that addresses the blocked account.
        bob: User,
        /// A stranger's public root, visible on Public only.
        carol_root: Status,
        /// Bob's post that has nothing to do with the blocked account.
        unrelated: Status,
        /// Alice's own reply to (and mention of) the blocked account.
        alice_to_mallory: Status,
        /// Bob's reply to, @-mention of, and boost of a mention of mallory.
        addressing: [Status; 4],
    }

    /// Seeds [`AddressingWorld`]: bob replies to mallory, @-mentions her in a
    /// reply and in a root post, and boosts that root; he also posts something
    /// unrelated, and alice replies to mallory herself.
    async fn addressing_world(pool: &sqlx::PgPool) -> AddressingWorld {
        let alice = local(pool, "alice").await;
        let bob = local(pool, "bob").await;
        let carol = local(pool, "carol").await;
        let mallory = local(pool, "mallory").await;
        let _follow = follow::follow(pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        let _accepted = sqlx::query("UPDATE follows SET state = 'accepted'")
            .execute(pool)
            .await
            .expect("the follow is accepted");
        let _blocked = block::block(pool, BASE, &alice, &mallory)
            .await
            .expect("alice blocks mallory");
        let carol_root = post(pool, &carol, Visibility::Public, "carol root").await;
        let mallory_root = post(pool, &mallory, Visibility::Public, "mallory root").await;
        let to_mallory = reply(pool, &bob, &mallory_root, &[], "bob to mallory").await;
        let cc_mallory = reply(pool, &bob, &carol_root, &[&mallory], "bob cc mallory").await;
        let new = NewStatus {
            mentioned_user_ids: vec![mallory.id],
            ..NewStatus::local(&bob, "hi mallory")
        };
        let mentions_mallory = status::insert(pool, BASE, &new)
            .await
            .expect("the mention fixture inserts");
        let boost_of_mention = reblog(pool, &bob, &mentions_mallory).await;
        let unrelated = post(pool, &bob, Visibility::Public, "bob unrelated").await;
        let alice_to_mallory = reply(pool, &alice, &mallory_root, &[&mallory], "alice reply").await;
        AddressingWorld {
            alice,
            bob,
            carol_root,
            unrelated,
            alice_to_mallory,
            addressing: [to_mallory, cc_mallory, mentions_mallory, boost_of_mention],
        }
    }

    /// The ids `source` answers `viewer`, newest first.
    async fn page_as(pool: &sqlx::PgPool, viewer: &User, source: TimelineSource) -> Vec<Uuid> {
        page_ids(pool, &timeline(Some(as_viewer(pool, viewer).await), source)).await
    }

    /// A followed author's reply to a blocked account, their post
    /// @-mentioning one and their boost of a post mentioning one all leave
    /// Home and Public; their unrelated post stays, and so does the viewer's
    /// own reply to the blocked account.
    #[tokio::test]
    async fn replies_to_and_mentions_of_blocked_accounts_leave_home_and_public() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let world = addressing_world(&pool).await;

        let home = page_as(&pool, &world.alice, TimelineSource::Home).await;
        assert_eq!(
            home,
            newest_first(&[&world.unrelated, &world.alice_to_mallory]),
            "bob's replies, mentions and boost of the blocked account are out of Home; \
             his unrelated post and alice's own reply stay"
        );
        let public = page_as(&pool, &world.alice, TimelineSource::Public).await;
        assert_eq!(
            public,
            newest_first(&[&world.carol_root, &world.unrelated, &world.alice_to_mallory]),
            "and out of Public, where a stranger's root stays"
        );
    }

    /// The viewer's own `Account` page keeps their reply to the blocked
    /// account, another account's page hides what addresses it, and an
    /// anonymous page hides nothing.
    #[tokio::test]
    async fn account_pages_honour_the_addressee_exclusion() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let world = addressing_world(&pool).await;

        let own_source = TimelineSource::Account {
            user_id: world.alice.id,
        };
        assert_eq!(
            page_as(&pool, &world.alice, own_source).await,
            newest_first(&[&world.alice_to_mallory]),
            "the viewer's own account page keeps their reply to the blocked account"
        );
        let bob_source = TimelineSource::Account {
            user_id: world.bob.id,
        };
        assert_eq!(
            page_as(&pool, &world.alice, bob_source.clone()).await,
            newest_first(&[&world.unrelated]),
            "another account's page hides what addresses the blocked account"
        );
        let anonymous = page_ids(&pool, &timeline(None, bob_source)).await;
        let [to_mallory, cc_mallory, mentions_mallory, boost_of_mention] = &world.addressing;
        assert_eq!(
            anonymous,
            newest_first(&[
                to_mallory,
                cc_mallory,
                mentions_mallory,
                boost_of_mention,
                &world.unrelated
            ]),
            "an anonymous request has no blocks to honour"
        );
    }

    /// Seeds alice's Home and Public world: she follows `bob` (who boosts
    /// the blocked `carol`), `carol` (blocked, yet a follow row survives),
    /// `dave` (muted); `mallory` is a remote author on a blocked domain.
    /// Answers `(alice, [carol_post, bob_reb_carol, mallory_post, dave_post])`.
    async fn moderated_world(pool: &sqlx::PgPool) -> (User, [Status; 4]) {
        let alice = local(pool, "alice").await;
        let bob = local(pool, "bob").await;
        let carol = local(pool, "carol").await;
        let dave = local(pool, "dave").await;
        let mallory = remote(pool, "mallory", "remote.example").await;
        for followee in [&bob, &carol, &dave] {
            let _follow = follow::follow(pool, BASE, &alice, followee)
                .await
                .expect("alice follows");
        }
        let _accepted = sqlx::query("UPDATE follows SET state = 'accepted'")
            .execute(pool)
            .await
            .expect("the follows are accepted");
        let _blocked = block::block(pool, BASE, &alice, &carol)
            .await
            .expect("alice blocks carol");
        let _restored = sqlx::query(
            "INSERT INTO follows (id, follower_id, followee_id, state)
             VALUES (gen_random_uuid(), $1, $2, 'accepted')",
        )
        .bind(alice.id)
        .bind(carol.id)
        .execute(pool)
        .await
        .expect("a follow of the blocked author is on file");
        let _muted = mute::mute(pool, &alice, &dave, MuteOptions::default())
            .await
            .expect("alice mutes dave");
        domain_block::block_domain(pool, alice.id, "remote.example")
            .await
            .expect("alice domain-blocks remote.example");
        let carol_post = post(pool, &carol, Visibility::Public, "blocked carol").await;
        let boost = reblog(pool, &bob, &carol_post).await;
        let mallory_post = remote_post(pool, &mallory, "mal-public", &[]).await;
        let dave_post = post(pool, &dave, Visibility::Public, "muted dave").await;
        (alice, [carol_post, boost, mallory_post, dave_post])
    }

    /// `with_muted` lifts the mute and nothing else: a blocked followed
    /// author's post, a boost of a blocked author's post and a domain-blocked
    /// author's post stay off Home and Public either way, while the muted
    /// author's post returns; without `with_muted` all four are absent.
    #[tokio::test]
    async fn with_muted_never_lifts_blocks_or_domain_blocks() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let (alice, [carol_post, boost, mallory_post, dave_post]) = moderated_world(&pool).await;

        for source in [TimelineSource::Home, TimelineSource::Public] {
            let with_muted = filtered(
                Some(as_viewer(&pool, &alice).await),
                source.clone(),
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            );
            assert_eq!(
                page_ids(&pool, &with_muted).await,
                newest_first(&[&dave_post]),
                "with_muted keeps only the muted author's post of the four"
            );
            let plain = timeline(Some(as_viewer(&pool, &alice).await), source);
            let page = page_ids(&pool, &plain).await;
            let hidden = [&carol_post, &boost, &mallory_post, &dave_post];
            assert!(
                hidden.iter().all(|status| !page.contains(&status.id)),
                "without with_muted none of the four shows"
            );
        }
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
