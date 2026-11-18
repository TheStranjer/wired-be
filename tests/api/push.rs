//! Web Push subscriptions over HTTP (`/api/v1/push/subscription`,
//! WBE-144): create, get, update and delete of the access token's
//! subscription, the validation of its endpoint, keys and policy, the
//! per-token isolation of registrations, and the instance VAPID key the
//! subscription responses, the application entity and the instance
//! documents all advertise.
//!
//! Every route runs on a `push`-scoped user token; the auth and scope
//! refusals are checked for all four verbs against Pleroma's wording.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wired_be::{
    config::{
        BASE_URL_VAR, DATABASE_URL_VAR, HTTP_ALLOW_PRIVATE_HOSTS_VAR, RATE_LIMITS_ENABLED_VAR,
        TEST_BASE_URL, TEST_DATABASE_URL, VAPID_PRIVATE_KEY_VAR, VAPID_PUBLIC_KEY_VAR,
    },
    entities::push_subscription::ALERT_TYPES,
    services::vapid,
};

use crate::common;

/// A valid push callback endpoint.
const ENDPOINT: &str = "https://push.example/subscription/abc";
/// The second endpoint, for replacement tests.
const OTHER_ENDPOINT: &str = "https://push.example/subscription/xyz";

/// A `subscription` body for `endpoint` with both transport keys.
fn subscription_json(endpoint: &str) -> Value {
    json!({
        "subscription": {
            "endpoint": endpoint,
            "keys": {"p256dh": "BClientP256dhKey", "auth": "client-auth-secret"},
        },
    })
}

/// A `user` with one `push`-scoped token: the bearer string to authorize
/// the calls.
async fn push_token(pool: &PgPool, nickname: &str) -> String {
    let user = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &user, "push").await
}

/// The bearer string of a token of `nickname` granting `scopes`.
async fn scoped_token(pool: &PgPool, nickname: &str, scopes: &str) -> String {
    let user = common::create_local_user(pool, nickname).await;
    common::create_app_and_token(pool, &user, scopes).await
}

/// The stored `alerts` object of a response, spelled out complete: every
/// [`ALERT_TYPES`] name as a boolean.
fn expected_alerts(on: &[&str]) -> Value {
    let mut alerts = serde_json::Map::new();
    for alert_type in ALERT_TYPES {
        let value = Value::Bool(on.contains(&alert_type));
        let _entry: Option<Value> = alerts.insert(alert_type.to_owned(), value);
    }
    Value::Object(alerts)
}

/// The subscription row count of the database — the replacement and
/// isolation tests watch it.
async fn subscription_count(pool: &PgPool) -> i64 {
    let (count,) = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM web_push_subscriptions")
        .fetch_one(pool)
        .await
        .expect("the count runs");
    count
}

/// The stored keys of the token `bearer`'s subscription, as
/// `(key_p256dh, key_auth)`.
async fn stored_keys(pool: &PgPool, bearer: &str) -> (String, String) {
    sqlx::query_as(
        "SELECT s.key_p256dh, s.key_auth FROM web_push_subscriptions s
           JOIN oauth_tokens t ON t.id = s.token_id
          WHERE t.token = $1",
    )
    .bind(bearer)
    .fetch_one(pool)
    .await
    .expect("the token's subscription row reads")
}

/// `POST /api/v1/push/subscription` registers the token's subscription
/// and answers the entity: the minted id, the endpoint verbatim, every
/// alert type with the named ones on, the instance VAPID key as
/// `server_key` and the requested policy. `GET` reads the same object
/// back, and `DELETE` then empties it again.
#[tokio::test]
async fn create_get_and_delete_round_trip_the_subscription() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-roundtrip").await;

    let created = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "subscription": subscription_json(ENDPOINT)["subscription"],
            "data": {
                "alerts": {"follow": true, "mention": true, "pleroma:chat_mention": true},
                "policy": "follower",
            },
        }))
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "push create");
    let body = created.json_value();
    let id = body["id"]
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())
        .expect("id is the hyphenated UUIDv7");
    assert!(id.get_version_num() == 7, "the id is a UUIDv7 (decision 4)");
    assert_eq!(body["endpoint"], ENDPOINT, "the endpoint echoes");
    assert_eq!(
        body["alerts"],
        expected_alerts(&["follow", "mention", "pleroma:chat_mention"]),
        "every known type is listed, the named ones on"
    );
    assert_eq!(body["policy"], "follower", "the requested policy");
    let server_key = vapid::vapid_public_key(&pool)
        .await
        .expect("the instance VAPID key");
    assert_eq!(body["server_key"], server_key, "the VAPID public key");
    assert_eq!(
        body.as_object()
            .expect("an object")
            .keys()
            .collect::<Vec<_>>(),
        vec!["alerts", "endpoint", "id", "policy", "server_key"],
        "the entity's key set"
    );

    let fetched = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    common::assert_answer(&fetched, StatusCode::OK, "push show");
    assert_eq!(fetched.json_value(), body, "GET answers what POST wrote");

    let deleted = common::TestRequest::delete("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    common::assert_answer(&deleted, StatusCode::OK, "push delete");
    assert_eq!(
        deleted.json_value(),
        json!({}),
        "DELETE answers the empty object"
    );
    assert_eq!(subscription_count(&pool).await, 0, "and the row is gone");

    let missing = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    common::assert_answer(&missing, StatusCode::NOT_FOUND, "show after delete");
    common::assert_error(&missing, "Record not found", "show after delete");
    let again = common::TestRequest::delete("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    common::assert_answer(&again, StatusCode::NOT_FOUND, "double delete");
    common::assert_error(&again, "Record not found", "double delete");
}

/// A create that names no `data` defaults every alert to false and the
/// policy to `all`; a get before any create is the 404 of decision 8.
#[tokio::test]
async fn alerts_default_to_off_and_the_policy_to_all() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-defaults").await;

    let absent = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    common::assert_answer(&absent, StatusCode::NOT_FOUND, "show before create");
    common::assert_error(&absent, "Record not found", "show before create");

    let created = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&subscription_json(ENDPOINT))
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "dataless create");
    let body = created.json_value();
    assert_eq!(
        body["alerts"],
        expected_alerts(&[]),
        "every type listed, all false"
    );
    assert_eq!(body["policy"], "all", "the default policy");
}

/// A form body with the nested `subscription[…]` / `data[…]` keys
/// registers exactly like JSON (decision 10), and the transport keys are
/// stored base64url-normalised: `+` to `-`, `/` to `_`, padding dropped
/// (Pleroma's `ensure_base64_urlsafe`).
#[tokio::test]
async fn a_form_body_registers_with_urlsafe_keys() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-form").await;

    let created = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .form(&[
            ("subscription[endpoint]", ENDPOINT),
            ("subscription[keys][p256dh]", "BC+Raw/Key=="),
            ("subscription[keys][auth]", "auth_secret="),
            ("data[alerts][follow]", "true"),
            ("data[alerts][favourite]", "off"),
            ("data[alerts][admin.report]", "yes"),
            ("data[policy]", "followed"),
        ])
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "form create");
    let body = created.json_value();
    assert_eq!(body["policy"], "followed", "the form policy");
    assert_eq!(
        body["alerts"],
        expected_alerts(&["follow", "admin.report"]),
        "the form booleans land on their flags"
    );
    assert_eq!(
        stored_keys(&pool, &bearer).await,
        ("BC-Raw_Key".to_owned(), "auth_secret".to_owned()),
        "keys are normalised to URL-safe base64 without padding"
    );
}

/// A second create for the same token replaces the first subscription:
/// one row, the new endpoint, keys, alerts and policy.
#[tokio::test]
async fn re_create_replaces_the_tokens_subscription() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-twice").await;

    let first = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "subscription": subscription_json(ENDPOINT)["subscription"],
            "data": {"alerts": {"follow": true}, "policy": "follower"},
        }))
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::OK, "the first create");
    let second = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "subscription": subscription_json(OTHER_ENDPOINT)["subscription"],
            "data": {"alerts": {"mention": true}, "policy": "none"},
        }))
        .send(&app)
        .await;
    common::assert_answer(&second, StatusCode::OK, "the second create");
    let body = second.json_value();
    assert_eq!(
        body["id"],
        first.json_value()["id"],
        "the token keeps its one subscription id"
    );
    assert_eq!(body["endpoint"], OTHER_ENDPOINT, "the new endpoint");
    assert_eq!(
        body["alerts"],
        expected_alerts(&["mention"]),
        "the alerts are the new registration's, not the union"
    );
    assert_eq!(body["policy"], "none", "the new policy");
    assert_eq!(subscription_count(&pool).await, 1, "one row for one token");

    let fetched = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    assert_eq!(fetched.json_value(), body, "and GET reads the replacement");
}

/// `PUT` moves the alert flags and the policy it names and keeps the
/// rest; an empty submission changes nothing without being an error; a
/// token with no subscription gets the 404.
#[tokio::test]
async fn update_moves_only_the_named_preferences() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-editor").await;

    let absent = common::TestRequest::put("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({"data": {"policy": "none"}}))
        .send(&app)
        .await;
    common::assert_answer(&absent, StatusCode::NOT_FOUND, "update before create");
    common::assert_error(&absent, "Record not found", "update before create");

    let created = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "subscription": subscription_json(ENDPOINT)["subscription"],
            "data": {
                "alerts": {"follow": true, "mention": true},
                "policy": "follower",
            },
        }))
        .send(&app)
        .await;
    common::assert_answer(&created, StatusCode::OK, "the create");

    let updated = common::TestRequest::put("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "data": {"alerts": {"follow": false, "reblog": true}, "policy": "none"},
        }))
        .send(&app)
        .await;
    common::assert_answer(&updated, StatusCode::OK, "the update");
    let body = updated.json_value();
    assert_eq!(
        body["alerts"],
        expected_alerts(&["mention", "reblog"]),
        "named flags move, unnamed ones stay"
    );
    assert_eq!(body["policy"], "none", "the named policy moves");
    assert_eq!(body["endpoint"], ENDPOINT, "the endpoint never changes");
    assert_eq!(
        body["id"],
        created.json_value()["id"],
        "the same subscription, updated"
    );

    let untouched = common::TestRequest::put("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({}))
        .send(&app)
        .await;
    common::assert_answer(&untouched, StatusCode::OK, "an empty update");
    assert_eq!(
        untouched.json_value(),
        body,
        "and nothing the body did not name moved"
    );

    let bad = common::TestRequest::put("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({"data": {"policy": "everybody"}}))
        .send(&app)
        .await;
    common::assert_answer(&bad, StatusCode::UNPROCESSABLE_ENTITY, "a bad policy");
    common::assert_error(
        &bad,
        "Validation failed: Policy is not included in the list",
        "a bad policy",
    );
    let after_bad = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&bearer)
        .send(&app)
        .await;
    assert_eq!(
        after_bad.json_value(),
        body,
        "the refused update wrote nothing"
    );
}

/// A create that misses the endpoint or a key, sends an endpoint that is
/// no URL or is overlong, or names a policy outside the four, is the 422
/// with Mastodon's changeset wording. A plain-`http` endpoint is not among
/// the refusals: Pleroma stores whatever it is given, and the delivery
/// pipeline calls any `http`/`https` address (`e2e_realtime.rs` wakes a
/// loopback mock push service over exactly such an endpoint).
#[tokio::test]
async fn create_validates_the_endpoint_keys_and_policy() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let bearer = push_token(&pool, "push-invalid").await;
    let keys = || subscription_json(ENDPOINT)["subscription"]["keys"].clone();

    for (body, message) in [
        (json!({}), "Validation failed: Endpoint can't be blank"),
        (
            json!({"subscription": {"keys": keys()}}),
            "Validation failed: Endpoint can't be blank",
        ),
        (
            json!({"subscription": {"endpoint": "not a url", "keys": keys()}}),
            "Validation failed: Endpoint must be a valid URL",
        ),
        (
            json!({"subscription": {"endpoint": "ftp://push.example/1", "keys": keys()}}),
            "Validation failed: Endpoint must be a valid URL",
        ),
        (
            json!({"subscription": {"endpoint": format!("https://p.example/{}", "x".repeat(2048)), "keys": keys()}}),
            "Validation failed: Endpoint is too long (maximum is 2048 characters)",
        ),
        (
            json!({"subscription": {"endpoint": ENDPOINT}}),
            "Validation failed: P256dh key can't be blank",
        ),
        (
            json!({"subscription": {"endpoint": ENDPOINT, "keys": {"p256dh": "k"}}}),
            "Validation failed: Auth key can't be blank",
        ),
        (
            json!({
                "subscription": {"endpoint": ENDPOINT, "keys": {"p256dh": "k", "auth": "a"}},
                "data": {"policy": "ALL"},
            }),
            "Validation failed: Policy is not included in the list",
        ),
        (
            json!({"subscription": {"endpoint": ENDPOINT, "keys": {"p256dh": "  ", "auth": "a"}}}),
            "Validation failed: P256dh key can't be blank",
        ),
    ] {
        let response = common::TestRequest::post("/api/v1/push/subscription")
            .bearer(&bearer)
            .json(&body)
            .send(&app)
            .await;
        common::assert_answer(
            &response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a refused create",
        );
        common::assert_error(&response, message, "a refused create");
    }
    assert_eq!(subscription_count(&pool).await, 0, "no refusal wrote a row");

    let ok = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&json!({
            "subscription": {"endpoint": ENDPOINT, "keys": keys()},
            "data": {"policy": "followed"},
        }))
        .send(&app)
        .await;
    common::assert_answer(&ok, StatusCode::OK, "the good body after the refusals");
}

/// Two tokens of one account hold two subscriptions: registering for
/// one leaves the other untouched, and neither can read or delete the
/// other's.
#[tokio::test]
async fn subscriptions_are_isolated_per_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let user = common::create_local_user(&pool, "push-two-tokens").await;
    let one = common::create_app_and_token(&pool, &user, "push").await;
    let two = common::create_app_and_token(&pool, &user, "push").await;

    let _first = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&one)
        .json(&subscription_json(ENDPOINT))
        .send(&app)
        .await;
    let quiet = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&two)
        .send(&app)
        .await;
    common::assert_answer(&quiet, StatusCode::NOT_FOUND, "the second token's GET");

    let _second = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&two)
        .json(&json!({
            "subscription": subscription_json(OTHER_ENDPOINT)["subscription"],
            "data": {"alerts": {"follow": true}, "policy": "none"},
        }))
        .send(&app)
        .await;
    assert_eq!(subscription_count(&pool).await, 2, "one row per token");

    let mine = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&one)
        .send(&app)
        .await;
    assert_eq!(
        mine.json_value()["endpoint"],
        ENDPOINT,
        "the first token reads its own registration"
    );

    let _gone = common::TestRequest::delete("/api/v1/push/subscription")
        .bearer(&one)
        .send(&app)
        .await;
    assert_eq!(
        subscription_count(&pool).await,
        1,
        "only the first row went"
    );
    let other = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&two)
        .send(&app)
        .await;
    common::assert_answer(&other, StatusCode::OK, "the second token is untouched");
    assert_eq!(other.json_value()["endpoint"], OTHER_ENDPOINT);
}

/// Every route demands a `push`-scoped user token: no bearer is the 401
/// "Invalid credentials", a bogus one the 401 "The access token is
/// invalid", a token without the scope the 403 naming it, and an
/// app-only push token the 403 that wants a user.
#[tokio::test]
async fn every_route_demands_a_push_user_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let sightseer = scoped_token(&pool, "push-sightseer", "read").await;
    let app_only = common::create_app_only_token(&pool, "push").await;

    let requests = [
        common::TestRequest::get("/api/v1/push/subscription"),
        common::TestRequest::post("/api/v1/push/subscription"),
        common::TestRequest::put("/api/v1/push/subscription"),
        common::TestRequest::delete("/api/v1/push/subscription"),
    ];
    for request in requests {
        let anonymous = request.bearer("not-a-real-token").send(&app).await;
        common::assert_answer(&anonymous, StatusCode::UNAUTHORIZED, "a bogus bearer");
        common::assert_error(&anonymous, "The access token is invalid", "a bogus bearer");
    }
    for request in [
        common::TestRequest::get("/api/v1/push/subscription"),
        common::TestRequest::post("/api/v1/push/subscription"),
        common::TestRequest::put("/api/v1/push/subscription"),
        common::TestRequest::delete("/api/v1/push/subscription"),
    ] {
        let missing = request.send(&app).await;
        common::assert_answer(&missing, StatusCode::UNAUTHORIZED, "no bearer");
        common::assert_error(&missing, "Invalid credentials", "no bearer");
    }
    for request in [
        common::TestRequest::get("/api/v1/push/subscription").bearer(&sightseer),
        common::TestRequest::post("/api/v1/push/subscription").bearer(&sightseer),
        common::TestRequest::put("/api/v1/push/subscription").bearer(&sightseer),
        common::TestRequest::delete("/api/v1/push/subscription").bearer(&sightseer),
    ] {
        let refused = request.send(&app).await;
        common::assert_answer(&refused, StatusCode::FORBIDDEN, "a read-only token");
        common::assert_error(
            &refused,
            "Insufficient permissions: push.",
            "a read-only token",
        );
    }
    let userless = common::TestRequest::get("/api/v1/push/subscription")
        .bearer(&app_only)
        .send(&app)
        .await;
    common::assert_answer(&userless, StatusCode::FORBIDDEN, "an app-only token");
    common::assert_error(
        &userless,
        "This API requires an authenticated user",
        "an app-only token",
    );
    assert_eq!(
        subscription_count(&pool).await,
        0,
        "no refusal registered anything"
    );
}

/// The instance VAPID key is generated once, stable across every reader,
/// and surfaced in all the places the ticket names: the subscription
/// responses' `server_key`, the `Application` entity of both app routes,
/// the `/api/v1/pleroma/apps` listing, the `pleroma.vapid_public_key` of
/// both instance documents and the v2 `configuration.vapid.public_key`.
#[tokio::test]
async fn the_vapid_key_is_stable_and_surfaced_everywhere() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let v1_first = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await;
    let v1_second = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await;
    let key = v1_first.json_value()["pleroma"]["vapid_public_key"]
        .as_str()
        .expect("the v1 pleroma block advertises the key")
        .to_owned();
    assert_eq!(
        v1_second.json_value()["pleroma"]["vapid_public_key"],
        json!(key),
        "the same key on every read"
    );

    let v2 = common::TestRequest::get("/api/v2/instance")
        .send(&app)
        .await;
    assert_eq!(
        v2.json_value()["pleroma"]["vapid_public_key"],
        json!(key),
        "the v2 pleroma block agrees"
    );
    assert_eq!(
        v2.json_value()["configuration"]["vapid"]["public_key"],
        json!(key),
        "the v2 configuration group agrees"
    );

    let bearer = push_token(&pool, "push-vapid").await;
    let created = common::TestRequest::post("/api/v1/push/subscription")
        .bearer(&bearer)
        .json(&subscription_json(ENDPOINT))
        .send(&app)
        .await;
    assert_eq!(
        created.json_value()["server_key"],
        json!(key),
        "the subscription's server_key is the instance key"
    );

    let registered = common::TestRequest::post("/api/v1/apps")
        .json(&json!({"client_name": "Push Client", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob", "scopes": "push"}))
        .send(&app)
        .await;
    assert_eq!(
        registered.json_value()["vapid_key"],
        json!(key),
        "POST /api/v1/apps advertises it"
    );

    let verified = common::TestRequest::get("/api/v1/apps/verify_credentials")
        .bearer(&bearer)
        .send(&app)
        .await;
    assert_eq!(
        verified.json_value()["vapid_key"],
        json!(key),
        "GET /api/v1/apps/verify_credentials advertises it"
    );

    let listed = common::TestRequest::get("/api/v1/pleroma/apps")
        .bearer(&bearer)
        .send(&app)
        .await;
    assert_eq!(
        listed.json_value()[0]["vapid_key"],
        json!(key),
        "GET /api/v1/pleroma/apps advertises it on every entry"
    );

    assert_eq!(
        vapid::vapid_public_key(&pool)
            .await
            .expect("the helper reads"),
        key,
        "the helper, the DB and every endpoint read one key"
    );
}

/// A `VAPID_PRIVATE_KEY` config override is the last word at startup:
/// the pair is stored from the config (the public half derived when
/// none is pinned), and the lazy reader never regenerates over it.
#[tokio::test]
async fn configured_vapid_keys_take_over_the_stored_pair() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mine = vapid::vapid_public_key(&pool)
        .await
        .expect("the lazy seed runs first");

    let fresh = vapid::generate_pair_for_tests().expect("a test pair generates");
    let config = vapid_override_config(fresh.private_pem(), None);
    let advertised = vapid::ensure_instance_keypair(&pool, &config)
        .await
        .expect("the configured pair applies");
    assert_ne!(advertised, mine, "the override moved the key");
    assert_eq!(
        advertised,
        fresh.public_key(),
        "the public half was derived from the configured private one"
    );
    assert_eq!(
        vapid::vapid_public_key(&pool)
            .await
            .expect("the reader follows"),
        advertised,
        "and the lazy reader keeps it"
    );

    let app = common::test_app_with_config(pool, config);
    let instance = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await;
    assert_eq!(
        instance.json_value()["pleroma"]["vapid_public_key"],
        json!(advertised),
        "the instance document serves the configured key"
    );
}

/// A [`Config`](wired_be::config::Config) with only the VAPID settings
/// `private`/`public` set (a `None` public leaves the variable unset),
/// the limiter off and the SSRF guard as the test default has it.
fn vapid_override_config(private_pem: &str, public_key: Option<&str>) -> wired_be::config::Config {
    wired_be::config::Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        HTTP_ALLOW_PRIVATE_HOSTS_VAR => Some("true".to_owned()),
        RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        VAPID_PRIVATE_KEY_VAR => Some(private_pem.to_owned()),
        VAPID_PUBLIC_KEY_VAR => public_key.map(str::to_owned),
        _ => None,
    })
    .expect("the fixed VAPID override config is valid")
}
