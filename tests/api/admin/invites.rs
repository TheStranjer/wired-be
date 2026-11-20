//! Admin invites (WBE-199) over HTTP: `POST /users/invite_token`
//! deriving the invite type from its limits, the newest-first
//! `GET /users/invites`, the `POST /users/revoke_invite` lifecycle and
//! the `invite_create` and `invite_revoke` lines the two writes land in
//! the moderation log. Pleroma's `AdminAPI.InviteController` gates the
//! read with `admin:read:invites` and the three POSTs with
//! `admin:write:invites`; the route consts and the helpers the email and
//! gate submodules share ride here as `pub(super)`.

use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::support::{LOG, NOT_FOUND, admin};
use crate::common::{self, TestRequest};

/// The invite-creation route.
pub(super) const INVITE_CREATE: &str = "/api/v1/pleroma/admin/users/invite_token";
/// The invite-listing route.
pub(super) const INVITE_LIST: &str = "/api/v1/pleroma/admin/users/invites";
/// The invite-revocation route.
pub(super) const INVITE_REVOKE: &str = "/api/v1/pleroma/admin/users/revoke_invite";
/// The emailed-invite route.
pub(super) const INVITE_EMAIL: &str = "/api/v1/pleroma/admin/users/email_invite";
/// The registration-link route, merged at the app root.
pub(super) const REGISTRATION: &str = "/registration";
/// The 400 body of the email route while `INVITES_ENABLED` is off.
pub(super) const INVITES_DISABLED: &str =
    "To send invites you need to set the `invites_enabled` option to true.";
/// The 400 body of the email route when the body names no `email`.
pub(super) const MISSING_EMAIL: &str = "Missing parameter: email";
/// The 422 body of the revoke route when the body names no `token`.
const MISSING_TOKEN: &str = "Missing parameter: token";

/// An admin holding both invite scopes, as its bearer token.
pub(super) async fn invite_admin(pool: &PgPool, nickname: &str) -> String {
    admin(pool, nickname, "admin:read:invites admin:write:invites")
        .await
        .1
}

/// `POST`s `body` to an invite route as `token`, over JSON.
pub(super) async fn post_invite(
    app: &Router,
    token: &str,
    path: &str,
    body: Value,
) -> common::TestResponse {
    TestRequest::post(path)
        .bearer(token)
        .json(&body)
        .send(app)
        .await
}

/// The `invite_tokens` rows as `(id, token, invite_type)` texts, id order.
pub(super) async fn stored_invites(pool: &PgPool) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT id::text, token, invite_type FROM invite_tokens ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("the invite table reads back")
}

/// Seeds one `invite_tokens` row under a controlled `id`, so the listing's
/// order is fixed instead of riding on the millisecond the server minted
/// its `UUIDv7`s in.
async fn seed_invite(pool: &PgPool, id: u128, token: &str) {
    let _seeded = sqlx::query(
        "INSERT INTO invite_tokens (id, token, invite_type) VALUES ($1, $2, 'reusable')",
    )
    .bind(Uuid::from_u128(id))
    .bind(token)
    .execute(pool)
    .await
    .expect("the seed invite inserts");
}

/// A freshly minted entity answers its own seven whole keys: a uuid id
/// text, the model's 22-character secret, and an unserved, unspent row.
fn assert_fresh_entity(entity: &Value) {
    assert_eq!(entity["used"], false, "a fresh invite is unspent");
    assert_eq!(entity["uses"], 0, "and has served nothing");
    let id = entity["id"].as_str().expect("the id is text");
    assert!(Uuid::parse_str(id).is_ok(), "the id is uuid text: {id}");
    assert_eq!(
        entity["token"].as_str().map(str::len),
        Some(22),
        "the minted secret is the model's 22 characters"
    );
    assert_eq!(
        entity.as_object().map(serde_json::Map::len),
        Some(7),
        "exactly the view's seven keys, no created_at: {entity}"
    );
}

/// The `POST /users/invite_token` derivation: neither limit is the
/// one-time invite, a cap is the reusable one, a date the date-limited
/// one, and both together the combination — each answered as the entity
/// with its stored limits, and each standing in the table. A
/// form-encoded numeric string reads as the number it spells.
#[tokio::test]
async fn invite_tokens_derive_their_type_from_the_limits() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitecreator").await;

    let plain = post_invite(&app, &token, INVITE_CREATE, json!({})).await;
    common::assert_answer(&plain, StatusCode::OK, "the plain invite");
    let entity = plain.json_value();
    assert_eq!(entity["invite_type"], "one_time", "neither limit");
    assert_eq!(entity["expires_at"], Value::Null, "no expiry to name");
    assert_eq!(entity["max_use"], Value::Null, "and no cap");
    assert_fresh_entity(&entity);

    let capped = post_invite(&app, &token, INVITE_CREATE, json!({"max_use": 5})).await;
    let capped = capped.json_value();
    assert_eq!(capped["invite_type"], "reusable", "a cap alone");
    assert_eq!(capped["max_use"], 5, "and the cap round-trips");

    let dated = post_invite(
        &app,
        &token,
        INVITE_CREATE,
        json!({"expires_at": "2099-01-01"}),
    )
    .await;
    let dated = dated.json_value();
    assert_eq!(dated["invite_type"], "date_limited", "a date alone");
    assert_eq!(
        dated["expires_at"], "2099-01-01",
        "the plain YYYY-MM-DD day round-trips"
    );

    let both = post_invite(
        &app,
        &token,
        INVITE_CREATE,
        json!({"max_use": "3", "expires_at": "2099-01-01"}),
    )
    .await;
    let both = both.json_value();
    assert_eq!(
        both["invite_type"], "reusable_date_limited",
        "both limits together"
    );
    assert_eq!(both["max_use"], 3, "the string cap read as a number");
    assert_eq!(both["expires_at"], "2099-01-01", "beside the date");

    let form = TestRequest::post(INVITE_CREATE)
        .bearer(&token)
        .form(&[("max_use", "2")])
        .send(&app)
        .await;
    common::assert_answer(&form, StatusCode::OK, "the form-encoded cap");
    assert_eq!(
        form.json_value()["invite_type"],
        "reusable",
        "and the same derivation from a urlencoded body"
    );

    let mut stored_types: Vec<String> = stored_invites(&pool)
        .await
        .into_iter()
        .map(|(_, _, kind)| kind)
        .collect();
    stored_types.sort_unstable();
    assert_eq!(
        stored_types,
        vec![
            "date_limited".to_owned(),
            "one_time".to_owned(),
            "reusable".to_owned(),
            "reusable".to_owned(),
            "reusable_date_limited".to_owned(),
        ],
        "every answer stands in the table with its derived type"
    );
}

/// The `GET /users/invites` answer: `{"invites": […]}`, newest first on
/// the id's creation order, every entity whole.
#[tokio::test]
async fn the_invite_list_answers_newest_first() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "invitelisten").await;

    let empty = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&empty, StatusCode::OK, "the fresh listing");
    assert_eq!(
        empty.json_value(),
        json!({"invites": []}),
        "an empty table lists as an empty array"
    );

    seed_invite(&pool, 1, "oldest-invite").await;
    seed_invite(&pool, 2, "middle-invite").await;
    seed_invite(&pool, 3, "newest-invite").await;

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&listed, StatusCode::OK, "the seeded listing");
    let page = listed.json_value();
    let tokens: Vec<&str> = page["invites"]
        .as_array()
        .expect("the invites array")
        .iter()
        .map(|entry| entry["token"].as_str().expect("a token text"))
        .collect();
    assert_eq!(
        tokens,
        vec!["newest-invite", "middle-invite", "oldest-invite"],
        "the newest invite leads the array"
    );
    assert_eq!(
        page["invites"][0],
        json!({
            "id": Uuid::from_u128(3).to_string(),
            "token": "newest-invite",
            "used": false,
            "expires_at": null,
            "uses": 0,
            "max_use": null,
            "invite_type": "reusable",
        }),
        "each entry is the seven-key entity"
    );
}

/// The `POST /users/revoke_invite` lifecycle: the named invite is deleted
/// and answered as the entity it was, it drops out of the listing, a
/// second revoke (or any unknown token) is the 404, and a body without
/// `token` is the 422.
#[tokio::test]
async fn revoking_an_invite_deletes_it_and_answers_the_entity() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = invite_admin(&pool, "inviterevoker").await;

    let created = post_invite(&app, &token, INVITE_CREATE, json!({"max_use": 2})).await;
    common::assert_answer(&created, StatusCode::OK, "the create");
    let entity = created.json_value();

    let revoked = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");
    assert_eq!(
        revoked.json_value(),
        entity,
        "the entity exactly as the create answered it"
    );

    let listed = TestRequest::get(INVITE_LIST)
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value(),
        json!({"invites": []}),
        "the revoked invite is deleted, not marked used"
    );

    let again = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_all_refused(
        &[again],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "revoking the gone invite again",
    );
    let bogus = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": "never-minted"}),
    )
    .await;
    common::assert_all_refused(
        &[bogus],
        StatusCode::NOT_FOUND,
        NOT_FOUND,
        "an unknown token",
    );

    let bodiless = post_invite(&app, &token, INVITE_REVOKE, json!({})).await;
    common::assert_all_refused(
        &[bodiless],
        StatusCode::UNPROCESSABLE_ENTITY,
        MISSING_TOKEN,
        "a revoke without a token",
    );
}

/// The two invite writes land in the moderation log: `invite_create` and
/// `invite_revoke` sentences naming the invite id, each findable by the
/// listing's search with its action and `subject_id` in the data map.
#[tokio::test]
async fn invite_writes_record_the_moderation_log() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "invitelogger", "admin:read admin:write").await;

    let created = post_invite(&app, &token, INVITE_CREATE, json!({})).await;
    let entity = created.json_value();
    let id = entity["id"].as_str().expect("the id is text").to_owned();
    let revoked = post_invite(
        &app,
        &token,
        INVITE_REVOKE,
        json!({"token": entity["token"].clone()}),
    )
    .await;
    common::assert_answer(&revoked, StatusCode::OK, "the revoke");

    for (search, verb, action) in [
        ("created+invite", "created", "invite_create"),
        ("revoked+invite", "revoked", "invite_revoke"),
    ] {
        let found = TestRequest::get(&format!("{LOG}?search={search}"))
            .bearer(&token)
            .send(&app)
            .await;
        let page = found.json_value();
        assert_eq!(page["total"], json!(1), "one {action} line");
        let entry = &page["items"][0];
        assert_eq!(
            entry["message"],
            format!("@invitelogger {verb} invite: {id}"),
            "the sentence names the invite id"
        );
        assert_eq!(entry["data"]["action"], action, "the action");
        assert_eq!(
            entry["data"]["subject_id"], id,
            "and the invite rides in subject_id"
        );
        assert_eq!(
            entry["data"]["actor"]["nickname"], "invitelogger",
            "beside the acting admin, as every entry carries it"
        );
    }
}
