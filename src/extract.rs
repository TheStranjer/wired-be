//! Request-body extraction for the Mastodon client API, following
//! `docs/ARCHITECTURE.md` decision 10: handlers take [`ApiBody<T>`] or
//! [`ApiParams<T>`] and never axum's `Json` or `Form` directly.
//!
//! [`ApiBody`] reads and parses the body according to its `content-type`:
//! `application/json` is parsed as JSON, `application/x-www-form-urlencoded`
//! through a PHP/Rails-style key parser, so `ids[]=1&ids[]=2` becomes an
//! array, `a[b]=c` a nested object and `fields_attributes[0][name]=x` an
//! object keyed `"0"` ([`index_map_to_vec`] turns such an index map into a
//! list when an endpoint needs one). A missing or empty body stands for the
//! parameters `{}`. Because form values arrive as strings, [`flex`] holds
//! serde helpers that also accept the string spellings of booleans and
//! integers. Failures are [`AppError`]s (decision 8): malformed JSON 400, a
//! type mismatch 422, an unsupported content type with a non-empty body 415.
//! Multipart bodies are handled by [`multipart::MultipartForm`], which
//! structures its text fields with the same rules and keeps the file parts
//! beside them.
//!
//! [`ApiParams`] additionally parses the query string with the same key
//! rules and merges it under the body: Mastodon clients put parameters in
//! either place (e.g. `POST /api/v1/accounts/:id/follow?reblogs=false`),
//! and when a key appears in both the body wins.
//!
//! All of these extractors consume the body, so — per Axum's rules — they
//! must be the last argument of a handler.

pub mod multipart;

use std::mem;

use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, header::CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use url::form_urlencoded;

use crate::{error::AppError, middleware::MAX_BODY_BYTES};

/// Media type of a JSON request body.
const JSON_MIME: &str = "application/json";
/// Media type of a urlencoded request body.
const FORM_MIME: &str = "application/x-www-form-urlencoded";
/// Message of the 415 rejection for an unparseable content type.
const UNSUPPORTED_TYPE: &str = "Unsupported media type";
/// Prefix of the 400 rejection for an unparsable JSON body.
const MALFORMED_JSON: &str = "Malformed JSON body: ";
/// Prefix of the 422 rejection for a body that does not fit `T`.
const INVALID_PARAMS: &str = "Invalid parameters: ";

/// The parameters of an endpoint, taken from the request body.
///
/// A handler argument `ApiBody(params): ApiBody<MyParams>`, always the last
/// argument because it consumes the body. See the [module docs] for the
/// accepted content types and the error codes; fields that form requests
/// deliver as strings deserialize through [`flex`].
///
/// [module docs]: self
#[derive(Debug)]
pub struct ApiBody<T>(
    /// The deserialized parameters.
    pub T,
);

/// The request body, parsed per decision 10 and deserialized into `T`.
///
/// # Errors
///
/// [`AppError::BadRequest`] when the body cannot be read, is not UTF-8 or
/// is malformed JSON; [`AppError::UnsupportedMediaType`] for any content
/// type other than JSON and urlencoded with a non-empty body;
/// [`AppError::Unprocessable`] when the parameters do not have the shape
/// `T` asks for.
impl<T, S> FromRequest<S> for ApiBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request<Body>, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(into_params(body_value(req).await?)?))
    }
}

/// The parameters of an endpoint, the query string merged under the body.
///
/// A handler argument `ApiParams(params): ApiParams<MyParams>`, always the
/// last argument because it consumes the body. The query string is parsed
/// with the same PHP/Rails-style key rules as a form body; where a key
/// appears in both the body's value wins. Content types and error codes are
/// those of [`ApiBody`].
#[derive(Debug)]
pub struct ApiParams<T>(
    /// The deserialized parameters.
    pub T,
);

/// The query string and the body merged (body wins), parsed into `T`.
///
/// # Errors
///
/// As [`ApiBody`]'s.
impl<T, S> FromRequest<S> for ApiParams<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request<Body>, _state: &S) -> Result<Self, Self::Rejection> {
        let query = req.uri().query().unwrap_or_default().to_owned();
        let body = body_value(req).await?;
        Ok(Self(into_params(merge_over(&parse_form(&query), body))?))
    }
}

/// Reads the request body (at most [`MAX_BODY_BYTES`]) and parses it into
/// the parameters value: `{}` when empty, otherwise by its media type.
///
/// # Errors
///
/// As [`ApiBody`]'s `from_request`.
pub(crate) async fn body_value(req: Request<Body>) -> Result<Value, AppError> {
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|err| AppError::BadRequest(format!("Body is not valid UTF-8: {err}")))?;
    parse_body(text, &content_type)
}

/// Parses a request body of the given `content-type` into the parameters
/// value; an empty body is `{}` whatever its type claims.
///
/// # Errors
///
/// As [`ApiBody`]'s `from_request`.
fn parse_body(text: &str, content_type: &str) -> Result<Value, AppError> {
    if text.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let media_type = media_type_of(content_type);
    if media_type == FORM_MIME {
        Ok(parse_form(text))
    } else if media_type == JSON_MIME {
        serde_json::from_str(text)
            .map_err(|err| AppError::BadRequest(format!("{MALFORMED_JSON}{err}")))
    } else {
        Err(AppError::UnsupportedMediaType(UNSUPPORTED_TYPE.to_owned()))
    }
}

/// The media type of a `content-type` header value: everything before the
/// first `;`, trimmed and lowercased.
pub(crate) fn media_type_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Deserializes the parameters value into `T`.
///
/// # Errors
///
/// [`AppError::Unprocessable`] naming what does not fit.
pub(crate) fn into_params<T: DeserializeOwned>(value: Value) -> Result<T, AppError> {
    T::deserialize(value).map_err(|err| AppError::Unprocessable(format!("{INVALID_PARAMS}{err}")))
}

/// Puts every key of the `body` over the same key in `query` (the body
/// wins); a body that is not an object replaces the query entirely.
fn merge_over(query: &Value, body: Value) -> Value {
    match (query, &body) {
        (Value::Object(query_map), Value::Object(body_map)) => {
            let mut merged = query_map.clone();
            for (key, value) in body_map {
                let _over_wins: Option<Value> = merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        _ => body,
    }
}

/// Parses `application/x-www-form-urlencoded` text (also used for the query
/// string) into a JSON value, giving PHP/Rails-style keys their structure:
/// `a[b]=c` nests an object, `ids[]=1&ids[]=2` appends an array, repeated
/// plain keys become an array, and bracketed indexes stay object keys
/// (`fields_attributes[0][name]` becomes `{"fields_attributes":{"0":
/// {"name":…}}}`; [`index_map_to_vec`] turns such a map back into a list).
/// Values are the percent-decoded strings (`+` a space); containers win
/// over conflicting scalars, and the last write wins.
fn parse_form(text: &str) -> Value {
    parse_pairs(form_urlencoded::parse(text.as_bytes()))
}

/// Structures already-decoded key/value pairs — the pairs of a urlencoded
/// body, a query string, or the text fields of a multipart body — into one
/// parameters value by the same rules as [`parse_form`].
pub(crate) fn parse_pairs<P, K, V>(pairs: P) -> Value
where
    P: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut root = Value::Object(Map::new());
    for (key, value) in pairs {
        let path = split_key(key.as_ref());
        // A key starting with `[` (or empty) addresses no name; drop it.
        if matches!(path.first(), Some(first) if first.is_empty()) {
            continue;
        }
        assign(&mut root, &path, Value::String(value.as_ref().to_owned()));
    }
    root
}

/// Turns a parsed key like `fields_attributes[0][name]` into its segments
/// (`fields_attributes`, `0`, `name`); `key[]` becomes (`key`, `""`), the
/// empty segment marking an array append.
fn split_key(key: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut rest = key;
    while let Some((head, tail)) = rest.split_once('[') {
        // The text before `[` is a segment; when it follows a previous `]`
        // it still carries that bracket.
        segments.push(head.strip_suffix(']').unwrap_or(head));
        rest = tail;
    }
    segments.push(rest.strip_suffix(']').unwrap_or(rest));
    segments
}

/// Stores `value` in the JSON tree at `node` under the bracketed path
/// `path`, creating the objects and arrays the path asks for. The empty
/// segment appends to `node` itself as an array; a named segment descends
/// into `node` as an object; the end of the path merges the value in (see
/// [`merge_leaf`]).
fn assign(node: &mut Value, path: &[&str], value: Value) {
    let Some((&segment, rest)) = path.split_first() else {
        merge_leaf(node, value);
        return;
    };
    if segment.is_empty() {
        ensure_array(node);
        let Some(array) = node.as_array_mut() else {
            return;
        };
        if rest.is_empty() {
            array.push(value);
        } else {
            array.push(Value::Object(Map::new()));
            if let Some(element) = array.last_mut() {
                assign(element, rest, value);
            }
        }
    } else {
        ensure_object(node);
        let Some(map) = node.as_object_mut() else {
            return;
        };
        let slot = map.entry(segment.to_owned()).or_insert(Value::Null);
        assign(slot, rest, value);
    }
}

/// Makes `node` an array: kept as is, created when empty, an existing lone
/// value wrapped, an object replaced (containers win).
fn ensure_array(node: &mut Value) {
    match node {
        Value::Array(_) => {}
        Value::Null | Value::Object(_) => *node = Value::Array(Vec::new()),
        Value::Bool(_) | Value::String(_) | Value::Number(_) => {
            let lone = mem::replace(node, Value::Null);
            *node = Value::Array(vec![lone]);
        }
    }
}

/// Makes `node` an object, replacing anything else (containers win).
fn ensure_object(node: &mut Value) {
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
}

/// Combines an already-stored value with a new one for the same key: an
/// array grows, an empty slot stays the value, a scalar becomes a
/// two-element array (the repeated plain keys `a=1&a=2`), and an object
/// slot is replaced (last write wins).
fn merge_leaf(existing: &mut Value, value: Value) {
    match existing {
        Value::Array(array) => array.push(value),
        Value::Null | Value::Object(_) => *existing = value,
        Value::Bool(_) | Value::String(_) | Value::Number(_) => {
            let previous = mem::replace(existing, Value::Null);
            *existing = Value::Array(vec![previous, value]);
        }
    }
}

/// Turns a PHP-style index map into the list it encodes.
///
/// The values of an object, ordered by their keys as numbers when every
/// key is numeric — like the `fields_attributes` of a form body; a JSON
/// array unchanged; any other value as a one-element list. Object keys
/// that are not all numeric keep the plain key order [`Value`] objects are
/// stored in.
#[must_use]
pub fn index_map_to_vec(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            let numeric = entries.iter().all(|(key, _)| key.parse::<usize>().is_ok());
            if numeric {
                entries.sort_by_key(|(key, _)| key.parse::<usize>().unwrap_or_default());
            }
            entries.into_iter().map(|(_, item)| item).collect()
        }
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            vec![scalar]
        }
    }
}

/// Serde field helpers for the values forms and query strings deliver as
/// strings.
///
/// Attach them with
/// `#[serde(default, deserialize_with = "wired_be::extract::flex::bool")]`
/// (and the matching `opt_` variant for `Option` fields) so `reblogs=on`,
/// `reblogs=true` and a real JSON `true` all deserialize, and `count=50`
/// and `count="50"` likewise. An absent field needs `default`; `null`
/// deserializes to `None` through the `opt_` variants.
pub mod flex {
    use serde::{Deserialize as _, Deserializer, de::Error as SerdeError};
    use serde_json::Value;

    /// A `bool` that also accepts the form spellings `true`/`false` (any
    /// case), `1`/`0`, `on`/`off`, `yes`/`no` and `t`/`f`.
    ///
    /// # Errors
    ///
    /// A serde error when the value is neither a JSON boolean nor one of
    /// the listed strings.
    pub fn bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        coerce_bool(&value).ok_or_else(|| SerdeError::custom(expected("a boolean", &value)))
    }

    /// [`bool()`] for an `Option<bool>` field; a `null` stays `None`.
    ///
    /// # Errors
    ///
    /// As [`bool()`].
    pub fn opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        value.map_or_else(
            || Ok(None),
            |value| {
                coerce_bool(&value)
                    .map(Some)
                    .ok_or_else(|| SerdeError::custom(expected("a boolean", &value)))
            },
        )
    }

    /// A `u32` that also accepts a numeric string.
    ///
    /// # Errors
    ///
    /// A serde error when the value is not an integer in `u32`'s range,
    /// string or number alike.
    pub fn u32<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        coerce_int::<u32>(&value).ok_or_else(|| SerdeError::custom(expected("a number", &value)))
    }

    /// [`u32()`] for an `Option<u32>` field; a `null` stays `None`.
    ///
    /// # Errors
    ///
    /// As [`u32()`].
    pub fn opt_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        value.map_or(Ok(None), |value| {
            coerce_int::<u32>(&value)
                .map(Some)
                .ok_or_else(|| SerdeError::custom(expected("a number", &value)))
        })
    }

    /// An `i64` that also accepts a numeric string.
    ///
    /// # Errors
    ///
    /// A serde error when the value is not an integer in `i64`'s range,
    /// string or number alike.
    pub fn i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        coerce_int::<i64>(&value).ok_or_else(|| SerdeError::custom(expected("a number", &value)))
    }

    /// [`i64()`] for an `Option<i64>` field; a `null` stays `None`.
    ///
    /// # Errors
    ///
    /// As [`i64()`].
    pub fn opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<Value>::deserialize(deserializer)?;
        value.map_or(Ok(None), |value| {
            coerce_int::<i64>(&value)
                .map(Some)
                .ok_or_else(|| SerdeError::custom(expected("a number", &value)))
        })
    }

    /// The boolean behind a JSON value: real booleans and the form
    /// spellings, case- and whitespace-insensitively; anything else `None`.
    fn coerce_bool(value: &Value) -> Option<bool> {
        match value {
            Value::Bool(found) => Some(*found),
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" | "t" => Some(true),
                "false" | "0" | "off" | "no" | "f" => Some(false),
                _ => None,
            },
            Value::Null | Value::Number(_) | Value::Array(_) | Value::Object(_) => None,
        }
    }

    /// The integer behind a JSON value, from a number or a numeric string,
    /// `None` when neither fits `T`.
    fn coerce_int<T>(value: &Value) -> Option<T>
    where
        T: TryFrom<i64> + TryFrom<u64> + std::str::FromStr,
    {
        match value {
            Value::Number(number) => number
                .as_i64()
                .and_then(|int| T::try_from(int).ok())
                .or_else(|| number.as_u64().and_then(|uint| T::try_from(uint).ok())),
            Value::String(text) => text.trim().parse::<T>().ok(),
            Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
        }
    }

    /// The serde error text naming what was expected and what was found.
    fn expected(what: &str, value: &Value) -> String {
        format!("expected {what}, found {value}")
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        http::{HeaderValue, StatusCode},
        response::IntoResponse as _,
    };
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    /// An all-optional parameter shape using every `flex` coercion.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Follow {
        /// A flexible boolean.
        #[serde(default, deserialize_with = "flex::opt_bool")]
        reblogs: Option<bool>,
        /// A flexible unsigned integer.
        #[serde(default, deserialize_with = "flex::opt_u32")]
        count: Option<u32>,
        /// A flexible signed integer.
        #[serde(default, deserialize_with = "flex::opt_i64")]
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

    /// A parameter shape with required (non-`Option`) flex fields.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Strict {
        /// A required flex boolean.
        #[serde(deserialize_with = "flex::bool")]
        flag: bool,
        /// A required flex unsigned integer.
        #[serde(deserialize_with = "flex::u32")]
        n: u32,
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
    async fn flex_booleans_coerce_from_forms_and_json() {
        for (text, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("on", true),
            ("yes", true),
            ("t", true),
            (" false", false),
            ("0", false),
            ("OFF", false),
            ("no", false),
            ("f", false),
        ] {
            let follow: Follow = body_of(Some(FORM_MIME), &format!("reblogs={text}"))
                .await
                .unwrap_or_else(|err| panic!("reblogs={text}: {err}"));
            assert_eq!(follow.reblogs, Some(want), "form boolean {text:?}");
        }

        let follow: Follow = body_of(Some(JSON_MIME), r#"{"reblogs":false}"#)
            .await
            .unwrap();
        assert_eq!(follow.reblogs, Some(false), "a real json boolean");

        let nulled: Follow = body_of(Some(JSON_MIME), r#"{"reblogs":null}"#)
            .await
            .unwrap();
        assert_eq!(nulled.reblogs, None, "null stays None");

        let bool_err = body_of::<Follow>(Some(FORM_MIME), "reblogs=maybe")
            .await
            .expect_err("maybe is not a boolean");
        assert_eq!(
            status_of(bool_err),
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad bool 422"
        );

        let strict: Strict = body_of(Some(FORM_MIME), "flag=on&n=3").await.unwrap();
        assert_eq!(
            strict,
            Strict { flag: true, n: 3 },
            "required flex fields deserialize from strings"
        );

        let strict_err = body_of::<Strict>(Some(FORM_MIME), "flag=on")
            .await
            .expect_err("a required flex field cannot be absent");
        assert_eq!(
            status_of(strict_err),
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing strict field 422"
        );
    }

    #[tokio::test]
    async fn flex_integers_coerce_from_forms_and_json() {
        for (text, count, offset) in [
            ("count=42&offset=-7", 42, -7),
            ("count=+7&offset=0", 7, 0),
            ("count= 8 &offset= 9", 8, 9),
        ] {
            let follow: Follow = body_of(Some(FORM_MIME), text)
                .await
                .unwrap_or_else(|err| panic!("{text}: {err}"));
            assert_eq!(follow.count, Some(count), "u32 from {text}");
            assert_eq!(follow.offset, Some(offset), "i64 from {text}");
        }

        let follow: Follow = body_of(Some(JSON_MIME), r#"{"count":7,"offset":-2}"#)
            .await
            .unwrap();
        assert_eq!(
            follow,
            Follow {
                reblogs: None,
                count: Some(7),
                offset: Some(-2),
            },
            "real json numbers"
        );

        for text in [
            "count=4294967296",
            "offset=9223372036854775808",
            "count=1.5",
        ] {
            let err = body_of::<Follow>(Some(FORM_MIME), text)
                .await
                .expect_err(&format!("{text} must be rejected"));
            assert_eq!(
                status_of(err),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{text} is out of range or not an integer"
            );
        }
    }

    #[test]
    fn keys_split_at_brackets() {
        assert_eq!(split_key("name"), ["name"], "a plain key");
        assert_eq!(split_key("a[b]"), ["a", "b"], "one level");
        assert_eq!(split_key("ids[]"), ["ids", ""], "an append marker");
        assert_eq!(
            split_key("fields_attributes[0][name]"),
            ["fields_attributes", "0", "name"],
            "index then field"
        );
    }

    #[test]
    fn array_keys_become_json_arrays() {
        assert_eq!(parse_form("ids[]=1&ids[]=2"), json!({"ids": ["1", "2"]}));
        assert_eq!(
            parse_form("tags%5B%5D=a+b&tags[]=c"),
            json!({"tags": ["a b", "c"]}),
            "the encoded and plain marker are the same key"
        );
        assert_eq!(
            parse_form("a[][b]=1&a[][b]=2"),
            json!({"a": [{"b": "1"}, {"b": "2"}]}),
            "an append in the middle opens a new element"
        );
        assert_eq!(
            parse_form("a[b][]=x&a[b][]=y"),
            json!({"a": {"b": ["x", "y"]}}),
            "appends nest inside objects"
        );
    }

    #[test]
    fn nested_keys_become_json_objects() {
        assert_eq!(parse_form("a[b]=c"), json!({"a": {"b": "c"}}));
        assert_eq!(
            parse_form("a[b][c]=d&a[e]=f"),
            json!({"a": {"b": {"c": "d"}, "e": "f"}}),
            "levels coexist"
        );
        assert_eq!(
            parse_form(
                "fields_attributes[0][name]=x&fields_attributes[0][value]=y&fields_attributes[1][name]=z"
            ),
            json!({"fields_attributes": {
                "0": {"name": "x", "value": "y"},
                "1": {"name": "z"},
            }}),
            "numeric keys stay object keys, as required"
        );
    }

    #[test]
    fn repeated_plain_keys_become_arrays_and_decoding_applies() {
        assert_eq!(parse_form("a=1&a=2"), json!({"a": ["1", "2"]}));
        assert_eq!(parse_form("a=1&a=2&a=3"), json!({"a": ["1", "2", "3"]}));
        assert_eq!(
            parse_form("q=hello+world&a[b%28x%29]=1"),
            json!({"q": "hello world", "a": {"b(x)": "1"}}),
            "+ and %XX"
        );
        assert_eq!(
            parse_form("a=1&a[]=2"),
            json!({"a": ["1", "2"]}),
            "a lone value joins the array"
        );
    }

    #[test]
    fn containers_win_and_the_last_write_wins() {
        assert_eq!(
            parse_form("a=1&a[b]=c"),
            json!({"a": {"b": "c"}}),
            "object over scalar"
        );
        assert_eq!(
            parse_form("a[b]=c&a=1"),
            json!({"a": "1"}),
            "scalar over object"
        );
        assert_eq!(parse_form(""), json!({}), "empty text is no parameters");
        assert_eq!(
            parse_form("=1&[a]=2"),
            json!({}),
            "keys naming nothing are dropped"
        );
    }

    #[test]
    fn index_maps_become_lists() {
        let map = parse_form("f[10][v]=ten&f[2][v]=two&f[1][v]=one");
        let fields = map.get("f").expect("parsed map").clone();
        assert_eq!(
            index_map_to_vec(fields),
            vec![
                json!({"v": "one"}),
                json!({"v": "two"}),
                json!({"v": "ten"})
            ],
            "numeric keys order as numbers"
        );
        assert_eq!(
            index_map_to_vec(json!({"b": 2, "a": 1})),
            vec![json!(1), json!(2)],
            "non-numeric keys keep the object's key order"
        );
        assert_eq!(
            index_map_to_vec(json!([1, 2])),
            vec![json!(1), json!(2)],
            "arrays pass through"
        );
        assert_eq!(
            index_map_to_vec(json!("x")),
            vec![json!("x")],
            "a lone value becomes one item"
        );
        assert_eq!(
            index_map_to_vec(json!({})),
            Vec::<Value>::new(),
            "an empty map is empty"
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
        let follow: Follow =
            params_of("/x?count=5&offset=1", Some(FORM_MIME), "count=9&reblogs=on")
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
}
