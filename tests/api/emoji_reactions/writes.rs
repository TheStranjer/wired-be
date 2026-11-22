//! The two writes: reacting and withdrawing a reaction, their emoji
//! validation and their gates.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::{self, NewCustomEmoji},
        status::Visibility,
    },
};

use super::support::{
    BAD_TOKEN, CAT, CAT_URL, INVALID, NO_TOKEN, NO_WRITE, NOT_FOUND, PARTY, THUMB, UNKNOWN_ID,
    USER_REQUIRED, disabled_emoji, emoji, entry_keys, field, fixture, get, public_post, put,
    react_ok, reacted, reader_and_writer, refused, remove, route, segment, stored, summary,
    withdraw_ok,
};
use crate::common;

#[test]
fn the_emoji_segment_arrives_percent_encoded() {
    assert_eq!(segment(THUMB), "%F0%9F%91%8D", "a client's emoji");
    assert_eq!(segment(CAT), ":cat:", "colons ride unencoded");
    assert_eq!(
        segment("%F0%9F%91%8D"),
        "%25F0%259F%2591%258D",
        "a literal percent doubles, so a route that decodes its segment is \
         decoding what the client sent"
    );
}

#[tokio::test]
async fn a_unicode_reaction_answers_the_status_with_the_reaction_on_it() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    let body = reacted(&app, &token, &status_id, THUMB).await;
    assert_eq!(body["id"], status_id.to_string(), "the status itself");
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(THUMB, 1, true, &Value::Null, &[&alice.id])]),
        "the caller's own reaction: counted, flagged, and without an image"
    );
    assert_eq!(
        entry_keys(&body["pleroma"]["emoji_reactions"], 0),
        vec!["account_ids", "count", "me", "name", "url"],
        "Pleroma's five keys on the status-field entry"
    );
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            THUMB.to_owned()
        )],
        "one row, the triple the table keys on"
    );
    let ap_id: Option<String> = sqlx::query_scalar("SELECT ap_id FROM emoji_reactions")
        .fetch_optional(&pool)
        .await
        .expect("the activity id reads");
    let minted = ap_id.as_deref().unwrap_or("no id at all");
    assert!(
        minted.starts_with(&format!("{TEST_BASE_URL}/activities/")),
        "decision 6's local activity id is minted with the row, not {minted}"
    );
}

#[tokio::test]
async fn a_custom_reaction_carries_its_shortcode_and_image() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    emoji(&pool, "cat").await;

    let body = reacted(&app, &token, &status_id, CAT).await;
    assert_eq!(
        body["pleroma"]["emoji_reactions"],
        json!([summary(CAT, 1, true, &json!(CAT_URL), &[&alice.id])]),
        "the shortcode is the name and the emoji's image is the url"
    );

    // An unknown shortcode, another instance's emoji, and a retired one are all
    // the same refusal, and none of them stores a row.
    let remote = common::create_remote_user(&pool, "far", "far.example").await;
    let mirrored = custom_emoji::upsert(
        &pool,
        &NewCustomEmoji {
            shortcode: "far_cat".to_owned(),
            domain: remote.domain.clone(),
            image_url: "https://far.example/emoji/far_cat.png".to_owned(),
            ..NewCustomEmoji::default()
        },
    )
    .await
    .expect("the remote emoji upserts");
    assert_eq!(
        mirrored.domain.as_deref(),
        Some("far.example"),
        "the emoji that refuses is another instance's, not a local one"
    );
    disabled_emoji(&pool, "gone").await;

    let status_text = status_id.to_string();
    for unusable in [":dog:", ":far_cat:", ":gone:"] {
        refused(
            &put(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
        refused(
            &remove(&app, &token, &status_text, unusable).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            unusable,
        );
    }
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id.to_string(), status_text, CAT.to_owned())],
        "the one accepted reaction is the only row"
    );
}

#[tokio::test]
async fn an_emoji_that_is_not_one_emoji_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    let status_text = status_id.to_string();
    let two_thumbs = segment(&format!("{THUMB}{THUMB}"));
    let thumb_and_letter = format!("{}x", segment(THUMB));

    for junk in [
        "cat",
        ":cat",
        "cat:",
        "::",
        ":a b:",
        ":a-b:",
        "hello",
        &two_thumbs,
        &thumb_and_letter,
    ] {
        refused(
            &put(&app, &token, &status_text, junk).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            junk,
        );
        refused(
            &remove(&app, &token, &status_text, junk).await,
            StatusCode::BAD_REQUEST,
            INVALID,
            junk,
        );
    }
    assert!(stored(&pool).await.is_empty(), "nothing usable was stored");
}

#[tokio::test]
async fn reacting_twice_is_one_reaction_and_a_second_emoji_is_a_second_row() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;

    let first = reacted(&app, &token, &status_id, THUMB).await;
    let again = reacted(&app, &token, &status_id, THUMB).await;
    assert_eq!(
        first["pleroma"]["emoji_reactions"], again["pleroma"]["emoji_reactions"],
        "a repeat reaction restates the fact instead of adding to it"
    );
    assert_eq!(
        stored(&pool).await,
        vec![(
            alice.id.to_string(),
            status_id.to_string(),
            THUMB.to_owned()
        )],
        "one row for one pair"
    );

    react_ok(&app, &token, &status_id, PARTY).await;
    assert_eq!(
        stored(&pool).await.len(),
        2,
        "a different emoji of the same account is a second reaction"
    );
    assert_eq!(
        field(&app, Some(&token), &status_id).await,
        json!([
            summary(THUMB, 1, true, &Value::Null, &[&alice.id]),
            summary(PARTY, 1, true, &Value::Null, &[&alice.id]),
        ]),
        "and the roll-up names both, each group ordered by its own first \
         reaction"
    );
}

#[tokio::test]
async fn withdrawing_a_reaction_takes_it_and_repeating_changes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (alice, token) = reader_and_writer(&pool, "alice").await;
    let status_id = public_post(&pool, "bob").await;
    react_ok(&app, &token, &status_id, THUMB).await;
    let status_text = status_id.to_string();

    let after = remove(&app, &token, &status_text, THUMB).await;
    common::assert_answer(&after, StatusCode::OK, "the withdrawal");
    assert_eq!(
        after.json_value()["pleroma"]["emoji_reactions"],
        json!([]),
        "the status answers without the reaction it no longer has"
    );
    assert!(stored(&pool).await.is_empty(), "the row went");

    let repeated = remove(&app, &token, &status_text, THUMB).await;
    common::assert_answer(&repeated, StatusCode::OK, "a repeat withdrawal");
    assert_eq!(
        repeated.json_value()["pleroma"]["emoji_reactions"],
        json!([]),
        "withdrawing what is already gone is the same answer, not an error"
    );
    let stranger = remove(&app, &token, &status_text, PARTY).await;
    common::assert_answer(&stranger, StatusCode::OK, "an emoji never reacted with");
    assert_eq!(
        stranger.json_value()["id"],
        status_id.to_string(),
        "and it still answers the status"
    );
    refused(
        &remove(&app, &token, &status_text, ":never:").await,
        StatusCode::BAD_REQUEST,
        INVALID,
        "a shortcode that names no emoji of ours",
    );

    // The withdrawal is per emoji: another reaction of the same account stays.
    react_ok(&app, &token, &status_id, THUMB).await;
    react_ok(&app, &token, &status_id, PARTY).await;
    withdraw_ok(&app, &token, &status_text, THUMB).await;
    assert_eq!(
        stored(&pool).await,
        vec![(alice.id.to_string(), status_text, PARTY.to_owned())],
        "only the named emoji's row went"
    );
}

#[tokio::test]
async fn the_writes_want_a_user_token_that_writes_statuses() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_reader, read_token) = fixture(&pool, "reader", "read:statuses").await;
    let app_token = common::create_app_only_token(&pool, "write:statuses").await;
    let status_id = public_post(&pool, "bob").await;
    let path = route(&status_id.to_string(), Some(&segment(THUMB)));

    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.send(&app).await,
            StatusCode::UNAUTHORIZED,
            NO_TOKEN,
            "no token at all",
        );
    }
    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.bearer(&app_token).send(&app).await,
            StatusCode::FORBIDDEN,
            USER_REQUIRED,
            "an app-only token",
        );
    }
    for verb in [
        common::TestRequest::put(&path),
        common::TestRequest::delete(&path),
    ] {
        refused(
            &verb.bearer(&read_token).send(&app).await,
            StatusCode::FORBIDDEN,
            NO_WRITE,
            "a user token without write:statuses",
        );
    }
    refused(
        &put(&app, "not-a-token", &status_id.to_string(), THUMB).await,
        StatusCode::UNAUTHORIZED,
        BAD_TOKEN,
        "a token that does not resolve",
    );
    assert!(stored(&pool).await.is_empty(), "no refusal stored anything");
}

#[tokio::test]
async fn a_status_this_route_may_not_answer_is_the_shared_four_zero_four() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_alice, token) = reader_and_writer(&pool, "alice").await;
    let author = common::create_local_user(&pool, "secret").await;
    let private = common::create_status(&pool, &author, "hidden", Visibility::Private)
        .await
        .id
        .to_string();
    let direct = common::create_status(&pool, &author, "only me", Visibility::Direct)
        .await
        .id
        .to_string();
    let doomed = public_post(&pool, "doomed").await;
    let deleted = sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the fixture deletes the status")
        .rows_affected();
    assert_eq!(deleted, 1, "the status was live to delete");

    for target in [
        private,
        direct,
        doomed.to_string(),
        UNKNOWN_ID.to_owned(),
        "junk".to_owned(),
    ] {
        refused(
            &put(&app, &token, &target, THUMB).await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reacting to {target}"),
        );
        refused(
            &remove(&app, &token, &target, THUMB).await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("withdrawing on {target}"),
        );
        refused(
            &get(&app, Some(&token), &target, None, "").await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reading {target}"),
        );
        refused(
            &get(&app, Some(&token), &target, Some(THUMB), "").await,
            StatusCode::NOT_FOUND,
            NOT_FOUND,
            &format!("reading one emoji of {target}"),
        );
    }
    assert!(
        stored(&pool).await.is_empty(),
        "an unreachable status took no reactions"
    );
}
