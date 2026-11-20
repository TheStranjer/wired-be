//! The handshake: `GET /api/v1/streaming`, the [`Upgrade`] extractor that
//! reads it and the credential the request presents.
//!
//! Everything that decides whether a socket may be opened at all is settled
//! here, before the `101`: the token, the initial stream and the subprotocol
//! echo. What the socket does once it is open is [`Connection`]'s and
//! [`run`]'s.
//!
//! [`run`]: super::run::run

use std::{collections::HashMap, convert::Infallible};

use axum::{
    extract::{FromRequestParts, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, header::SEC_WEBSOCKET_PROTOCOL, request::Parts},
    response::Response,
};

use crate::{
    auth,
    error::{AppError, AppResult},
    state::AppState,
};

use super::{
    connection::Connection,
    names::{ACCESS_TOKEN_FIELD, INVALID_TOKEN, NOT_AN_UPGRADE},
    run::run,
};

/// The upgrade request's [`WebSocketUpgrade`], or `None` when the request is
/// not an upgrade at all.
///
/// This is an extractor of its own because axum implements neither
/// `Option<WebSocketUpgrade>` nor a hook for the rejection: the endpoint wants
/// a JSON 400 of its own for a plain `GET` on the path, and axum's rejection
/// is a plain-text one, while decision 8 says every failure of this API is
/// `{"error": …}`.
#[derive(Debug)]
pub(super) struct Upgrade(Option<WebSocketUpgrade>);

impl FromRequestParts<AppState> for Upgrade {
    /// Nothing rejects: a request that cannot be upgraded is the `None` case.
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

/// `GET /api/v1/streaming` — the WebSocket upgrade.
///
/// The credential and the initial subscription are settled *before* the
/// upgrade, so a socket that may not be opened is refused as an ordinary JSON
/// error instead of being opened and then closed: 401
/// `{"error":"Invalid access token"}` for a token that names no live user and
/// for a private stream on an anonymous socket, 400 for a stream name this
/// endpoint does not know, and 400 with [`NOT_AN_UPGRADE`] for a request that
/// is no upgrade at all.
///
/// A `Sec-WebSocket-Protocol` the client offered is echoed back as the chosen
/// subprotocol, which is what the Mastodon web client needs to accept the
/// handshake it smuggled its token into.
pub(super) async fn connect(
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Upgrade(upgrade): Upgrade,
    State(state): State<AppState>,
) -> AppResult<Response> {
    let mut upgrade = upgrade.ok_or_else(|| AppError::BadRequest(NOT_AN_UPGRADE.to_owned()))?;
    if let Some(protocol) = requested_protocol(&headers) {
        upgrade.set_selected_protocol(protocol);
    }
    let credential = match presented_token(&headers, &params) {
        None => None,
        Some(raw) => Some(
            auth::credential(&state, &raw)
                .await?
                .ok_or_else(|| AppError::Unauthorized(INVALID_TOKEN.to_owned()))?,
        ),
    };
    let mut connection = Connection::new(state, credential).await?;
    connection.open_initial(&params).await?;
    // The hub subscription is taken *here*, not in the socket task: the
    // broadcast bus delivers to whoever is subscribed when an event is
    // published, and the upgrade task starts some time after this handler has
    // already answered `101`. Taking the receiver now means an event published
    // the moment the client sees the handshake is buffered for it rather than
    // lost.
    let events = connection.state.hub.subscribe();
    Ok(upgrade.on_upgrade(move |socket| run(socket, connection, events)))
}

/// The first value of the request's `Sec-WebSocket-Protocol` header, split
/// the way `Plug.Conn.Utils.list/1` splits it in Pleroma: on commas, trimmed,
/// empty pieces dropped. This is the value the Mastodon web client puts its
/// bearer token in and the value echoed back as the chosen subprotocol.
fn requested_protocol(headers: &HeaderMap) -> Option<HeaderValue> {
    let offered = headers.get(SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    let first = offered
        .split(',')
        .map(str::trim)
        .find(|protocol| !protocol.is_empty())?;
    HeaderValue::from_str(first).ok()
}

/// The bearer token the request presents: the first `Sec-WebSocket-Protocol`
/// value first, then the `access_token` query parameter — Pleroma's order in
/// `find_access_token` (`lib/pleroma/web/mastodon_api/websocket_handler.ex`),
/// where the subprotocol is the Mastodon web client's carrier and the
/// parameter the fallback for clients that name their subprotocols something
/// else. An empty value on either side presents no token.
fn presented_token(headers: &HeaderMap, params: &HashMap<String, String>) -> Option<String> {
    let from_protocol =
        || requested_protocol(headers).and_then(|value| value.to_str().ok().map(str::to_owned));
    let from_query = || {
        params
            .get(ACCESS_TOKEN_FIELD)
            .filter(|token| !token.is_empty())
            .cloned()
    };
    from_protocol().or_else(from_query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::streaming::test_support::{protocol_headers, query};

    #[test]
    fn the_token_comes_from_the_subprotocol_or_the_query_parameter() {
        let headers = protocol_headers("tok3n, chat");
        assert_eq!(
            presented_token(&headers, &query(&[])).as_deref(),
            Some("tok3n"),
            "the first subprotocol value is the token"
        );
        assert_eq!(
            presented_token(&headers, &query(&[("access_token", "query-token")])).as_deref(),
            Some("tok3n"),
            "and it wins over the parameter, as in Pleroma's find_access_token"
        );
        assert_eq!(
            presented_token(
                &HeaderMap::new(),
                &query(&[("access_token", "query-token")])
            )
            .as_deref(),
            Some("query-token"),
            "with no subprotocol offered the parameter speaks"
        );
        assert_eq!(
            requested_protocol(&headers)
                .and_then(|value| value.to_str().ok().map(str::to_owned))
                .as_deref(),
            Some("tok3n"),
            "and that same value is what the server echoes back"
        );
        assert_eq!(
            presented_token(&HeaderMap::new(), &query(&[("access_token", "")])),
            None,
            "an empty parameter presents no token"
        );
        assert_eq!(
            presented_token(&protocol_headers(" , chat"), &query(&[])).as_deref(),
            Some("chat"),
            "an empty first offer is skipped, the next value is the token"
        );
        assert_eq!(
            requested_protocol(&protocol_headers(" , ")),
            None,
            "an offer of nothing names no protocol"
        );
    }
}
