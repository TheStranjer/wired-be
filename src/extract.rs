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

mod form;
pub mod multipart;
#[cfg(test)]
mod tests;

/// Serde field helpers for the values forms and query strings deliver as
/// strings.
///
/// Attach them with
/// `#[serde(default, deserialize_with = "wired_be::extract::flex::bool")]`
/// (and the matching `opt_` variant for `Option` fields) so `reblogs=on`,
/// `reblogs=true` and a real JSON `true` all deserialize, and `count=50`
/// and `count="50"` likewise. An absent field needs `default`; `null`
/// deserializes to `None` through the `opt_` variants.
pub mod flex;

use axum::{
    body::Body,
    extract::FromRequest,
    http::{Request, header::CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::{error::AppError, middleware::MAX_BODY_BYTES};
use form::parse_form;
pub use form::{index_map_to_vec, parse_pairs};

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
