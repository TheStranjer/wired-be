//! The `RESTRICT_UNAUTHENTICATED` gate over HTTP on the tag timeline: the
//! `timelines_federated` area refusing every anonymous read, the
//! `timelines_local` area refusing only the anonymous `local=true` one, and
//! authenticated reads riding through both.

use axum::http::StatusCode;

use super::support::{RESTRICTED, TAG, get, ids, newest_first, tagged};
use crate::common;

/// With `timelines_federated` restricted, the anonymous tag read is the 403
/// with the ticket's wording, an app-only token is no user to the gate, and a
/// user token reads the page; the anonymous `local=true` read is the other
/// area's, which stays open.
#[tokio::test]
async fn restricted_timelines_federated_refuses_anonymous_tag_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "a #foo post").await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_federated"]);

    let refused = get(&app, None, &format!("{TAG}/foo")).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "an anonymous tag read of a `timelines_federated`-restricted instance",
    );
    common::assert_error(&refused, RESTRICTED, "the anonymous tag refusal");

    let client = get(&app, Some(&app_only), &format!("{TAG}/foo")).await;
    common::assert_answer(
        &client,
        StatusCode::FORBIDDEN,
        "an app-only token is no user to this gate",
    );
    common::assert_error(&client, RESTRICTED, "the app-only tag refusal");

    let authenticated = get(&app, Some(&reader), &format!("{TAG}/foo")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated tag read passes whatever is restricted"
    );
    assert_eq!(
        ids(&authenticated.json_value()),
        newest_first(&[&post]),
        "and reads the normal page"
    );

    let local = get(&app, None, &format!("{TAG}/foo?local=true")).await;
    assert_eq!(
        local.status,
        StatusCode::OK,
        "`local=true` belongs to `timelines_local`, which is not restricted here"
    );
    assert_eq!(ids(&local.json_value()), newest_first(&[&post]));
}

/// With `timelines_local` restricted, the anonymous `local=true` tag read is
/// the 403 while the plain read stays open, and an authenticated `local=true`
/// read passes.
#[tokio::test]
async fn restricted_timelines_local_refuses_anonymous_local_tag_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "a #foo post").await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_local"]);

    let refused = get(&app, None, &format!("{TAG}/foo?local=true")).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "an anonymous `local=true` tag read of a `timelines_local`-restricted instance",
    );
    common::assert_error(&refused, RESTRICTED, "the anonymous local tag refusal");

    let plain = get(&app, None, &format!("{TAG}/foo")).await;
    assert_eq!(
        plain.status,
        StatusCode::OK,
        "a tag read without `local` is the federated one, which stays open"
    );
    assert_eq!(ids(&plain.json_value()), newest_first(&[&post]));

    let authenticated = get(&app, Some(&reader), &format!("{TAG}/foo?local=true")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated `local=true` tag read passes"
    );
    assert_eq!(ids(&authenticated.json_value()), newest_first(&[&post]));
}

/// The default configuration restricts nothing: the anonymous tag read,
/// `local=true` or not, answers the normal page.
#[tokio::test]
async fn unauthenticated_tag_reads_are_allowed_by_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = tagged(&pool, &alice, "a #foo post").await;
    let app = common::test_app(pool);

    for query in ["", "?local=true"] {
        let page = get(&app, None, &format!("{TAG}/foo{query}")).await;
        assert_eq!(page.status, StatusCode::OK, "`{query}` is open by default");
        assert_eq!(ids(&page.json_value()), newest_first(&[&post]));
    }
}
