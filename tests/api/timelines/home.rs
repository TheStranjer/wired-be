//! The home timeline over HTTP (`GET /api/v1/timelines/home`): the
//! viewer's own posts — the reblogs included — and the accepted follows'
//! rows at every visibility a follower reaches, a reblog by a follow
//! riding until that follow's `show_reblogs` toggle goes off, the
//! `Link`-keyed id cursor, the `local`/`remote` split by author origin,
//! `only_media`, and the block and mute exclusions `with_muted` lifts.

use axum::http::StatusCode;
use serde_json::Value;
use wired_be::models::{
    block,
    follow::{self, FollowToggles},
    mute::{self, MuteOptions},
    status::Visibility,
};

use super::support::{
    BASE, HOME, accepted_follow, attach_media, get, id_of, ids, newest_first, reblog, remote_tagged,
};
use crate::common;

/// The home timeline answers the viewer's own posts — reblogs included —
/// and the accepted follows' posts at every visibility a follower reaches
/// (public, unlisted and `private` alike), and nothing of a stranger's; a
/// user with neither follows nor posts reads the empty page.
#[tokio::test]
async fn home_answers_the_viewers_own_posts_and_their_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    let own = common::create_status(&pool, &alice, "my own post", Visibility::Public).await;
    let own_unlisted =
        common::create_status(&pool, &alice, "my quiet post", Visibility::Unlisted).await;
    let stranger_root =
        common::create_status(&pool, &carol, "stranger original", Visibility::Public).await;
    let own_reblog = reblog(&pool, &alice, &stranger_root).await;
    let followed_public =
        common::create_status(&pool, &bob, "followed public", Visibility::Public).await;
    let followed_unlisted =
        common::create_status(&pool, &bob, "followed unlisted", Visibility::Unlisted).await;
    let followed_private =
        common::create_status(&pool, &bob, "followed private", Visibility::Private).await;
    let _stranger_plain =
        common::create_status(&pool, &carol, "stranger talk", Visibility::Public).await;
    let alice_token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let dave_token = common::create_app_and_token(&pool, &dave, "read:statuses").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&alice_token), HOME).await;
    assert_eq!(page.status, StatusCode::OK, "a home page is a 200");
    assert_eq!(
        page.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[
            &own,
            &own_unlisted,
            &own_reblog,
            &followed_public,
            &followed_unlisted,
            &followed_private
        ]),
        "own rows (the boost included) and the follow's rows at every visibility a follower \
         reaches; the stranger's own rows do not ride"
    );

    let empty = get(&app, Some(&dave_token), HOME).await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "a user with no follows and no posts reads the empty page"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// A reblog by a followed user rides the home timeline until that follow's
/// `show_reblogs` toggle goes off — the follow's plain posts and the
/// viewer's own boost stay.
#[tokio::test]
async fn home_follow_reblogs_honour_the_show_reblogs_toggle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    accepted_follow(&pool, &alice, &carol).await;
    let original =
        common::create_status(&pool, &dave, "reblogged original", Visibility::Public).await;
    let bob_reblog = reblog(&pool, &bob, &original).await;
    let carol_reblog = reblog(&pool, &carol, &original).await;
    let carol_plain = common::create_status(&pool, &carol, "carol plain", Visibility::Public).await;
    let own_reblog = reblog(&pool, &alice, &original).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool.clone());

    let before = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&before.json_value()),
        newest_first(&[&bob_reblog, &carol_reblog, &carol_plain, &own_reblog]),
        "fresh follows show reblogs: both boosts ride, the unfollowed original does not"
    );

    let hidden = follow::set_toggles(
        &pool,
        alice.id,
        carol.id,
        FollowToggles {
            show_reblogs: Some(false),
            notify: None,
        },
    )
    .await
    .expect("the toggle writes")
    .expect("alice's follow of carol is open");
    assert!(!hidden.show_reblogs, "the follow now hides reblogs");

    let after = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after.json_value()),
        newest_first(&[&bob_reblog, &carol_plain, &own_reblog]),
        "carol's boost is gone, her plain posts, bob's boost and alice's own boost stay"
    );
}

/// The home page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids and repeats the request's other parameters,
/// `max_id` continues from there, `since_id` and `min_id` pages come back
/// newest-first, and past the oldest row the page is empty and carries no
/// `Link` header.
#[tokio::test]
async fn home_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let oldest = common::create_status(&pool, &bob, "home post one", Visibility::Public).await;
    let middle = common::create_status(&pool, &bob, "home post two", Visibility::Public).await;
    let newest = common::create_status(&pool, &bob, "home post three", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let first = get(&app, Some(&token), &format!("{HOME}?local=true&limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{HOME}?local=true&limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{HOME}?local=true&limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `local` and `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{HOME}?local=true&limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let polled = get(
        &app,
        Some(&token),
        &format!("{HOME}?since_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&polled.json_value()),
        newest_first(&[&newest]),
        "a since_id poll answers the newer rows newest-first"
    );

    let forward = get(
        &app,
        Some(&token),
        &format!("{HOME}?min_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        ids(&forward.json_value()),
        newest_first(&[&middle, &newest]),
        "a min_id page is read ascending and answered newest-first"
    );

    let empty = get(
        &app,
        Some(&token),
        &format!("{HOME}?max_id={}", id_of(&oldest)),
    )
    .await;
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "past the oldest row the page is empty"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// `local` and `remote` split the home page by the author's origin: a
/// followed local account's post and a followed remote account's mirrored
/// post each answer only their own half.
#[tokio::test]
async fn home_local_and_remote_split_the_page_by_author_origin() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    accepted_follow(&pool, &alice, &bob).await;
    let mallory_request = follow::follow(&pool, BASE, &alice, &mallory)
        .await
        .expect("alice follows mallory");
    let mallory_followed = follow::accept(&pool, mallory_request.id)
        .await
        .expect("the remote instance accepts the request");
    assert_eq!(
        mallory_followed.state,
        follow::FollowState::Accepted,
        "the remote follow reads accepted, the state a mirrored `Accept` leaves it in"
    );
    let ours = common::create_status(&pool, &bob, "local follow", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "home-remote", &[]).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&ours, &theirs]),
        "the unfiltered page carries both follows"
    );
    let only_local = get(&app, Some(&token), &format!("{HOME}?local=true")).await;
    assert_eq!(
        ids(&only_local.json_value()),
        newest_first(&[&ours]),
        "`local=true` keeps this instance's follow"
    );
    let one = get(&app, Some(&token), &format!("{HOME}?local=1")).await;
    assert_eq!(
        ids(&one.json_value()),
        newest_first(&[&ours]),
        "`local=1` is the same truthy text"
    );
    let only_remote = get(&app, Some(&token), &format!("{HOME}?remote=true")).await;
    assert_eq!(
        ids(&only_remote.json_value()),
        newest_first(&[&theirs]),
        "`remote=true` keeps the remote follow's mirrored row"
    );
}

/// `only_media` keeps the followed posts that carry an attachment and drops
/// the ones that do not.
#[tokio::test]
async fn home_only_media_keeps_the_posts_with_attachments() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let pictured = common::create_status(&pool, &bob, "with a shot", Visibility::Public).await;
    let wordy = common::create_status(&pool, &bob, "only words", Visibility::Public).await;
    attach_media(&pool, &bob, &pictured).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[&pictured, &wordy]),
        "without the filter both ride"
    );
    let media = get(&app, Some(&token), &format!("{HOME}?only_media=true")).await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached one"
    );
}

/// The viewer's own exclusions clear a home page: a blocked account's
/// status disappears from a followed booster's reblog, muting the followed
/// author empties the page, and `with_muted=true` lifts the mute only —
/// Pleroma's toggle over the home read.
#[tokio::test]
async fn home_hides_muted_and_blocked_accounts_unless_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    accepted_follow(&pool, &alice, &bob).await;
    let blocked = block::block(&pool, BASE, &alice, &carol)
        .await
        .expect("alice blocks carol");
    assert_eq!(blocked.blocked_id, carol.id, "the block is live");
    let carol_root =
        common::create_status(&pool, &carol, "blocked original", Visibility::Public).await;
    let bob_plain = common::create_status(&pool, &bob, "kept follow", Visibility::Public).await;
    let bob_reblog = reblog(&pool, &bob, &carol_root).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool.clone());

    let after_block = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after_block.json_value()),
        newest_first(&[&bob_plain]),
        "the boost of the blocked account's original is excluded, the plain follow stays"
    );

    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute is live");
    let after_mute = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&after_mute.json_value()),
        Vec::<String>::new(),
        "the muted follow leaves the page entirely"
    );

    let shown = get(&app, Some(&token), &format!("{HOME}?with_muted=true")).await;
    let shown_ids = ids(&shown.json_value());
    assert_eq!(
        shown_ids,
        newest_first(&[&bob_plain]),
        "`with_muted=true` lifts the mute, but the boost of the blocked original stays out"
    );
    assert!(
        !shown_ids.contains(&bob_reblog.id.to_string()),
        "the boost of the blocked account's original stays off the page"
    );
}
