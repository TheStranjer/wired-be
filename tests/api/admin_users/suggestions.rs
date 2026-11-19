//! The suggestion pool of WBE-197: `PATCH /users/suggest` setting the flag
//! and answering the moved rows as admin entities under the one `users`
//! key, `PATCH /users/unsuggest` emptying the pool again, the public
//! `/api/v1/suggestions` listing following the flag both ways — live local
//! accounts only — and the refusals that write nothing.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::support::{
    NO_ADMIN_WRITE, SUGGEST, UNSUGGEST, account_named, answered_users, flag_of, logged, mint,
    one_entry, patch_json, plain, read, seed, write_token,
};
use crate::common;

/// The ids one `/api/v1/suggestions` answer carries, in order.
fn suggested_ids(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("the suggestion array")
        .iter()
        .filter_map(|found| found["id"].as_str())
        .map(str::to_owned)
        .collect()
}

/// `PATCH /users/suggest` sets the flag on every account it resolved and
/// answers the moved rows as admin entities under the one `users` key — the
/// order the request named, the miss gone — and lands Pleroma's
/// `add_suggestion` sentence.
#[tokio::test]
async fn suggesting_users_answers_the_moved_accounts() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let body = json!({"nicknames": ["plain", "suspended", "ghost", "beeper"]});

    let suggested = patch_json(&seeded.app, &token, SUGGEST, &body).await;
    common::assert_answer(&suggested, StatusCode::OK, "the suggest write");
    let answered = suggested.json_value();
    assert_eq!(
        answered
            .as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        vec!["users"],
        "the one key Pleroma's index.json clause renders"
    );
    assert_eq!(
        answered_users(&answered),
        vec!["plain", "suspended", "beeper"],
        "the accounts that were found, in the order the request named them, the miss gone"
    );
    for entry in answered["users"].as_array().expect("the users array") {
        assert_eq!(
            entry["is_suggested"],
            json!(true),
            "each answer carries the flag the write set: {entry}"
        );
        assert!(
            entry["roles"].is_object() && entry["created_at"].is_string(),
            "and the answer is the admin entity, not a bare account: {entry}"
        );
    }

    let (added, added_subjects) = one_entry(&seeded.pool, "add_suggestion").await;
    assert_eq!(
        added, "@root added suggested users: @plain, @suspended, @beeper",
        "Pleroma's add_suggestion sentence"
    );
    assert_eq!(
        added_subjects,
        vec!["plain", "suspended", "beeper"],
        "and its subject list"
    );
}

/// The listings follow the flag both ways: `PATCH /users/suggest` puts the
/// live local picks into `/api/v1/suggestions` and leaves the deactivated
/// one out of it though its row is set, and `PATCH /users/unsuggest` empties
/// the pool again while answering the rows as they now stand.
#[tokio::test]
async fn the_suggestion_pool_follows_the_flag_both_ways() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let viewer = common::create_local_user(&seeded.pool, "watcher").await;
    let watching = mint(&seeded.pool, &viewer, "read").await;
    let names = json!({"nicknames": ["plain", "suspended", "beeper"]});
    let suggested = patch_json(&seeded.app, &token, SUGGEST, &names).await;
    common::assert_answer(&suggested, StatusCode::OK, "the suggest write");

    let pool = suggested_ids(&read(&seeded.app, &watching, "/api/v1/suggestions").await);
    let plain = plain(&seeded.pool).await;
    let beeper = account_named(&seeded.pool, "beeper").await;
    let suspended = account_named(&seeded.pool, "suspended").await;
    assert!(
        pool.contains(&plain.id.to_string()) && pool.contains(&beeper.id.to_string()),
        "both live local picks are in the listing the pool answers: {pool:?}"
    );
    assert!(
        !pool.contains(&suspended.id.to_string()),
        "the flagged-but-deactivated account is not, even though its flag is set"
    );
    assert!(
        flag_of(
            &seeded.pool,
            "SELECT is_suggested FROM users WHERE id = $1",
            &suspended
        )
        .await,
        "and the write did reach its row"
    );

    let unsuggested = patch_json(
        &seeded.app,
        &token,
        UNSUGGEST,
        &json!({"nicknames": ["plain", "beeper"]}),
    )
    .await;
    common::assert_answer(&unsuggested, StatusCode::OK, "the unsuggest write");
    assert_eq!(
        answered_users(&unsuggested.json_value()),
        vec!["plain", "beeper"],
        "the same pair of accounts, now leaving"
    );
    assert_eq!(
        unsuggested.json_value()["users"][0]["is_suggested"],
        json!(false),
        "answered as the row stands after the write"
    );
    let emptied = suggested_ids(&read(&seeded.app, &watching, "/api/v1/suggestions").await);
    assert_eq!(
        emptied,
        Vec::<String>::new(),
        "the pool the viewer sees is empty again"
    );

    let (removed, _) = one_entry(&seeded.pool, "remove_suggestion").await;
    assert_eq!(
        removed, "@root removed suggested users: @plain, @beeper",
        "and the pair's other sentence"
    );
}

/// The suggest pair writes nothing it was not asked to: a body that states no
/// list is the 422, the read scope is the write class's 403, and neither
/// leaves an entry behind.
#[tokio::test]
async fn a_suggest_write_refuses_what_it_cannot_write() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let body = json!({"nicknames": ["plain"]});

    let bare = patch_json(&seeded.app, &token, SUGGEST, &json!({})).await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a body with no list",
    );
    assert!(
        bare.text().contains("nicknames"),
        "and the 422 names the key it needs: {}",
        bare.text()
    );

    let refused = patch_json(&seeded.app, &seeded.token, UNSUGGEST, &body).await;
    common::assert_answer(
        &refused,
        StatusCode::FORBIDDEN,
        "the read scope must not write",
    );
    common::assert_error(&refused, NO_ADMIN_WRITE, "in the write class's words");
    assert!(
        logged(&seeded.pool, "add_suggestion").await.is_empty()
            && logged(&seeded.pool, "remove_suggestion").await.is_empty(),
        "and the refused writes left no entry behind"
    );
}
