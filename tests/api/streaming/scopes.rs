//! The scope each private stream requires: `read:notifications` for
//! `user:notification`, `read:statuses` for `user`, `user:pleroma_chat` and
//! `direct`, `read:lists` for `list`, and `read` for all of them. A token for
//! one of those scopes reaches no other stream.

use wired_be::models::list;

use super::harness::{Client, PUBLIC, Server, account, open, read_until, send, token};

/// The frame a refused subscription answers with.
const UNAUTHORIZED: &str = r#"{"error":"unauthorized"}"#;

/// Subscribes a socket authenticated with a `scopes` token to every private
/// stream, one at a time, and returns the streams that were refused.
///
/// An accepted subscription is answered with nothing, so each `subscribe` is
/// followed by one naming no stream: its refusal frame is the barrier, and any
/// frame before it is the answer to the stream under test.
async fn refused_streams(scopes: &str) -> Vec<&'static str> {
    let server = Server::start().await;
    let reader = account(server.pool(), "scope-reader").await;
    let owned = list::create(
        server.pool(),
        reader.id,
        &list::NewList {
            title: "Reading".to_owned(),
            ..list::NewList::default()
        },
    )
    .await
    .expect("the list fixture writes");
    let secret = token(server.pool(), &reader, scopes).await;
    let mut client = open(&server.url(&format!("{PUBLIC}&access_token={secret}"))).await;

    let mut refused = Vec::new();
    for stream in [
        "user",
        "user:notification",
        "user:pleroma_chat",
        "direct",
        "list",
    ] {
        if is_refused(&mut client, stream, &owned.id.to_string()).await {
            refused.push(stream);
        }
    }
    refused
}

/// Subscribes to `stream` (naming the list `list_id`) and reports whether the
/// socket answered it with the unauthorized frame; any other answer fails the
/// test.
async fn is_refused(client: &mut Client, stream: &str, list_id: &str) -> bool {
    let subscribe = serde_json::json!({
        "type": "subscribe",
        "stream": stream,
        "list": list_id,
    });
    send(client, &subscribe.to_string()).await;
    send(
        client,
        r#"{"type":"subscribe","stream":"this-stream-does-not-exist"}"#,
    )
    .await;
    let frames = read_until(client, "unknown stream").await;
    let answers: Vec<&str> = frames.iter().map(String::as_str).collect();
    match answers.as_slice() {
        [_barrier] => false,
        [UNAUTHORIZED, _barrier] => true,
        other => panic!("`{stream}` was answered with {other:?}"),
    }
}

/// A `read:notifications` token streams the notifications and nothing else.
#[tokio::test]
async fn read_notifications_reaches_only_the_notification_stream() {
    assert_eq!(
        refused_streams("read:notifications").await,
        ["user", "user:pleroma_chat", "direct", "list"]
    );
}

/// A `read:statuses` token streams the timeline, chats and direct messages, but
/// neither notifications nor lists.
#[tokio::test]
async fn read_statuses_does_not_reach_notifications_or_lists() {
    assert_eq!(
        refused_streams("read:statuses").await,
        ["user:notification", "list"]
    );
}

/// A `read:lists` token streams a list it owns and no user stream.
#[tokio::test]
async fn read_lists_reaches_only_the_list_stream() {
    assert_eq!(
        refused_streams("read:lists").await,
        ["user", "user:notification", "user:pleroma_chat", "direct"]
    );
}

/// A plain `read` token covers every private stream.
#[tokio::test]
async fn read_reaches_every_private_stream() {
    assert_eq!(refused_streams("read").await, Vec::<&str>::new());
}
