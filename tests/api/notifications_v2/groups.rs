//! The single-group routes: `GET {group_key}` answers the one group in the
//! listing's envelope, an unknown or foreign key is the 404, `GET
//! {group_key}/accounts` names every actor of the group once, and `POST
//! {group_key}/dismiss` hides the group from every read.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::models::status::Visibility;

use super::support::{
    LIST, NOT_FOUND, READ, WRITE, encoded, expanded_ids, favourite, favourite_note, fixture,
    group_keys, groups, list, unread,
};
use crate::common;

/// `GET /api/v2/notifications/{group_key}` answers the one group in the
/// same envelope, its whole-group bounds serving as the page bounds — and
/// a key the caller owns nothing under, foreign or unknown, is the 404.
#[tokio::test]
async fn a_single_group_is_the_listing_under_one_key() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &erin, "shown", Visibility::Public).await;
    let older = favourite(&pool, &alice, &bob, &post).await;
    let newer = favourite(&pool, &alice, &carol, &post).await;
    let alone = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let app = common::test_app(pool);

    let key = format!("favourite-{}", post.id);
    let response = common::TestRequest::get(&format!("{LIST}/{}", encoded(&key)))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a known group");
    let body = response.json_value();
    let listed = groups(&body);
    assert_eq!(listed.len(), 1, "the one group");
    assert_eq!(listed[0]["group_key"], key.as_str(), "under its key");
    assert_eq!(listed[0]["notifications_count"], 2, "the whole group");
    assert_eq!(
        listed[0]["most_recent_notification_id"],
        newer.id.to_string(),
        "newest id"
    );
    assert_eq!(
        listed[0]["page_min_id"],
        older.id.to_string(),
        "whole-group min"
    );
    assert_eq!(
        listed[0]["page_max_id"],
        newer.id.to_string(),
        "whole-group max"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![post.id.to_string()],
        "with the group's status expanded"
    );
    assert_eq!(
        expanded_ids(&body["accounts"]),
        vec![carol.id.to_string(), bob.id.to_string()],
        "and its samples"
    );

    let ungrouped = common::TestRequest::get(&format!("{LIST}/ungrouped-{}", alone.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&ungrouped, StatusCode::OK, "an ungrouped key");
    let alone_body = ungrouped.json_value();
    let single = groups(&alone_body);
    assert_eq!(single.len(), 1, "its own one-member group");
    assert_eq!(single[0]["notifications_count"], 1, "counted once");
    assert_eq!(
        single[0]["page_min_id"],
        alone.id.to_string(),
        "and bounded by itself"
    );
}

/// `GET {group_key}` 404s a key that names nothing — an unknown group, a
/// broken `ungrouped-` prefix, and another account's group alike.
#[tokio::test]
async fn unknown_and_foreign_group_keys_are_the_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let (_mallory, mallory_token) = fixture(&pool, "mallory", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let post = common::create_status(&pool, &erin, "private show", Visibility::Public).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    let app = common::test_app(pool);

    for (case, path) in [
        (
            "unknown key",
            format!(
                "{LIST}/favourite-{}",
                Uuid::parse_str("01890b9a-2c1d-7f3a-9e4b-7c2d5a8b1e3f").expect("fixture id")
            ),
        ),
        (
            "broken ungrouped prefix",
            format!("{LIST}/ungrouped-not-an-id"),
        ),
        (
            "foreign group",
            format!("{LIST}/{}", encoded(&format!("favourite-{}", post.id))),
        ),
    ] {
        let token_of = if case == "foreign group" {
            &mallory_token
        } else {
            &token
        };
        let missing = common::TestRequest::get(&path)
            .bearer(token_of)
            .send(&app)
            .await;
        common::assert_answer(&missing, StatusCode::NOT_FOUND, case);
        common::assert_error(&missing, NOT_FOUND, case);
    }
}

/// `GET {group_key}/accounts` answers every actor of the group — not just
/// the eight the sample shows — newest actor first, each once; an unknown
/// key answers the empty list, Pleroma's render of no users.
#[tokio::test]
async fn group_accounts_name_every_actor_once() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", READ).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let post = common::create_status(&pool, &erin, "all of them", Visibility::Public).await;
    favourite_note(&pool, &alice, &carol, &post).await;
    favourite_note(&pool, &alice, &bob, &post).await;
    favourite_note(&pool, &alice, &carol, &post).await;
    let alone = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    let mut crowd = Vec::new();
    for index in 0..9 {
        let fan = common::create_local_user(&pool, &format!("fan{index}")).await;
        favourite_note(&pool, &alice, &fan, &post).await;
        crowd.push(fan);
    }
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "{LIST}/{}/accounts",
        encoded(&format!("favourite-{}", post.id))
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&response, StatusCode::OK, "the group's accounts");
    let accounts = response.json_value();
    let listed = expanded_ids(&accounts);
    assert_eq!(
        listed.len(),
        crowd.len() + 2,
        "every distinct actor, past the sample's eight"
    );
    assert_eq!(
        listed.first().map(String::as_str),
        crowd.last().map(|fan| fan.id.to_string()).as_deref(),
        "newest actor first"
    );
    assert_eq!(
        accounts[0]["id"],
        crowd.last().expect("the newest fan").id.to_string(),
        "as full Account entities"
    );
    let mut seen: Vec<String> = Vec::new();
    for id in &listed {
        assert!(!seen.contains(id), "{id} named twice");
        seen.push(id.clone());
    }

    let ungrouped = common::TestRequest::get(&format!("{LIST}/ungrouped-{}/accounts", alone.id))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&ungrouped, StatusCode::OK, "an ungrouped key's actor");
    assert_eq!(
        expanded_ids(&ungrouped.json_value()),
        vec![bob.id.to_string()],
        "the one member's account"
    );

    let unknown = common::TestRequest::get(&format!(
        "{LIST}/{}/accounts",
        encoded(&format!("favourite-{}", Uuid::now_v7()))
    ))
    .bearer(&token)
    .send(&app)
    .await;
    common::assert_answer(&unknown, StatusCode::OK, "an unknown key's accounts");
    assert_eq!(
        unknown.json_value(),
        json!([]),
        "Pleroma's empty render, not a 404"
    );
}

/// `POST {group_key}/dismiss` hides the whole group from every read — the
/// listing, the single-group show, the count — and answers `{}`; the
/// ungrouped spelling dismisses its one row, and an unknown key changes
/// nothing without complaining.
#[tokio::test]
async fn dismissing_a_group_hides_it_from_every_read() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read_token) = fixture(&pool, "alice", READ).await;
    let write_token = common::create_app_and_token(&pool, &alice, WRITE).await;
    let erin = common::create_local_user(&pool, "erin").await;
    let bob = common::create_local_user(&pool, "bob").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let gone = common::create_status(&pool, &erin, "dismissed", Visibility::Public).await;
    let kept = common::create_status(&pool, &erin, "kept", Visibility::Public).await;
    let follow = common::create_notification(&pool, &alice, &bob, "follow", None).await;
    favourite_note(&pool, &alice, &bob, &gone).await;
    favourite_note(&pool, &alice, &erin, &gone).await;
    let kept_note = favourite(&pool, &alice, &carol, &kept).await;
    let app = common::test_app(pool.clone());

    let dismiss_path = encoded(&format!("favourite-{}", gone.id));
    let response = common::TestRequest::post(&format!("{LIST}/{dismiss_path}/dismiss"))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the dismissal");
    assert_eq!(
        response.json_value(),
        json!({}),
        "the empty object, Pleroma's `json(conn, %{{}})`"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "and a JSON body"
    );

    let body = list(&app, &read_token, "").await;
    assert_eq!(
        group_keys(&body),
        vec![
            format!("favourite-{}", kept.id),
            format!("ungrouped-{}", follow.id),
        ],
        "the dismissed group is off the listing, the follow stays"
    );
    assert_eq!(
        expanded_ids(&body["statuses"]),
        vec![kept.id.to_string()],
        "and the dismissed group's status is no longer expanded"
    );
    assert_eq!(
        unread(&app, &read_token, "").await,
        2,
        "the follow and the kept favourite are all that is left unread"
    );

    let shown = common::TestRequest::get(&format!("{LIST}/{dismiss_path}"))
        .bearer(&read_token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::NOT_FOUND, "a dismissed group");

    let again = common::TestRequest::post(&format!("{LIST}/{dismiss_path}/dismiss"))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "re-dismissing is not an error");

    let one_row = common::TestRequest::post(&format!("{LIST}/ungrouped-{}/dismiss", kept_note.id))
        .bearer(&write_token)
        .send(&app)
        .await;
    common::assert_answer(&one_row, StatusCode::OK, "dismissing one row by its id key");

    let unknown = common::TestRequest::post(&format!(
        "{LIST}/{}/dismiss",
        encoded(&format!("reblog-{}", Uuid::now_v7()))
    ))
    .bearer(&write_token)
    .send(&app)
    .await;
    common::assert_answer(&unknown, StatusCode::OK, "dismissing an unknown key");

    let final_body = list(&app, &read_token, "").await;
    assert_eq!(
        group_keys(&final_body),
        vec![format!("ungrouped-{}", follow.id)],
        "the kept group lost its only member to the ungrouped dismissal, \
         and the follow outlives all of it"
    );
}
