//! The authorize page's detour through the MFA page.

use super::*;

/// A valid consent login from an MFA-enabled account redirects to the
/// code page instead of issuing the code: the detour parks the challenge
/// and its Location round-trips every value escaped — the 43-char token,
/// the delivery target, the urlencoded state, the client id and the
/// approved scope — while nothing reaches the client yet. The page's own
/// shape and its two verify arms are the next two tests.
#[tokio::test]
async fn the_authorize_flow_detours_through_the_mfa_page() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "gina").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let detour = post_authorize(&http, &authorize_form(&app, "gina", "read", Some("st&ate"))).await;
    assert_eq!(detour.status, StatusCode::FOUND, "the login redirects");
    assert_eq!(
        detour.header("cache-control"),
        Some("no-store"),
        "not even the detour is cacheable"
    );
    let location = detour.header("location").expect("a Location header");
    assert!(
        location.starts_with("/oauth/mfa?"),
        "the redirect aims at the MFA page: {location}"
    );
    let token = query_param(location, "mfa_token").expect("the challenge token rides along");
    assert_eq!(token.len(), 43, "its 43-char shape");
    assert_eq!(
        query_param(location, "redirect_uri").as_deref(),
        Some(REDIRECT),
        "the delivery target round-trips"
    );
    assert_eq!(
        query_param(location, "state").as_deref(),
        Some("st&ate"),
        "the urlencoded state survives the trip"
    );
    assert_eq!(
        query_param(location, "client_id").as_deref(),
        Some(app.client_id.as_str()),
        "so does the client id"
    );
    assert_eq!(
        query_param(location, "scope").as_deref(),
        Some("read"),
        "and the approved scope"
    );
    let (_challenge_id, _authorization_id) =
        assert_challenge_stored(&pool, &token, &fix.user).await;
    assert_eq!(
        token_count(&pool, fix.user.id).await,
        0,
        "no token and no client-visible code yet"
    );
}

/// The parked challenge answers with Pleroma's code page: HTML, not one
/// byte cacheable, its title above a verify form that carries the
/// challenge token and type.
#[tokio::test]
async fn the_mfa_page_renders_the_challenge_into_a_verify_form() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _fix = mfa_account(&pool, "gina").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let detour = post_authorize(&http, &authorize_form(&app, "gina", "read", Some("st&ate"))).await;
    let location = detour.header("location").expect("a Location header");
    let token = query_param(location, "mfa_token").expect("the challenge token rides along");

    let page = common::TestRequest::get(location).send(&http).await;
    assert_eq!(page.status, StatusCode::OK, "the code page renders");
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8"),
        "as HTML"
    );
    assert_eq!(
        page.header("cache-control"),
        Some("no-store"),
        "and not one byte cacheable"
    );
    let html = page.text();
    assert!(
        html.contains("<h1>Two-factor authentication</h1>"),
        "Pleroma's page title"
    );
    assert!(
        html.contains("<form method=\"post\" action=\"/oauth/mfa/verify\">"),
        "the form posts to the verify endpoint"
    );
    assert!(
        html.contains(&format!("name=\"mfa[mfa_token]\" value=\"{token}\">")),
        "the challenge token rides the form"
    );
    assert!(
        html.contains("name=\"mfa[challenge_type]\" value=\"totp\">"),
        "as does the challenge type"
    );
}

/// A wrong code re-renders the form with the flash line and retires
/// nothing; the right one redirects the parked code — unused, the
/// client's to redeem — exactly as a direct approval would.
#[tokio::test]
async fn the_mfa_page_refuses_a_wrong_code_and_delivers_the_right_one() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "gina").await;
    let app = common::create_app(&pool, "read write").await;
    let http = common::test_app(pool.clone());

    let detour = post_authorize(&http, &authorize_form(&app, "gina", "read", Some("st&ate"))).await;
    let location = detour.header("location").expect("a Location header");
    let token = query_param(location, "mfa_token").expect("the challenge token rides along");
    let (_challenge_id, authorization_id) = assert_challenge_stored(&pool, &token, &fix.user).await;

    let real = current_code(&fix.secret);
    let failed = post_verify(
        &http,
        &verify_form(&token, "totp", &wrong_code(&real), REDIRECT, Some("st&ate")),
    )
    .await;
    assert_eq!(failed.status, StatusCode::UNAUTHORIZED, "Pleroma's 401");
    assert_eq!(
        failed.header("content-type"),
        Some("text/html; charset=utf-8"),
        "the form itself"
    );
    let retried = failed.text();
    assert!(
        retried.contains(TWO_FACTOR_FAILED),
        "with the flash line above it"
    );
    assert!(
        stored_challenge(&pool, &token).await.is_some(),
        "a refused verification retires nothing"
    );

    let finished = post_verify(
        &http,
        &verify_form(&token, "totp", &real, REDIRECT, Some("st&ate")),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::FOUND,
        "the right code finishes the flow"
    );
    assert_eq!(
        finished.header("cache-control"),
        Some("no-store"),
        "like every /oauth answer"
    );
    let redirect = finished.header("location").expect("a Location header");
    let code = query_param(redirect, "code").expect("the code parameter");
    assert_eq!(code.len(), 43, "the 43-char Pleroma code shape");
    assert_eq!(
        redirect,
        format!("{REDIRECT}?code={code}&state=st%26ate"),
        "the exact Location, the state round-tripped"
    );
    assert!(
        stored_challenge(&pool, &token).await.is_none(),
        "the passed challenge retires"
    );

    assert_parked_code_redeems(&pool, authorization_id, &code, &app).await;
}

/// The parked authorization behind `authorization_id` is exactly the
/// delivered `code`: unused, with the approved scopes, bound to the
/// [`REDIRECT`] the detour carried, and redeemable by `app` with it.
async fn assert_parked_code_redeems(
    pool: &PgPool,
    authorization_id: Uuid,
    code: &str,
    app: &OauthApp,
) {
    let parked = authorization_by_id(pool, authorization_id).await;
    assert_eq!(parked.token, code, "the delivered code is the parked one");
    assert_eq!(parked.scopes, "read", "with the approved scopes");
    assert!(!parked.used, "and still fresh for the client to redeem");
    assert_eq!(
        parked.redirect_uri.as_deref(),
        Some(REDIRECT),
        "the parked code is bound to the URI the detour carried"
    );
    let redeemed = oauth_token::consume_authorization(pool, code, app.id, Some(REDIRECT))
        .await
        .expect("the exchange reaches the database");
    assert!(redeemed.is_some(), "the code is a real authorization code");
}

/// The verification honours the out-of-band client: no redirect, the
/// code page instead — Pleroma's `oob_authorization_created.html`
/// through the same door the consent flow uses.
#[tokio::test]
async fn the_verification_delivers_the_code_to_an_oob_client() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let fix = mfa_account(&pool, "heidi").await;
    let app = register_app(&pool, &format!("{REDIRECT} {OOB}"), "read").await;
    let http = common::test_app(pool.clone());

    let mut fields = authorize_form(&app, "heidi", "read", None);
    fields[1] = ("authorization[redirect_uri]", OOB);
    let detour = post_authorize(&http, &fields).await;
    let location = detour.header("location").expect("a Location header");
    let token = query_param(location, "mfa_token").expect("the challenge token");
    assert_eq!(
        query_param(location, "redirect_uri").as_deref(),
        Some(OOB),
        "the out-of-band target round-trips"
    );

    let finished = post_verify(
        &http,
        &verify_form(&token, "totp", &current_code(&fix.secret), OOB, None),
    )
    .await;
    assert_eq!(
        finished.status,
        StatusCode::OK,
        "the oob client gets a page"
    );
    assert_eq!(
        finished.header("content-type"),
        Some("text/html; charset=utf-8"),
        "of HTML"
    );
    let body = finished.text();
    assert!(
        body.contains("<h1>Successfully authorized</h1>"),
        "Pleroma's oob page"
    );
    let code = oob_page_code(&body).to_owned();
    assert_eq!(code.len(), 43, "showing the 43-char code");
    let parked: OauthAuthorization =
        sqlx::query_as("SELECT * FROM oauth_authorizations WHERE token = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .expect("the code is the parked authorization");
    assert_eq!(parked.user_id, fix.user.id, "of the logging-in user");
}

/// The page renders every parameter it is handed escaped — a
/// malicious `state` or `mfa_token` cannot open a tag or break out of an
/// attribute — and `challenge_type=recovery` swaps to Pleroma's backup-
/// code form.
#[tokio::test]
async fn the_mfa_page_escapes_and_switches_forms() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let http = common::test_app(pool.clone());

    let evil = common::TestRequest::get(
        "/oauth/mfa?mfa_token=%22%3E%3Cscript%3Ealert%281%29%3C%2Fscript%3E\
         &state=%3Cb%3Est%3C%2Fb%3E&redirect_uri=%22onload%3Dx&client_id=ci%26dc&scope=read",
    )
    .send(&http)
    .await;
    assert_eq!(evil.status, StatusCode::OK, "the page always renders");
    let html = evil.text();
    assert!(
        !html.contains("<script>alert"),
        "the token cannot open a tag"
    );
    assert!(!html.contains("<b>st</b>"), "nor the state: {html}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "it shows escaped inside its attribute"
    );
    assert!(
        html.contains("value=\"ci&amp;dc\""),
        "ampersands escape in the client id too"
    );

    let recovery =
        common::TestRequest::get("/oauth/mfa?mfa_token=t0ken&challenge_type=recovery&state=st")
            .send(&http)
            .await;
    assert_eq!(recovery.status, StatusCode::OK, "the recovery form renders");
    let swapped = recovery.text();
    assert!(
        swapped.contains("<h1>Two-factor recovery</h1>"),
        "Pleroma's recovery title"
    );
    assert!(
        swapped.contains("name=\"mfa[challenge_type]\" value=\"recovery\">"),
        "and the recovery challenge type"
    );
    assert!(
        swapped.contains("challenge_type=totp"),
        "the page links back to the code form"
    );
}
