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
    federation::actor::actor_json,
    models::user::User,
};

use super::{ACTIVITY_JSON, AP_MEDIA_TYPES, HTML_MEDIA_TYPE, VARY_ACCEPT};

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

/// The 302 an HTML client gets instead of a document: Pleroma sends
/// browsers to the frontend profile page, `{BASE_URL}/{nickname}` (this
/// instance serves no HTML of its own, decision 1), and lets the frontend
/// decide whether that page exists. A nickname whose text cannot ride a
/// `Location` header names no page either, and answers the 404.
pub(super) fn html_redirect(config: &Config, nickname: &str) -> AppResult<Response> {
    frontend_redirect(config.base_url.as_str(), nickname)
}

/// The 302 to the frontend at `{base_url}/{path}`, shared by the actor
/// route's profile redirect and the replies route's redirect to the post
/// page. Text that cannot ride a `Location` header names no page, and
/// answers the plain 404.
pub(super) fn frontend_redirect(base_url: &str, path: &str) -> AppResult<Response> {
    let location = format!("{base_url}/{path}");
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The 302 a browser gets on `GET /notice/{id}`: the same path on the
/// frontend origin, `{FRONTEND_BASE_URL}/notice/{id}` — the notice lives
/// on the frontend, and this instance serves no HTML of its own
/// (decision 1). `FRONTEND_BASE_URL` defaults to `BASE_URL`, so without a
/// separate frontend this is the redirect to this server's own path, and
/// the frontend decides whether that notice exists. An `{id}` whose text
/// cannot ride a `Location` header names no page either, and answers the
/// 404.
pub(super) fn notice_redirect(config: &Config, id: &str) -> AppResult<Response> {
    let location = format!("{}/notice/{id}", config.frontend_base_url);
    let value = HeaderValue::from_str(&location)
        .ok()
        .ok_or(AppError::RouteNotFound)?;
    Ok((StatusCode::FOUND, [(header::LOCATION, value)]).into_response())
}

/// The actor document for `user` as an `ActivityPub` response.
pub(super) fn actor_response(user: &User, config: &Config) -> Response {
    document_response(&actor_json(user, config))
}

/// A built document's answer, the collection routes' shared ending.
pub(super) fn document(built: AppResult<Value>) -> AppResult<Response> {
    built.map(|document| document_response(&document))
}

/// Any `ActivityPub` document as a response: the JSON body under the
/// `activity+json` content type.
pub(super) fn document_response(document: &Value) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
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

    #[test]
    fn browsers_are_redirected_to_the_frontend_profile_url() {
        let config = Config::for_tests();
        let response = html_redirect(&config, "alice").expect("a plain nickname redirects");
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "the redirect is a 302"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static("https://test.example/alice")),
            "the Location is the frontend profile URL built from the base URL"
        );
    }

    #[test]
    fn a_nickname_no_header_can_carry_is_the_plain_404() {
        let config = Config::for_tests();
        let err = html_redirect(&config, "bad\nnickname")
            .expect_err("no Location header can carry a newline");
        assert_eq!(
            err.to_string(),
            "Not found",
            "and then the route answers like any other unrouted request"
        );
    }

    #[test]
    fn browsers_on_the_notice_route_are_redirected_to_the_frontend_origin() {
        let config = Config::for_tests();
        let response = notice_redirect(&config, "01934568-1111-7000-8000-abcdef012345")
            .expect("a uuid redirects");
        assert_eq!(
            response.status(),
            StatusCode::FOUND,
            "the redirect is a 302"
        );
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&HeaderValue::from_static(
                "https://test.example/notice/01934568-1111-7000-8000-abcdef012345"
            )),
            "the Location is the same path on the frontend origin, which the \
             test config defaults onto the base URL"
        );
    }

    #[test]
    fn a_notice_id_no_header_can_carry_is_the_plain_404() {
        let config = Config::for_tests();
        let err = notice_redirect(&config, "bad\nid")
            .expect_err("no Location header can carry a newline");
        assert_eq!(
            err.to_string(),
            "Not found",
            "and then the route answers like any other unrouted request"
        );
    }
}
