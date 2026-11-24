//! The cap on the recipient array of Pleroma's conversation rewrite
//! (`PATCH /api/v1/pleroma/conversations/{id}`): more than 200 distinct ids
//! is the 422 `Too many ids` before anything is read or written; exactly 200
//! is served.

use super::*;

/// The 422 body of an id array past the cap.
const TOO_MANY_IDS: &str = "Too many ids";
/// The most distinct ids one request may name.
const MAX_BATCH_IDS: usize = 200;

/// `count` distinct ids that name no account.
fn unknown_ids(count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| Uuid::from_u128(u128::try_from(n).expect("a small count") << 64).to_string())
        .collect()
}

/// `PATCH`es the rewrite of participation `row_id` with the JSON `recipients`.
async fn rewrite(
    app: &Router,
    token: &str,
    row_id: &str,
    recipients: &[String],
) -> common::TestResponse {
    request("PATCH", &format!("/api/v1/pleroma/conversations/{row_id}"))
        .bearer(token)
        .json(&json!({ "recipients": recipients }))
        .send(app)
        .await
}

/// 201 distinct ids is refused whether or not they name accounts, and the
/// stored recipients stay as they were.
#[tokio::test]
async fn a_rewrite_past_the_cap_is_refused_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let row_id = participation_for(&pool, &alice, &post).await;
    let mine = Uuid::parse_str(&row_id).expect("fixture id");

    let mut recipients = vec![bob.id.to_string()];
    recipients.extend(unknown_ids(MAX_BATCH_IDS));
    assert_eq!(recipients.len(), MAX_BATCH_IDS + 1, "one past the cap");
    let refused = rewrite(&app, &alice_token, &row_id, &recipients).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the rewrite is a 422"
    );
    assert_eq!(
        refused.json_value(),
        json!({"error": TOO_MANY_IDS}),
        "with the cap's body"
    );
    assert_eq!(
        conversation::recipients(&pool, mine)
            .await
            .expect("the list reads"),
        vec![bob.id],
        "and the stored recipients are unchanged"
    );
}

/// Exactly 200 distinct ids is served and stored, and the count is of
/// distinct ids: naming each twice is still 200.
#[tokio::test]
async fn a_rewrite_of_exactly_the_cap_succeeds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = actor(&pool, "alice", BOTH).await;
    let (bob, _) = actor(&pool, "bob", BOTH).await;
    let mut named = Vec::with_capacity(MAX_BATCH_IDS);
    for n in 0..MAX_BATCH_IDS {
        let (recipient, _) = actor(&pool, &format!("batch{n}"), BOTH).await;
        named.push(recipient.id);
    }
    let app = common::test_app(pool.clone());
    let post = dm(&pool, &alice, &[&bob], "@bob psst").await;
    let row_id = participation_for(&pool, &alice, &post).await;
    let mine = Uuid::parse_str(&row_id).expect("fixture id");

    let mut texts: Vec<String> = named.iter().map(ToString::to_string).collect();
    texts.extend(texts.clone());
    let served = rewrite(&app, &alice_token, &row_id, &texts).await;
    common::assert_answer(&served, StatusCode::OK, "200 distinct ids");

    let mut stored = conversation::recipients(&pool, mine)
        .await
        .expect("the list reads");
    stored.sort();
    named.sort();
    assert_eq!(stored, named, "all 200 are stored, once each");
}
