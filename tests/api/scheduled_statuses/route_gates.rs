//! Ownership and the scope pair on the scheduled-status routes: another
//! account's schedule is invisible and unreachable for every verb; no
//! token is 401 and the mismatched scopes are 403 before a row or a
//! moment is looked at; and the roots of the two scope trees reach their
//! own account's schedules and nothing else.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::util::time::mastodon_time;

use super::routes_support::{
    NO_READ, SCHEDULES, assert_one_fire, cancel, day, entity_ids, list, manager, move_to, parked,
    seeded_text, show,
};
use super::support::{NO_TOKEN, NO_WRITE, NOT_FOUND, scheduled_count, stored_schedule};
use crate::common;

/// Ownership is the whole gate on these routes: another account's schedule is
/// invisible in the listing and unreachable by id for every verb, so nobody
/// reads, moves or cancels a post that is not theirs.
#[tokio::test]
async fn one_account_cannot_reach_anothers_schedule() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (_alice, _alice_read, alice_write) = manager(&pool, "alice").await;
    let (_bob, bob_read, bob_write) = manager(&pool, "bob").await;
    let app = common::test_app(pool.clone());
    let row = parked(&app, &pool, &alice_write, "alice's", day(3)).await;
    let id = row.id.to_string();

    assert!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()).is_empty(),
        "bob's listing does not carry alice's schedule"
    );
    common::assert_all_refused(
        &[
            show(&app, &bob_read, &id).await,
            cancel(&app, &bob_write, &id).await,
            move_to(
                &app,
                &bob_write,
                &id,
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "a stranger at alice's schedule",
    );

    let intact = stored_schedule(&pool, row.id).await;
    assert_eq!(
        intact.scheduled_at,
        day(3),
        "alice's schedule stands where she left it"
    );
    assert_eq!(
        intact.params["status"], "alice's",
        "and still parks the post she parked"
    );
    assert_one_fire(&pool, row.id, day(3)).await;
    assert_eq!(
        entity_ids(&list(&app, &bob_read, "").await.json_value()).len(),
        0,
        "still nothing of alice's in bob's page"
    );
}

/// No token is the extractor's 401 on all four routes, and Pleroma's scope
/// pair refuses the mismatched tokens before either the row or the moment is
/// looked at: `read:statuses` to look, `write:statuses` to move and cancel.
#[tokio::test]
async fn the_routes_refuse_anonymous_and_out_of_scope_tokens() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, read, write) = manager(&pool, "alice").await;
    let row = seeded_text(&pool, &alice, day(3), "gated").await;
    let id = row.id.to_string();
    let app = common::test_app(pool.clone());

    common::assert_all_refused(
        &[
            common::TestRequest::get(SCHEDULES).send(&app).await,
            common::TestRequest::get(&format!("{SCHEDULES}/{id}"))
                .send(&app)
                .await,
            common::TestRequest::put(&format!("{SCHEDULES}/{id}"))
                .json(&json!({"scheduled_at": mastodon_time(&day(9))}))
                .send(&app)
                .await,
            common::TestRequest::delete(&format!("{SCHEDULES}/{id}"))
                .send(&app)
                .await,
        ],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "no token at all",
    );
    common::assert_all_refused(
        &[list(&app, &write, "").await, show(&app, &write, &id).await],
        StatusCode::FORBIDDEN,
        NO_READ,
        "a write-only token looking",
    );
    common::assert_all_refused(
        &[
            move_to(
                &app,
                &read,
                &id,
                json!({"scheduled_at": mastodon_time(&day(9))}),
            )
            .await,
            cancel(&app, &read, &id).await,
        ],
        StatusCode::FORBIDDEN,
        NO_WRITE,
        "a read-only token changing",
    );

    assert_eq!(
        scheduled_count(&pool).await,
        1,
        "every refusal above stopped at its gate: the schedule still stands"
    );
    assert_eq!(
        entity_ids(&list(&app, &read, "").await.json_value()).len(),
        1,
        "and the token that does hold the scope still reads it"
    );
}

/// The roots of the two scope trees satisfy their children here, and satisfy
/// nothing else: a `read` token reads its own account's schedules, a `write`
/// token moves and cancels its own, and either one is the shared 404 on
/// somebody else's row.
#[tokio::test]
async fn the_root_scopes_reach_their_own_accounts_schedules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _read, _write) = manager(&pool, "alice").await;
    let theirs = seeded_text(&pool, &alice, day(3), "alice's own").await;
    let dora = common::create_local_user(&pool, "dora").await;
    let dora_read = common::create_app_and_token(&pool, &dora, "read").await;
    let dora_row = seeded_text(&pool, &dora, day(4), "dora's own").await;
    let erin = common::create_local_user(&pool, "erin").await;
    let erin_write = common::create_app_and_token(&pool, &erin, "write").await;
    let erin_row = seeded_text(&pool, &erin, day(5), "erin's own").await;
    let erin_other = seeded_text(&pool, &erin, day(6), "erin's other").await;
    let app = common::test_app(pool.clone());

    let listed = list(&app, &dora_read, "").await;
    common::assert_answer(&listed, StatusCode::OK, "a root `read` token listing");
    assert_eq!(
        entity_ids(&listed.json_value()),
        vec![dora_row.id.to_string()],
        "and it lists only what its own account owns"
    );
    common::assert_answer(
        &show(&app, &dora_read, &dora_row.id.to_string()).await,
        StatusCode::OK,
        "the same token reading one of its own",
    );

    common::assert_answer(
        &move_to(
            &app,
            &erin_write,
            &erin_row.id.to_string(),
            json!({"scheduled_at": mastodon_time(&day(7))}),
        )
        .await,
        StatusCode::OK,
        "a root `write` token moving its own schedule",
    );
    assert_eq!(
        stored_schedule(&pool, erin_row.id).await.scheduled_at,
        day(7),
        "and the row moved"
    );
    common::assert_answer(
        &cancel(&app, &erin_write, &erin_other.id.to_string()).await,
        StatusCode::OK,
        "a root `write` token cancelling its own schedule",
    );

    common::assert_all_refused(
        &[
            show(&app, &dora_read, &theirs.id.to_string()).await,
            cancel(&app, &erin_write, &theirs.id.to_string()).await,
        ],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "alice's row, whatever the root scope",
    );
    assert_eq!(
        scheduled_count(&pool).await,
        3,
        "one cancel went through, and it was erin's own"
    );
}
