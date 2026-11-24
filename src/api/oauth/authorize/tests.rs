//! Unit tests of the authorize endpoint's helpers and its database-free
//! routes. The redirect to the frontend and the code flows that need
//! rows are covered by `tests/api/oauth/`.

use axum::http::{Request, header::CONTENT_TYPE};
use chrono::Utc;
use serde_json::json;
use tower::ServiceExt as _;
use uuid::Uuid;

use super::*;
use crate::{
    config::{Config, FRONTEND_BASE_URL_VAR, test_support::config_with},
    extract::into_params,
};

/// The redirect URI the [`app_row`] fixture registers.
const REGISTERED: &str = "https://client.example/callback";

/// A stored-looking app row: `scopes` registered, `REGISTERED` and
/// the out-of-band URI as redirect URIs.
fn app_row(scopes: &str) -> OauthApp {
    let now = Utc::now();
    OauthApp {
        id: Uuid::now_v7(),
        client_name: "Client".to_owned(),
        redirect_uris: format!("{REGISTERED} {OOB_REDIRECT_URI}"),
        scopes: scopes.to_owned(),
        website: None,
        client_id: "client-id".to_owned(),
        client_secret: "client-secret".to_owned(),
        trusted: false,
        user_id: None,
        inserted_at: now,
        updated_at: now,
    }
}

/// The message an [`AppError`] answers with.
fn message_of(err: &AppError) -> String {
    err.to_string()
}

#[test]
fn the_code_response_type_is_the_only_one_accepted() {
    require_code_response(Some(RESPONSE_TYPE_CODE)).expect("code is the code flow");
    for rejected in [None, Some(""), Some("token"), Some("CODE")] {
        let err = require_code_response(rejected).unwrap_err();
        assert_eq!(
            message_of(&err),
            UNSUPPORTED_RESPONSE_TYPE,
            "`{rejected:?}` names no implemented flow"
        );
    }
}

#[test]
fn redirect_uris_must_match_a_registered_entry_exactly() {
    let app = app_row("read");
    require_registered_redirect(&app, REGISTERED).expect("a space-separated entry");
    require_registered_redirect(&app, OOB_REDIRECT_URI).expect("the out-of-band uri");
    let newlined = OauthApp {
        redirect_uris: format!("{REGISTERED}\n{OOB_REDIRECT_URI}"),
        ..app_row("read")
    };
    require_registered_redirect(&newlined, REGISTERED).expect("newline-separated too");
    for presented in [
        "",
        &format!("{REGISTERED}/extra"),
        "https://client.example",
        &format!("{REGISTERED}?x=1"),
        &format!("{REGISTERED} "),
    ] {
        let err = require_registered_redirect(&app, presented).unwrap_err();
        assert_eq!(
            message_of(&err),
            INVALID_REDIRECT_URI,
            "`{presented:?}` is not the registered spelling"
        );
    }
}

#[test]
fn requested_scopes_default_to_read() {
    for blank in [None, Some(""), Some("   ")] {
        let scopes = requested_scopes(blank);
        assert_eq!(
            scopes.to_string(),
            Scopes::default().to_string(),
            "`{blank:?}` falls back to the default"
        );
    }
    assert_eq!(
        requested_scopes(Some("read+write")).to_string(),
        "read write",
        "an encoded query separator parses"
    );
}

#[test]
fn scopes_must_fit_inside_the_registration() {
    let app = app_row("read write");
    check_scopes_subset(&Scopes::parse("read:accounts"), &app).expect("sub-scope of read");
    check_scopes_subset(&Scopes::parse(""), &app).expect("approving nothing fits");
    let err = check_scopes_subset(&Scopes::parse("read follow"), &app).unwrap_err();
    assert_eq!(
        message_of(&err),
        INVALID_SCOPE,
        "unregistered follow is refused"
    );
}

#[test]
fn query_deserializes_every_wire_field_and_ignores_the_rest() {
    let query: AuthorizeQuery = into_params(json!({
        "response_type": "code",
        "client_id": "client-id",
        "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        "scope": "read write",
        "state": "st",
        "force_login": "true",
        "code_challenge": "abc",
    }))
    .unwrap();
    assert_eq!(
        query.response_type.as_deref(),
        Some(RESPONSE_TYPE_CODE),
        "the flow"
    );
    assert_eq!(query.client_id.as_deref(), Some("client-id"), "the client");
    assert_eq!(query.redirect_uri.as_deref(), Some(OOB_REDIRECT_URI));
    assert_eq!(query.scope.as_deref(), Some("read write"), "the scopes");
    assert_eq!(query.state.as_deref(), Some("st"), "the round trip");
}

#[test]
fn query_params_are_merged_and_replaced() {
    let appended = with_query_params(REGISTERED, &[("code", "abc")]);
    assert_eq!(appended, format!("{REGISTERED}?code=abc"), "a bare uri");

    let merged = with_query_params(
        &format!("{REGISTERED}?foo=1&state=old&code=stale"),
        &[("code", "new"), ("state", "x")],
    );
    assert_eq!(
        merged,
        format!("{REGISTERED}?foo=1&code=new&state=x"),
        "existing pairs are kept, replaced keys take the new value"
    );

    let kept_fragment = with_query_params(&format!("{REGISTERED}?x=1#frag"), &[("code", "abc")]);
    assert_eq!(
        kept_fragment,
        format!("{REGISTERED}?x=1&code=abc#frag"),
        "the fragment stays at the end"
    );

    let encoded = with_query_params(REGISTERED, &[("state", "a b&c=d")]);
    assert_eq!(
        encoded,
        format!("{REGISTERED}?state=a+b%26c%3Dd"),
        "values are urlencoded"
    );

    assert_eq!(
        with_query_params(&format!("{REGISTERED}?"), &[]),
        REGISTERED,
        "an emptied query drops the question mark"
    );
}

#[test]
fn the_frontend_location_keeps_the_query_re_encoded() {
    let config = config_with(&[(FRONTEND_BASE_URL_VAR, "https://fe.example/")]);
    let location = frontend_location(
        &config.frontend_base_url,
        "response_type=code&client_id=abc&redirect_uri=https://client.example/cb\
         &scope=read%20write&state=a+b%26c&code_challenge=xyz",
    )
    .expect("a header-safe location");
    assert_eq!(
        location,
        "https://fe.example/oauth/authorize?response_type=code&client_id=abc\
         &redirect_uri=https%3A%2F%2Fclient.example%2Fcb&scope=read+write\
         &state=a+b%26c&code_challenge=xyz",
        "every pair in order, unknown ones included, urlencoded again"
    );
}

#[test]
fn an_empty_query_adds_no_question_mark() {
    let location = frontend_location("https://fe.example", "").expect("header-safe");
    assert_eq!(
        location, "https://fe.example/oauth/authorize",
        "just the frontend's page"
    );
}

#[test]
fn a_frontend_url_no_header_can_carry_is_an_internal_error() {
    let refused = frontend_location("https://fe.example/\u{0}", "a=b");
    assert!(
        matches!(refused, Err(AppError::Internal(_))),
        "a null byte is no header value: {refused:?}"
    );
}

#[test]
fn a_found_reply_is_a_302_with_a_location() {
    let location = format!("{REGISTERED}?code=abc");
    let reply = AuthorizeReply::Found(HeaderValue::from_str(&location).unwrap());
    let response = reply.into_response();
    assert_eq!(response.status(), StatusCode::FOUND, "302");
    assert_eq!(
        response.headers().get(LOCATION),
        Some(&HeaderValue::from_str(&location).unwrap()),
        "the exact Location"
    );
}

#[tokio::test]
async fn a_code_reply_is_json() {
    let response = AuthorizeReply::Code("abc123".to_owned()).into_response();
    assert_eq!(response.status(), StatusCode::OK, "200");
    assert_eq!(
        response.headers().get(CONTENT_TYPE),
        Some(&HeaderValue::from_static("application/json")),
        "JSON, not a page"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed, json!({ "code": "abc123" }), "the code, alone");
}

#[test]
fn no_store_sets_and_replaces_the_cache_control() {
    let mut response = StatusCode::OK.into_response();
    let absent: Option<HeaderValue> = response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
    assert!(absent.is_none(), "the answer starts without one");
    let response = no_store(response);
    assert_eq!(
        response.headers().get(CACHE_CONTROL),
        Some(&HeaderValue::from_static(NO_STORE)),
        "no-store wins"
    );
}

/// The state the route-shaped tests below need: config only, the
/// lazy pool is never consulted (the validation gates that fail here
/// run before any query).
fn test_state() -> AppState {
    AppState::lazy_for_tests(Config::for_tests())
}

#[tokio::test]
async fn get_answers_at_oauth_authorize_and_refuses_other_flows() {
    let state = test_state();
    let app = router(&state).with_state(state);
    let request = Request::get("/oauth/authorize?response_type=token&client_id=whatever")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "the routed endpoint answers and rejects the flow"
    );
    assert!(
        response.headers().get(LOCATION).is_none(),
        "a refused request is never redirected"
    );
    assert_eq!(
        response.headers().get(CACHE_CONTROL),
        Some(&HeaderValue::from_static(NO_STORE)),
        "and stamps no-store"
    );
}

#[tokio::test]
async fn a_form_post_is_refused_with_a_400() {
    let state = test_state();
    let app = router(&state).with_state(state);
    let request = Request::post(AUTHORIZE_PATH)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(
            "authorization[name]=alice&authorization[password]=pw\
             &authorization[client_id]=c&authorization[redirect_uri]=r",
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "password login inside the authorize endpoint is gone"
    );
    assert!(response.headers().get(LOCATION).is_none(), "no redirect");
    assert_eq!(
        response.headers().get(CACHE_CONTROL),
        Some(&HeaderValue::from_static(NO_STORE)),
        "even the rejection is not cacheable"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["error"], "invalid_request", "the OAuth error code");
}
