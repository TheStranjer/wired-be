//! The `List` source predicate of the timeline statement: the membership
//! arm and the `replies_policy` arms that narrow a list's replies.

use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::models::user::User;

use super::ACCEPTED_FOLLOW_STATE;

/// Appends the `List` predicate: rows whose own author is a member of
/// `list_id`'s `list_members` — membership alone, no follow read — reblog
/// rows by members included (their author is the member who boosted; the
/// original's author needs no membership to ride a member's boost).
/// [`push_visibility_clause`] and [`crate::models::status`] then judge
/// every member row like any other read's, and [`TimelineQuery`]'s
/// moderation arms hide members the viewer put away.
///
/// Replies are left to [`push_list_replies_policy`], which reads the same
/// list's stored `replies_policy` in the statement — the one source for
/// every list, so no second round trip decides the page.
///
/// Reference: Pleroma's `Pleroma.Web.TimelineFilter.ListTimeline` behind
/// the list action of `timeline_controller.ex` (the ticket body's
/// `list`/`followed`/`none` spellings are authoritative).
///
/// [`push_visibility_clause`]: crate::models::status_visibility::push_visibility_clause
/// [`TimelineQuery`]: super::TimelineQuery
pub(super) fn push_list(
    builder: &mut QueryBuilder<Postgres>,
    list_id: Uuid,
    viewer: Option<&User>,
) {
    let _member: &mut _ = builder
        .push(" AND EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ")
        .push_bind(list_id)
        .push(" AND lm.user_id = s.user_id)");
    push_list_replies_policy(builder, list_id, viewer);
}

/// Opens the `replies_policy` group and runs its arms: a non-reply always
/// survives; under `none` no reply does; otherwise a reply survives when
/// [`push_list_member_replies`] answers it to a member, or — with a viewer
/// — [`push_list_self_arms`] answers it to the viewer and, under
/// `followed`, [`push_list_followed_replies`] answers it to a viewer's
/// accepted follow. Without a viewer (a page [`fetch`](super::TimelineQuery::fetch)
/// already answers empty) only the member arm reads.
fn push_list_replies_policy(
    builder: &mut QueryBuilder<Postgres>,
    list_id: Uuid,
    viewer: Option<&User>,
) {
    let _open: &mut _ = builder
        .push(" AND (s.in_reply_to_id IS NULL OR ((SELECT lp.replies_policy FROM lists lp WHERE lp.id = ")
        .push_bind(list_id)
        .push(") <> 'none' AND (");
    push_list_member_replies(builder, list_id);
    if let Some(viewer_id) = viewer.map(|user| user.id) {
        push_list_self_arms(builder, viewer_id);
        push_list_followed_replies(builder, list_id, viewer_id);
    }
    let _close: &mut _ = builder.push(")))");
}

/// The `list` arm — shared by every live policy: a reply addressed to a
/// member survives, whether it answers them (`in_reply_to_account_id`) or
/// @-mentions them (the same reading of "addressed to" as
/// [`push_reply_visibility`] gives).
///
/// [`push_reply_visibility`]: super::visibility_arms::push_reply_visibility
fn push_list_member_replies(builder: &mut QueryBuilder<Postgres>, list_id: Uuid) {
    let _member: &mut _ = builder
        .push(" EXISTS (SELECT 1 FROM list_members lm WHERE lm.list_id = ")
        .push_bind(list_id)
        .push(" AND (lm.user_id = s.in_reply_to_account_id OR EXISTS (SELECT 1 FROM status_mentions sm WHERE sm.status_id = s.id AND sm.user_id = lm.user_id)))");
}

/// The viewer's own arm, live under both `list` and `followed`: a reply
/// answering or @-mentioning the viewer survives on the strength of being
/// addressed to the reader, whoever else the policy counts (Mastodon reads
/// the current account next to the members in `list` and next to the
/// followed accounts in `followed`).
fn push_list_self_arms(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _self_arm: &mut _ = builder
        .push(" OR (s.in_reply_to_account_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM status_mentions sv WHERE sv.status_id = s.id AND sv.user_id = ")
        .push_bind(viewer_id)
        .push("))");
}

/// The `followed` policy's extra arm: a reply answering or @-mentioning an
/// account the viewer has an **accepted** follow of survives too — the
/// same arms [`push_following_arms`] builds for [`ReplyVisibility::Following`],
/// hung behind a read of this list's own policy so the `list` policy never
/// sees them.
///
/// [`push_following_arms`]: super::visibility_arms
/// [`ReplyVisibility::Following`]: super::ReplyVisibility::Following
fn push_list_followed_replies(
    builder: &mut QueryBuilder<Postgres>,
    list_id: Uuid,
    viewer_id: Uuid,
) {
    let _followed: &mut _ = builder
        .push(" OR ((SELECT fp.replies_policy FROM lists fp WHERE fp.id = ")
        .push_bind(list_id)
        .push(") = 'followed' AND (EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.in_reply_to_account_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(") OR EXISTS (SELECT 1 FROM status_mentions sf JOIN follows mf ON mf.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND mf.followee_id = sf.user_id AND mf.state = 'accepted' WHERE sf.status_id = s.id)))");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{
            follow,
            list::{self, List, ListChanges},
            mute::{self, MuteOptions},
            status::Visibility,
            user::Flag,
        },
        services::timeline::{
            TimelineFilters, TimelineSource,
            test_support::{
                BASE, add_member, as_viewer, attach_media, filtered, local, newest_first,
                owned_list, page_ids, post, reblog, reply, timeline,
            },
        },
    };

    /// Rewrites the list's stored replies policy, the same write the API's
    /// `PUT` performs.
    async fn set_policy(pool: &sqlx::PgPool, owner: &User, list: &List, policy: &str) {
        let rewritten = list::update(
            pool,
            owner.id,
            list.id,
            &ListChanges {
                title: None,
                replies_policy: Some(policy.to_owned()),
                exclusive: None,
            },
        )
        .await
        .expect("the policy update succeeds")
        .expect("the fixture list exists");
        assert_eq!(
            rewritten.replies_policy, policy,
            "the stored policy now reads `{policy}`"
        );
    }

    /// `List` answers rows authored by members — no follow read: a followed
    /// non-member stays out, a member's `private` post stays out only for
    /// the visibility clause's reason, a member's boost rides in (its
    /// author is the member), and the list's own owner is off the page
    /// unless they added themselves. An anonymous list fetches the empty
    /// page without a query.
    #[tokio::test]
    async fn list_answers_member_posts_without_follows_including_member_reblogs() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let carol_follow = follow::follow(&pool, BASE, &alice, &carol)
            .await
            .expect("alice follows carol");
        assert_eq!(
            carol_follow.state,
            follow::FollowState::Accepted,
            "the fixture follow is granted"
        );
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        let bob_public = post(&pool, &bob, Visibility::Public, "member public").await;
        let _bob_private = post(&pool, &bob, Visibility::Private, "member private").await;
        let carol_public = post(&pool, &carol, Visibility::Public, "followed non-member").await;
        let dave_public = post(&pool, &dave, Visibility::Public, "boosted original").await;
        let alice_own = post(&pool, &alice, Visibility::Public, "owner's own post").await;
        let bob_reblog = reblog(&pool, &bob, &dave_public).await;

        let page = page_ids(
            &pool,
            &timeline(Some(alice), TimelineSource::List { list_id: circle.id }),
        )
        .await;
        assert_eq!(
            page,
            newest_first(&[&bob_public, &bob_reblog]),
            "the member's post and the member's boost ride the list; the followed \
             non-member's post, the owner's own post and the un-followed member's private \
             row (by the visibility clause) do not"
        );
        assert!(
            !page.contains(&carol_public.id) && !page.contains(&alice_own.id),
            "membership, not follow or ownership, is what fills the list"
        );
        let anonymous = page_ids(
            &pool,
            &timeline(None, TimelineSource::List { list_id: circle.id }),
        )
        .await;
        assert!(
            anonymous.is_empty(),
            "an anonymous list page is the empty page"
        );
    }

    /// The list page lives by the standard filters and exclusions like
    /// every other timeline: the viewer's mute clears the member off it,
    /// `with_muted` brings the rows back, and `only_media` narrows the
    /// kept rows to the attached ones.
    #[tokio::test]
    async fn list_pages_honour_mutes_and_only_media() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        let plain = post(&pool, &bob, Visibility::Public, "plain member").await;
        let pictured = post(&pool, &bob, Visibility::Public, "pictured member").await;
        attach_media(&pool, pictured.id).await;
        let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
            .await
            .expect("alice mutes bob");
        assert_eq!(muted.muted_id, bob.id, "the mute is live");

        let list_source = || TimelineSource::List { list_id: circle.id };
        let hidden = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), list_source()),
        )
        .await;
        assert!(hidden.is_empty(), "the muted member leaves the list page");
        let shown = page_ids(
            &pool,
            &filtered(
                Some(as_viewer(&pool, &alice).await),
                list_source(),
                TimelineFilters {
                    with_muted: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            shown,
            newest_first(&[&plain, &pictured]),
            "with_muted lifts the viewer's own exclusion"
        );
        let media = page_ids(
            &pool,
            &filtered(
                Some(alice),
                list_source(),
                TimelineFilters {
                    with_muted: Flag::new(true),
                    only_media: Flag::new(true),
                    ..TimelineFilters::default()
                },
            ),
        )
        .await;
        assert_eq!(
            media,
            newest_first(&[&pictured]),
            "only_media keeps the attached row"
        );
    }

    /// The `replies_policy` matrix of one list: `list` keeps only replies
    /// answering or @-mentioning a member or the viewer, `followed` also
    /// those answering or @-mentioning an accepted follow, and `none`
    /// drops every reply — a root never falls to the policy, and the
    /// non-members' roots that the replies answer stay off the page
    /// because membership, not the thread, fills it.
    #[tokio::test]
    async fn list_replies_policy_selects_whom_a_reply_may_answer() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let eve = local(&pool, "eve").await;
        let dave_follow = follow::follow(&pool, BASE, &alice, &dave)
            .await
            .expect("alice follows dave");
        assert_eq!(
            dave_follow.state,
            follow::FollowState::Accepted,
            "the fixture follow is granted"
        );
        let circle = owned_list(&pool, &alice, "list", false).await;
        add_member(&pool, &circle, &bob).await;
        add_member(&pool, &circle, &carol).await;
        let carol_root = post(&pool, &carol, Visibility::Public, "member root").await;
        let dave_root = post(&pool, &dave, Visibility::Public, "followed root").await;
        let eve_root = post(&pool, &eve, Visibility::Public, "stranger root").await;
        let alice_root = post(&pool, &alice, Visibility::Public, "viewer root").await;
        let bob_root = post(&pool, &bob, Visibility::Public, "member root post").await;
        let to_member = reply(&pool, &bob, &carol_root, &[], "bob to carol").await;
        let to_followed = reply(&pool, &bob, &dave_root, &[], "bob to dave").await;
        let _to_stranger = reply(&pool, &bob, &eve_root, &[], "bob to eve").await;
        let to_viewer = reply(&pool, &bob, &alice_root, &[], "bob to alice").await;
        let mentions_member = reply(&pool, &bob, &eve_root, &[&carol], "bob cc carol").await;
        let mentions_followed = reply(&pool, &bob, &eve_root, &[&dave], "bob cc dave").await;

        let circle_id = circle.id;
        let page = |viewer: User| {
            filtered(
                Some(viewer),
                TimelineSource::List { list_id: circle_id },
                TimelineFilters::default(),
            )
        };

        let under_list = page_ids(&pool, &page(as_viewer(&pool, &alice).await)).await;
        assert_eq!(
            under_list,
            newest_first(&[
                &bob_root,
                &carol_root,
                &to_member,
                &to_viewer,
                &mentions_member
            ]),
            "`list` keeps roots and the replies answering or mentioning a member or the viewer"
        );

        set_policy(&pool, &alice, &circle, "followed").await;
        let under_followed = page_ids(&pool, &page(as_viewer(&pool, &alice).await)).await;
        assert_eq!(
            under_followed,
            newest_first(&[
                &bob_root,
                &carol_root,
                &to_member,
                &to_viewer,
                &mentions_member,
                &to_followed,
                &mentions_followed
            ]),
            "`followed` adds the replies answering or mentioning an accepted follow"
        );

        set_policy(&pool, &alice, &circle, "none").await;
        let under_none = page_ids(&pool, &page(alice)).await;
        assert_eq!(
            under_none,
            newest_first(&[&bob_root, &carol_root]),
            "`none` is no replies at all"
        );
    }
}
