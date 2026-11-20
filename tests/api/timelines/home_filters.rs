//! The home timeline's narrowing parameters and gate: `exclude_visibilities[]`
//! dropping the named rows, `reply_visibility` narrowing the replies —
//! `exclude_replies` ignored, an unknown spelling the 422 — and the
//! user-token demand for `read:statuses` with the malformed cursor's 400.

use axum::http::StatusCode;
use wired_be::models::status::Visibility;

use super::support::{HOME, accepted_follow, get, ids, newest_first, reply};
use crate::common;

/// `exclude_visibilities[]` drops the named visibilities from the home
/// page; a name outside the five stored spellings narrows nothing.
#[tokio::test]
async fn home_exclude_visibilities_drops_the_named_rows() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    accepted_follow(&pool, &alice, &bob).await;
    let own_unlisted =
        common::create_status(&pool, &alice, "my quiet post", Visibility::Unlisted).await;
    let bob_public =
        common::create_status(&pool, &bob, "followed public", Visibility::Public).await;
    let bob_private =
        common::create_status(&pool, &bob, "followed private", Visibility::Private).await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);
    let expected = newest_first(&[&own_unlisted, &bob_public, &bob_private]);

    let kept = get(
        &app,
        Some(&token),
        &format!("{HOME}?exclude_visibilities[]=Public"),
    )
    .await;
    assert_eq!(
        ids(&kept.json_value()),
        expected,
        "a name that spells no stored visibility excludes nothing"
    );
    let narrowed = get(
        &app,
        Some(&token),
        &format!("{HOME}?exclude_visibilities[]=private&exclude_visibilities[]=unlisted"),
    )
    .await;
    assert_eq!(
        ids(&narrowed.json_value()),
        newest_first(&[&bob_public]),
        "excluding `private` and `unlisted` leaves only the public row"
    );
}

/// Replies ride the home timeline by default — answering a non-followed
/// account included, and `exclude_replies` is not a home parameter and
/// changes nothing — while `reply_visibility=following` keeps only replies
/// addressed to the viewer or a follow and `self` only those addressed to
/// the viewer, by target or by @-mention.
#[tokio::test]
async fn home_reply_visibility_narrows_the_replies_and_exclude_replies_is_ignored() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let dave = common::create_local_user(&pool, "dave").await;
    accepted_follow(&pool, &alice, &bob).await;
    accepted_follow(&pool, &alice, &dave).await;
    let alice_root =
        common::create_status(&pool, &alice, "viewer's own root", Visibility::Public).await;
    let dave_root = common::create_status(&pool, &dave, "followed root", Visibility::Public).await;
    let carol_root =
        common::create_status(&pool, &carol, "stranger root", Visibility::Public).await;
    let to_carol = reply(&pool, &bob, &carol_root, &[], "answers a stranger").await;
    let to_alice = reply(&pool, &bob, &alice_root, &[], "answers the viewer").await;
    let to_dave = reply(&pool, &bob, &dave_root, &[], "answers a follow").await;
    let mentions_alice = reply(
        &pool,
        &bob,
        &carol_root,
        &[&alice],
        "stranger's thread, names alice",
    )
    .await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    let whole = get(&app, Some(&token), HOME).await;
    assert_eq!(
        ids(&whole.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_carol,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "by default every reply of a followed author rides, a stranger's own root does not"
    );
    let ignored = get(&app, Some(&token), &format!("{HOME}?exclude_replies=true")).await;
    assert_eq!(
        ids(&ignored.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_carol,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "`exclude_replies` is not a home parameter and drops nothing"
    );
    let following = get(
        &app,
        Some(&token),
        &format!("{HOME}?reply_visibility=following"),
    )
    .await;
    assert_eq!(
        ids(&following.json_value()),
        newest_first(&[
            &alice_root,
            &dave_root,
            &to_alice,
            &to_dave,
            &mentions_alice
        ]),
        "`following` keeps replies addressed to the viewer or a follow"
    );
    let only_self = get(&app, Some(&token), &format!("{HOME}?reply_visibility=self")).await;
    assert_eq!(
        ids(&only_self.json_value()),
        newest_first(&[&alice_root, &dave_root, &to_alice, &mentions_alice]),
        "`self` keeps only the replies addressed to the viewer, by target or mention"
    );
}

/// A `reply_visibility` outside `following` and `self` — the enum
/// Pleroma's `timeline_operation` lists — is the 422 with Mastodon's
/// changeset wording; the empty spelling is outside it too.
#[tokio::test]
async fn an_unknown_home_reply_visibility_is_the_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let token = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app(pool);

    for spelling in ["bogus", "Following", ""] {
        let response = get(
            &app,
            Some(&token),
            &format!("{HOME}?reply_visibility={spelling}"),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{spelling}` is the 422"
        );
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "the 422 answers JSON"
        );
        common::assert_error(
            &response,
            "Validation failed: Reply visibility is not included in the list",
            "an unusable `reply_visibility`",
        );
    }
}

/// The home route is gated on a user token granting `read:statuses`: no
/// token is the 401 "Invalid credentials" and a broken one the 401 "The
/// access token is invalid"; an app-only token and a user token without the
/// scope are the 403s; a granted token reads its page. A malformed cursor
/// is the 400 "Invalid id" every keyset listing here answers with.
#[tokio::test]
async fn home_requires_a_user_token_granting_read_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let own = common::create_status(&pool, &alice, "my own post", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let noscope = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app(pool);

    let anonymous = get(&app, None, HOME).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token");
    common::assert_error(&anonymous, "Invalid credentials", "no token");

    let broken = get(&app, Some("not-a-real-token"), HOME).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");

    let client = get(&app, Some(&app_only), HOME).await;
    common::assert_answer(&client, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &client,
        "This API requires an authenticated user",
        "an app-only token",
    );

    let unscoped = get(&app, Some(&noscope), HOME).await;
    common::assert_answer(
        &unscoped,
        StatusCode::FORBIDDEN,
        "a token without read:statuses",
    );
    common::assert_error(
        &unscoped,
        "Insufficient permissions: read:statuses.",
        "a token without read:statuses",
    );

    let granted = get(&app, Some(&reader), HOME).await;
    assert_eq!(
        granted.status,
        StatusCode::OK,
        "a granted user token reads its page"
    );
    assert_eq!(
        ids(&granted.json_value()),
        newest_first(&[&own]),
        "and sees their own post"
    );

    let bad_cursor = get(&app, Some(&reader), &format!("{HOME}?max_id=nope")).await;
    assert_eq!(
        bad_cursor.status,
        StatusCode::BAD_REQUEST,
        "a bogus cursor is the 400"
    );
    common::assert_error(&bad_cursor, "Invalid id", "a bogus cursor");
}
