//! The `delete_account` and `disable_account` routes over HTTP, and the
//! auth and scope checks of all four credential and departure utilities.

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::status::Visibility;

use super::support::{
    CHANGE_EMAIL, CHANGE_PASSWORD, DELETE_ACCOUNT, DISABLE_ACCOUNT, NEW_PASSWORD, PASSWORD,
    USERS_DELETE, fixture, post_json, queued_jobs, signed_in, stored, token_count,
};
use crate::common;

/// `delete_account` success: the account goes inactive with `deleted_at`
/// stamped, every token — the presenting one included — dies, and the
/// `users.delete` sweep waits on the background queue.
#[tokio::test]
async fn delete_account_marks_revokes_and_queues_the_sweep() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let _second = common::create_app_and_token(&pool, &alice, "write:accounts").await;
    let app = common::test_app(pool.clone());

    let response = post_json(&app, DELETE_ACCOUNT, &token, json!({"password": PASSWORD})).await;
    assert_eq!(response.status, StatusCode::OK, "the start answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let deleted = stored(&pool, alice.id).await;
    assert!(!deleted.is_active.get(), "the account is deactivated");
    assert!(
        deleted.deleted_at.is_some(),
        "and carries the deletion stamp"
    );

    assert_eq!(token_count(&pool, alice.id).await, 0, "every token revoked");
    assert_eq!(
        signed_in(&app, &token).await,
        StatusCode::UNAUTHORIZED,
        "including the one that asked"
    );

    assert_eq!(
        wired_be::jobs::deletion::KIND_DELETE,
        USERS_DELETE,
        "the test's mirror of the job kind"
    );
    assert_eq!(
        queued_jobs(&pool, USERS_DELETE).await,
        vec![(
            "background".to_owned(),
            json!({"user_id": alice.id.to_string()})
        )],
        "the sweep waits on the background queue, keyed to the account"
    );
}

/// The `delete_account` refusals: the wrong password is the 200 error
/// body; on an instance with deletion disabled the endpoint is 403 ahead
/// of even the password gate. Neither marks the account.
#[tokio::test]
async fn delete_account_refusals_leave_the_account_alone() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(&app, DELETE_ACCOUNT, &token, json!({"password": "nope"})).await;
    assert_eq!(wrong.status, StatusCode::OK, "Pleroma answers 200");
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal"
    );

    let untouched = stored(&pool, alice.id).await;
    assert!(untouched.is_active.get(), "still active");
    assert!(untouched.deleted_at.is_none(), "no deletion stamp");
    assert_eq!(token_count(&pool, alice.id).await, 1, "its token lives");
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "and no sweep was queued"
    );

    let mut closed = common::test_config();
    closed.allow_account_deletion = wired_be::config::Toggle::new(false);
    let closed_app = common::test_app_with_config(pool.clone(), closed);
    let refused = post_json(
        &closed_app,
        DELETE_ACCOUNT,
        &token,
        json!({"password": PASSWORD}),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "the gate is 403");
    assert_eq!(
        refused.json_value(),
        json!({"error": "Account deletion is disabled"}),
        "the ticket's body"
    );
    let wrong_too = post_json(
        &closed_app,
        DELETE_ACCOUNT,
        &token,
        json!({"password": "nope"}),
    )
    .await;
    assert_eq!(
        wrong_too.status,
        StatusCode::FORBIDDEN,
        "and it answers before the password gate"
    );
    assert!(
        stored(&pool, alice.id).await.deleted_at.is_none(),
        "the closed gate marked nothing"
    );
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "or queued anything"
    );
}

/// `disable_account`: inactive, all tokens gone, no `deleted_at` — and
/// the account's statuses stay live (that is the whole difference from
/// deletion). The wrong password refuses with the 200 body, unchanged.
#[tokio::test]
async fn disable_account_deactivates_but_keeps_the_data() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let (alice, token) = fixture(&pool, "alice", "write:accounts").await;
    let kept =
        common::create_status(&pool, &alice, "kept through disable", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let wrong = post_json(&app, DISABLE_ACCOUNT, &token, json!({"password": "nope"})).await;
    assert_eq!(
        wrong.json_value(),
        json!({"error": "Invalid password."}),
        "the password refusal is 200"
    );
    assert!(
        stored(&pool, alice.id).await.is_active.get(),
        "and changed nothing"
    );

    let response = post_json(&app, DISABLE_ACCOUNT, &token, json!({"password": PASSWORD})).await;
    assert_eq!(response.status, StatusCode::OK, "the disable answers");
    assert_eq!(
        response.json_value(),
        json!({"status": "success"}),
        "the documented success body"
    );

    let disabled = stored(&pool, alice.id).await;
    assert!(!disabled.is_active.get(), "the account is deactivated");
    assert!(disabled.deleted_at.is_none(), "but no deletion was started");
    assert_eq!(token_count(&pool, alice.id).await, 0, "every token revoked");
    assert_eq!(
        signed_in(&app, &token).await,
        StatusCode::UNAUTHORIZED,
        "including the presenting one"
    );
    assert!(disabled.email.is_some(), "the email stays on the row");
    assert!(
        disabled.password_hash.is_some(),
        "and so does the password hash: disable is not deletion"
    );
    let status_kept: bool =
        sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NULL FROM statuses WHERE id = $1")
            .bind(kept.id)
            .fetch_one(&pool)
            .await
            .expect("the status reads");
    assert!(status_kept, "disable touches no content");
    assert!(
        queued_jobs(&pool, USERS_DELETE).await.is_empty(),
        "and queues no sweep"
    );
}

/// All four utilities: no token is the extractor's 401, a wrong-scope or
/// app-only credential one of the 403s, a dead token the invalid 401, and
/// a body without the required parameters the extractor's 422.
#[tokio::test]
async fn the_four_utilities_enforce_auth_and_scope() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let read_only = common::create_app_and_token(&pool, &alice, "read:accounts").await;
    let app_only = common::create_app_only_token(&pool, "write:accounts").await;
    let (_writer, token) = fixture(&pool, "writer", "write:accounts").await;
    let app = common::test_app(pool.clone());

    // One body that satisfies every endpoint's required parameters (serde
    // ignores the surplus keys), so a gated route answers on the gate —
    // not the extractor's 422 — when the credential is refused.
    let full_body = json!({
        "password": PASSWORD,
        "new_password": NEW_PASSWORD,
        "new_password_confirmation": NEW_PASSWORD,
        "email": "someone@example.org",
    });

    for path in [
        CHANGE_PASSWORD,
        CHANGE_EMAIL,
        DELETE_ACCOUNT,
        DISABLE_ACCOUNT,
    ] {
        let anonymous = post_json(&app, path, "", full_body.clone()).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{path}: no token"
        );
        assert_eq!(
            anonymous.json_value(),
            json!({"error": "Invalid credentials"}),
            "{path}: the AuthUser no-token body"
        );

        let dead = post_json(&app, path, "not-a-stored-token", full_body.clone()).await;
        assert_eq!(
            dead.status,
            StatusCode::UNAUTHORIZED,
            "{path}: a dead token"
        );

        let unscoped = post_json(&app, path, &read_only, full_body.clone()).await;
        assert_eq!(
            unscoped.status,
            StatusCode::FORBIDDEN,
            "{path}: read:accounts"
        );
        assert_eq!(
            unscoped.json_value(),
            json!({"error": "Insufficient permissions: write:accounts."}),
            "{path}: the scope denial"
        );

        let no_user = post_json(&app, path, &app_only, full_body.clone()).await;
        assert_eq!(no_user.status, StatusCode::FORBIDDEN, "{path}: app-only");

        let short_body = post_json(&app, path, &token, json!({})).await;
        assert_eq!(
            short_body.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}: a body without the required parameters"
        );
    }
}
