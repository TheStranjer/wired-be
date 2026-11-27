//! The per-role OAuth access-token lifetime (WBE-890) over HTTP: the
//! admin, bot and user tiers every grant picks from the token's user, the
//! refresh grant choosing at refresh time, the `:oauth_token_ttl_secs`
//! admin-config override applying to the next token with no restart, its
//! 422 refusals, the legacy `OAUTH_TOKEN_TTL_SECS` fallback and a
//! hundred-year bot token working end to end.

use axum::{Router, http::StatusCode};
use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::{
        BASE_URL_VAR, Config, DATABASE_URL_VAR, RATE_LIMITS_ENABLED_VAR, TEST_BASE_URL,
        TEST_DATABASE_URL, TokenTtls,
    },
    models::{
        oauth_app::OauthApp,
        oauth_token,
        user::{Flag, NewUser, User},
    },
    util::password::hash_password,
};

use super::config_writes::{CONFIG, post_configs};
use super::support::admin;
use crate::common::{self, TestRequest};

/// The password every account fixture logs in with.
const PASSWORD: &str = "correct horse battery staple";
/// Seconds in a hundred years, the bot tier's default.
const CENTURY: i64 = 3_153_600_000;
/// Slack, in seconds, between issuing a token and measuring its lifetime.
const SLACK: i64 = 15;

/// The accounts the tiers tell apart.
#[derive(Clone, Copy)]
enum Role {
    /// An admin that is a `Person`.
    Admin,
    /// A `Service` actor with no staff role.
    Bot,
    /// A `Service` actor that is also an admin.
    BotAdmin,
    /// Everyone else.
    Plain,
}

/// A local account named `nickname` with a password and the shape `role`
/// names.
async fn account(pool: &PgPool, nickname: &str, role: Role) -> User {
    let is_admin = matches!(role, Role::Admin | Role::BotAdmin);
    let actor_type = if matches!(role, Role::Bot | Role::BotAdmin) {
        "Service"
    } else {
        "Person"
    };
    common::create_local_user_with(pool, nickname, |user| NewUser {
        email: Some(format!("{nickname}@example.org")),
        password_hash: Some(hash_password(PASSWORD).expect("the fixture password hashes")),
        is_admin: Flag::new(is_admin),
        actor_type: actor_type.to_owned(),
        ..user
    })
    .await
}

/// A config whose three tiers are far apart: admin 1000, bot 2000 and
/// user 3000 seconds.
fn tiered_config() -> Config {
    let mut config = common::test_config();
    config.oauth_token_ttls = TokenTtls {
        admin: 1_000,
        bot: 2_000,
        user: 3_000,
    };
    config
}

/// A config read from the environment-style `extra` variables.
fn config_from(extra: &[(&str, &str)]) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        RATE_LIMITS_ENABLED_VAR => Some("false".to_owned()),
        other => extra
            .iter()
            .find(|(name, _)| *name == other)
            .map(|(_, value)| (*value).to_owned()),
    })
    .expect("the fixture environment is valid")
}

/// `POST /oauth/token` with `body`, answering the 200 JSON body.
async fn token_answer(app: &Router, body: &Value) -> Value {
    let response = TestRequest::post("/oauth/token").json(body).send(app).await;
    common::assert_answer(&response, StatusCode::OK, "the token grant");
    response.json_value()
}

/// The password grant of `nickname` through `client`.
async fn password_grant(app: &Router, client: &OauthApp, nickname: &str) -> Value {
    token_answer(
        app,
        &json!({
            "grant_type": "password",
            "username": nickname,
            "password": PASSWORD,
            "scope": "read write",
            "client_id": client.client_id,
            "client_secret": client.client_secret,
        }),
    )
    .await
}

/// The refresh grant of `refresh_token` through `client`.
async fn refresh_grant(app: &Router, client: &OauthApp, refresh_token: &str) -> Value {
    token_answer(
        app,
        &json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client.client_id,
            "client_secret": client.client_secret,
        }),
    )
    .await
}

/// Seconds of validity the stored row behind `body`'s access token has
/// left.
async fn lifetime_secs(pool: &PgPool, body: &Value) -> i64 {
    let access = body["access_token"].as_str().expect("an access token");
    let (stored, _app) = oauth_token::get_with_app(pool, access)
        .await
        .expect("the lookup reaches the database")
        .expect("the token row was stored");
    stored
        .valid_until
        .signed_duration_since(Utc::now())
        .num_seconds()
}

/// Asserts the stored lifetime behind `body` is `secs`, give or take the
/// time the request took.
async fn assert_lifetime(pool: &PgPool, body: &Value, secs: i64, what: &str) {
    let left = lifetime_secs(pool, body).await;
    assert!(
        (secs - SLACK..=secs).contains(&left),
        "{what}: expected about {secs}s of validity, got {left}s"
    );
}

/// The `:oauth_token_ttl_secs` upsert of `pairs`, as the admin posts it.
fn ttl_item(pairs: &[(&str, Value)]) -> Value {
    json!([{
        "group": ":pleroma",
        "key": ":oauth_token_ttl_secs",
        "value": super::config_writes::wire(pairs),
    }])
}

/// Every grant that mints a user token picks the lifetime of the token's
/// user's tier — admin over bot over user — and the app-only token is in
/// the user tier.
#[tokio::test]
async fn each_tier_gets_its_own_lifetime() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), tiered_config());
    let client = common::create_app(&pool, "read write").await;
    let _admin = account(&pool, "tieradmin", Role::Admin).await;
    let _bot = account(&pool, "tierbot", Role::Bot).await;
    let _bot_admin = account(&pool, "tierbotadmin", Role::BotAdmin).await;
    let _plain = account(&pool, "tierplain", Role::Plain).await;

    let admin_body = password_grant(&app, &client, "tieradmin").await;
    assert_lifetime(&pool, &admin_body, 1_000, "an admin").await;
    let bot_body = password_grant(&app, &client, "tierbot").await;
    assert_lifetime(&pool, &bot_body, 2_000, "a bot").await;
    let bot_admin_body = password_grant(&app, &client, "tierbotadmin").await;
    assert_lifetime(
        &pool,
        &bot_admin_body,
        1_000,
        "a bot that is an admin takes the admin tier",
    )
    .await;
    let plain_body = password_grant(&app, &client, "tierplain").await;
    assert_lifetime(&pool, &plain_body, 3_000, "a regular user").await;

    let app_only = token_answer(
        &app,
        &json!({
            "grant_type": "client_credentials",
            "scope": "read",
            "client_id": client.client_id,
            "client_secret": client.client_secret,
        }),
    )
    .await;
    assert_lifetime(&pool, &app_only, 3_000, "an app-only token").await;
}

/// The refresh grant mints the replacement pair for the user's tier at
/// refresh time: a bot that is promoted to admin between two refreshes
/// gets the admin lifetime on the second.
#[tokio::test]
async fn the_refresh_grant_uses_the_tier_at_refresh_time() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), tiered_config());
    let client = common::create_app(&pool, "read write").await;
    let bot = account(&pool, "refreshbot", Role::Bot).await;

    let first = password_grant(&app, &client, "refreshbot").await;
    assert_lifetime(&pool, &first, 2_000, "the first grant, as a bot").await;
    let first_refresh = first["refresh_token"].as_str().expect("a refresh token");
    let second = refresh_grant(&app, &client, first_refresh).await;
    assert_lifetime(&pool, &second, 2_000, "a refresh, still as a bot").await;

    let promoted = sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
        .bind(bot.id)
        .execute(&pool)
        .await
        .expect("the promotion runs")
        .rows_affected();
    assert_eq!(promoted, 1, "one account was promoted");
    let second_refresh = second["refresh_token"].as_str().expect("a refresh token");
    let third = refresh_grant(&app, &client, second_refresh).await;
    assert_lifetime(&pool, &third, 1_000, "a refresh after the promotion").await;
}

/// An admin-config override changes the next token's lifetime with no
/// restart, only for its own tier, and deleting it restores the env value.
#[tokio::test]
async fn an_admin_config_override_applies_to_the_next_token() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), tiered_config());
    let client = common::create_app(&pool, "read write").await;
    let _bot = account(&pool, "overridebot", Role::Bot).await;
    let _plain = account(&pool, "overrideplain", Role::Plain).await;
    let (_root, root_token) = admin(
        &pool,
        "overrideroot",
        "admin:read:config admin:write:config",
    )
    .await;

    let before = password_grant(&app, &client, "overridebot").await;
    assert_lifetime(&pool, &before, 2_000, "the env value before the override").await;

    let posted = post_configs(&app, &root_token, ttl_item(&[("bot", json!(777))])).await;
    common::assert_answer(&posted, StatusCode::OK, "the override is accepted");
    assert_eq!(
        posted.json_value()["need_reboot"],
        json!(false),
        "no restart is needed"
    );

    let after = password_grant(&app, &client, "overridebot").await;
    assert_lifetime(&pool, &after, 777, "the stored override wins").await;
    let plain = password_grant(&app, &client, "overrideplain").await;
    assert_lifetime(&pool, &plain, 3_000, "another tier keeps its env value").await;

    let view = TestRequest::get(CONFIG)
        .bearer(&root_token)
        .send(&app)
        .await;
    common::assert_answer(&view, StatusCode::OK, "the effective GET");
    let body = view.json_value();
    let entry = body["configs"]
        .as_array()
        .expect("the entries")
        .iter()
        .find(|entry| entry["key"] == json!(":oauth_token_ttl_secs"))
        .expect("the key is whitelisted");
    assert_eq!(
        entry["db"],
        json!(["bot"]),
        "the row holds the one stored subkey"
    );

    let deleted = post_configs(
        &app,
        &root_token,
        json!([{"group": ":pleroma", "key": ":oauth_token_ttl_secs", "delete": true}]),
    )
    .await;
    common::assert_answer(&deleted, StatusCode::OK, "the row is deleted");
    let restored = password_grant(&app, &client, "overridebot").await;
    assert_lifetime(&pool, &restored, 2_000, "the env value is back").await;
}

/// A lifetime that is not a positive number of seconds with a
/// representable expiry, or a subkey that is no tier, is a 422 and writes
/// nothing.
#[tokio::test]
async fn bad_lifetimes_are_refused_with_a_422() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app_with_config(pool.clone(), tiered_config());
    let client = common::create_app(&pool, "read write").await;
    let _bot = account(&pool, "refusedbot", Role::Bot).await;
    let (_root, root_token) =
        admin(&pool, "refusedroot", "admin:read:config admin:write:config").await;

    for pair in [
        ("admin", json!(0)),
        ("bot", json!(-1)),
        ("user", json!("3600")),
        ("bot", json!(99_999_999_999_999_u64)),
        ("bot", json!(1.5)),
        ("root", json!(60)),
    ] {
        let refused = post_configs(&app, &root_token, ttl_item(std::slice::from_ref(&pair))).await;
        common::assert_answer(
            &refused,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{pair:?} is refused"),
        );
    }

    let stored = TestRequest::get(&format!("{CONFIG}?only_db=true"))
        .bearer(&root_token)
        .send(&app)
        .await;
    common::assert_answer(&stored, StatusCode::OK, "the stored rows");
    assert_eq!(
        stored.json_value()["configs"],
        json!([]),
        "no refused write stored anything"
    );
    let body = password_grant(&app, &client, "refusedbot").await;
    assert_lifetime(&pool, &body, 2_000, "the env value still stands").await;
}

/// A token of the bot tier's hundred-year default is stored, authenticates
/// and refreshes: the timestamp arithmetic holds end to end.
#[tokio::test]
async fn a_hundred_year_bot_token_works_end_to_end() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let client = common::create_app(&pool, "read write").await;
    let _bot = account(&pool, "centurybot", Role::Bot).await;

    let body = password_grant(&app, &client, "centurybot").await;
    assert_lifetime(&pool, &body, CENTURY, "a bot's default").await;
    let access = body["access_token"].as_str().expect("an access token");
    let verified = TestRequest::get("/api/v1/accounts/verify_credentials")
        .bearer(access)
        .send(&app)
        .await;
    common::assert_answer(&verified, StatusCode::OK, "the hundred-year token works");

    let refresh = body["refresh_token"].as_str().expect("a refresh token");
    let renewed = refresh_grant(&app, &client, refresh).await;
    assert_lifetime(&pool, &renewed, CENTURY, "the refreshed pair").await;
}

/// The legacy `OAUTH_TOKEN_TTL_SECS` is still the default of the admin and
/// user tiers, a tier variable beats it, and the bot tier ignores it.
#[tokio::test]
async fn the_legacy_variable_still_sets_the_admin_and_user_default() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let client = common::create_app(&pool, "read write").await;
    let _admin = account(&pool, "legacyadmin", Role::Admin).await;
    let _bot = account(&pool, "legacybot", Role::Bot).await;
    let _plain = account(&pool, "legacyplain", Role::Plain).await;

    let legacy = common::test_app_with_config(
        pool.clone(),
        config_from(&[("OAUTH_TOKEN_TTL_SECS", "600")]),
    );
    let admin_body = password_grant(&legacy, &client, "legacyadmin").await;
    assert_lifetime(&pool, &admin_body, 600, "an admin on the legacy value").await;
    let plain_body = password_grant(&legacy, &client, "legacyplain").await;
    assert_lifetime(&pool, &plain_body, 600, "a user on the legacy value").await;
    let bot_body = password_grant(&legacy, &client, "legacybot").await;
    assert_lifetime(&pool, &bot_body, CENTURY, "a bot ignores the legacy value").await;

    let mixed = common::test_app_with_config(
        pool.clone(),
        config_from(&[
            ("OAUTH_TOKEN_TTL_SECS", "600"),
            ("OAUTH_TOKEN_TTL_SECS_USER", "900"),
            ("OAUTH_TOKEN_TTL_SECS_BOT", "1200"),
        ]),
    );
    let mixed_plain = password_grant(&mixed, &client, "legacyplain").await;
    assert_lifetime(
        &pool,
        &mixed_plain,
        900,
        "a tier variable beats the legacy one",
    )
    .await;
    let mixed_admin = password_grant(&mixed, &client, "legacyadmin").await;
    assert_lifetime(
        &pool,
        &mixed_admin,
        600,
        "the other tier keeps the legacy value",
    )
    .await;
    let mixed_bot = password_grant(&mixed, &client, "legacybot").await;
    assert_lifetime(&pool, &mixed_bot, 1_200, "the bot variable").await;
}
