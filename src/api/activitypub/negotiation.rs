//! The router's content negotiation and response endings: which
//! `Accept` header values name an `ActivityPub` client and which a
//! browser, the frontend redirects the browser gets, and the shared
//! document-response helpers — the `Vary: Accept` ending every
//! federation answer leaves through.

use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use serde_json::Value;

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::{
        actor::{actor_json, instance_actor_json, profile_emojis},
        vocab::{ACTIVITY_JSON, AP_MEDIA_TYPES},
    },
    models::{activitypub_actor, user::User},
    state::AppState,
};

use super::{CHARSET_SUFFIX, HTML_MEDIA_TYPE, VARY_ACCEPT};

/// Whether `headers` name an `ActivityPub` media type in `Accept`. An
/// absent header is not one: a client that states nothing gets the
/// non-`ActivityPub` answer. The comparison is case-insensitive, as the
/// media type of a header value is.
pub(super) fn wants_activity_json(headers: &HeaderMap) -> bool {
    let Some(accept) = accept_text(headers) else {
        return false;
    };
    let folded = accept.to_ascii_lowercase();
    AP_MEDIA_TYPES
        .iter()
        .copied()
        .any(|media_type| folded.contains(media_type))
}

/// Whether `headers` name `text/html` in `Accept`: a browser, which the
/// frontend — not this API — serves. Case-insensitive, and an absent
/// header is no browser.
pub(super) fn wants_html(headers: &HeaderMap) -> bool {
    accept_text(headers).is_some_and(|accept| accept.to_ascii_lowercase().contains(HTML_MEDIA_TYPE))
}

/// The readable `Accept` header value: `None` when the header is absent
/// or holds bytes that are not text.
fn accept_text(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
}

/// The 302 an HTML client gets instead of a document:
/// `{FRONTEND_BASE_URL}/{path}` — the actor route sends browsers to the
/// profile page (`path` is the nickname), the replies and notice routes to
/// the post page (`notice/{id}`). This instance serves no HTML of its own
/// (decision 1), so every browser redirect leaves for the frontend origin,
/// which decides whether that page exists. `FRONTEND_BASE_URL` defaults to
/// `BASE_URL`. A `path` whose text cannot ride a `Location` header names no
/// page either, and answers the plain 404.
pub(super) fn frontend_redirect(config: &Config, path: &str) -> AppResult<Response> {
    let location = format!("{}/{path}", config.frontend_base_url);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The actor document for `user` as an `ActivityPub` response, carrying
/// the `Emoji` tags of the custom emojis the profile uses.
///
/// The `inbox` and `publicKey` it names are the account's
/// `activitypub_actors` row's.
///
/// # Errors
///
/// Any database failure of the row or emoji lookup.
pub(super) async fn actor_response(state: &AppState, user: &User) -> AppResult<Response> {
    let actor = activitypub_actor::get(&state.db, user.id).await?;
    let emojis = profile_emojis(&state.db, user).await?;
    Ok(document_response(&actor_json(
        user,
        actor.as_ref(),
        &state.config,
        &emojis,
    )))
}

/// The instance actor document for `user` as an `ActivityPub` response,
/// without `featured` and with the `Emoji` tags its profile text uses.
///
/// # Errors
///
/// Any database failure of the row or emoji lookup.
pub(super) async fn instance_actor_response(state: &AppState, user: &User) -> AppResult<Response> {
    let actor = activitypub_actor::get(&state.db, user.id).await?;
    let emojis = profile_emojis(&state.db, user).await?;
    Ok(document_response(&instance_actor_json(
        user,
        actor.as_ref(),
        &state.config,
        &emojis,
    )))
}

/// A built document's answer, the collection routes' shared ending.
pub(super) fn document(built: AppResult<Value>) -> AppResult<Response> {
    built.map(|document| document_response(&document))
}

/// The `Content-Type` of every `ActivityPub` document response:
/// [`ACTIVITY_JSON`] with the [`CHARSET_SUFFIX`]. Falls back to the bare
/// media type, which cannot happen for these two ASCII constants.
pub(super) fn activity_content_type() -> HeaderValue {
    HeaderValue::from_str(&format!("{ACTIVITY_JSON}{CHARSET_SUFFIX}"))
        .unwrap_or_else(|_| HeaderValue::from_static(ACTIVITY_JSON))
}

/// Any `ActivityPub` document as a response: the JSON body under the
/// `activity+json` content type.
pub(super) fn document_response(document: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, activity_content_type())],
        Json(document),
    )
        .into_response()
}

/// The handler's outcome as a response, with the `Vary: Accept` every
/// answer of this route carries: the same URL means a document or a 404
/// depending on the request header, so the errors the negotiation produced
/// get the header too.
pub(super) fn with_vary(outcome: AppResult<Response>) -> Response {
    let mut response = outcome.unwrap_or_else(AppError::into_response);
    let previous = response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static(VARY_ACCEPT));
    drop(previous);
    response
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::api::activitypub::test_support::headers_with_accept;

    #[test]
    fn the_document_content_type_is_activity_json_with_a_charset() {
        assert_eq!(
            activity_content_type(),
            HeaderValue::from_static("application/activity+json; charset=utf-8"),
            "the media type then the charset"
        );
    }

    #[test]
    fn the_activitypub_media_types_are_recognised() {
        for accept in [
            "application/activity+json",
            "application/ld+json",
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
            "application/json, application/activity+json",
            "text/html, application/ld+json;q=0.9, */*;q=0.8",
            "APPLICATION/Activity+JSON",
            " application/activity+json ",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(
                wants_activity_json(&headers),
                "{accept} names an ActivityPub media type"
            );
        }
    }

    #[test]
    fn any_other_accept_is_not_an_activitypub_client() {
        for accept in [
            "*/*",
            "text/html",
            "application/json",
            "application/activity+xml",
            "application/jsonld",
            "text/html;q=0.9",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(
                !wants_activity_json(&headers),
                "{accept} asks for something else"
            );
        }
    }

    #[test]
    fn an_absent_or_unreadable_accept_is_not_an_activitypub_client() {
        let absent = headers_with_accept(None);
        assert!(
            !wants_activity_json(&absent),
            "a request that states nothing gets the 404"
        );

        // A header value with bytes that are not visible ASCII is legal on
        // the wire but cannot be read as text; that is not an ActivityPub
        // request either.
        let mut unreadable = HeaderMap::new();
        let previous = unreadable.insert(
            header::ACCEPT,
            HeaderValue::from_bytes(b"application/activity+json\xff").unwrap(),
        );
        drop(previous);
        assert!(
            !wants_activity_json(&unreadable),
            "an unreadable Accept is not ActivityPub"
        );
    }

    #[test]
    fn every_answer_varies_on_accept() {
        let response = with_vary(Err(AppError::RouteNotFound));
        assert_eq!(
            response.headers().get(header::VARY),
            Some(&HeaderValue::from_static(VARY_ACCEPT)),
            "the header the route negotiates on is named in Vary"
        );
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "adding the header keeps the error's status"
        );
    }

    #[test]
    fn html_accepts_are_browsers_and_everything_else_is_not() {
        for accept in [
            "text/html",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "TEXT/HTML;q=0.9",
            "application/json, text/html",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(wants_html(&headers), "{accept} names text/html");
        }
        for accept in [
            "*/*",
            "application/json",
            "text/plain",
            "application/xhtml+xml",
            "application/activity+json",
        ] {
            let headers = headers_with_accept(Some(accept));
            assert!(!wants_html(&headers), "{accept} is not a browser");
        }
        assert!(
            !wants_html(&headers_with_accept(None)),
            "a request that states nothing is not a browser"
        );
    }

    /// A test config whose frontend origin differs from its base URL.
    fn separate_frontend_config() -> Config {
        let mut config = Config::for_tests();
        config.frontend_base_url = "https://fe.example".to_owned();
        config
    }

    #[test]
    fn browsers_are_redirected_to_the_frontend_origin() {
        let config = separate_frontend_config();
        let response = frontend_redirect(&config, "alice").expect("a plain nickname redirects");
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "the redirect is a 302"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("https://fe.example/alice")),
            "the Location is built from the frontend origin, not the base URL"
        );
    }

    #[test]
    fn the_notice_path_rides_the_frontend_origin() {
        let config = separate_frontend_config();
        let response = frontend_redirect(&config, "notice/01934568-1111-7000-8000-abcdef012345")
            .expect("a uuid redirects");
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static(
                "https://fe.example/notice/01934568-1111-7000-8000-abcdef012345"
            )),
            "the notice page lives on the frontend origin"
        );
    }

    #[test]
    fn the_frontend_origin_defaults_onto_the_base_url() {
        let config = Config::for_tests();
        let response = frontend_redirect(&config, "alice").expect("a plain nickname redirects");
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("https://test.example/alice")),
            "without a separate frontend the redirect stays on the base URL"
        );
    }

    #[test]
    fn a_path_no_header_can_carry_is_the_plain_404() {
        let config = separate_frontend_config();
        let err = frontend_redirect(&config, "bad\nnickname")
            .expect_err("no Location header can carry a newline");
        assert_eq!(
            err.to_string(),
            "Not found",
            "and then the route answers like any other unrouted request"
        );
    }
}
