//! The staff suggestions routes and the dismissal route.

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;
use wired_be::models::{
    block,
    mute::{self, MuteOptions},
    suggestion,
};

use super::support::{
    BASE, INVALID_TOKEN, NO_READ, NO_TOKEN, NO_WRITE_ACCOUNTS, NOT_FOUND, USER_REQUIRED,
    deactivate, delete, fixture, get, newest_first, seed_follow, suggested, wrapped_ids,
};
use crate::common;

/// Both suggestion versions answer the staff pool — suggested, local, active
/// accounts — v1 as bare `Account` entities and v2 with each account wrapped
/// in the source object it came from.
#[tokio::test]
async fn suggestions_answer_the_staff_pool_in_both_shapes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "read").await;
    let older = suggested(&pool, "older").await;
    let newer = suggested(&pool, "newer").await;
    let expected = newest_first([older.id, newer.id]);
    let plain = common::create_local_user(&pool, "plain").await;
    let mirror = suggested(&pool, "mirror").await;
    let unlocal = sqlx::query("UPDATE users SET local = false WHERE id = $1")
        .bind(mirror.id)
        .execute(&pool)
        .await
        .expect("the fixture becomes a mirror");
    assert_eq!(
        unlocal.rows_affected(),
        1,
        "the flagged pick is now a remote mirror"
    );
    let closed = suggested(&pool, "closed").await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool);

    let v1 = get(&app, Some(&token), "/api/v1/suggestions").await;
    common::assert_answer(&v1, StatusCode::OK, "the v1 listing");
    let v1_body = v1.json_value();
    assert_eq!(
        common::ids_of(&v1_body),
        expected,
        "the pool, newest account first, as plain accounts"
    );
    let mut names: Vec<&str> = v1_body
        .as_array()
        .expect("the v1 listing is an array")
        .iter()
        .filter_map(|account| account["username"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["newer", "older"],
        "every entry is a plain account entity, rendered from the pool"
    );

    let v2 = get(&app, Some(&token), "/api/v2/suggestions").await;
    common::assert_answer(&v2, StatusCode::OK, "the v2 listing");
    let v2_body = v2.json_value();
    assert_eq!(wrapped_ids(&v2_body), expected, "the same two accounts");
    for entry in v2_body.as_array().expect("the v2 listing is an array") {
        assert_eq!(entry["source"], "staff", "every suggestion is staff-made");
        assert_eq!(
            entry["sources"],
            json!(["staff"]),
            "v2 spells the source as a list too"
        );
        assert!(
            entry["account"]["id"].is_string() && entry["account"]["username"].is_string(),
            "and the account rides inside the wrapper: {entry}"
        );
    }
    for absent in [plain.id, mirror.id, closed.id, alice.id] {
        assert!(
            !common::ids_of(&v1_body).contains(&absent.to_string()),
            "{absent} is in nobody's pool: an unflagged, non-local, \
             deactivated or self account is out"
        );
    }
}

/// The pool is the viewer's own: nobody they follow, block, mute or have
/// dismissed shows up, and one viewer's state changes nobody else's list.
#[tokio::test]
async fn suggestions_exclude_every_relationship_the_viewer_has() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read").await;
    let (_, carol_token) = fixture(&pool, "carol", "read").await;
    let followed = suggested(&pool, "followed").await;
    let blocked = suggested(&pool, "blocked").await;
    let muted = suggested(&pool, "muted").await;
    let dismissed = suggested(&pool, "dismissed").await;
    let kept = suggested(&pool, "kept").await;

    seed_follow(&pool, &alice, &followed).await;
    let block_row = block::block(&pool, BASE, &alice, &blocked)
        .await
        .expect("the fixture block stores");
    assert_eq!(
        block_row.blocked_id, blocked.id,
        "the block is alice's on `blocked`"
    );
    let mute_row = mute::mute(&pool, &alice, &muted, MuteOptions::default())
        .await
        .expect("the fixture mute stores");
    assert_eq!(
        mute_row.muted_id, muted.id,
        "the mute is alice's on `muted`"
    );
    suggestion::dismiss(&pool, alice.id, dismissed.id)
        .await
        .expect("the fixture dismissal stores");
    let app = common::test_app(pool);

    let mine = get(&app, Some(&alice_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&mine),
        vec![kept.id.to_string()],
        "only the account alice has no relationship with is left"
    );
    let theirs = get(&app, Some(&carol_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&theirs).len(),
        5,
        "carol sees the whole pool: alice's relationships are her own business"
    );
}

/// `limit` bounds the suggestion page on both versions, and an unreadable
/// `limit` falls back to the default 40 rather than failing.
#[tokio::test]
async fn suggestions_honour_the_limit_on_both_versions() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read").await;
    let mut picks = Vec::new();
    for nickname in ["one", "two", "three"] {
        let pick = suggested(&pool, nickname).await;
        picks.push(pick.id);
    }
    let expected = newest_first(picks);
    let app = common::test_app(pool);

    let capped = get(&app, Some(&token), "/api/v1/suggestions?limit=2")
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&capped),
        expected[..2],
        "`limit=2` answers the head of the pool"
    );
    let capped_v2 = get(&app, Some(&token), "/api/v2/suggestions?limit=2")
        .await
        .json_value();
    assert_eq!(
        wrapped_ids(&capped_v2),
        expected[..2],
        "v2 pages the same head of the pool"
    );
    let whole = get(&app, Some(&token), "/api/v1/suggestions?limit=nope")
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&whole),
        expected,
        "an unreadable limit is the default page of 40, which holds all three"
    );
}

/// The suggestion routes need a user token that grants `read` (both
/// versions) or `write:accounts` (the dismissal); every other credential is
/// refused the way Pleroma's plugs refuse it.
#[tokio::test]
async fn suggestion_routes_demand_their_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read").await;
    let narrow = common::create_app_and_token(&pool, &alice, "read:statuses").await;
    let write_only = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "read").await;
    let app = common::test_app(pool);
    let dismiss_path = format!("/api/v1/suggestions/{id}", id = alice.id);

    common::assert_all_refused(
        &[
            get(&app, None, "/api/v1/suggestions").await,
            get(&app, None, "/api/v2/suggestions").await,
            delete(&app, "", &dismiss_path).await,
        ],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "the suggestion routes present no credential at all",
    );
    common::assert_all_refused(
        &[
            get(&app, Some("nope"), "/api/v1/suggestions").await,
            get(&app, Some("nope"), "/api/v2/suggestions").await,
            delete(&app, "nope", &dismiss_path).await,
        ],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "and a credential that does not resolve, rather than proceeding anonymously",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&app_only), "/api/v1/suggestions").await,
            get(&app, Some(&app_only), "/api/v2/suggestions").await,
            delete(&app, &app_only, &dismiss_path).await,
        ],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "the suggestion routes on a credential that names no user",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&narrow), "/api/v1/suggestions").await,
            get(&app, Some(&narrow), "/api/v2/suggestions").await,
        ],
        StatusCode::FORBIDDEN,
        NO_READ,
        "a `read:statuses` token does not grant the `read` root scope",
    );
    common::assert_all_refused(
        &[
            get(&app, Some(&write_only), "/api/v1/suggestions").await,
            get(&app, Some(&write_only), "/api/v2/suggestions").await,
        ],
        StatusCode::FORBIDDEN,
        NO_READ,
        "and a write-only token cannot read the pool",
    );
    common::assert_all_refused(
        &[
            delete(&app, &narrow, &dismiss_path).await,
            delete(&app, &read_only, &dismiss_path).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE_ACCOUNTS,
        "the dismissal needs `write:accounts`",
    );
}

/// `DELETE /api/v1/suggestions/{account_id}` records the dismissal and
/// answers the empty object; it is per-viewer, idempotent, and never clears
/// the account's own flag, because the pool is the admins' to curate.
#[tokio::test]
async fn dismissing_a_suggestion_is_per_viewer_and_keeps_the_flag() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let (_, carol_token) = fixture(&pool, "carol", "read").await;
    let pick = suggested(&pool, "pick").await;
    let app = common::test_app(pool.clone());
    let path = format!("/api/v1/suggestions/{id}", id = pick.id);

    let answer = delete(&app, &alice_token, &path).await;
    common::assert_answer(&answer, StatusCode::OK, "the dismissal");
    assert_eq!(answer.text(), "{}", "the answer is the empty JSON object");
    assert_eq!(
        suggestion::dismissed_ids(&pool, alice.id).await.unwrap(),
        vec![pick.id],
        "the pair is what got stored"
    );

    let gone = get(&app, Some(&alice_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(gone, json!([]), "alice stops seeing the account");
    let still = get(&app, Some(&carol_token), "/api/v1/suggestions")
        .await
        .json_value();
    assert_eq!(
        common::ids_of(&still),
        vec![pick.id.to_string()],
        "and carol keeps seeing it, because she dismissed nothing"
    );

    let again = delete(&app, &alice_token, &path).await;
    common::assert_answer(&again, StatusCode::OK, "dismissing twice");
    assert_eq!(again.text(), "{}", "with the same answer");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dismissed_suggestions")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 1, "and still one row");

    let stored: bool = sqlx::query_scalar("SELECT is_suggested FROM users WHERE id = $1")
        .bind(pick.id)
        .fetch_one(&pool)
        .await
        .expect("the flag reads back");
    assert!(stored, "the account stays in the pool itself");
}

/// The dismissal resolves its account like every other route keyed on an id:
/// a malformed id, an unknown one and a deactivated account are all 404, and
/// a refusal records nothing.
#[tokio::test]
async fn dismissing_an_unknown_account_is_not_found() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, token) = fixture(&pool, "alice", "read write").await;
    let closed = suggested(&pool, "closed").await;
    deactivate(&pool, &closed).await;
    let app = common::test_app(pool.clone());

    for path in [
        "/api/v1/suggestions/not-an-id".to_owned(),
        format!("/api/v1/suggestions/{id}", id = Uuid::now_v7()),
        format!("/api/v1/suggestions/{id}", id = closed.id),
    ] {
        common::assert_all_refused(
            &[delete(&app, &token, &path).await],
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &path,
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dismissed_suggestions")
        .fetch_one(&pool)
        .await
        .expect("the count reads");
    assert_eq!(rows, 0, "and no refusal recorded a dismissal");
}
