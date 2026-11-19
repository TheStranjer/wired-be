//! The create route's refusals: the weighted length limit at a configured
//! and at the default cap, the empty post, the 422 matrix over every bad
//! parameter in its own wording, and the form body posting exactly like the
//! JSON one.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::{models::status::Visibility, util::format::status_length};

use super::support::{
    BAD_CONTENT_TYPE, BAD_VISIBILITY, EMPTY_STATUS, STATUSES, WRITE, author_now, created_row,
    posted, publish, refused, stored_count, user_with_token,
};
use crate::common;

/// 422 body for a post over `MAX_STATUS_CHARS`.
const OVER_LIMIT: &str = "The status is over the character limit";

/// The length limit is `MAX_STATUS_CHARS` counted by `status_length`, which
/// charges every bare URL 23 characters: a text the raw count would refuse
/// posts, and one the weighted count refuses does not.
#[tokio::test]
async fn the_length_limit_charges_every_url_twenty_three_characters() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let mut config = common::test_config();
    config.max_status_chars = 25;
    let app = common::test_app_with_config(pool.clone(), config);

    let url = "https://example.com/a/path/longer/than/twenty-three";
    assert!(
        url.chars().count() > 25,
        "the fixture URL is on its own past the cap this test sets"
    );

    let text = format!("x {url}");
    let value = posted(&app, &token, json!({"status": &text})).await;
    assert_eq!(
        value["pleroma"]["content"],
        json!({ "text/plain": text }),
        "2 characters plus one URL weighted at 23 is the 25 this instance \
         allows, so the post is stored with the text as written"
    );

    let over = format!("{url} tail");
    refused(&app, &token, json!({"status": over}), OVER_LIMIT).await;
    assert_eq!(
        stored_count(&pool).await,
        1,
        "23 + 5 is over the same limit, and nothing was written for it"
    );
}

/// The same charge at the production default of `MAX_STATUS_CHARS`: two
/// hundred long bare URLs — ten thousand raw characters — weigh 200 × 23
/// plus their separators and post, while three hundred of the same URLs are
/// the 422. The pair is what proves the limit is measured by that weighting,
/// not by the length the body has as written.
#[tokio::test]
async fn the_default_limit_charges_each_url_its_weight_not_its_length() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());
    assert_eq!(
        common::test_config().max_status_chars,
        5000,
        "this test posts under the default limit, not a shrunk one"
    );

    let url = "https://example.com/a/very/long/path?query=string";
    let fit = vec![url; 200].join(" ");
    let over = vec![url; 300].join(" ");
    assert!(
        fit.chars().count() > 5000 && over.chars().count() > 5000,
        "both bodies are far more than 5000 characters as written: {} and {}",
        fit.chars().count(),
        over.chars().count()
    );
    assert!(
        status_length(&fit, 23) < 200 * 24 && status_length(&over, 23) > 5000,
        "the first measures well under 200 × 24 and so fits, the second's \
         300 charges alone pass the cap"
    );

    let value = posted(&app, &token, json!({"status": &fit})).await;
    assert_eq!(
        created_row(&pool, &value).await.raw_text,
        fit,
        "two hundred URLs post, and the row holds every one verbatim"
    );

    refused(&app, &token, json!({"status": over}), OVER_LIMIT).await;
    assert_eq!(
        stored_count(&pool).await,
        1,
        "three hundred of the same URLs are refused, and wrote nothing"
    );
}

/// A post with nothing to say — no `status`, an empty one, a blank one — is
/// 422 in the wording Pleroma uses for an empty payload, and writes nothing.
#[tokio::test]
async fn an_empty_status_is_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for body in [
        json!({}),
        json!({"status": ""}),
        json!({"status": "   "}),
        json!({"status": "\n\t \n"}),
        json!({"status": "  ", "spoiler_text": "a warning is no text"}),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "an empty post");
        common::assert_error(&response, EMPTY_STATUS, &format!("the body `{body}`"));
    }
    assert_eq!(stored_count(&pool).await, 0, "and nothing was written");
}

/// The 422 matrix over HTTP: one probe per refusal the endpoint owes — an
/// unknown `visibility`, an unknown `content_type` (down to Pleroma's own
/// `text/x-markdown` alias), a `spoiler_text` one character past 200, a
/// `language` one past 10 and a `status` that is only whitespace — each 422
/// in its own wording, and none of them reaches the database.
#[tokio::test]
async fn the_422_matrix_refuses_each_bad_parameter_in_its_own_wording() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    for (body, message) in [
        (
            json!({"status": "hi", "visibility": "bogus"}),
            BAD_VISIBILITY,
        ),
        (
            json!({"status": "hi", "content_type": "text/x-markdown"}),
            BAD_CONTENT_TYPE,
        ),
        (
            json!({"status": "hi", "spoiler_text": "x".repeat(201)}),
            "Validation failed: Spoiler text is too long (maximum is 200 characters)",
        ),
        (
            json!({"status": "hi", "language": "a".repeat(11)}),
            "Validation failed: Language is too long (maximum is 10 characters)",
        ),
        (json!({"status": "   \t\n  "}), EMPTY_STATUS),
    ] {
        let response = publish(&app, Some(token.as_str()), body.clone()).await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the refusal for `{body}`"),
        );
        common::assert_error(&response, message, &format!("the wording for `{body}`"));
    }

    assert_eq!(stored_count(&pool).await, 0, "not one of them wrote a row");
    assert_eq!(
        author_now(&pool, alice.id).await.statuses_count,
        0,
        "and none of them counted for the author"
    );
}

/// A form body creates exactly what the same keys would as JSON, with the
/// string spellings of booleans and defaults accepted (decision 10).
#[tokio::test]
async fn a_form_body_posts_like_the_json_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = user_with_token(&pool, "alice", WRITE).await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[
            ("status", "posted from a form"),
            ("visibility", "unlisted"),
            ("sensitive", "true"),
            ("spoiler_text", "news"),
            ("language", "nl"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "a form post");
    let value = response.json_value();
    let row = created_row(&pool, &value).await;

    assert_eq!(value["content"], "<p>posted from a form</p>", "the text");
    assert_eq!(value["visibility"], "unlisted", "the scope");
    assert_eq!(value["sensitive"], true, "a form `true` is the flag on");
    assert_eq!(value["spoiler_text"], "news", "the content warning");
    assert_eq!(value["language"], "nl", "and the language tag");
    assert_eq!(
        (
            row.raw_text.as_str(),
            row.visibility,
            row.sensitive,
            row.spoiler_text.as_str(),
            row.language.as_deref(),
        ),
        (
            "posted from a form",
            Visibility::Unlisted,
            true,
            "news",
            Some("nl")
        ),
        "stored exactly as the JSON body would have stored it"
    );

    let off = common::TestRequest::post(STATUSES)
        .bearer(&token)
        .form(&[("status", "flag off"), ("sensitive", "0")])
        .send(&app)
        .await;
    common::assert_answer(&off, StatusCode::OK, "the second form post");
    assert_eq!(
        off.json_value()["sensitive"],
        false,
        "`0` is the flag off, whatever the account defaults to"
    );
}
