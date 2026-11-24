//! The page cap of `GET /api/v1/accounts/familiar_followers`: only the first
//! 40 requested ids are answered.

use axum::http::StatusCode;

use super::support::{familiar_path, fixture, get};
use crate::common;

/// The page the batch answers: the listing cap of 40.
const PAGE: usize = 40;

/// Asking about 41 known accounts answers the first 40 of them, in the order
/// asked; the 41st is never read. Asking about exactly 40 answers all 40.
#[tokio::test]
async fn familiar_followers_answers_only_the_first_page_of_ids() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, token) = fixture(&pool, "alice", "read:follows").await;
    let mut asked = Vec::with_capacity(PAGE + 1);
    for n in 0..=PAGE {
        let account = common::create_local_user(&pool, &format!("known{n}")).await;
        asked.push(account.id.to_string());
    }
    let app = common::test_app(pool);

    let over = get(&app, Some(&token), &familiar_path(&asked)).await;
    common::assert_answer(&over, StatusCode::OK, "41 known ids");
    let answered = common::ids_of(&over.json_value());
    assert_eq!(answered.len(), PAGE, "answers a page of 40 entries");
    assert_eq!(
        answered,
        asked[..PAGE].to_vec(),
        "the first 40 ids, in the order asked"
    );

    let exact = get(&app, Some(&token), &familiar_path(&asked[..PAGE])).await;
    common::assert_answer(&exact, StatusCode::OK, "40 known ids");
    assert_eq!(
        common::ids_of(&exact.json_value()),
        asked[..PAGE].to_vec(),
        "exactly one page is answered whole"
    );
}
