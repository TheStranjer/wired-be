//! The internal webhooks and the gates: the instance-installed rows
//! reading like any other and resisting every admin write with Pleroma's
//! 403 and the ticket's 422, the listing's ascending id order and page
//! window, the four `webhook_*` moderation-log lines, and the scope and
//! role halves of the gate over every webhook route.

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::support::{
    INVALID_TOKEN, LOG, NO_ADMIN_READ, NO_ADMIN_WRITE, NO_TOKEN, NOT_ADMIN, USER_REQUIRED, admin,
    moderator, regular,
};
use super::webhooks_crud::{
    WEBHOOKS, act_on_webhook, assert_webhook_keys, create_webhook, delete_webhook, get_webhook,
    patch_webhook, post_webhook, stored_webhook, webhook_admin,
};
use crate::common::{self, TestRequest};

/// The 403 body of any write onto an internal webhook (Pleroma's
/// `FallbackController` `:forbidden` wording).
const FORBIDDEN: &str = "Forbidden";
/// The 422 body of deleting an internal webhook (pinned by the ticket).
const CANNOT_DELETE_INTERNAL: &str = "Cannot delete internal webhook";

/// The ids the webhook listing answers, in order.
fn listed_ids(response: &common::TestResponse) -> Vec<String> {
    response
        .json_value()
        .as_array()
        .expect("the array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("the id is text").to_owned())
        .collect()
}

/// Seeds one `webhooks` row under a controlled `id`, so the listing's
/// order and the internal-protection tests command the table instead of
/// riding on the instant the API minted its `UUIDv7`s. The fixed seed
/// secret is 64 `s`s, the minted shape without the randomness.
async fn seed_webhook(pool: &PgPool, id: u128, url: &str, internal: bool) -> Uuid {
    let seeded_id = Uuid::from_u128(id);
    let _seeded = sqlx::query(
        "INSERT INTO webhooks (id, url, events, secret, enabled, internal) \
         VALUES ($1, $2, '{report.created}', $3, true, $4)",
    )
    .bind(seeded_id)
    .bind(url)
    .bind("s".repeat(64))
    .bind(internal)
    .execute(pool)
    .await
    .expect("the seed webhook inserts");
    seeded_id
}

/// An instance-installed webhook is read like any other: the show answers
/// its entity and the listing holds it beside the external rows.
#[tokio::test]
async fn internal_webhooks_are_read_like_any_other() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookreader").await;

    let internal_id = seed_webhook(&pool, 7, "https://internal.example.org/hook", true).await;
    let external_id = seed_webhook(&pool, 8, "https://external.example.org/hook", false).await;

    let shown = TestRequest::get(&format!("{WEBHOOKS}/{internal_id}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_answer(&shown, StatusCode::OK, "the internal show");
    assert_eq!(
        shown.json_value()["internal"],
        json!(true),
        "reads answer internal webhooks verbatim"
    );

    let listed = TestRequest::get(WEBHOOKS).bearer(&token).send(&app).await;
    let entries = listed.json_value();
    let rows = entries.as_array().expect("the array");
    assert_eq!(rows.len(), 2, "the listing includes the internal webhook");
    assert_eq!(
        rows[0]["id"],
        json!(internal_id.to_string()),
        "it reads side by side with an external webhook, in id order"
    );
    assert_eq!(
        rows[1]["id"],
        json!(external_id.to_string()),
        "the external neighbour follows"
    );
}

/// The internal protection: the delete answers the ticket's 422 and the
/// patch, the gate actions and the rotation answer Pleroma's 403 — with
/// the row left standing on every refusal.
#[tokio::test]
async fn internal_webhooks_resist_the_admin_writes() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookguard").await;

    let internal_id = seed_webhook(&pool, 7, "https://internal.example.org/hook", true).await;
    let internal_text = internal_id.to_string();

    let deleted = TestRequest::delete(&format!("{WEBHOOKS}/{internal_text}"))
        .bearer(&token)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[deleted],
        StatusCode::UNPROCESSABLE_ENTITY,
        CANNOT_DELETE_INTERNAL,
        "the delete of an internal webhook",
    );

    let patched = TestRequest::patch(&format!("{WEBHOOKS}/{internal_text}"))
        .bearer(&token)
        .json(&json!({"enabled": false}))
        .send(&app)
        .await;
    common::assert_all_refused(
        &[patched],
        StatusCode::FORBIDDEN,
        FORBIDDEN,
        "a patch onto an internal webhook",
    );
    let disabled = post_webhook(
        &app,
        &token,
        &format!("{WEBHOOKS}/{internal_text}/disable"),
        json!({}),
    )
    .await;
    common::assert_all_refused(
        &[disabled],
        StatusCode::FORBIDDEN,
        FORBIDDEN,
        "the disable of an internal webhook",
    );
    let rotated = post_webhook(
        &app,
        &token,
        &format!("{WEBHOOKS}/{internal_text}/rotate_secret"),
        json!({}),
    )
    .await;
    common::assert_all_refused(
        &[rotated],
        StatusCode::FORBIDDEN,
        FORBIDDEN,
        "the rotation of an internal webhook",
    );

    let row = stored_webhook(&pool, internal_id)
        .await
        .expect("the row survives every refused write");
    assert_eq!(row.0, "https://internal.example.org/hook", "nothing moved");
    assert_eq!(row.2, "s".repeat(64), "not even the secret");
    assert!(row.3, "the gate stands");
    assert!(row.4, "and the flag");
}

/// The listing order and window: seeded ids come back ascending (the v7
/// text sorts as creation order, decision 4), `page` / `page_size` cut
/// the window, unparsable or non-positive parameters fall to the defaults
/// (page 1, 50 rows), and an API-created webhook — minted right now —
/// lands after every seeded row.
#[tokio::test]
async fn the_webhook_listing_orders_by_id_and_pages() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let token = webhook_admin(&pool, "hookpager").await;

    let first = seed_webhook(&pool, 1, "https://example.org/one", false).await;
    let second = seed_webhook(&pool, 2, "https://example.org/two", false).await;
    let third = seed_webhook(&pool, 3, "https://example.org/three", false).await;
    let seeded = vec![first.to_string(), second.to_string(), third.to_string()];

    let full = get_webhook(&app, &token, WEBHOOKS).await;
    common::assert_answer(&full, StatusCode::OK, "the full listing");
    assert_eq!(listed_ids(&full), seeded, "ordered by id");

    let windowed = get_webhook(&app, &token, &format!("{WEBHOOKS}?page=2&page_size=2")).await;
    let last = json!([{"id": third.to_string(), "url": "https://example.org/three",
                       "events": ["report.created"], "secret": "s".repeat(64),
                       "enabled": true, "internal": false}]);
    assert_eq!(
        windowed.json_value(),
        last,
        "the second window of two holds the last row"
    );

    let head = get_webhook(&app, &token, &format!("{WEBHOOKS}?page_size=2")).await;
    let two = vec![first.to_string(), second.to_string()];
    assert_eq!(listed_ids(&head), two, "the first window");

    let past_end = get_webhook(&app, &token, &format!("{WEBHOOKS}?page=9&page_size=2")).await;
    assert_eq!(
        past_end.json_value(),
        json!([]),
        "a window past the table is empty"
    );

    let junk = get_webhook(&app, &token, &format!("{WEBHOOKS}?page=abc&page_size=0")).await;
    assert_eq!(
        listed_ids(&junk).len(),
        3,
        "unusable parameters fall to the 1 / 50 defaults"
    );

    let (_, newest) =
        create_webhook(&app, &token, json!({"url": "https://example.org/created"})).await;
    let full_again = get_webhook(&app, &token, WEBHOOKS).await;
    let mut expected = seeded;
    expected.push(newest);
    assert_eq!(
        listed_ids(&full_again),
        expected,
        "the freshly minted v7 id sorts last"
    );
}

/// The four logged writes land in the moderation log — `webhook_create`,
/// `webhook_update`, `webhook_rotate_secret` and `webhook_delete` — each
/// sentence naming the webhook id with the id in the data map's
/// `subject_id`, as the invite lines do. (The enable/disable actions are
/// not among the ticket's logged verbs.)
#[tokio::test]
async fn webhook_writes_record_the_moderation_log() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, token) = admin(&pool, "hooklogger", "admin:read admin:write").await;

    let created = post_webhook(
        &app,
        &token,
        WEBHOOKS,
        json!({"url": "https://example.org/hook", "events": ["report.created"]}),
    )
    .await;
    let entity = created.json_value();
    let id = entity["id"].as_str().expect("the id is text").to_owned();
    let patched = TestRequest::patch(&format!("{WEBHOOKS}/{id}"))
        .bearer(&token)
        .json(&json!({"enabled": false}))
        .send(&app)
        .await;
    common::assert_answer(&patched, StatusCode::OK, "the patch");
    let rotated = post_webhook(
        &app,
        &token,
        &format!("{WEBHOOKS}/{id}/rotate_secret"),
        json!({}),
    )
    .await;
    common::assert_answer(&rotated, StatusCode::OK, "the rotation");
    // The unlogged actions must not add a fifth line under any search.
    let gated = post_webhook(&app, &token, &format!("{WEBHOOKS}/{id}/enable"), json!({})).await;
    common::assert_answer(&gated, StatusCode::OK, "the enable");
    let deleted = TestRequest::delete(&format!("{WEBHOOKS}/{id}"))
        .bearer(&token)
        .send(&app)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "the delete");

    for (search, verb, action) in [
        ("created+webhook", "created", "webhook_create"),
        ("updated+webhook", "updated", "webhook_update"),
        (
            "rotated+secret+for+webhook",
            "rotated secret for",
            "webhook_rotate_secret",
        ),
        ("deleted+webhook", "deleted", "webhook_delete"),
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
            format!("@hooklogger {verb} webhook: {id}"),
            "the sentence names the webhook id"
        );
        assert_eq!(entry["data"]["action"], action, "the action");
        assert_eq!(
            entry["data"]["subject_id"], id,
            "and the webhook rides in subject_id"
        );
        assert_eq!(
            entry["data"]["actor"]["nickname"], "hooklogger",
            "beside the acting admin, as every entry carries it"
        );
    }
}

/// The webhook routes' scope half: the reads ask `admin:read:webhooks`
/// and the writes `admin:write:webhooks`, each accepting its narrow alias
/// and refusing the other class.
#[tokio::test]
async fn webhook_routes_answer_the_scope_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, read_only) = admin(&pool, "hookread", "admin:read:webhooks").await;
    let (_, write_only) = admin(&pool, "hookwrite", "admin:write:webhooks").await;

    let (_, id) = create_webhook(
        &app,
        &write_only,
        json!({"url": "https://example.org/hook"}),
    )
    .await;

    let listed = get_webhook(&app, &read_only, WEBHOOKS).await;
    common::assert_answer(&listed, StatusCode::OK, "the narrow read scope lists");
    let shown = get_webhook(&app, &read_only, &format!("{WEBHOOKS}/{id}")).await;
    common::assert_answer(&shown, StatusCode::OK, "and shows");

    let list_with_write = get_webhook(&app, &write_only, WEBHOOKS).await;
    let show_with_write = get_webhook(&app, &write_only, &format!("{WEBHOOKS}/{id}")).await;
    common::assert_all_refused(
        &[list_with_write, show_with_write],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "the write scope never reads",
    );

    let refusals = [
        post_webhook(
            &app,
            &read_only,
            WEBHOOKS,
            json!({"url": "https://example.org/x"}),
        )
        .await,
        patch_webhook(&app, &read_only, &id, json!({"enabled": false})).await,
        delete_webhook(&app, &read_only, &id).await,
        act_on_webhook(&app, &read_only, &id, "/disable").await,
        act_on_webhook(&app, &read_only, &id, "/enable").await,
        act_on_webhook(&app, &read_only, &id, "/rotate_secret").await,
    ];
    common::assert_all_refused(
        &refusals,
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "the read scope never writes",
    );

    // Nothing the read scope tried actually changed: the webhook still
    // stands as created, enabled and at its original url.
    let standing = get_webhook(&app, &read_only, &format!("{WEBHOOKS}/{id}")).await;
    let row = standing.json_value();
    assert_eq!(row["url"], json!("https://example.org/hook"), "the url");
    assert_eq!(row["enabled"], json!(true), "and the gate");
}

/// The same gate's two remaining halves: an admin whose token grants only
/// ordinary scopes is refused both classes, each refusal naming its own
/// class, while the broad `admin:read` / `admin:write` roots open every
/// webhook route.
#[tokio::test]
async fn unscoped_and_broad_admins_answer_the_webhook_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let (_, unscoped) = admin(&pool, "hookplain", "read write").await;

    let refused_get = get_webhook(&app, &unscoped, WEBHOOKS).await;
    common::assert_all_refused(
        &[refused_get],
        StatusCode::FORBIDDEN,
        NO_ADMIN_READ,
        "an admin with no admin grant",
    );
    let refused_post = post_webhook(
        &app,
        &unscoped,
        WEBHOOKS,
        json!({"url": "https://example.org/y"}),
    )
    .await;
    common::assert_all_refused(
        &[refused_post],
        StatusCode::FORBIDDEN,
        NO_ADMIN_WRITE,
        "and the POST refusal names the write class",
    );

    let (_, both) = admin(&pool, "hookbroad", "admin:read admin:write").await;
    let (broad, _) = create_webhook(&app, &both, json!({"url": "https://example.org/broad"})).await;
    assert_webhook_keys(&broad);
    let listed = get_webhook(&app, &both, WEBHOOKS).await;
    assert_eq!(
        listed_ids(&listed).len(),
        1,
        "the broad roots grant both classes"
    );
}

/// The webhook routes' role and credential half: a moderator or a regular
/// user holding the webhook scopes is refused the role, an app-only token
/// the user, and no token at all the credentials.
#[tokio::test]
async fn webhook_routes_answer_the_role_gate() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let scopes = "admin:read:webhooks admin:write:webhooks";
    let (_, mod_hooks) = moderator(&pool, "hookmod", scopes).await;
    let (_, user_hooks) = regular(&pool, "hookuser", scopes).await;
    let app_only = common::create_app_only_token(&pool, scopes).await;

    for (token, who) in [(&mod_hooks, "a moderator"), (&user_hooks, "a regular user")] {
        let refused = TestRequest::get(WEBHOOKS).bearer(token).send(&app).await;
        common::assert_all_refused(&[refused], StatusCode::FORBIDDEN, NOT_ADMIN, who);
        let posted = post_webhook(
            &app,
            token,
            WEBHOOKS,
            json!({"url": "https://example.org/hook"}),
        )
        .await;
        common::assert_all_refused(&[posted], StatusCode::FORBIDDEN, NOT_ADMIN, who);
    }

    let app_only_refused = TestRequest::get(WEBHOOKS)
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_all_refused(
        &[app_only_refused],
        StatusCode::FORBIDDEN,
        USER_REQUIRED,
        "an app-only token",
    );

    let anonymous = TestRequest::get(WEBHOOKS).send(&app).await;
    common::assert_all_refused(
        &[anonymous],
        StatusCode::UNAUTHORIZED,
        NO_TOKEN,
        "an anonymous GET",
    );
    let bogus = post_webhook(
        &app,
        "no-such-token",
        WEBHOOKS,
        json!({"url": "https://example.org/hook"}),
    )
    .await;
    common::assert_all_refused(
        &[bogus],
        StatusCode::UNAUTHORIZED,
        INVALID_TOKEN,
        "an unknown token on the POST",
    );
}
