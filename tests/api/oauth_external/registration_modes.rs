//! The registration modes around `POST /oauth/register`: closed
//! registrations answered on the form with a line, `approval_required`
//! trading the code for the approval page, `activation_required` for the
//! confirmation page — unless the submitted email is the provider's own,
//! which stands as confirmed — and the out-of-band client reading its
//! code off the OOB page.

use axum::http::StatusCode;
use wired_be::{
    config::Toggle,
    models::{oauth_token, user, user_identity},
};

use super::provider::{OOB, PROVIDER, UNLINKED_UID};
use super::support::{
    assert_no_store, count_codes, count_identities, count_states, count_users, fixture,
    fixture_with, location, oob_app, pending_state, post_form, prepare_to, query_param,
};
use crate::common;

/// With `REGISTRATIONS_OPEN` off, external registrations are blocked the
/// same way the form reports every refusal: the form again with the line
/// "Registrations are closed", no user and no link.
#[tokio::test]
async fn closed_registrations_refuse_the_form_with_a_line() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "xena", "read", |config| {
        config.registrations_open = Toggle::new(false);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the form answers, not a redirect"
    );
    let page = response.text();
    assert!(
        page.contains("Registrations are closed"),
        "with the refusal line: {page}"
    );
    assert_eq!(count_users(&pool).await, 1, "only the fixture user exists");
    assert_eq!(count_identities(&pool).await, 0, "nothing was linked");
    site.provider.stop();
}

/// `REGISTRATIONS_APPROVAL_REQUIRED`: the account is created and the
/// identity linked, but the login waits for staff — an HTML page with the
/// approval message replaces the code.
#[tokio::test]
async fn approval_mode_answers_the_message_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "yuri", "read", |config| {
        config.registrations_approval_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "octocat@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "an HTML page"
    );
    assert_no_store(&response);
    let page = response.text();
    assert!(
        page.contains("once your account is approved"),
        "with the approval message: {page}"
    );
    assert!(!page.contains("action=\"/oauth/register\""), "no form left");
    assert!(
        !page.contains("check your email"),
        "approval is the wait that ends last"
    );

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        created.approval_pending.get(),
        "the account awaits approval"
    );
    assert_eq!(
        count_codes(&pool).await,
        0,
        "an unapproved account gets no code"
    );
    let linked = user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
        .await
        .expect("the identity lookup runs")
        .expect("the identity is linked for the login after approval");
    assert_eq!(linked.id, created.id, "to the awaiting account");
    assert_eq!(count_states(&pool).await, 0, "and its state is spent");
    site.provider.stop();
}

/// `ACCOUNT_ACTIVATION_REQUIRED` with an email the provider does not
/// vouch for: the account lands unconfirmed and the confirmation message
/// page replaces the code.
#[tokio::test]
async fn activation_mode_answers_the_confirmation_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture_with(pool.clone(), "zoe", "read", |config| {
        config.account_activation_required = Toggle::new(true);
    })
    .await;
    let state = pending_state(&site.http, &site.app, "").await;

    let response = post_form(
        &site.http,
        &[
            ("state", state.as_str()),
            ("nickname", "octocat"),
            ("email", "different@example.org"),
        ],
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    let page = response.text();
    assert!(
        page.contains("check your email"),
        "with the confirmation message: {page}"
    );

    let created = user::get_by_nickname(&pool, "octocat")
        .await
        .expect("the lookup runs")
        .expect("the registration created the user");
    assert!(
        !created.email_confirmed.get(),
        "the stranger email is not confirmed"
    );
    assert_eq!(count_codes(&pool).await, 0, "and no code skips the click");
    assert!(
        user_identity::find_user(&pool, PROVIDER, UNLINKED_UID)
            .await
            .expect("the identity lookup runs")
            .is_some(),
        "the identity is linked all the same: the provider account is proven"
    );
    assert_eq!(count_states(&pool).await, 0, "and its state is spent");
    site.provider.stop();
}

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
        StatusCode::FOUND,
        "the provider-verified email logs in: {}",
        response.text()
    );
    let code = query_param(&location(&response), "code").expect("a code");

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

/// An out-of-band client gets the OOB code page from registration, the
/// very answer the callback gives a linked out-of-band user.
#[tokio::test]
async fn registration_answers_the_out_of_band_code_page() {
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
    assert_eq!(response.status, StatusCode::OK, "a page, not a redirect");
    assert_eq!(
        response.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the OOB page"
    );
    assert_no_store(&response);
    let code: String = sqlx::query_scalar("SELECT token FROM oauth_authorizations")
        .fetch_one(&pool)
        .await
        .expect("the code was minted for the out-of-band client");
    assert!(
        response.text().contains(code.as_str()),
        "and shows the code the client will read off it"
    );
    site.provider.stop();
}
