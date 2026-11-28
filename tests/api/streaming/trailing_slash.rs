//! `/api/v1/streaming/` (WBE-898): Soapbox always dials the streaming path
//! with a trailing slash, as Pleroma's Cowboy dispatch allows, so the slash
//! form must answer exactly as `/api/v1/streaming` does — the plain `GET`, the
//! refusals before the upgrade, and the upgrade itself.

use axum::http::StatusCode;
use serde_json::Value;
use wired_be::streaming::hub::Audience;

use super::harness::{
    PUBLIC, Server, account, event_on, next_text, open, open_with_protocol, public_update, refusal,
    token,
};
use crate::common;

/// A plain `GET` without upgrade headers answers the same status and body on
/// both spellings of the path.
#[tokio::test]
async fn a_plain_get_answers_the_same_with_or_without_the_slash() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let bare = common::TestRequest::get("/api/v1/streaming")
        .send(&app)
        .await;
    let slash = common::TestRequest::get("/api/v1/streaming/")
        .send(&app)
        .await;

    assert_eq!(
        slash.status,
        StatusCode::BAD_REQUEST,
        "the slash form is refused as no upgrade: {}",
        slash.text()
    );
    assert_eq!(slash.status, bare.status, "the same status");
    assert_eq!(slash.text(), bare.text(), "the same body");
    assert_eq!(
        slash.json_value().get("error").and_then(Value::as_str),
        Some("Expected a WebSocket upgrade"),
        "naming what the request lacked"
    );
}

/// An invalid token is refused the same way on both spellings.
#[tokio::test]
async fn an_invalid_token_is_refused_the_same_with_or_without_the_slash() {
    let server = Server::start().await;
    let query = format!("{PUBLIC}&access_token=nope");

    let bare = refusal(&server.url(&query)).await;
    let slash = refusal(&server.url_with_slash(&query)).await;

    assert_eq!(slash.0, StatusCode::UNAUTHORIZED, "a bad token never opens");
    assert_eq!(slash, bare, "the same status and body");
}

/// An unknown `stream` value is refused the same way on both spellings.
#[tokio::test]
async fn an_unknown_stream_is_refused_the_same_with_or_without_the_slash() {
    let server = Server::start().await;

    let bare = refusal(&server.url("stream=nonsense")).await;
    let slash = refusal(&server.url_with_slash("stream=nonsense")).await;

    assert_eq!(slash.0, StatusCode::BAD_REQUEST, "an unknown stream is 400");
    assert_eq!(slash, bare, "the same status and body");
}

/// A valid upgrade switches protocols on the slash form and streams the same
/// frames as the bare one.
#[tokio::test]
async fn a_valid_upgrade_streams_the_same_with_or_without_the_slash() {
    let server = Server::start().await;
    let reader = account(server.pool(), "slash-upgrade").await;
    let secret = token(server.pool(), &reader, "read").await;
    let (mut bare, bare_response) = open_with_protocol(&server.url(PUBLIC), &secret).await;
    let (mut slash, slash_response) =
        open_with_protocol(&server.url_with_slash(PUBLIC), &secret).await;

    server.publish(public_update("both"));

    assert_eq!(
        slash_response.status(),
        StatusCode::SWITCHING_PROTOCOLS,
        "the slash form upgrades"
    );
    assert_eq!(
        slash_response.status(),
        bare_response.status(),
        "the same status"
    );
    assert_eq!(
        slash_response.headers().get("sec-websocket-protocol"),
        bare_response.headers().get("sec-websocket-protocol"),
        "the same echoed subprotocol"
    );
    assert_eq!(
        next_text(&mut slash).await,
        next_text(&mut bare).await,
        "the same frame on both sockets"
    );
}

/// Soapbox's own handshake: the `access_token` query parameter on the user
/// stream, with the trailing slash.
#[tokio::test]
async fn the_slash_form_opens_the_user_stream_on_a_query_token() {
    let server = Server::start().await;
    let reader = account(server.pool(), "slash-reader").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut client =
        open(&server.url_with_slash(&format!("stream=user&access_token={secret}"))).await;

    let own_stream = format!("user:{}", reader.id);
    server.publish(event_on(
        &[own_stream.as_str()],
        "via-slash",
        Audience::Users(vec![reader.id]),
    ));

    assert!(
        next_text(&mut client).await.contains("via-slash"),
        "the socket streams for the user that token names"
    );
}
