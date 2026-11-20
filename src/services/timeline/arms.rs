//! The source predicates of the timeline statement: the ` AND ` group that
//! decides which rows a [`TimelineSource`] is made of, for the home, public,
//! hashtag, direct and account sources.

use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::models::user::User;

use super::{
    ACCEPTED_FOLLOW_STATE, REBLOG_AUTHOR_SQL, TAG_JOIN_SQL, TimelineSource, list_arms::push_list,
};

/// Appends the [`TimelineSource`] predicate — the ` AND ` group that decides
/// which rows the timeline is made of (the visibility it must carry lives
/// here too, where the source narrows it; see [`TimelineSource`]'s docs).
pub(super) fn push_source(
    builder: &mut QueryBuilder<Postgres>,
    source: &TimelineSource,
    viewer: Option<&User>,
) {
    match source {
        TimelineSource::Home => push_home(builder, viewer),
        TimelineSource::Public => push_public(builder, viewer),
        TimelineSource::Hashtag { any, all, none } => push_hashtag(builder, any, all, none),
        TimelineSource::Direct => {
            let _dm: &mut _ = builder.push(" AND s.visibility = 'direct'");
        }
        TimelineSource::Account { user_id } => {
            let _authored: &mut _ = builder.push(" AND s.user_id = ").push_bind(*user_id);
        }
        TimelineSource::List { list_id } => push_list(builder, *list_id, viewer),
    }
}

/// Appends the `Home` predicate: the viewer's own rows (their reblogs
/// included), plus rows by accounts the viewer has an **accepted** follow
/// of — and a reblog row only while that follow shows reblogs — and then
/// [`push_exclusive_list_exclusion`] takes the members of the viewer's
/// `exclusive` lists back out. Without a viewer the source is already
/// answered empty by [`TimelineQuery::fetch`]; the `AND false` arm is the
/// belt to that brace, so the predicate can never widen to "everything"
/// when the caller forgot it.
///
/// [`TimelineQuery::fetch`]: super::TimelineQuery::fetch
fn push_home(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let Some(viewer_id) = viewer.map(|user| user.id) else {
        let _nothing: &mut _ = builder.push(" AND false");
        return;
    };
    let _followed: &mut _ = builder
        .push(" AND (s.user_id = ")
        .push_bind(viewer_id)
        .push(" OR EXISTS (SELECT 1 FROM follows f WHERE f.follower_id = ")
        .push_bind(viewer_id)
        .push(" AND f.followee_id = s.user_id")
        .push(ACCEPTED_FOLLOW_STATE)
        .push(" AND (s.reblog_of_id IS NULL OR f.show_reblogs)))");
    push_exclusive_list_exclusion(builder, viewer_id);
}

/// Appends the `exclusive` list exclusion (Mastodon's `FeedManager` rule,
/// which Pleroma reaches by addressing an exclusive list's posts away from
/// the home timeline): neither a row of a member of one of the viewer's
/// `exclusive` lists nor a reblog **of** a member's row may ride the home
/// timeline — the member's own boosts of other accounts go with them, the
/// way [`push_author_edge_exclusion`] sweeps blocked authors' whole
/// presence — while the viewer's own rows were never in question: an
/// account that lists itself keeps its place in its own home.
///
/// [`push_author_edge_exclusion`]: super::visibility_arms::push_author_edge_exclusion
fn push_exclusive_list_exclusion(builder: &mut QueryBuilder<Postgres>, viewer_id: Uuid) {
    let _exclusive: &mut _ = builder
        .push(" AND (s.user_id = ")
        .push_bind(viewer_id)
        .push(
            " OR NOT EXISTS (SELECT 1 FROM list_members lm \
               JOIN lists l ON l.id = lm.list_id \
               WHERE l.user_id = ",
        )
        .push_bind(viewer_id)
        .push(" AND l.exclusive AND (lm.user_id = s.user_id OR lm.user_id = ")
        .push(REBLOG_AUTHOR_SQL)
        .push(")))");
}

/// Appends the `Public` predicate: `public` rows, and `local` rows as well
/// for an authenticated viewer this instance owns (`local` never federates,
/// so a remote mirror or an anonymous request never sees one; and
/// `unlisted`, though the visibility clause admits it to everyone, is not
/// part of *the public timeline*).
fn push_public(builder: &mut QueryBuilder<Postgres>, viewer: Option<&User>) {
    let _public: &mut _ = builder.push(" AND (s.visibility = 'public'");
    if viewer.is_some_and(|user| user.local.get()) {
        let _local: &mut _ = builder.push(" OR s.visibility = 'local'");
    }
    let _close: &mut _ = builder.push(")");
}

/// Appends the `Hashtag` predicate: `public` rows only, joined through the
/// tag conditions — `any` as one array bind, `all` and `none` one bound
/// name per EXISTS arm (an empty list adds nothing).
fn push_hashtag(
    builder: &mut QueryBuilder<Postgres>,
    any: &[String],
    all: &[String],
    none: &[String],
) {
    let _public: &mut _ = builder.push(" AND s.visibility = 'public'");
    if !any.is_empty() {
        let _any: &mut _ = builder
            .push(" AND EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ANY(")
            .push_bind(any.to_vec())
            .push("))");
    }
    for name in all {
        let _all: &mut _ = builder
            .push(" AND EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ")
            .push_bind(name.clone())
            .push(")");
    }
    for name in none {
        let _none: &mut _ = builder
            .push(" AND NOT EXISTS (")
            .push(TAG_JOIN_SQL)
            .push(" AND h.name = ")
            .push_bind(name.clone())
            .push(")");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{follow, status::Visibility},
        services::timeline::test_support::{
            BASE, as_viewer, direct, local, newest_first, page_ids, post, reblog, remote,
            remote_post, reply, timeline,
        },
    };

    /// The `Public` timeline is `public` for everyone — never `unlisted`,
    /// `private` or `direct`, not even the viewer's own — while `local`
    /// posts appear exactly for an authenticated viewer this instance owns.
    #[tokio::test]
    async fn public_shows_public_to_everyone_and_local_only_to_local_viewers() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let public = post(&pool, &alice, Visibility::Public, "public").await;
        let unlisted = post(&pool, &alice, Visibility::Unlisted, "unlisted").await;
        let instance_local = post(&pool, &alice, Visibility::Local, "local only").await;
        let private = post(&pool, &alice, Visibility::Private, "private").await;
        let dm = post(&pool, &alice, Visibility::Direct, "unaddressed dm").await;
        let remote_public = remote_post(&pool, &mallory, "remote-public", &[]).await;

        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Public)).await;
        assert_eq!(
            anonymous,
            newest_first(&[&public, &remote_public]),
            "the anonymous public timeline is public rows only"
        );
        let for_alice = page_ids(
            &pool,
            &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Public),
        )
        .await;
        assert_eq!(
            for_alice,
            newest_first(&[&public, &remote_public, &instance_local]),
            "a local viewer's public timeline gains the `local` row and nothing else"
        );
        assert!(
            !for_alice.contains(&unlisted.id)
                && !for_alice.contains(&private.id)
                && !for_alice.contains(&dm.id),
            "unlisted, private and direct are off the public timeline even for their author"
        );
        let for_mallory = page_ids(&pool, &timeline(Some(mallory), TimelineSource::Public)).await;
        assert_eq!(
            for_mallory,
            newest_first(&[&public, &remote_public]),
            "a remote viewer never sees a `local` row"
        );
    }

    /// The `Hashtag` source keeps `public` rows only and reads `any` as a
    /// union, `all` as an intersection and `none` as a subtraction over the
    /// stored (lowercase, `#`-less) tag names.
    #[tokio::test]
    async fn hashtag_tags_select_public_rows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let mallory = remote(&pool, "mallory", "remote.example").await;
        let foo_one = post(&pool, &alice, Visibility::Public, "#foo first").await;
        let bar_one = post(&pool, &alice, Visibility::Public, "#bar only").await;
        let both = post(&pool, &alice, Visibility::Public, "#foo #bar both").await;
        let foo_remote = remote_post(&pool, &mallory, "tagged-remote", &["foo"]).await;
        let foo_unlisted = post(&pool, &alice, Visibility::Unlisted, "#foo quiet").await;
        let foo_local = post(&pool, &alice, Visibility::Local, "#foo instance").await;
        let tags = |any: &[&str], all: &[&str], none: &[&str]| TimelineSource::Hashtag {
            any: any.iter().map(|name| (*name).to_owned()).collect(),
            all: all.iter().map(|name| (*name).to_owned()).collect(),
            none: none.iter().map(|name| (*name).to_owned()).collect(),
        };

        let for_any_foo = page_ids(&pool, &timeline(None, tags(&["foo"], &[], &[]))).await;
        assert_eq!(
            for_any_foo,
            newest_first(&[&foo_one, &both, &foo_remote]),
            "`any: foo` is every public row tagged foo, no matter who tagged it"
        );
        assert!(
            !for_any_foo.contains(&foo_unlisted.id) && !for_any_foo.contains(&foo_local.id),
            "the tag timeline is public-only: unlisted and local rows stay out"
        );
        let for_either = page_ids(&pool, &timeline(None, tags(&["foo", "bar"], &[], &[]))).await;
        assert_eq!(
            for_either,
            newest_first(&[&foo_one, &bar_one, &both, &foo_remote]),
            "`any` unions"
        );
        let for_both_tags = page_ids(&pool, &timeline(None, tags(&[], &["foo", "bar"], &[]))).await;
        assert_eq!(for_both_tags, newest_first(&[&both]), "`all` intersects");
        let for_foo_no_bar =
            page_ids(&pool, &timeline(Some(alice), tags(&["foo"], &[], &["bar"]))).await;
        assert_eq!(
            for_foo_no_bar,
            newest_first(&[&foo_one, &foo_remote]),
            "`none` subtracts, for any viewer"
        );
        let for_nothing_foo = page_ids(&pool, &timeline(None, tags(&[], &[], &["foo"]))).await;
        assert_eq!(
            for_nothing_foo,
            newest_first(&[&bar_one]),
            "`none` alone narrows the tagged set too"
        );
    }

    /// `Direct` answers exactly the `direct` rows the viewer is a party to —
    /// sent, or received by mention — no other account's DMs and no public
    /// post, and nothing at all for an anonymous request.
    #[tokio::test]
    async fn direct_answers_the_conversations_the_viewer_is_in() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let a_to_b = direct(&pool, &alice, &[&bob], "alice to bob").await;
        let b_to_a = direct(&pool, &bob, &[&alice], "bob to alice").await;
        let a_to_c = direct(&pool, &alice, &[&carol], "alice to carol").await;
        let public = post(&pool, &alice, Visibility::Public, "not a dm").await;

        let for_alice = page_ids(&pool, &timeline(Some(alice), TimelineSource::Direct)).await;
        assert_eq!(
            for_alice,
            newest_first(&[&a_to_b, &b_to_a, &a_to_c]),
            "the sender sees everything they sent and everything sent to them"
        );
        assert!(!for_alice.contains(&public.id), "a public post is not a DM");
        let for_bob = page_ids(&pool, &timeline(Some(bob), TimelineSource::Direct)).await;
        assert_eq!(
            for_bob,
            newest_first(&[&a_to_b, &b_to_a]),
            "the recipient only sees the messages naming them"
        );
        assert!(
            !for_bob.contains(&a_to_c.id),
            "a DM between other accounts never leaks into someone else's direct timeline"
        );
        let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Direct)).await;
        assert!(
            anonymous.is_empty(),
            "an anonymous direct timeline is the empty page"
        );
    }

    /// `Account` is everything the author wrote — posts, replies, their own
    /// reblogs — judged per row by the visibility clause: followers reach
    /// `private`, a mentioned party reaches `direct`, and a stranger (or the
    /// anonymous request) keeps to `public` and `unlisted`; someone else's
    /// reblog of the author's post is that reblogger's row, not the
    /// author's.
    #[tokio::test]
    async fn account_answers_the_authors_rows_with_per_row_visibility() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;
        let carol = local(&pool, "carol").await;
        let alice_follow = follow::follow(&pool, BASE, &alice, &bob)
            .await
            .expect("alice follows bob");
        assert_eq!(
            alice_follow.state,
            follow::FollowState::Accepted,
            "bob is unlocked, so the follow is granted"
        );
        let bob_id = bob.id;
        let account = move || TimelineSource::Account { user_id: bob_id };
        let public = post(&pool, &bob, Visibility::Public, "bob public").await;
        let unlisted = post(&pool, &bob, Visibility::Unlisted, "bob unlisted").await;
        let private = post(&pool, &bob, Visibility::Private, "bob private").await;
        let dm = direct(&pool, &bob, &[&alice], "bob dm to alice").await;
        let root = post(&pool, &carol, Visibility::Public, "carol root").await;
        let bob_reply = reply(&pool, &bob, &root, &[], "bob replies to carol").await;
        let carol_reblogs = reblog(&pool, &carol, &public).await;

        let for_follower = page_ids(&pool, &timeline(Some(alice), account())).await;
        assert_eq!(
            for_follower,
            newest_first(&[&public, &unlisted, &private, &dm, &bob_reply]),
            "a follower sees the whole history"
        );
        assert!(
            !for_follower.contains(&carol_reblogs.id),
            "someone else's boost of the author is the reblogger's row"
        );
        let for_stranger = page_ids(&pool, &timeline(Some(carol), account())).await;
        assert_eq!(
            for_stranger,
            newest_first(&[&public, &unlisted, &bob_reply]),
            "a non-follower reaches public and unlisted — but neither private nor DM"
        );
        let anonymous = page_ids(&pool, &timeline(None, account())).await;
        assert_eq!(
            anonymous,
            newest_first(&[&public, &unlisted, &bob_reply]),
            "the anonymous request reads the stranger's page"
        );
        let for_author = page_ids(&pool, &timeline(Some(bob), account())).await;
        assert_eq!(
            for_author,
            newest_first(&[&public, &unlisted, &private, &dm, &bob_reply]),
            "the author sees all of their own rows"
        );
    }
}
