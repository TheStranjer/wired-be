//! The retired browser half of the MFA flow: `GET /oauth/mfa` and
//! `POST /oauth/mfa/verify` are not routed. `GET /oauth/mfa` is read as an
//! external-provider entry (`/oauth/{provider}`) and answers its JSON 404
//! "Unknown provider"; the `POST` falls through to the fallback 404. Only
//! `POST /oauth/mfa/challenge` answers (see the `password_grant` tests).

use super::*;

/// `GET /oauth/mfa` answers a JSON 404 and no page, with or without the
/// query the old page took.
#[tokio::test]
async fn the_code_entry_page_is_not_routed() {
    let db = common::test_db().await;
    let http = common::test_app(db.pool().clone());

    for uri in ["/oauth/mfa", "/oauth/mfa?mfa_token=t0ken&state=st"] {
        let response = common::TestRequest::get(uri).send(&http).await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, uri);
        common::assert_error(&response, "Unknown provider", uri);
        assert_ne!(
            response.header("content-type"),
            Some("text/html; charset=utf-8"),
            "no HTML page for {uri}"
        );
    }
}

/// `POST /oauth/mfa/verify` is gone too, and a correct code posted to it
/// delivers nothing: the challenge stays parked and no token is issued.
#[tokio::test]
async fn the_verify_form_is_not_routed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "olga").await;
    let app = common::create_app(&pool, "read").await;
    let http = common::test_app(pool.clone());
    let token = mfa_grant(&http, &app, "olga", "read").await;

    let code = current_code(&fix.secret);
    let response = common::TestRequest::post("/oauth/mfa/verify")
        .form(&[
            ("mfa[mfa_token]", token.as_str()),
            ("mfa[challenge_type]", "totp"),
            ("mfa[code]", code.as_str()),
        ])
        .send(&http)
        .await;

    common::assert_answer(&response, StatusCode::NOT_FOUND, "the verify form");
    assert!(
        response.header("location").is_none(),
        "nothing is delivered"
    );
    assert!(
        stored_challenge(&pool, &token).await.is_some(),
        "the challenge is untouched"
    );
    assert_eq!(
        token_count(&pool, fix.user.id).await,
        0,
        "and no token was issued"
    );
}
