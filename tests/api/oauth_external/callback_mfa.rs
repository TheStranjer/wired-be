//! The callback of an MFA-enabled linked account: the provider identity
//! alone is not the second factor, and the server renders no code-entry
//! page to ask for it, so the callback refuses with a 403 and delivers
//! nothing; an account without MFA keeps the direct code.

use axum::http::StatusCode;

use super::provider::{LINKED_UID, PROVIDER, REDIRECT};
use super::support::{
    assert_no_store, count_codes, fixture, link, location, prepare, query_pairs, query_param,
};
use crate::common;

/// The password the MFA fixture account is created with (never used: the
/// provider identity stands in for it).
const PASSWORD: &str = "correct horse battery staple";

/// A linked account with MFA enabled is not handed a code: the callback
/// answers 403, with no redirect, no parked authorization and no
/// challenge row.
#[tokio::test]
async fn the_callback_refuses_an_mfa_account() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let site = fixture(pool.clone(), "mallory", "read write").await;
    let (user, _secret, _codes) =
        common::create_mfa_user_with_password(&pool, "mfa-gabe", PASSWORD, 1).await;
    link(&pool, &user, LINKED_UID).await;
    let state = prepare(&site.http, &site.app, "&scope=read&state=st%26ate").await;

    let response = common::TestRequest::get(&format!(
        "/oauth/{PROVIDER}/callback?code={LINKED_UID}&state={state}"
    ))
    .send(&site.http)
    .await;

    common::assert_answer(&response, StatusCode::FORBIDDEN, "an MFA account");
    assert!(
        response.header("location").is_none(),
        "no redirect, so no detour to a code page"
    );
    assert_eq!(count_codes(&pool).await, 0, "no authorization is parked");
    let challenges: i64 = sqlx::query_scalar("SELECT count(*) FROM mfa_tokens")
        .fetch_one(&pool)
        .await
        .expect("the challenge count runs");
    assert_eq!(challenges, 0, "and no challenge is started");
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
    assert_no_store(&response);
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
