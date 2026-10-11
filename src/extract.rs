//! Request-body extraction for the Mastodon client API, following
//! `docs/ARCHITECTURE.md` decision 10: handlers take [`ApiBody<T>`] and
//! never axum's `Json` or `Form` directly.
//!
//! [`ApiBody`] reads and parses the body according to its `content-type`:
//! `application/json` is parsed as JSON, `application/x-www-form-urlencoded`
//! through a PHP/Rails-style key parser, so `ids[]=1&ids[]=2` becomes an
//! array and `a[b]=c` a nested object. A missing or empty body stands for
//! the parameters `{}`. Failures are [`AppError`]s (decision 8): malformed
//! JSON 400, a type mismatch 422, an unsupported content type with a
//! non-empty body 415. Multipart bodies will be handled by their own
//! extractor when the upload tickets land.
//!
//! The extractor consumes the body, so — per Axum's rules — it must be the
//! last argument of a handler.

use std::mem;

use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, header::CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use url::form_urlencoded;

use crate::error::{AppError, AppResult};

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
/// Largest request body any extractor will buffer (16 MB).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The parameters of an endpoint, taken from the request body.
///
/// A handler argument `ApiBody(params): ApiBody<MyParams>`, always the last
/// argument because it consumes the body. See the [module docs](self) for
/// the accepted content types and the error codes.
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

/// Reads the request body (at most [`MAX_BODY_BYTES`]) and parses it into
/// the parameters value: `{}` when empty, otherwise by its media type.
///
/// # Errors
///
/// As [`ApiBody`]'s `from_request`.
async fn body_value(req: Request<Body>) -> AppResult<Value> {
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
fn parse_body(text: &str, content_type: &str) -> AppResult<Value> {
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
fn media_type_of(content_type: &str) -> String {
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
pub(crate) fn into_params<T: DeserializeOwned>(value: Value) -> AppResult<T> {
    T::deserialize(value).map_err(|err| AppError::Unprocessable(format!("{INVALID_PARAMS}{err}")))
}

/// Parses `application/x-www-form-urlencoded` text into a JSON value,
/// giving PHP/Rails-style keys their structure: `a[b]=c` nests an object,
/// `ids[]=1&ids[]=2` appends an array, repeated plain keys become an array,
/// and bracketed indexes stay object keys (`fields_attributes[0][name]`
/// becomes `{"fields_attributes":{"0":{"name":…}}}`). Values are the
/// percent-decoded strings (`+` a space); containers win over conflicting
/// scalars, and the last write wins.
fn parse_form(text: &str) -> Value {
    let mut root = Value::Object(Map::new());
    for (key, value) in form_urlencoded::parse(text.as_bytes()) {
        let path = split_key(&key);
        // A key starting with `[` (or empty) addresses no name; drop it.
        if matches!(path.first(), Some(first) if first.is_empty()) {
            continue;
        }
        assign(&mut root, &path, Value::String(value.into_owned()));
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

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    /// Media types the extractor parses, spelled as clients send them.
    const JSON_MIME_TYPE: &str = "application/json";
    /// The urlencoded media type.
    const FORM_MIME_TYPE: &str = "application/x-www-form-urlencoded";

    /// A small parameters shape the deserialization tests use.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Params {
        /// A required scalar.
        name: String,
        /// An optional list.
        #[serde(default)]
        items: Vec<String>,
    }

    #[tokio::test]
    async fn a_json_body_becomes_the_parameters_object() {
        let request = Request::post("/x")
            .header(CONTENT_TYPE, JSON_MIME_TYPE)
            .body(Body::from(r#"{"name":"C","items":["a"]}"#))
            .unwrap();
        let ApiBody(params) = ApiBody::<Params>::from_request(request, &()).await.unwrap();
        assert_eq!(
            params,
            Params {
                name: "C".to_owned(),
                items: vec!["a".to_owned()],
            },
            "the json body deserializes"
        );
    }

    #[tokio::test]
    async fn an_empty_body_is_the_empty_object_whatever_its_type() {
        for content_type in [JSON_MIME_TYPE, FORM_MIME_TYPE, "text/plain"] {
            let request = Request::post("/x")
                .header(CONTENT_TYPE, content_type)
                .body(Body::empty())
                .unwrap();
            let value = body_value(request).await.unwrap();
            assert_eq!(value, json!({}), "`{content_type}` with no body is {{}}");
        }
    }

    #[tokio::test]
    async fn a_malformed_json_body_is_a_bad_request() {
        let request = Request::post("/x")
            .header(CONTENT_TYPE, JSON_MIME_TYPE)
            .body(Body::from("{oops"))
            .unwrap();
        let err = body_value(request).await.expect_err("broken json fails");
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "malformed json is a 400, got {err}"
        );
        assert!(
            err.to_string().starts_with(MALFORMED_JSON),
            "the message names the cause"
        );
    }

    #[tokio::test]
    async fn an_unsupported_media_type_with_a_body_is_a_415() {
        for content_type in ["text/plain", "application/xml; charset=utf-8", ""] {
            let request = Request::post("/x")
                .header(CONTENT_TYPE, content_type)
                .body(Body::from("name=x"))
                .unwrap();
            let err = body_value(request)
                .await
                .expect_err("an unparsable content type must fail");
            assert!(
                matches!(err, AppError::UnsupportedMediaType(_)),
                "an unparsable type is a 415, got {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_utf8_body_is_a_bad_request() {
        let request = Request::post("/x")
            .header(CONTENT_TYPE, FORM_MIME_TYPE)
            .body(Body::from(&[0xff_u8, 0xfe, 0x00][..]))
            .unwrap();
        let err = body_value(request).await.expect_err("invalid utf-8 fails");
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "undecodable bytes are a 400, got {err}"
        );
    }

    #[test]
    fn form_keys_get_their_php_style_structure() {
        assert_eq!(
            parse_form("name=Mixed+Name&items%5B%5D=a&items%5B%5D=b"),
            json!({"name": "Mixed Name", "items": ["a", "b"]}),
            "`+` decodes and `[]` keys append"
        );
        assert_eq!(
            parse_form("a%5Bb%5D=c&a%5Bd%5D=e"),
            json!({"a": {"b": "c", "d": "e"}}),
            "`a[b]=c` nests an object"
        );
        assert_eq!(
            parse_form("ids=1&ids=2"),
            json!({"ids": ["1", "2"]}),
            "repeated plain keys become an array"
        );
        assert_eq!(
            parse_form("x%5B0%5D%5Bname%5D=n"),
            json!({"x": {"0": {"name": "n"}}}),
            "bracketed indexes stay object keys"
        );
    }

    #[test]
    fn keys_with_nothing_before_a_bracket_are_dropped() {
        assert_eq!(parse_form("[bad]=x&=y&keep=z"), json!({"keep": "z"}));
    }

    #[test]
    fn containers_win_over_scalars_and_the_last_write_wins() {
        assert_eq!(
            parse_form("a=1&a%5B%5D=2"),
            json!({"a": ["1", "2"]}),
            "a scalar grows into an array when a list key follows"
        );
        assert_eq!(
            parse_form("a=%7B%7D&b=c"),
            json!({"a": "{}", "b": "c"}),
            "values stay percent-decoded strings"
        );
    }

    #[test]
    fn media_type_of_strips_parameters_and_case() {
        assert_eq!(
            media_type_of("Application/JSON; charset=UTF-8"),
            JSON_MIME,
            "lowercased and cut at `;`"
        );
        assert_eq!(media_type_of(""), "", "nothing in, nothing out");
    }

    #[test]
    fn a_body_that_does_not_fit_the_shape_is_unprocessable() {
        let err = into_params::<Params>(json!({"items": []})).expect_err("name is required");
        assert!(
            matches!(err, AppError::Unprocessable(_)),
            "a shape mismatch is a 422, got {err}"
        );
        assert!(
            err.to_string().starts_with(INVALID_PARAMS),
            "the message names the parameters"
        );
        let ok = into_params::<Params>(json!({"name": "x"})).unwrap();
        assert_eq!(ok.items, Vec::<String>::new(), "defaults fill the gaps");
    }

    #[tokio::test]
    async fn form_bodies_reach_the_same_params_as_json() {
        let request = Request::post("/x")
            .header(CONTENT_TYPE, FORM_MIME_TYPE)
            .body(Body::from("name=C&items%5B%5D=a&items%5B%5D=b"))
            .unwrap();
        let ApiBody(params) = ApiBody::<Params>::from_request(request, &()).await.unwrap();
        assert_eq!(params.name, "C", "the form name decodes");
        assert_eq!(params.items, vec!["a", "b"], "the list form keys collect");
    }
}
