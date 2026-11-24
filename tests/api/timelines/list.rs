//! The list timeline over HTTP (`GET /api/v1/timelines/list/{list_id}`):
//! the members' posts read through membership alone, the replies policy
//! taken from the stored list row, the `only_media` and `with_muted`
//! narrows, the blocked member and the memberless empty page, the one 404
//! for a foreign, unknown or malformed id, and the user-token gate
//! demanding `read:lists`, and the `home`-context phrase filters applied to
//! the page. Owns the `owned_list` and `add_member` fixtures
//! every test here seeds through [`wired_be::models::list`].

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::models::{
    block,
    filter::{self, NewFilter},
    list::{self, NewList},
    media_attachment::{self, NewMediaAttachment},
    mute::{self, MuteOptions},
    status::Visibility,
    user::User,
};

use super::support::{BASE, LIST, get, id_of, ids, newest_first, reply};
use crate::common;

/// Creates `owner`'s list titled `title` with `policy` as its replies
/// policy, straight through [`wired_be::models::list`].
async fn owned_list(pool: &PgPool, owner: &User, policy: &str) -> list::List {
    list::create(
        pool,
        owner.id,
        &NewList {
            title: "circle".to_owned(),
            replies_policy: policy.to_owned(),
            exclusive: false,
        },
    )
    .await
    .expect("the list fixture creates")
}

/// Adds `member` to `list_id` through the model, below the API's own
/// follow rule: membership is what the timeline reads.
async fn add_member(pool: &PgPool, list_id: Uuid, member: &User) {
    let added = list::add_members(pool, list_id, &[member.id])
        .await
        .expect("the membership fixture adds");
    assert_eq!(added, 1, "exactly one membership went in");
}

/// The list timeline answers the members' posts and nothing else:
/// membership alone puts a row on the page (no follow is read, in either
/// direction), and the list owner's own post stays off it because the
/// owner is not a member.
#[tokio::test]
async fn list_timeline_answers_member_posts_without_follows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let member_post = common::create_status(&pool, &bob, "member talk", Visibility::Public).await;
    let member_reply = reply(&pool, &bob, &member_post, &[], "member answers themselves").await;
    let _followed_stranger =
        common::create_status(&pool, &carol, "non-member talk", Visibility::Public).await;
    let _own = common::create_status(&pool, &alice, "owner's own post", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&token), &format!("{LIST}/{}", circle.id)).await;
    assert_eq!(page.status, StatusCode::OK, "a list page is a 200");
    assert_eq!(
        page.header("content-type"),
        Some("application/json"),
        "the page is JSON"
    );
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[&member_post, &member_reply]),
        "the member's root and the member's reply ride the page; the non-member's and the \
         owner's own posts do not"
    );
}

/// The endpoint takes the replies policy from the stored list row: two
/// lists, each with its own single member posting a root and a reply
/// addressed to that same member, and a `none` list answers the root
/// while a `list` list answers root and reply.
#[tokio::test]
async fn list_timeline_replies_follow_the_stored_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let dave = common::create_local_user(&pool, "dave").await;
    let talking = owned_list(&pool, &alice, "list").await;
    add_member(&pool, talking.id, &bob).await;
    let talking_root = common::create_status(&pool, &bob, "talking root", Visibility::Public).await;
    let talking_reply = reply(&pool, &bob, &talking_root, &[], "a member's answer").await;
    let silent = owned_list(&pool, &alice, "none").await;
    add_member(&pool, silent.id, &dave).await;
    let silent_root = common::create_status(&pool, &dave, "silent root", Visibility::Public).await;
    let _silent_reply = reply(&pool, &dave, &silent_root, &[], "an answer `none` drops").await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let policy_list = get(&app, Some(&token), &format!("{LIST}/{}", talking.id)).await;
    assert_eq!(
        ids(&policy_list.json_value()),
        newest_first(&[&talking_root, &talking_reply]),
        "`list` keeps the reply addressed to the member"
    );
    let policy_none = get(&app, Some(&token), &format!("{LIST}/{}", silent.id)).await;
    assert_eq!(
        ids(&policy_none.json_value()),
        newest_first(&[&silent_root]),
        "`none` drops every reply, addressed however it likes"
    );
}

/// The list page walks the id cursor like every other timeline: `limit`
/// cuts it, the `Link` header names the next and prev targets over the
/// page's oldest and newest ids, `max_id` continues from there, and past
/// the oldest row the page is empty and carries no `Link` header.
#[tokio::test]
async fn list_pages_walk_the_id_cursor_under_link_headers() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let oldest = common::create_status(&pool, &bob, "list post one", Visibility::Public).await;
    let middle = common::create_status(&pool, &bob, "list post two", Visibility::Public).await;
    let newest = common::create_status(&pool, &bob, "list post three", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let first = get(&app, Some(&token), &format!("{route}?limit=2")).await;
    assert_eq!(
        ids(&first.json_value()),
        newest_first(&[&middle, &newest]),
        "the first page is the two newest rows"
    );
    let expected_link = format!(
        "<{BASE}{route}?limit=2&max_id={}>; rel=\"next\", \
         <{BASE}{route}?limit=2&min_id={}>; rel=\"prev\"",
        id_of(&middle),
        id_of(&newest),
    );
    assert_eq!(
        first.header("link"),
        Some(expected_link.as_str()),
        "next walks to the page's oldest id, prev back to its newest, `limit` repeated"
    );

    let second = get(
        &app,
        Some(&token),
        &format!("{route}?limit=2&max_id={}", id_of(&middle)),
    )
    .await;
    assert_eq!(
        ids(&second.json_value()),
        newest_first(&[&oldest]),
        "the next target answers the row older than the cursor"
    );

    let empty = get(
        &app,
        Some(&token),
        &format!("{route}?max_id={}", id_of(&oldest)),
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

/// `only_media` keeps the member's attached row, the viewer's mute clears
/// the member off the list page, and `with_muted=true` brings the rows
/// back — Pleroma's toggles, applied over the membership read.
#[tokio::test]
async fn list_timeline_narrows_with_only_media_and_lifts_mutes_with_with_muted() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let plain = common::create_status(&pool, &bob, "plain member", Visibility::Public).await;
    let pictured = common::create_status(&pool, &bob, "pictured member", Visibility::Public).await;
    let attached = media_attachment::insert(
        &pool,
        &NewMediaAttachment {
            user_id: Some(bob.id),
            status_id: Some(pictured.id),
            url: format!("{BASE}/media/list.png"),
            content_type: "image/png".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect("the media fixture attaches");
    assert_eq!(attached.status_id, Some(pictured.id), "one image lands");
    let muted = mute::mute(&pool, &alice, &bob, MuteOptions::default())
        .await
        .expect("alice mutes bob");
    assert_eq!(muted.muted_id, bob.id, "the mute is live");
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let hidden = get(&app, Some(&token), &route).await;
    assert_eq!(
        ids(&hidden.json_value()),
        Vec::<String>::new(),
        "the muted member leaves the list page"
    );
    let shown = get(&app, Some(&token), &format!("{route}?with_muted=true")).await;
    assert_eq!(
        ids(&shown.json_value()),
        newest_first(&[&plain, &pictured]),
        "`with_muted=true` lifts alice's own exclusion"
    );
    let media = get(
        &app,
        Some(&token),
        &format!("{route}?with_muted=true&only_media=true"),
    )
    .await;
    assert_eq!(
        ids(&media.json_value()),
        newest_first(&[&pictured]),
        "`only_media=true` keeps the attached row"
    );
}

/// A list that is another account's, an id that names no list and a
/// malformed id all answer the same 404 "Record not found": the timeline
/// never tells a foreign list from a missing one, nor either from a
/// misspelled id.
#[tokio::test]
async fn a_foreign_unknown_or_malformed_list_is_the_json_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let his = owned_list(&pool, &bob, "list").await;
    add_member(&pool, his.id, &alice).await;
    let _his_post =
        common::create_status(&pool, &bob, "his member's post", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    for spelling in [
        his.id.to_string(),
        Uuid::now_v7().to_string(),
        "not-a-real-id".to_owned(),
    ] {
        let response = get(&app, Some(&token), &format!("{LIST}/{spelling}")).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "`{spelling}` is the 404"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 404 answers JSON"
        );
        common::assert_error(&response, "Record not found", "an unreadable list id");
    }
}

/// The list route is gated on a user token granting `read:lists`: no
/// token is the 401 "Invalid credentials" and a broken one the 401
/// "The access token is invalid"; an app-only token and a user token that
/// only grants `read:statuses` are the 403s; a granted token reads its
/// page — `read:statuses` alone never does.
#[tokio::test]
async fn the_list_route_requires_a_user_token_granting_read_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let member_post = common::create_status(&pool, &bob, "listed talk", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let statuses_only = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:lists").await;
    let app = common::test_app(pool);
    let route = format!("{LIST}/{}", circle.id);

    let anonymous = get(&app, None, &route).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), &route).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), &route).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&statuses_only), &route).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:lists",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:lists.",
        "a token without read:lists",
    );

    let granted = get(&app, Some(&reader), &route).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&member_post]),
        "and sees the member's post"
    );
}

/// The blocked member stays off the list page like every other read's
/// excluded author — and `read:lists` on the viewer's own list answers
/// the empty page for a list with no members.
#[tokio::test]
async fn a_blocked_member_leaves_the_list_page_and_an_empty_list_answers_empty() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    add_member(&pool, circle.id, &carol).await;
    let carol_post = common::create_status(&pool, &carol, "kept member", Visibility::Public).await;
    let blocked = block::block(&pool, BASE, &alice, &bob)
        .await
        .expect("alice blocks bob");
    assert_eq!(blocked.blocked_id, bob.id, "the block is live");
    let _bob_post = common::create_status(&pool, &bob, "blocked member", Visibility::Public).await;
    let empty_list = owned_list(&pool, &alice, "list").await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool);

    let page = get(&app, Some(&token), &format!("{LIST}/{}", circle.id)).await;
    assert_eq!(
        ids(&page.json_value()),
        newest_first(&[&carol_post]),
        "the blocked member is out, the unblocked one stays"
    );
    let empty = get(&app, Some(&token), &format!("{LIST}/{}", empty_list.id)).await;
    assert_eq!(
        empty.status,
        StatusCode::OK,
        "a memberless own list is a 200"
    );
    assert_eq!(
        empty.json::<Vec<Value>>(),
        Vec::<Value>::new(),
        "and the empty page"
    );
    assert_eq!(
        empty.header("link"),
        None,
        "an empty page sends no Link header"
    );
}

/// The list timeline renders in the `home` filter context ("Home and lists"):
/// an irreversible home filter drops the matching member post, a warn filter
/// keeps it with a non-empty `filtered`, and a `public`-only filter changes
/// nothing.
#[tokio::test]
async fn list_timeline_applies_the_home_context_filters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let circle = owned_list(&pool, &alice, "list").await;
    add_member(&pool, circle.id, &bob).await;
    let spoiler = common::create_status(&pool, &bob, "a spoiler inside", Visibility::Public).await;
    let clean = common::create_status(&pool, &bob, "nothing to see", Visibility::Public).await;
    let token = common::create_app_and_token(&pool, &alice, "read:lists").await;
    let app = common::test_app(pool.clone());
    let path = format!("{LIST}/{}", circle.id);
    let filter_for = |contexts: &[&str], irreversible: bool| NewFilter {
        phrase: "spoiler".to_owned(),
        context: contexts.iter().map(|name| (*name).to_owned()).collect(),
        irreversible,
        ..NewFilter::default()
    };

    let public_only = filter::create(&pool, alice.id, &filter_for(&["public"], true))
        .await
        .expect("the public-only filter creates");
    let untouched = get(&app, Some(&token), &path).await;
    assert_eq!(
        ids(&untouched.json_value()),
        newest_first(&[&spoiler, &clean]),
        "a `public`-only filter does not reach the list timeline"
    );
    for entry in untouched
        .json_value()
        .as_array()
        .expect("a page is an array")
    {
        assert_eq!(
            entry["filtered"],
            serde_json::json!([]),
            "and flags nothing"
        );
    }
    assert!(
        filter::delete(&pool, public_only.id, alice.id)
            .await
            .expect("the filter deletes"),
        "the public-only filter was live"
    );

    let warn = filter::create(&pool, alice.id, &filter_for(&["home"], false))
        .await
        .expect("the warn filter creates");
    let warned = get(&app, Some(&token), &path).await;
    let body = warned.json_value();
    assert_eq!(
        ids(&body),
        newest_first(&[&spoiler, &clean]),
        "a warn filter keeps the matching post on the page"
    );
    let flagged = body
        .as_array()
        .expect("a page is an array")
        .iter()
        .find(|entry| entry["id"] == serde_json::json!(spoiler.id.to_string()))
        .expect("the matching post is on the page");
    assert!(
        flagged["filtered"]
            .as_array()
            .is_some_and(|results| !results.is_empty()),
        "the matching post carries a non-empty `filtered`"
    );
    assert!(
        filter::delete(&pool, warn.id, alice.id)
            .await
            .expect("the filter deletes"),
        "the warn filter was live"
    );

    let _hider = filter::create(&pool, alice.id, &filter_for(&["home"], true))
        .await
        .expect("the irreversible filter creates");
    let hidden = get(&app, Some(&token), &path).await;
    assert_eq!(
        ids(&hidden.json_value()),
        newest_first(&[&clean]),
        "an irreversible home filter hides the matching post from the list"
    );
}
