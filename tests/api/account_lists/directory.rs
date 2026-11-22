//! `GET /api/v1/directory`: the discoverable profile directory.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::user::Flag;

use super::support::{
    BASE, INVALID_TOKEN, backdate, deactivate, directory, discoverable, fixture, get, listed_ids,
    make_discoverable,
};
use crate::common;

/// `GET /api/v1/directory` needs no token and answers the discoverable,
/// active accounts: newest activity first, the never-posted ones last, and
/// nothing that opted out or closed down.
#[tokio::test]
async fn directory_lists_discoverable_accounts_without_auth() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let newest = discoverable(&pool, "newest", Some(1)).await;
    let older = discoverable(&pool, "older", Some(5)).await;
    let quiet = discoverable(&pool, "quiet", None).await;
    let private = common::create_local_user(&pool, "private").await;
    let closed = discoverable(&pool, "closed", Some(0)).await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool);

    let listed = directory(&app, "").await;
    assert_eq!(
        listed_ids(&listed),
        vec![
            newest.id.to_string(),
            older.id.to_string(),
            quiet.id.to_string()
        ],
        "newest status first, the account that never posted last"
    );
    assert!(
        !listed_ids(&listed).contains(&private.id.to_string()),
        "an account that never opted into discovery is not listed"
    );
    assert!(
        !listed_ids(&listed).contains(&closed.id.to_string()),
        "and neither is a deactivated one"
    );
}

/// One directory entry is an `Account` entity: the stored profile, rendered
/// for an anonymous viewer, so `pleroma.relationship` is the empty object.
#[tokio::test]
async fn directory_answers_account_entities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |mut new| {
        new.discoverable = Flag::new(true);
        new.name = "Alice".to_owned();
        new
    })
    .await;
    let app = common::test_app(pool);

    let listed = directory(&app, "").await;
    let entries = listed.as_array().expect("the listing is an array");
    assert_eq!(entries.len(), 1, "the one discoverable account");
    let account = &entries[0];
    assert_eq!(account["id"], alice.id.to_string(), "the id");
    assert_eq!(account["username"], "alice", "the handle");
    assert_eq!(account["acct"], "alice", "and the local acct");
    assert_eq!(account["display_name"], "Alice", "the display name");
    assert_eq!(account["url"], format!("{BASE}/users/alice"), "the url");
    assert_eq!(
        account["pleroma"]["relationship"],
        json!({}),
        "an anonymous request has no relationship to embed"
    );
}

/// The `order` and `local` parameters steer the listing: `new` sorts on the
/// account itself, `local=true` drops the remote mirrors, and any other
/// spelling keeps Pleroma's default of local accounts plus mirrors.
#[tokio::test]
async fn directory_order_and_local_flags_narrow_the_listing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let veteran = discoverable(&pool, "veteran", Some(1)).await;
    let fresh = discoverable(&pool, "fresh", Some(9)).await;
    backdate(&pool, &veteran, 30).await;
    backdate(&pool, &fresh, 1).await;
    let mirror = common::create_remote_user(&pool, "mirror", "remote.example").await;
    make_discoverable(&pool, &mirror).await;
    let app = common::test_app(pool);
    let by_activity = vec![
        veteran.id.to_string(),
        fresh.id.to_string(),
        mirror.id.to_string(),
    ];

    assert_eq!(
        listed_ids(&directory(&app, "").await),
        by_activity,
        "with no `order` the activity sort runs, and it lists the mirror too"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?order=bogus").await),
        by_activity,
        "an unrecognised spelling is the default order"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?order=new").await),
        vec![
            mirror.id.to_string(),
            fresh.id.to_string(),
            veteran.id.to_string()
        ],
        "`new` puts the newest account first, ignoring last activity: the \
         fresh mirror, then the year-old veteran"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?local=true").await),
        vec![veteran.id.to_string(), fresh.id.to_string()],
        "`local=true` answers this instance's own accounts only"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?local=false").await),
        by_activity,
        "and `local=false` is the default: mirrors stay in"
    );
}

/// The directory pages by `offset` and honours `limit`, so walking the same
/// order twice covers every account exactly once.
#[tokio::test]
async fn directory_pages_by_limit_and_offset() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut expected = Vec::new();
    for (number, nickname) in ["one", "two", "three"].iter().enumerate() {
        let days_ago = i64::try_from(number).expect("a small index") + 1;
        let posted = discoverable(&pool, nickname, Some(days_ago)).await;
        expected.push(posted.id.to_string());
    }
    let app = common::test_app(pool);

    assert_eq!(
        listed_ids(&directory(&app, "?limit=2").await),
        expected[..2],
        "`limit=2` answers the head of the listing"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?limit=2&offset=2").await),
        expected[2..],
        "and `offset=2` picks up where it stopped"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?offset=99").await),
        Vec::<String>::new(),
        "an offset past the end answers nothing"
    );
    assert_eq!(
        listed_ids(&directory(&app, "?limit=-3").await),
        expected,
        "an unusable limit falls back to the default page of 40"
    );
}

/// The directory never requires a token, but it never ignores a bad one
/// either: an invalid credential is the extractor's 401, and a valid one
/// only decides whose view the entities render as.
#[tokio::test]
async fn directory_accepts_an_anonymous_request_and_refuses_a_bad_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "watcher", "read").await;
    let listed = discoverable(&pool, "listed", Some(1)).await;
    let app = common::test_app(pool);
    let open = get(&app, None, "/api/v1/directory").await;
    common::assert_answer(&open, StatusCode::OK, "the directory with no token");
    assert_eq!(
        listed_ids(&open.json_value()),
        vec![listed.id.to_string()],
        "the anonymous listing is the listing"
    );

    let with_token = get(&app, Some(&token), "/api/v1/directory").await;
    common::assert_answer(
        &with_token,
        StatusCode::OK,
        "a token the directory does not need",
    );
    assert_eq!(
        listed_ids(&with_token.json_value()),
        listed_ids(&open.json_value()),
        "a viewer who is not in the listing sees the same accounts"
    );
    common::assert_all_refused(
        &[get(&app, Some("not-a-token"), "/api/v1/directory").await],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "a bad token on an open listing is refused, not ignored",
    );
}
