//! The `AuthUser` + scope pipeline every filter route enforces: no token,
//! a token no row carries and an app-only token answer the 401/401/403
//! gates; reading demands `read:filters` (or `read`) and writing
//! `write:filters` (or `write`), and what was denied stored nothing.

use axum::http::StatusCode;
use serde_json::json;

use super::support::{
    ROOT, create_body, create_json, destroy, fixture, list, listed, phrases, seed, show, update,
};
use crate::common;

/// The credential half of the pipeline: no token at all is the 401 with
/// Pleroma's "Invalid credentials", a token no row carries the 401 naming it,
/// and an app-only token the 403 demanding an authenticated user.
#[tokio::test]
async fn the_credential_gates_refuse_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let app_only = common::create_app_only_token(&pool, "read write").await;
    let app = common::test_app(pool);

    let anonymous = common::TestRequest::get(ROOT).send(&app).await;
    common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "no token is a 401");
    common::assert_error(&anonymous, "Invalid credentials", "the absent token");

    let bogus = [
        list(&app, "not-a-real-token").await,
        show(&app, "not-a-real-token", hers.id).await,
    ];
    common::assert_all_refused(
        &bogus,
        StatusCode::UNAUTHORIZED,
        "The access token is invalid",
        "a token no row carries",
    );

    let userless = [
        list(&app, &app_only).await,
        create_json(&app, &app_only, create_body()).await,
    ];
    common::assert_all_refused(
        &userless,
        StatusCode::FORBIDDEN,
        "This API requires an authenticated user",
        "an app-only token",
    );
}

/// The scope half of the pipeline: reading needs `read:filters` (or `read`)
/// and writing `write:filters` (or `write`); every route a token lacks the
/// scope for answers the 403 naming it.
#[tokio::test]
async fn tokens_without_the_route_scope_are_refused() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, _) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let (_reader, read_only) = fixture(&pool, "reader", "read").await;
    let (_writer, write_broad) = fixture(&pool, "writer", "write").await;
    let (_narrow, wrong_scope) = fixture(&pool, "narrow", "read:statuses write:statuses").await;
    let app = common::test_app(pool);

    for (who, token) in [("read:statuses", &wrong_scope), ("write", &write_broad)] {
        let refused = [list(&app, token).await, show(&app, token, hers.id).await];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: read:filters.",
            &format!("`{who}` on every reading route"),
        );
    }
    for (who, token) in [("read:statuses", &wrong_scope), ("read", &read_only)] {
        let refused = [
            create_json(&app, token, create_body()).await,
            update(
                &app,
                token,
                hers.id,
                json!({"phrase": "x", "context": ["home"]}),
            )
            .await,
            destroy(&app, token, hers.id).await,
        ];
        common::assert_all_refused(
            &refused,
            StatusCode::FORBIDDEN,
            "Insufficient permissions: write:filters.",
            &format!("`{who}` on every writing route"),
        );
    }
}

/// The granted side: the narrow sub-scopes pass on their own routes, the
/// broad `read` and `write` trees pass everywhere, and what was denied really
/// stored nothing.
#[tokio::test]
async fn the_granted_scopes_use_every_route() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, alice_token) = fixture(&pool, "alice", "read write").await;
    let hers = seed(&pool, &alice, "hers", None).await;
    let (_filer, filter_read) = fixture(&pool, "filer", "read:filters").await;
    let (_mutator, filter_write) = fixture(&pool, "mutator", "write:filters").await;
    let app = common::test_app(pool);

    let granted_reads = [
        list(&app, &filter_read).await,
        show(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_reads,
        StatusCode::OK,
        "`read:filters` lists and the broad read shows",
    );
    let rewrite = json!({"phrase": "rewritten", "context": ["home"]});
    let granted_writes = [
        create_json(&app, &filter_write, create_body()).await,
        create_json(&app, &alice_token, create_body()).await,
        update(&app, &alice_token, hers.id, rewrite).await,
        destroy(&app, &alice_token, hers.id).await,
    ];
    common::assert_all_answered(
        &granted_writes,
        StatusCode::OK,
        "`write:filters` creates; the broad write creates, updates and deletes",
    );
    assert_eq!(
        phrases(&listed(&app, &alice_token).await),
        ["Spoilers"],
        "alice's list afterwards: her created row, the deleted one gone"
    );
    assert_eq!(
        phrases(&listed(&app, &filter_read).await),
        Vec::<&str>::new(),
        "while the read-only filer stored nothing of its own"
    );
}
