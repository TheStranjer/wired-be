//! Unit tests of the `Home` source: whose rows ride the following
//! timeline, how a follow's `show_reblogs` toggle and the viewer's
//! `exclusive` lists take rows back out. Test-only.

use crate::{
    models::{
        follow,
        list::{self, ListChanges},
        status::Visibility,
        user::{Flag, NewUser, User, insert as insert_user},
    },
    services::timeline::{
        TimelineSource,
        test_support::{
            BASE, add_member, as_viewer, local, newest_first, owned_list, page_ids, post, reblog,
            timeline,
        },
    },
    test_fixtures::local_new,
};

/// Inserts a locked local user named `nickname`, so a follow of them
/// stays `pending` and never joins a home timeline.
async fn locked_local(pool: &sqlx::PgPool, nickname: &str) -> User {
    insert_user(
        pool,
        &NewUser {
            locked: Flag::new(true),
            ..local_new(nickname)
        },
    )
    .await
    .expect("the locked fixture inserts")
}

/// `Home` is the viewer's own rows and those of their **accepted**
/// follows only — a stranger's public post and a locked account's
/// pending-requested post both stay out — and an anonymous `Home` is the
/// empty page without a query. A follower reaches the followed
/// account's `private` and `unlisted` posts: that is the visibility
/// clause deciding, not the source.
#[tokio::test]
async fn home_answers_own_posts_and_accepted_follows_only() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let dave = locked_local(&pool, "dave").await;
    let accepted = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows open bob");
    assert_eq!(
        accepted.state,
        follow::FollowState::Accepted,
        "bob is unlocked, so the follow is granted"
    );
    let pending = follow::follow(&pool, BASE, &alice, &dave)
        .await
        .expect("alice requests locked dave");
    assert_eq!(
        pending.state,
        follow::FollowState::Pending,
        "dave is locked, so the request waits"
    );
    let own = post(&pool, &alice, Visibility::Public, "own post").await;
    let own_unlisted = post(&pool, &alice, Visibility::Unlisted, "own unlisted").await;
    let followed_public = post(&pool, &bob, Visibility::Public, "followed public").await;
    let followed_private = post(&pool, &bob, Visibility::Private, "followed private").await;
    let stranger = post(&pool, &carol, Visibility::Public, "stranger public").await;
    let pending_post = post(&pool, &dave, Visibility::Public, "pending public").await;

    let for_alice = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
    assert_eq!(
        for_alice,
        newest_first(&[&own, &own_unlisted, &followed_public, &followed_private]),
        "home is exactly the viewer's own posts and their accepted follows' visible posts"
    );
    assert!(
        !for_alice.contains(&stranger.id) && !for_alice.contains(&pending_post.id),
        "a stranger and a merely-requested locked account never join the home timeline"
    );
    let anonymous = page_ids(&pool, &timeline(None, TimelineSource::Home)).await;
    assert!(anonymous.is_empty(), "an anonymous home is the empty page");
}

/// A reblog row on `Home` rides its author's follow: with that follow's
/// `show_reblogs` switched off the boost disappears, the author's plain
/// posts stay, and the viewer's own reblogs were never governed by the
/// toggle.
#[tokio::test]
async fn home_hides_reblogs_from_follows_that_switched_them_off() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let dave = local(&pool, "dave").await;
    let bob_follow = follow::follow(&pool, BASE, &alice, &bob)
        .await
        .expect("alice follows bob");
    assert!(bob_follow.show_reblogs, "a fresh follow shows reblogs");
    let carol_follow = follow::follow(&pool, BASE, &alice, &carol)
        .await
        .expect("alice follows carol");
    assert!(carol_follow.show_reblogs, "and so does this one");
    let original = post(&pool, &dave, Visibility::Public, "reblogged original").await;
    let bob_reblog = reblog(&pool, &bob, &original).await;
    let carol_reblog = reblog(&pool, &carol, &original).await;
    let carol_plain = post(&pool, &carol, Visibility::Public, "carol plain").await;
    let own_reblog = reblog(&pool, &alice, &original).await;

    let before = page_ids(
        &pool,
        &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Home),
    )
    .await;
    assert_eq!(
        before,
        newest_first(&[&bob_reblog, &carol_reblog, &carol_plain, &own_reblog]),
        "with the default toggle both follows' reblogs are on the home timeline"
    );

    let hidden = follow::set_toggles(
        &pool,
        alice.id,
        carol.id,
        follow::FollowToggles {
            show_reblogs: Some(false),
            notify: None,
        },
    )
    .await
    .expect("the toggle writes")
    .expect("alice's follow of carol is open");
    assert!(!hidden.show_reblogs, "the follow now hides reblogs");
    let after = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
    assert_eq!(
        after,
        newest_first(&[&bob_reblog, &carol_plain, &own_reblog]),
        "carol's boost is gone, her plain posts and the viewer's own boost stay"
    );
}

/// An `exclusive` list takes its members back out of the viewer's
/// `Home`: the member's posts, the member's own boosts, and even a
/// stranger's boost **of** the member's post go, while a member of a
/// non-exclusive list stays and the viewer's own rows are never in
/// question — an account that lists itself keeps its place in its own
/// home.
#[tokio::test]
async fn an_exclusive_list_removes_its_members_from_home() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = local(&pool, "alice").await;
    let bob = local(&pool, "bob").await;
    let carol = local(&pool, "carol").await;
    let dave = local(&pool, "dave").await;
    for followed in [&bob, &carol, &dave] {
        let granted = follow::follow(&pool, BASE, &alice, followed)
            .await
            .expect("alice follows the fixture accounts");
        assert_eq!(
            granted.state,
            follow::FollowState::Accepted,
            "every fixture account is open"
        );
    }
    let open = owned_list(&pool, &alice, "list", false).await;
    add_member(&pool, &open, &carol).await;
    let exclusive = owned_list(&pool, &alice, "list", true).await;
    add_member(&pool, &exclusive, &bob).await;
    add_member(&pool, &exclusive, &alice).await;
    let alice_own = post(&pool, &alice, Visibility::Public, "own post").await;
    let bob_post = post(&pool, &bob, Visibility::Public, "exclusive member").await;
    let carol_post = post(&pool, &carol, Visibility::Public, "open list member").await;
    let dave_post = post(&pool, &dave, Visibility::Public, "plain follow").await;
    let bob_boost = reblog(&pool, &bob, &dave_post).await;
    let dave_boost = reblog(&pool, &dave, &bob_post).await;

    let home = page_ids(
        &pool,
        &timeline(Some(as_viewer(&pool, &alice).await), TimelineSource::Home),
    )
    .await;
    assert_eq!(
        home,
        newest_first(&[&alice_own, &carol_post, &dave_post]),
        "the exclusive member's rows are gone, her boost with them, and the viewer's \
         self-membership exempts her own post"
    );
    assert!(
        !home.contains(&bob_boost.id) && !home.contains(&dave_boost.id),
        "the member's boost and even the stranger's boost of the member's post stay out"
    );

    let relaxed = list::update(
        &pool,
        alice.id,
        exclusive.id,
        &ListChanges {
            title: None,
            replies_policy: None,
            exclusive: Some(false),
        },
    )
    .await
    .expect("the flag update succeeds")
    .expect("the fixture list exists");
    assert!(!relaxed.exclusive, "the list is no longer exclusive");
    let open_home = page_ids(&pool, &timeline(Some(alice), TimelineSource::Home)).await;
    assert_eq!(
        open_home,
        newest_first(&[
            &alice_own,
            &bob_post,
            &carol_post,
            &dave_post,
            &bob_boost,
            &dave_boost
        ]),
        "with exclusivity off, every followed row rides home again"
    );
}
