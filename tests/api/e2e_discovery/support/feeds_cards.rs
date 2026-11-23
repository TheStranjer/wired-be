//! The rich media card job.

use super::*;

/// Scenario 7, the link card: a mock page answers `/page` with Open Graph
/// tags; a status linking to it renders `card: null` until the job queue
/// is drained, and then carries the cached card on the show route and the
/// standalone card endpoint alike.
#[tokio::test]
async fn the_rich_media_job_turns_a_linked_page_into_a_card() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_, alice_token) = user(&pool, "alice").await;
    let peer = MockPeer::start().await;
    let (app, state) = app_and_state(pool);

    let page = format!("{}/page", peer.base);
    let status = published(&app, &alice_token, &format!("look at {page}"), "public").await;
    let id = id_of(&status);

    let before = read(&app, &alice_token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&before, StatusCode::OK, "the fresh post reads");
    assert_eq!(
        before.json_value()["card"],
        Value::Null,
        "no card exists before the jobs run"
    );

    let _ran = common::run_all_jobs(&state).await;

    let shown = read(&app, &alice_token, &format!("{STATUSES}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "the carded post reads");
    let card = shown.json_value()["card"].clone();
    assert_eq!(card["url"], json!(page), "the card names the page");
    assert_eq!(card["title"], json!("Mock og Title"), "the og:title");
    assert_eq!(
        card["image"],
        json!(format!("{}/og.png", peer.base)),
        "the og:image as the absolute URL the page wrote"
    );
    assert_eq!(
        card["type"],
        json!("link"),
        "Pleroma's card type is fixed to link"
    );
    assert_eq!(
        card["pleroma"]["opengraph"]["og:type"],
        json!("article"),
        "the raw og:type rides under pleroma.opengraph"
    );
    assert_eq!(
        card["description"],
        json!("Mock og description"),
        "the og:description"
    );
    let standalone = read(&app, &alice_token, &format!("{STATUSES}/{id}/card")).await;
    common::assert_answer(&standalone, StatusCode::OK, "the card endpoint answers");
    assert_eq!(standalone.json_value(), card, "with the very same JSON");
    assert!(
        peer.page_hits() >= 1,
        "the mock page was fetched by the job"
    );
}
