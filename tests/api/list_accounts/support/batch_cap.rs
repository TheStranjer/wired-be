//! The cap on the `account_ids` array of the add and remove routes: more than
//! 200 distinct ids is the 422 `Too many ids` before anything is read or
//! written; exactly 200 is served.

use super::*;

/// The 422 body of an id array past the cap.
const TOO_MANY_IDS: &str = "Too many ids";
/// The most distinct ids one request may name.
const MAX_BATCH_IDS: usize = 200;

/// `count` distinct ids that name no account (they need not: the cap is
/// checked before any read).
fn unknown_ids(count: usize) -> Vec<Uuid> {
    (1..=count)
        .map(|n| Uuid::from_u128(u128::try_from(n).expect("a small count") << 64))
        .collect()
}

/// `count` fresh local accounts `scene`'s owner follows, in creation order.
async fn followed_accounts(pool: &PgPool, scene: &Scene, count: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let account = user(pool, &format!("batch{n}")).await;
        follow_user(pool, &scene.alice, &account).await;
        ids.push(account.id);
    }
    ids
}

/// Asserts `response` is the cap's 422 with its exact body.
fn assert_too_many(response: &common::TestResponse, what: &str) {
    assert_eq!(
        response.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{what} is a 422"
    );
    assert_eq!(
        response.json_value(),
        json!({"error": TOO_MANY_IDS}),
        "with the cap's body for {what}"
    );
}

/// 201 distinct ids is refused and nothing is written — even though the
/// unknown ids would otherwise be a 404 — in both body spellings.
#[tokio::test]
async fn adding_past_the_cap_is_refused_and_writes_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);

    let mut ids = scene.ids();
    ids.extend(unknown_ids(MAX_BATCH_IDS + 1 - ids.len()));
    assert_eq!(
        ids.len(),
        MAX_BATCH_IDS + 1,
        "the fixture is one past the cap"
    );

    let json_body = add_json(&app, &scene.token, scene.list.id, account_ids(&ids)).await;
    assert_too_many(&json_body, "the JSON add");
    let form_body = add_form(&app, &scene.token, scene.list.id, &ids).await;
    assert_too_many(&form_body, "the form add");

    assert_members(&app, &scene, &[], "a refused batch added nobody").await;
}

/// Exactly 200 distinct ids is served, and a batch is counted by distinct ids:
/// naming each of them twice is still 200.
#[tokio::test]
async fn adding_exactly_the_cap_succeeds() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let ids = followed_accounts(&pool, &scene, MAX_BATCH_IDS).await;
    let app = common::test_app(pool);

    let mut doubled = ids.clone();
    doubled.extend(ids);
    let response = add_json(&app, &scene.token, scene.list.id, account_ids(&doubled)).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "200 distinct ids are served"
    );
    assert_eq!(response.json_value(), json!({}), "with the empty object");

    let members = listed(&app, &scene.token, scene.list.id, "limit=0").await;
    assert_eq!(members.len(), MAX_BATCH_IDS, "and all of them joined");
}

/// 201 distinct ids on a removal is the same 422 and removes nobody; exactly
/// 200 removes the member it names.
#[tokio::test]
async fn removing_is_capped_the_same_way() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let scene = Scene::new(&pool).await;
    let app = common::test_app(pool);
    added(&app, &scene.token, scene.list.id, &[scene.bob.id]).await;

    let mut ids = vec![scene.bob.id];
    ids.extend(unknown_ids(MAX_BATCH_IDS));
    assert_eq!(
        ids.len(),
        MAX_BATCH_IDS + 1,
        "the fixture is one past the cap"
    );

    let refused = removed(&app, &scene, &ids).await;
    assert_too_many(&refused, "the JSON removal");
    let texts = account_ids_text(&ids);
    let query: Vec<String> = texts
        .iter()
        .map(|text| format!("account_ids[]={text}"))
        .collect();
    let refused_query = remove_query(&app, &scene.token, scene.list.id, &query.join("&")).await;
    assert_too_many(&refused_query, "the query removal");
    assert_members(&app, &scene, &["bob"], "a refused removal removed nobody").await;

    ids.truncate(MAX_BATCH_IDS);
    let served = removed(&app, &scene, &ids).await;
    assert_eq!(served.status, StatusCode::OK, "200 distinct ids are served");
    assert_members(&app, &scene, &[], "and bob is gone").await;
}
