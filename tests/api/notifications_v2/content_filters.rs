//! The caller's content filters on the grouped API: the `notifications`
//! context applies to `/api/v2/notifications` as it does to v1 — an
//! irreversible match drops the group and its status, a warning match
//! annotates the status — and a filter of another context changes nothing.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use wired_be::models::{
    filter::{self, phrase::NewPhrase},
    status::Visibility,
    user::User,
};

use super::support::{LIST, READ, encoded, expanded_ids, fixture, group_keys, groups, list};
use crate::common;

/// Stores `owner`'s filter for `phrase` in `contexts`, hiding it or not.
async fn make_filter(
    pool: &PgPool,
    owner: &User,
    contexts: &[&str],
    irreversible: bool,
) -> filter::Filter {
    filter::phrase::create_phrase(
        pool,
        owner.id,
        &NewPhrase {
            phrase: "spoiler".to_owned(),
            context: contexts.iter().map(|name| (*name).to_owned()).collect(),
            irreversible,
            ..NewPhrase::default()
        },
    )
    .await
    .expect("the filter fixture creates")
}

/// An irreversible `notifications` filter removes the group, its status and
/// its actors from the listing and from the single-group read, while a
/// group about a clean status stays.
#[tokio::test]
async fn an_irreversible_notifications_filter_drops_the_group_and_its_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let spoiler = common::create_status(&pool, &bob, "a spoiler inside", Visibility::Public).await;
    let clean = common::create_status(&pool, &carol, "nothing here", Visibility::Public).await;
    let hidden = common::create_notification(&pool, &alice, &bob, "mention", Some(&spoiler)).await;
    let _kept = common::create_notification(&pool, &alice, &carol, "mention", Some(&clean)).await;
    let _row = make_filter(&pool, &alice, &["notifications"], true).await;
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    assert_eq!(
        group_keys(&body).len(),
        1,
        "only the group about the clean status is left"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![clean.id.to_string()],
        "the hidden status is not expanded"
    );
    assert_eq!(
        expanded_ids(&body["accounts"]),
        vec![carol.id.to_string()],
        "nor is the actor of the hidden group"
    );

    let key = format!("ungrouped-{}", hidden.id);
    let single = common::TestRequest::get(&format!("{LIST}/{}", encoded(&key)))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&single, StatusCode::OK, "the hidden group's show");
    let single = single.json_value();
    assert!(
        groups(&single).is_empty() && single["statuses"] == json!([]),
        "the single-group read drops it too: {single}"
    );
}

/// A warning filter keeps the group and its status, and flags the status.
#[tokio::test]
async fn a_warning_notifications_filter_annotates_the_status() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let spoiler = common::create_status(&pool, &bob, "a spoiler inside", Visibility::Public).await;
    let _note = common::create_notification(&pool, &alice, &bob, "mention", Some(&spoiler)).await;
    let _row = make_filter(&pool, &alice, &["notifications"], false).await;
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    assert_eq!(group_keys(&body).len(), 1, "the group stays");
    let statuses = body["statuses"].as_array().expect("statuses is a list");
    assert_eq!(statuses.len(), 1, "the status stays");
    let filtered = statuses[0]["filtered"]
        .as_array()
        .expect("filtered is a list");
    assert!(!filtered.is_empty(), "the warning match is reported");
}

/// A filter that applies only to the `home` context leaves the grouped
/// notifications untouched, even when it hides irreversibly.
#[tokio::test]
async fn a_home_only_filter_does_not_touch_the_grouped_notifications() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let bob = common::create_local_user(&pool, "bob").await;
    let spoiler = common::create_status(&pool, &bob, "a spoiler inside", Visibility::Public).await;
    let _note = common::create_notification(&pool, &alice, &bob, "mention", Some(&spoiler)).await;
    let _row = make_filter(&pool, &alice, &["home"], true).await;
    let app = common::test_app(pool);

    let body = list(&app, &token, "").await;
    assert_eq!(group_keys(&body).len(), 1, "the group stays");
    let statuses = body["statuses"].as_array().expect("statuses is a list");
    assert_eq!(statuses.len(), 1, "the status stays");
    assert_eq!(
        statuses[0]["filtered"],
        json!([]),
        "and carries no annotation"
    );
}
