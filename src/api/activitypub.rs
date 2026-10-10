//! `ActivityPub` routes: the URLs other instances fetch actors and
//! collections at (`docs/ARCHITECTURE.md` decision 3).
//!
//! This router is merged at the top level of [`crate::app`], not under
//! `/api`: the URLs it serves are the actor ids that go out in activities,
//! so they must stay exactly `{BASE_URL}/users/{nickname}`.
//!
//! Today it is the actor document, `GET /users/{nickname}` — Pleroma's
//! `ActivityPubController.user/2`
//! (`lib/pleroma/web/activity_pub/activity_pub_controller.ex`) rendering
//! `UserView.render("user.json", …)`
//! (`lib/pleroma/web/activity_pub/views/user_view.ex`). Inboxes, outboxes
//! and the collections beside them arrive in later tickets and get their
//! handlers here.
//!
//! The route negotiates content: an `Accept` header naming one of the two
//! `ActivityPub` media types gets the actor document, anything else gets
//! Pleroma's 404 `{"error":"Not found"}` for now, because this backend is
//! API-only and has no HTML page to hand an agent back (a later feed ticket
//! redirects HTML clients). Every answer carries `Vary: Accept`, since the
//! header decides which of the two the client sees.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse as _, Response},
    routing::get,
};

use crate::{
    config::Config,
    error::{AppError, AppResult},
    federation::actor::actor_json,
    models::user::{User, get_by_nickname},
    state::AppState,
};

/// `Content-Type` of an actor document: `ActivityPub` is JSON-LD with its
/// own registered media type, and the charset is spelled out because
/// axum's `Json` type would send it without one.
const ACTIVITY_JSON: &str = "application/activity+json; charset=utf-8";
/// The `Vary` value of every response: the `Accept` header picks the
/// document, the 404 or (later) the redirect.
const VARY_ACCEPT: &str = "Accept";
/// The media types whose presence in `Accept` means "this is an
/// `ActivityPub` client", the pair Pleroma's `:activitypub` accepts
/// pipeline lists.
const AP_MEDIA_TYPES: [&str; 2] = ["application/activity+json", "application/ld+json"];

/// The top-level `ActivityPub` router, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/users/{nickname}", get(get_actor))
}

/// `GET /users/{nickname}` — the local actor named `nickname`, as an
/// `ActivityPub` document. No authentication: federation fetches actors
/// anonymously.
///
/// `Vary: Accept` is added to whatever the handler below decides, so the
/// negotiation itself never has to remember the header.
async fn get_actor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(nickname): Path<String>,
) -> Response {
    let response = negotiated_actor(&state, &nickname, &headers).await;
    with_vary(response)
}

/// The response the route sends: the actor document for an `ActivityPub`
/// client that asks for a live local account, and the errors the ticket
/// prescribes for everyone else.
async fn negotiated_actor(
    state: &AppState,
    nickname: &str,
    headers: &HeaderMap,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    let Some(user) = get_by_nickname(&state.db, nickname).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !user.local.get() {
        return Err(AppError::RouteNotFound);
    }
    if !user.is_active.get() {
        return Err(AppError::Gone);
    }
    Ok(actor_response(&user, &state.config))
}

/// Whether `headers` name an `ActivityPub` media type in `Accept`. An
/// absent header is not one: a client that states nothing gets the
/// non-`ActivityPub` answer. The comparison is case-insensitive, as the
/// media type of a header value is.
fn wants_activity_json(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let folded = accept.to_ascii_lowercase();
    AP_MEDIA_TYPES
        .iter()
        .copied()
        .any(|media_type| folded.contains(media_type))
}

/// The actor document for `user` as an `ActivityPub` response.
fn actor_response(user: &User, config: &Config) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
        Json(actor_json(user, config)),
    )
        .into_response()
}

/// The handler's outcome as a response, with the `Vary: Accept` every
/// answer of this route carries: the same URL means a document or a 404
/// depending on the request header, so the errors the negotiation produced
/// get the header too.
fn with_vary(outcome: AppResult<Response>) -> Response {
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

    /// A [`HeaderMap`] holding only `Accept: <accept>` (`None` sends a map
    /// with no `Accept` at all).
    fn headers_with_accept(accept: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            let value = HeaderValue::from_str(accept).unwrap();
            let previous = headers.insert(header::ACCEPT, value);
            drop(previous);
        }
        headers
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
}
