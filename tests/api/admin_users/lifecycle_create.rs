//! The lifecycle create and delete of WBE-196: the batch create that
//! bypasses the confirmation and approval queues even on an instance that
//! demands both, its per-item success and refusal answers, the
//! fifty-account cap that refuses the oversized batch whole, and the
//! admin's delete starting every named account's own sweep.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::models::user::{Flag, NewUser, User};

use super::support::{
    PASSWORD, USERS, account_named, create_body, exists, grant_body, jobs_of, lifecycle_flags,
    logged, mail_config, mint, post_json, seed, token_count, write_token,
};
use crate::common::{self, TestRequest};

// ===== WBE-196: the account lifecycle — create, delete, activation, approval =====

/// Whether `account` carries a deletion stamp.
async fn deletion_stamped(pool: &PgPool, account: &User) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
        .bind(account.id)
        .fetch_one(pool)
        .await
        .expect("the stamp reads back")
}

/// The admin-created account is active, confirmed and approved on an
/// instance that demands confirmation AND approval: the registration
/// queues no letter for it, its row clears the staff gate the mode sets,
/// it signs in at once, and the batch logs Pleroma's one `create` entry
/// with its plural `subjects` key.
#[tokio::test]
async fn admin_created_accounts_bypass_every_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = mail_config();
    config.registrations_approval_required = wired_be::config::Toggle::new(true);
    config.account_activation_required = wired_be::config::Toggle::new(true);
    let app = common::test_app_with_config(pool.clone(), config);
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: Flag::new(true),
        email: Some("root@example.org".to_owned()),
        ..user
    })
    .await;
    let token = mint(&pool, &root, "admin:write:accounts").await;

    let response = post_json(
        &app,
        &token,
        USERS,
        Some(&create_body(&[
            ("fresh", "fresh@example.org", PASSWORD),
            ("other", "other@example.org", PASSWORD),
        ])),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the bulk create");
    assert_eq!(
        response.json_value(),
        json!([
            {"code": 200, "type": "success",
             "data": {"email": "fresh@example.org", "nickname": "fresh"}},
            {"code": 200, "type": "success",
             "data": {"email": "other@example.org", "nickname": "other"}},
        ]),
        "one success entry per item, in the order the request stated them"
    );

    for nickname in ["fresh", "other"] {
        let (active, confirmed, pending) = lifecycle_flags(&pool, nickname).await;
        assert!(
            active && confirmed && !pending,
            "{nickname} is active, confirmed and approved whatever the modes: {active} {confirmed} {pending}"
        );
    }
    assert!(
        jobs_of(&pool, "mailer", "mailer.send").await.is_empty(),
        "activation mode queued no confirmation letter for an admin-created account"
    );

    let client = common::create_app(&pool, "read").await;
    let login = TestRequest::post("/oauth/token")
        .json(&grant_body(
            &client.client_id,
            &client.client_secret,
            "fresh",
        ))
        .send(&app)
        .await;
    common::assert_answer(
        &login,
        StatusCode::OK,
        "the created account signs straight in",
    );
    assert!(
        !login.json_value()["access_token"].is_null(),
        "and the grant hands out its credential"
    );

    let entries = logged(&pool, "create").await;
    assert_eq!(entries.len(), 1, "one create entry for the whole batch");
    assert_eq!(
        entries[0].0, "@root created users: @fresh, @other",
        "Pleroma's create sentence"
    );
    let named: Vec<&str> = entries[0].1["subjects"]
        .as_array()
        .expect("the plural subjects key Pleroma's clause reads")
        .iter()
        .filter_map(|found| found["nickname"].as_str())
        .collect();
    assert_eq!(named, vec!["fresh", "other"], "both created accounts");
    assert!(
        entries[0].1.get("subject").is_none(),
        "and the entry keeps Pleroma's spelling of the key"
    );
}

/// A mixed batch answers per item, and stays per item: the one valid
/// account is stored and logged while the duplicate, the malformed
/// address and the short password each report their own 400 refusal —
/// where Pleroma rolls the whole batch back into one 409, the ticket pins
/// the always-200 per-item answer.
#[tokio::test]
async fn the_create_answers_every_item_for_itself() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;

    let response = post_json(
        &seeded.app,
        &token,
        USERS,
        Some(&create_body(&[
            ("goodone", "goodone@example.org", PASSWORD),
            ("plain", "dup@example.org", PASSWORD),
            ("wrong", "not-an-email", PASSWORD),
            ("shorty", "shorty@example.org", "pw"),
        ])),
    )
    .await;
    common::assert_answer(&response, StatusCode::OK, "the mixed batch");
    let items = response.json_value();
    let items = items.as_array().expect("four items");
    assert_eq!(items.len(), 4, "one entry per submitted item");

    assert_eq!(items[0]["code"], 200, "the valid item stored");
    assert_eq!(items[0]["type"], "success");
    for (index, needle, who) in [
        (1, "has already been taken", "the duplicate handle"),
        (2, "has invalid format", "the malformed address"),
        (3, "should be at least", "the too-short password"),
    ] {
        assert_eq!(items[index]["code"], 400, "{who} reports the pinned code");
        assert_eq!(items[index]["type"], "error", "{who} is an error item");
        assert!(
            items[index]["error"]
                .as_str()
                .unwrap_or_default()
                .contains(needle),
            "{who} says why: {}",
            items[index]["error"]
        );
    }
    assert_eq!(
        items[1]["data"]["nickname"], "plain",
        "the refusal names what was submitted"
    );
    assert_eq!(
        items[2]["data"]["email"], "not-an-email",
        "handle and address alike"
    );

    assert!(
        exists(&seeded.pool, "goodone").await,
        "the good item is stored"
    );
    let (active, confirmed, pending) = lifecycle_flags(&seeded.pool, "goodone").await;
    assert!(
        active && confirmed && !pending,
        "on the default open instance the bypass is the registration's own plain state"
    );
    assert!(
        !exists(&seeded.pool, "wrong").await,
        "the refused items created nothing"
    );
    assert!(!exists(&seeded.pool, "shorty").await, "every one of them");

    let entries = logged(&seeded.pool, "create").await;
    assert_eq!(entries.len(), 1, "one entry for the batch");
    assert_eq!(
        entries[0].0, "@root created users: @goodone",
        "naming only the created"
    );
}

/// The fifty-account cap refuses the oversized batch whole, before any
/// item is created and with no entry logged.
#[tokio::test]
async fn the_create_caps_the_batch_at_fifty() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let before = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&seeded.pool)
        .await
        .expect("the count reads");

    let users: Vec<Value> = (0..51)
        .map(|index| {
            json!({
                "nickname": format!("batch{index}"),
                "email": format!("batch{index}@example.org"),
                "password": PASSWORD,
            })
        })
        .collect();
    let response = post_json(&seeded.app, &token, USERS, Some(&json!({ "users": users }))).await;
    common::assert_answer(&response, StatusCode::UNPROCESSABLE_ENTITY, "a batch of 51");
    assert!(
        response.text().contains("50"),
        "and the 422 states the cap: {}",
        response.text()
    );

    let after = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
        .fetch_one(&seeded.pool)
        .await
        .expect("the count reads");
    assert_eq!(after, before, "nothing was created");
    assert!(
        logged(&seeded.pool, "create").await.is_empty(),
        "and the refused batch left no entry"
    );
}

/// The admin's delete starts each named account's own deletion exactly as
/// `POST /api/pleroma/delete_account` starts it — marked, revoked, swept
/// on the `background` queue — answers the started nicknames, ignores the
/// caller's own account and the misses, and the `nicknames[]` arrives in
/// the body OR the query string.
#[tokio::test]
async fn admin_delete_starts_the_sweep_of_every_other_account() {
    let db = common::test_db().await;
    let seeded = seed(&db).await;
    let token = write_token(&seeded).await;
    let plain = account_named(&seeded.pool, "plain").await;
    let _plain_token = mint(&seeded.pool, &plain, "read").await;

    let response = TestRequest::delete(USERS)
        .bearer(&token)
        .json(&json!({"nicknames": ["plain", "ghost", "root"]}))
        .send(&seeded.app)
        .await;
    common::assert_answer(&response, StatusCode::OK, "the delete");
    assert_eq!(
        response.json_value(),
        json!(["plain"]),
        "the started nicknames: the miss skipped, the caller ignored"
    );

    let sweeps = jobs_of(&seeded.pool, "background", "users.delete").await;
    assert_eq!(sweeps.len(), 1, "one sweep queued");
    assert_eq!(
        sweeps[0]["user_id"],
        plain.id.to_string(),
        "and it names plain's row, on the queue account deletion uses"
    );
    assert!(
        deletion_stamped(&seeded.pool, &plain).await,
        "the account carries its deletion stamp"
    );
    let (active, _, _) = lifecycle_flags(&seeded.pool, "plain").await;
    assert!(!active, "and it is inactive");
    assert_eq!(
        token_count(&seeded.pool, &plain).await,
        0,
        "every token went"
    );
    let root = account_named(&seeded.pool, "root").await;
    assert!(
        !deletion_stamped(&seeded.pool, &root).await && token_count(&seeded.pool, &root).await > 0,
        "the caller's own account stands untouched, tokens included"
    );

    // The same call through the query string, bracketed.
    let again = TestRequest::delete(&format!("{USERS}?nicknames[]=beeper&nicknames[]=ghost"))
        .bearer(&token)
        .send(&seeded.app)
        .await;
    common::assert_answer(&again, StatusCode::OK, "the query-string delete");
    assert_eq!(again.json_value(), json!(["beeper"]), "one started name");
    assert_eq!(
        jobs_of(&seeded.pool, "background", "users.delete")
            .await
            .len(),
        2,
        "and its own sweep job"
    );

    let entries = logged(&seeded.pool, "delete").await;
    let messages: Vec<&str> = entries
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "@root deleted users: @plain",
            "@root deleted users: @beeper"
        ],
        "Pleroma's delete sentence, once per request"
    );
}
