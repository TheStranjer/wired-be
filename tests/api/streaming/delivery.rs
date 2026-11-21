//! Who an event reaches once it is on the hub the socket watches: the
//! reader's own mute, block, domain and thread-mute filters suppressing
//! authored events, the `Everyone` audience that asks for a token, the
//! `local_only` flag an anonymous socket never sees, and the heartbeat —
//! a live socket pinged, a silent one closed.

use futures_util::StreamExt as _;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as Frame;
use uuid::Uuid;
use wired_be::{
    config::Config,
    models::{
        block, conversation_mute, domain_block,
        mute::{self, MuteOptions},
    },
    streaming::hub::Audience,
};

use super::harness::{
    PING_WAIT, PUBLIC, Server, account, assert_absent, authored_update, event_on,
    handshake_by_hand, local_only_update, open, public_update, read_until, token, watch_frames,
};
use crate::common;

/// A reader never sees an event authored by an account it muted or blocked,
/// posted from a domain it blocked, or sitting in a thread it muted — the
/// per-connection filters, read once when the socket opened.
#[tokio::test]
async fn a_readers_own_filters_suppress_events() {
    let server = Server::start().await;
    let reader = account(server.pool(), "filtering-reader").await;
    let muted = account(server.pool(), "filter-muted").await;
    let blocked = account(server.pool(), "filter-blocked").await;
    let muted_thread = Uuid::now_v7();
    let secret = token(server.pool(), &reader, "read").await;
    let _muted = mute::mute(server.pool(), &reader, &muted, MuteOptions::default())
        .await
        .expect("the mute fixture writes");
    let _blocked = block::block(
        server.pool(),
        &server.state.config.base_url,
        &reader,
        &blocked,
    )
    .await
    .expect("the block fixture writes");
    domain_block::block_domain(server.pool(), reader.id, "spam.example")
        .await
        .expect("the domain block fixture writes");
    conversation_mute::mute(server.pool(), reader.id, muted_thread)
        .await
        .expect("the thread mute fixture writes");
    let mut client = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    server.publish(authored_update(
        "by-muted",
        muted.id,
        "muted",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "by-blocked",
        blocked.id,
        "blocked",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "from-blocked-domain",
        Uuid::now_v7(),
        "noisy@spam.example",
        Uuid::now_v7(),
    ));
    server.publish(authored_update(
        "in-muted-thread",
        Uuid::now_v7(),
        "talkative",
        muted_thread,
    ));
    server.publish(public_update("control"));

    let frames = read_until(&mut client, "control").await;
    assert_absent(
        &frames,
        &[
            "by-muted",
            "by-blocked",
            "from-blocked-domain",
            "in-muted-thread",
        ],
    );
}

/// An `Everyone` event reaches an authenticated socket and no anonymous one; a
/// `Public` event reaches both.
#[tokio::test]
async fn audience_decides_between_an_anonymous_and_an_authenticated_socket() {
    let server = Server::start().await;
    let reader = account(server.pool(), "audience-reader").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut anonymous = open(&server.url(PUBLIC)).await;
    let mut authenticated = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    server.publish(event_on(&["public"], "for-everyone", Audience::Everyone));
    server.publish(public_update("for-anyone"));

    let both = read_until(&mut authenticated, "for-anyone").await;
    assert!(
        both.iter().any(|frame| frame.contains("for-everyone")),
        "an authenticated socket sees an Everyone event: {both:?}"
    );
    let alone = read_until(&mut anonymous, "for-anyone").await;
    assert_absent(&alone, &["for-everyone"]);
}

/// A `local_only` event is for local viewers: an anonymous socket on the same
/// stream skips it however its audience and streams match, and the control
/// event published after it arrives as usual. An authenticated socket sees
/// both.
#[tokio::test]
async fn a_local_only_event_never_reaches_an_anonymous_socket() {
    let server = Server::start().await;
    let reader = account(server.pool(), "local-gate-reader").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut anonymous = open(&server.url(PUBLIC)).await;
    let mut authenticated = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    server.publish(local_only_update(&["public"], "local-for-everyone"));
    server.publish(public_update("local-control"));

    let alone = read_until(&mut anonymous, "local-control").await;
    assert_absent(&alone, &["local-for-everyone"]);
    assert!(
        alone.iter().any(|frame| frame.contains("local-control")),
        "the anonymous socket receives the control event after it: {alone:?}"
    );
    let both = read_until(&mut authenticated, "local-control").await;
    assert!(
        both.iter()
            .any(|frame| frame.contains("local-for-everyone")),
        "an authenticated socket is a local viewer and does see the event: {both:?}"
    );
}

/// Every heartbeat period the server pings the socket, and the client's
/// automatic pong keeps the connection alive: what a proxy needs to see to leave
/// a streaming connection alone.
#[tokio::test]
async fn the_server_pings_a_live_socket() {
    let config = Config {
        streaming_ping_interval_secs: 1,
        ..common::test_config()
    };
    let server = Server::with_config(config).await;
    let mut client = open(&server.url(PUBLIC)).await;

    let frame = timeout(PING_WAIT, client.next())
        .await
        .expect("a ping arrives inside its interval")
        .expect("the socket is healthy")
        .expect("the frame arrives");

    assert!(
        matches!(frame, Frame::Ping(_)),
        "the heartbeat is a WebSocket Ping, not a frame the endpoint invented: {frame:?}"
    );
}

/// A socket that never answers a ping is closed. This test handshakes over a raw
/// TCP connection so that nothing answers for it, and reads frames until the
/// server ends the connection.
#[tokio::test]
async fn a_socket_that_never_answers_pings_is_closed() {
    let config = Config {
        streaming_ping_interval_secs: 1,
        ..common::test_config()
    };
    let server = Server::with_config(config).await;
    let mut raw = handshake_by_hand(&server, PUBLIC).await;

    let saw = timeout(PING_WAIT, watch_frames(&mut raw))
        .await
        .expect("the server ends the silent socket within its two intervals");

    assert!(saw.ping, "the server pinged a socket that never answered");
    assert!(
        saw.close,
        "and said so with a close frame before hanging up: {saw:?}"
    );
}
