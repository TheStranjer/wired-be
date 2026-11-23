//! The callback of an MFA-enabled linked account: the provider identity alone
//! is not the second factor, so the browser detours to `/oauth/mfa` with no
//! code, the TOTP verification delivers the parked code to the stored
//! redirect and state, and an account without MFA keeps the direct code.

use axum::http::StatusCode;
use chrono::Utc;
use wired_be::{models::oauth_token, util::totp};

use super::provider::{LINKED_UID, PROVIDER, REDIRECT};
use super::support::{
    assert_no_store, count_codes, fixture, link, location, prepare, query_pairs, query_param,
};
use crate::common;

/// The password the MFA fixture account is created with (never used: the
/// provider identity stands in for it).
const PASSWORD: &str = "correct horse battery staple";

/// A linked account with MFA enabled is not handed a code: the callback
/// answers 302 to `/oauth/mfa` carrying the challenge, the app's client id,
/// scope, redirect URI and state, and no authorization is delivered. The
/// TOTP verification then redirects the code to the stored redirect URI with
/// the client's state.
#[tokio::test]
async fn the_callback_detours_an_mfa_account_and_the_verification_delivers_the_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mallory", "read write").await;
    let (user, secret, _codes) =
        common::create_mfa_user_with_password(&pool, "mfa-gabe", PASSWORD, 1).await;
    link(&pool, &user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "&scope=read&state=st%26ate").await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "a detour redirect");
    assert_no_store(&response);
    let url = location(&response);
    assert!(
        url.starts_with("/oauth/mfa?"),
        "the MFA page, not the app's redirect URI: {url}"
    );
    assert!(
        query_param(&url, "code").is_none(),
        "no code rides the detour: {url}"
    );
    let token = query_param(&url, "mfa_token").expect("the challenge token");
    assert_eq!(
        query_param(&url, "redirect_uri").as_deref(),
        Some(REDIRECT),
        "the stored redirect URI rides along"
    );
    assert_eq!(
        query_param(&url, "state").as_deref(),
        Some("st&ate"),
        "and the client's state"
    );
    assert_eq!(
        query_param(&url, "client_id").as_deref(),
        Some(site.app.client_id.as_str()),
        "and the application"
    );
    assert_eq!(
        query_param(&url, "scope").as_deref(),
        Some("read"),
        "and the scopes"
    );
    let parked: oauth_token::OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations")
            .fetch_one(&pool)
            .await
            .expect("the authorization is parked behind the challenge");
    assert_eq!(parked.user_id, user.id, "for the linked account");
    assert!(!parked.used, "still unspent");

    let real = totp::code_at(&secret, Utc::now().timestamp()).expect("a generated secret decodes");
    let finished = common::TestRequest::post("/oauth/mfa/verify")
        .form(&[
            ("mfa[mfa_token]", token.as_str()),
            ("mfa[challenge_type]", "totp"),
            ("mfa[code]", real.as_str()),
            ("mfa[redirect_uri]", REDIRECT),
            ("mfa[state]", "st&ate"),
        ])
        .send(&site.http)
        .await;
    assert_eq!(
        finished.status,
        StatusCode::FOUND,
        "the second factor passes"
    );
    let delivered = location(&finished);
    let code = query_param(&delivered, "code").expect("the code, now");
    assert_eq!(
        delivered,
        format!("{REDIRECT}?code={code}&state=st%26ate"),
        "the stored redirect URI with the client's state"
    );
    assert_eq!(code, parked.token, "the parked authorization's code");
    site.provider.stop();
}

/// A wrong second factor delivers nothing: the callback's detour is a real
/// gate, not a decoration.
#[tokio::test]
async fn a_wrong_code_after_the_detour_delivers_no_code() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mallet", "read").await;
    let (user, secret, _codes) =
        common::create_mfa_user_with_password(&pool, "mfa-hana", PASSWORD, 1).await;
    link(&pool, &user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "").await;
    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;
    let token = query_param(&location(&response), "mfa_token").expect("the challenge token");

    let real = totp::code_at(&secret, Utc::now().timestamp()).expect("a generated secret decodes");
    let (head, tail) = real.split_at(1);
    let wrong = format!("{}{tail}", if head == "0" { "1" } else { "0" });
    let refused = common::TestRequest::post("/oauth/mfa/verify")
        .form(&[
            ("mfa[mfa_token]", token.as_str()),
            ("mfa[challenge_type]", "totp"),
            ("mfa[code]", wrong.as_str()),
            ("mfa[redirect_uri]", REDIRECT),
        ])
        .send(&site.http)
        .await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "a wrong code is refused"
    );
    assert!(refused.header("location").is_none(), "and goes nowhere");
    site.provider.stop();
}

/// A linked account without MFA is unchanged: the 302 to the app's redirect
/// URI carries the code and the client's state.
#[tokio::test]
async fn the_callback_still_grants_a_code_without_mfa() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "nina", "read").await;
    link(&pool, &site.user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "&state=client-state").await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    assert_eq!(response.status, StatusCode::FOUND, "the app is redirected");
    let url = location(&response);
    let code = query_param(&url, "code").expect("a code on the redirect");
    assert_eq!(
        url,
        format!("{REDIRECT}?code={code}&state=client-state"),
        "straight to the registered URI"
    );
    assert_eq!(
        query_pairs(&url).len(),
        2,
        "the code and the state, nothing else"
    );
    assert_eq!(count_codes(&pool).await, 1, "one code minted");
    site.provider.stop();
}
