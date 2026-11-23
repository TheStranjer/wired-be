//! Tests of the body and parameter extractors; the flex helpers' cases live
//! in the `flex` child, which shares the fixtures below.

mod flex;

use axum::{
    http::{HeaderValue, StatusCode},
    response::IntoResponse as _,
};
use serde::Deserialize;

use super::*;

/// An all-optional parameter shape using every `flex` coercion.
#[derive(Debug, Deserialize, PartialEq)]
struct Follow {
    /// A flexible boolean.
    #[serde(default, deserialize_with = "super::flex::opt_bool")]
    reblogs: Option<bool>,
    /// A flexible unsigned integer.
    #[serde(default, deserialize_with = "super::flex::opt_u32")]
    count: Option<u32>,
    /// A flexible signed integer.
    #[serde(default, deserialize_with = "super::flex::opt_i64")]
    offset: Option<i64>,
}

/// A parameter shape with one required field.
#[derive(Debug, Deserialize, PartialEq)]
struct Named {
    /// The required name.
    name: String,
}

/// A parameter shape with a required list of strings.
#[derive(Debug, Deserialize, PartialEq)]
struct Ids {
    /// The required ids.
    ids: Vec<String>,
}

/// A parameter shape with a required (non-`Option`) flex field.
#[derive(Debug, Deserialize, PartialEq)]
struct Strict {
    /// A required flex boolean.
    #[serde(deserialize_with = "super::flex::bool")]
    flag: bool,
    /// A required flex unsigned integer.
    #[serde(deserialize_with = "super::flex::u32")]
    n: u32,
}

/// A parameter shape with an optional flex 32-bit integer, the width of
/// a schema `integer` column (a scrobble's `length`).
#[derive(Debug, Deserialize, PartialEq)]
struct Timed {
    /// An optional flex signed 32-bit integer.
    #[serde(default, deserialize_with = "super::flex::opt_i32")]
    length: Option<i32>,
}

/// Builds a POST `uri` with an optional `content-type` and `body`.
fn request(content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::post("/api/v1/endpoint")
        .body(Body::from(body.to_owned()))
        .unwrap();
    if let Some(content_type) = content_type {
        let value = HeaderValue::from_str(content_type).unwrap();
        let _set: Option<HeaderValue> = request.headers_mut().insert(CONTENT_TYPE, value);
    }
    request
}

/// Extracts `ApiBody::<T>` from such a request.
async fn body_of<T: DeserializeOwned>(
    content_type: Option<&str>,
    body: &str,
) -> Result<T, AppError> {
    let ApiBody(parsed) = ApiBody::<T>::from_request(request(content_type, body), &()).await?;
    Ok(parsed)
}

/// Extracts `ApiParams::<T>` from a POST of `uri` (query string and
/// all) with `body`.
async fn params_of<T: DeserializeOwned>(
    uri: &str,
    content_type: Option<&str>,
    body: &str,
) -> Result<T, AppError> {
    let mut builder = Request::post(uri);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    let request = builder.body(Body::from(body.to_owned())).unwrap();
    let ApiParams(parsed) = ApiParams::<T>::from_request(request, &()).await?;
    Ok(parsed)
}

/// The status an [`AppError`] is answered with.
fn status_of(err: AppError) -> StatusCode {
    err.into_response().status()
}
#[tokio::test]
async fn json_bodies_deserialize() {
    let ids: Ids = body_of(Some(JSON_MIME), r#"{"ids":["1","2"]}"#)
        .await
        .unwrap();
    assert_eq!(ids.ids, vec!["1", "2"], "a json array body");

    let named: Named = body_of(
        Some("application/json; charset=utf-8"),
        r#"{"name":"Renée"}"#,
    )
    .await
    .unwrap();
    assert_eq!(named.name, "Renée", "media type parameters are ignored");

    let upper: Named = body_of(Some("Application/JSON"), r#"{"name":"x"}"#)
        .await
        .unwrap();
    assert_eq!(
        upper.name, "x",
        "the media type is matched case-insensitively"
    );
}

#[tokio::test]
async fn form_bodies_deserialize() {
    let named: Named = body_of(Some(FORM_MIME), "name=a+b%20c").await.unwrap();
    assert_eq!(named.name, "a b c", "+ is a space, %XX decodes");

    let ids: Ids = body_of(Some(FORM_MIME), "ids[]=1&ids[]=2").await.unwrap();
    assert_eq!(ids.ids, vec!["1", "2"], "array keys reach T");
}

#[tokio::test]
async fn an_empty_body_stands_for_no_parameters() {
    for content_type in [None, Some(JSON_MIME), Some(FORM_MIME), Some("text/plain")] {
        let follow: Follow = body_of(content_type, "")
            .await
            .unwrap_or_else(|err| panic!("empty body with {content_type:?}: {err}"));
        assert_eq!(
            follow,
            Follow {
                reblogs: None,
                count: None,
                offset: None,
            },
            "all-optional T sees {{}} for content type {content_type:?}"
        );
    }
}

#[tokio::test]
async fn an_empty_body_with_a_required_field_is_unprocessable() {
    let err = body_of::<Named>(None, "")
        .await
        .expect_err("a required field is missing");
    assert_eq!(
        status_of(err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty body 422"
    );
}

#[tokio::test]
async fn malformed_json_is_a_bad_request() {
    let err = body_of::<Named>(Some(JSON_MIME), r#"{"name":"#)
        .await
        .expect_err("truncated JSON must be rejected");
    assert_eq!(
        status_of(err),
        StatusCode::BAD_REQUEST,
        "truncated json gets 400"
    );
}

#[tokio::test]
async fn a_type_mismatch_is_unprocessable() {
    let json_err = body_of::<Named>(Some(JSON_MIME), r#"{"name":1}"#)
        .await
        .expect_err("a number is not a string");
    assert_eq!(
        status_of(json_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "json mismatch 422"
    );

    let missing_err = body_of::<Named>(Some(FORM_MIME), "id=1")
        .await
        .expect_err("the required name was not sent");
    let message = missing_err.to_string();
    assert_eq!(
        status_of(missing_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing field 422"
    );
    assert!(
        message.starts_with(INVALID_PARAMS),
        "the 422 names the problem: {message}"
    );

    let flex_err = body_of::<Follow>(Some(FORM_MIME), "count=abc")
        .await
        .expect_err("not a number");
    assert_eq!(
        status_of(flex_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "flex mismatch 422"
    );
}

#[tokio::test]
async fn an_unsupported_content_type_is_a_415() {
    for content_type in [Some("text/plain"), Some("application/xml"), None] {
        let err = body_of::<Follow>(content_type, "count=5")
            .await
            .expect_err("an unsupported type with a body must be rejected");
        assert_eq!(
            status_of(err),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?} gets 415"
        );
    }
}

#[tokio::test]
async fn malformed_json_reports_the_cause() {
    let err = body_of::<Named>(Some(JSON_MIME), "{oops}")
        .await
        .expect_err("not JSON at all");
    let message = err.to_string();
    assert_eq!(status_of(err), StatusCode::BAD_REQUEST, "bad json is a 400");
    assert!(
        message.starts_with(MALFORMED_JSON),
        "the 400 names the cause: {message}"
    );
}

#[tokio::test]
async fn api_params_reads_the_query_string() {
    let follow: Follow = params_of("/x?reblogs=0&count=5&offset=-2", None, "")
        .await
        .unwrap();
    assert_eq!(
        follow,
        Follow {
            reblogs: Some(false),
            count: Some(5),
            offset: Some(-2),
        },
        "query-only parameters"
    );

    let ids: Ids = params_of("/x?ids[]=3&ids[]=4", None, "").await.unwrap();
    assert_eq!(ids.ids, vec!["3", "4"], "array keys work in the query too");

    let named: Named = params_of("/x?name=a+b", None, "").await.unwrap();
    assert_eq!(named.name, "a b", "the query decodes like a form");
}

#[tokio::test]
async fn api_params_merges_body_over_query() {
    let follow: Follow = params_of("/x?count=5&offset=1", Some(FORM_MIME), "count=9&reblogs=on")
        .await
        .unwrap();
    assert_eq!(
        follow,
        Follow {
            reblogs: Some(true),
            count: Some(9),
            offset: Some(1),
        },
        "the body wins shared keys and adds its own"
    );

    let from_json: Follow = params_of("/x?offset=-1", Some(JSON_MIME), r#"{"count":7}"#)
        .await
        .unwrap();
    assert_eq!(
        from_json,
        Follow {
            reblogs: None,
            count: Some(7),
            offset: Some(-1),
        },
        "a json body merges over the query too"
    );

    let named: Named = params_of("/x?name=query", Some(FORM_MIME), "name=body")
        .await
        .unwrap();
    assert_eq!(named.name, "body", "the shared key comes from the body");
}

#[tokio::test]
async fn api_params_body_rules_apply() {
    let type_err = params_of::<Follow>("/x?count=1", Some("text/plain"), "count=2")
        .await
        .expect_err("a plain-text body is unsupported");
    assert_eq!(
        status_of(type_err),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "415 through ApiParams"
    );

    let query_err = params_of::<Follow>("/x?count=abc", None, "")
        .await
        .expect_err("a bad query value is a mismatch");
    assert_eq!(
        status_of(query_err),
        StatusCode::UNPROCESSABLE_ENTITY,
        "422 through the query"
    );

    let empty: Follow = params_of("/x?count=5", Some(JSON_MIME), "").await.unwrap();
    assert_eq!(
        empty.count,
        Some(5),
        "an empty body keeps the query parameters"
    );
}

/// Echoes the parameters an `ApiParams` request carried.
async fn echo_params(ApiParams(params): ApiParams<Value>) -> axum::Json<Value> {
    axum::Json(params)
}

/// GETs `uri` from a one-route router echoing its `ApiParams`.
async fn get_echo(uri: &str) -> axum::response::Response {
    use tower::ServiceExt as _;

    axum::Router::new()
        .route("/x", axum::routing::get(echo_params))
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_too_deeply_nested_query_key_is_a_400() {
    let uri = format!("/x?a{}=1", "[x]".repeat(5000));
    let response = get_echo(&uri).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "deep query keys are refused, not recursed into"
    );

    let ok = get_echo("/x?authorization[scope][]=read&ids[]=1&ids[]=2").await;
    assert_eq!(ok.status(), StatusCode::OK, "ordinary keys still parse");
}

#[tokio::test]
async fn a_too_deeply_nested_form_body_key_is_a_400() {
    let body = format!("a{}=1", "[]".repeat(100_000));
    let err = body_of::<Value>(Some(FORM_MIME), &body)
        .await
        .expect_err("a 100k-deep key");
    assert_eq!(
        status_of(err),
        StatusCode::BAD_REQUEST,
        "400 through ApiBody"
    );
}
