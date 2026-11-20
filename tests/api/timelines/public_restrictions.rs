//! The `RESTRICT_UNAUTHENTICATED` gate over HTTP: the default
//! configuration leaving every anonymous public read open, the
//! `timelines_local` area refusing anonymous `local=true` reads, the
//! `timelines_federated` area refusing every other anonymous read, and
//! both areas listed closing the timeline to anonymous reads only —
//! authenticated reads and the extractor's 401 riding through untouched.

use axum::http::StatusCode;
use wired_be::models::status::Visibility;

use super::support::{PUBLIC, RESTRICTED, get, ids, newest_first, remote_tagged};
use crate::common;

/// The default configuration restricts nothing: every anonymous read —
/// federated or `local=true` — answers the normal page.
#[tokio::test]
async fn unauthenticated_public_reads_are_allowed_by_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "default-open", &[]).await;
    let app = common::test_app(pool);

    for (query, want) in [
        ("", newest_first(&[&ours, &theirs])),
        ("?local=true", newest_first(&[&ours])),
        ("?remote=true", newest_first(&[&theirs])),
    ] {
        let page = get(&app, None, &format!("{PUBLIC}{query}")).await;
        assert_eq!(
            page.status,
            StatusCode::OK,
            "the default instance refuses nothing"
        );
        assert_eq!(
            ids(&page.json_value()),
            want,
            "`GET {PUBLIC}{query}` reads normally"
        );
    }
}

/// With `timelines_local` restricted, the anonymous `local=true` read is the
/// 403 with the ticket's exact wording; every other read — the plain and
/// `local=false` anonymous pages and any authenticated one — stays normal.
#[tokio::test]
async fn restricted_timelines_local_refuses_anonymous_local_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app_only = common::create_app_only_token(&pool, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_local"]);
    let expected = newest_first(&[&post]);

    let anonymous_local = get(&app, None, &format!("{PUBLIC}?local=true")).await;
    common::assert_answer(
        &anonymous_local,
        StatusCode::FORBIDDEN,
        "an anonymous `local=true` read of a `timelines_local`-restricted instance",
    );
    common::assert_error(
        &anonymous_local,
        RESTRICTED,
        "an anonymous `local=true` read of a `timelines_local`-restricted instance",
    );

    let client_local = get(&app, Some(&app_only), &format!("{PUBLIC}?local=true")).await;
    common::assert_answer(
        &client_local,
        StatusCode::FORBIDDEN,
        "an app-only token is no user to this gate",
    );
    common::assert_error(
        &client_local,
        RESTRICTED,
        "an app-only token reads anonymously",
    );

    let anonymous = get(&app, None, &format!("{PUBLIC}?local=false")).await;
    assert_eq!(
        anonymous.status,
        StatusCode::OK,
        "`local=false` is the federated page, which this area does not gate"
    );
    assert_eq!(ids(&anonymous.json_value()), expected, "and reads normally");

    let federated = get(&app, None, PUBLIC).await;
    assert_eq!(
        federated.status,
        StatusCode::OK,
        "the plain anonymous page stays open"
    );

    let authenticated = get(&app, Some(&reader), &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated `local=true` read was never the gate's business"
    );
    assert_eq!(
        ids(&authenticated.json_value()),
        expected,
        "and reads normally"
    );
}

/// With `timelines_federated` restricted, the anonymous non-`local` reads —
/// the plain page and `remote=true` included — are the same 403, while
/// `local=true` belongs to the other area and stays open.
#[tokio::test]
async fn restricted_timelines_federated_refuses_anonymous_federated_reads() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mallory = common::create_remote_user(&pool, "mallory", "remote.example").await;
    let ours = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let theirs = remote_tagged(&pool, &mallory, "federated-gated", &[]).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app = common::test_app_with_restricted_areas(pool, &["timelines_federated"]);

    for query in ["", "?remote=true", "?local=false"] {
        let refused = get(&app, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("an anonymous read `{query}` of a `timelines_federated`-restricted instance"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the refusal for `{query}`"));
    }

    let local_page = get(&app, None, &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        local_page.status,
        StatusCode::OK,
        "`local=true` belongs to `timelines_local`, which is not restricted here"
    );
    assert_eq!(
        ids(&local_page.json_value()),
        newest_first(&[&ours]),
        "and reads its own page normally"
    );

    let authenticated = get(&app, Some(&reader), PUBLIC).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "an authenticated federated read passes whatever is restricted"
    );
    assert_eq!(
        ids(&authenticated.json_value()),
        newest_first(&[&ours, &theirs]),
        "and sees the whole page"
    );
}

/// With both areas listed, every anonymous read is the same 403 — an
/// authenticated read stays normal — and a presented-but-broken token is
/// still the extractor's 401: the restriction never papers over a bad
/// credential.
#[tokio::test]
async fn both_areas_restricted_close_the_timeline_to_anonymous_reads_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let post = common::create_status(&pool, &alice, "from home", Visibility::Public).await;
    let reader = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let app =
        common::test_app_with_restricted_areas(pool, &["timelines_local", "timelines_federated"]);

    for query in ["", "?local=true", "?local=false&remote=true"] {
        let refused = get(&app, None, &format!("{PUBLIC}{query}")).await;
        common::assert_answer(
            &refused,
            StatusCode::FORBIDDEN,
            &format!("the anonymous read `{query}` of a fully restricted instance"),
        );
        common::assert_error(&refused, RESTRICTED, &format!("the refusal for `{query}`"));
    }

    let authenticated = get(&app, Some(&reader), &format!("{PUBLIC}?local=true")).await;
    assert_eq!(
        authenticated.status,
        StatusCode::OK,
        "a user token reads whatever the instance restricts to strangers"
    );
    assert_eq!(ids(&authenticated.json_value()), newest_first(&[&post]));

    let broken = get(&app, Some("not-a-real-token"), PUBLIC).await;
    common::assert_answer(&broken, StatusCode::UNAUTHORIZED, "a broken token");
    common::assert_error(&broken, "The access token is invalid", "a broken token");
}
