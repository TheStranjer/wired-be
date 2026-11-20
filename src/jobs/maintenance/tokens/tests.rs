//! The credential sweep's tests: exactly which dead rows of the five
//! credential tables the hourly pass reclaims and which still-usable ones it
//! leaves, the reset window it reads from the configuration, the retry a
//! database it cannot reach buys it, and the two registries its kind is wired
//! into.

use serde_json::json;
use sqlx::query_scalar;
use uuid::Uuid;

use super::*;
use crate::{
    config::Config,
    jobs::{
        QUEUE_BACKGROUND,
        maintenance::test_support::{
            DAY, FIRST_ATTEMPT, HOURLY, app_state, broken_state, row_exists, state_with_config,
            user,
        },
        scheduler::periodic_jobs,
    },
    models::{
        oauth_app::{NewOauthApp, OauthApp, create as create_app},
        user::User,
    },
};

/// Registers an OAuth app named `name`: the issuer the credential rows
/// point at through their `app_id` foreign keys.
async fn register_app(pool: &PgPool, name: &str) -> OauthApp {
    create_app(
        pool,
        &NewOauthApp {
            client_name: name.to_owned(),
            redirect_uris: "https://client.example/callback".to_owned(),
            scopes: "read".to_owned(),
            website: None,
            user_id: None,
        },
    )
    .await
    .expect("the app fixture inserts")
}

/// Inserts an `oauth_tokens` row for `owner` issued to `client` whose
/// `valid_until` is `secs` seconds from now (negative for the past), so
/// a test can set the table into any point of the grace window.
async fn oauth_token_expiring_in(
    pool: &PgPool,
    owner: &User,
    client: &OauthApp,
    secs: i64,
) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO oauth_tokens
                       (id, token, refresh_token, user_id, app_id, scopes, valid_until)
                   VALUES ($1, $2, $3, $4, $5, 'read', now() + make_interval(secs => $6))";
    let written = query(sql)
        .bind(id)
        .bind(format!("access-{id}"))
        .bind(format!("refresh-{id}"))
        .bind(owner.id)
        .bind(client.id)
        .bind(secs)
        .execute(pool)
        .await
        .expect("the fixture oauth token inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts an `oauth_authorizations` code for `owner` issued to
/// `client`, expiring `secs` seconds from now and already exchanged
/// when `used`, returning its id.
async fn authorization_row(
    pool: &PgPool,
    owner: &User,
    client: &OauthApp,
    secs: i64,
    used: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO oauth_authorizations
                       (id, token, user_id, app_id, scopes, valid_until, used)
                   VALUES ($1, $2, $3, $4, 'read', now() + make_interval(secs => $5), $6)";
    let written = query(sql)
        .bind(id)
        .bind(format!("code-{id}"))
        .bind(owner.id)
        .bind(client.id)
        .bind(secs)
        .bind(used)
        .execute(pool)
        .await
        .expect("the fixture authorization inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts an `mfa_tokens` challenge for `owner` whose `valid_until` is
/// `secs` seconds from now, returning its id.
async fn mfa_token_expiring_in(pool: &PgPool, owner: &User, secs: i64) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO mfa_tokens (id, token, user_id, authorization_id, valid_until)
                   VALUES ($1, $2, $3, NULL, now() + make_interval(secs => $4))";
    let written = query(sql)
        .bind(id)
        .bind(format!("mfa-{id}"))
        .bind(owner.id)
        .bind(secs)
        .execute(pool)
        .await
        .expect("the fixture mfa challenge inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts a `password_reset_tokens` row for `owner` issued `age_secs`
/// in the past and spent when `used`, returning its id.
async fn password_reset_row(pool: &PgPool, owner: &User, age_secs: i64, used: bool) -> Uuid {
    let id = Uuid::now_v7();
    let sql = "INSERT INTO password_reset_tokens (id, token, user_id, used, inserted_at)
                   VALUES ($1, $2, $3, $4, now() - make_interval(secs => $5))";
    let written = query(sql)
        .bind(id)
        .bind(format!("reset-{id}"))
        .bind(owner.id)
        .bind(used)
        .bind(age_secs)
        .execute(pool)
        .await
        .expect("the fixture reset token inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    id
}

/// Inserts an `oauth_login_states` round-trip state expiring `secs`
/// seconds from now (negative for one already aged out), returning its
/// state key — the table's primary key is the text the sweep matches.
async fn login_state_row(pool: &PgPool, secs: i64) -> String {
    let state = format!("state-{secs}-{}", Uuid::now_v7());
    let written = query(
        "INSERT INTO oauth_login_states (state, provider, params, valid_until)
             VALUES ($1, 'github', '{}', now() + make_interval(secs => $2))",
    )
    .bind(&state)
    .bind(secs)
    .execute(pool)
    .await
    .expect("the fixture login state inserts");
    assert_eq!(written.rows_affected(), 1, "one row was written");
    state
}

/// Whether the `oauth_login_states` row keyed `state` is still there.
async fn login_state_exists(pool: &PgPool, state: &str) -> bool {
    let rows = query_scalar::<_, i64>("SELECT count(*) FROM oauth_login_states WHERE state = $1")
        .bind(state)
        .fetch_one(pool)
        .await
        .expect("the state count reads");
    rows == 1
}

/// The token sweep deletes exactly the dead rows of each table and
/// keeps everything still inside its validity window.
#[tokio::test]
async fn the_token_sweep_deletes_only_the_dead_rows() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let client = register_app(&pool, "Client").await;
    let expired_token = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
    let graced_token = oauth_token_expiring_in(&pool, &alice, &client, -6 * DAY).await;
    let expired_code = authorization_row(&pool, &alice, &client, -60, false).await;
    let spent_code = authorization_row(&pool, &alice, &client, 600, true).await;
    let live_code = authorization_row(&pool, &alice, &client, 600, false).await;
    let stale_challenge = mfa_token_expiring_in(&pool, &alice, -1).await;
    let fresh_challenge = mfa_token_expiring_in(&pool, &alice, 300).await;
    let spent_reset = password_reset_row(&pool, &alice, 60, true).await;
    let lapsed_reset = password_reset_row(&pool, &alice, DAY + 60, false).await;
    let fresh_reset = password_reset_row(&pool, &alice, 60, false).await;
    let expired_state = login_state_row(&pool, -1).await;
    let live_state = login_state_row(&pool, 300).await;

    let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "a sweep over dead and live rows succeeds");
    assert!(
        !row_exists(&pool, "oauth_tokens", expired_token).await,
        "a token expired eight days ago is past its refresh-token week"
    );
    assert!(
        row_exists(&pool, "oauth_tokens", graced_token).await,
        "a token expired six days ago can still be refreshed and stays"
    );
    assert!(
        !row_exists(&pool, "oauth_authorizations", expired_code).await,
        "an expired code goes even unused: it can never be redeemed"
    );
    assert!(
        !row_exists(&pool, "oauth_authorizations", spent_code).await,
        "a spent code goes even fresh: it has been redeemed once already"
    );
    assert!(
        row_exists(&pool, "oauth_authorizations", live_code).await,
        "a fresh unused code stays"
    );
    assert!(
        !row_exists(&pool, "mfa_tokens", stale_challenge).await,
        "an expired challenge goes"
    );
    assert!(
        row_exists(&pool, "mfa_tokens", fresh_challenge).await,
        "a pending challenge stays"
    );
    assert!(
        !row_exists(&pool, "password_reset_tokens", spent_reset).await,
        "a spent reset token goes even fresh"
    );
    assert!(
        !row_exists(&pool, "password_reset_tokens", lapsed_reset).await,
        "a reset token older than the default one-day window goes"
    );
    assert!(
        row_exists(&pool, "password_reset_tokens", fresh_reset).await,
        "a fresh unused reset token stays"
    );
    assert!(
        !login_state_exists(&pool, &expired_state).await,
        "a login state past its ten minutes goes: no read answers it any more"
    );
    assert!(
        login_state_exists(&pool, &live_state).await,
        "a round trip still inside its window stays"
    );
}

/// The reset window comes from the configuration, not from a literal:
/// the same row survives the default one-day ttl and dies under an
/// overridden one-hour one.
#[tokio::test]
async fn the_reset_window_comes_from_the_config() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let aged = password_reset_row(&pool, &alice, 3_700, false).await;

    let default_pass = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;
    assert_eq!(default_pass, Ok(()), "the default-config sweep succeeds");
    assert!(
        row_exists(&pool, "password_reset_tokens", aged).await,
        "an hour-old token sits inside the default one-day window"
    );

    let mut config = Config::for_tests();
    config.password_reset_ttl_secs = 3_600;
    let short_window_pass =
        handle_tokens(state_with_config(&pool, config), FIRST_ATTEMPT, json!({})).await;
    assert_eq!(
        short_window_pass,
        Ok(()),
        "the overridden-ttl sweep succeeds"
    );
    assert!(
        !row_exists(&pool, "password_reset_tokens", aged).await,
        "the same token is outside a one-hour window and goes"
    );
}

#[tokio::test]
async fn the_token_sweep_passes_over_tables_it_need_not_touch() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();

    let outcome = handle_tokens(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "nothing to delete is a clean pass");
}

/// A sweep that cannot reach the database is a retryable failure, so
/// the queue keeps the pass while the scheduler enqueues the next hour:
/// the dead rows wait for a working sweep instead of being assumed gone.
#[tokio::test]
async fn an_unreachable_database_retries_the_token_sweep() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let client = register_app(&pool, "Client").await;
    let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
    let state = broken_state().await;

    let outcome = handle_tokens(state, FIRST_ATTEMPT, json!({})).await;

    match outcome {
        Err(JobError::Retry(detail)) => assert!(
            detail.starts_with("token sweep failed"),
            "the retry states what failed: {detail}"
        ),
        Ok(()) => panic!("a closed pool must not read as a successful sweep"),
        Err(other) => panic!("a closed pool is retryable, not {other:?}"),
    }
    assert!(
        row_exists(&pool, "oauth_tokens", expired).await,
        "the healthy pool sees no work the broken one claimed to do"
    );
}

/// The token sweep is wired into both registries the ticket names: the
/// worker can run its kind, and the scheduler places it on the
/// maintenance queue once an hour.
#[test]
fn the_token_kind_is_registered_and_scheduled() {
    let registry = crate::job_registry();
    assert!(
        registry.get(KIND_TOKENS).is_some(),
        "the worker knows `{KIND_TOKENS}`"
    );

    let job = periodic_jobs()
        .into_iter()
        .find(|job| job.kind == KIND_TOKENS)
        .expect("the sweep is on the periodic schedule");
    assert_eq!(job.queue, QUEUE_BACKGROUND, "on the maintenance queue");
    assert_eq!(job.every, HOURLY, "once an hour");
}

/// The handler the registry binds to the kind is this module's
/// [`handle_tokens`], not some other sweep: calling the registered
/// handler deletes the dead rows and keeps the live ones.
#[tokio::test]
async fn the_registered_token_handler_is_this_sweep() {
    let db = crate::test_db::test_db().await.expect("test database slot");
    let pool = db.pool().clone();
    let alice = user(&pool, "alice").await;
    let client = register_app(&pool, "Client").await;
    let expired = oauth_token_expiring_in(&pool, &alice, &client, -8 * DAY).await;
    let live = authorization_row(&pool, &alice, &client, 600, false).await;
    let registry = crate::job_registry();
    let handler = registry
        .get(KIND_TOKENS)
        .expect("the sweep's kind is registered");

    let outcome = handler(app_state(&pool), FIRST_ATTEMPT, json!({})).await;

    assert_eq!(outcome, Ok(()), "the registered handler runs");
    assert!(
        !row_exists(&pool, "oauth_tokens", expired).await,
        "and it is this module's sweep: the expired token is gone"
    );
    assert!(
        row_exists(&pool, "oauth_authorizations", live).await,
        "and only the dead rows went: the live code stays"
    );
}
