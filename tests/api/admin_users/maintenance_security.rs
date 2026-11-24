//! The security half of the follow maintenance of WBE-198: the admin's MFA
//! removal that resets the settings column to the migration default and
//! answers the nickname, and the forced follow pair that connects and
//! disconnects any local pair with Pleroma's sentences — refusing the
//! misses, the remote follower and the self-follow without logging them.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::{Flag, NewUser, User};

use super::support::{
    DISABLE_MFA, FOLLOW, UNFOLLOW, follows_token, logged, one_entry, plain, seed, write_token,
};
use crate::common::{self, TestRequest};

/// The state of the `follows` row of one pair, `None` without one.
async fn follow_state(pool: &PgPool, follower: &User, target: &User) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT state FROM follows WHERE follower_id = $1 AND followee_id = $2",
    )
    .bind(follower.id)
    .bind(target.id)
    .fetch_optional(pool)
    .await
    .expect("the follow row reads back")
    .flatten()
}

/// Removing MFA as an admin: the settings column returns to the migration
/// default, the answer is the nickname itself, an unknown name is the 404
/// and a missing `nickname` the 422 — and the write logs.
#[tokio::test]
async fn disable_mfa_resets_the_settings_and_answers_the_nickname() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let (twofa, _secret, _codes) =
        common::create_mfa_user_with_password(&seeded.pool, "twofa", "their secret pass", 3).await;

    let response = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({ "nickname": "twofa" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the MFA removal");
    assert_eq!(
        response.json_value(),
        json!("twofa"),
        "Pleroma echoes the nickname"
    );

    let stored: Value =
        sqlx::query_scalar::<_, Value>("SELECT mfa_settings FROM users WHERE id = $1")
            .bind(twofa.id)
            .fetch_one(&seeded.pool)
            .await
            .expect("the settings read back");
    assert_eq!(
        stored,
        json!({"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}),
        "the column is the migration default again"
    );
    let (message, subjects) = one_entry(&seeded.pool, "disable_mfa").await;
    assert_eq!(
        message, "@root disabled MFA for users: @twofa",
        "the added entry"
    );
    assert_eq!(subjects, vec!["twofa".to_owned()], "for the account");

    let missing = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({ "nickname": "ghost" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "an unknown handle");
    let bare = TestRequest::put(DISABLE_MFA)
        .bearer(&token)
        .json(&json!({}))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &bare,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the body without nickname",
    );
}

/// The forced follow connects the pair even across a locked target — the
/// request is accepted on the spot — logs Pleroma's `made … follow`
/// sentence, and the unfollow takes it back and logs the pair's other
/// sentence.
#[tokio::test]
async fn the_admin_follow_pair_connects_reports_and_disconnects() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = follows_token(&seeded).await;
    let lockedy = common::create_local_user_with(&seeded.pool, "lockedy", |user| NewUser {
        locked: Flag::new(true),
        ..user
    })
    .await;
    let follower = plain(&seeded.pool).await;

    let response = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the forced follow");
    assert_eq!(response.json_value(), json!("ok"), "the ticket's answer");
    assert_eq!(
        follow_state(&seeded.pool, &follower, &lockedy)
            .await
            .as_deref(),
        Some("accepted"),
        "the admin's act is the answer a locked target would have given"
    );
    let entries = logged(&seeded.pool, "follow").await;
    assert_eq!(entries.len(), 1, "one entry");
    assert_eq!(
        entries[0].0, "@root made @plain follow @lockedy",
        "Pleroma's sentence"
    );
    assert_eq!(
        entries[0].1["follower"]["nickname"],
        json!("plain"),
        "both maps ride in the data"
    );
    assert_eq!(
        entries[0].1["followed"]["nickname"],
        json!("lockedy"),
        "on both sides"
    );

    let withdrawn = TestRequest::post(UNFOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&withdrawn, StatusCode::OK, "the forced unfollow");
    assert_eq!(withdrawn.json_value(), json!("ok"), "the same answer");
    assert!(
        follow_state(&seeded.pool, &follower, &lockedy)
            .await
            .is_none(),
        "the row is gone"
    );
    let withdrawn_log = logged(&seeded.pool, "unfollow").await;
    assert_eq!(withdrawn_log.len(), 1, "and its own entry");
    assert_eq!(
        withdrawn_log[0].0, "@root made @plain unfollow @lockedy",
        "Pleroma's sentence"
    );

    // Re-following an accepted pair changes nothing and still reports ok;
    // a second unfollow is the same no-op with its second entry.
    let again = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the re-follow");
    let idempotent = TestRequest::post(UNFOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "lockedy" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&idempotent, StatusCode::OK, "the repeat unfollow");
    assert_eq!(
        logged(&seeded.pool, "unfollow").await.len(),
        2,
        "logged again"
    );
}

/// The follow pair refuses what it cannot force: a name that answers to
/// nobody, a remote FOLLOWER (this instance cannot order a mirror) and
/// following yourself — the model's own 422 rides through.
#[tokio::test]
async fn the_admin_follow_pair_refuses_the_impossible() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = follows_token(&seeded).await;

    let miss = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "ghost" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &miss,
        StatusCode::NOT_FOUND,
        "a name that answers to nobody",
    );

    let mirror_first = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "faraway@remote.example", "followed": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&mirror_first, StatusCode::NOT_FOUND, "a remote follower");

    let selfish = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain", "followed": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(
        &selfish,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the self-follow",
    );
    common::assert_error(
        &selfish,
        "Can not follow yourself",
        "the model's own wording",
    );

    let bare = TestRequest::post(FOLLOW)
        .bearer(&token)
        .json(&json!({ "follower": "plain" }))
        .send(&seeded.app)
        .await;
    common::assert_answer(&bare, StatusCode::UNPROCESSABLE_ENTITY, "the half body");
    assert!(
        logged(&seeded.pool, "follow").await.is_empty(),
        "and no refusal left an entry behind"
    );
}
