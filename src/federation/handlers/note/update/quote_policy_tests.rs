//! Tests of the quote policy the edit fold stores from an `Update`'s
//! `interactionPolicy.canQuote`.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::super::test_support::{app_state, author, author_again, mirrored, note, view_of};
use super::super::update_note;
use crate::{
    federation::{dispatch::Outcome, vocab::AS_PUBLIC},
    models::status::{self, QuotePolicy},
};

/// An `Update` of `uri` to `text`, stamped `updated`, whose `canQuote` is
/// `can_quote` when one is given.
fn edit_with(uri: &str, text: &str, updated: &str, can_quote: Option<Value>) -> Value {
    let mut extra = json!({"content": format!("<p>{text}</p>"), "updated": updated});
    if let Some(policy) = can_quote {
        extra["interactionPolicy"] = json!({"canQuote": policy});
    }
    note(uri, extra)
}

/// The quote policy stored for `uri`.
async fn stored_policy(pool: &PgPool, uri: &str) -> Option<QuotePolicy> {
    status::get_by_uri(pool, uri)
        .await
        .expect("the uri read")
        .expect("the status is stored")
        .quote_policy
        .map(|policy| policy.0)
}

/// The policy that lets anybody quote.
fn everyone() -> QuotePolicy {
    QuotePolicy {
        automatic: vec![AS_PUBLIC.to_owned()],
        manual: Vec::new(),
    }
}

#[tokio::test]
async fn an_update_that_changes_the_policy_updates_the_stored_value() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/policy-edit-1";
    drop(mirrored(&pool, &author, uri, "first words").await);
    assert_eq!(stored_policy(&pool, uri).await, None, "nothing advertised");

    let opened = edit_with(
        uri,
        "second words",
        "2024-05-01T12:00:00.000Z",
        Some(json!({"automaticApproval": [AS_PUBLIC]})),
    );
    let outcome = update_note(
        state.clone(),
        view_of("Update", opened, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");
    assert_eq!(outcome, Outcome::Handled, "the edit is folded");
    assert_eq!(
        stored_policy(&pool, uri).await,
        Some(everyone()),
        "the advertised policy is stored"
    );

    let closed = edit_with(
        uri,
        "third words",
        "2024-05-01T13:00:00.000Z",
        Some(json!({})),
    );
    let second = update_note(
        state,
        view_of("Update", closed, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");
    assert_eq!(second, Outcome::Handled, "the second edit is folded");
    assert_eq!(
        stored_policy(&pool, uri).await,
        Some(QuotePolicy::default()),
        "an empty canQuote replaces it: nobody may quote"
    );
}

#[tokio::test]
async fn a_policy_change_with_unchanged_text_and_time_is_still_stored() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/policy-edit-2";
    drop(mirrored(&pool, &author, uri, "first words").await);
    let first = edit_with(
        uri,
        "second words",
        "2024-05-01T12:00:00.000Z",
        Some(json!({"automaticApproval": [AS_PUBLIC]})),
    );
    let folded = update_note(
        state.clone(),
        view_of("Update", first, None),
        author_again(&pool).await,
    )
    .await
    .expect("the first edit folds");
    assert_eq!(folded, Outcome::Handled, "the first edit applies");

    let restated = edit_with(uri, "second words", "2024-05-01T12:00:00.000Z", None);
    let outcome = update_note(
        state,
        view_of("Update", restated, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");

    assert_eq!(
        outcome,
        Outcome::Ignored("an Update no newer than the stored version"),
        "the text and time are stale"
    );
    assert_eq!(
        stored_policy(&pool, uri).await,
        None,
        "but a document that advertises nothing clears the stored policy"
    );
}

#[tokio::test]
async fn a_stale_update_does_not_touch_the_policy() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let state = app_state(&pool);
    let author = author(&pool).await;
    let uri = "https://remote.example/users/bob/statuses/policy-edit-3";
    drop(mirrored(&pool, &author, uri, "first words").await);
    let newer = edit_with(
        uri,
        "newer words",
        "2024-05-01T12:00:05.000Z",
        Some(json!({"automaticApproval": [AS_PUBLIC]})),
    );
    let folded = update_note(
        state.clone(),
        view_of("Update", newer, None),
        author_again(&pool).await,
    )
    .await
    .expect("the newer edit folds");
    assert_eq!(folded, Outcome::Handled, "the newer edit applies");

    let outdated = edit_with(
        uri,
        "older words",
        "2024-05-01T12:00:00.000Z",
        Some(json!({})),
    );
    let outcome = update_note(
        state,
        view_of("Update", outdated, None),
        author_again(&pool).await,
    )
    .await
    .expect("the fold completes");

    assert_eq!(
        outcome,
        Outcome::Ignored("an Update no newer than the stored version"),
        "the older document is stale"
    );
    assert_eq!(
        stored_policy(&pool, uri).await,
        Some(everyone()),
        "and its policy is not applied"
    );
}
