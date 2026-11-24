//! The registration modes around `POST /oauth/register` that the JSON
//! registration tests (`json_registration_modes`) do not cover: the
//! provider's own email standing as confirmed under
//! `ACCOUNT_ACTIVATION_REQUIRED`, and the out-of-band client reading its
//! code from the JSON answer. A form-encoded submission is answered as
//! JSON too.

use axum::http::StatusCode;
use wired_be::{
    config::Toggle,
    models::{oauth_token, user},
};

use super::provider::{OOB, PROVIDER, UNLINKED_UID};
use super::support::{
    assert_no_store, fixture, fixture_with, oob_app, pending_state, post_form, prepare_to,
    query_param,
};
use crate::common;

/// The provider's own email, submitted verbatim (in any case), is the
/// proof `ACCOUNT_ACTIVATION_REQUIRED` asks for: the account registers
/// confirmed and the login completes with a code.
#[tokio::test]
async fn the_provider_email_counts_as_confirmed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "adam", "read", |config| {
        config.account_activation_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", &format!("USER-{UNLINKED_UID}@EXAMPLE.ORG")),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the provider-verified email logs in: {}",
        response.text()
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "answered as JSON"
    );
    let url = response.json_value()["redirect_uri"]
        .as_str()
        .expect("a redirect_uri")
        .to_owned();
    let code = query_param(&url, "code").expect("a code");

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        created.email_confirmed.get(),
        "the provider said this very address"
    );
    assert!(
        created.confirmation_token.is_none(),
        "so there is nothing to confirm by letter"
    );
    let stored: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code is stored");
    assert_eq!(stored.user_id, created.id, "for the confirmed account");
    site.provider.stop();
}

/// An out-of-band client gets the OOB code from registration, the
/// very answer the callback gives a linked out-of-band user.
#[tokio::test]
async fn registration_answers_the_out_of_band_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "eve", "read").await;
    let app = oob_app(&pool, "read").await;
    let state = prepare_to(&site.http, &app, OOB, "").await;
    let handed = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={UNLINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    assert_eq!(handed.status, StatusCode::FOUND, "the hand-off");

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "an answer, not a redirect");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the OOB code as JSON"
    );
    assert_no_store(&response);
    let code: String = sqlx::query_scalar("SELECT token FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the code was minted for the out-of-band client");
    assert_eq!(
        response.json_value(),
        serde_json::json!({ "code": code }),
        "carrying the code the client will redeem"
    );
    site.provider.stop();
}
