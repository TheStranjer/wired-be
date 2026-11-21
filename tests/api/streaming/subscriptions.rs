//! Frames that move subscriptions on a live socket: the `subscribe` and
//! `unsubscribe` pair, the list stream as a topic of its own, the pinned
//! refusal frames for an unknown or unauthorized stream, the subscription
//! past [`MAX_SUBSCRIPTIONS`] the cap turns away, and Pleroma's
//! `pleroma:authenticate` — refused on a bad token — that gives an
//! anonymous socket a user without a reconnect.
//!
//! [`MAX_SUBSCRIPTIONS`]: wired_be::api::streaming::MAX_SUBSCRIPTIONS

use wired_be::{api::streaming::MAX_SUBSCRIPTIONS, models::list, streaming::hub::Audience};

use super::harness::{
    PUBLIC, Server, account, assert_absent, event_on, next_text, open, public_update, read_until,
    send, settled, token,
};

/// A `subscribe` frame opens a stream on a live socket and an `unsubscribe`
/// frame closes it again, with the events to prove both.
#[tokio::test]
async fn subscribe_and_unsubscribe_frames_move_a_stream_in_and_out() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"Rust"}"#,
    )
    .await;
    settled(&mut client).await;
    server.publish(event_on(&["hashtag:rust"], "tagged", Audience::Public));
    server.publish(public_update("public-control"));
    let tagged = read_until(&mut client, "public-control").await;
    assert_eq!(
        tagged.first().expect("the hashtag event"),
        r#"{"stream":["hashtag:rust"],"event":"update","payload":"{\"id\":\"tagged\"}"}"#,
        "the hashtag stream the frame opened, delivered — and the tag was normalized"
    );

    send(
        &mut client,
        r#"{"type":"unsubscribe","stream":"hashtag","tag":"rust"}"#,
    )
    .await;
    settled(&mut client).await;
    server.publish(event_on(&["hashtag:rust"], "dropped", Audience::Public));
    server.publish(public_update("public-after"));
    let after = read_until(&mut client, "public-after").await;
    assert_absent(&after, &["dropped"]);
}

/// A list stream is a topic of its own: the socket holding `list:<own id>` is
/// silent about the events of another list.
#[tokio::test]
async fn a_list_stream_delivers_its_own_list_only() {
    let server = Server::start().await;
    let reader = account(server.pool(), "list-reader").await;
    let mine = list::create(
        server.pool(),
        reader.id,
        &list::NewList {
            title: "Reading".to_owned(),
            ..list::NewList::default()
        },
    )
    .await
    .expect("the list fixture writes");
    let secret = token(server.pool(), &reader, "read:lists").await;
    let mut client = open(&server.url(&format!(
        "stream=list&list={}&access_token={secret}",
        mine.id
    )))
    .await;

    server.publish(event_on(&["list:1"], "other-list", Audience::Public));
    let mine_stream = format!("list:{}", mine.id);
    server.publish(event_on(
        &[mine_stream.as_str()],
        "my-list",
        Audience::Public,
    ));

    let frames = read_until(&mut client, "my-list").await;
    assert_absent(&frames, &["other-list"]);
}

/// A stream this endpoint does not know is refused with the ticket's frame, and
/// a private stream on an anonymous socket with the ticket's other one; in both
/// cases the connection stays open and keeps what it had.
#[tokio::test]
async fn a_refused_subscription_answers_the_pinned_frame() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(&mut client, r#"{"type":"subscribe","stream":"nonsense"}"#).await;
    let unknown = next_text(&mut client).await;
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    let unauthorized = next_text(&mut client).await;

    assert_eq!(
        unknown, r#"{"error":"bad_request","error_description":"unknown stream"}"#,
        "an unknown stream is the ticket's bad_request frame"
    );
    assert_eq!(
        unauthorized, r#"{"error":"unauthorized"}"#,
        "a private stream without a user is the ticket's unauthorized frame"
    );

    server.publish(public_update("still-streaming"));
    assert!(
        next_text(&mut client).await.contains("still-streaming"),
        "and the socket is alive with the stream it came in on"
    );
}

/// The subscription past [`MAX_SUBSCRIPTIONS`] is refused and delivers nothing,
/// while the streams the socket already holds keep working — the handshake's own
/// `stream=public` counting against the cap like every other.
#[tokio::test]
async fn the_subscription_past_the_cap_is_refused() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    for index in 0..MAX_SUBSCRIPTIONS - 1 {
        send(
            &mut client,
            &format!(r#"{{"type":"subscribe","stream":"hashtag","tag":"cap{index}"}}"#),
        )
        .await;
    }
    send(
        &mut client,
        r#"{"type":"subscribe","stream":"hashtag","tag":"one-too-many"}"#,
    )
    .await;

    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"bad_request","error_description":"too many subscriptions"}"#,
        "the cap is answered the way an unknown stream is"
    );
    settled(&mut client).await;

    server.publish(event_on(&["hashtag:cap0"], "held", Audience::Public));
    server.publish(event_on(
        &["hashtag:one-too-many"],
        "refused",
        Audience::Public,
    ));
    let frames = read_until(&mut client, "held").await;
    assert_absent(&frames, &["refused"]);
}

/// `pleroma:authenticate` gives an anonymous socket a user without a reconnect,
/// and the private streams open for it from then on.
#[tokio::test]
async fn an_anonymous_socket_authenticates_without_a_reconnect() {
    let server = Server::start().await;
    let reader = account(server.pool(), "late-auth").await;
    let other = account(server.pool(), "late-other").await;
    let secret = token(server.pool(), &reader, "read").await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        &format!(r#"{{"type":"pleroma:authenticate","token":"{secret}"}}"#),
    )
    .await;
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    settled(&mut client).await;
    let own_stream = format!("user:{}", reader.id);
    let other_stream = format!("user:{}", other.id);
    server.publish(event_on(
        &[other_stream.as_str()],
        "for-other",
        Audience::Users(vec![other.id]),
    ));
    server.publish(event_on(
        &[own_stream.as_str()],
        "after-auth",
        Audience::Users(vec![reader.id]),
    ));

    let frames = read_until(&mut client, "after-auth").await;
    assert_absent(&frames, &["for-other"]);
}

/// A `pleroma:authenticate` whose token names nobody is refused, and the socket
/// stays as anonymous as it connected.
#[tokio::test]
async fn authenticating_with_a_bad_token_is_refused() {
    let server = Server::start().await;
    let mut client = open(&server.url(PUBLIC)).await;

    send(
        &mut client,
        r#"{"type":"pleroma:authenticate","token":"nope"}"#,
    )
    .await;

    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"unauthorized"}"#,
        "a bad token authenticates nothing"
    );
    send(&mut client, r#"{"type":"subscribe","stream":"user"}"#).await;
    assert_eq!(
        next_text(&mut client).await,
        r#"{"error":"unauthorized"}"#,
        "and the socket is still anonymous"
    );
}
